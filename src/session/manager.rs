//! Lightweight runtime state for session-restore: where to save, whether the live
//! state has diverged from disk, and an opaque slot for the in-flight debounced save
//! timer.
//!
//! This is the long-lived companion to the snapshot/save logic — it persists for the
//! lifetime of the compositor, owned by `Naru`. The debounce timer wiring (Phase 2c.4)
//! hangs off the `pending_save_token` field; until then the field stays `None`.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;

use naru_config::SessionRestore;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{LoopHandle, RegistrationToken};

use super::snapshot::build_from_naru;
use super::state::WindowEntry;
use super::storage::{default_state_path, load, save_atomic};

/// Debounce window: delay between the last `mark_dirty` call and the actual save.
///
/// Picked to swallow user-driven bursts (e.g. dragging a window across several
/// columns in a fraction of a second) without making the on-disk state lag noticeably
/// behind reality. One second is the same window IPC clients expect for similar
/// "settled state" reads.
const SAVE_DEBOUNCE: Duration = Duration::from_secs(1);

/// Per-process runtime state for session-restore.
///
/// Constructed only when the feature is enabled (i.e. `config.session_restore.off ==
/// false`). When the user toggles the feature off via config reload, callers should
/// drop the `Option<SessionManager>` rather than leaving a disabled manager around;
/// that way no stale state file is touched.
#[derive(Debug)]
pub struct SessionManager {
    /// Resolved on-disk path for the persisted state. Set once at construction and
    /// never changed during the manager's lifetime — if the user moves the file via a
    /// config reload, we drop and rebuild the manager rather than retargeting in place.
    pub state_path: PathBuf,

    /// Whether the in-memory window state has changed since the last successful save.
    ///
    /// Set by hook-sites at layout mutation points; cleared by the save callback once
    /// the file has been atomically renamed into place.
    pub dirty: bool,

    /// Calloop registration of the pending debounced-save timer, if one is scheduled.
    ///
    /// `None` means no save is queued (either nothing has changed since the last
    /// flush, or the manager is freshly constructed).
    pub pending_save_token: Option<RegistrationToken>,

    /// Saved-window entries from the prior session, awaiting their respawned
    /// counterparts to map. Loaded once at construction; entries are popped via
    /// [`SessionManager::take_pending_for_app`] when a new mapped window's `app_id`
    /// matches the front entry for that app.
    ///
    /// Multi-instance match is FIFO over saved-order — the i-th map of `app_id` X
    /// gets paired with the i-th saved entry of `app_id` X. Documented since Phase 2a;
    /// formalized here.
    pub pending_restore: VecDeque<WindowEntry>,
}

impl SessionManager {
    /// Build a manager from the resolved config, or return `None` if the feature is
    /// off or the state path can't be resolved.
    ///
    /// Path resolution precedence:
    /// 1. `config.state_path` (user-supplied override).
    /// 2. `$XDG_STATE_HOME/naru/session.json`, falling back to
    ///    `~/.local/state/naru/session.json` per the XDG spec.
    pub fn new(config: &SessionRestore) -> Option<Self> {
        if config.off {
            return None;
        }
        let state_path = config
            .state_path
            .as_deref()
            .map(PathBuf::from)
            .or_else(default_state_path)?;

        // Load the prior session into memory once. From here on, `pending_restore` is
        // consumed entry-by-entry as new windows map; entries that never match (e.g.
        // an app the user uninstalled) just sit there until compositor exit.
        let pending_restore = match load(&state_path) {
            Ok(Some(s)) => VecDeque::from(s.windows),
            Ok(None) => VecDeque::new(),
            Err(e) => {
                warn!(
                    "session-restore: load failed at {}: {e:#}; starting with no \
                     pending entries",
                    state_path.display()
                );
                VecDeque::new()
            }
        };

        Some(Self {
            state_path,
            dirty: false,
            pending_save_token: None,
            pending_restore,
        })
    }

    /// Pop the front pending entry whose `app_id` matches.
    ///
    /// "Front" = saved-order — the i-th window of a given app to map after restart
    /// is paired with the i-th saved entry of that app. Returns `None` if no entry
    /// matches; callers should treat that as "this is a new window, not a restore."
    pub fn take_pending_for_app(&mut self, app_id: &str) -> Option<WindowEntry> {
        let pos = self
            .pending_restore
            .iter()
            .position(|e| e.app_id == app_id)?;
        self.pending_restore.remove(pos)
    }

    /// Mark the in-memory state as having diverged from disk and (re)schedule a
    /// debounced save.
    ///
    /// The debounce semantics are "delay from the last call": each `mark_dirty`
    /// cancels any prior pending timer and inserts a fresh one with [`SAVE_DEBOUNCE`].
    /// A burst of layout changes (e.g. ten column-moves in half a second) coalesces
    /// into a single save 1 s after the last mutation rather than ten saves spread
    /// across the burst.
    ///
    /// The fired timer's callback rebuilds the snapshot from the live state and
    /// calls [`save_atomic`]; failures are logged but never propagated, since a
    /// failed save shouldn't crash the compositor.
    pub fn mark_dirty(&mut self, loop_handle: &LoopHandle<'static, crate::naru::State>) {
        self.dirty = true;

        // Cancel any prior pending save so the timer always reflects the most recent
        // dirty mark. Without this, two `mark_dirty`s in quick succession would queue
        // two saves, the first redundant.
        if let Some(token) = self.pending_save_token.take() {
            loop_handle.remove(token);
        }

        let timer = Timer::from_duration(SAVE_DEBOUNCE);
        let token = loop_handle
            .insert_source(timer, |_deadline, _, state| {
                // Build the snapshot first (read-only borrow of naru) before reaching
                // for the manager's mutable bits — Rust's NLL handles the staggered
                // borrows of distinct fields.
                let snapshot = build_from_naru(&state.naru);

                if let Some(sm) = state.naru.session_manager.as_mut() {
                    sm.pending_save_token = None;
                    if sm.take_dirty() {
                        if let Err(e) = save_atomic(&sm.state_path, &snapshot) {
                            warn!("session-restore: save failed: {e:#}");
                        }
                    }
                }

                TimeoutAction::Drop
            })
            .map_err(|e| warn!("session-restore: scheduling save timer failed: {e}"))
            .ok();

        self.pending_save_token = token;
    }

    /// Test-and-clear the dirty flag.
    ///
    /// Used by the save path: when the timer fires, the callback calls this to
    /// decide whether there's anything to write, and if so, performs the save and
    /// the flag stays cleared. If the flag was already false the timer fire is a
    /// no-op (e.g. if a save raced ahead manually).
    pub fn take_dirty(&mut self) -> bool {
        std::mem::replace(&mut self.dirty, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use naru_config::LaunchCommand;

    fn cfg(off: bool, state_path: Option<&str>) -> SessionRestore {
        SessionRestore {
            off,
            state_path: state_path.map(String::from),
            launch_commands: vec![LaunchCommand {
                app_id: "x".into(),
                command: vec!["x".into()],
                cwd_from_child: false,
            }],
        }
    }

    #[test]
    fn off_returns_none() {
        let c = cfg(true, None);
        assert!(SessionManager::new(&c).is_none());
    }

    #[test]
    fn explicit_state_path_wins() {
        let c = cfg(false, Some("/tmp/naru-session-test.json"));
        let m = SessionManager::new(&c).expect("manager");
        assert_eq!(m.state_path, PathBuf::from("/tmp/naru-session-test.json"));
        assert!(!m.dirty);
        assert!(m.pending_save_token.is_none());
    }

    #[test]
    fn default_path_falls_back_to_xdg() {
        // We can't assert the exact path in a portable way (depends on $XDG_STATE_HOME
        // and $HOME at test time), but it must end with naru/session.json.
        let c = cfg(false, None);
        let m = SessionManager::new(&c).expect("manager");
        let suffix: PathBuf = ["naru", "session.json"].iter().collect();
        assert!(
            m.state_path.ends_with(&suffix),
            "state_path {} should end with naru/session.json",
            m.state_path.display(),
        );
    }

    #[test]
    fn take_dirty_clears_flag() {
        let c = cfg(false, Some("/tmp/x.json"));
        let mut m = SessionManager::new(&c).expect("manager");
        assert!(!m.take_dirty());
        // Bypass mark_dirty (which requires a real calloop LoopHandle) and exercise
        // the flag round-trip directly. The mark_dirty path is exercised at runtime
        // through the hook sites in handlers/compositor.rs and handlers/xdg_shell.rs.
        m.dirty = true;
        assert!(m.take_dirty());
        assert!(!m.take_dirty());
    }
}

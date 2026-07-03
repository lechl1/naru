//! Lightweight runtime state for session-restore: where to save, whether the live
//! state has diverged from disk, and an opaque slot for the in-flight debounced save
//! timer.
//!
//! This is the long-lived companion to the snapshot/save logic — it persists for the
//! lifetime of the compositor, owned by `Naru`. The debounce timer wiring (Phase 2c.4)
//! hangs off the `pending_save_token` field; until then the field stays `None`.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::Duration;

use naru_config::SessionRestore;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{LoopHandle, RegistrationToken};

use super::snapshot::build_from_naru;
use super::state::{ActiveWorkspace, SessionState, WindowEntry};
use super::storage::{default_state_path, load, save_atomic};

/// Debounce window: delay between the last `mark_dirty` call and the actual save.
///
/// Picked to swallow user-driven bursts (e.g. dragging a window across several
/// columns in a fraction of a second) without making the on-disk state lag noticeably
/// behind reality. One second is the same window IPC clients expect for similar
/// "settled state" reads.
const SAVE_DEBOUNCE: Duration = Duration::from_secs(1);

/// Interval for the unconditional periodic save.
///
/// The debounced save only fires on layout *mutations*; a terminal's working
/// directory changes when the user `cd`s, which is invisible to the layout and so
/// never marks the state dirty. Without a periodic re-snapshot, that updated cwd
/// would only reach disk on a clean shutdown — lost entirely on a crash or power
/// loss. This timer rebuilds the snapshot every minute (re-reading `cwd-from-child`
/// apps' live cwd) and writes only when the result differs from the last save, so an
/// idle session does no disk I/O.
const PERIODIC_SAVE: Duration = Duration::from_secs(60);

/// Grace period after startup before session-restore mode ends unconditionally.
///
/// During restore, workspaces materialized at saved indices are kept as empty
/// placeholders so out-of-order window maps still land on the right workspace
/// (see [`crate::layout::Monitor::restoring`]). The compositor normally ends
/// restore mode the instant the pending-restore queue drains, but a saved
/// window whose app was uninstalled or crashed on relaunch never maps — so this
/// bounded backstop lifts the protection and reclaims unused workspaces even
/// then. Generous, because some apps (browsers restoring many tabs) map slowly.
const RESTORE_SETTLE: Duration = Duration::from_secs(60);

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
    /// [`SessionManager::take_pending_for`] when a new mapped window matches a saved
    /// entry by `app_id` (and cwd when available).
    ///
    /// Multi-instance match is FIFO over saved-order — the i-th map of `app_id` X
    /// gets paired with the i-th saved entry of `app_id` X. Documented since Phase 2a;
    /// formalized here.
    pub pending_restore: VecDeque<WindowEntry>,

    /// The workspace that was active at save time, awaiting restore-settle to be
    /// re-focused. Taken once via [`SessionManager::take_pending_active_workspace`];
    /// `None` after that, or when the prior session recorded no active workspace.
    pub pending_active_workspace: Option<ActiveWorkspace>,

    /// The snapshot most recently written to disk, used to skip redundant writes.
    ///
    /// The periodic timer rebuilds the snapshot every [`PERIODIC_SAVE`]; comparing
    /// against this lets an idle (or layout-stable, cwd-stable) session avoid an
    /// fsync every minute. `None` until the first successful save.
    last_saved: Option<SessionState>,
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
        // an app the user uninstalled) just sit there until compositor exit. The
        // saved active workspace is stashed too, applied once restore settles.
        let (pending_restore, pending_active_workspace) = match load(&state_path) {
            Ok(Some(s)) => (VecDeque::from(s.windows), s.active_workspace),
            Ok(None) => (VecDeque::new(), None),
            Err(e) => {
                warn!(
                    "session-restore: load failed at {}: {e:#}; starting with no \
                     pending entries",
                    state_path.display()
                );
                (VecDeque::new(), None)
            }
        };

        Some(Self {
            state_path,
            dirty: false,
            pending_save_token: None,
            pending_restore,
            pending_active_workspace,
            last_saved: None,
        })
    }

    /// Take the saved active workspace, if any, leaving `None` behind so it is applied
    /// at most once. Called when restore settles (all windows placed, or the settle
    /// timeout) to return focus to the workspace the user left active.
    pub fn take_pending_active_workspace(&mut self) -> Option<ActiveWorkspace> {
        self.pending_active_workspace.take()
    }

    /// Register the recurring [`PERIODIC_SAVE`] timer on the event loop.
    ///
    /// Call once, after the manager is installed on `Naru`. The timer reschedules
    /// itself on every fire and fetches the live manager from `State` each time, so it
    /// keeps working across config reloads as long as a manager is present. When
    /// session-restore is disabled there is no manager and this is never called.
    pub fn schedule_periodic_saves(loop_handle: &LoopHandle<'static, crate::naru::State>) {
        let timer = Timer::from_duration(PERIODIC_SAVE);
        let res = loop_handle.insert_source(timer, |_deadline, _, state| {
            // Build the snapshot under a read-only borrow before reaching for the
            // manager's mutable bits, mirroring the debounced-save callback.
            let snapshot = build_from_naru(&state.naru);
            if let Some(sm) = state.naru.session_manager.as_mut() {
                sm.persist_if_changed(snapshot);
            }
            TimeoutAction::ToDuration(PERIODIC_SAVE)
        });
        if let Err(e) = res {
            warn!("session-restore: scheduling periodic save timer failed: {e}");
        }
    }

    /// Schedule a one-shot timer that ends session-restore mode after
    /// [`RESTORE_SETTLE`], as a backstop for placeholder-workspace protection.
    ///
    /// Call once at startup when there are pending entries to restore. The
    /// compositor ends restore mode early when the last pending window maps; this
    /// guarantees it also ends if some saved windows never reappear. Fires once,
    /// then drops itself.
    pub fn schedule_restore_settle(loop_handle: &LoopHandle<'static, crate::naru::State>) {
        let timer = Timer::from_duration(RESTORE_SETTLE);
        let res = loop_handle.insert_source(timer, |_deadline, _, state| {
            state.naru.layout.end_session_restore();
            // Backstop the active-workspace restore too, in case some saved windows
            // never reappeared and the map-order path above never drained. No-op if
            // that path already consumed the pending value.
            state.naru.restore_active_workspace();
            TimeoutAction::Drop
        });
        if let Err(e) = res {
            warn!("session-restore: scheduling restore-settle timer failed: {e}");
        }
    }

    /// Write `snapshot` iff it differs from the last successfully saved state.
    ///
    /// Updates the [`Self::last_saved`] change-detection cache and clears the dirty
    /// flag on a successful write. Returns whether a write actually happened. Save
    /// errors are logged, not propagated — a failed save must never crash the
    /// compositor.
    fn persist_if_changed(&mut self, snapshot: SessionState) -> bool {
        if self.last_saved.as_ref() == Some(&snapshot) {
            return false;
        }
        match save_atomic(&self.state_path, &snapshot) {
            Ok(()) => {
                self.last_saved = Some(snapshot);
                self.dirty = false;
                true
            }
            Err(e) => {
                warn!("session-restore: save failed: {e:#}");
                false
            }
        }
    }

    /// Pop the pending entry for a newly-mapped window, matching on `app_id` and, when
    /// they help, `cwd` and `title`.
    ///
    /// A restored process is spawned in its saved directory (see
    /// [`crate::session::restore`]), so a multi-instance app like a terminal carries a
    /// distinct cwd per window. GUI apps (browsers, PWAs) instead share one process cwd,
    /// so the window `title` is what tells their windows apart. Match precedence, most to
    /// least specific:
    ///
    /// 1. `(app_id, cwd, title)` — pins a window exactly even when neither cwd nor title
    ///    alone is unique.
    /// 2. `(app_id, cwd)` — the terminal case (distinct cwds).
    /// 3. `(app_id, title)` — the browser case (shared cwd, distinct titles).
    /// 4. `(app_id)` FIFO — last resort; the i-th same-app window to map gets the i-th
    ///    saved entry.
    ///
    /// A `title` is only *used* when it actually distinguishes: it's matched by exact
    /// equality, and an empty/absent title never matches. Titles settle asynchronously as
    /// an app loads, so a caller that needs the settled title should hold the window and
    /// re-match once it's available rather than trusting the map-time value. Returns
    /// `None` if no entry shares the `app_id`.
    pub fn take_pending_for(
        &mut self,
        app_id: &str,
        cwd: Option<&Path>,
        title: Option<&str>,
    ) -> Option<WindowEntry> {
        let title = title.filter(|t| !t.is_empty());

        // Try each key in precedence order; the first that finds an entry wins.
        let find = |pred: &dyn Fn(&WindowEntry) -> bool| self.pending_restore.iter().position(pred);
        let pos = None
            .or_else(|| match (cwd, title) {
                (Some(cwd), Some(title)) => find(&|e| {
                    e.app_id == app_id
                        && e.cwd.as_deref() == Some(cwd)
                        && e.title.as_deref() == Some(title)
                }),
                _ => None,
            })
            .or_else(|| match cwd {
                Some(cwd) => find(&|e| e.app_id == app_id && e.cwd.as_deref() == Some(cwd)),
                None => None,
            })
            .or_else(|| match title {
                Some(title) => {
                    find(&|e| e.app_id == app_id && e.title.as_deref() == Some(title))
                }
                None => None,
            })
            .or_else(|| find(&|e| e.app_id == app_id))?;
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
                        sm.persist_if_changed(snapshot);
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
            }],
            cwd_from_child: Vec::new(),
        }
    }

    #[test]
    fn off_returns_none() {
        let c = cfg(true, None);
        assert!(SessionManager::new(&c).is_none());
    }

    #[test]
    fn take_pending_for_distinguishes_same_app_by_title() {
        use crate::session::state::{Placement, WorkspaceRef};

        let path = std::env::temp_dir()
            .join(format!("naru-title-test-{}.json", std::process::id()));
        let c = cfg(false, path.to_str());
        let mut m = SessionManager::new(&c).expect("manager");

        let entry = |title: &str, col: usize| WindowEntry {
            app_id: "librewolf".into(),
            title: Some(title.into()),
            cwd: Some(PathBuf::from("/home")),
            flatpak_id: None,
            exec: None,
            command: None,
            tmux_session: None,
            claude_session: None,
            output: None,
            workspace: WorkspaceRef::Index { index: 0 },
            placement: Placement::Tiled {
                column_index: col,
                tile_index: 0,
                width: 0.0,
                height: 0.0,
                is_fullscreen: false,
                is_maximized: false,
            },
        };
        // Three same-app, same-cwd windows distinguishable only by title.
        m.pending_restore = [entry("GitHub", 0), entry("Docs", 1), entry("Mail", 2)].into();

        let cwd = Some(Path::new("/home"));
        // A window whose title matches "Docs" pulls the Docs entry, not FIFO.
        let got = m.take_pending_for("librewolf", cwd, Some("Docs")).unwrap();
        assert_eq!(got.title.as_deref(), Some("Docs"));
        // Title still settling (None) → falls back to (app_id, cwd) FIFO → GitHub.
        let got = m.take_pending_for("librewolf", cwd, None).unwrap();
        assert_eq!(got.title.as_deref(), Some("GitHub"));
        // A non-matching title falls back too, leaving the last one → Mail.
        let got = m.take_pending_for("librewolf", cwd, Some("Nope")).unwrap();
        assert_eq!(got.title.as_deref(), Some("Mail"));
        assert!(m.take_pending_for("librewolf", None, None).is_none());

        let _ = std::fs::remove_file(&path);
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
    fn persist_if_changed_skips_redundant_writes() {
        use crate::session::state::{Placement, SessionState, WorkspaceRef, SCHEMA_VERSION};

        let path = std::env::temp_dir().join(format!(
            "naru-persist-test-{}.json",
            std::process::id()
        ));
        let c = cfg(false, path.to_str());
        let mut m = SessionManager::new(&c).expect("manager");

        let empty = SessionState {
            version: SCHEMA_VERSION,
            windows: vec![],
            active_workspace: None,
        };
        // First write goes through; an identical follow-up is skipped.
        assert!(m.persist_if_changed(empty.clone()), "first write happens");
        assert!(
            !m.persist_if_changed(empty.clone()),
            "identical snapshot must not rewrite"
        );

        // A materially different snapshot (one window) writes again.
        let with_window = SessionState {
            version: SCHEMA_VERSION,
            windows: vec![WindowEntry {
                app_id: "org.kde.konsole".into(),
                title: None,
                cwd: Some(PathBuf::from("/home/leo/work")),
                flatpak_id: None,
                exec: None,
                command: None,
                tmux_session: None,
                claude_session: None,
                output: None,
                workspace: WorkspaceRef::Index { index: 0 },
                placement: Placement::Tiled {
                    column_index: 0,
                    tile_index: 0,
                    width: 0.0,
                    height: 0.0,
                    is_fullscreen: false,
                    is_maximized: false,
                },
            }],
            active_workspace: None,
        };
        assert!(
            m.persist_if_changed(with_window.clone()),
            "changed snapshot writes"
        );
        assert!(
            !m.persist_if_changed(with_window),
            "same changed snapshot is then skipped"
        );

        let _ = std::fs::remove_file(&path);
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

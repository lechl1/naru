//! Lightweight runtime state for session-restore: where to save, whether the live
//! state has diverged from disk, and an opaque slot for the in-flight debounced save
//! timer.
//!
//! This is the long-lived companion to the snapshot/save logic — it persists for the
//! lifetime of the compositor, owned by `Naru`. The debounce timer wiring (Phase 2c.4)
//! hangs off the `pending_save_token` field; until then the field stays `None`.

use std::path::PathBuf;

use naru_config::SessionRestore;
use smithay::reexports::calloop::RegistrationToken;

use super::storage::default_state_path;

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
    /// flush, or the manager is freshly constructed). Phase 2c.4 will populate this
    /// from `mark_dirty(&LoopHandle)`; for now the field exists so the type signature
    /// of `SessionManager` is stable across the remaining sub-phases.
    pub pending_save_token: Option<RegistrationToken>,
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
        Some(Self {
            state_path,
            dirty: false,
            pending_save_token: None,
        })
    }

    /// Mark the in-memory state as having diverged from disk.
    ///
    /// In Phase 2c.4 this will additionally schedule (or reset) a debounced calloop
    /// timer; for now it just sets the flag so call sites can be wired in their final
    /// shape across the remaining sub-phases without churn.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
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
    fn mark_dirty_then_take_dirty() {
        let c = cfg(false, Some("/tmp/x.json"));
        let mut m = SessionManager::new(&c).expect("manager");
        assert!(!m.take_dirty());
        m.mark_dirty();
        assert!(m.take_dirty());
        // take_dirty clears the flag.
        assert!(!m.take_dirty());
    }
}

//! Respawn-on-startup logic.
//!
//! At compositor start, the persisted [`SessionState`] is loaded from disk and each
//! saved [`WindowEntry`] is converted into an argv vector via [`resolve_launch_argv`]
//! and dispatched through `crate::utils::spawn`. The clients reconnect to the new
//! Wayland socket on their own.
//!
//! ## What this module does NOT do (yet)
//!
//! - **Position steering**: this v1 just spawns the processes. The new windows take
//!   whatever placement the layout decides — typically the focus-relative `Auto`
//!   target. Restoring exact column/tile positions requires intercepting the
//!   `add_window` pathway in `handlers/compositor.rs` and matching each new mapped
//!   window against the saved entries; that's Phase 3.5.
//!
//! - **Multi-instance matching**: when the saved state contains five konsoles, all
//!   five are spawned in saved-order, but Phase 3.5's per-window matching is what
//!   actually pins each new konsole to its specific saved (column, tile). Until
//!   then, multi-instance order on screen will reflect launch race-conditions.
//!
//! - **Cleanup of the state file after restore**: deliberate. The file is left in
//!   place so the *next* save replaces it atomically; deleting it on restore would
//!   create a window where a crash mid-startup loses the prior session.

use naru_config::SessionRestore;

use super::state::WindowEntry;

/// Build the argv vector to respawn a saved window.
///
/// Lookup precedence:
///
/// 1. A user-configured `launch-command` matching the entry's `app_id`.
/// 2. The bare `app_id` itself, used as the executable name.
///
/// `%s` substitution: any argv element exactly equal to the literal string `"%s"` is
/// replaced with the entry's captured `cwd`. When `cwd` is `None` (unreadable, dead
/// pid, sandboxed client) the `%s` element is **dropped** rather than substituted with
/// a sentinel — passing e.g. an empty string or `"/"` to a user app would be worse
/// than just letting the app start without an explicit cwd argument.
pub fn resolve_launch_argv(entry: &WindowEntry, config: &SessionRestore) -> Vec<String> {
    let template: Vec<String> = config
        .launch_command_for(&entry.app_id)
        .map(|lc| lc.command.clone())
        .unwrap_or_else(|| vec![entry.app_id.clone()]);

    let cwd_str = entry
        .cwd
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned());

    template
        .into_iter()
        .filter_map(|arg| {
            if arg == "%s" {
                cwd_str.clone()
            } else {
                Some(arg)
            }
        })
        .collect()
}

/// Respawn each saved window via its resolved launch-command.
///
/// Phase 3.5 hoists state loading out of this function and into
/// `SessionManager::new` (so the loaded entries can also be consulted by the
/// add_window matcher), making this a thin pass over an in-memory slice.
///
/// A corrupt or unreadable state file is handled at load time in `SessionManager`;
/// by the time entries reach this function they are already structurally valid.
pub fn restore_apps(config: &SessionRestore, entries: &[WindowEntry]) {
    if entries.is_empty() {
        debug!("session-restore: no prior session entries; nothing to respawn");
        return;
    }

    info!("session-restore: respawning {} window(s)", entries.len());

    for entry in entries {
        let argv = resolve_launch_argv(entry, config);
        if argv.is_empty() {
            // Defensive: resolve_launch_argv always produces at least one element
            // (the app_id fallback) unless the user set a launch-command to an empty
            // argv, which is a config bug worth noting.
            warn!(
                "session-restore: empty launch-command for app_id={:?}; skipping",
                entry.app_id
            );
            continue;
        }
        debug!(
            "session-restore: spawning {:?} (cwd={:?})",
            argv, entry.cwd
        );
        crate::utils::spawning::spawn(argv, None);
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use naru_config::LaunchCommand;

    use super::*;
    use crate::session::state::{Placement, WorkspaceRef};

    fn entry_with_cwd(app_id: &str, cwd: Option<&str>) -> WindowEntry {
        WindowEntry {
            app_id: app_id.into(),
            title: None,
            cwd: cwd.map(PathBuf::from),
            output: None,
            workspace: WorkspaceRef::Index { index: 0 },
            placement: Placement::Tiled {
                column_index: 0,
                tile_index: 0,
                is_fullscreen: false,
                is_maximized: false,
            },
        }
    }

    fn cfg(commands: Vec<LaunchCommand>) -> SessionRestore {
        SessionRestore {
            off: false,
            state_path: None,
            launch_commands: commands,
        }
    }

    #[test]
    fn resolve_substitutes_cwd_into_template() {
        let c = cfg(vec![LaunchCommand {
            app_id: "org.kde.konsole".into(),
            command: vec!["konsole".into(), "--workdir".into(), "%s".into()],
        }]);
        let e = entry_with_cwd("org.kde.konsole", Some("/home/leo/work"));
        assert_eq!(
            resolve_launch_argv(&e, &c),
            vec!["konsole", "--workdir", "/home/leo/work"]
        );
    }

    #[test]
    fn resolve_drops_placeholder_when_cwd_missing() {
        let c = cfg(vec![LaunchCommand {
            app_id: "org.kde.dolphin".into(),
            command: vec!["dolphin".into(), "%s".into()],
        }]);
        let e = entry_with_cwd("org.kde.dolphin", None);
        // %s vanishes when cwd is None, leaving just the executable.
        assert_eq!(resolve_launch_argv(&e, &c), vec!["dolphin"]);
    }

    #[test]
    fn resolve_falls_back_to_app_id_when_no_match() {
        let c = cfg(vec![]);
        let e = entry_with_cwd("firefox", Some("/home/leo"));
        // No launch-command for firefox → use "firefox" as the executable.
        // %s isn't in the template so cwd is unused.
        assert_eq!(resolve_launch_argv(&e, &c), vec!["firefox"]);
    }

    #[test]
    fn resolve_handles_multiple_placeholders() {
        // Pathological config but valid: multiple %s should all be substituted.
        let c = cfg(vec![LaunchCommand {
            app_id: "x".into(),
            command: vec!["x".into(), "%s".into(), "--cwd".into(), "%s".into()],
        }]);
        let e = entry_with_cwd("x", Some("/p"));
        assert_eq!(resolve_launch_argv(&e, &c), vec!["x", "/p", "--cwd", "/p"]);
    }

    #[test]
    fn resolve_preserves_non_placeholder_args() {
        let c = cfg(vec![LaunchCommand {
            app_id: "x".into(),
            command: vec!["x".into(), "--flag".into(), "value".into()],
        }]);
        let e = entry_with_cwd("x", None);
        assert_eq!(resolve_launch_argv(&e, &c), vec!["x", "--flag", "value"]);
    }
}

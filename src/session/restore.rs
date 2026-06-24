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

use std::path::Path;

use naru_config::SessionRestore;

use super::state::WindowEntry;

/// Build the argv vector to respawn a saved window.
///
/// Lookup precedence (generic by default — no per-app table):
///
/// 1. A user-configured `launch-command` override matching the entry's `app_id`.
/// 2. A captured PWA / site-specific-browser command (from the matching `.desktop`
///    file's `Exec`; see [`crate::session::desktop`]). Beats the browser launchers
///    below because those reopen the whole browser instead of the app.
/// 3. `flatpak run <flatpak_id>` when the window was a flatpak app (id captured at save).
/// 4. The captured executable path (native apps), run in the saved cwd by the caller.
/// 5. The bare `app_id` as a last-resort command name.
///
/// `%s` substitution applies only to case 1 (user templates): the literal `"%s"` is
/// replaced with the entry's captured `cwd` anywhere it appears in an argv element, and
/// any element containing `"%s"` is **dropped** when no cwd was captured rather than
/// substituted with a sentinel. Cases 2–5 don't need `%s` because the process is spawned
/// directly in the saved cwd ([`crate::utils::spawning::spawn_with_cwd`]).
pub fn resolve_launch_argv(entry: &WindowEntry, config: &SessionRestore) -> Vec<String> {
    // 1. Explicit user override.
    if let Some(lc) = config.launch_command_for(&entry.app_id) {
        return substitute_cwd(&lc.command, entry.cwd.as_deref());
    }
    // 2. PWA / site-specific-browser: a captured `.desktop` `Exec` argv. Must beat
    //    flatpak_id/exec, which would reopen the whole browser instead of the app.
    if let Some(command) = &entry.command {
        if !command.is_empty() {
            return command.clone();
        }
    }
    // 3. Flatpak app: relaunch through flatpak.
    if let Some(flatpak_id) = &entry.flatpak_id {
        return vec!["flatpak".into(), "run".into(), flatpak_id.clone()];
    }
    // 4. Native app: exec the captured binary (spawned in the saved cwd).
    if let Some(exec) = &entry.exec {
        return vec![exec.clone()];
    }
    // 5. Last resort.
    vec![entry.app_id.clone()]
}

/// Substitute the saved `cwd` into a user launch-command template's `%s` slots,
/// dropping any element that still references `%s` when no cwd is available.
fn substitute_cwd(template: &[String], cwd: Option<&Path>) -> Vec<String> {
    let cwd_str = cwd.map(|p| p.to_string_lossy().into_owned());
    template
        .iter()
        .filter_map(|arg| {
            if arg.contains("%s") {
                cwd_str.as_deref().map(|cwd| arg.replace("%s", cwd))
            } else {
                Some(arg.clone())
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
        info!(
            "session-restore: spawning {:?} (cwd={:?})",
            argv, entry.cwd
        );
        // Spawn in the saved directory so the respawned process's cwd matches the
        // saved entry — this is what lets `take_pending_for` pin multi-instance
        // same-app windows (terminals) back to their specific slot by cwd.
        crate::utils::spawning::spawn_with_cwd(argv, None, entry.cwd.clone());
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use naru_config::LaunchCommand;

    use super::*;
    use crate::session::state::{Placement, WorkspaceRef};

    fn entry(app_id: &str, cwd: Option<&str>) -> WindowEntry {
        WindowEntry {
            app_id: app_id.into(),
            title: None,
            cwd: cwd.map(PathBuf::from),
            flatpak_id: None,
            exec: None,
            command: None,
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
        }
    }

    fn cfg(commands: Vec<LaunchCommand>) -> SessionRestore {
        SessionRestore {
            off: false,
            state_path: None,
            launch_commands: commands,
            cwd_from_child: Vec::new(),
        }
    }

    #[test]
    fn resolve_uses_flatpak_id_generically() {
        // No launch-command needed: a captured flatpak id drives `flatpak run`.
        let mut e = entry("brave-browser", None);
        e.flatpak_id = Some("com.brave.Browser".into());
        assert_eq!(
            resolve_launch_argv(&e, &cfg(vec![])),
            vec!["flatpak", "run", "com.brave.Browser"]
        );
    }

    #[test]
    fn resolve_uses_exec_generically() {
        // A native app reopens via its captured executable; cwd is applied by the
        // spawner, not the argv, so no `--workdir` is needed.
        let mut e = entry("org.kde.konsole", Some("/home/leo/work"));
        e.exec = Some("/usr/bin/konsole".into());
        assert_eq!(resolve_launch_argv(&e, &cfg(vec![])), vec!["/usr/bin/konsole"]);
    }

    #[test]
    fn resolve_prefers_flatpak_over_exec() {
        // If both happen to be set, flatpak wins (the exe path is inside the sandbox).
        let mut e = entry("brave-browser", None);
        e.flatpak_id = Some("com.brave.Browser".into());
        e.exec = Some("/app/brave/brave".into());
        assert_eq!(
            resolve_launch_argv(&e, &cfg(vec![])),
            vec!["flatpak", "run", "com.brave.Browser"]
        );
    }

    #[test]
    fn resolve_user_override_wins_over_generic() {
        let c = cfg(vec![LaunchCommand {
            app_id: "brave-browser".into(),
            command: vec!["my-brave-wrapper".into(), "%s".into()],
        }]);
        let mut e = entry("brave-browser", Some("/dl"));
        e.flatpak_id = Some("com.brave.Browser".into());
        // The user override is used, with %s substituted, ignoring the flatpak id.
        assert_eq!(resolve_launch_argv(&e, &c), vec!["my-brave-wrapper", "/dl"]);
    }

    #[test]
    fn resolve_override_drops_placeholder_when_cwd_missing() {
        let c = cfg(vec![LaunchCommand {
            app_id: "org.kde.dolphin".into(),
            command: vec!["dolphin".into(), "%s".into()],
        }]);
        let e = entry("org.kde.dolphin", None);
        assert_eq!(resolve_launch_argv(&e, &c), vec!["dolphin"]);
    }

    #[test]
    fn resolve_uses_captured_pwa_command() {
        // A PWA window carries the exact `.desktop` Exec argv; it is used verbatim.
        let mut e = entry("crx_abc", None);
        e.command = Some(vec![
            "flatpak".into(),
            "run".into(),
            "net.imput.helium".into(),
            "--app-id=abc".into(),
        ]);
        assert_eq!(
            resolve_launch_argv(&e, &cfg(vec![])),
            vec!["flatpak", "run", "net.imput.helium", "--app-id=abc"]
        );
    }

    #[test]
    fn resolve_prefers_pwa_command_over_flatpak_id() {
        // The browser's flatpak id would reopen the whole browser, so the captured
        // per-app command must win.
        let mut e = entry("crx_abc", None);
        e.flatpak_id = Some("net.imput.helium".into());
        e.command = Some(vec![
            "flatpak".into(),
            "run".into(),
            "net.imput.helium".into(),
            "--app-id=abc".into(),
        ]);
        assert_eq!(
            resolve_launch_argv(&e, &cfg(vec![])),
            vec!["flatpak", "run", "net.imput.helium", "--app-id=abc"]
        );
    }

    #[test]
    fn resolve_user_override_wins_over_pwa_command() {
        let c = cfg(vec![LaunchCommand {
            app_id: "crx_abc".into(),
            command: vec!["my-launcher".into()],
        }]);
        let mut e = entry("crx_abc", None);
        e.command = Some(vec!["flatpak".into(), "run".into(), "x".into()]);
        assert_eq!(resolve_launch_argv(&e, &c), vec!["my-launcher"]);
    }

    #[test]
    fn resolve_falls_back_to_app_id_when_nothing_known() {
        // No override, no flatpak id, no exec → bare app_id as a last resort.
        let e = entry("firefox", Some("/home/leo"));
        assert_eq!(resolve_launch_argv(&e, &cfg(vec![])), vec!["firefox"]);
    }
}

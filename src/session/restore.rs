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

use super::state::{TmuxAttach, WindowEntry};

/// Build the argv vector to respawn a saved window.
///
/// Lookup precedence (generic by default — no per-app table):
///
/// 1. A user-configured `launch-command` override matching the entry's `app_id`.
/// 2. A captured PWA / site-specific-browser command (from the matching `.desktop`
///    file's `Exec`; see [`crate::session::desktop`]). Beats the browser launchers
///    below because those reopen the whole browser instead of the app.
/// 3. Konsole: `<konsole> --workdir <cwd>`, so the saved per-window directory is pinned
///    explicitly rather than left to process-cwd inheritance (which single-instance
///    forwarding can override).
/// 4. `flatpak run <flatpak_id>` when the window was a flatpak app (id captured at save).
/// 5. The captured executable path (native apps), run in the saved cwd by the caller.
/// 6. The bare `app_id` as a last-resort command name.
///
/// `%s` substitution applies only to case 1 (user templates): the literal `"%s"` is
/// replaced with the entry's captured `cwd` anywhere it appears in an argv element, and
/// any element containing `"%s"` is **dropped** when no cwd was captured rather than
/// substituted with a sentinel. Cases 2–6 don't need `%s` because the process is spawned
/// directly in the saved cwd ([`crate::utils::spawning::spawn_with_cwd`]).
pub fn resolve_launch_argv(entry: &WindowEntry, config: &SessionRestore) -> Vec<String> {
    // 1. Explicit user override.
    if let Some(lc) = config.launch_command_for(&entry.app_id) {
        return substitute_cwd(&lc.command, entry.cwd.as_deref());
    }
    // 1b. tmux reattach: a terminal that was running tmux is relaunched reattaching
    //     to its session (`tmux new-session -A` attaches if it exists, else creates).
    //     The captured terminal binary runs the tmux command inside itself via the
    //     common `-e` convention; Konsole also gets `--workdir` for the directory.
    //     Terminals that don't take `-e` (kitty/foot/wezterm) use a `launch-command`
    //     override (case 1) instead. Falls through when we can't build a command.
    if let Some(attach) = &entry.tmux_session {
        if let Some(argv) = tmux_reattach_argv(entry, attach) {
            return argv;
        }
    }
    // 1c. Claude Code resume: a terminal that was running `claude` directly reopens
    //     running `claude --resume <id>` so the conversation continues. Captured
    //     only when *not* under tmux (a claude inside tmux lives under the tmux
    //     server, restored by the reattach above), so the two never compete;
    //     ordering tmux first is belt-and-suspenders. Falls through when we can't
    //     build a command (no captured binary for a non-Konsole terminal).
    if let Some(session_id) = &entry.claude_session {
        if let Some(argv) =
            terminal_exec_argv(entry, crate::session::claude::resume_command(session_id))
        {
            return argv;
        }
    }
    // 2. PWA / site-specific-browser: a captured `.desktop` `Exec` argv. Must beat
    //    flatpak_id/exec, which would reopen the whole browser instead of the app.
    //    A compositor restart kills the browser uncleanly, so it's relaunched with
    //    the crash-restore bubble suppressed (see [`suppress_crash_restore`]).
    if let Some(command) = &entry.command {
        if !command.is_empty() {
            return suppress_crash_restore(command.clone());
        }
    }
    // 3. Konsole: pin the saved cwd with `--workdir`. A bare relaunch would rely on
    //    process-cwd inheritance, which single-instance forwarding into an already-running
    //    Konsole can override; `--workdir` sets the directory for this window explicitly.
    if entry.app_id == "org.kde.konsole" {
        if let Some(cwd) = &entry.cwd {
            let bin = entry.exec.clone().unwrap_or_else(|| "konsole".into());
            return vec![bin, "--workdir".into(), cwd.to_string_lossy().into_owned()];
        }
    }
    // 4. Flatpak app: relaunch through flatpak.
    if let Some(flatpak_id) = &entry.flatpak_id {
        return vec!["flatpak".into(), "run".into(), flatpak_id.clone()];
    }
    // 5. Native app: exec the captured binary (spawned in the saved cwd).
    if let Some(exec) = &entry.exec {
        return vec![exec.clone()];
    }
    // 6. Last resort.
    vec![entry.app_id.clone()]
}

/// Chromium switch that suppresses the "restore pages?" crash-recovery bubble.
const HIDE_CRASH_RESTORE_BUBBLE: &str = "--hide-crash-restore-bubble";

/// Append [`HIDE_CRASH_RESTORE_BUBBLE`] to a PWA launch argv (idempotently).
///
/// A compositor restart kills the browser uncleanly, so on relaunch Chromium
/// (Brave/Helium/Chrome) believes it crashed and prompts to restore the previous
/// pages — interfering with naru's own restore of the PWA. Every captured
/// `command` is a Chromium site-specific-browser launcher, so appending this flag
/// makes the app just reopen without the prompt. Trailing Chromium flags reach the
/// browser: for a flatpak SSB (`flatpak run … com.brave.Browser --app-id=…`)
/// everything after the app id is forwarded into the sandbox, and a native SSB
/// passes it straight through.
fn suppress_crash_restore(mut command: Vec<String>) -> Vec<String> {
    if !command.iter().any(|a| a == HIDE_CRASH_RESTORE_BUBBLE) {
        command.push(HIDE_CRASH_RESTORE_BUBBLE.to_owned());
    }
    command
}

/// Build the argv that relaunches `entry`'s terminal reattaching to its tmux
/// session. The captured terminal binary runs `tmux [<socket>] new-session -A -s
/// <session>` inside itself (the socket flags are carried in `attach` so restore
/// reaches the same server). Returns `None` when there's no binary to run, so the
/// caller falls through to a plain relaunch.
fn tmux_reattach_argv(entry: &WindowEntry, attach: &TmuxAttach) -> Option<Vec<String>> {
    terminal_exec_argv(entry, crate::session::tmux::reattach_command(attach))
}

/// Wrap an in-terminal command (`inner`, e.g. a tmux reattach or `claude --resume`)
/// in the argv that relaunches `entry`'s terminal running it. Konsole is invoked
/// as `konsole [--workdir <cwd>] -e <inner>`; any other terminal as `<exec> -e
/// <inner>` (the common `-e` convention). Terminals that don't take `-e`
/// (kitty/foot/wezterm) use a `launch-command` override instead. Returns `None`
/// when there's no binary to run (no captured `exec` for a non-Konsole terminal).
fn terminal_exec_argv(entry: &WindowEntry, inner: Vec<String>) -> Option<Vec<String>> {
    if entry.app_id == "org.kde.konsole" {
        let bin = entry.exec.clone().unwrap_or_else(|| "konsole".into());
        let mut argv = vec![bin];
        if let Some(cwd) = &entry.cwd {
            argv.push("--workdir".into());
            argv.push(cwd.to_string_lossy().into_owned());
        }
        argv.push("-e".into());
        argv.extend(inner);
        return Some(argv);
    }

    let exec = entry.exec.clone()?;
    let mut argv = vec![exec, "-e".into()];
    argv.extend(inner);
    Some(argv)
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

/// The deduplicated `(argv, cwd)` list to spawn for `entries`, in order.
///
/// A single-instance app that restores its own session — any browser (Librewolf,
/// Brave, Chrome, Firefox, …) — would open **duplicate** windows if launched once per
/// saved window:
/// the first launch reopens all the windows the browser itself remembers, and every
/// further launch just forwards to the running instance and pops one extra blank
/// window. So each distinct whole-app launch command is issued only **once**, and the
/// browser is left to create its own windows; the `add_window` matcher then pins each
/// to its saved slot (by app_id, cwd, and title) as it maps.
///
/// Terminals are exempt: they open exactly one window per launch and don't restore a
/// session of their own, so each saved terminal still needs its own spawn (its
/// per-window `--workdir` / `-e` command usually differs anyway, but same-cwd konsoles
/// share one argv and must not collapse). PWAs, tmux/claude terminals, and user
/// launch-commands carry per-window arguments, so their argvs differ naturally.
fn plan_launches(
    config: &SessionRestore,
    entries: &[WindowEntry],
) -> Vec<(Vec<String>, Option<std::path::PathBuf>)> {
    let mut launched: std::collections::HashSet<Vec<String>> = std::collections::HashSet::new();
    let mut plan = Vec::new();
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
        // Terminals launch one window per invocation; everything else collapses to a
        // single launch per distinct command.
        let is_terminal = config.reads_cwd_from_child(&entry.app_id);
        if !is_terminal && !launched.insert(argv.clone()) {
            debug!(
                "session-restore: {argv:?} already launched; the app restores its own \
                 remaining windows — skipping duplicate spawn"
            );
            continue;
        }
        plan.push((argv, entry.cwd.clone()));
    }
    plan
}

/// Respawn the saved windows, deduplicating self-restoring apps (see [`plan_launches`]).
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

    let plan = plan_launches(config, entries);
    info!(
        "session-restore: respawning {} window(s) via {} launch(es)",
        entries.len(),
        plan.len(),
    );

    for (argv, cwd) in plan {
        info!("session-restore: spawning {argv:?} (cwd={cwd:?})");
        // Spawn in the saved directory so the respawned process's cwd matches the
        // saved entry — this is what lets `take_pending_for` pin multi-instance
        // same-app windows (terminals) back to their specific slot by cwd.
        crate::utils::spawning::spawn_with_cwd(argv, None, cwd);
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
    fn plan_launches_dedups_self_restoring_apps_but_not_terminals() {
        let mut config = cfg(vec![]);
        config.cwd_from_child = vec!["org.kde.konsole".into()];

        let konsole = |cwd: &str| {
            let mut e = entry("org.kde.konsole", Some(cwd));
            e.exec = Some("/usr/bin/konsole".into());
            e
        };
        let librewolf = |title: &str| {
            let mut e = entry("librewolf", Some("/home/leo"));
            e.flatpak_id = Some("io.gitlab.librewolf-community".into());
            e.title = Some(title.into());
            e
        };
        // Two konsoles in the *same* cwd (identical argv) interleaved with three
        // librewolf windows.
        let entries = vec![
            konsole("/home/leo"),
            librewolf("Mail"),
            konsole("/home/leo"),
            librewolf("Docs"),
            librewolf("News"),
        ];

        let plan = plan_launches(&config, &entries);

        let konsole_launches = plan
            .iter()
            .filter(|(a, _)| a.first().map(String::as_str) == Some("/usr/bin/konsole"))
            .count();
        let librewolf_launches = plan
            .iter()
            .filter(|(a, _)| a.iter().any(|s| s == "io.gitlab.librewolf-community"))
            .count();
        // Terminals always launch per window (even with an identical argv); the
        // self-restoring browser launches exactly once.
        assert_eq!(konsole_launches, 2, "each konsole opens its own window");
        assert_eq!(
            librewolf_launches, 1,
            "librewolf launches once and restores its own windows",
        );
        assert_eq!(plan.len(), 3);
    }

    #[test]
    fn plan_launches_dedups_every_browser_regardless_of_launcher() {
        // The dedup is browser-agnostic: it keys on the launch command, not any
        // per-browser list. A flatpak browser (Brave, via flatpak_id) and a native
        // one (Firefox, via exec) both collapse to a single launch each.
        let config = cfg(vec![]); // no terminals configured
        let brave = || {
            let mut e = entry("brave-browser", Some("/home/leo"));
            e.flatpak_id = Some("com.brave.Browser".into());
            e
        };
        let firefox = || {
            let mut e = entry("firefox", Some("/home/leo"));
            e.exec = Some("/usr/bin/firefox".into());
            e
        };
        let entries = vec![brave(), firefox(), brave(), firefox(), brave()];

        let plan = plan_launches(&config, &entries);

        assert_eq!(plan.len(), 2, "three Braves + two Firefoxes → one launch each");
        assert!(plan
            .iter()
            .any(|(a, _)| a.iter().any(|s| s == "com.brave.Browser")));
        assert!(plan.iter().any(|(a, _)| a == &vec!["/usr/bin/firefox".to_string()]));
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
        // spawner, not the argv, so no `--workdir` is needed. (Konsole has its own
        // --workdir branch — covered separately — so use a different native app here.)
        let mut e = entry("org.kde.kate", Some("/home/leo/work"));
        e.exec = Some("/usr/bin/kate".into());
        assert_eq!(resolve_launch_argv(&e, &cfg(vec![])), vec!["/usr/bin/kate"]);
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
        // A PWA window carries the exact `.desktop` Exec argv; it's used as-is, with
        // the crash-restore bubble suppressed so the browser doesn't prompt on relaunch.
        let mut e = entry("crx_abc", None);
        e.command = Some(vec![
            "flatpak".into(),
            "run".into(),
            "net.imput.helium".into(),
            "--app-id=abc".into(),
        ]);
        assert_eq!(
            resolve_launch_argv(&e, &cfg(vec![])),
            vec![
                "flatpak",
                "run",
                "net.imput.helium",
                "--app-id=abc",
                "--hide-crash-restore-bubble",
            ]
        );
    }

    #[test]
    fn resolve_prefers_pwa_command_over_flatpak_id() {
        // The browser's flatpak id would reopen the whole browser, so the captured
        // per-app command must win (again with the crash-restore bubble suppressed).
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
            vec![
                "flatpak",
                "run",
                "net.imput.helium",
                "--app-id=abc",
                "--hide-crash-restore-bubble",
            ]
        );
    }

    #[test]
    fn resolve_pwa_command_suppresses_crash_bubble_idempotently() {
        // If the captured command already carries the flag (e.g. a re-saved entry),
        // it isn't appended twice.
        let mut e = entry("crx_abc", None);
        e.command = Some(vec![
            "brave".into(),
            "--app-id=abc".into(),
            "--hide-crash-restore-bubble".into(),
        ]);
        assert_eq!(
            resolve_launch_argv(&e, &cfg(vec![])),
            vec!["brave", "--app-id=abc", "--hide-crash-restore-bubble"]
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
    fn resolve_konsole_uses_workdir() {
        // Konsole relaunch pins the saved cwd via --workdir, using the captured exec.
        let mut e = entry("org.kde.konsole", Some("/home/leo/work"));
        e.exec = Some("/usr/bin/konsole".into());
        assert_eq!(
            resolve_launch_argv(&e, &cfg(vec![])),
            vec!["/usr/bin/konsole", "--workdir", "/home/leo/work"]
        );
    }

    #[test]
    fn resolve_konsole_without_cwd_falls_through_to_exec() {
        // No saved cwd → nothing to pin, so the generic exec path applies.
        let mut e = entry("org.kde.konsole", None);
        e.exec = Some("/usr/bin/konsole".into());
        assert_eq!(resolve_launch_argv(&e, &cfg(vec![])), vec!["/usr/bin/konsole"]);
    }

    #[test]
    fn resolve_user_override_wins_over_konsole_workdir() {
        let c = cfg(vec![LaunchCommand {
            app_id: "org.kde.konsole".into(),
            command: vec!["my-term".into(), "%s".into()],
        }]);
        let mut e = entry("org.kde.konsole", Some("/dl"));
        e.exec = Some("/usr/bin/konsole".into());
        assert_eq!(resolve_launch_argv(&e, &c), vec!["my-term", "/dl"]);
    }

    #[test]
    fn resolve_falls_back_to_app_id_when_nothing_known() {
        // No override, no flatpak id, no exec → bare app_id as a last resort.
        let e = entry("firefox", Some("/home/leo"));
        assert_eq!(resolve_launch_argv(&e, &cfg(vec![])), vec!["firefox"]);
    }

    #[test]
    fn resolve_konsole_tmux_reattaches_with_workdir() {
        // Konsole that was running tmux reopens via --workdir + -e tmux attach-or-create.
        let mut e = entry("org.kde.konsole", Some("/home/leo/work"));
        e.exec = Some("/usr/bin/konsole".into());
        e.tmux_session = Some(TmuxAttach::NameOnly("dev".into()));
        assert_eq!(
            resolve_launch_argv(&e, &cfg(vec![])),
            vec![
                "/usr/bin/konsole",
                "--workdir",
                "/home/leo/work",
                "-e",
                "tmux",
                "new-session",
                "-A",
                "-s",
                "dev",
            ]
        );
    }

    #[test]
    fn resolve_konsole_tmux_without_cwd_omits_workdir() {
        let mut e = entry("org.kde.konsole", None);
        e.exec = Some("/usr/bin/konsole".into());
        e.tmux_session = Some(TmuxAttach::NameOnly("dev".into()));
        assert_eq!(
            resolve_launch_argv(&e, &cfg(vec![])),
            vec!["/usr/bin/konsole", "-e", "tmux", "new-session", "-A", "-s", "dev"]
        );
    }

    #[test]
    fn resolve_generic_terminal_tmux_uses_dash_e() {
        // A non-Konsole terminal reattaches via its captured binary + `-e`.
        let mut e = entry("Alacritty", Some("/ws"));
        e.exec = Some("/usr/bin/alacritty".into());
        e.tmux_session = Some(TmuxAttach::NameOnly("work".into()));
        assert_eq!(
            resolve_launch_argv(&e, &cfg(vec![])),
            vec!["/usr/bin/alacritty", "-e", "tmux", "new-session", "-A", "-s", "work"]
        );
    }

    #[test]
    fn resolve_tmux_custom_socket_prepends_flags() {
        // A session captured on a custom socket reattaches with the same `-L`/`-S`
        // flags so restore reaches the same tmux server.
        let mut e = entry("Alacritty", Some("/ws"));
        e.exec = Some("/usr/bin/alacritty".into());
        e.tmux_session = Some(TmuxAttach::new(
            "work".into(),
            vec!["-L".into(), "ws".into()],
        ));
        assert_eq!(
            resolve_launch_argv(&e, &cfg(vec![])),
            vec![
                "/usr/bin/alacritty",
                "-e",
                "tmux",
                "-L",
                "ws",
                "new-session",
                "-A",
                "-s",
                "work",
            ]
        );
    }

    #[test]
    fn resolve_user_override_wins_over_tmux() {
        // An explicit launch-command is honored even when a tmux session was captured.
        let c = cfg(vec![LaunchCommand {
            app_id: "Alacritty".into(),
            command: vec!["my-term".into()],
        }]);
        let mut e = entry("Alacritty", None);
        e.exec = Some("/usr/bin/alacritty".into());
        e.tmux_session = Some(TmuxAttach::NameOnly("work".into()));
        assert_eq!(resolve_launch_argv(&e, &c), vec!["my-term"]);
    }

    #[test]
    fn resolve_tmux_without_exec_falls_through() {
        // No captured binary for a non-Konsole terminal → can't build a reattach
        // command, so fall through to the normal (here: app_id) path.
        let mut e = entry("Alacritty", None);
        e.tmux_session = Some(TmuxAttach::NameOnly("work".into()));
        assert_eq!(resolve_launch_argv(&e, &cfg(vec![])), vec!["Alacritty"]);
    }

    #[test]
    fn resolve_konsole_claude_resumes_with_workdir() {
        // Konsole that was running claude reopens via --workdir + -e claude --resume.
        let mut e = entry("org.kde.konsole", Some("/home/leo/work"));
        e.exec = Some("/usr/bin/konsole".into());
        e.claude_session = Some("abc-123".into());
        assert_eq!(
            resolve_launch_argv(&e, &cfg(vec![])),
            vec![
                "/usr/bin/konsole",
                "--workdir",
                "/home/leo/work",
                "-e",
                "claude",
                "--resume",
                "abc-123",
                "--dangerously-skip-permissions",
                "--remote-control",
            ]
        );
    }

    #[test]
    fn resolve_generic_terminal_claude_uses_dash_e() {
        // A non-Konsole terminal resumes via its captured binary + `-e`.
        let mut e = entry("Alacritty", Some("/ws"));
        e.exec = Some("/usr/bin/alacritty".into());
        e.claude_session = Some("sess-42".into());
        assert_eq!(
            resolve_launch_argv(&e, &cfg(vec![])),
            vec![
                "/usr/bin/alacritty",
                "-e",
                "claude",
                "--resume",
                "sess-42",
                "--dangerously-skip-permissions",
                "--remote-control",
            ]
        );
    }

    #[test]
    fn resolve_tmux_wins_over_claude() {
        // Should both ever be set, tmux reattach takes precedence (a claude inside
        // tmux is restored by reattaching the live session, not a fresh --resume).
        let mut e = entry("Alacritty", Some("/ws"));
        e.exec = Some("/usr/bin/alacritty".into());
        e.tmux_session = Some(TmuxAttach::NameOnly("work".into()));
        e.claude_session = Some("sess-42".into());
        assert_eq!(
            resolve_launch_argv(&e, &cfg(vec![])),
            vec!["/usr/bin/alacritty", "-e", "tmux", "new-session", "-A", "-s", "work"]
        );
    }

    #[test]
    fn resolve_user_override_wins_over_claude() {
        // An explicit launch-command is honored even when a claude session was captured.
        let c = cfg(vec![LaunchCommand {
            app_id: "Alacritty".into(),
            command: vec!["my-term".into()],
        }]);
        let mut e = entry("Alacritty", None);
        e.exec = Some("/usr/bin/alacritty".into());
        e.claude_session = Some("sess-42".into());
        assert_eq!(resolve_launch_argv(&e, &c), vec!["my-term"]);
    }

    #[test]
    fn resolve_claude_without_exec_falls_through() {
        // No captured binary for a non-Konsole terminal → can't build a resume
        // command, so fall through to the app_id path.
        let mut e = entry("Alacritty", None);
        e.claude_session = Some("sess-42".into());
        assert_eq!(resolve_launch_argv(&e, &cfg(vec![])), vec!["Alacritty"]);
    }
}

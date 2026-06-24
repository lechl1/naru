//! Configuration for the session-restore feature.
//!
//! Session restore persists the set of open windows and their layout positions across
//! compositor restarts. On startup, the saved entries are respawned via per-app-id
//! launch-command templates (with `%s` substituted by the captured working directory),
//! and as each new window maps it is steered into its saved slot.
//!
//! This is the configuration schema only — the save/restore plumbing lives in the
//! main crate's `session` module. Disabled by default until the feature stabilises.

use crate::utils::MergeWith;

/// Resolved, runtime-ready session-restore configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRestore {
    /// When true, session save/restore is disabled (no state file is written, none is read).
    pub off: bool,
    /// Override path for the persisted session state file.
    ///
    /// `None` means use the platform default — `$XDG_STATE_HOME/naru/session.json`,
    /// falling back to `$HOME/.local/state/naru/session.json`.
    pub state_path: Option<String>,
    /// Per-app-id launch-command templates.
    ///
    /// Each entry maps a window's xdg `app_id` to the argv vector used to respawn it
    /// at restore time. Any argv element equal to the literal string `"%s"` is
    /// substituted with the captured working directory of the original window. If the
    /// cwd was not captured (sandboxed client, dead pid, etc.), `"%s"` is dropped from
    /// the argv. Apps without a matching entry fall back to spawning the bare `app_id`
    /// as the executable.
    pub launch_commands: Vec<LaunchCommand>,
}

/// One per-app-id launch-command template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchCommand {
    pub app_id: String,
    pub command: Vec<String>,
    /// Read the captured working directory from the window's foreground **child**
    /// process rather than the client process itself.
    ///
    /// Terminal emulators (konsole, etc.) keep their own process cwd at wherever they
    /// were launched (typically `$HOME`); the directory the user actually navigated to
    /// lives in the child shell. With this set, the cwd is resolved by descending into
    /// the client's process tree and read **fresh at save time** (so a later `cd` is
    /// honoured), instead of being captured once at window-map time. Apps where the
    /// client process holds the meaningful cwd (file managers, editors) should leave
    /// this off.
    pub cwd_from_child: bool,
}

impl Default for SessionRestore {
    fn default() -> Self {
        Self {
            // Off by default in v1 until restore-on-startup is wired up and tested.
            off: true,
            state_path: None,
            launch_commands: builtin_launch_commands(),
        }
    }
}

/// Built-in launch commands shipped with the compositor.
///
/// User-provided `launch-command` entries replace this list wholesale (see `MergeWith`).
fn builtin_launch_commands() -> Vec<LaunchCommand> {
    vec![
        LaunchCommand {
            app_id: "org.kde.dolphin".into(),
            command: vec!["dolphin".into(), "%s".into()],
            cwd_from_child: false,
        },
        LaunchCommand {
            app_id: "org.kde.konsole".into(),
            command: vec!["konsole".into(), "--workdir".into(), "%s".into()],
            // konsole's own process cwd stays at its launch dir; the live working
            // directory is the child shell's. Descend to it and re-read at save time.
            cwd_from_child: true,
        },
    ]
}

#[derive(knuffel::Decode, Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionRestorePart {
    #[knuffel(child)]
    pub off: bool,
    #[knuffel(child)]
    pub on: bool,
    #[knuffel(child, unwrap(argument))]
    pub state_path: Option<String>,
    #[knuffel(children(name = "launch-command"))]
    pub launch_commands: Vec<LaunchCommandPart>,
}

#[derive(knuffel::Decode, Debug, Clone, PartialEq, Eq)]
pub struct LaunchCommandPart {
    #[knuffel(property(name = "app-id"))]
    pub app_id: String,
    #[knuffel(property(name = "cwd-from-child"), default)]
    pub cwd_from_child: bool,
    #[knuffel(arguments)]
    pub command: Vec<String>,
}

impl MergeWith<SessionRestorePart> for SessionRestore {
    fn merge_with(&mut self, part: &SessionRestorePart) {
        // off/on toggle: matches the XwaylandSatellite convention — `off` wins if both
        // are present, but a later `on` flips it back so includes can override base.
        self.off |= part.off;
        if part.on {
            self.off = false;
        }
        merge_clone_opt!((self, part), state_path);
        if !part.launch_commands.is_empty() {
            // User-provided launch-commands fully replace the built-in list. This is
            // intentional: it lets users disable a built-in mapping (e.g. konsole)
            // without forcing them to also re-declare the others.
            self.launch_commands = part
                .launch_commands
                .iter()
                .map(|p| LaunchCommand {
                    app_id: p.app_id.clone(),
                    command: p.command.clone(),
                    cwd_from_child: p.cwd_from_child,
                })
                .collect();
        }
    }
}

impl SessionRestore {
    /// Look up the launch-command template for a given app_id.
    ///
    /// Returns `None` if no entry matches; callers should fall back to spawning the
    /// bare `app_id` as the executable.
    pub fn launch_command_for(&self, app_id: &str) -> Option<&LaunchCommand> {
        self.launch_commands.iter().find(|lc| lc.app_id == app_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_include_dolphin_and_konsole() {
        let sr = SessionRestore::default();
        assert!(sr.off, "feature is off by default");
        assert!(sr.launch_command_for("org.kde.dolphin").is_some());
        assert!(sr.launch_command_for("org.kde.konsole").is_some());
        assert!(sr.launch_command_for("does.not.exist").is_none());
    }

    #[test]
    fn user_launch_commands_replace_builtins() {
        let mut sr = SessionRestore::default();
        let part = SessionRestorePart {
            off: false,
            on: true,
            state_path: None,
            launch_commands: vec![LaunchCommandPart {
                app_id: "firefox".into(),
                cwd_from_child: false,
                command: vec!["firefox".into(), "--new-window".into()],
            }],
        };
        sr.merge_with(&part);
        assert!(!sr.off);
        assert_eq!(sr.launch_commands.len(), 1);
        assert_eq!(sr.launch_commands[0].app_id, "firefox");
        // Built-ins are gone once the user defines any launch-command.
        assert!(sr.launch_command_for("org.kde.dolphin").is_none());
    }

    #[test]
    fn empty_user_block_keeps_builtins() {
        let mut sr = SessionRestore::default();
        let part = SessionRestorePart::default();
        sr.merge_with(&part);
        assert!(sr.launch_command_for("org.kde.dolphin").is_some());
    }
}

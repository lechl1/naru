//! Configuration for the session-restore feature.
//!
//! Session restore persists the set of open windows and their layout positions across
//! compositor restarts. On startup each saved window is relaunched — generically,
//! using launch info captured from its process (a flatpak id, or the executable path,
//! run in the saved working directory) — and steered back into its saved slot as it
//! maps. `launch-command` entries are an optional per-app-id override for the rare apps
//! the generic path can't relaunch correctly.
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
    /// Optional per-app-id launch-command overrides.
    ///
    /// Restore is generic by default: each window is relaunched from launch info
    /// captured at save time (flatpak id → `flatpak run <id>`, otherwise the
    /// executable path), run in the saved working directory. An entry here overrides
    /// that for a specific `app_id` — the argv is used verbatim, with any element equal
    /// to the literal `"%s"` replaced by the saved cwd (dropped if no cwd was captured).
    /// Empty by default; user entries are additive (they don't wipe a built-in list,
    /// because there isn't one anymore).
    pub launch_commands: Vec<LaunchCommand>,
    /// App-ids whose working directory should be read from the foreground **child**
    /// process rather than the client process itself.
    ///
    /// Terminal emulators keep their own process cwd at wherever they were launched
    /// (typically `$HOME`); the directory the user navigated to lives in the child
    /// shell. For these app-ids the cwd is resolved by descending the process tree and
    /// read fresh at save time, so a later `cd` is honoured. Defaults to common
    /// terminals; user values are added to the defaults.
    pub cwd_from_child: Vec<String>,
}

/// One per-app-id launch-command override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchCommand {
    pub app_id: String,
    pub command: Vec<String>,
}

impl Default for SessionRestore {
    fn default() -> Self {
        Self {
            // Off by default in v1 until restore-on-startup is wired up and tested.
            off: true,
            state_path: None,
            launch_commands: Vec::new(),
            cwd_from_child: builtin_cwd_from_child(),
        }
    }
}

/// App-ids that default to reading cwd from their child shell (terminal emulators).
fn builtin_cwd_from_child() -> Vec<String> {
    vec![
        "org.kde.konsole".into(),
        "org.kde.yakuake".into(),
        "Alacritty".into(),
        "kitty".into(),
        "foot".into(),
        "org.wezfurlong.wezterm".into(),
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
    #[knuffel(child, unwrap(arguments))]
    pub cwd_from_child: Option<Vec<String>>,
}

#[derive(knuffel::Decode, Debug, Clone, PartialEq, Eq)]
pub struct LaunchCommandPart {
    #[knuffel(property(name = "app-id"))]
    pub app_id: String,
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
        // User launch-commands are additive overrides — a later entry for an app_id
        // wins over an earlier one via `launch_command_for`'s find-first semantics.
        for p in &part.launch_commands {
            self.launch_commands.push(LaunchCommand {
                app_id: p.app_id.clone(),
                command: p.command.clone(),
            });
        }
        // cwd-from-child app-ids are added to the built-in terminal defaults.
        if let Some(extra) = &part.cwd_from_child {
            for app_id in extra {
                if !self.cwd_from_child.iter().any(|a| a == app_id) {
                    self.cwd_from_child.push(app_id.clone());
                }
            }
        }
    }
}

impl SessionRestore {
    /// Look up a user-provided launch-command override for `app_id`, if any.
    pub fn launch_command_for(&self, app_id: &str) -> Option<&LaunchCommand> {
        self.launch_commands.iter().find(|lc| lc.app_id == app_id)
    }

    /// Whether `app_id`'s working directory should be read from its child shell.
    pub fn reads_cwd_from_child(&self, app_id: &str) -> bool {
        self.cwd_from_child.iter().any(|a| a == app_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_generic() {
        let sr = SessionRestore::default();
        assert!(sr.off, "feature is off by default");
        // No hardcoded launch-commands — restore is generic.
        assert!(sr.launch_commands.is_empty());
        // Terminals still default to child-cwd capture.
        assert!(sr.reads_cwd_from_child("org.kde.konsole"));
        assert!(!sr.reads_cwd_from_child("org.kde.dolphin"));
    }

    #[test]
    fn user_launch_commands_are_additive_overrides() {
        let mut sr = SessionRestore::default();
        let part = SessionRestorePart {
            off: false,
            on: true,
            state_path: None,
            launch_commands: vec![LaunchCommandPart {
                app_id: "firefox".into(),
                command: vec!["firefox".into(), "--new-window".into()],
            }],
            cwd_from_child: None,
        };
        sr.merge_with(&part);
        assert!(!sr.off);
        assert_eq!(sr.launch_command_for("firefox").unwrap().command.len(), 2);
    }

    #[test]
    fn user_cwd_from_child_adds_to_defaults() {
        let mut sr = SessionRestore::default();
        let part = SessionRestorePart {
            cwd_from_child: Some(vec!["my.custom.Terminal".into()]),
            ..Default::default()
        };
        // `on`/`off` absent here; default keeps it off, but cwd-from-child still merges.
        sr.merge_with(&part);
        assert!(sr.reads_cwd_from_child("my.custom.Terminal"));
        // Defaults are preserved.
        assert!(sr.reads_cwd_from_child("org.kde.konsole"));
    }
}

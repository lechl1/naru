//! Serializable schema for the persisted session state.
//!
//! The on-disk format is JSON. KDL would match the user-facing config format, but the
//! `knuffel` crate is decode-only — for an internal state file written tens of times per
//! session, the round-trip burden of hand-rolling a KDL serializer outweighs the
//! consistency benefit. JSON via `serde_json` keeps the writer trivial and the file
//! easy to inspect with `jq` when something goes wrong.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Bump this when the on-disk schema changes incompatibly. `load()` treats a mismatch as
/// a missing state file rather than an error, so an upgrade simply discards the prior
/// session — better than crashing the compositor on startup.
pub const SCHEMA_VERSION: u32 = 1;

/// Top-level on-disk session state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionState {
    /// Schema version. See [`SCHEMA_VERSION`].
    pub version: u32,
    /// Saved windows, in the order they should be respawned at restore time.
    ///
    /// Multi-instance disambiguation uses this order: when several saved entries share
    /// an `app_id`, the i-th window of that app to map after restart is matched to the
    /// i-th saved entry. Captured `cwd` is informational; matching does not consult it
    /// (avoids surprising mismatches if the user’s shell `cd`’d after window creation).
    pub windows: Vec<WindowEntry>,
}

impl SessionState {
    pub fn empty() -> Self {
        Self {
            version: SCHEMA_VERSION,
            windows: Vec::new(),
        }
    }
}

impl Default for SessionState {
    fn default() -> Self {
        Self::empty()
    }
}

/// One persisted window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowEntry {
    /// xdg `app_id` reported by the client. Used both as the dispatch key for
    /// `launch-command` lookup and as the post-respawn matching key.
    pub app_id: String,

    /// Last-known window title. Captured for diagnostics and human-readability of the
    /// state file — never used for matching, since titles are unstable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,

    /// Working directory captured from `/proc/<client-pid>/cwd` at map time.
    ///
    /// `None` means cwd capture failed (sandboxed Flatpak/snap client whose `/proc`
    /// view is its own root, dead PID, EACCES). At restore time this leaves the `%s`
    /// placeholder in the launch-command unfilled — the placeholder is dropped from
    /// the argv rather than substituted with a sentinel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,

    /// Flatpak application id (e.g. `"com.brave.Browser"`) when the client is a flatpak
    /// app, read from its process at save time. `None` for non-flatpak clients.
    ///
    /// This is what makes flatpak restore generic: rather than hardcoding a launch
    /// command per browser, the saved entry remembers it should be relaunched via
    /// `flatpak run <flatpak_id>`. The toplevel `app_id` (a WM class like
    /// `brave-browser`) is not the flatpak id, so it can't be used directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flatpak_id: Option<String>,

    /// Absolute path to the client's executable (`/proc/<pid>/exe`) for non-flatpak
    /// clients, captured at save time. `None` when unreadable or for flatpak apps
    /// (whose exe lives inside the sandbox and isn't launchable from the host).
    ///
    /// This generalises native-app restore: the window is relaunched by exec'ing this
    /// binary in the saved `cwd`, so e.g. a terminal reopens in the right directory
    /// without any app-specific `--workdir`-style launch command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec: Option<String>,

    /// Fully-resolved relaunch argv captured from the matching `.desktop` file's
    /// `Exec` line (matched by `StartupWMClass == app_id`) when the window is a
    /// Chromium site-specific-browser / PWA — e.g. an installed "web app".
    ///
    /// PWAs are owned by the browser process, so `flatpak_id`/`exec` would reopen the
    /// whole browser instead of the app; this carries the exact per-app launch command
    /// (`flatpak run … --app-id=…`) and takes precedence over them at restore time.
    /// `None` for every non-PWA window. See [`crate::session::desktop`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,

    /// The tmux session this terminal window was attached to, captured at save
    /// time. `None` for non-terminal windows and terminals not running tmux.
    ///
    /// tmux sessions live in the tmux *server*, independent of the terminal, so
    /// this is captured terminal-agnostically (walk the terminal's process tree
    /// for the tmux client, then ask the server which session it's on). On
    /// restore the captured terminal is relaunched reattaching to it via
    /// `tmux new-session -A -s <name>` (attach-or-create). See [`crate::session::tmux`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmux_session: Option<TmuxAttach>,

    /// The Claude Code session this terminal window was running, captured at save
    /// time as the session id (the transcript's `<uuid>`). `None` for non-terminal
    /// windows, terminals not running `claude`, and `claude` running inside tmux
    /// (restored by tmux reattach instead).
    ///
    /// On restore the captured terminal is relaunched running
    /// `claude --resume <id>` so the conversation continues. See
    /// [`crate::session::claude`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_session: Option<String>,

    /// Connector name of the output the window was on, e.g. `"DP-1"`. `None` for the
    /// "any output" wildcard at restore time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,

    /// Workspace identity. Stored as either a name (preferred when the user has named
    /// workspaces) or a per-output index.
    pub workspace: WorkspaceRef,

    /// How the window sat within its workspace.
    pub placement: Placement,
}

/// A tmux session a terminal was attached to, plus how to reach its server.
///
/// `socket_args` are the tmux global flags that select a non-default socket
/// (`["-L", "<label>"]` or `["-S", "<path>"]`); empty for the default socket.
/// They're captured from the running client so restore reattaches on the same
/// server, and replayed in front of the `new-session` command.
///
/// Serializes compactly — as a bare session-name string on the default socket —
/// and deserializes from either the string or the full object form, so files
/// written before custom-socket support still load.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum TmuxAttach {
    /// Default socket: just the session name.
    NameOnly(String),
    /// Non-default socket: session name plus the socket-selecting flags.
    WithSocket {
        session: String,
        socket_args: Vec<String>,
    },
}

impl TmuxAttach {
    /// Build from a session name and (possibly empty) socket-selecting flags.
    pub fn new(session: String, socket_args: Vec<String>) -> Self {
        if socket_args.is_empty() {
            Self::NameOnly(session)
        } else {
            Self::WithSocket {
                session,
                socket_args,
            }
        }
    }

    pub fn session(&self) -> &str {
        match self {
            Self::NameOnly(s) => s,
            Self::WithSocket { session, .. } => session,
        }
    }

    pub fn socket_args(&self) -> &[String] {
        match self {
            Self::NameOnly(_) => &[],
            Self::WithSocket { socket_args, .. } => socket_args,
        }
    }
}

impl<'de> Deserialize<'de> for TmuxAttach {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Accept either a bare string (older default-socket form) or the object
        // form `{ session, socket_args }`.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            NameOnly(String),
            WithSocket {
                session: String,
                #[serde(default)]
                socket_args: Vec<String>,
            },
        }
        Ok(match Repr::deserialize(deserializer)? {
            Repr::NameOnly(session) => TmuxAttach::new(session, Vec::new()),
            Repr::WithSocket {
                session,
                socket_args,
            } => TmuxAttach::new(session, socket_args),
        })
    }
}

/// Workspace identity. Names are preferred since they survive workspace re-ordering;
/// indices are a fallback for unnamed workspaces.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "snake_case")]
pub enum WorkspaceRef {
    Name { name: String },
    Index { index: usize },
}

/// Which fixed-side panel a window sat in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelSide {
    Left,
    Right,
}

/// How a window was positioned within its workspace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Placement {
    /// Tiled into the scrolling carousel at a specific column + tile slot.
    Tiled {
        /// 0-based column index from the left of the scrolling row.
        column_index: usize,
        /// 0-based index of the window within its column (relevant when columns
        /// hold multiple stacked windows).
        tile_index: usize,
        /// Window width in logical pixels at save time. `0.0` (the default for
        /// state files written before width capture) means "let the layout pick".
        #[serde(default)]
        width: f64,
        /// Window height in logical pixels at save time. `0.0` means "default".
        #[serde(default)]
        height: f64,
        #[serde(default)]
        is_fullscreen: bool,
        #[serde(default)]
        is_maximized: bool,
    },
    /// Parked in one of the fixed-side panels (left/right strips). These are not
    /// part of the scrolling carousel; a window is steered back into its panel on
    /// restore via the same `open-in-fixed-side` path window-rules use.
    SidePanel {
        /// Which panel the window was in.
        side: PanelSide,
        /// 0-based column index within the strip (outer-to-inner ordering matches
        /// the strip's own column order).
        column_index: usize,
        /// 0-based index of the window within its strip column.
        tile_index: usize,
        /// Window width in logical pixels at save time. `0.0` means "default".
        #[serde(default)]
        width: f64,
        /// Window height in logical pixels at save time. `0.0` means "default".
        #[serde(default)]
        height: f64,
    },
    /// Free-floating window with explicit logical-pixel geometry.
    Floating {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        #[serde(default)]
        is_fullscreen: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_round_trips() {
        let s = SessionState::empty();
        let json = serde_json::to_string(&s).unwrap();
        let back: SessionState = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
        assert_eq!(s.version, SCHEMA_VERSION);
    }

    #[test]
    fn one_window_round_trips() {
        let s = SessionState {
            version: SCHEMA_VERSION,
            windows: vec![WindowEntry {
                app_id: "org.kde.konsole".into(),
                title: Some("~/work".into()),
                cwd: Some(PathBuf::from("/home/leo/work")),
                flatpak_id: None,
                exec: None,
                command: None,
                tmux_session: None,
                claude_session: None,
                output: Some("DP-1".into()),
                workspace: WorkspaceRef::Name {
                    name: "code".into(),
                },
                placement: Placement::Tiled {
                    column_index: 2,
                    tile_index: 0,
                    width: 800.0,
                    height: 600.0,
                    is_fullscreen: false,
                    is_maximized: false,
                },
            }],
        };
        let json = serde_json::to_string_pretty(&s).unwrap();
        // Spot-check a couple of stable fields appear in the JSON.
        assert!(json.contains("\"app_id\""));
        assert!(json.contains("\"org.kde.konsole\""));
        assert!(json.contains("\"by\": \"name\""));
        assert!(json.contains("\"kind\": \"tiled\""));

        let back: SessionState = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn floating_round_trips() {
        let entry = WindowEntry {
            app_id: "firefox".into(),
            title: None,
            cwd: None,
            flatpak_id: None,
            exec: None,
            command: None,
            tmux_session: None,
            claude_session: None,
            output: None,
            workspace: WorkspaceRef::Index { index: 0 },
            placement: Placement::Floating {
                x: 100.0,
                y: 50.0,
                width: 800.0,
                height: 600.0,
                is_fullscreen: false,
            },
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: WindowEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
        // None fields should be omitted, not serialized as null.
        assert!(!json.contains("\"cwd\""));
        assert!(!json.contains("\"title\""));
    }

    #[test]
    fn side_panel_round_trips() {
        let entry = WindowEntry {
            app_id: "org.kde.konsole".into(),
            title: None,
            cwd: None,
            flatpak_id: None,
            exec: None,
            command: None,
            tmux_session: None,
            claude_session: None,
            output: None,
            workspace: WorkspaceRef::Index { index: 0 },
            placement: Placement::SidePanel {
                side: PanelSide::Right,
                column_index: 0,
                tile_index: 1,
                width: 400.0,
                height: 720.0,
            },
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"kind\":\"side_panel\""));
        assert!(json.contains("\"side\":\"right\""));
        let back: WindowEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn pwa_command_round_trips_and_omits_when_none() {
        let mut entry = WindowEntry {
            app_id: "crx_abc".into(),
            title: None,
            cwd: None,
            flatpak_id: Some("net.imput.helium".into()),
            exec: None,
            command: Some(vec![
                "flatpak".into(),
                "run".into(),
                "net.imput.helium".into(),
                "--app-id=abc".into(),
            ]),
            tmux_session: None,
            claude_session: None,
            output: None,
            workspace: WorkspaceRef::Index { index: 0 },
            placement: Placement::Floating {
                x: 0.0,
                y: 0.0,
                width: 800.0,
                height: 600.0,
                is_fullscreen: false,
            },
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"command\""));
        assert_eq!(serde_json::from_str::<WindowEntry>(&json).unwrap(), entry);

        // None is omitted, not serialized as null.
        entry.command = None;
        let json = serde_json::to_string(&entry).unwrap();
        assert!(!json.contains("\"command\""));
    }

    #[test]
    fn tmux_session_round_trips_and_omits_when_none() {
        let mut entry = WindowEntry {
            app_id: "org.kde.konsole".into(),
            title: None,
            cwd: None,
            flatpak_id: None,
            exec: Some("/usr/bin/konsole".into()),
            command: None,
            tmux_session: Some(TmuxAttach::NameOnly("work".into())),
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
        };
        // Default socket serializes as a bare session-name string.
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"tmux_session\":\"work\""));
        assert_eq!(serde_json::from_str::<WindowEntry>(&json).unwrap(), entry);

        // A custom socket serializes as the object form and round-trips.
        entry.tmux_session = Some(TmuxAttach::new(
            "work".into(),
            vec!["-L".into(), "ws".into()],
        ));
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"socket_args\":[\"-L\",\"ws\"]"));
        assert_eq!(serde_json::from_str::<WindowEntry>(&json).unwrap(), entry);

        // Omitted (not null) when there's no tmux session.
        entry.tmux_session = None;
        let json = serde_json::to_string(&entry).unwrap();
        assert!(!json.contains("tmux_session"));
        assert_eq!(serde_json::from_str::<WindowEntry>(&json).unwrap().tmux_session, None);
    }

    #[test]
    fn tmux_attach_deserializes_legacy_string_and_object() {
        // A bare string (pre-custom-socket files) loads as a default-socket attach.
        let from_str: TmuxAttach = serde_json::from_str("\"work\"").unwrap();
        assert_eq!(from_str, TmuxAttach::NameOnly("work".into()));
        assert!(from_str.socket_args().is_empty());

        // The object form carries the socket flags.
        let from_obj: TmuxAttach =
            serde_json::from_str(r#"{"session":"work","socket_args":["-S","/tmp/s"]}"#).unwrap();
        assert_eq!(from_obj.session(), "work");
        assert_eq!(from_obj.socket_args(), ["-S", "/tmp/s"]);

        // An object without socket_args collapses to the default socket.
        let no_args: TmuxAttach = serde_json::from_str(r#"{"session":"work"}"#).unwrap();
        assert_eq!(no_args, TmuxAttach::NameOnly("work".into()));
    }

    #[test]
    fn claude_session_round_trips_and_omits_when_none() {
        let mut entry = WindowEntry {
            app_id: "org.kde.konsole".into(),
            title: None,
            cwd: Some(PathBuf::from("/ws/naru")),
            flatpak_id: None,
            exec: Some("/usr/bin/konsole".into()),
            command: None,
            tmux_session: None,
            claude_session: Some("943ab4f3-4d35-4ca7".into()),
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
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"claude_session\":\"943ab4f3-4d35-4ca7\""));
        assert_eq!(serde_json::from_str::<WindowEntry>(&json).unwrap(), entry);

        // Omitted (not null) when there's no Claude session.
        entry.claude_session = None;
        let json = serde_json::to_string(&entry).unwrap();
        assert!(!json.contains("claude_session"));
    }

    #[test]
    fn missing_optional_fields_default_on_load() {
        // Older format that didn't yet have `is_maximized` should still parse.
        let json = r#"{
            "version": 1,
            "windows": [{
                "app_id": "x",
                "workspace": {"by": "index", "index": 0},
                "placement": {"kind": "tiled", "column_index": 0, "tile_index": 0}
            }]
        }"#;
        let s: SessionState = serde_json::from_str(json).unwrap();
        assert_eq!(s.windows.len(), 1);
        let p = &s.windows[0].placement;
        match p {
            Placement::Tiled {
                is_fullscreen,
                is_maximized,
                width,
                height,
                ..
            } => {
                assert!(!is_fullscreen);
                assert!(!is_maximized);
                // Width/height absent in the old format default to 0.0.
                assert_eq!(*width, 0.0);
                assert_eq!(*height, 0.0);
            }
            _ => panic!("expected Tiled"),
        }
    }
}

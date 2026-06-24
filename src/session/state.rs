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

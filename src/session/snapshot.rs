//! Build a [`SessionState`] from the live compositor state.
//!
//! ## Traversal
//!
//! The walk reuses [`Layout::workspaces`] (which already produces
//! `(Option<&Monitor>, usize, &Workspace)` triples — every monitor, plus the
//! "no-outputs" stash for windows that exist while their output is missing) and
//! [`Workspace::tiles_with_ipc_layouts`] (which the IPC server uses for
//! `naru msg windows` and is the most stable per-tile-with-position API in the
//! codebase). Mapping the IPC's [`WindowLayout`] into our serializable [`Placement`]
//! keeps us off of layout-internal types.
//!
//! ## What's captured
//!
//! - **app_id**: required; windows without one are skipped (nothing to launch them by).
//! - **title**: not captured in this phase. The IPC's per-window title is informational,
//!   and adding the lookup here would require borrowing into XDG toplevel state which
//!   the snapshot's read-only path shouldn't do.
//! - **cwd**: read from the cached `Mapped::session_cwd` (captured at construction in
//!   Phase 2c.1; never re-read).
//! - **output**: connector name, e.g. `"DP-1"`, from `Monitor::output_name`. `None` for
//!   windows in the no-outputs stash.
//! - **workspace**: prefers the user-set name; falls back to the per-monitor index.
//! - **placement**: `Tiled` with 0-based column/tile indices when the window is in the
//!   scrolling layout; `Floating` with placeholder zero geometry otherwise. Real
//!   floating-window geometry capture lands in a follow-up sub-phase — the IPC's
//!   `WindowLayout` doesn't currently surface it, so we'd need a separate read from
//!   `FloatingSpace`. Saved entries with zero-sized floating geometry simply ignore
//!   that field at restore time and let the client size itself.

use std::path::PathBuf;

use crate::layout::workspace::Workspace as LayoutWorkspace;
use crate::layout::LayoutElement;
use crate::naru::Naru;
use crate::session::state::{
    Placement, SessionState, WindowEntry, WorkspaceRef, SCHEMA_VERSION,
};
use crate::window::Mapped;

pub fn build_from_naru(naru: &Naru) -> SessionState {
    let mut windows = Vec::new();

    for (mon, ws_idx, ws) in naru.layout.workspaces() {
        let output = mon.map(|m| m.output_name().clone());
        let workspace = workspace_ref_for(ws, ws_idx);

        for (tile, layout) in ws.tiles_with_ipc_layouts() {
            let mapped = tile.window();
            let app_id = match mapped.app_id() {
                Some(s) if !s.is_empty() => s,
                _ => continue,
            };

            let placement = placement_from_ipc_layout(&layout);

            windows.push(WindowEntry {
                app_id,
                title: None,
                cwd: mapped.session_cwd().map(PathBuf::from),
                output: output.clone(),
                workspace: workspace.clone(),
                placement,
            });
        }
    }

    SessionState {
        version: SCHEMA_VERSION,
        windows,
    }
}

fn workspace_ref_for(ws: &LayoutWorkspace<Mapped>, ws_idx: usize) -> WorkspaceRef {
    match ws.name() {
        Some(n) => WorkspaceRef::Name { name: n.clone() },
        None => WorkspaceRef::Index { index: ws_idx },
    }
}

fn placement_from_ipc_layout(layout: &naru_ipc::WindowLayout) -> Placement {
    if let Some((col, til)) = layout.pos_in_scrolling_layout {
        // IPC layout indices are 1-based to match user-facing actions; our
        // serialized form is 0-based for closer alignment with the underlying Vec
        // indices the restore path will consult. saturating_sub handles the
        // (theoretically impossible but cheap to defend against) zero-index case.
        Placement::Tiled {
            column_index: col.saturating_sub(1),
            tile_index: til.saturating_sub(1),
            is_fullscreen: false,
            is_maximized: false,
        }
    } else {
        // Floating window. Real geometry capture is deferred to a follow-up —
        // see module-level docs.
        Placement::Floating {
            x: 0.0,
            y: 0.0,
            width: 0.0,
            height: 0.0,
            is_fullscreen: false,
        }
    }
}

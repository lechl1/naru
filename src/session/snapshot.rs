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
//! - **cwd**: by default the cached `Mapped::session_cwd` (captured at construction —
//!   the directory the window was opened in). For app_ids whose launch-command has
//!   `cwd-from-child` set (terminals), [`resolve_cwd`] instead re-reads the foreground
//!   child shell's cwd fresh here at save time, so a later `cd` is persisted.
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

use naru_config::SessionRestore;

use crate::layout::fixed_strip::FixedSide;
use crate::layout::workspace::Workspace as LayoutWorkspace;
use crate::layout::LayoutElement;
use crate::naru::Naru;
use crate::session::state::{
    PanelSide, Placement, SessionState, TmuxAttach, WindowEntry, WorkspaceRef, SCHEMA_VERSION,
};
use crate::utils::with_toplevel_role;
use crate::window::Mapped;

pub fn build_from_naru(naru: &Naru) -> SessionState {
    let mut windows = Vec::new();

    let config = naru.config.borrow();
    let session_restore = &config.session_restore;

    // Index installed `.desktop` files once per snapshot so PWA windows can recover
    // their exact relaunch command (matched by StartupWMClass == app_id). One dir walk
    // per save; could be cached in `SessionManager` if save frequency makes it hot.
    let wm_class_index = crate::session::index_startup_wm_classes();

    // Fixed-side panels are owned per-output now, so enumerate them once per
    // monitor (keyed by output name) rather than once per workspace — otherwise
    // each panel window would be saved once for every workspace on the monitor.
    let mut panels_seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Per-socket tmux client-pid → session maps, queried from each server at
    // most once per save (lazily, only if a terminal window is encountered).
    // Reused to map each terminal window to the session it's attached to.
    let mut socket_maps = crate::session::tmux::SocketMaps::new();

    for (mon, ws_idx, ws) in naru.layout.workspaces() {
        let output = mon.map(|m| m.output_name().clone());
        let workspace = workspace_ref_for(ws, ws_idx);

        // Carousel + floating windows.
        for (tile, layout) in ws.tiles_with_ipc_layouts() {
            let mapped = tile.window();
            let app_id = match mapped.app_id() {
                Some(s) if !s.is_empty() => s,
                _ => continue,
            };

            let (w, h) = window_size(mapped);
            let portrait = mon.is_some_and(|m| {
                let s = m.view_size();
                s.h > s.w
            });
            let placement = placement_from_ipc_layout(&layout, w, persisted_height(portrait, h));
            let (flatpak_id, exec) = launch_info(mapped);
            let command = crate::session::ssb_launch_argv(
                &app_id,
                flatpak_id.as_deref(),
                exec.as_deref(),
                &wm_class_index,
            );
            let (cwd, tmux_session, claude_session) =
                resolve_terminal_state(mapped, &app_id, session_restore, &mut socket_maps);

            windows.push(WindowEntry {
                app_id: app_id.clone(),
                title: None,
                cwd,
                flatpak_id,
                exec,
                command,
                tmux_session,
                claude_session,
                output: output.clone(),
                workspace: workspace.clone(),
                placement,
            });
        }

        // Fixed-side panel windows — not covered by the carousel iterator.
        // Panels live on the monitor (shared across its workspaces), so emit
        // them once per output.
        let Some(mon) = mon else {
            continue;
        };
        if !panels_seen.insert(mon.output_name().clone()) {
            continue;
        }
        for (side, col_idx, tile_idx, tile) in mon.fixed_side_tiles() {
            let mapped = tile.window();
            let app_id = match mapped.app_id() {
                Some(s) if !s.is_empty() => s,
                _ => continue,
            };

            let (w, h) = window_size(mapped);
            let portrait = {
                let s = mon.view_size();
                s.h > s.w
            };
            let h = persisted_height(portrait, h);
            let (flatpak_id, exec) = launch_info(mapped);
            let command = crate::session::ssb_launch_argv(
                &app_id,
                flatpak_id.as_deref(),
                exec.as_deref(),
                &wm_class_index,
            );
            let (cwd, tmux_session, claude_session) =
                resolve_terminal_state(mapped, &app_id, session_restore, &mut socket_maps);
            windows.push(WindowEntry {
                app_id: app_id.clone(),
                title: None,
                cwd,
                flatpak_id,
                exec,
                command,
                tmux_session,
                claude_session,
                output: output.clone(),
                workspace: workspace.clone(),
                placement: Placement::SidePanel {
                    side: panel_side_for(side),
                    column_index: col_idx,
                    tile_index: tile_idx,
                    width: w,
                    height: h,
                },
            });
        }
    }

    SessionState {
        version: SCHEMA_VERSION,
        windows,
    }
}

/// Resolve the working directory and any tmux / Claude Code session for a window,
/// sharing a single Konsole D-Bus query across all three.
///
/// **cwd** — default is the cwd captured once at window-map time
/// (`Mapped::session_cwd`), i.e. the directory the window was *opened in*. For
/// terminals (the `cwd-from-child` set) the meaningful cwd lives in the
/// foreground child shell and changes as the user `cd`s, so it is re-read fresh
/// here at save time.
///
/// **tmux** — only terminals are considered; from the window's foreground pid
/// [`tmux::session_for_window`] walks the process tree for a tmux client and maps
/// it to its session (per-socket server queries cached in `socket_maps`).
///
/// **claude** — likewise only terminals; [`claude::session_for_window`] walks the
/// tree for a `claude` process and resolves its resumable session id. A `claude`
/// running inside tmux lives under the tmux server (not the terminal's tree), so
/// the two are naturally mutually exclusive here.
///
/// Konsole is single-process/multi-window: all its windows share one client PID,
/// so `/proc` can't yield per-window data. Its D-Bus exposes a per-session shell
/// pid (correlated to the window by title); we query it **once** and derive the
/// cwd (`/proc/<shell>/cwd`) and the tmux/claude foreground pid from that same pid.
fn resolve_terminal_state(
    mapped: &Mapped,
    app_id: &str,
    sr: &SessionRestore,
    socket_maps: &mut crate::session::tmux::SocketMaps,
) -> (Option<PathBuf>, Option<TmuxAttach>, Option<String>) {
    let pid = mapped.credentials().map(|c| c.pid);

    if app_id == "org.kde.konsole" {
        if let Some(pid) = pid {
            if let Some(title) = with_toplevel_role(mapped.toplevel(), |role| role.title.clone()) {
                if let Some(shell_pid) =
                    crate::session::konsole::shell_pid_for_window(pid, &title)
                {
                    let cwd = crate::session::read_cwd_for_pid(shell_pid)
                        .or_else(|| mapped.session_cwd().map(PathBuf::from));
                    let tmux = crate::session::tmux::session_for_window(shell_pid, socket_maps);
                    let claude = crate::session::claude::session_for_window(shell_pid);
                    return (cwd, tmux, claude);
                }
            }
        }
        // D-Bus lookup failed — fall through to the generic process-tree paths.
    }

    let is_terminal = sr.reads_cwd_from_child(app_id);
    (
        resolve_cwd_generic(mapped, app_id, sr),
        is_terminal
            .then(|| pid.and_then(|p| crate::session::tmux::session_for_window(p, socket_maps)))
            .flatten(),
        is_terminal
            .then(|| pid.and_then(crate::session::claude::session_for_window))
            .flatten(),
    )
}

/// Working directory for a non-Konsole window: the foreground child shell's cwd
/// for terminals (re-read fresh so a later `cd` is persisted), else the map-time
/// capture. Falls back to the map-time capture if the child descent yields nothing.
fn resolve_cwd_generic(mapped: &Mapped, app_id: &str, sr: &SessionRestore) -> Option<PathBuf> {
    if sr.reads_cwd_from_child(app_id) {
        if let Some(pid) = mapped.credentials().map(|c| c.pid) {
            if let Some(cwd) = crate::session::read_cwd_from_child(pid) {
                return Some(cwd);
            }
        }
    }
    mapped.session_cwd().map(PathBuf::from)
}

/// Capture how to relaunch a window generically: its flatpak id (if a flatpak app) and
/// otherwise its executable path. Returns `(flatpak_id, exec)`; for a flatpak app `exec`
/// is `None` (the exe is inside the sandbox), and for a native app `flatpak_id` is `None`.
fn launch_info(mapped: &Mapped) -> (Option<String>, Option<String>) {
    let Some(pid) = mapped.credentials().map(|c| c.pid) else {
        return (None, None);
    };
    if let Some(flatpak_id) = crate::session::read_flatpak_id_for_pid(pid) {
        return (Some(flatpak_id), None);
    }
    (None, crate::session::read_exec_for_pid(pid))
}

/// The window's current logical size, as `(width, height)` floats.
fn window_size(mapped: &Mapped) -> (f64, f64) {
    let size = mapped.size();
    (size.w as f64, size.h as f64)
}

/// The window height to persist. Intentionally *not* saved on a landscape
/// output: restored windows take the layout's automatic height, so a window
/// that is alone in its column fills the whole column and a multi-window column
/// splits evenly. Persisting a stale fixed height there is what left a lone
/// restored window stuck at, e.g., 50% of the column. On a *portrait* output the
/// vertical split within a column is the meaningful axis, so the real height is
/// kept. `0.0` is the "use the default/automatic height" sentinel the restore
/// path already understands.
fn persisted_height(portrait: bool, h: f64) -> f64 {
    if portrait {
        h
    } else {
        0.0
    }
}

fn panel_side_for(side: FixedSide) -> PanelSide {
    match side {
        FixedSide::Left => PanelSide::Left,
        FixedSide::Right => PanelSide::Right,
    }
}

fn workspace_ref_for(ws: &LayoutWorkspace<Mapped>, ws_idx: usize) -> WorkspaceRef {
    match ws.name() {
        Some(n) => WorkspaceRef::Name { name: n.clone() },
        None => WorkspaceRef::Index { index: ws_idx },
    }
}

fn placement_from_ipc_layout(layout: &naru_ipc::WindowLayout, width: f64, height: f64) -> Placement {
    if let Some((col, til)) = layout.pos_in_scrolling_layout {
        // IPC layout indices are 1-based to match user-facing actions; our
        // serialized form is 0-based for closer alignment with the underlying Vec
        // indices the restore path will consult. saturating_sub handles the
        // (theoretically impossible but cheap to defend against) zero-index case.
        Placement::Tiled {
            column_index: col.saturating_sub(1),
            tile_index: til.saturating_sub(1),
            width,
            height,
            is_fullscreen: false,
            is_maximized: false,
        }
    } else {
        // Floating window. Logical-pixel position capture (x/y) is still deferred
        // — the IPC WindowLayout doesn't surface it — but width/height are real.
        Placement::Floating {
            x: 0.0,
            y: 0.0,
            width,
            height,
            is_fullscreen: false,
        }
    }
}

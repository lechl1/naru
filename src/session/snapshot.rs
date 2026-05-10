//! Build a [`SessionState`] from the live compositor state.
//!
//! ## What this module owns
//!
//! Walking the active layout (monitors → workspaces → columns → tiles → windows) and
//! producing a [`SessionState`] that captures the position, output, workspace, and cwd
//! of every mapped window, suitable for round-tripping through
//! [`crate::session::storage::save_atomic`].
//!
//! ## Why this is its own module
//!
//! The translation from "live `Layout<Mapped>`" to "serializable schema" is the spot
//! where the layout's internal data structures meet the persisted format. Keeping it
//! isolated means the serde types in [`crate::session::state`] can stay
//! framework-agnostic, and the ~10 layout-mutation hook sites (Phase 2c) only depend on
//! the small public surface of this module rather than reaching into layout internals
//! themselves.
//!
//! ## Phase 2b status
//!
//! This file currently stubs `build_from_naru` to return an empty state. The real
//! traversal lands in Phase 2c alongside the `Mapped::session_cwd` field that captures
//! cwd at first map — those changes need to land together because the snapshot's `cwd`
//! field reads from `Mapped::session_cwd`, and adding both at once avoids a useless
//! intermediate where one half exists without the other.

use crate::naru::Naru;
use crate::session::state::SessionState;

/// Build a [`SessionState`] snapshot from the live compositor state.
///
/// Walks every mapped window in `naru.layout`, capturing app_id, output connector
/// name, workspace identity (preferring named workspaces over per-output indices),
/// column/tile placement, and the cwd previously captured on `Mapped::session_cwd`.
///
/// Currently a stub returning [`SessionState::empty`]; the real traversal arrives in
/// Phase 2c.
pub fn build_from_naru(_naru: &Naru) -> SessionState {
    // TODO(Phase 2c): walk naru.layout.windows() and translate each (Monitor, Mapped)
    // pair into a WindowEntry. Skeleton:
    //
    // let mut windows = Vec::new();
    // for (mon, mapped) in naru.layout.windows() {
    //     let app_id = match mapped.app_id() {
    //         Some(id) if !id.is_empty() => id,
    //         _ => continue, // skip windows that haven't set an app_id
    //     };
    //     let output = mon.map(|m| m.output_name().clone());
    //     let workspace = locate_workspace(naru, mapped);
    //     let placement = locate_placement(naru, mapped);
    //     windows.push(WindowEntry { app_id, title, cwd, output, workspace, placement });
    // }
    // SessionState { version: SCHEMA_VERSION, windows }

    SessionState::empty()
}

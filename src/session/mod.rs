//! Session restore: persist and restore the set of open windows + their layout
//! positions across compositor restarts.
//!
//! ## Phases (this file is the entry point for all of them)
//!
//! - **Phase 2a** (this commit): schema + JSON serde + atomic-write storage + cwd reader.
//! - **Phase 2b**: live snapshot building (iterate the active layout, gather positions).
//! - **Phase 2c**: debounced save + event hooks at window/layout mutation sites.
//! - **Phase 3**: respawn-on-startup + steer-on-first-map placement.
//! - **Phase 4**: feature gate + integration tests.
//!
//! ## Key constraint
//!
//! Wayland has no compositor-level protocol for telling an already-running client
//! "open at position X". Restore is implemented as **respawn-and-place-on-first-map**:
//! at startup the saved entries are launched via per-app-id command templates, and
//! as each new window maps it is steered into its saved slot. Existing already-running
//! clients can NOT be retroactively snapped into saved positions.

pub mod cwd;
pub mod snapshot;
pub mod state;
pub mod storage;

pub use cwd::{cwd_for_surface, read_cwd_for_pid};
pub use snapshot::build_from_naru;
pub use state::{Placement, SessionState, WindowEntry, WorkspaceRef, SCHEMA_VERSION};
pub use storage::{default_state_path, load, save_atomic};

//! Read a process's current working directory via `/proc/<pid>/cwd`.
//!
//! The `/proc/<pid>/cwd` symlink is the standard Linux mechanism for inspecting another
//! process's cwd; it requires that the inspecting process either own the target or have
//! `CAP_SYS_PTRACE`. The compositor runs as the same user as its clients, so for native
//! Wayland clients this works.
//!
//! Returns `None` for the cases we explicitly want to tolerate at runtime:
//!
//! - **Process exited** between window-map and capture (`ENOENT`).
//! - **Sandboxed client** (Flatpak / snap) whose `/proc` view is its own pid namespace
//!   root, making the cwd target a path that's meaningless on the host. The symlink
//!   still resolves to *something*, but pointing the launch-command at a non-existent
//!   host path would be worse than dropping the `%s` slot, so callers should treat any
//!   non-`/`-prefixed or non-existent target as an opt-out.
//! - **Permission denied** (`EACCES`) — shouldn't happen for same-user clients but is
//!   accepted gracefully.
//!
//! Wiring to a Wayland client (extracting the pid via `SO_PEERCRED` on the client's
//! socket) lives in Phase 2b — this module only owns the pid → path part.

use std::fs;
use std::path::PathBuf;

use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

use crate::utils::get_credentials_for_surface;

/// Read the working directory of process `pid`.
///
/// Returns `None` on any error or if the resolved path is not absolute. Callers should
/// substitute the returned path into the `%s` slot of a launch-command and drop the slot
/// entirely if `None` is returned.
pub fn read_cwd_for_pid(pid: i32) -> Option<PathBuf> {
    if pid <= 0 {
        return None;
    }

    let link = format!("/proc/{pid}/cwd");
    let target = fs::read_link(&link).ok()?;

    // Must be absolute. /proc/<pid>/cwd should always resolve to an absolute path on
    // Linux, so a relative target indicates something has gone very wrong (or we're on
    // a kernel that rewrote it). Drop it rather than risk a chdir-relative spawn.
    if !target.is_absolute() {
        return None;
    }

    // Optional sanity: don't trust paths that don't currently exist on the host. This
    // catches the "Flatpak sandbox saw /home/user but the host doesn't have that path"
    // case for self-built Flatpak apps. Existence-checks are racy by definition, but
    // wrong-cwd is worse than no-cwd here, so the race tolerance is acceptable.
    if !target.exists() {
        return None;
    }

    Some(target)
}

/// Read the working directory of the client backing a Wayland surface.
///
/// Composes [`get_credentials_for_surface`] (which exposes the connecting client's
/// `SO_PEERCRED`-derived PID) with [`read_cwd_for_pid`]. Returns `None` if either step
/// fails, with the same tolerant semantics as `read_cwd_for_pid` (sandboxed clients,
/// dead PIDs, paths not present on the host).
///
/// Intended capture point: the first `xdg_toplevel.commit` after a window maps. By that
/// point the client process has been alive long enough to have its cwd set; capturing
/// later (e.g. on focus) risks the user's shell having `cd`'d, which would be
/// unexpected for "restore my window".
pub fn cwd_for_surface(surface: &WlSurface) -> Option<PathBuf> {
    let creds = get_credentials_for_surface(surface)?;
    read_cwd_for_pid(creds.pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negative_pid_returns_none() {
        assert!(read_cwd_for_pid(0).is_none());
        assert!(read_cwd_for_pid(-1).is_none());
    }

    #[test]
    fn missing_pid_returns_none() {
        // PID 1 (init/systemd) exists; pick something that almost certainly doesn't.
        // i32::MAX is well above kernel.pid_max.
        assert!(read_cwd_for_pid(i32::MAX).is_none());
    }

    #[test]
    fn self_pid_returns_some_absolute() {
        let me = std::process::id();
        // u32 -> i32: process IDs always fit on Linux (kernel.pid_max <= 2^22).
        let pid = i32::try_from(me).unwrap();
        let cwd = read_cwd_for_pid(pid).expect("self cwd should be readable");
        assert!(cwd.is_absolute());
    }
}

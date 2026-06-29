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

/// Maximum depth to descend when walking a client's process tree looking for the
/// foreground child whose cwd is meaningful. Terminals are usually one level deep
/// (konsole → shell); a small cap keeps a pathological `fork()` chain bounded.
const MAX_CHILD_DEPTH: usize = 8;

/// Read the working directory of a window's foreground **child** process.
///
/// Terminal emulators keep their own process cwd at the launch directory; the shell
/// running inside the window is a child process, and *its* cwd is the directory the
/// user navigated to. This descends the process tree starting at `pid` — following the
/// most recently spawned child at each level (the closest approximation to "foreground"
/// available without the pty's foreground process group) — and returns the cwd of the
/// deepest descendant whose cwd is readable and valid per [`read_cwd_for_pid`].
///
/// Falls back to walking back up the chain (and finally `pid` itself) if a deeper
/// descendant's cwd can't be read, so a terminal with no live shell still yields
/// *some* directory rather than `None`.
pub fn read_cwd_from_child(pid: i32) -> Option<PathBuf> {
    let chain = descendant_chain(pid);
    // Deepest-first: prefer the shell/editor over the terminal wrapper, but tolerate a
    // dead leaf by falling back up the chain.
    chain.iter().rev().find_map(|&p| read_cwd_for_pid(p))
}

/// Build the descent chain `[pid, child, grandchild, ...]`, following the last-listed
/// (most recently spawned) child at each level up to [`MAX_CHILD_DEPTH`].
pub(crate) fn descendant_chain(pid: i32) -> Vec<i32> {
    let mut chain = vec![pid];
    let mut current = pid;
    for _ in 0..MAX_CHILD_DEPTH {
        match last_child_of(current) {
            Some(child) => {
                chain.push(child);
                current = child;
            }
            None => break,
        }
    }
    chain
}

/// Read the last entry of `/proc/<pid>/task/<pid>/children` (the kernel's per-task
/// children list, space-separated pids). Returns `None` if the file is absent
/// (kernel without `CONFIG_PROC_CHILDREN`), unreadable, or empty.
fn last_child_of(pid: i32) -> Option<i32> {
    let path = format!("/proc/{pid}/task/{pid}/children");
    let contents = fs::read_to_string(&path).ok()?;
    contents.split_whitespace().last()?.parse().ok()
}

/// Read the flatpak application id of process `pid`, if it is a flatpak app.
///
/// Flatpak apps carry a `/.flatpak-info` at their sandbox root (`/proc/<pid>/root`)
/// whose `[Application] name=` is the flatpak id (e.g. `com.brave.Browser`). This is
/// the generic way to learn how to relaunch a flatpak window — via `flatpak run <id>`
/// — without hardcoding a table of apps. Returns `None` for non-flatpak processes or
/// when the file is unreadable.
pub fn read_flatpak_id_for_pid(pid: i32) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    let info = fs::read_to_string(format!("/proc/{pid}/root/.flatpak-info")).ok()?;
    let mut in_application = false;
    for line in info.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_application = line == "[Application]";
            continue;
        }
        if in_application {
            if let Some(name) = line.strip_prefix("name=") {
                let name = name.trim();
                if !name.is_empty() {
                    return Some(name.to_owned());
                }
            }
        }
    }
    None
}

/// Read the absolute executable path of process `pid` via `/proc/<pid>/exe`.
///
/// Used to relaunch a native (non-flatpak) window generically — exec this binary in
/// the saved cwd instead of hardcoding a per-app command. Returns `None` when the
/// target is unreadable, not absolute, or doesn't exist on the host (the latter
/// rejects sandboxed clients whose exe lives inside their own mount namespace —
/// those are relaunched via [`read_flatpak_id_for_pid`] instead).
pub fn read_exec_for_pid(pid: i32) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    let target = fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    if !target.is_absolute() || !target.exists() {
        return None;
    }
    Some(target.to_string_lossy().into_owned())
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

    #[test]
    fn child_descent_leafless_falls_back_to_self() {
        // The test process may or may not have live children (the test harness can
        // spawn threads but not necessarily child *processes*). Either way, descending
        // must yield an absolute path — at worst our own cwd via the chain's root.
        let pid = i32::try_from(std::process::id()).unwrap();
        let cwd = read_cwd_from_child(pid).expect("descent should fall back to self cwd");
        assert!(cwd.is_absolute());
    }

    #[test]
    fn child_descent_finds_real_child_cwd() {
        use std::path::Path;
        use std::process::{Command, Stdio};

        // Spawn a child that chdir's to /tmp and blocks, so its cwd differs from ours.
        // /tmp is guaranteed absolute and present on every Linux host the tests run on.
        let mut child = Command::new("sh")
            .args(["-c", "cd /tmp && exec sleep 30"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleeper");

        // Poll briefly for the child's cwd to settle to /tmp (the `cd` + `exec` race).
        let child_pid = i32::try_from(child.id()).unwrap();
        let mut found = None;
        for _ in 0..50 {
            if let Some(c) = read_cwd_for_pid(child_pid) {
                if c == Path::new("/tmp") {
                    found = Some(c);
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // Descend directly from the child to remove ambiguity about which of the test
        // runner's many children is "last" (other parallel tests may spawn processes).
        // A leaf process descends to itself, exercising read_cwd_from_child end-to-end.
        let descended = read_cwd_from_child(child_pid);
        let _ = child.kill();
        let _ = child.wait();

        // The child's cwd is /tmp, and descending from it must surface that directory.
        assert_eq!(found.as_deref(), Some(Path::new("/tmp")));
        assert_eq!(descended.as_deref(), Some(Path::new("/tmp")));
    }
}

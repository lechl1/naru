//! Terminal-agnostic tmux session capture for session-restore.
//!
//! tmux sessions live in the tmux *server*, not the terminal that displays
//! them, so they can be detected and restored without knowing or hardcoding
//! which terminal is running. For a terminal window we walk its process tree
//! for the tmux *client* (the panes live under the separate server process, so
//! the only tmux in a terminal's own descendant tree is the client driving it),
//! read the client's command line to learn which socket it uses (default, or a
//! custom `-L`/`-S` socket), then ask *that* server which session the client is
//! on (`tmux list-clients`, matched by the client's pid).
//!
//! On restore the captured terminal is relaunched reattaching to the session via
//! `tmux [<socket>] new-session -A -s <name>` (attach-or-create), so it works
//! whether the server survived the compositor restart (reattach to the live
//! session) or not, e.g. after a reboot (open a fresh session of that name).
//! Pair with tmux-resurrect/continuum if you want the session *contents* back
//! after a reboot.

use std::collections::HashMap;
use std::fs;
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use super::cwd::descendant_chain;
use super::state::TmuxAttach;

/// Per-socket cache of `client_pid → session_name` maps, so each distinct tmux
/// socket is queried at most once per save. Keyed by the socket-selecting args
/// (empty key = default socket).
pub type SocketMaps = HashMap<Vec<String>, HashMap<i32, String>>;

/// The argv that reattaches to (or creates) `attach`'s session on its socket.
///
/// `tmux [<socket>] new-session -A -s <name>` — `-A` attaches when the session
/// already exists and creates it otherwise, robust across both a compositor
/// restart (reattach) and a reboot (fresh session of that name).
pub fn reattach_command(attach: &TmuxAttach) -> Vec<String> {
    let mut argv = vec!["tmux".to_owned()];
    argv.extend(attach.socket_args().iter().cloned());
    argv.extend(["new-session", "-A", "-s"].map(str::to_owned));
    argv.push(attach.session().to_owned());
    argv
}

/// The tmux session a terminal whose process tree starts at `fg_pid` is attached
/// to, if any, together with the socket needed to reach it. `socket_maps` caches
/// each socket's client list across windows in one save.
pub fn session_for_window(fg_pid: i32, socket_maps: &mut SocketMaps) -> Option<TmuxAttach> {
    let chain = descendant_chain(fg_pid);
    let cmdlines: Vec<(i32, Vec<String>)> =
        chain.iter().map(|&p| (p, read_cmdline(p))).collect();
    let (client_pid, socket_args) = find_client(&cmdlines)?;

    let map = socket_maps
        .entry(socket_args.clone())
        .or_insert_with(|| client_pid_sessions(&socket_args));
    let session = map.get(&client_pid)?.clone();
    Some(TmuxAttach::new(session, socket_args))
}

/// Find the deepest tmux *client* in a process chain (root → leaf), returning
/// its pid and the socket-selecting args from its command line. Split from the
/// `/proc` walk so it's unit-testable.
fn find_client(cmdlines: &[(i32, Vec<String>)]) -> Option<(i32, Vec<String>)> {
    cmdlines
        .iter()
        .rev()
        .find(|(_, argv)| is_tmux(argv))
        .map(|(pid, argv)| (*pid, socket_args_from_argv(argv)))
}

/// Whether `argv` invokes tmux (its program's basename is `tmux`).
fn is_tmux(argv: &[String]) -> bool {
    argv.first()
        .and_then(|a| a.rsplit('/').next())
        .is_some_and(|base| base == "tmux")
}

/// Extract the socket-selecting global flags (`-L <label>` / `-S <path>`) from a
/// tmux command line, in both the spaced (`-L name`) and attached (`-Lname`)
/// forms. Returns the first one found, or empty for the default socket.
fn socket_args_from_argv(argv: &[String]) -> Vec<String> {
    let mut it = argv.iter().skip(1);
    while let Some(a) = it.next() {
        if a == "-L" || a == "-S" {
            if let Some(v) = it.next() {
                return vec![a.clone(), v.clone()];
            }
        } else if let Some(v) = a.strip_prefix("-L").filter(|v| !v.is_empty()) {
            return vec!["-L".to_owned(), v.to_owned()];
        } else if let Some(v) = a.strip_prefix("-S").filter(|v| !v.is_empty()) {
            return vec!["-S".to_owned(), v.to_owned()];
        }
    }
    Vec::new()
}

/// Read `/proc/<pid>/cmdline` as an argv vector (NUL-separated). Empty on failure.
fn read_cmdline(pid: i32) -> Vec<String> {
    let Ok(bytes) = fs::read(format!("/proc/{pid}/cmdline")) else {
        return Vec::new();
    };
    bytes
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

/// Query a tmux server for every attached client's pid and the session it is on.
///
/// Runs `tmux [<socket>] list-clients -F '#{client_pid} #{session_name}'` on a
/// short-lived thread, bounded so a hung server can't stall a save. Returns an
/// empty map when tmux isn't installed, no server is on that socket, or the
/// query times out.
fn client_pid_sessions(socket_args: &[String]) -> HashMap<i32, String> {
    let socket_args = socket_args.to_vec();
    let (tx, rx) = mpsc::channel();
    if std::thread::Builder::new()
        .name("tmux-list-clients".to_owned())
        .spawn(move || {
            let _ = tx.send(run_list_clients(&socket_args));
        })
        .is_err()
    {
        return HashMap::new();
    }
    rx.recv_timeout(Duration::from_millis(200))
        .ok()
        .flatten()
        .map(|out| parse_list_clients(&out))
        .unwrap_or_default()
}

/// Run `tmux [<socket>] list-clients …` and return its stdout, or `None` on any
/// failure.
fn run_list_clients(socket_args: &[String]) -> Option<String> {
    let output = Command::new("tmux")
        .args(socket_args)
        .args(["list-clients", "-F", "#{client_pid} #{session_name}"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// Parse `list-clients -F '#{client_pid} #{session_name}'` output into a
/// client-pid → session-name map. One line per client: `<pid> <session>`; the
/// pid is the first whitespace-delimited field and the remainder is the name
/// (which may itself contain spaces).
fn parse_list_clients(output: &str) -> HashMap<i32, String> {
    output
        .lines()
        .filter_map(|line| {
            let (pid, name) = line.trim().split_once(char::is_whitespace)?;
            let pid: i32 = pid.parse().ok()?;
            let name = name.trim();
            (!name.is_empty()).then(|| (pid, name.to_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn reattach_default_socket() {
        assert_eq!(
            reattach_command(&TmuxAttach::new("work".into(), vec![])),
            vec!["tmux", "new-session", "-A", "-s", "work"]
        );
    }

    #[test]
    fn reattach_custom_socket_prepends_flags() {
        let attach = TmuxAttach::new("work".into(), vec!["-L".into(), "ws".into()]);
        assert_eq!(
            reattach_command(&attach),
            vec!["tmux", "-L", "ws", "new-session", "-A", "-s", "work"]
        );
    }

    #[test]
    fn detects_tmux_by_basename() {
        assert!(is_tmux(&argv(&["/usr/bin/tmux", "attach"])));
        assert!(is_tmux(&argv(&["tmux"])));
        assert!(!is_tmux(&argv(&["bash"])));
        assert!(!is_tmux(&argv(&[])));
    }

    #[test]
    fn socket_args_default_and_custom() {
        assert!(socket_args_from_argv(&argv(&["tmux", "attach"])).is_empty());
        assert_eq!(
            socket_args_from_argv(&argv(&["tmux", "-L", "ws", "attach", "-t", "x"])),
            vec!["-L", "ws"]
        );
        assert_eq!(
            socket_args_from_argv(&argv(&["tmux", "-S", "/tmp/s", "new"])),
            vec!["-S", "/tmp/s"]
        );
        // Attached form `-Lname`.
        assert_eq!(
            socket_args_from_argv(&argv(&["tmux", "-Lws", "attach"])),
            vec!["-L", "ws"]
        );
    }

    #[test]
    fn find_client_picks_deepest_tmux_with_socket() {
        let chain = vec![
            (10, argv(&["konsole"])),
            (20, argv(&["bash"])),
            (30, argv(&["tmux", "-L", "ws", "attach"])),
        ];
        assert_eq!(
            find_client(&chain),
            Some((30, vec!["-L".to_owned(), "ws".to_owned()]))
        );
        // No tmux in the chain → not running tmux.
        let chain = vec![(10, argv(&["konsole"])), (20, argv(&["bash"]))];
        assert_eq!(find_client(&chain), None);
    }

    #[test]
    fn parses_pid_and_session_with_spaces() {
        let map = parse_list_clients("1234 work\n42 my long name\nbad line\n7 \n");
        assert_eq!(map.get(&1234).map(String::as_str), Some("work"));
        assert_eq!(map.get(&42).map(String::as_str), Some("my long name"));
        assert_eq!(map.len(), 2);
    }
}

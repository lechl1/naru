//! Terminal-agnostic tmux session capture for session-restore.
//!
//! tmux sessions live in the tmux *server*, not in the terminal that displays
//! them, so they can be detected and restored without knowing or hardcoding
//! which terminal is running. At save time we ask the server for the pid of
//! every attached client and the session it is on (`tmux list-clients`), then
//! map a terminal window to its session by finding one of those client pids
//! inside that window's process tree (the tmux *panes* live under the separate
//! server process, so the only client in a terminal's own descendant tree is
//! the one driving that terminal).
//!
//! On restore the captured terminal is relaunched reattaching to the session
//! via `tmux new-session -A -s <name>` (attach-or-create), so it works whether
//! the server survived the compositor restart (reattach to the live session)
//! or not, e.g. after a reboot (open a fresh session of that name). Pair with
//! tmux-resurrect/continuum if you want the session *contents* back after a
//! reboot.
//!
//! Limitation: the query uses tmux's default socket, so sessions on a custom
//! socket (`tmux -L`/`-S`) aren't detected.

use std::collections::HashMap;
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use super::cwd::descendant_chain;

/// The argv that reattaches to (or creates) tmux session `name`.
///
/// `new-session -A` attaches when the session already exists and creates it
/// otherwise — robust across a compositor restart (reattach) and a reboot
/// (fresh session of that name).
pub fn reattach_command(session: &str) -> Vec<String> {
    vec![
        "tmux".into(),
        "new-session".into(),
        "-A".into(),
        "-s".into(),
        session.into(),
    ]
}

/// The tmux session a terminal whose process tree starts at `fg_pid` is attached
/// to, if any. `clients` is the client-pid → session map from
/// [`client_pid_sessions`].
pub fn session_for_pid_tree(fg_pid: i32, clients: &HashMap<i32, String>) -> Option<String> {
    if clients.is_empty() {
        return None;
    }
    session_for_chain(&descendant_chain(fg_pid), clients)
}

/// Find the session of the first (deepest) pid in `chain` that is a known tmux
/// client. Split out from [`session_for_pid_tree`] so the lookup is unit-testable
/// without a live process tree.
fn session_for_chain(chain: &[i32], clients: &HashMap<i32, String>) -> Option<String> {
    chain.iter().rev().find_map(|p| clients.get(p).cloned())
}

/// Query the tmux server for every attached client's pid and the session it is
/// on.
///
/// Runs `tmux list-clients -F '#{client_pid} #{session_name}'` (default socket)
/// on a short-lived thread, bounded so a hung server can't stall a save (mirrors
/// the Konsole D-Bus query). Returns an empty map when tmux isn't installed, no
/// server is running, or the query times out.
pub fn client_pid_sessions() -> HashMap<i32, String> {
    let (tx, rx) = mpsc::channel();
    if std::thread::Builder::new()
        .name("tmux-list-clients".to_owned())
        .spawn(move || {
            let _ = tx.send(run_list_clients());
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

/// Run `tmux list-clients` and return its stdout, or `None` on any failure.
fn run_list_clients() -> Option<String> {
    let output = Command::new("tmux")
        .args(["list-clients", "-F", "#{client_pid} #{session_name}"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// Parse `tmux list-clients -F '#{client_pid} #{session_name}'` output into a
/// client-pid → session-name map. One line per client: `<pid> <session>`. The
/// pid is the first whitespace-delimited field; the remainder is the session
/// name, which may itself contain spaces.
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

    #[test]
    fn reattach_is_attach_or_create() {
        assert_eq!(
            reattach_command("work"),
            vec!["tmux", "new-session", "-A", "-s", "work"]
        );
    }

    #[test]
    fn parses_pid_and_session() {
        let map = parse_list_clients("1234 work\n5678 dev\n");
        assert_eq!(map.get(&1234).map(String::as_str), Some("work"));
        assert_eq!(map.get(&5678).map(String::as_str), Some("dev"));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn session_name_may_contain_spaces() {
        let map = parse_list_clients("4242 my long session name\n");
        assert_eq!(map.get(&4242).map(String::as_str), Some("my long session name"));
    }

    #[test]
    fn skips_garbage_and_empty_names() {
        let map = parse_list_clients("\nnotapid session\n7 \n99 ok\n");
        assert_eq!(map.len(), 1);
        assert_eq!(map.get(&99).map(String::as_str), Some("ok"));
    }

    #[test]
    fn chain_lookup_prefers_deepest_client() {
        let mut clients = HashMap::new();
        clients.insert(100, "outer".to_owned());
        clients.insert(300, "inner".to_owned());
        // chain = [terminal, shell, tmux-client]; deepest known client wins.
        assert_eq!(
            session_for_chain(&[100, 200, 300], &clients).as_deref(),
            Some("inner")
        );
        // No client pid in the chain → not running tmux.
        assert_eq!(session_for_chain(&[200, 400], &clients), None);
    }

    #[test]
    fn empty_client_map_is_none() {
        assert_eq!(session_for_pid_tree(1, &HashMap::new()), None);
    }
}

//! Per-window working-directory capture for KDE Konsole.
//!
//! ## Why Konsole needs special handling
//!
//! Konsole is a **single-process, multi-window** application: every window opened from
//! its UI is served by one process, so all of them report the *same* Wayland client PID.
//! The generic [`read_cwd_from_child`](super::cwd::read_cwd_from_child) descends that one
//! process and can only find a single shell, so every Konsole window would be saved with
//! the same directory — and with no per-window PID there's nothing better to read from
//! `/proc`.
//!
//! ## Where the per-window cwd actually lives
//!
//! Konsole exposes its sessions over D-Bus on a per-process service,
//! `org.kde.konsole-<pid>`. Each `/Sessions/<n>` has `processId()` (the shell) whose
//! `/proc/<pid>/cwd` is the directory the user `cd`'d to, and `title(1)` (the displayed
//! title). The Wayland toplevel title is that displayed title plus a " — Konsole" suffix,
//! so we correlate a window to its session by title and read the matching shell's cwd.
//!
//! The lookup is best-effort: any failure (feature off, no session bus, Konsole gone,
//! unmatched title) returns `None` and the caller falls back to the generic capture.

use std::path::PathBuf;

/// Best-effort per-window working directory for a Konsole window.
///
/// Correlates `window_title` (the Wayland toplevel title) against the Konsole sessions
/// exposed by `org.kde.konsole-<pid>` and returns the matching shell's cwd. Returns
/// `None` on any failure so the caller can fall back to the generic cwd capture.
#[cfg(feature = "dbus")]
pub fn cwd_for_window(pid: i32, window_title: &str) -> Option<PathBuf> {
    let sessions = query_session_pids_bounded(pid)?;
    let shell_pid = pick_by_title(window_title, &sessions)?;
    super::cwd::read_cwd_for_pid(shell_pid)
}

/// Stub for builds without the `dbus` feature: no D-Bus, so no per-window cwd.
#[cfg(not(feature = "dbus"))]
pub fn cwd_for_window(_pid: i32, _window_title: &str) -> Option<PathBuf> {
    None
}

/// Best-effort per-window foreground shell pid for a Konsole window.
///
/// Konsole's single shared client PID can't be walked per-window, but its D-Bus
/// sessions each expose `processId()` (the shell). Correlating the window title
/// to a session (same as [`cwd_for_window`]) yields that window's shell pid —
/// the starting point for tmux-client detection. `None` on any failure.
#[cfg(feature = "dbus")]
pub fn shell_pid_for_window(pid: i32, window_title: &str) -> Option<i32> {
    let sessions = query_session_pids_bounded(pid)?;
    pick_by_title(window_title, &sessions)
}

/// Stub for builds without the `dbus` feature.
#[cfg(not(feature = "dbus"))]
pub fn shell_pid_for_window(_pid: i32, _window_title: &str) -> Option<i32> {
    None
}

/// Run the blocking D-Bus query on a short-lived thread and wait only briefly for it.
///
/// The snapshot is built on the compositor's main thread; offloading mirrors
/// `Naru::update_locked_hint` so a hung Konsole (zbus' own reply timeout is 25 s) can't
/// stall a save. On timeout the orphaned thread finishes and drops its result harmlessly.
#[cfg(feature = "dbus")]
fn query_session_pids_bounded(pid: i32) -> Option<Vec<(String, i32)>> {
    use std::sync::mpsc;
    use std::time::Duration;

    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("konsole-session-query".to_owned())
        .spawn(move || {
            let _ = tx.send(query_session_pids(pid));
        })
        .ok()?;

    rx.recv_timeout(Duration::from_millis(200)).ok().flatten()
}

/// Query `org.kde.konsole-<pid>` for `(displayed_title, shell_pid)` of every
/// session. `None` if the service is unreachable or yields nothing. The shell
/// pid is the starting point for both per-window cwd (`/proc/<pid>/cwd`) and
/// tmux-client detection (walking its process tree).
#[cfg(feature = "dbus")]
fn query_session_pids(pid: i32) -> Option<Vec<(String, i32)>> {
    let conn = zbus::blocking::Connection::session().ok()?;
    let service = format!("org.kde.konsole-{pid}");

    let xml = introspect(&conn, &service, "/Sessions")?;

    let mut out = Vec::new();
    for id in session_ids_from_introspection(&xml) {
        let path = format!("/Sessions/{id}");
        // Per-session failures skip that session rather than abort the whole capture.
        let Some(title) = call_string(&conn, &service, &path, "title", &1i32) else {
            continue;
        };
        let Some(shell_pid) = call_i32(&conn, &service, &path, "processId", &()) else {
            continue;
        };
        out.push((title, shell_pid));
    }

    (!out.is_empty()).then_some(out)
}

/// Pick the session cwd whose displayed title best matches the window title.
///
/// Exact match first, then prefix (the Wayland title is the session title plus a
/// " — Konsole" suffix). With no title match but a single session, use it. Multiple
/// prefix matches (two windows sharing a title) resolve to the first — FIFO, the same
/// tie-break the restore matcher uses.
#[cfg(any(feature = "dbus", test))]
fn pick_by_title<T: Clone>(window_title: &str, sessions: &[(String, T)]) -> Option<T> {
    if let Some((_, v)) = sessions.iter().find(|(t, _)| !t.is_empty() && t == window_title) {
        return Some(v.clone());
    }
    if let Some((_, v)) = sessions
        .iter()
        .find(|(t, _)| !t.is_empty() && window_title.starts_with(t.as_str()))
    {
        return Some(v.clone());
    }
    if let [(_, v)] = sessions {
        return Some(v.clone());
    }
    None
}

/// `org.freedesktop.DBus.Introspectable.Introspect` on `path`, returning the XML.
#[cfg(feature = "dbus")]
fn introspect(conn: &zbus::blocking::Connection, service: &str, path: &str) -> Option<String> {
    let msg = conn
        .call_method(
            Some(service),
            path,
            Some("org.freedesktop.DBus.Introspectable"),
            "Introspect",
            &(),
        )
        .ok()?;
    msg.body().deserialize::<String>().ok()
}

/// Extract `/Sessions/<id>` child node names from an introspection XML document.
///
/// Pure string scan (no XML crate): node lines look like `<node name="3"/>`.
#[cfg(any(feature = "dbus", test))]
fn session_ids_from_introspection(xml: &str) -> Vec<String> {
    xml.lines()
        .filter_map(|line| {
            let line = line.trim();
            let rest = line.strip_prefix("<node name=\"")?;
            let end = rest.find('"')?;
            Some(rest[..end].to_owned())
        })
        .collect()
}

/// Call a `org.kde.konsole.Session` method returning a string.
#[cfg(feature = "dbus")]
fn call_string<B>(
    conn: &zbus::blocking::Connection,
    service: &str,
    path: &str,
    method: &str,
    body: &B,
) -> Option<String>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    let msg = conn
        .call_method(Some(service), path, Some("org.kde.konsole.Session"), method, body)
        .ok()?;
    msg.body().deserialize::<String>().ok()
}

/// Call a `org.kde.konsole.Session` method returning an int.
#[cfg(feature = "dbus")]
fn call_i32<B>(
    conn: &zbus::blocking::Connection,
    service: &str,
    path: &str,
    method: &str,
    body: &B,
) -> Option<i32>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    let msg = conn
        .call_method(Some(service), path, Some("org.kde.konsole.Session"), method, body)
        .ok()?;
    msg.body().deserialize::<i32>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sessions(pairs: &[(&str, &str)]) -> Vec<(String, PathBuf)> {
        pairs
            .iter()
            .map(|(t, p)| ((*t).to_owned(), PathBuf::from(p)))
            .collect()
    }

    #[test]
    fn prefix_match_handles_konsole_suffix() {
        let s = sessions(&[
            ("naru : claude", "/ws/naru"),
            ("creeper : claude", "/ws/creeper"),
        ]);
        // The Wayland title carries the " — Konsole" suffix the session title lacks.
        assert_eq!(
            pick_by_title("creeper : claude — Konsole", &s),
            Some(PathBuf::from("/ws/creeper"))
        );
    }

    #[test]
    fn exact_match_wins() {
        let s = sessions(&[("x", "/a"), ("x : y", "/b")]);
        assert_eq!(pick_by_title("x", &s), Some(PathBuf::from("/a")));
    }

    #[test]
    fn single_session_used_when_no_title_match() {
        let s = sessions(&[("anything", "/only")]);
        assert_eq!(
            pick_by_title("unrelated title", &s),
            Some(PathBuf::from("/only"))
        );
    }

    #[test]
    fn no_match_among_many_is_none() {
        let s = sessions(&[("a", "/a"), ("b", "/b")]);
        assert_eq!(pick_by_title("zzz", &s), None);
    }

    #[test]
    fn ambiguous_prefix_resolves_to_first() {
        let s = sessions(&[("naru : zsh", "/first"), ("naru : zsh", "/second")]);
        assert_eq!(
            pick_by_title("naru : zsh — Konsole", &s),
            Some(PathBuf::from("/first"))
        );
    }

    #[test]
    fn empty_titles_are_ignored() {
        let s = sessions(&[("", "/blank"), ("creeper", "/ws/creeper")]);
        assert_eq!(
            pick_by_title("creeper — Konsole", &s),
            Some(PathBuf::from("/ws/creeper"))
        );
        // A blank-title session must not match an empty-prefix.
        assert_eq!(pick_by_title("", &s), None);
    }

    #[test]
    fn session_ids_parsed_from_introspection_xml() {
        let xml = r#"<node>
  <node name="1"/>
  <node name="2"/>
  <interface name="org.kde.konsole.Session"/>
</node>"#;
        assert_eq!(session_ids_from_introspection(xml), vec!["1", "2"]);
    }
}

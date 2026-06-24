//! Minimal `.desktop` file lookup for session restore.
//!
//! ## Why this exists
//!
//! A PWA / site-specific-browser (SSB) window — e.g. an installed Chromium "web app"
//! like a YouTube PWA — is owned by the *browser* process. So the generic relaunch
//! paths in [`super::restore`] (`flatpak run <flatpak_id>` or the captured `exec`)
//! reopen the **whole browser**, not the app. The per-app launch arguments
//! (`--app-id=…`, `--app=…`, `--profile-directory=…`) aren't on the browser process's
//! `/proc/<pid>/cmdline` either: the launcher forwards them to the already-running
//! browser over IPC and exits.
//!
//! They *are* recorded in the app's installed `.desktop` file, which is the canonical
//! way desktop environments map a window back to the app that launched it:
//!
//! ```text
//! StartupWMClass=crx_agimnkijcaahngcdmfeangaknmldooml
//! Exec=flatpak 'run' '--command=/app/bin/helium' 'net.imput.helium' \
//!      '--profile-directory=Default' '--app-id=agimnkijcaahngcdmfeangaknmldooml'
//! ```
//!
//! Chromium sets the Wayland `app_id` to that same `StartupWMClass`, so matching the
//! window's `app_id` against `StartupWMClass` recovers the exact relaunch command.
//!
//! ## Scope
//!
//! This is a deliberately small slice of desktop-file tracking (the codebase otherwise
//! has none — see `src/dbus/gnome_shell_introspect.rs`). It indexes only the
//! `[Desktop Entry]` group's `StartupWMClass` + `Exec`, and [`ssb_launch_argv`] gates
//! on the SSB markers `--app-id=` / `--app=` so only PWA windows ever get the captured
//! command; everything else keeps the existing generic relaunch behaviour.

use std::collections::HashMap;
use std::path::PathBuf;

/// Field codes a desktop `Exec` may contain (per the Desktop Entry Spec). They are
/// expanded by the *launcher* with file/URL arguments; for a respawn we have none, so
/// every occurrence is dropped. `%%` (a literal percent) is handled separately.
const FIELD_CODES: &[&str] = &[
    "%f", "%F", "%u", "%U", "%d", "%D", "%n", "%N", "%i", "%c", "%k", "%v", "%m",
];

/// Build a map of `StartupWMClass` → relaunch argv by scanning the standard
/// application directories: `applications/` under `$XDG_DATA_HOME` (default
/// `~/.local/share`) followed by each `$XDG_DATA_DIRS` entry (default
/// `/usr/local/share:/usr/share`). On a typical system `$XDG_DATA_DIRS` already
/// includes the flatpak export dirs, so exported PWA launchers are covered.
///
/// Earlier directories win: a key already present is not overwritten, matching XDG
/// precedence (user dir shadows system dirs).
pub fn index_startup_wm_classes() -> HashMap<String, Vec<String>> {
    index_dirs(&application_dirs())
}

/// The argv to relaunch `app_id` as a PWA, or `None` if it isn't one.
///
/// Returns the indexed `Exec` argv only when a desktop file's `StartupWMClass` equals
/// `app_id` **and** that `Exec` carries an SSB marker (`--app-id=` or `--app=`). The
/// marker gate keeps non-PWA windows (which also have desktop files) on the existing
/// flatpak_id/exec relaunch path.
pub fn ssb_launch_argv(app_id: &str, index: &HashMap<String, Vec<String>>) -> Option<Vec<String>> {
    let argv = index.get(app_id)?;
    let is_ssb = argv
        .iter()
        .any(|a| a.starts_with("--app-id=") || a.starts_with("--app="));
    is_ssb.then(|| argv.clone())
}

/// The ordered list of `applications/` directories to scan, user dir first.
fn application_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    if let Some(home) = data_home {
        dirs.push(home.join("applications"));
    }

    let data_dirs = std::env::var_os("XDG_DATA_DIRS")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/usr/local/share:/usr/share"));
    for dir in std::env::split_paths(&data_dirs) {
        dirs.push(dir.join("applications"));
    }

    dirs
}

/// Scan `dirs` in order, returning the `StartupWMClass` → argv index. First match wins.
/// Split out from [`index_startup_wm_classes`] so tests can point it at a fixture dir.
fn index_dirs(dirs: &[PathBuf]) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();

    for dir in dirs {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => continue, // Missing/unreadable dir is normal; skip it.
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let Ok(contents) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Some((wm_class, exec)) = parse_desktop_entry(&contents) else {
                continue;
            };
            // First (highest-precedence) definition of a given WM class wins.
            map.entry(wm_class).or_insert_with(|| parse_exec(&exec));
        }
    }

    map
}

/// Extract `(StartupWMClass, Exec)` from the `[Desktop Entry]` group only.
///
/// Stops at the first action/other group header (`[Desktop Action …]`) so per-action
/// `Exec` lines don't shadow the main one. Returns `None` unless both keys are present.
fn parse_desktop_entry(contents: &str) -> Option<(String, String)> {
    let mut in_entry = false;
    let mut wm_class = None;
    let mut exec = None;

    for line in contents.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry || line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "StartupWMClass" => wm_class = Some(value.trim().to_string()),
            "Exec" => exec = Some(value.trim().to_string()),
            _ => {}
        }
    }

    Some((wm_class?, exec?))
}

/// Tokenize a desktop `Exec` value into an argv, stripping field codes.
///
/// Honors both single and double quotes. The Desktop Entry Spec only blesses double
/// quotes, but real-world exporters (flatpak's PWA launchers among them) emit
/// single-quoted tokens like `'run' '--app-id=…'`, so we accept either; a quote is
/// literal only when escaped as `\"`. Field codes ([`FIELD_CODES`]) are dropped and
/// `%%` collapses to a literal `%`.
fn parse_exec(exec: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut has_token = false;
    let mut quote: Option<char> = None;
    let mut chars = exec.chars().peekable();

    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else if c == '\\' && chars.peek() == Some(&q) {
                    cur.push(chars.next().unwrap());
                } else {
                    cur.push(c);
                }
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    has_token = true;
                }
                c if c.is_whitespace() => {
                    if has_token {
                        push_arg(&mut args, std::mem::take(&mut cur));
                        has_token = false;
                    }
                }
                _ => {
                    cur.push(c);
                    has_token = true;
                }
            },
        }
    }
    if has_token {
        push_arg(&mut args, cur);
    }

    args
}

/// Push one tokenized argument unless it is a bare field code, expanding `%%` → `%`.
fn push_arg(args: &mut Vec<String>, arg: String) {
    if FIELD_CODES.contains(&arg.as_str()) {
        return;
    }
    args.push(arg.replace("%%", "%"));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "naru-desktop-test-{}-{}",
            std::process::id(),
            fastrand::u32(..)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn parse_exec_handles_single_quoted_tokens() {
        // The real Helium PWA launcher format: single-quoted tokens.
        let argv = parse_exec(
            "flatpak 'run' '--command=/app/bin/helium' 'net.imput.helium' \
             '--profile-directory=Default' '--app-id=agimnkijcaahngcdmfeangaknmldooml'",
        );
        assert_eq!(
            argv,
            vec![
                "flatpak",
                "run",
                "--command=/app/bin/helium",
                "net.imput.helium",
                "--profile-directory=Default",
                "--app-id=agimnkijcaahngcdmfeangaknmldooml",
            ]
        );
    }

    #[test]
    fn parse_exec_strips_field_codes_and_unescapes_percent() {
        let argv = parse_exec("brave --app=https://x.test/ %U %u 100%% done");
        // %U and %u are dropped; %% becomes a literal %.
        assert_eq!(
            argv,
            vec!["brave", "--app=https://x.test/", "100%", "done"]
        );
    }

    #[test]
    fn parse_exec_handles_double_quotes_with_spaces() {
        let argv = parse_exec(r#"prog "arg with spaces" plain"#);
        assert_eq!(argv, vec!["prog", "arg with spaces", "plain"]);
    }

    #[test]
    fn parse_desktop_entry_reads_only_main_group() {
        let contents = "\
[Desktop Entry]
Name=YouTube
StartupWMClass=crx_abc
Exec=flatpak run net.imput.helium --app-id=abc

[Desktop Action Search]
Name=Search
Exec=flatpak run net.imput.helium --app-id=abc --app-launch-url=https://x/search
";
        let (wm, exec) = parse_desktop_entry(contents).unwrap();
        assert_eq!(wm, "crx_abc");
        // The action group's Exec must NOT shadow the main one.
        assert_eq!(exec, "flatpak run net.imput.helium --app-id=abc");
    }

    #[test]
    fn parse_desktop_entry_requires_both_keys() {
        assert!(parse_desktop_entry("[Desktop Entry]\nExec=foo\n").is_none());
        assert!(parse_desktop_entry("[Desktop Entry]\nStartupWMClass=bar\n").is_none());
    }

    #[test]
    fn index_and_ssb_lookup_end_to_end() {
        let dir = tmp_dir();
        // A PWA launcher (SSB markers present).
        std::fs::write(
            dir.join("pwa.desktop"),
            "[Desktop Entry]\nStartupWMClass=crx_abc\n\
             Exec=flatpak 'run' 'net.imput.helium' '--app-id=abc'\n",
        )
        .unwrap();
        // A plain app — has a StartupWMClass but no SSB marker.
        std::fs::write(
            dir.join("plain.desktop"),
            "[Desktop Entry]\nStartupWMClass=org.kde.konsole\nExec=konsole %u\n",
        )
        .unwrap();
        // A non-desktop file is ignored.
        std::fs::write(dir.join("noise.txt"), "StartupWMClass=ignored\n").unwrap();

        let index = index_dirs(&[dir.clone()]);

        // PWA: recovered with the marker, field codes already stripped.
        assert_eq!(
            ssb_launch_argv("crx_abc", &index),
            Some(vec![
                "flatpak".into(),
                "run".into(),
                "net.imput.helium".into(),
                "--app-id=abc".into(),
            ])
        );
        // Plain app: indexed, but ssb gate rejects it (no --app-id=/--app=).
        assert!(index.contains_key("org.kde.konsole"));
        assert_eq!(ssb_launch_argv("org.kde.konsole", &index), None);
        // Unknown class: absent.
        assert_eq!(ssb_launch_argv("crx_missing", &index), None);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn index_first_dir_wins() {
        let high = tmp_dir();
        let low = tmp_dir();
        std::fs::write(
            high.join("a.desktop"),
            "[Desktop Entry]\nStartupWMClass=crx_x\nExec=high --app-id=x\n",
        )
        .unwrap();
        std::fs::write(
            low.join("b.desktop"),
            "[Desktop Entry]\nStartupWMClass=crx_x\nExec=low --app-id=x\n",
        )
        .unwrap();

        let index = index_dirs(&[high.clone(), low.clone()]);
        assert_eq!(index.get("crx_x").unwrap()[0], "high");

        let _ = std::fs::remove_dir_all(high);
        let _ = std::fs::remove_dir_all(low);
    }
}

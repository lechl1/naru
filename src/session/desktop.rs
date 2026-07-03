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
//!
//! ## Colliding launchers
//!
//! The same web app installed under two browsers produces two `.desktop` files with the
//! *identical* `StartupWMClass` — Chromium derives it from the extension id, which is the
//! same across browsers. The real case that motivated this: a YouTube PWA left a stale
//! Helium launcher behind after the user moved to Brave, so both
//! `Exec=flatpak run … net.imput.helium … --app-id=<id>` and
//! `Exec=flatpak run … com.brave.Browser … --app-id=<id>` claim `crx_<id>`. Picking the
//! wrong one relaunches (or fails to launch) the wrong browser. So the index keeps *all*
//! candidate commands per class, and [`ssb_launch_argv`] disambiguates by the running
//! window's captured owner (its flatpak id, or its executable) before falling back to the
//! highest-precedence entry.

use std::collections::HashMap;
use std::path::PathBuf;

/// Field codes a desktop `Exec` may contain (per the Desktop Entry Spec). They are
/// expanded by the *launcher* with file/URL arguments; for a respawn we have none, so
/// every occurrence is dropped. `%%` (a literal percent) is handled separately.
const FIELD_CODES: &[&str] = &[
    "%f", "%F", "%u", "%U", "%d", "%D", "%n", "%N", "%i", "%c", "%k", "%v", "%m",
];

/// Build a map of `StartupWMClass` → candidate relaunch argvs by scanning the standard
/// application directories: `applications/` under `$XDG_DATA_HOME` (default
/// `~/.local/share`) followed by each `$XDG_DATA_DIRS` entry (default
/// `/usr/local/share:/usr/share`). On a typical system `$XDG_DATA_DIRS` already
/// includes the flatpak export dirs, so exported PWA launchers are covered.
///
/// Each class maps to *all* matching `Exec` argvs in scan order (user dir first), so a
/// web app installed under two browsers — which share one `StartupWMClass` — keeps both
/// candidates for [`ssb_launch_argv`] to disambiguate. The first entry is the
/// highest-precedence one (user dir shadows system dirs) and is used as the fallback.
pub fn index_startup_wm_classes() -> HashMap<String, Vec<Vec<String>>> {
    index_dirs(&application_dirs())
}

/// The argv to relaunch `app_id` as a PWA, or `None` if it isn't one.
///
/// Considers only desktop files whose `StartupWMClass` equals `app_id` **and** whose
/// `Exec` carries an SSB marker (`--app-id=` or `--app=`); the marker gate keeps non-PWA
/// windows (which also have desktop files) on the existing flatpak_id/exec relaunch path.
///
/// When more than one browser installed the same web app they collide on one
/// `StartupWMClass`, so the running window's captured owner picks the right launcher:
/// `flatpak_id` matches the candidate whose `Exec` names that flatpak app (a bare token,
/// e.g. `com.brave.Browser`), and for a native browser `exec` matches by the launcher
/// command's basename. With no match — or nothing to match on — the highest-precedence
/// candidate is used, preserving the single-launcher behaviour.
pub fn ssb_launch_argv(
    app_id: &str,
    flatpak_id: Option<&str>,
    exec: Option<&str>,
    index: &HashMap<String, Vec<Vec<String>>>,
) -> Option<Vec<String>> {
    let ssb: Vec<&Vec<String>> = index.get(app_id)?.iter().filter(|a| is_ssb(a)).collect();
    let first = ssb.first().copied()?;
    let chosen = pick_matching_owner(&ssb, flatpak_id, exec).unwrap_or(first);
    Some(chosen.clone())
}

/// Whether an `Exec` argv is a site-specific browser (carries `--app-id=`/`--app=`).
fn is_ssb(argv: &[String]) -> bool {
    argv.iter()
        .any(|a| a.starts_with("--app-id=") || a.starts_with("--app="))
}

/// Pick the candidate whose launcher belongs to the running window's owner.
///
/// A flatpak PWA's `Exec` carries its browser's flatpak id as a bare token
/// (`flatpak run … com.brave.Browser … --app-id=…`), so a captured `flatpak_id` selects
/// by exact token match. A native browser's `Exec` starts with the browser command, so a
/// captured `exec` selects by that command's basename. `None` if nothing matches (the
/// caller then falls back to the highest-precedence candidate).
fn pick_matching_owner<'a>(
    ssb: &[&'a Vec<String>],
    flatpak_id: Option<&str>,
    exec: Option<&str>,
) -> Option<&'a Vec<String>> {
    if let Some(id) = flatpak_id {
        if let Some(m) = ssb.iter().find(|argv| argv.iter().any(|t| t == id)) {
            return Some(m);
        }
    }
    if let Some(exec) = exec {
        let want = basename(exec);
        if let Some(m) = ssb
            .iter()
            .find(|argv| argv.first().map(|c| basename(c)) == Some(want))
        {
            return Some(m);
        }
    }
    None
}

/// The final `/`-separated component of a path-or-command (the whole string if none).
fn basename(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
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

/// Scan `dirs` in order, returning the `StartupWMClass` → candidate-argvs index. All
/// matching desktop files are kept per class, in scan order (highest-precedence first),
/// so colliding launchers can be disambiguated later. Split out from
/// [`index_startup_wm_classes`] so tests can point it at a fixture dir.
fn index_dirs(dirs: &[PathBuf]) -> HashMap<String, Vec<Vec<String>>> {
    let mut map: HashMap<String, Vec<Vec<String>>> = HashMap::new();

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
            let argv = parse_exec(&exec);
            // Keep every launcher for the class; identical duplicates (the same app
            // exported into several dirs) collapse so they don't crowd the candidates.
            let candidates = map.entry(wm_class).or_default();
            if !candidates.contains(&argv) {
                candidates.push(argv);
            }
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

        // PWA: recovered with the marker, field codes already stripped. A single
        // candidate needs no disambiguation, so the owner hints go unused.
        assert_eq!(
            ssb_launch_argv("crx_abc", None, None, &index),
            Some(vec![
                "flatpak".into(),
                "run".into(),
                "net.imput.helium".into(),
                "--app-id=abc".into(),
            ])
        );
        // Plain app: indexed, but ssb gate rejects it (no --app-id=/--app=).
        assert!(index.contains_key("org.kde.konsole"));
        assert_eq!(ssb_launch_argv("org.kde.konsole", None, None, &index), None);
        // Unknown class: absent.
        assert_eq!(ssb_launch_argv("crx_missing", None, None, &index), None);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn colliding_launchers_disambiguate_by_owner() {
        // The real YouTube case: one web app, two browsers, one shared StartupWMClass.
        let dir = tmp_dir();
        std::fs::write(
            dir.join("brave.desktop"),
            "[Desktop Entry]\nStartupWMClass=crx_yt\n\
             Exec=flatpak 'run' '--command=brave' 'com.brave.Browser' '--app-id=yt'\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("helium.desktop"),
            "[Desktop Entry]\nStartupWMClass=crx_yt\n\
             Exec=flatpak 'run' '--command=/app/bin/helium' 'net.imput.helium' '--app-id=yt'\n",
        )
        .unwrap();

        let index = index_dirs(&[dir.clone()]);
        // Both launchers are kept as candidates for the class.
        assert_eq!(index.get("crx_yt").map(Vec::len), Some(2));

        // A window owned by Brave picks Brave's launcher regardless of scan order.
        assert_eq!(
            ssb_launch_argv("crx_yt", Some("com.brave.Browser"), None, &index),
            Some(vec![
                "flatpak".into(),
                "run".into(),
                "--command=brave".into(),
                "com.brave.Browser".into(),
                "--app-id=yt".into(),
            ])
        );
        // And a Helium-owned window picks Helium's.
        assert_eq!(
            ssb_launch_argv("crx_yt", Some("net.imput.helium"), None, &index),
            Some(vec![
                "flatpak".into(),
                "run".into(),
                "--command=/app/bin/helium".into(),
                "net.imput.helium".into(),
                "--app-id=yt".into(),
            ])
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn native_launcher_disambiguates_by_exec_basename() {
        // Two native-browser PWAs colliding; the captured executable path selects one
        // by its launcher command's basename.
        let dir = tmp_dir();
        std::fs::write(
            dir.join("brave.desktop"),
            "[Desktop Entry]\nStartupWMClass=crx_yt\n\
             Exec=/opt/brave/brave --profile-directory=Default --app-id=yt\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("chromium.desktop"),
            "[Desktop Entry]\nStartupWMClass=crx_yt\nExec=chromium --app-id=yt\n",
        )
        .unwrap();

        let index = index_dirs(&[dir.clone()]);
        assert_eq!(
            ssb_launch_argv("crx_yt", None, Some("/usr/bin/chromium"), &index),
            Some(vec!["chromium".into(), "--app-id=yt".into()])
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn no_owner_match_falls_back_to_first_candidate() {
        // When the captured owner matches neither launcher (e.g. a since-removed
        // browser), the highest-precedence candidate is still returned.
        let dir = tmp_dir();
        std::fs::write(
            dir.join("brave.desktop"),
            "[Desktop Entry]\nStartupWMClass=crx_yt\n\
             Exec=flatpak run com.brave.Browser --app-id=yt\n",
        )
        .unwrap();

        let index = index_dirs(&[dir.clone()]);
        assert_eq!(
            ssb_launch_argv("crx_yt", Some("org.some.Other"), None, &index),
            Some(vec![
                "flatpak".into(),
                "run".into(),
                "com.brave.Browser".into(),
                "--app-id=yt".into(),
            ])
        );

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
        // Both are kept, highest-precedence (user dir) first.
        assert_eq!(index.get("crx_x").unwrap().len(), 2);
        assert_eq!(index.get("crx_x").unwrap()[0][0], "high");

        let _ = std::fs::remove_dir_all(high);
        let _ = std::fs::remove_dir_all(low);
    }
}

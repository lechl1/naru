//! Resumable Claude Code session capture for session-restore.
//!
//! Claude Code (the `claude` CLI) runs as a TUI inside a terminal and persists
//! each conversation to `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`,
//! where `<encoded-cwd>` is the absolute working directory with every
//! non-alphanumeric character replaced by `-`. On restore the terminal is
//! relaunched running `claude --resume <session-id>` so the conversation picks
//! up where it left off.
//!
//! Detection mirrors tmux: walk the terminal's process tree for the `claude`
//! process and take its cwd, then resolve the active session as the most
//! recently modified transcript in that cwd's project directory. The directory
//! encoding is lossy (`/a/b` and `/a.b` both encode to `-a-b`), so the candidate
//! is confirmed against the `cwd` recorded inside the transcript before its id is
//! trusted. A `claude` running *inside* tmux lives under the tmux server, not the
//! terminal's own process tree, so it's covered by tmux reattach instead and
//! never double-captured here.

use std::fs;
use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::cwd::{descendant_chain, read_cmdline, read_cwd_for_pid};

/// The argv that resumes Claude Code session `session_id`.
pub fn resume_command(session_id: &str) -> Vec<String> {
    vec!["claude".to_owned(), "--resume".to_owned(), session_id.to_owned()]
}

/// The id of the resumable Claude Code session a terminal whose process tree
/// starts at `fg_pid` is running, if any — suitable for [`resume_command`].
///
/// Returns `None` when no `claude` process is in the tree, its cwd is unreadable,
/// or that cwd's project directory has no transcript recorded for it.
pub fn session_for_window(fg_pid: i32) -> Option<String> {
    let claude_pid = find_claude(fg_pid)?;
    let cwd = read_cwd_for_pid(claude_pid)?;
    let dir = project_dir(&cwd)?;
    newest_session_for_cwd(&dir, &cwd)
}

/// The pid of the deepest `claude` process in `fg_pid`'s descendant chain.
fn find_claude(fg_pid: i32) -> Option<i32> {
    descendant_chain(fg_pid)
        .into_iter()
        .rev()
        .find(|&pid| is_claude(&read_cmdline(pid)))
}

/// Whether `argv` invokes the Claude Code CLI (its program's basename is `claude`).
fn is_claude(argv: &[String]) -> bool {
    argv.first()
        .and_then(|a| a.rsplit('/').next())
        .is_some_and(|base| base == "claude")
}

/// The `~/.claude/projects/<encoded-cwd>` directory Claude Code stores a working
/// directory's transcripts under. `None` if the home directory is unknown.
fn project_dir(cwd: &Path) -> Option<PathBuf> {
    let home = directories::BaseDirs::new()?.home_dir().to_owned();
    Some(home.join(".claude/projects").join(encode_cwd(cwd)))
}

/// Encode an absolute path the way Claude Code names its project directories:
/// every non-alphanumeric character becomes `-` (so `/home/u/.x` → `-home-u--x`).
fn encode_cwd(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The session id of the most recently modified transcript in `dir` whose
/// recorded working directory is `cwd`.
///
/// The newest transcript is the live conversation; the `cwd` check disambiguates
/// the lossy directory encoding (two real directories can share one project dir,
/// in which case only the matching transcripts are ours). `None` if no transcript
/// records this cwd.
fn newest_session_for_cwd(dir: &Path, cwd: &Path) -> Option<String> {
    let mut transcripts: Vec<(SystemTime, PathBuf)> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|p| Some((fs::metadata(&p).ok()?.modified().ok()?, p)))
        .collect();
    // Newest first; the live session is the one being written.
    transcripts.sort_by(|a, b| b.0.cmp(&a.0));

    transcripts
        .iter()
        .find(|(_, p)| transcript_cwd(p).as_deref() == Some(cwd))
        .and_then(|(_, p)| session_id_from_path(p))
}

/// The working directory a transcript was recorded in, read from the `cwd` field
/// of its JSONL records. The leading records are session metadata
/// (`{"type":"mode",…}`) that carry no `cwd`; the conversation records that do
/// begin a few lines in, so a bounded prefix is scanned for the first one. `None`
/// if unreadable or no record within the prefix carries a `cwd`.
fn transcript_cwd(path: &Path) -> Option<PathBuf> {
    let file = fs::File::open(path).ok()?;
    BufReader::new(file)
        .lines()
        .take(MAX_TRANSCRIPT_PREFIX_LINES)
        .flatten()
        .find_map(|line| json_cwd(&line))
}

/// Extract a `"cwd"` string field from one JSONL record. `None` if the line isn't
/// JSON or has no `cwd`.
fn json_cwd(line: &str) -> Option<PathBuf> {
    let value: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    value.get("cwd")?.as_str().map(PathBuf::from)
}

/// How many leading transcript records to scan for a `cwd`. The field appears
/// within the first handful of lines; this just bounds work on a huge transcript
/// whose records somehow never carry one.
const MAX_TRANSCRIPT_PREFIX_LINES: usize = 64;

/// A transcript's session id is its filename stem (the `<uuid>` of `<uuid>.jsonl`).
fn session_id_from_path(path: &Path) -> Option<String> {
    path.file_stem()?.to_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn resume_command_targets_session() {
        assert_eq!(
            resume_command("abc-123"),
            vec!["claude", "--resume", "abc-123"]
        );
    }

    #[test]
    fn detects_claude_by_basename() {
        assert!(is_claude(&argv(&["/home/u/.local/bin/claude", "--resume"])));
        assert!(is_claude(&argv(&["claude"])));
        assert!(!is_claude(&argv(&["node", "claude.js"])));
        assert!(!is_claude(&argv(&["zsh"])));
        assert!(!is_claude(&argv(&[])));
    }

    #[test]
    fn encode_cwd_replaces_every_non_alnum() {
        assert_eq!(
            encode_cwd(Path::new("/home/leochl/workspace/naru")),
            "-home-leochl-workspace-naru"
        );
        // Dots collapse to `-` too, matching Claude Code (`/home/u/.alloy`).
        assert_eq!(encode_cwd(Path::new("/home/u/.alloy")), "-home-u--alloy");
    }

    #[test]
    fn newest_matching_transcript_wins() {
        use std::io::Write as _;

        let dir = std::env::temp_dir().join(format!(
            "naru-claude-test-{}-{}",
            std::process::id(),
            fastrand::u32(..)
        ));
        fs::create_dir_all(&dir).unwrap();
        let cwd = Path::new("/ws/project");

        let write = |id: &str, recorded_cwd: &str| {
            let mut f = fs::File::create(dir.join(format!("{id}.jsonl"))).unwrap();
            // Mirror the real layout: leading metadata records carry no cwd; the
            // first conversation record (a few lines in) is what records it.
            writeln!(f, r#"{{"type":"mode","mode":"normal","sessionId":"{id}"}}"#).unwrap();
            writeln!(f, r#"{{"type":"summary"}}"#).unwrap();
            writeln!(f, r#"{{"type":"user","cwd":"{recorded_cwd}"}}"#).unwrap();
        };

        // An older transcript for our cwd, and a foreign one that sorts newer.
        write("older-ours", "/ws/project");
        std::thread::sleep(std::time::Duration::from_millis(20));
        write("newer-foreign", "/ws/other");

        // The foreign transcript is newest but doesn't match; ours is chosen.
        assert_eq!(
            newest_session_for_cwd(&dir, cwd).as_deref(),
            Some("older-ours")
        );

        // A newer transcript for our cwd then takes precedence.
        std::thread::sleep(std::time::Duration::from_millis(20));
        write("newer-ours", "/ws/project");
        assert_eq!(
            newest_session_for_cwd(&dir, cwd).as_deref(),
            Some("newer-ours")
        );

        // No transcript records this cwd → nothing to resume.
        assert_eq!(newest_session_for_cwd(&dir, Path::new("/ws/absent")), None);

        let _ = fs::remove_dir_all(&dir);
    }
}

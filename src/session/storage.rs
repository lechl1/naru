//! Atomic load / save for the persisted session state file.
//!
//! Default location: `$XDG_STATE_HOME/naru/session.json`, falling back to
//! `$HOME/.local/state/naru/session.json` when `XDG_STATE_HOME` is unset.
//!
//! Saves are performed via the canonical write-tempfile-then-rename dance: writes go to
//! a sibling `session.json.tmp.<random>` first and are only `rename(2)`d into place once
//! the body has been fully flushed to disk. On POSIX same-filesystem, `rename` is
//! atomic, so a crash mid-write can never produce a half-written `session.json`.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

use super::state::{SessionState, SCHEMA_VERSION};

/// Resolve the default session state file path.
///
/// Returns `None` only when neither `$XDG_STATE_HOME` nor `$HOME` are set — in practice
/// callers should treat that as "session restore disabled" rather than an error.
pub fn default_state_path() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.state_dir().map_or_else(
        // BaseDirs::state_dir() returns None on platforms without a state dir
        // (notably non-Linux Unix); fall back to ~/.local/state explicitly to match
        // what the XDG Base Directory spec says we should construct anyway.
        || b.home_dir().join(".local/state/naru/session.json"),
        |s| s.join("naru/session.json"),
    ))
}

/// Load the session state from disk.
///
/// Returns `Ok(None)` for any of:
/// - the file does not exist (first run, or feature was previously off);
/// - the file exists but is empty;
/// - the schema version does not match `SCHEMA_VERSION` (treat as a clean slate rather
///   than crashing on a stale or future format).
///
/// Returns `Err` only for genuine IO or JSON parse errors on a present, version-matched
/// file — those are worth surfacing because they indicate corruption.
pub fn load(path: &Path) -> Result<Option<SessionState>> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("reading session state from {}", path.display()))
        }
    };

    if bytes.is_empty() {
        return Ok(None);
    }

    let state: SessionState = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing session state at {}", path.display()))?;

    if state.version != SCHEMA_VERSION {
        warn!(
            "session state at {} has version {} (expected {}); discarding and starting fresh",
            path.display(),
            state.version,
            SCHEMA_VERSION,
        );
        return Ok(None);
    }

    Ok(Some(state))
}

/// Atomically save the session state to disk.
///
/// Creates the parent directory if missing, writes to a randomly-suffixed sibling temp
/// file, fsyncs it, then renames into place. On any error the temp file is best-effort
/// removed.
pub fn save_atomic(path: &Path, state: &SessionState) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dir {}", parent.display()))?;
    }

    let body = serde_json::to_vec_pretty(state).context("serializing session state to JSON")?;

    let tmp_path = tmp_sibling(path);

    let result = (|| -> Result<()> {
        let mut f = fs::File::create(&tmp_path)
            .with_context(|| format!("creating temp file {}", tmp_path.display()))?;
        f.write_all(&body)
            .with_context(|| format!("writing temp file {}", tmp_path.display()))?;
        f.sync_all()
            .with_context(|| format!("fsync temp file {}", tmp_path.display()))?;
        drop(f);
        fs::rename(&tmp_path, path).with_context(|| {
            format!(
                "renaming {} -> {}",
                tmp_path.display(),
                path.display()
            )
        })?;
        Ok(())
    })();

    if result.is_err() {
        // Don't shadow the real error if cleanup also fails.
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

/// Build a sibling temp-file path next to `target` with a randomized suffix.
///
/// The suffix combines pid + a `fastrand` u32 so that two simultaneous saves from the
/// same process (debouncer + shutdown hook racing, say) don't clobber each other's temp
/// files before the rename.
fn tmp_sibling(target: &Path) -> PathBuf {
    let pid = std::process::id();
    let rnd: u32 = fastrand::u32(..);
    let mut name = target
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("session.json"));
    name.push(format!(".tmp.{pid}.{rnd:08x}"));
    target.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::state::{Placement, WindowEntry, WorkspaceRef};

    fn tmp_dir() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "naru-session-test-{}-{}",
            std::process::id(),
            fastrand::u32(..)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn load_missing_returns_none() {
        let dir = tmp_dir();
        let path = dir.join("nope.json");
        assert!(load(&path).unwrap().is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tmp_dir();
        let path = dir.join("session.json");
        let state = SessionState {
            version: SCHEMA_VERSION,
            windows: vec![WindowEntry {
                app_id: "org.kde.dolphin".into(),
                title: None,
                cwd: Some(PathBuf::from("/tmp")),
                output: Some("DP-1".into()),
                workspace: WorkspaceRef::Index { index: 1 },
                placement: Placement::Tiled {
                    column_index: 0,
                    tile_index: 0,
                    is_fullscreen: false,
                    is_maximized: false,
                },
            }],
        };
        save_atomic(&path, &state).unwrap();
        let loaded = load(&path).unwrap().expect("file should exist");
        assert_eq!(state, loaded);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn save_creates_parent_dir() {
        let dir = tmp_dir();
        // Two levels deep — neither exists.
        let path = dir.join("a/b/session.json");
        save_atomic(&path, &SessionState::empty()).unwrap();
        assert!(path.is_file());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn version_mismatch_returns_none() {
        let dir = tmp_dir();
        let path = dir.join("session.json");
        std::fs::write(
            &path,
            r#"{"version": 999, "windows": []}"#,
        )
        .unwrap();
        assert!(load(&path).unwrap().is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn empty_file_returns_none() {
        let dir = tmp_dir();
        let path = dir.join("session.json");
        std::fs::write(&path, "").unwrap();
        assert!(load(&path).unwrap().is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_json_returns_err() {
        let dir = tmp_dir();
        let path = dir.join("session.json");
        std::fs::write(&path, "{not json").unwrap();
        assert!(load(&path).is_err());
        let _ = fs::remove_dir_all(dir);
    }
}

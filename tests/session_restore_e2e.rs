//! End-to-end integration tests for the session-restore pipeline.
//!
//! These tests exercise the public surface of `naru::session` from outside the
//! crate: saving a `SessionState`, loading it back, resolving launch-commands,
//! and walking the multi-instance match path. They complement the per-module
//! unit tests in `src/session/*.rs` by verifying the *full* pipeline rather
//! than individual components in isolation.
//!
//! ## What's NOT covered here
//!
//! - The compositor-side hooks (`session_mark_dirty`, `add_window` matcher,
//!   `move_column_to_index`). Those need a live `Naru`/`State` setup that the
//!   existing test infrastructure doesn't expose. They're validated manually
//!   end-to-end (build → restart session → observe).

use std::path::PathBuf;

use naru::session::{
    load, resolve_launch_argv, save_atomic, Placement, SessionManager, SessionState,
    WindowEntry, WorkspaceRef, SCHEMA_VERSION,
};
use naru_config::{LaunchCommand, SessionRestore};

/// Build a fresh per-test scratch directory under `$TMPDIR`. Cleanup is
/// best-effort on success; test failures leave the dir in place for debugging.
fn scratch_dir(label: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "naru-session-e2e-{}-{}-{}",
        label,
        std::process::id(),
        fastrand::u32(..)
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn cleanup(dir: &PathBuf) {
    let _ = std::fs::remove_dir_all(dir);
}

fn config_with_builtins() -> SessionRestore {
    SessionRestore {
        off: false,
        state_path: None,
        launch_commands: vec![
            LaunchCommand {
                app_id: "org.kde.dolphin".into(),
                command: vec!["dolphin".into(), "%s".into()],
                cwd_from_child: false,
            },
            LaunchCommand {
                app_id: "org.kde.konsole".into(),
                command: vec!["konsole".into(), "--workdir".into(), "%s".into()],
                cwd_from_child: true,
            },
        ],
    }
}

fn tiled(col: usize, tile: usize) -> Placement {
    Placement::Tiled {
        column_index: col,
        tile_index: tile,
        width: 0.0,
        height: 0.0,
        is_fullscreen: false,
        is_maximized: false,
    }
}

fn floating(x: f64, y: f64, w: f64, h: f64) -> Placement {
    Placement::Floating {
        x,
        y,
        width: w,
        height: h,
        is_fullscreen: false,
    }
}

fn entry(
    app_id: &str,
    cwd: Option<&str>,
    workspace: WorkspaceRef,
    placement: Placement,
) -> WindowEntry {
    WindowEntry {
        app_id: app_id.into(),
        title: None,
        cwd: cwd.map(PathBuf::from),
        output: None,
        workspace,
        placement,
    }
}

#[test]
fn full_save_load_roundtrip_preserves_diverse_windows() {
    let dir = scratch_dir("full-roundtrip");
    let path = dir.join("session.json");

    let state = SessionState {
        version: SCHEMA_VERSION,
        windows: vec![
            entry(
                "org.kde.dolphin",
                Some("/home/leo/work"),
                WorkspaceRef::Name {
                    name: "code".into(),
                },
                tiled(0, 0),
            ),
            entry(
                "org.kde.konsole",
                Some("/tmp"),
                WorkspaceRef::Index { index: 1 },
                tiled(2, 0),
            ),
            entry(
                "firefox",
                None,
                WorkspaceRef::Index { index: 0 },
                floating(100.0, 50.0, 800.0, 600.0),
            ),
            // Multi-instance — three konsoles on the same workspace, varying cwds.
            entry(
                "org.kde.konsole",
                Some("/var/log"),
                WorkspaceRef::Name {
                    name: "ops".into(),
                },
                tiled(0, 0),
            ),
            entry(
                "org.kde.konsole",
                Some("/etc"),
                WorkspaceRef::Name {
                    name: "ops".into(),
                },
                tiled(1, 0),
            ),
        ],
    };

    save_atomic(&path, &state).expect("save");
    let loaded = load(&path).expect("load").expect("file should exist");
    assert_eq!(state, loaded);
    cleanup(&dir);
}

#[test]
fn resolve_uses_builtin_dolphin_template() {
    let cfg = config_with_builtins();
    let e = entry(
        "org.kde.dolphin",
        Some("/home/leo/proj"),
        WorkspaceRef::Index { index: 0 },
        tiled(0, 0),
    );
    assert_eq!(
        resolve_launch_argv(&e, &cfg),
        vec!["dolphin", "/home/leo/proj"]
    );
}

#[test]
fn resolve_uses_builtin_konsole_template_with_workdir() {
    let cfg = config_with_builtins();
    let e = entry(
        "org.kde.konsole",
        Some("/srv"),
        WorkspaceRef::Index { index: 0 },
        tiled(0, 0),
    );
    assert_eq!(
        resolve_launch_argv(&e, &cfg),
        vec!["konsole", "--workdir", "/srv"]
    );
}

#[test]
fn resolve_falls_back_to_app_id_for_unknown() {
    let cfg = config_with_builtins();
    let e = entry(
        "firefox",
        Some("/should/be/ignored"),
        WorkspaceRef::Index { index: 0 },
        tiled(0, 0),
    );
    // No launch-command for firefox → bare app_id, cwd unused (no %s slot).
    assert_eq!(resolve_launch_argv(&e, &cfg), vec!["firefox"]);
}

#[test]
fn resolve_drops_placeholder_when_cwd_unavailable() {
    let cfg = config_with_builtins();
    let e = entry(
        "org.kde.dolphin",
        None,
        WorkspaceRef::Index { index: 0 },
        tiled(0, 0),
    );
    // %s vanishes from argv when cwd is None — better than passing "" or "/".
    assert_eq!(resolve_launch_argv(&e, &cfg), vec!["dolphin"]);
}

#[test]
fn pending_restore_is_fifo_per_app_id() {
    let dir = scratch_dir("pending-fifo");
    let path = dir.join("session.json");

    // Three konsoles in saved order, distinguishable by cwd.
    let state = SessionState {
        version: SCHEMA_VERSION,
        windows: vec![
            entry(
                "org.kde.konsole",
                Some("/first"),
                WorkspaceRef::Index { index: 0 },
                tiled(0, 0),
            ),
            entry(
                "org.kde.konsole",
                Some("/second"),
                WorkspaceRef::Index { index: 0 },
                tiled(1, 0),
            ),
            entry(
                "org.kde.konsole",
                Some("/third"),
                WorkspaceRef::Index { index: 0 },
                tiled(2, 0),
            ),
        ],
    };
    save_atomic(&path, &state).unwrap();

    let cfg = SessionRestore {
        off: false,
        state_path: Some(path.to_string_lossy().into_owned()),
        launch_commands: vec![],
    };
    let mut sm = SessionManager::new(&cfg).expect("manager");

    // i-th `take_pending_for_app("org.kde.konsole")` returns the i-th saved entry.
    let first = sm.take_pending_for_app("org.kde.konsole").unwrap();
    assert_eq!(first.cwd.as_deref(), Some(std::path::Path::new("/first")));

    let second = sm.take_pending_for_app("org.kde.konsole").unwrap();
    assert_eq!(second.cwd.as_deref(), Some(std::path::Path::new("/second")));

    let third = sm.take_pending_for_app("org.kde.konsole").unwrap();
    assert_eq!(third.cwd.as_deref(), Some(std::path::Path::new("/third")));

    // Queue exhausted.
    assert!(sm.take_pending_for_app("org.kde.konsole").is_none());
    cleanup(&dir);
}

#[test]
fn pending_restore_only_pops_matching_app_id() {
    let dir = scratch_dir("pending-mixed");
    let path = dir.join("session.json");

    let state = SessionState {
        version: SCHEMA_VERSION,
        windows: vec![
            entry(
                "org.kde.dolphin",
                None,
                WorkspaceRef::Index { index: 0 },
                tiled(0, 0),
            ),
            entry(
                "org.kde.konsole",
                None,
                WorkspaceRef::Index { index: 0 },
                tiled(1, 0),
            ),
            entry(
                "org.kde.dolphin",
                None,
                WorkspaceRef::Index { index: 0 },
                tiled(2, 0),
            ),
        ],
    };
    save_atomic(&path, &state).unwrap();

    let cfg = SessionRestore {
        off: false,
        state_path: Some(path.to_string_lossy().into_owned()),
        launch_commands: vec![],
    };
    let mut sm = SessionManager::new(&cfg).expect("manager");

    // Dolphin pops both dolphins in saved order, skipping the konsole between.
    assert!(sm.take_pending_for_app("org.kde.dolphin").is_some());
    assert!(sm.take_pending_for_app("org.kde.dolphin").is_some());
    assert!(sm.take_pending_for_app("org.kde.dolphin").is_none());

    // Konsole still there.
    assert!(sm.take_pending_for_app("org.kde.konsole").is_some());
    assert!(sm.take_pending_for_app("org.kde.konsole").is_none());

    // Unknown app: never matches.
    assert!(sm.take_pending_for_app("nonexistent").is_none());

    cleanup(&dir);
}

#[test]
fn manager_starts_empty_when_no_state_file() {
    let dir = scratch_dir("first-run");
    let path = dir.join("does-not-exist.json");

    let cfg = SessionRestore {
        off: false,
        state_path: Some(path.to_string_lossy().into_owned()),
        launch_commands: vec![],
    };
    let mut sm = SessionManager::new(&cfg).expect("manager");

    // No file → no pending entries; matcher always returns None.
    assert!(sm.take_pending_for_app("anything").is_none());
    cleanup(&dir);
}

#[test]
fn manager_treats_version_mismatch_as_fresh_start() {
    let dir = scratch_dir("version-mismatch");
    let path = dir.join("session.json");

    // Write a future-version state file.
    std::fs::write(
        &path,
        r#"{"version": 999, "windows": [{"app_id": "x",
            "workspace": {"by": "index", "index": 0},
            "placement": {"kind": "tiled", "column_index": 0, "tile_index": 0}}]}"#,
    )
    .unwrap();

    let cfg = SessionRestore {
        off: false,
        state_path: Some(path.to_string_lossy().into_owned()),
        launch_commands: vec![],
    };
    let mut sm = SessionManager::new(&cfg).expect("manager");

    // load() returned Ok(None) → pending_restore is empty, won't try to restore
    // a windowfrom an incompatible schema.
    assert!(sm.take_pending_for_app("x").is_none());
    cleanup(&dir);
}

#[test]
fn off_config_yields_no_manager() {
    let cfg = SessionRestore {
        off: true,
        state_path: Some("/tmp/should-not-be-touched.json".into()),
        launch_commands: vec![],
    };
    assert!(SessionManager::new(&cfg).is_none());
}

#[test]
fn save_then_reopen_simulates_full_session_cycle() {
    // Mimics the actual flow: live state → save → reload → resolve argv.
    // This is the closest single test to the user-visible behavior.
    let dir = scratch_dir("full-cycle");
    let path = dir.join("session.json");
    let cfg = config_with_builtins();

    // Phase: pretend two terminals and a file manager are open, shutdown.
    let live_state = SessionState {
        version: SCHEMA_VERSION,
        windows: vec![
            entry(
                "org.kde.konsole",
                Some("/home/leo/proj"),
                WorkspaceRef::Name {
                    name: "code".into(),
                },
                tiled(0, 0),
            ),
            entry(
                "org.kde.konsole",
                Some("/var/log"),
                WorkspaceRef::Name {
                    name: "ops".into(),
                },
                tiled(0, 0),
            ),
            entry(
                "org.kde.dolphin",
                Some("/home/leo"),
                WorkspaceRef::Name {
                    name: "code".into(),
                },
                tiled(1, 0),
            ),
        ],
    };
    save_atomic(&path, &live_state).unwrap();

    // Phase: compositor restart → manager loads pending entries.
    let restored_cfg = SessionRestore {
        off: false,
        state_path: Some(path.to_string_lossy().into_owned()),
        launch_commands: cfg.launch_commands.clone(),
    };
    let sm = SessionManager::new(&restored_cfg).expect("manager");

    // The respawn pass would iterate sm.pending_restore and resolve each.
    let argvs: Vec<Vec<String>> = sm
        .pending_restore
        .iter()
        .map(|e| resolve_launch_argv(e, &restored_cfg))
        .collect();

    assert_eq!(
        argvs,
        vec![
            vec!["konsole", "--workdir", "/home/leo/proj"],
            vec!["konsole", "--workdir", "/var/log"],
            vec!["dolphin", "/home/leo"],
        ]
    );

    cleanup(&dir);
}

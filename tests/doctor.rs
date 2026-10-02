#![cfg(target_os = "linux")]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! `doctor` through the compiled CLI: which checks fail it, which only warn,
//! and that it reads the root without creating or repairing anything. herdr
//! and gh are fakes; git, ssh and rsync are the real tools.
use std::{fs, os::unix::fs::PermissionsExt, path::{Path, PathBuf}, process::{Command, Output}};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-farm");

fn script(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn doctor(home: &Path, root: &Path) -> (Output, String) {
    let out = Command::new(BIN).env_clear().env("HOME", home).env("PATH", format!("{}:/usr/bin:/bin", home.join("bin").display()))
        .env("HERDR_BIN_PATH", home.join("herdr")).arg("--root").arg(root).arg("doctor").output().unwrap();
    let text = String::from_utf8(out.stdout.clone()).unwrap();
    (out, text)
}

/// A herdr older than the minimum fails `doctor`; missing gh login, a
/// missing root and executor metrics, valid or not, only warn; unreadable
/// retry state fails it. None of it creates the root or rewrites a file.
#[test]
fn doctor_fails_only_on_required_checks_and_changes_nothing() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let root: PathBuf = home.join("root");
    // herdr reports the version in `herdr-version`; gh is installed but logged out.
    script(&home.join("herdr"), "#!/bin/sh\ncase \"$*\" in\n--version) cat \"$HOME/herdr-version\";;\n'session list --json') echo '{\"sessions\":[]}';;\n*) echo '{\"result\":{}}';;\nesac\n");
    fs::create_dir(home.join("bin")).unwrap();
    script(&home.join("bin/gh"), "#!/bin/sh\n[ \"$1\" = --version ] && { echo 'gh version 2'; exit 0; }\necho 'not logged in' >&2\nexit 1\n");

    fs::write(home.join("herdr-version"), "herdr 0.9.0\n").unwrap();
    let (out, text) = doctor(home, &root);
    assert!(!out.status.success(), "{text}");
    assert!(text.contains("[FAIL] herdr: 0.9.0") && text.contains("0.9.1 or later is required"), "{text}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("some checks failed"));
    assert!(!root.exists());

    fs::write(home.join("herdr-version"), "herdr 0.9.1\n").unwrap();
    let (out, text) = doctor(home, &root);
    assert!(out.status.success(), "{text}");
    assert!(text.contains("[ok  ] herdr: 0.9.1"), "{text}");
    assert!(text.contains("[warn] gh auth: not logged in; pull request follow-up will not work"), "{text}");
    assert!(text.contains("[warn] root: does not exist yet") && text.contains(&format!("root:       {}\n", root.display())), "{text}");
    assert!(!text.contains("executor"), "{text}");
    assert!(!root.exists(), "doctor must not create the root");

    // Metrics of a ticker that is not running are advisory, even unreadable ones.
    let new = Command::new(BIN).env_clear().env("HOME", home).arg("--root").arg(&root).args(["new", "demo"]).output().unwrap();
    assert!(new.status.success(), "{}", String::from_utf8_lossy(&new.stderr));
    let metrics = root.join(".ticker-metrics.json");
    fs::write(&metrics, r#"{"control":{"queued":0,"running":1,"high_water":2,"completed":3},"transfer":{"queued":0,"running":0,"high_water":0,"completed":0},"max_queue_delay_ms":40,"uncertain":false}"#).unwrap();
    let (out, text) = doctor(home, &root);
    assert!(out.status.success(), "{text}");
    assert!(text.contains("[warn] ticker: not running"), "{text}");
    assert!(text.contains("[warn] executor: control q=0 r=1 hw=2 done=3; transfer q=0 r=0 hw=0 done=0; delay_ms=40; uncertain=false"), "{text}");
    assert!(text.contains("[ok  ] root: 1 project(s)") && text.contains("[ok  ] project demo: active; never opened"), "{text}");
    fs::write(&metrics, "{broken").unwrap();
    let (out, text) = doctor(home, &root);
    assert!(out.status.success(), "{text}");
    assert!(text.contains("[warn] executor: metrics unreadable"), "{text}");

    // Unreadable retry state fails the project, and is left for repair.
    let state = root.join("demo/.state/ticker.json");
    fs::write(&state, "{broken").unwrap();
    let (out, text) = doctor(home, &root);
    assert!(!out.status.success(), "{text}");
    assert!(text.contains(&format!("[FAIL] project demo: invalid retry state {}", state.display())), "{text}");
    assert_eq!((fs::read(&state).unwrap(), fs::read(&metrics).unwrap()), (b"{broken".to_vec(), b"{broken".to_vec()));
}

/// Replaces `parses_versions`.
///
/// `doctor` reads herdr's version from its first numeric word, ignores a
/// pre-release suffix and compares numerically, not as text; output with no
/// version fails the check.
#[test]
fn doctor_compares_herdr_versions_numerically_and_fails_without_one() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    script(&home.join("herdr"), "#!/bin/sh\ncase \"$*\" in\n--version) cat \"$HOME/herdr-version\";;\n'session list --json') echo '{\"sessions\":[]}';;\n*) echo '{\"result\":{}}';;\nesac\n");
    fs::create_dir(home.join("bin")).unwrap();
    for (reported, line) in [
        ("herdr 0.9.2-preview.3\n", "[ok  ] herdr: 0.9.2 ("),
        ("0.10.0\n", "[ok  ] herdr: 0.10.0 ("),
        ("herdr 0.9.0-preview.9\n", "[FAIL] herdr: 0.9.0 ("),
        ("herdr\n", "[FAIL] herdr: could not read a version from `herdr`"),
    ] {
        fs::write(home.join("herdr-version"), reported).unwrap();
        let (_, text) = doctor(home, &home.join("root"));
        assert!(text.contains(line), "{reported}: {text}");
    }
}

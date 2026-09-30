#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! Task records through the compiled CLI over a disposable migrated project:
//! `task add/rename/show`, each call a new process that reopens the store.
//! Persisted events are read from the store file.
use herdr_projects::{migration, runtime};
use serde_json::Value;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-projects");

struct Project { home: tempfile::TempDir }

impl Project {
    fn new() -> Self {
        let p = Project { home: tempfile::tempdir().unwrap() };
        for command in ["new", "pause"] { assert!(p.cli(&[command, "demo"]).status.success()); }
        let project = p.project();
        migration::apply(&project, &migration::inspect_with_config(&project, &p.home.path().join("owner.toml")).unwrap(), true).unwrap();
        p
    }
    fn project(&self) -> std::path::PathBuf { self.home.path().join("root/demo") }
    fn cli(&self, args: &[&str]) -> Output {
        Command::new(BIN).env_clear().env("HOME", self.home.path()).env("PATH", "/usr/bin:/bin")
            .args(["--root", self.home.path().join("root").to_str().unwrap()]).args(args).output().unwrap()
    }
    fn head(&self) -> u64 { runtime::snapshot(&self.project()).unwrap().head }
    /// Event sequences recorded for `entity`.
    fn events(&self, entity: &str) -> Vec<i64> {
        let db = rusqlite::Connection::open(self.project().join(".state/state.db")).unwrap();
        let mut stmt = db.prepare("SELECT sequence FROM events WHERE entity=?1 ORDER BY sequence").unwrap();
        stmt.query_map([entity], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
    }
    fn show(&self, task: &str) -> Value {
        let out = self.cli(&["task", "demo", "show", task]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap()
    }
    /// A refused command (not a crash) changes no state.
    fn refused(&self, args: &[&str]) {
        let before = runtime::snapshot(&self.project()).unwrap();
        let out = self.cli(args);
        assert!(!out.status.success(), "{args:?} accepted");
        assert!(!String::from_utf8_lossy(&out.stderr).contains("panicked"), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(runtime::snapshot(&self.project()).unwrap(), before, "{args:?} wrote state");
    }
}

/// Replaces `atomic_records_intents_events_reopen_and_history`.
///
/// Each accepted change commits its record and one event at the next
/// sequence; a later process reads back exactly what was committed. A change
/// against a stale head or revision writes nothing.
#[test]
fn task_changes_commit_with_their_events_and_survive_reopening() {
    let p = Project::new();
    let start = p.head();
    assert!(p.cli(&["task", "demo", "add", "t-1", "--title", "first", "--expected-head", &start.to_string()]).status.success());
    assert_eq!(p.head(), start + 1);
    assert_eq!(p.events("t-1"), [start as i64 + 1]);
    let added = p.show("t-1");
    p.refused(&["task", "demo", "add", "t-2", "--title", "stale", "--expected-head", &start.to_string()]);
    p.refused(&["task", "demo", "rename", "t-1", "--title", "stale", "--expected-revision", "2", "--expected-head", &p.head().to_string()]);
    assert_eq!(p.show("t-1"), added);
    assert!(p.cli(&["task", "demo", "rename", "t-1", "--title", "renamed", "--expected-revision", "1", "--expected-head", &p.head().to_string()]).status.success());
    assert_eq!(p.events("t-1"), [start as i64 + 1, start as i64 + 2]);
    let renamed = p.show("t-1");
    assert_ne!(renamed, added);
    assert!(renamed.to_string().contains("renamed"), "{renamed}");
    assert_eq!(p.show("t-1"), renamed, "a new process read different state");
}

/// Replaces `invalid_ids_and_overflow_are_rejected`.
///
/// Path-like or spaced task ids and a revision past the store's range are
/// refused without a write.
#[test]
fn path_like_task_ids_and_out_of_range_revisions_are_refused() {
    let p = Project::new();
    for id in ["../../other", "bad id", "a/b"] {
        p.refused(&["task", "demo", "add", id, "--title", "bad", "--expected-head", &p.head().to_string()]);
    }
    assert!(p.cli(&["task", "demo", "add", "t-1", "--title", "ok", "--expected-head", &p.head().to_string()]).status.success());
    p.refused(&["task", "demo", "rename", "t-1", "--title", "overflow", "--expected-revision", &u64::MAX.to_string(), "--expected-head", &p.head().to_string()]);
    p.refused(&["task", "demo", "rename", "t-1", "--title", "overflow", "--expected-revision", &(u64::MAX - 1).to_string(), "--expected-head", &p.head().to_string()]);
    assert!(runtime::snapshot(&p.project()).unwrap().tasks.iter().all(|t| t.id.as_str() == "t-1"));
}

/// The advisory public read ignores unrelated damaged historical event bodies.
/// Administrative snapshots still diagnose the injected corruption.
#[test]
fn scoped_attempt_token_inventory_ignores_unrelated_corrupt_history() {
    let p = Project::new();
    let out = p.cli(&["task", "demo", "add", "retired", "--title", "Retired task", "--expected-head", &p.head().to_string()]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(p.show("retired")["title"], "Retired task");
    // Establish the scoped-open schema check before injecting a data fault,
    // as the ticker does before a store page is damaged during normal use.
    let control = herdr_projects::store::controlled::ReadControl::new(std::time::Instant::now() + std::time::Duration::from_secs(10), Default::default());
    drop(migration::open_active_scoped(&p.project(), control).unwrap());
    let raw = rusqlite::Connection::open(p.project().join(".state/state.db")).unwrap();
    raw.execute_batch("PRAGMA ignore_check_constraints=ON").unwrap();
    assert!(raw.execute("UPDATE events SET payload='{' WHERE entity='retired'", []).unwrap() > 0);
    drop(raw);
    assert!(runtime::snapshot(&p.project()).is_err());
    let control = herdr_projects::store::controlled::ReadControl::new(std::time::Instant::now() + std::time::Duration::from_secs(10), Default::default());
    let mut store = migration::open_active_scoped(&p.project(), control).unwrap();
    assert!(store.attempt_tokens(&[]).unwrap().entries.is_empty());
    let out = p.cli(&["scheduler", "demo", "inspect"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

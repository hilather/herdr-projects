//! Lane C quality proxies end to end: a real reserved attempt plus planted
//! submissions and pinned-CI runs over a real Git repository, observed with
//! `herdr-projects telemetry <slug> quality collect` and read with `quality report`.

#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.

mod support;

use serde_json::json;
use std::{fs, path::Path, process::Command};
use support::telemetry::*;

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git").current_dir(repo).env_clear().env("PATH", "/usr/bin:/bin")
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"]).args(args).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// Commit the working tree on a fresh branch from `base`, after `change`.
fn candidate(repo: &Path, base: &str, name: &str, change: impl FnOnce()) -> String {
    git(repo, &["checkout", "-q", "-b", name, base]);
    change();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", name]);
    git(repo, &["rev-parse", "HEAD"])
}

fn canonical_bytes(project: &Path) -> Vec<Vec<u8>> {
    ["state.db", "state.db-wal"].iter().map(|name| fs::read(project.join(".state").join(name)).unwrap_or_default()).collect()
}

/// Task `work` (the fixture's attempt): first candidate rejected by pinned CI,
/// a second candidate accepted. `t2`: first candidate accepted, adds tests.
/// `t3`: first candidate accepted but deletes a 4-line test file, so flagged
/// and excluded. `t4`: submitted, not yet verified. M45 = 1/2.
#[test]
fn first_candidate_ci_proxy_and_test_weakening_flag() {
    let f = Fixture::new();
    let repo = f.tmp.path().join("repo");
    fs::create_dir_all(repo.join("tests")).unwrap();
    fs::create_dir_all(repo.join("src")).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("tests/a.rs"), "#[test]\nfn a() {\n    assert!(true);\n}\n").unwrap();
    fs::write(repo.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let w1 = candidate(&repo, &base, "w1", || fs::write(repo.join("src/lib.rs"), "pub fn f() { g() }\n").unwrap());
    let w2 = candidate(&repo, &base, "w2", || fs::write(repo.join("src/lib.rs"), "pub fn f() {}\npub fn g() {}\n").unwrap());
    let c2 = candidate(&repo, &base, "t2", || fs::write(repo.join("tests/b.rs"), "#[test]\nfn b() {}\n").unwrap());
    let c3 = candidate(&repo, &base, "t3", || fs::remove_file(repo.join("tests/a.rs")).unwrap());

    let db = rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap();
    db.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    let hex = |c: char| c.to_string().repeat(64);
    for task in ["t2", "t3", "t4"] {
        db.execute("INSERT INTO tasks(id,revision,state,title) VALUES(?1,1,'running',?1)", [task]).unwrap();
        db.execute("INSERT INTO attempts(id,task_id,revision,state,reservation,termination_observed) VALUES(?1,?2,2,'completed',?1,1)", [format!("{task}-a1"), task.to_owned()]).unwrap();
    }
    let t2 = "t2-a1".to_owned();
    let (t3, t4) = ("t3-a1".to_owned(), "t4-a1".to_owned());
    for (sub, task, attempt, oid, at) in [('1', "work", &f.attempt, &w1, 1_000), ('2', "work", &f.attempt, &w2, 3_000), ('3', "t2", &t2, &c2, 1_000), ('4', "t3", &t3, &c3, 1_000), ('5', "t4", &t4, &c2, 1_000)] {
        db.execute("INSERT INTO result_submissions(submission_id,project_store,idempotency_key,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,artifact_manifest,claimed_checks,created_unix_ms)
            VALUES(?1,'store',?1,?2,'{}',?3,1,?2,?4,?5,?6,?7,'sha1','[]','[]',?8)", rusqlite::params![hex(sub), hex('d'), task, attempt, repo.to_str().unwrap(), base, oid, at]).unwrap();
    }
    // (run, submission, task, attempt, commit, state, at): `work`'s first
    // candidate is rejected, then accepted on a re-run; only the first counts.
    for (run, sub, task, attempt, oid, state, at) in [('a', '1', "work", &f.attempt, &w1, "rejected", 2_000), ('b', '1', "work", &f.attempt, &w1, "accepted", 2_500),
        ('c', '2', "work", &f.attempt, &w2, "accepted", 4_000), ('e', '3', "t2", &t2, &c2, "accepted", 2_000), ('f', '4', "t3", &t3, &c3, "accepted", 2_000)] {
        let accepted = state == "accepted";
        db.execute("INSERT INTO verification_runs(run_id,project_store,idempotency_key,payload_digest,submission_id,task_id,contract_revision,contract_digest,attempt_id,policy_id,policy_digest,
            commit_oid,tree_oid,object_format,memory_fence,isolation,argv,library_manifest,state,reason,exit_status,receipt_digest,store_device,store_inode,created_unix_ms)
            VALUES(?1,'store',?1,?2,?3,?4,1,?2,?5,'ci',?6,?7,?7,'sha1',0,'linux-unshare-user-pid-mount-v1','[\"x\"]','[]',?8,?9,?10,?11,1,1,?12)",
            rusqlite::params![hex(run), hex('d'), hex(sub), task, attempt, hex('9'), oid, state, (!accepted).then_some("cargo test failed"),
                if accepted { 0 } else { 101 }, accepted.then(|| hex('8')), at]).unwrap();
    }
    drop(db);

    let m45 = &f.cli_args(&["quality", "report"]).0["metrics"]["M45"];
    assert_eq!(m45["value"], json!({"status": "unavailable", "reason": "collection_not_run"}), "no sidecar yet: unavailable, not 0");
    assert_eq!((&m45["pending"], &m45["excluded"]["not_collected"]), (&json!(1), &json!(3)));
    // The Codex collect creates the sidecar (and so changes `usage`); the baseline follows it.
    f.cli("collect");
    let attempts = f.cli_args(&["attempts", "--json"]).0;
    let before = f.report();
    assert_eq!(f.cli_args(&["quality", "report"]).0["metrics"]["M45"]["reason"], json!("empty_denominator"));
    let canonical = canonical_bytes(&f.project);

    let (collected, _) = f.cli_args(&["quality", "collect"]);
    assert_eq!(collected, json!({"observed": 3, "flagged": 1, "weakening_unavailable": 0, "deferred": 0}));
    assert_eq!(f.cli_args(&["quality", "collect"]).0, json!({"observed": 0, "flagged": 0, "weakening_unavailable": 0, "deferred": 0}), "settled signals are not re-observed");
    assert_eq!(f.cli_args(&["quality", "status"]).0, json!({"stream": "quality", "version": 1}));

    let after = f.report();
    let m45 = &f.cli_args(&["quality", "report"]).0["metrics"]["M45"];
    assert_eq!((&m45["definition"], &m45["proxy"], &m45["source_trust"]), (&json!("M45.proxy-v1"), &json!(true), &json!("proxy_observed")));
    assert_eq!((&m45["numerator"], &m45["denominator"], &m45["value"], &m45["pending"]), (&json!(1), &json!(2), &json!("1/2"), &json!(1)));
    assert_eq!(m45["excluded"], json!({"test_weakening": 1, "weakening_unavailable": 0, "not_collected": 0}));
    assert_eq!(m45["flagged"], json!([{"task_id": "t3", "submission_id": hex('4'), "attempt_id": "t3-a1", "run_id": hex('f'), "ci_state": "accepted",
        "tests_added_lines": 0, "tests_deleted_lines": 4, "tests_binary_files": 0, "weakening": "flagged"}]));
    let windowed = f.cli_args(&["quality", "report", "--since", "2001"]).0;
    assert_eq!(windowed["metrics"]["M45"]["denominator"], json!(0), "first runs at 2000 are outside the window; the re-run at 2500 does not count");

    // Proxies never affect acceptance: canonical bytes, attempts and M02 are unchanged.
    assert_eq!(canonical_bytes(&f.project), canonical, "state.db is only read");
    assert_eq!(f.cli_args(&["attempts", "--json"]).0, attempts);
    assert_eq!(after["metrics"]["M02"], before["metrics"]["M02"]);
    assert_eq!(after["tasks"], before["tasks"]);

    type Row = (String, String, String, String, Option<i64>, Option<i64>, String, String);
    let rows: Vec<Row> = f.sidecar()
        .prepare("SELECT task_id,run_id,policy_digest,ci_state,tests_added_lines,tests_deleted_lines,weakening,source_trust FROM proxy_signals ORDER BY task_id").unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?))).unwrap().map(Result::unwrap).collect();
    let row = |task: &str, run: char, state: &str, added: i64, deleted: i64, weakening: &str|
        (task.to_owned(), hex(run), hex('9'), state.to_owned(), Some(added), Some(deleted), weakening.to_owned(), "proxy_observed".to_owned());
    assert_eq!(rows, vec![row("t2", 'e', "accepted", 2, 0, "clear"), row("t3", 'f', "accepted", 0, 4, "flagged"), row("work", 'a', "rejected", 0, 0, "clear")]);
    for name in ["telemetry.db", "telemetry.db-wal"] {
        let bytes = fs::read(f.project.join(".state").join(name)).unwrap_or_default();
        assert!(!bytes.windows(6).any(|w| w == b"tests/"), "{name}: counts only, no test path is stored");
    }
}

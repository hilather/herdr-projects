//! Lane C quality proxies end to end: a real reserved attempt plus planted
//! submissions and pinned-CI runs over a real Git repository, observed with
//! `herdr-projects telemetry <slug> quality collect` and read with `quality report`.

#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.

mod support;

use serde_json::json;
use std::{fs, path::Path, process::Command};
use support::telemetry::*;

fn git(repo: &Path, args: &[&str]) -> String { git_at(repo, None, args) }

/// `git` with author and committer dates at `at` (Unix seconds), if given.
fn git_at(repo: &Path, at: Option<i64>, args: &[&str]) -> String {
    let mut command = Command::new("git");
    command.current_dir(repo).env_clear().env("PATH", "/usr/bin:/bin");
    if let Some(at) = at { command.env("GIT_AUTHOR_DATE", format!("@{at} +0000")).env("GIT_COMMITTER_DATE", format!("@{at} +0000")); }
    let out = command
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
    assert_eq!(collected["proxy_signals"], json!({"observed": 3, "flagged": 1, "weakening_unavailable": 0, "deferred": 0}));
    assert_eq!(f.cli_args(&["quality", "collect"]).0["proxy_signals"], json!({"observed": 0, "flagged": 0, "weakening_unavailable": 0, "deferred": 0}), "settled signals are not re-observed");
    assert_eq!(f.cli_args(&["quality", "status"]).0, json!({"stream": "quality", "version": 2}));

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

/// Integrations on `refs/heads/main` (days before now, horizon 7 days):
/// I0 merged at -28 adds 1 line and is backed out at -27 by a commit restoring
/// its parent tree (no trailer); I1 merged at -25 adds 4 lines and is reverted
/// at -24 with a `This reverts commit` trailer; I2 merged at -22 adds 3 lines,
/// one edited at -20 (inside its horizon) and the file deleted by a trailer
/// revert at -10 (outside it); I3 merged at -1 is younger than the horizon.
/// M48 revert rate = 2/3; M47 survival = (0 + 0 + 2)/(1 + 4 + 3) = 2/8, with
/// area churn +1/-6 (I0 -1, I1 -4, I2 +1/-1); I3 censored. At 14 days I2's
/// late revert counts: M48 = 3/3, M47 = 0/8.
#[test]
fn revert_within_horizon_counts_recent_censored() {
    let f = Fixture::new();
    let repo = f.tmp.path().join("outcomes");
    fs::create_dir_all(repo.join("src")).unwrap();
    const DAY: i64 = 86_400;
    let now = unix_ms() / 1000;
    let at = |days: i64| Some(now - days * DAY);
    git(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    git(&repo, &["add", "-A"]);
    git_at(&repo, at(30), &["commit", "-q", "-m", "base"]);
    // A branch commit writing `file` at `days + 1`, merged with --no-ff at `days`.
    let integrate = |name: &str, file: &str, text: &str, days: i64| {
        git(&repo, &["checkout", "-q", "-b", name, "main"]);
        fs::write(repo.join(file), text).unwrap();
        git(&repo, &["add", "-A"]);
        git_at(&repo, at(days + 1), &["commit", "-q", "-m", name]);
        git(&repo, &["checkout", "-q", "main"]);
        git_at(&repo, at(days), &["merge", "-q", "--no-ff", "-m", &format!("merge {name}"), name]);
        git(&repo, &["rev-parse", "HEAD"])
    };
    let i0 = integrate("b0", "src/zero.rs", "zero\n", 28);
    git(&repo, &["rm", "-q", "src/zero.rs"]);
    git_at(&repo, at(27), &["commit", "-q", "-m", "Back out zero"]);
    let i1 = integrate("b1", "src/one.rs", "1\n2\n3\n4\n", 25);
    git_at(&repo, at(24), &["revert", "--no-edit", "-m", "1", &i1]);
    let i2 = integrate("b2", "src/two.rs", "a\nb\nc\n", 22);
    fs::write(repo.join("src/two.rs"), "a\nB\nc\n").unwrap();
    git_at(&repo, at(20), &["commit", "-q", "-am", "edit two"]);
    git(&repo, &["rm", "-q", "src/two.rs"]);
    git_at(&repo, at(10), &["commit", "-q", "-m", &format!("Revert two\n\nThis reverts commit {i2}.")]);
    let i3 = integrate("b3", "src/three.rs", "x\ny\n", 1);

    let db = rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap();
    db.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    let hex = |c: char| c.to_string().repeat(64);
    for (id, oid, days) in [('0', &i0, 28), ('1', &i1, 25), ('2', &i2, 22), ('3', &i3, 1)] {
        db.execute("INSERT INTO integrated_commits(integrated_id,candidate_id,operation_id,repository,ref_name,commit_oid,tree_oid,expected_old_oid,object_format,created_unix_ms)
            VALUES(?1,?1,?1,?2,'refs/heads/main',?3,?4,?5,'sha1',?6)", rusqlite::params![hex(id), repo.to_str().unwrap(), oid,
            git(&repo, &["rev-parse", &format!("{oid}^{{tree}}")]), git(&repo, &["rev-parse", &format!("{oid}^1")]), at(days).unwrap() * 1000]).unwrap();
    }
    drop(db);

    let metrics = f.cli_args(&["quality", "report"]).0["metrics"].clone();
    assert_eq!((&metrics["M47"]["value"], &metrics["M48"]["value"]), (&json!({"status": "unavailable", "reason": "collection_not_run"}), &json!({"status": "unavailable", "reason": "collection_not_run"})));
    assert_eq!(metrics["M46"]["value"], json!({"status": "unavailable", "reason": "no_main_check_producer"}));
    assert_eq!(metrics["flaky_tests"]["value"], json!({"status": "unavailable", "reason": "no_repeat_runs"}));
    f.cli("collect");
    let attempts = f.cli_args(&["attempts", "--json"]).0;
    let canonical = canonical_bytes(&f.project);

    let collected = f.cli_args(&["quality", "collect", "--horizon-days", "7"]).0;
    assert_eq!(collected["integration_outcomes"], json!({"horizon_days": 7, "observed": 3, "censored": 1, "unavailable": 0, "deferred": 0}));
    assert_eq!(f.cli_args(&["quality", "collect", "--horizon-days", "7"]).0["integration_outcomes"]["observed"], json!(0), "settled outcomes are not re-observed");

    let metrics = f.cli_args(&["quality", "report", "--horizon-days", "7"]).0["metrics"].clone();
    let (m47, m48) = (&metrics["M47"], &metrics["M48"]);
    assert_eq!((&m48["name"], &m48["value"], &m48["numerator"], &m48["denominator"]), (&json!("revert_rate_proxy"), &json!("2/3"), &json!(2), &json!(3)));
    assert_eq!((&m48["reverted_by"], &m48["censored"], &m48["not_collected"], &m48["unavailable"]), (&json!({"trailer": 1, "tree_restore": 1}), &json!(1), &json!(0), &json!(0)));
    assert_eq!((&m47["name"], &m47["value"], &m47["numerator"], &m47["denominator"]), (&json!("code_survival_proxy"), &json!("2/8"), &json!(2), &json!(8)));
    assert_eq!((&m47["area_churn"], &m47["censored"]), (&json!({"added_lines": 1, "deleted_lines": 6}), &json!(1)));
    for m in [m47, m48] {
        assert_eq!((&m["proxy"], &m["source_trust"], &m["horizon_days"]), (&json!(true), &json!("proxy_observed"), &json!(7)));
    }
    let windowed = f.cli_args(&["quality", "report", "--horizon-days", "7", "--since", &(at(26).unwrap() * 1000).to_string()]).0;
    assert_eq!((&windowed["metrics"]["M48"]["value"], &windowed["metrics"]["M47"]["value"]), (&json!("1/2"), &json!("2/7")), "I0 (merged at -28) is outside the window");
    assert_eq!(f.cli_args(&["quality", "report", "--horizon-days", "14"]).0["metrics"]["M48"]["not_collected"], json!(3), "each horizon is observed separately");

    f.cli_args(&["quality", "collect", "--horizon-days", "14"]);
    let metrics = f.cli_args(&["quality", "report", "--horizon-days", "14"]).0["metrics"].clone();
    assert_eq!((&metrics["M48"]["value"], &metrics["M47"]["value"], &metrics["M48"]["censored"]), (&json!("3/3"), &json!("0/8"), &json!(1)));
    // The fleet report shows the default 14-day horizon.
    assert_eq!(f.report()["metrics"]["M48"]["value"], json!("3/3"));

    // Proxies never affect acceptance: the canonical store is only read.
    assert_eq!(canonical_bytes(&f.project), canonical, "state.db is only read");
    assert_eq!(f.cli_args(&["attempts", "--json"]).0, attempts);
    for name in ["telemetry.db", "telemetry.db-wal"] {
        let bytes = fs::read(f.project.join(".state").join(name)).unwrap_or_default();
        assert!(!bytes.windows(4).any(|w| w == b"src/"), "{name}: counts only, no path is stored");
    }
}

/// Write the fixture rollout `parts` as session `sid` under `home`, in `cwd`, at `ts_ms`.
fn rollout(home: &Path, sid: &str, parts: &[&str], cwd: &str, ts_ms: i64) {
    let dir = home.join(".codex/sessions/2026/09/28");
    fs::create_dir_all(&dir).unwrap();
    let text = parts.iter().map(|part| fs::read_to_string(Path::new(FIXTURES).join(part)).unwrap()).collect::<String>();
    let ts = jiff::Timestamp::from_millisecond(ts_ms).unwrap().to_string();
    fs::write(dir.join(format!("rollout-2026-09-28T00-00-00-{sid}.jsonl")),
        text.replace("@SID@", sid).replace("@CWD@", cwd).replace("@TS@", &ts).replace("@VERSION@", "0.154.0")).unwrap();
}

/// TM3.8 sequential arms (contracts-quality.md §3). Group G on task `work`
/// seals arms 1 `codex`, 2 `fast` (codex with other arguments) and 3 `claude`
/// while attempt A0 (reserved before G) runs, so A0 is no arm. A reservation
/// on `fast` before arm 1 binds nothing (launch order is fixed); A1 on `codex`
/// becomes arm 1 and A2 on `fast` arm 2; arm 3 never launches. A1's candidate
/// passes verification and A2's fails, yet the operator selects arm 2:
/// selection is not verification. Losers keep their cost: A1 used 1 record
/// (1000 in, 400 cached, 120 out, 80 reasoning, 1120 total) and A2 2 records
/// (1500, 500, 180, 100, 1680), so the arms' total is 2500 in, 900 cached,
/// 300 out, 180 reasoning, 2800 total over 3 records; arm 3 is a failure
/// with no cost of its own, and the winner's 1680 is only a drill-down.
#[test]
fn group_arms_fixed_before_outcomes_and_losers_keep_cost() {
    let f = Fixture::new();
    let db_path = f.project.join(".state/state.db");
    let fast_home = f.tmp.path().join("fast-home");
    let mut fast = codex_profile(&f.config, "codex", "fast", Some(&fast_home));
    fast.arguments_digest = "1".repeat(64);
    plant_profile(&db_path, fast);
    plant_profile(&db_path, codex_profile(&f.config, "claude", "claude", None));
    {
        let mut db = herdr_projects::store::SqliteStore::open(&db_path).unwrap();
        let snapshot = db.read_snapshot(None).unwrap();
        db.set_scheduler_policy(snapshot.head, snapshot.scheduler.unwrap().policy.revision, 1, 5).unwrap();
    }
    let sql = || rusqlite::Connection::open(&db_path).unwrap();
    let count = |table: &str| sql().query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get::<_, i64>(0)).unwrap();

    // `other` differs from `codex` only by name and execution home: the same configuration, refused, nothing written.
    assert!(f.cli_fail(&["quality", "groups", "create", "work", "--arm", "codex", "--arm", "other"]).contains("arm 2 repeats the configuration of an earlier arm"));
    assert_eq!(count("candidate_groups"), 0);
    let created = f.cli_args(&["quality", "groups", "create", "work", "--arm", "codex", "--arm", "fast", "--arm", "claude"]).0["group"].clone();
    let group = created["group_id"].as_str().unwrap().to_owned();
    assert!(group.starts_with("sha256:") && group.len() == 71, "{group}");
    assert_eq!((&created["task_id"], &created["contract_revision"], &created["creator_principal"]), (&json!("work"), &json!(null), &json!("operator:cli")));
    assert_eq!(created["arms"].as_array().unwrap().iter().map(|a| a["arm"].as_i64().unwrap()).collect::<Vec<_>>(), [1, 2, 3]);
    let a0_configuration: String = sql().query_row("SELECT chosen_configuration_id FROM dispatch_decisions WHERE attempt_id=?1", [&f.attempt], |r| r.get(0)).unwrap();
    assert_eq!(created["arms"][0]["configuration_id"], json!(a0_configuration), "arm 1 is the codex configuration");
    assert!(f.cli_fail(&["quality", "groups", "create", "work", "--arm", "fast", "--arm", "claude"]).contains("already has a candidate group"));

    let latest = || sql().query_row("SELECT attempt_id,decided_unix_ms FROM dispatch_decisions ORDER BY rowid DESC LIMIT 1", [], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))).unwrap();
    let bound = || sql().prepare("SELECT arm,attempt_id FROM candidate_arm_attempts ORDER BY arm").unwrap()
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))).unwrap().map(Result::unwrap).collect::<Vec<_>>();
    // Arms are ordinary attempts through the existing launch path, bound in their reservation transaction, in launch order.
    f.readmit("fast");
    let (early, _) = latest();
    assert_eq!(bound(), [], "arm 1 comes first: a `fast` reservation now is no arm, and A0 predates the group");
    f.readmit("codex");
    let (a1, a1_decided) = latest();
    f.readmit("fast");
    let (a2, a2_decided) = latest();
    assert_eq!(bound(), [(1, a1.clone()), (2, a2.clone())]);
    assert_eq!(f.cli_args(&["quality", "groups", "show"]).0["groups"][0]["status"], json!("open"));

    // The worker side, as launches and results would write it: collector bindings, rollouts, candidates and pinned CI.
    let worktree = |attempt: &str| format!("{}/.state/worktrees/{attempt}/repo-00", f.project.display());
    for (attempt, home) in [(&a1, &f.home), (&a2, &fast_home)] {
        sql().execute("INSERT INTO collector_bindings(attempt_id,revision,state,collector,execution_home,unix_ms,source) VALUES(?1,1,'active','codex',?2,?3,'apply_launch_started')",
            rusqlite::params![attempt, home.display().to_string(), unix_ms()]).unwrap();
    }
    rollout(&f.home, "00000000-0000-4000-8000-0000000000a1", &["head.jsonl"], &worktree(&a1), a1_decided + 1_000);
    rollout(&fast_home, "00000000-0000-4000-8000-0000000000a2", &["head.jsonl", "tail.jsonl"], &worktree(&a2), a2_decided + 1_000);
    let hex = |c: char| c.to_string().repeat(64);
    let db = sql();
    db.execute("INSERT INTO task_contracts(task_id,contract_revision,plan_revision,project_store,expected_head,repository,base_oid,object_format,memory_snapshot_id,route,raw_bytes,raw_digest,installed_seq)
        VALUES('work',1,NULL,'store',0,'/repo',?1,'sha1',NULL,'verify_only',x'61',?2,(SELECT max(sequence) FROM events))", rusqlite::params!["b".repeat(40), hex('c')]).unwrap();
    db.execute("INSERT INTO acceptance_policies(task_id,contract_revision,policy_id,body) VALUES('work',1,'ci','cargo test')", []).unwrap();
    for (sub, attempt, oid, at) in [('1', &a1, "1".repeat(40), 1_000), ('2', &a2, "2".repeat(40), 2_000)] {
        db.execute("INSERT INTO result_submissions(submission_id,project_store,idempotency_key,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,artifact_manifest,claimed_checks,created_unix_ms)
            VALUES(?1,'store',?1,?2,'{}','work',1,?2,?3,'/repo',?4,?5,'sha1','[]','[]',?6)", rusqlite::params![hex(sub), hex('d'), attempt, "b".repeat(40), oid, at]).unwrap();
    }
    for (run, sub, attempt, oid, accepted) in [('a', '1', &a1, "1".repeat(40), true), ('b', '2', &a2, "2".repeat(40), false)] {
        db.execute("INSERT INTO verification_runs(run_id,project_store,idempotency_key,payload_digest,submission_id,task_id,contract_revision,contract_digest,attempt_id,policy_id,policy_digest,
            commit_oid,tree_oid,object_format,memory_fence,isolation,argv,library_manifest,state,reason,exit_status,receipt_digest,store_device,store_inode,created_unix_ms)
            VALUES(?1,'store',?1,?2,?3,'work',1,?2,?4,'ci',?2,?5,?5,'sha1',0,'linux-unshare-user-pid-mount-v1','[\"x\"]','[]',?6,?7,?8,?9,1,1,3000)",
            rusqlite::params![hex(run), hex('d'), hex(sub), attempt, oid, if accepted { "accepted" } else { "rejected" }, (!accepted).then_some("cargo test failed"),
                if accepted { 0 } else { 101 }, accepted.then(|| hex('8'))]).unwrap();
    }
    sql().execute("INSERT INTO result_submissions(submission_id,project_store,idempotency_key,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,artifact_manifest,claimed_checks,created_unix_ms)
        VALUES(?1,'store',?1,?2,'{}','work',1,?2,?3,'/repo',?4,?4,'sha1','[]','[]',500)", rusqlite::params![hex('0'), hex('d'), early, "b".repeat(40)]).unwrap();
    drop(db);

    // Arms are fixed: once sealed, and with outcomes in, no arm can be added, changed, removed or rebound.
    for statement in ["INSERT INTO candidate_group_arms(group_id,arm,configuration_id,profile_digest) SELECT group_id,4,configuration_id,profile_digest FROM candidate_group_arms WHERE arm=1",
        "DELETE FROM candidate_group_arms WHERE arm=3", "UPDATE candidate_group_arms SET arm=4 WHERE arm=3",
        "DELETE FROM candidate_arm_attempts WHERE arm=1", "UPDATE candidate_groups SET arm_count=2"] {
        assert!(sql().execute(statement, []).is_err(), "{statement}");
    }
    let before = f.cli_args(&["quality", "groups", "show"]).0;
    assert_eq!(before["groups"][0]["arms"].as_array().unwrap().len(), 3);

    // A selection names a member's candidate only.
    for (args, message) in [(vec!["--arm", "4"], "arm 4 is not a member"), (vec!["--arm", "3"], "arm 3 has no attempt"),
        (vec!["--arm", "1", "--submission", &hex('2')], "is not a candidate of arm 1"), (vec!["--arm", "1", "--submission", &hex('0')], "is not a candidate of arm 1"),
        (vec!["--arm", "2", "--reason", "cheapest"], "unknown selection reason")] {
        let mut command = vec!["quality", "groups", "select", &group];
        command.extend(args);
        assert!(f.cli_fail(&command).contains(message), "{message}");
    }
    assert_eq!(count("candidate_selections"), 0);

    f.cli("collect");
    let usage = |attempt: &str| f.cli_args(&["attempts", "--json"]).0["attempts"].as_array().unwrap().iter().find(|a| a["attempt_id"] == attempt).unwrap()["usage"].clone();
    let a1_usage = json!({"input_tokens": 1000, "cached_input_tokens": 400, "cache_write_input_tokens": 0, "output_tokens": 120, "reasoning_output_tokens": 80, "total_tokens": 1120, "records": 1});
    let a2_usage = json!({"input_tokens": 1500, "cached_input_tokens": 500, "cache_write_input_tokens": 0, "output_tokens": 180, "reasoning_output_tokens": 100, "total_tokens": 1680, "records": 2});
    assert_eq!((usage(&a1), usage(&a2)), (a1_usage.clone(), a2_usage.clone()));

    // Runner-ups are bound arms, distinct and not the winner (arm 3 never launched).
    for (args, message) in [(vec!["--runner-up", "2"], "runner-up arm 2 is the selected arm"), (vec!["--runner-up", "3"], "runner-up arm 3 is not a bound arm"),
        (vec!["--runner-up", "1", "--runner-up", "1"], "runner-up arm 1 is repeated"), (vec!["--runner-up", "first"], "--runner-up first is not an arm number")] {
        let mut command = vec!["quality", "groups", "select", &group, "--arm", "2"];
        command.extend(args);
        assert!(f.cli_fail(&command).contains(message), "{message}");
    }
    assert!(f.cli_fail(&["quality", "groups", "select", &group, "--none", "--runner-up", "1"]).contains("cannot be used with"));
    assert_eq!(count("candidate_selections"), 0);
    let selection = f.cli_args(&["quality", "groups", "select", &group, "--arm", "2", "--runner-up", "1", "--reason", "operator_judgment"]).0["selection"].clone();
    assert_eq!((&selection["outcome"], &selection["arm"], &selection["attempt_id"], &selection["submission_id"]), (&json!("selected"), &json!(2), &json!(a2), &json!(hex('2'))));
    assert_eq!((&selection["selector_kind"], &selection["selector_principal"], &selection["reason"]), (&json!("operator"), &json!("operator:cli"), &json!("operator_judgment")));
    // Arm 2's candidate is rejected but its attempt is still open (it may resubmit), so its arm outcome is pending.
    // The operator's order is recorded: the winner rank 1, the runner-up 2.
    assert_eq!(selection["evidence"], json!([{"arm": 1, "attempt_id": a1, "submission_id": hex('1'), "verification": "accepted", "arm_outcome": "accepted", "rank": 2},
        {"arm": 2, "attempt_id": a2, "submission_id": hex('2'), "verification": "rejected", "arm_outcome": "pending", "rank": 1}]));
    assert!(f.cli_fail(&["quality", "groups", "select", &group, "--none"]).contains("already has a selection"));
    assert!(sql().execute("DELETE FROM candidate_selections", []).is_err(), "a selection is immutable");

    let shown = f.cli_args(&["quality", "groups", "show"]).0["groups"][0].clone();
    assert_eq!((&shown["group_id"], &shown["status"], &shown["selection"]["arm"]), (&json!(group), &json!("closed"), &json!(2)));
    let arms = shown["arms"].as_array().unwrap();
    let summary = arms.iter().map(|a| (a["arm"].clone(), a["attempt_id"].clone(), a["role"].clone(), a["outcome"].clone(), a["candidate"].clone(), a["verification"]["state"].clone()))
        .collect::<Vec<_>>();
    assert_eq!(summary, [(json!(1), json!(a1), json!("not_selected"), json!("candidate"), json!(hex('1')), json!("accepted")),
        (json!(2), json!(a2), json!("selected"), json!("candidate"), json!(hex('2')), json!("rejected")),
        (json!(3), json!(null), json!("not_selected"), json!("failure_no_candidate"), json!(null), json!(null))]);
    assert_eq!((&arms[0]["usage"], &arms[1]["usage"], &arms[2]["usage"]), (&a1_usage, &a2_usage, &json!(null)), "each arm keeps its own attempt's cost");
    assert_eq!(shown["cost"], json!({"arms_launched": 2, "arms_not_launched": 1, "winner_usage": a2_usage,
        "arms_total": {"input_tokens": 2500, "cached_input_tokens": 900, "cache_write_input_tokens": 0, "output_tokens": 300, "reasoning_output_tokens": 180, "total_tokens": 2800, "records": 3}}));
    assert_eq!(shown["integration_hold"], json!({"enforced": true, "integrated_without_selection": []}));
    // Selection moved no cost and changed no outcome: the loser's usage and every attempt record are as before.
    assert_eq!(usage(&a1), a1_usage);
    assert_eq!(arms[0]["verification"], before["groups"][0]["arms"][0]["verification"]);
}

/// Groups planted through `quality groups create`, with the worker side of
/// each arm (launch, result, pinned CI) written as the controller would.
#[derive(Default)]
struct Planted {
    /// `task` (revision 1) or `task/r<revision>` -> group ID.
    ids: std::collections::BTreeMap<String, String>,
    /// Configuration IDs of the first group's arms.
    configurations: Vec<String>,
    /// `(key, arm)` -> submission ID.
    subs: std::collections::BTreeMap<(String, i64), String>,
    submission: u32,
}

impl Planted {
    /// A group for `task`'s contract `revision`; arm outcomes are acc(epted),
    /// rej(ected), none (terminal, no candidate), run(ning) or unb(ound).
    fn group(&mut self, f: &Fixture, task: &str, revision: i64, profiles: &[&str], outcomes: &[&str]) {
        let db_path = f.project.join(".state/state.db");
        let sql = || { let db = rusqlite::Connection::open(&db_path).unwrap(); db.execute_batch("PRAGMA foreign_keys=OFF").unwrap(); db };
        let key = if revision == 1 { task.to_owned() } else { format!("{task}/r{revision}") };
        let db = sql();
        if revision == 1 { db.execute("INSERT INTO tasks(id,revision,state,title) VALUES(?1,1,'running',?1)", [task]).unwrap(); }
        db.execute("INSERT INTO task_contracts(task_id,contract_revision,plan_revision,project_store,expected_head,repository,base_oid,object_format,memory_snapshot_id,route,raw_bytes,raw_digest,installed_seq)
            VALUES(?1,?4,NULL,'store',0,'/repo',?2,'sha1',NULL,'verify_only',CAST(?1 AS BLOB),?3,(SELECT max(sequence) FROM events))",
            rusqlite::params![task, "b".repeat(40), format!("{:x<64}", key), revision]).unwrap();
        db.execute("INSERT INTO acceptance_policies(task_id,contract_revision,policy_id,body) VALUES(?1,?2,'ci','cargo test')", rusqlite::params![task, revision]).unwrap();
        drop(db);
        let mut args = vec!["quality", "groups", "create", task];
        for profile in profiles { args.extend(["--arm", profile]); }
        let group = f.cli_args(&args).0["group"].clone();
        if self.configurations.is_empty() { self.configurations = group["arms"].as_array().unwrap().iter().map(|a| a["configuration_id"].as_str().unwrap().to_owned()).collect(); }
        let id = group["group_id"].as_str().unwrap().to_owned();
        let db = sql();
        for (i, outcome) in outcomes.iter().enumerate().filter(|(_, o)| **o != "unb") {
            let (arm, attempt) = (i as i64 + 1, if revision == 1 { format!("{task}-a{}", i + 1) } else { format!("{task}-r{revision}-a{}", i + 1) });
            db.execute("INSERT INTO attempts(id,task_id,revision,state,reservation,termination_observed) VALUES(?1,?2,1,?3,?1,?4)",
                rusqlite::params![attempt, task, if *outcome == "run" { "running" } else { "completed" }, i64::from(*outcome != "run")]).unwrap();
            db.execute("INSERT INTO dispatch_decisions(attempt_id,task_id,task_revision,contract_revision,chosen_configuration_id,eligible,chooser_kind,chooser_principal,reason_codes,decided_unix_ms)
                VALUES(?1,?2,1,?4,?3,'[\"x\"]','operator','operator:cli','[\"x\"]',1)", rusqlite::params![attempt, task, group["arms"][i]["configuration_id"].as_str().unwrap(), revision]).unwrap();
            db.execute("INSERT INTO candidate_arm_attempts(group_id,arm,attempt_id,bound_unix_ms,source) VALUES(?1,?2,?3,1,'admit_prepared')", rusqlite::params![id, arm, attempt]).unwrap();
            if matches!(*outcome, "none" | "run") { continue; }
            self.submission += 1;
            let submission = self.submission;
            let sub = format!("{submission:064x}");
            self.subs.insert((key.clone(), arm), sub.clone());
            db.execute("INSERT INTO result_submissions(submission_id,project_store,idempotency_key,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,artifact_manifest,claimed_checks,created_unix_ms)
                VALUES(?1,'store',?1,?2,'{}',?3,?8,?2,?4,'/repo',?5,?6,'sha1','[]','[]',?7)", rusqlite::params![sub, "d".repeat(64), task, attempt, "b".repeat(40), format!("{submission:040x}"), i64::from(submission), revision]).unwrap();
            let accepted = *outcome == "acc";
            db.execute("INSERT INTO verification_runs(run_id,project_store,idempotency_key,payload_digest,submission_id,task_id,contract_revision,contract_digest,attempt_id,policy_id,policy_digest,
                commit_oid,tree_oid,object_format,memory_fence,isolation,argv,library_manifest,state,reason,exit_status,receipt_digest,store_device,store_inode,created_unix_ms)
                VALUES(?1,'store',?1,?2,?3,?4,?11,?2,?5,'ci',?2,?6,?6,'sha1',0,'linux-unshare-user-pid-mount-v1','[\"x\"]','[]',?7,?8,?9,?10,1,1,100)",
                rusqlite::params![format!("{:064x}", 1000 + submission), "d".repeat(64), sub, task, attempt, format!("{submission:040x}"), if accepted { "accepted" } else { "rejected" },
                    (!accepted).then_some("cargo test failed"), if accepted { 0 } else { 101 }, accepted.then(|| "8".repeat(64)), revision]).unwrap();
        }
        self.ids.insert(key, id);
    }
}

/// TM3.8 selection and paired outcomes (contracts-quality.md §4). Arms are
/// configurations A `codex`, B `fast`, C `claude`; each group is on its own
/// task. Arm outcomes: acc(epted), rej(ected), none (terminal, no candidate),
/// run(ning, no candidate), unb(ound).
///
/// | group | arms | outcomes          | selection                       |
/// |-------|------|-------------------|---------------------------------|
/// | g0    | ABC  | acc acc unb       | operator arm 1 (A)              |
/// | g1    | AB   | acc rej           | rule -> A                       |
/// | g2    | AB   | rej acc           | rule -> B                       |
/// | g3    | AB   | none rej          | operator arm 2 (B, rejected)    |
/// | g4    | AB   | acc acc           | rule -> A (launch order), ranks |
/// | g5    | AB   | rej rej           | rule -> no selection            |
/// | g6    | ABC  | rej rej acc       | rule -> C                       |
/// | g7    | ABC  | acc acc run       | operator arm 1 (A)              |
/// | g8    | AB   | acc acc           | judge -> B                      |
/// | g9    | AB   | acc none          | operator --none                 |
/// | g10   | AB   | run unb           | open (the rule refuses)         |
///
/// M41 over the 10 closed groups: A selected in g0 g1 g4 g7 = 4/10, B in g2
/// g3 g8 = 3/10, C 1 of 3 groups (below 10: insufficient data). A vs B: 4 wins,
/// 3 losses, ties 2 no-selection + 1 other (g6) = 4/7. M42 A-B uses verified
/// outcomes, not selections: both accepted g0 g4 g7 g8, A only g1 g9, B only
/// g2, neither g3 g5 g6: (2 - 1)/10 = +10 points; A-C has 3 groups, g7
/// pending, n = 2: insufficient data.
#[test]
fn win_rate_and_paired_difference_over_closed_groups() {
    let f = Fixture::new();
    let db_path = f.project.join(".state/state.db");
    let mut fast = codex_profile(&f.config, "codex", "fast", Some(&f.tmp.path().join("fast-home")));
    fast.arguments_digest = "1".repeat(64);
    plant_profile(&db_path, fast);
    plant_profile(&db_path, codex_profile(&f.config, "claude", "claude", None));

    let empty = f.report()["metrics"].clone();
    assert_eq!((&empty["M41"]["value"], &empty["M42"]["value"]), (&json!({"status": "unavailable", "reason": "no_closed_groups"}), &json!({"status": "unavailable", "reason": "no_closed_groups"})));

    // (task, arm profiles, arm outcomes)
    let groups: [(&str, &[&str], &[&str]); 11] = [
        ("g0", &["codex", "fast", "claude"], &["acc", "acc", "unb"]), ("g1", &["codex", "fast"], &["acc", "rej"]), ("g2", &["codex", "fast"], &["rej", "acc"]),
        ("g3", &["codex", "fast"], &["none", "rej"]), ("g4", &["codex", "fast"], &["acc", "acc"]), ("g5", &["codex", "fast"], &["rej", "rej"]),
        ("g6", &["codex", "fast", "claude"], &["rej", "rej", "acc"]), ("g7", &["codex", "fast", "claude"], &["acc", "acc", "run"]),
        ("g8", &["codex", "fast"], &["acc", "acc"]), ("g9", &["codex", "fast"], &["acc", "none"]), ("g10", &["codex", "fast"], &["run", "unb"])];
    let mut planted = Planted::default();
    for (task, profiles, outcomes) in groups { planted.group(&f, task, 1, profiles, outcomes); }
    let Planted { ids, configurations, subs, .. } = planted;
    let sub = |task: &str, arm: i64| subs[&(task.to_owned(), arm)].clone();
    let (a, b, c) = (configurations[0].clone(), configurations[1].clone(), configurations[2].clone());
    let select = |task: &str, args: &[&str]| {
        let mut command = vec!["quality", "groups", "select", &ids[task]];
        command.extend(args);
        f.cli_args(&command).0["selection"].clone()
    };

    // The rule decides only on settled arms, in launch order: g10's arm 1 is still running.
    assert!(f.cli_fail(&["quality", "groups", "select", &ids["g10"], "--rule"]).contains("arm 1 is pending"));
    assert!(f.cli_fail(&["quality", "groups", "select", &ids["g1"], "--rule", "--reason", "operator_judgment"]).contains("cannot be used with"));
    for (task, winner, submission, reason) in [("g1", Some(1), Some(("g1", 1)), "first_passing_verification"), ("g2", Some(2), Some(("g2", 2)), "first_passing_verification"),
        ("g4", Some(1), Some(("g4", 1)), "first_passing_verification"), ("g5", None, None, "none_acceptable"), ("g6", Some(3), Some(("g6", 3)), "first_passing_verification")] {
        let s = select(task, &["--rule"]);
        assert_eq!((&s["selector_kind"], &s["selector_principal"], &s["reason"]), (&json!("rule"), &json!("rule:first_accepted_in_launch_order.v1"), &json!(reason)), "{task}");
        assert_eq!((&s["arm"], &s["submission_id"]), (&json!(winner), &json!(submission.map(|(t, a)| sub(t, a)))), "{task}");
    }
    // A tie goes to launch order; every accepted arm is ranked, the winner first.
    let g4 = f.cli_args(&["quality", "groups", "show"]).0["groups"][4]["selection"]["evidence"].clone();
    assert_eq!(g4, json!([{"arm": 1, "attempt_id": "g4-a1", "submission_id": sub("g4", 1), "verification": "accepted", "arm_outcome": "accepted", "rank": 1},
        {"arm": 2, "attempt_id": "g4-a2", "submission_id": sub("g4", 2), "verification": "accepted", "arm_outcome": "accepted", "rank": 2}]));

    // Operator selections; g3 selects a rejected candidate: selection is not verification.
    select("g0", &["--arm", "1", "--reason", "operator_judgment"]);
    select("g3", &["--arm", "2"]);
    select("g7", &["--arm", "1"]);
    select("g9", &["--none", "--reason", "none_acceptable"]);

    // A judge sees g8's candidates blind, in the recorded presentation order, and names one.
    let presented = f.cli_args(&["quality", "groups", "present", &ids["g8"]]).0;
    let text = presented.to_string();
    for hidden in ["g8-a1", "g8-a2", &a, &b, "codex", "fast"] { assert!(!text.contains(hidden), "{hidden} is not shown to a judge"); }
    let candidates = presented["candidates"].as_array().unwrap();
    assert_eq!(candidates.iter().map(|c| c["position"].as_i64().unwrap()).collect::<Vec<_>>(), [1, 2]);
    let position = |sub: &str| candidates.iter().find(|c| c["submission_id"] == sub).unwrap()["position"].clone();
    // The judge names its runner-up by presented submission and may record its own configuration.
    let (s1, s2) = (sub("g8", 1), sub("g8", 2));
    let base = ["quality", "groups", "select", &ids["g8"], "--judge", "blind-1", "--submission", &s2];
    let judge_fail = |extra: &[&str]| f.cli_fail(&[&base[..], extra].concat());
    assert!(judge_fail(&["--judge-configuration", "gpt-judge"]).contains("is not a configuration ID"));
    assert!(judge_fail(&["--runner-up", &s2]).contains("runner-up arm 2 is the selected arm"));
    assert!(judge_fail(&["--runner-up", &sub("g4", 1)]).contains("is not a presented candidate"));
    assert!(f.cli_fail(&["quality", "groups", "select", &ids["g8"], "--arm", "1", "--judge-configuration", &c]).contains("--judge"));
    let s = f.cli_args(&[&base[..], &["--runner-up", &s1, "--judge-configuration", &c]].concat()).0["selection"].clone();
    assert_eq!((&s["selector_kind"], &s["selector_principal"], &s["reason"], &s["arm"]), (&json!("judge"), &json!("judge:blind-1"), &json!("judge_preference"), &json!(2)));
    assert_eq!(s["evidence"], json!([
        {"arm": 1, "attempt_id": "g8-a1", "submission_id": s1, "verification": "accepted", "arm_outcome": "accepted", "presented": position(&s1), "judge_configuration_id": c, "rank": 2},
        {"arm": 2, "attempt_id": "g8-a2", "submission_id": s2, "verification": "accepted", "arm_outcome": "accepted", "presented": position(&s2), "judge_configuration_id": c, "rank": 1}]));
    assert!(f.cli_fail(&["quality", "groups", "present", &ids["g8"]]).contains("already has a selection"));

    let report = f.cli_args(&["quality", "groups", "report"]).0;
    let registry = |unit: &str| json!({"value": 10, "unit": unit, "source": "registry.v1"});
    assert_eq!(report["min_sample"], registry("closed_groups_containing_both"));
    let (m41, m42) = (&report["metrics"]["M41"], &report["metrics"]["M42"]);
    assert_eq!((&m41["closed_groups"], &m41["open_groups"], &m41["definition"]), (&json!(10), &json!(1), &json!("M41.v1")));
    assert_eq!((&m41["min_sample"], &m42["min_sample"]), (&registry("closed_groups"), &registry("closed_groups_containing_both")));
    let cell = |m: &serde_json::Value, selector: &str, c: &str| m["by_selector"][selector]["configurations"].as_array().unwrap().iter().find(|x| x["configuration_id"] == c).unwrap().clone();
    let insufficient = json!({"status": "unavailable", "reason": "insufficient_data"});
    assert_eq!(cell(m41, "all", &a), json!({"configuration_id": a, "groups": 10, "selected": 4, "no_selection": 2, "other_selected": 4, "value": "4/10"}));
    assert_eq!(cell(m41, "all", &b), json!({"configuration_id": b, "groups": 10, "selected": 3, "no_selection": 2, "other_selected": 5, "value": "3/10"}));
    assert_eq!(cell(m41, "all", &c), json!({"configuration_id": c, "groups": 3, "selected": 1, "no_selection": 0, "other_selected": 2, "value": insufficient}), "3 groups: counts shown, never a rate");
    let pair = |list: &serde_json::Value, x: &str, y: &str| list.as_array().unwrap().iter().find(|p| p["a"] == x && p["b"] == y).unwrap().clone();
    assert_eq!(pair(&m41["by_selector"]["all"]["head_to_head"], &a, &b),
        json!({"a": a, "b": b, "groups": 10, "wins": 4, "losses": 3, "ties": {"no_selection": 2, "other_selected": 1}, "value": "4/7"}));
    assert_eq!(pair(&m41["by_selector"]["all"]["head_to_head"], &b, &a)["value"], json!("3/7"));
    let short = |c: &str| c[..19].to_owned();
    let mut shown = [(a.clone(), "4/10"), (b.clone(), "3/10")];
    shown.sort();
    assert_eq!(m41["value"], json!(shown.iter().map(|(c, v)| format!("{}={v}", short(c))).collect::<Vec<_>>().join(" ")));
    // The interval resamples the ten tasks (g0..g9 in task order; per-task a - b: 0 +1 -1 0 0 0 0 0 0 +1) with
    // the registry seed. Expected ranks 25 and 975 of 1000 draws are from an independent Python SplitMix64
    // re-implementation of the contracts-quality.md §4 method (exact Fraction sort).
    let interval = |lower: &str, upper: &str, lower_difference: &str, upper_difference: &str, clusters: u32| json!({"method": "percentile_bootstrap.v1", "resample": "task",
        "clusters": clusters, "iterations": 1000, "seed": "0x4d34325f626f6f74", "level": "0.95", "lower": lower, "upper": upper, "lower_difference": lower_difference,
        "upper_difference": upper_difference, "unit": "percentage_points", "task_family": {"status": "unavailable", "reason": "no_task_family_data"}, "source": "registry.v1"});
    assert_eq!(pair(&m42["pairs"], &a, &b), json!({"a": a, "b": b, "groups": 10, "pending": 0, "n": 10, "both_accepted": 4, "a_only": 2, "b_only": 1,
        "neither": 3, "difference": "1/10", "value": 10.0, "unit": "percentage_points", "uncertainty": interval("-20.00", "40.00", "-2/10", "4/10", 10)}));
    assert_eq!((&pair(&m42["pairs"], &b, &a)["value"], &pair(&m42["pairs"], &b, &a)["uncertainty"]), (&json!(-10.0), &interval("-40.00", "20.00", "-4/10", "2/10", 10)));
    assert_eq!(m42["estimator"], json!({"method": "percentile_bootstrap.v1", "resample": "task", "task_family": {"status": "unavailable", "reason": "no_task_family_data"}}));
    let ac = pair(&m42["pairs"], &a, &c);
    assert_eq!((&ac["groups"], &ac["pending"], &ac["n"], &ac["a_only"], &ac["b_only"], &ac["value"], &ac["uncertainty"]), (&json!(3), &json!(1), &json!(2), &json!(1), &json!(1), &insufficient, &insufficient));
    let (lo, hi) = if a < b { (&a, &b) } else { (&b, &a) };
    assert_eq!(m42["value"], json!(format!("{}-{}={}pp", short(lo), short(hi), if lo == &a { "10.0" } else { "-10.0" })));

    // Selector type is a dimension; with a lower threshold every cell shows its rate.
    let low = f.cli_args(&["quality", "groups", "report", "--min-groups", "1"]).0["metrics"]["M41"].clone();
    assert_eq!(low["min_sample"], json!({"value": 1, "unit": "closed_groups", "source": "override", "registry": {"value": 10, "source": "registry.v1"}}));
    let rates = |selector: &str| [&a, &b, &c].map(|x| low["by_selector"][selector]["configurations"].as_array().unwrap().iter().find(|y| y["configuration_id"] == *x).map(|y| y["value"].clone()));
    assert_eq!(rates("rule"), [Some(json!("2/5")), Some(json!("1/5")), Some(json!("1/1"))]);
    assert_eq!(rates("operator"), [Some(json!("2/4")), Some(json!("1/4")), Some(json!("0/2"))]);
    assert_eq!(rates("judge"), [Some(json!("0/1")), Some(json!("1/1")), None]);
    assert_eq!(pair(&low["by_selector"]["rule"]["head_to_head"], &a, &b)["ties"], json!({"no_selection": 1, "other_selected": 1}));

    // The fleet report carries both at the default threshold; a window with no selections is unavailable, never 0.
    let fleet = f.report()["metrics"].clone();
    assert_eq!((&fleet["M41"]["value"], &fleet["M42"]["value"]), (&m41["value"], &m42["value"]));
    let later = (unix_ms() + 86_400_000).to_string();
    let windowed = f.cli_args(&["quality", "groups", "report", "--since", &later]).0["metrics"].clone();
    assert_eq!((&windowed["M41"]["value"], &windowed["M42"]["closed_groups"]), (&json!({"status": "unavailable", "reason": "no_closed_groups"}), &json!(0)));
}

/// M42's interval resamples whole tasks (contracts-quality.md §4): task t1 has
/// two groups (contract revisions 1 and 2), so its two paired groups move
/// together. Arms A `codex`, B `fast`, C `claude`.
///
/// | group | arms | outcomes     | a - b | selection                           |
/// |-------|------|--------------|-------|-------------------------------------|
/// | t1/r1 | AB   | acc rej      | +1    | operator arm 1                      |
/// | t1/r2 | AB   | acc rej      | +1    | rule -> A                           |
/// | t2    | ABC  | rej acc acc  | -1    | operator arm 2, runner-ups 3 then 1 |
/// | t3    | AB   | acc acc      | 0     | judge -> A, runner-up B             |
/// | t4    | AB   | none none    | 0     | operator --none                     |
///
/// A-B: n = 5, difference (2 - 1)/5 = +20 points; clusters in task order
/// t1 (2/2), t2 (-1/1), t3 (0/1), t4 (0/1). The registry minimum (10) hides
/// the value and interval; `--min-groups 5` shows both, labelled `override`.
/// Expected ranks 25 and 975 of 1000 draws come from an independent Python
/// SplitMix64 re-implementation (exact Fraction sort): -3/4 and 6/7. Resampling
/// the five groups instead would give -2/5 and 4/5.
#[test]
fn paired_interval_resamples_tasks_and_selections_record_runner_ups() {
    let f = Fixture::new();
    let db_path = f.project.join(".state/state.db");
    let mut fast = codex_profile(&f.config, "codex", "fast", Some(&f.tmp.path().join("fast-home")));
    fast.arguments_digest = "1".repeat(64);
    plant_profile(&db_path, fast);
    plant_profile(&db_path, codex_profile(&f.config, "claude", "claude", None));
    let mut planted = Planted::default();
    planted.group(&f, "t1", 1, &["codex", "fast"], &["acc", "rej"]);
    planted.group(&f, "t1", 2, &["codex", "fast"], &["acc", "rej"]);
    planted.group(&f, "t2", 1, &["codex", "fast", "claude"], &["rej", "acc", "acc"]);
    planted.group(&f, "t3", 1, &["codex", "fast"], &["acc", "acc"]);
    planted.group(&f, "t4", 1, &["codex", "fast"], &["none", "none"]);
    let Planted { ids, configurations, subs, .. } = planted;
    let (a, b) = (configurations[0].clone(), configurations[1].clone());
    let select = |key: &str, args: &[&str]| {
        let mut command = vec!["quality", "groups", "select", &ids[key]];
        command.extend(args);
        f.cli_args(&command).0["selection"].clone()
    };
    select("t1", &["--arm", "1"]);
    assert_eq!(select("t1/r2", &["--rule"])["arm"], json!(1));
    // Three arms, ranked by the operator: winner 2, then 3, then 1.
    let t2 = select("t2", &["--arm", "2", "--runner-up", "3", "--runner-up", "1"]);
    assert_eq!(t2["evidence"].as_array().unwrap().iter().map(|e| (e["arm"].clone(), e["rank"].clone())).collect::<Vec<_>>(),
        [(json!(1), json!(3)), (json!(2), json!(1)), (json!(3), json!(2))]);
    let t3 = select("t3", &["--judge", "blind-2", "--submission", &subs[&("t3".to_owned(), 1)], "--runner-up", &subs[&("t3".to_owned(), 2)]]);
    assert_eq!((&t3["evidence"][0]["rank"], &t3["evidence"][1]["rank"], &t3["evidence"][0]["judge_configuration_id"]), (&json!(1), &json!(2), &json!(null)));
    // No winner, no ranks: the operator's --none evidence is unchanged.
    assert_eq!(select("t4", &["--none"])["evidence"], json!([
        {"arm": 1, "attempt_id": "t4-a1", "submission_id": null, "verification": "no_candidate", "arm_outcome": "no_candidate"},
        {"arm": 2, "attempt_id": "t4-a2", "submission_id": null, "verification": "no_candidate", "arm_outcome": "no_candidate"}]));

    let pair = |min: Option<&str>, x: &str, y: &str| {
        let mut args = vec!["quality", "groups", "report"];
        if let Some(min) = min { args.extend(["--min-groups", min]); }
        let report = f.cli_args(&args).0;
        let p = report["metrics"]["M42"]["pairs"].as_array().unwrap().iter().find(|p| p["a"] == x && p["b"] == y).unwrap().clone();
        (report["metrics"]["M42"]["min_sample"].clone(), p)
    };
    let insufficient = json!({"status": "unavailable", "reason": "insufficient_data"});
    let (sample, ab) = pair(None, &a, &b);
    assert_eq!(sample, json!({"value": 10, "unit": "closed_groups_containing_both", "source": "registry.v1"}));
    assert_eq!((&ab["n"], &ab["difference"], &ab["value"], &ab["uncertainty"]), (&json!(5), &json!("1/5"), &insufficient, &insufficient), "5 groups: counts only");
    let (_, ab6) = pair(Some("6"), &a, &b);
    assert_eq!((&ab6["value"], &ab6["uncertainty"]), (&insufficient, &insufficient));
    let (sample, ab) = pair(Some("5"), &a, &b);
    assert_eq!(sample, json!({"value": 5, "unit": "closed_groups_containing_both", "source": "override", "registry": {"value": 10, "source": "registry.v1"}}));
    let interval = |lower: &str, upper: &str, lower_difference: &str, upper_difference: &str| json!({"method": "percentile_bootstrap.v1", "resample": "task",
        "clusters": 4, "iterations": 1000, "seed": "0x4d34325f626f6f74", "level": "0.95", "lower": lower, "upper": upper, "lower_difference": lower_difference,
        "upper_difference": upper_difference, "unit": "percentage_points", "task_family": {"status": "unavailable", "reason": "no_task_family_data"}, "source": "registry.v1"});
    assert_eq!((&ab["n"], &ab["a_only"], &ab["b_only"], &ab["value"]), (&json!(5), &json!(2), &json!(1), &json!(20.0)));
    assert_eq!(ab["uncertainty"], interval("-75.00", "85.71", "-3/4", "6/7"));
    assert_eq!(pair(Some("5"), &b, &a).1["uncertainty"], interval("-85.71", "75.00", "-6/7", "3/4"));
    // Deterministic: the same rows and seed give the same interval.
    assert_eq!(pair(Some("5"), &a, &b).1, ab);
}

/// TM3.5 regression: an arm's worker cannot elevate its own candidate. The
/// store refuses a `worker:*` principal, an attempt's identity and an import
/// as creator or selector (a judge named after an attempt too), and `quality
/// groups create|select` refuse a worker execution context (`HOME` is a
/// retained profile's execution home), writing nothing; the owner then selects.
#[test]
fn a_worker_cannot_create_a_group_or_select_its_own_arm() {
    let f = Fixture::new();
    let db_path = f.project.join(".state/state.db");
    let mut fast = codex_profile(&f.config, "codex", "fast", Some(&f.tmp.path().join("fast-home")));
    fast.arguments_digest = "1".repeat(64);
    plant_profile(&db_path, fast);
    let mut planted = Planted::default();
    planted.group(&f, "g", 1, &["codex", "fast"], &["rej", "acc"]);
    let group = planted.ids["g"].clone();
    let mut store = herdr_projects::store::SqliteStore::open(&db_path).unwrap();
    let choice = herdr_projects::store::SelectionChoice::Arm { arm: 1, submission: None, runner_up: vec![] };
    for principal in ["worker:g-a1", "g-a1", "import:bot"] {
        let err = format!("{:?}", store.select_candidate(&group, &choice, "operator_judgment", principal, unix_ms()).unwrap_err());
        assert!(err.contains("cannot select a candidate"), "{principal}: {err}");
        let err = format!("{:?}", store.create_candidate_group("work", &["codex".into(), "fast".into()], principal, unix_ms()).unwrap_err());
        assert!(err.contains("cannot create a candidate group"), "{principal}: {err}");
    }
    let sub = planted.subs[&("g".to_owned(), 1)].clone();
    assert!(format!("{:?}", store.select_candidate_by_judge(&group, &sub, "g-a1", None, &[], unix_ms()).unwrap_err()).contains("cannot select a candidate"));
    drop(store);
    let as_worker = |args: &[&str]| Command::new(BIN).env_clear().env("HOME", f.tmp.path().join("fast-home")).env("PATH", "/usr/bin:/bin")
        .args(["--root", f.root.to_str().unwrap(), "telemetry", "demo", "quality", "groups"]).args(args).output().unwrap();
    for args in [vec!["select", group.as_str(), "--arm", "1"], vec!["create", "work", "--arm", "codex", "--arm", "fast"]] {
        let out = as_worker(&args);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success() && err.contains("`quality groups` records the project owner (operator:cli) and refuses to run inside a worker execution context: HOME is a worker execution home"), "{args:?}: {err}");
    }
    let selections = || rusqlite::Connection::open(&db_path).unwrap().query_row("SELECT count(*) FROM candidate_selections", [], |r| r.get::<_, i64>(0)).unwrap();
    assert_eq!(selections(), 0);
    // `show` stays readable anywhere; the owner selects.
    assert!(as_worker(&["show"]).status.success());
    assert_eq!(f.cli_args(&["quality", "groups", "select", &group, "--rule"]).0["selection"]["arm"], json!(2));
    assert_eq!(selections(), 1);
}

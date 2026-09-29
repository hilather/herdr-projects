//! Telemetry sidecar end to end: a real reserved attempt, hand-written Codex
//! rollouts under its execution home, and `herdr-projects telemetry` on the CLI.

#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.

mod support;
#[path = "../src/store/test_schema.rs"]
mod test_schema;

use herdr_projects::store::SqliteStore;
use std::{fs, path::{Path, PathBuf}, process::Command};
use support::telemetry::*;

fn attempt_usage(report: &serde_json::Value) -> serde_json::Value { report["attempts"][0]["usage"].clone() }
fn unavailable(reason: &str) -> serde_json::Value { serde_json::json!({"status": "unavailable", "reason": reason}) }

/// Gate: `0.154.0` is certified only by the live run (card S5,
/// docs/telemetry/codex-live-0.154.0.md); no test hook certifies it.
#[test]
fn codex_usage_binds_and_sums_exactly() {
    let f = Fixture::new();
    let path = f.rollout(&f.home, SID, &["head.jsonl"], &f.worktree(), f.decided + 1_000, "0.154.0");
    f.cli("collect");
    let tail = fs::read_to_string(Path::new(FIXTURES).join("tail.jsonl")).unwrap();
    let ts = jiff::Timestamp::from_millisecond(f.decided + 1_000).unwrap().to_string();
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    std::io::Write::write_all(&mut file, tail.replace("@SID@", SID).replace("@CWD@", &f.worktree()).replace("@TS@", &ts).as_bytes()).unwrap();
    f.cli("collect");
    let (report, _) = f.cli("collect");
    // Cached input is a subset of input and reasoning a subset of output: neither is added.
    assert_eq!(attempt_usage(&report), serde_json::json!({"input_tokens": 1500, "cached_input_tokens": 500, "cache_write_input_tokens": 0,
        "output_tokens": 180, "reasoning_output_tokens": 100, "total_tokens": 1680, "records": 2}));
    assert_eq!(f.usage(), [(1, DIGEST_1.into(), 1, None, Some(1000), Some(400), Some(120), Some(80), Some(1120)),
        (2, DIGEST_2.into(), 1, None, Some(500), Some(100), Some(60), Some(20), Some(560))]);
    let discrepancies = f.sidecar().prepare("SELECT kind,summed_total,reported_total FROM codex_discrepancy").unwrap()
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))).unwrap().map(Result::unwrap).collect::<Vec<_>>();
    assert_eq!(discrepancies, [("token_count_total".to_owned(), 1680, 900)]);
}

#[test]
fn collect_twice_is_idempotent() {
    let f = Fixture::new();
    let path = f.rollout(&f.home, SID, &["head.jsonl"], &f.worktree(), f.decided + 1_000, "0.154.0");
    let (first, _) = f.cli("collect");
    assert_eq!(first["collected"]["records"], 1);
    let rows = f.usage();
    assert_eq!(rows.iter().map(|r| (r.0, r.1.clone())).collect::<Vec<_>>(), [(1, DIGEST_1.to_owned())]);
    let (second, _) = f.cli("collect");
    assert_eq!(second["collected"]["records"], 0, "nothing new past the offset");
    assert_eq!(f.usage(), rows);
    let tail = fs::read_to_string(Path::new(FIXTURES).join("tail.jsonl")).unwrap();
    let ts = jiff::Timestamp::from_millisecond(f.decided + 1_000).unwrap().to_string();
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    std::io::Write::write_all(&mut file, tail.replace("@SID@", SID).replace("@CWD@", &f.worktree()).replace("@TS@", &ts).as_bytes()).unwrap();
    let (third, _) = f.cli("collect");
    assert_eq!(third["collected"]["records"], 1, "only the appended record");
    assert_eq!(f.usage().iter().map(|r| (r.0, r.1.clone())).collect::<Vec<_>>(), [(1, DIGEST_1.to_owned()), (2, DIGEST_2.to_owned())]);
    assert_eq!(f.binding(), ("bound".to_owned(), Some(f.attempt.clone())));
    // Turns and rate limits are metadata: stored once each, used_percent as a decimal string.
    let turns = f.sidecar().prepare("SELECT turn_id,model,effort,duration_ms,time_to_first_token_ms FROM codex_turns ORDER BY turn_id").unwrap()
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?))).unwrap().map(Result::unwrap).collect::<Vec<_>>();
    assert_eq!(turns, [("turn-1".into(), "gpt-5.5".into(), "high".into(), 4200, 350), ("turn-2".into(), "gpt-5.5".into(), "medium".into(), 1800, 200)]);
    let limits = f.sidecar().prepare("SELECT ordinal,limit_id,used_percent,window_minutes,resets_at,plan_type FROM codex_rate_limits ORDER BY ordinal").unwrap()
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?, r.get::<_, String>(5)?))).unwrap().map(Result::unwrap).collect::<Vec<_>>();
    assert_eq!(limits, [(1, "codex".into(), "37.5".into(), 300, 1790003600, "pro".into()), (2, "codex".into(), "42.5".into(), 300, 1790003600, "pro".into())]);
    let (fourth, _) = f.cli("collect");
    assert_eq!(fourth["collected"]["records"], 0);
    assert_eq!((f.count("codex_usage"), f.count("codex_turns"), f.count("codex_rate_limits"), f.count("codex_quarantine")), (2, 2, 2, 0));
}

#[test]
fn rewritten_record_is_quarantined() {
    let f = Fixture::new();
    let path = f.rollout(&f.home, SID, &["head.jsonl", "tail.jsonl"], &f.worktree(), f.decided + 1_000, "0.154.0");
    f.cli("collect");
    // The same file rewritten (new inode) with a different second record.
    let text = fs::read_to_string(&path).unwrap().replace("\"resp-2\"", "\"resp-2b\"");
    let replacement = path.with_extension("tmp");
    fs::write(&replacement, text).unwrap();
    fs::rename(&replacement, &path).unwrap();
    let (report, _) = f.cli("collect");
    assert_eq!(f.usage().iter().map(|r| r.1.clone()).collect::<Vec<_>>(), [DIGEST_1, DIGEST_2], "the first row is kept");
    let quarantine = f.sidecar().query_row("SELECT session_id,ordinal,first_digest,new_digest FROM codex_quarantine", [],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))).unwrap();
    assert_eq!(quarantine, (SID.to_owned(), 2, DIGEST_2.to_owned(), DIGEST_2B.to_owned()));
    assert_eq!(f.count("codex_quarantine"), 1);
    assert_eq!(attempt_usage(&report), unavailable("quarantined"));
}

#[test]
fn uncertified_version_keeps_no_counters() {
    let f = Fixture::new();
    f.rollout(&f.home, SID, &["head.jsonl", "tail.jsonl"], &f.worktree(), f.decided + 1_000, "0.999.0");
    let (report, _) = f.cli("collect");
    assert_eq!(f.usage(), [(1, DIGEST_1.into(), 0, Some("cli_version_uncertified".into()), None, None, None, None, None),
        (2, DIGEST_2.into(), 0, Some("cli_version_uncertified".into()), None, None, None, None, None)]);
    let source = f.sidecar().query_row("SELECT cli_version,records FROM rollout_sources", [], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))).unwrap();
    assert_eq!(source, ("0.999.0".to_owned(), 2));
    assert_eq!(f.count("codex_discrepancy"), 0);
    assert_eq!(attempt_usage(&report), unavailable("cli_version_uncertified"));
    assert_eq!(report["sessions"][0]["certified"], false);
}

impl Fixture {
    /// Rewrite the sidecar as a binary that did not yet certify the version
    /// stored it (`uncertified_version_keeps_no_counters`): NULL counters, no
    /// reported totals, no discrepancies.
    fn as_if_collected_uncertified(&self) {
        self.sidecar().execute_batch("UPDATE codex_usage SET accepted=0,reason='cli_version_uncertified',cache_write_input_tokens=NULL,
            cached_input_tokens=NULL,input_tokens=NULL,output_tokens=NULL,reasoning_output_tokens=NULL,total_tokens=NULL;
            UPDATE rollout_sources SET thread_usage=NULL,token_count_usage=NULL; DELETE FROM codex_discrepancy;").unwrap();
    }
}

/// Found by the 0.154.0 live run: rows collected while the version was still
/// uncertified are re-read from their rollout once it is certified, without
/// double counting; until then they are unavailable, never 0.
#[test]
fn records_collected_before_certification_are_reread() {
    let f = Fixture::new();
    f.rollout(&f.home, SID, &["head.jsonl", "tail.jsonl"], &f.worktree(), f.decided + 1_000, "0.154.0");
    f.cli("collect");
    f.as_if_collected_uncertified();
    assert_eq!(attempt_usage(&f.cli("usage").0), unavailable("cli_version_uncertified"), "a read re-reads nothing");
    let (report, _) = f.cli("collect");
    assert_eq!((&report["collected"]["records"], &report["collected"]["reevaluated"]), (&0.into(), &2.into()));
    // 1000 + 500 input, 400 + 100 cached, 120 + 60 output, 80 + 20 reasoning, 1120 + 560 total.
    let sums = serde_json::json!({"input_tokens": 1500, "cached_input_tokens": 500, "cache_write_input_tokens": 0,
        "output_tokens": 180, "reasoning_output_tokens": 100, "total_tokens": 1680, "records": 2});
    assert_eq!(attempt_usage(&report), sums);
    let rows = [(1, DIGEST_1.into(), 1, None, Some(1000), Some(400), Some(120), Some(80), Some(1120)),
        (2, DIGEST_2.into(), 1, None, Some(500), Some(100), Some(60), Some(20), Some(560))];
    assert_eq!(f.usage(), rows);
    assert_eq!(f.sidecar().query_row("SELECT kind,summed_total,reported_total FROM codex_discrepancy", [],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))).unwrap(), ("token_count_total".to_owned(), 1680, 900));
    assert_eq!(f.binding(), ("bound".to_owned(), Some(f.attempt.clone())));
    assert_eq!(metric(&f.report(), "M08")["value"], 1500);
    let (again, _) = f.cli("collect");
    assert_eq!((&again["collected"]["records"], &again["collected"]["reevaluated"]), (&0.into(), &0.into()));
    assert_eq!(attempt_usage(&again), sums, "no double count");
    assert_eq!(outcome_usage(&f), sums);
    assert_eq!(f.usage(), rows);
    assert_eq!((f.count("codex_usage"), f.count("codex_turns"), f.count("codex_rate_limits"), f.count("codex_quarantine")), (2, 2, 2, 0));
}

#[test]
fn uncertified_records_without_their_rollout_stay_unavailable() {
    let f = Fixture::new();
    let path = f.rollout(&f.home, SID, &["head.jsonl", "tail.jsonl"], &f.worktree(), f.decided + 1_000, "0.154.0");
    f.cli("collect");
    f.as_if_collected_uncertified();
    fs::remove_file(path).unwrap();
    let (report, _) = f.cli("collect");
    let expected = serde_json::json!({"status": "unavailable", "reason": "cli_version_uncertified", "detail": "rollout_unavailable"});
    assert_eq!(attempt_usage(&report), expected);
    assert_eq!(report["sessions"][0]["reevaluation"], "rollout_unavailable");
    assert_eq!(outcome_usage(&f), expected);
    assert_eq!(f.usage().iter().map(|r| (r.2, r.3.as_deref(), r.8)).collect::<Vec<_>>(), [(0, Some("cli_version_uncertified"), None); 2]);
    assert_eq!(metric(&f.report(), "M08")["value"], unavailable("no_certified_source"));
}

#[test]
fn partial_last_line_waits() {
    let f = Fixture::new();
    let path = f.rollout(&f.home, SID, &["head.jsonl", "tail.jsonl"], &f.worktree(), f.decided + 1_000, "0.154.0");
    let full = fs::read(&path).unwrap();
    let cut = full.windows(8).position(|w| w == b"\"resp-2\"").unwrap();
    fs::write(&path, &full[..cut]).unwrap();
    let (first, _) = f.cli("collect");
    assert_eq!(first["collected"]["records"], 1);
    assert_eq!(f.usage().len(), 1);
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    std::io::Write::write_all(&mut file, &full[cut..]).unwrap();
    let (second, _) = f.cli("collect");
    assert_eq!(second["collected"]["records"], 1, "ingested once completed");
    assert_eq!(f.usage().iter().map(|r| (r.0, r.1.clone())).collect::<Vec<_>>(), [(1, DIGEST_1.to_owned()), (2, DIGEST_2.to_owned())]);
    assert_eq!(f.count("codex_quarantine"), 0);
}

#[test]
fn invariant_violation_is_not_accepted() {
    let f = Fixture::new();
    let path = f.rollout(&f.home, SID, &["head.jsonl", "tail.jsonl"], &f.worktree(), f.decided + 1_000, "0.154.0");
    // total 561 != input 500 + output 60.
    let text = fs::read_to_string(&path).unwrap().replacen("\"total_tokens\":560", "\"total_tokens\":561", 1);
    fs::write(&path, text).unwrap();
    f.cli("collect");
    let rows = f.usage();
    assert_eq!((rows[1].0, rows[1].2, rows[1].3.as_deref(), rows[1].8), (2, 0, Some("invariant_violation"), None));
    assert_ne!(rows[0].3.as_deref(), Some("invariant_violation"));
}

#[test]
fn rollout_before_decision_or_elsewhere_is_unbound() {
    let cases: [(&str, fn(&Fixture) -> (PathBuf, String, i64)); 3] = [
        ("earlier", |f| (f.home.clone(), f.worktree(), f.decided - 60_000)),
        ("cwd-outside", |f| (f.home.clone(), format!("{}/repo", f.project.display()), f.decided + 1_000)),
        ("other-home", |f| (f.tmp.path().canonicalize().unwrap().join("other-home"), f.worktree(), f.decided + 1_000)),
    ];
    for (name, case) in cases {
        let f = Fixture::new();
        let (home, cwd, ts) = case(&f);
        f.rollout(&home, SID, &["head.jsonl"], &cwd, ts, "0.154.0");
        let (report, _) = f.cli("collect");
        assert_eq!(f.binding(), ("unbound".to_owned(), None), "{name}");
        assert_eq!(attempt_usage(&report), unavailable("not_bound"), "{name}");
    }
}

#[test]
fn content_never_persists() {
    let f = Fixture::new();
    let path = f.rollout(&f.home, SID, &["head.jsonl"], &f.worktree(), f.decided + 1_000, "0.154.0");
    let mut output = f.cli("collect").1;
    // Hold a reader across the next collect so its frames stay in the WAL.
    let reader = f.sidecar();
    let _ = reader.query_row("SELECT count(*) FROM codex_usage", [], |r| r.get::<_, i64>(0)).unwrap();
    let tail = fs::read_to_string(Path::new(FIXTURES).join("tail.jsonl")).unwrap();
    let ts = jiff::Timestamp::from_millisecond(f.decided + 1_000).unwrap().to_string();
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    std::io::Write::write_all(&mut file, tail.replace("@SID@", SID).replace("@CWD@", &f.worktree()).replace("@TS@", &ts).as_bytes()).unwrap();
    output.extend(f.cli("collect").1);
    output.extend(f.cli("usage").1);
    let state = f.project.join(".state");
    let wal = fs::read(state.join("telemetry.db-wal")).unwrap();
    assert!(!wal.is_empty(), "the second collect wrote through the WAL");
    for name in ["telemetry.db", "telemetry.db-wal", "telemetry.db-shm"] {
        assert!(!contains_canary(&fs::read(state.join(name)).unwrap()), "{name}");
    }
    assert!(!contains_canary(&output), "{}", String::from_utf8_lossy(&output));
    assert_eq!(f.count("codex_usage"), 2, "the collects did read both parts");
    drop(reader);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(fs::metadata(state.join("telemetry.db")).unwrap().permissions().mode() & 0o777, 0o600);
}

fn metric(report: &serde_json::Value, id: &str) -> serde_json::Value { report["metrics"][id].clone() }

/// Contracts §6 worked example, planted row by row into a fresh canonical store:
/// t1 verify_only verified; t2 verified and integrated; t3 verified, integration
/// blocked; t4 failed; t5 queued. Attempts 1, 2, 1, 2, 1; none has a decision.
#[test]
fn golden_acceptance_and_amplification() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("root/demo");
    fs::create_dir_all(project.join(".state")).unwrap();
    let db_path = project.join(".state/state.db");
    drop(SqliteStore::create(&db_path).unwrap());
    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    let oid = "a".repeat(40);
    let hex = |c: char| c.to_string().repeat(64);
    for (task, state, route, attempts) in [("t1", "succeeded", Some("verify_only"), 1), ("t2", "succeeded", Some("verify_then_integrate"), 2),
        ("t3", "blocked", Some("verify_then_integrate"), 1), ("t4", "failed", None, 2), ("t5", "queued", None, 1)] {
        db.execute("INSERT INTO tasks(id,revision,state,title) VALUES(?1,1,?2,?1)", [task, state]).unwrap();
        for n in 1..=attempts {
            db.execute("INSERT INTO attempts(id,task_id,revision,state,reservation,termination_observed) VALUES(?1,?2,2,'completed',?1,1)", [format!("{task}-a{n}"), task.to_owned()]).unwrap();
        }
        if let Some(route) = route {
            db.execute("INSERT INTO task_contracts(task_id,contract_revision,project_store,expected_head,repository,base_oid,object_format,route,raw_bytes,raw_digest,installed_seq)
                VALUES(?1,1,'store',1,'/repo',?2,'sha1',?3,x'7b7d',?4,1)", rusqlite::params![task, oid, route, hex('c')]).unwrap();
        }
    }
    for (task, attempt, sub, result) in [("t1", "t1-a1", '1', '4'), ("t2", "t2-a2", '2', '5'), ("t3", "t3-a1", '3', '6')] {
        db.execute("INSERT INTO result_submissions(submission_id,project_store,idempotency_key,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,artifact_manifest,claimed_checks,created_unix_ms)
            VALUES(?1,'store',?1,?2,'{}',?3,1,?2,?4,'/repo',?5,?5,'sha1','[]','[]',1000)", rusqlite::params![hex(sub), hex('d'), task, attempt, oid]).unwrap();
        db.execute("INSERT INTO verified_results(result_id,run_id,submission_id,commit_oid,tree_oid,object_format,policy_digest,receipt_digest,isolation,memory_fence,created_unix_ms)
            VALUES(?1,?1,?2,?3,?3,'sha1',?4,?4,'linux-unshare-user-pid-mount-v1',0,2000)", rusqlite::params![hex(result), hex(sub), oid, hex('e')]).unwrap();
    }
    for (operation, result, state) in [("op-t2", '5', "integrated"), ("op-t3", '6', "blocked")] {
        db.execute("INSERT INTO integration_operations(operation_id,project_store,idempotency_key,payload_digest,repository,ref_name,expected_old_oid,verified_result_id,state,generation,object_format,checks_passed,created_unix_ms)
            VALUES(?1,'store',?1,?2,'/repo','refs/heads/main',?3,?4,?5,1,'sha1',1,3000)", rusqlite::params![operation, hex('f'), oid, hex(result), state]).unwrap();
    }
    db.execute("INSERT INTO integrated_commits(integrated_id,candidate_id,operation_id,repository,ref_name,commit_oid,tree_oid,expected_old_oid,object_format,created_unix_ms)
        VALUES(?1,?1,'op-t2','/repo','refs/heads/main',?2,?2,?2,'sha1',4000)", rusqlite::params![hex('9'), oid]).unwrap();
    drop(db);
    let out = Command::new(BIN).env_clear().env("HOME", tmp.path()).env("PATH", "/usr/bin:/bin")
        .args(["--root", tmp.path().join("root").to_str().unwrap(), "telemetry", "demo", "report", "--json"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["tasks"], serde_json::json!({"accepted": 2, "open": 2, "succeeded_without_evidence": 0, "terminal": 3}));
    let m02 = metric(&report, "M02");
    assert_eq!((&m02["definition"], &m02["numerator"], &m02["denominator"], &m02["value"]), (&"M02.slice-v1".into(), &2.into(), &3.into(), &"2/3".into()));
    let m07 = metric(&report, "M07");
    assert_eq!((&m07["definition"], &m07["numerator"], &m07["denominator"], &m07["value"]), (&"M07.slice-v1".into(), &5.into(), &2.into(), &"5/2".into()));
    assert_eq!(m07["attempts_without_decision"], 5, "counted and flagged");
    for id in ["M31", "M32", "M33"] {
        assert_eq!(metric(&report, id)["value"], unavailable("attention_not_collected"), "{id}");
    }
    let text = Command::new(BIN).env_clear().env("HOME", tmp.path()).env("PATH", "/usr/bin:/bin")
        .args(["--root", tmp.path().join("root").to_str().unwrap(), "telemetry", "demo", "report", "--text"]).output().unwrap();
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(text.lines().any(|l| l.starts_with("M02 ") && l.contains("2/3")), "{text}");
    assert!(text.lines().any(|l| l.starts_with("M31 ") && l.contains("n/a")), "{text}");
}

/// Gate like `codex_usage_binds_and_sums_exactly`: counters exist only because
/// the live run certified codex 0.154.0; no test hook certifies it.
#[test]
fn usage_metrics_follow_certified_sources() {
    let f = Fixture::new();
    f.rollout(&f.home, SID, &["head.jsonl", "tail.jsonl"], &f.worktree(), f.decided + 1_000, "0.154.0");
    plant_profile(&f.project.join(".state/state.db"), codex_profile(&f.config, "claude", "claude", None));
    f.readmit("claude");
    f.readmit("other");
    f.cancel_reserved();
    f.cli("collect");
    let report = f.report();
    let m08 = metric(&report, "M08");
    assert_eq!((&m08["definition"], &m08["value"]), (&"M08.slice-v1".into(), &1500.into()));
    let m09 = metric(&report, "M09");
    assert_eq!((&m09["value"], &m09["reasoning_output_tokens"]), (&180.into(), &100.into()), "reasoning is a subset, not added");
    let m13 = metric(&report, "M13");
    assert_eq!((&m13["numerator"], &m13["denominator"], &m13["value"], &m13["adapter_absent"]), (&1.into(), &2.into(), &"1/2".into(), &1.into()));
    assert_eq!(m13["incomplete"], serde_json::json!({"not_bound": 1}));
    let m15 = metric(&report, "M15");
    assert_eq!((&m15["numerator"], &m15["denominator"], &m15["value"]), (&2.into(), &2.into(), &"2/2".into()));
}

#[test]
fn no_source_is_unavailable_not_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("root/demo");
    fs::create_dir_all(project.join(".state")).unwrap();
    drop(SqliteStore::create(&project.join(".state/state.db")).unwrap());
    let out = Command::new(BIN).env_clear().env("HOME", tmp.path()).env("PATH", "/usr/bin:/bin")
        .args(["--root", tmp.path().join("root").to_str().unwrap(), "telemetry", "demo", "report", "--json"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    for id in ["M02", "M07"] {
        let m = metric(&report, id);
        assert_eq!((&m["value"], &m["reason"]), (&serde_json::Value::Null, &"empty_denominator".into()), "{id}");
    }
    for id in ["M08", "M09", "M15"] {
        assert_eq!(metric(&report, id)["value"], unavailable("no_certified_source"), "{id}");
    }
    assert_eq!(metric(&report, "M13")["value"], unavailable("collection_not_run"));
    assert_eq!(metric(&report, "M40")["decisions"], serde_json::json!([]));
    assert!(!project.join(".state/telemetry.db").exists(), "the report writes nothing");
}

#[test]
fn quota_headroom_at_dispatch() {
    let f = Fixture::new();
    f.rollout(&f.home, SID, &["head.jsonl"], &f.worktree(), f.decided - 60_000, "0.154.0");
    f.cli("collect");
    let report = f.report();
    assert_eq!(metric(&report, "M40")["decisions"], serde_json::json!([{"age_ms": 60000, "attempt_id": f.attempt, "decided_unix_ms": f.decided,
        "limit_id": "codex", "value": "62.5", "window_minutes": 300}]));
    // Rate limits are metadata, kept for an uncertified version; counters are not.
    assert_eq!(metric(&report, "M08")["value"], unavailable("no_certified_source"));

    let f = Fixture::new();
    f.rollout(&f.home, SID, &["head.jsonl"], &f.worktree(), f.decided + 1_000, "0.154.0");
    f.cli("collect");
    assert_eq!(metric(&f.report(), "M40")["decisions"], serde_json::json!([{"attempt_id": f.attempt, "decided_unix_ms": f.decided,
        "value": unavailable("no_observation")}]));
}

fn outcome_usage(f: &Fixture) -> serde_json::Value { f.cli_args(&["attempts", "--json"]).0["attempts"][0]["usage"].clone() }

/// Contracts §4 `usage` in `telemetry attempts`: the certified bound sums, else
/// the reason, the same as `usage` reports for that attempt.
#[test]
fn attempts_show_bound_usage_or_its_reason() {
    let f = Fixture::new();
    f.rollout(&f.home, SID, &["head.jsonl", "tail.jsonl"], &f.worktree(), f.decided + 1_000, "0.154.0");
    assert_eq!(outcome_usage(&f), unavailable("collection_not_run"));
    assert!(!f.project.join(".state/telemetry.db").exists(), "attempts creates no sidecar");
    f.cli("collect");
    // 1000 + 500 input, 400 + 100 cached, 120 + 60 output, 80 + 20 reasoning, 1120 + 560 total.
    assert_eq!(outcome_usage(&f), serde_json::json!({"input_tokens": 1500, "cached_input_tokens": 500, "cache_write_input_tokens": 0,
        "output_tokens": 180, "reasoning_output_tokens": 100, "total_tokens": 1680, "records": 2}));
    let text = Command::new(BIN).env_clear().env("HOME", f.tmp.path().join("home")).env("PATH", "/usr/bin:/bin")
        .args(["--root", f.root.to_str().unwrap(), "telemetry", "demo", "attempts"]).output().unwrap();
    assert!(String::from_utf8(text.stdout).unwrap().trim_end().ends_with("usage=in=1500 out=180 total=1680"));

    let f = Fixture::new();
    f.rollout(&f.home, SID, &["head.jsonl"], &f.worktree(), f.decided - 60_000, "0.154.0");
    f.cli("collect");
    assert_eq!(outcome_usage(&f), unavailable("not_bound"));

    let f = Fixture::new();
    f.rollout(&f.home, SID, &["head.jsonl", "tail.jsonl"], &f.worktree(), f.decided + 1_000, "0.999.0");
    f.cli("collect");
    assert_eq!(outcome_usage(&f), unavailable("cli_version_uncertified"));

    let f = Fixture::new();
    let path = f.rollout(&f.home, SID, &["head.jsonl", "tail.jsonl"], &f.worktree(), f.decided + 1_000, "0.154.0");
    f.cli("collect");
    let replacement = path.with_extension("tmp");
    fs::write(&replacement, fs::read_to_string(&path).unwrap().replace("\"resp-2\"", "\"resp-2b\"")).unwrap();
    fs::rename(&replacement, &path).unwrap();
    f.cli("collect");
    assert_eq!(outcome_usage(&f), unavailable("quarantined"));
}

/// Every file under `dir` with its length and modification time.
fn tree(dir: &Path) -> Vec<(PathBuf, u64, std::time::SystemTime)> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let meta = fs::symlink_metadata(&path).unwrap();
        if meta.is_dir() { out.extend(tree(&path)); } else { out.push((path, meta.len(), meta.modified().unwrap())); }
    }
    out.sort();
    out
}

fn names(dir: &Path) -> Vec<PathBuf> { tree(dir).into_iter().map(|(path, ..)| path).collect() }

/// `pane fleet` as the plugin popup runs it (stdin closed: the hold-open prompt returns).
fn fleet_pane(f: &Fixture) -> String {
    fs::write(f.project.join("PROJECT.md"), "# demo\n").unwrap();
    let out = Command::new(BIN).env_clear().env("HOME", f.tmp.path().join("home")).env("PATH", "/usr/bin:/bin")
        .args(["--root", f.root.to_str().unwrap(), "pane", "fleet"]).stdin(std::process::Stdio::null()).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

/// Contracts §0 read-only opens: `attempts`, `usage`, `report` and the fleet
/// pane write nothing under `.state`, with or without a live sidecar writer.
#[test]
fn reads_leave_state_untouched() {
    let f = Fixture::new();
    let path = f.rollout(&f.home, SID, &["head.jsonl"], &f.worktree(), f.decided + 1_000, "0.154.0");
    f.cli("collect");
    let state = f.project.join(".state");
    let before = tree(&state);
    assert!(!before.iter().any(|(p, ..)| p.to_string_lossy().ends_with("-wal") || p.to_string_lossy().ends_with("-shm")), "{before:?}");
    f.cli_args(&["attempts", "--json"]);
    f.cli("usage");
    f.report();
    let pane = fleet_pane(&f);
    assert!(pane.contains("M08 input_tokens 1000\n"), "{pane}");
    assert_eq!(tree(&state), before, "no reader writes or creates a file");

    // A live writer (the ticker) keeps the sidecar's WAL open: readers see its
    // committed frames through the existing `-shm` and still create nothing.
    let writer = f.sidecar();
    let _ = writer.query_row("SELECT count(*) FROM codex_usage", [], |r| r.get::<_, i64>(0)).unwrap();
    let tail = fs::read_to_string(Path::new(FIXTURES).join("tail.jsonl")).unwrap();
    let ts = jiff::Timestamp::from_millisecond(f.decided + 1_000).unwrap().to_string();
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    std::io::Write::write_all(&mut file, tail.replace("@SID@", SID).replace("@CWD@", &f.worktree()).replace("@TS@", &ts).as_bytes()).unwrap();
    f.cli("collect");
    let live = names(&state);
    assert!(live.iter().any(|p| p.ends_with("telemetry.db-wal")), "{live:?}");
    assert_eq!(metric(&f.report(), "M08")["value"], 1500, "the WAL's committed frames are read");
    f.cli_args(&["attempts", "--json"]);
    f.cli("usage");
    let pane = fleet_pane(&f);
    assert!(pane.contains("M08 input_tokens 1500\n"), "{pane}");
    assert_eq!(names(&state), live);
    drop(writer);
}

fn contains_canary(bytes: &[u8]) -> bool {
    let lower = bytes.to_ascii_lowercase();
    lower.windows(6).any(|w| w == b"canary")
}

/// Card S0: a sidecar written before streams existed (`user_version` 2, no
/// `telemetry_streams`) reads unchanged and is not upgraded by a read; the next
/// collect upgrades it in place, idempotently, to stream `codex=2` with
/// byte-identical `usage`; a stream newer than this binary is refused.
#[test]
fn sidecar_streams_upgrade_v2_store() {
    let f = Fixture::new();
    f.rollout(&f.home, SID, &["head.jsonl"], &f.worktree(), f.decided + 1_000, "0.154.0");
    f.cli("collect");
    f.sidecar().execute_batch("DROP TABLE telemetry_streams").unwrap();
    let streams = |f: &Fixture| f.sidecar().prepare("SELECT stream,version FROM telemetry_streams ORDER BY stream").unwrap()
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))).unwrap().map(Result::unwrap).collect::<Vec<_>>();
    let user_version = |f: &Fixture| f.sidecar().query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0)).unwrap();
    assert_eq!(user_version(&f), 2);
    let state = f.project.join(".state");
    let before = tree(&state);
    let (report, v2) = f.cli("usage");
    assert_eq!(attempt_usage(&report), serde_json::json!({"input_tokens": 1000, "cached_input_tokens": 400, "cache_write_input_tokens": 0,
        "output_tokens": 120, "reasoning_output_tokens": 80, "total_tokens": 1120, "records": 1}));
    assert_eq!(metric(&f.report(), "M08")["value"], 1000);
    assert_eq!(tree(&state), before, "a read neither upgrades nor creates a file");

    f.cli("collect");
    f.cli("collect");
    // Only the codex stream's history is asserted here; each lane's stream is
    // asserted by that lane's own tests, so adding a lane never edits this one.
    assert_eq!(streams(&f).into_iter().find(|(stream, _)| stream == "codex"), Some(("codex".to_owned(), 2)));
    assert_eq!(user_version(&f), 2);
    let before = tree(&state);
    assert_eq!(f.cli("usage").1, v2, "usage is byte-identical after the upgrade");
    assert_eq!(metric(&f.report(), "M08")["value"], 1000);
    assert_eq!(f.cli_args(&["accounting", "status"]).0["stream"], "accounting");
    assert_eq!(tree(&state), before, "reads of an upgraded sidecar create no file");

    f.sidecar().execute("INSERT OR REPLACE INTO telemetry_streams(stream,version) VALUES('accounting',99)", []).unwrap();
    for args in [&["usage"][..], &["report", "--json"], &["collect"]] {
        let error = f.cli_fail(args);
        assert!(error.contains("telemetry sidecar stream accounting version 99 is newer than this binary"), "{args:?}: {error}");
    }
    assert_eq!(streams(&f), [("accounting".to_owned(), 99), ("codex".to_owned(), 2), ("ingest".to_owned(), 1)]);
}

/// Rollout `name` of session `sid` holding `head.jsonl` (one record: 1000 in, 120 out).
fn session(f: &Fixture, name: &str, sid: &str, cwd: &str, ts_ms: i64) {
    let path = f.rollout(&f.home, name, &["head.jsonl"], cwd, ts_ms, "0.154.0");
    fs::write(&path, fs::read_to_string(&path).unwrap().replace(SID, sid)).unwrap();
}

/// `(session_id, binding, attempt_id, basis)` per rollout source, from `collectors bindings`.
fn sources(f: &Fixture) -> Vec<(String, String, Option<String>, String)> {
    let (out, _) = f.cli_args(&["collectors", "bindings"]);
    out["sources"].as_array().unwrap().iter().map(|s| (s["session_id"].as_str().unwrap().to_owned(), s["binding"].as_str().unwrap().to_owned(),
        s["attempt_id"].as_str().map(str::to_owned), s["basis"].as_str().unwrap().to_owned())).collect()
}

/// Card A1 (TM1.1): a rollout binds only through the attempt's canonical
/// collector binding, written when the attempt is launched (the real launch is
/// checked in tests/cli.rs; here `Fixture::bind` plants it). A revocation keeps
/// rollouts bound before it with their accepted usage and binds none started
/// after it; a rollout from another project's worktree stays unbound; an
/// attempt from before 0052 falls back to contracts §5 rules 1-4.
#[test]
fn rollout_binds_only_through_canonical_binding() {
    const ELSEWHERE: &str = "00000000-0000-4000-8000-0000000000e1";
    const LATER: &str = "00000000-0000-4000-8000-0000000000f1";
    let one = serde_json::json!({"input_tokens": 1000, "cached_input_tokens": 400, "cache_write_input_tokens": 0,
        "output_tokens": 120, "reasoning_output_tokens": 80, "total_tokens": 1120, "records": 1});
    let f = Fixture::reserved();
    let unbound = |sid: &str, basis: &str| (sid.to_owned(), "unbound".to_owned(), None, basis.to_owned());
    let bound = |sid: &str, basis: &str| (sid.to_owned(), "bound".to_owned(), Some(f.attempt.clone()), basis.to_owned());
    // Started 1 ms after the decision, before the revocation below.
    f.rollout(&f.home, SID, &["head.jsonl"], &f.worktree(), f.decided + 1, "0.154.0");
    // The same attempt id under another project's worktrees.
    session(&f, "elsewhere", ELSEWHERE, &format!("{}/other/.state/worktrees/{}/repo-00", f.root.display(), f.attempt), f.decided + 1_000);
    let (report, _) = f.cli("collect");
    assert_eq!(attempt_usage(&report), unavailable("not_bound"), "reserved, never launched");
    assert_eq!(sources(&f), [unbound(ELSEWHERE, "no_match"), unbound(SID, "no_binding")]);
    assert_eq!(f.cli_args(&["collectors", "bindings"]).0["bindings"], serde_json::json!([]));

    f.bind();
    let bindings = f.cli_args(&["collectors", "bindings"]).0["bindings"].clone();
    let launched = bindings[0]["unix_ms"].as_i64().unwrap();
    assert!(launched >= f.decided);
    assert_eq!(bindings, serde_json::json!([{"attempt_id": f.attempt, "revision": 1, "state": "active", "collector": "codex", "unix_ms": launched}]));
    let (report, _) = f.cli("collect");
    assert_eq!(attempt_usage(&report), one);
    assert_eq!(sources(&f), [unbound(ELSEWHERE, "no_match"), bound(SID, "collector_binding")]);

    let (revoked, _) = f.cli_args(&["collectors", "revoke", &f.attempt]);
    let at = revoked["binding"]["unix_ms"].as_i64().unwrap();
    assert!(at >= launched);
    let expected = serde_json::json!({"attempt_id": f.attempt, "revision": 2, "state": "revoked", "collector": "codex", "unix_ms": at});
    assert_eq!(revoked, serde_json::json!({"binding": expected, "written": true}));
    assert_eq!(f.cli_args(&["collectors", "revoke", &f.attempt]).0, serde_json::json!({"binding": expected, "written": false}), "replay appends nothing");
    assert_eq!(f.cli_args(&["collectors", "bindings"]).0["bindings"].as_array().unwrap().len(), 2);
    session(&f, "later", LATER, &f.worktree(), at + 1_000);
    let (report, _) = f.cli("collect");
    assert_eq!(attempt_usage(&report), one, "accepted usage bound before the revocation is kept");
    assert_eq!(sources(&f), [unbound(ELSEWHERE, "no_match"), unbound(LATER, "binding_revoked"), bound(SID, "collector_binding")]);
    assert_eq!(f.usage().iter().filter(|r| r.2 == 1).count(), 3, "every record is accepted; binding only attributes");
    assert!(f.cli_fail(&["collectors", "revoke", "no-such-attempt"]).contains("has no collector binding"));

    // An attempt reserved before 0052 keeps rules 1-4 and cannot be revoked.
    let g = Fixture::reserved();
    let db_path = g.project.join(".state/state.db");
    test_schema::historical(&rusqlite::Connection::open(&db_path).unwrap(), 51).unwrap();
    SqliteStore::open(&db_path).unwrap().upgrade_v1().unwrap();
    g.rollout(&g.home, SID, &["head.jsonl"], &g.worktree(), g.decided + 1_000, "0.154.0");
    let (report, _) = g.cli("collect");
    assert_eq!(attempt_usage(&report), one);
    assert_eq!(sources(&g), [(SID.to_owned(), "bound".to_owned(), Some(g.attempt.clone()), "predates_binding".to_owned())]);
    let bindings = g.cli_args(&["collectors", "bindings"]).0["bindings"].clone();
    assert_eq!((&bindings[0]["revision"], &bindings[0]["state"], &bindings[0]["collector"]), (&1.into(), &"predates_binding".into(), &serde_json::Value::Null));
    assert!(g.cli_fail(&["collectors", "revoke", &g.attempt]).contains("predates collector bindings"));
}

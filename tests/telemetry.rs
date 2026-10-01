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
    // The default `usage` output is one readable line per attempt and per rollout.
    assert_eq!(f.text(&["usage"]), format!("{0} usage=in=1500 out=180 total=1680 records=2 after_termination=null\n\
        session {SID} binding=bound attempt={0} cli=0.154.0 certified=true records=2 accepted=2 quarantined=false\n", f.attempt));
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
    assert_eq!(attempt_usage(&f.cli_args(&["usage", "--json"]).0), unavailable("cli_version_uncertified"), "a read re-reads nothing");
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
    type RolloutCase = (&'static str, fn(&Fixture) -> (PathBuf, String, i64));
    let cases: [RolloutCase; 3] = [
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
    output.extend(f.cli_args(&["usage", "--json"]).1);
    output.extend(f.text(&["usage"]).into_bytes());
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
    assert_eq!(report["tasks"], serde_json::json!({"accepted": 2, "open": 2, "succeeded_without_evidence": 0, "terminal": 3, "replay_candidates": 0}));
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
    assert_eq!((&m13["numerator"], &m13["denominator"], &m13["value"], &m13["adapter_absent"]), (&1.into(), &3.into(), &"1/3".into(), &0.into()));
    assert_eq!(m13["incomplete"], serde_json::json!({"not_bound": 2}));
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

const ACCOUNTING: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/telemetry/accounting");

fn digest(path: &Path) -> String { format!("sha256:{:x}", <sha2::Sha256 as sha2::Digest>::digest(path.as_os_str().as_encoded_bytes())) }

/// Extended M40 in the report (contracts §6, contracts-accounting §5): per
/// decision and limit window, the remaining value `accounting quota` shows,
/// once `accounting sync` built the quota tables.
#[test]
fn quota_headroom_at_dispatch() {
    // `quota-single.jsonl`: used 37.5% of the 300-minute `codex` window, one minute
    // before dispatch; the window resets an hour after it → remaining 100 − 37.5 = 62.5, fresh.
    let f = Fixture::new();
    let resets = f.decided / 1000 + 3_600;
    let text = fs::read_to_string(Path::new(ACCOUNTING).join("quota-single.jsonl")).unwrap()
        .replace("@T1@", &jiff::Timestamp::from_millisecond(f.decided - 60_000).unwrap().to_string()).replace("@R1@", &resets.to_string());
    let fixture = f.tmp.path().join("quota-single.jsonl");
    fs::write(&fixture, text).unwrap();
    f.rollout(&f.home, "quota", &[fixture.to_str().unwrap()], &f.worktree(), f.decided - 60_000, "0.154.0");
    f.cli("collect");
    // Before a sync the quota tables do not exist yet: unavailable, never 0.
    let m40 = metric(&f.report(), "M40");
    assert_eq!((&m40["definition"], &m40["name"], &m40["stale_after_ms"]), (&"M40.quota-windows-v1".into(), &"quota_headroom_at_dispatch".into(), &900_000.into()));
    assert_eq!(m40["decisions"], serde_json::json!([{"attempt_id": f.attempt, "decided_unix_ms": f.decided, "service": "codex",
        "value": unavailable("ledger_not_synced")}]));
    f.cli_args(&["accounting", "sync"]);
    let account = digest(&f.home);
    let report = f.report();
    assert_eq!(metric(&report, "M40")["decisions"], serde_json::json!([{"attempt_id": f.attempt, "decided_unix_ms": f.decided, "service": "codex",
        "account": account, "account_basis": "execution_home", "windows": [
            {"limit_id": "codex", "window_kind": "primary", "unit": "percent", "window_id": format!("codex:{account}:codex:primary:{}", resets * 1000),
             "window_minutes": 300, "resets_unix_ms": resets * 1000, "observed_unix_ms": f.decided - 60_000, "age_ms": 60_000,
             "value": "62.5", "used": "37.5", "freshness": "fresh"},
            {"limit_id": "codex", "window_kind": "secondary", "value": unavailable("not_reported")}]}]));
    // The same decisions as `accounting quota`.
    assert_eq!(metric(&report, "M40")["decisions"], f.cli_args(&["accounting", "quota", "--json"]).0["metrics"]["M40"]["decisions"]);
    // Text (the fleet pane's body): one line per decision and limit window.
    let text = f.text(&["report"]);
    let m40: Vec<&str> = text.lines().filter(|l| l.starts_with("M40 ")).collect();
    assert_eq!(m40, [format!("M40 quota_headroom_at_dispatch {} codex primary remaining 62.5% age_ms=60000 fresh", f.attempt),
        format!("M40 quota_headroom_at_dispatch {} codex secondary n/a (not_reported)", f.attempt)], "{text}");
    fs::write(f.project.join("PROJECT.md"), "# demo\n").unwrap();
    let pane = Command::new(BIN).env_clear().env("HOME", f.tmp.path().join("home")).env("PATH", "/usr/bin:/bin")
        .args(["--root", f.root.to_str().unwrap(), "pane", "fleet"]).output().unwrap();
    assert!(pane.status.success(), "{}", String::from_utf8_lossy(&pane.stderr));
    let pane = String::from_utf8(pane.stdout).unwrap();
    assert_eq!(pane.lines().filter(|l| l.starts_with("M40 ")).collect::<Vec<_>>(), m40, "{pane}");

    // `head.jsonl` reports a window that reset at 1790003600 s (September 2026),
    // before any decision made now: its remaining value no longer applies.
    let f = Fixture::new();
    f.rollout(&f.home, SID, &["head.jsonl"], &f.worktree(), f.decided - 60_000, "0.154.0");
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    let report = f.report();
    let primary = metric(&report, "M40")["decisions"][0]["windows"][0].clone();
    assert_eq!((&primary["value"], &primary["age_ms"], &primary["resets_unix_ms"], primary.get("freshness")),
        (&unavailable("window_reset_since_observation"), &60_000.into(), &1_790_003_600_000i64.into(), None));
    assert!(f.text(&["report"]).lines().any(|l| l == format!("M40 quota_headroom_at_dispatch {} codex primary n/a (window_reset_since_observation) age_ms=60000", f.attempt)));
    // Rate limits are metadata, kept for an uncertified version; counters are not.
    assert_eq!(metric(&report, "M08")["value"], unavailable("no_certified_source"));

    let f = Fixture::new();
    f.rollout(&f.home, SID, &["head.jsonl"], &f.worktree(), f.decided + 1_000, "0.154.0");
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(metric(&f.report(), "M40")["decisions"], serde_json::json!([{"attempt_id": f.attempt, "decided_unix_ms": f.decided, "service": "codex",
        "account": digest(&f.home), "account_basis": "execution_home", "value": unavailable("no_observation")}]));
    assert!(f.text(&["report"]).lines().any(|l| l == format!("M40 quota_headroom_at_dispatch {} n/a (no_observation)", f.attempt)));
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
    f.cli_args(&["usage", "--json"]);
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
    f.cli_args(&["usage", "--json"]);
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
/// collect upgrades it in place, idempotently, to the current stream `codex`
/// (3: TM5.1's read indexes) with byte-identical `usage`; a stream newer than
/// this binary is refused.
#[test]
fn sidecar_streams_upgrade_v2_store() {
    let f = Fixture::new();
    f.rollout(&f.home, SID, &["head.jsonl"], &f.worktree(), f.decided + 1_000, "0.154.0");
    f.cli("collect");
    // Back to a v2 sidecar: no streams table, no codex 0003 indexes.
    f.sidecar().execute_batch("DROP TABLE telemetry_streams; DROP TABLE otlp_records; DROP TABLE gemini_file_cursors; DROP INDEX codex_usage_by_path; DROP INDEX rollout_sources_by_attempt;
        DROP INDEX codex_usage_by_turn; DROP INDEX codex_usage_by_response; PRAGMA user_version = 2").unwrap();
    let streams = |f: &Fixture| f.sidecar().prepare("SELECT stream,version FROM telemetry_streams ORDER BY stream").unwrap()
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))).unwrap().map(Result::unwrap).collect::<Vec<_>>();
    let user_version = |f: &Fixture| f.sidecar().query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0)).unwrap();
    assert_eq!(user_version(&f), 2);
    let state = f.project.join(".state");
    let before = tree(&state);
    let (report, v2) = f.cli_args(&["usage", "--json"]);
    assert_eq!(attempt_usage(&report), serde_json::json!({"input_tokens": 1000, "cached_input_tokens": 400, "cache_write_input_tokens": 0,
        "output_tokens": 120, "reasoning_output_tokens": 80, "total_tokens": 1120, "records": 1}));
    assert_eq!(metric(&f.report(), "M08")["value"], 1000);
    assert_eq!(tree(&state), before, "a read neither upgrades nor creates a file");

    f.cli("collect");
    f.cli("collect");
    // Every lane stream with migrations is at its latest version beside `codex`.
    let expected = |extra: (&str, i64)| {
        let mut streams: std::collections::BTreeMap<String, i64> = herdr_projects::telemetry::LANES.iter().filter(|l| !l.migrations.is_empty())
            .map(|l| (l.stream.to_owned(), l.migrations.len() as i64)).collect();
        streams.insert("codex".to_owned(), 4);
        streams.insert(extra.0.to_owned(), extra.1);
        streams.into_iter().collect::<Vec<_>>()
    };
    assert_eq!(streams(&f), expected(("codex", 4)));
    assert_eq!(user_version(&f), 4);
    let before = tree(&state);
    assert_eq!(f.cli_args(&["usage", "--json"]).1, v2, "usage is byte-identical after the upgrade");
    assert_eq!(metric(&f.report(), "M08")["value"], 1000);
    assert_eq!(f.cli_args(&["accounting", "status"]).0["stream"], "accounting");
    assert_eq!(tree(&state), before, "reads of an upgraded sidecar create no file");

    f.sidecar().execute("INSERT OR REPLACE INTO telemetry_streams(stream,version) VALUES('accounting',99)", []).unwrap();
    for args in [&["usage"][..], &["report", "--json"], &["collect"]] {
        let error = f.cli_fail(args);
        assert!(error.contains("telemetry sidecar stream accounting version 99 is newer than this binary"), "{args:?}: {error}");
    }
    assert_eq!(streams(&f), expected(("accounting", 99)));
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

/// Demo finding: a submission rejected by one acceptance policy (`content`)
/// and accepted by another (`clean`) later is rejected, not accepted because
/// its latest run across all policies was; nothing will integrate it, and the
/// text form has no dangling separators. Rows are planted as the verifier and
/// cancellation leave them for a cancelled attempt.
#[test]
fn verification_combines_every_acceptance_policy() {
    let f = Fixture::new();
    f.cancel_reserved();
    let db = rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap();
    db.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    let (oid, sub) = ("a".repeat(40), "1".repeat(64));
    db.execute("INSERT INTO task_contracts(task_id,contract_revision,project_store,expected_head,repository,base_oid,object_format,route,raw_bytes,raw_digest,installed_seq)
        VALUES('work',1,'store',1,'/repo',?1,'sha1','verify_then_integrate',x'7b7d',?2,1)", rusqlite::params![oid, "c".repeat(64)]).unwrap();
    for policy in ["clean", "content"] {
        db.execute("INSERT INTO acceptance_policies(task_id,contract_revision,policy_id,body) VALUES('work',1,?1,'policy')", [policy]).unwrap();
    }
    db.execute("INSERT INTO result_submissions(submission_id,project_store,idempotency_key,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,artifact_manifest,claimed_checks,created_unix_ms)
        VALUES(?1,'store','submit',?2,'{}','work',1,?2,?3,'/repo',?4,?4,'sha1','[]','[]',1000)", rusqlite::params![sub, "d".repeat(64), f.attempt, oid]).unwrap();
    let run = |run: char, policy: &str, state: &str, reason: Option<&str>, created: i64| {
        let accepted = state == "accepted";
        db.execute("INSERT INTO verification_runs(run_id,project_store,idempotency_key,payload_digest,submission_id,task_id,contract_revision,contract_digest,attempt_id,policy_id,policy_digest,commit_oid,tree_oid,object_format,memory_fence,isolation,argv,library_manifest,state,reason,exit_status,receipt_digest,store_device,store_inode,created_unix_ms)
            VALUES(?1,'store',?1,?2,?3,'work',1,?2,?4,?5,?2,?6,?6,'sha1',0,'linux-unshare-user-pid-mount-v1','[\"check\"]','[]',?7,?8,?9,?10,1,1,?11)",
            rusqlite::params![run.to_string().repeat(64), "e".repeat(64), sub, f.attempt, policy, oid, state, reason,
                if accepted { Some(0) } else { Some(1) }, accepted.then(|| "f".repeat(64)), created]).unwrap();
    };
    run('2', "clean", "accepted", None, 3000);
    let record = || f.cli_args(&["attempts", "--json"]).0["attempts"][0].clone();
    let first = record();
    assert_eq!(first["verification"], serde_json::json!({"state": "pending", "policies": [
        {"policy_id": "clean", "state": "accepted"}, {"policy_id": "content", "state": "pending"}]}), "a policy without a run is pending");
    assert_eq!(first["integration"], serde_json::json!({"state": "pending"}));

    run('3', "content", "rejected", Some("checks_failed"), 2000);
    let second = record();
    assert_eq!(second["verification"], serde_json::json!({"state": "rejected", "reason": "checks_failed", "policies": [
        {"policy_id": "clean", "state": "accepted"}, {"policy_id": "content", "state": "rejected", "reason": "checks_failed"}]}));
    assert_eq!(second["integration"], serde_json::json!({"state": "not_applicable", "reason": "verification_rejected"}));
    assert_eq!((&second["terminal_state"], &second["accepted"]), (&serde_json::json!("cancelled"), &serde_json::json!(false)));
    assert_eq!(f.text(&["attempts"]), format!("{} task=work state=cancelled wall_ms=unavailable:not_running result=submitted \
        verification=rejected:checks_failed integration=not_applicable:verification_rejected accepted=false attention=unavailable:attention_not_collected usage=unavailable:collection_not_run\n", f.attempt));
}

/// Herdr stand-in (as tests/telemetry_accounting.rs): answers `agent list` from
/// `$HOME/agents.json` and exits without a reply when that file is absent.
const FAKE_HERDR: &str = "#!/bin/sh\ncase \"$*\" in\n'agent list') [ -f \"$HOME/agents.json\" ] || exit 1; cat \"$HOME/agents.json\";;\n*) exit 2;;\nesac\n";

/// Contracts §4 `attention`: B6b's per-attempt summary in the outcome record.
/// The first attempt is cancelled before any launch (`not_launched` once samples
/// exist); the readmitted one runs and is sampled each minute: working at 0,
/// waiting at 1, working at 2 (one closed wait of 60000 ms), Herdr unreachable
/// at 3, waiting at 4 (after the gap), then no pass since (the wait is censored
/// by a `not_observed` gap). Observed = working 0–1 + waiting 1–2 = 120000 ms.
#[test]
fn attempts_show_attention_summary() {
    let f = Fixture::new();
    let first = f.attempt.clone();
    f.readmit("codex");
    let db_path = f.project.join(".state/state.db");
    let db = rusqlite::Connection::open(&db_path).unwrap();
    let attempt: String = db.query_row("SELECT id FROM attempts WHERE state='reserved'", [], |r| r.get(0)).unwrap();
    let base = fs::canonicalize(f.tmp.path()).unwrap();
    let herdr = base.join("herdr");
    fs::write(&herdr, FAKE_HERDR).unwrap();
    fs::set_permissions(&herdr, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let socket = base.join("herdr.sock");
    let _server = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let home = f.tmp.path().join("home");
    let cli = |args: &[&str]| -> String {
        let out = Command::new(BIN).env_clear().env("HOME", &home).env("PATH", "/usr/bin:/bin").env("HERDR_BIN_PATH", &herdr)
            .env("HERDR_PROJECTS_TELEMETRY_COLLECT_SECS", "60").args(["--root", f.root.to_str().unwrap(), "telemetry", "demo"]).args(args).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    };
    let attention = || {
        let report: serde_json::Value = serde_json::from_str(&cli(&["attempts", "--json"])).unwrap();
        report["attempts"].as_array().unwrap().iter().map(|a| (a["attempt_id"].as_str().unwrap().to_owned(), a["attention"].clone())).collect::<Vec<_>>()
    };
    f.cli("collect");
    // No sample yet: every record keeps `attention_not_collected`.
    assert_eq!(attention(), [(first.clone(), unavailable("attention_not_collected")), (attempt.clone(), unavailable("attention_not_collected"))]);

    // Fixture only: the launch receipt `apply_launch_started` records, one minute before the first pass, and a running attempt.
    let minute = |m: f64| 1_700_000_000_000 + (m * 60_000.0) as i64;
    let receipt = serde_json::json!({"version": 2, "attempt": attempt, "operation": "op-launch",
        "route": {"machine": "", "socket": socket, "workspace_id": "w1", "tab_id": "w1:t1", "pane_id": "w1:p1", "cwd": "/work"},
        "terminal": "term-1", "session": {"device": 1, "inode": 2, "born_secs": 3, "born_nanos": 4},
        "agent": {"kind": "codex", "name": "worker"}, "observed_unix_ms": minute(-1.0)});
    db.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('runtime.launch_started','op-launch',1,1,?1)", [receipt.to_string()]).unwrap();
    db.execute("UPDATE attempts SET state='running' WHERE id=?1", [&attempt]).unwrap();
    for status in [Some("working"), Some("blocked"), Some("working"), None, Some("blocked")] {
        match status {
            Some(status) => fs::write(home.join("agents.json"), serde_json::json!({"result": {"agents": [{"pane_id": "w1:p1", "workspace_id": "w1", "tab_id": "w1:t1",
                "cwd": "/work", "agent": "codex", "name": "worker", "agent_status": status}]}}).to_string()).unwrap(),
            None => fs::remove_file(home.join("agents.json")).unwrap(),
        }
        cli(&["accounting", "observe-attention"]);
    }
    // Fixture only: the passes ran milliseconds apart; re-time pass k to minute k.
    let sidecar = f.sidecar();
    let times: Vec<i64> = sidecar.prepare("SELECT DISTINCT observed_unix_ms FROM attention_samples ORDER BY 1").unwrap()
        .query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
    assert_eq!(times.len(), 5);
    for (k, old) in times.iter().enumerate() {
        sidecar.execute("UPDATE attention_samples SET observed_unix_ms=?1 WHERE observed_unix_ms=?2", [minute(k as f64), *old]).unwrap();
    }
    drop(sidecar);

    // Waits: 1–2 closed (60000 ms, counted) and 4 (after the gap, censored) → 2 counted, 1 censored.
    // Gaps: 2–4 `herdr_unreachable`, from 4 `not_observed`.
    assert_eq!(attention(), [(first, unavailable("not_launched")), (attempt.clone(), serde_json::json!({
        "interventions": 2, "uncertain_starts": 0, "waiting_ms": 60_000, "observed_ms": 120_000, "intervals": 2, "censored_intervals": 1,
        "gaps": {"herdr_unreachable": 1, "not_observed": 1}, "reason_type": "blocked_untyped", "basis": "live", "source": "herdr-agent-list-v1"}))]);
    let text = cli(&["attempts"]);
    let line = text.lines().find(|l| l.starts_with(attempt.as_str())).unwrap();
    assert!(line.contains(" attention=waits=2 waiting_ms=60000 censored=1 gaps=2 usage="), "{line}");
}

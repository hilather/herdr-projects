//! Telemetry sidecar end to end: a real reserved attempt, hand-written Codex
//! rollouts under its execution home, and `herdr-projects telemetry` on the CLI.

#![cfg(all(feature = "state-store", target_os = "linux"))]

use herdr_projects::{domain::*, store::SqliteStore};
use sha2::{Digest, Sha256};
use std::{fs, path::{Path, PathBuf}, process::Command};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-projects");
const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/telemetry/codex-0.154.0");
const SID: &str = "00000000-0000-4000-8000-00000000c0de";
/// sha256 of the canonical payloads, computed with `sha256sum` outside the crate.
const DIGEST_1: &str = "sha256:c56c0b798f22b872890b69470f332d6be530384c7f5d5b61d0ae3a41bb6f65bf";
const DIGEST_2: &str = "sha256:6c2dcf23c84488655ff53556237af61da2717e13172cdf50dad886d53e84be41";
const DIGEST_2B: &str = "sha256:49f430f4555d26b959282f29656f343ad7e239dadf49714298b21d46e61e698f";

struct Fixture { tmp: tempfile::TempDir, root: PathBuf, project: PathBuf, home: PathBuf, attempt: String, decided: i64, config: herdr_projects::migration::ConfigReference }

impl Fixture {
    /// Project `demo` with one attempt reserved by automatic admission on a Codex
    /// profile whose execution home is `codex-home`, and a second retained Codex
    /// profile (`other-home`) that no attempt uses.
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tmp.path()).unwrap();
        let (root, home) = (base.join("root"), base.join("codex-home"));
        let project = root.join("demo");
        fs::create_dir_all(project.join(".state")).unwrap();
        fs::create_dir_all(base.join("home")).unwrap();
        let db_path = project.join(".state/state.db");
        fs::write(base.join("owner.toml"), "version = 1\n").unwrap();
        let config = herdr_projects::migration::config_reference(&base.join("owner.toml")).unwrap();
        let digest = config.digest.clone().unwrap();
        let mut db = SqliteStore::create(&db_path).unwrap();
        let id = TaskId::new("work").unwrap();
        db.commit(Commit { expected_head: 0, mutations: vec![Mutation::Task { expected: None,
            next: Task { id: id.clone(), revision: 1, state: TaskState::Draft, title: "work".into(), active_attempt: None } }] }).unwrap();
        db.create_runtime(Some(&id), Some(1), db.current_head().unwrap(), &RuntimeRoute::default()).unwrap();
        let revision = db.read_snapshot(None).unwrap().tasks[0].revision;
        db.queue_task(&id, revision, db.current_head().unwrap(), &QueueRequest { priority: 0, dependencies: Vec::new() }, unix_ms() - 1_000).unwrap();
        let snapshot = db.read_snapshot(None).unwrap();
        db.set_scheduler_policy(snapshot.head, snapshot.scheduler.unwrap().policy.revision, 1, 3).unwrap();
        let snapshot = db.read_snapshot(None).unwrap();
        let binding = &snapshot.runtime_bindings[0];
        db.record_observations(snapshot.head, &[herdr_projects::reconcile::RuntimeObservation { binding: binding.id.clone(), binding_revision: binding.revision,
            task_revision: Some(snapshot.tasks[0].revision), observed_unix_ms: unix_ms(), collector: "herdr-git-v1".into(), config_digest: Some(digest.clone()),
            ..herdr_projects::reconcile::RuntimeObservation::default() }]).unwrap();
        let snapshot = db.read_snapshot(None).unwrap();
        db.set_project_state(snapshot.head, snapshot.control.unwrap().revision, ProjectState::Active, unix_ms(), Some(&digest)).unwrap();
        drop(db);
        plant_profile(&db_path, codex_profile(&config, "codex", "codex", Some(&home)));
        SqliteStore::open(&db_path).unwrap().record_native_capability_evidence(unix_ms(), unix_ms() + 3_600_000).unwrap();
        // Fixture only. Production code has no writer for this column.
        rusqlite::Connection::open(&db_path).unwrap().execute("UPDATE project_control SET factory_admission='on' WHERE singleton=1", []).unwrap();
        worker_snapshots(&project, None);
        let inputs = herdr_projects::admission::prepared_admission_inputs(&project).unwrap().expect("a ready candidate");
        insert_grant(&db_path, &inputs);
        herdr_projects::admission::admit_once(&project).unwrap();
        let (attempt, decided) = rusqlite::Connection::open(&db_path).unwrap()
            .query_row("SELECT attempt_id,decided_unix_ms FROM dispatch_decisions", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        plant_profile(&db_path, codex_profile(&config, "codex", "other", Some(&base.join("other-home"))));
        Fixture { tmp, root, project, home, attempt, decided, config }
    }

    fn worktree(&self) -> String { format!("{}/.state/worktrees/{}/repo-00", self.project.display(), self.attempt) }

    /// Write `parts` of the fixture rollout under `home`, substituting the placeholders.
    fn rollout(&self, home: &Path, name: &str, parts: &[&str], cwd: &str, ts_ms: i64, version: &str) -> PathBuf {
        let dir = home.join(".codex/sessions/2026/09/28");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("rollout-2026-09-28T00-00-00-{name}.jsonl"));
        let text = parts.iter().map(|part| fs::read_to_string(Path::new(FIXTURES).join(part)).unwrap()).collect::<String>();
        let ts = jiff::Timestamp::from_millisecond(ts_ms).unwrap().to_string();
        fs::write(&path, text.replace("@SID@", SID).replace("@CWD@", cwd).replace("@TS@", &ts).replace("@VERSION@", version)).unwrap();
        path
    }

    fn cli(&self, command: &str) -> (serde_json::Value, Vec<u8>) { self.cli_args(&[command]) }

    fn cli_args(&self, args: &[&str]) -> (serde_json::Value, Vec<u8>) {
        let out = Command::new(BIN).env_clear().env("HOME", self.tmp.path().join("home")).env("PATH", "/usr/bin:/bin")
            .args(["--root", self.root.to_str().unwrap(), "telemetry", "demo"]).args(args).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let mut bytes = out.stdout.clone();
        bytes.extend_from_slice(&out.stderr);
        (serde_json::from_slice(&out.stdout).unwrap(), bytes)
    }

    fn sidecar(&self) -> rusqlite::Connection { rusqlite::Connection::open(self.project.join(".state/telemetry.db")).unwrap() }

    /// `(ordinal, payload_digest, accepted, reason, input, cached, output, reasoning, total)` per record.
    fn usage(&self) -> Vec<(i64, String, i64, Option<String>, Option<i64>, Option<i64>, Option<i64>, Option<i64>, Option<i64>)> {
        let db = self.sidecar();
        let mut stmt = db.prepare("SELECT ordinal,payload_digest,accepted,reason,input_tokens,cached_input_tokens,output_tokens,reasoning_output_tokens,total_tokens FROM codex_usage ORDER BY session_id,ordinal").unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?))).unwrap().map(Result::unwrap).collect()
    }

    fn count(&self, table: &str) -> i64 { self.sidecar().query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0)).unwrap() }

    /// `(binding, attempt_id)` of the one rollout source.
    fn binding(&self) -> (String, Option<String>) {
        self.sidecar().query_row("SELECT binding,attempt_id FROM rollout_sources", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap()
    }

    fn report(&self) -> serde_json::Value { self.cli_args(&["report", "--json"]).0 }

    /// Cancel the reserved attempt (released: the task is cancelled).
    fn cancel_reserved(&self) {
        let db_path = self.project.join(".state/state.db");
        let attempt: String = rusqlite::Connection::open(&db_path).unwrap().query_row("SELECT id FROM attempts WHERE state='reserved'", [], |r| r.get(0)).unwrap();
        let mut db = SqliteStore::open(&db_path).unwrap();
        assert!(db.cancel_attempt(&AttemptId::new(attempt).unwrap(), 1, db.current_head().unwrap(), "next arm", unix_ms()).unwrap().released);
    }

    /// Cancel the reserved attempt, queue the task again and admit it on `profile` only.
    fn readmit(&self, profile: &str) {
        let db_path = self.project.join(".state/state.db");
        self.cancel_reserved();
        let mut db = SqliteStore::open(&db_path).unwrap();
        // No production path re-opens a cancelled task; the fixture blocks it as a failed attempt would.
        let snapshot = db.read_snapshot(None).unwrap();
        let mut task = snapshot.tasks[0].clone();
        let expected = task.revision;
        task.revision += 1;
        task.state = TaskState::Blocked;
        db.commit(Commit { expected_head: snapshot.head, mutations: vec![Mutation::Task { expected: Some(expected), next: task.clone() }] }).unwrap();
        db.queue_task(&task.id, task.revision, db.current_head().unwrap(), &QueueRequest { priority: 0, dependencies: Vec::new() }, unix_ms()).unwrap();
        db.record_native_capability_evidence(unix_ms(), unix_ms() + 3_600_000).unwrap();
        drop(db);
        worker_snapshots(&self.project, Some(profile));
        let inputs = herdr_projects::admission::prepared_admission_inputs(&self.project).unwrap().expect("a ready candidate");
        assert_eq!(inputs.effective_profile.as_ref().unwrap().name, profile);
        insert_grant(&db_path, &inputs);
        herdr_projects::admission::admit_once(&self.project).unwrap();
        assert_eq!(rusqlite::Connection::open(&db_path).unwrap().query_row("SELECT count(*) FROM attempts WHERE state='reserved'", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
    }
}

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

fn unix_ms() -> i64 { jiff::Timestamp::now().as_millisecond() }

fn codex_profile(config: &herdr_projects::migration::ConfigReference, kind: &str, name: &str, home: Option<&Path>) -> FrozenProfile {
    let evidence = VersionedReference { id: "sim-evidence".into(), revision: 1, digest: "a".repeat(64) };
    let supported = CapabilityEvidence::Supported { evidence: evidence.clone() };
    FrozenProfile {
        version: 1, name: name.into(), kind: kind.into(), definition_digest: "b".repeat(64), config: config.clone(),
        arguments_digest: "c".repeat(64), environment_names: Vec::new(), execution_home: home.map(|home| home.display().to_string()),
        permission_policy: VersionedReference { id: "sim-policy".into(), revision: 1, digest: "d".repeat(64) }, adapter: evidence,
        agent: ExecutableIdentity { path: "/usr/bin/git".into(), digest: "e".repeat(64), version: "0.154.0".into() },
        herdr: ExecutableIdentity { path: "/usr/bin/git".into(), digest: "f".repeat(64), version: "1.0.0".into() },
        capabilities: ProfileCapabilities { launch: supported.clone(), readiness_observation: supported.clone(), prompt_submission: supported.clone(),
            stop: supported, checkpoint_acknowledgment: CapabilityEvidence::Unknown, structured_usage: CapabilityEvidence::Unknown, resume: CapabilityEvidence::Unknown },
        workflow_certificate: None,
    }
}

fn plant_profile(db_path: &Path, profile: FrozenProfile) {
    use std::os::unix::fs::MetadataExt;
    profile.validate_for_launch().unwrap();
    let reference = profile.reference().unwrap();
    let canonical = fs::canonicalize(db_path).unwrap();
    let metadata = fs::metadata(&canonical).unwrap();
    let report = serde_json::json!({"preparation": {"profile": profile, "reference": reference, "launchable": true, "protocol_capable": false, "certified": false},
        "source_store": [canonical, metadata.dev(), metadata.ino()]});
    let text = serde_json::to_string(&report).unwrap();
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('profile.native_retained',?1,1,1,?2)",
        rusqlite::params![reference.id, serde_json::to_string(&reference).unwrap()]).unwrap();
    conn.execute("INSERT INTO native_profiles(profile_digest,report,report_digest,sequence) VALUES(?1,?2,?3,?4)",
        rusqlite::params![reference.digest, text, format!("{:x}", Sha256::digest(text.as_bytes())), conn.last_insert_rowid()]).unwrap();
}

/// Retained worker knowledge for the queued task and every retained profile (or only `only`).
fn worker_snapshots(project: &Path, only: Option<&str>) {
    let db_path = project.join(".state/state.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let profiles = conn.prepare("SELECT report FROM native_profiles").unwrap()
        .query_map([], |row| row.get::<_, String>(0)).unwrap()
        .map(|report| serde_json::from_value::<FrozenProfile>(serde_json::from_str::<serde_json::Value>(&report.unwrap()).unwrap()["preparation"]["profile"].clone()).unwrap())
        .collect::<Vec<_>>();
    let revision: i64 = conn.query_row("SELECT revision FROM tasks WHERE id='work'", [], |r| r.get(0)).unwrap();
    for profile in profiles.into_iter().filter(|p| only.is_none_or(|name| p.name == name)) {
        let exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM memory_snapshots WHERE task_id='work' AND task_revision=?1 AND profile_name=?2)",
            rusqlite::params![revision, profile.name], |r| r.get(0)).unwrap();
        if exists { continue; }
        let mut memory = herdr_projects::memory::MemoryStore::from_sqlite(SqliteStore::open(&db_path).unwrap(), project.join(".state/objects"));
        memory.create_worker_snapshot(SnapshotRequest { schema_version: 1, task_id: "work".into(), profile: profile.name.clone(), domains: vec![], paths: vec![], pinned_keys: vec![], sensitivity: "default".into() },
            &profile.name, &profile.definition_digest, profile.config.digest.as_deref(), 32000, "Factory fixture instructions", unix_ms(), None).unwrap();
    }
}

fn insert_grant(db_path: &Path, inputs: &LaunchInputs) {
    let profile = inputs.effective_profile.as_ref().unwrap();
    let grant = ApprovalGrant { version: 1, scope: ApprovalScope::for_launch(inputs).unwrap(), policy: profile.permission_policy.clone(),
        issued_unix_ms: 0, expires_unix_ms: unix_ms() + 3_600_000 };
    let reference = grant.reference().unwrap();
    let payload = serde_json::to_vec(&grant).unwrap();
    rusqlite::Connection::open(db_path).unwrap().execute("INSERT INTO approval_grants(id,payload,payload_hash) VALUES(?1,?2,?3)",
        rusqlite::params![reference.id, String::from_utf8(payload).unwrap(), reference.digest]).unwrap();
}

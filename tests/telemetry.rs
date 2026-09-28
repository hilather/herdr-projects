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

struct Fixture { tmp: tempfile::TempDir, root: PathBuf, project: PathBuf, home: PathBuf, attempt: String, decided: i64 }

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
        db.set_scheduler_policy(snapshot.head, snapshot.scheduler.unwrap().policy.revision, 1, 2).unwrap();
        let snapshot = db.read_snapshot(None).unwrap();
        let binding = &snapshot.runtime_bindings[0];
        db.record_observations(snapshot.head, &[herdr_projects::reconcile::RuntimeObservation { binding: binding.id.clone(), binding_revision: binding.revision,
            task_revision: Some(snapshot.tasks[0].revision), observed_unix_ms: unix_ms(), collector: "herdr-git-v1".into(), config_digest: Some(digest.clone()),
            ..herdr_projects::reconcile::RuntimeObservation::default() }]).unwrap();
        let snapshot = db.read_snapshot(None).unwrap();
        db.set_project_state(snapshot.head, snapshot.control.unwrap().revision, ProjectState::Active, unix_ms(), Some(&digest)).unwrap();
        drop(db);
        plant_profile(&db_path, codex_profile(&config, "codex", &home));
        SqliteStore::open(&db_path).unwrap().record_native_capability_evidence(unix_ms(), unix_ms() + 3_600_000).unwrap();
        // Fixture only. Production code has no writer for this column.
        rusqlite::Connection::open(&db_path).unwrap().execute("UPDATE project_control SET factory_admission='on' WHERE singleton=1", []).unwrap();
        worker_snapshots(&project);
        let inputs = herdr_projects::admission::prepared_admission_inputs(&project).unwrap().expect("a ready candidate");
        insert_grant(&db_path, &inputs);
        herdr_projects::admission::admit_once(&project).unwrap();
        let (attempt, decided) = rusqlite::Connection::open(&db_path).unwrap()
            .query_row("SELECT attempt_id,decided_unix_ms FROM dispatch_decisions", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        plant_profile(&db_path, codex_profile(&config, "other", &base.join("other-home")));
        Fixture { tmp, root, project, home, attempt, decided }
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

    fn cli(&self, command: &str) -> (serde_json::Value, Vec<u8>) {
        let out = Command::new(BIN).env_clear().env("HOME", self.tmp.path().join("home")).env("PATH", "/usr/bin:/bin")
            .args(["--root", self.root.to_str().unwrap(), "telemetry", "demo", command]).output().unwrap();
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
}

fn attempt_usage(report: &serde_json::Value) -> serde_json::Value { report["attempts"][0]["usage"].clone() }
fn unavailable(reason: &str) -> serde_json::Value { serde_json::json!({"status": "unavailable", "reason": reason}) }

/// Gate: `0.154.0` is certified only by the live run (card S5/S7); until then
/// counters are not persisted and this test fails. No test hook certifies it.
#[test]
#[ignore = "gate: passes only after the live run certifies codex 0.154.0"]
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

fn contains_canary(bytes: &[u8]) -> bool {
    let lower = bytes.to_ascii_lowercase();
    lower.windows(6).any(|w| w == b"canary")
}

fn unix_ms() -> i64 { jiff::Timestamp::now().as_millisecond() }

fn codex_profile(config: &herdr_projects::migration::ConfigReference, name: &str, home: &Path) -> FrozenProfile {
    let evidence = VersionedReference { id: "sim-evidence".into(), revision: 1, digest: "a".repeat(64) };
    let supported = CapabilityEvidence::Supported { evidence: evidence.clone() };
    FrozenProfile {
        version: 1, name: name.into(), kind: "codex".into(), definition_digest: "b".repeat(64), config: config.clone(),
        arguments_digest: "c".repeat(64), environment_names: Vec::new(), execution_home: Some(home.display().to_string()),
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

/// Retained worker knowledge for the queued task and every retained profile.
fn worker_snapshots(project: &Path) {
    let db_path = project.join(".state/state.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let profiles = conn.prepare("SELECT report FROM native_profiles").unwrap()
        .query_map([], |row| row.get::<_, String>(0)).unwrap()
        .map(|report| serde_json::from_value::<FrozenProfile>(serde_json::from_str::<serde_json::Value>(&report.unwrap()).unwrap()["preparation"]["profile"].clone()).unwrap())
        .collect::<Vec<_>>();
    for profile in profiles {
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

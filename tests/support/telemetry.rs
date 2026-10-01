//! Shared telemetry test fixture: a real reserved Codex attempt and helpers
//! to write rollouts and run `herdr-projects telemetry` on the CLI. Used by
//! every telemetry test crate through `mod support;`.
#![allow(dead_code)] // Each test crate uses a different subset.

use herdr_projects::{domain::*, store::SqliteStore};
use sha2::{Digest, Sha256};
use std::{fs, path::{Path, PathBuf}, process::Command};

pub const BIN: &str = env!("CARGO_BIN_EXE_herdr-projects");
pub const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/telemetry/codex-0.154.0");
pub const SID: &str = "00000000-0000-4000-8000-00000000c0de";
/// sha256 of the canonical payloads, computed with `sha256sum` outside the crate.
pub const DIGEST_1: &str = "sha256:c56c0b798f22b872890b69470f332d6be530384c7f5d5b61d0ae3a41bb6f65bf";
pub const DIGEST_2: &str = "sha256:6c2dcf23c84488655ff53556237af61da2717e13172cdf50dad886d53e84be41";
pub const DIGEST_2B: &str = "sha256:49f430f4555d26b959282f29656f343ad7e239dadf49714298b21d46e61e698f";

pub struct Fixture { pub tmp: tempfile::TempDir, pub root: PathBuf, pub project: PathBuf, pub home: PathBuf, pub attempt: String, pub decided: i64, pub config: herdr_projects::migration::ConfigReference }

impl Fixture {
    /// Project `demo` with one attempt reserved by automatic admission on a Codex
    /// profile whose execution home is `codex-home`, and a second retained Codex
    /// profile (`other-home`) that no attempt uses. The attempt has the active
    /// collector binding its launch would write (`bind`), yet stays reserved so
    /// it can be cancelled and readmitted.
    pub fn new() -> Self {
        let f = Self::reserved();
        f.bind();
        f
    }

    /// The collector binding revision `apply_launch_started` writes for this
    /// attempt (tests/cli.rs checks it on a real launch). Fixture only: no
    /// launch happens here.
    pub fn bind(&self) {
        rusqlite::Connection::open(self.project.join(".state/state.db")).unwrap().execute("INSERT INTO collector_bindings(attempt_id,revision,state,collector,execution_home,unix_ms,source)
            VALUES(?1,1,'active','codex',?2,?3,'apply_launch_started')", rusqlite::params![self.attempt, self.home.display().to_string(), unix_ms()]).unwrap();
    }

    /// Like `new`, without a collector binding: the attempt is only reserved.
    pub fn reserved() -> Self {
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

    pub fn worktree(&self) -> String { format!("{}/.state/worktrees/{}/repo-00", self.project.display(), self.attempt) }

    /// Write `parts` of the fixture rollout under `home`, substituting the placeholders.
    pub fn rollout(&self, home: &Path, name: &str, parts: &[&str], cwd: &str, ts_ms: i64, version: &str) -> PathBuf {
        let dir = home.join(".codex/sessions/2026/09/28");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("rollout-2026-09-28T00-00-00-{name}.jsonl"));
        let text = parts.iter().map(|part| fs::read_to_string(Path::new(FIXTURES).join(part)).unwrap()).collect::<String>();
        let ts = jiff::Timestamp::from_millisecond(ts_ms).unwrap().to_string();
        fs::write(&path, text.replace("@SID@", SID).replace("@CWD@", cwd).replace("@TS@", &ts).replace("@VERSION@", version)).unwrap();
        path
    }

    pub fn cli(&self, command: &str) -> (serde_json::Value, Vec<u8>) { self.cli_args(&[command]) }

    pub fn cli_args(&self, args: &[&str]) -> (serde_json::Value, Vec<u8>) {
        let out = Command::new(BIN).env_clear().env("HOME", self.tmp.path().join("home")).env("PATH", "/usr/bin:/bin")
            .args(["--root", self.root.to_str().unwrap(), "telemetry", "demo"]).args(args).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let mut bytes = out.stdout.clone();
        bytes.extend_from_slice(&out.stderr);
        (serde_json::from_slice(&out.stdout).unwrap(), bytes)
    }

    /// Run a telemetry command in its text form; returns its stdout.
    pub fn text(&self, args: &[&str]) -> String {
        let out = Command::new(BIN).env_clear().env("HOME", self.tmp.path().join("home")).env("PATH", "/usr/bin:/bin")
            .args(["--root", self.root.to_str().unwrap(), "telemetry", "demo"]).args(args).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }

    /// Run a telemetry command that must fail; returns its stderr.
    pub fn cli_fail(&self, args: &[&str]) -> String {
        let out = Command::new(BIN).env_clear().env("HOME", self.tmp.path().join("home")).env("PATH", "/usr/bin:/bin")
            .args(["--root", self.root.to_str().unwrap(), "telemetry", "demo"]).args(args).output().unwrap();
        assert!(!out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
        String::from_utf8(out.stderr).unwrap()
    }

    pub fn sidecar(&self) -> rusqlite::Connection { rusqlite::Connection::open(self.project.join(".state/telemetry.db")).unwrap() }

    /// `(ordinal, payload_digest, accepted, reason, input, cached, output, reasoning, total)` per record.
    pub fn usage(&self) -> Vec<(i64, String, i64, Option<String>, Option<i64>, Option<i64>, Option<i64>, Option<i64>, Option<i64>)> {
        let db = self.sidecar();
        let mut stmt = db.prepare("SELECT ordinal,payload_digest,accepted,reason,input_tokens,cached_input_tokens,output_tokens,reasoning_output_tokens,total_tokens FROM codex_usage ORDER BY session_id,ordinal").unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?))).unwrap().map(Result::unwrap).collect()
    }

    pub fn count(&self, table: &str) -> i64 { self.sidecar().query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0)).unwrap() }

    /// `(binding, attempt_id)` of the one rollout source.
    pub fn binding(&self) -> (String, Option<String>) {
        self.sidecar().query_row("SELECT binding,attempt_id FROM rollout_sources", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap()
    }

    pub fn report(&self) -> serde_json::Value { self.cli_args(&["report", "--json"]).0 }

    /// Cancel the reserved attempt (released: the task is cancelled).
    pub fn cancel_reserved(&self) {
        let db_path = self.project.join(".state/state.db");
        let attempt: String = rusqlite::Connection::open(&db_path).unwrap().query_row("SELECT id FROM attempts WHERE state='reserved'", [], |r| r.get(0)).unwrap();
        let mut db = SqliteStore::open(&db_path).unwrap();
        assert!(db.cancel_attempt(&AttemptId::new(attempt).unwrap(), 1, db.current_head().unwrap(), "next arm", unix_ms()).unwrap().released);
    }

    /// Cancel the reserved attempt, queue the task again and admit it on `profile` only.
    pub fn readmit(&self, profile: &str) {
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

pub fn unix_ms() -> i64 { jiff::Timestamp::now().as_millisecond() }

pub fn codex_profile(config: &herdr_projects::migration::ConfigReference, kind: &str, name: &str, home: Option<&Path>) -> FrozenProfile {
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

pub fn plant_profile(db_path: &Path, profile: FrozenProfile) {
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
pub fn worker_snapshots(project: &Path, only: Option<&str>) {
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

pub fn insert_grant(db_path: &Path, inputs: &LaunchInputs) {
    let profile = inputs.effective_profile.as_ref().unwrap();
    let grant = ApprovalGrant { version: 1, scope: ApprovalScope::for_launch(inputs).unwrap(), policy: profile.permission_policy.clone(),
        issued_unix_ms: 0, expires_unix_ms: unix_ms() + 3_600_000 };
    let reference = grant.reference().unwrap();
    let payload = serde_json::to_vec(&grant).unwrap();
    rusqlite::Connection::open(db_path).unwrap().execute("INSERT INTO approval_grants(id,payload,payload_hash) VALUES(?1,?2,?3)",
        rusqlite::params![reference.id, String::from_utf8(payload).unwrap(), reference.digest]).unwrap();
}

/// Observe the public aggregate-backed surfaces, omitting observation clocks.
pub fn aggregate_read_snapshot(f: &Fixture) -> serde_json::Value {
    use serde_json::json;
    let report = f.report();
    let metrics: Vec<_> = ["M08", "M09", "M12", "M14", "M15", "M16", "M17", "M18"]
        .iter().map(|id| report["metrics"][id].clone()).collect();
    let cost = f.cli_args(&["view", "cost", "--json"]).0;
    let rows: Vec<_> = cost["rows"].as_array().unwrap().iter().map(|r|
        json!({"metric_id": r["metric_id"], "value": r["value"], "coverage": r["coverage"],
            "status": r["status"], "basis": r["basis"], "digest": r["projection"]["content_digest"]})).collect();
    json!({"metrics": metrics, "after_termination": report["after_termination"],
        "tools": f.cli_args(&["accounting", "tools", "--json"]).0, "cost": rows})
}

/// Exercise maintained reads against source replay and the analytics verifier.
pub fn verify_aggregate_replay(f: &Fixture, replay: &serde_json::Value) {
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(&aggregate_read_snapshot(f), replay, "maintained reads equal full derivation");
    f.cli_args(&["accounting", "reprice"]);
    let priced = aggregate_read_snapshot(f);
    let ledger = f.cli_args(&["accounting", "entries"]).1;
    let cost = f.cli_args(&["accounting", "cost", "--json"]).1;
    f.cli_args(&["query", "--metric", "M08,M09,M12,M14,M16,M17,M18", "--json"]);
    f.cli_args(&["analytics", "refresh"]);
    let pinned = f.cli_args(&["analytics", "snapshot"]).1;
    assert_eq!(f.cli_args(&["analytics", "rebuild", "--verify"]).0["identical"], true);
    f.cli_args(&["analytics", "rebuild"]);
    assert_eq!(f.cli_args(&["analytics", "snapshot"]).1, pinned);
    // The public store's missing projection marker selects full ledger replay.
    f.sidecar().execute("DELETE FROM usage_ledger", []).unwrap();
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(f.cli_args(&["accounting", "entries"]).1, ledger);
    assert_eq!(aggregate_read_snapshot(f), priced);
    assert_eq!(f.cli_args(&["accounting", "cost", "--json"]).1, cost);
    f.cli_args(&["analytics", "refresh"]);
    assert_eq!(f.cli_args(&["analytics", "rebuild", "--verify"]).0["identical"], true);
}

/// Plant the retained termination observation without launching a worker.
pub fn plant_aggregate_termination(f: &Fixture) -> i64 {
    f.cancel_reserved();
    let at = unix_ms();
    rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap().execute(
        "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('runtime.worker_terminated',?1,2,1,?2)",
        rusqlite::params![f.attempt, serde_json::json!({"version":1,"attempt":f.attempt,
            "cause":"cancellation","observed_unix_ms":at}).to_string()]).unwrap();
    at
}

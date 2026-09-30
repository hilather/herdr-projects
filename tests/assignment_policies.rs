//! TM4.7 propensity-logged assignment policies (docs/telemetry/contracts-evaluation.md §9),
//! end to end: the `telemetry <slug> policies` commands, automatic admission
//! in shadow and assignment mode, the owner-signed `randomized_assignment`
//! grant, and TM4.4's weighted comparison over the logged probabilities.
#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns.

use herdr_projects::{domain::*, store::SqliteStore};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, path::{Path, PathBuf}, process::Command};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-projects");
const GRANT_NS: &str = "randomized-assignment@herdr-projects";
const PROHIBITED: [&str; 5] = ["alter_review_policy", "alter_verification_policy", "choose_outside_eligible_set", "exceed_budget", "increase_permissions"];
const POLICY: &str = "{\"version\":1,\"checks\":[\"/usr/bin/git\",\"diff\",\"--quiet\"]}";

fn unix_ms() -> i64 { i64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis()).unwrap() }
fn sha256_id(bytes: &str) -> String { format!("sha256:{:x}", Sha256::digest(bytes.as_bytes())) }

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("/usr/bin/git").arg("-C").arg(repo).args(args).env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "f").env("GIT_AUTHOR_EMAIL", "f@example.invalid").env("GIT_COMMITTER_NAME", "f").env("GIT_COMMITTER_EMAIL", "f@example.invalid")
        .env("GIT_AUTHOR_DATE", "2020-01-01T00:00:00Z").env("GIT_COMMITTER_DATE", "2020-01-01T00:00:00Z").output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// Contracts §2 bytes of a `sim_profile` arm, written out by hand.
fn sim_configuration(version: &str) -> String {
    format!(concat!(r#"{{"adapter":{{"digest":"{a}","id":"sim-evidence","revision":1}},"agent_digest":"{e}","agent_version":"{version}","#,
        r#""arguments_digest":"{c}","definition_digest":"{b}","environment_names":[],"kind":"codex","#,
        r#""permission_policy":{{"digest":"{d}","id":"sim-policy","revision":1}},"reasoning_effort":null,"reasoning_effort_reason":"mapping_unverified","#,
        r#""requested_model":null,"requested_model_reason":"mapping_unverified","schema":"agent_configuration.v1"}}"#),
        a = "a".repeat(64), b = "b".repeat(64), c = "c".repeat(64), d = "d".repeat(64), e = "e".repeat(64), version = version)
}

fn sim_profile(config: &herdr_projects::migration::ConfigReference, name: &str, version: &str) -> FrozenProfile {
    let evidence = VersionedReference { id: "sim-evidence".into(), revision: 1, digest: "a".repeat(64) };
    let supported = CapabilityEvidence::Supported { evidence: evidence.clone() };
    FrozenProfile {
        version: 1, name: name.into(), kind: "codex".into(), definition_digest: "b".repeat(64), config: config.clone(), arguments_digest: "c".repeat(64),
        environment_names: Vec::new(), execution_home: None,
        permission_policy: VersionedReference { id: "sim-policy".into(), revision: 1, digest: "d".repeat(64) }, adapter: evidence,
        agent: ExecutableIdentity { path: "/usr/bin/git".into(), digest: "e".repeat(64), version: version.into() },
        herdr: ExecutableIdentity { path: "/usr/bin/git".into(), digest: "f".repeat(64), version: "1.0.0".into() },
        capabilities: ProfileCapabilities { launch: supported.clone(), readiness_observation: supported.clone(), prompt_submission: supported.clone(), stop: supported,
            checkpoint_acknowledgment: CapabilityEvidence::Unknown, structured_usage: CapabilityEvidence::Unknown, resume: CapabilityEvidence::Unknown },
        workflow_certificate: None,
    }
}

fn plant_profile(db_path: &Path, profile: &FrozenProfile) {
    use std::os::unix::fs::MetadataExt;
    profile.validate_for_launch().unwrap();
    let reference = profile.reference().unwrap();
    let canonical = fs::canonicalize(db_path).unwrap();
    let metadata = fs::metadata(&canonical).unwrap();
    let report = json!({"preparation": {"profile": profile, "reference": reference, "launchable": true, "protocol_capable": false, "certified": false},
        "source_store": [canonical, metadata.dev(), metadata.ino()]}).to_string();
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('profile.native_retained',?1,1,1,?2)",
        rusqlite::params![reference.id, serde_json::to_string(&reference).unwrap()]).unwrap();
    let sequence = conn.last_insert_rowid();
    conn.execute("INSERT INTO native_profiles(profile_digest,report,report_digest,sequence) VALUES(?1,?2,?3,?4)",
        rusqlite::params![reference.digest, report, format!("{:x}", Sha256::digest(report.as_bytes())), sequence]).unwrap();
}

/// A migrated, active project with the owner key, one git repository and two
/// retained arms (`sim` 1.0.0 and `sim-next` 0.154.1), automatic admission on.
/// `arms` is in admission's evaluation order (profile digest), with each
/// arm's configuration id computed from its hand-written bytes.
struct World { home: tempfile::TempDir, root: PathBuf, project: PathBuf, key: PathBuf, db_path: PathBuf, repository: String, oid: String,
    arms: Vec<(String, String)> }

impl World {
    /// `tasks` are queued up front (in rank order) with runtime bindings, fresh observations and contracts.
    fn new(tasks: &[&str]) -> Self {
        use herdr_projects::{migration, runtime};
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        let world_cmd = |args: &[&str]| Command::new(BIN).env_clear().env("HOME", home.path()).args(args).output().unwrap();
        for action in ["new", "pause"] { assert!(world_cmd(&["--root", root.to_str().unwrap(), action, "demo"]).status.success()); }
        let key = home.path().join("owner");
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&key).output().unwrap().status.success());
        let public = fs::read_to_string(key.with_extension("pub")).unwrap().split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        let project = root.join("demo");
        let config = home.path().join(".config/herdr-projects/config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, format!("[authority]\nversion=1\nrevision=1\napproval_public_key={public:?}\n")).unwrap();
        let plan = migration::inspect_with_config(&project, &config).unwrap();
        migration::apply(&project, &plan, true).unwrap();
        let s = runtime::snapshot(&project).unwrap();
        runtime::set_state(&project, s.head, s.control.unwrap().revision, ProjectState::Active, &config).unwrap();
        let reference = migration::config_reference(&config).unwrap();
        let digest = reference.digest.clone().unwrap();
        let repo = home.path().join("repo");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main", "--template="]);
        fs::write(repo.join("README"), "fixture\n").unwrap();
        git(&repo, &["add", "--", "README"]);
        git(&repo, &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "fixture"]);
        let oid = git(&repo, &["rev-parse", "HEAD"]);
        let repository = fs::canonicalize(&repo).unwrap().display().to_string();
        let db_path = project.join(".state/state.db");
        // Tasks and bindings pause the project; fresh observations of every binding let it resume.
        let head = || runtime::snapshot(&project).unwrap().head;
        for id in tasks {
            let task = TaskId::new(*id).unwrap();
            runtime::add_task(&project, task.clone(), (*id).into(), head()).unwrap();
            runtime::create_binding(&project, Some(&task), Some(1), head(), &RuntimeRoute::default()).unwrap();
            let revision = runtime::snapshot(&project).unwrap().tasks.iter().find(|t| t.id == task).unwrap().revision;
            runtime::queue_task(&project, &task, revision, head(), &QueueRequest { priority: 0, dependencies: Vec::new() }).unwrap();
        }
        let snapshot = runtime::snapshot(&project).unwrap();
        runtime::scheduler_policy(&project, snapshot.head, snapshot.scheduler.as_ref().unwrap().policy.revision, 1, 2).unwrap();
        let snapshot = runtime::snapshot(&project).unwrap();
        let observations: Vec<_> = snapshot.runtime_bindings.iter().map(|binding| herdr_projects::reconcile::RuntimeObservation { binding: binding.id.clone(),
            binding_revision: binding.revision, task_revision: binding.task.as_ref().and_then(|id| snapshot.tasks.iter().find(|t| &t.id == id)).map(|t| t.revision),
            observed_unix_ms: unix_ms(), collector: "herdr-git-v1".into(), config_digest: Some(digest.clone()), ..herdr_projects::reconcile::RuntimeObservation::default() }).collect();
        runtime::record_observations(&project, &herdr_projects::reconcile::ObservationBatch { expected_head: snapshot.head, observations, dispatch_allowed: false, recorded_head: None }).unwrap();
        let s = runtime::snapshot(&project).unwrap();
        runtime::set_state(&project, s.head, s.control.unwrap().revision, ProjectState::Active, &config).unwrap();
        let mut arms = Vec::new();
        for (name, version) in [("sim", "1.0.0"), ("sim-next", "0.154.1")] {
            let profile = sim_profile(&reference, name, version);
            plant_profile(&db_path, &profile);
            arms.push((profile.reference().unwrap().digest, name.to_owned(), sha256_id(&sim_configuration(version))));
        }
        arms.sort();
        SqliteStore::open(&db_path).unwrap().record_native_capability_evidence(unix_ms(), unix_ms() + 3_600_000).unwrap();
        // Fixture only. Production code has no writer for this column.
        rusqlite::Connection::open(&db_path).unwrap().execute("UPDATE project_control SET factory_admission='on' WHERE singleton=1", []).unwrap();
        let world = World { home, root, project, key, db_path, repository, oid, arms: arms.into_iter().map(|(_, name, id)| (name, id)).collect() };
        for id in tasks { world.contract(id); }
        world
    }
    fn hp(&self, args: &[&str]) -> std::process::Output {
        Command::new(BIN).env_clear().env("HOME", self.home.path()).env("PATH", "/usr/bin:/bin").arg("--root").arg(&self.root).args(args).output().unwrap()
    }
    fn policies(&self, args: &[&str]) -> Value {
        let mut all = vec!["telemetry", "demo", "policies"];
        all.extend_from_slice(args);
        let out = self.hp(&all);
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn policies_fail(&self, args: &[&str]) -> String {
        let mut all = vec!["telemetry", "demo", "policies"];
        all.extend_from_slice(args);
        let out = self.hp(&all);
        assert!(!out.status.success(), "{args:?} succeeded: {}", String::from_utf8_lossy(&out.stdout));
        String::from_utf8_lossy(&out.stderr).into_owned()
    }
    fn db(&self) -> rusqlite::Connection { rusqlite::Connection::open(&self.db_path).unwrap() }
    fn count(&self, sql: &str) -> i64 { self.db().query_row(sql, [], |r| r.get(0)).unwrap() }

    /// Task `id`'s verify-only contract writing `src/<id>.rs` at the fixture commit.
    fn contract(&self, id: &str) {
        let store = fs::canonicalize(&self.db_path).unwrap().display().to_string();
        let conn = self.db();
        let head: i64 = conn.query_row("SELECT COALESCE(MAX(sequence),0) FROM events", [], |r| r.get(0)).unwrap();
        let path = format!("src/{id}.rs");
        let raw = serde_json::to_vec(&json!({"version": 1, "project_store": store, "expected_head": head, "task_id": id, "contract_revision": 1,
            "deliverable": "fixture work", "non_goals": "no provider calls", "repository": self.repository, "base_oid": self.oid, "object_format": "sha1",
            "scope": {"paths": [{"path": path, "access": "write"}]}, "acceptance_policies": [{"id": "builds", "text": POLICY}], "dependencies": [],
            "capability_flags": ["discovered", "launchable"], "profile_kind": "codex", "retry_class": "none", "result_schema_id": "result-v1", "route": "verify_only",
            "authority": {"id": "fixture-owner", "revision": 1, "digest": "ab".repeat(32)}})).unwrap();
        let digest = format!("{:x}", Sha256::digest(&raw));
        conn.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('contract.installed',?1,1,1,?2)",
            rusqlite::params![id, json!({"digest": digest, "route": "verify_only"}).to_string()]).unwrap();
        let sequence = conn.last_insert_rowid();
        conn.execute("INSERT INTO task_contracts(task_id,contract_revision,plan_revision,project_store,expected_head,repository,base_oid,object_format,memory_snapshot_id,route,raw_bytes,raw_digest,installed_seq) VALUES(?1,1,NULL,?2,?3,?4,?5,'sha1',NULL,'verify_only',?6,?7,?8)",
            rusqlite::params![id, store, head, self.repository, self.oid, raw, digest, sequence]).unwrap();
        conn.execute("INSERT INTO acceptance_policies(task_id,contract_revision,policy_id,body) VALUES(?1,1,'builds',?2)", rusqlite::params![id, POLICY]).unwrap();
        conn.execute("INSERT INTO contract_scope_paths(task_id,contract_revision,ordinal,path,access,certainty) VALUES(?1,1,0,?2,'write','exact')", rusqlite::params![id, path]).unwrap();
        conn.execute("INSERT INTO resource_claims(task_id,contract_revision,ordinal,kind,resource,access,certainty) VALUES(?1,1,0,'path',?2,'write','exact')", rusqlite::params![id, path]).unwrap();
    }

    /// Retained worker knowledge of the queued task for `profile`.
    fn knowledge(&self, task: &str, profile: &str) {
        let conn = self.db();
        let reports: Vec<String> = conn.prepare("SELECT report FROM native_profiles").unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
        let profile = reports.iter().map(|r| serde_json::from_value::<FrozenProfile>(serde_json::from_str::<Value>(r).unwrap()["preparation"]["profile"].clone()).unwrap())
            .find(|p| p.name == profile).unwrap();
        let (task, revision): (String, i64) = conn.query_row("SELECT id,revision FROM tasks WHERE id=?1", [task], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        let exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM memory_snapshots WHERE task_id=?1 AND task_revision=?2 AND profile_name=?3)",
            rusqlite::params![task, revision, profile.name], |r| r.get(0)).unwrap();
        if exists { return; }
        let mut memory = herdr_projects::memory::MemoryStore::from_sqlite(SqliteStore::open(&self.db_path).unwrap(), self.project.join(".state/objects"));
        memory.create_worker_snapshot(SnapshotRequest { schema_version: 1, task_id: task, profile: profile.name.clone(), domains: vec![], paths: vec![], pinned_keys: vec![],
            sensitivity: "default".into() }, &profile.name, &profile.definition_digest, profile.config.digest.as_deref(), 32000, "Fixture instructions", unix_ms(), None).unwrap();
    }

    /// Owner launch approvals of the queued task for every arm: knowledge is
    /// retained last-in-order first, so the prepared inputs are that arm's own.
    fn approve_all(&self, task: &str) {
        for (name, _) in self.arms.iter().rev() {
            self.knowledge(task, name);
            let inputs = herdr_projects::admission::prepared_admission_inputs(&self.project).unwrap().unwrap_or_else(|| panic!("a ready candidate: {}", {let o=self.hp(&["scheduler", "demo", "inspect"]); format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))}));
            assert_eq!((&inputs.effective_profile.as_ref().unwrap().name, inputs.task.as_str()), (name, task));
            let profile = inputs.effective_profile.as_ref().unwrap();
            let grant = ApprovalGrant { version: 1, scope: ApprovalScope::for_launch(&inputs).unwrap(), policy: profile.permission_policy.clone(), issued_unix_ms: 0,
                expires_unix_ms: unix_ms() + 3_600_000 };
            let reference = grant.reference().unwrap();
            self.db().execute("INSERT INTO approval_grants(id,payload,payload_hash) VALUES(?1,?2,?3)",
                rusqlite::params![reference.id, serde_json::to_string(&grant).unwrap(), reference.digest]).unwrap();
        }
    }

    /// One admission wake; returns its reason.
    fn admit(&self) -> &'static str { herdr_projects::admission::admit_decision(&self.project).unwrap().reason }

    /// Add, approve and admit task `id`, then cancel its never-launched attempt to free the slot.
    fn run_task(&self, id: &str) -> &'static str {
        self.approve_all(id);
        let reason = self.admit();
        if reason == "reserved" { self.cancel(id); }
        reason
    }
    fn cancel(&self, id: &str) {
        let mut db = SqliteStore::open(&self.db_path).unwrap();
        let attempt: String = self.db().query_row("SELECT id FROM attempts WHERE task_id=?1 AND state='reserved'", [id], |r| r.get(0)).unwrap();
        assert!(db.cancel_attempt(&AttemptId::new(attempt).unwrap(), 1, db.current_head().unwrap(), "fixture", unix_ms()).unwrap().released);
    }

    /// `(chosen, eligible, chooser_kind, principal, reason_codes)` of task `id`'s decision.
    fn decision(&self, id: &str) -> (String, Value, String, String, String) {
        self.db().query_row("SELECT chosen_configuration_id,eligible,chooser_kind,chooser_principal,reason_codes FROM dispatch_decisions WHERE task_id=?1", [id],
            |r| Ok((r.get(0)?, serde_json::from_str(&r.get::<_, String>(1)?).unwrap(), r.get(2)?, r.get(3)?, r.get(4)?))).unwrap()
    }

    fn sign(&self, key: &Path, name: &str, body: &Value) -> (String, String, String) {
        let doc = self.home.path().join(name);
        let bytes = serde_json::to_vec_pretty(body).unwrap();
        fs::write(&doc, &bytes).unwrap();
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-Y", "sign", "-f"]).arg(key).args(["-n", GRANT_NS]).arg(&doc).output().unwrap().status.success());
        (doc.display().to_string(), doc.with_extension("json.sig").display().to_string(), format!("sha256:{:x}", Sha256::digest(&bytes)))
    }
    /// A `randomized_assignment` grant for this project, valid for an hour, with `changes` merged in.
    fn grant(&self, changes: Value) -> Value {
        let store = fs::canonicalize(&self.db_path).unwrap().display().to_string();
        let mut grant = json!({"schema": "randomized_assignment_authority.v1", "scope": "randomized_assignment", "issuer": "owner", "project_store": store,
            "policies": ["uniform.v1"], "max_exploration_ppm": 0, "arm_caps": {&self.arms[0].1: 100, &self.arms[1].1: 100},
            "valid_from_unix_ms": unix_ms() - 60_000, "expires_unix_ms": unix_ms() + 3_600_000, "prohibited_effects": PROHIBITED,
            "authority": herdr_projects::authority::policy_reference(&self.project).unwrap()});
        for (k, v) in changes.as_object().unwrap() { grant[k] = v.clone(); }
        grant
    }
    fn install(&self, name: &str, grant: &Value) -> String {
        let (doc, sig, digest) = self.sign(&self.key, name, grant);
        let installed = self.policies(&["authority", "import", &doc, &sig])["grant"].clone();
        assert_eq!((&installed["grant_id"], &installed["installed"]), (&json!(digest), &json!(true)));
        digest
    }
}

/// `policies simulate` on stated arms with a fixed seed.
fn simulate(dir: &Path, input: &Value, extra: &[&str]) -> Value {
    let file = dir.join(format!("input-{}.json", unix_ms()));
    fs::write(&file, input.to_string()).unwrap();
    let root = dir.join("root");
    let mut args = vec!["--root", root.to_str().unwrap(), "telemetry", "demo", "policies", "simulate", "--input", file.to_str().unwrap()];
    args.extend_from_slice(extra);
    let out = Command::new(BIN).env_clear().env("HOME", dir).args(&args).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

fn arm_id(c: char) -> String { format!("sha256:{}", c.to_string().repeat(64)) }
fn probabilities(evaluation: &Value) -> Vec<i64> { evaluation["arms"].as_array().unwrap().iter().map(|a| a["probability_ppm"].as_i64().unwrap()).collect() }

#[test]
fn policy_probabilities_match_hand_computed_literals() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b, c) = (arm_id('a'), arm_id('b'), arm_id('c'));
    // B has 3 successes and 1 failure in the class: posterior mean 4/6 beats A's and C's 1/2.
    let input = json!({"policies": ["deterministic.v1", "uniform.v1", {"policy": "epsilon.v1", "epsilon_ppm": 100000}],
        "arms": [{"configuration_id": a}, {"configuration_id": b, "successes": 3, "failures": 1}, {"configuration_id": c}]});
    // SplitMix64 from seed 0 first yields 0xe220a8397b1dcdaf: draw = 16294208416658607535 mod 1000000 = 607535.
    let report = simulate(dir.path(), &input, &["--seed", "0"]);
    let e = report["evaluations"].as_array().unwrap();
    assert_eq!(e.iter().map(|e| (e["seed"].as_str().unwrap(), e["draw_ppm"].as_i64().unwrap())).collect::<Vec<_>>(), [("0000000000000000", 607535); 3]);
    // deterministic.v1: the first allowed arm with probability 1.
    assert_eq!(probabilities(&e[0]), [1_000_000, 0, 0]);
    assert_eq!(e[0]["chosen_configuration_id"], json!(a));
    // uniform.v1: 1000000 / 3 = 333333 rem 1, the remainder to the first arm; 333334 <= 607535 < 666667 picks B.
    assert_eq!(probabilities(&e[1]), [333334, 333333, 333333]);
    assert_eq!(e[1]["chosen_configuration_id"], json!(b));
    // epsilon.v1 (10%): the 100000 split 33334/33333/33333, plus 900000 to the greedy arm B.
    assert_eq!(probabilities(&e[2]), [33334, 933333, 33333]);
    assert_eq!((&e[2]["greedy_configuration_id"], &e[2]["chosen_configuration_id"]), (&json!(b), &json!(b)));
    // Seed 1: 0x910a2dec89025cc1 mod 1000000 = 822465 >= 666667 picks C under uniform.v1.
    let report = simulate(dir.path(), &input, &["--seed", "1"]);
    assert_eq!((&report["evaluations"][1]["draw_ppm"], &report["evaluations"][1]["chosen_configuration_id"]), (&json!(822465), &json!(c)));

    // thompson.v1 (prior 1/1, floor 1%): Beta(21,1) against Beta(1,21) wins every one of the 1000 draws
    // (P(B > A) = 1/C(42,21) ≈ 2e-12), so A gets 10000 + 980000 and B the floor 10000.
    let dominated = json!({"policies": ["thompson.v1"], "arms": [{"configuration_id": a, "successes": 20}, {"configuration_id": b, "failures": 20}]});
    let t = &simulate(dir.path(), &dominated, &["--seed", "0"])["evaluations"][0];
    assert_eq!(probabilities(t), [990000, 10000]);
    assert_eq!(t["spec"], json!({"policy": "thompson.v1", "prior": [1, 1], "floor_ppm": 10000}));
    assert_eq!(t["thompson_wins"], json!([{"configuration_id": a, "wins": 1000}, {"configuration_id": b, "wins": 0}]));
    assert_eq!(t["chosen_configuration_id"], json!(a));

    // Cost and quota constraints: A reached its cap (1 decision), C has 0% quota left; B takes everything.
    let constrained = json!({"policies": [{"policy": "uniform.v1", "arm_caps": {&a: 1}}, {"policy": "epsilon.v1", "epsilon_ppm": 300000, "arm_caps": {&a: 1}}],
        "arms": [{"configuration_id": a, "assigned": 1}, {"configuration_id": b, "headroom_percent": "12.5"}, {"configuration_id": c, "headroom_percent": "0"}]});
    let e = simulate(dir.path(), &constrained, &["--seed", "0"])["evaluations"].clone();
    for evaluation in e.as_array().unwrap() {
        assert_eq!(probabilities(evaluation), [0, 1_000_000, 0]);
        assert_eq!((&evaluation["arms"][0]["excluded"], &evaluation["arms"][2]["excluded"]), (&json!("arm_cap_reached"), &json!("quota_exhausted")));
        assert_eq!(evaluation["chosen_configuration_id"], json!(b));
    }
    // No arm within the constraints: the policy abstains and chooses nothing.
    let none = json!({"policies": [{"policy": "uniform.v1", "min_headroom_percent": 20}], "arms": [{"configuration_id": b, "headroom_percent": "12.5"}]});
    let e = &simulate(dir.path(), &none, &[])["evaluations"][0];
    assert_eq!((probabilities(e), &e["abstained"], &e["chosen_configuration_id"]), (vec![0], &json!("no_arm_within_constraints"), &Value::Null));
}

#[test]
fn policies_never_choose_outside_the_eligible_set() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b, c, d) = (arm_id('a'), arm_id('b'), arm_id('c'), arm_id('d'));
    let caps = json!({&a: 2});
    let input = json!({"policies": [{"policy": "deterministic.v1", "arm_caps": caps}, {"policy": "uniform.v1", "arm_caps": caps},
            {"policy": "epsilon.v1", "epsilon_ppm": 500000, "arm_caps": caps}, {"policy": "thompson.v1", "arm_caps": caps, "floor_ppm": 50000}],
        "arms": [{"configuration_id": a, "assigned": 2, "successes": 40}, {"configuration_id": b, "successes": 2, "failures": 5},
            {"configuration_id": c, "headroom_percent": "0.000", "successes": 9}, {"configuration_id": d, "failures": 1}]});
    let report = simulate(dir.path(), &input, &["--seed", "0", "--sweep", "400"]);
    for e in report["evaluations"].as_array().unwrap() {
        let p = e["probabilities"].as_array().unwrap().iter().map(|v| v.as_i64().unwrap()).collect::<Vec<_>>();
        assert_eq!((p[0], p[2], p.iter().sum::<i64>()), (0, 0, 1_000_000), "{e}");
        assert_eq!((&e["outside_eligible"], &e["abstained"]), (&json!(0), &json!(0)), "{e}");
        let chosen = e["chosen"].as_object().unwrap();
        assert!(chosen.keys().all(|k| *k == b || *k == d), "{e}");
        assert_eq!(chosen.values().map(|v| v.as_i64().unwrap()).sum::<i64>(), 400);
    }
    // deterministic.v1 always takes the first allowed arm; uniform.v1 gives B and D 500000 each.
    assert_eq!(report["evaluations"][0]["chosen"], json!({&b: 400}));
    assert_eq!(report["evaluations"][1]["probabilities"], json!([0, 500000, 0, 500000]));
}

#[test]
fn shadow_mode_reports_disagreement_and_leaves_the_canonical_decision_unchanged() {
    let w = World::new(&["t0", "t1", "t2", "t3", "t4", "t5"]);
    // Default: off, no settings row, nothing recorded.
    let shown = w.policies(&["show"]);
    assert_eq!((&shown["mode"], &shown["default"], &shown["grants"]), (&json!("off"), &json!(true), &json!([])));
    assert!(w.policies_fail(&["suggest", "--task", "t0"]).contains("assignment policies are off"));
    let set = w.policies(&["configure", "--mode", "shadow", "--policy", "deterministic.v1", "--policy", "uniform.v1"]);
    assert_eq!((&set["settings"]["mode"], &set["settings"]["revision"], &set["settings"]["grant_id"]), (&json!("shadow"), &json!(1), &Value::Null));

    // The suggestion for a ready task: both arms approved, the rule's first arm first.
    w.approve_all("t0");
    let s = w.policies(&["suggest", "--task", "t0", "--json"]);
    assert_eq!((&s["mode"], &s["default_suggestion"], &s["task_class"]), (&json!("shadow"), &json!(false), &json!("code")));
    assert_eq!(s["eligible"].as_array().unwrap().iter().map(|e| (e["configuration_id"].as_str().unwrap(), e["status"].as_str().unwrap())).collect::<Vec<_>>(),
        [(w.arms[0].1.as_str(), "eligible"), (w.arms[1].1.as_str(), "eligible")]);
    assert_eq!((probabilities(&s["suggestion"]), &s["suggestion"]["chosen_configuration_id"]), (vec![1_000_000, 0], &json!(w.arms[0].1)));
    assert_eq!(probabilities(&s["policies"][1]), [500000, 500000]);
    let uniform_suggestion = s["policies"][1].clone();
    assert_eq!(w.count("SELECT count(*) FROM attempts"), 0, "a suggestion reserves nothing");

    assert_eq!(w.admit(), "reserved");
    w.cancel("t0");
    for id in ["t1", "t2", "t3", "t4", "t5"] { assert_eq!(w.run_task(id), "reserved"); }
    // Canonical: every decision is exactly the rule's, whatever the shadow policies would do.
    for id in ["t0", "t1", "t2", "t3", "t4", "t5"] {
        assert_eq!(w.decision(id), (w.arms[0].1.clone(), json!([
            {"configuration_id": w.arms[0].1, "probability_ppm": 1_000_000, "profile_digest": w.decision(id).1[0]["profile_digest"], "status": "chosen"},
            {"configuration_id": w.arms[1].1, "probability_ppm": 0, "profile_digest": w.decision(id).1[1]["profile_digest"], "status": "not_evaluated"}]),
            "automatic_admission".into(), "rule:automatic-admission.v1".into(), r#"["first_matching_approval"]"#.into()), "{id}");
    }
    assert_eq!(w.count("SELECT count(*) FROM dispatch_policy_assignments"), 0);

    let report = w.policies(&["shadow", "--json"]);
    let policies = report["policies"].as_array().unwrap();
    let find = |name: &str| policies.iter().find(|p| p["policy"] == name).unwrap().clone();
    let det = find("deterministic.v1");
    assert_eq!((&det["decisions"], &det["disagreements"], &det["disagreement_rate"]["value"]), (&json!(6), &json!(0), &json!("0/6")));
    // uniform.v1 shadow choices, computed independently: its digest is sha256('{"policy":"uniform.v1"}') =
    // sha256:2f8e33a7…449c; the seed of t<i> is the first 8 bytes of sha256("assignment-seed.v1\0<digest>\0t<i>\0<queued revision 3>"),
    // and SplitMix64's first output mod 1000000 gives draws 359724, 992822, 358761, 543981, 967475, 751440.
    // At 500000 each, a draw below 500000 picks the first arm (the rule's): t1, t3, t4 and t5 disagree.
    let rows: Vec<&Value> = report["decisions"].as_array().unwrap().iter().filter(|r| r["policy"] == "uniform.v1").collect();
    assert_eq!(rows.iter().map(|r| (r["task_id"].as_str().unwrap(), r["draw_ppm"].as_i64().unwrap())).collect::<Vec<_>>(),
        [("t0", 359724), ("t1", 992822), ("t2", 358761), ("t3", 543981), ("t4", 967475), ("t5", 751440)]);
    assert!(rows.iter().all(|r| (r["draw_ppm"].as_i64().unwrap() >= 500_000) == (r["suggested_configuration_id"] == json!(w.arms[1].1))));
    let uniform = find("uniform.v1");
    assert_eq!((&uniform["decisions"], &uniform["disagreements"], &uniform["disagreement_rate"]),
        (&json!(6), &json!(4), &json!({"value": "4/6", "decimal": "0.6667"})));
    assert_eq!(uniform["by_class"]["code"]["decisions"], json!(6));
    assert_eq!(rows[0]["seed"], uniform_suggestion["seed"], "the suggestion and the shadow record use the same seed");
    assert_eq!(report["canonical"], json!({"decisions": 6, "by_chooser": {"automatic_admission": {"shadowed": 6}}}));
}

#[test]
fn assignment_needs_the_switch_and_an_owner_signed_grant_and_respects_caps() {
    let w = World::new(&["p0", "p1", "p2", "p3", "p4"]);
    let (a, b) = (w.arms[0].1.clone(), w.arms[1].1.clone());
    // Without a grant the switch cannot turn assignment on.
    assert!(w.policies_fail(&["configure", "--mode", "assign", "--policy", "uniform.v1"]).contains("owner-signed randomized_assignment grant"));
    // A grant signed by any key but the owner's is refused at import.
    let other = w.home.path().join("other");
    assert!(Command::new("/usr/bin/ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&other).output().unwrap().status.success());
    let (doc, sig, _) = w.sign(&other, "forged.json", &w.grant(json!({})));
    assert!(w.policies_fail(&["authority", "import", &doc, &sig]).contains("signature verification failed"));
    // An owner grant bounds the policy and its exploration share.
    let epsilon_only = w.install("epsilon.json", &w.grant(json!({"policies": ["epsilon.v1"], "max_exploration_ppm": 200000})));
    assert!(w.policies_fail(&["configure", "--mode", "assign", "--policy", "uniform.v1", "--grant", &epsilon_only]).contains("does not permit uniform.v1"));
    assert!(w.policies_fail(&["configure", "--mode", "assign", "--policy", r#"{"policy":"epsilon.v1","epsilon_ppm":300000}"#, "--grant", &epsilon_only])
        .contains("exceeds the grant's max_exploration_ppm"));
    assert!(w.policies_fail(&["configure", "--mode", "shadow", "--policy", "uniform.v1", "--grant", &epsilon_only]).contains("--grant applies to assign only"));
    assert_eq!(w.policies(&["show"])["mode"], json!("off"));

    // Caps: two decisions per arm under this grant.
    let grant = w.install("uniform.json", &w.grant(json!({"arm_caps": {&a: 2, &b: 2}})));
    let set = w.policies(&["configure", "--mode", "assign", "--policy", "uniform.v1", "--policy", "deterministic.v1", "--grant", &grant]);
    let revision = set["settings"]["revision"].as_i64().unwrap();
    assert_eq!((&set["settings"]["mode"], &set["settings"]["grant_id"]), (&json!("assign"), &json!(grant)));

    let mut chosen = Vec::new();
    for i in 0..5 {
        let id = format!("p{i}");
        let reason = w.run_task(&id);
        if i == 4 {
            // Both arms at their cap: the policy abstains and nothing is reserved, whatever the rule would do.
            assert_eq!(reason, "policy_abstained", "{id}");
            assert_eq!(w.count("SELECT count(*) FROM attempts WHERE task_id='p4'"), 0);
            continue;
        }
        assert_eq!(reason, "reserved", "{id}");
        let (configuration, eligible, kind, principal, reasons) = w.decision(&id);
        let (seed, draw, policy, logged_grant, constraints): (String, i64, String, String, String) = w.db().query_row(
            "SELECT p.seed,p.draw_ppm,p.policy,p.grant_id,p.constraints FROM dispatch_policy_assignments p JOIN dispatch_decisions d ON d.attempt_id=p.attempt_id WHERE d.task_id=?1",
            [&id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).unwrap();
        assert_eq!((kind.as_str(), principal, reasons.as_str(), policy.as_str(), logged_grant.as_str(), seed.len()),
            ("automatic_admission", format!("policy:uniform.v1@{revision}"), r#"["exploration"]"#, "uniform.v1", grant.as_str(), 16));
        let logged: Vec<(String, i64, String)> = eligible.as_array().unwrap().iter()
            .map(|e| (e["configuration_id"].as_str().unwrap().to_owned(), e["probability_ppm"].as_i64().unwrap(), e["status"].as_str().unwrap().to_owned())).collect();
        let capped: Vec<&String> = [&a, &b].into_iter().filter(|arm| chosen.iter().filter(|c| c == arm).count() >= 2).collect();
        // Exactly the approved arms, never another; a capped arm is logged at 0 with its reason.
        let expected_p = |arm: &String| if capped.contains(&arm) { 0 } else if capped.is_empty() { 500_000 } else { 1_000_000 };
        assert_eq!(logged.iter().map(|l| (l.0.clone(), l.1)).collect::<Vec<_>>(), [(a.clone(), expected_p(&a)), (b.clone(), expected_p(&b))], "{id}");
        assert_eq!(serde_json::from_str::<Value>(&constraints).unwrap(), json!(capped.iter().map(|c| json!({"configuration_id": c, "reason": "arm_cap_reached"})).collect::<Vec<_>>()));
        // The draw decides: below the first arm's probability picks it.
        let expected = if draw < expected_p(&a) { &a } else { &b };
        assert_eq!(&configuration, expected, "{id} draw {draw}");
        assert!(logged.iter().all(|l| l.2 == if l.0 == configuration { "chosen" } else { "eligible" }));
        chosen.push(configuration);
    }
    assert_eq!((chosen.iter().filter(|c| **c == a).count(), chosen.iter().filter(|c| **c == b).count()), (2, 2));
    // Review and verification policy are the task's own: the reserved inputs pin the installed contract and its policies are untouched.
    assert_eq!(w.count("SELECT count(*) FROM acceptance_policies WHERE policy_id='builds' AND body='{\"version\":1,\"checks\":[\"/usr/bin/git\",\"diff\",\"--quiet\"]}'"), 5);
    assert_eq!(w.count("SELECT count(*) FROM attempt_inputs i JOIN task_contracts c ON c.task_id=json_extract(i.payload,'$.inputs.task') AND c.raw_digest=json_extract(i.payload,'$.inputs.task_contract.digest')"), 4);
    // The shadow record of the secondary policy disagrees whenever uniform chose the second arm.
    let report = w.policies(&["shadow", "--json"]);
    let det = report["policies"].as_array().unwrap().iter().find(|p| p["policy"] == "deterministic.v1").unwrap().clone();
    let uniform = report["policies"].as_array().unwrap().iter().find(|p| p["policy"] == "uniform.v1").unwrap().clone();
    assert_eq!((&det["decisions"], &det["disagreements"]), (&json!(4), &json!(2)));
    assert_eq!((&uniform["decisions"], &uniform["disagreements"]), (&json!(4), &json!(0)));
    let shown = w.policies(&["show"]);
    assert_eq!((&shown["mode"], &shown["assigned_decisions"], &shown["grants"].as_array().unwrap().len()), (&json!("assign"), &json!(4), &2));

    // Switching off returns admission to the rule: the task the policy held back is reserved by it.
    w.policies(&["configure", "--mode", "off"]);
    assert_eq!(w.admit(), "reserved");
    assert_eq!((w.decision("p4").0, w.decision("p4").3), (a.clone(), "rule:automatic-admission.v1".to_owned()));
    assert_eq!(w.count("SELECT count(*) FROM dispatch_policy_assignments"), 4);
}

#[test]
fn logged_probabilities_enable_the_weighted_compare_estimate() {
    let n = 48;
    let ids: Vec<String> = (0..n).map(|i| format!("c{i:02}")).collect();
    let w = World::new(&ids.iter().map(String::as_str).collect::<Vec<_>>());
    let (a, b) = (w.arms[0].1.clone(), w.arms[1].1.clone());
    let grant = w.install("uniform.json", &w.grant(json!({})));
    w.policies(&["configure", "--mode", "assign", "--policy", "uniform.v1", "--grant", &grant]);
    for i in 0..n { assert_eq!(w.run_task(&format!("c{i:02}")), "reserved"); }
    // Outcome evidence (fixture): every third task has a verified result, so it is accepted; the rest stay cancelled.
    let conn = w.db();
    let mut accepted = std::collections::BTreeMap::<String, (i64, i64)>::new();
    for i in 0..n {
        let id = format!("c{i:02}");
        let (attempt, configuration): (String, String) = conn.query_row("SELECT attempt_id,chosen_configuration_id FROM dispatch_decisions WHERE task_id=?1", [&id], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        let entry = accepted.entry(configuration).or_default();
        entry.1 += 1;
        if i % 3 == 0 {
            entry.0 += 1;
            let (submission, result) = (format!("{:x}", Sha256::digest(format!("s-{id}").as_bytes())), format!("{:x}", Sha256::digest(format!("r-{id}").as_bytes())));
            conn.execute("INSERT INTO result_submissions(submission_id,project_store,idempotency_key,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,artifact_manifest,claimed_checks,created_unix_ms)
                VALUES(?1,'store',?1,?2,'{}',?3,1,?2,?4,?5,?6,?6,'sha1','[]','[]',?7)", rusqlite::params![submission, "d".repeat(64), id, attempt, w.repository, w.oid, unix_ms()]).unwrap();
            conn.execute("INSERT INTO verified_results(result_id,run_id,submission_id,commit_oid,tree_oid,object_format,policy_digest,receipt_digest,isolation,memory_fence,created_unix_ms)
                VALUES(?1,?1,?2,?3,?3,'sha1',?4,?4,'linux-unshare-user-pid-mount-v1',0,?5)", rusqlite::params![result, submission, w.oid, "e".repeat(64), unix_ms()]).unwrap();
        }
    }
    drop(conn);
    // Every decision logged 500000 for both arms, so the Hájek weights are all 1000000 / 500000 = 2 / 2: the
    // weighted rate equals the raw rate and the effective sample size is the task count.
    assert_eq!(w.count("SELECT count(*) FROM dispatch_decisions d, json_each(d.eligible) e WHERE json_extract(e.value,'$.probability_ppm')=500000"), 2 * n);
    // Choices computed independently from the seeds (uniform.v1 digest, task c<i>, queued revision 3): 26 tasks
    // drew below 500000 (first arm), 22 did not; of the accepted c00, c03, …, c45, 9 are on the first arm and 7 on the second.
    let (ya, na) = accepted[&a];
    let (yb, nb) = accepted[&b];
    assert_eq!(((ya, na), (yb, nb)), ((9, 26), (7, 22)), "{accepted:?}");
    let out = w.hp(&["telemetry", "demo", "compare", "--metric", "M02", "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    let cell = report["results"][0]["cells"].as_array().unwrap().iter().find(|c| c["task_class"] == "code").unwrap().clone();
    assert_eq!(cell["propensity"], json!({"status": "available", "method": "hajek_ipw.v1"}));
    for (arm, value, weighted, decimal, t) in [(&a, "9/26", "9/26", "0.3462", 26), (&b, "7/22", "7/22", "0.3182", 22)] {
        let arm_cell = cell["arms"].as_array().unwrap().iter().find(|c| c["configuration_id"] == json!(arm)).unwrap();
        assert_eq!(arm_cell["value"], json!(value));
        assert_eq!(arm_cell["propensity_weighted"], json!({"method": "hajek_ipw.v1", "value": weighted, "decimal": decimal, "tasks": t,
            "effective_sample_size": format!("{t}.00"), "observational": true}), "{cell}");
    }
    assert_eq!(accepted.len(), 2);
}

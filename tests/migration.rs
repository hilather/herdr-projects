#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! Legacy-project migration workflows driven through the compiled CLI:
//! `migration inspect/plan/apply/recover/status`, `reconcile --record`,
//! `runtime state/admission/rebind/create`, `task add/list` and
//! `operations retire`. Legacy claim records are serialized from the public
//! claim types so fixtures track the on-disk format. Lost attempts that still
//! hold capacity have no CLI verb, so they are committed with the public store
//! API, as in tests/memory_regressions.rs.
use herdr_projects::{coordinator_prime, domain::*, launch_claim, migration, notification_claim, prompt_claim};
use serde_json::{json, Value};
use std::{fs, path::PathBuf, process::{Command, Output}};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-projects");

struct Project { home: tempfile::TempDir, project: PathBuf }

impl Project {
    /// A CLI-created project left `paused` or `archived`, as migration requires.
    fn new(lifecycle: &str) -> Self {
        let home = tempfile::tempdir().unwrap();
        let p = Project { project: home.path().join("root/demo"), home };
        for command in ["new", lifecycle] { p.ok(&[command, "demo"]); }
        p
    }
    fn cli(&self, args: &[&str]) -> Output {
        Command::new(BIN).env_clear().env("HOME", self.home.path()).env("PATH", "/usr/bin:/bin")
            .args(["--root", self.home.path().join("root").to_str().unwrap()]).args(args).output().unwrap()
    }
    fn ok(&self, args: &[&str]) -> Value {
        let out = self.cli(args);
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&out.stdout).into_owned()))
    }
    /// Asserts refusal and that the canonical store did not change.
    fn refused(&self, args: &[&str]) -> String {
        let before = self.snapshot();
        let out = self.cli(args);
        assert!(!out.status.success(), "{args:?} unexpectedly succeeded: {}", String::from_utf8_lossy(&out.stdout));
        assert_eq!(self.snapshot(), before, "{args:?} changed state while refusing");
        String::from_utf8_lossy(&out.stderr).into_owned()
    }
    fn write(&self, path: &str, bytes: impl AsRef<[u8]>) { fs::write(self.project.join(path), bytes).unwrap(); }
    fn read(&self, path: &str) -> Vec<u8> { fs::read(self.project.join(path)).unwrap() }
    fn thread(&self, id: &str, extra: &[(&str, toml::Value)]) {
        let mut value: toml::Table = toml::from_str(&format!("id = '{id}'\nstatus = 'resolved'\n")).unwrap();
        for (key, v) in extra { value.insert((*key).into(), v.clone()); }
        self.write(&format!("threads/{id}.toml"), toml::to_string(&value).unwrap());
    }
    /// Sorted blockers `migration inspect` reports for sources under `prefix`.
    fn blockers(&self, prefix: &str) -> Vec<String> {
        let plan = self.ok(&["migration", "demo", "inspect"]);
        let mut found: Vec<String> = plan["blockers"].as_array().unwrap().iter()
            .map(|b| b.as_str().unwrap()).filter(|b| b.starts_with(prefix))
            .map(|b| b.split_once(':').unwrap().0.to_owned()).collect();
        found.sort();
        found
    }
    fn plan(&self) -> String {
        let plan = self.home.path().join("plan.json");
        let _ = fs::remove_file(&plan);
        self.ok(&["migration", "demo", "plan", "--output", plan.to_str().unwrap()]);
        plan.display().to_string()
    }
    fn apply(&self, plan: &str) -> Output { self.cli(&["migration", "demo", "apply", "--plan", plan, "--writers-stopped"]) }
    fn migrate(&self) {
        let plan = self.plan();
        let out = self.apply(&plan);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(self.ok(&["migration", "demo", "status"])["phase"], "active");
    }
    /// Full canonical snapshot as `task list` prints it.
    fn snapshot(&self) -> Value { self.ok(&["task", "demo", "list"]) }
    fn head(&self) -> String { self.snapshot()["head"].to_string() }
    fn control(&self) -> Value { self.ok(&["runtime", "demo", "inspect"])["control"].clone() }
    fn set_state(&self, state: &str) -> Output {
        let (head, revision) = (self.head(), self.control()["revision"].to_string());
        self.cli(&["runtime", "demo", "state", state, "--expected-head", &head, "--expected-revision", &revision])
    }
    fn record(&self) { assert_eq!(self.ok(&["reconcile", "demo", "--record"])["dispatch_allowed"], false); }
    fn marker(&self) -> Value { serde_json::from_slice(&self.read(".state/format.json")).unwrap() }
    fn route(&self, name: &str, body: &str) -> String {
        let path = self.home.path().join(name);
        fs::write(&path, body).unwrap();
        path.display().to_string()
    }
}

fn digest(c: char) -> String { c.to_string().repeat(64) }
fn prompt(phase: prompt_claim::Phase, notified: bool) -> prompt_claim::Claim {
    let error = if phase == prompt_claim::Phase::Uncertain { "lost reply".into() } else { String::new() };
    prompt_claim::Claim { sequence: 1, execution: digest('a'), prompt: "brief".into(), phase, error, notified }
}
fn launch(generation: u64, phase: launch_claim::Phase, notified: bool) -> launch_claim::Claim {
    let error = if phase == launch_claim::Phase::Uncertain { "lost acknowledgement".into() } else { String::new() };
    launch_claim::Claim { sequence: 1, generation, execution: digest('a'), arguments_digest: digest('b'), route_digest: digest('c'),
        terminal: "terminal".into(), phase, error, notified }
}
fn notification(phase: notification_claim::Phase) -> notification_claim::Claim {
    let error = if matches!(phase, notification_claim::Phase::Pending | notification_claim::Phase::Confirmed) { String::new() } else { "pending delivery".into() };
    notification_claim::Claim { sequence: 1, batch: Some(notification_claim::Batch::new(vec!["item-a".into()]).unwrap()),
        mode: notification_claim::Mode::Toast, authority: digest('a'), payload: "fixture".into(), phase, retry_of: None, error }
}
fn t<T: serde::Serialize>(value: &T) -> toml::Value { toml::Value::try_from(value).unwrap() }

/// A legacy project whose records still carry unfinished external effects is
/// refused per record; once the operator resolves them, the confirmed history
/// imports and every legacy source keeps its exact bytes.
#[test]
fn legacy_records_with_unreconciled_effects_block_migration_until_resolved() {
    use launch_claim::Phase as L;
    use prompt_claim::Phase as P;
    let p = Project::new("pause");
    let empty = || toml::Value::Table(Default::default());
    let mut bad_execution = prompt(P::Confirmed, true);
    bad_execution.execution = "invalid".into();
    let blocked: Vec<(&str, Vec<(&str, toml::Value)>)> = vec![
        ("t-live-copy", vec![("pending_live_copy", empty())]),
        ("t-final-copy", vec![("pending_final_copy", empty())]),
        ("t-final-notice", vec![("pending_final_notice", empty())]),
        ("t-final-counter", vec![("final_copy_sequence", toml::Value::Integer(-1))]),
        ("t-brief-pending", vec![("prompt_sequence", 1.into()), ("prompt_claim", t(&prompt(P::Pending, false)))]),
        ("t-brief-corrupt", vec![("prompt_sequence", 1.into()), ("prompt_claim", t(&bad_execution))]),
        ("t-brief-unnotified", vec![("prompt_sequence", 1.into()), ("prompt_claim", t(&prompt(P::Uncertain, false)))]),
        ("t-launch-pending", vec![("launch_sequence", 1.into()), ("launch_claim", t(&launch(0, L::Pending, false)))]),
        ("t-launch-unnotified", vec![("launch_sequence", 1.into()), ("launch_claim", t(&launch(0, L::Uncertain, false)))]),
    ];
    let clean: Vec<(&str, Vec<(&str, toml::Value)>)> = vec![
        ("t-brief-confirmed", vec![("prompt_sequence", 1.into()), ("prompt_claim", t(&prompt(P::Confirmed, true)))]),
        ("t-launch-confirmed", vec![("launch_sequence", 1.into()), ("launch_claim", t(&launch(0, L::Confirmed, true)))]),
        // A resolved thread may retain notified, uncertain history.
        ("t-launch-history", vec![("launch_sequence", 1.into()), ("launch_claim", t(&launch(0, L::Uncertain, true)))]),
    ];
    for (id, extra) in blocked.iter().chain(&clean) { p.thread(id, extra); }
    let mut expected: Vec<String> = blocked.iter().map(|(id, _)| format!("threads/{id}.toml")).collect();
    expected.sort();
    assert_eq!(p.blockers("threads/"), expected);

    // Coordinator prime and start records: only confirmed deliveries within the
    // recorded prime request import.
    let prime = |phase, notified, request: u64| json!({"prime_request": request, "prime_sequence": 1,
        "prime_claim": coordinator_prime::Claim { request: 1, delivery: prompt(phase, notified) }});
    let start = |generation, phase, notified| json!({"prime_request": 1, "launch_sequence": 1, "launch_claim": launch(generation, phase, notified)});
    for (record, refused) in [
        (json!({"prime_request": 1, "prime_sequence": 1, "prime_claim": null}), false),
        (prime(P::Pending, false, 1), true),
        (prime(P::Uncertain, true, 1), true),
        (prime(P::Confirmed, true, 0), true),
        (prime(P::Confirmed, true, 1), false),
        (start(1, L::Pending, false), true),
        (start(1, L::Uncertain, true), true),
        (start(2, L::Confirmed, true), true),
        (start(1, L::Confirmed, true), false),
    ] {
        p.write(".state/coordinator.json", serde_json::to_vec(&record).unwrap());
        assert_eq!(p.blockers(".state/coordinator.json").len(), usize::from(refused), "{record}");
    }
    // Ticker notification claims and unconsumed suppressions.
    use notification_claim::Phase as N;
    let ticker = |phase, suppressed: Value| json!({"notification_sequence": 1, "notification_claim": notification(phase), "notification_suppressed": suppressed});
    for (record, refused) in [
        (ticker(N::Pending, json!([])), true),
        (ticker(N::Uncertain, json!([])), true),
        (ticker(N::NotShown, json!([])), true),
        (ticker(N::Confirmed, json!([])), false),
        (ticker(N::Suppressed, json!(["item-a"])), true),
        (ticker(N::Suppressed, json!([])), false),
    ] {
        p.write(".state/ticker.json", serde_json::to_vec(&record).unwrap());
        assert_eq!(p.blockers(".state/ticker.json").len(), usize::from(refused), "{record}");
    }

    // A plan written while blocked cannot be applied.
    let plan = p.plan();
    assert!(!p.apply(&plan).status.success());
    assert!(!p.project.join(".state/state.db").exists());

    // The operator resolves the blocked threads; the confirmed history imports.
    for (id, _) in &blocked { fs::remove_file(p.project.join(format!("threads/{id}.toml"))).unwrap(); }
    let kept = ["threads/t-brief-confirmed.toml", "threads/t-launch-confirmed.toml", "threads/t-launch-history.toml", ".state/coordinator.json", ".state/ticker.json"];
    let before: Vec<Vec<u8>> = kept.iter().map(|path| p.read(path)).collect();
    assert!(p.blockers("").is_empty(), "{:?}", p.blockers(""));
    p.migrate();
    let snapshot = p.snapshot();
    let mut tasks: Vec<&str> = snapshot["tasks"].as_array().unwrap().iter().map(|t| t["id"].as_str().unwrap()).collect();
    tasks.sort();
    assert_eq!(tasks, ["legacy-t-brief-confirmed", "legacy-t-launch-confirmed", "legacy-t-launch-history"]);
    assert_eq!(kept.iter().map(|path| p.read(path)).collect::<Vec<_>>(), before);
}

/// After migration the project stays paused until every binding has fresh
/// evidence recorded under the current config; pausing again advances the
/// fence epoch and never rewrites the legacy lifecycle record.
#[test]
fn migrated_project_resumes_only_with_matching_evidence_and_pause_fences_the_epoch() {
    let p = Project::new("pause");
    p.thread("t-0001", &[]);
    let legacy = p.read(".state/project.json");
    p.migrate();
    let control = p.control();
    assert_eq!(control["state"], "paused");
    assert_eq!(control["reconciliation_required"], true);

    // No evidence yet.
    let admission = p.ok(&["runtime", "demo", "admission"]);
    assert!(admission["blockers"].to_string().contains("thread:t-0001: fresh matching observation required"), "{admission}");
    assert!(!p.set_state("active").status.success());
    assert_eq!(p.control(), control);

    // Evidence recorded under one config does not authorize another.
    p.record();
    let config = p.home.path().join(".config/herdr-projects/config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(&config, "# changed after observation\n").unwrap();
    assert!(!p.set_state("active").status.success());
    assert_eq!(p.control()["state"], "paused");

    p.record();
    let out = p.set_state("active");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let active = p.control();
    assert_eq!(active["state"], "active");
    assert_eq!(active["reconciliation_required"], false);
    assert!(active["config_digest"].is_string());
    assert_eq!(p.marker()["reconciliation_required"], false);

    assert!(p.set_state("paused").status.success());
    let paused = p.control();
    assert_eq!(paused["state"], "paused");
    assert!(paused["epoch"].as_u64() > active["epoch"].as_u64());
    assert!(paused["config_digest"].is_null());
    assert_eq!(p.marker()["reconciliation_required"], true);
    assert_eq!(p.read(".state/project.json"), legacy);
}

/// An archived legacy project stays archived; and an unselected lost attempt
/// that still holds capacity blocks resume, rebinding and new bindings for its
/// task until termination is observed.
#[test]
fn archived_projects_and_retained_attempts_refuse_resume_rebind_and_binding_creation() {
    let p = Project::new("archive");
    p.thread("t-0001", &[]);
    p.migrate();
    assert_eq!(p.control()["state"], "archived");
    p.record();
    let stderr = p.refused(&["runtime", "demo", "state", "active", "--expected-head", &p.head(), "--expected-revision", &p.control()["revision"].to_string()]);
    assert!(stderr.contains("restore archived project to paused"), "{stderr}");
    assert!(p.set_state("paused").status.success());
    p.ok(&["task", "demo", "add", "new-task", "--title", "new task", "--expected-head", &p.head()]);

    // Lost attempts are recorded by workers, not by an operator verb.
    let mut db = migration::open_active(&p.project).unwrap();
    let lost = |id: &str, task: &str| Mutation::Attempt { expected: None, next: Attempt { id: AttemptId::new(id).unwrap(),
        task: TaskId::new(task).unwrap(), revision: 1, state: AttemptState::Lost, snapshot: None, reservation: format!("held-{id}"), termination_observed: false } };
    for mutation in [lost("lost-legacy", "legacy-t-0001"), lost("lost-new", "new-task")] {
        let head = db.read_snapshot(None).unwrap().head;
        db.commit(Commit { expected_head: head, mutations: vec![mutation] }).unwrap();
    }
    drop(db);

    p.record();
    let admission = p.ok(&["runtime", "demo", "admission"]);
    assert!(admission["blockers"].to_string().contains("attempt termination remains unobserved; capacity retained"), "{admission}");
    p.refused(&["runtime", "demo", "state", "active", "--expected-head", &p.head(), "--expected-revision", &p.control()["revision"].to_string()]);

    let route = p.route("route.json", r#"{"socket":"/new/session.sock","workspace_id":"w","tab_id":"t","pane_id":"p","cwd":"/worktree"}"#);
    let stderr = p.refused(&["runtime", "demo", "rebind", "thread:t-0001", "--route", &route, "--expected-revision", "1", "--expected-head", &p.head()]);
    assert!(stderr.contains("reconcile every retained attempt before rebinding"), "{stderr}");
    let stderr = p.refused(&["runtime", "demo", "create", "--task", "new-task", "--task-revision", "1", "--route", &route, "--expected-head", &p.head()]);
    assert!(stderr.contains("reconcile every retained attempt"), "{stderr}");
    // Task identity and its expected revision must be supplied together.
    p.refused(&["runtime", "demo", "create", "--task", "new-task", "--route", &route, "--expected-head", &p.head()]);
    p.refused(&["runtime", "demo", "create", "--task-revision", "1", "--route", &route, "--expected-head", &p.head()]);
    let snapshot = p.snapshot();
    assert!(snapshot["attempts"].as_array().unwrap().iter().all(|a| a["state"] == "lost" && a["termination_observed"] == false));
    assert_eq!(snapshot["runtime_bindings"].as_array().unwrap().len(), 1);
}

/// New bindings are fenced by task revision and pane identity, carry no legacy
/// provenance, leave imported bindings untouched, pause an active project and
/// survive recovery without writing legacy runtime files.
#[test]
fn canonical_bindings_created_after_resume_keep_imported_provenance_and_repause() {
    let p = Project::new("pause");
    p.thread("t-0001", &[]);
    p.migrate();
    let legacy_thread = p.read("threads/t-0001.toml");
    p.ok(&["task", "demo", "add", "new-task", "--title", "new task", "--expected-head", &p.head()]);
    let imported = p.snapshot()["runtime_bindings"][0].clone();
    assert_eq!(imported["id"], "thread:t-0001");
    assert!(imported["source_path"].is_string());
    p.record();
    assert!(p.set_state("active").status.success());
    let active = p.control();

    let route = p.route("route.json", r#"{"socket":"/new/session.sock","workspace_id":"w","tab_id":"t","pane_id":"p","cwd":"/worktree"}"#);
    let create = |revision: &str| p.cli(&["runtime", "demo", "create", "--task", "new-task", "--task-revision", revision, "--route", &route, "--expected-head", &p.head()]);
    assert!(!create("2").status.success(), "stale task revision");
    let out = create("1");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let created: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(created["binding"]["id"], "task:new-task");
    assert!(created["binding"]["source_path"].is_null() && created["binding"]["source_digest"].is_null());
    assert_eq!(created["task_revision"], 2);
    let control = p.control();
    assert_eq!(control["state"], "paused");
    assert!(control["epoch"].as_u64() > active["epoch"].as_u64());
    let bindings = p.snapshot()["runtime_bindings"].clone();
    assert_eq!(bindings.as_array().unwrap().iter().find(|b| b["id"] == "thread:t-0001").unwrap(), &imported);

    p.refused(&["runtime", "demo", "create", "--task", "new-task", "--task-revision", "2", "--route", &route, "--expected-head", &p.head()]);
    let stderr = p.refused(&["runtime", "demo", "create", "--route", &route, "--expected-head", &p.head()]);
    assert!(stderr.contains("pane already referenced"), "{stderr}");
    let empty = p.route("empty.json", "{}");
    assert_eq!(p.ok(&["runtime", "demo", "create", "--route", &empty, "--expected-head", &p.head()])["binding"]["id"], "coordinator");

    p.ok(&["migration", "demo", "recover", "--writers-stopped"]);
    assert_eq!(p.snapshot()["runtime_bindings"].as_array().unwrap().len(), 3);
    assert!(!p.project.join("threads/new-task.toml").exists());
    assert!(!p.project.join(".state/coordinator.json").exists());
    assert_eq!(p.read("threads/t-0001.toml"), legacy_thread);
}

/// A legacy finalization whose receipt does not match the thread imports as an
/// ambiguous intent; retiring it stops delivery without claiming the effect is
/// absent and without touching tasks, attempts or the operation itself.
#[test]
fn retiring_an_imported_ambiguous_finalization_keeps_the_effect_possible() {
    let p = Project::new("pause");
    p.write("threads/t-0001.toml", "id='t-0001'\nstatus='resolved'\npr='https://example.invalid/pr/1'\nresolved_reason='merged'\nlast_finalization='final-1'\n");
    p.write(".state/ticker.json", serde_json::to_vec(&json!({"nudged": "other-hash", "notification_retry": {"hash": "notification-hash"},
        "finalizations": {"t-0001": {"operation_id": "final-1", "fingerprint": "old-identity", "pr": "https://example.invalid/pr/1", "reason": "merged"}}})).unwrap());
    p.migrate();
    let before = p.snapshot();
    let operation = before["operations"].as_array().unwrap().iter().find(|o| o["kind"] == "legacy.finalize").unwrap()["id"].as_str().unwrap().to_owned();
    let head = before["head"].to_string();
    let retire = ["operations", "demo", "retire", &operation, "--reason", "superseded by operator", "--expected-revision", "1", "--expected-head", &head];
    let delivery = p.ok(&retire);
    assert_eq!(delivery["state"], "permanent_failure");
    assert!(delivery["last_outcome"].to_string().contains("effect remains possible"), "{delivery}");
    p.refused(&retire);
    let after = p.snapshot();
    for field in ["tasks", "attempts", "operations"] { assert_eq!(after[field], before[field], "{field}"); }
    let listed = p.ok(&["operations", "demo", "inspect"]);
    assert_eq!(listed.as_array().unwrap().iter().find(|d| d["operation"] == operation.as_str()).unwrap()["state"], "permanent_failure");
}

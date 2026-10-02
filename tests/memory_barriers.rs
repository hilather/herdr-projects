#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! The memory completion barrier: a task cannot succeed while its attempt's
//! consumed memory is stale. Memory moves only through the compiled CLI
//! (`memory propose/review/promote`, signed `memory import` policy and
//! `memory reconcile`, `memory update/ack/package`) and readiness is read with
//! `memory readiness`. Attempts, snapshots, consumer-binding retirement, package
//! acknowledgment and task completion use the public store API because no
//! worker is launched.
use herdr_farm::{authority, domain::*, memory::*, migration, runtime, store::{SqliteStore, StoreError, UpdatePackageAck}};
use serde_json::{json, Value};
use std::{fs, path::PathBuf, process::{Command, Output}};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-farm");

struct Project { home: tempfile::TempDir, project: PathBuf, key: PathBuf, store: String }

impl Project {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let key = home.path().join("owner");
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&key).status().unwrap().success());
        let public = fs::read_to_string(key.with_extension("pub")).unwrap().split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        let owner = home.path().join("owner.toml");
        fs::write(&owner, format!("[authority]\nversion=1\nrevision=1\napproval_public_key={public:?}\n")).unwrap();
        let mut p = Project { project: home.path().join("root/demo"), key, store: String::new(), home };
        for command in ["new", "pause"] { p.ok(&[command, "demo"]); }
        migration::apply(&p.project, &migration::inspect_with_config(&p.project, &owner).unwrap(), true).unwrap();
        let s = runtime::snapshot(&p.project).unwrap();
        runtime::set_state(&p.project, s.head, s.control.unwrap().revision, ProjectState::Active, &owner).unwrap();
        p.store = p.project.join(".state/state.db").canonicalize().unwrap().display().to_string();
        p
    }
    fn cli(&self, args: &[&str]) -> Output {
        Command::new(BIN).env_clear().env("HOME", self.home.path()).env("PATH", "/usr/bin:/bin")
            .args(["--root", self.home.path().join("root").to_str().unwrap()]).args(args).output().unwrap()
    }
    fn ok(&self, args: &[&str]) -> Value {
        let out = self.cli(args);
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
    }
    fn head(&self) -> u64 { runtime::snapshot(&self.project).unwrap().head }
    fn path(&self, name: &str) -> PathBuf { self.home.path().join(name) }
    fn sign(&self, name: &str, bytes: &[u8], namespace: &str) -> (PathBuf, PathBuf) {
        let path = self.path(name);
        fs::write(&path, bytes).unwrap();
        let signature = PathBuf::from(format!("{}.sig", path.display()));
        let _ = fs::remove_file(&signature);
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-Y", "sign", "-f"]).arg(&self.key).args(["-n", namespace]).arg(&path).output().unwrap().status.success());
        (path, signature)
    }
    fn db(&self) -> SqliteStore { migration::open_active(&self.project).unwrap() }
    fn memory(&self) -> MemoryStore { MemoryStore::from_sqlite(self.db(), self.project.join(".state/objects")) }
    fn add_task(&self, task: &str) { self.ok(&["task", "demo", "add", task, "--title", task, "--expected-head", &self.head().to_string()]); }
    fn snapshot(&self, task: &str, domains: &[&str]) -> String {
        let request = SnapshotRequest { schema_version: 1, task_id: task.into(), profile: "worker".into(),
            domains: domains.iter().map(|d| d.to_string()).collect(), paths: vec![], pinned_keys: vec![], sensitivity: "default".into() };
        self.memory().create_task_snapshot(request, "worker", &"a".repeat(64), None, 32_000, "Project instructions",
            jiff::Timestamp::now().as_millisecond(), None).unwrap().id.as_str().into()
    }
    /// Record `{task}-attempt` as the task's running attempt on `snapshot`.
    fn attempt(&self, task: &str, snapshot: Option<String>) {
        let mut db = self.db();
        let state = db.read_snapshot(None).unwrap();
        let mut next = state.tasks.iter().find(|t| t.id.as_str() == task).unwrap().clone();
        let previous = next.revision;
        next.revision += 1;
        next.active_attempt = Some(AttemptId::new(format!("{task}-attempt")).unwrap());
        db.commit(Commit { expected_head: state.head, mutations: vec![
            Mutation::Attempt { expected: None, next: Attempt { id: AttemptId::new(format!("{task}-attempt")).unwrap(), task: next.id.clone(), revision: 1,
                state: AttemptState::Running, snapshot, reservation: format!("{task}-slot"), termination_observed: false } },
            Mutation::Task { expected: Some(previous), next },
        ] }).unwrap();
    }
    /// A task whose running attempt consumed a fresh snapshot scoped to `domains`.
    fn worker(&self, task: &str, domains: &[&str]) -> String {
        self.add_task(task);
        let snapshot = self.snapshot(task, domains);
        self.attempt(task, Some(snapshot.clone()));
        snapshot
    }
    fn id(&self, key: &str) -> String {
        self.ok(&["memory", "demo", "inspect"])["records"].as_array().unwrap().iter().find(|r| r["record_key"] == key).unwrap()["id"].as_str().unwrap().into()
    }
    /// Propose, owner-review and promote one change from `producer`'s attempt;
    /// `extra` overrides fields of the change (`expected`, `based_on`, `impact`).
    fn promote(&self, producer: &str, snapshot: &str, key: &str, domains: &[&str], extra: Value) {
        let body = self.memory().ingest_object(format!("{key} {}", self.head()).as_bytes()).unwrap();
        let id = format!("proposal-{key}-{}", self.head());
        let mut proposal = json!({"schema_version":1,"proposal_id":id,"producer":{"task_id":producer,"attempt_id":format!("{producer}-attempt")},
            "input_snapshot_id":snapshot,"changes":[{"record_key":key,"kind":"observation","scope":{"domains":domains,"paths":[]},
            "claim":key,"body_object":body.as_str(),"evidence":[],"based_on":[],"impact":"informational"}]});
        if let Value::Object(extra) = extra { for (k, v) in extra { proposal["changes"][0][k] = v; } }
        let path = self.path(&format!("{id}.json"));
        fs::write(&path, serde_json::to_vec(&proposal).unwrap()).unwrap();
        let receipt = self.ok(&["memory", "demo", "propose", "--input", path.to_str().unwrap()]);
        let head = self.head();
        let review = json!({"version":1,"project_store":self.store,"authority":authority::policy_reference(&self.project).unwrap(),
            "expected_head":head,"expires_unix_ms":jiff::Timestamp::now().as_millisecond()+60_000,"proposal_digest":receipt["payload_digest"],
            "record_keys":[key],"review":{"schema_version":1,"proposal_id":id,"decision":"approve","reason":"reviewed"}});
        let (doc, sig) = self.sign(&format!("{id}-review.json"), &serde_json::to_vec(&review).unwrap(), authority::MEMORY_REVIEW_NAMESPACE);
        let decision = self.ok(&["memory", "demo", "review", "--proposal", &id, "--decision-file", doc.to_str().unwrap(), "--signature", sig.to_str().unwrap(), "--expected-head", &head.to_string()]);
        self.ok(&["memory", "demo", "promote", "--proposal", &id, "--decision", decision["id"].as_str().unwrap()]);
    }
    /// Revise `key` from revision `from`.
    fn revise(&self, producer: &str, snapshot: &str, key: &str, from: u64, impact: &str) {
        self.promote(producer, snapshot, key, &["ui"], json!({"expected":{"record_id":self.id(key),"revision":from},"impact":impact}));
    }
    fn policy(&self, op: &str, key: &str) {
        let head = self.head();
        let body = json!({"version":1,"project_store":self.store,"revision":1,"authority":authority::policy_reference(&self.project).unwrap(),
            "expected_head":head,"op":op,"record_key":key});
        let (doc, sig) = self.sign(&format!("policy-{op}-{key}.json"), &serde_json::to_vec(&body).unwrap(), authority::MEMORY_SIGNATURE_NAMESPACE);
        self.ok(&["memory", "demo", "import", doc.to_str().unwrap(), sig.to_str().unwrap(), "--expected-head", &head.to_string()]);
    }
    /// Owner-signed reconciliation of every open invalidation for `task`.
    fn reconcile(&self, task: &str) {
        let open: Vec<Value> = self.ok(&["memory", "demo", "invalidations", "--task", task]).as_array().unwrap().iter()
            .filter(|i| i["resolved_seq"].is_null()).map(|i| json!({"id":i["id"],"task_id":task,"triggering_seq":i["triggering_seq"]})).collect();
        let head = self.head();
        let body = json!({"version":1,"id":format!("reconcile-{head}"),"project_store":self.store,"authority":authority::policy_reference(&self.project).unwrap(),
            "expected_head":head,"expires_unix_ms":jiff::Timestamp::now().as_millisecond()+60_000,"reason":"reviewed","invalidations":open});
        let (doc, sig) = self.sign("reconcile.json", &serde_json::to_vec(&body).unwrap(), authority::MEMORY_RECONCILE_NAMESPACE);
        self.ok(&["memory", "demo", "reconcile", doc.to_str().unwrap(), sig.to_str().unwrap(), "--expected-head", &head.to_string()]);
    }
    /// The delivery of `key`@`revision` routed to `task`.
    fn delivery(&self, task: &str, key: &str, revision: u64) -> String {
        let record = self.id(key);
        self.ok(&["memory", "demo", "deliveries"]).as_array().unwrap().iter()
            .find(|d| d["subscriber"] == format!("task:{task}") && d["record_id"] == record.as_str() && d["revision"] == revision)
            .unwrap_or_else(|| panic!("no delivery of {key}@{revision} to {task}"))["id"].as_str().unwrap().into()
    }
    /// The worker pulls one delivery and acknowledges it with `state`.
    fn ack(&self, task: &str, delivery: &str, state: &str) {
        let attempt = format!("{task}-attempt");
        let update = self.ok(&["memory", "demo", "update", "--delivery", delivery, "--attempt", &attempt]);
        let ack = self.path("ack.json");
        fs::write(&ack, serde_json::to_vec(&json!({"schema_version":1,"delivery_id":delivery,"attempt_id":attempt,"manifest_hash":update["update"]["manifest_hash"],"state":state})).unwrap()).unwrap();
        self.ok(&["memory", "demo", "ack", "--input", ack.to_str().unwrap()]);
    }
    /// Pull the pending package with `memory package` and acknowledge it through the store.
    fn package_ack(&self, selector: &str, id: &str, disposition: &str) {
        let package = self.ok(&["memory", "demo", "package", selector, id])["package"].clone();
        assert!(!package["change_ids"].as_array().unwrap().is_empty(), "{package}");
        let ack = UpdatePackageAck { schema_version: 1, package_id: package["package_id"].as_str().unwrap().into(),
            manifest_hash: package["manifest_hash"].as_str().unwrap().into(),
            change_ids: serde_json::from_value(package["change_ids"].clone()).unwrap(), disposition: disposition.into() };
        self.db().acknowledge_update_package(&ack, jiff::Timestamp::now().as_millisecond()).unwrap();
    }
    fn blockers(&self, task: &str) -> Vec<(String, String)> {
        self.ok(&["memory", "demo", "readiness", "--task", task])["blockers"].as_array().unwrap().iter()
            .map(|b| (b["kind"].as_str().unwrap().into(), b["id"].as_str().unwrap().into())).collect()
    }
    fn kinds(&self, task: &str) -> Vec<String> { self.blockers(task).into_iter().map(|(kind, _)| kind).collect() }
    /// A refused completion leaves the store unchanged; returns the error.
    fn refused(&self, task: &str, clear_attempt: bool) -> String {
        let before = runtime::snapshot(&self.project).unwrap();
        let error = succeed(&mut self.db(), task, clear_attempt).expect_err("completion accepted over stale memory");
        assert_eq!(runtime::snapshot(&self.project).unwrap(), before);
        error.to_string()
    }
}

fn succeed(db: &mut SqliteStore, task: &str, clear_attempt: bool) -> Result<u64, StoreError> {
    let state = db.read_snapshot(None)?;
    let mut next = state.tasks.iter().find(|t| t.id.as_str() == task).unwrap().clone();
    let previous = next.revision;
    next.revision += 1;
    next.state = TaskState::Succeeded;
    if clear_attempt { next.active_attempt = None; }
    db.commit(Commit { expected_head: state.head, mutations: vec![Mutation::Task { expected: Some(previous), next }] })
}

/// A promotion's invalidation fences its producer's completion, with or without
/// clearing the attempt, until the owner reconciles it; informational ones and
/// unrelated tasks are never fenced.
#[test]
fn promoted_invalidations_block_only_their_task_until_reconciled() {
    let p = Project::new();
    p.add_task("idle");
    let scribe = p.worker("scribe", &["ui"]);
    p.promote("scribe", &scribe, "note", &["ui"], json!({}));
    let advice = p.ok(&["memory", "demo", "invalidations", "--task", "scribe"]);
    assert_eq!((advice[0]["severity"].as_str(), advice[0]["resolved_seq"].is_null()), (Some("informational"), true), "{advice}");
    assert_eq!(p.blockers("scribe"), vec![]);
    succeed(&mut p.db(), "scribe", false).unwrap();

    let writer = p.worker("writer", &["infra"]);
    p.promote("writer", &writer, "rule", &["ui"], json!({"impact":"stop_at_checkpoint"}));
    assert_eq!(p.kinds("writer"), ["unresolved_invalidation"]);
    assert!(p.refused("writer", false).contains("memory completion blocked: unresolved_invalidation"));
    p.refused("writer", true);
    succeed(&mut p.db(), "idle", false).unwrap();
    p.reconcile("writer");
    assert_eq!(p.blockers("writer"), vec![]);
    succeed(&mut p.db(), "writer", true).unwrap();
}

/// An attempt without its task's snapshot has no knowledge to stand on.
#[test]
fn an_attempt_without_its_own_snapshot_cannot_complete() {
    let p = Project::new();
    p.add_task("bare");
    p.attempt("bare", None);
    assert_eq!(p.blockers("bare"), vec![("attempt_snapshot_unavailable".into(), "bare-attempt".into())]);
    p.refused("bare", true);
    let other = p.worker("other", &[]);
    p.add_task("borrower");
    p.attempt("borrower", Some(other));
    assert_eq!(p.kinds("borrower"), ["attempt_snapshot_unavailable"]);
    p.refused("borrower", false);
    assert_eq!(p.blockers("other"), vec![]);
}

/// A required update is covered only by the consuming attempt's own applied
/// receipt, per change: exactly via `memory ack`, or by an applied package on
/// its current binding. Seen is not applied, and neither resolves invalidations.
#[test]
fn required_updates_need_the_attempts_own_applied_receipt() {
    let p = Project::new();
    let scribe = p.worker("scribe", &["ui"]);
    p.promote("scribe", &scribe, "fact", &["ui"], json!({}));
    p.worker("exact", &["ui"]);
    let snapshot = p.worker("packaged", &["ui"]);
    p.revise("scribe", &scribe, "fact", 1, "reconcile_before_completion");
    let first = p.delivery("exact", "fact", 2);
    assert!(p.blockers("exact").contains(&("required_update_unapplied".into(), first.clone())));
    p.refused("exact", false);

    p.ack("exact", &first, "seen");
    assert!(p.kinds("exact").contains(&"required_update_unapplied".into()));
    p.ack("exact", &first, "applied");
    assert_eq!(p.kinds("exact"), ["unresolved_invalidation"], "an applied receipt is not reconciliation");
    // The other consumer's receipt does not speak for this one.
    assert!(p.kinds("packaged").contains(&"required_update_unapplied".into()));

    p.package_ack("--snapshot", &snapshot, "seen");
    assert!(p.kinds("packaged").contains(&"required_update_unapplied".into()));
    p.package_ack("--snapshot", &snapshot, "applied");
    assert_eq!(p.kinds("packaged"), ["unresolved_invalidation"]);
    assert_eq!(p.ok(&["memory", "demo", "receipts", "--attempt", "packaged-attempt"]), json!([]));

    // A later change to the same record has its own receipt requirement.
    p.revise("scribe", &scribe, "fact", 2, "stop_at_checkpoint");
    let later = p.delivery("exact", "fact", 3);
    let unapplied: Vec<_> = p.blockers("exact").into_iter().filter(|(kind, _)| kind == "required_update_unapplied").map(|(_, id)| id).collect();
    assert_eq!(unapplied, [later]);
    p.refused("exact", false);
    p.reconcile("exact");
    assert_eq!(p.blockers("exact"), vec![]);
    succeed(&mut p.db(), "exact", false).unwrap();
}

/// A newer snapshot that already holds the change covers it without a receipt;
/// a snapshot without it needs an applied package on the successor binding
/// that inherited the obligation.
#[test]
fn a_rebound_attempt_is_covered_by_its_new_snapshot_or_successor_package() {
    let p = Project::new();
    let scribe = p.worker("scribe", &["ui"]);
    p.promote("scribe", &scribe, "fact", &["ui"], json!({}));
    p.worker("fresh", &["ui"]);
    let moved_old = p.worker("moved", &["ui"]);
    p.revise("scribe", &scribe, "fact", 1, "reconcile_before_completion");
    for task in ["fresh", "moved"] {
        p.delivery(task, "fact", 2);
        assert_eq!(p.kinds(task), ["unresolved_invalidation", "required_update_unapplied"]);
    }

    let rebind = |task: &str, snapshot: String| {
        let mut db = p.db();
        let state = db.read_snapshot(None).unwrap();
        let mut attempt = state.attempts.iter().find(|a| a.id.as_str() == format!("{task}-attempt")).unwrap().clone();
        let previous = attempt.revision;
        (attempt.revision, attempt.snapshot) = (previous + 1, Some(snapshot));
        db.commit(Commit { expected_head: state.head, mutations: vec![Mutation::Attempt { expected: Some(previous), next: attempt }] }).unwrap();
    };
    rebind("fresh", p.snapshot("fresh", &["ui"]));
    assert_eq!(p.kinds("fresh"), ["unresolved_invalidation"]);
    assert_eq!(p.ok(&["memory", "demo", "receipts", "--attempt", "fresh-attempt"]), json!([]));
    p.reconcile("fresh");
    succeed(&mut p.db(), "fresh", false).unwrap();

    // The new snapshot is scoped away from the record, so only a receipt can cover it.
    let moved_new = p.snapshot("moved", &["infra"]);
    rebind("moved", moved_new.clone());
    assert!(p.kinds("moved").contains(&"required_update_unapplied".into()));
    let mut db = p.db();
    let old = db.consumer_binding_for_snapshot(&moved_old).unwrap().unwrap().binding_id;
    let successor = db.consumer_binding_for_snapshot(&moved_new).unwrap().unwrap().binding_id;
    db.retire_consumer_binding(&old, Some(&successor)).unwrap();
    p.package_ack("--binding", &successor, "seen");
    assert!(p.kinds("moved").contains(&"required_update_unapplied".into()));
    p.package_ack("--binding", &successor, "applied");
    assert_eq!(p.kinds("moved"), ["unresolved_invalidation"]);
    p.reconcile("moved");
    succeed(&mut p.db(), "moved", false).unwrap();
}

/// Consumed knowledge includes the sources it was derived from. Changing a
/// source fences consumers that still rely on it, but not one whose applied
/// newer revision no longer does; a new hard rule is mandatory for every task.
#[test]
fn changed_sources_and_unrouted_hard_rules_block_completion() {
    let p = Project::new();
    p.add_task("idle");
    let scribe = p.worker("scribe", &["ui"]);
    p.promote("scribe", &scribe, "source", &["infra"], json!({}));
    let author = p.worker("author", &["infra"]);
    p.promote("author", &author, "derived", &["ui"], json!({"based_on":[{"record_id":p.id("source"),"revision":1}]}));
    for task in ["reader", "updated"] { p.worker(task, &["ui"]); }
    assert_eq!(p.blockers("reader"), vec![]);
    p.revise("author", &author, "derived", 1, "informational");
    let update = p.delivery("updated", "derived", 2);
    for state in ["seen", "applied"] { p.ack("updated", &update, state); }

    p.promote("scribe", &scribe, "source", &["infra"], json!({"expected":{"record_id":p.id("source"),"revision":1}}));
    let source = p.id("source");
    assert_eq!(p.blockers("reader"), vec![("invalid_consumed_revision".into(), format!("{source}@1"))]);
    p.refused("reader", false);
    assert_eq!(p.blockers("updated"), vec![]);

    p.policy("hard_rule", "source");
    for task in ["reader", "idle"] {
        assert!(p.blockers(task).contains(&("mandatory_revision_missing".into(), format!("{source}@2"))), "{task}");
    }
    p.refused("idle", false);
}

/// Stores older than the receipt protocol stay inspectable, but a memory-era
/// store must be upgraded before any task completes.
#[test]
fn older_memory_schemas_require_upgrade_before_completion() {
    for version in [17, 18, 21, 22, 23] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let mut db = SqliteStore::create(&path).unwrap();
        db.commit(Commit { expected_head: 0, mutations: vec![Mutation::Task { expected: None,
            next: Task { id: TaskId::new("b").unwrap(), revision: 1, state: TaskState::Draft, title: "b".into(), active_attempt: None } }] }).unwrap();
        drop(db);
        test_schema::historical(&rusqlite::Connection::open(&path).unwrap(), version).unwrap();
        let mut db = SqliteStore::open(&path).unwrap();
        let now = jiff::Timestamp::now().as_millisecond();
        if version < 18 {
            assert_eq!(db.memory_readiness("b", now).unwrap().blockers, vec![]);
            continue;
        }
        assert_eq!(db.memory_readiness("b", now).unwrap().blockers, vec![MemoryBlocker { kind: "memory_schema_upgrade_required".into(), id: "b".into() }]);
        let before = db.read_snapshot(None).unwrap();
        let error = succeed(&mut db, "b", false).unwrap_err().to_string();
        assert!(error.contains("memory_schema_upgrade_required"), "v{version}: {error}");
        assert_eq!(db.read_snapshot(None).unwrap(), before);
        db.upgrade_v1().unwrap();
        assert_eq!(db.memory_readiness("b", now).unwrap().blockers, vec![]);
        succeed(&mut db, "b", false).unwrap();
    }
}

#[path = "../src/store/test_schema.rs"]
mod test_schema;

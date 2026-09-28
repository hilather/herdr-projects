#![cfg(all(feature = "state-store", target_os = "linux"))]
//! Barrier freeze/release/revoke through the compiled CLI over a disposable
//! project store. Members are real verified results: signed contracts, CLI
//! submissions and sandboxed verification. Attempts are recorded through the
//! public store API because no worker is launched.
use herdr_projects::{authority, domain::*, memory::*, migration, runtime};
use serde_json::{json, Value};
use std::{fs, path::PathBuf, process::{Command, Output}};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-projects");
const POLICY: &str = r#"{"version":1,"checks":["/usr/bin/git","diff","--quiet"]}"#;

struct Factory { home: tempfile::TempDir, project: PathBuf, key: PathBuf, repo: PathBuf, store: String, oid: String }

impl Factory {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let key = home.path().join("owner");
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&key).status().unwrap().success());
        let public = fs::read_to_string(key.with_extension("pub")).unwrap().split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        let config = home.path().join("owner.toml");
        fs::write(&config, format!("[authority]\nversion=1\nrevision=1\napproval_public_key={public:?}\n")).unwrap();
        let repo = home.path().join("repo");
        fs::create_dir(&repo).unwrap();
        let mut f = Factory { project: home.path().join("root/demo"), key, repo, store: String::new(), oid: String::new(), home };
        for command in ["new", "pause"] { f.ok(&[command, "demo"]); }
        migration::apply(&f.project, &migration::inspect_with_config(&f.project, &config).unwrap(), true).unwrap();
        let s = runtime::snapshot(&f.project).unwrap();
        runtime::set_state(&f.project, s.head, s.control.unwrap().revision, ProjectState::Active, &config).unwrap();
        f.store = f.project.join(".state/state.db").canonicalize().unwrap().display().to_string();
        f.git(&["init", "-q", "--object-format=sha1"]);
        f.git(&["commit", "-q", "--allow-empty", "-m", "base"]);
        f.oid = f.git(&["rev-parse", "HEAD"]);
        f
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
    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("/usr/bin/git").env_clear().env("PATH", "/usr/bin:/bin").env("HOME", self.home.path())
            .env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "fixture").env("GIT_AUTHOR_EMAIL", "fixture@example.com")
            .env("GIT_COMMITTER_NAME", "fixture").env("GIT_COMMITTER_EMAIL", "fixture@example.com")
            .current_dir(&self.repo).args(args).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }
    fn sign(&self, name: &str, bytes: &[u8], namespace: &str) -> (PathBuf, PathBuf) {
        let path = self.path(name);
        fs::write(&path, bytes).unwrap();
        let signature = PathBuf::from(format!("{}.sig", path.display()));
        let _ = fs::remove_file(&signature);
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-Y", "sign", "-f"]).arg(&self.key).args(["-n", namespace]).arg(&path).output().unwrap().status.success());
        (path, signature)
    }
    fn db(&self) -> herdr_projects::store::SqliteStore { migration::open_active(&self.project).unwrap() }
    fn memory(&self) -> MemoryStore { MemoryStore::from_sqlite(self.db(), self.project.join(".state/objects")) }
    /// Install (or revise) a signed verify-only contract; returns its digest.
    fn contract(&self, task: &str, revision: u64, extra: Value) -> String {
        let mut body = json!({"version":1,"project_store":self.store,"expected_head":self.head(),"task_id":task,"contract_revision":revision,
            "deliverable":"barrier fixture","non_goals":"no worker launch","acceptance_policies":[{"id":"builds","text":POLICY}],
            "repository":self.repo,"base_oid":self.oid,"object_format":"sha1","dependencies":[],"scope":{"paths":[],"named_resources":[]},
            "capability_flags":[],"profile_kind":"codex","retry_class":"none","result_schema_id":"result-v1","route":"verify_only",
            "authority":authority::policy_reference(&self.project).unwrap()});
        if let Value::Object(extra) = extra { for (k, v) in extra { body[k] = v; } }
        let (doc, sig) = self.sign(&format!("{task}-contract-{revision}.json"), &serde_json::to_vec(&body).unwrap(), authority::CONTRACT_SIGNATURE_NAMESPACE);
        self.ok(&["task", "demo", "contract", "put", "--input-file", doc.to_str().unwrap(), "--signature", sig.to_str().unwrap()])["digest"].as_str().unwrap().into()
    }
    /// A task whose running attempt consumed a worker snapshot scoped to `domains`.
    fn task(&self, task: &str, domains: &[&str]) -> String {
        self.ok(&["task", "demo", "add", task, "--title", task, "--expected-head", &self.head().to_string()]);
        let snapshot = self.memory().create_task_snapshot(SnapshotRequest { schema_version: 1, task_id: task.into(), profile: "worker".into(),
            domains: domains.iter().map(|d| d.to_string()).collect(), paths: vec![], pinned_keys: vec![], sensitivity: "default".into() },
            "worker", &"a".repeat(64), None, 32_000, "Barrier fixture instructions", 1, None).unwrap();
        let mut db = self.db();
        let state = db.read_snapshot(None).unwrap();
        let mut next = state.tasks.iter().find(|t| t.id.as_str() == task).unwrap().clone();
        let previous = next.revision;
        next.revision += 1;
        next.active_attempt = Some(AttemptId::new(format!("{task}-attempt")).unwrap());
        db.commit(Commit { expected_head: state.head, mutations: vec![
            Mutation::Attempt { expected: None, next: Attempt { id: AttemptId::new(format!("{task}-attempt")).unwrap(), task: next.id.clone(), revision: 1,
                state: AttemptState::Running, snapshot: Some(snapshot.id.as_str().into()), reservation: format!("{task}-slot"), termination_observed: false } },
            Mutation::Task { expected: Some(previous), next },
        ] }).unwrap();
        snapshot.id.as_str().into()
    }
    /// Submit a result for the task's attempt and run its signed policy.
    fn verify(&self, task: &str, revision: u64, digest: &str, key: &str) -> Output {
        let receipt = self.submit(task, &format!("{task}-attempt"), revision, digest, key);
        assert!(receipt.status.success(), "submit: {}", String::from_utf8_lossy(&receipt.stderr));
        let receipt: Value = serde_json::from_slice(&receipt.stdout).unwrap();
        let policy = self.path("policy.json");
        fs::write(&policy, POLICY).unwrap();
        let work = self.path(&format!("{key}-work"));
        self.cli(&["result", "demo", "verify", receipt["submission_id"].as_str().unwrap(), "--policy-id", "builds",
            "--policy-file", policy.to_str().unwrap(), "--idempotency-key", key, "--work-dir", work.to_str().unwrap()])
    }
    fn submit(&self, task: &str, attempt: &str, revision: u64, digest: &str, key: &str) -> Output {
        let objects: Vec<_> = self.git(&["rev-list", "--objects", "--all"]).lines().map(|line| {
            let oid = line.split_whitespace().next().unwrap();
            json!({"oid":oid,"relative_path":format!("{}/{}", &oid[..2], &oid[2..])})
        }).collect();
        let submission = json!({"idempotency_key":key,"task_id":task,"contract_revision":revision,"contract_digest":digest,
            "attempt_id":attempt,"repository":self.repo,"base_oid":self.oid,"candidate_oid":self.oid,
            "object_format":"sha1","artifact_manifest":[],"claimed_checks":[],"objects":objects});
        let path = self.path(&format!("{key}.json"));
        fs::write(&path, serde_json::to_vec(&submission).unwrap()).unwrap();
        self.cli(&["result", "demo", "submit", "--input-file", path.to_str().unwrap()])
    }
    /// An accepted verification of a fresh submission, as a barrier member.
    fn verified(&self, task: &str, revision: u64, digest: &str, key: &str) -> Value {
        let out = self.verify(task, revision, digest, key);
        assert!(out.status.success(), "verify: {}", String::from_utf8_lossy(&out.stderr));
        let run: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(run["state"], "accepted", "{run}");
        json!({"task_id":task,"contract_revision":revision,"attempt_id":format!("{task}-attempt"),"result_id":run["receipt"]["result_id"],
            "verification_id":run["run_id"],"integration_id":null,"proposal_dispositions":[]})
    }
    fn member(&self, task: &str, domains: &[&str]) -> Value {
        self.task(task, domains);
        let digest = self.contract(task, 1, Value::Null);
        self.verified(task, 1, &digest, &format!("{task}-1"))
    }
    fn freeze_at(&self, members: &[&Value], head: u64) -> Output {
        let path = self.path("members.json");
        fs::write(&path, serde_json::to_vec(members).unwrap()).unwrap();
        self.cli(&["memory", "demo", "barrier-freeze", "--input", path.to_str().unwrap(), "--expected-head", &head.to_string()])
    }
    fn freeze(&self, members: &[&Value]) -> Value {
        let out = self.freeze_at(members, self.head());
        assert!(out.status.success(), "freeze: {}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap()
    }
    /// A refused freeze leaves the head unchanged; returns stderr.
    fn freeze_refused(&self, members: &[&Value]) -> String {
        let head = self.head();
        let out = self.freeze_at(members, head);
        assert!(!out.status.success(), "freeze accepted: {}", String::from_utf8_lossy(&out.stdout));
        assert_eq!(self.head(), head);
        String::from_utf8_lossy(&out.stderr).into_owned()
    }
    fn inspect(&self, barrier: &Value) -> Value { self.ok(&["memory", "demo", "barrier", "--id", barrier["barrier_id"].as_str().unwrap()]) }
    fn released(&self, barrier: &Value) -> Value {
        let out = self.release(barrier);
        assert!(out.status.success(), "release: {}", String::from_utf8_lossy(&out.stderr));
        let released: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert!(released["released_seq"].is_u64() && released["revoked_seq"].is_null(), "{released}");
        released
    }
    /// A refused release leaves the head and the stored barrier unchanged; returns stderr.
    fn release_refused(&self, barrier: &Value) -> String {
        let (head, stored) = (self.head(), self.inspect(barrier));
        let out = self.release(barrier);
        assert!(!out.status.success(), "release accepted: {}", String::from_utf8_lossy(&out.stdout));
        assert!(stored["released_seq"].is_null());
        assert_eq!((self.head(), self.inspect(barrier)), (head, stored));
        String::from_utf8_lossy(&out.stderr).into_owned()
    }
    /// Draft, owner-sign and submit a release through the CLI.
    fn release(&self, barrier: &Value) -> Output {
        let expires = (jiff::Timestamp::now().as_millisecond() + 60_000).to_string();
        let draft = self.cli(&["memory", "demo", "barrier-release-draft", "--id", barrier["barrier_id"].as_str().unwrap(), "--expires-unix-ms", &expires]);
        if !draft.status.success() { return draft; }
        let draft: Value = serde_json::from_slice(&draft.stdout).unwrap();
        let (doc, sig) = self.sign("release.json", &serde_json::to_vec(&draft).unwrap(), authority::BARRIER_RELEASE_SIGNATURE_NAMESPACE);
        self.cli(&["memory", "demo", "barrier-release", doc.to_str().unwrap(), sig.to_str().unwrap(), "--expected-head", &draft["expected_head"].to_string()])
    }
    fn revoke(&self, barrier: &Value) -> Value { self.revoke_at(barrier, self.head()) }
    fn revoke_at(&self, barrier: &Value, head: u64) -> Value {
        self.ok(&["memory", "demo", "barrier-revoke", "--id", barrier["barrier_id"].as_str().unwrap(), "--expected-head", &head.to_string(), "--reason", "operator withdrawal"])
    }
    /// The worker pulls and applies one pending delivery addressed to its attempt.
    fn apply(&self, task: &str) {
        let delivery = self.ok(&["memory", "demo", "deliveries"]).as_array().unwrap().iter()
            .find(|d| d["subscriber"] == format!("task:{task}") && d["state"] == "pending").unwrap()["id"].as_str().unwrap().to_owned();
        let attempt = format!("{task}-attempt");
        let update = self.ok(&["memory", "demo", "update", "--delivery", &delivery, "--attempt", &attempt]);
        for state in ["seen", "applied"] {
            let ack = self.path("ack.json");
            fs::write(&ack, serde_json::to_vec(&json!({"schema_version":1,"delivery_id":delivery,"attempt_id":attempt,"manifest_hash":update["update"]["manifest_hash"],"state":state})).unwrap()).unwrap();
            self.ok(&["memory", "demo", "ack", "--input", ack.to_str().unwrap()]);
        }
    }
}

impl Factory {
    /// Propose, owner-review and promote one memory change from `producer`'s attempt.
    fn promote(&self, producer: &str, snapshot: &str, key: &str, kind: &str, domains: &[&str], impact: &str) { self.promote_on(producer, snapshot, key, kind, domains, impact, json!({})) }
    fn record(&self, key: &str) -> Value {
        let id = self.ok(&["memory", "demo", "inspect"])["records"].as_array().unwrap().iter().find(|r| r["record_key"] == key).unwrap()["id"].clone();
        json!({"record_id":id,"revision":1})
    }
    #[allow(clippy::too_many_arguments)]
    /// `extra` overrides fields of the proposed change (`expected`, `based_on`).
    fn promote_on(&self, producer: &str, snapshot: &str, key: &str, kind: &str, domains: &[&str], impact: &str, extra: Value) {
        let body = self.memory().ingest_object(format!("{key} {}", self.head()).as_bytes()).unwrap();
        let id = format!("proposal-{key}-{}", self.head());
        let proposal = json!({"schema_version":1,"proposal_id":id,"producer":{"task_id":producer,"attempt_id":format!("{producer}-attempt")},
            "input_snapshot_id":snapshot,"changes":[{"record_key":key,"kind":kind,"scope":{"domains":domains,"paths":[]},
            "claim":key,"body_object":body.as_str(),"evidence":[],"based_on":[],"impact":impact}]});
        let mut proposal = proposal;
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
    /// Install an owner-signed memory policy operation on one record.
    fn policy(&self, op: &str, key: &str) {
        let head = self.head();
        let body = json!({"version":1,"project_store":self.store,"revision":1,"authority":authority::policy_reference(&self.project).unwrap(),
            "expected_head":head,"op":op,"record_key":key});
        let (doc, sig) = self.sign(&format!("policy-{op}-{key}.json"), &serde_json::to_vec(&body).unwrap(), authority::MEMORY_SIGNATURE_NAMESPACE);
        self.ok(&["memory", "demo", "import", doc.to_str().unwrap(), sig.to_str().unwrap(), "--expected-head", &head.to_string()]);
    }
}

fn has(stderr: &str, text: &str) { assert!(stderr.contains(text), "expected {text:?} in {stderr}"); }

/// Frozen membership and memory evidence are rechecked at release: malformed
/// input, a task edit, memory the worker applied after freeze, new contract-scope
/// memory and a newer signed contract all refuse; unrelated memory does not.
#[test]
fn release_refuses_evidence_that_moved_after_freeze_but_ignores_unrelated_memory() {
    let f = Factory::new();
    let scribe = f.task("scribe", &[]);
    f.promote("scribe", &scribe, "ui.note", "observation", &["ui"], "informational");
    let alpha = f.member("alpha", &["ui"]);
    let beta = f.member("beta", &[]);
    // Values the store cannot represent are refused before any publication.
    let mut bad = alpha.clone();
    bad["proposal_dispositions"] = json!([{"proposal_id":"any-proposal","disposition":"maybe"}]);
    has(&f.freeze_refused(&[&bad]), "proposal disposition is invalid");
    let mut bad = alpha.clone();
    bad["integration_id"] = "short".into();
    has(&f.freeze_refused(&[&bad]), "integration id is invalid");
    // Replay after later events returns the stored barrier, not a later one.
    let head = f.head();
    let first = f.freeze(&[&alpha]);
    assert!(first["released_seq"].is_null() && first["revoked_seq"].is_null());
    f.promote("scribe", &scribe, "backend.note", "observation", &["backend"], "informational");
    let replay = f.freeze_at(&[&alpha], head);
    assert!(replay.status.success(), "{}", String::from_utf8_lossy(&replay.stderr));
    assert_eq!(serde_json::from_slice::<Value>(&replay.stdout).unwrap(), first);
    let both = f.freeze(&[&alpha, &beta]);
    assert_ne!(both["barrier_id"], first["barrier_id"]);
    assert_eq!(f.freeze(&[&alpha]), first);
    assert_eq!(f.inspect(&first), first);
    // A task edit between freeze and release fences the frozen membership. The
    // CLI refuses edits under an active attempt, so the edit uses the store API.
    let mut db = f.db();
    let state = db.read_snapshot(None).unwrap();
    let mut task = state.tasks.iter().find(|t| t.id.as_str() == "alpha").unwrap().clone();
    let previous = task.revision;
    (task.revision, task.title) = (previous + 1, "renamed".into());
    db.commit(Commit { expected_head: state.head, mutations: vec![Mutation::Task { expected: Some(previous), next: task }] }).unwrap();
    has(&f.release_refused(&first), "memory manifest changed");
    // The worker applies optional memory, published earlier, after freeze.
    f.promote("scribe", &scribe, "ui.later", "observation", &["ui"], "informational");
    let frozen = f.freeze(&[&alpha]);
    assert_ne!(frozen["barrier_id"], first["barrier_id"]);
    f.apply("alpha");
    has(&f.release_refused(&frozen), "memory manifest changed");
    // New contract-scope memory after freeze.
    let frozen = f.freeze(&[&alpha]);
    f.promote("scribe", &scribe, "api.contract", "contract", &["infra"], "informational");
    f.release_refused(&frozen);
    assert!(f.inspect(&frozen)["revoked_seq"].is_u64(), "contract-scope memory must revoke the pending barrier");
    // A newer signed contract revision: neither release nor re-freeze.
    let frozen = f.freeze(&[&alpha]);
    let digest = f.contract("alpha", 2, Value::Null);
    has(&f.release_refused(&frozen), "contract is no longer current");
    has(&f.freeze_refused(&[&alpha]), "contract is no longer current");
    // Evidence for the current contract releases, despite unrelated memory.
    let current = f.verified("alpha", 2, &digest, "alpha-2");
    let frozen = f.freeze(&[&current]);
    f.promote("scribe", &scribe, "backend.other", "observation", &["backend"], "informational");
    let released = f.released(&frozen);
    assert_eq!(f.inspect(&frozen), released);
}

/// Owner-promoted hard memory fences release until the member applies it.
#[test]
fn hard_memory_blocks_release_until_the_member_has_applied_it() {
    let f = Factory::new();
    let scribe = f.task("scribe", &[]);
    f.promote("scribe", &scribe, "ops.rule", "observation", &["ops"], "informational");
    let alpha = f.member("alpha", &["ui"]);
    // Moving the required set after freeze revokes the pending barrier.
    let frozen = f.freeze(&[&alpha]);
    f.policy("hard_rule", "ops.rule");
    f.release_refused(&frozen);
    assert!(f.inspect(&frozen)["revoked_seq"].is_u64());
    // A fresh freeze pins the new required set, but the member has not applied it.
    let fresh = f.freeze(&[&alpha]);
    assert!(fresh["required_set_generation"].as_u64() > frozen["required_set_generation"].as_u64());
    has(&f.release_refused(&fresh), "memory completion blocked");
    let blockers = f.ok(&["memory", "demo", "readiness", "--task", "alpha"])["blockers"].clone();
    assert!(blockers.as_array().unwrap().iter().any(|b| b["kind"] == "mandatory_revision_missing"), "{blockers}");
    f.apply("alpha");
    f.reconcile("alpha");
    assert_eq!(f.ok(&["memory", "demo", "readiness", "--task", "alpha"])["blockers"], json!([]));
    let applied = f.freeze(&[&alpha]);
    f.released(&applied);
}

fn blockers(f: &Factory, task: &str) -> Vec<String> {
    let report = f.ok(&["scheduler", "demo", "inspect"]);
    let entry = report["entries"].as_array().unwrap().iter().find(|e| e["task"] == task).unwrap();
    serde_json::from_value(entry["blockers"].clone()).unwrap()
}

/// Operator revocation withdraws dependency evidence without touching the live
/// member's capacity; only a later release over the member restores it.
#[test]
fn revocation_blocks_dependents_and_retains_capacity_until_a_later_release() {
    let f = Factory::new();
    f.task("alpha", &[]);
    f.ok(&["task", "demo", "add", "down", "--title", "down", "--expected-head", &f.head().to_string()]);
    let request = f.path("queue.json");
    fs::write(&request, r#"{"priority":0,"dependencies":[{"predecessor":"alpha","requirement":"verified_result"}]}"#).unwrap();
    f.ok(&["task", "demo", "queue", "down", "--input-file", request.to_str().unwrap(), "--expected-revision", "1", "--expected-head", &f.head().to_string()]);
    let digest = f.contract("alpha", 1, Value::Null);
    let alpha = f.verified("alpha", 1, &digest, "alpha-1");
    let beta = f.member("beta", &[]);
    let (available, blocked) = ("admission_disabled:verified_result", "verified_dependency_evidence_unavailable:alpha:verified_result");
    assert!(blockers(&f, "down").iter().any(|b| b == available));
    let frozen = f.freeze(&[&alpha]);
    let before = runtime::snapshot(&f.project).unwrap();
    let head = f.head();
    let revoked = f.revoke(&frozen);
    assert!(revoked["revoked_seq"].is_u64() && revoked["released_seq"].is_null());
    // Replay under the stale head is idempotent and appends nothing.
    let after = f.head();
    assert_eq!(f.revoke_at(&frozen, head), revoked);
    assert_eq!(f.head(), after);
    f.release_refused(&frozen);
    let state = runtime::snapshot(&f.project).unwrap();
    assert_eq!((state.attempts, state.tasks), (before.attempts.clone(), before.tasks.clone()));
    assert!(before.attempts.iter().all(|a| a.retains_capacity() && !a.termination_observed));
    assert!(blockers(&f, "down").iter().any(|b| b == blocked));
    // Later evidence from the same attempt cannot reattach while revoked...
    has(&String::from_utf8_lossy(&f.verify("alpha", 1, &digest, "alpha-later").stderr), "dependency blocked");
    assert!(blockers(&f, "down").iter().any(|b| b == blocked));
    // ...until a later barrier over the member is released, and only while it stands.
    let wave = f.freeze(&[&alpha, &beta]);
    f.released(&wave);
    f.verified("alpha", 1, &digest, "alpha-after-release");
    assert!(blockers(&f, "down").iter().any(|b| b == available), "{:?}", blockers(&f, "down"));
    f.revoke(&wave);
    assert!(blockers(&f, "down").iter().any(|b| b == blocked));
    has(&String::from_utf8_lossy(&f.verify("alpha", 1, &digest, "alpha-after-revoke").stderr), "dependency blocked");
}

/// Memory changes revoke frozen and released barriers whose evidence they touch,
/// through scoped invalidations and transitive sources, even after the worker's
/// binding retired; unrelated and advisory memory does not.
#[test]
fn memory_changes_revoke_barriers_over_the_evidence_they_invalidate() {
    let f = Factory::new();
    let scribe = f.task("scribe", &[]);
    f.promote("scribe", &scribe, "lab.source", "observation", &["lab"], "informational");
    let scholar = f.task("scholar", &["lab", "ui"]);
    f.promote_on("scholar", &scholar, "ui.derived", "observation", &["ui"], "informational", json!({"based_on":[f.record("lab.source")]}));
    let alpha = f.member("alpha", &["ui"]);
    let beta = f.member("beta", &["api"]);
    let released = f.released(&f.freeze(&[&alpha]));
    let pending = f.freeze(&[&beta]);
    let attempts = runtime::snapshot(&f.project).unwrap().attempts;
    let standing = |barrier: &Value| f.inspect(barrier)["revoked_seq"].is_null();
    for (key, domains, impact) in [("docs.note", ["docs"], "informational"), ("ui.advice", ["ui"], "informational")] {
        f.promote("scribe", &scribe, key, "observation", &domains, impact);
        assert!(standing(&released) && standing(&pending), "{key}");
    }
    f.promote("scribe", &scribe, "ui.breaking", "observation", &["ui"], "stop_at_checkpoint");
    let revoked = f.inspect(&released);
    assert!(revoked["revoked_seq"].is_u64());
    assert_eq!(revoked["released_seq"], released["released_seq"]);
    assert!(standing(&pending));
    assert_eq!(runtime::snapshot(&f.project).unwrap().attempts, attempts);
    // The worker applies both pending updates, the owner reconciles, the member
    // is re-released and then the worker's binding retires.
    f.apply("alpha");
    f.apply("alpha");
    f.reconcile("alpha");
    let again = f.released(&f.freeze(&[&alpha]));
    let binding: String = rusqlite::Connection::open(&f.store).unwrap().query_row(
        "SELECT binding_id FROM consumer_bindings WHERE attempt_id='alpha-attempt' AND active=1", [], |row| row.get(0)).unwrap();
    f.db().retire_consumer_binding(&binding, None).unwrap();
    // A new revision of the unconsumed source of consumed derived memory
    // reaches the retired worker's barrier, and only that barrier.
    f.promote_on("scholar", &scholar, "lab.source", "observation", &["lab"], "informational", json!({"expected":f.record("lab.source")}));
    let revoked = f.inspect(&again);
    assert!(revoked["revoked_seq"].is_u64(), "barrier source invalidation must not require an active worker binding");
    assert_eq!(revoked["released_seq"], again["released_seq"]);
    assert!(standing(&pending));
    // An owner memory policy change revokes pending barriers too.
    f.policy("revoke_head", "docs.note");
    assert!(!standing(&pending));
    assert_eq!(runtime::snapshot(&f.project).unwrap().attempts, attempts);
}


impl Factory {
    /// Queue launchable tasks with observed runtime bindings, before any attempt runs.
    fn prepare(&self, tasks: &[&str]) {
        let config = self.home.path().join("owner.toml");
        let set_state = |state| { let s = runtime::snapshot(&self.project).unwrap(); runtime::set_state(&self.project, s.head, s.control.unwrap().revision, state, &config).unwrap(); };
        set_state(ProjectState::Paused);
        let mut db = self.db();
        let scheduler = db.read_snapshot(None).unwrap().scheduler.unwrap().policy.revision;
        db.set_scheduler_policy(db.current_head().unwrap(), scheduler, 8, 3).unwrap();
        for task in tasks {
            self.ok(&["task", "demo", "add", task, "--title", task, "--expected-head", &self.head().to_string()]);
            let id = TaskId::new(*task).unwrap();
            db.create_runtime(Some(&id), Some(1), db.current_head().unwrap(), &RuntimeRoute::default()).unwrap();
            db.queue_task(&id, 2, db.current_head().unwrap(), &QueueRequest { priority: 0, dependencies: vec![] }, 0).unwrap();
        }
        let snapshot = db.read_snapshot(None).unwrap();
        let observations: Vec<_> = snapshot.runtime_bindings.iter().map(|binding| herdr_projects::reconcile::RuntimeObservation {
            binding: binding.id.clone(), binding_revision: binding.revision, task_revision: Some(3),
            config_digest: migration::config_reference(&config).unwrap().digest,
            observed_unix_ms: jiff::Timestamp::now().as_millisecond(), collector: "herdr-git-v1".into(), ..Default::default()
        }).collect();
        db.record_observations(snapshot.head, &observations).unwrap();
        set_state(ProjectState::Active);
    }
    /// Reserve one launch attempt per prepared task through a signed delegation,
    /// as a delegated subject would. Each task's contract names `barrier` as its
    /// required release. The native profile is a synthetic, uncertified record.
    fn reserve(&self, tasks: &[&str], barrier: &Value) -> Vec<(String, String)> {
        let config = self.home.path().join("owner.toml");
        let evidence = VersionedReference { id: "synthetic-cli-fixture".into(), revision: 1, digest: "a".repeat(64) };
        let supported = CapabilityEvidence::Supported { evidence: evidence.clone() };
        let profile = FrozenProfile {
            version: 1, name: "fixture".into(), kind: "codex".into(), definition_digest: "b".repeat(64),
            config: migration::config_reference(&config).unwrap(), arguments_digest: "c".repeat(64),
            environment_names: vec![], execution_home: None, permission_policy: authority::policy_reference(&self.project).unwrap(), adapter: evidence,
            agent: ExecutableIdentity { path: "/fixture/no-worker".into(), digest: "d".repeat(64), version: "1.0.0".into() },
            herdr: ExecutableIdentity { path: "/fixture/no-herdr".into(), digest: "e".repeat(64), version: "1.0.0".into() },
            capabilities: ProfileCapabilities { launch: supported.clone(), readiness_observation: supported.clone(), prompt_submission: supported.clone(), stop: supported,
                checkpoint_acknowledgment: CapabilityEvidence::Unknown, structured_usage: CapabilityEvidence::Unknown, resume: CapabilityEvidence::Unknown },
            workflow_certificate: None,
        };
        let metadata = fs::metadata(&self.store).unwrap();
        let report = serde_json::to_string(&json!({"preparation":{"profile":profile,"reference":profile.reference().unwrap(),"launchable":true,"protocol_capable":false,"certified":false},
            "source_store":[self.store, std::os::unix::fs::MetadataExt::dev(&metadata), std::os::unix::fs::MetadataExt::ino(&metadata)]})).unwrap();
        rusqlite::Connection::open(&self.store).unwrap().execute("INSERT INTO native_profiles(profile_digest,report,report_digest,sequence) VALUES(?1,?2,?3,(SELECT max(sequence) FROM events))",
            rusqlite::params![profile.reference().unwrap().digest, report, format!("{:x}", sha2::Digest::finalize(<sha2::Sha256 as sha2::Digest>::new_with_prefix(report.as_bytes())))]).unwrap();
        let contracts: Vec<_> = tasks.iter().map(|task| {
            let digest = self.contract(task, 1, json!({"version":2,"required_barrier":barrier["release_reference"]}));
            let mut memory = self.memory();
            memory.create_worker_snapshot(SnapshotRequest { schema_version: 1, task_id: task.to_string(), profile: profile.name.clone(), domains: vec![], paths: vec![], pinned_keys: vec![], sensitivity: "default".into() },
                &profile.name, &profile.definition_digest, profile.config.digest.as_deref(), 32000, "Delegated instructions", jiff::Timestamp::now().as_millisecond(), None).unwrap();
            json!({"id":task,"revision":1,"digest":digest})
        }).collect();
        let authority = authority::policy_reference(&self.project).unwrap();
        let policy = BudgetPolicy { version: 1, project_store: self.store.clone(), revision: 1, authority: authority.clone(),
            limits: BudgetLimits { max_attempts: Some(10), max_provider_tokens: None, unknown_usage: UnknownUsagePolicy::Refuse } };
        let (doc, sig) = self.sign("budget.json", &serde_json::to_vec(&policy).unwrap(), authority::BUDGET_SIGNATURE_NAMESPACE);
        self.ok(&["budget", "demo", "import", doc.to_str().unwrap(), sig.to_str().unwrap(), "--expected-head", &self.head().to_string()]);
        let subject = self.path("subject");
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&subject).status().unwrap().success());
        let subject_public = fs::read_to_string(subject.with_extension("pub")).unwrap().split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        let grant = json!({"version":2,"issuer":"owner","subject":"delegate","subject_public_key":subject_public,
            "action_classes":["reserve_attempt"],"repositories":[{"repository":self.repo,"ref":"refs/heads/factory"}],"profile_kinds":["codex"],
            "max_concurrent_attempts":tasks.len(),"expires_unix_ms":9_000_000_000_000i64,"revocation_epoch":1,"child_delegation":"forbidden",
            "policy_revision":authority.revision,"project_store":self.store,"authority":authority,
            "reservation_scope":{"task_contracts":contracts,"profiles":[profile.reference().unwrap()],"budget":policy.reference().unwrap(),
                "repository_bases":[{"repository":self.repo,"ref":"refs/heads/factory","commit_oid":self.oid,"object_format":"sha1"}],"max_total_attempts":tasks.len()}});
        let (doc, sig) = self.sign("grant.json", &serde_json::to_vec(&grant).unwrap(), authority::DELEGATION_SIGNATURE_NAMESPACE);
        let grant_id = self.ok(&["delegation", "demo", "import", doc.to_str().unwrap(), sig.to_str().unwrap()])["grant_id"].as_str().unwrap().to_owned();
        tasks.iter().map(|task| {
            let draft = self.ok(&["delegation", "demo", "draft", &grant_id, "--idempotency-key", task]);
            let request = self.path(&format!("{task}-request.json"));
            fs::write(&request, serde_json::to_vec(&draft).unwrap()).unwrap();
            let signature = self.path(&format!("{task}-request.json.sig"));
            assert!(Command::new("/usr/bin/ssh-keygen").args(["-Y", "sign", "-f"]).arg(&subject).args(["-n", authority::DELEGATED_RESERVATION_SIGNATURE_NAMESPACE]).arg(&request).output().unwrap().status.success());
            let receipt = self.ok(&["delegation", "demo", "reserve", request.to_str().unwrap(), signature.to_str().unwrap()]);
            assert_eq!(receipt["record"]["inputs"]["task"], *task);
            (receipt["record"]["attempt"].as_str().unwrap().to_owned(), receipt["record"]["inputs"]["task_contract"]["digest"].as_str().unwrap().to_owned())
        }).collect()
    }
}

/// Revoking a released barrier that downstream reserved attempts require blocks
/// their new results, records the live consumer and keeps every retained
/// capacity and existing cancellation exactly as it was.
#[test]
fn revoked_upstream_barrier_blocks_downstream_results_but_keeps_capacity_and_cancellations() {
    let f = Factory::new();
    f.prepare(&["down", "late"]);
    let alpha = f.member("alpha", &[]);
    let released = f.released(&f.freeze(&[&alpha]));
    let reserved = f.reserve(&["down", "late"], &released);
    let [(down, down_digest), (late, late_digest)] = [reserved[0].clone(), reserved[1].clone()];
    let first = f.submit("down", &down, 1, &down_digest, "down-1");
    assert!(first.status.success(), "{}", String::from_utf8_lossy(&first.stderr));
    // The operator cancels the never-launched `late` attempt before revocation.
    let state = runtime::snapshot(&f.project).unwrap();
    let revision = state.attempts.iter().find(|a| a.id.as_str() == late).unwrap().revision.to_string();
    let cancelled = f.ok(&["task", "demo", "cancel-attempt", &late, "--expected-revision", &revision, "--expected-head", &state.head.to_string(), "--reason", "operator stop"]);
    assert_eq!(cancelled["released"], true);
    let before = runtime::snapshot(&f.project).unwrap();
    f.revoke(&released);
    let after = runtime::snapshot(&f.project).unwrap();
    assert_eq!((&after.attempts, &after.cancellations), (&before.attempts, &before.cancellations));
    assert!(after.attempts.iter().find(|a| a.id.as_str() == down).unwrap().retains_capacity());
    let blockers = f.ok(&["memory", "demo", "readiness", "--task", "down"])["blockers"].clone();
    assert!(blockers.as_array().unwrap().iter().any(|b| b["kind"] == "required_barrier_revoked" && b["id"] == released["barrier_id"]), "{blockers}");
    // History replays; new results from the live and the terminated consumer are refused.
    let replay = f.submit("down", &down, 1, &down_digest, "down-1");
    assert_eq!(serde_json::from_slice::<Value>(&replay.stdout).unwrap()["replayed"], true);
    for (task, attempt, digest, reason) in [("down", &down, &down_digest, "required barrier was revoked"), ("late", &late, &late_digest, "required barrier is not currently released")] {
        let head = f.head();
        let refused = f.submit(task, attempt, 1, digest, &format!("{task}-2"));
        assert!(!refused.status.success());
        has(&String::from_utf8_lossy(&refused.stderr), reason);
        assert_eq!(f.head(), head);
    }
    assert_eq!(runtime::snapshot(&f.project).unwrap().attempts, before.attempts);
}

#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! Dependency satisfaction through the compiled CLI: signed contracts, CLI
//! submissions, sandboxed verification, Git CAS integration, signed factory
//! admission and memory reconciliation, observed through `scheduler inspect`
//! and the stored `dependency_satisfactions` rows. Attempts are recorded through
//! the public store API because no worker is launched.
use herdr_projects::{authority, domain::*, memory::*, migration, runtime};
use serde_json::{json, Value};
use std::{fs, path::PathBuf, process::{Command, Output}};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-projects");
const POLICY: &str = r#"{"version":1,"checks":["/usr/bin/git","diff","--quiet"]}"#;

struct Factory { home: tempfile::TempDir, project: PathBuf, key: PathBuf, repo: PathBuf, store: String, base: String, candidate: String }

impl Factory {
    /// A repository with a base commit, a candidate commit that adds `src/lib.rs`,
    /// and an unchecked-out `integration` branch at the base.
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let key = home.path().join("owner");
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&key).status().unwrap().success());
        let public = fs::read_to_string(key.with_extension("pub")).unwrap().split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        let config = home.path().join("owner.toml");
        fs::write(&config, format!("[authority]\nversion=1\nrevision=1\napproval_public_key={public:?}\n")).unwrap();
        let repo = home.path().join("repo");
        fs::create_dir(&repo).unwrap();
        let mut f = Factory { project: home.path().join("root/demo"), key, repo: repo.canonicalize().unwrap(), store: String::new(), base: String::new(), candidate: String::new(), home };
        for command in ["new", "pause"] { f.ok(&[command, "demo"]); }
        migration::apply(&f.project, &migration::inspect_with_config(&f.project, &config).unwrap(), true).unwrap();
        let s = runtime::snapshot(&f.project).unwrap();
        runtime::set_state(&f.project, s.head, s.control.unwrap().revision, ProjectState::Active, &config).unwrap();
        f.store = f.project.join(".state/state.db").canonicalize().unwrap().display().to_string();
        f.git(&["init", "-q", "--object-format=sha1", "--initial-branch=main"]);
        f.git(&["commit", "-q", "--allow-empty", "-m", "base"]);
        f.base = f.git(&["rev-parse", "HEAD"]);
        f.git(&["branch", "integration"]);
        fs::create_dir(f.repo.join("src")).unwrap();
        fs::write(f.repo.join("src/lib.rs"), "pub fn result() {}\n").unwrap();
        f.git(&["add", "."]);
        f.git(&["commit", "-q", "-m", "candidate"]);
        f.candidate = f.git(&["rev-parse", "HEAD"]);
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
    fn raw(&self) -> rusqlite::Connection { rusqlite::Connection::open(&self.store).unwrap() }
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
    /// Install (or revise) a signed contract that may integrate; returns its digest.
    fn contract(&self, task: &str, revision: u64) -> String {
        let body = json!({"version":1,"project_store":self.store,"expected_head":self.head(),"task_id":task,"contract_revision":revision,
            "deliverable":"dependency fixture","non_goals":"no worker launch","acceptance_policies":[{"id":"builds","text":POLICY}],
            "repository":self.repo,"base_oid":self.base,"object_format":"sha1","dependencies":[],"scope":{"paths":[{"path":"src/","access":"write"}],"named_resources":[]},
            "capability_flags":[],"profile_kind":"codex","retry_class":"none","result_schema_id":"result-v1","route":"verify_then_integrate",
            "authority":authority::policy_reference(&self.project).unwrap()});
        let (doc, sig) = self.sign(&format!("{task}-contract-{revision}.json"), &serde_json::to_vec(&body).unwrap(), authority::CONTRACT_SIGNATURE_NAMESPACE);
        self.ok(&["task", "demo", "contract", "put", "--input-file", doc.to_str().unwrap(), "--signature", sig.to_str().unwrap()])["digest"].as_str().unwrap().into()
    }
    /// A task with a signed contract and a running attempt `<task>-1` whose
    /// worker snapshot is scoped to `domains`; returns the snapshot id.
    fn predecessor(&self, task: &str, domains: &[&str]) -> String {
        self.ok(&["task", "demo", "add", task, "--title", task, "--expected-head", &self.head().to_string()]);
        self.contract(task, 1);
        self.attempt(task, 1, domains)
    }
    /// Start attempt `<task>-<n>`, ending the task's current attempt first.
    fn attempt(&self, task: &str, n: u32, domains: &[&str]) -> String {
        let snapshot = self.memory().create_task_snapshot(SnapshotRequest { schema_version: 1, task_id: task.into(), profile: "worker".into(),
            domains: domains.iter().map(|d| d.to_string()).collect(), paths: vec![], pinned_keys: vec![], sensitivity: "default".into() },
            "worker", &"a".repeat(64), None, 32_000, "Dependency fixture instructions", 1, None).unwrap();
        let mut db = self.db();
        let state = db.read_snapshot(None).unwrap();
        let id = AttemptId::new(format!("{task}-{n}")).unwrap();
        let mut next = state.tasks.iter().find(|t| t.id.as_str() == task).unwrap().clone();
        let previous = next.revision;
        let mut mutations = vec![];
        if let Some(current) = next.active_attempt.take() {
            let mut ended = state.attempts.iter().find(|a| a.id == current).unwrap().clone();
            let expected = ended.revision;
            ended.revision += 1;
            ended.state = AttemptState::Completed;
            ended.termination_observed = true;
            mutations.push(Mutation::Attempt { expected: Some(expected), next: ended });
        }
        next.revision += 1;
        next.active_attempt = Some(id.clone());
        mutations.push(Mutation::Attempt { expected: None, next: Attempt { id, task: next.id.clone(), revision: 1,
            state: AttemptState::Running, snapshot: Some(snapshot.id.as_str().into()), reservation: format!("{task}-slot-{n}"), termination_observed: false } });
        mutations.push(Mutation::Task { expected: Some(previous), next });
        db.commit(Commit { expected_head: state.head, mutations }).unwrap();
        snapshot.id.as_str().into()
    }
    /// A consumer task queued behind `predecessor` for `requirement`.
    fn consumer(&self, task: &str, predecessor: &str, requirement: &str) {
        self.ok(&["task", "demo", "add", task, "--title", task, "--expected-head", &self.head().to_string()]);
        let request = self.path(&format!("{task}-queue.json"));
        fs::write(&request, json!({"priority":0,"dependencies":[{"predecessor":predecessor,"requirement":requirement}]}).to_string()).unwrap();
        self.ok(&["task", "demo", "queue", task, "--input-file", request.to_str().unwrap(), "--expected-revision", "1", "--expected-head", &self.head().to_string()]);
    }
    /// Submit the candidate for `attempt`; returns the submission id.
    fn submit(&self, task: &str, attempt: &str, revision: u64, key: &str) -> String {
        let digest: String = self.raw().query_row("SELECT raw_digest FROM task_contracts WHERE task_id=?1 AND contract_revision=?2",
            rusqlite::params![task, revision], |r| r.get(0)).unwrap();
        let objects: Vec<_> = self.git(&["rev-list", "--objects", "--all"]).lines().map(|line| {
            let oid = line.split_whitespace().next().unwrap();
            json!({"oid":oid,"relative_path":format!("{}/{}", &oid[..2], &oid[2..])})
        }).collect();
        let submission = json!({"idempotency_key":key,"task_id":task,"contract_revision":revision,"contract_digest":digest,
            "attempt_id":attempt,"repository":self.repo,"base_oid":self.base,"candidate_oid":self.candidate,
            "object_format":"sha1","artifact_manifest":[],"claimed_checks":[],"objects":objects});
        let path = self.path(&format!("{key}.json"));
        fs::write(&path, serde_json::to_vec(&submission).unwrap()).unwrap();
        self.ok(&["result", "demo", "submit", "--input-file", path.to_str().unwrap()])["submission_id"].as_str().unwrap().into()
    }
    /// Run the signed policy on a submission; returns the accepted result id.
    fn verify(&self, submission: &str, key: &str) -> String {
        let policy = self.path("policy.json");
        fs::write(&policy, POLICY).unwrap();
        let work = self.path(&format!("{key}-work"));
        let run = self.ok(&["result", "demo", "verify", submission, "--policy-id", "builds", "--policy-file", policy.to_str().unwrap(),
            "--idempotency-key", key, "--work-dir", work.to_str().unwrap()]);
        assert_eq!(run["state"], "accepted", "{run}");
        run["receipt"]["result_id"].as_str().unwrap().into()
    }
    fn verified(&self, task: &str, attempt: &str, key: &str) -> String {
        let submission = self.submit(task, attempt, 1, key);
        self.verify(&submission, key)
    }
    fn integrate(&self, result: &str, key: &str) -> Value {
        let work = self.path(&format!("{key}-work"));
        let out = self.ok(&["result", "demo", "integrate", result, "--repository", self.repo.to_str().unwrap(), "--idempotency-key", key, "--work-dir", work.to_str().unwrap()]);
        assert_eq!(out["state"], "integrated", "{out}");
        out
    }
    /// Owner-signed factory admission over a passing vertical-slice manifest.
    fn admit(&self) {
        let manifest = br#"{"vertical_slice":"pass","git_sha":"fixture"}"#;
        let evidence = self.path("manifest.json");
        fs::write(&evidence, manifest).unwrap();
        let body = json!({"version":1,"enabled":true,"project_store":self.store,
            "evidence_digest":format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(manifest)),"authority":authority::policy_reference(&self.project).unwrap()});
        let (doc, sig) = self.sign("admission.json", &serde_json::to_vec(&body).unwrap(), authority::ADMISSION_SIGNATURE_NAMESPACE);
        assert_eq!(self.ok(&["factory", "admission", "demo", "--enable", "--policy", doc.to_str().unwrap(), "--signature", sig.to_str().unwrap(),
            "--evidence", evidence.to_str().unwrap()])["factory_admission"], "on");
    }
    fn inspect(&self) -> Value { self.ok(&["scheduler", "demo", "inspect"]) }
    /// The consumer's dependency and admission blockers.
    fn blockers(&self, task: &str) -> Vec<String> { dependency_lines(&self.inspect(), task) }
    /// Stored `(evidence_id, state)` satisfactions for one requirement.
    fn satisfactions(&self, requirement: &str) -> Vec<(String, String)> {
        let raw = self.raw();
        let mut stmt = raw.prepare("SELECT evidence_id, state FROM dependency_satisfactions WHERE requirement=?1 ORDER BY state, evidence_id").unwrap();
        stmt.query_map([requirement], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect()
    }
    /// Propose, owner-review and promote one memory change from `producer`'s attempt.
    fn promote(&self, producer: &str, snapshot: &str, key: &str, domains: &[&str], impact: &str) {
        let body = self.memory().ingest_object(format!("{key} {}", self.head()).as_bytes()).unwrap();
        let id = format!("proposal-{key}-{}", self.head());
        let proposal = json!({"schema_version":1,"proposal_id":id,"producer":{"task_id":producer,"attempt_id":format!("{producer}-1")},
            "input_snapshot_id":snapshot,"changes":[{"record_key":key,"kind":"observation","scope":{"domains":domains,"paths":[]},
            "claim":key,"body_object":body.as_str(),"evidence":[],"based_on":[],"impact":impact}]});
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
        assert!(!open.is_empty());
        let head = self.head();
        let body = json!({"version":1,"id":format!("reconcile-{head}"),"project_store":self.store,"authority":authority::policy_reference(&self.project).unwrap(),
            "expected_head":head,"expires_unix_ms":jiff::Timestamp::now().as_millisecond()+60_000,"reason":"reviewed","invalidations":open});
        let (doc, sig) = self.sign("reconcile.json", &serde_json::to_vec(&body).unwrap(), authority::MEMORY_RECONCILE_NAMESPACE);
        self.ok(&["memory", "demo", "reconcile", doc.to_str().unwrap(), sig.to_str().unwrap(), "--expected-head", &head.to_string()]);
    }
}

fn dependency_lines(report: &Value, task: &str) -> Vec<String> {
    report["entries"].as_array().unwrap().iter().find(|e| e["task"] == task).unwrap()["blockers"].as_array().unwrap().iter()
        .map(|b| b.as_str().unwrap().to_owned())
        .filter(|b| ["verified_dependency_evidence_unavailable:", "admission_disabled:", "predecessor_failed:"].iter().any(|p| b.starts_with(p)))
        .collect()
}
fn unavailable(pred: &str, requirement: &str) -> Vec<String> { vec![format!("verified_dependency_evidence_unavailable:{pred}:{requirement}")] }
fn disabled(requirement: &str) -> Vec<String> { vec![format!("admission_disabled:{requirement}")] }

/// A verification satisfies only `verified_result` edges and an integration
/// only `integrated_commit` edges. A newer receipt of either kind replaces the
/// valid row and keeps the old one as invalid history, and the second
/// integration keeps its commit. Until signed factory admission, a satisfied
/// edge still reports `admission_disabled`, and admission alone does not enable launch.
#[test]
fn receipts_satisfy_their_own_edges_and_admission_gates_them() {
    let f = Factory::new();
    f.predecessor("pred", &[]);
    f.consumer("needs-verified", "pred", "verified_result");
    f.consumer("needs-integrated", "pred", "integrated_commit");
    assert_eq!(f.blockers("needs-verified"), unavailable("pred", "verified_result"));
    assert_eq!(f.blockers("needs-integrated"), unavailable("pred", "integrated_commit"));
    let submission = f.submit("pred", "pred-1", 1, "first");
    assert_eq!(f.blockers("needs-verified"), unavailable("pred", "verified_result"), "a submission is not a receipt");
    let first = f.verify(&submission, "first");
    let report = f.inspect();
    assert_eq!(dependency_lines(&report, "needs-verified"), disabled("verified_result"));
    assert_eq!(dependency_lines(&report, "needs-integrated"), unavailable("pred", "integrated_commit"));
    assert_eq!((report["launch_enabled"].clone(), report["capability"]["automatic_admission"].clone()), (json!(false), json!(false)));
    assert_eq!(f.satisfactions("verified_result"), [(first.clone(), "valid".into())]);
    assert!(f.satisfactions("integrated_commit").is_empty());

    let second = f.verified("pred", "pred-1", "second");
    assert_eq!(f.satisfactions("verified_result"), [(first, "invalid".into()), (second.clone(), "valid".into())]);
    assert_eq!(f.blockers("needs-verified"), disabled("verified_result"));

    f.ok(&["result", "demo", "configure-integration", "--repository", f.repo.to_str().unwrap(), "--reference", "refs/heads/integration"]);
    f.integrate(&second, "integrate-a");
    let raw = f.raw();
    let integrated = |id: &str| raw.query_row("SELECT integrated_id FROM integrated_commits WHERE operation_id=(SELECT operation_id FROM integration_operations WHERE idempotency_key=?1)", [id], |r| r.get::<_, String>(0)).unwrap();
    assert_eq!(f.satisfactions("integrated_commit"), [(integrated("integrate-a"), "valid".into())]);
    assert_eq!(f.blockers("needs-integrated"), disabled("integrated_commit"));
    let b = f.integrate(&second, "integrate-b");
    assert_eq!(raw.query_row("SELECT count(*) FROM integrated_commits", [], |r| r.get::<_, i64>(0)).unwrap(), 2);
    assert_eq!(f.git(&["rev-parse", "refs/heads/integration"]), b["commit_oid"].as_str().unwrap());
    assert_eq!(f.satisfactions("integrated_commit"), [(integrated("integrate-a"), "invalid".into()), (integrated("integrate-b"), "valid".into())]);
    assert_eq!(f.blockers("needs-integrated"), disabled("integrated_commit"));

    f.admit();
    let report = f.inspect();
    assert_eq!(report["capability"]["automatic_admission"], true);
    for task in ["needs-verified", "needs-integrated"] { assert!(dependency_lines(&report, task).is_empty(), "{task}: {report}"); }
    let all: Vec<&Value> = report["entries"].as_array().unwrap().iter().flat_map(|e| e["blockers"].as_array().unwrap()).chain(report["capability"]["blockers"].as_array().unwrap()).collect();
    assert!(all.iter().all(|b| !b.as_str().unwrap().contains("admission_disabled")), "{all:?}");
    assert_eq!(report["launch_enabled"], false, "other blockers remain");
    assert!(report["entries"].as_array().unwrap().iter().all(|e| !e["blockers"].as_array().unwrap().is_empty()));
}

/// Only the task's current attempt counts. A verification of an older attempt
/// that finishes after the current one's neither replaces the current
/// satisfaction nor takes the slot for a consumer queued afterwards.
#[test]
fn a_late_verification_of_an_older_attempt_keeps_the_current_receipt() {
    let f = Factory::new();
    f.predecessor("pred", &[]);
    f.consumer("queued-before", "pred", "verified_result");
    let older = f.submit("pred", "pred-1", 1, "older");
    f.attempt("pred", 2, &[]);
    let current = f.verified("pred", "pred-2", "current");
    assert_eq!(f.blockers("queued-before"), disabled("verified_result"));
    let stale = f.verify(&older, "older");
    assert_eq!(f.raw().query_row("SELECT count(*) FROM verification_runs WHERE attempt_id='pred-1' AND state='accepted'", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
    assert_ne!(stale, current);
    assert_eq!(f.satisfactions("verified_result"), [(current.clone(), "valid".into())]);
    assert_eq!(f.blockers("queued-before"), disabled("verified_result"));
    f.consumer("queued-after", "pred", "verified_result");
    assert_eq!(f.blockers("queued-after"), disabled("verified_result"));
    let raw = f.raw();
    let evidence: Vec<String> = raw.prepare("SELECT evidence_id FROM dependency_satisfactions WHERE state='valid' ORDER BY task_id").unwrap()
        .query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
    assert_eq!(evidence, [current.clone(), current]);
}

/// A stored satisfaction stops counting once the predecessor starts a new
/// attempt or installs a newer contract revision, while the row itself stays valid.
#[test]
fn a_new_attempt_or_contract_revision_hides_the_stored_satisfaction() {
    let f = Factory::new();
    for pred in ["rerun", "revised"] {
        f.predecessor(pred, &[]);
        f.consumer(&format!("after-{pred}"), pred, "verified_result");
        f.verified(pred, &format!("{pred}-1"), pred);
        assert_eq!(f.blockers(&format!("after-{pred}")), disabled("verified_result"));
    }
    f.attempt("rerun", 2, &[]);
    f.contract("revised", 2);
    assert_eq!(f.blockers("after-rerun"), unavailable("rerun", "verified_result"));
    assert_eq!(f.blockers("after-revised"), unavailable("revised", "verified_result"));
    assert!(f.satisfactions("verified_result").iter().all(|(_, state)| state == "valid"));
    assert_eq!(f.satisfactions("verified_result").len(), 2);
}

/// An open stop-at-checkpoint memory invalidation for the predecessor hides
/// its satisfaction without touching the row or the consumer's queue entry;
/// owner reconciliation makes it count again.
#[test]
fn an_open_memory_fence_hides_the_satisfaction_until_reconciled() {
    let f = Factory::new();
    let scribe = f.predecessor("scribe", &[]);
    f.predecessor("pred", &["ui"]);
    f.consumer("needs-verified", "pred", "verified_result");
    let result = f.verified("pred", "pred-1", "pred");
    assert_eq!(f.blockers("needs-verified"), disabled("verified_result"));
    let queued = || f.raw().query_row("SELECT enqueue_sequence FROM task_queue WHERE task_id='needs-verified'", [], |r| r.get::<_, i64>(0)).unwrap();
    let before = queued();
    f.promote("scribe", &scribe, "ui.advice", &["ui"], "informational");
    assert_eq!(f.blockers("needs-verified"), disabled("verified_result"), "advisory memory is not a fence");
    f.promote("scribe", &scribe, "ui.breaking", &["ui"], "stop_at_checkpoint");
    assert_eq!(f.blockers("needs-verified"), unavailable("pred", "verified_result"));
    assert_eq!(f.satisfactions("verified_result"), [(result.clone(), "valid".into())]);
    f.reconcile("pred");
    assert_eq!(f.blockers("needs-verified"), disabled("verified_result"));
    assert_eq!(queued(), before);
    assert_eq!(f.satisfactions("verified_result"), [(result, "valid".into())]);
}

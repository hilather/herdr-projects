#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! Memory proposal review and promotion fences through the compiled CLI.
//! Proposals are submitted, owner-reviewed, promoted and hard-ruled with
//! `memory propose/review/promote/import`; unrelated work is `task add`.
//! A worker's input snapshot and running attempt are recorded through the
//! public store API, as no CLI path launches a worker here.
use herdr_farm::{authority, domain::*, memory::MemoryStore, migration, runtime};
use serde_json::{json, Value};
use std::{fs, path::PathBuf, process::{Command, Output}};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-farm");

struct Project { home: tempfile::TempDir, project: PathBuf, key: PathBuf, store: String }

impl Project {
    /// An active project with an owner key and a running worker attempt
    /// (`attempt-w` on `task-w`) whose knowledge snapshot proposals cite.
    fn new() -> (Self, String) {
        let home = tempfile::tempdir().unwrap();
        let key = home.path().join("owner");
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&key).output().unwrap().status.success());
        let public = fs::read_to_string(key.with_extension("pub")).unwrap().split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        let config = home.path().join(".config/herdr-farm/config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, format!("[authority]\nversion=1\nrevision=1\napproval_public_key={public:?}\n")).unwrap();
        let p = Project { project: home.path().join("root/demo"), key, store: String::new(), home };
        for command in ["new", "pause"] { p.ok(&[command, "demo"]); }
        fs::write(p.project.join("MEMORY.md"), "Ship only reviewed API changes.\n").unwrap();
        migration::apply(&p.project, &migration::inspect_with_config(&p.project, &config).unwrap(), true).unwrap();
        let store = p.project.join(".state/state.db").canonicalize().unwrap().display().to_string();
        let p = Project { store, ..p };
        let state = runtime::snapshot(&p.project).unwrap();
        runtime::set_state(&p.project, state.head, state.control.unwrap().revision, ProjectState::Active, &config).unwrap();
        p.ok(&["task", "demo", "add", "task-w", "--title", "worker", "--expected-head", &p.head().to_string()]);
        let mut memory = p.memory();
        let request = SnapshotRequest { schema_version: 1, task_id: "task-w".into(), profile: "worker".into(), domains: vec!["api".into()],
            paths: vec![], pinned_keys: vec![], sensitivity: "default".into() };
        let snapshot = memory.create_task_snapshot(request, "worker", &"a".repeat(64), None, 32_000, "Instructions", 1, None).unwrap();
        let mut db = migration::open_active(&p.project).unwrap();
        db.commit(Commit { expected_head: db.current_head().unwrap(), mutations: vec![Mutation::Attempt { expected: None, next: Attempt {
            id: AttemptId::new("attempt-w").unwrap(), task: TaskId::new("task-w").unwrap(), revision: 1, state: AttemptState::Running,
            snapshot: Some(snapshot.id.as_str().into()), reservation: "held".into(), termination_observed: false } }] }).unwrap();
        (p, snapshot.id.as_str().to_owned())
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
    fn memory(&self) -> MemoryStore { MemoryStore::from_sqlite(migration::open_active(&self.project).unwrap(), self.project.join(".state/objects")) }
    fn raw(&self) -> rusqlite::Connection { rusqlite::Connection::open(&self.store).unwrap() }
    fn sign(&self, name: &str, bytes: &[u8], namespace: &str) -> (String, String) {
        let path = self.home.path().join(name);
        fs::write(&path, bytes).unwrap();
        let signature = format!("{}.sig", path.display());
        let _ = fs::remove_file(&signature);
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-Y", "sign", "-f"]).arg(&self.key).args(["-n", namespace]).arg(&path).output().unwrap().status.success());
        (path.display().to_string(), signature)
    }
    /// Submit a one-change proposal from the running worker; returns its digest.
    fn propose(&self, snapshot: &str, id: &str, key: &str, kind: &str) -> String {
        let body = self.memory().ingest_object(format!("{key} body").as_bytes()).unwrap();
        let document = json!({"schema_version":1,"proposal_id":id,"producer":{"task_id":"task-w","attempt_id":"attempt-w"},
            "input_snapshot_id":snapshot,"observed_revisions":[],"changes":[{"record_key":key,"kind":kind,
            "scope":{"domains":["api"],"paths":[]},"claim":format!("{key} claim"),"body_object":body.as_str(),
            "evidence":[],"based_on":[],"impact":"informational"}]});
        let path = self.home.path().join(format!("{id}.json"));
        fs::write(&path, document.to_string()).unwrap();
        let receipt = self.ok(&["memory", "demo", "propose", "--input", path.to_str().unwrap()]);
        assert_eq!(receipt["validation"], "accepted", "{receipt}");
        receipt["payload_digest"].as_str().unwrap().to_owned()
    }
    /// The live v2 fence as a reviewer reads it from the store: every active
    /// mandatory or contract head, the scope catalogs and generations.
    fn read_set(&self) -> MemoryReadSet {
        let raw = self.raw();
        let pairs = |sql: &str| raw.prepare(sql).unwrap().query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u64>(1)?))).unwrap().map(Result::unwrap).collect::<Vec<_>>();
        let mandatory = "JOIN memory_records r ON r.id=h.record_id WHERE h.status='active' AND (r.is_hard=1 OR r.kind IN ('constraint','hard_memory','contract'))";
        MemoryReadSet {
            record_heads: pairs(&format!("SELECT h.record_id,h.revision FROM memory_heads h {mandatory} ORDER BY h.record_id")).into_iter()
                .map(|(record_id, revision)| ReadSetHead { record_id, revision }).collect(),
            validity_revisions: pairs(&format!("SELECT v.record_id,v.revision FROM memory_validity v JOIN memory_heads h ON h.record_id=v.record_id AND h.revision=v.revision {mandatory} ORDER BY v.record_id")).into_iter()
                .map(|(record_id, revision)| ReadSetValidity { record_id, revision }).collect(),
            reviewer_grant_id: None, revocation_epoch: 0,
            policy_revision: raw.query_row("SELECT COALESCE(MAX(revision),0) FROM memory_policies", [], |r| r.get(0)).unwrap(),
            required_set_generation: raw.query_row("SELECT generation FROM memory_required_generation", [], |r| r.get(0)).unwrap(),
            scope_catalog_generations: pairs("SELECT scope_id,generation FROM memory_scope_catalog ORDER BY scope_id").into_iter()
                .map(|(scope_id, generation)| ScopeCatalogGeneration { scope_id, generation }).collect(),
        }
    }
    /// Owner-sign and record an approving review; returns the decision id.
    fn review(&self, proposal: &str, digest: &str, key: &str, version: Option<u32>) -> String {
        let head = self.head();
        let document = MemoryReviewAuthorization { version: 1, project_store: self.store.clone(), authority: authority::policy_reference(&self.project).unwrap(),
            expected_head: head, expires_unix_ms: jiff::Timestamp::now().as_millisecond() + 600_000, proposal_digest: digest.into(), record_keys: vec![key.into()],
            review: ReviewDocument { schema_version: 1, proposal_id: proposal.into(), decision: "approve".into(), reason: "evidence supports the claim".into() },
            read_set_version: version, read_set: (version == Some(2)).then(|| self.read_set()) };
        let (doc, sig) = self.sign(&format!("{proposal}-review.json"), &serde_json::to_vec(&document).unwrap(), authority::MEMORY_REVIEW_NAMESPACE);
        let decision = self.ok(&["memory", "demo", "review", "--proposal", proposal, "--decision-file", &doc, "--signature", &sig, "--expected-head", &head.to_string()]);
        decision["id"].as_str().unwrap().to_owned()
    }
    fn promote(&self, proposal: &str, decision: &str) -> Output { self.cli(&["memory", "demo", "promote", "--proposal", proposal, "--decision", decision]) }
    /// A promotion refused on its fence writes no promotion row and no record.
    fn refused(&self, proposal: &str, decision: &str, key: &str) {
        let out = self.promote(proposal, decision);
        assert!(!out.status.success(), "{proposal} promoted: {}", String::from_utf8_lossy(&out.stdout));
        assert!(String::from_utf8_lossy(&out.stderr).contains("RevisionConflict"), "{}", String::from_utf8_lossy(&out.stderr));
        let raw = self.raw();
        assert_eq!(raw.query_row("SELECT count(*) FROM memory_promotions WHERE proposal_id=?1", [proposal], |r| r.get::<_, i64>(0)).unwrap(), 0);
        assert_eq!(raw.query_row("SELECT count(*) FROM memory_records WHERE record_key=?1", [key], |r| r.get::<_, i64>(0)).unwrap(), 0);
    }
    fn unrelated(&self, task: &str) { self.ok(&["task", "demo", "add", task, "--title", task, "--expected-head", &self.head().to_string()]); }
}

/// A v2 review survives unrelated events (task edits, another promotion)
/// and promotes once; a later replay returns the stored promotion row.
#[test]
fn unrelated_events_do_not_block_a_v2_promotion_and_replay_returns_the_stored_row() {
    let (p, snapshot) = Project::new();
    let digest = p.propose(&snapshot, "mp-api", "api.claim", "observation");
    let other = p.propose(&snapshot, "mp-note", "note.optional", "observation");
    let decision = p.review("mp-api", &digest, "api.claim", Some(2));
    p.unrelated("unrelated-1");
    let note = p.review("mp-note", &other, "note.optional", Some(2));
    assert!(p.promote("mp-note", &note).status.success());
    let first: PromotionReceipt = serde_json::from_value(p.ok(&["memory", "demo", "promote", "--proposal", "mp-api", "--decision", &decision])).unwrap();
    assert!(!first.reused);
    p.unrelated("unrelated-2");
    let replay: PromotionReceipt = serde_json::from_value(p.ok(&["memory", "demo", "promote", "--proposal", "mp-api", "--decision", &decision])).unwrap();
    assert!(replay.reused);
    assert_eq!((replay.sequence, &replay.change_ids), (first.sequence, &first.change_ids));
    let stored: u64 = p.raw().query_row("SELECT sequence FROM memory_promotions WHERE proposal_id='mp-api'", [], |r| r.get(0)).unwrap();
    assert_eq!(replay.sequence, stored);
    assert!(p.head() > replay.sequence);
    assert_eq!(p.raw().query_row("SELECT count(*) FROM memory_records WHERE record_key='api.claim'", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
}

/// A mandatory rule the owner adds after the review moves the v2 fence.
#[test]
fn a_new_hard_rule_blocks_a_v2_promotion() {
    let (p, snapshot) = Project::new();
    p.ok(&["memory", "demo", "import"]);
    let digest = p.propose(&snapshot, "mp-api", "api.claim", "observation");
    let decision = p.review("mp-api", &digest, "api.claim", Some(2));
    let head = p.head();
    let policy = MemoryPolicy { version: 1, project_store: p.store.clone(), revision: 1, authority: authority::policy_reference(&p.project).unwrap(),
        expected_head: head, op: MemoryPolicyOp::HardRule, record_key: Some("MEMORY.md".into()), memory_plan_digest: None, expected_memory_owner: None };
    let (doc, sig) = p.sign("hard-rule.json", &serde_json::to_vec(&policy).unwrap(), authority::MEMORY_SIGNATURE_NAMESPACE);
    p.ok(&["memory", "demo", "import", &doc, &sig, "--expected-head", &head.to_string()]);
    assert_eq!(p.raw().query_row("SELECT is_hard FROM memory_records WHERE record_key='MEMORY.md'", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
    p.refused("mp-api", &decision, "api.claim");
}

/// A contract inserted after the review is a phantom in the fence: it
/// conflicts, even though no existing head changed.
#[test]
fn a_contract_promoted_after_the_review_blocks_a_v2_promotion() {
    let (p, snapshot) = Project::new();
    let digest = p.propose(&snapshot, "mp-api", "api.claim", "observation");
    let contract = p.propose(&snapshot, "mp-contract", "api.contract", "contract");
    let decision = p.review("mp-api", &digest, "api.claim", Some(2));
    let contract_review = p.review("mp-contract", &contract, "api.contract", Some(2));
    assert!(p.promote("mp-contract", &contract_review).status.success());
    let (kind, hard): (String, i64) = p.raw().query_row("SELECT kind,is_hard FROM memory_records WHERE record_key='api.contract'", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!((kind.as_str(), hard), ("contract", 0));
    p.refused("mp-api", &decision, "api.claim");
}

/// Reviews without read set version 2 keep the whole-store fence: any
/// intervening event refuses promotion.
#[test]
fn reviews_without_a_v2_read_set_conflict_on_any_intervening_event() {
    for version in [None, Some(1), Some(3)] {
        let (p, snapshot) = Project::new();
        let digest = p.propose(&snapshot, "mp-api", "api.claim", "observation");
        let decision = p.review("mp-api", &digest, "api.claim", version);
        p.unrelated("unrelated");
        p.refused("mp-api", &decision, "api.claim");
    }
}

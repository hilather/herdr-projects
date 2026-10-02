#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! Regressions from the 2026-09-20 memory audit, driven through the compiled
//! CLI over a disposable migrated project: `memory propose/review/promote`,
//! signed `memory import` policy, `memory inspect/snapshot-input` and
//! coordinator `context`. Tasks go through `task add`; running attempts,
//! worker snapshots, object ingestion and garbage collection use the public
//! store API because no worker is launched and GC has no CLI verb.
use herdr_farm::{authority, domain::*, memory::*, migration, runtime};
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
        let config = home.path().join(".config/herdr-farm");
        fs::create_dir_all(&config).unwrap();
        fs::write(config.join("config.toml"), "[profiles.planner]\nkind='claude'\npermission_policy='interactive'\n").unwrap();
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
        serde_json::from_slice(&out.stdout).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&out.stdout).into_owned()))
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
    fn memory(&self) -> MemoryStore { MemoryStore::from_sqlite(migration::open_active(&self.project).unwrap(), self.project.join(".state/objects")) }
    fn ingest(&self, bytes: &[u8]) -> ObjectId { self.memory().ingest_object(bytes).unwrap() }
    fn available(&self, object: &ObjectId) -> bool { migration::open_active(&self.project).unwrap().object_available(object.as_str()).unwrap() }
    fn collect(&self) -> usize { self.memory().collect_unreferenced().unwrap() }
    /// Current head revision of `key` as `memory inspect` reports it.
    fn record(&self, key: &str) -> Option<Value> {
        self.ok(&["memory", "demo", "inspect"])["records"].as_array().unwrap().iter().find(|r| r["record_key"] == key).cloned()
    }
    fn expected(&self, key: &str) -> Value {
        let record = self.record(key).unwrap();
        json!({"record_id":record["id"],"revision":migration::open_active(&self.project).unwrap().memory_head(record["id"].as_str().unwrap()).unwrap().unwrap().revision})
    }
    fn snapshot(&self, task: &str, profile: &str, domains: &[&str], paths: &[&str], pinned: &[&str]) -> MemorySnapshot {
        let request = SnapshotRequest { schema_version: 1, task_id: task.into(), profile: profile.into(),
            domains: domains.iter().map(|d| d.to_string()).collect(), paths: paths.iter().map(|p| p.to_string()).collect(),
            pinned_keys: pinned.iter().map(|k| k.to_string()).collect(), sensitivity: "default".into() };
        self.memory().create_task_snapshot(request, profile, &"a".repeat(64), None, 32_000, "Project instructions",
            jiff::Timestamp::now().as_millisecond(), None).unwrap()
    }
    fn record_task(&self, task: &str) -> Option<Task> { runtime::snapshot(&self.project).unwrap().tasks.into_iter().find(|t| t.id.as_str() == task) }
    fn add_task(&self, task: &str) { self.ok(&["task", "demo", "add", task, "--title", task, "--expected-head", &self.head().to_string()]); }
    /// A task whose running attempt consumed a fresh worker snapshot; returns the snapshot id.
    fn producer(&self, task: &str) -> String { self.worker(task, &[]) }
    /// A task whose running attempt consumed a worker snapshot scoped to `paths`.
    fn worker(&self, task: &str, paths: &[&str]) -> String {
        if self.record_task(task).is_none() { self.add_task(task); }
        let snapshot = self.snapshot(task, "worker", &[], paths, &[]);
        let mut db = migration::open_active(&self.project).unwrap();
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
    /// `memory propose` one change; `change` overrides fields of the default change.
    fn propose(&self, id: &str, producer: &str, snapshot: &str, key: &str, body: &ObjectId, change: Value) -> Value {
        let mut proposal = json!({"schema_version":1,"proposal_id":id,"producer":{"task_id":producer,"attempt_id":format!("{producer}-attempt")},
            "input_snapshot_id":snapshot,"changes":[{"record_key":key,"kind":"observation","scope":{"domains":[],"paths":[]},
            "claim":key,"body_object":body.as_str(),"evidence":[],"based_on":[],"impact":"informational"}]});
        if let Value::Object(change) = change { for (k, v) in change { proposal["changes"][0][k] = v; } }
        let path = self.path(&format!("{id}.json"));
        fs::write(&path, serde_json::to_vec(&proposal).unwrap()).unwrap();
        let receipt = self.ok(&["memory", "demo", "propose", "--input", path.to_str().unwrap()]);
        assert_eq!(receipt["validation"], "accepted", "{receipt}");
        receipt
    }
    /// Owner-signed approval through `memory review`; returns the decision id.
    fn review(&self, id: &str, key: &str, receipt: &Value) -> String {
        let head = self.head();
        let review = json!({"version":1,"project_store":self.store,"authority":authority::policy_reference(&self.project).unwrap(),
            "expected_head":head,"expires_unix_ms":jiff::Timestamp::now().as_millisecond()+60_000,"proposal_digest":receipt["payload_digest"],
            "record_keys":[key],"review":{"schema_version":1,"proposal_id":id,"decision":"approve","reason":"reviewed"}});
        let (doc, sig) = self.sign(&format!("{id}-review.json"), &serde_json::to_vec(&review).unwrap(), authority::MEMORY_REVIEW_NAMESPACE);
        self.ok(&["memory", "demo", "review", "--proposal", id, "--decision-file", doc.to_str().unwrap(), "--signature", sig.to_str().unwrap(), "--expected-head", &head.to_string()])["id"]
            .as_str().unwrap().into()
    }
    fn promote(&self, id: &str, decision: &str) -> Output { self.cli(&["memory", "demo", "promote", "--proposal", id, "--decision", decision]) }
    /// Propose, review and promote a new record.
    fn remember(&self, producer: &str, snapshot: &str, key: &str, body: &[u8], change: Value) {
        let id = format!("proposal-{key}");
        let receipt = self.propose(&id, producer, snapshot, key, &self.ingest(body), change);
        let decision = self.review(&id, key, &receipt);
        let out = self.promote(&id, &decision);
        assert!(out.status.success(), "promote {key}: {}", String::from_utf8_lossy(&out.stderr));
    }
    /// Install an owner-signed memory policy operation through `memory import`.
    fn policy(&self, op: &str, key: &str) {
        let (head, revision) = (self.head(), self.ok(&["memory", "demo", "inspect"])["policies"].as_array().unwrap().len() + 1);
        let body = json!({"version":1,"project_store":self.store,"revision":revision,"authority":authority::policy_reference(&self.project).unwrap(),
            "expected_head":head,"op":op,"record_key":key});
        let (doc, sig) = self.sign(&format!("policy-{revision}.json"), &serde_json::to_vec(&body).unwrap(), authority::MEMORY_SIGNATURE_NAMESPACE);
        self.ok(&["memory", "demo", "import", doc.to_str().unwrap(), sig.to_str().unwrap(), "--expected-head", &head.to_string()]);
    }
    fn context(&self, args: &[&str]) -> String {
        let mut full = vec!["context", "demo"];
        full.extend(args);
        let out = self.cli(&full);
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }
}

fn checkpoint(context: &str) -> &str { context.split_whitespace().nth(1).unwrap() }

/// Bytes a proposal names survive collection from propose through promotion,
/// re-adding collected bytes restores them, promotion replays its receipt, and
/// a hard rule installed after review blocks the reviewed rewrite at promote.
#[test]
fn reviewed_proposals_keep_their_bytes_through_gc_recheck_hard_rules_and_replay() {
    let p = Project::new();
    let snapshot = p.producer("writer");

    // An unreferenced object is purged; re-adding identical bytes restores it.
    let body = p.ingest(b"collected then re-added claim");
    assert!(p.collect() >= 1);
    assert!(!p.available(&body));
    assert_eq!(p.ingest(b"collected then re-added claim"), body);
    assert!(p.available(&body), "re-ingest returned success but the object stays purged");

    // The accepted proposal protects its body and evidence before and after promotion.
    let evidence = p.ingest(b"retained evidence");
    let receipt = p.propose("proposal-fact", "writer", &snapshot, "ops.fact", &body,
        json!({"evidence":[{"object":format!("sha256:{}", evidence.as_str()),"validation_id":null}]}));
    p.collect();
    assert!(p.available(&body) && p.available(&evidence), "GC removed bytes of an accepted proposal");
    let decision = p.review("proposal-fact", "ops.fact", &receipt);
    p.collect();
    assert!(p.available(&body) && p.available(&evidence));
    let first = p.promote("proposal-fact", &decision);
    assert!(first.status.success(), "{}", String::from_utf8_lossy(&first.stderr));
    let first: Value = serde_json::from_slice(&first.stdout).unwrap();
    let head = p.head();
    let replay: Value = serde_json::from_slice(&p.promote("proposal-fact", &decision).stdout).unwrap();
    assert_eq!(replay["sequence"], first["sequence"], "replayed promotion changed its event sequence");
    assert_eq!(replay["reused"], true);
    assert_eq!(p.head(), head);
    p.collect();
    assert!(p.available(&body) && p.available(&evidence), "GC removed promoted evidence");
    let rendered = p.ok(&["memory", "demo", "snapshot-input", "--id", p.snapshot("writer", "worker", &[], &[], &[]).id.as_str()]);
    assert!(rendered["text"].as_str().unwrap().contains("collected then re-added claim"), "{rendered}");

    // A hard rule installed between review and promotion refuses the reviewed rewrite.
    let fact = p.expected("ops.fact");
    let rewrite = p.propose("proposal-rewrite", "writer", &snapshot, "ops.fact", &p.ingest(b"stale rewrite"), json!({"expected":fact}));
    let rewrite = p.review("proposal-rewrite", "ops.fact", &rewrite);
    p.policy("hard_rule", "ops.fact");
    let head = p.head();
    let refused = p.promote("proposal-rewrite", &rewrite);
    assert!(!refused.status.success(), "a stale informational proposal rewrote a now-hard record");
    assert_eq!(p.head(), head);
    assert_eq!(p.expected("ops.fact"), fact);
    assert_eq!(p.record("ops.fact").unwrap()["is_hard"], true);
}

/// Worker snapshots re-evaluate policy (a new hard rule is mandatory, not a
/// cached optional entry), honor applicability instead of kind weight, and the
/// profile-only brief fallback never borrows task-local memory.
#[test]
fn snapshots_follow_hard_rules_and_scope_and_the_profile_fallback_stays_project_wide() {
    let p = Project::new();
    let snapshot = p.producer("writer");
    p.remember("writer", &snapshot, "ops.fact", b"now required", json!({}));
    p.remember("writer", &snapshot, "infra.fact", b"infra detail", json!({"scope":{"domains":["infra"],"paths":["infra/main.tf"]}}));
    p.remember("writer", &snapshot, "private.note", b"private task finding", json!({"kind":"task_local"}));
    let id = |key: &str| p.record(key).unwrap()["id"].as_str().unwrap().to_owned();
    let role = |s: &MemorySnapshot, key: &str| s.entries.iter().find(|e| e.record_id.as_str() == id(key)).map(|e| e.role.clone());

    p.add_task("ui-task");
    let before = p.snapshot("ui-task", "worker", &[], &[], &[]);
    assert_eq!(role(&before, "ops.fact").as_deref(), Some("optional"));
    let ui = p.snapshot("ui-task", "worker", &["ui"], &["src/ui/**"], &[]);
    assert_eq!(role(&ui, "infra.fact"), None, "unrelated infra record selected for a UI-only scope");
    let infra = p.snapshot("ui-task", "worker", &["infra"], &[], &[]);
    assert!(role(&infra, "infra.fact").is_some(), "matching scope must still select the record");

    p.policy("hard_rule", "ops.fact");
    let after = p.snapshot("ui-task", "worker", &[], &[], &[]);
    assert_eq!(role(&after, "ops.fact").as_deref(), Some("mandatory"), "newly hard rule reused the optional selection");
    let rendered = p.ok(&["memory", "demo", "snapshot-input", "--id", after.id.as_str()]);
    assert!(rendered["text"].as_str().unwrap().contains("now required"));

    // The owning task holds a snapshot on a shared profile; the profile-only
    // fallback for that profile must not borrow its task-local memory.
    p.snapshot("writer", "shared-profile", &[], &[], &[]);
    let (index, files) = load_brief_memory(&p.project, "shared-profile", 32_000).unwrap();
    assert!(!index.contains("private task finding"), "{index}");
    assert!(index.contains("now required"), "{index}");
    assert!(files.is_empty());
}

/// Coordinator sessions keep independent cursors, deltas carry edited project
/// instructions, and acknowledging an older checkpoint cannot rewind a session.
#[test]
fn coordinator_sessions_keep_independent_monotonic_cursors_and_see_instruction_edits() {
    let p = Project::new();
    let a1 = p.context(&["--peek", "--session", "session-a"]);
    let b1 = p.context(&["--peek", "--session", "session-b"]);
    assert!(a1.contains("kind=full") && b1.contains("kind=full"), "{a1}\n{b1}");
    p.context(&["--session", "session-a", "--ack", checkpoint(&a1)]);
    assert!(p.context(&["--peek", "--session", "session-a"]).contains("kind=delta"));
    assert!(p.context(&["--peek", "--session", "session-b"]).contains("kind=full"), "session-b inherited session-a's cursor");

    let updated = "NEW MANDATORY INSTRUCTION: require two reviewers";
    fs::write(p.project.join("PROJECT.md"), format!("+++\nname='Demo'\n+++\n{updated}\n")).unwrap();
    let a2 = p.context(&["--peek", "--session", "session-a"]);
    assert!(a2.contains(updated), "next context omitted changed project instructions: {a2}");
    p.context(&["--session", "session-a", "--ack", checkpoint(&a2)]);

    p.add_task("new-task");
    let a3 = p.context(&["--peek", "--session", "session-a"]);
    assert!(a3.contains("kind=delta") && a3.contains("new-task"), "{a3}");
    p.context(&["--session", "session-a", "--ack", checkpoint(&a3)]);
    let _ = p.cli(&["context", "demo", "--session", "session-a", "--ack", checkpoint(&a2)]);
    let a4 = p.context(&["--peek", "--session", "session-a"]);
    assert!(a4.contains("kind=delta") && !a4.contains("new-task"), "stale acknowledgment rewound the session: {a4}");
}

impl Project {
    /// `memory snapshot` for `task` on the `planner` profile.
    fn cli_snapshot(&self, task: &str, domains: &[&str], pinned: &[&str]) -> Output {
        let scope = self.path("scope.json");
        fs::write(&scope, json!({"schema_version":1,"task_id":task,"profile":"planner","domains":domains,"paths":[],"pinned_keys":pinned,"sensitivity":"default"}).to_string()).unwrap();
        self.cli(&["memory", "demo", "snapshot", "--task", task, "--profile", "planner", "--input-file", scope.to_str().unwrap()])
    }
    fn snapshot_ok(&self, task: &str, domains: &[&str], pinned: &[&str]) -> Value {
        let out = self.cli_snapshot(task, domains, pinned);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn entry_keys(&self, snapshot: &Value) -> Vec<String> {
        let records = self.ok(&["memory", "demo", "inspect"])["records"].as_array().unwrap().clone();
        snapshot["entries"].as_array().unwrap().iter()
            .map(|e| records.iter().find(|r| r["id"] == e["record_id"]).unwrap()["record_key"].as_str().unwrap().to_owned()).collect()
    }
    /// The legacy files `memory plan` inventories: an index and one note.
    fn legacy_memory(&self, note: &str) {
        fs::write(self.project.join("MEMORY.md"), "# Memory\nindex body\n").unwrap();
        fs::write(self.project.join("memory/api.md"), note).unwrap();
    }
}

/// Replaces `same_inputs_reuse_manifest_and_unrelated_domain_is_excluded`.
#[test]
fn repeated_snapshot_reuses_its_manifest_and_selects_by_scope_or_pin() {
    let p = Project::new();
    let snapshot = p.producer("writer");
    p.remember("writer", &snapshot, "ui.note", b"ui body", json!({"scope":{"domains":["ui"],"paths":[]}}));
    p.remember("writer", &snapshot, "infra.note", b"infra body", json!({"scope":{"domains":["infra"],"paths":[]}}));
    p.remember("writer", &snapshot, "api.contract", b"contract body", json!({"scope":{"domains":["infra"],"paths":[]}}));
    p.add_task("ui-task");
    let first = p.snapshot_ok("ui-task", &["ui"], &["api.contract"]);
    let again = p.snapshot_ok("ui-task", &["ui"], &["api.contract"]);
    assert_eq!((&again["id"], &again["manifest_hash"]), (&first["id"], &first["manifest_hash"]));
    let keys = p.entry_keys(&first);
    assert!(keys.contains(&"ui.note".into()) && keys.contains(&"api.contract".into()), "{keys:?}");
    assert!(!keys.contains(&"infra.note".into()), "unrelated domain selected: {keys:?}");
    // A later record makes a new snapshot at a later sequence.
    p.remember("writer", &snapshot, "ui.later", b"later body", json!({"scope":{"domains":["ui"],"paths":[]}}));
    let later = p.snapshot_ok("ui-task", &["ui"], &["api.contract"]);
    assert_ne!(later["id"], first["id"]);
    assert!(later["sequence"].as_u64() > first["sequence"].as_u64());
    assert!(p.entry_keys(&later).contains(&"ui.later".into()));
}

/// Replaces `coordinator_constructor_skips_tasks_and_cli_rejects_reserved_id`.
#[test]
fn coordinator_snapshots_hold_only_hard_rules_and_the_task_id_is_reserved() {
    let p = Project::new();
    let snapshot = p.producer("writer");
    p.remember("writer", &snapshot, "ops.rule", b"always rule", json!({}));
    p.remember("writer", &snapshot, "ops.note", b"optional note", json!({}));
    p.policy("hard_rule", "ops.rule");
    let context = p.context(&["--peek", "--session", "session-a"]);
    let id = context.split_whitespace().find_map(|w| w.strip_prefix("snapshot=")).unwrap();
    let rendered = p.ok(&["memory", "demo", "snapshot-input", "--id", id]);
    assert_eq!(rendered["snapshot"]["task_id"], "coordinator");
    assert_eq!(p.entry_keys(&rendered["snapshot"]), ["ops.rule"]);
    assert!(rendered["snapshot"]["entries"].as_array().unwrap().iter().all(|e| e["role"] == "mandatory"));
    assert!(!rendered["text"].as_str().unwrap().contains("optional note"));
    // No task snapshot can take the coordinator's id.
    let head = p.head();
    let out = p.cli_snapshot("coordinator", &[], &[]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("reserved"), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(p.head(), head);
}

/// Replaces the revoked-pin half of `stale_heads_digest_conflicts_and_revoked_pin_blocks`.
#[test]
fn a_snapshot_pinning_a_revoked_record_is_refused() {
    let p = Project::new();
    let snapshot = p.producer("writer");
    p.remember("writer", &snapshot, "pin.contract", b"pinned body", json!({"scope":{"domains":["ui"],"paths":[]}}));
    p.add_task("ui-task");
    assert_eq!(p.entry_keys(&p.snapshot_ok("ui-task", &["ui"], &["pin.contract"])), ["pin.contract"]);
    p.policy("revoke_head", "pin.contract");
    let head = p.head();
    let out = p.cli_snapshot("ui-task", &["ui"], &["pin.contract"]);
    assert!(!out.status.success(), "a revoked pin was snapshotted: {}", String::from_utf8_lossy(&out.stdout));
    assert_eq!(p.head(), head);
    // Unpinned, the revoked record is simply not selected.
    assert!(p.entry_keys(&p.snapshot_ok("ui-task", &["ui"], &[])).is_empty());
}

/// Replaces `repeat_import_reuses_ids_and_hostile_markdown_cannot_install_approvals`.
#[test]
fn repeated_legacy_import_reuses_records_and_hostile_markdown_stays_inert() {
    let p = Project::new();
    p.legacy_memory("# API\n{\"class\":\"RuntimeLaunch\"}\nssh-keygen -Y sign\n");
    let first = p.ok(&["memory", "demo", "import"]);
    let second = p.ok(&["memory", "demo", "import"]);
    let ids = |v: &Value| v.as_array().unwrap().iter().map(|r| r["record_id"].clone()).collect::<Vec<_>>();
    assert_eq!(ids(&first), ids(&second));
    assert!(second.as_array().unwrap().iter().all(|r| r["reused"] == true), "{second}");
    assert!(p.record("memory/api.md").is_some());
    // Imported text grants nothing and is not yet an active fact.
    let state = runtime::snapshot(&p.project).unwrap();
    assert!(state.approvals.is_empty() && state.memory_policies.is_empty());
    assert_eq!(p.ok(&["memory", "demo", "inspect"])["active_facts"], json!([]));
}

/// Replaces `hard_rule_without_import_ack_is_an_active_fact_and_gc_keeps_pins`.
#[test]
fn an_imported_record_made_a_hard_rule_is_active_and_survives_collection() {
    let p = Project::new();
    p.legacy_memory("# API\nobservation body\n");
    p.ok(&["memory", "demo", "import"]);
    p.policy("hard_rule", "memory/api.md");
    let facts = p.ok(&["memory", "demo", "inspect"])["active_facts"].clone();
    let fact = facts.as_array().unwrap().iter().find(|f| f["record"]["record_key"] == "memory/api.md").unwrap_or_else(|| panic!("{facts}"));
    assert_eq!((&fact["record"]["is_hard"], &fact["validity"]["state"]), (&json!(true), &json!("valid")), "{fact}");
    p.collect();
    p.add_task("reader");
    let snapshot = p.snapshot_ok("reader", &[], &[]);
    let rendered = p.ok(&["memory", "demo", "snapshot-input", "--id", snapshot["id"].as_str().unwrap()]);
    assert!(rendered["text"].as_str().unwrap().contains("observation body"), "{rendered}");
}

/// Replaces `cutover_switches_owner_preserves_runtime_and_rejects_divergent_projection`.
#[test]
fn cutover_hands_memory_to_the_store_and_never_overwrites_an_edited_projection() {
    let p = Project::new();
    p.legacy_memory("# API\nobservation body\n");
    let plan_file = p.path("memory-plan.json");
    let plan = p.ok(&["memory", "demo", "plan", "--output", plan_file.to_str().unwrap()]);
    p.ok(&["memory", "demo", "import"]);
    assert_eq!(p.ok(&["memory", "demo", "inspect"])["authority"], "legacy-markdown");
    let head = p.head();
    let policy = json!({"version":1,"project_store":p.store,"revision":1,"authority":authority::policy_reference(&p.project).unwrap(),
        "expected_head":head,"op":"cutover","memory_plan_digest":plan["digest"],"expected_memory_owner":"legacy-markdown"});
    let (doc, sig) = p.sign("cutover.json", &serde_json::to_vec(&policy).unwrap(), authority::MEMORY_SIGNATURE_NAMESPACE);
    let cutover = || p.cli(&["memory", "demo", "cutover", "--plan", plan_file.to_str().unwrap(), doc.to_str().unwrap(), sig.to_str().unwrap(),
        "--expected-head", &head.to_string(), "--writers-stopped"]);

    // Publication fails after the signed policy commits; the index is then edited by hand.
    fs::create_dir(p.project.join("MEMORY.projection-next")).unwrap();
    assert!(!cutover().status.success());
    fs::remove_dir(p.project.join("MEMORY.projection-next")).unwrap();
    fs::write(p.project.join("MEMORY.md"), "manual edit").unwrap();
    let out = cutover();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("edited memory projection preserved"), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(fs::read_to_string(p.project.join("MEMORY.md")).unwrap(), "manual edit");

    // With the original bytes back, recovery completes and later replays are stable.
    fs::write(p.project.join("MEMORY.md"), "# Memory\nindex body\n").unwrap();
    for _ in 0..2 {
        let out = cutover();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap()["phase"], "active");
    }
    let format: Value = serde_json::from_slice(&fs::read(p.project.join(".state/format.json")).unwrap()).unwrap();
    assert_eq!((&format["memory"], &format["runtime"]), (&json!("sqlite-v1"), &json!("sqlite-v2")));
    assert_eq!(p.ok(&["memory", "demo", "inspect"])["authority"], "sqlite-v1");
    let projected = fs::read_to_string(p.project.join("MEMORY.md")).unwrap();
    assert!(projected.contains("memory projection") && projected.contains("index body"), "{projected}");
    // The runtime store still takes work after the owner changed.
    p.add_task("after-cutover");
}

impl Project {
    /// `memory propose` of the exact proposal document `proposal`.
    fn propose_raw(&self, name: &str, proposal: &Value) -> Output {
        let path = self.path(&format!("{name}.json"));
        fs::write(&path, serde_json::to_vec(proposal).unwrap()).unwrap();
        self.cli(&["memory", "demo", "propose", "--input", path.to_str().unwrap()])
    }
    fn change(&self, id: &str, producer: &str, snapshot: &str, key: &str, body: &ObjectId, change: Value) -> Value {
        let mut proposal = json!({"schema_version":1,"proposal_id":id,"producer":{"task_id":producer,"attempt_id":format!("{producer}-attempt")},
            "input_snapshot_id":snapshot,"changes":[{"record_key":key,"kind":"observation","scope":{"domains":[],"paths":[]},
            "claim":key,"body_object":body.as_str(),"evidence":[],"based_on":[],"impact":"informational"}]});
        if let Value::Object(change) = change { for (k, v) in change { proposal["changes"][0][k] = v; } }
        proposal
    }
    fn id(&self, key: &str) -> String { self.record(key).unwrap()["id"].as_str().unwrap().to_owned() }
    fn active(&self, key: &str) -> bool {
        self.ok(&["memory", "demo", "inspect"])["active_facts"].as_array().unwrap().iter().any(|f| f["record"]["record_key"] == key)
    }
}

/// Replaces `duplicate_proposal_reuses_result_and_changed_bytes_conflict`.
///
/// Proposing the same bytes again returns the stored receipt; different
/// bytes under the same proposal id are refused without a write.
#[test]
fn a_repeated_proposal_is_reused_and_changed_bytes_under_its_id_are_refused() {
    let p = Project::new();
    let snapshot = p.producer("writer");
    let body = p.ingest(b"validation errors carry a request id");
    let first = p.propose("mp-api", "writer", &snapshot, "api.errors", &body, json!({"kind":"contract","impact":"reconcile_before_completion"}));
    let head = p.head();
    let again = p.propose("mp-api", "writer", &snapshot, "api.errors", &body, json!({"kind":"contract","impact":"reconcile_before_completion"}));
    assert_eq!((&again["proposal_id"], &again["payload_digest"], &again["reused"]), (&first["proposal_id"], &first["payload_digest"], &json!(true)));
    assert_eq!(first["reused"], false);
    assert_eq!(p.head(), head);
    let changed = p.change("mp-api", "writer", &snapshot, "api.errors", &body, json!({"kind":"contract","impact":"reconcile_before_completion","claim":"different"}));
    assert!(!p.propose_raw("mp-api-changed", &changed).status.success(), "changed bytes reused a proposal id");
    assert_eq!(p.head(), head);
}

/// Replaces `competing_promotions_conflict_and_stale_review_must_be_repeated`.
///
/// Two reviewed proposals rewrite the same revision. The first promoted wins;
/// the other is refused at promote and the record stays at the winner's
/// revision. A new proposal against the current revision must be reviewed
/// again before it promotes.
#[test]
fn competing_reviewed_rewrites_promote_once_and_the_loser_must_be_proposed_again() {
    let p = Project::new();
    let snapshot = p.producer("writer");
    p.remember("writer", &snapshot, "api.errors", b"first claim", json!({}));
    let base = p.expected("api.errors");
    let receipts: Vec<Value> = ["a", "b"].iter().map(|n| p.propose(&format!("rewrite-{n}"), "writer", &snapshot, "api.errors", &p.ingest(format!("claim {n}").as_bytes()), json!({"expected":base}))).collect();
    let decision = p.review("rewrite-a", "api.errors", &receipts[0]);
    let won = p.promote("rewrite-a", &decision);
    assert!(won.status.success(), "{}", String::from_utf8_lossy(&won.stderr));
    let winner = p.expected("api.errors");
    assert_eq!(winner["revision"], 2);
    let loser = p.review("rewrite-b", "api.errors", &receipts[1]);
    let head = p.head();
    assert!(!p.promote("rewrite-b", &loser).status.success(), "a stale rewrite promoted over the winner");
    assert_eq!((p.head(), p.expected("api.errors")), (head, winner.clone()));
    let receipt = p.propose("rewrite-c", "writer", &snapshot, "api.errors", &p.ingest(b"claim c"), json!({"expected":winner}));
    assert!(!p.promote("rewrite-c", &loser).status.success(), "another proposal's review was reused");
    let decision = p.review("rewrite-c", "api.errors", &receipt);
    assert!(p.promote("rewrite-c", &decision).status.success());
    assert_eq!(p.expected("api.errors")["revision"], 3);
}

/// Replaces `missing_expiry_and_revocation_are_not_active_facts` (missing
/// evidence and revocation) and `ingest_cancels_gc_claim_and_unreferenced_objects_are_purged`.
///
/// A proposal whose body was never stored is rejected. A revoked record is no
/// longer an active fact, but its body stays retained by revision history
/// while an unreferenced object is collected.
#[test]
fn missing_bodies_are_rejected_and_revoked_records_leave_the_facts_but_keep_their_bytes() {
    let p = Project::new();
    let snapshot = p.producer("writer");
    let missing = ObjectId::from_hex("ab".repeat(32)).unwrap();
    let out = p.propose_raw("mp-missing", &p.change("mp-missing", "writer", &snapshot, "gone", &missing, json!({})));
    let rejected: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    assert_eq!(rejected["validation"], "rejected", "{rejected}");
    assert!(rejected["reason"].as_str().unwrap().contains("missing"), "{rejected}");
    assert!(p.record("gone").is_none());

    let body = p.ingest(b"revoked claim body");
    p.remember("writer", &snapshot, "live", b"revoked claim body", json!({}));
    assert!(p.active("live"));
    p.policy("revoke_head", "live");
    assert!(!p.active("live"), "a revoked record is still an active fact");
    let orphan = p.ingest(b"orphan bytes");
    assert!(p.collect() >= 1);
    assert!(!p.available(&orphan));
    assert!(p.available(&body), "collection removed a revoked record's body");
}

/// Replaces `routing_keeps_consumed_historical_dependencies_and_scope_boundaries`,
/// `coordinator_binding_is_addressable_without_an_attempt` and
/// `shadow_compare_drops_no_required_recipient`.
///
/// A worker on `ui/components/button.rs` consumed `derived`, which was based
/// on `source`. Later revisions are delivered to that worker for `source` and
/// `derived` (even once `derived` moves out of its scope and drops the
/// dependency) but not for another task's local note or a record on
/// `ui-extra/`. Every change reaches the coordinator session, which has no
/// attempt. A snapshot the worker never used receives nothing.
#[test]
fn memory_changes_reach_the_coordinator_and_only_the_live_worker_that_consumed_them() {
    let p = Project::new();
    let writer = p.producer("writer");
    let other = p.producer("other");
    p.remember("writer", &writer, "source", b"source 1", json!({"scope":{"domains":[],"paths":["backend/**"]}}));
    let source = p.expected("source");
    let scribe = p.worker("scribe", &["backend/api.rs"]);
    p.remember("scribe", &scribe, "derived", b"derived 1", json!({"scope":{"domains":[],"paths":["ui/**"]},"based_on":[source]}));
    p.remember("other", &other, "foreign", b"foreign 1", json!({"kind":"task_local","scope":{"domains":[],"paths":["ui/**"]}}));
    p.remember("writer", &writer, "near-prefix", b"near 1", json!({"scope":{"domains":[],"paths":["ui-extra/**"]}}));
    p.add_task("reader");
    let unused = p.snapshot("reader", "worker", &[], &["backend/api.rs"], &[]);
    let live = p.worker("reader", &["ui/components/button.rs"]);
    let consumed: Vec<String> = p.ok(&["memory", "demo", "snapshot-input", "--id", &live])["snapshot"]["entries"].as_array().unwrap().iter()
        .map(|e| e["record_id"].as_str().unwrap().to_owned()).collect();
    assert_eq!(consumed, [p.id("derived")]);
    let coordinator = p.context(&["--peek", "--session", "session-a"]);
    let coordinator = coordinator.split_whitespace().find_map(|w| w.strip_prefix("snapshot=")).unwrap().to_owned();
    let attempts = runtime::snapshot(&p.project).unwrap().attempts.len();

    let before = p.ok(&["memory", "demo", "deliveries"]).as_array().unwrap().len();
    for (producer, snapshot, key, change) in [("scribe", &scribe, "derived", json!({"scope":{"domains":[],"paths":["backend/**"]}})),
        ("writer", &writer, "source", json!({"scope":{"domains":[],"paths":["backend/**"]}})),
        ("other", &other, "foreign", json!({"kind":"task_local","scope":{"domains":[],"paths":["ui/**"]}})),
        ("writer", &writer, "near-prefix", json!({"scope":{"domains":[],"paths":["ui-extra/**"]}}))] {
        let mut change = change;
        change["expected"] = p.expected(key);
        let id = format!("update-{key}");
        let receipt = p.propose(&id, producer, snapshot, key, &p.ingest(format!("{key} 2").as_bytes()), change);
        let decision = p.review(&id, key, &receipt);
        let out = p.promote(&id, &decision);
        assert!(out.status.success(), "{key}: {}", String::from_utf8_lossy(&out.stderr));
    }
    let deliveries = p.ok(&["memory", "demo", "deliveries"]).as_array().unwrap()[before..].to_vec();
    let to = |snapshot: &str| -> Vec<String> {
        let mut keys: Vec<String> = deliveries.iter().filter(|d| d["snapshot_id"] == snapshot)
            .map(|d| ["source", "derived", "foreign", "near-prefix"].into_iter().find(|k| p.id(k) == d["record_id"].as_str().unwrap()).unwrap().to_owned()).collect();
        keys.sort();
        keys
    };
    assert_eq!(to(&live), ["derived", "source"]);
    assert!(deliveries.iter().filter(|d| d["snapshot_id"] == live.as_str()).all(|d| d["task_id"] == "reader" && d["subscriber"] == "task:reader"));
    assert_eq!(to(&coordinator), ["derived", "foreign", "near-prefix", "source"]);
    assert!(deliveries.iter().filter(|d| d["snapshot_id"] == coordinator.as_str()).all(|d| d["subscriber"].as_str().unwrap().starts_with("coordinator:") && d["task_id"].is_null()), "{deliveries:?}");
    assert!(to(unused.id.as_str()).is_empty(), "a snapshot no attempt used received a delivery");
    assert_eq!(runtime::snapshot(&p.project).unwrap().attempts.len(), attempts, "routing created an attempt");
}

/// Replaces `delegation_expiry_reason_does_not_classify_authority_mismatch_as_expired`.
///
/// Refused `memory review` commands are recorded as authority denials with a
/// reason: an expired authorization is `expired`, one naming another
/// authority policy is not, and an unreadable document is `invalid_document`.
/// None of them changes the head.
#[test]
fn refused_reviews_record_expiry_only_for_an_expired_authorization() {
    let p = Project::new();
    let snapshot = p.producer("writer");
    let receipt = p.propose("proposal-a", "writer", &snapshot, "ops.fact", &p.ingest(b"claim"), json!({}));
    let head = p.head();
    let mut authority = serde_json::to_value(authority::policy_reference(&p.project).unwrap()).unwrap();
    let valid = json!({"version":1,"project_store":p.store,"authority":authority,"expected_head":head,"expires_unix_ms":jiff::Timestamp::now().as_millisecond()+60_000,
        "proposal_digest":receipt["payload_digest"],"record_keys":["ops.fact"],"review":{"schema_version":1,"proposal_id":"proposal-a","decision":"approve","reason":"reviewed"}});
    authority["digest"] = json!("0".repeat(64));
    let mut expired = valid.clone();
    expired["expires_unix_ms"] = json!(jiff::Timestamp::now().as_millisecond() - 1);
    let mut foreign = valid.clone();
    foreign["authority"] = authority;
    let reasons: Vec<String> = [("expired", expired.to_string()), ("foreign", foreign.to_string()), ("malformed", "{\"version\":1}".to_owned())].iter().map(|(name, body)| {
        let (doc, sig) = p.sign(&format!("{name}.json"), body.as_bytes(), authority::MEMORY_REVIEW_NAMESPACE);
        let out = p.cli(&["memory", "demo", "review", "--proposal", "proposal-a", "--decision-file", doc.to_str().unwrap(), "--signature", sig.to_str().unwrap(), "--expected-head", &head.to_string()]);
        assert!(!out.status.success(), "{name} review accepted");
        assert_eq!(p.head(), head);
        let denials = p.ok(&["approval", "demo", "denials"]);
        let last = denials.as_array().unwrap().last().unwrap().clone();
        assert_eq!((last["class"].as_str(), last["command"].as_str()), (Some("memory"), Some("review")), "{denials}");
        last["reason_code"].as_str().unwrap().to_owned()
    }).collect();
    assert_eq!(reasons, ["expired", "refused", "invalid_document"]);
}

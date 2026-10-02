#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! Update packages for memory consumers. Changes are proposed, owner-reviewed
//! and promoted through the compiled CLI, and packages are pulled with
//! `memory package`, each pull in a fresh process. No coordinator session or
//! worker is launched, so coordinator snapshots, the producer's attempt,
//! consumer-binding retirement and package acknowledgment use the public store
//! API (as in tests/memory_barriers.rs).
use herdr_farm::{authority, domain::*, memory::MemoryStore, migration, runtime, store::{SqliteStore, UpdatePackageAck}};
use serde_json::{json, Value};
use std::{fs, path::PathBuf, process::{Command, Output}};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-farm");

struct Project { home: tempfile::TempDir, project: PathBuf, key: PathBuf, store: String }

impl Project {
    /// An active project with a producer task `producer` whose running attempt
    /// consumed a snapshot scoped to `ui`.
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
        p.worker("producer");
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
    fn db(&self) -> SqliteStore { migration::open_active(&self.project).unwrap() }
    fn raw(&self) -> rusqlite::Connection { rusqlite::Connection::open(&self.store).unwrap() }
    fn memory(&self) -> MemoryStore { MemoryStore::from_sqlite(self.db(), self.project.join(".state/objects")) }
    fn coordinator(&self, generation: &str) -> String {
        self.memory().create_coordinator_snapshot("session-a", "planner", &"b".repeat(64), None, 32_000, generation,
            jiff::Timestamp::now().as_millisecond()).unwrap().id.as_str().into()
    }
    /// Add `task` with a running attempt `{task}-attempt` on a `ui` snapshot; returns the snapshot.
    fn worker(&self, task: &str) -> String {
        self.ok(&["task", "demo", "add", task, "--title", task, "--expected-head", &self.head().to_string()]);
        let request = SnapshotRequest { schema_version: 1, task_id: task.into(), profile: "worker".into(),
            domains: vec!["ui".into()], paths: vec![], pinned_keys: vec![], sensitivity: "default".into() };
        let snapshot: String = self.memory().create_task_snapshot(request, "worker", &"a".repeat(64), None, 32_000, "Instructions",
            jiff::Timestamp::now().as_millisecond(), None).unwrap().id.as_str().into();
        let mut db = self.db();
        let state = db.read_snapshot(None).unwrap();
        let mut next = state.tasks.iter().find(|t| t.id.as_str() == task).unwrap().clone();
        let previous = next.revision;
        next.revision += 1;
        next.active_attempt = Some(AttemptId::new(format!("{task}-attempt")).unwrap());
        db.commit(Commit { expected_head: state.head, mutations: vec![
            Mutation::Attempt { expected: None, next: Attempt { id: AttemptId::new(format!("{task}-attempt")).unwrap(), task: next.id.clone(), revision: 1,
                state: AttemptState::Running, snapshot: Some(snapshot.clone()), reservation: format!("{task}-slot"), termination_observed: false } },
            Mutation::Task { expected: Some(previous), next },
        ] }).unwrap();
        snapshot
    }
    fn record(&self, key: &str) -> String {
        self.ok(&["memory", "demo", "inspect"])["records"].as_array().unwrap().iter().find(|r| r["record_key"] == key).unwrap()["id"].as_str().unwrap().into()
    }
    /// Propose, owner-review and promote `key` from the producer; `expected` revises it.
    fn promote(&self, key: &str, expected: Option<u64>) {
        let producer = self.db().read_snapshot(None).unwrap().attempts.into_iter().find(|a| a.task.as_str() == "producer").unwrap().snapshot.unwrap();
        let body = self.memory().ingest_object(format!("{key} {}", self.head()).as_bytes()).unwrap();
        let id = format!("proposal-{key}-{}", self.head());
        let mut proposal = json!({"schema_version":1,"proposal_id":id,"producer":{"task_id":"producer","attempt_id":"producer-attempt"},
            "input_snapshot_id":producer,"changes":[{"record_key":key,"kind":"observation","scope":{"domains":["ui"],"paths":[]},
            "claim":key,"body_object":body.as_str(),"evidence":[],"based_on":[],"impact":"informational"}]});
        if let Some(revision) = expected { proposal["changes"][0]["expected"] = json!({"record_id":self.record(key),"revision":revision}); }
        let path = self.path(&format!("{id}.json"));
        fs::write(&path, serde_json::to_vec(&proposal).unwrap()).unwrap();
        let receipt = self.ok(&["memory", "demo", "propose", "--input", path.to_str().unwrap()]);
        let head = self.head();
        let review = json!({"version":1,"project_store":self.store,"authority":authority::policy_reference(&self.project).unwrap(),
            "expected_head":head,"expires_unix_ms":jiff::Timestamp::now().as_millisecond()+60_000,"proposal_digest":receipt["payload_digest"],
            "record_keys":[key],"review":{"schema_version":1,"proposal_id":id,"decision":"approve","reason":"reviewed"}});
        let doc = self.path(&format!("{id}-review.json"));
        fs::write(&doc, serde_json::to_vec(&review).unwrap()).unwrap();
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-Y", "sign", "-f"]).arg(&self.key).args(["-n", authority::MEMORY_REVIEW_NAMESPACE]).arg(&doc).output().unwrap().status.success());
        let decision = self.ok(&["memory", "demo", "review", "--proposal", &id, "--decision-file", doc.to_str().unwrap(),
            "--signature", &format!("{}.sig", doc.display()), "--expected-head", &head.to_string()]);
        self.ok(&["memory", "demo", "promote", "--proposal", &id, "--decision", decision["id"].as_str().unwrap()]);
    }
    /// Pull a package in a fresh process; `changes` selects exact members.
    fn pull(&self, selector: &str, id: &str, changes: &[&str]) -> Output {
        let mut args = vec!["memory", "demo", "package", selector, id];
        for change in changes { args.extend(["--change", change]); }
        self.cli(&args)
    }
    fn package(&self, selector: &str, id: &str, changes: &[&str]) -> Value {
        let out = self.pull(selector, id, changes);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["package"].clone()
    }
    /// The one member of `package` that delivers `key`.
    fn change(&self, selector: &str, id: &str, key: &str) -> String {
        let record = self.record(key);
        let out = self.pull(selector, id, &[]);
        let manifest: Value = serde_json::from_slice(&out.stdout).unwrap();
        manifest["members"].as_array().unwrap().iter().find(|m| m["record_id"] == record.as_str()).unwrap()["change_id"].as_str().unwrap().into()
    }
    fn ack(&self, package: &Value, disposition: &str, changes: Option<&[&str]>) -> Result<(u64, String), String> {
        let change_ids = changes.map_or_else(|| serde_json::from_value(package["change_ids"].clone()).unwrap(), |c| c.iter().map(|c| c.to_string()).collect());
        let ack = UpdatePackageAck { schema_version: 1, package_id: package["package_id"].as_str().unwrap().into(),
            manifest_hash: package["manifest_hash"].as_str().unwrap().into(), change_ids, disposition: disposition.into() };
        self.db().acknowledge_update_package(&ack, jiff::Timestamp::now().as_millisecond())
            .map(|r| (r.sequence, r.package_id)).map_err(|e| e.to_string())
    }
    /// (source package, sequence) of `binding`'s `disposition` receipt for `change`.
    fn receipt(&self, binding: &str, change: &str, disposition: &str) -> Option<(String, u64)> {
        use rusqlite::OptionalExtension;
        self.raw().query_row("SELECT package_id,sequence FROM memory_change_receipts WHERE binding_id=?1 AND change_id=?2 AND disposition=?3",
            [binding, change, disposition], |r| Ok((r.get(0)?, r.get(1)?))).optional().unwrap()
    }
    fn cursor(&self, binding: &str) -> Option<String> {
        self.raw().query_row("SELECT applied_cursor FROM consumer_bindings WHERE binding_id=?1", [binding], |r| r.get(0)).unwrap()
    }
    fn count(&self, table: &str) -> u64 { self.raw().query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0)).unwrap() }
}

/// A coordinator pulls promoted changes as immutable packages. Pulling again
/// in a new process rebuilds the same package and acknowledges nothing; a
/// later promotion forms a wider package. `seen` must precede `applied`,
/// each logical receipt keeps the package that first produced it, and
/// applying an old package never applies a change promoted after it. A
/// selected pull narrows the batch but cannot name a change the binding does
/// not still owe, and a revised record cannot be applied at its old revision.
#[test]
fn coordinator_packages_rebuild_stably_and_receipts_keep_their_source() {
    let p = Project::new();
    let coordinator = p.coordinator("Coordinate");
    let (attempts, worker_receipts) = (p.count("attempts"), p.count("memory_update_receipts"));
    let refused = p.pull("--snapshot", &coordinator, &[]);
    assert!(!refused.status.success() && String::from_utf8_lossy(&refused.stderr).contains("no unresolved obligations"));
    p.promote("alpha", None);
    let first = p.pull("--snapshot", &coordinator, &[]);
    assert!(first.status.success(), "{}", String::from_utf8_lossy(&first.stderr));
    assert_eq!(p.pull("--snapshot", &coordinator, &[]).stdout, first.stdout, "a later pull rebuilds the same package");
    let p1 = p.package("--snapshot", &coordinator, &[]);
    let binding = p1["binding_id"].as_str().unwrap().to_owned();
    assert_eq!(p.package("--binding", &binding, &[]), p1);
    let a = p.change("--binding", &binding, "alpha");
    assert_eq!(p1["change_ids"], json!([a]));
    assert_eq!((p.count("memory_change_receipts"), p.count("update_packages")), (0, 1), "pulling acknowledges nothing");
    assert!(p.ack(&p1, "applied", None).unwrap_err().contains("seen acknowledgment required"));
    let seen1 = p.ack(&p1, "seen", None).unwrap();
    let head = p.head();
    assert_eq!(p.ack(&p1, "seen", None).unwrap(), seen1, "a replay returns the stored receipt");
    assert_eq!(p.head(), head);
    assert_eq!(p.cursor(&binding), None, "seen does not move the applied cursor");

    p.promote("beta", None);
    let b = p.change("--binding", &binding, "beta");
    let p2 = p.package("--binding", &binding, &[]);
    assert_ne!(p2["package_id"], p1["package_id"]);
    let mut both = vec![a.clone(), b.clone()]; both.sort();
    assert_eq!(p2["change_ids"], json!(both));
    let seen2 = p.ack(&p2, "seen", None).unwrap();
    assert_eq!(p.receipt(&binding, &a, "seen"), Some((p1["package_id"].as_str().unwrap().into(), seen1.0)), "the first package keeps the receipt");
    assert_eq!(p.receipt(&binding, &b, "seen"), Some((p2["package_id"].as_str().unwrap().into(), seen2.0)));
    assert_eq!(p.ack(&p1, "seen", None).unwrap(), seen1);
    // A selected package repeating `b` records its own acceptance, not a new receipt.
    let narrow = p.package("--binding", &binding, &[&b]);
    assert_eq!(narrow["change_ids"], json!([b]));
    assert_ne!(p.ack(&narrow, "seen", None).unwrap().0, seen2.0);
    assert_eq!(p.receipt(&binding, &b, "seen").unwrap().0, p2["package_id"].as_str().unwrap());

    // Applying the old package applies only its own change.
    let applied1 = p.ack(&p1, "applied", None).unwrap();
    let head = p.head();
    assert_eq!(p.ack(&p1, "applied", None).unwrap(), applied1);
    assert_eq!(p.head(), head);
    assert!(p.receipt(&binding, &a, "applied").is_some());
    assert_eq!(p.receipt(&binding, &b, "applied"), None);
    assert_eq!(p.cursor(&binding).as_deref(), p1["package_id"].as_str());
    assert!(p.ack(&p1, "applied", Some(&[&a, &b])).is_err(), "an ack cannot name changes outside its package");
    assert!(p.ack(&p1, "deferred", None).is_err());
    assert_eq!(p.package("--binding", &binding, &[])["change_ids"], json!([b]));

    let (head, packages) = (p.head(), p.count("update_packages"));
    for changes in [vec![a.as_str()], vec![&b, &b], vec![&b, "missing"]] {
        assert!(!p.pull("--binding", &binding, &changes).status.success(), "{changes:?}");
    }
    assert!(!p.pull("--binding", &"f".repeat(64), &[&b]).status.success());
    assert_eq!((p.head(), p.count("update_packages")), (head, packages), "refused pulls write nothing");

    // Revising `beta` supersedes `b`: its package cannot be applied, and the
    // receipts it would have touched keep their sources.
    p.promote("beta", Some(1));
    assert!(p.ack(&p2, "applied", None).unwrap_err().contains("superseded"));
    assert_eq!(p.receipt(&binding, &b, "applied"), None);
    assert_eq!(p.receipt(&binding, &a, "seen").unwrap().0, p1["package_id"].as_str().unwrap());
    let b2 = p.ok(&["memory", "demo", "package", "--binding", &binding])["members"].as_array().unwrap().iter()
        .find(|m| m["revision"] == 2).unwrap()["change_id"].as_str().unwrap().to_owned();
    let p3 = p.package("--binding", &binding, &[&b2]);
    p.ack(&p3, "seen", None).unwrap();
    p.ack(&p3, "applied", None).unwrap();
    assert!(p.receipt(&binding, &b2, "applied").is_some());
    assert_eq!(p.cursor(&binding).as_deref(), p3["package_id"].as_str());
    assert_eq!(p.ack(&p1, "applied", None).unwrap(), applied1, "the old package stays readable");
    assert_eq!((p.count("attempts"), p.count("memory_update_receipts")), (attempts, worker_receipts), "no worker evidence is invented");
}

/// A change copied to another binding stays owed there although a binding of
/// the same generation applied it, and a later coordinator generation that
/// inherits it must apply it itself. Only coordinator bindings move a cursor.
#[test]
fn a_change_applied_by_one_binding_stays_owed_by_the_bindings_it_was_copied_to() {
    let p = Project::new();
    let first = p.coordinator("Coordinate");
    let task = p.worker("consumer");
    p.promote("alpha", None);
    let coordinator = p.package("--snapshot", &first, &[]);
    let old = coordinator["binding_id"].as_str().unwrap().to_owned();
    let c = coordinator["change_ids"][0].as_str().unwrap().to_owned();
    p.ack(&coordinator, "seen", None).unwrap();
    let worker = p.db().consumer_binding_for_snapshot(&task).unwrap().unwrap();
    assert_eq!(worker.generation, p.db().consumer_binding(&old).unwrap().unwrap().generation);
    p.db().retire_consumer_binding(&old, Some(&worker.binding_id)).unwrap();
    p.ack(&coordinator, "applied", None).unwrap();
    assert_eq!(p.cursor(&old).as_deref(), coordinator["package_id"].as_str());

    let copied = p.package("--binding", &worker.binding_id, &[]);
    assert!(copied["change_ids"].as_array().unwrap().contains(&json!(c)), "{copied}");
    let second = p.coordinator("Coordinate again");
    let later = p.db().consumer_binding_for_snapshot(&second).unwrap().unwrap();
    assert!(later.generation > worker.generation);
    p.db().retire_consumer_binding(&worker.binding_id, Some(&later.binding_id)).unwrap();
    p.ack(&copied, "seen", None).unwrap();
    p.ack(&copied, "applied", None).unwrap();
    assert!(p.receipt(&worker.binding_id, &c, "applied").is_some());
    assert_eq!(p.cursor(&worker.binding_id), None, "a task binding has no applied cursor");

    let next = p.package("--binding", &later.binding_id, &[]);
    assert!(next["change_ids"].as_array().unwrap().contains(&json!(c)), "{next}");
    assert_eq!(p.receipt(&later.binding_id, &c, "applied"), None);
    p.ack(&next, "seen", None).unwrap();
    p.ack(&next, "applied", None).unwrap();
    assert!(p.receipt(&later.binding_id, &c, "applied").is_some());
    assert_eq!(p.cursor(&later.binding_id).as_deref(), next["package_id"].as_str());
}

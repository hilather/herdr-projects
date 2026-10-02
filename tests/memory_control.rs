#![cfg(feature = "state-store")]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
use herdr_farm::{authority, domain::*, memory::*, migration, runtime};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

// Test clients honor explicitly retryable acquisition failures caused by a
// concurrent fork retaining an unrelated descriptor until exec. Never retry
// authority, database, or application-validation failures.
fn retry_acquisition<T>(mut operation: impl FnMut() -> anyhow::Result<T>) -> anyhow::Result<T> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let result = operation();
        let retry = result.as_ref().err().is_some_and(|error| error.chain().any(|cause|
            matches!(cause.downcast_ref::<std::fs::TryLockError>(), Some(std::fs::TryLockError::WouldBlock))));
        if !retry || std::time::Instant::now() >= deadline { return result; }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let key = tmp.path().join("owner");
    assert!(
        Command::new("/usr/bin/ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key)
            .status()
            .unwrap()
            .success()
    );
    let public = fs::read_to_string(key.with_extension("pub"))
        .unwrap()
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    let config = tmp.path().join("config.toml");
    fs::write(
        &config,
        format!("[authority]\nversion=1\nrevision=1\napproval_public_key={public:?}\n"),
    )
    .unwrap();
    let project = tmp.path().join("project");
    fs::create_dir(&project).unwrap();
    for name in [".state", "threads", "inbox"] {
        fs::create_dir(project.join(name)).unwrap();
    }
    fs::write(
        project.join("PROJECT.md"),
        "+++\nname='Demo'\n+++\nOriginal instructions\n",
    )
    .unwrap();
    fs::write(project.join("TASKS.md"), "").unwrap();
    fs::write(project.join("MEMORY.md"), "Original memory").unwrap();
    fs::write(
        project.join(".state/project.json"),
        r#"{"status":"paused"}"#,
    )
    .unwrap();
    let p = migration::inspect_with_config(&project, &config).unwrap();
    migration::apply(&project, &p, true).unwrap();
    let state = runtime::snapshot(&project).unwrap();
    runtime::set_state(
        &project,
        state.head,
        state.control.unwrap().revision,
        ProjectState::Active,
        &config,
    )
    .unwrap();
    (tmp, project, key)
}
fn sign(key: &Path, path: &Path, bytes: &[u8], namespace: &str) -> PathBuf {
    fs::write(path, bytes).unwrap();
    let sig = PathBuf::from(format!("{}.sig", path.display()));
    let _ = fs::remove_file(&sig);
    assert!(
        Command::new("/usr/bin/ssh-keygen")
            .args(["-Y", "sign", "-f"])
            .arg(key)
            .args(["-n", namespace])
            .arg(path)
            .output()
            .unwrap()
            .status
            .success()
    );
    sig
}
fn decision(project: &Path, c: &MemoryImportCandidate, action: &str) -> MemoryImportDecision {
    MemoryImportDecision {
        version: 1,
        project_store: project
            .join(".state/state.db")
            .canonicalize()
            .unwrap()
            .display()
            .to_string(),
        authority: authority::policy_reference(project).unwrap(),
        expected_head: runtime::snapshot(project).unwrap().head,
        candidate_id: c.id.clone(),
        body_hash: c.body_hash.clone(),
        expected_revision: c.expected_revision,
        decision: action.into(),
    }
}
fn import(project: &Path) {
    let p = plan(project).unwrap();
    import_plan(project, &p).unwrap();
}

#[test]
fn manual_edit_stays_staged_until_exact_signed_review_and_replay_is_stable() {
    let (tmp, project, key) = fixture();
    import(&project);
    fs::write(project.join("MEMORY.md"), "Reviewed replacement").unwrap();
    assert!(import_file(&project, &project.join("MEMORY.md"), None).is_err());
    let c = import_file(&project, &project.join("MEMORY.md"), Some(1)).unwrap();
    let mut db = migration::open_active(&project).unwrap();
    assert_eq!(
        db.memory_head(c.record_id.as_str())
            .unwrap()
            .unwrap()
            .revision,
        1
    );
    fs::write(project.join("MEMORY.md"), "Changed after staging").unwrap();
    assert_eq!(
        import_candidate_preview(&project, &c.id).unwrap()["proposed"],
        "Reviewed replacement"
    );
    let mut memory = MemoryStore::from_sqlite(
        migration::open_active(&project).unwrap(),
        project.join(".state/objects"),
    );
    memory.collect_unreferenced().unwrap();
    assert!(db.object_available(c.body_hash.as_str()).unwrap());
    let doc = decision(&project, &c, "approve");
    let path = tmp.path().join("decision.json");
    let sig = sign(
        &key,
        &path,
        &serde_json::to_vec(&doc).unwrap(),
        authority::MEMORY_IMPORT_REVIEW_NAMESPACE,
    );
    let first = authority::review_memory_import(&project, &path, &sig, doc.expected_head).unwrap();
    let replay = authority::review_memory_import(&project, &path, &sig, doc.expected_head).unwrap();
    assert_eq!(first.sequence, replay.sequence);
    assert!(replay.reused);
    assert_eq!(first.resulting_revision, Some(2));
    assert_eq!(
        db.memory_revision(c.record_id.as_str(), 2)
            .unwrap()
            .unwrap()
            .body_hash,
        c.body_hash
    );
    assert!(import_file(&project, &project.join("MEMORY.md"), Some(1)).is_err());
    assert_eq!(
        db.memory_head(c.record_id.as_str())
            .unwrap()
            .unwrap()
            .revision,
        2
    );
}

#[test]
fn wrong_namespace_tampering_and_stale_approval_never_change_the_head() {
    let (tmp, project, key) = fixture();
    import(&project);
    fs::write(project.join("MEMORY.md"), "Candidate").unwrap();
    let c = import_file(&project, &project.join("MEMORY.md"), Some(1)).unwrap();
    let doc = decision(&project, &c, "approve");
    let path = tmp.path().join("decision.json");
    let bytes = serde_json::to_vec(&doc).unwrap();
    let sig = sign(&key, &path, &bytes, authority::MEMORY_SIGNATURE_NAMESPACE);
    assert!(authority::review_memory_import(&project, &path, &sig, doc.expected_head).is_err());
    let sig = sign(
        &key,
        &path,
        &bytes,
        authority::MEMORY_IMPORT_REVIEW_NAMESPACE,
    );
    let mut changed = doc.clone();
    changed.body_hash = ObjectId::from_hex("a".repeat(64)).unwrap();
    fs::write(&path, serde_json::to_vec(&changed).unwrap()).unwrap();
    assert!(authority::review_memory_import(&project, &path, &sig, doc.expected_head).is_err());
    fs::write(&path, &bytes).unwrap();
    runtime::add_task(
        &project,
        TaskId::new("new-task").unwrap(),
        "Unrelated event".into(),
        doc.expected_head,
    )
    .unwrap();
    assert!(authority::review_memory_import(&project, &path, &sig, doc.expected_head).is_err());
    let mut db = migration::open_active(&project).unwrap();
    assert_eq!(
        db.memory_head(c.record_id.as_str())
            .unwrap()
            .unwrap()
            .revision,
        1
    );
    let reject = decision(&project, &c, "reject");
    let sig = sign(
        &key,
        &path,
        &serde_json::to_vec(&reject).unwrap(),
        authority::MEMORY_IMPORT_REVIEW_NAMESPACE,
    );
    assert_eq!(
        authority::review_memory_import(&project, &path, &sig, reject.expected_head)
            .unwrap()
            .resulting_revision,
        None
    );
    assert_eq!(
        db.memory_head(c.record_id.as_str())
            .unwrap()
            .unwrap()
            .revision,
        1
    );
}

#[test]
fn snapshot_reconstructs_original_instructions_task_and_scope_after_edits() {
    let (_tmp, project, _) = fixture();
    let head = runtime::snapshot(&project).unwrap().head;
    runtime::add_task(
        &project,
        TaskId::new("task-a").unwrap(),
        "Original task".into(),
        head,
    )
    .unwrap();
    let req = SnapshotRequest {
        schema_version: 1,
        task_id: "task-a".into(),
        profile: "worker".into(),
        domains: vec!["ui".into()],
        paths: vec!["src/ui".into()],
        pinned_keys: vec![],
        sensitivity: "default".into(),
    };
    let mut memory = MemoryStore::from_sqlite(
        migration::open_active(&project).unwrap(),
        project.join(".state/objects"),
    );
    let snap = memory
        .create_task_snapshot(
            req.clone(),
            "worker",
            &"a".repeat(64),
            None,
            32_000,
            "Original instructions",
            1,
            None,
        )
        .unwrap();
    fs::write(project.join("PROJECT.md"), "Changed instructions").unwrap();
    let head = runtime::snapshot(&project).unwrap().head;
    runtime::rename_task(
        &project,
        &TaskId::new("task-a").unwrap(),
        "Changed task".into(),
        1,
        head,
    )
    .unwrap();
    let rendered = render_knowledge_snapshot(&project, snap.id.as_str()).unwrap();
    let text = rendered["text"].as_str().unwrap();
    assert!(text.contains("Original instructions"));
    assert!(text.contains("Original task"));
    assert!(!text.contains("Changed"));
    assert_eq!(
        rendered["inputs"]["request"],
        serde_json::to_value(req).unwrap()
    );
    let db = rusqlite::Connection::open(project.join(".state/state.db")).unwrap();
    assert!(
        db.execute(
            "UPDATE memory_snapshot_inputs SET instructions='tampered'",
            []
        )
        .is_err()
    );
}

#[test]
fn schema22_upgrade_does_not_invent_historical_inputs() {
    let (_tmp, project, _) = fixture();
    let path = project.join(".state/state.db");
    let db = rusqlite::Connection::open(&path).unwrap();
    test_schema::historical(&db, 22).unwrap();
    drop(db);
    let mut db = herdr_farm::store::SqliteStore::open(&path).unwrap();
    let before = db.read_snapshot(None).unwrap();
    db.upgrade_v1().unwrap();
    let after = db.read_snapshot(None).unwrap();
    assert_eq!(before.events, after.events);
    assert_eq!(after.schema_version, herdr_farm::store::SCHEMA);
    assert!(db.memory_snapshot_inputs("missing").is_err());
    db.integrity_check().unwrap();
}

#[test]
fn cutover_recovers_after_policy_and_owner_publication_using_original_signature() {
    let (tmp, project, key) = fixture();
    let p = plan(&project).unwrap();
    import_plan(&project, &p).unwrap();
    let head = runtime::snapshot(&project).unwrap().head;
    let policy = MemoryPolicy {
        version: 1,
        project_store: project
            .join(".state/state.db")
            .canonicalize()
            .unwrap()
            .display()
            .to_string(),
        revision: 1,
        authority: authority::policy_reference(&project).unwrap(),
        expected_head: head,
        op: MemoryPolicyOp::Cutover,
        record_key: None,
        memory_plan_digest: Some(p.digest.clone()),
        expected_memory_owner: Some("legacy-markdown".into()),
    };
    let plan_file = tmp.path().join("plan.json");
    fs::write(&plan_file, serde_json::to_vec(&p).unwrap()).unwrap();
    let doc = tmp.path().join("cutover.json");
    let sig = sign(
        &key,
        &doc,
        &serde_json::to_vec(&policy).unwrap(),
        authority::MEMORY_SIGNATURE_NAMESPACE,
    );
    // A publication failure after the policy transaction and ownership rename.
    fs::create_dir(project.join("MEMORY.projection-next")).unwrap();
    assert!(authority::cutover_memory(&project, &plan_file, &doc, &sig, head, true).is_err());
    assert_eq!(
        migration::read_format(&project).unwrap().memory,
        "sqlite-v1"
    );
    let current = runtime::snapshot(&project).unwrap();
    assert_eq!(current.memory_policies.len(), 1);
    assert!(
        runtime::add_task(
            &project,
            TaskId::new("blocked").unwrap(),
            "Must wait".into(),
            current.head
        )
        .is_err()
    );
    fs::remove_dir(project.join("MEMORY.projection-next")).unwrap();
    let recovered =
        authority::cutover_memory(&project, &plan_file, &doc, &sig, head, true).unwrap();
    assert_eq!(recovered.phase, migration::Phase::Active);
    assert!(
        fs::read_to_string(project.join("MEMORY.md"))
            .unwrap()
            .contains("memory projection")
    );
    assert_eq!(
        runtime::snapshot(&project).unwrap().memory_policies.len(),
        1
    );
    let replay = authority::cutover_memory(&project, &plan_file, &doc, &sig, head, true).unwrap();
    assert_eq!(replay.phase, migration::Phase::Active);
}

fn proposal(project: &Path) -> String {
    let head = runtime::snapshot(project).unwrap().head;
    runtime::add_task(
        project,
        TaskId::new("task-proposal").unwrap(),
        "Task".into(),
        head,
    )
    .unwrap();
    let mut memory = MemoryStore::from_sqlite(
        migration::open_active(project).unwrap(),
        project.join(".state/objects"),
    );
    let req = SnapshotRequest {
        schema_version: 1,
        task_id: "task-proposal".into(),
        profile: "worker".into(),
        domains: vec!["ui".into()],
        paths: vec![],
        pinned_keys: vec![],
        sensitivity: "default".into(),
    };
    let snap = memory
        .create_task_snapshot(
            req,
            "worker",
            &"a".repeat(64),
            None,
            32_000,
            "Instructions",
            1,
            None,
        )
        .unwrap();
    let mut db = migration::open_active(project).unwrap();
    let head = db.read_snapshot(None).unwrap().head;
    db.commit(Commit {
        expected_head: head,
        mutations: vec![Mutation::Attempt {
            expected: None,
            next: Attempt {
                id: AttemptId::new("attempt-a").unwrap(),
                task: TaskId::new("task-proposal").unwrap(),
                revision: 1,
                state: AttemptState::Running,
                snapshot: Some(snap.id.as_str().into()),
                reservation: "held".into(),
                termination_observed: false,
            },
        }],
    })
    .unwrap();
    let body = memory.ingest_object(&b"Reviewed claim"[..]).unwrap();
    let proposal = ProposalDocument {
        schema_version: 1,
        proposal_id: "proposal-a".into(),
        producer: ProposalProducer {
            task_id: "task-proposal".into(),
            attempt_id: "attempt-a".into(),
        },
        input_snapshot_id: snap.id.as_str().into(),
        observed_revisions: vec![],
        repository: None,
        changes: vec![ProposalChange {
            record_key: "ui.claim".into(),
            expected: None,
            kind: "observation".into(),
            scope: Applicability {
                domains: vec!["ui".into()],
                paths: vec![],
            },
            claim: "Claim".into(),
            body_object: body.as_str().into(),
            evidence: vec![],
            based_on: vec![],
            impact: "informational".into(),
        }],
    };
    let receipt = memory
        .propose(&serde_json::to_vec(&proposal).unwrap(), 1)
        .unwrap();
    assert_eq!(receipt.validation, "accepted");
    receipt.payload_digest
}

#[test]
fn proposal_review_requires_signed_exact_scope_and_promotion_rechecks_authority() {
    let (tmp, project, key) = fixture();
    let digest = proposal(&project);
    let head = runtime::snapshot(&project).unwrap().head;
    let mut doc = MemoryReviewAuthorization {
        version: 1,
        project_store: project
            .join(".state/state.db")
            .canonicalize()
            .unwrap()
            .display()
            .to_string(),
        authority: authority::policy_reference(&project).unwrap(),
        expected_head: head,
        expires_unix_ms: jiff::Timestamp::now().as_millisecond() + 60_000,
        proposal_digest: digest,
        record_keys: vec!["wrong.scope".into()],
        review: ReviewDocument {
            schema_version: 1,
            proposal_id: "proposal-a".into(),
            decision: "approve".into(),
            reason: "Verified evidence".into(),
        },
        read_set_version: None,
        read_set: None,
    };
    let path = tmp.path().join("review.json");
    let sig = sign(
        &key,
        &path,
        &serde_json::to_vec(&doc).unwrap(),
        authority::MEMORY_REVIEW_NAMESPACE,
    );
    assert!(authority::review_memory_proposal(&project, "proposal-a", &path, &sig, head).is_err());
    assert_eq!(runtime::snapshot(&project).unwrap().head, head);
    doc.record_keys = vec!["ui.claim".into()];
    let sig = sign(
        &key,
        &path,
        &serde_json::to_vec(&doc).unwrap(),
        "wrong-namespace",
    );
    assert!(authority::review_memory_proposal(&project, "proposal-a", &path, &sig, head).is_err());
    let sig = sign(
        &key,
        &path,
        &serde_json::to_vec(&doc).unwrap(),
        authority::MEMORY_REVIEW_NAMESPACE,
    );
    assert!(
        authority::review_memory_proposal(&project, "another-proposal", &path, &sig, head).is_err()
    );
    let review =
        authority::review_memory_proposal(&project, "proposal-a", &path, &sig, head).unwrap();
    // A changed pinned authority/configuration must not authorize promotion.
    let config = tmp.path().join("config.toml");
    let original = fs::read(&config).unwrap();
    fs::write(
        &config,
        String::from_utf8(original.clone())
            .unwrap()
            .replace("revision=1", "revision=2"),
    )
    .unwrap();
    assert!(authority::promote_memory_proposal(&project, "proposal-a", &review.id).is_err());
    fs::write(&config, &original).unwrap();
    let first = authority::promote_memory_proposal(&project, "proposal-a", &review.id).unwrap();
    let replay = authority::promote_memory_proposal(&project, "proposal-a", &review.id).unwrap();
    assert_eq!(first.sequence, replay.sequence);
    assert!(replay.reused);
}

#[test]
fn cutover_abort_is_limited_to_before_signed_policy_commit() {
    let (tmp, project, key) = fixture();
    let p = plan(&project).unwrap();
    import_plan(&project, &p).unwrap();
    // Model a crash immediately after recording cutover intent, before policy commit.
    let journal_path = project.join(".state/migration/memory-journal.json");
    let mut journal: MemoryJournal =
        serde_json::from_slice(&fs::read(&journal_path).unwrap()).unwrap();
    journal.phase = migration::Phase::CutoverPending;
    fs::write(&journal_path, serde_json::to_vec(&journal).unwrap()).unwrap();
    assert_eq!(
        abort_cutover(&project, &p, true).unwrap().phase,
        migration::Phase::Imported
    );
    let head = runtime::snapshot(&project).unwrap().head;
    let policy = MemoryPolicy {
        version: 1,
        project_store: project
            .join(".state/state.db")
            .canonicalize()
            .unwrap()
            .display()
            .to_string(),
        revision: 1,
        authority: authority::policy_reference(&project).unwrap(),
        expected_head: head,
        op: MemoryPolicyOp::Cutover,
        record_key: None,
        memory_plan_digest: Some(p.digest.clone()),
        expected_memory_owner: Some("legacy-markdown".into()),
    };
    let path = tmp.path().join("cutover.json");
    let sig = sign(
        &key,
        &path,
        &serde_json::to_vec(&policy).unwrap(),
        authority::MEMORY_SIGNATURE_NAMESPACE,
    );
    let plan_file = tmp.path().join("plan.json");
    fs::write(&plan_file, serde_json::to_vec(&p).unwrap()).unwrap();
    authority::cutover_memory(&project, &plan_file, &path, &sig, head, true).unwrap();
    assert!(abort_cutover(&project, &p, true).is_err());
}

#[test]
fn proposal_rejects_unconsumed_snapshot_and_unverified_evidence_claims() {
    let (_tmp, project, _key) = fixture();
    proposal(&project);
    let mut db = migration::open_active(&project).unwrap();
    let (_, payload) = db.memory_proposal_payload("proposal-a").unwrap().unwrap();
    let original: ProposalDocument = serde_json::from_str(&payload).unwrap();
    let mut memory = MemoryStore::from_sqlite(
        migration::open_active(&project).unwrap(),
        project.join(".state/objects"),
    );
    let mut unverified = original.clone();
    unverified.proposal_id = "proposal-validation".into();
    unverified.changes[0].evidence.push(ProposalEvidence {
        object: None,
        validation_id: Some("invented-validation".into()),
    });
    assert_eq!(
        memory
            .propose(&serde_json::to_vec(&unverified).unwrap(), 1)
            .unwrap()
            .validation,
        "rejected"
    );
    let mut state = db.read_snapshot(None).unwrap();
    let mut attempt = state.attempts.remove(0);
    let rev = attempt.revision;
    attempt.revision += 1;
    attempt.snapshot = None;
    db.commit(Commit {
        expected_head: state.head,
        mutations: vec![Mutation::Attempt {
            expected: Some(rev),
            next: attempt,
        }],
    })
    .unwrap();
    let mut unbound = original;
    unbound.proposal_id = "proposal-unbound".into();
    let receipt = memory
        .propose(&serde_json::to_vec(&unbound).unwrap(), 1)
        .unwrap();
    assert_eq!(receipt.validation, "rejected");
    assert!(receipt.reason.contains("not consumed"));
}

#[test]
fn manual_import_cli_stages_previews_and_requires_signed_review() {
    let (tmp, project, key) = fixture();
    import(&project);
    let cli = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_herdr-farm"))
            .env_clear()
            .env("HOME", tmp.path())
            .env("PATH", "/usr/bin:/bin")
            .args(["--root", tmp.path().to_str().unwrap(), "memory", "project"])
            .args(args)
            .output()
            .unwrap()
    };
    let file = project.join("MEMORY.md");
    fs::write(&file, "CLI candidate").unwrap();
    let out = cli(&[
        "import",
        "--file",
        file.to_str().unwrap(),
        "--expected-revision",
        "1",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let c: MemoryImportCandidate = serde_json::from_slice(&out.stdout).unwrap();
    let preview = cli(&["candidate", "--id", &c.id]);
    assert!(preview.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&preview.stdout).unwrap()["proposed"],
        "CLI candidate"
    );
    let doc = decision(&project, &c, "approve");
    let path = tmp.path().join("review.json");
    let sig = sign(
        &key,
        &path,
        &serde_json::to_vec(&doc).unwrap(),
        authority::MEMORY_IMPORT_REVIEW_NAMESPACE,
    );
    let out = cli(&[
        "review-import",
        path.to_str().unwrap(),
        sig.to_str().unwrap(),
        "--expected-head",
        &doc.expected_head.to_string(),
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<MemoryImportReceipt>(&out.stdout)
            .unwrap()
            .resulting_revision,
        Some(2)
    );
    let unsigned = cli(&[
        "review",
        "--proposal",
        "proposal-a",
        "--decision-file",
        path.to_str().unwrap(),
    ]);
    assert!(!unsigned.status.success());
    assert!(String::from_utf8_lossy(&unsigned.stderr).contains("--signature"));
}

#[test]
fn promotion_and_all_consumer_obligations_commit_or_roll_back_together() {
    let (tmp, project, key) = fixture();
    let digest = proposal(&project);
    let head = runtime::snapshot(&project).unwrap().head;
    runtime::add_task(
        &project,
        TaskId::new("consumer-b").unwrap(),
        "Other consumer".into(),
        head,
    )
    .unwrap();
    let mut memory = MemoryStore::from_sqlite(
        migration::open_active(&project).unwrap(),
        project.join(".state/objects"),
    );
    memory
        .create_task_snapshot(
            SnapshotRequest {
                schema_version: 1,
                task_id: "consumer-b".into(),
                profile: "worker".into(),
                domains: vec!["ui".into()],
                paths: vec![],
                pinned_keys: vec![],
                sensitivity: "default".into(),
            },
            "worker",
            &"a".repeat(64),
            None,
            32_000,
            "Instructions",
            1,
            None,
        )
        .unwrap();
    // A new claim must reach matching scopes, but not unrelated workers.
    for (id, domain) in [("backend-consumer", "backend"), ("ui-consumer", "ui")] {
        let head = runtime::snapshot(&project).unwrap().head;
        runtime::add_task(&project, TaskId::new(id).unwrap(), id.into(), head).unwrap();
        memory
            .create_task_snapshot(
                SnapshotRequest {
                    schema_version: 1,
                    task_id: id.into(),
                    profile: "worker".into(),
                    domains: vec![domain.into()],
                    paths: vec![],
                    pinned_keys: vec![],
                    sensitivity: "default".into(),
                },
                "worker",
                &"a".repeat(64),
                None,
                32_000,
                "Instructions",
                1,
                None,
            )
            .unwrap();
    }
    memory
        .create_coordinator_snapshot(
            "coordinator-a",
            "planner",
            &"b".repeat(64),
            None,
            32_000,
            "Instructions",
            1,
        )
        .unwrap();
    let head = runtime::snapshot(&project).unwrap().head;
    let doc = MemoryReviewAuthorization {
        version: 1,
        project_store: project
            .join(".state/state.db")
            .canonicalize()
            .unwrap()
            .display()
            .to_string(),
        authority: authority::policy_reference(&project).unwrap(),
        expected_head: head,
        expires_unix_ms: jiff::Timestamp::now().as_millisecond() + 60_000,
        proposal_digest: digest,
        record_keys: vec!["ui.claim".into()],
        review: ReviewDocument {
            schema_version: 1,
            proposal_id: "proposal-a".into(),
            decision: "approve".into(),
            reason: "Reviewed".into(),
        },
        read_set_version: None,
        read_set: None,
    };
    let path = tmp.path().join("review.json");
    let sig = sign(
        &key,
        &path,
        &serde_json::to_vec(&doc).unwrap(),
        authority::MEMORY_REVIEW_NAMESPACE,
    );
    let review =
        authority::review_memory_proposal(&project, "proposal-a", &path, &sig, head).unwrap();
    let raw = rusqlite::Connection::open(project.join(".state/state.db")).unwrap();
    raw.execute_batch("CREATE TRIGGER fail_routing BEFORE INSERT ON memory_delivery_intents WHEN NEW.subscriber='task:consumer-b' BEGIN SELECT RAISE(ABORT,'injected routing failure'); END;").unwrap();
    assert!(authority::promote_memory_proposal(&project, "proposal-a", &review.id).is_err());
    let mut db = migration::open_active(&project).unwrap();
    assert!(db.memory_record_by_key("ui.claim").unwrap().is_none());
    assert!(db.memory_promotion("proposal-a").unwrap().is_none());
    assert!(db.memory_delivery_intents().unwrap().is_empty());
    raw.execute_batch("DROP TRIGGER fail_routing;").unwrap();
    authority::promote_memory_proposal(&project, "proposal-a", &review.id).unwrap();
    authority::promote_memory_proposal(&project, "proposal-a", &review.id).unwrap();
    drop(db);
    let rows = migration::open_active(&project)
        .unwrap()
        .memory_delivery_intents()
        .unwrap();
    assert_eq!(rows.len(), 4);
    assert!(
        !rows
            .iter()
            .any(|r| r["subscriber"] == "task:backend-consumer")
    );
    assert!(rows.iter().any(|r| r["subscriber"] == "task:ui-consumer"));
    assert!(rows.iter().all(|r| r["state"] == "pending"));
    assert!(rows.iter().any(|r| r["subscriber"] == "task:consumer-b"));
    let binding:String=raw.query_row("SELECT binding_id FROM consumer_binding_obligations ORDER BY binding_id LIMIT 1",[],|row|row.get(0)).unwrap();
    let pull=|flag:&str,id:&str|Command::new(env!("CARGO_BIN_EXE_herdr-farm"))
        .env_clear().env("HOME",tmp.path()).env("PATH","/usr/bin:/bin")
        .args(["--root",tmp.path().to_str().unwrap(),"memory","project","package",flag,id])
        .output().unwrap();
    let first=pull("--binding",&binding);assert!(first.status.success(),"{}",String::from_utf8_lossy(&first.stderr));
    let manifest:serde_json::Value=serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(manifest["schema_version"],1);
    assert_eq!(manifest["package"]["binding_id"],binding);
    assert!(!manifest["members"].as_array().unwrap().is_empty());
    assert_eq!(manifest["package"]["change_ids"].as_array().unwrap().len(),manifest["members"].as_array().unwrap().len());
    for member in manifest["members"].as_array().unwrap() {
        assert_eq!(member["body_hash"].as_str().unwrap().len(),64);
        assert!(member["revision"].as_u64().unwrap()>0);
    }
    let snapshot:String=raw.query_row("SELECT snapshot_id FROM consumer_bindings WHERE binding_id=?1",[&binding],|row|row.get(0)).unwrap();
    let second=pull("--snapshot",&snapshot);assert!(second.status.success());assert_eq!(first.stdout,second.stdout);
    let third=pull("--binding",&binding);assert!(third.status.success());assert_eq!(first.stdout,third.stdout);
    assert_eq!(raw.query_row("SELECT count(*) FROM memory_change_receipts",[],|row|row.get::<_,u64>(0)).unwrap(),0);
    assert_eq!(migration::open_active(&project).unwrap().memory_delivery_intents().unwrap(),rows);
    let n: i64 = raw
        .query_row(
            "SELECT COUNT(DISTINCT task_id) FROM memory_invalidations WHERE record_id IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 3);
}

#[test]
fn worker_update_receipts_are_explicit_exact_and_survive_restart() {
    // Parallel CLI/signature spawns can briefly inherit another test's lock
    // between fork and exec. Exercise the documented retryable acquisition
    // failure without retrying authority, database or receipt-validation errors.
    fn acknowledge_worker_update_package(project: &Path, attempt: &str, ack: &herdr_farm::store::UpdatePackageAck) -> anyhow::Result<herdr_farm::store::WorkerPackageAckReceipt> {
        retry_acquisition(|| herdr_farm::memory::acknowledge_worker_update_package(project, attempt, ack))
    }
    fn acknowledge_memory_update(project: &Path, ack: &MemoryUpdateAck) -> anyhow::Result<MemoryUpdateReceipt> {
        retry_acquisition(|| herdr_farm::memory::acknowledge_memory_update(project, ack))
    }
    let (tmp, project, key) = fixture();
    let cli = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_herdr-farm"))
            .env_clear()
            .env("HOME", tmp.path())
            .env("PATH", "/usr/bin:/bin")
            .args(["--root", tmp.path().to_str().unwrap(), "memory", "project"])
            .args(args)
            .output()
            .unwrap()
    };
    import(&project);
    let head = runtime::snapshot(&project).unwrap().head;
    runtime::add_task(
        &project,
        TaskId::new("consumer").unwrap(),
        "Consume changes".into(),
        head,
    )
    .unwrap();
    let mut memory = MemoryStore::from_sqlite(
        migration::open_active(&project).unwrap(),
        project.join(".state/objects"),
    );
    let snap = memory
        .create_task_snapshot(
            SnapshotRequest {
                schema_version: 1,
                task_id: "consumer".into(),
                profile: "worker".into(),
                domains: vec![],
                paths: vec![],
                pinned_keys: vec![],
                sensitivity: "default".into(),
            },
            "worker",
            &"a".repeat(64),
            None,
            32_000,
            "Instructions",
            1,
            None,
        )
        .unwrap();
    // An unconsumed newer snapshot must not attract obligations for this attempt.
    memory
        .create_task_snapshot(
            SnapshotRequest {
                schema_version: 1,
                task_id: "consumer".into(),
                profile: "worker".into(),
                domains: vec![],
                paths: vec![],
                pinned_keys: vec![],
                sensitivity: "default".into(),
            },
            "worker",
            &"a".repeat(64),
            None,
            32_000,
            "Unused newer instructions",
            1,
            None,
        )
        .unwrap();
    let mut db = migration::open_active(&project).unwrap();
    let state = db.read_snapshot(None).unwrap();
    let mut task = state
        .tasks
        .iter()
        .find(|t| t.id.as_str() == "consumer")
        .unwrap()
        .clone();
    let previous = task.revision;
    task.revision += 1;
    task.active_attempt = Some(AttemptId::new("consumer-attempt").unwrap());
    db.commit(Commit {
        expected_head: state.head,
        mutations: vec![
            Mutation::Attempt {
                expected: None,
                next: Attempt {
                    id: AttemptId::new("consumer-attempt").unwrap(),
                    task: task.id.clone(),
                    revision: 1,
                    state: AttemptState::Running,
                    snapshot: Some(snap.id.as_str().into()),
                    reservation: "consumer-reservation".into(),
                    termination_observed: false,
                },
            },
            Mutation::Task {
                expected: Some(previous),
                next: task,
            },
        ],
    })
    .unwrap();
    // Publish through the real signed owner review service.
    let publish = |text: &str, revision: u64| {
        fs::write(project.join("MEMORY.md"), text).unwrap();
        let candidate = import_file(&project, &project.join("MEMORY.md"), Some(revision)).unwrap();
        let doc = decision(&project, &candidate, "approve");
        let path = tmp.path().join(format!("review-{revision}.json"));
        let sig = sign(
            &key,
            &path,
            &serde_json::to_vec(&doc).unwrap(),
            authority::MEMORY_IMPORT_REVIEW_NAMESPACE,
        );
        authority::review_memory_import(&project, &path, &sig, doc.expected_head).unwrap();
        candidate
    };
    let candidate = publish("First approved update", 1);
    assert_eq!(db.memory_delivery_intents().unwrap().len(), 1);
    let delivery = db
        .memory_delivery_intents()
        .unwrap()
        .into_iter()
        .find(|r| r["task_id"] == "consumer")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let head = runtime::snapshot(&project).unwrap().head;
    let pulled = read_memory_update(&project, &delivery, "consumer-attempt").unwrap();
    assert_eq!(pulled["body"], "First approved update");
    assert_eq!(runtime::snapshot(&project).unwrap().head, head);
    assert!(
        db.memory_update_receipts("consumer-attempt")
            .unwrap()
            .is_empty()
    );
    assert!(read_memory_update(&project, &delivery, "other-attempt").is_err());
    let update: MemoryUpdate = serde_json::from_value(pulled["update"].clone()).unwrap();
    let package = read_snapshot_update_package(&project, &update.input_snapshot_id).unwrap().package;
    let mut package_ack = herdr_farm::store::UpdatePackageAck {
        schema_version: 1, package_id: package.package_id, manifest_hash: package.manifest_hash,
        change_ids: package.change_ids, disposition: "seen".into(),
    };
    assert!(acknowledge_worker_update_package(&project, "consumer-attempt", &package_ack).is_err());
    assert!(acknowledge_worker_update_package(&project, "other-attempt", &package_ack).is_err());
    let mut ack = MemoryUpdateAck {
        schema_version: 1,
        delivery_id: delivery.clone(),
        attempt_id: "consumer-attempt".into(),
        manifest_hash: update.manifest_hash,
        state: "applied".into(),
    };
    assert!(acknowledge_memory_update(&project, &ack).is_err());
    ack.state = "seen".into();
    let mut wrong = ack.clone();
    wrong.manifest_hash = "0".repeat(64);
    assert!(acknowledge_memory_update(&project, &wrong).is_err());
    let fault = rusqlite::Connection::open(project.join(".state/state.db")).unwrap();
    fault.execute_batch("CREATE TRIGGER fail_receipt BEFORE INSERT ON memory_update_receipts BEGIN SELECT RAISE(ABORT,'injected receipt failure'); END;").unwrap();
    let before_failure = runtime::snapshot(&project).unwrap().head;
    assert!(acknowledge_memory_update(&project, &ack).is_err());
    assert_eq!(runtime::snapshot(&project).unwrap().head, before_failure);
    assert!(
        db.memory_update_receipts("consumer-attempt")
            .unwrap()
            .is_empty()
    );
    fault.execute_batch("DROP TRIGGER fail_receipt;").unwrap();
    let output = cli(&[
        "update",
        "--delivery",
        &delivery,
        "--attempt",
        "consumer-attempt",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        pulled
    );
    let ack_file = tmp.path().join("ack.json");
    fs::write(&ack_file, serde_json::to_vec(&ack).unwrap()).unwrap();
    let output = cli(&["ack", "--input", ack_file.to_str().unwrap()]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let seen: MemoryUpdateReceipt = serde_json::from_slice(&output.stdout).unwrap();
    let package_file = tmp.path().join("package-ack.json");
    fs::write(&package_file, serde_json::to_vec(&package_ack).unwrap()).unwrap();
    fault.execute_batch("CREATE TRIGGER fail_package_receipt BEFORE INSERT ON memory_change_receipts BEGIN SELECT RAISE(ABORT,'injected package receipt failure'); END;").unwrap();
    let package_head = runtime::snapshot(&project).unwrap().head;
    assert!(acknowledge_worker_update_package(&project, "consumer-attempt", &package_ack).is_err());
    assert_eq!(runtime::snapshot(&project).unwrap().head, package_head);
    fault.execute_batch("DROP TRIGGER fail_package_receipt;").unwrap();
    fault.execute_batch("CREATE TRIGGER fail_worker_package_evidence BEFORE INSERT ON worker_package_acknowledgments BEGIN SELECT RAISE(ABORT,'injected evidence failure'); END;").unwrap();
    assert!(acknowledge_worker_update_package(&project, "consumer-attempt", &package_ack).is_err());
    assert_eq!(runtime::snapshot(&project).unwrap().head, package_head);
    assert_eq!(fault.query_row("SELECT count(*) FROM memory_change_receipts", [], |row| row.get::<_,u64>(0)).unwrap(), 0);
    assert_eq!(fault.query_row("SELECT count(*) FROM worker_package_acknowledgments", [], |row| row.get::<_,u64>(0)).unwrap(), 0);
    fault.execute_batch("DROP TRIGGER fail_worker_package_evidence;").unwrap();
    let package_output = cli(&["package-ack", "--attempt", "consumer-attempt", "--input", package_file.to_str().unwrap()]);
    assert!(package_output.status.success(), "{}", String::from_utf8_lossy(&package_output.stderr));
    let package_response: serde_json::Value = serde_json::from_slice(&package_output.stdout).unwrap();
    assert_eq!(package_response["schema_version"], 1);
    let evidence_sequence = package_response["evidence_sequence"].as_u64().unwrap();
    assert!(evidence_sequence > package_response["receipt"]["sequence"].as_u64().unwrap());
    let evidence: String = fault.query_row("SELECT payload FROM events WHERE sequence=?1", [evidence_sequence], |row| row.get(0)).unwrap();
    let evidence: serde_json::Value = serde_json::from_str(&evidence).unwrap();
    assert_eq!(evidence["protocol"], "worker-change-declaration-v1");
    assert_eq!(evidence["attempt_id"], "consumer-attempt");
    assert_eq!(evidence["receipts"], serde_json::json!([{"change_id":delivery,"manifest_hash":ack.manifest_hash,"sequence":seen.sequence}]));
    let after_package = runtime::snapshot(&project).unwrap().head;
    assert_eq!(cli(&["package-ack", "--attempt", "consumer-attempt", "--input", package_file.to_str().unwrap()]).stdout, package_output.stdout);
    assert_eq!(runtime::snapshot(&project).unwrap().head, after_package);
    package_ack.disposition = "applied".into();
    assert!(acknowledge_worker_update_package(&project, "consumer-attempt", &package_ack).is_err());
    let output = cli(&["receipts", "--attempt", "consumer-attempt"]);
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<Vec<MemoryUpdateReceipt>>(&output.stdout).unwrap(),
        vec![seen.clone()]
    );
    drop(db);
    assert_eq!(acknowledge_memory_update(&project, &ack).unwrap(), seen);
    ack.state = "applied".into();
    let applied = acknowledge_memory_update(&project, &ack).unwrap();
    assert_eq!(acknowledge_memory_update(&project, &ack).unwrap(), applied);
    fs::write(&package_file, serde_json::to_vec(&package_ack).unwrap()).unwrap();
    let package_output = cli(&["package-ack", "--attempt", "consumer-attempt", "--input", package_file.to_str().unwrap()]);
    assert!(package_output.status.success(), "{}", String::from_utf8_lossy(&package_output.stderr));
    assert_eq!(cli(&["package-ack", "--attempt", "consumer-attempt", "--input", package_file.to_str().unwrap()]).stdout, package_output.stdout);
    let mut db = migration::open_active(&project).unwrap();
    assert_eq!(
        db.memory_update_receipts("consumer-attempt").unwrap().len(),
        2
    );
    // A receipt is a declaration, never validation evidence or invalidation resolution.
    let raw = rusqlite::Connection::open(project.join(".state/state.db")).unwrap();
    let unresolved: i64 = raw.query_row("SELECT count(*) FROM memory_invalidations WHERE task_id='consumer' AND resolved_seq IS NULL", [], |r|r.get(0)).unwrap();
    assert_eq!(unresolved, 1);
    let before_completion = db.read_snapshot(None).unwrap();
    let output = cli(&["readiness", "--task", "consumer"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let readiness: MemoryReadiness = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(readiness.head, before_completion.head);
    assert!(
        readiness
            .blockers
            .iter()
            .any(|b| b.kind == "unresolved_invalidation")
    );
    let mut completed = before_completion
        .tasks
        .iter()
        .find(|t| t.id.as_str() == "consumer")
        .unwrap()
        .clone();
    let old_revision = completed.revision;
    completed.revision += 1;
    completed.state = TaskState::Succeeded;
    completed.active_attempt = None;
    assert!(
        db.commit(Commit {
            expected_head: before_completion.head,
            mutations: vec![Mutation::Task {
                expected: Some(old_revision),
                next: completed
            }]
        })
        .is_err()
    );
    assert_eq!(db.read_snapshot(None).unwrap(), before_completion);

    assert!(
        raw.execute("DELETE FROM memory_update_receipts", [])
            .is_err()
    );

    let invalidations = db.memory_invalidations("consumer").unwrap();
    let reference = MemoryInvalidationReference {
        id: invalidations[0]["id"].as_str().unwrap().into(),
        task_id: "consumer".into(),
        triggering_seq: invalidations[0]["triggering_seq"].as_u64().unwrap(),
    };
    let mut reconciliation = MemoryReconciliation {
        version: 1,
        id: "owner-resolution".into(),
        project_store: project
            .join(".state/state.db")
            .canonicalize()
            .unwrap()
            .display()
            .to_string(),
        authority: authority::policy_reference(&project).unwrap(),
        expected_head: runtime::snapshot(&project).unwrap().head,
        expires_unix_ms: jiff::Timestamp::now().as_millisecond() + 60000,
        reason: "Owner reviewed the applied change and reconciled this conflict".into(),
        invalidations: vec![reference],
    };
    let resolution_file = tmp.path().join("resolution.json");
    let wrong_sig = sign(
        &key,
        &resolution_file,
        &serde_json::to_vec(&reconciliation).unwrap(),
        authority::MEMORY_IMPORT_REVIEW_NAMESPACE,
    );
    assert!(
        authority::reconcile_memory(
            &project,
            &resolution_file,
            &wrong_sig,
            reconciliation.expected_head
        )
        .is_err()
    );
    assert!(db.memory_invalidations("consumer").unwrap()[0]["resolved_seq"].is_null());
    // One incorrect item rolls back the entire owner-reviewed list.
    reconciliation.expected_head = runtime::snapshot(&project).unwrap().head;
    reconciliation
        .invalidations
        .push(MemoryInvalidationReference {
            id: "missing".into(),
            task_id: "consumer".into(),
            triggering_seq: 1,
        });
    let bad_sig = sign(
        &key,
        &resolution_file,
        &serde_json::to_vec(&reconciliation).unwrap(),
        authority::MEMORY_RECONCILE_NAMESPACE,
    );
    assert!(
        authority::reconcile_memory(
            &project,
            &resolution_file,
            &bad_sig,
            reconciliation.expected_head
        )
        .is_err()
    );
    assert!(db.memory_invalidations("consumer").unwrap()[0]["resolved_seq"].is_null());
    reconciliation.invalidations.pop();
    reconciliation.expected_head = runtime::snapshot(&project).unwrap().head;
    let resolution_sig = sign(
        &key,
        &resolution_file,
        &serde_json::to_vec(&reconciliation).unwrap(),
        authority::MEMORY_RECONCILE_NAMESPACE,
    );
    let body_path = project
        .join(".state/objects/sha256")
        .join(&candidate.body_hash.as_str()[..2])
        .join(candidate.body_hash.as_str());
    fs::write(&body_path, "corrupt before reconciliation").unwrap();
    assert!(
        authority::reconcile_memory(
            &project,
            &resolution_file,
            &resolution_sig,
            reconciliation.expected_head
        )
        .is_err()
    );
    assert!(db.memory_invalidations("consumer").unwrap()[0]["resolved_seq"].is_null());
    fs::write(&body_path, "First approved update").unwrap();
    reconciliation.expected_head = runtime::snapshot(&project).unwrap().head;
    let resolution_sig = sign(
        &key,
        &resolution_file,
        &serde_json::to_vec(&reconciliation).unwrap(),
        authority::MEMORY_RECONCILE_NAMESPACE,
    );
    let output = cli(&[
        "reconcile",
        resolution_file.to_str().unwrap(),
        resolution_sig.to_str().unwrap(),
        "--expected-head",
        &reconciliation.expected_head.to_string(),
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let resolved: MemoryReconciliationReceipt = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        db.memory_readiness("consumer", jiff::Timestamp::now().as_millisecond())
            .unwrap()
            .blockers
            .is_empty()
    );
    assert_eq!(
        authority::reconcile_memory(
            &project,
            &resolution_file,
            &resolution_sig,
            reconciliation.expected_head
        )
        .unwrap(),
        resolved
    );
    publish("Second approved update", 2);
    let second = db
        .memory_delivery_intents()
        .unwrap()
        .into_iter()
        .find(|r| r["revision"] == 3 && r["task_id"] == "consumer")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let second_update: MemoryUpdate = serde_json::from_value(
        read_memory_update(&project, &second, "consumer-attempt").unwrap()["update"].clone(),
    )
    .unwrap();
    let mut second_ack = MemoryUpdateAck {
        delivery_id: second,
        manifest_hash: second_update.manifest_hash,
        state: "seen".into(),
        ..ack.clone()
    };
    acknowledge_memory_update(&project, &second_ack).unwrap();
    // Superseding this update makes a first applied declaration invalid.
    publish("Third approved update", 3);
    let pending = read_snapshot_update_package(&project, snap.id.as_str()).unwrap().package;
    assert_eq!(pending.change_ids.len(), 2);
    let partial_package = herdr_farm::store::UpdatePackageAck {
        schema_version: 1, package_id: pending.package_id, manifest_hash: pending.manifest_hash,
        change_ids: pending.change_ids, disposition: "seen".into(),
    };
    // One exact receipt cannot speak for another member of the same package.
    assert!(acknowledge_worker_update_package(&project, "consumer-attempt", &partial_package).is_err());
    second_ack.state = "applied".into();
    assert!(acknowledge_memory_update(&project, &second_ack).is_err());
    assert_eq!(acknowledge_memory_update(&project, &ack).unwrap(), applied);
    assert_eq!(
        db.memory_update_receipts("consumer-attempt").unwrap().len(),
        3
    );

    // The current revision can be packaged independently of an obsolete one.
    // First see the full package, exercising reuse of an existing logical seen
    // receipt without moving its source or invalidating its package evidence.
    let third = db.memory_delivery_intents().unwrap().into_iter()
        .find(|r| r["revision"] == 4 && r["task_id"] == "consumer").unwrap()["id"]
        .as_str().unwrap().to_owned();
    let third_update: MemoryUpdate = serde_json::from_value(
        read_memory_update(&project, &third, "consumer-attempt").unwrap()["update"].clone(),
    ).unwrap();
    let mut third_ack = MemoryUpdateAck {
        delivery_id: third.clone(), manifest_hash: third_update.manifest_hash,
        state: "seen".into(), ..ack.clone()
    };
    acknowledge_memory_update(&project, &third_ack).unwrap();
    let wide_seen = acknowledge_worker_update_package(&project, "consumer-attempt", &partial_package).unwrap();
    let mut wide_applied = partial_package.clone();
    wide_applied.disposition = "applied".into();
    assert!(acknowledge_worker_update_package(&project, "consumer-attempt", &wide_applied).is_err());
    let output = cli(&["package", "--snapshot", snap.id.as_str(), "--change", &third]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let selected: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(selected["package"]["change_ids"], serde_json::json!([third]));
    assert_eq!(selected["members"][0]["revision"], 4);
    let selected_again = cli(&["package", "--binding", selected["package"]["binding_id"].as_str().unwrap(), "--change", &third]);
    assert!(selected_again.status.success());
    assert_eq!(selected_again.stdout, output.stdout);
    let mut selected_ack = herdr_farm::store::UpdatePackageAck {
        schema_version: 1,
        package_id: selected["package"]["package_id"].as_str().unwrap().into(),
        manifest_hash: selected["package"]["manifest_hash"].as_str().unwrap().into(),
        change_ids: vec![third.clone()], disposition: "seen".into(),
    };
    let selected_seen = acknowledge_worker_update_package(&project, "consumer-attempt", &selected_ack).unwrap();
    third_ack.state = "applied".into();
    acknowledge_memory_update(&project, &third_ack).unwrap();
    selected_ack.disposition = "applied".into();
    let selected_applied = acknowledge_worker_update_package(&project, "consumer-attempt", &selected_ack).unwrap();
    assert_eq!(acknowledge_worker_update_package(&project, "consumer-attempt", &selected_ack).unwrap(), selected_applied);
    selected_ack.disposition = "seen".into();
    assert_eq!(acknowledge_worker_update_package(&project, "consumer-attempt", &selected_ack).unwrap(), selected_seen);
    assert_eq!(acknowledge_worker_update_package(&project, "consumer-attempt", &partial_package).unwrap(), wide_seen);
    assert_eq!(read_snapshot_update_package(&project, snap.id.as_str()).unwrap().package.change_ids, vec![second_ack.delivery_id.clone()]);
    assert!(!cli(&["package", "--snapshot", snap.id.as_str(), "--change", &third]).status.success());
    assert_eq!(raw.query_row("SELECT count(*) FROM memory_change_receipts WHERE change_id=?1 AND disposition='applied'", [&second_ack.delivery_id], |r| r.get::<_,u64>(0)).unwrap(), 0);
    assert!(raw.execute("UPDATE memory_change_receipts SET package_id=package_id WHERE disposition='seen'", []).is_err());

    assert_eq!(
        authority::reconcile_memory(
            &project,
            &resolution_file,
            &resolution_sig,
            reconciliation.expected_head
        )
        .unwrap(),
        resolved
    );
    assert_eq!(
        db.memory_invalidations("consumer")
            .unwrap()
            .iter()
            .filter(|i| i["resolved_seq"].is_null())
            .count(),
        2
    );
    // Corrupt content is refused before a receipt can be created.
    let object = project
        .join(".state/objects/sha256")
        .join(&candidate.body_hash.as_str()[..2])
        .join(candidate.body_hash.as_str());
    fs::write(&object, "corrupt").unwrap();
    assert!(read_memory_update(&project, &delivery, "consumer-attempt").is_err());
    assert!(acknowledge_memory_update(&project, &ack).is_err());
    assert!(acknowledge_worker_update_package(&project, "consumer-attempt", &package_ack).is_err());
    fs::write(&object, "First approved update").unwrap();
    // A fresh attempt may share the starting snapshot, but old declarations cannot
    // speak for it. Replacing the task pointer fences even an idempotent old ack.
    let state = db.read_snapshot(None).unwrap();
    let mut task = state
        .tasks
        .iter()
        .find(|t| t.id.as_str() == "consumer")
        .unwrap()
        .clone();
    let revision = task.revision;
    task.revision += 1;
    task.active_attempt = Some(AttemptId::new("replacement").unwrap());
    db.commit(Commit {
        expected_head: state.head,
        mutations: vec![
            Mutation::Attempt {
                expected: None,
                next: Attempt {
                    id: AttemptId::new("replacement").unwrap(),
                    task: task.id.clone(),
                    revision: 1,
                    state: AttemptState::Running,
                    snapshot: Some(snap.id.as_str().into()),
                    reservation: "replacement-reservation".into(),
                    termination_observed: false,
                },
            },
            Mutation::Task {
                expected: Some(revision),
                next: task,
            },
        ],
    })
    .unwrap();
    assert!(acknowledge_memory_update(&project, &ack).is_err());
    let mut impersonated = ack.clone();
    impersonated.attempt_id = "replacement".into();
    assert!(acknowledge_memory_update(&project, &impersonated).is_err());
    assert!(acknowledge_worker_update_package(&project, "consumer-attempt", &package_ack).is_err());
    assert!(acknowledge_worker_update_package(&project, "replacement", &package_ack).is_err());
    assert!(db.memory_update_receipts("replacement").unwrap().is_empty());
    assert_eq!(raw.query_row("SELECT count(*) FROM worker_package_acknowledgments WHERE attempt_id='consumer-attempt'", [], |row| row.get::<_,u64>(0)).unwrap(), 5);
    assert!(raw.execute("UPDATE worker_package_acknowledgments SET attempt_id='replacement'", []).is_err());
    assert!(raw.execute("DELETE FROM worker_package_acknowledgments", []).is_err());
    let retained: String = raw.query_row("SELECT payload FROM events WHERE sequence=?1", [evidence_sequence], |row| row.get(0)).unwrap();
    assert_eq!(serde_json::from_str::<serde_json::Value>(&retained).unwrap(), evidence);
}

#[test]
fn schema23_upgrade_adds_empty_receipts_without_inventing_consumption() {
    let (_tmp, project, _) = fixture();
    let path = project.join(".state/state.db");
    let raw = rusqlite::Connection::open(&path).unwrap();
    test_schema::historical(&raw, 23).unwrap();
    let mut db = herdr_farm::store::SqliteStore::open(&path).unwrap();
    let before = db.read_snapshot(None).unwrap();
    assert!(db.memory_update_receipts("attempt").is_err());
    db.upgrade_v1().unwrap();
    assert!(db.memory_update_receipts("attempt").unwrap().is_empty());
    let after = db.read_snapshot(None).unwrap();
    assert_eq!(after.events, before.events);
    assert_eq!(after.tasks, before.tasks);
    assert_eq!(after.schema_version, herdr_farm::store::SCHEMA);
    db.integrity_check().unwrap();
}

#[test]
fn optional_update_supersession_requires_exact_applied_replacement_and_preserves_history() {
    let (tmp, project, key) = fixture();
    let first_digest = proposal(&project);
    let mut db = migration::open_active(&project).unwrap();
    let state = db.read_snapshot(None).unwrap();
    let mut task = state.tasks.iter().find(|t| t.id.as_str() == "task-proposal").unwrap().clone();
    let prior = task.revision;
    task.revision += 1;
    task.active_attempt = Some(AttemptId::new("attempt-a").unwrap());
    db.commit(Commit {expected_head:state.head,mutations:vec![Mutation::Task {expected:Some(prior),next:task}]}).unwrap();
    let snapshot = state.attempts.iter().find(|a| a.id.as_str() == "attempt-a").unwrap().snapshot.clone().unwrap();
    let approve = |proposal_id: &str, digest: &str| {
        let doc = MemoryReviewAuthorization {
            version:1,project_store:project.join(".state/state.db").canonicalize().unwrap().display().to_string(),
            authority:authority::policy_reference(&project).unwrap(),expected_head:runtime::snapshot(&project).unwrap().head,
            expires_unix_ms:jiff::Timestamp::now().as_millisecond()+60_000,proposal_digest:digest.into(),record_keys:vec!["ui.claim".into()],
            review:ReviewDocument {schema_version:1,proposal_id:proposal_id.into(),decision:"approve".into(),reason:"Reviewed optional observation".into()},
            read_set_version:None,read_set:None,
        };
        let path=tmp.path().join(format!("{proposal_id}-review.json"));
        let signature=sign(&key,&path,&serde_json::to_vec(&doc).unwrap(),authority::MEMORY_REVIEW_NAMESPACE);
        let review=authority::review_memory_proposal(&project,proposal_id,&path,&signature,doc.expected_head).unwrap();
        retry_acquisition(|| authority::promote_memory_proposal(&project,proposal_id,&review.id)).unwrap();
    };
    approve("proposal-a",&first_digest);
    let old=db.memory_delivery_intents().unwrap().into_iter().find(|d| d["cause_id"]=="proposal-a").unwrap();
    assert_eq!(old["severity"],"informational");
    let record=old["record_id"].as_str().unwrap().to_owned();
    let publish = |number:u64,impact:&str| {
        let mut memory=MemoryStore::from_sqlite(migration::open_active(&project).unwrap(),project.join(".state/objects"));
        let body=memory.ingest_object(format!("optional revision {number}").as_bytes()).unwrap();
        let proposal_id=format!("optional-{number}");
        let doc=ProposalDocument {
            schema_version:1,proposal_id:proposal_id.clone(),producer:ProposalProducer {task_id:"task-proposal".into(),attempt_id:"attempt-a".into()},
            input_snapshot_id:snapshot.clone(),observed_revisions:vec![],repository:None,
            changes:vec![ProposalChange {
                record_key:"ui.claim".into(),expected:Some(ObservedRevision {record_id:record.clone(),revision:number-1}),kind:"observation".into(),
                scope:Applicability {domains:vec!["ui".into()],paths:vec![]},claim:"Reviewed newer observation".into(),body_object:body.as_str().into(),
                evidence:vec![],based_on:vec![],impact:impact.into(),
            }],
        };
        let proposed=memory.propose(&serde_json::to_vec(&doc).unwrap(),jiff::Timestamp::now().as_millisecond()).unwrap();
        assert_eq!(proposed.validation,"accepted","{}",proposed.reason);
        approve(&proposal_id,&proposed.payload_digest);
        let mut store=migration::open_active(&project).unwrap();
        let delivery=store.memory_delivery_intents().unwrap().into_iter().find(|d| d["cause_id"]==proposal_id).unwrap();
        serde_json::from_value::<MemoryUpdate>(read_memory_update(&project,delivery["id"].as_str().unwrap(),"attempt-a").unwrap()["update"].clone()).unwrap()
    };
    let newer=publish(2,"informational");
    let package=read_snapshot_update_package(&project,&snapshot).unwrap().package;
    let request=herdr_farm::store::WorkerUpdateSupersession {
        schema_version:1,binding_id:package.binding_id.clone(),change_id:old["id"].as_str().unwrap().into(),
        replacement_change_id:newer.delivery_id.clone(),replacement_manifest_hash:newer.manifest_hash.clone(),reason:"Replaced by the reviewed current observation".into(),
    };
    assert!(supersede_worker_update(&project,"attempt-a",&request).is_err());
    let mut applied=MemoryUpdateAck {schema_version:1,delivery_id:newer.delivery_id.clone(),attempt_id:"attempt-a".into(),manifest_hash:newer.manifest_hash.clone(),state:"seen".into()};
    acknowledge_memory_update(&project,&applied).unwrap();
    assert!(supersede_worker_update(&project,"attempt-a",&request).is_err());
    applied.state="applied".into();
    let source=acknowledge_memory_update(&project,&applied).unwrap();
    let before=runtime::snapshot(&project).unwrap();
    let invalidations_before=db.memory_invalidations("task-proposal").unwrap();
    for bad in [
        herdr_farm::store::WorkerUpdateSupersession {binding_id:"f".repeat(64),..request.clone()},
        herdr_farm::store::WorkerUpdateSupersession {replacement_manifest_hash:"0".repeat(64),..request.clone()},
        herdr_farm::store::WorkerUpdateSupersession {replacement_change_id:request.change_id.clone(),..request.clone()},
        herdr_farm::store::WorkerUpdateSupersession {change_id:"unknown".into(),..request.clone()},
    ] { assert!(supersede_worker_update(&project,"attempt-a",&bad).is_err()); }
    assert!(supersede_worker_update(&project,"other-attempt",&request).is_err());
    assert_eq!(runtime::snapshot(&project).unwrap().head,before.head);
    let raw=rusqlite::Connection::open(project.join(".state/state.db")).unwrap();
    // Previously applied evidence is not enough for a new supersession after
    // expiry, revocation, validity loss, head movement or hard classification.
    for (mutate,restore) in [
        ("UPDATE memory_validity SET expiry_unix_ms=1 WHERE record_id=?1 AND revision=2", "UPDATE memory_validity SET expiry_unix_ms=NULL WHERE record_id=?1 AND revision=2"),
        ("UPDATE memory_validity SET state='blocked' WHERE record_id=?1 AND revision=2", "UPDATE memory_validity SET state='valid' WHERE record_id=?1 AND revision=2"),
        ("UPDATE memory_heads SET status='revoked' WHERE record_id=?1", "UPDATE memory_heads SET status='active' WHERE record_id=?1"),
        ("UPDATE memory_heads SET revision=1 WHERE record_id=?1", "UPDATE memory_heads SET revision=2 WHERE record_id=?1"),
        ("UPDATE memory_records SET is_hard=1 WHERE id=?1", "UPDATE memory_records SET is_hard=0 WHERE id=?1"),
        ("UPDATE memory_records SET kind='constraint' WHERE id=?1", "UPDATE memory_records SET kind='observation' WHERE id=?1"),
    ] {
        raw.execute(mutate,[&record]).unwrap();
        assert!(supersede_worker_update(&project,"attempt-a",&request).is_err(),"{mutate}");
        assert_eq!(runtime::snapshot(&project).unwrap().head,before.head);
        raw.execute(restore,[&record]).unwrap();
    }
    raw.execute_batch("CREATE TRIGGER fail_supersession BEFORE INSERT ON memory_update_supersessions BEGIN SELECT RAISE(ABORT,'injected supersession failure'); END;").unwrap();
    assert!(supersede_worker_update(&project,"attempt-a",&request).is_err());
    assert_eq!(runtime::snapshot(&project).unwrap().head,before.head);
    raw.execute_batch("DROP TRIGGER fail_supersession;").unwrap();
    let object=project.join(".state/objects/sha256").join(&newer.body_hash.as_str()[..2]).join(newer.body_hash.as_str());
    let original=fs::read(&object).unwrap();
    fs::write(&object,"corrupt replacement").unwrap();
    assert!(supersede_worker_update(&project,"attempt-a",&request).is_err());
    assert_eq!(runtime::snapshot(&project).unwrap().head,before.head);
    fs::write(&object,original).unwrap();
    // Upgrade a genuine pre-supersession schema and retain its exact receipts;
    // migration must not infer retirement from an already-applied replacement.
    test_schema::historical(&raw,42).unwrap();
    let mut upgraded=herdr_farm::store::SqliteStore::open(&project.join(".state/state.db")).unwrap();
    assert!(upgraded.worker_update_supersession(&package.binding_id,&request.change_id).is_err());
    upgraded.upgrade_v1().unwrap();
    assert_eq!(raw.query_row("SELECT count(*) FROM memory_update_supersessions",[],|r|r.get::<_,u64>(0)).unwrap(),0);
    assert_eq!(upgraded.read_snapshot(None).unwrap().head,before.head);
    assert_eq!(acknowledge_memory_update(&project,&applied).unwrap(),source);
    assert_eq!(read_snapshot_update_package(&project,&snapshot).unwrap().package,package);
    let path=tmp.path().join("supersession.json");
    fs::write(&path,serde_json::to_vec(&request).unwrap()).unwrap();
    let cli=|args:&[&str]| Command::new(env!("CARGO_BIN_EXE_herdr-farm")).env_clear().env("HOME",tmp.path()).env("PATH","/usr/bin:/bin")
        .args(["--root",tmp.path().to_str().unwrap(),"memory","project"]).args(args).output().unwrap();
    let output=cli(&["supersede-update","--attempt","attempt-a","--input",path.to_str().unwrap()]);
    assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stderr));
    let receipt:herdr_farm::store::WorkerUpdateSupersessionReceipt=serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt.replacement_receipt_sequence,source.sequence);
    assert_eq!(receipt.request,request);
    assert!(receipt.sequence>source.sequence);
    let head=runtime::snapshot(&project).unwrap().head;
    assert_eq!(cli(&["supersede-update","--attempt","attempt-a","--input",path.to_str().unwrap()]).stdout,output.stdout);
    assert_eq!(cli(&["supersession","--binding",&package.binding_id,"--change",&request.change_id]).stdout,output.stdout);
    assert_eq!(runtime::snapshot(&project).unwrap().head,head);
    assert_eq!(runtime::snapshot(&project).unwrap().attempts,before.attempts);
    assert_eq!(db.memory_invalidations("task-proposal").unwrap(),invalidations_before);
    assert_eq!(read_snapshot_update_package(&project,&snapshot).unwrap().package.change_ids,vec![newer.delivery_id.clone()]);
    assert!(read_selected_snapshot_update_package(&project,&snapshot,&[request.change_id.clone()]).is_err());
    assert!(!db.applied_cursor_covers(&package.binding_id,&request.change_id).unwrap());
    assert_eq!(raw.query_row("SELECT count(*) FROM memory_update_receipts WHERE delivery_id=?1",[&request.change_id],|r|r.get::<_,u64>(0)).unwrap(),0);
    assert_eq!(raw.query_row("SELECT count(*) FROM consumer_binding_obligations WHERE binding_id=?1",[&package.binding_id],|r|r.get::<_,u64>(0)).unwrap(),2);
    assert!(raw.execute("UPDATE memory_update_supersessions SET sequence=sequence",[]).is_err());
    assert!(raw.execute("DELETE FROM memory_update_supersessions",[]).is_err());
    assert!(supersede_worker_update(&project,"attempt-a",&herdr_farm::store::WorkerUpdateSupersession {reason:"Different decision".into(),..request.clone()}).is_err());
    let next=publish(3,"stop_at_checkpoint");
    // The original accepted decision remains historical after further updates.
    assert_eq!(supersede_worker_update(&project,"attempt-a",&request).unwrap(),receipt);
    let latest=publish(4,"informational");
    let mut latest_ack=MemoryUpdateAck {delivery_id:latest.delivery_id.clone(),manifest_hash:latest.manifest_hash.clone(),state:"seen".into(),..applied.clone()};
    acknowledge_memory_update(&project,&latest_ack).unwrap();
    latest_ack.state="applied".into();
    acknowledge_memory_update(&project,&latest_ack).unwrap();
    let mandatory=herdr_farm::store::WorkerUpdateSupersession {change_id:next.delivery_id,replacement_change_id:latest.delivery_id,replacement_manifest_hash:latest.manifest_hash,..request.clone()};
    let blockers=db.memory_readiness("task-proposal",jiff::Timestamp::now().as_millisecond()).unwrap().blockers;
    assert!(supersede_worker_update(&project,"attempt-a",&mandatory).is_err());
    assert_eq!(db.memory_readiness("task-proposal",jiff::Timestamp::now().as_millisecond()).unwrap().blockers,blockers);
    assert!(!blockers.is_empty());
    let state=db.read_snapshot(None).unwrap();
    let mut task=state.tasks.iter().find(|t|t.id.as_str()=="task-proposal").unwrap().clone();
    let revision=task.revision;task.revision+=1;task.active_attempt=None;
    db.commit(Commit {expected_head:state.head,mutations:vec![Mutation::Task {expected:Some(revision),next:task}]}).unwrap();
    assert!(supersede_worker_update(&project,"attempt-a",&request).is_err());
    assert_eq!(cli(&["supersession","--binding",&package.binding_id,"--change",&request.change_id]).stdout,output.stdout);
}

#[cfg(feature = "state-store")]
#[path = "../src/store/test_schema.rs"]
mod test_schema;

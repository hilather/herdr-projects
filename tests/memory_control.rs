#![cfg(feature = "state-store")]
use herdr_projects::{authority, domain::*, memory::*, migration, runtime};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

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
    db.execute_batch("DROP TABLE IF EXISTS native_profiles; DROP TABLE IF EXISTS memory_update_receipts; DROP TABLE memory_delivery_intents; DROP TABLE memory_import_decisions; DROP TABLE memory_import_candidates; DROP TABLE memory_snapshot_inputs; UPDATE store_meta SET schema_version=22; PRAGMA user_version=22;").unwrap();
    drop(db);
    let mut db = herdr_projects::store::SqliteStore::open(&path).unwrap();
    let before = db.read_snapshot(None).unwrap();
    db.upgrade_v1().unwrap();
    let after = db.read_snapshot(None).unwrap();
    assert_eq!(before.events, after.events);
    assert_eq!(after.schema_version, 25);
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
        Command::new(env!("CARGO_BIN_EXE_herdr-projects"))
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
    let (tmp, project, key) = fixture();
    let cli = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_herdr-projects"))
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
    second_ack.state = "applied".into();
    assert!(acknowledge_memory_update(&project, &second_ack).is_err());
    assert_eq!(acknowledge_memory_update(&project, &ack).unwrap(), applied);
    assert_eq!(
        db.memory_update_receipts("consumer-attempt").unwrap().len(),
        3
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
    assert!(db.memory_update_receipts("replacement").unwrap().is_empty());
}

#[test]
fn schema23_upgrade_adds_empty_receipts_without_inventing_consumption() {
    let (_tmp, project, _) = fixture();
    let path = project.join(".state/state.db");
    let raw = rusqlite::Connection::open(&path).unwrap();
    raw.execute_batch("DROP TABLE IF EXISTS native_profiles; DROP TABLE memory_update_receipts; UPDATE store_meta SET schema_version=23; PRAGMA user_version=23;").unwrap();
    let mut db = herdr_projects::store::SqliteStore::open(&path).unwrap();
    let before = db.read_snapshot(None).unwrap();
    assert!(db.memory_update_receipts("attempt").is_err());
    db.upgrade_v1().unwrap();
    assert!(db.memory_update_receipts("attempt").unwrap().is_empty());
    let after = db.read_snapshot(None).unwrap();
    assert_eq!(after.events, before.events);
    assert_eq!(after.tasks, before.tasks);
    assert_eq!(after.schema_version, 25);
    db.integrity_check().unwrap();
}

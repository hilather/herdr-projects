#![cfg(feature = "state-store")]
//! Regression coverage derived from the 2026-09-20 independent memory audit.
use crate::execution_guard::GatedSpawn;
use crate as herdr_farm;
use herdr_farm::{domain::*, memory::*, store::SqliteStore};
use std::{fs, path::Path};

fn fixture() -> (tempfile::TempDir, MemoryStore) {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("state.db");
    let mut db = SqliteStore::create(&path).unwrap();
    db.commit(Commit {
        expected_head: 0,
        mutations: vec![Mutation::Task {
            expected: None,
            next: Task {
                id: TaskId::new("task-a").unwrap(),
                revision: 1,
                state: TaskState::Draft,
                title: "A".into(),
                active_attempt: None,
            },
        }],
    })
    .unwrap();
    let memory = MemoryStore::from_sqlite(db, tmp.path().join("objects"));
    (tmp, memory)
}
fn request() -> SnapshotRequest {
    SnapshotRequest {
        schema_version: 1,
        task_id: "task-a".into(),
        profile: "worker".into(),
        domains: vec![],
        paths: vec![],
        pinned_keys: vec![],
        sensitivity: "default".into(),
    }
}
fn snap(memory: &mut MemoryStore, now: i64, cap: u64) -> Result<MemorySnapshot, MemoryError> {
    memory.create_task_snapshot(
        request(),
        "worker",
        &"a".repeat(64),
        None,
        cap,
        "instructions",
        now,
        None,
    )
}
fn put(
    memory: &mut MemoryStore,
    id: &str,
    kind: MemoryKind,
    body: &[u8],
    expiry: Option<i64>,
) -> ObjectId {
    let hash = memory.ingest_object(body).unwrap();
    memory
        .insert_revision(
            &ControlContext { now_unix_ms: 1 },
            NewRevision {
                id: MemoryRecordId::new(id).unwrap(),
                record_key: id.into(),
                scope_id: "project".into(),
                kind,
                body_hash: hash.clone(),
                provenance_hash: hash.clone(),
                applicability: Applicability {
                    domains: vec![],
                    paths: vec![],
                },
                dependencies: vec![],
                expected: None,
                expiry_unix_ms: expiry,
                validity_state: "valid".into(),
                validity_reason: "test".into(),
            },
        )
        .unwrap();
    hash
}
fn proposal(memory: &mut MemoryStore, root: &Path, expected: Option<ObservedRevision>) -> String {
    let mut db = SqliteStore::open(&root.join("state.db")).unwrap();
    let head = db.read_snapshot(None).unwrap().head;
    db.commit(Commit {
        expected_head: head,
        mutations: vec![Mutation::Attempt {
            expected: None,
            next: Attempt {
                id: AttemptId::new("attempt-a").unwrap(),
                task: TaskId::new("task-a").unwrap(),
                revision: 1,
                state: AttemptState::Running,
                snapshot: None,
                reservation: "held".into(),
                termination_observed: false,
            },
        }],
    })
    .unwrap();
    let snapshot = snap(memory, 1, 32_000).unwrap();
    let state = db.read_snapshot(None).unwrap();
    let mut attempt = state.attempts[0].clone();
    attempt.revision += 1;
    attempt.snapshot = Some(snapshot.id.as_str().into());
    db.commit(Commit {
        expected_head: state.head,
        mutations: vec![Mutation::Attempt {
            expected: Some(1),
            next: attempt,
        }],
    })
    .unwrap();
    let body = memory.ingest_object(&b"new proposed body"[..]).unwrap();
    let bytes = serde_json::to_vec(&serde_json::json!({
        "schema_version":1,"proposal_id":"proposal-a",
        "producer":{"task_id":"task-a","attempt_id":"attempt-a"},
        "input_snapshot_id":snapshot.id.as_str(),
        "changes":[{"record_key":"fact","expected":expected,"kind":"observation",
            "scope":{"domains":[],"paths":[]},"claim":"new claim", "body_object":body.as_str(),
            "evidence":[],"based_on":[],"impact":"informational"}]
    }))
    .unwrap();
    assert_eq!(memory.propose(&bytes, 1).unwrap().validation, "accepted");
    body.as_str().into()
}
fn approve(memory: &mut MemoryStore) -> ReviewDecision {
    memory.review(br#"{"schema_version":1,"proposal_id":"proposal-a","decision":"approve","reason":"reviewed"}"#, 2).unwrap()
}

#[test]
fn mandatory_body_must_count_towards_snapshot_budget() {
    let (_tmp, mut memory) = fixture();
    put(
        &mut memory,
        "rule",
        MemoryKind::Constraint,
        &vec![b'x'; 8_000],
        None,
    );
    assert!(
        matches!(
            snap(&mut memory, 1, 1_000),
            Err(MemoryError::RequiredContentTooLarge { .. })
        ),
        "8,000-character mandatory body was admitted under a 1,000-character cap"
    );
}
#[test]
fn snapshot_cache_must_not_reintroduce_expired_facts() {
    let (_tmp, mut memory) = fixture();
    put(
        &mut memory,
        "fact",
        MemoryKind::Observation,
        b"expires",
        Some(10),
    );
    assert_eq!(snap(&mut memory, 1, 32_000).unwrap().entries.len(), 1);
    assert!(
        snap(&mut memory, 11, 32_000).unwrap().entries.is_empty(),
        "cached expired fact returned"
    );
}
fn project_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    fs::create_dir(&project).unwrap();
    for child in [".state", "threads", "inbox"] {
        fs::create_dir(project.join(child)).unwrap();
    }
    fs::write(
        project.join("PROJECT.md"),
        "+++\nname='Demo'\n+++\nInstructions\n",
    )
    .unwrap();
    fs::write(project.join("TASKS.md"), "").unwrap();
    fs::write(project.join("MEMORY.md"), "Legacy memory").unwrap();
    fs::write(
        project.join(".state/project.json"),
        r#"{"status":"paused"}"#,
    )
    .unwrap();
    let plan = herdr_farm::migration::inspect(&project).unwrap();
    herdr_farm::migration::apply(&project, &plan, true).unwrap();
    (temp, project)
}
#[test]
fn coordinator_must_refuse_missing_mandatory_bytes() {
    let (_tmp, project) = project_fixture();
    let mut memory = MemoryStore::from_sqlite(
        herdr_farm::migration::open_active(&project).unwrap(),
        project.join(".state/objects"),
    );
    let hash = put(
        &mut memory,
        "rule",
        MemoryKind::Constraint,
        b"MANDATORY RULE",
        None,
    );
    fs::remove_file(
        project
            .join(".state/objects/sha256")
            .join(&hash.as_str()[..2])
            .join(hash.as_str()),
    )
    .unwrap();
    let profile = CheckpointProfile {
        name: "planner".into(),
        digest: "a".repeat(64),
        config_digest: None,
        budget_chars: 32_000,
    };
    assert!(
        coordinator_context(&project, "session", &profile, "").is_err(),
        "context succeeded with missing mandatory rule bytes"
    );
}
#[test]
fn corrupt_object_bytes_must_not_be_rendered_as_verified_memory() {
    let (_tmp, project) = project_fixture();
    let mut memory = MemoryStore::from_sqlite(
        herdr_farm::migration::open_active(&project).unwrap(),
        project.join(".state/objects"),
    );
    let hash = put(
        &mut memory,
        "rule",
        MemoryKind::Constraint,
        b"MANDATORY RULE",
        None,
    );
    fs::write(
        project
            .join(".state/objects/sha256")
            .join(&hash.as_str()[..2])
            .join(hash.as_str()),
        "CORRUPTED",
    )
    .unwrap();
    let profile = CheckpointProfile {
        name: "planner".into(),
        digest: "a".repeat(64),
        config_digest: None,
        budget_chars: 32_000,
    };
    assert!(
        coordinator_context(&project, "session", &profile, "").is_err(),
        "context accepted corrupted bytes under the original hash"
    );
}
#[test]
fn coordinator_output_must_fit_profile_budget() {
    let (_tmp, project) = project_fixture();
    let mut memory = MemoryStore::from_sqlite(
        herdr_farm::migration::open_active(&project).unwrap(),
        project.join(".state/objects"),
    );
    put(
        &mut memory,
        "rule",
        MemoryKind::Constraint,
        &vec![b'x'; 8_000],
        None,
    );
    let profile = CheckpointProfile {
        name: "planner".into(),
        digest: "a".repeat(64),
        config_digest: None,
        budget_chars: 1_000,
    };
    match coordinator_context(&project, "session", &profile, "") {
        Err(_) => (),
        Ok(context) => assert!(
            context.text.chars().count() <= 1_000,
            "oversized context: {} chars",
            context.text.chars().count()
        ),
    }
}

#[test]
fn abandoned_gc_claim_must_be_recoverable_after_reopen() {
    let (tmp, mut memory) = fixture();
    let hash = memory.ingest_object(&b"orphan"[..]).unwrap();
    let mut db = SqliteStore::open(&tmp.path().join("state.db")).unwrap();
    let claims = db.claim_gc(1).unwrap();
    assert_eq!(claims[0].0, hash.as_str());
    assert!(db.begin_gc_delete(&claims[0].0, claims[0].1).unwrap());
    drop(db);
    drop(memory);
    let mut reopened = MemoryStore::from_sqlite(
        SqliteStore::open(&tmp.path().join("state.db")).unwrap(),
        tmp.path().join("objects"),
    );
    assert_eq!(
        reopened.collect_unreferenced().unwrap(),
        1,
        "stranded gc_deleting object never reconciled"
    );
}

#[test]
fn cutover_must_verify_object_bytes_before_publishing_authority() {
    use crate as herdr_farm;
    use herdr_farm::{authority, migration, runtime};
    use sha2::{Digest, Sha256};
    use std::process::Command;
    let temp = tempfile::tempdir().unwrap();
    let key = temp.path().join("owner");
    assert!(
        Command::new("/usr/bin/ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key)
            .status_gated()
            .unwrap()
            .success()
    );
    let public = fs::read_to_string(key.with_extension("pub"))
        .unwrap()
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    let config = temp.path().join("config.toml");
    fs::write(
        &config,
        format!("[authority]\nversion=1\nrevision=1\napproval_public_key={public:?}\n"),
    )
    .unwrap();
    let project = temp.path().join("project");
    fs::create_dir(&project).unwrap();
    for child in [".state", "threads", "inbox"] {
        fs::create_dir(project.join(child)).unwrap();
    }
    fs::write(project.join("PROJECT.md"), "+++\nname='Demo'\n+++\n").unwrap();
    fs::write(project.join("TASKS.md"), "").unwrap();
    fs::write(project.join("MEMORY.md"), "remember").unwrap();
    fs::write(
        project.join(".state/project.json"),
        r#"{"status":"paused"}"#,
    )
    .unwrap();
    let runtime_plan = migration::inspect_with_config(&project, &config).unwrap();
    migration::apply(&project, &runtime_plan, true).unwrap();
    let before = runtime::snapshot(&project).unwrap();
    runtime::set_state(
        &project,
        before.head,
        before.control.unwrap().revision,
        ProjectState::Active,
        &config,
    )
    .unwrap();
    let plan = plan(&project).unwrap();
    let imports = import_plan(&project, &plan).unwrap();
    let hash = &imports[0].body_hash;
    fs::remove_file(
        project
            .join(".state/objects/sha256")
            .join(&hash[..2])
            .join(hash),
    )
    .unwrap();
    let head = runtime::snapshot(&project).unwrap().head;
    let policy_json = format!(
        "{{\"version\":1,\"revision\":1,\"approval_public_key\":{}}}",
        serde_json::to_string(&public).unwrap()
    );
    let policy = MemoryPolicy {
        version: 1,
        revision: 1,
        project_store: project.join(".state/state.db").display().to_string(),
        authority: VersionedReference {
            id: "owner-approval-policy".into(),
            revision: 1,
            digest: format!("{:x}", Sha256::digest(policy_json.as_bytes())),
        },
        expected_head: head,
        op: MemoryPolicyOp::Cutover,
        record_key: None,
        memory_plan_digest: Some(plan.digest.clone()),
        expected_memory_owner: Some("legacy-markdown".into()),
    };
    let doc = temp.path().join("policy.json");
    let plan_file = temp.path().join("plan.json");
    fs::write(&doc, serde_json::to_vec(&policy).unwrap()).unwrap();
    fs::write(&plan_file, serde_json::to_vec(&plan).unwrap()).unwrap();
    assert!(
        Command::new("/usr/bin/ssh-keygen")
            .args(["-Y", "sign", "-f"])
            .arg(&key)
            .args(["-n", authority::MEMORY_SIGNATURE_NAMESPACE])
            .arg(&doc)
            .output_gated()
            .unwrap()
            .status
            .success()
    );
    let result = authority::cutover_memory(
        &project,
        &plan_file,
        &doc,
        &temp.path().join("policy.json.sig"),
        head,
        true,
    );
    assert!(result.is_err(), "missing body must refuse cutover");
    assert_eq!(
        migration::read_format(&project).unwrap().memory,
        "legacy-markdown",
        "failed cutover published SQLite authority before verifying imported objects: {result:?}"
    );
}

#[test]
fn supported_object_body_must_not_be_silently_truncated_on_render() {
    let (_tmp, project) = project_fixture();
    let mut db = herdr_farm::migration::open_active(&project).unwrap();
    let head = db.read_snapshot(None).unwrap().head;
    db.commit(Commit {
        expected_head: head,
        mutations: vec![Mutation::Task {
            expected: None,
            next: Task {
                id: TaskId::new("task-a").unwrap(),
                revision: 1,
                state: TaskState::Draft,
                title: "A".into(),
                active_attempt: None,
            },
        }],
    })
    .unwrap();
    let mut memory = MemoryStore::from_sqlite(db, project.join(".state/objects"));
    put(
        &mut memory,
        "rule",
        MemoryKind::Constraint,
        &vec![b'x'; 100_000],
        None,
    );
    snap(&mut memory, 1, 1_000_000).unwrap();
    let (index, files) = load_brief_memory(&project, "worker", 1_000_000).unwrap();
    let text = format!(
        "{index}{}",
        files
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<String>()
    );
    assert!(
        text.contains(&"x".repeat(100_000)),
        "render returned success with a truncated object body"
    );
}

#[test]
fn import_plan_must_bind_the_supplied_inventory_to_its_digest() {
    let (_tmp, project) = project_fixture();
    let mut plan = plan(&project).unwrap();
    assert!(!plan.sources.is_empty());
    plan.sources.clear();
    assert!(
        import_plan(&project, &plan).is_err(),
        "altered empty inventory accepted under the original nonempty plan digest"
    );
}

#[test]
fn ingest_refuses_corrupt_existing_destination() {
    let (tmp, mut memory) = fixture();
    let id = memory.ingest_object(&b"original"[..]).unwrap();
    fs::write(
        tmp.path()
            .join("objects/sha256")
            .join(&id.as_str()[..2])
            .join(id.as_str()),
        b"tampered",
    )
    .unwrap();
    assert!(memory.ingest_object(&b"original"[..]).is_err());
}

#[test]
fn concurrent_ingests_in_one_process_do_not_share_staging_files() {
    let (tmp, _memory) = fixture();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    std::thread::scope(|scope| {
        let mut jobs = Vec::new();
        for i in 0..8 {
            let root = tmp.path().to_owned();
            let barrier = barrier.clone();
            jobs.push(scope.spawn(move || {
                let mut memory = MemoryStore::from_sqlite(
                    SqliteStore::open(&root.join("state.db")).unwrap(),
                    root.join("objects"),
                );
                let bytes = vec![i as u8; 100_000];
                barrier.wait();
                let id = memory.ingest_object(bytes.as_slice()).unwrap();
                let path = root
                    .join("objects/sha256")
                    .join(&id.as_str()[..2])
                    .join(id.as_str());
                assert_eq!(fs::read(path).unwrap(), bytes);
            }));
        }
        for job in jobs {
            job.join().unwrap();
        }
    });
}

#[test]
fn collection_failure_does_not_certify_purge() {
    let (tmp, mut memory) = fixture();
    let id = memory.ingest_object(&b"orphan"[..]).unwrap();
    let path = tmp
        .path()
        .join("objects/sha256")
        .join(&id.as_str()[..2])
        .join(id.as_str());
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(memory.collect_unreferenced().is_err());
    let raw = rusqlite::Connection::open(tmp.path().join("state.db")).unwrap();
    let state: String = raw
        .query_row(
            "SELECT availability FROM objects WHERE hash=?1",
            [id.as_str()],
            |r| r.get(0),
        )
        .unwrap();
    assert_ne!(state, "purged");
    fs::remove_dir(&path).unwrap();
    assert_eq!(memory.collect_unreferenced().unwrap(), 1);
}

#[test]
fn promotion_rechecks_expiry_without_an_intervening_event() {
    let (tmp, mut memory) = fixture();
    put(
        &mut memory,
        "fact",
        MemoryKind::Observation,
        b"old",
        Some(10),
    );
    proposal(
        &mut memory,
        tmp.path(),
        Some(ObservedRevision {
            record_id: "fact".into(),
            revision: 1,
        }),
    );
    let decision = approve(&mut memory);
    assert!(memory.promote("proposal-a", &decision.id, 11).is_err());
    let mut db = SqliteStore::open(&tmp.path().join("state.db")).unwrap();
    assert_eq!(db.memory_head("fact").unwrap().unwrap().revision, 1);
    assert!(db.memory_promotion("proposal-a").unwrap().is_none());
}

#[test]
fn concurrent_revision_reference_and_collection_never_lose_committed_bytes() {
    let (tmp, mut memory) = fixture();
    for n in 0..20 {
        let bytes = format!("racing body {n}");
        let id = memory.ingest_object(bytes.as_bytes()).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let result = std::thread::scope(|scope| {
            let root = tmp.path().to_owned();
            let collector_barrier = barrier.clone();
            let collector = scope.spawn(move || {
                let mut collector = MemoryStore::from_sqlite(
                    SqliteStore::open(&root.join("state.db")).unwrap(),
                    root.join("objects"),
                );
                collector_barrier.wait();
                collector.collect_unreferenced().unwrap();
            });
            barrier.wait();
            let result = memory.insert_revision(
                &ControlContext { now_unix_ms: 1 },
                NewRevision {
                    id: MemoryRecordId::new(format!("race-{n}")).unwrap(),
                    record_key: format!("race-{n}"),
                    scope_id: "project".into(),
                    kind: MemoryKind::Observation,
                    body_hash: id.clone(),
                    provenance_hash: id.clone(),
                    applicability: Applicability {
                        domains: vec![],
                        paths: vec![],
                    },
                    dependencies: vec![],
                    expected: None,
                    expiry_unix_ms: None,
                    validity_state: "valid".into(),
                    validity_reason: "test".into(),
                },
            );
            collector.join().unwrap();
            result
        });
        let mut db = SqliteStore::open(&tmp.path().join("state.db")).unwrap();
        if result.is_ok() {
            assert!(db.memory_head(&format!("race-{n}")).unwrap().is_some());
            assert_eq!(
                fs::read(
                    tmp.path()
                        .join("objects/sha256")
                        .join(&id.as_str()[..2])
                        .join(id.as_str())
                )
                .unwrap(),
                bytes.as_bytes()
            );
        } else {
            assert!(matches!(
                result,
                Err(MemoryError::EvidenceUnavailable { .. })
            ));
            assert!(db.memory_head(&format!("race-{n}")).unwrap().is_none());
        }
    }
}

#[test]
fn hard_memory_kind_is_mandatory_and_expiry_cannot_silently_remove_it() {
    let (_tmp,mut memory)=fixture();
    put(&mut memory,"hard-kind",MemoryKind::HardMemory,b"Required rule",Some(10));
    let snapshot=snap(&mut memory,1,32000).unwrap();
    assert_eq!(snapshot.entries.len(),1);
    assert_eq!(snapshot.entries[0].role,"mandatory");
    assert!(snap(&mut memory,10,32000).is_err());
}

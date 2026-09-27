//! v2 promotion fence. Compared in the same write transaction as the publish.
use super::*;
use crate::domain::{MemoryReadSet, ReadSetHead, ReadSetValidity, ScopeCatalogGeneration};
use rusqlite::OptionalExtension;

const SCHEMA_VERSION: u32 = 38;
// Same ceiling as the mandatory inventory. Past this, fail closed: a truncated
// scan would hide a phantom insert.
const READ_SET_LIMIT: usize = 10_000;

fn invalid(message: &str) -> StoreError {
    StoreError::Invalid(message.into())
}

fn fits_i64(value: u64) -> bool {
    i64::try_from(value).is_ok()
}

fn non_negative(value: i64, label: &str) -> Result<u64> {
    u64::try_from(value).map_err(|_| StoreError::Corrupt(format!("{label} is corrupt")))
}

fn grant_id_ok(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn validate_signed(signed: &MemoryReadSet) -> Result<()> {
    if let Some(id) = signed.reviewer_grant_id.as_deref() {
        if !grant_id_ok(id) {
            return Err(invalid("invalid reviewer grant id"));
        }
    }
    if !fits_i64(signed.revocation_epoch)
        || !fits_i64(signed.policy_revision)
        || !fits_i64(signed.required_set_generation)
    {
        return Err(invalid("read set generation exceeds SQLite integer range"));
    }
    for head in &signed.record_heads {
        if head.record_id.is_empty() || head.record_id.len() > 128 || !fits_i64(head.revision) {
            return Err(invalid("invalid read set record head"));
        }
    }
    for row in &signed.validity_revisions {
        if row.record_id.is_empty() || row.record_id.len() > 128 || !fits_i64(row.revision) {
            return Err(invalid("invalid read set validity revision"));
        }
    }
    for scope in &signed.scope_catalog_generations {
        if scope.scope_id.is_empty() || scope.scope_id.len() > 128 || !fits_i64(scope.generation) {
            return Err(invalid("invalid read set scope catalog"));
        }
    }
    Ok(())
}

fn revocation_epoch(db: &Connection, grant_id: Option<&str>) -> Result<u64> {
    let Some(id) = grant_id else {
        return Ok(0);
    };
    let epoch: Option<i64> = db
        .query_row(
            "SELECT revocation_epoch FROM delegation_grants WHERE id=?1",
            [id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(epoch) = epoch else {
        return Err(StoreError::Conflict);
    };
    let epoch = non_negative(epoch, "delegation revocation epoch")?;
    let revoked: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM delegation_revocations WHERE grant_id=?1)",
        [id],
        |row| row.get(0),
    )?;
    if revoked {
        // The grant row is immutable, so revocation has to move the fence.
        return epoch
            .checked_add(1)
            .filter(|value| fits_i64(*value))
            .ok_or_else(|| invalid("revocation epoch exhausted"));
    }
    Ok(epoch)
}

/// Live fence for `reviewer_grant_id`. The grant does not authorize promotion.
pub(super) fn current(db: &Connection, reviewer_grant_id: Option<&str>) -> Result<MemoryReadSet> {
    check_schema(db)?;
    let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchema(version));
    }
    if let Some(id) = reviewer_grant_id {
        if !grant_id_ok(id) {
            return Err(invalid("invalid reviewer grant id"));
        }
    }
    let mut heads = db.prepare(
        "SELECT r.id, h.revision FROM memory_records r
         JOIN memory_heads h ON h.record_id = r.id
         WHERE h.status = 'active'
           AND (r.is_hard = 1 OR r.kind IN ('constraint', 'hard_memory', 'contract'))
         ORDER BY r.id",
    )?;
    let record_heads = heads
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if record_heads.len() > READ_SET_LIMIT {
        return Err(StoreError::Limit(
            "memory read set exceeds 10000 records".into(),
        ));
    }
    let record_heads = record_heads
        .into_iter()
        .map(|(record_id, revision)| {
            Ok(ReadSetHead {
                record_id,
                revision: non_negative(revision, "memory head revision")?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let mut validity = db.prepare(
        "SELECT v.record_id, v.revision FROM memory_validity v
         JOIN memory_heads h ON h.record_id = v.record_id AND h.revision = v.revision
         JOIN memory_records r ON r.id = v.record_id
         WHERE h.status = 'active'
           AND (r.is_hard = 1 OR r.kind IN ('constraint', 'hard_memory', 'contract'))
         ORDER BY v.record_id",
    )?;
    let validity_revisions = validity
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if validity_revisions.len() > READ_SET_LIMIT {
        return Err(StoreError::Limit(
            "memory read set exceeds 10000 validity rows".into(),
        ));
    }
    let validity_revisions = validity_revisions
        .into_iter()
        .map(|(record_id, revision)| {
            Ok(ReadSetValidity {
                record_id,
                revision: non_negative(revision, "memory validity revision")?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let mut scopes =
        db.prepare("SELECT scope_id, generation FROM memory_scope_catalog ORDER BY scope_id")?;
    let scope_catalog_generations = scopes
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if scope_catalog_generations.len() > READ_SET_LIMIT {
        return Err(StoreError::Limit(
            "memory read set exceeds 10000 scope catalogs".into(),
        ));
    }
    let scope_catalog_generations = scope_catalog_generations
        .into_iter()
        .map(|(scope_id, generation)| {
            Ok(ScopeCatalogGeneration {
                scope_id,
                generation: non_negative(generation, "scope catalog generation")?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let required_set_generation: i64 = db.query_row(
        "SELECT generation FROM memory_required_generation WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    let policy_revision: i64 = db.query_row(
        "SELECT COALESCE(MAX(revision), 0) FROM memory_policies",
        [],
        |row| row.get(0),
    )?;
    Ok(MemoryReadSet {
        record_heads,
        validity_revisions,
        reviewer_grant_id: reviewer_grant_id.map(str::to_owned),
        revocation_epoch: revocation_epoch(db, reviewer_grant_id)?,
        policy_revision: non_negative(policy_revision, "memory policy revision")?,
        required_set_generation: non_negative(required_set_generation, "required set generation")?,
        scope_catalog_generations,
    })
}

pub(super) fn require_match(db: &Connection, signed: &MemoryReadSet) -> Result<()> {
    validate_signed(signed)?;
    if &current(db, signed.reviewer_grant_id.as_deref())? != signed {
        return Err(StoreError::Conflict);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{MemoryError, MemoryStore};

    fn user_version(db: &Connection) -> u32 {
        db.query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }

    fn generation(db: &Connection) -> i64 {
        db.query_row(
            "SELECT generation FROM memory_required_generation WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn setup() -> (tempfile::TempDir, MemoryStore, String) {
        let root = tempfile::tempdir().unwrap();
        let db = root.path().join("state.db");
        let mut store = SqliteStore::create(&db).unwrap();
        let task = Task {
            id: TaskId::new("task-api").unwrap(),
            revision: 1,
            state: TaskState::Draft,
            title: "api".into(),
            active_attempt: None,
        };
        store
            .commit(Commit {
                expected_head: 0,
                mutations: vec![Mutation::Task {
                    expected: None,
                    next: task,
                }],
            })
            .unwrap();
        let attempt = Attempt {
            id: AttemptId::new("att-api-2").unwrap(),
            task: TaskId::new("task-api").unwrap(),
            revision: 1,
            state: AttemptState::Running,
            snapshot: None,
            reservation: "held".into(),
            termination_observed: false,
        };
        let head = store.read_snapshot(None).unwrap().head;
        store
            .commit(Commit {
                expected_head: head,
                mutations: vec![Mutation::Attempt {
                    expected: None,
                    next: attempt,
                }],
            })
            .unwrap();
        let mut memory = MemoryStore::from_sqlite(store, db.parent().unwrap().join("objects"));
        let body = memory.ingest_object(&b"claim-body"[..]).unwrap();
        let req = SnapshotRequest {
            schema_version: 1,
            task_id: "task-api".into(),
            profile: "implementation".into(),
            domains: vec![],
            paths: vec![],
            pinned_keys: vec![],
            sensitivity: "default".into(),
        };
        let snap = memory
            .create_task_snapshot(
                req,
                "implementation",
                &"a".repeat(64),
                None,
                32_000,
                "instructions",
                1_000,
                None,
            )
            .unwrap();
        let state = memory.store.read_snapshot(None).unwrap();
        let mut attempt = state.attempts[0].clone();
        attempt.revision += 1;
        attempt.snapshot = Some(snap.id.as_str().into());
        memory
            .store
            .commit(Commit {
                expected_head: state.head,
                mutations: vec![Mutation::Attempt {
                    expected: Some(1),
                    next: attempt,
                }],
            })
            .unwrap();
        let doc = ProposalDocument {
            schema_version: 1,
            proposal_id: "mp-api-errors-01".into(),
            producer: ProposalProducer {
                task_id: "task-api".into(),
                attempt_id: "att-api-2".into(),
            },
            input_snapshot_id: snap.id.as_str().into(),
            observed_revisions: vec![],
            repository: None,
            changes: vec![ProposalChange {
                record_key: "api.error-envelope".into(),
                expected: None,
                kind: "observation".into(),
                scope: Applicability {
                    domains: vec!["api".into()],
                    paths: vec!["src/api".into()],
                },
                claim: "Validation errors contain code, message, and request_id.".into(),
                body_object: format!("sha256:{}", body.as_str()),
                evidence: vec![],
                based_on: vec![],
                impact: "reconcile_before_completion".into(),
            }],
        };
        let receipt = memory
            .propose(&serde_json::to_vec(&doc).unwrap(), 1_000)
            .unwrap();
        (root, memory, receipt.payload_digest)
    }

    fn auth(
        store: &mut SqliteStore,
        digest: &str,
        read_set_version: Option<u32>,
        grant_id: Option<&str>,
    ) -> PreparedMemoryReview {
        let head = store.read_snapshot(None).unwrap().head;
        let read_set = if read_set_version == Some(2) {
            Some(current(&store.connection, grant_id).unwrap())
        } else {
            None
        };
        PreparedMemoryReview {
            document: MemoryReviewAuthorization {
                version: 1,
                project_store: "/tmp/project-store".into(),
                authority: VersionedReference {
                    id: "owner-approval-policy".into(),
                    revision: 1,
                    digest: "ab".repeat(32),
                },
                expected_head: head,
                expires_unix_ms: 9_000_000_000_000,
                proposal_digest: digest.into(),
                record_keys: vec!["api.error-envelope".into()],
                review: ReviewDocument {
                    schema_version: 1,
                    proposal_id: "mp-api-errors-01".into(),
                    decision: "approve".into(),
                    reason: "evidence supports the claim".into(),
                },
                read_set_version,
                read_set,
            },
            config_digest: None,
        }
    }

    fn review_bytes() -> Vec<u8> {
        serde_json::to_vec(&ReviewDocument {
            schema_version: 1,
            proposal_id: "mp-api-errors-01".into(),
            decision: "approve".into(),
            reason: "evidence supports the claim".into(),
        })
        .unwrap()
    }

    fn reviewed(
        memory: &mut MemoryStore,
        digest: &str,
        read_set_version: Option<u32>,
        grant_id: Option<&str>,
    ) -> (ReviewDecision, PreparedMemoryReview) {
        let prepared = auth(&mut memory.store, digest, read_set_version, grant_id);
        let decision = memory
            .review_checked(&review_bytes(), 1_000, Some(&prepared))
            .unwrap();
        (decision, prepared)
    }

    fn put(memory: &mut MemoryStore, id: &str, key: &str, kind: MemoryKind) {
        let body = memory.ingest_object(key.as_bytes()).unwrap();
        memory
            .insert_revision(
                &ControlContext { now_unix_ms: 2_000 },
                NewRevision {
                    id: MemoryRecordId::new(id).unwrap(),
                    record_key: key.into(),
                    scope_id: "project".into(),
                    kind,
                    body_hash: body.clone(),
                    provenance_hash: body,
                    applicability: Applicability {
                        domains: vec!["api".into()],
                        paths: vec!["src/api".into()],
                    },
                    dependencies: vec![],
                    expected: None,
                    expiry_unix_ms: None,
                    validity_state: "valid".into(),
                    validity_reason: "control_insert".into(),
                },
            )
            .unwrap();
    }

    fn unrelated_event(memory: &MemoryStore) {
        memory
            .store
            .connection
            .execute(
                "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('fixture','unrelated',1,1,'{}')",
                [],
            )
            .unwrap();
    }

    fn insert_grant(memory: &MemoryStore) -> String {
        let id = "cd".repeat(32);
        memory
            .store
            .connection
            .execute(
                "INSERT INTO delegation_grants(id,raw_bytes,raw_digest,project_store,issuer,subject,subject_public_key,action_classes,repositories,profile_kinds,max_concurrent_attempts,expires_unix_ms,revocation_epoch,child_delegation,policy_revision,authority_digest,installed_unix_ms) VALUES(?1,?2,?1,'/tmp/project-store','owner','delegate','ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFakeKeyForReadSetGrant0000','[\"reserve_attempt\"]','[{\"repository\":\"/tmp/repo\",\"ref\":\"refs/heads/factory\"}]','[\"codex\"]',1,9000000000000,4,'forbidden',1,?3,1000)",
                params![id, b"{}", "ef".repeat(32)],
            )
            .unwrap();
        id
    }


    #[test]
    fn unrelated_event_allows_v2_promotion_and_replay_returns_the_stored_row() {
        let (_root, mut memory, digest) = setup();
        let (decision, prepared) = reviewed(&mut memory, &digest, Some(2), None);
        let before = generation(&memory.store.connection);
        unrelated_event(&memory);
        put(
            &mut memory,
            "note-optional",
            "note.optional",
            MemoryKind::Observation,
        );
        assert_eq!(generation(&memory.store.connection), before);
        assert!(memory.store.read_snapshot(None).unwrap().head > decision_event_fence(&decision));
        let first = memory
            .promote_checked("mp-api-errors-01", &decision.id, 1_000, Some(&prepared))
            .unwrap();
        assert!(!first.reused);
        unrelated_event(&memory);
        let replay = memory
            .promote_checked("mp-api-errors-01", &decision.id, 1_000, Some(&prepared))
            .unwrap();
        assert!(replay.reused);
        assert_eq!(replay.sequence, first.sequence);
        let stored: i64 = memory
            .store
            .connection
            .query_row(
                "SELECT sequence FROM memory_promotions WHERE proposal_id='mp-api-errors-01'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(replay.sequence, stored as u64);
        assert!(memory.store.read_snapshot(None).unwrap().head > replay.sequence);
    }

    fn decision_event_fence(decision: &ReviewDecision) -> u64 {
        let reviewed: serde_json::Value = serde_json::from_str(&decision.reviewed_heads).unwrap();
        reviewed["event_head"].as_u64().unwrap() + 1
    }

    #[test]
    fn new_constraint_blocks_v2_promotion() {
        let (_root, mut memory, digest) = setup();
        let (decision, prepared) = reviewed(&mut memory, &digest, Some(2), None);
        let before = generation(&memory.store.connection);
        put(
            &mut memory,
            "rule-constraint",
            "rule.constraint",
            MemoryKind::Constraint,
        );
        assert!(generation(&memory.store.connection) > before);
        let error = memory
            .promote_checked("mp-api-errors-01", &decision.id, 1_000, Some(&prepared))
            .unwrap_err();
        assert!(matches!(error, MemoryError::RevisionConflict { .. }));
        assert!(memory
            .store
            .memory_promotion("mp-api-errors-01")
            .unwrap()
            .is_none());
        assert!(memory
            .store
            .memory_record_by_key("rule.constraint")
            .unwrap()
            .is_some());
    }

    #[test]
    fn phantom_contract_conflicts_and_is_not_skipped() {
        let (_root, mut memory, digest) = setup();
        let (decision, prepared) = reviewed(&mut memory, &digest, Some(2), None);
        let before = generation(&memory.store.connection);
        put(
            &mut memory,
            "api-contract",
            "api.contract",
            MemoryKind::Contract,
        );
        assert_eq!(generation(&memory.store.connection), before);
        let error = memory
            .promote_checked("mp-api-errors-01", &decision.id, 1_000, Some(&prepared))
            .unwrap_err();
        assert!(matches!(error, MemoryError::RevisionConflict { .. }));
        assert!(memory
            .store
            .memory_promotion("mp-api-errors-01")
            .unwrap()
            .is_none());
        let contract = memory
            .store
            .memory_record_by_key("api.contract")
            .unwrap()
            .unwrap();
        assert_eq!(contract.kind, MemoryKind::Contract);
        assert!(!contract.is_hard);
    }

    #[test]
    fn v1_and_other_versions_still_conflict_on_an_intervening_event() {
        for version in [None, Some(1), Some(3)] {
            let (_root, mut memory, digest) = setup();
            let (decision, prepared) = reviewed(&mut memory, &digest, version, None);
            unrelated_event(&memory);
            let error = memory
                .promote_checked("mp-api-errors-01", &decision.id, 1_000, Some(&prepared))
                .unwrap_err();
            assert!(
                matches!(error, MemoryError::RevisionConflict { .. }),
                "{version:?} {error:?}"
            );
            assert!(memory
                .store
                .memory_promotion("mp-api-errors-01")
                .unwrap()
                .is_none());
        }
    }

    #[test]
    fn reviewer_grant_in_the_read_set_does_not_let_a_worker_promote_a_hard_change() {
        let (_root, mut memory, digest) = setup();
        put(
            &mut memory,
            "rule-constraint",
            "rule.constraint",
            MemoryKind::Constraint,
        );
        let grant = insert_grant(&memory);
        let (decision, prepared) = reviewed(&mut memory, &digest, Some(2), Some(&grant));
        assert_eq!(
            prepared
                .document
                .read_set
                .as_ref()
                .unwrap()
                .reviewer_grant_id
                .as_deref(),
            Some(grant.as_str())
        );
        assert_eq!(
            prepared
                .document
                .read_set
                .as_ref()
                .unwrap()
                .revocation_epoch,
            4
        );
        let error = memory
            .store
            .promote_reviewed_proposal(
                "mp-api-errors-01",
                &decision,
                &[NewRevision {
                    id: MemoryRecordId::new("rule-constraint").unwrap(),
                    record_key: "rule.constraint".into(),
                    scope_id: "project".into(),
                    kind: MemoryKind::Constraint,
                    body_hash: ObjectId::from_hex("ab".repeat(32)).unwrap(),
                    provenance_hash: ObjectId::from_hex("ab".repeat(32)).unwrap(),
                    applicability: Applicability {
                        domains: vec!["api".into()],
                        paths: vec![],
                    },
                    dependencies: vec![],
                    expected: Some(1),
                    expiry_unix_ms: None,
                    validity_state: "valid".into(),
                    validity_reason: "promoted".into(),
                }],
                &[],
                1_000,
                Some(&prepared),
            )
            .unwrap_err();
        assert!(
            matches!(error, StoreError::Invalid(ref message) if message.contains("mandatory")),
            "{error:?}"
        );
        assert_eq!(
            memory
                .store
                .memory_head("rule-constraint")
                .unwrap()
                .unwrap()
                .revision,
            1
        );
        assert!(memory
            .store
            .memory_promotion("mp-api-errors-01")
            .unwrap()
            .is_none());
    }

    #[test]
    fn upgrade_v1_from_37_preserves_review_rows_and_create_ends_at_38() {
        let fresh = tempfile::tempdir().unwrap();
        let created = SqliteStore::create(&fresh.path().join("state.db")).unwrap();
        assert_eq!(user_version(&created.connection), crate::store::SCHEMA);
        assert_eq!(
            created
                .connection
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='memory_required_generation'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        assert!(created
            .connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name='memory_scope_catalog'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
            .contains("STRICT"));
        drop(created);

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.db");
        let db = SqliteStore::create(&path).unwrap();
        let digest = "ab".repeat(32);
        db.connection
            .execute(
                "INSERT INTO memory_proposals(id,payload_digest,task_id,attempt_id,snapshot_id,review_state,payload,created_unix_ms) VALUES('mp-old',?1,'task-old','attempt-old',NULL,'validated','{}',1)",
                [&digest],
            )
            .unwrap();
        db.connection
            .execute(
                "INSERT INTO review_decisions(id,proposal_id,payload_digest,decision,classification,reviewed_heads,reason,created_unix_ms) VALUES('rev-old','mp-old',?1,'approve','[]','{\"event_head\":1}','kept',1)",
                [&digest],
            )
            .unwrap();
        let before: (String, String, String, String, String) = db
            .connection
            .query_row(
                "SELECT id,proposal_id,payload_digest,decision,reviewed_heads FROM review_decisions",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .unwrap();
        drop(db);
        let raw = rusqlite::Connection::open(&path).unwrap();
        crate::store::test_schema::historical(&raw, 37)
        .unwrap();
        drop(raw);
        let mut db = SqliteStore::open(&path).unwrap();
        assert_eq!(user_version(&db.connection), 37);
        assert!(matches!(
            db.import_legacy(&"cd".repeat(32), &[], &[]),
            Err(StoreError::UnsupportedSchema(37))
        ));
        let opened: (String, String, String, String, String) = db
            .connection
            .query_row(
                "SELECT id,proposal_id,payload_digest,decision,reviewed_heads FROM review_decisions",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .unwrap();
        assert_eq!(opened, before);
        db.upgrade_v1().unwrap();
        assert_eq!(user_version(&db.connection), crate::store::SCHEMA);
        assert_eq!(
            db.connection
                .query_row("SELECT schema_version FROM store_meta", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            crate::store::SCHEMA
        );
        let after: (String, String, String, String, String) = db
            .connection
            .query_row(
                "SELECT id,proposal_id,payload_digest,decision,reviewed_heads FROM review_decisions",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .unwrap();
        assert_eq!(after, before);
        assert_eq!(generation(&db.connection), 0);
        check_schema(&db.connection).unwrap();
        drop(db);
        let reopened = SqliteStore::open(&path).unwrap();
        assert_eq!(user_version(&reopened.connection), crate::store::SCHEMA);
    }

}

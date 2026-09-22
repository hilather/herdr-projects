//! Memory readiness is necessary, never sufficient, for verified completion.
use super::*;
use crate::domain::{MemoryBlocker, MemoryReadiness};

const CONSUMED_REVISIONS:&str="WITH RECURSIVE consumed(record_id,revision) AS (
        SELECT e.record_id,e.revision FROM attempts a JOIN memory_snapshots s ON s.id=a.snapshot AND s.task_id=a.task_id
        JOIN snapshot_entries e ON e.snapshot_id=s.id WHERE a.id=?2 AND a.task_id=?1
        UNION SELECT d.record_id,d.revision FROM memory_update_receipts x JOIN memory_delivery_intents d ON d.id=x.delivery_id
            WHERE x.attempt_id=?2 AND d.task_id=?1 AND x.state='applied'
        ), used(record_id,revision) AS (
        SELECT record_id,MAX(revision) FROM consumed GROUP BY record_id
        UNION SELECT d.source_record,d.source_revision FROM memory_dependencies d JOIN used u ON d.derived_record=u.record_id AND d.derived_revision=u.revision
        )";

pub(super) fn report(db: &Connection, task: &str, now: i64) -> Result<MemoryReadiness> {
    check_schema(db)?;
    let version: u32 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let attempt: Option<String> = db.query_row(
        "SELECT active_attempt FROM tasks WHERE id=?1",
        [task],
        |r| r.get(0),
    )?;
    let mut report = MemoryReadiness {
        task_id: task.into(),
        attempt_id: attempt.clone(),
        head: head(db)?,
        blockers: Vec::new(),
    };
    // Earlier schemas have no supported receipt protocol. Existing tasks remain
    // inspectable, but a memory-enabled store must upgrade before completion.
    if version < 18 {
        return Ok(report);
    }
    if version < 24 {
        report.blockers.push(MemoryBlocker {
            kind: "memory_schema_upgrade_required".into(),
            id: task.into(),
        });
        return Ok(report);
    }
    let mut collect = |sql: &str, values: &[&dyn rusqlite::ToSql]| -> Result<()> {
        let mut stmt = db.prepare(sql)?;
        let mut rows = stmt.query(values)?;
        while let Some(row) = rows.next()? {
            if report.blockers.len() >= 10_000 {
                return Err(StoreError::Limit(
                    "memory completion blockers exceed 10000".into(),
                ));
            }
            report.blockers.push(MemoryBlocker {
                kind: row.get(0)?,
                id: row.get(1)?,
            });
        }
        Ok(())
    };
    collect(
        "SELECT 'attempt_snapshot_unavailable',a.id FROM attempts a LEFT JOIN memory_snapshots s ON s.id=a.snapshot AND s.task_id=a.task_id WHERE a.id=?2 AND (a.task_id!=?1 OR s.id IS NULL)",
        &[&task, &attempt],
    )?;
    // Acknowledgment alone never resolves a contradiction or revoked dependency.
    collect(
        "SELECT 'unresolved_invalidation',id FROM memory_invalidations WHERE (task_id=?1 OR task_id IS NULL) AND resolved_seq IS NULL AND severity!='informational' ORDER BY id LIMIT 10001",
        &[&task],
    )?;
    // The current attempt may start with a newer snapshot that already contains a
    // required change. Otherwise only its own exact applied receipt can cover it.
    collect("SELECT 'required_update_unapplied',d.id FROM memory_delivery_intents d
        WHERE d.task_id=?1 AND d.severity!='informational'
        AND NOT EXISTS(SELECT 1 FROM memory_invalidations i WHERE i.task_id=d.task_id AND i.proposal_id=d.cause_id AND i.record_id=d.record_id AND i.triggering_seq=d.triggering_seq AND i.resolved_seq IS NOT NULL)
        AND NOT EXISTS(SELECT 1 FROM memory_update_receipts r WHERE r.delivery_id=d.id AND r.attempt_id=?2 AND r.state='applied')
        AND NOT EXISTS(SELECT 1 FROM attempts a JOIN memory_snapshots s ON s.id=a.snapshot AND s.task_id=a.task_id
            JOIN snapshot_entries e ON e.snapshot_id=s.id
            JOIN memory_heads h ON h.record_id=e.record_id AND h.revision=e.revision AND h.status='active'
            JOIN memory_validity v ON v.record_id=e.record_id AND v.revision=e.revision
            WHERE a.id=?2 AND a.task_id=?1 AND e.record_id=d.record_id AND e.revision>=d.revision
            AND v.state='valid' AND (v.expiry_unix_ms IS NULL OR v.expiry_unix_ms>?3))
        ORDER BY d.triggering_seq,d.id LIMIT 10001", &[&task,&attempt,&now])?;
    // A record can expire without a new event or routing intent. Recheck the exact
    // consumed revisions (including applied updates) and their transitive sources.
    // A newer applied revision supersedes its starting-snapshot version, but not
    // an older source pinned by another still-consumed derived revision.
    collect(&format!("{CONSUMED_REVISIONS} SELECT 'invalid_consumed_revision',u.record_id||'@'||u.revision FROM used u
        LEFT JOIN memory_heads h ON h.record_id=u.record_id
        LEFT JOIN memory_validity v ON v.record_id=u.record_id AND v.revision=u.revision
        LEFT JOIN memory_revisions r ON r.record_id=u.record_id AND r.revision=u.revision
        LEFT JOIN objects o ON o.hash=r.body_hash
        WHERE h.status IS NOT 'active' OR v.state IS NOT 'valid' OR v.expiry_unix_ms<=?3 OR o.availability IS NOT 'available'
        OR (h.revision!=u.revision AND EXISTS(SELECT 1 FROM memory_dependencies d JOIN used parent ON parent.record_id=d.derived_record AND parent.revision=d.derived_revision WHERE d.source_record=u.record_id AND d.source_revision=u.revision))
        ORDER BY u.record_id,u.revision LIMIT 10001"), &[&task,&attempt,&now])?;
    collect(
        "SELECT 'mandatory_revision_invalid',h.record_id||'@'||h.revision
        FROM memory_heads h JOIN memory_records r ON r.id=h.record_id
        JOIN memory_validity v ON v.record_id=h.record_id AND v.revision=h.revision
        WHERE h.status='active' AND (r.is_hard=1 OR r.kind IN ('constraint','hard_memory'))
        AND (v.state!='valid' OR v.expiry_unix_ms<=?1) ORDER BY h.record_id LIMIT 10001",
        &[&now],
    )?;
    // Mandatory changes cannot bypass the barrier just because they originated
    // through a policy/control path that has not yet created a delivery intent.
    collect("SELECT 'mandatory_revision_missing',h.record_id||'@'||h.revision
        FROM memory_heads h JOIN memory_records r ON r.id=h.record_id
        JOIN memory_validity v ON v.record_id=h.record_id AND v.revision=h.revision
        WHERE h.status='active' AND (r.is_hard=1 OR r.kind IN ('constraint','hard_memory'))
        AND v.state='valid' AND (v.expiry_unix_ms IS NULL OR v.expiry_unix_ms>?3)
        AND NOT EXISTS(SELECT 1 FROM attempts a JOIN memory_snapshots s ON s.id=a.snapshot AND s.task_id=a.task_id
            JOIN snapshot_entries e ON e.snapshot_id=s.id WHERE a.id=?2 AND a.task_id=?1 AND e.record_id=h.record_id AND e.revision=h.revision)
        AND NOT EXISTS(SELECT 1 FROM memory_delivery_intents d JOIN memory_update_receipts x ON x.delivery_id=d.id
            WHERE d.task_id=?1 AND d.record_id=h.record_id AND d.revision=h.revision AND x.attempt_id=?2 AND x.state='applied')
        ORDER BY h.record_id LIMIT 10001", &[&task,&attempt,&now])?;
    Ok(report)
}

pub(super) fn enforce(db: &Connection, task: &str, now: i64) -> Result<()> {
    let report = report(db, task, now)?;
    if let Some(blocker) = report.blockers.first() {
        return Err(StoreError::Invalid(format!(
            "memory completion blocked: {}:{} ({} blockers); inspect memory readiness",
            blocker.kind,
            blocker.id,
            report.blockers.len()
        )));
    }
    Ok(())
}

impl SqliteStore {
    pub(crate) fn memory_consumed_objects(&mut self, task: &str) -> Result<Vec<ObjectId>> {
        let tx = self.connection.transaction()?;
        check_schema(&tx)?;
        let version: u32 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 24 {
            return Err(StoreError::UnsupportedSchema(version));
        }
        let attempt: Option<String> = tx.query_row(
            "SELECT active_attempt FROM tasks WHERE id=?1",
            [task],
            |r| r.get(0),
        )?;
        let mut stmt=tx.prepare(&format!("{CONSUMED_REVISIONS} SELECT DISTINCT r.body_hash FROM used u JOIN memory_revisions r ON r.record_id=u.record_id AND r.revision=u.revision ORDER BY r.body_hash LIMIT 10001"))?;
        let rows = stmt
            .query_map(params![task, attempt], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if rows.len() > 10000 {
            return Err(StoreError::Limit(
                "consumed memory objects exceed 10000".into(),
            ));
        }
        rows.into_iter()
            .map(|h| ObjectId::from_hex(h).map_err(StoreError::Corrupt))
            .collect()
    }
    pub fn memory_readiness(&mut self, task: &str, now: i64) -> Result<MemoryReadiness> {
        let tx = self.connection.transaction()?;
        report(&tx, task, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryStore;

    fn fixture() -> (tempfile::TempDir, SqliteStore, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let mut db = SqliteStore::create(&path).unwrap();
        db.commit(Commit {
            expected_head: 0,
            mutations: ["a", "b"]
                .into_iter()
                .map(|id| Mutation::Task {
                    expected: None,
                    next: Task {
                        id: TaskId::new(id).unwrap(),
                        revision: 1,
                        state: TaskState::Draft,
                        title: id.into(),
                        active_attempt: None,
                    },
                })
                .collect(),
        })
        .unwrap();
        let mut memory = MemoryStore::from_sqlite(db, dir.path().join("objects"));
        let body = memory.ingest_object(&b"memory"[..]).unwrap();
        memory
            .insert_revision(
                &ControlContext { now_unix_ms: 1 },
                NewRevision {
                    id: MemoryRecordId::new("fact").unwrap(),
                    record_key: "fact".into(),
                    scope_id: "project".into(),
                    kind: MemoryKind::Observation,
                    body_hash: body.clone(),
                    provenance_hash: body,
                    applicability: Applicability {
                        domains: vec![],
                        paths: vec![],
                    },
                    dependencies: vec![],
                    expected: None,
                    expiry_unix_ms: None,
                    validity_state: "valid".into(),
                    validity_reason: "fixture".into(),
                },
            )
            .unwrap();
        let snapshot = memory
            .create_task_snapshot(
                SnapshotRequest {
                    schema_version: 1,
                    task_id: "a".into(),
                    profile: "worker".into(),
                    domains: vec![],
                    paths: vec![],
                    pinned_keys: vec![],
                    sensitivity: "default".into(),
                },
                "worker",
                &"a".repeat(64),
                None,
                32000,
                "Instructions",
                1,
                None,
            )
            .unwrap();
        let mut db = SqliteStore::open(&path).unwrap();
        let head = db.read_snapshot(None).unwrap().head;
        db.commit(Commit {
            expected_head: head,
            mutations: vec![
                Mutation::Attempt {
                    expected: None,
                    next: Attempt {
                        id: AttemptId::new("attempt-a").unwrap(),
                        task: TaskId::new("a").unwrap(),
                        revision: 1,
                        state: AttemptState::Running,
                        snapshot: Some(snapshot.id.as_str().into()),
                        reservation: "slot".into(),
                        termination_observed: false,
                    },
                },
                Mutation::Task {
                    expected: Some(1),
                    next: Task {
                        id: TaskId::new("a").unwrap(),
                        revision: 2,
                        state: TaskState::Running,
                        title: "A".into(),
                        active_attempt: Some(AttemptId::new("attempt-a").unwrap()),
                    },
                },
            ],
        })
        .unwrap();
        (dir, db, snapshot.id.as_str().into())
    }
    fn succeed(db: &mut SqliteStore, id: &str, clear_attempt: bool) -> Result<u64> {
        let state = db.read_snapshot(None)?;
        let mut task = state
            .tasks
            .iter()
            .find(|t| t.id.as_str() == id)
            .unwrap()
            .clone();
        let previous = task.revision;
        task.revision += 1;
        task.state = TaskState::Succeeded;
        if clear_attempt {
            task.active_attempt = None;
        }
        db.commit(Commit {
            expected_head: state.head,
            mutations: vec![Mutation::Task {
                expected: Some(previous),
                next: task,
            }],
        })
    }
    #[test]
    fn invalidations_block_completion_and_attempt_clearing_but_not_unrelated_tasks() {
        let (_dir, mut db, _) = fixture();
        let head = db.read_snapshot(None).unwrap().head;
        db.connection.execute("INSERT INTO memory_invalidations VALUES('block','a','fixture','fact','stop_at_checkpoint',?1,NULL,'fixture')",[head]).unwrap();
        let before = db.read_snapshot(None).unwrap();
        assert!(succeed(&mut db, "a", false).is_err());
        assert!(succeed(&mut db, "a", true).is_err());
        assert_eq!(db.read_snapshot(None).unwrap(), before);
        assert_eq!(
            db.memory_readiness("a", 1).unwrap().blockers[0].kind,
            "unresolved_invalidation"
        );
        succeed(&mut db, "b", false).unwrap();
    }
    #[test]
    fn expiry_and_missing_snapshot_block_without_an_update_event() {
        let (_dir, mut db, _) = fixture();
        assert!(db.memory_readiness("a", 1).unwrap().blockers.is_empty());
        db.connection
            .execute(
                "UPDATE memory_validity SET expiry_unix_ms=2 WHERE record_id='fact'",
                [],
            )
            .unwrap();
        assert!(db.memory_readiness("a", 1).unwrap().blockers.is_empty());
        assert_eq!(
            db.memory_readiness("a", 2).unwrap().blockers[0].kind,
            "invalid_consumed_revision"
        );
        assert!(succeed(&mut db, "a", true).is_err());
        db.connection
            .execute("UPDATE attempts SET snapshot=NULL WHERE id='attempt-a'", [])
            .unwrap();
        assert_eq!(
            db.memory_readiness("a", 2).unwrap().blockers[0].kind,
            "attempt_snapshot_unavailable"
        );
    }
    #[test]
    fn informational_invalidations_do_not_block_but_global_required_ones_do() {
        let (_dir, mut db, _) = fixture();
        let head = db.read_snapshot(None).unwrap().head;
        db.connection.execute("INSERT INTO memory_invalidations VALUES('advice','a','fixture','fact','informational',?1,NULL,'fixture')",[head]).unwrap();
        succeed(&mut db, "a", false).unwrap();
        db.connection.execute("INSERT INTO memory_invalidations VALUES('global',NULL,'fixture',NULL,'stop_at_checkpoint',?1,NULL,'fixture')",[head]).unwrap();
        assert!(succeed(&mut db, "b", false).is_err());
    }
    #[test]
    fn exact_applied_receipt_covers_only_its_required_update() {
        let (_dir, mut db, snapshot) = fixture();
        // A pending update absent from the consumed snapshot. Raw SQL is fixture
        // construction only; production updates come through signed promotion.
        db.connection.execute("INSERT INTO memory_revisions SELECT record_id,2,body_hash,provenance_hash,promoted_seq,applicability FROM memory_revisions WHERE record_id='fact' AND revision=1",[]).unwrap();
        db.connection
            .execute(
                "UPDATE memory_heads SET revision=2 WHERE record_id='fact'",
                [],
            )
            .unwrap();
        db.connection.execute("INSERT INTO memory_validity SELECT record_id,2,state,reason,expiry_unix_ms,evaluated_seq FROM memory_validity WHERE record_id='fact' AND revision=1",[]).unwrap();
        let seq = db.read_snapshot(None).unwrap().head;
        db.connection.execute("INSERT INTO memory_delivery_intents VALUES('update','fixture','task:a',?1,'a','fact',2,'reconcile_before_completion',?2,'pending')",params![snapshot,seq]).unwrap();
        assert_eq!(
            db.memory_readiness("a", 1).unwrap().blockers[0].kind,
            "required_update_unapplied"
        );
        assert!(succeed(&mut db, "a", false).is_err());
        let update = db.memory_update("update", "attempt-a").unwrap();
        let mut ack = MemoryUpdateAck {
            schema_version: 1,
            delivery_id: "update".into(),
            attempt_id: "attempt-a".into(),
            manifest_hash: update.manifest_hash,
            state: "seen".into(),
        };
        db.acknowledge_memory_update(&ack, 1).unwrap();
        assert!(succeed(&mut db, "a", false).is_err());
        ack.state = "applied".into();
        db.acknowledge_memory_update(&ack, 1).unwrap();
        assert!(db.memory_readiness("a", 1).unwrap().blockers.is_empty());
        // The applied revision becomes consumed knowledge and is rechecked for
        // expiry. Its newer body supersedes the original snapshot's version.
        db.connection
            .execute(
                "UPDATE memory_validity SET expiry_unix_ms=2 WHERE record_id='fact' AND revision=1",
                [],
            )
            .unwrap();
        assert!(db.memory_readiness("a", 2).unwrap().blockers.is_empty());
        db.connection
            .execute(
                "UPDATE memory_validity SET expiry_unix_ms=2 WHERE record_id='fact' AND revision=2",
                [],
            )
            .unwrap();
        assert!(
            db.memory_readiness("a", 2)
                .unwrap()
                .blockers
                .iter()
                .any(|b| b.kind == "invalid_consumed_revision" && b.id == "fact@2")
        );
        db.connection
            .execute(
                "UPDATE memory_validity SET expiry_unix_ms=NULL WHERE record_id='fact'",
                [],
            )
            .unwrap();
        // A second logical change has its own receipt requirement.
        db.connection.execute("INSERT INTO memory_delivery_intents VALUES('later','other-cause','task:a',?1,'a','fact',2,'stop_at_checkpoint',?2,'pending')",params![snapshot,seq]).unwrap();
        assert_eq!(db.memory_readiness("a", 1).unwrap().blockers[0].id, "later");
        assert!(succeed(&mut db, "a", false).is_err());
    }
    #[test]
    fn changed_dependency_and_unrouted_mandatory_revision_are_blockers() {
        let (_dir, mut db, _) = fixture();
        db.connection.execute_batch("INSERT INTO memory_records SELECT 'source','source',scope_id,kind,is_hard FROM memory_records WHERE id='fact';
            INSERT INTO memory_revisions SELECT 'source',revision,body_hash,provenance_hash,promoted_seq,applicability FROM memory_revisions WHERE record_id='fact';
            INSERT INTO memory_validity SELECT 'source',revision,state,reason,expiry_unix_ms,evaluated_seq FROM memory_validity WHERE record_id='fact';
            INSERT INTO memory_heads SELECT 'source',revision,status,row_revision FROM memory_heads WHERE record_id='fact';
            INSERT INTO memory_dependencies VALUES('fact',1,'source',1,'supports');").unwrap();
        assert!(db.memory_readiness("a", 1).unwrap().blockers.is_empty());
        db.connection.execute_batch("INSERT INTO memory_revisions SELECT record_id,2,body_hash,provenance_hash,promoted_seq,applicability FROM memory_revisions WHERE record_id='source' AND revision=1;
            INSERT INTO memory_validity SELECT record_id,2,state,reason,expiry_unix_ms,evaluated_seq FROM memory_validity WHERE record_id='source' AND revision=1;
            UPDATE memory_heads SET revision=2 WHERE record_id='source';").unwrap();
        assert!(
            db.memory_readiness("a", 1)
                .unwrap()
                .blockers
                .iter()
                .any(|b| b.kind == "invalid_consumed_revision" && b.id == "source@1")
        );
        assert!(succeed(&mut db, "a", false).is_err());
        db.connection
            .execute("UPDATE memory_records SET is_hard=1 WHERE id='source'", [])
            .unwrap();
        assert!(
            db.memory_readiness("a", 1)
                .unwrap()
                .blockers
                .iter()
                .any(|b| b.kind == "mandatory_revision_missing" && b.id == "source@2")
        );
        assert!(succeed(&mut db, "b", false).is_err());
    }

    #[test]
    fn later_mutation_in_success_batch_cannot_drop_consumed_snapshot() {
        let (_dir, mut db, _) = fixture();
        let state = db.read_snapshot(None).unwrap();
        let mut task = state
            .tasks
            .iter()
            .find(|t| t.id.as_str() == "a")
            .unwrap()
            .clone();
        let previous = task.revision;
        task.revision += 1;
        task.state = TaskState::Succeeded;
        let mut attempt = state.attempts[0].clone();
        attempt.revision += 1;
        attempt.snapshot = None;
        assert!(
            db.commit(Commit {
                expected_head: state.head,
                mutations: vec![
                    Mutation::Task {
                        expected: Some(previous),
                        next: task
                    },
                    Mutation::Attempt {
                        expected: Some(1),
                        next: attempt
                    },
                ]
            })
            .is_err()
        );
        assert_eq!(db.read_snapshot(None).unwrap(), state);
    }
    #[test]
    fn older_memory_schemas_require_upgrade_before_success() {
        let (_dir, mut db, _) = fixture();
        for version in [18, 21, 22, 23] {
            db.connection
                .pragma_update(None, "user_version", version)
                .unwrap();
            db.connection
                .execute("UPDATE store_meta SET schema_version=?1", [version])
                .unwrap();
            let before = db.read_snapshot(None).unwrap();
            assert_eq!(
                db.memory_readiness("b", 1).unwrap().blockers[0].kind,
                "memory_schema_upgrade_required"
            );
            assert!(succeed(&mut db, "b", false).is_err());
            assert_eq!(db.read_snapshot(None).unwrap(), before);
        }
    }
}

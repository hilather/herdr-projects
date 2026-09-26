//! Exact-change acknowledgments. These are worker declarations, not validation
//! evidence, transport success, or authorization to clear invalidations.
use super::*;
use crate::domain::{MemoryUpdate, MemoryUpdateAck, MemoryUpdateReceipt};

fn schema(db: &Connection) -> Result<()> {
    check_schema(db)?;
    let version: u32 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 24 {
        return Err(StoreError::UnsupportedSchema(version));
    }
    Ok(())
}

fn update(db: &Connection, delivery: &str, attempt: &str) -> Result<MemoryUpdate> {
    schema(db)?;
    // Only the current, live attempt that consumed this exact starting snapshot
    // can read or acknowledge the update. Never infer identity from terminal state.
    let row = db
        .query_row(
            "SELECT d.snapshot_id,d.record_id,d.revision,v.body_hash,d.triggering_seq,d.severity
         FROM memory_delivery_intents d JOIN tasks t ON t.id=d.task_id
         JOIN attempts a ON a.id=t.active_attempt AND a.task_id=t.id
         JOIN memory_revisions v ON v.record_id=d.record_id AND v.revision=d.revision
         WHERE d.id=?1 AND a.id=?2 AND a.snapshot=d.snapshot_id
         AND a.termination_observed=0 AND a.state IN ('running','awaiting_input')
         AND t.state NOT IN ('succeeded','failed','cancelled')",
            params![delivery, attempt],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, u64>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, u64>(4)?,
                    r.get::<_, String>(5)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| {
            StoreError::Invalid(
                "update does not belong to the current live attempt and snapshot".into(),
            )
        })?;
    let (snapshot, record, revision, body, sequence, severity) = row;
    let manifest_hash = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&serde_json::json!([
                "memory-update-v1",
                delivery,
                attempt,
                snapshot,
                record,
                revision,
                body,
                sequence,
                severity
            ]))
            .map_err(|e| StoreError::Invalid(e.to_string()))?
        )
    );
    Ok(MemoryUpdate {
        delivery_id: delivery.into(),
        attempt_id: attempt.into(),
        input_snapshot_id: snapshot,
        record_id: record,
        revision,
        body_hash: ObjectId::from_hex(body).map_err(StoreError::Corrupt)?,
        triggering_seq: sequence,
        severity,
        manifest_hash,
    })
}

impl SqliteStore {
    pub fn memory_update(&mut self, delivery: &str, attempt: &str) -> Result<MemoryUpdate> {
        let tx = self.connection.transaction()?;
        update(&tx, delivery, attempt)
    }

    pub(crate) fn acknowledge_memory_update(
        &mut self,
        ack: &MemoryUpdateAck,
        now: i64,
    ) -> Result<MemoryUpdateReceipt> {
        if ack.schema_version != 1 || !matches!(ack.state.as_str(), "seen" | "applied") {
            return Err(StoreError::Invalid(
                "unsupported memory acknowledgment version or state".into(),
            ));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let item = update(&tx, &ack.delivery_id, &ack.attempt_id)?;
        if ack.manifest_hash != item.manifest_hash {
            return Err(StoreError::Invalid(
                "memory acknowledgment digest mismatch".into(),
            ));
        }
        // Replay is idempotent only for the same still-current attempt. A stopped
        // or replaced attempt cannot acknowledge anything, including an old receipt.
        let existing: Option<u64> = tx.query_row(
            "SELECT sequence FROM memory_update_receipts WHERE delivery_id=?1 AND attempt_id=?2 AND state=?3 AND manifest_hash=?4",
            params![ack.delivery_id,ack.attempt_id,ack.state,ack.manifest_hash], |r| r.get(0)).optional()?;
        if let Some(sequence) = existing {
            return Ok(MemoryUpdateReceipt {
                delivery_id: ack.delivery_id.clone(),
                attempt_id: ack.attempt_id.clone(),
                state: ack.state.clone(),
                manifest_hash: ack.manifest_hash.clone(),
                sequence,
            });
        }
        if ack.state == "applied" {
            let seen: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM memory_update_receipts WHERE delivery_id=?1 AND attempt_id=?2 AND state='seen' AND manifest_hash=?3)",
                params![ack.delivery_id,ack.attempt_id,ack.manifest_hash], |r|r.get(0))?;
            if !seen {
                return Err(StoreError::Invalid(
                    "explicit seen acknowledgment required before applied".into(),
                ));
            }
            let current: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM memory_heads h JOIN memory_validity v ON v.record_id=h.record_id AND v.revision=h.revision
                 WHERE h.record_id=?1 AND h.revision=?2 AND h.status='active' AND v.state='valid'
                 AND (v.expiry_unix_ms IS NULL OR v.expiry_unix_ms>?3))",
                params![item.record_id,integer(item.revision)?,now], |r|r.get(0))?;
            if !current {
                return Err(StoreError::Invalid(
                    "cannot apply a superseded or invalid memory revision; pull the current update"
                        .into(),
                ));
            }
        }
        let payload = serde_json::to_string(ack).map_err(|e| StoreError::Invalid(e.to_string()))?;
        tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('memory.update_ack',?1,1,1,?2)", params![ack.delivery_id,payload])?;
        let sequence = head(&tx)?;
        tx.execute(
            "INSERT INTO memory_update_receipts VALUES(?1,?2,?3,?4,?5)",
            params![
                ack.delivery_id,
                ack.attempt_id,
                ack.state,
                ack.manifest_hash,
                integer(sequence)?
            ],
        )?;
        tx.commit()?;
        Ok(MemoryUpdateReceipt {
            delivery_id: ack.delivery_id.clone(),
            attempt_id: ack.attempt_id.clone(),
            state: ack.state.clone(),
            manifest_hash: ack.manifest_hash.clone(),
            sequence,
        })
    }

    pub fn memory_update_receipts(&mut self, attempt: &str) -> Result<Vec<MemoryUpdateReceipt>> {
        schema(&self.connection)?;
        let mut stmt = self.connection.prepare("SELECT delivery_id,attempt_id,state,manifest_hash,sequence FROM memory_update_receipts WHERE attempt_id=?1 ORDER BY sequence LIMIT 10001")?;
        let rows = stmt
            .query_map([attempt], |r| {
                Ok(MemoryUpdateReceipt {
                    delivery_id: r.get(0)?,
                    attempt_id: r.get(1)?,
                    state: r.get(2)?,
                    manifest_hash: r.get(3)?,
                    sequence: r.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if rows.len() > 10_000 {
            return Err(StoreError::Limit(
                "memory receipt inventory exceeds 10000".into(),
            ));
        }
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        Applicability, Attempt, AttemptId, AttemptState, Commit, ControlContext, MemoryKind,
        MemoryRecordId, MemoryUpdateAck, Mutation, NewRevision, SnapshotRequest, Task, TaskId,
        TaskState,
    };
    use crate::memory::MemoryStore;

    fn fixture() -> (tempfile::TempDir, SqliteStore, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let mut db = SqliteStore::create(&path).unwrap();
        db.commit(Commit {
            expected_head: 0,
            mutations: vec![Mutation::Task {
                expected: None,
                next: Task {
                    id: TaskId::new("a").unwrap(),
                    revision: 1,
                    state: TaskState::Draft,
                    title: "A".into(),
                    active_attempt: None,
                },
            }],
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
        let seq: i64 = db
            .connection
            .query_row("SELECT max(sequence) FROM events", [], |row| row.get(0))
            .unwrap();
        db.connection
            .execute(
                "INSERT INTO memory_delivery_intents VALUES('update','fixture','task:a',?1,'a','fact',1,'informational',?2,'pending')",
                params![snapshot.id.as_str(), seq],
            )
            .unwrap();
        (dir, db, snapshot.id.as_str().into())
    }

    #[test]
    fn worker_receipt_primary_key_still_replays_or_conflicts_on_delivery_and_attempt() {
        let (_dir, mut db, _) = fixture();
        let sql: String = db
            .connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name='memory_update_receipts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(sql.contains("PRIMARY KEY (delivery_id,attempt_id,state)"));
        assert!(!sql.contains("change_id"));
        assert!(!sql.contains("consumer_binding_generation"));
        let update = db.memory_update("update", "attempt-a").unwrap();
        let mut ack = MemoryUpdateAck {
            schema_version: 1,
            delivery_id: "update".into(),
            attempt_id: "attempt-a".into(),
            manifest_hash: update.manifest_hash.clone(),
            state: "seen".into(),
        };
        let seen = db.acknowledge_memory_update(&ack, 1).unwrap();
        assert_eq!(db.acknowledge_memory_update(&ack, 1).unwrap(), seen);
        let duplicate = db.connection.execute(
            "INSERT INTO memory_update_receipts VALUES('update','attempt-a','seen',?1,?2)",
            params![seen.manifest_hash, i64::try_from(seen.sequence).unwrap()],
        );
        assert_eq!(
            duplicate.unwrap_err().sqlite_error_code(),
            Some(rusqlite::ErrorCode::ConstraintViolation)
        );
        ack.state = "applied".into();
        let applied = db.acknowledge_memory_update(&ack, 1).unwrap();
        assert_ne!(applied.state, seen.state);
        assert_eq!(db.acknowledge_memory_update(&ack, 1).unwrap(), applied);
        let duplicate_applied = db.connection.execute(
            "INSERT INTO memory_update_receipts VALUES('update','attempt-a','applied',?1,?2)",
            params![applied.manifest_hash, i64::try_from(applied.sequence).unwrap()],
        );
        assert_eq!(
            duplicate_applied.unwrap_err().sqlite_error_code(),
            Some(rusqlite::ErrorCode::ConstraintViolation)
        );
    }
}

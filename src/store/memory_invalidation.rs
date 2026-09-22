//! Dependency invalidation is committed with the source mutation and routing.
use super::*;

pub(super) fn dependents(
    tx: &rusqlite::Transaction,
    source: &str,
    revision: u64,
    sequence: u64,
) -> Result<()> {
    let version: u32 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 24 {
        return Err(StoreError::UnsupportedSchema(version));
    }
    // Traverse historical edges too: a current derived fact can still depend on
    // an old intermediate revision whose own head has since changed.
    let mut stmt=tx.prepare("WITH RECURSIVE affected(id,revision) AS (
        SELECT derived_record,derived_revision FROM memory_dependencies WHERE source_record=?1 AND source_revision=?2
        UNION SELECT d.derived_record,d.derived_revision FROM memory_dependencies d JOIN affected a ON d.source_record=a.id AND d.source_revision=a.revision
        ) SELECT id,revision FROM affected LIMIT 10001")?;
    let rows = stmt
        .query_map(params![source, integer(revision)?], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, u64>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if rows.len() > 10000 {
        return Err(StoreError::Limit(
            "memory dependency invalidation exceeds 10000 revisions".into(),
        ));
    }
    for (id, rev) in rows {
        let changed=tx.execute("UPDATE memory_validity SET state='stale',reason='dependency_changed',evaluated_seq=?3
            WHERE record_id=?1 AND revision=?2 AND state='valid'
            AND EXISTS(SELECT 1 FROM memory_heads h WHERE h.record_id=?1 AND h.revision=?2 AND h.status='active')",params![id,integer(rev)?,integer(sequence)?])?;
        if changed != 0 {
            let cause = format!("dependency:{sequence}:{source}:{revision}");
            super::memory_delivery::record_change(
                tx,
                &cause,
                &id,
                rev,
                "stop_at_checkpoint",
                sequence,
            )?;
        }
    }
    Ok(())
}

pub(super) fn changed(
    tx: &rusqlite::Transaction,
    id: &str,
    revision: u64,
    sequence: u64,
    reason: &str,
) -> Result<()> {
    dependents(tx, id, revision, sequence)?;
    super::memory_delivery::record_change(
        tx,
        &format!("{reason}:{sequence}"),
        id,
        revision,
        "stop_at_checkpoint",
        sequence,
    )
}

/// Expiry has no write event, so selection must also check sources at read time.
pub(super) fn dependencies_current(
    db: &Connection,
    record: &str,
    revision: u64,
    now: i64,
) -> Result<bool> {
    Ok(db.query_row("WITH RECURSIVE sources(id,revision) AS (
        SELECT source_record,source_revision FROM memory_dependencies WHERE derived_record=?1 AND derived_revision=?2
        UNION SELECT d.source_record,d.source_revision FROM memory_dependencies d JOIN sources s ON d.derived_record=s.id AND d.derived_revision=s.revision
        ) SELECT NOT EXISTS(SELECT 1 FROM sources s LEFT JOIN memory_heads h ON h.record_id=s.id
        LEFT JOIN memory_validity v ON v.record_id=s.id AND v.revision=s.revision
        LEFT JOIN memory_revisions r ON r.record_id=s.id AND r.revision=s.revision LEFT JOIN objects o ON o.hash=r.body_hash
        WHERE h.status IS NOT 'active' OR h.revision!=s.revision OR v.state IS NOT 'valid' OR v.expiry_unix_ms<=?3 OR o.availability IS NOT 'available')",params![record,integer(revision)?,now],|r|r.get(0))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryStore;

    fn put(
        memory: &mut MemoryStore,
        id: &str,
        expected: Option<u64>,
        dependencies: Vec<(MemoryRecordId, u64, String)>,
    ) {
        let body = memory.ingest_object(id.as_bytes()).unwrap();
        memory
            .insert_revision(
                &ControlContext { now_unix_ms: 1 },
                NewRevision {
                    id: MemoryRecordId::new(id).unwrap(),
                    record_key: id.into(),
                    scope_id: "project".into(),
                    kind: MemoryKind::Observation,
                    body_hash: body.clone(),
                    provenance_hash: body,
                    applicability: Applicability {
                        domains: vec![],
                        paths: vec![],
                    },
                    dependencies,
                    expected,
                    expiry_unix_ms: None,
                    validity_state: "valid".into(),
                    validity_reason: "fixture".into(),
                },
            )
            .unwrap();
    }
    fn dep(id: &str) -> Vec<(MemoryRecordId, u64, String)> {
        vec![(MemoryRecordId::new(id).unwrap(), 1, "supports".into())]
    }
    #[test]
    fn source_change_atomically_invalidates_transitive_consumers_and_replay_is_quiet() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let mut db = SqliteStore::create(&path).unwrap();
        db.commit(Commit {
            expected_head: 0,
            mutations: vec![Mutation::Task {
                expected: None,
                next: Task {
                    id: TaskId::new("consumer").unwrap(),
                    revision: 1,
                    state: TaskState::Draft,
                    title: "Consumer".into(),
                    active_attempt: None,
                },
            }],
        })
        .unwrap();
        let mut memory = MemoryStore::from_sqlite(db, dir.path().join("objects"));
        put(&mut memory, "source", None, vec![]);
        put(&mut memory, "middle", None, dep("source"));
        put(&mut memory, "leaf", None, dep("middle"));
        put(&mut memory, "unrelated", None, vec![]);
        memory
            .create_task_snapshot(
                SnapshotRequest {
                    schema_version: 1,
                    task_id: "consumer".into(),
                    profile: "worker".into(),
                    domains: vec![],
                    paths: vec![],
                    pinned_keys: vec!["leaf".into()],
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
        let before = db.read_snapshot(None).unwrap();
        db.connection.execute_batch("CREATE TRIGGER fail_dependency BEFORE INSERT ON memory_delivery_intents WHEN NEW.record_id='leaf' BEGIN SELECT RAISE(ABORT,'injected dependency routing failure'); END;").unwrap();
        assert!(
            memory
                .revoke(
                    &ControlContext { now_unix_ms: 2 },
                    &MemoryRecordId::new("source").unwrap(),
                    1
                )
                .is_err()
        );
        assert_eq!(db.read_snapshot(None).unwrap(), before);
        assert_eq!(memory.active_facts(2).unwrap().len(), 4);
        assert!(db.memory_delivery_intents().unwrap().is_empty());
        db.connection
            .execute_batch("DROP TRIGGER fail_dependency;")
            .unwrap();
        put(&mut memory, "source", Some(1), vec![]);
        let active = memory.active_facts(2).unwrap();
        assert_eq!(active.len(), 2);
        assert!(
            active
                .iter()
                .all(|f| matches!(f.record.id.as_str(), "source" | "unrelated"))
        );
        let rows = db.memory_delivery_intents().unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r["severity"] == "stop_at_checkpoint"));
        let tx = db.connection.transaction().unwrap();
        let seq = head(&tx).unwrap();
        dependents(&tx, "source", 1, seq).unwrap();
        tx.commit().unwrap();
        assert_eq!(db.memory_delivery_intents().unwrap(), rows);
        assert_eq!(db.memory_invalidations("consumer").unwrap().len(), 2);
        put(
            &mut memory,
            "middle",
            Some(1),
            vec![(MemoryRecordId::new("source").unwrap(), 2, "supports".into())],
        );
        assert_eq!(memory.active_facts(2).unwrap().len(), 3);
        put(
            &mut memory,
            "leaf",
            Some(1),
            vec![(MemoryRecordId::new("middle").unwrap(), 2, "supports".into())],
        );
        assert_eq!(memory.active_facts(2).unwrap().len(), 4);
    }
    #[test]
    fn expiry_of_a_source_excludes_derived_facts_without_a_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let db = SqliteStore::create(&path).unwrap();
        let mut memory = MemoryStore::from_sqlite(db, dir.path().join("objects"));
        put(&mut memory, "source", None, vec![]);
        put(&mut memory, "derived", None, dep("source"));
        let db = SqliteStore::open(&path).unwrap();
        db.connection
            .execute(
                "UPDATE memory_validity SET expiry_unix_ms=2 WHERE record_id='source'",
                [],
            )
            .unwrap();
        assert_eq!(memory.active_facts(1).unwrap().len(), 2);
        assert!(memory.active_facts(2).unwrap().is_empty());
    }
}

//! Atomic routing work, not transport receipts or proof of applied knowledge.
use super::*;

// Follow the exact revisions the consumer read, including historical dependency
// edges. Current-head edges would lose the dependency precisely when it changes.
fn affected(
    tx: &rusqlite::Transaction,
    snapshot: &str,
    task: &str,
    record: &str,
    revision: u64,
) -> Result<bool> {
    if task == "coordinator" {
        return Ok(true);
    }
    let consumed: bool = tx.query_row(
        "WITH RECURSIVE used(record_id,revision) AS (
            SELECT record_id,revision FROM snapshot_entries WHERE snapshot_id=?1
            UNION
            SELECT d.source_record,d.source_revision FROM memory_dependencies d
            JOIN used u ON d.derived_record=u.record_id AND d.derived_revision=u.revision
        ) SELECT EXISTS(SELECT 1 FROM used WHERE record_id=?2)",
        params![snapshot, record],
        |r| r.get(0),
    )?;
    if consumed {
        return Ok(true);
    }
    let (kind, scope, hard, key, raw): (String, String, bool, String, String) = tx.query_row(
        "SELECT r.kind,r.scope_id,r.is_hard,r.record_key,v.applicability FROM memory_records r
         JOIN memory_revisions v ON v.record_id=r.id WHERE r.id=?1 AND v.revision=?2",
        params![record, integer(revision)?],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;
    if kind == "task_local" && scope != format!("task:{task}") {
        return Ok(false);
    }
    if hard || kind == "constraint" || kind == "hard_memory" {
        return Ok(true);
    }
    let request: Option<String> = tx
        .query_row(
            "SELECT request_json FROM memory_snapshot_inputs WHERE snapshot_id=?1",
            [snapshot],
            |r| r.get(0),
        )
        .optional()?;
    // Legacy snapshots have no trustworthy scope: retain conservative delivery.
    let Some(request) = request else {
        return Ok(true);
    };
    let request: crate::domain::SnapshotRequest =
        serde_json::from_str(&request).map_err(|e| StoreError::Corrupt(e.to_string()))?;
    let applicability: crate::domain::Applicability =
        serde_json::from_str(&raw).map_err(|e| StoreError::Corrupt(e.to_string()))?;
    applicability.validate().map_err(StoreError::Corrupt)?;
    Ok(
        request.pinned_keys.contains(&key)
            || crate::domain::scope_matches(&request, &applicability),
    )
}

pub(super) fn record_change(
    tx: &rusqlite::Transaction,
    cause: &str,
    record: &str,
    revision: u64,
    severity: &str,
    sequence: u64,
) -> Result<()> {
    let version: u32 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 23 {
        return Err(StoreError::UnsupportedSchema(version));
    }
    let mut query=tx.prepare("SELECT s.subscriber,s.snapshot_id,m.task_id FROM memory_subscriptions s JOIN memory_snapshots m ON m.id=s.snapshot_id LEFT JOIN tasks t ON t.id=m.task_id WHERE m.task_id='coordinator' OR (t.state NOT IN ('succeeded','failed','cancelled') AND (t.active_attempt IS NULL OR EXISTS(SELECT 1 FROM attempts a WHERE a.id=t.active_attempt AND a.snapshot=m.id AND a.termination_observed=0 AND a.state IN ('reserved','launching','running','awaiting_input')))) ORDER BY s.id LIMIT 10001")?;
    let consumers = query
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if consumers.len() > 10_000 {
        return Err(StoreError::Limit(
            "memory routing exceeds 10000 subscriptions; promotion not committed".into(),
        ));
    }
    for (subscriber, snapshot, task) in consumers {
        if !affected(tx, &snapshot, &task, record, revision)? {
            continue;
        }
        let task_id = if task == "coordinator" {
            None
        } else {
            Some(task)
        };
        let id = format!(
            "delivery-{:x}",
            Sha256::digest(
                serde_json::json!([cause, subscriber, snapshot, record, revision])
                    .to_string()
                    .as_bytes()
            )
        );
        tx.execute(
            "INSERT INTO memory_delivery_intents VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,'pending')",
            params![
                id,
                cause,
                subscriber,
                snapshot,
                task_id,
                record,
                integer(revision)?,
                severity,
                integer(sequence)?
            ],
        )?;
        if let Some(task) = task_id {
            let invalidation = format!(
                "inv-{:x}",
                Sha256::digest(
                    serde_json::json!([cause, task, record, revision])
                        .to_string()
                        .as_bytes()
                )
            );
            tx.execute("INSERT OR IGNORE INTO memory_invalidations VALUES(?1,?2,?3,?4,?5,?6,NULL,'routing_pending')",params![invalidation,task,cause,record,severity,integer(sequence)?])?;
        }
    }
    Ok(())
}
impl SqliteStore {
    pub fn memory_delivery_intents(&mut self) -> Result<Vec<serde_json::Value>> {
        let mut stmt=self.connection.prepare("SELECT id,cause_id,subscriber,snapshot_id,task_id,record_id,revision,severity,triggering_seq,state FROM memory_delivery_intents ORDER BY triggering_seq,id LIMIT 10001")?;
        let mut rows = stmt.query([])?;
        let mut result = Vec::new();
        while let Some(r) = rows.next()? {
            if result.len() >= 10_000 {
                return Err(StoreError::Limit(
                    "memory delivery inventory exceeds 10000".into(),
                ));
            }
            result.push(serde_json::json!({"id":r.get::<_,String>(0)?,"cause_id":r.get::<_,String>(1)?,"subscriber":r.get::<_,String>(2)?,"snapshot_id":r.get::<_,String>(3)?,"task_id":r.get::<_,Option<String>>(4)?,"record_id":r.get::<_,String>(5)?,"revision":r.get::<_,u64>(6)?,"severity":r.get::<_,String>(7)?,"triggering_seq":r.get::<_,u64>(8)?,"state":r.get::<_,String>(9)?}));
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryStore;

    #[test]
    fn routing_keeps_consumed_historical_dependencies_and_scope_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SqliteStore::create(&dir.path().join("state.db")).unwrap();
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
        let mut memory = MemoryStore::from_sqlite(db, dir.path().join("objects"));
        let body = memory.ingest_object(&b"fact"[..]).unwrap();
        for (id, dependencies, expected, kind, scope, paths) in [
            (
                "source",
                vec![],
                None,
                MemoryKind::Observation,
                "project",
                vec!["backend/**"],
            ),
            (
                "derived",
                vec![(MemoryRecordId::new("source").unwrap(), 1, "supports".into())],
                None,
                MemoryKind::Observation,
                "project",
                vec!["ui/**"],
            ),
            (
                "foreign",
                vec![],
                None,
                MemoryKind::TaskLocal,
                "task:other",
                vec!["ui/**"],
            ),
            (
                "near-prefix",
                vec![],
                None,
                MemoryKind::Observation,
                "project",
                vec!["ui-extra/**"],
            ),
        ] {
            memory
                .insert_revision(
                    &ControlContext { now_unix_ms: 1 },
                    NewRevision {
                        id: MemoryRecordId::new(id).unwrap(),
                        record_key: id.into(),
                        scope_id: scope.into(),
                        kind,
                        body_hash: body.clone(),
                        provenance_hash: body.clone(),
                        applicability: Applicability {
                            domains: vec![],
                            paths: paths.into_iter().map(String::from).collect(),
                        },
                        dependencies,
                        expected,
                        expiry_unix_ms: None,
                        validity_state: "valid".into(),
                        validity_reason: "test".into(),
                    },
                )
                .unwrap();
        }
        let snapshot = memory
            .create_task_snapshot(
                SnapshotRequest {
                    schema_version: 1,
                    task_id: "task-a".into(),
                    profile: "worker".into(),
                    domains: vec![],
                    paths: vec!["ui/components/button.rs".into()],
                    pinned_keys: vec![],
                    sensitivity: "default".into(),
                },
                "worker",
                &"a".repeat(64),
                None,
                32_000,
                "instructions",
                1,
                None,
            )
            .unwrap();
        assert_eq!(snapshot.entries.len(), 1);
        assert_eq!(snapshot.entries[0].record_id.as_str(), "derived");
        // Moving the derived head removes its current dependency; the consumed
        // revision still depended on source@1 and must receive a source update.
        memory
            .insert_revision(
                &ControlContext { now_unix_ms: 2 },
                NewRevision {
                    id: MemoryRecordId::new("derived").unwrap(),
                    record_key: "derived".into(),
                    scope_id: "project".into(),
                    kind: MemoryKind::Observation,
                    body_hash: body.clone(),
                    provenance_hash: body,
                    applicability: Applicability {
                        domains: vec![],
                        paths: vec!["backend/**".into()],
                    },
                    dependencies: vec![],
                    expected: Some(1),
                    expiry_unix_ms: None,
                    validity_state: "valid".into(),
                    validity_reason: "test".into(),
                },
            )
            .unwrap();
        let mut db = SqliteStore::open(&dir.path().join("state.db")).unwrap();
        let tx = db.connection.transaction().unwrap();
        assert!(affected(&tx, snapshot.id.as_str(), "task-a", "source", 1).unwrap());
        assert!(affected(&tx, snapshot.id.as_str(), "task-a", "derived", 2).unwrap());
        assert!(!affected(&tx, snapshot.id.as_str(), "task-a", "foreign", 1).unwrap());
        assert!(!affected(&tx, snapshot.id.as_str(), "task-a", "near-prefix", 1).unwrap());
    }
}

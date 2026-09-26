//! Active consumer generations. Historical rows stay; retirement does not delete them.
use super::*;
use rusqlite::OptionalExtension;

const SCHEMA_VERSION: u32 = 37;
const WITHOUT_SUCCESSOR: &str = "retired without a successor";
const SUCCESSOR_MISSING: &str = "successor binding is missing";
const SUCCESSOR_INACTIVE: &str = "successor binding is not active";
const SUCCESSOR_RETIRED: &str = "successor binding is retired";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsumerBinding {
    pub binding_id: String,
    pub consumer_id: String,
    pub generation: u64,
    pub snapshot_id: String,
    pub attempt_id: Option<String>,
    pub task_id: Option<String>,
    pub active: bool,
    pub retired: bool,
    pub successor_binding_id: Option<String>,
}

fn invalid(message: &str) -> StoreError {
    StoreError::Invalid(message.into())
}

fn require_schema(db: &Connection) -> Result<()> {
    check_schema(db)?;
    let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchema(version));
    }
    Ok(())
}

fn binding_identity(consumer_id: &str, generation: i64, snapshot_id: &str) -> String {
    format!(
        "{:x}",
        Sha256::digest(format!("{consumer_id}\0{generation}\0{snapshot_id}").as_bytes())
    )
}

fn validate_consumer(consumer_id: &str) -> Result<()> {
    if !(1..=256).contains(&consumer_id.len()) {
        return Err(invalid("invalid consumer binding"));
    }
    Ok(())
}

fn validate_binding_id(binding_id: &str) -> Result<()> {
    if binding_id.len() != 64 || !binding_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid("invalid consumer binding"));
    }
    Ok(())
}

/// Whether this snapshot would have been a subscription recipient, and the live
/// attempt when that attempt is what makes the snapshot current.
fn route_state(db: &Connection, snapshot_id: &str) -> Result<(bool, Option<String>)> {
    let row: Option<(String, Option<String>, Option<String>)> = db
        .query_row(
            "SELECT m.task_id,t.state,t.active_attempt FROM memory_snapshots m
             LEFT JOIN tasks t ON t.id=m.task_id WHERE m.id=?1",
            [snapshot_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((task_id, state, active_attempt)) = row else {
        return Err(invalid("memory snapshot is missing"));
    };
    if task_id == "coordinator" {
        return Ok((true, None));
    }
    let Some(state) = state else {
        return Ok((false, None));
    };
    if matches!(state.as_str(), "succeeded" | "failed" | "cancelled") {
        return Ok((false, None));
    }
    let Some(active_attempt) = active_attempt else {
        return Ok((true, None));
    };
    let live: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM attempts a WHERE a.id=?1 AND a.task_id=?2 AND a.snapshot=?3
         AND a.termination_observed=0 AND a.state IN ('reserved','launching','running','awaiting_input'))",
        params![active_attempt, task_id, snapshot_id],
        |row| row.get(0),
    )?;
    if live {
        Ok((true, Some(active_attempt)))
    } else {
        Ok((false, None))
    }
}

fn binding_from_row(row: &rusqlite::Row) -> rusqlite::Result<ConsumerBinding> {
    let generation = u64::try_from(row.get::<_, i64>(2)?).unwrap_or(0);
    Ok(ConsumerBinding {
        binding_id: row.get(0)?,
        consumer_id: row.get(1)?,
        generation,
        snapshot_id: row.get(3)?,
        attempt_id: row.get(4)?,
        task_id: row.get(5)?,
        active: row.get::<_, i64>(6)? == 1,
        retired: row.get::<_, i64>(7)? == 1,
        successor_binding_id: row.get(8)?,
    })
}

fn load(db: &Connection, binding_id: &str) -> Result<Option<ConsumerBinding>> {
    db.query_row(
        "SELECT binding_id,consumer_id,generation,snapshot_id,attempt_id,task_id,active,retired,successor_binding_id
         FROM consumer_bindings WHERE binding_id=?1",
        [binding_id],
        binding_from_row,
    )
    .optional()
    .map_err(Into::into)
}

fn load_by_snapshot(db: &Connection, snapshot_id: &str) -> Result<Option<ConsumerBinding>> {
    db.query_row(
        "SELECT binding_id,consumer_id,generation,snapshot_id,attempt_id,task_id,active,retired,successor_binding_id
         FROM consumer_bindings WHERE snapshot_id=?1",
        [snapshot_id],
        binding_from_row,
    )
    .optional()
    .map_err(Into::into)
}

fn insert_binding(
    tx: &rusqlite::Transaction,
    consumer_id: &str,
    snapshot_id: &str,
    task_id: Option<&str>,
    attempt_id: Option<&str>,
    generation: i64,
    active: bool,
) -> Result<()> {
    validate_consumer(consumer_id)?;
    if snapshot_id.is_empty() || generation <= 0 {
        return Err(invalid("invalid consumer binding"));
    }
    if attempt_id.is_some() && task_id.is_none() {
        return Err(invalid("invalid consumer binding"));
    }
    let now = jiff::Timestamp::now().as_millisecond();
    tx.execute(
        "INSERT INTO consumer_bindings(binding_id,consumer_id,generation,snapshot_id,attempt_id,task_id,active,retired,successor_binding_id,created_unix_ms)
         VALUES(?1,?2,?3,?4,?5,?6,?7,0,NULL,?8)",
        params![
            binding_identity(consumer_id, generation, snapshot_id),
            consumer_id,
            generation,
            snapshot_id,
            attempt_id,
            task_id,
            i64::from(active),
            now,
        ],
    )?;
    Ok(())
}

pub(super) fn ensure_for_snapshot(
    tx: &rusqlite::Transaction,
    consumer_id: &str,
    snapshot_id: &str,
    task_id: Option<&str>,
) -> Result<()> {
    let version: u32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < SCHEMA_VERSION {
        return Ok(());
    }
    if load_by_snapshot(tx, snapshot_id)?.is_some() {
        return Ok(());
    }
    let generation: i64 = tx.query_row(
        "SELECT coalesce(max(generation),0)+1 FROM consumer_bindings WHERE consumer_id=?1",
        [consumer_id],
        |row| row.get(0),
    )?;
    let (routes, live_attempt) = route_state(tx, snapshot_id)?;
    let attempt = if task_id.is_some() {
        live_attempt.as_deref()
    } else {
        None
    };
    insert_binding(
        tx,
        consumer_id,
        snapshot_id,
        task_id,
        attempt,
        generation,
        routes,
    )
}

pub(super) fn backfill_from_subscriptions(tx: &rusqlite::Transaction) -> Result<()> {
    let mut stmt = tx.prepare(
        "SELECT s.subscriber,s.snapshot_id,m.task_id FROM memory_subscriptions s
         JOIN memory_snapshots m ON m.id=s.snapshot_id ORDER BY s.subscriber,s.id",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(stmt);
    for (subscriber, snapshot, task) in rows {
        if load_by_snapshot(tx, &snapshot)?.is_some() {
            continue;
        }
        let generation: i64 = tx.query_row(
            "SELECT coalesce(max(generation),0)+1 FROM consumer_bindings WHERE consumer_id=?1",
            [&subscriber],
            |row| row.get(0),
        )?;
        let (routes, live_attempt) = route_state(tx, &snapshot)?;
        let task_id = if task == "coordinator" {
            None
        } else {
            Some(task)
        };
        let attempt = if task_id.is_some() {
            live_attempt
        } else {
            None
        };
        insert_binding(
            tx,
            &subscriber,
            &snapshot,
            task_id.as_deref(),
            attempt.as_deref(),
            generation,
            routes,
        )?;
    }
    Ok(())
}

/// O(non-retired bindings) per change. Move next to attempt and task writes
/// once those updates share one function.
pub(super) fn reconcile_active(tx: &rusqlite::Transaction) -> Result<()> {
    let version: u32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < SCHEMA_VERSION {
        return Ok(());
    }
    let mut stmt = tx.prepare(
        "SELECT binding_id,snapshot_id,active,attempt_id FROM consumer_bindings WHERE retired=0",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(stmt);
    for (id, snapshot, active, attempt) in rows {
        let (routes, live_attempt) = route_state(tx, &snapshot)?;
        let next = i64::from(routes);
        if active != next {
            tx.execute(
                "UPDATE consumer_bindings SET active=?2 WHERE binding_id=?1",
                params![id, next],
            )?;
        }
        if let (None, Some(live)) = (attempt, live_attempt) {
            tx.execute(
                "UPDATE consumer_bindings SET attempt_id=?2 WHERE binding_id=?1 AND attempt_id IS NULL AND task_id IS NOT NULL",
                params![id, live],
            )?;
        }
    }
    Ok(())
}

fn unresolved_deliveries(
    tx: &rusqlite::Transaction,
    binding: &ConsumerBinding,
) -> Result<Vec<String>> {
    let mut stmt = tx.prepare(
        "SELECT id FROM (
            SELECT d.id AS id FROM memory_delivery_intents d
            WHERE d.snapshot_id=?1
            AND NOT EXISTS(SELECT 1 FROM memory_update_receipts r WHERE r.delivery_id=d.id AND r.state='applied')
            UNION
            SELECT o.delivery_id AS id FROM consumer_binding_obligations o
            WHERE o.binding_id=?2
            AND NOT EXISTS(SELECT 1 FROM memory_update_receipts r WHERE r.delivery_id=o.delivery_id AND r.state='applied')
         ) ORDER BY id LIMIT 10001",
    )?;
    let rows = stmt
        .query_map(params![binding.snapshot_id, binding.binding_id], |row| {
            row.get(0)
        })?
        .collect::<std::result::Result<Vec<String>, _>>()?;
    if rows.len() > 10_000 {
        return Err(StoreError::Limit(
            "unresolved obligations exceed 10000".into(),
        ));
    }
    Ok(rows)
}

impl SqliteStore {
    pub fn consumer_binding(&mut self, binding_id: &str) -> Result<Option<ConsumerBinding>> {
        require_schema(&self.connection)?;
        validate_binding_id(binding_id)?;
        load(&self.connection, binding_id)
    }

    pub fn consumer_binding_for_snapshot(
        &mut self,
        snapshot_id: &str,
    ) -> Result<Option<ConsumerBinding>> {
        require_schema(&self.connection)?;
        if snapshot_id.is_empty() {
            return Err(invalid("memory snapshot is missing"));
        }
        load_by_snapshot(&self.connection, snapshot_id)
    }

    pub fn binding_obligations(&mut self, binding_id: &str) -> Result<Vec<String>> {
        require_schema(&self.connection)?;
        validate_binding_id(binding_id)?;
        if load(&self.connection, binding_id)?.is_none() {
            return Err(invalid("consumer binding is missing"));
        }
        let mut stmt = self.connection.prepare(
            "SELECT delivery_id FROM consumer_binding_obligations WHERE binding_id=?1 ORDER BY delivery_id LIMIT 10001",
        )?;
        let rows = stmt
            .query_map([binding_id], |row| row.get(0))?
            .collect::<std::result::Result<Vec<String>, _>>()?;
        if rows.len() > 10_000 {
            return Err(StoreError::Limit(
                "consumer binding obligations exceed 10000".into(),
            ));
        }
        Ok(rows)
    }

    pub fn binding_undeliverable(&mut self, binding_id: &str) -> Result<Vec<(String, String)>> {
        require_schema(&self.connection)?;
        validate_binding_id(binding_id)?;
        if load(&self.connection, binding_id)?.is_none() {
            return Err(invalid("consumer binding is missing"));
        }
        let mut stmt = self.connection.prepare(
            "SELECT delivery_id,reason FROM consumer_binding_undeliverable WHERE binding_id=?1 ORDER BY delivery_id LIMIT 10001",
        )?;
        let rows = stmt
            .query_map([binding_id], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if rows.len() > 10_000 {
            return Err(StoreError::Limit(
                "undeliverable obligations exceed 10000".into(),
            ));
        }
        Ok(rows)
    }

    /// Keep unresolved obligations addressable on the successor. Nothing is deleted.
    pub fn retire_consumer_binding(
        &mut self,
        binding_id: &str,
        successor: Option<&str>,
    ) -> Result<()> {
        validate_binding_id(binding_id)?;
        if let Some(successor) = successor {
            validate_binding_id(successor)?;
            if successor == binding_id {
                return Err(invalid("invalid successor binding"));
            }
        }
        let tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        require_schema(&tx)?;
        let Some(binding) = load(&tx, binding_id)? else {
            return Err(invalid("consumer binding is missing"));
        };
        if binding.retired {
            if binding.successor_binding_id.as_deref() == successor {
                tx.commit()?;
                return Ok(());
            }
            return Err(StoreError::Conflict);
        }
        let unresolved = unresolved_deliveries(&tx, &binding)?;
        let move_to = match successor {
            None => Err(WITHOUT_SUCCESSOR),
            Some(id) => match load(&tx, id)? {
                None => Err(SUCCESSOR_MISSING),
                Some(row) if row.retired => Err(SUCCESSOR_RETIRED),
                Some(row) if !row.active => Err(SUCCESSOR_INACTIVE),
                Some(_) => Ok(id),
            },
        };
        match move_to {
            Ok(successor_id) => {
                for delivery in &unresolved {
                    tx.execute(
                        "INSERT INTO consumer_binding_obligations(binding_id,delivery_id) VALUES(?1,?2)",
                        params![successor_id, delivery],
                    )?;
                }
                tx.execute(
                    "UPDATE consumer_bindings SET retired=1,active=0,successor_binding_id=?2 WHERE binding_id=?1",
                    params![binding.binding_id, successor_id],
                )?;
            }
            Err(reason) => {
                for delivery in &unresolved {
                    tx.execute(
                        "INSERT INTO consumer_binding_undeliverable(binding_id,delivery_id,reason) VALUES(?1,?2,?3)",
                        params![binding.binding_id, delivery, reason],
                    )?;
                }
                tx.execute(
                    "UPDATE consumer_bindings SET retired=1,active=0 WHERE binding_id=?1",
                    [&binding.binding_id],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        Applicability, Attempt, AttemptId, AttemptState, Commit, ControlContext, MemoryKind,
        MemoryRecordId, Mutation, NewRevision, SnapshotRequest, Task, TaskId, TaskState,
    };
    use crate::memory::MemoryStore;
    use std::collections::BTreeSet;

    fn user_version(db: &Connection) -> u32 {
        db.query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }

    fn count(db: &Connection, sql: &str) -> i64 {
        db.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    fn task(id: &str) -> Mutation {
        Mutation::Task {
            expected: None,
            next: Task {
                id: TaskId::new(id).unwrap(),
                revision: 1,
                state: TaskState::Draft,
                title: id.into(),
                active_attempt: None,
            },
        }
    }

    fn digest() -> String {
        "a".repeat(64)
    }

    #[test]
    fn upgrade_v1_from_36_preserves_receipts_and_create_ends_at_37() {
        let fresh = tempfile::tempdir().unwrap();
        let created = SqliteStore::create(&fresh.path().join("state.db")).unwrap();
        assert_eq!(user_version(&created.connection), 37);
        assert_eq!(
            count(
                &created.connection,
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='consumer_bindings'"
            ),
            1
        );
        let index_sql: String = created
            .connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name='consumer_bindings_by_generation'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(index_sql.contains("consumer_id"));
        assert!(index_sql.contains("generation"));
        assert!(index_sql.contains("active"));
        let open_fn = include_str!("mod.rs")
            .split("pub fn open")
            .nth(1)
            .unwrap()
            .split("pub fn integrity_check")
            .next()
            .unwrap();
        assert!(!open_fn.contains("upgrade_v1"));
        assert!(!open_fn.contains("0037_consumer_bindings"));
        drop(created);

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.db");
        let mut db = SqliteStore::create(&path).unwrap();
        let tx = db.connection.transaction().unwrap();
        tx.execute(
            "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('fixture','fact',1,1,'{}')",
            [],
        )
        .unwrap();
        let sequence: i64 = tx
            .query_row("SELECT max(sequence) FROM events", [], |row| row.get(0))
            .unwrap();
        let hash = "a".repeat(64);
        tx.execute(
            "INSERT INTO objects(hash,size,availability,collection,pin_count,fencing_token) VALUES(?1,1,'available','unclaimed',0,0)",
            [&hash],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO memory_records(id,record_key,scope_id,kind,is_hard) VALUES('fact','fact','project','observation',0)",
            [],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO memory_revisions(record_id,revision,body_hash,provenance_hash,promoted_seq,applicability) VALUES('fact',1,?1,?1,?2,'{\"domains\":[],\"paths\":[]}')",
            params![hash, sequence],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO attempts(id,task_id,revision,state,snapshot,reservation,termination_observed) VALUES('attempt-1','worker',1,'running','snap-1','slot',0)",
            [],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO tasks(id,revision,state,title,active_attempt) VALUES('worker',1,'running','Worker','attempt-1')",
            [],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO memory_snapshots(id,task_id,task_revision,profile_name,profile_digest,config_digest,selection_policy_version,estimator,sequence,required_bytes,optional_bytes,budget_bytes,omitted_optional_count,manifest_hash,scope_digest) VALUES('snap-1','worker',1,'worker',?1,NULL,1,'test',1,0,0,0,0,?2,?3)",
            params!["b".repeat(64), "c".repeat(64), "d".repeat(64)],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO memory_subscriptions(id,subscriber,snapshot_id,since_seq) VALUES('sub-1','task:worker','snap-1',1)",
            [],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO memory_delivery_intents(id,cause_id,subscriber,snapshot_id,task_id,record_id,revision,severity,triggering_seq,state) VALUES('delivery-1','cause','task:worker','snap-1','worker','fact',1,'informational',?1,'pending')",
            [sequence],
        )
        .unwrap();
        let manifest = "e".repeat(64);
        tx.execute(
            "INSERT INTO memory_update_receipts(delivery_id,attempt_id,state,manifest_hash,sequence) VALUES('delivery-1','attempt-1','seen',?1,?2)",
            params![manifest, sequence],
        )
        .unwrap();
        tx.commit().unwrap();
        let receipt_before: (String, String, String, String, i64) = db
            .connection
            .query_row(
                "SELECT delivery_id,attempt_id,state,manifest_hash,sequence FROM memory_update_receipts",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .unwrap();
        let snapshot_before: (String, String, String) = db
            .connection
            .query_row(
                "SELECT id,manifest_hash,scope_digest FROM memory_snapshots",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        let intent_before: (String, String, String) = db
            .connection
            .query_row(
                "SELECT id,snapshot_id,state FROM memory_delivery_intents",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        let attempts_before = count(&db.connection, "SELECT count(*) FROM attempts");
        drop(db);
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.execute_batch(
            "DROP TABLE IF EXISTS consumer_binding_undeliverable; DROP TABLE IF EXISTS consumer_binding_obligations; DROP TABLE IF EXISTS consumer_bindings; UPDATE store_meta SET schema_version=36; PRAGMA user_version=36;",
        )
        .unwrap();
        drop(raw);
        let mut db = SqliteStore::open(&path).unwrap();
        assert_eq!(user_version(&db.connection), 36);
        assert_eq!(
            db.memory_update_receipts("attempt-1").unwrap(),
            vec![crate::domain::MemoryUpdateReceipt {
                delivery_id: "delivery-1".into(),
                attempt_id: "attempt-1".into(),
                state: "seen".into(),
                manifest_hash: manifest.clone(),
                sequence: sequence as u64,
            }]
        );
        db.upgrade_v1().unwrap();
        assert_eq!(user_version(&db.connection), 37);
        assert_eq!(
            db.connection
                .query_row("SELECT schema_version FROM store_meta", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            37
        );
        let receipt_after: (String, String, String, String, i64) = db
            .connection
            .query_row(
                "SELECT delivery_id,attempt_id,state,manifest_hash,sequence FROM memory_update_receipts",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .unwrap();
        let snapshot_after: (String, String, String) = db
            .connection
            .query_row(
                "SELECT id,manifest_hash,scope_digest FROM memory_snapshots",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        let intent_after: (String, String, String) = db
            .connection
            .query_row(
                "SELECT id,snapshot_id,state FROM memory_delivery_intents",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(receipt_before, receipt_after);
        assert_eq!(snapshot_before, snapshot_after);
        assert_eq!(intent_before, intent_after);
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM attempts"),
            attempts_before
        );
        assert_eq!(
            db.memory_update_receipts("attempt-1").unwrap()[0].manifest_hash,
            manifest
        );
        let binding = db.consumer_binding_for_snapshot("snap-1").unwrap().unwrap();
        assert!(binding.active);
        assert_eq!(binding.attempt_id.as_deref(), Some("attempt-1"));
        assert!(!binding.retired);
        check_schema(&db.connection).unwrap();
        drop(db);
        let reopened = SqliteStore::open(&path).unwrap();
        assert_eq!(user_version(&reopened.connection), 37);
    }

    #[test]
    fn coordinator_binding_is_addressable_without_an_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let mut db = SqliteStore::create(&path).unwrap();
        db.commit(Commit {
            expected_head: 0,
            mutations: vec![task("other")],
        })
        .unwrap();
        let attempts_before = count(&db.connection, "SELECT count(*) FROM attempts");
        let mut memory = MemoryStore::from_sqlite(db, dir.path().join("objects"));
        let snapshot = memory
            .create_coordinator_snapshot(
                "session-a",
                "planner",
                &digest(),
                None,
                32_000,
                "Coordinate",
                1,
            )
            .unwrap();
        let binding = memory
            .store
            .consumer_binding_for_snapshot(snapshot.id.as_str())
            .unwrap()
            .unwrap();
        assert!(binding.attempt_id.is_none());
        assert!(binding.task_id.is_none());
        assert!(binding.active);
        assert_eq!(binding.consumer_id, "coordinator:session-a");
        assert_eq!(binding.generation, 1);
        let loaded = memory
            .store
            .consumer_binding(&binding.binding_id)
            .unwrap()
            .unwrap();
        assert_eq!(loaded, binding);
        assert_eq!(
            count(&memory.store.connection, "SELECT count(*) FROM attempts"),
            attempts_before
        );
    }

    #[test]
    fn shadow_compare_drops_no_required_recipient() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let mut db = SqliteStore::create(&path).unwrap();
        db.commit(Commit {
            expected_head: 0,
            mutations: vec![task("worker-a")],
        })
        .unwrap();
        let mut memory = MemoryStore::from_sqlite(db, dir.path().join("objects"));
        let request = |instructions_task: &str| SnapshotRequest {
            schema_version: 1,
            task_id: instructions_task.into(),
            profile: "worker".into(),
            domains: vec![],
            paths: vec![],
            pinned_keys: vec![],
            sensitivity: "default".into(),
        };
        let live = memory
            .create_task_snapshot(
                request("worker-a"),
                "worker",
                &digest(),
                None,
                32_000,
                "live instructions",
                1,
                None,
            )
            .unwrap();
        let unused = memory
            .create_task_snapshot(
                request("worker-a"),
                "worker",
                &digest(),
                None,
                32_000,
                "unused instructions",
                1,
                None,
            )
            .unwrap();
        let coordinator = memory
            .create_coordinator_snapshot(
                "session-a",
                "planner",
                &digest(),
                None,
                32_000,
                "Coordinate",
                1,
            )
            .unwrap();
        let head = memory.store.read_snapshot(None).unwrap().head;
        memory
            .store
            .commit(Commit {
                expected_head: head,
                mutations: vec![
                    Mutation::Attempt {
                        expected: None,
                        next: Attempt {
                            id: AttemptId::new("attempt-live").unwrap(),
                            task: TaskId::new("worker-a").unwrap(),
                            revision: 1,
                            state: AttemptState::Running,
                            snapshot: Some(live.id.as_str().into()),
                            reservation: "slot".into(),
                            termination_observed: false,
                        },
                    },
                    Mutation::Task {
                        expected: Some(1),
                        next: Task {
                            id: TaskId::new("worker-a").unwrap(),
                            revision: 2,
                            state: TaskState::Running,
                            title: "worker-a".into(),
                            active_attempt: Some(AttemptId::new("attempt-live").unwrap()),
                        },
                    },
                ],
            })
            .unwrap();
        let tx = memory.store.connection.transaction().unwrap();
        reconcile_active(&tx).unwrap();
        let old = super::super::memory_delivery::legacy_subscription_recipients(&tx).unwrap();
        let new = super::super::memory_delivery::active_binding_recipients(&tx).unwrap();
        let key = |row: &super::super::memory_delivery::RoutingRecipient| {
            (
                row.subscriber.clone(),
                row.snapshot_id.clone(),
                row.task_id.clone(),
            )
        };
        let old_keys: BTreeSet<_> = old.iter().map(key).collect();
        let new_keys: BTreeSet<_> = new.iter().map(key).collect();
        assert!(
            old_keys.iter().all(|row| new_keys.contains(row)),
            "required recipient only on the subscription side: {:?}",
            old_keys.difference(&new_keys).collect::<Vec<_>>()
        );
        assert_eq!(old_keys, new_keys);
        assert!(old.iter().any(|row| row.snapshot_id == live.id.as_str()));
        assert!(
            old.iter()
                .any(|row| row.snapshot_id == coordinator.id.as_str())
        );
        assert!(!old.iter().any(|row| row.snapshot_id == unused.id.as_str()));
        tx.rollback().unwrap();
        let hot = include_str!("memory_delivery.rs")
            .split("pub(super) fn record_change")
            .nth(1)
            .unwrap()
            .split("\nimpl SqliteStore")
            .next()
            .unwrap();
        assert!(!hot.contains("memory_subscriptions"));
        assert!(hot.contains("active_binding_recipients"));
        let named = memory
            .store
            .consumer_binding_for_snapshot(live.id.as_str())
            .unwrap()
            .unwrap();
        assert_eq!(named.attempt_id.as_deref(), Some("attempt-live"));
        assert_eq!(
            count(&memory.store.connection, "SELECT count(*) FROM attempts"),
            1
        );
        let coordinator_binding = memory
            .store
            .consumer_binding_for_snapshot(coordinator.id.as_str())
            .unwrap()
            .unwrap();
        assert!(coordinator_binding.attempt_id.is_none());
        let unused_binding = memory
            .store
            .consumer_binding_for_snapshot(unused.id.as_str())
            .unwrap()
            .unwrap();
        assert!(!unused_binding.active);
        assert!(!unused_binding.retired);
    }

    fn put_fact(memory: &mut MemoryStore) {
        let body = memory.ingest_object(&b"fact"[..]).unwrap();
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
                    validity_reason: "test".into(),
                },
            )
            .unwrap();
    }

    fn worker_with_fact() -> (tempfile::TempDir, MemoryStore, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let mut db = SqliteStore::create(&path).unwrap();
        db.commit(Commit {
            expected_head: 0,
            mutations: vec![task("worker-a")],
        })
        .unwrap();
        let mut memory = MemoryStore::from_sqlite(db, dir.path().join("objects"));
        put_fact(&mut memory);
        let snapshot = memory
            .create_task_snapshot(
                SnapshotRequest {
                    schema_version: 1,
                    task_id: "worker-a".into(),
                    profile: "worker".into(),
                    domains: vec![],
                    paths: vec![],
                    pinned_keys: vec!["fact".into()],
                    sensitivity: "default".into(),
                },
                "worker",
                &digest(),
                None,
                32_000,
                "instructions",
                1,
                None,
            )
            .unwrap();
        (dir, memory, snapshot.id.as_str().to_string())
    }

    fn route(memory: &mut MemoryStore, cause: &str) {
        let sequence = count(&memory.store.connection, "SELECT max(sequence) FROM events") as u64;
        let tx = memory
            .store
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        super::super::memory_delivery::record_change(
            &tx,
            cause,
            "fact",
            1,
            "informational",
            sequence,
        )
        .unwrap();
        tx.commit().unwrap();
    }

    fn intent_snapshots(memory: &MemoryStore, cause: &str) -> Vec<String> {
        let mut stmt = memory
            .store
            .connection
            .prepare("SELECT snapshot_id FROM memory_delivery_intents WHERE cause_id=?1 ORDER BY snapshot_id")
            .unwrap();
        stmt.query_map([cause], |row| row.get(0))
            .unwrap()
            .map(|row| row.unwrap())
            .collect()
    }

    #[test]
    fn retired_snapshot_gets_no_new_obligation_and_pending_stays_on_successor() {
        let (dir, mut memory, retired_snapshot) = worker_with_fact();
        let _ = dir;
        route(&mut memory, "change-1");
        assert_eq!(
            intent_snapshots(&memory, "change-1"),
            vec![retired_snapshot.clone()]
        );
        let pending = memory.store.memory_delivery_intents().unwrap();
        assert_eq!(pending.len(), 1);
        let delivery = pending[0]["id"].as_str().unwrap().to_string();
        let successor = memory
            .create_task_snapshot(
                SnapshotRequest {
                    schema_version: 1,
                    task_id: "worker-a".into(),
                    profile: "worker".into(),
                    domains: vec![],
                    paths: vec![],
                    pinned_keys: vec!["fact".into()],
                    sensitivity: "default".into(),
                },
                "worker",
                &digest(),
                None,
                32_000,
                "successor instructions",
                1,
                None,
            )
            .unwrap();
        let retired = memory
            .store
            .consumer_binding_for_snapshot(&retired_snapshot)
            .unwrap()
            .unwrap();
        let next = memory
            .store
            .consumer_binding_for_snapshot(successor.id.as_str())
            .unwrap()
            .unwrap();
        let snapshots_before = count(
            &memory.store.connection,
            "SELECT count(*) FROM memory_snapshots",
        );
        memory
            .store
            .retire_consumer_binding(&retired.binding_id, Some(&next.binding_id))
            .unwrap();
        memory
            .store
            .retire_consumer_binding(&retired.binding_id, Some(&next.binding_id))
            .unwrap();
        assert!(matches!(
            memory
                .store
                .retire_consumer_binding(&retired.binding_id, None),
            Err(StoreError::Conflict)
        ));
        let retired = memory
            .store
            .consumer_binding(&retired.binding_id)
            .unwrap()
            .unwrap();
        assert!(retired.retired);
        assert!(!retired.active);
        assert_eq!(
            retired.successor_binding_id.as_deref(),
            Some(next.binding_id.as_str())
        );
        assert_eq!(
            count(
                &memory.store.connection,
                "SELECT count(*) FROM memory_snapshots"
            ),
            snapshots_before
        );
        let still: i64 = memory
            .store
            .connection
            .query_row(
                "SELECT count(*) FROM memory_snapshots WHERE id=?1",
                [&retired_snapshot],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(still, 1);
        assert!(
            memory
                .store
                .binding_obligations(&next.binding_id)
                .unwrap()
                .contains(&delivery)
        );
        assert_eq!(
            memory
                .store
                .connection
                .query_row(
                    "SELECT snapshot_id,state FROM memory_delivery_intents WHERE id=?1",
                    [&delivery],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .unwrap(),
            (retired_snapshot.clone(), "pending".into())
        );
        route(&mut memory, "change-2");
        assert!(
            intent_snapshots(&memory, "change-2")
                .iter()
                .all(|snapshot| snapshot != &retired_snapshot)
        );
        assert!(
            intent_snapshots(&memory, "change-2")
                .iter()
                .any(|snapshot| snapshot == successor.id.as_str())
        );
        assert_eq!(
            intent_snapshots(&memory, "change-1"),
            vec![retired_snapshot]
        );
    }

    #[test]
    fn retirement_without_successor_records_why_it_is_undeliverable() {
        let (dir, mut memory, snapshot) = worker_with_fact();
        let _ = dir;
        route(&mut memory, "change-1");
        let delivery = memory.store.memory_delivery_intents().unwrap()[0]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let binding = memory
            .store
            .consumer_binding_for_snapshot(&snapshot)
            .unwrap()
            .unwrap();
        memory
            .store
            .retire_consumer_binding(&binding.binding_id, None)
            .unwrap();
        memory
            .store
            .retire_consumer_binding(&binding.binding_id, None)
            .unwrap();
        assert_eq!(
            memory
                .store
                .binding_undeliverable(&binding.binding_id)
                .unwrap(),
            vec![(delivery.clone(), WITHOUT_SUCCESSOR.into())]
        );
        assert_eq!(
            memory
                .store
                .connection
                .query_row(
                    "SELECT count(*) FROM memory_delivery_intents WHERE id=?1",
                    [&delivery],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        route(&mut memory, "change-2");
        assert!(intent_snapshots(&memory, "change-2").is_empty());
        assert_eq!(intent_snapshots(&memory, "change-1"), vec![snapshot]);
    }
}

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
fn route_state(db: &Connection, snapshot_id: &str) -> Result<(bool, Option<String>)> {route_state_with_budget(db,snapshot_id,None)}
fn route_state_with_budget(db:&Connection,snapshot_id:&str,budget:Option<&read_budget::ReadBudget>)->Result<(bool,Option<String>)> {
    let mut stmt=db.prepare("SELECT m.task_id,t.state,t.active_attempt FROM memory_snapshots m LEFT JOIN tasks t ON t.id=m.task_id WHERE m.id=?1")?;
    let mut rows=stmt.query([snapshot_id])?;
    let row:Option<(String,Option<String>,Option<String>)>=if let Some(row)=rows.next()? {
        if let Some(budget)=budget {budget.row(row,&[])?;}
        Some((row.get(0)?,row.get(1)?,row.get(2)?))
    } else {None};
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
         JOIN memory_snapshots m ON m.id=s.snapshot_id ORDER BY m.sequence,s.id",
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
        let task_id = if task == "coordinator" {
            None
        } else {
            Some(task.as_str())
        };
        ensure_for_snapshot(tx, &subscriber, &snapshot, task_id)?;
    }
    Ok(())
}

/// Must see attempt and task rows already written in this transaction.
pub(super) fn reconcile_active(tx: &Connection) -> Result<()> {reconcile_selected(tx,None,None)}
pub(super) fn reconcile_task(tx:&Connection,task:&str,budget:Option<&read_budget::ReadBudget>)->Result<()> {reconcile_selected(tx,Some(task),budget)}
fn reconcile_selected(tx:&Connection,task:Option<&str>,budget:Option<&read_budget::ReadBudget>)->Result<()> {
    let version: u32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < SCHEMA_VERSION {
        return Ok(());
    }
    let mut stmt = tx.prepare(if task.is_some() {
        "SELECT binding_id,snapshot_id,active,attempt_id FROM consumer_bindings WHERE retired=0 AND task_id=?1"
    } else {"SELECT binding_id,snapshot_id,active,attempt_id FROM consumer_bindings WHERE retired=0 AND ?1 IS NULL"})?;
    let mut rows=stmt.query([task])?;let mut bindings=Vec::new();
    while let Some(row)=rows.next()? {
        if let Some(budget)=budget {budget.row(row,&[])?;}
        bindings.push((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,i64>(2)?,row.get::<_,Option<String>>(3)?));
    }
    drop(rows);drop(stmt);
    for (id, snapshot, active, attempt) in bindings {
        let (routes, live_attempt) = route_state_with_budget(tx, &snapshot,budget)?;
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
        reconcile_active(&tx)?;
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

    fn user_version(db: &Connection) -> u32 {
        db.query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }

    fn count(db: &Connection, sql: &str) -> i64 {
        db.query_row(sql, [], |row| row.get(0)).unwrap()
    }



    #[test]
    fn upgrade_v1_from_36_preserves_receipts_and_create_ends_at_37() {
        let fresh = tempfile::tempdir().unwrap();
        let created = SqliteStore::create(&fresh.path().join("state.db")).unwrap();
        assert_eq!(user_version(&created.connection), crate::store::SCHEMA);
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
        crate::store::test_schema::historical(&raw, 36)
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
        assert_eq!(user_version(&db.connection), crate::store::SCHEMA);
        assert_eq!(
            db.connection
                .query_row("SELECT schema_version FROM store_meta", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            crate::store::SCHEMA
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
        assert_eq!(user_version(&reopened.connection), crate::store::SCHEMA);
    }

    #[test]
    fn backfill_numbers_generations_by_snapshot_sequence() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.db");
        let mut db = SqliteStore::create(&path).unwrap();
        let tx = db.connection.transaction().unwrap();
        let digest = "b".repeat(64);
        let scope = "d".repeat(64);
        for (id, sequence, manifest) in [
            ("snap-old", 1, "c".repeat(64)),
            ("snap-new", 5, "e".repeat(64)),
        ] {
            tx.execute(
                "INSERT INTO memory_snapshots(id,task_id,task_revision,profile_name,profile_digest,config_digest,selection_policy_version,estimator,sequence,required_bytes,optional_bytes,budget_bytes,omitted_optional_count,manifest_hash,scope_digest) VALUES(?1,'coordinator',1,'planner',?2,NULL,1,'test',?3,0,0,0,0,?4,?5)",
                params![id, digest, sequence, manifest, scope],
            )
            .unwrap();
        }
        // Subscription ids sort opposite snapshot age. Generation must follow sequence.
        tx.execute(
            "INSERT INTO memory_subscriptions(id,subscriber,snapshot_id,since_seq) VALUES('sub-z','coordinator:session','snap-old',1),('sub-a','coordinator:session','snap-new',1)",
            [],
        )
        .unwrap();
        tx.commit().unwrap();
        drop(db);
        let raw = rusqlite::Connection::open(&path).unwrap();
        crate::store::test_schema::historical(&raw, 36)
        .unwrap();
        drop(raw);
        let mut db = SqliteStore::open(&path).unwrap();
        let attempts = count(&db.connection, "SELECT count(*) FROM attempts");
        db.upgrade_v1().unwrap();
        let older = db
            .consumer_binding_for_snapshot("snap-old")
            .unwrap()
            .unwrap();
        let newer = db
            .consumer_binding_for_snapshot("snap-new")
            .unwrap()
            .unwrap();
        assert_eq!(older.generation, 1);
        assert_eq!(newer.generation, 2);
        assert!(older.attempt_id.is_none() && newer.attempt_id.is_none());
        assert!(older.task_id.is_none() && newer.task_id.is_none());
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM attempts"),
            attempts
        );
    }



}

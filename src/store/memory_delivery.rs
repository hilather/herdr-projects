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

pub(super) struct RoutingRecipient {
    pub binding_id: Option<String>,
    pub subscriber: String,
    pub snapshot_id: String,
    pub task_id: String,
}

const RECIPIENT_CAP: usize = 10_000;

fn map_recipients(
    rows: impl Iterator<Item = rusqlite::Result<RoutingRecipient>>,
) -> Result<Vec<RoutingRecipient>> {
    let consumers = rows.collect::<std::result::Result<Vec<_>, _>>()?;
    if consumers.len() > RECIPIENT_CAP {
        return Err(StoreError::Limit(
            "memory routing exceeds 10000 recipients; promotion not committed".into(),
        ));
    }
    Ok(consumers)
}

/// Pre-binding recipient set. Schema 37 does not route through this scan.
pub(super) fn legacy_subscription_recipients(
    tx: &rusqlite::Transaction,
) -> Result<Vec<RoutingRecipient>> {
    let mut query = tx.prepare("SELECT s.subscriber,s.snapshot_id,m.task_id FROM memory_subscriptions s JOIN memory_snapshots m ON m.id=s.snapshot_id LEFT JOIN tasks t ON t.id=m.task_id WHERE m.task_id='coordinator' OR (t.state NOT IN ('succeeded','failed','cancelled') AND (t.active_attempt IS NULL OR EXISTS(SELECT 1 FROM attempts a WHERE a.id=t.active_attempt AND a.snapshot=m.id AND a.termination_observed=0 AND a.state IN ('reserved','launching','running','awaiting_input')))) ORDER BY s.id LIMIT 10001")?;
    map_recipients(query.query_map([], |row| {
        Ok(RoutingRecipient {
            binding_id: None,
            subscriber: row.get(0)?,
            snapshot_id: row.get(1)?,
            task_id: row.get(2)?,
        })
    })?)
}

pub(super) fn active_binding_recipients(
    tx: &rusqlite::Transaction,
) -> Result<Vec<RoutingRecipient>> {
    let mut query = tx.prepare(
        "SELECT b.binding_id,b.consumer_id,b.snapshot_id,m.task_id FROM consumer_bindings b
         JOIN memory_snapshots m ON m.id=b.snapshot_id
         WHERE b.active=1 ORDER BY b.consumer_id,b.generation LIMIT 10001",
    )?;
    map_recipients(query.query_map([], |row| {
        Ok(RoutingRecipient {
            binding_id: Some(row.get(0)?),
            subscriber: row.get(1)?,
            snapshot_id: row.get(2)?,
            task_id: row.get(3)?,
        })
    })?)
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
    let consumers = if version < 37 {
        legacy_subscription_recipients(tx)?
    } else {
        super::consumer_bindings::reconcile_active(tx)?;
        active_binding_recipients(tx)?
    };
    for recipient in consumers {
        if !affected(
            tx,
            &recipient.snapshot_id,
            &recipient.task_id,
            record,
            revision,
        )? {
            continue;
        }
        let task_id = if recipient.task_id == "coordinator" {
            None
        } else {
            Some(recipient.task_id)
        };
        let id = format!(
            "delivery-{:x}",
            Sha256::digest(
                serde_json::json!([
                    cause,
                    recipient.subscriber,
                    recipient.snapshot_id,
                    record,
                    revision
                ])
                .to_string()
                .as_bytes()
            )
        );
        tx.execute(
            "INSERT INTO memory_delivery_intents VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,'pending')",
            params![
                id,
                cause,
                recipient.subscriber,
                recipient.snapshot_id,
                task_id,
                record,
                integer(revision)?,
                severity,
                integer(sequence)?
            ],
        )?;
        if let Some(binding) = &recipient.binding_id {
            tx.execute(
                "INSERT INTO consumer_binding_obligations(binding_id,delivery_id) VALUES(?1,?2)",
                params![binding, id],
            )?;
        }
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

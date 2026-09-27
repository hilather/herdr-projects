//! Retire an optional delivery by referencing an exact newer worker declaration.
//! This does not acknowledge the old revision or resolve any invalidation.
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerUpdateSupersession {
    pub schema_version: u32,
    pub binding_id: String,
    pub change_id: String,
    pub replacement_change_id: String,
    pub replacement_manifest_hash: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerUpdateSupersessionReceipt {
    pub schema_version: u32,
    pub request: WorkerUpdateSupersession,
    pub attempt_id: String,
    pub consumer_binding_generation: u64,
    pub replacement_receipt_sequence: u64,
    pub sequence: u64,
}

fn invalid(message: &str) -> StoreError {
    StoreError::Invalid(message.into())
}
fn schema(db: &Connection) -> Result<()> {
    check_schema(db)?;
    let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < 43 {
        return Err(StoreError::UnsupportedSchema(version));
    }
    Ok(())
}
fn hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|c| c.is_ascii_hexdigit())
}
fn change(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128
}

fn evidence(receipt: &WorkerUpdateSupersessionReceipt) -> serde_json::Value {
    serde_json::json!({
        "schema_version": 1, "protocol": "worker-optional-supersession-v1",
        "request": receipt.request, "attempt_id": receipt.attempt_id,
        "consumer_binding_generation": receipt.consumer_binding_generation,
        "replacement_receipt_sequence": receipt.replacement_receipt_sequence,
    })
}

fn load(
    db: &Connection,
    binding: &str,
    change: &str,
) -> Result<Option<WorkerUpdateSupersessionReceipt>> {
    let row: Option<(String, String, String, u64, u64)> = db
        .query_row(
            "SELECT receipt,attempt_id,replacement_change_id,replacement_receipt_sequence,sequence
         FROM memory_update_supersessions WHERE binding_id=?1 AND change_id=?2",
            params![binding, change],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    let Some((raw, attempt, replacement, support, sequence)) = row else {
        return Ok(None);
    };
    let receipt: WorkerUpdateSupersessionReceipt = serde_json::from_str(&raw)
        .map_err(|_| StoreError::Corrupt("invalid supersession receipt".into()))?;
    if receipt.schema_version != 1
        || receipt.request.schema_version != 1
        || receipt.request.binding_id != binding
        || receipt.request.change_id != change
        || receipt.attempt_id != attempt
        || receipt.request.replacement_change_id != replacement
        || receipt.replacement_receipt_sequence != support
        || receipt.sequence != sequence
    {
        return Err(StoreError::Corrupt(
            "supersession receipt identity mismatch".into(),
        ));
    }
    let supported: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM consumer_bindings b JOIN memory_update_receipts r ON r.attempt_id=b.attempt_id
         WHERE b.binding_id=?1 AND b.generation=?2 AND b.attempt_id=?3 AND r.delivery_id=?4
         AND r.state='applied' AND r.manifest_hash=?5 AND r.sequence=?6)",
        params![binding, integer(receipt.consumer_binding_generation)?, attempt, replacement,
            receipt.request.replacement_manifest_hash, integer(support)?], |row| row.get(0),
    )?;
    if !supported {
        return Err(StoreError::Corrupt(
            "supersession source receipt is missing or mismatched".into(),
        ));
    }
    let payload: Option<String> = db
        .query_row(
            "SELECT payload FROM events WHERE sequence=?1 AND kind='memory.update_superseded'
         AND entity=?2 AND revision=1 AND payload_version=1",
            params![integer(sequence)?, binding],
            |row| row.get(0),
        )
        .optional()?;
    let payload = payload.and_then(|p| serde_json::from_str::<serde_json::Value>(&p).ok());
    if payload.as_ref() != Some(&evidence(&receipt)) {
        return Err(StoreError::Corrupt("supersession evidence mismatch".into()));
    }
    Ok(Some(receipt))
}

struct Prepared {
    replacement: MemoryUpdate,
    receipt: WorkerUpdateSupersessionReceipt,
    reused: bool,
}

fn prepare(
    db: &Connection,
    attempt: &str,
    request: &WorkerUpdateSupersession,
    now: i64,
) -> Result<Prepared> {
    schema(db)?;
    super::delivery::now_check(now)?;
    if request.schema_version != 1
        || !hash(&request.binding_id)
        || !hash(&request.replacement_manifest_hash)
        || !change(&request.change_id)
        || !change(&request.replacement_change_id)
        || request.change_id == request.replacement_change_id
        || request.reason.trim().is_empty()
        || request.reason.len() > 1024
        || AttemptId::new(attempt).is_err()
    {
        return Err(invalid("invalid worker supersession request"));
    }
    let identity: Option<(u64,String)> = db.query_row(
        "SELECT b.generation,b.snapshot_id FROM consumer_bindings b JOIN attempts a ON a.id=b.attempt_id AND a.task_id=b.task_id
         JOIN tasks t ON t.id=a.task_id AND t.active_attempt=a.id
         WHERE b.binding_id=?1 AND b.attempt_id=?2 AND b.active=1 AND b.retired=0
         AND a.snapshot=b.snapshot_id AND a.termination_observed=0 AND a.state IN ('running','awaiting_input')
         AND t.state NOT IN ('succeeded','failed','cancelled')",
        params![request.binding_id,attempt], |row| Ok((row.get(0)?,row.get(1)?)),
    ).optional()?;
    let (generation, snapshot) =
        identity.ok_or_else(|| invalid("supersession requires the current live worker binding"))?;
    let old = super::memory_receipts::update(db, &request.change_id, attempt)?;
    let replacement = super::memory_receipts::update(db, &request.replacement_change_id, attempt)?;
    for update in [&old, &replacement] {
        let belongs: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM consumer_binding_obligations WHERE binding_id=?1 AND delivery_id=?2)",
            params![request.binding_id,update.delivery_id], |row| row.get(0),
        )?;
        if !belongs || update.input_snapshot_id != snapshot {
            return Err(invalid(
                "supersession changes must belong to the exact binding and snapshot",
            ));
        }
    }
    if request.replacement_manifest_hash != replacement.manifest_hash
        || old.record_id != replacement.record_id
        || replacement.revision <= old.revision
    {
        return Err(invalid(
            "supersession requires an exact newer revision of the same record",
        ));
    }
    let support: Option<u64> = db.query_row(
        "SELECT sequence FROM memory_update_receipts WHERE delivery_id=?1 AND attempt_id=?2 AND state='applied' AND manifest_hash=?3",
        params![replacement.delivery_id,attempt,replacement.manifest_hash], |row| row.get(0),
    ).optional()?;
    let support = support.ok_or_else(|| {
        invalid("supersession requires the worker's exact applied replacement receipt")
    })?;
    if let Some(receipt) = load(db, &request.binding_id, &request.change_id)? {
        if receipt.request != *request || receipt.attempt_id != attempt {
            return Err(StoreError::Conflict);
        }
        if receipt.consumer_binding_generation != generation
            || receipt.replacement_receipt_sequence != support
        {
            return Err(StoreError::Corrupt(
                "supersession supporting receipt mismatch".into(),
            ));
        }
        return Ok(Prepared {
            replacement,
            receipt,
            reused: true,
        });
    }
    let optional: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM memory_records WHERE id=?1 AND is_hard=0 AND kind NOT IN ('constraint','hard_memory'))",
        [&old.record_id], |row| row.get(0),
    )?;
    if old.severity != "informational" || !optional {
        return Err(invalid(
            "mandatory changes require explicit reconciliation, not optional supersession",
        ));
    }
    let already_applied: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM memory_change_receipts WHERE binding_id=?1 AND change_id=?2 AND disposition='applied')",
        params![request.binding_id,request.change_id], |row| row.get(0),
    )?;
    if already_applied {
        return Err(invalid("change already has an applied logical receipt"));
    }
    let current: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM memory_heads h JOIN memory_validity v ON v.record_id=h.record_id AND v.revision=h.revision
         JOIN memory_revisions r ON r.record_id=h.record_id AND r.revision=h.revision JOIN objects o ON o.hash=r.body_hash
         WHERE h.record_id=?1 AND h.revision=?2 AND h.status='active' AND v.state='valid' AND o.availability='available'
         AND (v.expiry_unix_ms IS NULL OR v.expiry_unix_ms>?3))",
        params![replacement.record_id,integer(replacement.revision)?,now], |row| row.get(0),
    )?;
    if !current
        || !super::memory_invalidation::dependencies_current(
            db,
            &replacement.record_id,
            replacement.revision,
            now,
        )?
    {
        return Err(invalid(
            "superseding revision is no longer current and valid",
        ));
    }
    Ok(Prepared {
        replacement,
        receipt: WorkerUpdateSupersessionReceipt {
            schema_version: 1,
            request: request.clone(),
            attempt_id: attempt.into(),
            consumer_binding_generation: generation,
            replacement_receipt_sequence: support,
            sequence: 0,
        },
        reused: false,
    })
}

impl SqliteStore {
    pub(crate) fn worker_supersession_replacement(
        &mut self,
        attempt: &str,
        request: &WorkerUpdateSupersession,
        now: i64,
    ) -> Result<MemoryUpdate> {
        let tx = self.connection.transaction()?;
        Ok(prepare(&tx, attempt, request, now)?.replacement)
    }

    pub(crate) fn supersede_worker_update(
        &mut self,
        attempt: &str,
        request: &WorkerUpdateSupersession,
    ) -> Result<WorkerUpdateSupersessionReceipt> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Busy-handler time must not extend the replacement's validity window.
        let now = jiff::Timestamp::now().as_millisecond();
        let prepared = prepare(&tx, attempt, request, now)?;
        if prepared.reused {
            return Ok(prepared.receipt);
        }
        let mut receipt = prepared.receipt;
        let payload =
            serde_json::to_string(&evidence(&receipt)).map_err(|e| invalid(&e.to_string()))?;
        tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('memory.update_superseded',?1,1,1,?2)",
            params![request.binding_id,payload])?;
        receipt.sequence = head(&tx)?;
        let raw = serde_json::to_string(&receipt).map_err(|e| invalid(&e.to_string()))?;
        tx.execute(
            "INSERT INTO memory_update_supersessions VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                request.binding_id,
                request.change_id,
                attempt,
                request.replacement_change_id,
                integer(receipt.replacement_receipt_sequence)?,
                integer(receipt.sequence)?,
                raw
            ],
        )?;
        tx.commit()?;
        Ok(receipt)
    }

    pub fn worker_update_supersession(
        &mut self,
        binding: &str,
        change_id: &str,
    ) -> Result<Option<WorkerUpdateSupersessionReceipt>> {
        if !hash(binding) || !change(change_id) {
            return Err(invalid("invalid supersession identity"));
        }
        let tx = self.connection.transaction()?;
        schema(&tx)?;
        load(&tx, binding, change_id)
    }
}

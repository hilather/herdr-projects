//! Versioned barrier memory evidence. Optional transport acknowledgments are
//! not consumption; use the same consumed-revision closure as readiness.
use super::*;
use rusqlite::types::ValueRef;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const ROW_LIMIT: usize = 10_000;
const PAYLOAD_LIMIT: usize = 8 * 1024 * 1024;

pub(super) struct Fence {
    pub payload: String,
    pub digest: String,
    // Derived from the current read set, never added to the canonical payload.
    pub expires_unix_ms: Option<i64>,
}

fn expiry(value:&Value)->Result<Option<i64>> {
    if value.is_null() {Ok(None)} else {value.as_i64().map(Some).ok_or_else(||StoreError::Corrupt("invalid barrier memory expiry".into()))}
}

fn rows(
    db: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
    budget: &read_budget::ReadBudget,
) -> Result<Vec<Value>> {
    budget.check()?;
    let mut query = db.prepare(sql)?;
    let mut cursor = query.query(params)?;
    let mut output = Vec::new();
    while let Some(row) = cursor.next()? {
        budget.row(row, &[])?;
        if output.len() == ROW_LIMIT {
            return Err(StoreError::Limit(
                "barrier read set exceeds 10000 rows in one section".into(),
            ));
        }
        let mut fields = Vec::with_capacity(row.as_ref().column_count());
        for column in 0..row.as_ref().column_count() {
            fields.push(match row.get_ref(column)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(value) => json!(value),
                ValueRef::Text(bytes) => Value::String(
                    std::str::from_utf8(bytes)
                        .map_err(|_| {
                            StoreError::Corrupt("barrier read set contains invalid text".into())
                        })?
                        .to_owned(),
                ),
                _ => {
                    return Err(StoreError::Corrupt(
                        "unexpected barrier read-set value".into(),
                    ));
                }
            });
        }
        output.push(Value::Array(fields));
    }
    Ok(output)
}

// Raw SqliteStore callers own this temporary SQL deadline. ControlledStore
// callers pass their existing budget through current_with_budget and must never
// install or remove this handler.
struct SqlDeadline<'a>(&'a Connection);
impl Drop for SqlDeadline<'_> {
    fn drop(&mut self) {
        self.0.progress_handler(0, None::<fn() -> bool>);
    }
}

pub(super) fn current(
    db: &Connection,
    generation: u64,
    members: &[BarrierMember],
) -> Result<Fence> {
    bounded(db, |budget| build(db, generation, members, budget))
}

pub(super) fn bounded<T>(db: &Connection, operation: impl FnOnce(&read_budget::ReadBudget) -> Result<T>) -> Result<T> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let control = controlled::ReadControl::new(deadline, Default::default());
    let budget = read_budget::ReadBudget::new(control.clone());
    db.progress_handler(1000, Some(move || Instant::now() >= deadline));
    let _deadline = SqlDeadline(db);
    let result = operation(&budget);
    control.check()?;
    result
}

pub(super) fn current_with_budget(db: &Connection, generation: u64, members: &[BarrierMember], budget: Option<&read_budget::ReadBudget>) -> Result<Fence> {
    match budget {
        // The caller owns its SQL progress handler and original deadline.
        Some(budget) => { budget.check()?; build(db, generation, members, budget) },
        None => current(db, generation, members),
    }
}

fn build(
    db: &Connection,
    generation: u64,
    members: &[BarrierMember],
    budget: &read_budget::ReadBudget,
) -> Result<Fence> {
    let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < 43 {
        return Err(StoreError::UnsupportedSchema(version));
    }
    let control = rows(db,
        "SELECT m.incarnation,c.revision,c.epoch,c.state,c.reconciliation_required,c.config_digest
         FROM active_work_meta m JOIN project_control c ON c.singleton=m.singleton WHERE m.singleton=1",[],budget)?;
    if control.len() != 1 {
        return Err(StoreError::Corrupt(
            "barrier control identity is missing".into(),
        ));
    }
    let policy = rows(
        db,
        "SELECT revision,payload_hash FROM memory_policies ORDER BY revision DESC LIMIT 1",
        [],
        budget,
    )?;
    let scopes = rows(
        db,
        "SELECT scope_id,generation FROM memory_scope_catalog ORDER BY scope_id LIMIT 10001",
        [],
        budget,
    )?;
    let required = rows(db,
        "SELECT r.id,r.record_key,r.scope_id,r.kind,r.is_hard,h.revision,h.status,h.row_revision,
                v.state,v.reason,v.expiry_unix_ms,v.evaluated_seq,x.body_hash,x.provenance_hash,x.applicability,o.availability
         FROM memory_records r LEFT JOIN memory_heads h ON h.record_id=r.id
         LEFT JOIN memory_revisions x ON x.record_id=h.record_id AND x.revision=h.revision
         LEFT JOIN memory_validity v ON v.record_id=h.record_id AND v.revision=h.revision
         LEFT JOIN objects o ON o.hash=x.body_hash
         WHERE (r.is_hard=1 OR r.kind IN ('constraint','hard_memory','contract'))
           AND (h.status='active' OR h.record_id IS NULL) ORDER BY r.id LIMIT 10001",[],budget)?;
    if required
        .iter()
        .any(|row| row[5].is_null() || row[8].is_null() || row[12].is_null() || row[15].is_null())
    {
        return Err(StoreError::Corrupt(
            "required barrier memory evidence is missing".into(),
        ));
    }
    let mut expires_unix_ms=None;
    for row in &required {
        // Match readiness's global mandatory set. Unconsumed optional contracts
        // remain part of the conservative read-set identity, not an expiry gate.
        if row[4].as_i64()==Some(1) || matches!(row[3].as_str(),Some("constraint"|"hard_memory")) {
            expires_unix_ms=earliest_expiry(expires_unix_ms,expiry(&row[10])?);
        }
    }
    let consumed = memory_barrier::consumed_revisions(43);
    let mut member_sets = Vec::with_capacity(members.len());
    for member in members {
        let identity = rows(db,
            "SELECT t.revision,t.state,a.revision,a.state,a.termination_observed,a.snapshot,s.manifest_hash,s.scope_digest
             FROM tasks t JOIN attempts a ON a.task_id=t.id LEFT JOIN memory_snapshots s ON s.id=a.snapshot AND s.task_id=a.task_id
             WHERE t.id=?1 AND a.id=?2",params![member.task_id,member.attempt_id],budget)?;
        if identity.len() != 1 {
            return Err(invalid("barrier member identity is missing"));
        }
        let revisions = rows(db,&format!("{consumed}
            SELECT u.record_id,u.revision,r.record_key,r.scope_id,r.kind,r.is_hard,
                   h.revision,h.status,h.row_revision,v.state,v.reason,v.expiry_unix_ms,v.evaluated_seq,
                   x.body_hash,x.provenance_hash,x.applicability,o.availability
            FROM used u LEFT JOIN memory_records r ON r.id=u.record_id
            LEFT JOIN memory_heads h ON h.record_id=u.record_id
            LEFT JOIN memory_revisions x ON x.record_id=u.record_id AND x.revision=u.revision
            LEFT JOIN memory_validity v ON v.record_id=u.record_id AND v.revision=u.revision
            LEFT JOIN objects o ON o.hash=x.body_hash ORDER BY u.record_id,u.revision LIMIT 10001"),
            params![member.task_id,member.attempt_id],budget)?;
        if revisions.iter().any(|row| {
            row[2].is_null()
                || row[6].is_null()
                || row[9].is_null()
                || row[13].is_null()
                || row[16].is_null()
        }) {
            return Err(StoreError::Corrupt(
                "consumed barrier memory evidence is missing".into(),
            ));
        }
        for row in &revisions {expires_unix_ms=earliest_expiry(expires_unix_ms,expiry(&row[11])?);}
        let dependencies = rows(db,&format!("{consumed}
            SELECT d.derived_record,d.derived_revision,d.source_record,d.source_revision,d.kind
            FROM memory_dependencies d JOIN used u ON u.record_id=d.derived_record AND u.revision=d.derived_revision
            ORDER BY d.derived_record,d.derived_revision,d.source_record,d.source_revision,d.kind LIMIT 10001"),
            params![member.task_id,member.attempt_id],budget)?;
        member_sets.push(
            json!({"task_id":member.task_id,"attempt_id":member.attempt_id,
            "identity":identity,"consumed":revisions,"dependencies":dependencies}),
        );
    }
    let mut value = json!({"schema_version":2,"kind":"barrier-memory","required_set_generation":generation,
        "control":control,"policy":policy,"scopes":scopes,"required":required,"members":member_sets});
    // The hash encoding remains stable if another dependency enables
    // serde_json's preserve_order feature in a future factory build.
    value.sort_all_objects();
    let payload = serde_json::to_string(&value).map_err(|e| invalid(&e.to_string()))?;
    if payload.len() > PAYLOAD_LIMIT {
        return Err(StoreError::Limit("barrier read set exceeds 8 MiB".into()));
    }
    budget.check()?;
    Ok(Fence {
        expires_unix_ms,
        digest: format!("{:x}", Sha256::digest(payload.as_bytes())),
        payload,
    })
}

pub(super) fn stored_version(db: &Connection, barrier: &str, digest: &str, budget: Option<&read_budget::ReadBudget>) -> Result<u32> {
    let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < 43 {
        return Ok(1);
    }
    let mut query = db.prepare(
        "SELECT schema_version,CASE WHEN octet_length(payload)<=8388608 THEN payload END FROM barrier_memory_read_sets WHERE barrier_id=?1",
    )?;
    let mut rows = query.query([barrier])?;
    let row: Option<(u32,Option<String>)> = if let Some(row) = rows.next()? {
        if let Some(budget) = budget { budget.row(row, &[])?; }
        Some((row.get(0)?,row.get(1)?))
    } else { None };
    match row {
        None => Ok(1),
        Some((_, None)) => Err(StoreError::Limit(
            "retained barrier read set exceeds 8 MiB".into(),
        )),
        Some((2, Some(payload)))
            if format!("{:x}", Sha256::digest(payload.as_bytes())) == digest =>
        {
            Ok(2)
        }
        Some(_) => Err(StoreError::Corrupt(
            "barrier read-set digest mismatch".into(),
        )),
    }
}

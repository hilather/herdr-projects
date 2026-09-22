//! Pull-based worker updates. Reading never acknowledges an update.
use super::*;
use anyhow::Result;

pub fn read_memory_update(
    project: &Path,
    delivery: &str,
    attempt: &str,
) -> Result<serde_json::Value> {
    let mut db = crate::migration::open_active(project)?;
    let update = db.memory_update(delivery, attempt)?;
    let bytes = read_object(&project.join(".state/objects"), &update.body_hash)?;
    let body = String::from_utf8(bytes)?;
    let revision = db.memory_revision(&update.record_id, update.revision)?;
    let record = db.memory_record(&update.record_id)?;
    Ok(serde_json::json!({"update": update, "body": body, "record": record, "revision": revision}))
}

pub fn acknowledge_memory_update(
    project: &Path,
    ack: &MemoryUpdateAck,
) -> Result<MemoryUpdateReceipt> {
    let _guard = mutation_guard(project)?;
    let mut db = crate::migration::open_active(project)?;
    let update = db.memory_update(&ack.delivery_id, &ack.attempt_id)?;
    // Availability is checked before creating a declaration; a missing/corrupt
    // body cannot silently turn into applied knowledge. SQL rechecks identity.
    read_object(&project.join(".state/objects"), &update.body_hash)?;
    Ok(db.acknowledge_memory_update(ack, jiff::Timestamp::now().as_millisecond())?)
}

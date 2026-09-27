//! Pull-based worker updates. Reading never acknowledges an update.
use super::*;
use anyhow::Result;

/// Pull exact immutable membership. Creation and retrieval are not seen/applied
/// evidence and do not change consumer generation, receipts or capacity.
pub fn read_update_package(project:&Path,binding:&str)->Result<crate::store::UpdatePackageManifest> {
    read_selected_update_package(project,binding,&[])
}

pub fn read_selected_update_package(project:&Path,binding:&str,changes:&[String])->Result<crate::store::UpdatePackageManifest> {
    let _guard=mutation_guard(project)?;
    let mut db=crate::migration::open_active(project)?;
    Ok(if changes.is_empty() {db.materialize_update_package_manifest(binding)?}
       else {db.materialize_selected_update_package_manifest(binding,changes)?})
}

pub fn read_snapshot_update_package(project:&Path,snapshot:&str)->Result<crate::store::UpdatePackageManifest> {
    read_selected_snapshot_update_package(project,snapshot,&[])
}

pub fn read_selected_snapshot_update_package(project:&Path,snapshot:&str,changes:&[String])->Result<crate::store::UpdatePackageManifest> {
    let _guard=mutation_guard(project)?;
    let mut db=crate::migration::open_active(project)?;
    let binding=db.consumer_binding_for_snapshot(snapshot)?
        .ok_or_else(||anyhow::anyhow!("snapshot has no consumer binding"))?;
    Ok(if changes.is_empty() {db.materialize_update_package_manifest(&binding.binding_id)?}
       else {db.materialize_selected_update_package_manifest(&binding.binding_id,changes)?})
}

/// Aggregate only exact receipts already accepted under the individual worker
/// declaration protocol. This cannot certify validation or clear invalidations.
pub fn acknowledge_worker_update_package(project:&Path,attempt:&str,ack:&crate::store::UpdatePackageAck)->Result<crate::store::WorkerPackageAckReceipt> {
    let _guard=mutation_guard(project)?;
    let mut db=crate::migration::open_active(project)?;
    let updates=db.worker_package_updates(attempt,ack)?;
    let mut remaining=50 * 1024 * 1024u64;
    for update in updates {
        let bytes=read_object_with_budget(&project.join(".state/objects"),&update.body_hash,remaining)?;
        remaining=remaining.checked_sub(bytes.len() as u64).ok_or_else(||anyhow::anyhow!("package bodies exceed 50 MiB"))?;
    }
    Ok(db.acknowledge_worker_update_package(attempt,ack,jiff::Timestamp::now().as_millisecond())?)
}

/// Explicitly retire an optional old delivery using the same live worker's
/// exact applied replacement. No applied receipt or invalidation is changed.
pub fn supersede_worker_update(project:&Path,attempt:&str,request:&crate::store::WorkerUpdateSupersession)->Result<crate::store::WorkerUpdateSupersessionReceipt> {
    let _guard=mutation_guard(project)?;
    let mut db=crate::migration::open_active(project)?;
    let replacement=db.worker_supersession_replacement(attempt,request,jiff::Timestamp::now().as_millisecond())?;
    read_object(&project.join(".state/objects"),&replacement.body_hash)?;
    Ok(db.supersede_worker_update(attempt,request)?)
}

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

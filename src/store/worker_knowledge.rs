//! Bind an immutable knowledge input to the sealed launch identity.
use super::*;

pub(super) fn validate(tx: &Connection, inputs: &LaunchInputs, now: i64) -> Result<()> {
    let actual = std::fs::canonicalize(
        tx.path()
            .ok_or_else(|| StoreError::Invalid("file-backed launch inputs required".into()))?,
    )
    .map_err(|e| StoreError::Invalid(e.to_string()))?;
    if actual.to_str() != Some(inputs.project_store.as_str()) {
        return Err(StoreError::Invalid(
            "knowledge inputs belong to another project store".into(),
        ));
    }
    let version: u32 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let Some(reference) = &inputs.memory else {
        if version >= 18
            && tx.query_row("SELECT EXISTS(SELECT 1 FROM memory_records)", [], |r| {
                r.get::<_, bool>(0)
            })?
        {
            return Err(StoreError::Invalid(
                "launch with memory records requires a bound knowledge snapshot".into(),
            ));
        }
        return Ok(());
    };
    if version < 24 {
        return Err(StoreError::UnsupportedSchema(version));
    }
    let profile = inputs.effective_profile.as_ref().ok_or_else(|| {
        StoreError::Invalid("knowledge requires retained profile evidence".into())
    })?;
    let row:Option<(String,u64,String,String,Option<String>,String,u64)>=tx.query_row(
        "SELECT task_id,task_revision,profile_name,profile_digest,config_digest,manifest_hash,sequence FROM memory_snapshots WHERE id=?1",
        [&reference.id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?))).optional()?;
    let Some((task, revision, name, digest, config, manifest, sequence)) = row else {
        return Err(StoreError::Invalid(
            "launch knowledge snapshot missing".into(),
        ));
    };
    if reference.revision != 1
        || reference.digest != manifest
        || task != inputs.task.as_str()
        || revision != inputs.task_revision
        || name != profile.name
        || digest != profile.definition_digest
        || config != inputs.config.digest
    {
        return Err(StoreError::Invalid(
            "launch knowledge task/profile/config/manifest binding mismatch".into(),
        ));
    }
    let captured: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM memory_snapshot_inputs WHERE snapshot_id=?1)",
        [&reference.id],
        |r| r.get(0),
    )?;
    if !captured {
        return Err(StoreError::Invalid(
            "launch knowledge predates retained inputs".into(),
        ));
    }
    let latest:u64=tx.query_row("SELECT coalesce(max(sequence),0) FROM events WHERE kind IN ('memory.revision_inserted','memory.head_changed','memory.revoked','memory.validity_changed','memory.record_policy_changed','memory.policy_changed','memory.policy_applied')",[],|r|r.get(0))?;
    if sequence < latest {
        return Err(StoreError::Invalid(
            "memory changed after knowledge selection; prepare a new snapshot and approval".into(),
        ));
    }
    let invalid:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM snapshot_entries e
        LEFT JOIN memory_heads h ON h.record_id=e.record_id LEFT JOIN memory_validity v ON v.record_id=e.record_id AND v.revision=e.revision
        LEFT JOIN memory_revisions r ON r.record_id=e.record_id AND r.revision=e.revision LEFT JOIN objects o ON o.hash=r.body_hash
        WHERE e.snapshot_id=?1 AND (h.status IS NOT 'active' OR h.revision!=e.revision OR v.state IS NOT 'valid' OR v.expiry_unix_ms<=?2 OR o.availability IS NOT 'available'))",params![reference.id,now],|r|r.get(0))?;
    if invalid {
        return Err(StoreError::Invalid(
            "launch knowledge contains stale or unavailable memory".into(),
        ));
    }
    let mut stmt =
        tx.prepare("SELECT record_id,revision FROM snapshot_entries WHERE snapshot_id=?1")?;
    let mut rows = stmt.query([&reference.id])?;
    while let Some(row) = rows.next()? {
        if !super::memory_invalidation::dependencies_current(
            tx,
            &row.get::<_, String>(0)?,
            row.get(1)?,
            now,
        )? {
            return Err(StoreError::Invalid(
                "launch knowledge has a stale dependency".into(),
            ));
        }
    }
    Ok(())
}

impl SqliteStore {
    /// The caller cannot choose a profile, snapshot, or bigger budget here.
    pub fn attempt_knowledge_snapshot(
        &mut self,
        attempt: &str,
        now: i64,
    ) -> Result<MemorySnapshot> {
        let tx = self.connection.transaction()?;
        check_schema(&tx)?;
        let records = super::reservations::read_inputs(&tx)?;
        let record = records
            .iter()
            .find(|r| r.attempt.as_str() == attempt)
            .ok_or_else(|| StoreError::Invalid("sealed attempt inputs missing".into()))?;
        let reference = record
            .inputs
            .memory
            .as_ref()
            .ok_or_else(|| StoreError::Invalid("attempt knowledge binding missing".into()))?;
        let bound:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM attempts a JOIN tasks t ON t.active_attempt=a.id AND t.id=a.task_id WHERE a.id=?1 AND a.snapshot=?2 AND t.revision=?3 AND a.termination_observed=0 AND a.state IN ('reserved','launching','running','awaiting_input'))",params![attempt,reference.id,integer(record.inputs.task_revision.checked_add(1).ok_or(StoreError::Conflict)?)?],|r|r.get(0))?;
        if !bound {
            return Err(StoreError::Invalid(
                "attempt knowledge is not bound to current live execution".into(),
            ));
        }
        let control = super::control::read(&tx)?;
        if control.epoch != record.inputs.control_epoch
            || control.config_digest != record.inputs.config.digest
        {
            return Err(StoreError::Conflict);
        }
        validate(&tx, &record.inputs, now)?;
        let id = reference.id.clone();
        tx.commit()?;
        self.read_memory_snapshot(&id)
    }
}

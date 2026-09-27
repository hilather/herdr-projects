//! Transactional consumption of trusted owner-signed barrier authorization.
use super::*;

pub(super) fn require_current_release(
    db: &Connection,
    reference: &BarrierReleaseReference,
    authority: &VersionedReference,
    config: Option<&str>,
    now: i64,
    budget: Option<&read_budget::ReadBudget>,
) -> Result<Option<i64>> {
    reference.validate().map_err(|e| invalid(&e))?;
    if let Some(budget) = budget { budget.check()?; }
    let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < 43 { return Err(StoreError::UnsupportedSchema(version)); }
    let barrier = load_with_budget(db, &reference.barrier_id, budget)?.ok_or_else(|| invalid("required barrier is missing"))?;
    if barrier.revoked_seq.is_some() || barrier.released_seq != Some(reference.release_sequence) {
        return Err(invalid("required barrier is not currently released"));
    }
    let mut query = db.prepare("SELECT a.raw FROM barrier_release_authorizations a JOIN events e ON e.sequence=a.sequence
        WHERE a.barrier_id=?1 AND a.authorization_digest=?2 AND a.sequence=?3
          AND e.kind='barrier.released' AND e.entity=a.barrier_id
          AND json_extract(e.payload,'$.authorization_digest')=a.authorization_digest")?;
    let mut rows = query.query(params![reference.barrier_id,reference.authorization_digest,reference.release_sequence])?;
    let row = rows.next()?.ok_or_else(|| invalid("required barrier lacks exact signed release evidence"))?;
    if let Some(budget) = budget { budget.row(row, &[(0,1)])?; }
    let raw: Vec<u8> = row.get(0)?;
    let prepared = PreparedBarrierRelease::parse_verified(&raw).map_err(StoreError::Corrupt)?;
    let doc = &prepared.document;
    if prepared.digest != reference.authorization_digest
        || doc.barrier_id != reference.barrier_id || &doc.authority != authority
        || config != Some(doc.config_digest.as_str())
        || doc.memory_manifest_version != barrier.memory_manifest_version
        || doc.memory_manifest_digest != barrier.memory_manifest_digest
        || doc.required_set_generation != barrier.required_set_generation
    { return Err(invalid("required barrier release identity or authority changed")); }
    let current: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM project_control c JOIN active_work_meta m ON m.singleton=c.singleton
        WHERE c.singleton=1 AND c.state='active' AND c.reconciliation_required=0
          AND c.revision=?1 AND c.epoch=?2 AND c.config_digest=?3 AND m.incarnation=?4)",
        params![doc.control_revision,doc.control_epoch,doc.config_digest,doc.store_incarnation], |row| row.get(0))?;
    let path = std::fs::canonicalize(db.path().ok_or_else(|| invalid("file-backed store required"))?).map_err(|e| invalid(&e.to_string()))?;
    if !current || path.to_str() != Some(doc.project_store.as_str()) {
        return Err(invalid("required barrier store or control identity changed"));
    }
    // A release authorization's deadline governed its original execution.
    // Consumption requires present readiness, never renewal of that signature.
    let expires=recheck_ready_with_budget(db, &barrier, now, budget)?;
    if let Some(budget) = budget { budget.check()?; }
    Ok(expires)
}

impl SqliteStore {
    pub(crate) fn draft_barrier_release(
        &mut self,
        barrier_id: &str,
        authority: VersionedReference,
        config_digest: &str,
        expires_unix_ms: i64,
    ) -> Result<BarrierReleaseAuthorization> {
        self.draft_barrier_release_with_budget(barrier_id,authority,config_digest,expires_unix_ms,None)
    }

    pub(crate) fn draft_barrier_release_with_budget(&mut self,barrier_id:&str,authority:VersionedReference,config_digest:&str,expires_unix_ms:i64,budget:Option<&read_budget::ReadBudget>)->Result<BarrierReleaseAuthorization> {
        if let Some(budget)=budget {budget.check()?;}
        let tx = self.connection.transaction()?;
        require_schema(&tx)?;
        let barrier = load_with_budget(&tx, barrier_id,budget)?.ok_or_else(|| invalid("barrier is not stored"))?;
        let control = control::read(&tx)?;
        let now = jiff::Timestamp::now().as_millisecond();
        if barrier.released_seq.is_some()
            || barrier.revoked_seq.is_some()
            || control.config_digest.as_deref() != Some(config_digest)
            || control.state != ProjectState::Active
            || control.reconciliation_required
            || expires_unix_ms <= now
        {
            return Err(StoreError::Conflict);
        }
        let memory_expiry=recheck_release_ready(&tx, &barrier, now,budget)?;
        let path = std::fs::canonicalize(
            tx.path()
                .ok_or_else(|| invalid("file-backed store required"))?,
        )
        .map_err(|e| invalid(&e.to_string()))?;
        let document = BarrierReleaseAuthorization {
            schema_version: 1,
            action: "release_barrier".into(),
            project_store: path
                .to_str()
                .ok_or_else(|| invalid("store path must be UTF-8"))?
                .into(),
            store_incarnation: tx.query_row(
                "SELECT incarnation FROM active_work_meta WHERE singleton=1",
                [],
                |row| row.get(0),
            )?,
            authority,
            config_digest: config_digest.into(),
            control_revision: control.revision,
            control_epoch: control.epoch,
            expected_head: head(&tx)?,
            barrier_id: barrier.barrier_id,
            memory_manifest_version: barrier.memory_manifest_version,
            memory_manifest_digest: barrier.memory_manifest_digest,
            required_set_generation: barrier.required_set_generation,
            release_policy: "all_members_ready_v1".into(),
            issued_unix_ms: now,
            expires_unix_ms,
        };
        let bytes = serde_json::to_vec(&document).map_err(|e| invalid(&e.to_string()))?;
        PreparedBarrierRelease::parse_verified(&bytes).map_err(|e| invalid(&e))?;
        if let Some(budget)=budget {budget.check()?;}
        let completed=jiff::Timestamp::now().as_millisecond();
        if completed<document.issued_unix_ms || completed>=document.expires_unix_ms {return Err(StoreError::Conflict);}
        if memory_expiry.is_some_and(|expiry|completed>=expiry) {return Err(invalid("barrier memory evidence expired during drafting"));}
        Ok(document)
    }

    pub(crate) fn release_authorized_barrier(
        &mut self,
        prepared: &PreparedBarrierRelease,
    ) -> Result<FrozenBarrier> {
        self.release_authorized_barrier_with_budget(prepared,None)
    }

    pub(crate) fn release_authorized_barrier_with_budget(&mut self,prepared:&PreparedBarrierRelease,budget:Option<&read_budget::ReadBudget>)->Result<FrozenBarrier> {
        if let Some(budget)=budget {budget.check()?;}
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_schema(&tx)?;
        let version: u32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version < 43 {
            return Err(StoreError::UnsupportedSchema(version));
        }
        // Read the clock after acquiring the write lock: SQLite busy time must
        // not extend an owner's authorization or a consumed record's validity.
        let now = jiff::Timestamp::now().as_millisecond();
        let doc = &prepared.document;
        let path = std::fs::canonicalize(
            tx.path()
                .ok_or_else(|| invalid("file-backed store required"))?,
        )
        .map_err(|e| invalid(&e.to_string()))?;
        let control = control::read(&tx)?;
        let incarnation: String = tx.query_row(
            "SELECT incarnation FROM active_work_meta WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        if path.to_str() != Some(doc.project_store.as_str())
            || incarnation != doc.store_incarnation
            || control.config_digest.as_deref() != Some(doc.config_digest.as_str())
            || control.revision != doc.control_revision
            || control.epoch != doc.control_epoch
            || control.state != ProjectState::Active
            || control.reconciliation_required
        {
            return Err(StoreError::Conflict);
        }
        let barrier =
            load_with_budget(&tx, &doc.barrier_id,budget)?.ok_or_else(|| invalid("barrier is not stored"))?;
        if barrier.memory_manifest_version != doc.memory_manifest_version
            || barrier.memory_manifest_digest != doc.memory_manifest_digest
            || barrier.required_set_generation != doc.required_set_generation
            || doc.release_policy != "all_members_ready_v1"
        {
            return Err(StoreError::Conflict);
        }
        let prior: Option<(String, Vec<u8>, u64)> = read_budget::optional(&tx,
            "SELECT authorization_digest,raw,sequence FROM barrier_release_authorizations WHERE barrier_id=?1",
            [&doc.barrier_id], budget, &[], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if let Some((digest, raw, sequence)) = prior {
            if digest != prepared.digest
                || raw != prepared.raw
                || barrier.released_seq != Some(sequence)
            {
                return Err(StoreError::Conflict);
            }
            if let Some(budget)=budget {budget.check()?;}
            return Ok(barrier);
        }
        // An old unproven release is history, not permission to manufacture a
        // signed release receipt for it after the fact.
        if barrier.released_seq.is_some()
            || barrier.revoked_seq.is_some()
            || head(&tx)? != doc.expected_head
            || now < doc.issued_unix_ms
            || now >= doc.expires_unix_ms
        {
            return Err(StoreError::Conflict);
        }
        let memory_expiry=recheck_release_ready(&tx, &barrier, now,budget)?;
        let released = publish_release_with_budget(&tx, &barrier, Some(&prepared.digest),budget)?;
        tx.execute(
            "INSERT INTO barrier_release_authorizations(barrier_id,authorization_digest,raw,sequence) VALUES(?1,?2,?3,?4)",
            params![doc.barrier_id, prepared.digest, prepared.raw, released.released_seq],
        )?;
        let released = load_with_budget(&tx, &doc.barrier_id,budget)?.ok_or_else(|| invalid("released barrier disappeared"))?;
        if let Some(budget)=budget {budget.check()?;}
        // Readiness and receipt publication can outlast the signed interval even
        // after the write lock was acquired. Refuse before committing the new
        // release; exact historical replay above does not execute authority.
        let commit_now=jiff::Timestamp::now().as_millisecond();
        if commit_now<doc.issued_unix_ms || commit_now>=doc.expires_unix_ms {
            return Err(StoreError::Conflict);
        }
        if memory_expiry.is_some_and(|expiry|commit_now>=expiry) {return Err(invalid("barrier memory evidence expired during publication"));}
        tx.commit()?;
        Ok(released)
    }
}

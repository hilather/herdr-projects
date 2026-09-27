//! Exact delegated admission shares the ordinary reservation/claim validators.
use super::*;
use rusqlite::OptionalExtension;

fn invalid(message: &str) -> StoreError { StoreError::Invalid(message.into()) }
fn schema(db: &Connection) -> Result<()> {
    check_schema(db)?;
    let version: u32 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 43 { return Err(StoreError::UnsupportedSchema(version)); }
    Ok(())
}

pub(super) fn load(db: &Connection, id: &str, budget: Option<&read_budget::ReadBudget>) -> Result<PreparedDelegation> {
    schema(db)?;
    if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) { return Err(invalid("invalid delegation id")); }
    let mut statement = db.prepare("SELECT raw_bytes FROM delegation_grants WHERE id=?1")?;
    let mut rows = statement.query([id])?;
    let row = rows.next()?.ok_or_else(|| invalid("delegation is missing"))?;
    if let Some(budget) = budget { budget.row(row, &[(0, 2)])?; }
    let raw = row.get_ref(0)?.as_blob().map_err(|_| invalid("invalid delegation bytes"))?;
    let grant = PreparedDelegation::parse_verified(raw).map_err(StoreError::Corrupt)?;
    if grant.digest != id { return Err(StoreError::Corrupt("delegation digest mismatch".into())); }
    Ok(grant)
}

fn incarnation(db: &Connection, request: &DelegatedReservationRequest) -> Result<()> {
    let current: String = db.query_row("SELECT incarnation FROM active_work_meta WHERE singleton=1", [], |r| r.get(0))?;
    if current != request.store_incarnation { return Err(invalid("delegated request belongs to another store incarnation")); }
    Ok(())
}

fn scope(db: &Connection, grant: &PreparedDelegation, inputs: &LaunchInputs, now: i64, budget: Option<&read_budget::ReadBudget>) -> Result<()> {
    let bounds = grant.reservation_scope.as_ref().ok_or_else(|| invalid("delegation v1 cannot reserve attempts"))?;
    if grant.project_store != inputs.project_store || now >= grant.expires_unix_ms
        || !grant.actions.contains(&DelegationAction::ReserveAttempt) {
        return Err(invalid("delegation is expired or belongs to another action/store"));
    }
    let revoked: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM delegation_revocations WHERE grant_id=?1)", [&grant.digest], |r| r.get(0))?;
    if revoked { return Err(invalid("delegation is revoked")); }
    let profile = inputs.effective_profile.as_ref().ok_or_else(|| invalid("delegation requires an effective profile"))?;
    if profile.permission_policy != grant.authority || !grant.profile_kinds.contains(&profile.kind)
        || !bounds.profiles.contains(&inputs.profile) || inputs.budget.as_ref() != Some(&bounds.budget)
        || !inputs.task_contract.as_ref().is_some_and(|r| r.id == inputs.task.as_str() && bounds.task_contracts.contains(r)) {
        return Err(invalid("launch exceeds delegated contract, profile, budget or policy scope"));
    }
    let contract = super::contract_binding::latest_with_budget(db, inputs.task.as_str(), budget)?
        .ok_or_else(|| invalid("delegation requires a signed task contract"))?;
    if contract.authority != grant.authority { return Err(invalid("delegated contract uses another authority policy")); }
    if inputs.repositories.is_empty() || !inputs.repositories.iter().all(|repo| bounds.repository_bases.iter().any(|base|
        base.repository == repo.repository && base.commit_oid == repo.commit && repo.tree.len() == base.commit_oid.len()))
        || !bounds.repository_bases.iter().any(|base| base.repository == contract.repository
            && base.commit_oid == contract.base_oid && base.object_format == contract.object_format) {
        return Err(invalid("launch exceeds delegated repository bases"));
    }
    Ok(())
}

pub(super) fn replay(db: &Connection, prepared: &PreparedDelegatedReservation) -> Result<Option<Reservation>> {
    schema(db)?;
    incarnation(db, &prepared.request)?;
    let stored: Option<(String, Vec<u8>, String)> = db.query_row(
        "SELECT request_digest,request_bytes,response FROM delegated_reservations WHERE grant_id=?1 AND idempotency_key=?2",
        params![prepared.request.grant_id, prepared.request.idempotency_key],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    ).optional()?;
    let Some((digest, raw, response)) = stored else { return Ok(None); };
    if digest != prepared.digest || raw != prepared.raw { return Err(StoreError::Conflict); }
    let result: Reservation = serde_json::from_str(&response).map_err(|_| StoreError::Corrupt("invalid delegated reservation response".into()))?;
    let retained = super::reservations::read_input(db, &result.record.operation, None)?;
    if retained != result.record { return Err(StoreError::Corrupt("delegated response differs from retained reservation".into())); }
    Ok(Some(result))
}

/// Called inside the same IMMEDIATE transaction as ordinary reservation.
pub(super) fn install_derived(db: &Connection, prepared: &PreparedDelegatedReservation, inputs: &LaunchInputs, now: i64, budget: Option<&read_budget::ReadBudget>) -> Result<()> {
    schema(db)?;
    incarnation(db, &prepared.request)?;
    let grant = load(db, &prepared.request.grant_id, budget)?;
    scope(db, &grant, inputs, now, budget)?;
    let approval = prepared.approval(&grant).map_err(StoreError::Invalid)?;
    approval.matches_launch(inputs, &grant.project_store, now).map_err(StoreError::Invalid)?;
    let bounds = grant.reservation_scope.as_ref().ok_or_else(|| invalid("delegation lacks reservation scope"))?;
    let (total, retained): (u32, u32) = db.query_row(
        "SELECT count(*),COALESCE(sum(a.termination_observed=0),0) FROM delegated_reservations d JOIN attempts a ON a.id=d.attempt_id WHERE d.grant_id=?1",
        [&grant.digest], |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if total >= bounds.max_total_attempts { return Err(invalid("delegation lifetime attempt limit reached")); }
    if retained >= grant.max_concurrent_attempts { return Err(invalid("delegation concurrent attempt limit reached")); }
    let reference = approval.reference().map_err(StoreError::Invalid)?;
    let payload = serde_json::to_string(&approval).map_err(|e| invalid(&e.to_string()))?;
    // Refuse collisions with an independently installed approval. Never adopt
    // an existing grant into delegated accounting after it may have been used.
    db.execute("INSERT INTO approval_grants(id,payload,payload_hash) VALUES(?1,?2,?3)", params![reference.id, payload, reference.digest])?;
    Ok(())
}

pub(super) fn record(db: &Connection, prepared: &PreparedDelegatedReservation, result: &Reservation) -> Result<()> {
    db.execute("INSERT INTO delegated_reservations VALUES(?1,?2,?3,?4,?5,?6,?7)", params![
        prepared.request.grant_id, prepared.request.idempotency_key, prepared.digest, prepared.raw,
        result.record.inputs.approval.id, result.record.attempt.as_str(),
        serde_json::to_string(result).map_err(|e| invalid(&e.to_string()))?,
    ])?;
    Ok(())
}

/// Ordinary owner grants have no delegated provenance. Derived grants must
/// continue to satisfy the original scope/revocation/incarnation at every claim.
pub(super) fn validate_derived(db: &Connection, inputs: &LaunchInputs, now: i64, budget: Option<&read_budget::ReadBudget>) -> Result<()> {
    let version: u32 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 43 { return Ok(()); }
    let mut statement = db.prepare("SELECT request_bytes,request_digest FROM delegated_reservations WHERE approval_id=?1")?;
    let mut rows = statement.query([&inputs.approval.id])?;
    let Some(row) = rows.next()? else { return Ok(()); };
    if let Some(budget) = budget { budget.row(row, &[(0, 2)])?; }
    let prepared = PreparedDelegatedReservation::parse_verified(row.get_ref(0)?.as_blob().map_err(|_| invalid("invalid delegated request bytes"))?)
        .map_err(StoreError::Corrupt)?;
    if prepared.digest != row.get::<_, String>(1)? { return Err(StoreError::Corrupt("delegated request digest mismatch".into())); }
    incarnation(db, &prepared.request)?;
    let grant = load(db, &prepared.request.grant_id, budget)?;
    scope(db, &grant, inputs, now, budget)?;
    prepared.approval(&grant).map_err(StoreError::Corrupt)?
        .matches_launch(inputs, &grant.project_store, now).map_err(StoreError::Invalid)
}

impl SqliteStore {
    pub(crate) fn delegation_for_reservation_signature(&self, id: &str) -> Result<PreparedDelegation> { load(&self.connection, id, None) }
    pub(crate) fn delegated_reservation_receipt(&self, prepared: &PreparedDelegatedReservation) -> Result<Option<Reservation>> { replay(&self.connection, prepared) }

    pub(crate) fn draft_delegated_reservation(&mut self, id: &str, key: &str, inputs: LaunchInputs, now: i64) -> Result<DelegatedReservationRequest> {
        let grant = load(&self.connection, id, None)?;
        scope(&self.connection, &grant, &inputs, now, None)?;
        let expected_head = self.current_head()?;
        self.validate_launch_draft(&inputs, expected_head, now)?;
        let request = DelegatedReservationRequest {
            schema_version: 1, grant_id: id.into(), subject: grant.subject,
            store_incarnation: self.connection.query_row("SELECT incarnation FROM active_work_meta WHERE singleton=1", [], |r| r.get(0))?,
            idempotency_key: key.into(), expected_head, issued_unix_ms: now, inputs,
        };
        PreparedDelegatedReservation::parse_verified(&serde_json::to_vec(&request).map_err(|e| invalid(&e.to_string()))?).map_err(StoreError::Invalid)?;
        Ok(request)
    }

    pub fn reserve_delegated(&mut self, prepared: &PreparedDelegatedReservation, now: i64) -> Result<Reservation> {
        let reparsed = PreparedDelegatedReservation::parse_verified(&prepared.raw).map_err(StoreError::Invalid)?;
        if reparsed.request != prepared.request || reparsed.digest != prepared.digest { return Err(invalid("changed delegated request")); }
        if let Some(result) = replay(&self.connection, prepared)? { return Ok(result); }
        let grant = load(&self.connection, &prepared.request.grant_id, None)?;
        let approval = prepared.approval(&grant).map_err(StoreError::Invalid)?;
        let mut inputs = prepared.request.inputs.clone();
        inputs.approval = approval.reference().map_err(StoreError::Invalid)?;
        self.reserve_delegated_prepared(&[PreparedLaunch { inputs }], prepared, now)
    }
}

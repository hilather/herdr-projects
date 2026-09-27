//! Approval use is committed atomically with a launch claim. No external effects.
use super::*;
use crate::operations::Claim;
use rusqlite::OptionalExtension;

#[allow(dead_code)]
pub(super) fn read_all(db: &Connection) -> Result<Vec<ApprovalRecord>> {
    read_all_with(db, &super::reservations::read_inputs(db)?, None)
}
pub(super) fn read_all_with(db: &Connection, inputs: &[AttemptInputRecord], budget: Option<&read_budget::ReadBudget>) -> Result<Vec<ApprovalRecord>> {
    let orphan:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM approval_uses u LEFT JOIN approval_grants g ON g.id=u.approval_id LEFT JOIN operations o ON o.id=u.operation_id WHERE g.id IS NULL OR o.id IS NULL) OR EXISTS(SELECT 1 FROM approval_revocations r LEFT JOIN approval_grants g ON g.id=r.approval_id WHERE g.id IS NULL)",[],|r|r.get(0))?;
    if orphan { return Err(StoreError::Corrupt("orphan approval record".into())); }
    let mut stmt=db.prepare("SELECT id FROM approval_grants ORDER BY id")?;
    let mut rows=stmt.query([])?;
    let mut ids=Vec::new();
    while let Some(r)=rows.next()? {
        if let Some(budget)=budget {budget.row(r,&[])?;}
        ids.push(r.get::<_,String>(0)?);
    }
    let mut records=Vec::new();
    for id in ids {
        let grant=grant(db,&id,budget)?;
        let revoked=db.query_row("SELECT revoked_unix_ms,reason FROM approval_revocations WHERE approval_id=?1",[&id],|r|Ok(ApprovalRevocation{revoked_unix_ms:r.get(0)?,reason:r.get(1)?})).optional()?;
        let consumed=db.query_row("SELECT operation_id,claim_revision,claim_epoch,consumed_unix_ms FROM approval_uses WHERE approval_id=?1",[&id],|r|Ok((r.get::<_,String>(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?
            .map(|(id,claim_revision,claim_epoch,consumed_unix_ms)|Ok::<ApprovalUse,StoreError>(ApprovalUse{operation:OperationId::new(id).map_err(StoreError::Corrupt)?,claim_revision,claim_epoch,consumed_unix_ms})).transpose()?;
        if let Some(used)=&consumed {
            let input=inputs.iter().find(|r|r.operation==used.operation).ok_or_else(||StoreError::Corrupt("approval use lacks launch inputs".into()))?;
            grant.matches_launch(&input.inputs,&grant.scope.project_store,used.consumed_unix_ms).map_err(|_|StoreError::Corrupt("approval use action mismatch".into()))?;
            let delivery=super::delivery::delivery(db,&used.operation)?;
            if delivery.epoch<used.claim_epoch||delivery.revision<used.claim_revision||delivery.attempts==0 {return Err(StoreError::Corrupt("approval use claim history mismatch".into()));}
        }
        records.push(ApprovalRecord{reference:grant.reference().map_err(|s|invalid(&s))?,grant,revoked,consumed});
    }
    Ok(records)
}

fn invalid(text: &str) -> StoreError { StoreError::Invalid(text.into()) }

/// The brief reuses exactly its launch's consumed approval. Administrative
/// readers still validate all records; a foreground prompt never scans them.
pub(super) fn validate_consumed_launch(
    db: &Connection,
    input: &AttemptInputRecord,
    now: i64,
    budget: Option<&read_budget::ReadBudget>,
) -> Result<()> {
    validate_consumption(db,input,Some(now),budget)
}
/// Stop evidence binds the original claim. Revocation or expiry must not require
/// renewed execution authority merely to stop and preserve that exact worker.
pub(super) fn validate_historical_consumption(db:&Connection,input:&AttemptInputRecord,budget:Option<&read_budget::ReadBudget>)->Result<()> {
    validate_consumption(db,input,None,budget)
}
fn validate_consumption(db:&Connection,input:&AttemptInputRecord,now:Option<i64>,budget:Option<&read_budget::ReadBudget>)->Result<()> {
    if let Some(budget) = budget { budget.check()?; }
    let approval = grant(db, &input.inputs.approval.id, budget)?;
    let mut stmt = db.prepare(
        "SELECT u.operation_id,u.claim_revision,u.claim_epoch,u.consumed_unix_ms,
         EXISTS(SELECT 1 FROM approval_revocations r WHERE r.approval_id=u.approval_id)
         FROM approval_uses u WHERE u.approval_id=?1",
    )?;
    let mut rows = stmt.query([&input.inputs.approval.id])?;
    let row = rows.next()?.ok_or_else(|| invalid("worker brief lacks consumed launch approval"))?;
    if let Some(budget) = budget { budget.row(row, &[])?; }
    let operation = OperationId::new(row.get::<_, String>(0)?).map_err(StoreError::Corrupt)?;
    let revision: u64 = row.get(1)?;
    let epoch: u64 = row.get(2)?;
    let consumed: i64 = row.get(3)?;
    let revoked: bool = row.get(4)?;
    if (revoked && now.is_some()) || operation != input.operation {
        return Err(invalid("worker brief lacks active consumed launch approval"));
    }
    approval.matches_launch(&input.inputs, &input.inputs.project_store, consumed)
        .map_err(|_| StoreError::Corrupt("approval use action mismatch".into()))?;
    let delivery = super::delivery::delivery_with_budget(db, &operation, budget)?;
    if delivery.epoch < epoch || delivery.revision < revision || delivery.attempts == 0 {
        return Err(StoreError::Corrupt("approval use claim history mismatch".into()));
    }
    if let Some(now)=now {
        approval.matches_launch(&input.inputs, &input.inputs.project_store, now)
            .map_err(|_| invalid("worker brief approval expired or changed"))?;
    }
    if let Some(budget) = budget { budget.check()?; }
    Ok(())
}

fn schema(db: &Connection) -> Result<()> {
    check_schema(db)?;
    let version: u32 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 13 { return Err(StoreError::UnsupportedSchema(version)); }
    Ok(())
}
fn project_path(db: &Connection) -> Result<String> {
    let path = db.path().ok_or_else(|| invalid("approval requires a file-backed store"))?;
    std::fs::canonicalize(path).map(|p| p.to_string_lossy().into_owned()).map_err(|_| invalid("approval store path unavailable"))
}
pub(super) fn grant(db: &Connection, id: &str, budget: Option<&read_budget::ReadBudget>) -> Result<ApprovalGrant> {
    let mut stmt = db.prepare("SELECT payload,payload_hash FROM approval_grants WHERE id=?1")?;
    let mut rows = stmt.query([id])?;
    let r = rows.next()?.ok_or_else(|| StoreError::from(rusqlite::Error::QueryReturnedNoRows))?;
    if let Some(budget) = budget { budget.row(r, &[(0, 2)])?; }
    let (payload, digest): (String, String) = (r.get(0)?, r.get(1)?);
    if payload.len() > MAX_RECORD_BYTES || format!("{:x}", Sha256::digest(payload.as_bytes())) != digest { return Err(StoreError::Corrupt("approval payload identity mismatch".into())); }
    let grant: ApprovalGrant = serde_json::from_str(&payload).map_err(|_| StoreError::Corrupt("invalid approval record".into()))?;
    let reference = grant.reference().map_err(|_| StoreError::Corrupt("invalid approval record".into()))?;
    if reference.id != id || reference.digest != digest { return Err(StoreError::Corrupt("approval reference mismatch".into())); }
    Ok(grant)
}
fn log(db: &Connection, kind: &str, id: &str, payload: &impl serde::Serialize) -> Result<()> {
    db.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES(?1,?2,1,1,?3)", params![kind,id,serde_json::to_string(payload).map_err(|_| invalid("approval event encoding failed"))?])?;
    Ok(())
}

// Validation is shared by admission and claim. Only consume_with_budget() records use.
fn check_inputs(db: &Connection, inputs: &LaunchInputs, now: i64,budget:Option<&read_budget::ReadBudget>) -> Result<()> {
    schema(db)?;
    super::contract_binding::validate_with_budget(db, inputs, now,budget)?;
    if crate::migration::config_reference(Path::new(&inputs.config.path)).map_err(|_|invalid("launch configuration is unreadable"))? != inputs.config {
        return Err(invalid("launch configuration changed since approval"));
    }
    let profile = inputs.effective_profile.as_ref().ok_or_else(|| invalid("historical launch lacks current approval evidence"))?;
    let approval = grant(db, &inputs.approval.id, budget)?;
    approval.matches_launch(inputs, &project_path(db)?, now).map_err(|_| invalid("launch approval is stale or mismatched"))?;
    if approval.policy != profile.permission_policy { return Err(invalid("approval policy differs from effective profile")); }
    let revoked: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM approval_revocations WHERE approval_id=?1)", [&inputs.approval.id], |r| r.get(0))?;
    if revoked { return Err(invalid("launch approval is revoked")); }
    super::delegated_reservation::validate_derived(db, inputs, now, budget)?;
    Ok(())
}

#[cfg(test)]
pub(super) fn validate_preparation(db: &Connection, inputs: &LaunchInputs, now: i64) -> Result<()> {validate_preparation_with_budget(db,inputs,now,None)}
pub(super) fn validate_preparation_with_budget(db: &Connection, inputs: &LaunchInputs, now: i64,budget:Option<&read_budget::ReadBudget>) -> Result<()> {
    check_inputs(db, inputs, now,budget)?;
    let consumed: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM approval_uses WHERE approval_id=?1)", [&inputs.approval.id], |r|r.get(0))?;
    if consumed { return Err(invalid("launch approval has already been consumed")); }
    Ok(())
}

impl SqliteStore {
    pub(crate) fn matching_launch_approval(&self, inputs: &LaunchInputs, now: i64,budget:Option<&read_budget::ReadBudget>) -> Result<Option<VersionedReference>> {
        let scope = ApprovalScope::for_launch(inputs).map_err(StoreError::Invalid)?;
        let mut stmt = self.connection.prepare("SELECT g.id FROM approval_grants g WHERE json_extract(g.payload,'$.scope.action_digest')=?1 AND NOT EXISTS(SELECT 1 FROM approval_uses u WHERE u.approval_id=g.id) AND NOT EXISTS(SELECT 1 FROM approval_revocations r WHERE r.approval_id=g.id) ORDER BY g.id LIMIT 65")?;
        let mut rows=stmt.query([scope.action_digest])?;let mut ids=Vec::new();
        while let Some(row)=rows.next()? {
            if let Some(budget)=budget {budget.row(row,&[])?;}
            ids.push(row.get::<_,String>(0)?);
        }
        if ids.len()>64 { return Err(StoreError::Limit("matching launch approval limit exceeded".into())); }
        for id in ids {
            let grant = grant(&self.connection, &id, budget)?;
            let mut candidate = inputs.clone();
            candidate.approval = grant.reference().map_err(StoreError::Corrupt)?;
            if grant.matches_launch(&candidate, &inputs.project_store, now).is_ok() && self.preparation_grant_accepted(&candidate, now,budget)? { return Ok(Some(candidate.approval)); }
        }
        Ok(None)
    }
    /// `Ok(false)` is a rejected grant. Store corruption still fails the wake.
    pub(crate) fn preparation_grant_accepted(&self, inputs: &LaunchInputs, now: i64,budget:Option<&read_budget::ReadBudget>) -> Result<bool> {
        match validate_preparation_with_budget(&self.connection, inputs, now,budget) {
            Ok(()) => Ok(true),
            Err(StoreError::Invalid(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }
}

fn check_launch_with_budget(db: &Connection, operation: &OperationId, now: i64, budget: Option<&read_budget::ReadBudget>) -> Result<String> {
    if let Some(budget) = budget { budget.check()?; }
    schema(db)?;
    let record = super::reservations::read_input(db,operation,budget)?;
    let inputs = &record.inputs;
    super::worker_knowledge::validate_with_budget(db,inputs,now,budget)?;
    super::budget::check_with_budget(db,inputs.budget.as_ref(),true,budget)?;
    let attempt=read_attempt_with_budget(db,&record.attempt,budget)?;
    let task=read_task_with_budget(db,inputs.task.as_str(),budget)?;
    if attempt.task!=task.id || attempt.revision!=1 || attempt.state!=AttemptState::Reserved
        || !attempt.retains_capacity() || attempt.reservation!=format!("worker:{}",attempt.id.as_str())
        || task.state!=TaskState::Running || task.active_attempt.as_ref()!=Some(&attempt.id)
        || Some(task.revision)!=inputs.task_revision.checked_add(1) {
        return Err(invalid("launch no longer owns an eligible retained reservation"));
    }
    if attempt.snapshot.as_deref()!=inputs.memory.as_ref().map(|r|r.id.as_str()) {
        return Err(invalid("attempt snapshot differs from approved launch knowledge"));
    }
    check_inputs(db, inputs, now,budget)?;
    let control = super::control::read_with_budget(db,budget)?;
    let scheduler = super::scheduler::read_policy(db,budget)?;
    if control.state != ProjectState::Active || control.reconciliation_required || control.epoch != inputs.control_epoch
        || control.config_digest != inputs.config.digest || scheduler.revision != inputs.scheduler_revision {
        return Err(invalid("launch control or policy changed since approval"));
    }
    let binding = super::runtime::read_binding(db,&inputs.binding,budget)?.filter(|b| b.revision == inputs.binding_revision).ok_or(StoreError::Conflict)?;
    if super::ownership::identity_digest(&binding)? != inputs.binding_digest { return Err(StoreError::Conflict); }
    if super::ownership::read_binding(db,&binding.id,budget)?.is_some_and(|o|o.attempt.is_some()||o.session.is_some()||o.agent.is_some()) {
        return Err(invalid("launch binding already has worker ownership"));
    }
    Ok(inputs.approval.id.clone())
}

pub(super) fn consume_with_budget(db: &Connection, operation: &OperationId, revision: u64, epoch: u64, now: i64, budget: Option<&read_budget::ReadBudget>) -> Result<()> {
    let approval = check_launch_with_budget(db, operation, now, budget)?;
    db.execute("INSERT INTO approval_uses VALUES(?1,?2,?3,?4,?5)", params![approval,operation.as_str(),integer(revision)?,integer(epoch)?,now])?;
    log(db, "approval.consumed", &approval, &serde_json::json!({"operation":operation,"claim_revision":revision,"claim_epoch":epoch,"consumed_unix_ms":now}))
}

pub(super) fn validate_use(db: &Connection, claim: &Claim, now: i64) -> Result<()> {
    validate_use_with_budget(db, claim, now, None)
}
pub(super) fn validate_use_with_budget(db: &Connection, claim: &Claim, now: i64, budget: Option<&read_budget::ReadBudget>) -> Result<()> {
    let approval = check_launch_with_budget(db, &claim.operation, now, budget)?;
    let valid: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM approval_uses WHERE approval_id=?1 AND operation_id=?2 AND claim_revision=?3 AND claim_epoch=?4)", params![approval,claim.operation.as_str(),integer(claim.revision)?,integer(claim.epoch)?], |r| r.get(0))?;
    if !valid { return Err(invalid("launch claim has no matching approval consumption")); }
    Ok(())
}

impl SqliteStore {
    /// Only authenticated future ingress may construct PreparedApproval. No raw
    /// JSON or actor-string constructor exists on this API.
    pub fn install_approval(&mut self, prepared: &PreparedApproval, expected_head: u64, now: i64) -> Result<VersionedReference> {
        super::delivery::now_check(now)?;
        let approval = &prepared.grant;
        let reference = approval.reference().map_err(|s| invalid(&s))?;
        if now < approval.issued_unix_ms || now >= approval.expires_unix_ms { return Err(invalid("approval is outside its validity interval")); }
        let actual = project_path(&self.connection)?;
        if actual != approval.scope.project_store { return Err(invalid("approval belongs to another project")); }
        let payload = serde_json::to_string(approval).map_err(|_| invalid("approval encoding failed"))?;
        if payload.len() > MAX_RECORD_BYTES { return Err(invalid("approval exceeds one MiB")); }
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema(&tx)?;
        if head(&tx)? != expected_head { return Err(StoreError::Conflict); }
        let tasks = read_tasks(&tx)?;
        let task = tasks.iter().find(|t| t.id == approval.scope.task).ok_or(StoreError::Conflict)?;
        if task.revision != approval.scope.task_revision && task.revision.checked_add(1) != Some(approval.scope.task_revision) { return Err(StoreError::Conflict); }
        tx.execute("INSERT INTO approval_grants VALUES(?1,?2,?3)", params![reference.id,payload,reference.digest])?;
        log(&tx, "approval.installed", &reference.id, approval)?;
        tx.commit()?;
        Ok(reference)
    }

    /// Revocation narrows authority; it never stops a running external effect.
    pub fn revoke_approval(&mut self, id: &str, expected_head: u64, now: i64, reason: &str) -> Result<u64> {
        super::delivery::now_check(now)?;
        if reason.trim().is_empty() || reason.len() > 4000 || reason.chars().any(char::is_control) { return Err(invalid("invalid approval revocation reason")); }
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema(&tx)?;if head(&tx)? != expected_head { return Err(StoreError::Conflict); }
        grant(&tx, id, None)?;
        tx.execute("INSERT INTO approval_revocations VALUES(?1,?2,?3)", params![id,now,reason])?;
        log(&tx, "approval.revoked", id, &serde_json::json!({"revoked_unix_ms":now,"reason":reason}))?;
        let head = head(&tx)?;tx.commit()?;Ok(head)
    }
}

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
fn grant(db: &Connection, id: &str, budget: Option<&read_budget::ReadBudget>) -> Result<ApprovalGrant> {
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

// Validation is shared by admission and claim. Only consume() records use.
fn check_inputs(db: &Connection, inputs: &LaunchInputs, now: i64) -> Result<()> {
    schema(db)?;
    if crate::migration::config_reference(Path::new(&inputs.config.path)).map_err(|_|invalid("launch configuration is unreadable"))? != inputs.config {
        return Err(invalid("launch configuration changed since approval"));
    }
    let profile = inputs.effective_profile.as_ref().ok_or_else(|| invalid("historical launch lacks current approval evidence"))?;
    let approval = grant(db, &inputs.approval.id, None)?;
    approval.matches_launch(inputs, &project_path(db)?, now).map_err(|_| invalid("launch approval is stale or mismatched"))?;
    if approval.policy != profile.permission_policy { return Err(invalid("approval policy differs from effective profile")); }
    let revoked: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM approval_revocations WHERE approval_id=?1)", [&inputs.approval.id], |r| r.get(0))?;
    if revoked { return Err(invalid("launch approval is revoked")); }
    Ok(())
}

pub(super) fn validate_preparation(db: &Connection, inputs: &LaunchInputs, now: i64) -> Result<()> {
    check_inputs(db, inputs, now)?;
    let consumed: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM approval_uses WHERE approval_id=?1)", [&inputs.approval.id], |r|r.get(0))?;
    if consumed { return Err(invalid("launch approval has already been consumed")); }
    Ok(())
}

fn check_launch(db: &Connection, operation: &OperationId, now: i64) -> Result<String> {
    schema(db)?;
    let record = super::reservations::read_inputs(db)?.into_iter().find(|r| &r.operation == operation).ok_or_else(|| invalid("launch input record missing"))?;
    let inputs = &record.inputs;
    super::worker_knowledge::validate(db,inputs,now)?;
    super::budget::check(db,inputs.budget.as_ref(),true)?;
    let attempts=read_attempts(db)?;
    let attempt=attempts.iter().find(|a|a.id==record.attempt).ok_or(StoreError::Conflict)?;
    let tasks=read_tasks(db)?;
    let task=tasks.iter().find(|t|t.id==inputs.task).ok_or(StoreError::Conflict)?;
    if attempt.task!=task.id || attempt.revision!=1 || attempt.state!=AttemptState::Reserved
        || !attempt.retains_capacity() || attempt.reservation!=format!("worker:{}",attempt.id.as_str())
        || task.state!=TaskState::Running || task.active_attempt.as_ref()!=Some(&attempt.id)
        || Some(task.revision)!=inputs.task_revision.checked_add(1) {
        return Err(invalid("launch no longer owns an eligible retained reservation"));
    }
    if attempt.snapshot.as_deref()!=inputs.memory.as_ref().map(|r|r.id.as_str()) {
        return Err(invalid("attempt snapshot differs from approved launch knowledge"));
    }
    check_inputs(db, inputs, now)?;
    let control = super::control::read(db)?;
    let scheduler = super::scheduler::read(db)?;
    if control.state != ProjectState::Active || control.reconciliation_required || control.epoch != inputs.control_epoch
        || control.config_digest != inputs.config.digest || scheduler.policy.revision != inputs.scheduler_revision {
        return Err(invalid("launch control or policy changed since approval"));
    }
    let bindings = super::runtime::read_all(db)?;
    let binding = bindings.iter().find(|b| b.id == inputs.binding && b.revision == inputs.binding_revision).ok_or(StoreError::Conflict)?;
    if super::ownership::identity_digest(binding)? != inputs.binding_digest { return Err(StoreError::Conflict); }
    if super::ownership::read_all(db)?.iter().any(|o|o.binding==binding.id&&(o.attempt.is_some()||o.session.is_some()||o.agent.is_some())) {
        return Err(invalid("launch binding already has worker ownership"));
    }
    Ok(inputs.approval.id.clone())
}

pub(super) fn consume(db: &Connection, operation: &OperationId, revision: u64, epoch: u64, now: i64) -> Result<()> {
    let approval = check_launch(db, operation, now)?;
    db.execute("INSERT INTO approval_uses VALUES(?1,?2,?3,?4,?5)", params![approval,operation.as_str(),integer(revision)?,integer(epoch)?,now])?;
    log(db, "approval.consumed", &approval, &serde_json::json!({"operation":operation,"claim_revision":revision,"claim_epoch":epoch,"consumed_unix_ms":now}))
}

pub(super) fn validate_use(db: &Connection, claim: &Claim, now: i64) -> Result<()> {
    let approval = check_launch(db, &claim.operation, now)?;
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

//! A separate, non-replayable brief submission obligation. Launch confirmation
//! alone never means that the worker has received its retained task knowledge.
use super::*;
use crate::operations::{Claim, Delivery, DeliveryState, Outcome};

fn invalid(message: &str) -> StoreError {
    StoreError::Invalid(message.into())
}
fn encode(value: &impl serde::Serialize) -> Result<String> {
    serde_json::to_string(value).map_err(|_| invalid("worker brief encoding failed"))
}
fn identity(intent: &WorkerBriefIntent) -> Result<OperationId> {
    OperationId::new(format!(
        "brief-{:x}",
        Sha256::digest(encode(intent)?.as_bytes())
    ))
    .map_err(StoreError::Invalid)
}
fn validate(intent: &WorkerBriefIntent) -> Result<()> {
    if intent.version != 1
        || intent.binding.is_empty()
        || intent.binding.len() > 512
        || intent.binding_revision == 0
        || intent.ownership_revision == 0
        || intent.prompt_chars == 0
        || intent.prompt_chars > 1_048_576
        || intent.prompt_digest.len() != 64
        || !intent
            .prompt_digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(invalid("invalid retained worker brief identity"));
    }
    Ok(())
}
pub(super) fn context_with_budget(db: &Connection, intent: &WorkerBriefIntent, budget: Option<&read_budget::ReadBudget>) -> Result<(AttemptInputRecord, RuntimeOwnership, Attempt, Task)> {
    validate(intent)?;
    let record = super::reservations::read_input(db, &intent.launch, budget)?;
    if record.attempt != intent.attempt { return Err(StoreError::Conflict); }
    if intent.binding != record.inputs.binding || intent.knowledge != record.inputs.memory {
        return Err(StoreError::Conflict);
    }
    let launch = super::delivery::delivery_with_budget(db, &record.operation, budget)?;
    let Some(Outcome::Confirmed { observed_identity }) = launch.last_outcome else {
        return Err(StoreError::Conflict);
    };
    if launch.state != DeliveryState::Confirmed
        || !super::launch::has_started_receipt(db, &record.operation, &observed_identity)?
    {
        return Err(StoreError::Conflict);
    }
    let binding = super::runtime::read_binding(db, &intent.binding, budget)?
        .ok_or(StoreError::Conflict)?;
    let ownership = super::ownership::read_binding(db, &intent.binding, budget)?
        .ok_or(StoreError::Conflict)?;
    let attempt = read_attempt_with_budget(db, &intent.attempt, budget)?;
    let task = read_task_with_budget(db, record.inputs.task.as_str(), budget)?;
    if binding.revision != intent.binding_revision
        || ownership.revision != intent.ownership_revision
        || ownership.binding_revision != binding.revision
        || ownership.identity_digest != super::ownership::identity_digest(&binding)?
        || ownership.origin != "launched"
        || ownership.attempt.as_ref() != Some(&intent.attempt)
        || !attempt.retains_capacity()
        || attempt.state != AttemptState::Launching
        || task.active_attempt.as_ref() != Some(&intent.attempt)
        || task.state != TaskState::Running
    {
        return Err(StoreError::Conflict);
    }
    Ok((record, ownership, attempt, task))
}
fn authority_with_budget(db: &Connection, record: &AttemptInputRecord, now: i64, budget: Option<&read_budget::ReadBudget>) -> Result<()> {
    let inputs = &record.inputs;
    let control = super::control::read_with_budget(db, budget)?;
    if control.state != ProjectState::Active
        || control.reconciliation_required
        || control.epoch != inputs.control_epoch
        || control.config_digest != inputs.config.digest
        || db.query_row("SELECT EXISTS(SELECT 1 FROM attempt_cancellations WHERE attempt_id=?1)
            OR EXISTS(SELECT 1 FROM events WHERE kind='attempt.completion_requested' AND entity=?1)", [record.attempt.as_str()], |row| row.get::<_,bool>(0))?
    {
        return Err(StoreError::Conflict);
    }
    if crate::migration::config_reference(Path::new(&inputs.config.path))
        .map_err(|_| invalid("worker configuration unavailable"))?
        != inputs.config
    {
        return Err(invalid(
            "worker configuration changed before brief submission",
        ));
    }
    super::worker_knowledge::validate_with_budget(db, inputs, now, budget)?;
    // Launch approval consumption does not grant a later prompt permanent
    // authority. Recheck the exact task contract and its required wave here.
    super::contract_binding::validate_with_budget(db, inputs, now, budget)?;
    super::budget::check_with_budget(db, inputs.budget.as_ref(), true, budget)?;
    super::approvals::validate_consumed_launch(db, record, now, budget)
}
pub(super) fn check_with_budget(db: &Connection, id: &OperationId, now: i64, budget: Option<&read_budget::ReadBudget>) -> Result<()> {
    if let Some(budget)=budget { budget.check()?; }
    let op = read_operation_with_budget(db, id, budget)?;
    let intent: WorkerBriefIntent = serde_json::from_value(op.payload)
        .map_err(|_| invalid("invalid worker brief operation"))?;
    if op.kind != "runtime.worker_brief"
        || op.payload_version != 1
        || identity(&intent)? != op.id
        || op.idempotency_key != op.id.as_str()
        || op.target != intent.binding
    {
        return Err(StoreError::Conflict);
    }
    let (record, _, _, task) = context_with_budget(db, &intent, budget)?;
    if op.task.as_ref() != Some(&task.id) || op.expected_revision != task.revision {
        return Err(StoreError::Conflict);
    }
    authority_with_budget(db, &record, now, budget)
}
pub(super) fn has_receipt(db: &Connection, operation: &OperationId, payload: &str) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM events WHERE kind='runtime.worker_brief_delivered' AND entity=?1 AND payload=?2)",
        params![operation.as_str(),payload], |r| r.get(0))?)
}
impl SqliteStore {
    pub fn enqueue_worker_brief(
        &mut self,
        prepared: &PreparedWorkerBrief,
        expected_head: u64,
        now: i64,
    ) -> Result<Operation> {
        self.enqueue_worker_brief_with_budget(prepared, expected_head, now, None)
    }
    pub(crate) fn enqueue_worker_brief_with_budget(&mut self, prepared:&PreparedWorkerBrief,expected_head:u64,now:i64,budget:Option<&read_budget::ReadBudget>) -> Result<Operation> {
        if let Some(budget)=budget {budget.check()?;}
        super::delivery::now_check(now)?;
        let intent = &prepared.intent;
        validate(intent)?;
        let id = identity(intent)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_schema(&tx)?;
        if head(&tx)? != expected_head {
            return Err(StoreError::Conflict);
        }
        let prior: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1)",
            [id.as_str()],
            |r| r.get(0),
        )?;
        if prior {
            return read_operation_with_budget(&tx, &id, budget);
        }
        let (record, _, _, task) = context_with_budget(&tx, intent, budget)?;
        authority_with_budget(&tx, &record, now, budget)?;
        // One initial brief per attempt, even if a caller obtains a newly
        // rendered payload after uncertainty. A changed digest is not a retry.
        let existing: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE kind='runtime.worker_brief' AND json_extract(payload,'$.attempt')=?1)", [intent.attempt.as_str()], |r| r.get(0))?;
        if existing {
            return Err(invalid("attempt already has a worker brief obligation"));
        }
        let payload = encode(intent)?;
        let operation = Operation {
            id,
            task: Some(task.id),
            kind: "runtime.worker_brief".into(),
            target: intent.binding.clone(),
            payload_version: 1,
            payload: serde_json::to_value(intent).map_err(|_| invalid("brief encoding failed"))?,
            expected_revision: task.revision,
            due_unix_ms: now,
            idempotency_key: identity(intent)?.as_str().into(),
        };
        tx.execute(
            "INSERT INTO operations VALUES(?1,?2,?3,?4,1,?5,?6,?7,?8,?1)",
            params![
                operation.id.as_str(),
                operation.task.as_ref().unwrap().as_str(),
                operation.kind,
                operation.target,
                payload,
                format!("{:x}", Sha256::digest(payload.as_bytes())),
                integer(task.revision)?,
                now
            ],
        )?;
        tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('operation.enqueued',?1,?2,1,?3)",
            params![operation.id.as_str(),integer(task.revision)?,encode(&operation)?])?;
        if let Some(budget)=budget {budget.check()?;}
        tx.commit()?;
        Ok(operation)
    }

    pub fn record_worker_brief_delivered(
        &mut self,
        claim: &Claim,
        prepared: &PreparedWorkerBriefReceipt,
        now: i64,
    ) -> Result<Delivery> {
        self.record_worker_brief_with_budget(claim,prepared,now,None)
    }
    pub(crate) fn record_worker_brief_with_budget(&mut self,claim:&Claim,prepared:&PreparedWorkerBriefReceipt,now:i64,budget:Option<&read_budget::ReadBudget>)->Result<Delivery> {
        self.apply_worker_brief(Some(claim),None,claim.revision,prepared,now,budget)
    }

    /// Only independently retained exact submission evidence may recover a lost
    /// reply. This records an existing effect and never resubmits the prompt.
    pub fn observe_worker_brief_delivered(
        &mut self,
        prepared: &PreparedWorkerBriefReceipt,
        expected_revision: u64,
        expected_head: u64,
        now: i64,
    ) -> Result<Delivery> {
        self.apply_worker_brief(None, Some(expected_head), expected_revision, prepared, now, None)
    }

    fn apply_worker_brief(
        &mut self,
        claim: Option<&Claim>,
        expected_head: Option<u64>,
        expected_revision: u64,
        prepared: &PreparedWorkerBriefReceipt,
        now: i64,
        budget: Option<&read_budget::ReadBudget>,
    ) -> Result<Delivery> {
        if let Some(budget)=budget {budget.check()?;}
        super::delivery::now_check(now)?;
        let receipt = &prepared.receipt;
        if receipt.version != 1
            || receipt.observed_unix_ms < 0
            || receipt.observed_unix_ms > now
            || now - receipt.observed_unix_ms > 30_000
            || identity(&receipt.intent)? != receipt.operation
            || claim.is_some_and(|c| c.operation != receipt.operation)
        {
            return Err(invalid("invalid worker brief receipt"));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_schema(&tx)?;
        if expected_head.is_some_and(|h| head(&tx).ok() != Some(h)) {
            return Err(StoreError::Conflict);
        }
        let old = super::delivery::delivery_with_budget(&tx, &receipt.operation, budget)?;
        let payload = encode(receipt)?;
        if old.state == DeliveryState::Confirmed
            && old.last_outcome
                == Some(Outcome::Confirmed {
                    observed_identity: payload.clone(),
                })
            && has_receipt(&tx, &receipt.operation, &payload)?
        {
            return Ok(old);
        }
        if old.revision != expected_revision {
            return Err(StoreError::Conflict);
        }
        if let Some(claim) = claim {
            if old.state != DeliveryState::Claimed
                || old.epoch != claim.epoch
                || old.owner.as_deref() != Some(&claim.owner)
                || old.lease_until_ms != Some(claim.lease_until_ms)
                || now >= claim.lease_until_ms
            {
                return Err(StoreError::Conflict);
            }
        } else if old.state != DeliveryState::Ambiguous || old.attempts != 1 {
            return Err(StoreError::Conflict);
        }
        let op = read_operation_with_budget(&tx, &receipt.operation, budget)?;
        if op.kind != "runtime.worker_brief"
            || op.payload
                != serde_json::to_value(&receipt.intent)
                    .map_err(|_| invalid("brief encoding failed"))?
        {
            return Err(StoreError::Conflict);
        }
        let (record, ownership, mut attempt, task) = context_with_budget(&tx, &receipt.intent, budget)?;
        if op.task.as_ref() != Some(&task.id) || op.expected_revision != task.revision {
            return Err(StoreError::Conflict);
        }
        let start: String = read_budget::one(&tx,
            "SELECT payload FROM events WHERE kind='runtime.launch_started' AND entity=?1",
            [record.operation.as_str()],
            budget, &[(0,1)],
            |r| r.get(0),
        )?;
        let start: LaunchStartedReceipt =
            serde_json::from_str(&start).map_err(|_| invalid("invalid retained start receipt"))?;
        if ownership.session.as_ref() != Some(&receipt.session)
            || ownership.agent.as_ref() != Some(&receipt.agent)
            || start.terminal != receipt.terminal
            || receipt.observed_unix_ms < start.observed_unix_ms
        {
            return Err(invalid(
                "brief acknowledgment belongs to another worker incarnation",
            ));
        }
        // A receipt records an effect that already occurred. Pause, revocation,
        // expiry or later memory invalidation cannot erase it or authorize more
        // execution; those remain barriers to subsequent work.
        attempt.revision = attempt
            .revision
            .checked_add(1)
            .ok_or(StoreError::Conflict)?;
        attempt.state = AttemptState::Running;
        tx.execute(
            "UPDATE attempts SET revision=?2,state='running' WHERE id=?1",
            params![attempt.id.as_str(), integer(attempt.revision)?],
        )?;
        tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('runtime.worker_brief_delivered',?1,?2,1,?3)", params![receipt.operation.as_str(),integer(old.revision)?,payload])?;
        let schema: u32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if schema >= 43 {
            // A prompt can cross the external boundary before withdrawal is
            // committed. Preserve delivery as fact, but explicitly associate
            // the acknowledgment with its durable invalidation and stop route.
            // The exact-receipt replay above must not manufacture a later race.
            let stale: Option<(String,u64,u64)> = tx.query_row(
                "SELECT r.barrier_id,i.revocation_sequence,i.sequence FROM attempt_barrier_invalidations i
                 JOIN attempt_required_releases r ON r.attempt_id=i.attempt_id WHERE i.attempt_id=?1",
                [record.attempt.as_str()], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
            ).optional()?;
            if let Some((barrier, revocation, invalidation)) = stale {
                tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('runtime.worker_brief_stale',?1,?2,1,?3)",
                    params![receipt.operation.as_str(),integer(old.revision)?,serde_json::json!({
                        "attempt_id":record.attempt,"task_id":record.inputs.task,
                        "barrier_id":barrier,"revocation_sequence":revocation,
                        "invalidation_sequence":invalidation,"reason":"required_barrier_revoked_at_acknowledgment"
                    }).to_string()])?;
            }
        }
        tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('attempt.changed',?1,?2,1,?3)", params![attempt.id.as_str(),integer(attempt.revision)?,encode(&attempt)?])?;
        let result = super::delivery::update_outcome(
            &tx,
            &old,
            &Outcome::Confirmed {
                observed_identity: payload,
            },
            now,
            claim.map(|c| c.owner.as_str()).unwrap_or("brief-recovery"),
        )?;
        super::consumer_bindings::reconcile_active(&tx)?;
        if let Some(budget)=budget {budget.check()?;}
        tx.commit()?;
        Ok(result)
    }
}

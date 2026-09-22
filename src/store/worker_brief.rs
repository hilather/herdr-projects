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
fn context(
    db: &Connection,
    intent: &WorkerBriefIntent,
) -> Result<(AttemptInputRecord, RuntimeOwnership, Attempt, Task)> {
    validate(intent)?;
    let record = super::reservations::read_inputs(db)?
        .into_iter()
        .find(|r| r.attempt == intent.attempt && r.operation == intent.launch)
        .ok_or(StoreError::Conflict)?;
    if intent.binding != record.inputs.binding || intent.knowledge != record.inputs.memory {
        return Err(StoreError::Conflict);
    }
    let launch = super::delivery::delivery(db, &record.operation)?;
    let Some(Outcome::Confirmed { observed_identity }) = launch.last_outcome else {
        return Err(StoreError::Conflict);
    };
    if launch.state != DeliveryState::Confirmed
        || !super::launch::has_started_receipt(db, &record.operation, &observed_identity)?
    {
        return Err(StoreError::Conflict);
    }
    let binding = super::runtime::read_all(db)?
        .into_iter()
        .find(|b| b.id == intent.binding)
        .ok_or(StoreError::Conflict)?;
    let ownership = super::ownership::read_all(db)?
        .into_iter()
        .find(|o| o.binding == intent.binding)
        .ok_or(StoreError::Conflict)?;
    let attempt = read_attempts(db)?
        .into_iter()
        .find(|a| a.id == intent.attempt)
        .ok_or(StoreError::Conflict)?;
    let task = read_tasks(db)?
        .into_iter()
        .find(|t| t.id == record.inputs.task)
        .ok_or(StoreError::Conflict)?;
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
fn authority(db: &Connection, record: &AttemptInputRecord, now: i64) -> Result<()> {
    let inputs = &record.inputs;
    let control = super::control::read(db)?;
    if control.state != ProjectState::Active
        || control.reconciliation_required
        || control.epoch != inputs.control_epoch
        || control.config_digest != inputs.config.digest
        || super::reservations::read_cancellations(db)?
            .iter()
            .any(|c| c.attempt == record.attempt)
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
    super::worker_knowledge::validate(db, inputs, now)?;
    super::budget::check(db, inputs.budget.as_ref(), true)?;
    let approvals = super::approvals::read_all(db)?;
    let grant = approvals
        .iter()
        .find(|a| a.reference == inputs.approval)
        .ok_or(StoreError::Conflict)?;
    if grant.revoked.is_some()
        || !grant
            .consumed
            .as_ref()
            .is_some_and(|u| u.operation == record.operation)
    {
        return Err(invalid(
            "worker brief lacks active consumed launch approval",
        ));
    }
    grant
        .grant
        .matches_launch(inputs, &inputs.project_store, now)
        .map_err(|_| invalid("worker brief approval expired or changed"))?;
    Ok(())
}
pub(super) fn check(db: &Connection, id: &OperationId, now: i64) -> Result<()> {
    let op = read_operation(db, id)?;
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
    let (record, _, _, task) = context(db, &intent)?;
    if op.task.as_ref() != Some(&task.id) || op.expected_revision != task.revision {
        return Err(StoreError::Conflict);
    }
    authority(db, &record, now)
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
            return read_operation(&tx, &id);
        }
        let (record, _, _, task) = context(&tx, intent)?;
        authority(&tx, &record, now)?;
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
        tx.commit()?;
        Ok(operation)
    }

    pub fn record_worker_brief_delivered(
        &mut self,
        claim: &Claim,
        prepared: &PreparedWorkerBriefReceipt,
        now: i64,
    ) -> Result<Delivery> {
        self.apply_worker_brief(Some(claim), None, claim.revision, prepared, now)
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
        self.apply_worker_brief(None, Some(expected_head), expected_revision, prepared, now)
    }

    fn apply_worker_brief(
        &mut self,
        claim: Option<&Claim>,
        expected_head: Option<u64>,
        expected_revision: u64,
        prepared: &PreparedWorkerBriefReceipt,
        now: i64,
    ) -> Result<Delivery> {
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
        let old = super::delivery::delivery(&tx, &receipt.operation)?;
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
        let op = read_operation(&tx, &receipt.operation)?;
        if op.kind != "runtime.worker_brief"
            || op.payload
                != serde_json::to_value(&receipt.intent)
                    .map_err(|_| invalid("brief encoding failed"))?
        {
            return Err(StoreError::Conflict);
        }
        let (record, ownership, mut attempt, task) = context(&tx, &receipt.intent)?;
        if op.task.as_ref() != Some(&task.id) || op.expected_revision != task.revision {
            return Err(StoreError::Conflict);
        }
        let start: String = tx.query_row(
            "SELECT payload FROM events WHERE kind='runtime.launch_started' AND entity=?1",
            [record.operation.as_str()],
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
        tx.commit()?;
        Ok(result)
    }
}

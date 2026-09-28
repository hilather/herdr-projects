//! Durable one-use worktree creation and observation. No Git invocation here.
use super::*;
use crate::operations::{Claim, DeliveryState};
fn invalid(s: &str) -> StoreError {
    StoreError::Invalid(s.into())
}
fn validate_with_budget(db: &Connection,intent:&WorktreeCreation,budget:Option<&read_budget::ReadBudget>)->Result<()> {
    if intent.version != 1
        || intent.token.len() != 64
        || !intent
            .token
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return Err(invalid("invalid worktree creation identity"));
    }
    let record=super::reservations::read_input(db,&intent.operation,budget)?;
    if record.attempt!=intent.attempt {return Err(StoreError::Conflict);}
    if intent.plans.is_empty()
        || worktree_plans(&record.inputs, &record.attempt).map_err(StoreError::Invalid)?
            != intent.plans
    {
        return Err(invalid("worktree plans differ from approved launch"));
    }
    let binding=super::runtime::read_binding(db,&record.inputs.binding,budget)?.ok_or(StoreError::Conflict)?;
    let attempt=read_attempt_with_budget(db,&intent.attempt,budget)?;
    if binding.revision != record.inputs.binding_revision
        || super::ownership::identity_digest(&binding)? != record.inputs.binding_digest
        || !binding.identity.machine.is_empty()
        || !binding.identity.pane_id.is_empty()
        || !binding.identity.tab_id.is_empty()
        || !binding.identity.worktree_path.is_empty()
        || !(attempt.state == AttemptState::Reserved && attempt.retains_capacity())
    {
        return Err(StoreError::Conflict);
    }
    Ok(())
}
pub(super) fn record_creation(
    db: &Connection,
    claim: &Claim,
    prepared: &PreparedWorktreeCreation,
    now: i64,
    budget: Option<&read_budget::ReadBudget>,
) -> Result<()> {
    let intent = &prepared.intent;
    validate_with_budget(db, intent, budget)?;
    if claim.operation != intent.operation {
        return Err(StoreError::Conflict);
    }
    super::approvals::validate_use_with_budget(db, claim, now, budget)?;
    let exists:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM events WHERE entity=?1 AND kind IN ('runtime.worktrees_creation','runtime.worktrees_ready','runtime.launch_creation','runtime.launch_target','runtime.launch_started'))",[claim.operation.as_str()],|r|r.get(0))?;
    if exists {
        return Err(invalid("worktree creation already attempted"));
    }
    db.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('runtime.worktrees_creation',?1,?2,1,?3)",params![claim.operation.as_str(),integer(claim.revision)?,serde_json::to_string(intent).map_err(|_|invalid("worktree intent encoding failed"))?])?;
    Ok(())
}
pub(crate) struct PreparationSelection {
    pub record: AttemptInputRecord,
    pub delivery: crate::operations::Delivery,
    pub binding: RuntimeBinding,
    pub events: Vec<Event>,
}
impl SqliteStore {
    pub(super) fn preparation_selection(&mut self, operation: &OperationId, expected: u64, budget: &read_budget::ReadBudget) -> Result<PreparationSelection> {
        budget.check()?;
        let tx = self.connection.transaction()?;
        let delivery = super::delivery::delivery_with_budget(&tx, operation, Some(budget))?;
        if delivery.revision != expected { return Err(StoreError::Conflict); }
        let record = super::reservations::read_input(&tx, operation, Some(budget))?;
        let binding = super::runtime::read_binding(&tx, &record.inputs.binding, Some(budget))?.ok_or(StoreError::Conflict)?;
        let mut statement = tx.prepare("SELECT sequence,kind,entity,revision,payload_version,payload FROM events WHERE entity=?1 AND kind='runtime.worktrees_creation' LIMIT 2")?;
        let mut rows = statement.query([operation.as_str()])?;
        let mut events = Vec::new();
        while let Some(row) = rows.next()? {
            budget.row(row, &[(5,1)])?;
            let payload: String = row.get(5)?;
            events.push(Event { sequence: row.get(0)?, kind: row.get(1)?, entity: row.get(2)?, revision: row.get(3)?, payload_version: row.get(4)?,
                payload: serde_json::from_str(&payload).map_err(|_| invalid("invalid preparation evidence"))? });
        }
        budget.check()?;
        Ok(PreparationSelection { record, delivery, binding, events })
    }
}

impl SqliteStore {
    pub(crate) fn observe_worktrees_with_budget(&mut self, prepared: &PreparedWorktreeReceipts, expected_revision: u64, expected_head: u64, now: i64, budget: Option<&read_budget::ReadBudget>) -> Result<()> {
        if let Some(budget) = budget { budget.check()?; }
        super::delivery::now_check(now)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_schema(&tx)?;
        if head(&tx)? != expected_head {
            return Err(StoreError::Conflict);
        }
        let intent = &prepared.intent;
        validate_with_budget(&tx, intent, budget)?;
        let delivery = super::delivery::delivery_with_budget(&tx, &intent.operation, budget)?;
        if delivery.revision != expected_revision
            || delivery.attempts != 1
            || !matches!(
                delivery.state,
                DeliveryState::Claimed | DeliveryState::Ambiguous
            )
        {
            return Err(StoreError::Conflict);
        }
        let payload = serde_json::to_string(intent)
            .map_err(|_| invalid("worktree intent encoding failed"))?;
        let exact:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM events WHERE kind='runtime.worktrees_creation' AND entity=?1 AND payload=?2)",params![intent.operation.as_str(),payload],|r|r.get(0))?;
        if !exact { return Err(StoreError::Conflict); }
        let record = super::reservations::read_input(&tx, &intent.operation, budget)?;
        super::approvals::validate_historical_consumption(&tx, &record, budget)?;
        if prepared.receipts.len() != intent.plans.len() {
            return Err(invalid("incomplete worktree receipts"));
        }
        let mut identities = std::collections::BTreeSet::new();
        for (receipt, plan) in prepared.receipts.iter().zip(&intent.plans) {
            if receipt.plan != *plan
                || !Path::new(&receipt.git_directory).is_absolute()
                || !Path::new(&receipt.common_directory).is_absolute()
                || [
                    &receipt.directory,
                    &receipt.git_identity,
                    &receipt.common_identity,
                ]
                .iter()
                .any(|i| i.inode == 0 || i.born_nanos >= 1_000_000_000)
                || !identities.insert((receipt.directory.device, receipt.directory.inode))
            {
                return Err(invalid("invalid worktree receipt"));
            }
        }
        let payload = serde_json::to_string(&prepared.receipts)
            .map_err(|_| invalid("worktree receipt encoding failed"))?;
        if payload.len() > MAX_RECORD_BYTES {
            return Err(invalid("worktree receipts exceed bounds"));
        }
        let mut stmt = tx.prepare("SELECT payload FROM events WHERE kind='runtime.worktrees_ready' AND entity=?1 LIMIT 2")?;
        let mut rows = stmt.query([intent.operation.as_str()])?;
        let mut existing = Vec::new();
        while let Some(row) = rows.next()? {
            if let Some(budget) = budget { budget.row(row, &[])?; }
            existing.push(row.get::<_, String>(0)?);
        }
        drop(rows);
        drop(stmt);
        if !existing.is_empty() {
            if existing == vec![payload] {
                return Ok(());
            }
            return Err(StoreError::Conflict);
        }
        tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('runtime.worktrees_ready',?1,?2,1,?3)",params![intent.operation.as_str(),integer(expected_revision)?,payload])?;
        if let Some(budget) = budget { budget.check()?; }
        tx.commit()?;
        Ok(())
    }
}

/// Native routes may use only the retained, complete worktree receipt vector.
/// These rows establish provenance; the adapter separately rechecks live files.
pub(super) fn execution_route_with_budget(
    db: &Connection, record: &AttemptInputRecord, binding: &RuntimeBinding,
    budget: Option<&read_budget::ReadBudget>,
) -> Result<(RuntimeRoute, Option<WorktreeReceipt>)> {
    if let Some(budget) = budget { budget.check()?; }
    let (route, primary) =
        worktree_execution_route(&record.inputs, &record.attempt, &binding.identity)
            .map_err(StoreError::Invalid)?;
    let Some(primary) = primary else {
        return Ok((route, None));
    };
    let read = |kind: &str| -> Result<String> {
        let mut statement = db.prepare("SELECT payload FROM events WHERE kind=?1 AND entity=?2 LIMIT 2")?;
        let mut rows = statement.query(params![kind, record.operation.as_str()])?;
        let mut values = Vec::new();
        while let Some(row) = rows.next()? {
            if let Some(budget) = budget { budget.row(row, &[(0,1)])?; }
            values.push(row.get::<_, String>(0)?);
        }
        if values.len() != 1 || values[0].len() > MAX_RECORD_BYTES {
            return Err(invalid(
                "worktree launch requires one complete retained receipt",
            ));
        }
        Ok(values.into_iter().next().unwrap())
    };
    let intent: WorktreeCreation = serde_json::from_str(&read("runtime.worktrees_creation")?)
        .map_err(|_| invalid("invalid worktree creation record"))?;
    validate_with_budget(db, &intent, budget)?;
    let receipts: Vec<WorktreeReceipt> = serde_json::from_str(&read("runtime.worktrees_ready")?)
        .map_err(|_| invalid("invalid worktree ready record"))?;
    if receipts.len() != intent.plans.len()
        || receipts.iter().zip(&intent.plans).any(|(r, p)| {
            r.plan != *p || r.directory.inode == 0 || r.directory.born_nanos >= 1_000_000_000
        })
    {
        return Err(invalid("worktree receipts do not match approved plans"));
    }
    super::approvals::validate_historical_consumption(db, record, budget)?;
    let receipt = receipts
        .into_iter()
        .find(|r| r.plan == primary)
        .ok_or(StoreError::Conflict)?;
    Ok((route, Some(receipt)))
}

impl SqliteStore {
    /// Called only while the canonical adapter holds the exclusive root barrier.
    /// Git descendants inherit that barrier, so acquisition establishes their
    /// quiescence. Absence of a native intent then proves no worker was launched.
    pub(crate) fn stop_worktree_preparation(
        &mut self,
        attempt_id: &AttemptId,
        expected_revision: u64,
        expected_head: u64,
        _guard: &crate::execution_guard::RootGuard,
        preserve: impl FnOnce(&AttemptInputRecord,&WorktreeCreation)->Result<(Vec<PreparationSnapshotReference>,AttemptOutputReference)>,
        now: i64,
    ) -> Result<Option<Attempt>> {
        self.stop_worktree_preparation_with_budget(attempt_id,expected_revision,expected_head,_guard,preserve,now,None)
    }
    pub(crate) fn stop_worktree_preparation_with_budget(&mut self,attempt_id:&AttemptId,expected_revision:u64,expected_head:u64,_guard:&crate::execution_guard::RootGuard,preserve:impl FnOnce(&AttemptInputRecord,&WorktreeCreation)->Result<(Vec<PreparationSnapshotReference>,AttemptOutputReference)>,now:i64,budget:Option<&read_budget::ReadBudget>)->Result<Option<Attempt>> {
        if let Some(budget)=budget {budget.check()?;}
        use crate::operations::Outcome;
        super::delivery::now_check(now)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Deferred)?;
        let Some(selected) = preparation_stop_selection(&tx, attempt_id, expected_revision, expected_head, now, budget)? else {
            return Ok(None);
        };
        tx.commit()?;
        if let Some(budget) = budget { budget.check()?; }
        // Keep the inherited root barrier while capturing, but release SQLite:
        // Git and filesystem preservation must never hold a database transaction.
        let (repository_snapshots, output_snapshot) = preserve(&selected.record, &selected.intent)?;
        if let Some(budget) = budget { budget.check()?; }
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = preparation_stop_selection(&tx, attempt_id, expected_revision, expected_head, now, budget)?
            .ok_or(StoreError::Conflict)?;
        if current != selected { return Err(StoreError::Conflict); }
        let PreparationStopSelection { mut attempt, mut task, record, intent, delivery, cancelled } = current;
        let valid_hash=|s:&str|s.len()==64&&s.bytes().all(|b|b.is_ascii_digit()||(b'a'..=b'f').contains(&b));
        if !repository_snapshots.iter().map(|s|&s.plan).eq(intent.plans.iter())
            || repository_snapshots.iter().any(|s|s.digest.as_ref().is_some_and(|d|!valid_hash(d)))
            || output_snapshot.source!=worker_output_path(&record.inputs,&record.attempt).map_err(StoreError::Invalid)?
            || output_snapshot.digest.as_ref().is_some_and(|d|!valid_hash(d)) {
            return Err(invalid("preparation stop requires exact preservation evidence"));
        }
        if delivery.state != DeliveryState::PermanentFailure {
            super::delivery::update_outcome(&tx,&delivery,&Outcome::PermanentFailure{diagnostic:"worktree preparation quiescent before native launch intent; checkout references retained".into()},now,"worktree-preparation-stop")?;
        }
        attempt.revision = attempt
            .revision
            .checked_add(1)
            .ok_or(StoreError::Conflict)?;
        attempt.termination_observed = true;
        attempt.state = if cancelled {
            AttemptState::Cancelled
        } else {
            AttemptState::Failed
        };
        task.revision = task.revision.checked_add(1).ok_or(StoreError::Conflict)?;
        task.active_attempt = None;
        task.state = if cancelled {
            TaskState::Cancelled
        } else {
            TaskState::Blocked
        };
        tx.execute(
            "UPDATE attempts SET revision=?2,state=?3,termination_observed=1 WHERE id=?1",
            params![
                attempt.id.as_str(),
                integer(attempt.revision)?,
                attempt.state.as_str()
            ],
        )?;
        super::dispatch_log::mark(&tx, &attempt, now, "stop_worktree_preparation_with_budget")?;
        tx.execute(
            "UPDATE tasks SET revision=?2,state=?3,active_attempt=NULL WHERE id=?1",
            params![
                task.id.as_str(),
                integer(task.revision)?,
                task.state.as_str()
            ],
        )?;
        for (kind, entity, revision, payload) in [
            (
                "attempt.changed",
                attempt.id.as_str(),
                attempt.revision,
                serde_json::to_value(&attempt),
            ),
            (
                "task.changed",
                task.id.as_str(),
                task.revision,
                serde_json::to_value(&task),
            ),
            (
                "runtime.worktrees_stopped",
                record.operation.as_str(),
                attempt.revision,
                Ok(
                    serde_json::json!({"version":2,"attempt":attempt.id,"operation":record.operation,"retained":intent,"repository_snapshots":repository_snapshots,"output_snapshot":output_snapshot,"observed_unix_ms":now,"cancelled":cancelled}),
                ),
            ),
        ] {
            tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES(?1,?2,?3,1,?4)",params![kind,entity,integer(revision)?,payload.map_err(|_|invalid("worktree stop encoding failed"))?.to_string()])?;
        }
        super::consumer_bindings::reconcile_task(&tx,attempt.task.as_str(),budget)?;
        if let Some(budget)=budget {budget.check()?;}
        tx.commit()?;
        Ok(Some(attempt))
    }
}

#[derive(PartialEq, Eq)]
struct PreparationStopSelection {
    attempt: Attempt,
    task: Task,
    record: AttemptInputRecord,
    intent: WorktreeCreation,
    delivery: crate::operations::Delivery,
    cancelled: bool,
}
fn preparation_stop_selection(
    db: &Connection,
    attempt_id: &AttemptId,
    expected_revision: u64,
    expected_head: u64,
    now: i64,
    budget: Option<&read_budget::ReadBudget>,
) -> Result<Option<PreparationStopSelection>> {
    if let Some(budget) = budget { budget.check()?; }
    check_schema(db)?;
    if head(db)? != expected_head {
        return Err(StoreError::Conflict);
    }
    let attempt=read_attempt_with_budget(db,attempt_id,budget)?;
    if attempt.revision != expected_revision
        || attempt.termination_observed
        || attempt.state != AttemptState::Reserved
        || !attempt.retains_capacity()
    {
        return Err(StoreError::Conflict);
    }
    let record=super::reservations::read_attempt_input(db,attempt_id.as_str(),budget)?;
    let mut stmt=db.prepare("SELECT payload FROM events WHERE kind='runtime.worktrees_creation' AND entity=?1 LIMIT 2")?;
    let mut rows=stmt.query([record.operation.as_str()])?;
    let mut payloads=Vec::new();
    while let Some(row)=rows.next()? {
        if let Some(budget)=budget {budget.row(row,&[(0,1)])?;}
        payloads.push(row.get::<_,String>(0)?);
    }
    drop(rows);
    drop(stmt);
    if payloads.len() != 1 || payloads[0].len() > MAX_RECORD_BYTES {
        return Err(StoreError::Conflict);
    }
    let intent: WorktreeCreation =
        serde_json::from_str(&payloads[0]).map_err(|_| invalid("invalid worktree intent"))?;
    validate_with_budget(db, &intent,budget)?;
    if intent.attempt != attempt.id || intent.operation != record.operation {
        return Err(StoreError::Conflict);
    }
    let native: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM events WHERE entity=?1 AND kind GLOB 'runtime.launch_*')",
        [record.operation.as_str()],
        |r| r.get(0),
    )?;
    if native
        || db.query_row("SELECT EXISTS(SELECT 1 FROM runtime_ownership WHERE binding_id=?1) OR EXISTS(SELECT 1 FROM runtime_ownership WHERE attempt_id=?2)",params![record.inputs.binding,attempt_id.as_str()],|row|row.get::<_,bool>(0))?
    {
        return Err(StoreError::Conflict);
    }
    let delivery = super::delivery::delivery_with_budget(db, &record.operation,budget)?;
    super::approvals::validate_historical_consumption(db,&record,budget)?;
    if delivery.attempts != 1
        || delivery.state == DeliveryState::Confirmed
    {
        return Err(StoreError::Conflict);
    }
    let cancelled:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM attempt_cancellations WHERE attempt_id=?1)",[attempt_id.as_str()],|row|row.get(0))?;
    let expired = delivery.lease_until_ms.is_some_and(|until| until <= now)
        || delivery.state == DeliveryState::Ambiguous
        || delivery.state == DeliveryState::PermanentFailure;
    if !cancelled && !expired {
        return Ok(None);
    }
    let task=read_task_with_budget(db,attempt.task.as_str(),budget)?;
    if task.active_attempt.as_ref() != Some(attempt_id) {
        return Err(StoreError::Conflict);
    }
    Ok(Some(PreparationStopSelection { attempt, task, record, intent, delivery, cancelled }))
}

//! Durable one-use worktree creation and observation. No Git invocation here.
use super::*;
use crate::operations::{Claim, DeliveryState};
fn invalid(s: &str) -> StoreError {
    StoreError::Invalid(s.into())
}
fn validate(db: &Connection, intent: &WorktreeCreation) -> Result<()> {
    if intent.version != 1
        || intent.token.len() != 64
        || !intent
            .token
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return Err(invalid("invalid worktree creation identity"));
    }
    let record = super::reservations::read_inputs(db)?
        .into_iter()
        .find(|r| r.operation == intent.operation && r.attempt == intent.attempt)
        .ok_or(StoreError::Conflict)?;
    if intent.plans.is_empty()
        || worktree_plans(&record.inputs, &record.attempt).map_err(StoreError::Invalid)?
            != intent.plans
    {
        return Err(invalid("worktree plans differ from approved launch"));
    }
    let binding = super::runtime::read_all(db)?
        .into_iter()
        .find(|b| b.id == record.inputs.binding)
        .ok_or(StoreError::Conflict)?;
    if binding.revision != record.inputs.binding_revision
        || super::ownership::identity_digest(&binding)? != record.inputs.binding_digest
        || !binding.identity.machine.is_empty()
        || !binding.identity.pane_id.is_empty()
        || !binding.identity.tab_id.is_empty()
        || !binding.identity.worktree_path.is_empty()
        || !read_attempts(db)?.iter().any(|a| {
            a.id == intent.attempt && a.state == AttemptState::Reserved && a.retains_capacity()
        })
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
) -> Result<()> {
    let intent = &prepared.intent;
    validate(db, intent)?;
    if claim.operation != intent.operation {
        return Err(StoreError::Conflict);
    }
    super::approvals::validate_use(db, claim, now)?;
    let exists:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM events WHERE entity=?1 AND kind IN ('runtime.worktrees_creation','runtime.worktrees_ready','runtime.launch_creation','runtime.launch_target','runtime.launch_started'))",[claim.operation.as_str()],|r|r.get(0))?;
    if exists {
        return Err(invalid("worktree creation already attempted"));
    }
    db.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('runtime.worktrees_creation',?1,?2,1,?3)",params![claim.operation.as_str(),integer(claim.revision)?,serde_json::to_string(intent).map_err(|_|invalid("worktree intent encoding failed"))?])?;
    Ok(())
}
impl SqliteStore {
    pub(crate) fn claim_worktree_creation(
        &mut self,
        expected: u64,
        prepared: &PreparedWorktreeCreation,
        now: i64,
        lease_ms: i64,
    ) -> Result<Claim> {
        self.claim_with_creation(
            &prepared.intent.operation,
            expected,
            "canonical-worktree-adapter",
            now,
            lease_ms,
            Some(super::delivery::LaunchPreparation::Worktrees(prepared)),
        )
    }
    pub(crate) fn observe_worktrees(
        &mut self,
        prepared: &PreparedWorktreeReceipts,
        expected_revision: u64,
        expected_head: u64,
        now: i64,
    ) -> Result<()> {
        super::delivery::now_check(now)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_schema(&tx)?;
        if head(&tx)? != expected_head {
            return Err(StoreError::Conflict);
        }
        let intent = &prepared.intent;
        validate(&tx, intent)?;
        let delivery = super::delivery::delivery(&tx, &intent.operation)?;
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
        if !exact
            || !super::approvals::read_all(&tx)?.iter().any(|g| {
                g.consumed
                    .as_ref()
                    .is_some_and(|u| u.operation == intent.operation)
            })
        {
            return Err(StoreError::Conflict);
        }
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
        let mut stmt = tx.prepare(
            "SELECT payload FROM events WHERE kind='runtime.worktrees_ready' AND entity=?1",
        )?;
        let existing = stmt
            .query_map([intent.operation.as_str()], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        if !existing.is_empty() {
            if existing == vec![payload] {
                return Ok(());
            }
            return Err(StoreError::Conflict);
        }
        tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('runtime.worktrees_ready',?1,?2,1,?3)",params![intent.operation.as_str(),integer(expected_revision)?,payload])?;
        tx.commit()?;
        Ok(())
    }
}

/// Native routes may use only the retained, complete worktree receipt vector.
/// These rows establish provenance; the adapter separately rechecks live files.
pub(super) fn execution_route(
    db: &Connection,
    record: &AttemptInputRecord,
    binding: &RuntimeBinding,
) -> Result<(RuntimeRoute, Option<WorktreeReceipt>)> {
    let (route, primary) =
        worktree_execution_route(&record.inputs, &record.attempt, &binding.identity)
            .map_err(StoreError::Invalid)?;
    let Some(primary) = primary else {
        return Ok((route, None));
    };
    let read = |kind: &str| -> Result<String> {
        let mut statement = db.prepare("SELECT payload FROM events WHERE kind=?1 AND entity=?2")?;
        let values = statement
            .query_map(params![kind, record.operation.as_str()], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if values.len() != 1 || values[0].len() > MAX_RECORD_BYTES {
            return Err(invalid(
                "worktree launch requires one complete retained receipt",
            ));
        }
        Ok(values.into_iter().next().unwrap())
    };
    let intent: WorktreeCreation = serde_json::from_str(&read("runtime.worktrees_creation")?)
        .map_err(|_| invalid("invalid worktree creation record"))?;
    validate(db, &intent)?;
    let receipts: Vec<WorktreeReceipt> = serde_json::from_str(&read("runtime.worktrees_ready")?)
        .map_err(|_| invalid("invalid worktree ready record"))?;
    if receipts.len() != intent.plans.len()
        || receipts.iter().zip(&intent.plans).any(|(r, p)| {
            r.plan != *p || r.directory.inode == 0 || r.directory.born_nanos >= 1_000_000_000
        })
    {
        return Err(invalid("worktree receipts do not match approved plans"));
    }
    if !super::approvals::read_all(db)?.iter().any(|g| {
        g.consumed
            .as_ref()
            .is_some_and(|u| u.operation == record.operation)
    }) {
        return Err(StoreError::Conflict);
    }
    let receipt = receipts
        .into_iter()
        .find(|r| r.plan == primary)
        .ok_or(StoreError::Conflict)?;
    Ok((route, Some(receipt)))
}
impl SqliteStore {
    pub(crate) fn worktree_execution_route(
        &mut self,
        record: &AttemptInputRecord,
        binding: &RuntimeBinding,
    ) -> Result<(RuntimeRoute, Option<WorktreeReceipt>)> {
        execution_route(&self.connection, record, binding)
    }
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
        use crate::operations::Outcome;
        super::delivery::now_check(now)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_schema(&tx)?;
        if head(&tx)? != expected_head {
            return Err(StoreError::Conflict);
        }
        let mut attempt = read_attempts(&tx)?
            .into_iter()
            .find(|a| &a.id == attempt_id)
            .ok_or(StoreError::Conflict)?;
        if attempt.revision != expected_revision
            || attempt.termination_observed
            || attempt.state != AttemptState::Reserved
            || !attempt.retains_capacity()
        {
            return Err(StoreError::Conflict);
        }
        let record = super::reservations::read_inputs(&tx)?
            .into_iter()
            .find(|r| &r.attempt == attempt_id)
            .ok_or(StoreError::Conflict)?;
        let mut stmt=tx.prepare("SELECT payload FROM events WHERE kind='runtime.worktrees_creation' AND entity=?1 LIMIT 2")?;
        let payloads = stmt
            .query_map([record.operation.as_str()], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        if payloads.len() != 1 || payloads[0].len() > MAX_RECORD_BYTES {
            return Err(StoreError::Conflict);
        }
        let intent: WorktreeCreation =
            serde_json::from_str(&payloads[0]).map_err(|_| invalid("invalid worktree intent"))?;
        validate(&tx, &intent)?;
        if intent.attempt != attempt.id || intent.operation != record.operation {
            return Err(StoreError::Conflict);
        }
        let native: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM events WHERE entity=?1 AND kind GLOB 'runtime.launch_*')",
            [record.operation.as_str()],
            |r| r.get(0),
        )?;
        if native
            || super::ownership::read_all(&tx)?.iter().any(|o| {
                o.binding == record.inputs.binding || o.attempt.as_ref() == Some(attempt_id)
            })
        {
            return Err(StoreError::Conflict);
        }
        let delivery = super::delivery::delivery(&tx, &record.operation)?;
        if delivery.attempts != 1
            || delivery.state == DeliveryState::Confirmed
            || !super::approvals::read_all(&tx)?.iter().any(|g| {
                g.consumed
                    .as_ref()
                    .is_some_and(|u| u.operation == record.operation)
            })
        {
            return Err(StoreError::Conflict);
        }
        let cancelled = super::reservations::read_cancellations(&tx)?
            .iter()
            .any(|c| &c.attempt == attempt_id);
        let expired = delivery.lease_until_ms.is_some_and(|until| until <= now)
            || delivery.state == DeliveryState::Ambiguous
            || delivery.state == DeliveryState::PermanentFailure;
        if !cancelled && !expired {
            return Ok(None);
        }
        let mut task = read_tasks(&tx)?
            .into_iter()
            .find(|t| t.id == attempt.task)
            .ok_or(StoreError::Conflict)?;
        if task.active_attempt.as_ref() != Some(attempt_id) {
            return Err(StoreError::Conflict);
        }
        // Filesystem capture runs only after eligibility and quiescence checks,
        // under the inherited root barrier and this unchanged state transaction.
        let (repository_snapshots,output_snapshot)=preserve(&record,&intent)?;
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
        tx.commit()?;
        Ok(Some(attempt))
    }
}

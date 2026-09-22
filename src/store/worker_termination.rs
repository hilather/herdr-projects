use super::*;
use crate::operations::{DeliveryState, Outcome};

fn validate_preservation(record:&AttemptInputRecord,snapshots:&[WorktreeSnapshotReference])->Result<()> {
    let plans=if record.inputs.repositories.is_empty(){vec![]}else{worktree_plans(&record.inputs,&record.attempt).map_err(StoreError::Invalid)?};
    if !snapshots.iter().map(|s|&s.plan).eq(plans.iter()) || snapshots.iter().any(|s|
        s.digest.len()!=64 || !s.digest.bytes().all(|b|b.is_ascii_digit() || (b'a'..=b'f').contains(&b))) {
        return Err(StoreError::Invalid("termination requires exact repository preservation references".into()));
    }
    Ok(())
}
fn validate_outputs(record:&AttemptInputRecord,output:Option<&AttemptOutputReference>)->Result<()> {
    let expected=worker_output_path(&record.inputs,&record.attempt).map_err(StoreError::Invalid)?;
    if !output.is_some_and(|value|value.source==expected && value.digest.as_ref().is_none_or(|digest|
        digest.len()==64 && digest.bytes().all(|b|b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))) {
        return Err(StoreError::Invalid("termination requires exact output preservation evidence".into()));
    }
    Ok(())
}

// A supervised worker is not the only executable resource of historical root
// creation. Its bootstrap shell has no durable descendant-quiescence evidence.
// A same-host reboot is also proof that all of that boot's descendants stopped.
fn require_workspace_quiescence(db:&Connection,operation:&OperationId,supervisor:&crate::worker_supervision::SupervisorIdentity,reboot:Option<&crate::worker_supervision::HostRebootEvidence>)->Result<()> {
    if let Some(evidence)=reboot {evidence.validate_for(supervisor).map_err(|_|StoreError::Invalid("invalid workspace reboot evidence".into()))?;}
    let mut query=db.prepare("SELECT kind,payload FROM events WHERE entity=?1 AND kind IN ('runtime.launch_creation','runtime.launch_workspace')")?;
    let mut rows=query.query([operation.as_str()])?;
    let mut kinds=std::collections::BTreeSet::new();
    while let Some(row)=rows.next()? {
        let kind:String=row.get(0)?;
        if !kinds.insert(kind.clone()) {return Err(StoreError::Corrupt("duplicate workspace provenance".into()));}
        let payload:String=row.get(1)?;
        let historical=if kind=="runtime.launch_creation" {
            let intent:LaunchCreationIntent=serde_json::from_str(&payload).map_err(|_|StoreError::Corrupt("invalid workspace creation provenance".into()))?;
            intent.version==1 && intent.route.workspace_id.is_empty()
        } else {
            let workspace:LaunchTarget=serde_json::from_str(&payload).map_err(|_|StoreError::Corrupt("invalid workspace provenance".into()))?;
            workspace.version==1
        };
        if historical && reboot.is_none() {return Err(StoreError::Invalid("historical workspace bootstrap quiescence is unresolved; worker stop cannot release capacity".into()));}
    }
    Ok(())
}

impl SqliteStore {
    pub(crate) fn validate_workspace_quiescence(&self,operation:&OperationId,supervisor:&crate::worker_supervision::SupervisorIdentity,reboot:Option<&crate::worker_supervision::HostRebootEvidence>)->Result<()> {
        require_workspace_quiescence(&self.connection,operation,supervisor,reboot)
    }
    /// Exact process quiescence frees capacity but never deletes runtime records,
    /// finalizes artifacts or establishes verified task success.
    pub fn record_worker_termination(
        &mut self,
        prepared: &PreparedWorkerTermination,
        expected_revision: u64,
        expected_head: u64,
        now: i64,
    ) -> Result<Attempt> {
        super::delivery::now_check(now)?;
        let receipt = &prepared.receipt;
        receipt
            .supervisor
            .validate()
            .map_err(|_| StoreError::Invalid("invalid worker termination identity".into()))?;
        if receipt.version != 1
            || receipt.observed_unix_ms < 0
            || receipt.observed_unix_ms > now
            || now - receipt.observed_unix_ms > 30_000
        {
            return Err(StoreError::Invalid(
                "invalid worker termination observation".into(),
            ));
        }
        let payload = serde_json::to_string(receipt)
            .map_err(|_| StoreError::Invalid("termination receipt encoding failed".into()))?;
        if payload.len() > MAX_RECORD_BYTES {
            return Err(StoreError::Invalid(
                "termination receipt exceeds bounds".into(),
            ));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_schema(&tx)?;
        if head(&tx)? != expected_head {
            return Err(StoreError::Conflict);
        }
        let mut attempt = read_attempts(&tx)?
            .into_iter()
            .find(|a| a.id == receipt.attempt)
            .ok_or(StoreError::Conflict)?;
        if attempt.termination_observed {
            let exact:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM events WHERE kind='runtime.worker_terminated' AND entity=?1 AND payload=?2)",params![attempt.id.as_str(),payload],|r|r.get(0))?;
            if exact {
                return Ok(attempt);
            }
            return Err(StoreError::Conflict);
        }
        if attempt.revision != expected_revision {
            return Err(StoreError::Conflict);
        }
        let record = super::reservations::read_inputs(&tx)?
            .into_iter()
            .find(|r| r.attempt == receipt.attempt && r.operation == receipt.launch)
            .ok_or(StoreError::Conflict)?;
        require_workspace_quiescence(&tx,&record.operation,&receipt.supervisor,receipt.host_reboot.as_ref())?;
        validate_preservation(&record,&receipt.repository_snapshots)?;
        validate_outputs(&record,receipt.output_snapshot.as_ref())?;
        let binding = super::runtime::read_all(&tx)?
            .into_iter()
            .find(|b| b.id == receipt.binding)
            .ok_or(StoreError::Conflict)?;
        let owner = super::ownership::read_all(&tx)?
            .into_iter()
            .find(|o| o.binding == binding.id)
            .ok_or(StoreError::Conflict)?;
        let mut task = read_tasks(&tx)?
            .into_iter()
            .find(|t| t.id == attempt.task)
            .ok_or(StoreError::Conflict)?;
        if record.inputs.binding != binding.id
            || binding.revision != receipt.binding_revision
            || binding.identity != receipt.retained_resources
            || owner.origin != "launched"
            || owner.attempt.as_ref() != Some(&attempt.id)
            || owner.revision != receipt.ownership_revision
            || owner.binding_revision != binding.revision
            || owner.identity_digest != super::ownership::identity_digest(&binding)?
            || task.active_attempt.as_ref() != Some(&attempt.id)
        {
            return Err(StoreError::Conflict);
        }
        let payload_start: String = tx.query_row(
            "SELECT payload FROM events WHERE kind='runtime.launch_started' AND entity=?1",
            [record.operation.as_str()],
            |r| r.get(0),
        )?;
        let start: LaunchStartedReceipt = serde_json::from_str(&payload_start)
            .map_err(|_| StoreError::Corrupt("invalid worker launch receipt".into()))?;
        let launch = super::delivery::delivery(&tx, &record.operation)?;
        if launch.state != DeliveryState::Confirmed
            || launch.last_outcome
                != Some(Outcome::Confirmed {
                    observed_identity: payload_start,
                })
            || start.version != 2
            || start.supervisor.as_ref() != Some(&receipt.supervisor)
            || start.attempt != attempt.id
            || start.observed_unix_ms > receipt.observed_unix_ms
        {
            return Err(StoreError::Conflict);
        }
        let cancelled = super::reservations::read_cancellations(&tx)?
            .iter()
            .any(|c| c.attempt == attempt.id);
        if (receipt.cause == WorkerTerminationCause::Cancellation) != cancelled {
            return Err(StoreError::Conflict);
        }
        // Retire outstanding brief obligations without asserting whether an
        // uncertain submission happened. Every old claim is fenced atomically.
        for operation in read_operations(&tx)?.into_iter().filter(|o| {
            o.kind == "runtime.worker_brief"
                && o.payload["attempt"].as_str() == Some(attempt.id.as_str())
        }) {
            let delivery = super::delivery::delivery(&tx, &operation.id)?;
            if !matches!(
                delivery.state,
                DeliveryState::Confirmed | DeliveryState::PermanentFailure
            ) {
                super::delivery::update_outcome(&tx,&delivery,&Outcome::PermanentFailure{diagnostic:"worker termination observed; brief obligation retired without replay or a claim that uncertain submission was absent".into()},now,"worker-termination")?;
            }
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
        for (kind, entity, revision, value) in [
            (
                "runtime.worker_resources_retained",
                binding.id.clone(),
                binding.revision,
                serde_json::to_string(&receipt.retained_resources),
            ),
            (
                "attempt.changed",
                attempt.id.as_str().into(),
                attempt.revision,
                serde_json::to_string(&attempt),
            ),
            (
                "task.changed",
                task.id.as_str().into(),
                task.revision,
                serde_json::to_string(&task),
            ),
            (
                "runtime.worker_terminated",
                attempt.id.as_str().into(),
                attempt.revision,
                Ok(payload),
            ),
        ] {
            tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES(?1,?2,?3,1,?4)",params![kind,entity,integer(revision)?,value.map_err(|_|StoreError::Invalid("termination event encoding failed".into()))?])?;
        }
        tx.commit()?;
        Ok(attempt)
    }
}

impl SqliteStore {
    /// Observe quiescence of a staged resource without claiming that an agent
    /// started, preserving the selected target as an outstanding resource record.
    pub fn record_launch_stopped(
        &mut self,
        prepared: &PreparedLaunchStopped,
        expected_revision: u64,
        expected_head: u64,
        now: i64,
    ) -> Result<Attempt> {
        super::delivery::now_check(now)?;
        let receipt = &prepared.receipt;
        if receipt.version != 1
            || receipt.target.version != 2
            || receipt.observed_unix_ms < receipt.target.observed_unix_ms
            || receipt.observed_unix_ms > now
            || now - receipt.observed_unix_ms > 30_000
        {
            return Err(StoreError::Invalid("invalid staged stop receipt".into()));
        }
        receipt
            .target
            .supervisor
            .as_ref()
            .ok_or(StoreError::Conflict)?
            .validate()
            .map_err(|_| StoreError::Invalid("invalid staged supervisor identity".into()))?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_schema(&tx)?;
        if head(&tx)? != expected_head {
            return Err(StoreError::Conflict);
        }
        let mut attempt = read_attempts(&tx)?
            .into_iter()
            .find(|a| a.id == receipt.target.attempt)
            .ok_or(StoreError::Conflict)?;
        if attempt.revision != expected_revision
            || attempt.termination_observed
            || attempt.state != AttemptState::Reserved
        {
            return Err(StoreError::Conflict);
        }
        let record = super::reservations::read_inputs(&tx)?
            .into_iter()
            .find(|i| i.attempt == attempt.id && i.operation == receipt.target.operation)
            .ok_or(StoreError::Conflict)?;
        require_workspace_quiescence(&tx,&record.operation,receipt.target.supervisor.as_ref().ok_or(StoreError::Conflict)?,receipt.host_reboot.as_ref())?;
        validate_preservation(&record,&receipt.repository_snapshots)?;
        validate_outputs(&record,receipt.output_snapshot.as_ref())?;
        let binding = super::runtime::read_all(&tx)?
            .into_iter()
            .find(|b| b.id == record.inputs.binding)
            .ok_or(StoreError::Conflict)?;
        if binding.revision != record.inputs.binding_revision
            || super::ownership::identity_digest(&binding)? != record.inputs.binding_digest
            || !attempt.retains_capacity()
        {
            return Err(StoreError::Conflict);
        }
        let target: String = tx.query_row(
            "SELECT payload FROM events WHERE kind='runtime.launch_target' AND entity=?1",
            [record.operation.as_str()],
            |r| r.get(0),
        )?;
        if serde_json::from_str::<LaunchTarget>(&target)
            .map_err(|_| StoreError::Corrupt("invalid staged target".into()))?
            != receipt.target
        {
            return Err(StoreError::Conflict);
        }
        let started: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM events WHERE kind='runtime.launch_started' AND entity=?1)",
            [record.operation.as_str()],
            |r| r.get(0),
        )?;
        if started
            || super::ownership::read_all(&tx)?
                .iter()
                .any(|o| o.binding == record.inputs.binding)
        {
            return Err(StoreError::Conflict);
        }
        let mut task = read_tasks(&tx)?
            .into_iter()
            .find(|t| t.id == attempt.task)
            .ok_or(StoreError::Conflict)?;
        if task.active_attempt.as_ref() != Some(&attempt.id) {
            return Err(StoreError::Conflict);
        }
        let delivery = super::delivery::delivery(&tx, &record.operation)?;
        if delivery.attempts != 1 || delivery.state == DeliveryState::Confirmed {
            return Err(StoreError::Conflict);
        }
        let cancelled = super::reservations::read_cancellations(&tx)?
            .iter()
            .any(|c| c.attempt == attempt.id);
        if delivery.state != DeliveryState::PermanentFailure {
            super::delivery::update_outcome(&tx,&delivery,&Outcome::PermanentFailure{diagnostic:"staged worker termination observed; selected resources retained, prior start outcome not asserted absent".into()},now,"staged-worker-stop")?;
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
                "runtime.launch_resources_retained",
                record.operation.as_str(),
                attempt.revision,
                serde_json::to_string(&receipt.target),
            ),
            (
                "attempt.changed",
                attempt.id.as_str(),
                attempt.revision,
                serde_json::to_string(&attempt),
            ),
            (
                "task.changed",
                task.id.as_str(),
                task.revision,
                serde_json::to_string(&task),
            ),
            (
                "runtime.launch_stopped",
                record.operation.as_str(),
                attempt.revision,
                serde_json::to_string(receipt),
            ),
        ] {
            tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES(?1,?2,?3,1,?4)",params![kind,entity,integer(revision)?,payload.map_err(|_|StoreError::Invalid("staged stop encoding failed".into()))?])?;
        }
        tx.commit()?;
        Ok(attempt)
    }
}

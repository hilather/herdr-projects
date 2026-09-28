use super::*;
use crate::operations::{DeliveryState, Outcome};

pub(crate) struct TerminationSelection {
    pub head:u64,
    pub attempt:Attempt,
    pub worker:Option<TerminationWorker>,
}
pub(crate) struct TerminationWorker {
    pub record:AttemptInputRecord,
    pub task:Task,
    pub binding:RuntimeBinding,
    pub owner:Option<RuntimeOwnership>,
    pub delivery:crate::operations::Delivery,
    pub events:Vec<Event>,
    pub cancelled:bool,
    /// An operator completion request: stop the worker without failing its task.
    pub completion:bool,
}
impl SqliteStore {
    pub(super) fn termination_selection(&mut self,id:&AttemptId,expected:u64,budget:&read_budget::ReadBudget)->Result<TerminationSelection> {
        budget.check()?;
        let tx=self.connection.transaction()?;
        let head=head(&tx)?;
        let attempt=read_attempt_with_budget(&tx,id,Some(budget))?;
        if attempt.revision!=expected {return Err(StoreError::Conflict);}
        if attempt.termination_observed {return Ok(TerminationSelection{head,attempt,worker:None});}
        let record=super::reservations::read_attempt_input(&tx,id.as_str(),Some(budget))?;
        let task=read_task_with_budget(&tx,attempt.task.as_str(),Some(budget))?;
        let binding=super::runtime::read_binding(&tx,&record.inputs.binding,Some(budget))?.ok_or(StoreError::Conflict)?;
        let owner=super::ownership::read_binding(&tx,&binding.id,Some(budget))?;
        let delivery=super::delivery::delivery_with_budget(&tx,&record.operation,Some(budget))?;
        super::approvals::validate_historical_consumption(&tx,&record,Some(budget))?;
        let cancelled=tx.query_row("SELECT EXISTS(SELECT 1 FROM attempt_cancellations WHERE attempt_id=?1)",[id.as_str()],|row|row.get(0))?;
        let completion=completion_requested(&tx,id)?;
        let mut query=tx.prepare("SELECT sequence,kind,entity,revision,payload_version,payload FROM events WHERE entity=?1 AND (kind GLOB 'runtime.launch_*' OR kind IN ('runtime.worktrees_creation','runtime.worktrees_ready')) ORDER BY sequence")?;
        let mut rows=query.query([record.operation.as_str()])?;
        let mut events=Vec::new();
        while let Some(row)=rows.next()? {
            budget.row(row,&[(5,1)])?;
            let payload:String=row.get(5)?;
            events.push(Event{sequence:row.get(0)?,kind:row.get(1)?,entity:row.get(2)?,revision:row.get(3)?,payload_version:row.get(4)?,payload:serde_json::from_str(&payload).map_err(|e|StoreError::Corrupt(e.to_string()))?});
        }
        budget.check()?;
        Ok(TerminationSelection{head,attempt,worker:Some(TerminationWorker{record,task,binding,owner,delivery,events,cancelled,completion})})
    }
}

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
fn require_workspace_quiescence(db:&Connection,operation:&OperationId,supervisor:&crate::worker_supervision::SupervisorIdentity,reboot:Option<&crate::worker_supervision::HostRebootEvidence>,budget:Option<&read_budget::ReadBudget>)->Result<()> {
    if let Some(evidence)=reboot {evidence.validate_for(supervisor).map_err(|_|StoreError::Invalid("invalid workspace reboot evidence".into()))?;}
    let mut query=db.prepare("SELECT kind,payload FROM events WHERE entity=?1 AND kind IN ('runtime.launch_creation','runtime.launch_workspace')")?;
    let mut rows=query.query([operation.as_str()])?;
    let mut kinds=std::collections::BTreeSet::new();
    while let Some(row)=rows.next()? {
        if let Some(budget)=budget {budget.row(row,&[(1,1)])?;}
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
    pub(crate) fn validate_workspace_quiescence_with_budget(&self,operation:&OperationId,supervisor:&crate::worker_supervision::SupervisorIdentity,reboot:Option<&crate::worker_supervision::HostRebootEvidence>,budget:Option<&read_budget::ReadBudget>)->Result<()> {
        require_workspace_quiescence(&self.connection,operation,supervisor,reboot,budget)
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
        self.record_worker_termination_with_budget(prepared,expected_revision,expected_head,now,None)
    }
    pub(crate) fn record_worker_termination_with_budget(&mut self,prepared:&PreparedWorkerTermination,expected_revision:u64,expected_head:u64,now:i64,budget:Option<&read_budget::ReadBudget>)->Result<Attempt> {
        if let Some(budget)=budget {budget.check()?;}
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
        let mut attempt = read_attempt_with_budget(&tx,&receipt.attempt,budget)?;
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
        let record = super::reservations::read_input(&tx,&receipt.launch,budget)?;
        if record.attempt!=receipt.attempt {return Err(StoreError::Conflict);}
        require_workspace_quiescence(&tx,&record.operation,&receipt.supervisor,receipt.host_reboot.as_ref(),budget)?;
        validate_preservation(&record,&receipt.repository_snapshots)?;
        validate_outputs(&record,receipt.output_snapshot.as_ref())?;
        let binding = super::runtime::read_binding(&tx,&receipt.binding,budget)?.ok_or(StoreError::Conflict)?;
        let owner = super::ownership::read_binding(&tx,&binding.id,budget)?.ok_or(StoreError::Conflict)?;
        let mut task = read_task_with_budget(&tx,attempt.task.as_str(),budget)?;
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
        let payload_start: String = read_budget::one(&tx,
            "SELECT payload FROM events WHERE kind='runtime.launch_started' AND entity=?1",
            [record.operation.as_str()],
            budget,&[(0,1)],|r| r.get(0),
        )?;
        let start: LaunchStartedReceipt = serde_json::from_str(&payload_start)
            .map_err(|_| StoreError::Corrupt("invalid worker launch receipt".into()))?;
        let launch = super::delivery::delivery_with_budget(&tx, &record.operation,budget)?;
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
        let cancelled:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM attempt_cancellations WHERE attempt_id=?1)",[attempt.id.as_str()],|row|row.get(0))?;
        // Cancellation overrides a completion request; either names the cause.
        let completion=!cancelled&&completion_requested(&tx,&attempt.id)?;
        let cause=if cancelled {WorkerTerminationCause::Cancellation} else if completion {WorkerTerminationCause::Completion} else {WorkerTerminationCause::ProcessExit};
        if receipt.cause != cause {
            return Err(StoreError::Conflict);
        }
        // A completed task must also clear its memory obligations; otherwise it
        // stays blocked (not failed) for the operator, as a process exit does.
        let succeeded=completion&&super::memory_barrier::report_with_budget(&tx,task.id.as_str(),now,budget)?.blockers.is_empty();
        // Retire outstanding brief obligations without asserting whether an
        // uncertain submission happened. Every old claim is fenced atomically.
        let mut query=tx.prepare("SELECT id FROM operations WHERE kind='runtime.worker_brief' AND json_extract(payload,'$.attempt')=?1 ORDER BY id")?;
        let mut rows=query.query([attempt.id.as_str()])?;
        let mut ids=Vec::new();
        while let Some(row)=rows.next()? {
            if let Some(budget)=budget {budget.row(row,&[])?;}
            ids.push(OperationId::new(row.get::<_,String>(0)?).map_err(StoreError::Corrupt)?);
        }
        drop(rows);drop(query);
        for id in ids {
            let operation=read_operation_with_budget(&tx,&id,budget)?;
            let delivery = super::delivery::delivery_with_budget(&tx, &operation.id,budget)?;
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
        } else if completion {
            AttemptState::Completed
        } else {
            AttemptState::Failed
        };
        task.revision = task.revision.checked_add(1).ok_or(StoreError::Conflict)?;
        task.active_attempt = None;
        task.state = if cancelled {
            TaskState::Cancelled
        } else if succeeded {
            TaskState::Succeeded
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
        super::dispatch_log::mark(&tx, &attempt, now, "record_worker_termination_with_budget")?;
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
        super::consumer_bindings::reconcile_task(&tx,attempt.task.as_str(),budget)?;
        if let Some(budget)=budget {budget.check()?;}
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
        self.record_launch_stopped_with_budget(prepared,expected_revision,expected_head,now,None)
    }
    pub(crate) fn record_launch_stopped_with_budget(&mut self,prepared:&PreparedLaunchStopped,expected_revision:u64,expected_head:u64,now:i64,budget:Option<&read_budget::ReadBudget>)->Result<Attempt> {
        if let Some(budget)=budget {budget.check()?;}
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
        let mut attempt = read_attempt_with_budget(&tx,&receipt.target.attempt,budget)?;
        if attempt.revision != expected_revision
            || attempt.termination_observed
            || attempt.state != AttemptState::Reserved
        {
            return Err(StoreError::Conflict);
        }
        let record = super::reservations::read_input(&tx,&receipt.target.operation,budget)?;
        if record.attempt!=attempt.id {return Err(StoreError::Conflict);}
        require_workspace_quiescence(&tx,&record.operation,receipt.target.supervisor.as_ref().ok_or(StoreError::Conflict)?,receipt.host_reboot.as_ref(),budget)?;
        validate_preservation(&record,&receipt.repository_snapshots)?;
        validate_outputs(&record,receipt.output_snapshot.as_ref())?;
        let binding = super::runtime::read_binding(&tx,&record.inputs.binding,budget)?.ok_or(StoreError::Conflict)?;
        if binding.revision != record.inputs.binding_revision
            || super::ownership::identity_digest(&binding)? != record.inputs.binding_digest
            || !attempt.retains_capacity()
        {
            return Err(StoreError::Conflict);
        }
        let target: String = read_budget::one(&tx,
            "SELECT payload FROM events WHERE kind='runtime.launch_target' AND entity=?1",
            [record.operation.as_str()],
            budget,&[(0,1)],|r| r.get(0),
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
            || super::ownership::read_binding(&tx,&record.inputs.binding,budget)?.is_some()
        {
            return Err(StoreError::Conflict);
        }
        let mut task = read_task_with_budget(&tx,attempt.task.as_str(),budget)?;
        if task.active_attempt.as_ref() != Some(&attempt.id) {
            return Err(StoreError::Conflict);
        }
        let delivery = super::delivery::delivery_with_budget(&tx, &record.operation,budget)?;
        if delivery.attempts != 1 || delivery.state == DeliveryState::Confirmed {
            return Err(StoreError::Conflict);
        }
        let cancelled:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM attempt_cancellations WHERE attempt_id=?1)",[attempt.id.as_str()],|row|row.get(0))?;
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
        super::dispatch_log::mark(&tx, &attempt, now, "record_launch_stopped_with_budget")?;
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
        super::consumer_bindings::reconcile_task(&tx,attempt.task.as_str(),budget)?;
        if let Some(budget)=budget {budget.check()?;}
        tx.commit()?;
        Ok(attempt)
    }
}

fn completion_requested(db:&Connection,attempt:&AttemptId)->Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM events WHERE kind='attempt.completion_requested' AND entity=?1)",[attempt.as_str()],|row|row.get(0))?)
}

/// Accepted evidence: a submission of this attempt under the task's installed
/// contract whose every acceptance policy has an accepted run, a verified
/// result and a current (version 2) contract check.
const ACCEPTED_SUBMISSION: &str = "SELECT s.submission_id FROM result_submissions s
    JOIN task_contracts c ON c.task_id=s.task_id AND c.contract_revision=s.contract_revision AND c.raw_digest=s.contract_digest
    WHERE s.task_id=?1 AND s.attempt_id=?2
      AND EXISTS(SELECT 1 FROM acceptance_policies p WHERE p.task_id=s.task_id AND p.contract_revision=s.contract_revision)
      AND NOT EXISTS(SELECT 1 FROM acceptance_policies p WHERE p.task_id=s.task_id AND p.contract_revision=s.contract_revision
          AND NOT EXISTS(SELECT 1 FROM verification_runs v JOIN verified_results r ON r.run_id=v.run_id
              JOIN verification_contract_checks k ON k.result_id=r.result_id AND k.version=2
              WHERE v.submission_id=s.submission_id AND v.policy_id=p.policy_id AND v.state='accepted'))
    ORDER BY s.created_unix_ms,s.submission_id LIMIT 1";

impl SqliteStore {
    /// Ask the controller to stop a task's started worker because its result
    /// was accepted. Only the attempt's state is requested here; the existing
    /// termination path stops the worker, proves its exit and only then
    /// releases capacity, marking the attempt completed and the task
    /// succeeded. Dependents keep relying on verification or integration
    /// evidence, never on this state. A repeat returns the recorded request.
    pub fn request_completion(&mut self,task_id:&TaskId,expected_revision:u64,now:i64)->Result<CompletionChange> {
        super::delivery::now_check(now)?;
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_schema(&tx)?;
        let version:u32=tx.query_row("PRAGMA user_version",[],|row|row.get(0))?;
        if version<43 {return Err(StoreError::UnsupportedSchema(version));}
        let task=read_task(&tx,task_id.as_str())?;
        let requested:Option<String>=tx.query_row("SELECT e.entity FROM events e JOIN attempts a ON a.id=e.entity
            WHERE e.kind='attempt.completion_requested' AND a.task_id=?1 ORDER BY e.sequence DESC LIMIT 1",[task_id.as_str()],|row|row.get(0)).optional()?;
        if let Some(requested)=requested {
            let attempt=read_attempt(&tx,&AttemptId::new(requested).map_err(StoreError::Corrupt)?)?;
            if task.active_attempt.as_ref().is_none_or(|active|*active==attempt.id) {
                return Ok(CompletionChange{head:head(&tx)?,task:task.id,attempt_revision:attempt.revision,terminated:attempt.termination_observed,attempt:attempt.id,replayed:true});
            }
        }
        if task.revision!=expected_revision {return Err(StoreError::Conflict);}
        let active=task.active_attempt.clone().ok_or_else(||StoreError::Invalid("task has no active attempt to complete".into()))?;
        let mut attempt=read_attempt(&tx,&active)?;
        if !attempt.retains_capacity() {return Err(StoreError::Invalid("attempt already has termination evidence".into()));}
        if tx.query_row("SELECT EXISTS(SELECT 1 FROM attempt_cancellations WHERE attempt_id=?1)",[active.as_str()],|row|row.get::<_,bool>(0))? {
            return Err(StoreError::Invalid("attempt has a cancellation request".into()));
        }
        let started:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM attempt_inputs i JOIN events e ON e.entity=i.operation_id AND e.kind='runtime.launch_started' WHERE i.attempt_id=?1)",[active.as_str()],|row|row.get(0))?;
        if !started {return Err(StoreError::Invalid("worker has not started; cancel the attempt instead".into()));}
        let submission:Option<String>=tx.query_row(ACCEPTED_SUBMISSION,params![task_id.as_str(),active.as_str()],|row|row.get(0)).optional()?;
        let submission=submission.ok_or_else(||StoreError::Invalid("completion requires a submission of this attempt with accepted verification for every acceptance policy".into()))?;
        attempt.revision=attempt.revision.checked_add(1).ok_or(StoreError::Conflict)?;
        tx.execute("UPDATE attempts SET revision=?2 WHERE id=?1",params![active.as_str(),integer(attempt.revision)?])?;
        let payload=serde_json::json!({"version":1,"task":task_id,"attempt":active,"submission":submission,"requested_unix_ms":now}).to_string();
        tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('attempt.completion_requested',?1,?2,1,?3)",params![active.as_str(),integer(attempt.revision)?,payload])?;
        let change=CompletionChange{head:head(&tx)?,task:task.id,attempt:active,attempt_revision:attempt.revision,replayed:false,terminated:false};
        tx.commit()?;
        Ok(change)
    }
}

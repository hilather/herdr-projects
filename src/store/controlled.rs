//! Explicit SQL execution controls with snapshot input/JSON-structure accounting.
//! This is not a peak-heap bound: decoding and SQLite still allocate independently.
use super::*;
mod work;
pub use work::SqlWorkMetrics;
pub(crate) use work::SqlWork;
use std::{os::unix::fs::MetadataExt,sync::{Arc,atomic::{AtomicU8,Ordering}},time::Instant};
use crate::runner::Cancellation;

#[derive(Clone)]
pub struct ReadControl {deadline:Instant,cancellation:Cancellation,row_bytes:i32}
impl ReadControl {
    pub fn new(deadline:Instant,cancellation:Cancellation)->Self {Self{deadline,cancellation,row_bytes:32*1024*1024}}
    pub fn deadline(&self)->Instant {self.deadline}
    pub fn cancellation(&self)->Cancellation {self.cancellation.clone()}
    pub fn check(&self)->Result<()> {match self.reason(){1=>Err(StoreError::Cancelled),2=>Err(StoreError::Deadline),_=>Ok(())}}
    fn reason(&self)->u8 {if self.cancellation.is_cancelled(){1}else if Instant::now()>=self.deadline{2}else{0}}
    pub fn with_row_limit(mut self,bytes:usize)->Result<Self> {
        if !(1024*1024..=32*1024*1024).contains(&bytes){return Err(StoreError::Invalid("encoded row limit must be 1–32 MiB".into()));}
        self.row_bytes=bytes as i32;Ok(self)
    }
}

/// Only explicitly controlled entry points are exposed; no raw-connection or
/// Deref escape permits callers to replace hooks or renew an expired deadline.
pub struct ControlledStore {store:SqliteStore,control:ReadControl,interrupted:Arc<AtomicU8>,work_budget:read_budget::ReadBudget,sql_work:Option<SqlWork>}
impl ControlledStore {
    pub(crate) fn queue_report(&mut self,now:i64)->Result<QueueReport> {
        self.control.check()?;
        self.store.queue_report_with_budget(now,&self.work_budget).map_err(|e|self.error(e))
    }
    pub(crate) fn queue_task(&mut self,id:&TaskId,revision:u64,head:u64,request:&QueueRequest,now:i64)->Result<u64> {
        self.control.check()?;
        self.store.queue_task_with_budget(id,revision,head,request,now,&self.work_budget).map_err(|e|self.error(e))
    }
    pub(crate) fn set_scheduler_policy(&mut self,head:u64,revision:u64,max_workers:u32,max_attempts:u32)->Result<u64> {
        self.control.check()?;
        self.store.set_scheduler_policy_with_budget(head,revision,max_workers,max_attempts,Some(&self.work_budget)).map_err(|e|self.error(e))
    }
    pub(crate) fn launch_start_selection(&mut self, operation: &OperationId, expected: u64) -> Result<super::launch::StartSelection> {
        self.control.check()?;
        self.store.start_selection(operation, expected, &self.work_budget).map_err(|e| self.error(e))
    }
    pub(crate) fn block_unnamed_start(&mut self, operation: &OperationId, expected: u64, diagnostic: &str, now: i64) -> Result<crate::operations::Delivery> {
        self.control.check()?;
        self.store.block_unnamed_start(operation, expected, diagnostic, now, &self.work_budget).map_err(|e| self.error(e))
    }
    pub(crate) fn record_launch_name(&mut self, claim: &crate::operations::Claim, prepared: &PreparedLaunchName, now: i64) -> Result<u64> {
        self.control.check()?;
        self.store.record_launch_name_with_budget(claim, prepared, now, Some(&self.work_budget)).map_err(|e| self.error(e))
    }
    pub(crate) fn observe_launch_started(&mut self, prepared: &PreparedLaunchStarted, revision: u64, head: u64, now: i64) -> Result<crate::operations::Delivery> {
        self.control.check()?;
        self.store.observe_launch_started_with_budget(prepared, revision, head, now, Some(&self.work_budget)).map_err(|e| self.error(e))
    }
    pub(crate) fn launch_advancement_selection(&mut self, operation: &OperationId, expected: u64) -> Result<super::launch::AdvancementSelection> {
        self.control.check()?;
        self.store.advancement_selection(operation, expected, &self.work_budget).map_err(|e| self.error(e))
    }
    pub(crate) fn worktree_preparation_selection(&mut self, operation: &OperationId, expected: u64) -> Result<super::worktrees::PreparationSelection> {
        self.control.check()?;
        self.store.preparation_selection(operation, expected, &self.work_budget).map_err(|e| self.error(e))
    }
    pub(crate) fn claim_worktree_creation(&mut self, expected: u64, prepared: &PreparedWorktreeCreation, now: i64, lease_ms: i64) -> Result<crate::operations::Claim> {
        self.control.check()?;
        self.store.claim_with_creation_budget(&prepared.intent.operation, expected, "canonical-worktree-adapter", now, lease_ms,
            Some(super::delivery::LaunchPreparation::Worktrees(prepared)), Some(&self.work_budget)).map_err(|e| self.error(e))
    }
    pub(crate) fn retain_observed_worktrees(&mut self, prepared: &PreparedWorktreeReceipts, now: i64) -> Result<()> {
        self.control.check()?;
        let selected = (|| {
            let tx = self.store.connection.transaction()?;
            let head = head(&tx)?;
            let delivery = super::delivery::delivery_with_budget(&tx, &prepared.intent.operation, Some(&self.work_budget))?;
            Ok::<_, StoreError>((head, delivery.revision))
        })().map_err(|e| self.error(e))?;
        self.store.observe_worktrees_with_budget(prepared, selected.1, selected.0, now, Some(&self.work_budget)).map_err(|e| self.error(e))
    }
    pub(crate) fn launch_preparation_input(&mut self, operation: &OperationId, expected: u64) -> Result<(AttemptInputRecord, crate::operations::Delivery)> {
        self.control.check()?;
        let result = (|| {
            let tx = self.store.connection.transaction()?;
            let delivery = super::delivery::delivery_with_budget(&tx, operation, Some(&self.work_budget))?;
            if delivery.revision != expected { return Err(StoreError::Conflict); }
            let record = super::reservations::read_input(&tx, operation, Some(&self.work_budget))?;
            self.work_budget.check()?;
            Ok((record, delivery))
        })();
        result.map_err(|e| self.error(e))
    }
    pub(crate) fn resource_creation_selection(&mut self, operation: &OperationId, expected: u64) -> Result<super::launch::ResourceRecoverySelection> {
        self.control.check()?;
        self.store.resource_creation_selection(operation, expected, &self.work_budget).map_err(|e| self.error(e))
    }
    pub(crate) fn claim_launch_creation(&mut self, expected: u64, prepared: &PreparedLaunchCreation, now: i64, lease_ms: i64) -> Result<crate::operations::Claim> {
        self.control.check()?;
        self.store.claim_with_creation_budget(&prepared.intent.operation, expected, "canonical-resource-adapter", now, lease_ms,
            Some(super::delivery::LaunchPreparation::Native(prepared)), Some(&self.work_budget)).map_err(|e| self.error(e))
    }
    pub(crate) fn continue_worktree_launch_creation(&mut self, claim: &crate::operations::Claim, prepared: &PreparedLaunchCreation, now: i64) -> Result<()> {
        self.control.check()?;
        self.store.continue_worktree_launch_creation_with_budget(claim, prepared, now, Some(&self.work_budget)).map_err(|e| self.error(e))
    }
    pub(crate) fn record_launch_workspace(&mut self, claim: &crate::operations::Claim, prepared: &PreparedLaunchWorkspace, now: i64) -> Result<u64> {
        self.control.check()?;
        self.store.record_launch_workspace_with_budget(claim, prepared, now, Some(&self.work_budget)).map_err(|e| self.error(e))
    }
    pub(crate) fn record_launch_layout(&mut self, claim: &crate::operations::Claim, prepared: &PreparedLaunchLayout, now: i64) -> Result<u64> {
        self.control.check()?;
        self.store.record_launch_layout_with_budget(claim, prepared, now, Some(&self.work_budget)).map_err(|e| self.error(e))
    }
    pub(crate) fn validate_launch_claim(&mut self, claim: &crate::operations::Claim, now: i64) -> Result<()> {
        self.control.check()?;
        self.store.validate_claim_with_budget(claim, now, Some(&self.work_budget)).map_err(|e| self.error(e))
    }
    pub(crate) fn record_launch_release(&mut self, claim: &crate::operations::Claim, prepared: &PreparedLaunchRelease, now: i64) -> Result<u64> {
        self.control.check()?;
        self.store.record_launch_release_with_budget(claim, prepared, now, Some(&self.work_budget)).map_err(|e| self.error(e))
    }
    pub(crate) fn resource_recovery_selection(&mut self, operation: &OperationId, expected: u64) -> Result<super::launch::ResourceRecoverySelection> {
        self.control.check()?;
        self.store.resource_recovery_selection(operation, expected, &self.work_budget).map_err(|e| self.error(e))
    }
    pub(crate) fn observe_launch_target(&mut self, prepared: &PreparedLaunchTarget, revision: u64, head: u64, now: i64) -> Result<u64> {
        self.control.check()?;
        self.store.observe_launch_target_with_budget(prepared, revision, head, now, Some(&self.work_budget)).map_err(|e| self.error(e))
    }
    pub(crate) fn observe_launch_workspace(&mut self, prepared: &PreparedLaunchWorkspace, revision: u64, head: u64, now: i64) -> Result<u64> {
        self.control.check()?;
        self.store.observe_launch_workspace_with_budget(prepared, revision, head, now, Some(&self.work_budget)).map_err(|e| self.error(e))
    }
    pub(crate) fn retain_observed_launch_target(&mut self, target: &LaunchTarget, now: i64) -> Result<u64> {
        self.control.check()?;
        let selected = (|| {
            let tx = self.store.connection.transaction()?;
            let head = head(&tx)?;
            let delivery = super::delivery::delivery_with_budget(&tx, &target.operation, Some(&self.work_budget))?;
            tx.commit()?;
            Ok::<_, StoreError>((head, delivery.revision))
        })().map_err(|e| self.error(e))?;
        self.store.observe_launch_target_with_budget(&PreparedLaunchTarget { target: target.clone() },
            selected.1, selected.0, now, Some(&self.work_budget)).map_err(|e| self.error(e))
    }
    pub(crate) fn termination_selection(&mut self,id:&AttemptId,expected:u64)->Result<super::worker_termination::TerminationSelection> {
        self.control.check()?;
        self.store.termination_selection(id,expected,&self.work_budget).map_err(|e|self.error(e))
    }
    pub(crate) fn validate_workspace_quiescence(&self,operation:&OperationId,supervisor:&crate::worker_supervision::SupervisorIdentity,reboot:Option<&crate::worker_supervision::HostRebootEvidence>)->Result<()> {
        self.control.check()?;
        self.store.validate_workspace_quiescence_with_budget(operation,supervisor,reboot,Some(&self.work_budget)).map_err(|e|self.error(e))
    }
    pub(crate) fn record_worker_termination(&mut self,prepared:&PreparedWorkerTermination,revision:u64,head:u64,now:i64)->Result<Attempt> {
        self.control.check()?;
        self.store.record_worker_termination_with_budget(prepared,revision,head,now,Some(&self.work_budget)).map_err(|e|self.error(e))
    }
    pub(crate) fn record_launch_stopped(&mut self,prepared:&PreparedLaunchStopped,revision:u64,head:u64,now:i64)->Result<Attempt> {
        self.control.check()?;
        self.store.record_launch_stopped_with_budget(prepared,revision,head,now,Some(&self.work_budget)).map_err(|e|self.error(e))
    }
    pub(crate) fn stop_worktree_preparation(&mut self,attempt:&AttemptId,revision:u64,head:u64,guard:&crate::execution_guard::RootGuard,preserve:impl FnOnce(&AttemptInputRecord,&WorktreeCreation)->Result<(Vec<PreparationSnapshotReference>,AttemptOutputReference)>,now:i64)->Result<Option<Attempt>> {
        self.control.check()?;
        self.store.stop_worktree_preparation_with_budget(attempt,revision,head,guard,preserve,now,Some(&self.work_budget)).map_err(|e|self.error(e))
    }
    pub(crate) fn worker_brief_selection(&self,id:&OperationId,expected:u64)->Result<(WorkerBriefIntent,AttemptInputRecord,LaunchStartedReceipt)> {
        self.control.check()?;
        let db=&self.store.connection;
        let budget=Some(&self.work_budget);
        let selected=(|| {
            let delivery=super::delivery::delivery_with_budget(db,id,budget)?;
            if delivery.revision!=expected || delivery.state!=crate::operations::DeliveryState::Pending || delivery.attempts!=0 || delivery.epoch!=0 {return Err(StoreError::Invalid("brief delivery is stale or was already claimed".into()));}
            super::worker_brief::check_with_budget(db,id,jiff::Timestamp::now().as_millisecond(),budget)?;
            let operation=read_operation_with_budget(db,id,budget)?;
            let intent:WorkerBriefIntent=serde_json::from_value(operation.payload).map_err(|e|StoreError::Corrupt(e.to_string()))?;
            let (record,_,_,_)=super::worker_brief::context_with_budget(db,&intent,budget)?;
            let launch=super::delivery::delivery_with_budget(db,&record.operation,budget)?;
            let Some(crate::operations::Outcome::Confirmed{observed_identity})=launch.last_outcome else {return Err(StoreError::Conflict);};
            let start=serde_json::from_str(&observed_identity).map_err(|e|StoreError::Corrupt(e.to_string()))?;
            Ok((intent,record,start))
        })();
        selected.map_err(|e|self.error(e))
    }
    /// See [`SqliteStore::replay_source_repository`].
    pub(crate) fn replay_source_repository(&mut self,task:&str)->Result<Option<String>> {
        self.control.check()?;
        self.store.replay_source_repository(task).map_err(|e|self.error(e))
    }
    pub(crate) fn render_attempt_brief(&mut self,project:&Path,attempt:&str)->anyhow::Result<crate::memory::WorkerBrief> {
        self.control.check()?;
        anyhow::ensure!(project.join(".state/state.db").canonicalize()?.to_str()==self.store.connection.path(),"knowledge store belongs to another project");
        let result=crate::memory::render_attempt_knowledge_budgeted(project,attempt,&mut self.store,Some(&self.work_budget));
        let knowledge=self.admission_result(result)?;
        self.control.check()?;
        let brief=crate::memory::compose_knowledge(knowledge)?;
        self.control.check()?;
        Ok(brief)
    }
    pub(crate) fn prepare_worker_brief(&mut self,project:&Path,attempt:&str,expected_head:u64)->anyhow::Result<Operation> {
        self.control.check()?;
        anyhow::ensure!(self.current_head()?==expected_head,"worker brief preparation head changed");
        let brief=self.render_attempt_brief(project,attempt)?;
        let record=self.store.sealed_attempt_input(attempt,Some(&self.work_budget))?;
        let binding=super::runtime::read_binding(&self.store.connection,&record.inputs.binding,Some(&self.work_budget))?.ok_or(StoreError::Conflict)?;
        let ownership=super::ownership::read_binding(&self.store.connection,&binding.id,Some(&self.work_budget))?.ok_or(StoreError::Conflict)?;
        anyhow::ensure!(record.inputs.memory.as_ref().map(|r|r.id.as_str())==Some(brief.snapshot_id.as_str()),"rendered brief knowledge changed");
        let prepared=PreparedWorkerBrief{intent:WorkerBriefIntent{version:1,attempt:record.attempt,launch:record.operation,binding:binding.id,binding_revision:binding.revision,ownership_revision:ownership.revision,knowledge:record.inputs.memory,prompt_digest:brief.prompt_digest,prompt_chars:brief.prompt_chars}};
        self.store.enqueue_worker_brief_with_budget(&prepared,expected_head,jiff::Timestamp::now().as_millisecond(),Some(&self.work_budget)).map_err(|e|self.error(e).into())
    }
    pub(crate) fn prepare_supervised_worker_brief(&mut self,project:&Path,attempt:&AttemptId,expected_revision:u64)->anyhow::Result<Operation> {
        self.control.check()?;
        let head=self.current_head()?;
        let current=read_attempt_with_budget(&self.store.connection,attempt,Some(&self.work_budget))?;
        anyhow::ensure!(current.revision==expected_revision && current.state==AttemptState::Launching && current.retains_capacity(),"worker attempt changed before brief preparation");
        let record=self.store.sealed_attempt_input(attempt.as_str(),Some(&self.work_budget))?;
        let delivery=super::delivery::delivery_with_budget(&self.store.connection,&record.operation,Some(&self.work_budget))?;
        anyhow::ensure!(delivery.state==crate::operations::DeliveryState::Confirmed,"worker start is unconfirmed");
        let Some(crate::operations::Outcome::Confirmed{observed_identity})=delivery.last_outcome else {return Err(StoreError::Conflict.into());};
        anyhow::ensure!(super::launch::has_started_receipt(&self.store.connection,&record.operation,&observed_identity)?,"worker start receipt missing");
        let start:LaunchStartedReceipt=serde_json::from_str(&observed_identity)?;
        anyhow::ensure!(start.version==2 && start.attempt==*attempt && start.operation==record.operation && start.supervisor.is_some(),"worker lacks supervised start evidence");
        self.prepare_worker_brief(project,attempt.as_str(),head)
    }
    pub(crate) fn record_worker_brief(&mut self,claim:&crate::operations::Claim,receipt:&PreparedWorkerBriefReceipt,now:i64)->Result<crate::operations::Delivery> {
        self.control.check()?;
        self.store.record_worker_brief_with_budget(claim,receipt,now,Some(&self.work_budget)).map_err(|e|self.error(e))
    }
    pub(crate) fn claim_worker_brief(&mut self,id:&OperationId,expected:u64,now:i64)->Result<crate::operations::Claim> {
        self.control.check()?;
        self.store.claim_with_creation_budget(id,expected,"canonical-brief-adapter",now,30_000,None,Some(&self.work_budget)).map_err(|e|self.error(e))
    }
    /// The prompt crossed the boundary but the agent never showed it accepted it:
    /// retain the claim's operation as ambiguous (never as confirmed), so only
    /// explicit observation or operator retirement can resolve it.
    pub(crate) fn record_worker_brief_unaccepted(&mut self,claim:&crate::operations::Claim,evidence:String,now:i64)->Result<crate::operations::Delivery> {
        self.control.check()?;
        self.store.finish_operation(claim,crate::operations::Outcome::Ambiguous{observation_required:evidence},now).map_err(|e|self.error(e))
    }
    pub(crate) fn validate_worker_brief_claim(&mut self,claim:&crate::operations::Claim,now:i64)->Result<()> {
        self.control.check()?;
        self.store.validate_claim_with_budget(claim,now,Some(&self.work_budget)).map_err(|e|self.error(e))
    }
    pub(crate) fn service_barrier_stops(&mut self, now: i64)->Result<super::barriers::BarrierStopTurn> {
        self.control.check()?;
        self.store.service_barrier_stops(now, &self.work_budget).map_err(|e|self.error(e))
    }
    pub(crate) fn frozen_barrier(&mut self,id:&str)->Result<Option<super::barriers::FrozenBarrier>> {
        self.store.frozen_barrier_with_budget(id,Some(&self.work_budget)).map_err(|e|self.error(e))
    }
    pub(crate) fn freeze_barrier(&mut self,members:&[super::barriers::BarrierMember],head:u64)->Result<super::barriers::FrozenBarrier> {
        self.store.freeze_barrier_with_budget(members,head,Some(&self.work_budget)).map_err(|e|self.error(e))
    }
    pub(crate) fn freeze_barrier_json(&mut self,raw:&[u8],head:u64)->Result<super::barriers::FrozenBarrier> {
        self.work_budget.input_json(raw,1)?;
        let members:Vec<super::barriers::BarrierMember>=serde_json::from_slice(raw)
            .map_err(|_|StoreError::Invalid("invalid barrier membership JSON (contents withheld)".into()))?;
        self.freeze_barrier(&members,head)
    }
    pub(crate) fn draft_barrier_release(&mut self,id:&str,authority:VersionedReference,config:&str,expires:i64)->Result<crate::domain::BarrierReleaseAuthorization> {
        self.store.draft_barrier_release_with_budget(id,authority,config,expires,Some(&self.work_budget)).map_err(|e|self.error(e))
    }
    pub(crate) fn release_authorized_barrier(&mut self,prepared:&crate::domain::PreparedBarrierRelease)->Result<super::barriers::FrozenBarrier> {
        self.store.release_authorized_barrier_with_budget(prepared,Some(&self.work_budget)).map_err(|e|self.error(e))
    }
    pub(crate) fn verify_barrier_objects(&mut self,project:&Path,id:&str)->anyhow::Result<()> {
        self.control.check()?;
        anyhow::ensure!(project.join(".state/state.db").canonicalize()?.to_str()==self.store.connection.path(),"barrier store belongs to another project");
        let barrier=self.store.frozen_barrier_with_budget(id,Some(&self.work_budget)).map_err(|e|self.error(e))?
            .ok_or_else(||anyhow::anyhow!("barrier missing"))?;
        let mut verified=std::collections::BTreeSet::new();
        let mut total=0usize;
        for member in &barrier.members {
            for object in self.store.memory_consumed_objects_with_budget(&member.task_id,Some(&self.work_budget)).map_err(|e|self.error(e))? {
                if verified.insert(object.as_str().to_owned()) {
                    anyhow::ensure!(verified.len()<=10000,"barrier object inventory exceeds bounds");
                    let bytes=crate::memory::read_object_controlled(&project.join(".state/objects"),&object,(64*1024*1024-total) as u64,Some(&self.work_budget))?;
                    total=total.checked_add(bytes.len()).ok_or_else(||anyhow::anyhow!("barrier evidence byte count overflow"))?;
                    anyhow::ensure!(total<=64*1024*1024,"barrier evidence exceeds 64 MiB read budget");
                }
            }
        }
        self.control.check()?;
        Ok(())
    }
    pub(crate) fn revoke_barrier(&mut self,id:&str,head:u64,reason:&str)->Result<super::barriers::FrozenBarrier> {
        self.work_budget.check()?;
        self.store.revoke_barrier_with_budget(id,head,Some(reason),Some(&self.work_budget)).map_err(|e|self.error(e))
    }
    pub(crate) fn insert_denial(&mut self,denial:&crate::domain::AuthorityDenial)->Result<()> {
        self.control.check()?;
        self.store.insert_denial(denial).map_err(|e|self.error(e))
    }
    pub(crate) fn set_auto_replans(&mut self,head:u64,on:bool)->Result<super::plans::AutoReplanControl> {
        self.work_budget.check()?;
        self.store.set_auto_replans(head,on).map_err(|e|self.error(e))
    }
    pub(crate) fn service_replans(&mut self)->Result<super::plans::ReplanTurn> {
        self.work_budget.check()?;
        self.store.service_replans(&self.work_budget).map_err(|e|self.error(e))
    }
    pub(crate) fn set_result_automation(&mut self,head:u64,verify:Option<bool>,integrate:Option<bool>)->Result<super::verification_jobs::ResultAutomationControl> {
        self.work_budget.check()?;
        self.store.set_result_automation(head,verify,integrate).map_err(|e|self.error(e))
    }
    pub(crate) fn service_integration_jobs(&mut self)->Result<super::integration_jobs::IntegrationJobTurn> {
        self.work_budget.check()?;
        self.store.service_integration_jobs(&self.work_budget).map_err(|e|self.error(e))
    }
    pub(crate) fn service_verification_jobs(&mut self)->Result<super::verification_jobs::VerificationJobTurn> {
        self.work_budget.check()?;
        self.store.service_verification_jobs(&self.work_budget).map_err(|e|self.error(e))
    }
    pub(crate) fn create_planner_session(&mut self,id:&str,intent:&str,evidence:&[u64],parent:u64,cursor:u64)->Result<super::plans::PlannerSession> {
        self.work_budget.check()?;
        self.store.create_planner_session(id,intent,evidence,parent,cursor,&self.work_budget).map_err(|e|self.error(e))
    }
    pub(crate) fn apply_plan_proposal(&mut self,raw:&[u8],parent:u64,key:&str)->Result<super::plans::PlanProposalReceipt> {
        self.work_budget.check()?;
        self.store.apply_plan_proposal_with_budget(raw,parent,key,Some(&self.work_budget)).map_err(|e|self.error(e))
    }
    pub(crate) fn inspect_plan(&mut self,expected:Option<u64>,after:Option<&str>,limit:usize)->Result<super::plans::PlanIntentPage> {
        self.work_budget.check()?;
        self.store.inspect_plan(expected,after,limit,&self.work_budget).map_err(|e|self.error(e))
    }
    pub(crate) fn planner_session(&self,id:&str)->Result<super::plans::PlannerSession> {
        self.work_budget.check()?;
        self.store.planner_session(id,&self.work_budget).map_err(|e|self.error(e))
    }
    pub(crate) fn service_waits(&mut self)->Result<super::plans::WaitTurn> {
        self.control.check()?;
        self.store.service_waits(&self.work_budget).map_err(|e|self.error(e))
    }
    pub(crate) fn register_wait(&mut self,task:&str,attempt:Option<&str>,condition:&str,deadline:Option<i64>,trigger:Option<&crate::domain::WaitTrigger>)->Result<super::plans::WaitRegistration> {
        self.work_budget.check()?;
        self.store.register_wait_with_budget(task,attempt,condition,deadline,trigger,Some(&self.work_budget)).map_err(|e|self.error(e))
    }
    pub(crate) fn rearm_wait(&mut self,id:&str,deadline:Option<i64>)->Result<super::plans::WaitRegistration> {
        self.work_budget.check()?;
        self.store.rearm_wait_with_budget(id,deadline,Some(&self.work_budget)).map_err(|e|self.error(e))
    }
    pub(crate) fn replay_wait(&mut self,id:&str)->Result<super::plans::WaitReplay> {
        self.control.check()?;
        self.store.replay_wait_with_budget(id,Some(&self.work_budget)).map_err(|e|self.error(e))
    }
    pub(crate) fn attempt_contract(&self, task: &str, frozen: Option<&crate::domain::VersionedReference>) -> Result<Option<crate::domain::PreparedContract>> {
        self.read(|store| store.attempt_contract(task, frozen))
    }
    pub(crate) fn task_contract_reference(&self, task: &str) -> Result<Option<crate::domain::VersionedReference>> {
        self.read(|store| store.task_contract_reference(task))
    }
    pub fn open(path:&Path,control:ReadControl)->Result<Self> { Self::open_checked(path,control,true,None) }
    /// Foreground operations validate the selected rows; full integrity scans
    /// remain an explicit administrative/open-time diagnostic.
    pub(crate) fn open_scoped(path:&Path,control:ReadControl)->Result<Self> { Self::open_checked(path,control,false,None) }
    pub(crate) fn open_scoped_observed(path:&Path,control:ReadControl,work:SqlWork)->Result<Self> { Self::open_checked(path,control,false,Some(work)) }
    fn open_checked(path:&Path,control:ReadControl,integrity:bool,sql_work:Option<SqlWork>)->Result<Self> {
        control.check()?;engine_check()?;
        let before=std::fs::symlink_metadata(path).map_err(|e|StoreError::Io(e.to_string()))?;
        if !before.is_file()||before.nlink()!=1{return Err(StoreError::Invalid("controlled database must be a single-link regular file".into()));}
        for suffix in ["-wal","-shm","-journal"] {
            let mut name=path.as_os_str().to_os_string();name.push(suffix);
            match std::fs::symlink_metadata(Path::new(&name)) {
                Ok(m) if !m.is_file()||m.nlink()!=1=>return Err(StoreError::Invalid("controlled database sidecar must be a single-link regular file".into())),
                Ok(_)=>{},Err(e) if e.kind()==std::io::ErrorKind::NotFound=>{},Err(e)=>return Err(StoreError::Io(e.to_string())),
            }
        }
        control.check()?;
        let connection=Connection::open_with_flags(path,OpenFlags::SQLITE_OPEN_READ_WRITE|OpenFlags::SQLITE_OPEN_NO_MUTEX|OpenFlags::SQLITE_OPEN_NOFOLLOW)?;
        let interrupted=Arc::new(AtomicU8::new(0));
        connection.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH,control.row_bytes)?;
        let c=control.clone();let reason=interrupted.clone();
        connection.progress_handler(1000,Some(move||{let n=c.reason();if n!=0{reason.store(n,Ordering::SeqCst);}n!=0}));
        let c=control.clone();let reason=interrupted.clone();
        connection.commit_hook(Some(move||{let n=c.reason();if n!=0{reason.store(n,Ordering::SeqCst);}n!=0}));
        connection.busy_timeout(Duration::from_millis(10).min(control.deadline.saturating_duration_since(Instant::now())))?;
        let work_budget=read_budget::ReadBudget::new(control.clone());
        let store=Self{store:SqliteStore{connection},control,interrupted,work_budget,sql_work};
        if let Some(work)=&store.sql_work {
            // The field retains the callback context until Drop unregisters it.
            unsafe { work.attach(&store.store.connection); }
        }
        store.read(|s|{s.connection.execute_batch("PRAGMA foreign_keys=ON; PRAGMA trusted_schema=OFF; PRAGMA synchronous=FULL;")?;Ok(())})?;
        store.read(|s|check_schema(&s.connection))?;
        store.read(|s|enable_wal(&s.connection))?;
        if integrity {store.read(SqliteStore::integrity_check)?;} else {store.read(|s|super::integrity::check_if_schema_changed(path,s))?;}
        let after=std::fs::symlink_metadata(path).map_err(|e|StoreError::Io(e.to_string()))?;
        if !after.is_file()||after.nlink()!=1||(before.dev(),before.ino())!=(after.dev(),after.ino()){return Err(StoreError::Invalid("controlled database changed during open".into()));}
        store.control.check()?;Ok(store)
    }
    pub(crate) fn prepare_admission(&mut self,project:&Path)->anyhow::Result<Option<LaunchInputs>> {
        self.control.check()?;
        let result=crate::admission::prepare_held(project,&mut self.store,&self.control,&self.work_budget);
        let value=self.admission_result(result)?;
        self.control.check()?;
        Ok(value)
    }
    pub(crate) fn decide_admission(&mut self,project:&Path)->anyhow::Result<crate::admission::AdmissionDecision> {
        self.control.check()?;
        let result=crate::admission::decide_held(project,&mut self.store,&self.control,&self.work_budget);
        self.admission_result(result)
    }
    fn admission_result<T>(&self,result:anyhow::Result<T>)->anyhow::Result<T> {
        // A successful committed reservation must not be relabelled as failure.
        result.map_err(|error| match self.interrupted.load(Ordering::SeqCst) {
            1=>StoreError::Cancelled.into(),2=>StoreError::Deadline.into(),_=>error,
        })
    }
    fn error(&self,error:StoreError)->StoreError {
        match self.interrupted.load(Ordering::SeqCst) {1=>StoreError::Cancelled,2=>StoreError::Deadline,_=>error}
    }
    fn read<T>(&self,read:impl FnOnce(&SqliteStore)->Result<T>)->Result<T> {
        self.control.check()?;let value=read(&self.store).map_err(|e|self.error(e))?;self.control.check()?;Ok(value)
    }
    /// Cancellable pieces of the resumable whole-store check.
    pub(crate) fn integrity_schema(&self)->Result<u32> {self.read(super::integrity::schema)}
    pub(crate) fn integrity_tables(&self)->Result<Vec<String>> {self.read(super::integrity::tables)}
    pub(crate) fn integrity_check_table(&self,table:&str)->Result<()> {self.read(|s|super::integrity::check_table(s,table))}
    /// Scoped advisory decoration rows; never reads the historical snapshot.
    pub fn attempt_tokens(&mut self, bindings: &[String]) -> Result<super::attempt_tokens::Rows> {
        self.control.check()?;
        self.store.attempt_token_rows(bindings, &self.work_budget).map_err(|e| self.error(e))
    }
    pub fn project_control(&self)->Result<Option<ProjectControl>> {self.read(SqliteStore::project_control)}
    pub fn import_operation_count(&self)->Result<u64> {self.read(SqliteStore::import_operation_count)}
    pub fn import_receipt(&self)->Result<(String,u64,u64)> {self.read(SqliteStore::import_receipt)}
    pub fn current_head(&self)->Result<u64> {self.read(SqliteStore::current_head)}
    pub fn read_targeted_hot_path(&self,now:i64,include_launches:bool)->Result<u32> {
        self.read(|store|store.read_targeted_hot_path(now,include_launches))
    }
    pub fn reconcile_active_work(&mut self,max_pages:Option<u32>)->Result<crate::store::active_work::ActiveWorkRun> {
        self.control.check()?;
        let value=self.store.reconcile_active_work_with_budget(max_pages,Some(&self.work_budget)).map_err(|e|self.error(e))?;
        self.control.check()?;
        Ok(value)
    }
    pub fn read_snapshot(&mut self,at:Option<u64>)->Result<Snapshot> {
        self.control.check()?;
        let budget=read_budget::ReadBudget::new(self.control.clone());
        let value=self.store.read_snapshot_with_budget(at,Some(&budget)).map_err(|e|self.error(e))?;
        self.control.check()?;Ok(value)
    }
    pub(crate) fn launch_rows(&mut self,at:u64,task:&TaskId,binding:&str,approval:Option<&VersionedReference>)->Result<super::effect_rows::LaunchRows> {
        self.control.check()?;
        let budget=read_budget::ReadBudget::new(self.control.clone());
        let value=self.store.launch_rows(at,task,binding,approval,Some(&budget)).map_err(|e|self.error(e))?;
        self.control.check()?;Ok(value)
    }
    #[cfg(target_os="linux")]
    pub fn native_profile_report(&mut self, reference:&VersionedReference)->Result<Option<serde_json::Value>> {
        self.control.check()?;
        let value=self.store.native_profile_report(reference).map_err(|e|self.error(e))?;
        self.control.check()?;
        Ok(value)
    }
    pub(crate) fn schedule_routine(&mut self,prepared:&PreparedRoutineTick,head:u64)->Result<Option<RoutineOccurrence>> {
        self.control.check()?;
        self.store.schedule_routine_with_budget(prepared,head,Some(&self.work_budget)).map_err(|e|self.error(e))
    }
    pub(crate) fn routine_planning_selection(&mut self,last:Option<&str>)->Result<(u64,Option<RoutineDefinition>)> {
        self.control.check()?;
        let value=self.store.routine_planning_selection(last,&self.work_budget).map_err(|e|self.error(e))?;
        self.control.check()?;Ok(value)
    }
    pub(crate) fn routine_planning_turn(&mut self,turn:u64)->Result<(u64,Option<RoutineDefinition>)> {
        self.control.check()?;
        let value=self.store.routine_planning_turn(turn,&self.work_budget).map_err(|e|self.error(e))?;
        self.control.check()?;Ok(value)
    }
    pub(crate) fn record_observations(&mut self,head:u64,observations:&[crate::reconcile::RuntimeObservation])->Result<u64> {
        self.control.check()?;
        self.store.record_observations_with_budget(head,observations,Some(&self.work_budget)).map_err(|e|self.error(e))
    }
    pub(crate) fn expire_claims(&mut self,now:i64)->Result<usize> {self.mutation(|store|store.expire_claims(now))}
    pub(crate) fn validate_launch_draft(&mut self,inputs:&LaunchInputs,head:u64,now:i64)->Result<()> {
        self.control.check()?;
        self.store.validate_launch_draft(inputs,head,now).map_err(|e|self.error(e))?;
        self.control.check()
    }
    pub(crate) fn reserve_prepared(&mut self,prepared:&[PreparedLaunch],head:u64,now:i64,dispatch:&DispatchContext)->Result<Reservation> {
        self.mutation(|store|store.reserve_prepared_dispatched(prepared,head,now,dispatch))
    }
    pub(crate) fn render_launch_knowledge(&mut self,project:&Path,id:&str)->anyhow::Result<serde_json::Value> {
        self.control.check()?;
        anyhow::ensure!(project.join(".state/state.db").canonicalize()?.to_str()==self.store.connection.path(), "knowledge store belongs to another project");
        let result=crate::memory::render_knowledge_snapshot_budgeted(project,id,&mut self.store,Some(&self.work_budget))?;
        let task=result["snapshot"]["task_id"].as_str().ok_or_else(||anyhow::anyhow!("knowledge task missing"))?;
        let mut used=0usize;
        for object in self.store.memory_consumed_objects_with_budget(task,Some(&self.work_budget)).map_err(|e|self.error(e))? {
            self.control.check()?;
            let bytes=crate::memory::read_object_controlled(&project.join(".state/objects"),&object,(64*1024*1024-used) as u64,Some(&self.work_budget))?;
            used=used.checked_add(bytes.len()).ok_or_else(||anyhow::anyhow!("knowledge evidence size overflow"))?;
            anyhow::ensure!(used<=64*1024*1024,"knowledge evidence exceeds read budget");
        }
        self.control.check()?;
        Ok(result)
    }
    fn mutation<T>(&mut self,write:impl FnOnce(&mut SqliteStore)->Result<T>)->Result<T> {
        self.control.check()?;
        // A successful commit remains success even if cancellation arrives
        // during final fsync. Only a latched hook veto explains a failed commit.
        write(&mut self.store).map_err(|e|self.error(e))
    }
}

impl Drop for ControlledStore {
    fn drop(&mut self) {
        if self.sql_work.is_some() { SqlWork::detach(&self.store.connection); }
    }
}

#[cfg(test)]
mod tests;

impl ControlledStore {
    pub(crate) fn service_result_completions(&mut self)->Result<(bool,Option<String>)> {
        self.control.check()?;
        self.store.service_result_completions().map_err(|error|self.error(error))
    }
}

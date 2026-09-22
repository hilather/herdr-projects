//! Deterministic recovery advice from one fenced snapshot and a fresh live batch.
//! Advice never grants effect authority, even for a retry candidate.
use anyhow::{Result,ensure};
use serde::Serialize;
use crate::{domain::{Snapshot,AttemptState},operations::DeliveryState};
use super::ObservationBatch;

#[derive(Debug,Clone,Copy,PartialEq,Eq,Serialize)]
#[serde(rename_all="snake_case")]
pub enum Classification { Matched, AlreadyCompleted, CancelledOrStale, RetryCandidate, Waiting, RequiresReconciliation }
#[derive(Debug,Clone,Copy,PartialEq,Eq,Serialize)]
#[serde(rename_all="snake_case")]
pub enum RepairAction { None, RefreshObservation, InspectEndpoint, ResolveIdentityConflict, AdoptResources, InspectTerminationEvidence, InspectLaunchIdentity, InspectReceipt, ExpireClaim, Wait, DeliverAfterValidation, RetireStaleIntent, InspectUnsupportedAdapter }
#[derive(Debug,PartialEq,Eq,Serialize)]
pub struct RepairItem {
    pub entity_kind:String,
    pub entity:String,
    pub expected_revision:u64,
    pub classification:Classification,
    pub action:RepairAction,
    pub reason:String,
}
#[derive(Debug,PartialEq,Eq,Serialize)]
pub struct RecoveryPlan {
    pub expected_head:u64,
    pub retained_attempts:usize,
    pub dispatch_allowed:bool,
    pub items:Vec<RepairItem>,
}

// Creation intent proves that an external effect may have happened. Missing
// target observations prove neither absence nor termination, even after expiry.
fn unobserved_creations(snapshot:&Snapshot)->std::collections::BTreeSet<&crate::domain::OperationId> {
    let creation=snapshot.events.iter().filter(|e|e.kind=="runtime.launch_creation").map(|e|e.entity.as_str()).collect::<std::collections::BTreeSet<_>>();
    let observed=snapshot.events.iter().filter(|e|matches!(e.kind.as_str(),"runtime.launch_target"|"runtime.launch_started")).map(|e|e.entity.as_str()).collect::<std::collections::BTreeSet<_>>();
    let retained=snapshot.attempts.iter().filter(|a|a.retains_capacity()).map(|a|&a.id).collect::<std::collections::BTreeSet<_>>();
    snapshot.attempt_inputs.iter().filter(|r|retained.contains(&r.attempt)&&creation.contains(r.operation.as_str())&&!observed.contains(r.operation.as_str())).map(|r|&r.operation).collect()
}
const UNOBSERVED_CREATION:&str="Launch creation may have occurred, but no exact process identity was recorded. An empty process inventory or expired claim does not prove termination. Retain capacity and resources; inspect launch creation evidence without recreating or releasing the worker.";

pub fn build(snapshot:&Snapshot,batch:&ObservationBatch,now:i64,config:Option<&str>)->Result<RecoveryPlan> {
    use Classification as C;use RepairAction as A;use super::ResourceState as S;
    ensure!(now>=0&&batch.expected_head==snapshot.head&&!batch.dispatch_allowed&&batch.recorded_head.is_none(),"recovery plan requires an unapplied observation batch at the current head");
    ensure!(batch.observations.len()==snapshot.runtime_bindings.len(),"recovery plan requires complete observations");
    let mut seen=std::collections::BTreeSet::new();
    for o in &batch.observations {o.validate().map_err(anyhow::Error::msg)?;ensure!(seen.insert(&o.binding)&&snapshot.runtime_bindings.iter().any(|b|b.id==o.binding&&b.revision==o.binding_revision&&o.task_revision==b.task.as_ref().and_then(|id|snapshot.tasks.iter().find(|t|&t.id==id).map(|t|t.revision))),"observation identities changed");}
    let unobserved=unobserved_creations(snapshot);
    let mut items=Vec::new();let mut matched_attempts=std::collections::BTreeSet::new();
    let mut add=|kind:&str,id:&str,revision:u64,classification,action,reason:&str|items.push(RepairItem{entity_kind:kind.into(),entity:id.into(),expected_revision:revision,classification,action,reason:reason.into()});
    for binding in &snapshot.runtime_bindings {
        let observation=batch.observations.iter().find(|o|o.binding==binding.id).expect("complete checked above");
        let owned=snapshot.ownership.iter().find(|o|o.binding==binding.id);
        let task=binding.task.as_ref().and_then(|id|snapshot.tasks.iter().find(|t|&t.id==id));
        let fresh=observation.observed_unix_ms<=now&&now-observation.observed_unix_ms<=30_000&&observation.config_digest.as_deref()==config;
        let (classification,action,reason)=if !fresh {(C::RequiresReconciliation,A::RefreshObservation,"Observation is stale or belongs to another config; collect again.")}
        else if snapshot.attempt_inputs.iter().any(|r|r.inputs.binding==binding.id&&unobserved.contains(&r.operation)) {(C::RequiresReconciliation,A::InspectLaunchIdentity,UNOBSERVED_CREATION)}
        else if binding.identity.machine.is_empty()&&binding.identity.pane_id.is_empty()&&binding.identity.worktree_path.is_empty() {(C::Matched,A::None,"No external resource is recorded.")}
        else if observation.pane==S::Unknown||observation.worktree==S::Unknown||!binding.identity.machine.is_empty() {(C::RequiresReconciliation,A::InspectEndpoint,"Endpoint or resource identity is unverified; absence and termination are not established.")}
        else if observation.pane==S::Mismatch||observation.worktree==S::Mismatch {(C::RequiresReconciliation,A::ResolveIdentityConflict,"Recorded and live identities disagree; retain resources and attempt capacity.")}
        else if observation.pane==S::Absent||observation.worktree==S::Absent {(C::RequiresReconciliation,A::InspectTerminationEvidence,"A recorded resource is absent; this alone does not prove worker termination.")}
        else if let Some(owned)=owned {
            if crate::store::ownership::observed(binding,task.map(|t|t.revision),observation,now,config)&&crate::store::ownership::matches(owned,binding,observation)? {
                if let Some(id)=&owned.attempt {if task.is_some_and(|t|t.active_attempt.as_ref()==Some(id)&&t.state==crate::domain::TaskState::Running)&&snapshot.attempts.iter().any(|a|&a.id==id&&binding.task.as_ref()==Some(&a.task)&&a.retains_capacity()&&matches!(a.state,AttemptState::Running|AttemptState::AwaitingInput)) {matched_attempts.insert(id.clone());}}
                (C::Matched,A::None,"Fresh resource evidence matches the adopted claim; this is not dispatch authorization.")
            }else{(C::RequiresReconciliation,A::ResolveIdentityConflict,"Ownership no longer matches live evidence; do not replace or release the worker.")}
        }else if crate::store::ownership::observed(binding,task.map(|t|t.revision),observation,now,config) {(C::RequiresReconciliation,A::AdoptResources,"Local evidence is an adoption candidate; adoption must recheck root conflicts and retained attempts.")}
        else {(C::RequiresReconciliation,A::InspectEndpoint,"Resource incarnation evidence is incomplete; no ownership can be inferred.")};
        add("runtime",&binding.id,binding.revision,classification,action,reason);
    }
    for attempt in &snapshot.attempts {
        let (classification,action,reason)=if !attempt.retains_capacity(){(C::AlreadyCompleted,A::None,"Termination is recorded; task success is a separate decision.")}
        else if snapshot.attempt_inputs.iter().any(|r|r.attempt==attempt.id&&unobserved.contains(&r.operation)){(C::RequiresReconciliation,A::InspectLaunchIdentity,UNOBSERVED_CREATION)}
        else if matched_attempts.contains(&attempt.id){(C::Matched,A::None,"Live adopted worker matches; its reservation remains held.")}
        else{(C::RequiresReconciliation,A::InspectTerminationEvidence,"Worker liveness is unresolved; retain capacity, including lost or unselected attempts.")};
        add("attempt",attempt.id.as_str(),attempt.revision,classification,action,reason);
    }
    for operation in &snapshot.operations {
        let delivery=snapshot.deliveries.iter().find(|d|d.operation==operation.id);
        let revision=delivery.map(|d|d.revision).unwrap_or(0);
        let (classification,action,reason)=if operation.kind=="runtime.launch"&&unobserved.contains(&operation.id) {
            if delivery.is_some_and(|d|d.state==DeliveryState::Claimed&&d.lease_until_ms.is_some_and(|until|until>now)) {
                (C::Waiting,A::Wait,"Launch creation is awaiting exact process identity under its original claim. Do not replay creation or release capacity; missing inventory is not termination evidence.")
            }else{(C::RequiresReconciliation,A::InspectLaunchIdentity,UNOBSERVED_CREATION)}
        }else{match delivery {
            None=>(C::RequiresReconciliation,A::InspectUnsupportedAdapter,"Delivery state is unavailable; inspect or upgrade the store."),
            Some(d)=>match d.state {
                DeliveryState::Confirmed=>(C::AlreadyCompleted,A::None,"Delivery is confirmed; confirmation is not task success."),
                DeliveryState::PermanentFailure=>(C::CancelledOrStale,A::None,"Delivery has a terminal disposition; no retry is requested."),
                DeliveryState::Ambiguous=>(C::RequiresReconciliation,A::InspectReceipt,"Effect may already have occurred, even if the task changed; inspect exact receipts and never blindly replay."),
                DeliveryState::Claimed if d.lease_until_ms.is_some_and(|until|until>now)=>(C::Waiting,A::Wait,"A delivery claim is still live; do not steal it."),
                DeliveryState::Claimed=>(C::RequiresReconciliation,A::ExpireClaim,"Claim expired or lacks lease evidence; expiry records ambiguity, not permission to replay."),
                DeliveryState::Pending if operation.task.is_some()&&!snapshot.tasks.iter().any(|t|Some(&t.id)==operation.task.as_ref()&&t.revision==operation.expected_revision)=>(C::CancelledOrStale,A::RetireStaleIntent,"Task revision changed; inspect and explicitly retire this stale intent."),
                DeliveryState::Pending if operation.task.is_none()&&!snapshot.control.as_ref().is_some_and(|c|c.revision==operation.expected_revision)=>(C::CancelledOrStale,A::RetireStaleIntent,"Project control revision changed; inspect and explicitly retire this stale intent."),
                DeliveryState::Pending if d.next_due_ms>now=>(C::Waiting,A::Wait,"Retry is not due yet."),
                DeliveryState::Pending if matches!(operation.kind.as_str(),"runtime.notification"|"runtime.finalization")=>(C::RetryCandidate,A::DeliverAfterValidation,"Explicit adapter must revalidate typed policy, control, revisions and resources before claiming or delivering."),
                DeliveryState::Pending=>(C::RequiresReconciliation,A::InspectUnsupportedAdapter,"No automatic adapter is available; inspect imported receipts or retire explicitly."),
            }
        }};
        add("operation",operation.id.as_str(),revision,classification,action,reason);
    }
    items.sort_by(|a,b|(&a.entity_kind,&a.entity).cmp(&(&b.entity_kind,&b.entity)));
    Ok(RecoveryPlan{expected_head:snapshot.head,retained_attempts:snapshot.attempts.iter().filter(|a|a.retains_capacity()).count(),dispatch_allowed:false,items})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{domain::*,operations::{Delivery,Outcome},store::SqliteStore};
    fn fixture()->(tempfile::TempDir,SqliteStore,Snapshot,ObservationBatch) {
        let temp=tempfile::tempdir().unwrap();let mut db=SqliteStore::create(&temp.path().join("state.db")).unwrap();let task=Task{id:TaskId::new("t").unwrap(),revision:1,state:TaskState::Blocked,title:"fixture".into(),active_attempt:None};
        let attempt=Attempt{id:AttemptId::new("lost").unwrap(),task:task.id.clone(),revision:1,state:AttemptState::Lost,snapshot:None,reservation:"slot".into(),termination_observed:false};
        let op=Operation{id:OperationId::new("op").unwrap(),task:Some(task.id.clone()),kind:"runtime.notification".into(),target:"coordinator".into(),payload_version:1,payload:serde_json::json!({}),expected_revision:1,due_unix_ms:0,idempotency_key:"op".into()};
        db.commit(Commit{expected_head:0,mutations:vec![Mutation::Task{expected:None,next:task},Mutation::Attempt{expected:None,next:attempt},Mutation::Enqueue(op)]}).unwrap();let snapshot=db.read_snapshot(None).unwrap();let batch=ObservationBatch{expected_head:snapshot.head,observations:vec![],dispatch_allowed:false,recorded_head:None};(temp,db,snapshot,batch)
    }
    #[test]
    fn ambiguous_stale_effects_never_become_retry_candidates_or_release_lost_capacity() {
        let(_temp,mut db,mut snapshot,batch)=fixture();let original=db.read_snapshot(None).unwrap();snapshot.tasks[0].revision+=1;
        snapshot.deliveries=vec![Delivery{operation:snapshot.operations[0].id.clone(),revision:2,state:DeliveryState::Ambiguous,epoch:1,attempts:1,owner:None,lease_until_ms:None,next_due_ms:0,last_outcome:Some(Outcome::Ambiguous{observation_required:"transport died".into()})}];
        let report=build(&snapshot,&batch,100,None).unwrap();assert!(!report.dispatch_allowed);assert_eq!(report.retained_attempts,1);assert_eq!(report.items.iter().find(|i|i.entity_kind=="operation").unwrap().action,RepairAction::InspectReceipt);assert_eq!(report.items.iter().find(|i|i.entity_kind=="attempt").unwrap().action,RepairAction::InspectTerminationEvidence);assert_eq!(build(&snapshot,&batch,100,None).unwrap(),report);assert_eq!(db.read_snapshot(None).unwrap(),original);
        snapshot.deliveries[0].state=DeliveryState::Pending;assert_eq!(build(&snapshot,&batch,100,None).unwrap().items.iter().find(|i|i.entity_kind=="operation").unwrap().action,RepairAction::RetireStaleIntent);
    }
    #[test]
    fn expired_claims_require_expiry_and_pending_adapters_only_receive_advice() {
        let(_temp,_db,mut snapshot,mut batch)=fixture();let mut delivery=snapshot.deliveries[0].clone();delivery.state=DeliveryState::Claimed;delivery.lease_until_ms=Some(101);snapshot.deliveries[0]=delivery;
        let action=|s:&Snapshot,now|build(s,&batch,now,None).unwrap().items.into_iter().find(|i|i.entity_kind=="operation").unwrap().action;
        assert_eq!(action(&snapshot,100),RepairAction::Wait);assert_eq!(action(&snapshot,101),RepairAction::ExpireClaim);
        snapshot.deliveries[0].state=DeliveryState::Pending;assert_eq!(action(&snapshot,101),RepairAction::DeliverAfterValidation);snapshot.operations[0].kind="legacy.notification".into();assert_eq!(action(&snapshot,101),RepairAction::InspectUnsupportedAdapter);
        batch.expected_head+=1;assert!(build(&snapshot,&batch,101,None).is_err());batch.expected_head-=1;batch.recorded_head=Some(snapshot.head);assert!(build(&snapshot,&batch,101,None).is_err());
    }
}

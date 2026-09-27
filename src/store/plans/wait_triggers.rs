use super::*;
use crate::domain::WaitTrigger;

pub(super) fn load(db: &Connection, wait: &str, budget: Option<&read_budget::ReadBudget>) -> Result<Option<WaitTrigger>> {
    let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < 43 { return Ok(None); }
    let row: Option<(String, String, u64, Option<u64>)> = read_budget::optional(db,
        "SELECT kind,reference_id,reference_revision,reference_generation FROM wait_triggers WHERE wait_id=?1",
        [wait],budget,&[],|row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)))?;
    let Some((kind, id, revision, generation)) = row else { return Ok(None); };
    let (trigger,condition) = match kind.as_str() {
        "owned_runtime_recovered" => (WaitTrigger::OwnedRuntimeRecovered { binding_id:id,binding_revision:revision,ownership_revision:generation.ok_or_else(||StoreError::Corrupt("missing recovery ownership revision".into()))? },"adapter_recovery"),
        "approval_decision" => (WaitTrigger::ApprovalDecision { approval_id:id, task_revision:revision },"user_decision"),
        "attempt_capacity_released" => (WaitTrigger::AttemptCapacityReleased { attempt_id:crate::domain::AttemptId::new(id).map_err(StoreError::Corrupt)?, after_revision:revision },"resource_availability"),
        _ => return Err(StoreError::Corrupt("unknown wait trigger".into())),
    };
    trigger.validate(condition).map_err(StoreError::Corrupt)?;
    Ok(Some(trigger))
}

pub(super) fn insert(db: &Connection, wait: &str, trigger: &WaitTrigger, budget:Option<&read_budget::ReadBudget>) -> Result<()> {
    match trigger {
        WaitTrigger::OwnedRuntimeRecovered { binding_id,binding_revision,ownership_revision } => {
            wait_recovery::validate_reference(db,binding_id,*binding_revision,*ownership_revision,budget)?;
            db.execute("INSERT INTO wait_triggers(wait_id,kind,reference_id,reference_revision,reference_generation) VALUES(?1,'owned_runtime_recovered',?2,?3,?4)",
                params![wait,binding_id,integer(*binding_revision)?,integer(*ownership_revision)?])?;
        }
        WaitTrigger::AttemptCapacityReleased { attempt_id, after_revision } => {
            let revision:Option<u64>=read_budget::optional(db,"SELECT revision FROM attempts WHERE id=?1",[attempt_id.as_str()],budget,&[],|row|row.get(0))?;
            if revision.is_none_or(|revision|revision<*after_revision) {return Err(invalid("capacity wait references a missing attempt or future revision"));}
            db.execute("INSERT INTO wait_triggers(wait_id,kind,reference_id,reference_revision) VALUES(?1,'attempt_capacity_released',?2,?3)",
                params![wait,attempt_id.as_str(),integer(*after_revision)?])?;
        }
        WaitTrigger::ApprovalDecision { approval_id, task_revision } => {
            db.execute("INSERT INTO wait_triggers(wait_id,kind,reference_id,reference_revision) VALUES(?1,'approval_decision',?2,?3)",
                params![wait,approval_id,integer(*task_revision)?])?;
        }
    }
    Ok(())
}

pub(super) fn current(db: &Connection, trigger: &WaitTrigger, budget: Option<&read_budget::ReadBudget>) -> Result<Option<(String,String,i64)>> {
    match trigger {
        WaitTrigger::OwnedRuntimeRecovered { binding_id,binding_revision,.. } => wait_recovery::current(db,binding_id,*binding_revision,budget),
        WaitTrigger::AttemptCapacityReleased { attempt_id, .. } => read_budget::optional(db,
            "SELECT kind,entity,sequence FROM events WHERE entity=?1 AND revision=(SELECT revision FROM attempts WHERE id=?1) AND kind IN ('runtime.worker_terminated','attempt.cancellation_requested') UNION ALL SELECT e.kind,e.entity,e.sequence FROM attempt_inputs i JOIN events e ON e.entity=i.operation_id WHERE i.attempt_id=?1 AND e.revision=(SELECT revision FROM attempts WHERE id=?1) AND e.kind IN ('runtime.launch_stopped','runtime.worktrees_stopped') ORDER BY sequence DESC LIMIT 1",
            [attempt_id.as_str()],budget,&[],|row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))),
        WaitTrigger::ApprovalDecision { approval_id, .. } => read_budget::optional(db,
            "SELECT kind,entity,sequence FROM events WHERE entity=?1 AND kind IN ('approval.installed','approval.revoked') ORDER BY sequence DESC LIMIT 1",
            [approval_id],budget,&[],|row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))),
    }
}

pub(super) fn relevant(db: &Connection, task: &str, condition: &str, trigger: &WaitTrigger, kind: &str, entity: &str, sequence: i64, budget: Option<&read_budget::ReadBudget>) -> Result<bool> {
    trigger.validate(condition).map_err(StoreError::Corrupt)?;
    match trigger {
        WaitTrigger::OwnedRuntimeRecovered { binding_id,binding_revision,ownership_revision } => {
            if kind!="runtime.observed" || entity!=binding_id {return Ok(false);}
            wait_recovery::relevant(db,binding_id,*binding_revision,*ownership_revision,sequence,budget)
        }
        WaitTrigger::AttemptCapacityReleased { attempt_id, after_revision } => {
            let path=match kind {
                "runtime.worker_terminated" if entity==attempt_id.as_str() => "/attempt",
                "attempt.cancellation_requested" if entity==attempt_id.as_str() => "/attempt/id",
                "runtime.launch_stopped" | "runtime.worktrees_stopped" => {
                    let related:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM attempt_inputs WHERE attempt_id=?1 AND operation_id=?2)",params![attempt_id.as_str(),entity],|row|row.get(0))?;
                    if !related {return Ok(false);}
                    if kind=="runtime.launch_stopped" {"/target/attempt"} else {"/attempt"}
                }
                _ => return Ok(false),
            };
            let attempt=super::super::read_attempt_with_budget(db,attempt_id,budget)?;
            if attempt.retains_capacity() || attempt.revision<=*after_revision {return Ok(false);}
            // Match the committed termination generation and receipt identity.
            // A cancel request, worker output or event name alone is insufficient.
            let mut stmt=db.prepare("SELECT payload,payload_version,sequence FROM events WHERE kind=?1 AND entity=?2 AND revision=?3 ORDER BY sequence LIMIT 2")?;
            let mut rows=stmt.query(params![kind,entity,integer(attempt.revision)?])?;
            let Some(row)=rows.next()? else {return Ok(false);};
            if let Some(budget)=budget {budget.row(row,&[(0,1)])?;}
            let payload:String=row.get(0)?;
            let version:u32=row.get(1)?;
            let receipt_sequence:i64=row.get(2)?;
            if version!=1 || payload.len()>MAX_RECORD_BYTES || rows.next()?.is_some() {
                return Err(StoreError::Corrupt("invalid capacity release receipt".into()));
            }
            if receipt_sequence!=sequence {return Ok(false);}
            let value:serde_json::Value=serde_json::from_str(&payload).map_err(|_|StoreError::Corrupt("invalid capacity release receipt".into()))?;
            Ok(value.pointer(path).and_then(|value|value.as_str())==Some(attempt_id.as_str())
                && (kind!="attempt.cancellation_requested" || value.get("released").and_then(|value|value.as_bool())==Some(true)))
        }
        WaitTrigger::ApprovalDecision { approval_id, task_revision } => {
            if entity != approval_id || !matches!(kind,"approval.installed"|"approval.revoked") { return Ok(false); }
            // Events alone are not a decision. Require the immutable record
            // written by authenticated ingress, including its content identity.
            let exists: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM approval_grants WHERE id=?1)",[approval_id],|row|row.get(0))?;
            if !exists { return Ok(false); }
            let grant = super::super::approvals::grant(db,approval_id,budget)?;
            let revision: Option<u64> = read_budget::optional(db,"SELECT revision FROM tasks WHERE id=?1",[task],budget,&[],|row|row.get(0))?;
            if grant.scope.task.as_str() != task || grant.scope.task_revision != *task_revision
                || !revision.is_some_and(|revision| revision == *task_revision || revision.checked_add(1) == Some(*task_revision)) {
                return Ok(false);
            }
            if kind == "approval.revoked" {
                return Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM approval_revocations WHERE approval_id=?1)",[approval_id],|row|row.get(0))?);
            }
            Ok(true)
        }
    }
}

//! Indexed rows for single-intent effect commands. Each read is fenced to one
//! event head (as `read_snapshot(Some(head))` is) without decoding retained history.
use super::*;
use crate::operations::Delivery;

/// Rows `Finalization::validate_rows` reads for one intent.
pub struct FinalizationRows {
    pub head: u64,
    pub control: ProjectControl,
    pub tasks: Vec<Task>,
    pub attempts: Vec<Attempt>,
    pub bindings: Vec<RuntimeBinding>,
}

pub(crate) struct RoutineRows {
    pub(crate) definition: RoutineDefinition,
    pub(crate) occurrence: RoutineOccurrence,
    pub(crate) delivery: Delivery,
}

/// Rows launch drafting/reservation read: the selected task and binding, the
/// current policy/control/budget identities and, if named, one installed approval.
pub(crate) struct LaunchRows {
    pub(crate) task: Option<Task>,
    pub(crate) binding: Option<RuntimeBinding>,
    pub(crate) scheduler_revision: u64,
    pub(crate) control_epoch: u64,
    pub(crate) budget: Option<VersionedReference>,
    /// Grant, whether it was revoked, and whether it was consumed.
    pub(crate) approval: Option<(ApprovalGrant, bool, bool)>,
    /// Every queued edge bound to its current valid satisfaction; `None` while any edge is unsatisfied.
    pub(crate) dependencies: Option<Vec<DependencyInput>>,
}

fn fenced(db: &Connection, at: Option<u64>) -> Result<u64> {
    check_schema(db)?;
    let head = head(db)?;
    if let Some(at) = at { if at != head { return Err(StoreError::HistoryUnavailable(at)); } }
    Ok(head)
}

fn find_task(db: &Connection, id: &TaskId) -> Result<Option<Task>> {
    match read_task(db, id.as_str()) { Err(StoreError::Conflict) => Ok(None), other => other.map(Some) }
}

impl SqliteStore {
    /// One operation and its delivery at the current (or expected) head.
    pub fn operation_rows(&mut self, id: &OperationId, at: Option<u64>) -> Result<(u64, Option<(Operation, Delivery)>)> {
        let tx = self.connection.transaction()?;
        let head = fenced(&tx, at)?;
        let rows = match read_operation(&tx, id) {
            Err(StoreError::Conflict) => None,
            operation => Some((operation?, delivery::delivery(&tx, id)?)),
        };
        tx.commit()?;
        Ok((head, rows))
    }
    /// Control, the intent's task, its capacity-retaining attempts and the named binding.
    pub fn finalization_rows(&mut self, operation: &Operation, binding: &str, at: Option<u64>) -> Result<FinalizationRows> {
        let tx = self.connection.transaction()?;
        let head = fenced(&tx, at)?;
        let control = control::read(&tx)?;
        let tasks = match &operation.task { Some(id) => find_task(&tx, id)?.into_iter().collect(), None => Vec::new() };
        let attempts = match &operation.task {
            Some(id) => read_attempt_rows(&tx, "SELECT id,task_id,revision,state,snapshot,reservation,termination_observed FROM attempts WHERE task_id=?1 AND termination_observed=0 ORDER BY id", [id.as_str()], None)?,
            None => Vec::new(),
        };
        let bindings = runtime::read_binding(&tx, binding, None)?.into_iter().collect();
        tx.commit()?;
        Ok(FinalizationRows { head, control, tasks, attempts, bindings })
    }
    /// One task at the expected head.
    pub fn task_at(&mut self, id: &TaskId, at: Option<u64>) -> Result<Option<Task>> {
        let tx = self.connection.transaction()?;
        fenced(&tx, at)?;
        let task = find_task(&tx, id)?;
        tx.commit()?;
        Ok(task)
    }
    pub(crate) fn launch_rows(&mut self, at: u64, task: &TaskId, binding: &str, approval: Option<&VersionedReference>, budget: Option<&read_budget::ReadBudget>) -> Result<LaunchRows> {
        let tx = self.connection.transaction()?;
        fenced(&tx, Some(at))?;
        let rows = LaunchRows {
            task: match read_task_with_budget(&tx, task.as_str(), budget) { Err(StoreError::Conflict) => None, other => Some(other?) },
            binding: runtime::read_binding(&tx, binding, budget)?,
            scheduler_revision: scheduler::read_policy(&tx, budget)?.revision,
            control_epoch: control::read_with_budget(&tx, budget)?.epoch,
            budget: budget::current_with_budget(&tx, budget)?.map(|p| p.reference()).transpose().map_err(StoreError::Corrupt)?,
            approval: match approval {
                Some(reference) if tx.query_row("SELECT EXISTS(SELECT 1 FROM approval_grants WHERE id=?1)", [&reference.id], |r| r.get(0))? => {
                    let grant = approvals::grant(&tx, &reference.id, budget)?;
                    let revoked = tx.query_row("SELECT EXISTS(SELECT 1 FROM approval_revocations WHERE approval_id=?1)", [&reference.id], |r| r.get(0))?;
                    let consumed = tx.query_row("SELECT EXISTS(SELECT 1 FROM approval_uses WHERE approval_id=?1)", [&reference.id], |r| r.get(0))?;
                    Some((grant, revoked, consumed))
                }
                _ => None,
            },
            dependencies: if tx.query_row("PRAGMA user_version", [], |r| r.get::<_, u32>(0))? >= 30 {
                satisfaction::satisfied_edges_on(&tx, task.as_str(), budget)?.map(|edges| edges.into_iter().map(|edge| edge.input()).collect())
            } else { Some(Vec::new()) },
        };
        tx.commit()?;
        Ok(rows)
    }
    /// The approved occurrence bound to `id`, its signed revision and delivery.
    pub(crate) fn routine_rows(&mut self, id: &OperationId, at: Option<u64>) -> Result<RoutineRows> {
        let tx = self.connection.transaction()?;
        fenced(&tx, at)?;
        let (definition, occurrence) = routines::selected(&tx, id)?;
        let delivery = delivery::delivery(&tx, id)?;
        tx.commit()?;
        Ok(RoutineRows { definition, occurrence, delivery })
    }
}

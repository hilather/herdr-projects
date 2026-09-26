//! Shadow readers for the controller hot path. The acted-on result stays `read_snapshot`.
use super::{
    Result, Snapshot, SqliteStore, StoreError, approvals, budget, capabilities, control,
    controlled, delivery, head, integer, read_attempts_with_budget, read_budget,
    read_operations_matching_with_budget, read_tasks_with_budget, reservations, satisfaction,
    scheduler,
};
use crate::domain::*;
use crate::operations::{Delivery, DeliveryState};
use rusqlite::Connection;
use std::sync::Mutex;

/// Production stays on the snapshot reader. A later change is what may select targeted reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotPathRead {
    Snapshot,
    Targeted,
}

pub const HOT_PATH_READ: HotPathRead = HotPathRead::Snapshot;

pub fn hot_path_uses_snapshot() -> bool {
    matches!(HOT_PATH_READ, HotPathRead::Snapshot)
}

static MISMATCHES: Mutex<u64> = Mutex::new(0);

pub fn targeted_mismatch_count() -> u64 {
    *MISMATCHES.lock().unwrap_or_else(|error| error.into_inner())
}

fn note_mismatch(added: u64) {
    if added == 0 {
        return;
    }
    let mut count = MISMATCHES.lock().unwrap_or_else(|error| error.into_inner());
    *count = count.saturating_add(added);
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    id: String,
    kind: String,
    mode: &'static str,
    revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AttemptResource {
    id: String,
    task: String,
    state: String,
    reservation: String,
    operation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DeliveryView {
    state: String,
    revision: u64,
    attempts: u32,
    epoch: u64,
    next_due_ms: i64,
    lease_until_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ClaimInput {
    id: String,
    kind: String,
    payload: serde_json::Value,
    delivery: Option<DeliveryView>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Decision {
    schema: u32,
    head: u64,
    /// Queued tasks with no blocker, in scheduler order.
    ready: Vec<String>,
    /// Queued tasks that are not ready, in the same order.
    blocked: Vec<(String, Vec<String>)>,
    candidates: Vec<Candidate>,
    resources: Vec<AttemptResource>,
    claims: Vec<ClaimInput>,
    events_since: Vec<Event>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Cancelled,
    Deadline,
    Corrupt,
    Other,
}

fn class_of(error: &StoreError) -> Class {
    match error {
        StoreError::Cancelled => Class::Cancelled,
        StoreError::Deadline => Class::Deadline,
        StoreError::Corrupt(_) => Class::Corrupt,
        _ => Class::Other,
    }
}

fn is_abort(error: &StoreError) -> bool {
    matches!(error, StoreError::Cancelled | StoreError::Deadline)
}

fn delivery_name(state: DeliveryState) -> &'static str {
    match state {
        DeliveryState::Pending => "pending",
        DeliveryState::Claimed => "claimed",
        DeliveryState::Ambiguous => "ambiguous",
        DeliveryState::Confirmed => "confirmed",
        DeliveryState::PermanentFailure => "permanent_failure",
    }
}

fn delivery_view(delivery: &Delivery) -> DeliveryView {
    DeliveryView {
        state: delivery_name(delivery.state).to_string(),
        revision: delivery.revision,
        attempts: delivery.attempts,
        epoch: delivery.epoch,
        next_due_ms: delivery.next_due_ms,
        lease_until_ms: delivery.lease_until_ms,
    }
}

fn check(budget: Option<&read_budget::ReadBudget>) -> Result<()> {
    if let Some(budget) = budget {
        budget.check()?;
    }
    Ok(())
}

/// Events strictly after `since`. Sequence 0 is the origin, so the shadow pass sees the same log as `read_snapshot`.
pub(super) fn events_since(
    db: &Connection,
    since: u64,
    budget: Option<&read_budget::ReadBudget>,
) -> Result<Vec<Event>> {
    check(budget)?;
    let mut stmt = db.prepare("SELECT sequence,kind,entity,revision,payload_version,payload FROM events WHERE sequence > ?1 ORDER BY sequence")?;
    let mut rows = stmt.query([integer(since)?])?;
    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        if let Some(budget) = budget {
            budget.row(row, &[(5, 1)])?;
        } else {
            check(budget)?;
        }
        let (sequence, kind, entity, revision, payload_version, payload) = (
            row.get::<_, u64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, u64>(3)?,
            row.get::<_, u32>(4)?,
            row.get::<_, String>(5)?,
        );
        result.push(Event {
            sequence,
            kind,
            entity,
            revision,
            payload_version,
            payload: serde_json::from_str(&payload)
                .map_err(|error| StoreError::Corrupt(error.to_string()))?,
        });
    }
    Ok(result)
}

fn resources_of(attempts: &[Attempt], inputs: &[AttemptInputRecord]) -> Vec<AttemptResource> {
    attempts
        .iter()
        .filter(|attempt| attempt.retains_capacity())
        .map(|attempt| AttemptResource {
            id: attempt.id.as_str().to_string(),
            task: attempt.task.as_str().to_string(),
            state: attempt.state.as_str().to_string(),
            reservation: attempt.reservation.clone(),
            operation: inputs
                .iter()
                .find(|input| input.attempt == attempt.id)
                .map(|input| input.operation.as_str().to_string()),
        })
        .collect()
}

fn claims_of(operations: &[Operation], deliveries: &[Delivery]) -> Vec<ClaimInput> {
    operations
        .iter()
        .map(|operation| ClaimInput {
            id: operation.id.as_str().to_string(),
            kind: operation.kind.clone(),
            payload: operation.payload.clone(),
            delivery: deliveries
                .iter()
                .find(|delivery| delivery.operation == operation.id)
                .map(delivery_view),
        })
        .collect()
}

fn event_exists(events: &[Event], entity: &str, kind: &str) -> bool {
    events
        .iter()
        .any(|event| event.entity == entity && event.kind == kind)
}

fn event_version_two(events: &[Event], entity: &str, kinds: &[&str]) -> bool {
    events.iter().any(|event| {
        event.entity == entity
            && kinds.contains(&event.kind.as_str())
            && event
                .payload
                .get("version")
                .and_then(|value| value.as_u64())
                == Some(2)
    })
}

fn launch_glob(events: &[Event], entity: &str) -> bool {
    events
        .iter()
        .any(|event| event.entity == entity && event.kind.starts_with("runtime.launch_"))
}

fn delivery_for<'a>(deliveries: &'a [Delivery], operation: &OperationId) -> Option<&'a Delivery> {
    deliveries
        .iter()
        .find(|delivery| &delivery.operation == operation)
}

fn dangling(deliveries: &[Delivery], operations: &[Operation]) -> bool {
    deliveries.iter().any(|delivery| {
        matches!(
            delivery.state,
            DeliveryState::Pending | DeliveryState::Ambiguous
        ) && !operations
            .iter()
            .any(|operation| operation.id == delivery.operation)
    })
}

fn candidates_from(loaded: &Loaded, now: i64, include_launches: bool) -> Result<Vec<Candidate>> {
    let schema = loaded.schema;
    let tasks = loaded.tasks.as_slice();
    let attempts = loaded.attempts.as_slice();
    let operations = loaded.operations.as_slice();
    let deliveries = loaded.deliveries.as_slice();
    let events = loaded.events.as_slice();
    let cancellations = loaded.cancellations.as_slice();
    let inputs = loaded.inputs.as_slice();
    let control = loaded.control.as_ref();
    if dangling(deliveries, operations) {
        return Err(StoreError::Corrupt("unresolved dangling delivery".into()));
    }
    let mut candidates = Vec::new();
    for delivery in deliveries {
        let Some(operation) = operations
            .iter()
            .find(|operation| operation.id == delivery.operation)
        else {
            continue;
        };
        let pending_effect = delivery.state == DeliveryState::Pending
            && delivery.next_due_ms <= now
            && matches!(
                operation.kind.as_str(),
                "runtime.notification" | "runtime.finalization" | "runtime.worker_brief"
            );
        let ambiguous_finalization =
            delivery.state == DeliveryState::Ambiguous && operation.kind == "runtime.finalization";
        if pending_effect || ambiguous_finalization {
            candidates.push(Candidate {
                id: operation.id.as_str().to_string(),
                kind: operation.kind.clone(),
                mode: if delivery.state == DeliveryState::Ambiguous {
                    "observe"
                } else {
                    "deliver"
                },
                revision: delivery.revision,
            });
        }
    }
    if schema >= 11 {
        for attempt in attempts.iter().filter(|attempt| attempt.retains_capacity()) {
            let Some(input) = inputs.iter().find(|input| input.attempt == attempt.id) else {
                continue;
            };
            let operation_id = input.operation.as_str();
            let cancelled = cancellations
                .iter()
                .any(|cancellation| cancellation.attempt == attempt.id);
            let started = event_version_two(
                events,
                operation_id,
                &["runtime.launch_started", "runtime.launch_target"],
            );
            let recovery = attempt.state == AttemptState::Reserved
                && event_exists(events, operation_id, "runtime.worktrees_creation")
                && !launch_glob(events, operation_id)
                && (cancelled
                    || delivery_for(deliveries, &input.operation).is_some_and(|delivery| {
                        delivery.lease_until_ms.is_some_and(|lease| lease <= now)
                            || matches!(
                                delivery.state,
                                DeliveryState::Ambiguous | DeliveryState::PermanentFailure
                            )
                    }));
            if !started && !recovery {
                continue;
            }
            let brief_pending = attempt.state == AttemptState::Launching
                && !cancelled
                && !operations.iter().any(|operation| {
                    operation.kind == "runtime.worker_brief"
                        && operation
                            .payload
                            .get("attempt")
                            .and_then(|value| value.as_str())
                            == Some(attempt.id.as_str())
                });
            if brief_pending {
                candidates.push(Candidate {
                    id: format!("prepare-brief-{}", attempt.id.as_str()),
                    kind: "runtime.worker_brief_prepare".into(),
                    mode: "deliver",
                    revision: attempt.revision,
                });
            }
            candidates.push(Candidate {
                id: format!("terminate-{}", attempt.id.as_str()),
                kind: "runtime.worker_termination".into(),
                mode: "observe",
                revision: attempt.revision,
            });
        }
        for input in inputs {
            let Some(operation) = operations.iter().find(|operation| {
                operation.id == input.operation && operation.kind == "runtime.launch"
            }) else {
                continue;
            };
            let Some(attempt) = attempts.iter().find(|attempt| attempt.id == input.attempt) else {
                continue;
            };
            let Some(delivery) = delivery_for(deliveries, &operation.id) else {
                continue;
            };
            if delivery.attempts != 1
                || delivery.state == DeliveryState::Confirmed
                || attempt.state != AttemptState::Reserved
                || !attempt.retains_capacity()
            {
                continue;
            }
            let entity = operation.id.as_str();
            let created = event_exists(events, entity, "runtime.launch_creation");
            let targeted = event_exists(events, entity, "runtime.launch_target");
            let released = event_exists(events, entity, "runtime.launch_release");
            let started = event_exists(events, entity, "runtime.launch_started");
            if created && (!targeted || (released && !started)) {
                candidates.push(Candidate {
                    id: operation.id.as_str().to_string(),
                    kind: operation.kind.clone(),
                    mode: "observe",
                    revision: delivery.revision,
                });
            }
        }
    }
    if include_launches
        && schema >= 13
        && control.is_some_and(|control| {
            control.state == ProjectState::Active && !control.reconciliation_required
        })
    {
        for input in inputs {
            let Some(operation) = operations.iter().find(|operation| {
                operation.id == input.operation && operation.kind == "runtime.launch"
            }) else {
                continue;
            };
            let Some(attempt) = attempts.iter().find(|attempt| attempt.id == input.attempt) else {
                continue;
            };
            let Some(task) = tasks.iter().find(|task| task.id == attempt.task) else {
                continue;
            };
            let Some(delivery) = delivery_for(deliveries, &operation.id) else {
                continue;
            };
            if attempt.state != AttemptState::Reserved
                || !attempt.retains_capacity()
                || task.active_attempt.as_ref() != Some(&attempt.id)
            {
                continue;
            }
            if cancellations
                .iter()
                .any(|cancellation| cancellation.attempt == attempt.id)
            {
                continue;
            }
            let due = delivery.state == DeliveryState::Pending
                && delivery.attempts == 0
                && delivery.epoch == 0
                && delivery.next_due_ms <= now;
            let claimed = delivery.state == DeliveryState::Claimed
                && delivery.attempts == 1
                && delivery.lease_until_ms.is_some_and(|lease| lease > now);
            if !due && !claimed {
                continue;
            }
            candidates.retain(|candidate| candidate.id != operation.id.as_str());
            candidates.push(Candidate {
                id: operation.id.as_str().to_string(),
                kind: operation.kind.clone(),
                mode: "deliver",
                revision: delivery.revision,
            });
        }
    }
    candidates.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(candidates)
}

fn queue_entries(
    db: &Connection,
    loaded: &Loaded,
    now: i64,
    budget: Option<&read_budget::ReadBudget>,
) -> Result<Vec<(String, Vec<String>)>> {
    let schema = loaded.schema;
    let tasks = loaded.tasks.as_slice();
    let attempts = loaded.attempts.as_slice();
    let scheduler = loaded.scheduler.as_ref();
    let control = loaded.control.as_ref();
    let approvals = loaded.approvals.as_slice();
    check(budget)?;
    let Some(scheduler) = scheduler else {
        return Ok(Vec::new());
    };
    let Some(control) = control else {
        return Ok(Vec::new());
    };
    let admission_on = satisfaction::admission_enabled(db)?;
    let retained = attempts
        .iter()
        .filter(|attempt| attempt.retains_capacity())
        .count();
    let available = (scheduler.policy.max_active_workers as usize).saturating_sub(retained);
    let budget_blockers = budget::report(db, false)?.blockers;
    let mut entries = Vec::new();
    for record in &scheduler.queue {
        check(budget)?;
        let task = tasks
            .iter()
            .find(|task| task.id == record.task)
            .ok_or(StoreError::Conflict)?;
        let mut blockers = Vec::new();
        if task.state != TaskState::Queued {
            blockers.push(format!("task_state:{:?}", task.state));
        }
        if control.state != ProjectState::Active || control.reconciliation_required {
            blockers.push("project_not_admitted".into());
        }
        if available == 0 {
            blockers.push("capacity_full".into());
        }
        if attempts
            .iter()
            .any(|attempt| attempt.task == task.id && attempt.retains_capacity())
        {
            blockers.push("task_capacity_retained".into());
        }
        if attempts
            .iter()
            .filter(|attempt| attempt.task == task.id)
            .count()
            >= scheduler.policy.max_attempts_per_task as usize
        {
            blockers.push("attempt_limit".into());
        }
        for edge in &record.dependencies {
            let predecessor = tasks
                .iter()
                .find(|task| task.id == edge.predecessor)
                .ok_or(StoreError::Conflict)?;
            if let Some(blocker) = satisfaction::dependency_blocker(
                db,
                task.id.as_str(),
                predecessor,
                edge.requirement,
                admission_on,
            )? {
                blockers.push(blocker);
            }
        }
        if schema >= 33 {
            let blocker = capabilities::queue_capability_blocker(db, task.id.as_str(), now)?;
            if let Some(blocker) = blocker {
                blockers.push(blocker);
            }
        }
        blockers.extend(budget_blockers.iter().cloned());
        let signed = schema >= 13
            && approvals.iter().any(|record| {
                record.consumed.is_none()
                    && record.grant.scope.class == ApprovalClass::RuntimeLaunch
                    && record.grant.scope.task == task.id
            });
        let retained_launch = attempts.iter().any(|attempt| {
            attempt.task == task.id
                && attempt.retains_capacity()
                && matches!(
                    attempt.state,
                    AttemptState::Reserved
                        | AttemptState::Launching
                        | AttemptState::Running
                        | AttemptState::AwaitingInput
                )
        });
        if !signed {
            blockers.push("owner_signature_not_scheduled".into());
        }
        if !retained_launch {
            blockers.push("launch_reserve_not_scheduled".into());
            blockers.push("controller_requires_reserved_attempt".into());
        }
        let age = now.saturating_sub(record.enqueued_unix_ms).max(0) / 60_000;
        let score = age + i64::from(record.priority);
        entries.push((
            record.enqueue_sequence,
            score,
            task.id.as_str().to_string(),
            blockers,
        ));
    }
    entries.sort_by(|left, right| {
        right
            .1
            .cmp(&left.1)
            .then(left.0.cmp(&right.0))
            .then(left.2.cmp(&right.2))
    });
    Ok(entries
        .into_iter()
        .map(|(_, _, task, blockers)| (task, blockers))
        .collect())
}

struct Loaded {
    schema: u32,
    head: u64,
    tasks: Vec<Task>,
    attempts: Vec<Attempt>,
    operations: Vec<Operation>,
    deliveries: Vec<Delivery>,
    events: Vec<Event>,
    cancellations: Vec<CancellationRequest>,
    inputs: Vec<AttemptInputRecord>,
    control: Option<ProjectControl>,
    scheduler: Option<SchedulerSnapshot>,
    approvals: Vec<ApprovalRecord>,
}

fn load_targeted(
    db: &Connection,
    since: u64,
    budget: Option<&read_budget::ReadBudget>,
) -> Result<Loaded> {
    check(budget)?;
    let schema: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let head = head(db)?;
    let tasks = read_tasks_with_budget(db, budget)?;
    // Attempt limits count terminated rows too, so this is the same attempt read as the snapshot.
    let attempts = read_attempts_with_budget(db, budget)?;
    let operations = read_operations_matching_with_budget(db, None, budget)?;
    let deliveries = if schema >= 3 {
        delivery::read_all_with_budget(db, budget)?
    } else {
        Vec::new()
    };
    let events = events_since(db, since, budget)?;
    let cancellations = if schema >= 11 {
        reservations::read_cancellations_with_budget(db, budget)?
    } else {
        Vec::new()
    };
    let inputs = if schema >= 11 {
        reservations::read_inputs_with_budget(db, budget)?
    } else {
        Vec::new()
    };
    let control = if schema >= 7 {
        Some(control::read_with_budget(db, budget)?)
    } else {
        None
    };
    let scheduler = if schema >= 10 {
        Some(scheduler::read_with_tasks(db, &tasks, budget)?)
    } else {
        None
    };
    let approvals = if schema >= 13 {
        approvals::read_all_with(db, &inputs, budget)?
    } else {
        Vec::new()
    };
    Ok(Loaded {
        schema,
        head,
        tasks,
        attempts,
        operations,
        deliveries,
        events,
        cancellations,
        inputs,
        control,
        scheduler,
        approvals,
    })
}

fn decide(
    db: &Connection,
    loaded: &Loaded,
    now: i64,
    include_launches: bool,
    budget: Option<&read_budget::ReadBudget>,
) -> Result<Decision> {
    check(budget)?;
    let entries = queue_entries(db, loaded, now, budget)?;
    let ready = entries
        .iter()
        .filter(|(_, blockers)| blockers.is_empty())
        .map(|(task, _)| task.clone())
        .collect();
    let blocked = entries
        .into_iter()
        .filter(|(_, blockers)| !blockers.is_empty())
        .collect();
    let candidates = candidates_from(loaded, now, include_launches)?;
    Ok(Decision {
        schema: loaded.schema,
        head: loaded.head,
        ready,
        blocked,
        candidates,
        resources: resources_of(&loaded.attempts, &loaded.inputs),
        claims: claims_of(&loaded.operations, &loaded.deliveries),
        events_since: loaded.events.clone(),
    })
}

fn loaded_from_snapshot(snapshot: &Snapshot) -> Loaded {
    Loaded {
        schema: snapshot.schema_version,
        head: snapshot.head,
        tasks: snapshot.tasks.clone(),
        attempts: snapshot.attempts.clone(),
        operations: snapshot.operations.clone(),
        deliveries: snapshot.deliveries.clone(),
        events: snapshot.events.clone(),
        cancellations: snapshot.cancellations.clone(),
        inputs: snapshot.attempt_inputs.clone(),
        control: snapshot.control.clone(),
        scheduler: snapshot.scheduler.clone(),
        approvals: snapshot.approvals.clone(),
    }
}

enum Seen {
    Value(Decision),
    Failed(Class),
}

impl PartialEq for Seen {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Value(left), Self::Value(right)) => left == right,
            (Self::Failed(left), Self::Failed(right)) => left == right,
            _ => false,
        }
    }
}

fn split_abort<T>(result: Result<T>) -> std::result::Result<Result<T>, StoreError> {
    match result {
        Err(error) if is_abort(&error) => Err(error),
        other => Ok(other),
    }
}

fn restart(budget: Option<&read_budget::ReadBudget>) {
    if let Some(budget) = budget {
        budget.restart();
    }
}

fn seen(result: &Result<Decision>) -> Seen {
    match result {
        Ok(decision) => Seen::Value(decision.clone()),
        Err(error) => Seen::Failed(class_of(error)),
    }
}

pub struct ShadowedRead {
    pub acted_on: std::result::Result<Snapshot, StoreError>,
    pub mismatches_added: u64,
}

impl SqliteStore {
    fn project_snapshot(
        &self,
        snapshot: &Snapshot,
        now: i64,
        include_launches: bool,
        budget: Option<&read_budget::ReadBudget>,
    ) -> Result<Decision> {
        decide(
            &self.connection,
            &loaded_from_snapshot(snapshot),
            now,
            include_launches,
            budget,
        )
    }

    #[cfg(test)]
    pub(crate) fn blocked_for_test(
        &self,
        snapshot: &Snapshot,
        now: i64,
    ) -> Result<Vec<(String, Vec<String>)>> {
        Ok(self.project_snapshot(snapshot, now, true, None)?.blocked)
    }

    #[cfg(test)]
    pub(crate) fn candidates_for_test(
        &self,
        snapshot: &Snapshot,
        now: i64,
    ) -> Result<Vec<(String, String)>> {
        Ok(self
            .project_snapshot(snapshot, now, true, None)?
            .candidates
            .into_iter()
            .map(|candidate| (candidate.id, candidate.mode.to_string()))
            .collect())
    }

    #[cfg(test)]
    pub(crate) fn claim_ids_for_test(&self, snapshot: &Snapshot, now: i64) -> Result<Vec<String>> {
        Ok(self
            .project_snapshot(snapshot, now, true, None)?
            .claims
            .into_iter()
            .map(|claim| claim.id)
            .collect())
    }

    fn project_targeted(
        &self,
        now: i64,
        include_launches: bool,
        budget: Option<&read_budget::ReadBudget>,
    ) -> Result<Decision> {
        let loaded = load_targeted(&self.connection, 0, budget)?;
        decide(&self.connection, &loaded, now, include_launches, budget)
    }

    /// Compare targeted readers with `snapshot` and record a mismatch. Cancelled and deadline results abort
    /// without another reader. The caller still acts on `snapshot`.
    pub fn shadow_against_snapshot(
        &mut self,
        snapshot: &Snapshot,
        now: i64,
        include_launches: bool,
    ) -> Result<u64> {
        self.shadow_against(snapshot, now, include_launches, None)
    }

    fn shadow_against(
        &mut self,
        snapshot: &Snapshot,
        now: i64,
        include_launches: bool,
        budget: Option<&read_budget::ReadBudget>,
    ) -> Result<u64> {
        check(budget)?;
        restart(budget);
        let left = split_abort(self.project_snapshot(snapshot, now, include_launches, budget))?;
        check(budget)?;
        restart(budget);
        let right = split_abort(self.project_targeted(now, include_launches, budget))?;
        let added = u64::from(seen(&left) != seen(&right));
        note_mismatch(added);
        Ok(added)
    }

    /// Read the snapshot the controller acts on, then shadow the targeted readers unless the caller already aborted.
    pub fn shadow_compare(
        &mut self,
        now: i64,
        include_launches: bool,
        control: Option<controlled::ReadControl>,
    ) -> Result<ShadowedRead> {
        let budget = control.map(read_budget::ReadBudget::new);
        let budget = budget.as_ref();
        check(budget)?;
        let snapshot = split_abort(self.read_snapshot_with_budget(None, budget))?;
        check(budget)?;
        let added = match &snapshot {
            Ok(snapshot) => self.shadow_against(snapshot, now, include_launches, budget)?,
            Err(error) => {
                restart(budget);
                let targeted = split_abort(self.project_targeted(now, include_launches, budget))?;
                let added = u64::from(
                    class_of(error)
                        != match &targeted {
                            Err(failure) => class_of(failure),
                            Ok(_) => Class::Other,
                        },
                );
                note_mismatch(added);
                added
            }
        };
        Ok(ShadowedRead {
            acted_on: snapshot,
            mismatches_added: added,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{HOT_PATH_READ, HotPathRead, hot_path_uses_snapshot, targeted_mismatch_count};
    use crate::domain::*;
    use crate::runner::Cancellation;
    use crate::store::{SCHEMA, SqliteStore, StoreError, controlled};
    use std::time::{Duration, Instant};

    fn open() -> (tempfile::TempDir, SqliteStore) {
        let temp = tempfile::tempdir().unwrap();
        let db = SqliteStore::create(&temp.path().join("state.db")).unwrap();
        (temp, db)
    }

    fn task(index: usize) -> Task {
        Task {
            id: TaskId::new(format!("t-{index:04}")).unwrap(),
            revision: 1,
            state: TaskState::Draft,
            title: format!("task {index}"),
            active_attempt: None,
        }
    }

    /// Same shape as the factory harness history: 996 tasks and two queue writes, 1000 events.
    fn seed_history(db: &mut SqliteStore, now: i64) {
        let mutations = (0..996)
            .map(|index| Mutation::Task {
                expected: None,
                next: task(index),
            })
            .collect();
        let mut head = db
            .commit(Commit {
                expected_head: 0,
                mutations,
            })
            .unwrap();
        head = db
            .queue_task(
                &TaskId::new("t-0000").unwrap(),
                1,
                head,
                &QueueRequest {
                    priority: 0,
                    dependencies: Vec::new(),
                },
                now,
            )
            .unwrap();
        db.queue_task(
            &TaskId::new("t-0001").unwrap(),
            1,
            head,
            &QueueRequest {
                priority: 0,
                dependencies: vec![Dependency {
                    predecessor: TaskId::new("t-0000").unwrap(),
                    requirement: DependencyRequirement::VerifiedResult,
                }],
            },
            now,
        )
        .unwrap();
    }

    #[test]
    fn targeted_shadow_matches_the_harness_history_fixture_including_a_corrupt_payload() {
        let (temp, mut db) = open();
        let now = 0;
        seed_history(&mut db, now);
        let snapshot = db.read_snapshot(None).unwrap();
        assert_eq!(snapshot.schema_version, SCHEMA);
        assert_eq!(snapshot.events.len(), 1000);
        assert_eq!(snapshot.head, 1000);
        let blocked = db.blocked_for_test(&snapshot, now).unwrap();
        let report = db.queue_report(now).unwrap();
        let expected = report
            .entries
            .iter()
            .map(|entry| (entry.task.as_str().to_string(), entry.blockers.clone()))
            .collect::<Vec<_>>();
        assert_eq!(blocked, expected);
        assert!(blocked.iter().all(|(_, blockers)| !blockers.is_empty()));
        assert_eq!(blocked[0].0, "t-0000");
        assert_eq!(blocked[1].0, "t-0001");
        assert!(
            blocked[1].1.iter().any(|blocker| blocker
                == "verified_dependency_evidence_unavailable:t-0000:verified_result")
        );
        let before = targeted_mismatch_count();
        let compared = db.shadow_compare(now, true, None).unwrap();
        assert!(compared.acted_on.is_ok());
        assert_eq!(compared.mismatches_added, 0);
        assert_eq!(targeted_mismatch_count(), before);

        let raw = rusqlite::Connection::open(temp.path().join("state.db")).unwrap();
        raw.execute_batch("PRAGMA ignore_check_constraints=ON")
            .unwrap();
        assert_eq!(
            raw.execute("UPDATE events SET payload='{' WHERE sequence=1", [])
                .unwrap(),
            1
        );
        drop(raw);
        // Stay on the open store. Re-opening runs quick_check, which is a different failure than payload decode.
        let compared = db.shadow_compare(now, true, None).unwrap();
        assert!(matches!(compared.acted_on, Err(StoreError::Corrupt(_))));
        assert_eq!(
            compared.mismatches_added, 0,
            "corrupt payload must fail both readers"
        );
        assert_eq!(targeted_mismatch_count(), before);
    }

    #[test]
    fn targeted_shadow_deadline_and_cancellation_abort_without_another_read() {
        let (temp, mut db) = open();
        seed_history(&mut db, 0);
        let raw = rusqlite::Connection::open(temp.path().join("state.db")).unwrap();
        raw.execute_batch("PRAGMA ignore_check_constraints=ON")
            .unwrap();
        raw.execute("UPDATE events SET payload='{' WHERE sequence=1", [])
            .unwrap();
        drop(raw);
        let expired = controlled::ReadControl::new(Instant::now(), Cancellation::default());
        assert!(matches!(
            db.shadow_compare(0, true, Some(expired)),
            Err(StoreError::Deadline)
        ));
        let control = controlled::ReadControl::new(
            Instant::now() + Duration::from_secs(5),
            Cancellation::default(),
        );
        control.cancellation().cancel();
        // A corrupt event is present. Cancellation must win and skip the shadow decode.
        assert!(matches!(
            db.shadow_compare(0, true, Some(control)),
            Err(StoreError::Cancelled)
        ));
    }

    #[test]
    fn targeted_notification_candidate_matches_the_snapshot_and_a_future_due_stays_blocked() {
        let (_temp, mut db) = open();
        db.commit(Commit {
            expected_head: 0,
            mutations: vec![Mutation::Task {
                expected: None,
                next: task(0),
            }],
        })
        .unwrap();
        let operation = |id: &str, due: i64| Operation {
            id: OperationId::new(id).unwrap(),
            task: Some(TaskId::new("t-0000").unwrap()),
            kind: "runtime.notification".into(),
            target: "coord".into(),
            payload_version: 1,
            payload: serde_json::json!({"inbox":[]}),
            expected_revision: 1,
            due_unix_ms: due,
            idempotency_key: id.into(),
        };
        db.commit(Commit {
            expected_head: 1,
            mutations: vec![
                Mutation::Enqueue(operation("notify-now", 0)),
                Mutation::Enqueue(operation("notify-later", 50_000)),
            ],
        })
        .unwrap();
        let compared = db.shadow_compare(0, true, None).unwrap();
        assert_eq!(compared.mismatches_added, 0);
        let snapshot = compared.acted_on.unwrap();
        assert_eq!(
            db.candidates_for_test(&snapshot, 0).unwrap(),
            vec![("notify-now".to_string(), "deliver".to_string())]
        );
        assert!(
            db.claim_ids_for_test(&snapshot, 0)
                .unwrap()
                .iter()
                .any(|id| id == "notify-later")
        );
    }

    #[test]
    fn targeted_hot_path_still_calls_read_snapshot() {
        assert!(hot_path_uses_snapshot());
        assert_eq!(HOT_PATH_READ, HotPathRead::Snapshot);
        let source = include_str!("../canonical_controller.rs");
        assert!(source.contains("const PREPARED_LAUNCH_DISPATCH_ENABLED: bool = true;"));
        assert!(!source.contains("HotPathRead::Targeted"));
        let poll = source
            .split("pub fn poll(")
            .nth(1)
            .expect("poll")
            .split("pub fn poll_queued")
            .next()
            .expect("poll body");
        assert!(poll.contains("read_snapshot(None)"), "{poll}");
        assert!(poll.contains("hot_path_uses_snapshot()"), "{poll}");
        assert!(poll.contains("shadow_against_snapshot"), "{poll}");
    }
}

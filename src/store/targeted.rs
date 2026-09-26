//! Targeted rows for the controller hot path. `read_snapshot` stays for admin and diagnostics.
use super::{
    Result, Snapshot, SqliteStore, StoreError, approvals, control, controlled,
    controller_hint::EffectMode, delivery, read_attempts_with_budget, read_budget,
    read_events_with_budget, read_operations_matching_with_budget, read_tasks_with_budget,
    reservations, scheduler,
};
use crate::domain::*;
use crate::operations::{Delivery, DeliveryState};
use rusqlite::Connection;
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotPathRead {
    Snapshot,
    Targeted,
}

pub const HOT_PATH_READ: HotPathRead = HotPathRead::Targeted;

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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Cancelled,
    Deadline,
    Corrupt,
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Seen {
    Ready,
    Failed(Class),
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

fn seen_result<T>(result: &Result<T>) -> Seen {
    match result {
        Ok(_) => Seen::Ready,
        Err(error) => Seen::Failed(class_of(error)),
    }
}

fn check(budget: Option<&read_budget::ReadBudget>) -> Result<()> {
    if let Some(budget) = budget {
        budget.check()?;
    }
    Ok(())
}

fn split_abort<T>(result: Result<T>) -> std::result::Result<Result<T>, StoreError> {
    match result {
        Err(error) if is_abort(&error) => Err(error),
        other => Ok(other),
    }
}

fn retained(attempts: &[Attempt]) -> Vec<Attempt> {
    // Terminated attempts do not retain capacity, so the targeted row set leaves them out.
    attempts
        .iter()
        .filter(|attempt| attempt.retains_capacity())
        .cloned()
        .collect()
}

pub(crate) fn due_effects(
    operations: &[Operation],
    deliveries: &[Delivery],
    now: i64,
    include_launches: bool,
) -> Vec<(String, EffectMode)> {
    let mut effects = Vec::new();
    for delivery in deliveries {
        let Some(operation) = operations
            .iter()
            .find(|operation| operation.id == delivery.operation)
        else {
            continue;
        };
        if operation.kind == "runtime.launch" && !include_launches {
            continue;
        }
        let due = delivery.state == DeliveryState::Pending
            && delivery.next_due_ms <= now
            && matches!(
                operation.kind.as_str(),
                "runtime.notification"
                    | "runtime.finalization"
                    | "runtime.worker_brief"
                    | "runtime.launch"
            );
        let observe =
            delivery.state == DeliveryState::Ambiguous && operation.kind == "runtime.finalization";
        if due || observe {
            effects.push((
                operation.id.as_str().to_string(),
                if observe {
                    EffectMode::Observe
                } else {
                    EffectMode::Deliver
                },
            ));
        }
    }
    effects.sort_by(|left, right| left.0.cmp(&right.0));
    effects
}

fn blocker_labels(entries: Vec<QueueEntry>) -> Vec<(String, Vec<String>)> {
    entries
        .into_iter()
        .map(|entry| (entry.task.as_str().to_string(), entry.blockers))
        .collect()
}

fn blockers_of(
    db: &Connection,
    now: i64,
    tasks: &[Task],
    attempts: &[Attempt],
    scheduler: Option<&SchedulerSnapshot>,
    control: Option<&ProjectControl>,
    approvals: &[ApprovalRecord],
) -> Result<Vec<(String, Vec<String>)>> {
    let Some(scheduler) = scheduler else {
        return Ok(Vec::new());
    };
    let Some(control) = control else {
        return Ok(Vec::new());
    };
    Ok(blocker_labels(scheduler::queue_blockers(
        db, now, tasks, attempts, scheduler, control, approvals,
    )?))
}

pub struct ShadowedRead {
    pub acted_on: std::result::Result<Snapshot, StoreError>,
    pub mismatches_added: u64,
}

impl SqliteStore {
    /// Targeted rows and the shared blocker loop, one transaction, compared with `snapshot`.
    fn same_as_snapshot(
        &mut self,
        snapshot: &Snapshot,
        now: i64,
        include_launches: bool,
        budget: Option<&read_budget::ReadBudget>,
    ) -> Result<bool> {
        check(budget)?;
        let tx = self.connection.transaction()?;
        let tasks = read_tasks_with_budget(&tx, budget)?;
        let attempts = read_attempts_with_budget(&tx, budget)?;
        let operations = read_operations_matching_with_budget(&tx, None, budget)?;
        let deliveries = if snapshot.schema_version >= 3 {
            delivery::read_all_with_budget(&tx, budget)?
        } else {
            Vec::new()
        };
        let control = if snapshot.schema_version >= 7 {
            Some(control::read_with_budget(&tx, budget)?)
        } else {
            None
        };
        let scheduler_rows = if snapshot.schema_version >= 10 {
            Some(scheduler::read_with_tasks(&tx, &tasks, budget)?)
        } else {
            None
        };
        let inputs = if snapshot.schema_version >= 11 {
            reservations::read_inputs_with_budget(&tx, budget)?
        } else {
            Vec::new()
        };
        let approvals = if snapshot.schema_version >= 13 {
            approvals::read_all_with(&tx, &inputs, budget)?
        } else {
            Vec::new()
        };
        let claims_match = operations == snapshot.operations && deliveries == snapshot.deliveries;
        let candidates_match = due_effects(&operations, &deliveries, now, include_launches)
            == due_effects(
                &snapshot.operations,
                &snapshot.deliveries,
                now,
                include_launches,
            );
        let from_snapshot = blockers_of(
            &tx,
            now,
            &snapshot.tasks,
            &snapshot.attempts,
            snapshot.scheduler.as_ref(),
            snapshot.control.as_ref(),
            &snapshot.approvals,
        )?;
        let from_rows = blockers_of(
            &tx,
            now,
            &tasks,
            &attempts,
            scheduler_rows.as_ref(),
            control.as_ref(),
            &approvals,
        )?;
        Ok(retained(&attempts) == retained(&snapshot.attempts)
            && claims_match
            && candidates_match
            && from_snapshot == from_rows)
    }

    fn shadow_against(
        &mut self,
        snapshot: &Snapshot,
        now: i64,
        include_launches: bool,
        budget: Option<&read_budget::ReadBudget>,
    ) -> Result<u64> {
        check(budget)?;
        if let Some(budget) = budget {
            budget.restart();
        }
        let added = match self.same_as_snapshot(snapshot, now, include_launches, budget) {
            Ok(same) => u64::from(!same),
            Err(error) if is_abort(&error) => return Err(error),
            Err(_) => 1,
        };
        note_mismatch(added);
        Ok(added)
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

    /// Read the snapshot and shadow the targeted readers unless the caller already aborted.
    /// `acted_on` stays the snapshot result; the controller hot path uses `read_targeted_hot_path`.
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
                self.mismatch_for_failed_snapshot(error, now, include_launches, budget)?
            }
        };
        Ok(ShadowedRead {
            acted_on: snapshot,
            mismatches_added: added,
        })
    }

    fn mismatch_for_failed_snapshot(
        &mut self,
        error: &StoreError,
        now: i64,
        include_launches: bool,
        budget: Option<&read_budget::ReadBudget>,
    ) -> Result<u64> {
        if let Some(budget) = budget {
            budget.restart();
        }
        let targeted = if matches!(error, StoreError::Corrupt(_)) {
            // Same event decoder as read_snapshot, so a corrupt payload fails both.
            read_events_with_budget(&self.connection, budget).map(|_| ())
        } else {
            self.targeted_ready(now, include_launches, budget).map(|_| ())
        };
        if let Err(failure) = &targeted
            && is_abort(failure)
        {
            return match targeted {
                Err(failure) => Err(failure),
                Ok(()) => unreachable!(),
            };
        }
        let added = u64::from(Seen::Failed(class_of(error)) != seen_result(&targeted));
        note_mismatch(added);
        Ok(added)
    }

    /// Schema after the targeted row read the controller acts on.
    /// Later migration tables are absent on an older published schema.
    pub fn read_targeted_hot_path(&mut self, now: i64, include_launches: bool) -> Result<u32> {
        self.targeted_ready(now, include_launches, None)
    }

    /// Status row count. Uses the active-work page, which does not decode retired attempts.
    pub fn hot_path_rows_decoded(&mut self, now: i64) -> Result<u64> {
        let _ = now;
        self.hot_path_page_rows()
    }

    fn targeted_ready(
        &mut self,
        now: i64,
        include_launches: bool,
        budget: Option<&read_budget::ReadBudget>,
    ) -> Result<u32> {
        check(budget)?;
        let tx = self.connection.transaction()?;
        let schema: u32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        let tasks = read_tasks_with_budget(&tx, budget)?;
        let attempts = read_attempts_with_budget(&tx, budget)?;
        let operations = read_operations_matching_with_budget(&tx, None, budget)?;
        let deliveries = if schema >= 3 {
            delivery::read_all_with_budget(&tx, budget)?
        } else {
            Vec::new()
        };
        let control = if schema >= 7 {
            Some(control::read_with_budget(&tx, budget)?)
        } else {
            None
        };
        let scheduler_rows = if schema >= 10 {
            Some(scheduler::read_with_tasks(&tx, &tasks, budget)?)
        } else {
            None
        };
        let inputs = if schema >= 11 {
            reservations::read_inputs_with_budget(&tx, budget)?
        } else {
            Vec::new()
        };
        let approvals = if schema >= 13 {
            approvals::read_all_with(&tx, &inputs, budget)?
        } else {
            Vec::new()
        };
        let _ = (
            retained(&attempts),
            due_effects(&operations, &deliveries, now, include_launches),
        );
        blockers_of(
            &tx,
            now,
            &tasks,
            &attempts,
            scheduler_rows.as_ref(),
            control.as_ref(),
            &approvals,
        )?;
        Ok(schema)
    }

    #[cfg(test)]
    pub(crate) fn blockers_for_test(
        &mut self,
        snapshot: &Snapshot,
        now: i64,
    ) -> Result<Vec<(String, Vec<String>)>> {
        let Some(scheduler) = snapshot.scheduler.as_ref() else {
            return Ok(Vec::new());
        };
        let Some(control) = snapshot.control.as_ref() else {
            return Ok(Vec::new());
        };
        let tx = self.connection.transaction()?;
        Ok(blocker_labels(scheduler::queue_blockers(
            &tx,
            now,
            &snapshot.tasks,
            &snapshot.attempts,
            scheduler,
            control,
            &snapshot.approvals,
        )?))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        HOT_PATH_READ, HotPathRead, due_effects, hot_path_uses_snapshot, targeted_mismatch_count,
    };
    use crate::domain::*;
    use crate::runner::Cancellation;
    use crate::store::controller_hint::EffectMode;
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
        let blocked = db.blockers_for_test(&snapshot, now).unwrap();
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
        assert_eq!(db.read_targeted_hot_path(now, true).unwrap(), SCHEMA);
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
        let compared = db.shadow_compare(now, true, None).unwrap();
        assert!(matches!(compared.acted_on, Err(StoreError::Corrupt(_))));
        assert_eq!(
            compared.mismatches_added, 0,
            "corrupt payload must fail both readers"
        );
        assert_eq!(targeted_mismatch_count(), before);
    }

    #[test]
    fn targeted_success_against_a_non_corrupt_snapshot_failure_is_a_mismatch() {
        let (temp, mut db) = open();
        db.commit(Commit {
            expected_head: 0,
            mutations: vec![Mutation::Task {
                expected: None,
                next: task(0),
            }],
        })
        .unwrap();
        let raw = rusqlite::Connection::open(temp.path().join("state.db")).unwrap();
        let payload = format!("\"{}\"", "x".repeat(16 * 1024 * 1024));
        raw.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('dense','x',1,1,?1)", [payload]).unwrap();
        drop(raw);
        let control = controlled::ReadControl::new(
            Instant::now() + Duration::from_secs(5),
            Cancellation::default(),
        );
        let compared = db.shadow_compare(0, true, Some(control)).unwrap();
        assert!(
            matches!(compared.acted_on, Err(StoreError::Limit(_))),
            "{:?}",
            compared.acted_on
        );
        assert_eq!(compared.mismatches_added, 1);
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
            due_effects(&snapshot.operations, &snapshot.deliveries, 0, true),
            vec![("notify-now".to_string(), EffectMode::Deliver)]
        );
        assert!(
            snapshot
                .operations
                .iter()
                .any(|operation| operation.id.as_str() == "notify-later")
        );
    }

    #[test]
    fn targeted_hot_path_is_the_production_default() {
        assert!(
            !hot_path_uses_snapshot(),
            "shadow mode must not be the production default"
        );
        assert_eq!(HOT_PATH_READ, HotPathRead::Targeted);
        let source = include_str!("../canonical_controller.rs");
        assert!(source.contains("const PREPARED_LAUNCH_DISPATCH_ENABLED: bool = true;"));
        let poll = source
            .split("pub fn poll(")
            .nth(1)
            .expect("poll")
            .split("pub fn poll_queued")
            .next()
            .expect("poll body");
        assert!(!poll.contains("hot_path_uses_snapshot()"), "{poll}");
        let targeted = poll
            .split("HotPathRead::Targeted")
            .nth(1)
            .expect("targeted arm")
            .split("HotPathRead::Snapshot")
            .next()
            .expect("targeted arm");
        assert!(targeted.contains("read_targeted_hot_path"), "{targeted}");
        assert!(!targeted.contains("read_snapshot"), "{targeted}");
        assert!(targeted.contains("StoreError::Cancelled"), "{targeted}");
        assert!(targeted.contains("StoreError::Deadline"), "{targeted}");
        let snapshot_arm = poll
            .split("HotPathRead::Snapshot")
            .nth(1)
            .expect("snapshot arm");
        assert!(snapshot_arm.contains("read_snapshot(None)"), "{snapshot_arm}");
    }
}

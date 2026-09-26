//! One automatic reservation per wake. Does not launch.
use crate::domain::*;
use crate::launch_preparation::seal_admission_inputs;
use crate::store::{SqliteStore, StoreError};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::Path;

fn store_file(project: &Path) -> Result<std::path::PathBuf> {
    let path = project.join(".state/state.db");
    std::fs::canonicalize(&path).with_context(|| format!("project store missing at {}", path.display()))
}

fn open_store(project: &Path) -> Result<SqliteStore> {
    SqliteStore::open(&store_file(project)?).map_err(anyhow::Error::from)
}

/// Schema 30+ and `factory_admission=on`. A missing column or older store stays off.
/// This probe does not integrity-check: the off flag is the steady state, and a
/// full `open` on every wake would run before the existing hint path.
pub fn wake_enabled(project: &Path) -> bool {
    let Ok(path) = store_file(project) else { return false };
    let Ok(connection) = rusqlite::Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    ) else { return false };
    let Ok(version) = connection.query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0)) else { return false };
    if version < 30 { return false; }
    connection.query_row("SELECT factory_admission FROM project_control WHERE singleton=1", [], |row| row.get::<_, String>(0)).ok().as_deref() == Some("on")
}

fn placeholder_approval() -> VersionedReference {
    VersionedReference { id: "unsigned-launch".into(), revision: 1, digest: "0".repeat(64) }
}

enum Choice {
    /// Placeholder approval. The caller substitutes an installed grant before reserving.
    Ready(LaunchInputs),
    /// Prerequisites are present, but no retained profile can be sealed.
    NeedsAuthority { task: Task, head: u64 },
}

fn unused_binding(state: &Snapshot, task: &TaskId) -> Option<RuntimeBinding> {
    state.runtime_bindings.iter().find(|binding| {
        binding.task.as_ref() == Some(task)
            && binding.identity.pane_id.is_empty()
            && binding.identity.tab_id.is_empty()
            && binding.identity.machine.is_empty()
            && binding.identity.worktree_path.is_empty()
            && !state.ownership.iter().any(|owned| {
                owned.binding == binding.id && (owned.attempt.is_some() || owned.session.is_some() || owned.agent.is_some())
            })
    }).cloned()
}

fn select(db: &mut SqliteStore, project_store: &str, now: i64) -> Result<Option<Choice>> {
    let state = db.read_snapshot(None)?;
    let scheduler = state.scheduler.as_ref().context("scheduler missing")?;
    let control = state.control.as_ref().context("project control missing")?;
    if control.state != ProjectState::Active || control.reconciliation_required {
        return Ok(None);
    }
    let retained = state.attempts.iter().filter(|attempt| attempt.retains_capacity()).count();
    if retained >= scheduler.policy.max_active_workers as usize {
        return Ok(None);
    }
    let profiles = db.admission_profiles()?;
    let budget = state.budget_policies.last().map(BudgetPolicy::reference).transpose().map_err(anyhow::Error::msg)?;
    let mut ranked = Vec::new();
    for record in &scheduler.queue {
        let Some(task) = state.tasks.iter().find(|task| task.id == record.task && task.state == TaskState::Queued && task.active_attempt.is_none()) else { continue };
        if record.enqueued_unix_ms > now { continue }
        if state.attempts.iter().any(|attempt| attempt.task == task.id && attempt.retains_capacity()) { continue }
        if state.attempts.iter().filter(|attempt| attempt.task == task.id).count() >= scheduler.policy.max_attempts_per_task as usize { continue }
        let Some(edges) = db.satisfied_edges(task.id.as_str())? else { continue };
        let Some(binding) = unused_binding(&state, &task.id) else { continue };
        let dependencies = edges.into_iter().map(|edge| DependencyInput {
            task: edge.predecessor,
            task_revision: edge.predecessor_revision,
            requirement: edge.requirement,
            evidence: VersionedReference { id: edge.satisfaction_id.clone(), revision: 1, digest: edge.satisfaction_id },
        }).collect::<Vec<_>>();
        let repositories = db.contract_pins(task.id.as_str())?;
        let score = (now - record.enqueued_unix_ms) / 60_000 + i64::from(record.priority);
        ranked.push((score, record.enqueue_sequence, task.clone(), binding, dependencies, repositories));
    }
    ranked.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)).then(left.2.id.cmp(&right.2.id)));
    let Some((_, _, task, binding, dependencies, repositories)) = ranked.into_iter().next() else { return Ok(None) };
    let profile = profiles.into_iter().find(|profile| {
        profile.config.digest == control.config_digest && (binding.identity.agent.is_empty() || profile.kind == binding.identity.agent)
    });
    let Some(profile) = profile else {
        return Ok(Some(Choice::NeedsAuthority { task, head: state.head }));
    };
    let inputs = seal_admission_inputs(project_store, &task, &binding, scheduler.policy.revision, control.epoch, &profile, placeholder_approval(), dependencies, repositories, budget).map_err(anyhow::Error::msg)?;
    Ok(Some(Choice::Ready(inputs)))
}

fn record_missing_grant(db: &mut SqliteStore, task: &TaskId, head: u64, now: i64) -> Result<()> {
    // Stable id: a retry of the same task does not insert another denial.
    let id = format!("admit-{}", &format!("{:x}", Sha256::digest(task.as_str().as_bytes()))[..32]);
    let denial = AuthorityDenial {
        id,
        unix_ms: now,
        class: "approval".into(),
        command: "admit".into(),
        actor_channel: "unknown-rejected".into(),
        reason_code: "authority_missing".into(),
        policy_digest: "0".repeat(64),
        expected_head: Some(head),
        actual_head: Some(head),
    };
    match db.insert_denial(&denial) {
        Ok(()) | Err(StoreError::Conflict) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn matching_grant<'a>(state: &'a Snapshot, inputs: &LaunchInputs, now: i64) -> Option<&'a ApprovalRecord> {
    state.approvals.iter().find(|approval| {
        approval.revoked.is_none() && approval.consumed.is_none() && {
            let mut candidate = inputs.clone();
            candidate.approval = approval.reference.clone();
            approval.grant.matches_launch(&candidate, &inputs.project_store, now).is_ok()
        }
    })
}

/// Inputs for the next candidate, with a placeholder approval. `None` when nothing is ready.
pub fn prepared_admission_inputs(project: &Path) -> Result<Option<LaunchInputs>> {
    let mut db = open_store(project)?;
    let now = jiff::Timestamp::now().as_millisecond();
    let project_store = store_file(project)?;
    let project_store = project_store.to_str().context("project store is not UTF-8")?;
    Ok(match select(&mut db, project_store, now)? {
        Some(Choice::Ready(inputs)) => Some(inputs),
        _ => None,
    })
}

/// Reserve at most one ready attempt through `reserve_prepared`. Does not launch.
pub fn admit_once(project: &Path) -> Result<()> {
    let mut db = open_store(project)?;
    if !db.factory_admission_enabled()? {
        return Ok(());
    }
    let now = jiff::Timestamp::now().as_millisecond();
    let project_store = store_file(project)?;
    let project_store = project_store.to_str().context("project store is not UTF-8")?;
    match select(&mut db, project_store, now)? {
        None => Ok(()),
        Some(Choice::NeedsAuthority { task, head }) => record_missing_grant(&mut db, &task.id, head, now),
        Some(Choice::Ready(mut inputs)) => {
            let state = db.read_snapshot(None)?;
            let task = inputs.task.clone();
            let head = state.head;
            let Some(reference) = matching_grant(&state, &inputs, now).map(|approval| approval.reference.clone()) else {
                record_missing_grant(&mut db, &task, head, now)?;
                return Ok(());
            };
            inputs.approval = reference;
            // The head is the snapshot this grant matched. A later writer conflicts inside reserve_prepared.
            db.reserve_prepared(&[PreparedLaunch { inputs }], head, now)?;
            Ok(())
        }
    }
}

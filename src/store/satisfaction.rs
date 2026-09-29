//! Schema 30 dependency evidence. A row is written only from a stored verified
//! result or integrated commit. Narrative task success is not a receipt, and a
//! verified_result row is not an integrated_commit row. factory_admission stays
//! off unless a test setter or a later signed installer changes it.
use super::*;
use crate::runner::Runner;
use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};

const SCHEMA_VERSION: u32 = 30;

fn invalid(message: &str) -> StoreError {
    StoreError::Invalid(message.into())
}
fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn at_least_30(db: &Connection) -> Result<bool> {
    let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    Ok(version >= SCHEMA_VERSION)
}
fn satisfaction_id(task: &str, predecessor: &str, requirement: &str, evidence_id: &str) -> String {
    sha256_hex(format!("{task}\0{predecessor}\0{requirement}\0{evidence_id}").as_bytes())
}

struct VerifiedReceipt {
    task_id: String,
}

fn load_verified(db: &Connection, result_id: &str) -> Result<Option<VerifiedReceipt>> {
    db.query_row(
        "SELECT r.task_id
         FROM verified_results v
         JOIN verification_runs r ON r.run_id = v.run_id
         WHERE v.result_id=?1 AND r.state='accepted' AND v.memory_fence=r.memory_fence
           AND v.isolation='linux-unshare-user-pid-mount-v1'",
        [result_id],
        |row| Ok(VerifiedReceipt { task_id: row.get(0)? }),
    )
    .optional()
    .map_err(StoreError::from)
}

/// A verified receipt may replace the current row only when its attempt is the
/// selected one and its contract is the predecessor's latest revision. An older
/// run must not strand that row as invalid: revalidation is forbidden.
fn verified_result_may_replace(tx: &Connection, result_id: &str) -> Result<bool> {
    tx.query_row(
        "SELECT EXISTS(
            SELECT 1
            FROM verified_results v
            JOIN verification_runs r ON r.run_id=v.run_id
            JOIN attempts a ON a.id=r.attempt_id AND a.task_id=r.task_id
            JOIN tasks t ON t.id=a.task_id
            WHERE v.result_id=?1 AND r.state='accepted'
              AND (
                t.active_attempt=r.attempt_id
                OR (
                  t.active_attempt IS NULL
                  AND r.attempt_id=(
                    SELECT latest.id FROM attempts latest
                    WHERE latest.task_id=r.task_id
                    ORDER BY latest.rowid DESC
                    LIMIT 1
                  )
                )
              )
              AND r.contract_revision=(
                SELECT MAX(c.contract_revision) FROM task_contracts c WHERE c.task_id=r.task_id
              )
         )",
        [result_id],
        |row| row.get(0),
    )
    .map_err(StoreError::from)
}

fn predecessor_revoked(db: &Connection, predecessor: &str) -> Result<bool> {
    let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    // Wave barriers are schema 40. Older databases have no such rows.
    if version < 40 {
        return Ok(false);
    }
    let source = if version >= 43 { "barrier_current_status" } else { "barrier_revisions" };
    // A revoked membership stays blocking until a later barrier for this task
    // is released. Checking here, not a one-shot satisfaction update, keeps a
    // re-record Invalid instead of a primary-key Conflict.
    db.query_row(
        &format!("SELECT EXISTS(
            SELECT 1
            FROM barrier_members m
            JOIN {source} b ON b.barrier_id=m.barrier_id
            WHERE m.task_id=?1
              AND b.revoked_seq IS NOT NULL
              AND NOT EXISTS (
                  SELECT 1
                  FROM barrier_members later_member
                  JOIN {source} later ON later.barrier_id=later_member.barrier_id
                  WHERE later_member.task_id=m.task_id
                    AND later.released_seq IS NOT NULL
                    AND later.revoked_seq IS NULL
                    AND later.created_seq>b.created_seq
              )
         )"),
        [predecessor],
        |row| row.get(0),
    )
    .map_err(StoreError::from)
}

fn insert_valid(
    tx: &Connection,
    task_id: &str,
    predecessor: &str,
    requirement: &str,
    evidence_id: &str,
    replace_existing: bool,
) -> Result<()> {
    insert_valid_with_budget(tx, task_id, predecessor, requirement, evidence_id, replace_existing, None)
}

fn insert_valid_with_budget(
    tx: &Connection, task_id: &str, predecessor: &str, requirement: &str,
    evidence_id: &str, replace_existing: bool, budget: Option<&read_budget::ReadBudget>,
) -> Result<()> {
    if let Some(budget) = budget { budget.check()?; }
    let result = if requirement == "integrated_commit" {
        if !super::contract_binding::integrated_output_checks_current(tx, evidence_id, budget)? { return Ok(()); }
        tx.query_row("SELECT o.verified_result_id FROM integrated_commits i JOIN integration_operations o ON o.operation_id=i.operation_id WHERE i.integrated_id=?1", [evidence_id], |r| r.get::<_, String>(0))?
    } else { evidence_id.to_string() };
    if !super::contract_binding::policy_matches_with_budget(tx, task_id, predecessor, requirement, &result, budget)? { return Ok(()); }
    if !super::contract_binding::verified_result_barrier_current(tx, &result, budget)? { return Ok(()); }
    if predecessor_revoked(tx, predecessor)? {
        return Err(invalid("dependency blocked"));
    }
    let existing: Option<(String, String)> = tx
        .query_row(
            "SELECT satisfaction_id, evidence_id FROM dependency_satisfactions WHERE task_id=?1 AND predecessor_task=?2 AND requirement=?3 AND state='valid'",
            params![task_id, predecessor, requirement],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((satisfaction_id, evidence)) = existing {
        if evidence == evidence_id {
            return Ok(());
        }
        if !replace_existing {
            return Ok(());
        }
        // A newer current receipt must not roll back the commit that wrote it.
        // The old row stays for history; only one valid row remains.
        tx.execute(
            "UPDATE dependency_satisfactions SET state='invalid' WHERE satisfaction_id=?1 AND state='valid'",
            [satisfaction_id],
        )?;
    }
    // A non-current receipt must not take the empty slot. Attach would then
    // be unable to record the attempt that actually counts.
    if !replace_existing {
        return Ok(());
    }
    let now = jiff::Timestamp::now().as_millisecond();
    tx.execute(
        "INSERT INTO dependency_satisfactions(satisfaction_id,task_id,predecessor_task,requirement,state,evidence_kind,evidence_id,created_unix_ms) VALUES(?1,?2,?3,?4,'valid',?4,?5,?6)",
        params![
            satisfaction_id(task_id, predecessor, requirement, evidence_id),
            task_id,
            predecessor,
            requirement,
            evidence_id,
            now
        ],
    )?;
    Ok(())
}

fn consumers(tx: &Connection, predecessor: &str, requirement: &str) -> Result<Vec<String>> {
    let mut stmt = tx.prepare(
        "SELECT task_id FROM task_dependencies WHERE predecessor_id=?1 AND requirement=?2 ORDER BY task_id",
    )?;
    let rows = stmt.query_map(params![predecessor, requirement], |row| row.get(0))?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(StoreError::from)
}

/// Satisfies `verified_result` edges only. An integrated_commit edge is left untouched.
pub(super) fn record_verified_result(tx: &Connection, result_id: &str) -> Result<()> {
    if !at_least_30(tx)? {
        return Ok(());
    }
    let Some(receipt) = load_verified(tx, result_id)? else {
        return Err(invalid("verified receipt is not stored"));
    };
    // An open memory fence still stores the row. The report is what hides it.
    // A stale attempt or contract does not replace a current valid row.
    let replace = verified_result_may_replace(tx, result_id)?;
    for task_id in consumers(tx, &receipt.task_id, "verified_result")? {
        insert_valid(
            tx,
            &task_id,
            &receipt.task_id,
            "verified_result",
            result_id,
            replace,
        )?;
    }
    Ok(())
}

fn integrated_predecessor(tx: &Connection, integrated_id: &str) -> Result<Option<String>> {
    tx.query_row(
        "SELECT r.task_id
         FROM integrated_commits i
         JOIN integration_operations o ON o.operation_id=i.operation_id
         JOIN verified_results v ON v.result_id=o.verified_result_id
         JOIN verification_runs r ON r.run_id=v.run_id
         WHERE i.integrated_id=?1 AND r.state='accepted'",
        [integrated_id],
        |row| row.get(0),
    )
    .optional()
    .map_err(StoreError::from)
}

/// Satisfies `integrated_commit` edges only. A verified_result edge is a different fact.
pub(super) fn record_integrated_commit(tx: &Connection, integrated_id: &str) -> Result<()> {
    if !at_least_30(tx)? {
        return Ok(());
    }
    let Some(predecessor) = integrated_predecessor(tx, integrated_id)? else {
        return Err(invalid("integrated receipt is not stored"));
    };
    for task_id in consumers(tx, &predecessor, "integrated_commit")? {
        insert_valid(
            tx,
            &task_id,
            &predecessor,
            "integrated_commit",
            integrated_id,
            true,
        )?;
    }
    Ok(())
}

/// Newest accepted result that may still replace, not merely the latest clock.
/// A later run for an older attempt must not be the one attach records.
fn current_verified(tx: &Connection, task_id: &str, predecessor: &str, budget: &read_budget::ReadBudget) -> Result<Option<String>> {
    let mut stmt = tx.prepare(
        "SELECT v.result_id
         FROM verification_runs r JOIN verified_results v ON v.run_id=r.run_id
         WHERE r.task_id=?1 AND r.state='accepted'
           AND r.attempt_id=COALESCE((SELECT active_attempt FROM tasks WHERE id=?1),
               (SELECT id FROM attempts WHERE task_id=?1 ORDER BY rowid DESC LIMIT 1))
           AND r.contract_revision=(SELECT MAX(contract_revision) FROM task_contracts WHERE task_id=?1)
           AND v.memory_fence=r.memory_fence AND v.isolation='linux-unshare-user-pid-mount-v1'
         ORDER BY v.created_unix_ms DESC, v.result_id DESC")?;
    let mut rows = stmt.query([predecessor])?;
    while let Some(row) = rows.next()? {
        budget.row(row, &[])?;
        let result_id: String = row.get(0)?;
        if super::contract_binding::policy_matches_with_budget(tx, task_id, predecessor, "verified_result", &result_id, Some(budget))?
            && super::contract_binding::verified_result_barrier_current(tx, &result_id, Some(budget))? {
            return Ok(Some(result_id));
        }
    }
    Ok(None)
}

fn latest_integrated(tx: &Connection, task_id: &str, predecessor: &str, budget: &read_budget::ReadBudget) -> Result<Option<String>> {
    let mut stmt = tx.prepare(
        "SELECT i.integrated_id, o.verified_result_id
         FROM integrated_commits i
         JOIN integration_operations o ON o.operation_id=i.operation_id
         JOIN verified_results v ON v.result_id=o.verified_result_id
         JOIN verification_runs r ON r.run_id=v.run_id
         WHERE r.task_id=?1 AND r.state='accepted'
         ORDER BY i.created_unix_ms DESC, i.integrated_id DESC")?;
    let mut rows = stmt.query([predecessor])?;
    while let Some(row) = rows.next()? {
        budget.row(row, &[])?;
        let (integrated_id, result_id): (String,String) = (row.get(0)?,row.get(1)?);
        if super::contract_binding::policy_matches_with_budget(tx, task_id, predecessor, "integrated_commit", &result_id, Some(budget))?
            && super::contract_binding::integrated_output_checks_current(tx, &integrated_id, Some(budget))?
            && super::contract_binding::verified_result_barrier_current(tx, &result_id, Some(budget))? {
            return Ok(Some(integrated_id));
        }
    }
    Ok(None)
}

/// Raw contract-install callers own this temporary SQL deadline. Queue mutation
/// passes its original budget directly to attachment without renewing a handler.
pub(super) fn attach_stored_receipts(tx: &Connection, task_id: &str) -> Result<()> {
    read_budget::with_local_deadline(tx, |budget| attach_stored_receipts_with_budget(tx, task_id, budget))
}

pub(super) fn attach_stored_receipts_with_budget(tx: &Connection, task_id: &str, budget: &read_budget::ReadBudget) -> Result<()> {
    if !at_least_30(tx)? { return Ok(()); }
    let mut stmt = tx.prepare(
        "SELECT predecessor_id, requirement FROM task_dependencies WHERE task_id=?1 ORDER BY predecessor_id LIMIT 257")?;
    let mut rows = stmt.query([task_id])?;
    let mut edges = Vec::new();
    while let Some(row) = rows.next()? {
        budget.row(row, &[])?;
        if edges.len() == 256 { return Err(StoreError::Limit("receipt attachment exceeds 256 dependencies".into())); }
        edges.push((row.get::<_,String>(0)?, row.get::<_,String>(1)?));
    }
    for (predecessor, requirement) in edges {
        budget.check()?;
        let evidence = match requirement.as_str() {
            "verified_result" => current_verified(tx, task_id, &predecessor, budget)?,
            "integrated_commit" => latest_integrated(tx, task_id, &predecessor, budget)?,
            _ => None,
        };
        if let Some(evidence) = evidence {
            // Attaching this consumer must not fan out through every other
            // consumer. Native receipt publication owns that separate operation.
            insert_valid_with_budget(tx, task_id, &predecessor, &requirement, &evidence, true, Some(budget))?;
        }
    }
    budget.check()
}

pub(super) fn admission_enabled(db: &Connection) -> Result<bool> {
    if !at_least_30(db)? {
        return Ok(false);
    }
    let value: String = db.query_row(
        "SELECT factory_admission FROM project_control WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    match value.as_str() {
        "off" => Ok(false),
        "on" => Ok(true),
        _ => Err(StoreError::Corrupt(
            "factory_admission is not off or on".into(),
        )),
    }
}

fn verified_counts(db: &Connection, task_id: &str, predecessor: &str,budget:Option<&read_budget::ReadBudget>) -> Result<bool> {
    if predecessor_revoked(db, predecessor)? {
        return Ok(false);
    }
    let result: Option<String> = db.query_row("SELECT evidence_id FROM dependency_satisfactions WHERE task_id=?1 AND predecessor_task=?2 AND requirement='verified_result' AND state='valid'", params![task_id, predecessor], |r| r.get(0)).optional()?;
    let Some(result) = result else { return Ok(false) };
    // A seeded candidate (contracts-review.md §8) or a candidate-group arm that
    // is not its group's selection (contracts-quality.md §3) never releases a
    // dependent. Checked here, where every release reads the edge, so a row
    // recorded before registration or selection stays held until it counts.
    if super::seeded_defects::seeded_result(db, &result)? || super::candidate_groups::held_result(db, &result)? { return Ok(false); }
    if !super::contract_binding::policy_matches_with_budget(db, task_id, predecessor, "verified_result", &result,budget)? { return Ok(false); }
    if !super::contract_binding::verified_result_barrier_current(db, &result, budget)? { return Ok(false); }
    db.query_row(
        "SELECT EXISTS(
            SELECT 1
            FROM dependency_satisfactions s
            JOIN verified_results v ON v.result_id=s.evidence_id AND s.evidence_kind='verified_result'
            JOIN verification_runs r ON r.run_id=v.run_id AND r.task_id=s.predecessor_task
            JOIN attempts a ON a.id=r.attempt_id AND a.task_id=r.task_id
            JOIN tasks t ON t.id=a.task_id
            WHERE s.task_id=?1 AND s.predecessor_task=?2 AND s.requirement='verified_result' AND s.state='valid'
              AND r.state='accepted' AND v.memory_fence=r.memory_fence
              AND (
                t.active_attempt=r.attempt_id
                OR (
                  t.active_attempt IS NULL
                  AND r.attempt_id=(
                    SELECT latest.id FROM attempts latest
                    WHERE latest.task_id=r.task_id
                    ORDER BY latest.rowid DESC
                    LIMIT 1
                  )
                )
              )
              AND r.contract_revision=(
                SELECT MAX(c.contract_revision) FROM task_contracts c WHERE c.task_id=r.task_id
              )
              AND NOT EXISTS(
                SELECT 1 FROM memory_invalidations i
                WHERE (i.task_id=s.predecessor_task OR i.task_id IS NULL)
                  AND i.resolved_seq IS NULL AND i.severity!='informational'
              )
         )",
        params![task_id, predecessor],
        |row| row.get(0),
    )
    .map_err(StoreError::from)
}

fn integrated_counts(db: &Connection, task_id: &str, predecessor: &str,budget:Option<&read_budget::ReadBudget>) -> Result<bool> {
    if predecessor_revoked(db, predecessor)? {
        return Ok(false);
    }
    let result: Option<String> = db.query_row("SELECT o.verified_result_id FROM dependency_satisfactions s JOIN integrated_commits i ON i.integrated_id=s.evidence_id JOIN integration_operations o ON o.operation_id=i.operation_id WHERE s.task_id=?1 AND s.predecessor_task=?2 AND s.requirement='integrated_commit' AND s.state='valid'", params![task_id, predecessor], |r| r.get(0)).optional()?;
    let Some(result) = result else { return Ok(false) };
    let integrated: String = db.query_row("SELECT evidence_id FROM dependency_satisfactions WHERE task_id=?1 AND predecessor_task=?2 AND requirement='integrated_commit' AND state='valid'", params![task_id,predecessor], |row| row.get(0))?;
    if !super::contract_binding::integrated_output_checks_current(db, &integrated, budget)? { return Ok(false); }
    if !super::contract_binding::policy_matches_with_budget(db, task_id, predecessor, "integrated_commit", &result,budget)? { return Ok(false); }
    if !super::contract_binding::verified_result_barrier_current(db, &result, budget)? { return Ok(false); }
    db.query_row(
        "SELECT EXISTS(
            SELECT 1
            FROM dependency_satisfactions s
            JOIN integrated_commits i ON i.integrated_id=s.evidence_id AND s.evidence_kind='integrated_commit'
            JOIN integration_operations o ON o.operation_id=i.operation_id
            JOIN verified_results v ON v.result_id=o.verified_result_id
            JOIN verification_runs r ON r.run_id=v.run_id AND r.task_id=s.predecessor_task
            WHERE s.task_id=?1 AND s.predecessor_task=?2 AND s.requirement='integrated_commit' AND s.state='valid'
              AND r.state='accepted' AND r.task_id=?2
              AND NOT EXISTS(
                SELECT 1 FROM memory_invalidations m
                WHERE (m.task_id=s.predecessor_task OR m.task_id IS NULL)
                  AND m.resolved_seq IS NULL AND m.severity!='informational'
              )
         )",
        params![task_id, predecessor],
        |row| row.get(0),
    )
    .map_err(StoreError::from)
}

/// `None` means this edge adds no dependency blocker. landed_commit and
/// integration_candidate never count as valid, even if some other receipt exists.
pub(super) fn dependency_blocker(
    db: &Connection,
    task_id: &str,
    predecessor: &Task,
    requirement: DependencyRequirement,
    admission_on: bool,
) -> Result<Option<String>> {
    dependency_blocker_with_budget(db,task_id,predecessor,requirement,admission_on,None)
}
pub(super) fn dependency_blocker_with_budget(db:&Connection,task_id:&str,predecessor:&Task,requirement:DependencyRequirement,admission_on:bool,budget:Option<&read_budget::ReadBudget>)->Result<Option<String>> {
    let pred = predecessor.id.as_str();
    let requirement_text = requirement.as_str();
    if matches!(predecessor.state, TaskState::Failed | TaskState::Cancelled) {
        return Ok(Some(format!(
            "predecessor_failed:{pred}:{requirement_text}"
        )));
    }
    let valid = at_least_30(db)?
        && match requirement {
            DependencyRequirement::VerifiedResult => verified_counts(db, task_id, pred,budget)?,
            DependencyRequirement::IntegratedCommit => integrated_counts(db, task_id, pred,budget)?,
            DependencyRequirement::IntegrationCandidate | DependencyRequirement::LandedCommit => {
                false
            }
        };
    if !valid {
        return Ok(Some(format!(
            "verified_dependency_evidence_unavailable:{pred}:{requirement_text}"
        )));
    }
    if !admission_on {
        return Ok(Some(format!("admission_disabled:{requirement_text}")));
    }
    Ok(None)
}

fn valid_satisfaction_id_with_budget(db:&Connection,task_id:&str,predecessor:&Task,requirement:DependencyRequirement,budget:Option<&read_budget::ReadBudget>)->Result<Option<String>> {
    // `admission_on: true` asks whether the receipt counts, not whether the flag is on.
    if dependency_blocker_with_budget(db, task_id, predecessor, requirement, true,budget)?.is_some() {
        return Ok(None);
    }
    db.query_row(
        "SELECT satisfaction_id FROM dependency_satisfactions WHERE task_id=?1 AND predecessor_task=?2 AND requirement=?3 AND state='valid'",
        params![task_id, predecessor.id.as_str(), requirement.as_str()],
        |row| row.get(0),
    )
    .optional()
    .map_err(StoreError::from)
}

/// Queue edges and sealed dependency inputs must name the same valid rows.
/// Integrated commits also need a pinned base that contains every one of them.
pub(super) fn require_dependency_evidence_with_budget(db:&Connection,inputs:&LaunchInputs,dependencies:&[Dependency],tasks:&[Task],proof:Option<&IntegratedProof>,budget:Option<&read_budget::ReadBudget>)->Result<()> {
    if inputs.dependencies.len() != dependencies.len() {
        return Err(invalid("task is not ready for reservation"));
    }
    let mut matched = std::collections::BTreeSet::new();
    for edge in dependencies {
        let predecessor = tasks
            .iter()
            .find(|task| task.id == edge.predecessor)
            .ok_or(StoreError::Conflict)?;
        let Some(dep) = inputs.dependencies.iter().find(|dep| {
            dep.task == edge.predecessor
                && dep.requirement == edge.requirement
                && matched.insert(dep.task.clone())
        }) else {
            return Err(invalid("task is not ready for reservation"));
        };
        let Some(satisfaction_id) =
            valid_satisfaction_id_with_budget(db, inputs.task.as_str(), predecessor, edge.requirement,budget)?
        else {
            return Err(invalid("task is not ready for reservation"));
        };
        if dep.evidence.digest != satisfaction_id || dep.task_revision != predecessor.revision {
            return Err(invalid("task is not ready for reservation"));
        }
    }
    if matched.len() != inputs.dependencies.len() {
        return Err(invalid("task is not ready for reservation"));
    }
    recheck_integrated_base(db, inputs, proof,budget)
}

pub(super) struct IntegratedProof {
    pub(super) task: TaskId,
    checked: std::collections::BTreeMap<(String, String), (String, Vec<String>)>,
}

fn integrated_groups(
    db: &Connection,
    task_id: &str,
    budget:Option<&read_budget::ReadBudget>,
) -> Result<std::collections::BTreeMap<(String, String), Vec<String>>> {
    let mut stmt = db.prepare(
        "SELECT i.repository, i.ref_name, i.commit_oid
         FROM dependency_satisfactions s
         JOIN integrated_commits i ON i.integrated_id=s.evidence_id AND s.evidence_kind='integrated_commit'
         JOIN task_dependencies d ON d.task_id=s.task_id AND d.predecessor_id=s.predecessor_task AND d.requirement='integrated_commit'
         WHERE s.task_id=?1 AND s.state='valid'
         ORDER BY i.repository, i.ref_name, s.predecessor_task LIMIT 257",
    )?;
    let mut rows=stmt.query([task_id])?;let mut count=0;
    let mut groups = std::collections::BTreeMap::new();
    while let Some(row)=rows.next()? {
        count+=1;if count>256{return Err(StoreError::Limit("integrated dependency limit exceeded".into()));}
        if let Some(budget)=budget {budget.row(row,&[])?;}
        let repository:String=row.get(0)?;let ref_name:String=row.get(1)?;let commit:String=row.get(2)?;
        groups
            .entry((repository, ref_name))
            .or_insert_with(Vec::new)
            .push(commit);
    }
    Ok(groups)
}

fn contract_base(db: &Connection, task_id: &str, repository: &str) -> Result<Option<String>> {
    db.query_row(
        "SELECT base_oid FROM task_contracts WHERE task_id=?1 AND repository=?2 ORDER BY contract_revision DESC LIMIT 1",
        params![task_id, repository],
        |row| row.get(0),
    )
    .optional()
    .map_err(StoreError::from)
}

/// Git runs before the reservation write transaction. Timeout, a non-UTF-8 path, or a non-ancestor is `integration_missing`.
pub(super) fn prove_integrated_base(db: &Connection, inputs: &LaunchInputs, control: Option<&super::controlled::ReadControl>,budget:Option<&read_budget::ReadBudget>) -> Result<IntegratedProof> {
    let groups = integrated_groups(db, inputs.task.as_str(),budget)?;
    let mut checked = std::collections::BTreeMap::new();
    // Every integrated-commit dependency, even a single one, must be contained in the pinned base.
    for ((repository, ref_name), commits) in groups {
        let Some(pin) = inputs.repositories.iter().find(|repo| repo.repository == repository) else {
            return Err(invalid("integration_missing"));
        };
        if contract_base(db, inputs.task.as_str(), &repository)?.is_some_and(|base| base != pin.commit) {
            return Err(invalid("integration_missing"));
        }
        let repo = std::path::Path::new(&repository);
        for commit in &commits {
            if commit == &pin.commit {
                continue;
            }
            // Failure here is not ancestry. Replace refs and a hang must not admit.
            if bounded_git_ok(repo, &["merge-base", "--is-ancestor", commit, &pin.commit], control).is_err() {
                return Err(invalid("integration_missing"));
            }
        }
        checked.insert((repository, ref_name), (pin.commit.clone(), commits));
    }
    Ok(IntegratedProof { task: inputs.task.clone(), checked })
}

fn recheck_integrated_base(db: &Connection, inputs: &LaunchInputs, proof: Option<&IntegratedProof>,budget:Option<&read_budget::ReadBudget>) -> Result<()> {
    let groups = integrated_groups(db, inputs.task.as_str(),budget)?;
    for ((repository, ref_name), commits) in &groups {
        let Some(pin) = inputs.repositories.iter().find(|repo| repo.repository == *repository) else {
            return Err(invalid("integration_missing"));
        };
        if contract_base(db, inputs.task.as_str(), repository)?.is_some_and(|base| base != pin.commit) {
            return Err(invalid("integration_missing"));
        }
        let Some(proof) = proof.filter(|proof| proof.task == inputs.task) else {
            return Err(invalid("integration_missing"));
        };
        match proof.checked.get(&(repository.clone(), ref_name.clone())) {
            Some((proven_pin, proven_commits)) if proven_pin == &pin.commit && proven_commits == commits => {}
            _ => return Err(invalid("integration_missing")),
        }
    }
    if let Some(proof) = proof.filter(|proof| proof.task == inputs.task) {
        for key in proof.checked.keys() {
            if !groups.contains_key(key) {
                return Err(invalid("integration_missing"));
            }
        }
    }
    Ok(())
}

fn bounded_git_ok(repo: &std::path::Path, args: &[&str], control: Option<&super::controlled::ReadControl>) -> std::result::Result<(), ()> {
    let mut command = crate::runner::Cmd::repository_git_command(repo, args).map_err(|_| ())?;
    command.deadline = Some(control.map_or_else(||std::time::Instant::now()+std::time::Duration::from_secs(5),|c|c.deadline()));
    if let Some(control)=control {control.check().map_err(|_|())?;command.cancellation=Some(control.cancellation());}
    let output = crate::runner::RealRunner.run(&command).map_err(|_| ())?;
    if output.success() { Ok(()) } else { Err(()) }
}

fn retained_profiles(db: &Connection, config_digest:Option<&str>, budget: Option<&read_budget::ReadBudget>) -> Result<Vec<FrozenProfile>> {
    use std::os::unix::fs::MetadataExt;
    let path = db
        .path()
        .ok_or_else(|| invalid("store path missing"))?;
    let path = std::fs::canonicalize(path).map_err(|e| StoreError::Io(e.to_string()))?;
    let metadata = std::fs::metadata(&path).map_err(|e| StoreError::Io(e.to_string()))?;
    let binding=serde_json::json!([path,metadata.dev(),metadata.ino()]).to_string();
    let mut stmt = db.prepare(
        "SELECT profile_digest, report, report_digest FROM native_profiles WHERE json(json_extract(report,'$.source_store'))=?1 AND json_extract(report,'$.preparation.profile.config.digest') IS ?2 ORDER BY profile_digest LIMIT 257",
    )?;
    let mut rows = stmt.query(params![binding,config_digest])?;
    let mut profiles = Vec::new();
    let mut count=0;
    while let Some(row) = rows.next()? {
        if let Some(budget)=budget {budget.row(row,&[(1,3)])?;}
        count+=1;
        if count>256 {return Err(StoreError::Limit("retained admission profile limit exceeded".into()));}
        let (digest, report, report_digest): (String, String, String) =
            (row.get(0)?, row.get(1)?, row.get(2)?);
        if format!("{:x}", Sha256::digest(report.as_bytes())) != report_digest {
            return Err(StoreError::Corrupt("native profile report digest mismatch".into()));
        }
        let value: serde_json::Value = serde_json::from_str(&report)
            .map_err(|_| StoreError::Corrupt("invalid native profile report".into()))?;
        if value["source_store"] != serde_json::json!([path, metadata.dev(), metadata.ino()]) {
            continue;
        }
        let Some(profile) = value["preparation"]["profile"].as_object() else {
            continue;
        };
        let profile: FrozenProfile = serde_json::from_value(serde_json::Value::Object(profile.clone()))
            .map_err(|_| StoreError::Corrupt("invalid retained profile".into()))?;
        if profile.reference().map_err(StoreError::Corrupt)?.digest != digest
            || profile.config.digest.as_deref()!=config_digest
            || profile.validate_for_launch().is_err()
        {
            continue;
        }
        profiles.push(profile);
    }
    Ok(profiles)
}

impl SqliteStore {
    pub(crate) fn factory_admission_enabled(&self) -> Result<bool> {
        admission_enabled(&self.connection)
    }

    #[cfg(test)]
    pub(crate) fn admission_profiles(&self) -> Result<Vec<FrozenProfile>> {
        let control=super::control::read_with_budget(&self.connection,None)?;
        self.admission_profiles_with_budget(control.config_digest.as_deref(),None)
    }
    pub(crate) fn admission_profiles_with_budget(&self,config_digest:Option<&str>,budget:Option<&read_budget::ReadBudget>)->Result<Vec<FrozenProfile>> {
        retained_profiles(&self.connection,config_digest,budget)
    }

    /// `None` when any edge lacks a current valid satisfaction. An empty queue is ready.
    #[cfg(test)]
    pub(crate) fn satisfied_edges(&self, task_id: &str) -> Result<Option<Vec<SatisfiedEdge>>> {self.satisfied_edges_with_budget(task_id,None)}
    pub(crate) fn satisfied_edges_with_budget(&self,task_id:&str,budget:Option<&read_budget::ReadBudget>)->Result<Option<Vec<SatisfiedEdge>>> {
        satisfied_edges_on(&self.connection,task_id,budget)
    }

    /// `None` when a contract base has no resolvable tree. That candidate is not reserved.
    pub(crate) fn contract_pins_with_budget(&self,task_id:&str,control:Option<&super::controlled::ReadControl>,budget:Option<&read_budget::ReadBudget>)->Result<Option<Vec<RepositoryInput>>> {
        let mut stmt = self.connection.prepare(
            "SELECT repository, base_oid FROM task_contracts WHERE task_id=?1 AND contract_revision=(SELECT MAX(contract_revision) FROM task_contracts c WHERE c.task_id=?1)",
        )?;
        let mut rows=stmt.query([task_id])?;
        let mut pins = Vec::new();
        while let Some(row)=rows.next()? {
            if let Some(budget)=budget {budget.row(row,&[])?;}
            let repository:String=row.get(0)?;let base:String=row.get(1)?;
            let Some(tree) = git_tree(&repository, &base, control) else { return Ok(None); };
            pins.push(RepositoryInput { repository, commit: base, tree });
        }
        Ok(Some(pins))
    }


}

pub(super) fn satisfied_edges_on(db:&Connection,task_id:&str,budget:Option<&read_budget::ReadBudget>)->Result<Option<Vec<SatisfiedEdge>>> {
        if !super::contract_binding::queue_matches_with_budget(db, task_id,budget)? { return Ok(None); }
        let mut stmt = db.prepare(
            "SELECT predecessor_id, requirement FROM task_dependencies WHERE task_id=?1 ORDER BY predecessor_id LIMIT 257",
        )?;
        let mut rows=stmt.query([task_id])?;let mut edges=Vec::new();
        while let Some(row)=rows.next()? {
            if edges.len()==256 {return Err(StoreError::Limit("task dependency limit exceeded".into()));}
            if let Some(budget)=budget {budget.row(row,&[])?;}
            edges.push((row.get::<_,String>(0)?,row.get::<_,String>(1)?));
        }
        let mut satisfied = Vec::new();
        for (predecessor_id, requirement) in edges {
            let requirement = match requirement.as_str() {
                "verified_result" => DependencyRequirement::VerifiedResult,
                "integrated_commit" => DependencyRequirement::IntegratedCommit,
                "integration_candidate" => DependencyRequirement::IntegrationCandidate,
                "landed_commit" => DependencyRequirement::LandedCommit,
                _ => return Err(StoreError::Corrupt("unknown dependency requirement".into())),
            };
            let predecessor = super::read_task_with_budget(db, &predecessor_id,budget)?;
            let Some(satisfaction_id) =
                valid_satisfaction_id_with_budget(db, task_id, &predecessor, requirement,budget)?
            else {
                return Ok(None);
            };
            satisfied.push(SatisfiedEdge {
                predecessor: predecessor.id.clone(),
                predecessor_revision: predecessor.revision,
                requirement,
                satisfaction_id,
            });
        }
        Ok(Some(satisfied))
}

pub(crate) struct ResourceClaim {
    pub kind: String,
    pub resource: String,
    pub access: String,
    pub certainty: String,
}

/// Advisory candidate-selection cache. Reservation independently reloads the
/// held claims under its current-head write transaction.
pub(crate) struct RetainedClaimSet {
    holders: Vec<(TaskId,Vec<ResourceClaim>)>,
}
impl SqliteStore {
    pub(crate) fn admission_retained_claims(&self,budget:Option<&read_budget::ReadBudget>)->Result<RetainedClaimSet> {
        let mut holders=Vec::new();let mut seen=std::collections::BTreeSet::new();
        for attempt in super::read_retained_attempts_with_budget(&self.connection,budget)? {
            if let Some(revision)=revision_at_reservation(&self.connection,attempt.id.as_str(),attempt.task.as_str())? {
                if seen.insert((attempt.task.clone(),revision)) {
                    let claims=claim_rows(&self.connection,attempt.task.as_str(),revision,budget)?;
                    if !claims.is_empty(){holders.push((attempt.task,claims));}
                }
            }
        }
        Ok(RetainedClaimSet{holders})
    }
    pub(crate) fn admission_claim_overlap(&self,task:&str,held:&RetainedClaimSet,budget:Option<&read_budget::ReadBudget>)->Result<bool> {
        let Some(revision)=latest_contract_revision(&self.connection,task)? else{return Ok(false);};
        let candidate=claim_rows(&self.connection,task,revision,budget)?;
        Ok(held.holders.iter().any(|(owner,claims)|owner.as_str()!=task&&candidate.iter().any(|claim|claims.iter().any(|other|claims_conflict(claim,other)))))
    }
}

pub(crate) fn claims_conflict(left: &ResourceClaim, right: &ResourceClaim) -> bool {
    if left.kind != right.kind {
        return false;
    }
    if left.kind == "named" {
        return left.resource == right.resource && (left.access == "write" || right.access == "write");
    }
    if left.kind != "path" {
        return true;
    }
    if left.certainty == "exact" && right.certainty == "exact" {
        return left.resource == right.resource && (left.access == "write" || right.access == "write");
    }
    paths_could_overlap(&left.resource, &right.resource)
}

fn paths_could_overlap(left: &str, right: &str) -> bool {
    let (left_dir, left_segs) = split_claim_path(left);
    let (right_dir, right_segs) = split_claim_path(right);
    if left_segs.is_empty() || right_segs.is_empty() {
        return true;
    }
    let shared = left_segs.len().min(right_segs.len());
    for index in 0..shared {
        let (left_part, right_part) = (left_segs[index], right_segs[index]);
        // `**` can cover the rest of either path.
        if left_part.contains("**") || right_part.contains("**") {
            return true;
        }
        if left_part != right_part && !glob_segment(left_part) && !glob_segment(right_part) {
            return false;
        }
    }
    if left_segs.len() == right_segs.len() {
        return true;
    }
    if left_segs.len() < right_segs.len() { left_dir } else { right_dir }
}

fn split_claim_path(path: &str) -> (bool, Vec<&str>) {
    let directory = path.ends_with('/');
    let trimmed = path.trim_end_matches('/');
    (directory, trimmed.split('/').filter(|part| !part.is_empty()).collect())
}

fn glob_segment(segment: &str) -> bool {
    segment.contains('*') || segment.contains('?') || segment.contains('[')
}

fn claim_rows(db: &Connection, task_id: &str, revision: i64,budget:Option<&read_budget::ReadBudget>) -> Result<Vec<ResourceClaim>> {
    let mut stmt = db.prepare(
        "SELECT kind, resource, access, certainty FROM resource_claims WHERE task_id=?1 AND contract_revision=?2 ORDER BY ordinal LIMIT 73",
    )?;
    let mut rows=stmt.query(params![task_id,revision])?;let mut claims=Vec::new();
    while let Some(row)=rows.next()? {
        if claims.len()==72 {return Err(StoreError::Limit("task resource claim limit exceeded".into()));}
        if let Some(budget)=budget {budget.row(row,&[])?;}
        claims.push(ResourceClaim{kind:row.get(0)?,resource:row.get(1)?,access:row.get(2)?,certainty:row.get(3)?});
    }
    Ok(claims)
}

fn latest_contract_revision(db: &Connection, task_id: &str) -> Result<Option<i64>> {
    Ok(db.query_row(
        "SELECT MAX(contract_revision) FROM task_contracts WHERE task_id=?1",
        [task_id],
        |row| row.get(0),
    )?)
}

/// Revision already installed when this attempt was reserved. A later put does not move it.
fn revision_at_reservation(db: &Connection, attempt_id: &str, task_id: &str) -> Result<Option<i64>> {
    let reserved: Option<i64> = db.query_row(
        "SELECT MIN(sequence) FROM events WHERE kind='attempt.reserved' AND entity=?1",
        [attempt_id],
        |row| row.get(0),
    )?;
    let Some(reserved) = reserved else {
        return Ok(None);
    };
    Ok(db.query_row(
        "SELECT MAX(contract_revision) FROM task_contracts WHERE task_id=?1 AND installed_seq<=?2",
        rusqlite::params![task_id, reserved],
        |row| row.get(0),
    )?)
}

/// The first retained holder whose claims overlap `task_id`'s, as bounded
/// display text: both claims (kind, resource excerpt, access) and the holder's
/// task and attempt ids. Never file content.
pub(super) fn overlap_with_retained_with_budget(db:&Connection,task_id:&str,attempts:&[Attempt],budget:Option<&read_budget::ReadBudget>)->Result<Option<String>> {
    let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < 35 {
        return Ok(None);
    }
    let Some(revision) = latest_contract_revision(db, task_id)? else {
        return Ok(None);
    };
    let candidate = claim_rows(db, task_id, revision,budget)?;
    if candidate.is_empty() {
        return Ok(None);
    }
    let show = |claim: &ResourceClaim| {
        let resource: String = claim.resource.chars().map(|c| if c.is_control() { '?' } else { c }).take(128).collect();
        format!("{} {} ({})", claim.kind, resource, claim.access)
    };
    for attempt in attempts {
        if !attempt.retains_capacity() || attempt.task.as_str() == task_id {
            continue;
        }
        let Some(held_revision) = revision_at_reservation(db, attempt.id.as_str(), attempt.task.as_str())? else {
            continue;
        };
        let held = claim_rows(db, attempt.task.as_str(), held_revision,budget)?;
        for claim in &candidate {
            if let Some(other) = held.iter().find(|other| claims_conflict(claim, other)) {
                return Ok(Some(format!("{} overlaps {} held by task {} attempt {}", show(claim), show(other), attempt.task.as_str(), attempt.id.as_str())));
            }
        }
    }
    Ok(None)
}

fn git_tree(repository: &str, commit: &str, control: Option<&super::controlled::ReadControl>) -> Option<String> {
    let spec = format!("{commit}^{{tree}}");
    let mut command = crate::runner::Cmd::repository_git_command(std::path::Path::new(repository), &["rev-parse", "--verify", &spec]).ok()?;
    command.deadline = Some(control.map_or_else(||std::time::Instant::now()+std::time::Duration::from_secs(5),|c|c.deadline()));
    if let Some(control)=control {control.check().ok()?;command.cancellation=Some(control.cancellation());}
    let output = crate::runner::RealRunner.run(&command).ok()?;
    if !output.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout_bytes).ok()?;
    let tree = text.trim();
    if matches!(tree.len(), 40 | 64) && tree.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Some(tree.to_string())
    } else {
        None
    }
}

pub(crate) struct SatisfiedEdge {
    pub predecessor: TaskId,
    pub predecessor_revision: u64,
    pub requirement: DependencyRequirement,
    pub satisfaction_id: String,
}

impl SatisfiedEdge {
    /// The sealed launch input naming this edge's current valid satisfaction.
    pub(crate) fn input(self) -> DependencyInput {
        DependencyInput {
            task: self.predecessor,
            task_revision: self.predecessor_revision,
            requirement: self.requirement,
            evidence: VersionedReference { id: self.satisfaction_id.clone(), revision: 1, digest: self.satisfaction_id },
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    fn user_version(db: &Connection) -> u32 {
        db.query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }
    fn dependencies(db: &Connection) -> Vec<(String, String, String)> {
        let mut stmt = db
            .prepare(
                "SELECT task_id, predecessor_id, requirement FROM task_dependencies ORDER BY task_id, predecessor_id",
            )
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
    }
    fn task(id: &str, state: TaskState) -> Mutation {
        Mutation::Task {
            expected: None,
            next: Task {
                id: TaskId::new(id).unwrap(),
                revision: 1,
                state,
                title: id.into(),
                active_attempt: None,
            },
        }
    }
    pub(in crate::store) fn store_verified_receipt(
        db: &SqliteStore,
        task_id: &str,
        attempt: &str,
        result_id: &str,
        contract_revision: i64,
        created_unix_ms: i64,
    ) {
        let digest = "d".repeat(64);
        let oid = "a".repeat(40);
        let have_contract: bool = db
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM task_contracts WHERE task_id=?1 AND contract_revision=?2)",
                params![task_id, contract_revision],
                |row| row.get(0),
            )
            .unwrap();
        if !have_contract {
            let raw = serde_json::to_vec(&serde_json::json!({
                "version":1,"project_store":"/tmp/project","expected_head":0,"task_id":task_id,"contract_revision":contract_revision,
                "deliverable":"dependency fixture","non_goals":"no external work","acceptance_policies":[{"id":"policy","text":"{}"}],
                "repository":"/tmp/repo","base_oid":oid,"object_format":"sha1","dependencies":[],"capability_flags":[],
                "profile_kind":"codex","retry_class":"none","result_schema_id":"result-v1","route":"verify_only",
                "authority":{"id":"owner-approval-policy","revision":1,"digest":digest}
            })).unwrap();
            let contract_digest = format!("{:x}", Sha256::digest(&raw));
            let installed: i64 = db
                .connection
                .query_row("SELECT COALESCE(MAX(sequence),1) FROM events", [], |row| {
                    row.get(0)
                })
                .unwrap();
            db.connection
                .execute(
                    "INSERT INTO task_contracts(task_id,contract_revision,plan_revision,project_store,expected_head,repository,base_oid,object_format,memory_snapshot_id,route,raw_bytes,raw_digest,installed_seq) VALUES(?1,?2,NULL,'/tmp/project',0,'/tmp/repo',?3,'sha1',NULL,'verify_only',?4,?5,?6)",
                    params![task_id, contract_revision, oid, raw, contract_digest, installed],
                )
                .unwrap();
        }
        let contract_digest: String = db.connection.query_row("SELECT raw_digest FROM task_contracts WHERE task_id=?1 AND contract_revision=?2", params![task_id,contract_revision], |row| row.get(0)).unwrap();
        let submission = format!("{:x}", Sha256::digest(format!("submission:{result_id}").as_bytes()));
        let policy_digest = format!("{:x}", Sha256::digest(b"{}"));
        // These unit receipts model verifier output, but retain exact contract
        // and submission parents so applicability checks can follow lineage.
        // Other integration/ownership parents remain synthetic in this fixture.
        db.connection
            .execute_batch("PRAGMA foreign_keys=OFF;")
            .unwrap();
        db.connection.execute("INSERT INTO result_submissions(submission_id,project_store,idempotency_key,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,memory_snapshot_id,artifact_manifest,claimed_checks,created_unix_ms) VALUES(?1,'/tmp/project',?1,?2,'{}',?3,?4,?5,?6,'/tmp/repo',?7,?7,'sha1',NULL,'[]','[]',?8)", params![submission,digest,task_id,contract_revision,contract_digest,attempt,oid,created_unix_ms]).unwrap();
        db.connection
            .execute(
                "INSERT INTO verification_runs(run_id,project_store,idempotency_key,payload_digest,submission_id,task_id,contract_revision,contract_digest,attempt_id,policy_id,policy_digest,commit_oid,tree_oid,object_format,memory_fence,isolation,argv,library_manifest,state,reason,exit_status,receipt_digest,store_device,store_inode,created_unix_ms) VALUES(?1,'/tmp/project',?1,?2,?9,?3,?6,?8,?4,'policy',?10,?5,?5,'sha1',0,'linux-unshare-user-pid-mount-v1','[]','[]','accepted',NULL,0,?2,0,0,?7)",
                params![
                    result_id,
                    digest,
                    task_id,
                    attempt,
                    oid,
                    contract_revision,
                    created_unix_ms,
                    contract_digest,
                    submission,
                    policy_digest
                ],
            )
            .unwrap();
        db.connection
            .execute(
                "INSERT INTO verified_results(result_id,run_id,submission_id,commit_oid,tree_oid,object_format,policy_digest,receipt_digest,isolation,memory_fence,created_unix_ms) VALUES(?1,?1,?5,?3,?3,'sha1',?6,?2,'linux-unshare-user-pid-mount-v1',0,?4)",
                params![result_id, digest, oid, created_unix_ms, submission, policy_digest],
            )
            .unwrap();
        db.connection.execute("INSERT INTO verification_contract_checks VALUES(?1,2)",[result_id]).unwrap();
        db.connection
            .execute_batch("PRAGMA foreign_keys=ON;")
            .unwrap();
    }

    #[test]
    fn schema_25_upgrade_preserves_landed_commit_bytes_and_ends_at_34() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.db");
        let mut db = SqliteStore::create(&path).unwrap();
        assert_eq!(user_version(&db.connection), crate::store::SCHEMA);
        db.commit(Commit {
            expected_head: 0,
            mutations: vec![
                task("consumer", TaskState::Draft),
                task("pred", TaskState::Draft),
            ],
        })
        .unwrap();
        for (consumer, predecessor, requirement) in [
            ("consumer", "pred", "landed_commit"),
            ("pred", "consumer", "verified_result"),
        ] {
            db.connection
                .execute(
                    "INSERT INTO task_dependencies(task_id,predecessor_id,requirement) VALUES(?1,?2,?3)",
                    params![consumer, predecessor, requirement],
                )
                .unwrap();
        }
        let preserved = dependencies(&db.connection);
        assert!(preserved.iter().any(|row| row.2 == "landed_commit"));
        drop(db);
        let raw = rusqlite::Connection::open(&path).unwrap();
        crate::store::test_schema::historical(&raw, 25)
        .unwrap();
        drop(raw);
        let mut db = SqliteStore::open(&path).unwrap();
        assert_eq!(user_version(&db.connection), 25);
        let old_check: String = db
            .connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='task_dependencies'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!old_check.contains("integrated_commit"));
        assert_eq!(dependencies(&db.connection), preserved);
        assert!(matches!(
            db.import_legacy(&"cd".repeat(32), &[], &[]),
            Err(StoreError::UnsupportedSchema(25))
        ));
        assert_eq!(user_version(&db.connection), 25);
        db.upgrade_v1().unwrap();
        assert_eq!(user_version(&db.connection), crate::store::SCHEMA);
        assert_eq!(dependencies(&db.connection), preserved);
        assert!(
            preserved
                .iter()
                .any(|row| row.0 == "consumer" && row.1 == "pred" && row.2 == "landed_commit")
        );
        let check_sql: String = db
            .connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='task_dependencies'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(check_sql.contains("integrated_commit"));
        assert!(check_sql.contains("landed_commit"));
        let admission: String = db
            .connection
            .query_row(
                "SELECT factory_admission FROM project_control WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(admission, "off");
        let policies: i64 = db
            .connection
            .query_row(
                "SELECT count(*) FROM factory_admission_policies",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(policies, 0);
        let satisfactions: i64 = db
            .connection
            .query_row("SELECT count(*) FROM dependency_satisfactions", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(satisfactions, 0);
        check_schema(&db.connection).unwrap();
        drop(db);
        let reopened = SqliteStore::open(&path).unwrap();
        assert_eq!(user_version(&reopened.connection), crate::store::SCHEMA);
    }
}

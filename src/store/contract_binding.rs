//! Validate the authoritative signed contract at each admission boundary.
//! Raw signed bytes remain the source of truth, including policy-bound edges.
use super::*;

pub(super) fn latest(db: &Connection, task: &str) -> Result<Option<PreparedContract>> {latest_with_budget(db,task,None)}
pub(super) fn latest_with_budget(db: &Connection, task: &str, budget:Option<&read_budget::ReadBudget>) -> Result<Option<PreparedContract>> {
    let version: u32 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 26 {
        return Ok(None);
    }
    let mut stmt=db.prepare("SELECT raw_bytes,raw_digest,contract_revision,project_store FROM task_contracts WHERE task_id=?1 ORDER BY contract_revision DESC LIMIT 1")?;
    let mut rows=stmt.query([task])?;
    let row:Option<(Vec<u8>,String,u64,String)>=if let Some(row)=rows.next()? {
        if let Some(budget)=budget {budget.row(row,&[(0,2)])?;}
        Some((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))
    } else {None};
    row.map(|(raw, digest, revision, project_store)| {
        let contract = PreparedContract::parse_verified(&raw).map_err(StoreError::Corrupt)?;
        if contract.digest != digest
            || contract.task_id.as_str() != task
            || contract.contract_revision != revision
            || contract.project_store != project_store
        {
            return Err(StoreError::Corrupt("task contract binding mismatch".into()));
        }
        Ok(contract)
    })
    .transpose()
}

/// Validate only the reachable graph. Uncontracted tasks retain their queue
/// prerequisites; a signed contract is authoritative once installed.
/// Share the planning graph's inventory bounds, with an additional byte/time cap.
pub(super) fn validate_dependency_graph(db: &Connection, proposed: &PreparedContract) -> Result<()> {
    use std::collections::{BTreeMap, BTreeSet, VecDeque};
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let root = proposed.task_id.as_str().to_string();
    let mut pending = VecDeque::from([root.clone()]);
    let mut graph = BTreeMap::<String, Vec<String>>::new();
    let mut bytes = 0usize;
    let mut edges = 0usize;
    while let Some(task) = pending.pop_front() {
        if std::time::Instant::now() >= deadline { return Err(StoreError::Limit("contract dependency validation deadline".into())); }
        if graph.contains_key(&task) { continue; }
        if graph.len() == 10_000 { return Err(StoreError::Limit("contract dependency graph exceeds 10000 tasks".into())); }
        let dependencies = if task == root {
            proposed.dependencies.iter().map(|edge| edge.predecessor.as_str().to_string()).collect()
        } else {
            let row: Option<(Option<Vec<u8>>,String)> = db.query_row(
                "SELECT CASE WHEN length(raw_bytes)<=65536 THEN raw_bytes END,raw_digest FROM task_contracts WHERE task_id=?1 ORDER BY contract_revision DESC LIMIT 1",
                [&task], |row| Ok((row.get(0)?,row.get(1)?)),
            ).optional()?;
            if let Some((raw,digest)) = row {
                let raw = raw.ok_or_else(|| StoreError::Limit("contract exceeds 64 KiB".into()))?;
                bytes += raw.len();
                if bytes > 32 * 1024 * 1024 { return Err(StoreError::Limit("contract dependency graph exceeds 32 MiB".into())); }
                let contract = PreparedContract::parse_verified(&raw).map_err(StoreError::Corrupt)?;
                if contract.digest != digest || contract.task_id.as_str() != task || contract.project_store != proposed.project_store {
                    return Err(StoreError::Corrupt("dependency contract binding mismatch".into()));
                }
                contract.dependencies.iter().map(|edge| edge.predecessor.as_str().to_string()).collect()
            } else {
                let mut query = db.prepare("SELECT predecessor_id FROM task_dependencies WHERE task_id=?1 ORDER BY predecessor_id LIMIT 257")?;
                let dependencies = query.query_map([&task], |row| row.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?;
                if dependencies.len() > 256 { return Err(StoreError::Limit("contract dependency task exceeds 256 edges".into())); }
                dependencies
            }
        };
        edges += dependencies.len();
        if edges > 100_000 { return Err(StoreError::Limit("contract dependency graph exceeds 100000 edges".into())); }
        pending.extend(dependencies.iter().cloned());
        graph.insert(task, dependencies);
    }
    // Iterative DFS avoids a call-stack dependency on graph depth.
    let mut active = BTreeSet::new();
    let mut done = BTreeSet::new();
    let mut stack = vec![(root, false)];
    while let Some((task, leaving)) = stack.pop() {
        if std::time::Instant::now() >= deadline { return Err(StoreError::Limit("contract dependency validation deadline".into())); }
        if leaving { active.remove(&task); done.insert(task); continue; }
        if done.contains(&task) { continue; }
        if !active.insert(task.clone()) { return Err(StoreError::Invalid("contract dependency cycle".into())); }
        stack.push((task.clone(), true));
        stack.extend(graph[&task].iter().map(|dependency| (dependency.clone(), false)));
    }
    Ok(())
}

fn reference(contract: &PreparedContract) -> VersionedReference {
    VersionedReference {
        id: contract.task_id.as_str().into(),
        revision: contract.contract_revision,
        digest: contract.digest.clone(),
    }
}

/// Exact prerequisite extracted from a result's retained launch contract.
pub(super) struct ResultBarrier {
    pub reference: BarrierReleaseReference,
    pub authority: VersionedReference,
    pub config: Option<String>,
}

/// A verifier checked the worker tree; only a native integration check proves
/// required outputs survived the merge. Old terminal records remain historical.
pub(super) fn integrated_output_checks_current(db: &Connection, integrated: &str, budget: Option<&read_budget::ReadBudget>) -> Result<bool> {
    let (raw,digest,task,revision): (Option<Vec<u8>>,String,String,u64) = read_budget::one(db,
        "SELECT CASE WHEN length(c.raw_bytes)<=65536 THEN c.raw_bytes END,c.raw_digest,c.task_id,c.contract_revision
         FROM integrated_commits i JOIN integration_operations o ON o.operation_id=i.operation_id
         JOIN verified_results r ON r.result_id=o.verified_result_id
         JOIN verification_runs v ON v.run_id=r.run_id AND v.submission_id=r.submission_id
         JOIN task_contracts c ON c.task_id=v.task_id AND c.contract_revision=v.contract_revision AND c.raw_digest=v.contract_digest
         WHERE i.integrated_id=?1", [integrated], budget, &[(0,2)],
         |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)))?;
    let raw = raw.ok_or_else(|| StoreError::Limit("integration contract exceeds 64 KiB".into()))?;
    let contract = PreparedContract::parse_verified(&raw).map_err(StoreError::Corrupt)?;
    if contract.digest != digest || contract.task_id.as_str() != task || contract.contract_revision != revision {
        return Err(StoreError::Corrupt("integration contract binding mismatch".into()));
    }
    if contract.required_outputs.is_empty() { return Ok(true); }
    let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < 43 { return Ok(false); }
    read_budget::one(db,
        "SELECT EXISTS(SELECT 1 FROM integration_contract_checks WHERE integrated_id=?1 AND version=1)",
        [integrated], budget, &[], |row| row.get(0))
}

pub(super) fn require_verified_result_barrier(db: &Connection, result: &str, now: i64) -> Result<()> {
    require_verified_result_barrier_with_budget(db, result, now, None)
}

pub(super) fn verified_result_barrier_current(db: &Connection, result: &str, budget: Option<&read_budget::ReadBudget>) -> Result<bool> {
    if let Some(budget) = budget { budget.check()?; }
    let exists: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM verified_results WHERE result_id=?1)", [result], |row| row.get(0))?;
    if !exists { return Ok(false); }
    match require_verified_result_barrier_with_budget(db, result, jiff::Timestamp::now().as_millisecond(), budget) {
        Ok(()) => Ok(true),
        Err(StoreError::Invalid(_) | StoreError::Conflict) => Ok(false),
        Err(error) => Err(error),
    }
}

pub(super) fn require_verified_result_barrier_with_budget(db: &Connection, result: &str, now: i64, budget: Option<&read_budget::ReadBudget>) -> Result<()> {
    verified_result_barrier_valid_until(db,result,now,budget).map(|_|())
}

pub(super) fn verified_result_barrier_valid_until(db: &Connection, result: &str, now: i64, budget: Option<&read_budget::ReadBudget>) -> Result<Option<i64>> {
    let mut query = db.prepare("SELECT s.task_id,s.contract_revision,s.contract_digest,s.attempt_id FROM verified_results r
         JOIN verification_runs v ON v.run_id=r.run_id AND v.submission_id=r.submission_id
         JOIN result_submissions s ON s.submission_id=r.submission_id AND s.task_id=v.task_id
           AND s.contract_revision=v.contract_revision AND s.contract_digest=v.contract_digest AND s.attempt_id=v.attempt_id
         WHERE r.result_id=?1")?;
    let mut rows = query.query([result])?;
    let row = rows.next()?.ok_or_else(|| StoreError::Corrupt("verified result lineage is missing".into()))?;
    if let Some(budget) = budget { budget.row(row, &[])?; }
    let (task, revision, digest, attempt): (String,u64,String,String) = (row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?);
    // Every receipt re-use needs fresh post-execution identity proof, including
    // contracts without explicit scope/output declarations. Version 1 predates
    // these checks; migration and historical replay cannot upgrade its assurance.
    let version:u32=db.query_row("PRAGMA user_version",[],|row|row.get(0))?;
    let attested=version>=43 && read_budget::one(db,
        "SELECT EXISTS(SELECT 1 FROM verification_contract_checks WHERE result_id=?1 AND version=2)",
        [result],budget,&[],|row|row.get::<_,bool>(0))?;
    if !attested {
        return Err(StoreError::Invalid("verified result requires fresh post-execution verification".into()));
    }
    if let Some(budget) = budget { budget.check()?; }
    if let Some(required) = result_barrier(db, &task, revision, &digest, &attempt, budget)? {
        return super::barriers::require_current_release_valid_until(db, &required.reference, &required.authority, required.config.as_deref(), now, budget);
    }
    Ok(None)
}

/// Reusing a result is a new authority boundary, even when its receipt is old.
/// Version-2 contracts must be the exact contract reserved by this attempt.
pub(super) fn require_result_barrier(db: &Connection, task: &str, revision: u64, digest: &str, attempt: &str, now: i64) -> Result<()> {
    if let Some(required) = result_barrier(db, task, revision, digest, attempt, None)? {
        super::barriers::require_current_release(db, &required.reference, &required.authority, required.config.as_deref(), now, None)?;
    }
    Ok(())
}

pub(super) fn result_barrier(db: &Connection, task: &str, revision: u64, digest: &str, attempt: &str, budget: Option<&read_budget::ReadBudget>) -> Result<Option<ResultBarrier>> {
    if let Some(budget) = budget { budget.check()?; }
    let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let reserved_barrier = if version >= 43 {
        let (required, revoked): (bool, bool) = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM attempt_required_releases WHERE attempt_id=?1),EXISTS(SELECT 1 FROM attempt_barrier_invalidations WHERE attempt_id=?1)",
            [attempt], |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if revoked { return Err(StoreError::Invalid("attempt's required barrier was revoked".into())); }
        required
    } else { false };
    let mut query = db.prepare("SELECT CASE WHEN length(raw_bytes)<=65536 THEN raw_bytes END,raw_digest FROM task_contracts WHERE task_id=?1 AND contract_revision=?2")?;
    let mut rows = query.query(params![task,revision])?;
    let row = rows.next()?.ok_or_else(|| StoreError::Corrupt("result task contract is missing".into()))?;
    if let Some(budget) = budget { budget.row(row, &[(0,2)])?; }
    let (raw, stored_digest): (Option<Vec<u8>>, String) = (row.get(0)?, row.get(1)?);
    let raw = raw.ok_or_else(|| StoreError::Limit("task contract exceeds 64 KiB".into()))?;
    let contract = PreparedContract::parse_verified(&raw).map_err(StoreError::Corrupt)?;
    if contract.digest != stored_digest || contract.digest != digest || contract.task_id.as_str() != task || contract.contract_revision != revision {
        return Err(StoreError::Corrupt("result task contract binding mismatch".into()));
    }
    // A result remains bound to the contract frozen by reservation, even when
    // neither contract requires a barrier. Old manual attempts without a frozen
    // contract keep their historical ingestion semantics.
    let mut query = db.prepare("SELECT CASE WHEN length(payload)<=?2 THEN payload END,payload_hash,operation_id FROM attempt_inputs WHERE attempt_id=?1")?;
    let mut rows = query.query(params![attempt,MAX_RECORD_BYTES])?;
    let record = if let Some(row) = rows.next()? {
        if let Some(budget) = budget { budget.row(row, &[(0,2)])?; }
        let (payload, hash, operation): (Option<String>, String, String) = (row.get(0)?,row.get(1)?,row.get(2)?);
        let payload = payload.ok_or_else(|| StoreError::Limit("attempt inputs exceed record limit".into()))?;
        if format!("{:x}", Sha256::digest(payload.as_bytes())) != hash {
            return Err(StoreError::Corrupt("attempt inputs digest mismatch".into()));
        }
        let record: AttemptInputRecord = serde_json::from_str(&payload).map_err(|e| StoreError::Corrupt(e.to_string()))?;
        if record.attempt.as_str() != attempt || record.operation.as_str() != operation
            || record.inputs.task.as_str() != task || record.inputs.project_store != contract.project_store {
            return Err(StoreError::Invalid("result differs from its reserved attempt".into()));
        }
        if let Some(frozen) = &record.inputs.task_contract {
            if record.inputs.version != 2 || frozen != &reference(&contract) {
                return Err(StoreError::Invalid("result contract differs from its reservation".into()));
            }
        }
        Some(record)
    } else { None };
    let Some(barrier) = &contract.required_barrier else {
        if reserved_barrier {
            return Err(StoreError::Invalid("result contract omits the attempt's reserved barrier".into()));
        }
        if let Some(budget) = budget { budget.check()?; }
        return Ok(None);
    };
    let latest: u64 = db.query_row("SELECT max(contract_revision) FROM task_contracts WHERE task_id=?1", [task], |row| row.get(0))?;
    if latest != revision { return Err(StoreError::Invalid("required barrier contract is no longer current".into())); }
    let record = record.ok_or_else(|| StoreError::Invalid("required barrier result lacks reserved launch inputs".into()))?;
    if record.inputs.version != 2 || record.inputs.task_contract != Some(reference(&contract)) {
        return Err(StoreError::Invalid("required barrier result differs from its reservation".into()));
    }
    if crate::migration::config_reference(Path::new(&record.inputs.config.path)).map_err(|e| StoreError::Invalid(e.to_string()))? != record.inputs.config {
        return Err(StoreError::Invalid("required barrier configuration changed".into()));
    }
    if let Some(budget) = budget { budget.check()?; }
    Ok(Some(ResultBarrier { reference: barrier.clone(), authority: contract.authority, config: record.inputs.config.digest }))
}

pub(super) fn queue_matches(db: &Connection, task: &str) -> Result<bool> {queue_matches_with_budget(db,task,None)}
pub(super) fn queue_matches_with_budget(db: &Connection, task: &str,budget:Option<&read_budget::ReadBudget>) -> Result<bool> {
    let Some(contract) = latest_with_budget(db, task,budget)? else {
        return Ok(true);
    };
    let mut stmt = db.prepare("SELECT predecessor_id,requirement FROM task_dependencies WHERE task_id=?1 ORDER BY predecessor_id LIMIT 257")?;
    let mut rows=stmt.query([task])?;let mut actual=Vec::new();
    while let Some(row)=rows.next()? {
        if actual.len()==256 {return Err(StoreError::Limit("task dependency limit exceeded".into()));}
        if let Some(budget)=budget {budget.row(row,&[])?;}
        actual.push((row.get::<_,String>(0)?,row.get::<_,String>(1)?));
    }
    let mut expected = contract
        .dependencies
        .iter()
        .map(|d| (d.predecessor.as_str().to_string(), d.edge.clone()))
        .collect::<Vec<_>>();
    expected.sort();
    Ok(actual == expected)
}

pub(super) fn validate_with_budget(db: &Connection, inputs: &LaunchInputs, now: i64,budget:Option<&read_budget::ReadBudget>) -> Result<()> {
    let contract = latest_with_budget(db, inputs.task.as_str(),budget)?;
    if contract.as_ref().map(reference) != inputs.task_contract {
        return Err(StoreError::Invalid(
            "task contract changed since launch preparation".into(),
        ));
    }
    if let Some(contract) = &contract {
        if let Some(reference) = &contract.required_barrier {
            super::barriers::require_current_release(db, reference, &contract.authority, inputs.config.digest.as_deref(), now, budget)?;
        }
        let profile = inputs
            .effective_profile
            .as_ref()
            .ok_or_else(|| StoreError::Invalid("contract requires an effective profile".into()))?;
        if !super::capabilities::profile_satisfies_contract_with_budget(db, contract, profile, now,budget)? {
            return Err(StoreError::Invalid(
                "profile does not satisfy task contract capabilities".into(),
            ));
        }
    }
    if !queue_matches_with_budget(db, inputs.task.as_str(),budget)? {
        return Err(StoreError::Invalid(
            "queue dependencies differ from signed task contract".into(),
        ));
    }
    Ok(())
}

/// Compare both the policy name and the digest of its exact signed body.
pub(super) fn policy_matches(
    db: &Connection,
    consumer: &str,
    predecessor: &str,
    requirement: &str,
    result: &str,
) -> Result<bool> {
    policy_matches_with_budget(db,consumer,predecessor,requirement,result,None)
}
pub(super) fn policy_matches_with_budget(db:&Connection,consumer:&str,predecessor:&str,requirement:&str,result:&str,budget:Option<&read_budget::ReadBudget>)->Result<bool> {
    let Some(contract) = latest_with_budget(db, consumer,budget)? else {
        return Ok(true);
    };
    let Some(edge) = contract
        .dependencies
        .iter()
        .find(|d| d.predecessor.as_str() == predecessor && d.edge == requirement)
    else {
        return Ok(false);
    };
    let digest = if let Some(digest) = &edge.policy_digest {
        digest.clone()
    } else {
        let policy = contract.acceptance_policies.iter().find(|p| p.id == edge.policy_id)
            .ok_or_else(|| StoreError::Corrupt("contract dependency policy missing".into()))?;
        format!("{:x}", Sha256::digest(policy.text.as_bytes()))
    };
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM verified_results v JOIN verification_runs r ON r.run_id=v.run_id WHERE v.result_id=?1 AND r.task_id=?2 AND r.policy_id=?3 AND r.policy_digest=?4 AND v.policy_digest=r.policy_digest)",
        params![result, predecessor, edge.policy_id, digest], |r| r.get(0),
    ).map_err(StoreError::from)
}

impl SqliteStore {
    pub(crate) fn admission_contract(&self,task:&str,budget:Option<&read_budget::ReadBudget>)->Result<Option<PreparedContract>> {
        latest_with_budget(&self.connection,task,budget)
    }
    pub(crate) fn admission_profile_matches_contract(&self,contract:Option<&PreparedContract>,profile:&FrozenProfile,now:i64,budget:Option<&read_budget::ReadBudget>)->Result<bool> {
        match contract {
            Some(contract)=>super::capabilities::profile_satisfies_contract_with_budget(&self.connection,contract,profile,now,budget),
            None=>Ok(true),
        }
    }
    /// The contract a reservation froze, or the latest one for an unfrozen attempt.
    pub(crate) fn attempt_contract(&self, task: &str, frozen: Option<&VersionedReference>) -> Result<Option<PreparedContract>> {
        let Some(frozen) = frozen else { return latest(&self.connection, task); };
        let raw: Vec<u8> = self.connection.query_row("SELECT raw_bytes FROM task_contracts WHERE task_id=?1 AND contract_revision=?2",
            params![task, frozen.revision], |row| row.get(0))?;
        let contract = PreparedContract::parse_verified(&raw).map_err(StoreError::Corrupt)?;
        if contract.digest != frozen.digest || contract.task_id.as_str() != task || contract.contract_revision != frozen.revision {
            return Err(StoreError::Corrupt("frozen task contract binding mismatch".into()));
        }
        Ok(Some(contract))
    }
    pub(crate) fn task_contract_reference(&self, task: &str) -> Result<Option<VersionedReference>> {
        Ok(latest(&self.connection, task)?.as_ref().map(reference))
    }
}

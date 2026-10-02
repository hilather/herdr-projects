//! Durable automatic verification jobs. An enqueued job grants no execution
//! authority and is not verification evidence; only a recorded verification run
//! under the job key can confirm its delivery.
use super::*;
use serde::Serialize;

const TURN_LIMIT: usize = 8;

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ResultAutomationControl { pub revision: u64, pub verify: bool, pub integrate: bool }
#[derive(Debug, Default, Serialize)]
pub struct VerificationJobTurn { pub pending: bool, pub enqueued: usize }
#[derive(Debug, Serialize)]
/// An automatic verification or integration job (`kind`); integration jobs have no policy.
pub struct VerificationJob { pub operation: OperationId, pub kind: String, pub submission_id: String, pub policy_id: Option<String>, pub delivery: crate::operations::Delivery, pub paused: Option<String> }

/// Sealed confirmation: the evidence must name a run recorded under this job's
/// key, submission, policy and store. Caller- or worker-written text cannot.
pub(super) fn has_run(db: &Connection, id: &OperationId, run_id: &str) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM verification_runs r JOIN operations o ON o.id=?1
        WHERE r.run_id=?2 AND r.idempotency_key=o.idempotency_key AND r.project_store=json_extract(o.payload,'$.project_store')
          AND r.submission_id=json_extract(o.payload,'$.submission_id') AND r.policy_id=json_extract(o.payload,'$.policy_id'))",
        params![id.as_str(), run_id], |row| row.get(0))?)
}
/// The pause reason stands until the job is next claimed, finished or reset.
const LATEST: &str = "SELECT kind,payload FROM events WHERE entity=?1 AND kind IN ('verification.paused','integration.paused','verification.job_reset','integration.job_reset','operation.claimed','operation.outcome') ORDER BY sequence DESC LIMIT 1";
fn paused(db: &Connection, id: &OperationId) -> Result<Option<String>> {
    let latest: Option<(String, String)> = db.query_row(LATEST, [id.as_str()], |row| Ok((row.get(0)?, row.get(1)?))).optional()?;
    Ok(latest.filter(|(kind, _)| kind.ends_with(".paused"))
        .and_then(|(_, payload)| serde_json::from_str::<serde_json::Value>(&payload).ok()?["reason"].as_str().map(str::to_owned)))
}

struct Candidate {
    submission_id: String,
    project_store: String,
    task_id: String,
    task_revision: i64,
    contract_revision: i64,
    policy_id: String,
    body: String,
}

/// A pending job whose task revision fence no longer holds can never be
/// claimed. Retire it durably (reason `task_revision_changed`) so the producer
/// can enqueue a job fenced on the current revision. Claimed and ambiguous jobs
/// are never retired here; they go through observation first. Returns whether
/// more stale jobs remain than one turn retires.
pub(super) fn retire_stale(tx: &Connection, lane: &str, budget: &read_budget::ReadBudget, now: i64) -> Result<bool> {
    let stale = {
        let mut stmt = tx.prepare("SELECT o.id,o.expected_revision,t.revision FROM operation_delivery d JOIN operations o ON o.id=d.operation_id JOIN tasks t ON t.id=o.task_id
            WHERE d.state='pending' AND o.kind=?1 AND o.expected_revision<>t.revision ORDER BY o.id LIMIT ?2")?;
        let mut rows = stmt.query(params![format!("{lane}.run"), TURN_LIMIT as i64 + 1])?;let mut stale = Vec::new();
        while let Some(row) = rows.next()? { budget.row(row, &[])?; stale.push((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)); }
        stale
    };
    for (id, bound, current) in stale.iter().take(TURN_LIMIT) {
        budget.check()?;
        let id = OperationId::new(id.clone()).map_err(StoreError::Corrupt)?;
        let outcome = crate::operations::Outcome::PermanentFailure { diagnostic: format!("task_revision_changed: bound to task revision {bound}, task is now at {current}; a job for the current revision replaces it") };
        let retired = super::delivery::update_outcome(tx, &super::delivery::delivery(tx, &id)?, &outcome, now, "result-automation")?;
        tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES(?4,?1,?2,1,?3)", params![id.as_str(), integer(retired.revision)?,
            serde_json::json!({"reason":"task_revision_changed","bound_revision":bound,"task_revision":current}).to_string(), format!("{lane}.job_retired")])?;
        if lane == "integration" {
            tx.execute("INSERT OR IGNORE INTO pending_integration_work SELECT s.submission_id,s.created_unix_ms FROM result_submissions s JOIN operations o ON o.id=?1
                WHERE s.submission_id=json_extract(o.payload,'$.submission_id')", [id.as_str()])?;
        }
    }
    Ok(stale.len() > TURN_LIMIT)
}
/// The job id for a pair; a replacement for a retired job also binds the task revision.
pub(super) fn fresh_id(tx: &Connection, parts: serde_json::Value, task_revision: i64) -> Result<OperationId> {
    let encode = |value: &serde_json::Value| -> Result<OperationId> {
        let encoded = serde_json::to_vec(value).map_err(|error| StoreError::Invalid(error.to_string()))?;
        OperationId::new(sha256_hex(&encoded)).map_err(StoreError::Invalid)
    };
    let base = encode(&parts)?;
    if !tx.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1)", [base.as_str()], |row| row.get::<_, bool>(0))? { return Ok(base); }
    let mut parts = parts;
    if let Some(array) = parts.as_array_mut() { array.push(task_revision.into()); }
    encode(&parts)
}

fn version(db: &Connection) -> Result<u32> { Ok(db.query_row("PRAGMA user_version", [], |row| row.get(0))?) }
fn enabled(db: &Connection) -> Result<bool> {
    Ok(db.query_row("SELECT verify=1 AND EXISTS(SELECT 1 FROM project_control WHERE singleton=1 AND state='active') FROM result_automation_control WHERE singleton=1", [], |row| row.get(0))?)
}
fn sha256_hex(bytes: &[u8]) -> String { format!("{:x}", Sha256::digest(bytes)) }
/// sha256(project_store, submission, contract_revision, policy_id, policy_digest),
/// plus the task revision for a job replacing a retired one.
fn identity(tx: &Connection, candidate: &Candidate, policy_digest: &str) -> Result<OperationId> {
    fresh_id(tx, serde_json::json!([candidate.project_store, candidate.submission_id, candidate.contract_revision, candidate.policy_id, policy_digest]), candidate.task_revision)
}

impl SqliteStore {
    pub fn result_automation(&self) -> Result<ResultAutomationControl> {
        let schema=version(&self.connection)?;
        if schema<44 {return Ok(ResultAutomationControl {revision:0,verify:false,integrate:false});}
        let sql=if schema>=45 {"SELECT revision,verify,integrate FROM result_automation_control WHERE singleton=1"}
            else {"SELECT revision,verify,0 FROM result_automation_control WHERE singleton=1"};
        Ok(self.connection.query_row(sql, [],
            |row| Ok(ResultAutomationControl { revision:row.get(0)?,verify:row.get(1)?,integrate:row.get(2)? }))?)
    }

    /// `None` leaves a switch unchanged. The integration switch needs schema 45.
    pub fn set_result_automation(&mut self, expected_head: u64, verify: Option<bool>, integrate: Option<bool>) -> Result<ResultAutomationControl> {
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;check_schema(&tx)?;
        let schema = version(&tx)?;
        if schema < 44 || (integrate.is_some() && schema < 45) { return Err(StoreError::UnsupportedSchema(schema)); }
        if head(&tx)? != expected_head { return Err(StoreError::Conflict); }
        let (revision, old_verify): (u64, bool) = tx.query_row("SELECT revision,verify FROM result_automation_control WHERE singleton=1", [], |row| Ok((row.get(0)?, row.get(1)?)))?;
        let old_integrate: bool = schema >= 45 && tx.query_row("SELECT integrate FROM result_automation_control WHERE singleton=1", [], |row| row.get(0))?;
        let (verify, integrate) = (verify.unwrap_or(old_verify), integrate.unwrap_or(old_integrate));
        let revision = if (old_verify, old_integrate) == (verify, integrate) { revision } else {
            let next = revision.checked_add(1).ok_or(StoreError::Conflict)?;
            tx.execute("UPDATE result_automation_control SET revision=?1,verify=?2 WHERE singleton=1", params![integer(next)?, verify])?;
            if schema >= 45 { tx.execute("UPDATE result_automation_control SET integrate=?1 WHERE singleton=1", [integrate])?; }
            tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('result.automation_changed','project',?1,1,?2)", params![integer(next)?, serde_json::json!({"verify":verify,"integrate":integrate}).to_string()])?;
            next
        };
        tx.commit()?;Ok(ResultAutomationControl { revision, verify, integrate })
    }
    /// At most `TURN_LIMIT` new jobs per turn. Each insert rechecks the contract
    /// revision, the exact policy body and the task revision fence it records.
    /// Pending jobs whose fence no longer holds are retired first and replaced.
    pub(crate) fn service_verification_jobs(&mut self, budget: &read_budget::ReadBudget) -> Result<VerificationJobTurn> {
        budget.check()?;
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;check_schema(&tx)?;
        if version(&tx)? < 44 || !enabled(&tx)? { return Ok(VerificationJobTurn::default()); }
        let now = jiff::Timestamp::now().as_millisecond();
        let stale = version(&tx)? >= 47 && retire_stale(&tx, "verification", budget, now)?;
        let mut candidates = Vec::new();
        {
            let mut stmt = tx.prepare(
                "SELECT s.submission_id,s.project_store,s.task_id,t.revision,s.contract_revision,p.policy_id,p.body
                 FROM pending_verification_work w
                 JOIN result_submissions s ON s.submission_id=w.submission_id
                 JOIN tasks t ON t.id=s.task_id
                 JOIN acceptance_policies p ON p.task_id=s.task_id AND p.contract_revision=s.contract_revision
                 WHERE NOT EXISTS(SELECT 1 FROM verification_runs r WHERE r.submission_id=s.submission_id AND r.policy_id=p.policy_id)
                   AND NOT EXISTS(SELECT 1 FROM operations o WHERE o.kind='verification.run'
                       AND json_extract(o.payload,'$.submission_id')=s.submission_id AND json_extract(o.payload,'$.policy_id')=p.policy_id
                       AND NOT EXISTS(SELECT 1 FROM events e WHERE e.entity=o.id AND e.kind='verification.job_retired'))
                 ORDER BY w.created_unix_ms,w.submission_id,p.policy_id LIMIT ?1")?;
            let mut rows = stmt.query([TURN_LIMIT as i64 + 1])?;
            while let Some(row) = rows.next()? {
                budget.row(row, &[])?;
                candidates.push(Candidate { submission_id: row.get(0)?, project_store: row.get(1)?, task_id: row.get(2)?, task_revision: row.get(3)?, contract_revision: row.get(4)?, policy_id: row.get(5)?, body: row.get(6)? });
            }
        }
        let more = stale || candidates.len() > TURN_LIMIT;
        let mut enqueued = 0;
        for candidate in candidates.iter().take(TURN_LIMIT) {
            budget.check()?;
            let policy_digest = sha256_hex(candidate.body.as_bytes());
            let id = identity(&tx, candidate, &policy_digest)?;
            let payload = serde_json::json!({"version":1,"project_store":candidate.project_store,"submission_id":candidate.submission_id,"task_id":candidate.task_id,
                "contract_revision":candidate.contract_revision,"policy_id":candidate.policy_id,"policy_digest":policy_digest}).to_string();
            enqueued += tx.execute(
                "INSERT INTO operations(id,task_id,kind,target,payload_version,payload,payload_hash,expected_revision,due_unix_ms,idempotency_key)
                 SELECT ?1,t.id,'verification.run',?3,1,?4,?5,t.revision,?6,?1 FROM tasks t
                 WHERE t.id=?2 AND t.revision=?7
                   AND EXISTS(SELECT 1 FROM result_submissions s JOIN task_contracts c ON c.task_id=s.task_id AND c.contract_revision=s.contract_revision
                              WHERE s.submission_id=?8 AND s.task_id=?2 AND s.contract_revision=?9 AND c.raw_digest=s.contract_digest)
                   AND EXISTS(SELECT 1 FROM acceptance_policies p WHERE p.task_id=?2 AND p.contract_revision=?9 AND p.policy_id=?10 AND p.body=?11)
                   AND NOT EXISTS(SELECT 1 FROM verification_runs r WHERE r.submission_id=?8 AND r.policy_id=?10)
                   AND NOT EXISTS(SELECT 1 FROM operations o WHERE o.id=?1)",
                params![id.as_str(), candidate.task_id, format!("verification:{}:{}", candidate.submission_id, candidate.policy_id), payload, sha256_hex(payload.as_bytes()),
                    now, candidate.task_revision, candidate.submission_id, candidate.contract_revision, candidate.policy_id, candidate.body],
            )?;
        }
        budget.check()?;
        tx.commit()?;
        Ok(VerificationJobTurn { pending: more, enqueued })
    }
    /// Records why a pending job was not started (for example, no isolation).
    /// Repeating the same reason appends nothing.
    pub fn note_verification_paused(&mut self, id: &OperationId, reason: &str) -> Result<()> {
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;check_schema(&tx)?;
        let reason: String = reason.chars().take(1024).collect();
        let (revision, kind): (u64, String) = tx.query_row("SELECT d.revision,o.kind FROM operation_delivery d JOIN operations o ON o.id=d.operation_id WHERE o.id=?1 AND o.kind IN ('verification.run','integration.run') AND d.state='pending'", [id.as_str()], |row| Ok((row.get(0)?, row.get(1)?)))?;
        if paused(&tx, id)?.as_deref() != Some(reason.as_str()) {
            let event = if kind == "integration.run" { "integration.paused" } else { "verification.paused" };
            tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES(?4,?1,?2,1,?3)", params![id.as_str(), integer(revision)?, serde_json::json!({"reason":reason}).to_string(), event])?;
        }
        tx.commit()?;Ok(())
    }
    pub fn verification_jobs(&mut self) -> Result<Vec<VerificationJob>> {
        let tx = self.connection.transaction()?;check_schema(&tx)?;
        let ids = {
            let mut stmt = tx.prepare("SELECT id,kind,json_extract(payload,'$.submission_id'),json_extract(payload,'$.policy_id') FROM operations WHERE kind IN ('verification.run','integration.run') ORDER BY kind DESC,id")?;
            stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, Option<String>>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut jobs = Vec::new();
        for (id, kind, submission_id, policy_id) in ids {
            let operation = OperationId::new(id).map_err(StoreError::Corrupt)?;
            let delivery = super::delivery::delivery(&tx, &operation)?;
            let paused = if delivery.state == crate::operations::DeliveryState::Pending { paused(&tx, &operation)? } else { None };
            jobs.push(VerificationJob { operation, kind, submission_id, policy_id, delivery, paused });
        }
        tx.commit()?;Ok(jobs)
    }
    /// Operator retry of a permanently failed job: the same operation (and so
    /// the same run key) returns to pending. Claim history is kept, so the
    /// generic 32-claim bound still applies; an exhausted job is refused.
    pub fn reset_verification_job(&mut self, id: &OperationId, expected_revision: u64, now: i64) -> Result<crate::operations::Delivery> {
        self.reset_result_job(id, "verification", expected_revision, now)
    }
    /// The same for an integration job. The next run rechecks the target from
    /// scratch, so a target that is still moved blocks again untouched.
    pub fn reset_integration_job(&mut self, id: &OperationId, expected_revision: u64, now: i64) -> Result<crate::operations::Delivery> {
        self.reset_result_job(id, "integration", expected_revision, now)
    }
    fn reset_result_job(&mut self, id: &OperationId, lane: &str, expected_revision: u64, now: i64) -> Result<crate::operations::Delivery> {
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;check_schema(&tx)?;
        let kind: String = tx.query_row("SELECT kind FROM operations WHERE id=?1", [id.as_str()], |row| row.get(0))?;
        let old = super::delivery::delivery(&tx, id)?;
        if kind != format!("{lane}.run") || old.revision != expected_revision || old.state != crate::operations::DeliveryState::PermanentFailure { return Err(StoreError::Conflict); }
        if tx.query_row("SELECT EXISTS(SELECT 1 FROM events WHERE entity=?1 AND kind=?2)", params![id.as_str(), format!("{lane}.job_retired")], |row| row.get::<_, bool>(0))? {
            return Err(StoreError::Invalid("this job was bound to an older task revision and has been replaced by a job for the current revision".into()));
        }
        if old.attempts >= 32 {
            let manual = if lane == "integration" { "integrate this result manually with `result integrate`" } else { "verify this policy manually with `result verify`" };
            return Err(StoreError::Invalid(format!("claim history is exhausted; {manual}")));
        }
        let revision = old.revision.checked_add(1).ok_or(StoreError::Conflict)?;
        tx.execute("UPDATE operation_delivery SET revision=?2,state='pending',owner=NULL,lease_until_ms=NULL,next_due_ms=?3 WHERE operation_id=?1", params![id.as_str(), integer(revision)?, now])?;
        tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES(?4,?1,?2,1,?3)",
            params![id.as_str(), integer(revision)?, serde_json::json!({"attempts":old.attempts,"previous":old.last_outcome}).to_string(), format!("{lane}.job_reset")])?;
        let delivery = super::delivery::delivery(&tx, id)?;
        tx.commit()?;Ok(delivery)
    }
}

pub fn set_project_result_automation(project: &Path, expected_head: u64, verify: Option<bool>, integrate: Option<bool>) -> anyhow::Result<ResultAutomationControl> {
    let _guard = crate::migration::runtime_mutation(project)?;
    let control = controlled::ReadControl::new(std::time::Instant::now() + Duration::from_secs(2), Default::default());
    Ok(crate::migration::open_active_scoped(project, control)?.set_result_automation(expected_head, verify, integrate)?)
}
pub fn service_project_verification_jobs(project: &Path) -> anyhow::Result<VerificationJobTurn> {
    let _guard = crate::migration::runtime_mutation(project)?;
    let control = controlled::ReadControl::new(std::time::Instant::now() + Duration::from_secs(2), Default::default());
    Ok(crate::migration::open_active_scoped(project, control)?.service_verification_jobs()?)
}
pub fn project_verification_jobs(project: &Path) -> anyhow::Result<Vec<VerificationJob>> {
    Ok(crate::migration::open_active(project)?.verification_jobs()?)
}
pub fn reset_project_verification_job(project: &Path, id: &str, expected_revision: u64) -> anyhow::Result<crate::operations::Delivery> {
    let _guard = crate::migration::runtime_mutation(project)?;
    let id = OperationId::new(id.to_owned()).map_err(anyhow::Error::msg)?;
    Ok(crate::migration::open_active(project)?.reset_verification_job(&id, expected_revision, jiff::Timestamp::now().as_millisecond())?)
}
pub fn reset_project_integration_job(project: &Path, id: &str, expected_revision: u64) -> anyhow::Result<crate::operations::Delivery> {
    let _guard = crate::migration::runtime_mutation(project)?;
    let id = OperationId::new(id.to_owned()).map_err(anyhow::Error::msg)?;
    Ok(crate::migration::open_active(project)?.reset_integration_job(&id, expected_revision, jiff::Timestamp::now().as_millisecond())?)
}

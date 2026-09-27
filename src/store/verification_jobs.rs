//! Durable automatic verification jobs (enqueue only). An enqueued job grants
//! no execution authority and is not verification evidence; no dispatcher
//! offers `verification.run` yet.
use super::*;
use serde::Serialize;

const TURN_LIMIT: usize = 8;

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ResultAutomationControl { pub revision: u64, pub verify: bool }
#[derive(Debug, Default, Serialize)]
pub struct VerificationJobTurn { pub pending: bool, pub enqueued: usize }

struct Candidate {
    submission_id: String,
    project_store: String,
    task_id: String,
    task_revision: i64,
    contract_revision: i64,
    policy_id: String,
    body: String,
}

fn version(db: &Connection) -> Result<u32> { Ok(db.query_row("PRAGMA user_version", [], |row| row.get(0))?) }
fn enabled(db: &Connection) -> Result<bool> {
    Ok(db.query_row("SELECT verify=1 AND EXISTS(SELECT 1 FROM project_control WHERE singleton=1 AND state='active') FROM result_automation_control WHERE singleton=1", [], |row| row.get(0))?)
}
fn sha256_hex(bytes: &[u8]) -> String { format!("{:x}", Sha256::digest(bytes)) }
/// sha256(project_store, submission, contract_revision, policy_id, policy_digest)
fn identity(candidate: &Candidate, policy_digest: &str) -> Result<OperationId> {
    let encoded = serde_json::to_vec(&serde_json::json!([candidate.project_store, candidate.submission_id, candidate.contract_revision, candidate.policy_id, policy_digest]))
        .map_err(|error| StoreError::Invalid(error.to_string()))?;
    OperationId::new(sha256_hex(&encoded)).map_err(StoreError::Invalid)
}

impl SqliteStore {
    pub fn set_result_automation(&mut self, expected_head: u64, verify: bool) -> Result<ResultAutomationControl> {
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;check_schema(&tx)?;
        let schema = version(&tx)?;
        if schema < 44 { return Err(StoreError::UnsupportedSchema(schema)); }
        if head(&tx)? != expected_head { return Err(StoreError::Conflict); }
        let (revision, old): (u64, bool) = tx.query_row("SELECT revision,verify FROM result_automation_control WHERE singleton=1", [], |row| Ok((row.get(0)?, row.get(1)?)))?;
        let revision = if old == verify { revision } else {
            let next = revision.checked_add(1).ok_or(StoreError::Conflict)?;
            tx.execute("UPDATE result_automation_control SET revision=?1,verify=?2 WHERE singleton=1", params![integer(next)?, verify])?;
            tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('result.automation_changed','project',?1,1,?2)", params![integer(next)?, serde_json::json!({"verify":verify}).to_string()])?;
            next
        };
        tx.commit()?;Ok(ResultAutomationControl { revision, verify })
    }
    /// At most `TURN_LIMIT` new jobs per turn. Each insert rechecks the contract
    /// revision, the exact policy body and the task revision fence it records.
    pub(crate) fn service_verification_jobs(&mut self, budget: &read_budget::ReadBudget) -> Result<VerificationJobTurn> {
        budget.check()?;
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;check_schema(&tx)?;
        if version(&tx)? < 44 || !enabled(&tx)? { return Ok(VerificationJobTurn::default()); }
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
                       AND json_extract(o.payload,'$.submission_id')=s.submission_id AND json_extract(o.payload,'$.policy_id')=p.policy_id)
                 ORDER BY w.created_unix_ms,w.submission_id,p.policy_id LIMIT ?1")?;
            let mut rows = stmt.query([TURN_LIMIT as i64 + 1])?;
            while let Some(row) = rows.next()? {
                budget.row(row, &[])?;
                candidates.push(Candidate { submission_id: row.get(0)?, project_store: row.get(1)?, task_id: row.get(2)?, task_revision: row.get(3)?, contract_revision: row.get(4)?, policy_id: row.get(5)?, body: row.get(6)? });
            }
        }
        let more = candidates.len() > TURN_LIMIT;
        let now = jiff::Timestamp::now().as_millisecond();
        let mut enqueued = 0;
        for candidate in candidates.iter().take(TURN_LIMIT) {
            budget.check()?;
            let policy_digest = sha256_hex(candidate.body.as_bytes());
            let id = identity(candidate, &policy_digest)?;
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
}

pub fn set_project_result_automation(project: &Path, expected_head: u64, verify: bool) -> anyhow::Result<ResultAutomationControl> {
    let _guard = crate::migration::runtime_mutation(project)?;
    let control = controlled::ReadControl::new(std::time::Instant::now() + Duration::from_secs(2), Default::default());
    Ok(crate::migration::open_active_scoped(project, control)?.set_result_automation(expected_head, verify)?)
}
pub fn service_project_verification_jobs(project: &Path) -> anyhow::Result<VerificationJobTurn> {
    let _guard = crate::migration::runtime_mutation(project)?;
    let control = controlled::ReadControl::new(std::time::Instant::now() + Duration::from_secs(2), Default::default());
    Ok(crate::migration::open_active_scoped(project, control)?.service_verification_jobs()?)
}

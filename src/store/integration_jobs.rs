//! Durable automatic integration jobs. An enqueued job grants no authority and
//! is not integration evidence; only an integrated operation under the job key
//! can confirm its delivery.
use super::*;
use serde::Serialize;

const TURN_LIMIT: usize = 8;
/// Pending rows examined per turn; rows waiting on a busy target stay queued.
const SCAN_LIMIT: usize = 32;
const ELIGIBLE: &str = "SELECT s.submission_id,s.project_store,s.task_id,t.revision,s.repository,g.ref_name,
        (SELECT r.result_id FROM verified_results r JOIN verification_runs v ON v.run_id=r.run_id
         JOIN verification_contract_checks k ON k.result_id=r.result_id AND k.version=2 WHERE r.submission_id=s.submission_id AND v.state='accepted' ORDER BY v.policy_id,r.result_id LIMIT 1)
     FROM result_submissions s
     JOIN task_contracts c ON c.task_id=s.task_id AND c.contract_revision=s.contract_revision AND c.raw_digest=s.contract_digest
     JOIN integration_targets g ON g.repository=s.repository
     JOIN tasks t ON t.id=s.task_id
     WHERE s.submission_id=?1 AND c.route='verify_then_integrate'
       AND NOT EXISTS(SELECT 1 FROM pending_verification_work w WHERE w.submission_id=s.submission_id)
       AND EXISTS(SELECT 1 FROM acceptance_policies p WHERE p.task_id=s.task_id AND p.contract_revision=s.contract_revision)
       AND NOT EXISTS(SELECT 1 FROM acceptance_policies p WHERE p.task_id=s.task_id AND p.contract_revision=s.contract_revision
           AND NOT EXISTS(SELECT 1 FROM verification_runs v JOIN verified_results r ON r.run_id=v.run_id
               JOIN verification_contract_checks k ON k.result_id=r.result_id AND k.version=2
               WHERE v.submission_id=s.submission_id AND v.policy_id=p.policy_id AND v.state='accepted'))
       AND NOT EXISTS(SELECT 1 FROM operations o WHERE o.kind='integration.run' AND json_extract(o.payload,'$.submission_id')=s.submission_id)
       AND NOT EXISTS(SELECT 1 FROM verified_results r JOIN integration_operations i ON i.verified_result_id=r.result_id WHERE r.submission_id=s.submission_id)";
const UNFINISHED: &str = "('effect_pending','candidate_prepared','validating','reconciliation_required')";

#[derive(Debug, Default, Serialize)]
pub struct IntegrationJobTurn { pub pending: bool, pub enqueued: usize }

/// Sealed confirmation: the evidence must name an integrated operation whose
/// idempotency key is this job's id.
pub(super) fn has_integration(db: &Connection, id: &OperationId, integration: &str) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM integration_operations i JOIN integrated_commits c ON c.operation_id=i.operation_id
        WHERE i.operation_id=?2 AND i.idempotency_key=?1 AND i.state='integrated')", params![id.as_str(), integration], |row| row.get(0))?)
}

struct Candidate { submission_id: String, project_store: String, task_id: String, task_revision: i64, repository: String, reference: String, result_id: String }

fn enabled(db: &Connection) -> Result<bool> {
    let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    Ok(version >= 45 && db.query_row("SELECT integrate=1 AND EXISTS(SELECT 1 FROM project_control WHERE singleton=1 AND state='active') FROM result_automation_control WHERE singleton=1", [], |row| row.get(0))?)
}
fn sha256_hex(bytes: &[u8]) -> String { format!("{:x}", Sha256::digest(bytes)) }

impl SqliteStore {
    /// Eligible: every acceptance policy of the submission has an accepted run
    /// with a fresh (version 2) contract check, the contract routes to
    /// integration, its repository has a configured target, and nothing has
    /// integrated the submission yet. At most `TURN_LIMIT` new jobs per turn and
    /// at most one unfinished job or integration per (repository, ref),
    /// checked in this same transaction.
    pub(crate) fn service_integration_jobs(&mut self, budget: &read_budget::ReadBudget) -> Result<IntegrationJobTurn> {
        budget.check()?;
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;check_schema(&tx)?;
        if !enabled(&tx)? { return Ok(IntegrationJobTurn::default()); }
        // Only the pending projection is read, oldest first and bounded; each
        // candidate's full eligibility is rechecked here, and a row that is not
        // eligible now is dropped (a later verified result or target re-adds it).
        let ids = {
            let mut stmt = tx.prepare("SELECT submission_id FROM pending_integration_work ORDER BY created_unix_ms,submission_id LIMIT ?1")?;
            let mut rows = stmt.query([SCAN_LIMIT as i64 + 1])?;let mut ids = Vec::new();
            while let Some(row) = rows.next()? { budget.row(row, &[])?; ids.push(row.get::<_, String>(0)?); }
            ids
        };
        let more = ids.len() > SCAN_LIMIT;
        let now = jiff::Timestamp::now().as_millisecond();
        let mut enqueued = 0;
        for submission_id in ids.iter().take(SCAN_LIMIT) {
            if enqueued == TURN_LIMIT { break; }
            budget.check()?;
            let candidate = tx.query_row(ELIGIBLE, [submission_id], |row| Ok(Candidate { submission_id: row.get(0)?, project_store: row.get(1)?, task_id: row.get(2)?, task_revision: row.get(3)?,
                repository: row.get(4)?, reference: row.get(5)?, result_id: row.get(6)? })).optional()?;
            let Some(candidate) = candidate else {
                tx.execute("DELETE FROM pending_integration_work WHERE submission_id=?1", [submission_id])?;
                continue;
            };
            budget.check()?;
            let encoded = serde_json::to_vec(&serde_json::json!([candidate.project_store, candidate.result_id, candidate.repository, candidate.reference]))
                .map_err(|error| StoreError::Invalid(error.to_string()))?;
            let id = OperationId::new(sha256_hex(&encoded)).map_err(StoreError::Invalid)?;
            let payload = serde_json::json!({"version":1,"project_store":candidate.project_store,"submission_id":candidate.submission_id,"task_id":candidate.task_id,
                "result_id":candidate.result_id,"repository":candidate.repository,"ref":candidate.reference}).to_string();
            enqueued += tx.execute(&format!(
                "INSERT INTO operations(id,task_id,kind,target,payload_version,payload,payload_hash,expected_revision,due_unix_ms,idempotency_key)
                 SELECT ?1,t.id,'integration.run',?3,1,?4,?5,t.revision,?6,?1 FROM tasks t
                 WHERE t.id=?2 AND t.revision=?7
                   AND NOT EXISTS(SELECT 1 FROM operations o JOIN operation_delivery d ON d.operation_id=o.id
                       WHERE o.kind='integration.run' AND d.state IN ('pending','claimed','ambiguous')
                         AND json_extract(o.payload,'$.repository')=?8 AND json_extract(o.payload,'$.ref')=?9)
                   AND NOT EXISTS(SELECT 1 FROM integration_operations i WHERE i.repository=?8 AND i.ref_name=?9 AND i.state IN {UNFINISHED})
                   AND NOT EXISTS(SELECT 1 FROM operations o WHERE o.id=?1)"),
                params![id.as_str(), candidate.task_id, format!("integration:{}", candidate.submission_id), payload, sha256_hex(payload.as_bytes()),
                    now, candidate.task_revision, candidate.repository, candidate.reference],
            )?;
        }
        budget.check()?;
        tx.commit()?;
        Ok(IntegrationJobTurn { pending: more, enqueued })
    }
    /// The tip the controller expects on a target: its latest recorded
    /// integration, else the base the submission was verified against. Any
    /// other tip means the target moved outside the controller.
    pub(crate) fn expected_integration_tip(&mut self, repository: &str, reference: &str, submission_id: &str) -> Result<String> {
        let tx = self.connection.transaction()?;check_schema(&tx)?;
        let latest: Option<String> = tx.query_row("SELECT commit_oid FROM integrated_commits WHERE repository=?1 AND ref_name=?2 ORDER BY created_unix_ms DESC,rowid DESC LIMIT 1",
            params![repository, reference], |row| row.get(0)).optional()?;
        let tip = match latest { Some(tip) => tip, None => tx.query_row("SELECT base_oid FROM result_submissions WHERE submission_id=?1", [submission_id], |row| row.get(0))? };
        tx.commit()?;Ok(tip)
    }
}

pub fn service_project_integration_jobs(project: &Path) -> anyhow::Result<IntegrationJobTurn> {
    let _guard = crate::migration::runtime_mutation(project)?;
    let control = controlled::ReadControl::new(std::time::Instant::now() + Duration::from_secs(2), Default::default());
    Ok(crate::migration::open_active_scoped(project, control)?.service_integration_jobs()?)
}

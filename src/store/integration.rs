//! Schema 28 integration queue. Confirm writes a satisfaction row for a matching
//! `integrated_commit` edge. The wake event is not that evidence.
//! `landed_commit` and `integration_candidate` rows are never rewritten here.
use super::*;
use crate::operations::{Claim, DeliveryState, Outcome};
use rusqlite::OptionalExtension;

const SCHEMA_VERSION: u32 = 28;
pub(crate) const LEASE_OWNER: &str = "integration-broker";

fn invalid(message: &str) -> StoreError {
    StoreError::Invalid(message.into())
}
fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn schema28(db: &Connection) -> Result<()> {
    check_schema(db)?;
    let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchema(version));
    }
    Ok(())
}
fn project_store(db: &Connection) -> Result<String> {
    let path = db
        .path()
        .ok_or_else(|| invalid("store path missing"))?;
    let path = std::fs::canonicalize(path).map_err(|error| StoreError::Io(error.to_string()))?;
    path.to_str()
        .map(str::to_string)
        .ok_or_else(|| invalid("store path is not utf-8"))
}

pub(crate) fn valid_ref_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("refs/heads/") else {
        return false;
    };
    !rest.is_empty()
        && !rest.ends_with('/')
        && !rest.contains("//")
        && rest.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && !part.ends_with(".lock")
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        })
}

#[derive(Clone)]
pub(crate) struct VerifiedIntegration {
    pub result_id: String,
    pub commit_oid: String,
    pub object_format: String,
    pub repository: String,
    pub task_id: String,
    pub task_revision: i64,
    pub policy_body: String,
    pub route: String,
}

#[derive(Clone)]
pub(crate) struct IntegrationView {
    pub operation_id: String,
    pub payload_digest: String,
    pub state: String,
    pub reason: Option<String>,
    pub generation: i64,
    pub expected_old_oid: String,
    pub checks_passed: bool,
    pub candidate_id: Option<String>,
    pub commit_oid: Option<String>,
    pub tree_oid: Option<String>,
    pub parent_base: Option<String>,
    pub parent_verified: Option<String>,
    pub verified_result_id: String,
    pub ref_name: String,
    pub repository: String,
    pub object_format: String,
}

pub(crate) struct IntegrationBegin {
    pub repository: String,
    pub ref_name: String,
    pub expected_old_oid: String,
    pub verified: VerifiedIntegration,
    pub idempotency_key: String,
    pub payload_digest: String,
}

pub(crate) enum IntegrationFinish {
    Blocked { reason: &'static str },
    Discarded { reason: &'static str },
    Reconciliation { reason: &'static str },
    Confirm,
}

fn claim_held(tx: &Connection, claim: &Claim, now: i64) -> Result<()> {
    super::delivery::now_check(now)?;
    let old = super::delivery::delivery(tx, &claim.operation)?;
    if old.state != DeliveryState::Claimed
        || old.revision != claim.revision
        || old.epoch != claim.epoch
        || old.owner.as_deref() != Some(claim.owner.as_str())
        || old.lease_until_ms != Some(claim.lease_until_ms)
        || now >= claim.lease_until_ms
    {
        return Err(StoreError::Conflict);
    }
    let (task, expected): (String, i64) = tx.query_row(
        "SELECT task_id, expected_revision FROM operations WHERE id=?1",
        [claim.operation.as_str()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let actual: i64 = tx.query_row("SELECT revision FROM tasks WHERE id=?1", [task], |row| {
        row.get(0)
    })?;
    if actual != expected {
        return Err(StoreError::Conflict);
    }
    Ok(())
}

fn view_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<IntegrationView> {
    let checks_passed: i64 = row.get(6)?;
    Ok(IntegrationView {
        operation_id: row.get(0)?,
        payload_digest: row.get(1)?,
        state: row.get(2)?,
        reason: row.get(3)?,
        generation: row.get(4)?,
        expected_old_oid: row.get(5)?,
        checks_passed: checks_passed == 1,
        candidate_id: row.get(7)?,
        commit_oid: row.get(8)?,
        tree_oid: row.get(9)?,
        parent_base: row.get(10)?,
        parent_verified: row.get(11)?,
        verified_result_id: row.get(12)?,
        ref_name: row.get(13)?,
        repository: row.get(14)?,
        object_format: row.get(15)?,
    })
}

const VIEW_SQL: &str = "SELECT o.operation_id, o.payload_digest, o.state, o.reason, o.generation, o.expected_old_oid, o.checks_passed, o.candidate_id, c.commit_oid, c.tree_oid, c.parent_base, c.parent_verified, o.verified_result_id, o.ref_name, o.repository, o.object_format FROM integration_operations o LEFT JOIN integration_candidates c ON c.candidate_id = o.candidate_id";

impl SqliteStore {
    pub(crate) fn configure_integration_ref(
        &mut self,
        repository: &str,
        ref_name: &str,
    ) -> Result<()> {
        if repository.is_empty()
            || repository.len() > 4096
            || repository.chars().any(char::is_control)
            || !valid_ref_name(ref_name)
        {
            return Err(invalid("integration ref is invalid"));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema28(&tx)?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT ref_name FROM integration_targets WHERE repository=?1",
                [repository],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(existing) = existing {
            if existing != ref_name {
                return Err(invalid("integration ref cannot be retargeted"));
            }
            tx.commit()?;
            return Ok(());
        }
        let now = jiff::Timestamp::now().as_millisecond();
        tx.execute(
            "INSERT INTO integration_targets(repository, ref_name, created_unix_ms) VALUES(?1,?2,?3)",
            params![repository, ref_name, now],
        )?;
        tx.execute(
            "INSERT INTO integration_target_leases(repository, ref_name, operation_id, generation) VALUES(?1,?2,NULL,0)",
            params![repository, ref_name],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn integration_ref(&mut self, repository: &str) -> Result<Option<String>> {
        let tx = self.connection.transaction()?;
        schema28(&tx)?;
        let found = tx
            .query_row(
                "SELECT ref_name FROM integration_targets WHERE repository=?1",
                [repository],
                |row| row.get(0),
            )
            .optional()?;
        tx.commit()?;
        Ok(found)
    }

    pub(crate) fn find_integration(
        &mut self,
        repository: &str,
        idempotency_key: &str,
    ) -> Result<Option<IntegrationView>> {
        let tx = self.connection.transaction()?;
        schema28(&tx)?;
        let found = tx
            .query_row(
                &format!("{VIEW_SQL} WHERE o.repository=?1 AND o.idempotency_key=?2 ORDER BY o.generation DESC LIMIT 1"),
                params![repository, idempotency_key],
                view_from_row,
            )
            .optional()?;
        tx.commit()?;
        Ok(found)
    }

    pub(crate) fn load_integration_operation(&mut self, operation_id: &str) -> Result<IntegrationView> {
        let tx = self.connection.transaction()?;
        schema28(&tx)?;
        let found = tx
            .query_row(
                &format!("{VIEW_SQL} WHERE o.operation_id=?1"),
                [operation_id],
                view_from_row,
            )
            .optional()?;
        tx.commit()?;
        found.ok_or_else(|| invalid("integration operation is missing"))
    }

    pub(crate) fn load_verified_for_integration(
        &mut self,
        result_id: &str,
    ) -> Result<VerifiedIntegration> {
        let project = project_store(&self.connection)?;
        let tx = self.connection.transaction()?;
        schema28(&tx)?;
        let row = tx
            .query_row(
                "SELECT r.result_id, r.commit_oid, r.object_format, r.policy_digest, s.candidate_oid, s.repository, s.task_id, v.policy_id, v.contract_revision, p.body, c.route, t.revision, v.project_store
                 FROM verified_results r
                 JOIN verification_runs v ON v.run_id = r.run_id
                 JOIN result_submissions s ON s.submission_id = r.submission_id
                 JOIN task_contracts c ON c.task_id = v.task_id AND c.contract_revision = v.contract_revision
                 JOIN acceptance_policies p ON p.task_id = v.task_id AND p.contract_revision = v.contract_revision AND p.policy_id = v.policy_id
                 JOIN tasks t ON t.id = v.task_id
                 WHERE r.result_id=?1",
                [result_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, i64>(8)?,
                        row.get::<_, String>(9)?,
                        row.get::<_, String>(10)?,
                        row.get::<_, i64>(11)?,
                        row.get::<_, String>(12)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            result_id,
            commit_oid,
            object_format,
            policy_digest,
            candidate_oid,
            repository,
            task_id,
            _policy_id,
            _contract_revision,
            policy_body,
            route,
            task_revision,
            stored_project,
        )) = row
        else {
            return Err(invalid("verified result is missing"));
        };
        if stored_project != project || commit_oid != candidate_oid {
            return Err(invalid("verified result does not match its submission"));
        }
        if sha256_hex(policy_body.as_bytes()) != policy_digest {
            return Err(StoreError::Corrupt("acceptance policy digest mismatch".into()));
        }
        tx.commit()?;
        Ok(VerifiedIntegration {
            result_id,
            commit_oid,
            object_format,
            repository,
            task_id,
            task_revision,
            policy_body,
            route,
        })
    }

    /// One lease operation per ref generation. The caller claims it before git runs.
    pub(crate) fn begin_integration(&mut self, begin: &IntegrationBegin) -> Result<String> {
        if !valid_ref_name(&begin.ref_name) {
            return Err(invalid("integration ref is invalid"));
        }
        let project = project_store(&self.connection)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema28(&tx)?;
        let configured: Option<String> = tx
            .query_row(
                "SELECT ref_name FROM integration_targets WHERE repository=?1",
                [&begin.repository],
                |row| row.get(0),
            )
            .optional()?;
        if configured.as_deref() != Some(begin.ref_name.as_str()) {
            return Err(invalid("integration ref is not configured"));
        }
        let blocking: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM integration_operations WHERE repository=?1 AND ref_name=?2 AND state IN ('effect_pending','candidate_prepared','validating','reconciliation_required'))",
            params![begin.repository, begin.ref_name],
            |row| row.get(0),
        )?;
        if blocking {
            return Err(StoreError::Conflict);
        }
        let generation: i64 = tx.query_row(
            "SELECT generation FROM integration_target_leases WHERE repository=?1 AND ref_name=?2",
            params![begin.repository, begin.ref_name],
            |row| row.get(0),
        )?;
        let next = generation.checked_add(1).ok_or_else(|| invalid("integration generation exhausted"))?;
        let operation_id = sha256_hex(
            format!(
                "{project}\0{}\0{next}\0{}",
                begin.idempotency_key, begin.verified.result_id
            )
            .as_bytes(),
        );
        let payload = serde_json::json!({
            "repository": begin.repository,
            "ref": begin.ref_name,
            "result_id": begin.verified.result_id,
            "expected_old": begin.expected_old_oid,
            "generation": next,
        })
        .to_string();
        let payload_hash = sha256_hex(payload.as_bytes());
        tx.execute(
            "INSERT INTO operations(id,task_id,kind,target,payload_version,payload,payload_hash,expected_revision,due_unix_ms,idempotency_key) VALUES(?1,?2,'integration.lease',?3,1,?4,?5,?6,0,?1)",
            params![
                operation_id,
                begin.verified.task_id,
                begin.ref_name,
                payload,
                payload_hash,
                begin.verified.task_revision
            ],
        )?;
        let updated = tx.execute(
            "UPDATE integration_target_leases SET generation=?3, operation_id=?4 WHERE repository=?1 AND ref_name=?2 AND generation=?5",
            params![begin.repository, begin.ref_name, next, operation_id, generation],
        )?;
        if updated != 1 {
            return Err(StoreError::Conflict);
        }
        let now = jiff::Timestamp::now().as_millisecond();
        tx.execute(
            "INSERT INTO integration_operations(operation_id,project_store,idempotency_key,payload_digest,repository,ref_name,expected_old_oid,verified_result_id,candidate_id,state,generation,object_format,checks_passed,reason,created_unix_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,NULL,'effect_pending',?9,?10,0,NULL,?11)",
            params![
                operation_id,
                project,
                begin.idempotency_key,
                begin.payload_digest,
                begin.repository,
                begin.ref_name,
                begin.expected_old_oid,
                begin.verified.result_id,
                next,
                begin.verified.object_format,
                now
            ],
        )?;
        tx.execute(
            "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('integration.effect_pending',?1,1,1,?2)",
            params![
                operation_id,
                serde_json::json!({
                    "expected_old": begin.expected_old_oid,
                    "result_id": begin.verified.result_id,
                    "generation": next
                })
                .to_string()
            ],
        )?;
        tx.commit()?;
        Ok(operation_id)
    }

    pub(crate) fn abandon_unclaimed(&mut self, operation_id: &str) -> Result<()> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema28(&tx)?;
        let updated = tx.execute(
            "UPDATE integration_operations SET state='discarded', reason='claim_failed' WHERE operation_id=?1 AND state='effect_pending'",
            [operation_id],
        )?;
        if updated == 1 {
            let (revision, task_id): (i64, Option<String>) = tx.query_row(
                "SELECT d.revision, o.task_id FROM operation_delivery d JOIN operations o ON o.id=d.operation_id WHERE d.operation_id=?1",
                [operation_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if let Some(task_id) = task_id {
                super::feedback::insert_feedback(
                    &tx,
                    &super::feedback::LocalFeedback {
                        operation_id: operation_id.to_string(),
                        outcome_revision: revision,
                        category: "integrator_rejection".into(),
                        task_id,
                        reason: "claim_failed".into(),
                    },
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Record M before any ref update so a lost reply can confirm only this oid.
    pub(crate) fn record_candidate(
        &mut self,
        claim: &Claim,
        commit_oid: &str,
        tree_oid: &str,
        parent_verified: &str,
        now: i64,
    ) -> Result<String> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema28(&tx)?;
        claim_held(&tx, claim, now)?;
        let (expected_old, object_format, state): (String, String, String) = tx.query_row(
            "SELECT expected_old_oid, object_format, state FROM integration_operations WHERE operation_id=?1",
            [claim.operation.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if state != "effect_pending" {
            return Err(StoreError::Conflict);
        }
        let candidate_id = sha256_hex(
            format!("{}\0{commit_oid}\0{tree_oid}", claim.operation.as_str()).as_bytes(),
        );
        let created = jiff::Timestamp::now().as_millisecond();
        tx.execute(
            "INSERT INTO integration_candidates(candidate_id,operation_id,commit_oid,tree_oid,parent_base,parent_verified,strategy,object_format,state,created_unix_ms) VALUES(?1,?2,?3,?4,?5,?6,'ort',?7,'prepared',?8)",
            params![
                candidate_id,
                claim.operation.as_str(),
                commit_oid,
                tree_oid,
                expected_old,
                parent_verified,
                object_format,
                created
            ],
        )?;
        let updated = tx.execute(
            "UPDATE integration_operations SET state='candidate_prepared', candidate_id=?2 WHERE operation_id=?1 AND state='effect_pending'",
            params![claim.operation.as_str(), candidate_id],
        )?;
        if updated != 1 {
            return Err(StoreError::Conflict);
        }
        tx.commit()?;
        Ok(candidate_id)
    }

    pub(crate) fn mark_checks_passed(&mut self, claim: &Claim, now: i64) -> Result<()> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema28(&tx)?;
        claim_held(&tx, claim, now)?;
        let updated = tx.execute(
            "UPDATE integration_operations SET state='validating', checks_passed=1 WHERE operation_id=?1 AND state='candidate_prepared' AND checks_passed=0",
            [claim.operation.as_str()],
        )?;
        if updated != 1 {
            return Err(StoreError::Conflict);
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn mark_publish_attempted(&mut self, claim: &Claim, now: i64) -> Result<()> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema28(&tx)?;
        claim_held(&tx, claim, now)?;
        // Remember that update-ref was attempted so a later third OID is not a stale base.
        let updated = tx.execute(
            "UPDATE integration_operations SET reason='publish_attempted' WHERE operation_id=?1 AND state='validating'",
            [claim.operation.as_str()],
        )?;
        if updated != 1 {
            return Err(StoreError::Conflict);
        }
        tx.commit()?;
        Ok(())
    }

    /// The ref is still the expected old OID, so the expired lease had no publish to protect.
    /// Generic retry backoff would delay the fresh claim that fences the next update-ref.
    pub(crate) fn requeue_expired_lease(&mut self, operation_id: &str, now: i64) -> Result<u64> {
        let id = crate::domain::OperationId::new(operation_id.to_string())
            .map_err(|error| invalid(&error))?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema28(&tx)?;
        let old = super::delivery::delivery(&tx, &id)?;
        if old.state == DeliveryState::Pending {
            let revision = old.revision;
            tx.commit()?;
            return Ok(revision);
        }
        let expired = old.state == DeliveryState::Claimed
            && old.owner.as_deref() == Some(LEASE_OWNER)
            && old.lease_until_ms.map(|until| now >= until).unwrap_or(false);
        if old.state != DeliveryState::Ambiguous && !expired {
            return Err(StoreError::Conflict);
        }
        let updated = super::delivery::update_outcome(
            &tx,
            &old,
            &Outcome::Retryable {
                no_effect_evidence: "integration lease expired before the ref moved".into(),
            },
            now,
            LEASE_OWNER,
        )?;
        tx.execute(
            "UPDATE operation_delivery SET next_due_ms=?2 WHERE operation_id=?1 AND state='pending'",
            params![id.as_str(), now],
        )?;
        tx.commit()?;
        Ok(updated.revision)
    }

    /// Receipt, satisfaction row, and lease release commit together. The wake event is not the evidence.
    pub(crate) fn finish_integration(
        &mut self,
        claim: &Claim,
        finish: IntegrationFinish,
        now: i64,
    ) -> Result<()> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema28(&tx)?;
        claim_held(&tx, claim, now)?;
        apply_finish(&tx, claim.operation.as_str(), &finish, now)?;
        tx.commit()?;
        Ok(())
    }

    /// Confirm or reconcile after the lease expired. A new update-ref still needs a live claim.
    pub(crate) fn finish_integration_observed(
        &mut self,
        operation_id: &str,
        finish: IntegrationFinish,
        now: i64,
    ) -> Result<()> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema28(&tx)?;
        apply_finish(&tx, operation_id, &finish, now)?;
        tx.commit()?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn testing_expire_lease(&mut self, operation_id: &str) -> Result<()> {
        schema28(&self.connection)?;
        let updated = self.connection.execute(
            "UPDATE operation_delivery SET lease_until_ms=0 WHERE operation_id=?1 AND state='claimed'",
            [operation_id],
        )?;
        if updated != 1 {
            return Err(StoreError::Conflict);
        }
        Ok(())
    }
}

fn apply_finish(
    tx: &Connection,
    operation_id: &str,
    finish: &IntegrationFinish,
    now: i64,
) -> Result<()> {
    let id = crate::domain::OperationId::new(operation_id.to_string()).map_err(|error| invalid(&error))?;
    let view = tx
            .query_row(
                &format!("{VIEW_SQL} WHERE o.operation_id=?1"),
                [operation_id],
                view_from_row,
            )
            .optional()?
            .ok_or_else(|| invalid("integration operation is missing"))?;
        if matches!(
            view.state.as_str(),
            "integrated" | "blocked" | "discarded" | "reconciliation_required"
        ) {
            return Err(StoreError::Conflict);
        }
        let (state, reason, outcome) = match &finish {
            IntegrationFinish::Blocked { reason } => (
                "blocked",
                Some(*reason),
                Outcome::PermanentFailure {
                    diagnostic: format!("integration blocked: {reason}"),
                },
            ),
            IntegrationFinish::Discarded { reason } => (
                "discarded",
                Some(*reason),
                Outcome::PermanentFailure {
                    diagnostic: format!("integration discarded: {reason}"),
                },
            ),
            IntegrationFinish::Reconciliation { reason } => (
                "reconciliation_required",
                Some(*reason),
                Outcome::Ambiguous {
                    observation_required: format!("integration ref requires reconciliation: {reason}"),
                },
            ),
            IntegrationFinish::Confirm => {
                if view.state != "validating" || !view.checks_passed {
                    return Err(StoreError::Conflict);
                }
                let candidate_id = view
                    .candidate_id
                    .clone()
                    .ok_or_else(|| invalid("integration candidate is missing"))?;
                let commit_oid = view
                    .commit_oid
                    .clone()
                    .ok_or_else(|| invalid("integration candidate is missing"))?;
                let tree_oid = view
                    .tree_oid
                    .clone()
                    .ok_or_else(|| invalid("integration candidate is missing"))?;
                let integrated_id = sha256_hex(
                    format!("{operation_id}\0{candidate_id}\0{commit_oid}").as_bytes(),
                );
                let created = jiff::Timestamp::now().as_millisecond();
                tx.execute(
                    "UPDATE integration_candidates SET state='published' WHERE candidate_id=?1 AND state='prepared'",
                    [&candidate_id],
                )?;
                tx.execute(
                    "INSERT INTO integrated_commits(integrated_id,candidate_id,operation_id,repository,ref_name,commit_oid,tree_oid,expected_old_oid,object_format,created_unix_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                    params![
                        integrated_id,
                        candidate_id,
                        operation_id,
                        view.repository,
                        view.ref_name,
                        commit_oid,
                        tree_oid,
                        view.expected_old_oid,
                        view.object_format,
                        created
                    ],
                )?;
                // The wake event is not evidence. The satisfaction row is the stored receipt.
                super::satisfaction::record_integrated_commit(tx, &integrated_id)?;
                tx.execute(
                    "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('integration.wake',?1,1,1,?2)",
                    params![
                        integrated_id,
                        serde_json::json!({
                            "integrated_id": integrated_id,
                            "commit_oid": commit_oid,
                            "satisfies_dependency": false
                        })
                        .to_string()
                    ],
                )?;
                (
                    "integrated",
                    None,
                    Outcome::Confirmed {
                        observed_identity: commit_oid,
                    },
                )
            }
        };
        if !matches!(finish, IntegrationFinish::Confirm) {
            tx.execute(
                "UPDATE integration_candidates SET state='discarded' WHERE operation_id=?1 AND state='prepared'",
                [operation_id],
            )?;
        }
        let updated = tx.execute(
            "UPDATE integration_operations SET state=?2, reason=?3 WHERE operation_id=?1 AND state=?4",
            params![operation_id, state, reason, view.state],
        )?;
        if updated != 1 {
            return Err(StoreError::Conflict);
        }
        // An expired or ambiguous lease can still record the observation. A live claim is not required.
        let old = super::delivery::delivery(&tx, &id)?;
        let outcome_revision = if matches!(
            old.state,
            DeliveryState::Claimed | DeliveryState::Ambiguous | DeliveryState::Pending
        ) {
            let delivered = super::delivery::update_outcome(&tx, &old, &outcome, now, LEASE_OWNER)?;
            i64::try_from(delivered.revision).map_err(|_| invalid("delivery revision does not fit"))?
        } else {
            i64::try_from(old.revision).map_err(|_| invalid("delivery revision does not fit"))?
        };
        if let (Some(category), Some(why)) = (feedback_category(finish), reason) {
            let task_id: Option<String> = tx.query_row(
                "SELECT task_id FROM operations WHERE id=?1",
                [operation_id],
                |row| row.get(0),
            )?;
            let task_id = task_id.ok_or_else(|| invalid("integration feedback is missing a task"))?;
            super::feedback::insert_feedback(
                &tx,
                &super::feedback::LocalFeedback {
                    operation_id: operation_id.to_string(),
                    outcome_revision,
                    category: category.into(),
                    task_id,
                    reason: why.to_string(),
                },
            )?;
        }
        Ok(())
}

fn feedback_category(finish: &IntegrationFinish) -> Option<&'static str> {
    match finish {
        IntegrationFinish::Blocked { .. } | IntegrationFinish::Discarded { .. } => {
            Some("integrator_rejection")
        }
        IntegrationFinish::Reconciliation { .. } => Some("integrator_conflict"),
        IntegrationFinish::Confirm => None,
    }
}

impl SqliteStore {
    pub(crate) fn other_integration_generation(
        &mut self,
        repository: &str,
        ref_name: &str,
        operation_id: &str,
    ) -> Result<bool> {
        let tx = self.connection.transaction()?;
        schema28(&tx)?;
        let found: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM integration_operations WHERE repository=?1 AND ref_name=?2 AND operation_id<>?3 AND state IN ('effect_pending','candidate_prepared','validating','reconciliation_required'))",
            params![repository, ref_name, operation_id],
            |row| row.get(0),
        )?;
        tx.commit()?;
        Ok(found)
    }

    pub(crate) fn held_claim(&mut self, operation_id: &str, now: i64) -> Result<Claim> {
        let tx = self.connection.transaction()?;
        schema28(&tx)?;
        let id = crate::domain::OperationId::new(operation_id.to_string())
            .map_err(|error| invalid(&error))?;
        let old = super::delivery::delivery(&tx, &id)?;
        tx.commit()?;
        if old.state != DeliveryState::Claimed
            || old.owner.as_deref() != Some(LEASE_OWNER)
            || old.lease_until_ms.map(|until| now >= until).unwrap_or(true)
        {
            return Err(StoreError::Conflict);
        }
        Ok(Claim {
            operation: id,
            revision: old.revision,
            owner: old.owner.unwrap_or_default(),
            epoch: old.epoch,
            lease_until_ms: old.lease_until_ms.unwrap_or_default(),
        })
    }

    #[cfg(test)]
    pub(crate) fn testing_accept_verified_result(
        &mut self,
        submission_id: &str,
        policy_id: &str,
        commit_oid: &str,
        tree_oid: &str,
    ) -> Result<String> {
        let project = project_store(&self.connection)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema28(&tx)?;
        let row = tx
            .query_row(
                "SELECT s.task_id, s.contract_revision, s.contract_digest, s.attempt_id, s.candidate_oid, s.object_format, p.body
                 FROM result_submissions s
                 JOIN acceptance_policies p ON p.task_id=s.task_id AND p.contract_revision=s.contract_revision AND p.policy_id=?2
                 WHERE s.submission_id=?1 AND s.project_store=?3",
                params![submission_id, policy_id, project],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                    ))
                },
            )
            .optional()?;
        let Some((task_id, contract_revision, contract_digest, attempt_id, candidate_oid, object_format, body)) =
            row
        else {
            return Err(invalid("verification target is missing"));
        };
        if candidate_oid != commit_oid {
            return Err(invalid("verified commit does not match the submission"));
        }
        let policy_digest = sha256_hex(body.as_bytes());
        let payload_digest = sha256_hex(format!("{submission_id}\0{policy_digest}\0{commit_oid}").as_bytes());
        let run_id = sha256_hex(format!("{project}\0test-verify\0{payload_digest}").as_bytes());
        let result_id = sha256_hex(format!("{run_id}\0{tree_oid}\0{policy_digest}").as_bytes());
        let receipt_digest = sha256_hex(format!("{run_id}\0{commit_oid}\0{tree_oid}").as_bytes());
        let now = jiff::Timestamp::now().as_millisecond();
        tx.execute(
            "INSERT INTO verification_runs(run_id,project_store,idempotency_key,payload_digest,submission_id,task_id,contract_revision,contract_digest,attempt_id,policy_id,policy_digest,commit_oid,tree_oid,object_format,memory_fence,isolation,argv,library_manifest,state,reason,exit_status,receipt_digest,store_device,store_inode,created_unix_ms) VALUES(?1,?2,'test-verify',?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,0,'linux-unshare-user-pid-mount-v1','[]','[]','accepted',NULL,0,?14,0,0,?15)",
            params![
                run_id,
                project,
                payload_digest,
                submission_id,
                task_id,
                contract_revision,
                contract_digest,
                attempt_id,
                policy_id,
                policy_digest,
                commit_oid,
                tree_oid,
                object_format,
                receipt_digest,
                now
            ],
        )?;
        tx.execute(
            "INSERT INTO verified_results(result_id,run_id,submission_id,commit_oid,tree_oid,object_format,policy_digest,receipt_digest,isolation,memory_fence,created_unix_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'linux-unshare-user-pid-mount-v1',0,?9)",
            params![
                result_id,
                run_id,
                submission_id,
                commit_oid,
                tree_oid,
                object_format,
                policy_digest,
                receipt_digest,
                now
            ],
        )?;
        tx.commit()?;
        Ok(result_id)
    }

    #[cfg(test)]
    pub(crate) fn testing_insert_dependency(
        &mut self,
        task: &str,
        predecessor: &str,
        requirement: &str,
    ) -> Result<()> {
        if !matches!(
            requirement,
            "verified_result" | "integration_candidate" | "landed_commit"
        ) {
            return Err(invalid("dependency requirement is not historical"));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema28(&tx)?;
        tx.execute(
            "INSERT INTO task_dependencies(task_id,predecessor_id,requirement) VALUES(?1,?2,?3)",
            params![task, predecessor, requirement],
        )?;
        tx.commit()?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn testing_dependencies(&self) -> Result<Vec<(String, String, String)>> {
        schema28(&self.connection)?;
        let mut stmt = self.connection.prepare(
            "SELECT task_id, predecessor_id, requirement FROM task_dependencies ORDER BY task_id, predecessor_id",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    #[cfg(test)]
    pub(crate) fn testing_integrated_oids(&self) -> Result<Vec<String>> {
        schema28(&self.connection)?;
        let mut stmt = self
            .connection
            .prepare("SELECT commit_oid FROM integrated_commits ORDER BY commit_oid")?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    #[cfg(test)]
    pub(crate) fn testing_candidate_states(&self, key: &str) -> Result<Vec<(String, String)>> {
        schema28(&self.connection)?;
        let mut stmt = self.connection.prepare(
            "SELECT c.state, c.commit_oid FROM integration_candidates c JOIN integration_operations o ON o.operation_id=c.operation_id WHERE o.idempotency_key=?1 ORDER BY c.candidate_id",
        )?;
        let rows = stmt.query_map([key], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_version(db: &Connection) -> u32 {
        db.query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }
    fn table_exists(db: &Connection, name: &str) -> bool {
        db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [name],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn upgrade_v1_from_27_to_28_preserves_historical_dependencies() {
        let fresh = tempfile::tempdir().unwrap();
        let created = SqliteStore::create(&fresh.path().join("state.db")).unwrap();
        assert_eq!(user_version(&created.connection), 36);
        assert!(table_exists(&created.connection, "integration_operations"));
        assert!(table_exists(&created.connection, "integration_candidates"));
        assert!(table_exists(&created.connection, "integrated_commits"));
        assert!(table_exists(&created.connection, "integration_target_leases"));
        drop(created);

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.db");
        let mut db = SqliteStore::create(&path).unwrap();
        db.commit(Commit {
            expected_head: 0,
            mutations: vec![
                Mutation::Task {
                    expected: None,
                    next: Task {
                        id: TaskId::new("consumer").unwrap(),
                        revision: 1,
                        state: TaskState::Draft,
                        title: "consumer".into(),
                        active_attempt: None,
                    },
                },
                Mutation::Task {
                    expected: None,
                    next: Task {
                        id: TaskId::new("pred").unwrap(),
                        revision: 1,
                        state: TaskState::Draft,
                        title: "pred".into(),
                        active_attempt: None,
                    },
                },
            ],
        })
        .unwrap();
        db.connection
            .execute(
                "INSERT INTO task_dependencies(task_id,predecessor_id,requirement) VALUES('consumer','pred','landed_commit')",
                [],
            )
            .unwrap();
        db.connection
            .execute(
                "INSERT INTO task_dependencies(task_id,predecessor_id,requirement) VALUES('pred','consumer','integration_candidate')",
                [],
            )
            .unwrap();
        let before = db.testing_dependencies().unwrap();
        drop(db);
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.execute_batch(
            "DROP TABLE IF EXISTS wait_replay_events; DROP TABLE IF EXISTS replan_requests; DROP TABLE IF EXISTS replan_budget_resets; DROP TABLE IF EXISTS attempt_infrastructure_retries; DROP TABLE IF EXISTS wait_conditions; DROP TABLE IF EXISTS resource_claims; DROP TABLE IF EXISTS delegation_stop_obligations; DROP TABLE IF EXISTS delegation_revocations; DROP TABLE IF EXISTS delegation_grants; DROP TABLE IF EXISTS capability_evidence; DROP TABLE IF EXISTS contract_named_resources; DROP TABLE IF EXISTS contract_scope_paths; DROP TABLE IF EXISTS plan_revisions; DROP TABLE IF EXISTS plan_proposals; DROP TABLE IF EXISTS dependency_satisfactions; DROP TABLE IF EXISTS factory_admission_policies; ALTER TABLE project_control DROP COLUMN factory_admission; DROP TABLE IF EXISTS feedback_claims; DROP TABLE IF EXISTS feedback_items; DROP TABLE IF EXISTS integrated_commits; DROP TABLE IF EXISTS integration_candidates; DROP TABLE IF EXISTS integration_operations; DROP TABLE IF EXISTS integration_target_leases; DROP TABLE IF EXISTS integration_targets; UPDATE store_meta SET schema_version=27; PRAGMA user_version=27;",
        )
        .unwrap();
        drop(raw);
        let mut db = SqliteStore::open(&path).unwrap();
        assert_eq!(user_version(&db.connection), 27);
        assert!(!table_exists(&db.connection, "integration_operations"));
        assert_eq!(db.testing_dependencies().is_err(), true);
        let preserved: Vec<(String, String, String)> = {
            let mut stmt = db
                .connection
                .prepare(
                    "SELECT task_id, predecessor_id, requirement FROM task_dependencies ORDER BY task_id, predecessor_id",
                )
                .unwrap();
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap()
        };
        assert_eq!(preserved, before);
        assert!(matches!(
            db.import_legacy(&"cd".repeat(32), &[], &[]),
            Err(StoreError::UnsupportedSchema(27))
        ));
        db.upgrade_v1().unwrap();
        assert_eq!(user_version(&db.connection), 36);
        assert_eq!(
            db.connection
                .query_row("SELECT schema_version FROM store_meta", [], |row| row.get::<_, u32>(0))
                .unwrap(),
            36
        );
        assert!(table_exists(&db.connection, "integrated_commits"));
        assert_eq!(db.testing_dependencies().unwrap(), before);
        let check_sql: String = db
            .connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='task_dependencies'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(check_sql.contains("landed_commit"));
        assert!(check_sql.contains("integration_candidate"));
        assert!(check_sql.contains("integrated_commit"));
        let satisfactions: i64 = db
            .connection
            .query_row(
                "SELECT count(*) FROM dependency_satisfactions",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(satisfactions, 0);
        let admission: String = db
            .connection
            .query_row(
                "SELECT factory_admission FROM project_control WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(admission, "off");
        check_schema(&db.connection).unwrap();
        drop(db);
        let reopened = SqliteStore::open(&path).unwrap();
        assert_eq!(user_version(&reopened.connection), 36);
    }
}

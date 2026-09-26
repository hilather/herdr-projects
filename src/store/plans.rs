//! Schema 31 untrusted plan revisions. A proposal file is not a signature and
//! is not installed as a task contract. Accepting one does not reserve an attempt.
//! Schema 36 waits commit the cursor in that same transaction and replay it once.
//! A schema 29 feedback row is the only replan trigger: two automatic replans for
//! one blocker inside a plan revision, then one inbox escalation. A pull-request
//! poll is not a trigger. Infrastructure retries stay on the same attempt.
use super::*;
use crate::domain::{Dependency, QueueRecord, Task, TaskId, TaskState};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const SCHEMA_VERSION: u32 = 31;
const PROPOSAL_LIMIT: usize = 256 * 1024;

fn invalid(message: &str) -> StoreError {
    StoreError::Invalid(message.into())
}
fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn schema31(db: &Connection) -> Result<()> {
    check_schema(db)?;
    let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchema(version));
    }
    Ok(())
}
fn store_path(db: &Connection) -> Result<PathBuf> {
    let path = db.path().ok_or_else(|| invalid("store path missing"))?;
    std::fs::canonicalize(path).map_err(|error| StoreError::Io(error.to_string()))
}
fn identifier(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
}
fn plain(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanProposalReceipt {
    pub plan_revision: u64,
    pub proposal_id: String,
    pub digest: String,
    pub replayed: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProposalFile {
    version: u32,
    contracts: Vec<ProposedContract>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProposedContract {
    task_id: TaskId,
    text: String,
    dependencies: Vec<Dependency>,
}

struct ParsedProposal {
    contracts: Vec<ProposedContract>,
    contract_texts: String,
}

fn parse_proposal(raw: &[u8]) -> Result<ParsedProposal> {
    if raw.is_empty() || raw.len() > PROPOSAL_LIMIT {
        return Err(invalid("plan proposal exceeds 256 KiB"));
    }
    if std::str::from_utf8(raw).is_err() {
        return Err(invalid("invalid plan proposal"));
    }
    let document: ProposalFile =
        serde_json::from_slice(raw).map_err(|_| invalid("invalid plan proposal"))?;
    if document.version != 1 || document.contracts.is_empty() || document.contracts.len() > 1024 {
        return Err(invalid("invalid plan proposal"));
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut listed = Vec::with_capacity(document.contracts.len());
    for contract in &document.contracts {
        if !seen.insert(contract.task_id.as_str().to_string())
            || !plain(&contract.text, PROPOSAL_LIMIT)
        {
            return Err(invalid("invalid plan proposal"));
        }
        if contract.dependencies.len() > 256 {
            return Err(invalid("dependency inventory exceeds bounds"));
        }
        listed.push(serde_json::json!({
            "task_id": contract.task_id.as_str(),
            "text": contract.text,
        }));
    }
    let contract_texts =
        serde_json::to_string(&listed).map_err(|error| invalid(&error.to_string()))?;
    if contract_texts.len() > PROPOSAL_LIMIT {
        return Err(invalid("plan proposal exceeds 256 KiB"));
    }
    Ok(ParsedProposal {
        contracts: document.contracts,
        contract_texts,
    })
}

/// Cycle check uses the live queue plus this proposal. Mentioned tasks replace
/// their edges; unmentioned queue edges stay. Nothing is written.
fn reject_cycles(db: &Connection, proposal: &ParsedProposal) -> Result<()> {
    let mut tasks = read_tasks(db)?;
    let mut queue = scheduler::read(db)?.queue;
    for contract in &proposal.contracts {
        if !tasks.iter().any(|task| task.id == contract.task_id) {
            tasks.push(Task {
                id: contract.task_id.clone(),
                revision: 1,
                state: TaskState::Draft,
                title: contract.task_id.as_str().to_string(),
                active_attempt: None,
            });
        }
        let dependencies = contract.dependencies.clone();
        if let Some(existing) = queue.iter_mut().find(|row| row.task == contract.task_id) {
            existing.dependencies = dependencies;
        } else {
            queue.push(QueueRecord {
                task: contract.task_id.clone(),
                priority: 0,
                enqueued_unix_ms: 0,
                enqueue_sequence: 0,
                dependencies,
            });
        }
    }
    scheduler::graph(&tasks, &queue)
}

struct ExistingProposal {
    proposal_id: String,
    digest: String,
    payload: Vec<u8>,
    plan_revision: u64,
}

fn lookup_proposal(
    db: &Connection,
    project_store: &str,
    key: &str,
) -> Result<Option<ExistingProposal>> {
    db.query_row(
        "SELECT proposal_id, payload_digest, payload, plan_revision FROM plan_proposals WHERE project_store=?1 AND idempotency_key=?2",
        params![project_store, key],
        |row| {
            Ok(ExistingProposal {
                proposal_id: row.get(0)?,
                digest: row.get(1)?,
                payload: row.get(2)?,
                plan_revision: u64::try_from(row.get::<_, i64>(3)?).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        3,
                        rusqlite::types::Type::Integer,
                        Box::new(error),
                    )
                })?,
            })
        },
    )
    .optional()
    .map_err(StoreError::from)
}

fn current_revision(db: &Connection) -> Result<u64> {
    let current: i64 = db.query_row(
        "SELECT coalesce(max(plan_revision), 0) FROM plan_revisions",
        [],
        |row| row.get(0),
    )?;
    u64::try_from(current).map_err(|_| StoreError::Corrupt("plan revision is invalid".into()))
}

impl SqliteStore {
    /// Store one unsigned proposal. The same key and digest replay the existing
    /// revision. A different digest for that key conflicts. A stale parent
    /// returns the current revision and does not rebase.
    pub fn apply_plan_proposal(
        &mut self,
        raw: &[u8],
        expected_parent: u64,
        idempotency_key: &str,
    ) -> Result<PlanProposalReceipt> {
        if raw.is_empty() || raw.len() > PROPOSAL_LIMIT {
            return Err(invalid("plan proposal exceeds 256 KiB"));
        }
        if !identifier(idempotency_key) {
            return Err(invalid("invalid plan idempotency key"));
        }
        let digest = sha256_hex(raw);
        let path = store_path(&self.connection)?;
        let project_store = path.to_string_lossy().into_owned();
        if project_store.is_empty() || project_store.len() > 4096 {
            return Err(invalid("plan project store path is invalid"));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema31(&tx)?;
        if let Some(existing) = lookup_proposal(&tx, &project_store, idempotency_key)? {
            if sha256_hex(&existing.payload) != existing.digest {
                return Err(StoreError::Corrupt("plan proposal digest mismatch".into()));
            }
            if existing.digest != digest {
                return Err(StoreError::Conflict);
            }
            let receipt = PlanProposalReceipt {
                plan_revision: existing.plan_revision,
                proposal_id: existing.proposal_id,
                digest: existing.digest,
                replayed: true,
            };
            tx.commit()?;
            return Ok(receipt);
        }
        let proposal = parse_proposal(raw)?;
        reject_cycles(&tx, &proposal)?;
        let current = current_revision(&tx)?;
        if current != expected_parent {
            return Err(StoreError::StalePlanParent(current));
        }
        let next = current
            .checked_add(1)
            .ok_or_else(|| invalid("plan revision exhausted"))?;
        let proposal_id =
            sha256_hex(format!("{project_store}\0{idempotency_key}\0{digest}").as_bytes());
        let created_unix_ms = jiff::Timestamp::now().as_millisecond();
        tx.execute(
            "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('plan.proposed',?1,?2,1,?3)",
            params![
                proposal_id,
                integer(next)?,
                serde_json::json!({
                    "proposal_id": proposal_id,
                    "digest": digest,
                    "parent_revision": current,
                    "contract_count": proposal.contracts.len(),
                })
                .to_string()
            ],
        )?;
        tx.execute(
            "INSERT INTO plan_proposals(proposal_id,project_store,idempotency_key,payload_digest,payload,parent_revision,plan_revision,created_unix_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                proposal_id,
                project_store,
                idempotency_key,
                digest,
                raw,
                integer(current)?,
                integer(next)?,
                created_unix_ms
            ],
        )?;
        tx.execute(
            "INSERT INTO plan_revisions(plan_revision,parent_revision,proposal_id,payload_digest,contract_texts,created_unix_ms) VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                integer(next)?,
                integer(current)?,
                proposal_id,
                digest,
                proposal.contract_texts,
                created_unix_ms
            ],
        )?;
        // A new revision starts its own replan budget. Older rows stay put.
        record_replan_reset(&tx, next, created_unix_ms)?;
        tx.commit()?;
        Ok(PlanProposalReceipt {
            plan_revision: next,
            proposal_id,
            digest,
            replayed: false,
        })
    }
}

const WAITS_SCHEMA: u32 = 36;
const AUTOMATIC_REPLANS: i64 = 2;

fn schema36(db: &Connection) -> Result<()> {
    check_schema(db)?;
    let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < WAITS_SCHEMA {
        return Err(StoreError::UnsupportedSchema(version));
    }
    Ok(())
}
fn wait_condition(value: &str) -> bool {
    matches!(
        value,
        "dependency_evidence"
            | "user_decision"
            | "resource_availability"
            | "adapter_recovery"
            | "validation_completion"
    )
}
fn record_replan_reset(tx: &Connection, plan_revision: u64, now: i64) -> Result<()> {
    let version: u32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < WAITS_SCHEMA {
        return Ok(());
    }
    tx.execute(
        "INSERT INTO replan_budget_resets(plan_revision, reset_unix_ms) VALUES(?1,?2)",
        params![integer(plan_revision)?, now],
    )?;
    Ok(())
}
fn blocker_fingerprint(task_id: &str, category: &str, reason: &str) -> String {
    sha256_hex(format!("{task_id}\0{category}\0{reason}").as_bytes())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WaitRegistration {
    pub wait_id: String,
    pub cursor_sequence: i64,
    pub already_registered: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WaitReplay {
    pub wait_id: String,
    pub cursor_sequence: i64,
    pub replayed_through: i64,
    pub events_applied: i64,
    pub wake_requested: bool,
    /// Wake requests another look. It is not evidence the condition holds.
    pub proved: bool,
    pub already_replayed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PullRequestPoll {
    pub url: String,
    pub check: String,
    pub closed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ReplanDecision {
    Automatic {
        replan_id: String,
        proposal_id: String,
        automatic_count: i64,
    },
    Escalated {
        replan_id: String,
        inbox_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InfrastructureRetry {
    pub attempt_id: String,
    pub ordinal: i64,
    pub attempts_consumed: i64,
    pub max_attempts_per_task: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AcceptanceRework {
    pub attempt_id: String,
    pub attempts_consumed: i64,
    pub contract_revision: Option<i64>,
}

struct FeedbackRow {
    category: String,
    task_id: String,
    reason: String,
}

fn load_feedback(tx: &Connection, feedback_id: &str) -> Result<Option<FeedbackRow>> {
    tx.query_row(
        "SELECT category, task_id, reason FROM feedback_items WHERE feedback_id=?1",
        [feedback_id],
        |row| {
            Ok(FeedbackRow {
                category: row.get(0)?,
                task_id: row.get(1)?,
                reason: row.get(2)?,
            })
        },
    )
    .optional()
    .map_err(StoreError::from)
}

fn automatic_count(tx: &Connection, plan_revision: u64, fingerprint: &str) -> Result<i64> {
    let count: i64 = tx.query_row(
        "SELECT count(*) FROM replan_requests WHERE plan_revision=?1 AND blocker_fingerprint=?2 AND outcome='automatic' AND created_unix_ms >= coalesce((SELECT reset_unix_ms FROM replan_budget_resets WHERE plan_revision=?1), 0)",
        params![integer(plan_revision)?, fingerprint],
        |row| row.get(0),
    )?;
    Ok(count)
}

fn attempt_budget(tx: &Connection, task_id: &str) -> Result<(i64, i64)> {
    let consumed: i64 = tx.query_row(
        "SELECT count(*) FROM attempts WHERE task_id=?1",
        [task_id],
        |row| row.get(0),
    )?;
    let limit: i64 = tx.query_row(
        "SELECT max_attempts_per_task FROM scheduler_policy WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    Ok((consumed, limit))
}

fn decision_from_row(
    outcome: &str,
    replan_id: String,
    proposal_id: Option<String>,
    inbox_id: Option<String>,
    automatic_count: i64,
) -> Result<ReplanDecision> {
    match outcome {
        "automatic" => Ok(ReplanDecision::Automatic {
            replan_id,
            proposal_id: proposal_id
                .ok_or_else(|| StoreError::Corrupt("automatic replan lacks a proposal".into()))?,
            automatic_count,
        }),
        "escalated" => Ok(ReplanDecision::Escalated {
            replan_id,
            inbox_id: inbox_id.ok_or_else(|| {
                StoreError::Corrupt("escalated replan lacks an inbox item".into())
            })?,
        }),
        _ => Err(StoreError::Corrupt("replan outcome is invalid".into())),
    }
}

const REPLAN_OWNER: &str = "replan-controller";

/// Ack the current lease. An unexpired claim held by someone else conflicts.
/// An expired lease is marked expired so the next epoch can ack.
fn ack_replan_proposal(
    tx: &Connection,
    feedback_id: &str,
    proposal_id: &str,
    now: i64,
) -> Result<()> {
    let state: String = tx.query_row(
        "SELECT state FROM feedback_items WHERE feedback_id=?1",
        [feedback_id],
        |row| row.get(0),
    )?;
    if state == "acked" {
        let existing: String = tx.query_row(
            "SELECT replan_proposal_id FROM feedback_items WHERE feedback_id=?1",
            [feedback_id],
            |row| row.get(0),
        )?;
        if existing == proposal_id {
            return Ok(());
        }
        return Err(StoreError::Conflict);
    }
    if state != "open" && state != "claimed" {
        return Err(StoreError::Conflict);
    }
    let active: Option<(i64, String, i64)> = tx
        .query_row(
            "SELECT claim_epoch, owner, lease_until_ms FROM feedback_claims WHERE feedback_id=?1 AND state='active'",
            [feedback_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let epoch = if let Some((epoch, held_by, lease_until)) = active {
        if now < lease_until {
            if held_by != REPLAN_OWNER {
                return Err(StoreError::Conflict);
            }
            epoch
        } else {
            let expired = tx.execute(
                "UPDATE feedback_claims SET state='expired' WHERE feedback_id=?1 AND claim_epoch=?2 AND state='active'",
                params![feedback_id, epoch],
            )?;
            if expired != 1 {
                return Err(StoreError::Conflict);
            }
            insert_replan_claim(tx, feedback_id, now)?
        }
    } else {
        insert_replan_claim(tx, feedback_id, now)?
    };
    if state == "open" {
        let claimed = tx.execute(
            "UPDATE feedback_items SET state='claimed' WHERE feedback_id=?1 AND state='open' AND replan_proposal_id IS NULL",
            [feedback_id],
        )?;
        if claimed != 1 {
            return Err(StoreError::Conflict);
        }
    }
    let acked_claim = tx.execute(
        "UPDATE feedback_claims SET state='acked' WHERE feedback_id=?1 AND claim_epoch=?2 AND state='active'",
        params![feedback_id, epoch],
    )?;
    if acked_claim != 1 {
        return Err(StoreError::Conflict);
    }
    let acked = tx.execute(
        "UPDATE feedback_items SET state='acked', replan_proposal_id=?2 WHERE feedback_id=?1 AND state='claimed' AND replan_proposal_id IS NULL",
        params![feedback_id, proposal_id],
    )?;
    if acked != 1 {
        return Err(StoreError::Conflict);
    }
    Ok(())
}

fn insert_replan_claim(tx: &Connection, feedback_id: &str, now: i64) -> Result<i64> {
    let next: i64 = tx.query_row(
        "SELECT coalesce(max(claim_epoch), 0) + 1 FROM feedback_claims WHERE feedback_id=?1",
        [feedback_id],
        |row| row.get(0),
    )?;
    let until = now
        .checked_add(60_000)
        .ok_or_else(|| invalid("lease exceeds clock range"))?;
    tx.execute(
        "INSERT INTO feedback_claims(feedback_id,claim_epoch,owner,lease_until_ms,state,claimed_unix_ms) VALUES(?1,?2,?3,?4,'active',?5)",
        params![feedback_id, next, REPLAN_OWNER, until, now],
    )?;
    Ok(next)
}

fn insert_escalation_inbox(
    tx: &Connection,
    inbox_id: &str,
    feedback_id: &str,
    feedback: &FeedbackRow,
    plan_revision: u64,
    now: i64,
) -> Result<()> {
    let created =
        jiff::Timestamp::from_millisecond(now).map_err(|error| invalid(&error.to_string()))?;
    let content = crate::domain::InboxContent {
        id: inbox_id.to_string(),
        kind: "replan-escalation".into(),
        subject: feedback.task_id.clone(),
        created: created.to_string(),
        summary: format!("replan budget exhausted for {}", feedback.task_id),
        body: format!(
            "impact: {} blocks {} at plan {plan_revision}. evidence: feedback {feedback_id} ({}). options: approve a new plan revision or stop this blocker.",
            feedback.reason, feedback.task_id, feedback.category
        ),
    };
    content.validate().map_err(StoreError::Invalid)?;
    let payload = serde_json::to_string(&content).map_err(|error| invalid(&error.to_string()))?;
    let hash = format!("{:x}", Sha256::digest(payload.as_bytes()));
    tx.execute(
        "INSERT INTO inbox_items VALUES(?1,?2,?3,?4,?5,?6)",
        params![content.id, integer(1)?, payload, hash, false, false],
    )?;
    tx.execute(
        "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('replan.escalated',?1,1,1,?2)",
        params![
            inbox_id,
            serde_json::json!({
                "feedback_id": feedback_id,
                "task_id": feedback.task_id,
                "plan_revision": plan_revision,
            })
            .to_string()
        ],
    )?;
    Ok(())
}

impl SqliteStore {
    /// Register the wait and the event cursor together. A repeat returns that cursor.
    pub fn register_wait(
        &mut self,
        task_id: &str,
        attempt_id: Option<&str>,
        condition: &str,
    ) -> Result<WaitRegistration> {
        if !identifier(task_id) || !wait_condition(condition) {
            return Err(invalid("wait is invalid"));
        }
        if let Some(attempt) = attempt_id {
            if !identifier(attempt) {
                return Err(invalid("wait attempt is invalid"));
            }
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema36(&tx)?;
        let task_exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM tasks WHERE id=?1)",
            [task_id],
            |row| row.get(0),
        )?;
        if !task_exists {
            return Err(invalid("wait task is missing"));
        }
        if let Some(attempt) = attempt_id {
            let attempt_exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM attempts WHERE id=?1 AND task_id=?2)",
                params![attempt, task_id],
                |row| row.get(0),
            )?;
            if !attempt_exists {
                return Err(invalid("wait attempt is missing"));
            }
        }
        let plan_revision = current_revision(&tx)?;
        let attempt_key = attempt_id.unwrap_or("");
        let wait_id = sha256_hex(
            format!("{task_id}\0{attempt_key}\0{condition}\0{plan_revision}").as_bytes(),
        );
        let existing: Option<i64> = tx
            .query_row(
                "SELECT cursor_sequence FROM wait_conditions WHERE wait_id=?1",
                [&wait_id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(cursor_sequence) = existing {
            tx.commit()?;
            return Ok(WaitRegistration {
                wait_id,
                cursor_sequence,
                already_registered: true,
            });
        }
        let now = jiff::Timestamp::now().as_millisecond();
        tx.execute(
            "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('wait.registered',?1,1,1,?2)",
            params![
                wait_id,
                serde_json::json!({
                    "task_id": task_id,
                    "condition": condition,
                    "plan_revision": plan_revision,
                })
                .to_string()
            ],
        )?;
        let cursor_sequence =
            i64::try_from(head(&tx)?).map_err(|_| invalid("wait cursor exceeds range"))?;
        tx.execute(
            "INSERT INTO wait_conditions(wait_id,task_id,attempt_id,condition,plan_revision,cursor_sequence,state,replayed_through,wake_requested,created_unix_ms) VALUES(?1,?2,?3,?4,?5,?6,'waiting',NULL,0,?7)",
            params![
                wait_id,
                task_id,
                attempt_id,
                condition,
                integer(plan_revision)?,
                cursor_sequence,
                now
            ],
        )?;
        tx.commit()?;
        Ok(WaitRegistration {
            wait_id,
            cursor_sequence,
            already_registered: false,
        })
    }

    /// Apply events after the cursor one time. A second call does not apply them again.
    pub fn replay_wait(&mut self, wait_id: &str) -> Result<WaitReplay> {
        if !identifier(wait_id) {
            return Err(invalid("wait is invalid"));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema36(&tx)?;
        let row: Option<(i64, String, Option<i64>, i64)> = tx
            .query_row(
                "SELECT cursor_sequence, state, replayed_through, wake_requested FROM wait_conditions WHERE wait_id=?1",
                [wait_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((cursor_sequence, state, replayed_through, wake_requested)) = row else {
            return Err(invalid("wait is not registered"));
        };
        if state == "replayed" {
            let events_applied: i64 = tx.query_row(
                "SELECT count(*) FROM wait_replay_events WHERE wait_id=?1",
                [wait_id],
                |row| row.get(0),
            )?;
            let replayed_through = replayed_through
                .ok_or_else(|| StoreError::Corrupt("replayed wait has no cursor".into()))?;
            tx.commit()?;
            return Ok(WaitReplay {
                wait_id: wait_id.to_string(),
                cursor_sequence,
                replayed_through,
                events_applied,
                wake_requested: wake_requested == 1,
                proved: false,
                already_replayed: true,
            });
        }
        let events: Vec<(i64, String)> = {
            let mut stmt = tx.prepare(
                "SELECT sequence, kind FROM events WHERE sequence > ?1 ORDER BY sequence",
            )?;
            let rows = stmt.query_map(params![cursor_sequence], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(StoreError::from)?
        };
        let mut wake = false;
        for (sequence, kind) in &events {
            let is_wake = kind == "wait.wake";
            wake |= is_wake;
            tx.execute(
                "INSERT INTO wait_replay_events(wait_id,event_sequence,kind,wake) VALUES(?1,?2,?3,?4)",
                params![wait_id, sequence, kind, i64::from(is_wake)],
            )?;
        }
        let replayed_through =
            i64::try_from(head(&tx)?).map_err(|_| invalid("wait cursor exceeds range"))?;
        let updated = tx.execute(
            "UPDATE wait_conditions SET state='replayed', replayed_through=?2, wake_requested=?3 WHERE wait_id=?1 AND state='waiting'",
            params![wait_id, replayed_through, i64::from(wake)],
        )?;
        if updated != 1 {
            return Err(StoreError::Conflict);
        }
        tx.commit()?;
        Ok(WaitReplay {
            wait_id: wait_id.to_string(),
            cursor_sequence,
            replayed_through,
            events_applied: i64::try_from(events.len())
                .map_err(|_| invalid("wait replay exceeds range"))?,
            wake_requested: wake,
            proved: false,
            already_replayed: false,
        })
    }

    /// A pull-request poll is not feedback and not a replan. This writes nothing.
    pub fn request_replan_from_poll(&mut self, poll: &PullRequestPoll) -> Result<ReplanDecision> {
        let _ = (poll.url.as_str(), poll.check.as_str(), poll.closed);
        Err(invalid("pull-request poll is not a replan trigger"))
    }

    /// Two automatic replans for this blocker and plan revision, then one inbox item.
    /// The third does not insert a plan proposal.
    pub fn request_replan(&mut self, feedback_id: &str) -> Result<ReplanDecision> {
        if !identifier(feedback_id) {
            return Err(invalid("replan requires a schema 29 feedback row"));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema36(&tx)?;
        if let Some(decision) = existing_replan(&tx, feedback_id)? {
            tx.commit()?;
            return Ok(decision);
        }
        let Some(feedback) = load_feedback(&tx, feedback_id)? else {
            return Err(invalid("replan requires a schema 29 feedback row"));
        };
        if !matches!(
            feedback.category.as_str(),
            "verifier_rejection" | "integrator_rejection" | "integrator_conflict" | "invalidation"
        ) {
            return Err(invalid("pull-request poll is not a replan trigger"));
        }
        let plan_revision = current_revision(&tx)?;
        let fingerprint =
            blocker_fingerprint(&feedback.task_id, &feedback.category, &feedback.reason);
        let used = automatic_count(&tx, plan_revision, &fingerprint)?;
        let now = jiff::Timestamp::now().as_millisecond();
        if used < AUTOMATIC_REPLANS {
            let replan_id = sha256_hex(format!("replan\0{feedback_id}").as_bytes());
            let proposal_id = replan_id.clone();
            tx.execute(
                "INSERT INTO replan_requests(replan_id,feedback_id,plan_revision,blocker_fingerprint,outcome,proposal_id,inbox_id,created_unix_ms) VALUES(?1,?2,?3,?4,'automatic',?5,NULL,?6)",
                params![
                    replan_id,
                    feedback_id,
                    integer(plan_revision)?,
                    fingerprint,
                    proposal_id,
                    now
                ],
            )?;
            ack_replan_proposal(&tx, feedback_id, &proposal_id, now)?;
            tx.commit()?;
            return Ok(ReplanDecision::Automatic {
                replan_id,
                proposal_id,
                automatic_count: used + 1,
            });
        }
        if let Some(decision) = existing_escalation(&tx, plan_revision, &fingerprint)? {
            tx.commit()?;
            return Ok(decision);
        }
        let replan_id = sha256_hex(format!("replan\0{feedback_id}").as_bytes());
        let inbox_id = sha256_hex(format!("escalate\0{plan_revision}\0{fingerprint}").as_bytes());
        insert_escalation_inbox(&tx, &inbox_id, feedback_id, &feedback, plan_revision, now)?;
        tx.execute(
            "INSERT INTO replan_requests(replan_id,feedback_id,plan_revision,blocker_fingerprint,outcome,proposal_id,inbox_id,created_unix_ms) VALUES(?1,?2,?3,?4,'escalated',NULL,?5,?6)",
            params![
                replan_id,
                feedback_id,
                integer(plan_revision)?,
                fingerprint,
                inbox_id,
                now
            ],
        )?;
        tx.commit()?;
        Ok(ReplanDecision::Escalated {
            replan_id,
            inbox_id,
        })
    }

    /// Same attempt. The retry row is not an attempt, so max_attempts_per_task stays put.
    pub fn retry_infrastructure(
        &mut self,
        task_id: &str,
        attempt_id: &str,
    ) -> Result<InfrastructureRetry> {
        if !identifier(task_id) || !identifier(attempt_id) {
            return Err(invalid("infrastructure retry is invalid"));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema36(&tx)?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM attempts WHERE id=?1 AND task_id=?2)",
            params![attempt_id, task_id],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(invalid("infrastructure retry attempt is missing"));
        }
        let ordinal: i64 = tx.query_row(
            "SELECT coalesce(max(retry_ordinal), 0) + 1 FROM attempt_infrastructure_retries WHERE attempt_id=?1",
            [attempt_id],
            |row| row.get(0),
        )?;
        let retry_id = sha256_hex(format!("{attempt_id}\0{ordinal}").as_bytes());
        let now = jiff::Timestamp::now().as_millisecond();
        tx.execute(
            "INSERT INTO attempt_infrastructure_retries(retry_id,attempt_id,task_id,retry_ordinal,created_unix_ms) VALUES(?1,?2,?3,?4,?5)",
            params![retry_id, attempt_id, task_id, ordinal, now],
        )?;
        let (attempts_consumed, max_attempts_per_task) = attempt_budget(&tx, task_id)?;
        tx.commit()?;
        Ok(InfrastructureRetry {
            attempt_id: attempt_id.to_string(),
            ordinal,
            attempts_consumed,
            max_attempts_per_task,
        })
    }

    /// Acceptance failure counts against the attempt cap. The row is already
    /// terminated, so it does not keep a live reservation without inputs.
    /// A changed contract must already be installed; this does not write one.
    /// The previous verification run is left as stored.
    pub fn rework_acceptance(
        &mut self,
        feedback_id: &str,
        contract_changed: bool,
    ) -> Result<AcceptanceRework> {
        if !identifier(feedback_id) {
            return Err(invalid("acceptance rework requires verifier feedback"));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema36(&tx)?;
        let Some(feedback) = load_feedback(&tx, feedback_id)? else {
            return Err(invalid("acceptance rework requires verifier feedback"));
        };
        if feedback.category != "verifier_rejection" {
            return Err(invalid("acceptance rework requires verifier feedback"));
        }
        let contract_revision = if contract_changed {
            Some(installed_rework_revision(&tx, &feedback.task_id)?)
        } else {
            None
        };
        let attempt_id = format!("rework-{}", &feedback_id[..32]);
        let existing: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM attempts WHERE id=?1 AND task_id=?2)",
            params![attempt_id, feedback.task_id],
            |row| row.get(0),
        )?;
        if existing {
            let (attempts_consumed, _) = attempt_budget(&tx, &feedback.task_id)?;
            tx.commit()?;
            return Ok(AcceptanceRework {
                attempt_id,
                attempts_consumed,
                contract_revision,
            });
        }
        let (consumed, limit) = attempt_budget(&tx, &feedback.task_id)?;
        if consumed >= limit {
            return Err(invalid("task attempt limit reached"));
        }
        // Terminated on insert: cancel has nothing to release, and the row still counts.
        tx.execute(
            "INSERT INTO attempts(id,task_id,revision,state,snapshot,reservation,termination_observed) VALUES(?1,?2,1,'failed',NULL,?3,1)",
            params![attempt_id, feedback.task_id, format!("rework:{attempt_id}")],
        )?;
        tx.execute(
            "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('attempt.rework',?1,1,1,?2)",
            params![
                attempt_id,
                serde_json::json!({
                    "task_id": feedback.task_id,
                    "feedback_id": feedback_id,
                    "contract_revision": contract_revision,
                })
                .to_string()
            ],
        )?;
        let (attempts_consumed, _) = attempt_budget(&tx, &feedback.task_id)?;
        tx.commit()?;
        Ok(AcceptanceRework {
            attempt_id,
            attempts_consumed,
            contract_revision,
        })
    }
}

fn existing_replan(tx: &Connection, feedback_id: &str) -> Result<Option<ReplanDecision>> {
    let row: Option<(String, String, Option<String>, Option<String>, i64)> = tx
        .query_row(
            "SELECT replan_id, outcome, proposal_id, inbox_id, plan_revision FROM replan_requests WHERE feedback_id=?1",
            [feedback_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .optional()?;
    let Some((replan_id, outcome, proposal_id, inbox_id, plan_revision)) = row else {
        return Ok(None);
    };
    let fingerprint: String = tx.query_row(
        "SELECT blocker_fingerprint FROM replan_requests WHERE replan_id=?1",
        [&replan_id],
        |row| row.get(0),
    )?;
    let count = automatic_count(
        tx,
        u64::try_from(plan_revision)
            .map_err(|_| StoreError::Corrupt("plan revision is invalid".into()))?,
        &fingerprint,
    )?;
    Ok(Some(decision_from_row(
        &outcome,
        replan_id,
        proposal_id,
        inbox_id,
        count,
    )?))
}

fn existing_escalation(
    tx: &Connection,
    plan_revision: u64,
    fingerprint: &str,
) -> Result<Option<ReplanDecision>> {
    let row: Option<(String, String)> = tx
        .query_row(
            "SELECT replan_id, inbox_id FROM replan_requests WHERE plan_revision=?1 AND blocker_fingerprint=?2 AND outcome='escalated'",
            params![integer(plan_revision)?, fingerprint],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    Ok(row.map(|(replan_id, inbox_id)| ReplanDecision::Escalated {
        replan_id,
        inbox_id,
    }))
}

/// Newer than the stored verification, and only if a signed install already wrote it.
fn installed_rework_revision(tx: &Connection, task_id: &str) -> Result<i64> {
    let verified: Option<i64> = tx.query_row(
        "SELECT max(contract_revision) FROM verification_runs WHERE task_id=?1",
        [task_id],
        |row| row.get(0),
    )?;
    let floor = verified.unwrap_or(0);
    let revision: Option<i64> = tx.query_row(
        "SELECT max(c.contract_revision) FROM task_contracts c WHERE c.task_id=?1 AND c.contract_revision>?2 AND EXISTS (SELECT 1 FROM events e WHERE e.kind='contract.installed' AND e.entity=c.task_id AND e.revision=c.contract_revision)",
        params![task_id, floor],
        |row| row.get(0),
    )?;
    revision.ok_or_else(|| invalid("acceptance rework has no contract revision"))
}

pub fn propose_plan(
    project: &Path,
    document: &Path,
    expected_parent: u64,
    idempotency_key: &str,
) -> Result<PlanProposalReceipt> {
    let _guard =
        crate::migration::runtime_mutation(project).map_err(|error| invalid(&error.to_string()))?;
    let bytes =
        crate::migration::read_plan_file(document).map_err(|error| invalid(&error.to_string()))?;
    let mut db =
        crate::migration::open_active(project).map_err(|error| invalid(&error.to_string()))?;
    db.apply_plan_proposal(&bytes, expected_parent, idempotency_key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        Attempt, AttemptId, AttemptState, DependencyRequirement, Mutation, QueueRequest, TaskId,
        TaskState,
    };
    use crate::store::feedback::LocalFeedback;

    fn fixture() -> (tempfile::TempDir, SqliteStore) {
        let temp = tempfile::tempdir().unwrap();
        let db = SqliteStore::create(&temp.path().join("state.db")).unwrap();
        (temp, db)
    }
    fn user_version(db: &Connection) -> u32 {
        db.query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }
    fn count(db: &Connection, sql: &str) -> i64 {
        db.query_row(sql, [], |row| row.get(0)).unwrap()
    }
    fn table_exists(db: &Connection, name: &str) -> bool {
        db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [name],
            |row| row.get(0),
        )
        .unwrap()
    }
    fn proposal(contracts: &str) -> Vec<u8> {
        format!(r#"{{"version":1,"contracts":[{contracts}]}}"#).into_bytes()
    }
    fn one(task: &str, text: &str, deps: &str) -> String {
        format!(r#"{{"task_id":"{task}","text":"{text}","dependencies":[{deps}]}}"#)
    }
    fn edge(predecessor: &str) -> String {
        format!(r#"{{"predecessor":"{predecessor}","requirement":"verified_result"}}"#)
    }

    #[test]
    fn replay_of_the_same_bytes_returns_one_plan_revision() {
        let (_temp, mut db) = fixture();
        let bytes = proposal(&one("alpha", "ship the widget", ""));
        let first = db.apply_plan_proposal(&bytes, 0, "replay-key").unwrap();
        assert!(!first.replayed);
        assert_eq!(first.plan_revision, 1);
        let second = db.apply_plan_proposal(&bytes, 0, "replay-key").unwrap();
        assert!(second.replayed);
        assert_eq!(
            second,
            PlanProposalReceipt {
                replayed: true,
                ..first
            }
        );
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM plan_revisions"),
            1
        );
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM plan_proposals"),
            1
        );
        assert_eq!(
            count(
                &db.connection,
                "SELECT count(*) FROM events WHERE kind='plan.proposed'"
            ),
            1
        );
        let stored: Vec<u8> = db
            .connection
            .query_row("SELECT payload FROM plan_proposals", [], |row| row.get(0))
            .unwrap();
        assert_eq!(stored, bytes);
        let texts: String = db
            .connection
            .query_row("SELECT contract_texts FROM plan_revisions", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(texts.contains("ship the widget"));
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM task_contracts"),
            0
        );
    }

    #[test]
    fn stale_parent_conflicts_and_returns_the_current_revision() {
        let (_temp, mut db) = fixture();
        let first = proposal(&one("alpha", "first text", ""));
        let stored = db.apply_plan_proposal(&first, 0, "key-1").unwrap();
        let before = count(&db.connection, "SELECT count(*) FROM plan_revisions");
        let error = db
            .apply_plan_proposal(&proposal(&one("beta", "other text", "")), 0, "key-2")
            .unwrap_err();
        assert!(
            matches!(error, StoreError::StalePlanParent(current) if current == stored.plan_revision)
        );
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM plan_revisions"),
            before
        );
        let parent: i64 = db
            .connection
            .query_row(
                "SELECT parent_revision FROM plan_revisions WHERE plan_revision=?1",
                [stored.plan_revision as i64],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(parent, 0);
        let digest: String = db
            .connection
            .query_row(
                "SELECT payload_digest FROM plan_revisions WHERE plan_revision=?1",
                [stored.plan_revision as i64],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(digest, stored.digest);
    }

    #[test]
    fn same_key_and_different_bytes_conflict() {
        let (_temp, mut db) = fixture();
        db.apply_plan_proposal(&proposal(&one("alpha", "one", "")), 0, "same-key")
            .unwrap();
        let error = db
            .apply_plan_proposal(&proposal(&one("alpha", "two", "")), 1, "same-key")
            .unwrap_err();
        assert!(matches!(error, StoreError::Conflict));
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM plan_revisions"),
            1
        );
    }

    #[test]
    fn dependency_cycle_is_rejected_without_a_revision_or_reservation() {
        let (_temp, mut db) = fixture();
        db.commit(Commit {
            expected_head: 0,
            mutations: ["a", "b"]
                .into_iter()
                .map(|id| Mutation::Task {
                    expected: None,
                    next: Task {
                        id: TaskId::new(id).unwrap(),
                        revision: 1,
                        state: TaskState::Draft,
                        title: id.into(),
                        active_attempt: None,
                    },
                })
                .collect(),
        })
        .unwrap();
        let head = db.read_snapshot(None).unwrap().head;
        db.queue_task(
            &TaskId::new("a").unwrap(),
            1,
            head,
            &QueueRequest {
                priority: 0,
                dependencies: vec![Dependency {
                    predecessor: TaskId::new("b").unwrap(),
                    requirement: DependencyRequirement::VerifiedResult,
                }],
            },
            0,
        )
        .unwrap();
        let attempts = count(&db.connection, "SELECT count(*) FROM attempts");
        let dependencies = count(&db.connection, "SELECT count(*) FROM task_dependencies");
        let queued = count(&db.connection, "SELECT count(*) FROM task_queue");
        let cyclic = proposal(&format!(
            "{},{}",
            one("a", "loop", &edge("b")),
            one("b", "loop", &edge("a"))
        ));
        let error = db.apply_plan_proposal(&cyclic, 0, "cycle-key").unwrap_err();
        assert!(matches!(error, StoreError::Invalid(message) if message.contains("cycle")));
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM plan_revisions"),
            0
        );
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM attempts"),
            attempts
        );
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM task_dependencies"),
            dependencies
        );
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM task_queue"),
            queued
        );
        let with_live_edge = proposal(&one("b", "closes the live edge", &edge("a")));
        let error = db
            .apply_plan_proposal(&with_live_edge, 0, "live-cycle")
            .unwrap_err();
        assert!(matches!(error, StoreError::Invalid(message) if message.contains("cycle")));
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM plan_revisions"),
            0
        );
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM task_contracts"),
            0
        );
    }

    #[test]
    fn payload_above_256_kib_is_refused_and_exact_limit_is_an_unsigned_proposal() {
        let (_temp, mut db) = fixture();
        let contracts_before = count(&db.connection, "SELECT count(*) FROM task_contracts");
        let attempts_before = count(&db.connection, "SELECT count(*) FROM attempts");
        let limit = proposal_of_len(PROPOSAL_LIMIT);
        assert_eq!(limit.len(), PROPOSAL_LIMIT);
        let stored = db.apply_plan_proposal(&limit, 0, "wide-key").unwrap();
        assert!(!stored.replayed);
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM plan_revisions"),
            1
        );
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM task_contracts"),
            contracts_before
        );
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM attempts"),
            attempts_before
        );
        let over = proposal_of_len(PROPOSAL_LIMIT + 1);
        let error = db.apply_plan_proposal(&over, 1, "wide-key-2").unwrap_err();
        assert!(matches!(error, StoreError::Invalid(message) if message.contains("256 KiB")));
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM plan_revisions"),
            1
        );
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM task_contracts"),
            contracts_before
        );
    }

    fn proposal_of_len(len: usize) -> Vec<u8> {
        let prefix = br#"{"version":1,"contracts":[{"task_id":"alpha","text":""#;
        let suffix = br#"","dependencies":[]}]}"#;
        assert!(len > prefix.len() + suffix.len());
        let mut out = Vec::with_capacity(len);
        out.extend_from_slice(prefix);
        out.resize(len - suffix.len(), b'a');
        out.extend_from_slice(suffix);
        assert_eq!(out.len(), len);
        out
    }

    #[test]
    fn proposal_path_does_not_call_verify_signature_or_write_contracts() {
        let source = include_str!("plans.rs");
        let production = source.split("mod tests").next().unwrap();
        let proposal = production
            .split("pub fn apply_plan_proposal")
            .nth(1)
            .unwrap()
            .split("fn schema36")
            .next()
            .unwrap();
        assert!(!proposal.contains("verify_signature"));
        assert!(!proposal.contains("task_contracts"));
        assert!(!proposal.contains("contract_scope_paths"));
        assert!(!proposal.contains("contract_named_resources"));
        assert!(!proposal.contains("install_contract"));
        assert!(!proposal.contains("reserve_"));
        assert!(!production.contains("verify_signature"));
        assert!(!production.contains("install_contract"));
        assert!(!production.contains("pr_polling"));
        assert!(!production.contains("reserve_"));
        let sql = include_str!("../../migrations/0031_plan_revisions.sql");
        assert!(!sql.contains("task_contracts"));
        assert!(!sql.contains("contract_scope_paths"));
        assert!(!sql.contains("contract_named_resources"));
        assert!(!sql.contains("verify_signature"));
        let cli = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cli.rs"),
        )
        .unwrap();
        let start = cli.find("Command::Plan{command}").unwrap();
        let rest = &cli[start..];
        let end = rest[1..]
            .find("\n        Command::")
            .map(|offset| offset + 1)
            .unwrap();
        let arm = &rest[..end];
        assert!(arm.contains("propose_plan"));
        assert!(!arm.contains("verify_signature"));
        assert!(!arm.contains("import_contract"));
        assert!(!arm.contains("reserve"));
        assert!(!arm.contains("task_contracts"));
        assert!(!arm.contains("contract_scope_paths"));
        assert!(!arm.contains("contract_named_resources"));
    }

    #[test]
    fn plan_propose_leaves_scope_tables_empty() {
        let (_temp, mut db) = fixture();
        assert!(table_exists(&db.connection, "contract_scope_paths"));
        assert!(table_exists(&db.connection, "contract_named_resources"));
        let paths = count(&db.connection, "SELECT count(*) FROM contract_scope_paths");
        let named = count(
            &db.connection,
            "SELECT count(*) FROM contract_named_resources",
        );
        let contracts = count(&db.connection, "SELECT count(*) FROM task_contracts");
        let attempts = count(&db.connection, "SELECT count(*) FROM attempts");
        let stored = db
            .apply_plan_proposal(
                &proposal(&one("alpha", "scope src/lib.rs schema lockfile", "")),
                0,
                "scope-key",
            )
            .unwrap();
        assert!(!stored.replayed);
        assert_eq!(stored.plan_revision, 1);
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM contract_scope_paths"),
            paths
        );
        assert_eq!(
            count(
                &db.connection,
                "SELECT count(*) FROM contract_named_resources"
            ),
            named
        );
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM task_contracts"),
            contracts
        );
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM attempts"),
            attempts
        );
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM plan_revisions"),
            1
        );
    }

    #[test]
    fn upgrade_v1_from_30_to_31_and_import_requires_current_schema() {
        let fresh = tempfile::tempdir().unwrap();
        let mut created = SqliteStore::create(&fresh.path().join("state.db")).unwrap();
        assert_eq!(user_version(&created.connection), 36);
        assert_eq!(
            created
                .connection
                .query_row("SELECT schema_version FROM store_meta", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            36
        );
        assert!(table_exists(&created.connection, "plan_proposals"));
        assert!(table_exists(&created.connection, "plan_revisions"));
        created.import_legacy(&"ab".repeat(32), &[], &[]).unwrap();
        drop(created);

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.db");
        let db = SqliteStore::create(&path).unwrap();
        drop(db);
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.execute_batch(
            "DROP TABLE IF EXISTS wait_replay_events; DROP TABLE IF EXISTS replan_requests; DROP TABLE IF EXISTS replan_budget_resets; DROP TABLE IF EXISTS attempt_infrastructure_retries; DROP TABLE IF EXISTS wait_conditions; DROP TABLE IF EXISTS resource_claims; DROP TABLE IF EXISTS delegation_stop_obligations; DROP TABLE IF EXISTS delegation_revocations; DROP TABLE IF EXISTS delegation_grants; DROP TABLE IF EXISTS capability_evidence; DROP TABLE IF EXISTS contract_named_resources; DROP TABLE IF EXISTS contract_scope_paths; DROP TABLE IF EXISTS plan_revisions; DROP TABLE IF EXISTS plan_proposals; UPDATE store_meta SET schema_version=30; PRAGMA user_version=30;",
        )
        .unwrap();
        drop(raw);
        let mut db = SqliteStore::open(&path).unwrap();
        assert_eq!(user_version(&db.connection), 30);
        assert!(!table_exists(&db.connection, "plan_revisions"));
        assert!(matches!(
            db.import_legacy(&"cd".repeat(32), &[], &[]),
            Err(StoreError::UnsupportedSchema(30))
        ));
        assert_eq!(user_version(&db.connection), 30);
        db.upgrade_v1().unwrap();
        assert_eq!(user_version(&db.connection), 36);
        assert_eq!(
            db.connection
                .query_row("SELECT schema_version FROM store_meta", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            36
        );
        assert!(table_exists(&db.connection, "plan_proposals"));
        assert!(table_exists(&db.connection, "plan_revisions"));
        check_schema(&db.connection).unwrap();
        drop(db);
        let reopened = SqliteStore::open(&path).unwrap();
        assert_eq!(user_version(&reopened.connection), 36);
        assert!(table_exists(&reopened.connection, "plan_revisions"));
    }

    fn seed_task(db: &mut SqliteStore, id: &str) {
        let head = db.read_snapshot(None).unwrap().head;
        db.commit(Commit {
            expected_head: head,
            mutations: vec![Mutation::Task {
                expected: None,
                next: Task {
                    id: TaskId::new(id).unwrap(),
                    revision: 1,
                    state: TaskState::Running,
                    title: id.into(),
                    active_attempt: None,
                },
            }],
        })
        .unwrap();
    }

    fn feedback_id(operation_id: &str) -> String {
        sha256_hex(format!("{operation_id}\0{}\0{}", 1, "verifier_rejection").as_bytes())
    }

    fn record_rejection(db: &mut SqliteStore, operation_id: &str, task: &str) -> String {
        db.testing_record_feedback(LocalFeedback {
            operation_id: operation_id.into(),
            outcome_revision: 1,
            category: "verifier_rejection".into(),
            task_id: task.into(),
            reason: "checks_failed".into(),
        })
        .unwrap();
        feedback_id(operation_id)
    }

    #[test]
    fn upgrade_v1_from_35_to_36_and_create_end_at_user_version_36() {
        let fresh = tempfile::tempdir().unwrap();
        let created_path = fresh.path().join("state.db");
        let created = SqliteStore::create(&created_path).unwrap();
        drop(created);
        assert_eq!(
            user_version(&rusqlite::Connection::open(&created_path).unwrap()),
            36
        );
        assert!(table_exists(
            &rusqlite::Connection::open(&created_path).unwrap(),
            "wait_conditions"
        ));
        assert!(table_exists(
            &rusqlite::Connection::open(&created_path).unwrap(),
            "replan_requests"
        ));
        let open_fn = include_str!("mod.rs")
            .split("pub fn open")
            .nth(1)
            .unwrap()
            .split("pub fn integrity_check")
            .next()
            .unwrap();
        assert!(!open_fn.contains("upgrade_v1"));
        assert!(!open_fn.contains("0036_waits"));
        let migration = include_str!("../../migrations/0036_waits.sql").to_ascii_lowercase();
        assert!(!migration.contains("update verification"));
        assert!(!migration.contains("delete from verification"));

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.db");
        let mut db = SqliteStore::create(&path).unwrap();
        seed_task(&mut db, "kept");
        drop(db);
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.execute_batch(
            "DROP TABLE IF EXISTS wait_replay_events; DROP TABLE IF EXISTS replan_requests; DROP TABLE IF EXISTS replan_budget_resets; DROP TABLE IF EXISTS attempt_infrastructure_retries; DROP TABLE IF EXISTS wait_conditions; UPDATE store_meta SET schema_version=35; PRAGMA user_version=35;",
        )
        .unwrap();
        drop(raw);
        let mut db = SqliteStore::open(&path).unwrap();
        assert_eq!(user_version(&db.connection), 35);
        assert!(!table_exists(&db.connection, "wait_conditions"));
        assert!(!table_exists(&db.connection, "replan_requests"));
        assert_eq!(db.read_snapshot(None).unwrap().tasks[0].title, "kept");
        db.upgrade_v1().unwrap();
        assert_eq!(user_version(&db.connection), 36);
        assert_eq!(
            db.connection
                .query_row("SELECT schema_version FROM store_meta", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            36
        );
        assert!(table_exists(&db.connection, "wait_conditions"));
        assert!(table_exists(&db.connection, "replan_requests"));
        assert!(table_exists(&db.connection, "replan_budget_resets"));
        assert!(table_exists(
            &db.connection,
            "attempt_infrastructure_retries"
        ));
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM wait_conditions"),
            0
        );
        check_schema(&db.connection).unwrap();
        drop(db);
        let reopened = SqliteStore::open(&path).unwrap();
        assert_eq!(user_version(&reopened.connection), 36);
    }

    #[test]
    fn restart_replays_the_wait_cursor_once() {
        let (_temp, mut db) = fixture();
        seed_task(&mut db, "task");
        let events_before = count(&db.connection, "SELECT count(*) FROM events");
        assert!(matches!(
            db.register_wait("missing", None, "dependency_evidence"),
            Err(StoreError::Invalid(_))
        ));
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM wait_conditions"),
            0
        );
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM events"),
            events_before
        );
        let registered = db
            .register_wait("task", None, "dependency_evidence")
            .unwrap();
        assert!(!registered.already_registered);
        let cursor_event: i64 = db
            .connection
            .query_row(
                "SELECT sequence FROM events WHERE kind='wait.registered' AND entity=?1",
                [&registered.wait_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(registered.cursor_sequence, cursor_event);
        let again = db
            .register_wait("task", None, "dependency_evidence")
            .unwrap();
        assert!(again.already_registered);
        assert_eq!(
            again,
            WaitRegistration {
                already_registered: true,
                ..registered.clone()
            }
        );
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM wait_conditions"),
            1
        );
        db.connection
            .execute(
                "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('wait.wake',?1,1,1,'{\"wake\":true}')",
                [&registered.wait_id],
            )
            .unwrap();
        db.connection
            .execute(
                "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('task.noted','task',1,1,'{}')",
                [],
            )
            .unwrap();
        let replayed = db.replay_wait(&registered.wait_id).unwrap();
        assert!(!replayed.already_replayed);
        assert!(replayed.wake_requested);
        assert!(!replayed.proved);
        assert_eq!(replayed.events_applied, 2);
        assert_eq!(replayed.cursor_sequence, registered.cursor_sequence);
        assert!(replayed.replayed_through > replayed.cursor_sequence);
        db.connection
            .execute(
                "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('wait.wake',?1,1,1,'{\"wake\":true}')",
                [&registered.wait_id],
            )
            .unwrap();
        let second = db.replay_wait(&registered.wait_id).unwrap();
        assert!(second.already_replayed);
        assert_eq!(second.events_applied, 2);
        assert!(!second.proved);
        assert_eq!(second.replayed_through, replayed.replayed_through);
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM wait_replay_events"),
            2
        );
        let registration_replayed: i64 = db
            .connection
            .query_row(
                "SELECT count(*) FROM wait_replay_events WHERE kind='wait.registered'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(registration_replayed, 0);
        let stored: (String, String) = db
            .connection
            .query_row("SELECT state, condition FROM wait_conditions", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(stored, ("replayed".into(), "dependency_evidence".into()));
        assert!(db
            .connection
            .execute("UPDATE wait_conditions SET wake_requested=0", [])
            .is_err());
    }

    #[test]
    fn third_replan_is_an_inbox_escalation_not_another_proposal() {
        let (_temp, mut db) = fixture();
        seed_task(&mut db, "task");
        let mut ids = Vec::new();
        for operation in ["op-1", "op-2", "op-3", "op-4"] {
            ids.push(record_rejection(&mut db, operation, "task"));
        }
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM plan_proposals"),
            0
        );
        let first = db.request_replan(&ids[0]).unwrap();
        let replay = db.request_replan(&ids[0]).unwrap();
        assert_eq!(first, replay);
        assert!(matches!(
            first,
            ReplanDecision::Automatic {
                automatic_count: 1,
                ..
            }
        ));
        let second = db.request_replan(&ids[1]).unwrap();
        assert!(matches!(
            second,
            ReplanDecision::Automatic {
                automatic_count: 2,
                ..
            }
        ));
        let third = db.request_replan(&ids[2]).unwrap();
        let ReplanDecision::Escalated { inbox_id, .. } = third.clone() else {
            panic!("third replan must escalate");
        };
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM plan_proposals"),
            0
        );
        assert_eq!(
            count(
                &db.connection,
                "SELECT count(*) FROM replan_requests WHERE outcome='automatic'"
            ),
            2
        );
        assert_eq!(
            count(
                &db.connection,
                "SELECT count(*) FROM replan_requests WHERE outcome='escalated'"
            ),
            1
        );
        assert_eq!(count(&db.connection, "SELECT count(*) FROM inbox_items"), 1);
        let kind: String = db
            .connection
            .query_row(
                "SELECT json_extract(payload, '$.kind') FROM inbox_items WHERE id=?1",
                [&inbox_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(kind, "replan-escalation");
        let third_state: (String, Option<String>) = db
            .connection
            .query_row(
                "SELECT state, replan_proposal_id FROM feedback_items WHERE feedback_id=?1",
                [&ids[2]],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(third_state, ("open".into(), None));
        let fourth = db.request_replan(&ids[3]).unwrap();
        assert_eq!(fourth, third);
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM plan_proposals"),
            0
        );
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM replan_requests"),
            3
        );
        assert_eq!(count(&db.connection, "SELECT count(*) FROM inbox_items"), 1);
        let proposed = db
            .apply_plan_proposal(&proposal(&one("task", "new plan", "")), 0, "reset-key")
            .unwrap();
        assert_eq!(proposed.plan_revision, 1);
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM replan_budget_resets"),
            1
        );
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM plan_proposals"),
            1
        );
        let fifth = record_rejection(&mut db, "op-5", "task");
        let reset = db.request_replan(&fifth).unwrap();
        assert!(matches!(
            reset,
            ReplanDecision::Automatic {
                automatic_count: 1,
                ..
            }
        ));
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM plan_proposals"),
            1
        );
        assert_eq!(count(&db.connection, "SELECT count(*) FROM inbox_items"), 1);
        assert_eq!(
            count(
                &db.connection,
                "SELECT count(*) FROM replan_requests WHERE outcome='automatic'"
            ),
            3
        );
    }

    #[test]
    fn pr_poll_fixture_does_not_create_a_replan() {
        let (_temp, mut db) = fixture();
        seed_task(&mut db, "task");
        let proposals = count(&db.connection, "SELECT count(*) FROM plan_proposals");
        let rejected = db.request_replan_from_poll(&PullRequestPoll {
            url: "https://github.com/owner/repo/pull/7".into(),
            check: "failure".into(),
            closed: true,
        });
        assert!(
            matches!(rejected, Err(StoreError::Invalid(message)) if message.contains("pull-request"))
        );
        assert!(matches!(
            db.testing_record_feedback(LocalFeedback {
                operation_id: "pr-poll-1".into(),
                outcome_revision: 1,
                category: "pr_poll".into(),
                task_id: "task".into(),
                reason: "https://github.com/owner/repo/pull/7".into(),
            }),
            Err(StoreError::Invalid(_))
        ));
        assert!(matches!(
            db.request_replan("pr-poll-1"),
            Err(StoreError::Invalid(_))
        ));
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM feedback_items"),
            0
        );
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM replan_requests"),
            0
        );
        assert_eq!(count(&db.connection, "SELECT count(*) FROM inbox_items"), 0);
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM plan_proposals"),
            proposals
        );
        assert_eq!(count(&db.connection, "SELECT count(*) FROM attempts"), 0);
    }

    #[test]
    fn infrastructure_retry_does_not_consume_max_attempts_per_task() {
        let (_temp, mut db) = fixture();
        seed_task(&mut db, "task");
        let head = db.read_snapshot(None).unwrap().head;
        db.commit(Commit {
            expected_head: head,
            mutations: vec![Mutation::Attempt {
                expected: None,
                next: Attempt {
                    id: AttemptId::new("attempt-1").unwrap(),
                    task: TaskId::new("task").unwrap(),
                    revision: 1,
                    state: AttemptState::Running,
                    snapshot: None,
                    reservation: "slot-1".into(),
                    termination_observed: false,
                },
            }],
        })
        .unwrap();
        let head = db.read_snapshot(None).unwrap().head;
        db.set_scheduler_policy(head, 1, 0, 1).unwrap();
        let feedback = record_rejection(&mut db, "accept-1", "task");
        let first = db.retry_infrastructure("task", "attempt-1").unwrap();
        assert_eq!(first.ordinal, 1);
        assert_eq!(first.attempts_consumed, 1);
        assert_eq!(first.max_attempts_per_task, 1);
        let second = db.retry_infrastructure("task", "attempt-1").unwrap();
        assert_eq!(second.ordinal, 2);
        assert_eq!(second.attempts_consumed, 1);
        assert_eq!(count(&db.connection, "SELECT count(*) FROM attempts"), 1);
        assert_eq!(
            count(
                &db.connection,
                "SELECT count(*) FROM attempt_infrastructure_retries"
            ),
            2
        );
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM replan_requests"),
            0
        );
        assert!(matches!(
            db.rework_acceptance(&feedback, false),
            Err(StoreError::Invalid(message)) if message.contains("attempt limit")
        ));
        assert_eq!(count(&db.connection, "SELECT count(*) FROM attempts"), 1);
    }

    fn seed_rejected_verification(db: &mut SqliteStore) {
        seed_task(db, "task");
        let head = db.read_snapshot(None).unwrap().head;
        db.commit(Commit {
            expected_head: head,
            mutations: vec![Mutation::Attempt {
                expected: None,
                next: Attempt {
                    id: AttemptId::new("attempt-1").unwrap(),
                    task: TaskId::new("task").unwrap(),
                    revision: 1,
                    state: AttemptState::Running,
                    snapshot: None,
                    reservation: "slot-1".into(),
                    termination_observed: false,
                },
            }],
        })
        .unwrap();
        let project_store = std::fs::canonicalize(db.connection.path().unwrap())
            .unwrap()
            .display()
            .to_string();
        let installed: i64 = db
            .connection
            .query_row("SELECT max(sequence) FROM events", [], |row| row.get(0))
            .unwrap();
        let raw = b"contract-v1";
        let digest = sha256_hex(raw);
        let base = "b".repeat(40);
        let oid = "a".repeat(40);
        db.connection.execute(
            "INSERT INTO task_contracts(task_id,contract_revision,plan_revision,project_store,expected_head,repository,base_oid,object_format,memory_snapshot_id,route,raw_bytes,raw_digest,installed_seq) VALUES('task',1,NULL,?1,0,'/tmp/repo',?2,'sha1',NULL,'verify_only',?3,?4,?5)",
            params![project_store, base, raw, digest, installed],
        ).unwrap();
        db.connection.execute(
            "INSERT INTO acceptance_policies(task_id,contract_revision,policy_id,body) VALUES('task',1,'policy-1','{\"version\":1}')",
            [],
        ).unwrap();
        let submission = "c".repeat(64);
        db.connection.execute(
            "INSERT INTO result_submissions(submission_id,project_store,idempotency_key,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,memory_snapshot_id,artifact_manifest,claimed_checks,created_unix_ms) VALUES(?1,?2,'submit-1',?3,'{}','task',1,?3,'attempt-1','/tmp/repo',?4,?5,'sha1',NULL,'[]','[]',0)",
            params![submission, project_store, digest, base, oid],
        ).unwrap();
        db.connection.execute(
            "INSERT INTO verification_runs(run_id,project_store,idempotency_key,payload_digest,submission_id,task_id,contract_revision,contract_digest,attempt_id,policy_id,policy_digest,commit_oid,tree_oid,object_format,memory_fence,isolation,argv,library_manifest,state,reason,exit_status,receipt_digest,store_device,store_inode,created_unix_ms) VALUES(?1,?2,'verify-1',?3,?4,'task',1,?3,'attempt-1','policy-1',?5,?6,NULL,'sha1',0,'linux-unshare-user-pid-mount-v1','[\"/usr/bin/true\"]','[]','rejected','checks_failed',1,NULL,1,1,0)",
            params![
                "d".repeat(64),
                project_store,
                digest,
                submission,
                sha256_hex(br#"{"version":1}"#),
                oid
            ],
        ).unwrap();
    }

    fn verification_snapshot(
        db: &Connection,
    ) -> (String, String, Option<String>, String, i64, Option<String>) {
        db.query_row(
            "SELECT run_id, state, reason, attempt_id, contract_revision, receipt_digest FROM verification_runs",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
        )
        .unwrap()
    }

    #[test]
    fn acceptance_rework_leaves_old_verification_rows_immutable() {
        let (_temp, mut db) = fixture();
        seed_rejected_verification(&mut db);
        let before = verification_snapshot(&db.connection);
        let old_digest: String = db
            .connection
            .query_row(
                "SELECT raw_digest FROM task_contracts WHERE contract_revision=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let feedback = record_rejection(&mut db, "accept-2", "task");
        assert!(matches!(
            db.rework_acceptance(&feedback, true),
            Err(StoreError::Invalid(message)) if message.contains("no contract revision")
        ));
        assert_eq!(count(&db.connection, "SELECT count(*) FROM attempts"), 1);
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM task_contracts"),
            1
        );
        let project_store = std::fs::canonicalize(db.connection.path().unwrap())
            .unwrap()
            .display()
            .to_string();
        let next_raw = b"contract-v2-installed";
        let next_digest = sha256_hex(next_raw);
        db.connection
            .execute(
                "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('contract.installed','task',2,1,?1)",
                [serde_json::json!({"digest": next_digest}).to_string()],
            )
            .unwrap();
        let installed: i64 = db
            .connection
            .query_row("SELECT max(sequence) FROM events", [], |row| row.get(0))
            .unwrap();
        db.connection
            .execute(
                "INSERT INTO task_contracts(task_id,contract_revision,plan_revision,project_store,expected_head,repository,base_oid,object_format,memory_snapshot_id,route,raw_bytes,raw_digest,installed_seq) VALUES('task',2,NULL,?1,0,'/tmp/repo',?2,'sha1',NULL,'verify_only',?3,?4,?5)",
                params![project_store, "b".repeat(40), next_raw, next_digest, installed],
            )
            .unwrap();
        let rework = db.rework_acceptance(&feedback, true).unwrap();
        assert_eq!(rework.attempts_consumed, 2);
        assert_eq!(rework.contract_revision, Some(2));
        assert_eq!(count(&db.connection, "SELECT count(*) FROM attempts"), 2);
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM task_contracts"),
            2
        );
        let held: (String, i64) = db
            .connection
            .query_row(
                "SELECT state, termination_observed FROM attempts WHERE id=?1",
                [&rework.attempt_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(held, ("failed".into(), 1));
        assert_eq!(
            count(
                &db.connection,
                "SELECT count(*) FROM attempts WHERE termination_observed=0"
            ),
            1
        );
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM attempt_inputs"),
            0
        );
        assert_eq!(verification_snapshot(&db.connection), before);
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM verified_results"),
            0
        );
        let kept: String = db
            .connection
            .query_row(
                "SELECT raw_digest FROM task_contracts WHERE contract_revision=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(kept, old_digest);
        let installed_digest: String = db
            .connection
            .query_row(
                "SELECT raw_digest FROM task_contracts WHERE contract_revision=2",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(installed_digest, next_digest);
        assert!(db
            .connection
            .execute("UPDATE verification_runs SET reason='rewritten'", [])
            .is_err());
        let again = db.rework_acceptance(&feedback, true).unwrap();
        assert_eq!(again.attempt_id, rework.attempt_id);
        assert_eq!(again.contract_revision, Some(2));
        assert_eq!(count(&db.connection, "SELECT count(*) FROM attempts"), 2);
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM task_contracts"),
            2
        );
        assert_eq!(verification_snapshot(&db.connection), before);
    }

    #[test]
    fn expired_feedback_lease_can_ack_and_a_live_other_owner_conflicts() {
        let (_temp, mut db) = fixture();
        seed_task(&mut db, "task");
        let expired_id = record_rejection(&mut db, "lease-expired", "task");
        db.claim_feedback_item(&expired_id, "other-owner", 1, 1)
            .unwrap();
        let decision = db.request_replan(&expired_id).unwrap();
        assert!(matches!(
            decision,
            ReplanDecision::Automatic {
                automatic_count: 1,
                ..
            }
        ));
        let claims: Vec<(i64, String)> = {
            let mut stmt = db
                .connection
                .prepare(
                    "SELECT claim_epoch, state FROM feedback_claims WHERE feedback_id=?1 ORDER BY claim_epoch",
                )
                .unwrap();
            stmt.query_map([&expired_id], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .map(|row| row.unwrap())
                .collect()
        };
        assert_eq!(claims, vec![(1, "expired".into()), (2, "acked".into())]);
        let acked: String = db
            .connection
            .query_row(
                "SELECT state FROM feedback_items WHERE feedback_id=?1",
                [&expired_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(acked, "acked");

        let live_id = record_rejection(&mut db, "lease-live", "task");
        let now = jiff::Timestamp::now().as_millisecond();
        db.claim_feedback_item(&live_id, "other-owner", now, 60_000)
            .unwrap();
        assert!(matches!(
            db.request_replan(&live_id),
            Err(StoreError::Conflict)
        ));
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM replan_requests"),
            1
        );
        let live: (String, i64, String) = db
            .connection
            .query_row(
                "SELECT i.state, c.claim_epoch, c.state FROM feedback_items i JOIN feedback_claims c ON c.feedback_id=i.feedback_id WHERE i.feedback_id=?1",
                [&live_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(live, ("claimed".into(), 1, "active".into()));
    }
}

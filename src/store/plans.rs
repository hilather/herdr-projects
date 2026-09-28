//! Schema 31 untrusted plan revisions. A proposal file is not a signature and
//! is not installed as a task contract. Accepting one does not reserve an attempt.
//! Waits commit their subscription cursor atomically and replay until a relevant wake.
//! A schema 29 feedback row is the only replan trigger: two automatic replans for
//! one blocker inside a plan revision, then one inbox escalation. A pull-request
//! poll is not a trigger. Infrastructure retries stay on the same attempt.
use super::*;
use crate::domain::{Dependency, QueueRecord, Task, TaskId, TaskState, WaitTrigger};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
mod waits;
mod wait_triggers;
mod wait_recovery;
mod replan_service;
mod sessions;
mod graph;
mod inspection;
mod changes;
pub use inspection::{PlanIntent,PlanIntentPage,inspect_project_plan};
pub use sessions::{PlannerSession,PlannerInput,PlannerEventInput,create_project_planner_session,show_project_planner_session};
pub use replan_service::{AutoReplanControl,ReplanTurn,set_project_auto_replans,service_project_replans};
pub use waits::{WaitTurn,register_project_wait,register_project_wait_with_deadline,register_project_wait_with_trigger,rearm_project_wait,replay_project_wait,request_project_replan,service_project_waits};

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

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProposalFile {
    version: u32,
    contracts: Vec<ProposedContract>,
    #[serde(default)]
    planner: Option<sessions::PlannerBinding>,
    #[serde(default)]
    envelope: Option<changes::Envelope>,
    #[serde(default)]
    changes: Vec<changes::Change>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProposedContract {
    task_id: TaskId,
    text: String,
    dependencies: Vec<Dependency>,
    #[serde(default, skip_serializing_if="Option::is_none")]
    cancellation_reason: Option<String>,
}

struct ParsedProposal {
    contracts: Vec<ProposedContract>,
    contract_texts: String,
    planner: Option<sessions::PlannerBinding>,
    envelope: Option<changes::Envelope>,
    changes: Vec<changes::Change>,
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
    if !matches!((document.version,document.planner.is_some()),(1,false)|(2,true)|(3,true)) || document.contracts.is_empty() || document.contracts.len() > 1024 {
        return Err(invalid("invalid plan proposal"));
    }
    if document.version == 3 {
        changes::validate_document(&document)?;
    } else if document.envelope.is_some() || !document.changes.is_empty()
        || document.contracts.iter().any(|c| c.cancellation_reason.is_some()) {
        return Err(invalid("typed changes require proposal version 3"));
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
        envelope: document.envelope,
        changes: document.changes,
        planner: document.planner,
        contracts: document.contracts,
        contract_texts,
    })
}

/// Cycle check overlays accepted planning intent and this proposal on the live
/// queue. Unmentioned intent stays; execution state is never written here.
fn reject_cycles(db: &Connection, proposal: &ParsedProposal,budget:Option<&read_budget::ReadBudget>) -> Result<()> {graph::validate(db,proposal,budget)}

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
    budget:Option<&read_budget::ReadBudget>,
) -> Result<Option<ExistingProposal>> {
    read_budget::optional(db,
        "SELECT proposal_id, payload_digest, payload, plan_revision FROM plan_proposals WHERE project_store=?1 AND idempotency_key=?2",
        params![project_store, key],budget,&[],
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
        self.apply_plan_proposal_with_budget(raw,expected_parent,idempotency_key,None)
    }
    pub(crate) fn apply_plan_proposal_with_budget(&mut self,raw:&[u8],expected_parent:u64,idempotency_key:&str,budget:Option<&read_budget::ReadBudget>)->Result<PlanProposalReceipt> {
        if let Some(budget)=budget {budget.check()?;budget.bytes(raw.len())?;}
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
        if let Some(existing) = lookup_proposal(&tx, &project_store, idempotency_key,budget)? {
            if sha256_hex(&existing.payload) != existing.digest {
                return Err(StoreError::Corrupt("plan proposal digest mismatch".into()));
            }
            if existing.digest != digest {
                return Err(StoreError::Conflict);
            }
            let proposal=parse_proposal(raw)?;
            changes::validate_replay(&tx,&proposal,&project_store,idempotency_key,existing.plan_revision-1,&existing.proposal_id,budget)?;
            if let Some(binding)=proposal.planner {
                sessions::validate_binding(&tx,&binding,existing.plan_revision-1,&project_store,budget)?;
                let linked:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM planner_proposal_inputs WHERE proposal_id=?1 AND session_id=?2 AND input_digest=?3)",
                    params![existing.proposal_id,binding.session_id,binding.input_digest],|r|r.get(0))?;
                if !linked {return Err(StoreError::Corrupt("planner proposal input link missing".into()));}
            }
            let schema:u32=tx.query_row("PRAGMA user_version",[],|row|row.get(0))?;
            if schema>=43 {
                let requested:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM replan_requests WHERE replan_id=?1 AND outcome='automatic')",[idempotency_key],|row|row.get(0))?;
                if requested {
                    let response:Option<String>=tx.query_row("SELECT proposal_id FROM replan_responses WHERE replan_id=?1",[idempotency_key],|row|row.get(0)).optional()?;
                    if response.as_deref()!=Some(existing.proposal_id.as_str()) {return Err(StoreError::Conflict);}
                }
            }
            let receipt = PlanProposalReceipt {
                plan_revision: existing.plan_revision,
                proposal_id: existing.proposal_id,
                digest: existing.digest,
                replayed: true,
            };
            if let Some(budget)=budget {budget.check()?;}
            tx.commit()?;
            return Ok(receipt);
        }
        let proposal = parse_proposal(raw)?;
        let current = current_revision(&tx)?;
        if current != expected_parent {
            return Err(StoreError::StalePlanParent(current));
        }
        if let Some(binding)=&proposal.planner {sessions::validate_binding(&tx,binding,current,&project_store,budget)?;}
        let schema:u32=tx.query_row("PRAGMA user_version",[],|row|row.get(0))?;
        let replan_parent:Option<u64>=if schema>=43 {
            tx.query_row("SELECT plan_revision FROM replan_requests WHERE replan_id=?1 AND outcome='automatic'",[idempotency_key],|row|row.get(0)).optional()?
        }else{None};
        if replan_parent.is_some_and(|parent|parent!=current) {return Err(StoreError::StalePlanParent(current));}
        changes::validate_identity(&tx,&proposal,&project_store,idempotency_key,current,budget)?;
        changes::validate_changes(&tx,&proposal,budget)?;
        reject_cycles(&tx,&proposal,budget)?;
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
        changes::record(&tx,&proposal,&proposal_id)?;
        if let Some(binding)=&proposal.planner {
            tx.execute("INSERT INTO planner_proposal_inputs(proposal_id,session_id,input_digest) VALUES(?1,?2,?3)",
                params![proposal_id,binding.session_id,binding.input_digest])?;
        }
        // A new revision starts its own replan budget. Older rows stay put.
        record_replan_reset(&tx, next, created_unix_ms)?;
        if replan_parent.is_some() {
            tx.execute("INSERT INTO replan_responses VALUES(?1,?2,?3)",params![idempotency_key,proposal_id,created_unix_ms])?;
            tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('replan.responded',?1,1,1,?2)",params![idempotency_key,serde_json::json!({"proposal_id":proposal_id,"plan_revision":next}).to_string()])?;
            let notice=sha256_hex(format!("replan-notice\0{idempotency_key}").as_bytes());
            tx.execute("UPDATE inbox_items SET revision=revision+1,seen=1,done=1 WHERE id=?1 AND done=0",[&notice])?;
            let revision:Option<u64>=tx.query_row("SELECT revision FROM inbox_items WHERE id=?1",[&notice],|row|row.get(0)).optional()?;
            if let Some(revision)=revision {
                tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('inbox.done',?1,?2,1,'{\"seen\":true,\"done\":true}')",params![notice,integer(revision)?])?;
            }
        }
        if let Some(budget)=budget {budget.check()?;}
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
    operation_id: String,
    category: String,
    task_id: String,
    reason: String,
}

fn load_feedback(tx: &Connection, feedback_id: &str, budget:Option<&read_budget::ReadBudget>) -> Result<Option<FeedbackRow>> {
    read_budget::optional(tx,
        "SELECT operation_id, category, task_id, reason FROM feedback_items WHERE feedback_id=?1",
        [feedback_id],budget,&[],
        |row| {
            Ok(FeedbackRow {
                operation_id: row.get(0)?,
                category: row.get(1)?,
                task_id: row.get(2)?,
                reason: row.get(3)?,
            })
        },
    )

}

fn automatic_count(tx: &Connection, plan_revision: u64, fingerprint: &str) -> Result<i64> {
    // Revisions, not wall-clock ordering, delimit the budget. Reset timestamps
    // are audit metadata: a clock correction must not hide requests already
    // charged to this revision.
    let count: i64 = tx.query_row(
        "SELECT count(*) FROM replan_requests WHERE plan_revision=?1 AND blocker_fingerprint=?2 AND outcome='automatic'",
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

/// The feedback acknowledgment transfers responsibility to this durable request;
/// it is not evidence that a planner has already produced an accepted proposal.
fn ensure_replan_notice(tx:&Connection,decision:&ReplanDecision,budget:Option<&read_budget::ReadBudget>)->Result<()> {
    let ReplanDecision::Automatic{replan_id,..}=decision else{return Ok(());};
    let (feedback_id,parent,created):(String,u64,i64)=tx.query_row(
        "SELECT feedback_id,plan_revision,created_unix_ms FROM replan_requests WHERE replan_id=?1 AND outcome='automatic'",
        [replan_id],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?;
    let feedback=load_feedback(tx,&feedback_id,budget)?.ok_or_else(||StoreError::Corrupt("replan feedback is missing".into()))?;
    let id=sha256_hex(format!("replan-notice\0{replan_id}").as_bytes());
    let content=crate::domain::InboxContent {
        id:id.clone(),kind:"replan-request".into(),subject:feedback.task_id.clone(),
        created:jiff::Timestamp::from_millisecond(created).map_err(|error|invalid(&error.to_string()))?.to_string(),
        summary:format!("plan revision requested for {}",feedback.task_id),
        body:serde_json::json!({"schema_version":1,"replan_id":replan_id,"feedback_id":feedback_id,"task_id":feedback.task_id,"operation_id":feedback.operation_id,"category":feedback.category,"reason":feedback.reason,"expected_plan_revision":parent,"idempotency_key":replan_id,"response":"Submit an unsigned plan proposal using the expected plan revision and idempotency key. This request grants no execution authority."}).to_string(),
    };
    content.validate().map_err(StoreError::Invalid)?;
    let payload=serde_json::to_string(&content).map_err(|error|invalid(&error.to_string()))?;
    let hash=sha256_hex(payload.as_bytes());
    let existing:Option<(String,String)>=read_budget::optional(tx,"SELECT payload,payload_hash FROM inbox_items WHERE id=?1",[&id],budget,&[],|row|Ok((row.get(0)?,row.get(1)?)))?;
    if let Some((old,old_hash))=existing {
        if old!=payload||old_hash!=hash {return Err(StoreError::Corrupt("replan notice identity mismatch".into()));}
        return Ok(());
    }
    tx.execute("INSERT INTO inbox_items VALUES(?1,1,?2,?3,0,0)",params![id,payload,hash])?;
    tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('replan.requested',?1,1,1,?2)",params![replan_id,serde_json::json!({"inbox_id":id,"feedback_id":feedback_id,"plan_revision":parent}).to_string()])?;
    Ok(())
}

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
        self.register_wait_with_deadline(task_id,attempt_id,condition,None)
    }
    pub fn register_wait_with_deadline(&mut self,task_id:&str,attempt_id:Option<&str>,condition:&str,deadline:Option<i64>)->Result<WaitRegistration> {
        self.register_wait_with_trigger(task_id,attempt_id,condition,deadline,None)
    }
    pub fn register_wait_with_trigger(&mut self,task_id:&str,attempt_id:Option<&str>,condition:&str,deadline:Option<i64>,trigger:Option<&WaitTrigger>)->Result<WaitRegistration> {
        self.register_wait_with_budget(task_id,attempt_id,condition,deadline,trigger,None)
    }
    pub(crate) fn register_wait_with_budget(&mut self,task_id:&str,attempt_id:Option<&str>,condition:&str,deadline:Option<i64>,trigger:Option<&WaitTrigger>,budget:Option<&read_budget::ReadBudget>)->Result<WaitRegistration> {
        if let Some(budget)=budget {budget.check()?;}
        if let Some(trigger)=trigger {trigger.validate(condition).map_err(StoreError::Invalid)?;}
        if deadline.is_some_and(|value|value<0||jiff::Timestamp::from_millisecond(value).is_err()) {return Err(invalid("invalid wait deadline"));}
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
        let schema:u32=tx.query_row("PRAGMA user_version",[],|row|row.get(0))?;
        if (deadline.is_some()||trigger.is_some())&&schema<43 {return Err(StoreError::UnsupportedSchema(schema));}
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
        let mut identity=format!("{task_id}\0{attempt_key}\0{condition}\0{plan_revision}");
        if let Some(deadline)=deadline {identity.push_str(&format!("\0deadline\0{deadline}"));}
        if let Some(trigger)=trigger {identity.push_str(&format!("\0trigger\0{}",serde_json::to_string(trigger).map_err(|_|invalid("wait trigger encoding failed"))?));}
        let wait_id=sha256_hex(identity.as_bytes());
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
                    "deadline_unix_ms": deadline,
                    "trigger": trigger,
                })
                .to_string()
            ],
        )?;
        let cursor_sequence =
            i64::try_from(head(&tx)?).map_err(|_| invalid("wait cursor exceeds range"))?;
        if let Some(deadline)=deadline {
            tx.execute("INSERT INTO wait_conditions(wait_id,task_id,attempt_id,condition,plan_revision,cursor_sequence,state,replayed_through,wake_requested,created_unix_ms,deadline_unix_ms) VALUES(?1,?2,?3,?4,?5,?6,'waiting',NULL,0,?7,?8)",params![wait_id,task_id,attempt_id,condition,integer(plan_revision)?,cursor_sequence,now,deadline])?;
        }else {tx.execute(
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
        )?;}
        if let Some(trigger)=trigger {wait_triggers::insert(&tx,&wait_id,trigger,budget)?;}
        // Evidence can precede subscription registration. Bind that observation
        // to a new addressed wake in the same transaction, avoiding a lost wake.
        if let Some((kind,entity,sequence))=current_wait_evidence(&tx,task_id,attempt_id,condition,trigger,budget)? {
            if relevant_wait_event(&tx,task_id,attempt_id,condition,trigger,&kind,&entity,sequence,budget)? {
                tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('wait.wake',?1,1,1,?2)",params![wait_id,serde_json::json!({"source_sequence":sequence,"proved":false}).to_string()])?;
            }
        }
        if let Some(budget)=budget {budget.check()?;}
        tx.commit()?;
        Ok(WaitRegistration {
            wait_id,
            cursor_sequence,
            already_registered: false,
        })
    }

    /// Replay a bounded page once, retaining the subscription until its own wake.
    pub fn replay_wait(&mut self, wait_id: &str) -> Result<WaitReplay> {
        self.replay_wait_with_budget(wait_id,None)
    }
    pub(crate) fn replay_wait_with_budget(&mut self, wait_id: &str,budget:Option<&read_budget::ReadBudget>) -> Result<WaitReplay> {
        if !identifier(wait_id) {
            return Err(invalid("wait is invalid"));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema36(&tx)?;
        let version: u32 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 42 { return Err(StoreError::UnsupportedSchema(version)); }
        let row: Option<(i64, String, Option<i64>, i64)> = tx
            .query_row(
                "SELECT cursor_sequence, state, replayed_through, wake_requested FROM wait_conditions WHERE wait_id=?1",
                [wait_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((cursor_sequence, _state, replayed_through, wake_requested)) = row else {
            return Err(invalid("wait is not registered"));
        };
        if wake_requested == 1 {
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
        let (task,attempt,condition):(String,Option<String>,String)=tx.query_row(
            "SELECT task_id,attempt_id,condition FROM wait_conditions WHERE wait_id=?1",[wait_id],
            |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?;
        let trigger=wait_triggers::load(&tx,wait_id,budget)?;
        let after = replayed_through.unwrap_or(cursor_sequence);
        let deadline:Option<i64>=if version>=43 {tx.query_row("SELECT deadline_unix_ms FROM wait_conditions WHERE wait_id=?1",[wait_id],|row|row.get(0))?}else{None};
        let expired=deadline.is_some_and(|deadline|jiff::Timestamp::now().as_millisecond()>=deadline);
        let events: Vec<(i64, String, String)> = if expired {
            // A timeout is an independent advisory trigger. Do not let an
            // unrelated event backlog delay it, and never treat it as evidence.
            tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('wait.wake',?1,1,1,?2)",params![wait_id,serde_json::json!({"reason":"deadline_expired","deadline_unix_ms":deadline,"proved":false}).to_string()])?;
            vec![(integer(head(&tx)?)?,"wait.wake".into(),wait_id.into())]
        }else {
            let mut stmt = tx.prepare(
                "SELECT sequence, kind, entity FROM events WHERE sequence > ?1 ORDER BY sequence LIMIT 1000",
            )?;
            let mut rows=stmt.query(params![after])?;
            let mut events=Vec::new();
            while let Some(row)=rows.next()? {
                if let Some(budget)=budget {budget.row(row,&[])?;}
                events.push((row.get(0)?,row.get(1)?,row.get(2)?));
            }
            events
        };
        let mut wake = false;
        let mut wake_sequence = None;
        for (sequence, kind, entity) in &events {
            let is_wake = (kind == "wait.wake" && entity == wait_id)
                || (!wake && relevant_wait_event(&tx,&task,attempt.as_deref(),&condition,trigger.as_ref(),kind,entity,*sequence,budget)?);
            wake |= is_wake;
            if is_wake && wake_sequence.is_none() { wake_sequence=Some(*sequence); }
            tx.execute(
                "INSERT INTO wait_replay_events(wait_id,event_sequence,kind,wake) VALUES(?1,?2,?3,?4)",
                params![wait_id, sequence, kind, i64::from(is_wake)],
            )?;
        }
        let replayed_through = events.last().map_or(after, |event| event.0);
        if let Some(sequence)=wake_sequence { insert_wait_notice(&tx,wait_id,&task,&condition,sequence,replayed_through,expired)?; }
        let updated = tx.execute(
            "UPDATE wait_conditions SET state='replayed', replayed_through=?2, wake_requested=?3 WHERE wait_id=?1 AND wake_requested=0",
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

    /// Two automatic replans for this blocker and plan revision, then one inbox item.
    /// The third does not insert a plan proposal.
    pub fn request_replan(&mut self, feedback_id: &str) -> Result<ReplanDecision> {
        self.request_replan_selected(feedback_id,None,false)
    }
    fn request_replan_selected(&mut self, feedback_id:&str,budget:Option<&read_budget::ReadBudget>,automatic:bool)->Result<ReplanDecision> {
        if let Some(budget)=budget {budget.check()?;}
        if !identifier(feedback_id) {
            return Err(invalid("replan requires a schema 29 feedback row"));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema36(&tx)?;
        if automatic && !replan_service::enabled(&tx)? {return Err(StoreError::Conflict);}
        if let Some(decision) = existing_replan(&tx, feedback_id)? {
            ensure_replan_notice(&tx,&decision,budget)?;
            tx.commit()?;
            return Ok(decision);
        }
        let Some(feedback) = load_feedback(&tx, feedback_id,budget)? else {
            return Err(invalid("replan requires a schema 29 feedback row"));
        };
        if !matches!(
            feedback.category.as_str(),
            "verifier_rejection" | "integrator_rejection" | "integrator_conflict" | "invalidation"
        ) {
            return Err(invalid("pull-request poll is not a replan trigger"));
        }
        let now = jiff::Timestamp::now().as_millisecond();
        if !replan_service::available(&tx,feedback_id,now)? {return Err(StoreError::Conflict);}
        let plan_revision = current_revision(&tx)?;
        let fingerprint =
            blocker_fingerprint(&feedback.task_id, &feedback.category, &feedback.reason);
        let used = automatic_count(&tx, plan_revision, &fingerprint)?;
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
            let decision=ReplanDecision::Automatic {
                replan_id,
                proposal_id,
                automatic_count: used + 1,
            };
            ensure_replan_notice(&tx,&decision,budget)?;
            replan_service::link(&tx,feedback_id,&decision)?;
            if let Some(budget)=budget {budget.check()?;}
            tx.commit()?;
            return Ok(decision);
        }
        if let Some(decision) = existing_escalation(&tx, plan_revision, &fingerprint)? {
            replan_service::link(&tx,feedback_id,&decision)?;
            if let Some(budget)=budget {budget.check()?;}
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
        let decision=ReplanDecision::Escalated {replan_id,inbox_id};
        replan_service::link(&tx,feedback_id,&decision)?;
        if let Some(budget)=budget {budget.check()?;}
        tx.commit()?;
        Ok(decision)
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
        let Some(feedback) = load_feedback(&tx, feedback_id,None)? else {
            return Err(invalid("acceptance rework requires verifier feedback"));
        };
        if feedback.category != "verifier_rejection" {
            return Err(invalid("acceptance rework requires verifier feedback"));
        }
        let attempt_id = format!("rework-{}", &feedback_id[..32]);
        let existing: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM attempts WHERE id=?1 AND task_id=?2)",
            params![attempt_id, feedback.task_id],
            |row| row.get(0),
        )?;
        if existing {
            // Replay returns the revision stored with this attempt, not a later install.
            let contract_revision = stored_rework_contract(&tx, &attempt_id)?;
            let (attempts_consumed, _) = attempt_budget(&tx, &feedback.task_id)?;
            tx.commit()?;
            return Ok(AcceptanceRework {
                attempt_id,
                attempts_consumed,
                contract_revision,
            });
        }
        let contract_revision = if contract_changed {
            Some(installed_rework_revision(
                &tx,
                &feedback.task_id,
                &feedback.operation_id,
            )?)
        } else {
            None
        };
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

fn current_wait_evidence(db:&Connection,task:&str,attempt:Option<&str>,condition:&str,trigger:Option<&WaitTrigger>,budget:Option<&read_budget::ReadBudget>)->Result<Option<(String,String,i64)>> {
    if let Some(trigger)=trigger {return wait_triggers::current(db,trigger,budget);}
    let query=match condition {
        "validation_completion" => "SELECT e.kind,e.entity,e.sequence FROM verification_runs v JOIN events e ON e.entity=v.run_id AND e.kind IN ('verification.accepted','verification.rejected') WHERE v.task_id=?1 AND (?2 IS NULL OR v.attempt_id=?2) AND v.contract_revision=(SELECT MAX(contract_revision) FROM task_contracts WHERE task_id=?1) ORDER BY e.sequence DESC LIMIT 1",
        "dependency_evidence" => "SELECT e.kind,e.entity,e.sequence FROM dependency_satisfactions d LEFT JOIN verified_results v ON d.evidence_kind='verified_result' AND v.result_id=d.evidence_id JOIN events e ON (d.evidence_kind='verified_result' AND e.kind='verification.accepted' AND e.entity=v.run_id) OR (d.evidence_kind='integrated_commit' AND e.kind='integration.wake' AND e.entity=d.evidence_id) WHERE d.task_id=?1 AND d.state='valid' AND (?2 IS NULL OR EXISTS(SELECT 1 FROM attempts a WHERE a.id=?2 AND a.task_id=?1)) ORDER BY e.sequence DESC LIMIT 1",
        _=>return Ok(None),
    };
    read_budget::optional(db,query,params![task,attempt],budget,&[],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?)))
}

/// Published receipts request reevaluation; they never bypass dependency,
/// contract, policy, attempt-generation or capacity validation at reservation.
fn relevant_wait_event(db:&Connection,task:&str,attempt:Option<&str>,condition:&str,trigger:Option<&WaitTrigger>,kind:&str,entity:&str,sequence:i64,budget:Option<&read_budget::ReadBudget>)->Result<bool> {
    if let Some(trigger)=trigger {return wait_triggers::relevant(db,task,condition,trigger,kind,entity,sequence,budget);}
    if condition=="dependency_evidence" && matches!(kind,"contract.installed"|"scheduler.task_queued") {
        return Ok(entity==task && dependency_wait_ready(db,task,budget)?);
    }
    let query=match (condition,kind) {
        ("validation_completion","verification.accepted"|"verification.rejected") =>
            "SELECT EXISTS(SELECT 1 FROM verification_runs v WHERE v.run_id=?1 AND v.task_id=?2 AND (?3 IS NULL OR v.attempt_id=?3) AND v.contract_revision=(SELECT MAX(contract_revision) FROM task_contracts WHERE task_id=?2))",
        ("dependency_evidence","verification.accepted") =>
            "SELECT EXISTS(SELECT 1 FROM dependency_satisfactions d JOIN verified_results v ON v.result_id=d.evidence_id WHERE d.task_id=?2 AND d.state='valid' AND d.evidence_kind='verified_result' AND v.run_id=?1 AND (?3 IS NULL OR EXISTS(SELECT 1 FROM attempts a WHERE a.id=?3 AND a.task_id=?2)))",
        ("dependency_evidence","integration.wake") =>
            "SELECT EXISTS(SELECT 1 FROM dependency_satisfactions d JOIN integrated_commits i ON i.integrated_id=d.evidence_id WHERE d.task_id=?2 AND d.state='valid' AND d.evidence_kind='integrated_commit' AND i.integrated_id=?1 AND (?3 IS NULL OR EXISTS(SELECT 1 FROM attempts a WHERE a.id=?3 AND a.task_id=?2)))",
        _=>return Ok(false),
    };
    let relevant:bool=db.query_row(query,params![entity,task,attempt],|row|row.get(0))?;
    if relevant && condition=="dependency_evidence" {
        return Ok(dependency_wait_ready(db,task,budget)?);
    }
    Ok(relevant)
}

fn dependency_wait_ready(db:&Connection,task:&str,budget:Option<&read_budget::ReadBudget>)->Result<bool> {
    let bound:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM task_contracts WHERE task_id=?1)",[task],|row|row.get(0))?;
    if !bound {return Ok(false);}
    Ok(super::satisfaction::satisfied_edges_on(db,task,budget)?.is_some_and(|edges|!edges.is_empty()))
}

fn insert_wait_notice(tx:&Connection,wait:&str,task:&str,condition:&str,sequence:i64,through:i64,expired:bool)->Result<()> {
    let trigger=if expired {"its deadline expired"}else{"a relevant event arrived"};
    let id=sha256_hex(format!("wait-notice\0{wait}").as_bytes());
    let content=InboxContent {id:id.clone(),kind:"wait-wake".into(),subject:task.into(),
        created:jiff::Timestamp::now().to_string(),summary:format!("reevaluate {condition} for {task}"),
        body:format!("Wait {wait} requests reevaluation because {trigger} (event {sequence}). Revalidate current evidence and authority; this wake is not proof or a launch grant.")};
    content.validate().map_err(StoreError::Invalid)?;
    let payload=serde_json::to_string(&content).map_err(|e|invalid(&e.to_string()))?;
    tx.execute("INSERT INTO inbox_items VALUES(?1,1,?2,?3,0,0)",params![id,payload,sha256_hex(payload.as_bytes())])?;
    tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('wait.notified',?1,1,1,?2)",params![wait,serde_json::json!({"inbox_id":id,"trigger_sequence":sequence,"trigger_through":through,"proved":false}).to_string()])?;
    Ok(())
}

fn existing_replan(tx: &Connection, feedback_id: &str) -> Result<Option<ReplanDecision>> {
    let version:u32=tx.query_row("PRAGMA user_version",[],|row|row.get(0))?;
    let query=if version>=43 {
        "SELECT r.replan_id,r.outcome,r.proposal_id,r.inbox_id,r.plan_revision FROM replan_feedback_links l JOIN replan_requests r ON r.replan_id=l.replan_id WHERE l.feedback_id=?1"
    }else {"SELECT replan_id,outcome,proposal_id,inbox_id,plan_revision FROM replan_requests WHERE feedback_id=?1"};
    let row: Option<(String, String, Option<String>, Option<String>, i64)> = tx
        .query_row(
            query,
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

/// Floor is this feedback's verification run, not the newest run on the task.
fn installed_rework_revision(tx: &Connection, task_id: &str, operation_id: &str) -> Result<i64> {
    let floor: Option<i64> = tx
        .query_row(
            "SELECT contract_revision FROM verification_runs WHERE run_id=?1 AND task_id=?2",
            params![operation_id, task_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(floor) = floor else {
        return Err(invalid("acceptance rework has no contract revision"));
    };
    let revision: Option<i64> = tx.query_row(
        "SELECT max(c.contract_revision) FROM task_contracts c WHERE c.task_id=?1 AND c.contract_revision>?2 AND EXISTS (SELECT 1 FROM events e WHERE e.kind='contract.installed' AND e.entity=c.task_id AND e.revision=c.contract_revision)",
        params![task_id, floor],
        |row| row.get(0),
    )?;
    revision.ok_or_else(|| invalid("acceptance rework has no contract revision"))
}

fn stored_rework_contract(tx: &Connection, attempt_id: &str) -> Result<Option<i64>> {
    let payload: Option<String> = tx
        .query_row(
            "SELECT payload FROM events WHERE kind='attempt.rework' AND entity=?1 ORDER BY sequence LIMIT 1",
            [attempt_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(payload) = payload else {
        return Err(StoreError::Corrupt("rework attempt has no event".into()));
    };
    let value: serde_json::Value = serde_json::from_str(&payload)
        .map_err(|_| StoreError::Corrupt("rework event is invalid".into()))?;
    match value.get("contract_revision") {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Number(number)) => number
            .as_i64()
            .map(Some)
            .ok_or_else(|| StoreError::Corrupt("rework contract revision is invalid".into())),
        Some(_) => Err(StoreError::Corrupt(
            "rework contract revision is invalid".into(),
        )),
    }
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
    let control=controlled::ReadControl::new(std::time::Instant::now()+Duration::from_secs(2),Default::default());
    let mut db=crate::migration::open_active_scoped(project,control).map_err(|error|invalid(&error.to_string()))?;
    db.apply_plan_proposal(&bytes, expected_parent, idempotency_key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Attempt, AttemptId, AttemptState, Mutation, TaskId, TaskState};
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
    fn upgrade_v1_from_30_to_31_and_import_requires_current_schema() {
        let fresh = tempfile::tempdir().unwrap();
        let mut created = SqliteStore::create(&fresh.path().join("state.db")).unwrap();
        assert_eq!(user_version(&created.connection), crate::store::SCHEMA);
        assert_eq!(
            created
                .connection
                .query_row("SELECT schema_version FROM store_meta", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            crate::store::SCHEMA
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
        crate::store::test_schema::historical(&raw, 30)
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
        assert_eq!(user_version(&db.connection), crate::store::SCHEMA);
        assert_eq!(
            db.connection
                .query_row("SELECT schema_version FROM store_meta", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            crate::store::SCHEMA
        );
        assert!(table_exists(&db.connection, "plan_proposals"));
        assert!(table_exists(&db.connection, "plan_revisions"));
        check_schema(&db.connection).unwrap();
        drop(db);
        let reopened = SqliteStore::open(&path).unwrap();
        assert_eq!(user_version(&reopened.connection), crate::store::SCHEMA);
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
            crate::store::SCHEMA
        );
        assert!(table_exists(
            &rusqlite::Connection::open(&created_path).unwrap(),
            "wait_conditions"
        ));
        assert!(table_exists(
            &rusqlite::Connection::open(&created_path).unwrap(),
            "replan_requests"
        ));

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.db");
        let mut db = SqliteStore::create(&path).unwrap();
        seed_task(&mut db, "kept");
        drop(db);
        let raw = rusqlite::Connection::open(&path).unwrap();
        crate::store::test_schema::historical(&raw, 35)
        .unwrap();
        drop(raw);
        let mut db = SqliteStore::open(&path).unwrap();
        assert_eq!(user_version(&db.connection), 35);
        assert!(!table_exists(&db.connection, "wait_conditions"));
        assert!(!table_exists(&db.connection, "replan_requests"));
        assert_eq!(db.read_snapshot(None).unwrap().tasks[0].title, "kept");
        db.upgrade_v1().unwrap();
        assert_eq!(user_version(&db.connection), crate::store::SCHEMA);
        assert_eq!(
            db.connection
                .query_row("SELECT schema_version FROM store_meta", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            crate::store::SCHEMA
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
        assert_eq!(user_version(&reopened.connection), crate::store::SCHEMA);
    }

    #[test]
    fn automatic_replans_respect_leases_and_retry_atomic_notice_failures() {
        let (_root,mut db)=fixture();seed_task(&mut db,"task");
        let held=record_rejection(&mut db,"held-feedback","task");
        let ready=record_rejection(&mut db,"ready-feedback","task");
        let now=jiff::Timestamp::now().as_millisecond();db.claim_feedback_item(&held,"another-consumer",now,60_000).unwrap();
        db.set_auto_replans(db.current_head().unwrap(),true).unwrap();db.connection.execute("UPDATE project_control SET state='active'",[]).unwrap();
        let budget=||read_budget::ReadBudget::new(controlled::ReadControl::new(std::time::Instant::now()+Duration::from_secs(5),Default::default()));
        let before=db.current_head().unwrap();
        db.connection.execute_batch("CREATE TEMP TRIGGER refuse_replan_notice BEFORE INSERT ON inbox_items BEGIN SELECT RAISE(ABORT,'fault'); END;").unwrap();
        assert!(db.service_replans(&budget()).is_err());assert_eq!(db.current_head().unwrap(),before);
        assert_eq!(count(&db.connection,"SELECT count(*) FROM replan_requests"),0);
        assert_eq!(count(&db.connection,"SELECT count(*) FROM replan_pending_feedback"),2);
        db.connection.execute_batch("DROP TRIGGER refuse_replan_notice;").unwrap();
        for _ in 0..3 {db.service_replans(&budget()).unwrap();}
        assert_eq!(count(&db.connection,"SELECT count(*) FROM replan_feedback_links"),1);
        assert!(existing_replan(&db.connection,&held).unwrap().is_none());assert!(existing_replan(&db.connection,&ready).unwrap().is_some());
        assert!(db.service_replans(&budget()).unwrap().pending);
    }

    #[test]
    fn automatic_replan_idle_selection_does_not_scan_linked_history() {
        use std::sync::{Arc,atomic::{AtomicUsize,Ordering}};
        let (_root,mut db)=fixture();seed_task(&mut db,"task");let first=record_rejection(&mut db,"baseline","task");
        let decision=db.request_replan(&first).unwrap();let ReplanDecision::Automatic{replan_id,..}=decision else {panic!()};
        db.set_auto_replans(db.current_head().unwrap(),true).unwrap();db.connection.execute("UPDATE project_control SET state='active'",[]).unwrap();
        let mut work=Vec::new();
        for history in [false,true] {
            if history {
                db.connection.execute_batch("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<10000) INSERT INTO feedback_items SELECT printf('%064d',x),printf('history-%d',x),1,'verifier_rejection','task','checks_failed','open',NULL,0 FROM n;").unwrap();
                db.connection.execute("INSERT INTO replan_feedback_links SELECT feedback_id,?1 FROM feedback_items WHERE operation_id LIKE 'history-%'",[&replan_id]).unwrap();
            }
            let steps=Arc::new(AtomicUsize::new(0));let counter=steps.clone();db.connection.progress_handler(1,Some(move||{counter.fetch_add(1,Ordering::Relaxed);false}));
            let budget=read_budget::ReadBudget::new(controlled::ReadControl::new(std::time::Instant::now()+Duration::from_secs(5),Default::default()));
            assert!(!db.service_replans(&budget).unwrap().pending);
            db.connection.progress_handler(0,None::<fn()->bool>);work.push(steps.load(Ordering::Relaxed));
        }
        eprintln!("automatic replan idle SQL steps at 0/10000 linked feedback rows: {work:?}");
        assert!(work[1]<=work[0]+100,"linked history increased idle work: {work:?}");
    }

    #[test]
    fn capacity_wait_requires_matching_termination_generation_and_receipt() {
        let (_temp,mut db)=fixture();seed_task(&mut db,"parent");seed_task(&mut db,"child");
        db.connection.execute("INSERT INTO attempts VALUES('child-attempt','child',1,'running',NULL,'slot',0)",[]).unwrap();
        let trigger=WaitTrigger::AttemptCapacityReleased{attempt_id:crate::domain::AttemptId::new("child-attempt").unwrap(),after_revision:1};
        let wait=db.register_wait_with_trigger("parent",None,"resource_availability",None,Some(&trigger)).unwrap();
        db.connection.execute_batch("UPDATE attempts SET state='completed',revision=2 WHERE id='child-attempt'; INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('attempt.changed','child-attempt',2,1,'{}');").unwrap();
        assert!(!db.replay_wait(&wait.wait_id).unwrap().wake_requested);
        assert_eq!(count(&db.connection,"SELECT count(*) FROM attempts WHERE termination_observed=0"),1);
        db.connection.execute_batch("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('runtime.worker_terminated','child-attempt',3,1,'{\"attempt\":\"child-attempt\"}');").unwrap();
        assert!(!db.replay_wait(&wait.wait_id).unwrap().wake_requested);
        // A terminal row does not make a receipt for another generation valid.
        db.connection.execute_batch("UPDATE attempts SET termination_observed=1,revision=4 WHERE id='child-attempt'; INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('runtime.worker_terminated','child-attempt',4,1,'{\"attempt\":\"other-attempt\"}');").unwrap();
        assert!(!db.replay_wait(&wait.wait_id).unwrap().wake_requested);
        db.connection.execute_batch("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('runtime.worker_terminated','child-attempt',2,1,'{\"attempt\":\"child-attempt\"}'); UPDATE attempts SET revision=5 WHERE id='child-attempt'; INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('runtime.worker_terminated','child-attempt',5,1,'{\"attempt\":\"child-attempt\"}');").unwrap();
        let termination_sequence=db.current_head().unwrap();
        let path=PathBuf::from(db.connection.path().unwrap());drop(db);
        let mut db=SqliteStore::open(&path).unwrap();
        let result=db.replay_wait(&wait.wait_id).unwrap();
        assert!(result.wake_requested);assert!(!result.proved);
        let notified_sequence:u64=db.connection.query_row("SELECT json_extract(payload,'$.trigger_sequence') FROM events WHERE kind='wait.notified' AND entity=?1",[&wait.wait_id],|row|row.get(0)).unwrap();
        assert_eq!(notified_sequence,termination_sequence);
        // A late older-generation event must not hide the retained current
        // termination when a successor registers after both events.
        db.connection.execute_batch("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('runtime.worker_terminated','child-attempt',2,1,'{\"attempt\":\"child-attempt\"}');").unwrap();
        let next=db.rearm_wait(&wait.wait_id,None).unwrap();
        assert_eq!(wait_triggers::load(&db.connection,&next.wait_id,None).unwrap(),Some(trigger.clone()));
        assert!(db.replay_wait(&next.wait_id).unwrap().wake_requested);
        db.connection.execute_batch("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('runtime.worker_terminated','child-attempt',5,1,'{\"attempt\":\"child-attempt\"}');").unwrap();
        let head=db.current_head().unwrap();
        assert!(db.register_wait_with_trigger("parent",None,"resource_availability",Some(1),Some(&trigger)).is_err());
        assert_eq!(db.current_head().unwrap(),head);
    }

    #[test]
    fn approval_decision_wait_filters_identity_scope_and_survives_restart() {
        let (_temp,mut db)=fixture();seed_task(&mut db,"task");seed_task(&mut db,"other");
        let grant=wait_approval(&db,"task");
        let id=grant.reference().unwrap().id;
        let trigger=WaitTrigger::ApprovalDecision{approval_id:id.clone(),task_revision:2};
        let wait=db.register_wait_with_trigger("task",None,"user_decision",None,Some(&trigger)).unwrap();
        let other=db.register_wait_with_trigger("other",None,"user_decision",None,Some(&trigger)).unwrap();
        let wrong_revision=db.register_wait_with_trigger("task",None,"user_decision",None,Some(&WaitTrigger::ApprovalDecision{approval_id:id.clone(),task_revision:3})).unwrap();
        let untyped=db.register_wait("task",None,"user_decision").unwrap();
        // An event without a retained authenticated decision is insufficient.
        db.connection.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('approval.installed',?1,1,1,'{}')",[&id]).unwrap();
        assert!(!db.replay_wait(&wait.wait_id).unwrap().wake_requested);
        let unrelated=wait_approval(&db,"other");
        db.install_approval(&crate::domain::PreparedApproval{grant:unrelated},db.current_head().unwrap(),jiff::Timestamp::now().as_millisecond()).unwrap();
        assert!(!db.replay_wait(&wait.wait_id).unwrap().wake_requested);
        db.install_approval(&crate::domain::PreparedApproval{grant},db.current_head().unwrap(),jiff::Timestamp::now().as_millisecond()).unwrap();
        let decision_sequence=db.current_head().unwrap();
        db.connection.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('unrelated','other',1,1,'{}')",[]).unwrap();
        let path=PathBuf::from(db.connection.path().unwrap());drop(db);
        let mut db=SqliteStore::open(&path).unwrap();
        assert!(db.replay_wait(&wait.wait_id).unwrap().wake_requested);
        let notice:(u64,u64)=db.connection.query_row("SELECT json_extract(payload,'$.trigger_sequence'),json_extract(payload,'$.trigger_through') FROM events WHERE kind='wait.notified' AND entity=?1",[&wait.wait_id],|row|Ok((row.get(0)?,row.get(1)?))).unwrap();
        assert_eq!(notice.0,decision_sequence);assert!(notice.1>notice.0);
        for wait in [&other,&wrong_revision,&untyped] {assert!(!db.replay_wait(&wait.wait_id).unwrap().wake_requested);}
        assert_eq!(count(&db.connection,"SELECT count(*) FROM inbox_items"),1);
        assert_eq!(count(&db.connection,"SELECT count(*) FROM approval_uses"),0);
        assert_eq!(count(&db.connection,"SELECT count(*) FROM attempts"),0);
    }

    fn wait_approval(db:&SqliteStore,task:&str)->crate::domain::ApprovalGrant {
        use crate::domain::*;
        let now=jiff::Timestamp::now().as_millisecond();
        ApprovalGrant{version:1,scope:ApprovalScope{version:1,class:ApprovalClass::RuntimeLaunch,
            project_store:std::fs::canonicalize(db.connection.path().unwrap()).unwrap().display().to_string(),
            task:TaskId::new(task).unwrap(),task_revision:2,target:"binding".into(),action_digest:"a".repeat(64)},
            policy:VersionedReference{id:"owner-policy".into(),revision:1,digest:"b".repeat(64)},issued_unix_ms:now-1000,expires_unix_ms:now+60_000}
    }

    #[test]
    fn approval_decision_wait_retains_trigger_on_rearm_and_rechecks_history() {
        let (_temp,mut db)=fixture();seed_task(&mut db,"task");
        let grant=wait_approval(&db,"task");let id=grant.reference().unwrap().id;
        let trigger=WaitTrigger::ApprovalDecision{approval_id:id.clone(),task_revision:2};
        db.install_approval(&crate::domain::PreparedApproval{grant},db.current_head().unwrap(),jiff::Timestamp::now().as_millisecond()).unwrap();
        let wait=db.register_wait_with_trigger("task",None,"user_decision",None,Some(&trigger)).unwrap();
        assert!(db.replay_wait(&wait.wait_id).unwrap().wake_requested);
        db.revoke_approval(&id,db.current_head().unwrap(),jiff::Timestamp::now().as_millisecond(),"withdrawn").unwrap();
        let next=db.rearm_wait(&wait.wait_id,None).unwrap();
        assert_eq!(wait_triggers::load(&db.connection,&next.wait_id,None).unwrap(),Some(trigger));
        assert!(db.replay_wait(&next.wait_id).unwrap().wake_requested);
        assert_eq!(count(&db.connection,"SELECT count(*) FROM approval_uses"),0);
        assert!(db.connection.execute("DELETE FROM wait_triggers",[]).is_err());
        assert!(db.connection.execute("UPDATE wait_triggers SET reference_revision=3",[]).is_err());
        // A later task incarnation must not reuse an old decision as a trigger.
        db.connection.execute("UPDATE tasks SET revision=3 WHERE id='task'",[]).unwrap();
        let later=db.rearm_wait(&next.wait_id,None).unwrap();
        assert!(!db.replay_wait(&later.wait_id).unwrap().wake_requested);
    }

    #[test]
    fn approval_decision_wait_registration_is_atomic_and_validates_trigger() {
        let (_temp,mut db)=fixture();seed_task(&mut db,"task");
        let trigger=WaitTrigger::ApprovalDecision{approval_id:format!("approval-{}","a".repeat(64)),task_revision:2};
        let head=db.current_head().unwrap();
        assert!(db.register_wait_with_trigger("task",None,"dependency_evidence",None,Some(&trigger)).is_err());
        let invalid=WaitTrigger::ApprovalDecision{approval_id:"approval-invalid".into(),task_revision:0};
        assert!(db.register_wait_with_trigger("task",None,"user_decision",None,Some(&invalid)).is_err());
        db.connection.execute_batch("CREATE TEMP TRIGGER refuse_wait_trigger BEFORE INSERT ON wait_triggers BEGIN SELECT RAISE(ABORT,'fault'); END;").unwrap();
        assert!(db.register_wait_with_trigger("task",None,"user_decision",None,Some(&trigger)).is_err());
        assert_eq!(db.current_head().unwrap(),head);
        assert_eq!(count(&db.connection,"SELECT count(*) FROM wait_conditions"),0);
        db.connection.execute_batch("DROP TRIGGER refuse_wait_trigger;").unwrap();
        let wait=db.register_wait_with_trigger("task",None,"user_decision",None,Some(&trigger)).unwrap();
        assert_eq!(db.register_wait_with_trigger("task",None,"user_decision",None,Some(&trigger)).unwrap(),WaitRegistration{already_registered:true,..wait});
    }

    #[test]
    fn wait_rearm_cancelled_budget_writes_nothing() {
        let (_temp,mut db)=fixture();seed_task(&mut db,"task");
        let first=db.register_wait_with_deadline("task",None,"user_decision",Some(1)).unwrap();
        db.replay_wait(&first.wait_id).unwrap();
        let head=db.current_head().unwrap();
        let budget=read_budget::ReadBudget::new(controlled::ReadControl::new(std::time::Instant::now()-Duration::from_secs(1),Default::default()));
        assert!(db.rearm_wait_with_budget(&first.wait_id,None,Some(&budget)).is_err());
        assert_eq!(db.current_head().unwrap(),head);
        assert_eq!(count(&db.connection,"SELECT count(*) FROM wait_rearms"),0);
    }

    #[test]
    fn wait_rearm_is_atomic_and_rejects_superseded_plans() {
        let (_temp,mut db)=fixture();seed_task(&mut db,"task");
        let first=db.register_wait_with_deadline("task",None,"user_decision",Some(1)).unwrap();
        db.replay_wait(&first.wait_id).unwrap();
        let head=db.current_head().unwrap();
        db.connection.execute_batch("CREATE TEMP TRIGGER refuse_rearm BEFORE INSERT ON wait_rearms BEGIN SELECT RAISE(ABORT,'fault'); END;").unwrap();
        assert!(db.rearm_wait(&first.wait_id,None).is_err());
        assert_eq!(db.current_head().unwrap(),head);
        assert_eq!(count(&db.connection,"SELECT count(*) FROM wait_conditions"),1);
        db.connection.execute_batch("DROP TRIGGER refuse_rearm;").unwrap();
        assert!(!db.rearm_wait(&first.wait_id,None).unwrap().already_registered);
        db.apply_plan_proposal(&proposal(&one("task","new plan","")),0,"rearm-new-plan").unwrap();
        let head=db.current_head().unwrap();
        assert!(db.rearm_wait(&first.wait_id,None).is_err());
        assert!(db.rearm_wait("missing",None).is_err());
        assert!(db.rearm_wait(&first.wait_id,Some(-1)).is_err());
        assert_eq!(db.current_head().unwrap(),head);
    }

    #[test]
    fn expired_wait_wakes_past_event_backlog_without_releasing_capacity() {
        let (_root,mut db)=fixture();seed_task(&mut db,"task");
        db.connection.execute("INSERT INTO attempts VALUES('deadline-attempt','task',1,'running',NULL,'deadline-slot',0)",[]).unwrap();
        let wait=db.register_wait_with_deadline("task",Some("deadline-attempt"),"validation_completion",Some(1)).unwrap();
        db.connection.execute_batch("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<1500) INSERT INTO events(kind,entity,revision,payload_version,payload) SELECT 'unrelated','other',1,1,'{}' FROM n;").unwrap();
        let budget=super::super::read_budget::ReadBudget::new(super::super::controlled::ReadControl::new(std::time::Instant::now()+Duration::from_secs(2),Default::default()));
        assert_eq!(db.service_waits(&budget).unwrap().notified,1);
        let replay=db.replay_wait(&wait.wait_id).unwrap();
        assert!(replay.wake_requested&&replay.already_replayed);
        assert!(!replay.proved);
        assert_eq!(replay.events_applied,1);
        assert_eq!(count(&db.connection,"SELECT count(*) FROM attempts WHERE termination_observed=0"),1);
        let path=PathBuf::from(db.connection.path().unwrap());drop(db);
        let mut db=SqliteStore::open(&path).unwrap();
        assert!(db.replay_wait(&wait.wait_id).unwrap().already_replayed);
        assert_eq!(count(&db.connection,"SELECT count(*) FROM inbox_items"),1);
    }

    #[test]
    fn wait_deadline_is_part_of_registration_identity_and_cannot_be_mutated() {
        let (_root,mut db)=fixture();seed_task(&mut db,"task");
        let future=jiff::Timestamp::now().as_millisecond()+60_000;
        let original=db.register_wait("task",None,"user_decision").unwrap();
        let wait=db.register_wait_with_deadline("task",None,"user_decision",Some(future)).unwrap();
        assert_ne!(wait.wait_id,original.wait_id);
        assert!(db.register_wait_with_deadline("task",None,"user_decision",Some(future)).unwrap().already_registered);
        assert!(!db.replay_wait(&wait.wait_id).unwrap().wake_requested);
        assert!(db.connection.execute("UPDATE wait_conditions SET deadline_unix_ms=0 WHERE wait_id=?1",[&wait.wait_id]).is_err());
        assert!(db.register_wait_with_deadline("task",None,"user_decision",Some(-1)).is_err());
        assert!(db.register_wait_with_deadline("task",None,"user_decision",Some(i64::MAX)).is_err());
    }

    #[test]
    fn expired_wait_notice_failure_rolls_back_the_deadline_event() {
        let (_root,mut db)=fixture();seed_task(&mut db,"task");
        let wait=db.register_wait_with_deadline("task",None,"user_decision",Some(0)).unwrap();
        let head=db.current_head().unwrap();
        db.connection.execute_batch("CREATE TRIGGER fail_deadline_notice BEFORE INSERT ON inbox_items BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
        assert!(db.replay_wait(&wait.wait_id).is_err());
        assert_eq!(db.current_head().unwrap(),head);
        assert_eq!(count(&db.connection,"SELECT count(*) FROM wait_replay_events"),0);
        db.connection.execute_batch("DROP TRIGGER fail_deadline_notice;").unwrap();
        assert!(db.replay_wait(&wait.wait_id).unwrap().wake_requested);
        assert_eq!(count(&db.connection,"SELECT count(*) FROM inbox_items"),1);
    }

    #[test]
    fn proposal_key_used_before_a_replan_cannot_be_relabelled_as_its_response() {
        let (_root,mut db)=fixture();seed_task(&mut db,"task");
        let feedback=record_rejection(&mut db,"preexisting-key","task");
        let key=sha256_hex(format!("replan\0{feedback}").as_bytes());
        let raw=proposal(&one("task","earlier unrelated proposal",""));
        db.apply_plan_proposal(&raw,0,&key).unwrap();
        db.request_replan(&feedback).unwrap();
        assert!(matches!(db.apply_plan_proposal(&raw,1,&key),Err(StoreError::Conflict)));
        assert_eq!(count(&db.connection,"SELECT count(*) FROM replan_responses"),0);
        assert_eq!(count(&db.connection,"SELECT count(*) FROM inbox_items WHERE done=1"),0);
    }

    #[test]
    fn replan_notice_failure_rolls_back_ack_and_budget() {
        let (_root,mut db)=fixture();seed_task(&mut db,"task");
        let feedback=record_rejection(&mut db,"notice-fail","task");
        let head=db.current_head().unwrap();
        db.connection.execute_batch("CREATE TRIGGER fail_notice BEFORE INSERT ON inbox_items BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
        assert!(db.request_replan(&feedback).is_err());
        assert_eq!(db.current_head().unwrap(),head);
        assert_eq!(count(&db.connection,"SELECT count(*) FROM replan_requests"),0);
        let state:String=db.connection.query_row("SELECT state FROM feedback_items WHERE feedback_id=?1",[feedback],|row|row.get(0)).unwrap();
        assert_eq!(state,"open");
    }

    #[test]
    fn replan_response_failure_is_atomic_and_stale_requests_cannot_rebase() {
        let (_root,mut db)=fixture();seed_task(&mut db,"task");
        let feedback=record_rejection(&mut db,"response-fail","task");
        let ReplanDecision::Automatic{replan_id,..}=db.request_replan(&feedback).unwrap() else{panic!()};
        let raw=proposal(&one("task","new approach",""));
        let head=db.current_head().unwrap();
        db.connection.execute_batch("CREATE TRIGGER fail_response BEFORE INSERT ON replan_responses BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
        assert!(db.apply_plan_proposal(&raw,0,&replan_id).is_err());
        assert_eq!(db.current_head().unwrap(),head);
        assert_eq!(count(&db.connection,"SELECT count(*) FROM plan_proposals"),0);
        assert_eq!(count(&db.connection,"SELECT count(*) FROM inbox_items WHERE done=1"),0);
        db.connection.execute_batch("DROP TRIGGER fail_response;").unwrap();
        db.apply_plan_proposal(&raw,0,"another-request").unwrap();
        assert!(matches!(db.apply_plan_proposal(&raw,1,&replan_id),Err(StoreError::StalePlanParent(1))));
        assert_eq!(count(&db.connection,"SELECT count(*) FROM replan_responses"),0);
    }

    #[test]
    fn replan_budget_survives_wall_clock_rollback_and_restart() {
        for automatic in [false,true] {
            let (_root,mut db)=fixture();seed_task(&mut db,"task");
            // Model a reset committed before the wall clock moved backward.
            // Do not change the host clock or rewrite immutable audit records.
            record_replan_reset(&db.connection,0,jiff::Timestamp::now().as_millisecond()+86_400_000).unwrap();
            let ids:Vec<_>=(0..4).map(|n|record_rejection(&mut db,&format!("clock-{n}"),"task")).collect();
            if automatic {
                db.set_auto_replans(db.current_head().unwrap(),true).unwrap();
                db.connection.execute("UPDATE project_control SET state='active'",[]).unwrap();
                let budget=read_budget::ReadBudget::new(controlled::ReadControl::new(std::time::Instant::now()+Duration::from_secs(5),Default::default()));
                db.service_replans(&budget).unwrap();
            } else {
                for id in &ids {db.request_replan(id).unwrap();}
            }
            assert_eq!(count(&db.connection,"SELECT count(*) FROM replan_requests WHERE outcome='automatic'"),2,"automatic={automatic}");
            assert_eq!(count(&db.connection,"SELECT count(*) FROM replan_requests WHERE outcome='escalated'"),1);
            let path=PathBuf::from(db.connection.path().unwrap());drop(db);
            let mut db=SqliteStore::open(&path).unwrap();
            let more=record_rejection(&mut db,"clock-after-restart","task");
            assert!(matches!(db.request_replan(&more).unwrap(),ReplanDecision::Escalated{..}));
            db.apply_plan_proposal(&proposal(&one("task","new plan","")),0,"clock-new-plan").unwrap();
            let fresh=record_rejection(&mut db,"clock-new-revision","task");
            assert!(matches!(db.request_replan(&fresh).unwrap(),ReplanDecision::Automatic{automatic_count:1,..}));
        }
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
        let feedback = record_rejection(&mut db, &"d".repeat(64), "task");
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
        db.connection
            .execute(
                "INSERT INTO acceptance_policies(task_id,contract_revision,policy_id,body) VALUES('task',2,'policy-1','{\"version\":1}')",
                [],
            )
            .unwrap();
        db.connection
            .execute(
                "INSERT INTO verification_runs(run_id,project_store,idempotency_key,payload_digest,submission_id,task_id,contract_revision,contract_digest,attempt_id,policy_id,policy_digest,commit_oid,tree_oid,object_format,memory_fence,isolation,argv,library_manifest,state,reason,exit_status,receipt_digest,store_device,store_inode,created_unix_ms) VALUES(?1,?2,'verify-2',?3,?4,'task',2,?3,'attempt-1','policy-1',?5,?6,NULL,'sha1',0,'linux-unshare-user-pid-mount-v1','[\"/usr/bin/true\"]','[]','rejected','checks_failed',1,NULL,1,1,0)",
                params![
                    "e".repeat(64),
                    project_store,
                    next_digest,
                    "c".repeat(64),
                    sha256_hex(br#"{"version":1}"#),
                    "a".repeat(40)
                ],
            )
            .unwrap();
        let later_raw = b"contract-v3-installed";
        let later_digest = sha256_hex(later_raw);
        db.connection
            .execute(
                "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('contract.installed','task',3,1,?1)",
                [serde_json::json!({"digest": later_digest}).to_string()],
            )
            .unwrap();
        let later_seq: i64 = db
            .connection
            .query_row("SELECT max(sequence) FROM events", [], |row| row.get(0))
            .unwrap();
        db.connection
            .execute(
                "INSERT INTO task_contracts(task_id,contract_revision,plan_revision,project_store,expected_head,repository,base_oid,object_format,memory_snapshot_id,route,raw_bytes,raw_digest,installed_seq) VALUES('task',3,NULL,?1,0,'/tmp/repo',?2,'sha1',NULL,'verify_only',?3,?4,?5)",
                params![project_store, "b".repeat(40), later_raw, later_digest, later_seq],
            )
            .unwrap();
        let replay = db.rework_acceptance(&feedback, true).unwrap();
        assert_eq!(replay.attempt_id, rework.attempt_id);
        assert_eq!(replay.contract_revision, Some(2));
        assert_eq!(count(&db.connection, "SELECT count(*) FROM attempts"), 2);
        assert_eq!(
            count(
                &db.connection,
                "SELECT count(*) FROM attempts WHERE termination_observed=0"
            ),
            1
        );
        let original: (String, i64) = db
            .connection
            .query_row(
                "SELECT state, contract_revision FROM verification_runs WHERE run_id=?1",
                ["d".repeat(64)],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(original, ("rejected".into(), 1));
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
    #[test]
    fn wait_service_rotates_durably_and_notifies_only_once() {
        let (root,mut db)=fixture();
        let mut waits=Vec::new();
        for n in 0..9 {
            let task=format!("waiter-{n}");seed_task(&mut db,&task);
            waits.push(db.register_wait(&task,None,"user_decision").unwrap().wait_id);
        }
        waits.sort();let last=waits.last().unwrap();
        db.connection.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('wait.wake',?1,1,1,'{}')",[last]).unwrap();
        let budget=||read_budget::ReadBudget::new(controlled::ReadControl::new(std::time::Instant::now()+Duration::from_secs(5),Default::default()));
        assert_eq!(db.service_waits(&budget()).unwrap().notified,0);
        drop(db);
        let mut db=SqliteStore::open(&root.path().join("state.db")).unwrap();
        assert_eq!(db.service_waits(&budget()).unwrap().notified,1);
        assert_eq!(db.service_waits(&budget()).unwrap().notified,0);
        assert_eq!(count(&db.connection,"SELECT count(*) FROM inbox_items"),1);
        assert_eq!(count(&db.connection,"SELECT count(*) FROM wait_conditions WHERE wake_requested=1"),1);
    }

    #[test]
    fn wait_notice_failure_rolls_back_replay_for_retry() {
        let (_root,mut db)=fixture();seed_task(&mut db,"waiting");
        let wait=db.register_wait("waiting",None,"user_decision").unwrap();
        db.connection.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('wait.wake',?1,1,1,'{}')",[&wait.wait_id]).unwrap();
        db.connection.execute_batch("CREATE TRIGGER fail_wait_notice BEFORE INSERT ON inbox_items BEGIN SELECT RAISE(ABORT,'fixture'); END").unwrap();
        assert!(db.replay_wait(&wait.wait_id).is_err());
        assert_eq!(count(&db.connection,"SELECT count(*) FROM wait_conditions WHERE wake_requested=1"),0);
        assert_eq!(count(&db.connection,"SELECT count(*) FROM wait_replay_events"),0);
        db.connection.execute_batch("DROP TRIGGER fail_wait_notice").unwrap();
        assert!(db.replay_wait(&wait.wait_id).unwrap().wake_requested);
        assert!(db.replay_wait(&wait.wait_id).unwrap().already_replayed);
        assert_eq!(count(&db.connection,"SELECT count(*) FROM inbox_items"),1);
    }

    #[test]
    fn upgraded_wait_resumes_in_bounded_pages_after_restart() {
        let (temp, mut db) = fixture();
        seed_task(&mut db, "task");
        let wait = db.register_wait("task", None, "dependency_evidence").unwrap();
        super::super::test_schema::historical(&db.connection, 41).unwrap();
        // The previous release persisted an empty first replay as terminal.
        db.connection.execute("UPDATE wait_conditions SET state='replayed', replayed_through=cursor_sequence, wake_requested=0 WHERE wait_id=?1", [&wait.wait_id]).unwrap();
        db.upgrade_v1().unwrap();
        for _ in 0..1001 {
            db.connection.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('task.noted','other',1,1,'{}')", []).unwrap();
        }
        db.connection.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('wait.wake',?1,1,1,'{}')", [&wait.wait_id]).unwrap();
        let first = db.replay_wait(&wait.wait_id).unwrap();
        assert_eq!(first.events_applied, 1000);
        assert!(!first.wake_requested);
        drop(db);
        let mut db = SqliteStore::open(&temp.path().join("state.db")).unwrap();
        let second = db.replay_wait(&wait.wait_id).unwrap();
        assert_eq!(second.events_applied, 2);
        assert!(second.wake_requested);
        assert!(second.replayed_through > first.replayed_through);
        assert_eq!(count(&db.connection, "SELECT count(*) FROM wait_replay_events"), 1002);
        assert!(db.replay_wait(&wait.wait_id).unwrap().already_replayed);
    }

    #[test]
    fn upgrade_discards_an_unrelated_historical_wake() {
        let (_temp, mut db) = fixture();
        seed_task(&mut db, "task");
        let wait = db.register_wait("task", None, "dependency_evidence").unwrap();
        super::super::test_schema::historical(&db.connection, 41).unwrap();
        db.connection.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('wait.wake','another-wait',1,1,'{}')", []).unwrap();
        let through = db.current_head().unwrap();
        db.connection.execute("UPDATE wait_conditions SET state='replayed', replayed_through=?2, wake_requested=1 WHERE wait_id=?1", params![wait.wait_id, through]).unwrap();
        db.upgrade_v1().unwrap();
        assert!(!db.replay_wait(&wait.wait_id).unwrap().wake_requested);
        db.connection.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('wait.wake',?1,1,1,'{}')", [&wait.wait_id]).unwrap();
        assert!(db.replay_wait(&wait.wait_id).unwrap().wake_requested);
    }

}

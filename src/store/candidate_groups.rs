//! Candidate groups (migration 0053, docs/telemetry/contracts-quality.md §3).
//! Sequential arms only: an arm is an ordinary attempt of the group's task,
//! reserved through the existing launch path and bound to its arm inside that
//! reservation transaction. Nothing here launches, verifies, integrates or
//! stores cost; every row is append-only.
use super::*;
use crate::domain::agent_configuration;
use serde::Serialize;

pub const CANDIDATE_GROUP_SCHEMA: &str = "candidate_group.v1";
/// Operator selection reasons for `selected` and for `no_selection`.
pub const SELECTED_REASONS: [&str; 3] = ["operator_judgment", "first_passing_verification", "unspecified"];
pub const NO_SELECTION_REASONS: [&str; 3] = ["no_candidate", "none_acceptable", "unspecified"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CandidateArm { pub arm: u32, pub configuration_id: String, pub profile_digest: String }

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CandidateGroup {
    pub group_id: String,
    pub task_id: String,
    pub contract_revision: Option<u64>,
    pub arms: Vec<CandidateArm>,
    pub creator_principal: String,
    pub sealed_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CandidateSelection {
    pub group_id: String,
    /// `selected` or `no_selection`.
    pub outcome: String,
    pub arm: Option<u32>,
    pub attempt_id: Option<String>,
    pub submission_id: Option<String>,
    pub selector_kind: String,
    pub selector_principal: String,
    pub reason: String,
    /// Per bound arm, at selection time: `{arm, attempt_id, submission_id, verification}`.
    pub evidence: serde_json::Value,
    pub selected_unix_ms: i64,
}

/// What an operator selects: one arm (its first candidate unless `submission`
/// names another of its attempt's submissions), or no arm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionChoice { Arm { arm: u32, submission: Option<String> }, NoSelection }

fn invalid(message: String) -> StoreError { StoreError::Invalid(message) }

fn schema_53(tx: &Connection) -> Result<()> {
    check_schema(tx)?;
    let version: u32 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 53 { return Err(StoreError::UnsupportedSchema(version)); }
    Ok(())
}

/// The latest retained native profile named `name`, as `(profile_digest, profile)`.
fn retained_profile(tx: &Connection, name: &str) -> Result<(String, FrozenProfile)> {
    let mut stmt = tx.prepare("SELECT profile_digest,report,report_digest FROM native_profiles ORDER BY sequence DESC LIMIT 256")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let (digest, report, report_digest): (String, String, String) = (row.get(0)?, row.get(1)?, row.get(2)?);
        if format!("{:x}", Sha256::digest(report.as_bytes())) != report_digest { return Err(StoreError::Corrupt("native profile report digest mismatch".into())); }
        let value: serde_json::Value = serde_json::from_str(&report).map_err(|_| StoreError::Corrupt("invalid native profile report".into()))?;
        if value["preparation"]["profile"]["name"].as_str() != Some(name) { continue; }
        let profile: FrozenProfile = serde_json::from_value(value["preparation"]["profile"].clone()).map_err(|_| StoreError::Corrupt("invalid retained profile".into()))?;
        return Ok((digest, profile));
    }
    Err(invalid(format!("no retained native profile named {name}")))
}

/// Bind the new attempt to its group's next arm, in the reservation
/// transaction after its dispatch decision. The next arm is the lowest unbound
/// arm of the task's sealed, unselected group for this contract revision; the
/// attempt binds only if its chosen configuration is that arm's. Anything else
/// binds nothing and never refuses the reservation.
pub(super) fn bind(tx: &Connection, inputs: &LaunchInputs, attempt: &AttemptId, now: i64) -> Result<()> {
    let Some(profile) = inputs.effective_profile.as_ref() else { return Ok(()) };
    let revision = inputs.task_contract.as_ref().map(|c| integer(c.revision)).transpose()?;
    let next: Option<(String, i64, String)> = tx.query_row("SELECT g.group_id,a.arm,a.configuration_id FROM candidate_groups g JOIN candidate_group_arms a ON a.group_id=g.group_id
        WHERE g.task_id=?1 AND g.contract_revision IS ?2 AND g.sealed_unix_ms IS NOT NULL
          AND NOT EXISTS(SELECT 1 FROM candidate_selections s WHERE s.group_id=g.group_id)
          AND NOT EXISTS(SELECT 1 FROM candidate_arm_attempts b WHERE b.group_id=a.group_id AND b.arm=a.arm)
        ORDER BY a.arm LIMIT 1", params![inputs.task.as_str(), revision], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
    let Some((group, arm, configuration)) = next else { return Ok(()) };
    if agent_configuration(profile).id != configuration { return Ok(()); }
    tx.execute("INSERT INTO candidate_arm_attempts(group_id,arm,attempt_id,bound_unix_ms,source) VALUES(?1,?2,?3,?4,'admit_prepared')",
        params![group, arm, attempt.as_str(), now])?;
    Ok(())
}

/// Combined verification of one submission: its latest run per policy;
/// `rejected` if any is, `accepted` if every one is, else `pending`.
fn verification(tx: &Connection, submission: &str) -> Result<&'static str> {
    let states: Vec<String> = tx.prepare("SELECT (SELECT v.state FROM verification_runs v WHERE v.submission_id=?1 AND v.policy_id=p.policy_id ORDER BY v.created_unix_ms DESC,v.rowid DESC LIMIT 1)
        FROM (SELECT DISTINCT policy_id FROM verification_runs WHERE submission_id=?1) p")?
        .query_map([submission], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    Ok(if states.iter().any(|s| s == "rejected") { "rejected" } else if !states.is_empty() && states.iter().all(|s| s == "accepted") { "accepted" } else { "pending" })
}

impl SqliteStore {
    /// Seal a candidate group for `task`'s current contract revision: one arm
    /// per retained native profile name, in launch order, each a distinct
    /// configuration. Arms are fixed here, before any of them is reserved.
    pub fn create_candidate_group(&mut self, task: &str, profiles: &[String], principal: &str, now: i64) -> Result<CandidateGroup> {
        if !(2..=8).contains(&profiles.len()) { return Err(invalid("a candidate group has 2 to 8 arms".into())); }
        if principal.is_empty() || principal.len() > 128 { return Err(invalid("invalid creator principal".into())); }
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema_53(&tx)?;
        let state: String = tx.query_row("SELECT state FROM tasks WHERE id=?1", [task], |r| r.get(0)).optional()?
            .ok_or_else(|| invalid(format!("no task {task}")))?;
        if matches!(state.as_str(), "succeeded" | "cancelled") { return Err(invalid(format!("task {task} is {state}"))); }
        let revision: Option<i64> = tx.query_row("SELECT max(contract_revision) FROM task_contracts WHERE task_id=?1", [task], |r| r.get(0))?;
        if tx.query_row("SELECT EXISTS(SELECT 1 FROM candidate_groups WHERE task_id=?1 AND contract_revision IS ?2)", params![task, revision], |r| r.get::<_, bool>(0))? {
            return Err(invalid(format!("task {task} already has a candidate group for this contract revision")));
        }
        let mut arms = Vec::with_capacity(profiles.len());
        for (index, name) in profiles.iter().enumerate() {
            let (profile_digest, profile) = retained_profile(&tx, name)?;
            let configuration = agent_configuration(&profile);
            if arms.iter().any(|a: &CandidateArm| a.configuration_id == configuration.id) {
                return Err(invalid(format!("arm {} repeats the configuration of an earlier arm", index + 1)));
            }
            super::dispatch_log::insert_configuration(&tx, &configuration, now)?;
            arms.push(CandidateArm { arm: index as u32 + 1, configuration_id: configuration.id, profile_digest });
        }
        let canonical_json = serde_json::json!({"arms": arms, "contract_revision": revision, "created_unix_ms": now,
            "schema": CANDIDATE_GROUP_SCHEMA, "task_id": task}).to_string();
        let group_id = format!("sha256:{:x}", Sha256::digest(canonical_json.as_bytes()));
        tx.execute("INSERT INTO candidate_groups(group_id,task_id,contract_revision,arm_count,creator_principal,canonical_json,created_unix_ms,sealed_unix_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,NULL)",
            params![group_id, task, revision, arms.len() as i64, principal, canonical_json, now])?;
        for arm in &arms {
            tx.execute("INSERT INTO candidate_group_arms(group_id,arm,configuration_id,profile_digest) VALUES(?1,?2,?3,?4)",
                params![group_id, arm.arm, arm.configuration_id, arm.profile_digest])?;
        }
        tx.execute("UPDATE candidate_groups SET sealed_unix_ms=?2 WHERE group_id=?1", params![group_id, now])?;
        tx.commit()?;
        Ok(CandidateGroup { group_id, task_id: task.to_owned(), contract_revision: revision.map(|r| r as u64), arms, creator_principal: principal.to_owned(), sealed_unix_ms: now })
    }

    /// Record the group's one selection. A selected arm must be bound and have
    /// a candidate (a submission of its attempt). Selection is not
    /// verification: it verifies, integrates and reassigns nothing.
    pub fn select_candidate(&mut self, group: &str, choice: &SelectionChoice, reason: &str, principal: &str, now: i64) -> Result<CandidateSelection> {
        if principal.is_empty() || principal.len() > 128 { return Err(invalid("invalid selector principal".into())); }
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema_53(&tx)?;
        let arm_count: i64 = tx.query_row("SELECT arm_count FROM candidate_groups WHERE group_id=?1 AND sealed_unix_ms IS NOT NULL", [group], |r| r.get(0)).optional()?
            .ok_or_else(|| invalid(format!("no candidate group {group}")))?;
        if tx.query_row("SELECT EXISTS(SELECT 1 FROM candidate_selections WHERE group_id=?1)", [group], |r| r.get::<_, bool>(0))? {
            return Err(invalid(format!("candidate group {group} already has a selection")));
        }
        let bound: Vec<(i64, String)> = tx.prepare("SELECT arm,attempt_id FROM candidate_arm_attempts WHERE group_id=?1 ORDER BY arm")?
            .query_map([group], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        let mut evidence = Vec::with_capacity(bound.len());
        for (arm, attempt) in &bound {
            let first: Option<String> = tx.query_row("SELECT submission_id FROM result_submissions WHERE attempt_id=?1 ORDER BY created_unix_ms,rowid LIMIT 1", [attempt], |r| r.get(0)).optional()?;
            let state = match &first { Some(s) => verification(&tx, s)?, None => "no_candidate" };
            evidence.push(serde_json::json!({"arm": arm, "attempt_id": attempt, "submission_id": first, "verification": state}));
        }
        let (outcome, arm, attempt, submission) = match choice {
            SelectionChoice::NoSelection => {
                if !NO_SELECTION_REASONS.contains(&reason) { return Err(invalid(format!("unknown no-selection reason {reason}"))); }
                ("no_selection", None, None, None)
            }
            SelectionChoice::Arm { arm, submission } => {
                if !SELECTED_REASONS.contains(&reason) { return Err(invalid(format!("unknown selection reason {reason}"))); }
                if *arm == 0 || i64::from(*arm) > arm_count { return Err(invalid(format!("arm {arm} is not a member of {group}"))); }
                let attempt = bound.iter().find(|(a, _)| *a == i64::from(*arm)).map(|(_, attempt)| attempt.clone())
                    .ok_or_else(|| invalid(format!("arm {arm} has no attempt")))?;
                let submission = match submission {
                    Some(id) => tx.query_row("SELECT submission_id FROM result_submissions WHERE submission_id=?1 AND attempt_id=?2", params![id, attempt], |r| r.get(0)).optional()?
                        .ok_or_else(|| invalid(format!("submission {id} is not a candidate of arm {arm}")))?,
                    None => evidence.iter().find(|e| e["arm"] == i64::from(*arm)).and_then(|e| e["submission_id"].as_str().map(str::to_owned))
                        .ok_or_else(|| invalid(format!("arm {arm} has no candidate")))?,
                };
                ("selected", Some(*arm), Some(attempt), Some(submission))
            }
        };
        let evidence = serde_json::Value::Array(evidence);
        tx.execute("INSERT INTO candidate_selections(group_id,outcome,arm,attempt_id,submission_id,selector_kind,selector_principal,reason,evidence,selected_unix_ms) VALUES(?1,?2,?3,?4,?5,'operator',?6,?7,?8,?9)",
            params![group, outcome, arm, attempt, submission, principal, reason, evidence.to_string(), now])?;
        tx.commit()?;
        Ok(CandidateSelection { group_id: group.to_owned(), outcome: outcome.into(), arm, attempt_id: attempt, submission_id: submission, selector_kind: "operator".into(),
            selector_principal: principal.to_owned(), reason: reason.to_owned(), evidence, selected_unix_ms: now })
    }
}

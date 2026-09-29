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
    /// Per bound arm, at selection time: `{arm, attempt_id, submission_id,
    /// verification, arm_outcome}`, plus `rank` for a selected arm (every
    /// selector) and `presented`, `judge_configuration_id` for a judge.
    pub evidence: serde_json::Value,
    pub selected_unix_ms: i64,
}

/// What an operator selects: one arm (its first candidate unless `submission`
/// names another of its attempt's submissions) with its runner-up arms in
/// order (evidence `rank`), or no arm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionChoice { Arm { arm: u32, submission: Option<String>, runner_up: Vec<u32> }, NoSelection }

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

/// Principal of the deterministic selection rule (contracts-quality.md §4):
/// the first arm in launch order whose verified outcome is `accepted`.
pub const RULE_SELECTOR: &str = "rule:first_accepted_in_launch_order.v1";
/// Blind presentation order for a judge (contracts-quality.md §4).
pub const PRESENTATION: &str = "candidate_presentation.v1";
const TERMINAL_ATTEMPT: [&str; 4] = ["completed", "failed", "cancelled", "lost"];

/// Combined verification of one submission (contracts §4): every acceptance
/// policy of its task revision and every policy with a run, each decided by
/// its latest run; `rejected` if any is, `accepted` if every one is, else
/// `pending`.
fn verification(tx: &Connection, submission: &str) -> Result<&'static str> {
    let states: Vec<Option<String>> = tx.prepare("SELECT (SELECT v.state FROM verification_runs v WHERE v.submission_id=?1 AND v.policy_id=p.policy_id ORDER BY v.created_unix_ms DESC,v.rowid DESC LIMIT 1)
        FROM (SELECT a.policy_id FROM acceptance_policies a JOIN result_submissions s ON s.task_id=a.task_id AND s.contract_revision=a.contract_revision WHERE s.submission_id=?1
            UNION SELECT policy_id FROM verification_runs WHERE submission_id=?1) p")?
        .query_map([submission], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    let is = |state: &'static str| move |s: &Option<String>| s.as_deref() == Some(state);
    Ok(if states.iter().any(is("rejected")) { "rejected" } else if !states.is_empty() && states.iter().all(is("accepted")) { "accepted" } else { "pending" })
}

/// The verified outcome of one bound arm's attempt now (`arm_outcome.v1`,
/// contracts-quality.md §4), read-only: over its submissions by
/// `(created_unix_ms, rowid)`, `accepted` if any is (`accepted` names the
/// first such); else `pending` if any submission's verification is pending or
/// the attempt is not terminal; else `rejected` with a submission,
/// `no_candidate` without. `first` is the first candidate and
/// `first_verification` its combined verification (`no_candidate` without).
/// Shared by the selectors here and telemetry's reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArmOutcome { pub first: Option<String>, pub first_verification: &'static str, pub outcome: &'static str, pub accepted: Option<String> }

/// [`ArmOutcome`] of `attempt`, read from `db` without writing.
pub fn arm_outcome(db: &Connection, attempt: &str) -> Result<ArmOutcome> {
    let state: Option<String> = db.query_row("SELECT state FROM attempts WHERE id=?1", [attempt], |r| r.get(0)).optional()?;
    let submissions: Vec<String> = db.prepare("SELECT submission_id FROM result_submissions WHERE attempt_id=?1 ORDER BY created_unix_ms,rowid")?
        .query_map([attempt], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    let states = submissions.iter().map(|s| verification(db, s)).collect::<Result<Vec<_>>>()?;
    let accepted = submissions.iter().zip(&states).find(|(_, v)| **v == "accepted").map(|(s, _)| s.clone());
    let terminal = state.as_deref().is_some_and(|s| TERMINAL_ATTEMPT.contains(&s));
    let outcome = if accepted.is_some() { "accepted" } else if states.contains(&"pending") || !terminal { "pending" }
        else if submissions.is_empty() { "no_candidate" } else { "rejected" };
    Ok(ArmOutcome { first: submissions.first().cloned(), first_verification: states.first().copied().unwrap_or("no_candidate"), outcome, accepted })
}

/// One bound arm at selection time, with its [`ArmOutcome`].
struct BoundArm { arm: i64, attempt: String, first: Option<String>, first_verification: &'static str, outcome: &'static str, accepted: Option<String> }

fn bound_arms(tx: &Connection, group: &str) -> Result<Vec<BoundArm>> {
    let bound: Vec<(i64, String)> = tx.prepare("SELECT arm,attempt_id FROM candidate_arm_attempts WHERE group_id=?1 ORDER BY arm")?
        .query_map([group], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut arms = Vec::with_capacity(bound.len());
    for (arm, attempt) in bound {
        let ArmOutcome { first, first_verification, outcome, accepted } = arm_outcome(tx, &attempt)?;
        arms.push(BoundArm { arm, attempt, first, first_verification, outcome, accepted });
    }
    Ok(arms)
}

/// Blind presentation positions (1-based) of the arms' first candidates:
/// ordered by `sha256("candidate_presentation.v1:" + group + ":" + submission)`.
/// Arms without a candidate are not presented.
fn presentation(group: &str, arms: &[BoundArm]) -> Vec<Option<usize>> {
    let key = |submission: &str| format!("{:x}", Sha256::digest(format!("{PRESENTATION}:{group}:{submission}").as_bytes()));
    let mut order: Vec<(String, usize)> = arms.iter().enumerate().filter_map(|(i, a)| a.first.as_deref().map(|s| (key(s), i))).collect();
    order.sort();
    let mut positions = vec![None; arms.len()];
    for (position, (_, i)) in order.iter().enumerate() { positions[*i] = Some(position + 1); }
    positions
}

/// `{arm, attempt_id, submission_id, verification, arm_outcome}` per bound arm:
/// the first candidate and its combined verification, and the arm's outcome.
fn evidence(arms: &[BoundArm]) -> Vec<serde_json::Value> {
    arms.iter().map(|a| serde_json::json!({"arm": a.arm, "attempt_id": a.attempt, "submission_id": a.first, "verification": a.first_verification, "arm_outcome": a.outcome})).collect()
}

/// Runner-up order of an operator or judge selection: `rank` 1 for the
/// winner, 2, 3, ... for each runner-up in the order given, null for every
/// other arm. Runner-ups must be bound arms, distinct, and not the winner.
fn ranked(arms: &[BoundArm], evidence: Vec<serde_json::Value>, winner: i64, runner_up: &[i64]) -> Result<Vec<serde_json::Value>> {
    for (i, arm) in runner_up.iter().enumerate() {
        if *arm == winner { return Err(invalid(format!("runner-up arm {arm} is the selected arm"))); }
        if runner_up[..i].contains(arm) { return Err(invalid(format!("runner-up arm {arm} is repeated"))); }
        if !arms.iter().any(|a| a.arm == *arm) { return Err(invalid(format!("runner-up arm {arm} is not a bound arm"))); }
    }
    Ok(evidence.into_iter().zip(arms).map(|(mut e, a)| {
        e["rank"] = if a.arm == winner { serde_json::json!(1) } else { runner_up.iter().position(|r| *r == a.arm).map_or(serde_json::Value::Null, |p| serde_json::json!(p + 2)) };
        e
    }).collect())
}

/// A judge configuration is a contracts §2 configuration ID (`sha256:` + 64
/// lowercase hex), recorded by value.
fn judge_configuration(configuration: Option<&str>) -> Result<serde_json::Value> {
    match configuration {
        None => Ok(serde_json::Value::Null),
        Some(id) if id.strip_prefix("sha256:").is_some_and(|h| h.len() == 64 && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))) => Ok(serde_json::json!(id)),
        Some(id) => Err(invalid(format!("judge configuration {id} is not a configuration ID (sha256: and 64 lowercase hex)"))),
    }
}

/// The sealed group's arm count, if it has no selection yet.
fn open_group(tx: &Connection, group: &str) -> Result<i64> {
    schema_53(tx)?;
    let arm_count: i64 = tx.query_row("SELECT arm_count FROM candidate_groups WHERE group_id=?1 AND sealed_unix_ms IS NOT NULL", [group], |r| r.get(0)).optional()?
        .ok_or_else(|| invalid(format!("no candidate group {group}")))?;
    if tx.query_row("SELECT EXISTS(SELECT 1 FROM candidate_selections WHERE group_id=?1)", [group], |r| r.get::<_, bool>(0))? {
        return Err(invalid(format!("candidate group {group} already has a selection")));
    }
    Ok(arm_count)
}

/// The one selection row; `(arm, attempt, submission)` for `selected`, else none.
struct Decision<'a> { group: &'a str, picked: Option<(u32, String, String)>, kind: &'static str, principal: &'a str, reason: &'a str, evidence: Vec<serde_json::Value>, now: i64 }

fn insert_selection(tx: rusqlite::Transaction<'_>, d: Decision<'_>) -> Result<CandidateSelection> {
    let evidence = serde_json::Value::Array(d.evidence);
    let (arm, attempt, submission) = match d.picked { Some((arm, attempt, submission)) => (Some(arm), Some(attempt), Some(submission)), None => (None, None, None) };
    let outcome = if arm.is_some() { "selected" } else { "no_selection" };
    tx.execute("INSERT INTO candidate_selections(group_id,outcome,arm,attempt_id,submission_id,selector_kind,selector_principal,reason,evidence,selected_unix_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![d.group, outcome, arm, attempt, submission, d.kind, d.principal, d.reason, evidence.to_string(), d.now])?;
    tx.commit()?;
    Ok(CandidateSelection { group_id: d.group.to_owned(), outcome: outcome.into(), arm, attempt_id: attempt, submission_id: submission, selector_kind: d.kind.into(),
        selector_principal: d.principal.to_owned(), reason: d.reason.to_owned(), evidence, selected_unix_ms: d.now })
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
        let arm_count = open_group(&tx, group)?;
        let arms = bound_arms(&tx, group)?;
        let mut evidence = evidence(&arms);
        let picked = match choice {
            SelectionChoice::NoSelection => {
                if !NO_SELECTION_REASONS.contains(&reason) { return Err(invalid(format!("unknown no-selection reason {reason}"))); }
                None
            }
            SelectionChoice::Arm { arm, submission, runner_up } => {
                if !SELECTED_REASONS.contains(&reason) { return Err(invalid(format!("unknown selection reason {reason}"))); }
                if *arm == 0 || i64::from(*arm) > arm_count { return Err(invalid(format!("arm {arm} is not a member of {group}"))); }
                let bound = arms.iter().find(|a| a.arm == i64::from(*arm)).ok_or_else(|| invalid(format!("arm {arm} has no attempt")))?;
                let submission = match submission {
                    Some(id) => tx.query_row("SELECT submission_id FROM result_submissions WHERE submission_id=?1 AND attempt_id=?2", params![id, bound.attempt], |r| r.get(0)).optional()?
                        .ok_or_else(|| invalid(format!("submission {id} is not a candidate of arm {arm}")))?,
                    None => bound.first.clone().ok_or_else(|| invalid(format!("arm {arm} has no candidate")))?,
                };
                evidence = ranked(&arms, evidence, bound.arm, &runner_up.iter().map(|r| i64::from(*r)).collect::<Vec<_>>())?;
                Some((*arm, bound.attempt.clone(), submission))
            }
        };
        insert_selection(tx, Decision { group, picked, kind: "operator", principal, reason, evidence, now })
    }

    /// Select by rule `first_accepted_in_launch_order.v1` (principal
    /// [`RULE_SELECTOR`]): the first arm in launch order whose verified
    /// outcome is `accepted`, with its first accepted submission, reason
    /// `first_passing_verification`; every earlier arm must be settled (not
    /// `pending`). With no accepted arm it closes the group with no selection
    /// (`none_acceptable`, or `no_candidate` if no arm submitted) only when every
    /// arm is bound and settled. Evidence ranks the accepted arms in launch
    /// order (`rank`, winner 1). The same rows always give the same answer.
    pub fn select_candidate_by_rule(&mut self, group: &str, now: i64) -> Result<CandidateSelection> {
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let arm_count = open_group(&tx, group)?;
        let arms = bound_arms(&tx, group)?;
        let winner = arms.iter().position(|a| a.outcome == "accepted");
        if let Some(pending) = arms[..winner.unwrap_or(arms.len())].iter().find(|a| a.outcome == "pending") {
            return Err(invalid(format!("arm {} is pending: the rule decides only when every earlier arm is settled", pending.arm)));
        }
        if winner.is_none() && (arms.len() as i64) < arm_count {
            return Err(invalid(format!("arm {} has not launched: the rule closes a group without a winner only when every arm is settled", arms.len() + 1)));
        }
        let mut rank = 0;
        let evidence = evidence(&arms).into_iter().zip(&arms).map(|(mut e, a)| {
            e["rank"] = if a.outcome == "accepted" { rank += 1; serde_json::json!(rank) } else { serde_json::Value::Null };
            e
        }).collect();
        let (picked, reason) = match winner {
            Some(i) => (Some((arms[i].arm as u32, arms[i].attempt.clone(), arms[i].accepted.clone().unwrap_or_default())), "first_passing_verification"),
            None if arms.iter().all(|a| a.first.is_none()) => (None, "no_candidate"),
            None => (None, "none_acceptable"),
        };
        insert_selection(tx, Decision { group, picked, kind: "rule", principal: RULE_SELECTOR, reason, evidence, now })
    }

    /// Record a judge's choice of one presented candidate (`judge_preference`,
    /// principal `judge:<judge>`). No model runs here: the judge saw each
    /// arm's first candidate in the blind [`PRESENTATION`] order, without arm,
    /// attempt or configuration, and names a submission, optionally with its
    /// runner-up candidates in order (presented submissions); evidence records
    /// every arm's `presented` position (null when not presented), `rank`, and
    /// the judge's `judge_configuration_id` (null when not given).
    pub fn select_candidate_by_judge(&mut self, group: &str, submission: &str, judge: &str, configuration: Option<&str>, runner_up: &[String], now: i64) -> Result<CandidateSelection> {
        if judge.is_empty() || judge.len() > 122 { return Err(invalid("invalid judge".into())); }
        let judge_configuration = judge_configuration(configuration)?;
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        open_group(&tx, group)?;
        let arms = bound_arms(&tx, group)?;
        let positions = presentation(group, &arms);
        let presented = |submission: &str| arms.iter().find(|a| a.first.as_deref() == Some(submission))
            .ok_or_else(|| invalid(format!("submission {submission} is not a presented candidate of {group}")));
        let chosen = presented(submission)?;
        let runner_up = runner_up.iter().map(|s| presented(s).map(|a| a.arm)).collect::<Result<Vec<_>>>()?;
        let picked = Some((chosen.arm as u32, chosen.attempt.clone(), submission.to_owned()));
        let evidence = evidence(&arms).into_iter().zip(positions).map(|(mut e, p)| { e["presented"] = serde_json::json!(p); e["judge_configuration_id"] = judge_configuration.clone(); e }).collect();
        let evidence = ranked(&arms, evidence, chosen.arm, &runner_up)?;
        let principal = format!("judge:{judge}");
        insert_selection(tx, Decision { group, picked, kind: "judge", principal: &principal, reason: "judge_preference", evidence, now })
    }
}

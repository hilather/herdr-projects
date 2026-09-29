//! Candidate groups (TM3.8, contracts-quality.md §3): `quality groups ...`.
//! `create` and `select` write canonical rows through the store's own
//! transactions (`SqliteStore`); `show` reads `state.db` and the sidecar
//! strictly read-only. Arm cost is each arm attempt's own usage from
//! `telemetry attempts`; nothing is reassigned between arms.
use anyhow::Result;
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use std::path::Path;

use crate::store::{SelectionChoice, SqliteStore};

/// Principal recorded for groups and selections made on this CLI.
const OPERATOR: &str = "operator:cli";

/// `herdr-projects telemetry <slug> quality groups ...`
#[derive(clap::Subcommand)]
pub enum Command {
    /// Seal a candidate group for a task: one arm per retained native profile,
    /// in launch order. Each later reservation of the task on the next arm's
    /// configuration becomes that arm; arms cannot change once sealed.
    Create {
        task: String,
        /// Retained native profile name, once per arm, in launch order (2-8).
        #[arg(long = "arm", required = true)]
        arms: Vec<String>,
    },
    /// Record the group's one selection: an arm's candidate, or `--none`.
    /// Selection is not verification and moves no cost.
    Select {
        group: String,
        #[arg(long, required_unless_present = "none", conflicts_with = "none")]
        arm: Option<u32>,
        /// A submission of the arm's attempt; default its first candidate.
        #[arg(long, requires = "arm")]
        submission: Option<String>,
        /// Close the group with no winner.
        #[arg(long)]
        none: bool,
        /// Reason code (contracts-quality.md §3).
        #[arg(long, default_value = "unspecified")]
        reason: String,
    },
    /// Groups with their arms, each arm's outcome and own cost, and the
    /// selection, as JSON. Read-only.
    Show,
}

pub fn run(project: &Path, command: Command) -> Result<Value> {
    let now = jiff::Timestamp::now().as_millisecond();
    let open = || SqliteStore::open(&project.join(".state/state.db"));
    Ok(match command {
        Command::Create { task, arms } => json!({"group": open()?.create_candidate_group(&task, &arms, OPERATOR, now)?}),
        Command::Select { group, arm, submission, none, reason } => {
            let choice = match arm { Some(arm) if !none => SelectionChoice::Arm { arm, submission }, _ => SelectionChoice::NoSelection };
            json!({"selection": open()?.select_candidate(&group, &choice, &reason, OPERATOR, now)?})
        }
        Command::Show => show(project)?,
    })
}

fn exists(db: &rusqlite::Connection, table: &str) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)", [table], |r| r.get(0))?)
}

/// Sum each numeric usage field over `usages`; `None` if any is not numeric.
fn sum(usages: &[&Value]) -> Option<Value> {
    let mut total = serde_json::Map::new();
    for usage in usages {
        for (key, value) in usage.as_object()? {
            let add = value.as_i64()?;
            let entry = total.entry(key.clone()).or_insert(json!(0));
            *entry = json!(entry.as_i64()?.checked_add(add)?);
        }
    }
    Some(Value::Object(total))
}

/// `(arm, configuration_id, profile_digest, bound attempt)`.
type ArmRow = (i64, String, String, Option<String>);
/// `(group_id, task_id, contract_revision, sealed_unix_ms, selection, arms)`.
type GroupRow = (String, String, Option<i64>, i64, Option<Value>, Vec<ArmRow>);

/// `{"groups": [...]}` in creation order. Each arm carries its attempt's
/// outcome and usage exactly as `telemetry attempts` reports them.
fn show(project: &Path) -> Result<Value> {
    let rows: Vec<GroupRow> = {
        let db = super::super::read_only(&project.join(".state/state.db"))?;
        crate::store::check_schema(&db)?;
        if !exists(&db, "candidate_groups")? { return Ok(json!({"groups": []})); }
        let db = db.unchecked_transaction()?;
        let groups: Vec<(String, String, Option<i64>, i64)> = db.prepare("SELECT group_id,task_id,contract_revision,sealed_unix_ms FROM candidate_groups WHERE sealed_unix_ms IS NOT NULL ORDER BY created_unix_ms,rowid")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
        let mut rows = Vec::with_capacity(groups.len());
        for (group, task, revision, sealed) in groups {
            let selection = db.query_row("SELECT outcome,arm,attempt_id,submission_id,selector_kind,selector_principal,reason,evidence,selected_unix_ms FROM candidate_selections WHERE group_id=?1", [&group],
                |r| Ok(json!({"outcome": r.get::<_, String>(0)?, "arm": r.get::<_, Option<i64>>(1)?, "attempt_id": r.get::<_, Option<String>>(2)?, "submission_id": r.get::<_, Option<String>>(3)?,
                    "selector_kind": r.get::<_, String>(4)?, "selector_principal": r.get::<_, String>(5)?, "reason": r.get::<_, String>(6)?,
                    "evidence": serde_json::from_str::<Value>(&r.get::<_, String>(7)?).unwrap_or(Value::Null), "selected_unix_ms": r.get::<_, i64>(8)?}))).optional()?;
            let arms = db.prepare("SELECT a.arm,a.configuration_id,a.profile_digest,b.attempt_id FROM candidate_group_arms a LEFT JOIN candidate_arm_attempts b ON b.group_id=a.group_id AND b.arm=a.arm WHERE a.group_id=?1 ORDER BY a.arm")?
                .query_map([&group], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
            rows.push((group, task, revision, sealed, selection, arms));
        }
        rows
    };
    let attempts = crate::telemetry::outcome::attempts(project)?;
    let outcome = |attempt: &str| attempts["attempts"].as_array().into_iter().flatten().find(|a| a["attempt_id"] == attempt).cloned();
    let mut groups = Vec::with_capacity(rows.len());
    for (group, task, revision, sealed, selection, arms) in rows {
        let winner = selection.as_ref().and_then(|s| s["arm"].as_i64());
        let mut records = Vec::with_capacity(arms.len());
        let (mut launched, mut unknown, mut premature) = (Vec::new(), Vec::new(), Vec::new());
        for (arm, configuration, profile, attempt) in arms {
            let record = attempt.as_deref().and_then(outcome);
            let role = match (winner, &selection) { (Some(w), _) if w == arm => "selected", (_, Some(_)) => "not_selected", _ => "open" };
            let entry = match (&attempt, &record) {
                (Some(attempt), Some(r)) => {
                    let candidate = r["result"]["state"] == "submitted";
                    if r["usage"]["status"] == "unavailable" { unknown.push(json!({"arm": arm, "reason": r["usage"]["reason"]})); } else { launched.push(r["usage"].clone()); }
                    if r["integration"]["state"] == "integrated" && role != "selected" { premature.push(json!({"arm": arm, "attempt_id": attempt})); }
                    json!({"arm": arm, "configuration_id": configuration, "profile_digest": profile, "attempt_id": attempt, "role": role,
                        "candidate": if candidate { r["result"]["submission_id"].clone() } else { Value::Null },
                        // Contracts §3: an arm without a candidate counts as a failure, not missing data.
                        "outcome": if candidate { "candidate" } else if r["terminal_state"] == "open" { "running" } else { "failure_no_candidate" },
                        "terminal_state": r["terminal_state"], "verification": r["verification"], "integration": r["integration"], "accepted": r["accepted"], "usage": r["usage"]})
                }
                _ => json!({"arm": arm, "configuration_id": configuration, "profile_digest": profile, "attempt_id": attempt, "role": role, "candidate": null,
                    "outcome": if selection.is_some() { "failure_no_candidate" } else { "not_launched" }, "usage": null}),
            };
            records.push(entry);
        }
        let not_launched = records.iter().filter(|r| r["attempt_id"].is_null()).count();
        let arms_total = if unknown.is_empty() { sum(&launched.iter().collect::<Vec<_>>()).unwrap_or_else(|| json!({"status": "unavailable", "reason": "usage_not_numeric"})) }
            else { json!({"status": "unavailable", "reason": "arm_usage_unavailable", "arms": unknown}) };
        let winner_usage = winner.and_then(|w| records.iter().find(|r| r["arm"] == w)).map_or(Value::Null, |r| r["usage"].clone());
        groups.push(json!({"group_id": group, "task_id": task, "contract_revision": revision, "sealed_unix_ms": sealed,
            "status": if selection.is_some() { "closed" } else { "open" }, "arms": records, "selection": selection,
            // Every launched arm's own usage; the winner's is a drill-down, never the group's cost.
            "cost": {"arms_total": arms_total, "arms_launched": launched.len() + unknown.len(), "arms_not_launched": not_launched, "winner_usage": winner_usage},
            // C3 records groups only; integration is not held (contracts-quality.md §3).
            "integration_hold": {"enforced": false, "integrated_without_selection": premature}}));
    }
    Ok(json!({"groups": groups}))
}

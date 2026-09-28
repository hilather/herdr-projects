//! Contracts §4 AttemptOutcome: a read-only projection, one record per canonical
//! attempt (one with sealed `attempt_inputs`), joining the dispatch decision,
//! lifecycle marks, result, verification and integration. Missing values are
//! `unavailable` or `censored` with a reason, never 0.
use crate::store::{SqliteStore, StoreError};
use rusqlite::{Connection, OptionalExtension};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path};

const TERMINAL: [&str; 4] = ["completed", "failed", "cancelled", "lost"];

fn status(status: &str, reason: &str) -> Value {
    json!({"reason": reason, "status": status})
}

/// `{"attempts": [...]}` in reservation order.
pub fn attempts(project: &Path) -> Result<Value, StoreError> {
    let mut store = SqliteStore::open(&project.join(".state/state.db"))?;
    let home = std::env::var("HOME").ok();
    let records = store.telemetry_read(|db| {
        let version: u32 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        let rows = db.prepare("SELECT i.attempt_id,a.task_id,a.state,json_extract(i.payload,'$.inputs.effective_profile.kind') FROM attempt_inputs i JOIN attempts a ON a.id=i.attempt_id ORDER BY i.rowid")?
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<String>>(3)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(|(attempt, task, state, kind)| record(db, version, &attempt, &task, &state, kind.as_deref(), home.as_deref())).collect::<rusqlite::Result<Vec<_>>>()
    })?;
    Ok(json!({"attempts": records}))
}

fn record(db: &Connection, version: u32, attempt: &str, task: &str, state: &str, kind: Option<&str>, home: Option<&str>) -> rusqlite::Result<Value> {
    let predates_decision = status("unavailable", "predates_dispatch_log");
    let decision = if version >= 50 {
        db.query_row("SELECT d.chosen_configuration_id,d.classification_id,c.class,c.band,d.contract_revision FROM dispatch_decisions d
            LEFT JOIN task_classifications c ON c.classification_id=d.classification_id WHERE d.attempt_id=?1", [attempt],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, Option<String>>(3)?, r.get::<_, Option<i64>>(4)?))).optional()?
    } else { None };
    let (configuration, classification, decided_revision) = match decision {
        Some((configuration, Some(id), class, band, revision)) => (json!(configuration), json!({"band": band, "class": class, "classification_id": id}), revision),
        Some((configuration, None, _, _, revision)) => (json!(configuration), status("unavailable", "no_classification"), revision),
        None => (predates_decision.clone(), predates_decision, None),
    };
    let marks: BTreeMap<String, i64> = if version >= 51 {
        db.prepare("SELECT state,unix_ms FROM attempt_lifecycle WHERE attempt_id=?1")?.query_map([attempt], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
    } else { BTreeMap::new() };
    // Every attempt reserved at or after 0051 has a `reserved` mark; without it
    // an absent mark may have happened before the log existed.
    let predates = !marks.contains_key("reserved");
    let terminal = TERMINAL.contains(&state);
    let terminal_ms = TERMINAL.iter().find_map(|s| marks.get(*s).copied());
    let at = |mark: Option<i64>, happened: bool| match mark {
        Some(ms) => json!(ms),
        None if predates && happened => status("unavailable", "predates_lifecycle_log"),
        None => Value::Null,
    };
    let (reserved, launching, running) = (marks.get("reserved").copied(), marks.get("launching").copied(), marks.get("running").copied());
    let active = match (running, terminal_ms) {
        (Some(start), Some(end)) => json!(end - start),
        (Some(_), None) if !terminal => status("censored", "open"),
        _ if predates => status("unavailable", "predates_lifecycle_log"),
        _ => status("unavailable", "not_running"),
    };
    let queue = match (reserved, launching) {
        (Some(start), Some(end)) => json!(end - start),
        _ if predates => status("unavailable", "predates_lifecycle_log"),
        _ => status("censored", if terminal { state } else { "open" }),
    };
    let submission = db.query_row("SELECT submission_id,candidate_oid,created_unix_ms,contract_revision FROM result_submissions WHERE attempt_id=?1 ORDER BY created_unix_ms,rowid LIMIT 1",
        [attempt], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?))).optional()?;
    let revision = submission.as_ref().map(|s| s.3).or(decided_revision);
    let route: Option<String> = db.query_row("SELECT route FROM task_contracts WHERE task_id=?1 AND contract_revision=?2", rusqlite::params![task, revision], |r| r.get(0)).optional()?;
    let integrates = route.as_deref() == Some("verify_then_integrate");
    let (result, verification, integration) = match &submission {
        None => (json!({"state": "not_submitted"}), json!({"state": "not_submitted"}), json!({"state": if integrates { "not_submitted" } else { "not_applicable" }})),
        Some((id, candidate, created, _)) => {
            let run: Option<(String, Option<String>)> = db.query_row("SELECT state,reason FROM verification_runs WHERE submission_id=?1 ORDER BY created_unix_ms DESC,rowid DESC LIMIT 1",
                [id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            let verification = match run {
                None => json!({"state": "pending"}),
                Some((state, None)) => json!({"state": state}),
                Some((state, Some(reason))) => json!({"reason": crate::domain::excerpt(&reason, home), "state": state}),
            };
            let integration = if !integrates { "not_applicable".to_owned() } else {
                db.query_row("SELECT i.state,EXISTS(SELECT 1 FROM integrated_commits c WHERE c.operation_id=i.operation_id) FROM integration_operations i
                    JOIN verified_results r ON r.result_id=i.verified_result_id WHERE r.submission_id=?1 ORDER BY i.created_unix_ms DESC,i.generation DESC LIMIT 1",
                    [id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?))).optional()?
                    .map(|(state, committed)| if committed { "integrated".to_owned() } else if state == "integrated" { "integrated_unconfirmed".to_owned() } else { state })
                    .unwrap_or_else(|| "pending".to_owned())
            };
            (json!({"candidate_oid": candidate, "created_unix_ms": created, "state": "submitted", "submission_id": id}), verification, json!({"state": integration}))
        }
    };
    // Contracts §6 `A`: evidence from any of this attempt's submissions for the current contract revision.
    let accepted: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM task_contracts t JOIN result_submissions s ON s.task_id=t.task_id AND s.contract_revision=t.contract_revision
        JOIN verified_results r ON r.submission_id=s.submission_id WHERE s.attempt_id=?1 AND t.task_id=?2
        AND t.contract_revision=(SELECT max(contract_revision) FROM task_contracts WHERE task_id=?2)
        AND (t.route='verify_only' OR EXISTS(SELECT 1 FROM integration_operations i JOIN integrated_commits c ON c.operation_id=i.operation_id WHERE i.verified_result_id=r.result_id)))",
        [attempt, task], |r| r.get(0))?;
    let usage = status("unavailable", if kind == Some("codex") { "collection_not_run" } else { "adapter_absent" });
    Ok(json!({
        "accepted": accepted, "active_ms": active, "attempt_id": attempt,
        "attention": status("unavailable", "attention_not_collected"),
        "classification": classification, "configuration_id": configuration, "integration": integration,
        "launching_unix_ms": at(launching, true), "queue_to_launch_ms": queue, "reserved_unix_ms": at(reserved, true),
        "result": result, "running_unix_ms": at(running, true), "task_id": task,
        "terminal_state": if terminal { state } else { "open" }, "terminal_unix_ms": at(terminal_ms, terminal),
        "usage": usage, "verification": verification,
    }))
}

/// One line per attempt for the terminal.
pub fn text(report: &Value) -> String {
    let show = |v: &Value| match v {
        Value::Object(map) => format!("{}:{}", map.get("status").or(map.get("state")).and_then(Value::as_str).unwrap_or("?"), map.get("reason").and_then(Value::as_str).unwrap_or("")),
        other => other.to_string(),
    };
    report["attempts"].as_array().into_iter().flatten().map(|a| format!("{} task={} state={} active_ms={} result={} verification={} integration={} accepted={} usage={}\n",
        a["attempt_id"].as_str().unwrap_or(""), a["task_id"].as_str().unwrap_or(""), a["terminal_state"].as_str().unwrap_or(""), show(&a["active_ms"]),
        show(&a["result"]), show(&a["verification"]), show(&a["integration"]), a["accepted"], show(&a["usage"]))).collect()
}

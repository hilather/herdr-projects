//! Contracts §4 AttemptOutcome: a read-only projection, one record per canonical
//! attempt (one with sealed `attempt_inputs`), joining the dispatch decision,
//! lifecycle marks, result, verification and integration. Missing values are
//! `unavailable` or `censored` with a reason, never 0.
use rusqlite::{Connection, OptionalExtension};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path};

const TERMINAL: [&str; 4] = ["completed", "failed", "cancelled", "lost"];

fn status(status: &str, reason: &str) -> Value {
    json!({"reason": reason, "status": status})
}

/// `{"attempts": [...]}` in reservation order.
/// Opened strictly read-only (contracts §0 "Reads"), in one read transaction.
pub fn attempts(project: &Path) -> anyhow::Result<Value> {
    let store = super::read_only(&project.join(".state/state.db"))?;
    crate::store::check_schema(&store)?;
    let home = std::env::var("HOME").ok();
    let records = {
        let db = store.unchecked_transaction()?;
        let version: u32 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        let rows = db.prepare("SELECT i.attempt_id,a.task_id,a.state,json_extract(i.payload,'$.inputs.effective_profile.kind') FROM attempt_inputs i JOIN attempts a ON a.id=i.attempt_id ORDER BY i.rowid")?
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<String>>(3)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(|(attempt, task, state, kind)| record(&db, version, &attempt, &task, &state, kind.as_deref(), home.as_deref())).collect::<rusqlite::Result<Vec<_>>>()?
    };
    // Contracts §4/§5 `usage`: the sidecar's bound, certified sums or its reason.
    let mut records = records;
    if let Some(sidecar) = super::sidecar::read(project)? {
        for record in records.iter_mut().filter(|r| r["usage"]["reason"] == "collection_not_run") {
            record["usage"] = super::sidecar::attempt_usage(&sidecar, record["attempt_id"].as_str().unwrap_or_default())?;
        }
        attention(project, &sidecar, &mut records)?;
    }
    Ok(json!({"attempts": records}))
}

/// Contracts §4 `attention`: once any attention sample exists, each launched
/// attempt's summary of `accounting attention` (contracts-accounting §6), or
/// `not_observed`; an attempt without a launch receipt is `not_launched`.
/// Before any sample every record keeps `attention_not_collected`.
fn attention(project: &Path, sidecar: &Connection, records: &mut [Value]) -> anyhow::Result<()> {
    let report = super::accounting::attention::read(project, sidecar)?;
    // Each attempt carries its own certification (live for Codex, fixture for other kinds).
    let launched: BTreeMap<&str, (&Value, &Value)> = report["attempts"].as_array().into_iter().flatten()
        .filter_map(|a| Some((a["attempt_id"].as_str()?, (&a["attention"], &a["certified"])))).collect();
    if launched.is_empty() { return Ok(()); }
    let signal = &report["signal"];
    for record in records.iter_mut() {
        let Some(&(a, certified)) = record["attempt_id"].as_str().and_then(|id| launched.get(id)) else {
            record["attention"] = status("unavailable", "not_launched");
            continue;
        };
        let mut gaps = BTreeMap::<&str, i64>::new();
        for gap in a["gaps"].as_array().into_iter().flatten() { *gaps.entry(gap["reason"].as_str().unwrap_or("unknown")).or_default() += 1; }
        let mut summary = if a["status"] == "unavailable" { a.clone() } else {
            let intervals = a["intervals"].as_array().map_or(0, Vec::len);
            let censored = a["intervals"].as_array().into_iter().flatten().filter(|i| i["duration_ms"].is_null()).count();
            json!({"interventions": a["interventions"], "uncertain_starts": a["uncertain_starts"], "waiting_ms": a["waiting_ms"],
                "observed_ms": a["observed_ms"], "intervals": intervals, "censored_intervals": censored, "reason_type": signal["reason_type"]})
        };
        summary["gaps"] = json!(gaps);
        summary["basis"] = certified.clone();
        summary["source"] = signal["source"].clone();
        record["attention"] = summary;
    }
    Ok(())
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
            let verification = verification(db, id, home)?;
            let integration = if !integrates { json!({"state": "not_applicable"}) } else {
                db.query_row("SELECT i.state,EXISTS(SELECT 1 FROM integrated_commits c WHERE c.operation_id=i.operation_id) FROM integration_operations i
                    JOIN verified_results r ON r.result_id=i.verified_result_id WHERE r.submission_id=?1 ORDER BY i.created_unix_ms DESC,i.generation DESC LIMIT 1",
                    [id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?))).optional()?
                    .map(|(state, committed)| json!({"state": if committed { "integrated" } else if state == "integrated" { "integrated_unconfirmed" } else { &state }}))
                    // Without an operation a rejected submission is not eligible (every policy needs an accepted run).
                    .unwrap_or_else(|| if verification["state"] == "rejected" { json!({"reason": "verification_rejected", "state": "not_applicable"}) } else { json!({"state": "pending"}) })
            };
            (json!({"candidate_oid": candidate, "created_unix_ms": created, "state": "submitted", "submission_id": id}), verification, integration)
        }
    };
    // Contracts §6 `A`: evidence from any of this attempt's submissions for the current contract revision.
    let accepted: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM task_contracts t JOIN result_submissions s ON s.task_id=t.task_id AND s.contract_revision=t.contract_revision
        JOIN verified_results r ON r.submission_id=s.submission_id WHERE s.attempt_id=?1 AND t.task_id=?2
        AND t.contract_revision=(SELECT max(contract_revision) FROM task_contracts WHERE task_id=?2)
        AND (t.route='verify_only' OR EXISTS(SELECT 1 FROM integration_operations i JOIN integrated_commits c ON c.operation_id=i.operation_id WHERE i.verified_result_id=r.result_id)))",
        [attempt, task], |r| r.get(0))?;
    // Without a sidecar; `attempts` replaces it with the sidecar's answer.
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

/// Contracts §4 `verification` for one submission, combined over its
/// acceptance policies (and any other policy that has a run): each policy's
/// latest run decides it, `rejected` if any policy's does, `accepted` only
/// when every policy's does, else `pending`. `policies` gives each one.
fn verification(db: &Connection, submission: &str, home: Option<&str>) -> rusqlite::Result<Value> {
    let runs = db.prepare("SELECT p.policy_id,(SELECT state FROM verification_runs v WHERE v.submission_id=?1 AND v.policy_id=p.policy_id ORDER BY created_unix_ms DESC,rowid DESC LIMIT 1),
        (SELECT reason FROM verification_runs v WHERE v.submission_id=?1 AND v.policy_id=p.policy_id ORDER BY created_unix_ms DESC,rowid DESC LIMIT 1)
        FROM (SELECT a.policy_id FROM acceptance_policies a JOIN result_submissions s ON s.task_id=a.task_id AND s.contract_revision=a.contract_revision WHERE s.submission_id=?1
            UNION SELECT policy_id FROM verification_runs WHERE submission_id=?1) p ORDER BY p.policy_id")?
        .query_map([submission], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let policies: Vec<Value> = runs.iter().map(|(policy, state, reason)| {
        let mut entry = json!({"policy_id": policy, "state": state.as_deref().unwrap_or("pending")});
        if let Some(reason) = reason { entry["reason"] = json!(crate::domain::excerpt(reason, home)); }
        entry
    }).collect();
    let rejected = policies.iter().find(|p| p["state"] == "rejected");
    let accepted = !policies.is_empty() && policies.iter().all(|p| p["state"] == "accepted");
    let mut combined = json!({"state": if rejected.is_some() { "rejected" } else if accepted { "accepted" } else { "pending" }});
    if let Some(reason) = rejected.and_then(|p| p.get("reason")) { combined["reason"] = reason.clone(); }
    if !policies.is_empty() { combined["policies"] = json!(policies); }
    Ok(combined)
}

/// One line per attempt for the terminal. `active_ms` prints as `wall_ms`:
/// it is wall time from the running mark to the terminal mark, idle included.
pub fn text(report: &Value) -> String {
    let show = |v: &Value| match v {
        Value::Object(map) if map.contains_key("total_tokens") => format!("in={} out={} total={}", map["input_tokens"], map["output_tokens"], map["total_tokens"]),
        Value::Object(map) => {
            let state = map.get("status").or(map.get("state")).and_then(Value::as_str).unwrap_or("?");
            map.get("reason").and_then(Value::as_str).map_or_else(|| state.to_owned(), |reason| format!("{state}:{reason}"))
        }
        other => other.to_string(),
    };
    let attention = |v: &Value| if v.get("waiting_ms").is_some() {
        format!("waits={} waiting_ms={} censored={} gaps={}", v["interventions"], v["waiting_ms"], v["censored_intervals"],
            v["gaps"].as_object().map_or(0, |g| g.values().filter_map(Value::as_i64).sum::<i64>()))
    } else { show(v) };
    report["attempts"].as_array().into_iter().flatten().map(|a| format!("{} task={} state={} wall_ms={} result={} verification={} integration={} accepted={} attention={} usage={}\n",
        a["attempt_id"].as_str().unwrap_or(""), a["task_id"].as_str().unwrap_or(""), a["terminal_state"].as_str().unwrap_or(""), show(&a["active_ms"]),
        show(&a["result"]), show(&a["verification"]), show(&a["integration"]), a["accepted"], attention(&a["attention"]), show(&a["usage"]))).collect()
}

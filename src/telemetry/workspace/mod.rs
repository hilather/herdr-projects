//! TM4.8 Herdr workspace surfaces (plan doc 15; contract
//! docs/telemetry/workspace.md): the fleet snapshot behind the fleet pane and
//! popup, the refreshing `telemetry <slug> watch` pane, the coordinator digest
//! section, the `doctor` telemetry checks, and the owner actions the plugin
//! popups route to.
//!
//! One snapshot (`telemetry-workspace.v1`) feeds every surface, so they show
//! the same values for the same read. Every value is copied from an existing
//! read path, never recomputed: the TM4.1 query service (services, replay,
//! coverage), the TM1.8 attempt projection (`telemetry attempts`: active
//! attempts, attention, usage), the TM4.4 comparison (`telemetry compare`)
//! the TM3.8 candidate groups (`quality groups show`) and the TM4.5 health
//! alerts (`health alerts`, recorded open alerts). When the query
//! service or the attempt projection cannot answer, the whole snapshot is
//! `unavailable` and no surface shows a number.
//!
//! Read-only and advisory: nothing here writes, launches, selects, changes a
//! budget, accepts a finding or writes project memory. The owner actions
//! (`owner.rs`) only parse and run the existing owner commands, which keep
//! their own authority checks (the worker-context refusal and the store's
//! principal checks).
use super::analytics::{compare, query};
use anyhow::Result;
use serde_json::{Value, json};
use std::path::Path;

pub mod owner;
mod render;

pub use render::{digest_section, text};

pub const CONTRACT: &str = "telemetry-workspace.v1";
pub const SCHEMA_VERSION: u32 = 1;
/// Metrics the snapshot reads from the query service (one request).
pub const METRICS: [&str; 5] = ["M13", "M38", "M39", "M40", "M49"];
/// The coordinator digest section's bounds (doc 15 §5: at most 40 lines).
pub const DIGEST_MAX_LINES: usize = 40;
pub const DIGEST_MAX_BYTES: usize = 4096;
/// Refresh interval bounds of `telemetry <slug> watch`, seconds.
pub const WATCH_DEFAULT_SECS: u64 = 5;
pub const WATCH_MAX_SECS: u64 = 300;

/// `herdr-projects telemetry <slug> workspace ...`
#[derive(clap::Subcommand, Clone, Debug)]
pub enum Command {
    /// The fleet snapshot every workspace surface renders (the pane body; `--json`: `telemetry-workspace.v1`). Read-only.
    Show { #[arg(long)] json: bool },
    /// The bounded coordinator digest section, exactly as `context` appends it. Read-only.
    Digest,
}

/// `herdr-projects telemetry <slug> watch`: the fleet pane, refreshed.
#[derive(clap::Args, Clone, Debug)]
pub struct WatchArgs {
    /// Seconds between refreshes (1-300).
    #[arg(long, default_value_t = WATCH_DEFAULT_SECS, value_parser = clap::value_parser!(u64).range(1..=WATCH_MAX_SECS))]
    pub interval_secs: u64,
    /// Stop after this many renders (default: until the pane is closed).
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    pub iterations: Option<u64>,
}

fn unavailable(reason: &str) -> Value { json!({"status": "unavailable", "reason": reason}) }

/// A whole-snapshot outage: said once, with no number anywhere.
fn down(slug: &str, reason: &str, error: &anyhow::Error) -> Value {
    let error: String = format!("{error:#}").chars().take(200).collect();
    json!({"schema_version": SCHEMA_VERSION, "contract": CONTRACT, "project": slug, "status": "unavailable", "reason": reason, "error": error})
}

/// The query service's own fields for one metric, verbatim.
fn metric_fields(result: &Value) -> Value {
    json!({"metric_id": result["metric_id"], "name": result["name"], "definition": result["definition"], "status": result["status"],
        "value": result["value"], "reason": result["reason"], "numerator": result["numerator"], "denominator": result["denominator"],
        "coverage": result["coverage"]["state"], "lag_ms": result["lag_ms"], "lag_reason": result["lag_reason"]})
}

fn result<'a>(out: &'a Value, metric: &str) -> Option<&'a Value> {
    out["results"].as_array()?.iter().find(|r| r["metric_id"] == metric)
}

/// `"<kind> <agent_version>"` per configuration, as `telemetry compare` labels arms.
fn labels(project: &Path) -> Result<std::collections::BTreeMap<String, String>> {
    let db = super::read_only(&project.join(".state/state.db"))?;
    let exists: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='agent_configurations')", [], |r| r.get(0))?;
    if !exists { return Ok(Default::default()); }
    let rows = db.prepare("SELECT configuration_id,coalesce(json_extract(canonical_json,'$.kind'),'unknown')||' '||coalesce(json_extract(canonical_json,'$.agent_version'),'unknown') FROM agent_configurations")?
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// The fleet snapshot of one project (`telemetry-workspace.v1`).
pub fn snapshot(project: &Path, slug: &str) -> Value {
    let args = query::Args { metrics: METRICS.iter().map(|m| (*m).to_owned()).collect(), cohort: None, from: None, to: None, as_of: None, as_of_seq: None,
        by: None, horizon_ms: None, drill: None, page_size: query::DEFAULT_PAGE, cursor: None, json: true };
    let answer = match query::request(&args).and_then(|request| query::run(project, &request)) {
        Ok(answer) => answer,
        Err(error) => return down(slug, "query_service_down", &error),
    };
    let attempts = match super::outcome::attempts(project) {
        Ok(attempts) => attempts,
        Err(error) => return down(slug, "attempt_projection_down", &error),
    };
    let labels = labels(project).unwrap_or_default();
    let label = |id: &Value| id.as_str().map(|id| labels.get(id).cloned().unwrap_or_else(|| "unknown".into()));
    let now = answer["query_unix_ms"].as_i64().unwrap_or_default();

    // Candidate groups (TM3.8), tagged race#1.. in creation order; a group
    // whose every arm finished is awaiting the owner's selection.
    let groups = super::quality::run(project, owner::groups_show())
        .and_then(|text| Ok(serde_json::from_str::<Value>(&text)?));
    let groups: Value = match groups {
        Ok(out) => Value::Array(out["groups"].as_array().into_iter().flatten().enumerate().map(|(i, g)| {
            let arms: Vec<Value> = g["arms"].as_array().into_iter().flatten().map(|a| json!({"arm": a["arm"], "configuration_id": a["configuration_id"],
                "label": label(&a["configuration_id"]), "attempt_id": a["attempt_id"], "outcome": a["outcome"], "role": a["role"]})).collect();
            let finished = arms.iter().all(|a| matches!(a["outcome"].as_str(), Some("candidate" | "failure_no_candidate")));
            json!({"tag": format!("race#{}", i + 1), "group_id": g["group_id"], "task_id": g["task_id"], "status": g["status"], "arms": arms,
                "awaiting_selection": g["status"] == "open" && finished})
        }).collect()),
        Err(_) => unavailable("candidate_groups_unreadable"),
    };
    let arm_of = |attempt: &str| groups.as_array().into_iter().flatten().find_map(|g| g["arms"].as_array()?.iter()
        .find(|a| a["attempt_id"] == attempt).map(|a| json!({"tag": g["tag"], "arm": a["arm"]})));

    // The open (censored) wait of each attempt, from the attention lane read
    // the projection summarizes (`accounting attention`): since when, and how
    // long it was observed so far. Never extrapolated to now.
    let open_waits: std::collections::BTreeMap<String, (Value, i64)> = super::sidecar::read(project).ok().flatten()
        .and_then(|db| super::accounting::attention::read(project, &db).ok())
        .map(|lane| lane["attempts"].as_array().into_iter().flatten().filter_map(|a| {
            if a["state"] != "open" { return None; }
            let open = a["attention"]["intervals"].as_array()?.iter().find(|i| i["end"] == "open_at_horizon")?;
            let (opened, last) = (open["opened_unix_ms"].as_i64()?, open["last_observed_unix_ms"].as_i64()?);
            Some((a["attempt_id"].as_str()?.to_owned(), (json!(opened), last - opened)))
        }).collect()).unwrap_or_default();

    // Active attempts (TM1.8 projection): every open attempt.
    let active: Vec<Value> = attempts["attempts"].as_array().into_iter().flatten().filter(|a| a["terminal_state"] == "open").map(|a| {
        let id = a["attempt_id"].as_str().unwrap_or_default();
        let state = if a["running_unix_ms"].is_i64() { "running" } else if a["launching_unix_ms"].is_i64() { "launching" } else { "reserved" };
        let waiting = match (a["attention"].get("waiting_ms"), open_waits.get(id)) {
            (Some(ms), Some((since, observed))) => json!({"waiting_ms": ms, "open": true, "open_since_unix_ms": since, "open_observed_ms": observed}),
            (Some(ms), None) => json!({"waiting_ms": ms, "open": false}),
            (None, _) => a["attention"].clone(),
        };
        let coverage = if a["usage"].get("total_tokens").is_some() { "complete" } else { "unavailable" };
        json!({"attempt_id": id, "task_id": a["task_id"], "state": state, "reserved_unix_ms": a["reserved_unix_ms"],
            "elapsed_ms": a["reserved_unix_ms"].as_i64().map(|at| (now - at).max(0)),
            "configuration_id": if a["configuration_id"].is_string() { a["configuration_id"].clone() } else { Value::Null },
            "configuration_label": label(&a["configuration_id"]).map_or_else(|| a["configuration_id"]["reason"].as_str().map(|r| format!("n/a ({r})")).into(), Value::from),
            "group": arm_of(id), "waiting": waiting, "usage": a["usage"], "coverage": coverage})
    }).collect();

    // Services: M38/M39 as served, M40 at the latest decision per service.
    let m40 = result(&answer, "M40");
    let mut quota: Vec<Value> = Vec::new();
    for decision in m40.and_then(|r| r["detail"]["decisions"].as_array()).into_iter().flatten() {
        let service = &decision["service"];
        match quota.iter_mut().find(|q| &q["service"] == service) {
            Some(q) if q["decided_unix_ms"].as_i64() >= decision["decided_unix_ms"].as_i64() => {}
            Some(q) => *q = decision.clone(),
            None => quota.push(decision.clone()),
        }
    }
    let services = json!({"M38": result(&answer, "M38").map(metric_fields), "M39": result(&answer, "M39").map(metric_fields),
        "M40": m40.map(metric_fields), "quota_at_last_dispatch": quota});

    // Configuration comparison (TM4.4): M02 per task class, suppression and intervals as computed there.
    let compared = compare::run(project, &compare::Args { metrics: vec!["M02".into()], by: "configuration".into(), cohort: None, from: None, to: None,
        horizon_ms: None, task_class: None, seed: None, json: true });
    let configurations = match compared {
        Ok(report) => {
            let arm_label = |id: &Value| report["configurations"].as_array().into_iter().flatten().find(|c| &c["configuration_id"] == id).map(|c| c["label"].clone())
                .unwrap_or(Value::Null);
            let cells: Vec<Value> = report["results"][0]["cells"].as_array().into_iter().flatten().map(|c| json!({"task_class": c["task_class"],
                "ranking": c["ranking"]["status"], "arms": c["arms"].as_array().into_iter().flatten().map(|a| json!({"configuration_id": a["configuration_id"],
                    "label": arm_label(&a["configuration_id"]), "tasks": a["tasks"], "status": a["status"], "value": a["value"], "decimal": a["decimal"],
                    "interval": a["interval"], "pooled": a["pooled"], "min_sample": a["min_sample"]})).collect::<Vec<_>>()})).collect();
            json!({"metric": "M02", "definition": report["results"][0]["definition"], "cohort": report["request"]["cohort"],
                "analysis": report["analysis"]["kind"], "level": report["estimators"]["uncertainty"]["level"], "min_tasks": report["estimators"]["min_sample"]["value"],
                "unallocated": report["population"]["unallocated"], "cells": cells})
        }
        Err(error) => { let mut v = unavailable("comparison_unavailable"); v["error"] = json!(format!("{error:#}").chars().take(200).collect::<String>()); v }
    };

    // TM4.5 health alerts as recorded (`telemetry <slug> health alerts`): the
    // inbox notices are that lane's own `health notify`.
    let alerts = match super::health::store::alerts(project, None) {
        Ok(out) if out["status"] == "unavailable" => out,
        Ok(out) => json!({"status": "available", "last_evaluated_unix_ms": out["last_evaluated_unix_ms"],
            "open": out["open"].as_array().into_iter().flatten().map(|a| json!({"alert_id": a["alert_id"], "rule": a["rule"], "labels": a["labels"],
                "state": a["state"], "reasons": a["reasons"].as_array().into_iter().flatten().filter_map(|r| r["code"].as_str()).collect::<Vec<_>>(),
                "opened_unix_ms": a["opened_unix_ms"], "occurrences": a["occurrences"], "notice_id": a["notice_id"]})).collect::<Vec<_>>()}),
        Err(_) => unavailable("health_alerts_unreadable"),
    };

    let mut needs: Vec<Value> = active.iter().filter(|a| a["waiting"]["open"] == true)
        .map(|a| json!({"kind": "waiting_on_you", "attempt_id": a["attempt_id"], "task_id": a["task_id"], "since_unix_ms": a["waiting"]["open_since_unix_ms"],
            "observed_ms": a["waiting"]["open_observed_ms"]})).collect();
    needs.sort_by_key(|n| std::cmp::Reverse(n["observed_ms"].as_i64().unwrap_or(0)));
    let rank = |state: &Value| match state.as_str() { Some("critical") => 0, Some("warn") => 1, _ => 2 };
    let mut open: Vec<&Value> = alerts["open"].as_array().into_iter().flatten().collect();
    open.sort_by_key(|a| (rank(&a["state"]), a["alert_id"].as_i64()));
    let mut needs: Vec<Value> = open.into_iter().map(|a| json!({"kind": "alert", "alert_id": a["alert_id"], "rule": a["rule"], "state": a["state"],
        "labels": a["labels"], "reasons": a["reasons"]})).chain(needs).collect();
    needs.extend(groups.as_array().into_iter().flatten().filter(|g| g["awaiting_selection"] == true)
        .map(|g| json!({"kind": "selection_pending", "group": g["tag"], "group_id": g["group_id"], "task_id": g["task_id"]})));

    json!({"schema_version": SCHEMA_VERSION, "contract": CONTRACT, "project": slug, "status": "available",
        "query_unix_ms": answer["query_unix_ms"], "advisory": {"authority": "none", "writes": "none"},
        "coverage": result(&answer, "M13").map(metric_fields),
        "needs_you": needs, "active": {"count": active.len(), "attempts": active}, "services": services,
        "configurations": configurations, "candidate_groups": groups,
        "replay": result(&answer, "M49").map(metric_fields), "alerts": alerts})
}

/// `telemetry <slug> workspace ...`: the switch, the project's own scope, one read.
pub fn run(root: &Path, slug: &str, config_dir: &Path, command: &Command) -> Result<String> {
    if !super::views::enabled(config_dir)? { anyhow::bail!("{}", super::views::disabled_message(config_dir)); }
    let scope = super::views::scope(root, slug)?;
    let snapshot = snapshot(&scope.dir, slug);
    Ok(match command {
        Command::Show { json: true } => serde_json::to_string_pretty(&snapshot)? + "\n",
        Command::Show { json: false } => text(&snapshot),
        Command::Digest => digest_section(&snapshot),
    })
}

/// The section `context` appends to the coordinator digest: none without a
/// canonical store or with the views switched off, else the bounded section.
pub fn context_section(project: &Path, slug: &str, config_dir: &Path) -> Option<String> {
    if !project.join(".state/state.db").is_file() || !super::views::enabled(config_dir).unwrap_or(false) { return None; }
    let scope = super::views::scope(project.parent()?, slug).ok()?;
    Some(digest_section(&snapshot(&scope.dir, slug)))
}

/// `telemetry <slug> watch`: render, sleep, repeat. On a terminal each render
/// replaces the last; piped, renders follow each other.
pub fn watch(root: &Path, slug: &str, config_dir: &Path, args: &WatchArgs) -> Result<()> {
    use std::io::{IsTerminal, Write};
    let mut rendered = 0u64;
    loop {
        let body = match super::views::enabled(config_dir) {
            Ok(false) => super::views::disabled_message(config_dir) + "\n",
            Ok(true) => match super::views::scope(root, slug) {
                Ok(scope) => text(&snapshot(&scope.dir, slug)),
                Err(error) => format!("error: {error:#}\n"),
            },
            Err(error) => format!("error: {error:#}\n"),
        };
        let mut out = std::io::stdout().lock();
        if out.is_terminal() { write!(out, "\x1b[H\x1b[2J")?; }
        write!(out, "{body}")?;
        writeln!(out, "(refreshes every {}s; read-only)", args.interval_secs)?;
        out.flush()?;
        drop(out);
        rendered += 1;
        if args.iterations.is_some_and(|n| rendered >= n) { return Ok(()); }
        std::thread::sleep(std::time::Duration::from_secs(args.interval_secs));
    }
}

/// `doctor` telemetry checks for one project with a canonical store (doc 15
/// §9): `(Some(true) ok | None degraded or failed, line)`. Advisory: none
/// fails `doctor`, each says what to run.
pub fn doctor_checks(project: &Path, slug: &str, config_dir: &Path) -> Vec<(Option<bool>, String)> {
    let mut out = Vec::new();
    if !project.join(".state/state.db").is_file() { return out; }
    match super::views::enabled(config_dir) {
        Ok(true) => {}
        Ok(false) => { out.push((None, "telemetry views: disabled ([telemetry] views = false); the fleet pane and digest section are off".into())); return out; }
        Err(error) => { out.push((None, format!("telemetry views: {error:#}"))); return out; }
    }
    let started = std::time::Instant::now();
    let snapshot = snapshot(project, slug);
    let elapsed = started.elapsed().as_millis();
    if snapshot["status"] != "available" {
        out.push((None, format!("telemetry query service: failed ({}); every workspace surface shows unavailable; run `telemetry {slug} query --metric M13` for the error",
            snapshot["reason"].as_str().unwrap_or("unknown"))));
        return out;
    }
    out.push((Some(true), format!("telemetry query service: ok (fleet snapshot in {elapsed} ms)")));
    let lag = &snapshot["coverage"]["lag_ms"];
    match lag.as_i64() {
        Some(ms) if ms <= 15 * 60_000 => out.push((Some(true), format!("telemetry ingestion lag: ok ({}s since the last collect)", ms / 1000))),
        Some(ms) => out.push((None, format!("telemetry ingestion lag: degraded ({}s since the last collect); run `telemetry {slug} collect`", ms / 1000))),
        None => out.push((None, format!("telemetry ingestion lag: degraded (n/a: {}); run `telemetry {slug} collect`", snapshot["coverage"]["lag_reason"].as_str().unwrap_or("unknown")))),
    }
    let active = snapshot["active"]["attempts"].as_array().cloned().unwrap_or_default();
    let uncovered: Vec<&Value> = active.iter().filter(|a| a["coverage"] != "complete").collect();
    if uncovered.is_empty() {
        out.push((Some(true), format!("telemetry collector coverage: ok ({} open attempt(s), all with bound usage)", active.len())));
    } else {
        let mut reasons: Vec<&str> = uncovered.iter().filter_map(|a| a["usage"]["reason"].as_str()).collect();
        reasons.sort_unstable();
        reasons.dedup();
        out.push((None, format!("telemetry collector coverage: degraded ({} of {} open attempt(s) without bound usage: {}); run `telemetry {slug} collect`",
            uncovered.len(), active.len(), reasons.join(", "))));
    }
    let section = digest_section(&snapshot);
    out.push(((section.len() <= DIGEST_MAX_BYTES && section.lines().count() <= DIGEST_MAX_LINES).then_some(true),
        format!("telemetry digest section: {} of {DIGEST_MAX_BYTES} bytes, {} of {DIGEST_MAX_LINES} lines", section.len(), section.lines().count())));
    match snapshot["alerts"]["open"].as_array() {
        Some(open) if open.is_empty() => out.push((Some(true), format!("telemetry alerts: ok (no open alert; last evaluated {})",
            snapshot["alerts"]["last_evaluated_unix_ms"].as_i64().map_or("never".into(), |t| t.to_string())))),
        Some(open) => out.push((None, format!("telemetry alerts: {} open; see `telemetry {slug} health alerts`, `health notify` leaves inbox notices", open.len()))),
        None => out.push((None, format!("telemetry alerts: n/a ({}); run `telemetry {slug} health evaluate`", snapshot["alerts"]["reason"].as_str().unwrap_or("unknown")))),
    }
    out
}

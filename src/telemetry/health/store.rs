//! Health states and deduplicated alerts in sidecar stream `health`
//! (docs/telemetry/contracts-health.md §4). An evaluation reads every rule
//! first (`rules::evaluate`, read-only), then in one immediate sidecar
//! transaction: opens an alert for a condition without an open one (unless
//! its cooldown since the last resolution is still running), updates the open
//! alert of a persisting condition (deduplication), resolves the open alert of
//! a rule back to `ok`, and records every rule's state. An `unknown` outcome
//! alerts only for a rule that once had its source (an outage); it never
//! resolves an alert. Writes nothing else: no `state.db`, no other stream.
use super::rules::{self, Outcome, State};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

/// Evaluations kept in `health_evaluations`.
pub const KEEP_EVALUATIONS: i64 = 1000;
/// The ticker evaluates at most once per this interval, and only once an operator has run `health evaluate`.
pub const TICK_INTERVAL_MS: i64 = 300_000;

fn unavailable(reason: &str) -> Value { json!({"status": "unavailable", "reason": reason}) }

pub(super) fn tables(db: &Connection) -> rusqlite::Result<bool> {
    db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='health_alerts')", [], |r| r.get(0))
}

pub fn slug(project: &Path) -> String { project.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default() }

pub(super) fn alert_json(r: &rusqlite::Row) -> rusqlite::Result<Value> {
    let parse = |i: usize| -> rusqlite::Result<Value> { Ok(serde_json::from_str::<Value>(&r.get::<_, String>(i)?).unwrap_or(Value::Null)) };
    let resolved: Option<i64> = r.get(12)?;
    Ok(json!({"alert_id": r.get::<_, i64>(0)?, "rule": r.get::<_, String>(1)?, "labels": parse(2)?, "state": r.get::<_, String>(3)?, "reasons": parse(4)?,
        "metric": parse(5)?, "evidence_window": parse(6)?, "evidence": parse(7)?, "rules_version": r.get::<_, String>(8)?,
        "opened_unix_ms": r.get::<_, i64>(9)?, "last_seen_unix_ms": r.get::<_, i64>(10)?, "occurrences": r.get::<_, i64>(11)?,
        "resolved_unix_ms": resolved, "status": if resolved.is_some() { "resolved" } else { "open" },
        "notified_unix_ms": r.get::<_, Option<i64>>(13)?, "notice_id": r.get::<_, Option<String>>(14)?}))
}

pub(super) const ALERT_COLUMNS: &str = "alert_id,rule,labels,state,reasons,metric,evidence_window,evidence,rules_version,opened_unix_ms,last_seen_unix_ms,occurrences,resolved_unix_ms,notified_unix_ms,notice_id";

/// Open alerts, then (with `since`) alerts opened or resolved at or after it. Read-only.
pub fn alerts(project: &Path, since: Option<i64>) -> Result<Value> {
    let Some(db) = crate::telemetry::sidecar::read(project)? else { return Ok(unavailable("collection_not_run")) };
    if !tables(&db)? { return Ok(json!({"open": [], "recent": [], "since_unix_ms": since})); }
    let open: Vec<Value> = db.prepare(&format!("SELECT {ALERT_COLUMNS} FROM health_alerts WHERE resolved_unix_ms IS NULL ORDER BY alert_id"))?
        .query_map([], alert_json)?.collect::<rusqlite::Result<_>>()?;
    let recent: Vec<Value> = match since {
        None => Vec::new(),
        Some(since) => db.prepare(&format!("SELECT {ALERT_COLUMNS} FROM health_alerts WHERE resolved_unix_ms IS NOT NULL AND (opened_unix_ms>=?1 OR resolved_unix_ms>=?1) ORDER BY alert_id"))?
            .query_map([since], alert_json)?.collect::<rusqlite::Result<_>>()?,
    };
    let last: Option<i64> = db.query_row("SELECT max(evaluated_unix_ms) FROM health_evaluations", [], |r| r.get(0))?;
    Ok(json!({"open": open, "recent": recent, "since_unix_ms": since, "last_evaluated_unix_ms": last}))
}

fn summary(outcomes: &[Outcome]) -> Value {
    let mut counts = BTreeMap::from([("ok", 0), ("warn", 0), ("critical", 0), ("unknown", 0)]);
    for o in outcomes { *counts.entry(o.state.as_str()).or_default() += 1; }
    json!(counts)
}

/// `health evaluate` (and the ticker): evaluate, then record states and alerts.
pub fn evaluate(project: &Path, source: &str, now: i64) -> Result<Value> {
    let outcomes = rules::evaluate(project, now);
    let slug = slug(project);
    let Some(mut db) = crate::telemetry::sidecar::open(project, false)? else {
        let states: Vec<Value> = outcomes.iter().map(|o| o.json(&slug)).collect();
        return Ok(json!({"recorded": unavailable("collection_not_run"), "evaluated_unix_ms": now, "summary": summary(&outcomes), "states": states}));
    };
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (mut opened, mut updated, mut resolved, mut suppressed) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for o in &outcomes {
        let key = o.key(&slug);
        let prior: Option<(bool, i64)> = tx.query_row("SELECT known,suppressed FROM health_rule_states WHERE rule_key=?1", [&key], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let was_known = prior.is_some_and(|p| p.0);
        let mut held = prior.map_or(0, |p| p.1);
        let open: Option<(i64, String)> = tx.query_row("SELECT alert_id,state FROM health_alerts WHERE rule_key=?1 AND resolved_unix_ms IS NULL", [&key],
            |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let (reasons, metric, window, evidence) = (serde_json::to_string(&o.reasons)?, o.metric.to_string(), o.window.to_string(), o.evidence.to_string());
        match (o.state, open) {
            (State::Ok, Some((id, _))) => {
                tx.execute("UPDATE health_alerts SET resolved_unix_ms=?2,last_seen_unix_ms=max(last_seen_unix_ms,?2) WHERE alert_id=?1", params![id, now])?;
                resolved.push(json!({"alert_id": id, "rule": o.rule.name, "labels": o.labels(&slug)}));
            }
            (State::Ok, None) => {}
            (state, Some((id, previous))) => {
                tx.execute("UPDATE health_alerts SET state=?2,reasons=?3,metric=?4,evidence_window=?5,evidence=?6,last_seen_unix_ms=?7,occurrences=occurrences+1 WHERE alert_id=?1",
                    params![id, state.as_str(), reasons, metric, window, evidence, now])?;
                updated.push(json!({"alert_id": id, "rule": o.rule.name, "state": state.as_str(), "previous_state": previous, "deduplicated": true}));
            }
            (state, None) if state != State::Unknown || was_known => {
                let last: Option<i64> = tx.query_row("SELECT max(resolved_unix_ms) FROM health_alerts WHERE rule_key=?1", [&key], |r| r.get(0))?;
                if let Some(at) = last.filter(|at| now - at < o.rule.cooldown_ms) {
                    held += 1;
                    suppressed.push(json!({"rule": o.rule.name, "labels": o.labels(&slug), "state": state.as_str(), "cooldown_until_unix_ms": at + o.rule.cooldown_ms}));
                } else {
                    let mut reasons = o.reasons.clone();
                    if state == State::Unknown { reasons.insert(0, json!({"code": "source_lost", "detail": "the rule's source was available before and is not now"})); }
                    tx.execute("INSERT INTO health_alerts(rule_key,rule,labels,state,reasons,metric,evidence_window,evidence,rules_version,opened_unix_ms,last_seen_unix_ms,occurrences)
                        VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?10,1)", params![key, o.rule.name, o.labels(&slug).to_string(), state.as_str(),
                        serde_json::to_string(&reasons)?, metric, window, evidence, rules::VERSION, now])?;
                    opened.push(json!({"alert_id": tx.last_insert_rowid(), "rule": o.rule.name, "labels": o.labels(&slug), "state": state.as_str()}));
                }
            }
            (_, None) => {}
        }
        tx.execute("INSERT INTO health_rule_states(rule_key,rule,state,reasons,known,suppressed,evaluated_unix_ms) VALUES(?1,?2,?3,?4,?5,?6,?7)
            ON CONFLICT(rule_key) DO UPDATE SET state=excluded.state,reasons=excluded.reasons,known=excluded.known,suppressed=excluded.suppressed,evaluated_unix_ms=excluded.evaluated_unix_ms",
            params![key, o.rule.name, o.state.as_str(), reasons, was_known || o.state != State::Unknown, held, now])?;
    }
    let summary = summary(&outcomes);
    tx.execute("INSERT INTO health_evaluations(evaluated_unix_ms,rules_version,source,summary) VALUES(?1,?2,?3,?4)", params![now, rules::VERSION, source, summary.to_string()])?;
    tx.execute("DELETE FROM health_evaluations WHERE evaluation <= (SELECT max(evaluation) FROM health_evaluations) - ?1", [KEEP_EVALUATIONS])?;
    tx.commit()?;
    let states: Vec<Value> = outcomes.iter().map(|o| o.json(&slug)).collect();
    Ok(json!({"recorded": true, "evaluated_unix_ms": now, "source": source, "rules_version": rules::VERSION, "summary": summary,
        "opened": opened, "updated": updated, "resolved": resolved, "suppressed": suppressed, "states": states}))
}

/// Ticker: evaluate at most once per `TICK_INTERVAL_MS`, only once an operator has evaluated.
pub fn tick(project: &Path) -> Result<()> {
    let Some(db) = crate::telemetry::sidecar::read(project)? else { return Ok(()) };
    if !tables(&db)? { return Ok(()); }
    let last: Option<i64> = db.query_row("SELECT max(evaluated_unix_ms) FROM health_evaluations", [], |r| r.get(0))?;
    drop(db);
    let now = jiff::Timestamp::now().as_millisecond();
    match last {
        Some(at) if now - at >= TICK_INTERVAL_MS => evaluate(project, "tick", now).map(drop),
        _ => Ok(()),
    }
}

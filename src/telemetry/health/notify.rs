//! Alert notices (docs/telemetry/contracts-health.md §6). Local: each open
//! alert becomes one inbox notice through the canonical inbox's stable-id
//! delivery (`SqliteStore::deliver_telemetry_notice`, the memory-review
//! reminder path), so a retry or a second run never duplicates it; the
//! alert records its notice id. This is the only `state.db` write of the
//! telemetry health lane and happens only on the explicit `health notify`
//! command, never from the ticker. External: the deployment's
//! `<config_dir>/telemetry-alerts.toml` (disabled by default, the export
//! destination contract: a local directory or stdout, no network client).
use super::store::{ALERT_COLUMNS, alert_json, slug, tables};
use crate::telemetry::export::external::{self, Destination, Setting};
use anyhow::{Result, bail};
use rusqlite::params;
use serde_json::{Value, json};
use std::path::Path;

pub const ALERTS_CONFIG: Setting = Setting { file: "telemetry-alerts.toml", schema: "telemetry-alerts-config.v1", what: "alert notification",
    disabled: "external_notification_disabled" };
const KIND: &str = "telemetry-health";

fn open_alerts(project: &Path) -> Result<Option<Vec<Value>>> {
    let Some(db) = crate::telemetry::sidecar::read(project)? else { return Ok(None) };
    if !tables(&db)? { return Ok(Some(Vec::new())); }
    Ok(Some(db.prepare(&format!("SELECT {ALERT_COLUMNS} FROM health_alerts WHERE resolved_unix_ms IS NULL ORDER BY alert_id"))?
        .query_map([], alert_json)?.collect::<rusqlite::Result<_>>()?))
}

/// The notice's one-line summary: labels and reason codes only.
fn summary(project: &str, alert: &Value) -> String {
    let labels = &alert["labels"];
    let mut scope = String::new();
    for key in ["service", "role"] { if let Some(v) = labels[key].as_str() { scope += &format!(" {key}={v}"); } }
    let codes: Vec<&str> = alert["reasons"].as_array().into_iter().flatten().filter_map(|r| r["code"].as_str()).collect();
    format!("telemetry health {}: {} [{}{scope}] {}; advisory — `herdr-projects telemetry {project} health alerts`", alert["state"].as_str().unwrap_or(""),
        alert["rule"].as_str().unwrap_or(""), labels["family"].as_str().unwrap_or(""), codes.join(","))
}

/// `health notify`: one inbox notice per open alert not yet noticed.
pub fn inbox(project: &Path) -> Result<Value> {
    let Some(alerts) = open_alerts(project)? else { return Ok(json!({"status": "unavailable", "reason": "collection_not_run"})) };
    let slug = slug(project);
    let (mut delivered, mut already) = (Vec::new(), 0usize);
    for alert in alerts {
        if !alert["notified_unix_ms"].is_null() { already += 1; continue; }
        let (id, opened) = (alert["alert_id"].as_i64().unwrap_or_default(), alert["opened_unix_ms"].as_i64().unwrap_or_default());
        let notice = format!("{KIND}-{id}-{opened}");
        let content = crate::domain::InboxContent { id: notice.clone(), kind: KIND.into(), subject: alert["rule"].as_str().unwrap_or("health").into(),
            created: String::new(), summary: summary(&slug, &alert), body: String::new() };
        let mut store = crate::store::SqliteStore::open(&project.join(".state/state.db"))?;
        let mut outcome = None;
        for _ in 0..3 {
            let now = jiff::Timestamp::now().as_millisecond();
            match store.deliver_telemetry_notice(store.current_head()?, &content, now) {
                Ok(result) => { outcome = Some(result); break; }
                Err(crate::store::StoreError::Conflict) => continue,
                Err(error) => return Err(error.into()),
            }
        }
        let Some(outcome) = outcome else { bail!("store head moved on every attempt; retry `health notify`") };
        drop(store);
        let now = jiff::Timestamp::now().as_millisecond();
        if let Some(db) = crate::telemetry::sidecar::open(project, false)? {
            db.execute("UPDATE health_alerts SET notified_unix_ms=?2,notice_id=?3 WHERE alert_id=?1 AND notified_unix_ms IS NULL", params![id, now, notice])?;
        }
        delivered.push(json!({"alert_id": id, "notice_id": notice, "outcome": if outcome == crate::domain::ReminderOutcome::Delivered { "delivered" } else { "already_delivered" }, "summary": content.summary}));
    }
    Ok(json!({"target": "inbox", "delivered": delivered, "already_notified": already}))
}

/// `health notify --external`: every open alert to the configured destination; refused unless enabled.
pub fn external(project: &Path, config_dir: &Path) -> Result<(Value, Option<String>)> {
    let destination = match external::load_setting(config_dir, &ALERTS_CONFIG)? {
        Ok(d) => d,
        Err((code, detail)) => bail!("notify rejected: {}", json!({"code": code, "detail": detail})),
    };
    let Some(alerts) = open_alerts(project)? else { return Ok((json!({"status": "unavailable", "reason": "collection_not_run"}), None)) };
    let slug = slug(project);
    let payload = |a: &Value| json!({"contract": "telemetry-health-alert.v1", "project": slug, "alert": {"alert_id": a["alert_id"], "rule": a["rule"],
        "labels": a["labels"], "state": a["state"], "reasons": a["reasons"], "metric": a["metric"], "evidence_window": a["evidence_window"],
        "evidence": a["evidence"], "rules_version": a["rules_version"], "opened_unix_ms": a["opened_unix_ms"], "last_seen_unix_ms": a["last_seen_unix_ms"],
        "occurrences": a["occurrences"]}});
    match destination {
        Destination::Stdout => {
            let lines: String = alerts.iter().map(|a| payload(a).to_string() + "\n").collect();
            Ok((json!({"target": "external", "destination": "stdout", "alerts": alerts.len()}), Some(lines)))
        }
        Destination::Directory(dir) => {
            let (mut written, mut existing) = (Vec::new(), 0usize);
            for a in &alerts {
                let file = dir.join(format!("{slug}-health-{}-{}.json", a["alert_id"], a["opened_unix_ms"]));
                if std::fs::symlink_metadata(&file).is_ok() { existing += 1; continue; }
                external::write_new(&file, (serde_json::to_string_pretty(&payload(a))? + "\n").as_bytes())?;
                written.push(json!(file.file_name().map(|n| n.to_string_lossy().into_owned())));
            }
            Ok((json!({"target": "external", "destination": "directory", "written": written, "already_written": existing}), None))
        }
    }
}

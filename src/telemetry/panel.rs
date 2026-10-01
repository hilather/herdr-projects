//! S7 fleet popup: the contracts §6 text report plus active attempts. Read-only
//! and metadata-only (contracts §7): IDs, enums, counters and durations. It
//! never writes, launches, or opens the sidecar for writing; unknown reads `n/a`.
use super::read_only;
use anyhow::Result;
use rusqlite::Connection;
use serde_json::Value;
use std::path::Path;

fn table(db: &Connection, name: &str) -> rusqlite::Result<bool> {
    db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)", [name], |r| r.get(0))
}

/// `1h02m`, `1m30s`, `45s`.
pub(crate) fn elapsed(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    match (s / 3600, s / 60 % 60, s % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m{s:02}s"),
        (h, m, _) => format!("{h}h{m:02}m"),
    }
}

fn show_usage(value: &Value) -> String {
    match value.get("reason").and_then(Value::as_str) {
        Some(reason) => format!("n/a ({reason})"),
        None => format!("in={} out={} total={}", value["input_tokens"], value["output_tokens"], value["total_tokens"]),
    }
}

/// Sidecar presence and age of the last collect that ingested a rollout.
pub fn collection(project: &Path, now_ms: i64) -> Result<String> {
    let Some(db) = super::sidecar::read(project)? else {
        return Ok("collection not run (no telemetry sidecar)".into());
    };
    let last: Option<i64> = db.query_row("SELECT max(updated_unix_ms) FROM collect_offsets", [], |r| r.get(0))?;
    Ok(format!("sidecar present; last collect {}", last.map_or("n/a (no_rollouts)".into(), |at| format!("{} ago", elapsed(now_ms - at)))))
}

/// The popup body for one project with a canonical store.
pub fn render(project: &Path, now_ms: i64) -> Result<String> {
    let mut out = format!("usage: {}\n\n", collection(project, now_ms)?);
    // Through the query service's read path (contracts-analytics.md §5), as `telemetry report`.
    out += &super::metrics::text(&super::analytics::query::report(project, None)?);
    let db = read_only(&project.join(".state/state.db"))?;
    let label = if table(&db, "dispatch_decisions")? {
        "(SELECT json_extract(c.canonical_json,'$.kind')||' '||json_extract(c.canonical_json,'$.agent_version') FROM dispatch_decisions d
          JOIN agent_configurations c ON c.configuration_id=d.chosen_configuration_id WHERE d.attempt_id=a.id)"
    } else { "NULL" };
    let reserved = if table(&db, "attempt_lifecycle")? { "(SELECT unix_ms FROM attempt_lifecycle l WHERE l.attempt_id=a.id AND l.state='reserved')" } else { "NULL" };
    let active: Vec<(String, String, String, Option<String>, Option<i64>, Option<String>)> = db.prepare(&format!(
        "SELECT a.id,a.task_id,a.state,{label},{reserved},json_extract(i.payload,'$.inputs.effective_profile.kind') FROM attempts a
         LEFT JOIN attempt_inputs i ON i.attempt_id=a.id WHERE a.state NOT IN ('completed','failed','cancelled','lost') ORDER BY a.rowid"))?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?.collect::<rusqlite::Result<_>>()?;
    let sidecar = super::sidecar::read(project)?;
    out += &format!("\nactive attempts ({})\n", active.len());
    for (id, task, state, label, reserved, kind) in active {
        let usage = match (kind.as_deref(), &sidecar) {
            (Some(kind), _) if !matches!(kind, "codex" | "claude" | "opencode") => "n/a (adapter_absent)".into(),
            (_, None) => "n/a (collection_not_run)".into(),
            (_, Some(db)) => show_usage(&super::sidecar::attempt_usage(db, &id)?),
        };
        out += &format!("{id} {task} {} {state} elapsed {} usage {usage}\n", label.unwrap_or_else(|| "n/a (predates_dispatch_log)".into()),
            reserved.map_or("n/a (predates_lifecycle_log)".into(), |at| elapsed(now_ms - at)));
    }
    Ok(out)
}

/// TM4.2: the view sections under the report, one per operator view, each row
/// exactly as `telemetry <slug> view <name>` prints it (docs/telemetry/operator-views.md).
pub fn views(project: &Path, slug: &str) -> Result<String> { super::views::pane(project, slug) }

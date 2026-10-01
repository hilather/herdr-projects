//! Durable dependency generations. Mutations (including corrections and
//! deletions) advance their table's generation in the writer's transaction.
//! The canonical store remains read-only: its file/WAL identity and event
//! head invalidate projections, including offline restores and fixture edits.
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path};
use super::registry::Provider;

pub(crate) fn installation_current(db: &Connection) -> Result<bool> {
    if !db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='analytics_input_installation')", [], |r| r.get::<_, bool>(0))? { return Ok(false); }
    let schema: i64 = db.query_row("PRAGMA schema_version", [], |r| r.get(0))?;
    let installed: Option<i64> = db.query_row("SELECT schema_version FROM analytics_input_installation WHERE singleton=1", [], |r| r.get(0)).optional()?;
    Ok(installed == Some(schema))
}

pub(crate) fn install(db: &Connection) -> Result<()> {
    let schema: i64 = db.query_row("PRAGMA schema_version", [], |r| r.get(0))?;
    let installed: Option<i64> = db.query_row("SELECT schema_version FROM analytics_input_installation WHERE singleton=1", [], |r| r.get(0)).optional()?;
    if installed == Some(schema) { return Ok(()); }
    let tables: Vec<String> = db.prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")?
        .query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    for table in tables.iter().filter(|t| !t.starts_with("analytics_") && !t.starts_with("sqlite_") && !t.starts_with("health_") && !t.starts_with("policy_")) {
        // sqlite_master identifiers are quoted; literals use separate escaping.
        let identifier = table.replace('"', "\"\"");
        let literal = table.replace('\'', "''");
        db.execute("INSERT OR IGNORE INTO analytics_input_frontiers(input) VALUES(?1)", [table])?;
        for event in ["INSERT", "UPDATE", "DELETE"] {
            db.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS \"analytics_input_{identifier}_{event}\" AFTER {event} ON \"{identifier}\" BEGIN
                UPDATE analytics_input_frontiers SET sequence=sequence+1 WHERE input='{literal}'; END;"))?;
        }
    }
    let schema: i64 = db.query_row("PRAGMA schema_version", [], |r| r.get(0))?;
    db.execute("INSERT INTO analytics_input_installation(singleton,schema_version) VALUES(1,?1)
        ON CONFLICT(singleton) DO UPDATE SET schema_version=excluded.schema_version", [schema])?;
    Ok(())
}

/// All input generations in one bounded read. `None` on an old sidecar.
pub(crate) fn generations(db: &Connection) -> Result<Option<BTreeMap<String, i64>>> {
    if !db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='analytics_input_frontiers' AND type='table')", [], |r| r.get::<_, bool>(0))? { return Ok(None); }
    let mut generations: BTreeMap<String, i64> = db.prepare("SELECT input,sequence FROM analytics_input_frontiers ORDER BY input")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    generations.insert("schema".to_owned(), db.query_row("PRAGMA schema_version", [], |r| r.get(0))?);
    Ok(Some(generations))
}

pub(crate) fn canonical(project: &Path) -> Result<Value> {
    canonical_with(project, false)
}

pub(crate) fn canonical_current(project: &Path) -> Result<Value> {
    canonical_with(project, true)
}

fn canonical_with(project: &Path, fresh: bool) -> Result<Value> {
    let files = canonical_files(project);
    let path = project.join(".state/state.db");
    let db = if fresh { crate::telemetry::read_only_fresh(&path)? } else { crate::telemetry::read_only(&path)? };
    let head: i64 = db.query_row("SELECT coalesce(max(sequence),0) FROM events", [], |r| r.get(0))?;
    Ok(json!({"files": files, "head": head}))
}

/// Validate a canonical read prepared before writer admission without opening
/// another database while holding the sidecar writer. File/WAL identity is
/// already part of every canonical stamp; a changed identity defers the plan.
pub(crate) fn canonical_unchanged(project: &Path, prepared: &Value) -> bool {
    prepared["files"] == json!(canonical_files(project))
}

fn canonical_files(project: &Path) -> Vec<Value> {
    use std::os::unix::fs::MetadataExt;
    ["state.db", "state.db-wal"].iter().map(|name| {
        std::fs::metadata(project.join(".state").join(name)).ok().filter(|m| *name != "state.db-wal" || m.len() > 0).map(|m|
            json!([m.dev(),m.ino(),m.len(),m.mtime(),m.mtime_nsec(),m.ctime(),m.ctime_nsec()])).unwrap_or(Value::Null)
    }).collect()
}

pub(crate) fn group(provider: Provider, id: &str) -> &'static str {
    match provider {
        Provider::Native if id == "M03" => "operating",
        Provider::Native => "native",
        Provider::Central => "central",
        Provider::Lane("accounting") => match id {
            "M08" | "M09" | "M10" | "M38" | "M39" => "usage",
            "M12" | "M14" => "cost",
            "M11" => "charges",
            "M04" => "budget",
            "M16" | "M17" | "M18" => "tools",
            "M31" | "M32" | "M33" => "attention",
            _ => "fleet",
        },
        Provider::Lane(stream) => stream,
        _ => "unsupported",
    }
}

/// These evaluators consult the clock (censoring, active windows or maturity).
/// Their dependency is time as well as stored inputs, so they never skip.
pub(crate) fn clock(group: &str) -> bool { matches!(group, "attention" | "fleet" | "review" | "quality") }

pub(crate) fn stamp(group: &str, canonical: &Value, generations: &BTreeMap<String, i64>) -> String {
    let relevant = |table: &str| table == "schema" || match group {
        "native" => false,
        "operating" => table.starts_with("operating_"),
        "central" | "comparison" => !table.starts_with("operating_"),
        "diagnostics" => matches!(table, "codex_usage" | "codex_usage_times" | "rollout_sources"),
        "usage" => matches!(table, "rollout_sources" | "codex_usage" | "codex_quarantine" | "accounting_stream" | "accounting_dirty_sessions" | "accounting_usage_totals" | "accounting_cache_totals" | "accounting_cache_frontier" | "accounting_source_summary"),
        "cost" => table.starts_with("valuation") || table.starts_with("rate_card") || table == "rollout_sources",
        "charges" => table.starts_with("provider_") || table.starts_with("valuation"),
        "budget" => table.starts_with("valuation") || table == "rollout_sources",
        "tools" => table.starts_with("codex_tool") || table.starts_with("codex_agent") || table.starts_with("codex_mcp") || table == "codex_exec_items"
            || table == "codex_turn_aborts" || table.starts_with("claude_") || table.starts_with("opencode_") || table.starts_with("rollout_") || table == "attention_samples" || table == "accounting_tool_summary",
        _ => true,
    };
    let selected: BTreeMap<&str, i64> = generations.iter().filter(|(t, _)| relevant(t)).map(|(t, s)| (t.as_str(), *s)).collect();
    serde_json::to_string(&json!({"canonical": canonical, "sources": selected, "registry": super::registry::VERSION})).unwrap_or_default()
}

pub(crate) fn cached(db: &Connection, group: &str, since: Option<i64>, stamp: &str) -> Result<Option<Value>> {
    if generations(db)?.is_none() { return Ok(None); }
    let body: Option<String> = db.query_row("SELECT body FROM analytics_provider_aggregates WHERE provider=?1 AND window_key=?2 AND inputs=?3",
        rusqlite::params![group, window(since), stamp], |r| r.get(0)).optional()?;
    body.map(|b| serde_json::from_str(&b).map_err(Into::into)).transpose()
}

/// Extract only the requested metric. A coverage read must not deserialize
/// the report's thousands of M40 decisions merely to retrieve M13.
pub(crate) fn cached_metric(db: &Connection, group: &str, id: &str, since: Option<i64>, stamp: &str) -> Result<Option<Value>> {
    if generations(db)?.is_none() { return Ok(None); }
    let body: Option<String> = db.query_row("SELECT json_extract(body,?4) FROM analytics_provider_aggregates
        WHERE provider=?1 AND window_key=?2 AND inputs=?3", rusqlite::params![group, window(since), stamp, format!("$.{id}")], |r| r.get(0)).optional()?.flatten();
    body.map(|b| serde_json::from_str(&b).map_err(Into::into)).transpose()
}

pub(crate) fn window(since: Option<i64>) -> String { since.map_or_else(|| "all".to_owned(), |s| s.to_string()) }

/// The lifecycle watermark is an exact projection of canonical inputs, not
/// an analytics revision watermark (which may intentionally be historical).
pub(crate) fn store_canonical(project: &Path, db: &Connection) -> Result<()> {
    let canonical = canonical(project)?;
    let key = serde_json::to_string(&canonical)?;
    if db.query_row("SELECT EXISTS(SELECT 1 FROM accounting_canonical_watermark WHERE canonical=?1)", [&key], |r| r.get::<_, bool>(0))? { return Ok(()); }
    let tasks = super::lifecycle::load(project)?;
    let body = json!({"events_head": canonical["head"], "lifecycle_digest": super::lifecycle::digest(&tasks),
        "last_event_unix_ms": super::lifecycle::last_event(&tasks)});
    db.execute("INSERT INTO accounting_canonical_watermark VALUES(1,?1,?2)
        ON CONFLICT(singleton) DO UPDATE SET canonical=excluded.canonical,body=excluded.body", rusqlite::params![key, serde_json::to_string(&body)?])?;
    Ok(())
}

pub(crate) fn canonical_watermark(db: &Connection, canonical: &Value) -> Result<Option<Value>> {
    if !db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='accounting_canonical_watermark')", [], |r| r.get::<_, bool>(0))? { return Ok(None); }
    let body: Option<String> = db.query_row("SELECT body FROM accounting_canonical_watermark WHERE canonical=?1",
        [serde_json::to_string(canonical)?], |r| r.get(0)).optional()?;
    body.map(|b| serde_json::from_str(&b).map_err(Into::into)).transpose()
}

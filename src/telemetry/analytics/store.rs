//! Incremental aggregate revisions in sidecar stream `analytics`
//! (contracts-analytics.md §4). A refresh evaluates tracked cells with
//! changed inputs and appends a revision only for a cell whose content changed:
//! the first is `initial`, each later one a `restatement` superseding the
//! previous. Nothing here writes `state.db` or another stream's tables.
use super::lifecycle::{Lineage, Row};
use super::query::{self, Cell, Sources};
use super::registry::{self, Provider};
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

fn unavailable(reason: &str) -> Value { json!({"status": "unavailable", "reason": reason}) }

fn analytics_tables(db: &Connection) -> rusqlite::Result<bool> {
    db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='analytics_cells')", [], |r| r.get(0))
}

/// Tracked cells (canonical keys, sorted). Read-only.
fn tracked(db: &Connection) -> Result<Vec<String>> {
    if !analytics_tables(db)? { return Ok(Vec::new()); }
    Ok(db.prepare("SELECT cell FROM analytics_cells ORDER BY cell")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
}

/// The default tracked set: every active metric's current definition, its
/// default cohort, the unbounded window.
fn defaults() -> Vec<Cell> {
    registry::METRICS.iter().filter(|m| m.family.activation().is_ok() && !matches!(m.versions[0].provider, Provider::Absent(_) | Provider::Recommendation)).map(Cell::default_for).collect()
}

type SerializedLineage = BTreeMap<String, Vec<(&'static str, String, String)>>;

struct Evaluated {
    key: String, cell: Cell, core: String, projection: Option<String>,
    lineage: SerializedLineage, digest: String,
}

fn evaluate_all(project: &Path, cells: &[Cell]) -> Result<(Vec<Evaluated>, Value)> {
    let mut sources = Sources::new(project)?;
    sources.use_aggregates = false;
    let watermarks = sources.watermarks()?;
    let mut out = Vec::new();
    for cell in cells {
        let (core, lineage) = query::evaluate(&mut sources, cell)?;
        let digest = query::content_digest(&core, &lineage);
        let projection = workspace_body(cell.metric.id, &core)?;
        let core = serde_json::to_string(&core)?;
        let lineage = lineage.into_iter().map(|(bucket, rows)| {
            let rows = rows.into_iter().map(|(kind, id, attrs)| Ok((kind, id, serde_json::to_string(&attrs)?))).collect::<Result<Vec<_>>>()?;
            Ok((bucket, rows))
        }).collect::<Result<SerializedLineage>>()?;
        out.push(Evaluated { key: cell.key(), digest, cell: cell.clone(), core, projection, lineage });
    }
    Ok((out, watermarks))
}

/// The cells a refresh or rebuild covers: tracked ones (or the defaults when
/// none is tracked yet) plus `extra`.
fn cells(project: &Path, extra: Option<Cell>) -> Result<Option<Vec<Cell>>> {
    let Some(db) = crate::telemetry::sidecar::read(project)? else { return Ok(None) };
    let mut cells: Vec<Cell> = tracked(&db)?.iter().filter_map(|key| Cell::parse(key)).collect();
    if cells.is_empty() { cells = defaults(); }
    if let Some(extra) = extra && !cells.iter().any(|c| c.key() == extra.key()) { cells.push(extra); }
    cells.sort_by_key(Cell::key);
    Ok(Some(cells))
}

fn track_cell(tx: &rusqlite::Transaction<'_>, cell: &Cell, key: &str, now: i64) -> Result<()> {
    tx.prepare_cached("INSERT OR IGNORE INTO analytics_cells(cell,metric,definition,cohort,window_from_unix_ms,window_to_unix_ms,horizon_ms,dimension,tracked_unix_ms)
        VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)")?.execute(rusqlite::params![key, cell.metric.id, cell.version.definition, cell.cohort.as_str(), cell.from, cell.to,
        cell.horizon, cell.by, now])?;
    Ok(())
}

/// Append one cell without retaining other cells' lineage in memory.
fn append_cell(tx: &rusqlite::Transaction<'_>, e: &Evaluated, watermarks: &str, now: i64) -> Result<Option<Value>> {
    track_cell(tx, &e.cell, &e.key, now)?;
    let latest: Option<(i64, String)> = tx.prepare_cached("SELECT revision,content_digest FROM analytics_revisions WHERE cell=?1 ORDER BY revision DESC LIMIT 1")?.query_row([&e.key],
        |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
    if latest.as_ref().is_some_and(|(_, digest)| *digest == e.digest) {
        tx.prepare_cached("UPDATE analytics_cells SET checked_unix_ms=?2 WHERE cell=?1")?.execute(rusqlite::params![e.key, now])?;
        // Normally this row already exists. Refresh also repairs missing
        // disposable projections without restating the authoritative cell.
        let revision = latest.as_ref().unwrap().0;
        if let Some(body) = &e.projection {
            let exists: bool = tx.prepare_cached("SELECT EXISTS(SELECT 1 FROM analytics_workspace_metrics WHERE revision=?1)")?.query_row([revision], |r| r.get(0))?;
            if !exists { insert_workspace_body(tx, revision, body)?; }
        }
        return Ok(None);
    }
    let supersedes = latest.map(|(revision, _)| revision);
    let kind = if supersedes.is_some() { "restatement" } else { "initial" };
    tx.prepare_cached("INSERT INTO analytics_revisions(cell,kind,supersedes,body,content_digest,watermarks,registry,recorded_unix_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)")?.execute(
        rusqlite::params![e.key, kind, supersedes, e.core, e.digest, watermarks, registry::VERSION, now])?;
    let revision = tx.last_insert_rowid();
    if let Some(body) = &e.projection { insert_workspace_body(tx, revision, body)?; }
    let mut insert = tx.prepare_cached("INSERT INTO analytics_lineage(revision,bucket,ordinal,entity_kind,entity_id,attrs) VALUES(?1,?2,?3,?4,?5,?6)")?;
    for (bucket, rows) in &e.lineage {
        for (ordinal, (kind, id, attrs)) in rows.iter().enumerate() {
            insert.execute(rusqlite::params![revision, bucket, ordinal as i64, kind, id, attrs])?;
        }
    }
    drop(insert);
    tx.prepare_cached("UPDATE analytics_cells SET checked_unix_ms=?2 WHERE cell=?1")?.execute(rusqlite::params![e.key, now])?;
    Ok(Some(json!({"cell": serde_json::from_str::<Value>(&e.key)?, "revision": revision, "kind": kind, "supersedes": supersedes, "content_digest": e.digest})))

}

/// Append a revision for each changed cell, in one immediate transaction.
fn append(project: &Path, evaluated: &[Evaluated], watermarks: &Value, now: i64) -> Result<Value> {
    let Some(mut db) = crate::telemetry::sidecar::open(project, false)? else { return Ok(unavailable("collection_not_run")) };
    // Comparison is supplemental presentation evidence, not a metric cell:
    // recording it must not change tracked cells, metric digests or lineage.
    let comparison = crate::telemetry::workspace::comparison(project)?;
    let body = serde_json::to_string(&comparison)?;
    let watermarks = serde_json::to_string(watermarks)?;
    // Evaluated bodies, lineage and rendering projections are already serialized.
    // Only the latest-revision decisions and their writes need the writer lock.
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate).context("acquire analytics writer")?;
    let previous: Option<String> = tx.query_row("SELECT body FROM analytics_workspace_comparisons ORDER BY revision DESC LIMIT 1", [], |r| r.get(0)).optional()?;
    if previous.as_deref() != Some(body.as_str()) {
        tx.execute("INSERT INTO analytics_workspace_comparisons(body,recorded_unix_ms) VALUES(?1,?2)", rusqlite::params![body, now])?;
    }
    let (mut appended, mut unchanged) = (Vec::new(), 0);
    for e in evaluated {
        match append_cell(&tx, e, &watermarks, now)? { Some(revision) => appended.push(revision), None => unchanged += 1 }
    }
    tx.commit()?;
    Ok(json!({"appended": appended, "unchanged": unchanged, "cells": evaluated.len(), "recorded_unix_ms": now}))
}

/// A bounded, disposable rendering projection of an immutable metric body.
/// The authoritative analytics body, digest and lineage remain untouched.
fn workspace_body(metric: &str, core: &Value) -> Result<Option<String>> {
    if !crate::telemetry::workspace::METRICS.contains(&metric) { return Ok(None); }
    let mut body = match core.as_object() {
        Some(core) => Value::Object(core.iter().filter(|(key, _)| key.as_str() != "detail").map(|(key, value)| (key.clone(), value.clone())).collect()),
        None => core.clone(),
    };
    if metric == "M40" {
        let mut quota: Vec<Value> = Vec::new();
        for decision in core["detail"]["decisions"].as_array().into_iter().flatten() {
            match quota.iter_mut().find(|q| q["service"] == decision["service"]) {
                Some(q) if q["decided_unix_ms"].as_i64() >= decision["decided_unix_ms"].as_i64() => {},
                Some(q) => *q = decision.clone(),
                None => quota.push(decision.clone()),
            }
        }
        body["detail"] = json!({"decisions": quota});
    }
    Ok(Some(serde_json::to_string(&body)?))
}

fn insert_workspace_body(db: &Connection, revision: i64, body: &str) -> Result<()> {
    db.prepare_cached("INSERT OR IGNORE INTO analytics_workspace_metrics(revision,body) VALUES(?1,?2)")?.execute(rusqlite::params![revision, body])?;
    Ok(())
}

fn workspace_metric_body(db: &Connection, metric: &str, core: &Value, revision: i64) -> Result<()> {
    if let Some(body) = workspace_body(metric, core)? { insert_workspace_body(db, revision, &body)?; }
    Ok(())
}

/// `telemetry <slug> analytics refresh`: writes only the sidecar's analytics tables.
pub fn refresh(project: &Path, extra: Option<Cell>) -> Result<Value> {
    let Some(mut db) = crate::telemetry::sidecar::open(project, false)? else { return Ok(unavailable("collection_not_run")) };
    let Some(cells) = cells(project, extra)? else { return Ok(unavailable("collection_not_run")) };
    // All provider opens on this thread share these pinned read snapshots.
    // WAL writers can collect/sync while evaluation and serialization run.
    let canonical_before = super::inputs::canonical_current(project)?;
    let snapshot = crate::telemetry::EvaluationReads::begin(project)?;
    let mut sources = Sources::new(project)?;
    if canonical_before != sources.canonical_inputs {
        let keys = cells.iter().map(Cell::key).collect::<Vec<_>>();
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate).context("acquire analytics writer")?;
        let locked = std::time::Instant::now();
        let now = jiff::Timestamp::now().as_millisecond();
        for (cell, key) in cells.iter().zip(&keys) { track_cell(&tx, cell, key, now)?; }
        tx.commit()?;
        return Ok(json!({"appended": [], "unchanged": 0, "cells": cells.len(),
            "recorded_unix_ms": jiff::Timestamp::now().as_millisecond(), "evaluated": [],
            "deferred": cells.iter().map(|c| serde_json::from_str::<Value>(&c.key())).collect::<serde_json::Result<Vec<_>>>()?,
            "comparison_deferred": true, "write_lock_ms": locked.elapsed().as_secs_f64() * 1000.0}));
    }
    let watermarks = serde_json::to_string(&sources.watermarks()?)?;
    let now = jiff::Timestamp::now().as_millisecond();
    let (mut appended, mut unchanged) = (Vec::new(), 0);
    let (mut evaluated, mut deferred) = (Vec::new(), Vec::new());
    let mut write_lock_ms = 0.0;
    let mut skipped = Vec::new();
    for cell in &cells {
        let key = cell.key();
        let group = super::inputs::group(cell.version.provider, cell.metric.id);
        let inputs = super::inputs::stamp(group, &sources.canonical_inputs, &sources.input_generations);
        let read = crate::telemetry::sidecar::read(project)?.expect("sidecar snapshot");
        let previous: Option<String> = read.query_row("SELECT inputs FROM analytics_checked_inputs WHERE cell=?1
            AND EXISTS(SELECT 1 FROM analytics_revisions WHERE cell=?1)", [&key], |r| r.get(0)).optional()?;
        let missing_projection = crate::telemetry::workspace::METRICS.contains(&cell.metric.id) && !read.query_row(
            "SELECT EXISTS(SELECT 1 FROM analytics_workspace_metrics WHERE revision=(SELECT max(revision) FROM analytics_revisions WHERE cell=?1))",
            [&key], |r| r.get::<_, bool>(0))?;
        let e = if !super::inputs::clock(group) && !missing_projection && previous.as_deref() == Some(inputs.as_str()) {
            None
        } else {
            let (core, lineage) = query::evaluate(&mut sources, cell)?;
            let digest = query::content_digest(&core, &lineage);
            let projection = workspace_body(cell.metric.id, &core)?;
            let core = serde_json::to_string(&core)?;
            let lineage = lineage.into_iter().map(|(bucket, rows)| Ok((bucket, rows.into_iter().map(|(kind, id, attrs)|
                Ok((kind, id, serde_json::to_string(&attrs)?))).collect::<Result<Vec<_>>>()?))).collect::<Result<SerializedLineage>>()?;
            evaluated.push(serde_json::from_str::<Value>(&key)?);
            Some(Evaluated { key: key.clone(), cell: cell.clone(), digest, core, projection, lineage })
        };
        if e.is_none() {
            skipped.push((key, group, inputs));
            continue;
        }
        // A separate writer reads the live generations, never the pinned snapshot.
        // Revalidate even skipped cells: a racing mutation must leave them due.
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate).context("acquire analytics writer")?;
        let locked = std::time::Instant::now();
        let current = super::inputs::generations(&tx)?.unwrap_or_default();
        let canonical = super::inputs::canonical_current(project)?;
        if inputs != super::inputs::stamp(group, &canonical, &current) {
            deferred.push(serde_json::from_str::<Value>(&key)?);
            // Persist the request without certifying its stale inputs. This
            // also keeps a deferred extra/default cell due on the next refresh.
            track_cell(&tx, cell, &key, now)?;
            tx.commit()?;
            write_lock_ms += locked.elapsed().as_secs_f64() * 1000.0;
            continue;
        }
        match append_cell(&tx, e.as_ref().expect("evaluated cell"), &watermarks, now)? {
            Some(revision) => appended.push(revision), None => unchanged += 1,
        }
        tx.prepare_cached("INSERT INTO analytics_checked_inputs(cell,inputs) VALUES(?1,?2) ON CONFLICT(cell) DO UPDATE SET inputs=excluded.inputs")?
            .execute(rusqlite::params![key, inputs])?;
        tx.commit()?;
        write_lock_ms += locked.elapsed().as_secs_f64() * 1000.0;
    }
    // Comparison and provider bodies are serialized before taking the writer.
    let comparison = serde_json::to_string(&crate::telemetry::workspace::comparison(project)?)?;
    let bodies = sources.bodies.iter().filter(|((_, group), _)| !super::inputs::clock(group))
        .map(|((since, group), body)| Ok((group.clone(), super::inputs::window(*since),
            super::inputs::stamp(group, &sources.canonical_inputs, &sources.input_generations), serde_json::to_string(body)?)))
        .collect::<Result<Vec<_>>>()?;
    drop(snapshot);
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate).context("acquire analytics writer")?;
    let locked = std::time::Instant::now();
    let current = super::inputs::generations(&tx)?.unwrap_or_default();
    let canonical = super::inputs::canonical_current(project)?;
    // Unchanged cells need only one bounded batch of checked-time updates.
    for (key, group, inputs) in skipped {
        if inputs != super::inputs::stamp(group, &canonical, &current) {
            deferred.push(serde_json::from_str::<Value>(&key)?);
            continue;
        }
        tx.prepare_cached("UPDATE analytics_cells SET checked_unix_ms=?2 WHERE cell=?1")?
            .execute(rusqlite::params![key, now])?;
        unchanged += 1;
    }
    let comparison_deferred = super::inputs::stamp("central", &canonical, &current)
        != super::inputs::stamp("central", &sources.canonical_inputs, &sources.input_generations);
    if !comparison_deferred {
        let previous: Option<String> = tx.query_row("SELECT body FROM analytics_workspace_comparisons ORDER BY revision DESC LIMIT 1", [], |r| r.get(0)).optional()?;
        if previous.as_deref() != Some(comparison.as_str()) {
            tx.execute("INSERT INTO analytics_workspace_comparisons(body,recorded_unix_ms) VALUES(?1,?2)", rusqlite::params![comparison, now])?;
        }
    }
    for (group, window, inputs, body) in bodies {
        if inputs != super::inputs::stamp(&group, &canonical, &current) { continue; }
        tx.prepare_cached("INSERT INTO analytics_provider_aggregates(provider,window_key,inputs,body) VALUES(?1,?2,?3,?4)
            ON CONFLICT(provider,window_key) DO UPDATE SET inputs=excluded.inputs,body=excluded.body")?
            .execute(rusqlite::params![group, window, inputs, body])?;
    }
    tx.commit()?;
    write_lock_ms += locked.elapsed().as_secs_f64() * 1000.0;
    Ok(json!({"appended": appended, "unchanged": unchanged, "cells": cells.len(), "recorded_unix_ms": now,
        "evaluated": evaluated, "deferred": deferred, "comparison_deferred": comparison_deferred, "write_lock_ms": write_lock_ms}))
}

/// A stored revision's lineage, as evaluation produced it.
fn stored_lineage(db: &Connection, revision: i64) -> Result<Lineage> {
    let mut lineage = Lineage::new();
    let mut stmt = db.prepare("SELECT bucket,entity_kind,entity_id,attrs FROM analytics_lineage WHERE revision=?1 ORDER BY bucket,ordinal")?;
    let rows = stmt.query_map([revision], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)))?;
    for row in rows {
        let (bucket, kind, id, attrs) = row?;
        let kind: &'static str = if kind == "attempt" { "attempt" } else { "task" };
        let row: Row = (kind, id, serde_json::from_str(&attrs)?);
        lineage.entry(bucket).or_default().push(row);
    }
    Ok(lineage)
}

/// `analytics rebuild [--verify]`: recompute every tracked cell from the
/// sources, ignoring nothing cached, and compare it byte for byte with the
/// latest stored revision (whose own bytes are re-digested: `stored_intact`).
/// Without `--verify` a differing cell gets a restatement.
pub fn rebuild(project: &Path, verify: bool) -> Result<Value> {
    let Some(cells) = cells(project, None)? else { return Ok(unavailable("collection_not_run")) };
    let (evaluated, watermarks) = evaluate_all(project, &cells)?;
    let mut report = Vec::new();
    {
        let db = crate::telemetry::sidecar::read(project)?;
        for e in &evaluated {
            let stored = match db.as_deref() { Some(db) => query::latest(db, &e.key)?, None => None };
            let (revision, stored_digest, intact) = match (&stored, db.as_deref()) {
                (Some(s), Some(db)) => (Some(s.revision), Some(s.digest.clone()), Some(query::content_digest(&s.body, &stored_lineage(db, s.revision)?) == s.digest)),
                _ => (None, None, None),
            };
            report.push(json!({"cell": serde_json::from_str::<Value>(&e.key)?, "revision": revision, "stored_digest": stored_digest, "rebuilt_digest": e.digest,
                "identical": stored_digest.as_deref() == Some(e.digest.as_str()), "stored_intact": intact}));
        }
    }
    let identical = report.iter().all(|r| r["identical"] == true);
    let appended = if verify { json!([]) } else { append(project, &evaluated, &watermarks, jiff::Timestamp::now().as_millisecond())?["appended"].clone() };
    if !verify {
        let Some(mut db) = crate::telemetry::sidecar::open(project, false)? else { return Ok(unavailable("collection_not_run")); };
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate).context("acquire analytics writer")?;
        // append evaluated the current comparison using the same estimator;
        // recreate its rendering row while retaining its recorded provenance.
        let latest: Option<(i64, String, i64)> = tx.query_row("SELECT revision,body,recorded_unix_ms FROM analytics_workspace_comparisons ORDER BY revision DESC LIMIT 1", [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
        if let Some((revision, body, at)) = latest {
            tx.execute("DELETE FROM analytics_workspace_comparisons WHERE revision=?1", [revision])?;
            tx.execute("INSERT INTO analytics_workspace_comparisons(revision,body,recorded_unix_ms) VALUES(?1,?2,?3)", rusqlite::params![revision, body, at])?;
        }
        tx.execute("DELETE FROM analytics_workspace_metrics", [])?;
        let rows: Vec<(i64, String, String)> = tx.prepare("SELECT r.revision,c.metric,r.body FROM analytics_revisions r JOIN analytics_cells c ON c.cell=r.cell ORDER BY r.revision")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
        for (revision, metric, body) in rows { workspace_metric_body(&tx, &metric, &serde_json::from_str(&body)?, revision)?; }
        tx.commit()?;
    }
    Ok(json!({"verify": verify, "identical": identical, "cells": report, "appended": appended}))
}

/// Recorded comparison for the pane. Old sidecars need an analytics refresh;
/// a read never creates tables or falls back to a whole-history computation.
pub(crate) fn workspace_comparison(project: &Path, digest: bool) -> Result<Option<Value>> {
    let Some(db) = crate::telemetry::sidecar::read(project)? else { return Ok(None); };
    let exists: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='analytics_workspace_comparisons')", [], |r| r.get(0))?;
    if !exists { return Ok(None); }
    let sql = if digest {
        "WITH latest AS (SELECT * FROM analytics_workspace_comparisons ORDER BY revision DESC LIMIT 1)
        SELECT revision, CASE WHEN json_type(body,'$.cells')='array' THEN json_set(body,
            '$.cell_count',json_array_length(body,'$.cells'), '$.cells',json(coalesce((SELECT json_group_array(json(value)) FROM (
                SELECT json_set(c.value,'$.arm_count',json_array_length(c.value,'$.arms'),'$.arms',json(coalesce((
                    SELECT json_group_array(json(value)) FROM (SELECT value FROM json_each(c.value,'$.arms') LIMIT 4)), '[]'))) AS value
                FROM json_each(latest.body,'$.cells') c LIMIT 4)), '[]'))) ELSE body END, recorded_unix_ms FROM latest"
    } else { "SELECT revision,body,recorded_unix_ms FROM analytics_workspace_comparisons ORDER BY revision DESC LIMIT 1" };
    let row: Option<(i64, String, i64)> = db.query_row(sql, [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
    row.map(|(seq, body, at)| {
        let mut report: Value = serde_json::from_str(&body)?;
        report["as_of"] = json!({"seq": seq, "unix_ms": at});
        Ok(report)
    }).transpose()
}

/// `analytics snapshot`: the latest content of every tracked cell, without
/// revision numbers or times, so two rebuilds from the same sources compare byte for byte.
pub fn snapshot(project: &Path) -> Result<Value> {
    let Some(db) = crate::telemetry::sidecar::read(project)? else { return Ok(unavailable("collection_not_run")) };
    let mut cells = Vec::new();
    for key in tracked(&db)? {
        let Some(stored) = query::latest(&db, &key)? else { continue };
        let lineage: BTreeMap<String, Vec<Value>> = stored_lineage(&db, stored.revision)?.into_iter()
            .map(|(bucket, rows)| (bucket, rows.into_iter().map(|(kind, id, attrs)| json!([kind, id, attrs])).collect())).collect();
        cells.push(json!({"cell": serde_json::from_str::<Value>(&key)?, "content_digest": stored.digest, "body": stored.body, "lineage": lineage}));
    }
    Ok(json!({"registry": registry::VERSION, "cells": cells}))
}

/// `analytics revisions [--metric M]`: revision history with provenance.
pub fn revisions(project: &Path, metric: Option<&str>) -> Result<Value> {
    let Some(db) = crate::telemetry::sidecar::read(project)? else { return Ok(unavailable("collection_not_run")) };
    if !analytics_tables(&db)? { return Ok(json!({"revisions": []})); }
    let mut stmt = db.prepare("SELECT r.revision,r.cell,r.kind,r.supersedes,r.content_digest,r.watermarks,r.recorded_unix_ms FROM analytics_revisions r
        JOIN analytics_cells c ON c.cell=r.cell WHERE ?1 IS NULL OR c.metric=?1 ORDER BY r.revision")?;
    let rows: Vec<Value> = stmt.query_map([metric], |r| {
        let (cell, watermarks): (String, String) = (r.get(1)?, r.get(5)?);
        Ok(json!({"revision": r.get::<_, i64>(0)?, "cell": serde_json::from_str::<Value>(&cell).unwrap_or(Value::Null), "kind": r.get::<_, String>(2)?,
            "supersedes": r.get::<_, Option<i64>>(3)?, "content_digest": r.get::<_, String>(4)?, "watermarks": serde_json::from_str::<Value>(&watermarks).unwrap_or(Value::Null),
            "recorded_unix_ms": r.get::<_, i64>(6)?}))
    })?.collect::<rusqlite::Result<_>>()?;
    Ok(json!({"revisions": rows}))
}

/// Ticker: refresh tracked cells at most once per `TICK_INTERVAL_MS`, only once
/// an operator has run `analytics refresh` (a tracked cell exists).
pub const TICK_INTERVAL_MS: i64 = 60_000;

pub fn tick(project: &Path) -> Result<()> {
    let Some(db) = crate::telemetry::sidecar::read(project)? else { return Ok(()) };
    if !analytics_tables(&db)? { return Ok(()); }
    let last: Option<i64> = db.query_row("SELECT max(checked_unix_ms) FROM analytics_cells", [], |r| r.get(0))?;
    drop(db);
    match last {
        Some(at) if jiff::Timestamp::now().as_millisecond() - at >= TICK_INTERVAL_MS => refresh(project, None).map(drop),
        _ => Ok(()),
    }
}

/// One `EXPLAIN QUERY PLAN` check. `inherent`: tables (or aliases) the query
/// must read in full because it aggregates every row; any other `SCAN` is a
/// missing index. `proposed`: the index its owning stream should add.
struct Plan { name: &'static str, store: &'static str, owner: &'static str, sql: String, inherent: &'static [&'static str], proposed: Option<&'static str> }

fn explain(db: &Connection, plan: &Plan) -> Result<Value> {
    let mut stmt = db.prepare(&format!("EXPLAIN QUERY PLAN {}", plan.sql))?;
    let nulls = vec![rusqlite::types::Null; stmt.parameter_count()];
    let details: Vec<String> = stmt.query_map(rusqlite::params_from_iter(nulls), |r| r.get::<_, String>(3))?.collect::<rusqlite::Result<_>>()?;
    let scans: Vec<String> = details.iter().filter_map(|d| d.strip_prefix("SCAN ").map(|rest| rest.split_whitespace().next().unwrap_or("").to_owned())).collect();
    let unexpected: Vec<&String> = scans.iter().filter(|s| !plan.inherent.contains(&s.as_str())).collect();
    // An automatic index is built for every run of the query: a persistent one is missing.
    let automatic: Vec<&String> = details.iter().filter(|d| d.contains("AUTOMATIC")).collect();
    let verdict = if !unexpected.is_empty() || !automatic.is_empty() { "needs_index" } else if scans.is_empty() { "indexed" } else { "full_scan_inherent" };
    Ok(json!({"name": plan.name, "store": plan.store, "owner": plan.owner, "plan": details, "scans": scans, "unexpected_scans": unexpected,
        "automatic_indexes": automatic, "verdict": verdict,
        "proposed_index": if verdict == "needs_index" { json!(plan.proposed) } else { Value::Null }}))
}

/// `analytics plans`: the hot queries' plans on this project's stores. Read-only.
pub fn plans(project: &Path) -> Result<Value> {
    let state = crate::telemetry::read_only(&project.join(".state/state.db"))?;
    let mut out = Vec::new();
    for (name, sql) in super::lifecycle::queries(&state)? {
        // Acceptance times join every verified result once: with the canonical
        // `verified_results_by_submission` index (0067) SQLite may drive the join from `r`.
        let inherent: &'static [&'static str] = match name { "lifecycle_attempts" => &["a"], "lifecycle_classes" => &["task_classifications"], "lifecycle_replay_candidates" => &["replay_candidates"],
            "lifecycle_acceptance_times" => &["c", "r"], _ => &["c"] };
        let proposed = (name == "lifecycle_acceptance_times").then_some("CREATE INDEX verified_results_by_submission ON verified_results(submission_id)");
        out.push(explain(&state, &Plan { name, store: "state.db", owner: "canonical (steward; read-only here)", sql, inherent, proposed })?);
    }
    out.push(explain(&state, &Plan { name: "canonical_head", store: "state.db", owner: "canonical (steward; read-only here)", sql: "SELECT coalesce(max(sequence),0) FROM events".into(), inherent: &[], proposed: None })?);
    drop(state);
    if let Some(db) = crate::telemetry::sidecar::read(project)? {
        if analytics_tables(&db)? {
            for (name, sql) in [("as_of_seq", query::AS_OF_SEQ), ("as_of_time", query::AS_OF_TIME), ("latest_revision", query::LATEST), ("next_revision", query::NEXT),
                ("lineage_page", query::PAGE), ("lineage_buckets", query::BUCKETS)] {
                out.push(explain(&db, &Plan { name, store: "telemetry.db", owner: "analytics", sql: sql.into(), inherent: &[], proposed: None })?);
            }
        }
        // Other streams' hot reads (central M08/M15/M13 source scan): described, never migrated here.
        out.push(explain(&db, &Plan { name: "codex_usage_by_source", store: "telemetry.db", owner: "codex (steward)",
            sql: "SELECT s.session_id,EXISTS(SELECT 1 FROM codex_usage u WHERE u.path_digest=s.path_digest AND u.reason='cli_version_uncertified'),
                (SELECT count(*) FROM codex_usage u WHERE u.path_digest=s.path_digest AND u.accepted=1) FROM rollout_sources s ORDER BY s.path_digest".into(),
            inherent: &["s"], proposed: Some("CREATE INDEX codex_usage_by_path ON codex_usage(path_digest, accepted, reason)") })?);
        out.push(explain(&db, &Plan { name: "codex_usage_by_session", store: "telemetry.db", owner: "codex (steward)",
            sql: "SELECT count(*),sum(input_tokens) FROM codex_usage WHERE session_id=?1 AND accepted=1".into(), inherent: &[], proposed: None })?);
    }
    let needs: Vec<&Value> = out.iter().filter(|p| p["verdict"] == "needs_index").collect();
    let summary = json!({"needs_index": needs.iter().map(|p| json!({"name": p["name"], "owner": p["owner"], "proposed_index": p["proposed_index"]})).collect::<Vec<_>>()});
    Ok(json!({"plans": out, "summary": summary}))
}

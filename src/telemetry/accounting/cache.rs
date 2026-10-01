//! M10: compatible normalized cache reads / inclusive input, never money.
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};
use serde_json::{Value, json};
use std::{collections::{BTreeMap, BTreeSet}, path::Path};

#[derive(Default)]
struct Tally {
    input: i64,
    read: i64,
    write: i64,
    records: i64,
    sessions: BTreeSet<String>,
    excluded: BTreeMap<String, usize>,
}

impl Tally {
    fn body(&self) -> Value {
        let value = if self.sessions.is_empty() { super::unavailable("no_eligible_cache_usage") }
            else if self.input == 0 { super::unavailable("empty_denominator") }
            else { json!(format!("{}/{}", self.read, self.input)) };
        json!({"value": value, "numerator": self.read, "denominator": self.input,
            "cache_write_tokens": self.write, "unit": "ratio", "basis": "normalized_accepted_usage",
            "coverage": {"certified_sessions": self.sessions.len(), "accepted_records": self.records,
                "excluded": self.excluded}})
    }

    fn include(&mut self, session: &str, totals: [i64; 4]) {
        if self.sessions.insert(session.to_owned()) {
            self.input += totals[0]; self.read += totals[1]; self.write += totals[2]; self.records += totals[3];
        }
    }
}

fn unavailable(reason: &str) -> Value {
    json!({"definition": "M10.v1", "name": "cache_read_share", "value": super::unavailable(reason)})
}

pub(crate) fn complete(db: &Connection) -> Result<bool> {
    let exists: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='accounting_cache_totals')", [], |r| r.get(0))?;
    Ok(exists && db.query_row("SELECT NOT EXISTS(SELECT 1 FROM rollout_sources s
        WHERE NOT EXISTS(SELECT 1 FROM accounting_cache_totals t WHERE t.session_id=s.session_id))", [], |r| r.get(0))?)
}

/// Newly captured sessions need their first totals; missing untouched totals
/// invalidate the incremental base instead.
pub(crate) fn complete_for_sync(db: &Connection) -> Result<bool> {
    Ok(db.query_row("SELECT NOT EXISTS(SELECT 1 FROM rollout_sources s
        WHERE NOT EXISTS(SELECT 1 FROM accounting_cache_totals t WHERE t.session_id=s.session_id)
        AND NOT EXISTS(SELECT 1 FROM accounting_dirty_sessions d WHERE d.session_id=s.session_id))", [], |r| r.get(0))?)
}

fn inputs(db: &Connection) -> Result<Option<String>> {
    let Some(generations) = crate::telemetry::analytics::inputs::generations(db)? else { return Ok(None); };
    let selected: BTreeMap<_, _> = generations.into_iter().filter(|(table, _)|
        matches!(table.as_str(), "rollout_sources" | "codex_usage" | "codex_quarantine")).collect();
    Ok((selected.len() == 3).then(|| serde_json::to_string(&selected)).transpose()?)
}

pub(crate) fn store_frontier(db: &Connection) -> Result<()> {
    if let Some(inputs) = inputs(db)? {
        db.execute("INSERT INTO accounting_cache_frontier VALUES(1,?1) ON CONFLICT(singleton) DO UPDATE SET inputs=excluded.inputs", [inputs])?;
    }
    Ok(())
}

fn current(db: &Connection) -> Result<bool> {
    if !complete(db)? { return Ok(false); }
    let table: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='accounting_cache_frontier')", [], |r| r.get(0))?;
    if !table { return Ok(false); }
    let invalidated: Option<String> = db.query_row("SELECT invalidated FROM accounting_stream WHERE singleton=1", [], |r| r.get(0))?;
    // Unrelated tool/attention mutations and an analytics-only upgrade do
    // not change cache usage. Native input generations still must match.
    if invalidated.is_some_and(|r| r != "schema_upgrade") { return Ok(false); }
    let Some(inputs) = inputs(db)? else { return Ok(false); };
    Ok(db.query_row("SELECT 1 FROM accounting_cache_frontier WHERE singleton=1 AND inputs=?1", [inputs], |_| Ok(())).optional()?.is_some())
}

/// Read summaries only. The verifier independently replays native records.
/// Uncommitted/missing frontiers require sync, rather than serving stale totals.
pub(crate) fn metric(project: &Path, since: Option<i64>, aggregates: bool) -> Result<Value> {
    let Some(db) = crate::telemetry::sidecar::read(project)? else { return Ok(unavailable("no_certified_source")); };
    let _snapshot = db.unchecked_transaction()?;
    if !current(&db)? { return Ok(unavailable("accounting_sync_required")); }
    evaluate(project, &db, since, aggregates)
}

fn evaluate(project: &Path, db: &Connection, since: Option<i64>, aggregates: bool) -> Result<Value> {
    let canonical = crate::telemetry::read_only(&project.join(".state/state.db"))?;
    let known: BTreeMap<String, Option<String>> = canonical.prepare("SELECT a.id,d.chosen_configuration_id FROM attempts a
        LEFT JOIN dispatch_decisions d ON d.attempt_id=a.id")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut totals = BTreeMap::<String, [i64; 4]>::new();
    if aggregates {
        for row in db.prepare("SELECT session_id,input_tokens,cache_read_tokens,cache_write_tokens,records FROM accounting_cache_totals")?
            .query_map([], |r| Ok((r.get(0)?, [r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?])))? {
            let (session, tally) = row?; totals.insert(session, tally);
        }
    } else {
        for entry in super::ledger::derive(db)?.iter().filter(|e| e.counted()) {
            if let Some(n) = entry.normalized {
                let total = totals.entry(entry.session.clone()).or_default();
                total[0] += n[0]; total[1] += n[1]; total[2] += n[3]; total[3] += 1;
            }
        }
    }
    let sql = if aggregates {
        "SELECT s.session_id,s.binding,s.attempt_id,CASE WHEN a.uncertified THEN '' ELSE s.cli_version END,
        a.quarantined,s.session_unix_ms,a.rejected FROM rollout_sources s JOIN accounting_source_summary a USING(path_digest) ORDER BY s.path_digest"
    } else {
        "SELECT s.session_id,s.binding,s.attempt_id,
        CASE WHEN EXISTS(SELECT 1 FROM codex_usage u WHERE u.path_digest=s.path_digest AND u.reason='cli_version_uncertified') THEN '' ELSE s.cli_version END,
        EXISTS(SELECT 1 FROM codex_quarantine q WHERE q.session_id=s.session_id),s.session_unix_ms,
        EXISTS(SELECT 1 FROM codex_usage u WHERE u.session_id=s.session_id AND u.reason='invariant_violation') FROM rollout_sources s ORDER BY s.path_digest"
    };
    let mut overall = Tally::default();
    let mut arms = BTreeMap::<String, Tally>::new();
    let mut unknown = Tally::default();
    let mut excluded_sessions = BTreeSet::new();
    let losing = super::otlp::losing_sessions(db)?;
    let mut eligible = BTreeMap::<String, (Option<String>, [i64; 4])>::new();
    let mut rows = db.prepare(sql)?;
    let mut rows = rows.query([])?;
    while let Some(row) = rows.next()? {
        let session: String = row.get(0)?;
        let binding: String = row.get(1)?;
        let attempt: Option<String> = row.get(2)?;
        let version: String = row.get(3)?;
        let quarantined: bool = row.get(4)?;
        let at: Option<i64> = row.get(5)?;
        let rejected: bool = row.get(6)?;
        if since.is_some_and(|s| at.is_none_or(|at| at < s)) { continue; }
        let arm = attempt.as_ref().and_then(|a| known.get(a)).cloned().flatten();
        let total = totals.get(&session).copied().unwrap_or_default();
        let reason = if binding != "bound" { Some(binding.as_str()) }
            else if !attempt.as_ref().is_some_and(|a| known.contains_key(a)) { Some("orphan") }
            else if losing.contains(&session) { Some("native_surface_precedence") }
            else if quarantined { Some("quarantined") }
            else if session.starts_with("gemini-cli:") { Some("cache_denominator_not_reconciled") }
            else if !crate::telemetry::codex::accepted_version(&version) { Some("cli_version_uncertified") }
            else if rejected { Some("records_not_accepted") }
            else if total[3] == 0 { Some("cache_counters_not_reported") }
            else { None };
        if let Some(reason) = reason {
            if excluded_sessions.insert(session.clone()) {
                *overall.excluded.entry(reason.to_owned()).or_default() += 1;
                let tally = arm.map(|a| arms.entry(a).or_default()).unwrap_or(&mut unknown);
                *tally.excluded.entry(reason.to_owned()).or_default() += 1;
            }
        } else {
            eligible.entry(session).or_insert((arm, total));
        }
    }
    // As with M08, a session qualifies through any certified bound source;
    // resumed sources never multiply its usage.
    for (session, (arm, total)) in eligible {
        overall.include(&session, total);
        arm.map(|a| arms.entry(a).or_default()).unwrap_or(&mut unknown).include(&session, total);
    }
    let mut body = overall.body();
    body["definition"] = json!("M10.v1"); body["name"] = json!("cache_read_share");
    body["by_configuration"] = json!({"configurations": arms.iter().map(|(a,t)| (a.clone(),t.body())).collect::<BTreeMap<_,_>>(),
        "configuration_unknown": unknown.body(), "label": "observational", "estimator": "ratio_of_token_sums.v1"});
    Ok(body)
}

/// M10 is an activity consumption comparison, distinct from terminal success
/// rates. It has no inherent higher-is-better direction or monetary meaning.
pub(crate) fn compare(project: &Path, args: &crate::telemetry::analytics::compare::Args) -> Result<Value> {
    if args.by != "configuration" || args.cohort.as_deref().is_some_and(|c| c != "activity_window")
        || args.to.is_some() || args.horizon_ms.is_some() || args.task_class.is_some() || args.seed.is_some() {
        anyhow::bail!("compare rejected: M10 supports configuration, activity_window and --from only");
    }
    let body = metric(project, args.from, true)?;
    Ok(json!({"contract": "analytics-cache-comparison.v1", "registry": crate::telemetry::analytics::registry::VERSION,
        "request": {"metrics": ["M10.v1"], "by": "configuration", "cohort": "activity_window", "from": args.from},
        "label": "observational", "estimator": "ratio_of_token_sums.v1", "metric": body,
        "configurations": body["by_configuration"]["configurations"], "configuration_unknown": body["by_configuration"]["configuration_unknown"],
        "routing": "never: advisory evidence only; nothing here is read by dispatch or admission"}))
}

//! Contracts §6 metric subset: a read-only report over `state.db` and the
//! sidecar. Unknown is never 0: a ratio with an empty denominator is `null`
//! with `empty_denominator`; a value without a source is `unavailable`.
use anyhow::Result;
use rusqlite::Connection;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

const TERMINAL: [&str; 4] = ["completed", "failed", "cancelled", "lost"];
const NAMES: [(&str, &str); 10] = [("M02", "task_acceptance_rate"), ("M07", "attempt_amplification"), ("M08", "input_tokens"), ("M09", "output_tokens"),
    ("M13", "usage_coverage"), ("M15", "effective_model_coverage"), ("M31", "attention"), ("M32", "attention"), ("M33", "attention"), ("M40", "quota_headroom_at_dispatch")];

struct Attempt { id: String, task: String, state: String, kind: Option<String>, home: Option<String>, decided: Option<i64> }

fn unavailable(reason: &str) -> Value {
    json!({"status": "unavailable", "reason": reason})
}

fn metric(id: &str, mut body: Value) -> Value {
    body["definition"] = json!(format!("{id}.slice-v1"));
    body
}

fn ratio(id: &str, numerator: usize, denominator: usize, extra: Value) -> Value {
    let mut body = json!({"numerator": numerator, "denominator": denominator});
    if denominator == 0 { body["value"] = Value::Null; body["reason"] = json!("empty_denominator"); } else { body["value"] = json!(format!("{numerator}/{denominator}")); }
    if let (Value::Object(body), Value::Object(extra)) = (&mut body, extra) { body.extend(extra); }
    metric(id, body)
}

/// Contracts §6 `T`/`A` evidence: every task `(id, state, accepted)`, where
/// accepted means a verified result for its current contract revision that is
/// verify-only or integrated. Shared by this report and the attention M31 cohort.
pub(crate) fn task_evidence(db: &Connection) -> Result<Vec<(String, String, bool)>> {
    Ok(db.prepare("SELECT t.id,t.state,EXISTS(SELECT 1 FROM task_contracts c JOIN result_submissions s ON s.task_id=c.task_id AND s.contract_revision=c.contract_revision
        JOIN verified_results r ON r.submission_id=s.submission_id WHERE c.task_id=t.id AND c.contract_revision=(SELECT max(contract_revision) FROM task_contracts WHERE task_id=t.id)
        AND (c.route='verify_only' OR EXISTS(SELECT 1 FROM integration_operations i JOIN integrated_commits k ON k.operation_id=i.operation_id WHERE i.verified_result_id=r.result_id)))
        FROM tasks t ORDER BY t.id")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?)
}

/// `herdr-projects telemetry <slug> report`. `since` bounds the activity window (Unix ms).
/// Assembled by the query service (`super::analytics::query::report`), the one
/// read path shared with `telemetry query`, the fleet pane and exports.
pub fn report(project: &Path, since: Option<i64>) -> Result<Value> { super::analytics::query::report(project, since) }

/// The central slice metrics (contracts §6) and the `tasks` summary, before
/// the lane providers (`super::LANES`) add theirs.
pub(crate) fn central(project: &Path, since: Option<i64>) -> Result<(BTreeMap<String, Value>, Value)> {
    if let Some(db) = super::sidecar::read(project)? && let Some(generations) = super::analytics::inputs::generations(&db)? {
        let stamp = super::analytics::inputs::stamp("central", &super::analytics::inputs::canonical(project)?, &generations);
        if let Some(body) = super::analytics::inputs::cached(&db, "central", since, &stamp)? {
            let mut metrics: BTreeMap<String, Value> = match body {
                Value::Object(values) => values.into_iter().collect(),
                body => serde_json::from_value(body)?,
            };
            if let Some(tasks) = metrics.remove("_tasks") { return Ok((metrics, tasks)); }
        }
    }
    central_uncached(project, since, true)
}

pub(crate) fn central_uncached(project: &Path, since: Option<i64>, aggregates: bool) -> Result<(BTreeMap<String, Value>, Value)> {
    let path = project.join(".state/state.db");
    let db = super::read_only(&path)?;
    let table = |name: &str| db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)", [name], |r| r.get::<_, bool>(0));
    let decided = if table("dispatch_decisions")? { "(SELECT decided_unix_ms FROM dispatch_decisions d WHERE d.attempt_id=a.id)" } else { "NULL" };
    let attempts: Vec<Attempt> = db.prepare(&format!("SELECT a.id,a.task_id,a.state,json_extract(i.payload,'$.inputs.effective_profile.kind'),
        json_extract(i.payload,'$.inputs.effective_profile.execution_home'),{decided} FROM attempts a LEFT JOIN attempt_inputs i ON i.attempt_id=a.id ORDER BY a.rowid"))?
        .query_map([], |r| Ok(Attempt { id: r.get(0)?, task: r.get(1)?, state: r.get(2)?, kind: r.get(3)?, home: r.get(4)?, decided: r.get(5)? }))?
        .collect::<rusqlite::Result<_>>()?;
    let in_window = |a: &Attempt| since.is_none_or(|since| a.decided.is_some_and(|at| at >= since));

    // Contracts §6 `T` and `A`: evidence for the task's current contract revision.
    // Replay candidates are evaluation artefacts (M49 only), never in `T`.
    let tasks = task_evidence(&db)?;
    let replay: BTreeSet<String> = if table("replay_candidates")? {
        db.prepare("SELECT task_id FROM replay_candidates")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
    } else { BTreeSet::new() };
    let (mut terminal, mut accepted, mut open, mut without_evidence, mut outside, mut replayed) = (BTreeSet::new(), 0, 0, 0, 0, 0);
    for (task, state, evidence) in &tasks {
        if replay.contains(task) { replayed += 1; continue; }
        if since.is_some() && !attempts.iter().any(|a| &a.task == task && in_window(a)) { outside += 1; continue; }
        if *evidence || ["succeeded", "failed", "cancelled"].contains(&state.as_str()) {
            terminal.insert(task.as_str());
            if *evidence { accepted += 1; } else if state == "succeeded" { without_evidence += 1; }
        } else { open += 1; }
    }
    let cohort: Vec<&Attempt> = attempts.iter().filter(|a| terminal.contains(a.task.as_str())).collect();
    let mut metrics = BTreeMap::new();
    metrics.insert("M02", ratio("M02", accepted, terminal.len(), json!({"excluded": {"open": open, "outside_window": outside, "replay_candidate": replayed}})));
    metrics.insert("M07", ratio("M07", cohort.len(), accepted, json!({"attempts_without_decision": cohort.iter().filter(|a| a.decided.is_none()).count(),
        "excluded": {"replay_candidate": replayed}})));

    let sidecar = super::sidecar::read(project)?;
    let _snapshot = sidecar.as_deref().filter(|db| db.is_autocommit()).map(|db| db.unchecked_transaction()).transpose()?;
    usage_metrics(sidecar.as_deref(), &attempts, since, &in_window, &mut metrics, aggregates)?;
    for id in ["M31", "M32", "M33"] { metrics.insert(id, metric(id, json!({"value": unavailable("attention_not_collected")}))); }
    metrics.insert("M40", headroom(project, sidecar.as_deref(), &attempts, &in_window, aggregates)?);
    #[cfg(target_os = "linux")]
    metrics.insert("M49", crate::replay::m49(&db, since)?);
    for (id, name) in NAMES { if let Some(m) = metrics.get_mut(id) { m["name"] = json!(name); } }
    let metrics: BTreeMap<String, Value> = metrics.into_iter().map(|(id, m)| (id.to_owned(), m)).collect();
    Ok((metrics, json!({"accepted": accepted, "open": open, "succeeded_without_evidence": without_evidence, "terminal": terminal.len(), "replay_candidates": replayed})))
}

/// M08, M09, M15 over certified bound sessions (activity window by session
/// start) and M13 over terminated attempts decided in the window.
fn usage_metrics(sidecar: Option<&Connection>, attempts: &[Attempt], since: Option<i64>, in_window: &dyn Fn(&Attempt) -> bool, metrics: &mut BTreeMap<&str, Value>, aggregates: bool) -> Result<()> {
    let terminated: Vec<&Attempt> = attempts.iter().filter(|a| TERMINAL.contains(&a.state.as_str()) && in_window(a)).collect();
    let adapter_absent = terminated.iter().filter(|a| a.kind.as_deref().is_some_and(|k| !matches!(k, "codex" | "claude" | "opencode"))).count();
    let codex: Vec<&&Attempt> = terminated.iter().filter(|a| matches!(a.kind.as_deref(), Some("codex" | "claude" | "opencode"))).collect();
    let Some(db) = sidecar else {
        for id in ["M08", "M09", "M15"] { metrics.insert(id, metric(id, json!({"value": unavailable("no_certified_source")}))); }
        metrics.insert("M13", metric("M13", json!({"value": unavailable("collection_not_run"), "adapter_absent": adapter_absent})));
        return Ok(());
    };
    // (session, binding, attempt, certified, quarantined, records, accepted records, session start)
    type Source = (String, String, Option<String>, bool, bool, i64, i64, Option<i64>);
    // Records collected before their version was certified keep NULL counters:
    // such a source stays uncertified (see `sidecar::attempt_usage`).
    let sql = if aggregates && super::accounting::ledger::aggregates_current(db)? {
        "SELECT s.session_id,s.binding,s.attempt_id,CASE WHEN a.uncertified THEN '' ELSE s.cli_version END,
        a.quarantined,s.records,a.accepted_records,s.session_unix_ms FROM rollout_sources s
        JOIN accounting_source_summary a USING(path_digest) ORDER BY s.path_digest"
    } else {
        "SELECT s.session_id,s.binding,s.attempt_id,
        CASE WHEN EXISTS(SELECT 1 FROM codex_usage u WHERE u.path_digest=s.path_digest AND u.reason='cli_version_uncertified') THEN '' ELSE s.cli_version END,EXISTS(SELECT 1 FROM codex_quarantine q WHERE q.session_id=s.session_id),
        s.records,(SELECT count(*) FROM codex_usage u WHERE u.path_digest=s.path_digest AND u.accepted=1),s.session_unix_ms FROM rollout_sources s ORDER BY s.path_digest"
    };
    let sources: Vec<Source> = db.prepare(sql)?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, super::codex::accepted_version(&r.get::<_, String>(3)?), r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let known: BTreeSet<&str> = attempts.iter().map(|a| a.id.as_str()).collect();
    let (mut certified, mut excluded) = (BTreeSet::new(), BTreeMap::<&str, usize>::new());
    for s in sources.iter().filter(|s| since.is_none_or(|since| s.7.is_some_and(|at| at >= since))) {
        let reason = match s {
            (_, binding, ..) if binding != "bound" => binding.as_str(),
            (_, _, attempt, ..) if !attempt.as_deref().is_some_and(|a| known.contains(a)) => "orphan",
            (.., true, _, _, _) => "quarantined",
            (_, _, _, false, ..) => "cli_version_uncertified",
            _ => { certified.insert(s.0.as_str()); continue; }
        };
        *excluded.entry(reason).or_default() += 1;
    }
    let coverage = json!({"certified_sessions": certified.len(), "excluded": excluded});
    if certified.is_empty() {
        for id in ["M08", "M09", "M15"] { metrics.insert(id, metric(id, json!({"value": unavailable("no_certified_source"), "coverage": coverage}))); }
    } else {
        let mut sums = [0i64; 5];
        let sql = if aggregates && super::accounting::ledger::aggregates_current(db)? {
            "SELECT input_tokens,output_tokens,reasoning_tokens,records,models FROM accounting_native_totals WHERE session_id=?1"
        } else {
            "SELECT coalesce(sum(input_tokens),0),coalesce(sum(output_tokens),0),coalesce(sum(reasoning_output_tokens),0),count(*),count(model)
                FROM codex_usage WHERE session_id=?1 AND accepted=1 AND NOT EXISTS(SELECT 1 FROM codex_usage e WHERE e.session_id=codex_usage.session_id AND e.accepted=1 AND e.response_id IS NOT NULL AND e.response_id=codex_usage.response_id AND e.payload_digest=codex_usage.payload_digest AND e.ordinal<codex_usage.ordinal)"
        };
        let mut totals = db.prepare(sql)?;
        for session in &certified {
            let row: [i64; 5] = totals.query_row([session], |r| Ok([r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?]))?;
            for (sum, value) in sums.iter_mut().zip(row) { *sum += value; }
        }
        metrics.insert("M08", metric("M08", json!({"value": sums[0], "coverage": coverage})));
        let reasoning = if certified.iter().any(|s| s.starts_with("claude-code:")) { unavailable("reasoning_tokens_not_reported") } else { json!(sums[2]) };
        metrics.insert("M09", metric("M09", json!({"value": sums[1], "reasoning_output_tokens": reasoning, "coverage": coverage})));
        metrics.insert("M15", ratio("M15", sums[4] as usize, sums[3] as usize, json!({"coverage": coverage})));
    }
    // Group once: searching all rollouts separately for each retained attempt
    // makes coverage quadratic in the binding count.
    let mut by_attempt = BTreeMap::<&str, Vec<&Source>>::new();
    for source in &sources {
        if source.1 == "bound" && let Some(attempt) = source.2.as_deref() { by_attempt.entry(attempt).or_default().push(source); }
    }
    let mut incomplete = BTreeMap::<&str, usize>::new();
    for a in &codex {
        let bound = by_attempt.get(a.id.as_str()).map(Vec::as_slice).unwrap_or_default();
        let reason = if bound.is_empty() { "not_bound" } else if bound.iter().any(|s| s.4) { "quarantined" }
            else if bound.iter().any(|s| !s.3) { "cli_version_uncertified" } else if bound.iter().any(|s| s.5 != s.6) { "records_not_accepted" } else { continue };
        *incomplete.entry(reason).or_default() += 1;
    }
    let complete = codex.len() - incomplete.values().sum::<usize>();
    metrics.insert("M13", ratio("M13", complete, codex.len(), json!({"adapter_absent": adapter_absent, "incomplete": incomplete})));
    Ok(())
}

/// Extended M40 (contracts-accounting §5, `M40.quota-windows-v1`): per
/// decision in the window and per limit window, the latest trusted remaining
/// value from the quota tables the last `accounting sync` built, never summed
/// across accounts, limits or services. Each entry equals the one `accounting
/// quota` prints; before a sync every Codex decision is `ledger_not_synced`.
fn headroom(project: &Path, sidecar: Option<&Connection>, attempts: &[Attempt], in_window: &dyn Fn(&Attempt) -> bool, aggregates: bool) -> Result<Value> {
    use super::accounting::quota;
    let synced = match sidecar { Some(db) => quota::synced(db)?, None => false };
    let aggregate = match sidecar { Some(db) => aggregates && quota::dispatch_current(project, db)?, None => false };
    let mut decisions = Vec::new();
    for a in attempts.iter().filter(|a| in_window(a)) {
        let Some(decided) = a.decided else { continue };
        let mut entry = match (a.kind.as_deref(), &a.home, sidecar) {
            (Some(kind), ..) if kind != "codex" => json!({"value": unavailable("adapter_absent")}),
            (_, None, _) => json!({"value": unavailable("execution_home_unknown")}),
            (.., None) => json!({"value": unavailable("collection_not_run")}),
            _ if !synced => json!({"value": unavailable("ledger_not_synced")}),
            (_, Some(home), Some(db)) => match if aggregate { quota::stored_headroom(db, &a.id, home, decided)? } else { None } {
                Some(body) => body,
                None => quota::headroom(db, home, decided)?,
            },
        };
        entry["attempt_id"] = json!(a.id);
        entry["decided_unix_ms"] = json!(decided);
        entry["service"] = json!("codex");
        decisions.push(entry);
    }
    let mut m40 = metric("M40", json!({"decisions": decisions, "stale_after_ms": quota::STALE_AFTER_MS}));
    m40["definition"] = json!("M40.quota-windows-v1");
    Ok(m40)
}

/// A structured metric value as one line of text, or `None` when it is not
/// one (an unavailable value carries a `reason`). M16 (`M16.tools-v1`):
/// `issued N, accepted K <status> (U unknown), executed E`, as `accounting
/// tools` prints it, never `n/a` while the counts are known.
pub fn structured_text(value: &Value) -> Option<String> {
    let o = value.as_object()?;
    if o.contains_key("reason") { return None; }
    let issued = o.get("issued")?.as_u64()?;
    let accepted = &value["accepted"];
    Some(format!("issued {issued}, accepted {} {} ({} unknown), executed {}", accepted["count"], accepted["status"].as_str().unwrap_or("unknown"),
        accepted["unknown"], value["executed"]))
}

/// One line per metric (M40: one per decision and limit window); anything unknown reads `n/a`, never 0.
pub fn text(report: &Value) -> String {
    let show = |m: &Value| match &m["value"] {
        Value::Null => format!("n/a ({})", m["reason"].as_str().unwrap_or("unknown")),
        Value::Object(o) => structured_text(&m["value"]).unwrap_or_else(|| format!("n/a ({})", o.get("reason").and_then(Value::as_str).unwrap_or("unknown"))),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let mut out = String::new();
    // Every metric in the report, in id order (central and lane-provided alike);
    // a metric's own `name` wins, else the central name.
    let Some(metrics) = report["metrics"].as_object() else { return out };
    for (id, m) in metrics {
        let id = id.as_str();
        let name = m["name"].as_str().or_else(|| NAMES.iter().find(|(n, _)| *n == id).map(|(_, name)| *name)).unwrap_or("");
        match m["decisions"].as_array() {
            Some(list) if list.is_empty() => out += &format!("{id} {name} n/a (no_decisions)\n"),
            // M40: one line per decision and limit window, or per decision without windows.
            Some(list) => for d in list {
                let attempt = d["attempt_id"].as_str().unwrap_or("");
                let Some(windows) = d["windows"].as_array() else { out += &format!("{id} {name} {attempt} {}\n", show(d)); continue };
                for w in windows {
                    let age = w["age_ms"].as_i64().map(|age| format!(" age_ms={age}")).unwrap_or_default();
                    let value = match w["value"].as_str() {
                        Some(remaining) => format!("remaining {remaining}%{age} {}", w["freshness"].as_str().unwrap_or("")),
                        None => format!("{}{age}", show(w)),
                    };
                    out += &format!("{id} {name} {attempt} {} {} {value}\n", w["limit_id"].as_str().unwrap_or(""), w["window_kind"].as_str().unwrap_or(""));
                }
            },
            None => out += &format!("{id} {name} {}\n", show(m)),
        }
    }
    for a in report["after_termination"].as_array().into_iter().flatten() {
        out += &format!("attempt {} after_termination records={} first_unix_ms={} terminated_unix_ms={}; still counted in M08\n",
            a["attempt_id"].as_str().unwrap_or("-"), a["after_termination"]["records"], a["after_termination"]["first_unix_ms"], a["after_termination"]["terminated_unix_ms"]);
    }
    let t = &report["tasks"];
    out + &format!("tasks terminal={} accepted={} open={} succeeded_without_evidence={}\n", t["terminal"], t["accepted"], t["open"], t["succeeded_without_evidence"])
}

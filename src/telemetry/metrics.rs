//! Contracts §6 metric subset: a read-only report over `state.db` and the
//! sidecar. Unknown is never 0: a ratio with an empty denominator is `null`
//! with `empty_denominator`; a value without a source is `unavailable`.
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};
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

/// `herdr-projects telemetry <slug> report`. `since` bounds the activity window (Unix ms).
pub fn report(project: &Path, since: Option<i64>) -> Result<Value> {
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
    let tasks: Vec<(String, String, bool)> = db.prepare("SELECT t.id,t.state,EXISTS(SELECT 1 FROM task_contracts c JOIN result_submissions s ON s.task_id=c.task_id AND s.contract_revision=c.contract_revision
        JOIN verified_results r ON r.submission_id=s.submission_id WHERE c.task_id=t.id AND c.contract_revision=(SELECT max(contract_revision) FROM task_contracts WHERE task_id=t.id)
        AND (c.route='verify_only' OR EXISTS(SELECT 1 FROM integration_operations i JOIN integrated_commits k ON k.operation_id=i.operation_id WHERE i.verified_result_id=r.result_id)))
        FROM tasks t ORDER BY t.id")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
    let (mut terminal, mut accepted, mut open, mut without_evidence, mut outside) = (BTreeSet::new(), 0, 0, 0, 0);
    for (task, state, evidence) in &tasks {
        if since.is_some() && !attempts.iter().any(|a| &a.task == task && in_window(a)) { outside += 1; continue; }
        if *evidence || ["succeeded", "failed", "cancelled"].contains(&state.as_str()) {
            terminal.insert(task.as_str());
            if *evidence { accepted += 1; } else if state == "succeeded" { without_evidence += 1; }
        } else { open += 1; }
    }
    let cohort: Vec<&Attempt> = attempts.iter().filter(|a| terminal.contains(a.task.as_str())).collect();
    let mut metrics = BTreeMap::new();
    metrics.insert("M02", ratio("M02", accepted, terminal.len(), json!({"excluded": {"open": open, "outside_window": outside}})));
    metrics.insert("M07", ratio("M07", cohort.len(), accepted, json!({"attempts_without_decision": cohort.iter().filter(|a| a.decided.is_none()).count()})));

    let sidecar = super::sidecar::read(project)?;
    usage_metrics(sidecar.as_deref(), &attempts, since, &in_window, &mut metrics)?;
    for id in ["M31", "M32", "M33"] { metrics.insert(id, metric(id, json!({"value": unavailable("attention_not_collected")}))); }
    let mut decisions = Vec::new();
    for a in attempts.iter().filter(|a| a.decided.is_some() && in_window(a)) {
        let decided = a.decided.unwrap_or_default();
        let mut entry = json!({"attempt_id": a.id, "decided_unix_ms": decided});
        match (&sidecar, a.kind.as_deref(), &a.home) {
            (_, Some(kind), _) if kind != "codex" => entry["value"] = unavailable("adapter_absent"),
            (None, ..) => entry["value"] = unavailable("collection_not_run"),
            (Some(db), _, Some(home)) => headroom(db, home, decided, &mut entry)?,
            _ => entry["value"] = unavailable("no_observation"),
        }
        decisions.push(entry);
    }
    metrics.insert("M40", metric("M40", json!({"decisions": decisions})));
    for (id, name) in NAMES { if let Some(m) = metrics.get_mut(id) { m["name"] = json!(name); } }
    // Lane providers (`super::LANES`) add metrics; a lane key replaces a central one.
    let mut metrics: BTreeMap<String, Value> = metrics.into_iter().map(|(id, m)| (id.to_owned(), m)).collect();
    for lane in &super::LANES { metrics.extend((lane.metrics)(project, since)?); }
    Ok(json!({"metrics": metrics, "since_unix_ms": since,
        "tasks": {"accepted": accepted, "open": open, "succeeded_without_evidence": without_evidence, "terminal": terminal.len()}}))
}

/// M08, M09, M15 over certified bound sessions (activity window by session
/// start) and M13 over terminated attempts decided in the window.
fn usage_metrics(sidecar: Option<&Connection>, attempts: &[Attempt], since: Option<i64>, in_window: &dyn Fn(&Attempt) -> bool, metrics: &mut BTreeMap<&str, Value>) -> Result<()> {
    let terminated: Vec<&Attempt> = attempts.iter().filter(|a| TERMINAL.contains(&a.state.as_str()) && in_window(a)).collect();
    let adapter_absent = terminated.iter().filter(|a| a.kind.as_deref().is_some_and(|k| k != "codex")).count();
    let codex: Vec<&&Attempt> = terminated.iter().filter(|a| a.kind.as_deref() == Some("codex")).collect();
    let Some(db) = sidecar else {
        for id in ["M08", "M09", "M15"] { metrics.insert(id, metric(id, json!({"value": unavailable("no_certified_source")}))); }
        metrics.insert("M13", metric("M13", json!({"value": unavailable("collection_not_run"), "adapter_absent": adapter_absent})));
        return Ok(());
    };
    // (session, binding, attempt, certified, quarantined, records, accepted records, session start)
    type Source = (String, String, Option<String>, bool, bool, i64, i64, Option<i64>);
    // Records collected before their version was certified keep NULL counters:
    // such a source stays uncertified (see `sidecar::attempt_usage`).
    let sources: Vec<Source> = db.prepare("SELECT s.session_id,s.binding,s.attempt_id,
        CASE WHEN EXISTS(SELECT 1 FROM codex_usage u WHERE u.path_digest=s.path_digest AND u.reason='cli_version_uncertified') THEN '' ELSE s.cli_version END,EXISTS(SELECT 1 FROM codex_quarantine q WHERE q.session_id=s.session_id),
        s.records,(SELECT count(*) FROM codex_usage u WHERE u.path_digest=s.path_digest AND u.accepted=1),s.session_unix_ms FROM rollout_sources s ORDER BY s.path_digest")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, super::codex::certified(&r.get::<_, String>(3)?), r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)))?
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
        for session in &certified {
            let row: [i64; 5] = db.query_row("SELECT coalesce(sum(input_tokens),0),coalesce(sum(output_tokens),0),coalesce(sum(reasoning_output_tokens),0),count(*),count(model)
                FROM codex_usage WHERE session_id=?1 AND accepted=1", [session], |r| Ok([r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?]))?;
            for (sum, value) in sums.iter_mut().zip(row) { *sum += value; }
        }
        metrics.insert("M08", metric("M08", json!({"value": sums[0], "coverage": coverage})));
        metrics.insert("M09", metric("M09", json!({"value": sums[1], "reasoning_output_tokens": sums[2], "coverage": coverage})));
        metrics.insert("M15", ratio("M15", sums[4] as usize, sums[3] as usize, json!({"coverage": coverage})));
    }
    let mut incomplete = BTreeMap::<&str, usize>::new();
    for a in &codex {
        let bound: Vec<&Source> = sources.iter().filter(|s| s.1 == "bound" && s.2.as_deref() == Some(a.id.as_str())).collect();
        let reason = if bound.is_empty() { "not_bound" } else if bound.iter().any(|s| s.4) { "quarantined" }
            else if bound.iter().any(|s| !s.3) { "cli_version_uncertified" } else if bound.iter().any(|s| s.5 != s.6) { "records_not_accepted" } else { continue };
        *incomplete.entry(reason).or_default() += 1;
    }
    let complete = codex.len() - incomplete.values().sum::<usize>();
    metrics.insert("M13", ratio("M13", complete, codex.len(), json!({"adapter_absent": adapter_absent, "incomplete": incomplete})));
    Ok(())
}

/// M40: `100 − used_percent` of the latest rate-limit row from the attempt's
/// execution home observed at or before the decision, exact decimal arithmetic.
fn headroom(db: &Connection, home: &str, decided: i64, entry: &mut Value) -> Result<()> {
    let home = format!("sha256:{:x}", <sha2::Sha256 as sha2::Digest>::digest(home.as_bytes()));
    let row: Option<(Option<String>, i64, Option<String>, Option<i64>)> = db.query_row("SELECT l.used_percent,l.observed_ts,l.limit_id,l.window_minutes FROM codex_rate_limits l
        WHERE l.observed_ts<=?2 AND EXISTS(SELECT 1 FROM rollout_sources s WHERE s.session_id=l.session_id AND s.home_digest=?1)
        ORDER BY l.observed_ts DESC,l.ordinal DESC LIMIT 1", rusqlite::params![home, decided], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()?;
    let Some((used, observed, limit, window)) = row else { entry["value"] = unavailable("no_observation"); return Ok(()) };
    match used.as_deref().and_then(subtract_from_100) {
        None => entry["value"] = unavailable("unparseable_observation"),
        Some(value) => { entry["value"] = json!(value); entry["age_ms"] = json!(decided - observed); entry["limit_id"] = json!(limit); entry["window_minutes"] = json!(window); }
    }
    Ok(())
}

fn subtract_from_100(used: &str) -> Option<String> {
    let (int, frac) = used.split_once('.').unwrap_or((used, ""));
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if int.is_empty() || int.len() > 6 || frac.len() > 12 || !digits(int) || !digits(frac) || used.ends_with('.') { return None; }
    let scale = 10i64.pow(frac.len() as u32);
    let left = 100 * scale - (int.parse::<i64>().ok()? * scale + if frac.is_empty() { 0 } else { frac.parse::<i64>().ok()? });
    let (sign, abs) = if left < 0 { ("-", -left) } else { ("", left) };
    Some(if frac.is_empty() { format!("{sign}{abs}") } else { format!("{sign}{}.{:0w$}", abs / scale, abs % scale, w = frac.len()) })
}

/// One line per metric (M40: one per decision); anything unknown reads `n/a`, never 0.
pub fn text(report: &Value) -> String {
    let show = |m: &Value| match &m["value"] {
        Value::Null => format!("n/a ({})", m["reason"].as_str().unwrap_or("unknown")),
        Value::Object(o) => format!("n/a ({})", o.get("reason").and_then(Value::as_str).unwrap_or("unknown")),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let mut out = String::new();
    for (id, name) in NAMES {
        let m = &report["metrics"][id];
        match m["decisions"].as_array() {
            Some(list) if list.is_empty() => out += &format!("{id} {name} n/a (no_decisions)\n"),
            Some(list) => for d in list {
                let age = d["age_ms"].as_i64().map(|age| format!(" age_ms={age}")).unwrap_or_default();
                out += &format!("{id} {name} {} {}{age}\n", d["attempt_id"].as_str().unwrap_or(""), show(d));
            },
            None => out += &format!("{id} {name} {}\n", show(m)),
        }
    }
    let t = &report["tasks"];
    out + &format!("tasks terminal={} accepted={} open={} succeeded_without_evidence={}\n", t["terminal"], t["accepted"], t["open"], t["succeeded_without_evidence"])
}

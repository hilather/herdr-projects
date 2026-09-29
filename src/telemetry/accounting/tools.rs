//! Tool calls and executions (docs/telemetry/contracts-accounting.md §9; plan
//! TM2.5, doc 07 M16–M18), derived at read time from lane A's A6 metadata
//! (contracts-collection.md A6: `codex_tool_calls`, `codex_exec_items`,
//! `codex_tool_sources`, read by SQL only; never any content). Nothing is
//! stored. Codex 0.154.0 writes no approval decision and no execution run
//! time: the accepted stage is only inferred (a B6b `blocked` wait or a
//! guardian review around the call, labelled `inferred`; neither is unknown),
//! M18 is `unavailable` with the reason, and the call → output wall time
//! (approval wait included) is shown apart, per tool and per host, never as M18. Unknown is never 0: a session whose tool metadata was not
//! read yet makes the metrics `unavailable`, and an execution without a
//! known outcome is counted apart, never as a success.
use anyhow::Result;
use rusqlite::Connection;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::unavailable;

/// The only exec item status certified live (codex-live-0.154.0-a4.md §4).
const CERTIFIED_STATUS: &str = "completed";
const NAMES: [(&str, &str); 3] = [("M16", "tool_call_volume"), ("M17", "tool_execution_success"), ("M18", "tool_latency_p95")];

struct Call { id: String, recorded: bool, name: Option<String>, status: Option<String>, turn: Option<String>, called: Option<i64>, output: Option<i64> }

struct Exec { turn: Option<String>, status: Option<String>, source: Option<String>, exit: Option<i64>, completed: Option<i64> }

/// A session with a bound rollout (the rollout's own `session_meta.id`), with
/// the evidence the accepted stage is inferred from: the `blocked` wait spans
/// of its attempts (B6b, §6) and the start times of guardian sessions naming
/// it as their parent; and the execution homes of its rollouts (the host).
struct Session {
    id: String,
    attempts: BTreeSet<String>,
    tools: std::result::Result<(Vec<Call>, Vec<Exec>), &'static str>,
    waits: Vec<(i64, i64)>,
    guardians: Vec<i64>,
    homes: BTreeSet<String>,
}

impl Session {
    /// The stage a call reached past `issued` (inferred; never from a typed
    /// decision): a call whose call → output interval overlaps a `blocked`
    /// wait of the same attempt was routed to the human (`human_routed`); a
    /// guardian session of this session started inside it was reviewed
    /// automatically (`auto_review`). Neither (or no output yet): unknown.
    fn accepted(&self, c: &Call) -> Option<&'static str> {
        let (called, output) = (c.called?, c.output?);
        if output < called { return None; }
        let human = self.waits.iter().any(|(opened, last)| *opened <= output && called <= *last);
        let auto = self.guardians.iter().any(|at| called <= *at && *at <= output);
        match (human, auto) {
            (true, true) => Some("human_routed_and_auto_review"),
            (true, false) => Some("human_routed"),
            (false, true) => Some("auto_review"),
            (false, false) => None,
        }
    }

    /// The host of its calls: the one execution home of its rollouts, else ambiguous.
    fn home(&self) -> Option<&str> { if self.homes.len() == 1 { self.homes.first().map(String::as_str) } else { None } }
}

/// Outcome class of one exec item (M17).
fn outcome(e: &Exec) -> std::result::Result<bool, &'static str> {
    match (e.status.as_deref(), e.exit) {
        (None, _) => Err("status_unreported"),
        (Some(CERTIFIED_STATUS), Some(code)) => Ok(code == 0),
        (Some(CERTIFIED_STATUS), None) => Err("exit_code_unknown"),
        (Some(_), _) => Err("status_not_certified"),
    }
}

/// The call an exec item is attributed to (`inferred`: no shared key): in the
/// same session and turn, the latest call at or before the item's completion
/// whose output, if any, is not before it.
fn attribute<'a>(calls: &'a [Call], e: &Exec) -> Option<&'a Call> {
    let (turn, at) = (e.turn.as_ref()?, e.completed?);
    calls.iter().filter(|c| c.recorded && c.turn.as_ref() == Some(turn) && c.called.is_some_and(|c| c <= at) && c.output.is_none_or(|o| o >= at))
        .max_by_key(|c| (c.called, c.id.as_str()))
}

fn exists(db: &Connection, table: &str) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)", [table], |r| r.get(0))?)
}

/// Sessions with a bound rollout started in the window, their tool rows or
/// why they are unavailable, and the sessions excluded (by reason).
fn sessions(db: &Connection, since: Option<i64>, blocked: &BTreeMap<String, Vec<(i64, i64)>>) -> Result<(Vec<Session>, BTreeMap<String, usize>)> {
    let a6 = exists(db, "codex_tool_sources")? && exists(db, "codex_tool_calls")? && exists(db, "codex_exec_items")?;
    let read = if a6 { "EXISTS(SELECT 1 FROM codex_tool_sources c WHERE c.path_digest=s.path_digest)" } else { "0" };
    type Source = (String, String, Option<String>, String, Option<i64>, bool, String);
    let sources: Vec<Source> = db.prepare(&format!("SELECT s.session_id,s.binding,s.attempt_id,s.cli_version,s.session_unix_ms,{read},s.home_digest
        FROM rollout_sources s ORDER BY s.session_id,s.path_digest"))?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?.collect::<rusqlite::Result<_>>()?;
    let guardians = guardians(db)?;
    let mut grouped = BTreeMap::<&str, Vec<&Source>>::new();
    for s in &sources { grouped.entry(s.0.as_str()).or_default().push(s); }
    let (mut out, mut excluded) = (Vec::new(), BTreeMap::<String, usize>::new());
    for (id, sources) in grouped {
        let start = sources.iter().filter_map(|s| s.4).min();
        if since.is_some_and(|since| start.is_none_or(|at| at < since)) { continue; }
        let bound: Vec<&&Source> = sources.iter().filter(|s| s.1 == "bound").collect();
        if bound.is_empty() { *excluded.entry(sources[0].1.clone()).or_default() += 1; continue; }
        if bound.iter().any(|s| !crate::telemetry::codex::certified(&s.3)) { *excluded.entry("cli_version_uncertified".to_owned()).or_default() += 1; continue; }
        let attempts: BTreeSet<String> = bound.iter().filter_map(|s| s.2.clone()).collect();
        let tools = if !a6 { Err("predates_collection") } else if sources.iter().any(|s| !s.5) { Err("pending_reread") } else { Ok(rows(db, id)?) };
        let waits = attempts.iter().filter_map(|a| blocked.get(a)).flatten().copied().collect();
        let guardians = guardians.get(id).cloned().unwrap_or_default();
        let homes = sources.iter().map(|s| s.6.clone()).collect();
        out.push(Session { id: id.to_owned(), attempts, tools, waits, guardians, homes });
    }
    Ok((out, excluded))
}

/// Start times of guardian sessions per parent session id, from native
/// evidence only (§3): a rollout whose A5 `thread_source` is `guardian_review`
/// or whose A4 `subagent_kind` is `review`, naming a parent in
/// `subagent_parent_thread_id`, else `rollout_threads.parent_thread_id`, other
/// than itself. A sidecar without the A4/A5 tables has none.
fn guardians(db: &Connection) -> Result<BTreeMap<String, Vec<i64>>> {
    let (a4, a5) = (exists(db, "rollout_metadata")?, exists(db, "rollout_threads")?);
    if !a4 && !a5 { return Ok(BTreeMap::new()); }
    let (m, t) = (if a4 { "LEFT JOIN rollout_metadata m ON m.path_digest=s.path_digest" } else { "" },
        if a5 { "LEFT JOIN rollout_threads t ON t.path_digest=s.path_digest" } else { "" });
    let parent = match (a4, a5) { (true, true) => "coalesce(m.subagent_parent_thread_id,t.parent_thread_id)", (true, false) => "m.subagent_parent_thread_id",
        _ => "t.parent_thread_id" };
    let guardian = match (a4, a5) { (true, true) => "(t.thread_source='guardian_review' OR m.subagent_kind='review')", (true, false) => "m.subagent_kind='review'",
        _ => "t.thread_source='guardian_review'" };
    let mut out = BTreeMap::<String, Vec<i64>>::new();
    let mut stmt = db.prepare(&format!("SELECT {parent},s.session_unix_ms FROM rollout_sources s {m} {t}
        WHERE {guardian} AND s.session_unix_ms IS NOT NULL AND {parent} IS NOT NULL AND {parent}<>s.session_id ORDER BY s.path_digest"))?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
        let (parent, at) = row?;
        out.entry(parent).or_default().push(at);
    }
    Ok(out)
}

/// The A6 metadata rows of one session (never content: no such column exists).
fn rows(db: &Connection, session: &str) -> Result<(Vec<Call>, Vec<Exec>)> {
    let calls = db.prepare("SELECT call_id,call_kind IS NOT NULL,name,status,turn_id,called_unix_ms,output_unix_ms FROM codex_tool_calls WHERE session_id=?1
        ORDER BY called_unix_ms IS NULL,called_unix_ms,call_id")?
        .query_map([session], |r| Ok(Call { id: r.get(0)?, recorded: r.get(1)?, name: r.get(2)?, status: r.get(3)?, turn: r.get(4)?, called: r.get(5)?, output: r.get(6)? }))?
        .collect::<rusqlite::Result<_>>()?;
    let items = db.prepare("SELECT turn_id,status,source,exit_code,completed_unix_ms FROM codex_exec_items WHERE session_id=?1
        ORDER BY completed_unix_ms IS NULL,completed_unix_ms,item_id")?
        .query_map([session], |r| Ok(Exec { turn: r.get(0)?, status: r.get(1)?, source: r.get(2)?, exit: r.get(3)?, completed: r.get(4)? }))?
        .collect::<rusqlite::Result<_>>()?;
    Ok((calls, items))
}

/// Counts over observed sessions.
#[derive(Default)]
struct Tally {
    issued: usize,
    by_name: BTreeMap<String, usize>,
    name_unreported: usize,
    by_status: BTreeMap<String, usize>,
    status_unreported: usize,
    without_output: usize,
    outputs_without_call: usize,
    executed: usize,
    by_source: BTreeMap<String, usize>,
    source_unreported: usize,
    attributed: BTreeMap<String, usize>,
    attributed_name_unreported: usize,
    unattributed: usize,
    succeeded: usize,
    failed: usize,
    unknown: BTreeMap<&'static str, usize>,
    /// call → output wall time per tool name (`None`: name unreported) and host (`None`: ambiguous).
    waits: Vec<(Option<String>, Option<String>, i64)>,
    negative: usize,
    /// Issued calls past `issued`, by inferred basis; the rest are unknown.
    accepted: BTreeMap<&'static str, usize>,
    accepted_unknown: usize,
}

impl Tally {
    fn add(&mut self, s: &Session, calls: &[Call], items: &[Exec]) {
        let count = |map: &mut BTreeMap<String, usize>, missing: &mut usize, key: &Option<String>| match key {
            Some(key) => *map.entry(key.clone()).or_default() += 1,
            None => *missing += 1,
        };
        for c in calls {
            if !c.recorded { self.outputs_without_call += 1; continue; }
            self.issued += 1;
            count(&mut self.by_name, &mut self.name_unreported, &c.name);
            count(&mut self.by_status, &mut self.status_unreported, &c.status);
            match s.accepted(c) {
                Some(basis) => *self.accepted.entry(basis).or_default() += 1,
                None => self.accepted_unknown += 1,
            }
            match (c.called, c.output) {
                (_, None) => self.without_output += 1,
                (Some(called), Some(output)) if output >= called => self.waits.push((c.name.clone(), s.home().map(str::to_owned), output - called)),
                (Some(_), Some(_)) => self.negative += 1,
                (None, Some(_)) => {}
            }
        }
        for e in items {
            self.executed += 1;
            count(&mut self.by_source, &mut self.source_unreported, &e.source);
            match attribute(calls, e) {
                Some(call) => count(&mut self.attributed, &mut self.attributed_name_unreported, &call.name),
                None => self.unattributed += 1,
            }
            match outcome(e) {
                Ok(true) => self.succeeded += 1,
                Ok(false) => self.failed += 1,
                Err(reason) => *self.unknown.entry(reason).or_default() += 1,
            }
        }
    }
}

/// Nearest-rank percentile (`p` in percent) of sorted values.
fn rank(sorted: &[i64], p: usize) -> i64 { sorted[(p * sorted.len()).div_ceil(100) - 1] }

fn distribution(mut values: Vec<i64>) -> Value {
    values.sort_unstable();
    if values.is_empty() { return json!({"samples": 0, "p50_ms": null, "p95_ms": null, "max_ms": null}); }
    json!({"samples": values.len(), "p50_ms": rank(&values, 50), "p95_ms": rank(&values, 95), "max_ms": values[values.len() - 1]})
}

fn metric(id: &str, mut body: Value) -> Value {
    body["definition"] = json!(format!("{id}.tools-v1"));
    body["name"] = json!(NAMES.iter().find(|(n, _)| *n == id).map_or("", |(_, name)| *name));
    body
}

fn unavailable_metrics(reason: &str, coverage: Option<&Value>) -> BTreeMap<String, Value> {
    NAMES.iter().map(|(id, _)| {
        let mut body = json!({"value": unavailable(reason)});
        if let Some(coverage) = coverage { body["coverage"] = coverage.clone(); }
        (id.to_string(), metric(id, body))
    }).collect()
}

const CALL_TO_OUTPUT: &str = "call → output line time of one tool call: includes any approval wait (queue time) and the model-side handling, so it is not \
    the execution's run time and never M18's value";

/// M16–M18 over the observed sessions; `unavailable` (never 0) when a
/// session's tool metadata is not read yet or no bound session exists.
fn computed(list: &[Session], coverage: &Value) -> BTreeMap<String, Value> {
    if let Some(reason) = ["predates_collection", "pending_reread"].into_iter().find(|r| list.iter().any(|s| s.tools.as_ref().err() == Some(r))) {
        return unavailable_metrics(reason, Some(coverage));
    }
    if list.is_empty() { return unavailable_metrics("no_bound_session", Some(coverage)); }
    let mut t = Tally::default();
    for s in list { if let Ok((calls, items)) = &s.tools { t.add(s, calls, items); } }
    let accepted: usize = t.accepted.values().sum();
    let m16 = json!({"value": {"issued": t.issued, "accepted": {"status": "inferred", "count": accepted, "unknown": t.accepted_unknown}, "executed": t.executed},
        "issued": {"calls": t.issued, "by_name": t.by_name, "name_unreported": t.name_unreported, "by_status": t.by_status, "status_unreported": t.status_unreported,
            "without_output": t.without_output, "outputs_without_call": t.outputs_without_call,
            "basis": "distinct (session, call_id) with a recorded call; a call replayed by a resumed rollout or a retried record is one logical call"},
        "accepted": {"label": "inferred", "calls": accepted, "by_basis": t.accepted, "unknown": t.accepted_unknown,
            "basis": {"human_routed": "the call → output interval overlaps a B6b `blocked` wait (first to last blocked sample) of the same attempt",
                "auto_review": "a guardian session naming this session as its parent (native evidence) started inside the call → output interval, in the call's turn",
                "human_routed_and_auto_review": "both"},
            "detail": "codex 0.154.0 writes no typed approval request or decision (codex-live-0.154.0-a4.md §3): the stage is inferred from a wait or a \
                guardian review around the call, never from a decision; a call with neither (or without an output) stays unknown, never accepted",
            "caveat": "a denied approval also ends the wait and yields an output: `accepted` means the approval stage completed, not that it was approved"},
        "executed": {"executions": t.executed, "scope": "command_execution", "by_source": t.by_source, "source_unreported": t.source_unreported,
            "attribution": {"basis": "inferred", "rule": "same session and turn, latest call at or before completion, output not before it",
                "by_call_name": t.attributed, "name_unreported": t.attributed_name_unreported, "unattributed": t.unattributed},
            "basis": "one CommandExecution item per execution instance; a repeated execution is another instance"},
        "certified": {"calls": "live", "call_status": "live for custom_tool_call, fixture for function_call", "exec_items": "live", "mcp_calls": "not_collected"},
        "coverage": coverage});
    let terminal = t.succeeded + t.failed;
    let mut m17 = json!({"numerator": t.succeeded, "denominator": terminal, "succeeded": t.succeeded, "failed": t.failed,
        "unknown": {"executions": t.unknown.values().sum::<usize>(), "by_reason": t.unknown}, "pending_calls": t.without_output,
        "cancelled": unavailable("cancellation_not_exposed"), "timed_out": unavailable("timeout_not_exposed"),
        "basis": "CommandExecution items with status `completed` (the only status certified live): exit code 0 succeeded, non-zero failed; a NULL exit code \
            or another status is unknown and excluded, never a success; a pending execution writes no item, so calls without an output are counted apart",
        "coverage": coverage});
    if terminal == 0 { m17["value"] = Value::Null; m17["reason"] = json!("empty_denominator"); } else { m17["value"] = json!(format!("{}/{terminal}", t.succeeded)); }
    let (mut by_name, mut by_home) = (BTreeMap::<String, Vec<i64>>::new(), BTreeMap::<String, Vec<i64>>::new());
    let (mut unnamed, mut ambiguous) = (Vec::new(), Vec::new());
    for (name, home, ms) in &t.waits {
        match name { Some(name) => by_name.entry(name.clone()).or_default().push(*ms), None => unnamed.push(*ms) }
        match home { Some(home) => by_home.entry(home.clone()).or_default().push(*ms), None => ambiguous.push(*ms) }
    }
    let mut wall = distribution(t.waits.iter().map(|(_, _, ms)| *ms).collect());
    wall["by_name"] = json!(by_name.into_iter().map(|(name, values)| (name, distribution(values))).collect::<BTreeMap<_, _>>());
    wall["name_unreported"] = distribution(unnamed);
    wall["by_home"] = json!(by_home.into_iter().map(|(home, values)| (home, distribution(values))).collect::<BTreeMap<_, _>>());
    wall["home_ambiguous"] = distribution(ambiguous);
    wall["host_basis"] = json!("execution_home");
    wall["negative_intervals"] = json!(t.negative);
    wall["method"] = json!("nearest_rank");
    wall["caveat"] = json!("includes_approval_wait");
    wall["detail"] = json!(CALL_TO_OUTPUT);
    let m18 = json!({"value": unavailable("execution_duration_not_exposed"),
        "detail": "codex 0.154.0 records no execution end − start: the CommandExecution duration and its start/completion times are the unified exec \
            startup (caveat startup_not_run_time), and the call → output time includes approval waits",
        "queue_time": unavailable("approval_decision_not_exposed"), "pending_calls": t.without_output, "timed_out": unavailable("timeout_not_exposed"),
        "call_to_output_ms": wall, "coverage": coverage});
    BTreeMap::from([("M16".to_owned(), metric("M16", m16)), ("M17".to_owned(), metric("M17", m17)), ("M18".to_owned(), metric("M18", m18))])
}

fn coverage(list: &[Session], excluded: &BTreeMap<String, usize>) -> Value {
    let count = |reason: &str| list.iter().filter(|s| s.tools.as_ref().err() == Some(&reason)).count();
    json!({"sessions": list.len(), "observed": list.iter().filter(|s| s.tools.is_ok()).count(), "pending_reread": count("pending_reread"),
        "predates_collection": count("predates_collection"), "excluded": excluded})
}

/// `accounting tools`: per bound session its tool call and execution counts
/// (or why they are unavailable), the coverage and M16–M18. Read-only.
pub fn read(project: &Path, db: &Connection) -> Result<Value> {
    let (list, excluded) = sessions(db, None, &super::attention::blocked_spans(project, db)?)?;
    let coverage = coverage(&list, &excluded);
    let sessions: Vec<Value> = list.iter().map(|s| {
        let tools = match &s.tools {
            Err(reason) => unavailable(reason),
            Ok((calls, items)) => {
                let mut t = Tally::default();
                t.add(s, calls, items);
                json!({"issued": t.issued, "without_output": t.without_output, "outputs_without_call": t.outputs_without_call, "executed": t.executed,
                    "attributed": t.executed - t.unattributed, "unattributed": t.unattributed, "succeeded": t.succeeded, "failed": t.failed,
                    "unknown": t.unknown.values().sum::<usize>()})
            }
        };
        json!({"session_id": s.id, "attempt_ids": s.attempts, "tools": tools})
    }).collect();
    Ok(json!({"sessions": sessions, "coverage": coverage, "metrics": computed(&list, &coverage)}))
}

/// M16–M18 for `telemetry <slug> report` (sessions started in the window).
pub fn metrics(project: &Path, since: Option<i64>) -> Result<BTreeMap<String, Value>> {
    let Some(db) = crate::telemetry::sidecar::read(project)? else { return Ok(unavailable_metrics("collection_not_run", None)) };
    let (list, excluded) = sessions(&db, since, &super::attention::blocked_spans(project, &db)?)?;
    Ok(computed(&list, &coverage(&list, &excluded)))
}

/// Text view: coverage, one line per session and per metric; unknown is `n/a (<reason>)`.
pub fn text(value: &Value) -> String {
    let reason = |v: &Value| format!("n/a ({})", v["reason"].as_str().unwrap_or("unknown"));
    if value["status"] == "unavailable" { return reason(value) + "\n"; }
    let c = &value["coverage"];
    let excluded: Vec<String> = c["excluded"].as_object().into_iter().flatten().map(|(k, v)| format!("{k} {v}")).collect();
    let mut out = format!("coverage {} sessions: {} observed, {} pending_reread, {} predates_collection; excluded {}\n", c["sessions"], c["observed"],
        c["pending_reread"], c["predates_collection"], if excluded.is_empty() { "none".to_owned() } else { excluded.join(", ") });
    for s in value["sessions"].as_array().into_iter().flatten() {
        let attempts: Vec<&str> = s["attempt_ids"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        let t = &s["tools"];
        let detail = if t["status"] == "unavailable" { reason(t) } else {
            format!("issued {} ({} without output, {} outputs without call), executed {} ({} inferred to a call, {} unattributed), succeeded {} failed {} unknown {}",
                t["issued"], t["without_output"], t["outputs_without_call"], t["executed"], t["attributed"], t["unattributed"], t["succeeded"], t["failed"], t["unknown"])
        };
        out += &format!("session {} attempts={}: {detail}\n", s["session_id"].as_str().unwrap_or(""), attempts.join(","));
    }
    let m = &value["metrics"];
    let (m16, m17, m18) = (&m["M16"], &m["M17"], &m["M18"]);
    out += &match m16["value"]["issued"].as_u64() {
        Some(issued) => format!("M16 tool_call_volume issued {issued}, accepted {} inferred ({} unknown), executed {}\n", m16["value"]["accepted"]["count"],
            m16["value"]["accepted"]["unknown"], m16["value"]["executed"]),
        None => format!("M16 tool_call_volume {}\n", reason(&m16["value"])),
    };
    out += &match &m17["value"] {
        Value::String(v) => format!("M17 tool_execution_success {v} (unknown {} excluded, pending calls {})\n", m17["unknown"]["executions"], m17["pending_calls"]),
        Value::Null => format!("M17 tool_execution_success n/a ({})\n", m17["reason"].as_str().unwrap_or("unknown")),
        other => format!("M17 tool_execution_success {}\n", reason(other)),
    };
    out += &format!("M18 tool_latency_p95 {}\n", reason(&m18["value"]));
    let wall = &m18["call_to_output_ms"];
    if wall["samples"].is_u64() {
        let p95 = wall["p95_ms"].as_i64().map_or("n/a".to_owned(), |n| n.to_string());
        out += &format!("call_to_output_ms p95 {p95} of {} calls (includes approval wait; not execution time)\n", wall["samples"]);
    }
    out
}

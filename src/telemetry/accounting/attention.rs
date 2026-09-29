//! Human attention intervals (docs/telemetry/contracts-accounting.md §6; plan
//! TM1.8 remainder "S4", doc 07 M31–M33, doc 10 §5a). Each observation pass
//! reads stock Herdr's `agent list` once per recorded socket (read-only,
//! bounded by the budget) and appends one sample per launched, unterminated
//! canonical attempt: the `agent_status` label of the pane recorded in its
//! `runtime.launch_started` receipt, or why no label was observed. Intervals,
//! gaps and the metrics are derived from the samples at read time; an
//! interval with no observed start or end is censored, never closed at a
//! guessed time, and an observation gap is reported as a gap, never as "no
//! attention". Only labels, reason codes and timestamps are stored.
use anyhow::Result;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use super::unavailable;
use crate::herdr::{self, Herdr};
use crate::runner::RealRunner;

pub const SOURCE: &str = "herdr-agent-list-v1";
/// The `agent_status` label stock Herdr gives an agent waiting on the human.
const WAITING: &str = "blocked";
/// The ticker's telemetry pass interval (`HERDR_PROJECTS_TELEMETRY_COLLECT_SECS`, default 300 s).
const DEFAULT_INTERVAL_SECS: i64 = 300;
/// Distinct Herdr sockets queried per pass; later attempts record `budget_exhausted`.
const MAX_SOCKETS: usize = 4;
const CALL_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REPLY: u64 = 1 << 20;
const MAX_AGENTS: usize = 1024;
const OPEN: [&str; 3] = ["launching", "running", "awaiting_input"];

/// A launched canonical attempt and the route its launch receipt recorded.
struct Bound {
    attempt: String,
    task: String,
    open: bool,
    launched: i64,
    /// Terminal lifecycle mark (contracts §4), when the attempt ended and has one.
    ended: Option<i64>,
    decided: Option<i64>,
    machine: String,
    socket: String,
    workspace: String,
    tab: String,
    pane: String,
    cwd: String,
    kind: String,
    name: String,
}

/// Attempts with a `runtime.launch_started` receipt (the latest per attempt), in attempt order.
fn bindings(project: &Path) -> Result<Vec<Bound>> {
    let path = project.join(".state/state.db");
    if !path.exists() { return Ok(Vec::new()); }
    let db = crate::telemetry::read_only(&path)?;
    let table = |name: &str| db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)", [name], |r| r.get::<_, bool>(0));
    let ended = if table("attempt_lifecycle")? {
        "(SELECT min(l.unix_ms) FROM attempt_lifecycle l WHERE l.attempt_id=a.id AND l.state IN ('completed','failed','cancelled','lost'))"
    } else { "NULL" };
    let decided = if table("dispatch_decisions")? { "(SELECT d.decided_unix_ms FROM dispatch_decisions d WHERE d.attempt_id=a.id)" } else { "NULL" };
    let sql = format!("WITH started AS (SELECT json_extract(payload,'$.attempt') AS attempt,max(sequence) AS sequence FROM events
        WHERE kind='runtime.launch_started' GROUP BY 1)
        SELECT a.id,a.task_id,a.state,a.termination_observed,e.payload,{ended},{decided} FROM started s JOIN events e ON e.sequence=s.sequence
        JOIN attempts a ON a.id=s.attempt ORDER BY a.rowid");
    type Row = (String, String, String, bool, String, Option<i64>, Option<i64>);
    let rows: Vec<Row> = db.prepare(&sql)?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?.collect::<rusqlite::Result<_>>()?;
    Ok(rows.into_iter().map(|(attempt, task, state, terminated, payload, ended, decided)| {
        let receipt: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
        let text = |value: &Value| value.as_str().unwrap_or("").to_owned();
        let route = &receipt["route"];
        let open = !terminated && OPEN.contains(&state.as_str());
        Bound { attempt, task, open, launched: receipt["observed_unix_ms"].as_i64().unwrap_or(0), ended: if open { None } else { ended }, decided,
            machine: text(&route["machine"]), socket: text(&route["socket"]), workspace: text(&route["workspace_id"]), tab: text(&route["tab_id"]),
            pane: text(&route["pane_id"]), cwd: text(&route["cwd"]), kind: text(&receipt["agent"]["kind"]), name: text(&receipt["agent"]["name"]) }
    }).collect())
}

/// The sampling interval the ticker uses; the gap threshold is twice it.
fn interval_ms() -> i64 {
    let secs = std::env::var("HERDR_PROJECTS_TELEMETRY_COLLECT_SECS").ok().and_then(|v| v.parse::<i64>().ok()).filter(|s| *s > 0).unwrap_or(DEFAULT_INTERVAL_SECS);
    secs.saturating_mul(1000)
}

/// The Herdr executable the ticker's client uses: `HERDR_BIN_PATH`, else `herdr` on `PATH`.
fn herdr_bin() -> String {
    std::env::var("HERDR_BIN_PATH").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "herdr".to_owned())
}

/// One read-only `herdr agent list` on `socket` through the shared Herdr
/// client (its socket variable and `HERDR_SESSION` removal) and the gated
/// runner, with its reply bounded by the remaining byte budget.
fn agent_list(bin: &str, socket: &str, remaining: &mut u64) -> std::result::Result<Vec<Value>, &'static str> {
    if !Path::new(socket).exists() { return Err("herdr_unreachable"); }
    let limit = (*remaining).min(MAX_REPLY) as usize;
    let out = Herdr::new(bin, socket, &RealRunner).agent_list_bounded(CALL_TIMEOUT, limit).map_err(|_| "herdr_unreachable")?;
    *remaining = remaining.saturating_sub(out.stdout_total_bytes.saturating_add(out.stderr_total_bytes));
    if out.stdout_truncated || out.stderr_truncated { return Err("budget_exhausted"); }
    let reply = herdr::reply_json(&out);
    if reply.as_ref().is_some_and(|r| r.get("error").is_some()) { return Err("herdr_error"); }
    if !out.success() { return Err("herdr_unreachable"); }
    let agents = reply.as_ref().and_then(|r| r["result"]["agents"].as_array()).ok_or("herdr_reply_invalid")?;
    if agents.len() > MAX_AGENTS { return Err("herdr_reply_invalid"); }
    Ok(agents.clone())
}

/// The attempt's agent state label, or why it was not observed. The agent must
/// be the only one on the recorded pane and match the receipt's workspace, tab,
/// cwd, kind and name.
fn classify(b: &Bound, agents: &[Value]) -> std::result::Result<&'static str, &'static str> {
    let matching: Vec<&Value> = agents.iter().filter(|a| a["pane_id"].as_str() == Some(b.pane.as_str())).collect();
    let agent = match matching.as_slice() { [] => return Err("agent_absent"), [agent] => *agent, _ => return Err("identity_mismatch") };
    let field = |key: &str| agent[key].as_str().unwrap_or("");
    if field("workspace_id") != b.workspace || field("tab_id") != b.tab || (!field("cwd").is_empty() && field("cwd") != b.cwd)
        || (!b.kind.is_empty() && field("agent") != b.kind) || (!b.name.is_empty() && field("name") != b.name) {
        return Err("identity_mismatch");
    }
    match field("agent_status") {
        "blocked" => Ok("blocked"),
        "working" => Ok("working"),
        "idle" => Ok("idle"),
        "done" => Ok("done"),
        "" | "unknown" => Err("state_unknown"),
        _ => Err("state_unrecognized"),
    }
}

/// One observation pass: a sample for every launched, unterminated attempt.
/// Herdr is queried before the sidecar transaction; writes only the sidecar.
pub fn observe(project: &Path, db: &mut Connection, budget: crate::telemetry::codex::Budget) -> Result<Value> {
    let bound: Vec<Bound> = bindings(project)?.into_iter().filter(|b| b.open).collect();
    let (now, interval, bin) = (jiff::Timestamp::now().as_millisecond(), interval_ms(), herdr_bin());
    let mut remaining = budget.bytes;
    let mut replies: BTreeMap<String, std::result::Result<Vec<Value>, &'static str>> = BTreeMap::new();
    let mut samples = Vec::new();
    for b in &bound {
        let sample = if !b.machine.is_empty() { Err("remote_route") }
            else if b.pane.is_empty() || !Path::new(&b.socket).is_absolute() { Err("route_unrecorded") }
            else {
                if !replies.contains_key(&b.socket) {
                    let reply = if replies.len() >= MAX_SOCKETS || remaining == 0 { Err("budget_exhausted") } else { agent_list(&bin, &b.socket, &mut remaining) };
                    replies.insert(b.socket.clone(), reply);
                }
                match &replies[&b.socket] { Err(reason) => Err(*reason), Ok(agents) => classify(b, agents) }
            };
        samples.push((b.attempt.as_str(), sample));
    }
    let tx = db.transaction()?;
    let (mut states, mut gaps) = (0, BTreeMap::<&str, i64>::new());
    for (attempt, sample) in &samples {
        let (state, gap) = match sample { Ok(state) => { states += 1; (Some(*state), None) } Err(gap) => { *gaps.entry(gap).or_default() += 1; (None, Some(*gap)) } };
        tx.execute("INSERT INTO attention_samples(attempt_id,observed_unix_ms,state,gap,interval_ms,source) VALUES(?1,?2,?3,?4,?5,?6)",
            params![attempt, now, state, gap, interval, SOURCE])?;
    }
    tx.commit()?;
    Ok(json!({"attempts": samples.len(), "states": states, "gaps": gaps}))
}

struct Sample { at: i64, state: std::result::Result<String, String>, interval: i64 }

struct Interval { opened: i64, start: &'static str, last: i64, closed: Option<i64>, end: &'static str, gap: Option<String>, counted: bool }

impl Interval {
    /// Only an interval with an observed transition in and out has a duration.
    fn duration(&self) -> Option<i64> { self.closed.filter(|_| self.start == "observed_transition").map(|closed| closed - self.opened) }
}

struct Gap { from: i64, to: Option<i64>, reason: String }

#[derive(Default)]
struct Derived { observed: bool, intervals: Vec<Interval>, gaps: Vec<Gap>, nonwaiting_ms: i64 }

impl Derived {
    fn waiting_ms(&self) -> i64 { self.intervals.iter().filter_map(Interval::duration).sum() }
    /// Observed time outside censored intervals: the M32 denominator share.
    fn resolved_ms(&self) -> i64 { self.nonwaiting_ms + self.waiting_ms() }
    fn interventions(&self) -> usize { self.intervals.iter().filter(|i| i.counted).count() }
    fn uncertain(&self) -> usize { self.intervals.len() - self.interventions() }
    fn censored(&self) -> usize { self.intervals.iter().filter(|i| i.duration().is_none()).count() }
    fn complete(&self) -> bool { self.observed && self.gaps.is_empty() }
}

/// Intervals and gaps of one attempt. A sample's label holds until the next
/// sample (sample resolution). Consecutive successful samples are continuous
/// when no failed sample lies between them and they are at most twice the
/// sampling interval apart; otherwise the span is a gap and an open interval
/// is censored there. An interval first seen after a gap that censored the
/// previous one may be the same wait: it is kept but not counted.
fn derive(b: &Bound, samples: &[Sample], now: i64) -> Derived {
    let mut d = Derived::default();
    // The last successful sample `(at, label, interval)`, the first failure since it, and the open interval.
    let mut prev: Option<(i64, &str, i64)> = None;
    let mut failed: Option<&str> = None;
    let mut open: Option<Interval> = None;
    for s in samples.iter().filter(|s| b.ended.is_none_or(|end| s.at <= end)) {
        let state = match &s.state { Err(reason) => { failed.get_or_insert(reason.as_str()); continue } Ok(state) => state.as_str() };
        let from = prev.map_or(b.launched, |p| p.0);
        let broken = failed.take().map(str::to_owned).or_else(|| (s.at - from > 2 * s.interval).then(|| "not_observed".to_owned()));
        let (mut gap, mut censored) = (false, false);
        if let Some(reason) = broken {
            d.gaps.push(Gap { from, to: Some(s.at), reason: reason.clone() });
            if let Some(mut interval) = open.take() { (interval.end, interval.gap, censored) = ("observation_gap", Some(reason), true); d.intervals.push(interval); }
            gap = true;
        } else if let Some((at, previous, _)) = prev && previous != WAITING {
            d.nonwaiting_ms += s.at - at;
        }
        if state == WAITING {
            match open.as_mut() {
                Some(interval) => interval.last = s.at,
                None => {
                    let start = if prev.is_none() { "first_observation" } else if gap { "after_gap" } else { "observed_transition" };
                    open = Some(Interval { opened: s.at, start, last: s.at, closed: None, end: "open", gap: None, counted: !censored });
                }
            }
        } else if let Some(mut interval) = open.take() {
            (interval.closed, interval.end) = (Some(s.at), "closed");
            d.intervals.push(interval);
        }
        prev = Some((s.at, state, s.interval));
    }
    let Some((last, state, interval)) = prev else {
        d.gaps.push(Gap { from: b.launched, to: if b.open { None } else { b.ended }, reason: failed.unwrap_or("not_observed").to_owned() });
        return d;
    };
    d.observed = true;
    let horizon = if b.open { Some(now) } else { b.ended };
    let trailing = failed.map(str::to_owned).or_else(|| horizon.filter(|h| h - last > 2 * interval).map(|_| "not_observed".to_owned()));
    match trailing {
        Some(reason) => {
            d.gaps.push(Gap { from: last, to: if b.open { None } else { b.ended }, reason: reason.clone() });
            if let Some(mut interval) = open.take() { (interval.end, interval.gap) = ("observation_gap", Some(reason)); d.intervals.push(interval); }
        }
        None => {
            if let (false, Some(end)) = (b.open, b.ended) && state != WAITING { d.nonwaiting_ms += end - last; }
            if let Some(mut interval) = open.take() { interval.end = if b.open { "open_at_horizon" } else { "attempt_ended" }; d.intervals.push(interval); }
        }
    }
    d
}

/// Samples per attempt in observation order, and the attempts sampled without a receipt.
fn samples(db: &Connection, known: &BTreeSet<&str>) -> Result<(BTreeMap<String, Vec<Sample>>, i64)> {
    let mut by_attempt = BTreeMap::<String, Vec<Sample>>::new();
    let mut orphans = 0;
    let mut stmt = db.prepare("SELECT attempt_id,observed_unix_ms,state,gap,interval_ms FROM attention_samples ORDER BY observed_unix_ms,rowid")?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let attempt: String = r.get(0)?;
        if !known.contains(attempt.as_str()) { orphans += 1; continue; }
        let state = match r.get::<_, Option<String>>(2)? { Some(state) => Ok(state), None => Err(r.get::<_, Option<String>>(3)?.unwrap_or_default()) };
        by_attempt.entry(attempt).or_default().push(Sample { at: r.get(1)?, state, interval: r.get(4)? });
    }
    Ok((by_attempt, orphans))
}

fn collected(db: &Connection) -> Result<bool> {
    let table: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='attention_samples')", [], |r| r.get(0))?;
    Ok(table && db.query_row("SELECT EXISTS(SELECT 1 FROM attention_samples)", [], |r| r.get(0))?)
}

/// A launched attempt and its attention (`None` when never sampled).
type Attention = (Bound, Option<Derived>);

/// Every launched attempt with its derived attention, and the orphan sample count.
fn derive_all(project: &Path, db: &Connection) -> Result<(Vec<Attention>, i64)> {
    let bound = bindings(project)?;
    let known: BTreeSet<&str> = bound.iter().map(|b| b.attempt.as_str()).collect();
    let (mut by_attempt, orphans) = samples(db, &known)?;
    let now = jiff::Timestamp::now().as_millisecond();
    let mut out = Vec::new();
    for b in bound {
        let samples = by_attempt.remove(&b.attempt).unwrap_or_default();
        let derived = (!samples.is_empty()).then(|| derive(&b, &samples, now));
        out.push((b, derived));
    }
    Ok((out, orphans))
}

/// Per launched attempt, the span of each of its waits from its first to its
/// last `blocked` sample (counted or not, closed or censored): observed
/// evidence that the attempt waited on the human then (§9 accepted stage).
pub fn blocked_spans(project: &Path, db: &Connection) -> Result<BTreeMap<String, Vec<(i64, i64)>>> {
    if !collected(db)? { return Ok(BTreeMap::new()); }
    let (all, _) = derive_all(project, db)?;
    Ok(all.into_iter().filter_map(|(b, d)| d.map(|d| (b.attempt, d.intervals.iter().map(|i| (i.opened, i.last)).collect()))).collect())
}

/// The union of `[from, to)` spans (overlaps counted once), in ms.
fn union_ms(mut spans: Vec<(i64, i64)>) -> i64 {
    spans.sort();
    let (mut total, mut reach) = (0, i64::MIN);
    for (from, to) in spans {
        let from = from.max(reach);
        if to > from { total += to - from; reach = to; }
    }
    total
}

/// Agent kinds whose `blocked` label was certified live as waiting on a human
/// approval prompt (docs/telemetry/codex-live-0.154.0-a4.md §3). Every other
/// kind's approval prompt is still `fixture`.
const LIVE_KINDS: [&str; 1] = ["codex"];
/// What M31/M32 can see: approvals decided by Codex's automatic reviewer never show `blocked`.
const SCOPE: &str = "human_routed_waits";

/// The certification of the signal for one attempt's agent kind.
fn certified(kind: &str) -> &'static str { if LIVE_KINDS.contains(&kind) { "live" } else { "fixture" } }

pub fn signal() -> Value {
    json!({"source": SOURCE, "field": "agent_status", "waiting_states": [WAITING], "reason_type": "blocked_untyped", "certified": "live",
        "certified_by_agent_kind": LIVE_KINDS.iter().map(|k| (k.to_string(), json!("live"))).collect::<serde_json::Map<_, _>>(), "other_agent_kinds": "fixture",
        "basis": "stock Herdr `agent list` agent_status; `blocked` was observed live (herdr 0.9.1) for a codex 0.154.0 approval prompt routed to the human \
            (docs/telemetry/codex-live-0.154.0-a4.md) and for claude's trust dialog (docs/herdr-notes.md); other agent kinds' approval prompts are not certified live",
        "scope": SCOPE,
        "caveat": "approvals decided by codex's automatic reviewer (approvals_reviewer = auto_review) never show `blocked`: they are not waits and M31/M32 do not count them",
        "resolution": "sample"})
}

fn attention_json(b: &Bound, derived: Option<&Derived>) -> Value {
    let gaps = |d: &Derived| d.gaps.iter().map(|g| json!({"from_unix_ms": g.from, "to_unix_ms": g.to, "reason": g.reason})).collect::<Vec<_>>();
    let unobserved = |gaps: Vec<Value>| { let mut v = unavailable("not_observed"); v["gaps"] = json!(gaps); v };
    match derived {
        None => unobserved(vec![json!({"from_unix_ms": b.launched, "to_unix_ms": if b.open { None } else { b.ended }, "reason": "not_observed"})]),
        Some(d) if !d.observed => unobserved(gaps(d)),
        Some(d) => json!({
            "intervals": d.intervals.iter().map(|i| json!({"opened_unix_ms": i.opened, "start": i.start, "last_observed_unix_ms": i.last,
                "closed_unix_ms": i.closed, "end": i.end, "gap_reason": i.gap, "duration_ms": i.duration(), "counted": i.counted})).collect::<Vec<_>>(),
            "gaps": gaps(d), "interventions": d.interventions(), "uncertain_starts": d.uncertain(), "waiting_ms": d.waiting_ms(), "observed_ms": d.resolved_ms()}),
    }
}

/// `accounting attention`: per launched attempt its intervals, gaps and
/// waiting time; the fleet union; M31–M33. Read-only.
pub fn read(project: &Path, db: &Connection) -> Result<Value> {
    if !collected(db)? {
        return Ok(json!({"signal": signal(), "attempts": [], "metrics": not_collected()}));
    }
    let (all, orphans) = derive_all(project, db)?;
    let mut attempts = Vec::new();
    let (mut spans, mut sum, mut interventions) = (Vec::new(), 0, 0);
    for (b, d) in &all {
        if let Some(d) = d {
            spans.extend(d.intervals.iter().filter_map(|i| i.duration().map(|n| (i.opened, i.opened + n))));
            sum += d.waiting_ms();
            interventions += d.interventions();
        }
        attempts.push(json!({"attempt_id": b.attempt, "task_id": b.task, "state": if b.open { "open" } else { "ended" }, "launched_unix_ms": b.launched,
            "ended_unix_ms": b.ended, "certified": certified(&b.kind), "attention": attention_json(b, d.as_ref())}));
    }
    Ok(json!({"signal": signal(), "attempts": attempts, "orphan_samples": orphans,
        "fleet": {"waiting_union_ms": union_ms(spans), "waiting_sum_ms": sum, "interventions": interventions},
        "metrics": computed(project, &all, None)?}))
}

const NAMES: [(&str, &str); 3] = [("M31", "human_interventions_per_accepted_task"), ("M32", "waiting_on_you_share"), ("M33", "permission_prompts_per_attempt")];

fn metric(id: &str, mut body: Value) -> Value {
    body["definition"] = json!(format!("{id}.attention-v1"));
    body["name"] = json!(NAMES.iter().find(|(n, _)| *n == id).map_or("", |(_, name)| *name));
    body
}

fn not_collected() -> BTreeMap<String, Value> {
    NAMES.iter().map(|(id, _)| (id.to_string(), metric(id, json!({"value": unavailable("attention_not_collected")})))).collect()
}

/// Contracts §6 `T` (terminal tasks) and `count(A)` (accepted), restricted to
/// tasks with an attempt decided in the window, as the central report does.
fn cohort(project: &Path, since: Option<i64>) -> Result<(BTreeSet<String>, usize)> {
    let db = crate::telemetry::read_only(&project.join(".state/state.db"))?;
    let tasks = crate::telemetry::metrics::task_evidence(&db)?;
    let windowed: Option<BTreeSet<String>> = match since {
        None => None,
        Some(since) => Some(if db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='dispatch_decisions')", [], |r| r.get::<_, bool>(0))? {
            db.prepare("SELECT DISTINCT a.task_id FROM attempts a JOIN dispatch_decisions d ON d.attempt_id=a.id WHERE d.decided_unix_ms>=?1")?
                .query_map([since], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        } else { BTreeSet::new() }),
    };
    let (mut terminal, mut accepted) = (BTreeSet::new(), 0);
    for (task, state, evidence) in tasks {
        if windowed.as_ref().is_some_and(|w| !w.contains(&task)) { continue; }
        if evidence || ["succeeded", "failed", "cancelled"].contains(&state.as_str()) {
            if evidence { accepted += 1; }
            terminal.insert(task);
        }
    }
    Ok((terminal, accepted))
}

fn ratio(numerator: i64, denominator: i64, mut body: Value) -> Value {
    body["numerator"] = json!(numerator);
    body["denominator"] = json!(denominator);
    if denominator == 0 { body["value"] = Value::Null; body["reason"] = json!("empty_denominator"); } else { body["value"] = json!(format!("{numerator}/{denominator}")); }
    body
}

/// M31 over the attempts of `T` (terminal cohort), M32 over launched attempts
/// decided in the window (assignment cohort), M33 unavailable: Herdr's
/// `blocked` carries no typed reason.
fn computed(project: &Path, all: &[Attention], since: Option<i64>) -> Result<BTreeMap<String, Value>> {
    let (terminal, accepted) = cohort(project, since)?;
    let t: Vec<&Attention> = all.iter().filter(|(b, _)| terminal.contains(&b.task)).collect();
    let complete = t.iter().filter(|(_, d)| d.as_ref().is_some_and(Derived::complete)).count();
    let not_observed = t.iter().filter(|(_, d)| !d.as_ref().is_some_and(|d| d.observed)).count();
    let counted: usize = t.iter().filter_map(|(_, d)| d.as_ref()).map(Derived::interventions).sum();
    let uncertain: usize = t.iter().filter_map(|(_, d)| d.as_ref()).map(Derived::uncertain).sum();
    let coverage = json!({"attempts": t.len(), "complete": complete, "not_observed": not_observed, "with_gaps": t.len() - complete - not_observed});
    let base = json!({"reason_type": "blocked_untyped", "source": "controller_observed", "scope": SCOPE, "coverage": coverage});
    let m31 = if complete < t.len() {
        let mut body = base;
        body["value"] = unavailable("incomplete_observation");
        body["observed_interventions"] = json!(counted);
        body["uncertain_starts"] = json!(uncertain);
        body["denominator"] = json!(accepted);
        body
    } else { ratio(counted as i64, accepted as i64, base) };

    let assigned: Vec<&Attention> = all.iter().filter(|(b, _)| since.is_none_or(|since| b.decided.is_some_and(|at| at >= since))).collect();
    let observed: Vec<&Derived> = assigned.iter().filter_map(|(_, d)| d.as_ref()).filter(|d| d.observed).collect();
    let spans = observed.iter().flat_map(|d| d.intervals.iter().filter_map(|i| i.duration().map(|n| (i.opened, i.opened + n)))).collect();
    let coverage = json!({"attempts": assigned.len(), "observed": observed.len(), "not_observed": assigned.len() - observed.len(),
        "with_gaps": observed.iter().filter(|d| !d.gaps.is_empty()).count(), "censored_intervals": observed.iter().map(|d| d.censored()).sum::<usize>()});
    let base = json!({"unit": "ms", "scope": SCOPE, "coverage": coverage, "waiting_union_ms": union_ms(spans)});
    let m32 = if observed.is_empty() {
        let mut body = base;
        body["value"] = unavailable("not_observed");
        body
    } else { ratio(observed.iter().map(|d| d.waiting_ms()).sum(), observed.iter().map(|d| d.resolved_ms()).sum(), base) };

    let m33 = json!({"value": unavailable("attention_reason_not_exposed"),
        "detail": "stock Herdr reports `blocked` without a typed reason: a permission prompt is not distinguishable from a question or trust dialog"});
    Ok(BTreeMap::from([("M31".to_owned(), metric("M31", m31)), ("M32".to_owned(), metric("M32", m32)), ("M33".to_owned(), metric("M33", m33))]))
}

/// M31–M33 for `telemetry <slug> report` (a lane key replaces the central one).
pub fn metrics(project: &Path, since: Option<i64>) -> Result<BTreeMap<String, Value>> {
    let Some(db) = crate::telemetry::sidecar::read(project)? else { return Ok(not_collected()) };
    if !collected(&db)? { return Ok(not_collected()); }
    let (all, _) = derive_all(project, &db)?;
    computed(project, &all, since)
}

/// Text view: one block per attempt, the fleet line and one line per metric; unknown is `n/a (<reason>)`.
pub fn text(value: &Value) -> String {
    let reason = |v: &Value| format!("n/a ({})", v["reason"].as_str().unwrap_or("unknown"));
    if value["status"] == "unavailable" { return reason(value) + "\n"; }
    let s = &value["signal"];
    let live = s["certified_by_agent_kind"].as_object().map(|kinds| kinds.keys().cloned().collect::<Vec<_>>().join(", ")).unwrap_or_default();
    let mut out = format!("signal {} {}={} (certified: live for {live}; {} for other agent kinds; {})\n", s["source"].as_str().unwrap_or(""),
        s["field"].as_str().unwrap_or(""), WAITING, s["other_agent_kinds"].as_str().unwrap_or(""), s["scope"].as_str().unwrap_or(""));
    let at = |v: &Value| v.as_i64().map_or("open".to_owned(), |n| n.to_string());
    for a in value["attempts"].as_array().into_iter().flatten() {
        let (id, state, attention) = (a["attempt_id"].as_str().unwrap_or(""), a["state"].as_str().unwrap_or(""), &a["attention"]);
        if attention["status"] == "unavailable" {
            out += &format!("attempt {id} {state} {}\n", reason(attention));
        } else {
            out += &format!("attempt {id} {state}: {} interventions ({} uncertain), waiting {} ms of {} observed ms\n", attention["interventions"],
                attention["uncertain_starts"], attention["waiting_ms"], attention["observed_ms"]);
            for i in attention["intervals"].as_array().into_iter().flatten() {
                let duration = i["duration_ms"].as_i64().map_or("censored".to_owned(), |n| format!("{n} ms"));
                out += &format!("  waiting {}..{} {} -> {} {duration}\n", i["opened_unix_ms"], at(&i["closed_unix_ms"]), i["start"].as_str().unwrap_or(""), i["end"].as_str().unwrap_or(""));
            }
        }
        for g in attention["gaps"].as_array().into_iter().flatten() {
            out += &format!("  gap {}..{} {}\n", g["from_unix_ms"], at(&g["to_unix_ms"]), g["reason"].as_str().unwrap_or(""));
        }
    }
    let fleet = &value["fleet"];
    if fleet.is_object() { out += &format!("fleet waiting union {} ms, sum {} ms, {} interventions\n", fleet["waiting_union_ms"], fleet["waiting_sum_ms"], fleet["interventions"]); }
    for (id, m) in value["metrics"].as_object().into_iter().flatten() {
        let shown = match &m["value"] { Value::String(v) => v.clone(), Value::Null => format!("n/a ({})", m["reason"].as_str().unwrap_or("unknown")), other => reason(other) };
        out += &format!("{id} {} {shown}\n", m["name"].as_str().unwrap_or(""));
    }
    out
}

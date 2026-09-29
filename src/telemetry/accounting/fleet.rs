//! Fleet efficiency (docs/telemetry/contracts-accounting.md §10; plan TM2.8,
//! doc 07 M34–M37, doc 10 §5a "Fan-out"), derived at read time from
//! canonical `state.db` rows only (read-only; nothing is stored): attempt
//! lifecycle marks (contracts §4: an attempt is active from its `running`
//! mark to its terminal mark), dispatch decisions with their task
//! classification, acceptance evidence (contracts §6 `A`) and integration
//! operations. M35 buckets fixed activity windows by their time-weighted
//! active-attempt count; M36 counts integrator-observed conflict/rebase
//! events. M34 and M37 have no canonical producer and are `unavailable`.
//! Only worker attempts are counted: the coordinator has no canonical
//! attempt, so its time and cost never enter a worker figure.
use anyhow::Result;
use rusqlite::Connection;
use serde_json::{Value, json};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::unavailable;

/// Default activity window length in minutes (one hour, aligned to UTC hours);
/// `accounting fleet --window-minutes` takes any divisor of a day.
pub const DEFAULT_WINDOW_MINUTES: i64 = 60;
const DAY_MINUTES: i64 = 1440;
const HOUR_MS: i128 = 3_600_000;
const TERMINAL: [&str; 4] = ["completed", "failed", "cancelled", "lost"];
/// Comparability: each bucket's task mix (class/band share of active time) is
/// within this total variation distance of the reference bucket's.
const MAX_TVD: (i128, i128) = (1, 10);
const UNCLASSIFIED: &str = "unclassified";
const NAMES: [(&str, &str, &str); 4] = [("M34", "coordinator_overhead", "M34.fleet-v1"), ("M35", "fan_out_efficiency", "M35.fanout-v1"),
    ("M36", "integration_conflict_rate", "M36.integration-v1"), ("M37", "overlap_waste_share", "M37.fleet-v1")];

/// An exact rational, always reduced with a positive denominator.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Q(i128, i128);

impl Q {
    fn new(n: i128, d: i128) -> Q {
        fn gcd(a: i128, b: i128) -> i128 { if b == 0 { a.abs() } else { gcd(b, a % b) } }
        let g = gcd(n, d).max(1) * d.signum();
        Q(n / g, d / g)
    }
    fn int(n: i128) -> Q { Q(n, 1) }
    fn add(self, o: Q) -> Q { Q::new(self.0 * o.1 + o.0 * self.1, self.1 * o.1) }
    fn sub(self, o: Q) -> Q { self.add(Q(-o.0, o.1)) }
    fn mul(self, o: Q) -> Q { Q::new(self.0 * o.0, self.1 * o.1) }
    fn div(self, o: Q) -> Q { Q::new(self.0 * o.1, self.1 * o.0) }
    fn abs(self) -> Q { Q(self.0.abs(), self.1) }
    fn show(self) -> String { if self.1 == 1 { self.0.to_string() } else { format!("{}/{}", self.0, self.1) } }
}

impl PartialOrd for Q {
    fn partial_cmp(&self, o: &Q) -> Option<Ordering> { Some(self.cmp(o)) }
}
impl Ord for Q {
    fn cmp(&self, o: &Q) -> Ordering { (self.0 * o.1).cmp(&(o.0 * self.1)) }
}

/// A worker attempt's known active interval `[from, to)`, with its task mix
/// key and its dispatch decision's agent configuration.
struct Run { attempt: String, from: i64, to: i64, mix: String, config: Option<String> }

/// An integration operation of an attempt: (ref, state, reason, created, integrated).
type Op = (String, String, Option<String>, i64, bool);

/// Canonical inputs, read once.
struct Fleet {
    horizon: i64,
    runs: Vec<Run>,
    /// Spans in which some attempt's activity is unknown (pre-log or end not
    /// marked), with that attempt's configuration.
    unknown: Vec<(i64, i64, Option<String>)>,
    coverage: BTreeMap<&'static str, usize>,
    /// First acceptance evidence time per accepted task (contracts §6 `A`),
    /// with the configuration of the attempt that produced it.
    accepted: Vec<(i64, Option<String>)>,
    /// Display labels (`<kind> <agent_version>`) per configuration id.
    labels: BTreeMap<String, String>,
    /// Integration operations per attempt, in creation order.
    operations: BTreeMap<String, Vec<Op>>,
}

fn table(db: &Connection, name: &str) -> rusqlite::Result<bool> {
    db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)", [name], |r| r.get(0))
}

fn load(db: &Connection, horizon: i64) -> Result<std::result::Result<Fleet, &'static str>> {
    if !table(db, "attempt_lifecycle")? { return Ok(Err("predates_lifecycle_log")); }
    let decisions = table(db, "dispatch_decisions")?;
    let mix = if decisions && table(db, "task_classifications")? {
        "(SELECT c.class||'/'||c.band FROM dispatch_decisions d JOIN task_classifications c ON c.classification_id=d.classification_id WHERE d.attempt_id=a.id)"
    } else { "NULL" };
    let config = if decisions { "(SELECT d.chosen_configuration_id FROM dispatch_decisions d WHERE d.attempt_id=a.id)" } else { "NULL" };
    let mark = |state: &str| format!("(SELECT l.unix_ms FROM attempt_lifecycle l WHERE l.attempt_id=a.id AND l.state='{state}')");
    let sql = format!("SELECT a.id,a.state,{},{},(SELECT min(l.unix_ms) FROM attempt_lifecycle l WHERE l.attempt_id=a.id AND l.state IN ('completed','failed','cancelled','lost')),{mix},{config}
        FROM attempts a ORDER BY a.rowid", mark("reserved"), mark("running"));
    type Row = (String, String, Option<i64>, Option<i64>, Option<i64>, Option<String>, Option<String>);
    let rows: Vec<Row> = db.prepare(&sql)?.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let configs: BTreeMap<String, Option<String>> = rows.iter().map(|r| (r.0.clone(), r.6.clone())).collect();
    let log_start = rows.iter().filter_map(|r| r.2).min();
    let (mut runs, mut unknown, mut coverage) = (Vec::new(), Vec::new(), BTreeMap::new());
    for key in ["attempts", "running_intervals", "open_censored", "never_running", "predates_lifecycle_log", "end_unknown"] { coverage.insert(key, 0); }
    let mut count = |key: &'static str| *coverage.entry(key).or_default() += 1;
    for (attempt, state, reserved, running, ended, class, config) in rows {
        count("attempts");
        let terminal = TERMINAL.contains(&state.as_str());
        match (reserved, running, ended) {
            // Predates the log: ended before it (terminal, no mark), else active at an unknown time.
            (None, _, _) => {
                count("predates_lifecycle_log");
                if !(terminal && ended.is_none()) { unknown.push((log_start.unwrap_or(i64::MIN), ended.unwrap_or(horizon), config)); }
            }
            (Some(_), None, _) => count("never_running"),
            (Some(_), Some(from), Some(to)) => { count("running_intervals"); runs.push(Run { attempt, from, to, mix: class.unwrap_or(UNCLASSIFIED.into()), config }); }
            (Some(_), Some(from), None) if !terminal => {
                count("running_intervals");
                count("open_censored");
                runs.push(Run { attempt, from, to: horizon, mix: class.unwrap_or(UNCLASSIFIED.into()), config });
            }
            (Some(_), Some(from), None) => { count("end_unknown"); unknown.push((from, horizon, config)); }
        }
    }
    // Per task, its first acceptance evidence and the attempt whose result it is.
    let mut first = BTreeMap::<String, (i64, String)>::new();
    let mut stmt = db.prepare("SELECT c.task_id,CASE WHEN c.route='verify_only' THEN r.created_unix_ms ELSE
            (SELECT min(k.created_unix_ms) FROM integration_operations i JOIN integrated_commits k ON k.operation_id=i.operation_id WHERE i.verified_result_id=r.result_id) END,
            s.attempt_id
        FROM task_contracts c JOIN result_submissions s ON s.task_id=c.task_id AND s.contract_revision=c.contract_revision
        JOIN verified_results r ON r.submission_id=s.submission_id
        WHERE c.task_id IN (SELECT id FROM tasks) AND c.contract_revision=(SELECT max(contract_revision) FROM task_contracts WHERE task_id=c.task_id)")?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?, r.get::<_, String>(2)?)))? {
        let (task, at, attempt) = row?;
        let Some(at) = at else { continue };
        let slot = first.entry(task).or_insert((at, attempt.clone()));
        if (at, &attempt) < (slot.0, &slot.1) { *slot = (at, attempt); }
    }
    drop(stmt);
    let accepted = first.into_values().map(|(at, attempt)| (at, configs.get(&attempt).cloned().flatten())).collect();
    // `<kind> <agent_version>` (contracts §2): derived for display, never an identity.
    let labels = if decisions && table(db, "agent_configurations")? {
        db.prepare("SELECT configuration_id,json_extract(canonical_json,'$.kind')||' '||json_extract(canonical_json,'$.agent_version') FROM agent_configurations
            WHERE json_valid(canonical_json)")?.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))?
            .filter_map(|r| r.map(|(id, label)| label.map(|l| (id, l))).transpose()).collect::<rusqlite::Result<_>>()?
    } else { BTreeMap::new() };
    let mut operations: BTreeMap<String, Vec<Op>> = BTreeMap::new();
    let mut stmt = db.prepare("SELECT s.attempt_id,o.ref_name,o.state,o.reason,o.created_unix_ms,
        o.state='integrated' OR EXISTS(SELECT 1 FROM integrated_commits k WHERE k.operation_id=o.operation_id)
        FROM integration_operations o JOIN verified_results r ON r.result_id=o.verified_result_id JOIN result_submissions s ON s.submission_id=r.submission_id
        ORDER BY s.attempt_id,o.created_unix_ms,o.operation_id")?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))))? {
        let (attempt, op) = row?;
        operations.entry(attempt).or_default().push(op);
    }
    Ok(Ok(Fleet { horizon, runs, unknown, coverage, accepted, labels, operations }))
}

/// Round half up to the nearest whole number of agents.
fn level(active_ms: i128, span_ms: i128) -> i64 { ((2 * active_ms + span_ms) / (2 * span_ms)) as i64 }

fn overlap(a: (i64, i64), b: (i64, i64)) -> i64 { (a.1.min(b.1) - a.0.max(b.0)).max(0) }

#[derive(Default)]
struct Bucket { windows: usize, span_ms: i128, active_ms: i128, accepted: i128, mix: BTreeMap<String, i128> }

fn shares(mix: &BTreeMap<String, i128>) -> BTreeMap<String, Q> {
    let total: i128 = mix.values().sum();
    mix.iter().filter(|(_, ms)| **ms > 0).map(|(k, ms)| (k.clone(), Q::new(*ms, total.max(1)))).collect()
}

fn tvd(a: &BTreeMap<String, Q>, b: &BTreeMap<String, Q>) -> Q {
    let keys: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    let zero = Q::int(0);
    keys.into_iter().fold(zero, |sum, k| sum.add(a.get(k).copied().unwrap_or(zero).sub(b.get(k).copied().unwrap_or(zero)).abs())).mul(Q::new(1, 2))
}

/// Windows, buckets and M35 over every worker attempt (`config` `None`) or
/// over the attempts of one agent configuration: its active time, the
/// acceptances of its attempts, and the unknown spans of its attempts or of
/// attempts without a configuration.
fn fan_out(f: &Fleet, since: Option<i64>, window_ms: i64, config: Option<&str>) -> (Value, Value) {
    let mine = |c: &Option<String>| config.is_none_or(|config| c.as_deref() == Some(config));
    let unknown: Vec<(i64, i64)> = f.unknown.iter().filter(|u| config.is_none() || u.2.is_none() || mine(&u.2)).map(|u| (u.0, u.1)).collect();
    let mut windows: BTreeMap<i64, (i128, i128, BTreeMap<String, i128>)> = BTreeMap::new();
    for run in f.runs.iter().filter(|r| mine(&r.config)) {
        if run.to <= run.from { continue; }
        for w in run.from.div_euclid(window_ms)..=(run.to - 1).div_euclid(window_ms) {
            let ms = overlap((run.from, run.to), (w * window_ms, (w + 1) * window_ms)) as i128;
            let entry = windows.entry(w).or_default();
            entry.0 += ms;
            *entry.2.entry(run.mix.clone()).or_default() += ms;
        }
    }
    for (at, _) in f.accepted.iter().filter(|a| mine(&a.1)) { windows.entry(at.div_euclid(window_ms)).or_default().1 += 1; }
    let mut excluded: BTreeMap<&str, usize> = ["incomplete", "concurrency_unknown", "outside_window"].into_iter().map(|k| (k, 0)).collect();
    let mut buckets: BTreeMap<i64, Bucket> = BTreeMap::new();
    for (w, (active, accepted, mix)) in windows {
        let span = (w * window_ms, (w + 1) * window_ms);
        let reason = if since.is_some_and(|since| span.0 < since) { "outside_window" } else if span.1 > f.horizon { "incomplete" }
            else if unknown.iter().any(|u| overlap(*u, span) > 0) { "concurrency_unknown" } else {
                let b = buckets.entry(level(active, window_ms as i128)).or_default();
                b.windows += 1;
                b.span_ms += window_ms as i128;
                b.active_ms += active;
                b.accepted += accepted;
                for (k, ms) in mix { *b.mix.entry(k).or_default() += ms; }
                continue;
            };
        *excluded.get_mut(reason).unwrap() += 1;
    }
    let per_hour = |b: &Bucket| Q::new(b.accepted * HOUR_MS, b.span_ms);
    let reference = buckets.iter().find(|(k, b)| **k >= 1 && b.accepted > 0).map(|(k, b)| (*k, per_hour(b), shares(&b.mix)));
    let (mut rows, mut previous, mut differs, mut unclassified) = (Vec::new(), None::<(i64, Q)>, false, false);
    for (k, b) in &buckets {
        let (k, thr, mix) = (*k, per_hour(b), shares(&b.mix));
        let mut row = json!({"level": k, "windows": b.windows, "window_ms": b.span_ms as i64, "active_ms": b.active_ms as i64,
            "mean_active": Q::new(b.active_ms, b.span_ms).show(), "accepted": b.accepted as i64, "accepted_per_hour": thr.show(),
            "mix": mix.iter().map(|(c, s)| (c.clone(), json!(s.show()))).collect::<serde_json::Map<_, _>>()});
        if k == 0 {
            row["per_agent_per_hour"] = unavailable("level_zero");
            row["m35"] = unavailable("level_zero");
            row["marginal_per_added_agent_per_hour"] = unavailable("level_zero");
            rows.push(row);
            continue;
        }
        row["per_agent_per_hour"] = json!(thr.div(Q::int(k as i128)).show());
        row["m35"] = match &reference {
            Some((r, rthr, _)) => json!(thr.div(Q::int(k as i128).mul(rthr.div(Q::int(*r as i128)))).show()),
            None => unavailable("no_accepted_throughput"),
        };
        row["marginal_per_added_agent_per_hour"] = match previous {
            Some((p, pthr)) => json!(thr.sub(pthr).div(Q::int((k - p) as i128)).show()),
            None => Value::Null,
        };
        if let Some((_, _, rmix)) = &reference {
            let distance = tvd(&mix, rmix);
            row["mix_tvd"] = json!(distance.show());
            differs |= distance > Q::new(MAX_TVD.0, MAX_TVD.1);
        }
        unclassified |= mix.contains_key(UNCLASSIFIED);
        previous = Some((k, thr));
        rows.push(row);
    }
    let top = buckets.keys().copied().filter(|k| *k >= 1).max();
    let mut reasons = Vec::new();
    if unclassified { reasons.push("classification_unknown"); }
    if differs { reasons.push("task_mix_differs"); }
    let comparability = json!({"test": "class_band_active_time_tvd", "max_tvd": Q::new(MAX_TVD.0, MAX_TVD.1).show(), "reference": "reference bucket",
        "label": if reasons.is_empty() { "comparable" } else { "descriptive" }, "reasons": reasons});
    let mut m35 = json!({"window_ms": window_ms, "window_minutes": window_ms / 60_000, "level_rule": "round_half_up(time_weighted_active_attempts)", "scope": "worker_attempts",
        "reference_level": reference.as_ref().map(|r| r.0), "level": top, "comparability": comparability.clone(), "label": comparability["label"].clone()});
    m35["value"] = match (&reference, top) {
        _ if buckets.is_empty() => unavailable("no_complete_window"),
        (None, _) => unavailable("no_accepted_throughput"),
        (Some((r, _, _)), Some(top)) if *r == top => unavailable("single_concurrency_level"),
        (Some(_), Some(top)) => rows.iter().find(|row| row["level"] == top).map_or(Value::Null, |row| row["m35"].clone()),
        (Some(_), None) => unavailable("no_accepted_throughput"),
    };
    if let (Some((r, rthr, _)), Some(top)) = (&reference, top) {
        m35["reference_per_agent_per_hour"] = json!(rthr.div(Q::int(*r as i128)).show());
        if *r != top { m35["marginal_per_added_agent_per_hour"] = rows.iter().find(|row| row["level"] == top).map_or(Value::Null, |row| row["marginal_per_added_agent_per_hour"].clone()); }
    }
    let detail = json!({"window_ms": window_ms, "window_minutes": window_ms / 60_000, "horizon_unix_ms": f.horizon, "coverage": f.coverage,
        "windows": {"bucketed": buckets.values().map(|b| b.windows).sum::<usize>(), "excluded": excluded}, "buckets": rows, "comparability": comparability});
    (detail, m35)
}

/// M35 per agent configuration (the dispatch decision's content-addressed
/// arm, contracts §2), where one has active time or an acceptance: the same
/// buckets, reference and comparability rule over its own attempts. Attempts
/// and acceptances without a configuration are counted apart, never assigned.
fn per_configuration(f: &Fleet, since: Option<i64>, window_ms: i64) -> (Value, Value) {
    let ids: BTreeSet<&String> = f.runs.iter().filter_map(|r| r.config.as_ref()).chain(f.accepted.iter().filter_map(|a| a.1.as_ref())).collect();
    let (mut detail, mut metric) = (serde_json::Map::new(), serde_json::Map::new());
    for id in ids {
        let (d, mut m) = fan_out(f, since, window_ms, Some(id));
        let attempts: BTreeSet<&str> = f.runs.iter().filter(|r| r.config.as_ref() == Some(id)).map(|r| r.attempt.as_str()).collect();
        let label = f.labels.get(id).map_or(Value::Null, |l| json!(l));
        detail.insert(id.clone(), json!({"display_label": label, "attempts": attempts.len(), "windows": d["windows"], "buckets": d["buckets"], "comparability": d["comparability"]}));
        if let Value::Object(o) = &mut m { for key in ["window_ms", "window_minutes", "level_rule", "scope", "comparability"] { o.remove(key); } }
        m["display_label"] = label;
        metric.insert(id.clone(), m);
    }
    let unconfigured = json!({"attempts": f.runs.iter().filter(|r| r.config.is_none()).count(), "accepted": f.accepted.iter().filter(|a| a.1.is_none()).count()});
    (json!({"configurations": detail, "configuration_unknown": unconfigured}), json!({"configurations": metric, "configuration_unknown": unconfigured}))
}

/// The attempt's own concurrency level: time-weighted active attempts over its active interval.
fn experienced(f: &Fleet, attempt: &str) -> Option<i64> {
    let run = f.runs.iter().find(|r| r.attempt == attempt && r.to > r.from)?;
    let span = (run.from, run.to);
    if f.unknown.iter().any(|u| overlap((u.0, u.1), span) > 0) { return None; }
    let active: i128 = f.runs.iter().map(|r| overlap((r.from, r.to), span) as i128).sum();
    Some(level(active, (span.1 - span.0) as i128))
}

fn ratio(numerator: usize, denominator: usize) -> Value {
    let mut body = json!({"numerator": numerator, "denominator": denominator});
    if denominator == 0 { body["value"] = Value::Null; body["reason"] = json!("empty_denominator"); } else { body["value"] = json!(format!("{numerator}/{denominator}")); }
    body
}

/// A conflict/rebase event the integrator records: the merge onto the target
/// conflicted, or the target moved since the candidate was built.
fn event(state: &str, reason: Option<&str>) -> Option<&'static str> {
    match (state, reason) {
        ("blocked", Some("merge_conflict")) => Some("merge_conflict"),
        ("discarded", Some("stale_base")) => Some("stale_base"),
        _ => None,
    }
}

/// M36: attempts with a conflict/rebase event before their first integration / attempts reaching integration.
fn conflicts(f: &Fleet, since: Option<i64>) -> Value {
    let (mut reached, mut conflicted, mut events) = (0, 0, BTreeMap::<&str, usize>::new());
    let mut by_target: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut by_bucket: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for (attempt, ops) in &f.operations {
        if since.is_some_and(|since| ops.iter().map(|o| o.3).min().is_none_or(|first| first < since)) { continue; }
        let before = |ops: &[&Op]| {
            let integrated = ops.iter().filter(|o| o.4).map(|o| o.3).min();
            ops.iter().filter(|o| !o.4 && integrated.is_none_or(|at| o.3 <= at)).filter_map(|o| event(&o.1, o.2.as_deref())).collect::<Vec<_>>()
        };
        let all: Vec<_> = ops.iter().collect();
        let found = before(&all);
        reached += 1;
        if !found.is_empty() { conflicted += 1; }
        for e in &found { *events.entry(e).or_default() += 1; }
        let bucket = experienced(f, attempt).map_or("unknown".to_owned(), |k| k.to_string());
        let b = by_bucket.entry(bucket).or_default();
        b.1 += 1;
        if !found.is_empty() { b.0 += 1; }
        let targets: BTreeSet<&String> = ops.iter().map(|o| &o.0).collect();
        for target in targets {
            let on: Vec<_> = ops.iter().filter(|o| &o.0 == target).collect();
            let t = by_target.entry(target.clone()).or_default();
            t.1 += 1;
            if !before(&on).is_empty() { t.0 += 1; }
        }
    }
    let split = |m: BTreeMap<String, (usize, usize)>| m.into_iter().map(|(k, (n, d))| (k, ratio(n, d))).collect::<serde_json::Map<_, _>>();
    let mut body = ratio(conflicted, reached);
    body["events"] = json!(events);
    body["by_target"] = json!(split(by_target));
    body["by_bucket"] = json!(split(by_bucket));
    body["scope"] = json!("integrator_observed");
    body["event_rule"] = json!("blocked/merge_conflict or discarded/stale_base on an operation created no later than the attempt's first integrated operation");
    body["not_observed"] = json!(["worker_side_rebase"]);
    body
}

fn m34() -> Value {
    json!({"value": unavailable("coordinator_usage_not_attributed"), "missing": ["coordinator_usage_scope", "coordinator_allocation_rule"],
        "detail": "the coordinator has no canonical attempt: its Codex rollouts are unbound (no role `coordinator` scope) and no versioned allocation rule exists; worker figures never include it"})
}

fn m37() -> Value {
    json!({"value": unavailable("supersession_reason_not_recorded"), "missing": ["accepted_supersession_reason"],
        "detail": "no canonical record says an attempt was superseded or abandoned because a sibling changed the same area; candidate selections name a winner, not why another was superseded"})
}

fn named(id: &str, mut body: Value) -> Value {
    let (_, name, definition) = NAMES.iter().find(|(n, _, _)| *n == id).copied().unwrap_or(("", "", ""));
    body["definition"] = json!(definition);
    body["name"] = json!(name);
    body
}

fn now() -> i64 { jiff::Timestamp::now().as_millisecond() }

/// `--window-minutes`: a whole number of windows per UTC day, so windows align to UTC midnight.
pub fn window_ms(minutes: i64) -> Result<i64> {
    anyhow::ensure!((1..=DAY_MINUTES).contains(&minutes) && DAY_MINUTES % minutes == 0,
        "--window-minutes must divide 1440 (a whole number of windows per UTC day), got {minutes}");
    Ok(minutes * 60_000)
}

fn computed(project: &Path, since: Option<i64>, window_ms: i64) -> Result<(Value, BTreeMap<String, Value>)> {
    let path = project.join(".state/state.db");
    let loaded = if path.exists() { load(&*crate::telemetry::read_only(&path)?, now())? } else { Err("no_state_store") };
    let (detail, m35, m36) = match loaded {
        Ok(f) => {
            let (mut detail, mut m35) = fan_out(&f, since, window_ms, None);
            let (by_detail, by_metric) = per_configuration(&f, since, window_ms);
            detail["by_configuration"] = by_detail;
            m35["by_configuration"] = by_metric;
            (detail, m35, conflicts(&f, since))
        }
        Err(reason) => (unavailable(reason), json!({"value": unavailable(reason)}), json!({"value": unavailable(reason)})),
    };
    let metrics = BTreeMap::from([("M34".to_owned(), named("M34", m34())), ("M35".to_owned(), named("M35", m35)),
        ("M36".to_owned(), named("M36", m36)), ("M37".to_owned(), named("M37", m37()))]);
    Ok((detail, metrics))
}

/// M34–M37 for `telemetry <slug> report`.
pub fn metrics(project: &Path, since: Option<i64>) -> Result<BTreeMap<String, Value>> {
    Ok(computed(project, since, window_ms(DEFAULT_WINDOW_MINUTES)?)?.1)
}

/// `accounting fleet [--window-minutes N]`: windows, buckets, coverage, the
/// per-configuration split and M34–M37.
pub fn read(project: &Path, window_minutes: i64) -> Result<Value> {
    let (mut detail, metrics) = computed(project, None, window_ms(window_minutes)?)?;
    if detail.get("status").is_some() { detail = json!({"status": "unavailable", "reason": detail["reason"].clone()}); }
    Ok(json!({"fleet": detail, "metrics": metrics}))
}

/// Text view: one line per bucket and per metric; unknown is `n/a (<reason>)`.
pub fn text(value: &Value) -> String {
    let show = |v: &Value| match v {
        Value::String(s) => s.clone(),
        Value::Null => "n/a".to_owned(),
        Value::Object(o) => format!("n/a ({})", o.get("reason").and_then(Value::as_str).unwrap_or("unknown")),
        other => other.to_string(),
    };
    let fleet = &value["fleet"];
    let mut out = String::new();
    if fleet["status"] == "unavailable" { out += &format!("fleet {}\n", show(fleet)); } else {
        let w = &fleet["windows"];
        out += &format!("windows {} ms: {} bucketed, excluded {}\n", fleet["window_ms"], w["bucketed"],
            w["excluded"].as_object().into_iter().flatten().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(", "));
        for b in fleet["buckets"].as_array().into_iter().flatten() {
            out += &format!("bucket k={} windows={} accepted={} per_hour={} per_agent={} m35={} marginal={}\n", b["level"], b["windows"], b["accepted"],
                show(&b["accepted_per_hour"]), show(&b["per_agent_per_hour"]), show(&b["m35"]), show(&b["marginal_per_added_agent_per_hour"]));
        }
    }
    for (id, m) in value["metrics"].as_object().into_iter().flatten() {
        let shown = match &m["value"] { Value::Null => format!("n/a ({})", m["reason"].as_str().unwrap_or("unknown")), v => show(v) };
        let label = m["label"].as_str().map(|l| format!(" ({l})")).unwrap_or_default();
        out += &format!("{id} {} {shown}{label}\n", m["name"].as_str().unwrap_or(""));
        for (config, c) in m["by_configuration"]["configurations"].as_object().into_iter().flatten() {
            let name = c["display_label"].as_str().map(|l| format!(" ({l})")).unwrap_or_default();
            out += &format!("{id} configuration {config}{name} {} ({})\n", show(&c["value"]), c["label"].as_str().unwrap_or(""));
        }
    }
    out
}

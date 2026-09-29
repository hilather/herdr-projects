//! Fleet efficiency (docs/telemetry/contracts-accounting.md §10; plan
//! TM2.8, doc 07 M34–M37, doc 10 §5a "Fan-out" and "Coordinator overhead"),
//! derived at read time (nothing is stored): attempt lifecycle marks
//! (contracts §4: an attempt is active from its `running` mark to its terminal
//! mark), dispatch decisions with their task classification, acceptance
//! evidence (contracts §6 `A`), integration operations and the owner's
//! supersession reasons from canonical `state.db` (read-only), the latest
//! valuation revision from the sidecar, and attempt worktree reflogs (a gated,
//! read-only `git`, counts only). M35 buckets fixed activity windows by their
//! time-weighted active-attempt count; M36 counts integrator-observed and,
//! apart, worker-observed conflict/rebase events; M34 prices the coordinator
//! scope (`coordinator-scope-v1`) against the project's lifecycle cost and
//! allocates it by rule `coordinator-allocation-v1`; M37 prices attempts
//! superseded because a sibling changed the same area. The coordinator has no
//! canonical attempt, so its time and cost never enter a worker figure.
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

fn gcd(a: i128, b: i128) -> i128 { if b == 0 { a.abs() } else { gcd(b, a % b) } }

impl Q {
    fn new(n: i128, d: i128) -> Q {
        let g = gcd(n, d).max(1) * d.signum();
        Q(n / g, d / g)
    }
    /// Checked forms for money (decimal amounts): `None` on overflow, never wrapped.
    fn try_add(self, o: Q) -> Option<Q> {
        let l = (self.1 / gcd(self.1, o.1).max(1)).checked_mul(o.1)?;
        Some(Q::new(self.0.checked_mul(l / self.1)?.checked_add(o.0.checked_mul(l / o.1)?)?, l))
    }
    fn try_mul(self, o: Q) -> Option<Q> {
        let (g1, g2) = (gcd(self.0, o.1).max(1), gcd(o.0, self.1).max(1));
        Some(Q::new((self.0 / g1).checked_mul(o.0 / g2)?, (self.1 / g2).checked_mul(o.1 / g1)?))
    }
    fn try_div(self, o: Q) -> Option<Q> { if o.0 == 0 { None } else { self.try_mul(Q::new(o.1, o.0)) } }
    /// A plain non-negative decimal string (a stored amount).
    fn decimal(text: &str) -> Option<Q> {
        let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
        if whole.is_empty() || !whole.bytes().chain(fraction.bytes()).all(|b| b.is_ascii_digit()) || whole.len() + fraction.len() > 36 { return None; }
        Some(Q::new(format!("{whole}{fraction}").parse().ok()?, 10i128.checked_pow(fraction.len() as u32)?))
    }
    /// Exact decimal when the denominator divides a power of ten, else `n/d`.
    fn money(self) -> String {
        let (mut d, mut twos, mut fives) = (self.1, 0u32, 0u32);
        while d % 2 == 0 { d /= 2; twos += 1; }
        while d % 5 == 0 { d /= 5; fives += 1; }
        let scale = twos.max(fives);
        let Some(m) = 10i128.checked_pow(scale).and_then(|p| self.0.checked_mul(p / self.1)).filter(|_| d == 1) else { return self.show() };
        let digits = format!("{:0>width$}", m.abs(), width = scale as usize + 1);
        let (int, frac) = digits.split_at(digits.len() - scale as usize);
        format!("{}{int}{}{frac}", if m < 0 { "-" } else { "" }, if scale > 0 { "." } else { "" })
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
struct Run { attempt: String, task: String, from: i64, to: i64, mix: String, config: Option<String> }

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
    let sql = format!("SELECT a.id,a.state,{},{},(SELECT min(l.unix_ms) FROM attempt_lifecycle l WHERE l.attempt_id=a.id AND l.state IN ('completed','failed','cancelled','lost')),{mix},{config},a.task_id
        FROM attempts a ORDER BY a.rowid", mark("reserved"), mark("running"));
    type Row = (String, String, Option<i64>, Option<i64>, Option<i64>, Option<String>, Option<String>, String);
    let rows: Vec<Row> = db.prepare(&sql)?.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let configs: BTreeMap<String, Option<String>> = rows.iter().map(|r| (r.0.clone(), r.6.clone())).collect();
    let log_start = rows.iter().filter_map(|r| r.2).min();
    let (mut runs, mut unknown, mut coverage) = (Vec::new(), Vec::new(), BTreeMap::new());
    for key in ["attempts", "running_intervals", "open_censored", "never_running", "predates_lifecycle_log", "end_unknown"] { coverage.insert(key, 0); }
    let mut count = |key: &'static str| *coverage.entry(key).or_default() += 1;
    for (attempt, state, reserved, running, ended, class, config, task) in rows {
        count("attempts");
        let terminal = TERMINAL.contains(&state.as_str());
        match (reserved, running, ended) {
            // Predates the log: ended before it (terminal, no mark), else active at an unknown time.
            (None, _, _) => {
                count("predates_lifecycle_log");
                if !(terminal && ended.is_none()) { unknown.push((log_start.unwrap_or(i64::MIN), ended.unwrap_or(horizon), config)); }
            }
            (Some(_), None, _) => count("never_running"),
            (Some(_), Some(from), Some(to)) => { count("running_intervals"); runs.push(Run { attempt, task, from, to, mix: class.unwrap_or(UNCLASSIFIED.into()), config }); }
            (Some(_), Some(from), None) if !terminal => {
                count("running_intervals");
                count("open_censored");
                runs.push(Run { attempt, task, from, to: horizon, mix: class.unwrap_or(UNCLASSIFIED.into()), config });
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

/// Worker-observed scope of M36: at most this many attempt worktrees are read
/// per report, and this many reflog entries per worktree.
const WORKER_ATTEMPTS: usize = 64;
const REFLOG_ENTRIES: &str = "1000";
const WORKTREES_PER_ATTEMPT: usize = 16;

/// The kind of a worktree `HEAD` reflog entry from its action (the subject up
/// to the first `:`); the rest of the subject (a commit message) is dropped
/// unread. `None` for anything that is not a rebase or merge.
fn reflog_kind(subject: &str) -> Option<&'static str> {
    let action = subject.split_once(':').map_or(subject, |a| a.0);
    let rebase = action.starts_with("rebase") || action.starts_with("pull --rebase");
    match action {
        _ if rebase && action.ends_with("(start)") => Some("rebase"),
        _ if rebase && action.ends_with("(continue)") => Some("rebase_conflict_resolved"),
        "commit (merge)" => Some("merge_conflict_resolved"),
        _ if action.starts_with("merge ") || (action.starts_with("pull") && !rebase) => Some("merge"),
        _ => None,
    }
}

/// Rebase and merge events a worker made in its own attempt worktrees
/// (`<project>/.state/worktrees/<attempt>/repo-NN`) up to `until` (ms), read
/// from each worktree's `HEAD` reflog with a gated, read-only `git`. Counts per
/// kind only; or why nothing was observed.
fn worker_events(project: &Path, attempt: &str, until: Option<i64>) -> std::result::Result<BTreeMap<&'static str, usize>, &'static str> {
    use crate::execution_guard::GatedSpawn;
    if attempt.is_empty() || attempt.starts_with('.') || attempt.contains(['/', '\\']) { return Err("worktree_absent"); }
    let root = project.join(".state/worktrees").join(attempt);
    let mut trees: Vec<std::path::PathBuf> = std::fs::read_dir(&root).map_err(|_| "worktree_absent")?.filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_str().is_some_and(|n| n.len() == 7 && n.starts_with("repo-") && n[5..].bytes().all(|b| b.is_ascii_digit())))
        .map(|e| e.path()).filter(|p| p.is_dir()).collect();
    trees.sort();
    trees.truncate(WORKTREES_PER_ATTEMPT);
    if trees.is_empty() { return Err("worktree_absent"); }
    let mut events = BTreeMap::new();
    for tree in trees {
        let out = std::process::Command::new("git").current_dir(&tree)
            .env_remove("GIT_DIR").env_remove("GIT_WORK_TREE").env_remove("GIT_INDEX_FILE").env_remove("GIT_OBJECT_DIRECTORY").env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .args(["--no-optional-locks", "reflog", "show", "--date=unix", "--format=%gd%x09%gs", "-n", REFLOG_ENTRIES, "HEAD", "--"])
            .output_gated().map_err(|_| "git_unavailable")?;
        if !out.status.success() { return Err("reflog_unavailable"); }
        for line in out.stdout.split(|b| *b == b'\n') {
            let text = String::from_utf8_lossy(line);
            let Some((selector, subject)) = text.split_once('\t') else { continue };
            let Some(kind) = reflog_kind(subject) else { continue };
            let seconds = selector.rsplit_once("@{").and_then(|(_, t)| t.strip_suffix('}')).and_then(|t| t.parse::<i64>().ok());
            if until.is_some_and(|until| seconds.is_none_or(|s| s.saturating_mul(1000) > until)) { continue; }
            *events.entry(kind).or_default() += 1;
        }
    }
    Ok(events)
}

/// M36: attempts with a conflict/rebase event before their first integration / attempts reaching integration.
fn conflicts(f: &Fleet, project: &Path, since: Option<i64>) -> Value {
    let (mut reached, mut conflicted, mut events) = (0, 0, BTreeMap::<&str, usize>::new());
    let (mut worker, mut worker_events_seen, mut worker_coverage) = ((0, 0), BTreeMap::<&str, usize>::new(), BTreeMap::<&str, usize>::new());
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
        // Worker-observed scope, apart from the integrator's: the attempt's own worktree reflog.
        let until = ops.iter().filter(|o| o.4).map(|o| o.3).min();
        let observed = if reached > WORKER_ATTEMPTS { Err("read_limit") } else { worker_events(project, attempt, until) };
        match observed {
            Ok(seen) => {
                *worker_coverage.entry("observed").or_default() += 1;
                worker.1 += 1;
                if !seen.is_empty() { worker.0 += 1; }
                for (kind, n) in seen { *worker_events_seen.entry(kind).or_default() += n; }
            }
            Err(reason) => *worker_coverage.entry(reason).or_default() += 1,
        }
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
    let mut worker_side = ratio(worker.0, worker.1);
    worker_side["scope"] = json!("worker_observed");
    worker_side["events"] = json!(worker_events_seen);
    worker_side["coverage"] = json!(worker_coverage);
    worker_side["event_rule"] = json!("a rebase, merge, or resolved rebase/merge conflict in the attempt worktree's HEAD reflog, recorded no later than its first integrated operation; counts only, the reflog subject is never kept");
    body["worker_observed"] = worker_side;
    body
}

/// M34 coordinator usage scope, versioned (§10).
pub const COORDINATOR_SCOPE: &str = "coordinator-scope-v1";
const COORDINATOR_SCOPE_RULE: &str = "Codex rollouts collected from a scanned execution home, bound to no attempt and outside every task worktree, \
    whose session_meta.cwd is the project directory (the coordinator pane's working directory)";
/// M34 allocation of coordinator cost to tasks, versioned (§10; plan doc 05 §5a).
pub const ALLOCATION_RULE: &str = "coordinator-allocation-v1";
const ALLOCATION_RULE_TEXT: &str = "each priced coordinator entry is split evenly across the tasks with an attempt running over its usage interval; \
    none running: unallocated; an unknown activity span over it: allocation_unknown";

/// A valued ledger entry: its priced amount (`None`: unpriced) and usage interval.
struct Entry { priced: Option<(String, Q)>, interval: Option<(i64, i64)> }

#[derive(PartialEq)]
enum Scope { Worker(String), Coordinator, Unattributed, Outside }

/// A collected Codex session in the latest valuation revision (§4), or
/// collected with records the revision has not valued yet (`valued` false).
struct Session { scope: Scope, start: Option<i64>, valued: bool, entries: Vec<Entry> }

/// Every collected session with its scope: bound to a known attempt (worker),
/// in a task worktree but not bound (or bound to an unknown attempt:
/// unattributed), the coordinator scope, or outside the project.
fn sessions(project: &Path, attempts: &BTreeSet<String>) -> Result<std::result::Result<Vec<Session>, &'static str>> {
    let Some(db) = crate::telemetry::sidecar::read(project)? else { return Ok(Err("collection_not_run")) };
    let cost = super::cost::cost(&db, None)?;
    if cost.get("status").is_some() { return Ok(Err("not_priced")); }
    let dir = std::fs::canonicalize(project).unwrap_or_else(|_| project.to_path_buf()).to_string_lossy().into_owned();
    let roots = [crate::telemetry::sanitize::home_prefix(&dir), dir];
    type Source = (Option<String>, bool, bool, Option<i64>, i64);
    let mut sources: BTreeMap<String, Source> = BTreeMap::new();
    let mut stmt = db.prepare("SELECT session_id,binding,attempt_id,cwd,cwd_attempt,session_unix_ms,records FROM rollout_sources ORDER BY path_digest")?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, String>(3)?,
        r.get::<_, Option<String>>(4)?, r.get::<_, Option<i64>>(5)?, r.get::<_, i64>(6)?)))? {
        let (session, binding, attempt, cwd, cwd_attempt, start, records) = row?;
        let s = sources.entry(session).or_insert((None, false, false, None, 0));
        if binding == "bound" { s.0 = s.0.take().or(attempt); }
        s.1 |= cwd_attempt.is_some() || binding != "unbound";
        s.2 |= roots.contains(&cwd);
        s.3 = match (s.3, start) { (Some(a), Some(b)) => Some(a.min(b)), (a, b) => a.or(b) };
        s.4 += records;
    }
    let mut valued: BTreeMap<String, Vec<Entry>> = BTreeMap::new();
    for session in cost["sessions"].as_array().into_iter().flatten() {
        let Some(id) = session["session_id"].as_str() else { continue };
        let entries = valued.entry(id.to_owned()).or_default();
        for e in session["entries"].as_array().into_iter().flatten() {
            let v = &e["valuation"];
            let priced = if v["status"] == "priced" { v["currency"].as_str().zip(v["amount"].as_str().and_then(Q::decimal)).map(|(c, a)| (c.to_owned(), a)) } else { None };
            let i = &e["usage_interval"];
            entries.push(Entry { priced, interval: i["from_unix_ms"].as_i64().zip(i["to_unix_ms"].as_i64()) });
        }
    }
    Ok(Ok(sources.into_iter().map(|(id, (bound, worktree, root, start, records))| {
        let scope = match bound {
            Some(a) if attempts.contains(&a) => Scope::Worker(a),
            Some(_) => Scope::Unattributed,
            None if worktree => Scope::Unattributed,
            None if root => Scope::Coordinator,
            None => Scope::Outside,
        };
        let entries = valued.remove(&id);
        Session { scope, start, valued: entries.is_some() || records == 0, entries: entries.unwrap_or_default() }
    }).collect()))
}

/// A published-rate estimate over entries (§4 rules): priced amounts per
/// currency, never added across currencies, with what is missing.
#[derive(Default, Clone)]
struct Sum { by_currency: BTreeMap<String, Q>, entries: usize, unpriced: usize, not_valued: usize, not_observed: usize, overflow: bool }

impl Sum {
    fn priced(&mut self, currency: &str, amount: Q) {
        let slot = self.by_currency.entry(currency.to_owned()).or_insert(Q::int(0));
        match slot.try_add(amount) { Some(v) => *slot = v, None => self.overflow = true }
    }
    fn session(&mut self, s: &Session) {
        if !s.valued { self.not_valued += 1; }
        for e in &s.entries {
            self.entries += 1;
            match &e.priced { Some((c, a)) => self.priced(c, *a), None => self.unpriced += 1 }
        }
    }
    fn merge(&mut self, o: &Sum) {
        for (c, a) in &o.by_currency { self.priced(c, *a); }
        self.entries += o.entries;
        self.unpriced += o.unpriced;
        self.not_valued += o.not_valued;
        self.not_observed += o.not_observed;
        self.overflow |= o.overflow;
    }
    /// What keeps the amount from being complete: unknown is never 0.
    fn gaps(&self) -> Vec<&'static str> {
        [(self.unpriced, "entries_unpriced"), (self.not_valued, "usage_not_valued"), (self.not_observed, "usage_not_observed")]
            .into_iter().filter(|(n, _)| *n > 0).map(|(_, g)| g).collect()
    }
    fn coverage(&self) -> Value {
        json!({"entries": self.entries, "priced": self.entries - self.unpriced, "unpriced": self.unpriced, "sessions_not_valued": self.not_valued,
            "attempts_without_observed_usage": self.not_observed})
    }
    fn estimate(&self) -> Value {
        let gaps = self.gaps();
        if self.overflow { return unavailable("amount_overflow"); }
        match self.by_currency.len() {
            0 if gaps.is_empty() => json!({"status": "complete", "currency": null, "amount": "0"}),
            0 => { let mut v = unavailable("no_priced_entries"); v["gaps"] = json!(gaps); v }
            1 => {
                let (currency, amount) = self.by_currency.iter().next().map(|(c, a)| (c.clone(), a.money())).unwrap_or_default();
                if gaps.is_empty() { json!({"status": "complete", "currency": currency, "amount": amount}) }
                else { json!({"status": "partial", "gaps": gaps, "currency": currency, "priced_amount": amount}) }
            }
            _ => json!({"status": "unavailable", "reason": "mixed_currency",
                "priced_by_currency": self.by_currency.iter().map(|(c, a)| (c.clone(), a.money())).collect::<BTreeMap<_, _>>()}),
        }
    }
    /// The single currency and priced amount, or why there is none.
    fn single(&self) -> std::result::Result<Option<(&String, Q)>, &'static str> {
        if self.overflow { return Err("amount_overflow"); }
        match self.by_currency.len() { 0 => Ok(None), 1 => Ok(self.by_currency.iter().next().map(|(c, a)| (c, *a))), _ => Err("mixed_currency") }
    }
}

/// `part / whole` of one currency as an exact rational; `partial` with the
/// priced share when anything is missing (`reasons`). Returns the value and
/// the metric's `reason` for a `null` value.
fn share(part: &Sum, whole: &Sum, reasons: &[String]) -> (Value, Option<&'static str>) {
    let (p, w) = match (part.single(), whole.single()) {
        (Err(e), _) | (_, Err(e)) => return (unavailable(e), None),
        (_, Ok(None)) if reasons.is_empty() => return (Value::Null, Some("empty_denominator")),
        (_, Ok(None)) => return ({ let mut v = unavailable("no_priced_entries"); v["reasons"] = json!(reasons); v }, None),
        (p, Ok(Some((c, w)))) => (p.ok().flatten().filter(|(pc, _)| *pc == c).map_or(Q::int(0), |(_, a)| a), w),
    };
    if w.0 == 0 { return (Value::Null, Some("empty_denominator")); }
    let Some(ratio) = p.try_div(w) else { return (unavailable("amount_overflow"), None) };
    if reasons.is_empty() { (json!(ratio.show()), None) } else { (json!({"status": "partial", "reasons": reasons, "priced_share": ratio.show()}), None) }
}

fn in_window(since: Option<i64>, start: Option<i64>) -> bool { since.is_none_or(|since| start.is_some_and(|t| t >= since)) }

/// Attempts with a known running interval in the window.
fn ran(f: Option<&Fleet>, since: Option<i64>) -> BTreeSet<&str> {
    f.map(|f| f.runs.iter().filter(|r| since.is_none_or(|t| r.from >= t)).map(|r| r.attempt.as_str()).collect()).unwrap_or_default()
}

/// M34: coordinator exclusive cost / total project lifecycle cost (coordinator
/// plus worker attempts), coordinator cost per active worker-thread-hour, and
/// the allocation to tasks under rule v1. Never part of a worker figure.
fn m34(f: Option<&Fleet>, usage: &std::result::Result<Vec<Session>, &'static str>, since: Option<i64>) -> Value {
    let mut body = json!({"scope": COORDINATOR_SCOPE, "scope_rule": COORDINATOR_SCOPE_RULE, "allocation_rule": ALLOCATION_RULE,
        "basis": super::cost::BASIS, "excluded_from": ["M35", "per_arm_worker_figures"]});
    let sessions = match usage { Ok(s) => s, Err(reason) => { body["value"] = unavailable(reason); return body; } };
    let coordinator: Vec<&Session> = sessions.iter().filter(|s| s.scope == Scope::Coordinator && in_window(since, s.start)).collect();
    let (mut coord, mut workers, mut observed, mut unattributed) = (Sum::default(), Sum::default(), BTreeSet::new(), 0);
    for s in &coordinator { coord.session(s); }
    for s in sessions.iter().filter(|s| in_window(since, s.start)) {
        match &s.scope {
            Scope::Worker(a) => { workers.session(s); observed.insert(a.as_str()); }
            Scope::Unattributed => unattributed += 1,
            _ => {}
        }
    }
    workers.not_observed = ran(f, since).difference(&observed).count();
    body["coordinator"] = json!({"sessions": coordinator.len(), "estimate": coord.estimate(), "coverage": coord.coverage()});
    if coordinator.is_empty() {
        body["value"] = unavailable("coordinator_usage_not_observed");
        body["detail"] = json!("no collected Codex session is in the coordinator scope: a coordinator run by another agent kind, or from an execution home that is not scanned, is not observed; never 0");
        return body;
    }
    let mut total = coord.clone();
    total.merge(&workers);
    let mut reasons: Vec<String> = coord.gaps().iter().map(|g| format!("coordinator_{g}")).chain(workers.gaps().iter().map(|g| format!("worker_{g}"))).collect();
    if unattributed > 0 { reasons.push("unattributed_worker_usage".into()); }
    body["total_project_lifecycle_cost"] = json!({"estimate": total.estimate(), "worker_attempts": observed.len(), "worker_coverage": workers.coverage(),
        "unattributed_sessions": unattributed});
    let (value, reason) = share(&coord, &total, &reasons);
    body["value"] = value;
    if let Some(reason) = reason { body["reason"] = json!(reason); }
    body["per_active_worker_thread_hour"] = thread_hour(f, &coord, since);
    body["allocation"] = allocation(f, &coordinator, &coord);
    body
}

/// Coordinator cost / active worker-thread-hours (Σ known active intervals in the window).
fn thread_hour(f: Option<&Fleet>, coord: &Sum, since: Option<i64>) -> Value {
    let Some(f) = f else { return json!({"value": unavailable("predates_lifecycle_log")}) };
    if f.unknown.iter().any(|u| since.is_none_or(|t| u.1 > t)) { return json!({"value": unavailable("active_time_unknown")}); }
    let active: i128 = f.runs.iter().map(|r| (r.to - r.from.max(since.unwrap_or(i64::MIN))).max(0) as i128).sum();
    let mut body = json!({"active_worker_thread_ms": active as i64, "open_censored": f.coverage.get("open_censored")});
    let (currency, amount) = match coord.single() { Err(e) => { body["value"] = unavailable(e); return body; } Ok(None) => { body["value"] = unavailable("no_priced_entries"); return body; } Ok(Some(c)) => c };
    body["currency"] = json!(currency);
    if active == 0 { body["value"] = Value::Null; body["reason"] = json!("empty_denominator"); return body; }
    let Some(rate) = amount.try_mul(Q::int(HOUR_MS)).and_then(|a| a.try_div(Q::int(active))) else { body["value"] = unavailable("amount_overflow"); return body };
    body["value"] = if coord.gaps().is_empty() { json!(rate.money()) } else { json!({"status": "partial", "gaps": coord.gaps(), "priced_value": rate.money()}) };
    body
}

/// Rule v1: each priced coordinator entry split evenly across the tasks with
/// an attempt running over its usage interval. The coordinator total is shown beside it.
fn allocation(f: Option<&Fleet>, coordinator: &[&Session], coord: &Sum) -> Value {
    let mut body = json!({"rule": ALLOCATION_RULE, "rule_text": ALLOCATION_RULE_TEXT, "coordinator_total": coord.estimate(), "unpriced_entries": coord.unpriced});
    let Some(f) = f else { body["status"] = json!("unavailable"); body["reason"] = json!("predates_lifecycle_log"); return body };
    let currency = match coord.single() { Err(e) => { body["status"] = json!("unavailable"); body["reason"] = json!(e); return body; } Ok(c) => c.map(|c| c.0.clone()) };
    let (mut by_task, mut unallocated, mut unknown, mut overflow) = (BTreeMap::<&str, Q>::new(), Q::int(0), Q::int(0), false);
    let mut add = |slot: &mut Q, amount: Q| match slot.try_add(amount) { Some(v) => *slot = v, None => overflow = true };
    for entry in coordinator.iter().flat_map(|s| &s.entries) {
        let Some((_, amount)) = &entry.priced else { continue };
        let Some((from, to)) = entry.interval else { add(&mut unknown, *amount); continue };
        if f.unknown.iter().any(|u| u.0 <= to && from < u.1) { add(&mut unknown, *amount); continue; }
        let tasks: BTreeSet<&str> = f.runs.iter().filter(|r| r.from <= to && from < r.to).map(|r| r.task.as_str()).collect();
        if tasks.is_empty() { add(&mut unallocated, *amount); continue; }
        let each = amount.try_div(Q::int(tasks.len() as i128)).unwrap_or(Q::int(0));
        for task in tasks { add(by_task.entry(task).or_insert(Q::int(0)), each); }
    }
    if overflow { body["status"] = json!("unavailable"); body["reason"] = json!("amount_overflow"); return body; }
    body["currency"] = json!(currency);
    body["by_task"] = json!(by_task.iter().map(|(t, a)| (t.to_string(), a.money())).collect::<BTreeMap<_, _>>());
    body["unallocated"] = json!(unallocated.money());
    body["allocation_unknown"] = json!(unknown.money());
    body
}

/// Supersession buckets (§10): overlap waste (M37's numerator) and the rest.
const BUCKETS: [&str; 5] = ["sibling_changed_same_area", "duplicate_effort", "other", "unexplained_abandonment", "not_superseded"];

/// M37: lifecycle cost of attempts superseded or abandoned because a sibling
/// changed the same area (an accepted supersession reason) / total worker
/// lifecycle cost. Unexplained abandonment (cancelled or lost, no reason) is
/// its own bucket; an attempt that ran without observed usage makes it partial.
fn m37(db: &Connection, f: Option<&Fleet>, usage: &std::result::Result<Vec<Session>, &'static str>, since: Option<i64>) -> Result<Value> {
    let mut body = json!({"reason_source": "attempt_supersessions (canonical 0060, owner-recorded)", "numerator_reason": "sibling_changed_same_area",
        "basis": super::cost::BASIS});
    if !table(db, "attempt_supersessions")? {
        body["value"] = unavailable("supersession_reason_not_recorded");
        body["missing"] = json!(["accepted_supersession_reason"]);
        return Ok(body);
    }
    let reasons: BTreeMap<String, String> = db.prepare("SELECT attempt_id,reason FROM attempt_supersessions")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let states: BTreeMap<String, String> = db.prepare("SELECT id,state FROM attempts")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut recorded = BTreeMap::<&str, usize>::new();
    for reason in reasons.values() { *recorded.entry(reason.as_str()).or_default() += 1; }
    body["records"] = json!(recorded);
    let sessions = match usage { Ok(s) => s, Err(reason) => { body["value"] = unavailable(reason); return Ok(body); } };
    let (mut per_attempt, mut unattributed) = (BTreeMap::<&str, Sum>::new(), 0);
    for s in sessions.iter().filter(|s| in_window(since, s.start)) {
        match &s.scope {
            Scope::Worker(a) => per_attempt.entry(a.as_str()).or_default().session(s),
            Scope::Unattributed => unattributed += 1,
            _ => {}
        }
    }
    for attempt in ran(f, since) { per_attempt.entry(attempt).or_insert_with(|| Sum { not_observed: 1, ..Sum::default() }); }
    let mut buckets: BTreeMap<&str, (usize, Sum)> = BUCKETS.iter().map(|b| (*b, (0, Sum::default()))).collect();
    let mut total = Sum::default();
    for (attempt, sum) in &per_attempt {
        let bucket = match (reasons.get(*attempt), states.get(*attempt).map(String::as_str)) {
            (Some(reason), _) => reason.as_str(),
            (None, Some("cancelled" | "lost")) => "unexplained_abandonment",
            _ => "not_superseded",
        };
        if let Some(b) = buckets.get_mut(bucket) { b.0 += 1; b.1.merge(sum); }
        total.merge(sum);
    }
    let mut gaps: Vec<String> = total.gaps().iter().map(|g| g.to_string()).collect();
    if unattributed > 0 { gaps.push("unattributed_worker_usage".into()); }
    let (value, reason) = share(&buckets["sibling_changed_same_area"].1, &total, &gaps);
    body["value"] = value;
    if let Some(reason) = reason { body["reason"] = json!(reason); }
    body["buckets"] = json!(buckets.iter().map(|(b, (n, s))| (b.to_string(), json!({"attempts": n, "estimate": s.estimate()}))).collect::<BTreeMap<_, _>>());
    body["total_lifecycle_cost"] = json!({"attempts": per_attempt.len(), "estimate": total.estimate(), "coverage": total.coverage(), "unattributed_sessions": unattributed});
    Ok(body)
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
    let db = if path.exists() { Some(crate::telemetry::read_only(&path)?) } else { None };
    let loaded = match &db { Some(db) => load(db, now())?, None => Err("no_state_store") };
    let (detail, m35, m36) = match &loaded {
        Ok(f) => {
            let (mut detail, mut m35) = fan_out(f, since, window_ms, None);
            let (by_detail, by_metric) = per_configuration(f, since, window_ms);
            detail["by_configuration"] = by_detail;
            m35["by_configuration"] = by_metric;
            (detail, m35, conflicts(f, project, since))
        }
        Err(reason) => (unavailable(reason), json!({"value": unavailable(reason)}), json!({"value": unavailable(reason)})),
    };
    let fleet = loaded.as_ref().ok();
    let (m34, m37) = match &db {
        Some(db) => {
            let attempts: BTreeSet<String> = db.prepare("SELECT id FROM attempts")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            let usage = sessions(project, &attempts)?;
            (m34(fleet, &usage, since), m37(db, fleet, &usage, since)?)
        }
        None => (json!({"value": unavailable("no_state_store")}), json!({"value": unavailable("no_state_store")})),
    };
    let metrics = BTreeMap::from([("M34".to_owned(), named("M34", m34)), ("M35".to_owned(), named("M35", m35)),
        ("M36".to_owned(), named("M36", m36)), ("M37".to_owned(), named("M37", m37))]);
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
        let shown = match &m["value"] {
            Value::Null => format!("n/a ({})", m["reason"].as_str().unwrap_or("unknown")),
            v if v["status"] == "partial" => format!("partial {} ({})", show(&v["priced_share"]),
                v["reasons"].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")),
            v => show(v),
        };
        let label = m["label"].as_str().map(|l| format!(" ({l})")).unwrap_or_default();
        out += &format!("{id} {} {shown}{label}\n", m["name"].as_str().unwrap_or(""));
        for (config, c) in m["by_configuration"]["configurations"].as_object().into_iter().flatten() {
            let name = c["display_label"].as_str().map(|l| format!(" ({l})")).unwrap_or_default();
            out += &format!("{id} configuration {config}{name} {} ({})\n", show(&c["value"]), c["label"].as_str().unwrap_or(""));
        }
    }
    out
}

/// Refuse the owner's supersession CLI inside a worker execution context
/// (contracts-review.md §9 markers): the working directory is a task worktree,
/// or `HOME` is a recorded worker execution home. Markers, not authority: the
/// store refuses every worker principal and the schema accepts only the owner.
fn refuse_worker_context(project: &Path) -> Result<()> {
    const REFUSED: &str = "`accounting supersede` records the project owner (operator:cli) and refuses to run inside a worker execution context";
    let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    if let (Ok(cwd), Some(root)) = (std::env::current_dir(), project.parent())
        && let Ok(rest) = canonical(&cwd).strip_prefix(canonical(root)) {
        let parts: Vec<&std::ffi::OsStr> = rest.components().map(|c| c.as_os_str()).take(3).collect();
        anyhow::ensure!(!(parts.len() == 3 && parts[1] == ".state" && parts[2] == "worktrees"), "{REFUSED}: the working directory is a task worktree");
    }
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else { return Ok(()) };
    let db = crate::telemetry::read_only(&project.join(".state/state.db"))?;
    let mut homes: Vec<String> = db.prepare("SELECT json_extract(report,'$.preparation.profile.execution_home') FROM native_profiles
        WHERE json_type(report,'$.preparation.profile.execution_home')='text'")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    if table(&db, "collector_bindings")? {
        homes.extend(db.prepare("SELECT DISTINCT execution_home FROM collector_bindings WHERE execution_home IS NOT NULL")?.query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?);
    }
    let home = canonical(&home);
    anyhow::ensure!(!homes.iter().any(|h| canonical(Path::new(h)) == home), "{REFUSED}: HOME is a worker execution home");
    Ok(())
}

/// `accounting supersede`: the owner's accepted supersession reason for an
/// ended attempt, written through the store's own transaction (§10).
pub fn supersede(project: &Path, request: crate::store::SupersessionRequest) -> Result<Value> {
    let path = project.join(".state/state.db");
    anyhow::ensure!(path.is_file(), "no canonical store for this project");
    refuse_worker_context(project)?;
    let record = crate::store::SqliteStore::open(&path)?.record_attempt_supersession(&request, "operator:cli", now())?;
    Ok(json!({"supersession": record}))
}

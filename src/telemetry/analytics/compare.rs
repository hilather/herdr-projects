//! TM4.4 configuration comparisons (`analytics-comparison.v2`,
//! docs/telemetry/contracts-evaluation.md §1–§5): `telemetry <slug> compare`.
//! Cohort membership is the query service's (`lifecycle::evaluate`, the same
//! lineage `query --drill` pages); arms are `AgentConfiguration`s from the
//! dispatch log. Read-only over `state.db` and the sidecar. Every result is
//! observational: no ranking across task classes, no causal claim, never read
//! by dispatch.
use super::estimators::{self as est, Bounds, unavailable};
use super::lifecycle::{self, Task};
use super::registry::{self, COMPARISON, Cohort};
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub const SCHEMA_VERSION: u32 = 1;
const ROUTING: &str = "never: advisory evidence only; nothing here is read by dispatch or admission";

/// `telemetry <slug> compare ...`
#[derive(clap::Args, Clone, Debug)]
pub struct Args {
    /// Comparable registry metric (`M02`, `M07`, `M30`); repeatable or comma-separated.
    #[arg(long = "metric", required = true, value_delimiter = ',')]
    pub metrics: Vec<String>,
    /// Comparison arm: `configuration` (the content-addressed AgentConfiguration).
    #[arg(long, default_value = "configuration")]
    pub by: String,
    /// `terminal_cohort` or `assignment_cohort`; M30 uses `activity_window` by default.
    #[arg(long)]
    pub cohort: Option<String>,
    /// Window start, inclusive, UTC Unix ms.
    #[arg(long)]
    pub from: Option<i64>,
    /// Window end, exclusive, UTC Unix ms.
    #[arg(long)]
    pub to: Option<i64>,
    /// Assignment-cohort follow-up horizon (ms).
    #[arg(long)]
    pub horizon_ms: Option<i64>,
    /// Only tasks of this task class (latest classification).
    #[arg(long)]
    pub task_class: Option<String>,
    /// Bootstrap seed override (decimal or 0x hex), recorded as `source: override`.
    #[arg(long, value_parser = parse_seed)]
    pub seed: Option<u64>,
    #[arg(long)]
    pub json: bool,
}

fn parse_seed(text: &str) -> Result<u64, String> {
    match text.strip_prefix("0x") { Some(hex) => u64::from_str_radix(hex, 16), None => text.parse() }.map_err(|e| e.to_string())
}

fn reject(detail: Value) -> anyhow::Error { anyhow::anyhow!("compare rejected: {detail}") }

struct Request { metrics: Vec<(&'static str, &'static str, bool)>, cohort: Cohort, from: Option<i64>, to: Option<i64>, horizon: Option<i64>, class: Option<String>,
    bootstrap: est::Bootstrap, seed_source: Value }

fn request(args: &Args) -> Result<Request> {
    if args.by != "configuration" { return Err(reject(json!({"code": "dimension_unsupported", "by": args.by, "supported": ["configuration"]}))); }
    let mut metrics = Vec::new();
    for name in &args.metrics {
        let (metric, version) = registry::resolve(name).map_err(reject)?;
        let Some(&entry) = COMPARISON.metrics.iter().find(|(id, definition, _)| *id == metric.id && *definition == version.definition) else {
            return Err(reject(json!({"code": "comparison_unsupported", "metric": name,
                "supported": COMPARISON.metrics.iter().map(|m| m.1).collect::<Vec<_>>()})));
        };
        if !metrics.contains(&entry) { metrics.push(entry); }
    }
    let first_candidates = metrics.iter().any(|m| m.0 == "M30");
    if first_candidates && metrics.len() > 1 { return Err(reject(json!({"code": "cohort_unsupported", "detail": "compare M30 separately: it uses a submission cohort"}))); }
    let cohort = match &args.cohort { None => if first_candidates { Cohort::Activity } else { Cohort::Terminal }, Some(text) => Cohort::parse(text).map_err(|code| reject(json!({"code": code, "cohort": text,
        "accepted": if first_candidates { vec!["activity_window"] } else { vec!["terminal_cohort", "assignment_cohort"] }})))? };
    if (cohort == Cohort::Activity) != first_candidates { return Err(reject(json!({"code": "cohort_unsupported", "cohort": cohort.as_str(), "supported": if first_candidates { vec!["activity_window"] } else { vec!["terminal_cohort", "assignment_cohort"] }}))); }
    if args.from.zip(args.to).is_some_and(|(from, to)| from >= to) { return Err(reject(json!({"code": "empty_window", "from": args.from, "to": args.to}))); }
    if args.horizon_ms.is_some() && cohort != Cohort::Assignment { return Err(reject(json!({"code": "horizon_unsupported", "cohort": cohort.as_str()}))); }
    if args.horizon_ms.is_some_and(|h| h <= 0) { return Err(reject(json!({"code": "horizon_out_of_range"}))); }
    let mut bootstrap = COMPARISON.bootstrap;
    let seed_source = match args.seed {
        None => json!(COMPARISON.version),
        Some(seed) => { bootstrap.seed = seed; json!({"source": "override", "registry": {"seed": COMPARISON.bootstrap.seed_hex(), "source": COMPARISON.version}}) }
    };
    Ok(Request { metrics, cohort, from: args.from, to: args.to, horizon: args.horizon_ms, class: args.task_class.clone(), bootstrap, seed_source })
}

/// One dispatch decision: the chosen arm and the logged per-arm probabilities.
struct Decision { attempt: String, configuration: String, eligible: BTreeMap<String, i64>, chooser: String, decided: i64 }

struct Sources { tasks: Vec<Task>, decisions: BTreeMap<String, Decision>, configurations: BTreeMap<String, Value>, bands: BTreeMap<String, String>,
    reviewed: Option<BTreeSet<String>>, dispatch_log: bool }

fn table(db: &rusqlite::Connection, name: &str) -> rusqlite::Result<bool> {
    db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)", [name], |r| r.get(0))
}

fn load(project: &Path) -> Result<Sources> {
    let tasks = lifecycle::load(project)?;
    let db = crate::telemetry::read_only(&project.join(".state/state.db"))?;
    let db = db.unchecked_transaction()?;
    let dispatch_log = table(&db, "dispatch_decisions")?;
    let mut decisions = BTreeMap::new();
    let mut configurations = BTreeMap::new();
    if dispatch_log {
        let rows: Vec<(String, String, String, String, i64)> = db.prepare("SELECT attempt_id,chosen_configuration_id,eligible,chooser_kind,decided_unix_ms FROM dispatch_decisions ORDER BY attempt_id")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect::<rusqlite::Result<_>>()?;
        for (attempt, configuration, eligible, chooser, decided) in rows {
            let eligible: BTreeMap<String, i64> = serde_json::from_str::<Vec<Value>>(&eligible).unwrap_or_default().iter()
                .filter_map(|e| Some((e["configuration_id"].as_str()?.to_owned(), e["probability_ppm"].as_i64()?))).collect();
            decisions.insert(attempt.clone(), Decision { attempt, configuration, eligible, chooser, decided });
        }
        for row in db.prepare("SELECT configuration_id,canonical_json FROM agent_configurations")?.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (id, canonical) = row?;
            configurations.insert(id, serde_json::from_str(&canonical).unwrap_or(Value::Null));
        }
    }
    // Latest classification wins, in the query service's order (`lifecycle_classes`).
    let mut bands = BTreeMap::new();
    if table(&db, "task_classifications")? {
        for row in db.prepare("SELECT task_id,band FROM task_classifications ORDER BY task_id,created_unix_ms,revision")?.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (task, band) = row?;
            bands.insert(task, band);
        }
    }
    let reviewed = if table(&db, "review_opportunities")? {
        Some(db.prepare("SELECT DISTINCT task_id FROM review_opportunities")?.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<_>>()?)
    } else { None };
    Ok(Sources { tasks, decisions, configurations, bands, reviewed, dispatch_log })
}

/// One cohort member with its arm (or why it has none).
struct Unit<'a> { task: &'a Task, outcome: String, class: String, band: String, arm: Result<String, &'static str>, first: Option<&'a Decision> }

/// The task's arm: the one configuration every attempt was dispatched on.
fn arm<'a>(task: &Task, decisions: &'a BTreeMap<String, Decision>) -> (Result<String, &'static str>, Option<&'a Decision>) {
    if task.attempts.is_empty() { return (Err("not_assigned"), None); }
    let own: Vec<&Decision> = task.attempts.iter().filter_map(|a| decisions.get(&a.id)).collect();
    let first = own.iter().min_by(|a, b| (a.decided, &a.attempt).cmp(&(b.decided, &b.attempt))).copied();
    if own.len() < task.attempts.len() { return (Err("configuration_unknown"), first); }
    let chosen: BTreeSet<&str> = own.iter().map(|d| d.configuration.as_str()).collect();
    if chosen.len() > 1 { return (Err("mixed_configuration"), first); }
    (Ok(chosen.into_iter().next().unwrap_or_default().to_owned()), first)
}

/// `(numerator, denominator)` one task contributes: M02 (accepted, 1); M07 (attempts, accepted).
fn contribution(metric: &str, unit: &Unit) -> (i64, i64) {
    let accepted = i64::from(unit.outcome == "accepted");
    if metric == "M07" { (unit.task.attempts.len() as i64, accepted) } else { (accepted, 1) }
}

fn counts<'a>(items: impl Iterator<Item = &'a str>) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    for item in items { *out.entry(item.to_owned()).or_default() += 1; }
    out
}

/// Same distribution of `key` across every arm (compared as proportions, exactly).
fn same_mix(groups: &[Vec<&Unit>], key: for<'a, 'b> fn(&'a Unit<'b>) -> &'a str) -> bool {
    let dists: Vec<(BTreeMap<String, usize>, usize)> = groups.iter().map(|g| (counts(g.iter().map(|u| key(u))), g.len())).collect();
    let labels: BTreeSet<&String> = dists.iter().flat_map(|(d, _)| d.keys()).collect();
    dists.windows(2).all(|w| labels.iter().all(|l| w[0].0.get(*l).copied().unwrap_or(0) * w[1].1 == w[1].0.get(*l).copied().unwrap_or(0) * w[0].1))
}

fn band_of<'a>(u: &'a Unit) -> &'a str { &u.band }
fn policy_of<'a>(u: &'a Unit) -> &'a str { u.task.first_candidate.as_ref().map_or("unknown", |f| f.policy_digest.as_str()) }
fn class_of<'a>(u: &'a Unit) -> &'a str { &u.class }

/// A computed arm cell: its JSON, point value and interval bounds (for rankings).
struct Cell { value: Value, point: Option<(i128, i128)>, bounds: Option<Bounds>, shown: bool }

/// Beta-binomial empirical-Bayes pooled M02 (`beta_binomial_eb.v1`): the
/// class rate shrunk toward the arm's all-class rate `Y/N` with prior
/// strength `κ` pseudo-tasks: `(y + κY/N) / (n + κ)` = `(yN + κY) / ((n + κ)N)`.
fn pooled(y: i64, n: i64, all: (i64, i64)) -> Value {
    let (big_y, big_n) = (i128::from(all.0), i128::from(all.1));
    let kappa = i128::from(COMPARISON.prior_strength);
    let (num, den) = (i128::from(y) * big_n + kappa * big_y, (i128::from(n) + kappa) * big_n);
    json!({"model": COMPARISON.pooling, "value": est::reduced(num, den), "decimal": est::decimal(num, den, 4), "prior_mean": format!("{}/{}", all.0, all.1),
        "prior_strength": COMPARISON.prior_strength, "shrinkage": est::reduced(kappa, i128::from(n) + kappa)})
}

fn cell(metric: &str, configuration: &str, units: &[&Unit], all: Option<(i64, i64)>, propensity: Option<Value>, spec: &est::Bootstrap, source: &Value) -> Cell {
    let parts: Vec<(i64, i64)> = units.iter().map(|u| contribution(metric, u)).collect();
    let (num, den) = parts.iter().fold((0, 0), |(n, d), (x, y)| (n + x, d + y));
    let shown = units.len() >= COMPARISON.min_tasks as usize;
    let mut out = json!({"configuration_id": configuration, "tasks": units.len(), "numerator": num, "denominator": den,
        "breakdown": counts(units.iter().map(|u| u.outcome.as_str())), "difficulty": counts(units.iter().map(|u| u.band.as_str()))});
    if metric == "M30" { out["policies"] = json!(counts(units.iter().map(|u| policy_of(u)))); }
    let (mut point, mut bounds) = (None, None);
    if !shown {
        // Plan doc 07 §6: suppressed below the registry minimum, counts still shown.
        out["status"] = json!("suppressed");
        out["value"] = unavailable("insufficient_data");
        out["min_sample"] = json!({"value": COMPARISON.min_tasks, "unit": if metric == "M30" { "adjudicated_first_candidates" } else { "terminal_tasks" }, "source": COMPARISON.version});
        for key in ["interval", "pooled", "propensity_weighted"] { out[key] = unavailable("insufficient_data"); }
        return Cell { value: out, point, bounds, shown };
    }
    if den == 0 {
        out["status"] = json!("empty");
        out["value"] = Value::Null;
        out["reason"] = json!("empty_denominator");
    } else {
        out["status"] = json!("shown");
        out["value"] = json!(format!("{num}/{den}"));
        out["decimal"] = json!(est::decimal(i128::from(num), i128::from(den), 4));
        point = Some((i128::from(num), i128::from(den)));
    }
    // Clusters in task-ID order (units arrive sorted by task).
    let (interval, b) = est::ratio_interval(&parts, spec, source);
    out["interval"] = interval;
    bounds = b;
    out["pooled"] = match (metric, all) {
        ("M02", Some(all)) => pooled(num, den, all),
        ("M02", None) => unavailable("pooling_is_per_class"),
        _ => unavailable("pooling_not_declared"),
    };
    out["propensity_weighted"] = propensity.unwrap_or_else(|| unavailable("not_computed"));
    Cell { value: out, point, bounds, shown }
}

fn lcm(a: u128, b: u128) -> Option<u128> { (a / est::gcd(a, b)).checked_mul(b) }

/// Hájek inverse-propensity estimate per arm (`hajek_ipw.v1`) over one
/// class cell, when every decision in it logged a positive probability for
/// every compared arm; otherwise one `unavailable` reason for the cell.
fn propensities(metric: &str, arms: &BTreeMap<&str, Vec<&Unit>>) -> Result<BTreeMap<String, Value>, Value> {
    let all: Vec<&Unit> = arms.values().flatten().copied().collect();
    let deterministic = all.iter().all(|u| u.first.is_some_and(|d| d.eligible.iter().all(|(c, p)| if *c == d.configuration { *p == 1_000_000 } else { *p == 0 })));
    if deterministic {
        return Err(json!({"status": "unavailable", "reason": "deterministic_assignment", "observational": true,
            "detail": "every decision chose its arm with probability 1 (probability_ppm 1000000); no reweighting is possible (TM4.7 randomized policies produce positive propensities)"}));
    }
    let missing = all.iter().filter(|u| !arms.keys().all(|arm| u.first.and_then(|d| d.eligible.get(*arm)).is_some_and(|p| *p > 0))).count();
    if missing > 0 { return Err(json!({"status": "unavailable", "reason": "positivity_violated", "decisions_without_positive_probability": missing})); }
    let mut out = BTreeMap::new();
    for (arm, units) in arms {
        let probs: Vec<u128> = units.iter().map(|u| u.first.and_then(|d| d.eligible.get(*arm)).copied().unwrap_or(1) as u128).collect();
        let Some(l) = probs.iter().try_fold(1u128, |acc, p| lcm(acc, *p)) else { out.insert(arm.to_string(), unavailable("arithmetic_overflow")); continue };
        let weights: Vec<i128> = probs.iter().map(|p| (l / p) as i128).collect();
        let (mut nw, mut dw, mut sw, mut sw2) = (0i128, 0i128, 0i128, 0i128);
        for (w, u) in weights.iter().zip(units) {
            let (n, d) = contribution(metric, u);
            nw += w * i128::from(n);
            dw += w * i128::from(d);
            sw += w;
            sw2 += w * w;
        }
        out.insert(arm.to_string(), if dw == 0 { json!({"method": COMPARISON.propensity, "value": null, "reason": "empty_denominator"}) } else {
            json!({"method": COMPARISON.propensity, "value": est::reduced(nw, dw), "decimal": est::decimal(nw, dw, 4), "tasks": units.len(),
                "effective_sample_size": est::decimal(sw * sw, sw2, 2), "observational": true})
        });
    }
    Ok(out)
}

/// A ranking within one class cell: only when every arm is shown, the
/// intervals separate in point order, and the units are comparable.
fn ranking(higher_is_better: bool, cells: &[(&str, &Cell)], matched: &[&'static str]) -> Value {
    let mut reasons: Vec<Value> = matched.iter().map(|r| json!(r)).collect();
    if cells.len() < 2 { reasons.push(json!("single_arm")); }
    let suppressed: Vec<&str> = cells.iter().filter(|(_, c)| !c.shown).map(|(id, _)| *id).collect();
    if !suppressed.is_empty() { reasons.push(json!({"insufficient_data": suppressed})); }
    if cells.iter().any(|(_, c)| c.shown && (c.bounds.is_none() || c.point.is_none())) { reasons.push(json!("interval_unavailable")); }
    let mut order: Vec<&(&str, &Cell)> = cells.iter().filter(|(_, c)| c.point.is_some() && c.bounds.is_some()).collect();
    order.sort_by(|a, b| { let o = est::compare(a.1.point.unwrap_or_default(), b.1.point.unwrap_or_default()); if higher_is_better { o.reverse() } else { o } });
    if reasons.is_empty() {
        let separated = order.windows(2).all(|w| {
            let (better, worse) = (w[0].1.bounds.unwrap_or(Bounds { lower: (0, 0), upper: (0, 0) }), w[1].1.bounds.unwrap_or(Bounds { lower: (0, 0), upper: (0, 0) }));
            if higher_is_better { est::compare(better.lower, worse.upper).is_gt() } else { est::compare(better.upper, worse.lower).is_lt() }
        });
        if !separated { reasons.push(json!("intervals_overlap")); }
    }
    if !reasons.is_empty() { return json!({"status": "not_supported", "reasons": reasons}); }
    json!({"status": "intervals_separated", "order": order.iter().map(|(id, _)| id).collect::<Vec<_>>(), "higher_is_better": higher_is_better,
        "scope": "this task class and cohort only", "universal": false, "observational": true, "causal": false, "routing": ROUTING})
}

/// Product identity of an arm: the declared components only. A model name
/// appears only when the configuration itself declares `requested_model`;
/// otherwise the arm is a product-level opaque result.
fn identity(configuration: &str, canonical: Option<&Value>) -> Value {
    let Some(c) = canonical else { return json!({"configuration_id": configuration, "identity": unavailable("configuration_not_recorded"),
        "model_identity": {"status": "opaque", "reason": "configuration_not_recorded"}}) };
    let declared = |field: &str| match &c[field] {
        Value::String(v) => json!({"status": "declared", "value": v}),
        _ => json!({"status": "unavailable", "reason": c[format!("{field}_reason")].as_str().unwrap_or("not_declared")}),
    };
    let model_identity = match &c["requested_model"] {
        Value::String(model) => json!({"status": "declared", "requested_model": model}),
        _ => json!({"status": "opaque", "reason": c["requested_model_reason"].as_str().unwrap_or("not_declared"),
            "detail": "the configuration does not declare a model: results are product-level; effective model names are never listed per arm"}),
    };
    json!({"configuration_id": configuration, "label": format!("{} {}", c["kind"].as_str().unwrap_or("unknown"), c["agent_version"].as_str().unwrap_or("unknown")),
        "product": {"kind": c["kind"], "agent_version": c["agent_version"], "adapter": {"id": c["adapter"]["id"], "revision": c["adapter"]["revision"]}},
        "environment": {"environment_names": c["environment_names"], "permission_policy": {"id": c["permission_policy"]["id"], "revision": c["permission_policy"]["revision"]}},
        "declared_capabilities": {"requested_model": declared("requested_model"), "reasoning_effort": declared("reasoning_effort")},
        "model_identity": model_identity})
}

/// Per-attempt usage (contracts §5 bound usage) summed over the arm's attempts:
/// complete, partial (a labelled subtotal, never the total) or unavailable.
fn cost(sidecar: Option<&rusqlite::Connection>, units: &[&Unit]) -> Result<Value> {
    let attempts: Vec<&str> = units.iter().flat_map(|u| u.task.attempts.iter().map(|a| a.id.as_str())).collect();
    let (mut known, mut total, mut reasons) = (0usize, 0i64, BTreeMap::<String, usize>::new());
    for attempt in &attempts {
        let usage = match sidecar { Some(db) => crate::telemetry::sidecar::attempt_usage(db, attempt)?, None => unavailable("collection_not_run") };
        match usage["total_tokens"].as_i64() {
            Some(tokens) => { known += 1; total += tokens; }
            None => *reasons.entry(usage["reason"].as_str().unwrap_or("unknown").to_owned()).or_default() += 1,
        }
    }
    let state = if attempts.is_empty() { "not_applicable" } else if known == attempts.len() { "complete" } else if known == 0 { "unavailable" } else { "partial" };
    Ok(json!({"basis": "usage_tokens", "state": state, "attempts": attempts.len(), "known": known, "missing": attempts.len() - known, "reasons": reasons,
        "total_tokens": if known == 0 { Value::Null } else { json!(total) }, "subtotal": known > 0 && known < attempts.len(),
        "failed_and_cancelled_attempts_included": true}))
}

/// Mixed-model allocation from the sidecar's model segments: counts only,
/// never model names.
fn model_allocation(sidecar: Option<&rusqlite::Connection>, units: &[&Unit]) -> Result<Value> {
    let Some(db) = sidecar else { return Ok(unavailable("collection_not_run")) };
    if !table(db, "session_graph_nodes")? || !table(db, "model_segments")? { return Ok(unavailable("accounting_not_synced")); }
    let mut sessions = db.prepare("SELECT DISTINCT session_id FROM session_graph_nodes WHERE attempt_id=?1 ORDER BY session_id")?;
    let mut segments = db.prepare("SELECT bucket,model FROM model_segments WHERE session_id=?1")?;
    let (mut single, mut mixed, mut unobserved) = (0usize, 0usize, 0usize);
    for unit in units {
        let (mut models, mut mixed_bucket) = (BTreeSet::<String>::new(), false);
        for attempt in &unit.task.attempts {
            let ids: Vec<String> = sessions.query_map([&attempt.id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            for session in ids {
                for row in segments.query_map([&session], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))? {
                    let (bucket, model) = row?;
                    match (bucket.as_str(), model) { ("model", Some(m)) => { models.insert(m); } ("mixed", _) => mixed_bucket = true, _ => {} }
                }
            }
        }
        if models.len() > 1 || mixed_bucket { mixed += 1 } else if models.len() == 1 { single += 1 } else { unobserved += 1 }
    }
    Ok(json!({"single_model_tasks": single, "mixed_model_tasks": mixed, "unobserved_tasks": unobserved, "mixed_model_allocation": mixed > 0,
        "rule": "a task whose sessions switched or mixed models stays in its configuration arm and is flagged; its success and cost are never split across model cohorts",
        "model_names": "withheld"}))
}

/// `telemetry <slug> compare`: the full JSON report.
pub fn run(project: &Path, args: &Args) -> Result<Value> { run_report(project, args, false) }

/// Same cells and labels, without cost/model diagnostics the workspace does
/// not consume. Called only while recording a workspace history revision.
pub(crate) fn workspace(project: &Path, args: &Args) -> Result<Value> { run_report(project, args, true) }

fn run_report(project: &Path, args: &Args, workspace: bool) -> Result<Value> {
    let r = request(args)?;
    let sources = load(project)?;
    let sidecar = crate::telemetry::sidecar::read(project)?;
    let (body, lineage) = lifecycle::evaluate(&sources.tasks, &lifecycle::Request { metric: r.metrics[0].0, cohort: r.cohort, from: r.from, to: r.to, horizon: r.horizon, by: None });
    let by_id: BTreeMap<&str, &Task> = sources.tasks.iter().map(|t| (t.id.as_str(), t)).collect();
    let mut units: Vec<Unit> = Vec::new();
    let mut filtered = 0usize;
    for (bucket, rows) in &lineage {
        let Some(outcome) = bucket.strip_prefix("outcome.") else { continue };
        for (_, id, _) in rows {
            let Some(task) = by_id.get(id.as_str()) else { continue };
            let class = task.dimension("task_class");
            if r.class.as_ref().is_some_and(|c| *c != class) { filtered += 1; continue; }
            let (arm, first) = if r.metrics[0].0 == "M30" {
                let decision = task.first_candidate.as_ref().and_then(|f| sources.decisions.get(&f.attempt));
                (decision.map(|d| d.configuration.clone()).ok_or("configuration_unknown"), decision)
            } else { arm(task, &sources.decisions) };
            units.push(Unit { task, outcome: outcome.to_owned(), band: sources.bands.get(&task.id).cloned().unwrap_or_else(|| "unclassified".into()), class, arm, first });
        }
    }
    units.sort_by(|a, b| a.task.id.cmp(&b.task.id));
    let mut arms: BTreeMap<&str, Vec<&Unit>> = BTreeMap::new();
    let mut unallocated = BTreeMap::<&str, usize>::new();
    for u in &units { match &u.arm { Ok(c) => arms.entry(c.as_str()).or_default().push(u), Err(reason) => *unallocated.entry(reason).or_default() += 1 } }
    let mut exclusions = body["exclusions"].clone();
    if filtered > 0 { exclusions["task_class_filter"] = json!(filtered); }
    let classes: BTreeSet<&str> = arms.values().flatten().map(|u| u.class.as_str()).collect();
    let source = &r.seed_source;

    // Per-arm description: identity, assignments and failures, coverage.
    let mut configurations = Vec::new();
    let mut review = Vec::new();
    let mut notes: Vec<Value> = Vec::new();
    let (mut partial_cost, mut mixed_model) = (Vec::new(), Vec::new());
    for (&configuration, members) in &arms {
        let mut entry = identity(configuration, sources.configurations.get(configuration));
        let decisions: Vec<&Decision> = members.iter().flat_map(|u| u.task.attempts.iter().filter_map(|a| sources.decisions.get(&a.id))).collect();
        entry["assignments"] = json!({"tasks": members.len(), "attempts": members.iter().map(|u| u.task.attempts.len()).sum::<usize>(), "decisions": decisions.len(),
            "chooser_kind": counts(decisions.iter().map(|d| d.chooser.as_str()))});
        entry["outcomes"] = json!(counts(members.iter().map(|u| u.outcome.as_str())));
        entry["attempt_states"] = json!(counts(members.iter().flat_map(|u| u.task.attempts.iter().map(|a| a.state.as_str()))));
        entry["task_classes"] = json!(counts(members.iter().map(|u| u.class.as_str())));
        entry["difficulty"] = json!({"proxy": "task_classifications.band (preassignment rubric)", "bands": counts(members.iter().map(|u| u.band.as_str()))});
        entry["cost"] = if workspace { Value::Null } else { cost(sidecar.as_deref(), members)? };
        if entry["cost"]["state"] != "complete" { partial_cost.push(json!({"configuration_id": configuration, "state": entry["cost"]["state"], "missing": entry["cost"]["missing"]})); }
        entry["review_coverage"] = match &sources.reviewed {
            None => unavailable("review_capture_absent"),
            Some(reviewed) => {
                let n = members.iter().filter(|u| reviewed.contains(&u.task.id)).count();
                review.push((configuration, n, members.len()));
                json!({"reviewed_tasks": n, "tasks": members.len(), "value": format!("{n}/{}", members.len())})
            }
        };
        entry["model_allocation"] = if workspace { Value::Null } else { model_allocation(sidecar.as_deref(), members)? };
        if entry["model_allocation"]["mixed_model_allocation"] == true { mixed_model.push(json!({"configuration_id": configuration, "tasks": entry["model_allocation"]["mixed_model_tasks"]})); }
        configurations.push(entry);
    }

    notes.push(json!({"code": "failures_included", "detail": if r.metrics[0].0 == "M30" { "adjudicated first candidates only; pending cases excluded and shown separately; configuration frozen to the first submission attempt" } else { "failed, cancelled and succeeded-without-evidence tasks stay in every denominator; assignment-cohort unfinished tasks count as not accepted" },
        "breakdown": body["breakdown"]}));
    if !unallocated.is_empty() {
        notes.push(json!({"code": "unallocated_tasks", "detail": "tasks dispatched on several configurations, without a dispatch decision, or never assigned belong to no arm", "counts": unallocated}));
    }
    if review.len() > 1 && review.windows(2).any(|w| w[0].1 * w[1].2 != w[1].1 * w[0].2) {
        notes.push(json!({"code": "uneven_review_coverage", "detail": "arms were reviewed at different rates: review-dependent outcomes are not comparable across them",
            "arms": review.iter().map(|(c, n, d)| json!({"configuration_id": c, "value": format!("{n}/{d}")})).collect::<Vec<_>>()}));
    }
    if !partial_cost.is_empty() { notes.push(json!({"code": "cost_partial", "detail": "usage is missing for some attempts: known totals are subtotals", "arms": partial_cost})); }
    if !mixed_model.is_empty() { notes.push(json!({"code": "mixed_model_allocation", "arms": mixed_model})); }
    if !sources.dispatch_log { notes.push(json!({"code": "dispatch_log_absent", "detail": "store before migration 0050: no arm can be identified"})); }

    // Results per metric: one cell per task class, then all classes (never ranked).
    let mut results = Vec::new();
    for &(metric, definition, higher) in &r.metrics {
        let mut class_cells = Vec::new();
        for &class in &classes {
            let in_class: BTreeMap<&str, Vec<&Unit>> = arms.iter().map(|(c, us)| (*c, us.iter().filter(|u| u.class == class).copied().collect::<Vec<_>>()))
                .filter(|(_, us)| !us.is_empty()).collect();
            let weighted = propensities(metric, &in_class);
            let computed: Vec<(&str, Cell)> = in_class.iter().map(|(c, us)| {
                let all: Vec<&Unit> = arms[c].to_vec();
                let total = all.iter().map(|u| contribution(metric, u)).fold((0, 0), |(n, d), (x, y)| (n + x, d + y));
                let propensity = match &weighted { Ok(map) => map.get(*c).cloned(), Err(reason) => Some(reason.clone()) };
                (*c, cell(metric, c, us, Some(total), propensity, &r.bootstrap, source))
            }).collect();
            let groups: Vec<Vec<&Unit>> = in_class.values().cloned().collect();
            let mut matched: Vec<&'static str> = if same_mix(&groups, band_of) { Vec::new() } else { vec!["difficulty_mix_differs"] };
            if metric == "M30" && !same_mix(&groups, policy_of) { matched.push("verification_policy_mix_differs"); }
            let refs: Vec<(&str, &Cell)> = computed.iter().map(|(c, cell)| (*c, cell)).collect();
            class_cells.push(json!({"task_class": class, "arms": computed.iter().map(|(_, c)| c.value.clone()).collect::<Vec<_>>(),
                "propensity": match &weighted { Ok(_) => json!({"status": "available", "method": COMPARISON.propensity}), Err(reason) => reason.clone() },
                "ranking": ranking(higher, &refs, &matched)}));
        }
        let overall: Vec<Value> = arms.iter().map(|(c, us)| cell(metric, c, us, None, Some(unavailable("per_class_only")), &r.bootstrap, source).value).collect();
        let groups: Vec<Vec<&Unit>> = arms.values().cloned().collect();
        let mut reasons = vec![json!("universal_ranking_not_supported")];
        if classes.len() > 1 || !same_mix(&groups, class_of) { reasons.push(json!("task_class_unmatched")); }
        let m = registry::find(metric).map_or(("", ""), |m| (m.name, m.unit));
        results.push(json!({"metric_id": metric, "definition": definition, "name": m.0, "unit": m.1, "higher_is_better": higher,
            "cells": class_cells, "all_classes": {"arms": overall, "ranking": {"status": "not_supported", "reasons": reasons}}}));
    }

    // Paired analysis for candidate groups: lane C's M42 (same read path as `telemetry report`), restricted to these arms.
    let paired = if workspace { Value::Null } else { match crate::telemetry::quality::metrics(project, r.from)?.remove("M42") {
        None => unavailable("paired_metric_absent"),
        Some(m42) => {
            let pairs: Vec<Value> = m42["pairs"].as_array().into_iter().flatten()
                .filter(|p| arms.contains_key(p["a"].as_str().unwrap_or_default()) && arms.contains_key(p["b"].as_str().unwrap_or_default())).cloned().collect();
            json!({"definition": m42["definition"], "value": m42["value"], "closed_groups": m42["closed_groups"], "min_sample": m42["min_sample"],
                "estimator": m42["estimator"], "acceptance": m42["acceptance"], "pairs": pairs, "window": "selected at or after --from"})
        }
    } };

    Ok(json!({"schema_version": SCHEMA_VERSION, "contract": COMPARISON.version, "registry": registry::VERSION,
        "request": {"metrics": r.metrics.iter().map(|m| m.1).collect::<Vec<_>>(), "by": "configuration", "cohort": r.cohort.as_str(), "from": r.from, "to": r.to,
            "horizon_ms": r.horizon, "task_class": r.class},
        "analysis": {"kind": "observational", "causal": false, "routing": ROUTING,
            "detail": "production assignment is not randomized: stronger arms may receive harder tasks; recorded covariates do not remove selection bias"},
        "estimators": {"bootstrap": {"method": r.bootstrap.method, "resample": "task", "iterations": r.bootstrap.iterations, "seed": r.bootstrap.seed_hex(),
            "level": r.bootstrap.level(), "source": source}, "min_sample": {"value": COMPARISON.min_tasks, "unit": if r.metrics[0].0 == "M30" { "adjudicated_first_candidates_per_configuration_class_cell" } else { "terminal_tasks_per_configuration_class_cell" }, "source": COMPARISON.version},
            "pooling": {"model": COMPARISON.pooling, "prior_strength": COMPARISON.prior_strength, "prior_mean": "arm_all_class_rate"}, "propensity": COMPARISON.propensity},
        "population": {"cohort": r.cohort.as_str(), "members": units.len(), "allocated": units.len() - unallocated.values().sum::<usize>(), "unallocated": unallocated,
            "exclusions": exclusions, "coverage": body["coverage"], "censored": body.get("censored").cloned().unwrap_or(Value::Null)},
        "configurations": configurations, "results": results, "paired": paired, "notes": notes,
        "source_watermarks": {"canonical": {"lifecycle_digest": if workspace { String::new() } else { lifecycle::digest(&sources.tasks) }, "decisions": sources.decisions.len(),
            "configurations": sources.configurations.len()}, "sidecar": sidecar.is_some()}}))
}

/// One line per arm cell for the terminal.
pub fn text(report: &Value) -> String {
    let mut out = format!("{} cohort={} observational causal=false\n", report["contract"].as_str().unwrap_or(""), report["request"]["cohort"].as_str().unwrap_or(""));
    let short = |v: &Value| v.as_str().map_or(String::new(), |s| s.get(..19).unwrap_or(s).to_owned());
    let value = |v: &Value| match v { Value::String(s) => s.clone(), Value::Object(o) => format!("unavailable({})", o.get("reason").and_then(Value::as_str).unwrap_or("?")), _ => "null".into() };
    for result in report["results"].as_array().into_iter().flatten() {
        out += &format!("{} {} {}\n", result["metric_id"].as_str().unwrap_or(""), result["name"].as_str().unwrap_or(""), result["definition"].as_str().unwrap_or(""));
        for c in result["cells"].as_array().into_iter().flatten() {
            out += &format!("  task_class={} ranking={}\n", c["task_class"].as_str().unwrap_or(""), c["ranking"]["status"].as_str().unwrap_or(""));
            for a in c["arms"].as_array().into_iter().flatten() {
                out += &format!("    {} tasks={} value={} interval=[{}, {}] pooled={} {}\n", short(&a["configuration_id"]), a["tasks"], value(&a["value"]),
                    a["interval"]["lower"].as_str().unwrap_or("-"), a["interval"]["upper"].as_str().unwrap_or("-"), value(&a["pooled"]["value"]), a["status"].as_str().unwrap_or(""));
            }
        }
        out += "  all_classes ranking=not_supported(universal_ranking_not_supported)\n";
    }
    for note in report["notes"].as_array().into_iter().flatten() { out += &format!("note {}\n", note["code"].as_str().unwrap_or("")); }
    out
}

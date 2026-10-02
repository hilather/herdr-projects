//! TM4.4 experiment planning and reports (docs/telemetry/contracts-evaluation.md
//! §6–§7): `telemetry <slug> experiments plan|report`. Planning is pure
//! fixed-point arithmetic; reports read the D4 preregistered experiments
//! (`store::protocol_state`, contracts-review.md §7) strictly read-only and
//! add task-clustered intervals and the causal label. Nothing here assigns,
//! launches or routes.
use super::estimators::{self as est, SCALE, unavailable};
use super::registry::{self, COMPARISON};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

pub const PLAN_CONTRACT: &str = "experiment-planning.v1";
pub const REPORT_CONTRACT: &str = "experiment-report.v1";

/// `herdr-farm telemetry <slug> experiments ...`
#[derive(clap::Subcommand)]
pub enum Command {
    /// Sample size to detect a stated difference in a rate metric: unpaired and
    /// paired designs, optionally with a clustered design effect. Reads nothing.
    Plan {
        /// Registry rate metric (unit `ratio`), e.g. `M02`.
        #[arg(long)]
        metric: String,
        /// Reference-arm rate `p₁`, a decimal in (0, 1).
        #[arg(long)]
        baseline_rate: String,
        /// Absolute difference `d` to detect (`p₂ = p₁ + d`), a decimal in (0, 1).
        #[arg(long)]
        min_detectable_effect: String,
        /// Two-sided significance level: 0.10, 0.05 or 0.01.
        #[arg(long, default_value = "0.05")]
        alpha: String,
        /// Power: 0.80, 0.90 or 0.95.
        #[arg(long, default_value = "0.80")]
        power: String,
        /// Paired design: expected discordant-pair share `ψ`; default assumes independent arms within a task.
        #[arg(long)]
        discordance: Option<String>,
        /// Tasks per cluster (shared context or integration target), for the design effect.
        #[arg(long, requires = "icc")]
        cluster_size: Option<u32>,
        /// Intra-cluster correlation `ρ` for the design effect `1 + (m − 1)ρ`.
        #[arg(long, requires = "cluster_size")]
        icc: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// D4 preregistered experiments: intention-to-treat estimates with
    /// exclusions, crossover and task-clustered intervals. Read-only.
    Report {
        /// Protocol ledger sequence to replay to (default: head).
        #[arg(long)]
        as_of: Option<i64>,
        #[arg(long)]
        json: bool,
    },
}

fn reject(detail: Value) -> anyhow::Error { anyhow::anyhow!("experiments rejected: {detail}") }

/// Two-sided `z_{1−α/2}` and one-sided `z_{1−β}` quantiles, nine decimals (units of 10⁻⁹).
const Z_ALPHA: [(&str, u128); 3] = [("0.10", 1_644_853_627), ("0.05", 1_959_963_985), ("0.01", 2_575_829_304)];
const Z_POWER: [(&str, u128); 3] = [("0.80", 841_621_234), ("0.90", 1_281_551_566), ("0.95", 1_644_853_627)];

fn quantile(table: &[(&str, u128)], text: &str, code: &str) -> Result<(u128, u128)> {
    let value = est::parse_fixed(text).ok_or_else(|| reject(json!({"code": code, "value": text})))?;
    table.iter().find(|(k, _)| est::parse_fixed(k) == Some(value)).map(|(_, z)| (value, *z))
        .ok_or_else(|| reject(json!({"code": code, "value": text, "supported": table.iter().map(|t| t.0).collect::<Vec<_>>()})))
}

fn open_unit(text: &str, code: &str) -> Result<u128> {
    match est::parse_fixed(text) { Some(v) if v > 0 && v < SCALE => Ok(v), _ => Err(reject(json!({"code": code, "value": text, "expected": "decimal in (0, 1), at most 9 places"}))) }
}

/// `ceil(a / b)` for positive integers.
fn ceil_div(a: u128, b: u128) -> u128 { a.div_ceil(b) }

/// Plan (contracts-evaluation.md §6). All quantities are integers in units of
/// 10⁻⁹, each square root is `floor(√·)` and each product floors; only the
/// final sample size is a ceiling.
pub fn plan(metric: &str, baseline: &str, effect: &str, alpha: &str, power: &str, discordance: Option<&str>, cluster: Option<(u32, &str)>) -> Result<Value> {
    let (m, _) = registry::resolve(metric).map_err(reject)?;
    if m.unit != "ratio" { return Err(reject(json!({"code": "not_a_rate_metric", "metric": metric, "unit": m.unit}))); }
    let (p1, d) = (open_unit(baseline, "baseline_out_of_range")?, open_unit(effect, "effect_out_of_range")?);
    let p2 = p1 + d;
    if p2 >= SCALE { return Err(reject(json!({"code": "effect_out_of_range", "detail": "baseline + effect must stay below 1"}))); }
    let ((alpha_v, za), (power_v, zb)) = (quantile(&Z_ALPHA, alpha, "unsupported_alpha")?, quantile(&Z_POWER, power, "unsupported_power")?);
    let s = SCALE;
    let d2 = d * d; // S² units

    // Unpaired, two proportions, equal arms:
    // n = ⌈(z_α √(2 p̄ q̄) + z_β √(p₁q₁ + p₂q₂))² / d²⌉, p̄ = (p₁ + p₂)/2.
    let sum = p1 + p2;
    let pooled_root = est::isqrt(sum * (2 * s - sum) / 2); // √(2p̄q̄) in S units
    let split_root = est::isqrt(p1 * (s - p1) + p2 * (s - p2));
    let unpaired_term = za * pooled_root / s + zb * split_root / s;
    let per_arm = ceil_div(unpaired_term * unpaired_term, d2);

    // Paired (McNemar, Connor 1987): n = ⌈(z_α √ψ + z_β √(ψ − d²))² / d²⌉ pairs,
    // ψ = discordant share; independent arms give ψ = p₁(1 − p₂) + p₂(1 − p₁).
    let (psi, psi_source) = match discordance {
        None => ((p1 * (s - p2) + p2 * (s - p1)) / s, "assumed_independent_arms"),
        Some(text) => (open_unit(text, "discordance_out_of_range")?, "declared"),
    };
    if psi * s <= d2 { return Err(reject(json!({"code": "discordance_out_of_range", "detail": "ψ must exceed d²"}))); }
    let paired_term = za * est::isqrt(psi * s) / s + zb * est::isqrt(psi * s - d2) / s;
    let pairs = ceil_div(paired_term * paired_term, d2);

    let clustered = match cluster {
        None => Value::Null,
        Some((size, icc)) => {
            let rho = est::parse_fixed(icc).filter(|v| *v <= s).ok_or_else(|| reject(json!({"code": "icc_out_of_range", "value": icc})))?;
            if size == 0 { return Err(reject(json!({"code": "cluster_size_out_of_range"}))); }
            let effect = s + u128::from(size - 1) * rho;
            json!({"cluster_size": size, "icc": est::fixed(rho), "design_effect": est::fixed(effect), "formula": "n × (1 + (m − 1)ρ), ceiling",
                "unpaired_per_arm": ceil_div(per_arm * effect, s), "unpaired_total_tasks": 2 * ceil_div(per_arm * effect, s), "paired_pairs": ceil_div(pairs * effect, s)})
        }
    };
    Ok(json!({"schema_version": 1, "contract": PLAN_CONTRACT, "metric": m.id, "definition": m.versions[0].definition,
        "inputs": {"baseline_rate": est::fixed(p1), "min_detectable_effect": est::fixed(d), "alternative_rate": est::fixed(p2), "alpha": est::fixed(alpha_v), "two_sided": true,
            "power": est::fixed(power_v)},
        "z": {"alpha": est::fixed(za), "power": est::fixed(zb)},
        "arithmetic": "fixed point 1e-9: floor(sqrt) and floor(product) at each step, ceiling at the end",
        "unpaired": {"formula": "ceil((z_a*sqrt(2*pbar*(1-pbar)) + z_b*sqrt(p1*(1-p1) + p2*(1-p2)))^2 / d^2), pbar = (p1+p2)/2", "per_arm": per_arm, "total_tasks": 2 * per_arm},
        "paired": {"formula": "ceil((z_a*sqrt(psi) + z_b*sqrt(psi - d^2))^2 / d^2) closed candidate groups containing both arms",
            "discordance": {"value": est::fixed(psi), "source": psi_source}, "pairs": pairs, "arm_attempts": 2 * pairs},
        "clustered": clustered,
        "registry_minimum": {"unpaired": {"value": COMPARISON.min_tasks, "unit": "terminal_tasks_per_configuration_class_cell"}, "paired": {"value": 10, "unit": "closed_groups_containing_both", "source": "registry.v1"},
            "note": "the display minimum is a floor, not a power guarantee"},
        "note": "an approximate planning figure, not evidence: run it as a preregistered experiment (review experiments register) with its assignment and outcome rules frozen first"}))
}

pub fn plan_text(plan: &Value) -> String {
    format!("{} {} baseline={} effect={} alpha={} power={} unpaired_per_arm={} unpaired_total={} paired_pairs={} discordance={}({})\n",
        plan["contract"].as_str().unwrap_or(""), plan["metric"].as_str().unwrap_or(""), plan["inputs"]["baseline_rate"].as_str().unwrap_or(""),
        plan["inputs"]["min_detectable_effect"].as_str().unwrap_or(""), plan["inputs"]["alpha"].as_str().unwrap_or(""), plan["inputs"]["power"].as_str().unwrap_or(""),
        plan["unpaired"]["per_arm"], plan["unpaired"]["total_tasks"], plan["paired"]["pairs"], plan["paired"]["discordance"]["value"].as_str().unwrap_or(""),
        plan["paired"]["discordance"]["source"].as_str().unwrap_or(""))
}

/// `(Σ outcome, units)` per task over `units`, in task-ID order.
fn clusters<'a>(units: impl Iterator<Item = (&'a str, i64)>) -> Vec<(i64, i64)> {
    let mut by: BTreeMap<&str, (i64, i64)> = BTreeMap::new();
    for (task, y) in units { let c = by.entry(task).or_default(); c.0 += y; c.1 += 1; }
    by.into_values().collect()
}

/// Experiment reports over the D4 ledger at `as_of`.
pub fn report(project: &Path, as_of: Option<i64>) -> Result<Value> {
    let db = crate::telemetry::read_only(&project.join(".state/state.db"))?;
    crate::store::check_schema(&db)?;
    let db = db.unchecked_transaction()?;
    let now = jiff::Timestamp::now().as_millisecond();
    let Some(state) = crate::store::protocol_state(&db, as_of, now)? else {
        return Ok(json!({"schema_version": 1, "contract": REPORT_CONTRACT, "experiments": unavailable("review_protocols_absent")}));
    };
    let spec = COMPARISON.bootstrap;
    let source = json!(COMPARISON.version);
    let mut task_of = db.prepare("SELECT task_id FROM review_opportunities WHERE opportunity_id=?1")?;
    let mut experiments = Vec::new();
    for e in &state.experiments {
        let mut tasks = BTreeMap::new();
        for u in &e.units { tasks.insert(u.opportunity_id.as_str(), task_of.query_row([&u.opportunity_id], |r| r.get::<_, String>(0)).context("unit opportunity")?); }
        let task = |opportunity: &str| tasks.get(opportunity).map_or("", String::as_str);
        let analyzable = |arm: &str| -> Vec<(&str, i64)> {
            e.units.iter().filter(|u| u.arm == arm && u.status == "analyzable").map(|u| (task(&u.opportunity_id), u.outcome.unwrap_or(0) as i64)).collect()
        };
        let arms: Vec<String> = e.definition["arms"].as_array().into_iter().flatten().filter_map(|a| a["arm"].as_str().map(str::to_owned)).collect();
        let mut differences = serde_json::Map::new();
        for arm in arms.iter().skip(1) {
            let estimate = &e.estimate["differences"][arm];
            let available = estimate["value"].is_string();
            let interval = if !available { unavailable("insufficient_data") } else if e.design == "randomized" {
                let reference = clusters(analyzable(&e.reference_arm).into_iter());
                let treatment = clusters(analyzable(arm).into_iter());
                est::difference_interval(&reference, &treatment, &spec, &source)
            } else {
                // Matched: complete blocks, clustered by the reference unit's task; each block contributes (arm − reference, 1).
                let mut blocks: BTreeMap<&str, BTreeMap<&str, (i64, &str)>> = BTreeMap::new();
                for u in e.units.iter().filter(|u| u.status == "analyzable") {
                    if let Some(b) = &u.block { blocks.entry(b.as_str()).or_default().insert(u.arm.as_str(), (u.outcome.unwrap_or(0) as i64, task(&u.opportunity_id))); }
                }
                let complete = blocks.values().filter(|b| arms.iter().all(|a| b.contains_key(a.as_str())));
                let per: Vec<(&str, i64)> = complete.map(|b| (b[e.reference_arm.as_str()].1, b[arm.as_str()].0 - b[e.reference_arm.as_str()].0)).collect();
                let mut by: BTreeMap<&str, (i64, i64)> = BTreeMap::new();
                for (t, diff) in per { let c = by.entry(t).or_default(); c.0 += diff; c.1 += 1; }
                let (mut interval, _) = est::ratio_interval(&by.into_values().collect::<Vec<_>>(), &spec, &source);
                if interval.get("resample").is_some() { interval["resample"] = json!("block_by_task"); }
                interval
            };
            let causal = if available {
                json!({"status": "causal_estimate", "design": e.design, "analysis": "intention_to_treat",
                    "scope": "effect of the assigned arm on the preregistered primary outcome among preregistered eligible units; not a model ranking; never read by dispatch"})
            } else { json!({"status": "unavailable", "reason": "insufficient_data", "min_units": e.min_units}) };
            let mut entry = estimate.clone();
            entry["interval"] = interval;
            entry["causal"] = causal;
            differences.insert(arm.clone(), entry);
        }
        let causal = differences.values().any(|d| d["causal"]["status"] == "causal_estimate");
        let mut statuses: BTreeMap<&str, BTreeMap<&str, usize>> = BTreeMap::new();
        for u in &e.units { *statuses.entry(u.arm.as_str()).or_default().entry(u.status.as_str()).or_default() += 1; }
        experiments.push(json!({"experiment": e.experiment, "design": e.design, "assignment_seed": e.seed, "definition_digest": e.definition_digest,
            "preregistered": e.definition, "registered_unix_ms": e.registered_unix_ms, "analysis": "intention_to_treat", "reference_arm": e.reference_arm,
            "min_units": e.min_units, "outcome": e.estimate["outcome"], "units": {"assigned": e.units.len(), "by_arm_status": statuses},
            "arms": e.estimate["arms"], "complete_blocks": e.estimate["complete_blocks"], "differences": differences,
            "estimator": {"interval": if e.design == "randomized" { "percentile_bootstrap.v1 over tasks within each arm" } else { "percentile_bootstrap.v1 over complete blocks grouped by task" },
                "seed": spec.seed_hex(), "iterations": spec.iterations, "level": spec.level(), "source": COMPARISON.version},
            "label": if causal { "causal" } else { "descriptive" }, "routing": "never: experiment estimates are not read by dispatch"}));
    }
    Ok(json!({"schema_version": 1, "contract": REPORT_CONTRACT, "head_seq": state.head_seq, "as_of_seq": state.as_of_seq, "experiments": experiments}))
}

pub fn report_text(report: &Value) -> String {
    let mut out = format!("{} as_of_seq={}\n", report["contract"].as_str().unwrap_or(""), report["as_of_seq"]);
    for e in report["experiments"].as_array().into_iter().flatten() {
        out += &format!("{} design={} label={} reference={}\n", e["experiment"].as_str().unwrap_or(""), e["design"].as_str().unwrap_or(""),
            e["label"].as_str().unwrap_or(""), e["reference_arm"].as_str().unwrap_or(""));
        for (arm, d) in e["differences"].as_object().into_iter().flatten() {
            out += &format!("  {arm} difference={} interval=[{}, {}] causal={}\n", d["value"], d["interval"]["lower"].as_str().unwrap_or("-"),
                d["interval"]["upper"].as_str().unwrap_or("-"), d["causal"]["status"].as_str().unwrap_or(""));
        }
    }
    out
}

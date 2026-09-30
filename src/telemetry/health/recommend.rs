//! TM4.5 advisory recommendations (docs/telemetry/contracts-health.md §5):
//! `telemetry <slug> recommend --role <task class>`. A recommendation exists
//! only when the TM4.4 comparison (`analytics::compare`) ranks the role's cell
//! (`intervals_separated`); otherwise it is `no_recommendation` with the
//! comparison's own reasons. It carries the metric and definition versions,
//! the evidence window, the uncertainty and M50 evidence freshness, and is
//! `stale` once the recommended configuration's lineage dispatches another
//! configuration. Purely advisory: this module opens `state.db` only through
//! `telemetry::read_only`, writes nothing anywhere and has no path to launch
//! authority, worker profiles, model access, spending limits or acceptance
//! checks; nothing in dispatch or admission reads it.
use crate::telemetry::analytics::{compare, registry};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub const CONTRACT: &str = "telemetry-recommendation.v1";
const ADVISORY: &str = "advisory only: a person decides; nothing here is read by dispatch or admission, and it changes no authority, profile, model access, spending limit or acceptance check";

/// `telemetry <slug> recommend ...`
#[derive(clap::Args, Clone, Debug)]
pub struct Args {
    /// The role: a task class (taxonomy v1), the unit TM4.4 ranks configurations in.
    #[arg(long)]
    pub role: String,
    /// Comparable metric the recommendation rests on (`M02` or `M07`).
    #[arg(long, default_value = "M02")]
    pub metric: String,
    /// Evidence window start, inclusive, UTC Unix ms.
    #[arg(long)]
    pub from: Option<i64>,
    /// Evidence window end, exclusive, UTC Unix ms.
    #[arg(long)]
    pub to: Option<i64>,
    #[arg(long)]
    pub json: bool,
}

/// One dispatch decision as the freshness lineage reads it.
struct Decision { attempt: String, configuration: String, decided: i64, profile: Option<String>, kind: Option<String> }

/// The canonical dispatch log, read-only: decisions and configuration labels.
pub struct DispatchLog { decisions: Vec<Decision>, labels: BTreeMap<String, String> }

impl DispatchLog {
    pub fn load(project: &Path) -> Result<Self> {
        let db = crate::telemetry::read_only(&project.join(".state/state.db"))?;
        let table = |name: &str| db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)", [name], |r| r.get::<_, bool>(0));
        if !table("dispatch_decisions")? || !table("agent_configurations")? { return Ok(DispatchLog { decisions: Vec::new(), labels: BTreeMap::new() }); }
        let inputs = if table("attempt_inputs")? { "(SELECT json_extract(i.payload,'$.inputs.effective_profile.name') FROM attempt_inputs i WHERE i.attempt_id=d.attempt_id)" } else { "NULL" };
        let decisions = db.prepare(&format!("SELECT d.attempt_id,d.chosen_configuration_id,d.decided_unix_ms,{inputs},
            (SELECT json_extract(c.canonical_json,'$.kind') FROM agent_configurations c WHERE c.configuration_id=d.chosen_configuration_id)
            FROM dispatch_decisions d ORDER BY d.decided_unix_ms,d.attempt_id"))?
            .query_map([], |r| Ok(Decision { attempt: r.get(0)?, configuration: r.get(1)?, decided: r.get(2)?, profile: r.get(3)?, kind: r.get(4)? }))?
            .collect::<rusqlite::Result<_>>()?;
        let labels = db.prepare("SELECT configuration_id,json_extract(canonical_json,'$.kind'),json_extract(canonical_json,'$.agent_version') FROM agent_configurations")?
            .query_map([], |r| Ok((r.get::<_, String>(0)?, format!("{} {}", r.get::<_, Option<String>>(1)?.unwrap_or_else(|| "unknown".into()),
                r.get::<_, Option<String>>(2)?.unwrap_or_else(|| "unknown".into())))))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(DispatchLog { decisions, labels })
    }

    fn label(&self, configuration: &str) -> Value { self.labels.get(configuration).map_or(Value::Null, |l| json!(l)) }

    /// M50 (`registry::FRESHNESS`) for `configuration` resting on `supporting` tasks.
    fn freshness(&self, configuration: &str, supporting: i64) -> Value {
        let own: Vec<&Decision> = self.decisions.iter().filter(|d| d.configuration == configuration).collect();
        let basis = if !own.is_empty() && own.iter().all(|d| d.profile.is_some()) { "profile" } else { "kind" };
        let key = |d: &Decision| if basis == "profile" { d.profile.clone() } else { d.kind.clone() };
        let keys: BTreeSet<String> = own.iter().filter_map(|d| key(d)).collect();
        let (stale_n, stale_d) = registry::FRESHNESS.stale_below;
        let base = json!({"metric_id": "M50", "definition": registry::FRESHNESS.definition, "stale_below": format!("{stale_n}/{stale_d}"),
            "lineage": {"basis": basis, "keys": keys}, "recommended_configuration_id": configuration, "supporting_observations": supporting,
            "unit": "tasks"});
        let latest = self.decisions.iter().filter(|d| key(d).is_some_and(|k| keys.contains(&k))).max_by(|a, b| (a.decided, &a.attempt).cmp(&(b.decided, &b.attempt)));
        let mut out = base;
        let Some(latest) = latest.filter(|_| supporting > 0) else {
            out["value"] = json!({"status": "unavailable", "reason": if supporting == 0 { "no_supporting_observations" } else { "lineage_unknown" }});
            out["state"] = json!("unknown");
            return out;
        };
        let under = if latest.configuration == configuration { supporting } else { 0 };
        let stale = i128::from(under) * i128::from(stale_d) < i128::from(stale_n) * i128::from(supporting);
        out["value"] = json!(format!("{under}/{supporting}"));
        out["decimal"] = json!(crate::telemetry::analytics::estimators::decimal(i128::from(under), i128::from(supporting), 4));
        out["under_current_configuration"] = json!(under);
        out["current_configuration_id"] = json!(latest.configuration);
        out["current_label"] = self.label(&latest.configuration);
        out["current_decided_unix_ms"] = json!(latest.decided);
        out["state"] = json!(if stale { "stale" } else { "fresh" });
        out
    }
}

fn valid_role(role: &str) -> bool {
    !role.is_empty() && role.len() <= 64 && role.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b))
}

/// `telemetry <slug> recommend`: the recommendation JSON. Reads only.
pub fn run(project: &Path, args: &Args) -> Result<Value> {
    if !valid_role(&args.role) { bail!("recommend rejected: {}", json!({"code": "invalid_role", "role": args.role, "detail": "a task class: letters, digits, `_`, `-`, `.`; at most 64"})); }
    let compare_args = compare::Args { metrics: vec![args.metric.clone()], by: "configuration".into(), cohort: None, from: args.from, to: args.to, horizon_ms: None,
        task_class: Some(args.role.clone()), seed: None, json: true };
    let report = compare::run(project, &compare_args).map_err(|e| anyhow::anyhow!("{}", e.to_string().replacen("compare rejected", "recommend rejected", 1)))?;
    if report["results"].as_array().is_none_or(|r| r.len() != 1) { bail!("recommend rejected: {}", json!({"code": "one_metric", "metrics": args.metric})); }
    Ok(recommendation(&report, &args.role, &DispatchLog::load(project)?))
}

/// The recommendation for `role` from one TM4.4 comparison report.
pub fn recommendation(report: &Value, role: &str, log: &DispatchLog) -> Value {
    let result = &report["results"][0];
    let request = &report["request"];
    let arm_label = |id: &str| report["configurations"].as_array().into_iter().flatten().find(|c| c["configuration_id"] == id).map(|c| c["label"].clone())
        .unwrap_or_else(|| log.label(id));
    let mut out = json!({"schema_version": 1, "contract": CONTRACT, "role": role, "role_basis": "task_class",
        "advisory": {"advisory": true, "authority": "none", "routing": ADVISORY, "writes": "none"},
        "metric": {"metric_id": result["metric_id"], "definition": result["definition"], "higher_is_better": result["higher_is_better"],
            "registry": report["registry"], "comparison": report["contract"], "freshness": registry::FRESHNESS.definition},
        "evidence_window": {"cohort": request["cohort"], "from_unix_ms": request["from"], "to_unix_ms": request["to"], "semantics": "half_open",
            "time_basis": "task_terminal_time"},
        "analysis": report["analysis"], "population": {"members": report["population"]["members"], "allocated": report["population"]["allocated"],
            "unallocated": report["population"]["unallocated"], "coverage": report["population"]["coverage"]},
        "caveats": report["notes"].as_array().into_iter().flatten().map(|n| n["code"].clone()).collect::<Vec<_>>(),
        "recommendation": null, "uncertainty": null, "freshness": null});
    let cell = result["cells"].as_array().into_iter().flatten().find(|c| c["task_class"] == role);
    let Some(cell) = cell else {
        out["status"] = json!("no_recommendation");
        out["reasons"] = json!([{"code": "no_evidence_for_role", "detail": "no allocated terminal task of this class in the window"}]);
        return out;
    };
    let arms: Vec<Value> = cell["arms"].as_array().into_iter().flatten().map(|a| json!({"configuration_id": a["configuration_id"],
        "label": arm_label(a["configuration_id"].as_str().unwrap_or_default()), "tasks": a["tasks"], "status": a["status"], "value": a["value"],
        "interval": a["interval"]})).collect();
    out["arms"] = json!(arms);
    let ranking = &cell["ranking"];
    if ranking["status"] != "intervals_separated" {
        out["status"] = json!("no_recommendation");
        out["reasons"] = json!([{"code": "ranking_not_supported", "compare_reasons": ranking["reasons"]}]);
        return out;
    }
    let order: Vec<&str> = ranking["order"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let find = |id: &str| cell["arms"].as_array().into_iter().flatten().find(|a| a["configuration_id"] == id).cloned().unwrap_or(Value::Null);
    let best_id = order[0];
    let (best, runner) = (find(best_id), order.get(1).map(|id| find(id)).unwrap_or(Value::Null));
    let freshness = log.freshness(best_id, best["tasks"].as_i64().unwrap_or(0));
    out["recommendation"] = json!({"configuration_id": best_id, "label": arm_label(best_id), "value": best["value"], "decimal": best["decimal"],
        "tasks": best["tasks"], "pooled": best["pooled"]});
    out["uncertainty"] = json!({"estimator": report["estimators"]["bootstrap"], "min_sample": report["estimators"]["min_sample"],
        "recommended": {"interval": best["interval"]}, "runner_up": {"configuration_id": runner["configuration_id"],
            "label": runner["configuration_id"].as_str().map_or(Value::Null, arm_label), "value": runner["value"], "interval": runner["interval"]},
        "ranking": {"order": order, "scope": ranking["scope"], "observational": ranking["observational"], "causal": ranking["causal"]}});
    let mut reasons = vec![json!({"code": "intervals_separated", "order": order.iter().map(|id| arm_label(id)).collect::<Vec<_>>()})];
    let status = match freshness["state"].as_str() {
        Some("fresh") => "recommended",
        Some("stale") => {
            reasons.push(json!({"code": "configuration_changed", "from": arm_label(best_id), "to": freshness["current_label"],
                "detail": "the recommended configuration's lineage now dispatches another configuration; its evidence belongs to the old arm (consider a replay run)"}));
            "stale"
        }
        _ => { reasons.push(json!({"code": "freshness_unknown", "reason": freshness["value"]["reason"]})); "stale" }
    };
    out["freshness"] = freshness;
    out["status"] = json!(status);
    out["reasons"] = json!(reasons);
    out
}

/// Text form: the status line, the recommendation and its evidence.
pub fn text(rec: &Value) -> String {
    let s = |v: &Value| v.as_str().map_or_else(|| v.to_string(), str::to_owned);
    let mut out = format!("role {} · {} · {} {} · advisory\n", s(&rec["role"]), s(&rec["status"]), s(&rec["metric"]["metric_id"]), s(&rec["metric"]["definition"]));
    if !rec["recommendation"].is_null() {
        let r = &rec["recommendation"];
        out += &format!("  recommended {} value={} interval=[{}, {}] n={}\n", s(&r["label"]), s(&r["value"]), s(&rec["uncertainty"]["recommended"]["interval"]["lower"]),
            s(&rec["uncertainty"]["recommended"]["interval"]["upper"]), s(&r["tasks"]));
        out += &format!("  freshness M50 {} ({}; stale below {})\n", s(&rec["freshness"]["value"]), s(&rec["freshness"]["state"]), s(&rec["freshness"]["stale_below"]));
    }
    for reason in rec["reasons"].as_array().into_iter().flatten() { out += &format!("  reason {}\n", s(&reason["code"])); }
    out
}

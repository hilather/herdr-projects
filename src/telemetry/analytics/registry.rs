//! The TM4.1 metric registry `analytics-registry.v1` (docs/telemetry/contracts-analytics.md §1):
//! one declared, read-only table of every metric the report or the query
//! service can name, with its definition versions, family, cohorts, window
//! semantics, unit, certification and activation. A change here is a new
//! registry version, never an edit in place of a published definition.
use serde_json::{Value, json};

pub const VERSION: &str = "analytics-registry.v1";

/// The TM3.5 quality certificate. Without it every validated-quality family is
/// `unavailable: awaiting_quality_certificate`; landing or withdrawing it is
/// this one line (a registry change).
pub const QUALITY_CERTIFICATE: Option<&str> = Some("docs/telemetry/certificate-quality.md");

/// certificate-quality.md §4: fixture-certified quality values are not yet production quality claims.
const PRODUCTION_GATE: &str = "awaiting_producer_certificate (TM5.2 quality-producer section; certificate-quality.md §4)";

/// Plan doc 07 §1 cohort modes. `completed_task` is not one of them (doc 08 §4).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cohort { Activity, Terminal, Assignment }

impl Cohort {
    pub fn as_str(self) -> &'static str {
        match self { Cohort::Activity => "activity_window", Cohort::Terminal => "terminal_cohort", Cohort::Assignment => "assignment_cohort" }
    }
    pub fn parse(text: &str) -> Result<Self, &'static str> {
        match text {
            "activity_window" => Ok(Cohort::Activity),
            "terminal_cohort" => Ok(Cohort::Terminal),
            "assignment_cohort" => Ok(Cohort::Assignment),
            "completed_task" => Err("ambiguous_cohort"),
            _ => Err("unknown_cohort"),
        }
    }
}

/// Who evaluates a definition. `Native`: the query service itself, with
/// half-open windows, dimensions and drill-down lineage. `Central` / `Lane`:
/// the body `telemetry report` prints, windowed only from `since` (= `from`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Provider { Native, Central, Lane(&'static str), Absent(&'static str) }

impl Provider {
    pub fn as_json(self) -> Value {
        match self {
            Provider::Native => json!({"kind": "native"}),
            Provider::Central => json!({"kind": "central_report"}),
            Provider::Lane(stream) => json!({"kind": "lane", "stream": stream}),
            Provider::Absent(reason) => json!({"kind": "absent", "reason": reason}),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Family { Lifecycle, Consumption, Cost, Tools, Attention, Fleet, Services, ReviewQuality, PairedQuality, SeededQuality, Proxy, Replay, Freshness }

impl Family {
    pub fn as_str(self) -> &'static str {
        match self {
            Family::Lifecycle => "lifecycle", Family::Consumption => "consumption", Family::Cost => "cost", Family::Tools => "tools",
            Family::Attention => "attention", Family::Fleet => "fleet", Family::Services => "services", Family::ReviewQuality => "review_quality",
            Family::PairedQuality => "paired_quality", Family::SeededQuality => "seeded_quality", Family::Proxy => "proxy",
            Family::Replay => "replay", Family::Freshness => "freshness",
        }
    }

    /// Plan doc 12 TM4.1: families activate independently. `Ok(gate)` names the
    /// certificate or card that activated the family; `Err(reason)` is the
    /// `unavailable` reason every metric of the family returns.
    pub fn activation(self) -> Result<Value, &'static str> {
        match self {
            Family::Lifecycle | Family::Consumption | Family::Cost | Family::Tools | Family::Attention | Family::Fleet | Family::Services =>
                Ok(json!({"card": "TM2.6", "certificate": "docs/telemetry/certificate-core.md"})),
            // Proxies activate at TM3.7; their values are also covered by the TM3.5 fixture certificate.
            Family::Proxy => Ok(json!({"card": "TM3.7", "certificate": QUALITY_CERTIFICATE, "production": PRODUCTION_GATE})),
            // A fixture certificate: values are served with their basis/trust labels, while production
            // quality activation waits for the producer certificate (certificate-quality.md §4).
            Family::ReviewQuality | Family::PairedQuality | Family::SeededQuality => match QUALITY_CERTIFICATE {
                Some(certificate) => Ok(json!({"card": "TM3.5", "certificate": certificate, "production": PRODUCTION_GATE})),
                None => Err("awaiting_quality_certificate"),
            },
            Family::Replay => Ok(json!({"card": "TM4.6", "suite": "replay-suite.v1", "evidence": REPLAY})),
            Family::Freshness => Err("awaiting_configuration_evidence"),
        }
    }

    /// Proxy metrics are a separate family that can never stand in for a validated-quality metric (doc 12 TM0.3).
    pub fn proxy(self) -> bool { self == Family::Proxy }
}

/// How a definition treats `--from/--to`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Window { HalfOpen, SinceOnly }

pub struct Version {
    pub definition: &'static str,
    pub provider: Provider,
    /// First is the default cohort.
    pub cohorts: &'static [Cohort],
    pub window: Window,
    pub time_basis: &'static str,
    pub dimensions: &'static [&'static str],
}

pub struct Metric {
    pub id: &'static str,
    pub name: &'static str,
    pub family: Family,
    pub unit: &'static str,
    /// First is the current definition `query` serves by default.
    pub versions: &'static [Version],
    /// From certificate-core.md §5 or certificate-quality.md §2: `certified-live`,
    /// `certified-fixture`, `restricted`, `unavailable`, `fixture` (TM4.1's own fixtures) or `absent`.
    pub certification: &'static str,
    pub evidence: &'static str,
    pub restriction: Option<&'static str>,
}

const T: &[Cohort] = &[Cohort::Terminal];
const TA: &[Cohort] = &[Cohort::Terminal, Cohort::Assignment];
const A: &[Cohort] = &[Cohort::Activity];
const AS: &[Cohort] = &[Cohort::Assignment];
/// Bounded categorical dimensions of the native lifecycle definitions (doc 07 §1).
pub const LIFECYCLE_DIMENSIONS: &[&str] = &["agent_kind", "route", "task_class"];
/// Identities that belong in drill-down, never in a metric label.
pub const HIGH_CARDINALITY: &[&str] = &["task_id", "attempt_id", "session_id", "invocation_id", "finding_id", "submission_id", "entry_id"];

const fn native(definition: &'static str, cohorts: &'static [Cohort], time_basis: &'static str) -> Version {
    Version { definition, provider: Provider::Native, cohorts, window: Window::HalfOpen, time_basis, dimensions: LIFECYCLE_DIMENSIONS }
}
const fn central(definition: &'static str, cohorts: &'static [Cohort], time_basis: &'static str) -> Version {
    Version { definition, provider: Provider::Central, cohorts, window: Window::SinceOnly, time_basis, dimensions: &[] }
}
const fn lane(stream: &'static str, definition: &'static str, cohorts: &'static [Cohort], time_basis: &'static str) -> Version {
    Version { definition, provider: Provider::Lane(stream), cohorts, window: Window::SinceOnly, time_basis, dimensions: &[] }
}
const fn absent(definition: &'static str, cohorts: &'static [Cohort], reason: &'static str) -> Version {
    Version { definition, provider: Provider::Absent(reason), cohorts, window: Window::HalfOpen, time_basis: "none", dimensions: &[] }
}

const CORE: &str = "docs/telemetry/certificate-core.md §5";
const QUALITY: &str = "docs/telemetry/certificate-quality.md §2";
const REPLAY: &str = "tests/replay_suite.rs (TM4.6 fixture suite v1)";
const QUERY: &str = "tests/telemetry_query.rs (TM4.1 fixtures; the same A/T evidence as the certified slice-v1)";

macro_rules! m {
    ($id:literal, $name:literal, $family:ident, $unit:literal, [$($v:expr),+], $cert:literal, $evidence:expr, $restriction:expr) => {
        Metric { id: $id, name: $name, family: Family::$family, unit: $unit, versions: &[$($v),+], certification: $cert, evidence: $evidence, restriction: $restriction }
    };
}

pub const METRICS: &[Metric] = &[
    m!("M01", "accepted_tasks", Lifecycle, "tasks", [native("M01.cohort-v1", TA, "task_terminal_time")], "fixture", QUERY, None),
    m!("M02", "task_acceptance_rate", Lifecycle, "ratio", [native("M02.cohort-v1", TA, "task_terminal_time"),
        central("M02.slice-v1", T, "attempt_decided_at_or_after_since")], "certified-fixture", CORE, None),
    m!("M03", "accepted_throughput", Lifecycle, "tasks_per_hour", [absent("M03.v1", A, "operating_hours_not_recorded")], "absent", "no producer", None),
    m!("M04", "cost_per_accepted_task", Cost, "currency_per_task", [lane("accounting", "M04.cost-v1", T, "attempt_decided_at_or_after_since")], "certified-fixture", CORE, Some("R1 fixture-only rate cards; R5 shadow budget")),
    m!("M05", "tokens_per_accepted_task", Lifecycle, "tokens_per_task", [absent("M05.v1", T, "no_producer")], "absent", "no producer", None),
    m!("M06", "task_lead_time_p95", Lifecycle, "milliseconds", [native("M06.cohort-v1", T, "task_terminal_time")], "fixture", QUERY, None),
    m!("M07", "attempt_amplification", Lifecycle, "attempts_per_accepted_task", [native("M07.cohort-v1", TA, "task_terminal_time"),
        central("M07.slice-v1", T, "attempt_decided_at_or_after_since")], "certified-fixture", CORE, None),
    m!("M08", "input_tokens", Consumption, "tokens", [lane("accounting", "M08.slice-v1", A, "session_start")], "certified-live", CORE, None),
    m!("M09", "output_tokens", Consumption, "tokens", [lane("accounting", "M09.slice-v1", A, "session_start")], "certified-live", CORE, None),
    m!("M10", "cache_read_share", Consumption, "ratio", [absent("M10.v1", A, "no_producer")], "absent", "no producer", None),
    m!("M11", "reported_spend_subtotal", Cost, "currency", [lane("accounting", "M11.charges-v1", A, "charge_period")], "certified-fixture", CORE, Some("R2 no provider billing source")),
    m!("M12", "repriced_estimated_spend", Cost, "currency", [lane("accounting", "M12.cost-v1", A, "usage_time")], "certified-fixture", CORE, Some("R1 fixture-only rate cards")),
    m!("M13", "usage_coverage", Consumption, "ratio", [central("M13.slice-v1", A, "attempt_decided")], "certified-live", CORE, None),
    m!("M14", "cost_coverage", Cost, "ratio", [lane("accounting", "M14.cost-v1", A, "usage_time")], "certified-fixture", CORE, Some("R1 fixture-only rate cards")),
    m!("M15", "effective_model_coverage", Consumption, "ratio", [central("M15.slice-v1", A, "session_start")], "certified-live", CORE, None),
    m!("M16", "tool_call_volume", Tools, "calls", [lane("accounting", "M16.tools-v1", A, "call_time")], "certified-live", CORE, None),
    m!("M17", "tool_execution_success", Tools, "ratio", [lane("accounting", "M17.tools-v1", A, "call_time")], "certified-live", CORE, None),
    m!("M18", "tool_latency_p95", Tools, "milliseconds", [lane("accounting", "M18.tools-v1", A, "call_time")], "restricted", CORE, Some("execution_duration_not_exposed")),
    m!("M19", "blocked_time_share", Lifecycle, "ratio", [absent("M19.v1", AS, "no_producer")], "absent", "no producer", None),
    m!("M20", "review_completion", ReviewQuality, "ratio", [lane("review", "M20.v1", AS, "opportunity_assigned")], "certified-fixture", QUALITY, Some("basis declared: worker-declared completions without acceptance")),
    m!("M21", "validated_unique_findings", ReviewQuality, "findings", [lane("review", "M21.v1", A, "discovery")], "certified-fixture", QUALITY, None),
    m!("M22", "proposal_validation_rate", ReviewQuality, "ratio", [lane("review", "M22.v1", A, "submission")], "certified-fixture", QUALITY, None),
    m!("M23", "duplicate_report_share", ReviewQuality, "ratio", [lane("review", "M23.v1", A, "submission")], "certified-fixture", QUALITY, None),
    m!("M24", "review_discovery_efficiency", ReviewQuality, "findings_per_currency", [lane("review", "M24.v1", A, "opportunity_closed")], "restricted", QUALITY, Some("cost: TM2.6 accounting certificate and non-fixture rate cards")),
    m!("M25", "verified_fix_rate", ReviewQuality, "ratio", [lane("review", "M25.v1", AS, "repair_assignment")], "certified-fixture", QUALITY, None),
    m!("M26", "currently_resolved_rate", ReviewQuality, "ratio", [lane("review", "M26.v1", AS, "repair_assignment")], "certified-fixture", QUALITY, None),
    m!("M27", "reopen_rate", ReviewQuality, "ratio", [lane("review", "M27.v1", A, "integration")], "certified-fixture", QUALITY, None),
    m!("M28", "skeptical_incremental_yield", ReviewQuality, "findings_per_opportunity", [lane("review", "M28.v1", A, "opportunity")], "certified-fixture", QUALITY, Some("descriptive only: no preregistered randomized experiment")),
    m!("M29", "quality_attribution_coverage", ReviewQuality, "ratio", [lane("review", "M29.v1", A, "credit")], "certified-fixture", QUALITY, None),
    m!("M30", "first_candidate_verification_rate", ReviewQuality, "ratio", [absent("M30.v1", A, "no_producer")], "absent", "no producer", None),
    m!("M31", "human_interventions_per_accepted_task", Attention, "interventions_per_task", [lane("accounting", "M31.attention-v1", T, "attention_interval")], "certified-live", CORE, None),
    m!("M32", "waiting_on_you_share", Attention, "ratio", [lane("accounting", "M32.attention-v1", AS, "attention_interval")], "certified-live", CORE, None),
    m!("M33", "permission_prompts_per_attempt", Attention, "prompts_per_attempt", [lane("accounting", "M33.attention-v1", A, "attention_interval")], "restricted", CORE, Some("attention_reason_not_exposed")),
    m!("M34", "coordinator_overhead", Fleet, "ratio", [lane("accounting", "M34.fleet-v1", A, "usage_time")], "certified-fixture", CORE, Some("R6 coordinator observed only as Codex")),
    m!("M35", "fan_out_efficiency", Fleet, "ratio", [lane("accounting", "M35.fanout-v1", A, "active_attempts")], "certified-fixture", CORE, None),
    m!("M36", "integration_conflict_rate", Fleet, "ratio", [lane("accounting", "M36.integration-v1", A, "integration")], "certified-fixture", CORE, None),
    m!("M37", "overlap_waste_share", Fleet, "ratio", [lane("accounting", "M37.fleet-v1", A, "usage_time")], "certified-fixture", CORE, None),
    m!("M38", "throttled_time_share", Services, "ratio", [lane("accounting", "M38.slice-v1", A, "availability_interval")], "restricted", CORE, Some("throttling_not_certified")),
    m!("M39", "provider_error_rate", Services, "ratio", [lane("accounting", "M39.slice-v1", A, "invocation")], "restricted", CORE, Some("provider_errors_not_certified")),
    m!("M40", "quota_headroom_at_dispatch", Services, "native_units", [central("M40.quota-windows-v1", A, "attempt_decided")], "certified-live", CORE, Some("R7 a quota account is an execution home")),
    m!("M41", "candidate_win_rate", PairedQuality, "ratio", [lane("quality", "M41.v1", A, "group_closed")], "certified-fixture", QUALITY, Some("minimum 10 closed groups (registry.v1)")),
    m!("M42", "paired_acceptance_difference", PairedQuality, "percentage_points", [lane("quality", "M42.v1", A, "group_closed")], "certified-fixture", QUALITY, Some("task_family unavailable; minimum 10 closed groups")),
    m!("M43", "seeded_recall", SeededQuality, "ratio", [lane("review", "M43.v1", A, "opportunity")], "certified-fixture", QUALITY, Some("minimum 20 trials")),
    m!("M44", "clean_control_false_alarm_rate", SeededQuality, "ratio", [lane("review", "M44.v1", A, "opportunity")], "certified-fixture", QUALITY, Some("minimum 20 trials")),
    m!("M45", "first_candidate_ci_pass_proxy", Proxy, "ratio", [lane("quality", "M45.proxy-v1", A, "first_ci_run")], "certified-fixture", QUALITY, Some("proxy only; never replaces M30")),
    m!("M46", "main_breakage_after_integration_proxy", Proxy, "ratio", [lane("quality", "M46.proxy-v1", A, "integration")], "unavailable", QUALITY, Some("no_main_check_producer")),
    m!("M47", "code_survival_proxy", Proxy, "ratio", [lane("quality", "M47.proxy-v1", A, "integration")], "restricted", QUALITY, Some("censoring only")),
    m!("M48", "revert_rate_proxy", Proxy, "ratio", [lane("quality", "M48.proxy-v1", A, "integration")], "restricted", QUALITY, Some("censoring only")),
    m!("M49", "replay_suite_pass_rate", Replay, "ratio", [central("M49.v1", A, "replay_attempt_decided")], "fixture", REPLAY, Some("fixture suite v1 only; raw rates with n")),
    m!("M50", "evidence_freshness", Freshness, "ratio", [absent("M50.v1", A, "no_producer")], "absent", "no producer (TM4.4)", None),
    m!("flaky_tests", "newly_flaky_tests_proxy", Proxy, "tests", [lane("quality", "flaky_tests.proxy-v1", A, "ci_run")], "unavailable", QUALITY, Some("no_repeat_runs")),
];

/// TM4.4 comparison estimators (`analytics-comparison.v1`,
/// docs/telemetry/contracts-evaluation.md §2): the registry's declared
/// statistical methods for configuration comparisons. Not metrics: a change
/// here is a new comparison version.
pub struct Comparison {
    pub version: &'static str,
    /// Comparable native definitions and whether a higher value is better.
    pub metrics: &'static [(&'static str, &'static str, bool)],
    pub bootstrap: super::estimators::Bootstrap,
    /// Minimum terminal tasks per configuration × task-class cell (plan doc 07 §6).
    pub min_tasks: u32,
    /// `beta_binomial_eb.v1`: prior mean = the arm's own all-class rate, prior strength in pseudo-tasks.
    pub pooling: &'static str,
    pub prior_strength: i64,
    pub propensity: &'static str,
    /// Paired candidate-group analysis: lane C's M42 (its own minimum, `registry.v1`).
    pub paired: &'static str,
}

pub const COMPARISON: Comparison = Comparison {
    version: "analytics-comparison.v1",
    metrics: &[("M02", "M02.cohort-v1", true), ("M07", "M07.cohort-v1", false)],
    bootstrap: super::estimators::Bootstrap { method: "percentile_bootstrap.v1", iterations: 1000, seed: 0x544d_345f_636d_7072, level_permille: 950 },
    min_tasks: 20,
    pooling: "beta_binomial_eb.v1",
    prior_strength: 10,
    propensity: "hajek_ipw.v1",
    paired: "M42.v1",
};

pub fn comparison_json() -> Value {
    let c = &COMPARISON;
    json!({"version": c.version, "metrics": c.metrics.iter().map(|(id, definition, higher)| json!({"id": id, "definition": definition, "higher_is_better": higher})).collect::<Vec<_>>(),
        "bootstrap": {"method": c.bootstrap.method, "resample": "task", "iterations": c.bootstrap.iterations, "seed": c.bootstrap.seed_hex(), "level": c.bootstrap.level()},
        "min_sample": {"value": c.min_tasks, "unit": "terminal_tasks_per_configuration_class_cell"},
        "pooling": {"model": c.pooling, "prior_mean": "arm_all_class_rate", "prior_strength": c.prior_strength, "metrics": ["M02"]},
        "propensity": {"method": c.propensity, "requires": "positive logged probability_ppm for every compared arm on every decision in the cell"},
        "paired": {"definition": c.paired, "min_sample": "registry.v1 (lane C)"}})
}

pub fn find(id: &str) -> Option<&'static Metric> { METRICS.iter().find(|m| m.id == id) }

/// `M02` (current definition) or an explicit definition `M02.slice-v1`.
pub fn resolve(name: &str) -> Result<(&'static Metric, &'static Version), Value> {
    if let Some(metric) = find(name) { return Ok((metric, &metric.versions[0])); }
    let id = name.split('.').next().unwrap_or_default();
    let Some(metric) = find(id) else { return Err(json!({"code": "unknown_metric", "metric": name})) };
    metric.versions.iter().find(|v| v.definition == name).map(|v| (metric, v))
        .ok_or_else(|| json!({"code": "unknown_definition", "metric": id, "definition": name, "known": metric.versions.iter().map(|v| v.definition).collect::<Vec<_>>()}))
}

/// `telemetry <slug> metrics registry --json`.
pub fn json() -> Value {
    let metrics: Vec<Value> = METRICS.iter().map(|m| {
        let (active, activation) = match m.family.activation() { Ok(gate) => (true, gate), Err(reason) => (false, json!({"status": "unavailable", "reason": reason})) };
        let versions: Vec<Value> = m.versions.iter().enumerate().map(|(i, v)| json!({"definition": v.definition, "current": i == 0,
            "provider": v.provider.as_json(), "cohorts": v.cohorts.iter().map(|c| c.as_str()).collect::<Vec<_>>(), "default_cohort": v.cohorts[0].as_str(),
            "window": match v.window { Window::HalfOpen => "half_open", Window::SinceOnly => "since_only" }, "time_basis": v.time_basis,
            "dimensions": v.dimensions})).collect();
        json!({"id": m.id, "name": m.name, "family": m.family.as_str(), "proxy": m.family.proxy(), "unit": m.unit, "definition": m.versions[0].definition,
            "versions": versions, "certification": {"status": m.certification, "evidence": m.evidence, "restriction": m.restriction},
            "active": active, "activation": activation})
    }).collect();
    json!({"registry": VERSION, "quality_certificate": QUALITY_CERTIFICATE, "cohorts": ["activity_window", "terminal_cohort", "assignment_cohort"],
        "rejected_cohorts": {"completed_task": "ambiguous_cohort"}, "high_cardinality_identities": HIGH_CARDINALITY, "metrics": metrics,
        "comparison": comparison_json()})
}

pub fn text() -> String {
    let mut out = format!("{VERSION} quality_certificate={}\n", QUALITY_CERTIFICATE.unwrap_or("none"));
    for m in METRICS {
        let state = match m.family.activation() { Ok(_) => "active".to_owned(), Err(reason) => format!("unavailable({reason})") };
        let v = &m.versions[0];
        out += &format!("{} {} {} {} family={} cohorts={} unit={} certification={} {state}\n", m.id, m.name, v.definition,
            match v.provider { Provider::Native => "native", Provider::Central => "central", Provider::Lane(s) => s, Provider::Absent(_) => "absent" },
            m.family.as_str(), v.cohorts.iter().map(|c| c.as_str()).collect::<Vec<_>>().join(","), m.unit, m.certification);
    }
    out
}

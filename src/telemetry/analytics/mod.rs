//! TM4.1 analytics (plan doc 12): the metric registry, the query service and
//! incremental aggregate revisions in sidecar stream `analytics`. Hooks
//! registered centrally in `super::LANES`; migrations under
//! `migrations/telemetry/analytics/` only. Contract:
//! docs/telemetry/contracts-analytics.md. Never grants launch, changes budgets
//! or accepts results; never writes `state.db`.
use anyhow::Result;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

pub mod compare;
pub mod estimators;
pub mod experiments;
pub(crate) mod inputs;
pub mod lifecycle;
pub mod query;
pub mod registry;
pub mod store;

pub const STREAM: &str = "analytics";
/// `include_str!` of `migrations/telemetry/analytics/`, in order; index + 1 is the stream version.
pub const MIGRATIONS: &[&str] = &[include_str!("../../../migrations/telemetry/analytics/0001_aggregate_revisions.sql"),
    include_str!("../../../migrations/telemetry/analytics/0002_workspace_projections.sql"),
    include_str!("../../../migrations/telemetry/analytics/0003_input_frontiers.sql")];

pub(crate) fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{:x}", <sha2::Sha256 as sha2::Digest>::digest(bytes))
}

/// `herdr-projects telemetry <slug> metrics ...`
#[derive(clap::Subcommand)]
pub enum MetricsCommand {
    /// The versioned metric registry: definitions, families, cohorts, units, certification and activation. Read-only.
    Registry {
        #[arg(long)]
        json: bool,
    },
}

/// `herdr-projects telemetry <slug> analytics ...`
#[derive(clap::Subcommand)]
pub enum Command {
    /// Stream version of this lane's sidecar tables. Read-only.
    Status,
    /// Evaluate tracked cells whose inputs changed (first run: every active metric's
    /// default cell), retain clock-dependent evaluation, and append changed revisions. Writes only the sidecar.
    Refresh {
        /// Also track this cell: a metric or definition, with the query flags below.
        #[arg(long)]
        metric: Option<String>,
        #[arg(long)]
        cohort: Option<String>,
        #[arg(long)]
        from: Option<i64>,
        #[arg(long)]
        to: Option<i64>,
        #[arg(long)]
        horizon_ms: Option<i64>,
        #[arg(long)]
        by: Option<String>,
    },
    /// Recompute every tracked cell from the sources and compare it byte for byte
    /// with its latest revision; without --verify a differing cell is restated.
    Rebuild {
        #[arg(long)]
        verify: bool,
    },
    /// Latest content of every tracked cell, without revision numbers or times. Read-only.
    Snapshot,
    /// Revision history with source watermarks. Read-only.
    Revisions {
        #[arg(long)]
        metric: Option<String>,
    },
    /// EXPLAIN QUERY PLAN of the hot queries on this project's stores. Read-only.
    Plans,
}

pub fn run_metrics(command: MetricsCommand) -> Result<String> {
    match command {
        MetricsCommand::Registry { json: true } => Ok(serde_json::to_string_pretty(&registry::json())? + "\n"),
        MetricsCommand::Registry { json: false } => Ok(registry::text()),
    }
}

/// `config_dir` holds the drill-down cursor key (`export::cursor`).
pub fn run_query(project: &Path, config_dir: &Path, args: &query::Args) -> Result<String> {
    let request = query::request(args)?;
    let out = query::run_with(project, &request, &crate::telemetry::export::cursor::Keyring::new(config_dir))?;
    Ok(if args.json { serde_json::to_string_pretty(&out)? + "\n" } else { query::text(&out) })
}

/// TM4.4 `telemetry <slug> compare` (contracts-evaluation.md). Read-only.
pub fn run_compare(project: &Path, args: &compare::Args) -> Result<String> {
    let report = compare::run(project, args)?;
    Ok(if args.json { serde_json::to_string_pretty(&report)? + "\n" } else { compare::text(&report) })
}

/// TM4.4 `telemetry <slug> experiments plan|report` (contracts-evaluation.md). Read-only.
pub fn run_experiments(project: &Path, command: experiments::Command) -> Result<String> {
    let (value, json, text): (Value, bool, fn(&Value) -> String) = match command {
        experiments::Command::Plan { metric, baseline_rate, min_detectable_effect, alpha, power, discordance, cluster_size, icc, json } =>
            (experiments::plan(&metric, &baseline_rate, &min_detectable_effect, &alpha, &power, discordance.as_deref(), cluster_size.zip(icc.as_deref()))?, json, experiments::plan_text),
        experiments::Command::Report { as_of, json } => (experiments::report(project, as_of)?, json, experiments::report_text),
    };
    Ok(if json { serde_json::to_string_pretty(&value)? + "\n" } else { text(&value) })
}

pub fn run(project: &Path, command: Command) -> Result<String> {
    let value = match command {
        Command::Status => super::sidecar::status(project, STREAM)?,
        Command::Refresh { metric, cohort, from, to, horizon_ms, by } => {
            let extra = match metric {
                None => None,
                Some(metric) => {
                    let args = query::Args { metrics: vec![metric], cohort, from, to, as_of: None, as_of_seq: None, by, horizon_ms, drill: None,
                        page_size: query::DEFAULT_PAGE, cursor: None, json: true };
                    query::request(&args)?.cells.into_iter().next()
                }
            };
            store::refresh(project, extra)?
        }
        Command::Rebuild { verify } => store::rebuild(project, verify)?,
        Command::Snapshot => store::snapshot(project)?,
        Command::Revisions { metric } => store::revisions(project, metric.as_deref())?,
        Command::Plans => store::plans(project)?,
    };
    Ok(serde_json::to_string_pretty(&value)? + "\n")
}

/// Lane hook: the query service adds no metric keys to `telemetry report`.
pub fn metrics(_project: &Path, _since: Option<i64>) -> Result<BTreeMap<String, Value>> { Ok(BTreeMap::new()) }

/// Ticker telemetry pass: refresh tracked cells, rate-limited (`store::TICK_INTERVAL_MS`).
pub fn tick(project: &Path, _budget: super::codex::Budget) -> Result<()> { store::tick(project) }

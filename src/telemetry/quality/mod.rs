//! Lane C (docs/telemetry/phase2-lanes.md): sidecar stream `quality`. Hooks
//! registered centrally in `super::LANES`; this lane adds subcommands, metrics,
//! tick work and `migrations/telemetry/quality/NNNN_*.sql` here only.
//! Contracts: docs/telemetry/contracts-quality.md.
use anyhow::Result;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

pub(crate) mod groups;
mod outcomes;
mod proxy;
mod registry;

pub const STREAM: &str = "quality";
/// `include_str!` of `migrations/telemetry/quality/`, in order; index + 1 is the stream version.
pub const MIGRATIONS: &[&str] = &[
    include_str!("../../../migrations/telemetry/quality/0001_proxy_signals.sql"),
    include_str!("../../../migrations/telemetry/quality/0002_integration_outcomes.sql"),
];

/// `herdr-projects telemetry <slug> quality ...`
#[derive(clap::Subcommand)]
pub enum Command {
    /// Stream version of this lane's sidecar tables. Read-only.
    Status,
    /// Observe TM3.7 proxy signals (first-candidate CI, test weakening) and
    /// integration outcomes (revert, survival) into the sidecar. Reads
    /// `state.db` read-only; never changes acceptance.
    Collect {
        /// Outcome horizon after each integration, in days.
        #[arg(long, default_value_t = outcomes::DEFAULT_HORIZON_DAYS, value_parser = clap::value_parser!(u32).range(1..=3650))]
        horizon_days: u32,
    },
    /// Lane metrics (M41, M42, M45-M48, flaky tests) as JSON. Read-only.
    Report {
        /// Activity window start (Unix ms): by the first CI run (M45), by the integration (M47, M48).
        #[arg(long)]
        since: Option<i64>,
        /// Outcome horizon for M47 and M48, in days.
        #[arg(long, default_value_t = outcomes::DEFAULT_HORIZON_DAYS, value_parser = clap::value_parser!(u32).range(1..=3650))]
        horizon_days: u32,
    },
    /// TM3.8 candidate groups: sealed arms, selection and per-arm outcome and cost.
    Groups {
        #[command(subcommand)]
        command: groups::Command,
    },
}

/// The command's stdout.
pub fn run(project: &Path, command: Command) -> Result<String> {
    let value = match command {
        Command::Status => super::sidecar::status(project, STREAM)?,
        Command::Collect { horizon_days } => serde_json::json!({
            "proxy_signals": proxy::collect(project, true, usize::MAX)?,
            "integration_outcomes": outcomes::collect(project, true, horizon_days, COLLECT_INTEGRATIONS * outcomes::CALLS_PER_INTEGRATION)?,
        }),
        Command::Groups { command } => groups::run(project, command)?,
        Command::Report { since, horizon_days } => serde_json::json!({"metrics": lane_metrics(project, since, horizon_days)?, "since_unix_ms": since, "horizon_days": horizon_days}),
    };
    Ok(serde_json::to_string_pretty(&value)? + "\n")
}

/// Integrations observed per `quality collect` (tick: one), each within
/// `outcomes::CALLS_PER_INTEGRATION` git calls.
const COLLECT_INTEGRATIONS: usize = 16;

fn lane_metrics(project: &Path, since: Option<i64>, horizon_days: u32) -> Result<BTreeMap<String, Value>> {
    let (m47, m48) = outcomes::metrics(project, since, horizon_days)?;
    let mut paired = groups::paired_metrics(project, since, None)?;
    let unavailable = |definition: &str, name: &str, reason: &str| serde_json::json!({"definition": definition, "name": name, "proxy": true,
        "source_trust": "proxy_observed", "value": {"status": "unavailable", "reason": reason}});
    let mut metrics = BTreeMap::from([
        ("M45".to_owned(), proxy::m45(project, since)?),
        ("M46".to_owned(), unavailable("M46.proxy-v1", "main_breakage_after_integration_proxy", "no_main_check_producer")),
        ("M47".to_owned(), m47),
        ("M48".to_owned(), m48),
        ("flaky_tests".to_owned(), unavailable("flaky_tests.proxy-v1", "newly_flaky_tests_proxy", "no_repeat_runs")),
    ]);
    metrics.append(&mut paired);
    Ok(metrics)
}

/// Metrics merged into `telemetry <slug> report` (`super::metrics::report`),
/// M47/M48 at the default horizon.
pub fn metrics(project: &Path, since: Option<i64>) -> Result<BTreeMap<String, Value>> {
    lane_metrics(project, since, outcomes::DEFAULT_HORIZON_DAYS)
}

/// Ticker telemetry pass, after the Codex collect, within `budget`. Writes only
/// an existing sidecar and runs at most `TICK_DIFFS` diffs and one
/// integration's outcome (default horizon) per pass.
pub fn tick(project: &Path, _budget: super::codex::Budget) -> Result<()> {
    const TICK_DIFFS: usize = 16;
    proxy::collect(project, false, TICK_DIFFS)?;
    outcomes::collect(project, false, outcomes::DEFAULT_HORIZON_DAYS, outcomes::CALLS_PER_INTEGRATION).map(drop)
}

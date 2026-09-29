//! Lane C (docs/telemetry/phase2-lanes.md): sidecar stream `quality`. Hooks
//! registered centrally in `super::LANES`; this lane adds subcommands, metrics,
//! tick work and `migrations/telemetry/quality/NNNN_*.sql` here only.
//! Contracts: docs/telemetry/contracts-quality.md.
use anyhow::Result;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

mod proxy;

pub const STREAM: &str = "quality";
/// `include_str!` of `migrations/telemetry/quality/`, in order; index + 1 is the stream version.
pub const MIGRATIONS: &[&str] = &[include_str!("../../../migrations/telemetry/quality/0001_proxy_signals.sql")];

/// `herdr-projects telemetry <slug> quality ...`
#[derive(clap::Subcommand)]
pub enum Command {
    /// Stream version of this lane's sidecar tables. Read-only.
    Status,
    /// Observe TM3.7 proxy signals (first-candidate CI, test weakening) into
    /// the sidecar. Reads `state.db` read-only; never changes acceptance.
    Collect,
    /// Lane metrics (M45) as JSON. Read-only.
    Report {
        /// Activity window start (Unix ms), by the first CI run.
        #[arg(long)]
        since: Option<i64>,
    },
}

/// The command's stdout.
pub fn run(project: &Path, command: Command) -> Result<String> {
    let value = match command {
        Command::Status => super::sidecar::status(project, STREAM)?,
        Command::Collect => serde_json::to_value(proxy::collect(project, true, usize::MAX)?)?,
        Command::Report { since } => serde_json::json!({"metrics": {"M45": proxy::m45(project, since)?}, "since_unix_ms": since}),
    };
    Ok(serde_json::to_string_pretty(&value)? + "\n")
}

/// Metrics merged into `telemetry <slug> report` (`super::metrics::report`).
/// Empty until `metrics::text` (steward) renders lane metrics: the fleet pane
/// must show every reported metric. M45 is `quality report` meanwhile.
pub fn metrics(_project: &Path, _since: Option<i64>) -> Result<BTreeMap<String, Value>> { Ok(BTreeMap::new()) }

/// Ticker telemetry pass, after the Codex collect, within `budget`. Writes only
/// an existing sidecar and runs at most `TICK_DIFFS` diffs per pass.
pub fn tick(project: &Path, _budget: super::codex::Budget) -> Result<()> {
    const TICK_DIFFS: usize = 16;
    proxy::collect(project, false, TICK_DIFFS).map(drop)
}

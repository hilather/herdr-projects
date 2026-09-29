//! Lane B (docs/telemetry/phase2-lanes.md): sidecar stream `accounting`. Hooks
//! registered centrally in `super::LANES`; this lane adds subcommands, metrics,
//! tick work and `migrations/telemetry/accounting/NNNN_*.sql` here only.
use anyhow::Result;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

pub const STREAM: &str = "accounting";
/// `include_str!` of `migrations/telemetry/accounting/`, in order; index + 1 is the stream version.
pub const MIGRATIONS: &[&str] = &[];

/// `herdr-projects telemetry <slug> accounting ...`
#[derive(clap::Subcommand)]
pub enum Command {
    /// Stream version of this lane's sidecar tables. Read-only.
    Status,
}

/// The command's stdout.
pub fn run(project: &Path, command: Command) -> Result<String> {
    match command { Command::Status => Ok(serde_json::to_string_pretty(&super::sidecar::status(project, STREAM)?)? + "\n") }
}

/// Metrics merged into `telemetry <slug> report` (`super::metrics::report`).
pub fn metrics(_project: &Path, _since: Option<i64>) -> Result<BTreeMap<String, Value>> { Ok(BTreeMap::new()) }

/// Ticker telemetry pass, after the Codex collect, within `budget`. Writes only the sidecar.
pub fn tick(_project: &Path, _budget: super::codex::Budget) -> Result<()> { Ok(()) }

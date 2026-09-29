//! Lane B (docs/telemetry/phase2-lanes.md): sidecar stream `accounting`. Hooks
//! registered centrally in `super::LANES`; this lane adds subcommands, metrics,
//! tick work and `migrations/telemetry/accounting/NNNN_*.sql` here only.
//! Contract: docs/telemetry/contracts-accounting.md.
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub mod cost;
pub mod graph;
pub mod ledger;

pub const STREAM: &str = "accounting";
/// `include_str!` of `migrations/telemetry/accounting/`, in order; index + 1 is the stream version.
pub const MIGRATIONS: &[&str] = &[include_str!("../../../migrations/telemetry/accounting/0001_usage_ledger.sql"),
    include_str!("../../../migrations/telemetry/accounting/0002_session_graph.sql"),
    include_str!("../../../migrations/telemetry/accounting/0003_rate_cards.sql")];

/// `herdr-projects telemetry <slug> accounting ...`
#[derive(clap::Subcommand)]
pub enum Command {
    /// Stream version of this lane's sidecar tables. Read-only.
    Status,
    /// Rebuild the usage ledger, session graph and model segments from the collected Codex rows. Writes only the sidecar.
    Sync,
    /// The synced usage ledger: entries with their provenance. Read-only.
    Entries,
    /// The synced session graph and model segments per session, with the rollup. Read-only.
    Sessions,
    /// Append a rate card version from a local TOML (`.toml`) or JSON file.
    /// Rates are decimal strings; an existing version is never changed. Writes only the sidecar.
    ImportRateCard { file: std::path::PathBuf },
    /// Every imported rate card version. Read-only.
    RateCards,
    /// Value the synced ledger with the rate cards effective at usage time; appends a
    /// calculation revision when the result changed. Writes only the sidecar.
    Reprice,
    /// Published-rate estimates per attempt and session, with basis, rate card and coverage. Read-only.
    Cost {
        /// Print JSON (exact decimals) instead of text (rounded to 6 places).
        #[arg(long)]
        json: bool,
        /// Show this earlier calculation revision instead of the latest.
        #[arg(long)]
        revision: Option<i64>,
    },
}

fn unavailable(reason: &str) -> Value {
    json!({"status": "unavailable", "reason": reason})
}

/// The command's stdout.
pub fn run(project: &Path, command: Command) -> Result<String> {
    let value = match command {
        Command::Status => super::sidecar::status(project, STREAM)?,
        Command::Sync => match super::sidecar::open(project, false)? {
            Some(mut db) => ledger::sync(&mut db)?,
            None => unavailable("collection_not_run"),
        },
        Command::Entries => match super::sidecar::read(project)? {
            Some(db) => ledger::read(&db)?,
            None => unavailable("collection_not_run"),
        },
        Command::Sessions => match super::sidecar::read(project)? {
            Some(db) => graph::read(&db)?,
            None => unavailable("collection_not_run"),
        },
        Command::ImportRateCard { file } => match super::sidecar::open(project, false)? {
            Some(mut db) => cost::import(&mut db, &file)?,
            None => unavailable("collection_not_run"),
        },
        Command::RateCards => match super::sidecar::read(project)? {
            Some(db) => cost::list(&db)?,
            None => unavailable("collection_not_run"),
        },
        Command::Reprice => match super::sidecar::open(project, false)? {
            Some(mut db) => cost::reprice(&mut db)?,
            None => unavailable("collection_not_run"),
        },
        Command::Cost { json, revision } => {
            let value = match super::sidecar::read(project)? {
                Some(db) => cost::cost(&db, revision)?,
                None => unavailable("collection_not_run"),
            };
            if !json { return Ok(cost::text(&value)); }
            value
        }
    };
    Ok(serde_json::to_string_pretty(&value)? + "\n")
}

fn metric(id: &str, name: &str, mut body: Value) -> Value {
    body["definition"] = json!(format!("{id}.slice-v1"));
    body["name"] = json!(name);
    body
}

/// Contracts §6 M08/M09, replacing the central ones with the same numbers:
/// counted ledger entries (derived from the Codex tables, so no sync is
/// needed) of certified sessions, i.e. with a source bound to a known attempt,
/// not quarantined, of a certified version, started in the window.
pub fn metrics(project: &Path, since: Option<i64>) -> Result<BTreeMap<String, Value>> {
    let both = |m08: Value, m09: Value| BTreeMap::from([("M08".to_owned(), metric("M08", "input_tokens", m08)), ("M09".to_owned(), metric("M09", "output_tokens", m09))]);
    let Some(db) = super::sidecar::read(project)? else {
        let body = json!({"value": unavailable("no_certified_source")});
        return Ok(both(body.clone(), body));
    };
    let state = project.join(".state/state.db");
    let known: BTreeSet<String> = if state.exists() {
        super::read_only(&state)?.prepare("SELECT id FROM attempts")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
    } else { BTreeSet::new() };
    // A source holding rows stored while its version was uncertified stays uncertified.
    type Source = (String, String, Option<String>, String, bool, Option<i64>);
    let sources: Vec<Source> = db.prepare("SELECT s.session_id,s.binding,s.attempt_id,
        CASE WHEN EXISTS(SELECT 1 FROM codex_usage u WHERE u.path_digest=s.path_digest AND u.reason='cli_version_uncertified') THEN '' ELSE s.cli_version END,
        EXISTS(SELECT 1 FROM codex_quarantine q WHERE q.session_id=s.session_id),s.session_unix_ms FROM rollout_sources s ORDER BY s.path_digest")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?.collect::<rusqlite::Result<_>>()?;
    let (mut certified, mut excluded) = (BTreeSet::new(), BTreeMap::<&str, usize>::new());
    for (session, binding, attempt, version, quarantined, at) in &sources {
        if since.is_some_and(|since| at.is_none_or(|at| at < since)) { continue; }
        let reason = if binding != "bound" { binding.as_str() } else if !attempt.as_ref().is_some_and(|a| known.contains(a)) { "orphan" }
            else if *quarantined { "quarantined" } else if !super::codex::certified(version) { "cli_version_uncertified" }
            else { certified.insert(session.as_str()); continue };
        *excluded.entry(reason).or_default() += 1;
    }
    let coverage = json!({"certified_sessions": certified.len(), "excluded": excluded});
    if certified.is_empty() {
        let body = json!({"value": unavailable("no_certified_source"), "coverage": coverage});
        return Ok(both(body.clone(), body));
    }
    let (mut input, mut output, mut reasoning) = (0, 0, 0);
    for entry in ledger::derive(&db)?.iter().filter(|e| e.counted() && certified.contains(e.session.as_str())) {
        let Some(n) = entry.normalized else { continue };
        (input, output, reasoning) = (input + n[0], output + n[4], reasoning + n[5]);
    }
    Ok(both(json!({"value": input, "coverage": coverage}), json!({"value": output, "reasoning_output_tokens": reasoning, "coverage": coverage})))
}

/// Ticker telemetry pass, after the Codex collect: rebuild the ledger of an
/// existing sidecar. Writes only the sidecar; never creates it.
pub fn tick(project: &Path, _budget: super::codex::Budget) -> Result<()> {
    if let Some(mut db) = super::sidecar::open(project, false)? { ledger::sync(&mut db)?; }
    Ok(())
}

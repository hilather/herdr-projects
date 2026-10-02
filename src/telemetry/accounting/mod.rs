//! Lane B (docs/telemetry/phase2-lanes.md): sidecar stream `accounting`. Hooks
//! registered centrally in `super::LANES`; this lane adds subcommands, metrics,
//! tick work and `migrations/telemetry/accounting/NNNN_*.sql` here only.
//! Contract: docs/telemetry/contracts-accounting.md.
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub mod attention;
pub mod budget;
pub(crate) mod cache;
pub mod charges;
pub mod cost;
pub mod fleet;
pub mod fx;
pub mod graph;
pub mod ledger;
pub mod quota;
pub mod tools;
pub(crate) mod otlp;

pub const STREAM: &str = "accounting";
/// `include_str!` of `migrations/telemetry/accounting/`, in order; index + 1 is the stream version.
pub const MIGRATIONS: &[&str] = &[include_str!("../../../migrations/telemetry/accounting/0001_usage_ledger.sql"),
    include_str!("../../../migrations/telemetry/accounting/0002_session_graph.sql"),
    include_str!("../../../migrations/telemetry/accounting/0003_rate_cards.sql"),
    include_str!("../../../migrations/telemetry/accounting/0004_quota_windows.sql"),
    include_str!("../../../migrations/telemetry/accounting/0005_attention.sql"),
    include_str!("../../../migrations/telemetry/accounting/0006_a4_metadata.sql"),
    include_str!("../../../migrations/telemetry/accounting/0007_thread_lineage.sql"),
    include_str!("../../../migrations/telemetry/accounting/0008_drop_superseded.sql"),
    include_str!("../../../migrations/telemetry/accounting/0009_valuation_deltas.sql"),
    include_str!("../../../migrations/telemetry/accounting/0010_charges_fx.sql"),
    include_str!("../../../migrations/telemetry/accounting/0011_quota_window_lookup.sql"),
    include_str!("../../../migrations/telemetry/accounting/0012_incremental_sync.sql"),
    include_str!("../../../migrations/telemetry/accounting/0013_claude_code.sql"),
    include_str!("../../../migrations/telemetry/accounting/0014_opencode.sql"),
    include_str!("../../../migrations/telemetry/accounting/0015_read_aggregates.sql"),
    include_str!("../../../migrations/telemetry/accounting/0016_cache_read_share.sql"),
    include_str!("../../../migrations/telemetry/accounting/0017_compact_dispositions.sql"),
    include_str!("../../../migrations/telemetry/accounting/0018_muse.sql"),
    include_str!("../../../migrations/telemetry/accounting/0019_otlp_ledger.sql"),
    include_str!("../../../migrations/telemetry/accounting/0020_otlp_devin.sql")];

/// `herdr-farm telemetry <slug> accounting ...`
#[derive(clap::Subcommand)]
pub enum Command {
    /// Stream version of this lane's sidecar tables. Read-only.
    Status,
    /// Sync changed sessions and quota accounts; rebuild after invalidation. Writes only the sidecar.
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
        #[arg(long, conflicts_with = "as_of")]
        revision: Option<i64>,
        /// Show the latest revision computed at or before this instant (Unix ms).
        #[arg(long)]
        as_of: Option<i64>,
    },
    /// Append provider charge and invoice revisions from a local TOML (`.toml`) or JSON
    /// file marked synthetic (fixture-only). Amounts are decimal strings. Writes only the sidecar.
    ImportCharges { file: std::path::PathBuf },
    /// Provider-reported charges (basis provider_billed) with their corrections, reconciled
    /// against the estimate of the same usage; unmatched charges and uncharged estimates
    /// apart, never added. Prints JSON. Read-only.
    Charges {
        /// Charges imported and the valuation revision computed at or before this instant (Unix ms).
        #[arg(long)]
        as_of: Option<i64>,
    },
    /// Allocate an invoice or subscription bill to attempts by a named, versioned rule.
    /// Prints JSON. Read-only.
    Allocate {
        invoice: String,
        /// Allocation rule.
        #[arg(long, default_value = "by_total_tokens.v1")]
        rule: String,
        /// Invoice revision and valuation revision as of this instant (Unix ms).
        #[arg(long)]
        as_of: Option<i64>,
    },
    /// Append a dated exchange-rate table version from a local TOML (`.toml`) or JSON file
    /// marked synthetic (fixture-only). Rates are decimal strings. Writes only the sidecar.
    ImportFx { file: std::path::PathBuf },
    /// The stored estimates converted to one currency with the rate effective over each
    /// entry's usage interval, as a separate dated valuation. Prints JSON. Read-only.
    Fx {
        /// Target currency (ISO 4217).
        #[arg(long)]
        to: String,
        /// Convert this calculation revision instead of the latest.
        #[arg(long, conflicts_with = "as_of")]
        revision: Option<i64>,
        /// The valuation revision and exchange-rate tables as of this instant (Unix ms).
        #[arg(long)]
        as_of: Option<i64>,
    },
    /// TM2.4 budget bridge in shadow mode: what the canonical budget policy (and an optional
    /// synthetic what-if policy) would decide over the valued ledger. Never enforced; reads
    /// state.db read-only. Prints JSON.
    BudgetShadow {
        /// A synthetic what-if policy file (TOML or JSON): limits per project and task,
        /// in-flight reservation estimates and a new request.
        #[arg(long)]
        policy: Option<std::path::PathBuf>,
        /// The valuation revision computed at or before this instant (Unix ms).
        #[arg(long)]
        as_of: Option<i64>,
    },
    /// Synced quota windows (native units), observation trust, M38/M39 and
    /// headroom per limit window at each dispatch decision (extended M40). Read-only.
    Quota {
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// One attention observation pass: read-only `herdr agent list` per recorded
    /// socket, one sample per launched, unterminated attempt. Writes only the sidecar.
    ObserveAttention,
    /// Human attention per launched attempt: waiting intervals (unioned, censored
    /// when unobserved), observation gaps, and M31–M33. Read-only.
    Attention {
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Tool calls and command executions per bound session (A6 metadata only),
    /// with coverage and M16–M18. Maintained summaries with live fallback; read-only.
    Tools {
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Fleet efficiency from canonical attempt lifecycle, acceptance and integration
    /// rows: concurrency buckets and M35 (also per agent configuration), integration
    /// conflicts (M36), M34/M37. Read-only.
    Fleet {
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
        /// Activity window length in minutes; must divide 1440 (windows align to UTC days).
        #[arg(long, default_value_t = fleet::DEFAULT_WINDOW_MINUTES)]
        window_minutes: i64,
    },
    /// Record the project owner's accepted reason why an ended attempt was superseded
    /// or abandoned (M37). Append-only, once per attempt; writes only `state.db`.
    Supersede {
        attempt: String,
        /// `superseded` or `abandoned`.
        #[arg(long)]
        outcome: String,
        /// `sibling_changed_same_area`, `duplicate_effort` or `other`.
        #[arg(long)]
        reason: String,
        /// The sibling attempt (required unless the reason is `other`).
        #[arg(long)]
        sibling: Option<String>,
        /// Evidence reference `<kind>:<id>` (attempt, task, submission, verified_result,
        /// integration_operation, commit, candidate_group), once per reference (1-16).
        #[arg(long = "evidence", required = true)]
        evidence: Vec<String>,
    },
}

fn unavailable(reason: &str) -> Value {
    json!({"status": "unavailable", "reason": reason})
}

/// The command's stdout.
pub fn run(project: &Path, command: Command) -> Result<String> {
    let value = match command {
        Command::Status => {
            let mut value = super::sidecar::status(project, STREAM)?;
            if value["version"] == MIGRATIONS.len() && let Some(db) = super::sidecar::read(project)? && let Some(status) = ledger::status(&db)? {
                value["sync"] = status;
            }
            value
        },
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
        Command::Cost { json, revision, as_of } => {
            let value = match super::sidecar::read(project)? {
                Some(db) => cost::cost(&db, revision, as_of)?,
                None => unavailable("collection_not_run"),
            };
            if !json { return Ok(cost::text(&value)); }
            value
        }
        Command::ImportCharges { file } => match super::sidecar::open(project, false)? {
            Some(mut db) => charges::import(&mut db, &file)?,
            None => unavailable("collection_not_run"),
        },
        Command::Charges { as_of } => match super::sidecar::read(project)? {
            Some(db) => charges::charges(&db, as_of)?,
            None => unavailable("collection_not_run"),
        },
        Command::Allocate { invoice, rule, as_of } => match super::sidecar::read(project)? {
            Some(db) => charges::allocate(&db, &invoice, &rule, as_of)?,
            None => unavailable("collection_not_run"),
        },
        Command::ImportFx { file } => match super::sidecar::open(project, false)? {
            Some(mut db) => fx::import(&mut db, &file)?,
            None => unavailable("collection_not_run"),
        },
        Command::Fx { to, revision, as_of } => match super::sidecar::read(project)? {
            Some(db) => fx::convert(&db, &to, revision, as_of)?,
            None => unavailable("collection_not_run"),
        },
        Command::BudgetShadow { policy, as_of } => budget::shadow(project, policy.as_deref(), as_of)?,
        Command::Quota { json } => {
            let value = match super::sidecar::read(project)? {
                Some(db) => quota::read(project, &db)?,
                None => unavailable("collection_not_run"),
            };
            if !json { return Ok(quota::text(&value)); }
            value
        }
        Command::ObserveAttention => match super::sidecar::open(project, false)? {
            Some(mut db) => attention::observe(project, &mut db, super::codex::Budget::CLI)?,
            None => unavailable("collection_not_run"),
        },
        Command::Attention { json } => {
            let value = match super::sidecar::read(project)? {
                Some(db) => attention::read(project, &db)?,
                None => unavailable("collection_not_run"),
            };
            if !json { return Ok(attention::text(&value)); }
            value
        }
        Command::Tools { json } => {
            let value = match super::sidecar::read(project)? {
                Some(db) => tools::read(project, &db)?,
                None => unavailable("collection_not_run"),
            };
            if !json { return Ok(tools::text(&value)); }
            value
        }
        Command::Fleet { json, window_minutes } => {
            let value = fleet::read(project, window_minutes)?;
            if !json { return Ok(fleet::text(&value)); }
            value
        }
        Command::Supersede { attempt, outcome, reason, sibling, evidence } =>
            fleet::supersede(project, crate::store::SupersessionRequest { attempt, outcome, reason, sibling, evidence })?,
    };
    Ok(serde_json::to_string_pretty(&value)? + "\n")
}

fn metric(id: &str, name: &str, mut body: Value) -> Value {
    body["definition"] = json!(format!("{id}.slice-v1"));
    body["name"] = json!(name);
    body
}

/// M38/M39 (§5): no certified Codex source, so `unavailable` with the reason.
fn with_availability(mut metrics: BTreeMap<String, Value>) -> BTreeMap<String, Value> {
    for (id, name, body) in quota::availability_metrics() { metrics.insert(id.to_owned(), metric(id, name, body)); }
    metrics
}

/// M08/M09 (below), M38/M39 (§5), M31–M33 (§6, replacing the central
/// `attention_not_collected` entries), M16–M18 (§9), M34–M37 (§10),
/// M12/M14 (§12), M11 (§13) and M04 (§14).
pub fn metrics(project: &Path, since: Option<i64>) -> Result<BTreeMap<String, Value>> {
    let mut metrics = BTreeMap::new();
    metrics.extend(metrics_without_attention(project, since)?);
    metrics.extend(metric_group(project, "attention", since)?);
    Ok(metrics)
}

pub(crate) fn metrics_without_attention(project: &Path, since: Option<i64>) -> Result<BTreeMap<String, Value>> {
    let mut metrics = BTreeMap::new();
    for group in ["usage", "cost", "charges", "budget", "tools", "fleet"] {
        metrics.extend(metric_group(project, group, since)?);
    }
    Ok(metrics)
}

/// Query only the requested metric family; M08 must not build tool, fleet,
/// cost and attention reports as a side effect.
pub(crate) fn metric_group(project: &Path, group: &str, since: Option<i64>) -> Result<BTreeMap<String, Value>> {
    if !super::analytics::inputs::clock(group) && let Some(db) = super::sidecar::read(project)?
        && let Some(generations) = super::analytics::inputs::generations(&db)? {
        let canonical = super::analytics::inputs::canonical(project)?;
        let stamp = super::analytics::inputs::stamp(group, &canonical, &generations);
        if let Some(body) = super::analytics::inputs::cached(&db, group, since, &stamp)? {
            return match body { Value::Object(values) => Ok(values.into_iter().collect()), body => Ok(serde_json::from_value(body)?) };
        }
    }
    metric_group_uncached(project, group, since, true)
}

pub(crate) fn metric_group_uncached(project: &Path, group: &str, since: Option<i64>, aggregates: bool) -> Result<BTreeMap<String, Value>> {
    match group {
        "usage" => {
            let mut metrics = usage_metrics_with(project, since, aggregates)?;
            metrics.insert("M10".to_owned(), cache::metric(project, since, aggregates)?);
            Ok(metrics)
        },
        "cost" => cost::metrics_with(project, since, aggregates),
        "charges" => charges::metrics(project, since),
        "budget" => budget::metrics(project, since),
        "attention" => attention::metrics(project, since),
        "tools" => tools::metrics_with(project, since, aggregates),
        _ => fleet::metrics_with(project, since, aggregates),
    }
}

/// Contracts §6 M08/M09, replacing the central ones with the same numbers:
/// counted ledger entries (derived from the Codex tables, so no sync is
/// needed) of certified sessions, i.e. with a source bound to a known attempt,
/// not quarantined, of a certified version, with every record accepted,
/// started in the window.
fn usage_metrics_with(project: &Path, since: Option<i64>, aggregates: bool) -> Result<BTreeMap<String, Value>> {
    let both = |m08: Value, m09: Value| with_availability(BTreeMap::from([("M08".to_owned(), metric("M08", "input_tokens", m08)),
        ("M09".to_owned(), metric("M09", "output_tokens", m09))]));
    let Some(db) = super::sidecar::read(project)? else {
        let body = json!({"value": unavailable("no_certified_source")});
        return Ok(both(body.clone(), body));
    };
    // The frontier, certification and totals must describe one SQLite
    // snapshot while collectors and syncs race with this read.
    let _snapshot = db.unchecked_transaction()?;
    let state = project.join(".state/state.db");
    let known: BTreeSet<String> = if state.exists() {
        super::read_only(&state)?.prepare("SELECT id FROM attempts")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
    } else { BTreeSet::new() };
    // A source holding rows stored while its version was uncertified stays uncertified.
    type Source = (String, String, Option<String>, String, bool, Option<i64>, bool);
    let sql = if aggregates && ledger::aggregates_current(&db)? {
        "SELECT s.session_id,s.binding,s.attempt_id,CASE WHEN a.uncertified THEN '' ELSE s.cli_version END,
        a.quarantined,s.session_unix_ms,a.rejected FROM rollout_sources s JOIN accounting_source_summary a USING(path_digest) ORDER BY s.path_digest"
    } else {
        "SELECT s.session_id,s.binding,s.attempt_id,
        CASE WHEN EXISTS(SELECT 1 FROM codex_usage u WHERE u.path_digest=s.path_digest AND u.reason='cli_version_uncertified') THEN '' ELSE s.cli_version END,
        EXISTS(SELECT 1 FROM codex_quarantine q WHERE q.session_id=s.session_id),s.session_unix_ms,
        EXISTS(SELECT 1 FROM codex_usage u WHERE u.session_id=s.session_id AND u.reason='invariant_violation') FROM rollout_sources s ORDER BY s.path_digest"
    };
    let sources: Vec<Source> = db.prepare(sql)?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?.collect::<rusqlite::Result<_>>()?;
    let losing = if sources.iter().any(|s| s.0.starts_with("otlp:")) { otlp::losing_sessions(&db)? } else { BTreeSet::new() };
    let (mut certified, mut excluded) = (BTreeSet::new(), BTreeMap::<&str, usize>::new());
    for (session, binding, attempt, version, quarantined, at, rejected) in &sources {
        if since.is_some_and(|since| at.is_none_or(|at| at < since)) { continue; }
        let reason = if binding != "bound" { binding.as_str() } else if !attempt.as_ref().is_some_and(|a| known.contains(a)) { "orphan" }
            else if losing.contains(session) { "native_surface_precedence" }
            else if *quarantined { "quarantined" } else if !super::codex::accepted_version(version) { "cli_version_uncertified" }
            else if *rejected { "records_not_accepted" }
            else { certified.insert(session.as_str()); continue };
        *excluded.entry(reason).or_default() += 1;
    }
    let coverage = json!({"certified_sessions": certified.len(), "excluded": excluded});
    if certified.is_empty() {
        let body = json!({"value": unavailable("no_certified_source"), "coverage": coverage});
        return Ok(both(body.clone(), body));
    }
    let (mut input, mut output, mut reasoning) = (0, 0, 0);
    if aggregates && ledger::aggregates_current(&db)? {
        let mut totals = db.prepare("SELECT input_tokens,output_tokens,reasoning_tokens FROM accounting_usage_totals WHERE session_id=?1")?;
        for session in &certified {
            let (i, o, r): (i64, i64, i64) = totals.query_row([session], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            (input, output, reasoning) = (input + i, output + o, reasoning + r);
        }
    } else {
        for entry in ledger::derive(&db)?.iter().filter(|e| e.counted() && certified.contains(e.session.as_str())) {
            let Some(n) = entry.normalized else { continue };
            (input, output, reasoning) = (input + n[0], output + n[4], reasoning + n[5]);
        }
    }
    let reasoning = if certified.iter().any(|s| s.starts_with("claude-code:") || s.starts_with("otlp:claude-code:")) { unavailable("reasoning_tokens_not_reported") } else { json!(reasoning) };
    Ok(both(json!({"value": input, "coverage": coverage}), json!({"value": output, "reasoning_output_tokens": reasoning, "coverage": coverage})))
}

/// Ticker telemetry pass, after the Codex collect: one attention observation
/// pass within the tick budget, then sync the ledger of an existing
/// sidecar, then reprice it when a rate card exists and the rate cards or
/// the ledger changed since the last reprice (§12), within the tick budget.
/// Writes only the sidecar; never creates it.
pub fn tick(project: &Path, budget: super::codex::Budget) -> Result<()> {
    tick_observed(project, budget).map(|_| ())
}

/// The same atomic accounting tick, with diagnostic timings for scale measurements.
pub fn tick_observed(project: &Path, budget: super::codex::Budget) -> Result<Value> {
    match tick_once(project, budget) {
        Err(error) if crate::telemetry::writer::busy(&error) => Ok(json!({"deferred": "writer_busy"})),
        result => result,
    }
}

fn tick_once(project: &Path, budget: super::codex::Budget) -> Result<Value> {
    if let Some(mut db) = super::sidecar::open(project, false)? {
        db.busy_timeout(std::time::Duration::ZERO)?;
        let t = std::time::Instant::now();
        let observed = attention::observe(project, &mut db, budget);
        let attention_ms = t.elapsed().as_secs_f64() * 1e3;
        let (_, mut diagnostics) = match ledger::sync_observed(&mut db) {
            Ok(result) => result,
            Err(error) if crate::telemetry::writer::busy(&error) => {
                if let Err(error) = observed && !crate::telemetry::writer::busy(&error) { return Err(error); }
                return Ok(json!({"deferred": "writer_busy", "attention_ms": attention_ms}));
            }
            Err(error) => return Err(error),
        };
        if diagnostics["deferred"].is_string() {
            observed?;
            diagnostics["attention_ms"] = json!(attention_ms);
            return Ok(diagnostics);
        }
        let t = std::time::Instant::now();
        cost::tick(&mut db, budget)?;
        diagnostics["attention_ms"] = json!(attention_ms);
        diagnostics["cost_tick_ms"] = json!(t.elapsed().as_secs_f64() * 1e3);
        observed?;
        return Ok(diagnostics);
    }
    Ok(Value::Null)
}

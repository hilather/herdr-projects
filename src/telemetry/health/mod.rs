//! TM4.5 health alerts and advisory routing evidence (plan doc 12 TM4.5,
//! doc 08 §6, doc 15 §7): the declared health rules (`rules.rs`), their
//! states and deduplicated alerts in sidecar stream `health` (`store.rs`),
//! inbox and optional external notices (`notify.rs`) and advisory
//! recommendations with M50 evidence freshness (`recommend.rs`). Hooks
//! registered centrally in `super::LANES`; migrations under
//! `migrations/telemetry/health/` only. Contract:
//! docs/telemetry/contracts-health.md. Never grants launch, changes budgets,
//! profiles or model access, or accepts results; only `health notify` writes
//! `state.db`, and only inbox notices.
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

pub mod notify;
pub mod recommend;
pub mod rules;
pub mod store;

pub const STREAM: &str = "health";
/// `include_str!` of `migrations/telemetry/health/`, in order; index + 1 is the stream version.
pub const MIGRATIONS: &[&str] = &[include_str!("../../../migrations/telemetry/health/0001_health_alerts.sql")];
pub const CONTRACT: &str = "telemetry-health.v1";

/// `herdr-projects telemetry <slug> health [...]`
#[derive(clap::Args, Clone, Debug)]
pub struct Args {
    #[command(subcommand)]
    pub command: Option<Command>,
    /// Print JSON instead of text.
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Subcommand, Clone, Debug)]
pub enum Command {
    /// Evaluate every rule now and record states and alerts (dedup, cooldown). Writes only the sidecar `health` stream.
    Evaluate {
        #[arg(long)]
        json: bool,
    },
    /// Open alerts, plus alerts opened or resolved at or after --since (Unix ms). Read-only.
    Alerts {
        #[arg(long)]
        since: Option<i64>,
        #[arg(long)]
        json: bool,
    },
    /// Write each open, not yet noticed alert as one inbox notice (deduplicated);
    /// with --external send open alerts to the deployment's configured destination (off by default).
    Notify {
        #[arg(long)]
        external: bool,
    },
    /// The declared rule table (`health-rules.v1`). Read-only.
    Rules,
    /// Stream version of this lane's sidecar tables. Read-only.
    Status,
}

/// `health` without a subcommand: every rule evaluated live (read-only) and the recorded open alerts.
pub fn current(project: &Path) -> Result<Value> {
    let now = jiff::Timestamp::now().as_millisecond();
    let slug = store::slug(project);
    let outcomes = rules::evaluate(project, now);
    let mut summary = BTreeMap::from([("ok", 0), ("warn", 0), ("critical", 0), ("unknown", 0)]);
    for o in &outcomes { *summary.entry(o.state.as_str()).or_default() += 1; }
    let alerts = store::alerts(project, None)?;
    Ok(json!({"schema_version": 1, "contract": CONTRACT, "rules_version": rules::VERSION, "project": slug, "evaluated_unix_ms": now,
        "mode": "live_read_only", "summary": summary, "states": outcomes.iter().map(|o| o.json(&slug)).collect::<Vec<_>>(),
        "alerts": if alerts["status"] == "unavailable" { alerts } else { json!({"open": alerts["open"], "last_evaluated_unix_ms": alerts["last_evaluated_unix_ms"]}) },
        "advisory": "health states and alerts report; the canonical admission and budget paths still decide"}))
}

fn word(v: &Value) -> String { v.as_str().map_or_else(|| if v.is_null() { "-".into() } else { v.to_string() }, str::to_owned) }

fn codes(reasons: &Value) -> String {
    reasons.as_array().into_iter().flatten().filter_map(|r| r["code"].as_str()).collect::<Vec<_>>().join(",")
}

fn scope(labels: &Value) -> String {
    ["family", "service", "role"].iter().filter_map(|k| labels[*k].as_str().map(|v| if *k == "family" { v.to_owned() } else { format!("{k}={v}") })).collect::<Vec<_>>().join(" ")
}

fn alert_line(a: &Value) -> String {
    format!("  #{} {} [{}] {} {} since {} seen {}x{}\n", word(&a["alert_id"]), word(&a["rule"]), scope(&a["labels"]), word(&a["state"]), codes(&a["reasons"]),
        word(&a["opened_unix_ms"]), word(&a["occurrences"]), a["resolved_unix_ms"].as_i64().map_or(String::new(), |t| format!(" resolved {t}")))
}

/// Text form of `current`: one line per rule state, then the open alerts.
pub fn text(v: &Value) -> String {
    let mut out = format!("{} · health · {} · {}\n", word(&v["project"]), word(&v["rules_version"]), word(&v["mode"]));
    for s in v["states"].as_array().into_iter().flatten() {
        out += &format!("  {} [{}]: {} {}\n", word(&s["rule"]), scope(&s["labels"]), word(&s["state"]), codes(&s["reasons"]));
    }
    match v["alerts"]["open"].as_array() {
        Some(open) => { out += &format!("alerts open {}\n", open.len()); for a in open { out += &alert_line(a); } }
        None => out += &format!("alerts n/a ({})\n", word(&v["alerts"]["reason"])),
    }
    out
}

fn alerts_text(v: &Value) -> String {
    if v["status"] == "unavailable" { return format!("n/a ({})\n", word(&v["reason"])); }
    let mut out = format!("alerts open {}\n", v["open"].as_array().map_or(0, Vec::len));
    for a in v["open"].as_array().into_iter().flatten().chain(v["recent"].as_array().into_iter().flatten()) { out += &alert_line(a); }
    out
}

/// `herdr-projects telemetry <slug> health ...`
pub fn run(project: &Path, config_dir: &Path, args: Args) -> Result<String> {
    let pretty = |v: &Value| -> Result<String> { Ok(serde_json::to_string_pretty(v)? + "\n") };
    match args.command {
        None => { let v = current(project)?; if args.json { pretty(&v) } else { Ok(text(&v)) } }
        Some(Command::Evaluate { json }) => {
            let v = store::evaluate(project, "cli", jiff::Timestamp::now().as_millisecond())?;
            if json || args.json { pretty(&v) } else {
                Ok(format!("evaluated {} rules: opened {} updated {} resolved {} suppressed {}\n", v["states"].as_array().map_or(0, Vec::len),
                    v["opened"].as_array().map_or(0, Vec::len), v["updated"].as_array().map_or(0, Vec::len), v["resolved"].as_array().map_or(0, Vec::len),
                    v["suppressed"].as_array().map_or(0, Vec::len)))
            }
        }
        Some(Command::Alerts { since, json }) => { let v = store::alerts(project, since)?; if json || args.json { pretty(&v) } else { Ok(alerts_text(&v)) } }
        Some(Command::Notify { external: false }) => pretty(&notify::inbox(project)?),
        Some(Command::Notify { external: true }) => {
            let (receipt, lines) = notify::external(project, config_dir)?;
            // stdout destination: the alert lines are the output (a pipe), the receipt goes nowhere else.
            Ok(match lines { Some(lines) => lines, None => pretty(&receipt)? })
        }
        Some(Command::Rules) => pretty(&rules::table()),
        Some(Command::Status) => pretty(&super::sidecar::status(project, STREAM)?),
    }
}

/// `herdr-projects telemetry <slug> recommend ...`
pub fn run_recommend(project: &Path, args: &recommend::Args) -> Result<String> {
    let v = recommend::run(project, args)?;
    Ok(if args.json { serde_json::to_string_pretty(&v)? + "\n" } else { recommend::text(&v) })
}

/// Lane hook: health adds no metric keys to `telemetry report`.
pub fn metrics(_project: &Path, _since: Option<i64>) -> Result<BTreeMap<String, Value>> { Ok(BTreeMap::new()) }

/// Ticker telemetry pass: evaluate at most once per `store::TICK_INTERVAL_MS`,
/// only after an operator's first `health evaluate`. Never notifies.
pub fn tick(project: &Path, _budget: super::codex::Budget) -> Result<()> { store::tick(project) }

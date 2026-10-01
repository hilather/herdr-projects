//! TM5.3 retention, deletion and holds (plan doc 09, doc 12 TM5.3;
//! docs/telemetry/operations-runbook.md). The declared retention classes
//! (`retention.v1`, [`CLASSES`]) cover every sidecar table and every on-disk
//! artefact telemetry and the worker sandbox leave behind. `maintenance plan`
//! computes what retention would delete (read-only); `maintenance apply`
//! deletes it as the project owner (`operator:cli`, refused in a worker
//! context), under the project's runtime lock, only after `--confirm <plan
//! digest>` when anything destructive is due, and writes a salted,
//! append-only tombstone before each deletion so that no collect, rebuild or
//! restore brings the content back. Holds block deletion. Canonical state
//! (`state.db`) follows its own lifecycle: nothing here writes it.
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub mod backup;
pub mod store;

pub use store::Tombstones;

pub const POLICY: &str = "retention.v1";
pub const POLICY_FILE: &str = "telemetry-retention.toml";
pub const POLICY_SCHEMA: &str = "telemetry-retention.v1";
const DAY_MS: i64 = 86_400_000;
/// Keys listed per class and list in a plan (the digest covers all).
const LISTED: usize = 50;
/// The submission spool's entry cap per attempt (`submission_spool::ENTRY_LIMIT`).
const SPOOL_ENTRIES: u64 = 256;
/// The Git quarantine copy limit per import (`git_quarantine::BYTE_LIMIT`).
const QUARANTINE_BYTES: u64 = 1024 * 1024 * 1024;
const LIVE: [&str; 4] = ["reserved", "launching", "running", "awaiting_input"];
const TERMINAL_TASK: [&str; 3] = ["succeeded", "failed", "cancelled"];

pub const SESSIONS: &str = "sidecar.normalized_sessions";
pub const ATTENTION: &str = "sidecar.attention_samples";
pub const HEALTH: &str = "sidecar.health_evaluations";
pub const ANALYTICS: &str = "sidecar.analytics_revisions";
pub const QUARANTINE: &str = "artefact.git_quarantine";
pub const SPOOL: &str = "artefact.submission_spool";
pub const REPLAY: &str = "artefact.replay_repos";
pub const BACKUPS: &str = "artefact.backups";
pub const EXPORTS: &str = "optin.external_export_files";
pub const TOMBSTONES: &str = "ops.tombstones";

/// What maintenance does with a class.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Deleted by `maintenance apply` once older than its retention.
    Prune,
    /// Kept: source of truth without a TTL (back it up).
    Retain,
    /// Derived and rebuilt from surviving sources; pruned only with them.
    FollowsSources,
    /// Listed, never pruned by `retention.v1`.
    Listed,
    /// Owned elsewhere; telemetry cannot delete it.
    External,
    /// No store exists.
    NotBuilt,
}

impl Action {
    fn name(self) -> &'static str {
        match self {
            Action::Prune => "prune",
            Action::Retain => "retain",
            Action::FollowsSources => "follows_sources",
            Action::Listed => "listed_not_pruned",
            Action::External => "external_lifecycle",
            Action::NotBuilt => "not_built",
        }
    }
}

/// One declared retention class.
pub struct Class {
    pub id: &'static str,
    pub store: &'static str,
    pub scope: &'static str,
    pub default_days: Option<i64>,
    /// `derivable` (rebuilt from surviving sources), `source_of_truth` or `canonical`.
    pub basis: &'static str,
    /// Deletion loses something not re-derivable: `apply` needs `--confirm`.
    pub destructive: bool,
    pub action: Action,
    pub age_from: &'static str,
    pub requires: &'static str,
}

/// `retention.v1`: doc 09's defaults, one row per sidecar table group and on-disk artefact.
pub const CLASSES: &[Class] = &[
    Class { id: "sidecar.otlp", store: "telemetry.db", scope: "otlp_records, gemini_file_cursors", default_days: None, basis: "source_of_truth", destructive: true,
        action: Action::Retain, age_from: "observed_unix_ms", requires: "kept: exporters may not replay; included in full sidecar backups" },
    Class { id: "secret.otlp_tokens", store: "<config_dir>/otlp-<project-digest>.token", scope: "per-project bearer tokens", default_days: None, basis: "source_of_truth", destructive: true,
        action: Action::External, age_from: "-", requires: "never included in telemetry backups; delete to rotate while receiver stopped" },
    Class { id: SESSIONS, store: "telemetry.db", scope: "per native session: claude_messages, claude_tool_results, opencode_messages, opencode_tools, codex_*, rollout_*, collect_offsets, codex_tool_sources, source_bindings, source_observations, ingest_quarantine, coverage_gaps, source_cursors, usage_entries, usage_dispositions, model_segments, quota_window_observations, session_graph_nodes",
        default_days: Some(90), basis: "derivable_from_native_source", destructive: true, action: Action::Prune, age_from: "last durable acceptance (rollout_sources.observed_unix_ms)",
        requires: "bound attempt terminal; accounting ledger synced after acceptance with no unresolved or conflicting disposition; no quarantined record" },
    Class { id: ATTENTION, store: "telemetry.db", scope: "attention_samples", default_days: Some(90), basis: "source_of_truth", destructive: true, action: Action::Prune,
        age_from: "observed_unix_ms", requires: "attempt terminal" },
    Class { id: HEALTH, store: "telemetry.db", scope: "health_evaluations", default_days: Some(90), basis: "derivable", destructive: false, action: Action::Prune,
        age_from: "evaluated_unix_ms", requires: "none (the newest 1000 are also capped by the health lane)" },
    Class { id: ANALYTICS, store: "telemetry.db", scope: "analytics_revisions and their analytics_lineage and analytics_workspace_metrics (superseded revisions only), analytics_workspace_comparisons (latest kept)", default_days: Some(365), basis: "derivable",
        destructive: false, action: Action::Prune, age_from: "recorded_unix_ms", requires: "a later revision of the same cell exists; comparison rows are outside the window and not latest" },
    Class { id: "sidecar.derived_projections", store: "telemetry.db", scope: "usage_ledger, model_segments, session_graph_nodes, quota_windows, proxy_signals, integration_outcomes, quality_verification_runs, quality_test_results, quality_verification_observations, quality_observation_tests, quality_verification_cursor, policy_shadow_decisions, health_rule_states, health_alerts, analytics_cells, analytics_workspace_metrics, analytics_workspace_comparisons, source_bindings",
        default_days: None, basis: "derivable", destructive: false, action: Action::FollowsSources, age_from: "-", requires: "rebuilt by collect, sync and lane ticks from surviving sources" },
    Class { id: "sidecar.accounting_imports", store: "telemetry.db", scope: "rate_cards, rate_card_models, rate_card_rates, provider_charges, provider_invoices, fx_tables, fx_rates",
        default_days: None, basis: "source_of_truth", destructive: true, action: Action::Retain, age_from: "-", requires: "kept; re-import needs the original files (certificate-core R4)" },
    Class { id: "sidecar.valuation_history", store: "telemetry.db", scope: "valuation_revisions, valuations, valuation_bases, valuation_deltas, valuation_delta_revisions, valuation_inputs",
        default_days: None, basis: "source_of_truth", destructive: true, action: Action::Retain, age_from: "-", requires: "kept; as-of views need it (certificate-core R4); entries whose sessions expired stay as references" },
    Class { id: TOMBSTONES, store: "telemetry-ops.db", scope: "tombstones", default_days: Some(400), basis: "source_of_truth", destructive: true, action: Action::Listed,
        age_from: "recorded_unix_ms", requires: "listed when expired, never pruned by retention.v1 (they prevent resurrection)" },
    Class { id: "ops.audit", store: "telemetry-ops.db", scope: "holds, maintenance_runs, backups inventory, restores", default_days: None, basis: "source_of_truth", destructive: true,
        action: Action::Retain, age_from: "-", requires: "kept" },
    Class { id: QUARANTINE, store: "<project>/.git-quarantine/<attempt>", scope: "the attempt's Git quarantines", default_days: Some(7), basis: "source_of_truth", destructive: true,
        action: Action::Prune, age_from: "the attempt's terminal lifecycle mark", requires: "attempt terminal with termination observed; an import verdict (import.json) beside every quarantine" },
    Class { id: SPOOL, store: "<project>/.state/spool/<attempt>", scope: "the attempt's submission spool", default_days: Some(7), basis: "source_of_truth", destructive: true,
        action: Action::Prune, age_from: "the attempt's terminal lifecycle mark", requires: "attempt terminal with termination observed; every request has its receipt (disposition)" },
    Class { id: REPLAY, store: "<root>/.replay/<slug>/repos/<suite>/<seq>/<case>", scope: "one replay candidate's repository", default_days: Some(30), basis: "source_of_truth",
        destructive: true, action: Action::Prune, age_from: "the replay run's record", requires: "candidate task terminal with no live attempt; path inside the replay root" },
    Class { id: BACKUPS, store: "backup directories in the inventory", scope: "a sidecar backup (manifest and files)", default_days: Some(30), basis: "source_of_truth",
        destructive: true, action: Action::Prune, age_from: "created_unix_ms", requires: "the directory's manifest still has the inventoried digest" },
    Class { id: EXPORTS, store: "the enabled external export directory", scope: "<slug>-<id>-<offset>.json|csv (+ .manifest.json)", default_days: Some(7),
        basis: "source_of_truth", destructive: true, action: Action::Prune, age_from: "file modification time", requires: "external export enabled with a directory destination; a project override may only shorten it" },
    Class { id: "optin.captured_evidence", store: "-", scope: "opt-in content capture", default_days: Some(7), basis: "source_of_truth", destructive: true, action: Action::NotBuilt,
        age_from: "-", requires: "no capture store exists (contracts §7)" },
    Class { id: "canonical.state", store: "<project>/.state/state.db", scope: "workflow history, dispatch decisions, accepted usage/budget and quality evidence including verification run load/test metadata, seeded-defect and replay registries",
        default_days: None, basis: "canonical", destructive: true, action: Action::External, age_from: "-", requires: "canonical lifecycle; telemetry never writes or deletes it" },
    Class { id: "canonical.verification_execution_slots", store: "<project>/.state/verification-load.lock", scope: "ephemeral OFD byte locks (no data)",
        default_days: None, basis: "derivable", destructive: false, action: Action::External, age_from: "-", requires: "verifier owned; never back up live locks; the empty lock file is recreated" },
    Class { id: "native.codex_rollouts", store: "<execution_home>/.codex/sessions", scope: "Codex rollout files", default_days: None, basis: "source_of_truth", destructive: true,
        action: Action::External, age_from: "-", requires: "owned by Codex; their availability is the replay horizon" },
    Class { id: "secret.cursor_key", store: "<config_dir>/telemetry-cursor.key", scope: "drill-down cursor key", default_days: None, basis: "source_of_truth", destructive: true,
        action: Action::External, age_from: "-", requires: "a secret: never backed up with telemetry; deleting it revokes every cursor" },
];

fn class(id: &str) -> Option<&'static Class> { CLASSES.iter().find(|c| c.id == id) }

/// Tables deleted per session, by the column that names it (`retention.v1`
/// deletion scope). A sidecar table with a `session_id` or `path_digest`
/// column outside these lists refuses `apply`: it would escape deletion.
const BY_PATH: &[&str] = &["collect_offsets", "rollout_sources", "rollout_metadata", "rollout_threads", "rollout_subagents", "rollout_ingest_state", "rollout_forks",
    "rollout_turn_ends", "rollout_turn_terminations", "codex_tool_sources", "source_bindings", "session_graph_nodes", "usage_dispositions", "accounting_source_summary"];
const BY_SESSION: &[&str] = &["codex_usage", "codex_usage_times", "codex_quarantine", "codex_discrepancy", "codex_rate_limits", "codex_rate_limit_windows", "codex_turns",
    "claude_messages", "claude_tool_results", "opencode_messages", "opencode_tools", "codex_tool_calls", "codex_tool_namespaces", "codex_exec_items", "codex_mcp_calls", "codex_agent_items", "codex_turn_aborts", "codex_fork_reconciliation", "usage_entries",
    "model_segments", "quota_window_observations", "session_graph_nodes", "accounting_dirty_sessions", "accounting_usage_totals", "accounting_native_totals", "accounting_source_summary", "accounting_tool_summary"];
/// `(table, column)` holding the source's path digest.
const BY_SOURCE: &[(&str, &str)] = &[("source_observations", "producer_epoch"), ("ingest_quarantine", "source"), ("coverage_gaps", "source"), ("source_cursors", "source")];
/// Priced history that keeps a session id as a reference (R4).
const REFERENCES: &[&str] = &["valuations", "valuation_deltas", "valuation_bases", "provider_charges"];

const ANALYTICS_TRIGGERS: &str = "
CREATE TRIGGER IF NOT EXISTS analytics_revisions_no_delete BEFORE DELETE ON analytics_revisions
BEGIN SELECT RAISE(ABORT, 'analytics revision is immutable'); END;
CREATE TRIGGER IF NOT EXISTS analytics_lineage_no_delete BEFORE DELETE ON analytics_lineage
BEGIN SELECT RAISE(ABORT, 'analytics lineage is immutable'); END;";

/// `herdr-projects telemetry <slug> maintenance ...`
#[derive(clap::Subcommand, Clone, Debug)]
pub enum Command {
    /// The declared retention classes (`retention.v1`) with this deployment's overrides. Read-only.
    Classes { #[arg(long)] json: bool },
    /// What retention would delete now: eligible, blocked and held items, quotas, and the plan digest. Read-only.
    Plan {
        /// Evaluate ages as of this Unix ms instead of now (what-if; `apply` always uses now).
        #[arg(long)]
        now: Option<i64>,
        /// Also delete this Codex session regardless of age (operator deletion; holds and accounting still apply).
        #[arg(long = "forget-session")]
        forget: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Delete what the plan lists, as the owner, after writing tombstones. Destructive items need --confirm <plan digest>.
    Apply {
        #[arg(long)]
        confirm: Option<String>,
        #[arg(long = "dry-run")]
        dry_run: bool,
        #[arg(long = "forget-session")]
        forget: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Legal or administrative holds: they block deletion of their class (or `all`) and scope.
    Hold { #[command(subcommand)] command: HoldCommand },
}

#[derive(clap::Subcommand, Clone, Debug)]
pub enum HoldCommand {
    /// Place a hold on CLASS (a retention class id or `all`), optionally one key (session, attempt, task or backup id).
    Add { #[arg(long)] class: String, #[arg(long)] scope: Option<String>, #[arg(long)] reason: String },
    /// Release an active hold.
    Release { hold_id: String, #[arg(long)] reason: String },
    /// Every hold, active and released. Read-only.
    List,
}

pub fn run(project: &Path, config_dir: &Path, command: Command) -> Result<String> {
    match command {
        Command::Classes { json } => {
            let value = classes(config_dir, &super::health::store::slug(project))?;
            Ok(if json { pretty(&value) } else { classes_text(&value) })
        }
        Command::Plan { now, forget, json } => {
            let plan = plan(project, config_dir, now.unwrap_or_else(store::now), &forget)?;
            Ok(if json { pretty(&plan.json) } else { plan_text(&plan.json) })
        }
        Command::Apply { confirm, dry_run, forget, json } => {
            let value = apply(project, config_dir, confirm.as_deref(), dry_run, &forget)?;
            Ok(if json { pretty(&value) } else { apply_text(&value) })
        }
        Command::Hold { command } => {
            let value = match command {
                HoldCommand::Add { class: id, scope, reason } => {
                    super::review::refuse_owner_cli_in_worker_context(project, "`maintenance hold`")?;
                    ensure!(id == "all" || class(&id).is_some(), "unknown retention class {id} (see `maintenance classes`)");
                    store::add_hold(project, &id, scope.as_deref(), &reason)?
                }
                HoldCommand::Release { hold_id, reason } => {
                    super::review::refuse_owner_cli_in_worker_context(project, "`maintenance hold`")?;
                    store::release_hold(project, &hold_id, &reason)?
                }
                HoldCommand::List => json!({"holds": store::holds_of(project)?.iter().map(store::Hold::json).collect::<Vec<_>>()}),
            };
            Ok(pretty(&value))
        }
    }
}

fn pretty(value: &Value) -> String { serde_json::to_string_pretty(value).unwrap_or_default() + "\n" }

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyFile { schema: String, #[serde(default)] days: BTreeMap<String, i64>, #[serde(default)] projects: BTreeMap<String, ProjectPolicy> }

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectPolicy { #[serde(default)] days: BTreeMap<String, i64> }

/// Retention days per prunable class: `(days, "default" | "override" | "project_override")`.
fn policy(config_dir: &Path, slug: &str) -> Result<BTreeMap<&'static str, (i64, &'static str)>> {
    let mut out: BTreeMap<&'static str, (i64, &'static str)> = CLASSES.iter().filter_map(|c| Some((c.id, (c.default_days?, "default")))).collect();
    let path = config_dir.join(POLICY_FILE);
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(error) => return Err(error).context("read the retention policy"),
    };
    ensure!(meta.file_type().is_file() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o022 == 0 && meta.len() <= 16 * 1024,
        "{POLICY_FILE} must be a regular file owned by this user, not group/world writable, at most 16 KiB");
    let file: PolicyFile = toml::from_str(&std::fs::read_to_string(&path)?).map_err(|e| anyhow::anyhow!("invalid {POLICY_FILE}: {e}"))?;
    ensure!(file.schema == POLICY_SCHEMA, "{POLICY_FILE} schema must be {POLICY_SCHEMA}");
    let project = file.projects.get(slug).map(|p| p.days.clone()).unwrap_or_default();
    for (days, source) in [(&file.days, "override"), (&project, "project_override")] {
        for (id, &value) in days {
            let class = class(id).with_context(|| format!("{POLICY_FILE}: unknown retention class {id}"))?;
            let default = class.default_days.with_context(|| format!("{POLICY_FILE}: {id} has no retention duration"))?;
            ensure!((0..=3650).contains(&value), "{POLICY_FILE}: {id} days must be 0 to 3650");
            ensure!(class.id != TOMBSTONES || value >= default, "{POLICY_FILE}: tombstones are kept at least {default} days");
            ensure!(!class.id.starts_with("optin.") || value <= default, "{POLICY_FILE}: {id} may only be shortened (default {default} days)");
            out.insert(class.id, (value, source));
        }
    }
    Ok(out)
}

fn classes(config_dir: &Path, slug: &str) -> Result<Value> {
    let policy = policy(config_dir, slug)?;
    Ok(json!({"policy": POLICY, "classes": CLASSES.iter().map(|c| {
        let (days, source) = policy.get(c.id).map_or((None, None), |(d, s)| (Some(*d), Some(*s)));
        json!({"class": c.id, "store": c.store, "scope": c.scope, "default_days": c.default_days, "retention_days": days, "policy_source": source,
            "basis": c.basis, "destructive": c.destructive, "action": c.action.name(), "age_from": c.age_from, "requires": c.requires})
    }).collect::<Vec<_>>()}))
}

fn classes_text(value: &Value) -> String {
    let mut out = format!("{}\n", value["policy"].as_str().unwrap_or_default());
    for c in value["classes"].as_array().into_iter().flatten() {
        let days = c["retention_days"].as_i64().map_or("-".to_owned(), |d| format!("{d}d"));
        out += &format!("{} {} {} {}{}\n", c["class"].as_str().unwrap_or_default(), c["action"].as_str().unwrap_or_default(), days,
            c["basis"].as_str().unwrap_or_default(), if c["destructive"] == true { " destructive" } else { "" });
    }
    out
}

/// One deletable item: its class, display key, tombstone keys and what to remove.
#[derive(Clone, Debug)]
pub struct Item {
    pub class: &'static str,
    pub key: String,
    tombs: Vec<(Option<String>, Option<i64>)>,
    target: Target,
    detail: Value,
    reason: &'static str,
}

#[derive(Clone, Debug)]
enum Target {
    Session { id: String, paths: Vec<String> },
    Attention { attempt: String, before: i64 },
    Health { before: i64 },
    Revision { revision: i64 },
    Comparison { revision: i64 },
    Tree(PathBuf),
    Backup { id: String, location: PathBuf, files: Vec<String>, missing: bool },
    File(Vec<PathBuf>),
}

pub struct Plan {
    pub json: Value,
    pub items: Vec<Item>,
    pub digest: String,
    pub destructive: usize,
}

struct Canonical(Option<super::ReadOnly>);

impl Canonical {
    fn open(project: &Path) -> Result<Self> {
        let path = project.join(".state/state.db");
        Ok(Self(if path.is_file() { Some(super::read_only(&path)?) } else { None }))
    }
    fn has(&self, table: &str) -> Result<bool> {
        let Some(db) = &self.0 else { return Ok(false) };
        Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)", [table], |r| r.get(0))?)
    }
    /// `(state, termination_observed, terminal mark unix ms)`.
    fn attempt(&self, id: &str) -> Result<Option<(String, bool, Option<i64>)>> {
        let Some(db) = &self.0 else { return Ok(None) };
        let Some((state, observed)) = db.query_row("SELECT state,termination_observed FROM attempts WHERE id=?1", [id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?))).optional()?
        else { return Ok(None) };
        let mark = if self.has("attempt_lifecycle")? {
            db.query_row("SELECT max(unix_ms) FROM attempt_lifecycle WHERE attempt_id=?1 AND state IN ('completed','failed','cancelled','lost')", [id], |r| r.get(0))?
        } else { None };
        Ok(Some((state, observed, mark)))
    }
}

struct Ctx<'a> {
    project: &'a Path,
    now: i64,
    policy: BTreeMap<&'static str, (i64, &'static str)>,
    holds: Vec<store::Hold>,
    tombstones: Tombstones,
    canonical: Canonical,
    config_dir: &'a Path,
}

#[derive(Default)]
struct Found { eligible: Vec<Item>, blocked: Vec<Value>, held: Vec<Value> }

impl Found {
    fn block(&mut self, key: String, reason: &str) { self.blocked.push(json!({"key": key, "reason": reason})); }
    /// Eligible unless an active hold covers one of `scopes`.
    fn offer(&mut self, ctx: &Ctx, item: Item, scopes: &[&str]) {
        match ctx.holds.iter().find(|h| h.covers(item.class, scopes, &ctx.tombstones.salts)) {
            Some(hold) => self.held.push(json!({"key": item.key, "hold_id": hold.id})),
            None => self.eligible.push(item),
        }
    }
}

impl Ctx<'_> {
    fn cutoff(&self, class: &str) -> i64 { self.now - self.policy.get(class).map_or(i64::MAX / 4, |(days, _)| days * DAY_MS) }
}

/// The retention plan at `now`. Read-only: creates and writes nothing.
pub fn plan(project: &Path, config_dir: &Path, now: i64, forget: &[String]) -> Result<Plan> {
    let slug = super::health::store::slug(project);
    let ops = store::read(project)?;
    let (holds, tombstones) = match &ops { Some(db) => (store::holds(db)?, Tombstones::load(db)?), None => (Vec::new(), Tombstones::default()) };
    let ctx = Ctx { project, now, policy: policy(config_dir, &slug)?, holds, tombstones, canonical: Canonical::open(project)?, config_dir };
    let sidecar = super::sidecar::read(project)?;
    let mut classes = Vec::new();
    let mut items = Vec::new();
    for class in CLASSES.iter().filter(|c| c.action == Action::Prune) {
        let found = match (class.id, &sidecar) {
            (SESSIONS, Some(db)) => sessions(&ctx, db, forget)?,
            (ATTENTION, Some(db)) => attention(&ctx, db)?,
            (HEALTH, Some(db)) => health(&ctx, db)?,
            (ANALYTICS, Some(db)) => analytics(&ctx, db)?,
            (QUARANTINE, _) => quarantines(&ctx)?,
            (SPOOL, _) => spools(&ctx)?,
            (REPLAY, _) => replay_repos(&ctx)?,
            (BACKUPS, _) => match &ops { Some(db) => backups(&ctx, db)?, None => Found::default() },
            (EXPORTS, _) => exports(&ctx, &slug)?,
            _ => Found::default(),
        };
        let (days, source) = ctx.policy[class.id];
        let listed = |items: &[Item]| items.iter().take(LISTED).map(|i| { let mut d = i.detail.clone(); d["key"] = json!(i.key); d }).collect::<Vec<_>>();
        classes.push(json!({"class": class.id, "retention_days": days, "policy_source": source, "destructive": class.destructive,
            "cutoff_unix_ms": ctx.cutoff(class.id), "eligible_count": found.eligible.len(), "eligible": listed(&found.eligible),
            "blocked": found.blocked.iter().take(LISTED).collect::<Vec<_>>(), "blocked_count": found.blocked.len(),
            "held": found.held.iter().take(LISTED).collect::<Vec<_>>()}));
        items.extend(found.eligible);
    }
    let keys: Vec<(&str, &str)> = items.iter().map(|i| (i.class, i.key.as_str())).collect();
    let digest = store::sha256(json!({"policy": POLICY, "project": slug, "items": keys}).to_string().as_bytes());
    let destructive = items.iter().filter(|i| class(i.class).is_some_and(|c| c.destructive)).count();
    let tombstone_state = match &ops {
        Some(db) => json!({"count": ctx.tombstones.count, "by_class": store::tombstone_counts(db)?, "expired_listed": store::expired_tombstones(db, ctx.cutoff(TOMBSTONES))?}),
        None => json!({"count": 0, "by_class": {}, "expired_listed": 0}),
    };
    let json = json!({"policy": POLICY, "project": slug, "now_unix_ms": now, "plan_digest": digest, "items": items.len(), "destructive_items": destructive,
        "classes": classes, "quotas": quotas(project, &ctx)?, "holds": ctx.holds.iter().filter(|h| h.released.is_none()).map(store::Hold::json).collect::<Vec<_>>(),
        "tombstones": tombstone_state, "canonical_written": false});
    Ok(Plan { json, items, digest, destructive })
}

fn sessions(ctx: &Ctx, db: &Connection, forget: &[String]) -> Result<Found> {
    let mut found = Found::default();
    let cutoff = ctx.cutoff(SESSIONS);
    let rows: Vec<(String, String, Option<String>, i64)> = db.prepare("SELECT session_id,path_digest,attempt_id,observed_unix_ms FROM rollout_sources ORDER BY session_id,path_digest")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut by: BTreeMap<String, (Vec<String>, BTreeSet<String>, i64)> = BTreeMap::new();
    for (session, path, attempt, observed) in rows {
        let slot = by.entry(session).or_insert((Vec::new(), BTreeSet::new(), i64::MIN));
        slot.0.push(path);
        slot.1.extend(attempt);
        slot.2 = slot.2.max(observed);
    }
    let synced: Option<i64> = if table(db, "usage_ledger")? { db.query_row("SELECT synced_unix_ms FROM usage_ledger WHERE singleton=1", [], |r| r.get(0)).optional()? } else { None };
    for (session, (paths, attempts, observed)) in by {
        let forgotten = forget.contains(&session);
        if observed >= cutoff && !forgotten { continue; }
        let key = format!("session:{session}");
        let accepted: i64 = db.query_row("SELECT count(*) FROM codex_usage WHERE session_id=?1 AND accepted=1", [&session], |r| r.get(0))?;
        let records: i64 = db.query_row("SELECT count(*) FROM codex_usage WHERE session_id=?1", [&session], |r| r.get(0))?;
        let quarantined: i64 = db.query_row("SELECT count(*) FROM codex_quarantine WHERE session_id=?1", [&session], |r| r.get(0))?;
        let mut blocked = None;
        for attempt in &attempts {
            match ctx.canonical.attempt(attempt)? {
                Some((state, _, _)) if LIVE.contains(&state.as_str()) => blocked = Some("attempt_not_terminal"),
                _ => {}
            }
        }
        if blocked.is_none() && quarantined > 0 { blocked = Some("quarantine_unresolved"); }
        if blocked.is_none() && accepted > 0 {
            if synced.is_none_or(|s| s < observed) { blocked = Some("ledger_not_synced"); } else {
                let pending: i64 = db.query_row(&format!("SELECT count(*) FROM usage_dispositions WHERE disposition IN ('unresolved','conflict') AND path_digest IN ({})",
                    paths.iter().map(|p| format!("'{}'", p.replace('\'', "''"))).collect::<Vec<_>>().join(",")), [], |r| r.get(0))?;
                if pending > 0 { blocked = Some("accounting_disposition_pending"); }
            }
        }
        if let Some(reason) = blocked { found.block(key, reason); continue; }
        let mut tombs = vec![(Some(key.clone()), None)];
        tombs.extend(paths.iter().map(|p| (Some(format!("path:{p}")), None)));
        let item = Item { class: SESSIONS, key, tombs, target: Target::Session { id: session.clone(), paths }, reason: if forgotten { "operator_deletion" } else { "retention_expired" },
            detail: json!({"records": records, "age_days": (ctx.now - observed).div_euclid(DAY_MS), "reason": if forgotten { "operator_deletion" } else { "retention_expired" }}) };
        let scopes: Vec<&str> = std::iter::once(session.as_str()).chain(attempts.iter().map(String::as_str)).collect();
        found.offer(ctx, item, &scopes);
    }
    Ok(found)
}

fn attention(ctx: &Ctx, db: &Connection) -> Result<Found> {
    let mut found = Found::default();
    if !table(db, "attention_samples")? { return Ok(found); }
    let before = ctx.cutoff(ATTENTION);
    let rows: Vec<(String, i64)> = db.prepare("SELECT attempt_id,count(*) FROM attention_samples WHERE observed_unix_ms<?1 GROUP BY attempt_id ORDER BY attempt_id")?
        .query_map([before], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    for (attempt, samples) in rows {
        let key = format!("attempt:{attempt}");
        if let Some((state, _, _)) = ctx.canonical.attempt(&attempt)? && LIVE.contains(&state.as_str()) { found.block(key, "attempt_not_terminal"); continue; }
        let item = Item { class: ATTENTION, key: key.clone(), tombs: vec![(Some(key), Some(before))], target: Target::Attention { attempt: attempt.clone(), before },
            detail: json!({"samples": samples}), reason: "retention_expired" };
        found.offer(ctx, item, &[attempt.as_str()]);
    }
    Ok(found)
}

fn health(ctx: &Ctx, db: &Connection) -> Result<Found> {
    let mut found = Found::default();
    if !table(db, "health_evaluations")? { return Ok(found); }
    let before = ctx.cutoff(HEALTH);
    let rows: i64 = db.query_row("SELECT count(*) FROM health_evaluations WHERE evaluated_unix_ms<?1", [before], |r| r.get(0))?;
    if rows > 0 {
        let item = Item { class: HEALTH, key: "evaluations_before_cutoff".into(), tombs: vec![(None, Some(before))], target: Target::Health { before },
            detail: json!({"evaluations": rows}), reason: "retention_expired" };
        found.offer(ctx, item, &[]);
    }
    Ok(found)
}

fn analytics(ctx: &Ctx, db: &Connection) -> Result<Found> {
    let mut found = Found::default();
    if !table(db, "analytics_revisions")? { return Ok(found); }
    let rows: Vec<(i64, String)> = db.prepare("SELECT r.revision,r.content_digest FROM analytics_revisions r WHERE r.recorded_unix_ms<?1
        AND EXISTS(SELECT 1 FROM analytics_revisions n WHERE n.cell=r.cell AND n.revision>r.revision) ORDER BY r.revision")?
        .query_map([ctx.cutoff(ANALYTICS)], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    for (revision, digest) in rows {
        let item = Item { class: ANALYTICS, key: format!("revision:{revision}"), tombs: vec![(Some(format!("revision:{revision}:{digest}")), None)],
            target: Target::Revision { revision }, detail: json!({"content_digest": digest}), reason: "retention_expired" };
        found.offer(ctx, item, &[]);
    }
    if table(db, "analytics_workspace_comparisons")? {
        let rows: Vec<(i64, i64)> = db.prepare("SELECT revision,recorded_unix_ms FROM analytics_workspace_comparisons WHERE recorded_unix_ms<?1 AND revision<(SELECT max(revision) FROM analytics_workspace_comparisons) ORDER BY revision")?
            .query_map([ctx.cutoff(ANALYTICS)], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        for (revision, at) in rows {
            found.offer(ctx, Item { class: ANALYTICS, key: format!("comparison:{revision}"),
                tombs: vec![(Some(format!("comparison:{revision}:{at}")), None)], target: Target::Comparison { revision },
                detail: json!({"recorded_unix_ms": at}), reason: "retention_expired" }, &[]);
        }
    }
    Ok(found)
}

/// Entries of a directory that is not a link, sorted; none when absent.
fn entries(dir: &Path) -> Result<Vec<(String, std::fs::Metadata)>> {
    match std::fs::symlink_metadata(dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
        Ok(meta) => ensure!(meta.is_dir(), "{} is not a directory", dir.display()),
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if let Some(name) = entry.file_name().to_str() { out.push((name.to_owned(), std::fs::symlink_metadata(entry.path())?)); }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Bytes and files under `path` without following links.
fn usage(path: &Path) -> (u64, u64) {
    let Ok(meta) = std::fs::symlink_metadata(path) else { return (0, 0) };
    if !meta.is_dir() { return (meta.len(), 1); }
    let Ok(dir) = std::fs::read_dir(path) else { return (0, 0) };
    dir.flatten().map(|e| usage(&e.path())).fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1))
}

/// Terminal-attempt gate shared by quarantines and spools; `Err(reason)` blocks.
fn ended(ctx: &Ctx, attempt: &str, class: &str) -> Result<std::result::Result<i64, &'static str>> {
    Ok(match ctx.canonical.attempt(attempt)? {
        None => Err("attempt_unknown"),
        Some((state, _, _)) if LIVE.contains(&state.as_str()) => Err("attempt_not_terminal"),
        Some((_, false, _)) => Err("termination_not_observed"),
        Some((_, true, None)) => Err("terminal_time_unknown"),
        Some((_, true, Some(at))) if at >= ctx.cutoff(class) => Err("retention_not_reached"),
        Some((_, true, Some(at))) => Ok(at),
    })
}

fn quarantines(ctx: &Ctx) -> Result<Found> {
    let mut found = Found::default();
    let root = ctx.project.join(crate::worker_supervision::GIT_QUARANTINE_DIR);
    for (attempt, meta) in entries(&root)? {
        let key = format!("attempt:{attempt}");
        if !meta.is_dir() { found.block(key, "not_a_directory"); continue; }
        let at = match ended(ctx, &attempt, QUARANTINE)? { Ok(at) => at, Err("retention_not_reached") => continue, Err(reason) => { found.block(key, reason); continue } };
        let dir = root.join(&attempt);
        // Every quarantine (a directory holding `upper/`) needs its import verdict.
        let mut pending = 0;
        let mut stack = vec![dir.clone()];
        while let Some(d) = stack.pop() {
            if d.join("upper").is_dir() && !d.join("import.json").is_file() { pending += 1; continue; }
            if d.join("upper").is_dir() { continue; }
            for (name, meta) in entries(&d).unwrap_or_default() { if meta.is_dir() { stack.push(d.join(name)); } }
        }
        if pending > 0 { found.block(key, "import_verdict_missing"); continue; }
        let (bytes, files) = usage(&dir);
        let item = Item { class: QUARANTINE, key: key.clone(), tombs: vec![(Some(key), None)], target: Target::Tree(dir), reason: "retention_expired",
            detail: json!({"bytes": bytes, "files": files, "age_days": (ctx.now - at).div_euclid(DAY_MS)}) };
        found.offer(ctx, item, &[attempt.as_str()]);
    }
    Ok(found)
}

fn spools(ctx: &Ctx) -> Result<Found> {
    let mut found = Found::default();
    let root = ctx.project.join(".state/spool");
    for (attempt, meta) in entries(&root)? {
        let key = format!("attempt:{attempt}");
        if !meta.is_dir() { found.block(key, "not_a_directory"); continue; }
        let dir = root.join(&attempt);
        let names: BTreeSet<String> = entries(&dir)?.into_iter().map(|(n, _)| n).collect();
        let undisposed = names.iter().filter_map(|n| n.strip_suffix(".request")).filter(|d| !names.contains(&format!("{d}.receipt"))).count();
        let at = match ended(ctx, &attempt, SPOOL)? { Ok(at) => at, Err("retention_not_reached") => continue, Err(reason) => { found.block(key, reason); continue } };
        // A request without a receipt has no disposition yet: the ticker answers or denies it first.
        if undisposed > 0 { found.blocked.push(json!({"key": key, "reason": "request_without_receipt", "requests": undisposed})); continue; }
        let (bytes, files) = usage(&dir);
        let item = Item { class: SPOOL, key: key.clone(), tombs: vec![(Some(key), None)], target: Target::Tree(dir), reason: "retention_expired",
            detail: json!({"bytes": bytes, "files": files, "age_days": (ctx.now - at).div_euclid(DAY_MS)}) };
        found.offer(ctx, item, &[attempt.as_str()]);
    }
    Ok(found)
}

fn replay_repos(ctx: &Ctx) -> Result<Found> {
    let mut found = Found::default();
    if !ctx.canonical.has("replay_candidates")? { return Ok(found); }
    let db = ctx.canonical.0.as_ref().expect("checked by has");
    let Some(root) = ctx.project.parent() else { return Ok(found) };
    let repos = root.join(".replay").join(super::health::store::slug(ctx.project)).join("repos");
    let rows: Vec<(String, String, String, i64)> = db.prepare("SELECT c.task_id,c.repository,t.state,l.recorded_unix_ms FROM replay_candidates c JOIN tasks t ON t.id=c.task_id
        JOIN replay_runs r ON r.run_id=c.run_id JOIN replay_log l ON l.seq=r.seq ORDER BY c.task_id")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
    for (task, repository, state, recorded) in rows {
        let path = PathBuf::from(&repository);
        if std::fs::symlink_metadata(&path).is_err() || recorded >= ctx.cutoff(REPLAY) { continue; }
        let key = format!("task:{task}");
        let inside = path.is_absolute() && path.components().all(|c| !matches!(c, std::path::Component::ParentDir)) && path.starts_with(&repos) && path != repos;
        if !inside { found.block(key, "outside_replay_root"); continue; }
        if !TERMINAL_TASK.contains(&state.as_str()) { found.block(key, "task_not_terminal"); continue; }
        let live: i64 = db.query_row(&format!("SELECT count(*) FROM attempts WHERE task_id=?1 AND state IN ({})", LIVE.map(|s| format!("'{s}'")).join(",")), [&task], |r| r.get(0))?;
        if live > 0 { found.block(key, "attempt_live"); continue; }
        let (bytes, files) = usage(&path);
        let item = Item { class: REPLAY, key: key.clone(), tombs: vec![(Some(key), None)], target: Target::Tree(path), reason: "retention_expired",
            detail: json!({"bytes": bytes, "files": files, "age_days": (ctx.now - recorded).div_euclid(DAY_MS)}) };
        found.offer(ctx, item, &[task.as_str()]);
    }
    Ok(found)
}

fn backups(ctx: &Ctx, db: &Connection) -> Result<Found> {
    let mut found = Found::default();
    for (id, location, manifest_digest, created) in store::backups(db)? {
        if created >= ctx.cutoff(BACKUPS) { continue; }
        let key = format!("backup:{id}");
        let location = PathBuf::from(location);
        let manifest = location.join(backup::MANIFEST);
        let (files, missing) = match std::fs::read(&manifest) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (Vec::new(), true),
            Err(error) => return Err(error.into()),
            Ok(bytes) => {
                if store::sha256(&bytes) != manifest_digest { found.block(key, "manifest_changed"); continue; }
                let value: Value = serde_json::from_slice(&bytes)?;
                (value["files"].as_array().into_iter().flatten().filter_map(|f| f["name"].as_str().map(str::to_owned)).collect(), false)
            }
        };
        let item = Item { class: BACKUPS, key: key.clone(), tombs: vec![(Some(key), None)], target: Target::Backup { id: id.clone(), location, files, missing },
            detail: json!({"age_days": (ctx.now - created).div_euclid(DAY_MS), "location_present": !missing}), reason: "retention_expired" };
        found.offer(ctx, item, &[id.as_str()]);
    }
    Ok(found)
}

/// `<slug>-<16 hex>-<offset>.json|csv`, and a CSV page's `.manifest.json` companion.
fn export_name<'a>(name: &'a str, slug: &str) -> Option<&'a str> {
    let rest = name.strip_prefix(slug)?.strip_prefix('-')?;
    let base = rest.strip_suffix(".manifest.json").unwrap_or(rest);
    let (stem, ext) = base.rsplit_once('.')?;
    let (id, offset) = stem.split_once('-')?;
    (matches!(ext, "json" | "csv") && id.len() == 16 && id.bytes().all(|b| b.is_ascii_hexdigit()) && !offset.is_empty() && offset.bytes().all(|b| b.is_ascii_digit()))
        .then(|| name.strip_suffix(".manifest.json").unwrap_or(name))
}

fn exports(ctx: &Ctx, slug: &str) -> Result<Found> {
    let mut found = Found::default();
    let Ok(super::export::external::Destination::Directory(dir)) = super::export::external::load(ctx.config_dir)? else { return Ok(found) };
    let mut groups: BTreeMap<String, (Vec<PathBuf>, i64)> = BTreeMap::new();
    for (name, meta) in entries(&dir)? {
        let Some(page) = export_name(&name, slug) else { continue };
        if !meta.is_file() || meta.uid() != unsafe { libc::geteuid() } { continue; }
        let slot = groups.entry(page.to_owned()).or_insert((Vec::new(), i64::MIN));
        slot.0.push(dir.join(&name));
        slot.1 = slot.1.max(meta.mtime() * 1000 + meta.mtime_nsec() / 1_000_000);
    }
    for (page, (files, modified)) in groups {
        if modified >= ctx.cutoff(EXPORTS) { continue; }
        let key = format!("file:{page}");
        let item = Item { class: EXPORTS, key: key.clone(), tombs: vec![(Some(key), None)], detail: json!({"files": files.len(), "age_days": (ctx.now - modified).div_euclid(DAY_MS)}),
            target: Target::File(files), reason: "retention_expired" };
        found.offer(ctx, item, &[page.as_str()]);
    }
    Ok(found)
}

fn quotas(project: &Path, ctx: &Ctx) -> Result<Value> {
    let size = |p: PathBuf| std::fs::symlink_metadata(p).map_or(0, |m| m.len());
    let sidecar = super::sidecar::path(project);
    let wal = PathBuf::from(format!("{}-wal", sidecar.display()));
    let spool = entries(&project.join(".state/spool"))?.into_iter().filter(|(_, m)| m.is_dir()).map(|(attempt, _)| {
        let dir = project.join(".state/spool").join(&attempt);
        let n = entries(&dir).map_or(0, |e| e.len() as u64);
        json!({"attempt": attempt, "entries": n, "bytes": usage(&dir).0, "limit_entries": SPOOL_ENTRIES, "state": if n > SPOOL_ENTRIES { "over_limit" } else { "ok" }})
    }).collect::<Vec<_>>();
    let quarantine = entries(&project.join(crate::worker_supervision::GIT_QUARANTINE_DIR))?.into_iter().filter(|(_, m)| m.is_dir()).map(|(attempt, _)| {
        let bytes = usage(&project.join(crate::worker_supervision::GIT_QUARANTINE_DIR).join(&attempt)).0;
        json!({"attempt": attempt, "bytes": bytes, "limit_bytes": QUARANTINE_BYTES, "state": if bytes > QUARANTINE_BYTES { "over_limit" } else { "ok" }})
    }).collect::<Vec<_>>();
    let backups: u64 = match store::read(project)? {
        Some(db) => store::backups(&db)?.iter().map(|(_, location, _, _)| usage(Path::new(location)).0).sum(),
        None => 0,
    };
    let _ = ctx;
    Ok(json!({"telemetry_db_bytes": size(sidecar.clone()), "telemetry_wal_bytes": size(wal), "ops_db_bytes": size(store::path(project)),
        "spool": spool, "git_quarantine": quarantine, "backup_bytes": backups}))
}

fn table(db: &Connection, name: &str) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)", [name], |r| r.get(0))?)
}

/// Refuse a sidecar holding a per-session table outside the declared deletion scope.
fn declared(db: &Connection) -> Result<()> {
    let tables: Vec<String> = db.prepare("SELECT DISTINCT m.name FROM sqlite_master m, pragma_table_info(m.name) p WHERE m.type='table' AND p.name IN ('session_id','path_digest')")?
        .query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    for name in tables {
        ensure!(BY_PATH.contains(&name.as_str()) || BY_SESSION.contains(&name.as_str()) || REFERENCES.contains(&name.as_str()),
            "sidecar table {name} holds per-session rows outside the declared deletion scope of {POLICY}; refusing maintenance");
    }
    Ok(())
}

/// Delete every row of one Codex session (and its sources) in `tx`.
pub fn purge_session(tx: &Connection, session: &str, paths: &[String]) -> Result<usize> {
    let mut rows = 0;
    for name in BY_SESSION { if table(tx, name)? { rows += tx.execute(&format!("DELETE FROM {name} WHERE session_id=?1"), [session])?; } }
    for path in paths {
        for name in BY_PATH { if table(tx, name)? { rows += tx.execute(&format!("DELETE FROM {name} WHERE path_digest=?1"), [path])?; } }
        for (name, column) in BY_SOURCE { if table(tx, name)? { rows += tx.execute(&format!("DELETE FROM {name} WHERE {column}=?1"), [path])?; } }
    }
    if table(tx, "accounting_dirty_sessions")? { tx.execute("DELETE FROM accounting_dirty_sessions WHERE session_id=?1", [session])?; }
    super::accounting::ledger::invalidate(tx, "retention_enforcement")?;
    Ok(rows)
}

fn delete_revisions(tx: &Connection, revisions: &[i64]) -> Result<usize> {
    if revisions.is_empty() { return Ok(0); }
    // The only deletion path of immutable revisions: the guard triggers are
    // dropped and recreated inside this one transaction.
    tx.execute_batch("DROP TRIGGER IF EXISTS analytics_revisions_no_delete; DROP TRIGGER IF EXISTS analytics_lineage_no_delete;")?;
    let mut rows = 0;
    for revision in revisions {
        if table(tx, "analytics_workspace_metrics")? { rows += tx.execute("DELETE FROM analytics_workspace_metrics WHERE revision=?1", [revision])?; }
        rows += tx.execute("DELETE FROM analytics_lineage WHERE revision=?1", [revision])?;
        rows += tx.execute("DELETE FROM analytics_revisions WHERE revision=?1", [revision])?;
    }
    tx.execute_batch(ANALYTICS_TRIGGERS)?;
    Ok(rows)
}

/// Apply every tombstone to a sidecar connection (idempotent): no tombstoned
/// session, attention sample, evaluation or revision survives. Used after
/// `apply`, by the collector's rebuild path and on a restored copy.
pub fn enforce(db: &mut Connection, tombstones: &Tombstones) -> Result<BTreeMap<&'static str, usize>> {
    let mut out = BTreeMap::new();
    if tombstones.is_empty() { return Ok(out); }
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if table(&tx, "accounting_stream")? {
        tx.execute("UPDATE accounting_stream SET tombstones=?1,invalidated='retention_enforcement' WHERE tombstones<>?1", [tombstones.count as i64])?;
    }
    if table(&tx, "rollout_sources")? {
        let rows: Vec<(String, String)> = tx.prepare("SELECT session_id,path_digest FROM rollout_sources")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        let mut sessions: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (session, path) in rows {
            if tombstones.key(SESSIONS, &format!("session:{session}")).is_some() || tombstones.key(SESSIONS, &format!("path:{path}")).is_some() {
                sessions.entry(session).or_default().push(path);
            }
        }
        // Rows a source left without its `rollout_sources` row (an interrupted pass).
        if table(&tx, "codex_usage")? {
            let orphans: Vec<String> = tx.prepare("SELECT DISTINCT session_id FROM codex_usage WHERE session_id NOT IN (SELECT session_id FROM rollout_sources)")?
                .query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            for session in orphans { if tombstones.key(SESSIONS, &format!("session:{session}")).is_some() { sessions.entry(session).or_default(); } }
        }
        let mut n = 0;
        for (session, paths) in &sessions { purge_session(&tx, session, paths)?; n += 1; }
        if n > 0 { out.insert(SESSIONS, n); }
    }
    if table(&tx, "attention_samples")? {
        let attempts: Vec<String> = tx.prepare("SELECT DISTINCT attempt_id FROM attention_samples")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        let mut n = 0;
        for attempt in attempts {
            match tombstones.key(ATTENTION, &format!("attempt:{attempt}")) {
                Some(None) => n += tx.execute("DELETE FROM attention_samples WHERE attempt_id=?1", [&attempt])?,
                Some(Some(before)) => n += tx.execute("DELETE FROM attention_samples WHERE attempt_id=?1 AND observed_unix_ms<?2", params![attempt, before])?,
                None => {}
            }
        }
        if n > 0 { out.insert(ATTENTION, n); }
    }
    if let Some(&before) = tombstones.before.get(HEALTH) && table(&tx, "health_evaluations")? {
        let n = tx.execute("DELETE FROM health_evaluations WHERE evaluated_unix_ms<?1", [before])?;
        if n > 0 { out.insert(HEALTH, n); }
    }
    if table(&tx, "analytics_revisions")? && tombstones.keys.keys().any(|(c, _)| c == ANALYTICS) {
        let rows: Vec<(i64, String)> = tx.prepare("SELECT revision,content_digest FROM analytics_revisions")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        let doomed: Vec<i64> = rows.into_iter().filter(|(r, d)| tombstones.key(ANALYTICS, &format!("revision:{r}:{d}")).is_some()).map(|(r, _)| r).collect();
        if !doomed.is_empty() { delete_revisions(&tx, &doomed)?; out.insert(ANALYTICS, doomed.len()); }
    }
    if table(&tx, "analytics_workspace_comparisons")? {
        let rows: Vec<(i64, i64)> = tx.prepare("SELECT revision,recorded_unix_ms FROM analytics_workspace_comparisons")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        for (revision, at) in rows {
            if tombstones.key(ANALYTICS, &format!("comparison:{revision}:{at}")).is_some() {
                tx.execute("DELETE FROM analytics_workspace_comparisons WHERE revision=?1", [revision])?;
            }
        }
    }
    if !out.is_empty() { super::accounting::ledger::invalidate(&tx, "retention_enforcement")?; }
    tx.commit()?;
    Ok(out)
}

/// Remove a tree without following links (Git leaves read-only directories).
pub(crate) fn remove_tree(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(meta) if !meta.is_dir() => Ok(std::fs::remove_file(path)?),
        Ok(_) => {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
            for entry in std::fs::read_dir(path)? { remove_tree(&entry?.path())?; }
            Ok(std::fs::remove_dir(path)?)
        }
    }
}

/// The lock collect takes shared and apply/restore exclusive, so a collect
/// never works from tombstones older than the rows it writes.
pub fn lock(project: &Path, exclusive: bool) -> Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).custom_flags(libc::O_NOFOLLOW)
        .open(project.join(".state/telemetry-maintenance.lock")).context("open the telemetry maintenance lock")?;
    if exclusive { file.lock()?; } else { file.lock_shared()?; }
    Ok(file)
}

/// `maintenance apply`: owner only; `--confirm` for destructive items; tombstone, then delete.
pub fn apply(project: &Path, config_dir: &Path, confirm: Option<&str>, dry_run: bool, forget: &[String]) -> Result<Value> {
    super::review::refuse_owner_cli_in_worker_context(project, "`maintenance apply`")?;
    let _runtime = crate::migration::runtime_mutation(project)?;
    let _lock = lock(project, true)?;
    let plan = plan(project, config_dir, store::now(), forget)?;
    if let Some(db) = super::sidecar::read(project)? { declared(&db)?; }
    if dry_run {
        return Ok(json!({"dry_run": true, "plan_digest": plan.digest, "would_delete": plan.items.iter().map(|i| json!({"class": i.class, "key": i.key})).collect::<Vec<_>>(),
            "confirm_required": plan.destructive > 0, "plan": plan.json}));
    }
    if plan.destructive > 0 {
        match confirm {
            Some(digest) if digest == plan.digest => {}
            Some(digest) => bail!("the plan changed: its digest is {} (confirmed {digest}); review `maintenance plan` again", plan.digest),
            None => bail!("apply would delete {} destructive item(s); review `maintenance plan` and pass --confirm {}", plan.destructive, plan.digest),
        }
    }
    let ops = store::open(project)?;
    let started = store::now();
    let run = store::sha256(format!("{}:{started}", plan.digest).as_bytes());
    ops.execute("INSERT INTO maintenance_runs(run_id,plan_digest,policy,principal,started_unix_ms) VALUES(?1,?2,?3,?4,?5)",
        params![run, plan.digest, POLICY, store::PRINCIPAL, started])?;
    let mut deleted: BTreeMap<&str, usize> = BTreeMap::new();
    let mut tombstoned = 0;
    let mut sidecar = super::sidecar::open(project, false)?;
    for item in &plan.items {
        // Tombstone first: an interruption leaves the tombstone, and the next
        // apply, collect or restore finishes the deletion.
        let keys: Vec<(Option<&str>, Option<i64>)> = item.tombs.iter().map(|(k, b)| (k.as_deref(), *b)).collect();
        tombstoned += store::tombstone(&ops, item.class, &keys, item.reason, &run, started)?;
        match &item.target {
            Target::Session { id, paths } => {
                let db = sidecar.as_mut().context("the sidecar disappeared")?;
                let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
                purge_session(&tx, id, paths)?;
                tx.commit()?;
            }
            Target::Attention { attempt, before } => {
                sidecar.as_ref().context("the sidecar disappeared")?
                    .execute("DELETE FROM attention_samples WHERE attempt_id=?1 AND observed_unix_ms<?2", params![attempt, before])?;
            }
            Target::Health { before } => { sidecar.as_ref().context("the sidecar disappeared")?.execute("DELETE FROM health_evaluations WHERE evaluated_unix_ms<?1", [before])?; }
            Target::Revision { revision } => {
                let db = sidecar.as_mut().context("the sidecar disappeared")?;
                let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
                delete_revisions(&tx, &[*revision])?;
                tx.commit()?;
            }
            Target::Comparison { revision } => { sidecar.as_ref().context("the sidecar disappeared")?.execute("DELETE FROM analytics_workspace_comparisons WHERE revision=?1", [revision])?; }
            Target::Tree(path) => remove_tree(path)?,
            Target::Backup { id, location, files, missing } => {
                if !missing {
                    for name in files { remove_tree(&location.join(backup::safe_name(name)?))?; }
                    remove_tree(&location.join(backup::MANIFEST))?;
                    let _ = std::fs::remove_dir(location);
                }
                ops.execute("UPDATE backups SET deleted_unix_ms=?2 WHERE backup_id=?1", params![id, store::now()])?;
            }
            Target::File(paths) => for path in paths { remove_tree(path)?; },
        }
        *deleted.entry(item.class).or_default() += 1;
    }
    // Finish earlier interrupted runs too: every tombstone holds.
    let enforced = match sidecar.as_mut() { Some(db) => enforce(db, &Tombstones::load(&ops)?)?, None => BTreeMap::new() };
    let summary = json!({"deleted": deleted, "tombstones_added": tombstoned, "enforced": enforced});
    ops.execute("UPDATE maintenance_runs SET completed_unix_ms=?2,summary=?3 WHERE run_id=?1", params![run, store::now(), summary.to_string()])?;
    Ok(json!({"run_id": run, "plan_digest": plan.digest, "policy": POLICY, "principal": store::PRINCIPAL, "deleted": deleted,
        "items": plan.items.iter().map(|i| json!({"class": i.class, "key": i.key})).collect::<Vec<_>>(), "tombstones_added": tombstoned,
        "canonical_written": false}))
}

fn plan_text(plan: &Value) -> String {
    let mut out = format!("plan {} {} items={} destructive={} digest={}\n", plan["policy"].as_str().unwrap_or_default(), plan["project"].as_str().unwrap_or_default(),
        plan["items"], plan["destructive_items"], plan["plan_digest"].as_str().unwrap_or_default());
    for c in plan["classes"].as_array().into_iter().flatten() {
        out += &format!("{} {}d{}: eligible={} blocked={} held={}\n", c["class"].as_str().unwrap_or_default(), c["retention_days"], if c["destructive"] == true { " destructive" } else { "" },
            c["eligible_count"], c["blocked_count"], c["held"].as_array().map_or(0, Vec::len));
        for e in c["eligible"].as_array().into_iter().flatten() { out += &format!("  delete {}\n", e["key"].as_str().unwrap_or_default()); }
        for b in c["blocked"].as_array().into_iter().flatten() { out += &format!("  blocked {} ({})\n", b["key"].as_str().unwrap_or_default(), b["reason"].as_str().unwrap_or_default()); }
        for h in c["held"].as_array().into_iter().flatten() { out += &format!("  held {} ({})\n", h["key"].as_str().unwrap_or_default(), h["hold_id"].as_str().unwrap_or_default()); }
    }
    for s in plan["quotas"]["spool"].as_array().into_iter().flatten().filter(|s| s["state"] != "ok") {
        out += &format!("quota spool {} entries={} limit={} {}\n", s["attempt"].as_str().unwrap_or_default(), s["entries"], s["limit_entries"], s["state"].as_str().unwrap_or_default());
    }
    out
}

fn apply_text(value: &Value) -> String {
    if value["dry_run"] == true {
        let mut out = format!("dry-run digest={} confirm_required={}\n", value["plan_digest"].as_str().unwrap_or_default(), value["confirm_required"]);
        for i in value["would_delete"].as_array().into_iter().flatten() { out += &format!("  would delete {} {}\n", i["class"].as_str().unwrap_or_default(), i["key"].as_str().unwrap_or_default()); }
        return out;
    }
    let mut out = format!("applied digest={} tombstones_added={}\n", value["plan_digest"].as_str().unwrap_or_default(), value["tombstones_added"]);
    for i in value["items"].as_array().into_iter().flatten() { out += &format!("  deleted {} {}\n", i["class"].as_str().unwrap_or_default(), i["key"].as_str().unwrap_or_default()); }
    out
}

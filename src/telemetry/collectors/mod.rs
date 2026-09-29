//! Lane A (docs/telemetry/phase2-lanes.md): sidecar stream `ingest`. Hooks
//! registered centrally in `super::LANES`; this lane adds subcommands, metrics,
//! tick work and `migrations/telemetry/ingest/NNNN_*.sql` here only.
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

pub const STREAM: &str = "ingest";
/// `include_str!` of `migrations/telemetry/ingest/`, in order; index + 1 is the stream version.
pub const MIGRATIONS: &[&str] = &[include_str!("../../../migrations/telemetry/ingest/0001_source_bindings.sql"),
    include_str!("../../../migrations/telemetry/ingest/0002_source_observations.sql"),
    include_str!("../../../migrations/telemetry/ingest/0003_malformed_quarantine.sql"),
    include_str!("../../../migrations/telemetry/ingest/0004_codex_metadata.sql")];

/// `herdr-projects telemetry <slug> collectors ...`
#[derive(clap::Subcommand)]
pub enum Command {
    /// Stream version of this lane's sidecar tables. Read-only.
    Status,
    /// Canonical collector binding revisions and why each rollout is bound or not. Read-only.
    Bindings,
    /// Per rollout: the A4 session metadata (model provider, fork and subagent
    /// parent ids) and the span of its usage record times. Read-only.
    Sessions,
    /// Append a `revoked` revision to the attempt's active collector binding:
    /// rollouts that start from now on are not bound to it (contracts-collection.md).
    Revoke { attempt: String },
    /// Per adapter and source field: whether it is collected, its basis and
    /// what certifies it (`live`, `fixture` or `none`). Static; reads nothing.
    Capabilities {
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
}

/// The command's stdout.
pub fn run(project: &Path, command: Command) -> Result<String> {
    let value = match command {
        Command::Status => super::sidecar::status(project, STREAM)?,
        Command::Bindings => bindings(project)?,
        Command::Sessions => sessions(project)?,
        Command::Capabilities { json: false } => return Ok(capabilities_text(&capabilities()?)),
        Command::Capabilities { json: true } => capabilities()?,
        Command::Revoke { attempt } => {
            let mut store = crate::store::SqliteStore::open(&project.join(".state/state.db"))?;
            let (binding, written) = store.revoke_collector_binding(&attempt, jiff::Timestamp::now().as_millisecond())?;
            json!({"binding": binding, "written": written})
        }
    };
    Ok(serde_json::to_string_pretty(&value)? + "\n")
}

/// What certifies a field's value (contracts-collection.md A3).
#[derive(Clone, Copy)]
enum Certified {
    /// Observed in the live run of a certified version (docs/telemetry/codex-live-0.154.0.md).
    Live,
    /// Collected and exercised by the fixture corpus only.
    Fixture,
    /// Not collected.
    None,
}

/// A Codex field that is collected, with what certifies it and what it does not mean.
fn field(kind: &'static str, field: impl Into<String>, certified: Certified, caveat: Option<&'static str>) -> Field {
    Field { kind, field: field.into(), certified, caveat, reason: None }
}
/// A Codex field that is not collected, and why.
fn absent(kind: &'static str, field: &'static str, reason: &'static str) -> Field {
    Field { kind, field: field.into(), certified: Certified::None, caveat: None, reason: Some(reason) }
}

struct Field {
    kind: &'static str,
    field: String,
    certified: Certified,
    caveat: Option<&'static str>,
    reason: Option<&'static str>,
}

const USAGE: [&str; 6] = ["cache_write_input_tokens", "cached_input_tokens", "input_tokens", "output_tokens", "reasoning_output_tokens", "total_tokens"];

/// The one declared Codex field table. Collected fields are exactly the
/// `sanitize::codex_allowlist` paths (checked when printed, and against the
/// emitted envelopes by tests/telemetry_conformance.rs), plus the line
/// `timestamp` every envelope keeps as `occurred_unix_ms`. `Live`: observed in
/// the 0.154.0 live run; `Fixture`: fixture corpus only.
fn codex_fields() -> Vec<Field> {
    use Certified::{Fixture, Live};
    let usage = |kind: &'static str, prefix: &str, caveat: Option<&'static str>| USAGE.map(|name|
        field(kind, format!("{prefix}.{name}"), Live, if name == "cache_write_input_tokens" && caveat.is_none() { Some("overlap_with_input_not_certified") } else { caveat }));
    let mut fields = vec![
        field("line", "timestamp", Fixture, Some("envelope_occurred_unix_ms")),
        field("session_meta", "id", Live, None),
        field("session_meta", "timestamp", Live, None),
        field("session_meta", "cwd", Live, None),
        field("session_meta", "cli_version", Live, None),
        field("session_meta", "originator", Fixture, None),
        field("session_meta", "source", Fixture, None),
        // A4: metadata of the proposed §5/§7 revision (contracts-collection.md),
        // fixture-certified until the planned live run.
        field("session_meta", "model_provider", Fixture, None),
        field("session_meta", "forked_from_id", Fixture, Some("semantics_not_certified")),
        field("session_meta", "subagent_kind", Fixture, Some("from_source_subagent")),
        field("session_meta", "subagent_parent_thread_id", Fixture, Some("from_source_subagent")),
        field("session_meta", "subagent_depth", Fixture, Some("from_source_subagent")),
        absent("session_meta", "forked_from_ordinal_exclusive", "not_collected"),
        absent("session_meta", "agent_nickname", "not_collected"),
        absent("session_meta", "agent_role", "not_collected"),
        absent("session_meta", "base_instructions", "content_forbidden"),
        field("turn_context", "turn_id", Fixture, None),
        field("turn_context", "model", Live, None),
        field("turn_context", "effort", Live, None),
        absent("turn_context", "cwd", "not_collected"),
        absent("turn_context", "approval_policy", "not_collected"),
        absent("turn_context", "collaboration_mode", "content_forbidden"),
        absent("turn_context", "user_instructions", "content_forbidden"),
        field("task_started", "turn_id", Fixture, None),
        absent("task_started", "started_at", "not_collected"),
        field("token_usage_record", "session_id", Fixture, None),
        field("token_usage_record", "turn_id", Fixture, None),
        field("token_usage_record", "response_id", Fixture, None),
    ];
    fields.extend(usage("token_usage_record", "usage", None));
    fields.extend(usage("token_usage_record", "thread_token_usage", Some("reconciliation_only")));
    fields.extend([absent("token_usage_record", "thread_id", "not_collected"), absent("token_usage_record", "root_turn_id", "not_collected"),
        absent("token_usage_record", "turn_token_usage", "not_collected")]);
    fields.extend(usage("token_count", "info.total_token_usage", Some("reconciliation_only")));
    fields.extend([
        field("token_count", "rate_limits.limit_id", Live, Some("semantics_not_certified")),
        field("token_count", "rate_limits.plan_type", Live, Some("semantics_not_certified")),
        field("token_count", "rate_limits.primary.used_percent", Live, Some("semantics_not_certified")),
        field("token_count", "rate_limits.primary.window_minutes", Live, Some("semantics_not_certified")),
        field("token_count", "rate_limits.primary.resets_at", Live, Some("semantics_not_certified")),
        absent("token_count", "info.last_token_usage", "not_collected"),
        absent("token_count", "info.model_context_window", "not_collected"),
        absent("token_count", "rate_limits.limit_name", "not_collected"),
        field("token_count", "rate_limits.secondary.used_percent", Fixture, Some("semantics_not_certified")),
        field("token_count", "rate_limits.secondary.window_minutes", Fixture, Some("semantics_not_certified")),
        field("token_count", "rate_limits.secondary.resets_at", Fixture, Some("semantics_not_certified")),
        field("token_count", "rate_limits.rate_limit_reached_type", Fixture, Some("semantics_not_certified")),
        absent("token_count", "rate_limits.credits", "not_collected"),
        field("task_complete", "turn_id", Fixture, None),
        field("task_complete", "duration_ms", Fixture, None),
        field("task_complete", "time_to_first_token_ms", Fixture, None),
        absent("task_complete", "started_at", "not_collected"),
        absent("task_complete", "completed_at", "not_collected"),
        absent("task_complete", "last_agent_message", "content_forbidden"),
        absent("response_item", "*", "content_forbidden"),
        // Tool/exec metadata (call id, tool name, duration, exit status): proposed
        // in the A4 revision, held until the live run shows its record shape.
        absent("exec_command_end", "*", "not_collected"),
        absent("mcp_tool_call_end", "*", "not_collected"),
    ]);
    fields
}

/// `collectors capabilities --json`: the declared table with each collected
/// field's basis from its sanitizer class. Fails if the table and the
/// sanitizer allowlist disagree.
fn capabilities() -> Result<Value> {
    use super::sanitize::{Class, codex_allowlist};
    let declared = codex_fields();
    let mut out = Vec::new();
    for f in &declared {
        let class = if f.kind == "line" { Some(Class::Text) } else {
            codex_allowlist(f.kind).unwrap_or_default().into_iter().find(|(path, _)| *path == f.field).map(|(_, class)| class)
        };
        let collected = f.reason.is_none();
        anyhow::ensure!(collected == class.is_some(), "capability table and allowlist disagree on codex {}.{}", f.kind, f.field);
        let basis = match (f.kind, class) {
            ("line", _) => "reported",
            (_, None) => "unavailable",
            (_, Some(Class::Id | Class::Number)) => "reported",
            (_, Some(Class::Text | Class::Tag)) => "reported_excerpt",
            (_, Some(Class::Path)) => "reported_home_redacted",
        };
        let certified = match f.certified { Certified::Live => "live", Certified::Fixture => "fixture", Certified::None => "none" };
        out.push(json!({"kind": f.kind, "field": f.field, "available": collected, "basis": basis, "certified": certified, "caveat": f.caveat, "reason": f.reason}));
    }
    for kind in ["session_meta", "turn_context", "task_started", "token_usage_record", "token_count", "task_complete"] {
        for (path, _) in codex_allowlist(kind).unwrap_or_default() {
            anyhow::ensure!(declared.iter().any(|f| f.kind == kind && f.field == path), "codex {kind}.{path} is collected but not declared");
        }
    }
    Ok(json!({"adapters": [{"adapter": "codex", "interface": "rollout_jsonl", "certified_versions": super::codex::CERTIFIED,
        "uncertified_version": "cli_version_uncertified", "fields": out}]}))
}

fn capabilities_text(value: &Value) -> String {
    let mut out = String::new();
    for adapter in value["adapters"].as_array().into_iter().flatten() {
        let versions: Vec<&str> = adapter["certified_versions"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        out += &format!("{} {} certified_versions={}\n", adapter["adapter"].as_str().unwrap_or("-"), adapter["interface"].as_str().unwrap_or("-"), versions.join(","));
        for f in adapter["fields"].as_array().into_iter().flatten() {
            let word = |key: &str| f[key].as_str().unwrap_or("-").to_owned();
            out += &format!("  {}.{} available={} basis={} certified={}", word("kind"), word("field"), f["available"], word("basis"), word("certified"));
            for key in ["caveat", "reason"] { if let Some(text) = f[key].as_str() { out += &format!(" {key}={text}"); } }
            out += "\n";
        }
    }
    out
}

fn exists(db: &rusqlite::Connection, table: &str) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)", [table], |r| r.get(0))?)
}

/// `{bindings, sources}`: every canonical revision, and each rollout source's
/// binding with its `basis` (`null` until a collect of this binary). Read-only.
fn bindings(project: &Path) -> Result<Value> {
    let mut bindings = Vec::new();
    let state = project.join(".state/state.db");
    if state.exists() {
        let db = super::read_only(&state)?;
        if exists(&db, "collector_bindings")? {
            let mut stmt = db.prepare("SELECT attempt_id,revision,state,collector,unix_ms FROM collector_bindings ORDER BY attempt_id,revision")?;
            for row in stmt.query_map([], |r| Ok(json!({"attempt_id": r.get::<_, String>(0)?, "revision": r.get::<_, i64>(1)?,
                "state": r.get::<_, String>(2)?, "collector": r.get::<_, Option<String>>(3)?, "unix_ms": r.get::<_, i64>(4)?})))? { bindings.push(row?); }
        }
    }
    let mut sources = Vec::new();
    if let Some(db) = super::sidecar::read(project)? {
        let basis = if exists(&db, "source_bindings")? { "(SELECT basis FROM source_bindings b WHERE b.path_digest=s.path_digest)" } else { "NULL" };
        let mut stmt = db.prepare(&format!("SELECT s.session_id,s.binding,s.attempt_id,{basis} FROM rollout_sources s ORDER BY s.session_id,s.path_digest"))?;
        for row in stmt.query_map([], |r| Ok(json!({"session_id": r.get::<_, String>(0)?, "binding": r.get::<_, String>(1)?,
            "attempt_id": r.get::<_, Option<String>>(2)?, "basis": r.get::<_, Option<String>>(3)?})))? { sources.push(row?); }
    }
    Ok(json!({"bindings": bindings, "sources": sources}))
}

/// `collectors sessions`: per rollout source, its A4 metadata and usage record
/// times. A field is `unavailable` with `predates_collection` when the sidecar
/// has no A4 tables (ingest stream < 4, read without migrating), and with
/// `pending_reread` while a rollout read before A4 waits to be read again.
/// `null` is a value the rollout did not report. Read-only.
fn sessions(project: &Path) -> Result<Value> {
    let Some(db) = super::sidecar::read(project)? else { return Ok(json!({"sessions": unavailable("collection_not_run")})) };
    let a4 = exists(&db, "rollout_metadata")?;
    let columns = if a4 {
        "m.model_provider,m.forked_from_id,m.subagent_kind,m.subagent_parent_thread_id,m.subagent_depth,
        (SELECT count(t.record_unix_ms) FROM codex_usage u JOIN codex_usage_times t USING(session_id,ordinal) WHERE u.path_digest=s.path_digest),
        (SELECT min(t.record_unix_ms) FROM codex_usage u JOIN codex_usage_times t USING(session_id,ordinal) WHERE u.path_digest=s.path_digest),
        (SELECT max(t.record_unix_ms) FROM codex_usage u JOIN codex_usage_times t USING(session_id,ordinal) WHERE u.path_digest=s.path_digest),
        m.path_digest IS NOT NULL FROM rollout_sources s LEFT JOIN rollout_metadata m ON m.path_digest=s.path_digest"
    } else { "NULL,NULL,NULL,NULL,NULL,0,NULL,NULL,0 FROM rollout_sources s" };
    let mut stmt = db.prepare(&format!("SELECT s.session_id,s.path_digest,s.binding,s.attempt_id,s.records,
        (SELECT count(*) FROM codex_usage u WHERE u.path_digest=s.path_digest),{columns} ORDER BY s.session_id,s.path_digest"))?;
    let rows = stmt.query_map([], |r| {
        let pending = match (a4, r.get::<_, bool>(14)?) { (false, _) => Some("predates_collection"), (true, false) => Some("pending_reread"), _ => None };
        let known = |value: Value| pending.map_or(value, unavailable);
        let text = |i: usize| r.get::<_, Option<String>>(i).map(|v| known(json!(v)));
        Ok(json!({"session_id": r.get::<_, String>(0)?, "path_digest": r.get::<_, String>(1)?, "binding": r.get::<_, String>(2)?,
            "attempt_id": r.get::<_, Option<String>>(3)?, "records": r.get::<_, i64>(4)?, "model_provider": text(6)?, "forked_from_id": text(7)?,
            "subagent": known(json!({"kind": r.get::<_, Option<String>>(8)?, "parent_thread_id": r.get::<_, Option<String>>(9)?, "depth": r.get::<_, Option<i64>>(10)?})),
            "record_times": known(json!({"stored": r.get::<_, i64>(5)?, "timed": r.get::<_, i64>(11)?, "first_unix_ms": r.get::<_, Option<i64>>(12)?,
                "last_unix_ms": r.get::<_, Option<i64>>(13)?}))}))
    })?;
    Ok(json!({"sessions": rows.collect::<rusqlite::Result<Vec<_>>>()?}))
}

fn unavailable(reason: &str) -> Value {
    json!({"status": "unavailable", "reason": reason})
}

/// Metrics merged into `telemetry <slug> report` (`super::metrics::report`).
pub fn metrics(_project: &Path, _since: Option<i64>) -> Result<BTreeMap<String, Value>> { Ok(BTreeMap::new()) }

/// Ticker telemetry pass, after the Codex collect, within `budget`. Writes only the sidecar.
pub fn tick(_project: &Path, _budget: super::codex::Budget) -> Result<()> { Ok(()) }

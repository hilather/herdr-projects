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
    include_str!("../../../migrations/telemetry/ingest/0004_codex_metadata.sql"),
    include_str!("../../../migrations/telemetry/ingest/0005_codex_threads.sql"),
    include_str!("../../../migrations/telemetry/ingest/0006_codex_tool_metadata.sql"),
    include_str!("../../../migrations/telemetry/ingest/0007_codex_followups.sql"),
    include_str!("../../../migrations/telemetry/ingest/0008_codex_live_run2.sql"),
    include_str!("../../../migrations/telemetry/ingest/0009_turn_terminations.sql")];

/// `herdr-projects telemetry <slug> collectors ...`
#[derive(clap::Subcommand)]
pub enum Command {
    /// Stream version of this lane's sidecar tables. Read-only.
    Status,
    /// Canonical collector binding revisions and why each rollout is bound or not. Read-only.
    Bindings,
    /// Per rollout: the A4 session metadata (model provider, fork and subagent
    /// parent ids), the A5 thread lineage (parent thread id, reported session
    /// id, thread source), the span of its usage record times, and (A7) the
    /// subagent detail and whether its last turn's final event was read,
    /// is still open, ended by the product's termination (F4) or is missing,
    /// and (A8) whether it was aborted, and a
    /// fork's fork point and how its reported totals were reconciled. Read-only.
    Sessions,
    /// Per session: the A6 tool call metadata (call id, tool name, status,
    /// turn, call and output times; A8: a function call's namespace) and exec
    /// items (id, status, source, exit code, startup duration), and the A8 MCP
    /// calls (server, tool, status, hint, error flag, duration), subagent and
    /// collab items (type, ids, status) and aborted turns. Metadata only, never
    /// a tool's input, arguments, output, result content or command. Read-only.
    Tools {
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Append a `revoked` revision to the attempt's active collector binding:
    /// rollouts that start from now on are not bound to it (contracts-collection.md).
    Revoke { attempt: String },
    /// Per adapter and source field: whether it is collected, its basis and
    /// what certifies it (`live`, `fixture` or `none`); then the project's
    /// retained Codex profiles with their recorded agent version and a warning
    /// when it is uncertified or can drift. Read-only.
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
        Command::Tools { json: false } => return Ok(tools_text(&tools(project)?)),
        Command::Tools { json: true } => tools(project)?,
        Command::Capabilities { json: false } => return Ok(capabilities_text(&capabilities(project)?)),
        Command::Capabilities { json: true } => capabilities(project)?,
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
        field("line", "timestamp", Live, Some("envelope_occurred_unix_ms")),
        field("session_meta", "id", Live, None),
        field("session_meta", "timestamp", Live, None),
        field("session_meta", "cwd", Live, None),
        field("session_meta", "cli_version", Live, None),
        field("session_meta", "originator", Live, None),
        field("session_meta", "source", Live, None),
        // A4: metadata of the §5/§7 revision (contracts-collection.md). Live where
        // the A4 live run (codex-live-0.154.0-a4.md) or the second live run
        // (codex-live-0.154.0-run2.md: a `codex exec fork`, a spawned subagent)
        // saw a value; else fixture.
        field("session_meta", "model_provider", Live, None),
        // run2: the fork names its origin and replays none of its records, but its
        // reported thread totals include the origin's (a `thread_total` discrepancy).
        field("session_meta", "forked_from_id", Live, Some("fork_thread_total_includes_origin")),
        field("session_meta", "subagent_kind", Live, Some("from_source_subagent")),
        // A7: the `other` variant's tag; the A4 live run saw only `guardian`.
        field("session_meta", "subagent_detail", Live, Some("observed_guardian_only")),
        field("session_meta", "subagent_parent_thread_id", Live, Some("from_source_subagent")),
        field("session_meta", "subagent_depth", Live, Some("from_source_subagent")),
        // A5: the thread lineage outside `source`, live in the A4 run (guardian)
        // and run2 (a `thread_spawn` subagent). Both children report their
        // parent's `session_id`.
        field("session_meta", "parent_thread_id", Live, Some("observed_guardian_and_thread_spawn")),
        field("session_meta", "session_id", Live, Some("child_reports_parent_session")),
        field("session_meta", "thread_source", Live, Some("observed_user_guardian_review_subagent")),
        // A8: the fork point, live in run2 (a `codex exec fork`). A fork's
        // reported totals are reconciled against its origin's at this point.
        field("session_meta", "forked_from_ordinal_exclusive", Live, None),
        field("session_meta", "history_base.thread_id", Live, None),
        field("session_meta", "history_base.end_ordinal_exclusive", Live, None),
        field("session_meta", "history_base.end_byte_offset", Live, Some("origin_file_length_at_fork")),
        absent("session_meta", "agent_nickname", "not_collected"),
        absent("session_meta", "agent_role", "not_collected"),
        absent("session_meta", "base_instructions", "content_forbidden"),
        field("turn_context", "turn_id", Live, None),
        field("turn_context", "model", Live, None),
        field("turn_context", "effort", Live, None),
        absent("turn_context", "cwd", "not_collected"),
        absent("turn_context", "approval_policy", "not_collected"),
        absent("turn_context", "collaboration_mode", "content_forbidden"),
        absent("turn_context", "user_instructions", "content_forbidden"),
        field("task_started", "turn_id", Live, None),
        absent("task_started", "started_at", "not_collected"),
        field("token_usage_record", "session_id", Live, Some("child_reports_parent_session")),
        field("token_usage_record", "turn_id", Live, None),
        field("token_usage_record", "response_id", Live, None),
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
        field("task_complete", "turn_id", Live, None),
        field("task_complete", "duration_ms", Live, None),
        field("task_complete", "time_to_first_token_ms", Live, None),
        absent("task_complete", "started_at", "not_collected"),
        absent("task_complete", "completed_at", "not_collected"),
        absent("task_complete", "last_agent_message", "content_forbidden"),
        // A8: an aborted turn's final event (run2: a declined approval, `interrupted`).
        field("turn_aborted", "turn_id", Live, None),
        field("turn_aborted", "reason", Live, None),
        field("turn_aborted", "duration_ms", Live, None),
        absent("turn_aborted", "started_at", "not_collected"),
        absent("turn_aborted", "completed_at", "not_collected"),
        absent("response_item", "message", "content_forbidden"),
        absent("response_item", "reasoning", "content_forbidden"),
        // A6 tool/exec metadata (contracts-collection.md A6), the allowlist of
        // codex-live-0.154.0-a4.md §4. Live where the A4 live run saw a value
        // (exec and wait calls, CommandExecution items); else fixture.
        field("custom_tool_call", "call_id", Live, None),
        field("custom_tool_call", "name", Live, None),
        field("custom_tool_call", "status", Live, None),
        field("custom_tool_call", "internal_chat_message_metadata_passthrough.turn_id", Live, None),
        absent("custom_tool_call", "id", "not_collected"),
        absent("custom_tool_call", "internal_chat_message_metadata_passthrough.create_time", "not_collected"),
        absent("custom_tool_call", "input", "content_forbidden"),
        field("function_call", "call_id", Live, None),
        field("function_call", "name", Live, None),
        // A8: run2's `spawn_agent` and `wait_agent` calls (`collaboration`).
        field("function_call", "namespace", Live, None),
        // The live `function_call`s (`wait`; run2 `spawn_agent`, `wait_agent`)
        // carried no status.
        field("function_call", "status", Fixture, None),
        field("function_call", "internal_chat_message_metadata_passthrough.turn_id", Live, None),
        absent("function_call", "id", "not_collected"),
        absent("function_call", "arguments", "content_forbidden"),
        field("custom_tool_call_output", "call_id", Live, None),
        absent("custom_tool_call_output", "id", "not_collected"),
        absent("custom_tool_call_output", "output", "content_forbidden"),
        field("function_call_output", "call_id", Live, None),
        absent("function_call_output", "id", "not_collected"),
        absent("function_call_output", "output", "content_forbidden"),
        field("item_completed", "thread_id", Live, None),
        field("item_completed", "turn_id", Live, None),
        field("item_completed", "item.type", Live, None),
        // A8 widens `item.id` to the MCP and agent items and `item.status` to
        // the MCP and collab items; run2 certified the exec status `failed`
        // (non-zero exit) beside `completed`.
        field("item_completed", "item.id", Live, Some("typed_items_only")),
        field("item_completed", "item.status", Live, Some("observed_completed_failed")),
        field("item_completed", "item.source", Live, Some("command_execution_only")),
        field("item_completed", "item.exit_code", Live, Some("command_execution_only")),
        field("item_completed", "item.duration.secs", Live, Some("startup_not_run_time")),
        field("item_completed", "item.duration.nanos", Live, Some("startup_not_run_time")),
        absent("item_completed", "started_at_ms", "not_collected"),
        absent("item_completed", "completed_at_ms", "not_collected"),
        absent("item_completed", "item.process_id", "not_collected"),
        absent("item_completed", "item.cwd", "not_collected"),
        absent("item_completed", "item.client_id", "not_collected"),
        absent("item_completed", "item.phase", "not_collected"),
        absent("item_completed", "item.command", "content_forbidden"),
        absent("item_completed", "item.parsed_cmd", "content_forbidden"),
        absent("item_completed", "item.stdout", "content_forbidden"),
        absent("item_completed", "item.stderr", "content_forbidden"),
        absent("item_completed", "item.aggregated_output", "content_forbidden"),
        absent("item_completed", "item.formatted_output", "content_forbidden"),
        absent("item_completed", "item.content", "content_forbidden"),
        // A8 (run2, steward §7 decision): an MCP call is an `exec` custom tool
        // call plus an `McpToolCall` item (no typed MCP event exists), whose
        // server and tool names, hint and error flag are collected (live: a
        // local stub); never its `arguments` (keyed by data) or result content.
        field("item_completed", "item.server", Live, Some("mcp_tool_call_only")),
        field("item_completed", "item.tool", Live, Some("mcp_tool_call_only")),
        field("item_completed", "item.readOnlyHint", Live, Some("mcp_tool_call_only")),
        field("item_completed", "item.result.isError", Live, Some("mcp_tool_call_only")),
        absent("item_completed", "item.arguments", "content_forbidden"),
        absent("item_completed", "item.result.content", "content_forbidden"),
        // A8: `SubAgentActivity` and `CollabAgentToolCall` items keep their
        // type, ids and status only.
        field("item_completed", "item.agent_thread_id", Live, Some("subagent_activity_only")),
        field("item_completed", "item.sender_thread_id", Live, Some("collab_agent_tool_call_only")),
        field("item_completed", "item.receiver_thread_ids", Live, Some("collab_agent_tool_call_only")),
        absent("item_completed", "item.kind", "not_collected"),
        absent("item_completed", "item.agents_states", "not_collected"),
        absent("item_completed", "item.agent_path", "content_forbidden"),
        absent("item_completed", "item.receiver_agents", "content_forbidden"),
    ]);
    fields
}

/// `collectors capabilities --json`: the declared table with each collected
/// field's basis from its sanitizer class. Fails if the table and the
/// sanitizer allowlist disagree.
fn capabilities(project: &Path) -> Result<Value> {
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
            (_, Some(Class::Id | Class::Number | Class::Bool | Class::IdList)) => "reported",
            (_, Some(Class::Text | Class::Tag)) => "reported_excerpt",
            (_, Some(Class::Path)) => "reported_home_redacted",
        };
        let certified = match f.certified { Certified::Live => "live", Certified::Fixture => "fixture", Certified::None => "none" };
        out.push(json!({"kind": f.kind, "field": f.field, "available": collected, "basis": basis, "certified": certified, "caveat": f.caveat, "reason": f.reason}));
    }
    for kind in ["session_meta", "turn_context", "task_started", "token_usage_record", "token_count", "task_complete", "turn_aborted", "custom_tool_call",
        "function_call", "custom_tool_call_output", "function_call_output", "item_completed"] {
        for (path, _) in codex_allowlist(kind).unwrap_or_default() {
            anyhow::ensure!(declared.iter().any(|f| f.kind == kind && f.field == path), "codex {kind}.{path} is collected but not declared");
        }
    }
    Ok(json!({"adapters": [{"adapter": "codex", "interface": "rollout_jsonl", "certified_versions": super::codex::CERTIFIED,
        "uncertified_version": "cli_version_uncertified", "fields": out, "profiles": super::codex::profile_versions(project)?}]}))
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
        for p in adapter["profiles"].as_array().into_iter().flatten() {
            let word = |key: &str| p[key].as_str().unwrap_or("-").to_owned();
            out += &format!("profile {} agent {} version {} certified={}\n", word("profile"), word("agent"), word("version"), p["certified"]);
            for w in p["warnings"].as_array().into_iter().flatten() {
                out += &format!("  WARNING {}: {}\n", w["code"].as_str().unwrap_or("-"), w["detail"].as_str().unwrap_or(""));
            }
            if p["warnings"].as_array().is_some_and(|w| !w.is_empty()) { out += &format!("  to fix: {}\n", super::codex::PIN_ADVICE); }
        }
    }
    out
}

fn exists(db: &rusqlite::Connection, table: &str) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)", [table], |r| r.get(0))?)
}

/// Whether an attempt's latest usage collector binding was explicitly revoked.
/// Advisory readers keep historical usage, but stop decorating a live pane.
pub fn binding_revoked(project: &Path, attempt: &str) -> Result<bool> {
    use rusqlite::OptionalExtension;
    let db = super::read_only(&project.join(".state/state.db"))?;
    if !exists(&db, "collector_bindings")? { return Ok(false); }
    let state: Option<String> = db.query_row("SELECT state FROM collector_bindings WHERE attempt_id=?1 ORDER BY revision DESC LIMIT 1", [attempt], |r| r.get(0)).optional()?;
    Ok(state.as_deref() == Some("revoked"))
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

/// `collectors sessions`: per rollout source, its A4 metadata, A5 thread
/// lineage, usage record times and A7 subagent detail and final event. A
/// field is `unavailable` with `predates_collection` when the sidecar has no
/// table for it (ingest stream < 4, < 5 for `thread`, < 7 for
/// `subagent.detail` and `final_event`, read without migrating), and with
/// `pending_reread` while a rollout read before it waits to be read again.
/// `null` is a value the rollout did not report; `thread.session_id` is also
/// `null` when it equals `session_id`. `final_event.state`: `no_turn`,
/// `complete` (the last turn's `task_complete` was read), `aborted` (A8: it
/// ended with `turn_aborted`), `ended_by_termination` (F4, ingest 0009: the
/// product ended the bound attempt while the turn was open; `termination`
/// then names the receipt's cause and time), `open` (not yet), or `missing`
/// (a pending `final_event_missing` coverage gap: idle past the threshold
/// without it); it needs ingest 0008 too. `fork` (A8, ingest 0008): `null` for a rollout
/// that names no fork point, else `{forked_from_ordinal_exclusive,
/// history_base {thread_id, end_ordinal_exclusive, end_byte_offset},
/// reconciliation {thread_total, token_count_total}}`, a
/// `codex_fork_reconciliation` state per reported total (`null` before one is
/// reconciled or without `history_base.thread_id`). `after_termination` (F3):
/// for a rollout bound to an attempt with a termination receipt,
/// `{terminated_unix_ms, records, first_unix_ms}` counts its usage records
/// whose line time is after the receipt (a later `codex exec resume` of the
/// ended attempt's session, still charged to it), else `null`; `unavailable`
/// without record times (ingest < 4). Read-only (the canonical store too).
fn sessions(project: &Path) -> Result<Value> {
    let Some(db) = super::sidecar::read(project)? else { return Ok(json!({"sessions": unavailable("collection_not_run")})) };
    let terminated_at: BTreeMap<String, i64> = super::codex::canonical_attempts(project)?.iter()
        .filter_map(|a| a.terminated_unix_ms().map(|at| (a.id().to_owned(), at))).collect();
    let a4 = exists(&db, "rollout_metadata")?;
    let a5 = exists(&db, "rollout_threads")?;
    let a7 = exists(&db, "rollout_ingest_state")?;
    let a8 = exists(&db, "rollout_forks")?;
    let terminations = exists(&db, "rollout_turn_terminations")?;
    let columns = if a4 {
        "m.model_provider,m.forked_from_id,m.subagent_kind,m.subagent_parent_thread_id,m.subagent_depth,
        (SELECT count(t.record_unix_ms) FROM codex_usage u JOIN codex_usage_times t USING(session_id,ordinal) WHERE u.path_digest=s.path_digest),
        (SELECT min(t.record_unix_ms) FROM codex_usage u JOIN codex_usage_times t USING(session_id,ordinal) WHERE u.path_digest=s.path_digest),
        (SELECT max(t.record_unix_ms) FROM codex_usage u JOIN codex_usage_times t USING(session_id,ordinal) WHERE u.path_digest=s.path_digest),
        m.path_digest IS NOT NULL"
    } else { "NULL,NULL,NULL,NULL,NULL,0,NULL,NULL,0" };
    let (threads, join) = if a5 { ("l.parent_thread_id,l.session_id,l.thread_source,l.path_digest IS NOT NULL", "LEFT JOIN rollout_threads l ON l.path_digest=s.path_digest") }
        else { ("NULL,NULL,NULL,0", "") };
    let join = if a4 { format!("LEFT JOIN rollout_metadata m ON m.path_digest=s.path_digest {join}") } else { join.to_owned() };
    let (followups, join) = if a7 {
        ("d.subagent_detail,d.path_digest IS NOT NULL,x.last_turn_offset,x.last_turn_id,x.last_turn_completed,x.path_digest IS NOT NULL,
        EXISTS(SELECT 1 FROM coverage_gaps g WHERE g.source=s.path_digest AND g.start_offset=x.last_turn_offset AND g.reason='final_event_missing' AND g.recovery='pending')",
            format!("{join} LEFT JOIN rollout_subagents d ON d.path_digest=s.path_digest LEFT JOIN rollout_ingest_state x ON x.path_digest=s.path_digest"))
    } else { ("NULL,0,NULL,NULL,0,0,0", join) };
    let (live2, join) = if a8 {
        ("k.forked_from_ordinal_exclusive,k.base_thread_id,k.base_end_ordinal_exclusive,k.base_end_byte_offset,k.path_digest IS NOT NULL,
        coalesce(e.last_turn_aborted,0),e.path_digest IS NOT NULL,
        (SELECT state FROM codex_fork_reconciliation r WHERE r.session_id=s.session_id AND r.kind='thread_total'),
        (SELECT state FROM codex_fork_reconciliation r WHERE r.session_id=s.session_id AND r.kind='token_count_total')",
            format!("{join} LEFT JOIN rollout_forks k ON k.path_digest=s.path_digest LEFT JOIN rollout_turn_ends e ON e.path_digest=s.path_digest"))
    } else { ("NULL,NULL,NULL,NULL,0,0,0,NULL,NULL", join) };
    let (ended, join) = if terminations && a7 {
        ("t.cause,t.terminated_unix_ms", format!("{join} LEFT JOIN rollout_turn_terminations t ON t.path_digest=s.path_digest AND t.turn_offset=x.last_turn_offset"))
    } else { ("NULL,NULL", join) };
    let mut stmt = db.prepare(&format!("SELECT s.session_id,s.path_digest,s.binding,s.attempt_id,s.records,
        (SELECT count(*) FROM codex_usage u WHERE u.path_digest=s.path_digest),{columns},{threads},{followups},{live2},{ended} FROM rollout_sources s {join}
        ORDER BY s.session_id,s.path_digest"))?;
    let rows = stmt.query_map([], |r| {
        let waiting = |table: bool, read: usize| Ok::<_, rusqlite::Error>(match (table, r.get::<_, bool>(read)?) {
            (false, _) => Some("predates_collection"), (true, false) => Some("pending_reread"), _ => None });
        let (pending, thread_pending) = (waiting(a4, 14)?, waiting(a5, 18)?);
        let (detail_pending, fork_pending) = (waiting(a7, 20)?, waiting(a8, 30)?);
        // A8: the final event needs the A7 turn state and the A8 turn end.
        let final_pending = match waiting(a7, 24)? {
            Some(reason) => Some(reason),
            None if !a8 => Some("predates_collection"),
            None => (!r.get::<_, bool>(32)?).then_some("pending_reread"),
        };
        let known = |value: Value| pending.map_or(value, unavailable);
        let detail = detail_pending.map_or(json!(r.get::<_, Option<String>>(19)?), unavailable);
        let final_event = match final_pending {
            Some(reason) => unavailable(reason),
            None => {
                let (opened, turn, completed, missing): (Option<i64>, Option<String>, bool, bool) = (r.get(21)?, r.get(22)?, r.get(23)?, r.get(25)?);
                let aborted: bool = r.get(31)?;
                let ended: (Option<String>, Option<i64>) = (r.get(35)?, r.get(36)?);
                let state = match (opened, completed, missing) {
                    (None, ..) => "no_turn",
                    (_, true, _) if aborted => "aborted",
                    (_, true, _) => "complete",
                    _ if ended.0.is_some() => "ended_by_termination",
                    (_, _, true) => "missing",
                    _ => "open",
                };
                match ended {
                    (Some(cause), at) if state == "ended_by_termination" =>
                        json!({"state": state, "turn_id": turn, "termination": {"cause": cause, "observed_unix_ms": at}}),
                    _ => json!({"state": state, "turn_id": turn}),
                }
            }
        };
        let text = |i: usize| r.get::<_, Option<String>>(i).map(|v| known(json!(v)));
        // A8: the fork point, and how the fork's reported totals were reconciled.
        let fork = match fork_pending {
            Some(reason) => unavailable(reason),
            None => {
                let (ordinal, base, end_ordinal, end_offset): (Option<i64>, Option<String>, Option<i64>, Option<i64>) = (r.get(26)?, r.get(27)?, r.get(28)?, r.get(29)?);
                let history_base = (base.is_some() || end_ordinal.is_some() || end_offset.is_some())
                    .then(|| json!({"thread_id": base, "end_ordinal_exclusive": end_ordinal, "end_byte_offset": end_offset}));
                let states: (Option<String>, Option<String>) = (r.get(33)?, r.get(34)?);
                let reconciliation = base.is_some().then(|| json!({"thread_total": states.0, "token_count_total": states.1}));
                if ordinal.is_none() && history_base.is_none() { Value::Null } else {
                    json!({"forked_from_ordinal_exclusive": ordinal, "history_base": history_base, "reconciliation": reconciliation})
                }
            }
        };
        let thread = json!({"parent_thread_id": r.get::<_, Option<String>>(15)?, "session_id": r.get::<_, Option<String>>(16)?,
            "source": r.get::<_, Option<String>>(17)?});
        let after_termination = match r.get::<_, Option<String>>(3)?.and_then(|attempt| terminated_at.get(&attempt).copied()) {
            None => Value::Null,
            Some(_) if !a4 => unavailable("predates_collection"),
            Some(at) => {
                let (records, first): (i64, Option<i64>) = db.query_row("SELECT count(*),min(t.record_unix_ms) FROM codex_usage u JOIN codex_usage_times t USING(session_id,ordinal)
                    WHERE u.path_digest=?1 AND t.record_unix_ms>?2", rusqlite::params![r.get::<_, String>(1)?, at], |q| Ok((q.get(0)?, q.get(1)?)))?;
                json!({"terminated_unix_ms": at, "records": records, "first_unix_ms": first})
            }
        };
        Ok(json!({"session_id": r.get::<_, String>(0)?, "path_digest": r.get::<_, String>(1)?, "binding": r.get::<_, String>(2)?,
            "after_termination": after_termination,
            "attempt_id": r.get::<_, Option<String>>(3)?, "records": r.get::<_, i64>(4)?, "model_provider": text(6)?, "forked_from_id": text(7)?,
            "subagent": known(json!({"kind": r.get::<_, Option<String>>(8)?, "detail": detail, "parent_thread_id": r.get::<_, Option<String>>(9)?,
                "depth": r.get::<_, Option<i64>>(10)?})), "final_event": final_event, "fork": fork,
            "thread": thread_pending.map_or(thread, unavailable),
            "record_times": known(json!({"stored": r.get::<_, i64>(5)?, "timed": r.get::<_, i64>(11)?, "first_unix_ms": r.get::<_, Option<i64>>(12)?,
                "last_unix_ms": r.get::<_, Option<i64>>(13)?}))}))
    })?;
    Ok(json!({"sessions": rows.collect::<rusqlite::Result<Vec<_>>>()?}))
}

/// `collectors tools`: per session (the rollout's own `session_meta.id`), its
/// bound attempts, A6 tool calls and exec items, and the A8 MCP calls, agent
/// items and aborted turns, metadata only. `tool_calls` and `exec_items` are
/// `unavailable` with `predates_collection` when the sidecar has no A6 tables
/// (ingest stream < 6, read without migrating), and with `pending_reread`
/// while a rollout of the session read before A6 waits to be read again; the
/// A8 lists (and a tool call's `namespace`) likewise for ingest 0008. `[]` is
/// an observed session without such activity. Read-only.
fn tools(project: &Path) -> Result<Value> {
    let Some(db) = super::sidecar::read(project)? else { return Ok(json!({"sessions": unavailable("collection_not_run")})) };
    let a6 = exists(&db, "codex_tool_sources")?;
    let a8 = exists(&db, "rollout_forks")?;
    let waiting = |marker: &str| format!("EXISTS(SELECT 1 FROM rollout_sources p WHERE p.session_id=s.session_id AND p.path_digest NOT IN (SELECT path_digest FROM {marker}))");
    let pending = if a6 { waiting("codex_tool_sources") } else { "1".into() };
    let pending8 = if a8 { waiting("rollout_forks") } else { "1".into() };
    let mut stmt = db.prepare(&format!("SELECT s.session_id,{pending},{pending8},(SELECT json_group_array(DISTINCT a.attempt_id) FROM (SELECT attempt_id FROM rollout_sources b
        WHERE b.session_id=s.session_id AND b.binding='bound' ORDER BY attempt_id) a) FROM (SELECT DISTINCT session_id FROM rollout_sources) s ORDER BY s.session_id"))?;
    let sessions: Vec<(String, bool, bool, String)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
    let namespace = if a8 { "(SELECT n.namespace FROM codex_tool_namespaces n WHERE n.session_id=c.session_id AND n.call_id=c.call_id)" } else { "NULL" };
    let mut out = Vec::new();
    for (session, waiting, waiting8, attempts) in sessions {
        let reason = if !a6 { Some("predates_collection") } else if waiting { Some("pending_reread") } else { None };
        let reason8 = if !a8 { Some("predates_collection") } else if waiting8 { Some("pending_reread") } else { None };
        let (calls, items) = match reason {
            Some(reason) => (unavailable(reason), unavailable(reason)),
            None => {
                let calls = db.prepare(&format!("SELECT call_id,call_kind,name,status,turn_id,called_unix_ms,output_kind,output_unix_ms,{namespace} FROM codex_tool_calls c
                    WHERE session_id=?1 ORDER BY called_unix_ms IS NULL,called_unix_ms,call_id"))?
                    .query_map([&session], |r| {
                        let (called, output): (Option<i64>, Option<i64>) = (r.get(5)?, r.get(7)?);
                        let namespace = match reason8 { Some(reason) => unavailable(reason), None => json!(r.get::<_, Option<String>>(8)?) };
                        Ok(json!({"call_id": r.get::<_, String>(0)?, "call_kind": r.get::<_, Option<String>>(1)?, "name": r.get::<_, Option<String>>(2)?,
                            "namespace": namespace, "status": r.get::<_, Option<String>>(3)?, "turn_id": r.get::<_, Option<String>>(4)?, "called_unix_ms": called,
                            "output_kind": r.get::<_, Option<String>>(6)?, "output_unix_ms": output, "call_to_output_ms": called.zip(output).map(|(c, o)| o - c)}))
                    })?.collect::<rusqlite::Result<Vec<_>>>()?;
                let items = db.prepare("SELECT item_id,thread_id,turn_id,status,source,exit_code,startup_duration_secs,startup_duration_nanos,completed_unix_ms
                    FROM codex_exec_items WHERE session_id=?1 ORDER BY completed_unix_ms IS NULL,completed_unix_ms,item_id")?
                    .query_map([&session], |r| Ok(json!({"item_id": r.get::<_, String>(0)?, "thread_id": r.get::<_, Option<String>>(1)?,
                        "turn_id": r.get::<_, Option<String>>(2)?, "status": r.get::<_, Option<String>>(3)?, "source": r.get::<_, Option<String>>(4)?,
                        "exit_code": r.get::<_, Option<i64>>(5)?, "startup_duration": {"secs": r.get::<_, Option<i64>>(6)?, "nanos": r.get::<_, Option<i64>>(7)?},
                        "completed_unix_ms": r.get::<_, Option<i64>>(8)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
                (json!(calls), json!(items))
            }
        };
        let (mcp, agents, aborts) = match reason8 {
            Some(reason) => (unavailable(reason), unavailable(reason), unavailable(reason)),
            None => {
                let flag = |r: &rusqlite::Row, i: usize| r.get::<_, Option<i64>>(i).map(|v| json!(v.map(|v| v != 0)));
                let mcp = db.prepare("SELECT item_id,thread_id,turn_id,server,tool,status,read_only_hint,is_error,duration_secs,duration_nanos,completed_unix_ms
                    FROM codex_mcp_calls WHERE session_id=?1 ORDER BY completed_unix_ms IS NULL,completed_unix_ms,item_id")?
                    .query_map([&session], |r| Ok(json!({"item_id": r.get::<_, String>(0)?, "thread_id": r.get::<_, Option<String>>(1)?,
                        "turn_id": r.get::<_, Option<String>>(2)?, "server": r.get::<_, Option<String>>(3)?, "tool": r.get::<_, Option<String>>(4)?,
                        "status": r.get::<_, Option<String>>(5)?, "read_only_hint": flag(r, 6)?, "is_error": flag(r, 7)?,
                        "duration": {"secs": r.get::<_, Option<i64>>(8)?, "nanos": r.get::<_, Option<i64>>(9)?}, "completed_unix_ms": r.get::<_, Option<i64>>(10)?})))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let agents = db.prepare("SELECT item_id,item_type,thread_id,turn_id,status,agent_thread_id,sender_thread_id,receiver_thread_ids,completed_unix_ms
                    FROM codex_agent_items WHERE session_id=?1 ORDER BY completed_unix_ms IS NULL,completed_unix_ms,item_type,item_id")?
                    .query_map([&session], |r| {
                        let receivers: Option<String> = r.get(7)?;
                        Ok(json!({"item_id": r.get::<_, String>(0)?, "type": r.get::<_, String>(1)?, "thread_id": r.get::<_, Option<String>>(2)?,
                            "turn_id": r.get::<_, Option<String>>(3)?, "status": r.get::<_, Option<String>>(4)?, "agent_thread_id": r.get::<_, Option<String>>(5)?,
                            "sender_thread_id": r.get::<_, Option<String>>(6)?,
                            "receiver_thread_ids": receivers.and_then(|text| serde_json::from_str::<Value>(&text).ok()), "completed_unix_ms": r.get::<_, Option<i64>>(8)?}))
                    })?.collect::<rusqlite::Result<Vec<_>>>()?;
                let aborts = db.prepare("SELECT turn_id,reason,duration_ms,aborted_unix_ms FROM codex_turn_aborts WHERE session_id=?1
                    ORDER BY aborted_unix_ms IS NULL,aborted_unix_ms,turn_id")?
                    .query_map([&session], |r| Ok(json!({"turn_id": r.get::<_, String>(0)?, "reason": r.get::<_, Option<String>>(1)?,
                        "duration_ms": r.get::<_, Option<i64>>(2)?, "aborted_unix_ms": r.get::<_, Option<i64>>(3)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
                (json!(mcp), json!(agents), json!(aborts))
            }
        };
        let attempts: Value = serde_json::from_str(&attempts)?;
        out.push(json!({"session_id": session, "attempt_ids": attempts, "tool_calls": calls, "exec_items": items, "mcp_calls": mcp, "agent_items": agents,
            "turn_aborts": aborts}));
    }
    Ok(json!({"sessions": out}))
}

fn tools_text(value: &Value) -> String {
    let Some(sessions) = value["sessions"].as_array() else { return format!("tools {}\n", value["sessions"]["reason"].as_str().unwrap_or("-")) };
    let word = |v: &Value| match v { Value::String(s) => s.clone(), Value::Null => "-".into(), other => other.to_string() };
    let mut out = String::new();
    for s in sessions {
        let attempts: Vec<String> = s["attempt_ids"].as_array().into_iter().flatten().map(word).collect();
        out += &format!("{} attempts={}\n", word(&s["session_id"]), if attempts.is_empty() { "-".into() } else { attempts.join(",") });
        for (key, label) in [("tool_calls", "call"), ("exec_items", "exec"), ("mcp_calls", "mcp"), ("agent_items", "agent"), ("turn_aborts", "abort")] {
            let Some(rows) = s[key].as_array() else {
                out += &format!("  {key} unavailable {}\n", word(&s[key]["reason"]));
                continue;
            };
            for r in rows {
                out += &match label {
                    // A8: a namespace only where one is reported, so A6 lines keep their shape.
                    "call" => format!("  call {} {} name={}{} status={} turn={} called={} output={} call_to_output_ms={}\n", word(&r["call_id"]), word(&r["call_kind"]),
                        word(&r["name"]), if r["namespace"].is_string() { format!(" namespace={}", word(&r["namespace"])) } else { String::new() },
                        word(&r["status"]), word(&r["turn_id"]), word(&r["called_unix_ms"]), word(&r["output_unix_ms"]), word(&r["call_to_output_ms"])),
                    "exec" => format!("  exec {} status={} source={} exit_code={} turn={} completed={}\n", word(&r["item_id"]), word(&r["status"]), word(&r["source"]),
                        word(&r["exit_code"]), word(&r["turn_id"]), word(&r["completed_unix_ms"])),
                    "mcp" => format!("  mcp {} server={} tool={} status={} read_only_hint={} is_error={} turn={} completed={}\n", word(&r["item_id"]), word(&r["server"]),
                        word(&r["tool"]), word(&r["status"]), word(&r["read_only_hint"]), word(&r["is_error"]), word(&r["turn_id"]), word(&r["completed_unix_ms"])),
                    "agent" => {
                        let receivers: Vec<String> = r["receiver_thread_ids"].as_array().into_iter().flatten().map(word).collect();
                        format!("  agent {} {} status={} agent_thread={} sender={} receivers={} turn={} completed={}\n", word(&r["item_id"]), word(&r["type"]),
                            word(&r["status"]), word(&r["agent_thread_id"]), word(&r["sender_thread_id"]),
                            if r["receiver_thread_ids"].is_array() { receivers.join(",") } else { "-".into() }, word(&r["turn_id"]), word(&r["completed_unix_ms"]))
                    }
                    _ => format!("  abort {} reason={} duration_ms={} aborted={}\n", word(&r["turn_id"]), word(&r["reason"]), word(&r["duration_ms"]),
                        word(&r["aborted_unix_ms"])),
                };
            }
        }
    }
    out
}

fn unavailable(reason: &str) -> Value {
    json!({"status": "unavailable", "reason": reason})
}

/// Metrics merged into `telemetry <slug> report` (`super::metrics::report`).
pub fn metrics(_project: &Path, _since: Option<i64>) -> Result<BTreeMap<String, Value>> { Ok(BTreeMap::new()) }

/// Ticker telemetry pass, after the Codex collect, within `budget`. Writes only the sidecar.
pub fn tick(_project: &Path, _budget: super::codex::Budget) -> Result<()> { Ok(()) }

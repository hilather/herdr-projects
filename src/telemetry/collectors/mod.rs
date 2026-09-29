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
    include_str!("../../../migrations/telemetry/ingest/0007_codex_followups.sql")];

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
    /// is still open or is missing. Read-only.
    Sessions,
    /// Per session: the A6 tool call metadata (call id, tool name, status,
    /// turn, call and output times) and exec items (id, status, source, exit
    /// code, startup duration). Metadata only, never a tool's input, arguments,
    /// output or command. Read-only.
    Tools {
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
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
        Command::Tools { json: false } => return Ok(tools_text(&tools(project)?)),
        Command::Tools { json: true } => tools(project)?,
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
        absent("session_meta", "forked_from_ordinal_exclusive", "not_collected"),
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
        field("item_completed", "item.id", Live, Some("command_execution_only")),
        field("item_completed", "item.status", Live, Some("command_execution_only")),
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
        // run2: an MCP call is an `exec` custom tool call plus an `McpToolCall`
        // item; the item's own fields are not collected (a §7 revision is
        // proposed in codex-live-0.154.0-run2.md). No typed MCP event exists.
        absent("item_completed", "item.server", "not_collected"),
        absent("item_completed", "item.tool", "not_collected"),
        absent("item_completed", "item.readOnlyHint", "not_collected"),
        absent("item_completed", "item.arguments", "content_forbidden"),
        absent("item_completed", "item.result", "content_forbidden"),
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
    for kind in ["session_meta", "turn_context", "task_started", "token_usage_record", "token_count", "task_complete", "custom_tool_call", "function_call",
        "custom_tool_call_output", "function_call_output", "item_completed"] {
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

/// `collectors sessions`: per rollout source, its A4 metadata, A5 thread
/// lineage, usage record times and A7 subagent detail and final event. A
/// field is `unavailable` with `predates_collection` when the sidecar has no
/// table for it (ingest stream < 4, < 5 for `thread`, < 7 for
/// `subagent.detail` and `final_event`, read without migrating), and with
/// `pending_reread` while a rollout read before it waits to be read again.
/// `null` is a value the rollout did not report; `thread.session_id` is also
/// `null` when it equals `session_id`. `final_event.state`: `no_turn`,
/// `complete` (the last turn's `task_complete` was read), `open` (not yet), or
/// `missing` (a pending `final_event_missing` coverage gap: idle past the
/// threshold without it). Read-only.
fn sessions(project: &Path) -> Result<Value> {
    let Some(db) = super::sidecar::read(project)? else { return Ok(json!({"sessions": unavailable("collection_not_run")})) };
    let a4 = exists(&db, "rollout_metadata")?;
    let a5 = exists(&db, "rollout_threads")?;
    let a7 = exists(&db, "rollout_ingest_state")?;
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
    let mut stmt = db.prepare(&format!("SELECT s.session_id,s.path_digest,s.binding,s.attempt_id,s.records,
        (SELECT count(*) FROM codex_usage u WHERE u.path_digest=s.path_digest),{columns},{threads},{followups} FROM rollout_sources s {join}
        ORDER BY s.session_id,s.path_digest"))?;
    let rows = stmt.query_map([], |r| {
        let waiting = |table: bool, read: usize| Ok::<_, rusqlite::Error>(match (table, r.get::<_, bool>(read)?) {
            (false, _) => Some("predates_collection"), (true, false) => Some("pending_reread"), _ => None });
        let (pending, thread_pending) = (waiting(a4, 14)?, waiting(a5, 18)?);
        let (detail_pending, final_pending) = (waiting(a7, 20)?, waiting(a7, 24)?);
        let known = |value: Value| pending.map_or(value, unavailable);
        let detail = detail_pending.map_or(json!(r.get::<_, Option<String>>(19)?), unavailable);
        let final_event = match final_pending {
            Some(reason) => unavailable(reason),
            None => {
                let (opened, turn, completed, missing): (Option<i64>, Option<String>, bool, bool) = (r.get(21)?, r.get(22)?, r.get(23)?, r.get(25)?);
                let state = match (opened, completed, missing) { (None, ..) => "no_turn", (_, true, _) => "complete", (_, _, true) => "missing", _ => "open" };
                json!({"state": state, "turn_id": turn})
            }
        };
        let text = |i: usize| r.get::<_, Option<String>>(i).map(|v| known(json!(v)));
        let thread = json!({"parent_thread_id": r.get::<_, Option<String>>(15)?, "session_id": r.get::<_, Option<String>>(16)?,
            "source": r.get::<_, Option<String>>(17)?});
        Ok(json!({"session_id": r.get::<_, String>(0)?, "path_digest": r.get::<_, String>(1)?, "binding": r.get::<_, String>(2)?,
            "attempt_id": r.get::<_, Option<String>>(3)?, "records": r.get::<_, i64>(4)?, "model_provider": text(6)?, "forked_from_id": text(7)?,
            "subagent": known(json!({"kind": r.get::<_, Option<String>>(8)?, "detail": detail, "parent_thread_id": r.get::<_, Option<String>>(9)?,
                "depth": r.get::<_, Option<i64>>(10)?})), "final_event": final_event,
            "thread": thread_pending.map_or(thread, unavailable),
            "record_times": known(json!({"stored": r.get::<_, i64>(5)?, "timed": r.get::<_, i64>(11)?, "first_unix_ms": r.get::<_, Option<i64>>(12)?,
                "last_unix_ms": r.get::<_, Option<i64>>(13)?}))}))
    })?;
    Ok(json!({"sessions": rows.collect::<rusqlite::Result<Vec<_>>>()?}))
}

/// `collectors tools`: per session (the rollout's own `session_meta.id`), its
/// bound attempts, A6 tool calls and exec items, metadata only. `tool_calls`
/// and `exec_items` are `unavailable` with `predates_collection` when the
/// sidecar has no A6 tables (ingest stream < 6, read without migrating), and
/// with `pending_reread` while a rollout of the session read before A6 waits
/// to be read again. `[]` is an observed session without tool activity.
/// Read-only.
fn tools(project: &Path) -> Result<Value> {
    let Some(db) = super::sidecar::read(project)? else { return Ok(json!({"sessions": unavailable("collection_not_run")})) };
    let a6 = exists(&db, "codex_tool_sources")?;
    let pending = if a6 { "EXISTS(SELECT 1 FROM rollout_sources p WHERE p.session_id=s.session_id AND p.path_digest NOT IN (SELECT path_digest FROM codex_tool_sources))" } else { "1" };
    let mut stmt = db.prepare(&format!("SELECT s.session_id,{pending},(SELECT json_group_array(DISTINCT a.attempt_id) FROM (SELECT attempt_id FROM rollout_sources b
        WHERE b.session_id=s.session_id AND b.binding='bound' ORDER BY attempt_id) a) FROM (SELECT DISTINCT session_id FROM rollout_sources) s ORDER BY s.session_id"))?;
    let sessions: Vec<(String, bool, String)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::new();
    for (session, waiting, attempts) in sessions {
        let reason = if !a6 { Some("predates_collection") } else if waiting { Some("pending_reread") } else { None };
        let (calls, items) = match reason {
            Some(reason) => (unavailable(reason), unavailable(reason)),
            None => {
                let calls = db.prepare("SELECT call_id,call_kind,name,status,turn_id,called_unix_ms,output_kind,output_unix_ms FROM codex_tool_calls
                    WHERE session_id=?1 ORDER BY called_unix_ms IS NULL,called_unix_ms,call_id")?
                    .query_map([&session], |r| {
                        let (called, output): (Option<i64>, Option<i64>) = (r.get(5)?, r.get(7)?);
                        Ok(json!({"call_id": r.get::<_, String>(0)?, "call_kind": r.get::<_, Option<String>>(1)?, "name": r.get::<_, Option<String>>(2)?,
                            "status": r.get::<_, Option<String>>(3)?, "turn_id": r.get::<_, Option<String>>(4)?, "called_unix_ms": called,
                            "output_kind": r.get::<_, Option<String>>(6)?, "output_unix_ms": output,
                            "call_to_output_ms": called.zip(output).map(|(c, o)| o - c)}))
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
        let attempts: Value = serde_json::from_str(&attempts)?;
        out.push(json!({"session_id": session, "attempt_ids": attempts, "tool_calls": calls, "exec_items": items}));
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
        for (key, label) in [("tool_calls", "call"), ("exec_items", "exec")] {
            let Some(rows) = s[key].as_array() else {
                out += &format!("  {key} unavailable {}\n", word(&s[key]["reason"]));
                continue;
            };
            for r in rows {
                out += &match label {
                    "call" => format!("  call {} {} name={} status={} turn={} called={} output={} call_to_output_ms={}\n", word(&r["call_id"]), word(&r["call_kind"]),
                        word(&r["name"]), word(&r["status"]), word(&r["turn_id"]), word(&r["called_unix_ms"]), word(&r["output_unix_ms"]), word(&r["call_to_output_ms"])),
                    _ => format!("  exec {} status={} source={} exit_code={} turn={} completed={}\n", word(&r["item_id"]), word(&r["status"]), word(&r["source"]),
                        word(&r["exit_code"]), word(&r["turn_id"]), word(&r["completed_unix_ms"])),
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

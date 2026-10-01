//! Tool calls and executions (docs/telemetry/contracts-accounting.md §9; plan
//! TM2.5, doc 07 M16–M18), derived at read time from lane A's A6 metadata
//! (contracts-collection.md A6: `codex_tool_calls`, `codex_exec_items`,
//! `codex_tool_sources`) and A8 metadata (`codex_mcp_calls`,
//! `codex_turn_aborts`, `codex_tool_namespaces`, `codex_agent_items`,
//! `rollout_forks` as the re-read marker), read by SQL only; never any
//! content. Exact tallies and integer wait samples are maintained per session;
//! reads before sync fall back to the same derivation. An MCP call is one call (its carrying `exec`
//! call is matched by turn and time, `inferred`); a call ended by
//! `turn_aborted` is `declined_or_aborted`, never accepted. Codex 0.154.0 writes no approval decision and no execution run
//! time: the accepted stage is only inferred (a B6b `blocked` wait or a
//! guardian review around the call, labelled `inferred`; neither is unknown),
//! M18 is `unavailable` with the reason, and the call → output wall time
//! (approval wait included) is shown apart, per tool and per host, never as M18. Unknown is never 0: a session whose tool metadata was not
//! read yet makes the metrics `unavailable`, and an execution without a
//! known outcome is counted apart, never as a success.
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::unavailable;

/// Exec item statuses certified live: `completed` (codex-live-0.154.0-a4.md §4)
/// and `failed` with a non-zero exit code (codex-live-0.154.0-run2.md §4).
const COMPLETED: &str = "completed";
const FAILED: &str = "failed";
/// The namespace of `spawn_agent` / `wait_agent` calls (run2 §2): not tool executions.
const COLLABORATION: &str = "collaboration";
const NAMES: [(&str, &str); 3] = [("M16", "tool_call_volume"), ("M17", "tool_execution_success"), ("M18", "tool_latency_p95")];

struct Call { id: String, recorded: bool, kind: Option<String>, name: Option<String>, status: Option<String>, turn: Option<String>, called: Option<i64>,
    output: Option<i64> }

struct Exec { turn: Option<String>, status: Option<String>, source: Option<String>, exit: Option<i64>, completed: Option<i64> }

/// One `McpToolCall` item (A8): configuration names, status and error flag.
struct Mcp { turn: Option<String>, server: Option<String>, tool: Option<String>, status: Option<String>, is_error: Option<i64>, completed: Option<i64> }

/// A session's A6 and A8 metadata rows (never content: no such column exists).
#[derive(Default)]
struct Rows {
    calls: Vec<Call>,
    claude_results: Vec<(String, Option<bool>)>,
    opencode_results: Vec<(Option<String>, Option<String>, Option<bool>)>,
    items: Vec<Exec>,
    mcp: Vec<Mcp>,
    /// `turn_id` → `aborted_unix_ms` of each `turn_aborted` (`None`: no line time).
    aborts: BTreeMap<String, Option<i64>>,
    /// `call_id` → `function_call.namespace`.
    namespaces: BTreeMap<String, String>,
    /// `(item_id, agent_thread_id)` of the `SubAgentActivity` items, and the number of `CollabAgentToolCall` items.
    activities: Vec<(String, Option<String>)>,
    collab_items: usize,
}

/// A session with a bound rollout (the rollout's own `session_meta.id`), with
/// the evidence the accepted stage is inferred from: the `blocked` wait spans
/// of its attempts (B6b, §6) and the start times of guardian sessions naming
/// it as their parent; and the execution homes of its rollouts (the host).
struct Session {
    id: String,
    attempts: BTreeSet<String>,
    summary: Option<Tally>,
    tools: std::result::Result<Rows, &'static str>,
    waits: Vec<(i64, i64)>,
    guardians: Vec<i64>,
    homes: BTreeSet<String>,
}

impl Session {
    /// The stage a call reached past `issued` (inferred; never from a typed
    /// decision): a call whose call → output interval overlaps a `blocked`
    /// wait of the same attempt was routed to the human (`human_routed`); a
    /// guardian session of this session started inside it was reviewed
    /// automatically (`auto_review`). Neither (or no output yet): unknown.
    fn accepted(&self, c: &Call) -> Option<&'static str> {
        let (called, output) = (c.called?, c.output?);
        if output < called { return None; }
        let human = self.waits.iter().any(|(opened, last)| *opened <= output && called <= *last);
        let auto = self.guardians.iter().any(|at| called <= *at && *at <= output);
        match (human, auto) {
            (true, true) => Some("human_routed_and_auto_review"),
            (true, false) => Some("human_routed"),
            (false, true) => Some("auto_review"),
            (false, false) => None,
        }
    }

    /// The host of its calls: the one execution home of its rollouts, else ambiguous.
    fn home(&self) -> Option<&str> { if self.homes.len() == 1 { self.homes.first().map(String::as_str) } else { None } }
}

/// Outcome class of one exec item (M17): `completed` with an exit code
/// (0 succeeded, else failed), `failed` with a non-zero exit code (failed).
fn outcome(e: &Exec) -> std::result::Result<bool, &'static str> {
    match (e.status.as_deref(), e.exit) {
        (None, _) => Err("status_unreported"),
        (Some(COMPLETED), Some(code)) => Ok(code == 0),
        (Some(COMPLETED), None) => Err("exit_code_unknown"),
        (Some(FAILED), Some(code)) if code != 0 => Ok(false),
        (Some(_), _) => Err("status_not_certified"),
    }
}

/// Outcome class of one MCP call (M17): `is_error` 1 failed; `is_error` 0
/// with status `completed` succeeded.
fn mcp_outcome(m: &Mcp) -> std::result::Result<bool, &'static str> {
    match (m.is_error, m.status.as_deref()) {
        (Some(1), _) => Ok(false),
        (None, _) => Err("is_error_unreported"),
        (Some(_), Some(COMPLETED)) => Ok(true),
        (Some(_), None) => Err("status_unreported"),
        (Some(_), Some(_)) => Err("status_not_certified"),
    }
}

/// The carrying `exec` call of each MCP item, in item order (`inferred`: the
/// item id is not a call id): in the same session and turn, the latest
/// recorded `custom_tool_call` named `exec` at or before the item's
/// completion whose output, if any, is not before it, not already carrying
/// an earlier item. Items are taken in completion order.
fn carriers<'a>(calls: &'a [Call], mcp: &[Mcp]) -> Vec<Option<&'a Call>> {
    let mut used = BTreeSet::<&str>::new();
    let mut order: Vec<usize> = (0..mcp.len()).collect();
    order.sort_by_key(|i| (mcp[*i].completed.is_none(), mcp[*i].completed));
    let mut out = vec![None; mcp.len()];
    for i in order {
        let m = &mcp[i];
        let (Some(turn), Some(at)) = (m.turn.as_ref(), m.completed) else { continue };
        let carrier = calls.iter().filter(|c| c.recorded && c.kind.as_deref() == Some("custom_tool_call") && c.name.as_deref() == Some("exec")
            && c.turn.as_ref() == Some(turn) && c.called.is_some_and(|c| c <= at) && c.output.is_none_or(|o| o >= at) && !used.contains(c.id.as_str()))
            .max_by_key(|c| (c.called, c.id.as_str()));
        if let Some(c) = carrier { used.insert(c.id.as_str()); }
        out[i] = carrier;
    }
    out
}

/// Whether a call ended with its turn's `turn_aborted` (M16), with the basis:
/// its output is the turn's last at or before the abort (the declined
/// approval of run2 §4), or it has no output and was made at or before it.
/// Other calls of an aborted turn keep their stage.
fn aborted(rows: &Rows, c: &Call) -> Option<&'static str> {
    let at = (*rows.aborts.get(c.turn.as_ref()?)?)?;
    match (c.called, c.output) {
        (Some(called), None) if called <= at => Some("no_output_before_abort"),
        (_, Some(output)) if output <= at => {
            let last = rows.calls.iter().filter(|o| o.recorded && o.turn == c.turn).filter_map(|o| o.output).filter(|o| *o <= at).max();
            (last == Some(output)).then_some("last_output_before_abort")
        }
        _ => None,
    }
}

/// The call an exec item is attributed to (`inferred`: no shared key): in the
/// same session and turn, the latest call at or before the item's completion
/// whose output, if any, is not before it.
fn attribute<'a>(calls: &'a [Call], e: &Exec) -> Option<&'a Call> {
    let (turn, at) = (e.turn.as_ref()?, e.completed?);
    calls.iter().filter(|c| c.recorded && c.turn.as_ref() == Some(turn) && c.called.is_some_and(|c| c <= at) && c.output.is_none_or(|o| o >= at))
        .max_by_key(|c| (c.called, c.id.as_str()))
}

/// Lane A's A8 tables (ingest 0008): without them the sidecar predates A8.
const A8_TABLES: [&str; 5] = ["rollout_forks", "codex_mcp_calls", "codex_turn_aborts", "codex_tool_namespaces", "codex_agent_items"];

fn exists(db: &Connection, table: &str) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)", [table], |r| r.get(0))?)
}

/// Sessions with a bound rollout started in the window, their tool rows or
/// why they are unavailable, and the sessions excluded (by reason).
fn sessions(db: &Connection, since: Option<i64>, blocked: &BTreeMap<String, Vec<(i64, i64)>>, scoped: bool, summaries: bool) -> Result<(Vec<Session>, BTreeMap<String, usize>)> {
    let a6 = exists(db, "codex_tool_sources")? && exists(db, "codex_tool_calls")? && exists(db, "codex_exec_items")?;
    let a8 = A8_TABLES.iter().try_fold(true, |all, t| Ok::<_, anyhow::Error>(all && exists(db, t)?))?;
    let read = if a6 { "EXISTS(SELECT 1 FROM codex_tool_sources c WHERE c.path_digest=s.path_digest)" } else { "0" };
    // A source without a `rollout_forks` row was read before A8 and waits for its re-read.
    let reread = if a8 { "EXISTS(SELECT 1 FROM rollout_forks k WHERE k.path_digest=s.path_digest)" } else { "0" };
    type Source = (String, String, Option<String>, String, Option<i64>, bool, String, bool);
    let scope = if scoped { " WHERE s.session_id IN (SELECT session_id FROM accounting_selected)" } else { "" };
    let sources: Vec<Source> = db.prepare(&format!("SELECT s.session_id,s.binding,s.attempt_id,s.cli_version,s.session_unix_ms,{read},s.home_digest,{reread}
        FROM rollout_sources s{scope} ORDER BY s.session_id,s.path_digest"))?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)))?.collect::<rusqlite::Result<_>>()?;
    let guardians = guardians(db)?;
    // DG4b's table is present on Codex-only stores too. One snapshot-wide
    // emptiness check avoids probing it separately for every selected session.
    let claude_results = exists(db, "claude_tool_results")?
        && db.query_row("SELECT EXISTS(SELECT 1 FROM claude_tool_results)", [], |r| r.get::<_, bool>(0))?;
    let mut grouped = BTreeMap::<&str, Vec<&Source>>::new();
    for s in &sources { grouped.entry(s.0.as_str()).or_default().push(s); }
    let (mut out, mut excluded) = (Vec::new(), BTreeMap::<String, usize>::new());
    for (id, sources) in grouped {
        let start = sources.iter().filter_map(|s| s.4).min();
        if since.is_some_and(|since| start.is_none_or(|at| at < since)) { continue; }
        let bound: Vec<&&Source> = sources.iter().filter(|s| s.1 == "bound").collect();
        if bound.is_empty() { *excluded.entry(sources[0].1.clone()).or_default() += 1; continue; }
        if bound.iter().any(|s| !crate::telemetry::codex::accepted_version(&s.3)) { *excluded.entry("cli_version_uncertified".to_owned()).or_default() += 1; continue; }
        let attempts: BTreeSet<String> = bound.iter().filter_map(|s| s.2.clone()).collect();
        let summary = if summaries {
            let text: Option<String> = db.query_row("SELECT tally FROM accounting_tool_summary WHERE session_id=?1", [id], |r| r.get(0)).optional()?;
            text.map(|text| serde_json::from_str::<Tally>(&text)).transpose()?
        } else { None };
        let tools = if !a6 { Err("predates_collection") } else if sources.iter().any(|s| !s.5) { Err("pending_reread") }
            else if !a8 { Err("predates_collection") } else if sources.iter().any(|s| !s.7) { Err("pending_reread") } else { Ok(if summary.is_some() { Rows::default() } else { rows(db, id, claude_results)? }) };
        let waits = attempts.iter().filter_map(|a| blocked.get(a)).flatten().copied().collect();
        let guardians = guardians.get(id).cloned().unwrap_or_default();
        let homes = sources.iter().map(|s| s.6.clone()).collect();
        out.push(Session { id: id.to_owned(), attempts, tools, summary, waits, guardians, homes });
    }
    Ok((out, excluded))
}

/// Start times of guardian sessions per parent session id, from native
/// evidence only (§3): a rollout whose A5 `thread_source` is `guardian_review`
/// or whose A4 `subagent_kind` is `review`, naming a parent in
/// `subagent_parent_thread_id`, else `rollout_threads.parent_thread_id`, other
/// than itself. A sidecar without the A4/A5 tables has none.
fn guardians(db: &Connection) -> Result<BTreeMap<String, Vec<i64>>> {
    let (a4, a5) = (exists(db, "rollout_metadata")?, exists(db, "rollout_threads")?);
    if !a4 && !a5 { return Ok(BTreeMap::new()); }
    let (m, t) = (if a4 { "LEFT JOIN rollout_metadata m ON m.path_digest=s.path_digest" } else { "" },
        if a5 { "LEFT JOIN rollout_threads t ON t.path_digest=s.path_digest" } else { "" });
    let parent = match (a4, a5) { (true, true) => "coalesce(m.subagent_parent_thread_id,t.parent_thread_id)", (true, false) => "m.subagent_parent_thread_id",
        _ => "t.parent_thread_id" };
    let guardian = match (a4, a5) { (true, true) => "(t.thread_source='guardian_review' OR m.subagent_kind='review')", (true, false) => "m.subagent_kind='review'",
        _ => "t.thread_source='guardian_review'" };
    let mut out = BTreeMap::<String, Vec<i64>>::new();
    let mut stmt = db.prepare(&format!("SELECT {parent},s.session_unix_ms FROM rollout_sources s {m} {t}
        WHERE {guardian} AND s.session_unix_ms IS NOT NULL AND {parent} IS NOT NULL AND {parent}<>s.session_id ORDER BY s.path_digest"))?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
        let (parent, at) = row?;
        out.entry(parent).or_default().push(at);
    }
    Ok(out)
}

/// The A6 and A8 metadata rows of one session (never content: no such column exists).
fn rows(db: &Connection, session: &str, claude_results: bool) -> Result<Rows> {
    let calls = db.prepare("SELECT call_id,call_kind IS NOT NULL,call_kind,name,status,turn_id,called_unix_ms,output_unix_ms FROM codex_tool_calls WHERE session_id=?1
        ORDER BY called_unix_ms IS NULL,called_unix_ms,call_id")?
        .query_map([session], |r| Ok(Call { id: r.get(0)?, recorded: r.get(1)?, kind: r.get(2)?, name: r.get(3)?, status: r.get(4)?, turn: r.get(5)?,
            called: r.get(6)?, output: r.get(7)? }))?
        .collect::<rusqlite::Result<_>>()?;
    let items = db.prepare("SELECT turn_id,status,source,exit_code,completed_unix_ms FROM codex_exec_items WHERE session_id=?1
        ORDER BY completed_unix_ms IS NULL,completed_unix_ms,item_id")?
        .query_map([session], |r| Ok(Exec { turn: r.get(0)?, status: r.get(1)?, source: r.get(2)?, exit: r.get(3)?, completed: r.get(4)? }))?
        .collect::<rusqlite::Result<_>>()?;
    let mcp = db.prepare("SELECT turn_id,server,tool,status,is_error,completed_unix_ms FROM codex_mcp_calls WHERE session_id=?1
        ORDER BY completed_unix_ms IS NULL,completed_unix_ms,item_id")?
        .query_map([session], |r| Ok(Mcp { turn: r.get(0)?, server: r.get(1)?, tool: r.get(2)?, status: r.get(3)?, is_error: r.get(4)?, completed: r.get(5)? }))?
        .collect::<rusqlite::Result<_>>()?;
    let aborts = db.prepare("SELECT turn_id,aborted_unix_ms FROM codex_turn_aborts WHERE session_id=?1")?
        .query_map([session], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    let namespaces = db.prepare("SELECT call_id,namespace FROM codex_tool_namespaces WHERE session_id=?1")?
        .query_map([session], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    let activities = db.prepare("SELECT item_id,agent_thread_id FROM codex_agent_items WHERE session_id=?1 AND item_type='SubAgentActivity' ORDER BY item_id")?
        .query_map([session], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    let collab_items = db.query_row("SELECT count(*) FROM codex_agent_items WHERE session_id=?1 AND item_type='CollabAgentToolCall'", [session], |r| r.get(0))?;
    let claude_results = if claude_results {
        db.prepare("SELECT call_id,is_error FROM claude_tool_results WHERE session_id=?1 ORDER BY call_id")?
            .query_map([session], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
    } else { Vec::new() };
    let opencode_results = if exists(db, "opencode_tools")? {
        db.prepare("SELECT tool,status,is_error FROM opencode_tools WHERE session_id=?1 ORDER BY part_id")?
            .query_map([session], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<rusqlite::Result<_>>()?
    } else { Vec::new() };
    Ok(Rows { opencode_results, claude_results, calls, items, mcp, aborts, namespaces, activities, collab_items })
}

/// Counts over observed sessions.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Tally {
    issued: usize,
    by_name: BTreeMap<String, usize>,
    name_unreported: usize,
    by_status: BTreeMap<String, usize>,
    status_unreported: usize,
    by_namespace: BTreeMap<String, usize>,
    without_output: usize,
    outputs_without_call: usize,
    executed: usize,
    by_source: BTreeMap<String, usize>,
    source_unreported: usize,
    attributed: BTreeMap<String, usize>,
    attributed_name_unreported: usize,
    unattributed: usize,
    succeeded: usize,
    failed: usize,
    opencode_executed: usize,
    opencode_succeeded: usize,
    opencode_failed: usize,
    #[serde(default)]
    claude_sidechain_turns: usize,
    claude_executed: usize,
    claude_succeeded: usize,
    claude_failed: usize,
    claude_unknown: BTreeMap<String, usize>,
    unknown: BTreeMap<String, usize>,
    /// MCP calls (A8): per server and tool, their carrying `exec` call, outcome.
    mcp: usize,
    mcp_by_server: BTreeMap<String, BTreeMap<String, usize>>,
    mcp_unnamed: usize,
    mcp_carried: usize,
    mcp_without_call: usize,
    mcp_succeeded: usize,
    mcp_failed: usize,
    mcp_unknown: BTreeMap<String, usize>,
    /// The carrying call's call → output wall time per MCP server and tool (`None`: unreported).
    mcp_waits: Vec<(Option<String>, Option<String>, i64)>,
    /// Collaboration calls (`spawn_agent`, `wait_agent`), the agent threads a
    /// spawn call started (its `SubAgentActivity` item id is the call id) and
    /// the `CollabAgentToolCall` items.
    spawned: BTreeSet<String>,
    collab_items: usize,
    /// call → output wall time per tool name (`None`: name unreported) and host (`None`: ambiguous).
    waits: Vec<(Option<String>, Option<String>, i64)>,
    negative: usize,
    /// Issued calls past `issued`, by inferred basis; the rest are unknown.
    accepted: BTreeMap<String, usize>,
    accepted_unknown: usize,
    /// Issued calls ended by their turn's `turn_aborted`, by basis: neither accepted nor unknown.
    aborted: BTreeMap<String, usize>,
}

impl Tally {
    fn merge(&mut self, other: &Tally) {
        macro_rules! counts { ($($field:ident),*) => { $(self.$field += other.$field;)* }; }
        macro_rules! maps { ($($field:ident),*) => { $(for (key, count) in &other.$field { *self.$field.entry(key.clone()).or_default() += count; })* }; }
        counts!(issued,name_unreported,status_unreported,without_output,outputs_without_call,executed,source_unreported,
            attributed_name_unreported,unattributed,succeeded,failed,mcp,mcp_unnamed,mcp_carried,mcp_without_call,mcp_succeeded,mcp_failed,
            collab_items,negative,accepted_unknown,claude_executed,claude_succeeded,claude_failed,claude_sidechain_turns);
        maps!(by_name,by_status,by_namespace,by_source,attributed,unknown,mcp_unknown,accepted,aborted,claude_unknown);
        for (server, tools) in &other.mcp_by_server {
            for (tool, count) in tools { *self.mcp_by_server.entry(server.clone()).or_default().entry(tool.clone()).or_default() += count; }
        }
        self.spawned.extend(other.spawned.iter().cloned());
        self.waits.extend(other.waits.iter().cloned());
        self.mcp_waits.extend(other.mcp_waits.iter().cloned());
    }

    fn add(&mut self, s: &Session, rows: &Rows) {
        if let Some(summary) = &s.summary { self.merge(summary); return; }
        if s.id.starts_with("claude-code:") { self.claude_sidechain_turns += rows.activities.len(); }
        let (calls, items) = (&rows.calls, &rows.items);
        let count = |map: &mut BTreeMap<String, usize>, missing: &mut usize, key: &Option<String>| match key {
            Some(key) => *map.entry(key.clone()).or_default() += 1,
            None => *missing += 1,
        };
        for c in calls {
            if !c.recorded { self.outputs_without_call += 1; continue; }
            self.issued += 1;
            count(&mut self.by_name, &mut self.name_unreported, &c.name);
            count(&mut self.by_status, &mut self.status_unreported, &c.status);
            if let Some(namespace) = rows.namespaces.get(&c.id) { *self.by_namespace.entry(namespace.clone()).or_default() += 1; }
            match (aborted(rows, c), s.accepted(c)) {
                (Some(basis), _) => *self.aborted.entry(basis.to_owned()).or_default() += 1,
                (None, Some(basis)) => *self.accepted.entry(basis.to_owned()).or_default() += 1,
                (None, None) => self.accepted_unknown += 1,
            }
            match (c.called, c.output) {
                (_, None) => self.without_output += 1,
                (Some(called), Some(output)) if output >= called => self.waits.push((c.name.clone(), s.home().map(str::to_owned), output - called)),
                (Some(_), Some(_)) => self.negative += 1,
                (None, Some(_)) => {}
            }
        }
        for (name, status, error) in &rows.opencode_results {
            self.issued += 1;
            count(&mut self.by_name, &mut self.name_unreported, name);
            count(&mut self.by_status, &mut self.status_unreported, status);
            self.accepted_unknown += 1;
            if let Some(error) = error {
                self.executed += 1;
                self.opencode_executed += 1;
                *self.by_source.entry("opencode".to_owned()).or_default() += 1;
                if *error { self.failed += 1; self.opencode_failed += 1; }
                else { self.succeeded += 1; self.opencode_succeeded += 1; }
            } else { self.without_output += 1; }
        }
        for (id, error) in &rows.claude_results {
            self.executed += 1;
            self.claude_executed += 1;
            *self.by_source.entry("claude-code".to_owned()).or_default() += 1;
            if let Some(call) = calls.iter().find(|c| c.id == *id) {
                count(&mut self.attributed, &mut self.attributed_name_unreported, &call.name);
            } else { self.unattributed += 1; }
            match error {
                Some(false) => { self.succeeded += 1; self.claude_succeeded += 1; }
                Some(true) => { self.failed += 1; self.claude_failed += 1; }
                None => {
                    *self.unknown.entry("is_error_unreported".to_owned()).or_default() += 1;
                    *self.claude_unknown.entry("is_error_unreported".to_owned()).or_default() += 1;
                }
            }
        }
        for e in items {
            self.executed += 1;
            count(&mut self.by_source, &mut self.source_unreported, &e.source);
            match attribute(calls, e) {
                Some(call) => count(&mut self.attributed, &mut self.attributed_name_unreported, &call.name),
                None => self.unattributed += 1,
            }
            match outcome(e) {
                Ok(true) => self.succeeded += 1,
                Ok(false) => self.failed += 1,
                Err(reason) => *self.unknown.entry(reason.to_owned()).or_default() += 1,
            }
        }
        for (m, carrier) in rows.mcp.iter().zip(carriers(calls, &rows.mcp)) {
            self.mcp += 1;
            self.executed += 1;
            match (&m.server, &m.tool) {
                (Some(server), Some(tool)) => *self.mcp_by_server.entry(server.clone()).or_default().entry(tool.clone()).or_default() += 1,
                _ => self.mcp_unnamed += 1,
            }
            match carrier {
                Some(c) => {
                    self.mcp_carried += 1;
                    if let (Some(called), Some(output)) = (c.called, c.output) && output >= called {
                        self.mcp_waits.push((m.server.clone(), m.tool.clone(), output - called));
                    }
                }
                // The call itself was not matched: it is issued once, here, and its stage is unknown.
                None => { self.mcp_without_call += 1; self.issued += 1; self.accepted_unknown += 1; }
            }
            match mcp_outcome(m) {
                Ok(true) => self.mcp_succeeded += 1,
                Ok(false) => self.mcp_failed += 1,
                Err(reason) => *self.mcp_unknown.entry(reason.to_owned()).or_default() += 1,
            }
        }
        let collaboration: BTreeSet<&str> = rows.namespaces.iter().filter(|(_, n)| *n == COLLABORATION).map(|(id, _)| id.as_str()).collect();
        self.spawned.extend(rows.activities.iter().filter(|(id, _)| collaboration.contains(id.as_str())).filter_map(|(_, thread)| thread.clone()));
        self.collab_items += rows.collab_items;
    }
}

/// Nearest-rank percentile (`p` in percent) of sorted values.
fn rank(sorted: &[i64], p: usize) -> i64 { sorted[(p * sorted.len()).div_ceil(100) - 1] }

fn distribution(mut values: Vec<i64>) -> Value {
    values.sort_unstable();
    if values.is_empty() { return json!({"samples": 0, "p50_ms": null, "p95_ms": null, "max_ms": null}); }
    json!({"samples": values.len(), "p50_ms": rank(&values, 50), "p95_ms": rank(&values, 95), "max_ms": values[values.len() - 1]})
}

fn metric(id: &str, mut body: Value) -> Value {
    body["definition"] = json!(format!("{id}.tools-v1"));
    body["name"] = json!(NAMES.iter().find(|(n, _)| *n == id).map_or("", |(_, name)| *name));
    body
}

fn unavailable_metrics(reason: &str, coverage: Option<&Value>) -> BTreeMap<String, Value> {
    NAMES.iter().map(|(id, _)| {
        let mut body = json!({"value": unavailable(reason)});
        if let Some(coverage) = coverage { body["coverage"] = coverage.clone(); }
        (id.to_string(), metric(id, body))
    }).collect()
}

const CALL_TO_OUTPUT: &str = "call → output line time of one tool call: includes any approval wait (queue time) and the model-side handling, so it is not \
    the execution's run time and never M18's value";

/// M16–M18 over the observed sessions; `unavailable` (never 0) when a
/// session's tool metadata is not read yet or no bound session exists.
fn computed(list: &[Session], coverage: &Value) -> BTreeMap<String, Value> {
    if let Some(reason) = ["predates_collection", "pending_reread"].into_iter().find(|r| list.iter().any(|s| s.tools.as_ref().err() == Some(r))) {
        return unavailable_metrics(reason, Some(coverage));
    }
    if list.is_empty() { return unavailable_metrics("no_bound_session", Some(coverage)); }
    let mut t = Tally::default();
    for s in list { if let Ok(rows) = &s.tools { t.add(s, rows); } }
    let accepted: usize = t.accepted.values().sum();
    let aborted: usize = t.aborted.values().sum();
    let commands = t.executed - t.mcp - t.claude_executed - t.opencode_executed;
    let mut m16 = json!({"value": {"issued": t.issued, "accepted": {"status": "inferred", "count": accepted, "unknown": t.accepted_unknown,
            "declined_or_aborted": aborted}, "executed": t.executed},
        "issued": {"calls": t.issued, "by_name": t.by_name, "name_unreported": t.name_unreported, "by_status": t.by_status, "status_unreported": t.status_unreported,
            "by_namespace": t.by_namespace, "without_output": t.without_output, "outputs_without_call": t.outputs_without_call, "mcp_without_call": t.mcp_without_call,
            "basis": "distinct (session, call_id) with a recorded call; a call replayed by a resumed rollout or a retried record is one logical call; an MCP call \
                is the exec call carrying it (counted once), or, when none was matched, one more call (`mcp_without_call`, not in by_name or by_status)"},
        "accepted": {"label": "inferred", "calls": accepted, "by_basis": t.accepted, "unknown": t.accepted_unknown,
            "declined_or_aborted": {"calls": aborted, "by_basis": t.aborted},
            "basis": {"human_routed": "the call → output interval overlaps a B6b `blocked` wait (first to last blocked sample) of the same attempt",
                "auto_review": "a guardian session naming this session as its parent (native evidence) started inside the call → output interval, in the call's turn",
                "human_routed_and_auto_review": "both",
                "last_output_before_abort": "declined_or_aborted: the call's turn ended in `turn_aborted` and its output is the turn's last at or before the abort \
                    (live: a declined approval aborts the turn right after the call's output)",
                "no_output_before_abort": "declined_or_aborted: the call's turn ended in `turn_aborted` after the call, which has no output"},
            "detail": "codex 0.154.0 writes no typed approval request or decision (codex-live-0.154.0-a4.md §3): the stage is inferred from a wait or a \
                guardian review around the call, never from a decision; a call with neither (or without an output) stays unknown, never accepted; a call \
                ended by its turn's abort is declined_or_aborted, neither accepted nor unknown; other calls of an aborted turn keep their stage",
            "caveat": "a denied approval that does not abort the turn also ends the wait and yields an output: `accepted` means the approval stage completed, \
                not that it was approved"},
        "executed": {"executions": t.executed, "scope": ["command_execution", "mcp"], "by_scope": {"command_execution": commands, "mcp": t.mcp},
            "by_source": t.by_source, "source_unreported": t.source_unreported,
            "attribution": {"basis": "inferred", "rule": "same session and turn, latest call at or before completion, output not before it",
                "by_call_name": t.attributed, "name_unreported": t.attributed_name_unreported, "unattributed": t.unattributed},
            "basis": "one CommandExecution item per execution instance (a repeated execution is another instance) and one McpToolCall item per MCP call; \
                collaboration calls (spawn_agent, wait_agent) write neither"},
        "mcp": {"calls": t.mcp, "by_server": t.mcp_by_server, "server_or_tool_unreported": t.mcp_unnamed,
            "carrier": {"basis": "inferred", "rule": "same session and turn, latest exec custom_tool_call at or before the item's completion, output not before \
                it, one item per call", "matched": t.mcp_carried, "unmatched": t.mcp_without_call},
            "basis": "one McpToolCall item per MCP call, counted once: in codex 0.154.0 an MCP call runs inside an exec custom_tool_call (code mode), which is \
                the same logical call, never a second one"},
        "collaboration": {"calls": t.by_namespace.get(COLLABORATION).copied().unwrap_or(0), "spawned_threads": t.spawned.len(), "collab_items": t.collab_items,
            "basis": "function calls in namespace `collaboration` (spawn_agent, wait_agent) and the agent threads a spawn call started (its SubAgentActivity \
                item id is the call id); not tool executions"},
        "certified": {"calls": if list.iter().any(|s| s.id.starts_with("claude-code:") || s.id.starts_with("opencode:")) { "fixture" } else { "live" }, "call_status": "live for custom_tool_call, fixture for function_call", "exec_items": if list.iter().any(|s| s.id.starts_with("claude-code:") || s.id.starts_with("opencode:")) { "fixture" } else { "live" }, "mcp_calls": "live",
            "turn_aborts": "live", "namespaces": "live"},
        "coverage": coverage});
    if list.iter().any(|s| s.id.starts_with("claude-code:")) {
        m16["executed"]["scope"].as_array_mut().expect("scope array").push(json!("claude-code"));
        m16["executed"]["by_scope"]["claude-code"] = json!(t.claude_executed);
        m16["executed"]["claude_code_basis"] = json!("fixture: distinct tool-result link; attribution by exact call id; outcome from reported is_error");
        m16["collaboration"]["sidechain_turns"] = json!(t.claude_sidechain_turns);
        m16["collaboration"]["claude_code_basis"] = json!("fixture: sidechain turns attributed to parent session, child identity unreported");
    }
    if list.iter().any(|s| s.id.starts_with("opencode:")) {
        m16["executed"]["scope"].as_array_mut().expect("scope array").push(json!("opencode"));
        m16["executed"]["by_scope"]["opencode"] = json!(t.opencode_executed);
        m16["executed"]["opencode_basis"] = json!("fixture: distinct native tool part id; completed/error states only");
    }
    let mut unknown = t.unknown.clone();
    for (reason, n) in &t.mcp_unknown { *unknown.entry(reason.to_owned()).or_default() += n; }
    let (succeeded, failed) = (t.succeeded + t.mcp_succeeded, t.failed + t.mcp_failed);
    let terminal = succeeded + failed;
    let scope = |succeeded: usize, failed: usize, unknown: &BTreeMap<String, usize>| json!({"succeeded": succeeded, "failed": failed,
        "unknown": {"executions": unknown.values().sum::<usize>(), "by_reason": unknown}});
    let mut command_unknown = t.unknown.clone();
    command_unknown.remove("is_error_unreported");
    let mut m17 = json!({"numerator": succeeded, "denominator": terminal, "succeeded": succeeded, "failed": failed,
        "unknown": {"executions": unknown.values().sum::<usize>(), "by_reason": unknown}, "pending_calls": t.without_output,
        "by_scope": {"command_execution": scope(t.succeeded - t.claude_succeeded - t.opencode_succeeded, t.failed - t.claude_failed - t.opencode_failed, &command_unknown), "mcp": scope(t.mcp_succeeded, t.mcp_failed, &t.mcp_unknown)},
        "cancelled": unavailable("cancellation_not_exposed"), "timed_out": unavailable("timeout_not_exposed"),
        "basis": "command_execution: CommandExecution items with status `completed` and exit code 0 succeeded, `completed` with a non-zero exit code or \
            `failed` with a non-zero exit code (both certified live) failed; a NULL exit code, `failed` without a non-zero exit code or another status is \
            unknown and excluded, never a success. mcp: McpToolCall items with is_error 1 failed, is_error 0 with status `completed` succeeded, else \
            unknown. A pending execution writes no item, so calls without an output are counted apart",
        "coverage": coverage});
    if list.iter().any(|s| s.id.starts_with("claude-code:")) {
        m17["by_scope"]["claude-code"] = scope(t.claude_succeeded, t.claude_failed, &t.claude_unknown);
        m17["certified"] = json!({"claude-code": "fixture"});
        m17["claude_code_basis"] = json!("is_error false succeeds, true fails, absent is unknown; no exit code inferred");
    }
    if list.iter().any(|s| s.id.starts_with("opencode:")) {
        m17["by_scope"]["opencode"] = scope(t.opencode_succeeded, t.opencode_failed, &BTreeMap::new());
        m17["certified"]["opencode"] = json!("fixture");
    }
    if terminal == 0 { m17["value"] = Value::Null; m17["reason"] = json!("empty_denominator"); } else { m17["value"] = json!(format!("{succeeded}/{terminal}")); }
    let (mut by_name, mut by_home) = (BTreeMap::<String, Vec<i64>>::new(), BTreeMap::<String, Vec<i64>>::new());
    let (mut unnamed, mut ambiguous) = (Vec::new(), Vec::new());
    for (name, home, ms) in &t.waits {
        match name { Some(name) => by_name.entry(name.clone()).or_default().push(*ms), None => unnamed.push(*ms) }
        match home { Some(home) => by_home.entry(home.clone()).or_default().push(*ms), None => ambiguous.push(*ms) }
    }
    let mut wall = distribution(t.waits.iter().map(|(_, _, ms)| *ms).collect());
    wall["by_name"] = json!(by_name.into_iter().map(|(name, values)| (name, distribution(values))).collect::<BTreeMap<_, _>>());
    wall["name_unreported"] = distribution(unnamed);
    wall["by_home"] = json!(by_home.into_iter().map(|(home, values)| (home, distribution(values))).collect::<BTreeMap<_, _>>());
    wall["home_ambiguous"] = distribution(ambiguous);
    // The carrying exec call's call → output of each matched MCP call, per server and tool (also inside the overall and `by_name` `exec`).
    let (mut by_server, mut mcp_unnamed) = (BTreeMap::<String, BTreeMap<String, Vec<i64>>>::new(), Vec::new());
    for (server, tool, ms) in &t.mcp_waits {
        match (server, tool) {
            (Some(server), Some(tool)) => by_server.entry(server.clone()).or_default().entry(tool.clone()).or_default().push(*ms),
            _ => mcp_unnamed.push(*ms),
        }
    }
    let mut mcp = distribution(t.mcp_waits.iter().map(|(_, _, ms)| *ms).collect());
    mcp["by_server"] = json!(by_server.into_iter().map(|(server, tools)| (server, tools.into_iter().map(|(tool, v)| (tool, distribution(v)))
        .collect::<BTreeMap<_, _>>())).collect::<BTreeMap<_, _>>());
    mcp["server_or_tool_unreported"] = distribution(mcp_unnamed);
    mcp["basis"] = json!("the carrying exec call's call → output (matched by turn and time, inferred); not the MCP call's run time");
    wall["mcp"] = mcp;
    wall["host_basis"] = json!("execution_home");
    wall["negative_intervals"] = json!(t.negative);
    wall["method"] = json!("nearest_rank");
    wall["caveat"] = json!("includes_approval_wait");
    wall["detail"] = json!(CALL_TO_OUTPUT);
    let m18 = json!({"value": unavailable("execution_duration_not_exposed"),
        "detail": "codex 0.154.0 records no execution end − start: the CommandExecution duration and its start/completion times are the unified exec \
            startup (caveat startup_not_run_time), and the call → output time includes approval waits",
        "mcp_duration": {"value": unavailable("execution_duration_not_exposed"), "detail": "the McpToolCall duration is measured like an exec item's \
            (caveat startup_not_run_time, certified only for a local stub): never read as run time"},
        "queue_time": unavailable("approval_decision_not_exposed"), "pending_calls": t.without_output, "timed_out": unavailable("timeout_not_exposed"),
        "call_to_output_ms": wall, "coverage": coverage});
    BTreeMap::from([("M16".to_owned(), metric("M16", m16)), ("M17".to_owned(), metric("M17", m17)), ("M18".to_owned(), metric("M18", m18))])
}

fn coverage(list: &[Session], excluded: &BTreeMap<String, usize>) -> Value {
    let count = |reason: &str| list.iter().filter(|s| s.tools.as_ref().err() == Some(&reason)).count();
    json!({"sessions": list.len(), "observed": list.iter().filter(|s| s.tools.is_ok()).count(), "pending_reread": count("pending_reread"),
        "predates_collection": count("predates_collection"), "excluded": excluded})
}

fn summaries_current(project: &Path, db: &Connection) -> Result<bool> {
    if !super::ledger::aggregates_current(db)? { return Ok(false); }
    let Some(generations) = crate::telemetry::analytics::inputs::generations(db)? else { return Ok(false); };
    let canonical = crate::telemetry::analytics::inputs::canonical(project)?;
    let stamp = crate::telemetry::analytics::inputs::stamp("tools", &canonical, &generations);
    Ok(db.query_row("SELECT inputs=?1 FROM accounting_tool_frontier WHERE singleton=1", [stamp], |r| r.get(0)).optional()?.unwrap_or(false))
}

/// Replace only replayed sessions' exact tallies. Wait durations are retained
/// as integers: aggregate nearest-rank distributions use the same sample set.
pub(crate) fn store(project: &Path, db: &Connection, full: bool) -> Result<()> {
    let canonical = crate::telemetry::analytics::inputs::canonical(project)?;
    let canonical_text = serde_json::to_string(&canonical)?;
    let previous: Option<String> = db.query_row("SELECT canonical FROM accounting_tool_frontier WHERE singleton=1", [], |r| r.get(0)).optional()?;
    let all = full || previous.as_deref() != Some(canonical_text.as_str());
    if all {
        db.execute_batch("DELETE FROM accounting_tool_summary;
            INSERT OR IGNORE INTO accounting_selected SELECT session_id FROM rollout_sources;")?;
    } else {
        // New guardian evidence changes the parent session's approval inference.
        db.execute_batch("INSERT OR IGNORE INTO accounting_selected SELECT claimed_parent_session_id FROM session_graph_nodes
            WHERE session_id IN (SELECT session_id FROM accounting_selected) AND claimed_parent_session_id IS NOT NULL;")?;
    }
    let selected: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM accounting_selected)", [], |r| r.get(0))?;
    if selected {
        db.execute_batch("DELETE FROM accounting_tool_summary WHERE session_id IN (SELECT session_id FROM accounting_selected);")?;
        let blocked = super::attention::blocked_spans(project, db)?;
        let (list, _) = sessions(db, None, &blocked, true, false)?;
        let mut insert = db.prepare_cached("INSERT INTO accounting_tool_summary(session_id,tally) VALUES(?1,?2)")?;
        for s in &list {
            if let Ok(rows) = &s.tools {
                let mut tally = Tally::default();
                tally.add(s, rows);
                insert.execute(rusqlite::params![s.id, serde_json::to_string(&tally)?])?;
            }
        }
    }
    let generations = crate::telemetry::analytics::inputs::generations(db)?.unwrap_or_default();
    let stamp = crate::telemetry::analytics::inputs::stamp("tools", &canonical, &generations);
    db.execute("INSERT INTO accounting_tool_frontier(singleton,canonical,inputs) VALUES(1,?1,?2)
        ON CONFLICT(singleton) DO UPDATE SET canonical=excluded.canonical,inputs=excluded.inputs", rusqlite::params![canonical_text, stamp])?;
    Ok(())
}

/// `accounting tools`: per bound session its tool call and execution counts
/// (or why they are unavailable), the coverage and M16–M18. Read-only.
pub fn read(project: &Path, db: &Connection) -> Result<Value> {
    let _snapshot = db.is_autocommit().then(|| db.unchecked_transaction()).transpose()?;
    let (list, excluded) = sessions(db, None, &super::attention::blocked_spans(project, db)?, false, summaries_current(project, db)?)?;
    let coverage = coverage(&list, &excluded);
    let sessions: Vec<Value> = list.iter().map(|s| {
        let tools = match &s.tools {
            Err(reason) => unavailable(reason),
            Ok(rows) => {
                let mut t = Tally::default();
                t.add(s, rows);
                // `attributed`/`unattributed`: command executions inferred to a call; MCP calls are counted in `mcp_calls`.
                json!({"issued": t.issued, "without_output": t.without_output, "outputs_without_call": t.outputs_without_call, "executed": t.executed,
                    "attributed": t.executed - t.mcp - t.unattributed, "unattributed": t.unattributed, "mcp_calls": t.mcp,
                    "succeeded": t.succeeded + t.mcp_succeeded, "failed": t.failed + t.mcp_failed,
                    "unknown": t.unknown.values().chain(t.mcp_unknown.values()).sum::<usize>(), "declined_or_aborted": t.aborted.values().sum::<usize>()})
            }
        };
        json!({"session_id": s.id, "attempt_ids": s.attempts, "tools": tools})
    }).collect();
    Ok(json!({"sessions": sessions, "coverage": coverage, "metrics": computed(&list, &coverage)}))
}

/// M16–M18 for `telemetry <slug> report` (sessions started in the window).
pub fn metrics(project: &Path, since: Option<i64>) -> Result<BTreeMap<String, Value>> {
    metrics_with(project, since, true)
}

pub(crate) fn metrics_with(project: &Path, since: Option<i64>, aggregates: bool) -> Result<BTreeMap<String, Value>> {
    let Some(db) = crate::telemetry::sidecar::read(project)? else { return Ok(unavailable_metrics("collection_not_run", None)) };
    let _snapshot = db.unchecked_transaction()?;
    let (list, excluded) = sessions(&db, since, &super::attention::blocked_spans(project, &db)?, false, aggregates && summaries_current(project, &db)?)?;
    Ok(computed(&list, &coverage(&list, &excluded)))
}

/// Text view: coverage, one line per session and per metric; unknown is `n/a (<reason>)`.
pub fn text(value: &Value) -> String {
    let reason = |v: &Value| format!("n/a ({})", v["reason"].as_str().unwrap_or("unknown"));
    if value["status"] == "unavailable" { return reason(value) + "\n"; }
    let c = &value["coverage"];
    let excluded: Vec<String> = c["excluded"].as_object().into_iter().flatten().map(|(k, v)| format!("{k} {v}")).collect();
    let mut out = format!("coverage {} sessions: {} observed, {} pending_reread, {} predates_collection; excluded {}\n", c["sessions"], c["observed"],
        c["pending_reread"], c["predates_collection"], if excluded.is_empty() { "none".to_owned() } else { excluded.join(", ") });
    for s in value["sessions"].as_array().into_iter().flatten() {
        let attempts: Vec<&str> = s["attempt_ids"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        let t = &s["tools"];
        let detail = if t["status"] == "unavailable" { reason(t) } else {
            format!("issued {} ({} without output, {} outputs without call), executed {} ({} inferred to a call, {} unattributed), succeeded {} failed {} unknown {}",
                t["issued"], t["without_output"], t["outputs_without_call"], t["executed"], t["attributed"], t["unattributed"], t["succeeded"], t["failed"], t["unknown"])
        };
        out += &format!("session {} attempts={}: {detail}\n", s["session_id"].as_str().unwrap_or(""), attempts.join(","));
    }
    let m = &value["metrics"];
    let (m16, m17, m18) = (&m["M16"], &m["M17"], &m["M18"]);
    out += &match m16["value"]["issued"].as_u64() {
        Some(issued) => format!("M16 tool_call_volume issued {issued}, accepted {} inferred ({} unknown), executed {}\n", m16["value"]["accepted"]["count"],
            m16["value"]["accepted"]["unknown"], m16["value"]["executed"]),
        None => format!("M16 tool_call_volume {}\n", reason(&m16["value"])),
    };
    if m16["mcp"].is_object() {
        let servers: Vec<String> = m16["mcp"]["by_server"].as_object().into_iter().flatten()
            .flat_map(|(server, tools)| tools.as_object().into_iter().flatten().map(move |(tool, n)| format!("{server}/{tool} {n}"))).collect();
        out += &format!("M16 mcp_calls {}{} (counted once with their exec call), declined_or_aborted {}\n", m16["mcp"]["calls"],
            if servers.is_empty() { String::new() } else { format!(" [{}]", servers.join(", ")) }, m16["value"]["accepted"]["declined_or_aborted"]);
    }
    out += &match &m17["value"] {
        Value::String(v) => format!("M17 tool_execution_success {v} (unknown {} excluded, pending calls {})\n", m17["unknown"]["executions"], m17["pending_calls"]),
        Value::Null => format!("M17 tool_execution_success n/a ({})\n", m17["reason"].as_str().unwrap_or("unknown")),
        other => format!("M17 tool_execution_success {}\n", reason(other)),
    };
    if m17["by_scope"].is_object() {
        let scope = |name: &str| { let s = &m17["by_scope"][name]; format!("{name} {} succeeded {} failed {} unknown", s["succeeded"], s["failed"], s["unknown"]["executions"]) };
        out += &format!("M17 by scope: {}; {}\n", scope("command_execution"), scope("mcp"));
    }
    out += &format!("M18 tool_latency_p95 {}\n", reason(&m18["value"]));
    let wall = &m18["call_to_output_ms"];
    if wall["samples"].is_u64() {
        let p95 = wall["p95_ms"].as_i64().map_or("n/a".to_owned(), |n| n.to_string());
        out += &format!("call_to_output_ms p95 {p95} of {} calls (includes approval wait; not execution time)\n", wall["samples"]);
    }
    out
}

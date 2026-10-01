//! OpenCode 1.18.34 SQLite projections, from recorded OpenCode execution homes.
//! Completed messages only; content is never persisted or used for identity.
use super::*;

pub const FIXTURE_VERSIONS: &[&str] = &["1.18.34"];

pub fn capabilities() -> Value {
    let fields = ["session.id", "session.version", "message.id", "message.modelID", "message.providerID",
        "message.tokens.input", "message.tokens.output", "message.tokens.reasoning", "message.tokens.cache.read",
        "message.tokens.cache.write", "message.cost", "message.time.created", "message.time.completed",
        "tool.name", "tool.status", "tool.is_error"].map(|field| json!({"kind":"metadata", "field":field,
            "available":true, "basis":"reported", "certified":"fixture"}));
    json!({"adapter":"opencode", "interface":"session_sqlite", "certified_versions":[],
        "fixture_versions":FIXTURE_VERSIONS, "accepted_versions":FIXTURE_VERSIONS, "certification":"fixture",
        "uncertified_version":"cli_version_uncertified", "fields":fields, "profiles":[],
        "live_certification":"separate_owner_gated_step", "otlp_mapping":"none"})
}

pub fn allowlist() -> Vec<(String, sanitize::Class)> {
    use sanitize::Class::*;
    [("message_id",Id),("model",Text),("provider",Text),("input",Number),("output",Number),("reasoning",Number),
        ("cache_read",Number),("cache_write",Number),("created_unix_ms",Number),("completed_unix_ms",Number)]
        .into_iter().map(|(k,c)|(k.to_owned(),c)).collect()
}

fn text(raw: &Value, name: &str, class: sanitize::Class) -> Option<String> {
    sanitize::field(raw, name, class).as_str().map(str::to_owned)
}
fn time(raw: &Value, name: &str) -> Option<i64> {
    raw.pointer(name).and_then(Value::as_i64).filter(|v| *v >= 0)
}
fn stamp(value: Option<i64>) -> Option<String> {
    value.and_then(|v| jiff::Timestamp::from_millisecond(v).ok()).map(|v| v.to_string())
}
fn regular(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
        && path.ancestors().skip(1).all(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()))
}
fn exists(db: &Connection, table: &str) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)", [table], |r| r.get(0))?)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn collect(db: &mut Connection, home: &str, worktrees: &str,
    tombstones: &super::super::maintenance::Tombstones, remaining: &mut u64, done: &mut Collected) -> Result<()> {
    // No XDG/environment fallback or agent invocation. Only the default latest-channel DB.
    let path = Path::new(home).join(".local/share/opencode/opencode.db");
    if !regular(&path) { return Ok(()); }
    let source = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    source.execute_batch("PRAGMA query_only=ON;")?;
    if !exists(&source, "session")? { return Ok(()); }
    // A consistent source snapshot; never read auth/config/event payload tables.
    source.execute_batch("BEGIN DEFERRED")?;
    let mut sessions = source.prepare("SELECT id,directory,version,time_created FROM session ORDER BY id")?;
    for row in sessions.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?)))? {
        let (sid, cwd, version, created) = row?;
        let Some(sid) = text(&json!({"id":sid}), "id", sanitize::Class::Id) else { continue };
        let session = format!("opencode:{sid}");
        let key = digest(format!("{}:{sid}", path.display()).as_bytes());
        if tombstones.key(super::super::maintenance::SESSIONS, &format!("session:{session}")).is_some()
            || tombstones.key(super::super::maintenance::SESSIONS, &format!("path:{key}")).is_some() { continue; }
        let version = format!("opencode/{}", text(&json!({"version":version}), "version", sanitize::Class::Text).unwrap_or_default());
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ledger = ingest::Ledger::begin(&tx, &key, 0, jiff::Timestamp::now().as_millisecond())?;
        let now = jiff::Timestamp::now().as_millisecond();
        let mut cursor = Cursor { offset:0, records:0, rate_limits:0, model:None, effort:None, session:None, uncertified:false, turn:None };
        if tx.query_row("SELECT 1 FROM rollout_sources WHERE path_digest=?1", [&key], |_| Ok(())).optional()?.is_none() {
            apply(&tx, &ledger, 0, json!({"type":"session_meta", "payload":{"id":session, "cwd":cwd,
                "timestamp":stamp(Some(created)), "cli_version":version, "originator":"opencode", "source":"user"}}),
                &key, home, worktrees, &mut cursor, now, done)?;
        } else {
            cursor.session = Some((session.clone(), version.clone(), Some(created)));
        }
        tx.execute("UPDATE rollout_sources SET cli_version=?2 WHERE path_digest=?1", params![key,version])?;
        cursor.records = tx.query_row("SELECT count(*) FROM opencode_messages WHERE session_id=?1", [&session], |r| r.get(0))?;
        for (table, native_v2) in [("message", false), ("session_message", true)] {
            if !exists(&source, table)? { continue; }
            // Skip unfinished and oversized rows without deserializing content.
            let role = if native_v2 { "type='assistant'" } else { "json_extract(data,'$.role')='assistant'" };
            let sql = format!("SELECT id,time_updated,length(CAST(data AS BLOB)) FROM {table} WHERE session_id=?1 AND {role} AND json_valid(data)
                AND json_type(data,'$.time.completed')='integer' AND length(CAST(data AS BLOB))<=16777216 ORDER BY time_created,id");
            let mut messages = source.prepare(&sql)?;
            for message in messages.query_map([&sid], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, u64>(2)?)))? {
                let (id, revision, bytes) = message?;
                let old: Option<(i64,i64)> = tx.query_row("SELECT ordinal,source_revision FROM opencode_messages WHERE session_id=?1 AND message_id=?2",
                    params![session,id], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
                if old.is_some_and(|(_, stored)| stored == revision) {
                    let reason: Option<String> = tx.query_row("SELECT reason FROM codex_usage WHERE session_id=?1 AND ordinal=?2",
                        params![session,old.expect("existing message").0], |r| r.get(0))?;
                    if reason.as_deref() != Some("cli_version_uncertified") || !accepted_version(&version) { continue; }
                }
                if *remaining < bytes { done.budget_exhausted = true; break; }
                // Extract metadata in SQLite. Rust never receives text, reasoning,
                // structured output, tool input/output or error values.
                let projection = format!("SELECT json_object(
                    'modelID',json_extract(data,'$.modelID'),'providerID',json_extract(data,'$.providerID'),
                    'model',json_object('id',json_extract(data,'$.model.id'),'providerID',json_extract(data,'$.model.providerID')),
                    'time',json_object('created',json_extract(data,'$.time.created'),'completed',json_extract(data,'$.time.completed')),
                    'cost',json_extract(data,'$.cost'),'tokens',json_object('input',json_extract(data,'$.tokens.input'),
                    'output',json_extract(data,'$.tokens.output'),'reasoning',json_extract(data,'$.tokens.reasoning'),
                    'cache',json_object('read',json_extract(data,'$.tokens.cache.read'),'write',json_extract(data,'$.tokens.cache.write'))),
                    'content',(SELECT json_group_array(json_object('type','tool','id',json_extract(p.value,'$.id'),
                    'name',json_extract(p.value,'$.name'),'state',json_object('status',json_extract(p.value,'$.state.status')),
                    'time',json_object('created',json_extract(p.value,'$.time.created'),'completed',json_extract(p.value,'$.time.completed'))))
                    FROM json_each(data,'$.content') p WHERE json_extract(p.value,'$.type')='tool')) FROM {table} WHERE id=?1");
                let data: String = source.query_row(&projection, [&id], |r| r.get(0))?;
                *remaining -= bytes;
                done.bytes += bytes;
                let raw: Value = serde_json::from_str(&data)?;
                let Some(id) = text(&json!({"id":id}), "id", sanitize::Class::Id) else { continue };
                let ordinal = old.map(|r| r.0).unwrap_or(cursor.records + 1);
                // OpenCode's native input is disjoint from cache; native output excludes reasoning.
                let number = |p: &str| raw.pointer(p).and_then(Value::as_i64).filter(|v| (0..=(1 << 53)).contains(v));
                let input = number("/tokens/input").zip(number("/tokens/cache/read")).zip(number("/tokens/cache/write"))
                    .and_then(|((i,r),w)| i.checked_add(r)?.checked_add(w));
                let reasoning = number("/tokens/reasoning");
                let output = number("/tokens/output").zip(reasoning).and_then(|(o,r)| o.checked_add(r));
                let model = text(&raw, if native_v2 { "model.id" } else { "modelID" }, sanitize::Class::Text);
                let provider = text(&raw, if native_v2 { "model.providerID" } else { "providerID" }, sanitize::Class::Text);
                cursor.model = model.clone();
                cursor.records = ordinal - 1;
                apply(&tx, &ledger, ordinal as u64, json!({"type":"token_usage_record", "timestamp":stamp(time(&raw,"/time/completed")),
                    "payload":{"response_id":id, "usage":{"input_tokens":input,"cached_input_tokens":number("/tokens/cache/read"),
                    "cache_write_input_tokens":number("/tokens/cache/write"),"output_tokens":output,"reasoning_output_tokens":reasoning,
                    "total_tokens":input.zip(output).and_then(|(i,o)| i.checked_add(o))}}}), &key, home, worktrees, &mut cursor, now, done)?;
                let cost = raw["cost"].as_f64().filter(|v| v.is_finite() && *v >= 0.0);
                tx.execute("INSERT OR IGNORE INTO opencode_messages(session_id,message_id,ordinal,path_digest,source_revision,model_id,provider_id,cost,created_unix_ms,completed_unix_ms)
                    VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)", params![session,id,ordinal,key,revision,model,provider,cost,time(&raw,"/time/created"),time(&raw,"/time/completed")])?;
                tx.execute("UPDATE opencode_messages SET source_revision=?3 WHERE session_id=?1 AND message_id=?2", params![session,id,revision])?;
                let payload = json!({"message_id":id,"model":model,"provider":provider,"input":number("/tokens/input"),
                    "output":number("/tokens/output"),"reasoning":reasoning,"cache_read":number("/tokens/cache/read"),
                    "cache_write":number("/tokens/cache/write"),"created_unix_ms":time(&raw,"/time/created"),"completed_unix_ms":time(&raw,"/time/completed")});
                ledger.observe(&tx, ordinal as u64, ingest::Record {kind:"opencode_message",payload:&payload,
                    occurred_unix_ms:time(&raw,"/time/completed"),session:&session,adapter_version:&version,certified:accepted_version(&version)},now)?;
                if native_v2 {
                    for part in raw["content"].as_array().into_iter().flatten().filter(|p| p["type"] == "tool") {
                        tool(&tx, &session, &id, part, true)?;
                    }
                }
                cursor.records = tx.query_row("SELECT count(*) FROM opencode_messages WHERE session_id=?1", [&session], |r| r.get(0))?;
            }
        }
        // Parts are independently mutable; project allowlisted metadata even when
        // their completed message did not change. No input/output/error text is read.
        if exists(&source, "part")? {
            let mut parts = source.prepare("SELECT message_id,json_object('id',id,'tool',json_extract(data,'$.tool'),
                'state',json_object('status',json_extract(data,'$.state.status'),'time',json_object(
                'start',json_extract(data,'$.state.time.start'),'end',json_extract(data,'$.state.time.end'))))
                FROM part WHERE session_id=?1 AND json_valid(data) AND json_extract(data,'$.type')='tool' ORDER BY id")?;
            for row in parts.query_map([&sid], |r| Ok((r.get::<_, String>(0)?,r.get::<_, String>(1)?)))? {
                let (message, metadata) = row?;
                if tx.query_row("SELECT 1 FROM opencode_messages WHERE session_id=?1 AND message_id=?2", params![session,message], |_| Ok(())).optional()?.is_none() { continue; }
                if *remaining < metadata.len() as u64 { done.budget_exhausted = true; break; }
                *remaining -= metadata.len() as u64;
                done.bytes += metadata.len() as u64;
                tool(&tx, &session, &message, &serde_json::from_str(&metadata)?, false)?;
            }
        }
        tx.execute("UPDATE rollout_sources SET records=?2 WHERE path_digest=?1", params![key,cursor.records])?;
        ledger.finish(&tx, Some(&session), cursor.records as u64 + 1, now)?;
        tx.commit()?;
        done.files += 1;
        if done.budget_exhausted { break; }
    }
    Ok(())
}

fn tool(tx: &Transaction, session: &str, message: &str, raw: &Value, v2: bool) -> Result<()> {
    let Some(id) = text(raw, "id", sanitize::Class::Id) else { return Ok(()) };
    let name = text(raw, if v2 { "name" } else { "tool" }, sanitize::Class::Tag);
    let status = raw["state"]["status"].as_str().filter(|s| matches!(*s,"pending"|"running"|"completed"|"error"));
    let error = status.and_then(|s| match s { "error" => Some(true), "completed" => Some(false), _ => None });
    // Only the status/error flag is retained. Never the native error string/object.
    tx.execute("INSERT INTO opencode_tools(session_id,message_id,part_id,tool,status,is_error,created_unix_ms,completed_unix_ms)
        VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(session_id,part_id) DO UPDATE SET status=excluded.status,is_error=excluded.is_error,
        completed_unix_ms=excluded.completed_unix_ms", params![session,message,id,name,status,error,
        time(raw,if v2 { "/time/created" } else { "/state/time/start" }),time(raw,if v2 { "/time/completed" } else { "/state/time/end" })])?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn apply(tx: &Transaction, ledger: &ingest::Ledger, at: u64, value: Value, key: &str, home: &str, worktrees: &str,
    cursor: &mut Cursor, now: i64, done: &mut Collected) -> Result<()> {
    let line = serde_json::to_vec(&value)?;
    let tag = serde_json::from_slice::<Tag>(&line)?;
    anyhow::ensure!(record(tx, ledger, at, &tag, &line, key, &digest(home.as_bytes()), worktrees, cursor, now, done, &mut None)?, "invalid OpenCode metadata mapping");
    Ok(())
}

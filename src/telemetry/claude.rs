//! Claude Code native metadata adapter. All sources are isolated execution homes
//! from canonical Claude attempts; no default owner home is ever discovered.
use super::*;

pub const FIXTURE_VERSIONS: &[&str] = &["2.1.3", "2.1.286"];

pub(super) fn walk(root: &Path, out: &mut Vec<PathBuf>) {
    if !root.parent().is_some_and(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()))
        || !std::fs::symlink_metadata(root).is_ok_and(|m| m.is_dir()) { return; }
    let Ok(dirs) = std::fs::read_dir(root) else { return };
    for dir in dirs.flatten() {
        if !dir.file_type().is_ok_and(|t| t.is_dir()) { continue; }
        let Ok(files) = std::fs::read_dir(dir.path()) else { continue };
        for file in files.flatten() {
            if file.file_type().is_ok_and(|t| t.is_file()) && file.path().extension().is_some_and(|s| s == "jsonl") {
                out.push(file.path());
            }
        }
    }
}

pub fn capabilities() -> Value {
    use sanitize::Class::*;
    let source_fields = [("sessionId", Id), ("timestamp", Text), ("cwd", Path), ("version", Text), ("type", Tag),
        ("message.model", Text), ("message.id", Id), ("isSidechain", Bool),
        ("message.usage.input_tokens", Number), ("message.usage.output_tokens", Number),
        ("message.usage.cache_creation_input_tokens", Number), ("message.usage.cache_read_input_tokens", Number),
        ("message.content.tool_use.id", Id), ("message.content.tool_use.name", Tag),
        ("message.content.tool_result.tool_use_id", Id), ("message.content.tool_result.is_error", Bool)];
    let fields = source_fields.into_iter().map(|(field, class)| json!({"kind": "line", "field": field,
        "available": true, "basis": match class { Text | Tag => "reported_excerpt", Path => "binding_only_home_redacted", _ => "reported" },
        "certified": "fixture", "caveat": if field == "cwd" { Some("binding_only") } else { None }})).chain(
        ["message.text", "message.thinking", "tool_use.input", "tool_result.content", "toolUseResult", "summary", "user_prompt"].map(|field|
            json!({"kind": "line", "field": field, "available": false, "basis": "unavailable", "certified": "none", "reason": "content_forbidden"})));
    json!({"adapter": "claude-code", "interface": "session_jsonl", "certified_versions": [], "fixture_versions": FIXTURE_VERSIONS,
        "accepted_versions": FIXTURE_VERSIONS, "certification": "fixture", "uncertified_version": "cli_version_uncertified",
        "fields": fields.collect::<Vec<_>>(), "profiles": [], "live_certification": "separate_owner_gated_step"})
}

pub fn allowlist() -> Vec<(String, sanitize::Class)> {
    use sanitize::Class::*;
    [("session_id", Id), ("timestamp", Text), ("version", Text), ("line_type", Tag), ("model", Id), ("message_id", Id),
        ("isSidechain", Bool), ("input_tokens", Number), ("output_tokens", Number), ("cache_creation_input_tokens", Number),
        ("cache_read_input_tokens", Number), ("tool_use_ids", IdList), ("tool_names", IdList), ("tool_result_ids", IdList),
        ("tool_result_errors", Number), ("unmapped_count", Number), ("unmapped_keys", IdList)].into_iter().map(|(k, c)| (k.to_owned(), c)).collect()
}

/// Only field names are reported for drift. Forbidden subtrees are never walked.
fn unmapped(raw: &Value, prefix: &str, known: &[&str], out: &mut Vec<String>, count: &mut u64) {
    if let Some(object) = raw.as_object() {
        for key in object.keys().filter(|k| !known.contains(&k.as_str())) {
            *count += 1;
            if out.len() < 128 { out.push(format!("{prefix}{key}")); }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn record_line(tx: &Transaction, ledger: &ingest::Ledger, at: u64, line: &[u8], _file: &Path, key: &str, home: &str,
    worktrees: &str, cursor: &mut Cursor, now: i64, done: &mut Collected) -> Result<bool> {
    let Ok(raw) = serde_json::from_slice::<Value>(line) else { return Ok(false) };
    let Some(kind) = raw["type"].as_str() else { return Ok(false) };
    let field = |value: &Value, name: &str, class| sanitize::field(value, name, class);
    let id = |value: &Value, name: &str| field(value, name, sanitize::Class::Id).as_str().map(str::to_owned);
    let timestamp = raw["timestamp"].as_str();
    if cursor.session.is_none() {
        let (Some(session), Some(cwd), Some(version)) = (id(&raw, "sessionId"), raw["cwd"].as_str(), raw["version"].as_str()) else { return Ok(true) };
        // Project slugs are lossy (every non-alphanumeric becomes a dash).
        // Walk all project directories and bind only from the reported absolute cwd.
        if Path::new(cwd).is_relative() || cwd.split('/').any(|p| p == "." || p == "..") { return Ok(false); }
        let version = field(&json!({"version": version}), "version", sanitize::Class::Text);
        let meta = json!({"type": "session_meta", "payload": {"id": format!("claude-code:{session}"), "cwd": cwd,
            "timestamp": timestamp, "cli_version": format!("claude-code/{}", version.as_str().unwrap_or("unknown")),
            "originator": "claude-code", "source": "user", "model_provider": "anthropic"}});
        apply(tx, ledger, at, meta, key, home, worktrees, cursor, now, done)?;
    }
    let Some((session, mut version, started)) = cursor.session.clone() else { return Ok(true) };
    // Mixed-session lines never inherit a different session's binding.
    if id(&raw, "sessionId").is_some_and(|s| format!("claude-code:{s}") != session) { return Ok(false); }
    if let Some(reported) = raw["version"].as_str() {
        let sanitized = field(&json!({"version": reported}), "version", sanitize::Class::Text);
        let reported = format!("claude-code/{}", sanitized.as_str().unwrap_or("unknown"));
        if reported != version {
            version = reported;
            tx.execute("UPDATE rollout_sources SET cli_version=?2 WHERE path_digest=?1", params![key, version])?;
            cursor.session = Some((session.clone(), version.clone(), started));
        }
    }
    let message = &raw["message"];
    let mut unknown = Vec::new();
    let mut unmapped_count = 0;
    unmapped(&raw, "", &["type", "sessionId", "timestamp", "cwd", "version", "isSidechain", "message", "toolUseResult", "summary"], &mut unknown, &mut unmapped_count);
    unmapped(message, "message.", &["id", "model", "usage", "content"], &mut unknown, &mut unmapped_count);
    let usage = &message["usage"];
    unmapped(usage, "message.usage.", &["input_tokens", "output_tokens", "cache_creation_input_tokens", "cache_read_input_tokens"], &mut unknown, &mut unmapped_count);
    if !matches!(kind, "assistant" | "user" | "summary" | "system") { unknown.push(format!("type:{kind}")); unmapped_count += 1; }
    let mut payload = json!({"session_id": session, "timestamp": timestamp, "version": raw["version"], "line_type": kind,
        // Preserve the certified model identifier, which the generic long-token
        // masker otherwise redacts. All other model strings keep excerpt rules.
        "model": field(message, "model", if message["model"].as_str() == Some("claude-haiku-4-5-20251001") {
            sanitize::Class::Id
        } else { sanitize::Class::Text }), "message_id": id(message, "id"), "isSidechain": raw["isSidechain"].as_bool(),
        "tool_use_ids": [], "tool_names": [], "tool_result_ids": [], "tool_result_errors": 0});
    for counter in ["input_tokens", "output_tokens", "cache_creation_input_tokens", "cache_read_input_tokens"] {
        payload[counter] = field(usage, counter, sanitize::Class::Number);
    }
    cursor.model = payload["model"].as_str().map(str::to_owned);
    if kind == "assistant" && usage.is_object() {
        let reported_id = id(message, "id");
        let message_id = reported_id.clone().unwrap_or_else(|| format!("unmapped:{key}:{at}"));
        let ordinal: Option<i64> = tx.query_row("SELECT ordinal FROM claude_messages WHERE session_id=?1 AND message_id=?2",
            params![session, message_id], |r| r.get(0)).optional()?;
        let ordinal = match ordinal {
            Some(ordinal) => ordinal,
            None => {
                let ordinal: i64 = tx.query_row("SELECT coalesce(max(ordinal),0)+1 FROM claude_messages WHERE session_id=?1", [&session], |r| r.get(0))?;
                tx.execute("INSERT INTO claude_messages(session_id,message_id,ordinal,path_digest,byte_offset) VALUES(?1,?2,?3,?4,?5)",
                    params![session, message_id, ordinal, key, at as i64])?;
                ordinal
            }
        };
        let first: (String, i64) = tx.query_row("SELECT path_digest,byte_offset FROM claude_messages WHERE session_id=?1 AND message_id=?2",
            params![session, message_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        if first == (key.to_owned(), at as i64) {
            let number = |k: &str| payload[k].as_i64().filter(|v| (0..=(1 << 53)).contains(v));
            let input = reported_id.as_ref().and_then(|_| number("input_tokens")).zip(number("cache_read_input_tokens")).zip(number("cache_creation_input_tokens"))
                .and_then(|((i, r), w)| i.checked_add(r)?.checked_add(w));
            let output = number("output_tokens");
            let record = json!({"type": "token_usage_record", "timestamp": timestamp, "payload": {"response_id": message_id,
                "usage": {"input_tokens": input, "cached_input_tokens": number("cache_read_input_tokens"),
                    "cache_write_input_tokens": number("cache_creation_input_tokens"), "output_tokens": output,
                    "reasoning_output_tokens": 0, "total_tokens": input.zip(output).and_then(|(i,o)| i.checked_add(o))}}});
            cursor.records = ordinal - 1;
            apply(tx, ledger, at, record, key, home, worktrees, cursor, now, done)?;
        }
        cursor.records = tx.query_row("SELECT count(*) FROM claude_messages WHERE path_digest=?1", [key], |r| r.get(0))?;
    }
    if raw["isSidechain"].as_bool() == Some(true) {
        let activity = format!("sidechain:{key}:{at}");
        tx.execute("INSERT OR IGNORE INTO codex_agent_items(session_id,item_type,item_id,agent_thread_id,completed_unix_ms)
            VALUES(?1,'SubAgentActivity',?2,NULL,?3)", params![session, activity, ms(timestamp)])?;
    }
    for block in message["content"].as_array().into_iter().flatten() {
        let block_kind = block["type"].as_str().unwrap_or("unknown");
        let allowed: &[&str] = match block_kind {
            "tool_use" => &["type", "id", "name", "input"],
            "tool_result" => &["type", "tool_use_id", "is_error", "content"],
            "text" => &["type", "text"], "thinking" => &["type", "thinking", "signature"], _ => &["type"],
        };
        unmapped(block, "message.content.", allowed, &mut unknown, &mut unmapped_count);
        if !matches!(block_kind, "tool_use" | "tool_result" | "text" | "thinking") { unknown.push(format!("block_type:{block_kind}")); unmapped_count += 1; }
        if kind == "assistant" && block_kind == "tool_use" && let Some(call) = id(block, "id") {
            let name = field(block, "name", sanitize::Class::Tag);
            payload["tool_use_ids"].as_array_mut().unwrap().push(json!(call));
            payload["tool_names"].as_array_mut().unwrap().push(name.clone());
            apply(tx, ledger, at, json!({"type": "response_item", "timestamp": timestamp, "payload": {
                "type": "function_call", "call_id": call, "name": name}}), key, home, worktrees, cursor, now, done)?;
        }
        if kind == "user" && block_kind == "tool_result" && let Some(call) = id(block, "tool_use_id") {
            payload["tool_result_ids"].as_array_mut().unwrap().push(json!(call));
            let error = block["is_error"].as_bool();
            if error == Some(true) { payload["tool_result_errors"] = json!(payload["tool_result_errors"].as_i64().unwrap_or(0) + 1); }
            apply(tx, ledger, at, json!({"type": "response_item", "timestamp": timestamp, "payload": {
                "type": "function_call_output", "call_id": call}}), key, home, worktrees, cursor, now, done)?;
            // Preserve reported boolean outcomes without inventing shell exit codes.
            tx.execute("INSERT OR IGNORE INTO claude_tool_results(session_id,call_id,is_error,completed_unix_ms) VALUES(?1,?2,?3,?4)",
                params![session, call, error, ms(timestamp)])?;
        }
    }
    payload["unmapped_count"] = json!(unmapped_count);
    payload["unmapped_keys"] = json!(unknown);
    ledger.observe(tx, at, ingest::Record { kind: "claude_line", payload: &payload, occurred_unix_ms: ms(timestamp), session: &session,
        adapter_version: &version, certified: accepted_version(&version) }, now)?;
    cursor.uncertified |= !accepted_version(&version);
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn apply(tx: &Transaction, ledger: &ingest::Ledger, at: u64, value: Value, key: &str, home: &str, worktrees: &str,
    cursor: &mut Cursor, now: i64, done: &mut Collected) -> Result<()> {
    let line = serde_json::to_vec(&value)?;
    let tag = serde_json::from_slice::<Tag>(&line)?;
    anyhow::ensure!(record(tx, ledger, at, &tag, &line, key, home, worktrees, cursor, now, done, &mut None)?, "invalid Claude metadata mapping");
    Ok(())
}

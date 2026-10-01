//! Muse native session metadata. Only model completions carry additive usage.
use super::*;
use std::collections::BTreeMap;

pub const FIXTURE_VERSIONS: &[&str] = &["1.4.0-R4161.1"];

pub(super) struct Source {
    session: String,
    parent: Option<String>,
    parent_key: Option<String>,
    version: String,
}

pub(super) fn discover(home: &str, attempts: &[CanonicalAttempt]) -> BTreeMap<PathBuf, Source> {
    let versions: std::collections::BTreeSet<_> = attempts
        .iter()
        .filter(|a| a.kind.as_deref() == Some("muse") && a.home.as_deref() == Some(home))
        .map(|a| a.version.as_deref().unwrap_or("unknown"))
        .collect();
    if versions.is_empty() {
        return BTreeMap::new();
    }
    let version = if versions.len() == 1 {
        *versions.first().unwrap()
    } else {
        "unknown"
    };
    // Resolve Muse's default XDG data root relative to the recorded HOME.
    // Never consult the collector process's XDG_DATA_HOME or an owner home.
    let root = Path::new(home).join(".local/share/muse/sessions");
    let mut out = BTreeMap::new();
    fn walk(dir: &Path, depth: usize, version: &str, out: &mut BTreeMap<PathBuf, Source>) {
        if depth > 6
            || !dir.is_absolute()
            || dir
                .ancestors()
                .any(|p| !std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()))
        {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                walk(&path, depth + 1, version, out);
            } else if entry.file_type().is_ok_and(|t| t.is_file())
                && entry.file_name() == "session.jsonl"
            {
                let Some(name) = dir.file_name().and_then(|s| s.to_str()) else {
                    continue;
                };
                let Some(id) = sanitize::field(&json!({"id":name}), "id", sanitize::Class::Id)
                    .as_str()
                    .map(str::to_owned)
                else {
                    continue;
                };
                let parent_dir = dir
                    .parent()
                    .filter(|p| p.file_name().is_some_and(|n| n == "subagent"))
                    .and_then(Path::parent);
                let parent = parent_dir
                    .and_then(Path::file_name)
                    .and_then(|s| s.to_str())
                    .and_then(|name| {
                        sanitize::field(&json!({"id":name}), "id", sanitize::Class::Id)
                            .as_str()
                            .map(|id| format!("muse:{id}"))
                    });
                out.insert(
                    path,
                    Source {
                        session: format!("muse:{id}"),
                        parent,
                        parent_key: parent_dir.map(|p| {
                            digest(p.join("session.jsonl").as_os_str().as_encoded_bytes())
                        }),
                        version: format!("muse/{version}"),
                    },
                );
            }
        }
    }
    walk(&root, 0, version, &mut out);
    out
}

pub fn capabilities() -> Value {
    let fields = ["id", "recorded_at", "payload.record.workspace_root", "payload.event.model",
        "payload.event.usage.input_tokens", "payload.event.usage.output_tokens", "payload.event.usage.cache_read_tokens",
        "payload.event.usage.cache_write_tokens", "payload.event.usage.reasoning_tokens", "path.parent_session_id"]
        .map(|field| json!({"kind":"model_completed", "field":field, "available":true, "basis":"reported", "certified":"fixture"}));
    json!({"adapter":"muse", "interface":"session_jsonl", "certified_versions":[], "fixture_versions":FIXTURE_VERSIONS,
        "accepted_versions":FIXTURE_VERSIONS, "certification":"fixture", "uncertified_version":"cli_version_uncertified",
        "fields":fields, "profiles":[], "live_certification":"separate_owner_gated_step"})
}

#[allow(clippy::too_many_arguments)]
pub(super) fn record_line(
    tx: &Transaction,
    ledger: &ingest::Ledger,
    at: u64,
    line: &[u8],
    source: &Source,
    key: &str,
    home: &str,
    worktrees: &str,
    cursor: &mut Cursor,
    now: i64,
    done: &mut Collected,
) -> Result<bool> {
    let Ok(raw) = serde_json::from_slice::<Value>(line) else {
        return Ok(false);
    };
    let timestamp = raw["recorded_at"]
        .as_i64()
        .filter(|v| *v >= 0)
        .and_then(|v| jiff::Timestamp::from_millisecond(v / 1000).ok())
        .map(|v| v.to_string());
    if cursor.session.is_none() {
        if raw["payload_type"] != "runtime.session.metadata" {
            return Ok(true);
        }
        let cwd = raw
            .pointer("/payload/record/workspace_root")
            .and_then(Value::as_str)
            .unwrap_or("");
        apply(
            tx,
            ledger,
            at,
            json!({"type":"session_meta", "payload":{"id":source.session,"cwd":cwd,
            "timestamp":timestamp,"cli_version":source.version,"originator":"muse","source":"user",
            "parent_thread_id":source.parent,"subagent_parent_thread_id":source.parent,
            "subagent_kind":source.parent.as_ref().map(|_| "muse"),"thread_source":source.parent.as_ref().map(|_| "subagent")}}),
            key,
            home,
            worktrees,
            cursor,
            now,
            done,
        )?;
    }
    // Preserve the exact parent file path, including its date directories.
    // Binding resolves it again even if the parent arrives on a later pass.
    if raw["payload_type"] == "runtime.session.metadata"
        && let Some(parent_key) = &source.parent_key
    {
        tx.execute("INSERT OR IGNORE INTO muse_parents(path_digest,session_id,parent_path_digest) VALUES(?1,?2,?3)",
            params![key,source.session,parent_key])?;
    }
    if raw["payload_type"] != "runtime.session"
        || raw.pointer("/payload/event/kind").and_then(Value::as_str) != Some("model_completed")
    {
        return Ok(true);
    }
    let Some(event) = sanitize::field(&raw, "id", sanitize::Class::Id)
        .as_str()
        .map(str::to_owned)
    else {
        return Ok(false);
    };
    let old: Option<(i64,String,i64)> = tx.query_row("SELECT ordinal,path_digest,byte_offset FROM muse_events WHERE session_id=?1 AND event_id=?2",
        params![source.session,event], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let ordinal = match old {
        Some((ordinal, path, offset)) if path == key && offset == at as i64 => ordinal,
        Some(_) => {
            cursor.records = tx.query_row(
                "SELECT count(*) FROM muse_events WHERE path_digest=?1",
                [key],
                |r| r.get(0),
            )?;
            return Ok(true);
        }
        None => {
            let ordinal: i64 = tx.query_row(
                "SELECT coalesce(max(ordinal),0)+1 FROM muse_events WHERE session_id=?1",
                [&source.session],
                |r| r.get(0),
            )?;
            tx.execute(
                "INSERT INTO muse_events VALUES(?1,?2,?3,?4,?5)",
                params![source.session, event, ordinal, key, at as i64],
            )?;
            ordinal
        }
    };
    let usage = &raw["payload"]["event"]["usage"];
    let number = |name| sanitize::field(usage, name, sanitize::Class::Number).as_i64();
    let input = number("input_tokens");
    let output = number("output_tokens");
    cursor.model = sanitize::field(&raw["payload"]["event"], "model", sanitize::Class::Text)
        .as_str()
        .map(str::to_owned);
    cursor.records = ordinal - 1;
    apply(
        tx,
        ledger,
        at,
        json!({"type":"token_usage_record","timestamp":timestamp,"payload":{"response_id":event,
        "usage":{"input_tokens":input,"output_tokens":output,"cached_input_tokens":number("cache_read_tokens"),
        "cache_write_input_tokens":number("cache_write_tokens"),"reasoning_output_tokens":number("reasoning_tokens"),
        "total_tokens":input.zip(output).and_then(|(i,o)| i.checked_add(o))}}}),
        key,
        home,
        worktrees,
        cursor,
        now,
        done,
    )?;
    if let Some(parent) = &source.parent {
        tx.execute("INSERT OR IGNORE INTO codex_agent_items(session_id,item_type,item_id,agent_thread_id,completed_unix_ms)
            VALUES(?1,'SubAgentActivity',?2,?3,?4)", params![parent,format!("{}:{event}",source.session),source.session,ms(timestamp.as_deref())])?;
    }
    cursor.records = tx.query_row(
        "SELECT count(*) FROM muse_events WHERE path_digest=?1",
        [key],
        |r| r.get(0),
    )?;
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn apply(
    tx: &Transaction,
    ledger: &ingest::Ledger,
    at: u64,
    value: Value,
    key: &str,
    home: &str,
    worktrees: &str,
    cursor: &mut Cursor,
    now: i64,
    done: &mut Collected,
) -> Result<()> {
    let line = serde_json::to_vec(&value)?;
    let tag = serde_json::from_slice::<Tag>(&line)?;
    anyhow::ensure!(
        record(
            tx, ledger, at, &tag, &line, key, home, worktrees, cursor, now, done, &mut None
        )?,
        "invalid Muse metadata mapping"
    );
    if let Some((session, version, _)) = &cursor.session {
        let certified = accepted_version(version);
        ledger.observe(
            tx,
            at,
            ingest::Record {
                kind: value["type"].as_str().expect("mapped kind"),
                payload: &value["payload"],
                occurred_unix_ms: ms(tag.timestamp.as_deref()),
                session,
                adapter_version: version,
                certified,
            },
            now,
        )?;
        cursor.uncertified |= !certified;
    }
    Ok(())
}

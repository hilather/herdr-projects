//! Gemini 0.62.0 native chat metadata: update records, never content or sums.
use super::*;
use std::collections::BTreeMap;

pub(super) struct Source {
    roots: BTreeMap<String, String>,
    version: String,
}

/// Project temp directory names are registry short ids in 0.62.0, not hashes.
/// Bind against the metadata's full projectHash, never that directory name.
pub(super) fn discover(
    home: &str,
    attempts: &[CanonicalAttempt],
    worktrees: &str,
) -> BTreeMap<PathBuf, Source> {
    let candidates: Vec<_> = attempts
        .iter()
        .filter(|a| a.kind.as_deref() == Some("gemini") && a.home.as_deref() == Some(home))
        .collect();
    if candidates.is_empty() {
        return BTreeMap::new();
    }
    let mut roots = BTreeMap::new();
    for attempt in &candidates {
        let root = PathBuf::from(format!("{worktrees}{}", attempt.id));
        if root
            .ancestors()
            .any(|p| !std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()))
        {
            continue;
        }
        if let Ok(dirs) = std::fs::read_dir(&root) {
            for dir in dirs
                .flatten()
                .filter(|d| d.file_type().is_ok_and(|t| t.is_dir()))
            {
                let cwd = dir.path().display().to_string();
                roots.insert(format!("{:x}", Sha256::digest(cwd.as_bytes())), cwd);
            }
        }
    }
    let versions: std::collections::BTreeSet<_> = candidates
        .iter()
        .filter_map(|a| a.version.as_deref())
        .collect();
    let version = if versions.len() == 1 {
        *versions.first().unwrap()
    } else {
        "unknown"
    };
    let root = Path::new(home).join(".gemini/tmp");
    if !root.is_absolute()
        || root
            .ancestors()
            .any(|p| !std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()))
    {
        return BTreeMap::new();
    }
    let Ok(dirs) = std::fs::read_dir(root) else {
        return BTreeMap::new();
    };
    let mut out = BTreeMap::new();
    for dir in dirs
        .flatten()
        .filter(|d| d.file_type().is_ok_and(|t| t.is_dir()))
    {
        let chats = dir.path().join("chats");
        if !std::fs::symlink_metadata(&chats).is_ok_and(|m| m.is_dir()) {
            continue;
        }
        let Ok(files) = std::fs::read_dir(chats) else {
            continue;
        };
        for file in files.flatten() {
            if file.file_type().is_ok_and(|t| t.is_file())
                && file.path().extension().is_some_and(|e| e == "jsonl")
            {
                out.insert(
                    file.path(),
                    Source {
                        roots: roots.clone(),
                        version: version.to_owned(),
                    },
                );
            }
        }
    }
    out
}

pub fn fields() -> Vec<Value> {
    ["sessionId", "projectHash", "startTime", "id", "timestamp", "type", "model", "tokens.input", "tokens.output",
        "tokens.cached", "tokens.thoughts", "tokens.tool", "tokens.total", "toolCalls.id", "toolCalls.name", "toolCalls.status"].into_iter().map(|field|
        json!({"kind":"native_chat","field":field,"available":true,"basis":if matches!(field,"model"|"toolCalls.name") {"reported_excerpt"} else {"reported"},"certified":"fixture","caveat":"message_updates_not_additive"})).chain(
        ["content", "thoughts", "toolCalls.args", "toolCalls.result", "toolCalls.description", "$set.summary", "$set.memoryScratchpad"].map(|field|
            json!({"kind":"native_chat","field":field,"available":false,"basis":"unavailable","certified":"none","reason":"content_forbidden"}))).collect()
}

pub fn allowlist() -> Vec<(String, sanitize::Class)> {
    use sanitize::Class::*;
    [
        ("session_id", Id),
        ("message_id", Id),
        ("timestamp", Text),
        ("line_type", Tag),
        ("model", Text),
        ("input", Number),
        ("output", Number),
        ("cached", Number),
        ("thoughts", Number),
        ("tool", Number),
        ("total", Number),
        ("tool_ids", IdList),
        ("tool_names", IdList),
        ("tool_statuses", IdList),
    ]
    .into_iter()
    .map(|(k, c)| (k.to_owned(), c))
    .collect()
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
    let id = |value: &Value, name| {
        sanitize::field(value, name, sanitize::Class::Id)
            .as_str()
            .map(str::to_owned)
    };
    if cursor.session.is_none() {
        let (Some(session), Some(hash), Some(start)) = (
            id(&raw, "sessionId"),
            raw["projectHash"].as_str(),
            raw["startTime"].as_str(),
        ) else {
            return Ok(true);
        };
        // An unmatched hash is retained unbound with an empty cwd, never guessed.
        let cwd = source.roots.get(hash).map(String::as_str).unwrap_or("");
        let value = json!({"type":"session_meta","payload":{"id":format!("gemini-cli:{session}"),"cwd":cwd,"timestamp":start,
            "cli_version":format!("gemini-cli/{}",source.version),"originator":"gemini-cli","source":"user","model_provider":"google"}});
        let bytes = serde_json::to_vec(&value)?;
        let tag = serde_json::from_slice::<Tag>(&bytes)?;
        if !record(
            tx, ledger, at, &tag, &bytes, key, home, worktrees, cursor, now, done, &mut None,
        )? {
            return Ok(false);
        }
    }
    let Some((session, version, _)) = &cursor.session else {
        return Ok(true);
    };
    if id(&raw, "sessionId").is_some_and(|id| format!("gemini-cli:{id}") != *session) {
        return Ok(false);
    }
    let kind = raw["type"]
        .as_str()
        .unwrap_or(if raw.get("$set").is_some() {
            "metadata_update"
        } else {
            "metadata"
        });
    let mut payload = json!({"session_id":session,"message_id":id(&raw,"id"),"timestamp":raw["timestamp"].as_str(),"line_type":kind,
        "model":sanitize::field(&raw,"model",sanitize::Class::Text),"tool_ids":[],"tool_names":[],"tool_statuses":[]});
    // Never traverse content, thoughts, arguments, results, summary or scratchpad.
    for counter in ["input", "output", "cached", "thoughts", "tool", "total"] {
        payload[counter] = sanitize::field(&raw["tokens"], counter, sanitize::Class::Number);
    }
    for call in raw["toolCalls"].as_array().into_iter().flatten().take(128) {
        payload["tool_ids"]
            .as_array_mut()
            .unwrap()
            .push(sanitize::field(call, "id", sanitize::Class::Id));
        payload["tool_names"]
            .as_array_mut()
            .unwrap()
            .push(sanitize::field(call, "name", sanitize::Class::Tag));
        payload["tool_statuses"].as_array_mut().unwrap().push(json!(
            call["status"]
                .as_str()
                .filter(|s| matches!(*s, "success" | "error" | "cancelled" | "pending"))
        ));
    }
    ledger.observe(
        tx,
        at,
        ingest::Record {
            kind: "gemini_line",
            payload: &payload,
            occurred_unix_ms: ms(raw["timestamp"].as_str()),
            session,
            adapter_version: version,
            certified: source.version == "0.62.0",
        },
        now,
    )?;
    Ok(true)
}

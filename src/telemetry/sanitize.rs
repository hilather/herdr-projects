//! Contracts §7 for collected source records: the per-kind allowlist first
//! (every other field is dropped unread), then the excerpt rules on the kept
//! strings. The result is bounded metadata; `null` stands for an allowlisted
//! field that is absent or not of its class, never omitted.
use serde_json::{Map, Value};

/// How an allowlisted field is kept.
#[derive(Clone, Copy)]
pub enum Class {
    /// An identifier: kept verbatim when ≤128 chars without control characters.
    Id,
    /// Enum-like or free text: excerpt rules 1-5.
    Text,
    /// A string, or the first key of an object (Codex `source`), then as `Text`.
    Tag,
    /// A path: rule 2 only (contracts §7 stores `cwd` so), ≤1024 chars.
    Path,
    /// An integer stays an integer; any other number becomes its decimal text.
    Number,
    /// A JSON boolean, kept as is (A8: MCP `readOnlyHint`, `result.isError`).
    Bool,
    /// An array of identifiers, each kept as `Id` (`null` if not one); any
    /// other value is `null` (A8: `receiver_thread_ids`).
    IdList,
}

const USAGE: [&str; 6] = ["cache_write_input_tokens", "cached_input_tokens", "input_tokens", "output_tokens", "reasoning_output_tokens", "total_tokens"];

/// The allowlist of contracts §5 per Codex record kind (the `type` tag, or the
/// `event_msg` payload type), as dotted paths into the record's `payload`.
/// `None`: the kind is not collected.
pub fn codex_allowlist(kind: &str) -> Option<Vec<(String, Class)>> {
    use Class::*;
    let fields = |list: &[(&str, Class)]| list.iter().map(|(path, class)| (path.to_string(), *class)).collect::<Vec<_>>();
    let usage = |prefix: &'static str| USAGE.iter().map(move |f| (format!("{prefix}.{f}"), Number));
    Some(match kind {
        "gemini_line" => super::codex::gemini::allowlist(),
        "claude_line" => super::codex::claude::allowlist(),
        "opencode_message" => super::codex::opencode::allowlist(),
        "session_meta" => fields(&[("id", Id), ("timestamp", Text), ("cwd", Path), ("cli_version", Text), ("originator", Text), ("source", Tag),
            ("model_provider", Text), ("forked_from_id", Id), ("subagent_kind", Tag), ("subagent_detail", Text), ("subagent_parent_thread_id", Id), ("subagent_depth", Number),
            ("parent_thread_id", Id), ("session_id", Id), ("thread_source", Tag),
            // A8: the fork point a `codex exec fork` names (contracts-collection.md A8).
            ("forked_from_ordinal_exclusive", Number), ("history_base.thread_id", Id), ("history_base.end_ordinal_exclusive", Number),
            ("history_base.end_byte_offset", Number)]),
        "turn_context" => fields(&[("turn_id", Id), ("model", Text), ("effort", Text)]),
        "token_usage_record" => fields(&[("session_id", Id), ("turn_id", Id), ("response_id", Id)]).into_iter()
            .chain(usage("usage")).chain(usage("thread_token_usage")).collect(),
        "token_count" => fields(&[("rate_limits.limit_id", Text), ("rate_limits.plan_type", Text), ("rate_limits.primary.used_percent", Number),
            ("rate_limits.primary.window_minutes", Number), ("rate_limits.primary.resets_at", Number), ("rate_limits.secondary.used_percent", Number),
            ("rate_limits.secondary.window_minutes", Number), ("rate_limits.secondary.resets_at", Number), ("rate_limits.rate_limit_reached_type", Text)]).into_iter()
            .chain(usage("info.total_token_usage")).collect(),
        "task_started" => fields(&[("turn_id", Id)]),
        "task_complete" => fields(&[("turn_id", Id), ("duration_ms", Number), ("time_to_first_token_ms", Number)]),
        // A6 tool/exec metadata (contracts-collection.md A6): `response_item`
        // tool calls and outputs by their payload type, never `input`,
        // `arguments` or `output`; `item_completed` never the command or its output.
        "custom_tool_call" => fields(&[("call_id", Id), ("name", Tag), ("status", Tag), ("internal_chat_message_metadata_passthrough.turn_id", Id)]),
        // A8: `namespace` (live: `collaboration`).
        "function_call" => fields(&[("call_id", Id), ("name", Tag), ("namespace", Tag), ("status", Tag), ("internal_chat_message_metadata_passthrough.turn_id", Id)]),
        "custom_tool_call_output" | "function_call_output" => fields(&[("call_id", Id)]),
        "item_completed" => fields(&[("thread_id", Id), ("turn_id", Id), ("item.type", Tag), ("item.id", Id), ("item.status", Tag), ("item.source", Tag),
            ("item.exit_code", Number), ("item.duration.secs", Number), ("item.duration.nanos", Number),
            // A8 (contracts-collection.md A8): `McpToolCall` server and tool
            // names, hint and error flag (never `arguments` or `result.content`);
            // `SubAgentActivity` and `CollabAgentToolCall` ids (never
            // `agent_path`, `receiver_agents` or `agents_states`).
            ("item.server", Tag), ("item.tool", Tag), ("item.readOnlyHint", Bool), ("item.result.isError", Bool),
            ("item.agent_thread_id", Id), ("item.sender_thread_id", Id), ("item.receiver_thread_ids", IdList)]),
        // A8: an aborted turn's final event.
        "turn_aborted" => fields(&[("turn_id", Id), ("reason", Tag), ("duration_ms", Number)]),
        _ => return None,
    })
}

/// Allowlisted envelope fields read from another place of the record: the
/// Codex subagent source (`session_meta.source.subagent`, a string or a
/// one-key object) is kept as flat fields beside the `source` tag
/// (contracts-collection.md A4), with the `other` variant's string tag as
/// `subagent_detail` (A7; any other type is `null`).
fn source_path(path: &str) -> &str {
    match path {
        "subagent_kind" => "source.subagent",
        "subagent_detail" => "source.subagent.other",
        "subagent_parent_thread_id" => "source.subagent.thread_spawn.parent_thread_id",
        "subagent_depth" => "source.subagent.thread_spawn.depth",
        other => other,
    }
}

/// The allowlisted field at envelope `path` of `raw`, sanitized by `class`;
/// `null` when absent or not of its class.
pub fn field(raw: &Value, path: &str, class: Class) -> Value {
    source_path(path).split('.').try_fold(raw, |value, key| value.get(key)).map_or(Value::Null, |value| keep(value, class))
}

/// A new object holding only `fields` of `raw`, each sanitized by its class.
pub fn payload(fields: &[(String, Class)], raw: &Value) -> Value {
    let mut out = Value::Object(Map::new());
    for (path, class) in fields {
        let value = field(raw, path, *class);
        let mut slot = &mut out;
        for key in path.split('.') {
            slot = slot.as_object_mut().expect("allowlist paths nest objects only").entry(key).or_insert_with(|| Value::Object(Map::new()));
        }
        *slot = value;
    }
    out
}

fn keep(value: &Value, class: Class) -> Value {
    let text = |value: &Value| value.as_str().map(|s| Value::String(excerpt(s))).unwrap_or(Value::Null);
    match class {
        Class::Id => value.as_str().filter(|s| s.chars().count() <= 128 && !s.chars().any(char::is_control)).map_or(Value::Null, |s| Value::String(s.into())),
        Class::Text => text(value),
        Class::Tag => match value {
            Value::Object(map) => map.keys().next().map_or(Value::Null, |key| Value::String(excerpt(key))),
            other => text(other),
        },
        Class::Path => value.as_str().filter(|s| s.len() <= 1024 && !s.chars().any(char::is_control)).map_or(Value::Null, |s| Value::String(home_prefix(s))),
        Class::Number => match value {
            Value::Number(n) if n.is_i64() || n.is_u64() => value.clone(),
            Value::Number(n) => Value::String(n.to_string()),
            _ => Value::Null,
        },
        Class::Bool => value.as_bool().map_or(Value::Null, Value::Bool),
        Class::IdList => value.as_array().map_or(Value::Null, |ids| Value::Array(ids.iter().map(|id| keep(id, Class::Id)).collect())),
    }
}

/// Rule 2 for a path: the user's home directory, or any `/home/<user>` or
/// `/Users/<user>`, prefix becomes `~`.
pub fn home_prefix(path: &str) -> String {
    if let Some(home) = std::env::var("HOME").ok().map(|h| h.trim_end_matches('/').to_owned()).filter(|h| h.len() > 1)
        && let Some(rest) = path.strip_prefix(&home).filter(|rest| rest.is_empty() || rest.starts_with('/')) {
        return format!("~{rest}");
    }
    for root in ["/home/", "/Users/"] {
        if let Some(user) = path.strip_prefix(root) {
            return format!("~{}", &user[user.find('/').unwrap_or(user.len())..]);
        }
    }
    path.to_owned()
}

/// Contracts §7 excerpt rules 1-5.
pub fn excerpt(text: &str) -> String {
    // 1. First line; control characters become spaces.
    let line: String = text.split(['\n', '\r']).next().unwrap_or("").chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let words: Vec<String> = line.split(' ').map(str::to_owned).collect();
    let mut out: Vec<String> = Vec::with_capacity(words.len());
    let mut bearer = false;
    for word in words {
        // 2. Home prefixes, 3. URL query strings and fragments.
        let mut word = match word.find("/home/").or_else(|| word.find("/Users/")).or_else(|| home_start(&word)) {
            Some(at) => format!("{}{}", &word[..at], home_prefix(&word[at..])),
            None => word,
        };
        if word.contains("://") && let Some(cut) = word.find(['?', '#']) { word.truncate(cut); }
        // 4. Token-like strings.
        if bearer && !word.is_empty() { word = "[redacted]".into(); }
        bearer = word.eq_ignore_ascii_case("bearer");
        out.push(mask(&word));
    }
    // 5. At most 160 Unicode scalar values.
    let joined = out.join(" ");
    if joined.chars().count() <= 160 { joined } else { joined.chars().take(159).chain(['…']).collect() }
}

fn home_start(word: &str) -> Option<usize> {
    let home = std::env::var("HOME").ok().map(|h| h.trim_end_matches('/').to_owned()).filter(|h| h.len() > 1)?;
    word.find(&home).filter(|at| word[at + home.len()..].is_empty() || word[at + home.len()..].starts_with('/'))
}

fn mask(word: &str) -> String {
    let token = |c: char| c.is_ascii_alphanumeric() || "_-+/=".contains(c);
    let mut out = String::new();
    let mut rest = word;
    while let Some(start) = rest.find(token) {
        out.push_str(&rest[..start]);
        let run_len = rest[start..].find(|c: char| !token(c)).unwrap_or(rest.len() - start);
        let run = &rest[start..start + run_len];
        out.push_str(&mask_run(run));
        rest = &rest[start + run_len..];
    }
    out.push_str(rest);
    out
}

fn mask_run(run: &str) -> String {
    let lower = run.to_ascii_lowercase();
    for key in ["key=", "token=", "secret=", "password="] {
        if let Some(at) = lower.find(key) && run.len() > at + key.len() {
            return format!("{}[redacted]", &run[..at + key.len()]);
        }
    }
    let secret_prefix = ["sk-", "ghp_", "github_pat_", "akia"].iter().any(|p| lower.starts_with(p) && run.len() > p.len())
        || (lower.starts_with("xox") && lower.as_bytes().get(4) == Some(&b'-'));
    let long = run.len() >= 20 && run.chars().any(|c| c.is_ascii_alphabetic()) && run.chars().any(|c| c.is_ascii_digit());
    if secret_prefix || long { "[redacted]".into() } else { run.into() }
}

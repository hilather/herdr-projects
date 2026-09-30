//! Consistent export redaction (contracts.md §7, contracts-export.md §5):
//! exports carry metadata and evidence references, never content. Every
//! string leaf and object key passes one rule: a `sha256:` digest or a page
//! cursor is kept; an identifier under an identity key (`id`, `task_id`, ...)
//! is kept when it has identifier grammar and no secret prefix; anything else
//! is text and gets the §7 excerpt rules (first line, home prefixes `~`, URL
//! query/fragment stripped, token-like runs `[redacted]`, 160 scalars).
use crate::telemetry::sanitize;
use serde_json::{Map, Value};

/// Keys whose string values are identities of canonical rows.
const ID_KEYS: [&str; 7] = ["id", "task_id", "attempt_id", "session_id", "metric_id", "definition", "export_id"];

fn digest(text: &str) -> bool {
    text.strip_prefix("sha256:").is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
}

fn cursor(text: &str) -> bool {
    let mut parts = text.split('.');
    parts.next() == Some(super::cursor::PREFIX) && parts.by_ref().take(2).all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_hexdigit())) && parts.next().is_none()
}

/// Identifier grammar: `[A-Za-z0-9][A-Za-z0-9._:-]{0,191}` without a secret prefix.
pub fn identifier(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let secret = ["sk-", "ghp_", "github_pat_", "akia"].iter().any(|p| lower.starts_with(p)) || (lower.starts_with("xox") && lower.as_bytes().get(4) == Some(&b'-'));
    !secret && (1..=192).contains(&text.len()) && text.as_bytes()[0].is_ascii_alphanumeric()
        && text.bytes().all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
}

pub fn text(key: Option<&str>, value: &str) -> String {
    if digest(value) || (key == Some("next_cursor") && cursor(value)) { return value.to_owned(); }
    if key.is_some_and(|k| ID_KEYS.contains(&k)) && identifier(value) { return value.to_owned(); }
    sanitize::excerpt(value)
}

/// Object keys are structure (dimension names, reasons, lane fields): kept
/// with identifier grammar, otherwise excerpted like text.
fn key(k: &str) -> String { if identifier(k) || digest(k) { k.to_owned() } else { sanitize::excerpt(k) } }

pub fn value(v: &Value) -> Value { walk(None, v) }

fn walk(parent: Option<&str>, v: &Value) -> Value {
    match v {
        Value::String(s) => Value::String(text(parent, s)),
        Value::Array(items) => Value::Array(items.iter().map(|i| walk(parent, i)).collect()),
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                let mut name = key(k);
                // Two keys that redact alike stay distinct, never silently merged.
                while out.contains_key(&name) { name.push('#'); }
                out.insert(name, walk(Some(k.as_str()), v));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

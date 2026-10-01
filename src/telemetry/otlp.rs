//! DG4a/DG4h: bounded OTLP/HTTP JSON and protobuf, allowlist before storage.
mod protobuf;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
    time::{Duration, Instant},
};

pub const STREAM: &str = "otlp";
pub const MIGRATIONS: &[&str] = &[
    include_str!("../../migrations/telemetry/otlp/0001_records.sql"),
    include_str!("../../migrations/telemetry/otlp/0002_file_cursors.sql"),
    include_str!("../../migrations/telemetry/otlp/0003_attempt_tokens.sql"),
];
pub const MAX_BODY: usize = 4 * 1024 * 1024;
const MAX_RECORDS: usize = 4096;

#[derive(clap::Subcommand)]
pub enum Command {
    /// Serve for a bounded lifetime; port 0 selects an ephemeral loopback port.
    Serve {
        #[arg(long, default_value_t = 4318)]
        port: u16,
        #[arg(long, default_value_t = 3600)]
        seconds: u64,
        #[arg(long, default_value_t = 10000)]
        max_requests: usize,
    },
    /// Read sanitized records, including unbound and unmapped diagnostics.
    Records,
    /// Mint a scoped bearer credential and print it once.
    MintToken {
        #[arg(long)]
        attempt: String,
        #[arg(long, default_value_t = 3600)]
        seconds: u64,
    },
    /// Revoke using the nonsecret token hash returned at mint time.
    RevokeToken {
        #[arg(long)]
        token_hash: String,
    },
}

// Native names and allowlisted attribute paths are shared with capabilities.
const MAPPINGS: &[(&str, &str, &str, &[&str])] = &[
    (
        "claude-code",
        "claude_code.token.usage",
        "usage",
        &["type", "model"],
    ),
    ("claude-code", "claude_code.cost.usage", "usage", &["model"]),
    (
        "claude-code",
        "claude_code.api_request",
        "usage",
        &[
            "model",
            "input_tokens",
            "output_tokens",
            "cache_read_tokens",
            "cache_creation_tokens",
            "duration_ms",
            "cost_usd",
        ],
    ),
    (
        "claude-code",
        "claude_code.tool_result",
        "tool",
        &["tool_name", "success", "duration_ms"],
    ),
    (
        "gemini-cli",
        "gemini_cli.token.usage",
        "usage",
        &["type", "model"],
    ),
    (
        "gemini-cli",
        "gemini_cli.api_response",
        "usage",
        &[
            "model",
            "input_token_count",
            "output_token_count",
            "cached_content_token_count",
            "thoughts_token_count",
            "tool_token_count",
            "total_token_count",
            "duration_ms",
        ],
    ),
    (
        "gemini-cli",
        "gemini_cli.tool_call",
        "tool",
        &["function_name", "success", "duration_ms"],
    ),
    ("muse", "model_call", "usage", &["gen_ai.request.model", "gen_ai.provider.name", "gen_ai.usage.input_tokens", "gen_ai.usage.output_tokens", "tokens.cached", "cache_read_tokens", "cache_write_tokens", "reasoning_tokens", "duration_ms"]),
    ("muse", "tbh.approval_review.token_usage", "usage", &["token_type"]),
    ("grok", "grok_code.api_request", "usage", &["model", "input_tokens", "output_tokens", "reasoning_tokens", "cache_read_tokens", "cache_creation_tokens", "cost_usd_micros", "duration_ms", "turn_number", "stop_reason"]),
    ("grok", "grok_code.token.usage", "usage", &["type", "model"]),
    ("grok", "grok_code.cost.usage", "usage", &["model"]),
    ("grok", "grok_code.session.count", "usage", &[]),
    ("grok", "grok_code.turn.count", "usage", &["outcome", "model"]),
    ("grok", "grok_code.tool.usage", "tool", &["tool_name", "outcome"]),
    ("grok", "grok_code.error.count", "tool", &["error_category", "model"]),
];

const GROK_LIVE_FIELDS: &[&str] = &["model", "input_tokens", "output_tokens", "reasoning_tokens", "cache_read_tokens", "cache_creation_tokens"];

pub fn capabilities() -> Vec<Value> {
    let mut out = Vec::new();
    for harness in ["claude-code", "gemini-cli", "grok", "muse", "codex"] {
        let mut fields = Vec::new();
        for (_, name, _, attrs) in MAPPINGS.iter().filter(|m| m.0 == harness) {
            for field in attrs.iter().copied().chain(if name.ends_with(".usage") || name.ends_with("token_usage") || (harness == "grok" && *name != "grok_code.api_request") {
                vec![
                    "value",
                    "timeUnixNano",
                    "startTimeUnixNano",
                    "aggregationTemporality",
                    "unit",
                ]
            } else {
                vec!["timeUnixNano"]
            }) {
                // Grok 1.0.46 live run (docs/telemetry/grok-live-1.0.46.md) observed the
                // api_request usage counters and model; everything else stays fixture.
                let live = harness == "grok" && *name == "grok_code.api_request" && GROK_LIVE_FIELDS.contains(&field);
                fields.push(json!({"kind":name,"field":field,"available":true,"basis":if matches!(field,"model"|"tool_name"|"function_name"|"error_category"|"outcome"|"gen_ai.request.model"|"gen_ai.provider.name"|"token_type") {"reported_excerpt"} else {"reported"},"certified":if live {"live"} else {"fixture"},"live_versions":if live {json!(["1.0.46"])} else {json!([])},"caveat":"native_scope_only_no_cross_surface_sum","reason":null}));
            }
        }
        for field in [
            "prompt",
            "response_text",
            "tool_input",
            "tool_output",
            "function_args",
            "tool_parameters",
            "file_contents",
        ] {
            fields.push(json!({"kind":"content","field":field,"available":false,"basis":"unavailable","certified":"none","caveat":null,"reason":"content_forbidden"}));
        }
        for field in ["service.name", "herdr.attempt_id"] {
            fields.push(json!({"kind":"resource","field":field,"available":harness!="codex","basis":if harness=="codex" {"unavailable"} else {"reported"},"certified":if harness=="codex" {"none"} else {"fixture"},"caveat":"exact_binding_only","reason":null}));
        }
        if harness == "codex" {
            fields.insert(0,json!({"kind":"otlp","field":"*","available":false,"basis":"unavailable","certified":"none","caveat":null,"reason":"names_not_certified_use_rollout_adapter"}));
        }
        out.push(json!({"adapter":format!("otlp:{harness}"),"interface":"otlp_http_json_protobuf","certified_versions":[],"uncertified_version":"fixture_only","fields":fields}));
        if harness == "muse" {
            let cap = out.last_mut().unwrap();
            cap["fixture_versions"] = json!(["1.4.0-R4161.1"]);
            cap["accepted_versions"] = json!(["1.4.0-R4161.1"]);
            cap["native_source"] = json!({"certified":"none", "reason":"local_usage_schema_not_established"});
        }
        if harness == "grok" {
            let cap = out.last_mut().unwrap();
            cap["fixture_versions"] = json!(["1.0.46"]);
            cap["certified_versions"] = json!(["1.0.46"]);
            cap["accepted_versions"] = json!(["1.0.46"]);
            cap["native_source"] = json!({"certified":"none", "reason":"local_usage_schema_not_established"});
        }
    }
    out
}

fn array<'a>(v: &'a Value, key: &str) -> Result<&'a Vec<Value>> {
    v.get(key)
        .and_then(Value::as_array)
        .with_context(|| format!("invalid {key}"))
}
fn attributes(v: &Value) -> Result<BTreeMap<String, Value>> {
    ensure!(v.is_object(), "invalid attribute container");
    let mut out = BTreeMap::new();
    if v.get("attributes").is_none() {
        return Ok(out);
    }
    ensure!(array(v, "attributes")?.len() <= 128, "too many attributes");
    for a in array(v, "attributes")? {
        let key = a["key"].as_str().context("invalid attribute key")?;
        ensure!(!out.contains_key(key), "duplicate attribute key");
        let object = a["value"].as_object().context("invalid AnyValue")?;
        ensure!(object.len() == 1, "invalid AnyValue");
        let (ty, value) = object.iter().next().context("empty AnyValue")?;
        let value = match ty.as_str() {
            "stringValue" if value.is_string() => value.clone(),
            "boolValue" if value.is_boolean() => value.clone(),
            "intValue" => json!(integer(value).context("invalid integer attribute")?),
            "doubleValue" if value.is_number() => value.clone(),
            "arrayValue" | "kvlistValue" | "bytesValue" => Value::Null,
            _ => anyhow::bail!("invalid AnyValue type"),
        };
        out.insert(key.to_owned(), value);
    }
    Ok(out)
}
fn integer(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_str()?.parse().ok())
}
fn number(v: &Value) -> Option<Value> {
    if v.as_str().is_some_and(|s| s.len() > 64) {
        return None;
    }
    if let Some(n) = integer(v) {
        return (n >= 0).then(|| json!(n));
    }
    v.as_f64()
        .or_else(|| v.as_str()?.parse().ok())
        .filter(|n| n.is_finite() && *n >= 0.0)
        .map(|_| v.clone())
}
fn identifier(v: &Value) -> Option<Value> {
    let s = v.as_str()?;
    (s.len() <= 128 && !s.is_empty() && !s.contains(char::is_control))
        .then(|| json!(super::sanitize::excerpt(s)))
}
fn timestamp(v: &Value, key: &str) -> Result<Value> {
    match v.get(key) {
        None => Ok(Value::Null),
        Some(n) => {
            let text = n
                .as_str()
                .map(str::to_owned)
                .or_else(|| n.as_u64().map(|n| n.to_string()))
                .context("invalid timestamp")?;
            ensure!(
                text.len() <= 20
                    && text.bytes().all(|b| b.is_ascii_digit())
                    && text.parse::<u64>().is_ok(),
                "invalid timestamp"
            );
            Ok(json!(text))
        }
    }
}

/// Public collector/store entry point. Validate the entire request before opening
/// the sidecar; one transaction, sanitized-record digest identity, no raw spool.
/// This API deliberately accepts no HTTP credentials: the transport authenticates
/// before calling it. Like public sidecar APIs it is for trusted local callers.
pub fn ingest(project: &Path, endpoint: &str, bytes: &[u8]) -> Result<usize> {
    ensure!(bytes.len() <= MAX_BODY, "body oversized");
    let root: Value = serde_json::from_slice(bytes).context("malformed JSON")?;
    ingest_root(project, endpoint, root, None)
}

/// Public transport-equivalent API: authenticate a scoped token before decoding.
pub fn ingest_attempt(
    project: &Path,
    endpoint: &str,
    bytes: &[u8],
    content_type: &str,
    token: &str,
) -> Result<usize> {
    let attempt = authenticate_attempt(project, token)?.context("unauthorized attempt token")?;
    ingest_encoded(project, endpoint, bytes, content_type, Some(&attempt))
}

/// Trusted local protobuf collector entry point, sharing JSON validation/storage.
pub fn ingest_protobuf(project: &Path, endpoint: &str, bytes: &[u8]) -> Result<usize> {
    ingest_encoded(project, endpoint, bytes, "application/x-protobuf", None)
}
fn ingest_encoded(
    project: &Path,
    endpoint: &str,
    bytes: &[u8],
    content_type: &str,
    attempt: Option<&str>,
) -> Result<usize> {
    ensure!(bytes.len() <= MAX_BODY, "body oversized");
    let root = match content_type {
        "application/json" => serde_json::from_slice(bytes).context("malformed JSON")?,
        "application/x-protobuf" => protobuf::request(endpoint, bytes)?,
        _ => anyhow::bail!("unsupported content type"),
    };
    ingest_root(project, endpoint, root, attempt)
}

fn ingest_root(project: &Path, endpoint: &str, root: Value, token_attempt: Option<&str>) -> Result<usize> {
    let metrics = match endpoint {
        "/v1/logs" => false,
        "/v1/metrics" => true,
        _ => anyhow::bail!("unsupported endpoint"),
    };
    let resources = array(
        &root,
        if metrics {
            "resourceMetrics"
        } else {
            "resourceLogs"
        },
    )?;
    let attempts = super::codex::canonical_attempts(project)?;
    let mut records = Vec::new();
    for resource in resources {
        ensure!(resource.is_object(), "invalid resource group");
        let ra = attributes(resource.get("resource").unwrap_or(&json!({})))?;
        let service = ra.get("service.name").and_then(Value::as_str).unwrap_or("");
        let harness = if service == "grok-cli" { "grok" } else if service == "tbh" { "muse" } else { service };
        let service_version = ra.get("service.version").and_then(Value::as_str);
        let cli_version = if harness == "grok" {
            // A present client version is authoritative, including an unknown version.
            ra.get("client.version").map(|v| v.as_str()).unwrap_or_else(|| {
                service_version.map(|v| v.split_whitespace().next().unwrap_or(""))
            })
        } else { service_version };
        let known = matches!(service, "claude-code" | "gemini-cli" | "grok-cli" | "tbh");
        let adapter = if known {
            format!("otlp:{harness}")
        } else if harness == "codex" {
            "otlp:codex".into()
        } else {
            "otlp:unknown".into()
        };
        let attempted = (!harness.is_empty())
            .then(|| ra.get("herdr.attempt_id").and_then(Value::as_str))
            .flatten();
        let named_attempt = ra.get("herdr.attempt_id");
        ensure!(named_attempt.is_none_or(Value::is_string), "invalid attempt attribute");
        let conflict = token_attempt.is_some_and(|id| named_attempt.and_then(Value::as_str).is_some_and(|named| named != id));
        let attempt = if conflict { None } else { token_attempt.or(attempted).filter(|id| attempts.iter().any(|a| a.id == *id)) };
        let binding = if conflict {
            "unknown_attempt"
        } else if attempt.is_some() {
            "exact"
        } else if attempted.is_some() {
            "unknown_attempt"
        } else {
            "unbound"
        };
        for scope in array(resource, if metrics { "scopeMetrics" } else { "scopeLogs" })? {
            for entry in array(scope, if metrics { "metrics" } else { "logRecords" })? {
                let ea = attributes(entry)?;
                // DG4a: a JSON log body may carry only the event name (Claude Code,
                // Gemini CLI); it is used as a name and never stored. Protobuf
                // bodies are discarded at decode, so Grok/Muse use eventName.
                let name = if metrics {
                    entry["name"].as_str()
                } else {
                    entry["eventName"]
                        .as_str()
                        .or_else(|| entry["body"]["stringValue"].as_str())
                        .or_else(|| ea.get("event.name").and_then(Value::as_str))
                }
                .unwrap_or("");
                let unsupported = metrics
                    && (entry.get("exponentialHistogram").is_some() || entry.get("summary").is_some());
                if metrics {
                    ensure!(
                        ["sum", "gauge", "histogram", "exponentialHistogram", "summary"]
                            .iter().filter(|key| entry.get(**key).is_some()).count() == 1,
                        "invalid metric data"
                    );
                }
                let mapping = MAPPINGS
                    .iter()
                    .find(|m| !unsupported && known && m.0 == harness && m.1 == name && metrics == (name.ends_with(".usage") || name.ends_with("token_usage") || (m.0 == "grok" && m.1 != "grok_code.api_request"))
                        && (harness != "grok" || cli_version == Some("1.0.46"))
                        && (harness != "muse" || cli_version == Some("1.4.0-R4161.1")));
                let points = if unsupported {
                    // One keys-only diagnostic per instrument; never inspect unsupported points.
                    vec![json!({})]
                } else if metrics {
                    let data = entry
                        .get("sum")
                        .or_else(|| entry.get("gauge"))
                        .or_else(|| entry.get("histogram"))
                        .context("invalid metric data")?;
                    array(data, "dataPoints")?.clone()
                } else {
                    vec![entry.clone()]
                };
                for point in points {
                    ensure!(records.len() < MAX_RECORDS, "too many records");
                    let attrs = attributes(&point)?;
                    let mapping = mapping.filter(|m| {
                        !m.1.ends_with("token.usage")
                            || attrs.get("type").and_then(Value::as_str).is_some_and(|s| {
                                if harness == "claude-code" {
                                    matches!(s, "input" | "output" | "cacheRead" | "cacheCreation")
                                } else if harness == "grok" {
                                    matches!(s, "input" | "output" | "reasoning" | "cache_read" | "cache_creation")
                                } else {
                                    matches!(s, "input" | "output" | "cache" | "thought" | "tool")
                                }
                            })
                    });
                    let mut payload = json!({"adapter":adapter,"attempt_id":attempt,"binding":binding,"source_trust":"collector_observed","certified":"fixture", "timeUnixNano":timestamp(&point,"timeUnixNano")?});
                    if !metrics {
                        if point.get("observedTimeUnixNano").is_some() {
                            payload["observedTimeUnixNano"] = timestamp(&point, "observedTimeUnixNano")?;
                        }
                        if let Some(severity) = point.get("severityNumber") {
                            ensure!(severity.as_u64().is_some_and(|n| n <= 24), "invalid severity");
                            payload["severityNumber"] = severity.clone();
                        }
                    }
                    if harness == "muse" {
                        payload["cli_version"] = if cli_version == Some("1.4.0-R4161.1") { json!("1.4.0-R4161.1") } else { Value::Null };
                        if cli_version != Some("1.4.0-R4161.1") {
                            payload["mapping_certified"] = json!("none");
                            payload["reason"] = json!("cli_version_uncertified");
                        }
                    }
                    if harness == "grok" {
                        if cli_version == Some("1.0.46") {
                            // Only the reviewed build-string shape may leave the exporter.
                            if let Some(build) = service_version.filter(|v| {
                                v.starts_with("1.0.46 (") && v.ends_with(')')
                                    && v[8..v.len()-1].chars().all(|c| c.is_ascii_hexdigit())
                                    && v.len() <= 80
                            }) { payload["service_build"] = json!(build); }
                        }
                        payload["cli_version"] = if cli_version == Some("1.0.46") { json!("1.0.46") } else { Value::Null };
                        if cli_version != Some("1.0.46") {
                            payload["mapping_certified"] = json!("none");
                            payload["reason"] = json!("cli_version_uncertified");
                        }
                    }
                    if unsupported {
                        payload["reason"] = json!("unsupported_metric_type");
                        if let Some(name) = identifier(&json!(name)) {
                            payload["native_name"] = name;
                        }
                    }
                    let mut allowed = BTreeMap::new();
                    let mut invalid_usage = false;
                    if let Some((_, native, kind, fields)) = mapping {
                        payload["native_name"] = json!(native);
                        payload["kind"] = json!(kind);
                        for field in *fields {
                            if let Some(raw) = attrs.get(*field) {
                                let safe = match *field {
                                    "stop_reason" | "model" | "tool_name" | "function_name" | "error_category" | "outcome" | "gen_ai.request.model" | "gen_ai.provider.name" => identifier(raw),
                                    "token_type" => raw.as_str().filter(|s| matches!(*s, "total" | "input" | "cached_input" | "output")).map(|s| json!(s)),
                                    "type" => raw
                                        .as_str()
                                        .filter(|s| {
                                            matches!(
                                                *s,
                                                "input"
                                                    | "output"
                                                    | "cacheRead"
                                                    | "cacheCreation"
                                                    | "cache"
                                                    | "thought"
                                                    | "tool"
                                                    | "reasoning"
                                                    | "cache_read"
                                                    | "cache_creation"
                                            )
                                        })
                                        .map(|s| json!(s)),
                                    "success" => raw
                                        .as_bool()
                                        .or_else(|| match raw.as_str()? {
                                            "true" => Some(true),
                                            "false" => Some(false),
                                            _ => None,
                                        })
                                        .map(|b| json!(b)),
                                    _ => number(raw),
                                };
                                if safe.is_none() && harness == "grok" && !metrics && *native == "grok_code.api_request" {
                                    invalid_usage = true;
                                    continue;
                                }
                                ensure!(safe.is_some(), "invalid mapped attribute");
                                allowed.insert(*field, safe.unwrap());
                            }
                        }
                        payload["attributes"] = json!(allowed);
                        if harness == "grok" {
                            if metrics {
                                if *kind == "usage" {
                                    // Reconciliation observations never count as API usage.
                                    payload["kind"] = json!("unmapped");
                                    payload["reason"] = json!("usage_reconciliation_only");
                                }
                            } else {
                                let session = attrs.get("session.id").and_then(Value::as_str).filter(|s| !s.is_empty());
                                let sequence = attrs.get("event.sequence").and_then(integer).filter(|n| *n >= 0);
                                let prompt = attrs.get("prompt.id").and_then(Value::as_str).filter(|s| !s.is_empty());
                                let turn = attrs.get("turn_number").and_then(integer).filter(|n| *n >= 0);
                                let key = session.zip(sequence).map(|(s, n)| json!(["session_sequence", s, n, prompt, turn]))
                                    .or_else(|| prompt.zip(turn).map(|(p, n)| json!(["prompt_turn", p, n])));
                                if let Some(key) = key {
                                    payload["usage_source_key"] = json!(format!("{:x}", Sha256::digest(serde_json::to_vec(&key)?)));
                                    payload["usage_authority"] = json!("api_request");
                                    let counters = (|| {
                                        let a = &payload["attributes"];
                                        let input = a["input_tokens"].as_u64()?;
                                        let output = a["output_tokens"].as_u64()?;
                                        let cached = a["cache_read_tokens"].as_u64()?;
                                        let creation = a["cache_creation_tokens"].as_u64()?;
                                        let reasoning = a["reasoning_tokens"].as_u64()?;
                                        a["cost_usd_micros"].as_u64()?;
                                        if cached.checked_add(creation)? > input || reasoning > output {
                                            return None;
                                        }
                                        Some((cached, input.checked_add(output)?))
                                    })();
                                    if let Some((cached, total)) = counters {
                                        payload["attributes"]["cached_input_tokens"] = json!(cached);
                                        payload["attributes"]["cache_write_input_tokens"] = payload["attributes"]["cache_creation_tokens"].clone();
                                        payload["attributes"]["reasoning_output_tokens"] = payload["attributes"]["reasoning_tokens"].clone();
                                        payload["attributes"]["total_tokens"] = json!(total);
                                    } else {
                                        invalid_usage = true;
                                    }
                                } else {
                                    payload["kind"] = json!("unmapped");
                                    payload["reason"] = json!("missing_usage_identity");
                                }
                            }
                        }
                        if metrics {
                            if entry.get("histogram").is_some() {
                                let count = point.get("count").and_then(|n| n.as_u64().or_else(|| n.as_str()?.parse::<u64>().ok())).context("invalid histogram count")?;
                                payload["count"] = json!(count);
                                if let Some(sum) = point.get("sum") {
                                    payload["sum"] = number(sum).context("invalid histogram sum")?;
                                }
                            } else {
                                ensure!(point.get("asInt").is_some() != point.get("asDouble").is_some(), "invalid metric value fields");
                                payload["value"] = if let Some(raw) = point.get("asInt") {
                                    integer(raw).filter(|n| *n >= 0).map(|n| json!(n))
                                } else { point.get("asDouble").filter(|v| v.is_number()).and_then(number) }.context("invalid metric value")?;
                            }
                            payload["startTimeUnixNano"] = timestamp(&point, "startTimeUnixNano")?;
                            if entry.get("gauge").is_none() {
                                let data = entry.get("sum").or_else(|| entry.get("histogram")).unwrap();
                                let temp = data["aggregationTemporality"].clone();
                                ensure!(matches!(temp.as_i64(), Some(1 | 2)) || matches!(temp.as_str(), Some("AGGREGATION_TEMPORALITY_DELTA" | "AGGREGATION_TEMPORALITY_CUMULATIVE")), "unknown counter temporality");
                                payload["aggregationTemporality"] = temp;
                            }
                            // Reviewed units only; never retain arbitrary exporter text.
                            let unit = entry["unit"].as_str().unwrap_or("");
                            ensure!(
                                matches!(unit, "" | "USD" | "{token}" | "token" | "tokens")
                                    || (harness == "grok" && matches!(unit, "{session}" | "{turn}" | "{call}" | "{error}")),
                                "unsupported unit"
                            );
                            payload["unit"] = json!(unit);
                        }
                    } else {
                        payload["kind"] = json!("unmapped");
                    }
                    if invalid_usage {
                        payload["kind"] = json!("unmapped");
                        payload["reason"] = json!("invalid_usage_counters");
                        let object = payload.as_object_mut().unwrap();
                        object.remove("attributes");
                        object.remove("usage_authority");
                        object.remove("usage_source_key");
                    }
                    super::accounting::otlp::declare_usage(&mut payload, harness, cli_version, metrics);
                    let allowed_keys = if invalid_usage { &[][..] } else { mapping.map_or(&[][..], |m| m.3) };
                    let keys: Vec<_> = attrs
                        .keys()
                        .filter(|k| {
                            !allowed_keys.contains(&k.as_str()) && k.as_str() != "event.name"
                        })
                        .filter_map(|k| identifier(&json!(k)))
                        .collect();
                    let resource_keys: Vec<_> = ra
                        .keys()
                        .filter(|k| !matches!(k.as_str(), "service.name" | "herdr.attempt_id"))
                        .filter_map(|k| identifier(&json!(k)))
                        .collect();
                    payload["unmapped_attribute_keys"] = json!(keys);
                    payload["unmapped_resource_keys"] = json!(resource_keys);
                    if conflict { payload["reason"] = json!("cross_attempt_quarantined"); }
                    records.push(payload);
                }
            }
        }
    }
    let _lock = super::maintenance::lock(project, false)?;
    let mut db = super::sidecar::open(project, true)?.context("sidecar unavailable")?;
    let tx = db.transaction()?;
    let now = jiff::Timestamp::now().as_millisecond();
    let mut written = 0;
    for record in records {
        let text = serde_json::to_string(&record)?;
        let identity = if record["usage_authority"] == "api_request" {
            format!("grok-api:{:x}", Sha256::digest(serde_json::to_vec(&json!([
                record["attempt_id"], record["binding"], record["usage_source_key"]
            ]))?))
        } else { format!("sha256:{:x}", Sha256::digest(text.as_bytes())) };
        written += tx.execute("INSERT OR IGNORE INTO otlp_records(identity,adapter,attempt_id,binding,kind,source_trust,certified,record,observed_unix_ms) VALUES(?1,?2,?3,?4,?5,'collector_observed','fixture',?6,?7)",
            rusqlite::params![identity,record["adapter"].as_str(),record["attempt_id"].as_str(),record["binding"].as_str(),record["kind"].as_str(),text,now])?;
    }
    tx.commit()?;
    Ok(written)
}

pub fn records(project: &Path) -> Result<Value> {
    let Some(db) = super::sidecar::read(project)? else {
        return Ok(json!([]));
    };
    let version: i64 = db.query_row(
        "SELECT coalesce((SELECT version FROM telemetry_streams WHERE stream='otlp'),0)",
        [],
        |r| r.get(0),
    )?;
    if version == 0 {
        return Ok(json!([]));
    }
    let rows = db
        .prepare("SELECT record FROM otlp_records ORDER BY identity")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!(
        rows.into_iter()
            .map(|s| serde_json::from_str::<Value>(&s))
            .collect::<serde_json::Result<Vec<_>>>()?
    ))
}

fn project_hash(project: &Path) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(
            std::fs::canonicalize(project)?
                .as_os_str()
                .as_encoded_bytes()
        )
    ))
}
fn secret_hash(secret: &str) -> String {
    format!("{:x}", Sha256::digest(secret.as_bytes()))
}

/// Mint once, persist only the hash, and restrict authority to an existing attempt.
pub fn mint_attempt_token(project: &Path, attempt: &str, seconds: u64) -> Result<Value> {
    ensure!((1..=86400).contains(&seconds), "invalid token lifetime");
    ensure!(
        super::codex::canonical_attempts(project)?
            .iter()
            .any(|a| a.id == attempt),
        "unknown attempt"
    );
    let mut entropy = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut entropy)?;
    let secret: String = entropy.iter().map(|b| format!("{b:02x}")).collect();
    let hash = secret_hash(&secret);
    let now = jiff::Timestamp::now().as_millisecond();
    let expires = now + (seconds as i64) * 1000;
    let _lock = super::maintenance::lock(project, false)?;
    let db = super::sidecar::open(project, true)?.context("sidecar unavailable")?;
    db.execute("INSERT INTO otlp_attempt_tokens(token_hash,project_hash,attempt_id,expires_unix_ms,created_unix_ms) VALUES(?1,?2,?3,?4,?5)", rusqlite::params![hash, project_hash(project)?, attempt, expires, now])?;
    Ok(json!({"token":secret,"token_hash":hash,"attempt_id":attempt,"expires_unix_ms":expires}))
}

pub fn revoke_attempt_token(project: &Path, hash: &str) -> Result<()> {
    let _lock = super::maintenance::lock(project, false)?;
    let db = super::sidecar::open(project, true)?.context("sidecar unavailable")?;
    ensure!(db.execute("UPDATE otlp_attempt_tokens SET revoked_unix_ms=?1 WHERE token_hash=?2 AND project_hash=?3", rusqlite::params![jiff::Timestamp::now().as_millisecond(), hash, project_hash(project)?])? == 1, "unknown token hash");
    Ok(())
}
fn authenticate_attempt(project: &Path, secret: &str) -> Result<Option<String>> {
    use rusqlite::OptionalExtension;
    if secret.len() != 64 || !secret.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(None);
    }
    let Some(db) = super::sidecar::read(project)? else {
        return Ok(None);
    };
    let exists: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='otlp_attempt_tokens')", [], |r| r.get(0))?;
    if !exists {
        return Ok(None);
    }
    Ok(db.query_row("SELECT attempt_id FROM otlp_attempt_tokens WHERE token_hash=?1 AND project_hash=?2 AND revoked_unix_ms IS NULL AND expires_unix_ms>?3", rusqlite::params![secret_hash(secret), project_hash(project)?, jiff::Timestamp::now().as_millisecond()], |r| r.get(0)).optional()?)
}

/// Token scope includes the canonical project path, not merely a reusable slug.
pub fn token_path(project: &Path, config: &Path) -> Result<std::path::PathBuf> {
    let path = std::fs::canonicalize(project)?;
    Ok(config.join(format!(
        "otlp-{:x}.token",
        Sha256::digest(path.as_os_str().as_encoded_bytes())
    )))
}
fn token(project: &Path, config: &Path) -> Result<String> {
    let path = token_path(project, config)?;
    std::fs::create_dir_all(config)?;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
    {
        Ok(mut file) => {
            let mut entropy = [0u8; 32];
            std::fs::File::open("/dev/urandom")?.read_exact(&mut entropy)?;
            let secret = entropy
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
            file.write_all(secret.as_bytes())?;
            file.sync_all()?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)?;
    let meta = file.metadata()?;
    // SAFETY: geteuid has no pointer arguments.
    ensure!(
        meta.is_file() && meta.mode() & 0o777 == 0o600 && meta.uid() == unsafe { libc::geteuid() },
        "OTLP token must be owned by this user with mode 0600"
    );
    let mut secret = String::new();
    Read::by_ref(&mut file)
        .take(65)
        .read_to_string(&mut secret)?;
    ensure!(
        secret.len() == 64 && secret.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid OTLP token"
    );
    Ok(secret)
}

fn request(stream: &mut TcpStream, secret: &str, project: &Path, remaining: Duration) -> u16 {
    fn parse(
        stream: &mut TcpStream,
        secret: &str,
        project: &Path,
        remaining: Duration,
    ) -> std::result::Result<u16, u16> {
        let deadline = Instant::now() + remaining.min(Duration::from_secs(2));
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            if head.len() >= 16384 {
                return Err(400);
            }
            let timeout = deadline.saturating_duration_since(Instant::now());
            if timeout.is_zero() {
                return Err(400);
            }
            stream.set_read_timeout(Some(timeout)).map_err(|_| 400u16)?;
            let mut byte = [0];
            stream.read_exact(&mut byte).map_err(|_| 400u16)?;
            head.push(byte[0]);
        }
        let text = std::str::from_utf8(&head).map_err(|_| 400u16)?;
        let mut lines = text.split("\r\n");
        let first: Vec<_> = lines
            .next()
            .unwrap_or_default()
            .split_whitespace()
            .collect();
        if first.len() != 3 || first[0] != "POST" || first[2] != "HTTP/1.1" {
            return Err(400);
        }
        if !matches!(first[1], "/v1/logs" | "/v1/metrics") {
            return Err(404);
        }
        let mut headers = BTreeMap::new();
        for line in lines.filter(|s| !s.is_empty()) {
            let (key, val) = line.split_once(':').ok_or(400u16)?;
            if headers
                .insert(key.to_ascii_lowercase(), val.trim())
                .is_some()
            {
                return Err(400);
            }
        }
        let expected = format!("Bearer {secret}");
        let supplied = headers.get("authorization").copied().unwrap_or("");
        let diff = supplied
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |a, (x, y)| a | (x ^ y));
        let attempt = if supplied.len() == expected.len() && diff == 0 { None } else {
            let bearer = supplied.strip_prefix("Bearer ").ok_or(401u16)?;
            Some(authenticate_attempt(project, bearer).map_err(|_| 503u16)?.ok_or(401u16)?)
        };
        if headers.contains_key("transfer-encoding") || headers.contains_key("content-encoding") {
            return Err(415);
        }
        let content_type = headers.get("content-type").copied().unwrap_or("");
        if !matches!(content_type, "application/json" | "application/x-protobuf") {
            return Err(415);
        }
        let length = headers
            .get("content-length")
            .ok_or(400u16)?
            .parse::<usize>()
            .map_err(|_| 400u16)?;
        if length > MAX_BODY {
            return Err(413);
        }
        let mut body = vec![0; length];
        let mut read = 0;
        while read < length {
            let timeout = deadline.saturating_duration_since(Instant::now());
            if timeout.is_zero() {
                return Err(400);
            }
            stream.set_read_timeout(Some(timeout)).map_err(|_| 400u16)?;
            let n = stream.read(&mut body[read..]).map_err(|_| 400u16)?;
            if n == 0 {
                return Err(400);
            }
            read += n;
        }
        ingest_encoded(project, first[1], &body, content_type, attempt.as_deref()).map_err(|e| {
            if e.downcast_ref::<rusqlite::Error>().is_some()
                || e.downcast_ref::<std::io::Error>().is_some()
            {
                503u16
            } else {
                400u16
            }
        })?;
        Ok(200)
    }
    parse(stream, secret, project, remaining).unwrap_or_else(|s| s)
}

pub fn serve(
    project: &Path,
    config: &Path,
    port: u16,
    seconds: u64,
    max_requests: usize,
) -> Result<()> {
    ensure!(
        (1..=86400).contains(&seconds) && (1..=100000).contains(&max_requests),
        "invalid serve bounds"
    );
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))?;
    let secret = token(project, config)?;
    listener.set_nonblocking(true)?;
    println!(
        "{}",
        json!({"address":listener.local_addr()?.to_string(),"token_file":token_path(project,config)?})
    );
    std::io::stdout().flush()?;
    let end = Instant::now() + Duration::from_secs(seconds);
    let mut window = Instant::now();
    let mut rate = 0;
    let mut requests = 0;
    while Instant::now() < end && requests < max_requests {
        let (mut stream, _) = match listener.accept() {
            Ok(pair) => pair,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        requests += 1;
        if window.elapsed() >= Duration::from_secs(1) {
            window = Instant::now();
            rate = 0;
        }
        rate += 1;
        let status = if rate > 30 {
            429
        } else {
            request(
                &mut stream,
                &secret,
                project,
                end.saturating_duration_since(Instant::now()),
            )
        };
        stream.set_write_timeout(Some(Duration::from_millis(200)))?;
        let reason = match status {
            200 => "OK",
            400 => "Bad Request",
            401 => "Unauthorized",
            413 => "Payload Too Large",
            415 => "Unsupported Media Type",
            429 => "Too Many Requests",
            503 => "Service Unavailable",
            _ => "Not Found",
        };
        let _ = write!(
            stream,
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
        );
    }
    Ok(())
}
pub fn run(project: &Path, config: &Path, command: Command) -> Result<()> {
    match command {
        Command::Serve {
            port,
            seconds,
            max_requests,
        } => serve(project, config, port, seconds, max_requests),
        Command::MintToken { attempt, seconds } => {
            println!("{}", mint_attempt_token(project, &attempt, seconds)?);
            Ok(())
        }
        Command::RevokeToken { token_hash } => {
            revoke_attempt_token(project, &token_hash)?;
            println!("{}", json!({"revoked":true,"token_hash":token_hash}));
            Ok(())
        },
        Command::Records => {
            println!("{}", serde_json::to_string_pretty(&records(project)?)?);
            Ok(())
        }
    }
}
pub fn metrics(_: &Path, _: Option<i64>) -> Result<BTreeMap<String, Value>> {
    Ok(BTreeMap::new())
}
pub fn tick(_: &Path, _: super::codex::Budget) -> Result<()> {
    Ok(())
}

fn configured_settings(project: &Path, config: &Path) -> Result<Option<toml::Value>> {
    let path = config.join("config.toml");
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut text = String::new();
    file.take(65537).read_to_string(&mut text)?;
    ensure!(text.len() <= 65536, "OTLP config oversized");
    let parsed: toml::Value = toml::from_str(&text)?;
    let slug = project
        .file_name()
        .and_then(|s| s.to_str())
        .context("invalid project slug")?;
    let settings = parsed
        .get("telemetry")
        .and_then(|t| t.get("otlp"))
        .and_then(|t| t.get("projects"))
        .and_then(|p| p.get(slug));
    Ok(settings.filter(|s| s.get("enabled").and_then(toml::Value::as_bool) == Some(true)).cloned())
}

/// Explicit receiver configuration also makes a project eligible for a ticker
/// worker, even before its first OTLP observation has created a sidecar.
pub fn configured(project: &Path, config: &Path) -> Result<bool> {
    Ok(configured_settings(project, config)?.is_some())
}

/// Optional ticker receiver: independent bounded thread, one listener per project.
/// Configuration is scoped by slug; absent configuration performs no socket work.
pub fn start_configured(project: &Path, config: &Path) -> Result<()> {
    static RUNNING: std::sync::Mutex<
        Option<BTreeMap<std::path::PathBuf, std::thread::JoinHandle<()>>>,
    > = std::sync::Mutex::new(None);
    let Some(settings) = configured_settings(project, config)? else { return Ok(()); };
    let port = settings
        .get("port")
        .and_then(toml::Value::as_integer)
        .unwrap_or(4318);
    let port = u16::try_from(port).context("invalid OTLP port")?;
    let mut guard = RUNNING
        .lock()
        .map_err(|_| anyhow::anyhow!("OTLP thread registry poisoned"))?;
    let running = guard.get_or_insert_with(BTreeMap::new);
    if running.get(project).is_some_and(|h| !h.is_finished()) {
        return Ok(());
    }
    if let Some(old) = running.remove(project) {
        let _ = old.join();
    }
    let (project, config) = (project.to_path_buf(), config.to_path_buf());
    let key = project.clone();
    let handle = std::thread::Builder::new()
        .name("telemetry-otlp".into())
        .spawn(move || {
            super::background::idle_priority(|warning| eprintln!("{warning}"));
            if let Err(error) = serve(&project, &config, port, 300, 10000) {
                eprintln!("OTLP receiver: {error}");
            }
        })?;
    running.insert(key, handle);
    Ok(())
}

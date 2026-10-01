//! DG4a: bounded OTLP/HTTP JSON, allowlist before durable storage.
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
pub const MIGRATIONS: &[&str] = &[include_str!(
    "../../migrations/telemetry/otlp/0001_records.sql"
), include_str!("../../migrations/telemetry/otlp/0002_file_cursors.sql")];
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
];

pub fn capabilities() -> Vec<Value> {
    let mut out = Vec::new();
    for harness in ["claude-code", "gemini-cli", "codex"] {
        let mut fields = Vec::new();
        for (_, name, _, attrs) in MAPPINGS.iter().filter(|m| m.0 == harness) {
            for field in attrs.iter().copied().chain(if name.ends_with(".usage") {
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
                fields.push(json!({"kind":name,"field":field,"available":true,"basis":if matches!(field,"model"|"tool_name"|"function_name") {"reported_excerpt"} else {"reported"},"certified":"fixture","caveat":"native_scope_only_no_cross_surface_sum","reason":null}));
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
        out.push(json!({"adapter":format!("otlp:{harness}"),"interface":"otlp_http_json","certified_versions":[],"uncertified_version":"fixture_only","fields":fields}));
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
    let metrics = match endpoint {
        "/v1/logs" => false,
        "/v1/metrics" => true,
        _ => anyhow::bail!("unsupported endpoint"),
    };
    let root: Value = serde_json::from_slice(bytes).context("malformed JSON")?;
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
        let harness = ra.get("service.name").and_then(Value::as_str).unwrap_or("");
        let known = matches!(harness, "claude-code" | "gemini-cli");
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
        let attempt = attempted.filter(|id| attempts.iter().any(|a| a.id == *id));
        let binding = if attempt.is_some() {
            "exact"
        } else if attempted.is_some() {
            "unknown_attempt"
        } else {
            "unbound"
        };
        for scope in array(resource, if metrics { "scopeMetrics" } else { "scopeLogs" })? {
            for entry in array(scope, if metrics { "metrics" } else { "logRecords" })? {
                let ea = attributes(entry)?;
                // Log body may carry only a reviewed event name, never arbitrary text.
                let name = if metrics {
                    entry["name"].as_str()
                } else {
                    entry["eventName"]
                        .as_str()
                        .or_else(|| entry["body"]["stringValue"].as_str())
                        .or_else(|| ea.get("event.name").and_then(Value::as_str))
                }
                .context("missing native name")?;
                let mapping = MAPPINGS
                    .iter()
                    .find(|m| m.0 == harness && m.1 == name && metrics == name.ends_with(".usage"));
                let points = if metrics {
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
                                } else {
                                    matches!(s, "input" | "output" | "cache" | "thought" | "tool")
                                }
                            })
                    });
                    let mut payload = json!({"adapter":adapter,"attempt_id":attempt,"binding":binding,"source_trust":"collector_observed","certified":"fixture", "timeUnixNano":timestamp(&point,"timeUnixNano")?});
                    let mut allowed = BTreeMap::new();
                    if let Some((_, native, kind, fields)) = mapping {
                        payload["native_name"] = json!(native);
                        payload["kind"] = json!(kind);
                        for field in *fields {
                            if let Some(raw) = attrs.get(*field) {
                                let safe = match *field {
                                    "model" | "tool_name" | "function_name" => identifier(raw),
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
                                ensure!(safe.is_some(), "invalid mapped attribute");
                                allowed.insert(*field, safe.unwrap());
                            }
                        }
                        payload["attributes"] = json!(allowed);
                        if metrics {
                            ensure!(
                                point.get("asInt").is_some() != point.get("asDouble").is_some(),
                                "invalid metric value fields"
                            );
                            let val = if let Some(raw) = point.get("asInt") {
                                integer(raw).filter(|n| *n >= 0).map(|n| json!(n))
                            } else {
                                point
                                    .get("asDouble")
                                    .filter(|v| v.is_number())
                                    .and_then(number)
                            }
                            .context("invalid metric value")?;
                            payload["value"] = val;
                            payload["startTimeUnixNano"] = timestamp(&point, "startTimeUnixNano")?;
                            let temp = entry["sum"]["aggregationTemporality"].clone();
                            ensure!(
                                matches!(temp.as_i64(), Some(1 | 2))
                                    || matches!(
                                        temp.as_str(),
                                        Some(
                                            "AGGREGATION_TEMPORALITY_DELTA"
                                                | "AGGREGATION_TEMPORALITY_CUMULATIVE"
                                        )
                                    ),
                                "unknown counter temporality"
                            );
                            payload["aggregationTemporality"] = temp;
                            // Reviewed units only; never retain arbitrary exporter text.
                            let unit = entry["unit"].as_str().unwrap_or("");
                            ensure!(
                                matches!(unit, "" | "USD" | "{token}" | "token" | "tokens"),
                                "unsupported unit"
                            );
                            payload["unit"] = json!(unit);
                        }
                    } else {
                        payload["kind"] = json!("unmapped");
                    }
                    let allowed_keys = mapping.map_or(&[][..], |m| m.3);
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
        let identity = format!("sha256:{:x}", Sha256::digest(text.as_bytes()));
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
        if supplied.len() != expected.len() || diff != 0 {
            return Err(401);
        }
        if headers.contains_key("transfer-encoding") || headers.contains_key("content-encoding") {
            return Err(415);
        }
        if headers.get("content-type").copied() != Some("application/json") {
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
        ingest(project, first[1], &body).map_err(|e| {
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

/// Optional ticker receiver: independent bounded thread, one listener per project.
/// Configuration is scoped by slug; absent configuration performs no socket work.
pub fn start_configured(project: &Path, config: &Path) -> Result<()> {
    static RUNNING: std::sync::Mutex<
        Option<BTreeMap<std::path::PathBuf, std::thread::JoinHandle<()>>>,
    > = std::sync::Mutex::new(None);
    let path = config.join("config.toml");
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
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
    if settings
        .and_then(|s| s.get("enabled"))
        .and_then(toml::Value::as_bool)
        != Some(true)
    {
        return Ok(());
    }
    let port = settings
        .and_then(|s| s.get("port"))
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

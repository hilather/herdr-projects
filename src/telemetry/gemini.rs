//! Gemini 0.62.0 SDK file envelopes, converted to the DG4a privacy mapper.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Seek, SeekFrom},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};

/// Future launch support must set GEMINI_TELEMETRY_OUTFILE to this exact path.
const OUTFILE: &str = "gemini-telemetry.json";
const MAX_OBJECT: usize = super::otlp::MAX_BODY;

pub(super) fn capabilities() -> Value {
    let mapped = super::otlp::capabilities()
        .into_iter()
        .find(|a| a["adapter"] == "otlp:gemini-cli")
        .unwrap();
    let mut fields = mapped["fields"].as_array().unwrap().clone();
    fields.extend(super::codex::gemini::fields());
    json!({"adapter":"gemini-cli", "interface":"local_sdk_file", "certified_versions":[],
        "fixture_versions":["0.62.0"], "accepted_versions":["0.62.0"], "certification":"fixture",
        "uncertified_version":"file_format_not_certified", "fields":fields,
        "native_session":{"certified":"fixture","interface":"session_jsonl","fields":super::codex::gemini::fields(),"caveat":"message_updates_not_additive_no_cross_surface_sum"},
        "live_certification":"separate_owner_gated_step"})
}

fn nanos(value: &Value) -> Result<u64> {
    let pair = value.as_array().context("invalid SDK time")?;
    ensure!(pair.len() == 2, "invalid SDK time");
    let seconds = pair[0].as_u64().context("invalid SDK seconds")?;
    let fraction = pair[1]
        .as_u64()
        .filter(|n| *n < 1_000_000_000)
        .context("invalid SDK nanoseconds")?;
    seconds
        .checked_mul(1_000_000_000)
        .and_then(|n| n.checked_add(fraction))
        .context("SDK time overflow")
}

fn attributes(value: &Value) -> Result<Value> {
    let object = value.as_object().context("invalid SDK attributes")?;
    ensure!(object.len() <= 128, "too many SDK attributes");
    Ok(json!(
        object
            .iter()
            .map(|(key, value)| {
                let any = if value.is_string() {
                    json!({"stringValue":value})
                } else if value.is_boolean() {
                    json!({"boolValue":value})
                } else if value.is_i64() {
                    json!({"intValue":value})
                } else if value.is_number() {
                    json!({"doubleValue":value})
                } else {
                    json!({"arrayValue":{}})
                };
                json!({"key":key,"value":any})
            })
            .collect::<Vec<_>>()
    ))
}

fn resource(attempt: Option<&str>) -> Value {
    let mut attrs = vec![json!({"key":"service.name","value":{"stringValue":"gemini-cli"}})];
    if let Some(id) = attempt {
        attrs.push(json!({"key":"herdr.attempt_id","value":{"stringValue":id}}));
    }
    json!({"attributes":attrs})
}

fn attempt_at<'a>(
    attempts: &'a [super::codex::CanonicalAttempt],
    home: &str,
    at: u64,
) -> Option<&'a str> {
    let at = i64::try_from(at / 1_000_000).ok()?;
    let mut matching = attempts.iter().filter(|a| a.binds_gemini(home, at));
    let first = matching.next()?;
    matching.next().is_none().then_some(first.id.as_str())
}

/// Only SDK envelope shape changes here; all durable fields pass DG4a's mapper.
fn ingest_object(
    project: &Path,
    raw: &Value,
    home: &str,
    attempts: &[super::codex::CanonicalAttempt],
) -> Result<()> {
    if let Some(scopes) = raw["scopeMetrics"].as_array() {
        for scope in scopes {
            for metric in scope["metrics"].as_array().context("invalid SDK metrics")? {
                // Only the reviewed token counter; histograms and traces stay unavailable.
                if metric["descriptor"]["name"] != "gemini_cli.token.usage" {
                    continue;
                }
                ensure!(metric["dataPointType"] == 3, "expected SDK SUM");
                for point in metric["dataPoints"]
                    .as_array()
                    .context("invalid SDK points")?
                {
                    let at = nanos(&point["endTime"])?;
                    let start = nanos(&point["startTime"])?;
                    let value = &point["value"];
                    let mut p = json!({"attributes":attributes(&point["attributes"])?, "timeUnixNano":at.to_string(),"startTimeUnixNano":start.to_string()});
                    p[if value.is_i64() { "asInt" } else { "asDouble" }] = value.clone();
                    let envelope = json!({"resourceMetrics":[{"resource":resource(attempt_at(attempts,home,at)),"scopeMetrics":[{"metrics":[{
                        "name":"gemini_cli.token.usage", "unit":metric["descriptor"]["unit"],
                        "sum":{"aggregationTemporality": match metric["aggregationTemporality"].as_u64() { Some(0) => 1, Some(1) => 2, _ => anyhow::bail!("unknown SDK temporality") },"dataPoints":[p]}}]}]}]});
                    super::otlp::ingest(project, "/v1/metrics", &serde_json::to_vec(&envelope)?)?;
                }
            }
        }
    } else if raw["attributes"].is_object() && raw.get("hrTime").is_some() {
        let name = raw["attributes"]["event.name"].as_str().unwrap_or("");
        if !matches!(name, "gemini_cli.api_response" | "gemini_cli.tool_call") {
            return Ok(());
        }
        let at = nanos(&raw["hrTime"])?;
        let envelope = json!({"resourceLogs":[{"resource":resource(attempt_at(attempts,home,at)),"scopeLogs":[{"logRecords":[{
            "eventName":name,"timeUnixNano":at.to_string(),"attributes":attributes(&raw["attributes"])?}]}]}]});
        super::otlp::ingest(project, "/v1/logs", &serde_json::to_vec(&envelope)?)?;
    }
    Ok(())
}

pub(super) fn collect(
    project: &Path,
    budget: super::codex::Budget,
    attempts: &[super::codex::CanonicalAttempt],
) -> Result<()> {
    let homes: std::collections::BTreeSet<_> =
        attempts.iter().filter_map(|a| a.gemini_home()).collect();
    let mut remaining = budget.bytes;
    for home in homes {
        if remaining == 0 {
            break;
        }
        let root = Path::new(home);
        // Refuse relative homes and symlink components, including parent directories.
        if !root.is_absolute()
            || root
                .ancestors()
                .any(|p| !std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()))
        {
            continue;
        }
        let path = root.join(OUTFILE);
        let mut file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)
        {
            Ok(file) => file,
            Err(e)
                if matches!(e.kind(), std::io::ErrorKind::NotFound)
                    || e.raw_os_error() == Some(libc::ELOOP) =>
            {
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        let meta = file.metadata()?;
        if !meta.is_file() {
            continue;
        }
        let key = format!(
            "sha256:{:x}",
            Sha256::digest(path.as_os_str().as_encoded_bytes())
        );
        let _lock = super::maintenance::lock(project, false)?;
        let db = super::sidecar::open(project, true)?.context("sidecar unavailable")?;
        use rusqlite::OptionalExtension;
        let cursor: Option<(u64, u64, u64)> = db
            .query_row(
                "SELECT device,inode,byte_offset FROM gemini_file_cursors WHERE source_digest=?1",
                [&key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let offset = cursor
            .filter(|(dev, ino, at)| *dev == meta.dev() && *ino == meta.ino() && *at <= meta.len())
            .map_or(0, |c| c.2);
        file.seek(SeekFrom::Start(offset))?;
        let limit = remaining.min(MAX_OBJECT as u64);
        let mut bytes = Vec::new();
        file.take(limit).read_to_end(&mut bytes)?;
        remaining = remaining.saturating_sub(bytes.len() as u64);
        // SDK objects are pretty printed: consume complete JSON values and their newline,
        // never an unfinished last object. Digests make cursor-write crash replay safe.
        let mut stream = serde_json::Deserializer::from_slice(&bytes).into_iter::<Value>();
        let mut consumed = 0;
        while let Some(value) = stream.next() {
            match value {
                Ok(value) => {
                    let end = stream.byte_offset();
                    if bytes.get(end) != Some(&b'\n') {
                        break;
                    }
                    ingest_object(project, &value, home, attempts)?;
                    consumed = end + 1;
                }
                Err(error) if error.is_eof() => break,
                Err(_) => break, // fail closed; no raw error excerpts persisted
            }
        }
        if consumed > 0 {
            db.execute("INSERT INTO gemini_file_cursors(source_digest,device,inode,byte_offset) VALUES(?1,?2,?3,?4) ON CONFLICT(source_digest) DO UPDATE SET device=excluded.device,inode=excluded.inode,byte_offset=excluded.byte_offset",
                rusqlite::params![key,meta.dev(),meta.ino(),offset + consumed as u64])?;
        }
    }
    Ok(())
}

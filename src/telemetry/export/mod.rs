//! TM4.3 portable exports (`export.v1`, docs/telemetry/contracts-export.md):
//! schema-versioned JSON and CSV pages read only through the analytics query
//! service (`analytics::query::run_with`), with the metric definition,
//! cohort, as-of cutoff, cost basis, exclusions, coverage, projection
//! revision and watermarks; drill-down records paged under an authenticated,
//! scoped, expiring cursor (`cursor`); consistent §7 redaction (`redact`);
//! spreadsheet-safe CSV (`csv`); an optional external destination, disabled
//! by default (`external`). Never writes a store.
use super::analytics::{query, registry};
use anyhow::Result;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub mod csv;
pub mod cursor;
pub mod external;
pub mod redact;

pub const CONTRACT: &str = "export.v1";
pub const SCHEMA_VERSION: u32 = 1;
/// Largest page an export may render (bytes); larger answers need paging or fewer metrics.
pub const MAX_BYTES: u64 = 8 * 1024 * 1024;

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format { Json, Csv }

impl Format {
    fn as_str(self) -> &'static str { match self { Format::Json => "json", Format::Csv => "csv" } }
}

/// `telemetry <slug> export ...`
#[derive(clap::Args, Clone, Debug)]
pub struct Args {
    /// Registry metric (`M02`) or explicit definition (`M02.slice-v1`); repeatable or comma-separated.
    #[arg(long = "metric", required = true, value_delimiter = ',')]
    pub metrics: Vec<String>,
    /// `activity_window`, `terminal_cohort` or `assignment_cohort` (as `query`).
    #[arg(long)]
    pub cohort: Option<String>,
    /// Window start, inclusive, UTC Unix ms.
    #[arg(long)]
    pub from: Option<i64>,
    /// Window end, exclusive, UTC Unix ms.
    #[arg(long)]
    pub to: Option<i64>,
    /// Knowledge time (Unix ms): the analytics revision recorded at or before it.
    #[arg(long, conflicts_with = "as_of_seq")]
    pub as_of: Option<i64>,
    /// Projection sequence: the cell's latest revision at or below it.
    #[arg(long)]
    pub as_of_seq: Option<i64>,
    /// One bounded categorical dimension (`route`, `task_class`, `agent_kind`).
    #[arg(long)]
    pub by: Option<String>,
    /// Assignment-cohort horizon, ms.
    #[arg(long)]
    pub horizon_ms: Option<i64>,
    /// Export the records of one drill-down bucket of one native metric, paged.
    #[arg(long)]
    pub drill: Option<String>,
    #[arg(long, value_enum, default_value = "json")]
    pub format: Format,
    /// Records per page, 1-500.
    #[arg(long, default_value_t = query::DEFAULT_PAGE)]
    pub page_size: u32,
    /// The previous page's `next_cursor`.
    #[arg(long)]
    pub cursor: Option<String>,
    /// Write the page to this new file (never replaced); CSV adds `<FILE>.manifest.json`.
    #[arg(long, conflicts_with = "external")]
    pub out: Option<PathBuf>,
    /// Byte cap of the rendered page (default and maximum 8 MiB).
    #[arg(long)]
    pub max_bytes: Option<u64>,
    /// Send the page to the configured external destination (telemetry-export.toml; disabled by default).
    #[arg(long)]
    pub external: bool,
}

/// An export-level rejection with a structured diagnostic.
#[derive(Debug)]
pub struct Rejected(pub Value);
impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "export rejected: {}", self.0) }
}
impl std::error::Error for Rejected {}

fn reject(code: &str, detail: Value) -> anyhow::Error {
    let mut body = json!({"code": code});
    if let (Value::Object(body), Value::Object(detail)) = (&mut body, detail) { body.extend(detail); }
    Rejected(body).into()
}

fn value_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "missing",
        Value::String(s) if s.split_once('/').is_some_and(|(n, d)| !n.is_empty() && !d.is_empty()) => "ratio",
        Value::String(_) => "string",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "decimal",
        Value::Bool(_) => "boolean",
        _ => "object",
    }
}

/// Value, its type and status, and a typed token for every missing counter:
/// `unavailable:<reason>`, `empty:<reason>` or `not_applicable:<why>`, never 0.
fn typed(status: &str, reason: &Value, value: &Value, numerator: &Value, denominator: &Value) -> Value {
    let (mut status, mut reason, value) = match value {
        Value::Object(o) if o.get("status").and_then(Value::as_str) == Some("unavailable") => ("unavailable".to_owned(), o.get("reason").cloned().unwrap_or(Value::Null), Value::Null),
        v => (status.to_owned(), reason.clone(), v.clone()),
    };
    if value.is_null() && status == "available" { status = "unavailable".into(); }
    if value.is_null() && reason.is_null() { reason = json!("value_not_reported"); }
    let token = |field: &str, v: &Value| -> Option<String> {
        if !v.is_null() { return None; }
        let why = reason.as_str().unwrap_or("unknown");
        Some(match (status.as_str(), field) {
            ("unavailable", _) => format!("unavailable:{why}"),
            (_, "value") => format!("{status}:{why}"),
            (_, field) => format!("not_applicable:no_{field}"),
        })
    };
    let missing: serde_json::Map<String, Value> = [("value", &value), ("numerator", numerator), ("denominator", denominator)].into_iter()
        .filter_map(|(f, v)| token(f, v).map(|t| (f.to_owned(), json!(t)))).collect();
    let value_status = if value.is_null() || status != "available" { format!("{status}:{}", reason.as_str().unwrap_or("unknown")) } else { status.clone() };
    json!({"status": status, "reason": reason, "value": value, "value_type": value_type(&value), "value_status": value_status,
        "numerator": numerator, "denominator": denominator, "missing": missing})
}

fn merge(into: &mut Value, from: Value) {
    if let (Value::Object(into), Value::Object(from)) = (into, from) { into.extend(from); }
}

/// One query result as an export metric (summary pages only).
fn metric(r: &Value) -> Value {
    let status = r["status"].as_str().unwrap_or("unavailable");
    let mut m = json!({"metric_id": r["metric_id"], "name": r["name"], "definition": r["definition"], "registry": r["registry"], "family": r["family"],
        "proxy": r["proxy"], "unit": r["unit"], "certification": r["certification"], "cohort": r["cohort"], "time_basis": r["time_basis"], "window": r["window"],
        "horizon_ms": r["horizon_ms"], "by": r["by"], "exclusions": r["exclusions"], "coverage": r["coverage"],
        "as_of": {"requested": r.get("as_of").cloned().unwrap_or(Value::Null), "event_cutoff_unix_ms": r["event_cutoff_unix_ms"], "observation_cutoff_unix_ms": r["observation_cutoff_unix_ms"]},
        "lag_ms": r["lag_ms"], "lag_reason": r["lag_reason"], "projection": r["projection"], "source_watermarks": r["source_watermarks"]});
    merge(&mut m, typed(status, &r["reason"], &r["value"], &r["numerator"], &r["denominator"]));
    for key in ["breakdown", "censored", "provisional", "diagnostic", "detail"] {
        if let Some(v) = r.get(key).filter(|v| !v.is_null()) { m[key] = v.clone(); }
    }
    m["cost_basis"] = match &r["rate_card_revision"] {
        Value::Null => json!({"status": "not_applicable", "reason": "not_a_cost_metric"}),
        rate => {
            let unavailable = rate["status"] == "unavailable";
            json!({"status": if unavailable { "unavailable" } else { "available" }, "reason": if unavailable { rate["reason"].clone() } else { Value::Null },
                "rate_card_revision": rate, "basis": r["detail"].get("basis").cloned().unwrap_or(Value::Null),
                "currency": r["detail"].get("currency").cloned().unwrap_or(Value::Null)})
        }
    };
    m["cells"] = r["cells"].as_array().map(|cells| cells.iter().map(|c| {
        let mut cell = json!({"dimension": c["dimension"]});
        let status = if c["value"].is_null() { "empty" } else { "available" };
        merge(&mut cell, typed(status, &c["reason"], &c["value"], &c["numerator"], &c["denominator"]));
        cell
    }).collect::<Vec<_>>()).map_or(json!([]), Value::Array);
    m
}

fn sha256(bytes: &[u8]) -> String { format!("sha256:{:x}", <sha2::Sha256 as sha2::Digest>::digest(bytes)) }

/// Build one export page (JSON value) from the query service's answer.
fn document(out: &Value, args: &Args, now: i64, max_bytes: u64, external: Option<&str>) -> Value {
    let first = args.cursor.is_none();
    let results = out["results"].as_array().cloned().unwrap_or_default();
    let drill = out.get("drill").cloned().unwrap_or(Value::Null);
    let snapshot = if drill.is_null() {
        json!(results.iter().map(|r| json!({"metric_id": r["metric_id"], "definition": r["definition"], "revision": r["projection"]["revision"],
            "content_digest": r["projection"]["content_digest"]})).collect::<Vec<_>>())
    } else { json!({"drill": drill["snapshot"]}) };
    let export_id = sha256(serde_json::to_string(&json!({"request": out["request"], "snapshot": snapshot})).unwrap_or_default().as_bytes());
    let records = if drill.is_null() { Value::Null } else if drill["status"] == "unavailable" { drill.clone() } else {
        json!({"metric_id": drill["metric_id"], "definition": drill["definition"], "bucket": drill["bucket"], "buckets": drill["buckets"], "snapshot": drill["snapshot"],
            "offset": drill["offset"], "page_size": drill["page_size"], "total": drill["total"], "rows": drill["rows"]})
    };
    let rows = drill["rows"].as_array().map_or(0, Vec::len);
    let page = json!({"first": first, "last": drill["next_cursor"].is_null(), "offset": drill.get("offset").cloned().unwrap_or(json!(0)), "rows": rows,
        "page_size": if drill.is_null() { Value::Null } else { json!(args.page_size) }, "total": drill.get("total").cloned().unwrap_or(Value::Null),
        "next_cursor": drill.get("next_cursor").cloned().unwrap_or(Value::Null), "next_cursor_expires_unix_ms": drill.get("next_cursor_expires_unix_ms").cloned().unwrap_or(Value::Null)});
    let created = jiff::Timestamp::from_millisecond(now).map(|t| t.to_string()).unwrap_or_default();
    let mut manifest = json!({"contract": CONTRACT, "schema_version": SCHEMA_VERSION, "export_id": export_id, "format": args.format.as_str(),
        "created_unix_ms": now, "created_at": created, "timezone": "UTC", "complete": true,
        "query": {"contract": out["contract"], "schema_version": out["schema_version"], "registry": registry::VERSION, "request": redact::value(&out["request"]), "query_unix_ms": out["query_unix_ms"]},
        "snapshot": snapshot, "page": page,
        "bounds": {"max_records_per_page": query::MAX_PAGE, "max_bytes": max_bytes, "max_cells_per_metric": query::MAX_CELLS},
        "redaction": {"policy": "contracts.md §7", "content": "metadata and evidence references only; no prompts, tool arguments, output, secrets or home paths",
            "text": "excerpt rules 1-5 on every string except sha256 digests, page cursors and identifiers under identity keys"},
        "summary": if first { "metrics on this page" } else { "on the first page only; continuation pages carry records of the same snapshot" },
        "external": external.map(|d| json!({"destination": d}))});
    if args.format == Format::Csv {
        manifest["csv"] = json!({"columns": csv::COLUMNS.as_slice(), "dialect": "RFC 4180, CRLF, UTF-8, header row",
            "formula_guard": "a cell starting with = + - @ TAB or CR (other than a plain decimal numeral or ratio) is prefixed with a single quote; the JSON export holds the exact text",
            "missing": "typed tokens <status>:<reason> (unavailable, empty, not_applicable, none, unknown); an empty cell only where the column does not apply to the row kind"});
    }
    let metrics: Vec<Value> = if first { results.iter().map(metric).collect() } else { Vec::new() };
    json!({"manifest": manifest, "metrics": redact::value(&json!(metrics)), "records": redact::value(&records)})
}

/// `telemetry <slug> export`: what to print on stdout.
pub fn run(project: &Path, config_dir: &Path, slug: &str, args: &Args) -> Result<String> {
    let max_bytes = args.max_bytes.unwrap_or(MAX_BYTES);
    if !(1..=MAX_BYTES).contains(&max_bytes) { return Err(reject("max_bytes_out_of_range", json!({"max_bytes": max_bytes, "max": MAX_BYTES}))); }
    if args.cursor.is_some() && args.drill.is_none() { return Err(reject("cursor_needs_drill", json!({"detail": "only drill-down records are paged; drop --cursor"}))); }
    // The deployment setting is checked before anything is read.
    let destination = if args.external {
        match external::load(config_dir)? {
            Ok(d) => Some(d),
            Err((code, detail)) => return Err(reject(code, json!({"config": external::CONFIG_FILE, "detail": detail}))),
        }
    } else { None };
    let qargs = query::Args { metrics: args.metrics.clone(), cohort: args.cohort.clone(), from: args.from, to: args.to, as_of: args.as_of, as_of_seq: args.as_of_seq,
        by: args.by.clone(), horizon_ms: args.horizon_ms, drill: args.drill.clone(), page_size: args.page_size, cursor: args.cursor.clone(), json: true };
    let request = query::request(&qargs)?;
    let now = jiff::Timestamp::now().as_millisecond();
    let out = query::run_with(project, &request, &cursor::Keyring::new(config_dir))?;
    let doc = document(&out, args, now, max_bytes, destination.as_ref().map(external::Destination::kind));
    let data = match args.format { Format::Json => serde_json::to_string_pretty(&doc)? + "\n", Format::Csv => csv::render(&doc) };
    if data.len() as u64 > max_bytes {
        return Err(reject("export_too_large", json!({"bytes": data.len(), "max_bytes": max_bytes, "detail": "page with a smaller --page-size or export fewer metrics"})));
    }
    let manifest = |data: &str| -> Result<String> {
        let mut m = doc["manifest"].clone();
        m["data"] = json!({"format": args.format.as_str(), "bytes": data.len(), "digest": sha256(data.as_bytes())});
        Ok(serde_json::to_string_pretty(&m)? + "\n")
    };
    let write = |path: &Path| -> Result<Value> {
        external::write_new(path, data.as_bytes())?;
        let mut files = vec![json!({"file": path.file_name().map(|n| n.to_string_lossy().into_owned()), "bytes": data.len(), "digest": sha256(data.as_bytes())})];
        if args.format == Format::Csv {
            let companion = PathBuf::from(format!("{}.manifest.json", path.display()));
            let text = manifest(&data)?;
            external::write_new(&companion, text.as_bytes())?;
            files.push(json!({"file": companion.file_name().map(|n| n.to_string_lossy().into_owned()), "bytes": text.len(), "digest": sha256(text.as_bytes())}));
        }
        Ok(json!({"contract": CONTRACT, "export_id": doc["manifest"]["export_id"], "written": files, "next_cursor": doc["manifest"]["page"]["next_cursor"]}))
    };
    match (&args.out, destination) {
        (Some(path), _) => Ok(serde_json::to_string_pretty(&write(path)?)? + "\n"),
        (None, Some(external::Destination::Directory(dir))) => {
            let id = doc["manifest"]["export_id"].as_str().unwrap_or_default().trim_start_matches("sha256:").get(..16).unwrap_or_default().to_owned();
            let name = format!("{slug}-{id}-{}.{}", doc["manifest"]["page"]["offset"], args.format.as_str());
            Ok(serde_json::to_string_pretty(&write(&dir.join(name))?)? + "\n")
        }
        (None, _) => Ok(data),
    }
}

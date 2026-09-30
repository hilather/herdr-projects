//! The query service (`analytics-query.v1`, docs/telemetry/contracts-analytics.md §2):
//! the one read path behind `telemetry query`, `telemetry report`, the fleet
//! pane and (TM4.2/TM4.3) views and exports. Read-only: it opens `state.db`
//! and the sidecar strictly read-only and never writes a revision.
use super::lifecycle::{self, Lineage, Task};
use super::registry::{self, Cohort, Metric, Provider, Version, Window};
use crate::telemetry::export::cursor::Keyring;
use anyhow::Result;
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

pub const CONTRACT: &str = "analytics-query.v1";
pub const SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_PAGE: u32 = 100;
pub const MAX_PAGE: u32 = 500;
/// Most dimension cells one result may carry (doc 08 §4 bounded complexity).
pub const MAX_CELLS: usize = 64;
/// Priced metrics: they carry the valuation (rate card) revision they read.
const PRICED: [&str; 6] = ["M04", "M12", "M14", "M24", "M34", "M37"];

/// `herdr-projects telemetry <slug> report`: central slice metrics, then every
/// lane's (`super::super::LANES`, a lane key replacing a central one).
pub fn report(project: &Path, since: Option<i64>) -> Result<Value> {
    let (mut metrics, tasks) = crate::telemetry::metrics::central(project, since)?;
    for lane in &crate::telemetry::LANES { metrics.extend((lane.metrics)(project, since)?); }
    Ok(json!({"metrics": metrics, "since_unix_ms": since, "tasks": tasks}))
}

/// `telemetry <slug> query ...`
#[derive(clap::Args, Clone, Debug)]
pub struct Args {
    /// Registry metric (`M02`, current definition) or explicit definition (`M02.slice-v1`); repeatable or comma-separated.
    #[arg(long = "metric", required = true, value_delimiter = ',')]
    pub metrics: Vec<String>,
    /// `activity_window`, `terminal_cohort` or `assignment_cohort`; default: each metric's registry cohort. `completed_task` is rejected.
    #[arg(long)]
    pub cohort: Option<String>,
    /// Window start, inclusive, UTC Unix ms.
    #[arg(long)]
    pub from: Option<i64>,
    /// Window end, exclusive, UTC Unix ms.
    #[arg(long)]
    pub to: Option<i64>,
    /// Knowledge time (Unix ms): answer from the analytics revision recorded at or before it.
    #[arg(long, conflicts_with = "as_of_seq")]
    pub as_of: Option<i64>,
    /// Projection sequence: answer from the cell's latest revision at or below it.
    #[arg(long)]
    pub as_of_seq: Option<i64>,
    /// One bounded categorical dimension (`route`, `task_class`, `agent_kind`); identities are drill-downs, never labels.
    #[arg(long)]
    pub by: Option<String>,
    /// Assignment-cohort horizon after first assignment, ms; later outcomes are `unfinished`.
    #[arg(long)]
    pub horizon_ms: Option<i64>,
    /// Drill-down bucket (`numerator`, `denominator`, `outcome.<o>`, `excluded.<reason>`) of one native metric.
    #[arg(long)]
    pub drill: Option<String>,
    /// Drill-down page size, 1-500.
    #[arg(long, default_value_t = DEFAULT_PAGE)]
    pub page_size: u32,
    /// Opaque cursor from the previous page's `next_cursor`.
    #[arg(long)]
    pub cursor: Option<String>,
    /// Print JSON (the read contract) instead of text.
    #[arg(long)]
    pub json: bool,
}

/// A request-level rejection with a structured diagnostic (doc 08 §4).
#[derive(Debug)]
pub struct Rejected(pub Value);
impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "query rejected: {}", self.0) }
}
impl std::error::Error for Rejected {}

fn reject(code: &str, detail: Value) -> anyhow::Error {
    let mut body = json!({"code": code});
    if let (Value::Object(body), Value::Object(detail)) = (&mut body, detail) { body.extend(detail); }
    Rejected(body).into()
}

/// One aggregate cell: what a revision is keyed by.
#[derive(Clone)]
pub struct Cell {
    pub metric: &'static Metric,
    pub version: &'static Version,
    pub cohort: Cohort,
    pub from: Option<i64>,
    pub to: Option<i64>,
    pub horizon: Option<i64>,
    pub by: Option<String>,
}

impl Cell {
    pub fn default_for(metric: &'static Metric) -> Self {
        let version = &metric.versions[0];
        Cell { metric, version, cohort: version.cohorts[0], from: None, to: None, horizon: None, by: None }
    }
    /// Canonical JSON key (sorted keys, compact).
    pub fn key(&self) -> String {
        serde_json::to_string(&json!({"metric": self.metric.id, "definition": self.version.definition, "cohort": self.cohort.as_str(),
            "from": self.from, "to": self.to, "horizon_ms": self.horizon, "by": self.by})).unwrap_or_default()
    }
    pub fn parse(key: &str) -> Option<Self> {
        let v: Value = serde_json::from_str(key).ok()?;
        let (metric, version) = registry::resolve(v["definition"].as_str()?).ok()?;
        Some(Cell { metric, version, cohort: Cohort::parse(v["cohort"].as_str()?).ok()?, from: v["from"].as_i64(), to: v["to"].as_i64(),
            horizon: v["horizon_ms"].as_i64(), by: v["by"].as_str().map(str::to_owned) })
    }
    /// Why this cell cannot be evaluated, as `(reason, diagnostic)`.
    fn unsupported(&self) -> Option<(&'static str, Value)> {
        if let Err(reason) = self.metric.family.activation() { return Some((reason, json!({"family": self.metric.family.as_str()}))); }
        if let Provider::Absent(reason) = self.version.provider { return Some((reason, json!({"producer": null}))); }
        if !self.version.cohorts.contains(&self.cohort) {
            return Some(("cohort_unsupported", json!({"supported": self.version.cohorts.iter().map(|c| c.as_str()).collect::<Vec<_>>()})));
        }
        if self.version.window == Window::SinceOnly && self.to.is_some() {
            return Some(("window_end_unsupported", json!({"window": "since_only", "detail": "this definition windows from `from` only; drop `to`"})));
        }
        if self.horizon.is_some() && !(self.cohort == Cohort::Assignment && self.version.provider == Provider::Native) {
            return Some(("horizon_unsupported", json!({"detail": "a horizon applies to native assignment cohorts"})));
        }
        if let Some(by) = &self.by {
            if registry::HIGH_CARDINALITY.contains(&by.as_str()) {
                return Some(("high_cardinality_dimension", json!({"dimension": by, "detail": "identities are drill-down rows (--drill), never metric labels"})));
            }
            if !self.version.dimensions.contains(&by.as_str()) {
                return Some(("dimension_unsupported", json!({"dimension": by, "supported": self.version.dimensions})));
            }
        }
        None
    }
}

/// The normalized request: parsed, validated, echoed and digested.
pub struct Request {
    pub cells: Vec<Cell>,
    pub cohort: Option<Cohort>,
    pub as_of: Option<i64>,
    pub as_of_seq: Option<i64>,
    pub drill: Option<String>,
    pub page_size: u32,
    pub cursor: Option<String>,
    pub normalized: Value,
}

pub fn request(args: &Args) -> Result<Request> {
    let accepted = json!({"accepted": ["activity_window", "terminal_cohort", "assignment_cohort"]});
    let cohort = match args.cohort.as_deref().map(Cohort::parse) {
        None => None,
        Some(Ok(cohort)) => Some(cohort),
        Some(Err("ambiguous_cohort")) => return Err(reject("ambiguous_cohort", json!({"cohort": args.cohort, "accepted": accepted["accepted"],
            "detail": "completed_task could select successful tasks only; use terminal_cohort (succeeded, failed and cancelled tasks) or assignment_cohort"}))),
        Some(Err(code)) => return Err(reject(code, json!({"cohort": args.cohort, "accepted": accepted["accepted"]}))),
    };
    if let (Some(from), Some(to)) = (args.from, args.to) && from >= to {
        return Err(reject("empty_window", json!({"from": from, "to": to, "detail": "the window is half-open [from, to)"})));
    }
    if !(1..=MAX_PAGE).contains(&args.page_size) { return Err(reject("page_size_out_of_range", json!({"page_size": args.page_size, "max": MAX_PAGE}))); }
    if args.horizon_ms.is_some_and(|h| h <= 0) { return Err(reject("horizon_out_of_range", json!({"horizon_ms": args.horizon_ms}))); }
    let mut cells = Vec::new();
    for name in &args.metrics {
        let (metric, version) = registry::resolve(name.trim()).map_err(Rejected)?;
        if cells.iter().any(|c: &Cell| c.version.definition == version.definition) { continue; }
        cells.push(Cell { metric, version, cohort: cohort.unwrap_or(version.cohorts[0]), from: args.from, to: args.to, horizon: args.horizon_ms, by: args.by.clone() });
    }
    if args.drill.is_some() && cells.len() != 1 { return Err(reject("drill_needs_one_metric", json!({"metrics": cells.len()}))); }
    let normalized = json!({"schema_version": SCHEMA_VERSION, "contract": CONTRACT, "registry": registry::VERSION,
        "metrics": cells.iter().map(|c| c.version.definition).collect::<Vec<_>>(), "cohort": cohort.map(Cohort::as_str),
        "window": {"from_unix_ms": args.from, "to_unix_ms": args.to}, "horizon_ms": args.horizon_ms, "by": args.by,
        "as_of": {"unix_ms": args.as_of, "seq": args.as_of_seq}, "drill": args.drill, "page_size": args.page_size});
    Ok(Request { cells, cohort, as_of: args.as_of, as_of_seq: args.as_of_seq, drill: args.drill.clone(), page_size: args.page_size, cursor: args.cursor.clone(), normalized })
}

/// Read-only sources for one evaluation pass: canonical lifecycle rows, the
/// source watermarks and the report bodies per provider and `since`.
pub struct Sources<'a> {
    pub project: &'a Path,
    pub tasks: Vec<Task>,
    watermarks: Option<Value>,
    bodies: BTreeMap<(Option<i64>, String), BTreeMap<String, Value>>,
}

impl<'a> Sources<'a> {
    pub fn new(project: &'a Path) -> Result<Self> {
        Ok(Sources { project, tasks: lifecycle::load(project)?, watermarks: None, bodies: BTreeMap::new() })
    }

    /// Where each source stood when read: canonical head and lifecycle input
    /// digest; per sidecar stream versions, last collect and valuation.
    pub fn watermarks(&mut self) -> Result<Value> {
        if let Some(w) = &self.watermarks { return Ok(w.clone()); }
        let state = crate::telemetry::read_only(&self.project.join(".state/state.db"))?;
        let head: i64 = state.query_row("SELECT coalesce(max(sequence),0) FROM events", [], |r| r.get(0))?;
        let canonical = json!({"events_head": head, "lifecycle_digest": lifecycle::digest(&self.tasks), "last_event_unix_ms": lifecycle::last_event(&self.tasks)});
        let sidecar = match crate::telemetry::sidecar::read(self.project)? {
            None => Value::Null,
            Some(db) => {
                let table = |name: &str| db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)", [name], |r| r.get::<_, bool>(0));
                let streams: BTreeMap<String, i64> = if table("telemetry_streams")? {
                    db.prepare("SELECT stream,version FROM telemetry_streams")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
                } else { BTreeMap::from([("codex".to_owned(), db.query_row("PRAGMA user_version", [], |r| r.get(0))?)]) };
                let last_collect: Option<i64> = db.query_row("SELECT max(updated_unix_ms) FROM collect_offsets", [], |r| r.get(0))?;
                let usage_rowid: Option<i64> = db.query_row("SELECT max(rowid) FROM codex_usage", [], |r| r.get(0))?;
                let valuation = if table("valuation_revisions")? {
                    db.query_row("SELECT revision,digest,computed_unix_ms FROM valuation_revisions ORDER BY revision DESC LIMIT 1", [],
                        |r| Ok(json!({"revision": r.get::<_, i64>(0)?, "digest": r.get::<_, String>(1)?, "computed_unix_ms": r.get::<_, i64>(2)?}))).optional()?
                } else { None };
                let cards: Vec<(String, i64, String)> = if table("rate_cards")? {
                    db.prepare("SELECT card_id,version,digest FROM rate_cards ORDER BY card_id,version")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?
                } else { Vec::new() };
                json!({"streams": streams, "last_collect_unix_ms": last_collect, "codex_usage_rowid": usage_rowid, "valuation": valuation,
                    "rate_cards": {"count": cards.len(), "digest": super::sha256(serde_json::to_string(&cards)?.as_bytes())}})
            }
        };
        let w = json!({"canonical": canonical, "sidecar": sidecar});
        self.watermarks = Some(w.clone());
        Ok(w)
    }

    /// The body `telemetry report --since <since>` prints for `id` from `provider`.
    fn body(&mut self, provider: Provider, id: &str, since: Option<i64>) -> Result<Option<Value>> {
        let key = match provider { Provider::Central => "central".to_owned(), Provider::Lane(stream) => stream.to_owned(), _ => return Ok(None) };
        if !self.bodies.contains_key(&(since, key.clone())) {
            let map = match provider {
                Provider::Central => crate::telemetry::metrics::central(self.project, since)?.0,
                Provider::Lane(stream) => match crate::telemetry::LANES.iter().find(|l| l.stream == stream) {
                    Some(lane) => (lane.metrics)(self.project, since)?,
                    None => BTreeMap::new(),
                },
                _ => BTreeMap::new(),
            };
            self.bodies.insert((since, key.clone()), map);
        }
        Ok(self.bodies[&(since, key)].get(id).cloned())
    }
}

fn unavailable_core(reason: &str, diagnostic: Value) -> Value {
    json!({"status": "unavailable", "reason": reason, "diagnostic": diagnostic, "value": {"status": "unavailable", "reason": reason},
        "numerator": null, "denominator": null, "exclusions": {}, "coverage": {"state": "unavailable", "reasons": {reason: null}}, "event_cutoff_unix_ms": null})
}

/// Map a lane or central body onto the contract: value, numerator,
/// denominator, exclusions and coverage are lifted; the whole body stays in
/// `detail` (its keys are lane-native fields, never labels).
fn lane_core(body: Value) -> Value {
    let status = match body["value"].get("status").and_then(Value::as_str) { Some("unavailable") => "unavailable", Some("partial") => "partial", _ => "available" };
    let reason = body["value"].get("reason").cloned().or_else(|| body.get("reason").cloned()).unwrap_or(Value::Null);
    let exclusions = match &body["excluded"] { Value::Object(o) => Value::Object(o.clone()), _ => json!({}) };
    let coverage = match status {
        "unavailable" => json!({"state": "unavailable", "reasons": {reason.as_str().unwrap_or("unknown"): null}}),
        // The lane does not state its expected population: coverage unknown, never 100 %.
        _ => json!({"state": if status == "partial" { "partial" } else { "unknown" }, "lane": body.get("coverage").cloned().unwrap_or(Value::Null)}),
    };
    json!({"status": status, "reason": reason, "value": body.get("value").cloned().unwrap_or(Value::Null), "numerator": body.get("numerator").cloned().unwrap_or(Value::Null),
        "denominator": body.get("denominator").cloned().unwrap_or(Value::Null), "exclusions": exclusions, "coverage": coverage, "event_cutoff_unix_ms": null, "detail": body})
}

/// Evaluate one cell live: `(core, lineage)`. Deterministic in the sources.
pub fn evaluate(sources: &mut Sources, cell: &Cell) -> Result<(Value, Lineage)> {
    if let Some((reason, diagnostic)) = cell.unsupported() { return Ok((unavailable_core(reason, diagnostic), Lineage::new())); }
    match cell.version.provider {
        Provider::Native => {
            let request = lifecycle::Request { metric: cell.metric.id, cohort: cell.cohort, from: cell.from, to: cell.to, horizon: cell.horizon, by: cell.by.as_deref() };
            let (mut core, lineage) = lifecycle::evaluate(&sources.tasks, &request);
            if core["cells"].as_array().is_some_and(|cells| cells.len() > MAX_CELLS) {
                return Ok((unavailable_core("too_many_cells", json!({"max": MAX_CELLS})), Lineage::new()));
            }
            core["status"] = json!(if core["value"].is_null() { "empty" } else { "available" });
            Ok((core, lineage))
        }
        provider => match sources.body(provider, cell.metric.id, cell.from)? {
            Some(body) => Ok((lane_core(body), Lineage::new())),
            None => Ok((unavailable_core("provider_missing_metric", json!({"provider": provider.as_json()})), Lineage::new())),
        },
    }
}

/// sha256 of the canonical content: the core body and every lineage row.
pub fn content_digest(core: &Value, lineage: &Lineage) -> String {
    let rows: BTreeMap<&String, Vec<Value>> = lineage.iter().filter(|(_, rows)| !rows.is_empty()).map(|(bucket, rows)| (bucket, rows.iter().map(|(kind, id, attrs)| json!([kind, id, attrs])).collect())).collect();
    super::sha256(serde_json::to_string(&json!({"body": core, "lineage": rows})).unwrap_or_default().as_bytes())
}

/// A stored revision of a cell.
pub struct Stored { pub revision: i64, pub kind: String, pub supersedes: Option<i64>, pub body: Value, pub digest: String, pub watermarks: Value, pub recorded: i64 }

fn stored_row(r: &rusqlite::Row) -> rusqlite::Result<Stored> {
    let body: String = r.get(3)?;
    let watermarks: String = r.get(5)?;
    Ok(Stored { revision: r.get(0)?, kind: r.get(1)?, supersedes: r.get(2)?, body: serde_json::from_str(&body).unwrap_or(Value::Null), digest: r.get(4)?,
        watermarks: serde_json::from_str(&watermarks).unwrap_or(Value::Null), recorded: r.get(6)? })
}

/// Hot analytics reads, also checked by `analytics plans`.
pub const AS_OF_SEQ: &str = "SELECT revision,kind,supersedes,body,content_digest,watermarks,recorded_unix_ms FROM analytics_revisions WHERE cell=?1 AND revision<=?2 ORDER BY revision DESC LIMIT 1";
pub const AS_OF_TIME: &str = "SELECT revision,kind,supersedes,body,content_digest,watermarks,recorded_unix_ms FROM analytics_revisions WHERE cell=?1 AND recorded_unix_ms<=?2 ORDER BY recorded_unix_ms DESC,revision DESC LIMIT 1";
pub const LATEST: &str = "SELECT revision,kind,supersedes,body,content_digest,watermarks,recorded_unix_ms FROM analytics_revisions WHERE cell=?1 ORDER BY revision DESC LIMIT 1";
pub const NEXT: &str = "SELECT min(revision) FROM analytics_revisions WHERE cell=?1 AND revision>?2";
pub const PAGE: &str = "SELECT ordinal,entity_kind,entity_id,attrs FROM analytics_lineage WHERE revision=?1 AND bucket=?2 AND ordinal>=?3 ORDER BY ordinal LIMIT ?4";
pub const BUCKETS: &str = "SELECT bucket,count(*) FROM analytics_lineage WHERE revision=?1 GROUP BY bucket ORDER BY bucket";

fn analytics_tables(db: &rusqlite::Connection) -> rusqlite::Result<bool> {
    db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='analytics_revisions')", [], |r| r.get(0))
}

pub fn latest(db: &rusqlite::Connection, key: &str) -> Result<Option<Stored>> {
    if !analytics_tables(db)? { return Ok(None); }
    Ok(db.query_row(LATEST, [key], stored_row).optional()?)
}

fn as_of(db: &rusqlite::Connection, key: &str, at: Option<i64>, seq: Option<i64>) -> Result<Option<Stored>> {
    if !analytics_tables(db)? { return Ok(None); }
    Ok(match (at, seq) {
        (_, Some(seq)) => db.query_row(AS_OF_SEQ, rusqlite::params![key, seq], stored_row).optional()?,
        (Some(at), None) => db.query_row(AS_OF_TIME, rusqlite::params![key, at], stored_row).optional()?,
        (None, None) => None,
    })
}

/// One stored lineage page: `(rows, total)`.
fn stored_page(db: &rusqlite::Connection, revision: i64, bucket: &str, offset: i64, size: u32) -> Result<(Vec<Value>, i64, BTreeMap<String, i64>)> {
    let buckets: BTreeMap<String, i64> = db.prepare(BUCKETS)?.query_map([revision], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    let rows = db.prepare(PAGE)?.query_map(rusqlite::params![revision, bucket, offset, size], |r| {
        let attrs: String = r.get(3)?;
        Ok(row_json(r.get::<_, String>(1)?.as_str(), r.get(2)?, serde_json::from_str(&attrs).unwrap_or(Value::Null)))
    })?.collect::<rusqlite::Result<_>>()?;
    Ok((rows, buckets.get(bucket).copied().unwrap_or(0), buckets))
}

fn row_json(kind: &str, id: String, attrs: Value) -> Value {
    let mut row = json!({"entity": kind, "id": id});
    if let (Value::Object(row), Value::Object(attrs)) = (&mut row, attrs) { row.extend(attrs); }
    row
}

/// The contract envelope around a core body.
fn envelope(cell: &Cell, core: Value, projection: Value, watermarks: &Value, observation_cutoff: i64) -> Value {
    let native = cell.version.provider == Provider::Native;
    let mut out = json!({"metric_id": cell.metric.id, "name": cell.metric.name, "definition": cell.version.definition, "family": cell.metric.family.as_str(),
        "proxy": cell.metric.family.proxy(), "unit": cell.metric.unit, "cohort": cell.cohort.as_str(), "time_basis": cell.version.time_basis,
        "window": {"from_unix_ms": cell.from, "to_unix_ms": cell.to, "semantics": match cell.version.window { Window::HalfOpen => "half_open", Window::SinceOnly => "since_only" }},
        "horizon_ms": cell.horizon, "by": cell.by, "registry": registry::VERSION,
        "certification": {"status": cell.metric.certification, "evidence": cell.metric.evidence, "restriction": cell.metric.restriction},
        "activation": cell.metric.family.activation().unwrap_or_else(|reason| json!({"status": "unavailable", "reason": reason})),
        "projection": projection, "observation_cutoff_unix_ms": observation_cutoff});
    if let (Value::Object(out), Value::Object(core)) = (&mut out, core) { for (k, v) in core { out.entry(k).or_insert(v); } }
    let sidecar = &watermarks["sidecar"];
    out["source_watermarks"] = if native { json!({"canonical": watermarks["canonical"]}) } else { watermarks.clone() };
    let (lag, lag_reason) = if native { (json!(0), Value::Null) } else {
        match sidecar["last_collect_unix_ms"].as_i64() {
            Some(at) => (json!(observation_cutoff - at), Value::Null),
            None => (Value::Null, json!(if sidecar.is_null() { "collection_not_run" } else { "no_collect_recorded" })),
        }
    };
    out["lag_ms"] = lag;
    out["lag_reason"] = lag_reason;
    out["rate_card_revision"] = if PRICED.contains(&cell.metric.id) {
        match &sidecar["valuation"] {
            Value::Object(v) => json!({"valuation_revision": v["revision"], "valuation_digest": v["digest"], "rate_cards": sidecar["rate_cards"]}),
            _ => json!({"status": "unavailable", "reason": if sidecar.is_null() { "collection_not_run" } else { "not_priced" }}),
        }
    } else { Value::Null };
    out
}

/// Cursor kind of drill-down pages (`export::cursor`): keyed MAC, project
/// scope and expiry; the position binds `{request, snapshot, revision, bucket, next}`.
const CURSOR_KIND: &str = "analytics-drill";

/// `telemetry <slug> query`: the read contract (JSON). Drill-down cursors use
/// the key under `$HOME/.config/herdr-projects` (`run_with` names it).
pub fn run(project: &Path, request: &Request) -> Result<Value> { run_with(project, request, &Keyring::from_home()) }

/// As `run`, with drill-down cursors sealed and opened by `keys`.
pub fn run_with(project: &Path, request: &Request, keys: &Keyring) -> Result<Value> {
    let now = jiff::Timestamp::now().as_millisecond();
    let mut sources = Sources::new(project)?;
    let watermarks = sources.watermarks()?;
    let sidecar = crate::telemetry::sidecar::read(project)?;
    let request_digest = super::sha256(serde_json::to_string(&request.normalized)?.as_bytes());
    let mut results = Vec::new();
    let mut drill = Value::Null;
    for cell in &request.cells {
        let key = cell.key();
        let historical = request.as_of.is_some() || request.as_of_seq.is_some();
        let (result, lineage_source) = if historical {
            match sidecar.as_deref().map(|db| as_of(db, &key, request.as_of, request.as_of_seq)).transpose()?.flatten() {
                None => {
                    let core = unavailable_core("no_revision_as_of", json!({"cell": serde_json::from_str::<Value>(&key)?, "detail": "no analytics revision of this cell was recorded by then; `analytics refresh` records one"}));
                    (envelope(cell, core, json!({"mode": "revision", "revision": null}), &json!({"canonical": null, "sidecar": null}), request.as_of.unwrap_or(now)), None)
                }
                Some(stored) => {
                    let db = sidecar.as_deref().expect("a revision was read from it");
                    let next: Option<i64> = db.query_row(NEXT, rusqlite::params![key, stored.revision], |r| r.get(0))?;
                    let current = latest(db, &key)?.map(|s| s.revision);
                    let projection = json!({"mode": "revision", "revision": stored.revision, "kind": stored.kind, "supersedes": stored.supersedes,
                        "superseded_by": next, "current_revision": current, "restated": next.is_some(), "recorded_unix_ms": stored.recorded, "content_digest": stored.digest});
                    let mut out = envelope(cell, stored.body.clone(), projection, &stored.watermarks, stored.recorded);
                    out["as_of"] = json!({"unix_ms": request.as_of, "seq": request.as_of_seq});
                    (out, Some(Err(stored.revision)))
                }
            }
        } else {
            let (core, lineage) = evaluate(&mut sources, cell)?;
            let digest = content_digest(&core, &lineage);
            let matched = match sidecar.as_deref() { Some(db) => latest(db, &key)?.filter(|s| s.digest == digest).map(|s| s.revision), None => None };
            let projection = json!({"mode": "live", "revision": null, "matches_revision": matched, "content_digest": digest});
            (envelope(cell, core, projection, &watermarks, now), Some(Ok((lineage, digest, matched))))
        };
        if let (Some(bucket), Some(source)) = (&request.drill, lineage_source) {
            let auth = Auth { keys, project: crate::telemetry::export::cursor::project_scope(project)?, now };
            drill = drill_page(sidecar.as_deref(), cell, &result, source, bucket, request, &request_digest, &auth)?;
        } else if request.drill.is_some() {
            drill = json!({"status": "unavailable", "reason": "no_revision_as_of"});
        }
        results.push(result);
    }
    let mut out = json!({"schema_version": SCHEMA_VERSION, "contract": CONTRACT, "request": request.normalized, "query_unix_ms": now, "results": results});
    if request.drill.is_some() { out["drill"] = drill; }
    Ok(out)
}

/// One bounded drill-down page. A live snapshot that matches a stored
/// revision is pinned to it, so later pages survive new arrivals; an
/// unpinned live snapshot that changed is `restart_required`, never mixed.
/// Who may continue a drill-down: the cursor key, the project scope and the time.
struct Auth<'a> { keys: &'a Keyring, project: String, now: i64 }

#[allow(clippy::too_many_arguments)]
fn drill_page(db: Option<&rusqlite::Connection>, cell: &Cell, result: &Value, source: std::result::Result<(Lineage, String, Option<i64>), i64>, bucket: &str,
    request: &Request, request_digest: &str, auth: &Auth) -> Result<Value> {
    if cell.version.provider != Provider::Native || result["status"] == "unavailable" {
        return Ok(json!({"status": "unavailable", "reason": "drill_unsupported", "detail": "lane metrics drill down through their lane's ledger commands"}));
    }
    let cursor = match request.cursor.as_deref() {
        None => None,
        Some(token) => Some(auth.keys.open(CURSOR_KIND, &auth.project, auth.now, token).map_err(|(code, detail)| reject(code, detail))?),
    };
    if let Some(c) = &cursor && (c["request"] != request_digest || c["bucket"] != bucket) {
        return Err(reject("cursor_mismatch", json!({"detail": "a cursor continues only the request that issued it"})));
    }
    let offset = cursor.as_ref().and_then(|c| c["next"].as_i64()).unwrap_or(0);
    let pinned = cursor.as_ref().and_then(|c| c["revision"].as_i64());
    let (rows, total, buckets, snapshot, revision) = match (pinned, source) {
        (Some(revision), _) | (None, Err(revision)) => {
            let db = db.ok_or_else(|| reject("restart_required", json!({"detail": "the pinned snapshot is gone"})))?;
            let digest: Option<String> = db.query_row("SELECT content_digest FROM analytics_revisions WHERE revision=?1", [revision], |r| r.get(0)).optional()?;
            let digest = digest.ok_or_else(|| reject("restart_required", json!({"detail": "the pinned snapshot is gone"})))?;
            if cursor.as_ref().is_some_and(|c| c["snapshot"] != digest.as_str()) { return Err(reject("restart_required", json!({}))); }
            let (rows, total, buckets) = stored_page(db, revision, bucket, offset, request.page_size)?;
            (rows, total, buckets, digest, Some(revision))
        }
        (None, Ok((lineage, digest, matched))) => {
            if cursor.as_ref().is_some_and(|c| c["snapshot"] != digest.as_str()) {
                return Err(reject("restart_required", json!({"detail": "the live snapshot changed since the first page; start again without --cursor"})));
            }
            if let (Some(revision), Some(db)) = (matched, db) {
                let (rows, total, buckets) = stored_page(db, revision, bucket, offset, request.page_size)?;
                (rows, total, buckets, digest, Some(revision))
            } else {
                let list = lineage.get(bucket).map(Vec::as_slice).unwrap_or_default();
                let rows = list.iter().skip(offset as usize).take(request.page_size as usize).map(|(kind, id, attrs)| row_json(kind, id.clone(), attrs.clone())).collect();
                (rows, list.len() as i64, lineage.iter().map(|(b, r)| (b.clone(), r.len() as i64)).collect(), digest, None)
            }
        }
    };
    let next = offset + rows.len() as i64;
    let (next_cursor, expires) = if next < total {
        let (token, exp) = auth.keys.seal(CURSOR_KIND, &auth.project, auth.now, json!({"request": request_digest, "snapshot": snapshot, "revision": revision, "bucket": bucket, "next": next}))?;
        (Some(token), Some(exp))
    } else { (None, None) };
    Ok(json!({"metric_id": cell.metric.id, "definition": cell.version.definition, "bucket": bucket, "buckets": buckets, "rows": rows, "offset": offset,
        "page_size": request.page_size, "total": total, "next_cursor": next_cursor, "next_cursor_expires_unix_ms": expires, "snapshot": {"content_digest": snapshot, "revision": revision}}))
}

/// Text form: one line per metric, then drill rows.
pub fn text(out: &Value) -> String {
    let mut text = String::new();
    for r in out["results"].as_array().into_iter().flatten() {
        let value = match &r["value"] {
            Value::Null => format!("n/a ({})", r["reason"].as_str().unwrap_or("unknown")),
            Value::Object(o) => format!("n/a ({})", o.get("reason").and_then(Value::as_str).unwrap_or("unknown")),
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let projection = match r["projection"]["revision"].as_i64() { Some(n) => format!("revision {n}"), None => r["projection"]["mode"].as_str().unwrap_or("").to_owned() };
        text += &format!("{} {} {} {} {value} numerator={} denominator={} coverage={} {projection}\n", r["metric_id"].as_str().unwrap_or(""), r["name"].as_str().unwrap_or(""),
            r["definition"].as_str().unwrap_or(""), r["cohort"].as_str().unwrap_or(""), r["numerator"], r["denominator"], r["coverage"]["state"].as_str().unwrap_or("unknown"));
        for c in r["cells"].as_array().into_iter().flatten() {
            let label = c["dimension"].as_object().and_then(|o| o.iter().next()).map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or(""))).unwrap_or_default();
            text += &format!("  {label} {}\n", c["value"].as_str().map_or_else(|| format!("n/a ({})", c["reason"].as_str().unwrap_or("unknown")), str::to_owned));
        }
    }
    if let Some(rows) = out["drill"]["rows"].as_array() {
        for row in rows { text += &format!("{} {} {}\n", row["entity"].as_str().unwrap_or(""), row["id"].as_str().unwrap_or(""), row["disposition"].as_str().or(row["state"].as_str()).unwrap_or("")); }
        if let Some(cursor) = out["drill"]["next_cursor"].as_str() { text += &format!("next_cursor {cursor}\n"); }
    }
    text
}

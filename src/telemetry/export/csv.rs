//! CSV rendering of an `export.v1` page (contracts-export.md §3): RFC 4180
//! (CRLF records, fields quoted when they hold `,` `"` CR LF or edge
//! whitespace), one header row, one row per metric, dimension cell, record
//! and a closing `page` row. Spreadsheet safety (OWASP CSV injection): a
//! cell that starts with `=`, `+`, `-`, `@`, TAB or CR is prefixed with `'`,
//! except a plain decimal numeral (`-12`, `3.5`) or a ratio (`2/3`). The
//! canonical JSON export is the exact-text source.
use serde_json::Value;

pub const COLUMNS: [&str; 33] = ["row_kind", "metric_id", "definition", "cohort", "window_from", "window_to", "dimension", "dimension_value",
    "record_bucket", "record_entity", "record_id", "value_status", "value", "value_type", "unit", "numerator", "denominator", "exclusions",
    "coverage", "coverage_known", "coverage_expected", "cost_basis", "projection_mode", "projection_revision", "projection_restated",
    "content_digest", "as_of", "event_cutoff", "observation_cutoff", "attrs", "page_offset", "page_total", "next_cursor"];

fn numeral(text: &str) -> bool {
    let body = text.strip_prefix('-').unwrap_or(text);
    let (int, frac) = body.split_once('.').map_or((body, None), |(i, f)| (i, Some(f)));
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let ratio = text.split_once('/').is_some_and(|(n, d)| digits(n) && digits(d));
    ratio || (digits(int) && frac.is_none_or(digits))
}

/// The formula-injection guard.
pub fn guard(cell: &str) -> String {
    match cell.chars().next() {
        Some('=' | '+' | '-' | '@' | '\t' | '\r') if !numeral(cell) => format!("'{cell}"),
        _ => cell.to_owned(),
    }
}

fn field(cell: &str) -> String {
    let cell = guard(cell);
    if cell.contains([',', '"', '\n', '\r']) || cell.starts_with(' ') || cell.ends_with(' ') { format!("\"{}\"", cell.replace('"', "\"\"")) } else { cell }
}

/// A missing value: `<status>:<reason>`, never empty and never 0.
fn missing(status: &str, reason: &Value) -> String { format!("{status}:{}", reason.as_str().unwrap_or("unknown")) }

fn time(v: &Value, absent: &str) -> String {
    match v.as_i64().and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok()) { Some(t) => t.to_string(), None => absent.to_owned() }
}

fn scalar(v: &Value) -> String {
    match v { Value::String(s) => s.clone(), Value::Null => String::new(), other => other.to_string() }
}

/// `field` of a metric or cell body: its value or the typed missing token.
fn counter(body: &Value, field: &str) -> String {
    match &body[field] {
        Value::Null => body["missing"][field].as_str().unwrap_or("unavailable:unknown").to_owned(),
        v => scalar(v),
    }
}

fn row(cells: &[(&str, String)]) -> String {
    COLUMNS.iter().map(|c| cells.iter().find(|(k, _)| k == c).map(|(_, v)| field(v)).unwrap_or_default()).collect::<Vec<_>>().join(",") + "\r\n"
}

/// Render one export page (`mod.rs` document) as CSV.
pub fn render(doc: &Value) -> String {
    let mut out = COLUMNS.join(",") + "\r\n";
    for m in doc["metrics"].as_array().into_iter().flatten() {
        let projection = &m["projection"];
        let as_of = match (&m["as_of"]["requested"]["unix_ms"], &m["as_of"]["requested"]["seq"]) {
            (_, Value::Number(seq)) => format!("seq:{seq}"),
            (Value::Number(_), _) => time(&m["as_of"]["requested"]["unix_ms"], ""),
            _ => "live".into(),
        };
        let common = vec![("metric_id", scalar(&m["metric_id"])), ("definition", scalar(&m["definition"])), ("cohort", scalar(&m["cohort"])),
            ("window_from", time(&m["window"]["from_unix_ms"], "unbounded")), ("window_to", time(&m["window"]["to_unix_ms"], "unbounded")),
            ("unit", scalar(&m["unit"])), ("projection_mode", scalar(&projection["mode"])),
            ("projection_revision", match &projection["revision"] { Value::Null => "none:live".into(), v => scalar(v) }),
            ("projection_restated", match &projection["restated"] { Value::Bool(b) => b.to_string(), _ => "none:live".into() }),
            ("content_digest", match &projection["content_digest"] { Value::Null => missing("unavailable", &m["reason"]), v => scalar(v) }), ("as_of", as_of)];
        let mut metric = common.clone();
        metric.extend([("row_kind", "metric".into()), ("value_status", scalar(&m["value_status"])),
            ("value", match &m["value"] { Value::Null => counter(m, "value"), Value::String(s) => s.clone(), v => serde_json::to_string(v).unwrap_or_default() }),
            ("value_type", scalar(&m["value_type"])), ("numerator", counter(m, "numerator")), ("denominator", counter(m, "denominator")),
            ("exclusions", serde_json::to_string(&m["exclusions"]).unwrap_or_default()), ("coverage", scalar(&m["coverage"]["state"])),
            ("coverage_known", match &m["coverage"]["known"] { Value::Null => format!("unknown:coverage_{}", m["coverage"]["state"].as_str().unwrap_or("unknown")), v => scalar(v) }),
            ("coverage_expected", match &m["coverage"]["expected"] { Value::Null => format!("unknown:coverage_{}", m["coverage"]["state"].as_str().unwrap_or("unknown")), v => scalar(v) }),
            ("cost_basis", match m["cost_basis"]["status"].as_str() { Some("not_applicable") => missing("not_applicable", &m["cost_basis"]["reason"]),
                _ => serde_json::to_string(&m["cost_basis"]).unwrap_or_default() }),
            ("event_cutoff", time(&m["as_of"]["event_cutoff_unix_ms"], "unavailable:no_event_in_cohort")),
            ("observation_cutoff", time(&m["as_of"]["observation_cutoff_unix_ms"], "unavailable:unknown"))]);
        out += &row(&metric);
        for c in m["cells"].as_array().into_iter().flatten() {
            let (name, label) = c["dimension"].as_object().and_then(|o| o.iter().next()).map(|(k, v)| (k.clone(), scalar(v))).unwrap_or_default();
            let mut cell = common.clone();
            cell.extend([("row_kind", "cell".into()), ("dimension", name), ("dimension_value", label), ("value_status", scalar(&c["value_status"])),
                ("value", match &c["value"] { Value::Null => counter(c, "value"), v => scalar(v) }), ("value_type", scalar(&c["value_type"])),
                ("numerator", counter(c, "numerator")), ("denominator", counter(c, "denominator"))]);
            out += &row(&cell);
        }
    }
    let records = &doc["records"];
    for r in records["rows"].as_array().into_iter().flatten() {
        let mut attrs = r.clone();
        if let Value::Object(o) = &mut attrs { o.remove("entity"); o.remove("id"); }
        out += &row(&[("row_kind", "record".into()), ("metric_id", scalar(&records["metric_id"])), ("definition", scalar(&records["definition"])),
            ("record_bucket", scalar(&records["bucket"])), ("record_entity", scalar(&r["entity"])), ("record_id", scalar(&r["id"])),
            ("projection_revision", match &records["snapshot"]["revision"] { Value::Null => "none:live".into(), v => scalar(v) }),
            ("content_digest", scalar(&records["snapshot"]["content_digest"])), ("attrs", serde_json::to_string(&attrs).unwrap_or_default())]);
    }
    let page = &doc["manifest"]["page"];
    out += &row(&[("row_kind", "page".into()), ("page_offset", scalar(&page["offset"])), ("page_total", match &page["total"] { Value::Null => "not_applicable:no_records".into(), v => scalar(v) }),
        ("next_cursor", match &page["next_cursor"] { Value::Null => "none:last_page".into(), v => scalar(v) })]);
    out
}

//! Text forms of the fleet snapshot: the pane body and the coordinator digest
//! section. Pure functions of the snapshot, so every surface prints the same
//! values; unknown reads `n/a (<reason>)`, never 0.
use super::{DIGEST_MAX_BYTES, DIGEST_MAX_LINES};
use serde_json::Value;
use std::fmt::Write as _;

/// At most this many rows per pane section; the rest are counted.
const PANE_ROWS: usize = 64;
const DIGEST_WAITING: usize = 5;
const DIGEST_CLASSES: usize = 4;
const DIGEST_ARMS: usize = 4;
const DIGEST_GROUPS: usize = 5;
const DIGEST_ALERTS: usize = 5;

fn short(id: &str) -> String { id.chars().take(16).collect() }
fn word(v: &Value) -> &str { v.as_str().unwrap_or("?") }
fn duration(ms: &Value) -> String { ms.as_i64().map_or_else(|| "n/a".into(), crate::telemetry::panel::elapsed) }

fn codes(reasons: &Value) -> String { reasons.as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>().join(",") }

/// `family service=codex role=bug-fix`, as `telemetry health` prints alert labels.
fn scope(labels: &Value) -> String {
    ["family", "service", "role"].iter().filter_map(|k| labels[*k].as_str().map(|v| if *k == "family" { v.to_owned() } else { format!("{k}={v}") })).collect::<Vec<_>>().join(" ")
}

/// A value the way the query service states it: `n/a (<reason>)` when unknown.
fn value(field: &Value) -> String {
    let reason = field["reason"].as_str().or_else(|| field["value"]["reason"].as_str());
    match (&field["value"], reason) {
        (Value::String(s), _) => s.clone(),
        (Value::Number(n), _) => n.to_string(),
        (_, Some(reason)) => format!("n/a ({reason})"),
        (Value::Null, None) => "n/a".into(),
        (other, None) => other.to_string(),
    }
}

/// `●` bound usage, `◐` partial, `○` unavailable (with its reason).
fn coverage(a: &Value) -> String {
    match a["coverage"].as_str() {
        Some("complete") => "●".into(),
        Some("partial") => "◐".into(),
        _ => format!("○ ({})", a["usage"]["reason"].as_str().unwrap_or("unavailable")),
    }
}

fn waiting(a: &Value) -> String {
    let w = &a["waiting"];
    match (w.get("waiting_ms"), w["open"] == true) {
        (Some(ms), true) if ms.as_i64() > Some(0) => format!("{} closed + {} so far (waiting now)", duration(ms), duration(&w["open_observed_ms"])),
        (Some(_), true) => format!("{} so far (waiting now)", duration(&w["open_observed_ms"])),
        (Some(ms), false) => duration(ms),
        (None, _) => format!("n/a ({})", w["reason"].as_str().unwrap_or("unknown")),
    }
}

fn config(label: &Value, id: &Value) -> String {
    match (label.as_str(), id.as_str()) {
        (Some(label), Some(id)) => format!("{label} [{}]", id.trim_start_matches("sha256:").chars().take(8).collect::<String>()),
        (Some(label), None) => label.to_owned(),
        _ => "n/a".into(),
    }
}

fn interval(i: &Value) -> String {
    match (i["lower"].as_str(), i["upper"].as_str()) {
        (Some(lower), Some(upper)) => format!("[{lower}–{upper}]"),
        _ => format!("interval n/a ({})", i["reason"].as_str().unwrap_or("unavailable")),
    }
}

fn arm_evidence(a: &Value, min: &Value) -> String {
    match a["status"].as_str() {
        Some("shown") => format!("{} ({}) {} n={} pooled {}", word(&a["value"]), word(&a["decimal"]), interval(&a["interval"]), a["tasks"],
            match &a["pooled"]["value"] { Value::String(p) => p.clone(), _ => format!("n/a ({})", a["pooled"]["reason"].as_str().unwrap_or("unavailable")) }),
        Some("suppressed") => format!("insufficient data n={} (min {min})", a["tasks"]),
        _ => format!("{} n={}", value(a), a["tasks"]),
    }
}

fn quota_line(q: &Value) -> String {
    let windows: Vec<String> = match q["windows"].as_array() {
        Some(windows) => windows.iter().map(|w| match &w["value"] {
            Value::String(remaining) => format!("{} {remaining}% remaining (window {}m, {})", word(&w["window_kind"]), w["window_minutes"], word(&w["freshness"])),
            other => format!("{} n/a ({})", word(&w["window_kind"]), other["reason"].as_str().unwrap_or("unavailable")),
        }).collect(),
        None => vec![format!("n/a ({})", q["value"]["reason"].as_str().unwrap_or("unavailable"))],
    };
    format!("{} quota at last dispatch ({}): {}", word(&q["service"]), short(word(&q["attempt_id"])), windows.join(" · "))
}

/// Provenance is separate from the read time: recorded history can be stale
/// while active attempts and alerts are read now.
fn history(s: &Value) -> String {
    let mut revisions: Vec<String> = [("M13", &s["coverage"]), ("M38", &s["services"]["M38"]), ("M39", &s["services"]["M39"]),
        ("M40", &s["services"]["M40"]), ("M49", &s["replay"]), ("M02 comparison", &s["configurations"])].into_iter().map(|(name, field)| {
        match (field["as_of"]["seq"].as_i64(), field["as_of"]["unix_ms"].as_i64()) {
            (Some(seq), Some(at)) => format!("{name}={seq}@{at}"),
            _ => format!("{name}=unavailable (no_revision_as_of)"),
        }
    }).collect();
    revisions.insert(0, "History as_of (revision@Unix ms):".into());
    revisions.join(" ")
}

fn missing_quota(services: &Value) -> String {
    if services["M40"]["reason"] == "no_revision_as_of" { value(&services["M40"]) } else { "n/a (no_decisions)".into() }
}

/// The pane body (also `telemetry <slug> workspace show` and `watch`).
pub fn text(s: &Value) -> String {
    let slug = s["project"].as_str().unwrap_or("?");
    if s["status"] != "available" {
        return format!("{slug} · fleet · unavailable ({}): nothing numeric is shown\n", word(&s["reason"]));
    }
    let mut out = String::new();
    let at = s["query_unix_ms"].as_i64().and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok())
        .map_or_else(|| "n/a".into(), |t| t.strftime("%Y-%m-%d %H:%M:%S UTC").to_string());
    let _ = writeln!(out, "{slug} · fleet · query {at} · usage coverage {} · advisory, read-only", s["coverage"].as_object().map_or("n/a".into(), |_| value(&s["coverage"])));

    let _ = writeln!(out, "{}", history(s));

    let needs = s["needs_you"].as_array().cloned().unwrap_or_default();
    let _ = writeln!(out, "─ NEEDS YOU ({})", needs.len());
    for n in needs.iter().take(PANE_ROWS) {
        let _ = match n["kind"].as_str() {
            Some("alert") => writeln!(out, "  ! alert #{} {} {} [{}] {}", n["alert_id"], word(&n["state"]), word(&n["rule"]), scope(&n["labels"]), codes(&n["reasons"])),
            Some("waiting_on_you") => writeln!(out, "  ! {} task {} waiting on you {} so far", short(word(&n["attempt_id"])), word(&n["task_id"]), duration(&n["observed_ms"])),
            _ => writeln!(out, "  ! {} task {}: every arm finished; select a winner (action `Projects: select candidate`)", word(&n["group"]), word(&n["task_id"])),
        };
    }

    let active = s["active"]["attempts"].as_array().cloned().unwrap_or_default();
    let _ = writeln!(out, "─ ACTIVE ({})", active.len());
    for a in active.iter().take(PANE_ROWS) {
        let group = a["group"].as_object().map_or(String::new(), |g| format!(" {} arm {}", word(&g["tag"]), g["arm"]));
        let _ = writeln!(out, "  {} task {} {} config {}{group} · elapsed {} · waiting {} · usage {}", short(word(&a["attempt_id"])), word(&a["task_id"]),
            word(&a["state"]), config(&a["configuration_label"], &a["configuration_id"]), duration(&a["elapsed_ms"]), waiting(a), coverage(a));
    }
    if active.len() > PANE_ROWS { let _ = writeln!(out, "  (+{} more)", active.len() - PANE_ROWS); }

    let services = &s["services"];
    let _ = writeln!(out, "─ SERVICES");
    let _ = writeln!(out, "  M38 throttled time share: {}", value(&services["M38"]));
    let _ = writeln!(out, "  M39 provider error rate: {}", value(&services["M39"]));
    let quota = services["quota_at_last_dispatch"].as_array().cloned().unwrap_or_default();
    if quota.is_empty() { let _ = writeln!(out, "  M40 quota headroom at dispatch: {}", missing_quota(services)); }
    for q in &quota { let _ = writeln!(out, "  {}", quota_line(q)); }

    let c = &s["configurations"];
    if c["status"] == "unavailable" {
        let _ = writeln!(out, "─ CONFIGURATIONS: n/a ({})", word(&c["reason"]));
    } else {
        let _ = writeln!(out, "─ CONFIGURATIONS · M02 acceptance · {} · {} · 95% interval · min {} tasks per cell · never a routing decision",
            word(&c["cohort"]), word(&c["analysis"]), c["min_tasks"]);
        let cells = c["cells"].as_array().cloned().unwrap_or_default();
        if cells.is_empty() { let _ = writeln!(out, "  n/a (no_terminal_tasks_with_one_configuration)"); }
        for cell in cells.iter().take(PANE_ROWS) {
            let _ = writeln!(out, "  {}", word(&cell["task_class"]));
            for a in cell["arms"].as_array().into_iter().flatten().take(PANE_ROWS) {
                let _ = writeln!(out, "    {}  {}", config(&a["label"], &a["configuration_id"]), arm_evidence(a, &c["min_tasks"]));
            }
        }
    }

    let groups = s["candidate_groups"].as_array().cloned().unwrap_or_default();
    if s["candidate_groups"]["status"] == "unavailable" {
        let _ = writeln!(out, "─ CANDIDATE GROUPS: n/a ({})", word(&s["candidate_groups"]["reason"]));
    } else {
        let _ = writeln!(out, "─ CANDIDATE GROUPS ({})", groups.len());
    }
    for g in groups.iter().take(PANE_ROWS) {
        let arms: Vec<String> = g["arms"].as_array().into_iter().flatten()
            .map(|a| format!("arm {} {} {}", a["arm"], config(&a["label"], &a["configuration_id"]), word(&a["outcome"]))).collect();
        let _ = writeln!(out, "  {} task {} {}: {}", word(&g["tag"]), word(&g["task_id"]), word(&g["status"]), arms.join(" · "));
    }

    let _ = writeln!(out, "─ REPLAY");
    let _ = writeln!(out, "  M49 replay suite pass rate: {}", value(&s["replay"]));
    match s["alerts"]["open"].as_array() {
        Some(open) => {
            let _ = writeln!(out, "─ ALERTS ({} open; `health notify` leaves inbox notices)", open.len());
            for a in open.iter().take(PANE_ROWS) {
                let _ = writeln!(out, "  #{} {} {} [{}] {} · since {} · seen {}x{}", a["alert_id"], word(&a["state"]), word(&a["rule"]), scope(&a["labels"]), codes(&a["reasons"]),
                    a["opened_unix_ms"], a["occurrences"], if a["notice_id"].is_string() { " · noticed" } else { "" });
            }
        }
        None => { let _ = writeln!(out, "─ ALERTS: n/a ({})", word(&s["alerts"]["reason"])); }
    }
    let _ = writeln!(out, "actions: fleet-race · fleet-select · fleet-replay (each shows the owner command and runs it only on confirmation)");
    out
}

/// The coordinator digest section (doc 15 §5): advisory, bounded to
/// `DIGEST_MAX_LINES` lines and `DIGEST_MAX_BYTES` bytes by top-N selection,
/// one line when the query service is down.
pub fn digest_section(s: &Value) -> String {
    if s["status"] != "available" {
        return format!("## Fleet (advisory): unavailable ({}); no telemetry evidence this turn\n", word(&s["reason"]));
    }
    let mut lines: Vec<String> = Vec::new();
    let at = s["query_unix_ms"].as_i64().and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok())
        .map_or_else(|| "n/a".into(), |t| t.strftime("%H:%M UTC").to_string());
    lines.push(format!("## Fleet (advisory · as of {at} · {})", super::CONTRACT));

    lines.push(history(s));
    let active = s["active"]["attempts"].as_array().cloned().unwrap_or_default();
    let count = |state: &str| active.iter().filter(|a| a["state"] == state).count();
    let covered = active.iter().filter(|a| a["coverage"] == "complete").count();
    lines.push(format!("Active attempts: {} (running {}, launching {}, reserved {}); bound usage {} of {}", active.len(), count("running"), count("launching"),
        count("reserved"), covered, active.len()));

    let waiting: Vec<&Value> = s["needs_you"].as_array().into_iter().flatten().filter(|n| n["kind"] == "waiting_on_you").collect();
    if waiting.is_empty() {
        lines.push("Waiting on operator: none".into());
    } else {
        let mut shown: Vec<String> = waiting.iter().take(DIGEST_WAITING)
            .map(|n| format!("{} task {} ({} so far)", short(word(&n["attempt_id"])), word(&n["task_id"]), duration(&n["observed_ms"]))).collect();
        if waiting.len() > DIGEST_WAITING { shown.push(format!("+{} more", waiting.len() - DIGEST_WAITING)); }
        lines.push(format!("Waiting on operator: {}", shown.join("; ")));
    }

    let services = &s["services"];
    let quota: Vec<String> = services["quota_at_last_dispatch"].as_array().into_iter().flatten().map(quota_line).collect();
    lines.push(format!("Services: throttled {}; errors {}; {}", value(&services["M38"]), value(&services["M39"]),
        if quota.is_empty() { format!("quota {}", missing_quota(services)) } else { quota.join("; ") }));

    let c = &s["configurations"];
    if c["status"] == "unavailable" {
        lines.push(format!("Routing evidence: n/a ({})", word(&c["reason"])));
    } else {
        lines.push(format!("Routing evidence (M02 acceptance, {}, 95% interval, n; below {} tasks insufficient):", word(&c["cohort"]), c["min_tasks"]));
        let cells = c["cells"].as_array().cloned().unwrap_or_default();
        if cells.is_empty() { lines.push("  none yet".into()); }
        for cell in cells.iter().take(DIGEST_CLASSES) {
            let arms = cell["arms"].as_array().cloned().unwrap_or_default();
            let mut parts: Vec<String> = arms.iter().take(DIGEST_ARMS).map(|a| match a["status"].as_str() {
                Some("shown") => format!("{} {} {} n={}", word(&a["label"]), word(&a["value"]), interval(&a["interval"]), a["tasks"]),
                Some("suppressed") => format!("{} insufficient (n={})", word(&a["label"]), a["tasks"]),
                _ => format!("{} {} n={}", word(&a["label"]), value(a), a["tasks"]),
            }).collect();
            let arm_count = cell["arm_count"].as_u64().map_or(arms.len(), |n| n as usize);
            if arm_count > DIGEST_ARMS { parts.push(format!("+{} more", arm_count - DIGEST_ARMS)); }
            lines.push(format!("  {}: {}", word(&cell["task_class"]), parts.join("; ")));
        }
        let cell_count = c["cell_count"].as_u64().map_or(cells.len(), |n| n as usize);
        if cell_count > DIGEST_CLASSES { lines.push(format!("  +{} more task classes (`telemetry {} compare --metric M02`)", cell_count - DIGEST_CLASSES, word(&s["project"]))); }
    }

    let pending: Vec<&Value> = s["needs_you"].as_array().into_iter().flatten().filter(|n| n["kind"] == "selection_pending").collect();
    if !pending.is_empty() {
        let mut shown: Vec<String> = pending.iter().take(DIGEST_GROUPS).map(|n| format!("{} task {}", word(&n["group"]), word(&n["task_id"]))).collect();
        let count = s["digest_counts"]["pending"].as_u64().map_or(pending.len(), |n| n as usize);
        if count > DIGEST_GROUPS { shown.push(format!("+{} more", count - DIGEST_GROUPS)); }
        lines.push(format!("Candidate groups awaiting the operator's selection: {}", shown.join("; ")));
    }
    match s["alerts"]["open"].as_array() {
        Some(open) => {
            let alerts: Vec<&Value> = s["needs_you"].as_array().into_iter().flatten().filter(|n| n["kind"] == "alert").collect();
            let mut shown: Vec<String> = alerts.iter().take(DIGEST_ALERTS).map(|a| format!("{} {} [{}] {}", word(&a["state"]), word(&a["rule"]), scope(&a["labels"]), codes(&a["reasons"]))).collect();
            let count = s["digest_counts"]["alerts"].as_u64().map_or(alerts.len(), |n| n as usize);
            if count > DIGEST_ALERTS { shown.push(format!("+{} more", count - DIGEST_ALERTS)); }
            lines.push(if open.is_empty() { "Health alerts: none open".into() } else { format!("Health alerts ({} open): {}", count, shown.join("; ")) });
        }
        None => lines.push(format!("Health alerts: n/a ({})", word(&s["alerts"]["reason"]))),
    }
    lines.push(format!("Replay M49: {}", value(&s["replay"])));
    lines.push("Evidence only: it grants no launch, budget or selection. Give a dispatch reason code (launch selection `reason`, `thread start --reason`). Do not copy this section into memory.".into());

    // Hard bounds: the lines above are already top-N; this only guards the caps.
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        let line: String = line.chars().take(400).collect();
        let last = i + 1 == lines.len();
        if i + 2 > DIGEST_MAX_LINES || out.len() + line.len() + 1 + if last { 0 } else { 48 } > DIGEST_MAX_BYTES {
            out += "(fleet section truncated at its size cap)\n";
            break;
        }
        out += &line;
        out.push('\n');
    }
    out
}

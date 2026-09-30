//! Presentation of query-service results: one row per metric with its value,
//! basis, coverage, sample size, lag and projection. Nothing here computes a
//! metric: every number comes from the query result it is given, and an
//! unknown value is `n/a (reason)`, never 0.
use crate::telemetry::analytics::registry::{self, Provider};
use serde_json::{Value, json};

fn na(reason: Option<&str>) -> String { format!("n/a ({})", reason.unwrap_or("unknown")) }

fn provider(r: &Value) -> Option<Provider> { registry::resolve(r["definition"].as_str()?).ok().map(|(_, v)| v.provider) }

/// Up to two decimals, without trailing zeros.
fn decimal(x: f64) -> String {
    let text = format!("{x:.2}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    if text == "-0" { "0".into() } else { text.into() }
}

fn unit_word(unit: &str) -> String {
    match unit {
        "milliseconds" => "ms".into(),
        "native_units" | "ratio" => String::new(),
        other => other.replace('_', " "),
    }
}

/// `"n/d"` with its reading: a percentage for ratios, else a decimal in the metric's unit.
fn fraction(text: &str, unit: &str) -> Option<String> {
    let (n, d) = text.split_once('/')?;
    let (n, d): (f64, f64) = (n.trim().parse().ok()?, d.trim().parse().ok()?);
    if d == 0.0 { return None; }
    Some(if unit == "ratio" { format!("{text} ({:.1}%)", n * 100.0 / d) } else { format!("{text} (= {} {})", decimal(n / d), unit_word(unit)) })
}

/// The accounting basis of a cost value and its tag: `est.` for an estimate,
/// `billed` for a provider-billed amount; never both in one value.
fn cost_basis(r: &Value) -> (Option<String>, &'static str) {
    let basis = r["detail"]["basis"].as_str().map(str::to_owned);
    let tag = match basis.as_deref() {
        Some("provider_billed") => "billed",
        Some(b) if b.contains("estimate") => "est.",
        _ => "",
    };
    (basis.filter(|_| !tag.is_empty()), tag)
}

/// The measurement basis a row states: canonical lifecycle rows, or the lane's own basis/trust.
fn basis(r: &Value) -> String {
    let detail = &r["detail"];
    match provider(r) {
        Some(Provider::Native) => "canonical_lifecycle".into(),
        Some(Provider::Absent(_)) => "no_producer".into(),
        Some(Provider::Recommendation) => "per_recommendation".into(),
        Some(Provider::Central) => detail["basis"].as_str().unwrap_or("central_report").into(),
        Some(Provider::Lane(stream)) => ["basis", "source_trust", "trust"].iter().find_map(|k| detail[*k].as_str()).map_or_else(|| format!("lane_{stream}"), str::to_owned),
        None => "unknown".into(),
    }
}

/// M40: one value per dispatch decision and limit window, never summed.
fn decisions(list: &[Value]) -> String {
    if list.is_empty() { return na(Some("no_decisions")); }
    let shown: Vec<String> = list.iter().take(3).map(|d| {
        let attempt = d["attempt_id"].as_str().unwrap_or("?");
        match d["windows"].as_array() {
            Some(windows) if !windows.is_empty() => {
                let w: Vec<String> = windows.iter().map(|w| match w["value"].as_str() {
                    Some(remaining) => format!("{} {remaining}% left", w["window_kind"].as_str().unwrap_or("window")),
                    None => na(w["value"]["reason"].as_str().or(w["reason"].as_str())),
                }).collect();
                format!("{attempt} {}", w.join(", "))
            }
            _ => format!("{attempt} {}", na(d["value"]["reason"].as_str().or(d["reason"].as_str()))),
        }
    }).collect();
    let more = list.len().saturating_sub(3);
    format!("{} decision(s): {}{}", list.len(), shown.join("; "), if more > 0 { format!("; +{more} more") } else { String::new() })
}

/// The value as the operator reads it.
fn display(r: &Value) -> String {
    let detail = &r["detail"];
    let unit = r["unit"].as_str().unwrap_or("");
    let reason = || r["reason"].as_str().or(r["value"]["reason"].as_str()).or(detail["reason"].as_str()).or(detail["value"]["reason"].as_str());
    if r["status"] == "unavailable" { return na(reason()); }
    if let Some(list) = detail["decisions"].as_array() { return decisions(list); }
    let (_, tag) = cost_basis(r);
    let tag = if tag.is_empty() { String::new() } else { format!("{tag} ") };
    let currency = detail["currency"].as_str().or(r["value"]["currency"].as_str());
    match &r["value"] {
        Value::Null => na(reason().or(Some("no_value"))),
        Value::Object(o) => match (o.get("status").and_then(Value::as_str), o.get("priced_amount").and_then(Value::as_str)) {
            (Some("partial"), Some(amount)) => format!("{tag}{} {amount} observed portion (partial: {})", currency.unwrap_or("?"),
                o.get("reason").and_then(Value::as_str).unwrap_or("unknown")),
            _ => na(o.get("reason").and_then(Value::as_str)),
        },
        Value::String(s) => match currency {
            Some(currency) => format!("{tag}{currency} {s}"),
            None => fraction(s, unit).unwrap_or_else(|| format!("{tag}{s} {}", unit_word(unit)).trim_end().to_owned()),
        },
        Value::Number(n) => format!("{tag}{n} {}", unit_word(unit)).trim_end().to_owned(),
        other => other.to_string(),
    }
}

fn coverage_text(c: &Value) -> String {
    let state = c["state"].as_str().unwrap_or("unknown");
    if let (Some(known), Some(expected)) = (c["known"].as_u64(), c["expected"].as_u64()) { return format!("{state} {known}/{expected}"); }
    let counts: Vec<String> = c["lane"].as_object().into_iter().flatten().filter_map(|(k, v)| v.as_u64().map(|n| format!("{k}={n}"))).collect();
    if counts.is_empty() { state.into() } else { format!("{state} ({})", counts.join(" ")) }
}

/// Sample size and where it was read: `(n, basis)` or `(null, reason)`.
fn sample(r: &Value) -> (Value, &'static str) {
    if r["status"] == "unavailable" { return (Value::Null, "unavailable"); }
    let detail = &r["detail"];
    let candidates: [(&Value, &'static str); 8] = [(&r["denominator"], "denominator"), (&r["samples"], "samples"), (&r["coverage"]["known"], "coverage_known"),
        (&detail["findings"]["denominator"], "findings"), (&detail["coverage"]["entries"], "valued_entries"), (&detail["charges_counted"], "charges_counted"),
        (&detail["coverage"]["certified_sessions"], "certified_sessions"), (&detail["decisions"], "decisions")];
    for (value, basis) in candidates {
        if let Some(n) = value.as_u64() { return (json!(n), basis); }
        if let Some(list) = value.as_array() { return (json!(list.len()), basis); }
    }
    (Value::Null, "sample_not_reported")
}

/// `1h02m`, `1m30s`, `45s`.
fn elapsed(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    match (s / 3600, s / 60 % 60, s % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m{s:02}s"),
        (h, m, _) => format!("{h}h{m:02}m"),
    }
}

fn lag_text(r: &Value) -> String { r["lag_ms"].as_i64().map_or_else(|| na(r["lag_reason"].as_str()), elapsed) }

fn projection_text(p: &Value) -> String {
    match p["revision"].as_i64() {
        Some(n) => format!("revision {n}{}", if p["restated"] == true { " (restated)" } else { "" }),
        None if p["mode"] == "revision" => "no revision".into(),
        None => "live".into(),
    }
}

/// One row: the query's own fields (value, numerator, denominator, coverage,
/// lag, projection) with the presentation next to them.
pub fn row(r: &Value, label: &str) -> Value {
    let (cost_basis, tag) = cost_basis(r);
    let (n, sample_basis) = sample(r);
    let mut row = json!({"metric_id": r["metric_id"], "label": label, "name": r["name"], "definition": r["definition"], "family": r["family"],
        "proxy": r["proxy"], "unit": r["unit"], "cohort": r["cohort"], "status": r["status"], "value": r["value"], "reason": r["reason"],
        "numerator": r["numerator"], "denominator": r["denominator"], "display": display(r), "basis": basis(r), "cost_basis": cost_basis,
        "basis_tag": if tag.is_empty() { Value::Null } else { json!(tag) }, "coverage": r["coverage"], "coverage_text": coverage_text(&r["coverage"]),
        "sample": {"n": n, "basis": sample_basis}, "lag_ms": r["lag_ms"], "lag_reason": r["lag_reason"], "lag_text": lag_text(r),
        "observation_cutoff_unix_ms": r["observation_cutoff_unix_ms"], "event_cutoff_unix_ms": r["event_cutoff_unix_ms"],
        "projection": r["projection"], "projection_text": projection_text(&r["projection"]), "certification": r["certification"]["status"],
        "rate_card_revision": r["rate_card_revision"]});
    row["text"] = json!(line(&row));
    row
}

fn line(row: &Value) -> String {
    let n = row["sample"]["n"].as_u64().map_or_else(|| "n/a".to_owned(), |n| n.to_string());
    format!("  {} {}{}: {} · basis {} · coverage {} · n={n} · lag {} · {}", row["metric_id"].as_str().unwrap_or(""),
        if row["proxy"] == true { "[proxy] " } else { "" }, row["label"].as_str().unwrap_or(""), row["display"].as_str().unwrap_or(""),
        row["basis"].as_str().unwrap_or(""), row["coverage_text"].as_str().unwrap_or(""), row["lag_text"].as_str().unwrap_or(""),
        row["projection_text"].as_str().unwrap_or(""))
}

/// Per requested agent (`agent_kind`: the dispatched profile's kind) cells of one native metric.
pub fn agent_cells(r: &Value) -> Value {
    let unit = r["unit"].as_str().unwrap_or("");
    let cells: Vec<Value> = r["cells"].as_array().into_iter().flatten().map(|c| {
        let kind = c["dimension"]["agent_kind"].clone();
        let shown = match &c["value"] {
            Value::String(s) => fraction(s, unit).unwrap_or_else(|| s.clone()),
            Value::Null => na(c["reason"].as_str()),
            other => other.to_string(),
        };
        json!({"agent_kind": kind, "value": c["value"], "reason": c["reason"], "numerator": c["numerator"], "denominator": c["denominator"], "display": shown})
    }).collect();
    json!({"metric_id": r["metric_id"], "definition": r["definition"], "status": r["status"], "reason": r["reason"], "cells": cells})
}

/// Requested versus reported versus unknown model identity, from the query only.
pub fn identity(m15: Option<&Value>, m02_by_agent: Option<&Value>) -> Value {
    let requested: Vec<Value> = m02_by_agent.and_then(|r| r["cells"].as_array()).into_iter().flatten()
        .map(|c| json!({"agent_kind": c["dimension"]["agent_kind"], "tasks": c["denominator"]})).collect();
    let reported = match m15 {
        Some(r) if r["status"] != "unavailable" && r["numerator"].is_u64() && r["denominator"].is_u64() => {
            let (n, d) = (r["numerator"].as_u64().unwrap_or(0), r["denominator"].as_u64().unwrap_or(0));
            json!({"records": d, "reported": n, "unknown": d - n.min(d)})
        }
        Some(r) => json!({"status": "unavailable", "reason": r["reason"].as_str().or(r["value"]["reason"].as_str()).unwrap_or("unknown")}),
        None => json!({"status": "unavailable", "reason": "not_queried"}),
    };
    json!({"requested_agent": requested, "requested_model_name": {"status": "unavailable", "reason": "not_in_query_service"},
        "reported_effective_model": reported, "hidden_identity": "unknown stays unknown; never inferred from the agent kind"})
}

/// Verified versus integrated versus currently resolved fixes, each from its own metric.
pub fn fixes(m25: Option<&Value>, m27: Option<&Value>, m26: Option<&Value>) -> Value {
    let pair = |r: Option<&Value>| match r {
        Some(r) if r["status"] != "unavailable" => json!({"value": r["value"], "numerator": r["detail"]["findings"]["numerator"], "denominator": r["detail"]["findings"]["denominator"],
            "display": display(r)}),
        Some(r) => json!({"value": null, "display": display(r)}),
        None => json!({"value": null, "display": na(Some("not_queried"))}),
    };
    let integrated = match m27 {
        Some(r) if r["status"] != "unavailable" => match (r["detail"]["denominator"].as_u64(), r["detail"]["censored"].as_u64()) {
            (Some(observed), Some(censored)) => json!({"integrations": observed + censored, "observed_full_horizon_or_reopened": observed, "censored": censored,
                "display": format!("{} integrated ({observed} observed for the reopen horizon, {censored} censored)", observed + censored)}),
            _ => json!({"integrations": null, "display": na(Some("integrations_not_reported"))}),
        },
        Some(r) => json!({"integrations": null, "display": display(r)}),
        None => json!({"integrations": null, "display": na(Some("not_queried"))}),
    };
    json!({"verified": pair(m25), "integrated": integrated, "currently_resolved": pair(m26),
        "note": "verified (M25), integrated (M27 integrations) and currently resolved (M26) are separate outcomes; a reopened fix keeps its verification and loses current resolution"})
}

/// Source watermarks and lag of every family, from the results' own watermarks.
pub fn sources(results: &[Value]) -> Value {
    let canonical = results.iter().find_map(|r| r["source_watermarks"]["canonical"].as_object().cloned()).map_or(Value::Null, Value::Object);
    let lane = results.iter().find(|r| r["source_watermarks"].get("sidecar").is_some());
    let sidecar = match lane {
        Some(r) if r["source_watermarks"]["sidecar"].is_object() => r["source_watermarks"]["sidecar"].clone(),
        Some(r) => json!({"status": "unavailable", "reason": r["lag_reason"].as_str().unwrap_or("collection_not_run")}),
        None => json!({"status": "unavailable", "reason": "no_lane_metric_queried"}),
    };
    let mut families: std::collections::BTreeMap<String, Value> = std::collections::BTreeMap::new();
    for r in results {
        let family = r["family"].as_str().unwrap_or("unknown").to_owned();
        let entry = families.entry(family).or_insert_with(|| json!({"metrics": 0, "unavailable": 0, "lag_ms": null, "lag_reason": null}));
        entry["metrics"] = json!(entry["metrics"].as_u64().unwrap_or(0) + 1);
        if r["status"] == "unavailable" { entry["unavailable"] = json!(entry["unavailable"].as_u64().unwrap_or(0) + 1); }
        if let Some(lag) = r["lag_ms"].as_i64() { entry["lag_ms"] = json!(entry["lag_ms"].as_i64().map_or(lag, |l| l.max(lag))); }
        else if entry["lag_ms"].is_null() { entry["lag_reason"] = r["lag_reason"].clone(); }
    }
    json!({"canonical": canonical, "sidecar": sidecar, "families": families})
}

pub fn header(body: &Value) -> String {
    let window = &body["window"];
    let bound = |v: &Value, open: &str| v.as_i64().map_or_else(|| open.to_owned(), |n| n.to_string());
    let knowledge = body["as_of_unix_ms"].as_i64().map_or_else(|| "live".to_owned(), |n| format!("as of {n}"));
    format!("{} · {} view · window [{}, {}) · {knowledge}\n", body["project"].as_str().unwrap_or(""), body["view"].as_str().unwrap_or(""),
        bound(&window["from_unix_ms"], "-inf"), bound(&window["to_unix_ms"], "+inf"))
}

/// The rows of one view, then its extra section (per agent, fixes or sources).
pub fn rows_text(body: &Value) -> String {
    let mut out = String::new();
    for row in body["rows"].as_array().into_iter().flatten() { out += row["text"].as_str().unwrap_or(""); out.push('\n'); }
    if let Some(cells) = body["by_agent"].as_array() {
        for m in cells {
            if m["status"] == "unavailable" { out += &format!("    {} by agent_kind: {}\n", m["metric_id"].as_str().unwrap_or(""), na(m["reason"].as_str())); continue; }
            for c in m["cells"].as_array().into_iter().flatten() {
                let n = c["denominator"].as_u64().map_or_else(|| "n/a".to_owned(), |n| n.to_string());
                out += &format!("    {} agent_kind={}: {} · n={n}\n", m["metric_id"].as_str().unwrap_or(""), c["agent_kind"].as_str().unwrap_or("?"), c["display"].as_str().unwrap_or(""));
            }
        }
    }
    let identity = &body["identity"];
    if identity.is_object() {
        let requested: Vec<String> = identity["requested_agent"].as_array().into_iter().flatten()
            .map(|c| format!("{} {} tasks", c["agent_kind"].as_str().unwrap_or("?"), c["tasks"])).collect();
        out += &format!("  identity requested agent (profile kind): {}; requested model name: n/a (not_in_query_service)\n",
            if requested.is_empty() { na(Some("no_terminal_tasks")) } else { requested.join(", ") });
        let reported = &identity["reported_effective_model"];
        out += &match reported["records"].as_u64() {
            Some(records) => format!("  identity reported effective model: {} of {records} usage records; unknown: {} records\n", reported["reported"], reported["unknown"]),
            None => format!("  identity reported effective model: {}\n", na(reported["reason"].as_str())),
        };
    }
    let fixes = &body["fixes"];
    if fixes.is_object() {
        out += &format!("  fixes verified: {} | integrated: {} | currently resolved: {}\n", fixes["verified"]["display"].as_str().unwrap_or(""),
            fixes["integrated"]["display"].as_str().unwrap_or(""), fixes["currently_resolved"]["display"].as_str().unwrap_or(""));
    }
    let sources = &body["sources"];
    if sources.is_object() {
        let c = &sources["canonical"];
        out += &format!("  sources canonical: events_head={} last_event={}\n", c["events_head"],
            c["last_event_unix_ms"].as_i64().map_or_else(|| na(Some("no_events")), |n| n.to_string()));
        let s = &sources["sidecar"];
        out += &match s["status"].as_str() {
            Some("unavailable") => format!("  sources sidecar: {}\n", na(s["reason"].as_str())),
            _ => {
                let streams: Vec<String> = s["streams"].as_object().into_iter().flatten().map(|(k, v)| format!("{k}={v}")).collect();
                format!("  sources sidecar: streams {} · last collect {} · valuation revision {}\n", streams.join(" "),
                    s["last_collect_unix_ms"].as_i64().map_or_else(|| na(Some("no_collect_recorded")), |n| n.to_string()),
                    s["valuation"]["revision"].as_i64().map_or_else(|| na(Some("not_priced")), |n| n.to_string()))
            }
        };
        for (family, f) in sources["families"].as_object().into_iter().flatten() {
            out += &format!("  family {family}: {} metrics, {} unavailable, lag {}\n", f["metrics"], f["unavailable"],
                f["lag_ms"].as_i64().map_or_else(|| na(f["lag_reason"].as_str()), elapsed));
        }
    }
    out
}

/// A drill-down page: the row it drills, then its identities and the next cursor.
pub fn drill_text(body: &Value) -> String {
    let mut out = format!("{} · {} view · drill {}\n", body["project"].as_str().unwrap_or(""), body["view"].as_str().unwrap_or(""),
        body["row"]["metric_id"].as_str().unwrap_or(""));
    if let Some(text) = body["row"]["text"].as_str() { out += text; out.push('\n'); }
    let d = &body["drill"];
    if d["status"] == "unavailable" { return out + &format!("  drill: {}\n", na(d["reason"].as_str())); }
    let rows = d["rows"].as_array().map(Vec::as_slice).unwrap_or_default();
    let offset = d["offset"].as_i64().unwrap_or(0);
    out += &format!("  bucket {} rows {}-{} of {} · snapshot {}\n", d["bucket"].as_str().unwrap_or(""), if rows.is_empty() { offset } else { offset + 1 },
        offset + rows.len() as i64, d["total"], d["snapshot"]["revision"].as_i64().map_or_else(|| "live".to_owned(), |n| format!("revision {n}")));
    for r in rows {
        out += &format!("  {} {} {}\n", r["entity"].as_str().unwrap_or(""), r["id"].as_str().unwrap_or(""), r["disposition"].as_str().or(r["state"].as_str()).unwrap_or(""));
    }
    if let Some(cursor) = d["next_cursor"].as_str() { out += &format!("  next: --cursor {cursor}\n"); }
    out
}

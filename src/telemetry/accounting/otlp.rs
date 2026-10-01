//! DG4j cross-surface precedence, isolated from native ledger replay.
use anyhow::Result;
use rusqlite::Connection;
use std::collections::BTreeSet;
use super::ledger::Entry;

/// Presence of a bound native source wins for the whole attempt/harness,
/// even when incomplete or uncertified: OTLP must not conceal native gaps.
pub(super) fn apply_precedence(db: &Connection, entries: &mut [Entry]) -> Result<()> {
    if !entries.iter().any(|e| e.session.starts_with("otlp:")) { return Ok(()); }
    let losing = losing_sessions(db)?;
    for entry in entries.iter_mut().filter(|e| losing.contains(&e.session)) {
        for (_, disposition, reason) in &mut entry.provenance {
            *disposition = "duplicate";
            *reason = Some("native_surface_precedence".to_owned());
        }
    }
    Ok(())
}

/// Shared surface choice for live attempt and metric reads, before ledger sync.
pub(crate) fn losing_sessions(db: &Connection) -> Result<BTreeSet<String>> {
    Ok(db.prepare("SELECT o.session_id FROM rollout_sources o
        WHERE o.originator IN ('otlp:claude-code','otlp:muse') AND EXISTS(
        SELECT 1 FROM rollout_sources n WHERE n.attempt_id=o.attempt_id AND n.binding='bound'
        AND 'otlp:' || n.originator=o.originator)")?
        .query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
}

/// Declare only complete request counters with an accepted harness version.
/// Metrics and ambiguous Muse `tokens.cached` observations remain evidence only.
pub(crate) fn declare_usage(record: &mut serde_json::Value, harness: &str, version: Option<&str>, metrics: bool) {
    use serde_json::json;
    if metrics || record["kind"] != "usage" || !matches!(harness, "claude-code" | "muse") { return; }
    let Some(version) = version.filter(|v| crate::telemetry::codex::accepted_version(&format!("{harness}/{v}"))) else { return; };
    let name = record["native_name"].as_str();
    if !matches!((harness, name), ("claude-code", Some("claude_code.api_request")) | ("muse", Some("model_call"))) { return; }
    let a = &record["attributes"];
    let counters = (|| {
        let (input, output, read, write, reasoning) = if harness == "claude-code" {
            let read = a["cache_read_tokens"].as_u64()?;
            let write = a["cache_creation_tokens"].as_u64()?;
            (a["input_tokens"].as_u64()?.checked_add(read)?.checked_add(write)?, a["output_tokens"].as_u64()?, read, write, 0)
        } else {
            (a["gen_ai.usage.input_tokens"].as_u64()?, a["gen_ai.usage.output_tokens"].as_u64()?,
             a["cache_read_tokens"].as_u64()?, a["cache_write_tokens"].as_u64()?, a["reasoning_tokens"].as_u64()?)
        };
        let total = input.checked_add(output)?;
        if read.checked_add(write)? > input || reasoning > output || [input,output,read,write,reasoning,total].iter().any(|n| *n > 1 << 53) { return None; }
        Some([input,read,write,output,reasoning,total])
    })();
    let Some(counters) = counters else { return; };
    record["cli_version"] = json!(version);
    record["usage_authority"] = json!(if harness == "claude-code" { "api_request" } else { "model_call" });
    for (key, value) in ["input_tokens","cached_input_tokens","cache_write_input_tokens","output_tokens","reasoning_output_tokens","total_tokens"].into_iter().zip(counters) {
        record["attributes"][key] = json!(value);
    }
    if harness == "muse" { record["attributes"]["model"] = record["attributes"]["gen_ai.request.model"].clone(); }
}

/// Keep the central native aggregate compatible with ledger surface precedence.
pub(super) fn store_native_totals(db: &Connection) -> Result<()> {
    db.execute_batch("DELETE FROM accounting_native_totals
        WHERE session_id LIKE 'otlp:%' AND session_id IN (SELECT session_id FROM accounting_selected);
        INSERT INTO accounting_native_totals
        SELECT s.session_id,coalesce(sum(e.input_tokens),0),coalesce(sum(e.output_tokens),0),coalesce(sum(e.reasoning_tokens),0),count(e.entry_id),count(e.model)
        FROM accounting_selected s LEFT JOIN usage_entries e ON e.session_id=s.session_id AND e.basis='delta'
        AND EXISTS(SELECT 1 FROM usage_dispositions d WHERE d.entry_id=e.entry_id AND d.disposition='accepted')
        WHERE s.session_id LIKE 'otlp:%' GROUP BY s.session_id;")?;
    Ok(())
}

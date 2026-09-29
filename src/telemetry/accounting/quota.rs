//! Quota windows (docs/telemetry/contracts-accounting.md §5, doc 05 §5b):
//! Codex rate-limit snapshots (`codex_rate_limits`, read by SQL only) become
//! observations with a trust level and the provider windows they identify,
//! rebuilt whole by each sync. A reset starts a new window, so nothing is ever
//! subtracted across one; a `used` below the window's high-water mark without
//! a reset is flagged, never subtracted. Extended M40 reads them per dispatch
//! decision; M38/M39 have no certified Codex source.
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

use super::unavailable;

/// An observation older than this at a dispatch decision is labeled `stale` (its value is still shown).
pub const STALE_AFTER_MS: i64 = 900_000;
/// Fixed-point places for native percent values (Codex prints at most a few).
const PLACES: u32 = 12;
const HUNDRED: i128 = 100 * 10i128.pow(PLACES);
/// The Codex collector (contracts §5 allowlist) stores only the `primary` window.
const COLLECTED: &str = "primary";
const NOT_COLLECTED: &str = "secondary";

/// `0 ≤ used ≤ 100` in plain decimal notation, as a fixed-point value.
fn percent(text: &str) -> Option<i128> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if whole.is_empty() || whole.len() > 3 || fraction.len() > PLACES as usize || !digits(whole) || !digits(fraction) || text.ends_with('.') { return None; }
    let value = whole.parse::<i128>().ok()? * 10i128.pow(PLACES) + format!("{fraction:0<w$}", w = PLACES as usize).parse::<i128>().ok()?;
    (value <= HUNDRED).then_some(value)
}

/// Exact, trailing zeros trimmed: `42.500000000000` → `42.5`, `58.0` → `58`.
fn show(value: i128) -> String {
    let (whole, fraction) = (value / 10i128.pow(PLACES), value % 10i128.pow(PLACES));
    let fraction = format!("{fraction:0>w$}", w = PLACES as usize);
    let fraction = fraction.trim_end_matches('0');
    if fraction.is_empty() { whole.to_string() } else { format!("{whole}.{fraction}") }
}

struct Window {
    id: String,
    account: String,
    limit: String,
    minutes: i64,
    resets: i64,
    evidence: &'static str,
    first_observed: i64,
    last_observed: i64,
    first_used: i128,
    used: i128,
    plan: Option<String>,
    observations: i64,
    flagged: i64,
}

/// One complete snapshot of a window.
struct Snapshot<'a> {
    account: &'a str,
    limit: &'a str,
    minutes: i64,
    resets: i64,
    observed: i64,
    used: i128,
    plan: &'a Option<String>,
}

impl Window {
    fn open(s: &Snapshot, evidence: &'static str) -> Window {
        Window { id: format!("codex:{}:{}:{COLLECTED}:{}", s.account, s.limit, s.resets), account: s.account.to_owned(), limit: s.limit.to_owned(),
            minutes: s.minutes, resets: s.resets, evidence, first_observed: s.observed, last_observed: s.observed, first_used: s.used, used: s.used,
            plan: s.plan.clone(), observations: 1, flagged: 0 }
    }
}

/// Rebuild `quota_observations` and `quota_windows` inside the sync
/// transaction; returns the number of windows. Per account (execution home)
/// and limit, snapshots are taken in `(observed, session, ordinal)` order.
pub fn store(tx: &Connection) -> Result<usize> {
    tx.execute_batch("DELETE FROM quota_observations; DELETE FROM quota_windows;")?;
    type Row = (String, i64, Option<String>, Option<String>, Option<i64>, Option<i64>, Option<String>, i64, Option<String>, i64);
    let rows: Vec<Row> = tx.prepare("SELECT l.session_id,l.ordinal,l.limit_id,l.used_percent,l.window_minutes,l.resets_at,l.plan_type,l.observed_ts,
        (SELECT min(s.home_digest) FROM rollout_sources s WHERE s.session_id=l.session_id),
        (SELECT count(DISTINCT s.home_digest) FROM rollout_sources s WHERE s.session_id=l.session_id)
        FROM codex_rate_limits l ORDER BY l.observed_ts,l.session_id,l.ordinal")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let (mut current, mut done) = (BTreeMap::<(String, String), Window>::new(), Vec::new());
    for (session, ordinal, limit, used_text, minutes, resets_s, plan, observed, account, homes) in rows {
        let used = used_text.as_deref().and_then(percent);
        let resets = resets_s.and_then(|s| s.checked_mul(1000));
        let (trust, window_id) = match (&account, &limit, &used_text, minutes, resets) {
            (Some(_), _, _, _, _) if homes > 1 => ("account_ambiguous", None),
            (Some(account), Some(limit), Some(_), Some(minutes), Some(resets)) => match used {
                Some(used) if minutes > 0 && minutes.checked_mul(60_000).is_some_and(|w| resets.checked_sub(w).is_some()) => {
                    let key = (account.clone(), limit.clone());
                    let snapshot = Snapshot { account, limit, minutes, resets, observed, used, plan: &plan };
                    match current.get_mut(&key) {
                        None => {
                            let w = Window::open(&snapshot, "first_observation");
                            let id = w.id.clone();
                            current.insert(key, w);
                            ("trusted", Some(id))
                        }
                        Some(w) if resets > w.resets => {
                            let evidence = if observed >= w.resets { "reset_elapsed" } else { "reset_moved" };
                            let next = Window::open(&snapshot, evidence);
                            let id = next.id.clone();
                            done.push(std::mem::replace(w, next));
                            ("trusted", Some(id))
                        }
                        Some(w) if resets < w.resets => ("window_regressed", None),
                        Some(w) if minutes != w.minutes => ("window_conflict", None),
                        Some(w) if used < w.used => { w.flagged += 1; ("used_decreased_without_reset", Some(w.id.clone())) }
                        Some(w) => {
                            (w.used, w.last_observed, w.observations) = (used, observed, w.observations + 1);
                            if plan.is_some() { w.plan = plan.clone(); }
                            ("trusted", Some(w.id.clone()))
                        }
                    }
                }
                _ => ("unparseable", None),
            },
            _ => ("incomplete", None),
        };
        let account = account.filter(|_| homes == 1);
        tx.execute("INSERT INTO quota_observations(session_id,ordinal,service,account,limit_id,window_kind,unit,window_minutes,resets_unix_ms,used,remaining,plan_type,
            observed_unix_ms,trust,window_id) VALUES(?1,?2,'codex',?3,?4,?5,'percent',?6,?7,?8,?9,?10,?11,?12,?13)",
            params![session, ordinal, account, limit, COLLECTED, minutes, resets, used.map(show), used.map(|u| show(HUNDRED - u)), plan, observed, trust, window_id])?;
    }
    done.extend(current.into_values());
    for w in &done {
        tx.execute("INSERT INTO quota_windows(window_id,service,account,limit_id,window_kind,unit,window_minutes,window_start_unix_ms,resets_unix_ms,start_evidence,
            first_observed_unix_ms,last_observed_unix_ms,first_used,used,remaining,observed_increase,plan_type,observations,flagged)
            VALUES(?1,'codex',?2,?3,?4,'percent',?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
            params![w.id, w.account, w.limit, COLLECTED, w.minutes, w.resets - w.minutes * 60_000, w.resets, w.evidence, w.first_observed, w.last_observed,
                show(w.first_used), show(w.used), show(HUNDRED - w.used), show(w.used - w.first_used), w.plan, w.observations, w.flagged])?;
    }
    Ok(done.len())
}

fn synced(db: &Connection) -> Result<bool> {
    let table = |name: &str| db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)", [name], |r| r.get::<_, bool>(0));
    Ok(table("usage_ledger")? && table("quota_windows")? && db.query_row("SELECT 1 FROM usage_ledger", [], |_| Ok(())).optional()?.is_some())
}

/// M38/M39 (doc 07): Codex 0.154.0 exposes neither on a certified field.
pub fn availability_metrics() -> [(&'static str, &'static str, Value); 2] {
    [("M38", "throttled_time_share", json!({"value": unavailable("throttling_not_certified"),
        "detail": "codex rollouts record no throttled intervals; the collector allowlist (contracts §5) keeps no availability events"})),
     ("M39", "provider_error_rate", json!({"value": unavailable("provider_errors_not_certified"),
        "detail": "no typed provider error field is collected or certified for codex 0.154.0; human-readable messages are never certified"}))]
}

/// `(attempt_id, profile kind, execution_home, decided_unix_ms)`.
type Decision = (String, Option<String>, Option<String>, i64);

/// Attempts with a dispatch decision, in attempt order.
fn decisions(project: &Path) -> Result<Vec<Decision>> {
    let path = project.join(".state/state.db");
    if !path.exists() { return Ok(Vec::new()); }
    let db = crate::telemetry::read_only(&path)?;
    if !db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='dispatch_decisions')", [], |r| r.get::<_, bool>(0))? {
        return Ok(Vec::new());
    }
    let rows = db.prepare("SELECT a.id,json_extract(i.payload,'$.inputs.effective_profile.kind'),json_extract(i.payload,'$.inputs.effective_profile.execution_home'),
        d.decided_unix_ms FROM attempts a JOIN dispatch_decisions d ON d.attempt_id=a.id LEFT JOIN attempt_inputs i ON i.attempt_id=a.id ORDER BY a.rowid")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Extended M40 for one decision: per limit and window kind, the latest
/// trusted remaining value of the attempt's account observed at or before the
/// decision, with its age. Native percent; never summed across accounts or services.
fn headroom(db: &Connection, home: &str, decided: i64) -> Result<Value> {
    let account = format!("sha256:{:x}", <sha2::Sha256 as sha2::Digest>::digest(home.as_bytes()));
    let seen: i64 = db.query_row("SELECT count(*) FROM quota_observations WHERE account=?1 AND observed_unix_ms<=?2", params![account, decided], |r| r.get(0))?;
    if seen == 0 { return Ok(json!({"account": account, "value": unavailable("no_observation")})); }
    let limits: Vec<String> = db.prepare("SELECT DISTINCT limit_id FROM quota_observations WHERE account=?1 AND observed_unix_ms<=?2 AND trust='trusted' ORDER BY limit_id")?
        .query_map(params![account, decided], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    if limits.is_empty() { return Ok(json!({"account": account, "value": unavailable("no_trusted_observation")})); }
    let mut windows = Vec::new();
    for limit in limits {
        let (window, observed, resets, minutes, used, remaining): (String, i64, i64, i64, String, String) = db.query_row("SELECT window_id,observed_unix_ms,resets_unix_ms,
            window_minutes,used,remaining FROM quota_observations WHERE account=?1 AND limit_id=?2 AND observed_unix_ms<=?3 AND trust='trusted'
            ORDER BY observed_unix_ms DESC,session_id DESC,ordinal DESC LIMIT 1", params![account, limit, decided],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?;
        let age = decided - observed;
        let mut entry = json!({"limit_id": limit, "window_kind": COLLECTED, "unit": "percent", "window_id": window, "window_minutes": minutes,
            "resets_unix_ms": resets, "observed_unix_ms": observed, "age_ms": age});
        if decided >= resets {
            // The window reset after the snapshot: its remaining value no longer applies, and the new window's is unknown.
            entry["value"] = unavailable("window_reset_since_observation");
        } else {
            entry["value"] = json!(remaining);
            entry["used"] = json!(used);
            entry["freshness"] = json!(if age > STALE_AFTER_MS { "stale" } else { "fresh" });
        }
        windows.push(entry);
        windows.push(json!({"limit_id": limit, "window_kind": NOT_COLLECTED, "value": unavailable("not_collected")}));
    }
    Ok(json!({"account": account, "windows": windows}))
}

/// `accounting quota`: synced windows, observation trust, M38/M39 and extended
/// M40 per dispatch decision. Read-only; `ledger_not_synced` before a sync.
pub fn read(project: &Path, db: &Connection) -> Result<Value> {
    if !synced(db)? { return Ok(unavailable("ledger_not_synced")); }
    let windows: Vec<Value> = db.prepare("SELECT window_id,account,limit_id,window_kind,unit,window_minutes,window_start_unix_ms,resets_unix_ms,start_evidence,
        first_observed_unix_ms,last_observed_unix_ms,first_used,used,remaining,observed_increase,plan_type,observations,flagged
        FROM quota_windows ORDER BY account,limit_id,window_kind,resets_unix_ms")?
        .query_map([], |r| Ok(json!({"window_id": r.get::<_, String>(0)?, "service": "codex", "account": r.get::<_, String>(1)?, "limit_id": r.get::<_, String>(2)?,
            "window_kind": r.get::<_, String>(3)?, "unit": r.get::<_, String>(4)?, "window_minutes": r.get::<_, i64>(5)?,
            "window_start_unix_ms": r.get::<_, i64>(6)?, "resets_unix_ms": r.get::<_, i64>(7)?, "start_evidence": r.get::<_, String>(8)?,
            "first_observed_unix_ms": r.get::<_, i64>(9)?, "last_observed_unix_ms": r.get::<_, i64>(10)?, "first_used": r.get::<_, String>(11)?,
            "used": r.get::<_, String>(12)?, "remaining": r.get::<_, String>(13)?, "observed_increase": r.get::<_, String>(14)?,
            "plan_type": r.get::<_, Option<String>>(15)?, "observations": r.get::<_, i64>(16)?, "flagged": r.get::<_, i64>(17)?})))?
        .collect::<rusqlite::Result<_>>()?;
    let mut trust = BTreeMap::<String, i64>::new();
    for row in db.prepare("SELECT trust,count(*) FROM quota_observations GROUP BY trust")?.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
        let (t, n) = row?;
        trust.insert(t, n);
    }
    let mut list = Vec::new();
    for (attempt, kind, home, decided) in decisions(project)? {
        let mut entry = match (kind.as_deref(), home) {
            (Some(kind), _) if kind != "codex" => json!({"value": unavailable("adapter_absent")}),
            (_, None) => json!({"value": unavailable("execution_home_unknown")}),
            (_, Some(home)) => headroom(db, &home, decided)?,
        };
        entry["attempt_id"] = json!(attempt);
        entry["decided_unix_ms"] = json!(decided);
        entry["service"] = json!("codex");
        list.push(entry);
    }
    let mut metrics = serde_json::Map::new();
    for (id, name, body) in availability_metrics() { metrics.insert(id.to_owned(), super::metric(id, name, body)); }
    let mut m40 = super::metric("M40", "quota_headroom_at_dispatch", json!({"decisions": list, "stale_after_ms": STALE_AFTER_MS}));
    m40["definition"] = json!("M40.quota-windows-v1");
    metrics.insert("M40".to_owned(), m40);
    Ok(json!({"semantics": "not_certified", "windows": windows, "observations": trust, "metrics": metrics}))
}

/// Text view: one line per window, metric and decision window; unknown is `n/a (<reason>)`, never 0.
pub fn text(value: &Value) -> String {
    let reason = |v: &Value| format!("n/a ({})", v["reason"].as_str().unwrap_or("unknown"));
    if value["status"] == "unavailable" { return reason(value) + "\n"; }
    let mut out = String::new();
    for w in value["windows"].as_array().into_iter().flatten() {
        out += &format!("window {} {} {} reset {}: used {}% remaining {}% increase {}% ({} observations, {} flagged, {})\n",
            w["account"].as_str().unwrap_or(""), w["limit_id"].as_str().unwrap_or(""), w["window_kind"].as_str().unwrap_or(""), w["resets_unix_ms"],
            w["used"].as_str().unwrap_or(""), w["remaining"].as_str().unwrap_or(""), w["observed_increase"].as_str().unwrap_or(""),
            w["observations"], w["flagged"], w["start_evidence"].as_str().unwrap_or(""));
    }
    for id in ["M38", "M39"] {
        let m = &value["metrics"][id];
        out += &format!("{id} {} {}\n", m["name"].as_str().unwrap_or(""), reason(&m["value"]));
    }
    for d in value["metrics"]["M40"]["decisions"].as_array().into_iter().flatten() {
        let attempt = d["attempt_id"].as_str().unwrap_or("");
        match d["windows"].as_array() {
            None => out += &format!("M40 {attempt} {}\n", reason(&d["value"])),
            Some(windows) => for w in windows {
                let (limit, kind) = (w["limit_id"].as_str().unwrap_or(""), w["window_kind"].as_str().unwrap_or(""));
                out += &match w["value"].as_str() {
                    Some(remaining) => format!("M40 {attempt} {limit} {kind} remaining {remaining}% age_ms={} {}\n", w["age_ms"], w["freshness"].as_str().unwrap_or("")),
                    None => format!("M40 {attempt} {limit} {kind} {}\n", reason(&w["value"])),
                };
            },
        }
    }
    out
}

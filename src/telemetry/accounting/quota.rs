//! Quota windows (docs/telemetry/contracts-accounting.md §5, doc 05 §5b):
//! Codex rate-limit snapshots (`codex_rate_limits` and, from A4, the secondary
//! window in `codex_rate_limit_windows`, read by SQL only) become
//! observations with a trust level and the provider windows they identify,
//! replayed in order for affected accounts by each sync. A reset starts a new window, so nothing is ever
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
/// What `account` identifies: the execution home's digest, not the provider
/// login (credentials are never read). Two homes holding one login appear as
/// two accounts; `shared_window_candidates` names them, nothing is merged.
pub const ACCOUNT_BASIS: &str = "execution_home";
/// Fixed-point places for native percent values (Codex prints at most a few).
const PLACES: u32 = 12;
const HUNDRED: i128 = 100 * 10i128.pow(PLACES);
/// `resets_at` jitter within one window (codex-live-0.154.0-run2.md §6: one
/// snapshot +5 s): before a window's reset elapsed, a snapshot whose reset
/// differs from the window's by at most this many ms is the same window,
/// neither `reset_moved` nor `window_regressed`. The same tolerance matches
/// shared-window candidates across accounts.
pub const RESETS_TOLERANCE_MS: i64 = 60_000;
/// Window kinds of a Codex snapshot; `secondary` comes from A4 (ingest 0004), certified `fixture`.
const KINDS: [&str; 2] = ["primary", "secondary"];

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
    kind: &'static str,
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
    kind: &'static str,
    minutes: i64,
    resets: i64,
    observed: i64,
    used: i128,
    plan: &'a Option<String>,
}

impl Window {
    fn open(s: &Snapshot, evidence: &'static str) -> Window {
        Window { id: format!("codex:{}:{}:{}:{}", s.account, s.limit, s.kind, s.resets), account: s.account.to_owned(), limit: s.limit.to_owned(), kind: s.kind,
            minutes: s.minutes, resets: s.resets, evidence, first_observed: s.observed, last_observed: s.observed, first_used: s.used, used: s.used,
            plan: s.plan.clone(), observations: 1, flagged: 0 }
    }
}

/// One window of a snapshot as collected: `used_percent` text, `window_minutes`, `resets_at` seconds.
type Fields = (Option<String>, Option<i64>, Option<i64>);

/// Resume the last window of a key from its exact persisted fixed-point fields.
fn current_window(tx: &Connection, account: &str, limit: &str, kind: &'static str) -> Result<Option<Window>> {
    Ok(tx.prepare_cached("SELECT window_id,window_minutes,resets_unix_ms,start_evidence,first_observed_unix_ms,last_observed_unix_ms,
        first_used,used,plan_type,observations,flagged FROM quota_windows WHERE account=?1 AND limit_id=?2 AND window_kind=?3
        ORDER BY resets_unix_ms DESC LIMIT 1")?.query_row(params![account, limit, kind], |r| {
        let evidence: String = r.get(3)?;
        let first: String = r.get(6)?;
        let used: String = r.get(7)?;
        Ok(Window { id: r.get(0)?, account: account.to_owned(), limit: limit.to_owned(), kind, minutes: r.get(1)?, resets: r.get(2)?,
            evidence: match evidence.as_str() { "reset_elapsed" => "reset_elapsed", "reset_moved" => "reset_moved", "first_observation" => "first_observation", _ => return Err(rusqlite::Error::InvalidQuery) },
            first_observed: r.get(4)?, last_observed: r.get(5)?, first_used: percent(&first).ok_or(rusqlite::Error::InvalidQuery)?, used: percent(&used).ok_or(rusqlite::Error::InvalidQuery)?,
            plan: r.get(8)?, observations: r.get(9)?, flagged: r.get(10)? })
    }).optional()?)
}

/// Rebuild `quota_window_observations` and `quota_windows` inside the sync
/// transaction; returns the number of windows. Per account (execution home),
/// limit and window kind, snapshots are taken in `(observed, session, ordinal)`
/// order. The secondary window (A4) follows the same rules; a snapshot whose
/// A4 row is absent (read before A4) has no secondary observation.
pub fn store(tx: &Connection) -> Result<usize> {
    tx.execute_batch("CREATE TEMP TABLE IF NOT EXISTS accounting_selected(session_id TEXT PRIMARY KEY); DELETE FROM accounting_selected;
        INSERT INTO accounting_selected SELECT session_id FROM rollout_sources UNION SELECT session_id FROM codex_rate_limits;")?;
    store_scoped(tx, true)
}

pub(crate) fn store_scoped(tx: &Connection, full: bool) -> Result<usize> {
    tx.execute_batch("CREATE TEMP TABLE IF NOT EXISTS accounting_accounts(account TEXT PRIMARY KEY); DELETE FROM accounting_accounts;
        INSERT OR IGNORE INTO accounting_accounts SELECT home_digest FROM rollout_sources WHERE session_id IN (SELECT session_id FROM accounting_selected);
        INSERT OR IGNORE INTO accounting_accounts SELECT account FROM quota_window_observations WHERE session_id IN (SELECT session_id FROM accounting_selected) AND account IS NOT NULL;
        CREATE TEMP TABLE IF NOT EXISTS accounting_quota_sessions(session_id TEXT PRIMARY KEY); DELETE FROM accounting_quota_sessions;
        INSERT INTO accounting_quota_sessions SELECT session_id FROM accounting_selected UNION SELECT session_id FROM rollout_sources WHERE home_digest IN (SELECT account FROM accounting_accounts);")?;
    type Order = (i64, String, i64);
    let first_new: Option<Order> = tx.query_row("SELECT l.observed_ts,l.session_id,l.ordinal FROM codex_rate_limits l
        WHERE l.session_id IN (SELECT session_id FROM accounting_selected) AND NOT EXISTS(SELECT 1 FROM quota_window_observations o
        WHERE o.session_id=l.session_id AND o.ordinal=l.ordinal AND o.window_kind='primary') ORDER BY l.observed_ts,l.session_id,l.ordinal LIMIT 1",
        [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
    let last: Option<Order> = tx.query_row("SELECT observed_unix_ms,session_id,ordinal FROM quota_window_observations
        ORDER BY observed_unix_ms DESC,session_id DESC,ordinal DESC LIMIT 1", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
    let correction: bool = tx.query_row("SELECT quota_rebuild FROM accounting_stream", [], |r| r.get(0))?;
    // A late snapshot can change every later trust decision of its account.
    // Ordered new rows extend the exact saved state; all other changes replay.
    let append = !full && !correction && first_new.as_ref().is_none_or(|first| last.as_ref().is_none_or(|last| first > last));
    if full { tx.execute_batch("DELETE FROM quota_window_observations; DELETE FROM quota_windows;")?; }
    else if !append { tx.execute_batch("DELETE FROM quota_window_observations WHERE session_id IN (SELECT session_id FROM accounting_quota_sessions);
        DELETE FROM quota_windows WHERE account IN (SELECT account FROM accounting_accounts);")?; }
    let filter = if append { " WHERE l.session_id IN (SELECT session_id FROM accounting_selected) AND NOT EXISTS(SELECT 1 FROM quota_window_observations o
        WHERE o.session_id=l.session_id AND o.ordinal=l.ordinal AND o.window_kind='primary')" }
        else if full { "" } else { " WHERE l.session_id IN (SELECT session_id FROM accounting_quota_sessions)" };
    type Row = (String, i64, Option<String>, Fields, Option<String>, i64, Option<String>, i64, Option<Fields>, Option<String>);
    let rows: Vec<Row> = tx.prepare(&format!("SELECT l.session_id,l.ordinal,l.limit_id,l.used_percent,l.window_minutes,l.resets_at,l.plan_type,l.observed_ts,
        (SELECT min(s.home_digest) FROM rollout_sources s WHERE s.session_id=l.session_id),
        (SELECT count(DISTINCT s.home_digest) FROM rollout_sources s WHERE s.session_id=l.session_id),
        w.session_id IS NOT NULL,w.secondary_used_percent,w.secondary_window_minutes,w.secondary_resets_at,w.rate_limit_reached_type
        FROM codex_rate_limits l LEFT JOIN codex_rate_limit_windows w ON w.session_id=l.session_id AND w.ordinal=l.ordinal
        {filter} ORDER BY l.observed_ts,l.session_id,l.ordinal"))?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, (r.get(3)?, r.get(4)?, r.get(5)?), r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?,
            r.get::<_, bool>(10)?.then(|| Ok::<_, rusqlite::Error>((r.get(11)?, r.get(12)?, r.get(13)?))).transpose()?, r.get(14)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let (mut current, mut done) = (BTreeMap::<(String, String, &str), Window>::new(), Vec::new());
    for (session, ordinal, limit, primary, plan, observed, account, homes, secondary, reached) in rows {
        for (kind, fields) in KINDS.into_iter().zip([Some(primary), secondary]) {
            let Some((used_text, minutes, resets_s)) = fields else { continue };
            let used = used_text.as_deref().and_then(percent);
            let resets = resets_s.and_then(|s| s.checked_mul(1000));
            let (trust, window_id) = match (&account, &limit, &used_text, minutes, resets) {
                (Some(_), _, _, _, _) if homes > 1 => ("account_ambiguous", None),
                // A secondary window the rollout reported as `null`.
                (_, _, None, None, None) if kind == "secondary" => ("not_reported", None),
                (Some(account), Some(limit), Some(_), Some(minutes), Some(resets)) => match used {
                    Some(used) if minutes > 0 && minutes.checked_mul(60_000).is_some_and(|w| resets.checked_sub(w).is_some()) => {
                        let key = (account.clone(), limit.clone(), kind);
                        let snapshot = Snapshot { account, limit, kind, minutes, resets, observed, used, plan: &plan };
                        if append && !current.contains_key(&key) && let Some(window) = current_window(tx, account, limit, kind)? {
                            current.insert(key.clone(), window);
                        }
                        match current.get_mut(&key) {
                            None => {
                                let w = Window::open(&snapshot, "first_observation");
                                let id = w.id.clone();
                                current.insert(key, w);
                                ("trusted", Some(id))
                            }
                            // Within the tolerance of the window's reset, before it elapsed: the same window (jitter).
                            Some(w) if (resets - w.resets).abs() <= RESETS_TOLERANCE_MS && observed < w.resets => {
                                if minutes != w.minutes { ("window_conflict", None) }
                                else if used < w.used { w.flagged += 1; ("used_decreased_without_reset", Some(w.id.clone())) }
                                else {
                                    (w.used, w.last_observed, w.observations) = (used, observed, w.observations + 1);
                                    if plan.is_some() { w.plan = plan.clone(); }
                                    ("trusted", Some(w.id.clone()))
                                }
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
            tx.prepare_cached("INSERT INTO quota_window_observations(session_id,ordinal,service,account,limit_id,window_kind,unit,window_minutes,resets_unix_ms,used,remaining,
                plan_type,observed_unix_ms,trust,window_id,rate_limit_reached_type) VALUES(?1,?2,'codex',?3,?4,?5,'percent',?6,?7,?8,?9,?10,?11,?12,?13,?14)")?
                    .execute(params![session, ordinal, account.as_ref().filter(|_| homes == 1), limit, kind, minutes, resets, used.map(show), used.map(|u| show(HUNDRED - u)),
                    plan, observed, trust, window_id, reached])?;
        }
    }
    done.extend(current.into_values());
    for w in &done {
        tx.prepare_cached("INSERT OR REPLACE INTO quota_windows(window_id,service,account,limit_id,window_kind,unit,window_minutes,window_start_unix_ms,resets_unix_ms,start_evidence,
            first_observed_unix_ms,last_observed_unix_ms,first_used,used,remaining,observed_increase,plan_type,observations,flagged)
            VALUES(?1,'codex',?2,?3,?4,'percent',?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)")?
                .execute(params![w.id, w.account, w.limit, w.kind, w.minutes, w.resets - w.minutes * 60_000, w.resets, w.evidence, w.first_observed, w.last_observed,
                show(w.first_used), show(w.used), show(HUNDRED - w.used), show(w.used - w.first_used), w.plan, w.observations, w.flagged])?;
    }
    tx.execute("UPDATE accounting_stream SET quota_rebuild=0", [])?;
    Ok(tx.query_row("SELECT count(*) FROM quota_windows", [], |r| r.get(0))?)
}

pub(crate) fn synced(db: &Connection) -> Result<bool> {
    let table = |name: &str| db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)", [name], |r| r.get::<_, bool>(0));
    Ok(table("usage_ledger")? && table("quota_window_observations")? && db.query_row("SELECT 1 FROM usage_ledger", [], |_| Ok(())).optional()?.is_some())
}

/// M38/M39 (doc 07): Codex 0.154.0 exposes neither on a certified field.
pub fn availability_metrics() -> [(&'static str, &'static str, Value); 2] {
    [("M38", "throttled_time_share", json!({"value": unavailable("throttling_not_certified"),
        "detail": "codex rollouts record no throttled intervals; rate_limit_reached_type is kept as evidence only, its semantics are not certified"})),
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

/// One window kind of one limit at a decision: the latest trusted remaining
/// value of the account observed at or before it, with its age. Without one:
/// `not_reported` (the latest snapshot's window was `null`), `not_collected`
/// (no snapshot carries the kind, e.g. read before A4), else `no_trusted_observation`.
fn window(db: &Connection, account: &str, limit: &str, kind: &str, decided: i64) -> Result<Value> {
    let trusted = db.prepare_cached("SELECT window_id,observed_unix_ms,resets_unix_ms,window_minutes,used,remaining FROM quota_window_observations
        WHERE account=?1 AND limit_id=?2 AND window_kind=?3 AND observed_unix_ms<=?4 AND trust='trusted'
        ORDER BY observed_unix_ms DESC,session_id DESC,ordinal DESC LIMIT 1")?.query_row(params![account, limit, kind, decided],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?, r.get::<_, String>(4)?, r.get::<_, String>(5)?))).optional()?;
    let Some((window, observed, resets, minutes, used, remaining)) = trusted else {
        let latest: Option<String> = db.prepare_cached("SELECT trust FROM quota_window_observations WHERE account=?1 AND limit_id=?2 AND window_kind=?3 AND observed_unix_ms<=?4
            ORDER BY observed_unix_ms DESC,session_id DESC,ordinal DESC LIMIT 1")?
                .query_row(params![account, limit, kind, decided], |r| r.get(0)).optional()?;
        let reason = match latest.as_deref() { Some("not_reported") => "not_reported", None => "not_collected", Some(_) => "no_trusted_observation" };
        return Ok(json!({"limit_id": limit, "window_kind": kind, "value": unavailable(reason)}));
    };
    let age = decided - observed;
    let mut entry = json!({"limit_id": limit, "window_kind": kind, "unit": "percent", "window_id": window, "window_minutes": minutes,
        "resets_unix_ms": resets, "observed_unix_ms": observed, "age_ms": age});
    // Other homes that reported this very window (same limit, kind, length and reset) by the decision:
    // probably one login. Named, never merged; their values are not used here.
    let shared: Vec<String> = db.prepare_cached("SELECT DISTINCT account FROM quota_window_observations WHERE limit_id=?1 AND window_kind=?2 AND window_minutes=?3
        AND resets_unix_ms BETWEEN ?4-?6 AND ?4+?6 AND observed_unix_ms<=?5 AND trust='trusted' AND account IS NOT NULL ORDER BY account")?
        .query_map(params![limit, kind, minutes, resets, decided, RESETS_TOLERANCE_MS], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    if shared.iter().any(|a| a != account) { entry["shared_window_candidates"] = json!(shared); }
    if decided >= resets {
        // The window reset after the snapshot: its remaining value no longer applies, and the new window's is unknown.
        entry["value"] = unavailable("window_reset_since_observation");
    } else {
        entry["value"] = json!(remaining);
        entry["used"] = json!(used);
        entry["freshness"] = json!(if age > STALE_AFTER_MS { "stale" } else { "fresh" });
    }
    Ok(entry)
}

/// Extended M40 for one decision: per limit, each window kind (`window`).
/// Native percent; never summed across accounts, limits, kinds or services.
pub(crate) fn headroom(db: &Connection, home: &str, decided: i64) -> Result<Value> {
    let account = format!("sha256:{:x}", <sha2::Sha256 as sha2::Digest>::digest(home.as_bytes()));
    // Both lookups run once per dispatch decision: an existence test, and the
    // account's few limit ids walked through the index (a plain DISTINCT read
    // every observation of the account each time; certificate-scale.md §5).
    let seen: bool = db.prepare_cached("SELECT EXISTS(SELECT 1 FROM quota_window_observations WHERE account=?1 AND observed_unix_ms<=?2)")?
        .query_row(params![account, decided], |r| r.get(0))?;
    if !seen { return Ok(json!({"account": account, "account_basis": ACCOUNT_BASIS, "value": unavailable("no_observation")})); }
    let limits: Vec<String> = db.prepare_cached("WITH RECURSIVE ids(id) AS (SELECT min(limit_id) FROM quota_window_observations WHERE account=?1
        UNION ALL SELECT (SELECT min(limit_id) FROM quota_window_observations WHERE account=?1 AND limit_id>ids.id) FROM ids WHERE ids.id IS NOT NULL)
        SELECT id FROM ids WHERE id IS NOT NULL AND EXISTS(SELECT 1 FROM quota_window_observations q WHERE q.account=?1 AND q.limit_id=ids.id
        AND q.observed_unix_ms<=?2 AND q.trust='trusted') ORDER BY id")?
        .query_map(params![account, decided], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    if limits.is_empty() { return Ok(json!({"account": account, "account_basis": ACCOUNT_BASIS, "value": unavailable("no_trusted_observation")})); }
    let mut windows = Vec::new();
    for limit in limits {
        for kind in KINDS { windows.push(window(db, &account, &limit, kind, decided)?); }
    }
    Ok(json!({"account": account, "account_basis": ACCOUNT_BASIS, "windows": windows}))
}

/// Keep historical decision answers alongside the quota projection. Ordered
/// snapshots later than a decision cannot change it. Earlier corrections can
/// also change shared-window candidates, so all homes at/after the earliest
/// affected observation are reconsidered.
pub(crate) fn store_dispatch(project: &Path, db: &Connection, full: bool, floor: Option<i64>) -> Result<()> {
    let canonical = serde_json::to_string(&crate::telemetry::analytics::inputs::canonical(project)?)?;
    let previous: Option<String> = db.query_row("SELECT canonical FROM accounting_dispatch_frontier WHERE singleton=1", [], |r| r.get(0)).optional()?;
    let all = full || previous.as_deref() != Some(canonical.as_str());
    if all { db.execute_batch("DELETE FROM accounting_dispatch_headroom;")?; }
    if all || floor.is_some() {
        let mut insert = db.prepare_cached("INSERT INTO accounting_dispatch_headroom(attempt_id,home_digest,decided_unix_ms,body) VALUES(?1,?2,?3,?4)
            ON CONFLICT(attempt_id) DO UPDATE SET home_digest=excluded.home_digest,decided_unix_ms=excluded.decided_unix_ms,body=excluded.body")?;
        for (attempt, kind, home, decided) in decisions(project)? {
            if kind.as_deref().is_some_and(|k| k != "codex") || !all && floor.is_some_and(|at| decided < at) { continue; }
            let Some(home) = home else { continue; };
            let body = headroom(db, &home, decided)?;
            insert.execute(params![attempt, body["account"].as_str().unwrap_or_default(), decided, serde_json::to_string(&body)?])?;
        }
    }
    db.execute("INSERT INTO accounting_dispatch_frontier(singleton,canonical) VALUES(1,?1)
        ON CONFLICT(singleton) DO UPDATE SET canonical=excluded.canonical", [canonical])?;
    Ok(())
}

pub(crate) fn dispatch_current(project: &Path, db: &Connection) -> Result<bool> {
    // M40 has always read the last synced quota projection, even if the next
    // collect has pending rows. A maintenance invalidation requires fallback.
    if !db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='accounting_dispatch_frontier')", [], |r| r.get::<_, bool>(0))? { return Ok(false); }
    let canonical = serde_json::to_string(&crate::telemetry::analytics::inputs::canonical(project)?)?;
    let valid: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM accounting_dispatch_frontier WHERE canonical=?1)
        AND EXISTS(SELECT 1 FROM accounting_stream WHERE invalidated IS NULL)", [&canonical], |r| r.get(0))?;
    Ok(valid)
}

pub(crate) fn stored_headroom(db: &Connection, attempt: &str, home: &str, decided: i64) -> Result<Option<Value>> {
    let account = format!("sha256:{:x}", <sha2::Sha256 as sha2::Digest>::digest(home.as_bytes()));
    let body: Option<String> = db.prepare_cached("SELECT body FROM accounting_dispatch_headroom WHERE attempt_id=?1 AND home_digest=?2 AND decided_unix_ms=?3")?
        .query_row(params![attempt, account, decided], |r| r.get(0)).optional()?;
    body.map(|b| serde_json::from_str(&b).map_err(Into::into)).transpose()
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
    // Windows of different accounts with the same limit, kind, length and reset (within the jitter
    // tolerance of the group's earliest reset): one provider window seen from several homes.
    let mut groups = BTreeMap::<(String, String, i64, i64), Vec<(String, String)>>::new();
    let mut ordered: Vec<&Value> = windows.iter().collect();
    ordered.sort_by_key(|w| (w["limit_id"].as_str().unwrap_or("").to_owned(), w["window_kind"].as_str().unwrap_or("").to_owned(),
        w["window_minutes"].as_i64().unwrap_or(0), w["resets_unix_ms"].as_i64().unwrap_or(0)));
    for w in ordered {
        let text = |key: &str| w[key].as_str().unwrap_or("").to_owned();
        let (minutes, resets) = (w["window_minutes"].as_i64().unwrap_or(0), w["resets_unix_ms"].as_i64().unwrap_or(0));
        let anchor = groups.keys().rev().find(|k| k.0 == text("limit_id") && k.1 == text("window_kind") && k.2 == minutes && resets - k.3 <= RESETS_TOLERANCE_MS)
            .map_or(resets, |k| k.3);
        groups.entry((text("limit_id"), text("window_kind"), minutes, anchor)).or_default().push((text("account"), text("window_id")));
    }
    let shared: Vec<Value> = groups.into_iter().filter(|(_, members)| members.iter().any(|m| m.0 != members[0].0)).map(|((limit, kind, minutes, resets), mut members)| {
        members.sort();
        let (accounts, ids): (Vec<String>, Vec<String>) = members.into_iter().unzip();
        json!({"limit_id": limit, "window_kind": kind, "window_minutes": minutes, "resets_unix_ms": resets, "accounts": accounts, "window_ids": ids,
            "evidence": "same_limit_kind_minutes_resets", "merged": false})
    }).collect();
    let mut trust = BTreeMap::<String, BTreeMap<String, i64>>::new();
    for row in db.prepare("SELECT window_kind,trust,count(*) FROM quota_window_observations GROUP BY window_kind,trust")?
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))? {
        let (kind, t, n) = row?;
        trust.entry(kind).or_default().insert(t, n);
    }
    // `rate_limit_reached_type` per snapshot: evidence only, never a throttling metric.
    let mut reached = BTreeMap::<String, i64>::new();
    for row in db.prepare("SELECT rate_limit_reached_type,count(DISTINCT session_id||':'||ordinal) FROM quota_window_observations
        WHERE rate_limit_reached_type IS NOT NULL GROUP BY 1")?.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
        let (t, n) = row?;
        reached.insert(t, n);
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
    Ok(json!({"semantics": "not_certified", "account_basis": ACCOUNT_BASIS, "windows": windows, "shared_window_candidates": shared, "observations": trust,
        "evidence": {"rate_limit_reached_type": {"snapshots": reached, "semantics": "not_certified", "certified": "fixture"}}, "metrics": metrics}))
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
    for c in value["shared_window_candidates"].as_array().into_iter().flatten() {
        let accounts: Vec<&str> = c["accounts"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        out += &format!("shared window candidate {} {} reset {}: accounts {} (execution homes; not merged, never summed)\n",
            c["limit_id"].as_str().unwrap_or(""), c["window_kind"].as_str().unwrap_or(""), c["resets_unix_ms"], accounts.join(", "));
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

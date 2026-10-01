//! DG3 project operating time. Only consecutive observed Active endpoints
//! contribute duration; open tails, restarts and missed passes never do.
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

pub const STREAM: &str = "operating";
pub const MIGRATIONS: &[&str] = &[include_str!(
    "../../migrations/telemetry/operating/0001_intervals.sql"
)];
/// A gap strictly longer than three expected ticker passes breaks continuity.
pub const GAP_PASSES: i64 = 3;

/// Public sidecar producer API. The caller supplies an observed project state,
/// its canonical control epoch, a unique ticker-run identity and pass cadence.
/// Repeated/non-forward observations are harmless; all writes commit together.
pub fn observe(
    project: &Path,
    session: &str,
    active: bool,
    control_epoch: i64,
    at: i64,
    cadence_ms: i64,
) -> Result<()> {
    ensure!(
        !session.is_empty() && cadence_ms > 0 && at >= 0,
        "invalid operating observation"
    );
    let mut db = super::sidecar::open(project, true)?.expect("created sidecar");
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let previous: Option<(i64, String, bool, i64, i64)> = tx.query_row(
        "SELECT last_unix_ms,session,active,control_epoch,cadence_ms FROM operating_clock WHERE singleton=1", [],
        |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
    let mut merge = false;
    if let Some((last, old_session, was_active, epoch, cadence)) = previous {
        if at <= last {
            return Ok(());
        }
        let reason = if old_session != session {
            Some("restart")
        } else if at.saturating_sub(last) > cadence.saturating_mul(GAP_PASSES) {
            Some("gap")
        } else if epoch != control_epoch || was_active != active {
            Some("control_changed")
        } else if !active {
            Some("paused")
        } else {
            None
        };
        if let Some(reason) = reason {
            tx.execute(
                "UPDATE operating_intervals SET close_reason=?1 WHERE close_reason='open'",
                [reason],
            )?;
            if reason != "paused" {
                tx.execute(
                    "INSERT INTO operating_gaps(start_unix_ms,end_unix_ms,reason) VALUES(?1,?2,?3)",
                    params![last, at, reason],
                )?;
            }
        } else {
            merge = was_active;
        }
    }
    if active {
        if merge {
            tx.execute(
                "UPDATE operating_intervals SET end_unix_ms=?1 WHERE close_reason='open'",
                [at],
            )?;
        } else {
            tx.execute("INSERT INTO operating_intervals(session,control_epoch,start_unix_ms,end_unix_ms,close_reason) VALUES(?1,?2,?3,?3,'open')", params![session,control_epoch,at])?;
        }
    }
    tx.execute("INSERT INTO operating_clock VALUES(1,?1,?1,?2,?3,?4,?5)
        ON CONFLICT(singleton) DO UPDATE SET last_unix_ms=excluded.last_unix_ms,session=excluded.session,active=excluded.active,control_epoch=excluded.control_epoch,cadence_ms=excluded.cadence_ms",
        params![at,session,active,control_epoch,cadence_ms])?;
    tx.commit()?;
    Ok(())
}

/// Read canonical project control only; operating observations grant no work.
pub fn observe_project(project: &Path, session: &str, cadence_ms: i64) -> Result<()> {
    let db = super::read_only(&project.join(".state/state.db"))?;
    let (state, epoch): (String, i64) = db.query_row(
        "SELECT state,epoch FROM project_control WHERE singleton=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    observe(
        project,
        session,
        state == "active",
        epoch,
        jiff::Timestamp::now().as_millisecond(),
        cadence_ms,
    )
}

fn table(db: &Connection, name: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [name],
        |r| r.get(0),
    )?)
}

/// M03 uses one authoritative acceptance per task, never receipt/retry count.
/// Duration is the union of clipped observed intervals, in integer ms. The
/// unreduced rational tasks/hour avoids rounding in query, revisions and export.
pub fn evaluate(project: &Path, from: Option<i64>, to: Option<i64>) -> Result<Value> {
    let observed = super::sidecar::read(project)?
        .as_deref()
        .map(|db| -> Result<bool> {
            if !table(db, "operating_clock")? {
                return Ok(false);
            }
            Ok(
                db.query_row("SELECT EXISTS(SELECT 1 FROM operating_clock)", [], |r| {
                    r.get(0)
                })?,
            )
        })
        .transpose()?
        .unwrap_or(false);
    if !observed {
        return evaluate_population(project, std::iter::empty(), from, to);
    }
    let db = super::read_only(&project.join(".state/state.db"))?;
    let tasks = super::metrics::task_evidence(&db)?;
    let replay = if table(&db, "replay_candidates")? {
        db.prepare("SELECT task_id FROM replay_candidates")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<BTreeSet<String>>>()?
    } else {
        BTreeSet::new()
    };
    evaluate_evidence(project, &tasks, &replay, from, to)
}

pub(crate) fn evaluate_evidence(
    project: &Path,
    tasks: &[(String, String, bool)],
    replay: &BTreeSet<String>,
    from: Option<i64>,
    to: Option<i64>,
) -> Result<Value> {
    evaluate_population(
        project,
        tasks
            .iter()
            .map(|(id, _, accepted)| (id.as_str(), *accepted, replay.contains(id))),
        from,
        to,
    )
}

fn evaluate_population<'a>(
    project: &Path,
    tasks: impl Iterator<Item = (&'a str, bool, bool)>,
    from: Option<i64>,
    to: Option<i64>,
) -> Result<Value> {
    let unavailable = || {
        json!({"definition":"M03.operating-v1","name":"accepted_throughput","status":"unavailable","reason":"operating_hours_not_recorded","value":null,
        "numerator":null,"denominator":null,"coverage":{"state":"unknown","known":null,"expected":null,"missing":null,"unit":"operating_milliseconds","reasons":{"operating_hours_not_recorded":null}},"exclusions":{}})
    };
    let Some(db) = super::sidecar::read(project)? else {
        return Ok(unavailable());
    };
    if !table(&db, "operating_clock")? {
        return Ok(unavailable());
    }
    let clock: Option<(i64, i64)> = db
        .query_row(
            "SELECT first_unix_ms,last_unix_ms FROM operating_clock WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((first, last)) = clock else {
        return Ok(unavailable());
    };
    let lo = from.unwrap_or(first);
    let hi = to.unwrap_or(last);
    let rows: Vec<(i64,i64,String)> = db.prepare("SELECT start_unix_ms,end_unix_ms,close_reason FROM operating_intervals WHERE start_unix_ms<=?2 AND end_unix_ms>=?1 ORDER BY start_unix_ms,end_unix_ms")?
        .query_map(params![lo,hi], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut ms = 0i64;
    let mut end = lo;
    let mut censored = 0;
    for (start, stop, reason) in &rows {
        if reason == "open" && to.is_none_or(|to| to > *stop) {
            censored += 1;
        }
        let begin = (*start).max(lo).max(end);
        let stop = (*stop).min(hi);
        ms = ms.saturating_add(stop.saturating_sub(begin).max(0));
        end = end.max(stop);
    }
    let gaps: i64 = db.query_row(
        "SELECT count(*) FROM operating_gaps WHERE start_unix_ms<?2 AND end_unix_ms>?1",
        params![lo, hi],
        |r| r.get(0),
    )?;
    // Current validity comes from the shared lifecycle evidence. Occurrence
    // stays at the earliest authoritative receipt across contract revisions:
    // accepting a correction must not move the original work into a new window.
    let canonical = super::read_only(&project.join(".state/state.db"))?;
    let times: BTreeMap<String,Option<i64>> = canonical.prepare("SELECT c.task_id,
        min(CASE WHEN c.route='verify_only' THEN r.created_unix_ms ELSE k.created_unix_ms END)
        FROM task_contracts c JOIN result_submissions s ON s.task_id=c.task_id AND s.contract_revision=c.contract_revision
        JOIN verified_results r ON r.submission_id=s.submission_id
        LEFT JOIN integration_operations i ON i.verified_result_id=r.result_id
        LEFT JOIN integrated_commits k ON k.operation_id=i.operation_id GROUP BY c.task_id")?
        .query_map([], |r| Ok((r.get(0)?,r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut accepted = 0i64;
    let mut unknown_time = 0;
    let mut replay = 0;
    let mut cutoff = None;
    for (id, is_accepted, is_replay) in tasks {
        if is_replay {
            replay += 1;
            continue;
        }
        if !is_accepted {
            continue;
        }
        match times.get(id).copied().flatten() {
            Some(at) if from.is_none_or(|f| at >= f) && to.is_none_or(|t| at < t) => {
                accepted += 1;
                cutoff = Some(cutoff.map_or(at, |old: i64| old.max(at)));
            }
            None => unknown_time += 1,
            _ => {}
        }
    }
    let mut reasons = BTreeMap::new();
    if gaps > 0 {
        reasons.insert("observation_gaps", gaps);
    }
    if lo < first {
        reasons.insert("before_observation", 1);
    }
    if hi > last || to.is_none() {
        reasons.insert("unobserved_tail", 1);
    }
    if unknown_time > 0 {
        reasons.insert("acceptance_time_unknown", unknown_time);
    }
    let partial = !reasons.is_empty();
    Ok(
        json!({"definition":"M03.operating-v1","name":"accepted_throughput",
        "status":if ms == 0 {"empty"} else if partial {"partial"} else {"available"},
        "value":if ms == 0 {Value::Null} else {json!(format!("{}/{}",i128::from(accepted)*3_600_000,i128::from(ms)))},
        "reason":if ms == 0 {Some("zero_operating_hours")} else {None},
        "numerator":accepted,"denominator":format!("{ms}/3600000"),"operating_ms":ms,
        "coverage":{"state":if partial {"partial"} else {"complete"},"known":ms,"expected":if partial {Value::Null} else {json!(ms)},
            "missing":if partial {Value::Null} else {json!(0)},"unit":"operating_milliseconds","reasons":reasons},
        "censored":{"open_intervals":censored},"provisional":partial,
        "exclusions":{"acceptance_time_unknown":unknown_time,"replay_candidate":replay},
        "event_cutoff_unix_ms":cutoff}),
    )
}

/// Reports reuse the central reader's already-loaded acceptance evidence.
pub fn metrics(_: &Path, _: Option<i64>) -> Result<BTreeMap<String, Value>> {
    Ok(BTreeMap::new())
}
/// Observation is driven by ticker passes, independently of collection cadence.
pub fn tick(_: &Path, _: super::codex::Budget) -> Result<()> {
    Ok(())
}

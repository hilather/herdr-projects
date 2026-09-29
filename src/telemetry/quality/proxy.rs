//! TM3.7 proxy signals (contracts-quality.md §1): the pinned-CI result of each
//! task's first candidate (M45) and a test-weakening flag from `git diff
//! --numstat` under `tests/`. Counts only; no path or diff text is kept.
//! `source_trust = proxy_observed`: never read to accept, verify or integrate.
use anyhow::Result;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

const KIND: &str = "first_candidate_ci";
/// Flagged when the candidate removes more lines than it adds under `tests/`.
const RULE: &str = "tests-net-removal.v1";

/// A task's first submission (by `created_unix_ms`, then insertion) and the
/// first `verification_runs` row for it, if any.
struct First { task: String, submission: String, attempt: String, repository: String, base: String, candidate: String, run: Option<Run> }
struct Run { id: String, state: String, policy: String, at: i64 }

fn first_candidates(project: &Path) -> Result<Vec<First>> {
    let db = super::super::read_only(&project.join(".state/state.db"))?;
    let rows = db.prepare("SELECT s.task_id,s.submission_id,s.attempt_id,s.repository,s.base_oid,s.candidate_oid,r.run_id,r.state,r.policy_digest,r.created_unix_ms
        FROM result_submissions s LEFT JOIN verification_runs r ON r.rowid=(SELECT v.rowid FROM verification_runs v WHERE v.submission_id=s.submission_id ORDER BY v.created_unix_ms,v.rowid LIMIT 1)
        WHERE s.rowid=(SELECT f.rowid FROM result_submissions f WHERE f.task_id=s.task_id ORDER BY f.created_unix_ms,f.rowid LIMIT 1) ORDER BY s.task_id")?
        .query_map([], |r| {
            let run = match r.get::<_, Option<String>>(6)? {
                Some(id) => Some(Run { id, state: r.get(7)?, policy: r.get(8)?, at: r.get(9)? }),
                None => None,
            };
            Ok(First { task: r.get(0)?, submission: r.get(1)?, attempt: r.get(2)?, repository: r.get(3)?, base: r.get(4)?, candidate: r.get(5)?, run })
        })?.collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// `(added, deleted, binary files)` under `tests/`, or the reason it is unknown.
fn test_diff(repository: &str, base: &str, candidate: &str) -> std::result::Result<(i64, i64, i64), &'static str> {
    use crate::execution_guard::GatedSpawn;
    if !Path::new(repository).is_dir() { return Err("repository_missing"); }
    let out = std::process::Command::new("git").current_dir(repository)
        .env_remove("GIT_DIR").env_remove("GIT_WORK_TREE").env_remove("GIT_INDEX_FILE").env_remove("GIT_OBJECT_DIRECTORY").env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .args(["diff", "--numstat", "--no-renames", "--no-ext-diff", "--no-textconv", "--end-of-options", base, candidate, "--", ":(top)tests/"])
        .output_gated().map_err(|_| "git_unavailable")?;
    if !out.status.success() { return Err("diff_failed"); }
    let text = String::from_utf8(out.stdout).map_err(|_| "diff_unparseable")?;
    let (mut added, mut deleted, mut binary) = (0i64, 0i64, 0i64);
    for line in text.lines() {
        let mut fields = line.splitn(3, '\t');
        match (fields.next(), fields.next(), fields.next()) {
            (Some("-"), Some("-"), Some(_)) => binary += 1,
            (Some(a), Some(d), Some(_)) => {
                added += a.parse::<i64>().map_err(|_| "diff_unparseable")?;
                deleted += d.parse::<i64>().map_err(|_| "diff_unparseable")?;
            }
            _ => return Err("diff_unparseable"),
        }
    }
    Ok((added, deleted, binary))
}

#[derive(Debug, Default, serde::Serialize)]
pub struct Collected {
    pub observed: u64,
    pub flagged: u64,
    pub weakening_unavailable: u64,
    /// First candidates left for the next pass because the diff limit was reached.
    pub deferred: u64,
}

/// Write a signal for each task whose first candidate has a verification run
/// and no settled signal yet (an `unavailable` diff is retried). The canonical
/// store is only read; `create` false leaves an absent sidecar absent.
pub fn collect(project: &Path, create: bool, limit: usize) -> Result<Collected> {
    let mut out = Collected::default();
    let Some(mut db) = super::super::sidecar::open(project, create)? else { return Ok(out) };
    let settled: std::collections::BTreeSet<String> = db.prepare("SELECT task_id FROM proxy_signals WHERE kind=?1 AND weakening<>'unavailable'")?
        .query_map([KIND], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    let pending: Vec<First> = first_candidates(project)?.into_iter().filter(|f| f.run.is_some() && !settled.contains(&f.task)).collect();
    let mut rows = Vec::new();
    for first in pending {
        if rows.len() >= limit { out.deferred += 1; continue; }
        let diff = test_diff(&first.repository, &first.base, &first.candidate);
        rows.push((first, diff));
    }
    let now = jiff::Timestamp::now().as_millisecond();
    let tx = db.transaction()?;
    for (first, diff) in &rows {
        let Some(run) = &first.run else { continue };
        let (counts, weakening, reason) = match diff {
            Ok((added, deleted, binary)) => (Some((*added, *deleted, *binary)), if deleted > added { "flagged" } else { "clear" }, None),
            Err(reason) => (None, "unavailable", Some(*reason)),
        };
        tx.execute("INSERT INTO proxy_signals(kind,task_id,submission_id,attempt_id,run_id,policy_digest,base_oid,candidate_oid,ci_state,verified_unix_ms,
            tests_added_lines,tests_deleted_lines,tests_binary_files,weakening,weakening_reason,weakening_rule,source_trust,observed_unix_ms)
            VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,'proxy_observed',?17)
            ON CONFLICT(kind,task_id) DO UPDATE SET tests_added_lines=excluded.tests_added_lines,tests_deleted_lines=excluded.tests_deleted_lines,
            tests_binary_files=excluded.tests_binary_files,weakening=excluded.weakening,weakening_reason=excluded.weakening_reason,observed_unix_ms=excluded.observed_unix_ms
            WHERE proxy_signals.weakening='unavailable'",
            params![KIND, first.task, first.submission, first.attempt, run.id, run.policy, first.base, first.candidate, run.state, run.at,
                counts.map(|c| c.0), counts.map(|c| c.1), counts.map(|c| c.2), weakening, reason, RULE, now])?;
        out.observed += 1;
        match weakening { "flagged" => out.flagged += 1, "unavailable" => out.weakening_unavailable += 1, _ => {} }
    }
    tx.commit()?;
    Ok(out)
}

/// M45 first-candidate CI pass (proxy): tasks whose first candidate's first
/// pinned-CI run accepted / tasks with such a run and a clear weakening check.
/// Flagged and unchecked candidates are excluded and reported; tasks whose
/// first candidate is not yet verified are `pending`.
pub fn m45(project: &Path, since: Option<i64>) -> Result<Value> {
    let firsts = first_candidates(project)?;
    let sidecar = super::super::sidecar::read(project)?;
    let signals = match &sidecar {
        Some(db) if has_table(db)? => Some(signals(db)?),
        _ => None,
    };
    let in_window = |run: &Run| since.is_none_or(|since| run.at >= since);
    let (mut passed, mut denominator, mut pending, mut not_collected, mut unavailable) = (0usize, 0usize, 0usize, 0usize, 0usize);
    let mut flagged = Vec::new();
    for first in &firsts {
        let Some(run) = &first.run else { if since.is_none() { pending += 1; } continue };
        if !in_window(run) { continue; }
        match signals.as_ref().and_then(|s| s.get(&first.task)) {
            Some(signal) if signal["run_id"] == run.id.as_str() => match signal["weakening"].as_str() {
                Some("clear") => { denominator += 1; if run.state == "accepted" { passed += 1; } }
                Some("flagged") => flagged.push(signal.clone()),
                _ => unavailable += 1,
            },
            _ => not_collected += 1,
        }
    }
    let flagged_count = flagged.len();
    let mut body = json!({"definition": "M45.proxy-v1", "name": "first_candidate_ci_pass_proxy", "proxy": true, "source_trust": "proxy_observed",
        "weakening_rule": RULE, "numerator": passed, "denominator": denominator, "pending": pending, "flagged": flagged,
        "excluded": {"test_weakening": flagged_count, "weakening_unavailable": unavailable, "not_collected": not_collected}});
    body["value"] = if signals.is_none() { json!({"status": "unavailable", "reason": "collection_not_run"}) }
        else if denominator == 0 { body["reason"] = json!("empty_denominator"); Value::Null }
        else { json!(format!("{passed}/{denominator}")) };
    Ok(body)
}

fn has_table(db: &Connection) -> rusqlite::Result<bool> {
    db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='proxy_signals')", [], |r| r.get(0))
}

/// Stored signals by task, as reported for flagged candidates.
fn signals(db: &Connection) -> rusqlite::Result<BTreeMap<String, Value>> {
    db.prepare("SELECT task_id,submission_id,attempt_id,run_id,ci_state,tests_added_lines,tests_deleted_lines,tests_binary_files,weakening FROM proxy_signals WHERE kind=?1")?
        .query_map([KIND], |r| Ok((r.get::<_, String>(0)?, json!({"task_id": r.get::<_, String>(0)?, "submission_id": r.get::<_, String>(1)?,
            "attempt_id": r.get::<_, String>(2)?, "run_id": r.get::<_, String>(3)?, "ci_state": r.get::<_, String>(4)?, "tests_added_lines": r.get::<_, Option<i64>>(5)?,
            "tests_deleted_lines": r.get::<_, Option<i64>>(6)?, "tests_binary_files": r.get::<_, Option<i64>>(7)?, "weakening": r.get::<_, String>(8)?}))))?
        .collect()
}

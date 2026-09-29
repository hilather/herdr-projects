//! TM3.7 integration outcomes (contracts-quality.md §2): whether each
//! integrated commit was reverted on its integrated ref within a horizon (M48)
//! and how many of the lines it added survive at the horizon by `git blame`
//! (M47). Integrations younger than the horizon are censored. Counts and
//! object IDs only; no path, message or line text is kept.
//! `source_trust = proxy_observed`: never read to accept, verify or integrate.
use anyhow::Result;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub const DEFAULT_HORIZON_DAYS: u32 = 14;
const DAY_MS: i64 = 86_400_000;
/// Excludes vendored and lock files from survival; lines are attributed by blame.
const RULE: &str = "outcomes.v1";
const MAX_FILES: usize = 32;
const MAX_COMMITS: usize = 512;
const MAX_OUTPUT: u64 = 4 << 20;
/// Worst-case git calls for one integration: range, revert scan, horizon
/// commit, diff, tree listing, churn, and two blames per file.
pub const CALLS_PER_INTEGRATION: usize = 6 + 2 * MAX_FILES;

struct Integration { id: String, repository: String, ref_name: String, commit: String, parent: String, at: i64 }

fn integrations(project: &Path) -> Result<Vec<Integration>> {
    let db = super::super::read_only(&project.join(".state/state.db"))?;
    let rows = db.prepare("SELECT integrated_id,repository,ref_name,commit_oid,expected_old_oid,created_unix_ms FROM integrated_commits ORDER BY created_unix_ms,integrated_id")?
        .query_map([], |r| Ok(Integration { id: r.get(0)?, repository: r.get(1)?, ref_name: r.get(2)?, commit: r.get(3)?, parent: r.get(4)?, at: r.get(5)? }))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

type Git<T> = std::result::Result<T, &'static str>;

/// `git` in `repo` through the spawn gate, stdout capped at `MAX_OUTPUT`.
fn git(repo: &str, args: &[&str]) -> Git<String> {
    use crate::execution_guard::GatedSpawn;
    use std::io::Read;
    use std::process::{Command, Stdio};
    let mut child = Command::new("git").current_dir(repo)
        .env_remove("GIT_DIR").env_remove("GIT_WORK_TREE").env_remove("GIT_INDEX_FILE").env_remove("GIT_OBJECT_DIRECTORY").env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .args(["--literal-pathspecs", "-c", "log.showSignature=false"]).args(args)
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn_gated().map_err(|_| "git_unavailable")?;
    let mut out = Vec::new();
    let read = child.stdout.take().map(|s| s.take(MAX_OUTPUT + 1).read_to_end(&mut out));
    if !matches!(read, Some(Ok(_))) || out.len() as u64 > MAX_OUTPUT {
        let _ = child.kill();
        let _ = child.wait();
        return Err("output_limit");
    }
    if !child.wait().map_err(|_| "git_unavailable")?.success() { return Err("git_failed"); }
    String::from_utf8(out).map_err(|_| "git_unparseable")
}

struct Observed { horizon: String, reverted: &'static str, added: i64, surviving: i64, churn: (i64, i64) }

/// `(added, deleted)` lines; `None` for a binary file.
type Lines = Option<(i64, i64)>;

/// Per path of `git diff --numstat -z`.
fn numstat(text: &str) -> Git<Vec<(String, Lines)>> {
    text.split_terminator('\0').map(|record| {
        let mut fields = record.splitn(3, '\t');
        match (fields.next(), fields.next(), fields.next()) {
            (Some("-"), Some("-"), Some(path)) => Ok((path.to_owned(), None)),
            (Some(a), Some(d), Some(path)) => Ok((path.to_owned(), Some((a.parse().map_err(|_| "git_unparseable")?, d.parse().map_err(|_| "git_unparseable")?)))),
            _ => Err("git_unparseable"),
        }
    }).collect()
}

/// Lines that `git blame --incremental` output attributes to `commits`.
fn blamed(text: &str, commits: &BTreeSet<&str>) -> i64 {
    text.lines().filter_map(|line| {
        let fields: Vec<&str> = line.split(' ').collect();
        match fields[..] {
            [sha, _, _, n] if commits.contains(sha) => n.parse::<i64>().ok(),
            _ => None,
        }
    }).sum()
}

fn vendored(path: &str) -> bool {
    path.ends_with(".lock") || path.split('/').any(|part| part == "vendor" || part == "third_party")
}

fn observe(i: &Integration, deadline_ms: i64, calls: &mut usize) -> Git<Observed> {
    let repo = i.repository.as_str();
    if !Path::new(repo).is_dir() { return Err("repository_missing"); }
    let until = format!("--until=@{}", deadline_ms.div_euclid(1000));
    let mut run = |args: &[&str]| { *calls += 1; git(repo, args) };
    // The integration's own commits (the merge and its branch), and its parent tree.
    let range = run(&["log", "--no-color", "--boundary", "--format=%m %H %T", "--end-of-options", &format!("{}..{}", i.parent, i.commit), "--"])?;
    let (mut own, mut parent_tree) = (BTreeSet::new(), None);
    for line in range.lines() {
        match line.split(' ').collect::<Vec<_>>()[..] {
            ["-", sha, tree] if sha == i.parent => parent_tree = Some(tree),
            ["-", _, _] => {}
            [_, sha, _] => { own.insert(sha); }
            _ => return Err("git_unparseable"),
        }
    }
    let parent_tree = parent_tree.ok_or("not_on_ref")?;
    if own.len() > MAX_COMMITS { return Err("history_limit"); }
    // Commits on the ref after the integration, up to the horizon.
    let limit = format!("--max-count={}", MAX_COMMITS + 1);
    let later = run(&["log", "--no-color", "--format=%x1e%H %T%n%B", &limit, &until, "--end-of-options", &format!("^{}", i.commit), &i.ref_name, "--"])?;
    let mut commits = Vec::new();
    for record in later.split('\x1e').skip(1) {
        let (head, body) = record.split_once('\n').unwrap_or((record, ""));
        let (sha, tree) = head.split_once(' ').ok_or("git_unparseable")?;
        let targets: Vec<&str> = body.match_indices("This reverts commit ")
            .map(|(at, m)| body[at + m.len()..].split(|c: char| !c.is_ascii_hexdigit()).next().unwrap_or("")).collect();
        commits.push((sha, tree, targets));
    }
    if commits.len() > MAX_COMMITS { return Err("history_limit"); }
    // A revert counts unless a later revert within the horizon reverts it (lineage).
    // `git log` lists newer commits first, so each commit's reverters are already decided.
    let mut effective = BTreeMap::new();
    for (sha, _, _) in &commits {
        let undone = commits.iter().any(|(other, _, targets)| targets.contains(sha) && effective.get(other) == Some(&true));
        effective.insert(*sha, !undone);
    }
    let live = |sha: &str| effective.get(sha) == Some(&true);
    let reverted = if commits.iter().any(|(sha, _, targets)| live(sha) && targets.iter().any(|t| own.contains(t))) { "trailer" }
        else if commits.iter().any(|(sha, tree, _)| live(sha) && *tree == parent_tree) { "tree_restore" }
        else { "none" };
    // Survival at the ref's first-parent commit at the horizon.
    let horizon = run(&["rev-list", "-1", "--first-parent", &until, "--end-of-options", &i.ref_name, "--"])?.trim().to_owned();
    if horizon != i.commit && !commits.iter().any(|(sha, _, _)| *sha == horizon) { return Err("not_on_ref"); }
    let diff = numstat(&run(&["diff", "--numstat", "-z", "--no-renames", "--no-ext-diff", "--no-textconv", "--end-of-options", &i.parent, &i.commit, "--"])?)?;
    let files: Vec<String> = diff.into_iter().filter(|(path, counts)| matches!(counts, Some((a, _)) if *a > 0) && !vendored(path)).map(|(path, _)| path).collect();
    if files.len() > MAX_FILES { return Err("too_many_files"); }
    let (mut added, mut surviving, mut churn) = (0, 0, (0, 0));
    if !files.is_empty() {
        let paths: Vec<&str> = files.iter().map(String::as_str).collect();
        let present = run(&[&["ls-tree", "-r", "-z", "--name-only", "--end-of-options", &horizon, "--"], &paths[..]].concat())?;
        let present: BTreeSet<&str> = present.split_terminator('\0').collect();
        for (_, counts) in numstat(&run(&[&["diff", "--numstat", "-z", "--no-renames", "--no-ext-diff", "--no-textconv", "--end-of-options", &i.commit, &horizon, "--"], &paths[..]].concat())?)? {
            let (a, d) = counts.unwrap_or((0, 0));
            churn = (churn.0 + a, churn.1 + d);
        }
        for path in &paths {
            let at_commit = blamed(&run(&["blame", "--incremental", &i.commit, "--", path])?, &own);
            added += at_commit;
            if present.contains(path) {
                surviving += blamed(&run(&["blame", "--incremental", &horizon, "--", path])?, &own).min(at_commit);
            }
        }
    }
    Ok(Observed { horizon, reverted, added, surviving, churn })
}

#[derive(Debug, Default, serde::Serialize)]
pub struct Collected {
    pub horizon_days: u32,
    pub observed: u64,
    /// Integrations younger than the horizon: not observed yet.
    pub censored: u64,
    pub unavailable: u64,
    /// Integrations left for the next pass because the call budget was spent.
    pub deferred: u64,
}

/// Write an outcome for each integration past the horizon without a settled
/// one (an unavailable outcome is retried), within `max_calls` git calls.
/// The canonical store is only read; `create` false leaves an absent sidecar absent.
pub fn collect(project: &Path, create: bool, horizon_days: u32, max_calls: usize) -> Result<Collected> {
    let mut out = Collected { horizon_days, ..Collected::default() };
    let Some(mut db) = super::super::sidecar::open(project, create)? else { return Ok(out) };
    let horizon_ms = i64::from(horizon_days) * DAY_MS;
    let settled: BTreeSet<String> = db.prepare("SELECT integrated_id FROM integration_outcomes WHERE horizon_ms=?1 AND unavailable_reason IS NULL")?
        .query_map([horizon_ms], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    let now = jiff::Timestamp::now().as_millisecond();
    let (mut calls, mut rows) = (0usize, Vec::new());
    for integration in integrations(project)?.into_iter().filter(|i| !settled.contains(&i.id)) {
        if now < integration.at + horizon_ms { out.censored += 1; continue; }
        if max_calls.saturating_sub(calls) < CALLS_PER_INTEGRATION { out.deferred += 1; continue; }
        let observed = observe(&integration, integration.at + horizon_ms, &mut calls);
        rows.push((integration, observed));
    }
    let tx = db.transaction()?;
    for (i, observed) in &rows {
        let (o, reason) = match observed { Ok(o) => (Some(o), None), Err(reason) => (None, Some(*reason)) };
        tx.execute("INSERT INTO integration_outcomes(integrated_id,horizon_ms,commit_oid,integrated_unix_ms,horizon_oid,reverted,added_lines,surviving_lines,
            churn_added_lines,churn_deleted_lines,unavailable_reason,rule,source_trust,observed_unix_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,'proxy_observed',?13)
            ON CONFLICT(integrated_id,horizon_ms) DO UPDATE SET horizon_oid=excluded.horizon_oid,reverted=excluded.reverted,added_lines=excluded.added_lines,
            surviving_lines=excluded.surviving_lines,churn_added_lines=excluded.churn_added_lines,churn_deleted_lines=excluded.churn_deleted_lines,
            unavailable_reason=excluded.unavailable_reason,observed_unix_ms=excluded.observed_unix_ms WHERE integration_outcomes.unavailable_reason IS NOT NULL",
            params![i.id, horizon_ms, i.commit, i.at, o.map(|o| &o.horizon), o.map(|o| o.reverted), o.map(|o| o.added), o.map(|o| o.surviving),
                o.map(|o| o.churn.0), o.map(|o| o.churn.1), reason, RULE, now])?;
        out.observed += 1;
        if reason.is_some() { out.unavailable += 1; }
    }
    tx.commit()?;
    Ok(out)
}

/// M47 code survival and M48 revert rate (proxies) at `horizon_days`, over
/// integrations created at or after `since`.
pub fn metrics(project: &Path, since: Option<i64>, horizon_days: u32) -> Result<(Value, Value)> {
    let horizon_ms = i64::from(horizon_days) * DAY_MS;
    let sidecar = super::super::sidecar::read(project)?;
    let rows = match &sidecar {
        Some(db) if has_table(db)? => Some(outcomes(db, horizon_ms)?),
        _ => None,
    };
    let now = jiff::Timestamp::now().as_millisecond();
    let (mut censored, mut not_collected, mut unavailable, mut observed) = (0usize, 0usize, 0usize, 0usize);
    let (mut trailer, mut tree, mut added, mut surviving, mut churn) = (0usize, 0usize, 0i64, 0i64, (0i64, 0i64));
    for i in integrations(project)?.iter().filter(|i| since.is_none_or(|since| i.at >= since)) {
        if now < i.at + horizon_ms { censored += 1; continue; }
        match rows.as_ref().and_then(|rows| rows.get(&i.id)) {
            None => not_collected += 1,
            Some(None) => unavailable += 1,
            Some(Some((reverted, a, s, c))) => {
                observed += 1;
                match reverted.as_str() { "trailer" => trailer += 1, "tree_restore" => tree += 1, _ => {} }
                (added, surviving, churn) = (added + a, surviving + s, (churn.0 + c.0, churn.1 + c.1));
            }
        }
    }
    let metric = |definition: &str, name: &str, numerator: i64, denominator: i64, (key, extra): (&str, Value)| {
        let mut body = json!({"definition": definition, "name": name, "proxy": true, "source_trust": "proxy_observed", "rule": RULE, "horizon_days": horizon_days,
            "numerator": numerator, "denominator": denominator, "censored": censored, "not_collected": not_collected, "unavailable": unavailable, key: extra});
        body["value"] = if rows.is_none() { json!({"status": "unavailable", "reason": "collection_not_run"}) }
            else if denominator == 0 { body["reason"] = json!("empty_denominator"); Value::Null }
            else { json!(format!("{numerator}/{denominator}")) };
        body
    };
    let m47 = metric("M47.proxy-v1", "code_survival_proxy", surviving, added, ("area_churn", json!({"added_lines": churn.0, "deleted_lines": churn.1})));
    let m48 = metric("M48.proxy-v1", "revert_rate_proxy", (trailer + tree) as i64, observed as i64, ("reverted_by", json!({"trailer": trailer, "tree_restore": tree})));
    Ok((m47, m48))
}

fn has_table(db: &Connection) -> rusqlite::Result<bool> {
    db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='integration_outcomes')", [], |r| r.get(0))
}

/// Outcomes at `horizon_ms` by integration; `None` when unavailable.
type Outcome = Option<(String, i64, i64, (i64, i64))>;
fn outcomes(db: &Connection, horizon_ms: i64) -> rusqlite::Result<BTreeMap<String, Outcome>> {
    db.prepare("SELECT integrated_id,reverted,added_lines,surviving_lines,churn_added_lines,churn_deleted_lines FROM integration_outcomes WHERE horizon_ms=?1")?
        .query_map([horizon_ms], |r| {
            let reverted: Option<String> = r.get(1)?;
            let counts = |k| r.get::<_, Option<i64>>(k).map(Option::unwrap_or_default);
            Ok((r.get(0)?, match reverted { Some(reverted) => Some((reverted, counts(2)?, counts(3)?, (counts(4)?, counts(5)?))), None => None }))
        })?.collect()
}

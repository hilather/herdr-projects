//! Candidate groups (TM3.8, contracts-quality.md §3-4): `quality groups ...`.
//! `create` and `select` (operator, rule, judge) write canonical rows through
//! the store's own transactions (`SqliteStore`); `show`, `present` and
//! `report` (M41, M42) read `state.db` (and `show` the sidecar) strictly
//! read-only. Arm cost is each arm attempt's own usage from
//! `telemetry attempts`; nothing is reassigned between arms.
use anyhow::Result;
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use std::path::Path;

use super::registry;
use crate::store::{SelectionChoice, SqliteStore, arm_outcome};

/// Principal recorded for groups and selections made on this CLI.
const OPERATOR: &str = "operator:cli";

/// `herdr-projects telemetry <slug> quality groups ...`
#[derive(clap::Subcommand)]
pub enum Command {
    /// Seal a candidate group for a task: one arm per retained native profile,
    /// in launch order. Each later reservation of the task on the next arm's
    /// configuration becomes that arm; arms cannot change once sealed.
    Create {
        task: String,
        /// Retained native profile name, once per arm, in launch order (2-8).
        #[arg(long = "arm", required = true)]
        arms: Vec<String>,
    },
    /// Record the group's one selection: an arm's candidate, `--none`, the
    /// deterministic `--rule`, or a `--judge`'s choice of a presented
    /// `--submission`. Selection is not verification and moves no cost.
    Select {
        group: String,
        #[arg(long, conflicts_with_all = ["none", "rule", "judge"])]
        arm: Option<u32>,
        /// With `--arm`: a submission of the arm's attempt (default its first
        /// candidate). With `--judge`: the presented candidate chosen.
        #[arg(long)]
        submission: Option<String>,
        /// Close the group with no winner.
        #[arg(long, conflicts_with_all = ["rule", "judge"])]
        none: bool,
        /// Rule `first_accepted_in_launch_order.v1` (contracts-quality.md §4).
        #[arg(long, conflicts_with_all = ["judge", "submission", "reason"])]
        rule: bool,
        /// Judge name, recorded as principal `judge:<name>`; requires `--submission`.
        #[arg(long, requires = "submission", conflicts_with = "reason")]
        judge: Option<String>,
        /// With `--judge`: the judge's configuration ID (`sha256:...`), recorded in the evidence.
        #[arg(long, requires = "judge")]
        judge_configuration: Option<String>,
        /// Runner-up, best first, once per rank (evidence `rank` 2, 3, ...):
        /// an arm number with `--arm`, a presented submission with `--judge`.
        #[arg(long = "runner-up", conflicts_with_all = ["none", "rule"])]
        runner_up: Vec<String>,
        /// Reason code for `--arm` or `--none` (contracts-quality.md §3).
        #[arg(long)]
        reason: Option<String>,
    },
    /// An open group's candidates in blind presentation order for a judge:
    /// no arm, attempt or configuration. Read-only.
    Present { group: String },
    /// M41 candidate win rate and M42 paired acceptance difference over
    /// closed groups, as JSON. Read-only.
    Report {
        /// Window start (Unix ms), by the selection time.
        #[arg(long)]
        since: Option<i64>,
        /// Closed groups a cell needs before its value is shown, overriding
        /// the registry minimum (reported as `min_sample.source = override`).
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..=10_000))]
        min_groups: Option<u32>,
    },
    /// Groups with their arms, each arm's outcome and own cost, and the
    /// selection, as JSON. Read-only.
    Show,
}

pub fn run(project: &Path, command: Command) -> Result<Value> {
    let now = jiff::Timestamp::now().as_millisecond();
    let open = || SqliteStore::open(&project.join(".state/state.db"));
    Ok(match command {
        Command::Create { task, arms } => json!({"group": open()?.create_candidate_group(&task, &arms, OPERATOR, now)?}),
        Command::Select { group, arm, submission, none, rule, judge, judge_configuration, runner_up, reason } => {
            let mut store = open()?;
            let selection = if rule { store.select_candidate_by_rule(&group, now)? }
                else if let Some(judge) = judge {
                    store.select_candidate_by_judge(&group, submission.as_deref().unwrap_or_default(), &judge, judge_configuration.as_deref(), &runner_up, now)?
                }
                else {
                    let choice = match arm {
                        Some(arm) => {
                            let runner_up = runner_up.iter().map(|r| r.parse::<u32>().map_err(|_| anyhow::anyhow!("--runner-up {r} is not an arm number"))).collect::<Result<_>>()?;
                            SelectionChoice::Arm { arm, submission, runner_up }
                        }
                        None if none && submission.is_none() && runner_up.is_empty() => SelectionChoice::NoSelection,
                        None => anyhow::bail!("select needs --arm, --none, --rule or --judge with --submission"),
                    };
                    store.select_candidate(&group, &choice, reason.as_deref().unwrap_or("unspecified"), OPERATOR, now)?
                };
            json!({"selection": selection})
        }
        Command::Present { group } => present(project, &group)?,
        Command::Report { since, min_groups } => json!({"metrics": paired_metrics(project, since, min_groups)?, "since_unix_ms": since, "min_sample": registry::min_sample_json("M42", min_groups).1}),
        Command::Show => show(project)?,
    })
}

fn exists(db: &rusqlite::Connection, table: &str) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)", [table], |r| r.get(0))?)
}

/// Sum each numeric usage field over `usages`; `None` if any is not numeric.
fn sum(usages: &[&Value]) -> Option<Value> {
    let mut total = serde_json::Map::new();
    for usage in usages {
        for (key, value) in usage.as_object()? {
            let add = value.as_i64()?;
            let entry = total.entry(key.clone()).or_insert(json!(0));
            *entry = json!(entry.as_i64()?.checked_add(add)?);
        }
    }
    Some(Value::Object(total))
}

/// `(arm, configuration_id, profile_digest, bound attempt)`.
type ArmRow = (i64, String, String, Option<String>);
/// `(group_id, task_id, contract_revision, sealed_unix_ms, selection, arms)`.
type GroupRow = (String, String, Option<i64>, i64, Option<Value>, Vec<ArmRow>);

/// `{"groups": [...]}` in creation order. Each arm carries its attempt's
/// outcome and usage exactly as `telemetry attempts` reports them.
fn show(project: &Path) -> Result<Value> {
    let rows: Vec<GroupRow> = {
        let db = super::super::read_only(&project.join(".state/state.db"))?;
        crate::store::check_schema(&db)?;
        if !exists(&db, "candidate_groups")? { return Ok(json!({"groups": []})); }
        let db = db.unchecked_transaction()?;
        let groups: Vec<(String, String, Option<i64>, i64)> = db.prepare("SELECT group_id,task_id,contract_revision,sealed_unix_ms FROM candidate_groups WHERE sealed_unix_ms IS NOT NULL ORDER BY created_unix_ms,rowid")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
        let mut rows = Vec::with_capacity(groups.len());
        for (group, task, revision, sealed) in groups {
            let selection = db.query_row("SELECT outcome,arm,attempt_id,submission_id,selector_kind,selector_principal,reason,evidence,selected_unix_ms FROM candidate_selections WHERE group_id=?1", [&group],
                |r| Ok(json!({"outcome": r.get::<_, String>(0)?, "arm": r.get::<_, Option<i64>>(1)?, "attempt_id": r.get::<_, Option<String>>(2)?, "submission_id": r.get::<_, Option<String>>(3)?,
                    "selector_kind": r.get::<_, String>(4)?, "selector_principal": r.get::<_, String>(5)?, "reason": r.get::<_, String>(6)?,
                    "evidence": serde_json::from_str::<Value>(&r.get::<_, String>(7)?).unwrap_or(Value::Null), "selected_unix_ms": r.get::<_, i64>(8)?}))).optional()?;
            let arms = db.prepare("SELECT a.arm,a.configuration_id,a.profile_digest,b.attempt_id FROM candidate_group_arms a LEFT JOIN candidate_arm_attempts b ON b.group_id=a.group_id AND b.arm=a.arm WHERE a.group_id=?1 ORDER BY a.arm")?
                .query_map([&group], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
            rows.push((group, task, revision, sealed, selection, arms));
        }
        rows
    };
    let attempts = crate::telemetry::outcome::attempts(project)?;
    let outcome = |attempt: &str| attempts["attempts"].as_array().into_iter().flatten().find(|a| a["attempt_id"] == attempt).cloned();
    let mut groups = Vec::with_capacity(rows.len());
    for (group, task, revision, sealed, selection, arms) in rows {
        let winner = selection.as_ref().and_then(|s| s["arm"].as_i64());
        let mut records = Vec::with_capacity(arms.len());
        let (mut launched, mut unknown, mut premature) = (Vec::new(), Vec::new(), Vec::new());
        for (arm, configuration, profile, attempt) in arms {
            let record = attempt.as_deref().and_then(outcome);
            let role = match (winner, &selection) { (Some(w), _) if w == arm => "selected", (_, Some(_)) => "not_selected", _ => "open" };
            let entry = match (&attempt, &record) {
                (Some(attempt), Some(r)) => {
                    let candidate = r["result"]["state"] == "submitted";
                    if r["usage"]["status"] == "unavailable" { unknown.push(json!({"arm": arm, "reason": r["usage"]["reason"]})); } else { launched.push(r["usage"].clone()); }
                    if r["integration"]["state"] == "integrated" && role != "selected" { premature.push(json!({"arm": arm, "attempt_id": attempt})); }
                    json!({"arm": arm, "configuration_id": configuration, "profile_digest": profile, "attempt_id": attempt, "role": role,
                        "candidate": if candidate { r["result"]["submission_id"].clone() } else { Value::Null },
                        // Contracts §3: an arm without a candidate counts as a failure, not missing data.
                        "outcome": if candidate { "candidate" } else if r["terminal_state"] == "open" { "running" } else { "failure_no_candidate" },
                        "terminal_state": r["terminal_state"], "verification": r["verification"], "integration": r["integration"], "accepted": r["accepted"], "usage": r["usage"]})
                }
                _ => json!({"arm": arm, "configuration_id": configuration, "profile_digest": profile, "attempt_id": attempt, "role": role, "candidate": null,
                    "outcome": if selection.is_some() { "failure_no_candidate" } else { "not_launched" }, "usage": null}),
            };
            records.push(entry);
        }
        let not_launched = records.iter().filter(|r| r["attempt_id"].is_null()).count();
        let arms_total = if unknown.is_empty() { sum(&launched.iter().collect::<Vec<_>>()).unwrap_or_else(|| json!({"status": "unavailable", "reason": "usage_not_numeric"})) }
            else { json!({"status": "unavailable", "reason": "arm_usage_unavailable", "arms": unknown}) };
        let winner_usage = winner.and_then(|w| records.iter().find(|r| r["arm"] == w)).map_or(Value::Null, |r| r["usage"].clone());
        groups.push(json!({"group_id": group, "task_id": task, "contract_revision": revision, "sealed_unix_ms": sealed,
            "status": if selection.is_some() { "closed" } else { "open" }, "arms": records, "selection": selection,
            // Every launched arm's own usage; the winner's is a drill-down, never the group's cost.
            "cost": {"arms_total": arms_total, "arms_launched": launched.len() + unknown.len(), "arms_not_launched": not_launched, "winner_usage": winner_usage},
            // Integration is held until selection (contracts-quality.md §3); the list keeps
            // any arm integrated before the hold existed.
            "integration_hold": {"enforced": true, "integrated_without_selection": premature}}));
    }
    Ok(json!({"groups": groups}))
}

/// Same key as the store's judge selection (`candidate_presentation.v1`).
const PRESENTATION: &str = "candidate_presentation.v1";

/// `{"group_id", "candidates": [{position, submission_id, repository, base_oid,
/// candidate_oid}]}`: each bound arm's first candidate, ordered by
/// `sha256("candidate_presentation.v1:" + group + ":" + submission)`, the
/// order `select --judge` records. No arm, attempt or configuration.
fn present(project: &Path, group: &str) -> Result<Value> {
    let db = super::super::read_only(&project.join(".state/state.db"))?;
    crate::store::check_schema(&db)?;
    if !exists(&db, "candidate_groups")? { anyhow::bail!("no candidate group {group}"); }
    let db = db.unchecked_transaction()?;
    let closed: Option<bool> = db.query_row("SELECT EXISTS(SELECT 1 FROM candidate_selections s WHERE s.group_id=g.group_id) FROM candidate_groups g WHERE g.group_id=?1 AND g.sealed_unix_ms IS NOT NULL",
        [group], |r| r.get(0)).optional()?;
    match closed { None => anyhow::bail!("no candidate group {group}"), Some(true) => anyhow::bail!("candidate group {group} already has a selection"), Some(false) => {} }
    let attempts: Vec<String> = db.prepare("SELECT attempt_id FROM candidate_arm_attempts WHERE group_id=?1 ORDER BY arm")?
        .query_map([group], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    let mut shown = Vec::new();
    for attempt in attempts {
        let Some(submission) = arm_outcome(&db, &attempt)?.first else { continue };
        let key = format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(format!("{PRESENTATION}:{group}:{submission}").as_bytes()));
        let (repository, base, candidate): (String, String, String) = db.query_row("SELECT repository,base_oid,candidate_oid FROM result_submissions WHERE submission_id=?1", [&submission],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        shown.push((key, json!({"submission_id": submission, "repository": repository, "base_oid": base, "candidate_oid": candidate})));
    }
    shown.sort_by(|a, b| a.0.cmp(&b.0));
    let candidates: Vec<Value> = shown.into_iter().enumerate().map(|(i, (_, mut c))| { c["position"] = json!(i + 1); c }).collect();
    Ok(json!({"group_id": group, "presentation": PRESENTATION, "candidates": candidates}))
}

/// One closed group: its task, selector kind, the selected configuration (if
/// any) and each sealed arm's configuration with its verified outcome now
/// (`not_launched` for an arm never bound).
struct Closed { task: String, kind: String, selected: Option<String>, arms: Vec<(String, &'static str)> }

/// `(closed groups selected at or after since, open groups)`.
fn closed_groups(project: &Path, since: Option<i64>) -> Result<Option<(Vec<Closed>, usize)>> {
    let db = super::super::read_only(&project.join(".state/state.db"))?;
    crate::store::check_schema(&db)?;
    if !exists(&db, "candidate_groups")? { return Ok(None); }
    let db = db.unchecked_transaction()?;
    let open: i64 = db.query_row("SELECT count(*) FROM candidate_groups g WHERE g.sealed_unix_ms IS NOT NULL AND NOT EXISTS(SELECT 1 FROM candidate_selections s WHERE s.group_id=g.group_id)", [], |r| r.get(0))?;
    let rows: Vec<(String, String, String, Option<String>)> = db.prepare("SELECT s.group_id,g.task_id,s.selector_kind,a.configuration_id FROM candidate_selections s JOIN candidate_groups g ON g.group_id=s.group_id
        LEFT JOIN candidate_group_arms a ON a.group_id=s.group_id AND a.arm=s.arm WHERE ?1 IS NULL OR s.selected_unix_ms>=?1 ORDER BY g.created_unix_ms,g.rowid")?
        .query_map([since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut closed = Vec::with_capacity(rows.len());
    for (group, task, kind, selected) in rows {
        let arms: Vec<(String, Option<String>)> = db.prepare("SELECT a.configuration_id,b.attempt_id FROM candidate_group_arms a LEFT JOIN candidate_arm_attempts b ON b.group_id=a.group_id AND b.arm=a.arm WHERE a.group_id=?1 ORDER BY a.arm")?
            .query_map([&group], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        let arms = arms.into_iter().map(|(configuration, attempt)| Ok((configuration, match attempt { Some(a) => arm_outcome(&db, &a)?.outcome, None => "not_launched" })))
            .collect::<Result<_>>()?;
        closed.push(Closed { task, kind, selected, arms });
    }
    Ok(Some((closed, open as usize)))
}

fn unavailable(reason: &str) -> Value { json!({"status": "unavailable", "reason": reason}) }

/// `sha256:` plus the first 12 hex digits, for summary lines.
fn short(configuration: &str) -> &str { configuration.get(..19).unwrap_or(configuration) }

/// M41 over `groups` for one selector dimension: per configuration and per
/// ordered pair of configurations.
fn win_rates(groups: &[&Closed], min: usize) -> Value {
    let configurations: std::collections::BTreeSet<&str> = groups.iter().flat_map(|g| g.arms.iter().map(|(c, _)| c.as_str())).collect();
    let contains = |g: &Closed, c: &str| g.arms.iter().any(|(a, _)| a == c);
    let cells: Vec<Value> = configurations.iter().map(|&c| {
        let with: Vec<&&Closed> = groups.iter().filter(|g| contains(g, c)).collect();
        let selected = with.iter().filter(|g| g.selected.as_deref() == Some(c)).count();
        let none = with.iter().filter(|g| g.selected.is_none()).count();
        json!({"configuration_id": c, "groups": with.len(), "selected": selected, "no_selection": none, "other_selected": with.len() - selected - none,
            "value": if with.len() < min { unavailable("insufficient_data") } else { json!(format!("{selected}/{}", with.len())) }})
    }).collect();
    let mut pairs = Vec::new();
    for &a in &configurations {
        for &b in configurations.iter().filter(|&&b| b != a) {
            let both: Vec<&&Closed> = groups.iter().filter(|g| contains(g, a) && contains(g, b)).collect();
            if both.is_empty() { continue; }
            let wins = both.iter().filter(|g| g.selected.as_deref() == Some(a)).count();
            let losses = both.iter().filter(|g| g.selected.as_deref() == Some(b)).count();
            let none = both.iter().filter(|g| g.selected.is_none()).count();
            let value = if both.len() < min { unavailable("insufficient_data") } else if wins + losses == 0 { unavailable("no_decisive_groups") } else { json!(format!("{wins}/{}", wins + losses)) };
            pairs.push(json!({"a": a, "b": b, "groups": both.len(), "wins": wins, "losses": losses,
                "ties": {"no_selection": none, "other_selected": both.len() - wins - losses - none}, "value": value}));
        }
    }
    json!({"closed_groups": groups.len(), "configurations": cells, "head_to_head": pairs})
}

/// M41 and M42 (plan doc 07 §5b), definitions `M41.v1` and `M42.v1`.
/// `min_groups` overrides the registry minimum of both.
pub fn paired_metrics(project: &Path, since: Option<i64>, min_groups: Option<u32>) -> Result<std::collections::BTreeMap<String, Value>> {
    let ((min41, sample41), (min42, sample42)) = (registry::min_sample_json("M41", min_groups), registry::min_sample_json("M42", min_groups));
    let (min, min42) = (min41 as usize, min42 as usize);
    let base = |definition: &str, name: &str, sample: Value| json!({"definition": definition, "name": name, "min_sample": sample});
    let (mut m41, mut m42) = (base("M41.v1", "candidate_win_rate", sample41), base("M42.v1", "paired_acceptance_difference", sample42));
    let Some((closed, open)) = closed_groups(project, since)? else {
        m41["value"] = unavailable("candidate_groups_absent");
        m42["value"] = unavailable("candidate_groups_absent");
        return Ok([("M41".to_owned(), m41), ("M42".to_owned(), m42)].into());
    };
    for m in [&mut m41, &mut m42] { m["closed_groups"] = json!(closed.len()); m["open_groups"] = json!(open); }

    // M41: selections, with the selector kind as a dimension.
    let all: Vec<&Closed> = closed.iter().collect();
    let mut by_selector = serde_json::Map::new();
    by_selector.insert("all".into(), win_rates(&all, min));
    for kind in ["operator", "rule", "judge"] {
        by_selector.insert(kind.into(), win_rates(&closed.iter().filter(|g| g.kind == kind).collect::<Vec<_>>(), min));
    }
    let shown: Vec<String> = by_selector["all"]["configurations"].as_array().into_iter().flatten().filter_map(|c| c["value"].as_str().map(|v| format!("{}={v}", short(c["configuration_id"].as_str().unwrap_or_default())))).collect();
    m41["value"] = if closed.is_empty() { unavailable("no_closed_groups") } else if shown.is_empty() { unavailable("insufficient_data") } else { json!(shown.join(" ")) };
    m41["by_selector"] = Value::Object(by_selector);

    // M42: verified acceptance (arm_outcome.v1), not selection; pending arms exclude their group.
    let configurations: std::collections::BTreeSet<&str> = closed.iter().flat_map(|g| g.arms.iter().map(|(c, _)| c.as_str())).collect();
    let outcome = |g: &Closed, c: &str| g.arms.iter().find(|(a, _)| a == c).map(|(_, o)| *o);
    let mut pairs = Vec::new();
    let mut summary = Vec::new();
    for &a in &configurations {
        for &b in configurations.iter().filter(|&&b| b != a) {
            let (mut groups, mut pending, mut counts) = (0usize, 0usize, [0usize; 4]);
            // Per task: (sum of a - b acceptance, paired groups), the bootstrap's clusters.
            let mut clusters = std::collections::BTreeMap::<&str, (i64, i64)>::new();
            for g in &closed {
                let (Some(x), Some(y)) = (outcome(g, a), outcome(g, b)) else { continue };
                groups += 1;
                if x == "pending" || y == "pending" { pending += 1; continue; }
                counts[usize::from(x == "accepted") * 2 + usize::from(y == "accepted")] += 1;
                let cluster = clusters.entry(g.task.as_str()).or_default();
                cluster.0 += i64::from(x == "accepted") - i64::from(y == "accepted");
                cluster.1 += 1;
            }
            if groups == 0 { continue; }
            let [neither, b_only, a_only, both] = counts;
            let n = groups - pending;
            let value = if n < min42 { unavailable("insufficient_data") } else {
                json!(((a_only as f64 - b_only as f64) * 100.0 / n as f64 * 100.0).round() / 100.0)
            };
            if a < b && value.is_number() { summary.push(format!("{}-{}={}pp", short(a), short(b), value)); }
            pairs.push(json!({"a": a, "b": b, "groups": groups, "pending": pending, "n": n, "both_accepted": both, "a_only": a_only, "b_only": b_only, "neither": neither,
                "difference": format!("{}/{n}", a_only as i64 - b_only as i64), "value": value, "unit": "percentage_points",
                "uncertainty": if n < min42 { unavailable("insufficient_data") } else { bootstrap(&clusters.into_values().collect::<Vec<_>>()) }}));
        }
    }
    m42["acceptance"] = json!("arm_outcome.v1 verified acceptance, not selection");
    m42["estimator"] = json!({"method": registry::M42_BOOTSTRAP.method, "resample": "task", "task_family": unavailable("no_task_family_data")});
    m42["value"] = if closed.is_empty() { unavailable("no_closed_groups") } else if summary.is_empty() { unavailable("insufficient_data") } else { json!(summary.join(" ")) };
    m42["pairs"] = json!(pairs);
    Ok([("M41".to_owned(), m41), ("M42".to_owned(), m42)].into())
}

/// SplitMix64, the bootstrap's declared generator.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    /// Uniform in `0..k` by rejection (no modulo bias).
    fn below(&mut self, k: u64) -> u64 {
        let limit = u64::MAX - u64::MAX % k;
        loop { let x = self.next(); if x < limit { return x % k; } }
    }
}

/// `n/d` percentage points to two decimals, rounded half away from zero, in
/// integer arithmetic.
fn points(n: i64, d: i64) -> String {
    let hundredths = (2 * n.unsigned_abs() * 10_000 + d.unsigned_abs()) / (2 * d.unsigned_abs());
    format!("{}{}.{:02}", if n < 0 && hundredths > 0 { "-" } else { "" }, hundredths / 100, hundredths % 100)
}

/// M42 interval (`percentile_bootstrap.v1`, contracts-quality.md §4) over
/// `clusters`, one `(sum of a - b acceptance, paired groups)` per task in task
/// ID order: each iteration draws as many tasks with replacement (SplitMix64
/// from the registry seed, restarted per pair) and takes sum / count; the
/// iterations are sorted exactly (as fractions) and the nearest-rank
/// percentiles `ceil(B × 25/1000)` and `ceil(B × 975/1000)` are reported.
fn bootstrap(clusters: &[(i64, i64)]) -> Value {
    let spec = &registry::M42_BOOTSTRAP;
    if clusters.len() < 2 { return unavailable("single_task"); }
    let mut rng = SplitMix64(spec.seed);
    let k = clusters.len() as u64;
    let mut draws: Vec<(i64, i64)> = (0..spec.iterations).map(|_| (0..k).fold((0, 0), |(n, d), _| {
        let (x, y) = clusters[rng.below(k) as usize];
        (n + x, d + y)
    })).collect();
    draws.sort_by(|(n1, d1), (n2, d2)| (i128::from(*n1) * i128::from(*d2)).cmp(&(i128::from(*n2) * i128::from(*d1))).then(d1.cmp(d2)));
    let tail = u64::from(1000 - spec.level_permille) / 2;
    let rank = |permille: u64| (u64::from(spec.iterations) * permille).div_ceil(1000) as usize - 1;
    let (lower, upper) = (draws[rank(tail)], draws[rank(1000 - tail)]);
    json!({"method": spec.method, "resample": "task", "clusters": k, "iterations": spec.iterations, "seed": format!("{:#018x}", spec.seed),
        "level": format!("0.{}", spec.level_permille / 10), "lower": points(lower.0, lower.1), "upper": points(upper.0, upper.1),
        "lower_difference": format!("{}/{}", lower.0, lower.1), "upper_difference": format!("{}/{}", upper.0, upper.1), "unit": "percentage_points",
        "task_family": unavailable("no_task_family_data"), "source": registry::VERSION})
}

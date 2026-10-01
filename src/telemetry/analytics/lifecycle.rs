//! Native lifecycle definitions (`M01/M02/M06/M07.cohort-v1`,
//! contracts-analytics.md §3): fixed terminal and assignment cohorts over the
//! canonical store, read strictly read-only. Failed, cancelled and
//! succeeded-without-evidence tasks stay in `T`; open tasks are an explicit
//! exclusion; nothing unknown is placed in a window by guess. Replay
//! candidates (TM4.6) are evaluation artefacts measured by M49 only: every
//! lifecycle cohort excludes them as `replay_candidate`.
use super::registry::Cohort;
use anyhow::Result;
use rusqlite::Connection;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub struct Attempt { pub id: String, pub state: String, pub decided: Option<i64>, pub reserved: Option<i64>, pub ended: Option<i64>, pub kind: Option<String> }

pub struct FirstCandidate { pub at: i64, pub attempt: String, pub policy: String, pub policy_digest: String, pub outcome: &'static str }

pub struct Task {
    pub id: String,
    pub state: String,
    pub accepted: bool,
    pub accepted_at: Option<i64>,
    pub route: Option<String>,
    pub class: Option<String>,
    pub attempts: Vec<Attempt>,
    /// A replay candidate (`replay_candidates`): outside every lifecycle cohort.
    pub replay: bool,
    pub first_candidate: Option<FirstCandidate>,
}

impl Task {
    /// `accepted`, `succeeded_without_evidence`, `failed`, `cancelled` or `open` (contracts §6 `T`/`A`).
    pub fn disposition(&self) -> &'static str {
        if self.accepted { return "accepted"; }
        match self.state.as_str() { "succeeded" => "succeeded_without_evidence", "failed" => "failed", "cancelled" => "cancelled", _ => "open" }
    }
    /// Acceptance: when the evidence completed. Otherwise the latest terminal
    /// lifecycle mark of its attempts (the canonical store keeps no task
    /// terminal time); `None` when no attempt has one.
    pub fn terminal_at(&self) -> Option<i64> {
        match self.disposition() {
            "open" => None,
            "accepted" => self.accepted_at,
            _ => self.attempts.iter().filter_map(|a| a.ended).max(),
        }
    }
    /// First assignment: the earliest dispatch decision, else reservation mark.
    pub fn assigned_at(&self) -> Option<i64> { self.attempts.iter().filter_map(|a| a.decided.or(a.reserved)).min() }
    pub fn dimension(&self, name: &str) -> String {
        match name {
            "policy" => self.first_candidate.as_ref().map(|f| format!("{}@{}", f.policy, f.policy_digest)).unwrap_or_else(|| "unknown".into()),
            "route" => self.route.clone().unwrap_or_else(|| "none".into()),
            "task_class" => self.class.clone().unwrap_or_else(|| "unclassified".into()),
            "agent_kind" => {
                let kinds: BTreeSet<&str> = self.attempts.iter().map(|a| a.kind.as_deref().unwrap_or("unknown")).collect();
                match kinds.len() { 0 => "unassigned".into(), 1 => kinds.into_iter().next().unwrap_or_default().to_owned(), _ => "mixed".into() }
            }
            _ => "unknown".into(),
        }
    }
    fn attrs(&self) -> Value {
        json!({"disposition": self.disposition(), "terminal_unix_ms": self.terminal_at(), "assigned_unix_ms": self.assigned_at(), "attempts": self.attempts.len(),
            "route": self.dimension("route"), "agent_kind": self.dimension("agent_kind"), "task_class": self.dimension("task_class")})
    }
}

fn table(db: &Connection, name: &str) -> rusqlite::Result<bool> {
    db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)", [name], |r| r.get(0))
}

/// The hot canonical reads, also checked by `analytics plans`: `(name, sql)`.
/// Each is one pass over its driving table; correlated lookups use primary keys or indexes.
pub fn queries(db: &Connection) -> Result<Vec<(&'static str, String)>> {
    let decided = if table(db, "dispatch_decisions")? { "(SELECT decided_unix_ms FROM dispatch_decisions d WHERE d.attempt_id=a.id)" } else { "NULL" };
    let (reserved, ended) = if table(db, "attempt_lifecycle")? {
        ("(SELECT unix_ms FROM attempt_lifecycle l WHERE l.attempt_id=a.id AND l.state='reserved')",
         "(SELECT max(unix_ms) FROM attempt_lifecycle l WHERE l.attempt_id=a.id AND l.state IN ('completed','failed','cancelled','lost'))")
    } else { ("NULL", "NULL") };
    let mut out = vec![
        ("lifecycle_attempts", format!("SELECT a.id,a.task_id,a.state,{decided},{reserved},{ended},json_extract(i.payload,'$.inputs.effective_profile.kind')
            FROM attempts a LEFT JOIN attempt_inputs i ON i.attempt_id=a.id ORDER BY a.task_id,a.id")),
        ("lifecycle_contracts", "SELECT c.task_id,c.route FROM task_contracts c WHERE c.contract_revision=(SELECT max(contract_revision) FROM task_contracts x WHERE x.task_id=c.task_id)".to_owned()),
        ("lifecycle_acceptance_times", "SELECT c.task_id,min(r.created_unix_ms),min(k.created_unix_ms) FROM task_contracts c
            JOIN result_submissions s ON s.task_id=c.task_id AND s.contract_revision=c.contract_revision JOIN verified_results r ON r.submission_id=s.submission_id
            LEFT JOIN integration_operations i ON i.verified_result_id=r.result_id LEFT JOIN integrated_commits k ON k.operation_id=i.operation_id
            WHERE c.contract_revision=(SELECT max(contract_revision) FROM task_contracts x WHERE x.task_id=c.task_id) GROUP BY c.task_id".to_owned()),
    ];
    if table(db, "replay_candidates")? {
        out.push(("lifecycle_replay_candidates", "SELECT task_id FROM replay_candidates".to_owned()));
    }
    if table(db, "task_classifications")? {
        out.push(("lifecycle_classes", "SELECT task_id,class FROM task_classifications ORDER BY task_id,created_unix_ms,revision".to_owned()));
    }
    out.extend([("first_candidate_submissions", FIRST_SUBMISSIONS.to_owned()),
        ("first_candidate_policies", FIRST_POLICIES.to_owned()),
        ("first_candidate_verdicts", FIRST_VERDICTS.to_owned())]);
    Ok(out)
}

/// Every task with its attempts and evidence, sorted by task id.
pub fn load(project: &Path) -> Result<Vec<Task>> {
    let db = crate::telemetry::read_only(&project.join(".state/state.db"))?;
    let evidence = crate::telemetry::metrics::task_evidence(&db)?;
    let queries = queries(&db)?;
    let sql = |name: &str| queries.iter().find(|q| q.0 == name).map(|q| q.1.clone());
    let mut attempts: BTreeMap<String, Vec<Attempt>> = BTreeMap::new();
    let mut stmt = db.prepare(&sql("lifecycle_attempts").unwrap_or_default())?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(1)?, Attempt { id: r.get(0)?, state: r.get(2)?, decided: r.get(3)?, reserved: r.get(4)?, ended: r.get(5)?, kind: r.get(6)? })))?;
    for row in rows { let (task, attempt) = row?; attempts.entry(task).or_default().push(attempt); }
    let routes: BTreeMap<String, String> = db.prepare(&sql("lifecycle_contracts").unwrap_or_default())?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    let times: BTreeMap<String, (Option<i64>, Option<i64>)> = db.prepare(&sql("lifecycle_acceptance_times").unwrap_or_default())?
        .query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?.collect::<rusqlite::Result<_>>()?;
    let mut classes = BTreeMap::new();
    if let Some(sql) = sql("lifecycle_classes") {
        for row in db.prepare(&sql)?.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? { let (task, class) = row?; classes.insert(task, class); }
    }
    let replay: BTreeSet<String> = match sql("lifecycle_replay_candidates") {
        Some(sql) => db.prepare(&sql)?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?,
        None => BTreeSet::new(),
    };
    Ok(evidence.into_iter().map(|(id, state, accepted)| {
        let route = routes.get(&id).cloned();
        let accepted_at = accepted.then(|| times.get(&id).and_then(|(verified, integrated)| if route.as_deref() == Some("verify_only") { *verified } else { *integrated })).flatten();
        Task { first_candidate: None, attempts: attempts.remove(&id).unwrap_or_default(), class: classes.get(&id).cloned(), route, accepted, accepted_at, replay: replay.contains(&id), state, id }
    }).collect())
}

/// First submissions are ordered across all attempts and contract revisions.
/// Walk attempts once and use the canonical attempt index for their submissions.
const FIRST_SUBMISSIONS: &str = "SELECT a.task_id,s.submission_id,s.attempt_id,s.contract_revision,s.created_unix_ms
    FROM attempts a CROSS JOIN result_submissions s WHERE s.attempt_id=a.id
    ORDER BY a.task_id,s.created_unix_ms,s.submission_id";
const FIRST_POLICIES: &str = "SELECT policy_id,body FROM acceptance_policies
    WHERE task_id=?1 AND contract_revision=?2 ORDER BY policy_id";
const FIRST_VERDICTS: &str = "SELECT v.policy_id,v.policy_digest,v.state,
    EXISTS(SELECT 1 FROM verified_results r WHERE r.submission_id=v.submission_id
        AND r.run_id=v.run_id AND r.policy_digest=v.policy_digest)
    FROM verification_runs v WHERE v.submission_id=?1";

/// Only M30 reads candidate/policy/verdict history. Prepare each lookup once;
/// retries belonging to later submissions cannot contribute to the first.
fn first_candidates(db: &Connection) -> Result<BTreeMap<String, FirstCandidate>> {
    let mut firsts = BTreeMap::new();
    let mut submissions = db.prepare(FIRST_SUBMISSIONS)?;
    let mut policies_stmt = db.prepare(FIRST_POLICIES)?;
    let mut verdicts_stmt = db.prepare(FIRST_VERDICTS)?;
    let rows = submissions.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?)))?;
    for row in rows {
        let (task, submission, attempt, revision, at) = row?;
        if firsts.contains_key(&task) { continue; }
        let policies: Vec<(String, String)> = policies_stmt.query_map(rusqlite::params![task, revision], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        let mut verdicts = BTreeMap::<(String, String), (bool, bool)>::new();
        for row in verdicts_stmt.query_map([&submission], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, bool>(3)?)))? {
            let (policy, digest, state, receipt) = row?;
            let verdict = verdicts.entry((policy, digest)).or_default();
            verdict.0 |= state == "accepted" && receipt;
            verdict.1 |= state == "rejected";
        }
        let (mut passed, mut rejected) = (0usize, false);
        for (policy, body) in &policies {
            let digest = format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(body.as_bytes()));
            let (accepted, failed) = verdicts.get(&(policy.clone(), digest)).copied().unwrap_or_default();
            if accepted { passed += 1; } else if failed { rejected = true; }
        }
        let outcome = if policies.is_empty() { "policy_unknown" } else if passed == policies.len() { "accepted" } else if rejected { "rejected" } else { "pending" };
        firsts.insert(task, FirstCandidate { at, attempt, policy: policies.iter().map(|p| p.0.as_str()).collect::<Vec<_>>().join(","), policy_digest: super::sha256(serde_json::to_string(&policies)?.as_bytes()), outcome });
    }
    Ok(firsts)
}

pub(crate) fn load_first_candidates(project: &Path, tasks: &mut [Task]) -> Result<()> {
    let db = crate::telemetry::read_only(&project.join(".state/state.db"))?;
    let mut firsts = first_candidates(&db)?;
    for task in tasks { task.first_candidate = firsts.remove(&task.id); }
    Ok(())
}

/// Report needs no rich lifecycle load: reuse central's task evidence and
/// replay set. Its M30 body then participates in P2's validated provider cache.
pub(crate) fn first_candidate_report(db: &Connection, evidence: &[(String, String, bool)], replay: &BTreeSet<String>, since: Option<i64>) -> Result<Value> {
    let mut firsts = first_candidates(db)?;
    let tasks: Vec<Task> = evidence.iter().map(|(id, state, accepted)| Task {
        id: id.clone(), state: state.clone(), accepted: *accepted, accepted_at: None,
        route: None, class: None, attempts: Vec::new(), replay: replay.contains(id), first_candidate: firsts.remove(id),
    }).collect();
    let (mut body, _) = evaluate(&tasks, &Request { metric: "M30", cohort: Cohort::Activity, from: since, to: None, horizon: None, by: None });
    body["definition"] = json!("M30.submission-v1");
    body["name"] = json!("first_candidate_verification_rate");
    Ok(body)
}

/// Canonical input digest of the extracted rows: the lifecycle source watermark.
pub fn digest(tasks: &[Task]) -> String {
    // The replay mark is appended only when set, so a project without replay keeps its digest.
    let rows: Vec<Value> = tasks.iter().map(|t| {
        let mut row = json!([t.id, t.state, t.accepted, t.accepted_at, t.route, t.class,
            t.attempts.iter().map(|a| json!([a.id, a.state, a.decided, a.reserved, a.ended, a.kind])).collect::<Vec<_>>()]);
        if t.replay && let Value::Array(row) = &mut row { row.push(json!("replay_candidate")); }
        row
    }).collect();
    super::sha256(serde_json::to_string(&rows).unwrap_or_default().as_bytes())
}

/// Latest event time among the extracted rows (occurrence time, not observation).
pub fn last_event(tasks: &[Task]) -> Option<i64> {
    tasks.iter().flat_map(|t| t.attempts.iter().flat_map(|a| [a.decided, a.reserved, a.ended]).chain([t.accepted_at])).flatten().max()
}

/// One lineage row: `(entity_kind, entity_id, attrs)`.
pub type Row = (&'static str, String, Value);
pub type Lineage = BTreeMap<String, Vec<Row>>;

pub struct Request<'a> { pub metric: &'a str, pub cohort: Cohort, pub from: Option<i64>, pub to: Option<i64>, pub horizon: Option<i64>, pub by: Option<&'a str> }

struct Member<'a> { task: &'a Task, outcome: &'static str }

/// Cohort membership: members with their outcome (at the horizon for the
/// assignment cohort) and exclusions by reason.
fn cohort<'a>(tasks: &'a [Task], r: &Request) -> (Vec<Member<'a>>, BTreeMap<&'static str, Vec<&'a Task>>, Option<i64>) {
    let bounded = r.from.is_some() || r.to.is_some();
    let inside = |at: i64| r.from.is_none_or(|from| at >= from) && r.to.is_none_or(|to| at < to);
    let (mut members, mut excluded, mut cutoff) = (Vec::new(), BTreeMap::<&'static str, Vec<&Task>>::new(), None::<i64>);
    for task in tasks {
        // Evaluation artefacts: measured by M49, never in a lifecycle cohort.
        if task.replay { excluded.entry("replay_candidate").or_default().push(task); continue; }
        if r.metric == "M30" {
            let Some(first) = &task.first_candidate else { excluded.entry("no_submission").or_default().push(task); continue; };
            if !inside(first.at) { excluded.entry("outside_window").or_default().push(task); continue; }
            cutoff = cutoff.max(Some(first.at));
            if ["pending", "policy_unknown"].contains(&first.outcome) { excluded.entry(first.outcome).or_default().push(task); continue; }
            members.push(Member { task, outcome: first.outcome });
            continue;
        }
        let disposition = task.disposition();
        match r.cohort {
            Cohort::Terminal => {
                if disposition == "open" { excluded.entry("open").or_default().push(task); continue; }
                match (bounded, task.terminal_at()) {
                    (true, None) => { excluded.entry("terminal_time_unknown").or_default().push(task); continue; }
                    (true, Some(at)) if !inside(at) => { excluded.entry("outside_window").or_default().push(task); continue; }
                    _ => {}
                }
                cutoff = cutoff.max(task.terminal_at());
                members.push(Member { task, outcome: disposition });
            }
            Cohort::Assignment | Cohort::Activity => {
                if task.attempts.is_empty() { excluded.entry("not_assigned").or_default().push(task); continue; }
                let assigned = task.assigned_at();
                match (bounded, assigned) {
                    (true, None) => { excluded.entry("assignment_time_unknown").or_default().push(task); continue; }
                    (true, Some(at)) if !inside(at) => { excluded.entry("outside_window").or_default().push(task); continue; }
                    _ => {}
                }
                cutoff = cutoff.max(assigned);
                let outcome = match (disposition, r.horizon) {
                    ("open", _) => "unfinished",
                    (_, None) => disposition,
                    (_, Some(horizon)) => match (assigned, task.terminal_at()) {
                        (Some(start), Some(end)) if end <= start.saturating_add(horizon) => disposition,
                        _ => "unfinished",
                    },
                };
                members.push(Member { task, outcome });
            }
        }
    }
    (members, excluded, cutoff)
}

fn ratio(numerator: usize, denominator: usize) -> (Value, Option<&'static str>) {
    if denominator == 0 { (Value::Null, Some("empty_denominator")) } else { (json!(format!("{numerator}/{denominator}")), None) }
}

/// `(numerator, denominator, value, reason, extra)` of one metric over one member set.
fn compute(metric: &str, members: &[&Member]) -> (Value, Value, Value, Option<&'static str>, Value) {
    let accepted = members.iter().filter(|m| m.outcome == "accepted").count();
    let mut breakdown = BTreeMap::<&str, usize>::new();
    for m in members { *breakdown.entry(m.outcome).or_default() += 1; }
    match metric {
        "M01" => (json!(accepted), Value::Null, json!(accepted), None, json!({"breakdown": breakdown})),
        "M02" | "M30" => { let (value, reason) = ratio(accepted, members.len()); (json!(accepted), json!(members.len()), value, reason, json!({"breakdown": breakdown})) }
        "M07" => {
            let attempts: Vec<&Attempt> = members.iter().flat_map(|m| &m.task.attempts).collect();
            let (value, reason) = ratio(attempts.len(), accepted);
            let unknown = attempts.iter().filter(|a| !["completed", "failed", "cancelled"].contains(&a.state.as_str())).count();
            (json!(attempts.len()), json!(accepted), value, reason, json!({"attempts_without_decision": attempts.iter().filter(|a| a.decided.is_none()).count(),
                "unknown_launch_outcome": unknown, "breakdown": breakdown}))
        }
        _ => {
            // M06: nearest-rank p95 (doc 07 §6) of acceptance − first admission over accepted tasks with both times.
            let mut samples: Vec<i64> = members.iter().filter(|m| m.outcome == "accepted")
                .filter_map(|m| Some(m.task.accepted_at? - m.task.assigned_at()?)).collect();
            samples.sort_unstable();
            let missing = accepted - samples.len();
            let value = if samples.is_empty() { Value::Null } else { json!(samples[(samples.len() * 95).div_ceil(100) - 1]) };
            let reason = samples.is_empty().then_some("no_samples");
            (json!(samples.len()), Value::Null, value, reason, json!({"samples": samples.len(), "missing_times": missing, "breakdown": breakdown,
                "censoring": "completed_case_descriptive: failed and open tasks are counted, never given a lead time"}))
        }
    }
}

/// Evaluate one native metric: the body (without projection fields) and its drill-down lineage.
pub fn evaluate(tasks: &[Task], r: &Request) -> (Value, Lineage) {
    let (members, excluded, cutoff) = cohort(tasks, r);
    let all: Vec<&Member> = members.iter().collect();
    let (numerator, denominator, value, reason, extra) = compute(r.metric, &all);
    let mut body = json!({"numerator": numerator, "denominator": denominator, "value": value, "reason": reason,
        "exclusions": excluded.iter().map(|(k, v)| (k.to_string(), json!(v.len()))).collect::<serde_json::Map<_, _>>(), "event_cutoff_unix_ms": cutoff});
    if let (Value::Object(body), Value::Object(extra)) = (&mut body, extra) { body.extend(extra); }
    // Coverage (doc 07 §1): known vs expected placement of the eligible population.
    let unknown = excluded.get("terminal_time_unknown").or(excluded.get("assignment_time_unknown")).map_or(0, Vec::len);
    let (known, expected) = if r.metric == "M06" {
        (body["samples"].as_u64().unwrap_or(0) as usize, members.iter().filter(|m| m.outcome == "accepted").count())
    } else { (members.len(), members.len() + unknown) };
    let mut reasons = serde_json::Map::new();
    if unknown > 0 { reasons.insert(if r.cohort == Cohort::Terminal { "terminal_time_unknown".into() } else { "assignment_time_unknown".into() }, json!(unknown)); }
    if r.metric == "M06" && known < expected { reasons.insert("acceptance_or_admission_time_unknown".into(), json!(expected - known)); }
    body["coverage"] = json!({"state": if known == expected { "complete" } else { "partial" }, "known": known, "expected": expected, "missing": expected - known, "reasons": reasons});
    if r.metric == "M30" {
        let pending = excluded.get("pending").map_or(0, Vec::len);
        let unknown = excluded.get("policy_unknown").map_or(0, Vec::len);
        body["pending"] = json!(pending);
        body["coverage"] = json!({"state": if pending + unknown == 0 { "complete" } else { "partial" },
            "known": members.len(), "expected": members.len() + pending + unknown, "missing": pending + unknown,
            "reasons": {"pending": pending, "policy_unknown": unknown}});
    }
    if r.cohort == Cohort::Assignment {
        let unfinished = members.iter().filter(|m| m.outcome == "unfinished").count();
        body["censored"] = json!({"unfinished": unfinished});
        body["provisional"] = json!(unfinished > 0);
        body["horizon_ms"] = json!(r.horizon);
    }
    if let Some(dimension) = r.by {
        let mut cells: BTreeMap<String, Vec<&Member>> = BTreeMap::new();
        for m in &members { cells.entry(m.task.dimension(dimension)).or_default().push(m); }
        body["cells"] = cells.iter().map(|(label, subset)| {
            let (numerator, denominator, value, reason, _) = compute(r.metric, subset);
            json!({"dimension": {dimension: label}, "numerator": numerator, "denominator": denominator, "value": value, "reason": reason})
        }).collect();
    }
    let mut lineage = Lineage::new();
    let task_row = |t: &Task| ("task", t.id.clone(), t.attrs());
    let numerator: Vec<Row> = match r.metric {
        "M07" => members.iter().flat_map(|m| m.task.attempts.iter().map(|a| ("attempt", a.id.clone(),
            json!({"task_id": m.task.id, "state": a.state, "decided_unix_ms": a.decided})))).collect(),
        _ => members.iter().filter(|m| m.outcome == "accepted").map(|m| task_row(m.task)).collect(),
    };
    lineage.insert("numerator".into(), numerator);
    lineage.insert("denominator".into(), match r.metric {
        "M02" | "M30" => members.iter().map(|m| task_row(m.task)).collect(),
        "M07" => members.iter().filter(|m| m.outcome == "accepted").map(|m| task_row(m.task)).collect(),
        _ => Vec::new(),
    });
    for m in &members { lineage.entry(format!("outcome.{}", m.outcome)).or_default().push(task_row(m.task)); }
    for (reason, list) in &excluded { lineage.insert(format!("excluded.{reason}"), list.iter().map(|t| task_row(t)).collect()); }
    for rows in lineage.values_mut() { rows.sort_by(|a, b| a.1.cmp(&b.1)); }
    (body, lineage)
}

//! TM4.7 assignment policies (docs/telemetry/contracts-evaluation.md §9):
//! the policy inputs read from the query service (recorded outcomes per arm
//! and task class) and the accounting lane (quota headroom), the shadow
//! record in sidecar stream `policies`, and `telemetry <slug> policies ...`.
//! The policies themselves are pure (`crate::domain::PolicySpec`). Shadow and
//! suggestion never change a dispatch; assignment is admission's, behind the
//! operator switch and an owner-signed `randomized_assignment` grant.
use super::analytics::estimators as est;
use super::analytics::lifecycle;
use crate::domain::{ArmInput, PolicyEvaluation, PolicySpec, EPSILON, THOMPSON};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const STREAM: &str = "policies";
/// `include_str!` of `migrations/telemetry/policies/`, in order; index + 1 is the stream version.
pub const MIGRATIONS: &[&str] = &[include_str!("../../migrations/telemetry/policies/0001_policy_shadow.sql")];
pub const SHADOW_SCHEMA: &str = "assignment-policy-shadow.v1";
pub const SIMULATION_SCHEMA: &str = "assignment-policy-simulation.v1";
pub const SUGGESTION_SCHEMA: &str = "assignment-policy-suggestion.v1";

/// No report keys: shadow disagreement is its own report.
pub fn metrics(_project: &Path, _since: Option<i64>) -> Result<BTreeMap<String, Value>> { Ok(BTreeMap::new()) }
/// Shadow rows are written by admission; nothing to do on a tick.
pub fn tick(_project: &Path, _budget: super::codex::Budget) -> Result<()> { Ok(()) }

/// Whether any of `specs` reads recorded outcomes.
pub fn needs_outcomes(specs: &[PolicySpec]) -> bool { specs.iter().any(|s| s.policy == EPSILON || s.policy == THOMPSON) }

/// Terminal task outcomes per `(task class, configuration)` from the query
/// service's lifecycle rows: a task counts for the one configuration all its
/// attempts were dispatched on (mixed or unknown arms are skipped); accepted
/// is a success, every other terminal disposition a failure; open tasks do not count.
#[derive(Debug, Default)]
pub struct Evidence { outcomes: BTreeMap<(String, String), (u64, u64)> }

impl Evidence {
    pub fn load(project: &Path) -> Result<Self> {
        let tasks = lifecycle::load(project)?;
        let db = super::read_only(&project.join(".state/state.db"))?;
        let mut arms = BTreeMap::new();
        let present: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='dispatch_decisions')", [], |r| r.get(0))?;
        if present {
            for row in db.prepare("SELECT attempt_id,chosen_configuration_id FROM dispatch_decisions")?.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
                let (attempt, configuration) = row?;
                arms.insert(attempt, configuration);
            }
        }
        let mut outcomes = BTreeMap::new();
        for task in &tasks {
            let disposition = task.disposition();
            if disposition == "open" || task.attempts.is_empty() { continue; }
            let chosen: Option<Vec<&String>> = task.attempts.iter().map(|a| arms.get(&a.id)).collect();
            let Some(chosen) = chosen else { continue };
            if chosen.iter().any(|c| *c != chosen[0]) { continue; }
            let entry: &mut (u64, u64) = outcomes.entry((task.class.clone().unwrap_or_else(|| "unclassified".into()), chosen[0].clone())).or_default();
            if disposition == "accepted" { entry.0 += 1 } else { entry.1 += 1 }
        }
        Ok(Self { outcomes })
    }
    /// `(successes, failures)` of `configuration` in `class`.
    pub fn posterior(&self, class: &str, configuration: &str) -> (u64, u64) {
        self.outcomes.get(&(class.to_owned(), configuration.to_owned())).copied().unwrap_or_default()
    }
}

/// Exact thousandths of a decimal percent string (`"62.5"` → 62500).
fn percent_milli(text: &str) -> Option<i64> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    if fraction.len() > 3 || whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) || !fraction.bytes().all(|b| b.is_ascii_digit()) { return None; }
    let padded = format!("{fraction:0<3}");
    Some(whole.parse::<i64>().ok()? * 1000 + padded.parse::<i64>().ok()?)
}

/// The smallest fresh remaining quota (percent, thousandths) of the
/// profile's execution home at `now` (contracts-accounting.md §5 M40);
/// `None` when unknown: another adapter, no home, no synced ledger, or no
/// fresh trusted window. Unknown never excludes an arm.
pub fn headroom(project: &Path, profile: &crate::domain::FrozenProfile, now: i64) -> Option<i64> {
    if profile.kind != "codex" { return None; }
    let home = profile.execution_home.as_deref()?;
    let path = super::sidecar::path(project);
    if !path.is_file() { return None; }
    let db = super::read_only(&path).ok()?;
    let synced: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='quota_window_observations')", [], |r| r.get(0)).ok()?;
    if !synced { return None; }
    let value = super::accounting::quota::headroom(&db, home, now).ok()?;
    value["windows"].as_array()?.iter().filter(|w| w["freshness"] == "fresh").filter_map(|w| percent_milli(w["value"].as_str()?)).min()
}

/// One policy's evaluation over the named arms, as reported everywhere.
pub fn evaluation_json(spec: &PolicySpec, evaluation: &PolicyEvaluation, arms: &[String]) -> Value {
    let excluded: BTreeMap<usize, &str> = evaluation.excluded.iter().copied().collect();
    json!({"policy": spec.policy, "policy_digest": spec.digest(), "spec": serde_json::from_str::<Value>(&spec.canonical_json()).unwrap_or_default(),
        "seed": evaluation.seed_hex(), "draw_ppm": evaluation.draw_ppm,
        "arms": arms.iter().enumerate().map(|(i, c)| {
            let mut arm = json!({"configuration_id": c, "probability_ppm": evaluation.probabilities.get(i).copied().unwrap_or(0)});
            if let Some(reason) = excluded.get(&i) { arm["excluded"] = json!(reason); }
            arm
        }).collect::<Vec<_>>(),
        "chosen_configuration_id": evaluation.chosen.map(|i| arms[i].clone()),
        "abstained": evaluation.abstained,
        "greedy_configuration_id": evaluation.greedy.map(|i| arms[i].clone()),
        "thompson_wins": evaluation.thompson_wins.as_ref().map(|w| w.iter().map(|(i, n)| json!({"configuration_id": arms[*i], "wins": n})).collect::<Vec<_>>())})
}

/// What each configured policy would have chosen for one canonical decision.
pub struct ShadowRecord<'a> {
    pub attempt: &'a str,
    pub task: &'a str,
    pub class: &'a str,
    pub settings_revision: u64,
    pub mode: &'a str,
    pub arms: &'a [String],
    pub evaluations: &'a [(PolicySpec, PolicyEvaluation)],
    pub actual: &'a str,
    pub now: i64,
}

/// Append shadow rows to the sidecar (after the canonical commit; no
/// cross-database transaction). The same attempt and policy replay.
pub fn record_shadow(project: &Path, record: &ShadowRecord) -> Result<()> {
    let mut db = super::sidecar::open(project, true)?.context("telemetry sidecar unavailable")?;
    let tx = db.transaction()?;
    for (spec, evaluation) in record.evaluations {
        let value = evaluation_json(spec, evaluation, record.arms);
        tx.execute("INSERT OR IGNORE INTO policy_shadow_decisions(attempt_id,policy_digest,policy,spec,settings_revision,mode,task_id,task_class,arms,suggested_configuration_id,abstained,actual_configuration_id,seed,draw_ppm,recorded_unix_ms)
            VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
            rusqlite::params![record.attempt, spec.digest(), spec.policy, spec.canonical_json(), record.settings_revision as i64, record.mode, record.task, record.class,
                value["arms"].to_string(), value["chosen_configuration_id"].as_str(), evaluation.abstained, record.actual, evaluation.seed_hex(), evaluation.draw_ppm, record.now])?;
    }
    tx.commit()?;
    Ok(())
}

fn rate(n: u64, d: u64) -> Value {
    if d == 0 { return json!({"value": null, "reason": "empty_denominator"}); }
    json!({"value": format!("{n}/{d}"), "decimal": est::decimal(i128::from(n), i128::from(d), 4)})
}

/// `policies shadow`: per policy, how often its shadow choice differed from
/// the canonical choice, overall and per task class; decisions it abstained
/// on are counted apart and never as agreement. Read-only.
pub fn shadow_report(project: &Path) -> Result<Value> {
    let mut canonical = json!({"decisions": 0, "by_chooser": {}});
    let state = project.join(".state/state.db");
    let mut chooser = BTreeMap::<String, (String, bool)>::new();
    let mut mode = json!("off");
    if state.is_file() {
        let db = super::read_only(&state)?;
        let has = |name: &str| db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)", [name], |r| r.get::<_, bool>(0));
        if has("dispatch_decisions")? {
            let assigned = if has("dispatch_policy_assignments")? { "EXISTS(SELECT 1 FROM dispatch_policy_assignments p WHERE p.attempt_id=d.attempt_id)" } else { "0" };
            for row in db.prepare(&format!("SELECT attempt_id,chooser_kind,{assigned} FROM dispatch_decisions d"))?.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, bool>(2)?)))? {
                let (attempt, kind, policy) = row?;
                chooser.insert(attempt, (kind, policy));
            }
        }
        if has("assignment_policy_settings")? {
            mode = json!(db.query_row("SELECT mode FROM assignment_policy_settings ORDER BY revision DESC LIMIT 1", [], |r| r.get::<_, String>(0)).unwrap_or_else(|_| "off".to_owned()));
        }
    }
    let mut rows = Vec::new();
    if let Some(db) = super::sidecar::open(project, false)?.filter(|db| db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='policy_shadow_decisions')", [], |r| r.get::<_, bool>(0)).unwrap_or(false)) {
        let mut stmt = db.prepare("SELECT attempt_id,policy,policy_digest,settings_revision,mode,task_id,task_class,arms,suggested_configuration_id,abstained,actual_configuration_id,seed,draw_ppm
            FROM policy_shadow_decisions ORDER BY recorded_unix_ms,attempt_id,policy_digest")?;
        rows = stmt.query_map([], |r| Ok(json!({"attempt_id": r.get::<_, String>(0)?, "policy": r.get::<_, String>(1)?, "policy_digest": r.get::<_, String>(2)?,
            "settings_revision": r.get::<_, i64>(3)?, "mode": r.get::<_, String>(4)?, "task_id": r.get::<_, String>(5)?, "task_class": r.get::<_, String>(6)?,
            "arms": serde_json::from_str::<Value>(&r.get::<_, String>(7)?).unwrap_or_default(), "suggested_configuration_id": r.get::<_, Option<String>>(8)?,
            "abstained": r.get::<_, Option<String>>(9)?, "actual_configuration_id": r.get::<_, String>(10)?, "seed": r.get::<_, String>(11)?, "draw_ppm": r.get::<_, i64>(12)?})))?
            .collect::<rusqlite::Result<_>>()?;
    }
    // (policy, digest) → (decisions, agreements, disagreements, abstentions, by class (decisions, disagreements))
    type Tally = (u64, u64, u64, u64, BTreeMap<String, (u64, u64)>);
    let mut tallies = BTreeMap::<(String, String), Tally>::new();
    for row in &rows {
        let t = tallies.entry((row["policy"].as_str().unwrap_or_default().to_owned(), row["policy_digest"].as_str().unwrap_or_default().to_owned())).or_default();
        t.0 += 1;
        let class = t.4.entry(row["task_class"].as_str().unwrap_or_default().to_owned()).or_default();
        match row["suggested_configuration_id"].as_str() {
            None => t.3 += 1,
            Some(s) if s == row["actual_configuration_id"] => { t.1 += 1; class.0 += 1; }
            Some(_) => { t.2 += 1; class.0 += 1; class.1 += 1; }
        }
    }
    let policies: Vec<Value> = tallies.into_iter().map(|((policy, digest), (n, agree, disagree, abstain, classes))| json!({
        "policy": policy, "policy_digest": digest, "decisions": n, "agreements": agree, "disagreements": disagree, "abstentions": abstain,
        "disagreement_rate": rate(disagree, agree + disagree),
        "by_class": classes.into_iter().map(|(c, (d, x))| (c, json!({"decisions": d, "disagreements": x, "disagreement_rate": rate(x, d)}))).collect::<BTreeMap<_, _>>()})).collect();
    let shadowed: std::collections::BTreeSet<&str> = rows.iter().filter_map(|r| r["attempt_id"].as_str()).collect();
    let mut by_chooser = BTreeMap::<String, BTreeMap<&str, u64>>::new();
    for (attempt, (kind, policy)) in &chooser {
        let reason = if shadowed.contains(attempt.as_str()) { "shadowed" } else if kind != "automatic_admission" { "single_profile_chooser" } else if *policy { "policy_assigned" } else { "policies_off" };
        *by_chooser.entry(kind.clone()).or_default().entry(reason).or_default() += 1;
    }
    canonical["decisions"] = json!(chooser.len());
    canonical["by_chooser"] = json!(by_chooser);
    Ok(json!({"schema": SHADOW_SCHEMA, "mode": mode, "read_only": true, "policies": policies, "canonical": canonical, "decisions": rows,
        "note": "shadow choices never change a dispatch; the canonical decision's chooser and probabilities are in dispatch_decisions"}))
}

fn shadow_text(report: &Value) -> String {
    let mut out = format!("mode {}\n", report["mode"].as_str().unwrap_or("off"));
    for p in report["policies"].as_array().into_iter().flatten() {
        let r = &p["disagreement_rate"];
        out += &format!("{} {} decisions {} disagreements {} abstentions {} rate {}\n", p["policy"].as_str().unwrap_or(""), p["policy_digest"].as_str().unwrap_or(""),
            p["decisions"], p["disagreements"], p["abstentions"], r["value"].as_str().map_or_else(|| "n/a (empty_denominator)".to_owned(), |v| format!("{v} ({})", r["decimal"].as_str().unwrap_or(""))));
    }
    out
}

/// `simulate` input: the arms a policy would weigh and the policies to run.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SimulationInput { policies: Vec<Value>, arms: Vec<SimulationArm> }

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SimulationArm {
    configuration_id: String,
    #[serde(default)]
    headroom_percent: Option<String>,
    #[serde(default)]
    assigned: u64,
    #[serde(default)]
    successes: u64,
    #[serde(default)]
    failures: u64,
}

fn spec_of(value: &Value) -> Result<PolicySpec> {
    let text = match value { Value::String(s) => s.clone(), other => other.to_string() };
    PolicySpec::parse(&text).map_err(anyhow::Error::msg)
}

/// Evaluate policies on stated arms for one seed, or count choices over a
/// seed range. Reads nothing; writes nothing.
fn simulate(input: &Path, seed: u64, sweep: Option<u64>) -> Result<Value> {
    let input: SimulationInput = serde_json::from_slice(&std::fs::read(input).with_context(|| format!("read {}", input.display()))?)?;
    if input.arms.is_empty() || input.arms.len() > 256 { bail!("simulate needs 1 to 256 arms"); }
    let mut arms = Vec::new();
    for a in &input.arms {
        let headroom = match &a.headroom_percent { None => None, Some(text) => Some(percent_milli(text).context("headroom_percent is a decimal with at most 3 places")?) };
        arms.push(ArmInput { configuration_id: a.configuration_id.clone(), headroom_milli: headroom, assigned: a.assigned, successes: a.successes, failures: a.failures });
    }
    let names: Vec<String> = arms.iter().map(|a| a.configuration_id.clone()).collect();
    let specs = input.policies.iter().map(spec_of).collect::<Result<Vec<_>>>()?;
    let evaluations: Vec<Value> = specs.iter().map(|spec| {
        let Some(count) = sweep else { return evaluation_json(spec, &spec.evaluate(&arms, seed), &names) };
        let mut chosen = BTreeMap::<String, u64>::new();
        let (mut abstained, mut outside) = (0u64, 0u64);
        for s in seed..seed.saturating_add(count) {
            let e = spec.evaluate(&arms, s);
            match e.chosen {
                // A choice is outside the eligible set if it names no stated arm or one the policy gave probability 0.
                Some(i) if i < names.len() && e.probabilities[i] > 0 => *chosen.entry(names[i].clone()).or_default() += 1,
                Some(_) => outside += 1,
                None => abstained += 1,
            }
        }
        json!({"policy": spec.policy, "policy_digest": spec.digest(), "seeds": {"from": seed, "count": count}, "chosen": chosen, "abstained": abstained,
            "outside_eligible": outside, "probabilities": spec.evaluate(&arms, seed).probabilities})
    }).collect();
    Ok(json!({"schema": SIMULATION_SCHEMA, "read_only": true, "arms": names, "evaluations": evaluations}))
}

/// `herdr-projects telemetry <slug> policies ...`
#[derive(clap::Subcommand)]
pub enum Command {
    /// The operator switch (default `off`), configured policies, grants and assignment counts, as JSON. Read-only.
    Show,
    /// Append a settings revision: `off`, `shadow` (record what each policy would choose), `suggest`
    /// (also the coordinator's default suggestion) or `assign` (the first policy chooses among the approved
    /// profiles; needs an owner-signed `randomized_assignment` grant).
    Configure {
        #[arg(long, value_parser = ["off", "shadow", "suggest", "assign"])]
        mode: String,
        /// A policy: `deterministic.v1`, `uniform.v1`, or a JSON spec such as
        /// `{"policy":"epsilon.v1","epsilon_ppm":100000}`; repeatable, the first is primary.
        #[arg(long = "policy")]
        policies: Vec<String>,
        /// Installed `randomized_assignment` grant (assign only).
        #[arg(long)]
        grant: Option<String>,
    },
    /// Owner-signed `randomized_assignment` grants (`randomized_assignment_authority.v1`,
    /// namespace `randomized-assignment@herdr-projects`).
    #[command(subcommand)]
    Authority(AuthorityCommand),
    /// Evaluate policies on arms stated in a JSON file (what-if); reads and writes nothing.
    Simulate {
        #[arg(long)]
        input: PathBuf,
        #[arg(long, default_value_t = 0)]
        seed: u64,
        /// Evaluate seeds `seed..seed+N` and count choices per arm.
        #[arg(long)]
        sweep: Option<u64>,
    },
    /// The configured policies' suggestion for one ready task among its approved profiles. Read-only.
    Suggest {
        #[arg(long)]
        task: String,
        #[arg(long)]
        json: bool,
    },
    /// Shadow-versus-actual disagreement per policy and task class. Read-only.
    Shadow {
        #[arg(long)]
        json: bool,
    },
}

#[derive(clap::Subcommand)]
pub enum AuthorityCommand {
    /// Install an owner-signed grant; enables nothing by itself.
    Import { document: PathBuf, signature: PathBuf },
}

pub fn run(project: &Path, command: Command) -> Result<String> {
    let pretty = |v: Value| -> Result<String> { Ok(serde_json::to_string_pretty(&v)? + "\n") };
    match command {
        Command::Show => {
            let db = super::read_only(&project.join(".state/state.db"))?;
            pretty(crate::store::assignment_state(&db, jiff::Timestamp::now().as_millisecond())?.context("assignment policies need store schema 65")?)
        }
        Command::Configure { mode, policies, grant } => {
            let specs = policies.iter().map(|p| PolicySpec::parse(p).map_err(anyhow::Error::msg)).collect::<Result<Vec<_>>>()?;
            let settings = crate::authority::configure_assignment(project, &mode, &specs, grant.as_deref())?;
            pretty(json!({"settings": {"revision": settings.revision, "mode": settings.mode, "grant_id": settings.grant_id, "set_unix_ms": settings.set_unix_ms,
                "policies": settings.policies.iter().map(|p| json!({"policy": p.policy, "policy_digest": p.digest()})).collect::<Vec<_>>()}}))
        }
        Command::Authority(AuthorityCommand::Import { document, signature }) => pretty(json!({"grant": crate::authority::import_assignment_authority(project, &document, &signature)?})),
        Command::Simulate { input, seed, sweep } => pretty(simulate(&input, seed, sweep)?),
        Command::Suggest { task, json } => {
            let report = crate::admission::policy_suggestion(project, &task)?;
            if json { return pretty(report); }
            Ok(format!("{} {} suggests {} (mode {}, default suggestion {})\n", report["task_id"].as_str().unwrap_or(""), report["suggestion"]["policy"].as_str().unwrap_or(""),
                report["suggestion"]["chosen_configuration_id"].as_str().unwrap_or("none"), report["mode"].as_str().unwrap_or(""), report["default_suggestion"]))
        }
        Command::Shadow { json } => {
            let report = shadow_report(project)?;
            if json { pretty(report) } else { Ok(shadow_text(&report)) }
        }
    }
}

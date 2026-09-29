//! `telemetry <slug> review seeds ...` (contracts-review.md §8, plan TM3.6):
//! the evaluation authority's seeded-candidate registry, detections, reveal
//! and disposal, and the seeded recall (M43) and clean-control false-alarm
//! (M44) report per reviewer configuration. Every write is the project
//! owner's (`operator:cli`) through `SqliteStore`; `show` and `report` read
//! `state.db` strictly read-only. Reviewer-facing views (`review present`)
//! never read the registry.
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

use crate::store::{EvaluationArm, EvaluationOpportunity, SeedSpec, SeedState, SqliteStore, seed_state};

const OPERATOR: &str = "operator:cli";
const EVALUATION_AUTHORITY: &str = "evaluation_owner.v1";
/// Plan doc 07 §6 provisional minimum for unpaired rates: smaller cells are suppressed.
pub const DEFAULT_MIN_TRIALS: u32 = 20;

/// `herdr-projects telemetry <slug> review seeds ...`
#[derive(clap::Subcommand)]
pub enum SeedsCommand {
    /// Evaluation candidates, seeds, detections, trials and the seed history,
    /// replayed with finding triage to a history sequence, as JSON. Owner view; read-only.
    Show {
        #[arg(long)]
        as_of: Option<i64>,
    },
    /// Register a submission's exact candidate before any review of it: seeded
    /// (`--seed CLASS=sha256:<reproducer>`, repeatable) or a clean control (`--control`).
    Register {
        submission: String,
        /// `logic`, `boundary`, `concurrency`, `security`, `test_weakening` or
        /// `requirement_omission`, `=` the reproducer's `sha256:<hex64>` reference.
        #[arg(long = "seed", conflicts_with = "control", required_unless_present = "control")]
        seeds: Vec<String>,
        #[arg(long)]
        control: bool,
        #[arg(long)]
        expect_seq: Option<i64>,
    },
    /// Link a triaged (validated or duplicate) claim of a review of the seed's candidate to the seed.
    Detect {
        seed: i64,
        #[arg(long)]
        claim: i64,
        /// `sha256:<hex64>` or `verification_run:<hex64>`, repeatable (at least one).
        #[arg(long = "evidence")]
        evidence: Vec<String>,
        #[arg(long)]
        expect_seq: Option<i64>,
    },
    /// Reverse the detection recorded at history sequence SEQ.
    Retract {
        seq: i64,
        #[arg(long)]
        expect_seq: Option<i64>,
    },
    /// Reveal a candidate once every review of it has ended; it is not reviewed again.
    Reveal {
        submission: String,
        #[arg(long)]
        expect_seq: Option<i64>,
    },
    /// Record that a revealed candidate was `discarded` or `repaired`.
    Dispose {
        submission: String,
        #[arg(long)]
        disposition: String,
        #[arg(long)]
        expect_seq: Option<i64>,
    },
    /// M43 seeded recall and M44 clean-control false-alarm rate per reviewer
    /// configuration, seed class, kind and protocol, as JSON. Read-only.
    Report {
        /// Window start (Unix ms), by the opportunity's assignment.
        #[arg(long)]
        since: Option<i64>,
        /// Cells with fewer trials (or controls) are `insufficient_data`, with counts.
        #[arg(long, default_value_t = DEFAULT_MIN_TRIALS, value_parser = clap::value_parser!(u32).range(1..))]
        min_trials: u32,
        #[arg(long)]
        as_of: Option<i64>,
    },
}

fn state(project: &Path, as_of: Option<i64>) -> Result<Option<SeedState>> {
    match super::read(project)? { Some(db) => Ok(seed_state(&db, as_of)?), None => Ok(None) }
}

pub fn run(project: &Path, command: SeedsCommand, now: i64) -> Result<Value> {
    let open = || SqliteStore::open(&project.join(".state/state.db"));
    Ok(match command {
        SeedsCommand::Show { as_of } => json!({"seeds": state(project, as_of)?.context("seeded defects need store schema 58")?}),
        SeedsCommand::Register { submission, seeds, control, expect_seq } => {
            let arm = if control { EvaluationArm::CleanControl } else {
                EvaluationArm::Seeded(seeds.iter().map(|s| {
                    let (class, reproducer) = s.split_once('=').with_context(|| format!("--seed {s}: expected CLASS=sha256:<hex64>"))?;
                    Ok(SeedSpec { seed_class: class.to_owned(), reproducer_ref: reproducer.to_owned() })
                }).collect::<Result<_>>()?)
            };
            json!({"event": open()?.register_evaluation_candidate(&submission, &arm, expect_seq, OPERATOR, now)?})
        }
        SeedsCommand::Detect { seed, claim, evidence, expect_seq } => json!({"event": open()?.record_seed_detection(seed, claim, &evidence, expect_seq, OPERATOR, now)?}),
        SeedsCommand::Retract { seq, expect_seq } => json!({"event": open()?.retract_seed_detection(seq, expect_seq, OPERATOR, now)?}),
        SeedsCommand::Reveal { submission, expect_seq } => json!({"event": open()?.reveal_evaluation_candidate(&submission, expect_seq, OPERATOR, now)?}),
        SeedsCommand::Dispose { submission, disposition, expect_seq } => json!({"event": open()?.dispose_evaluation_candidate(&submission, &disposition, expect_seq, OPERATOR, now)?}),
        SeedsCommand::Report { since, min_trials, as_of } => {
            let state = state(project, as_of)?;
            json!({"metrics": metrics(state.as_ref(), since, min_trials), "since_unix_ms": since, "min_trials": min_trials})
        }
    })
}

fn unavailable(reason: &str) -> Value { json!({"status": "unavailable", "reason": reason}) }

/// `n/d` as a percentage with two decimals, rounded half up.
fn percent(n: usize, d: usize) -> String {
    let hundredths = (n * 20_000 + d) / (2 * d);
    format!("{}.{:02}", hundredths / 100, hundredths % 100)
}

/// One rate cell: counts always shown; the value is null for an empty
/// denominator and suppressed below `min`.
#[derive(Default, Clone, Copy)]
struct Cell { hits: usize, total: usize, pending: usize }

impl Cell {
    fn add(&mut self, status: &str, hit: &str) {
        match status { "pending" => self.pending += 1, s => { self.total += 1; if s == hit { self.hits += 1; } } }
    }
    fn json(&self, hits: &str, total: &str, min: u32) -> Value {
        let mut out = json!({hits: self.hits, total: self.total, "pending": self.pending});
        if self.total == 0 {
            out["value"] = Value::Null;
            out["percent"] = Value::Null;
            out["reason"] = json!("empty_denominator");
        } else if self.total < min as usize {
            out["value"] = unavailable("insufficient_data");
            out["percent"] = unavailable("insufficient_data");
        } else {
            out["value"] = json!(format!("{}/{}", self.hits, self.total));
            out["percent"] = json!(percent(self.hits, self.total));
        }
        out
    }
}

fn cells(map: &BTreeMap<String, Cell>, hits: &str, total: &str, min: u32) -> Value {
    map.iter().map(|(k, c)| (k.clone(), c.json(hits, total, min))).collect::<serde_json::Map<_, _>>().into()
}

/// M43 and M44 (contracts-review.md §8). Also in `review report` and
/// `telemetry <slug> report` at the default minimum.
pub fn metrics(state: Option<&SeedState>, since: Option<i64>, min: u32) -> BTreeMap<String, Value> {
    let mut m43 = json!({"definition": "M43.v1", "name": "seeded_recall", "basis": "owner_triage", "trust": EVALUATION_AUTHORITY, "scope": "seeded_work_only"});
    let mut m44 = json!({"definition": "M44.v1", "name": "clean_control_false_alarm_rate", "basis": "owner_triage", "trust": EVALUATION_AUTHORITY, "scope": "seeded_work_only"});
    let Some(state) = state else {
        for m in [&mut m43, &mut m44] { m["value"] = unavailable("seeded_defects_absent"); }
        return BTreeMap::from([("M43".to_owned(), m43), ("M44".to_owned(), m44)]);
    };
    let in_window = |o: &EvaluationOpportunity| since.is_none_or(|s| o.assigned_unix_ms.is_some_and(|at| at >= s));
    let config = |o: &EvaluationOpportunity| o.configuration_id.clone().unwrap_or_else(|| "unknown".into());
    let not_completed = |arm: &str| state.opportunities.iter().filter(|o| o.arm == arm && !o.completed && in_window(o)).count();

    // M43: one trial per seed on each completed opportunity of its candidate.
    let (mut all, mut by_config, mut by_class, mut by_kind) = (Cell::default(), BTreeMap::<String, Cell>::new(), BTreeMap::<String, Cell>::new(), BTreeMap::<String, Cell>::new());
    let mut by_config_class = BTreeMap::<String, BTreeMap<String, Cell>>::new();
    for t in &state.trials {
        let Some(o) = state.opportunities.iter().find(|o| o.opportunity_id == t.opportunity_id).filter(|o| in_window(o)) else { continue };
        all.add(&t.status, "detected");
        by_config.entry(config(o)).or_default().add(&t.status, "detected");
        by_config_class.entry(config(o)).or_default().entry(t.seed_class.clone()).or_default().add(&t.status, "detected");
        by_class.entry(t.seed_class.clone()).or_default().add(&t.status, "detected");
        by_kind.entry(format!("{}/{}", o.kind, o.protocol)).or_default().add(&t.status, "detected");
    }
    let (h, d) = ("detected", "trials");
    merge(&mut m43, all.json(h, d, min));
    m43["by_configuration"] = by_config.iter().map(|(k, c)| {
        let mut cell = c.json(h, d, min);
        cell["by_seed_class"] = by_config_class.get(k).map_or(json!({}), |m| cells(m, h, d, min));
        (k.clone(), cell)
    }).collect::<serde_json::Map<_, _>>().into();
    m43["by_seed_class"] = cells(&by_class, h, d, min);
    m43["by_kind_protocol"] = cells(&by_kind, h, d, min);
    m43["not_completed"] = json!(not_completed("seeded"));

    // M44: completed clean-control opportunities with at least one rejected-only submission.
    let (mut all, mut by_config, mut by_kind) = (Cell::default(), BTreeMap::<String, Cell>::new(), BTreeMap::<String, Cell>::new());
    for o in state.opportunities.iter().filter(|o| o.arm == "clean_control" && o.completed && in_window(o)) {
        let count = |k: &str| o.submissions.get(k).copied().unwrap_or(0);
        let status = if count("rejected_only") > 0 { "false_alarm" } else if count("pending") > 0 { "pending" } else { "clean" };
        all.add(status, "false_alarm");
        by_config.entry(config(o)).or_default().add(status, "false_alarm");
        by_kind.entry(format!("{}/{}", o.kind, o.protocol)).or_default().add(status, "false_alarm");
    }
    let (h, d) = ("false_alarms", "controls");
    merge(&mut m44, all.json(h, d, min));
    m44["by_configuration"] = cells(&by_config, h, d, min);
    m44["by_kind_protocol"] = cells(&by_kind, h, d, min);
    m44["not_completed"] = json!(not_completed("clean_control"));
    for m in [&mut m43, &mut m44] { m["min_trials"] = json!(min); m["as_of_seq"] = json!(state.as_of_seq); }
    BTreeMap::from([("M43".to_owned(), m43), ("M44".to_owned(), m44)])
}

fn merge(into: &mut Value, from: Value) {
    if let (Some(into), Value::Object(from)) = (into.as_object_mut(), from) { into.extend(from); }
}

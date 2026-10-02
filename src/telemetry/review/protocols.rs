//! `telemetry <slug> review protocols|experiments ...` (contracts-review.md
//! §7, plan TM3.4): the versioned review-protocol registry, second-review
//! passes bound to the ordinary reviews they follow, preregistered
//! experiments with units assigned before outcomes, and M28. Every write is
//! the project owner's (`operator:cli`) through `SqliteStore`; `show` and M28
//! read `state.db` strictly read-only. Nothing here opens, assigns or launches
//! a review: an experiment arm is a label, never a routing decision.
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::store::{ProtocolState, SqliteStore, protocol_state};

const OPERATOR: &str = "operator:cli";
const TRIAGE_AUTHORITY: &str = "operator_owner.v1";
const MAX_DEFINITION_BYTES: u64 = 8 * 1024;

/// `herdr-farm telemetry <slug> review protocols ...`
#[derive(clap::Subcommand)]
pub enum ProtocolsCommand {
    /// Register a versioned review protocol from a `review_protocol.v1` JSON file.
    Register {
        #[arg(long)]
        input_file: PathBuf,
        #[arg(long)]
        expect_seq: Option<i64>,
    },
    /// Bind an opportunity run under a registered protocol as a pass after the
    /// ordinary reviews `--prior`, before its review starts.
    Bind {
        opportunity: String,
        /// An ordinary review opportunity this pass follows, repeatable.
        #[arg(long = "prior", required = true)]
        priors: Vec<String>,
        #[arg(long)]
        expect_seq: Option<i64>,
    },
    /// Reverse the pass binding recorded at history sequence SEQ (in error):
    /// it stops counting as a pass from the retraction on.
    Retract {
        seq: i64,
        #[arg(long)]
        expect_seq: Option<i64>,
    },
    /// Protocols, passes with their incremental yield and the protocol
    /// history, replayed to a history sequence, as JSON. Read-only.
    Show {
        #[arg(long)]
        as_of: Option<i64>,
    },
}

/// `herdr-farm telemetry <slug> review experiments ...`
#[derive(clap::Subcommand)]
pub enum ExperimentsCommand {
    /// Preregister an experiment from a `review_experiment.v1` JSON file (frozen).
    Register {
        #[arg(long)]
        input_file: PathBuf,
        #[arg(long)]
        expect_seq: Option<i64>,
    },
    /// Assign an eligible opportunity to an arm before any outcome: from the
    /// recorded seed (randomized) or `--block` and `--arm` (matched).
    Assign {
        experiment: String,
        opportunity: String,
        #[arg(long, requires = "arm")]
        block: Option<String>,
        #[arg(long, requires = "block")]
        arm: Option<String>,
        #[arg(long)]
        expect_seq: Option<i64>,
    },
    /// Exclude an assigned unit: `ineligible_discovered`, `artifact_withdrawn`,
    /// `protocol_violation` or `operator_error`. It stays listed.
    Exclude {
        experiment: String,
        opportunity: String,
        #[arg(long)]
        reason: String,
        #[arg(long)]
        expect_seq: Option<i64>,
    },
    /// Reverse the unit exclusion recorded at history sequence SEQ (in error):
    /// the unit returns to the estimate from the retraction on.
    Retract {
        seq: i64,
        #[arg(long)]
        expect_seq: Option<i64>,
    },
    /// Experiments with units, exclusions, crossover and the preregistered
    /// estimate, replayed to a history sequence, as JSON. Read-only.
    Show {
        #[arg(long)]
        as_of: Option<i64>,
    },
}

fn definition(path: &Path) -> Result<Vec<u8>> {
    let size = std::fs::metadata(path).with_context(|| format!("read {}", path.display()))?.len();
    anyhow::ensure!(size <= MAX_DEFINITION_BYTES, "definition exceeds {MAX_DEFINITION_BYTES} bytes");
    std::fs::read(path).with_context(|| format!("read {}", path.display()))
}

fn state(project: &Path, as_of: Option<i64>) -> Result<ProtocolState> {
    let db = super::read(project)?.context("review protocols need store schema 57")?;
    protocol_state(&db, as_of, jiff::Timestamp::now().as_millisecond())?.context("review protocols need store schema 57")
}

pub(super) fn protocols(project: &Path, command: ProtocolsCommand, now: i64) -> Result<Value> {
    let open = || SqliteStore::open(&project.join(".state/state.db"));
    Ok(match command {
        ProtocolsCommand::Register { input_file, expect_seq } => json!({"event": open()?.register_review_protocol(&definition(&input_file)?, expect_seq, OPERATOR, now)?}),
        ProtocolsCommand::Bind { opportunity, priors, expect_seq } => json!({"event": open()?.bind_review_pass(&opportunity, &priors, expect_seq, OPERATOR, now)?}),
        ProtocolsCommand::Retract { seq, expect_seq } => json!({"event": open()?.retract_protocol_record(seq, "pass_bound", expect_seq, OPERATOR, now)?}),
        ProtocolsCommand::Show { as_of } => {
            let s = state(project, as_of)?;
            json!({"protocols": {"head_seq": s.head_seq, "as_of_seq": s.as_of_seq, "protocols": s.protocols, "passes": s.passes, "history": s.history}})
        }
    })
}

pub(super) fn experiments(project: &Path, command: ExperimentsCommand, now: i64) -> Result<Value> {
    let open = || SqliteStore::open(&project.join(".state/state.db"));
    Ok(match command {
        ExperimentsCommand::Register { input_file, expect_seq } => json!({"event": open()?.register_review_experiment(&definition(&input_file)?, expect_seq, OPERATOR, now)?}),
        ExperimentsCommand::Assign { experiment, opportunity, block, arm, expect_seq } =>
            json!({"event": open()?.assign_experiment_unit(&experiment, &opportunity, block.as_deref(), arm.as_deref(), expect_seq, OPERATOR, now)?}),
        ExperimentsCommand::Exclude { experiment, opportunity, reason, expect_seq } =>
            json!({"event": open()?.exclude_experiment_unit(&experiment, &opportunity, &reason, expect_seq, OPERATOR, now)?}),
        ExperimentsCommand::Retract { seq, expect_seq } => json!({"event": open()?.retract_protocol_record(seq, "unit_excluded", expect_seq, OPERATOR, now)?}),
        ExperimentsCommand::Show { as_of } => {
            let s = state(project, as_of)?;
            json!({"experiments": {"head_seq": s.head_seq, "as_of_seq": s.as_of_seq, "experiments": s.experiments}})
        }
    })
}

fn unavailable(reason: &str) -> Value { json!({"status": "unavailable", "reason": reason}) }

/// M28 skeptical incremental yield: descriptive over eligible passes (bound
/// in the window `since`), with each preregistered experiment's estimate
/// beside it, never merged into it.
pub(super) fn metric(state: Option<&ProtocolState>, since: Option<i64>) -> Value {
    let mut m = json!({"definition": "M28.v1", "name": "skeptical_incremental_yield", "basis": "owner_triage", "trust": TRIAGE_AUTHORITY,
        "estimate": "descriptive", "observational": true, "causal": unavailable("not_randomized")});
    let Some(state) = state else { m["value"] = unavailable("review_protocols_absent"); return m };
    let window: Vec<_> = state.passes.iter().filter(|p| since.is_none_or(|s| p.bound_unix_ms >= s)).collect();
    let eligible: Vec<_> = window.iter().filter(|p| p.eligible).collect();
    let new: usize = eligible.iter().map(|p| p.new_unique_findings.len()).sum();
    m["numerator"] = json!(new);
    m["denominator"] = json!(eligible.len());
    m["value"] = if eligible.is_empty() { Value::Null } else { json!(format!("{new}/{}", eligible.len())) };
    if eligible.is_empty() { m["reason"] = json!("empty_denominator"); }
    m["rediscovered"] = json!(eligible.iter().map(|p| p.rediscovered).sum::<usize>());
    let mut excluded: BTreeMap<&str, usize> = BTreeMap::new();
    for p in &window { if let Some(reason) = &p.exclusion { *excluded.entry(reason).or_default() += 1; } }
    m["excluded"] = json!(excluded);
    let mut by: BTreeMap<&str, (usize, usize, &str)> = BTreeMap::new();
    for p in &eligible {
        let cell = by.entry(&p.protocol).or_insert((0, 0, &p.prior_disclosure));
        cell.0 += p.new_unique_findings.len();
        cell.1 += 1;
    }
    m["by_protocol"] = by.into_iter().map(|(k, (n, d, disclosure))| (k.to_owned(), json!({"value": format!("{n}/{d}"), "prior_disclosure": disclosure}))).collect();
    // Control opportunities: units of experiment arms without a protocol.
    m["control_opportunities"] = json!(state.experiments.iter().map(|e| e.units.iter().filter(|u| u.treatment_received.is_none() && u.status != "excluded").count()).sum::<usize>());
    m["experiments"] = state.experiments.iter().map(|e| (e.experiment.clone(), json!({"estimate": e.estimate["estimate"], "analysis": e.estimate["analysis"],
        "reference_arm": e.reference_arm, "differences": e.estimate["differences"], "uncertainty": e.estimate["uncertainty"]}))).collect();
    m["as_of_seq"] = json!(state.as_of_seq);
    m
}

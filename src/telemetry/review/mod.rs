//! Lane D (docs/telemetry/phase2-lanes.md): review capture, contracts in
//! docs/telemetry/contracts-review.md. Sidecar stream `review` (no tables yet).
//! Hooks registered centrally in `super::LANES`; this lane adds subcommands,
//! metrics, tick work and `migrations/telemetry/review/NNNN_*.sql` here only.
//! Writes go through the canonical store (`SqliteStore`, one `state.db`
//! transaction each); `show`, `present`, `report`, `findings show`, `fixes show`,
//! `protocols show`, `experiments show`, `seeds show`, `seeds report` and the metrics
//! hook read `state.db` strictly read-only. `present`, the reviewer-facing view, never
//! reads seed state (§8). A launched reviewer (§11) may also run `session` and
//! `submit`, its receipt channel; the blind brief is built in `store::review_launch`.
use anyhow::{Context, Result};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

mod acceptance;
mod fixes;
mod protocols;
mod seeds;
mod signer;

use crate::store::{FindingSummary, FindingTarget, ReviewAssignmentChoice, ReviewOpportunitySpec, ReviewVisibility, SqliteStore, TriageOutcome, TriageRequest, finding_state, fix_state, protocol_state, review_visibility};

pub const STREAM: &str = "review";
/// `include_str!` of `migrations/telemetry/review/`, in order; index + 1 is the stream version.
pub const MIGRATIONS: &[&str] = &[];

/// Principal recorded for rows written on this CLI.
const OPERATOR: &str = "operator:cli";
const MAX_RECEIPT_BYTES: u64 = 64 * 1024;
/// Recorded on the owner's finding triage (contracts-review.md §5).
const TRIAGE_AUTHORITY: &str = "operator_owner.v1";

/// `herdr-projects telemetry <slug> review ...`
#[derive(clap::Subcommand)]
pub enum Command {
    /// Stream version of this lane's sidecar tables. Read-only.
    Status,
    /// Open a review opportunity on one submission's exact candidate.
    Open {
        submission: String,
        /// `candidate_diff`, `candidate_tree` or `contract_scope`.
        #[arg(long, default_value = "candidate_diff")]
        scope: String,
        /// Review method: `code`, `skeptical`, `security`, `test`, `architecture`.
        #[arg(long, default_value = "code")]
        kind: String,
        /// `gate`, `evaluation` or `advisory`.
        #[arg(long, default_value = "evaluation")]
        role: String,
        /// Versioned protocol identifier, e.g. `review-protocol.v1`.
        #[arg(long)]
        protocol: String,
        /// Finding already known before this review (`finding:<ref>`), repeatable.
        #[arg(long = "prior-finding")]
        prior_findings: Vec<String>,
        /// Time budget in milliseconds.
        #[arg(long)]
        budget_ms: Option<u64>,
    },
    /// Assign the opportunity's reviewer, once: `--reviewer` (operator) or
    /// `--blind` over `--candidate` profiles (blind_cross_provider.v1).
    Assign {
        opportunity: String,
        /// Retained native profile chosen by the operator.
        #[arg(long, conflicts_with_all = ["blind", "candidates"])]
        reviewer: Option<String>,
        /// Deterministic blind cross-provider policy over `--candidate`.
        #[arg(long, requires = "candidates")]
        blind: bool,
        /// Retained native profile weighed by the blind policy, repeatable.
        #[arg(long = "candidate")]
        candidates: Vec<String>,
    },
    /// Record that an attempt started a session of an assigned opportunity.
    Start {
        opportunity: String,
        #[arg(long)]
        attempt: String,
    },
    /// Record a session's end from the reviewer's receipt (`review_receipt.v1`)
    /// as a proposal with declared coverage.
    Complete {
        #[arg(long)]
        input_file: PathBuf,
    },
    /// A delegated reviewer's decision on a session's completion: a
    /// `review_acceptance.v1` request signed with a `code_review` grant
    /// subject's key (namespace `review-acceptance@herdr-projects`).
    /// `accept draft SESSION` writes the exact request bytes to sign offline.
    #[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
    Accept {
        #[command(subcommand)]
        draft: Option<AcceptCommand>,
        #[arg(required = true)]
        session: Option<String>,
        #[arg(long, required = true)]
        document: Option<PathBuf>,
        #[arg(long, required = true)]
        signature: Option<PathBuf>,
    },
    /// The reviewing worker's channel: submit the `review_receipt.v1` of a
    /// session recorded at launch while its attempt runs, as a proposal
    /// (`worker:<attempt>`). Allowed in a worker execution context.
    Submit {
        #[arg(long)]
        input_file: PathBuf,
    },
    /// The review session recorded when `--attempt` was launched: the ids its
    /// receipt names, never the author. Read-only; allowed in a worker context.
    Session {
        #[arg(long)]
        attempt: String,
    },
    /// Delegated `code_review` authority: owner-signed grants and revocations.
    #[command(subcommand)]
    Authority(acceptance::AuthorityCommand),
    /// The trusted reviewer-signer process (contracts-review.md §12): the
    /// operator's signer key for one `reviewer:<token>`, outside every worker,
    /// deciding completed reviews under the owner's grant by a mechanical policy.
    #[command(subcommand)]
    Signer(signer::SignerCommand),
    /// What a blind reviewer may see of an opportunity: the exact candidate,
    /// scope and protocol, never the author attempt or configuration. Read-only.
    Present { opportunity: String },
    /// Opportunities with assignment, sessions and completions, as JSON. Read-only.
    Show {
        /// Window start (Unix ms), by the opportunity's creation.
        #[arg(long)]
        since: Option<i64>,
        /// Replay sessions, completions and decisions to this ledger sequence.
        #[arg(long)]
        as_of: Option<i64>,
    },
    /// Lane metrics (M20–M29, M43, M44) as JSON. Read-only.
    Report {
        /// Window start (Unix ms), by the assignment (unassigned: by creation;
        /// finding submissions: by arrival; findings: by their discovery's
        /// arrival; repairs: by opening; integrations: by integration;
        /// passes: by binding).
        #[arg(long)]
        since: Option<i64>,
        /// M27 reopen horizon in days.
        #[arg(long, default_value_t = 14, value_parser = clap::value_parser!(u32).range(1..=3650))]
        horizon_days: u32,
    },
    /// Finding submissions, claims, triage and duplicate history.
    #[command(subcommand)]
    Findings(FindingsCommand),
    /// Repair opportunities, fixes, reopenings, introduction and role credit.
    #[command(subcommand)]
    Fixes(fixes::FixesCommand),
    /// Versioned review protocols and second-review passes (incremental yield).
    #[command(subcommand)]
    Protocols(protocols::ProtocolsCommand),
    /// Preregistered review experiments: units, exclusions, crossover, estimates.
    #[command(subcommand)]
    Experiments(protocols::ExperimentsCommand),
    /// Seeded-defect evaluation: registry, detections, reveal, disposal and
    /// the M43/M44 report. Evaluation authority (the project owner) only.
    #[command(subcommand)]
    Seeds(seeds::SeedsCommand),
}

/// `herdr-projects telemetry <slug> review accept draft ...`
#[derive(clap::Subcommand)]
pub enum AcceptCommand {
    /// Write the exact canonical `review_acceptance.v1` request for the
    /// grant's subject to sign offline (`ssh-keygen -Y sign -n
    /// review-acceptance@herdr-projects`). Signs and decides nothing.
    Draft {
        session: String,
        /// The installed `code_review` grant the decision is made under.
        #[arg(long)]
        grant: String,
        /// Draft a rejection with this reason instead of an acceptance.
        #[arg(long)]
        reject: Option<String>,
        /// New file for the request bytes; must not exist.
        #[arg(long)]
        output: PathBuf,
    },
}

/// `herdr-projects telemetry <slug> review findings ...`. Every write is a
/// triage decision or correction by `operator:cli`, the project owner.
#[derive(clap::Subcommand)]
pub enum FindingsCommand {
    /// Submissions, claims, canonical findings and history replayed to a
    /// history sequence, as JSON. Read-only.
    Show {
        /// Replay the history only up to this sequence (default: its head).
        #[arg(long)]
        as_of: Option<i64>,
    },
    /// Validate a claim: mint a new canonical finding (`--new`) or link an existing one (`--finding`).
    Validate {
        claim: i64,
        #[arg(long, conflicts_with = "finding", required_unless_present = "finding")]
        new: bool,
        /// Title of the new canonical finding (kept as a contracts §7 excerpt).
        #[arg(long, requires = "new")]
        title: Option<String>,
        #[arg(long)]
        finding: Option<String>,
        /// `critical`, `high`, `medium`, `low` or `informational` (finding_severity.v1).
        #[arg(long)]
        severity: String,
        #[command(flatten)]
        common: DecisionArgs,
    },
    /// Reject a claim: `insufficient_evidence`, `intended_behavior` or `out_of_scope`.
    Reject {
        claim: i64,
        #[arg(long)]
        reason: String,
        #[command(flatten)]
        common: DecisionArgs,
    },
    /// Mark a claim a duplicate of an existing canonical finding.
    Duplicate {
        claim: i64,
        #[arg(long)]
        of: String,
        #[command(flatten)]
        common: DecisionArgs,
    },
    /// Return a claim to pending (`reopened` or `decided_in_error`).
    Reset {
        claim: i64,
        #[arg(long)]
        reason: Option<String>,
        #[command(flatten)]
        common: DecisionArgs,
    },
    /// Split a submission into claims, one `--claim TITLE` each (2 to 32).
    Split {
        submission: i64,
        #[arg(long = "claim", required = true)]
        claims: Vec<String>,
        #[arg(long)]
        expect_seq: Option<i64>,
    },
    /// Make an earlier claim revision of a submission current again.
    Restore {
        submission: i64,
        #[arg(long)]
        revision: i64,
        #[arg(long)]
        expect_seq: Option<i64>,
    },
    /// Merge canonical finding SOURCE into TARGET (same root cause).
    Merge {
        source: String,
        #[arg(long)]
        into: String,
        #[arg(long)]
        expect_seq: Option<i64>,
    },
    /// Reverse the merge recorded at history sequence SEQ.
    Unmerge {
        seq: i64,
        #[arg(long)]
        expect_seq: Option<i64>,
    },
}

#[derive(clap::Args)]
pub struct DecisionArgs {
    /// `sha256:<hex64>` or `verification_run:<hex64>`, repeatable.
    #[arg(long = "evidence")]
    evidence: Vec<String>,
    /// Refuse unless the finding history head is still this sequence.
    #[arg(long)]
    expect_seq: Option<i64>,
}

/// Refuse the owner's review CLI inside a worker execution context. Every row
/// it writes is recorded as `operator:cli` (the project owner), and seed state
/// is the owner's alone (§8), so a worker must not reach them through the CLI
/// (contracts-review.md §9). Two markers the product sets for every canonical
/// worker: its `HOME` is its profile's execution home
/// (`worker_supervision::isolated_gated_command`; launch refuses a profile
/// without one), recorded in this project's retained native profiles and
/// collector bindings; its working directory is its task worktree under
/// `<root>/<project>/.state/worktrees/`. They are markers, not authority: a
/// process that rewrites its own environment evades them, and the store API
/// still refuses every worker principal.
fn refuse_worker_context(project: &Path) -> Result<()> { refuse_owner_cli_in_worker_context(project, "the review CLI") }

/// [`refuse_worker_context`]'s markers for another owner CLI named `cli`
/// (`quality groups create|select`, contracts-quality.md §3).
pub(crate) fn refuse_owner_cli_in_worker_context(project: &Path, cli: &str) -> Result<()> {
    let refused = format!("{cli} records the project owner (operator:cli) and refuses to run inside a worker execution context");
    let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    if let (Ok(cwd), Some(root)) = (std::env::current_dir(), project.parent())
        && let Ok(rest) = canonical(&cwd).strip_prefix(canonical(root)) {
        let parts: Vec<&std::ffi::OsStr> = rest.components().map(|c| c.as_os_str()).take(3).collect();
        anyhow::ensure!(!(parts.len() == 3 && parts[1] == ".state" && parts[2] == "worktrees"), "{refused}: the working directory is a task worktree");
    }
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else { return Ok(()) };
    if !project.join(".state/state.db").is_file() { return Ok(()); }
    let homes = execution_homes(&*super::read_only(&project.join(".state/state.db"))?)?;
    let home = canonical(&home);
    anyhow::ensure!(!homes.iter().any(|h| canonical(Path::new(h)) == home), "{refused}: HOME is a worker execution home");
    Ok(())
}

/// The execution homes recorded in this project's retained native profiles
/// and collector bindings: the worker-context marker (§9) and the places the
/// review signer's key may never be (§12).
fn execution_homes(db: &rusqlite::Connection) -> Result<Vec<String>> {
    let mut homes: Vec<String> = db.prepare("SELECT json_extract(report,'$.preparation.profile.execution_home') FROM native_profiles
        WHERE json_type(report,'$.preparation.profile.execution_home')='text'")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    if db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='collector_bindings')", [], |r| r.get::<_, bool>(0))? {
        homes.extend(db.prepare("SELECT DISTINCT execution_home FROM collector_bindings WHERE execution_home IS NOT NULL")?.query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?);
    }
    Ok(homes)
}

/// Projects root and project name of `project`, as a brief's receipt commands name them.
fn root_and_slug(project: &Path) -> Result<(String, String)> {
    let project = std::fs::canonicalize(project).with_context(|| format!("project {} is unavailable", project.display()))?;
    let root = project.parent().context("project has no root")?.to_str().context("projects root is not UTF-8")?.to_owned();
    let slug = project.file_name().and_then(|n| n.to_str()).context("project name is not UTF-8")?.to_owned();
    Ok((root, slug))
}

/// The blind review brief of an assigned opportunity (contracts-review.md
/// §11): the retained instructions of its review task's worker snapshot,
/// used by `memory <slug> snapshot --worker --review-opportunity`. Read-only.
pub fn review_brief_instructions(project: &Path, opportunity: &str) -> Result<String> {
    refuse_worker_context(project)?;
    let (root, slug) = root_and_slug(project)?;
    Ok(SqliteStore::open(&project.join(".state/state.db"))?.review_brief(opportunity, &root, &slug)?.text)
}

/// Bind review task `task`'s worker snapshot `snapshot` (built from
/// `review_brief_instructions`) to `opportunity`, after checking its complete
/// retained rendering for author identities. The owner's record (`operator:cli`).
pub fn bind_review_brief(project: &Path, opportunity: &str, task: &str, snapshot: &str) -> Result<Value> {
    refuse_worker_context(project)?;
    let (root, slug) = root_and_slug(project)?;
    let rendered = crate::memory::render_knowledge_snapshot(project, snapshot)?;
    let text = rendered["text"].as_str().context("retained knowledge text missing")?;
    let now = jiff::Timestamp::now().as_millisecond();
    Ok(json!(SqliteStore::open(&project.join(".state/state.db"))?.bind_review_brief(opportunity, task, snapshot, text, &root, &slug, OPERATOR, now)?))
}

/// The command's stdout. `config_dir` is the product configuration
/// directory (`~/.config/herdr-projects`), where the review signer lives.
pub fn run(project: &Path, config_dir: &Path, command: Command) -> Result<String> {
    // A worker may run only the blind reviewer view, its own session and the receipt channel.
    if !matches!(command, Command::Present { .. } | Command::Session { .. } | Command::Submit { .. }) { refuse_worker_context(project)?; }
    // Inside the sandbox the store is read-only: the channel goes through the
    // attempt's submission spool and prints the ticker's answer.
    #[cfg(target_os = "linux")]
    if let Some(spool) = crate::submission_spool::worker_spool() {
        use crate::submission_spool::{Kind, exchange};
        match &command {
            Command::Submit { input_file } => return exchange(&spool, Kind::ReviewSubmit, Some(receipt_bytes(input_file)?), None),
            Command::Session { attempt } => return exchange(&spool, Kind::ReviewSession, None, Some(attempt.clone())),
            Command::Present { opportunity } => return exchange(&spool, Kind::ReviewPresent, None, Some(opportunity.clone())),
            _ => {}
        }
    }
    let now = jiff::Timestamp::now().as_millisecond();
    let open = || SqliteStore::open(&project.join(".state/state.db"));
    let value = match command {
        Command::Status => super::sidecar::status(project, STREAM)?,
        Command::Open { submission, scope, kind, role, protocol, prior_findings, budget_ms } => {
            let spec = ReviewOpportunitySpec { submission_id: submission, scope, kind, role, protocol, prior_findings, budget_ms };
            json!({"opportunity": open()?.open_review_opportunity(&spec, OPERATOR, now)?})
        }
        Command::Assign { opportunity, reviewer, blind, candidates } => {
            let choice = match reviewer {
                Some(profile) => ReviewAssignmentChoice::Operator { profile },
                None if blind => ReviewAssignmentChoice::BlindCrossProvider { candidates },
                None => anyhow::bail!("assign needs --reviewer or --blind with --candidate"),
            };
            json!({"assignment": open()?.assign_review(&opportunity, &choice, OPERATOR, now)?})
        }
        Command::Start { opportunity, attempt } => json!({"session": open()?.start_review_session(&opportunity, &attempt, OPERATOR, now)?}),
        Command::Complete { input_file } => {
            let size = std::fs::metadata(&input_file).with_context(|| format!("read {}", input_file.display()))?.len();
            anyhow::ensure!(size <= MAX_RECEIPT_BYTES, "review receipt exceeds {MAX_RECEIPT_BYTES} bytes");
            let bytes = std::fs::read(&input_file).with_context(|| format!("read {}", input_file.display()))?;
            json!({"completion": open()?.complete_review_session(&bytes, OPERATOR, now)?})
        }
        Command::Accept { draft: Some(AcceptCommand::Draft { session, grant, reject, output }), .. } => acceptance::draft(project, &session, &grant, reject.as_deref(), &output)?,
        Command::Accept { draft: None, session, document, signature } => {
            let (Some(session), Some(document), Some(signature)) = (session, document, signature) else { anyhow::bail!("accept needs SESSION --document --signature, or `accept draft`") };
            acceptance::accept(project, &session, &document, &signature)?
        }
        Command::Submit { input_file } => return worker_submit(project, &receipt_bytes(&input_file)?),
        Command::Session { attempt } => return worker_session(project, &attempt),
        Command::Authority(command) => acceptance::authority(project, command)?,
        Command::Signer(command) => signer::run(project, config_dir, command)?,
        Command::Present { opportunity } => return worker_present(project, &opportunity),
        Command::Show { since, as_of } => show(project, since, as_of)?,
        Command::Report { since, horizon_days } => json!({"metrics": lane_metrics(project, since, i64::from(horizon_days))?, "since_unix_ms": since}),
        Command::Findings(command) => findings(project, command, now)?,
        Command::Fixes(command) => fixes::run(project, command, now)?,
        Command::Protocols(command) => protocols::protocols(project, command, now)?,
        Command::Seeds(command) => seeds::run(project, command, now)?,
        Command::Experiments(command) => protocols::experiments(project, command, now)?,
    };
    Ok(serde_json::to_string_pretty(&value)? + "\n")
}

/// A worker's review receipt file, bounded as the store bounds it.
fn receipt_bytes(input_file: &Path) -> Result<Vec<u8>> {
    let size = std::fs::metadata(input_file).with_context(|| format!("read {}", input_file.display()))?.len();
    anyhow::ensure!(size <= MAX_RECEIPT_BYTES, "review receipt exceeds {MAX_RECEIPT_BYTES} bytes");
    std::fs::read(input_file).with_context(|| format!("read {}", input_file.display()))
}

/// `review submit`'s stdout for receipt `bytes` (the worker channel, §11):
/// run by the CLI outside a sandbox, or by the ticker for a spooled request.
pub fn worker_submit(project: &Path, bytes: &[u8]) -> Result<String> {
    let now = jiff::Timestamp::now().as_millisecond();
    let completion = SqliteStore::open(&project.join(".state/state.db"))?.submit_review_receipt(bytes, now)?;
    Ok(serde_json::to_string_pretty(&json!({"completion": completion}))? + "\n")
}

/// `review session --attempt`'s stdout. Read-only.
pub fn worker_session(project: &Path, attempt: &str) -> Result<String> {
    let session = SqliteStore::open(&project.join(".state/state.db"))?.review_session_for_attempt(attempt)?;
    Ok(serde_json::to_string_pretty(&json!({"session": session}))? + "\n")
}

/// `review present`'s stdout. Read-only.
pub fn worker_present(project: &Path, opportunity: &str) -> Result<String> {
    Ok(serde_json::to_string_pretty(&present(project, opportunity)?)? + "\n")
}

fn findings(project: &Path, command: FindingsCommand, now: i64) -> Result<Value> {
    let open = || SqliteStore::open(&project.join(".state/state.db"));
    let decide = |claim: i64, outcome: TriageOutcome, common: DecisionArgs| -> Result<Value> {
        let request = TriageRequest { outcome, evidence: common.evidence, expected_seq: common.expect_seq };
        Ok(json!({"event": open()?.triage_finding_claim(claim, &request, OPERATOR, now)?}))
    };
    match command {
        FindingsCommand::Show { as_of } => {
            let db = read(project)?.context("finding triage needs store schema 55")?;
            let state = finding_state(&db, as_of)?.context("finding triage needs store schema 55")?;
            Ok(json!({"findings": state}))
        }
        FindingsCommand::Validate { claim, new, title, finding, severity, common } => {
            let target = match finding { Some(id) if !new => FindingTarget::Existing(id), _ => FindingTarget::New { title } };
            decide(claim, TriageOutcome::Validated { target, severity }, common)
        }
        FindingsCommand::Reject { claim, reason, common } => decide(claim, TriageOutcome::Rejected { reason }, common),
        FindingsCommand::Duplicate { claim, of, common } => decide(claim, TriageOutcome::Duplicate { of }, common),
        FindingsCommand::Reset { claim, reason, common } => decide(claim, TriageOutcome::Pending { reason }, common),
        FindingsCommand::Split { submission, claims, expect_seq } => {
            let titles: Vec<Option<String>> = claims.into_iter().map(Some).collect();
            Ok(json!({"event": open()?.split_finding_submission(submission, &titles, expect_seq, OPERATOR, now)?}))
        }
        FindingsCommand::Restore { submission, revision, expect_seq } => Ok(json!({"event": open()?.restore_finding_claims(submission, revision, expect_seq, OPERATOR, now)?})),
        FindingsCommand::Merge { source, into, expect_seq } => Ok(json!({"event": open()?.merge_findings(&source, &into, expect_seq, OPERATOR, now)?})),
        FindingsCommand::Unmerge { seq, expect_seq } => Ok(json!({"event": open()?.unmerge_findings(seq, expect_seq, OPERATOR, now)?})),
    }
}

fn unavailable(reason: &str) -> Value { json!({"status": "unavailable", "reason": reason}) }

/// `state.db` read-only, or `None` before migration 0054.
pub(super) fn read(project: &Path) -> Result<Option<super::ReadOnly>> {
    let db = super::read_only(&project.join(".state/state.db"))?;
    crate::store::check_schema(&db)?;
    let present: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='review_opportunities')", [], |r| r.get(0))?;
    Ok(present.then_some(db))
}

fn present(project: &Path, opportunity: &str) -> Result<Value> {
    let db = read(project)?.context("review capture needs store schema 54")?;
    let row = db.query_row("SELECT o.opportunity_id,o.task_id,o.contract_revision,s.repository,s.base_oid,o.candidate_oid,s.object_format,o.scope,o.kind,o.protocol,o.prior_findings,o.budget_ms
        FROM review_opportunities o JOIN result_submissions s ON s.submission_id=o.submission_id WHERE o.opportunity_id=?1", [opportunity],
        |r| Ok(json!({"opportunity_id": r.get::<_, String>(0)?, "task_id": r.get::<_, String>(1)?, "contract_revision": r.get::<_, i64>(2)?,
            "repository": r.get::<_, String>(3)?, "base_oid": r.get::<_, String>(4)?, "candidate_oid": r.get::<_, String>(5)?, "object_format": r.get::<_, String>(6)?,
            "scope": r.get::<_, String>(7)?, "kind": r.get::<_, String>(8)?, "protocol": r.get::<_, String>(9)?,
            "prior_findings": serde_json::from_str::<Value>(&r.get::<_, String>(10)?).unwrap_or(Value::Null), "budget_ms": r.get::<_, Option<i64>>(11)?}))).optional()?;
    let row = row.with_context(|| format!("no review opportunity {opportunity}"))?;
    // Blindness check: a reviewer sees only these fields, never seed state,
    // the evaluation arm, the author or its configuration (§3, §8).
    anyhow::ensure!(row.as_object().is_some_and(|o| o.keys().all(|k| PRESENTED.contains(&k.as_str()))), "review presentation carries a field outside the blind view");
    Ok(json!({"presentation": row}))
}

/// The only fields of the blind reviewer view (`review present`).
const PRESENTED: [&str; 12] = ["opportunity_id", "task_id", "contract_revision", "repository", "base_oid", "candidate_oid", "object_format", "scope", "kind", "protocol", "prior_findings", "budget_ms"];

/// One opportunity as `show` reports it, with its status for M20.
struct Opportunity { record: Value, kind: String, protocol: String, assigned: Option<i64>, created: i64, status: &'static str, findings: Option<i64>,
    /// `(session_id, attempt_id, same_attempt_as_author)` in order, the completed session, the latest completion time.
    sessions: Vec<(String, String, bool)>, completed_session: Option<String>, ended: Option<i64> }

fn opportunities(db: &rusqlite::Connection) -> Result<Vec<Opportunity>> { opportunities_at(db, None) }

/// As `opportunities`, with sessions and completions replayed to `at`.
fn opportunities_at(db: &rusqlite::Connection, at: Option<&ReviewVisibility>) -> Result<Vec<Opportunity>> {
    let db = db.unchecked_transaction()?;
    let rows: Vec<(Value, String, String, i64)> = db.prepare("SELECT opportunity_id,submission_id,task_id,contract_revision,candidate_oid,scope,kind,role,protocol,prior_findings,budget_ms,creator_principal,created_unix_ms
        FROM review_opportunities ORDER BY created_unix_ms,rowid")?
        .query_map([], |r| Ok((json!({"opportunity_id": r.get::<_, String>(0)?, "submission_id": r.get::<_, String>(1)?, "task_id": r.get::<_, String>(2)?,
            "contract_revision": r.get::<_, i64>(3)?, "candidate_oid": r.get::<_, String>(4)?, "scope": r.get::<_, String>(5)?, "kind": r.get::<_, String>(6)?,
            "role": r.get::<_, String>(7)?, "protocol": r.get::<_, String>(8)?, "prior_findings": serde_json::from_str::<Value>(&r.get::<_, String>(9)?).unwrap_or(Value::Null),
            "budget_ms": r.get::<_, Option<i64>>(10)?, "creator_principal": r.get::<_, String>(11)?, "created_unix_ms": r.get::<_, i64>(12)?}),
            r.get(6)?, r.get(8)?, r.get(12)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::with_capacity(rows.len());
    for (mut record, kind, protocol, created) in rows {
        let id = record["opportunity_id"].as_str().unwrap_or_default().to_owned();
        let assignment = db.query_row("SELECT policy,reviewer_configuration_id,reviewer_profile_digest,reviewer_family,author_attempt_id,author_configuration_id,author_family,same_family,blind,reason,eligible,assigner_principal,assigned_unix_ms
            FROM review_assignments WHERE opportunity_id=?1", [&id],
            |r| Ok(json!({"policy": r.get::<_, String>(0)?, "reviewer_configuration_id": r.get::<_, String>(1)?, "reviewer_profile_digest": r.get::<_, String>(2)?,
                "reviewer_family": r.get::<_, Option<String>>(3)?, "author_attempt_id": r.get::<_, String>(4)?, "author_configuration_id": r.get::<_, Option<String>>(5)?,
                "author_family": r.get::<_, Option<String>>(6)?, "same_family": r.get::<_, Option<bool>>(7)?, "blind": r.get::<_, bool>(8)?, "reason": r.get::<_, String>(9)?,
                "eligible": serde_json::from_str::<Value>(&r.get::<_, String>(10)?).unwrap_or(Value::Null), "assigner_principal": r.get::<_, String>(11)?,
                "assigned_unix_ms": r.get::<_, i64>(12)?}))).optional()?;
        let sessions: Vec<Value> = db.prepare("SELECT r.session_id,r.ordinal,r.attempt_id,r.configuration_id,r.matches_assignment,r.same_attempt_as_author,r.recorder_principal,r.started_unix_ms,
                c.outcome,c.reason,c.findings_submitted,c.finding_refs,c.evidence_refs,c.coverage_basis,c.trust,c.receipt_digest,c.recorder_principal,c.completed_unix_ms
            FROM review_sessions r LEFT JOIN review_completions c ON c.session_id=r.session_id WHERE r.opportunity_id=?1 ORDER BY r.ordinal")?
            .query_map([&id], |r| {
                let completion = match r.get::<_, Option<String>>(8)? {
                    None => Value::Null,
                    Some(outcome) => json!({"outcome": outcome, "reason": r.get::<_, Option<String>>(9)?, "findings_submitted": r.get::<_, i64>(10)?,
                        "finding_refs": serde_json::from_str::<Value>(&r.get::<_, String>(11)?).unwrap_or(Value::Null),
                        "evidence_refs": serde_json::from_str::<Value>(&r.get::<_, String>(12)?).unwrap_or(Value::Null),
                        "coverage_basis": r.get::<_, String>(13)?, "trust": r.get::<_, String>(14)?, "receipt_digest": r.get::<_, String>(15)?,
                        "recorder_principal": r.get::<_, String>(16)?, "completed_unix_ms": r.get::<_, i64>(17)?}),
                };
                Ok(json!({"session_id": r.get::<_, String>(0)?, "ordinal": r.get::<_, i64>(1)?, "attempt_id": r.get::<_, String>(2)?,
                    "configuration_id": r.get::<_, Option<String>>(3)?, "matches_assignment": r.get::<_, Option<bool>>(4)?,
                    "same_attempt_as_author": r.get::<_, bool>(5)?, "recorder_principal": r.get::<_, String>(6)?, "started_unix_ms": r.get::<_, i64>(7)?,
                    "completion": completion}))
            })?.collect::<rusqlite::Result<_>>()?;
        let sessions: Vec<Value> = match at {
            None => sessions,
            Some(v) => sessions.into_iter().filter(|s| v.started(s["session_id"].as_str().unwrap_or_default())).map(|mut s| {
                if !v.completed(s["session_id"].as_str().unwrap_or_default()) { s["completion"] = Value::Null; }
                s
            }).collect(),
        };
        let completed = sessions.iter().find(|s| s["completion"]["outcome"] == "completed");
        let status = if assignment.is_none() { "unassigned" } else if sessions.is_empty() { "no_session" } else if completed.is_some() { "completed" }
            else if sessions.iter().any(|s| s["completion"].is_null()) { "in_progress" } else { "ended_without_completion" };
        let findings = completed.and_then(|s| s["completion"]["findings_submitted"].as_i64());
        let completed_session = completed.and_then(|s| s["session_id"].as_str()).map(str::to_owned);
        let ended = sessions.iter().filter_map(|s| s["completion"]["completed_unix_ms"].as_i64()).max();
        let session_list = sessions.iter().map(|s| (s["session_id"].as_str().unwrap_or_default().to_owned(), s["attempt_id"].as_str().unwrap_or_default().to_owned(),
            s["same_attempt_as_author"].as_bool().unwrap_or(false))).collect();
        let assigned = assignment.as_ref().and_then(|a| a["assigned_unix_ms"].as_i64());
        record["status"] = json!(status);
        // Unknown is not zero: only a completed review has a findings count.
        record["findings_submitted"] = findings.map_or_else(|| unavailable(status), |n| json!(n));
        record["assignment"] = assignment.unwrap_or(Value::Null);
        record["sessions"] = Value::Array(sessions);
        out.push(Opportunity { record, kind, protocol, assigned, created, status, findings, sessions: session_list, completed_session, ended });
    }
    Ok(out)
}

fn show(project: &Path, since: Option<i64>, as_of: Option<i64>) -> Result<Value> {
    let db = read(project)?;
    // Replay to a watermark of the shared ledger: sessions, completions and decisions appear where they happened (§9, §11).
    let visibility = match &db { Some(db) => Some(review_visibility(db, as_of)?), None => None };
    let (listed, decided) = match &db { Some(db) => (opportunities_at(db, visibility.as_ref().filter(|_| as_of.is_some()))?, acceptance::decisions(db)?), None => (Vec::new(), None) };
    let mut records: Vec<Value> = listed.into_iter().filter(|o| since.is_none_or(|s| o.created >= s)).map(|o| o.record).collect();
    // Each completion's delegated decision: `null` while undecided (a proposal), unavailable before the authority tables.
    for record in &mut records {
        for session in record["sessions"].as_array_mut().into_iter().flatten() {
            if session["completion"].is_null() { continue; }
            let id = session["session_id"].as_str().unwrap_or_default().to_owned();
            let visible = |d: &Value| visibility.as_ref().is_none_or(|v| v.decided(&id)).then(|| {
                let mut d = d.clone();
                d["ledger_seq"] = json!(visibility.as_ref().and_then(|v| v.decision_seq(&id)));
                d
            });
            session["completion"]["acceptance"] = match &decided { None => unavailable(acceptance::ABSENT), Some(d) => d.get(&id).and_then(visible).unwrap_or(Value::Null) };
        }
    }
    let active = match &decided { Some(_) => json!({"active": true, "authority": acceptance::AUTHORITY}), None => json!({"active": false, "reason": acceptance::ABSENT}) };
    let mut out = json!({"acceptance": active, "opportunities": records, "since_unix_ms": since});
    if let (Some(v), Some(_)) = (&visibility, as_of) { out["head_seq"] = json!(v.head_seq); out["as_of_seq"] = json!(v.as_of_seq); }
    Ok(out)
}

fn ratio(numerator: usize, denominator: usize) -> Value {
    if denominator == 0 { Value::Null } else { json!(format!("{numerator}/{denominator}")) }
}

/// Metrics merged into `telemetry <slug> report` (`super::metrics::report`).
/// M20 review completion over assigned opportunities (declared coverage);
/// M22/M23 over fully triaged submissions; M21, M25–M27 and M29 from fix
/// attribution (§6); M28 from review protocols (§7); M24 needs review
/// lifecycle cost.
pub fn metrics(project: &Path, since: Option<i64>) -> Result<BTreeMap<String, Value>> { lane_metrics(project, since, fixes::DEFAULT_HORIZON_DAYS) }

fn lane_metrics(project: &Path, since: Option<i64>, horizon_days: i64) -> Result<BTreeMap<String, Value>> {
    let mut m20 = json!({"definition": "M20.v1", "name": "review_completion", "basis": "declared", "trust": "proposal"});
    match read(project)? {
        None => m20["value"] = unavailable("review_capture_absent"),
        Some(db) => {
            let all = opportunities(&db)?;
            let unassigned = all.iter().filter(|o| o.assigned.is_none() && since.is_none_or(|s| o.created >= s)).count();
            let cohort: Vec<&Opportunity> = all.iter().filter(|o| o.assigned.is_some_and(|at| since.is_none_or(|s| at >= s))).collect();
            let completed = cohort.iter().filter(|o| o.status == "completed").count();
            let count = |status: &str| cohort.iter().filter(|o| o.status == status).count();
            let mut by = BTreeMap::<String, (usize, usize)>::new();
            for o in &cohort {
                let cell = by.entry(format!("{}/{}", o.kind, o.protocol)).or_default();
                cell.1 += 1;
                if o.status == "completed" { cell.0 += 1; }
            }
            m20["numerator"] = json!(completed);
            m20["denominator"] = json!(cohort.len());
            m20["value"] = ratio(completed, cohort.len());
            if cohort.is_empty() { m20["reason"] = json!("empty_denominator"); }
            m20["status"] = json!({"completed": completed, "completed_empty": cohort.iter().filter(|o| o.findings == Some(0)).count(),
                "ended_without_completion": count("ended_without_completion"), "in_progress": count("in_progress"), "no_session": count("no_session")});
            m20["unassigned"] = json!(unassigned);
            m20["by_kind_protocol"] = by.into_iter().map(|(k, (n, d))| (k, ratio(n, d))).collect();
        }
    }
    let mut out = BTreeMap::from([("M20".to_owned(), m20)]);
    // M22/M23 over original submissions, from the owner's triage (contracts-review.md §5),
    // without seed-linked claims: evaluation artefacts, counted apart (§9).
    let triage = match read(project)? { Some(db) => finding_state(&db, None)?, None => None };
    let decided = match read(project)? { Some(db) => acceptance::decisions(&db)?, None => None };
    let mut m22 = json!({"definition": "M22.v1", "name": "proposal_validation_rate", "basis": "owner_triage", "trust": TRIAGE_AUTHORITY});
    let mut m23 = json!({"definition": "M23.v1", "name": "duplicate_report_share", "basis": "owner_triage", "trust": TRIAGE_AUTHORITY});
    match &triage {
        None => for m in [&mut m22, &mut m23] { m["value"] = unavailable("finding_triage_absent"); },
        Some(state) => {
            let (s, seeded_submissions, seeded_claims) = FindingSummary::without_seed_links(state.submissions.iter().filter(|x| since.is_none_or(|at| x.recorded_unix_ms >= at)));
            let buckets = json!({"validated_only": s.validated_only, "rejected_only": s.rejected_only, "duplicate_only": s.duplicate_only, "mixed": s.mixed});
            for (m, numerator) in [(&mut m22, s.has_validated_claim), (&mut m23, s.duplicate_only)] {
                m["numerator"] = json!(numerator);
                m["denominator"] = json!(s.adjudicated);
                m["value"] = ratio(numerator, s.adjudicated);
                if s.adjudicated == 0 { m["reason"] = json!("empty_denominator"); }
                m["buckets"] = buckets.clone();
                m["pending"] = json!(s.pending);
                m["claim_drilldown"] = json!(s.claims);
                m["seeded_evaluation"] = json!({"submissions": seeded_submissions, "claims": seeded_claims});
                m["as_of_seq"] = json!(state.as_of_seq);
                // Drill-down only: the owner's triage decides findings; a review's delegated decision never changes it.
                if let Some(d) = &decided {
                    m["review_acceptance"] = acceptance::acceptance_counts(d, state.submissions.iter().filter(|x| since.is_none_or(|at| x.recorded_unix_ms >= at)).map(|x| x.session_id.as_str()));
                }
            }
        }
    }
    out.insert("M22".to_owned(), m22);
    out.insert("M23".to_owned(), m23);
    // M21 sums discovery credit; M25–M27 and M29 follow repairs; M24 needs review cost (§6).
    let attribution = match read(project)? { Some(db) => fix_state(&db, None)?, None => None };
    let now = jiff::Timestamp::now().as_millisecond();
    out.extend(fixes::metrics(attribution.as_ref(), now, since, horizon_days * 86_400_000));
    // M21 drill-down: F by its discovery review's delegated decision (credit is unchanged).
    if let (Some(state), Some(d), Some(m21)) = (&attribution, &decided, out.get_mut("M21")) {
        let claim_session: BTreeMap<i64, &str> = state.triage.submissions.iter().flat_map(|s| s.claims.iter().map(move |c| (c.claim_id, s.session_id.as_str()))).collect();
        let discovery: BTreeMap<&str, i64> = state.triage.findings.iter().filter_map(|g| Some((g.finding_id.as_str(), g.discovery_claim?))).collect();
        let sessions = state.findings.iter().filter(|f| f.status == "validated" && !f.seeded_evaluation && since.is_none_or(|s| f.discovered_unix_ms.is_some_and(|at| at >= s)))
            .filter_map(|f| discovery.get(f.finding_id.as_str()).and_then(|c| claim_session.get(c)).copied());
        m21["review_acceptance"] = acceptance::acceptance_counts(d, sessions);
    }
    // M24 over closed opportunities assigned in the window, costed from the accounting sidecar (§10).
    let m24 = match read(project)? {
        None => json!({"definition": "M24.v1", "name": "review_discovery_efficiency", "value": unavailable("review_capture_absent")}),
        Some(db) => {
            let all = opportunities(&db)?;
            let mut attempts = BTreeMap::<String, usize>::new();
            for o in &all { for s in &o.sessions { *attempts.entry(s.1.clone()).or_default() += 1; } }
            let authors: std::collections::BTreeSet<String> = db.prepare("SELECT DISTINCT attempt_id FROM result_submissions")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            let cohort: Vec<acceptance::ReviewOpportunityCost> = all.iter().filter(|o| o.assigned.is_some_and(|at| since.is_none_or(|s| at >= s)))
                .map(|o| acceptance::ReviewOpportunityCost { status: o.status, completed_session: o.completed_session.as_deref(), ended_unix_ms: o.ended,
                    sessions: o.sessions.iter().map(|s| acceptance::ReviewSessionCost { attempt_id: &s.1, same_attempt_as_author: s.2 }).collect() }).collect();
            acceptance::m24(project, &cohort, &attempts, &authors, decided.as_ref(), triage.as_ref(), horizon_days * 86_400_000, now)?
        }
    };
    out.insert("M24".to_owned(), m24);
    // M28 skeptical incremental yield: descriptive; experiments beside it (§7).
    let registry = match read(project)? { Some(db) => protocol_state(&db, None, now)?, None => None };
    out.insert("M28".to_owned(), protocols::metric(registry.as_ref(), since));
    // M43/M44 over seeded evaluation candidates (§8), at the default minimum sample.
    let seeded = match read(project)? { Some(db) => crate::store::seed_state(&db, None)?, None => None };
    out.extend(seeds::metrics(seeded.as_ref(), since, seeds::DEFAULT_MIN_TRIALS));
    Ok(out)
}

/// Ticker telemetry pass, after the Codex collect, within `budget`. Writes only the sidecar.
pub fn tick(_project: &Path, _budget: super::codex::Budget) -> Result<()> { Ok(()) }

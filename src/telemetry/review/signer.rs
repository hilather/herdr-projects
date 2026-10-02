//! `telemetry <slug> review signer init|run|status` (contracts-review.md §12,
//! card D10): the trusted reviewer-signer process. It runs as the operator,
//! outside every worker, and holds one reviewer key in
//! `<config_dir>/review-signer/<token>/` (directory 0700, key 0600), a
//! directory the canonical worker sandbox hides (`worker_supervision::Isolation`
//! hides `~/.config/herdr-farm` and `<pinned config dir>/review-signer`).
//! It decides completed reviews only under an installed owner-signed
//! `code_review` grant whose subject key is its own, by an owner-configured,
//! versioned, mechanical policy (never the review's content), and submits
//! each decision through the D8 `accept_review` path, which verifies the
//! grant's owner signature and the request's signature again. It never sees
//! the owner key and can never mint, widen or extend a grant.
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::runner::{Cmd, RealRunner, Runner};
use crate::store::{PreparedReviewAuthority, SqliteStore, review_authority_state};

pub const POLICY_SCHEMA: &str = "review_signer_policy.v1";
pub const AUDIT_SCHEMA: &str = "review_signer_audit.v1";
const SIGNERS: &str = "review-signer";
const KEY: &str = "id_ed25519";
const PUBLIC: &str = "id_ed25519.pub";
const POLICY: &str = "policy.json";
const AUDIT: &str = "audit.jsonl";
const WORK: &str = "work";
const KEYGEN: &str = "/usr/bin/ssh-keygen";
/// The policy `init` writes; the owner edits it (and raises `revision`).
const DEFAULT_POLICY: &str = "{\n  \"schema\": \"review_signer_policy.v1\",\n  \"revision\": 1,\n  \"require_worker_receipt\": true,\n  \"min_evidence_refs\": 0,\n  \"on_failure\": \"reject\"\n}\n";
const MAX_PER_PASS: u32 = 128;
const MAX_POLICY_BYTES: u64 = 4096;

/// `herdr-farm telemetry <slug> review signer ...`
#[derive(clap::Subcommand)]
pub enum SignerCommand {
    /// Generate the signer's Ed25519 key in `<config_dir>/review-signer/<token>/`,
    /// write its default decision policy, and write a draft
    /// `code_review_authority.v1` grant for the owner to sign offline.
    Init {
        /// The reviewer principal, `reviewer:<token>`.
        #[arg(long)]
        subject: String,
        /// Repository the grant covers (absolute path, as submissions name it), repeatable.
        #[arg(long = "repository", required = true)]
        repositories: Vec<String>,
        /// Task contract revision the grant covers, `TASK:REVISION`, repeatable.
        #[arg(long = "task", required = true)]
        tasks: Vec<String>,
        /// Review kind the grant covers, repeatable (default `code`).
        #[arg(long = "kind")]
        kinds: Vec<String>,
        #[arg(long, default_value_t = 16, value_parser = clap::value_parser!(u32).range(1..=1024))]
        max_decisions: u32,
        /// Validity from now, in days.
        #[arg(long, default_value_t = 7, value_parser = clap::value_parser!(u32).range(1..=366))]
        valid_days: u32,
        /// New file for the draft grant bytes; must not exist.
        #[arg(long)]
        output: PathBuf,
    },
    /// Decide completed reviews in the scope of this signer's grants by its
    /// policy, sign each request and record it through `review accept`.
    Run {
        #[arg(long)]
        subject: String,
        /// One pass, then exit (else a pass every `--interval-secs`).
        #[arg(long)]
        once: bool,
        /// At most this many decisions per pass.
        #[arg(long, default_value_t = 16, value_parser = clap::value_parser!(u32).range(1..=MAX_PER_PASS as i64))]
        max: u32,
        #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..=86_400))]
        interval_secs: u64,
    },
    /// The signer's key check, policy, grants with remaining decisions and
    /// its last audited decisions. Read-only.
    Status {
        #[arg(long)]
        subject: String,
        /// How many of the latest audited decisions to show.
        #[arg(long, default_value_t = 10)]
        last: usize,
    },
}

pub(super) fn run(project: &Path, config_dir: &Path, command: SignerCommand) -> Result<Value> {
    refuse_worker(project)?;
    match command {
        SignerCommand::Init { subject, repositories, tasks, kinds, max_decisions, valid_days, output } =>
            init(project, config_dir, &subject, repositories, &tasks, kinds, max_decisions, valid_days, &output),
        SignerCommand::Run { subject, once, max, interval_secs } => {
            if once { return pass(project, config_dir, &subject, max); }
            // A failed pass (a refused key check, a busy store) decides
            // nothing; it is reported and the next pass checks everything again.
            loop {
                match pass(project, config_dir, &subject, max) {
                    Ok(value) => println!("{}", serde_json::to_string(&value)?),
                    Err(error) => println!("{}", json!({"error": format!("{error:#}"), "unix_ms": jiff::Timestamp::now().as_millisecond()})),
                }
                std::io::stdout().flush()?;
                std::thread::sleep(Duration::from_secs(interval_secs));
            }
        }
        SignerCommand::Status { subject, last } => status(project, config_dir, &subject, last),
    }
}

/// The review CLI's worker guard (§9), plus: the process's `HOME` is not
/// under the projects root. A canonical worker cannot read the signer
/// directory at all (it is hidden in the sandbox); this refuses before
/// anything is read when one runs the signer anyway.
fn refuse_worker(project: &Path) -> Result<()> {
    super::refuse_worker_context(project)?;
    if let (Some(home), Some(root)) = (std::env::var_os("HOME"), project.parent()) {
        ensure!(!canonical(Path::new(&home)).starts_with(canonical(root)),
            "the review signer refuses to run inside a worker execution context: HOME is under the projects root");
    }
    Ok(())
}

fn canonical(path: &Path) -> PathBuf { fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()) }
fn euid() -> u32 { unsafe { libc::geteuid() } }

fn token(subject: &str) -> Result<&str> {
    let token = subject.strip_prefix("reviewer:").context("the signer's subject is a reviewer principal `reviewer:<token>`")?;
    ensure!(!token.is_empty() && token.len() <= 64 && !token.starts_with('.')
        && token.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b)),
        "the signer's subject is `reviewer:<token>` (lowercase letters, digits, `.`, `_`, `-`; at most 64)");
    Ok(token)
}

/// Every retained execution home of every project under the projects root.
fn execution_homes(root: &Path) -> Result<Vec<PathBuf>> {
    let mut homes = Vec::new();
    let Ok(entries) = fs::read_dir(root) else { return Ok(homes) };
    for entry in entries.flatten() {
        let db = entry.path().join(".state/state.db");
        if !db.is_file() { continue; }
        let db = super::super::read_only(&db)?;
        homes.extend(super::execution_homes(&db)?.into_iter().map(|h| canonical(Path::new(&h))));
    }
    Ok(homes)
}

/// Where the signer directory may be: directly under the pinned owner
/// configuration's directory (which the worker sandbox hides together with its
/// `review-signer/`), and not inside the projects root (every project and
/// task worktree), nor any retained execution home.
fn check_location(project: &Path, config_dir: &Path, dir: &Path) -> Result<()> {
    let journal = crate::migration::status(project)?;
    let pinned = journal.plan.config.context("the project has no pinned owner configuration")?;
    let pinned_dir = canonical(Path::new(&pinned.path)).parent().map(Path::to_path_buf).context("owner configuration has no directory")?;
    ensure!(canonical(config_dir) == pinned_dir,
        "the review signer lives beside the pinned owner configuration ({}), which workers cannot see", pinned_dir.display());
    let dir = canonical(dir);
    let root = canonical(project.parent().context("project has no root")?);
    ensure!(!dir.starts_with(&root), "the signer key directory {} is inside the projects root (a project or worktree)", dir.display());
    for home in execution_homes(&root)? {
        ensure!(!dir.starts_with(&home) && !home.starts_with(&dir), "the signer key directory {} overlaps execution home {}", dir.display(), home.display());
    }
    Ok(())
}

/// Owner-only directory: a real directory (not a symlink) owned by this user,
/// with exactly `mode` permission bits.
fn check_dir(path: &Path, mode: u32) -> Result<()> {
    let m = fs::symlink_metadata(path).with_context(|| format!("signer directory {} is unavailable", path.display()))?;
    ensure!(m.file_type().is_dir(), "signer directory {} must be a directory, not a symlink", path.display());
    ensure!(m.uid() == euid(), "signer directory {} must be owned by this user", path.display());
    ensure!(m.mode() & 0o777 == mode, "signer directory {} must have mode {mode:o} (has {:o})", path.display(), m.mode() & 0o777);
    Ok(())
}

struct Signer { subject: String, dir: PathBuf, key: PathBuf, public_key: String }

/// Before every use: the signer's directory and key are owner-only (0700,
/// 0600, one link), real files rather than symlinks, owned by this user, and
/// where no worker can see them.
fn signer(project: &Path, config_dir: &Path, subject: &str) -> Result<Signer> {
    let token = token(subject)?;
    let signers = config_dir.join(SIGNERS);
    let dir = signers.join(token);
    check_dir(&signers, 0o700)?;
    check_dir(&dir, 0o700)?;
    check_location(project, config_dir, &dir)?;
    let key = dir.join(KEY);
    let m = fs::symlink_metadata(&key).with_context(|| format!("signer key {} is unavailable", key.display()))?;
    ensure!(m.file_type().is_file(), "signer key {} must be a regular file, not a symlink", key.display());
    ensure!(m.uid() == euid() && m.nlink() == 1, "signer key {} must be owned by this user with a single link", key.display());
    ensure!(m.mode() & 0o777 == 0o600, "signer key {} must have mode 600 (has {:o})", key.display(), m.mode() & 0o777);
    let public = dir.join(PUBLIC);
    let m = fs::symlink_metadata(&public).with_context(|| format!("signer public key {} is unavailable", public.display()))?;
    ensure!(m.file_type().is_file() && m.uid() == euid() && m.mode() & 0o022 == 0, "signer public key {} must be a regular file owned by this user, not group/world writable", public.display());
    let text = String::from_utf8(crate::migration::read_plan_file(&public)?).context("signer public key is not UTF-8")?;
    let public_key = text.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
    ensure!(public_key.starts_with("ssh-ed25519 "), "signer public key is not one Ed25519 key");
    Ok(Signer { subject: subject.to_owned(), dir, key, public_key })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Policy { schema: String, revision: u64, require_worker_receipt: bool, min_evidence_refs: u32, on_failure: String }

/// The owner-configured decision policy and its version `{schema, revision, digest}`.
fn policy(signer: &Signer) -> Result<(Policy, Value)> {
    let path = signer.dir.join(POLICY);
    let m = fs::symlink_metadata(&path).with_context(|| format!("signer policy {} is unavailable", path.display()))?;
    ensure!(m.file_type().is_file() && m.uid() == euid() && m.mode() & 0o077 == 0 && m.len() <= MAX_POLICY_BYTES,
        "signer policy {} must be a regular owner-only file (mode 600) of at most {MAX_POLICY_BYTES} bytes", path.display());
    let bytes = crate::migration::read_plan_file(&path)?;
    let p: Policy = serde_json::from_slice(&bytes).map_err(|e| anyhow::anyhow!("invalid signer policy: {e}"))?;
    ensure!(p.schema == POLICY_SCHEMA, "signer policy schema must be {POLICY_SCHEMA}");
    ensure!((1..=i64::MAX as u64).contains(&p.revision), "signer policy revision must be positive");
    ensure!(p.min_evidence_refs <= 64, "signer policy min_evidence_refs is at most 64");
    ensure!(matches!(p.on_failure.as_str(), "reject" | "leave_undecided"), "signer policy on_failure is `reject` or `leave_undecided`");
    let version = json!({"schema": POLICY_SCHEMA, "revision": p.revision, "digest": format!("sha256:{:x}", Sha256::digest(&bytes))});
    Ok((p, version))
}

/// The mechanical decision on one candidate's stored facts: `(decision,
/// reason, rule)`, decision `None` when the policy leaves it undecided. Rules
/// in order; the first that fails decides. Never reads the review's content.
fn decide(policy: &Policy, facts: &Value) -> (Option<&'static str>, Option<&'static str>, &'static str) {
    let s = |k: &str| facts[k].as_str().unwrap_or_default();
    let n = |k: &str| facts[k].as_i64().unwrap_or(-1);
    let worker = format!("worker:{}", s("attempt_id"));
    let rules: [(&str, bool, &str); 5] = [
        ("well_formed_receipt", s("trust") == "proposal" && s("coverage_basis") == "declared" && n("findings_submitted") == n("finding_refs")
            && s("receipt_digest").strip_prefix("sha256:").is_some_and(|h| h.len() == 64), "protocol_violation"),
        ("exact_candidate", s("receipt_submission_id") == s("submission_id") && s("receipt_candidate_oid") == s("candidate_oid"), "wrong_scope"),
        ("launched_to_assigned_reviewer", facts["launched"] == true && s("session_recorder") == "service:launch" && facts["matches_assignment"] == true
            && facts["session_configuration_id"] == facts["assigned_configuration_id"], "protocol_violation"),
        ("receipt_from_reviewer_worker", !policy.require_worker_receipt || s("receipt_recorder") == worker, "protocol_violation"),
        ("min_evidence_refs", n("evidence_refs") >= i64::from(policy.min_evidence_refs), "evidence_missing"),
    ];
    match rules.iter().find(|(_, ok, _)| !ok) {
        None => (Some("accepted"), None, "all_rules_passed"),
        Some((rule, _, reason)) if policy.on_failure == "reject" => (Some("rejected"), Some(reason), rule),
        Some((rule, _, _)) => (None, None, rule),
    }
}

/// Append one line to the signer's audit log (append-only, owner-only).
fn audit(signer: &Signer, mut line: Value) -> Result<()> {
    line["schema"] = json!(AUDIT_SCHEMA);
    line["subject"] = json!(signer.subject);
    line["recorded_unix_ms"] = json!(jiff::Timestamp::now().as_millisecond());
    let path = signer.dir.join(AUDIT);
    let mut file = OpenOptions::new().append(true).create(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&path)
        .with_context(|| format!("open signer audit log {}", path.display()))?;
    let m = file.metadata()?;
    ensure!(m.is_file() && m.uid() == euid() && m.mode() & 0o077 == 0 && m.nlink() == 1, "signer audit log {} must be a regular owner-only file", path.display());
    file.write_all(format!("{}\n", serde_json::to_string(&line)?).as_bytes())?;
    file.sync_all()?;
    Ok(())
}

fn audit_lines(signer: &Signer) -> Result<Vec<Value>> {
    let path = signer.dir.join(AUDIT);
    if fs::symlink_metadata(&path).is_err() { return Ok(Vec::new()); }
    let text = String::from_utf8(crate::migration::read_plan_file(&path)?).context("signer audit log is not UTF-8")?;
    text.lines().map(|l| serde_json::from_str(l).context("invalid signer audit line")).collect()
}

fn new_file(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).mode(mode).custom_flags(libc::O_NOFOLLOW).open(path)
        .with_context(|| format!("create {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn init(project: &Path, config_dir: &Path, subject: &str, mut repositories: Vec<String>, tasks: &[String], mut kinds: Vec<String>,
    max_decisions: u32, valid_days: u32, output: &Path) -> Result<Value> {
    let token = token(subject)?.to_owned();
    ensure!(fs::symlink_metadata(output).is_err(), "{} exists; the draft grant is written to a new file", output.display());
    let signers = config_dir.join(SIGNERS);
    let dir = signers.join(&token);
    ensure!(fs::symlink_metadata(&dir).is_err(), "signer {subject} already exists at {}; a signer's key is never replaced in place", dir.display());
    // The draft's scope is checked with a placeholder key before any key exists.
    repositories.sort();
    repositories.dedup();
    if kinds.is_empty() { kinds.push("code".into()); }
    kinds.sort();
    kinds.dedup();
    let mut scope = tasks.iter().map(|t| {
        let (task, revision) = t.rsplit_once(':').context("a task is `TASK:REVISION`")?;
        Ok(json!({"task_id": task, "contract_revision": revision.parse::<i64>().context("a task's contract revision is an integer")?}))
    }).collect::<Result<Vec<Value>>>()?;
    scope.sort_by_key(|t| (t["task_id"].as_str().unwrap_or_default().to_owned(), t["contract_revision"].as_i64()));
    scope.dedup();
    let store = fs::canonicalize(project.join(".state/state.db")).context("project store unavailable")?.to_string_lossy().into_owned();
    let authority = crate::authority::policy_reference(project)?;
    let now = jiff::Timestamp::now().as_millisecond();
    let grant = |key: &str| -> Result<Vec<u8>> {
        let mut bytes = serde_json::to_vec_pretty(&json!({"schema": "code_review_authority.v1", "scope": "code_review", "issuer": "owner",
            "subject": subject, "subject_public_key": key, "subject_configurations": [], "project_store": store, "repositories": repositories,
            "tasks": scope, "kinds": kinds, "review_configurations": [], "actions": ["accept_review_completion"], "max_decisions": max_decisions,
            "valid_from_unix_ms": now, "expires_unix_ms": now + i64::from(valid_days) * 86_400_000,
            "prohibited_effects": ["alter_requirements", "approve_author_attempt", "approve_own_work", "child_delegation", "increase_permissions"],
            "authority": authority}))?;
        bytes.push(b'\n');
        PreparedReviewAuthority::parse_verified(&bytes).map_err(|e| anyhow::anyhow!("draft grant: {e}"))?;
        Ok(bytes)
    };
    grant(&format!("ssh-ed25519 {}", "A".repeat(68)))?;
    // The key directory: owner-only, beside the pinned configuration, outside every project and execution home.
    if fs::symlink_metadata(&signers).is_err() {
        fs::DirBuilder::new().mode(0o700).create(&signers).with_context(|| format!("create {}", signers.display()))?;
    }
    check_dir(&signers, 0o700)?;
    check_location(project, config_dir, &signers.join(&token))?;
    fs::DirBuilder::new().mode(0o700).create(&dir).with_context(|| format!("create {}", dir.display()))?;
    check_dir(&dir, 0o700)?;
    let key = dir.join(KEY);
    let mut keygen = Cmd::new(KEYGEN, Duration::from_secs(10)).args(["-q", "-t", "ed25519", "-N", "", "-C", subject, "-f"])
        .arg(key.to_str().context("signer key path is not UTF-8")?);
    keygen.env_clear = true;
    keygen = keygen.env("PATH", "/usr/bin:/bin");
    let out = RealRunner.run(&keygen).context("ssh-keygen failed")?;
    ensure!(out.success(), "ssh-keygen failed: {}", out.error_text());
    new_file(&dir.join(POLICY), DEFAULT_POLICY.as_bytes(), 0o600)?;
    let signer = signer(project, config_dir, subject)?;
    let (_, policy) = policy(&signer)?;
    let bytes = grant(&signer.public_key)?;
    new_file(output, &bytes, 0o644)?;
    let digest = format!("sha256:{:x}", Sha256::digest(&bytes));
    audit(&signer, json!({"event": "init", "public_key": signer.public_key, "policy": policy, "draft_grant_digest": digest}))?;
    Ok(json!({"signer": {"subject": subject, "directory": signer.dir, "public_key": signer.public_key, "policy": policy},
        "draft_grant": {"document": output, "grant_id": digest, "namespace": crate::authority::REVIEW_AUTHORITY_SIGNATURE_NAMESPACE,
            "signed_by": "owner", "next": "the owner signs the exact bytes offline, then runs `review authority import DOC DOC.sig`"}}))
}

/// This signer's grants: each installed grant naming its subject, with its
/// status, `other_key` when it names another public key, and `unverified`
/// when an active grant's owner signature does not verify now (never used).
fn grants(project: &Path, db: &crate::telemetry::ReadOnly, store: &SqliteStore, signer: &Signer, now: i64) -> Result<Vec<Value>> {
    let state = review_authority_state(db, now)?.context("the review signer needs the reviewer-authority store schema")?;
    let mut mine = Vec::new();
    for g in state["grants"].as_array().into_iter().flatten().filter(|g| g["subject"] == signer.subject.as_str()) {
        let id = g["grant_id"].as_str().unwrap_or_default();
        let (grant, _) = store.review_authority_grant(id)?;
        let status = if grant.subject_public_key() != signer.public_key { json!("other_key") }
            else if g["status"] == "active" && crate::authority::verify_installed_review_grant(project, id).is_err() { json!("unverified") }
            else { g["status"].clone() };
        let (max, used) = (g["max_decisions"].as_i64().unwrap_or(0), g["decisions"].as_i64().unwrap_or(0));
        mine.push(json!({"grant_id": id, "status": status, "max_decisions": max, "decisions": used, "remaining": (max - used).max(0),
            "valid_from_unix_ms": g["valid_from_unix_ms"], "expires_unix_ms": g["expires_unix_ms"], "revocation": g["revocation"]}));
    }
    Ok(mine)
}

/// One bounded pass: for each active grant of this signer's key, decide its
/// candidates by the policy, sign and submit through `accept_review`.
/// Idempotent: a decided session is no longer a candidate, and a request the
/// store already holds replays.
fn pass(project: &Path, config_dir: &Path, subject: &str, max: u32) -> Result<Value> {
    let signer = signer(project, config_dir, subject)?;
    let (policy, version) = policy(&signer)?;
    let now = jiff::Timestamp::now().as_millisecond();
    let db_path = project.join(".state/state.db");
    let store = SqliteStore::open(&db_path)?;
    let grants = grants(project, &super::super::read_only(&db_path)?, &store, &signer, now)?;
    let (mut decided, mut undecided, mut refused) = (Vec::new(), Vec::new(), Vec::new());
    let mut budget = max;
    for grant in grants.iter().filter(|g| g["status"] == "active") {
        let id = grant["grant_id"].as_str().unwrap_or_default();
        let mut remaining = grant["remaining"].as_u64().unwrap_or(0);
        for facts in store.review_signer_candidates(id, now)? {
            if budget == 0 || remaining == 0 { break; }
            let session = facts["session_id"].as_str().unwrap_or_default().to_owned();
            let (decision, reason, rule) = decide(&policy, &facts);
            let Some(decision) = decision else {
                undecided.push(json!({"session_id": session, "grant_id": id, "rule": rule}));
                continue;
            };
            budget -= 1;
            let mut line = json!({"event": "decision", "grant_id": id, "session_id": session, "decision": decision, "reason": reason, "rule": rule,
                "policy": version, "checks": facts});
            match sign_and_accept(project, config_dir, subject, &session, id, reason) {
                Ok((acceptance, request_digest)) => {
                    remaining -= 1;
                    line["result"] = json!("recorded");
                    line["request_digest"] = json!(request_digest);
                    line["ledger_seq"] = acceptance["ledger_seq"].clone();
                    decided.push(json!({"session_id": session, "grant_id": id, "decision": decision, "reason": reason, "rule": rule,
                        "request_digest": request_digest, "ledger_seq": acceptance["ledger_seq"], "replayed": acceptance["replayed"]}));
                }
                Err(error) => {
                    line["result"] = json!("refused");
                    line["error"] = json!(format!("{error:#}"));
                    refused.push(json!({"session_id": session, "grant_id": id, "error": format!("{error:#}")}));
                }
            }
            audit(&signer, line)?;
        }
    }
    let grants = grants_now(project, &signer)?;
    Ok(json!({"signer": {"subject": subject, "public_key": signer.public_key, "policy": version}, "grants": grants,
        "decided": decided, "left_undecided": undecided, "refused": refused, "max_per_pass": max}))
}

fn grants_now(project: &Path, signer: &Signer) -> Result<Vec<Value>> {
    let db_path = project.join(".state/state.db");
    grants(project, &super::super::read_only(&db_path)?, &SqliteStore::open(&db_path)?, signer, jiff::Timestamp::now().as_millisecond())
}

/// Draft the D9 canonical request, sign its exact bytes with the signer key
/// (checked again first), and record it through the D8 accept path.
fn sign_and_accept(project: &Path, config_dir: &Path, subject: &str, session: &str, grant: &str, reason: Option<&str>) -> Result<(Value, String)> {
    let (bytes, _) = SqliteStore::open(&project.join(".state/state.db"))?.draft_review_acceptance(session, grant, reason)?;
    let request_digest = format!("sha256:{:x}", Sha256::digest(&bytes));
    let signer = signer(project, config_dir, subject)?;
    let work = signer.dir.join(WORK);
    if fs::symlink_metadata(&work).is_ok() { fs::remove_dir_all(&work)?; }
    fs::DirBuilder::new().mode(0o700).create(&work)?;
    let result = (|| {
        let document = work.join("request.json");
        new_file(&document, &bytes, 0o600)?;
        let mut sign = Cmd::new(KEYGEN, Duration::from_secs(10)).args(["-Y", "sign", "-f"]).arg(signer.key.to_str().context("signer key path is not UTF-8")?)
            .args(["-n", crate::authority::REVIEW_ACCEPTANCE_SIGNATURE_NAMESPACE]).stdin(std::str::from_utf8(&bytes)?);
        sign.env_clear = true;
        sign = sign.env("PATH", "/usr/bin:/bin");
        sign.capture_limit = 8192;
        let out = RealRunner.run(&sign).context("ssh-keygen signing failed")?;
        ensure!(out.success() && !out.stdout_truncated, "ssh-keygen signing failed: {}", out.error_text());
        let signature = work.join("request.json.sig");
        new_file(&signature, &out.stdout_bytes, 0o600)?;
        let acceptance = crate::authority::accept_review(project, session, &document, &signature)?;
        Ok(serde_json::to_value(acceptance)?)
    })();
    let _ = fs::remove_dir_all(&work);
    Ok((result?, request_digest))
}

fn status(project: &Path, config_dir: &Path, subject: &str, last: usize) -> Result<Value> {
    let token = token(subject)?;
    let signer = match signer(project, config_dir, subject) {
        Ok(signer) => signer,
        Err(error) => return Ok(json!({"signer": {"subject": subject, "directory": config_dir.join(SIGNERS).join(token), "key_check": format!("refused: {error:#}")}})),
    };
    let policy = match policy(&signer) { Ok((_, version)) => version, Err(error) => json!({"error": format!("{error:#}")}) };
    let lines = audit_lines(&signer)?;
    let decisions: Vec<Value> = lines.into_iter().filter(|l| l["event"] == "decision").collect();
    let skip = decisions.len().saturating_sub(last);
    Ok(json!({"signer": {"subject": subject, "directory": signer.dir, "public_key": signer.public_key, "key_check": "ok", "policy": policy},
        "grants": grants_now(project, &signer)?, "last_decisions": decisions[skip..].iter().map(|d| json!({
            "recorded_unix_ms": d["recorded_unix_ms"], "session_id": d["session_id"], "grant_id": d["grant_id"], "decision": d["decision"],
            "reason": d["reason"], "rule": d["rule"], "result": d["result"], "ledger_seq": d["ledger_seq"], "policy": d["policy"]})).collect::<Vec<_>>()}))
}

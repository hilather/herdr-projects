//! Replay evaluation suite (plan TM4.6, docs/telemetry/contracts-replay.md):
//! accepted historical tasks become versioned replay cases, and a new
//! configuration is measured on a reproducible stratified subset through the
//! ordinary queue, admission, launch, verification and budget paths.
//!
//! * `replay extract` (owner) reads each accepted task (its latest
//!   integrated submission) deterministically. The accepted change's test
//!   files `tests/expected/<path>` become hidden checks: their bytes go to the
//!   owner's check store `<projects root>/.replay/<slug>/checks/<sha256>`,
//!   outside every path a worker is shown (the sandbox covers the projects
//!   root except the worker's own project); the store records only paths and
//!   digests. A metadata-only contamination scan looks for the solution's
//!   distinctive lines in PROJECT.md, retained memory objects, worker briefs
//!   and the source contract, and for a retained source worktree;
//!   contaminated cases are recorded but excluded, with counts.
//! * `replay run` creates ordinary tasks, registered as replay candidates,
//!   each over its own replay repository holding only the base commit's
//!   history. It grants nothing: the owner signs each drafted contract
//!   (`replay contract`) and each launch approval as for any other task.
//! * The hidden check runs only in the isolated verifier, which binds the
//!   pinned hidden files read-only into its private root (`verification`).
//! * `replay report` gives per-configuration pass rates (M49) with the suite
//!   version, exclusions and n.
use anyhow::{Context, Result, ensure};
use rusqlite::Connection;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::{BTreeMap, BTreeSet}, fs, path::{Path, PathBuf}, process::Command as Process};

use crate::{domain::{ContractScope, TaskId, TASK_CLASSIFIER, classify_task}, execution_guard::GatedSpawn, migration,
    store::{REPLAY_PRINCIPAL, ReplayCaseRecord, ReplaySource, ReplaySuiteRecord}};

/// The v1 extractor: accepted history, expected-output test files.
pub const EXTRACTOR: &str = "accepted-history.expected-output.v1";
/// Test files of the accepted change that become hidden checks.
pub const EXPECTED_PREFIX: &str = "tests/expected/";
pub const SCANNER: &str = "distinctive-lines.v1";
pub const DEFINITION: &str = "M49.v1";
/// Shortest trimmed added line that counts as distinctive solution content.
const MIN_LINE: usize = 16;
const MAX_HIDDEN_BYTES: usize = 65_536;
const MAX_SCAN_FILES: usize = 4096;
const MAX_SCAN_BYTES: u64 = 1 << 20;

/// `herdr-farm replay <slug> ...` (owner CLI; refused inside a worker execution context).
#[derive(clap::Subcommand)]
pub enum Command {
    /// Build immutable suite version SUITE from the project's accepted tasks. Writes hidden checks outside the project.
    Extract { #[arg(long)] suite: String },
    /// A suite version: its cases, exclusions, retirements and runs. Read-only.
    Show { #[arg(long)] suite: String },
    /// The reproducible subset `stratified:N` drawn with SEED. Read-only.
    Subset { #[arg(long)] suite: String, #[arg(long)] subset: String, #[arg(long)] seed: String },
    /// Retire one case whose checks no longer apply; it leaves every later run and report.
    Retire { #[arg(long)] suite: String, #[arg(long = "case")] case_id: String, #[arg(long)] reason: String },
    /// Create one ordinary task per case of the subset, registered as replay candidates. Grants no launch.
    Run { #[arg(long)] suite: String, #[arg(long)] configuration: String, #[arg(long)] subset: String, #[arg(long)] seed: String, #[arg(long)] expected_head: u64 },
    /// Draft the unsigned contract of replay task TASK at the current head; the owner signs and installs it with `task contract put`.
    Contract { task: String, #[arg(long)] output: PathBuf },
    /// Per-configuration pass rates on the suite (M49) with exclusions and n. Read-only.
    Report { #[arg(long)] suite: String, #[arg(long)] since: Option<i64> },
}

pub fn run(project: &Path, command: Command) -> Result<Value> {
    crate::telemetry::review::refuse_owner_cli_in_worker_context(project, "the replay CLI")?;
    match command {
        Command::Extract { suite } => extract(project, &suite),
        Command::Show { suite } => show(project, &suite),
        Command::Subset { suite, subset, seed } => {
            let db = read(project)?;
            let (cases, _) = eligible(&db, &suite)?;
            Ok(json!({"suite_version": suite, "subset": subset, "seed": seed, "cases": select(&suite, &cases, parse_subset(&subset)?, &seed)?}))
        }
        Command::Retire { suite, case_id, reason } => {
            let _guard = migration::runtime_mutation(project)?;
            let seq = migration::open_active(project)?.retire_replay_case(&suite, &case_id, &reason, REPLAY_PRINCIPAL, now())?;
            Ok(json!({"seq": seq, "suite_version": suite, "case_id": case_id, "reason": reason}))
        }
        Command::Run { suite, configuration, subset, seed, expected_head } => run_suite(project, &suite, &configuration, &subset, &seed, expected_head),
        Command::Contract { task, output } => contract(project, &task, &output),
        Command::Report { suite, since } => report(&*read(project)?, &suite, since),
    }
}

fn now() -> i64 { jiff::Timestamp::now().as_millisecond() }
fn hex(bytes: &[u8]) -> String { format!("{:x}", Sha256::digest(bytes)) }
fn read(project: &Path) -> Result<crate::telemetry::ReadOnly> { crate::telemetry::read_only(&project.join(".state/state.db")) }
fn slug(project: &Path) -> Result<String> { Ok(project.file_name().and_then(|n| n.to_str()).context("project has no name")?.to_owned()) }
/// `<projects root>/.replay/<slug>`: outside the project, so hidden from its workers.
fn replay_dir(project: &Path) -> Result<PathBuf> {
    let project = project.canonicalize()?;
    Ok(project.parent().context("project has no root")?.join(".replay").join(slug(&project)?))
}
fn check_path(project: &Path, sha: &str) -> Result<PathBuf> { Ok(replay_dir(project)?.join("checks").join(sha)) }
fn label(value: &str, what: &str) -> Result<()> {
    ensure!(!value.is_empty() && value.len() <= 32 && value.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)) && !value.starts_with('.'),
        "{what} must be 1 to 32 ASCII letters, digits, '-', '_' or '.'");
    Ok(())
}
fn private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    fs::DirBuilder::new().recursive(true).mode(0o700).create(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn git_command(repo: &Path) -> Process {
    let mut command = Process::new("/usr/bin/git");
    command.env_clear().env("PATH", "/usr/bin:/bin").env("HOME", "/").env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0").args(["-c", "core.hooksPath=/dev/null", "-C"]).arg(repo);
    command
}
fn git(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = git_command(repo).args(args).output_gated().context("git")?;
    ensure!(out.status.success(), "git {} failed: {}", args.first().copied().unwrap_or_default(), String::from_utf8_lossy(&out.stderr).trim());
    Ok(out.stdout)
}

/// `(status, path)` of every change from `base` to `reference`.
fn changes(repo: &Path, base: &str, reference: &str) -> Result<Vec<(char, String)>> {
    let raw = git(repo, &["diff", "--name-status", "--no-renames", "-z", base, reference])?;
    let parts: Vec<&[u8]> = raw.split(|b| *b == 0).filter(|p| !p.is_empty()).collect();
    ensure!(parts.len().is_multiple_of(2), "unexpected git diff output");
    parts.chunks(2).map(|pair| Ok((pair[0].first().copied().context("empty status")? as char, String::from_utf8(pair[1].to_vec())?))).collect()
}

/// Everything a replay candidate could read that might carry a solution:
/// `(source kind, id, text)`, plus the attempts with a retained worktree or Git quarantine.
struct Corpus { documents: Vec<(&'static str, String, String)>, retained: BTreeSet<String>, scanned: BTreeMap<&'static str, usize> }

fn corpus(project: &Path, briefs: Vec<(String, String)>) -> Result<Corpus> {
    let mut documents = Vec::new();
    let mut scanned = BTreeMap::from([("project_md", 0), ("memory_objects", 0), ("worker_briefs", 0)]);
    if let Ok(text) = fs::read_to_string(project.join("PROJECT.md")) { documents.push(("project_md", "PROJECT.md".to_owned(), text)); scanned.insert("project_md", 1); }
    let objects = project.join(".state/objects");
    let mut stack = vec![objects.clone()];
    let mut files = Vec::new();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() { stack.push(entry.path()); } else if kind.is_file() { files.push(entry.path()); }
        }
        ensure!(files.len() <= MAX_SCAN_FILES, "too many retained memory objects to scan");
    }
    files.sort();
    for file in files {
        if fs::metadata(&file).is_ok_and(|m| m.len() <= MAX_SCAN_BYTES) && let Ok(bytes) = fs::read(&file) {
            let id = file.strip_prefix(&objects).unwrap_or(&file).display().to_string();
            documents.push(("memory_object", id, String::from_utf8_lossy(&bytes).into_owned()));
            *scanned.get_mut("memory_objects").unwrap() += 1;
        }
    }
    scanned.insert("worker_briefs", briefs.len());
    documents.extend(briefs.into_iter().map(|(id, text)| ("worker_brief", id, text)));
    let mut retained = BTreeSet::new();
    for dir in [project.join(".state/worktrees"), project.join(".git-quarantine")] {
        if let Ok(entries) = fs::read_dir(dir) { retained.extend(entries.flatten().filter_map(|e| e.file_name().into_string().ok())); }
    }
    Ok(Corpus { documents, retained, scanned })
}

/// Metadata-only scan: which sources carry how many distinctive lines. Never the lines.
fn scan(corpus: &Corpus, lines: &BTreeSet<String>, source: &ReplaySource, contract_text: &str) -> Value {
    let mut hits = Vec::new();
    let count = |text: &str| lines.iter().filter(|line| text.contains(line.as_str())).count();
    for (kind, id, text) in &corpus.documents {
        let n = count(text);
        if n > 0 { hits.push(json!({"source": kind, "id": id, "lines": n})); }
    }
    let n = count(contract_text);
    if n > 0 { hits.push(json!({"source": "source_contract", "id": source.task_id, "lines": n})); }
    for attempt in &source.attempt_ids {
        if corpus.retained.contains(attempt) { hits.push(json!({"source": "retained_attempt_worktree", "id": attempt, "lines": null})); }
    }
    let mut scanned = corpus.scanned.clone();
    scanned.insert("source_contract", 1);
    json!({"scanner": SCANNER, "state": if hits.is_empty() { "clean" } else { "contaminated" }, "distinctive_lines": lines.len(), "scanned": scanned, "hits": hits})
}

/// Deliverable and non-goals of a signed contract: what a candidate's contract repeats.
fn contract_prose(raw: &[u8]) -> String {
    let doc: Value = serde_json::from_slice(raw).unwrap_or(Value::Null);
    format!("{}\n{}", doc["deliverable"].as_str().unwrap_or_default(), doc["non_goals"].as_str().unwrap_or_default())
}

fn classification(source: &ReplaySource) -> Value {
    if let Some(recorded) = &source.classification { return recorded.clone(); }
    let scope = ContractScope { revision: source.contract_revision as u64, route: source.route.clone(), write_paths: source.write_paths.clone(),
        write_named_resources: source.write_named_resources.clone() };
    let c = classify_task(&source.task_id, Some(&scope), source.dependencies, 1, 0);
    json!({"class": c.class, "band": c.band, "classifier": TASK_CLASSIFIER, "source": "derived_from_contract"})
}

fn extract(project: &Path, suite: &str) -> Result<Value> {
    label(suite, "suite version")?;
    let (sources, briefs) = {
        let db = read(project)?;
        ensure!(crate::store::replay_suite(&db, suite)?.is_none(), "replay suite {suite} is already recorded: a suite version is immutable");
        (crate::store::replay_sources(&db)?, crate::store::replay_brief_payloads(&db)?)
    };
    let corpus = corpus(project, briefs)?;
    let mut exclusions: BTreeMap<&str, usize> = BTreeMap::from([("no_hidden_check", 0), ("no_solution_paths", 0), ("hidden_check_too_large", 0), ("contaminated", 0)]);
    let mut cases = Vec::new();
    let mut pending_files: Vec<(String, Vec<u8>)> = Vec::new();
    for source in &sources {
        let repo = Path::new(&source.repository);
        let changed = changes(repo, &source.base_oid, &source.reference_oid)?;
        let tests: Vec<&(char, String)> = changed.iter().filter(|(s, p)| p.starts_with(EXPECTED_PREFIX) && p.len() > EXPECTED_PREFIX.len() && matches!(s, 'A' | 'M')).collect();
        let solution: Vec<&String> = changed.iter().filter(|(s, p)| !p.starts_with("tests/") && matches!(s, 'A' | 'M')).map(|(_, p)| p).collect();
        if tests.is_empty() { *exclusions.get_mut("no_hidden_check").unwrap() += 1; continue; }
        if solution.is_empty() { *exclusions.get_mut("no_solution_paths").unwrap() += 1; continue; }
        let mut hidden = Vec::new();
        let mut too_large = false;
        for (index, (_, path)) in tests.iter().enumerate() {
            let bytes = git(repo, &["cat-file", "blob", &format!("{}:{path}", source.reference_oid)])?;
            if bytes.len() > MAX_HIDDEN_BYTES { too_large = true; break; }
            let sha = hex(&bytes);
            hidden.push(json!({"policy_id": format!("hidden-{}", index + 1), "target": &path[EXPECTED_PREFIX.len()..], "sha256": sha, "bytes": bytes.len()}));
            pending_files.push((sha, bytes));
        }
        if too_large || hidden.len() > 8 { *exclusions.get_mut("hidden_check_too_large").unwrap() += 1; continue; }
        // Distinctive content: trimmed added lines of the accepted change that the base does not already hold.
        let mut base_text = String::new();
        for (status, path) in &changed {
            if *status == 'M' { base_text.push_str(&String::from_utf8_lossy(&git(repo, &["cat-file", "blob", &format!("{}:{path}", source.base_oid)])?)); }
        }
        let diff = String::from_utf8_lossy(&git(repo, &["diff", "-U0", "--no-color", "--no-renames", &source.base_oid, &source.reference_oid])?).into_owned();
        let lines: BTreeSet<String> = diff.lines().filter(|l| l.starts_with('+') && !l.starts_with("+++")).map(|l| l[1..].trim().to_owned())
            .filter(|l| l.len() >= MIN_LINE && !base_text.contains(l.as_str())).collect();
        let contamination = scan(&corpus, &lines, source, &contract_prose(&source.contract_raw));
        let contaminated = contamination["state"] == "contaminated";
        if contaminated { *exclusions.get_mut("contaminated").unwrap() += 1; }
        let classification = classification(source);
        let stratum = classification["class"].as_str().unwrap_or("unknown").to_owned();
        let hidden = Value::Array(hidden);
        cases.push(ReplayCaseRecord {
            case_id: format!("{}.r{}", source.task_id, source.contract_revision), ordinal: cases.len() as i64 + 1, source_task_id: source.task_id.clone(),
            source_submission_id: source.submission_id.clone(), source_result_id: source.result_id.clone(), contract_revision: source.contract_revision,
            contract_digest: source.contract_digest.clone(), repository: source.repository.clone(), object_format: source.object_format.clone(),
            base_oid: source.base_oid.clone(), reference_oid: source.reference_oid.clone(), integrated_oid: source.integrated_oid.clone(),
            hidden_check_ref: format!("sha256:{}", hex(hidden.to_string().as_bytes())), hidden_checks: hidden, solution_paths: json!(solution),
            classification, stratum, contamination, status: if contaminated { "contaminated" } else { "eligible" }.into(),
        });
    }
    let exclusions = json!({"sources": sources.len(), "no_hidden_check": exclusions["no_hidden_check"], "no_solution_paths": exclusions["no_solution_paths"],
        "hidden_check_too_large": exclusions["hidden_check_too_large"], "contaminated": exclusions["contaminated"]});
    ensure!(!cases.is_empty(), "no accepted task yields a replay case: {exclusions}");
    // Hidden checks: private, content-addressed, outside the project.
    let checks = replay_dir(project)?.join("checks");
    private_dir(&checks)?;
    for (sha, bytes) in pending_files {
        let path = checks.join(&sha);
        if crate::verification::hidden_digest(&path).as_deref() == Some(sha.as_str()) { continue; }
        let temporary = checks.join(format!(".{sha}.tmp"));
        fs::write(&temporary, &bytes)?;
        fs::set_permissions(&temporary, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        fs::rename(&temporary, &path)?;
    }
    let manifest = json!({"suite_version": suite, "extractor": EXTRACTOR, "exclusions": exclusions, "cases": cases});
    let record = ReplaySuiteRecord { suite_version: suite.into(), extractor: EXTRACTOR.into(), manifest_digest: format!("sha256:{}", hex(manifest.to_string().as_bytes())), exclusions };
    let _guard = migration::runtime_mutation(project)?;
    let seq = migration::open_active(project)?.record_replay_suite(&record, &cases, REPLAY_PRINCIPAL, now())?;
    Ok(json!({"seq": seq, "suite": record, "cases": cases}))
}

/// Eligible, unretired cases of `suite` and the retired ids.
fn eligible(db: &Connection, suite: &str) -> Result<(Vec<ReplayCaseRecord>, BTreeSet<String>)> {
    let (_, cases) = crate::store::replay_suite(db, suite)?.with_context(|| format!("no replay suite {suite}"))?;
    let retired: BTreeSet<String> = crate::store::replay_retirements(db, suite)?.into_iter().map(|r| r.0).collect();
    Ok((cases.into_iter().filter(|c| c.status == "eligible" && !retired.contains(&c.case_id)).collect(), retired))
}

fn parse_subset(subset: &str) -> Result<usize> {
    let n = subset.strip_prefix("stratified:").and_then(|n| n.parse::<usize>().ok()).context("subset must be stratified:N")?;
    ensure!((1..=256).contains(&n), "a stratified subset has 1 to 256 cases");
    Ok(n)
}

/// Reproducible stratified draw: each stratum (classification) is ordered by
/// sha256(seed, suite, case); strata take turns in name order until N.
fn select(suite: &str, cases: &[ReplayCaseRecord], n: usize, seed: &str) -> Result<Vec<String>> {
    ensure!(!seed.is_empty() && seed.len() <= 64 && !seed.chars().any(char::is_control), "seed must be 1 to 64 plain characters");
    let mut strata: BTreeMap<&str, Vec<(String, &str)>> = BTreeMap::new();
    for case in cases { strata.entry(&case.stratum).or_default().push((hex(format!("{seed}\0{suite}\0{}", case.case_id).as_bytes()), &case.case_id)); }
    let mut queues: Vec<std::collections::VecDeque<&str>> = strata.into_values().map(|mut v| { v.sort(); v.into_iter().map(|(_, c)| c).collect() }).collect();
    let mut chosen = Vec::new();
    while chosen.len() < n && queues.iter().any(|q| !q.is_empty()) {
        for queue in &mut queues {
            if chosen.len() == n { break; }
            if let Some(case) = queue.pop_front() { chosen.push(case.to_owned()); }
        }
    }
    Ok(chosen)
}

fn show(project: &Path, suite: &str) -> Result<Value> {
    let db = read(project)?;
    let (record, cases) = crate::store::replay_suite(&db, suite)?.with_context(|| format!("no replay suite {suite}"))?;
    let retired: Vec<Value> = crate::store::replay_retirements(&db, suite)?.into_iter().map(|(case, reason, seq)| json!({"case_id": case, "reason": reason, "seq": seq})).collect();
    let runs: Vec<Value> = crate::store::replay_runs(&db, suite)?.into_iter().map(|(seq, run, configuration, subset, seed, cases, recorded)|
        json!({"seq": seq, "run_id": run, "configuration": configuration, "subset": subset, "seed": seed, "cases": cases, "recorded_unix_ms": recorded})).collect();
    Ok(json!({"suite": record, "cases": cases, "retired": retired, "runs": runs}))
}

/// Concrete bounded runner: inherited routine ownership survives owner death
/// until Git exits; timeout/cancellation kills and reaps its process group.
fn repository_git(repo: &Path, args: &[&str], input: Option<&Path>, output: Option<&Path>, control: &crate::store::controlled::ReadControl, locks: &[crate::runner::InheritedLock]) -> Result<()> {
    use crate::runner::{Cmd, RealRunner, Runner};
    control.check()?;
    let timeout = control.deadline().saturating_duration_since(std::time::Instant::now());
    let mut command = if let Some(input) = input {
        Cmd::new("/bin/sh", timeout).args(["-c", "input=$1; shift; exec /usr/bin/git \"$@\" < \"$input\"", "replay-git", input.to_str().context("input path")?])
    } else { Cmd::new("/usr/bin/git", timeout) };
    command = command.args(["-c", "core.hooksPath=/dev/null", "-C", repo.to_str().context("repository path")?]).args(args.iter().copied())
        .env("PATH", "/usr/bin:/bin").env("HOME", "/").env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_TERMINAL_PROMPT", "0");
    command.env_clear = true;
    command.deadline = Some(control.deadline());
    command.cancellation = Some(control.cancellation());
    command.inherited_locks = locks.to_vec();
    command.capture_limit = 4096;
    if let Some(output) = output { command.stdout_file = Some((output.to_owned(), 128 * 1024 * 1024)); }
    let result = RealRunner.run(&command)?;
    ensure!(result.success(), "replay repository Git failed or exceeded execution budget");
    control.check()?;
    Ok(())
}

/// A replay repository at `base`: only the base commit's history, so no later
/// change (the accepted solution and its tests included) is reachable.
fn replay_repository(source: &Path, dest: &Path, format: &str, base: &str, control: &crate::store::controlled::ReadControl, locks: &[crate::runner::InheritedLock]) -> Result<PathBuf> {
    ensure!(!dest.exists(), "replay repository already exists");
    let parent = dest.parent().context("replay repository has no parent")?;
    private_dir(parent)?;
    let staging = parent.join(format!(".{}.staging", dest.file_name().and_then(|n| n.to_str()).unwrap_or("repo")));
    if staging.exists() { fs::remove_dir_all(&staging)?; }
    repository_git(parent, &["init", "-q", &format!("--object-format={format}"), "-b", "main", staging.to_str().context("path")?], None, None, control, locks)?;
    let wanted = staging.join(".git/replay-wanted");
    fs::write(&wanted, format!("{base}\n"))?;
    let pack = staging.join(".git/replay.pack");
    repository_git(source, &["pack-objects", "--revs", "--stdout", "-q"], Some(&wanted), Some(&pack), control, locks)?;
    repository_git(&staging, &["unpack-objects", "-q"], Some(&pack), None, control, locks)?;
    fs::remove_file(&pack)?;
    fs::remove_file(&wanted)?;
    repository_git(&staging, &["update-ref", "refs/heads/main", base], None, None, control, locks)?;
    repository_git(&staging, &["reset", "-q", "--hard", "main"], None, None, control, locks)?;
    fs::rename(&staging, dest)?;
    Ok(dest.canonicalize()?)
}

struct RunStaging { path: PathBuf, retained: bool }
impl RunStaging {
    fn new(parent: &Path) -> Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        loop {
            let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = parent.join(format!(".run-{}-{id}", std::process::id()));
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self { path, retained: false }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
    }
}
impl Drop for RunStaging {
    fn drop(&mut self) { if !self.retained { let _ = fs::remove_dir_all(&self.path); } }
}

fn run_suite(project: &Path, suite: &str, configuration: &str, subset: &str, seed: &str, expected_head: u64) -> Result<Value> {
    let control = crate::store::controlled::ReadControl::new(std::time::Instant::now() + std::time::Duration::from_secs(600), Default::default());
    let cases = draw_run(project, suite, configuration, subset, seed, &control)?;
    let repos = replay_dir(project)?.join("repos").join(suite);
    private_dir(&repos)?;
    // Atomic directory creation keeps simultaneous draws independent. Drop
    // removes all repositories on Git, lock, head-check or store failure.
    let mut staging = RunStaging::new(&repos)?;
    let repositories = cases.iter().map(|case| replay_repository(Path::new(&case.repository), &staging.path.join(&case.case_id), &case.object_format, &case.base_oid, &control, &[])).collect::<Result<Vec<_>>>()?;
    let result = {
        let _guard = migration::runtime_mutation(project)?;
        let mut db = migration::open_active(project)?;
        db.atomic_replay(|db| record_run(db, suite, configuration, subset, seed, expected_head, &cases, &control,
            |index, _, _| Ok(repositories[index].clone())))?
    };
    staging.retained = true;
    Ok(result)
}

/// Caller must hold project mutation ownership (signed routine execution).
#[allow(clippy::too_many_arguments)] // Mirrors the replay CLI plus owned store and execution budget.
pub(crate) fn run_suite_owned(project: &Path, db: &mut crate::store::SqliteStore, suite: &str, configuration: &str, subset: &str, seed: &str, expected_head: u64, control: &crate::store::controlled::ReadControl, locks: &[crate::runner::InheritedLock]) -> Result<Value> {
    let cases = draw_run(project, suite, configuration, subset, seed, control)?;
    let repos = replay_dir(project)?.join("repos").join(suite);
    record_run(db, suite, configuration, subset, seed, expected_head, &cases, control, |_, seq, case| {
        replay_repository(Path::new(&case.repository), &repos.join(seq.to_string()).join(&case.case_id), &case.object_format, &case.base_oid, control, locks)
    })
}

fn draw_run(project: &Path, suite: &str, configuration: &str, subset: &str, seed: &str, control: &crate::store::controlled::ReadControl) -> Result<Vec<ReplayCaseRecord>> {
    control.check()?;
    label(configuration, "configuration label")?;
    let n = parse_subset(subset)?;
    let (cases, _) = eligible(&*read(project)?, suite)?;
    let chosen = select(suite, &cases, n, seed)?;
    ensure!(!chosen.is_empty(), "replay suite {suite} has no eligible case");
    let chosen_cases: Vec<ReplayCaseRecord> = chosen.iter().map(|id| cases.iter().find(|c| &c.case_id == id).unwrap().clone()).collect();
    // The source repository holds the accepted change and its tests: each
    // candidate's own sandbox hides it (and the check store) at launch,
    // derived from this registry (`canonical_worker::resources::launch_hides`),
    // so ordinary launches on that repository are unaffected.
    for case in &chosen_cases {
        control.check()?;
        Path::new(&case.repository).canonicalize().with_context(|| format!("source repository of case {} is unavailable", case.case_id))?;
        for check in case.hidden_checks.as_array().into_iter().flatten() {
            let sha = check["sha256"].as_str().unwrap_or_default();
            ensure!(crate::verification::hidden_digest(&check_path(project, sha)?).as_deref() == Some(sha),
                "hidden check {sha} of case {} no longer applies: retire the case", case.case_id);
        }
    }
    Ok(chosen_cases)
}

#[allow(clippy::too_many_arguments)]
fn record_run(db: &mut crate::store::SqliteStore, suite: &str, configuration: &str, subset: &str, seed: &str, expected_head: u64, chosen_cases: &[ReplayCaseRecord], control: &crate::store::controlled::ReadControl,
    mut repository_for: impl FnMut(usize, i64, &ReplayCaseRecord) -> Result<PathBuf>) -> Result<Value> {
    let chosen: Vec<String> = chosen_cases.iter().map(|case| case.case_id.clone()).collect();
    let head = db.read_snapshot(None)?.head;
    ensure!(head == expected_head, "project head is {head}, expected {expected_head}");
    let run_id = format!("sha256:{}", hex(json!({"suite": suite, "configuration": configuration, "subset": subset, "seed": seed, "cases": chosen, "head": head}).to_string().as_bytes()));
    control.check()?;
    let seq = db.record_replay_run(&run_id, suite, configuration, subset, seed, &chosen, REPLAY_PRINCIPAL, now())?;
    let mut head = head;
    let mut tasks = Vec::new();
    for (index, case) in chosen_cases.iter().enumerate() {
        control.check()?;
        // One repository per run and case: a candidate imported by one run is never visible to another.
        let repository = repository_for(index, seq, case)?;
        let task = TaskId::new(format!("replay-{suite}-{seq}-{}", index + 1)).map_err(anyhow::Error::msg)?;
        head = db.commit(crate::domain::Commit { expected_head: head, mutations: vec![crate::domain::Mutation::Task { expected: None, next: crate::domain::Task { id: task.clone(), revision: 1, state: crate::domain::TaskState::Draft, title: format!("Replay {suite} {} ({configuration})", case.case_id), active_attempt: None } }] })?;
        db.register_replay_candidate(task.as_str(), &run_id, suite, &case.case_id, &repository.display().to_string(), REPLAY_PRINCIPAL, now())?;
        let request: crate::domain::QueueRequest = serde_json::from_value(json!({"priority": 0, "dependencies": []}))?;
        head = db.queue_task(&task, 1, head, &request, now())?;
        tasks.push(json!({"task_id": task.as_str(), "case_id": case.case_id, "stratum": case.stratum, "repository": repository, "base_oid": case.base_oid}));
    }
    Ok(json!({"seq": seq, "run_id": run_id, "suite_version": suite, "configuration": configuration, "subset": subset, "seed": seed, "cases": chosen, "tasks": tasks, "head": head}))
}

/// The owner-signable contract of a replay task: the source contract's
/// deliverable, scope and profile over the replay repository at the base
/// commit, verify-only, with one acceptance policy per hidden check. A
/// policy names its hidden file by path and digest, never by content.
fn contract(project: &Path, task: &str, output: &Path) -> Result<Value> {
    let db = read(project)?;
    let (_, suite, case_id, repository) = crate::store::replay_candidate(&db, task)?.with_context(|| format!("task {task} is not a replay candidate"))?;
    let (_, cases) = crate::store::replay_suite(&db, &suite)?.context("replay suite missing")?;
    let case = cases.into_iter().find(|c| c.case_id == case_id).context("replay case missing")?;
    let raw = crate::store::replay_contract_raw(&db, &case.source_task_id, case.contract_revision)?.context("source contract missing")?;
    let source: Value = serde_json::from_slice(&raw).context("source contract is not JSON")?;
    let head = crate::runtime::snapshot(project)?.head;
    drop(db);
    let mut policies = Vec::new();
    for check in case.hidden_checks.as_array().into_iter().flatten() {
        let sha = check["sha256"].as_str().context("hidden check digest")?;
        let path = check_path(project, sha)?.display().to_string();
        let target = check["target"].as_str().context("hidden check target")?;
        let body = json!({"version": 1, "checks": ["/usr/bin/git", "diff", "--no-index", "--quiet", "--", path, target], "hidden": [{"path": path, "sha256": sha}]});
        policies.push(json!({"id": check["policy_id"], "text": body.to_string()}));
    }
    let store = project.join(".state/state.db").canonicalize()?.display().to_string();
    let mut document = json!({"version": source["version"], "project_store": store, "expected_head": head, "task_id": task, "contract_revision": 1,
        "deliverable": source["deliverable"], "non_goals": format!("{} Replay evaluation candidate ({suite}/{case_id}): verified by hidden checks, never integrated.",
            source["non_goals"].as_str().unwrap_or_default()),
        "acceptance_policies": policies, "repository": repository, "base_oid": case.base_oid, "object_format": case.object_format, "dependencies": [],
        "capability_flags": source["capability_flags"], "profile_kind": source["profile_kind"], "retry_class": source["retry_class"],
        "result_schema_id": source["result_schema_id"], "route": "verify_only", "authority": crate::authority::policy_reference(project)?});
    if !source["scope"].is_null() { document["scope"] = source["scope"].clone(); }
    let targets: Vec<Value> = case.hidden_checks.as_array().into_iter().flatten().map(|c| json!({"path": c["target"], "kind": "git_file"})).collect();
    document["outputs"] = match source["outputs"].as_array() {
        Some(outputs) => Value::Array(outputs.iter().filter(|o| !o["path"].as_str().unwrap_or_default().starts_with("tests/")).cloned().collect()),
        None => Value::Array(targets),
    };
    let mut bytes = serde_json::to_vec_pretty(&document)?;
    bytes.push(b'\n');
    fs::write(output, &bytes).with_context(|| format!("write {}", output.display()))?;
    Ok(json!({"task_id": task, "suite_version": suite, "case_id": case_id, "output": output, "expected_head": head, "digest": hex(&bytes),
        "policies": document["acceptance_policies"].as_array().map(|p| p.iter().map(|p| p["id"].clone()).collect::<Vec<_>>())}))
}

fn ratio(passed: usize, attempted: usize) -> Value { if attempted == 0 { Value::Null } else { json!(format!("{passed}/{attempted}")) } }

/// Per-configuration M49 over `suite`: replay cases whose candidate passed
/// every hidden check / cases attempted (passed + failed). Pending, not
/// launched and retired candidates are outside the denominator, counted.
pub fn report(db: &Connection, suite: &str, since: Option<i64>) -> Result<Value> {
    let (record, cases) = crate::store::replay_suite(db, suite)?.with_context(|| format!("no replay suite {suite}"))?;
    let retired = crate::store::replay_retirements(db, suite)?.len();
    let statuses = crate::store::replay_candidate_statuses(db, Some(suite))?;
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    // configuration id -> (run labels, count per status, (passed, attempted) per stratum)
    type Cell<'a> = (BTreeSet<String>, BTreeMap<&'a str, usize>, BTreeMap<String, (usize, usize)>);
    let mut by: BTreeMap<String, Cell> = BTreeMap::new();
    for s in &statuses {
        if since.is_some_and(|since| s.decided_unix_ms.is_none_or(|at| at < since)) { *counts.entry("outside_window").or_default() += 1; continue; }
        let Some(configuration) = &s.configuration_id else { *counts.entry(s.status.as_str()).or_default() += 1; continue };
        if s.status == "retired" { *counts.entry("retired").or_default() += 1; continue; }
        let entry = by.entry(configuration.clone()).or_default();
        entry.0.insert(s.configuration.clone());
        *entry.1.entry(s.status.as_str()).or_default() += 1;
        let stratum = entry.2.entry(s.stratum.clone()).or_default();
        if s.status == "passed" { stratum.0 += 1; }
        if s.status != "pending" { stratum.1 += 1; }
    }
    let configurations: Vec<Value> = by.iter().map(|(id, (labels, status, strata))| {
        let (passed, failed, pending) = (status.get("passed").copied().unwrap_or(0), status.get("failed").copied().unwrap_or(0), status.get("pending").copied().unwrap_or(0));
        json!({"configuration_id": id, "labels": labels, "suite_version": suite, "numerator": passed, "denominator": passed + failed, "value": ratio(passed, passed + failed),
            "n": passed + failed, "failed": failed, "pending": pending,
            "by_stratum": strata.iter().map(|(k, (p, a))| (k.clone(), json!({"numerator": p, "denominator": a, "value": ratio(*p, *a)}))).collect::<BTreeMap<_, _>>()})
    }).collect();
    Ok(json!({"metric": "M49", "definition": DEFINITION, "suite_version": suite, "extractor": record.extractor, "manifest_digest": record.manifest_digest,
        "cases": {"recorded": cases.len(), "eligible": cases.iter().filter(|c| c.status == "eligible").count(), "retired": retired},
        "exclusions": {"extraction": record.exclusions, "contaminated_cases": cases.iter().filter(|c| c.status == "contaminated").count(), "retired_cases": retired,
            "candidates_not_launched": counts.get("not_launched").copied().unwrap_or(0), "candidates_retired": counts.get("retired").copied().unwrap_or(0),
            "candidates_outside_window": counts.get("outside_window").copied().unwrap_or(0)},
        "uncertainty": {"method": "raw_with_n", "reason": "no shared bootstrap estimator on this branch; each rate carries its n"},
        "configurations": configurations}))
}

/// The M49 body `telemetry report` serves (central provider): the latest suite version's report, pooled value on top.
pub fn m49(db: &Connection, since: Option<i64>) -> Result<Value> {
    let present: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='replay_suites')", [], |r| r.get(0))?;
    let latest: Option<String> = if present { db.query_row("SELECT suite_version FROM replay_suites ORDER BY seq DESC LIMIT 1", [], |r| r.get(0)).ok() } else { None };
    let Some(suite) = latest else {
        return Ok(json!({"definition": DEFINITION, "name": "replay_suite_pass_rate", "value": {"status": "unavailable", "reason": "no_replay_suite"}}));
    };
    let mut body = report(db, &suite, since)?;
    let (passed, attempted) = body["configurations"].as_array().into_iter().flatten()
        .fold((0, 0), |(p, a), c| (p + c["numerator"].as_u64().unwrap_or(0), a + c["denominator"].as_u64().unwrap_or(0)));
    body["name"] = json!("replay_suite_pass_rate");
    body["numerator"] = json!(passed);
    body["denominator"] = json!(attempted);
    body["value"] = ratio(passed as usize, attempted as usize);
    if attempted == 0 { body["reason"] = json!("empty_denominator"); }
    body["excluded"] = body["exclusions"].clone();
    Ok(body)
}

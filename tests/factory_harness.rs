//! Deterministic factory harness. Logical time, a fake runner, and disposable
//! git repos only — no network, agent, or slept timing. Latency bars are not frozen.

#![cfg(feature = "state-store")]

use anyhow::Result;
use herdr_projects::{
    domain::*,
    runner::{Cmd, Output, Runner},
    store::SqliteStore,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    fmt::Write,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

// `verify` execs `current_exe` inside `unshare`. This test binary is that
// executable, and the cargo harness has no verification-setup command.
#[cfg(target_os = "linux")]
#[used]
#[unsafe(link_section = ".init_array")]
static VERTICAL_SLICE_VERIFY_HOOK: unsafe extern "C" fn() = vertical_slice_verify_hook;

#[cfg(target_os = "linux")]
unsafe extern "C" fn vertical_slice_verify_hook() {
    if std::env::args().any(|arg| arg == "verification-setup") {
        std::process::exit(herdr_projects::verification::setup_main());
    }
}

const SEED: u64 = 7;
const EVENTS: usize = 1000;
const QUEUE_WRITES: usize = 2;
const EVENTS_PER_QUEUE: usize = 2;
const TASKS: usize = EVENTS - QUEUE_WRITES * EVENTS_PER_QUEUE;
const LOGICAL_DEADLINE_MS: i64 = 60_000;
const FORGED_RECEIPT: &str = "forged-lost-reply-receipt";

const ABSENT_TABLES: &[&str] = &[
    "dependency_satisfactions",
    "verified_results",
    "verification_receipts",
    "integrated_commits",
    "feedback_items",
];
const TRUSTED_EMPTY: &[&str] = &[
    "acceptance_policies",
    "task_contracts",
    "result_submissions",
    "result_objects",
    "memory_update_receipts",
    "review_decisions",
    "memory_promotions",
    "proposal_validations",
    "approval_grants",
    "approval_uses",
    "approval_revocations",
    "migration_receipt",
];

struct SeededClock {
    origin_ms: i64,
    now_ms: i64,
}

impl SeededClock {
    fn from_seed(seed: u64) -> Self {
        let origin_ms = i64::try_from(seed % 86_400_000).unwrap();
        Self {
            origin_ms,
            now_ms: origin_ms,
        }
    }

    fn now(&self) -> i64 {
        self.now_ms
    }

    /// Admission and reply deadlines only. This does not sleep.
    fn advance_to_deadline(&mut self) -> i64 {
        self.now_ms = self.now_ms.checked_add(LOGICAL_DEADLINE_MS).unwrap();
        self.now_ms
    }
}

struct FakeRunner {
    calls: RefCell<Vec<String>>,
}

impl FakeRunner {
    fn new() -> Self {
        Self {
            calls: RefCell::new(Vec::new()),
        }
    }
}

impl Runner for FakeRunner {
    fn run(&self, cmd: &Cmd) -> Result<Output> {
        let line = cmd.display();
        self.calls.borrow_mut().push(line.clone());
        if line.contains("update-ref") {
            return Ok(Output {
                timed_out: true,
                stdout: FORGED_RECEIPT.to_string(),
                stdout_bytes: FORGED_RECEIPT.as_bytes().to_vec(),
                ..Output::default()
            });
        }
        anyhow::bail!("FakeRunner: no rule for `{line}`");
    }

    fn socket_request(&self, _socket: &Path, _line: &str, _timeout: Duration) -> Result<String> {
        anyhow::bail!("FakeRunner: factory harness opens no sockets");
    }
}

#[derive(Debug, PartialEq, Eq, Serialize)]
struct Limits {
    events: usize,
    latency: &'static str,
    provisional_5s_15s_250ms: &'static str,
    tick_250ms: &'static str,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
struct Measurement {
    seed: u64,
    clock_origin_ms: i64,
    logical_deadline_ms: i64,
    repo_commit: String,
    schema_version: u32,
    events: usize,
    tasks: usize,
    queued: usize,
    dependency_edges: usize,
    queue_report_input_rows: usize,
    queue_report_head: u64,
    launch_enabled: bool,
    max_active_workers: u32,
    queue_entries: Vec<String>,
    read_snapshot_events: usize,
    read_snapshot_tasks: usize,
    read_snapshot_head: u64,
    lost_reply: &'static str,
    runner_calls: Vec<String>,
    trusted_receipt: bool,
    trusted_rows_added: i64,
    satisfaction_table: &'static str,
    verified_results_table: &'static str,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
struct Manifest {
    git_sha: String,
    cargo_lock_sha256: String,
    schema_version: u32,
    rustc: String,
    os: String,
    rusqlite: String,
    seed: u64,
    limits: Limits,
    measurement: Measurement,
    decision_log: String,
}

fn task_id(index: usize) -> TaskId {
    TaskId::new(format!("t-{index:04}")).unwrap()
}

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "factory")
        .env("GIT_AUTHOR_EMAIL", "factory@example.invalid")
        .env("GIT_AUTHOR_DATE", "2020-01-01T00:00:00Z")
        .env("GIT_COMMITTER_NAME", "factory")
        .env("GIT_COMMITTER_EMAIL", "factory@example.invalid")
        .env("GIT_COMMITTER_DATE", "2020-01-01T00:00:00Z")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} status {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn build_repo(root: &Path, seed: u64) -> (PathBuf, String) {
    let repo = root.join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "factory", "--template="]);
    fs::write(
        repo.join("README"),
        format!("factory-fixture\nseed={seed}\n"),
    )
    .unwrap();
    git(&repo, &["add", "--", "README"]);
    git(
        &repo,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "fixture",
        ],
    );
    let commit = git(&repo, &["rev-parse", "HEAD"]).trim().to_string();
    assert_eq!(commit.len(), 40, "{commit}");
    (repo, commit)
}

fn workspace_git_sha() -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(env!("CARGO_MANIFEST_DIR"))
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn cargo_lock_sha256() -> String {
    let bytes = fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.lock")).unwrap();
    format!("{:x}", Sha256::digest(bytes))
}

fn rustc_version() -> String {
    let output = Command::new("rustc").arg("--version").output().unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn open_readonly(path: &Path) -> rusqlite::Connection {
    let db = rusqlite::Connection::open(path).unwrap();
    db.execute_batch("PRAGMA query_only=ON;").unwrap();
    db
}

fn row_counts(path: &Path) -> BTreeMap<String, i64> {
    let db = open_readonly(path);
    let mut stmt = db
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .unwrap();
    let names: Vec<String> = stmt
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(|name| name.unwrap())
        .collect();
    drop(stmt);
    let mut counts = BTreeMap::new();
    for name in names {
        assert!(
            name.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'),
            "{name}"
        );
        let count: i64 = db
            .query_row(&format!("SELECT count(*) FROM \"{name}\""), [], |row| {
                row.get(0)
            })
            .unwrap();
        counts.insert(name, count);
    }
    counts
}

fn schema_version(path: &Path) -> u32 {
    let db = open_readonly(path);
    let user_version: u32 = db
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    let stored: u32 = db
        .query_row(
            "SELECT schema_version FROM store_meta WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(user_version, stored);
    user_version
}

fn database_bytes(path: &Path) -> Vec<u8> {
    let mut bytes = fs::read(path).unwrap();
    for suffix in ["-wal", "-shm"] {
        let mut extra = path.as_os_str().to_os_string();
        extra.push(suffix);
        if let Ok(more) = fs::read(PathBuf::from(extra)) {
            bytes.extend(more);
        }
    }
    bytes
}

/// A timed-out reply is not a confirmed result. The forged body must not become a receipt.
fn inject_lost_reply(runner: &FakeRunner, repo: &Path, db_path: &Path) -> bool {
    let output = runner
        .run(
            &Cmd::new("git", Duration::from_secs(1))
                .args(["update-ref", "refs/factory/integrate", "lost"])
                .cwd(repo),
        )
        .unwrap();
    assert!(output.timed_out);
    assert!(output.stdout.contains(FORGED_RECEIPT));
    let before = database_bytes(db_path);
    assert!(
        !before
            .windows(FORGED_RECEIPT.len())
            .any(|window| window == FORGED_RECEIPT.as_bytes())
    );
    let after = database_bytes(db_path);
    assert_eq!(before, after);
    false
}

fn entry_line(entry: &QueueEntry) -> String {
    format!(
        "{} revision={} priority={} blockers={}",
        entry.task.as_str(),
        entry.task_revision,
        entry.effective_priority,
        entry.blockers.join(",")
    )
}

fn decision_log(measurement: &Measurement) -> String {
    let mut log = String::new();
    writeln!(log, "seed={}", measurement.seed).unwrap();
    writeln!(log, "clock_origin_ms={}", measurement.clock_origin_ms).unwrap();
    writeln!(
        log,
        "logical_deadline_ms={}",
        measurement.logical_deadline_ms
    )
    .unwrap();
    writeln!(log, "repo_commit={}", measurement.repo_commit).unwrap();
    writeln!(log, "schema={}", measurement.schema_version).unwrap();
    writeln!(log, "events={}", measurement.events).unwrap();
    writeln!(log, "tasks={}", measurement.tasks).unwrap();
    writeln!(log, "queued={}", measurement.queued).unwrap();
    writeln!(log, "dependency_edges={}", measurement.dependency_edges).unwrap();
    writeln!(
        log,
        "queue_report_input_rows={}",
        measurement.queue_report_input_rows
    )
    .unwrap();
    writeln!(log, "queue_report_head={}", measurement.queue_report_head).unwrap();
    writeln!(
        log,
        "queue_report_launch_enabled={}",
        measurement.launch_enabled
    )
    .unwrap();
    writeln!(
        log,
        "queue_report_max_active_workers={}",
        measurement.max_active_workers
    )
    .unwrap();
    for entry in &measurement.queue_entries {
        writeln!(log, "queue_report_entry={entry}").unwrap();
    }
    writeln!(
        log,
        "read_snapshot_events={}",
        measurement.read_snapshot_events
    )
    .unwrap();
    writeln!(
        log,
        "read_snapshot_tasks={}",
        measurement.read_snapshot_tasks
    )
    .unwrap();
    writeln!(log, "read_snapshot_head={}", measurement.read_snapshot_head).unwrap();
    writeln!(log, "lost_reply={}", measurement.lost_reply).unwrap();
    writeln!(log, "runner_calls={}", measurement.runner_calls.join("|")).unwrap();
    writeln!(log, "trusted_receipt={}", measurement.trusted_receipt).unwrap();
    writeln!(log, "trusted_rows_added={}", measurement.trusted_rows_added).unwrap();
    writeln!(log, "satisfaction_table={}", measurement.satisfaction_table).unwrap();
    writeln!(
        log,
        "verified_results_table={}",
        measurement.verified_results_table
    )
    .unwrap();
    log
}

fn write_manifest(dir: &Path, manifest: &Manifest) -> serde_json::Value {
    let path = dir.join("factory-harness-manifest.json");
    let text = serde_json::to_vec_pretty(manifest).unwrap();
    fs::write(&path, &text).unwrap();
    let read = fs::read(&path).unwrap();
    assert_eq!(read, text);
    serde_json::from_slice(&read).unwrap()
}

fn seed_store(db: &mut SqliteStore, now: i64) {
    let mutations = (0..TASKS)
        .map(|index| Mutation::Task {
            expected: None,
            next: Task {
                id: task_id(index),
                revision: 1,
                state: TaskState::Draft,
                title: format!("task {index}"),
                active_attempt: None,
            },
        })
        .collect();
    let mut head = db
        .commit(Commit {
            expected_head: 0,
            mutations,
        })
        .unwrap();
    assert_eq!(head, TASKS as u64);
    head = db
        .queue_task(
            &task_id(0),
            1,
            head,
            &QueueRequest {
                priority: 0,
                dependencies: Vec::new(),
            },
            now,
        )
        .unwrap();
    assert_eq!(head, TASKS as u64 + EVENTS_PER_QUEUE as u64);
    head = db
        .queue_task(
            &task_id(1),
            1,
            head,
            &QueueRequest {
                priority: 0,
                dependencies: vec![Dependency {
                    predecessor: task_id(0),
                    requirement: DependencyRequirement::VerifiedResult,
                }],
            },
            now,
        )
        .unwrap();
    assert_eq!(head, EVENTS as u64);
}

fn assert_unchanged_store(before: &BTreeMap<String, i64>, after: &BTreeMap<String, i64>) {
    assert_eq!(before, after);
    assert_eq!(before.get("events").copied(), Some(EVENTS as i64));
    for table in ABSENT_TABLES {
        assert_eq!(before.get(*table), None, "{table}");
    }
    for table in TRUSTED_EMPTY {
        assert_eq!(before.get(*table).copied(), Some(0), "{table}");
    }
    for name in before.keys() {
        let lower = name.to_ascii_lowercase();
        assert!(!lower.contains("satisfaction"), "{name}");
        assert!(!lower.contains("verified_result"), "{name}");
        assert!(!lower.contains("verification_receipt"), "{name}");
    }
}

fn run(seed: u64) -> (String, serde_json::Value) {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, repo_commit) = build_repo(tmp.path(), seed);
    let db_path = tmp.path().join("state.db");
    let mut clock = SeededClock::from_seed(seed);
    let mut db = SqliteStore::create(&db_path).unwrap();
    seed_store(&mut db, clock.now());

    let report = db.queue_report(clock.now()).unwrap();
    let snapshot = db.read_snapshot(None).unwrap();
    drop(db);

    let scheduler = snapshot.scheduler.as_ref().unwrap();
    let dependency_edges = scheduler
        .queue
        .iter()
        .map(|record| record.dependencies.len())
        .sum::<usize>();
    // Count input rows. Wall-clock latency is not a gate; 5 s / 15 s / 250 ms stay unfrozen.
    let queue_report_input_rows = snapshot.tasks.len()
        + snapshot.attempts.len()
        + scheduler.queue.len()
        + dependency_edges
        + snapshot.budget_policies.len()
        + usize::from(snapshot.control.is_some());
    assert_eq!(snapshot.events.len(), EVENTS);
    assert_eq!(snapshot.tasks.len(), TASKS);
    assert_eq!(scheduler.queue.len(), QUEUE_WRITES);
    assert_eq!(dependency_edges, 1);
    assert_eq!(queue_report_input_rows, EVENTS);
    assert_eq!(snapshot.attempts.len(), 0);
    assert!(snapshot.routine_receipts.is_empty());
    assert!(snapshot.approvals.is_empty());
    assert_eq!(snapshot.head, EVENTS as u64);
    assert_eq!(snapshot.schema_version, 26);
    assert_eq!(report.head, EVENTS as u64);
    assert!(!report.launch_enabled);
    assert_eq!(report.policy.max_active_workers, 0);
    assert_eq!(report.retained_attempts, 0);
    assert_eq!(report.available_slots, 0);
    assert_eq!(report.entries.len(), 2);
    assert_eq!(report.entries[0].task.as_str(), "t-0000");
    assert_eq!(report.entries[1].task.as_str(), "t-0001");
    assert_eq!(
        report.entries[0].blockers,
        vec![
            "project_not_admitted".to_string(),
            "capacity_full".to_string(),
            "owner_signature_not_scheduled".to_string(),
            "launch_reserve_not_scheduled".to_string(),
            "controller_requires_reserved_attempt".to_string(),
        ]
    );
    assert_eq!(
        report.entries[1].blockers,
        vec![
            "project_not_admitted".to_string(),
            "capacity_full".to_string(),
            "verified_dependency_evidence_unavailable:t-0000:verified_result".to_string(),
            "owner_signature_not_scheduled".to_string(),
            "launch_reserve_not_scheduled".to_string(),
            "controller_requires_reserved_attempt".to_string(),
        ]
    );
    assert!(
        report
            .entries
            .iter()
            .all(|entry| entry.task_revision == 2 && entry.effective_priority == 0)
    );

    let before_counts = row_counts(&db_path);
    let schema = schema_version(&db_path);
    assert_eq!(schema, 26);
    let deadline = clock.advance_to_deadline();
    assert_eq!(deadline, clock.origin_ms + LOGICAL_DEADLINE_MS);
    let runner = FakeRunner::new();
    let trusted_receipt = inject_lost_reply(&runner, &repo, &db_path);
    let after_head = git(&repo, &["rev-parse", "HEAD"]).trim().to_string();
    assert_eq!(after_head, repo_commit);
    let after_counts = row_counts(&db_path);
    let trusted_rows_added =
        after_counts.values().sum::<i64>() - before_counts.values().sum::<i64>();
    assert_eq!(trusted_rows_added, 0);
    assert_unchanged_store(&before_counts, &after_counts);
    assert_eq!(schema_version(&db_path), 26);
    let stored = database_bytes(&db_path);
    assert!(
        !stored
            .windows(FORGED_RECEIPT.len())
            .any(|window| window == FORGED_RECEIPT.as_bytes())
    );
    let db = open_readonly(&db_path);
    let forged_events: i64 = db
        .query_row(
            "SELECT count(*) FROM events WHERE payload LIKE ?",
            [format!("%{FORGED_RECEIPT}%")],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(forged_events, 0);
    drop(db);

    let measurement = Measurement {
        seed,
        clock_origin_ms: clock.origin_ms,
        logical_deadline_ms: deadline,
        repo_commit,
        schema_version: schema,
        events: snapshot.events.len(),
        tasks: snapshot.tasks.len(),
        queued: scheduler.queue.len(),
        dependency_edges,
        queue_report_input_rows,
        queue_report_head: report.head,
        launch_enabled: report.launch_enabled,
        max_active_workers: report.policy.max_active_workers,
        queue_entries: report.entries.iter().map(entry_line).collect(),
        read_snapshot_events: snapshot.events.len(),
        read_snapshot_tasks: snapshot.tasks.len(),
        read_snapshot_head: snapshot.head,
        lost_reply: "timed_out",
        runner_calls: runner.calls.borrow().clone(),
        trusted_receipt,
        trusted_rows_added,
        satisfaction_table: "absent",
        verified_results_table: "absent",
    };
    assert!(!measurement.trusted_receipt);
    assert_eq!(
        measurement.runner_calls,
        vec!["git update-ref refs/factory/integrate lost".to_string()]
    );
    let log = decision_log(&measurement);
    let manifest = Manifest {
        git_sha: workspace_git_sha(),
        cargo_lock_sha256: cargo_lock_sha256(),
        schema_version: schema,
        rustc: rustc_version(),
        os: std::env::consts::OS.to_string(),
        rusqlite: rusqlite::version().to_string(),
        seed,
        limits: Limits {
            events: EVENTS,
            latency: "not frozen",
            provisional_5s_15s_250ms: "not frozen",
            tick_250ms: "next_tick_delay while canonical root-exclusive work is pending",
        },
        measurement,
        decision_log: log.clone(),
    };
    let value = write_manifest(tmp.path(), &manifest);
    assert_eq!(value["decision_log"], log);
    assert_eq!(value["limits"]["latency"], "not frozen");
    assert_eq!(value["schema_version"], 26);
    (log, value)
}

fn assert_appendix_unfrozen() {
    let doc = include_str!("../docs/factory/baseline.md");
    let appendix = doc
        .split_once("## Measurement appendix")
        .expect("measurement appendix")
        .1;
    for phrase in [
        "not frozen",
        "next_tick_delay",
        "canonical root-exclusive",
        "1000",
        "launch_enabled",
    ] {
        assert!(appendix.contains(phrase), "appendix missing {phrase}");
    }
    let lower = appendix.to_ascii_lowercase();
    for phrase in [
        "p95 passed",
        "targets met",
        "latency passed",
        "within 250",
        "under 250",
        "p95 <",
        "meets the 5",
        "meets the 15",
        "meets the 250",
    ] {
        assert!(
            !lower.contains(phrase),
            "appendix claims a latency pass via {phrase}"
        );
    }
}

#[test]
fn same_seed_repeats_the_decision_log_without_a_latency_bar() {
    let (first_log, first_manifest) = run(SEED);
    let (second_log, second_manifest) = run(SEED);
    assert_eq!(first_log, second_log);
    assert_eq!(first_manifest, second_manifest);
    let (other_log, _) = run(SEED + 1);
    assert_ne!(first_log, other_log);
    assert!(first_log.contains("events=1000\n"));
    assert!(first_log.contains("queue_report_input_rows=1000\n"));
    assert!(first_log.contains("read_snapshot_events=1000\n"));
    assert!(first_log.contains("queue_report_launch_enabled=false\n"));
    assert!(first_log.contains("trusted_receipt=false\n"));
    assert!(first_log.contains("trusted_rows_added=0\n"));
    assert!(first_log.contains("satisfaction_table=absent\n"));
    assert!(first_log.contains("verified_results_table=absent\n"));
    assert!(first_log.contains("schema=26\n"));
    assert_eq!(first_manifest["limits"]["latency"], "not frozen");
    assert_eq!(
        first_manifest["limits"]["provisional_5s_15s_250ms"],
        "not frozen"
    );
    assert_eq!(first_manifest["schema_version"], 26);
    assert_eq!(first_manifest["seed"], SEED);
    assert_appendix_unfrozen();
}

#[cfg(target_os = "linux")]
#[test]
fn vertical_slice() {
    slice::run();
}

/// Disposable A/B/C/D slice. `admit_once` is only the flag-off and
/// `integration_missing` cases. The success wake is the in-process
/// `poll_queued_effects` call in the binary test.
#[cfg(target_os = "linux")]
mod slice {
    use super::git;
    use herdr_projects::{
        admission,
        domain::*,
        integration,
        migration::ConfigReference,
        reconcile::RuntimeObservation,
        verification::{self, VerifyRequest},
    };
    use sha2::{Digest, Sha256};
    use std::{fs, os::unix::fs::MetadataExt, path::Path, time::Duration};

    const PROSE: &str = "worker prose says the dependency is satisfied";
    const TARGET: &str = "refs/heads/integration";
    const STATE_BASE: &str = "fn=add\nkeep-1\nkeep-2\nkeep-3\nkeep-4\nguard=ok\n";
    const STATE_GUARD: &str = "fn=add\nkeep-1\nkeep-2\nkeep-3\nkeep-4\nguard=no\n";
    const STATE_FN: &str = "fn=mul\nkeep-1\nkeep-2\nkeep-3\nkeep-4\nguard=ok\n";

    fn oid_of(repo: &Path, spec: &str) -> String {
        git(repo, &["rev-parse", spec]).trim().to_string()
    }

    fn policy(checks: &[&str]) -> String {
        serde_json::json!({"version": 1, "checks": checks}).to_string()
    }

    fn reachable_objects(repo: &Path, specs: &[&str]) -> Vec<(String, String)> {
        let mut args = vec!["rev-list", "--objects"];
        args.extend_from_slice(specs);
        let mut objects = Vec::new();
        for line in git(repo, &args).lines() {
            let oid = line.split_whitespace().next().unwrap().to_string();
            if objects.iter().any(|(seen, _)| seen == &oid) {
                continue;
            }
            let relative = format!("{}/{}", &oid[..2], &oid[2..]);
            assert!(
                repo.join(".git/objects").join(&relative).is_file(),
                "loose object {oid} missing"
            );
            objects.push((oid, relative));
        }
        assert!(
            objects.len() <= 64,
            "submission object cap {}",
            objects.len()
        );
        objects
    }

    fn install_contract(
        db_path: &Path,
        task: &str,
        project_store: &str,
        repository: &str,
        base: &str,
        route: &str,
        body: &str,
    ) -> String {
        let raw = serde_json::json!({
            "version": 1,
            "task_id": task,
            "contract_revision": 1,
            "repository": repository,
            "base_oid": base,
            "object_format": "sha1",
            "route": route,
            "acceptance_policies": [{"id": "builds", "text": body}]
        })
        .to_string();
        let digest = format!("{:x}", Sha256::digest(raw.as_bytes()));
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        let sequence: i64 = conn
            .query_row("SELECT MAX(sequence) FROM events", [], |row| row.get(0))
            .unwrap();
        conn.execute(
            "INSERT INTO task_contracts(task_id,contract_revision,plan_revision,project_store,expected_head,repository,base_oid,object_format,memory_snapshot_id,route,raw_bytes,raw_digest,installed_seq) VALUES(?1,1,NULL,?2,0,?3,?4,'sha1',NULL,?5,?6,?7,?8)",
            rusqlite::params![task, project_store, repository, base, route, raw.as_bytes(), digest, sequence],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO acceptance_policies(task_id,contract_revision,policy_id,body) VALUES(?1,1,'builds',?2)",
            rusqlite::params![task, body],
        )
        .unwrap();
        digest
    }

    fn satisfaction_text(db_path: &Path) -> String {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        let mut stmt = conn
            .prepare("SELECT satisfaction_id, task_id, predecessor_task, requirement, state, evidence_kind, evidence_id FROM dependency_satisfactions ORDER BY task_id, predecessor_task")
            .unwrap();
        let rows: Vec<String> = stmt
            .query_map([], |row| {
                Ok(format!(
                    "{} {} {} {} {} {} {}",
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?
                ))
            })
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        rows.join("\n")
    }

    fn count(db_path: &Path, sql: &str) -> i64 {
        rusqlite::Connection::open(db_path)
            .unwrap()
            .query_row(sql, [], |row| row.get(0))
            .unwrap()
    }

    fn attempts_for(db_path: &Path, task: &str) -> i64 {
        rusqlite::Connection::open(db_path)
            .unwrap()
            .query_row(
                "SELECT count(*) FROM attempts WHERE task_id=?1",
                [task],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn submit_and_verify(
        db_path: &Path,
        repo: &Path,
        work: &Path,
        task: &str,
        attempt: &str,
        base: &str,
        candidate: &str,
        digest: &str,
        body: &str,
        key: &str,
    ) -> String {
        let objects = reachable_objects(repo, &[base, candidate]);
        assert!(objects.iter().any(|(oid, _)| oid == base));
        assert!(objects.iter().any(|(oid, _)| oid == candidate));
        let submission = serde_json::json!({
            "idempotency_key": format!("submit-{key}"),
            "task_id": task,
            "contract_revision": 1,
            "contract_digest": digest,
            "attempt_id": attempt,
            "repository": repo,
            "base_oid": base,
            "candidate_oid": candidate,
            "object_format": "sha1",
            "artifact_manifest": [{"path": "src/fn.txt", "oid": candidate}],
            "claimed_checks": [PROSE],
            "objects": objects.iter().map(|(oid, relative)| serde_json::json!({"oid": oid, "relative_path": relative})).collect::<Vec<_>>()
        });
        let mut db = herdr_projects::store::SqliteStore::open(db_path).unwrap();
        let before = count(db_path, "SELECT count(*) FROM dependency_satisfactions");
        db.submit_result(&serde_json::to_vec(&submission).unwrap())
            .unwrap();
        assert_eq!(
            count(db_path, "SELECT count(*) FROM dependency_satisfactions"),
            before,
            "worker prose created a satisfaction row"
        );
        assert!(
            !satisfaction_text(db_path).contains(PROSE),
            "satisfaction stored worker prose"
        );
        let policy_path = work.join(format!("policy-{key}.json"));
        fs::write(&policy_path, body).unwrap();
        let request = VerifyRequest::new(
            db.show_results(None)
                .unwrap()
                .into_iter()
                .find(|view| view.attempt_id == attempt)
                .unwrap()
                .submission_id,
            "builds",
            &policy_path,
            format!("verify-{key}"),
            Duration::from_secs(60),
            work,
        );
        let outcome = verification::verify(&mut db, &request).unwrap();
        assert_eq!(
            outcome.state, "accepted",
            "{key} {:?} stdout={} argv={:?}",
            outcome.reason, outcome.stdout, outcome.argv
        );
        let result = outcome
            .receipt
            .expect("verified receipt")
            .result_id()
            .to_string();
        assert!(
            !satisfaction_text(db_path).contains(PROSE),
            "verifier stored worker prose as satisfaction"
        );
        result
    }

    fn integrate(
        db_path: &Path,
        repo: &Path,
        work: &Path,
        result_id: &str,
        key: &str,
    ) -> integration::IntegrateOutcome {
        let mut db = herdr_projects::store::SqliteStore::open(db_path).unwrap();
        integration::integrate(
            &mut db,
            &integration::IntegrateRequest {
                result_id: result_id.to_string(),
                idempotency_key: key.to_string(),
                repository: repo.to_path_buf(),
                work_dir: work.to_path_buf(),
                fault: integration::Fault::None,
            },
        )
        .unwrap()
    }

    fn retain_profile(db_path: &Path) {
        let canonical = fs::canonicalize(db_path).unwrap();
        let metadata = fs::metadata(&canonical).unwrap();
        let evidence = VersionedReference {
            id: "test-only-evidence".into(),
            revision: 1,
            digest: "a".repeat(64),
        };
        let supported = CapabilityEvidence::Supported {
            evidence: evidence.clone(),
        };
        let profile = FrozenProfile {
            version: 1,
            name: "fixture".into(),
            kind: "claude".into(),
            definition_digest: "b".repeat(64),
            config: ConfigReference {
                path: "/no/such/admission-config.toml".into(),
                digest: None,
            },
            arguments_digest: "c".repeat(64),
            environment_names: vec![],
            execution_home: None,
            permission_policy: evidence.clone(),
            adapter: evidence.clone(),
            agent: ExecutableIdentity {
                path: "/usr/bin/git".into(),
                digest: "d".repeat(64),
                version: "1.0.0".into(),
            },
            herdr: ExecutableIdentity {
                path: "/usr/bin/git".into(),
                digest: "e".repeat(64),
                version: "0.9.1".into(),
            },
            capabilities: ProfileCapabilities {
                launch: supported.clone(),
                readiness_observation: supported.clone(),
                prompt_submission: supported.clone(),
                stop: supported,
                checkpoint_acknowledgment: CapabilityEvidence::Unknown,
                structured_usage: CapabilityEvidence::Unknown,
                resume: CapabilityEvidence::Unknown,
            },
            workflow_certificate: None,
        };
        let reference = profile.reference().unwrap();
        let report = serde_json::json!({
            "preparation": {
                "profile": profile,
                "reference": reference,
                "launchable": true,
                "protocol_capable": false,
                "certified": false
            },
            "source_store": [canonical, metadata.dev(), metadata.ino()]
        });
        let text = serde_json::to_string(&report).unwrap();
        let report_digest = format!("{:x}", Sha256::digest(text.as_bytes()));
        let conn = rusqlite::Connection::open(db_path).unwrap();
        let sequence: i64 = conn
            .query_row("SELECT MAX(sequence) FROM events", [], |row| row.get(0))
            .unwrap();
        conn.execute(
            "INSERT INTO native_profiles(profile_digest,report,report_digest,sequence) VALUES(?1,?2,?3,?4)",
            rusqlite::params![reference.digest, text, report_digest, sequence],
        )
        .unwrap();
    }

    fn install_grant(db_path: &Path, project: &Path) -> String {
        let inputs = admission::prepared_admission_inputs(project)
            .unwrap()
            .unwrap_or_else(|| panic!("no ready candidate\n{}", satisfaction_text(db_path)));
        let now = jiff::Timestamp::now().as_millisecond();
        let grant = ApprovalGrant {
            version: 1,
            scope: ApprovalScope::for_launch(&inputs).unwrap(),
            policy: inputs
                .effective_profile
                .as_ref()
                .unwrap()
                .permission_policy
                .clone(),
            issued_unix_ms: 0,
            expires_unix_ms: now + 3_600_000,
        };
        let approval = grant.reference().unwrap();
        let payload = String::from_utf8(serde_json::to_vec(&grant).unwrap()).unwrap();
        rusqlite::Connection::open(db_path)
            .unwrap()
            .execute(
                "INSERT INTO approval_grants(id,payload,payload_hash) VALUES(?1,?2,?3)",
                rusqlite::params![approval.id, payload, approval.digest],
            )
            .unwrap();
        inputs.task.as_str().to_string()
    }

    fn enable_admission(db_path: &Path) {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        let off: String = conn
            .query_row(
                "SELECT factory_admission FROM project_control WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            off, "off",
            "migration default must stay off until the fixture UPDATE"
        );
        let updated = conn
            .execute(
                "UPDATE project_control SET factory_admission='on' WHERE singleton=1",
                [],
            )
            .unwrap();
        assert_eq!(updated, 1);
    }

    pub fn run() {
        let migration = include_str!("../migrations/0030_dependency_satisfaction.sql");
        assert!(
            migration.contains("factory_admission TEXT NOT NULL DEFAULT 'off'"),
            "factory_admission migration default changed"
        );
        let doc = include_str!("../docs/factory/vertical-slice.md");
        assert!(
            doc.contains("live Codex repository-editing cell is not run"),
            "vertical-slice doc must say the live cell is not run"
        );

        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let repo = root.path().join("repo");
        let work = root.path().join("work");
        fs::create_dir_all(project.join(".state")).unwrap();
        fs::create_dir_all(&work).unwrap();
        fs::create_dir_all(&repo).unwrap();
        let db_path = project.join(".state/state.db");

        git(&repo, &["init", "-q", "-b", "factory", "--template="]);
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::write(repo.join("src/fn.txt"), "fn=add\n").unwrap();
        fs::write(repo.join("src/peer.txt"), "peer=1\n").unwrap();
        fs::write(repo.join("src/state.txt"), STATE_BASE).unwrap();
        git(&repo, &["add", "--", "src"]);
        git(
            &repo,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "base"],
        );
        git(&repo, &["branch", "integration"]);
        let base = oid_of(&repo, "HEAD");

        git(&repo, &["checkout", "-q", "-b", "worker-a"]);
        fs::write(repo.join("src/fn.txt"), "fn=mul\n").unwrap();
        fs::write(repo.join("WORKER.md"), format!("{PROSE}\n")).unwrap();
        git(&repo, &["add", "--", "src/fn.txt", "WORKER.md"]);
        git(
            &repo,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "a"],
        );
        let commit_a = oid_of(&repo, "HEAD");
        git(&repo, &["checkout", "-q", "factory"]);

        git(&repo, &["checkout", "-q", "-b", "worker-p"]);
        fs::write(repo.join("src/peer.txt"), "peer=2\n").unwrap();
        fs::write(repo.join("src/state.txt"), STATE_GUARD).unwrap();
        git(&repo, &["add", "--", "src/peer.txt", "src/state.txt"]);
        git(
            &repo,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "p"],
        );
        let commit_p = oid_of(&repo, "HEAD");
        git(&repo, &["checkout", "-q", "factory"]);

        git(&repo, &["checkout", "-q", "-b", "worker-s"]);
        fs::write(repo.join("src/state.txt"), STATE_FN).unwrap();
        git(&repo, &["add", "--", "src/state.txt"]);
        git(
            &repo,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "s"],
        );
        let commit_s = oid_of(&repo, "HEAD");
        git(&repo, &["checkout", "-q", "factory"]);
        assert_eq!(oid_of(&repo, TARGET), base);
        assert_eq!(
            git(&repo, &["symbolic-ref", "HEAD"]).trim(),
            "refs/heads/factory"
        );
        assert!(git(&repo, &["show", &format!("{commit_a}:WORKER.md")]).contains(PROSE));

        let repo = repo.canonicalize().unwrap();
        fs::create_dir_all(project.join(".state")).unwrap();
        let db = herdr_projects::store::SqliteStore::create(&db_path).unwrap();
        drop(db);
        let db_path = db_path.canonicalize().unwrap();
        let project_store = db_path.to_str().unwrap().to_string();
        let repository = repo.to_str().unwrap().to_string();
        let mut db = herdr_projects::store::SqliteStore::open(&db_path).unwrap();
        let producers = ["a", "p", "s"];
        let mut mutations: Vec<Mutation> = ["a", "p", "s", "b", "c", "d", "w", "dstale"]
            .into_iter()
            .map(|id| Mutation::Task {
                expected: None,
                next: Task {
                    id: TaskId::new(id).unwrap(),
                    revision: 1,
                    state: TaskState::Draft,
                    title: id.into(),
                    active_attempt: None,
                },
            })
            .collect();
        for id in producers {
            mutations.push(Mutation::Attempt {
                expected: None,
                next: Attempt {
                    id: AttemptId::new(format!("attempt-{id}")).unwrap(),
                    task: TaskId::new(id).unwrap(),
                    revision: 1,
                    state: AttemptState::Completed,
                    snapshot: None,
                    reservation: format!("slot-{id}"),
                    termination_observed: true,
                },
            });
        }
        db.commit(Commit {
            expected_head: 0,
            mutations,
        })
        .unwrap();
        drop(db);
        let admission_off: String = rusqlite::Connection::open(&db_path)
            .unwrap()
            .query_row(
                "SELECT factory_admission FROM project_control WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(admission_off, "off");

        let policy_a = policy(&["/usr/bin/git", "grep", "-q", "^fn=mul$", "--", "src/fn.txt"]);
        let policy_p = policy(&[
            "/usr/bin/git",
            "grep",
            "-q",
            "^peer=2$",
            "--",
            "src/peer.txt",
        ]);
        let policy_s = policy(&[
            "/usr/bin/git",
            "grep",
            "--all-match",
            "-q",
            "-e",
            "^fn=mul$",
            "-e",
            "^guard=ok$",
            "--",
            "src/state.txt",
        ]);
        let policy_consumer = policy(&["/usr/bin/git", "rev-parse", "HEAD"]);
        let digest_a = install_contract(
            &db_path,
            "a",
            &project_store,
            &repository,
            &base,
            "verify_then_integrate",
            &policy_a,
        );
        let digest_p = install_contract(
            &db_path,
            "p",
            &project_store,
            &repository,
            &base,
            "verify_then_integrate",
            &policy_p,
        );
        let digest_s = install_contract(
            &db_path,
            "s",
            &project_store,
            &repository,
            &base,
            "verify_then_integrate",
            &policy_s,
        );
        for task in ["b", "c", "dstale"] {
            install_contract(
                &db_path,
                task,
                &project_store,
                &repository,
                &base,
                "verify_only",
                &policy_consumer,
            );
        }
        let mut db = herdr_projects::store::SqliteStore::open(&db_path).unwrap();
        integration::configure_integration_ref(&mut db, &repo, TARGET).unwrap();
        drop(db);

        let result_a = submit_and_verify(
            &db_path,
            &repo,
            &work,
            "a",
            "attempt-a",
            &base,
            &commit_a,
            &digest_a,
            &policy_a,
            "a",
        );
        assert_eq!(
            count(&db_path, "SELECT count(*) FROM dependency_satisfactions"),
            0
        );
        let result_p = submit_and_verify(
            &db_path,
            &repo,
            &work,
            "p",
            "attempt-p",
            &base,
            &commit_p,
            &digest_p,
            &policy_p,
            "p",
        );
        let result_s = submit_and_verify(
            &db_path,
            &repo,
            &work,
            "s",
            "attempt-s",
            &base,
            &commit_s,
            &digest_s,
            &policy_s,
            "s",
        );
        assert_eq!(
            count(&db_path, "SELECT count(*) FROM dependency_satisfactions"),
            0
        );

        let mut db = herdr_projects::store::SqliteStore::open(&db_path).unwrap();
        for id in ["b", "c", "d", "dstale"] {
            let snapshot = db.read_snapshot(None).unwrap();
            let revision = snapshot
                .tasks
                .iter()
                .find(|task| task.id.as_str() == id)
                .unwrap()
                .revision;
            db.create_runtime(
                Some(&TaskId::new(id).unwrap()),
                Some(revision),
                snapshot.head,
                &RuntimeRoute::default(),
            )
            .unwrap();
        }
        let snapshot = db.read_snapshot(None).unwrap();
        let now = jiff::Timestamp::now().as_millisecond();
        let observations: Vec<RuntimeObservation> = snapshot
            .runtime_bindings
            .iter()
            .map(|binding| RuntimeObservation {
                binding: binding.id.clone(),
                binding_revision: binding.revision,
                task_revision: binding.task.as_ref().map(|task| {
                    snapshot
                        .tasks
                        .iter()
                        .find(|item| item.id == *task)
                        .unwrap()
                        .revision
                }),
                observed_unix_ms: now,
                collector: "herdr-git-v1".into(),
                ..RuntimeObservation::default()
            })
            .collect();
        let head = db
            .record_observations(snapshot.head, &observations)
            .unwrap();
        let snapshot = db.read_snapshot(None).unwrap();
        db.set_project_state(
            head,
            snapshot.control.as_ref().unwrap().revision,
            ProjectState::Active,
            now,
            None,
        )
        .unwrap();
        let snapshot = db.read_snapshot(None).unwrap();
        db.set_scheduler_policy(
            snapshot.head,
            snapshot.scheduler.as_ref().unwrap().policy.revision,
            4,
            3,
        )
        .unwrap();
        drop(db);
        retain_profile(&db_path);

        let mut db = herdr_projects::store::SqliteStore::open(&db_path).unwrap();
        let snapshot = db.read_snapshot(None).unwrap();
        let revision = snapshot
            .tasks
            .iter()
            .find(|task| task.id.as_str() == "c")
            .unwrap()
            .revision;
        db.queue_task(
            &TaskId::new("c").unwrap(),
            revision,
            snapshot.head,
            &QueueRequest {
                priority: 0,
                dependencies: vec![Dependency {
                    predecessor: TaskId::new("a").unwrap(),
                    requirement: DependencyRequirement::VerifiedResult,
                }],
            },
            jiff::Timestamp::now().as_millisecond(),
        )
        .unwrap();
        drop(db);
        let rows = satisfaction_text(&db_path);
        assert!(rows.contains(&result_a), "{rows}");
        assert!(
            rows.contains("c a verified_result valid verified_result"),
            "{rows}"
        );
        assert!(!rows.contains(PROSE), "{rows}");
        assert!(!rows.contains("integrated_commit"), "{rows}");
        assert_eq!(attempts_for(&db_path, "c"), 0);
        admission::admit_once(&project).unwrap();
        assert_eq!(attempts_for(&db_path, "c"), 0, "flag off must not reserve");
        assert!(!admission::wake_enabled(&project));

        rusqlite::Connection::open(&db_path)
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER vertical_slice_crash_receipt BEFORE INSERT ON integrated_commits BEGIN SELECT RAISE(ABORT, 'vertical-slice-crash'); END;",
            )
            .unwrap();
        let mut db = herdr_projects::store::SqliteStore::open(&db_path).unwrap();
        let crashed = integration::integrate(
            &mut db,
            &integration::IntegrateRequest {
                result_id: result_a.clone(),
                idempotency_key: "integrate-a".into(),
                repository: repo.clone(),
                work_dir: work.clone(),
                fault: integration::Fault::None,
            },
        )
        .unwrap_err();
        drop(db);
        let moved = oid_of(&repo, TARGET);
        assert_ne!(
            moved, base,
            "CAS did not publish before the crash: {crashed:#}"
        );
        assert_eq!(
            count(&db_path, "SELECT count(*) FROM integrated_commits"),
            0,
            "{crashed:#}"
        );
        assert_eq!(
            count(
                &db_path,
                "SELECT count(*) FROM dependency_satisfactions WHERE evidence_kind='integrated_commit'"
            ),
            0
        );
        rusqlite::Connection::open(&db_path)
            .unwrap()
            .execute_batch("DROP TRIGGER vertical_slice_crash_receipt;")
            .unwrap();
        let mut db = herdr_projects::store::SqliteStore::open(&db_path).unwrap();
        let confirmed = integration::reconcile_integration(&mut db, &repo, "integrate-a").unwrap();
        drop(db);
        assert_eq!(confirmed.state, "integrated", "{:?}", confirmed.reason);
        assert_eq!(confirmed.commit_oid.as_deref(), Some(moved.as_str()));
        assert_eq!(oid_of(&repo, TARGET), moved);
        assert_eq!(
            count(&db_path, "SELECT count(*) FROM integrated_commits"),
            1
        );

        let mut db = herdr_projects::store::SqliteStore::open(&db_path).unwrap();
        let snapshot = db.read_snapshot(None).unwrap();
        let revision = snapshot
            .tasks
            .iter()
            .find(|task| task.id.as_str() == "b")
            .unwrap()
            .revision;
        db.queue_task(
            &TaskId::new("b").unwrap(),
            revision,
            snapshot.head,
            &QueueRequest {
                priority: 0,
                dependencies: vec![Dependency {
                    predecessor: TaskId::new("a").unwrap(),
                    requirement: DependencyRequirement::IntegratedCommit,
                }],
            },
            jiff::Timestamp::now().as_millisecond(),
        )
        .unwrap();
        drop(db);
        assert!(
            satisfaction_text(&db_path).contains("b a integrated_commit valid integrated_commit")
        );
        assert_eq!(attempts_for(&db_path, "b"), 0);

        let integrated_p = integrate(&db_path, &repo, &work, &result_p, "integrate-p");
        assert_eq!(
            integrated_p.state, "integrated",
            "{:?}",
            integrated_p.reason
        );
        let tip = oid_of(&repo, TARGET);
        assert_eq!(integrated_p.commit_oid.as_deref(), Some(tip.as_str()));
        assert_ne!(tip, moved);

        let mut db = herdr_projects::store::SqliteStore::open(&db_path).unwrap();
        let snapshot = db.read_snapshot(None).unwrap();
        let revision = snapshot
            .tasks
            .iter()
            .find(|task| task.id.as_str() == "w")
            .unwrap()
            .revision;
        db.queue_task(
            &TaskId::new("w").unwrap(),
            revision,
            snapshot.head,
            &QueueRequest {
                priority: 0,
                dependencies: vec![Dependency {
                    predecessor: TaskId::new("s").unwrap(),
                    requirement: DependencyRequirement::IntegratedCommit,
                }],
            },
            jiff::Timestamp::now().as_millisecond(),
        )
        .unwrap();
        drop(db);
        let broken = integrate(&db_path, &repo, &work, &result_s, "integrate-s");
        assert_eq!(broken.state, "blocked", "{:?}", broken.reason);
        assert_eq!(broken.reason.as_deref(), Some("checks_failed"));
        assert_eq!(
            oid_of(&repo, TARGET),
            tip,
            "failed combined tree must not move the ref"
        );
        assert_eq!(
            count(
                &db_path,
                "SELECT count(*) FROM dependency_satisfactions WHERE task_id='w'"
            ),
            0,
            "combined-tree failure created a satisfaction row"
        );
        assert!(!satisfaction_text(&db_path).contains(PROSE));

        install_contract(
            &db_path,
            "d",
            &project_store,
            &repository,
            &tip,
            "verify_only",
            &policy_consumer,
        );
        let mut db = herdr_projects::store::SqliteStore::open(&db_path).unwrap();
        let snapshot = db.read_snapshot(None).unwrap();
        let revision = snapshot
            .tasks
            .iter()
            .find(|task| task.id.as_str() == "d")
            .unwrap()
            .revision;
        db.queue_task(
            &TaskId::new("d").unwrap(),
            revision,
            snapshot.head,
            &QueueRequest {
                priority: 1,
                dependencies: vec![
                    Dependency {
                        predecessor: TaskId::new("a").unwrap(),
                        requirement: DependencyRequirement::IntegratedCommit,
                    },
                    Dependency {
                        predecessor: TaskId::new("p").unwrap(),
                        requirement: DependencyRequirement::IntegratedCommit,
                    },
                ],
            },
            jiff::Timestamp::now().as_millisecond(),
        )
        .unwrap();
        drop(db);
        let rows = satisfaction_text(&db_path);
        assert!(rows.contains("d a integrated_commit"), "{rows}");
        assert!(rows.contains("d p integrated_commit"), "{rows}");
        assert_eq!(attempts_for(&db_path, "d"), 0);

        let mut db = herdr_projects::store::SqliteStore::open(&db_path).unwrap();
        let snapshot = db.read_snapshot(None).unwrap();
        let revision = snapshot
            .tasks
            .iter()
            .find(|task| task.id.as_str() == "dstale")
            .unwrap()
            .revision;
        db.queue_task(
            &TaskId::new("dstale").unwrap(),
            revision,
            snapshot.head,
            &QueueRequest {
                priority: 20,
                dependencies: vec![
                    Dependency {
                        predecessor: TaskId::new("a").unwrap(),
                        requirement: DependencyRequirement::IntegratedCommit,
                    },
                    Dependency {
                        predecessor: TaskId::new("p").unwrap(),
                        requirement: DependencyRequirement::IntegratedCommit,
                    },
                ],
            },
            jiff::Timestamp::now().as_millisecond(),
        )
        .unwrap();
        drop(db);
        enable_admission(&db_path);
        assert!(admission::wake_enabled(&project));
        assert_eq!(install_grant(&db_path, &project), "dstale");
        let stale = admission::admit_once(&project).unwrap_err();
        assert!(
            stale.to_string().contains("integration_missing"),
            "{stale:#}"
        );
        assert_eq!(attempts_for(&db_path, "dstale"), 0);
        assert_eq!(attempts_for(&db_path, "b"), 0);
        assert_eq!(attempts_for(&db_path, "c"), 0);
        assert_eq!(attempts_for(&db_path, "d"), 0);

        let manifest = serde_json::json!({
            "vertical_slice": "pass",
            "git_sha": super::workspace_git_sha(),
            "combined_tree": "checks_failed",
            "restarted_after_cas": true,
            "worker_prose_satisfaction_rows": 0
        });
        let path = root.path().join("vertical-slice-manifest.json");
        let text = serde_json::to_vec_pretty(&manifest).unwrap();
        fs::write(&path, &text).unwrap();
        let read: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(read["vertical_slice"], "pass");
        assert_eq!(read["git_sha"], super::workspace_git_sha());
        assert!(read["git_sha"].as_str().unwrap().len() >= 40);
    }
}

#[cfg(target_os = "linux")]
fn unix_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

#[cfg(target_os = "linux")]
fn sql_count(path: &Path, query: &str) -> i64 {
    rusqlite::Connection::open(path)
        .unwrap()
        .query_row(query, [], |row| row.get(0))
        .unwrap()
}

#[cfg(target_os = "linux")]
fn install_fixture_contract(
    db_path: &Path,
    task: &str,
    repository: &str,
    oid: &str,
    object_format: &str,
    path: &str,
) -> String {
    let store = fs::canonicalize(db_path).unwrap().display().to_string();
    let raw = serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "task_id": task,
        "contract_revision": 1,
        "repository": repository,
        "base_oid": oid,
        "object_format": object_format,
        "scope_path": path,
        "acceptance_policy": PLANNING_POLICY,
    }))
    .unwrap();
    let digest = format!("{:x}", Sha256::digest(&raw));
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute(
        "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('contract.installed',?1,1,1,?2)",
        rusqlite::params![task, serde_json::json!({"digest": digest, "route": "verify_only"}).to_string()],
    )
    .unwrap();
    let sequence = conn.last_insert_rowid();
    let head: i64 = conn
        .query_row("SELECT MAX(sequence) FROM events", [], |row| row.get(0))
        .unwrap();
    conn.execute(
        "INSERT INTO task_contracts(task_id,contract_revision,plan_revision,project_store,expected_head,repository,base_oid,object_format,memory_snapshot_id,route,raw_bytes,raw_digest,installed_seq) VALUES(?1,1,NULL,?2,?3,?4,?5,?6,NULL,'verify_only',?7,?8,?9)",
        rusqlite::params![task, store, head, repository, oid, object_format, raw, digest, sequence],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO acceptance_policies(task_id,contract_revision,policy_id,body) VALUES(?1,1,'builds',?2)",
        rusqlite::params![task, PLANNING_POLICY],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO contract_scope_paths(task_id,contract_revision,ordinal,path,access,certainty) VALUES(?1,1,0,?2,'write','exact')",
        rusqlite::params![task, path],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO resource_claims(task_id,contract_revision,ordinal,kind,resource,access,certainty) VALUES(?1,1,0,'path',?2,'write','exact')",
        rusqlite::params![task, path],
    )
    .unwrap();
    digest
}

#[cfg(target_os = "linux")]
fn plant_profile(db_path: &Path, config: &herdr_projects::migration::ConfigReference) {
    use std::os::unix::fs::MetadataExt;
    let evidence = VersionedReference {
        id: "sim-evidence".into(),
        revision: 1,
        digest: "a".repeat(64),
    };
    let supported = CapabilityEvidence::Supported {
        evidence: evidence.clone(),
    };
    let profile = FrozenProfile {
        version: 1,
        name: "sim".into(),
        kind: "codex".into(),
        definition_digest: "b".repeat(64),
        config: config.clone(),
        arguments_digest: "c".repeat(64),
        environment_names: Vec::new(),
        execution_home: None,
        permission_policy: VersionedReference {
            id: "sim-policy".into(),
            revision: 1,
            digest: "d".repeat(64),
        },
        adapter: evidence,
        agent: ExecutableIdentity {
            path: "/usr/bin/git".into(),
            digest: "e".repeat(64),
            version: "1.0.0".into(),
        },
        herdr: ExecutableIdentity {
            path: "/usr/bin/git".into(),
            digest: "f".repeat(64),
            version: "1.0.0".into(),
        },
        capabilities: ProfileCapabilities {
            launch: supported.clone(),
            readiness_observation: supported.clone(),
            prompt_submission: supported.clone(),
            stop: supported,
            checkpoint_acknowledgment: CapabilityEvidence::Unknown,
            structured_usage: CapabilityEvidence::Unknown,
            resume: CapabilityEvidence::Unknown,
        },
        workflow_certificate: None,
    };
    profile.validate_for_launch().unwrap();
    let reference = profile.reference().unwrap();
    let canonical = fs::canonicalize(db_path).unwrap();
    let metadata = fs::metadata(&canonical).unwrap();
    let report = serde_json::json!({
        "preparation": {
            "profile": profile,
            "reference": reference,
            "launchable": true,
            "protocol_capable": false,
            "certified": false
        },
        "source_store": [canonical, metadata.dev(), metadata.ino()]
    });
    let text = serde_json::to_string(&report).unwrap();
    let report_digest = format!("{:x}", Sha256::digest(text.as_bytes()));
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute(
        "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('profile.native_retained',?1,1,1,?2)",
        rusqlite::params![reference.id, serde_json::to_string(&reference).unwrap()],
    )
    .unwrap();
    let sequence = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO native_profiles(profile_digest,report,report_digest,sequence) VALUES(?1,?2,?3,?4)",
        rusqlite::params![reference.digest, text, report_digest, sequence],
    )
    .unwrap();
}

#[cfg(target_os = "linux")]
fn insert_grant(db_path: &Path, inputs: &LaunchInputs) {
    let profile = inputs.effective_profile.as_ref().unwrap();
    let grant = ApprovalGrant {
        version: 1,
        scope: ApprovalScope::for_launch(inputs).unwrap(),
        policy: profile.permission_policy.clone(),
        issued_unix_ms: 0,
        expires_unix_ms: unix_ms() + 3_600_000,
    };
    let reference = grant.reference().unwrap();
    let payload = serde_json::to_vec(&grant).unwrap();
    assert_eq!(format!("{:x}", Sha256::digest(&payload)), reference.digest);
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute(
        "INSERT INTO approval_grants(id,payload,payload_hash) VALUES(?1,?2,?3)",
        rusqlite::params![
            reference.id,
            String::from_utf8(payload).unwrap(),
            reference.digest
        ],
    )
    .unwrap();
}

#[cfg(target_os = "linux")]
const PLANNING_POLICY: &str = "{\"version\":1,\"checks\":[\"/usr/bin/git\",\"diff\",\"--quiet\"]}";


#[cfg(target_os = "linux")]
#[test]
fn planning_gate_ten_logical_workers() {
    let doc = include_str!("../docs/factory/planning-gate.md");
    assert!(
        doc.contains("live F2.7 is not run"),
        "planning gate must say live F2.7 is not run"
    );
    let lower = doc.to_ascii_lowercase();
    for phrase in [
        "f2.7 passed",
        "live pilot passed",
        "pilot passed",
        "targets met",
    ] {
        assert!(
            !lower.contains(phrase),
            "planning gate claims a live pass via {phrase}"
        );
    }

    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    fs::create_dir_all(project.join(".state")).unwrap();
    let db_path = project.join(".state/state.db");
    let (repo, oid) = build_repo(tmp.path(), SEED);
    let repo = fs::canonicalize(&repo).unwrap();
    let repository = repo.display().to_string();
    let object_format = if oid.len() == 40 { "sha1" } else { "sha256" };
    assert!(matches!(oid.len(), 40 | 64), "{oid}");
    let object_path = format!("{}/{}", &oid[..2], &oid[2..]);
    assert!(
        repo.join(".git/objects").join(&object_path).is_file(),
        "commit object missing"
    );

    let config_path = tmp.path().join("owner.toml");
    fs::write(&config_path, "version = 1\n").unwrap();
    let config = herdr_projects::migration::config_reference(&config_path).unwrap();
    let digest = config.digest.clone().unwrap();

    let mut db = SqliteStore::create(&db_path).unwrap();
    assert_eq!(db.read_snapshot(None).unwrap().schema_version, 41);
    let workers: Vec<String> = (0..10).map(|index| format!("w-{index:02}")).collect();
    db.commit(Commit {
        expected_head: 0,
        mutations: workers
            .iter()
            .map(|id| Mutation::Task {
                expected: None,
                next: Task {
                    id: TaskId::new(id).unwrap(),
                    revision: 1,
                    state: TaskState::Draft,
                    title: format!("logical worker {id}"),
                    active_attempt: None,
                },
            })
            .collect(),
    })
    .unwrap();
    for id in &workers {
        let snapshot = db.read_snapshot(None).unwrap();
        let task = snapshot
            .tasks
            .iter()
            .find(|task| task.id.as_str() == id)
            .unwrap();
        db.create_runtime(
            Some(&task.id),
            Some(task.revision),
            snapshot.head,
            &RuntimeRoute::default(),
        )
        .unwrap();
    }
    let queued_at = unix_ms() - 1_000;
    for id in &workers {
        let snapshot = db.read_snapshot(None).unwrap();
        let task = snapshot
            .tasks
            .iter()
            .find(|task| task.id.as_str() == id)
            .unwrap();
        db.queue_task(
            &task.id,
            task.revision,
            snapshot.head,
            &QueueRequest {
                priority: 0,
                dependencies: Vec::new(),
            },
            queued_at,
        )
        .unwrap();
    }
    let snapshot = db.read_snapshot(None).unwrap();
    let policy = snapshot.scheduler.as_ref().unwrap().policy.clone();
    db.set_scheduler_policy(
        snapshot.head,
        policy.revision,
        10,
        policy.max_attempts_per_task,
    )
    .unwrap();
    let now = unix_ms();
    let snapshot = db.read_snapshot(None).unwrap();
    let observations = snapshot
        .runtime_bindings
        .iter()
        .map(|binding| {
            let task_revision = binding.task.as_ref().and_then(|id| {
                snapshot
                    .tasks
                    .iter()
                    .find(|task| &task.id == id)
                    .map(|task| task.revision)
            });
            herdr_projects::reconcile::RuntimeObservation {
                binding: binding.id.clone(),
                binding_revision: binding.revision,
                task_revision,
                observed_unix_ms: now,
                collector: "herdr-git-v1".into(),
                config_digest: Some(digest.clone()),
                ..herdr_projects::reconcile::RuntimeObservation::default()
            }
        })
        .collect::<Vec<_>>();
    db.record_observations(snapshot.head, &observations)
        .unwrap();
    let snapshot = db.read_snapshot(None).unwrap();
    let control = snapshot.control.unwrap();
    db.set_project_state(
        snapshot.head,
        control.revision,
        ProjectState::Active,
        now,
        Some(&digest),
    )
    .unwrap();
    drop(db);

    for (index, id) in workers.iter().enumerate() {
        let path = if index >= 8 {
            "shared/gate.rs".to_string()
        } else {
            format!("src/w{index:02}.rs")
        };
        install_fixture_contract(&db_path, id, &repository, &oid, object_format, &path);
    }
    plant_profile(&db_path, &config);
    assert!(!herdr_projects::admission::wake_enabled(&project));
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let flag: String = conn
            .query_row(
                "SELECT factory_admission FROM project_control WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(flag, "off");
        // Fixture only. Production code has no writer for this column.
        conn.execute(
            "UPDATE project_control SET factory_admission='on' WHERE singleton=1",
            [],
        )
        .unwrap();
    }
    assert!(herdr_projects::admission::wake_enabled(&project));

    let mut reserved = Vec::new();
    for _ in 0..workers.len() {
        let Some(inputs) = herdr_projects::admission::prepared_admission_inputs(&project).unwrap()
        else {
            break;
        };
        let task = inputs.task.as_str().to_string();
        insert_grant(&db_path, &inputs);
        let before = sql_count(&db_path, "SELECT count(*) FROM attempts");
        herdr_projects::admission::admit_once(&project).unwrap();
        let after = sql_count(&db_path, "SELECT count(*) FROM attempts");
        assert_eq!(after, before + 1, "admission did not reserve {task}");
        reserved.push(task);
    }
    assert!(
        herdr_projects::admission::prepared_admission_inputs(&project)
            .unwrap()
            .is_none(),
        "a blocked worker was still ready"
    );
    herdr_projects::admission::admit_once(&project).unwrap();
    assert_eq!(
        reserved.len(),
        9,
        "expected one overlap to leave a single worker unreserved, got {reserved:?}"
    );
    assert!(reserved.contains(&"w-08".to_string()), "{reserved:?}");
    assert!(!reserved.contains(&"w-09".to_string()), "{reserved:?}");
    assert_eq!(sql_count(&db_path, "SELECT count(*) FROM attempts"), 9);
    assert_eq!(
        sql_count(
            &db_path,
            "SELECT count(*) FROM (SELECT id FROM attempts GROUP BY id HAVING count(*)>1)"
        ),
        0
    );

    let (attempt, digest): (String, String) = {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let attempt = conn
            .query_row("SELECT id FROM attempts WHERE task_id='w-00'", [], |row| {
                row.get(0)
            })
            .unwrap();
        let digest = conn
            .query_row(
                "SELECT raw_digest FROM task_contracts WHERE task_id='w-00' AND contract_revision=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        (attempt, digest)
    };
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    let mismatch = work.join("policy.json");
    fs::write(&mismatch, b"{\"version\":1,\"checks\":[\"/bin/false\"]}").unwrap();
    let mut db = SqliteStore::open(&db_path).unwrap();
    for index in 1..=3 {
        let submission = serde_json::json!({
            "idempotency_key": format!("submit-{index}"),
            "task_id": "w-00",
            "contract_revision": 1,
            "contract_digest": digest,
            "attempt_id": attempt,
            "repository": repository,
            "base_oid": oid,
            "candidate_oid": oid,
            "object_format": object_format,
            "artifact_manifest": [{"path": "README", "oid": oid}],
            "claimed_checks": ["worker prose is not evidence"],
            "objects": [{"oid": oid, "relative_path": object_path}]
        });
        let receipt = db
            .submit_result(&serde_json::to_vec(&submission).unwrap())
            .unwrap();
        assert!(!receipt.replayed);
        let request = herdr_projects::verification::VerifyRequest::new(
            receipt.submission_id,
            "builds",
            &mismatch,
            format!("verify-{index}"),
            Duration::from_secs(5),
            &work,
        );
        let outcome = herdr_projects::verification::verify(&mut db, &request).unwrap();
        assert_eq!(outcome.state, "rejected", "{:?}", outcome.reason);
        assert_eq!(outcome.reason.as_deref(), Some("policy_digest_mismatch"));
        assert!(outcome.receipt.is_none());
    }
    let feedback: Vec<String> = {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let mut stmt = conn
            .prepare("SELECT feedback_id FROM feedback_items ORDER BY created_unix_ms, feedback_id")
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .map(|row| row.unwrap())
            .collect()
    };
    assert_eq!(feedback.len(), 3);
    assert_eq!(
        sql_count(
            &db_path,
            "SELECT count(DISTINCT task_id) FROM feedback_items"
        ),
        1
    );
    assert_eq!(
        sql_count(
            &db_path,
            "SELECT count(*) FROM feedback_items WHERE task_id='w-00' AND category='verifier_rejection' AND reason='policy_digest_mismatch'"
        ),
        3
    );
    assert_eq!(
        sql_count(&db_path, "SELECT count(*) FROM plan_proposals"),
        0
    );
    let first = db.request_replan(&feedback[0]).unwrap();
    let replayed = db.request_replan(&feedback[0]).unwrap();
    assert_eq!(format!("{first:?}"), format!("{replayed:?}"));
    assert!(
        format!("{first:?}").contains("automatic_count: 1"),
        "{first:?}"
    );
    let second = db.request_replan(&feedback[1]).unwrap();
    assert!(
        format!("{second:?}").contains("automatic_count: 2"),
        "{second:?}"
    );
    let third = db.request_replan(&feedback[2]).unwrap();
    assert!(format!("{third:?}").contains("Escalated"), "{third:?}");
    let again = db.request_replan(&feedback[2]).unwrap();
    assert_eq!(format!("{third:?}"), format!("{again:?}"));
    assert_eq!(
        sql_count(
            &db_path,
            "SELECT count(*) FROM replan_requests WHERE outcome='automatic'"
        ),
        2
    );
    assert_eq!(
        sql_count(
            &db_path,
            "SELECT count(*) FROM replan_requests WHERE outcome='escalated'"
        ),
        1
    );
    assert_eq!(
        sql_count(&db_path, "SELECT count(*) FROM plan_proposals"),
        0
    );
    assert_eq!(sql_count(&db_path, "SELECT count(*) FROM inbox_items"), 1);
    let kind: String = rusqlite::Connection::open(&db_path)
        .unwrap()
        .query_row(
            "SELECT json_extract(payload, '$.kind') FROM inbox_items",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(kind, "replan-escalation");

    let waits = [
        ("w-01", "user_decision"),
        ("w-02", "resource_availability"),
        ("w-03", "adapter_recovery"),
        ("w-09", "validation_completion"),
    ];
    let registered = db
        .register_wait("w-00", Some(&attempt), "dependency_evidence")
        .unwrap();
    let duplicate = db
        .register_wait("w-00", Some(&attempt), "dependency_evidence")
        .unwrap();
    assert!(
        format!("{registered:?}").contains("already_registered: false"),
        "{registered:?}"
    );
    assert!(
        format!("{duplicate:?}").contains("already_registered: true"),
        "{duplicate:?}"
    );
    for (task, condition) in waits {
        db.register_wait(task, None, condition).unwrap();
    }
    assert_eq!(
        sql_count(
            &db_path,
            "SELECT count(DISTINCT condition) FROM wait_conditions"
        ),
        5
    );
    let wait_id: String = rusqlite::Connection::open(&db_path)
        .unwrap()
        .query_row(
            "SELECT wait_id FROM wait_conditions WHERE task_id='w-00'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let replay = db.replay_wait(&wait_id).unwrap();
    assert!(
        format!("{replay:?}").contains("proved: false"),
        "{replay:?}"
    );
    assert!(
        format!("{replay:?}").contains("already_replayed: false"),
        "{replay:?}"
    );
    let second_replay = db.replay_wait(&wait_id).unwrap();
    assert!(
        format!("{second_replay:?}").contains("already_replayed: true"),
        "{second_replay:?}"
    );
    assert!(
        format!("{second_replay:?}").contains("proved: false"),
        "{second_replay:?}"
    );

    let attempts_before = sql_count(&db_path, "SELECT count(*) FROM attempts");
    let retry = db.retry_infrastructure("w-00", &attempt).unwrap();
    assert!(format!("{retry:?}").contains("ordinal: 1"), "{retry:?}");
    assert_eq!(
        sql_count(&db_path, "SELECT count(*) FROM attempts"),
        attempts_before
    );
    assert_eq!(
        sql_count(
            &db_path,
            "SELECT count(*) FROM attempt_infrastructure_retries"
        ),
        1
    );
    drop(db);

    assert_eq!(sql_count(&db_path, "SELECT count(*) FROM attempts"), 9);
    assert_eq!(
        sql_count(&db_path, "SELECT count(DISTINCT id) FROM attempts"),
        sql_count(&db_path, "SELECT count(*) FROM attempts")
    );
    assert_eq!(
        sql_count(
            &db_path,
            "SELECT count(*) FROM (SELECT id FROM attempts GROUP BY id HAVING count(*)>1)"
        ),
        0
    );
    assert_eq!(
        sql_count(
            &db_path,
            "SELECT count(*) FROM attempts WHERE task_id='w-09'"
        ),
        0
    );
    assert_eq!(
        sql_count(&db_path, "SELECT count(*) FROM dependency_satisfactions"),
        0
    );
    assert_eq!(
        sql_count(&db_path, "SELECT count(*) FROM verified_results"),
        0
    );
    assert_eq!(
        sql_count(
            &db_path,
            "SELECT count(*) FROM verification_runs WHERE state='accepted'"
        ),
        0
    );
    assert_eq!(
        sql_count(
            &db_path,
            "SELECT count(*) FROM verification_runs WHERE state='rejected' AND task_id='w-00'"
        ),
        3
    );
    assert_eq!(
        sql_count(
            &db_path,
            "SELECT count(*) FROM verification_runs WHERE task_id!='w-00'"
        ),
        0
    );
    assert_eq!(
        sql_count(
            &db_path,
            "SELECT count(*) FROM result_submissions WHERE claimed_checks LIKE '%not evidence%'"
        ),
        3
    );
    assert_eq!(
        sql_count(&db_path, "SELECT count(*) FROM replan_requests"),
        3
    );
}

#[cfg(target_os = "linux")]
struct MemoryProject {
    tmp: tempfile::TempDir,
    project: PathBuf,
    key: PathBuf,
}

#[cfg(target_os = "linux")]
fn memory_project() -> MemoryProject {
    let tmp = tempfile::tempdir().unwrap();
    let key = tmp.path().join("owner");
    assert!(
        Command::new("/usr/bin/ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key)
            .status()
            .unwrap()
            .success()
    );
    let public = fs::read_to_string(key.with_extension("pub"))
        .unwrap()
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    let config = tmp.path().join("config.toml");
    fs::write(
        &config,
        format!("[authority]\nversion=1\nrevision=1\napproval_public_key={public:?}\n"),
    )
    .unwrap();
    let project = tmp.path().join("project");
    fs::create_dir(&project).unwrap();
    for name in [".state", "threads", "inbox"] {
        fs::create_dir(project.join(name)).unwrap();
    }
    fs::create_dir_all(project.join(".state/objects")).unwrap();
    fs::write(
        project.join("PROJECT.md"),
        "+++\nname='Demo'\n+++\nOriginal instructions\n",
    )
    .unwrap();
    fs::write(project.join("TASKS.md"), "").unwrap();
    fs::write(project.join("MEMORY.md"), "Original memory").unwrap();
    fs::write(
        project.join(".state/project.json"),
        r#"{"status":"paused"}"#,
    )
    .unwrap();
    let plan = herdr_projects::migration::inspect_with_config(&project, &config).unwrap();
    herdr_projects::migration::apply(&project, &plan, true).unwrap();
    let state = herdr_projects::runtime::snapshot(&project).unwrap();
    herdr_projects::runtime::set_state(
        &project,
        state.head,
        state.control.unwrap().revision,
        ProjectState::Active,
        &config,
    )
    .unwrap();
    assert_eq!(
        herdr_projects::runtime::snapshot(&project)
            .unwrap()
            .schema_version,
        41
    );
    MemoryProject { tmp, project, key }
}

#[cfg(target_os = "linux")]
fn state_db(project: &Path) -> PathBuf {
    project.join(".state/state.db")
}

#[cfg(target_os = "linux")]
fn assert_admission_off(project: &Path) {
    let flag: String = rusqlite::Connection::open(state_db(project))
        .unwrap()
        .query_row(
            "SELECT factory_admission FROM project_control WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(flag, "off");
}

#[cfg(target_os = "linux")]
fn sign_review(key: &Path, path: &Path, bytes: &[u8]) -> PathBuf {
    fs::write(path, bytes).unwrap();
    let sig = PathBuf::from(format!("{}.sig", path.display()));
    let _ = fs::remove_file(&sig);
    assert!(
        Command::new("/usr/bin/ssh-keygen")
            .args(["-Y", "sign", "-f"])
            .arg(key)
            .args(["-n", herdr_projects::authority::MEMORY_REVIEW_NAMESPACE])
            .arg(path)
            .status()
            .unwrap()
            .success()
    );
    sig
}

#[cfg(target_os = "linux")]
fn memory_read_set(project: &Path) -> MemoryReadSet {
    let conn = rusqlite::Connection::open(state_db(project)).unwrap();
    let required_set_generation: i64 = conn
        .query_row(
            "SELECT generation FROM memory_required_generation WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let policy_revision: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(revision), 0) FROM memory_policies",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let load = |sql: &str| -> Vec<(String, i64)> {
        let mut stmt = conn.prepare(sql).unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(|row| row.unwrap())
            .collect()
    };
    let record_heads = load(
        "SELECT r.id, h.revision FROM memory_records r
         JOIN memory_heads h ON h.record_id = r.id
         WHERE h.status = 'active'
           AND (r.is_hard = 1 OR r.kind IN ('constraint', 'hard_memory', 'contract'))
         ORDER BY r.id",
    )
    .into_iter()
    .map(|(record_id, revision)| ReadSetHead {
        record_id,
        revision: u64::try_from(revision).unwrap(),
    })
    .collect();
    let validity_revisions = load(
        "SELECT v.record_id, v.revision FROM memory_validity v
         JOIN memory_heads h ON h.record_id = v.record_id AND h.revision = v.revision
         JOIN memory_records r ON r.id = v.record_id
         WHERE h.status = 'active'
           AND (r.is_hard = 1 OR r.kind IN ('constraint', 'hard_memory', 'contract'))
         ORDER BY v.record_id",
    )
    .into_iter()
    .map(|(record_id, revision)| ReadSetValidity {
        record_id,
        revision: u64::try_from(revision).unwrap(),
    })
    .collect();
    let scope_catalog_generations =
        load("SELECT scope_id, generation FROM memory_scope_catalog ORDER BY scope_id")
            .into_iter()
            .map(|(scope_id, generation)| ScopeCatalogGeneration {
                scope_id,
                generation: u64::try_from(generation).unwrap(),
            })
            .collect();
    MemoryReadSet {
        record_heads,
        validity_revisions,
        reviewer_grant_id: None,
        revocation_epoch: 0,
        policy_revision: u64::try_from(policy_revision).unwrap(),
        required_set_generation: u64::try_from(required_set_generation).unwrap(),
        scope_catalog_generations,
    }
}

#[cfg(target_os = "linux")]
fn add_task_snapshot(project: &Path, task: &str, domains: &[&str], instructions: &str) -> String {
    let head = herdr_projects::runtime::snapshot(project).unwrap().head;
    herdr_projects::runtime::add_task(project, TaskId::new(task).unwrap(), task.into(), head)
        .unwrap();
    let mut memory = herdr_projects::memory::MemoryStore::from_sqlite(
        herdr_projects::migration::open_active(project).unwrap(),
        project.join(".state/objects"),
    );
    let snap = memory
        .create_task_snapshot(
            SnapshotRequest {
                schema_version: 1,
                task_id: task.into(),
                profile: "worker".into(),
                domains: domains.iter().map(|domain| (*domain).to_string()).collect(),
                paths: Vec::new(),
                pinned_keys: Vec::new(),
                sensitivity: "default".into(),
            },
            "worker",
            &"a".repeat(64),
            None,
            32_000,
            instructions,
            1,
            None,
        )
        .unwrap();
    snap.id.as_str().to_string()
}

#[cfg(target_os = "linux")]
fn bind_attempt(project: &Path, task: &str, attempt: &str, snapshot: &str) {
    let mut db = herdr_projects::migration::open_active(project).unwrap();
    let head = db.read_snapshot(None).unwrap().head;
    db.commit(Commit {
        expected_head: head,
        mutations: vec![Mutation::Attempt {
            expected: None,
            next: Attempt {
                id: AttemptId::new(attempt).unwrap(),
                task: TaskId::new(task).unwrap(),
                revision: 1,
                state: AttemptState::Running,
                snapshot: Some(snapshot.into()),
                reservation: attempt.into(),
                termination_observed: false,
            },
        }],
    })
    .unwrap();
}

#[cfg(target_os = "linux")]
fn propose_observation(
    project: &Path,
    proposal: &str,
    task: &str,
    attempt: &str,
    snapshot: &str,
    key: &str,
    domain: &str,
) -> String {
    let mut memory = herdr_projects::memory::MemoryStore::from_sqlite(
        herdr_projects::migration::open_active(project).unwrap(),
        project.join(".state/objects"),
    );
    let body = memory.ingest_object(key.as_bytes()).unwrap();
    let doc = ProposalDocument {
        schema_version: 1,
        proposal_id: proposal.into(),
        producer: ProposalProducer {
            task_id: task.into(),
            attempt_id: attempt.into(),
        },
        input_snapshot_id: snapshot.into(),
        observed_revisions: Vec::new(),
        repository: None,
        changes: vec![ProposalChange {
            record_key: key.into(),
            expected: None,
            kind: "observation".into(),
            scope: Applicability {
                domains: vec![domain.into()],
                paths: Vec::new(),
            },
            claim: format!("note {key}"),
            body_object: body.as_str().into(),
            evidence: Vec::new(),
            based_on: Vec::new(),
            impact: "informational".into(),
        }],
    };
    let receipt = memory
        .propose(&serde_json::to_vec(&doc).unwrap(), 1_000)
        .unwrap();
    assert_eq!(receipt.validation, "accepted", "{}", receipt.reason);
    receipt.payload_digest
}

#[cfg(target_os = "linux")]
fn review_proposal(
    fixture: &MemoryProject,
    proposal: &str,
    digest: &str,
    key: &str,
    read_set_version: Option<u32>,
) -> ReviewDecision {
    let head = herdr_projects::runtime::snapshot(&fixture.project)
        .unwrap()
        .head;
    let doc = MemoryReviewAuthorization {
        version: 1,
        project_store: state_db(&fixture.project)
            .canonicalize()
            .unwrap()
            .display()
            .to_string(),
        authority: herdr_projects::authority::policy_reference(&fixture.project).unwrap(),
        expected_head: head,
        expires_unix_ms: jiff::Timestamp::now().as_millisecond() + 60_000,
        proposal_digest: digest.into(),
        record_keys: vec![key.into()],
        review: ReviewDocument {
            schema_version: 1,
            proposal_id: proposal.into(),
            decision: "approve".into(),
            reason: "evidence supports the claim".into(),
        },
        read_set_version,
        read_set: read_set_version
            .filter(|version| *version == 2)
            .map(|_| memory_read_set(&fixture.project)),
    };
    let path = fixture.tmp.path().join(format!("{proposal}.json"));
    let sig = sign_review(&fixture.key, &path, &serde_json::to_vec(&doc).unwrap());
    herdr_projects::authority::review_memory_proposal(&fixture.project, proposal, &path, &sig, head)
        .unwrap()
}

#[cfg(target_os = "linux")]
fn event_head(decision: &ReviewDecision) -> u64 {
    let reviewed: serde_json::Value = serde_json::from_str(&decision.reviewed_heads).unwrap();
    reviewed["event_head"].as_u64().unwrap()
}

#[cfg(target_os = "linux")]
fn intents_for(db: &mut SqliteStore, cause: &str) -> Vec<serde_json::Value> {
    db.memory_delivery_intents()
        .unwrap()
        .into_iter()
        .filter(|row| row["cause_id"] == cause)
        .collect()
}

#[cfg(target_os = "linux")]
#[test]
fn memory_gate_doc_is_a_simulator() {
    let doc = include_str!("../docs/factory/memory-gate.md");
    let lower = doc.to_ascii_lowercase();
    assert!(lower.contains("simulator"));
    assert!(lower.contains("no production flag was flipped"));
    for scenario in [
        "Two domains promote together",
        "Same-head conflict",
        "Subscription retirement",
        "Barrier invalidation",
        "Package gap",
        "Coordinator receipt without an attempt",
    ] {
        assert!(doc.contains(scenario), "memory gate omits {scenario}");
    }
    assert!(lower.contains("does not claim a live 40-worker certificate"));
    assert!(lower.contains("or a latency bar"));
    assert!(!lower.contains("certificate passed"));
    assert!(!lower.contains("latency bar met"));
    assert!(!lower.contains("targets met"));
    let controller = include_str!("../src/canonical_controller.rs");
    assert!(controller.contains("const PREPARED_LAUNCH_DISPATCH_ENABLED: bool = true;"));
}

#[cfg(target_os = "linux")]
#[test]
fn memory_two_domains_promote_and_same_head_conflicts() {
    let fixture = memory_project();
    let project = &fixture.project;
    assert_admission_off(project);
    let api_snap = add_task_snapshot(project, "api-task", &["api"], "api instructions");
    bind_attempt(project, "api-task", "api-attempt", &api_snap);
    let ui_snap = add_task_snapshot(project, "ui-task", &["ui"], "ui instructions");
    bind_attempt(project, "ui-task", "ui-attempt", &ui_snap);
    let api_digest = propose_observation(
        project,
        "mp-api",
        "api-task",
        "api-attempt",
        &api_snap,
        "api.error-envelope",
        "api",
    );
    let ui_digest = propose_observation(
        project,
        "mp-ui",
        "ui-task",
        "ui-attempt",
        &ui_snap,
        "ui.copy",
        "ui",
    );
    // v2 fences on the read set, not the event head, so an intervening review
    // of the other domain does not block either promotion.
    let api_review = review_proposal(
        &fixture,
        "mp-api",
        &api_digest,
        "api.error-envelope",
        Some(2),
    );
    let ui_review = review_proposal(&fixture, "mp-ui", &ui_digest, "ui.copy", Some(2));
    let api_promoted =
        herdr_projects::authority::promote_memory_proposal(project, "mp-api", &api_review.id)
            .unwrap();
    let ui_promoted =
        herdr_projects::authority::promote_memory_proposal(project, "mp-ui", &ui_review.id)
            .unwrap();
    assert!(!api_promoted.reused);
    assert!(!ui_promoted.reused);
    assert!(
        api_promoted
            .change_ids
            .iter()
            .all(|id| !ui_promoted.change_ids.contains(id))
    );
    let mut db = herdr_projects::migration::open_active(project).unwrap();
    let api = db
        .memory_record_by_key("api.error-envelope")
        .unwrap()
        .unwrap();
    let ui = db.memory_record_by_key("ui.copy").unwrap().unwrap();
    assert_ne!(api.id, ui.id);
    assert_eq!(
        db.memory_head(api.id.as_str()).unwrap().unwrap().revision,
        1
    );
    assert_eq!(db.memory_head(ui.id.as_str()).unwrap().unwrap().revision, 1);
    assert!(db.memory_promotion("mp-api").unwrap().is_some());
    assert!(db.memory_promotion("mp-ui").unwrap().is_some());
    drop(db);

    let same_a = propose_observation(
        project,
        "mp-same-a",
        "api-task",
        "api-attempt",
        &api_snap,
        "api.same-a",
        "api",
    );
    let same_b = propose_observation(
        project,
        "mp-same-b",
        "api-task",
        "api-attempt",
        &api_snap,
        "api.same-b",
        "api",
    );
    let first_review = review_proposal(&fixture, "mp-same-a", &same_a, "api.same-a", None);
    let shared_head = event_head(&first_review);
    // Each review stamps a new event head. The second approval is stored at the
    // first decision's head so both promotions expect that same fence.
    {
        let template: serde_json::Value =
            serde_json::from_str(&first_review.reviewed_heads).unwrap();
        let mut reviewed = template.clone();
        reviewed["records"] = serde_json::json!({});
        reviewed["authorization"]["proposal_digest"] = serde_json::json!(same_b);
        reviewed["authorization"]["record_keys"] = serde_json::json!(["api.same-b"]);
        reviewed["authorization"]["review"]["proposal_id"] = serde_json::json!("mp-same-b");
        assert_eq!(reviewed["event_head"].as_u64(), Some(shared_head));
        let conn = rusqlite::Connection::open(state_db(project)).unwrap();
        conn.execute(
            "INSERT INTO review_decisions(id,proposal_id,payload_digest,decision,classification,reviewed_heads,reason,created_unix_ms) VALUES('rev-same-b','mp-same-b',?1,'approve','[]',?2,'same event head',1)",
            rusqlite::params![same_b, reviewed.to_string()],
        )
        .unwrap();
    }
    let stored = {
        let mut db = herdr_projects::migration::open_active(project).unwrap();
        db.review_decision("rev-same-b").unwrap().unwrap()
    };
    assert_eq!(event_head(&stored), shared_head);
    let promoted =
        herdr_projects::authority::promote_memory_proposal(project, "mp-same-a", &first_review.id)
            .unwrap();
    assert!(!promoted.reused);
    let conflict =
        herdr_projects::authority::promote_memory_proposal(project, "mp-same-b", "rev-same-b")
            .unwrap_err();
    let text = format!("{conflict:#}").to_ascii_lowercase();
    assert!(text.contains("conflict"), "{conflict:#}");
    let mut db = herdr_projects::migration::open_active(project).unwrap();
    assert!(db.memory_promotion("mp-same-a").unwrap().is_some());
    assert!(db.memory_promotion("mp-same-b").unwrap().is_none());
    assert!(db.memory_record_by_key("api.same-b").unwrap().is_none());
    assert_eq!(
        sql_count(&state_db(project), "SELECT count(*) FROM memory_promotions"),
        3
    );
    assert_admission_off(project);
}

#[cfg(target_os = "linux")]
#[test]
fn memory_retired_subscription_keeps_the_pending_obligation() {
    let fixture = memory_project();
    let project = &fixture.project;
    let reader = add_task_snapshot(project, "reader", &["notes"], "reader instructions");
    let writer = add_task_snapshot(project, "writer", &["notes"], "writer instructions");
    bind_attempt(project, "writer", "writer-attempt", &writer);
    let first_digest = propose_observation(
        project,
        "mp-note-1",
        "writer",
        "writer-attempt",
        &writer,
        "notes.one",
        "notes",
    );
    let first_review = review_proposal(&fixture, "mp-note-1", &first_digest, "notes.one", None);
    herdr_projects::authority::promote_memory_proposal(project, "mp-note-1", &first_review.id)
        .unwrap();
    let successor = {
        let mut memory = herdr_projects::memory::MemoryStore::from_sqlite(
            herdr_projects::migration::open_active(project).unwrap(),
            project.join(".state/objects"),
        );
        memory
            .create_task_snapshot(
                SnapshotRequest {
                    schema_version: 1,
                    task_id: "reader".into(),
                    profile: "worker".into(),
                    domains: vec!["notes".into()],
                    paths: Vec::new(),
                    pinned_keys: Vec::new(),
                    sensitivity: "default".into(),
                },
                "worker",
                &"a".repeat(64),
                None,
                32_000,
                "successor instructions",
                1,
                None,
            )
            .unwrap()
            .id
            .as_str()
            .to_string()
    };
    let mut db = herdr_projects::migration::open_active(project).unwrap();
    let pending = intents_for(&mut db, "mp-note-1");
    let delivery = pending
        .iter()
        .find(|row| row["snapshot_id"] == reader)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        pending.iter().find(|row| row["id"] == delivery).unwrap()["state"],
        "pending"
    );
    let retired = db.consumer_binding_for_snapshot(&reader).unwrap().unwrap();
    let next = db
        .consumer_binding_for_snapshot(&successor)
        .unwrap()
        .unwrap();
    db.retire_consumer_binding(&retired.binding_id, Some(&next.binding_id))
        .unwrap();
    db.retire_consumer_binding(&retired.binding_id, Some(&next.binding_id))
        .unwrap();
    let retired = db.consumer_binding(&retired.binding_id).unwrap().unwrap();
    assert!(retired.retired);
    assert!(!retired.active);
    assert_eq!(
        retired.successor_binding_id.as_deref(),
        Some(next.binding_id.as_str())
    );
    assert!(
        db.binding_obligations(&next.binding_id)
            .unwrap()
            .contains(&delivery)
    );
    assert_eq!(
        intents_for(&mut db, "mp-note-1")
            .iter()
            .filter(|row| row["id"] == delivery)
            .count(),
        1
    );
    drop(db);
    let second_digest = propose_observation(
        project,
        "mp-note-2",
        "writer",
        "writer-attempt",
        &writer,
        "notes.two",
        "notes",
    );
    let second_review = review_proposal(&fixture, "mp-note-2", &second_digest, "notes.two", None);
    herdr_projects::authority::promote_memory_proposal(project, "mp-note-2", &second_review.id)
        .unwrap();
    let mut db = herdr_projects::migration::open_active(project).unwrap();
    let fresh = intents_for(&mut db, "mp-note-2");
    assert!(fresh.iter().all(|row| row["snapshot_id"] != reader));
    assert!(fresh.iter().any(|row| row["snapshot_id"] == successor));
    let fresh_ids: Vec<String> = fresh
        .iter()
        .map(|row| row["id"].as_str().unwrap().to_string())
        .collect();
    let retired_obligations = db.binding_obligations(&retired.binding_id).unwrap();
    assert!(fresh_ids.iter().all(|id| !retired_obligations.contains(id)));
    let successor_obligations = db.binding_obligations(&next.binding_id).unwrap();
    assert!(
        fresh_ids
            .iter()
            .any(|id| successor_obligations.contains(id))
    );
    assert!(successor_obligations.contains(&delivery));
    assert_eq!(
        intents_for(&mut db, "mp-note-1")
            .iter()
            .filter(|row| row["id"] == delivery && row["state"] == "pending")
            .count(),
        1
    );
    assert_admission_off(project);
}

#[cfg(target_os = "linux")]
#[test]
fn memory_package_gap_and_coordinator_ack_without_an_attempt() {
    let fixture = memory_project();
    let project = &fixture.project;
    let writer = add_task_snapshot(project, "writer", &["pkg"], "writer instructions");
    bind_attempt(project, "writer", "writer-attempt", &writer);
    let coordinator = {
        let mut memory = herdr_projects::memory::MemoryStore::from_sqlite(
            herdr_projects::migration::open_active(project).unwrap(),
            project.join(".state/objects"),
        );
        memory
            .create_coordinator_snapshot(
                "gate",
                "planner",
                &"a".repeat(64),
                None,
                32_000,
                "Coordinate",
                1,
            )
            .unwrap()
            .id
            .as_str()
            .to_string()
    };
    let first_digest = propose_observation(
        project,
        "mp-pkg-a",
        "writer",
        "writer-attempt",
        &writer,
        "pkg.a",
        "pkg",
    );
    let first_review = review_proposal(&fixture, "mp-pkg-a", &first_digest, "pkg.a", None);
    herdr_projects::authority::promote_memory_proposal(project, "mp-pkg-a", &first_review.id)
        .unwrap();
    let mut db = herdr_projects::migration::open_active(project).unwrap();
    let binding = db
        .consumer_binding_for_snapshot(&coordinator)
        .unwrap()
        .unwrap();
    assert!(binding.attempt_id.is_none());
    assert!(binding.task_id.is_none());
    let first = db.materialize_update_package(&binding.binding_id).unwrap();
    assert_eq!(first.change_ids.len(), 1);
    let old_id = first.change_ids[0].clone();
    drop(db);
    let second_digest = propose_observation(
        project,
        "mp-pkg-b",
        "writer",
        "writer-attempt",
        &writer,
        "pkg.b",
        "pkg",
    );
    let second_review = review_proposal(&fixture, "mp-pkg-b", &second_digest, "pkg.b", None);
    herdr_projects::authority::promote_memory_proposal(project, "mp-pkg-b", &second_review.id)
        .unwrap();
    let mut db = herdr_projects::migration::open_active(project).unwrap();
    let new_id = intents_for(&mut db, "mp-pkg-b")
        .iter()
        .find(|row| row["snapshot_id"] == coordinator)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(!first.change_ids.contains(&new_id));
    let attempts = sql_count(&state_db(project), "SELECT count(*) FROM attempts");
    assert_eq!(attempts, 1);
    let seen = herdr_projects::store::UpdatePackageAck {
        schema_version: 1,
        package_id: first.package_id.clone(),
        manifest_hash: first.manifest_hash.clone(),
        change_ids: first.change_ids.clone(),
        disposition: "seen".into(),
    };
    db.acknowledge_update_package(&seen, 1).unwrap();
    let mut applied = seen;
    applied.disposition = "applied".into();
    db.acknowledge_update_package(&applied, 1).unwrap();
    assert!(
        db.applied_cursor_covers(&binding.binding_id, &old_id)
            .unwrap()
    );
    assert!(
        !db.applied_cursor_covers(&binding.binding_id, &new_id)
            .unwrap()
    );
    let reloaded = db.consumer_binding(&binding.binding_id).unwrap().unwrap();
    assert!(reloaded.attempt_id.is_none());
    drop(db);
    assert_eq!(
        sql_count(&state_db(project), "SELECT count(*) FROM attempts"),
        attempts
    );
    let applied_new: i64 = rusqlite::Connection::open(state_db(project))
        .unwrap()
        .query_row(
            "SELECT count(*) FROM memory_change_receipts WHERE change_id=?1 AND disposition='applied'",
            [&new_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(applied_new, 0);
    assert_admission_off(project);
}

#[cfg(target_os = "linux")]
struct BarrierSeed {
    task: String,
    attempt: String,
    result: String,
    verification: String,
}

#[cfg(target_os = "linux")]
fn seed_barrier_member(path: &Path, task: &str) -> BarrierSeed {
    let conn = rusqlite::Connection::open(path).unwrap();
    let seq: i64 = conn
        .query_row("SELECT coalesce(max(sequence),0) FROM events", [], |row| {
            row.get(0)
        })
        .unwrap();
    if seq == 0 {
        conn.execute(
            "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('fixture',?1,1,1,'{}')",
            [task],
        )
        .unwrap();
    }
    let installed: i64 = conn
        .query_row("SELECT coalesce(max(sequence),0) FROM events", [], |row| {
            row.get(0)
        })
        .unwrap();
    let hash = "11".repeat(32);
    let attempt = format!("attempt-{task}");
    let snapshot = format!("snap-{task}");
    conn.execute(
        "INSERT INTO tasks(id,revision,state,title,active_attempt) VALUES(?1,1,'running',?1,NULL)",
        [task],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO memory_snapshots(id,task_id,task_revision,profile_name,profile_digest,config_digest,selection_policy_version,estimator,sequence,required_bytes,optional_bytes,budget_bytes,omitted_optional_count,manifest_hash,scope_digest)
         VALUES(?1,?2,1,'worker',?3,NULL,1,'test',1,0,0,0,0,?3,?3)",
        rusqlite::params![snapshot, task, hash],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO attempts(id,task_id,revision,state,snapshot,reservation,termination_observed) VALUES(?1,?2,1,'running',?3,?4,0)",
        rusqlite::params![attempt, task, snapshot, format!("slot-{task}")],
    )
    .unwrap();
    conn.execute(
        "UPDATE tasks SET active_attempt=?2 WHERE id=?1",
        rusqlite::params![task, attempt],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO task_contracts(task_id,contract_revision,plan_revision,project_store,expected_head,repository,base_oid,object_format,memory_snapshot_id,route,raw_bytes,raw_digest,installed_seq)
         VALUES(?1,1,NULL,'/tmp/project',0,'/tmp/repo',?2,'sha1',NULL,'verify_only',x'61',?3,?4)",
        rusqlite::params![task, "b".repeat(40), hash, installed],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO acceptance_policies(task_id,contract_revision,policy_id,body) VALUES(?1,1,'policy-1','{}')",
        [task],
    )
    .unwrap();
    let submission = format!(
        "{:x}",
        Sha256::digest(format!("submission-{task}").as_bytes())
    );
    let result = format!("{:x}", Sha256::digest(format!("result-{task}").as_bytes()));
    let verification = format!("{:x}", Sha256::digest(format!("run-{task}").as_bytes()));
    conn.execute(
        "INSERT INTO result_submissions(submission_id,project_store,idempotency_key,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,memory_snapshot_id,artifact_manifest,claimed_checks,created_unix_ms)
         VALUES(?1,'/tmp/project',?2,?3,'{}',?4,1,?3,?5,'/tmp/repo',?6,?6,'sha1',NULL,'[]','[]',1)",
        rusqlite::params![submission, format!("submit-{task}"), hash, task, attempt, "b".repeat(40)],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO verification_runs(run_id,project_store,idempotency_key,payload_digest,submission_id,task_id,contract_revision,contract_digest,attempt_id,policy_id,policy_digest,commit_oid,tree_oid,object_format,memory_fence,isolation,argv,library_manifest,state,reason,exit_status,receipt_digest,store_device,store_inode,created_unix_ms)
         VALUES(?1,'/tmp/project',?2,?3,?4,?5,1,?3,?6,'policy-1',?3,?7,?7,'sha1',0,'linux-unshare-user-pid-mount-v1','[]','[]','accepted',NULL,0,?3,0,0,1)",
        rusqlite::params![verification, format!("verify-{task}"), hash, submission, task, attempt, "c".repeat(40)],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO verified_results(result_id,run_id,submission_id,commit_oid,tree_oid,object_format,policy_digest,receipt_digest,isolation,memory_fence,created_unix_ms)
         VALUES(?1,?2,?3,?4,?4,'sha1',?5,?5,'linux-unshare-user-pid-mount-v1',0,1)",
        rusqlite::params![result, verification, submission, "c".repeat(40), hash],
    )
    .unwrap();
    BarrierSeed {
        task: task.into(),
        attempt,
        result,
        verification,
    }
}

#[cfg(target_os = "linux")]
fn barrier_member(seed: &BarrierSeed) -> herdr_projects::store::BarrierMember {
    herdr_projects::store::BarrierMember {
        task_id: seed.task.clone(),
        contract_revision: 1,
        attempt_id: seed.attempt.clone(),
        result_id: seed.result.clone(),
        verification_id: seed.verification.clone(),
        integration_id: None,
        proposal_dispositions: Vec::new(),
    }
}

#[cfg(target_os = "linux")]
#[test]
fn memory_barrier_edit_invalidates_the_release_token() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("state.db");
    SqliteStore::create(&path).unwrap();
    let alpha = seed_barrier_member(&path, "alpha");
    let beta = seed_barrier_member(&path, "beta");
    let mut db = SqliteStore::open(&path).unwrap();
    assert_eq!(db.read_snapshot(None).unwrap().schema_version, 41);
    let head = db.read_snapshot(None).unwrap().head;
    let frozen = db.freeze_barrier(&[barrier_member(&alpha)], head).unwrap();
    let head = db.read_snapshot(None).unwrap().head;
    let edited = db
        .freeze_barrier(&[barrier_member(&alpha), barrier_member(&beta)], head)
        .unwrap();
    assert_ne!(edited.barrier_id, frozen.barrier_id);
    assert_ne!(edited.release_token, frozen.release_token);
    let err = db
        .release_barrier(&edited.barrier_id, &frozen.release_token, 0, 1)
        .unwrap_err();
    assert!(
        matches!(err, herdr_projects::store::StoreError::Invalid(ref message) if message.contains("release token")),
        "{err:?}"
    );
    assert!(
        db.freeze_barrier(&[barrier_member(&alpha), barrier_member(&beta)], 0)
            .unwrap()
            .released_seq
            .is_none()
    );
    let before = db.read_snapshot(None).unwrap().attempts;
    assert!(before.iter().all(|attempt| !attempt.termination_observed));
    let head = db.read_snapshot(None).unwrap().head;
    let revoked = db.revoke_barrier(&frozen.barrier_id, head).unwrap();
    assert!(revoked.revoked_seq.is_some());
    let after = db.read_snapshot(None).unwrap().attempts;
    assert_eq!(after, before);
    assert!(after.iter().all(|attempt| !attempt.termination_observed));
    let flag: String = rusqlite::Connection::open(&path)
        .unwrap()
        .query_row(
            "SELECT factory_admission FROM project_control WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(flag, "off");
}

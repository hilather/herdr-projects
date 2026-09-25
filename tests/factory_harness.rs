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

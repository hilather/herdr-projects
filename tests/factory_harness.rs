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

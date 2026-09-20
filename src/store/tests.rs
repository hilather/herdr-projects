use super::*;
use std::{process::{Command, Stdio}, sync::{Arc, Barrier}, thread, time::Instant};
use tempfile::TempDir;

fn task(id: &str, revision: u64) -> Task {
    Task { id: TaskId::new(id).unwrap(), revision, state: TaskState::Queued, title: "fixture".into(), active_attempt: None }
}
fn put(next: Task, expected: Option<u64>) -> Mutation { Mutation::Task { expected, next } }
fn intent(id: &str) -> Operation {
    Operation { id: OperationId::new(id).unwrap(), task: TaskId::new("t-1").unwrap(), kind: "launch".into(), target: "fixture-session".into(), payload_version: 1, payload: serde_json::json!({"argv":["fixture"]}), expected_revision: 1, due_unix_ms: 0, idempotency_key: id.into() }
}
fn lab() -> (TempDir, SqliteStore) {
    let temp = TempDir::new().unwrap();
    let db = SqliteStore::create(&temp.path().join("state.db")).unwrap();
    (temp, db)
}

#[test]
fn atomic_records_intents_events_reopen_and_history() {
    let (temp, mut db) = lab();
    let mut task = task("t-1", 1);
    task.active_attempt = Some(AttemptId::new("a-1").unwrap());
    let attempt = Attempt { id: AttemptId::new("a-1").unwrap(), task: task.id.clone(), revision: 1, state: AttemptState::Reserved, snapshot: None, reservation: "slot-1".into(), termination_observed: false };
    assert_eq!(db.commit(Commit { expected_head: 0, mutations: vec![put(task.clone(), None), Mutation::Attempt { expected: None, next: attempt.clone() }, Mutation::Enqueue(intent("op-1"))] }).unwrap(), 3);
    let before = db.read_snapshot(Some(3)).unwrap();
    assert_eq!(before.tasks, vec![task]);
    assert_eq!(before.attempts, vec![attempt]);
    assert!(before.attempts[0].retains_capacity());
    assert_eq!(before.operations, vec![intent("op-1")]);
    assert_eq!(before.events.iter().map(|e| e.sequence).collect::<Vec<_>>(), vec![1,2,3]);
    assert!(matches!(db.read_snapshot(Some(0)), Err(StoreError::HistoryUnavailable(0))));
    db.checkpoint().unwrap();
    drop(db);
    assert_eq!(SqliteStore::open(&temp.path().join("state.db")).unwrap().read_snapshot(None).unwrap(), before);
}

#[test]
fn failures_roll_back_prior_changes_and_do_not_consume_event_sequences() {
    let (_temp, mut db) = lab();
    db.commit(Commit { expected_head: 0, mutations: vec![put(task("t-1",1), None)] }).unwrap();
    let before = db.read_snapshot(None).unwrap();
    let mut op = intent("op-1"); op.expected_revision = 2;
    assert!(matches!(db.commit(Commit { expected_head: 1, mutations: vec![put(task("t-1",2),Some(1)), Mutation::Enqueue(op.clone()), put(task("missing",2),Some(1))] }), Err(StoreError::Conflict)));
    assert_eq!(db.read_snapshot(None).unwrap(), before);
    assert!(matches!(db.commit(Commit { expected_head: 0, mutations: vec![put(task("t-2",1), None)] }), Err(StoreError::Conflict)));
    assert!(matches!(db.commit(Commit { expected_head: 1, mutations: vec![put(task("t-1",3),Some(1))] }), Err(StoreError::Invalid(_))));
    assert!(matches!(db.commit(Commit { expected_head: 1, mutations: vec![put(task("t-1",3),Some(2))] }), Err(StoreError::Conflict)));
    assert_eq!(db.commit(Commit { expected_head: 1, mutations: vec![put(task("t-1",2),Some(1)), Mutation::Enqueue(op)] }).unwrap(), 3);
    assert_eq!(db.read_snapshot(None).unwrap().events.iter().map(|e| e.sequence).collect::<Vec<_>>(), vec![1,2,3]);
}

#[test]
fn foreign_keys_and_reservations_reject_invalid_batches() {
    let (_temp, mut db) = lab();
    let mut missing = task("t-1",1); missing.active_attempt = Some(AttemptId::new("missing").unwrap());
    assert!(matches!(db.commit(Commit { expected_head: 0, mutations: vec![put(missing,None)] }), Err(StoreError::Conflict)));
    assert_eq!(db.read_snapshot(None).unwrap().head, 0);
    let attempt = Attempt { id: AttemptId::new("a-1").unwrap(), task: TaskId::new("t-1").unwrap(), revision: 1, state: AttemptState::Lost, snapshot: None, reservation: "slot-1".into(), termination_observed: false };
    db.commit(Commit { expected_head: 0, mutations: vec![put(task("t-1",1), None), Mutation::Attempt { expected:None, next:attempt.clone() }] }).unwrap();
    let mut second = attempt.clone(); second.id = AttemptId::new("a-2").unwrap();
    assert!(matches!(db.commit(Commit { expected_head: 2, mutations: vec![Mutation::Attempt { expected:None, next:second.clone() }] }), Err(StoreError::Conflict)));
    let mut terminated = attempt; terminated.revision = 2; terminated.termination_observed = true;
    db.commit(Commit { expected_head: 2, mutations: vec![Mutation::Attempt { expected:Some(1), next:terminated }, Mutation::Attempt { expected:None, next:second }] }).unwrap();
    db.integrity_check().unwrap();
}

#[test]
fn duplicate_intent_key_and_stale_binding_are_atomic() {
    let (_temp, mut db) = lab();
    db.commit(Commit { expected_head:0, mutations:vec![put(task("t-1",1),None),Mutation::Enqueue(intent("op-1"))] }).unwrap();
    let before = db.read_snapshot(None).unwrap();
    let mut duplicate = intent("op-2"); duplicate.idempotency_key = "op-1".into();
    assert!(matches!(db.commit(Commit { expected_head:2, mutations:vec![Mutation::Enqueue(duplicate)] }), Err(StoreError::Conflict)));
    assert!(matches!(db.commit(Commit { expected_head:2, mutations:vec![put(task("t-1",2),Some(1)),Mutation::Enqueue(intent("op-3"))] }), Err(StoreError::Conflict)));
    assert!(matches!(db.commit(Commit { expected_head:2, mutations:vec![Mutation::Enqueue(intent("op-4")),put(task("t-1",2),Some(1))] }), Err(StoreError::Conflict)));
    assert_eq!(db.read_snapshot(None).unwrap(), before);
}

#[test]
fn competing_connections_cannot_duplicate_ids_or_win_the_same_revision() {
    let (temp, mut db) = lab();
    for updating in [false,true] {
        let head = db.read_snapshot(None).unwrap().head;
        let barrier = Arc::new(Barrier::new(2));
        let workers: Vec<_> = (0..2).map(|_| {
            let path = temp.path().join("state.db"); let barrier = barrier.clone();
            thread::spawn(move || {
                let mut db = SqliteStore::open(&path).unwrap();
                barrier.wait();
                db.commit(Commit { expected_head:head, mutations:vec![put(task("t-1",if updating {2} else {1}), if updating {Some(1)} else {None})] })
            })
        }).collect();
        let results: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(),1);
        assert_eq!(results.iter().filter(|r| matches!(r,Err(StoreError::Conflict))).count(),1);
        let snapshot = db.read_snapshot(None).unwrap();
        assert_eq!(snapshot.tasks.len(),1);
        assert_eq!(snapshot.tasks[0].revision,if updating {2} else {1});
    }
}

#[test]
fn busy_is_bounded_and_reader_sees_only_committed_state() {
    let (temp, mut db) = lab();
    let mut writer = SqliteStore::open(&temp.path().join("state.db")).unwrap();
    let tx = writer.connection.transaction_with_behavior(TransactionBehavior::Immediate).unwrap();
    tx.execute("INSERT INTO tasks VALUES('t-hidden',1,'queued','hidden',NULL)", []).unwrap();
    assert!(db.read_snapshot(None).unwrap().tasks.is_empty());
    let start = Instant::now();
    assert!(matches!(db.commit(Commit { expected_head:0, mutations:vec![put(task("t-1",1),None)] }), Err(StoreError::Busy)));
    assert!(start.elapsed() < Duration::from_secs(3));
    drop(tx);
    assert_eq!(db.commit(Commit { expected_head:0, mutations:vec![put(task("t-1",1),None)] }).unwrap(),1);
}

#[test]
fn disk_full_rolls_back_all_records_events_and_intents() {
    let (temp, mut db) = lab();
    db.commit(Commit { expected_head:0, mutations:vec![put(task("t-1",1),None)] }).unwrap();
    let before = db.read_snapshot(None).unwrap();
    db.checkpoint().unwrap();
    let pages: u32 = db.connection.query_row("PRAGMA page_count", [], |r| r.get(0)).unwrap();
    db.connection.pragma_update(None,"max_page_count",pages).unwrap();
    let mut large = intent("large"); large.expected_revision = 2; large.payload = serde_json::json!({"blob":"x".repeat(200_000)});
    assert!(matches!(db.commit(Commit { expected_head:1, mutations:vec![put(task("t-1",2),Some(1)),Mutation::Enqueue(large)] }),Err(StoreError::DiskFull)));
    assert_eq!(db.read_snapshot(None).unwrap(),before);
    drop(db);
    let mut reopened = SqliteStore::open(&temp.path().join("state.db")).unwrap();
    assert_eq!(reopened.read_snapshot(None).unwrap(),before);
}

#[test]
fn refuses_unknown_schema_non_database_missing_and_symlink() {
    let (temp, db) = lab(); drop(db);
    let path = temp.path().join("state.db");
    let raw = Connection::open(&path).unwrap();
    raw.pragma_update(None,"user_version",99).unwrap(); drop(raw);
    let before = std::fs::read(&path).unwrap();
    assert!(matches!(SqliteStore::open(&path),Err(StoreError::UnsupportedSchema(99))));
    assert_eq!(std::fs::read(&path).unwrap(),before);
    assert!(SqliteStore::create(&path).is_err());
    assert!(SqliteStore::open(&temp.path().join("missing")).is_err());
    assert!(!temp.path().join("missing").exists());
    std::os::unix::fs::symlink(&path,temp.path().join("link")).unwrap();
    assert!(matches!(SqliteStore::open(&temp.path().join("link")),Err(StoreError::Invalid(_))));
    std::fs::write(temp.path().join("broken"), b"not a SQLite database").unwrap();
    assert!(matches!(SqliteStore::open(&temp.path().join("broken")),Err(StoreError::Corrupt(_))));
}

#[test]
fn changed_schema_is_rechecked_before_commit_and_bad_payload_is_visible() {
    let (temp, mut db) = lab();
    db.commit(Commit { expected_head:0, mutations:vec![put(task("t-1",1),None),Mutation::Enqueue(intent("op-1"))] }).unwrap();
    let raw = Connection::open(temp.path().join("state.db")).unwrap();
    raw.execute("UPDATE operations SET payload='{}'",[]).unwrap();
    assert!(matches!(db.read_snapshot(None),Err(StoreError::Corrupt(_))));
    raw.pragma_update(None,"user_version",10).unwrap();
    assert!(matches!(db.commit(Commit { expected_head:2, mutations:vec![put(task("t-1",2),Some(1))] }),Err(StoreError::UnsupportedSchema(10))));
}

#[test]
fn invalid_ids_and_overflow_are_rejected() {
    assert!(TaskId::new("../../other").is_err());
    assert!(serde_json::from_str::<TaskId>("\"bad id\"").is_err());
    let (_temp, mut db) = lab();
    assert!(matches!(db.commit(Commit { expected_head:0, mutations:vec![put(task("t-1",u64::MAX),Some(u64::MAX-1))] }),Err(StoreError::Invalid(_))));
}

// The subprocess is a native test-harness child. Readiness is explicit, not a
// guessed delay. Parent kills it at pre-commit and post-commit boundaries.
#[test]
fn crash_child() {
    let Some(root) = std::env::var_os("HP_STORE_CRASH_ROOT") else { return; };
    let root = std::path::PathBuf::from(root);
    let mut db = SqliteStore::open(&root.join("state.db")).unwrap();
    if std::env::var_os("HP_STORE_AFTER_COMMIT").is_some() {
        db.commit(Commit { expected_head:0, mutations:vec![put(task("t-1",1),None),Mutation::Enqueue(intent("op-1"))] }).unwrap();
        std::fs::write(root.join("ready"),b"committed").unwrap();
        loop { thread::sleep(Duration::from_secs(1)); }
    }
    let tx = db.connection.transaction_with_behavior(TransactionBehavior::Immediate).unwrap();
    tx.execute("INSERT INTO tasks VALUES('t-partial',1,'queued',?1,NULL)",["x".repeat(300_000)]).unwrap();
    tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('fixture','t-partial',1,1,'{}')",[]).unwrap();
    // Force dirty pages into the WAL before death without committing them.
    tx.cache_flush().unwrap();
    std::fs::write(root.join("ready"),b"uncommitted").unwrap();
    loop { thread::sleep(Duration::from_secs(1)); }
}

#[test]
fn process_death_preserves_atomicity_on_both_sides_of_commit() {
    for after in [false,true] {
        let (temp, db) = lab(); drop(db);
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args(["--exact","store::tests::crash_child","--nocapture"])
            .env("HP_STORE_CRASH_ROOT",temp.path()).env_remove("HP_STORE_AFTER_COMMIT")
            .stdout(Stdio::null()).stderr(Stdio::inherit());
        if after { command.env("HP_STORE_AFTER_COMMIT","1"); }
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now()+Duration::from_secs(10);
        while !temp.path().join("ready").exists() {
            if Instant::now() >= deadline || child.try_wait().unwrap().is_some() {
                let _ = child.kill(); let _ = child.wait(); panic!("crash fixture failed to reach boundary");
            }
            thread::sleep(Duration::from_millis(10));
        }
        child.kill().unwrap(); child.wait().unwrap();
        let mut reopened = SqliteStore::open(&temp.path().join("state.db")).unwrap();
        let snapshot = reopened.read_snapshot(None).unwrap();
        assert_eq!(snapshot.tasks.len(),usize::from(after));
        assert_eq!(snapshot.operations.len(),usize::from(after));
        assert_eq!(snapshot.head,if after {2} else {0});
        reopened.integrity_check().unwrap();
    }
}

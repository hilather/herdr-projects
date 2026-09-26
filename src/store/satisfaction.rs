//! Schema 30 dependency evidence. A row is written only from a stored verified
//! result or integrated commit. Narrative task success is not a receipt, and a
//! verified_result row is not an integrated_commit row. factory_admission stays
//! off unless a test setter or a later signed installer changes it.
use super::*;
use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};

const SCHEMA_VERSION: u32 = 30;

fn invalid(message: &str) -> StoreError {
    StoreError::Invalid(message.into())
}
fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn at_least_30(db: &Connection) -> Result<bool> {
    let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    Ok(version >= SCHEMA_VERSION)
}
fn satisfaction_id(task: &str, predecessor: &str, requirement: &str, evidence_id: &str) -> String {
    sha256_hex(format!("{task}\0{predecessor}\0{requirement}\0{evidence_id}").as_bytes())
}

struct VerifiedReceipt {
    task_id: String,
}

fn load_verified(db: &Connection, result_id: &str) -> Result<Option<VerifiedReceipt>> {
    db.query_row(
        "SELECT r.task_id
         FROM verified_results v
         JOIN verification_runs r ON r.run_id = v.run_id
         WHERE v.result_id=?1 AND r.state='accepted' AND v.memory_fence=r.memory_fence
           AND v.isolation='linux-unshare-user-pid-mount-v1'",
        [result_id],
        |row| Ok(VerifiedReceipt { task_id: row.get(0)? }),
    )
    .optional()
    .map_err(StoreError::from)
}

fn insert_valid(
    tx: &Connection,
    task_id: &str,
    predecessor: &str,
    requirement: &str,
    evidence_id: &str,
) -> Result<()> {
    let existing: Option<(String, String)> = tx
        .query_row(
            "SELECT satisfaction_id, evidence_id FROM dependency_satisfactions WHERE task_id=?1 AND predecessor_task=?2 AND requirement=?3 AND state='valid'",
            params![task_id, predecessor, requirement],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((satisfaction_id, evidence)) = existing {
        if evidence == evidence_id {
            return Ok(());
        }
        // A newer stored receipt must not roll back the commit that wrote it.
        // The old row stays for history; only one valid row remains.
        tx.execute(
            "UPDATE dependency_satisfactions SET state='invalid' WHERE satisfaction_id=?1 AND state='valid'",
            [satisfaction_id],
        )?;
    }
    let now = jiff::Timestamp::now().as_millisecond();
    tx.execute(
        "INSERT INTO dependency_satisfactions(satisfaction_id,task_id,predecessor_task,requirement,state,evidence_kind,evidence_id,created_unix_ms) VALUES(?1,?2,?3,?4,'valid',?4,?5,?6)",
        params![
            satisfaction_id(task_id, predecessor, requirement, evidence_id),
            task_id,
            predecessor,
            requirement,
            evidence_id,
            now
        ],
    )?;
    Ok(())
}

fn consumers(tx: &Connection, predecessor: &str, requirement: &str) -> Result<Vec<String>> {
    let mut stmt = tx.prepare(
        "SELECT task_id FROM task_dependencies WHERE predecessor_id=?1 AND requirement=?2 ORDER BY task_id",
    )?;
    let rows = stmt.query_map(params![predecessor, requirement], |row| row.get(0))?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(StoreError::from)
}

/// Satisfies `verified_result` edges only. An integrated_commit edge is left untouched.
pub(super) fn record_verified_result(tx: &Connection, result_id: &str) -> Result<()> {
    if !at_least_30(tx)? {
        return Ok(());
    }
    let Some(receipt) = load_verified(tx, result_id)? else {
        return Err(invalid("verified receipt is not stored"));
    };
    // An open memory fence still stores the row. The report is what hides it.
    for task_id in consumers(tx, &receipt.task_id, "verified_result")? {
        insert_valid(tx, &task_id, &receipt.task_id, "verified_result", result_id)?;
    }
    Ok(())
}

fn integrated_predecessor(tx: &Connection, integrated_id: &str) -> Result<Option<String>> {
    tx.query_row(
        "SELECT r.task_id
         FROM integrated_commits i
         JOIN integration_operations o ON o.operation_id=i.operation_id
         JOIN verified_results v ON v.result_id=o.verified_result_id
         JOIN verification_runs r ON r.run_id=v.run_id
         WHERE i.integrated_id=?1 AND r.state='accepted'",
        [integrated_id],
        |row| row.get(0),
    )
    .optional()
    .map_err(StoreError::from)
}

/// Satisfies `integrated_commit` edges only. A verified_result edge is a different fact.
pub(super) fn record_integrated_commit(tx: &Connection, integrated_id: &str) -> Result<()> {
    if !at_least_30(tx)? {
        return Ok(());
    }
    let Some(predecessor) = integrated_predecessor(tx, integrated_id)? else {
        return Err(invalid("integrated receipt is not stored"));
    };
    for task_id in consumers(tx, &predecessor, "integrated_commit")? {
        insert_valid(
            tx,
            &task_id,
            &predecessor,
            "integrated_commit",
            integrated_id,
        )?;
    }
    Ok(())
}

fn latest_verified(tx: &Connection, predecessor: &str) -> Result<Option<String>> {
    tx.query_row(
        "SELECT v.result_id
         FROM verified_results v
         JOIN verification_runs r ON r.run_id=v.run_id
         WHERE r.task_id=?1 AND r.state='accepted'
         ORDER BY v.created_unix_ms DESC, v.result_id DESC
         LIMIT 1",
        [predecessor],
        |row| row.get(0),
    )
    .optional()
    .map_err(StoreError::from)
}

fn latest_integrated(tx: &Connection, predecessor: &str) -> Result<Option<String>> {
    tx.query_row(
        "SELECT i.integrated_id
         FROM integrated_commits i
         JOIN integration_operations o ON o.operation_id=i.operation_id
         JOIN verified_results v ON v.result_id=o.verified_result_id
         JOIN verification_runs r ON r.run_id=v.run_id
         WHERE r.task_id=?1 AND r.state='accepted'
         ORDER BY i.created_unix_ms DESC, i.integrated_id DESC
         LIMIT 1",
        [predecessor],
        |row| row.get(0),
    )
    .optional()
    .map_err(StoreError::from)
}

/// Edges queued after the receipt still see it. Edges with no receipt stay empty.
pub(super) fn attach_stored_receipts(tx: &Connection, task_id: &str) -> Result<()> {
    if !at_least_30(tx)? {
        return Ok(());
    }
    let mut stmt = tx.prepare(
        "SELECT predecessor_id, requirement FROM task_dependencies WHERE task_id=?1 ORDER BY predecessor_id",
    )?;
    let edges = stmt
        .query_map([task_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for (predecessor, requirement) in edges {
        match requirement.as_str() {
            "verified_result" => {
                if let Some(result_id) = latest_verified(tx, &predecessor)? {
                    record_verified_result(tx, &result_id)?;
                }
            }
            "integrated_commit" => {
                if let Some(integrated_id) = latest_integrated(tx, &predecessor)? {
                    record_integrated_commit(tx, &integrated_id)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

pub(super) fn admission_enabled(db: &Connection) -> Result<bool> {
    if !at_least_30(db)? {
        return Ok(false);
    }
    let value: String = db.query_row(
        "SELECT factory_admission FROM project_control WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    match value.as_str() {
        "off" => Ok(false),
        "on" => Ok(true),
        _ => Err(StoreError::Corrupt(
            "factory_admission is not off or on".into(),
        )),
    }
}

fn verified_counts(db: &Connection, task_id: &str, predecessor: &str) -> Result<bool> {
    db.query_row(
        "SELECT EXISTS(
            SELECT 1
            FROM dependency_satisfactions s
            JOIN verified_results v ON v.result_id=s.evidence_id AND s.evidence_kind='verified_result'
            JOIN verification_runs r ON r.run_id=v.run_id AND r.task_id=s.predecessor_task
            JOIN attempts a ON a.id=r.attempt_id AND a.task_id=r.task_id
            JOIN tasks t ON t.id=a.task_id
            WHERE s.task_id=?1 AND s.predecessor_task=?2 AND s.requirement='verified_result' AND s.state='valid'
              AND r.state='accepted' AND v.memory_fence=r.memory_fence
              AND (
                t.active_attempt=r.attempt_id
                OR (
                  t.active_attempt IS NULL
                  AND r.attempt_id=(
                    SELECT latest.id FROM attempts latest
                    WHERE latest.task_id=r.task_id
                    ORDER BY latest.rowid DESC
                    LIMIT 1
                  )
                )
              )
              AND r.contract_revision=(
                SELECT MAX(c.contract_revision) FROM task_contracts c WHERE c.task_id=r.task_id
              )
              AND NOT EXISTS(
                SELECT 1 FROM memory_invalidations i
                WHERE (i.task_id=s.predecessor_task OR i.task_id IS NULL)
                  AND i.resolved_seq IS NULL AND i.severity!='informational'
              )
         )",
        params![task_id, predecessor],
        |row| row.get(0),
    )
    .map_err(StoreError::from)
}

fn integrated_counts(db: &Connection, task_id: &str, predecessor: &str) -> Result<bool> {
    db.query_row(
        "SELECT EXISTS(
            SELECT 1
            FROM dependency_satisfactions s
            JOIN integrated_commits i ON i.integrated_id=s.evidence_id AND s.evidence_kind='integrated_commit'
            JOIN integration_operations o ON o.operation_id=i.operation_id
            JOIN verified_results v ON v.result_id=o.verified_result_id
            JOIN verification_runs r ON r.run_id=v.run_id AND r.task_id=s.predecessor_task
            WHERE s.task_id=?1 AND s.predecessor_task=?2 AND s.requirement='integrated_commit' AND s.state='valid'
              AND r.state='accepted' AND r.task_id=?2
              AND NOT EXISTS(
                SELECT 1 FROM memory_invalidations m
                WHERE (m.task_id=s.predecessor_task OR m.task_id IS NULL)
                  AND m.resolved_seq IS NULL AND m.severity!='informational'
              )
         )",
        params![task_id, predecessor],
        |row| row.get(0),
    )
    .map_err(StoreError::from)
}

/// `None` means this edge adds no dependency blocker. landed_commit and
/// integration_candidate never count as valid, even if some other receipt exists.
pub(super) fn dependency_blocker(
    db: &Connection,
    task_id: &str,
    predecessor: &Task,
    requirement: DependencyRequirement,
    admission_on: bool,
) -> Result<Option<String>> {
    let pred = predecessor.id.as_str();
    let requirement_text = requirement.as_str();
    if matches!(predecessor.state, TaskState::Failed | TaskState::Cancelled) {
        return Ok(Some(format!(
            "predecessor_failed:{pred}:{requirement_text}"
        )));
    }
    let valid = at_least_30(db)?
        && match requirement {
            DependencyRequirement::VerifiedResult => verified_counts(db, task_id, pred)?,
            DependencyRequirement::IntegratedCommit => integrated_counts(db, task_id, pred)?,
            DependencyRequirement::IntegrationCandidate | DependencyRequirement::LandedCommit => {
                false
            }
        };
    if !valid {
        return Ok(Some(format!(
            "verified_dependency_evidence_unavailable:{pred}:{requirement_text}"
        )));
    }
    if !admission_on {
        return Ok(Some(format!("admission_disabled:{requirement_text}")));
    }
    Ok(None)
}

#[cfg(test)]
impl SqliteStore {
    /// Library tests only. Not compiled into a release build and not `pub`.
    pub(super) fn testing_set_factory_admission(&self, enabled: bool) -> Result<()> {
        if !at_least_30(&self.connection)? {
            return Err(StoreError::UnsupportedSchema(self.connection.query_row(
                "PRAGMA user_version",
                [],
                |row| row.get(0),
            )?));
        }
        let value = if enabled { "on" } else { "off" };
        let updated = self.connection.execute(
            "UPDATE project_control SET factory_admission=?1 WHERE singleton=1",
            [value],
        )?;
        if updated != 1 {
            return Err(StoreError::Conflict);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_version(db: &Connection) -> u32 {
        db.query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }
    fn dependencies(db: &Connection) -> Vec<(String, String, String)> {
        let mut stmt = db
            .prepare(
                "SELECT task_id, predecessor_id, requirement FROM task_dependencies ORDER BY task_id, predecessor_id",
            )
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
    }
    fn task(id: &str, state: TaskState) -> Mutation {
        Mutation::Task {
            expected: None,
            next: Task {
                id: TaskId::new(id).unwrap(),
                revision: 1,
                state,
                title: id.into(),
                active_attempt: None,
            },
        }
    }
    fn queue(
        db: &mut SqliteStore,
        id: &str,
        predecessor: &str,
        requirement: DependencyRequirement,
    ) {
        let snapshot = db.read_snapshot(None).unwrap();
        let revision = snapshot
            .tasks
            .iter()
            .find(|item| item.id.as_str() == id)
            .unwrap()
            .revision;
        db.queue_task(
            &TaskId::new(id).unwrap(),
            revision,
            snapshot.head,
            &QueueRequest {
                priority: 0,
                dependencies: vec![Dependency {
                    predecessor: TaskId::new(predecessor).unwrap(),
                    requirement,
                }],
            },
            0,
        )
        .unwrap();
    }
    fn dependency_lines(report: &QueueReport, task_id: &str) -> Vec<String> {
        report
            .entries
            .iter()
            .find(|entry| entry.task.as_str() == task_id)
            .unwrap()
            .blockers
            .iter()
            .filter(|blocker| {
                blocker.starts_with("verified_dependency_evidence_unavailable:")
                    || blocker.starts_with("admission_disabled:")
                    || blocker.starts_with("predecessor_failed:")
            })
            .cloned()
            .collect()
    }
    fn store_verified_receipt(
        db: &SqliteStore,
        task_id: &str,
        attempt: &str,
        result_id: &str,
        contract_revision: i64,
    ) {
        let digest = "d".repeat(64);
        let oid = "a".repeat(40);
        let have_contract: bool = db
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM task_contracts WHERE task_id=?1 AND contract_revision=?2)",
                params![task_id, contract_revision],
                |row| row.get(0),
            )
            .unwrap();
        if !have_contract {
            let installed: i64 = db
                .connection
                .query_row("SELECT COALESCE(MAX(sequence),1) FROM events", [], |row| {
                    row.get(0)
                })
                .unwrap();
            db.connection
                .execute(
                    "INSERT INTO task_contracts(task_id,contract_revision,plan_revision,project_store,expected_head,repository,base_oid,object_format,memory_snapshot_id,route,raw_bytes,raw_digest,installed_seq) VALUES(?1,?2,NULL,'/tmp/project',0,'/tmp/repo',?3,'sha1',NULL,'verify_only',?4,?5,?6)",
                    params![task_id, contract_revision, oid, vec![b'x'], digest.clone(), installed],
                )
                .unwrap();
        }
        // The run's other parents are not what this writer reads. Foreign keys
        // stay off only for the receipt insert.
        db.connection
            .execute_batch("PRAGMA foreign_keys=OFF;")
            .unwrap();
        db.connection
            .execute(
                "INSERT INTO verification_runs(run_id,project_store,idempotency_key,payload_digest,submission_id,task_id,contract_revision,contract_digest,attempt_id,policy_id,policy_digest,commit_oid,tree_oid,object_format,memory_fence,isolation,argv,library_manifest,state,reason,exit_status,receipt_digest,store_device,store_inode,created_unix_ms) VALUES(?1,'/tmp/project',?1,?2,?2,?3,?6,?2,?4,'policy',?2,?5,?5,'sha1',0,'linux-unshare-user-pid-mount-v1','[]','[]','accepted',NULL,0,?2,0,0,?6)",
                params![result_id, digest, task_id, attempt, oid, contract_revision],
            )
            .unwrap();
        db.connection
            .execute(
                "INSERT INTO verified_results(result_id,run_id,submission_id,commit_oid,tree_oid,object_format,policy_digest,receipt_digest,isolation,memory_fence,created_unix_ms) VALUES(?1,?1,?2,?3,?3,'sha1',?2,?2,'linux-unshare-user-pid-mount-v1',0,?4)",
                params![result_id, digest, oid, contract_revision],
            )
            .unwrap();
        db.connection
            .execute_batch("PRAGMA foreign_keys=ON;")
            .unwrap();
    }
    fn insert_integrated_commit(
        tx: &Connection,
        integrated_id: &str,
        verified_result_id: &str,
        commit_oid: &str,
    ) {
        let digest = "d".repeat(64);
        let old = "c".repeat(40);
        let candidate = format!("{:x}", Sha256::digest(integrated_id.as_bytes()));
        tx.execute(
            "INSERT INTO integration_operations(operation_id,project_store,idempotency_key,payload_digest,repository,ref_name,expected_old_oid,verified_result_id,candidate_id,state,generation,object_format,checks_passed,reason,created_unix_ms) VALUES(?1,'/tmp/project',?1,?2,'/tmp/repo','refs/heads/integration',?3,?4,NULL,'integrated',1,'sha1',1,NULL,1)",
            params![integrated_id, digest, old, verified_result_id],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO integrated_commits(integrated_id,candidate_id,operation_id,repository,ref_name,commit_oid,tree_oid,expected_old_oid,object_format,created_unix_ms) VALUES(?1,?2,?1,'/tmp/repo','refs/heads/integration',?3,?3,?4,'sha1',1)",
            params![integrated_id, candidate, commit_oid, old],
        )
        .unwrap();
    }

    #[test]
    fn schema_25_upgrade_preserves_landed_commit_bytes_and_ends_at_30() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.db");
        let mut db = SqliteStore::create(&path).unwrap();
        assert_eq!(user_version(&db.connection), 30);
        db.commit(Commit {
            expected_head: 0,
            mutations: vec![
                task("consumer", TaskState::Draft),
                task("pred", TaskState::Draft),
            ],
        })
        .unwrap();
        for (consumer, predecessor, requirement) in [
            ("consumer", "pred", "landed_commit"),
            ("pred", "consumer", "verified_result"),
        ] {
            db.connection
                .execute(
                    "INSERT INTO task_dependencies(task_id,predecessor_id,requirement) VALUES(?1,?2,?3)",
                    params![consumer, predecessor, requirement],
                )
                .unwrap();
        }
        let preserved = dependencies(&db.connection);
        assert!(preserved.iter().any(|row| row.2 == "landed_commit"));
        drop(db);
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.execute_batch(
            "DROP TABLE IF EXISTS dependency_satisfactions; DROP TABLE IF EXISTS factory_admission_policies;
             CREATE TABLE task_dependencies_v10 (
                task_id TEXT NOT NULL, predecessor_id TEXT NOT NULL,
                requirement TEXT NOT NULL CHECK(requirement IN ('verified_result','integration_candidate','landed_commit')),
                PRIMARY KEY(task_id,predecessor_id), CHECK(task_id<>predecessor_id)
             );
             INSERT INTO task_dependencies_v10 SELECT task_id, predecessor_id, requirement FROM task_dependencies;
             DROP TABLE task_dependencies;
             ALTER TABLE task_dependencies_v10 RENAME TO task_dependencies;
             ALTER TABLE project_control DROP COLUMN factory_admission;
             DROP TABLE IF EXISTS feedback_claims; DROP TABLE IF EXISTS feedback_items;
             DROP TABLE IF EXISTS integrated_commits; DROP TABLE IF EXISTS integration_candidates;
             DROP TABLE IF EXISTS integration_operations; DROP TABLE IF EXISTS integration_target_leases;
             DROP TABLE IF EXISTS integration_targets; DROP TABLE IF EXISTS verified_results;
             DROP TABLE IF EXISTS verification_runs; DROP TABLE IF EXISTS result_objects;
             DROP TABLE IF EXISTS result_submissions; DROP TABLE IF EXISTS acceptance_policies;
             DROP TABLE IF EXISTS task_contracts;
             UPDATE store_meta SET schema_version=25; PRAGMA user_version=25;",
        )
        .unwrap();
        drop(raw);
        let mut db = SqliteStore::open(&path).unwrap();
        assert_eq!(user_version(&db.connection), 25);
        let old_check: String = db
            .connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='task_dependencies'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!old_check.contains("integrated_commit"));
        assert_eq!(dependencies(&db.connection), preserved);
        assert!(matches!(
            db.import_legacy(&"cd".repeat(32), &[], &[]),
            Err(StoreError::UnsupportedSchema(25))
        ));
        assert_eq!(user_version(&db.connection), 25);
        db.upgrade_v1().unwrap();
        assert_eq!(user_version(&db.connection), 30);
        assert_eq!(dependencies(&db.connection), preserved);
        assert!(
            preserved
                .iter()
                .any(|row| row.0 == "consumer" && row.1 == "pred" && row.2 == "landed_commit")
        );
        let check_sql: String = db
            .connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='task_dependencies'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(check_sql.contains("integrated_commit"));
        assert!(check_sql.contains("landed_commit"));
        let admission: String = db
            .connection
            .query_row(
                "SELECT factory_admission FROM project_control WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(admission, "off");
        let policies: i64 = db
            .connection
            .query_row(
                "SELECT count(*) FROM factory_admission_policies",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(policies, 0);
        let satisfactions: i64 = db
            .connection
            .query_row("SELECT count(*) FROM dependency_satisfactions", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(satisfactions, 0);
        check_schema(&db.connection).unwrap();
        drop(db);
        let reopened = SqliteStore::open(&path).unwrap();
        assert_eq!(user_version(&reopened.connection), 30);
    }

    #[test]
    fn migration_defaults_factory_admission_off_and_release_code_does_not_write_it() {
        let temp = tempfile::tempdir().unwrap();
        let db = SqliteStore::create(&temp.path().join("state.db")).unwrap();
        let admission: String = db
            .connection
            .query_row(
                "SELECT factory_admission FROM project_control WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(admission, "off");
        assert!(!admission_enabled(&db.connection).unwrap());
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        fn walk(dir: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, files);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    files.push(path);
                }
            }
        }
        walk(&root, &mut files);
        for path in &files {
            let text = std::fs::read_to_string(path).unwrap();
            if text.contains("SET factory_admission") {
                assert!(
                    path.ends_with("src/store/satisfaction.rs"),
                    "production writer in {}",
                    path.display()
                );
            }
        }
        let source = std::fs::read_to_string(root.join("store/satisfaction.rs")).unwrap();
        let test_cfg = source.find("#[cfg(test)]").unwrap();
        let update_at = source.find("SET factory_admission").unwrap();
        assert!(update_at > test_cfg);
        assert!(!source[..test_cfg].contains("SET factory_admission"));
        assert!(!source[..test_cfg].contains("install_admission_policy"));
    }

    #[test]
    fn queue_report_satisfaction_rows_follow_the_admission_flag() {
        let temp = tempfile::tempdir().unwrap();
        let mut db = SqliteStore::create(&temp.path().join("state.db")).unwrap();
        db.commit(Commit {
            expected_head: 0,
            mutations: vec![
                task("pred", TaskState::Draft),
                task("needs-verified", TaskState::Draft),
                task("needs-integrated", TaskState::Draft),
            ],
        })
        .unwrap();
        let head = db.read_snapshot(None).unwrap().head;
        db.commit(Commit {
            expected_head: head,
            mutations: vec![Mutation::Attempt {
                expected: None,
                next: Attempt {
                    id: AttemptId::new("attempt-1").unwrap(),
                    task: TaskId::new("pred").unwrap(),
                    revision: 1,
                    state: AttemptState::Running,
                    snapshot: None,
                    reservation: "slot-pred".into(),
                    termination_observed: false,
                },
            }],
        })
        .unwrap();
        queue(
            &mut db,
            "needs-verified",
            "pred",
            DependencyRequirement::VerifiedResult,
        );
        queue(
            &mut db,
            "needs-integrated",
            "pred",
            DependencyRequirement::IntegratedCommit,
        );
        let blocked = db.queue_report(0).unwrap();
        assert!(!blocked.launch_enabled);
        assert!(!blocked.capability.automatic_admission);
        assert_eq!(
            dependency_lines(&blocked, "needs-verified"),
            vec!["verified_dependency_evidence_unavailable:pred:verified_result".to_string()]
        );
        assert_eq!(
            dependency_lines(&blocked, "needs-integrated"),
            vec!["verified_dependency_evidence_unavailable:pred:integrated_commit".to_string()]
        );
        let snapshot = db.read_snapshot(None).unwrap();
        let mut predecessor = snapshot
            .tasks
            .iter()
            .find(|item| item.id.as_str() == "pred")
            .unwrap()
            .clone();
        predecessor.revision += 1;
        predecessor.state = TaskState::Succeeded;
        db.commit(Commit {
            expected_head: snapshot.head,
            mutations: vec![Mutation::Task {
                expected: Some(1),
                next: predecessor,
            }],
        })
        .unwrap();
        let narrative = db.queue_report(0).unwrap();
        assert_eq!(
            dependency_lines(&narrative, "needs-verified"),
            vec!["verified_dependency_evidence_unavailable:pred:verified_result".to_string()]
        );
        let tx = db.connection.transaction().unwrap();
        assert!(record_verified_result(&tx, &"e".repeat(64)).is_err());
        assert_eq!(
            tx.query_row("SELECT count(*) FROM dependency_satisfactions", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
            0
        );
        drop(tx);
        let result_id = "e".repeat(64);
        store_verified_receipt(&db, "pred", "attempt-1", &result_id, 1);
        let tx = db.connection.transaction().unwrap();
        record_verified_result(&tx, &result_id).unwrap();
        let kinds: Vec<String> = {
            let mut stmt = tx
                .prepare("SELECT requirement FROM dependency_satisfactions ORDER BY requirement")
                .unwrap();
            stmt.query_map([], |row| row.get(0))
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap()
        };
        assert_eq!(kinds, vec!["verified_result".to_string()]);
        tx.commit().unwrap();
        let admitted_off = db.queue_report(0).unwrap();
        assert!(!admitted_off.launch_enabled);
        assert_eq!(
            dependency_lines(&admitted_off, "needs-verified"),
            vec!["admission_disabled:verified_result".to_string()]
        );
        assert_eq!(
            dependency_lines(&admitted_off, "needs-integrated"),
            vec!["verified_dependency_evidence_unavailable:pred:integrated_commit".to_string()]
        );
        assert!(
            admitted_off
                .entries
                .iter()
                .flat_map(|entry| &entry.blockers)
                .all(|blocker| !blocker.contains("admission_disabled:integrated_commit"))
        );
        db.testing_set_factory_admission(true).unwrap();
        let admitted_on = db.queue_report(0).unwrap();
        assert!(admitted_on.capability.automatic_admission);
        assert!(
            admitted_on
                .entries
                .iter()
                .flat_map(|entry| &entry.blockers)
                .chain(admitted_on.capability.blockers.iter())
                .all(|blocker| !blocker.contains("admission_disabled"))
        );
        assert!(dependency_lines(&admitted_on, "needs-verified").is_empty());
        assert_eq!(
            dependency_lines(&admitted_on, "needs-integrated"),
            vec!["verified_dependency_evidence_unavailable:pred:integrated_commit".to_string()]
        );
        // Other blockers still fill every entry, so the flag alone does not enable launch.
        assert!(!admitted_on.launch_enabled);
        assert!(
            admitted_on
                .entries
                .iter()
                .all(|entry| !entry.blockers.is_empty())
        );
    }

    fn queued_predecessor(db: &mut SqliteStore) {
        db.commit(Commit {
            expected_head: 0,
            mutations: vec![
                task("pred", TaskState::Draft),
                task("needs-verified", TaskState::Draft),
                task("needs-integrated", TaskState::Draft),
            ],
        })
        .unwrap();
        let head = db.read_snapshot(None).unwrap().head;
        db.commit(Commit {
            expected_head: head,
            mutations: vec![Mutation::Attempt {
                expected: None,
                next: Attempt {
                    id: AttemptId::new("attempt-1").unwrap(),
                    task: TaskId::new("pred").unwrap(),
                    revision: 1,
                    state: AttemptState::Running,
                    snapshot: None,
                    reservation: "slot-pred".into(),
                    termination_observed: false,
                },
            }],
        })
        .unwrap();
        queue(
            db,
            "needs-verified",
            "pred",
            DependencyRequirement::VerifiedResult,
        );
        queue(
            db,
            "needs-integrated",
            "pred",
            DependencyRequirement::IntegratedCommit,
        );
    }
    fn satisfaction_states(db: &Connection, requirement: &str) -> Vec<(String, String)> {
        let mut stmt = db
            .prepare(
                "SELECT evidence_id, state FROM dependency_satisfactions WHERE requirement=?1 ORDER BY state, evidence_id",
            )
            .unwrap();
        stmt.query_map([requirement], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
    }

    #[test]
    fn newer_receipt_replaces_valid_satisfaction_without_rolling_back_the_commit() {
        let temp = tempfile::tempdir().unwrap();
        let mut db = SqliteStore::create(&temp.path().join("state.db")).unwrap();
        queued_predecessor(&mut db);
        let first = "e".repeat(64);
        let second = "f".repeat(64);
        store_verified_receipt(&db, "pred", "attempt-1", &first, 1);
        store_verified_receipt(&db, "pred", "attempt-1", &second, 1);
        let tx = db.connection.transaction().unwrap();
        record_verified_result(&tx, &first).unwrap();
        record_verified_result(&tx, &second).unwrap();
        record_verified_result(&tx, &second).unwrap();
        tx.commit().unwrap();
        assert_eq!(
            satisfaction_states(&db.connection, "verified_result"),
            vec![(first, "invalid".into()), (second.clone(), "valid".into()),]
        );
        let integrated_a = "1".repeat(64);
        let integrated_b = "2".repeat(64);
        db.connection
            .execute_batch("PRAGMA foreign_keys=OFF;")
            .unwrap();
        let tx = db.connection.transaction().unwrap();
        insert_integrated_commit(&tx, &integrated_a, &second, &"a".repeat(40));
        record_integrated_commit(&tx, &integrated_a).unwrap();
        tx.commit().unwrap();
        // The second confirm inserts the new integrated commit and then replaces
        // the satisfaction. A Conflict there would undo cas_ref's commit forever.
        let tx = db.connection.transaction().unwrap();
        insert_integrated_commit(&tx, &integrated_b, &second, &"b".repeat(40));
        record_integrated_commit(&tx, &integrated_b).unwrap();
        tx.commit().unwrap();
        db.connection
            .execute_batch("PRAGMA foreign_keys=ON;")
            .unwrap();
        let commits: i64 = db
            .connection
            .query_row("SELECT count(*) FROM integrated_commits", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(commits, 2);
        assert_eq!(
            satisfaction_states(&db.connection, "integrated_commit"),
            vec![
                (integrated_a, "invalid".into()),
                (integrated_b, "valid".into()),
            ]
        );
    }

    #[test]
    fn superseded_attempt_or_older_contract_stays_unsatisfied() {
        let temp = tempfile::tempdir().unwrap();
        let mut db = SqliteStore::create(&temp.path().join("state.db")).unwrap();
        queued_predecessor(&mut db);
        let result_id = "e".repeat(64);
        store_verified_receipt(&db, "pred", "attempt-1", &result_id, 1);
        let tx = db.connection.transaction().unwrap();
        record_verified_result(&tx, &result_id).unwrap();
        tx.commit().unwrap();
        assert_eq!(
            dependency_lines(&db.queue_report(0).unwrap(), "needs-verified"),
            vec!["admission_disabled:verified_result".to_string()]
        );
        let head = db.read_snapshot(None).unwrap().head;
        db.commit(Commit {
            expected_head: head,
            mutations: vec![Mutation::Attempt {
                expected: None,
                next: Attempt {
                    id: AttemptId::new("attempt-2").unwrap(),
                    task: TaskId::new("pred").unwrap(),
                    revision: 1,
                    state: AttemptState::Running,
                    snapshot: None,
                    reservation: "slot-pred-2".into(),
                    termination_observed: false,
                },
            }],
        })
        .unwrap();
        db.connection
            .execute_batch(
                "UPDATE tasks SET active_attempt='attempt-2' WHERE id='pred';
                 UPDATE attempts SET state='completed', termination_observed=1 WHERE id='attempt-2';
                 UPDATE tasks SET active_attempt=NULL WHERE id='pred';",
            )
            .unwrap();
        assert_eq!(
            dependency_lines(&db.queue_report(0).unwrap(), "needs-verified"),
            vec!["verified_dependency_evidence_unavailable:pred:verified_result".to_string()]
        );
        assert_eq!(
            satisfaction_states(&db.connection, "verified_result"),
            vec![(result_id, "valid".into())]
        );

        let temp = tempfile::tempdir().unwrap();
        let mut db = SqliteStore::create(&temp.path().join("state.db")).unwrap();
        queued_predecessor(&mut db);
        let result_id = "e".repeat(64);
        store_verified_receipt(&db, "pred", "attempt-1", &result_id, 1);
        let tx = db.connection.transaction().unwrap();
        record_verified_result(&tx, &result_id).unwrap();
        tx.commit().unwrap();
        let installed: i64 = db
            .connection
            .query_row("SELECT MAX(sequence) FROM events", [], |row| row.get(0))
            .unwrap();
        db.connection
            .execute(
                "INSERT INTO task_contracts(task_id,contract_revision,plan_revision,project_store,expected_head,repository,base_oid,object_format,memory_snapshot_id,route,raw_bytes,raw_digest,installed_seq) VALUES('pred',2,NULL,'/tmp/project',0,'/tmp/repo',?1,'sha1',NULL,'verify_only',?2,?3,?4)",
                params!["b".repeat(40), vec![b'y'], "e".repeat(64), installed],
            )
            .unwrap();
        assert_eq!(
            dependency_lines(&db.queue_report(0).unwrap(), "needs-verified"),
            vec!["verified_dependency_evidence_unavailable:pred:verified_result".to_string()]
        );
        assert_eq!(
            satisfaction_states(&db.connection, "verified_result"),
            vec![(result_id, "valid".into())]
        );
    }

    #[test]
    fn open_memory_fence_hides_a_stored_satisfaction_until_it_resolves() {
        let temp = tempfile::tempdir().unwrap();
        let mut db = SqliteStore::create(&temp.path().join("state.db")).unwrap();
        queued_predecessor(&mut db);
        db.connection
            .execute(
                "INSERT INTO memory_invalidations(id,task_id,proposal_id,record_id,severity,triggering_seq,resolved_seq,reason) VALUES('inv-1','pred','proposal-1',NULL,'stop_at_checkpoint',1,NULL,'fence open')",
                [],
            )
            .unwrap();
        let result_id = "e".repeat(64);
        store_verified_receipt(&db, "pred", "attempt-1", &result_id, 1);
        let tx = db.connection.transaction().unwrap();
        record_verified_result(&tx, &result_id).unwrap();
        tx.commit().unwrap();
        assert_eq!(
            satisfaction_states(&db.connection, "verified_result"),
            vec![(result_id.clone(), "valid".into())]
        );
        assert_eq!(
            dependency_lines(&db.queue_report(0).unwrap(), "needs-verified"),
            vec!["verified_dependency_evidence_unavailable:pred:verified_result".to_string()]
        );
        let queued_before: i64 = db
            .connection
            .query_row(
                "SELECT enqueue_sequence FROM task_queue WHERE task_id='needs-verified'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        db.connection
            .execute(
                "UPDATE memory_invalidations SET resolved_seq=1 WHERE id='inv-1'",
                [],
            )
            .unwrap();
        assert_eq!(
            dependency_lines(&db.queue_report(0).unwrap(), "needs-verified"),
            vec!["admission_disabled:verified_result".to_string()]
        );
        let queued_after: i64 = db
            .connection
            .query_row(
                "SELECT enqueue_sequence FROM task_queue WHERE task_id='needs-verified'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(queued_before, queued_after);
        assert_eq!(
            satisfaction_states(&db.connection, "verified_result"),
            vec![(result_id.clone(), "valid".into())]
        );
    }

    #[test]
    fn launch_enabled_is_true_only_when_the_flag_is_on_and_an_entry_is_clear() {
        let entry = |id: &str, blockers: Vec<String>| QueueEntry {
            task: TaskId::new(id).unwrap(),
            task_revision: 1,
            effective_priority: 0,
            blockers,
        };
        assert!(!super::super::scheduler::automatic_launch_enabled(
            false,
            &[entry("b", Vec::new())]
        ));
        assert!(!super::super::scheduler::automatic_launch_enabled(
            true,
            &[entry("a", vec!["capacity_full".into()])]
        ));
        assert!(super::super::scheduler::automatic_launch_enabled(
            true,
            &[entry("b", Vec::new())]
        ));
    }
}

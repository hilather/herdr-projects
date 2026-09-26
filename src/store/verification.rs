//! Schema 27 verification runs. Acceptance is one short transaction.
//! It does not release a worker slot. Acceptance writes a satisfaction row for a
//! matching `verified_result` edge; that row is the evidence.
use super::*;
use crate::verification::{ISOLATION, VerificationReceipt};
use rusqlite::OptionalExtension;
use std::{
    fs::File,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

const SCHEMA_VERSION: u32 = 27;

fn invalid(message: &str) -> StoreError {
    StoreError::Invalid(message.into())
}
fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn schema27(db: &Connection) -> Result<()> {
    check_schema(db)?;
    let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchema(version));
    }
    Ok(())
}

#[derive(Clone)]
pub(crate) struct RetainedObject {
    pub oid: String,
    pub relative_path: String,
    pub bytes: Vec<u8>,
}

pub(crate) struct VerifyTarget {
    pub project_store: String,
    pub submission_id: String,
    pub payload_digest: String,
    pub task_id: String,
    pub contract_revision: i64,
    pub contract_digest: String,
    pub attempt_id: String,
    pub attempt_revision: i64,
    pub candidate_oid: String,
    pub object_format: String,
    pub policy_id: String,
    pub policy_body: String,
    pub policy_digest: String,
    pub memory_fence: u64,
    pub store_device: i64,
    pub store_inode: i64,
    pub objects: Vec<RetainedObject>,
    pin: File,
}

pub(crate) struct ExistingRun {
    pub run_id: String,
    pub payload_digest: String,
    pub state: String,
    pub reason: Option<String>,
    pub argv: Vec<String>,
}

pub(crate) struct RunDraft {
    pub idempotency_key: String,
    pub payload_digest: String,
    pub argv: Vec<String>,
    pub libraries: Vec<String>,
    pub tree_oid: Option<String>,
    pub exit_status: Option<i32>,
    pub reason: Option<String>,
    pub receipt: Option<VerificationReceipt>,
}

fn loose_relative(path: &str) -> bool {
    let mut parts = path.split('/');
    let Some(prefix) = parts.next() else {
        return false;
    };
    let Some(rest) = parts.next() else {
        return false;
    };
    parts.next().is_none()
        && prefix.len() == 2
        && !rest.is_empty()
        && prefix.chars().all(|c| c.is_ascii_hexdigit())
        && rest.chars().all(|c| c.is_ascii_hexdigit())
}

fn staging_file(db_path: &Path, idempotency_key: &str, byte_sha256: &str) -> PathBuf {
    db_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("factory-objects")
        .join("staging")
        .join(sha256_hex(idempotency_key.as_bytes()))
        .join(byte_sha256)
}

fn read_retained(path: &Path, expected_hash: &str, size: u64) -> Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| invalid("missing retained object"))?;
    let meta = file
        .metadata()
        .map_err(|error| StoreError::Io(error.to_string()))?;
    if !meta.is_file() || meta.len() != size {
        return Err(invalid("missing retained object"));
    }
    let mut bytes = Vec::new();
    file.take(size + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| StoreError::Io(error.to_string()))?;
    if bytes.len() as u64 != size || sha256_hex(&bytes) != expected_hash {
        return Err(invalid("retained object bytes changed"));
    }
    Ok(bytes)
}

impl VerifyTarget {
    fn pin_matches(&self) -> Result<()> {
        let meta = self
            .pin
            .metadata()
            .map_err(|error| StoreError::Io(error.to_string()))?;
        let dev = i64::try_from(meta.dev()).map_err(|_| invalid("store device does not fit"))?;
        let ino = i64::try_from(meta.ino()).map_err(|_| invalid("store inode does not fit"))?;
        if dev != self.store_device || ino != self.store_inode {
            return Err(invalid("project store changed"));
        }
        Ok(())
    }
}

impl SqliteStore {
    pub(crate) fn load_verify_target(
        &mut self,
        submission_id: &str,
        policy_id: &str,
    ) -> Result<VerifyTarget> {
        let path = std::fs::canonicalize(
            self.connection
                .path()
                .ok_or_else(|| invalid("store path missing"))?,
        )
        .map_err(|error| StoreError::Io(error.to_string()))?;
        let pin = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|error| StoreError::Io(error.to_string()))?;
        let meta = pin
            .metadata()
            .map_err(|error| StoreError::Io(error.to_string()))?;
        if !meta.is_file() {
            return Err(invalid("project store changed"));
        }
        let store_device =
            i64::try_from(meta.dev()).map_err(|_| invalid("store device does not fit"))?;
        let store_inode =
            i64::try_from(meta.ino()).map_err(|_| invalid("store inode does not fit"))?;
        let tx = self.connection.transaction()?;
        schema27(&tx)?;
        let project_store = path.to_string_lossy().into_owned();
        let row = tx
            .query_row(
                "SELECT s.payload_digest, s.idempotency_key, s.task_id, s.contract_revision, s.contract_digest, s.attempt_id, s.candidate_oid, s.object_format, a.revision, p.body, c.raw_bytes, c.raw_digest
                 FROM result_submissions s
                 JOIN attempts a ON a.id = s.attempt_id AND a.task_id = s.task_id
                 JOIN task_contracts c ON c.task_id = s.task_id AND c.contract_revision = s.contract_revision
                 JOIN acceptance_policies p ON p.task_id = s.task_id AND p.contract_revision = s.contract_revision AND p.policy_id = ?3
                 WHERE s.submission_id = ?1 AND s.project_store = ?2",
                params![submission_id, project_store, policy_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, i64>(8)?,
                        row.get::<_, String>(9)?,
                        row.get::<_, Vec<u8>>(10)?,
                        row.get::<_, String>(11)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            payload_digest,
            submission_key,
            task_id,
            contract_revision,
            contract_digest,
            attempt_id,
            candidate_oid,
            object_format,
            attempt_revision,
            policy_body,
            raw,
            raw_digest,
        )) = row
        else {
            return Err(invalid("verification target is missing"));
        };
        if sha256_hex(&raw) != raw_digest || raw_digest != contract_digest {
            return Err(StoreError::Corrupt("task contract digest mismatch".into()));
        }
        let mut objects = Vec::new();
        {
            let mut stmt = tx.prepare(
                "SELECT oid, relative_path, byte_sha256, size FROM result_objects WHERE submission_id = ?1 ORDER BY oid",
            )?;
            let mut rows = stmt.query([submission_id])?;
            while let Some(row) = rows.next()? {
                let relative_path: String = row.get(1)?;
                if !loose_relative(&relative_path) {
                    return Err(invalid("retained object path is not a loose git object"));
                }
                objects.push((
                    row.get::<_, String>(0)?,
                    relative_path,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ));
            }
        }
        if objects.is_empty() || !objects.iter().any(|(oid, _, _, _)| oid == &candidate_oid) {
            return Err(invalid("missing retained object"));
        }
        let policy_digest = sha256_hex(policy_body.as_bytes());
        let memory_fence = head(&tx)?;
        tx.commit()?;
        let mut retained = Vec::with_capacity(objects.len());
        for (oid, relative_path, byte_sha256, size) in objects {
            let size =
                u64::try_from(size).map_err(|_| invalid("retained object size is invalid"))?;
            let bytes = read_retained(
                &staging_file(&path, &submission_key, &byte_sha256),
                &byte_sha256,
                size,
            )?;
            retained.push(RetainedObject {
                oid,
                relative_path,
                bytes,
            });
        }
        Ok(VerifyTarget {
            project_store,
            submission_id: submission_id.to_string(),
            payload_digest,
            task_id,
            contract_revision,
            contract_digest,
            attempt_id,
            attempt_revision,
            candidate_oid,
            object_format,
            policy_id: policy_id.to_string(),
            policy_body,
            policy_digest,
            memory_fence,
            store_device,
            store_inode,
            objects: retained,
            pin,
        })
    }

    pub(crate) fn lookup_verification(
        &mut self,
        project_store: &str,
        idempotency_key: &str,
    ) -> Result<Option<ExistingRun>> {
        let tx = self.connection.transaction()?;
        schema27(&tx)?;
        let found = lookup(&tx, project_store, idempotency_key)?;
        tx.commit()?;
        Ok(found)
    }

    /// Record the run. A receipt is inserted only when the recheck still matches.
    /// Attempts and dependency rows are left untouched.
    pub(crate) fn commit_verification(
        &mut self,
        target: &VerifyTarget,
        draft: RunDraft,
    ) -> Result<(ExistingRun, Option<VerificationReceipt>)> {
        target.pin_matches()?;
        let tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        schema27(&tx)?;
        if let Some(existing) = lookup(&tx, &target.project_store, &draft.idempotency_key)? {
            if existing.payload_digest != draft.payload_digest {
                return Err(StoreError::Conflict);
            }
            // A rejected run recorded before feedback existed is repaired here.
            // The same key still inserts at most one row.
            if existing.state == "rejected" {
                let version: u32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
                if version >= 29 {
                    let (task_id, stored_reason): (String, String) = tx.query_row(
                        "SELECT task_id, reason FROM verification_runs WHERE run_id=?1",
                        [&existing.run_id],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )?;
                    super::feedback::insert_feedback(
                        &tx,
                        &super::feedback::LocalFeedback {
                            operation_id: existing.run_id.clone(),
                            outcome_revision: 1,
                            category: "verifier_rejection".into(),
                            task_id,
                            reason: stored_reason,
                        },
                    )?;
                }
            }
            tx.commit()?;
            return Ok((existing, None));
        }
        let fresh = reload(&tx, target)?;
        let expected_run = crate::verification::run_identity(
            &target.project_store,
            &draft.idempotency_key,
            &draft.payload_digest,
        );
        let expected_result = crate::verification::result_identity(
            &expected_run,
            draft.tree_oid.as_deref().unwrap_or(""),
            &fresh.policy_digest,
        );
        let receipt_ok = draft.receipt.as_ref().is_some_and(|receipt| {
            receipt_matches(receipt, &fresh)
                && receipt.run_id() == expected_run
                && receipt.result_id() == expected_result
                && receipt.binding_ok()
        });
        let accepting = draft.reason.is_none() && receipt_ok && fresh.same_generation(target);
        let (state, reason, receipt) = if accepting {
            ("accepted", None, draft.receipt.as_ref())
        } else if !fresh.same_generation(target) || (draft.reason.is_none() && !receipt_ok) {
            ("rejected", Some("stale_verification"), None)
        } else {
            (
                "rejected",
                draft.reason.as_deref().or(Some("isolation_setup_failed")),
                None,
            )
        };
        let reason = reason.filter(|_| state == "rejected");
        let tree_oid = if state == "accepted" {
            draft
                .tree_oid
                .clone()
                .or_else(|| receipt.map(|item| item.tree_oid().to_string()))
        } else {
            draft.tree_oid.clone()
        };
        if state == "accepted" && tree_oid.is_none() {
            return Err(invalid("accepted verification is missing a tree"));
        }
        let run_id = expected_run;
        let receipt_digest = receipt.map(VerificationReceipt::digest);
        let result_id = if state == "accepted" {
            Some(expected_result)
        } else {
            None
        };
        let argv =
            serde_json::to_string(&draft.argv).map_err(|error| invalid(&error.to_string()))?;
        let libraries =
            serde_json::to_string(&draft.libraries).map_err(|error| invalid(&error.to_string()))?;
        let exit_status = if state == "accepted" {
            Some(0)
        } else {
            draft.exit_status
        };
        let now = jiff::Timestamp::now().as_millisecond();
        tx.execute(
            "INSERT INTO verification_runs(run_id,project_store,idempotency_key,payload_digest,submission_id,task_id,contract_revision,contract_digest,attempt_id,policy_id,policy_digest,commit_oid,tree_oid,object_format,memory_fence,isolation,argv,library_manifest,state,reason,exit_status,receipt_digest,store_device,store_inode,created_unix_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25)",
            params![
                run_id,
                target.project_store,
                draft.idempotency_key,
                draft.payload_digest,
                target.submission_id,
                target.task_id,
                target.contract_revision,
                fresh.contract_digest,
                target.attempt_id,
                target.policy_id,
                fresh.policy_digest,
                target.candidate_oid,
                tree_oid,
                target.object_format,
                integer(target.memory_fence)?,
                ISOLATION,
                argv,
                libraries,
                state,
                reason,
                exit_status,
                receipt_digest,
                target.store_device,
                target.store_inode,
                now
            ],
        )?;
        if state == "accepted" {
            let digest = receipt_digest
                .ok_or_else(|| invalid("accepted verification is missing a receipt"))?;
            let stored_result = result_id.as_deref().unwrap();
            tx.execute(
                "INSERT INTO verified_results(result_id,run_id,submission_id,commit_oid,tree_oid,object_format,policy_digest,receipt_digest,isolation,memory_fence,created_unix_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                params![
                    stored_result,
                    run_id,
                    target.submission_id,
                    target.candidate_oid,
                    tree_oid.unwrap(),
                    target.object_format,
                    fresh.policy_digest,
                    digest,
                    ISOLATION,
                    integer(target.memory_fence)?,
                    now
                ],
            )?;
            // Narrative acceptance is not enough; only this stored receipt can satisfy.
            super::satisfaction::record_verified_result(&tx, stored_result)?;
        }
        if state == "rejected" {
            super::feedback::insert_feedback(
                &tx,
                &super::feedback::LocalFeedback {
                    operation_id: run_id.clone(),
                    outcome_revision: 1,
                    category: "verifier_rejection".into(),
                    task_id: target.task_id.clone(),
                    reason: reason.unwrap_or("rejected").to_string(),
                },
            )?;
        }
        tx.execute(
            "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES(?1,?2,1,1,?3)",
            params![
                if state == "accepted" { "verification.accepted" } else { "verification.rejected" },
                run_id,
                serde_json::json!({"state": state, "reason": reason, "submission_id": target.submission_id}).to_string()
            ],
        )?;
        tx.commit()?;
        let returned = if state == "accepted" {
            draft.receipt
        } else {
            None
        };
        Ok((
            ExistingRun {
                run_id,
                payload_digest: draft.payload_digest,
                state: state.to_string(),
                reason: reason.map(str::to_string),
                argv: draft.argv,
            },
            returned,
        ))
    }

    #[cfg(test)]
    pub(crate) fn verified_result_count(&self) -> Result<i64> {
        schema27(&self.connection)?;
        Ok(self
            .connection
            .query_row("SELECT count(*) FROM verified_results", [], |row| {
                row.get(0)
            })?)
    }

    #[cfg(test)]
    pub(crate) fn capacity_fingerprint(&self) -> Result<Vec<(String, i64, String)>> {
        schema27(&self.connection)?;
        let mut stmt = self
            .connection
            .prepare("SELECT id, termination_observed, reservation FROM attempts ORDER BY id")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    #[cfg(test)]
    pub(crate) fn dependency_count(&self) -> Result<i64> {
        schema27(&self.connection)?;
        Ok(self
            .connection
            .query_row("SELECT count(*) FROM task_dependencies", [], |row| {
                row.get(0)
            })?)
    }

    #[cfg(test)]
    pub(crate) fn testing_append_event(&mut self) -> Result<()> {
        let tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        schema27(&tx)?;
        tx.execute(
            "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('verification.fence','fence',1,1,'{}')",
            [],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// One poll of a rejected verification. A second call is the duplicate poll.
    #[cfg(test)]
    pub(crate) fn testing_poll_rejected_verification(&mut self) -> Result<ExistingRun> {
        self.seed_rejected_verification()?;
        let target = self.rejected_verification_target()?;
        let (stored, _) = self.commit_verification(
            &target,
            RunDraft {
                idempotency_key: "verify-reject-1".into(),
                payload_digest: sha256_hex(b"verifier-rejection-payload"),
                argv: vec!["/usr/bin/true".into()],
                libraries: vec![],
                tree_oid: None,
                exit_status: Some(1),
                reason: Some("checks_failed".into()),
                receipt: None,
            },
        )?;
        Ok(stored)
    }

    #[cfg(test)]
    fn seed_rejected_verification(&mut self) -> Result<()> {
        use crate::domain::*;
        let path = std::fs::canonicalize(
            self.connection
                .path()
                .ok_or_else(|| invalid("store path missing"))?,
        )
        .map_err(|error| StoreError::Io(error.to_string()))?;
        let project_store = path
            .to_str()
            .ok_or_else(|| invalid("store path is not utf-8"))?
            .to_string();
        let submission_id = "c".repeat(64);
        let exists: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM result_submissions WHERE submission_id=?1)",
            [&submission_id],
            |row| row.get(0),
        )?;
        if exists {
            return Ok(());
        }
        self.commit(Commit {
            expected_head: 0,
            mutations: vec![
                Mutation::Task {
                    expected: None,
                    next: Task {
                        id: TaskId::new("consumer").unwrap(),
                        revision: 1,
                        state: TaskState::Draft,
                        title: "consumer".into(),
                        active_attempt: None,
                    },
                },
                Mutation::Task {
                    expected: None,
                    next: Task {
                        id: TaskId::new("task").unwrap(),
                        revision: 1,
                        state: TaskState::Running,
                        title: "verify".into(),
                        active_attempt: None,
                    },
                },
                Mutation::Attempt {
                    expected: None,
                    next: Attempt {
                        id: AttemptId::new("attempt-1").unwrap(),
                        task: TaskId::new("task").unwrap(),
                        revision: 1,
                        state: AttemptState::Running,
                        snapshot: None,
                        reservation: "slot-1".into(),
                        termination_observed: false,
                    },
                },
            ],
        })?;
        self.connection.execute(
            "INSERT INTO task_dependencies(task_id,predecessor_id,requirement) VALUES('consumer','task','landed_commit')",
            [],
        )?;
        let installed: i64 = self
            .connection
            .query_row("SELECT max(sequence) FROM events", [], |row| row.get(0))?;
        let raw = b"contract-v1";
        let raw_digest = sha256_hex(raw);
        let policy = r#"{"version":1,"checks":["/usr/bin/true"]}"#;
        let oid = "a".repeat(40);
        let base = "b".repeat(40);
        let tx = self.connection.transaction()?;
        tx.execute(
            "INSERT INTO task_contracts(task_id,contract_revision,plan_revision,project_store,expected_head,repository,base_oid,object_format,memory_snapshot_id,route,raw_bytes,raw_digest,installed_seq) VALUES('task',1,NULL,?1,0,?2,?3,'sha1',NULL,'verify_only',?4,?5,?6)",
            params![project_store, "/tmp/repo", base, raw, raw_digest, installed],
        )?;
        tx.execute(
            "INSERT INTO acceptance_policies(task_id,contract_revision,policy_id,body) VALUES('task',1,'policy-1',?1)",
            [policy],
        )?;
        tx.execute(
            "INSERT INTO result_submissions(submission_id,project_store,idempotency_key,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,memory_snapshot_id,artifact_manifest,claimed_checks,created_unix_ms) VALUES(?1,?2,'submit-1',?3,'{}','task',1,?4,'attempt-1',?5,?6,?7,'sha1',NULL,'[]','[]',0)",
            params![
                submission_id,
                project_store,
                sha256_hex(b"verifier-rejection-payload"),
                raw_digest,
                "/tmp/repo",
                base,
                oid
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    #[cfg(test)]
    fn rejected_verification_target(&mut self) -> Result<VerifyTarget> {
        let path = std::fs::canonicalize(
            self.connection
                .path()
                .ok_or_else(|| invalid("store path missing"))?,
        )
        .map_err(|error| StoreError::Io(error.to_string()))?;
        let pin = std::fs::File::open(&path).map_err(|error| StoreError::Io(error.to_string()))?;
        let meta = pin
            .metadata()
            .map_err(|error| StoreError::Io(error.to_string()))?;
        let project_store = path
            .to_str()
            .ok_or_else(|| invalid("store path is not utf-8"))?
            .to_string();
        let policy = r#"{"version":1,"checks":["/usr/bin/true"]}"#;
        let raw_digest = sha256_hex(b"contract-v1");
        Ok(VerifyTarget {
            project_store,
            submission_id: "c".repeat(64),
            payload_digest: sha256_hex(b"verifier-rejection-payload"),
            task_id: "task".into(),
            contract_revision: 1,
            contract_digest: raw_digest,
            attempt_id: "attempt-1".into(),
            attempt_revision: 1,
            candidate_oid: "a".repeat(40),
            object_format: "sha1".into(),
            policy_id: "policy-1".into(),
            policy_body: policy.into(),
            policy_digest: sha256_hex(policy.as_bytes()),
            memory_fence: head(&self.connection)?,
            store_device: i64::try_from(meta.dev()).map_err(|_| invalid("store device does not fit"))?,
            store_inode: i64::try_from(meta.ino()).map_err(|_| invalid("store inode does not fit"))?,
            objects: Vec::new(),
            pin,
        })
    }
}

struct Reloaded {
    contract_digest: String,
    policy_digest: String,
    attempt_revision: i64,
    payload_digest: String,
    candidate_oid: String,
    memory_fence: u64,
    store_device: i64,
    store_inode: i64,
}

impl Reloaded {
    fn same_generation(&self, target: &VerifyTarget) -> bool {
        self.contract_digest == target.contract_digest
            && self.policy_digest == target.policy_digest
            && self.attempt_revision == target.attempt_revision
            && self.payload_digest == target.payload_digest
            && self.candidate_oid == target.candidate_oid
            && self.memory_fence == target.memory_fence
            && self.store_device == target.store_device
            && self.store_inode == target.store_inode
    }
}

fn reload(db: &Connection, target: &VerifyTarget) -> Result<Reloaded> {
    let row = db
        .query_row(
            "SELECT s.payload_digest, s.candidate_oid, c.raw_digest, a.revision, p.body
             FROM result_submissions s
             JOIN attempts a ON a.id = s.attempt_id AND a.task_id = s.task_id
             JOIN task_contracts c ON c.task_id = s.task_id AND c.contract_revision = s.contract_revision
             JOIN acceptance_policies p ON p.task_id = s.task_id AND p.contract_revision = s.contract_revision AND p.policy_id = ?2
             WHERE s.submission_id = ?1",
            params![target.submission_id, target.policy_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, i64>(3)?, row.get::<_, String>(4)?)),
        )
        .optional()?;
    let Some((payload_digest, candidate_oid, contract_digest, attempt_revision, policy_body)) = row
    else {
        return Err(invalid("verification target is missing"));
    };
    let pin_meta = target
        .pin
        .metadata()
        .map_err(|error| StoreError::Io(error.to_string()))?;
    Ok(Reloaded {
        contract_digest,
        policy_digest: sha256_hex(policy_body.as_bytes()),
        attempt_revision,
        payload_digest,
        candidate_oid,
        memory_fence: head(db)?,
        store_device: i64::try_from(pin_meta.dev())
            .map_err(|_| invalid("store device does not fit"))?,
        store_inode: i64::try_from(pin_meta.ino())
            .map_err(|_| invalid("store inode does not fit"))?,
    })
}

fn receipt_matches(receipt: &VerificationReceipt, fresh: &Reloaded) -> bool {
    receipt.isolation() == ISOLATION
        && receipt.exit_status() == 0
        && receipt.policy_digest() == fresh.policy_digest
        && receipt.commit_oid() == fresh.candidate_oid
        && receipt.memory_fence() == fresh.memory_fence
        && receipt.store_device() == fresh.store_device
        && receipt.store_inode() == fresh.store_inode
}

fn lookup(
    db: &Connection,
    project_store: &str,
    idempotency_key: &str,
) -> Result<Option<ExistingRun>> {
    let row = db
        .query_row(
            "SELECT run_id, payload_digest, state, reason, argv FROM verification_runs WHERE project_store = ?1 AND idempotency_key = ?2",
            params![project_store, idempotency_key],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, Option<String>>(3)?, row.get::<_, String>(4)?)),
        )
        .optional()?;
    let Some((run_id, payload_digest, state, reason, argv)) = row else {
        return Ok(None);
    };
    let argv: Vec<String> = serde_json::from_str(&argv)
        .map_err(|_| StoreError::Corrupt("verification argv is invalid".into()))?;
    Ok(Some(ExistingRun {
        run_id,
        payload_digest,
        state,
        reason,
        argv,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_version(db: &Connection) -> u32 {
        db.query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }
    fn table_exists(db: &Connection, name: &str) -> bool {
        db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            [name],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn upgrade_v1_from_26_to_27_and_open_does_not_migrate() {
        let fresh = tempfile::tempdir().unwrap();
        let fresh_path = fresh.path().join("state.db");
        let created = SqliteStore::create(&fresh_path).unwrap();
        assert_eq!(user_version(&created.connection), 35);
        assert!(table_exists(&created.connection, "verification_runs"));
        assert!(table_exists(&created.connection, "verified_results"));
        drop(created);

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.db");
        let mut db = SqliteStore::create(&path).unwrap();
        db.import_legacy(&"ab".repeat(32), &[], &[]).unwrap();
        drop(db);
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.execute_batch(
            "DROP TABLE IF EXISTS resource_claims; DROP TABLE IF EXISTS delegation_stop_obligations; DROP TABLE IF EXISTS delegation_revocations; DROP TABLE IF EXISTS delegation_grants; DROP TABLE IF EXISTS capability_evidence; DROP TABLE IF EXISTS contract_named_resources; DROP TABLE IF EXISTS contract_scope_paths; DROP TABLE IF EXISTS plan_revisions; DROP TABLE IF EXISTS plan_proposals; DROP TABLE IF EXISTS dependency_satisfactions; DROP TABLE IF EXISTS factory_admission_policies; ALTER TABLE project_control DROP COLUMN factory_admission; DROP TABLE IF EXISTS feedback_claims; DROP TABLE IF EXISTS feedback_items; DROP TABLE IF EXISTS integrated_commits; DROP TABLE IF EXISTS integration_candidates; DROP TABLE IF EXISTS integration_operations; DROP TABLE IF EXISTS integration_target_leases; DROP TABLE IF EXISTS integration_targets; DROP TABLE IF EXISTS verified_results; DROP TABLE IF EXISTS verification_runs; UPDATE store_meta SET schema_version = 26; PRAGMA user_version = 26;",
        )
        .unwrap();
        drop(raw);
        let mut db = SqliteStore::open(&path).unwrap();
        assert_eq!(user_version(&db.connection), 26);
        assert!(!table_exists(&db.connection, "verification_runs"));
        assert!(!table_exists(&db.connection, "verified_results"));
        assert!(matches!(
            db.import_legacy(&"cd".repeat(32), &[], &[]),
            Err(StoreError::UnsupportedSchema(26))
        ));
        db.upgrade_v1().unwrap();
        assert_eq!(user_version(&db.connection), 35);
        assert_eq!(
            db.connection
                .query_row("SELECT schema_version FROM store_meta", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            35
        );
        assert!(table_exists(&db.connection, "verification_runs"));
        assert!(table_exists(&db.connection, "verified_results"));
        check_schema(&db.connection).unwrap();
        drop(db);
        let reopened = SqliteStore::open(&path).unwrap();
        assert_eq!(user_version(&reopened.connection), 35);
        assert!(table_exists(&reopened.connection, "verified_results"));
    }

    #[test]
    fn duplicate_verifier_rejection_polls_do_not_insert_duplicate_feedback() {
        let temp = tempfile::tempdir().unwrap();
        let mut db = SqliteStore::create(&temp.path().join("state.db")).unwrap();
        let first = db.testing_poll_rejected_verification().unwrap();
        let attempts: i64 = db
            .connection
            .query_row("SELECT count(*) FROM attempts", [], |row| row.get(0))
            .unwrap();
        let deps: Vec<(String, String, String)> = {
            let mut stmt = db
                .connection
                .prepare(
                    "SELECT task_id, predecessor_id, requirement FROM task_dependencies ORDER BY task_id, predecessor_id",
                )
                .unwrap();
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap()
        };
        let second = db.testing_poll_rejected_verification().unwrap();
        assert_eq!(first.state, "rejected");
        assert_eq!(first.reason.as_deref(), Some("checks_failed"));
        assert_eq!(first.run_id, second.run_id);
        assert_eq!(
            db.connection
                .query_row("SELECT count(*) FROM feedback_items", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            db.connection
                .query_row("SELECT count(*) FROM verified_results", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            db.connection
                .query_row("SELECT count(*) FROM attempts", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            attempts
        );
        let after: Vec<(String, String, String)> = {
            let mut stmt = db
                .connection
                .prepare(
                    "SELECT task_id, predecessor_id, requirement FROM task_dependencies ORDER BY task_id, predecessor_id",
                )
                .unwrap();
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap()
        };
        assert_eq!(after, deps);
        assert_eq!(
            deps,
            vec![("consumer".into(), "task".into(), "landed_commit".into())]
        );
    }

    #[test]
    fn verifier_rejection_below_schema_29_rolls_back_without_feedback() {
        let temp = tempfile::tempdir().unwrap();
        let mut db = SqliteStore::create(&temp.path().join("state.db")).unwrap();
        db.connection
            .execute_batch("UPDATE store_meta SET schema_version=28; PRAGMA user_version=28;")
            .unwrap();
        assert!(matches!(
            db.testing_poll_rejected_verification(),
            Err(StoreError::UnsupportedSchema(28))
        ));
        assert_eq!(
            db.connection
                .query_row("SELECT count(*) FROM verification_runs", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            db.connection
                .query_row("SELECT count(*) FROM feedback_items", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn duplicate_poll_inserts_missing_feedback_once() {
        let temp = tempfile::tempdir().unwrap();
        let mut db = SqliteStore::create(&temp.path().join("state.db")).unwrap();
        let first = db.testing_poll_rejected_verification().unwrap();
        db.connection
            .execute_batch("DROP TRIGGER IF EXISTS feedback_items_no_delete; DELETE FROM feedback_items;")
            .unwrap();
        assert_eq!(
            db.connection
                .query_row("SELECT count(*) FROM feedback_items", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            0
        );
        let repaired = db.testing_poll_rejected_verification().unwrap();
        assert_eq!(repaired.run_id, first.run_id);
        assert_eq!(
            db.connection
                .query_row("SELECT count(*) FROM feedback_items", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            db.connection
                .query_row("SELECT count(*) FROM verification_runs", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
        db.testing_poll_rejected_verification().unwrap();
        assert_eq!(
            db.connection
                .query_row("SELECT count(*) FROM feedback_items", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
        let deps: Vec<(String, String, String)> = {
            let mut stmt = db
                .connection
                .prepare(
                    "SELECT task_id, predecessor_id, requirement FROM task_dependencies ORDER BY task_id, predecessor_id",
                )
                .unwrap();
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap()
        };
        assert_eq!(
            deps,
            vec![("consumer".into(), "task".into(), "landed_commit".into())]
        );
    }
}

//! Schema 29 local feedback. Rows are written only in the same transaction as a
//! verifier or integrator rejection. A pull-request poll is not evidence, and
//! ack does not reserve an attempt or satisfy a dependency.
use super::*;
use rusqlite::OptionalExtension;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::Path;

const SCHEMA_VERSION: u32 = 29;

fn invalid(message: &str) -> StoreError {
    StoreError::Invalid(message.into())
}
fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn schema29(db: &Connection) -> Result<()> {
    check_schema(db)?;
    let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchema(version));
    }
    Ok(())
}
fn identifier(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
    {
        return Err(invalid("feedback identifier is invalid"));
    }
    Ok(())
}
fn reason_ok(value: &str) -> bool {
    (1..=128).contains(&value.len()) && value.chars().all(|c| c.is_ascii() && !c.is_control())
}
fn allowed_category(category: &str) -> bool {
    matches!(
        category,
        "verifier_rejection" | "integrator_rejection" | "integrator_conflict" | "invalidation"
    )
}

pub(crate) struct LocalFeedback {
    pub operation_id: String,
    pub outcome_revision: i64,
    pub category: String,
    pub task_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FeedbackItem {
    pub feedback_id: String,
    pub operation_id: String,
    pub outcome_revision: i64,
    pub category: String,
    pub task_id: String,
    pub reason: String,
    pub state: String,
    pub replan_proposal_id: Option<String>,
    pub created_unix_ms: i64,
    pub claim_epoch: Option<i64>,
    pub owner: Option<String>,
    pub lease_until_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FeedbackClaim {
    pub feedback_id: String,
    pub claim_epoch: i64,
    pub owner: String,
    pub lease_until_ms: i64,
}

/// Insert once per `(operation_id, outcome_revision, category)`. Below schema 29
/// the caller must roll the rejection back; a committed run with no row cannot
/// be repaired by upgrade_v1.
pub(crate) fn insert_feedback(tx: &Connection, item: &LocalFeedback) -> Result<()> {
    let version: u32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchema(version));
    }
    identifier(&item.operation_id)?;
    identifier(&item.task_id)?;
    if item.outcome_revision <= 0 || !reason_ok(&item.reason) {
        return Err(invalid("feedback outcome is invalid"));
    }
    if !allowed_category(&item.category) {
        return Err(invalid("external poll is not factory evidence"));
    }
    let existing: Option<(String, String)> = tx
        .query_row(
            "SELECT task_id, reason FROM feedback_items WHERE operation_id=?1 AND outcome_revision=?2 AND category=?3",
            params![item.operation_id, item.outcome_revision, item.category],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((task_id, reason)) = existing {
        if task_id != item.task_id || reason != item.reason {
            return Err(StoreError::Conflict);
        }
        return Ok(());
    }
    let feedback_id = sha256_hex(
        format!(
            "{}\0{}\0{}",
            item.operation_id, item.outcome_revision, item.category
        )
        .as_bytes(),
    );
    let now = jiff::Timestamp::now().as_millisecond();
    tx.execute(
        "INSERT INTO feedback_items(feedback_id,operation_id,outcome_revision,category,task_id,reason,state,replan_proposal_id,created_unix_ms) VALUES(?1,?2,?3,?4,?5,?6,'open',NULL,?7)",
        params![
            feedback_id,
            item.operation_id,
            item.outcome_revision,
            item.category,
            item.task_id,
            item.reason,
            now
        ],
    )?;
    Ok(())
}

fn item_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<FeedbackItem> {
    Ok(FeedbackItem {
        feedback_id: row.get(0)?,
        operation_id: row.get(1)?,
        outcome_revision: row.get(2)?,
        category: row.get(3)?,
        task_id: row.get(4)?,
        reason: row.get(5)?,
        state: row.get(6)?,
        replan_proposal_id: row.get(7)?,
        created_unix_ms: row.get(8)?,
        claim_epoch: row.get(9)?,
        owner: row.get(10)?,
        lease_until_ms: row.get(11)?,
    })
}

const SHOW_SQL: &str = "SELECT i.feedback_id, i.operation_id, i.outcome_revision, i.category, i.task_id, i.reason, i.state, i.replan_proposal_id, i.created_unix_ms, c.claim_epoch, c.owner, c.lease_until_ms FROM feedback_items i LEFT JOIN feedback_claims c ON c.feedback_id = i.feedback_id AND c.claim_epoch = (SELECT max(claim_epoch) FROM feedback_claims WHERE feedback_id = i.feedback_id) WHERE (?1 IS NULL OR i.feedback_id = ?1) ORDER BY i.created_unix_ms, i.feedback_id";

impl SqliteStore {
    pub(crate) fn show_feedback_items(&mut self, id: Option<&str>) -> Result<Vec<FeedbackItem>> {
        let tx = self.connection.transaction()?;
        schema29(&tx)?;
        let mut stmt = tx.prepare(SHOW_SQL)?;
        let rows = stmt.query_map(params![id], item_from_row)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// Lease one open item. An unexpired lease stays with its owner. An expired
    /// lease can be taken at a new epoch; the old epoch cannot ack afterwards.
    pub(crate) fn claim_feedback_item(
        &mut self,
        feedback_id: &str,
        owner: &str,
        now: i64,
        lease_ms: i64,
    ) -> Result<FeedbackClaim> {
        identifier(feedback_id)?;
        identifier(owner)?;
        super::delivery::now_check(now)?;
        if !(1..=300_000).contains(&lease_ms) {
            return Err(invalid("lease must be 1..300000 ms"));
        }
        let until = now
            .checked_add(lease_ms)
            .ok_or_else(|| invalid("lease exceeds clock range"))?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema29(&tx)?;
        let state: Option<String> = tx
            .query_row(
                "SELECT state FROM feedback_items WHERE feedback_id=?1",
                [feedback_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(state) = state else {
            return Err(StoreError::Conflict);
        };
        if state == "acked" {
            return Err(StoreError::Conflict);
        }
        let active: Option<(i64, String, i64)> = tx
            .query_row(
                "SELECT claim_epoch, owner, lease_until_ms FROM feedback_claims WHERE feedback_id=?1 AND state='active'",
                [feedback_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((epoch, held_by, lease_until)) = active {
            if now < lease_until {
                if held_by != owner {
                    return Err(StoreError::Conflict);
                }
                tx.commit()?;
                return Ok(FeedbackClaim {
                    feedback_id: feedback_id.to_string(),
                    claim_epoch: epoch,
                    owner: owner.to_string(),
                    lease_until_ms: lease_until,
                });
            }
            let updated = tx.execute(
                "UPDATE feedback_claims SET state='expired' WHERE feedback_id=?1 AND claim_epoch=?2 AND state='active'",
                params![feedback_id, epoch],
            )?;
            if updated != 1 {
                return Err(StoreError::Conflict);
            }
        }
        let next: i64 = tx.query_row(
            "SELECT coalesce(max(claim_epoch), 0) + 1 FROM feedback_claims WHERE feedback_id=?1",
            [feedback_id],
            |row| row.get(0),
        )?;
        tx.execute(
            "INSERT INTO feedback_claims(feedback_id,claim_epoch,owner,lease_until_ms,state,claimed_unix_ms) VALUES(?1,?2,?3,?4,'active',?5)",
            params![feedback_id, next, owner, until, now],
        )?;
        let updated = tx.execute(
            "UPDATE feedback_items SET state='claimed' WHERE feedback_id=?1 AND state IN ('open','claimed')",
            [feedback_id],
        )?;
        if updated != 1 {
            return Err(StoreError::Conflict);
        }
        tx.commit()?;
        Ok(FeedbackClaim {
            feedback_id: feedback_id.to_string(),
            claim_epoch: next,
            owner: owner.to_string(),
            lease_until_ms: until,
        })
    }

    /// Record the replan proposal id for the current unexpired lease. Does not
    /// insert an attempt and does not write a dependency satisfaction.
    /// No production caller until schema 36 replans; the lease fence is tested.
    #[allow(dead_code)]
    pub(crate) fn ack_feedback_item(
        &mut self,
        feedback_id: &str,
        claim_epoch: i64,
        owner: &str,
        replan_proposal_id: &str,
        now: i64,
    ) -> Result<()> {
        identifier(feedback_id)?;
        identifier(owner)?;
        identifier(replan_proposal_id)?;
        if claim_epoch <= 0 {
            return Err(invalid("feedback claim is invalid"));
        }
        super::delivery::now_check(now)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema29(&tx)?;
        let state: Option<String> = tx
            .query_row(
                "SELECT state FROM feedback_items WHERE feedback_id=?1",
                [feedback_id],
                |row| row.get(0),
            )
            .optional()?;
        if state.as_deref() != Some("claimed") {
            return Err(StoreError::Conflict);
        }
        let claim: Option<(String, i64, String)> = tx
            .query_row(
                "SELECT owner, lease_until_ms, state FROM feedback_claims WHERE feedback_id=?1 AND claim_epoch=?2",
                params![feedback_id, claim_epoch],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((held_by, lease_until, claim_state)) = claim else {
            return Err(StoreError::Conflict);
        };
        let latest: i64 = tx.query_row(
            "SELECT coalesce(max(claim_epoch), 0) FROM feedback_claims WHERE feedback_id=?1",
            [feedback_id],
            |row| row.get(0),
        )?;
        if claim_epoch != latest
            || claim_state != "active"
            || held_by != owner
            || now >= lease_until
        {
            return Err(StoreError::Conflict);
        }
        let updated = tx.execute(
            "UPDATE feedback_claims SET state='acked' WHERE feedback_id=?1 AND claim_epoch=?2 AND state='active'",
            params![feedback_id, claim_epoch],
        )?;
        if updated != 1 {
            return Err(StoreError::Conflict);
        }
        let updated = tx.execute(
            "UPDATE feedback_items SET state='acked', replan_proposal_id=?2 WHERE feedback_id=?1 AND state='claimed' AND replan_proposal_id IS NULL",
            params![feedback_id, replan_proposal_id],
        )?;
        if updated != 1 {
            return Err(StoreError::Conflict);
        }
        tx.commit()?;
        Ok(())
    }
}

pub fn show_feedback(project: &Path, id: Option<&str>) -> Result<Vec<FeedbackItem>> {
    let mut db =
        crate::migration::open_active(project).map_err(|error| invalid(&error.to_string()))?;
    db.show_feedback_items(id)
}

pub fn claim_feedback(
    project: &Path,
    id: &str,
    owner: &str,
    lease_ms: i64,
) -> Result<FeedbackClaim> {
    let mut db =
        crate::migration::open_active(project).map_err(|error| invalid(&error.to_string()))?;
    let now = jiff::Timestamp::now().as_millisecond();
    db.claim_feedback_item(id, owner, now, lease_ms)
}

#[cfg(test)]
impl SqliteStore {
    pub(crate) fn testing_record_feedback(&mut self, item: LocalFeedback) -> Result<()> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema29(&tx)?;
        insert_feedback(&tx, &item)?;
        tx.commit()?;
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
    fn table_exists(db: &Connection, name: &str) -> bool {
        db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [name],
            |row| row.get(0),
        )
        .unwrap()
    }
    fn count(db: &Connection, sql: &str) -> i64 {
        db.query_row(sql, [], |row| row.get(0)).unwrap()
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
    fn seed_tasks(db: &mut SqliteStore) {
        db.commit(Commit {
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
                        title: "task".into(),
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
        })
        .unwrap();
        db.connection
            .execute(
                "INSERT INTO task_dependencies(task_id,predecessor_id,requirement) VALUES('consumer','task','landed_commit')",
                [],
            )
            .unwrap();
    }
    fn rejection(task: &str) -> LocalFeedback {
        LocalFeedback {
            operation_id: "a".repeat(64),
            outcome_revision: 1,
            category: "verifier_rejection".into(),
            task_id: task.into(),
            reason: "checks_failed".into(),
        }
    }

    #[test]
    fn upgrade_v1_from_28_to_29_and_create_end_at_user_version_29() {
        let fresh = tempfile::tempdir().unwrap();
        let created = SqliteStore::create(&fresh.path().join("state.db")).unwrap();
        assert_eq!(user_version(&created.connection), 35);
        assert_eq!(
            created
                .connection
                .query_row("SELECT schema_version FROM store_meta", [], |row| row.get::<_, u32>(0))
                .unwrap(),
            35
        );
        assert!(table_exists(&created.connection, "feedback_items"));
        assert!(table_exists(&created.connection, "feedback_claims"));
        drop(created);

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.db");
        let db = SqliteStore::create(&path).unwrap();
        drop(db);
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.execute_batch(
            "DROP TABLE IF EXISTS resource_claims; DROP TABLE IF EXISTS delegation_stop_obligations; DROP TABLE IF EXISTS delegation_revocations; DROP TABLE IF EXISTS delegation_grants; DROP TABLE IF EXISTS capability_evidence; DROP TABLE IF EXISTS contract_named_resources; DROP TABLE IF EXISTS contract_scope_paths; DROP TABLE IF EXISTS plan_revisions; DROP TABLE IF EXISTS plan_proposals; DROP TABLE IF EXISTS dependency_satisfactions; DROP TABLE IF EXISTS factory_admission_policies; ALTER TABLE project_control DROP COLUMN factory_admission; DROP TABLE IF EXISTS feedback_claims; DROP TABLE IF EXISTS feedback_items; UPDATE store_meta SET schema_version=28; PRAGMA user_version=28;",
        )
        .unwrap();
        drop(raw);
        let mut db = SqliteStore::open(&path).unwrap();
        assert_eq!(user_version(&db.connection), 28);
        assert!(!table_exists(&db.connection, "feedback_items"));
        assert!(matches!(
            db.import_legacy(&"cd".repeat(32), &[], &[]),
            Err(StoreError::UnsupportedSchema(28))
        ));
        db.upgrade_v1().unwrap();
        assert_eq!(user_version(&db.connection), 35);
        assert_eq!(
            db.connection
                .query_row("SELECT schema_version FROM store_meta", [], |row| row.get::<_, u32>(0))
                .unwrap(),
            35
        );
        assert!(table_exists(&db.connection, "feedback_items"));
        assert!(table_exists(&db.connection, "feedback_claims"));
        check_schema(&db.connection).unwrap();
        drop(db);
        let reopened = SqliteStore::open(&path).unwrap();
        assert_eq!(user_version(&reopened.connection), 35);
        assert!(table_exists(&reopened.connection, "feedback_claims"));
    }

    #[test]
    fn duplicate_verifier_rejection_feedback_polls_do_not_insert_duplicate_items() {
        let temp = tempfile::tempdir().unwrap();
        let mut db = SqliteStore::create(&temp.path().join("state.db")).unwrap();
        seed_tasks(&mut db);
        let attempts = count(&db.connection, "SELECT count(*) FROM attempts");
        let deps = dependencies(&db.connection);
        db.testing_record_feedback(rejection("task")).unwrap();
        db.testing_record_feedback(rejection("task")).unwrap();
        assert_eq!(count(&db.connection, "SELECT count(*) FROM feedback_items"), 1);
        assert_eq!(count(&db.connection, "SELECT count(*) FROM attempts"), attempts);
        assert_eq!(dependencies(&db.connection), deps);
    }

    #[test]
    fn expired_feedback_claim_can_be_retaken_and_ack_does_not_reserve_or_satisfy() {
        let temp = tempfile::tempdir().unwrap();
        let mut db = SqliteStore::create(&temp.path().join("state.db")).unwrap();
        seed_tasks(&mut db);
        db.testing_record_feedback(rejection("task")).unwrap();
        let id: String = db
            .connection
            .query_row("SELECT feedback_id FROM feedback_items", [], |row| row.get(0))
            .unwrap();
        let attempts = count(&db.connection, "SELECT count(*) FROM attempts");
        let operations = count(&db.connection, "SELECT count(*) FROM operations");
        let deps = dependencies(&db.connection);
        let first = db
            .claim_feedback_item(&id, "owner-a", 1_000_000, 1_000)
            .unwrap();
        assert_eq!(first.claim_epoch, 1);
        let second = db
            .claim_feedback_item(&id, "owner-b", 1_001_000, 1_000)
            .unwrap();
        assert_eq!(second.claim_epoch, 2);
        assert!(db
            .ack_feedback_item(&id, 1, "owner-a", "replan-1", 1_001_000)
            .is_err());
        db.ack_feedback_item(&id, second.claim_epoch, "owner-b", "replan-1", 1_001_000)
            .unwrap();
        let state: String = db
            .connection
            .query_row(
                "SELECT state FROM feedback_items WHERE feedback_id=?1",
                [&id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "acked");
        assert_eq!(count(&db.connection, "SELECT count(*) FROM attempts"), attempts);
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM operations"),
            operations
        );
        assert_eq!(dependencies(&db.connection), deps);
        let satisfactions: i64 = db
            .connection
            .query_row(
                "SELECT count(*) FROM dependency_satisfactions",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(satisfactions, 0);
    }

    #[test]
    fn stale_feedback_claim_cannot_ack() {
        let temp = tempfile::tempdir().unwrap();
        let mut db = SqliteStore::create(&temp.path().join("state.db")).unwrap();
        seed_tasks(&mut db);
        db.testing_record_feedback(rejection("task")).unwrap();
        let id: String = db
            .connection
            .query_row("SELECT feedback_id FROM feedback_items", [], |row| row.get(0))
            .unwrap();
        let claim = db
            .claim_feedback_item(&id, "owner-a", 1_000_000, 1_000)
            .unwrap();
        let attempts = count(&db.connection, "SELECT count(*) FROM attempts");
        assert!(matches!(
            db.ack_feedback_item(&id, claim.claim_epoch, "owner-a", "replan-1", 1_001_000),
            Err(StoreError::Conflict)
        ));
        let state: String = db
            .connection
            .query_row("SELECT state FROM feedback_items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(state, "claimed");
        assert_eq!(count(&db.connection, "SELECT count(*) FROM attempts"), attempts);
        assert_eq!(
            dependencies(&db.connection),
            vec![("consumer".into(), "task".into(), "landed_commit".into())]
        );
    }

    #[test]
    fn pr_poll_category_does_not_insert_feedback_or_change_satisfaction() {
        let temp = tempfile::tempdir().unwrap();
        let mut db = SqliteStore::create(&temp.path().join("state.db")).unwrap();
        seed_tasks(&mut db);
        let attempts = count(&db.connection, "SELECT count(*) FROM attempts");
        let deps = dependencies(&db.connection);
        let rejected = db.testing_record_feedback(LocalFeedback {
            operation_id: "pr-poll-1".into(),
            outcome_revision: 1,
            category: "pr_poll".into(),
            task_id: "task".into(),
            reason: "https://github.com/owner/repo/pull/7".into(),
        });
        assert!(matches!(rejected, Err(StoreError::Invalid(_))));
        assert_eq!(count(&db.connection, "SELECT count(*) FROM feedback_items"), 0);
        assert_eq!(count(&db.connection, "SELECT count(*) FROM attempts"), attempts);
        assert_eq!(dependencies(&db.connection), deps);
        let satisfactions: i64 = db
            .connection
            .query_row(
                "SELECT count(*) FROM dependency_satisfactions",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(satisfactions, 0);
    }

    #[test]
    fn pr_modules_do_not_write_feedback_or_satisfaction() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let markers = [
            "feedback_items",
            "feedback_claims",
            "insert_feedback",
            "task_dependencies",
            "dependency_satisfactions",
            "integrated_commits",
            "SqliteStore",
            "satisfaction",
        ];
        for name in ["src/pr.rs", "src/pr_polling.rs"] {
            let source = std::fs::read_to_string(root.join(name)).unwrap();
            for marker in markers {
                assert!(
                    !source.contains(marker),
                    "{name} must not write feedback or satisfaction via {marker}"
                );
            }
        }
    }
}

//! Schema 31 untrusted plan revisions. A proposal file is not a signature and
//! is not installed as a task contract. Accepting one does not reserve an attempt.
use super::*;
use crate::domain::{Dependency, QueueRecord, Task, TaskId, TaskState};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const SCHEMA_VERSION: u32 = 31;
const PROPOSAL_LIMIT: usize = 256 * 1024;

fn invalid(message: &str) -> StoreError {
    StoreError::Invalid(message.into())
}
fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn schema31(db: &Connection) -> Result<()> {
    check_schema(db)?;
    let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchema(version));
    }
    Ok(())
}
fn store_path(db: &Connection) -> Result<PathBuf> {
    let path = db.path().ok_or_else(|| invalid("store path missing"))?;
    std::fs::canonicalize(path).map_err(|error| StoreError::Io(error.to_string()))
}
fn identifier(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
}
fn plain(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanProposalReceipt {
    pub plan_revision: u64,
    pub proposal_id: String,
    pub digest: String,
    pub replayed: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProposalFile {
    version: u32,
    contracts: Vec<ProposedContract>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProposedContract {
    task_id: TaskId,
    text: String,
    dependencies: Vec<Dependency>,
}

struct ParsedProposal {
    contracts: Vec<ProposedContract>,
    contract_texts: String,
}

fn parse_proposal(raw: &[u8]) -> Result<ParsedProposal> {
    if raw.is_empty() || raw.len() > PROPOSAL_LIMIT {
        return Err(invalid("plan proposal exceeds 256 KiB"));
    }
    if std::str::from_utf8(raw).is_err() {
        return Err(invalid("invalid plan proposal"));
    }
    let document: ProposalFile =
        serde_json::from_slice(raw).map_err(|_| invalid("invalid plan proposal"))?;
    if document.version != 1 || document.contracts.is_empty() || document.contracts.len() > 1024 {
        return Err(invalid("invalid plan proposal"));
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut listed = Vec::with_capacity(document.contracts.len());
    for contract in &document.contracts {
        if !seen.insert(contract.task_id.as_str().to_string())
            || !plain(&contract.text, PROPOSAL_LIMIT)
        {
            return Err(invalid("invalid plan proposal"));
        }
        if contract.dependencies.len() > 256 {
            return Err(invalid("dependency inventory exceeds bounds"));
        }
        listed.push(serde_json::json!({
            "task_id": contract.task_id.as_str(),
            "text": contract.text,
        }));
    }
    let contract_texts =
        serde_json::to_string(&listed).map_err(|error| invalid(&error.to_string()))?;
    if contract_texts.len() > PROPOSAL_LIMIT {
        return Err(invalid("plan proposal exceeds 256 KiB"));
    }
    Ok(ParsedProposal {
        contracts: document.contracts,
        contract_texts,
    })
}

/// Cycle check uses the live queue plus this proposal. Mentioned tasks replace
/// their edges; unmentioned queue edges stay. Nothing is written.
fn reject_cycles(db: &Connection, proposal: &ParsedProposal) -> Result<()> {
    let mut tasks = read_tasks(db)?;
    let mut queue = scheduler::read(db)?.queue;
    for contract in &proposal.contracts {
        if !tasks.iter().any(|task| task.id == contract.task_id) {
            tasks.push(Task {
                id: contract.task_id.clone(),
                revision: 1,
                state: TaskState::Draft,
                title: contract.task_id.as_str().to_string(),
                active_attempt: None,
            });
        }
        let dependencies = contract.dependencies.clone();
        if let Some(existing) = queue.iter_mut().find(|row| row.task == contract.task_id) {
            existing.dependencies = dependencies;
        } else {
            queue.push(QueueRecord {
                task: contract.task_id.clone(),
                priority: 0,
                enqueued_unix_ms: 0,
                enqueue_sequence: 0,
                dependencies,
            });
        }
    }
    scheduler::graph(&tasks, &queue)
}

struct ExistingProposal {
    proposal_id: String,
    digest: String,
    payload: Vec<u8>,
    plan_revision: u64,
}

fn lookup_proposal(
    db: &Connection,
    project_store: &str,
    key: &str,
) -> Result<Option<ExistingProposal>> {
    db.query_row(
        "SELECT proposal_id, payload_digest, payload, plan_revision FROM plan_proposals WHERE project_store=?1 AND idempotency_key=?2",
        params![project_store, key],
        |row| {
            Ok(ExistingProposal {
                proposal_id: row.get(0)?,
                digest: row.get(1)?,
                payload: row.get(2)?,
                plan_revision: u64::try_from(row.get::<_, i64>(3)?).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        3,
                        rusqlite::types::Type::Integer,
                        Box::new(error),
                    )
                })?,
            })
        },
    )
    .optional()
    .map_err(StoreError::from)
}

fn current_revision(db: &Connection) -> Result<u64> {
    let current: i64 = db.query_row(
        "SELECT coalesce(max(plan_revision), 0) FROM plan_revisions",
        [],
        |row| row.get(0),
    )?;
    u64::try_from(current).map_err(|_| StoreError::Corrupt("plan revision is invalid".into()))
}

impl SqliteStore {
    /// Store one unsigned proposal. The same key and digest replay the existing
    /// revision. A different digest for that key conflicts. A stale parent
    /// returns the current revision and does not rebase.
    pub fn apply_plan_proposal(
        &mut self,
        raw: &[u8],
        expected_parent: u64,
        idempotency_key: &str,
    ) -> Result<PlanProposalReceipt> {
        if raw.is_empty() || raw.len() > PROPOSAL_LIMIT {
            return Err(invalid("plan proposal exceeds 256 KiB"));
        }
        if !identifier(idempotency_key) {
            return Err(invalid("invalid plan idempotency key"));
        }
        let digest = sha256_hex(raw);
        let path = store_path(&self.connection)?;
        let project_store = path.to_string_lossy().into_owned();
        if project_store.is_empty() || project_store.len() > 4096 {
            return Err(invalid("plan project store path is invalid"));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema31(&tx)?;
        if let Some(existing) = lookup_proposal(&tx, &project_store, idempotency_key)? {
            if sha256_hex(&existing.payload) != existing.digest {
                return Err(StoreError::Corrupt("plan proposal digest mismatch".into()));
            }
            if existing.digest != digest {
                return Err(StoreError::Conflict);
            }
            let receipt = PlanProposalReceipt {
                plan_revision: existing.plan_revision,
                proposal_id: existing.proposal_id,
                digest: existing.digest,
                replayed: true,
            };
            tx.commit()?;
            return Ok(receipt);
        }
        let proposal = parse_proposal(raw)?;
        reject_cycles(&tx, &proposal)?;
        let current = current_revision(&tx)?;
        if current != expected_parent {
            return Err(StoreError::StalePlanParent(current));
        }
        let next = current
            .checked_add(1)
            .ok_or_else(|| invalid("plan revision exhausted"))?;
        let proposal_id =
            sha256_hex(format!("{project_store}\0{idempotency_key}\0{digest}").as_bytes());
        let created_unix_ms = jiff::Timestamp::now().as_millisecond();
        tx.execute(
            "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('plan.proposed',?1,?2,1,?3)",
            params![
                proposal_id,
                integer(next)?,
                serde_json::json!({
                    "proposal_id": proposal_id,
                    "digest": digest,
                    "parent_revision": current,
                    "contract_count": proposal.contracts.len(),
                })
                .to_string()
            ],
        )?;
        tx.execute(
            "INSERT INTO plan_proposals(proposal_id,project_store,idempotency_key,payload_digest,payload,parent_revision,plan_revision,created_unix_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                proposal_id,
                project_store,
                idempotency_key,
                digest,
                raw,
                integer(current)?,
                integer(next)?,
                created_unix_ms
            ],
        )?;
        tx.execute(
            "INSERT INTO plan_revisions(plan_revision,parent_revision,proposal_id,payload_digest,contract_texts,created_unix_ms) VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                integer(next)?,
                integer(current)?,
                proposal_id,
                digest,
                proposal.contract_texts,
                created_unix_ms
            ],
        )?;
        tx.commit()?;
        Ok(PlanProposalReceipt {
            plan_revision: next,
            proposal_id,
            digest,
            replayed: false,
        })
    }
}

pub fn propose_plan(
    project: &Path,
    document: &Path,
    expected_parent: u64,
    idempotency_key: &str,
) -> Result<PlanProposalReceipt> {
    let _guard =
        crate::migration::runtime_mutation(project).map_err(|error| invalid(&error.to_string()))?;
    let bytes =
        crate::migration::read_plan_file(document).map_err(|error| invalid(&error.to_string()))?;
    let mut db =
        crate::migration::open_active(project).map_err(|error| invalid(&error.to_string()))?;
    db.apply_plan_proposal(&bytes, expected_parent, idempotency_key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{DependencyRequirement, Mutation, QueueRequest, TaskId, TaskState};

    fn fixture() -> (tempfile::TempDir, SqliteStore) {
        let temp = tempfile::tempdir().unwrap();
        let db = SqliteStore::create(&temp.path().join("state.db")).unwrap();
        (temp, db)
    }
    fn user_version(db: &Connection) -> u32 {
        db.query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }
    fn count(db: &Connection, sql: &str) -> i64 {
        db.query_row(sql, [], |row| row.get(0)).unwrap()
    }
    fn table_exists(db: &Connection, name: &str) -> bool {
        db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [name],
            |row| row.get(0),
        )
        .unwrap()
    }
    fn proposal(contracts: &str) -> Vec<u8> {
        format!(r#"{{"version":1,"contracts":[{contracts}]}}"#).into_bytes()
    }
    fn one(task: &str, text: &str, deps: &str) -> String {
        format!(r#"{{"task_id":"{task}","text":"{text}","dependencies":[{deps}]}}"#)
    }
    fn edge(predecessor: &str) -> String {
        format!(r#"{{"predecessor":"{predecessor}","requirement":"verified_result"}}"#)
    }

    #[test]
    fn replay_of_the_same_bytes_returns_one_plan_revision() {
        let (_temp, mut db) = fixture();
        let bytes = proposal(&one("alpha", "ship the widget", ""));
        let first = db.apply_plan_proposal(&bytes, 0, "replay-key").unwrap();
        assert!(!first.replayed);
        assert_eq!(first.plan_revision, 1);
        let second = db.apply_plan_proposal(&bytes, 0, "replay-key").unwrap();
        assert!(second.replayed);
        assert_eq!(
            second,
            PlanProposalReceipt {
                replayed: true,
                ..first
            }
        );
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM plan_revisions"),
            1
        );
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM plan_proposals"),
            1
        );
        assert_eq!(
            count(
                &db.connection,
                "SELECT count(*) FROM events WHERE kind='plan.proposed'"
            ),
            1
        );
        let stored: Vec<u8> = db
            .connection
            .query_row("SELECT payload FROM plan_proposals", [], |row| row.get(0))
            .unwrap();
        assert_eq!(stored, bytes);
        let texts: String = db
            .connection
            .query_row("SELECT contract_texts FROM plan_revisions", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(texts.contains("ship the widget"));
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM task_contracts"),
            0
        );
    }

    #[test]
    fn stale_parent_conflicts_and_returns_the_current_revision() {
        let (_temp, mut db) = fixture();
        let first = proposal(&one("alpha", "first text", ""));
        let stored = db.apply_plan_proposal(&first, 0, "key-1").unwrap();
        let before = count(&db.connection, "SELECT count(*) FROM plan_revisions");
        let error = db
            .apply_plan_proposal(&proposal(&one("beta", "other text", "")), 0, "key-2")
            .unwrap_err();
        assert!(
            matches!(error, StoreError::StalePlanParent(current) if current == stored.plan_revision)
        );
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM plan_revisions"),
            before
        );
        let parent: i64 = db
            .connection
            .query_row(
                "SELECT parent_revision FROM plan_revisions WHERE plan_revision=?1",
                [stored.plan_revision as i64],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(parent, 0);
        let digest: String = db
            .connection
            .query_row(
                "SELECT payload_digest FROM plan_revisions WHERE plan_revision=?1",
                [stored.plan_revision as i64],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(digest, stored.digest);
    }

    #[test]
    fn same_key_and_different_bytes_conflict() {
        let (_temp, mut db) = fixture();
        db.apply_plan_proposal(&proposal(&one("alpha", "one", "")), 0, "same-key")
            .unwrap();
        let error = db
            .apply_plan_proposal(&proposal(&one("alpha", "two", "")), 1, "same-key")
            .unwrap_err();
        assert!(matches!(error, StoreError::Conflict));
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM plan_revisions"),
            1
        );
    }

    #[test]
    fn dependency_cycle_is_rejected_without_a_revision_or_reservation() {
        let (_temp, mut db) = fixture();
        db.commit(Commit {
            expected_head: 0,
            mutations: ["a", "b"]
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
                .collect(),
        })
        .unwrap();
        let head = db.read_snapshot(None).unwrap().head;
        db.queue_task(
            &TaskId::new("a").unwrap(),
            1,
            head,
            &QueueRequest {
                priority: 0,
                dependencies: vec![Dependency {
                    predecessor: TaskId::new("b").unwrap(),
                    requirement: DependencyRequirement::VerifiedResult,
                }],
            },
            0,
        )
        .unwrap();
        let attempts = count(&db.connection, "SELECT count(*) FROM attempts");
        let dependencies = count(&db.connection, "SELECT count(*) FROM task_dependencies");
        let queued = count(&db.connection, "SELECT count(*) FROM task_queue");
        let cyclic = proposal(&format!(
            "{},{}",
            one("a", "loop", &edge("b")),
            one("b", "loop", &edge("a"))
        ));
        let error = db.apply_plan_proposal(&cyclic, 0, "cycle-key").unwrap_err();
        assert!(matches!(error, StoreError::Invalid(message) if message.contains("cycle")));
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM plan_revisions"),
            0
        );
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM attempts"),
            attempts
        );
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM task_dependencies"),
            dependencies
        );
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM task_queue"),
            queued
        );
        let with_live_edge = proposal(&one("b", "closes the live edge", &edge("a")));
        let error = db
            .apply_plan_proposal(&with_live_edge, 0, "live-cycle")
            .unwrap_err();
        assert!(matches!(error, StoreError::Invalid(message) if message.contains("cycle")));
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM plan_revisions"),
            0
        );
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM task_contracts"),
            0
        );
    }

    #[test]
    fn payload_above_256_kib_is_refused_and_exact_limit_is_an_unsigned_proposal() {
        let (_temp, mut db) = fixture();
        let contracts_before = count(&db.connection, "SELECT count(*) FROM task_contracts");
        let attempts_before = count(&db.connection, "SELECT count(*) FROM attempts");
        let limit = proposal_of_len(PROPOSAL_LIMIT);
        assert_eq!(limit.len(), PROPOSAL_LIMIT);
        let stored = db.apply_plan_proposal(&limit, 0, "wide-key").unwrap();
        assert!(!stored.replayed);
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM plan_revisions"),
            1
        );
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM task_contracts"),
            contracts_before
        );
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM attempts"),
            attempts_before
        );
        let over = proposal_of_len(PROPOSAL_LIMIT + 1);
        let error = db.apply_plan_proposal(&over, 1, "wide-key-2").unwrap_err();
        assert!(matches!(error, StoreError::Invalid(message) if message.contains("256 KiB")));
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM plan_revisions"),
            1
        );
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM task_contracts"),
            contracts_before
        );
    }

    fn proposal_of_len(len: usize) -> Vec<u8> {
        let prefix = br#"{"version":1,"contracts":[{"task_id":"alpha","text":""#;
        let suffix = br#"","dependencies":[]}]}"#;
        assert!(len > prefix.len() + suffix.len());
        let mut out = Vec::with_capacity(len);
        out.extend_from_slice(prefix);
        out.resize(len - suffix.len(), b'a');
        out.extend_from_slice(suffix);
        assert_eq!(out.len(), len);
        out
    }

    #[test]
    fn proposal_path_does_not_call_verify_signature_or_write_contracts() {
        let source = include_str!("plans.rs");
        let production = source.split("mod tests").next().unwrap();
        assert!(!production.contains("verify_signature"));
        assert!(!production.contains("task_contracts"));
        assert!(!production.contains("contract_scope_paths"));
        assert!(!production.contains("contract_named_resources"));
        assert!(!production.contains("install_contract"));
        assert!(!production.contains("reserve_"));
        let sql = include_str!("../../migrations/0031_plan_revisions.sql");
        assert!(!sql.contains("task_contracts"));
        assert!(!sql.contains("contract_scope_paths"));
        assert!(!sql.contains("contract_named_resources"));
        assert!(!sql.contains("verify_signature"));
        let cli = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cli.rs"),
        )
        .unwrap();
        let start = cli.find("Command::Plan{command}").unwrap();
        let rest = &cli[start..];
        let end = rest[1..]
            .find("\n        Command::")
            .map(|offset| offset + 1)
            .unwrap();
        let arm = &rest[..end];
        assert!(arm.contains("propose_plan"));
        assert!(!arm.contains("verify_signature"));
        assert!(!arm.contains("import_contract"));
        assert!(!arm.contains("reserve"));
        assert!(!arm.contains("task_contracts"));
        assert!(!arm.contains("contract_scope_paths"));
        assert!(!arm.contains("contract_named_resources"));
    }

    #[test]
    fn plan_propose_leaves_scope_tables_empty() {
        let (_temp, mut db) = fixture();
        assert!(table_exists(&db.connection, "contract_scope_paths"));
        assert!(table_exists(&db.connection, "contract_named_resources"));
        let paths = count(&db.connection, "SELECT count(*) FROM contract_scope_paths");
        let named = count(
            &db.connection,
            "SELECT count(*) FROM contract_named_resources",
        );
        let contracts = count(&db.connection, "SELECT count(*) FROM task_contracts");
        let attempts = count(&db.connection, "SELECT count(*) FROM attempts");
        let stored = db
            .apply_plan_proposal(
                &proposal(&one("alpha", "scope src/lib.rs schema lockfile", "")),
                0,
                "scope-key",
            )
            .unwrap();
        assert!(!stored.replayed);
        assert_eq!(stored.plan_revision, 1);
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM contract_scope_paths"),
            paths
        );
        assert_eq!(
            count(
                &db.connection,
                "SELECT count(*) FROM contract_named_resources"
            ),
            named
        );
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM task_contracts"),
            contracts
        );
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM attempts"),
            attempts
        );
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM plan_revisions"),
            1
        );
    }

    #[test]
    fn upgrade_v1_from_30_to_31_and_import_requires_current_schema() {
        let fresh = tempfile::tempdir().unwrap();
        let mut created = SqliteStore::create(&fresh.path().join("state.db")).unwrap();
        assert_eq!(user_version(&created.connection), 34);
        assert_eq!(
            created
                .connection
                .query_row("SELECT schema_version FROM store_meta", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            34
        );
        assert!(table_exists(&created.connection, "plan_proposals"));
        assert!(table_exists(&created.connection, "plan_revisions"));
        created.import_legacy(&"ab".repeat(32), &[], &[]).unwrap();
        drop(created);

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.db");
        let db = SqliteStore::create(&path).unwrap();
        drop(db);
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.execute_batch(
            "DROP TABLE IF EXISTS delegation_stop_obligations; DROP TABLE IF EXISTS delegation_revocations; DROP TABLE IF EXISTS delegation_grants; DROP TABLE IF EXISTS capability_evidence; DROP TABLE IF EXISTS contract_named_resources; DROP TABLE IF EXISTS contract_scope_paths; DROP TABLE IF EXISTS plan_revisions; DROP TABLE IF EXISTS plan_proposals; UPDATE store_meta SET schema_version=30; PRAGMA user_version=30;",
        )
        .unwrap();
        drop(raw);
        let mut db = SqliteStore::open(&path).unwrap();
        assert_eq!(user_version(&db.connection), 30);
        assert!(!table_exists(&db.connection, "plan_revisions"));
        assert!(matches!(
            db.import_legacy(&"cd".repeat(32), &[], &[]),
            Err(StoreError::UnsupportedSchema(30))
        ));
        assert_eq!(user_version(&db.connection), 30);
        db.upgrade_v1().unwrap();
        assert_eq!(user_version(&db.connection), 34);
        assert_eq!(
            db.connection
                .query_row("SELECT schema_version FROM store_meta", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            34
        );
        assert!(table_exists(&db.connection, "plan_proposals"));
        assert!(table_exists(&db.connection, "plan_revisions"));
        check_schema(&db.connection).unwrap();
        drop(db);
        let reopened = SqliteStore::open(&path).unwrap();
        assert_eq!(user_version(&reopened.connection), 34);
        assert!(table_exists(&reopened.connection, "plan_revisions"));
    }
}

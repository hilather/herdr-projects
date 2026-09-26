//! Schema 33 capability evidence. Reads retained native profiles and does not mint them.
//! A launchable fixture is not workflow-certified, and a fixture row is not live evidence.
use super::*;
use rusqlite::OptionalExtension;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

const SCHEMA_VERSION: u32 = 33;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CapabilityLevel {
    Discovered,
    Launchable,
    RepositoryCapable,
    MemoryProtocolCapable,
    WorkflowCertified,
}

impl CapabilityLevel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Discovered => "discovered",
            Self::Launchable => "launchable",
            Self::RepositoryCapable => "repository-capable",
            Self::MemoryProtocolCapable => "memory-protocol-capable",
            Self::WorkflowCertified => "workflow-certified",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "discovered" => Self::Discovered,
            "launchable" => Self::Launchable,
            "repository-capable" => Self::RepositoryCapable,
            "memory-protocol-capable" => Self::MemoryProtocolCapable,
            "workflow-certified" => Self::WorkflowCertified,
            _ => return None,
        })
    }
}

struct EvidenceDraft {
    adapter_kind: &'static str,
    binary_digest: String,
    os_name: &'static str,
    profile_digest: String,
    profile_kind: String,
    level: CapabilityLevel,
    test_id: &'static str,
    observed_unix_ms: i64,
    expires_unix_ms: i64,
    live: i64,
}

fn invalid(message: &str) -> StoreError {
    StoreError::Invalid(message.into())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn hex64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn schema33(db: &Connection) -> Result<()> {
    check_schema(db)?;
    let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchema(version));
    }
    Ok(())
}

/// Levels already shown by the frozen profile. Certification is not inferred.
fn levels_from_profile(profile: &FrozenProfile) -> Result<Vec<CapabilityLevel>> {
    profile
        .validate()
        .map_err(|_| invalid("invalid capability profile"))?;
    if !hex64(&profile.agent.digest) {
        return Err(invalid("invalid capability profile"));
    }
    let mut levels = vec![CapabilityLevel::Discovered];
    if profile.validate_for_launch().is_ok() {
        levels.push(CapabilityLevel::Launchable);
    }
    Ok(levels)
}

fn evidence_id(draft: &EvidenceDraft) -> String {
    sha256_hex(
        format!(
            "{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}",
            draft.adapter_kind,
            draft.profile_kind,
            draft.profile_digest,
            draft.level.as_str(),
            draft.binary_digest,
            draft.os_name,
            draft.test_id,
            draft.observed_unix_ms,
            draft.expires_unix_ms,
            draft.live
        )
        .as_bytes(),
    )
}

fn write_level(db: &Connection, draft: &EvidenceDraft) -> Result<()> {
    if draft.live != 0 || draft.level == CapabilityLevel::WorkflowCertified {
        // This schema records shown levels only. It does not certify a profile.
        return Err(invalid("capability evidence cannot certify a profile"));
    }
    let id = evidence_id(draft);
    // The same window is a replay. A later window is a new row and must not abort the batch.
    let existing: Option<i64> = db
        .query_row(
            "SELECT 1 FROM capability_evidence WHERE evidence_id=?1",
            [&id],
            |_| Ok(1),
        )
        .optional()?;
    if existing.is_some() {
        return Ok(());
    }
    db.execute(
        "INSERT INTO capability_evidence(evidence_id,adapter_kind,binary_digest,os_name,profile_digest,profile_kind,level,test_id,observed_unix_ms,expires_unix_ms,live) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
        params![
            id,
            draft.adapter_kind,
            draft.binary_digest,
            draft.os_name,
            draft.profile_digest,
            draft.profile_kind,
            draft.level.as_str(),
            draft.test_id,
            draft.observed_unix_ms,
            draft.expires_unix_ms,
            draft.live
        ],
    )?;
    Ok(())
}

fn drafts_for_profile(
    profile: &FrozenProfile,
    adapter_kind: &'static str,
    os_name: &'static str,
    test_id: &'static str,
    observed_unix_ms: i64,
    expires_unix_ms: i64,
) -> Result<Vec<EvidenceDraft>> {
    let reference = profile
        .reference()
        .map_err(|_| invalid("invalid capability profile"))?;
    if !hex64(&reference.digest) {
        return Err(invalid("invalid capability profile"));
    }
    Ok(levels_from_profile(profile)?
        .into_iter()
        .map(|level| EvidenceDraft {
            adapter_kind,
            binary_digest: profile.agent.digest.clone(),
            os_name,
            profile_digest: reference.digest.clone(),
            profile_kind: profile.kind.clone(),
            level,
            test_id,
            observed_unix_ms,
            expires_unix_ms,
            live: 0,
        })
        .collect())
}

fn store_binding(db: &Connection) -> Result<(PathBuf, u64, u64)> {
    let raw = db.path().ok_or_else(|| invalid("store path missing"))?;
    let path = Path::new(raw)
        .canonicalize()
        .map_err(|error| StoreError::Io(error.to_string()))?;
    let metadata = std::fs::metadata(&path).map_err(|error| StoreError::Io(error.to_string()))?;
    Ok((path, metadata.dev(), metadata.ino()))
}

/// Same path, device, and inode check as `native_profile_report`. A copied file must not count.
fn report_matches_store(report: &serde_json::Value, path: &Path, dev: u64, ino: u64) -> bool {
    report.get("source_store") == Some(&serde_json::json!([path, dev, ino]))
}

fn load_native_report(report: &str, report_digest: &str) -> Result<serde_json::Value> {
    if sha256_hex(report.as_bytes()) != report_digest {
        return Err(StoreError::Corrupt(
            "native profile report digest mismatch".into(),
        ));
    }
    serde_json::from_str(report)
        .map_err(|_| StoreError::Corrupt("invalid native profile report".into()))
}

fn profile_from_report(value: &serde_json::Value, digest: &str) -> Result<FrozenProfile> {
    let profile: FrozenProfile = serde_json::from_value(value["preparation"]["profile"].clone())
        .map_err(|_| StoreError::Corrupt("invalid retained profile".into()))?;
    let reference = profile
        .reference()
        .map_err(|_| StoreError::Corrupt("invalid retained profile".into()))?;
    if reference.digest != digest {
        return Err(StoreError::Corrupt(
            "retained profile reference mismatch".into(),
        ));
    }
    Ok(profile)
}

fn check_window(now: i64, expires_unix_ms: i64) -> Result<()> {
    super::delivery::now_check(now)?;
    super::delivery::now_check(expires_unix_ms)?;
    if expires_unix_ms <= now {
        return Err(invalid(
            "capability evidence expiry is not after observation",
        ));
    }
    Ok(())
}

impl SqliteStore {
    /// Record levels for native profile digests that already exist. Does not insert profiles.
    pub fn record_native_capability_evidence(
        &mut self,
        now: i64,
        expires_unix_ms: i64,
    ) -> Result<()> {
        check_window(now, expires_unix_ms)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema33(&tx)?;
        let (path, dev, ino) = store_binding(&tx)?;
        let mut stmt = tx.prepare(
            "SELECT profile_digest, report, report_digest FROM native_profiles ORDER BY sequence",
        )?;
        let mut rows = stmt.query([])?;
        let mut drafts = Vec::new();
        while let Some(row) = rows.next()? {
            let digest: String = row.get(0)?;
            let report: String = row.get(1)?;
            let report_digest: String = row.get(2)?;
            let value = load_native_report(&report, &report_digest)?;
            if !report_matches_store(&value, &path, dev, ino) {
                continue;
            }
            let profile = profile_from_report(&value, &digest)?;
            drafts.extend(drafts_for_profile(
                &profile,
                "native",
                std::env::consts::OS,
                "retained-native-profile",
                now,
                expires_unix_ms,
            )?);
        }
        drop(rows);
        drop(stmt);
        for draft in &drafts {
            write_level(&tx, draft)?;
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
impl SqliteStore {
    /// The fake adapter's shown levels. A fixture receipt stays non-live and uncertified.
    fn record_fake_adapter_evidence(
        &mut self,
        profile: &FrozenProfile,
        now: i64,
        expires_unix_ms: i64,
    ) -> Result<()> {
        check_window(now, expires_unix_ms)?;
        let drafts = drafts_for_profile(
            profile,
            "fake",
            "fixture",
            "fake-adapter",
            now,
            expires_unix_ms,
        )?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema33(&tx)?;
        for draft in &drafts {
            write_level(&tx, draft)?;
        }
        tx.commit()?;
        Ok(())
    }
}

fn contract_request(db: &Connection, task_id: &str) -> Result<Option<(String, Vec<String>)>> {
    let raw: Option<Vec<u8>> = db
        .query_row(
            "SELECT raw_bytes FROM task_contracts WHERE task_id=?1 ORDER BY contract_revision DESC LIMIT 1",
            [task_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(raw) = raw else {
        return Ok(None);
    };
    let value: serde_json::Value = serde_json::from_slice(&raw)
        .map_err(|_| StoreError::Corrupt("invalid task contract".into()))?;
    let kind = value["profile_kind"]
        .as_str()
        .ok_or_else(|| StoreError::Corrupt("invalid task contract".into()))?
        .to_string();
    let flags = value["capability_flags"]
        .as_array()
        .ok_or_else(|| StoreError::Corrupt("invalid task contract".into()))?;
    let mut requested = Vec::new();
    for flag in flags {
        let flag = flag
            .as_str()
            .ok_or_else(|| StoreError::Corrupt("invalid task contract".into()))?;
        requested.push(flag.to_string());
    }
    Ok(Some((kind, requested)))
}

fn levels_for(
    db: &Connection,
    adapter: &str,
    digest: &str,
    now: i64,
) -> Result<Vec<CapabilityLevel>> {
    // Latest in-window row per level. A future observation is not shown yet.
    let mut stmt = db.prepare(
        "SELECT level FROM capability_evidence WHERE adapter_kind=?1 AND profile_digest=?2 AND observed_unix_ms<=?3 AND expires_unix_ms>?3 ORDER BY observed_unix_ms DESC, evidence_id DESC",
    )?;
    let mut rows = stmt.query(params![adapter, digest, now])?;
    let mut levels = Vec::new();
    while let Some(row) = rows.next()? {
        let level: String = row.get(0)?;
        let level = CapabilityLevel::parse(&level)
            .ok_or_else(|| StoreError::Corrupt("unknown capability level".into()))?;
        if !levels.contains(&level) {
            levels.push(level);
        }
    }
    Ok(levels)
}

fn selected_native_digest(db: &Connection, kind: &str) -> Result<Option<String>> {
    let (path, dev, ino) = store_binding(db)?;
    let mut stmt = tx_profiles(db)?;
    let mut rows = stmt.query([])?;
    let mut selected: Option<(i64, String)> = None;
    while let Some(row) = rows.next()? {
        let digest: String = row.get(0)?;
        let report: String = row.get(1)?;
        let report_digest: String = row.get(2)?;
        let sequence: i64 = row.get(3)?;
        let value = load_native_report(&report, &report_digest)?;
        if !report_matches_store(&value, &path, dev, ino) {
            continue;
        }
        let profile = profile_from_report(&value, &digest)?;
        if profile.kind != kind {
            continue;
        }
        if selected
            .as_ref()
            .is_none_or(|(prior, _)| sequence >= *prior)
        {
            selected = Some((sequence, digest));
        }
    }
    Ok(selected.map(|(_, digest)| digest))
}

fn tx_profiles(db: &Connection) -> Result<rusqlite::Statement<'_>> {
    Ok(db.prepare(
        "SELECT profile_digest, report, report_digest, sequence FROM native_profiles ORDER BY sequence",
    )?)
}

fn selected_levels(db: &Connection, kind: &str, now: i64) -> Result<Vec<CapabilityLevel>> {
    if let Some(digest) = selected_native_digest(db, kind)? {
        return levels_for(db, "native", &digest, now);
    }
    let fake: Option<String> = db
        .query_row(
            "SELECT profile_digest FROM capability_evidence WHERE adapter_kind='fake' AND profile_kind=?1 AND observed_unix_ms<=?2 AND expires_unix_ms>?2 ORDER BY observed_unix_ms DESC, profile_digest DESC LIMIT 1",
            params![kind, now],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(digest) = fake {
        return levels_for(db, "fake", &digest, now);
    }
    Ok(Vec::new())
}

/// Queue blocker when the contract asks for a level the selected profile has not shown.
/// Absence is `capability_unsupported`. A shown launchable level is not certification.
pub(super) fn queue_capability_blocker(
    db: &Connection,
    task_id: &str,
    now: i64,
) -> Result<Option<String>> {
    let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < SCHEMA_VERSION {
        return Ok(None);
    }
    let Some((kind, flags)) = contract_request(db, task_id)? else {
        return Ok(None);
    };
    if flags.is_empty() {
        return Ok(None);
    }
    let shown = selected_levels(db, &kind, now)?;
    let lacks = flags
        .iter()
        .any(|flag| CapabilityLevel::parse(flag).is_none_or(|level| !shown.contains(&level)));
    if lacks {
        Ok(Some("capability_unsupported".into()))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::ConfigReference;
    use std::os::unix::fs::MetadataExt;

    fn user_version(db: &Connection) -> u32 {
        db.query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }

    fn table_exists(db: &Connection, name: &str) -> bool {
        db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?1)",
            [name],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn codex_fixture() -> FrozenProfile {
        let mut profile = crate::domain::profile::fixture(ConfigReference {
            path: "/fixture/codex.toml".into(),
            digest: None,
        });
        profile.kind = "codex".into();
        profile.name = "codex".into();
        profile.agent.path = "/fixture/codex".into();
        profile.agent.version = "0.154.0".into();
        profile.workflow_certificate = None;
        assert!(profile.validate_for_launch().is_ok());
        profile
    }

    fn stored_levels(db: &Connection, digest: &str) -> Vec<String> {
        let mut stmt = db
            .prepare("SELECT level FROM capability_evidence WHERE profile_digest=?1 ORDER BY level")
            .unwrap();
        stmt.query_map([digest], |row| row.get(0))
            .unwrap()
            .map(|row| row.unwrap())
            .collect()
    }

    #[test]
    fn upgrade_v1_from_32_to_34_and_create_end_at_user_version_34() {
        let fresh = tempfile::tempdir().unwrap();
        let created = SqliteStore::create(&fresh.path().join("state.db")).unwrap();
        assert_eq!(user_version(&created.connection), 34);
        assert_eq!(
            created
                .connection
                .query_row("SELECT schema_version FROM store_meta", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            34
        );
        assert!(table_exists(&created.connection, "capability_evidence"));
        let open_source = include_str!("mod.rs");
        let open = open_source
            .split("pub fn open")
            .nth(1)
            .unwrap()
            .split("pub fn integrity_check")
            .next()
            .unwrap();
        assert!(!open.contains("upgrade_v1"));
        assert!(!open.contains("0033_capability_evidence"));
        drop(created);

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.db");
        let mut db = SqliteStore::create(&path).unwrap();
        db.commit(Commit {
            expected_head: 0,
            mutations: vec![Mutation::Task {
                expected: None,
                next: Task {
                    id: TaskId::new("kept").unwrap(),
                    revision: 1,
                    state: TaskState::Draft,
                    title: "kept".into(),
                    active_attempt: None,
                },
            }],
        })
        .unwrap();
        drop(db);
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.execute_batch(
            "DROP TABLE IF EXISTS delegation_stop_obligations; DROP TABLE IF EXISTS delegation_revocations; DROP TABLE IF EXISTS delegation_grants; DROP TABLE IF EXISTS capability_evidence; UPDATE store_meta SET schema_version=32; PRAGMA user_version=32;",
        )
        .unwrap();
        drop(raw);
        let mut db = SqliteStore::open(&path).unwrap();
        assert_eq!(user_version(&db.connection), 32);
        assert!(!table_exists(&db.connection, "capability_evidence"));
        assert_eq!(db.read_snapshot(None).unwrap().tasks[0].title, "kept");
        db.upgrade_v1().unwrap();
        assert_eq!(user_version(&db.connection), 34);
        assert_eq!(
            db.connection
                .query_row("SELECT schema_version FROM store_meta", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            34
        );
        assert!(table_exists(&db.connection, "capability_evidence"));
        assert_eq!(
            db.connection
                .query_row("SELECT count(*) FROM capability_evidence", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        check_schema(&db.connection).unwrap();
        drop(db);
        let mut reopened = SqliteStore::open(&path).unwrap();
        assert_eq!(user_version(&reopened.connection), 34);
        assert_eq!(reopened.read_snapshot(None).unwrap().schema_version, 34);
    }

    #[test]
    fn codex_shaped_fixture_is_launchable_without_workflow_certification() {
        let temp = tempfile::tempdir().unwrap();
        let mut db = SqliteStore::create(&temp.path().join("state.db")).unwrap();
        let profile = codex_fixture();
        let digest = profile.reference().unwrap().digest;
        db.record_fake_adapter_evidence(&profile, 1_000, 10_000)
            .unwrap();
        db.record_fake_adapter_evidence(&profile, 1_000, 10_000)
            .unwrap();
        let levels = stored_levels(&db.connection, &digest);
        assert_eq!(
            levels,
            vec!["discovered".to_string(), "launchable".to_string()]
        );
        assert!(!levels.iter().any(|level| level == "workflow-certified"));
        let live: i64 = db
            .connection
            .query_row(
                "SELECT max(live) FROM capability_evidence WHERE profile_digest=?1",
                [&digest],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(live, 0);
        let os_name: String = db
            .connection
            .query_row(
                "SELECT os_name FROM capability_evidence WHERE profile_digest=?1 AND level='launchable'",
                [&digest],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(os_name, "fixture");
        assert!(
            db.connection
                .execute("UPDATE capability_evidence SET live=1", [])
                .is_err()
        );
        assert!(
            db.connection
                .execute("DELETE FROM capability_evidence", [])
                .is_err()
        );
        let sql = include_str!("../../migrations/0033_capability_evidence.sql");
        assert!(!sql.contains("model"));
        assert!(!sql.contains("reasoning_effort"));
        assert!(!sql.contains("PREPARED_LAUNCH_DISPATCH_ENABLED"));
    }

    #[test]
    fn changed_profile_digest_does_not_keep_the_old_level() {
        let temp = tempfile::tempdir().unwrap();
        let mut db = SqliteStore::create(&temp.path().join("state.db")).unwrap();
        let original = codex_fixture();
        let original_digest = original.reference().unwrap().digest;
        db.record_fake_adapter_evidence(&original, 1_000, 10_000)
            .unwrap();
        let mut changed = original;
        // Model and extra args are inside these digests, so the old row no longer applies.
        changed.definition_digest = "f".repeat(64);
        changed.arguments_digest = "9".repeat(64);
        changed.capabilities.stop = crate::domain::CapabilityEvidence::Unknown;
        let changed_digest = changed.reference().unwrap().digest;
        assert_ne!(original_digest, changed_digest);
        db.record_fake_adapter_evidence(&changed, 2_000, 10_000)
            .unwrap();
        assert!(
            stored_levels(&db.connection, &original_digest).contains(&"launchable".to_string())
        );
        let changed_levels = stored_levels(&db.connection, &changed_digest);
        assert!(!changed_levels.iter().any(|level| level == "launchable"));
        assert!(
            !changed_levels
                .iter()
                .any(|level| level == "workflow-certified")
        );
        let shown = selected_levels(&db.connection, "codex", 3_000).unwrap();
        assert!(!shown.contains(&CapabilityLevel::Launchable));
        assert!(!shown.contains(&CapabilityLevel::WorkflowCertified));
    }

    fn queue_task(db: &mut SqliteStore, id: &str) {
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
                dependencies: vec![],
            },
            1_000,
        )
        .unwrap();
    }

    fn install_contract(db: &mut SqliteStore, repo: &str, task: &str, flags: &[&str]) {
        let head = db.read_snapshot(None).unwrap().head;
        let store = std::fs::canonicalize(db.connection.path().unwrap()).unwrap();
        let bytes = serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "project_store": store.display().to_string(),
            "expected_head": head,
            "task_id": task,
            "contract_revision": 1,
            "deliverable": "show capability",
            "non_goals": "no certification",
            "acceptance_policies": [{"id": "shown", "text": "capability evidence"}],
            "repository": repo,
            "base_oid": "ab".repeat(32),
            "object_format": "sha256",
            "dependencies": [],
            "capability_flags": flags,
            "profile_kind": "codex",
            "retry_class": "none",
            "result_schema_id": "result-v1",
            "route": "verify_only",
            "authority": {"id": "owner-approval-policy", "revision": 1, "digest": "cd".repeat(32)}
        }))
        .unwrap();
        let prepared = PreparedContract::parse_verified(&bytes).unwrap();
        db.install_contract(&prepared).unwrap();
    }

    #[test]
    fn contract_level_the_profile_lacks_is_capability_unsupported_and_not_certified() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let repo = std::fs::canonicalize(&repo).unwrap();
        let mut db = SqliteStore::create(&temp.path().join("state.db")).unwrap();
        db.commit(Commit {
            expected_head: 0,
            mutations: ["ask-certified", "ask-launch", "ask-model"]
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
        for id in ["ask-certified", "ask-launch", "ask-model"] {
            queue_task(&mut db, id);
        }
        let repo = repo.display().to_string();
        install_contract(&mut db, &repo, "ask-certified", &["workflow-certified"]);
        install_contract(&mut db, &repo, "ask-launch", &["launchable"]);
        install_contract(&mut db, &repo, "ask-model", &["model"]);
        let profile = codex_fixture();
        db.record_fake_adapter_evidence(&profile, 1_000, 10_000)
            .unwrap();
        let report = db.queue_report(2_000).unwrap();
        let blockers = |id: &str| {
            report
                .entries
                .iter()
                .find(|entry| entry.task.as_str() == id)
                .unwrap()
                .blockers
                .clone()
        };
        assert!(
            blockers("ask-certified")
                .iter()
                .any(|blocker| blocker == "capability_unsupported")
        );
        assert!(
            blockers("ask-model")
                .iter()
                .any(|blocker| blocker == "capability_unsupported")
        );
        assert!(
            blockers("ask-launch")
                .iter()
                .all(|blocker| blocker != "capability_unsupported")
        );
        assert!(
            report
                .entries
                .iter()
                .flat_map(|entry| &entry.blockers)
                .chain(report.capability.blockers.iter())
                .all(|blocker| !blocker.contains("certified"))
        );
        assert!(!report.launch_enabled);
        let certified: i64 = db
            .connection
            .query_row(
                "SELECT count(*) FROM capability_evidence WHERE level='workflow-certified'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(certified, 0);
    }

    #[test]
    fn native_profile_evidence_ignores_certified_flag_and_does_not_mint_profiles() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let repo = std::fs::canonicalize(repo).unwrap().display().to_string();
        let mut db = SqliteStore::create(&temp.path().join("state.db")).unwrap();
        db.commit(Commit {
            expected_head: 0,
            mutations: vec![Mutation::Task {
                expected: None,
                next: Task {
                    id: TaskId::new("ask-launch").unwrap(),
                    revision: 1,
                    state: TaskState::Draft,
                    title: "ask-launch".into(),
                    active_attempt: None,
                },
            }],
        })
        .unwrap();
        queue_task(&mut db, "ask-launch");
        install_contract(&mut db, &repo, "ask-launch", &["launchable"]);
        let mut native = codex_fixture();
        native.capabilities.stop = crate::domain::CapabilityEvidence::Unknown;
        native.workflow_certificate = Some(crate::domain::VersionedReference {
            id: "not-a-certificate".into(),
            revision: 1,
            digest: "e".repeat(64),
        });
        assert!(native.validate_for_launch().is_err());
        let reference = native.reference().unwrap();
        let path = std::fs::canonicalize(db.connection.path().unwrap()).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let report = serde_json::json!({
            "preparation": {
                "profile": native,
                "reference": reference,
                "launchable": true,
                "protocol_capable": true,
                "certified": true
            },
            "source_store": [path, meta.dev(), meta.ino()]
        });
        let report = serde_json::to_string(&report).unwrap();
        let report_digest = sha256_hex(report.as_bytes());
        db.connection
            .execute(
                "INSERT INTO native_profiles(profile_digest,report,report_digest,sequence) VALUES(?1,?2,?3,(SELECT max(sequence) FROM events))",
                params![reference.digest, report, report_digest],
            )
            .unwrap();
        let before: i64 = db
            .connection
            .query_row("SELECT count(*) FROM native_profiles", [], |row| row.get(0))
            .unwrap();
        db.record_native_capability_evidence(1_000, 10_000).unwrap();
        db.record_native_capability_evidence(1_000, 10_000).unwrap();
        let after: i64 = db
            .connection
            .query_row("SELECT count(*) FROM native_profiles", [], |row| row.get(0))
            .unwrap();
        assert_eq!(before, after);
        let levels = stored_levels(&db.connection, &reference.digest);
        assert_eq!(levels, vec!["discovered".to_string()]);
        assert!(
            !levels
                .iter()
                .any(|level| level == "launchable" || level == "workflow-certified")
        );
        let fake = codex_fixture();
        db.record_fake_adapter_evidence(&fake, 1_500, 10_000)
            .unwrap();
        let report = db.queue_report(2_000).unwrap();
        let blockers = &report.entries[0].blockers;
        assert!(
            blockers
                .iter()
                .any(|blocker| blocker == "capability_unsupported")
        );
        assert!(
            blockers
                .iter()
                .all(|blocker| !blocker.contains("certified"))
        );
    }

    fn bind_native(db: &Connection, profile: &FrozenProfile) {
        let reference = profile.reference().unwrap();
        let path = std::fs::canonicalize(db.path().unwrap()).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let report = serde_json::json!({
            "preparation": {
                "profile": profile,
                "reference": reference,
                "launchable": profile.validate_for_launch().is_ok(),
                "protocol_capable": false,
                "certified": true
            },
            "source_store": [path, meta.dev(), meta.ino()]
        });
        let report = serde_json::to_string(&report).unwrap();
        let report_digest = sha256_hex(report.as_bytes());
        db.execute(
            "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('profile.native_retained',?1,1,1,'{}')",
            [reference.digest.as_str()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO native_profiles(profile_digest,report,report_digest,sequence) VALUES(?1,?2,?3,(SELECT max(sequence) FROM events))",
            params![reference.digest, report, report_digest],
        )
        .unwrap();
    }

    #[test]
    fn later_observation_window_does_not_abort_another_digest() {
        let temp = tempfile::tempdir().unwrap();
        let mut db = SqliteStore::create(&temp.path().join("state.db")).unwrap();
        let first = codex_fixture();
        let mut second = codex_fixture();
        second.name = "other".into();
        second.arguments_digest = "9".repeat(64);
        let first_digest = first.reference().unwrap().digest;
        let second_digest = second.reference().unwrap().digest;
        bind_native(&db.connection, &first);
        bind_native(&db.connection, &second);
        db.record_native_capability_evidence(1_000, 2_000).unwrap();
        db.record_native_capability_evidence(3_000, 8_000).unwrap();
        let windows = |digest: &str, observed: i64| -> i64 {
            db.connection
                .query_row(
                    "SELECT count(*) FROM capability_evidence WHERE profile_digest=?1 AND observed_unix_ms=?2 AND level='launchable'",
                    params![digest, observed],
                    |row| row.get(0),
                )
                .unwrap()
        };
        assert_eq!(windows(&first_digest, 1_000), 1);
        assert_eq!(windows(&first_digest, 3_000), 1);
        assert_eq!(windows(&second_digest, 3_000), 1);
        assert!(
            !selected_levels(&db.connection, "codex", 2_500)
                .unwrap()
                .contains(&CapabilityLevel::Launchable)
        );
        let shown = selected_levels(&db.connection, "codex", 4_000).unwrap();
        assert!(shown.contains(&CapabilityLevel::Launchable));
        assert!(!shown.contains(&CapabilityLevel::WorkflowCertified));
    }

    #[test]
    fn queue_report_before_observation_is_capability_unsupported() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let repo = std::fs::canonicalize(repo).unwrap().display().to_string();
        let mut db = SqliteStore::create(&temp.path().join("state.db")).unwrap();
        db.commit(Commit {
            expected_head: 0,
            mutations: vec![Mutation::Task {
                expected: None,
                next: Task {
                    id: TaskId::new("ask-launch").unwrap(),
                    revision: 1,
                    state: TaskState::Draft,
                    title: "ask-launch".into(),
                    active_attempt: None,
                },
            }],
        })
        .unwrap();
        queue_task(&mut db, "ask-launch");
        install_contract(&mut db, &repo, "ask-launch", &["launchable"]);
        let current = codex_fixture();
        db.record_fake_adapter_evidence(&current, 5_000, 9_000)
            .unwrap();
        let early = db.queue_report(2_000).unwrap();
        assert!(
            early.entries[0]
                .blockers
                .iter()
                .any(|blocker| blocker == "capability_unsupported")
        );
        let during = db.queue_report(6_000).unwrap();
        assert!(
            during.entries[0]
                .blockers
                .iter()
                .all(|blocker| blocker != "capability_unsupported")
        );
        let mut future = current;
        future.arguments_digest = "9".repeat(64);
        future.capabilities.stop = crate::domain::CapabilityEvidence::Unknown;
        db.record_fake_adapter_evidence(&future, 8_000, 12_000)
            .unwrap();
        let still_current = db.queue_report(6_000).unwrap();
        assert!(
            still_current.entries[0]
                .blockers
                .iter()
                .all(|blocker| blocker != "capability_unsupported")
        );
    }

    #[test]
    fn copied_native_profile_is_not_launchable() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let repo = std::fs::canonicalize(repo).unwrap().display().to_string();
        let path = temp.path().join("state.db");
        let mut db = SqliteStore::create(&path).unwrap();
        db.commit(Commit {
            expected_head: 0,
            mutations: vec![Mutation::Task {
                expected: None,
                next: Task {
                    id: TaskId::new("ask-launch").unwrap(),
                    revision: 1,
                    state: TaskState::Draft,
                    title: "ask-launch".into(),
                    active_attempt: None,
                },
            }],
        })
        .unwrap();
        queue_task(&mut db, "ask-launch");
        install_contract(&mut db, &repo, "ask-launch", &["launchable"]);
        bind_native(&db.connection, &codex_fixture());
        db.record_native_capability_evidence(1_000, 10_000).unwrap();
        let original = db.queue_report(2_000).unwrap();
        assert!(
            original.entries[0]
                .blockers
                .iter()
                .all(|blocker| blocker != "capability_unsupported")
        );
        let certified: i64 = db
            .connection
            .query_row(
                "SELECT count(*) FROM capability_evidence WHERE level='workflow-certified'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(certified, 0);
        db.connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
        let copy_path = temp.path().join("copy.db");
        std::fs::copy(&path, &copy_path).unwrap();
        let mut copied = SqliteStore::open(&copy_path).unwrap();
        let before: i64 = copied
            .connection
            .query_row("SELECT count(*) FROM capability_evidence", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(before > 0);
        copied
            .record_native_capability_evidence(1_000, 10_000)
            .unwrap();
        let after: i64 = copied
            .connection
            .query_row("SELECT count(*) FROM capability_evidence", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(before, after);
        let report = copied.queue_report(2_000).unwrap();
        assert!(
            report.entries[0]
                .blockers
                .iter()
                .any(|blocker| blocker == "capability_unsupported")
        );
        let certified: i64 = copied
            .connection
            .query_row(
                "SELECT count(*) FROM capability_evidence WHERE level='workflow-certified'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(certified, 0);
    }
}

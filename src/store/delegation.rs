//! Schema 34 delegation grants. Verified bytes stay in their own namespace.
//! A grant is not a launch approval, so reserve_attempt does not insert an attempt.
use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

const SCHEMA_VERSION: u32 = 34;
const STOP_BOUNDARY: &str = "stops_new_admits_does_not_undo_started_provider_call";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegationReserve {
    pub grant_id: String,
    /// Always false. This schema consults the grant and does not launch.
    pub reserved: bool,
}

fn invalid(message: &str) -> StoreError {
    StoreError::Invalid(message.into())
}

fn schema34(db: &Connection) -> Result<()> {
    check_schema(db)?;
    let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchema(version));
    }
    Ok(())
}

fn project_path(db: &Connection) -> Result<String> {
    let path = db
        .path()
        .ok_or_else(|| invalid("delegation requires a file-backed store"))?;
    std::fs::canonicalize(path)
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(|_| invalid("delegation store path unavailable"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn denial_id(grant_id: &str, reason: &str, now: i64) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
    sha256_hex(format!("{grant_id}\0{reason}\0{now}\0{nonce}").as_bytes())
}

impl SqliteStore {
    pub fn install_delegation(
        &mut self,
        prepared: &PreparedDelegation,
        now: i64,
    ) -> Result<String> {
        super::delivery::now_check(now)?;
        let parsed =
            PreparedDelegation::parse_verified(&prepared.raw).map_err(|error| invalid(&error))?;
        if parsed.digest != prepared.digest
            || parsed.digest != sha256_hex(&prepared.raw)
            || parsed != *prepared
        {
            return Err(invalid("changed delegation bytes"));
        }
        if now >= parsed.expires_unix_ms {
            return Err(invalid("delegation grant is expired"));
        }
        let actual = project_path(&self.connection)?;
        if parsed.project_store != actual {
            return Err(invalid("delegation belongs to another project"));
        }
        let tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        schema34(&tx)?;
        let existing: Option<Vec<u8>> = tx
            .query_row(
                "SELECT raw_bytes FROM delegation_grants WHERE id=?1",
                [parsed.digest.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(raw) = existing {
            if raw != parsed.raw || sha256_hex(&raw) != parsed.digest {
                return Err(invalid("changed delegation bytes"));
            }
            tx.commit()?;
            return Ok(parsed.digest);
        }
        let actions = serde_json::to_string(
            &parsed
                .actions
                .iter()
                .map(|action| action.as_str())
                .collect::<Vec<_>>(),
        )
        .map_err(|_| invalid("delegation action encoding failed"))?;
        let repositories = parsed.repository_column().map_err(|error| invalid(&error))?;
        let kinds = parsed.profile_kind_column().map_err(|error| invalid(&error))?;
        tx.execute(
            "INSERT INTO delegation_grants(id,raw_bytes,raw_digest,project_store,issuer,subject,subject_public_key,action_classes,repositories,profile_kinds,max_concurrent_attempts,expires_unix_ms,revocation_epoch,child_delegation,policy_revision,authority_digest,installed_unix_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
            params![
                parsed.digest,
                parsed.raw,
                parsed.digest,
                parsed.project_store,
                parsed.issuer,
                parsed.subject,
                parsed.subject_public_key,
                actions,
                repositories,
                kinds,
                parsed.max_concurrent_attempts,
                parsed.expires_unix_ms,
                integer(parsed.revocation_epoch)?,
                parsed.child_delegation,
                integer(parsed.policy_revision)?,
                parsed.authority.digest,
                now
            ],
        )?;
        tx.execute(
            "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('delegation.installed',?1,1,1,?2)",
            params![
                parsed.digest,
                serde_json::json!({"digest": parsed.digest, "actions": parsed.actions.iter().map(|action| action.as_str()).collect::<Vec<_>>()}).to_string()
            ],
        )?;
        tx.commit()?;
        Ok(parsed.digest)
    }

    /// Revocation stops later reserve_attempt admits. It does not change an attempt
    /// that already exists, because a started provider call is not undone.
    pub fn revoke_delegation(
        &mut self,
        id: &str,
        expected_head: u64,
        now: i64,
        reason: &str,
    ) -> Result<u64> {
        super::delivery::now_check(now)?;
        if reason.trim().is_empty() || reason.len() > 4_000 || reason.chars().any(char::is_control)
        {
            return Err(invalid("invalid delegation revocation reason"));
        }
        let tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        schema34(&tx)?;
        if head(&tx)? != expected_head {
            return Err(StoreError::Conflict);
        }
        let epoch: i64 = tx
            .query_row(
                "SELECT revocation_epoch FROM delegation_grants WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| invalid("delegation grant is missing"))?;
        let obligation = sha256_hex(format!("{id}\0{now}\0{reason}").as_bytes());
        tx.execute(
            "INSERT INTO delegation_revocations(grant_id,revoked_unix_ms,reason,revocation_epoch) VALUES(?1,?2,?3,?4)",
            params![id, now, reason, epoch],
        )?;
        tx.execute(
            "INSERT INTO delegation_stop_obligations(obligation_id,grant_id,created_unix_ms,boundary) VALUES(?1,?2,?3,?4)",
            params![obligation, id, now, STOP_BOUNDARY],
        )?;
        tx.execute(
            "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('delegation.revoked',?1,1,1,?2)",
            params![
                id,
                serde_json::json!({
                    "reason": reason,
                    "stops_new_admits": true,
                    "undoes_started_provider_call": false
                })
                .to_string()
            ],
        )?;
        let next = head(&tx)?;
        tx.commit()?;
        Ok(next)
    }

    /// Consult `reserve_attempt` only when factory admission is on.
    /// Expired, wrong-repo, and self-signed grants do not reserve. A valid grant
    /// does not either: it is not `ApprovalScope::for_launch`.
    pub fn reserve_attempt(
        &mut self,
        grant_id: &str,
        repository: &str,
        git_ref: &str,
        profile_kind: &str,
        now: i64,
    ) -> Result<DelegationReserve> {
        super::delivery::now_check(now)?;
        schema34(&self.connection)?;
        if !super::satisfaction::admission_enabled(&self.connection)? {
            return Err(invalid("factory admission is off"));
        }
        let loaded: Option<(Vec<u8>, String)> = self
            .connection
            .query_row(
                "SELECT raw_bytes, raw_digest FROM delegation_grants WHERE id=?1",
                [grant_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((raw, digest)) = loaded else {
            return Err(invalid("delegation grant is missing"));
        };
        if digest != grant_id || sha256_hex(&raw) != digest {
            return Err(StoreError::Corrupt(
                "delegation grant digest mismatch".into(),
            ));
        }
        let parsed = match PreparedDelegation::parse_verified(&raw) {
            Ok(parsed) => parsed,
            Err(error) => {
                let reason = if error.contains("self-signature") {
                    "self_signature"
                } else if error.contains("child delegation") {
                    "child_delegation"
                } else if error.contains("not a launch approval") {
                    "not_launch_approval"
                } else {
                    "invalid_document"
                };
                self.record_delegation_denial(grant_id, reason, &"0".repeat(64), now)?;
                return Err(invalid(&error));
            }
        };
        if parsed.digest != digest {
            return Err(StoreError::Corrupt(
                "delegation grant digest mismatch".into(),
            ));
        }
        let actual = project_path(&self.connection)?;
        let path_matches = parsed
            .repositories
            .iter()
            .any(|repo| repo.repository == repository);
        if parsed.project_store != actual || !path_matches {
            self.record_delegation_denial(grant_id, "wrong_repo", &parsed.authority.digest, now)?;
            return Err(invalid("delegation grant is for the wrong repo"));
        }
        // The signed ref is part of the scope. A path match is not every ref on that path.
        if !parsed
            .repositories
            .iter()
            .any(|repo| repo.repository == repository && repo.git_ref == git_ref)
        {
            self.record_delegation_denial(grant_id, "wrong_ref", &parsed.authority.digest, now)?;
            return Err(invalid("delegation grant is for the wrong ref"));
        }
        if !parsed
            .profile_kinds
            .iter()
            .any(|kind| kind == profile_kind)
        {
            self.record_delegation_denial(
                grant_id,
                "profile_kind",
                &parsed.authority.digest,
                now,
            )?;
            return Err(invalid("delegation grant does not include the profile kind"));
        }
        if now >= parsed.expires_unix_ms {
            self.record_delegation_denial(grant_id, "expired", &parsed.authority.digest, now)?;
            return Err(invalid("delegation grant is expired"));
        }
        if parsed.issuer == parsed.subject {
            self.record_delegation_denial(
                grant_id,
                "self_signature",
                &parsed.authority.digest,
                now,
            )?;
            return Err(invalid("delegation self-signature is forbidden"));
        }
        let revoked: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM delegation_revocations WHERE grant_id=?1)",
            [grant_id],
            |row| row.get(0),
        )?;
        if revoked {
            self.record_delegation_denial(
                grant_id,
                "authority_revoked",
                &parsed.authority.digest,
                now,
            )?;
            return Err(invalid("delegation grant is revoked"));
        }
        if !parsed.actions.contains(&DelegationAction::ReserveAttempt) {
            self.record_delegation_denial(
                grant_id,
                "action_not_granted",
                &parsed.authority.digest,
                now,
            )?;
            return Err(invalid("delegation does not grant reserve_attempt"));
        }
        // max_concurrent_attempts stays on the row. This schema does not reserve, so the cap is not applied.
        // A matching reserve_attempt class is still not a launch approval.
        if parsed.matches_launch().is_ok() {
            return Err(invalid("delegation grant is not a launch approval"));
        }
        Ok(DelegationReserve {
            grant_id: parsed.digest,
            reserved: false,
        })
    }

    fn record_delegation_denial(
        &mut self,
        grant_id: &str,
        reason: &str,
        policy_digest: &str,
        now: i64,
    ) -> Result<()> {
        let actual = self.read_snapshot(None).ok().map(|snapshot| snapshot.head);
        self.insert_denial(&AuthorityDenial {
            id: denial_id(grant_id, reason, now),
            unix_ms: now,
            class: "delegation".into(),
            command: "reserve".into(),
            actor_channel: "unknown-rejected".into(),
            reason_code: reason.into(),
            policy_digest: policy_digest.to_string(),
            expected_head: None,
            actual_head: actual,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reconcile::RuntimeObservation;

    fn user_version(db: &Connection) -> u32 {
        db.query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }

    fn attempts(db: &mut SqliteStore) -> usize {
        db.read_snapshot(None).unwrap().attempts.len()
    }

    fn admission_on(db: &SqliteStore) {
        db.connection
            .execute(
                "UPDATE project_control SET factory_admission='on' WHERE singleton=1",
                [],
            )
            .unwrap();
    }

    fn document(
        store: &str,
        repository: &str,
        issuer: &str,
        subject: &str,
        expires: i64,
    ) -> Vec<u8> {
        let mut bytes = serde_json::to_vec_pretty(&serde_json::json!({
            "version": 1,
            "issuer": issuer,
            "subject": subject,
            "subject_public_key": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIDelegationSubjectKeyExampleValue1234567890",
            "action_classes": ["reserve_attempt"],
            "repositories": [{"repository": repository, "ref": "refs/heads/factory"}],
            "profile_kinds": ["codex"],
            "max_concurrent_attempts": 1,
            "expires_unix_ms": expires,
            "revocation_epoch": 1,
            "child_delegation": "forbidden",
            "policy_revision": 1,
            "project_store": store,
            "authority": {"id": "owner-approval-policy", "revision": 1, "digest": "ab".repeat(32)}
        }))
        .unwrap();
        bytes.push(b'\n');
        bytes
    }

    fn install(db: &mut SqliteStore, repository: &str, expires: i64, now: i64) -> String {
        let store = std::fs::canonicalize(db.connection.path().unwrap())
            .unwrap()
            .display()
            .to_string();
        let bytes = document(&store, repository, "owner", "delegate", expires);
        let prepared = PreparedDelegation::parse_verified(&bytes).unwrap();
        db.install_delegation(&prepared, now).unwrap()
    }

    #[test]
    fn upgrade_v1_from_33_to_34_preserves_denials_and_create_ends_at_34() {
        let fresh = tempfile::tempdir().unwrap();
        let created = SqliteStore::create(&fresh.path().join("state.db")).unwrap();
        assert_eq!(user_version(&created.connection), 36);
        assert_eq!(
            created
                .connection
                .query_row("SELECT schema_version FROM store_meta", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            36
        );
        assert!(table_exists(&created.connection, "delegation_grants"));
        assert!(table_exists(&created.connection, "delegation_revocations"));
        assert!(table_exists(
            &created.connection,
            "delegation_stop_obligations"
        ));
        let open_source = include_str!("mod.rs");
        let open = open_source
            .split("pub fn open")
            .nth(1)
            .unwrap()
            .split("pub fn integrity_check")
            .next()
            .unwrap();
        assert!(!open.contains("upgrade_v1"));
        assert!(!open.contains("0034_delegation_grants"));
        let controller = include_str!("../canonical_controller.rs");
        assert!(controller.contains("const PREPARED_LAUNCH_DISPATCH_ENABLED: bool = true"));
        drop(created);

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.db");
        let mut db = SqliteStore::create(&path).unwrap();
        db.insert_denial(&AuthorityDenial {
            id: "kept-denial".into(),
            unix_ms: 10,
            class: "contract".into(),
            command: "put".into(),
            actor_channel: "cli-owner".into(),
            reason_code: "invalid_document".into(),
            policy_digest: "cd".repeat(32),
            expected_head: Some(1),
            actual_head: Some(1),
        })
        .unwrap();
        drop(db);
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.execute_batch(
            "DROP TABLE IF EXISTS wait_replay_events; DROP TABLE IF EXISTS replan_requests; DROP TABLE IF EXISTS replan_budget_resets; DROP TABLE IF EXISTS attempt_infrastructure_retries; DROP TABLE IF EXISTS wait_conditions; DROP TABLE IF EXISTS resource_claims; DROP TABLE IF EXISTS delegation_stop_obligations; DROP TABLE IF EXISTS delegation_revocations; DROP TABLE IF EXISTS delegation_grants;
             DROP TRIGGER IF EXISTS authority_denials_no_update; DROP TRIGGER IF EXISTS authority_denials_no_delete;
             CREATE TABLE authority_denials_v26 (
                id TEXT PRIMARY KEY NOT NULL, unix_ms INTEGER NOT NULL,
                class TEXT NOT NULL CHECK (class IN ('approval','budget','routine-store','memory','contract')),
                command TEXT NOT NULL,
                actor_channel TEXT NOT NULL CHECK (actor_channel IN ('cli-owner','unknown-rejected')),
                reason_code TEXT NOT NULL,
                policy_digest TEXT NOT NULL CHECK (length(policy_digest) = 64),
                expected_head INTEGER, actual_head INTEGER
             ) STRICT;
             INSERT INTO authority_denials_v26 SELECT id, unix_ms, class, command, actor_channel, reason_code, policy_digest, expected_head, actual_head FROM authority_denials;
             DROP TABLE authority_denials;
             ALTER TABLE authority_denials_v26 RENAME TO authority_denials;
             CREATE TRIGGER authority_denials_no_update BEFORE UPDATE ON authority_denials BEGIN SELECT RAISE(ABORT,'authority denial is immutable'); END;
             CREATE TRIGGER authority_denials_no_delete BEFORE DELETE ON authority_denials BEGIN SELECT RAISE(ABORT,'authority denial is immutable'); END;
             UPDATE store_meta SET schema_version=33; PRAGMA user_version=33;",
        )
        .unwrap();
        assert!(raw
            .execute(
                "INSERT INTO authority_denials VALUES('also-kept',11,'delegation','import','cli-owner','refused','ab',NULL,NULL)",
                [],
            )
            .is_err());
        drop(raw);
        let mut db = SqliteStore::open(&path).unwrap();
        assert_eq!(user_version(&db.connection), 33);
        assert!(!table_exists(&db.connection, "delegation_grants"));
        db.upgrade_v1().unwrap();
        assert_eq!(user_version(&db.connection), 36);
        assert_eq!(
            db.connection
                .query_row("SELECT schema_version FROM store_meta", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            36
        );
        let kept: i64 = db
            .connection
            .query_row(
                "SELECT count(*) FROM authority_denials WHERE id='kept-denial' AND class='contract'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(kept, 1);
        db.insert_denial(&AuthorityDenial {
            id: "new-delegation".into(),
            unix_ms: 12,
            class: "delegation".into(),
            command: "import".into(),
            actor_channel: "cli-owner".into(),
            reason_code: "signature_failed".into(),
            policy_digest: "ef".repeat(32),
            expected_head: None,
            actual_head: None,
        })
        .unwrap();
        check_schema(&db.connection).unwrap();
        drop(db);
        let reopened = SqliteStore::open(&path).unwrap();
        assert_eq!(user_version(&reopened.connection), 36);
        assert_eq!(
            reopened
                .connection
                .query_row("SELECT count(*) FROM authority_denials", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    fn table_exists(db: &Connection, name: &str) -> bool {
        db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?1)",
            [name],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn expired_wrong_repo_and_self_signed_delegations_do_not_reserve() {
        let temp = tempfile::tempdir().unwrap();
        let repo_a = temp.path().join("repo-a");
        let repo_b = temp.path().join("repo-b");
        std::fs::create_dir(&repo_a).unwrap();
        std::fs::create_dir(&repo_b).unwrap();
        let repo_a = std::fs::canonicalize(repo_a).unwrap().display().to_string();
        let repo_b = std::fs::canonicalize(repo_b).unwrap().display().to_string();
        let mut db = SqliteStore::create(&temp.path().join("state.db")).unwrap();
        admission_on(&db);
        let expired = install(&mut db, &repo_a, 5_000, 1_000);
        let error = db
            .reserve_attempt(&expired, &repo_a, "refs/heads/factory", "codex", 5_000)
            .unwrap_err();
        assert!(
            matches!(error, StoreError::Invalid(ref message) if message.contains("expired")),
            "{error}"
        );
        assert_eq!(attempts(&mut db), 0);
        assert_eq!(denial_reasons(&db), vec!["expired".to_string()]);

        let wrong = install(&mut db, &repo_a, 9_000, 1_000);
        let error = db
            .reserve_attempt(&wrong, &repo_b, "refs/heads/factory", "codex", 1_500)
            .unwrap_err();
        assert!(
            matches!(error, StoreError::Invalid(ref message) if message.contains("wrong repo")),
            "{error}"
        );
        assert_eq!(attempts(&mut db), 0);

        let store = std::fs::canonicalize(db.connection.path().unwrap())
            .unwrap()
            .display()
            .to_string();
        assert!(PreparedDelegation::parse_verified(&document(
            &store, &repo_a, "owner", "owner", 9_000
        ))
        .is_err());
        // Column CHECK forbids issuer = subject, so the stored row keeps distinct
        // columns while the raw bytes name the issuer as the subject.
        let original = document(&store, &repo_a, "owner", "delegate", 9_000);
        let parsed = PreparedDelegation::parse_verified(&original).unwrap();
        let forged = String::from_utf8(original.clone())
            .unwrap()
            .replace("\"subject\": \"delegate\"", "\"subject\": \"owner\"   ");
        assert_ne!(forged.as_bytes(), original.as_slice());
        assert!(PreparedDelegation::parse_verified(forged.as_bytes())
            .unwrap_err()
            .contains("self-signature"));
        let forged_id = sha256_hex(forged.as_bytes());
        db.connection
            .execute(
                "INSERT INTO delegation_grants(id,raw_bytes,raw_digest,project_store,issuer,subject,subject_public_key,action_classes,repositories,profile_kinds,max_concurrent_attempts,expires_unix_ms,revocation_epoch,child_delegation,policy_revision,authority_digest,installed_unix_ms) VALUES(?1,?2,?1,?3,'owner','delegate',?4,'[\"reserve_attempt\"]',?5,'[\"codex\"]',1,9000,1,'forbidden',1,?6,1000)",
                params![
                    forged_id,
                    forged.as_bytes(),
                    store,
                    parsed.subject_public_key,
                    serde_json::to_string(&serde_json::json!([{"repository": repo_a, "ref": "refs/heads/factory"}])).unwrap(),
                    parsed.authority.digest
                ],
            )
            .unwrap();
        let error = db
            .reserve_attempt(&forged_id, &repo_a, "refs/heads/factory", "codex", 1_500)
            .unwrap_err();
        assert!(
            matches!(error, StoreError::Invalid(ref message) if message.contains("self-signature")),
            "{error}"
        );
        assert_eq!(attempts(&mut db), 0);
        let reasons = denial_reasons(&db);
        assert!(reasons.contains(&"expired".to_string()));
        assert!(reasons.contains(&"wrong_repo".to_string()));
        assert!(reasons.contains(&"self_signature".to_string()));
        assert!(
            db.connection
                .query_row(
                    "SELECT count(*) FROM operations WHERE kind='runtime.launch'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap()
                == 0
        );
    }

    fn denial_reasons(db: &SqliteStore) -> Vec<String> {
        let mut stmt = db
            .connection
            .prepare("SELECT reason_code FROM authority_denials WHERE class='delegation' ORDER BY unix_ms, id")
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .map(|row| row.unwrap())
            .collect()
    }

    #[test]
    fn delegation_document_does_not_satisfy_matches_launch() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let repo = std::fs::canonicalize(repo).unwrap().display().to_string();
        let db = SqliteStore::create(&temp.path().join("state.db")).unwrap();
        let store = std::fs::canonicalize(db.connection.path().unwrap())
            .unwrap()
            .display()
            .to_string();
        let bytes = document(&store, &repo, "owner", "delegate", 9_000);
        assert!(serde_json::from_slice::<ApprovalGrant>(&bytes).is_err());
        let prepared = PreparedDelegation::parse_verified(&bytes).unwrap();
        let mut inputs: LaunchInputs =
            serde_json::from_str(include_str!("../../tests/fixtures/launch-inputs-v1.json"))
                .unwrap();
        inputs.version = 2;
        let profile = crate::domain::profile::fixture(crate::migration::ConfigReference {
            path: temp.path().join("config.toml").display().to_string(),
            digest: None,
        });
        inputs.profile = profile.reference().unwrap();
        inputs.effective_profile = Some(profile);
        inputs.project_store = store.clone();
        let scope = ApprovalScope::for_launch(&inputs).unwrap();
        assert!(matches!(scope.class, ApprovalClass::RuntimeLaunch));
        let mut grant = ApprovalGrant {
            version: 1,
            scope: scope.clone(),
            policy: inputs
                .effective_profile
                .as_ref()
                .unwrap()
                .permission_policy
                .clone(),
            issued_unix_ms: 100,
            expires_unix_ms: 200,
        };
        inputs.approval = grant.reference().unwrap();
        grant.scope = ApprovalScope::for_launch(&inputs).unwrap();
        inputs.approval = grant.reference().unwrap();
        grant
            .matches_launch(&inputs, &inputs.project_store, 100)
            .unwrap();
        assert!(prepared.matches_launch().is_err());
        assert!(serde_json::from_slice::<ApprovalGrant>(&prepared.raw).is_err());
        assert_ne!(
            grant.scope,
            ApprovalScope {
                version: 1,
                class: ApprovalClass::RuntimeLaunch,
                project_store: prepared.project_store.clone(),
                task: inputs.task.clone(),
                task_revision: inputs.task_revision,
                target: prepared.subject.clone(),
                action_digest: prepared.digest.clone(),
            }
        );
    }

    #[test]
    fn empty_delegation_tables_do_not_change_the_exact_grant_path() {
        let (temp, mut db, prepared) = exact_grant_fixture();
        assert_eq!(
            db.connection
                .query_row("SELECT count(*) FROM delegation_grants", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            db.connection
                .query_row("SELECT count(*) FROM delegation_revocations", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        let head = db.read_snapshot(None).unwrap().head;
        let reserved = db.reserve_prepared(&prepared, head, 1_000).unwrap();
        let snapshot = db.read_snapshot(None).unwrap();
        assert_eq!(snapshot.attempts.len(), 1);
        assert!(snapshot
            .operations
            .iter()
            .any(|operation| operation.kind == "runtime.launch"));
        assert_eq!(snapshot.attempts[0].id, reserved.record.attempt);
        drop(db);
        drop(temp);
    }

    #[test]
    fn valid_delegation_with_admission_on_is_not_a_launch_and_flag_off_does_not_consult() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let repo = std::fs::canonicalize(repo).unwrap().display().to_string();
        let mut db = SqliteStore::create(&temp.path().join("state.db")).unwrap();
        let id = install(&mut db, &repo, 9_000, 1_000);
        let before = db.read_snapshot(None).unwrap();
        let error = db
            .reserve_attempt(&id, &repo, "refs/heads/factory", "codex", 1_500)
            .unwrap_err();
        assert!(
            matches!(error, StoreError::Invalid(ref message) if message.contains("admission is off")),
            "{error}"
        );
        assert_eq!(db.read_snapshot(None).unwrap().attempts, before.attempts);
        assert!(denial_reasons(&db).is_empty());
        admission_on(&db);
        let decision = db
            .reserve_attempt(&id, &repo, "refs/heads/factory", "codex", 1_500)
            .unwrap();
        assert_eq!(decision.grant_id, id);
        assert!(!decision.reserved);
        assert_eq!(attempts(&mut db), 0);
        assert_eq!(
            db.connection
                .query_row(
                    "SELECT count(*) FROM operations WHERE kind='runtime.launch'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        assert!(denial_reasons(&db).is_empty());
        let child = String::from_utf8(document(
            &std::fs::canonicalize(db.connection.path().unwrap())
                .unwrap()
                .display()
                .to_string(),
            &repo,
            "owner",
            "delegate",
            9_000,
        ))
        .unwrap()
        .replace(
            "\"child_delegation\": \"forbidden\"",
            "\"child_delegation\": \"allowed\"  ",
        );
        assert!(PreparedDelegation::parse_verified(child.as_bytes())
            .unwrap_err()
            .contains("child delegation"));
    }

    fn scope_document(store: &str, repositories: serde_json::Value, profile_kinds: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "issuer": "owner",
            "subject": "delegate",
            "subject_public_key": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIDelegationSubjectKeyExampleValue1234567890",
            "action_classes": ["reserve_attempt"],
            "repositories": repositories,
            "profile_kinds": profile_kinds,
            "max_concurrent_attempts": 1,
            "expires_unix_ms": 9_000,
            "revocation_epoch": 1,
            "child_delegation": "forbidden",
            "policy_revision": 1,
            "project_store": store,
            "authority": {"id": "owner-approval-policy", "revision": 1, "digest": "ab".repeat(32)}
        }))
        .unwrap()
    }

    #[test]
    fn stored_scope_text_over_the_check_is_invalid_not_conflict() {
        let temp = tempfile::tempdir().unwrap();
        let mut db = SqliteStore::create(&temp.path().join("state.db")).unwrap();
        let store = std::fs::canonicalize(db.connection.path().unwrap())
            .unwrap()
            .display()
            .to_string();
        let repositories = (0..4)
            .map(|index| {
                serde_json::json!({
                    "repository": format!("/{index}{}", "a".repeat(4094)),
                    "ref": "refs/heads/factory"
                })
            })
            .collect::<Vec<_>>();
        let raw = scope_document(&store, serde_json::Value::Array(repositories), serde_json::json!(["codex"]));
        let parse_error = PreparedDelegation::parse_verified(&raw).unwrap_err();
        assert!(parse_error.contains("stored text exceeds 16384"), "{parse_error}");
        let error = db
            .install_delegation(
                &PreparedDelegation {
                    digest: "ab".repeat(32),
                    raw,
                    issuer: "owner".into(),
                    subject: "delegate".into(),
                    subject_public_key: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIDelegationSubjectKeyExampleValue1234567890".into(),
                    actions: vec![DelegationAction::ReserveAttempt],
                    repositories: vec![],
                    profile_kinds: vec!["codex".into()],
                    max_concurrent_attempts: 1,
                    expires_unix_ms: 9_000,
                    revocation_epoch: 1,
                    child_delegation: "forbidden".into(),
                    policy_revision: 1,
                    project_store: store.clone(),
                    authority: VersionedReference {
                        id: "owner-approval-policy".into(),
                        revision: 1,
                        digest: "ab".repeat(32),
                    },
                },
                1_000,
            )
            .unwrap_err();
        assert!(
            matches!(error, StoreError::Invalid(ref message) if message.contains("stored text")),
            "{error:?}"
        );
        assert!(!matches!(error, StoreError::Conflict));
        assert!(denial_reasons(&db).iter().all(|reason| reason != "stale_head"));
        let kinds = (0..8)
            .map(|index| format!("{index}{}", "\\".repeat(63)))
            .collect::<Vec<_>>();
        let raw = scope_document(
            &store,
            serde_json::json!([{"repository": store, "ref": "refs/heads/factory"}]),
            serde_json::Value::Array(kinds.into_iter().map(serde_json::Value::String).collect()),
        );
        let parse_error = PreparedDelegation::parse_verified(&raw).unwrap_err();
        assert!(parse_error.contains("profile kinds"), "{parse_error}");
        assert_eq!(attempts(&mut db), 0);
    }

    #[test]
    fn consultation_denies_ref_or_profile_kind_mismatch_without_reserving() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let repo = std::fs::canonicalize(repo).unwrap().display().to_string();
        let mut db = SqliteStore::create(&temp.path().join("state.db")).unwrap();
        let id = install(&mut db, &repo, 9_000, 1_000);
        admission_on(&db);
        let wrong_ref = db
            .reserve_attempt(&id, &repo, "refs/heads/other", "codex", 1_500)
            .unwrap_err();
        assert!(
            matches!(wrong_ref, StoreError::Invalid(ref message) if message.contains("wrong ref")),
            "{wrong_ref}"
        );
        let wrong_kind = db
            .reserve_attempt(&id, &repo, "refs/heads/factory", "claude", 1_500)
            .unwrap_err();
        assert!(
            matches!(wrong_kind, StoreError::Invalid(ref message) if message.contains("profile kind")),
            "{wrong_kind}"
        );
        // The stored cap is 1. Consultation does not enforce it and does not launch.
        let first = db
            .reserve_attempt(&id, &repo, "refs/heads/factory", "codex", 1_500)
            .unwrap();
        let second = db
            .reserve_attempt(&id, &repo, "refs/heads/factory", "codex", 1_600)
            .unwrap();
        assert!(!first.reserved && !second.reserved);
        assert!(PreparedDelegation::parse_verified(&document(
            &std::fs::canonicalize(db.connection.path().unwrap()).unwrap().display().to_string(),
            &repo,
            "owner",
            "delegate",
            9_000,
        ))
        .unwrap()
        .matches_launch()
        .is_err());
        assert_eq!(attempts(&mut db), 0);
        assert_eq!(
            db.connection
                .query_row(
                    "SELECT count(*) FROM operations WHERE kind='runtime.launch'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        let reasons = denial_reasons(&db);
        assert!(reasons.contains(&"wrong_ref".to_string()));
        assert!(reasons.contains(&"profile_kind".to_string()));
    }

    #[test]
    fn revoked_delegation_stops_new_admits_and_does_not_undo_a_started_attempt() {
        let (_temp, mut db, prepared) = exact_grant_fixture();
        let head = db.read_snapshot(None).unwrap().head;
        let reserved = db.reserve_prepared(&prepared, head, 1_000).unwrap();
        let repo = std::fs::canonicalize(_temp.path())
            .unwrap()
            .display()
            .to_string();
        let id = install(&mut db, &repo, 9_000, 1_100);
        let head = db.read_snapshot(None).unwrap().head;
        db.revoke_delegation(&id, head, 1_200, "stop new admits")
            .unwrap();
        let boundary: String = db
            .connection
            .query_row(
                "SELECT boundary FROM delegation_stop_obligations WHERE grant_id=?1",
                [&id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(boundary, STOP_BOUNDARY);
        let snapshot = db.read_snapshot(None).unwrap();
        let attempt = snapshot
            .attempts
            .iter()
            .find(|attempt| attempt.id == reserved.record.attempt)
            .unwrap();
        assert!(attempt.retains_capacity());
        assert!(!attempt.termination_observed);
        admission_on(&db);
        let error = db
            .reserve_attempt(&id, &repo, "refs/heads/factory", "codex", 1_300)
            .unwrap_err();
        assert!(
            matches!(error, StoreError::Invalid(ref message) if message.contains("revoked")),
            "{error}"
        );
        let snapshot = db.read_snapshot(None).unwrap();
        assert_eq!(snapshot.attempts.len(), 1);
        assert!(snapshot.attempts[0].retains_capacity());
    }

    fn exact_grant_fixture() -> (tempfile::TempDir, SqliteStore, Vec<PreparedLaunch>) {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join(".state")).unwrap();
        let path = temp.path().join(".state/state.db");
        let mut db = SqliteStore::create(&path).unwrap();
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
        for id in ["a", "b"] {
            let id = TaskId::new(id).unwrap();
            let head = db.read_snapshot(None).unwrap().head;
            db.create_runtime(Some(&id), Some(1), head, &RuntimeRoute::default())
                .unwrap();
            let head = db.read_snapshot(None).unwrap().head;
            db.queue_task(
                &id,
                2,
                head,
                &QueueRequest {
                    priority: 0,
                    dependencies: vec![],
                },
                0,
            )
            .unwrap();
        }
        let snapshot = db.read_snapshot(None).unwrap();
        db.set_scheduler_policy(snapshot.head, 1, 1, 3).unwrap();
        let snapshot = db.read_snapshot(None).unwrap();
        let observations = snapshot
            .runtime_bindings
            .iter()
            .map(|binding| RuntimeObservation {
                binding: binding.id.clone(),
                binding_revision: binding.revision,
                task_revision: Some(3),
                observed_unix_ms: 1000,
                collector: "herdr-git-v1".into(),
                ..RuntimeObservation::default()
            })
            .collect::<Vec<_>>();
        db.record_observations(snapshot.head, &observations)
            .unwrap();
        let snapshot = db.read_snapshot(None).unwrap();
        db.set_project_state(
            snapshot.head,
            snapshot.control.unwrap().revision,
            ProjectState::Active,
            1000,
            None,
        )
        .unwrap();
        let snapshot = db.read_snapshot(None).unwrap();
        let config = crate::migration::ConfigReference {
            path: temp.path().join("config.toml").display().to_string(),
            digest: None,
        };
        let mut prepared = snapshot
            .runtime_bindings
            .iter()
            .map(|binding| PreparedLaunch {
                inputs: LaunchInputs {
                    version: 2,
                    project_store: std::fs::canonicalize(&path).unwrap().display().to_string(),
                    task: binding.task.clone().unwrap(),
                    task_revision: 3,
                    scheduler_revision: snapshot.scheduler.as_ref().unwrap().policy.revision,
                    control_epoch: snapshot.control.as_ref().unwrap().epoch,
                    binding: binding.id.clone(),
                    binding_revision: binding.revision,
                    binding_digest: super::super::ownership::identity_digest(binding).unwrap(),
                    profile: crate::domain::profile::fixture(config.clone())
                        .reference()
                        .unwrap(),
                    effective_profile: Some(crate::domain::profile::fixture(config.clone())),
                    approval: VersionedReference {
                        id: "fixture-approval".into(),
                        revision: 1,
                        digest: "b".repeat(64),
                    },
                    config: config.clone(),
                    repositories: vec![],
                    dependencies: vec![],
                    memory: None,
                    budget: None,
                },
            })
            .collect::<Vec<_>>();
        for launch in &mut prepared {
            let grant = ApprovalGrant {
                version: 1,
                scope: ApprovalScope::for_launch(&launch.inputs).unwrap(),
                policy: launch
                    .inputs
                    .effective_profile
                    .as_ref()
                    .unwrap()
                    .permission_policy
                    .clone(),
                issued_unix_ms: 0,
                expires_unix_ms: 100_000,
            };
            let head = db.read_snapshot(None).unwrap().head;
            launch.inputs.approval = db
                .install_approval(&PreparedApproval { grant }, head, 1000)
                .unwrap();
        }
        (temp, db, prepared)
    }
}

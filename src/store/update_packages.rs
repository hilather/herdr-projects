//! Immutable packages of obligations already recorded for one binding generation.
//! Rebuilding reads those rows. It does not invent obligations or attempts.
use super::*;
use rusqlite::OptionalExtension;
use serde::Serialize;

const SCHEMA_VERSION: u32 = 39;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UpdatePackage {
    pub package_id: String,
    pub binding_id: String,
    pub consumer_binding_generation: u64,
    pub manifest_hash: String,
    pub change_ids: Vec<String>,
    pub created_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct UpdatePackageAck {
    pub schema_version: u32,
    pub package_id: String,
    pub manifest_hash: String,
    pub change_ids: Vec<String>,
    pub disposition: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PackageAckReceipt {
    pub package_id: String,
    pub disposition: String,
    pub change_ids: Vec<String>,
    pub sequence: u64,
}

struct Member {
    change_id: String,
    record_id: String,
    revision: u64,
    body_hash: String,
    severity: String,
    triggering_seq: u64,
    snapshot_id: String,
}

struct BindingRow {
    generation: u64,
    attempt_id: Option<String>,
}

fn invalid(message: &str) -> StoreError {
    StoreError::Invalid(message.into())
}

fn require_schema(db: &Connection) -> Result<()> {
    check_schema(db)?;
    let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchema(version));
    }
    Ok(())
}

fn hex64(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn binding_id_ok(binding_id: &str) -> bool {
    hex64(binding_id)
}

fn package_identity(binding_id: &str, generation: u64, change_ids: &[String]) -> String {
    let mut bytes = b"update-package-v1\0".to_vec();
    bytes.extend(binding_id.as_bytes());
    bytes.push(0);
    bytes.extend(generation.to_string().as_bytes());
    for id in change_ids {
        bytes.push(0);
        bytes.extend(id.as_bytes());
    }
    format!("{:x}", Sha256::digest(&bytes))
}

fn manifest_hash(binding_id: &str, generation: u64, members: &[Member]) -> Result<String> {
    let listed: Vec<_> = members
        .iter()
        .map(|member| {
            serde_json::json!([
                member.change_id,
                member.record_id,
                member.revision,
                member.body_hash,
                member.severity,
                member.triggering_seq,
                member.snapshot_id
            ])
        })
        .collect();
    let body = serde_json::json!(["update-package-v1", binding_id, generation, listed]);
    let bytes = serde_json::to_vec(&body).map_err(|error| invalid(&error.to_string()))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn load_binding(db: &Connection, binding_id: &str) -> Result<BindingRow> {
    let row = db
        .query_row(
            "SELECT generation, attempt_id FROM consumer_bindings WHERE binding_id=?1",
            [binding_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .optional()?
        .ok_or_else(|| invalid("consumer binding is missing"))?;
    let generation = u64::try_from(row.0).map_err(|_| invalid("invalid consumer binding"))?;
    if generation == 0 {
        return Err(invalid("invalid consumer binding"));
    }
    Ok(BindingRow {
        generation,
        attempt_id: row.1,
    })
}

fn unresolved_members(db: &Connection, binding_id: &str, generation: u64) -> Result<Vec<Member>> {
    let mut stmt = db.prepare(
        "SELECT o.delivery_id, d.record_id, d.revision, v.body_hash, d.severity, d.triggering_seq, d.snapshot_id
         FROM consumer_binding_obligations o
         JOIN memory_delivery_intents d ON d.id=o.delivery_id
         JOIN memory_revisions v ON v.record_id=d.record_id AND v.revision=d.revision
         WHERE o.binding_id=?1
         AND NOT EXISTS (
             SELECT 1 FROM memory_change_receipts c
             WHERE c.consumer_binding_generation=?2 AND c.change_id=o.delivery_id AND c.disposition='applied'
         )
         ORDER BY o.delivery_id LIMIT 10001",
    )?;
    let rows = stmt
        .query_map(
            params![
                binding_id,
                i64::try_from(generation).map_err(|_| invalid("invalid consumer binding"))?
            ],
            |row| {
                Ok(Member {
                    change_id: row.get(0)?,
                    record_id: row.get(1)?,
                    revision: row.get(2)?,
                    body_hash: row.get(3)?,
                    severity: row.get(4)?,
                    triggering_seq: row.get(5)?,
                    snapshot_id: row.get(6)?,
                })
            },
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if rows.len() > 10_000 {
        return Err(StoreError::Limit(
            "unresolved obligations exceed 10000".into(),
        ));
    }
    let obligations: i64 = db.query_row(
        "SELECT count(*) FROM consumer_binding_obligations o
         WHERE o.binding_id=?1
         AND NOT EXISTS (
             SELECT 1 FROM memory_change_receipts c
             WHERE c.consumer_binding_generation=?2 AND c.change_id=o.delivery_id AND c.disposition='applied'
         )",
        params![binding_id, i64::try_from(generation).map_err(|_| invalid("invalid consumer binding"))?],
        |row| row.get(0),
    )?;
    if obligations != rows.len() as i64 {
        return Err(StoreError::Corrupt(
            "obligation is missing its delivery revision".into(),
        ));
    }
    Ok(rows)
}

fn member_ids(db: &Connection, package_id: &str) -> Result<Vec<String>> {
    let mut stmt = db.prepare(
        "SELECT change_id FROM update_package_members WHERE package_id=?1 ORDER BY position",
    )?;
    stmt.query_map([package_id], |row| row.get(0))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn load_package(db: &Connection, package_id: &str) -> Result<Option<UpdatePackage>> {
    let row = db
        .query_row(
            "SELECT package_id, binding_id, consumer_binding_generation, manifest_hash, created_seq
             FROM update_packages WHERE package_id=?1",
            [package_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((package_id, binding_id, generation, manifest_hash, created_seq)) = row else {
        return Ok(None);
    };
    let change_ids = member_ids(db, &package_id)?;
    Ok(Some(UpdatePackage {
        package_id,
        binding_id,
        consumer_binding_generation: u64::try_from(generation)
            .map_err(|_| StoreError::Corrupt("update package generation is invalid".into()))?,
        manifest_hash,
        change_ids,
        created_seq: u64::try_from(created_seq)
            .map_err(|_| StoreError::Corrupt("update package sequence is invalid".into()))?,
    }))
}

fn materialize(tx: &rusqlite::Transaction, binding_id: &str) -> Result<UpdatePackage> {
    require_schema(tx)?;
    let binding = load_binding(tx, binding_id)?;
    let members = unresolved_members(tx, binding_id, binding.generation)?;
    if members.is_empty() {
        return Err(invalid("no unresolved obligations"));
    }
    let change_ids: Vec<String> = members
        .iter()
        .map(|member| member.change_id.clone())
        .collect();
    let package_id = package_identity(binding_id, binding.generation, &change_ids);
    let manifest = manifest_hash(binding_id, binding.generation, &members)?;
    let created_seq = members
        .iter()
        .map(|member| member.triggering_seq)
        .max()
        .ok_or_else(|| invalid("no unresolved obligations"))?;
    if let Some(existing) = load_package(tx, &package_id)? {
        if existing.change_ids != change_ids
            || existing.manifest_hash != manifest
            || existing.binding_id != binding_id
            || existing.consumer_binding_generation != binding.generation
            || existing.created_seq != created_seq
        {
            return Err(StoreError::Corrupt(
                "stored update package does not match its obligations".into(),
            ));
        }
        return Ok(existing);
    }
    let generation =
        i64::try_from(binding.generation).map_err(|_| invalid("invalid consumer binding"))?;
    let created = i64::try_from(created_seq).map_err(|_| invalid("invalid change sequence"))?;
    tx.execute(
        "INSERT INTO update_packages(package_id,binding_id,consumer_binding_generation,manifest_hash,created_seq)
         VALUES(?1,?2,?3,?4,?5)",
        params![package_id, binding_id, generation, manifest, created],
    )?;
    for (position, change_id) in change_ids.iter().enumerate() {
        tx.execute(
            "INSERT INTO update_package_members(package_id,position,change_id) VALUES(?1,?2,?3)",
            params![
                package_id,
                i64::try_from(position).map_err(|_| invalid("invalid package position"))?,
                change_id
            ],
        )?;
    }
    Ok(UpdatePackage {
        package_id,
        binding_id: binding_id.into(),
        consumer_binding_generation: binding.generation,
        manifest_hash: manifest,
        change_ids,
        created_seq,
    })
}

fn normalize_change_ids(change_ids: &[String]) -> Result<Vec<String>> {
    if change_ids.is_empty() || change_ids.len() > 10_000 {
        return Err(invalid(
            "package acknowledgment must list that package's change ids",
        ));
    }
    let mut sorted = change_ids.to_vec();
    sorted.sort();
    if sorted.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(invalid("package acknowledgment repeats a change id"));
    }
    if sorted.iter().any(|id| id.is_empty() || id.len() > 128) {
        return Err(invalid("invalid change id"));
    }
    Ok(sorted)
}

fn receipt_sequences(
    db: &Connection,
    generation: u64,
    change_ids: &[String],
    disposition: &str,
    package_id: &str,
) -> Result<Option<u64>> {
    let generation = i64::try_from(generation).map_err(|_| invalid("invalid consumer binding"))?;
    let mut sequences = Vec::new();
    let mut missing = false;
    for change_id in change_ids {
        let row: Option<(String, i64)> = db
            .query_row(
                "SELECT package_id, sequence FROM memory_change_receipts
                 WHERE consumer_binding_generation=?1 AND change_id=?2 AND disposition=?3",
                params![generation, change_id, disposition],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((stored_package, sequence)) = row else {
            missing = true;
            continue;
        };
        if stored_package != package_id {
            return Err(StoreError::Conflict);
        }
        let sequence = u64::try_from(sequence)
            .map_err(|_| StoreError::Corrupt("change receipt sequence is invalid".into()))?;
        sequences.push(sequence);
    }
    if sequences.is_empty() {
        return Ok(None);
    }
    if missing {
        return Err(StoreError::Corrupt(
            "package acknowledgment receipts are partial".into(),
        ));
    }
    let first = sequences[0];
    if sequences.iter().any(|sequence| *sequence != first) {
        return Err(StoreError::Corrupt(
            "package acknowledgment receipts diverged".into(),
        ));
    }
    Ok(Some(first))
}

fn revision_current(db: &Connection, record: &str, revision: u64, now: i64) -> Result<bool> {
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM memory_heads h JOIN memory_validity v ON v.record_id=h.record_id AND v.revision=h.revision
         WHERE h.record_id=?1 AND h.revision=?2 AND h.status='active' AND v.state='valid'
         AND (v.expiry_unix_ms IS NULL OR v.expiry_unix_ms>?3))",
        params![record, i64::try_from(revision).map_err(|_| invalid("invalid change revision"))?, now],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

fn acknowledge(
    tx: &rusqlite::Transaction,
    ack: &UpdatePackageAck,
    now: i64,
) -> Result<PackageAckReceipt> {
    if ack.schema_version != 1 || !matches!(ack.disposition.as_str(), "seen" | "applied") {
        return Err(invalid(
            "unsupported package acknowledgment version or disposition",
        ));
    }
    if !hex64(&ack.package_id) || !hex64(&ack.manifest_hash) {
        return Err(invalid("invalid package acknowledgment"));
    }
    require_schema(tx)?;
    let listed = normalize_change_ids(&ack.change_ids)?;
    let Some(package) = load_package(tx, &ack.package_id)? else {
        return Err(invalid("update package is missing"));
    };
    if ack.manifest_hash != package.manifest_hash {
        return Err(invalid("package acknowledgment digest mismatch"));
    }
    let mut expected = package.change_ids.clone();
    expected.sort();
    if listed != expected {
        return Err(invalid(
            "package acknowledgment must list that package's change ids",
        ));
    }
    let binding = load_binding(tx, &package.binding_id)?;
    if binding.generation != package.consumer_binding_generation {
        return Err(StoreError::Corrupt(
            "update package generation does not match its binding".into(),
        ));
    }
    if let Some(sequence) = receipt_sequences(
        tx,
        binding.generation,
        &package.change_ids,
        &ack.disposition,
        &package.package_id,
    )? {
        return Ok(PackageAckReceipt {
            package_id: package.package_id,
            disposition: ack.disposition.clone(),
            change_ids: package.change_ids,
            sequence,
        });
    }
    if ack.disposition == "applied" {
        if receipt_sequences(
            tx,
            binding.generation,
            &package.change_ids,
            "seen",
            &package.package_id,
        )?
        .is_none()
        {
            return Err(invalid(
                "explicit seen acknowledgment required before applied",
            ));
        }
        let mut stmt = tx.prepare(
            "SELECT d.record_id, d.revision FROM update_package_members m
             JOIN memory_delivery_intents d ON d.id=m.change_id
             WHERE m.package_id=?1 ORDER BY m.position",
        )?;
        let revisions = stmt
            .query_map([&package.package_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        for (record, revision) in revisions {
            let revision =
                u64::try_from(revision).map_err(|_| invalid("invalid change revision"))?;
            if !revision_current(tx, &record, revision, now)? {
                return Err(invalid(
                    "cannot apply a superseded or invalid memory revision; pull the current update",
                ));
            }
        }
    }
    let payload = serde_json::to_string(ack).map_err(|error| invalid(&error.to_string()))?;
    tx.execute(
        "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('memory.package_ack',?1,1,1,?2)",
        params![package.package_id, payload],
    )?;
    let sequence = head(tx)?;
    let generation =
        i64::try_from(binding.generation).map_err(|_| invalid("invalid consumer binding"))?;
    let stored = integer(sequence)?;
    for change_id in &package.change_ids {
        tx.execute(
            "INSERT INTO memory_change_receipts(consumer_binding_generation,change_id,disposition,binding_id,package_id,sequence)
             VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                generation,
                change_id,
                ack.disposition,
                package.binding_id,
                package.package_id,
                stored
            ],
        )?;
    }
    // A coordinator binding has no attempt. The cursor names this package only.
    if ack.disposition == "applied" && binding.attempt_id.is_none() {
        let updated = tx.execute(
            "UPDATE consumer_bindings SET applied_cursor=?2 WHERE binding_id=?1 AND attempt_id IS NULL",
            params![package.binding_id, package.package_id],
        )?;
        if updated != 1 {
            return Err(StoreError::Conflict);
        }
    }
    Ok(PackageAckReceipt {
        package_id: package.package_id,
        disposition: ack.disposition.clone(),
        change_ids: package.change_ids,
        sequence,
    })
}

impl SqliteStore {
    // Promotion commits obligations first. Nothing in this process calls the
    // materializer yet, so a crash before the package row can still rebuild it.
    #[allow(dead_code)]
    pub(crate) fn materialize_update_package(&mut self, binding_id: &str) -> Result<UpdatePackage> {
        if !binding_id_ok(binding_id) {
            return Err(invalid("invalid consumer binding"));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let package = materialize(&tx, binding_id)?;
        tx.commit()?;
        Ok(package)
    }

    #[allow(dead_code)]
    pub(crate) fn acknowledge_update_package(
        &mut self,
        ack: &UpdatePackageAck,
        now: i64,
    ) -> Result<PackageAckReceipt> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let receipt = acknowledge(&tx, ack, now)?;
        tx.commit()?;
        Ok(receipt)
    }

    /// Coordinator readiness follows the binding cursor, not worker receipts
    /// and not every change at or below a sequence.
    #[allow(dead_code)]
    pub(crate) fn applied_cursor_covers(
        &mut self,
        binding_id: &str,
        change_id: &str,
    ) -> Result<bool> {
        if !binding_id_ok(binding_id) || change_id.is_empty() {
            return Err(invalid("invalid consumer binding"));
        }
        let tx = self.connection.transaction()?;
        require_schema(&tx)?;
        let covered: bool = tx.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM consumer_bindings b
                JOIN update_package_members m ON m.package_id=b.applied_cursor
                WHERE b.binding_id=?1 AND m.change_id=?2
             )",
            params![binding_id, change_id],
            |row| row.get(0),
        )?;
        Ok(covered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        Applicability, Commit, ControlContext, MemoryKind, MemoryRecordId, Mutation, NewRevision,
        Task, TaskId, TaskState,
    };
    use crate::memory::MemoryStore;

    fn user_version(db: &Connection) -> u32 {
        db.query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }

    fn count(db: &Connection, sql: &str) -> i64 {
        db.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    fn digest() -> String {
        "a".repeat(64)
    }

    fn put_fact(memory: &mut MemoryStore) {
        let body = memory.ingest_object(&b"fact"[..]).unwrap();
        memory
            .insert_revision(
                &ControlContext { now_unix_ms: 1 },
                NewRevision {
                    id: MemoryRecordId::new("fact").unwrap(),
                    record_key: "fact".into(),
                    scope_id: "project".into(),
                    kind: MemoryKind::Observation,
                    body_hash: body.clone(),
                    provenance_hash: body,
                    applicability: Applicability {
                        domains: vec![],
                        paths: vec![],
                    },
                    dependencies: vec![],
                    expected: None,
                    expiry_unix_ms: None,
                    validity_state: "valid".into(),
                    validity_reason: "test".into(),
                },
            )
            .unwrap();
    }

    fn coordinator() -> (tempfile::TempDir, MemoryStore) {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SqliteStore::create(&dir.path().join("state.db")).unwrap();
        db.commit(Commit {
            expected_head: 0,
            mutations: vec![Mutation::Task {
                expected: None,
                next: Task {
                    id: TaskId::new("other").unwrap(),
                    revision: 1,
                    state: TaskState::Draft,
                    title: "other".into(),
                    active_attempt: None,
                },
            }],
        })
        .unwrap();
        let mut memory = MemoryStore::from_sqlite(db, dir.path().join("objects"));
        put_fact(&mut memory);
        memory
            .create_coordinator_snapshot(
                "session-a",
                "planner",
                &digest(),
                None,
                32_000,
                "Coordinate",
                1,
            )
            .unwrap();
        (dir, memory)
    }

    fn promote(memory: &mut MemoryStore, cause: &str, sequence: u64) {
        let tx = memory
            .store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        super::super::memory_delivery::record_change(
            &tx,
            cause,
            "fact",
            1,
            "informational",
            sequence,
        )
        .unwrap();
        tx.commit().unwrap();
    }

    fn sequence(memory: &MemoryStore, sql: &str) -> u64 {
        let value: i64 = memory
            .store
            .connection
            .query_row(sql, [], |row| row.get(0))
            .unwrap();
        u64::try_from(value).unwrap()
    }

    fn binding(memory: &mut MemoryStore) -> crate::store::consumer_bindings::ConsumerBinding {
        let snapshot: String = memory
            .store
            .connection
            .query_row(
                "SELECT id FROM memory_snapshots WHERE task_id='coordinator'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        memory
            .store
            .consumer_binding_for_snapshot(&snapshot)
            .unwrap()
            .unwrap()
    }

    fn delivery(memory: &MemoryStore, cause: &str) -> (String, u64) {
        memory
            .store
            .connection
            .query_row(
                "SELECT id, triggering_seq FROM memory_delivery_intents WHERE cause_id=?1",
                [cause],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
    }

    #[test]
    fn crash_rebuilds_the_same_package_and_old_ack_does_not_apply_a_new_change() {
        let (_dir, mut memory) = coordinator();
        let earliest = sequence(&memory, "SELECT min(sequence) FROM events");
        let latest = sequence(&memory, "SELECT max(sequence) FROM events");
        assert!(earliest < latest);
        promote(&mut memory, "change-a", latest);
        let binding = binding(&mut memory);
        assert!(binding.attempt_id.is_none());
        assert_eq!(
            count(
                &memory.store.connection,
                "SELECT count(*) FROM update_packages"
            ),
            0
        );
        let obligations = count(
            &memory.store.connection,
            "SELECT count(*) FROM consumer_binding_obligations",
        );
        let first = memory
            .store
            .materialize_update_package(&binding.binding_id)
            .unwrap();
        assert_eq!(first.change_ids.len(), 1);
        assert_eq!(
            memory
                .store
                .materialize_update_package(&binding.binding_id)
                .unwrap(),
            first
        );
        memory
            .store
            .connection
            .execute_batch(
                "DROP TRIGGER IF EXISTS update_packages_no_delete; DROP TRIGGER IF EXISTS update_package_members_no_delete; DELETE FROM update_package_members; DELETE FROM update_packages;",
            )
            .unwrap();
        assert_eq!(
            memory
                .store
                .materialize_update_package(&binding.binding_id)
                .unwrap(),
            first
        );
        assert_eq!(
            count(
                &memory.store.connection,
                "SELECT count(*) FROM consumer_binding_obligations"
            ),
            obligations
        );
        promote(&mut memory, "change-b", earliest);
        let (old_id, old_seq) = delivery(&memory, "change-a");
        let (new_id, new_seq) = delivery(&memory, "change-b");
        assert!(new_seq < old_seq);
        assert_eq!(first.change_ids, vec![old_id.clone()]);
        let wider = memory
            .store
            .materialize_update_package(&binding.binding_id)
            .unwrap();
        assert_ne!(wider.package_id, first.package_id);
        assert_eq!(
            wider.change_ids,
            vec![new_id.clone(), old_id.clone()]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
        );
        let attempts = count(&memory.store.connection, "SELECT count(*) FROM attempts");
        let worker_receipts = count(
            &memory.store.connection,
            "SELECT count(*) FROM memory_update_receipts",
        );
        let seen = UpdatePackageAck {
            schema_version: 1,
            package_id: first.package_id.clone(),
            manifest_hash: first.manifest_hash.clone(),
            change_ids: first.change_ids.clone(),
            disposition: "seen".into(),
        };
        let seen_receipt = memory.store.acknowledge_update_package(&seen, 1).unwrap();
        assert_eq!(
            memory.store.acknowledge_update_package(&seen, 1).unwrap(),
            seen_receipt
        );
        assert!(
            !memory
                .store
                .applied_cursor_covers(&binding.binding_id, &old_id)
                .unwrap()
        );
        let cursor: Option<String> = memory
            .store
            .connection
            .query_row(
                "SELECT applied_cursor FROM consumer_bindings WHERE binding_id=?1",
                [&binding.binding_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(cursor.is_none());
        let mut applied = seen.clone();
        applied.disposition = "applied".into();
        let applied_receipt = memory
            .store
            .acknowledge_update_package(&applied, 1)
            .unwrap();
        let head_after = sequence(&memory, "SELECT max(sequence) FROM events");
        assert_eq!(
            memory
                .store
                .acknowledge_update_package(&applied, 1)
                .unwrap(),
            applied_receipt
        );
        assert_eq!(
            sequence(&memory, "SELECT max(sequence) FROM events"),
            head_after
        );
        assert!(
            memory
                .store
                .applied_cursor_covers(&binding.binding_id, &old_id)
                .unwrap()
        );
        assert!(
            !memory
                .store
                .applied_cursor_covers(&binding.binding_id, &new_id)
                .unwrap()
        );
        let applied_new: i64 = memory
            .store
            .connection
            .query_row(
                "SELECT count(*) FROM memory_change_receipts WHERE change_id=?1 AND disposition='applied'",
                [&new_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(applied_new, 0);
        let cursor: String = memory
            .store
            .connection
            .query_row(
                "SELECT applied_cursor FROM consumer_bindings WHERE binding_id=?1",
                [&binding.binding_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cursor, first.package_id);
        let mut both = applied.clone();
        both.change_ids = vec![old_id, new_id];
        assert!(matches!(
            memory.store.acknowledge_update_package(&both, 1),
            Err(StoreError::Invalid(_))
        ));
        assert!(matches!(
            memory.store.acknowledge_update_package(
                &UpdatePackageAck {
                    disposition: "deferred".into(),
                    ..applied
                },
                1
            ),
            Err(StoreError::Invalid(_))
        ));
        assert_eq!(
            count(&memory.store.connection, "SELECT count(*) FROM attempts"),
            attempts
        );
        assert_eq!(
            count(
                &memory.store.connection,
                "SELECT count(*) FROM memory_update_receipts"
            ),
            worker_receipts
        );
    }

    #[test]
    fn create_ends_at_39_and_upgrade_from_38_preserves_worker_receipts() {
        let fresh = tempfile::tempdir().unwrap();
        let created = SqliteStore::create(&fresh.path().join("state.db")).unwrap();
        assert_eq!(user_version(&created.connection), 39);
        assert_eq!(
            created
                .connection
                .query_row("SELECT schema_version FROM store_meta", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            39
        );
        for table in [
            "update_packages",
            "update_package_members",
            "memory_change_receipts",
        ] {
            let sql: String = created
                .connection
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(sql.contains("STRICT"), "{table}");
        }
        let index: String = created
            .connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name='memory_change_receipts_by_generation'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(index.contains("consumer_binding_generation"));
        assert!(index.contains("change_id"));
        assert!(index.contains("disposition"));
        let open_fn = include_str!("mod.rs")
            .split("pub fn open")
            .nth(1)
            .unwrap()
            .split("pub fn integrity_check")
            .next()
            .unwrap();
        assert!(!open_fn.contains("upgrade_v1"));
        assert!(!open_fn.contains("0039_update_packages"));
        drop(created);

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.db");
        let mut db = SqliteStore::create(&path).unwrap();
        let tx = db.connection.transaction().unwrap();
        tx.execute(
            "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('fixture','fact',1,1,'{}')",
            [],
        )
        .unwrap();
        let sequence: i64 = tx
            .query_row("SELECT max(sequence) FROM events", [], |row| row.get(0))
            .unwrap();
        let hash = "a".repeat(64);
        tx.execute(
            "INSERT INTO objects(hash,size,availability,collection,pin_count,fencing_token) VALUES(?1,1,'available','unclaimed',0,0)",
            [&hash],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO memory_records(id,record_key,scope_id,kind,is_hard) VALUES('fact','fact','project','observation',0)",
            [],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO memory_revisions(record_id,revision,body_hash,provenance_hash,promoted_seq,applicability) VALUES('fact',1,?1,?1,?2,'{\"domains\":[],\"paths\":[]}')",
            params![hash, sequence],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO attempts(id,task_id,revision,state,snapshot,reservation,termination_observed) VALUES('attempt-1','worker',1,'running','snap-1','slot',0)",
            [],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO tasks(id,revision,state,title,active_attempt) VALUES('worker',1,'running','Worker','attempt-1')",
            [],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO memory_snapshots(id,task_id,task_revision,profile_name,profile_digest,config_digest,selection_policy_version,estimator,sequence,required_bytes,optional_bytes,budget_bytes,omitted_optional_count,manifest_hash,scope_digest) VALUES('snap-1','worker',1,'worker',?1,NULL,1,'test',1,0,0,0,0,?2,?3)",
            params!["b".repeat(64), "c".repeat(64), "d".repeat(64)],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO memory_delivery_intents(id,cause_id,subscriber,snapshot_id,task_id,record_id,revision,severity,triggering_seq,state) VALUES('delivery-1','cause','task:worker','snap-1','worker','fact',1,'informational',?1,'pending')",
            [sequence],
        )
        .unwrap();
        let manifest = "e".repeat(64);
        tx.execute(
            "INSERT INTO memory_update_receipts(delivery_id,attempt_id,state,manifest_hash,sequence) VALUES('delivery-1','attempt-1','seen',?1,?2)",
            params![manifest, sequence],
        )
        .unwrap();
        tx.commit().unwrap();
        let before: (String, String, String, String, i64) = db
            .connection
            .query_row(
                "SELECT delivery_id,attempt_id,state,manifest_hash,sequence FROM memory_update_receipts",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .unwrap();
        let attempts_before = count(&db.connection, "SELECT count(*) FROM attempts");
        drop(db);
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.execute_batch(
            "DROP TRIGGER IF EXISTS memory_change_receipts_no_update; DROP TRIGGER IF EXISTS memory_change_receipts_no_delete; DROP TRIGGER IF EXISTS update_package_members_no_update; DROP TRIGGER IF EXISTS update_package_members_no_delete; DROP TRIGGER IF EXISTS update_packages_no_update; DROP TRIGGER IF EXISTS update_packages_no_delete; DROP TABLE IF EXISTS memory_change_receipts; DROP TABLE IF EXISTS update_package_members; ALTER TABLE consumer_bindings DROP COLUMN applied_cursor; DROP TABLE IF EXISTS update_packages; UPDATE store_meta SET schema_version=38; PRAGMA user_version=38;",
        )
        .unwrap();
        drop(raw);
        let mut db = SqliteStore::open(&path).unwrap();
        assert_eq!(user_version(&db.connection), 38);
        assert!(matches!(
            db.import_legacy(&"cd".repeat(32), &[], &[]),
            Err(StoreError::UnsupportedSchema(38))
        ));
        assert_eq!(
            db.memory_update_receipts("attempt-1").unwrap(),
            vec![crate::domain::MemoryUpdateReceipt {
                delivery_id: "delivery-1".into(),
                attempt_id: "attempt-1".into(),
                state: "seen".into(),
                manifest_hash: manifest.clone(),
                sequence: sequence as u64,
            }]
        );
        db.upgrade_v1().unwrap();
        assert_eq!(user_version(&db.connection), 39);
        assert_eq!(
            db.connection
                .query_row("SELECT schema_version FROM store_meta", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            39
        );
        let after: (String, String, String, String, i64) = db
            .connection
            .query_row(
                "SELECT delivery_id,attempt_id,state,manifest_hash,sequence FROM memory_update_receipts",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .unwrap();
        assert_eq!(before, after);
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM attempts"),
            attempts_before
        );
        assert_eq!(
            db.memory_update_receipts("attempt-1").unwrap()[0].manifest_hash,
            manifest
        );
        check_schema(&db.connection).unwrap();
        drop(db);
        let reopened = SqliteStore::open(&path).unwrap();
        assert_eq!(user_version(&reopened.connection), 39);
    }
}

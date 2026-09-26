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
    task_id: Option<String>,
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
            "SELECT generation, task_id FROM consumer_bindings WHERE binding_id=?1",
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
        task_id: row.1,
    })
}

fn unresolved_members(db: &Connection, binding_id: &str) -> Result<Vec<Member>> {
    let mut stmt = db.prepare(
        "SELECT o.delivery_id, d.record_id, d.revision, v.body_hash, d.severity, d.triggering_seq, d.snapshot_id
         FROM consumer_binding_obligations o
         JOIN memory_delivery_intents d ON d.id=o.delivery_id
         JOIN memory_revisions v ON v.record_id=d.record_id AND v.revision=d.revision
         WHERE o.binding_id=?1
         AND NOT EXISTS (
             SELECT 1 FROM memory_change_receipts c
             WHERE c.binding_id=?1 AND c.change_id=o.delivery_id AND c.disposition='applied'
         )
         ORDER BY o.delivery_id LIMIT 10001",
    )?;
    let rows = stmt
        .query_map(params![binding_id], |row| {
            Ok(Member {
                change_id: row.get(0)?,
                record_id: row.get(1)?,
                revision: row.get(2)?,
                body_hash: row.get(3)?,
                severity: row.get(4)?,
                triggering_seq: row.get(5)?,
                snapshot_id: row.get(6)?,
            })
        })?
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
             WHERE c.binding_id=?1 AND c.change_id=o.delivery_id AND c.disposition='applied'
         )",
        params![binding_id],
        |row| row.get(0),
    )?;
    if obligations != rows.len() as i64 {
        return Err(StoreError::Corrupt(
            "obligation is missing its delivery revision".into(),
        ));
    }
    Ok(rows)
}

fn load_package(db: &Connection, package_id: &str) -> Result<Option<UpdatePackage>> {
    let row = db
        .query_row(
            "SELECT package_id, binding_id, consumer_binding_generation, manifest_hash
             FROM update_packages WHERE package_id=?1",
            [package_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((package_id, binding_id, generation, manifest_hash)) = row else {
        return Ok(None);
    };
    let mut stmt = db.prepare(
        "SELECT change_id FROM update_package_members WHERE package_id=?1 ORDER BY position",
    )?;
    let change_ids = stmt
        .query_map([&package_id], |row| row.get(0))?
        .collect::<std::result::Result<Vec<String>, _>>()?;
    Ok(Some(UpdatePackage {
        package_id,
        binding_id,
        consumer_binding_generation: u64::try_from(generation)
            .map_err(|_| StoreError::Corrupt("update package generation is invalid".into()))?,
        manifest_hash,
        change_ids,
    }))
}

fn materialize(tx: &rusqlite::Transaction, binding_id: &str) -> Result<UpdatePackage> {
    require_schema(tx)?;
    let binding = load_binding(tx, binding_id)?;
    let members = unresolved_members(tx, binding_id)?;
    if members.is_empty() {
        return Err(invalid("no unresolved obligations"));
    }
    let change_ids: Vec<String> = members
        .iter()
        .map(|member| member.change_id.clone())
        .collect();
    let package_id = package_identity(binding_id, binding.generation, &change_ids);
    let manifest = manifest_hash(binding_id, binding.generation, &members)?;
    if let Some(existing) = load_package(tx, &package_id)? {
        if existing.change_ids != change_ids
            || existing.manifest_hash != manifest
            || existing.binding_id != binding_id
            || existing.consumer_binding_generation != binding.generation
        {
            return Err(StoreError::Corrupt(
                "stored update package does not match its obligations".into(),
            ));
        }
        return Ok(existing);
    }
    let generation =
        i64::try_from(binding.generation).map_err(|_| invalid("invalid consumer binding"))?;
    tx.execute(
        "INSERT INTO update_packages(package_id,binding_id,consumer_binding_generation,manifest_hash)
         VALUES(?1,?2,?3,?4)",
        params![package_id, binding_id, generation, manifest],
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
    let mut missing = Vec::new();
    let mut elsewhere = Vec::new();
    let mut stored_sequences = Vec::new();
    for change_id in &package.change_ids {
        let row: Option<(String, i64)> = tx
            .query_row(
                "SELECT package_id, sequence FROM memory_change_receipts
                 WHERE binding_id=?1 AND change_id=?2 AND disposition=?3",
                params![package.binding_id, change_id, ack.disposition],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match row {
            None => missing.push(change_id.clone()),
            Some((stored_package, sequence)) if stored_package == package.package_id => {
                stored_sequences.push(u64::try_from(sequence).map_err(|_| {
                    StoreError::Corrupt("change receipt sequence is invalid".into())
                })?);
            }
            Some((stored_package, _)) => elsewhere.push((change_id.clone(), stored_package)),
        }
    }
    if missing.is_empty() && stored_sequences.is_empty() {
        return Err(StoreError::Conflict);
    }
    if missing.is_empty() && ack.disposition == "seen" {
        // Rows this ack inserted share the newest sequence on this package.
        // A later apply retargets older rows here without changing their sequence.
        let sequence =
            stored_sequences.iter().copied().max().ok_or_else(|| {
                invalid("package acknowledgment must list that package's change ids")
            })?;
        return Ok(PackageAckReceipt {
            package_id: package.package_id.clone(),
            disposition: ack.disposition.clone(),
            change_ids: package.change_ids.clone(),
            sequence,
        });
    }
    if missing.is_empty() && elsewhere.is_empty() {
        let sequence = stored_sequences[0];
        if stored_sequences.iter().any(|stored| *stored != sequence) {
            return Err(StoreError::Corrupt(
                "package acknowledgment receipts diverged".into(),
            ));
        }
        return Ok(PackageAckReceipt {
            package_id: package.package_id.clone(),
            disposition: ack.disposition.clone(),
            change_ids: package.change_ids.clone(),
            sequence,
        });
    }
    // A seen ack never moves an existing row onto an older or wider package.
    if ack.disposition == "seen" {
        if !stored_sequences.is_empty() && elsewhere.is_empty() {
            return Err(StoreError::Corrupt(
                "package acknowledgment receipts are partial".into(),
            ));
        }
    } else if !elsewhere.is_empty() || !stored_sequences.is_empty() {
        return Err(if elsewhere.is_empty() {
            StoreError::Corrupt("package acknowledgment receipts are partial".into())
        } else {
            StoreError::Conflict
        });
    }
    if ack.disposition == "applied" {
        let mut revisions_stmt = tx.prepare(
            "SELECT d.record_id, d.revision FROM update_package_members m
             JOIN memory_delivery_intents d ON d.id=m.change_id
             WHERE m.package_id=?1 ORDER BY m.position",
        )?;
        let revisions = revisions_stmt
            .query_map([&package.package_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(revisions_stmt);
        for (record, revision) in revisions {
            let revision =
                u64::try_from(revision).map_err(|_| invalid("invalid change revision"))?;
            let current: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM memory_heads h JOIN memory_validity v ON v.record_id=h.record_id AND v.revision=h.revision
                 WHERE h.record_id=?1 AND h.revision=?2 AND h.status='active' AND v.state='valid'
                 AND (v.expiry_unix_ms IS NULL OR v.expiry_unix_ms>?3))",
                params![record, i64::try_from(revision).map_err(|_| invalid("invalid change revision"))?, now],
                |row| row.get(0),
            )?;
            if !current {
                return Err(invalid(
                    "cannot apply a superseded or invalid memory revision; pull the current update",
                ));
            }
        }
        let mut seen_rows = Vec::new();
        for change_id in &package.change_ids {
            let seen: Option<String> = tx
                .query_row(
                    "SELECT package_id FROM memory_change_receipts WHERE binding_id=?1 AND change_id=?2 AND disposition='seen'",
                    params![package.binding_id, change_id],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(seen_package) = seen else {
                return Err(invalid(
                    "explicit seen acknowledgment required before applied",
                ));
            };
            seen_rows.push((change_id.clone(), seen_package));
        }
        let mut member_stmt =
            tx.prepare("SELECT change_id FROM update_package_members WHERE package_id=?1")?;
        for (_change_id, seen_package) in &seen_rows {
            if seen_package == &package.package_id {
                continue;
            }
            let members = member_stmt
                .query_map([seen_package], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            if members.is_empty()
                || members
                    .iter()
                    .any(|id| !package.change_ids.iter().any(|mine| mine == id))
            {
                return Err(StoreError::Conflict);
            }
        }
        drop(member_stmt);
        let payload = serde_json::to_string(ack).map_err(|error| invalid(&error.to_string()))?;
        tx.execute(
            "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('memory.package_ack',?1,1,1,?2)",
            params![package.package_id, payload],
        )?;
        let sequence = head(tx)?;
        let stored = integer(sequence)?;
        for (change_id, seen_package) in &seen_rows {
            if seen_package == &package.package_id {
                continue;
            }
            tx.execute(
                "UPDATE memory_change_receipts SET package_id=?1
                 WHERE binding_id=?2 AND change_id=?3 AND disposition='seen'",
                params![package.package_id, package.binding_id, change_id],
            )?;
        }
        let generation =
            i64::try_from(binding.generation).map_err(|_| invalid("invalid consumer binding"))?;
        for change_id in &package.change_ids {
            tx.execute(
                "INSERT INTO memory_change_receipts(binding_id,change_id,disposition,consumer_binding_generation,package_id,sequence)
                 VALUES(?1,?2,'applied',?3,?4,?5)",
                params![
                    package.binding_id,
                    change_id,
                    generation,
                    package.package_id,
                    stored
                ],
            )?;
        }
        if binding.task_id.is_none() {
            let updated = tx.execute(
                "UPDATE consumer_bindings SET applied_cursor=?2 WHERE binding_id=?1 AND task_id IS NULL",
                params![package.binding_id, package.package_id],
            )?;
            if updated != 1 {
                return Err(StoreError::Conflict);
            }
        }
        return Ok(PackageAckReceipt {
            package_id: package.package_id,
            disposition: ack.disposition.clone(),
            change_ids: package.change_ids,
            sequence,
        });
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
    for change_id in &missing {
        tx.execute(
            "INSERT INTO memory_change_receipts(binding_id,change_id,disposition,consumer_binding_generation,package_id,sequence)
             VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                package.binding_id,
                change_id,
                ack.disposition,
                generation,
                package.package_id,
                stored
            ],
        )?;
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

    /// Applied changes for this binding. Not the latest cursor package and not
    /// every change at or below a triggering sequence.
    #[allow(dead_code)]
    pub(crate) fn applied_cursor_covers(
        &mut self,
        binding_id: &str,
        change_id: &str,
    ) -> Result<bool> {
        if !binding_id_ok(binding_id) {
            return Err(invalid("invalid consumer binding"));
        }
        if change_id.is_empty() || change_id.len() > 128 {
            return Err(invalid("invalid change id"));
        }
        let tx = self.connection.transaction()?;
        require_schema(&tx)?;
        let covered: bool = tx.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM memory_change_receipts
                WHERE binding_id=?1 AND change_id=?2 AND disposition='applied'
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
        let only_new = memory
            .store
            .materialize_update_package(&binding.binding_id)
            .unwrap();
        assert_eq!(only_new.change_ids, vec![new_id.clone()]);
        let mut seen_new = UpdatePackageAck {
            schema_version: 1,
            package_id: only_new.package_id.clone(),
            manifest_hash: only_new.manifest_hash.clone(),
            change_ids: only_new.change_ids.clone(),
            disposition: "seen".into(),
        };
        memory
            .store
            .acknowledge_update_package(&seen_new, 1)
            .unwrap();
        seen_new.disposition = "applied".into();
        memory
            .store
            .acknowledge_update_package(&seen_new, 1)
            .unwrap();
        assert!(
            memory
                .store
                .applied_cursor_covers(&binding.binding_id, &old_id)
                .unwrap()
        );
        assert!(
            memory
                .store
                .applied_cursor_covers(&binding.binding_id, &new_id)
                .unwrap()
        );
        let cursor: String = memory
            .store
            .connection
            .query_row(
                "SELECT applied_cursor FROM consumer_bindings WHERE binding_id=?1",
                [&binding.binding_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cursor, only_new.package_id);
        assert!(new_seq < old_seq);
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
        assert_eq!(user_version(&created.connection), 40);
        assert_eq!(
            created
                .connection
                .query_row("SELECT schema_version FROM store_meta", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            40
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
        let receipt_sql: String = created
            .connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='memory_change_receipts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(receipt_sql.contains("PRIMARY KEY (binding_id, change_id, disposition)"));
        assert_eq!(
            created
                .connection
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='index' AND name='memory_change_receipts_by_generation'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );
        assert!(
            !created
                .connection
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE type='table' AND name='update_packages'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap()
                .contains("created_seq")
        );
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
            "DROP TRIGGER IF EXISTS barrier_stale_briefs_no_update; DROP TRIGGER IF EXISTS barrier_stale_briefs_no_delete; DROP TRIGGER IF EXISTS barrier_members_no_update; DROP TRIGGER IF EXISTS barrier_members_no_delete; DROP TRIGGER IF EXISTS barrier_revisions_no_membership_update; DROP TABLE IF EXISTS barrier_stale_briefs; DROP TABLE IF EXISTS barrier_members; DROP TABLE IF EXISTS barrier_revisions; DROP TRIGGER IF EXISTS memory_change_receipts_no_update; DROP TRIGGER IF EXISTS memory_change_receipts_no_delete; DROP TRIGGER IF EXISTS update_package_members_no_update; DROP TRIGGER IF EXISTS update_package_members_no_delete; DROP TRIGGER IF EXISTS update_packages_no_update; DROP TRIGGER IF EXISTS update_packages_no_delete; DROP TABLE IF EXISTS memory_change_receipts; DROP TABLE IF EXISTS update_package_members; ALTER TABLE consumer_bindings DROP COLUMN applied_cursor; DROP TABLE IF EXISTS update_packages; UPDATE store_meta SET schema_version=38; PRAGMA user_version=38;",
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
        assert_eq!(user_version(&db.connection), 40);
        assert_eq!(
            db.connection
                .query_row("SELECT schema_version FROM store_meta", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            40
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
        assert_eq!(user_version(&reopened.connection), 40);
    }

    fn ack(package: &UpdatePackage, disposition: &str) -> UpdatePackageAck {
        UpdatePackageAck {
            schema_version: 1,
            package_id: package.package_id.clone(),
            manifest_hash: package.manifest_hash.clone(),
            change_ids: package.change_ids.clone(),
            disposition: disposition.into(),
        }
    }

    #[test]
    fn later_package_supersedes_unapplied_seen_and_old_package_stays_readable() {
        let (_dir, mut memory) = coordinator();
        let latest = sequence(&memory, "SELECT max(sequence) FROM events");
        promote(&mut memory, "change-a", latest);
        let binding = binding(&mut memory);
        let first = memory
            .store
            .materialize_update_package(&binding.binding_id)
            .unwrap();
        memory
            .store
            .acknowledge_update_package(&ack(&first, "seen"), 1)
            .unwrap();
        let earliest = sequence(&memory, "SELECT min(sequence) FROM events");
        promote(&mut memory, "change-b", earliest);
        let (old_id, _) = delivery(&memory, "change-a");
        let (new_id, _) = delivery(&memory, "change-b");
        let wider = memory
            .store
            .materialize_update_package(&binding.binding_id)
            .unwrap();
        let stored_manifest: String = memory
            .store
            .connection
            .query_row(
                "SELECT manifest_hash FROM update_packages WHERE package_id=?1",
                [&first.package_id],
                |row| row.get(0),
            )
            .unwrap();
        let stored_members: Vec<String> = memory
            .store
            .connection
            .prepare("SELECT change_id FROM update_package_members WHERE package_id=?1 ORDER BY position")
            .unwrap()
            .query_map([&first.package_id], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(stored_manifest, first.manifest_hash);
        assert_eq!(stored_members, vec![old_id.clone()]);
        let seen_first = memory
            .store
            .acknowledge_update_package(&ack(&first, "seen"), 1)
            .unwrap();
        let seen_wider = memory
            .store
            .acknowledge_update_package(&ack(&wider, "seen"), 1)
            .unwrap();
        let head_after_wider_seen = sequence(&memory, "SELECT max(sequence) FROM events");
        assert_eq!(
            memory
                .store
                .acknowledge_update_package(&ack(&wider, "seen"), 1)
                .unwrap(),
            seen_wider
        );
        assert_eq!(
            sequence(&memory, "SELECT max(sequence) FROM events"),
            head_after_wider_seen
        );
        let seen_package: (String, i64) = memory
            .store
            .connection
            .query_row(
                "SELECT package_id, sequence FROM memory_change_receipts WHERE binding_id=?1 AND change_id=?2 AND disposition='seen'",
                params![binding.binding_id, old_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(seen_package.0, first.package_id);
        assert_eq!(seen_package.1, seen_first.sequence as i64);
        assert_eq!(
            memory
                .store
                .acknowledge_update_package(&ack(&first, "seen"), 1)
                .unwrap(),
            seen_first
        );
        assert_eq!(
            sequence(&memory, "SELECT max(sequence) FROM events"),
            head_after_wider_seen
        );
        memory
            .store
            .connection
            .execute(
                "INSERT INTO memory_revisions SELECT record_id,2,body_hash,provenance_hash,promoted_seq,applicability FROM memory_revisions WHERE record_id='fact' AND revision=1",
                [],
            )
            .unwrap();
        memory
            .store
            .connection
            .execute(
                "UPDATE memory_heads SET revision=2 WHERE record_id='fact'",
                [],
            )
            .unwrap();
        assert!(matches!(
            memory
                .store
                .acknowledge_update_package(&ack(&wider, "applied"), 1),
            Err(StoreError::Invalid(_))
        ));
        let still_first: String = memory
            .store
            .connection
            .query_row(
                "SELECT package_id FROM memory_change_receipts WHERE binding_id=?1 AND change_id=?2 AND disposition='seen'",
                params![binding.binding_id, old_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(still_first, first.package_id);
        memory
            .store
            .connection
            .execute(
                "UPDATE memory_heads SET revision=1 WHERE record_id='fact'",
                [],
            )
            .unwrap();
        memory
            .store
            .acknowledge_update_package(&ack(&wider, "applied"), 1)
            .unwrap();
        assert!(
            memory
                .store
                .applied_cursor_covers(&binding.binding_id, &old_id)
                .unwrap()
        );
        assert!(
            memory
                .store
                .applied_cursor_covers(&binding.binding_id, &new_id)
                .unwrap()
        );
        let moved: String = memory
            .store
            .connection
            .query_row(
                "SELECT package_id FROM memory_change_receipts WHERE binding_id=?1 AND change_id=?2 AND disposition='seen'",
                params![binding.binding_id, old_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(moved, wider.package_id);
        let head_after_apply = sequence(&memory, "SELECT max(sequence) FROM events");
        assert_eq!(
            memory
                .store
                .acknowledge_update_package(&ack(&wider, "seen"), 1)
                .unwrap()
                .sequence,
            seen_wider.sequence
        );
        assert_eq!(
            sequence(&memory, "SELECT max(sequence) FROM events"),
            head_after_apply
        );
        assert!(matches!(
            memory
                .store
                .acknowledge_update_package(&ack(&first, "seen"), 1),
            Err(StoreError::Conflict)
        ));
        assert_eq!(
            sequence(&memory, "SELECT max(sequence) FROM events"),
            head_after_apply
        );
        let stayed: String = memory
            .store
            .connection
            .query_row(
                "SELECT package_id FROM memory_change_receipts WHERE binding_id=?1 AND change_id=?2 AND disposition='seen'",
                params![binding.binding_id, old_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stayed, wider.package_id);
        let unchanged_members: Vec<String> = memory
            .store
            .connection
            .prepare("SELECT change_id FROM update_package_members WHERE package_id=?1 ORDER BY position")
            .unwrap()
            .query_map([&first.package_id], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(unchanged_members, vec![old_id.clone()]);
        let mut extra = ack(&first, "applied");
        extra.change_ids = vec![old_id, new_id.clone()];
        assert!(matches!(
            memory.store.acknowledge_update_package(&extra, 1),
            Err(StoreError::Invalid(_))
        ));
        let applied_package: String = memory
            .store
            .connection
            .query_row(
                "SELECT package_id FROM memory_change_receipts WHERE binding_id=?1 AND change_id=?2 AND disposition='applied'",
                params![binding.binding_id, new_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(applied_package, wider.package_id);
        let err = memory
            .store
            .applied_cursor_covers(&binding.binding_id, "")
            .unwrap_err();
        assert!(matches!(err, StoreError::Invalid(message) if message == "invalid change id"));
    }

    #[test]
    fn same_generation_does_not_hide_another_bindings_copied_delivery() {
        let (_dir, mut memory) = coordinator();
        let latest = sequence(&memory, "SELECT max(sequence) FROM events");
        promote(&mut memory, "change-a", latest);
        let coordinator_binding = binding(&mut memory);
        assert_eq!(coordinator_binding.generation, 1);
        let (change_id, _) = delivery(&memory, "change-a");
        let other = memory
            .create_task_snapshot(
                crate::domain::SnapshotRequest {
                    schema_version: 1,
                    task_id: "other".into(),
                    profile: "worker".into(),
                    domains: vec![],
                    paths: vec![],
                    pinned_keys: vec!["fact".into()],
                    sensitivity: "default".into(),
                },
                "worker",
                &digest(),
                None,
                32_000,
                "other",
                1,
                None,
            )
            .unwrap();
        let other_binding = memory
            .store
            .consumer_binding_for_snapshot(other.id.as_str())
            .unwrap()
            .unwrap();
        assert_eq!(other_binding.generation, 1);
        assert_ne!(other_binding.binding_id, coordinator_binding.binding_id);
        memory
            .store
            .connection
            .execute(
                "INSERT INTO consumer_binding_obligations(binding_id,delivery_id) VALUES(?1,?2)",
                params![other_binding.binding_id, change_id],
            )
            .unwrap();
        let first = memory
            .store
            .materialize_update_package(&coordinator_binding.binding_id)
            .unwrap();
        memory
            .store
            .acknowledge_update_package(&ack(&first, "seen"), 1)
            .unwrap();
        memory
            .store
            .acknowledge_update_package(&ack(&first, "applied"), 1)
            .unwrap();
        let copied = memory
            .store
            .materialize_update_package(&other_binding.binding_id)
            .unwrap();
        assert_eq!(copied.change_ids, vec![change_id.clone()]);
        memory
            .store
            .acknowledge_update_package(&ack(&copied, "seen"), 1)
            .unwrap();
        memory
            .store
            .acknowledge_update_package(&ack(&copied, "applied"), 1)
            .unwrap();
        assert!(
            memory
                .store
                .applied_cursor_covers(&other_binding.binding_id, &change_id)
                .unwrap()
        );
        let again = memory
            .create_coordinator_snapshot(
                "session-a",
                "planner",
                &digest(),
                None,
                32_000,
                "Coordinate again",
                2,
            )
            .unwrap();
        let later = memory
            .store
            .consumer_binding_for_snapshot(again.id.as_str())
            .unwrap()
            .unwrap();
        assert!(later.generation > coordinator_binding.generation);
        assert_eq!(later.consumer_id, coordinator_binding.consumer_id);
        memory
            .store
            .connection
            .execute(
                "INSERT INTO consumer_binding_obligations(binding_id,delivery_id) VALUES(?1,?2)",
                params![later.binding_id, change_id],
            )
            .unwrap();
        let next = memory
            .store
            .materialize_update_package(&later.binding_id)
            .unwrap();
        assert_eq!(next.change_ids, vec![change_id.clone()]);
        memory
            .store
            .acknowledge_update_package(&ack(&next, "seen"), 1)
            .unwrap();
        memory
            .store
            .acknowledge_update_package(&ack(&next, "applied"), 1)
            .unwrap();
        assert!(
            memory
                .store
                .applied_cursor_covers(&later.binding_id, &change_id)
                .unwrap()
        );
        let cursor: Option<String> = memory
            .store
            .connection
            .query_row(
                "SELECT applied_cursor FROM consumer_bindings WHERE binding_id=?1",
                [&other_binding.binding_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(cursor.is_none());
    }
}

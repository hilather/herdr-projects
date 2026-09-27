//! Immutable packages of obligations already recorded for one binding generation.
//! Rebuilding reads those rows. It does not invent obligations or attempts.
use super::*;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

const SCHEMA_VERSION: u32 = 39;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdatePackage {
    pub package_id: String,
    pub binding_id: String,
    pub consumer_binding_generation: u64,
    pub manifest_hash: String,
    pub change_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdatePackageAck {
    pub schema_version: u32,
    pub package_id: String,
    pub manifest_hash: String,
    pub change_ids: Vec<String>,
    pub disposition: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PackageAckReceipt {
    pub package_id: String,
    pub disposition: String,
    pub change_ids: Vec<String>,
    pub sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkerPackageAckReceipt {
    pub schema_version: u32,
    pub receipt: PackageAckReceipt,
    pub evidence_sequence: u64,
}

#[derive(Serialize)]
struct WorkerChangeReceipt {
    change_id: String,
    manifest_hash: String,
    sequence: u64,
}

struct WorkerMembers {
    package: UpdatePackage,
    updates: Vec<MemoryUpdate>,
    receipts: Vec<WorkerChangeReceipt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdatePackageMember {
    pub change_id: String,
    pub record_id: String,
    pub revision: u64,
    pub body_hash: String,
    pub severity: String,
    pub triggering_seq: u64,
    pub snapshot_id: String,
}
type Member = UpdatePackageMember;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdatePackageManifest {
    pub schema_version: u32,
    pub package: UpdatePackage,
    pub members: Vec<UpdatePackageMember>,
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
    let supersession = supersession_filter(db)?;
    let mut stmt = db.prepare(&format!(
        "SELECT o.delivery_id, d.record_id, d.revision, v.body_hash, d.severity, d.triggering_seq, d.snapshot_id
         FROM consumer_binding_obligations o
         JOIN memory_delivery_intents d ON d.id=o.delivery_id
         JOIN memory_revisions v ON v.record_id=d.record_id AND v.revision=d.revision
         WHERE o.binding_id=?1
         AND NOT EXISTS (
             SELECT 1 FROM memory_change_receipts c
             WHERE c.binding_id=?1 AND c.change_id=o.delivery_id AND c.disposition='applied'
         )
         {supersession} ORDER BY o.delivery_id LIMIT 10001"
    ))?;
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
        &format!(
            "SELECT count(*) FROM consumer_binding_obligations o
         WHERE o.binding_id=?1
         AND NOT EXISTS (
             SELECT 1 FROM memory_change_receipts c
             WHERE c.binding_id=?1 AND c.change_id=o.delivery_id AND c.disposition='applied'
         ) {supersession}"
        ),
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

fn supersession_filter(db: &Connection) -> Result<&'static str> {
    let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    Ok(if version >= 43 {
        "AND NOT EXISTS(SELECT 1 FROM memory_update_supersessions s WHERE s.binding_id=o.binding_id AND s.change_id=o.delivery_id)"
    } else {
        ""
    })
}

// Exact indexed lookups let a caller pull a bounded batch without scanning or
// deleting the rest of a consumer's backlog. Every requested change must belong
// to this binding and still need an applied receipt.
fn selected_members(db: &Connection, binding: &str, selected: &[String]) -> Result<Vec<Member>> {
    let selected = normalize_change_ids(selected)?;
    let supersession = supersession_filter(db)?;
    let mut query = db.prepare(&format!(
        "SELECT o.delivery_id,d.record_id,d.revision,v.body_hash,d.severity,d.triggering_seq,d.snapshot_id
         FROM consumer_binding_obligations o JOIN memory_delivery_intents d ON d.id=o.delivery_id
         JOIN memory_revisions v ON v.record_id=d.record_id AND v.revision=d.revision
         WHERE o.binding_id=?1 AND o.delivery_id=?2 AND NOT EXISTS (
           SELECT 1 FROM memory_change_receipts c WHERE c.binding_id=?1 AND c.change_id=?2 AND c.disposition='applied') {supersession}"
    ))?;
    selected
        .iter()
        .map(|change| {
            query
                .query_row(params![binding, change], |row| {
                    Ok(Member {
                        change_id: row.get(0)?,
                        record_id: row.get(1)?,
                        revision: row.get(2)?,
                        body_hash: row.get(3)?,
                        severity: row.get(4)?,
                        triggering_seq: row.get(5)?,
                        snapshot_id: row.get(6)?,
                    })
                })
                .optional()?
                .ok_or_else(|| {
                    invalid("selected change is not an unresolved obligation of this binding")
                })
        })
        .collect()
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
    Ok(materialize_parts(tx, binding_id, None)?.0)
}
fn materialize_parts(
    tx: &rusqlite::Transaction,
    binding_id: &str,
    selected: Option<&[String]>,
) -> Result<(UpdatePackage, Vec<Member>)> {
    require_schema(tx)?;
    let binding = load_binding(tx, binding_id)?;
    let members = match selected {
        Some(changes) => selected_members(tx, binding_id, changes)?,
        None => unresolved_members(tx, binding_id)?,
    };
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
        return Ok((existing, members));
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
    Ok((
        UpdatePackage {
            package_id,
            binding_id: binding_id.into(),
            consumer_binding_generation: binding.generation,
            manifest_hash: manifest,
            change_ids,
        },
        members,
    ))
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
    // Package acceptance has its own immutable event. Logical change receipts
    // retain their original source package even when another package repeats it.
    // The kind/entity index bounds this lookup to this exact package (at most
    // one accepted event per disposition), independent of event history.
    let mut prior = tx.prepare(
        "SELECT sequence,payload,revision,payload_version FROM events WHERE kind='memory.package_ack' AND entity=?1 ORDER BY sequence LIMIT 3",
    )?;
    let events = prior
        .query_map([&package.package_id], |row| {
            Ok((
                row.get::<_, u64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, u64>(2)?,
                row.get::<_, u64>(3)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if events.len() > 2 {
        return Err(StoreError::Corrupt(
            "duplicate package acceptance events".into(),
        ));
    }
    let mut accepted = None;
    let mut dispositions = std::collections::BTreeSet::new();
    for (sequence, payload, revision, version) in events {
        let event: UpdatePackageAck = serde_json::from_str(&payload)
            .map_err(|_| StoreError::Corrupt("invalid package acceptance event".into()))?;
        if revision != 1
            || version != 1
            || event.schema_version != 1
            || event.package_id != package.package_id
            || event.manifest_hash != package.manifest_hash
            || normalize_change_ids(&event.change_ids)? != expected
            || !matches!(event.disposition.as_str(), "seen" | "applied")
            || !dispositions.insert(event.disposition.clone())
        {
            return Err(StoreError::Corrupt(
                "package acceptance event mismatch".into(),
            ));
        }
        if event.disposition == ack.disposition {
            accepted = Some(sequence);
        }
    }
    drop(prior);
    let mut missing = Vec::new();
    for change in &package.change_ids {
        let generation: Option<u64> = tx
            .query_row(
                "SELECT consumer_binding_generation FROM memory_change_receipts
             WHERE binding_id=?1 AND change_id=?2 AND disposition=?3",
                params![package.binding_id, change, ack.disposition],
                |row| row.get(0),
            )
            .optional()?;
        match generation {
            None => missing.push(change),
            Some(generation) if generation == binding.generation => {}
            Some(_) => {
                return Err(StoreError::Corrupt(
                    "change receipt generation mismatch".into(),
                ));
            }
        }
    }
    if let Some(sequence) = accepted {
        if !missing.is_empty() {
            return Err(StoreError::Corrupt(
                "package acceptance is missing change receipts".into(),
            ));
        }
        return Ok(PackageAckReceipt {
            package_id: package.package_id,
            disposition: ack.disposition.clone(),
            change_ids: package.change_ids,
            sequence,
        });
    }
    if ack.disposition == "applied" {
        for change in &package.change_ids {
            let eligible: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM memory_delivery_intents d
                 JOIN memory_heads h ON h.record_id=d.record_id AND h.revision=d.revision
                 JOIN memory_validity v ON v.record_id=h.record_id AND v.revision=h.revision
                 WHERE d.id=?1 AND h.status='active' AND v.state='valid'
                 AND (v.expiry_unix_ms IS NULL OR v.expiry_unix_ms>?2))",
                params![change, now],
                |row| row.get(0),
            )?;
            if !eligible {
                return Err(invalid(
                    "cannot apply a superseded or invalid memory revision; pull the current update",
                ));
            }
            let seen: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM memory_change_receipts WHERE binding_id=?1 AND change_id=?2
                 AND disposition='seen' AND consumer_binding_generation=?3)",
                params![package.binding_id, change, integer(binding.generation)?], |row| row.get(0),
            )?;
            if !seen {
                return Err(invalid(
                    "explicit seen acknowledgment required before applied",
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
    for change in missing {
        tx.execute(
            "INSERT INTO memory_change_receipts(binding_id,change_id,disposition,consumer_binding_generation,package_id,sequence)
             VALUES(?1,?2,?3,?4,?5,?6)",
            params![package.binding_id, change, ack.disposition, integer(binding.generation)?, package.package_id, integer(sequence)?],
        )?;
    }
    if ack.disposition == "applied" && binding.task_id.is_none() {
        tx.execute("UPDATE consumer_bindings SET applied_cursor=?2 WHERE binding_id=?1 AND task_id IS NULL",
            params![package.binding_id, package.package_id])?;
    }
    Ok(PackageAckReceipt {
        package_id: package.package_id,
        disposition: ack.disposition.clone(),
        change_ids: package.change_ids,
        sequence,
    })
}

// This production worker path aggregates existing exact-change declarations.
// A caller's package manifest alone is never supporting application evidence.
fn worker_members(db: &Connection, attempt: &str, ack: &UpdatePackageAck) -> Result<WorkerMembers> {
    require_schema(db)?;
    let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < 43 {
        return Err(StoreError::UnsupportedSchema(version));
    }
    if ack.schema_version != 1
        || !matches!(ack.disposition.as_str(), "seen" | "applied")
        || !hex64(&ack.package_id)
        || !hex64(&ack.manifest_hash)
        || AttemptId::new(attempt).is_err()
    {
        return Err(invalid("invalid worker package acknowledgment"));
    }
    let package =
        load_package(db, &ack.package_id)?.ok_or_else(|| invalid("update package is missing"))?;
    let mut expected = package.change_ids.clone();
    expected.sort();
    if package.manifest_hash != ack.manifest_hash
        || normalize_change_ids(&ack.change_ids)? != expected
    {
        return Err(invalid(
            "worker acknowledgment does not match the exact package",
        ));
    }
    let current: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM consumer_bindings b JOIN attempts a ON a.id=b.attempt_id AND a.task_id=b.task_id
         JOIN tasks t ON t.id=a.task_id AND t.active_attempt=a.id
         WHERE b.binding_id=?1 AND b.generation=?2 AND a.id=?3 AND a.snapshot=b.snapshot_id
         AND b.active=1 AND b.retired=0 AND a.termination_observed=0 AND a.state IN ('running','awaiting_input'))",
        params![package.binding_id, integer(package.consumer_binding_generation)?, attempt], |row| row.get(0),
    )?;
    if !current {
        return Err(invalid(
            "package does not belong to the current live worker binding",
        ));
    }
    let mut updates = Vec::with_capacity(package.change_ids.len());
    let mut receipts = Vec::with_capacity(package.change_ids.len());
    for change in &package.change_ids {
        let item = super::memory_receipts::update(db, change, attempt)?;
        let received: Option<u64> = db.query_row(
            "SELECT sequence FROM memory_update_receipts WHERE delivery_id=?1 AND attempt_id=?2 AND state=?3 AND manifest_hash=?4",
            params![change, attempt, ack.disposition, item.manifest_hash], |row| row.get(0),
        ).optional()?;
        let sequence = received.ok_or_else(|| {
            invalid("exact worker change receipts are required before package acknowledgment")
        })?;
        receipts.push(WorkerChangeReceipt {
            change_id: change.clone(),
            manifest_hash: item.manifest_hash.clone(),
            sequence,
        });
        updates.push(item);
    }
    Ok(WorkerMembers {
        package,
        updates,
        receipts,
    })
}

impl SqliteStore {
    pub(crate) fn worker_package_updates(
        &mut self,
        attempt: &str,
        ack: &UpdatePackageAck,
    ) -> Result<Vec<MemoryUpdate>> {
        let tx = self.connection.transaction()?;
        Ok(worker_members(&tx, attempt, ack)?.updates)
    }

    pub(crate) fn acknowledge_worker_update_package(
        &mut self,
        attempt: &str,
        ack: &UpdatePackageAck,
        now: i64,
    ) -> Result<WorkerPackageAckReceipt> {
        super::delivery::now_check(now)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let members = worker_members(&tx, attempt, ack)?;
        let receipt = acknowledge(&tx, ack, now)?;
        let evidence = serde_json::json!({
            "schema_version": 1, "protocol": "worker-change-declaration-v1",
            "package_id": ack.package_id, "manifest_hash": ack.manifest_hash,
            "binding_id": members.package.binding_id,
            "consumer_binding_generation": members.package.consumer_binding_generation,
            "attempt_id": attempt, "disposition": ack.disposition,
            "package_receipt_sequence": receipt.sequence, "receipts": members.receipts,
        });
        let existing: Option<(u64, u64)> = tx.query_row(
            "SELECT package_receipt_sequence,evidence_sequence FROM worker_package_acknowledgments WHERE package_id=?1 AND attempt_id=?2 AND disposition=?3",
            params![ack.package_id, attempt, ack.disposition], |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?;
        let evidence_sequence = if let Some((accepted, sequence)) = existing {
            let stored: Option<String> = tx.query_row(
                "SELECT payload FROM events WHERE sequence=?1 AND kind='memory.worker_package_ack' AND entity=?2 AND revision=1 AND payload_version=1",
                params![integer(sequence)?, ack.package_id], |row| row.get(0),
            ).optional()?;
            let stored =
                stored.and_then(|payload| serde_json::from_str::<serde_json::Value>(&payload).ok());
            if accepted != receipt.sequence || stored.as_ref() != Some(&evidence) {
                return Err(StoreError::Corrupt(
                    "worker package supporting evidence mismatch".into(),
                ));
            }
            sequence
        } else {
            let payload =
                serde_json::to_string(&evidence).map_err(|error| invalid(&error.to_string()))?;
            if payload.len() > 8 * 1024 * 1024 {
                return Err(StoreError::Limit(
                    "worker package evidence exceeds 8 MiB".into(),
                ));
            }
            tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('memory.worker_package_ack',?1,1,1,?2)", params![ack.package_id, payload])?;
            let sequence = head(&tx)?;
            tx.execute(
                "INSERT INTO worker_package_acknowledgments VALUES(?1,?2,?3,?4,?5)",
                params![
                    ack.package_id,
                    attempt,
                    ack.disposition,
                    integer(receipt.sequence)?,
                    integer(sequence)?
                ],
            )?;
            sequence
        };
        tx.commit()?;
        Ok(WorkerPackageAckReceipt {
            schema_version: 1,
            receipt,
            evidence_sequence,
        })
    }

    /// Promotion commits obligations first; a later pull materializes the same
    /// immutable package after a crash, without acknowledging any obligation.
    pub fn materialize_update_package(&mut self, binding_id: &str) -> Result<UpdatePackage> {
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

    pub fn materialize_update_package_manifest(
        &mut self,
        binding_id: &str,
    ) -> Result<UpdatePackageManifest> {
        if !binding_id_ok(binding_id) {
            return Err(invalid("invalid consumer binding"));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (package, members) = materialize_parts(&tx, binding_id, None)?;
        tx.commit()?;
        Ok(UpdatePackageManifest {
            schema_version: 1,
            package,
            members,
        })
    }

    pub fn materialize_selected_update_package_manifest(
        &mut self,
        binding_id: &str,
        changes: &[String],
    ) -> Result<UpdatePackageManifest> {
        if !binding_id_ok(binding_id) {
            return Err(invalid("invalid consumer binding"));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (package, members) = materialize_parts(&tx, binding_id, Some(changes))?;
        tx.commit()?;
        Ok(UpdatePackageManifest {
            schema_version: 1,
            package,
            members,
        })
    }

    pub fn acknowledge_update_package(
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
    pub fn applied_cursor_covers(&mut self, binding_id: &str, change_id: &str) -> Result<bool> {
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
        assert_eq!(user_version(&created.connection), crate::store::SCHEMA);
        assert_eq!(
            created
                .connection
                .query_row("SELECT schema_version FROM store_meta", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            crate::store::SCHEMA
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
        crate::store::test_schema::historical(&raw, 38).unwrap();
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
        assert_eq!(user_version(&db.connection), crate::store::SCHEMA);
        assert_eq!(
            db.connection
                .query_row("SELECT schema_version FROM store_meta", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            crate::store::SCHEMA
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
        assert_eq!(user_version(&reopened.connection), crate::store::SCHEMA);
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
    fn selected_packages_preserve_logical_receipts_and_reject_foreign_or_missing_changes() {
        let (_dir, mut memory) = coordinator();
        let seq = sequence(&memory, "SELECT max(sequence) FROM events");
        promote(&mut memory, "change-a", seq);
        promote(&mut memory, "change-b", seq);
        let binding = binding(&mut memory);
        let wide = memory
            .store
            .materialize_update_package(&binding.binding_id)
            .unwrap();
        let original = memory
            .store
            .acknowledge_update_package(&ack(&wide, "seen"), 1)
            .unwrap();
        let selected = vec![wide.change_ids[0].clone()];
        let narrow = memory
            .store
            .materialize_selected_update_package_manifest(&binding.binding_id, &selected)
            .unwrap();
        assert_eq!(narrow.package.change_ids, selected);
        let seen = memory
            .store
            .acknowledge_update_package(&ack(&narrow.package, "seen"), 1)
            .unwrap();
        assert_ne!(seen.sequence, original.sequence);
        let applied = memory
            .store
            .acknowledge_update_package(&ack(&narrow.package, "applied"), 1)
            .unwrap();
        assert_eq!(
            memory
                .store
                .acknowledge_update_package(&ack(&narrow.package, "applied"), 1)
                .unwrap(),
            applied
        );
        assert_eq!(
            memory
                .store
                .acknowledge_update_package(&ack(&wide, "seen"), 1)
                .unwrap(),
            original
        );
        let source: (String,u64) = memory.store.connection.query_row(
            "SELECT package_id,sequence FROM memory_change_receipts WHERE change_id=?1 AND disposition='seen'",
            [&selected[0]], |row| Ok((row.get(0)?,row.get(1)?)),
        ).unwrap();
        assert_eq!(source, (wide.package_id.clone(), original.sequence));
        let remaining = &wide.change_ids[1];
        assert_eq!(
            memory
                .store
                .materialize_update_package(&binding.binding_id)
                .unwrap()
                .change_ids,
            vec![remaining.clone()]
        );
        let before = sequence(&memory, "SELECT max(sequence) FROM events");
        let packages = count(
            &memory.store.connection,
            "SELECT count(*) FROM update_packages",
        );
        for invalid in [
            vec![],
            selected,
            vec![remaining.clone(), remaining.clone()],
            vec![remaining.clone(), "missing".into()],
        ] {
            assert!(
                memory
                    .store
                    .materialize_selected_update_package_manifest(&binding.binding_id, &invalid)
                    .is_err()
            );
        }
        assert!(
            memory
                .store
                .materialize_selected_update_package_manifest(&"f".repeat(64), &[remaining.clone()])
                .is_err()
        );
        assert_eq!(
            sequence(&memory, "SELECT max(sequence) FROM events"),
            before
        );
        assert_eq!(
            count(
                &memory.store.connection,
                "SELECT count(*) FROM update_packages"
            ),
            packages
        );
        // A later explicit acknowledgment can reuse identical logical changes;
        // it cannot alter the receipts or infer membership outside this package.
        memory
            .store
            .acknowledge_update_package(&ack(&wide, "applied"), 1)
            .unwrap();
        assert_eq!(
            memory
                .store
                .acknowledge_update_package(&ack(&narrow.package, "applied"), 1)
                .unwrap(),
            applied
        );
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
        assert_eq!(moved, first.package_id);
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
        assert_eq!(
            memory
                .store
                .acknowledge_update_package(&ack(&first, "seen"), 1)
                .unwrap(),
            seen_first
        );
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
        assert_eq!(stayed, first.package_id);
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
    fn upgrade_preserves_package_receipts_without_inventing_worker_protocol_evidence() {
        let (_dir, mut memory) = coordinator();
        let latest = sequence(&memory, "SELECT max(sequence) FROM events");
        promote(&mut memory, "change-a", latest);
        let binding = binding(&mut memory);
        let package = memory
            .store
            .materialize_update_package(&binding.binding_id)
            .unwrap();
        let declaration = ack(&package, "seen");
        let before = memory
            .store
            .acknowledge_update_package(&declaration, 1)
            .unwrap();
        promote(&mut memory, "change-b", latest);
        let wider = memory
            .store
            .materialize_update_package(&binding.binding_id)
            .unwrap();
        let wider_seen = memory
            .store
            .acknowledge_update_package(&ack(&wider, "seen"), 1)
            .unwrap();
        let wider_applied = memory
            .store
            .acknowledge_update_package(&ack(&wider, "applied"), 1)
            .unwrap();
        crate::store::test_schema::historical(&memory.store.connection, 42).unwrap();
        // Schema 39-42 permitted application to retarget a logical seen row.
        // Preserve that historical source rather than inventing a new receipt
        // or treating the old package's original acceptance event as missing.
        memory.store.connection.execute(
            "UPDATE memory_change_receipts SET package_id=?1 WHERE binding_id=?2 AND disposition='seen'",
            params![wider.package_id,binding.binding_id],
        ).unwrap();
        let before_upgrade = sequence(&memory, "SELECT max(sequence) FROM events");
        memory.store.upgrade_v1().unwrap();
        assert_eq!(
            memory
                .store
                .acknowledge_update_package(&declaration, 1)
                .unwrap(),
            before
        );
        assert_eq!(
            memory
                .store
                .acknowledge_update_package(&ack(&wider, "seen"), 1)
                .unwrap(),
            wider_seen
        );
        assert_eq!(
            memory
                .store
                .acknowledge_update_package(&ack(&wider, "applied"), 1)
                .unwrap(),
            wider_applied
        );
        assert_eq!(
            sequence(&memory, "SELECT max(sequence) FROM events"),
            before_upgrade
        );
        assert_eq!(
            count(
                &memory.store.connection,
                "SELECT count(*) FROM memory_change_receipts"
            ),
            4
        );
        assert!(
            memory
                .store
                .connection
                .execute("UPDATE memory_change_receipts SET sequence=sequence", [])
                .is_err()
        );
        assert_eq!(
            count(
                &memory.store.connection,
                "SELECT count(*) FROM worker_package_acknowledgments"
            ),
            0
        );
        assert_eq!(
            count(
                &memory.store.connection,
                "SELECT count(*) FROM events WHERE kind='memory.worker_package_ack'"
            ),
            0
        );
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

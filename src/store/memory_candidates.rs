//! Manual edits are immutable candidates. Only signed control review changes heads.
use super::*;
use rusqlite::OptionalExtension;

fn invalid(message: &str) -> StoreError {
    StoreError::Invalid(message.into())
}
fn schema(db: &Connection) -> Result<()> {
    check_schema(db)?;
    let version: u32 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 23 {
        return Err(StoreError::UnsupportedSchema(version));
    }
    Ok(())
}
fn candidate(db: &Connection, id: &str) -> Result<Option<MemoryImportCandidate>> {
    let row = db.query_row("SELECT id,record_id,record_key,expected_revision,body_hash,provenance_hash,created_unix_ms FROM memory_import_candidates WHERE id=?1", [id], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,Option<u64>>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?,r.get::<_,i64>(6)?))).optional()?;
    row.map(|r| {
        Ok(MemoryImportCandidate {
            id: r.0,
            record_id: MemoryRecordId::new(r.1).map_err(StoreError::Corrupt)?,
            record_key: r.2,
            expected_revision: r.3,
            body_hash: ObjectId::from_hex(r.4).map_err(StoreError::Corrupt)?,
            provenance_hash: ObjectId::from_hex(r.5).map_err(StoreError::Corrupt)?,
            created_unix_ms: r.6,
        })
    })
    .transpose()
}
fn check_base(db: &Connection, c: &MemoryImportCandidate) -> Result<()> {
    let existing: Option<(String,u64,String)> = db.query_row("SELECT r.id,h.revision,h.status FROM memory_records r JOIN memory_heads h ON h.record_id=r.id WHERE r.scope_id='project' AND r.record_key=?1", [&c.record_key], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    match (existing, c.expected_revision) {
        (None, None) => Ok(()),
        (Some((id, rev, status)), Some(expected))
            if id == c.record_id.as_str() && rev == expected && status == "active" =>
        {
            Ok(())
        }
        _ => Err(StoreError::Conflict),
    }
}
impl SqliteStore {
    pub fn memory_import_candidate(&mut self, id: &str) -> Result<Option<MemoryImportCandidate>> {
        schema(&self.connection)?;
        candidate(&self.connection, id)
    }
    pub fn stage_memory_import(
        &mut self,
        c: &MemoryImportCandidate,
    ) -> Result<MemoryImportCandidate> {
        if c.id.len() > 128
            || c.record_key.len() > 512
            || c.id.is_empty()
            || c.record_key.is_empty()
        {
            return Err(invalid("invalid import candidate"));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema(&tx)?;
        // Check the supplied base even when the bytes or candidate ID are reused.
        check_base(&tx, c)?;
        if let Some(old) = candidate(&tx, &c.id)? {
            if old.record_id != c.record_id
                || old.record_key != c.record_key
                || old.expected_revision != c.expected_revision
                || old.body_hash != c.body_hash
                || old.provenance_hash != c.provenance_hash
            {
                return Err(invalid("candidate identity conflict"));
            }
            return Ok(old);
        }
        for id in [&c.body_hash, &c.provenance_hash] {
            objects::require_available(&tx, id.as_str())?;
            objects::cancel_pending(&tx, id.as_str())?;
        }
        tx.execute(
            "INSERT INTO memory_import_candidates VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                c.id,
                c.record_id.as_str(),
                c.record_key,
                c.expected_revision.map(integer).transpose()?,
                c.body_hash.as_str(),
                c.provenance_hash.as_str(),
                c.created_unix_ms
            ],
        )?;
        tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('memory.import_staged',?1,1,1,?2)",params![c.id,serde_json::json!({"candidate":c.id,"base":c.expected_revision}).to_string()])?;
        tx.commit()?;
        Ok(c.clone())
    }
    pub(crate) fn decide_memory_import(
        &mut self,
        prepared: &PreparedMemoryImportDecision,
    ) -> Result<MemoryImportReceipt> {
        let doc = &prepared.document;
        if doc.version != 1 || !matches!(doc.decision.as_str(), "approve" | "reject") {
            return Err(invalid("invalid import decision"));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema(&tx)?;
        let path = std::fs::canonicalize(
            tx.path()
                .ok_or_else(|| invalid("file-backed store required"))?,
        )
        .map_err(|_| invalid("store unavailable"))?;
        if path.to_str() != Some(doc.project_store.as_str()) {
            return Err(invalid("import decision belongs to another project"));
        }
        let encoded = serde_json::to_string(doc).map_err(|_| invalid("invalid import decision"))?;
        let digest = format!("{:x}", Sha256::digest(encoded.as_bytes()));
        if let Some((decision,old,seq,revision))=tx.query_row("SELECT decision,authorization_hash,sequence,resulting_revision FROM memory_import_decisions WHERE candidate_id=?1",[&doc.candidate_id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,u64>(2)?,r.get::<_,Option<u64>>(3)?))).optional()? {
            if old!=digest {return Err(invalid("candidate already reviewed under a different authorization"));}
            return Ok(MemoryImportReceipt{candidate_id:doc.candidate_id.clone(),decision,sequence:seq,resulting_revision:revision,reused:true});
        }
        if head(&tx)? != doc.expected_head
            || control::read(&tx)?.config_digest != prepared.config_digest
        {
            return Err(StoreError::Conflict);
        }
        let c = candidate(&tx, &doc.candidate_id)?.ok_or_else(|| invalid("candidate missing"))?;
        if c.body_hash != doc.body_hash || c.expected_revision != doc.expected_revision {
            return Err(invalid("decision does not cover candidate bytes/base"));
        }
        check_base(&tx, &c)?;
        let revision = if doc.decision == "approve" {
            let previous = memory::record(&tx, c.record_id.as_str())?;
            let (applicability, expiry_unix_ms, dependencies) = if let Some(base) =
                c.expected_revision
            {
                let (app,expiry):(String,Option<i64>)=tx.query_row("SELECT r.applicability,v.expiry_unix_ms FROM memory_revisions r JOIN memory_validity v ON v.record_id=r.record_id AND v.revision=r.revision WHERE r.record_id=?1 AND r.revision=?2",params![c.record_id.as_str(),integer(base)?],|r|Ok((r.get(0)?,r.get(1)?)))?;
                let mut statement=tx.prepare("SELECT source_record,source_revision,kind FROM memory_dependencies WHERE derived_record=?1 AND derived_revision=?2 ORDER BY source_record,source_revision,kind")?;
                let rows =
                    statement.query_map(params![c.record_id.as_str(), integer(base)?], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, u64>(1)?,
                            r.get::<_, String>(2)?,
                        ))
                    })?;
                let mut dependencies = Vec::new();
                for row in rows {
                    let (id, revision, kind) = row?;
                    dependencies.push((
                        MemoryRecordId::new(id).map_err(StoreError::Corrupt)?,
                        revision,
                        kind,
                    ));
                }
                (
                    serde_json::from_str(&app)
                        .map_err(|_| invalid("invalid base applicability"))?,
                    expiry,
                    dependencies,
                )
            } else {
                (
                    Applicability {
                        domains: vec![],
                        paths: vec![c.record_key.clone()],
                    },
                    None,
                    vec![],
                )
            };
            let next = NewRevision {
                id: c.record_id.clone(),
                record_key: c.record_key.clone(),
                scope_id: "project".into(),
                kind: previous.map(|r| r.kind).unwrap_or(MemoryKind::Observation),
                body_hash: c.body_hash,
                provenance_hash: c.provenance_hash,
                applicability,
                dependencies,
                expected: c.expected_revision,
                expiry_unix_ms,
                validity_state: "valid".into(),
                validity_reason: "control_import_review".into(),
            };
            {
                let (head, sequence) = memory::apply_memory_revision_in_tx(&tx, &next)?;
                super::memory_delivery::record_change(
                    &tx,
                    &doc.candidate_id,
                    head.record_id.as_str(),
                    head.revision,
                    "stop_at_checkpoint",
                    sequence,
                )?;
                Some(head.revision)
            }
        } else {
            None
        };
        tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('memory.import_reviewed',?1,1,1,?2)",params![doc.candidate_id,encoded])?;
        let sequence = head(&tx)?;
        tx.execute(
            "INSERT INTO memory_import_decisions VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                doc.candidate_id,
                doc.decision,
                encoded,
                digest,
                integer(sequence)?,
                revision.map(integer).transpose()?
            ],
        )?;
        tx.commit()?;
        Ok(MemoryImportReceipt {
            candidate_id: doc.candidate_id.clone(),
            decision: doc.decision.clone(),
            sequence,
            resulting_revision: revision,
            reused: false,
        })
    }
    pub fn memory_snapshot_inputs(&mut self, id: &str) -> Result<MemorySnapshotInputs> {
        schema(&self.connection)?;
        let row:Option<(String,String,String,String,String,String)>=self.connection.query_row("SELECT instructions,instruction_hash,request_json,request_hash,task_text,task_hash FROM memory_snapshot_inputs WHERE snapshot_id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional()?;
        let (instructions, instruction_hash, request, request_hash, task_text, task_hash) =
            row.ok_or_else(|| invalid("snapshot predates retained inputs; create a new snapshot"))?;
        let scope: String = self.connection.query_row(
            "SELECT scope_digest FROM memory_snapshots WHERE id=?1",
            [id],
            |r| r.get(0),
        )?;
        if format!("{:x}", Sha256::digest(task_text.as_bytes())) != task_hash
            || format!("{:x}", Sha256::digest(instructions.as_bytes())) != instruction_hash
            || format!("{:x}", Sha256::digest(request.as_bytes())) != request_hash
            || scope != request_hash
        {
            return Err(StoreError::Corrupt("snapshot input hash mismatch".into()));
        }
        Ok(MemorySnapshotInputs {
            snapshot_id: SnapshotId::new(id).map_err(StoreError::Corrupt)?,
            task_text,
            instructions,
            instruction_hash,
            request: serde_json::from_str(&request)
                .map_err(|_| StoreError::Corrupt("invalid retained snapshot request".into()))?,
        })
    }
}

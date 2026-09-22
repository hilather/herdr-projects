//! Owner-reviewed resolution of exact task invalidations, never automatic success.
use super::*;

impl SqliteStore {
    pub(crate) fn reconcile_memory(
        &mut self,
        prepared: &PreparedMemoryReconciliation,
        now: i64,
    ) -> Result<MemoryReconciliationReceipt> {
        let doc = &prepared.document;
        if doc.version != 1
            || doc.id.is_empty()
            || doc.id.len() > 128
            || !doc
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
            || doc.reason.trim().is_empty()
            || doc.reason.len() > 4096
            || doc.invalidations.is_empty()
            || doc.invalidations.len() > 256
        {
            return Err(StoreError::Invalid("invalid memory reconciliation".into()));
        }
        let mut ids = std::collections::BTreeSet::new();
        for item in &doc.invalidations {
            if !ids.insert(&item.id)
                || item.id.len() > 128
                || item.task_id.len() > 128
                || item.triggering_seq == 0
            {
                return Err(StoreError::Invalid(
                    "invalid or duplicate invalidation reference".into(),
                ));
            }
        }
        let payload = serde_json::to_string(doc).map_err(|e| StoreError::Invalid(e.to_string()))?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_schema(&tx)?;
        let version: u32 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 24 {
            return Err(StoreError::UnsupportedSchema(version));
        }
        let path = std::fs::canonicalize(
            tx.path()
                .ok_or_else(|| StoreError::Invalid("file-backed store required".into()))?,
        )
        .map_err(|e| StoreError::Invalid(e.to_string()))?;
        if path.to_str() != Some(doc.project_store.as_str())
            || control::read(&tx)?.config_digest != prepared.config_digest
        {
            return Err(StoreError::Conflict);
        }
        let prior: Option<(u64, String)> = tx
            .query_row(
                "SELECT sequence,payload FROM events WHERE kind='memory.reconciled' AND entity=?1",
                [&doc.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((sequence, old)) = prior {
            if old != payload {
                return Err(StoreError::Invalid(
                    "reconciliation id reused with different contents".into(),
                ));
            }
            return Ok(MemoryReconciliationReceipt {
                id: doc.id.clone(),
                sequence,
                resolved: doc.invalidations.iter().map(|i| i.id.clone()).collect(),
            });
        }
        if head(&tx)? != doc.expected_head || now >= doc.expires_unix_ms {
            return Err(StoreError::Conflict);
        }
        // Validate the complete list before making any change. A stale reference,
        // already-resolved item or wrong task rolls back the whole resolution.
        for item in &doc.invalidations {
            let matches:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM memory_invalidations WHERE id=?1 AND task_id=?2 AND triggering_seq=?3 AND resolved_seq IS NULL)",params![item.id,item.task_id,integer(item.triggering_seq)?],|r|r.get(0))?;
            if !matches {
                return Err(StoreError::Conflict);
            }
        }
        tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('memory.reconciled',?1,1,1,?2)",params![doc.id,payload])?;
        let sequence = head(&tx)?;
        for item in &doc.invalidations {
            tx.execute("UPDATE memory_invalidations SET resolved_seq=?2 WHERE id=?1 AND resolved_seq IS NULL",params![item.id,integer(sequence)?])?;
        }
        // Owner disposition covers these exact conflicts/updates only. It cannot
        // bless invalid knowledge, missing mandatory rules, or other pending work.
        let tasks: std::collections::BTreeSet<_> = doc
            .invalidations
            .iter()
            .map(|i| i.task_id.as_str())
            .collect();
        for task in tasks {
            let readiness = super::memory_barrier::report(&tx, task, now)?;
            if readiness.blockers.iter().any(|b| {
                !matches!(
                    b.kind.as_str(),
                    "unresolved_invalidation" | "required_update_unapplied"
                )
            }) {
                return Err(StoreError::Invalid(
                    "reconciliation requires current valid consumed knowledge and mandatory rules"
                        .into(),
                ));
            }
        }
        tx.commit()?;
        Ok(MemoryReconciliationReceipt {
            id: doc.id.clone(),
            sequence,
            resolved: doc.invalidations.iter().map(|i| i.id.clone()).collect(),
        })
    }

    pub fn memory_invalidations(&mut self, task: &str) -> Result<Vec<serde_json::Value>> {
        check_schema(&self.connection)?;
        let version: u32 = self
            .connection
            .query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 22 {
            return Err(StoreError::UnsupportedSchema(version));
        }
        let mut stmt=self.connection.prepare("SELECT id,task_id,proposal_id,record_id,severity,triggering_seq,resolved_seq,reason FROM memory_invalidations WHERE task_id=?1 OR task_id IS NULL ORDER BY triggering_seq,id LIMIT 10001")?;
        let rows=stmt.query_map([task],|r|Ok(serde_json::json!({"id":r.get::<_,String>(0)?,"task_id":r.get::<_,Option<String>>(1)?,"cause_id":r.get::<_,String>(2)?,"record_id":r.get::<_,Option<String>>(3)?,"severity":r.get::<_,String>(4)?,"triggering_seq":r.get::<_,u64>(5)?,"resolved_seq":r.get::<_,Option<u64>>(6)?,"reason":r.get::<_,String>(7)?})))?.collect::<std::result::Result<Vec<_>,_>>()?;
        if rows.len() > 10000 {
            return Err(StoreError::Limit(
                "memory invalidations exceed 10000".into(),
            ));
        }
        Ok(rows)
    }
}

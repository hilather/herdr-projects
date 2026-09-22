//! Creation intents retain path ownership even before Git returns a receipt.
use super::*;

pub(crate) fn read(
    path: &Path,
    publication: &Publication,
    budget: &mut Budget,
) -> Result<Vec<(String, WorktreePlan)>> {
    read_published(path, publication, budget, |tx, budget| {
        let version: u32 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 13 {
            let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM events WHERE kind IN ('runtime.worktrees_creation','runtime.worktrees_ready'))",[],|r|r.get(0))?;
            ensure!(
                !exists,
                "worktree references require launch approval schema"
            );
            return Ok(vec![]);
        }
        let orphan:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM events r LEFT JOIN events c ON c.entity=r.entity AND c.kind='runtime.worktrees_creation' WHERE r.kind='runtime.worktrees_ready' AND c.entity IS NULL)",[],|r|r.get(0))?;
        ensure!(!orphan, "worktree receipt lacks creation provenance");
        let mut statement=tx.prepare("SELECT e.entity,e.payload,i.payload,i.payload_hash,a.id,a.task_id,
            o.task_id,o.kind,o.target,o.expected_revision,o.payload,o.payload_hash,o.payload_version,o.idempotency_key,
            d.attempts,u.claim_revision,u.claim_epoch,u.consumed_unix_ms,g.payload,g.payload_hash,r.payload,
            e.revision,d.epoch,d.revision,b.id,b.task_id,t.id
            FROM events e
            LEFT JOIN attempt_inputs i ON i.operation_id=e.entity
            LEFT JOIN attempts a ON a.id=i.attempt_id
            LEFT JOIN tasks t ON t.id=a.task_id
            LEFT JOIN operations o ON o.id=i.operation_id
            LEFT JOIN runtime_bindings b ON b.id=o.target
            LEFT JOIN operation_delivery d ON d.operation_id=o.id
            LEFT JOIN approval_uses u ON u.operation_id=o.id
            LEFT JOIN approval_grants g ON g.id=u.approval_id
            LEFT JOIN events r ON r.entity=e.entity AND r.kind='runtime.worktrees_ready'
            WHERE e.kind='runtime.worktrees_creation'")?;
        let mut rows = statement.query([])?;
        let mut result = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        while let Some(row) = rows.next()? {
            budget.record()?;
            for column in [
                0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 11, 13, 18, 19, 20, 24, 25, 26,
            ] {
                let size = match row.get_ref(column)? {
                    rusqlite::types::ValueRef::Text(bytes) => bytes.len(),
                    rusqlite::types::ValueRef::Null if column == 20 => 0,
                    _ => anyhow::bail!("worktree reference lacks launch provenance"),
                };
                ensure!(
                    size <= MAX_RECORD_BYTES,
                    "worktree inventory field exceeds bounds"
                );
                budget.charge(size)?;
            }
            let entity: String = row.get(0)?;
            ensure!(
                seen.insert(entity.clone()),
                "duplicate worktree creation or receipt"
            );
            let payload: String = row.get(2)?;
            let digest: String = row.get(3)?;
            ensure!(
                format!("{:x}", Sha256::digest(payload.as_bytes())) == digest,
                "worktree launch input hash mismatch"
            );
            let record: AttemptInputRecord = serde_json::from_str(&payload)?;
            super::super::reservations::validate_inputs(&record.inputs)?;
            let (attempt, operation) = super::super::reservations::record_ids(&record.inputs)?;
            ensure!(
                record.attempt == attempt
                    && record.operation == operation
                    && record.operation.as_str() == entity
                    && record.inputs.project_store
                        == path
                            .to_str()
                            .ok_or_else(|| anyhow::anyhow!("worktree store path is not UTF-8"))?
                    && row.get::<_, String>(4)? == record.attempt.as_str()
                    && row.get::<_, String>(5)? == record.inputs.task.as_str()
                    && row.get::<_, String>(6)? == record.inputs.task.as_str()
                    && row.get::<_, String>(24)? == record.inputs.binding
                    && row.get::<_, String>(25)? == record.inputs.task.as_str()
                    && row.get::<_, String>(26)? == record.inputs.task.as_str()
                    && row.get::<_, String>(7)? == "runtime.launch"
                    && row.get::<_, String>(8)? == record.inputs.binding
                    && Some(row.get::<_, u64>(9)?) == record.inputs.task_revision.checked_add(1)
                    && row.get::<_, String>(10)? == payload
                    && row.get::<_, String>(11)? == digest
                    && row.get::<_, u32>(12)? == 1
                    && row.get::<_, String>(13)? == entity,
                "worktree launch operation provenance mismatch"
            );
            ensure!(
                row.get::<_, u64>(14)? == 1
                    && row.get::<_, u64>(15)? == row.get::<_, u64>(21)?
                    && row.get::<_, u64>(16)? > 0
                    && (row.get::<_, u64>(16)? == row.get::<_, u64>(22)?
                        || (row.get::<_, u64>(16)?.checked_add(1) == Some(row.get::<_, u64>(22)?)
                            && tx.query_row("SELECT EXISTS(SELECT 1 FROM events WHERE entity=?1 AND kind='operation.outcome' AND json_extract(payload,'$.actor')='lease-expiry' AND json_extract(payload,'$.epoch')=?2 AND json_extract(payload,'$.outcome.kind')='ambiguous' AND revision=?3)", rusqlite::params![entity, row.get::<_, u64>(22)?, row.get::<_, u64>(15)?.checked_add(1)], |r| r.get::<_, bool>(0))?))
                    && row.get::<_, u64>(23)? >= row.get::<_, u64>(15)?,
                "worktree creation claim provenance mismatch"
            );
            let grant_json: String = row.get(18)?;
            let grant_digest: String = row.get(19)?;
            ensure!(
                format!("{:x}", Sha256::digest(grant_json.as_bytes())) == grant_digest,
                "worktree approval hash mismatch"
            );
            let grant: ApprovalGrant = serde_json::from_str(&grant_json)?;
            ensure!(
                grant.reference().map_err(anyhow::Error::msg)? == record.inputs.approval,
                "worktree approval reference mismatch"
            );
            grant
                .matches_launch(&record.inputs, &record.inputs.project_store, row.get(17)?)
                .map_err(anyhow::Error::msg)?;
            let intent: WorktreeCreation = serde_json::from_str(&row.get::<_, String>(1)?)?;
            let plans =
                worktree_plans(&record.inputs, &record.attempt).map_err(anyhow::Error::msg)?;
            ensure!(
                intent.version == 1
                    && intent.operation == record.operation
                    && intent.attempt == record.attempt
                    && !plans.is_empty()
                    && intent.plans == plans
                    && intent.token.len() == 64
                    && intent
                        .token
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "worktree creation differs from approved resources"
            );
            if let Some(payload) = row.get::<_, Option<String>>(20)? {
                let receipts: Vec<WorktreeReceipt> = serde_json::from_str(&payload)?;
                ensure!(
                    receipts.len() == plans.len(),
                    "incomplete worktree ownership receipt"
                );
                let mut identities = std::collections::BTreeSet::new();
                for (receipt, plan) in receipts.iter().zip(&plans) {
                    ensure!(
                        &receipt.plan == plan
                            && [&receipt.git_directory, &receipt.common_directory]
                                .iter()
                                .all(|p| Path::new(p).is_absolute()
                                    && Path::new(p).components().all(|c| matches!(
                                        c,
                                        std::path::Component::RootDir
                                            | std::path::Component::Normal(_)
                                    )))
                            && [
                                &receipt.directory,
                                &receipt.git_identity,
                                &receipt.common_identity
                            ]
                            .iter()
                            .all(|i| i.inode > 0 && i.born_nanos < 1_000_000_000)
                            && identities
                                .insert((receipt.directory.device, receipt.directory.inode)),
                        "invalid retained worktree identity"
                    );
                }
            }
            for plan in plans {
                budget.record()?;
                budget.check()?;
                result.push((record.inputs.binding.clone(), plan));
            }
        }
        Ok(result)
    })
}

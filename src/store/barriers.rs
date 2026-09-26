//! Frozen wave membership. Release calls `memory_barrier::enforce` per member.
//! It does not keep a second copy of the mandatory-head check.
use super::*;
use rusqlite::OptionalExtension;
use serde::Serialize;

const SCHEMA_VERSION: u32 = 40;
const MAX_MEMBERS: usize = 1_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProposalDisposition {
    pub proposal_id: String,
    pub disposition: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BarrierMember {
    pub task_id: String,
    pub contract_revision: u64,
    pub attempt_id: String,
    pub result_id: String,
    pub verification_id: String,
    pub integration_id: Option<String>,
    pub proposal_dispositions: Vec<ProposalDisposition>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FrozenBarrier {
    pub barrier_id: String,
    pub required_set_generation: u64,
    pub memory_manifest_digest: String,
    pub release_token: String,
    pub released_seq: Option<u64>,
    pub revoked_seq: Option<u64>,
    pub members: Vec<BarrierMember>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct StaleBrief {
    pub brief_id: String,
    pub barrier_id: String,
    pub attempt_id: String,
    pub sequence: u64,
    pub accepted: bool,
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

fn digest_json(value: &serde_json::Value) -> Result<String> {
    let bytes = serde_json::to_vec(value).map_err(|error| invalid(&error.to_string()))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn non_negative(value: i64, what: &str) -> Result<u64> {
    u64::try_from(value).map_err(|_| invalid(what))
}

fn identifier(value: &str, what: &str) -> Result<()> {
    if TaskId::new(value).is_err() {
        return Err(invalid(what));
    }
    Ok(())
}

fn insert_event(
    db: &Connection,
    kind: &str,
    entity: &str,
    payload: &serde_json::Value,
) -> Result<i64> {
    let text = serde_json::to_string(payload).map_err(|error| invalid(&error.to_string()))?;
    db.execute(
        "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES(?1,?2,1,1,?3)",
        params![kind, entity, text],
    )?;
    db.query_row("SELECT last_insert_rowid()", [], |row| row.get(0))
        .map_err(StoreError::from)
}

fn required_generation(db: &Connection) -> Result<u64> {
    let generation: i64 = db
        .query_row(
            "SELECT generation FROM memory_required_generation WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| StoreError::Corrupt("required set generation is missing".into()))?;
    non_negative(generation, "required set generation")
}

fn canonical_dispositions(member: &BarrierMember) -> Result<Vec<(String, String)>> {
    let mut listed = Vec::with_capacity(member.proposal_dispositions.len());
    for item in &member.proposal_dispositions {
        identifier(&item.proposal_id, "proposal id is invalid")?;
        if !matches!(
            item.disposition.as_str(),
            "promoted" | "rejected" | "deferred"
        ) {
            return Err(invalid("proposal disposition is invalid"));
        }
        if listed.iter().any(|(id, _)| id == &item.proposal_id) {
            return Err(invalid("proposal disposition is duplicated"));
        }
        listed.push((item.proposal_id.clone(), item.disposition.clone()));
    }
    listed.sort();
    if serde_json::to_string(&listed)
        .map_err(|error| invalid(&error.to_string()))?
        .len()
        > 65536
    {
        return Err(StoreError::Limit(
            "proposal dispositions exceed 65536 bytes".into(),
        ));
    }
    Ok(listed)
}

fn dispositions_json(listed: &[(String, String)]) -> Result<String> {
    serde_json::to_string(listed).map_err(|error| invalid(&error.to_string()))
}

fn stored_disposition(db: &Connection, proposal_id: &str) -> Result<Option<String>> {
    let row: Option<(String, bool, bool)> = db
        .query_row(
            "SELECT p.review_state,
                    EXISTS(SELECT 1 FROM memory_promotions m WHERE m.proposal_id=p.id),
                    EXISTS(SELECT 1 FROM review_decisions d WHERE d.proposal_id=p.id AND d.decision='reject')
             FROM memory_proposals p WHERE p.id=?1",
            [proposal_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((review_state, promoted, rejected)) = row else {
        return Ok(None);
    };
    let disposition = if promoted {
        "promoted"
    } else if rejected || review_state == "rejected" {
        "rejected"
    } else {
        "deferred"
    };
    Ok(Some(disposition.into()))
}

fn dispositions_match(
    db: &Connection,
    member: &BarrierMember,
    listed: &[(String, String)],
) -> Result<()> {
    let mut stmt = db.prepare(
        "SELECT id FROM memory_proposals WHERE task_id=?1 AND attempt_id=?2 ORDER BY id",
    )?;
    let stored = stmt
        .query_map(params![member.task_id, member.attempt_id], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if stored.len() > MAX_MEMBERS {
        return Err(StoreError::Limit("proposals exceed 1000".into()));
    }
    let mut expected = Vec::with_capacity(stored.len());
    for proposal_id in &stored {
        let disposition = stored_disposition(db, proposal_id)?
            .ok_or_else(|| StoreError::Corrupt("proposal disposition disappeared".into()))?;
        expected.push((proposal_id.clone(), disposition));
    }
    if listed != expected.as_slice() {
        return Err(invalid(
            "proposal disposition does not match stored membership",
        ));
    }
    Ok(())
}

fn evidence_matches(db: &Connection, member: &BarrierMember) -> Result<()> {
    identifier(&member.task_id, "task id is invalid")?;
    identifier(&member.attempt_id, "attempt id is invalid")?;
    if member.contract_revision == 0 {
        return Err(invalid("contract revision is invalid"));
    }
    let contract_revision = i64::try_from(member.contract_revision)
        .map_err(|_| invalid("contract revision is invalid"))?;
    if !hex64(&member.result_id) || !hex64(&member.verification_id) {
        return Err(invalid("result or verification id is invalid"));
    }
    if member.integration_id.as_ref().is_some_and(|id| !hex64(id)) {
        return Err(invalid("integration id is invalid"));
    }
    let route: String = db
        .query_row(
            "SELECT route FROM task_contracts WHERE task_id=?1 AND contract_revision=?2",
            params![member.task_id, contract_revision],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| invalid("task contract is not stored"))?;
    let attempt_exists: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM attempts WHERE id=?1 AND task_id=?2)",
        params![member.attempt_id, member.task_id],
        |row| row.get(0),
    )?;
    if !attempt_exists {
        return Err(invalid("attempt is not stored"));
    }
    let linked: Option<(String, String, String, i64, String)> = db
        .query_row(
            "SELECT r.run_id, r.task_id, r.attempt_id, r.contract_revision, r.state
             FROM verified_results v JOIN verification_runs r ON r.run_id=v.run_id
             WHERE v.result_id=?1",
            [&member.result_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    let Some((run_id, task_id, attempt_id, stored_revision, state)) = linked else {
        return Err(invalid("result is not a stored verified result"));
    };
    if run_id != member.verification_id
        || task_id != member.task_id
        || attempt_id != member.attempt_id
        || stored_revision != contract_revision
        || state != "accepted"
    {
        return Err(invalid("verification does not match result"));
    }
    match (route.as_str(), member.integration_id.as_deref()) {
        ("verify_only", None) => {}
        ("verify_only", Some(_)) => return Err(invalid("integration is not required")),
        ("verify_then_integrate", None) => return Err(invalid("integration is required")),
        ("verify_then_integrate", Some(integration_id)) => {
            let verified: Option<String> = db
                .query_row(
                    "SELECT o.verified_result_id
                     FROM integrated_commits i
                     JOIN integration_operations o ON o.operation_id=i.operation_id
                     WHERE i.integrated_id=?1",
                    [integration_id],
                    |row| row.get(0),
                )
                .optional()?;
            if verified.as_deref() != Some(member.result_id.as_str()) {
                return Err(invalid("integration does not match result"));
            }
        }
        _ => return Err(invalid("task contract route is invalid")),
    }
    let active: Option<String> = db.query_row(
        "SELECT active_attempt FROM tasks WHERE id=?1",
        [&member.task_id],
        |row| row.get(0),
    )?;
    if active.as_deref() != Some(member.attempt_id.as_str()) {
        return Err(invalid("member attempt is not active"));
    }
    Ok(())
}

fn memory_manifest(db: &Connection, generation: u64, members: &[BarrierMember]) -> Result<String> {
    let mut stmt = db.prepare(
        "SELECT h.record_id, h.revision
         FROM memory_heads h JOIN memory_records r ON r.id=h.record_id
         WHERE h.status='active' AND (r.is_hard=1 OR r.kind IN ('constraint','hard_memory'))
         ORDER BY h.record_id",
    )?;
    let heads = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if heads.len() > 10_000 {
        return Err(StoreError::Limit("mandatory heads exceed 10000".into()));
    }
    let mut snapshots = Vec::with_capacity(members.len());
    for member in members {
        let manifest: Option<String> = db.query_row(
            "SELECT s.manifest_hash
             FROM attempts a
             LEFT JOIN memory_snapshots s ON s.id=a.snapshot AND s.task_id=a.task_id
             WHERE a.id=?1 AND a.task_id=?2",
            params![member.attempt_id, member.task_id],
            |row| row.get(0),
        )?;
        snapshots.push(serde_json::json!([member.task_id, manifest]));
    }
    let head_json: Vec<_> = heads
        .into_iter()
        .map(|(record, revision)| serde_json::json!([record, revision]))
        .collect();
    digest_json(&serde_json::json!([
        "barrier-memory-v1",
        generation,
        head_json,
        snapshots
    ]))
}

fn canonical_members(
    db: &Connection,
    members: &[BarrierMember],
) -> Result<Vec<(BarrierMember, String)>> {
    if members.is_empty() {
        return Err(invalid("barrier membership is empty"));
    }
    if members.len() > MAX_MEMBERS {
        return Err(StoreError::Limit("barrier membership exceeds 1000".into()));
    }
    let mut ordered = members.to_vec();
    ordered.sort_by(|left, right| left.task_id.cmp(&right.task_id));
    if ordered
        .windows(2)
        .any(|pair| pair[0].task_id == pair[1].task_id)
    {
        return Err(invalid("barrier membership repeats a task"));
    }
    let mut canonical = Vec::with_capacity(ordered.len());
    for member in ordered {
        evidence_matches(db, &member)?;
        let listed = canonical_dispositions(&member)?;
        dispositions_match(db, &member, &listed)?;
        let json = dispositions_json(&listed)?;
        canonical.push((member, json));
    }
    Ok(canonical)
}

fn identity(
    generation: u64,
    manifest: &str,
    members: &[(BarrierMember, String)],
) -> Result<String> {
    let listed: Vec<_> = members
        .iter()
        .map(|(member, dispositions)| {
            serde_json::json!([
                member.task_id,
                member.contract_revision,
                member.attempt_id,
                member.result_id,
                member.verification_id,
                member.integration_id,
                dispositions
            ])
        })
        .collect();
    digest_json(&serde_json::json!([
        "barrier-v1",
        generation,
        manifest,
        listed
    ]))
}

fn release_token(barrier_id: &str) -> Result<String> {
    digest_json(&serde_json::json!(["barrier-release-v1", barrier_id]))
}

fn load(db: &Connection, barrier_id: &str) -> Result<Option<FrozenBarrier>> {
    let mut header = db.prepare(
        "SELECT barrier_id, required_set_generation, memory_manifest_digest, release_token, released_seq, revoked_seq
         FROM barrier_revisions WHERE barrier_id=?1",
    )?;
    let mut headers = header.query([barrier_id])?;
    let Some(row) = headers.next()? else {
        return Ok(None);
    };
    let barrier_id: String = row.get(0)?;
    let generation: i64 = row.get(1)?;
    let manifest: String = row.get(2)?;
    let token: String = row.get(3)?;
    let released: Option<i64> = row.get(4)?;
    let revoked: Option<i64> = row.get(5)?;
    drop(headers);
    drop(header);
    let mut stmt = db.prepare(
        "SELECT task_id, contract_revision, attempt_id, result_id, verification_id, integration_id, proposal_dispositions
         FROM barrier_members WHERE barrier_id=?1 ORDER BY position",
    )?;
    let rows = stmt
        .query_map([&barrier_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, String>(6)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut members = Vec::with_capacity(rows.len());
    for (
        task_id,
        contract_revision,
        attempt_id,
        result_id,
        verification_id,
        integration_id,
        dispositions,
    ) in rows
    {
        let parsed: Vec<(String, String)> = serde_json::from_str(&dispositions)
            .map_err(|error| StoreError::Corrupt(error.to_string()))?;
        members.push(BarrierMember {
            task_id,
            contract_revision: non_negative(contract_revision, "stored contract revision")?,
            attempt_id,
            result_id,
            verification_id,
            integration_id,
            proposal_dispositions: parsed
                .into_iter()
                .map(|(proposal_id, disposition)| ProposalDisposition {
                    proposal_id,
                    disposition,
                })
                .collect(),
        });
    }
    Ok(Some(FrozenBarrier {
        barrier_id,
        required_set_generation: non_negative(generation, "stored required set generation")?,
        memory_manifest_digest: manifest,
        release_token: token,
        released_seq: released
            .map(|value| non_negative(value, "stored release"))
            .transpose()?,
        revoked_seq: revoked
            .map(|value| non_negative(value, "stored revocation"))
            .transpose()?,
        members,
    }))
}

fn recheck_ready(db: &Connection, barrier: &FrozenBarrier, now: i64) -> Result<()> {
    // Live generation, not the value captured when the caller built the request.
    if required_generation(db)? != barrier.required_set_generation {
        return Err(invalid("required set generation moved"));
    }
    for member in &barrier.members {
        evidence_matches(db, member)?;
        let listed = canonical_dispositions(member)?;
        dispositions_match(db, member, &listed)?;
        if listed
            .iter()
            .any(|(_, disposition)| disposition == "deferred")
        {
            return Err(invalid("deferred proposal blocks release"));
        }
        // Mandatory-head coverage stays in the per-task check.
        memory_barrier::enforce(db, &member.task_id, now)?;
    }
    Ok(())
}

impl SqliteStore {
    pub(crate) fn freeze_barrier(
        &mut self,
        members: &[BarrierMember],
        expected_head: u64,
    ) -> Result<FrozenBarrier> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_schema(&tx)?;
        let canonical = canonical_members(&tx, members)?;
        let generation = required_generation(&tx)?;
        let prepared: Vec<BarrierMember> =
            canonical.iter().map(|(member, _)| member.clone()).collect();
        let manifest = memory_manifest(&tx, generation, &prepared)?;
        let barrier_id = identity(generation, &manifest, &canonical)?;
        if let Some(existing) = load(&tx, &barrier_id)? {
            return Ok(existing);
        }
        if head(&tx)? != expected_head {
            return Err(StoreError::Conflict);
        }
        let token = release_token(&barrier_id)?;
        let created = insert_event(
            &tx,
            "barrier.frozen",
            &barrier_id,
            &serde_json::json!({"generation": generation, "members": prepared.len()}),
        )?;
        tx.execute(
            "INSERT INTO barrier_revisions(barrier_id,required_set_generation,memory_manifest_digest,release_token,released_seq,revoked_seq,created_seq)
             VALUES(?1,?2,?3,?4,NULL,NULL,?5)",
            params![barrier_id, i64::try_from(generation).map_err(|_| invalid("required set generation"))?, manifest, token, created],
        )?;
        for (position, (member, dispositions)) in canonical.iter().enumerate() {
            tx.execute(
                "INSERT INTO barrier_members(barrier_id,position,task_id,contract_revision,attempt_id,result_id,verification_id,integration_id,proposal_dispositions)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![
                    barrier_id,
                    i64::try_from(position).map_err(|_| invalid("barrier position"))?,
                    member.task_id,
                    i64::try_from(member.contract_revision).map_err(|_| invalid("contract revision is invalid"))?,
                    member.attempt_id,
                    member.result_id,
                    member.verification_id,
                    member.integration_id,
                    dispositions
                ],
            )?;
        }
        let stored = load(&tx, &barrier_id)?
            .ok_or_else(|| StoreError::Corrupt("frozen barrier is missing".into()))?;
        tx.commit()?;
        Ok(stored)
    }

    pub(crate) fn release_barrier(
        &mut self,
        barrier_id: &str,
        token: &str,
        expected_head: u64,
        now: i64,
    ) -> Result<FrozenBarrier> {
        if !hex64(barrier_id) || !hex64(token) {
            return Err(invalid("release token does not match barrier"));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_schema(&tx)?;
        let barrier = load(&tx, barrier_id)?.ok_or_else(|| invalid("barrier is not stored"))?;
        if barrier.release_token != token {
            return Err(invalid("release token does not match barrier"));
        }
        if barrier.released_seq.is_some() {
            return Ok(barrier);
        }
        // Revocation wins inside this transaction. It does not free capacity.
        if barrier.revoked_seq.is_some() {
            return Err(invalid("barrier is revoked"));
        }
        if head(&tx)? != expected_head {
            return Err(StoreError::Conflict);
        }
        recheck_ready(&tx, &barrier, now)?;
        let sequence = insert_event(
            &tx,
            "barrier.released",
            barrier_id,
            &serde_json::json!({"token": token}),
        )?;
        let updated = tx.execute(
            "UPDATE barrier_revisions SET released_seq=?2 WHERE barrier_id=?1 AND released_seq IS NULL AND revoked_seq IS NULL",
            params![barrier_id, sequence],
        )?;
        if updated != 1 {
            return Err(StoreError::Conflict);
        }
        let stored = load(&tx, barrier_id)?
            .ok_or_else(|| StoreError::Corrupt("released barrier is missing".into()))?;
        tx.commit()?;
        Ok(stored)
    }

    pub(crate) fn revoke_barrier(
        &mut self,
        barrier_id: &str,
        expected_head: u64,
    ) -> Result<FrozenBarrier> {
        if !hex64(barrier_id) {
            return Err(invalid("barrier id is invalid"));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_schema(&tx)?;
        let barrier = load(&tx, barrier_id)?.ok_or_else(|| invalid("barrier is not stored"))?;
        if barrier.revoked_seq.is_some() {
            return Ok(barrier);
        }
        if barrier.released_seq.is_some() {
            return Err(invalid("released barrier cannot be revoked"));
        }
        if head(&tx)? != expected_head {
            return Err(StoreError::Conflict);
        }
        // Dependents lose valid evidence. The attempt row stays so its slot
        // remains held until termination_observed.
        tx.execute(
            "UPDATE dependency_satisfactions SET state='invalid'
             WHERE state='valid' AND predecessor_task IN (SELECT task_id FROM barrier_members WHERE barrier_id=?1)",
            [barrier_id],
        )?;
        let sequence = insert_event(&tx, "barrier.revoked", barrier_id, &serde_json::json!({}))?;
        let updated = tx.execute(
            "UPDATE barrier_revisions SET revoked_seq=?2 WHERE barrier_id=?1 AND revoked_seq IS NULL AND released_seq IS NULL",
            params![barrier_id, sequence],
        )?;
        if updated != 1 {
            return Err(StoreError::Conflict);
        }
        let stored = load(&tx, barrier_id)?
            .ok_or_else(|| StoreError::Corrupt("revoked barrier is missing".into()))?;
        tx.commit()?;
        Ok(stored)
    }

    pub(crate) fn record_stale_brief(
        &mut self,
        barrier_id: &str,
        attempt_id: &str,
        payload: &str,
        expected_head: u64,
    ) -> Result<StaleBrief> {
        if !hex64(barrier_id)
            || TaskId::new(attempt_id).is_err()
            || payload.is_empty()
            || payload.len() > 65536
        {
            return Err(invalid("stale brief is invalid"));
        }
        let brief_id = digest_json(&serde_json::json!([
            "barrier-stale-brief-v1",
            barrier_id,
            attempt_id,
            payload
        ]))?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_schema(&tx)?;
        if let Some(existing) = load_brief(&tx, &brief_id)? {
            return Ok(existing);
        }
        let barrier = load(&tx, barrier_id)?.ok_or_else(|| invalid("barrier is not stored"))?;
        if barrier.revoked_seq.is_none() {
            return Err(invalid("barrier is not revoked"));
        }
        if !barrier
            .members
            .iter()
            .any(|member| member.attempt_id == attempt_id)
        {
            return Err(invalid("stale brief attempt is not a member"));
        }
        if head(&tx)? != expected_head {
            return Err(StoreError::Conflict);
        }
        let sequence = insert_event(
            &tx,
            "barrier.stale_brief",
            &brief_id,
            &serde_json::json!({"barrier": barrier_id, "attempt": attempt_id}),
        )?;
        tx.execute(
            "INSERT INTO barrier_stale_briefs(brief_id,barrier_id,attempt_id,payload,recorded_seq,accepted) VALUES(?1,?2,?3,?4,?5,0)",
            params![brief_id, barrier_id, attempt_id, payload, sequence],
        )?;
        let stored = load_brief(&tx, &brief_id)?
            .ok_or_else(|| StoreError::Corrupt("stale brief is missing".into()))?;
        tx.commit()?;
        Ok(stored)
    }

    pub(crate) fn accept_stale_brief(&mut self, brief_id: &str) -> Result<()> {
        if !hex64(brief_id) {
            return Err(invalid("stale brief is invalid"));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_schema(&tx)?;
        let recorded =
            load_brief(&tx, brief_id)?.ok_or_else(|| invalid("stale brief is not recorded"))?;
        if recorded.accepted {
            return Err(StoreError::Corrupt("stale brief was accepted".into()));
        }
        Err(invalid("stale brief cannot be accepted"))
    }
}

fn load_brief(db: &Connection, brief_id: &str) -> Result<Option<StaleBrief>> {
    db.query_row(
        "SELECT brief_id, barrier_id, attempt_id, recorded_seq, accepted FROM barrier_stale_briefs WHERE brief_id=?1",
        [brief_id],
        |row| {
            Ok(StaleBrief {
                brief_id: row.get(0)?,
                barrier_id: row.get(1)?,
                attempt_id: row.get(2)?,
                sequence: row.get::<_, i64>(3)? as u64,
                accepted: row.get::<_, i64>(4)? != 0,
            })
        },
    )
    .optional()
    .map_err(StoreError::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_version(db: &Connection) -> u32 {
        db.query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }

    fn head_of(db: &SqliteStore) -> u64 {
        db.connection
            .query_row("SELECT coalesce(max(sequence),0) FROM events", [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    fn digest() -> String {
        "11".repeat(32)
    }

    #[derive(Clone)]
    struct Seeded {
        task: String,
        attempt: String,
        result: String,
        verification: String,
        integration: Option<String>,
    }

    fn member_of(seeded: &Seeded, dispositions: Vec<ProposalDisposition>) -> BarrierMember {
        BarrierMember {
            task_id: seeded.task.clone(),
            contract_revision: 1,
            attempt_id: seeded.attempt.clone(),
            result_id: seeded.result.clone(),
            verification_id: seeded.verification.clone(),
            integration_id: seeded.integration.clone(),
            proposal_dispositions: dispositions,
        }
    }

    fn seed(db: &Connection, task: &str, route: &str) -> Seeded {
        let seq: i64 = db
            .query_row("SELECT coalesce(max(sequence),0) FROM events", [], |row| {
                row.get(0)
            })
            .unwrap();
        let event = if seq == 0 {
            db.execute(
                "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('fixture',?1,1,1,'{}')",
                [task],
            )
            .unwrap();
            db.query_row("SELECT last_insert_rowid()", [], |row| row.get(0))
                .unwrap()
        } else {
            seq
        };
        let _ = event;
        let installed: i64 = db
            .query_row("SELECT coalesce(max(sequence),0) FROM events", [], |row| {
                row.get(0)
            })
            .unwrap();
        db.execute(
            "INSERT INTO tasks(id,revision,state,title,active_attempt) VALUES(?1,1,'running',?1,NULL)",
            [task],
        )
        .unwrap();
        let attempt = format!("attempt-{task}");
        let snapshot = format!("snap-{task}");
        let hash = digest();
        db.execute(
            "INSERT INTO memory_snapshots(id,task_id,task_revision,profile_name,profile_digest,config_digest,selection_policy_version,estimator,sequence,required_bytes,optional_bytes,budget_bytes,omitted_optional_count,manifest_hash,scope_digest)
             VALUES(?1,?2,1,'worker',?3,NULL,1,'test',1,0,0,0,0,?3,?3)",
            params![snapshot, task, hash],
        )
        .unwrap();
        db.execute(
            "INSERT INTO attempts(id,task_id,revision,state,snapshot,reservation,termination_observed) VALUES(?1,?2,1,'running',?3,?4,0)",
            params![attempt, task, snapshot, format!("slot-{task}")],
        )
        .unwrap();
        db.execute(
            "UPDATE tasks SET active_attempt=?2 WHERE id=?1",
            params![task, attempt],
        )
        .unwrap();
        db.execute(
            "INSERT INTO task_contracts(task_id,contract_revision,plan_revision,project_store,expected_head,repository,base_oid,object_format,memory_snapshot_id,route,raw_bytes,raw_digest,installed_seq)
             VALUES(?1,1,NULL,'/tmp/project',0,'/tmp/repo',?2,'sha1',NULL,?3,x'61',?4,?5)",
            params![task, "b".repeat(40), route, hash, installed],
        )
        .unwrap();
        db.execute(
            "INSERT INTO acceptance_policies(task_id,contract_revision,policy_id,body) VALUES(?1,1,'policy-1','{}')",
            [task],
        )
        .unwrap();
        let submission = format!(
            "{:x}",
            Sha256::digest(format!("submission-{task}").as_bytes())
        );
        let result = format!("{:x}", Sha256::digest(format!("result-{task}").as_bytes()));
        let verification = format!("{:x}", Sha256::digest(format!("run-{task}").as_bytes()));
        db.execute(
            "INSERT INTO result_submissions(submission_id,project_store,idempotency_key,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,memory_snapshot_id,artifact_manifest,claimed_checks,created_unix_ms)
             VALUES(?1,'/tmp/project',?2,?3,'{}',?4,1,?3,?5,'/tmp/repo',?6,?6,'sha1',NULL,'[]','[]',1)",
            params![submission, format!("submit-{task}"), hash, task, attempt, "b".repeat(40)],
        )
        .unwrap();
        db.execute(
            "INSERT INTO verification_runs(run_id,project_store,idempotency_key,payload_digest,submission_id,task_id,contract_revision,contract_digest,attempt_id,policy_id,policy_digest,commit_oid,tree_oid,object_format,memory_fence,isolation,argv,library_manifest,state,reason,exit_status,receipt_digest,store_device,store_inode,created_unix_ms)
             VALUES(?1,'/tmp/project',?2,?3,?4,?5,1,?3,?6,'policy-1',?3,?7,?7,'sha1',0,'linux-unshare-user-pid-mount-v1','[]','[]','accepted',NULL,0,?3,0,0,1)",
            params![verification, format!("verify-{task}"), hash, submission, task, attempt, "c".repeat(40)],
        )
        .unwrap();
        db.execute(
            "INSERT INTO verified_results(result_id,run_id,submission_id,commit_oid,tree_oid,object_format,policy_digest,receipt_digest,isolation,memory_fence,created_unix_ms)
             VALUES(?1,?2,?3,?4,?4,'sha1',?5,?5,'linux-unshare-user-pid-mount-v1',0,1)",
            params![result, verification, submission, "c".repeat(40), hash],
        )
        .unwrap();
        Seeded {
            task: task.into(),
            attempt,
            result,
            verification,
            integration: None,
        }
    }

    fn add_result(db: &Connection, seeded: &Seeded, label: &str) -> (String, String) {
        let hash = digest();
        let submission = format!(
            "{:x}",
            Sha256::digest(format!("submission-{label}").as_bytes())
        );
        let result = format!("{:x}", Sha256::digest(format!("result-{label}").as_bytes()));
        let verification = format!("{:x}", Sha256::digest(format!("run-{label}").as_bytes()));
        db.execute(
            "INSERT INTO result_submissions(submission_id,project_store,idempotency_key,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,memory_snapshot_id,artifact_manifest,claimed_checks,created_unix_ms)
             VALUES(?1,'/tmp/project',?2,?3,'{}',?4,1,?3,?5,'/tmp/repo',?6,?6,'sha1',NULL,'[]','[]',1)",
            params![submission, format!("submit-{label}"), hash, seeded.task, seeded.attempt, "b".repeat(40)],
        )
        .unwrap();
        db.execute(
            "INSERT INTO verification_runs(run_id,project_store,idempotency_key,payload_digest,submission_id,task_id,contract_revision,contract_digest,attempt_id,policy_id,policy_digest,commit_oid,tree_oid,object_format,memory_fence,isolation,argv,library_manifest,state,reason,exit_status,receipt_digest,store_device,store_inode,created_unix_ms)
             VALUES(?1,'/tmp/project',?2,?3,?4,?5,1,?3,?6,'policy-1',?3,?7,?7,'sha1',0,'linux-unshare-user-pid-mount-v1','[]','[]','accepted',NULL,0,?3,0,0,1)",
            params![verification, format!("verify-{label}"), hash, submission, seeded.task, seeded.attempt, "c".repeat(40)],
        )
        .unwrap();
        db.execute(
            "INSERT INTO verified_results(result_id,run_id,submission_id,commit_oid,tree_oid,object_format,policy_digest,receipt_digest,isolation,memory_fence,created_unix_ms)
             VALUES(?1,?2,?3,?4,?4,'sha1',?5,?5,'linux-unshare-user-pid-mount-v1',0,1)",
            params![result, verification, submission, "c".repeat(40), hash],
        )
        .unwrap();
        (result, verification)
    }

    fn add_integration(db: &Connection, seeded: &Seeded, label: &str) -> String {
        let hash = digest();
        let operation = format!("op-{label}");
        db.execute(
            "INSERT INTO operations(id,task_id,kind,target,payload_version,payload,payload_hash,expected_revision,due_unix_ms,idempotency_key)
             VALUES(?1,?2,'integration','refs/heads/main',1,'{}',?3,1,0,?4)",
            params![operation, seeded.task, hash, format!("idem-{label}")],
        )
        .unwrap();
        db.execute(
            "INSERT INTO integration_targets(repository,ref_name,created_unix_ms) VALUES('/tmp/repo','refs/heads/main',1) ON CONFLICT(repository) DO NOTHING",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO integration_target_leases(repository,ref_name,operation_id,generation) VALUES('/tmp/repo','refs/heads/main',NULL,0) ON CONFLICT(repository,ref_name) DO NOTHING",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO integration_operations(operation_id,project_store,idempotency_key,payload_digest,repository,ref_name,expected_old_oid,verified_result_id,candidate_id,state,generation,object_format,checks_passed,reason,created_unix_ms)
             VALUES(?1,'/tmp/project',?2,?3,'/tmp/repo','refs/heads/main',?4,?5,NULL,'integrated',1,'sha1',1,NULL,1)",
            params![operation, format!("int-{label}"), hash, "b".repeat(40), seeded.result],
        )
        .unwrap();
        let candidate = format!(
            "{:x}",
            Sha256::digest(format!("candidate-{label}").as_bytes())
        );
        let integrated = format!(
            "{:x}",
            Sha256::digest(format!("integrated-{label}").as_bytes())
        );
        db.execute(
            "INSERT INTO integration_candidates(candidate_id,operation_id,commit_oid,tree_oid,parent_base,parent_verified,strategy,object_format,state,created_unix_ms)
             VALUES(?1,?2,?3,?3,?3,?3,'ort','sha1','published',1)",
            params![candidate, operation, "c".repeat(40)],
        )
        .unwrap();
        db.execute(
            "INSERT INTO integrated_commits(integrated_id,candidate_id,operation_id,repository,ref_name,commit_oid,tree_oid,expected_old_oid,object_format,created_unix_ms)
             VALUES(?1,?2,?3,'/tmp/repo','refs/heads/main',?4,?4,?4,'sha1',1)",
            params![integrated, candidate, operation, "c".repeat(40)],
        )
        .unwrap();
        integrated
    }

    fn hard_head(db: &Connection) {
        db.execute(
            "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('fixture','hard',1,1,'{}')",
            [],
        )
        .unwrap();
        let seq: i64 = db
            .query_row("SELECT last_insert_rowid()", [], |row| row.get(0))
            .unwrap();
        let hash = digest();
        db.execute(
            "INSERT INTO objects(hash,size,availability,collection,pin_count,fencing_token) VALUES(?1,1,'available','unclaimed',0,0)",
            [&hash],
        )
        .unwrap();
        db.execute(
            "INSERT INTO memory_records(id,record_key,scope_id,kind,is_hard) VALUES('rule','rule','project','hard_memory',1)",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO memory_revisions(record_id,revision,body_hash,provenance_hash,promoted_seq,applicability) VALUES('rule',1,?1,?1,?2,'{\"domains\":[],\"paths\":[]}')",
            params![hash, seq],
        )
        .unwrap();
        db.execute(
            "INSERT INTO memory_validity(record_id,revision,state,reason,expiry_unix_ms,evaluated_seq) VALUES('rule',1,'valid','fixture',NULL,?1)",
            [seq],
        )
        .unwrap();
        db.execute(
            "INSERT INTO memory_heads(record_id,revision,status,row_revision) VALUES('rule',1,'active',1)",
            [],
        )
        .unwrap();
    }

    fn proposal(db: &Connection, seeded: &Seeded, id: &str) {
        db.execute(
            "INSERT INTO memory_proposals(id,payload_digest,task_id,attempt_id,snapshot_id,review_state,payload,created_unix_ms) VALUES(?1,?2,?3,?4,NULL,'validated','{}',1)",
            params![id, digest(), seeded.task, seeded.attempt],
        )
        .unwrap();
    }

    fn promote(db: &Connection, proposal_id: &str) {
        db.execute(
            "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('fixture',?1,1,1,'{}')",
            [proposal_id],
        )
        .unwrap();
        let seq: i64 = db
            .query_row("SELECT last_insert_rowid()", [], |row| row.get(0))
            .unwrap();
        let decision = format!("decision-{proposal_id}");
        db.execute(
            "INSERT INTO review_decisions(id,proposal_id,payload_digest,decision,classification,reviewed_heads,reason,created_unix_ms) VALUES(?1,?2,?3,'approve','observation','[]','promoted',1)",
            params![decision, proposal_id, digest()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO memory_promotions(proposal_id,decision_id,payload_digest,sequence,change_ids,created_unix_ms) VALUES(?1,?2,?3,?4,'[]',1)",
            params![proposal_id, decision, digest(), seq],
        )
        .unwrap();
    }

    fn satisfy(db: &Connection, dependent: &str, predecessor: &str, result: &str) {
        db.execute(
            "INSERT INTO tasks(id,revision,state,title,active_attempt) VALUES(?1,1,'queued',?1,NULL)",
            [dependent],
        )
        .unwrap();
        db.execute(
            "INSERT INTO task_dependencies(task_id,predecessor_id,requirement) VALUES(?1,?2,'verified_result')",
            params![dependent, predecessor],
        )
        .unwrap();
        let satisfaction = format!(
            "{:x}",
            Sha256::digest(format!("sat-{dependent}").as_bytes())
        );
        db.execute(
            "INSERT INTO dependency_satisfactions(satisfaction_id,task_id,predecessor_task,requirement,state,evidence_kind,evidence_id,created_unix_ms)
             VALUES(?1,?2,?3,'verified_result','valid','verified_result',?4,1)",
            params![satisfaction, dependent, predecessor, result],
        )
        .unwrap();
    }

    fn attempt_row(db: &Connection, attempt: &str) -> (String, i64, i64) {
        db.query_row(
            "SELECT reservation, termination_observed, revision FROM attempts WHERE id=?1",
            [attempt],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap()
    }

    fn open_store() -> (tempfile::TempDir, SqliteStore) {
        let dir = tempfile::tempdir().unwrap();
        let db = SqliteStore::create(&dir.path().join("state.db")).unwrap();
        (dir, db)
    }

    #[test]
    fn create_ends_at_40_and_upgrade_from_39_reaches_40() {
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
            "barrier_revisions",
            "barrier_members",
            "barrier_stale_briefs",
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
        for index in [
            "barrier_members_by_attempt",
            "barrier_members_by_result",
            "barrier_stale_briefs_by_attempt",
        ] {
            let count: i64 = created
                .connection
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='index' AND name=?1",
                    [index],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "{index}");
        }
        let open_fn = include_str!("mod.rs")
            .split("pub fn open")
            .nth(1)
            .unwrap()
            .split("pub fn integrity_check")
            .next()
            .unwrap();
        assert!(!open_fn.contains("upgrade_v1"));
        assert!(!open_fn.contains("0040_barriers"));
        drop(created);

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.db");
        let db = SqliteStore::create(&path).unwrap();
        db.connection
            .execute(
                "INSERT INTO tasks(id,revision,state,title,active_attempt) VALUES('kept',1,'draft','kept',NULL)",
                [],
            )
            .unwrap();
        db.connection
            .execute_batch(
                "DROP TRIGGER IF EXISTS barrier_stale_briefs_no_update; DROP TRIGGER IF EXISTS barrier_stale_briefs_no_delete; DROP TRIGGER IF EXISTS barrier_members_no_update; DROP TRIGGER IF EXISTS barrier_members_no_delete; DROP TRIGGER IF EXISTS barrier_revisions_no_membership_update; DROP TABLE IF EXISTS barrier_stale_briefs; DROP TABLE IF EXISTS barrier_members; DROP TABLE IF EXISTS barrier_revisions; UPDATE store_meta SET schema_version=39; PRAGMA user_version=39;",
            )
            .unwrap();
        drop(db);
        let mut db = SqliteStore::open(&path).unwrap();
        assert_eq!(user_version(&db.connection), 39);
        assert!(matches!(
            db.import_legacy(&"cd".repeat(32), &[], &[]),
            Err(StoreError::UnsupportedSchema(39))
        ));
        let title: String = db
            .connection
            .query_row("SELECT title FROM tasks WHERE id='kept'", [], |row| {
                row.get(0)
            })
            .unwrap();
        db.upgrade_v1().unwrap();
        assert_eq!(user_version(&db.connection), 40);
        assert_eq!(
            db.connection
                .query_row("SELECT title FROM tasks WHERE id='kept'", [], |row| row
                    .get::<_, String>(
                    0
                ))
                .unwrap(),
            title
        );
        drop(db);
        let reopened = SqliteStore::open(&path).unwrap();
        assert_eq!(user_version(&reopened.connection), 40);
    }

    #[test]
    fn membership_edit_invalidates_the_release_token() {
        let (_dir, mut db) = open_store();
        let first = seed(&db.connection, "alpha", "verify_then_integrate");
        let second = seed(&db.connection, "beta", "verify_only");
        let (other_result, other_run) = add_result(&db.connection, &first, "alpha-2");
        proposal(&db.connection, &first, "prop-alpha");
        let integrated = add_integration(&db.connection, &first, "alpha-i1");
        let mut with_integration = member_of(
            &first,
            vec![ProposalDisposition {
                proposal_id: "prop-alpha".into(),
                disposition: "deferred".into(),
            }],
        );
        with_integration.integration_id = Some(integrated.clone());
        let head = head_of(&db);
        let frozen = db
            .freeze_barrier(&[with_integration.clone()], head)
            .unwrap();
        let head = head_of(&db);
        let again = db
            .freeze_barrier(&[with_integration.clone()], head)
            .unwrap();
        assert_eq!(again.barrier_id, frozen.barrier_id);
        assert_eq!(
            db.connection
                .query_row("SELECT count(*) FROM barrier_revisions", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );

        let mut added = with_integration.clone();
        let mut pair = vec![added.clone(), member_of(&second, vec![])];
        pair.reverse();
        let head = head_of(&db);
        let expanded = db.freeze_barrier(&pair, head).unwrap();
        assert_ne!(expanded.barrier_id, frozen.barrier_id);
        assert_ne!(expanded.release_token, frozen.release_token);
        let head = head_of(&db);
        assert!(matches!(
            db.release_barrier(&expanded.barrier_id, &frozen.release_token, head, 1),
            Err(StoreError::Invalid(ref message)) if message.contains("release token")
        ));
        assert!(expanded_released(&db, &expanded.barrier_id).is_none());

        let mut changed_result = with_integration.clone();
        changed_result.result_id = other_result;
        changed_result.verification_id = other_run;
        // The original integration still names the first result, so it no longer matches.
        let replacement = add_integration(
            &db.connection,
            &Seeded {
                result: changed_result.result_id.clone(),
                ..first.clone()
            },
            "alpha-i2",
        );
        changed_result.integration_id = Some(replacement);
        let head = head_of(&db);
        let edited = db.freeze_barrier(&[changed_result], head).unwrap();
        assert_ne!(edited.barrier_id, frozen.barrier_id);
        let head = head_of(&db);
        assert!(matches!(
            db.release_barrier(&edited.barrier_id, &frozen.release_token, head, 1),
            Err(StoreError::Invalid(_))
        ));

        promote(&db.connection, "prop-alpha");
        added.proposal_dispositions[0].disposition = "promoted".into();
        let head = head_of(&db);
        let promoted = db.freeze_barrier(&[added], head).unwrap();
        assert_ne!(promoted.barrier_id, frozen.barrier_id);
        let head = head_of(&db);
        assert!(matches!(
            db.release_barrier(&promoted.barrier_id, &frozen.release_token, head, 1),
            Err(StoreError::Invalid(ref message)) if message.contains("release token")
        ));
        let head = head_of(&db);
        assert!(matches!(
            db.release_barrier(&frozen.barrier_id, &frozen.release_token, head, 1),
            Err(StoreError::Invalid(ref message)) if message.contains("proposal disposition")
        ));
        let head = head_of(&db);
        let released = db
            .release_barrier(&promoted.barrier_id, &promoted.release_token, head, 1)
            .unwrap();
        assert!(released.released_seq.is_some());
        let replay = db
            .release_barrier(&promoted.barrier_id, &promoted.release_token, 0, 1)
            .unwrap();
        assert_eq!(replay.released_seq, released.released_seq);
    }

    fn expanded_released(db: &SqliteStore, barrier_id: &str) -> Option<i64> {
        db.connection
            .query_row(
                "SELECT released_seq FROM barrier_revisions WHERE barrier_id=?1",
                [barrier_id],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[test]
    fn mandatory_head_without_applied_blocks_release() {
        let (_dir, mut db) = open_store();
        let seeded = seed(&db.connection, "alpha", "verify_only");
        hard_head(&db.connection);
        let head = head_of(&db);
        let frozen = db
            .freeze_barrier(&[member_of(&seeded, vec![])], head)
            .unwrap();
        let head = head_of(&db);
        let error = db
            .release_barrier(&frozen.barrier_id, &frozen.release_token, head, 1)
            .unwrap_err();
        assert!(
            matches!(error, StoreError::Invalid(ref message) if message.contains("mandatory_revision_missing")),
            "{error:?}"
        );
        assert!(expanded_released(&db, &frozen.barrier_id).is_none());
    }

    #[test]
    fn required_set_generation_move_blocks_release() {
        let (_dir, mut db) = open_store();
        let seeded = seed(&db.connection, "alpha", "verify_only");
        let head = head_of(&db);
        let frozen = db
            .freeze_barrier(&[member_of(&seeded, vec![])], head)
            .unwrap();
        db.connection
            .execute(
                "UPDATE memory_required_generation SET generation=generation+1",
                [],
            )
            .unwrap();
        let head = head_of(&db);
        let error = db
            .release_barrier(&frozen.barrier_id, &frozen.release_token, head, 1)
            .unwrap_err();
        assert!(
            matches!(error, StoreError::Invalid(ref message) if message.contains("required set generation moved")),
            "{error:?}"
        );
        assert!(expanded_released(&db, &frozen.barrier_id).is_none());
    }

    #[test]
    fn revocation_during_release_blocks_dependents_and_does_not_release_capacity() {
        let (_dir, mut db) = open_store();
        let seeded = seed(&db.connection, "alpha", "verify_only");
        satisfy(&db.connection, "downstream", "alpha", &seeded.result);
        let before = attempt_row(&db.connection, &seeded.attempt);
        let live_before: i64 = db
            .connection
            .query_row(
                "SELECT count(*) FROM attempts WHERE termination_observed=0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let predecessor = Task {
            id: TaskId::new("alpha").unwrap(),
            revision: 1,
            state: TaskState::Running,
            title: "alpha".into(),
            active_attempt: Some(AttemptId::new(&seeded.attempt).unwrap()),
        };
        assert!(super::super::satisfaction::dependency_blocker(
            &db.connection,
            "downstream",
            &predecessor,
            DependencyRequirement::VerifiedResult,
            true
        )
        .unwrap()
        .is_none());
        let head = head_of(&db);
        let frozen = db
            .freeze_barrier(&[member_of(&seeded, vec![])], head)
            .unwrap();
        let head = head_of(&db);
        let revoked = db.revoke_barrier(&frozen.barrier_id, head).unwrap();
        assert!(revoked.revoked_seq.is_some());
        let again = db.revoke_barrier(&frozen.barrier_id, 0).unwrap();
        assert_eq!(again.revoked_seq, revoked.revoked_seq);
        let admission: String = db
            .connection
            .query_row("SELECT factory_admission FROM project_control", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(admission, "off");
        let head = head_of(&db);
        let error = db
            .release_barrier(&frozen.barrier_id, &frozen.release_token, head, 1)
            .unwrap_err();
        assert!(
            matches!(error, StoreError::Invalid(ref message) if message.contains("revoked")),
            "{error:?}"
        );
        let after = attempt_row(&db.connection, &seeded.attempt);
        assert_eq!(after, before);
        assert_eq!(after.1, 0);
        let live_after: i64 = db
            .connection
            .query_row(
                "SELECT count(*) FROM attempts WHERE termination_observed=0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(live_before, live_after);
        assert!(super::super::satisfaction::dependency_blocker(
            &db.connection,
            "downstream",
            &predecessor,
            DependencyRequirement::VerifiedResult,
            true
        )
        .unwrap()
        .is_some());
        assert!(expanded_released(&db, &frozen.barrier_id).is_none());
    }

    #[test]
    fn stale_brief_after_revocation_is_recorded_and_cannot_be_accepted() {
        let (_dir, mut db) = open_store();
        let seeded = seed(&db.connection, "alpha", "verify_only");
        let head = head_of(&db);
        let frozen = db
            .freeze_barrier(&[member_of(&seeded, vec![])], head)
            .unwrap();
        let head = head_of(&db);
        assert!(matches!(
            db.record_stale_brief(&frozen.barrier_id, &seeded.attempt, "late brief", head),
            Err(StoreError::Invalid(_))
        ));
        let head = head_of(&db);
        db.revoke_barrier(&frozen.barrier_id, head).unwrap();
        let head = head_of(&db);
        let recorded = db
            .record_stale_brief(&frozen.barrier_id, &seeded.attempt, "late brief", head)
            .unwrap();
        assert!(!recorded.accepted);
        let events_before: i64 = db
            .connection
            .query_row("SELECT count(*) FROM events", [], |row| row.get(0))
            .unwrap();
        let replay = db
            .record_stale_brief(&frozen.barrier_id, &seeded.attempt, "late brief", 0)
            .unwrap();
        assert_eq!(replay, recorded);
        let events_after: i64 = db
            .connection
            .query_row("SELECT count(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(events_before, events_after);
        assert!(matches!(
            db.accept_stale_brief(&recorded.brief_id),
            Err(StoreError::Invalid(ref message)) if message.contains("cannot be accepted")
        ));
        let accepted: i64 = db
            .connection
            .query_row(
                "SELECT accepted FROM barrier_stale_briefs WHERE brief_id=?1",
                [&recorded.brief_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(accepted, 0);
        assert!(db
            .connection
            .execute("UPDATE barrier_stale_briefs SET accepted=1", [])
            .is_err());
    }

    #[test]
    fn idempotent_freeze_returns_the_stored_row_not_a_later_max() {
        let (_dir, mut db) = open_store();
        let first = seed(&db.connection, "alpha", "verify_only");
        let second = seed(&db.connection, "beta", "verify_only");
        let head = head_of(&db);
        let original = db
            .freeze_barrier(&[member_of(&first, vec![])], head)
            .unwrap();
        db.connection
            .execute(
                "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('fixture','later',1,1,'{}')",
                [],
            )
            .unwrap();
        let replay = db.freeze_barrier(&[member_of(&first, vec![])], 0).unwrap();
        assert_eq!(replay, original);
        let head = head_of(&db);
        let later = db
            .freeze_barrier(
                &[member_of(&first, vec![]), member_of(&second, vec![])],
                head,
            )
            .unwrap();
        assert_ne!(later.barrier_id, original.barrier_id);
        let head = head_of(&db);
        let again = db
            .freeze_barrier(&[member_of(&first, vec![])], head)
            .unwrap();
        assert_eq!(again.barrier_id, original.barrier_id);
        assert_eq!(
            db.connection
                .query_row("SELECT count(*) FROM barrier_revisions", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn values_the_check_cannot_store_are_invalid() {
        let (_dir, mut db) = open_store();
        let seeded = seed(&db.connection, "alpha", "verify_only");
        let mut member = member_of(
            &seeded,
            vec![ProposalDisposition {
                proposal_id: "prop-alpha".into(),
                disposition: "maybe".into(),
            }],
        );
        let head = head_of(&db);
        let error = db.freeze_barrier(&[member.clone()], head).unwrap_err();
        assert!(matches!(error, StoreError::Invalid(_)), "{error:?}");
        member.proposal_dispositions.clear();
        member.integration_id = Some("short".into());
        let head = head_of(&db);
        let error = db.freeze_barrier(&[member], head).unwrap_err();
        assert!(matches!(error, StoreError::Invalid(_)), "{error:?}");
        assert_eq!(
            db.connection
                .query_row("SELECT count(*) FROM barrier_revisions", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}

//! Frozen wave membership. Release calls `memory_barrier::enforce` per member.
//! It does not keep a second copy of the mandatory-head check.
use super::*;
use rusqlite::OptionalExtension;
use serde::Serialize;
use std::collections::BTreeSet;
mod memory;
mod authorization;
mod invalidation;
mod stops;
pub use stops::{BarrierStopTurn, service_project_barrier_stops};
pub(super) fn memory_changed(db: &Connection, record: Option<&str>, sequence: u64) -> Result<()> {
    invalidation::changed(db, record, sequence)
}

fn earliest_expiry(left:Option<i64>,right:Option<i64>)->Option<i64> {left.into_iter().chain(right).min()}

pub(super) fn require_current_release(db: &Connection, reference: &BarrierReleaseReference, authority: &VersionedReference, config: Option<&str>, now: i64, budget: Option<&read_budget::ReadBudget>) -> Result<()> {
    require_current_release_valid_until(db,reference,authority,config,now,budget).map(|_|())
}

pub(super) fn require_current_release_valid_until(db: &Connection, reference: &BarrierReleaseReference, authority: &VersionedReference, config: Option<&str>, now: i64, budget: Option<&read_budget::ReadBudget>) -> Result<Option<i64>> {
    let Some(budget) = budget else {
        return memory::bounded(db, |budget| require_current_release_valid_until(db, reference, authority, config, now, Some(budget)));
    };
    let mut pending = vec![super::contract_binding::ResultBarrier {
        reference: reference.clone(), authority: authority.clone(), config: config.map(str::to_string),
    }];
    let mut visited = BTreeSet::new();
    let mut members = 0usize;
    let mut expires_unix_ms=None;
    while let Some(required) = pending.pop() {
        budget.check()?;
        let key = (required.reference.barrier_id.clone(), required.reference.release_sequence,
            required.reference.authorization_digest.clone(), required.authority.id.clone(),
            required.authority.revision, required.authority.digest.clone(), required.config.clone());
        if !visited.insert(key) { continue; }
        if visited.len() > 64 { return Err(StoreError::Limit("barrier ancestry exceeds 64 releases".into())); }
        expires_unix_ms=earliest_expiry(expires_unix_ms,authorization::require_current_release(db, &required.reference, &required.authority, required.config.as_deref(), now, Some(budget))?);
        let mut query = db.prepare("SELECT m.task_id,m.contract_revision,c.raw_digest,m.attempt_id
            FROM barrier_members m JOIN task_contracts c ON c.task_id=m.task_id AND c.contract_revision=m.contract_revision
            WHERE m.barrier_id=?1 ORDER BY m.position LIMIT 1001")?;
        let mut rows = query.query([&required.reference.barrier_id])?;
        while let Some(row) = rows.next()? {
            members += 1;
            if members > 1000 { return Err(StoreError::Limit("barrier ancestry exceeds 1000 member references".into())); }
            budget.row(row, &[])?;
            let (task, revision, digest, attempt): (String,u64,String,String) = (row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?);
            if let Some(parent) = super::contract_binding::result_barrier(db, &task, revision, &digest, &attempt, Some(budget))? {
                // Every prerequisite was released before the consuming wave.
                // Strict sequence descent rejects cycles and forward references.
                if parent.reference.release_sequence >= required.reference.release_sequence {
                    return Err(invalid("barrier prerequisite release is not earlier than its consuming wave"));
                }
                pending.push(parent);
            }
        }
    }
    budget.check()?;
    Ok(expires_unix_ms)
}

const SCHEMA_VERSION: u32 = 40;
const MAX_MEMBERS: usize = 1_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposalDisposition {
    pub proposal_id: String,
    pub disposition: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BarrierMember {
    pub task_id: String,
    pub contract_revision: u64,
    pub attempt_id: String,
    pub result_id: String,
    pub verification_id: String,
    pub integration_id: Option<String>,
    pub proposal_dispositions: Vec<ProposalDisposition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FrozenBarrier {
    pub barrier_id: String,
    pub memory_manifest_version: u32,
    pub required_set_generation: u64,
    pub memory_manifest_digest: String,
    pub release_token: String,
    pub released_seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_reference: Option<BarrierReleaseReference>,
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
    if member.proposal_dispositions.len() > MAX_MEMBERS {
        return Err(StoreError::Limit(
            "proposal dispositions exceed 1000".into(),
        ));
    }
    let mut listed = Vec::with_capacity(member.proposal_dispositions.len());
    for item in &member.proposal_dispositions {
        identifier(&item.proposal_id, "proposal id is invalid")?;
        if !matches!(
            item.disposition.as_str(),
            "promoted" | "rejected" | "deferred"
        ) {
            return Err(invalid("proposal disposition is invalid"));
        }
        listed.push((item.proposal_id.clone(), item.disposition.clone()));
    }
    listed.sort();
    if listed.windows(2).any(|pair|pair[0].0==pair[1].0) {return Err(invalid("proposal disposition is duplicated"));}
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

fn stored_disposition(db: &Connection, proposal_id: &str,budget:Option<&read_budget::ReadBudget>) -> Result<Option<String>> {
    let row: Option<(String, bool, bool)> = read_budget::optional(db,
            "SELECT p.review_state,
                    EXISTS(SELECT 1 FROM memory_promotions m WHERE m.proposal_id=p.id),
                    EXISTS(SELECT 1 FROM review_decisions d WHERE d.proposal_id=p.id AND d.decision='reject')
             FROM memory_proposals p WHERE p.id=?1",
            [proposal_id],budget,&[],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
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
    budget: Option<&read_budget::ReadBudget>,
) -> Result<()> {
    let mut stmt = db.prepare(
        "SELECT id FROM memory_proposals WHERE task_id=?1 AND attempt_id=?2 ORDER BY id LIMIT 1001",
    )?;
    let mut cursor = stmt.query(params![member.task_id, member.attempt_id])?;
    let mut stored = Vec::new();
    while let Some(row) = cursor.next()? {
        if stored.len() == MAX_MEMBERS { return Err(StoreError::Limit("proposals exceed 1000".into())); }
        if let Some(budget) = budget { budget.row(row, &[])?; }
        stored.push(row.get::<_,String>(0)?);
    }
    let mut expected = Vec::with_capacity(stored.len());
    for proposal_id in &stored {
        let disposition = stored_disposition(db, proposal_id,budget)?
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

fn evidence_matches_with_budget(db: &Connection, member: &BarrierMember, budget: Option<&read_budget::ReadBudget>) -> Result<()> {
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
    // A revised contract requires a new barrier decision. The retained signed
    // bytes bind its route and policy, not only the historical route column.
    let contract = super::contract_binding::latest_with_budget(db, &member.task_id, budget)?
        .ok_or_else(|| invalid("task contract is not stored"))?;
    if contract.contract_revision != member.contract_revision {
        return Err(invalid("barrier member contract is no longer current"));
    }
    let attempt_exists: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM attempts WHERE id=?1 AND task_id=?2)",
        params![member.attempt_id, member.task_id],
        |row| row.get(0),
    )?;
    if !attempt_exists {
        return Err(invalid("attempt is not stored"));
    }
    let mut query = db.prepare(
            "SELECT r.run_id, r.task_id, r.attempt_id, r.contract_revision, r.state, r.policy_id, r.policy_digest
             FROM verified_results v JOIN verification_runs r ON r.run_id=v.run_id
             JOIN result_submissions s ON s.submission_id=r.submission_id
             WHERE v.result_id=?1 AND r.contract_digest=?2 AND r.project_store=?3
             AND s.contract_digest=r.contract_digest AND s.contract_revision=r.contract_revision
             AND s.task_id=r.task_id AND s.attempt_id=r.attempt_id AND s.project_store=r.project_store
             AND s.repository=?4 AND s.base_oid=?5 AND s.memory_snapshot_id IS ?7
             AND v.submission_id=r.submission_id AND v.commit_oid=s.candidate_oid
             AND v.commit_oid=r.commit_oid AND v.tree_oid=r.tree_oid
             AND v.object_format=r.object_format AND s.object_format=r.object_format AND r.object_format=?6
             AND v.policy_digest=r.policy_digest AND v.receipt_digest=r.receipt_digest
             AND v.memory_fence=r.memory_fence AND v.isolation=r.isolation
             AND r.isolation='linux-unshare-user-pid-mount-v1' AND r.exit_status=0",
    )?;
    let mut linked_rows = query.query(params![member.result_id,contract.digest,contract.project_store,contract.repository,
                contract.base_oid,contract.object_format.as_str(),contract.memory_snapshot_id])?;
    let linked: Option<(String, String, String, i64, String, String, String)> = if let Some(row) = linked_rows.next()? {
        if let Some(budget) = budget { budget.row(row, &[])?; }
        Some((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
        ))
    } else { None };
    let Some((run_id, task_id, attempt_id, stored_revision, state, policy_id, policy_digest)) =
        linked
    else {
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
    let policy = contract
        .acceptance_policies
        .iter()
        .find(|policy| policy.id == policy_id)
        .ok_or_else(|| invalid("verification policy is not in the current signed contract"))?;
    if format!("{:x}", Sha256::digest(policy.text.as_bytes())) != policy_digest {
        return Err(invalid(
            "verification policy digest does not match the signed contract",
        ));
    }
    match (contract.route.as_str(), member.integration_id.as_deref()) {
        ("verify_only", None) => {}
        ("verify_only", Some(_)) => return Err(invalid("integration is not required")),
        ("verify_then_integrate", None) => return Err(invalid("integration is required")),
        ("verify_then_integrate", Some(integration_id)) => {
            let verified: bool = db
                .query_row(
                    "SELECT EXISTS(SELECT 1
                     FROM integrated_commits i
                     JOIN integration_operations o ON o.operation_id=i.operation_id
                     WHERE i.integrated_id=?1 AND o.state='integrated' AND o.checks_passed=1 AND o.verified_result_id=?2)",
                    params![integration_id,member.result_id],
                    |row| row.get(0),
                )?;
            if !verified {
                return Err(invalid("integration does not match result"));
            }
            if !super::contract_binding::integrated_output_checks_current(db, integration_id, budget)? {
                return Err(invalid("integration requires fresh merged-output verification"));
            }
        }
        _ => return Err(invalid("task contract route is invalid")),
    }
    let active: bool = db.query_row(
        "SELECT active_attempt IS ?2 FROM tasks WHERE id=?1",
        params![member.task_id,member.attempt_id],
        |row| row.get(0),
    )?;
    if !active {
        return Err(invalid("member attempt is not active"));
    }
    Ok(())
}

#[cfg(test)]
fn memory_manifest_v1(
    db: &Connection,
    generation: u64,
    members: &[BarrierMember],
) -> Result<String> {
    let mut stmt = db.prepare(
        "SELECT h.record_id, h.revision
         FROM memory_heads h JOIN memory_records r ON r.id=h.record_id
         WHERE h.status='active' AND (r.is_hard=1 OR r.kind IN ('constraint','hard_memory'))
         ORDER BY h.record_id LIMIT 10001",
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
    budget:Option<&read_budget::ReadBudget>,
) -> Result<Vec<(BarrierMember, String)>> {
    if let Some(budget)=budget {budget.check()?;}
    if members.is_empty() {
        return Err(invalid("barrier membership is empty"));
    }
    if members.len() > MAX_MEMBERS {
        return Err(StoreError::Limit("barrier membership exceeds 1000".into()));
    }
    // Charge caller-owned fields before cloning/encoding them. Sort references
    // so malformed or oversized input is not copied as an entire member tree.
    for member in members {
        if member.proposal_dispositions.len()>MAX_MEMBERS {return Err(StoreError::Limit("proposal dispositions exceed 1000".into()));}
        if let Some(budget)=budget {
            budget.bytes(1024)?;
            for value in [&member.task_id,&member.attempt_id,&member.result_id,&member.verification_id] {budget.bytes(value.len())?;}
            if let Some(value)=&member.integration_id {budget.bytes(value.len())?;}
            for item in &member.proposal_dispositions {budget.bytes(256)?;budget.bytes(item.proposal_id.len())?;budget.bytes(item.disposition.len())?;}
        }
    }
    let mut ordered:Vec<_> = members.iter().collect();
    ordered.sort_by(|left, right| left.task_id.cmp(&right.task_id));
    if ordered
        .windows(2)
        .any(|pair| pair[0].task_id == pair[1].task_id)
    {
        return Err(invalid("barrier membership repeats a task"));
    }
    let mut canonical = Vec::with_capacity(ordered.len());
    for member in ordered {
        evidence_matches_with_budget(db, member,budget)?;
        let listed = canonical_dispositions(member)?;
        dispositions_match(db, member, &listed,budget)?;
        let json = dispositions_json(&listed)?;
        if let Some(budget)=budget {budget.bytes(json.len())?;}
        canonical.push((member.clone(), json));
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
    load_with_budget(db, barrier_id, None)
}

fn load_with_budget(db: &Connection, barrier_id: &str, budget: Option<&read_budget::ReadBudget>) -> Result<Option<FrozenBarrier>> {
    let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let source = if version >= 43 { "barrier_current_status" } else { "barrier_revisions" };
    let mut header = db.prepare(&format!(
        "SELECT barrier_id, required_set_generation, memory_manifest_digest, release_token, released_seq, revoked_seq
         FROM {source} WHERE barrier_id=?1",
    ))?;
    let mut headers = header.query([barrier_id])?;
    let Some(row) = headers.next()? else {
        return Ok(None);
    };
    if let Some(budget) = budget { budget.row(row, &[])?; }
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
         FROM barrier_members WHERE barrier_id=?1 ORDER BY position LIMIT 1001",
    )?;
    let mut cursor = stmt.query([&barrier_id])?;
    let mut rows = Vec::new();
    while let Some(row) = cursor.next()? {
        if rows.len() == MAX_MEMBERS { return Err(StoreError::Limit("stored barrier membership exceeds 1000".into())); }
        if let Some(budget) = budget { budget.row(row, &[(6,1)])?; }
        rows.push((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, String>(6)?,
        ));
    }
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
    let memory_manifest_version = memory::stored_version(db, &barrier_id, &manifest, budget)?;
    let release_reference = if version >= 43 && released.is_some() {
        let mut query = db.prepare("SELECT authorization_digest,sequence FROM barrier_release_authorizations WHERE barrier_id=?1 AND sequence=?2")?;
        let mut rows = query.query(params![barrier_id,released])?;
        if let Some(row) = rows.next()? {
            if let Some(budget) = budget { budget.row(row, &[])?; }
            let reference = BarrierReleaseReference {
                schema_version: 1,
                barrier_id: barrier_id.clone(),
                authorization_digest: row.get(0)?,
                release_sequence: row.get(1)?,
            };
            reference.validate().map_err(StoreError::Corrupt)?;
            Some(reference)
        } else { None }
    } else { None };
    Ok(Some(FrozenBarrier {
        barrier_id,
        release_reference,
        memory_manifest_version,
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
    recheck_release_ready(db,barrier,now,None).map(|_|())
}

fn recheck_release_ready(db:&Connection,barrier:&FrozenBarrier,now:i64,budget:Option<&read_budget::ReadBudget>)->Result<Option<i64>> {
    let check = |budget:&read_budget::ReadBudget| {
        let mut expires=recheck_ready_with_budget(db, barrier, now, Some(budget))?;
        for member in &barrier.members {
            expires=earliest_expiry(expires,super::contract_binding::verified_result_barrier_valid_until(db, &member.result_id, now, Some(budget))?);
        }
        Ok(expires)
    };
    match budget {Some(budget)=>check(budget),None=>memory::bounded(db,check)}
}

fn recheck_ready_with_budget(db: &Connection, barrier: &FrozenBarrier, now: i64, budget: Option<&read_budget::ReadBudget>) -> Result<Option<i64>> {
    if barrier.memory_manifest_version != 2 {
        return Err(invalid(
            "legacy barrier requires a new freeze with a version-2 memory read set",
        ));
    }
    // Live generation, not the value captured when the caller built the request.
    if required_generation(db)? != barrier.required_set_generation {
        return Err(invalid("required set generation moved"));
    }
    let current=memory::current_with_budget(db, barrier.required_set_generation, &barrier.members, budget)?;
    if current.digest != barrier.memory_manifest_digest
    {
        return Err(invalid("frozen barrier memory manifest changed"));
    }
    for member in &barrier.members {
        evidence_matches_with_budget(db, member, budget)?;
        let listed = canonical_dispositions(member)?;
        dispositions_match(db, member, &listed, budget)?;
        if listed
            .iter()
            .any(|(_, disposition)| disposition == "deferred")
        {
            return Err(invalid("deferred proposal blocks release"));
        }
        // Mandatory-head coverage stays in the per-task check.
        memory_barrier::enforce_with_budget(db, &member.task_id, now,budget)?;
    }
    Ok(current.expires_unix_ms)
}

impl SqliteStore {
    pub fn frozen_barrier(&mut self, barrier_id: &str) -> Result<Option<FrozenBarrier>> {
        self.frozen_barrier_with_budget(barrier_id,None)
    }

    pub(crate) fn frozen_barrier_with_budget(&mut self,barrier_id:&str,budget:Option<&read_budget::ReadBudget>)->Result<Option<FrozenBarrier>> {
        if let Some(budget)=budget {budget.check()?;}
        if !hex64(barrier_id) {
            return Err(invalid("barrier id is invalid"));
        }
        let tx = self.connection.transaction()?;
        require_schema(&tx)?;
        load_with_budget(&tx, barrier_id,budget)
    }

    pub fn freeze_barrier(
        &mut self,
        members: &[BarrierMember],
        expected_head: u64,
    ) -> Result<FrozenBarrier> {
        self.freeze_barrier_with_budget(members,expected_head,None)
    }

    pub(crate) fn freeze_barrier_with_budget(&mut self,members:&[BarrierMember],expected_head:u64,budget:Option<&read_budget::ReadBudget>)->Result<FrozenBarrier> {
        if let Some(budget)=budget {budget.check()?;}
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_schema(&tx)?;
        let canonical = canonical_members(&tx, members,budget)?;
        let generation = required_generation(&tx)?;
        let prepared: Vec<BarrierMember> =
            canonical.iter().map(|(member, _)| member.clone()).collect();
        let prepare_fence = |budget:&read_budget::ReadBudget| {
            for member in &prepared {
                super::contract_binding::require_verified_result_barrier_with_budget(&tx, &member.result_id, jiff::Timestamp::now().as_millisecond(), Some(budget))?;
            }
            memory::current_with_budget(&tx, generation, &prepared, Some(budget))
        };
        let fence=match budget {Some(budget)=>prepare_fence(budget),None=>memory::bounded(&tx,prepare_fence)}?;
        let manifest = fence.digest;
        let barrier_id = identity(generation, &manifest, &canonical)?;
        if let Some(existing) = load_with_budget(&tx, &barrier_id,budget)? {
            if let Some(budget)=budget {budget.check()?;}
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
            &serde_json::json!({"generation": generation, "members": prepared.len(), "memory_manifest_version": 2}),
        )?;
        tx.execute(
            "INSERT INTO barrier_revisions(barrier_id,required_set_generation,memory_manifest_digest,release_token,released_seq,revoked_seq,created_seq)
             VALUES(?1,?2,?3,?4,NULL,NULL,?5)",
            params![barrier_id, i64::try_from(generation).map_err(|_| invalid("required set generation"))?, manifest, token, created],
        )?;
        tx.execute(
            "INSERT INTO barrier_memory_read_sets VALUES(?1,2,?2)",
            params![barrier_id, fence.payload],
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
        let stored = load_with_budget(&tx, &barrier_id,budget)?
            .ok_or_else(|| StoreError::Corrupt("frozen barrier is missing".into()))?;
        if let Some(budget)=budget {budget.check()?;}
        tx.commit()?;
        Ok(stored)
    }

    pub fn release_barrier(
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
        let stored = publish_release(&tx, &barrier, None)?;
        tx.commit()?;
        Ok(stored)
    }

    pub fn revoke_barrier(
        &mut self,
        barrier_id: &str,
        expected_head: u64,
    ) -> Result<FrozenBarrier> {
        self.revoke_barrier_with_budget(barrier_id,expected_head,None,None)
    }

    pub(crate) fn revoke_barrier_with_budget(&mut self,barrier_id:&str,expected_head:u64,reason:Option<&str>,budget:Option<&read_budget::ReadBudget>)->Result<FrozenBarrier> {
        if let Some(budget)=budget {budget.check()?;}
        if !hex64(barrier_id) {
            return Err(invalid("barrier id is invalid"));
        }
        if reason.is_some_and(|reason|reason.trim().is_empty() || reason.len()>4000 || reason.chars().any(char::is_control)) {
            return Err(invalid("invalid barrier revocation reason"));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_schema(&tx)?;
        let barrier = load_with_budget(&tx, barrier_id,budget)?.ok_or_else(|| invalid("barrier is not stored"))?;
        if let Some(sequence)=barrier.revoked_seq {
            if let Some(reason)=reason {
                let receipt:Option<(String,u64)>=read_budget::optional(&tx,
                    "SELECT payload,payload_version FROM events WHERE sequence=?1 AND kind='barrier.revoked' AND entity=?2",
                    params![integer(sequence)?,barrier_id],budget,&[(0,1)],|row|Ok((row.get(0)?,row.get(1)?)))?;
                let Some((payload,version))=receipt else{return Err(StoreError::Corrupt("barrier revocation event is missing".into()));};
                let payload:serde_json::Value=serde_json::from_str(&payload).map_err(|_|StoreError::Corrupt("invalid barrier revocation event".into()))?;
                if version!=1 || payload!=serde_json::json!({"source":"operator","reason":reason,"expected_head":expected_head}) {return Err(StoreError::Conflict);}
            }
            if let Some(budget)=budget {budget.check()?;}
            return Ok(barrier);
        }
        let version: u32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if barrier.released_seq.is_some() && version < 43 {
            return Err(StoreError::UnsupportedSchema(version));
        }
        if head(&tx)? != expected_head {
            return Err(StoreError::Conflict);
        }
        // The attempt row stays so its slot remains held until termination_observed.
        // Dependents stay blocked by this revoked membership, not by rewriting receipts.
        let payload=match reason {
            Some(reason)=>serde_json::json!({"source":"operator","reason":reason,"expected_head":expected_head}),
            None=>serde_json::json!({}),
        };
        let sequence = insert_event(&tx, "barrier.revoked", barrier_id, &payload)?;
        // Schema 43 applies the transition from the event, including automatic
        // memory invalidation. Historical schemas still use the direct update.
        if version < 43 {
            let updated = tx.execute(
                "UPDATE barrier_revisions SET revoked_seq=?2 WHERE barrier_id=?1 AND revoked_seq IS NULL AND released_seq IS NULL",
                params![barrier_id, sequence],
            )?;
            if updated != 1 {
                return Err(StoreError::Conflict);
            }
        }
        let stored = load_with_budget(&tx, barrier_id,budget)?
            .ok_or_else(|| StoreError::Corrupt("revoked barrier is missing".into()))?;
        if let Some(budget)=budget {budget.check()?;}
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

fn publish_release(db: &Connection, barrier: &FrozenBarrier, authorization: Option<&str>) -> Result<FrozenBarrier> {
    publish_release_with_budget(db,barrier,authorization,None)
}

fn publish_release_with_budget(db:&Connection,barrier:&FrozenBarrier,authorization:Option<&str>,budget:Option<&read_budget::ReadBudget>)->Result<FrozenBarrier> {
    let mut payload = serde_json::json!({"token": barrier.release_token});
    if let Some(digest) = authorization {
        payload["authorization_digest"] = digest.into();
        payload["release_policy"] = "all_members_ready_v1".into();
    }
    let sequence = insert_event(db, "barrier.released", &barrier.barrier_id, &payload)?;
    if db.execute(
        "UPDATE barrier_revisions SET released_seq=?2 WHERE barrier_id=?1 AND released_seq IS NULL AND revoked_seq IS NULL",
        params![barrier.barrier_id, sequence],
    )? != 1 {
        return Err(StoreError::Conflict);
    }
    load_with_budget(db, &barrier.barrier_id,budget)?.ok_or_else(|| StoreError::Corrupt("released barrier is missing".into()))
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
pub(super) mod tests {
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

    fn contract_bytes(task: &str, route: &str, revision: u64) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "version":1,"project_store":"/tmp/project","expected_head":0,"task_id":task,"contract_revision":revision,
            "deliverable":"Barrier fixture","non_goals":"No external work","acceptance_policies":[{"id":"policy-1","text":"{}"}],
            "repository":"/tmp/repo","base_oid":"b".repeat(40),"object_format":"sha1","dependencies":[],"capability_flags":[],
            "profile_kind":"codex","retry_class":"none","result_schema_id":"result-v1","route":route,
            "authority":{"id":"owner-approval-policy","revision":1,"digest":"ab".repeat(32)}
        })).unwrap()
    }

    /// Model authenticated predecessor evidence for supervised-process tests.
    /// The real signature ingress has its own service tests; this helper does
    /// not claim live execution or cryptographic verification of the fixture.
    pub(crate) fn install_worker_barrier_fixture(db: &mut SqliteStore, project: &Path, task: &TaskId, profile: &FrozenProfile) {
        let source = seed(&db.connection, "barrier-source", "verify_only");
        db.connection.execute("UPDATE attempts SET state='completed',termination_observed=1 WHERE id=?1", [&source.attempt]).unwrap();
        let frozen = db.freeze_barrier(&[member_of(&source, vec![])], head_of(db)).unwrap();
        let document = db.draft_barrier_release(&frozen.barrier_id, profile.permission_policy.clone(), profile.config.digest.as_deref().unwrap(), jiff::Timestamp::now().as_millisecond()+60_000).unwrap();
        let authorization = PreparedBarrierRelease::parse_verified(&serde_json::to_vec(&document).unwrap()).unwrap();
        let reference = db.release_authorized_barrier(&authorization).unwrap().release_reference.unwrap();
        let mut contract: serde_json::Value = serde_json::from_slice(&contract_bytes(task.as_str(), "verify_only", 1)).unwrap();
        contract["version"] = 2.into();
        contract["required_barrier"] = serde_json::to_value(reference).unwrap();
        contract["project_store"] = project.join(".state/state.db").canonicalize().unwrap().to_str().unwrap().into();
        contract["repository"] = project.to_str().unwrap().into();
        contract["profile_kind"] = profile.kind.clone().into();
        contract["authority"] = serde_json::to_value(&profile.permission_policy).unwrap();
        contract["expected_head"] = head_of(db).into();
        db.install_contract(&PreparedContract::parse_verified(&serde_json::to_vec(&contract).unwrap()).unwrap()).unwrap();
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
        let contract = PreparedContract::parse_verified(&contract_bytes(task, route, 1)).unwrap();
        let policy_digest = format!("{:x}", Sha256::digest(b"{}"));
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
             VALUES(?1,1,NULL,'/tmp/project',0,'/tmp/repo',?2,'sha1',NULL,?3,?6,?4,?5)",
            params![task, "b".repeat(40), route, contract.digest, installed, contract.raw],
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
             VALUES(?1,'/tmp/project',?2,?3,'{}',?4,1,?7,?5,'/tmp/repo',?6,?8,'sha1',NULL,'[]','[]',1)",
            params![submission, format!("submit-{task}"), hash, task, attempt, "b".repeat(40), contract.digest, "c".repeat(40)],
        )
        .unwrap();
        db.execute(
            "INSERT INTO verification_runs(run_id,project_store,idempotency_key,payload_digest,submission_id,task_id,contract_revision,contract_digest,attempt_id,policy_id,policy_digest,commit_oid,tree_oid,object_format,memory_fence,isolation,argv,library_manifest,state,reason,exit_status,receipt_digest,store_device,store_inode,created_unix_ms)
             VALUES(?1,'/tmp/project',?2,?3,?4,?5,1,?8,?6,'policy-1',?9,?7,?7,'sha1',0,'linux-unshare-user-pid-mount-v1','[]','[]','accepted',NULL,0,?3,0,0,1)",
            params![verification, format!("verify-{task}"), hash, submission, task, attempt, "c".repeat(40), contract.digest, policy_digest],
        )
        .unwrap();
        db.execute(
            "INSERT INTO verified_results(result_id,run_id,submission_id,commit_oid,tree_oid,object_format,policy_digest,receipt_digest,isolation,memory_fence,created_unix_ms)
             VALUES(?1,?2,?3,?4,?4,'sha1',?6,?5,'linux-unshare-user-pid-mount-v1',0,1)",
            params![result, verification, submission, "c".repeat(40), hash, policy_digest],
        )
        .unwrap();
        // Fixture models fresh native verification at the current proof version.
        db.execute("INSERT INTO verification_contract_checks VALUES(?1,2)",[&result]).unwrap();
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
        let contract_digest: String = db
            .query_row(
                "SELECT raw_digest FROM task_contracts WHERE task_id=?1 AND contract_revision=1",
                [&seeded.task],
                |r| r.get(0),
            )
            .unwrap();
        let policy_digest = format!("{:x}", Sha256::digest(b"{}"));
        let submission = format!(
            "{:x}",
            Sha256::digest(format!("submission-{label}").as_bytes())
        );
        let result = format!("{:x}", Sha256::digest(format!("result-{label}").as_bytes()));
        let verification = format!("{:x}", Sha256::digest(format!("run-{label}").as_bytes()));
        db.execute(
            "INSERT INTO result_submissions(submission_id,project_store,idempotency_key,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,memory_snapshot_id,artifact_manifest,claimed_checks,created_unix_ms)
             VALUES(?1,'/tmp/project',?2,?3,'{}',?4,1,?7,?5,'/tmp/repo',?6,?8,'sha1',NULL,'[]','[]',1)",
            params![submission, format!("submit-{label}"), hash, seeded.task, seeded.attempt, "b".repeat(40), contract_digest, "c".repeat(40)],
        )
        .unwrap();
        db.execute(
            "INSERT INTO verification_runs(run_id,project_store,idempotency_key,payload_digest,submission_id,task_id,contract_revision,contract_digest,attempt_id,policy_id,policy_digest,commit_oid,tree_oid,object_format,memory_fence,isolation,argv,library_manifest,state,reason,exit_status,receipt_digest,store_device,store_inode,created_unix_ms)
             VALUES(?1,'/tmp/project',?2,?3,?4,?5,1,?8,?6,'policy-1',?9,?7,?7,'sha1',0,'linux-unshare-user-pid-mount-v1','[]','[]','accepted',NULL,0,?3,0,0,1)",
            params![verification, format!("verify-{label}"), hash, submission, seeded.task, seeded.attempt, "c".repeat(40), contract_digest, policy_digest],
        )
        .unwrap();
        db.execute(
            "INSERT INTO verified_results(result_id,run_id,submission_id,commit_oid,tree_oid,object_format,policy_digest,receipt_digest,isolation,memory_fence,created_unix_ms)
             VALUES(?1,?2,?3,?4,?4,'sha1',?6,?5,'linux-unshare-user-pid-mount-v1',0,1)",
            params![result, verification, submission, "c".repeat(40), hash, policy_digest],
        )
        .unwrap();
        db.execute("INSERT INTO verification_contract_checks VALUES(?1,2)",[&result]).unwrap();
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

    fn optional_head(db: &Connection, kind: &str) {
        let seq: u64 = db
            .query_row("SELECT max(sequence) FROM events", [], |r| r.get(0))
            .unwrap();
        db.execute(
            "INSERT OR IGNORE INTO objects VALUES(?1,1,'available','unclaimed',0,0)",
            [digest()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO memory_records VALUES('note','note','project',?1,0)",
            [kind],
        )
        .unwrap();
        db.execute("INSERT INTO memory_revisions VALUES('note',1,?1,?1,?2,'{\"domains\":[],\"paths\":[]}')",params![digest(),seq]).unwrap();
        db.execute(
            "INSERT INTO memory_validity VALUES('note',1,'valid','fixture',NULL,?1)",
            [seq],
        )
        .unwrap();
        db.execute("INSERT INTO memory_heads VALUES('note',1,'active',1)", [])
            .unwrap();
    }

    #[test]
    fn barrier_read_set_rejects_memory_applied_after_freeze() {
        let (_dir, mut db) = open_store();
        let seeded = seed(&db.connection, "alpha", "verify_only");
        optional_head(&db.connection, "observation");
        let member = member_of(&seeded, vec![]);
        let frozen = db.freeze_barrier(&[member.clone()], head_of(&db)).unwrap();
        let seq = head_of(&db);
        db.connection.execute("INSERT INTO memory_delivery_intents VALUES('optional-update','fixture','task:alpha','snap-alpha','alpha','note',1,'informational',?1,'pending')",[seq]).unwrap();
        let update = db
            .memory_update("optional-update", &seeded.attempt)
            .unwrap();
        let mut ack = MemoryUpdateAck {
            schema_version: 1,
            delivery_id: update.delivery_id,
            attempt_id: seeded.attempt.clone(),
            manifest_hash: update.manifest_hash,
            state: "seen".into(),
        };
        db.acknowledge_memory_update(&ack, 1).unwrap();
        ack.state = "applied".into();
        db.acknowledge_memory_update(&ack, 1).unwrap();
        let before = head_of(&db);
        assert!(
            db.release_barrier(&frozen.barrier_id, &frozen.release_token, before, 1)
                .is_err(),
            "newly consumed optional memory released the old barrier"
        );
        assert_eq!(head_of(&db), before);
        let fresh = db.freeze_barrier(&[member], before).unwrap();
        assert_ne!(fresh.barrier_id, frozen.barrier_id);
    }

    #[test]
    fn barrier_read_set_rejects_contract_scope_phantoms() {
        let (_dir, mut db) = open_store();
        let seeded = seed(&db.connection, "alpha", "verify_only");
        let frozen = db
            .freeze_barrier(&[member_of(&seeded, vec![])], head_of(&db))
            .unwrap();
        optional_head(&db.connection, "contract");
        assert_eq!(
            required_generation(&db.connection).unwrap(),
            frozen.required_set_generation
        );
        let before = head_of(&db);
        assert!(
            db.release_barrier(&frozen.barrier_id, &frozen.release_token, before, 1)
                .is_err(),
            "a scope catalog change did not fence release"
        );
        assert_eq!(head_of(&db), before);
    }

    #[test]
    fn barrier_read_set_rejects_task_revision_changes() {
        let (_dir, mut db) = open_store();
        let seeded = seed(&db.connection, "alpha", "verify_only");
        let frozen = db
            .freeze_barrier(&[member_of(&seeded, vec![])], head_of(&db))
            .unwrap();
        db.connection
            .execute("UPDATE tasks SET revision=revision+1 WHERE id='alpha'", [])
            .unwrap();
        let before = head_of(&db);
        assert!(
            db.release_barrier(&frozen.barrier_id, &frozen.release_token, before, 1)
                .is_err(),
            "a changed task revision released frozen membership"
        );
        assert_eq!(head_of(&db), before);
    }

    #[test]
    fn barrier_read_set_ignores_unconsumed_optional_noise() {
        let (_dir, mut db) = open_store();
        let seeded = seed(&db.connection, "alpha", "verify_only");
        let member = member_of(&seeded, vec![]);
        let frozen = db.freeze_barrier(&[member.clone()], head_of(&db)).unwrap();
        optional_head(&db.connection, "observation");
        insert_event(
            &db.connection,
            "fixture",
            "unrelated",
            &serde_json::json!({}),
        )
        .unwrap();
        assert_eq!(db.freeze_barrier(&[member], 0).unwrap(), frozen);
        assert!(
            db.release_barrier(&frozen.barrier_id, &frozen.release_token, head_of(&db), 1)
                .unwrap()
                .released_seq
                .is_some()
        );
    }

    #[test]
    fn barrier_read_set_pins_transitive_sources_and_policy() {
        for mutate in [
            "UPDATE memory_validity SET reason='reevaluated',evaluated_seq=evaluated_seq+1 WHERE record_id='source'",
            "INSERT INTO memory_policies VALUES(1,'{}',lower(hex(zeroblob(32))))",
            "UPDATE project_control SET config_digest=lower(hex(zeroblob(32)))",
            "UPDATE active_work_meta SET incarnation=lower(hex(zeroblob(32)))",
        ] {
            let (_dir, mut db) = open_store();
            let seeded = seed(&db.connection, "alpha", "verify_only");
            optional_head(&db.connection, "observation");
            db.connection.execute("INSERT INTO memory_records SELECT 'source','source',scope_id,kind,is_hard FROM memory_records WHERE id='note'",[]).unwrap();
            db.connection.execute("INSERT INTO memory_revisions SELECT 'source',revision,body_hash,provenance_hash,promoted_seq,applicability FROM memory_revisions WHERE record_id='note'",[]).unwrap();
            db.connection.execute("INSERT INTO memory_validity SELECT 'source',revision,state,reason,expiry_unix_ms,evaluated_seq FROM memory_validity WHERE record_id='note'",[]).unwrap();
            db.connection.execute("INSERT INTO memory_heads SELECT 'source',revision,status,row_revision FROM memory_heads WHERE record_id='note'",[]).unwrap();
            db.connection
                .execute(
                    "INSERT INTO memory_dependencies VALUES('note',1,'source',1,'supports')",
                    [],
                )
                .unwrap();
            db.connection.execute("INSERT INTO snapshot_entries VALUES('snap-alpha',1,'note',1,'optional','fixture')",[]).unwrap();
            let frozen = db
                .freeze_barrier(&[member_of(&seeded, vec![])], head_of(&db))
                .unwrap();
            let payload: String = db
                .connection
                .query_row(
                    "SELECT payload FROM barrier_memory_read_sets WHERE barrier_id=?1",
                    [&frozen.barrier_id],
                    |r| r.get(0),
                )
                .unwrap();
            let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
            assert_eq!(value["schema_version"], 2);
            assert_eq!(value["members"][0]["consumed"].as_array().unwrap().len(), 2);
            assert_eq!(
                value["members"][0]["dependencies"],
                serde_json::json!([["note", 1, "source", 1, "supports"]])
            );
            db.connection.execute_batch(mutate).unwrap();
            let before = head_of(&db);
            assert!(
                db.release_barrier(&frozen.barrier_id, &frozen.release_token, before, 1)
                    .is_err(),
                "{mutate}"
            );
            assert_eq!(head_of(&db), before);
        }
    }

    #[test]
    fn barrier_read_set_publication_rolls_back_and_remains_immutable() {
        let (_dir, mut db) = open_store();
        let seeded = seed(&db.connection, "alpha", "verify_only");
        let member = member_of(&seeded, vec![]);
        db.connection.execute_batch("CREATE TRIGGER fail_barrier_memory BEFORE INSERT ON barrier_memory_read_sets BEGIN SELECT RAISE(ABORT,'injected read-set failure'); END;").unwrap();
        let before = head_of(&db);
        assert!(db.freeze_barrier(&[member.clone()], before).is_err());
        assert_eq!(head_of(&db), before);
        assert_eq!(
            db.connection
                .query_row("SELECT count(*) FROM barrier_revisions", [], |r| r
                    .get::<_, u64>(0))
                .unwrap(),
            0
        );
        db.connection
            .execute_batch("DROP TRIGGER fail_barrier_memory;")
            .unwrap();
        let frozen = db.freeze_barrier(&[member], before).unwrap();
        assert_eq!(frozen.memory_manifest_version, 2);
        assert!(
            db.connection
                .execute("UPDATE barrier_memory_read_sets SET payload=payload", [])
                .is_err()
        );
        assert!(
            db.connection
                .execute("DELETE FROM barrier_memory_read_sets", [])
                .is_err()
        );
    }

    #[test]
    fn barrier_read_set_matches_fixed_version_two_bytes() {
        let (_dir,mut db)=open_store();
        let seeded=seed(&db.connection,"alpha","verify_only");
        db.connection.execute("UPDATE active_work_meta SET incarnation=?1",["aa".repeat(32)]).unwrap();
        let frozen=db.freeze_barrier(&[member_of(&seeded,vec![])],head_of(&db)).unwrap();
        let raw:String=db.connection.query_row("SELECT payload FROM barrier_memory_read_sets WHERE barrier_id=?1",[&frozen.barrier_id],|r|r.get(0)).unwrap();
        assert_eq!(raw.as_bytes(),include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"),"/contracts/factory/barrier-memory-v2.json")));
        assert_eq!(frozen.memory_manifest_digest,include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/contracts/factory/barrier-memory-v2.sha256")).trim());
    }

    #[test]
    fn barrier_read_set_refuses_oversized_payload_before_publication() {
        let (_dir,mut db)=open_store();
        let seeded=seed(&db.connection,"alpha","verify_only");
        optional_head(&db.connection,"observation");
        db.connection.execute("INSERT INTO snapshot_entries VALUES('snap-alpha',1,'note',1,'optional','fixture')",[]).unwrap();
        // Store fault fixture: a single bounded SQL field can still make the
        // complete retained read set too large. Never publish a truncated fence.
        let applicability=serde_json::json!({"domains":["x".repeat(8*1024*1024)],"paths":[]}).to_string();
        db.connection.execute_batch("DROP TRIGGER memory_revisions_no_update;").unwrap();
        db.connection.execute("UPDATE memory_revisions SET applicability=?1 WHERE record_id='note'",[applicability]).unwrap();
        let before=head_of(&db);
        let error=db.freeze_barrier(&[member_of(&seeded,vec![])],before).unwrap_err();
        assert!(matches!(error,StoreError::Limit(ref message) if message.contains("8 MiB")),"{error:?}");
        assert_eq!(head_of(&db),before);
        assert_eq!(db.connection.query_row("SELECT count(*) FROM barrier_revisions",[],|r|r.get::<_,u64>(0)).unwrap(),0);
    }

    fn legacy_barrier(db: &Connection, member: &BarrierMember) -> FrozenBarrier {
        let canonical = canonical_members(db, &[member.clone()],None).unwrap();
        let generation = required_generation(db).unwrap();
        let manifest = memory_manifest_v1(db, generation, &[member.clone()]).unwrap();
        let id = identity(generation, &manifest, &canonical).unwrap();
        let token = release_token(&id).unwrap();
        let sequence = insert_event(
            db,
            "barrier.frozen",
            &id,
            &serde_json::json!({"generation":generation,"members":1}),
        )
        .unwrap();
        db.execute(
            "INSERT INTO barrier_revisions VALUES(?1,?2,?3,?4,NULL,NULL,?5)",
            params![id, generation, manifest, token, sequence],
        )
        .unwrap();
        db.execute(
            "INSERT INTO barrier_members VALUES(?1,0,?2,?3,?4,?5,?6,?7,?8)",
            params![
                id,
                member.task_id,
                member.contract_revision,
                member.attempt_id,
                member.result_id,
                member.verification_id,
                member.integration_id,
                canonical[0].1
            ],
        )
        .unwrap();
        load(db, &id).unwrap().unwrap()
    }

    #[test]
    fn barrier_read_set_upgrade_keeps_legacy_history_but_requires_a_new_freeze() {
        let (_dir, mut db) = open_store();
        let alpha = seed(&db.connection, "alpha", "verify_only");
        let beta = seed(&db.connection, "beta", "verify_only");
        let pending = legacy_barrier(&db.connection, &member_of(&alpha, vec![]));
        let released = legacy_barrier(&db.connection, &member_of(&beta, vec![]));
        let release_seq = insert_event(
            &db.connection,
            "barrier.released",
            &released.barrier_id,
            &serde_json::json!({"token":released.release_token}),
        )
        .unwrap();
        db.connection
            .execute(
                "UPDATE barrier_revisions SET released_seq=?2 WHERE barrier_id=?1",
                params![released.barrier_id, release_seq],
            )
            .unwrap();
        super::super::test_schema::historical(&db.connection, 42).unwrap();
        let before = head_of(&db);
        assert!(matches!(db.freeze_barrier(&[member_of(&alpha, vec![])], before), Err(StoreError::Invalid(message)) if message.contains("fresh post-execution verification")));
        assert_eq!(head_of(&db), before);
        db.upgrade_v1().unwrap();
        assert_eq!(head_of(&db), before);
        assert_eq!(db.connection.query_row("SELECT count(*) FROM barrier_release_authorizations", [], |row| row.get::<_,u64>(0)).unwrap(), 0);
        assert_eq!(db.connection.query_row("SELECT count(*) FROM barrier_release_revocations", [], |row| row.get::<_,u64>(0)).unwrap(), 0);
        assert_eq!(db.connection.query_row("SELECT count(*) FROM barrier_open_memory_unknown", [], |row| row.get::<_,u64>(0)).unwrap(), 2);
        assert_eq!(
            db.connection
                .query_row("SELECT count(*) FROM barrier_memory_read_sets", [], |r| r
                    .get::<_, u64>(
                    0
                ))
                .unwrap(),
            0
        );
        assert_eq!(
            load(&db.connection, &pending.barrier_id).unwrap().unwrap(),
            pending
        );
        let error = db
            .release_barrier(&pending.barrier_id, &pending.release_token, before, 1)
            .unwrap_err();
        assert!(
            matches!(error,StoreError::Invalid(ref message) if message.contains("new freeze")),
            "{error:?}"
        );
        assert_eq!(
            db.release_barrier(&released.barrier_id, &released.release_token, 0, 1)
                .unwrap()
                .released_seq,
            Some(release_seq as u64)
        );
        assert_eq!(head_of(&db), before);
        assert!(db.freeze_barrier(&[member_of(&alpha, vec![])], before).is_err());
        // Upgrade preserves old receipts without inventing post-execution proof.
        // Continue the fresh-freeze checks with current verified evidence.
        let current = seed(&db.connection, "current", "verify_only");
        let fresh = db
            .freeze_barrier(&[member_of(&current, vec![])], head_of(&db))
            .unwrap();
        assert_eq!(fresh.memory_manifest_version, 2);
        assert_ne!(fresh.barrier_id, pending.barrier_id);
        assert!(
            db.release_barrier(&fresh.barrier_id, &pending.release_token, head_of(&db), 1)
                .is_err()
        );
        assert!(
            db.release_barrier(&fresh.barrier_id, &fresh.release_token, head_of(&db), 1)
                .unwrap()
                .released_seq
                .is_some()
        );
        let revoked = db.revoke_barrier(&released.barrier_id, head_of(&db)).unwrap();
        assert_eq!(revoked.released_seq, Some(release_seq as u64));
        assert!(revoked.revoked_seq.is_some());
        let retained: Option<u64> = db.connection.query_row("SELECT released_seq FROM barrier_revisions WHERE barrier_id=?1", [&released.barrier_id], |row| row.get(0)).unwrap();
        assert_eq!(retained, Some(release_seq as u64));
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

    fn downstream_fixture() -> (tempfile::TempDir, SqliteStore, PreparedLaunch, BarrierReleaseReference) {
        downstream_fixture_with_expiry(None)
    }

    fn downstream_fixture_with_expiry(expiry:Option<i64>) -> (tempfile::TempDir, SqliteStore, PreparedLaunch, BarrierReleaseReference) {
        let (dir, mut db, preparations) = super::super::reservations::tests::fixture();
        let mut preparation = preparations.into_iter().find(|p| p.inputs.task.as_str() == "a").unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "# barrier launch fixture\n").unwrap();
        let config = crate::migration::config_reference(&config_path).unwrap();
        db.connection.execute("UPDATE project_control SET config_digest=?1", [&config.digest]).unwrap();
        let scheduler = db.read_snapshot(None).unwrap().scheduler.unwrap().policy;
        db.set_scheduler_policy(head_of(&db), scheduler.revision, 4, 3).unwrap();
        preparation.inputs.scheduler_revision = scheduler.revision + 1;
        let authority = VersionedReference { id: "owner-approval-policy".into(), revision: 1, digest: "ab".repeat(32) };
        let mut profile = crate::domain::profile::fixture(config.clone());
        profile.permission_policy = authority.clone();
        preparation.inputs.profile = profile.reference().unwrap();
        preparation.inputs.effective_profile = Some(profile.clone());
        preparation.inputs.config = config.clone();
        let source = seed(&db.connection, "alpha", "verify_only");
        if let Some(expiry)=expiry {
            optional_head(&db.connection,"observation");
            db.connection.execute("UPDATE memory_validity SET expiry_unix_ms=?1 WHERE record_id='note'",[expiry]).unwrap();
            db.connection.execute("INSERT INTO snapshot_entries VALUES('snap-alpha',1,'note',1,'optional','fixture')",[]).unwrap();
        }
        let frozen = db.freeze_barrier(&[member_of(&source, vec![])], head_of(&db)).unwrap();
        let mut document = db.draft_barrier_release(&frozen.barrier_id, authority.clone(), config.digest.as_deref().unwrap(), jiff::Timestamp::now().as_millisecond()+60_000).unwrap();
        // This unit fixture models previously authenticated bytes. The service
        // signature test above separately exercises the real owner key boundary.
        document.issued_unix_ms = 0;
        let authorized = PreparedBarrierRelease::parse_verified(&serde_json::to_vec(&document).unwrap()).unwrap();
        let released = db.release_authorized_barrier(&authorized).unwrap();
        let reference = BarrierReleaseReference { schema_version: 1, barrier_id: released.barrier_id, release_sequence: released.released_seq.unwrap(), authorization_digest: authorized.digest };
        assert_eq!(released.release_reference, Some(reference.clone()));
        assert_eq!(db.frozen_barrier(&reference.barrier_id).unwrap().unwrap().release_reference, Some(reference.clone()));
        if expiry.is_some() {
            let mut plan=SnapshotPlan {
                coordinator:false,session_id:None,request:SnapshotRequest {schema_version:1,task_id:"a".into(),profile:profile.name.clone(),domains:vec![],paths:vec![],pinned_keys:vec![],sensitivity:"default".into()},
                profile_name:profile.name.clone(),profile_digest:profile.definition_digest.clone(),config_digest:config.digest.clone(),budget_chars:32000,estimator:SELECTION_ESTIMATOR.into(),instructions:"Retained instructions".into(),now_unix_ms:1000,expected_heads_digest:None,
            };
            // A valid child launch retains its own snapshot. Deliberately spend
            // its budget on required framing so the optional source is consumed
            // only by the ancestor, not by this child.
            let selected=db.create_memory_snapshot(plan.clone()).unwrap();
            plan.budget_chars=selected.required_bytes;
            let snapshot=db.create_memory_snapshot(plan).unwrap();assert!(snapshot.entries.is_empty());
            preparation.inputs.memory=Some(VersionedReference{id:snapshot.id.as_str().into(),revision:1,digest:snapshot.manifest_hash});
        }
        let mut contract: serde_json::Value = serde_json::from_slice(&contract_bytes("a", "verify_only", 1)).unwrap();
        contract["version"] = 2.into();
        if let Some(memory)=&preparation.inputs.memory {contract["memory_snapshot_id"]=memory.id.clone().into();}
        contract["required_barrier"] = serde_json::to_value(&reference).unwrap();
        contract["project_store"] = preparation.inputs.project_store.clone().into();
        contract["repository"] = std::fs::canonicalize(dir.path()).unwrap().to_str().unwrap().into();
        contract["expected_head"] = head_of(&db).into();
        contract["profile_kind"] = profile.kind.into();
        db.install_contract(&PreparedContract::parse_verified(&serde_json::to_vec(&contract).unwrap()).unwrap()).unwrap();
        preparation.inputs.task_contract = db.task_contract_reference("a").unwrap();
        let grant = ApprovalGrant { version: 1, scope: ApprovalScope::for_launch(&preparation.inputs).unwrap(), policy: authority, issued_unix_ms: 0, expires_unix_ms: 100_000 };
        preparation.inputs.approval = db.install_approval(&PreparedApproval { grant }, head_of(&db), 1000).unwrap();
        (dir, db, preparation, reference)
    }

    #[test]
    fn downstream_barrier_requires_exact_authorization_and_current_evidence() {
        for fault in ["missing", "digest", "sequence", "authority", "config", "member_revision", "control"] {
            let (_dir, db, preparation, mut reference) = downstream_fixture();
            let mut authority = preparation.inputs.effective_profile.as_ref().unwrap().permission_policy.clone();
            let mut config = preparation.inputs.config.digest.clone();
            require_current_release(&db.connection, &reference, &authority, config.as_deref(), 1000, None).unwrap();
            match fault {
                "missing" => reference.barrier_id = "ff".repeat(32),
                "digest" => reference.authorization_digest = "ff".repeat(32),
                "sequence" => reference.release_sequence += 1,
                "authority" => authority.digest = "ff".repeat(32),
                "config" => config = Some("ff".repeat(32)),
                "member_revision" => { db.connection.execute("UPDATE tasks SET revision=revision+1 WHERE id='alpha'", []).unwrap(); },
                _ => { db.connection.execute("UPDATE project_control SET revision=revision+1", []).unwrap(); },
            }
            assert!(require_current_release(&db.connection, &reference, &authority, config.as_deref(), 1000, None).is_err(), "{fault}");
        }
        let (_dir, mut db, prepared) = authorized_fixture();
        let frozen = db.frozen_barrier(&prepared.document.barrier_id).unwrap().unwrap();
        let unsigned = db.release_barrier(&frozen.barrier_id, &frozen.release_token, head_of(&db), 1000).unwrap();
        assert!(unsigned.release_reference.is_none());
        let reference = BarrierReleaseReference { schema_version: 1, barrier_id: unsigned.barrier_id, release_sequence: unsigned.released_seq.unwrap(), authorization_digest: prepared.digest };
        assert!(require_current_release(&db.connection, &reference, &prepared.document.authority, Some(&prepared.document.config_digest), 1000, None).is_err());
    }

    #[test]
    fn downstream_barrier_preserves_shared_budget_and_sql_progress_handler() {
        use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
        let (_dir, db, preparation, reference) = downstream_fixture();
        let authority = &preparation.inputs.effective_profile.as_ref().unwrap().permission_policy;
        let cancellation = crate::runner::Cancellation::default();
        let control = controlled::ReadControl::new(std::time::Instant::now()+Duration::from_secs(10), cancellation.clone());
        let budget = read_budget::ReadBudget::new(control);
        let calls = Arc::new(AtomicUsize::new(0));
        let callback_calls = calls.clone();
        db.connection.progress_handler(1, Some(move || { callback_calls.fetch_add(1, Ordering::Relaxed); false }));
        let initial = budget.remaining_units();
        require_current_release(&db.connection, &reference, authority, preparation.inputs.config.digest.as_deref(), 1000, Some(&budget)).unwrap();
        assert!(budget.remaining_units() < initial);
        let previous_calls = calls.load(Ordering::Relaxed);
        db.connection.query_row("SELECT count(*) FROM tasks", [], |row| row.get::<_,u64>(0)).unwrap();
        assert!(calls.load(Ordering::Relaxed) > previous_calls, "nested barrier read replaced the caller's SQL progress handler");
        cancellation.cancel();
        assert!(matches!(require_current_release(&db.connection, &reference, authority, preparation.inputs.config.digest.as_deref(), 1000, Some(&budget)), Err(StoreError::Cancelled)));
    }

    fn downstream_result_fixture() -> (tempfile::TempDir, SqliteStore, Reservation, BarrierReleaseReference, serde_json::Value) {
        downstream_result_fixture_with_expiry(None)
    }

    fn downstream_result_fixture_with_expiry(expiry:Option<i64>) -> (tempfile::TempDir, SqliteStore, Reservation, BarrierReleaseReference, serde_json::Value) {
        let (dir, mut db, preparation, reference) = downstream_fixture_with_expiry(expiry);
        let reservation = db.reserve_prepared(&[preparation.clone()], head_of(&db), 1000).unwrap();
        let candidate = "c".repeat(40);
        let relative = format!("{}/{}", &candidate[..2], &candidate[2..]);
        let object = dir.path().join(".git/objects").join(&relative);
        std::fs::create_dir_all(object.parent().unwrap()).unwrap();
        // Submission stores opaque bytes; these are not verified Git evidence.
        std::fs::write(object, b"untrusted candidate fixture").unwrap();
        let base = "b".repeat(40);
        let base_relative = format!("{}/{}", &base[..2], &base[2..]);
        let base_object = dir.path().join(".git/objects").join(&base_relative);
        std::fs::create_dir_all(base_object.parent().unwrap()).unwrap();
        std::fs::write(base_object, b"untrusted base fixture").unwrap();
        let mut submission = serde_json::json!({
            "idempotency_key":"before-revocation", "task_id":"a", "contract_revision":1,
            "contract_digest":preparation.inputs.task_contract.as_ref().unwrap().digest,
            "attempt_id":reservation.record.attempt.as_str(),
            "repository":std::fs::canonicalize(dir.path()).unwrap().to_str().unwrap(),
            "base_oid":"b".repeat(40), "candidate_oid":candidate, "object_format":"sha1",
            "artifact_manifest":[], "claimed_checks":[], "objects":[{"oid":candidate,"relative_path":relative},{"oid":base,"relative_path":base_relative}]
        });
        if let Some(memory)=&reservation.record.inputs.memory {submission["memory_snapshot_id"]=memory.id.clone().into();}
        (dir, db, reservation, reference, submission)
    }

    #[test]
    fn downstream_barrier_revocation_blocks_new_result_submission_but_preserves_history() {
        let (_dir, mut db, reservation, reference, mut submission) = downstream_result_fixture();
        let original = serde_json::to_vec(&submission).unwrap();
        db.submit_result(&original).unwrap();
        db.revoke_barrier(&reference.barrier_id, head_of(&db)).unwrap();
        let before = db.read_snapshot(None).unwrap();
        assert!(db.submit_result(&original).unwrap().replayed);
        submission["idempotency_key"] = "after-revocation".into();
        let error = db.submit_result(&serde_json::to_vec(&submission).unwrap()).unwrap_err();
        assert!(error.to_string().contains("barrier"), "{error}");
        assert_eq!(db.read_snapshot(None).unwrap(), before);
        assert_eq!(db.connection.query_row("SELECT count(*) FROM result_submissions WHERE task_id='a'", [], |row| row.get::<_,u64>(0)).unwrap(), 1);
        assert!(before.attempts.iter().find(|a| a.id == reservation.record.attempt).unwrap().retains_capacity());
    }

    #[test]
    fn downstream_attempt_cannot_drop_its_barrier_by_using_a_later_v1_contract() {
        for terminated in [false, true] {
            let (_dir, mut db, reservation, _reference, mut submission) = downstream_result_fixture();
            let raw: Vec<u8> = db.connection.query_row("SELECT raw_bytes FROM task_contracts WHERE task_id='a' AND contract_revision=1", [], |row| row.get(0)).unwrap();
            let mut replacement: serde_json::Value = serde_json::from_slice(&raw).unwrap();
            replacement["version"] = 1.into();
            replacement["contract_revision"] = 2.into();
            replacement["expected_head"] = head_of(&db).into();
            replacement.as_object_mut().unwrap().remove("required_barrier");
            let replacement = PreparedContract::parse_verified(&serde_json::to_vec(&replacement).unwrap()).unwrap();
            db.install_contract(&replacement).unwrap();
            if terminated {
                db.connection.execute("UPDATE attempts SET termination_observed=1 WHERE id=?1", [reservation.record.attempt.as_str()]).unwrap();
            }
            submission["contract_revision"] = 2.into();
            submission["contract_digest"] = replacement.digest.clone().into();
            let before = db.read_snapshot(None).unwrap();
            let result = db.submit_result(&serde_json::to_vec(&submission).unwrap());
            assert!(result.is_err(), "reserved barrier was erased by a replacement contract: {result:?}");
            assert_eq!(db.read_snapshot(None).unwrap(), before);
            assert!(super::super::contract_binding::require_result_barrier(&db.connection, "a", 2, &replacement.digest, reservation.record.attempt.as_str(), 1001).is_err());
        }
    }

    #[test]
    fn downstream_result_cannot_borrow_an_unreserved_attempt() {
        let (_dir, mut db, reservation, _reference, mut submission) = downstream_result_fixture();
        db.connection.execute("INSERT INTO attempts(id,task_id,revision,state,snapshot,reservation,termination_observed) VALUES('unreserved-result','a',1,'running',NULL,'unreserved-slot',0)", []).unwrap();
        submission["attempt_id"] = "unreserved-result".into();
        let before = db.read_snapshot(None).unwrap();
        let error = db.submit_result(&serde_json::to_vec(&submission).unwrap()).unwrap_err();
        assert!(error.to_string().contains("reserved launch inputs"), "{error}");
        assert_eq!(db.read_snapshot(None).unwrap(), before);
        assert_eq!(db.connection.query_row("SELECT count(*) FROM result_submissions WHERE task_id='a'", [], |row| row.get::<_,u64>(0)).unwrap(), 0);
        assert!(before.attempts.iter().find(|a| a.id == reservation.record.attempt).unwrap().retains_capacity());
    }

    #[test]
    fn revoked_ancestor_blocks_result_dependencies_and_later_wave_reuse() {
        for cause in ["direct", "memory_invalidation", "memory_policy"] {
        let (_dir, mut db, reservation, ancestor, submission) = downstream_result_fixture();
        // As with the source fixture, seed an empty consumed memory snapshot.
        db.connection.execute("INSERT INTO memory_snapshots(id,task_id,task_revision,profile_name,profile_digest,config_digest,selection_policy_version,estimator,sequence,required_bytes,optional_bytes,budget_bytes,omitted_optional_count,manifest_hash,scope_digest) SELECT 'snap-a','a',3,profile_name,profile_digest,config_digest,selection_policy_version,estimator,sequence,required_bytes,optional_bytes,budget_bytes,omitted_optional_count,manifest_hash,scope_digest FROM memory_snapshots WHERE id='snap-alpha'", []).unwrap();
        db.connection.execute("UPDATE attempts SET snapshot='snap-a' WHERE id=?1", [reservation.record.attempt.as_str()]).unwrap();
        let submitted = db.submit_result(&serde_json::to_vec(&submission).unwrap()).unwrap();
        let target = db.load_verify_target(&submitted.submission_id, "policy-1").unwrap();
        let payload = "ed".repeat(32);
        let tree = "d".repeat(40);
        let receipt = crate::verification::testing_receipt(&target, &payload, "ancestor-check", &target.candidate_oid, &tree);
        let (run, receipt) = db.commit_verification(&target, super::super::verification::RunDraft {
            idempotency_key: "ancestor-check".into(), payload_digest: payload, argv: vec![], libraries: vec![],
            tree_oid: Some(tree), exit_status: Some(0), reason: None, receipt: Some(receipt),
        }).unwrap();
        let result = receipt.unwrap().result_id().to_string();
        db.connection.execute("INSERT INTO task_dependencies(task_id,predecessor_id,requirement) VALUES('b','a','verified_result')", []).unwrap();
        super::super::satisfaction::record_verified_result(&db.connection, &result).unwrap();
        let predecessor = db.read_snapshot(None).unwrap().tasks.into_iter().find(|t| t.id.as_str() == "a").unwrap();
        assert!(super::super::satisfaction::dependency_blocker(&db.connection, "b", &predecessor, DependencyRequirement::VerifiedResult, true).unwrap().is_none());
        let frozen = db.freeze_barrier(&[BarrierMember {
            task_id: "a".into(), contract_revision: 1, attempt_id: reservation.record.attempt.as_str().into(),
            result_id: result.clone(), verification_id: run.run_id, integration_id: None, proposal_dispositions: vec![],
        }], head_of(&db)).unwrap();
        let authority = reservation.record.inputs.effective_profile.as_ref().unwrap().permission_policy.clone();
        let config = reservation.record.inputs.config.digest.as_deref().unwrap();
        let document = db.draft_barrier_release(&frozen.barrier_id, authority.clone(), config, jiff::Timestamp::now().as_millisecond()+60_000).unwrap();
        let prepared = PreparedBarrierRelease::parse_verified(&serde_json::to_vec(&document).unwrap()).unwrap();
        let later = db.release_authorized_barrier(&prepared).unwrap().release_reference.unwrap();
        require_current_release(&db.connection, &later, &authority, Some(config), 1000, None).unwrap();
        let cancellation = crate::runner::Cancellation::default();
        let budget = read_budget::ReadBudget::new(controlled::ReadControl::new(std::time::Instant::now()+Duration::from_secs(10), cancellation.clone()));
        require_current_release(&db.connection, &later, &authority, Some(config), 1000, Some(&budget)).unwrap();
        let remaining = budget.remaining_units();
        require_current_release(&db.connection, &later, &authority, Some(config), 1000, Some(&budget)).unwrap();
        assert!(budget.remaining_units() < remaining, "ancestor reads must consume the original budget");
        cancellation.cancel();
        assert!(matches!(require_current_release(&db.connection, &later, &authority, Some(config), 1000, Some(&budget)), Err(StoreError::Cancelled)));
        // Model another exact reserved consumer and accepted result. Its signed
        // prerequisite is the later wave, so the next freeze has two ancestors.
        // This models store protocol evidence, not a live worker launch.
        let raw: Vec<u8> = db.connection.query_row("SELECT raw_bytes FROM task_contracts WHERE task_id='a' AND contract_revision=1", [], |row| row.get(0)).unwrap();
        let mut contract: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        contract["task_id"] = "b".into();
        contract["expected_head"] = head_of(&db).into();
        contract["required_barrier"] = serde_json::to_value(&later).unwrap();
        contract["dependencies"] = serde_json::json!([{"predecessor":"a","edge":"verified_result","policy_id":"policy-1"}]);
        let contract = PreparedContract::parse_verified(&serde_json::to_vec(&contract).unwrap()).unwrap();
        db.install_contract(&contract).unwrap();
        let mut source = reservation.record.clone();
        source.inputs.task = TaskId::new("b").unwrap();
        source.inputs.task_contract = db.task_contract_reference("b").unwrap();
        let consumer = synthetic_barrier_consumer(&db.connection, &source, 0, false).unwrap();
        db.connection.execute("UPDATE tasks SET active_attempt=?1,revision=101 WHERE id='b'", [consumer.attempt.as_str()]).unwrap();
        db.connection.execute("INSERT INTO memory_snapshots(id,task_id,task_revision,profile_name,profile_digest,config_digest,selection_policy_version,estimator,sequence,required_bytes,optional_bytes,budget_bytes,omitted_optional_count,manifest_hash,scope_digest) SELECT 'snap-b','b',101,profile_name,profile_digest,config_digest,selection_policy_version,estimator,sequence,required_bytes,optional_bytes,budget_bytes,omitted_optional_count,manifest_hash,scope_digest FROM memory_snapshots WHERE id='snap-a'", []).unwrap();
        db.connection.execute("UPDATE attempts SET snapshot='snap-b' WHERE id=?1", [consumer.attempt.as_str()]).unwrap();
        let mut downstream_submission = submission.clone();
        downstream_submission["task_id"] = "b".into();
        downstream_submission["attempt_id"] = consumer.attempt.as_str().into();
        downstream_submission["contract_digest"] = contract.digest.clone().into();
        downstream_submission["idempotency_key"] = "grandchild-submission".into();
        let submitted = db.submit_result(&serde_json::to_vec(&downstream_submission).unwrap()).unwrap();
        let target = db.load_verify_target(&submitted.submission_id, "policy-1").unwrap();
        let receipt = crate::verification::testing_receipt(&target, &"ed".repeat(32), "grandchild-check", &target.candidate_oid, &"d".repeat(40));
        let (run, receipt) = db.commit_verification(&target, super::super::verification::RunDraft {
            idempotency_key: "grandchild-check".into(), payload_digest: "ed".repeat(32), argv: vec![], libraries: vec![],
            tree_oid: Some("d".repeat(40)), exit_status: Some(0), reason: None, receipt: Some(receipt),
        }).unwrap();
        let grandchild = db.freeze_barrier(&[BarrierMember {
            task_id: "b".into(), contract_revision: 1, attempt_id: consumer.attempt.as_str().into(),
            result_id: receipt.unwrap().result_id().to_string(), verification_id: run.run_id,
            integration_id: None, proposal_dispositions: vec![],
        }], head_of(&db)).unwrap();
        assert_eq!(db.connection.query_row("SELECT count(*) FROM barrier_ancestors WHERE barrier_id=?1", [&grandchild.barrier_id], |row| row.get::<_,u64>(0)).unwrap(), 2);
        let mut members = frozen.members.clone();
        members.extend(db.frozen_barrier(&ancestor.barrier_id).unwrap().unwrap().members);
        let pending = db.freeze_barrier(&members, head_of(&db)).unwrap();
        let mut pending_document = db.draft_barrier_release(&pending.barrier_id, authority.clone(), config, jiff::Timestamp::now().as_millisecond()+60_000).unwrap();
        assert!(db.connection.execute("DELETE FROM barrier_open_descendants", []).is_err());
        assert!(db.connection.execute("UPDATE barrier_ancestors SET ancestor_id=ancestor_id", []).is_err());
        assert!(db.connection.execute("DELETE FROM barrier_ancestors", []).is_err());
        let before = db.read_snapshot(None).unwrap();
        db.connection.execute_batch("CREATE TRIGGER fail_descendant BEFORE INSERT ON barrier_ancestor_invalidations BEGIN SELECT RAISE(ABORT,'injected descendant failure'); END;").unwrap();
        assert!(db.revoke_barrier(&ancestor.barrier_id, before.head).is_err());
        assert_eq!(db.read_snapshot(None).unwrap(), before);
        assert!(db.frozen_barrier(&ancestor.barrier_id).unwrap().unwrap().revoked_seq.is_none());
        assert!(db.frozen_barrier(&later.barrier_id).unwrap().unwrap().revoked_seq.is_none());
        db.connection.execute_batch("DROP TRIGGER fail_descendant").unwrap();
        match cause {
            "direct" => { db.revoke_barrier(&ancestor.barrier_id, head_of(&db)).unwrap(); },
            "memory_invalidation" => { db.connection.execute("INSERT INTO memory_invalidations VALUES('ancestor-global',NULL,'cause',NULL,'stop_at_checkpoint',?1,NULL,'global change')", [head_of(&db)]).unwrap(); },
            _ => { let tx = db.connection.transaction().unwrap(); memory_changed(&tx, None, before.head).unwrap(); tx.commit().unwrap(); },
        }
        assert!(db.frozen_barrier(&later.barrier_id).unwrap().unwrap().revoked_seq.is_some(), "released descendant has no durable invalidation");
        assert!(db.frozen_barrier(&pending.barrier_id).unwrap().unwrap().revoked_seq.is_some(), "pending descendant has no durable invalidation");
        assert!(db.frozen_barrier(&grandchild.barrier_id).unwrap().unwrap().revoked_seq.is_some(), "transitive descendant has no durable invalidation");
        assert!(db.memory_readiness("b", 1001).unwrap().blockers.iter().any(|b| b.kind == "required_barrier_revoked" && b.id == later.barrier_id));
        assert_eq!(db.read_snapshot(None).unwrap().attempts, before.attempts);
        assert_eq!(db.connection.query_row("SELECT count(*) FROM barrier_open_descendants", [], |row| row.get::<_,u64>(0)).unwrap(), 0);
        if cause == "direct" {
            assert_eq!(db.connection.query_row("SELECT count(*) FROM barrier_ancestor_invalidations WHERE ancestor_id=?1", [&ancestor.barrier_id], |row| row.get::<_,u64>(0)).unwrap(), 3);
            assert!(db.connection.execute("UPDATE barrier_ancestor_invalidations SET sequence=sequence", []).is_err());
            assert!(db.connection.execute("DELETE FROM barrier_ancestor_invalidations", []).is_err());
        }
        let mut reopened = SqliteStore::open(&_dir.path().join(".state/state.db")).unwrap();
        assert!(reopened.frozen_barrier(&grandchild.barrier_id).unwrap().unwrap().revoked_seq.is_some());
        assert!(reopened.memory_readiness("b", 1001).unwrap().blockers.iter().any(|b| b.kind == "required_barrier_revoked"));
        let dependency_blocked = super::super::satisfaction::dependency_blocker(&db.connection, "b", &predecessor, DependencyRequirement::VerifiedResult, true).unwrap().is_some();
        let wave_blocked = require_current_release(&db.connection, &later, &authority, Some(config), 1000, None).is_err();
        assert!(dependency_blocked && wave_blocked, "revoked ancestor: dependency blocked={dependency_blocked}, later wave blocked={wave_blocked}");
        assert!(db.freeze_barrier(&frozen.members, head_of(&db)).is_err());
        assert!(db.draft_barrier_release(&pending.barrier_id, authority.clone(), config, jiff::Timestamp::now().as_millisecond()+60_000).is_err());
        // Model a newly authenticated request at the current head: rejection
        // must come from ancestry, independently of the optimistic event fence.
        pending_document.expected_head = head_of(&db);
        let pending_authorization = PreparedBarrierRelease::parse_verified(&serde_json::to_vec(&pending_document).unwrap()).unwrap();
        let error = db.release_authorized_barrier(&pending_authorization).unwrap_err();
        assert!(matches!(error, StoreError::Conflict) || error.to_string().contains("barrier"), "{error}");
        let before = head_of(&db);
        super::super::satisfaction::record_verified_result(&db.connection, &result).unwrap();
        assert_eq!(head_of(&db), before);
        assert_eq!(db.release_authorized_barrier(&prepared).unwrap().release_reference, Some(later), "historical replay must preserve the original release");
        }
    }

    #[test]
    fn descendant_routing_excludes_revoked_history_and_bounds_freeze_fanout() {
        use rusqlite::StatementStatus;
        // Synthetic copies exercise projection population only. Their source
        // membership is a real accepted store receipt with exact launch inputs;
        // the copied barrier IDs are not claims of canonical freeze identities.
        fn copy(db: &Connection, source: &str, id: &str) -> Result<()> {
            db.execute("INSERT INTO barrier_revisions SELECT ?1,required_set_generation,memory_manifest_digest,?1,NULL,NULL,created_seq FROM barrier_revisions WHERE barrier_id=?2", params![id,source])?;
            db.execute("INSERT INTO barrier_members SELECT ?1,position,task_id,contract_revision,attempt_id,result_id,verification_id,integration_id,proposal_dispositions FROM barrier_members WHERE barrier_id=?2", params![id,source])?;
            Ok(())
        }
        let mut work = Vec::new();
        for history in [0, 10_000] {
            let (_dir, mut db, reservation, ancestor, submission) = downstream_result_fixture();
            db.connection.execute("INSERT INTO memory_snapshots(id,task_id,task_revision,profile_name,profile_digest,config_digest,selection_policy_version,estimator,sequence,required_bytes,optional_bytes,budget_bytes,omitted_optional_count,manifest_hash,scope_digest) SELECT 'snap-a','a',3,profile_name,profile_digest,config_digest,selection_policy_version,estimator,sequence,required_bytes,optional_bytes,budget_bytes,omitted_optional_count,manifest_hash,scope_digest FROM memory_snapshots WHERE id='snap-alpha'", []).unwrap();
            db.connection.execute("UPDATE attempts SET snapshot='snap-a' WHERE id=?1", [reservation.record.attempt.as_str()]).unwrap();
            let submitted = db.submit_result(&serde_json::to_vec(&submission).unwrap()).unwrap();
            let target = db.load_verify_target(&submitted.submission_id, "policy-1").unwrap();
            let receipt = crate::verification::testing_receipt(&target, &"ed".repeat(32), "descendant-scale", &target.candidate_oid, &"d".repeat(40));
            let (run, receipt) = db.commit_verification(&target, super::super::verification::RunDraft {
                idempotency_key: "descendant-scale".into(), payload_digest: "ed".repeat(32), argv: vec![], libraries: vec![],
                tree_oid: Some("d".repeat(40)), exit_status: Some(0), reason: None, receipt: Some(receipt),
            }).unwrap();
            let frozen = db.freeze_barrier(&[BarrierMember {
                task_id: "a".into(), contract_revision: 1, attempt_id: reservation.record.attempt.as_str().into(),
                result_id: receipt.unwrap().result_id().to_string(), verification_id: run.run_id,
                integration_id: None, proposal_dispositions: vec![],
            }], head_of(&db)).unwrap();
            {
                let tx = db.connection.transaction().unwrap();
                for index in 0..history {
                    let id = format!("{:064x}", index+1);
                    copy(&tx, &frozen.barrier_id, &id).unwrap();
                    insert_event(&tx, "barrier.revoked", &id, &serde_json::json!({})).unwrap();
                }
                tx.commit().unwrap();
            }
            assert_eq!(db.connection.query_row("SELECT count(*) FROM barrier_open_descendants", [], |row| row.get::<_,u64>(0)).unwrap(), 1);
            if history == 0 {
                let tx = db.connection.transaction().unwrap();
                for index in 1..=999 { copy(&tx, &frozen.barrier_id, &format!("{index:064x}")).unwrap(); }
                tx.commit().unwrap();
                let before = head_of(&db);
                {
                    let tx = db.connection.transaction().unwrap();
                    assert!(matches!(copy(&tx, &frozen.barrier_id, &format!("{:064x}", 1000)), Err(StoreError::Conflict)));
                }
                assert_eq!(head_of(&db), before);
                assert_eq!(db.connection.query_row("SELECT count(*) FROM barrier_ancestors", [], |row| row.get::<_,u64>(0)).unwrap(), 1000);
                db.revoke_barrier(&format!("{:064x}", 1), head_of(&db)).unwrap();
                let tx = db.connection.transaction().unwrap();
                copy(&tx, &frozen.barrier_id, &format!("{:064x}", 1000)).unwrap();
                tx.commit().unwrap();
                // Exercise the entire admitted fanout, then restore it solely
                // to keep the later SQL-work comparison at one live descendant.
                db.connection.execute_batch("SAVEPOINT fanout").unwrap();
                insert_event(&db.connection, "barrier.revoked", &ancestor.barrier_id, &serde_json::json!({})).unwrap();
                assert_eq!(db.connection.query_row("SELECT count(*) FROM barrier_ancestor_invalidations", [], |row| row.get::<_,u64>(0)).unwrap(), 1000);
                assert_eq!(db.connection.query_row("SELECT count(*) FROM barrier_open_descendants", [], |row| row.get::<_,u64>(0)).unwrap(), 0);
                assert_eq!(db.connection.query_row("SELECT count(*) FROM attempt_barrier_invalidations", [], |row| row.get::<_,u64>(0)).unwrap(), 1);
                db.connection.execute_batch("ROLLBACK TO fanout; RELEASE fanout").unwrap();
                // Restore one active descendant for the history-work comparison.
                for index in 2..=1000 { db.revoke_barrier(&format!("{index:064x}"), head_of(&db)).unwrap(); }
            }
            let mut query = db.connection.prepare("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('barrier.revoked',?1,1,1,'{}')").unwrap();
            query.execute([&ancestor.barrier_id]).unwrap();
            work.push((query.get_status(StatementStatus::VmStep),query.get_status(StatementStatus::FullscanStep)));
            drop(query);
            assert!(db.frozen_barrier(&frozen.barrier_id).unwrap().unwrap().revoked_seq.is_some());
            assert_eq!(db.connection.query_row("SELECT count(*) FROM barrier_open_descendants", [], |row| row.get::<_,u64>(0)).unwrap(), 0);
            assert_eq!(db.connection.query_row("SELECT count(*) FROM barrier_ancestor_invalidations", [], |row| row.get::<_,u64>(0)).unwrap(), 1);
        }
        eprintln!("descendant invalidation SQL work with 1000/10000 revoked descendants: {work:?}");
        // The independently generated stores can differ by a bookkeeping VM
        // step. Bound actual work at both sizes: 2,000 steps cannot conceal a
        // row-by-row traversal of the 10,000 retained descendants, even through
        // an index (which would not appear as FullscanStep).
        for (steps, scans) in work {
            assert!(steps <= 2_000, "descendant invalidation exceeded its SQL-work bound: {steps}");
            assert_eq!(scans, 0);
        }
    }

    #[test]
    fn downstream_barrier_is_rechecked_before_verification_and_acceptance() {
        for fault in ["none", "none_claim", "revoked", "member_revision", "config_file"] {
            let (dir, mut db, reservation, reference, submission) = downstream_result_fixture();
            let submitted = db.submit_result(&serde_json::to_vec(&submission).unwrap()).unwrap();
            let target = db.load_verify_target(&submitted.submission_id, "policy-1").unwrap();
            let key = "barrier-verification";
            let payload = "ed".repeat(32);
            let tree = "d".repeat(40);
            // Store-boundary unit evidence, not a claim of verifier isolation.
            let receipt = crate::verification::testing_receipt(&target, &payload, key, &target.candidate_oid, &tree);
            match fault {
                "revoked" => { db.revoke_barrier(&reference.barrier_id, head_of(&db)).unwrap(); },
                "member_revision" => { db.connection.execute("UPDATE tasks SET revision=revision+1 WHERE id='alpha'", []).unwrap(); },
                "config_file" => { std::fs::write(dir.path().join("config.toml"), "# changed after verification started\n").unwrap(); },
                _ => {},
            }
            let before = db.read_snapshot(None).unwrap();
            let draft = super::super::verification::RunDraft {
                idempotency_key: key.into(), payload_digest: payload, argv: vec![], libraries: vec![],
                tree_oid: Some(tree), exit_status: Some(0), reason: None, receipt: Some(receipt),
            };
            let result = db.commit_verification(&target, draft);
            if fault.starts_with("none") {
                let (run, receipt) = result.unwrap();
                assert_eq!(run.state, "accepted");
                let result_id = receipt.unwrap().result_id().to_string();
                let verified = db.load_verified_for_integration(&result_id).unwrap();
                let repository = verified.repository.clone();
                db.configure_integration_ref(&repository, "refs/heads/barrier-test").unwrap();
                let begin = super::super::integration::IntegrationBegin {
                    repository, ref_name: "refs/heads/barrier-test".into(), expected_old_oid: "b".repeat(40),
                    verified, idempotency_key: "barrier-integration".into(), payload_digest: "ea".repeat(32),
                };
                let operation = db.begin_integration(&begin).unwrap();
                let operation = OperationId::new(operation).unwrap();
                let claim = db.claim_operation(&operation, 1, "integrator", 1000, 1000).unwrap();
                db.record_candidate(&claim, &"e".repeat(40), &"d".repeat(40), &target.candidate_oid, 1000).unwrap();
                db.mark_checks_passed(&claim, 1000).unwrap();
                db.mark_publish_attempted(&claim, 1000).unwrap();
                db.revoke_barrier(&reference.barrier_id, head_of(&db)).unwrap();
                let before = db.read_snapshot(None).unwrap();
                assert!(db.load_verified_for_integration(&result_id).is_err());
                let error = db.begin_integration(&begin).unwrap_err();
                assert!(error.to_string().contains("barrier"), "{error}");
                let error = db.mark_publish_attempted(&claim, 1001).unwrap_err();
                assert!(error.to_string().contains("barrier"), "{error}");
                assert_eq!(db.read_snapshot(None).unwrap(), before);
                assert_eq!(db.connection.query_row("SELECT count(*) FROM verified_results WHERE result_id=?1", [&result_id], |row| row.get::<_,u64>(0)).unwrap(), 1);
                // Model the trusted broker observing its candidate at the ref
                // after revocation raced the external update. Observation must
                // survive without treating that publication as applicable work.
                let finish = |db: &mut SqliteStore| {
                    if fault == "none_claim" {
                        db.finish_integration(&claim, super::super::integration::IntegrationFinish::Confirm, 1001)
                    } else {
                        db.finish_integration_observed(operation.as_str(), super::super::integration::IntegrationFinish::Confirm, 1001)
                    }
                };
                db.connection.execute_batch("CREATE TRIGGER reject_stale_observation BEFORE INSERT ON events WHEN NEW.kind='integration.observed_stale_publication' BEGIN SELECT RAISE(ABORT,'injected observation failure'); END;").unwrap();
                assert!(finish(&mut db).is_err());
                assert_eq!(db.read_snapshot(None).unwrap(), before);
                assert_eq!(db.connection.query_row("SELECT state FROM integration_operations WHERE operation_id=?1", [operation.as_str()], |row| row.get::<_,String>(0)).unwrap(), "validating");
                db.connection.execute_batch("DROP TRIGGER reject_stale_observation").unwrap();
                finish(&mut db).unwrap();
                let (state, reason): (String, Option<String>) = db.connection.query_row("SELECT state,reason FROM integration_operations WHERE operation_id=?1", [operation.as_str()], |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
                assert_eq!(state, "reconciliation_required");
                assert_eq!(reason.as_deref(), Some("required_barrier_changed_after_publish"));
                assert_eq!(db.connection.query_row("SELECT count(*) FROM integrated_commits WHERE operation_id=?1", [operation.as_str()], |row| row.get::<_,u64>(0)).unwrap(), 0);
                assert_eq!(db.connection.query_row("SELECT state FROM operation_delivery WHERE operation_id=?1", [operation.as_str()], |row| row.get::<_,String>(0)).unwrap(), "ambiguous");
                assert_eq!(db.read_snapshot(None).unwrap().attempts, before.attempts);
                let reopened = SqliteStore::open(&dir.path().join(".state/state.db")).unwrap();
                let payload: String = reopened.connection.query_row("SELECT payload FROM events WHERE kind='integration.observed_stale_publication' AND entity=?1", [operation.as_str()], |row| row.get(0)).unwrap();
                let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
                assert_eq!(payload["observed_commit_oid"], "e".repeat(40));
                assert_eq!(payload["verified_result_id"], result_id);
                assert_eq!(payload["ref_name"], "refs/heads/barrier-test");
            } else {
                let error = result.err().expect("stale barrier verification was accepted");
                assert!(error.to_string().contains("barrier") || error.to_string().contains("frozen"), "{fault}: {error}");
                assert!(db.load_verify_target(&submitted.submission_id, "policy-1").is_err());
                assert_eq!(db.read_snapshot(None).unwrap(), before);
                assert_eq!(db.connection.query_row("SELECT count(*) FROM verification_runs WHERE task_id='a'", [], |row| row.get::<_,u64>(0)).unwrap(), 0);
                assert!(before.attempts.iter().find(|a| a.id == reservation.record.attempt).unwrap().retains_capacity());
            }
        }
    }

    #[test]
    fn revocation_records_the_live_consumer_without_releasing_capacity() {
        let (dir, mut db, preparation, reference) = downstream_fixture();
        let reservation = db.reserve_prepared(&[preparation], head_of(&db), 1000).unwrap();
        let stored: (String,u64,String) = db.connection.query_row("SELECT barrier_id,release_sequence,authorization_digest FROM attempt_required_releases WHERE attempt_id=?1", [reservation.record.attempt.as_str()], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
        assert_eq!(stored, (reference.barrier_id.clone(),reference.release_sequence,reference.authorization_digest.clone()));
        assert!(db.connection.execute("DELETE FROM barrier_live_consumers", []).is_err());
        let attempts = db.read_snapshot(None).unwrap().attempts;
        db.revoke_barrier(&reference.barrier_id, head_of(&db)).unwrap();
        let readiness = db.memory_readiness("a", 1001).unwrap();
        assert!(readiness.blockers.iter().any(|b| b.kind == "required_barrier_revoked" && b.id == reference.barrier_id), "{readiness:?}");
        assert_eq!(db.read_snapshot(None).unwrap().attempts, attempts);
        assert_eq!(db.connection.query_row("SELECT count(*) FROM barrier_live_consumers", [], |row| row.get::<_,u64>(0)).unwrap(), 0);
        assert_eq!(db.connection.query_row("SELECT count(*) FROM attempt_barrier_invalidations", [], |row| row.get::<_,u64>(0)).unwrap(), 1);
        assert!(db.connection.execute("UPDATE attempt_required_releases SET release_sequence=release_sequence", []).is_err());
        assert!(db.connection.execute("DELETE FROM attempt_required_releases", []).is_err());
        assert!(db.connection.execute("UPDATE attempt_barrier_invalidations SET sequence=sequence", []).is_err());
        assert!(db.connection.execute("DELETE FROM attempt_barrier_invalidations", []).is_err());
        assert!(attempts.iter().find(|a| a.id == reservation.record.attempt).unwrap().retains_capacity());
        let head = head_of(&db);
        db.revoke_barrier(&reference.barrier_id, 0).unwrap();
        assert_eq!(head_of(&db), head);
        drop(db);
        let mut db = SqliteStore::open(&dir.path().join(".state/state.db")).unwrap();
        assert!(db.memory_readiness("a", 1001).unwrap().blockers.iter().any(|b| b.kind == "required_barrier_revoked"));
        assert_eq!(db.read_snapshot(None).unwrap().attempts, attempts);
    }

    #[test]
    fn barrier_stop_service_preserves_claimed_capacity_and_proves_unstarted_cancellation() {
        for claimed in [false, true] {
            let (dir, mut db, preparation, reference) = downstream_fixture();
            let reservation = db.reserve_prepared(&[preparation], head_of(&db), 1000).unwrap();
            if claimed { db.claim_operation(&reservation.record.operation, 1, "fixture", 1000, 1000).unwrap(); }
            db.revoke_barrier(&reference.barrier_id, head_of(&db)).unwrap();
            let before = db.read_snapshot(None).unwrap();
            assert!(before.attempts.iter().find(|a| a.id == reservation.record.attempt).unwrap().retains_capacity());
            assert!(db.connection.execute("DELETE FROM barrier_pending_stops", []).is_err());
            drop(db);
            let mut db = SqliteStore::open(&dir.path().join(".state/state.db")).unwrap();
            let budget = || read_budget::ReadBudget::new(controlled::ReadControl::new(std::time::Instant::now()+Duration::from_secs(10), Default::default()));
            db.connection.execute_batch("CREATE TRIGGER reject_barrier_stop BEFORE INSERT ON attempt_cancellations BEGIN SELECT RAISE(ABORT,'injected stop failure'); END;").unwrap();
            assert!(db.service_barrier_stops(1001, &budget()).is_err());
            assert_eq!(db.read_snapshot(None).unwrap(), before);
            assert_eq!(db.connection.query_row("SELECT count(*) FROM barrier_pending_stops", [], |row| row.get::<_,u64>(0)).unwrap(), 1);
            db.connection.execute_batch("DROP TRIGGER reject_barrier_stop").unwrap();
            let report = db.service_barrier_stops(1001, &budget()).unwrap();
            assert_eq!(report.requested, 1);
            let after = db.read_snapshot(None).unwrap();
            assert_eq!(after.cancellations.len(), 1);
            assert!(after.cancellations[0].reason.starts_with("required barrier revoked at event "));
            let attempt = after.attempts.iter().find(|a| a.id == reservation.record.attempt).unwrap();
            assert_eq!(attempt.retains_capacity(), claimed);
            assert_eq!(attempt.termination_observed, !claimed);
            assert_eq!(db.connection.query_row("SELECT count(*) FROM barrier_pending_stops", [], |row| row.get::<_,u64>(0)).unwrap(), 0);
            let report = db.service_barrier_stops(1002, &budget()).unwrap();
            assert_eq!(report.requested, 0);
            assert!(!report.pending);
            assert_eq!(db.read_snapshot(None).unwrap(), after);
        }
    }

    #[test]
    fn barrier_stop_routing_preserves_an_existing_cancellation() {
        let (_dir, mut db, preparation, reference) = downstream_fixture();
        let reservation = db.reserve_prepared(&[preparation], head_of(&db), 1000).unwrap();
        db.claim_operation(&reservation.record.operation, 1, "fixture", 1000, 1000).unwrap();
        let attempt = db.read_snapshot(None).unwrap().attempts.into_iter().find(|a| a.id == reservation.record.attempt).unwrap();
        db.cancel_attempt(&attempt.id, attempt.revision, head_of(&db), "operator requested stop", 1001).unwrap();
        let before = db.read_snapshot(None).unwrap();
        db.revoke_barrier(&reference.barrier_id, before.head).unwrap();
        assert_eq!(db.read_snapshot(None).unwrap().cancellations, before.cancellations);
        assert_eq!(db.read_snapshot(None).unwrap().attempts, before.attempts);
        assert_eq!(db.connection.query_row("SELECT count(*) FROM barrier_pending_stops", [], |row| row.get::<_,u64>(0)).unwrap(), 0);
        assert!(db.connection.execute("INSERT INTO barrier_pending_stops VALUES(?1)", [attempt.id.as_str()]).is_err());
    }

    #[test]
    fn barrier_stop_service_is_bounded_cancellable_and_rotates_past_failure() {
        let (_dir, mut db, preparation, reference) = downstream_fixture();
        let reservation = db.reserve_prepared(&[preparation], head_of(&db), 1000).unwrap();
        for generation in 0..8 { synthetic_barrier_consumer(&db.connection, &reservation.record, generation, false).unwrap(); }
        db.revoke_barrier(&reference.barrier_id, head_of(&db)).unwrap();
        let before = db.read_snapshot(None).unwrap();
        let cancellation = crate::runner::Cancellation::default();
        let budget = read_budget::ReadBudget::new(controlled::ReadControl::new(std::time::Instant::now()+Duration::from_secs(10), cancellation.clone()));
        cancellation.cancel();
        assert!(matches!(db.service_barrier_stops(1001, &budget), Err(StoreError::Cancelled)));
        assert_eq!(db.read_snapshot(None).unwrap(), before);
        let budget = || read_budget::ReadBudget::new(controlled::ReadControl::new(std::time::Instant::now()+Duration::from_secs(10), Default::default()));
        db.connection.execute_batch("CREATE TRIGGER reject_first_barrier_stop BEFORE INSERT ON attempt_cancellations WHEN NEW.attempt_id=(SELECT min(attempt_id) FROM barrier_pending_stops) BEGIN SELECT RAISE(ABORT,'injected first stop failure'); END;").unwrap();
        assert!(db.service_barrier_stops(1001, &budget()).is_err());
        let report = db.service_barrier_stops(1001, &budget()).unwrap();
        assert_eq!(report.requested, 8);
        assert_eq!(db.connection.query_row("SELECT count(*) FROM barrier_pending_stops", [], |row| row.get::<_,u64>(0)).unwrap(), 1);
        db.connection.execute_batch("DROP TRIGGER reject_first_barrier_stop").unwrap();
        assert_eq!(db.service_barrier_stops(1002, &budget()).unwrap().requested, 1);
        assert_eq!(db.read_snapshot(None).unwrap().cancellations.len(), 9);
    }

    #[test]
    fn live_consumer_publication_and_invalidation_are_atomic() {
        let (_dir, mut db, preparation, reference) = downstream_fixture();
        db.connection.execute_batch("CREATE TRIGGER reject_binding BEFORE INSERT ON attempt_required_releases BEGIN SELECT RAISE(ABORT,'injected binding failure'); END;").unwrap();
        let before = db.read_snapshot(None).unwrap();
        assert!(db.reserve_prepared(&[preparation.clone()], before.head, 1000).is_err());
        assert_eq!(db.read_snapshot(None).unwrap(), before);
        assert_eq!(db.connection.query_row("SELECT count(*) FROM attempt_required_releases", [], |row| row.get::<_,u64>(0)).unwrap(), 0);
        db.connection.execute_batch("DROP TRIGGER reject_binding").unwrap();
        db.reserve_prepared(&[preparation], before.head, 1000).unwrap();
        db.connection.execute_batch("CREATE TRIGGER reject_invalidation BEFORE INSERT ON attempt_barrier_invalidations BEGIN SELECT RAISE(ABORT,'injected invalidation failure'); END;").unwrap();
        let before = db.read_snapshot(None).unwrap();
        assert!(db.revoke_barrier(&reference.barrier_id, before.head).is_err());
        assert_eq!(db.read_snapshot(None).unwrap(), before);
        assert!(db.frozen_barrier(&reference.barrier_id).unwrap().unwrap().revoked_seq.is_none());
        assert_eq!(db.connection.query_row("SELECT count(*) FROM barrier_live_consumers", [], |row| row.get::<_,u64>(0)).unwrap(), 1);
        db.connection.execute_batch("DROP TRIGGER reject_invalidation").unwrap();
        db.revoke_barrier(&reference.barrier_id, before.head).unwrap();
        assert!(db.memory_readiness("a", 1001).unwrap().blockers.iter().any(|b| b.kind == "required_barrier_revoked"));
        assert!(db.connection.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('attempt.barrier_invalidated','not-bound',1,1,'{}')", []).is_err());
    }

    #[test]
    fn terminated_consumers_leave_live_routing_but_keep_their_requirement() {
        let (_dir, mut db, preparation, reference) = downstream_fixture();
        let reservation = db.reserve_prepared(&[preparation], head_of(&db), 1000).unwrap();
        db.connection.execute("UPDATE attempts SET termination_observed=1 WHERE id=?1", [reservation.record.attempt.as_str()]).unwrap();
        assert_eq!(db.connection.query_row("SELECT count(*) FROM barrier_live_consumers", [], |row| row.get::<_,u64>(0)).unwrap(), 0);
        db.revoke_barrier(&reference.barrier_id, head_of(&db)).unwrap();
        assert_eq!(db.connection.query_row("SELECT count(*) FROM attempt_barrier_invalidations", [], |row| row.get::<_,u64>(0)).unwrap(), 0);
        assert_eq!(db.connection.query_row("SELECT count(*) FROM attempt_required_releases", [], |row| row.get::<_,u64>(0)).unwrap(), 1);
        assert!(super::super::contract_binding::require_result_barrier(&db.connection, "a", 1, &reservation.record.inputs.task_contract.as_ref().unwrap().digest, reservation.record.attempt.as_str(), 1001).is_err());
    }

    // Synthetic routing population, not approved admissions or worker effects.
    // Preserve record hashes and references while varying immutable generations.
    fn synthetic_barrier_consumer(db: &Connection, source: &AttemptInputRecord, generation: u64, terminated: bool) -> Result<AttemptInputRecord> {
        let mut inputs = source.inputs.clone();
        inputs.task_revision = generation + 100;
        let (attempt, operation) = super::super::reservations::record_ids(&inputs)?;
        let record = AttemptInputRecord { attempt, operation, inputs };
        let payload = serde_json::to_string(&record).unwrap();
        let hash = format!("{:x}", Sha256::digest(payload.as_bytes()));
        db.execute("INSERT INTO attempts VALUES(?1,?2,1,'reserved',NULL,?3,?4)", params![record.attempt.as_str(),record.inputs.task.as_str(),format!("worker:{}",record.attempt.as_str()),terminated])?;
        db.execute("INSERT INTO operations VALUES(?1,?2,'runtime.launch',?3,1,?4,?5,?6,1000,?1)", params![record.operation.as_str(),record.inputs.task.as_str(),record.inputs.binding,payload,hash,record.inputs.task_revision+1])?;
        db.execute("INSERT INTO attempt_inputs VALUES(?1,?2,?3,?4)", params![record.attempt.as_str(),record.operation.as_str(),payload,hash])?;
        Ok(record)
    }

    #[test]
    fn live_consumer_routing_bounds_admission_and_ignores_retired_history() {
        use rusqlite::StatementStatus;
        let mut work = Vec::new();
        for history in [0, 10_000] {
            let (_dir, mut db, preparation, reference) = downstream_fixture();
            let reservation = db.reserve_prepared(&[preparation], head_of(&db), 1000).unwrap();
            {
                let tx = db.connection.transaction().unwrap();
                for generation in 0..history { synthetic_barrier_consumer(&tx, &reservation.record, generation, true).unwrap(); }
                tx.commit().unwrap();
            }
            assert_eq!(db.connection.query_row("SELECT count(*) FROM barrier_live_consumers", [], |row| row.get::<_,u64>(0)).unwrap(), 1);
            let mut query = db.connection.prepare("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('barrier.revoked',?1,1,1,'{}')").unwrap();
            query.execute([&reference.barrier_id]).unwrap();
            work.push((query.get_status(StatementStatus::VmStep),query.get_status(StatementStatus::FullscanStep)));
            drop(query);
            assert_eq!(db.connection.query_row("SELECT count(*) FROM attempt_barrier_invalidations", [], |row| row.get::<_,u64>(0)).unwrap(), 1);
        }
        eprintln!("live consumer revocation SQL work at 0/10000 retired consumers: {work:?}");
        assert!(work[1].0 <= work[0].0+100, "retired history increased VM work: {work:?}");
        assert_eq!(work[1].1, 0, "retired consumers caused a full scan");

        let (_dir, mut db, preparation, reference) = downstream_fixture();
        let reservation = db.reserve_prepared(&[preparation], head_of(&db), 1000).unwrap();
        {
            let tx = db.connection.transaction().unwrap();
            for generation in 0..999 { synthetic_barrier_consumer(&tx, &reservation.record, generation, false).unwrap(); }
            tx.commit().unwrap();
        }
        {
            let tx = db.connection.transaction().unwrap();
            let error = synthetic_barrier_consumer(&tx, &reservation.record, 999, false).unwrap_err();
            assert!(matches!(error, StoreError::Conflict), "{error}");
        }
        assert_eq!(db.connection.query_row("SELECT count(*) FROM attempt_required_releases", [], |row| row.get::<_,u64>(0)).unwrap(), 1000);
        // The same proposed binding succeeds after an existing consumer is
        // observed terminated, proving that the refusal was the live bound.
        db.connection.execute("UPDATE attempts SET termination_observed=1 WHERE id=?1", [reservation.record.attempt.as_str()]).unwrap();
        {
            let tx = db.connection.transaction().unwrap();
            synthetic_barrier_consumer(&tx, &reservation.record, 999, false).unwrap();
            tx.commit().unwrap();
        }
        assert_eq!(db.connection.query_row("SELECT count(*) FROM attempt_required_releases", [], |row| row.get::<_,u64>(0)).unwrap(), 1001);
        db.revoke_barrier(&reference.barrier_id, head_of(&db)).unwrap();
        assert_eq!(db.connection.query_row("SELECT count(*) FROM barrier_live_consumers", [], |row| row.get::<_,u64>(0)).unwrap(), 0);
        assert_eq!(db.connection.query_row("SELECT count(*) FROM attempt_barrier_invalidations", [], |row| row.get::<_,u64>(0)).unwrap(), 1000);
        assert_eq!(db.connection.query_row("SELECT count(*) FROM attempts WHERE task_id='a' AND termination_observed=0", [], |row| row.get::<_,u64>(0)).unwrap(), 1000);
    }

    #[test]
    fn downstream_barrier_is_checked_at_draft_reservation_claim_and_effect() {
        for boundary in ["draft", "reserve", "claim", "effect"] {
            let (_dir, mut db, preparation, reference) = downstream_fixture();
            db.validate_launch_draft(&preparation.inputs, head_of(&db), 1000).unwrap();
            let reservation = matches!(boundary, "claim" | "effect").then(|| db.reserve_prepared(&[preparation.clone()], head_of(&db), 1000).unwrap());
            let claim = (boundary == "effect").then(|| db.claim_operation(&reservation.as_ref().unwrap().record.operation, 1, "worker", 1000, 1000).unwrap());
            db.revoke_barrier(&reference.barrier_id, head_of(&db)).unwrap();
            let before = db.read_snapshot(None).unwrap();
            let result = match boundary {
                "draft" => db.validate_launch_draft(&preparation.inputs, before.head, 1001),
                "reserve" => db.reserve_prepared(&[preparation], before.head, 1001).map(|_| ()),
                "claim" => db.claim_operation(&reservation.as_ref().unwrap().record.operation, 1, "worker", 1001, 1000).map(|_| ()),
                _ => db.validate_claim(claim.as_ref().unwrap(), 1001),
            };
            assert!(result.is_err(), "{boundary} accepted a revoked barrier");
            assert_eq!(db.read_snapshot(None).unwrap(), before);
            if let Some(reservation) = reservation {
                assert!(before.attempts.iter().find(|a| a.id == reservation.record.attempt).unwrap().retains_capacity());
            }
        }
    }

    fn authorized_fixture() -> (tempfile::TempDir, SqliteStore, PreparedBarrierRelease) {
        let (dir, mut store) = open_store();
        store.connection.execute("UPDATE project_control SET state='active',reconciliation_required=0,config_digest=?1", ["cc".repeat(32)]).unwrap();
        let seeded = seed(&store.connection, "alpha", "verify_only");
        let barrier = store.freeze_barrier(&[member_of(&seeded, vec![])], head(&store.connection).unwrap()).unwrap();
        let now = jiff::Timestamp::now().as_millisecond();
        let control = control::read(&store.connection).unwrap();
        let incarnation: String = store.connection.query_row("SELECT incarnation FROM active_work_meta WHERE singleton=1", [], |row| row.get(0)).unwrap();
        let document = BarrierReleaseAuthorization {
            schema_version: 1, action: "release_barrier".into(),
            project_store: std::fs::canonicalize(store.connection.path().unwrap()).unwrap().to_str().unwrap().into(),
            store_incarnation: incarnation,
            authority: VersionedReference { id: "owner-approval-policy".into(), revision: 1, digest: "bb".repeat(32) },
            config_digest: "cc".repeat(32), control_revision: control.revision, control_epoch: control.epoch,
            expected_head: head(&store.connection).unwrap(), barrier_id: barrier.barrier_id,
            memory_manifest_version: 2, memory_manifest_digest: barrier.memory_manifest_digest,
            required_set_generation: barrier.required_set_generation, release_policy: "all_members_ready_v1".into(),
            issued_unix_ms: now-1000, expires_unix_ms: now+60_000,
        };
        let prepared = PreparedBarrierRelease::parse_verified(&serde_json::to_vec(&document).unwrap()).unwrap();
        (dir, store, prepared)
    }

    #[test]
    fn operator_revocation_is_fenced_replayable_and_atomic() {
        let (_dir,mut db,prepared)=authorized_fixture();
        let released=db.release_authorized_barrier(&prepared).unwrap();
        let before_attempt=attempt_row(&db.connection,&released.members[0].attempt_id);
        let before=head_of(&db);
        let budget=||read_budget::ReadBudget::new(controlled::ReadControl::new(std::time::Instant::now()+std::time::Duration::from_secs(5),Default::default()));
        assert!(matches!(db.revoke_barrier_with_budget(&released.barrier_id,before+1,Some("operator withdrawal"),Some(&budget())),Err(StoreError::Conflict)));
        for reason in ["", "\n", &"x".repeat(4001)] {assert!(db.revoke_barrier_with_budget(&released.barrier_id,before,Some(reason),Some(&budget())).is_err());}
        let expired=read_budget::ReadBudget::new(controlled::ReadControl::new(std::time::Instant::now(),Default::default()));
        assert!(matches!(db.revoke_barrier_with_budget(&released.barrier_id,before,Some("operator withdrawal"),Some(&expired)),Err(StoreError::Deadline)));
        db.connection.execute_batch("CREATE TEMP TRIGGER refuse_operator_revocation BEFORE INSERT ON barrier_release_revocations BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
        assert!(db.revoke_barrier_with_budget(&released.barrier_id,before,Some("operator withdrawal"),Some(&budget())).is_err());
        assert_eq!(head_of(&db),before);assert_eq!(db.frozen_barrier(&released.barrier_id).unwrap(),Some(released.clone()));
        db.connection.execute_batch("DROP TRIGGER refuse_operator_revocation;").unwrap();
        let revoked=db.revoke_barrier_with_budget(&released.barrier_id,before,Some("operator withdrawal"),Some(&budget())).unwrap();
        assert!(revoked.revoked_seq.is_some());assert_eq!(revoked.released_seq,released.released_seq);
        let payload:String=db.connection.query_row("SELECT payload FROM events WHERE sequence=?1",[integer(revoked.revoked_seq.unwrap()).unwrap()],|row|row.get(0)).unwrap();
        assert_eq!(serde_json::from_str::<serde_json::Value>(&payload).unwrap(),serde_json::json!({"source":"operator","reason":"operator withdrawal","expected_head":before}));
        let head=head_of(&db);let path=std::path::PathBuf::from(db.connection.path().unwrap());drop(db);
        let mut db=SqliteStore::open(&path).unwrap();
        assert_eq!(db.revoke_barrier_with_budget(&released.barrier_id,before,Some("operator withdrawal"),Some(&budget())).unwrap(),revoked);
        assert!(matches!(db.revoke_barrier_with_budget(&released.barrier_id,before,Some("different reason"),Some(&budget())),Err(StoreError::Conflict)));
        assert!(matches!(db.revoke_barrier_with_budget(&released.barrier_id,head,Some("operator withdrawal"),Some(&budget())),Err(StoreError::Conflict)));
        assert_eq!(head_of(&db),head);assert_eq!(attempt_row(&db.connection,&released.members[0].attempt_id),before_attempt);
    }

    #[test]
    fn controlled_freeze_and_inspection_exclude_cold_tasks_and_bound_input() {
        use std::time::{Duration,Instant};
        use super::super::controlled::{ControlledStore,ReadControl};
        let (_dir,mut db)=open_store();
        let seeded=seed(&db.connection,"alpha","verify_only");
        let member=member_of(&seeded,vec![]);
        db.connection.execute("INSERT INTO tasks(id,revision,state,title) VALUES('bad/task',1,'draft','unrelated malformed history')",[]).unwrap();
        assert!(db.read_snapshot(None).is_err());
        let before=head_of(&db);
        let path=std::path::PathBuf::from(db.connection.path().unwrap());
        let control=||ReadControl::new(Instant::now()+Duration::from_secs(5),Default::default());
        let mut scoped=ControlledStore::open_scoped(&path,control()).unwrap();
        // Small encoded JSON can still contain excessive structure. Account it
        // before deserialization, even when its member shape would be invalid.
        let dense=format!("[{}null]","{},".repeat(200_000));
        assert!(matches!(scoped.freeze_barrier_json(dense.as_bytes(),before),Err(StoreError::Limit(_))));
        drop(scoped);
        assert_eq!(head_of(&db),before);
        let mut scoped=ControlledStore::open_scoped(&path,control()).unwrap();
        let malformed=b"[{\"task_id\":\"private sentinel\"}]";
        let error=scoped.freeze_barrier_json(malformed,before).unwrap_err().to_string();
        assert!(error.contains("contents withheld"));assert!(!error.contains("private sentinel"));
        let raw=serde_json::to_vec(&vec![member.clone()]).unwrap();
        let frozen=scoped.freeze_barrier_json(&raw,before).unwrap();
        assert_eq!(scoped.frozen_barrier(&frozen.barrier_id).unwrap(),Some(frozen.clone()));
        assert_eq!(scoped.freeze_barrier(&[member],before).unwrap(),frozen);
        let after=head_of(&db);
        drop(scoped);
        let c=control();let mut scoped=ControlledStore::open_scoped(&path,c.clone()).unwrap();c.cancellation().cancel();
        assert!(matches!(scoped.frozen_barrier(&frozen.barrier_id),Err(StoreError::Cancelled)));
        assert_eq!(head_of(&db),after);
    }

    #[test]
    fn controlled_freeze_interrupts_member_publication_without_partial_barrier() {
        use std::time::{Duration,Instant};
        use super::super::controlled::{ControlledStore,ReadControl};
        let (_dir,mut db)=open_store();
        let seeded=seed(&db.connection,"alpha","verify_only");let members=[member_of(&seeded,vec![])];
        let before=head_of(&db);
        let budget=read_budget::ReadBudget::new(ReadControl::new(Instant::now()+Duration::from_secs(5),Default::default()));
        budget.bytes(budget.remaining_units()-128).unwrap();
        assert!(matches!(db.freeze_barrier_with_budget(&members,before,Some(&budget)),Err(StoreError::Limit(_))));
        db.connection.execute_batch("CREATE TRIGGER slow_barrier_member BEFORE INSERT ON barrier_members BEGIN
            SELECT (WITH RECURSIVE work(n) AS (VALUES(0) UNION ALL SELECT n+1 FROM work WHERE n<1000000000) SELECT sum(n) FROM work); END;").unwrap();
        let path=std::path::PathBuf::from(db.connection.path().unwrap());
        let mut scoped=ControlledStore::open_scoped(&path,ReadControl::new(Instant::now()+Duration::from_millis(500),Default::default())).unwrap();
        let start=Instant::now();
        assert!(matches!(scoped.freeze_barrier(&members,before),Err(StoreError::Deadline)));
        assert!(start.elapsed()<Duration::from_secs(3));drop(scoped);
        assert_eq!(head_of(&db),before);
        for table in ["barrier_revisions","barrier_memory_read_sets","barrier_members"] {
            assert_eq!(db.connection.query_row(&format!("SELECT count(*) FROM {table}"),[],|row|row.get::<_,u64>(0)).unwrap(),0,"{table}");
        }
        db.connection.execute_batch("DROP TRIGGER slow_barrier_member").unwrap();
        let mut scoped=ControlledStore::open_scoped(&path,ReadControl::new(Instant::now()+Duration::from_secs(5),Default::default())).unwrap();
        let frozen=scoped.freeze_barrier(&members,before).unwrap();
        assert_eq!(scoped.frozen_barrier(&frozen.barrier_id).unwrap(),Some(frozen.clone()));
        assert_eq!(db.frozen_barrier(&frozen.barrier_id).unwrap(),Some(frozen));
    }

    #[test]
    fn authorized_release_preserves_shared_budget_and_rolls_back_interrupted_publication() {
        use std::time::{Duration,Instant};
        use super::super::controlled::{ControlledStore,ReadControl};
        let (_dir,mut db,prepared)=authorized_fixture();
        let before=head_of(&db);
        let budget=read_budget::ReadBudget::new(ReadControl::new(Instant::now()+Duration::from_secs(5),Default::default()));
        budget.bytes(budget.remaining_units()-128).unwrap();
        assert!(matches!(db.release_authorized_barrier_with_budget(&prepared,Some(&budget)),Err(StoreError::Limit(_))));
        assert_eq!(head_of(&db),before);
        let expired=read_budget::ReadBudget::new(ReadControl::new(Instant::now(),Default::default()));
        assert!(matches!(db.draft_barrier_release_with_budget(&prepared.document.barrier_id,prepared.document.authority.clone(),&prepared.document.config_digest,prepared.document.expires_unix_ms,Some(&expired)),Err(StoreError::Deadline)));
        // Stall after the release event/header writes, at signed-receipt publication.
        // The readiness helper must not replace/remove the controlled SQL hook.
        db.connection.execute_batch("CREATE TRIGGER slow_signed_release BEFORE INSERT ON barrier_release_authorizations BEGIN
            SELECT (WITH RECURSIVE work(n) AS (VALUES(0) UNION ALL SELECT n+1 FROM work WHERE n<1000000000) SELECT sum(n) FROM work); END;").unwrap();
        let path=std::path::PathBuf::from(db.connection.path().unwrap());
        let mut controlled=ControlledStore::open(&path,ReadControl::new(Instant::now()+Duration::from_millis(500),Default::default())).unwrap();
        let start=Instant::now();
        assert!(matches!(controlled.release_authorized_barrier(&prepared),Err(StoreError::Deadline)));
        assert!(start.elapsed()<Duration::from_secs(3));
        drop(controlled);
        assert_eq!(head_of(&db),before);
        assert!(db.frozen_barrier(&prepared.document.barrier_id).unwrap().unwrap().released_seq.is_none());
        assert_eq!(db.connection.query_row("SELECT count(*) FROM barrier_release_authorizations",[],|row|row.get::<_,u64>(0)).unwrap(),0);
        db.connection.execute_batch("DROP TRIGGER slow_signed_release").unwrap();
        let control=ReadControl::new(Instant::now()+Duration::from_secs(5),Default::default());
        let mut controlled=ControlledStore::open(&path,control.clone()).unwrap();
        let released=controlled.release_authorized_barrier(&prepared).unwrap();
        assert_eq!(controlled.release_authorized_barrier(&prepared).unwrap(),released);
        control.cancellation().cancel();
        assert!(matches!(controlled.release_authorized_barrier(&prepared),Err(StoreError::Cancelled)));
        assert_eq!(db.frozen_barrier(&prepared.document.barrier_id).unwrap(),Some(released));
    }

    #[test]
    fn authorized_barrier_service_verifies_real_owner_signature_and_published_store() {
        use std::{fs, process::Command};
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        fs::create_dir(&project).unwrap();
        for child in [".state", "threads", "inbox"] { fs::create_dir(project.join(child)).unwrap(); }
        fs::write(project.join("PROJECT.md"), "+++\nname='Project'\n+++\n").unwrap();
        fs::write(project.join("TASKS.md"), "").unwrap();
        fs::write(project.join("MEMORY.md"), "").unwrap();
        fs::write(project.join(".state/project.json"), r#"{"status":"paused"}"#).unwrap();
        let key = dir.path().join("owner");
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-q","-t","ed25519","-N","","-f"]).arg(&key).status().unwrap().success());
        let public = fs::read_to_string(key.with_extension("pub")).unwrap().split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        let config = dir.path().join("owner.toml");
        fs::write(&config, format!("[authority]\nversion=1\nrevision=1\napproval_public_key={public:?}\n")).unwrap();
        let plan = crate::migration::inspect_with_config(&project, &config).unwrap();
        crate::migration::apply(&project, &plan, true).unwrap();
        let snapshot = crate::runtime::snapshot(&project).unwrap();
        crate::runtime::set_state(&project, snapshot.head, snapshot.control.unwrap().revision, ProjectState::Active, &config).unwrap();
        let db = crate::migration::open_active(&project).unwrap();
        let seeded = seed(&db.connection, "alpha", "verify_only");
        let before = head(&db.connection).unwrap();
        drop(db);
        let members=vec![member_of(&seeded,vec![])];
        let membership=dir.path().join("barrier-members.json");
        fs::write(&membership,serde_json::to_vec(&members).unwrap()).unwrap();
        let barrier=crate::memory::freeze_memory_barrier_file(&project,&membership,before).unwrap();
        assert_eq!(crate::memory::freeze_memory_barrier(&project,&members,before).unwrap(),barrier);
        assert_eq!(crate::memory::inspect_memory_barrier(&project,&barrier.barrier_id).unwrap(),Some(barrier.clone()));
        let mut db = crate::migration::open_active(&project).unwrap();
        assert_eq!(db.frozen_barrier(&barrier.barrier_id).unwrap(), Some(barrier.clone()));
        let control = control::read(&db.connection).unwrap();
        let before = head(&db.connection).unwrap();
        drop(db);
        let document = crate::authority::draft_barrier_release(&project, &barrier.barrier_id, jiff::Timestamp::now().as_millisecond()+60_000).unwrap();
        assert_eq!(document.barrier_id, barrier.barrier_id);
        assert_eq!(document.memory_manifest_digest, barrier.memory_manifest_digest);
        assert_eq!(document.control_revision, control.revision);
        assert_eq!(document.expected_head, before);
        assert_eq!(crate::runtime::snapshot(&project).unwrap().head, before);
        let path = dir.path().join("release.json");
        let signature = dir.path().join("release.json.sig");
        let bytes = serde_json::to_vec(&document).unwrap();
        fs::write(&path, &bytes).unwrap();
        for namespace in [crate::authority::SIGNATURE_NAMESPACE, crate::authority::BARRIER_RELEASE_SIGNATURE_NAMESPACE] {
            let output = Command::new("/usr/bin/ssh-keygen").args(["-Y","sign","-f"]).arg(&key).args(["-n",namespace]).arg(&path).output().unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            let result = crate::authority::release_memory_barrier(&project, &path, &signature, document.expected_head);
            if namespace == crate::authority::SIGNATURE_NAMESPACE {
                assert!(result.is_err());
                assert!(crate::migration::open_active(&project).unwrap().frozen_barrier(&barrier.barrier_id).unwrap().unwrap().released_seq.is_none());
                fs::remove_file(&signature).unwrap();
            } else {
                let released = result.unwrap();
                assert!(released.released_seq.is_some());
                assert_eq!(crate::authority::release_memory_barrier(&project, &path, &signature, document.expected_head).unwrap(), released);
            }
        }
        let db = crate::migration::open_active(&project).unwrap();
        assert_eq!(db.connection.query_row("SELECT raw FROM barrier_release_authorizations", [], |row| row.get::<_,Vec<u8>>(0)).unwrap(), bytes);
        assert_eq!(attempt_row(&db.connection, &seeded.attempt).1, 0);
        let before=head_of(&db);
        let revoked=crate::authority::revoke_memory_barrier(&project,&barrier.barrier_id,before,"withdraw reviewed release").unwrap();
        assert!(revoked.revoked_seq.is_some());
        assert_eq!(crate::authority::revoke_memory_barrier(&project,&barrier.barrier_id,before,"withdraw reviewed release").unwrap(),revoked);
        assert_eq!(db.connection.query_row("SELECT raw FROM barrier_release_authorizations",[],|row|row.get::<_,Vec<u8>>(0)).unwrap(),bytes);
        assert_eq!(attempt_row(&db.connection,&seeded.attempt).1,0);
    }

    #[test]
    fn transitive_source_fanout_is_bounded_before_any_derived_barrier_is_removed() {
        let (_dir, mut db) = open_store();
        let alpha = seed(&db.connection, "alpha", "verify_only");
        let beta = seed(&db.connection, "beta", "verify_only");
        optional_head(&db.connection, "observation");
        for id in ["source", "leaf"] {
            db.connection.execute("INSERT INTO memory_records SELECT ?1,?1,scope_id,kind,is_hard FROM memory_records WHERE id='note'", [id]).unwrap();
            db.connection.execute("INSERT INTO memory_revisions SELECT ?1,revision,body_hash,provenance_hash,promoted_seq,applicability FROM memory_revisions WHERE record_id='note'", [id]).unwrap();
            db.connection.execute("INSERT INTO memory_validity SELECT ?1,revision,state,reason,expiry_unix_ms,evaluated_seq FROM memory_validity WHERE record_id='note'", [id]).unwrap();
            db.connection.execute("INSERT INTO memory_heads SELECT ?1,revision,status,row_revision FROM memory_heads WHERE record_id='note'", [id]).unwrap();
        }
        db.connection.execute("INSERT INTO memory_dependencies VALUES('note',1,'source',1,'supports'),('leaf',1,'source',1,'supports')", []).unwrap();
        db.connection.execute("INSERT INTO snapshot_entries VALUES('snap-alpha',1,'note',1,'optional','fixture'),('snap-beta',1,'leaf',1,'optional','fixture')", []).unwrap();
        let a = db.freeze_barrier(&[member_of(&alpha, vec![])], head_of(&db)).unwrap();
        let b = db.freeze_barrier(&[member_of(&beta, vec![])], head_of(&db)).unwrap();
        // Synthetic retained v2 rows: 501 barriers depend on note, 500 on leaf;
        // all 1001 consume source. Each derived batch alone fits the limit.
        db.connection.execute_batch("BEGIN IMMEDIATE").unwrap();
        for index in 1..=999 {
            let id = format!("{index:064x}");
            let source = if index <= 500 { &a.barrier_id } else { &b.barrier_id };
            db.connection.execute("INSERT INTO barrier_revisions SELECT ?1,required_set_generation,memory_manifest_digest,?1,NULL,NULL,created_seq FROM barrier_revisions WHERE barrier_id=?2", params![id,source]).unwrap();
            db.connection.execute("INSERT INTO barrier_members SELECT ?1,position,task_id,contract_revision,attempt_id,result_id,verification_id,integration_id,proposal_dispositions FROM barrier_members WHERE barrier_id=?2", params![id,source]).unwrap();
            db.connection.execute("INSERT INTO barrier_memory_read_sets SELECT ?1,schema_version,payload FROM barrier_memory_read_sets WHERE barrier_id=?2", params![id,source]).unwrap();
        }
        db.connection.execute_batch("COMMIT").unwrap();
        let before = head_of(&db);
        assert!(matches!(db.apply_memory_op(MemoryPolicyOp::RevokeHead, "source", before), Err(StoreError::Limit(_))));
        assert_eq!(head_of(&db), before);
        assert_eq!(db.connection.query_row("SELECT count(*) FROM barrier_current_status WHERE revoked_seq IS NOT NULL", [], |row| row.get::<_,u64>(0)).unwrap(), 0);
        assert_eq!(db.connection.query_row("SELECT count(*) FROM barrier_open_memory_records WHERE record_id='source'", [], |row| row.get::<_,u64>(0)).unwrap(), 1001);
        assert_eq!(db.connection.query_row("SELECT count(*) FROM memory_validity WHERE state='valid'", [], |row| row.get::<_,u64>(0)).unwrap(), 3);
        assert_eq!(db.connection.query_row("SELECT status FROM memory_heads WHERE record_id='source'", [], |row| row.get::<_,String>(0)).unwrap(), "active");
    }

    #[test]
    fn direct_source_revocation_sql_work_is_independent_of_revoked_barrier_history() {
        use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
        let mut work = Vec::new();
        for history in [0, 10_000] {
            let (_dir, mut db) = open_store();
            let seeded = seed(&db.connection, "alpha", "verify_only");
            optional_head(&db.connection, "observation");
            db.connection.execute("INSERT INTO snapshot_entries VALUES('snap-alpha',1,'note',1,'optional','fixture')", []).unwrap();
            let frozen = db.freeze_barrier(&[member_of(&seeded, vec![])], head_of(&db)).unwrap();
            add_routing_history(&db.connection, &frozen.barrier_id, history, true);
            let before = head_of(&db);
            let steps = Arc::new(AtomicU64::new(0));
            let observed = steps.clone();
            db.connection.progress_handler(1, Some(move || { observed.fetch_add(1, Ordering::Relaxed); false }));
            let result = db.apply_memory_op(MemoryPolicyOp::RevokeHead, "note", before);
            db.connection.progress_handler(0, None::<fn() -> bool>);
            result.unwrap();
            work.push(steps.load(Ordering::Relaxed));
            assert!(db.frozen_barrier(&frozen.barrier_id).unwrap().unwrap().revoked_seq.is_some());
        }
        eprintln!("direct source revocation transaction SQL progress steps at 0/10000 revoked barriers: {work:?}");
        assert!(work[1] <= work[0]+100, "retained history increased SQL work: {work:?}");
    }

    #[test]
    fn barrier_memory_routing_distinguishes_noise_from_changed_evidence() {
        for change in ["optional_noise", "consumed", "contract", "policy", "legacy"] {
            let (_dir, mut db) = open_store();
            let seeded = seed(&db.connection, "alpha", "verify_only");
            optional_head(&db.connection, "observation");
            if change == "consumed" {
                db.connection.execute("INSERT INTO snapshot_entries VALUES('snap-alpha',1,'note',1,'optional','fixture')", []).unwrap();
            }
            let frozen = if change == "legacy" {
                legacy_barrier(&db.connection, &member_of(&seeded, vec![]))
            } else {
                db.freeze_barrier(&[member_of(&seeded, vec![])], head_of(&db)).unwrap()
            };
            // Merely routing an existing revision must not invalidate evidence.
            let cause = head_of(&db);
            let tx = db.connection.transaction_with_behavior(TransactionBehavior::Immediate).unwrap();
            super::super::memory_delivery::record_change(&tx, "redelivery", "note", 1, "informational", cause).unwrap();
            tx.commit().unwrap();
            assert!(db.frozen_barrier(&frozen.barrier_id).unwrap().unwrap().revoked_seq.is_none());
            if change == "policy" {
                let expected_head = head_of(&db);
                let policy = PreparedMemoryPolicy { policy: MemoryPolicy {
                    version: 1, project_store: db.connection.path().unwrap().into(), revision: 1,
                    authority: VersionedReference { id: "owner-approval-policy".into(), revision: 1, digest: digest() },
                    expected_head, op: MemoryPolicyOp::ImportAck, record_key: Some("note".into()),
                    memory_plan_digest: None, expected_memory_owner: None,
                }};
                db.install_memory_policy(&policy, expected_head).unwrap();
            } else {
                let id = if change == "consumed" { "note" } else { "other" };
                db.insert_memory_revision(&NewRevision {
                    id: MemoryRecordId::new(id).unwrap(), record_key: id.into(), scope_id: "project".into(),
                    kind: if change == "contract" { MemoryKind::Contract } else { MemoryKind::Observation },
                    body_hash: ObjectId::from_hex(digest()).unwrap(), provenance_hash: ObjectId::from_hex(digest()).unwrap(),
                    applicability: Applicability { domains: vec![], paths: vec![] }, dependencies: vec![],
                    expected: if change == "consumed" { Some(1) } else { None }, expiry_unix_ms: None,
                    validity_state: "valid".into(), validity_reason: "fixture".into(),
                }).unwrap();
            }
            let current = db.frozen_barrier(&frozen.barrier_id).unwrap().unwrap();
            assert_eq!(current.revoked_seq.is_some(), change != "optional_noise", "{change}");
            if change != "optional_noise" {
                assert_eq!(db.connection.query_row("SELECT count(*) FROM barrier_open_memory_records", [], |row| row.get::<_,u64>(0)).unwrap(), 0);
                assert_eq!(db.connection.query_row("SELECT count(*) FROM barrier_open_memory_unknown", [], |row| row.get::<_,u64>(0)).unwrap(), 0);
            }
        }
    }

    #[test]
    fn barrier_memory_publication_rolls_back_when_revocation_fails_or_fanout_overflows() {
        for fault in ["publication", "fanout"] {
            let (_dir, mut db) = open_store();
            let seeded = seed(&db.connection, "alpha", "verify_only");
            optional_head(&db.connection, "observation");
            db.connection.execute("INSERT INTO snapshot_entries VALUES('snap-alpha',1,'note',1,'optional','fixture')", []).unwrap();
            let frozen = db.freeze_barrier(&[member_of(&seeded, vec![])], head_of(&db)).unwrap();
            if fault == "publication" {
                db.release_barrier(&frozen.barrier_id, &frozen.release_token, head_of(&db), 1).unwrap();
                db.connection.execute_batch("CREATE TRIGGER fail_source_revocation BEFORE INSERT ON barrier_release_revocations BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
            } else {
                add_routing_history(&db.connection, &frozen.barrier_id, 1000, false);
            }
            let before = head_of(&db);
            let old = db.frozen_barrier(&frozen.barrier_id).unwrap().unwrap();
            let error = db.apply_memory_op(MemoryPolicyOp::HardRule, "note", before).unwrap_err();
            if fault == "fanout" { assert!(matches!(error, StoreError::Limit(_)), "{error:?}"); }
            assert_eq!(head_of(&db), before);
            assert_eq!(db.frozen_barrier(&frozen.barrier_id).unwrap().unwrap(), old);
            assert_eq!(db.connection.query_row("SELECT is_hard FROM memory_records WHERE id='note'", [], |row| row.get::<_,u64>(0)).unwrap(), 0);
            assert_eq!(db.connection.query_row("SELECT reason FROM memory_validity WHERE record_id='note' AND revision=1", [], |row| row.get::<_,String>(0)).unwrap(), "fixture");
        }
    }

    #[test]
    fn retired_worker_barrier_is_revoked_when_its_transitive_source_changes() {
        let (_dir, mut db) = open_store();
        let seeded = seed(&db.connection, "alpha", "verify_only");
        optional_head(&db.connection, "observation");
        db.connection.execute("INSERT INTO memory_records SELECT 'source','source',scope_id,kind,is_hard FROM memory_records WHERE id='note'",[]).unwrap();
        db.connection.execute("INSERT INTO memory_revisions SELECT 'source',revision,body_hash,provenance_hash,promoted_seq,applicability FROM memory_revisions WHERE record_id='note'",[]).unwrap();
        db.connection.execute("INSERT INTO memory_validity SELECT 'source',revision,state,reason,expiry_unix_ms,evaluated_seq FROM memory_validity WHERE record_id='note'",[]).unwrap();
        db.connection.execute("INSERT INTO memory_heads SELECT 'source',revision,status,row_revision FROM memory_heads WHERE record_id='note'",[]).unwrap();
        db.connection.execute("INSERT INTO memory_dependencies VALUES('note',1,'source',1,'supports')", []).unwrap();
        db.connection.execute("INSERT INTO snapshot_entries VALUES('snap-alpha',1,'note',1,'optional','fixture')", []).unwrap();
        let binding = "34".repeat(32);
        db.connection.execute("INSERT INTO consumer_bindings(binding_id,consumer_id,generation,snapshot_id,attempt_id,task_id,active,retired,successor_binding_id,created_unix_ms) VALUES(?1,'task:alpha',1,'snap-alpha','attempt-alpha','alpha',1,0,NULL,1)", [&binding]).unwrap();
        db.retire_consumer_binding(&binding, None).unwrap();
        let frozen = db.freeze_barrier(&[member_of(&seeded, vec![])], head_of(&db)).unwrap();
        let released = db.release_barrier(&frozen.barrier_id, &frozen.release_token, head_of(&db), 1).unwrap();
        let records: Vec<String> = db.connection.prepare("SELECT record_id FROM barrier_open_memory_records WHERE barrier_id=?1 ORDER BY record_id").unwrap().query_map([&frozen.barrier_id], |row| row.get(0)).unwrap().collect::<std::result::Result<_,_>>().unwrap();
        assert_eq!(records, vec!["note", "source"]);
        assert!(db.connection.execute("DELETE FROM barrier_open_memory_records", []).is_err());
        assert!(db.connection.execute("UPDATE barrier_open_memory_records SET record_id=record_id", []).is_err());
        assert_eq!(db.connection.query_row("SELECT count(*) FROM consumer_bindings WHERE active=1", [], |row| row.get::<_,u64>(0)).unwrap(), 0);
        db.apply_memory_op(MemoryPolicyOp::RevokeHead, "source", head_of(&db)).unwrap();
        let current = db.frozen_barrier(&released.barrier_id).unwrap().unwrap();
        assert!(current.revoked_seq.is_some(), "barrier source invalidation must not require an active worker binding");
        assert_eq!(current.released_seq, released.released_seq);
        assert_eq!(db.connection.query_row("SELECT count(*) FROM memory_delivery_intents", [], |row| row.get::<_,u64>(0)).unwrap(), 0);
        assert_eq!(db.connection.query_row("SELECT count(*) FROM barrier_open_memory_records", [], |row| row.get::<_,u64>(0)).unwrap(), 0);
    }

    #[test]
    fn automatic_barrier_invalidation_is_bounded_and_does_not_scan_revoked_history() {
        use rusqlite::StatementStatus;
        let mut work = Vec::new();
        for history in [0, 10_000] {
            let (_dir, mut db, prepared) = authorized_fixture();
            add_routing_history(&db.connection, &prepared.document.barrier_id, history, true);
            assert_eq!(db.connection.query_row("SELECT count(*) FROM barrier_open_members", [], |row| row.get::<_,u64>(0)).unwrap(), 1);
            let before = head_of(&db);
            let mut query = db.connection.prepare("INSERT INTO memory_invalidations VALUES('measured','alpha','cause',NULL,'stop_at_checkpoint',?1,NULL,'measure')").unwrap();
            query.execute([before]).unwrap();
            work.push((query.get_status(StatementStatus::VmStep),query.get_status(StatementStatus::FullscanStep)));
            drop(query);
            assert!(db.frozen_barrier(&prepared.document.barrier_id).unwrap().unwrap().revoked_seq.is_some());
        }
        eprintln!("automatic barrier invalidation SQL work at 0/10000 revoked barriers: {work:?}");
        assert!(work[1].0 <= work[0].0+100, "historical barriers increased VM work: {work:?}");
        assert_eq!(work[1].1, work[0].1, "historical barriers increased scans");

        let (_dir, db, prepared) = authorized_fixture();
        add_routing_history(&db.connection, &prepared.document.barrier_id, 1000, false);
        let before = head_of(&db);
        for task in [Some("alpha"), None] {
            let error = db.connection.execute("INSERT INTO memory_invalidations VALUES('overflow',?1,'cause',NULL,'stop_at_checkpoint',?2,NULL,'bounded')", params![task,before]).unwrap_err();
            assert!(error.to_string().contains("exceeds 1000"), "{error}");
            assert_eq!(head_of(&db), before);
            assert_eq!(db.connection.query_row("SELECT count(*) FROM memory_invalidations", [], |row| row.get::<_,u64>(0)).unwrap(), 0);
        }
        let mut db = db;
        db.revoke_barrier(&prepared.document.barrier_id, before).unwrap();
        let before = head_of(&db);
        db.connection.execute("INSERT INTO memory_invalidations VALUES('fits',NULL,'cause',NULL,'stop_at_checkpoint',?1,NULL,'exact limit')", [before]).unwrap();
        assert_eq!(head_of(&db), before+1000);
        assert_eq!(db.connection.query_row("SELECT count(*) FROM barrier_open_members", [], |row| row.get::<_,u64>(0)).unwrap(), 0);
    }

    // Synthetic immutable membership history for routing-work and fan-out tests;
    // these copied headers are not signed release or verification evidence.
    fn add_routing_history(db: &Connection, source: &str, count: usize, revoked: bool) {
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        for index in 0..count {
            let id = format!("{:x}", Sha256::digest(format!("routing-history-{index}").as_bytes()));
            db.execute("INSERT INTO barrier_revisions SELECT ?1,required_set_generation,memory_manifest_digest,?1,NULL,CASE WHEN ?3 THEN created_seq ELSE NULL END,created_seq FROM barrier_revisions WHERE barrier_id=?2", params![id,source,revoked]).unwrap();
            db.execute("INSERT INTO barrier_members SELECT ?1,position,task_id,contract_revision,attempt_id,result_id,verification_id,integration_id,proposal_dispositions FROM barrier_members WHERE barrier_id=?2", params![id,source]).unwrap();
        }
        db.execute_batch("COMMIT").unwrap();
    }

    #[test]
    fn automatic_barrier_revocation_rolls_back_with_its_invalidation() {
        let (_dir, mut db, prepared) = authorized_fixture();
        let released = db.release_authorized_barrier(&prepared).unwrap();
        let beta = seed(&db.connection, "beta", "verify_only");
        let pending = db.freeze_barrier(&[member_of(&beta, vec![])], head_of(&db)).unwrap();
        assert!(db.connection.execute("DELETE FROM barrier_open_members", []).is_err());
        assert!(db.connection.execute("UPDATE barrier_open_members SET task_id=task_id", []).is_err());
        db.connection.execute_batch("CREATE TRIGGER fail_automatic_revocation BEFORE INSERT ON barrier_release_revocations BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
        let before = head_of(&db);
        assert!(db.connection.execute("INSERT INTO memory_invalidations VALUES('failed-global',NULL,'cause',NULL,'stop_at_checkpoint',?1,NULL,'fault')", [before]).is_err());
        assert_eq!(head_of(&db), before);
        assert_eq!(db.frozen_barrier(&released.barrier_id).unwrap(), Some(released.clone()));
        assert_eq!(db.frozen_barrier(&pending.barrier_id).unwrap(), Some(pending));
        assert_eq!(db.connection.query_row("SELECT count(*) FROM memory_invalidations", [], |row| row.get::<_,u64>(0)).unwrap(), 0);
        assert_eq!(db.connection.query_row("SELECT count(*) FROM barrier_open_members", [], |row| row.get::<_,u64>(0)).unwrap(), 2);
        db.connection.execute_batch("DROP TRIGGER fail_automatic_revocation").unwrap();
        db.connection.execute("INSERT INTO memory_invalidations VALUES('failed-global',NULL,'cause',NULL,'stop_at_checkpoint',?1,NULL,'retry')", [before]).unwrap();
        assert_eq!(db.connection.query_row("SELECT count(*) FROM barrier_open_members", [], |row| row.get::<_,u64>(0)).unwrap(), 0);
        assert!(db.connection.execute("INSERT INTO barrier_open_members VALUES(?1,'alpha')", [&released.barrier_id]).is_err());
    }

    #[test]
    fn upgrade_revokes_barriers_with_retained_unresolved_invalidations() {
        let (_dir, mut db, prepared) = authorized_fixture();
        let released = db.release_authorized_barrier(&prepared).unwrap();
        let beta = seed(&db.connection, "beta", "verify_only");
        let pending = db.freeze_barrier(&[member_of(&beta, vec![])], head_of(&db)).unwrap();
        super::super::test_schema::historical(&db.connection, 42).unwrap();
        let before = head_of(&db);
        db.connection.execute("INSERT INTO memory_invalidations VALUES('old-alpha','alpha','cause',NULL,'stop_at_checkpoint',?1,NULL,'retained')", [before]).unwrap();
        db.connection.execute("INSERT INTO memory_invalidations VALUES('old-beta','beta','cause',NULL,'informational',?1,NULL,'advice')", [before]).unwrap();
        db.upgrade_v1().unwrap();
        let current = db.frozen_barrier(&released.barrier_id).unwrap().unwrap();
        assert_eq!(current.released_seq, released.released_seq);
        assert_eq!(current.revoked_seq, Some(before+1));
        assert!(db.frozen_barrier(&pending.barrier_id).unwrap().unwrap().revoked_seq.is_none());
        assert_eq!(db.connection.query_row("SELECT count(*) FROM barrier_release_authorizations", [], |row| row.get::<_,u64>(0)).unwrap(), 0);
        assert_eq!(db.connection.query_row("SELECT count(*) FROM barrier_open_members", [], |row| row.get::<_,u64>(0)).unwrap(), 1);
        let payload: String = db.connection.query_row("SELECT payload FROM events WHERE sequence=?1", [before+1], |row| row.get(0)).unwrap();
        let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(payload["invalidation_id"], "old-alpha");
        assert_eq!(payload["reason"], "schema43_unresolved_invalidation");
    }

    #[test]
    fn memory_invalidation_revokes_pending_and_released_barriers_atomically() {
        let (_dir, mut db, prepared) = authorized_fixture();
        let released = db.release_authorized_barrier(&prepared).unwrap();
        satisfy(&db.connection, "downstream", "alpha", &released.members[0].result_id);
        let beta = seed(&db.connection, "beta", "verify_only");
        let pending = db.freeze_barrier(&[member_of(&beta, vec![])], head_of(&db)).unwrap();
        let before_attempt = attempt_row(&db.connection, "attempt-alpha");
        let cause = insert_event(&db.connection, "fixture", "memory-cause", &serde_json::json!({})).unwrap();
        db.connection.execute("INSERT INTO memory_invalidations VALUES('advice','alpha','cause',NULL,'informational',?1,NULL,'advice')", [cause]).unwrap();
        db.connection.execute("INSERT INTO memory_invalidations VALUES('resolved','alpha','cause',NULL,'stop_at_checkpoint',?1,?1,'already resolved')", [cause]).unwrap();
        assert!(db.frozen_barrier(&released.barrier_id).unwrap().unwrap().revoked_seq.is_none());
        db.connection.execute("INSERT INTO memory_invalidations VALUES('scoped','alpha','cause',NULL,'stop_at_checkpoint',?1,NULL,'changed source')", [cause]).unwrap();
        let current = db.frozen_barrier(&released.barrier_id).unwrap().unwrap();
        assert!(current.revoked_seq.is_some(), "memory invalidation must revoke a released member's barrier");
        assert_eq!(current.released_seq, released.released_seq);
        assert!(db.frozen_barrier(&pending.barrier_id).unwrap().unwrap().revoked_seq.is_none());
        let count = head_of(&db);
        db.connection.execute("INSERT OR IGNORE INTO memory_invalidations VALUES('scoped','alpha','cause',NULL,'stop_at_checkpoint',?1,NULL,'changed source')", [cause]).unwrap();
        assert_eq!(head_of(&db), count);
        db.connection.execute("INSERT INTO memory_invalidations VALUES('global',NULL,'cause',NULL,'reconcile_before_completion',?1,NULL,'global change')", [cause]).unwrap();
        assert!(db.frozen_barrier(&pending.barrier_id).unwrap().unwrap().revoked_seq.is_some());
        assert_eq!(attempt_row(&db.connection, "attempt-alpha"), before_attempt);
        assert_eq!(db.connection.query_row("SELECT count(*) FROM events WHERE kind='barrier.revoked'", [], |row| row.get::<_,u64>(0)).unwrap(), 2);
        db.connection.execute("UPDATE memory_invalidations SET resolved_seq=?1 WHERE resolved_seq IS NULL", [head_of(&db)]).unwrap();
        assert!(db.frozen_barrier(&released.barrier_id).unwrap().unwrap().revoked_seq.is_some());
        assert!(super::super::satisfaction::record_verified_result(&db.connection, &released.members[0].result_id).is_err());
    }

    #[test]
    fn released_barrier_revocation_preserves_signed_history_and_blocks_reuse() {
        let (_dir, mut db, prepared) = authorized_fixture();
        let released = db.release_authorized_barrier(&prepared).unwrap();
        let member = released.members[0].clone();
        satisfy(&db.connection, "downstream", &member.task_id, &member.result_id);
        let before_attempt = attempt_row(&db.connection, &member.attempt_id);
        assert!(db.connection.execute("INSERT INTO barrier_release_revocations VALUES(?1,?2)", params![released.barrier_id, released.released_seq]).is_err());
        let unrelated = insert_event(&db.connection, "fixture", "other", &serde_json::json!({})).unwrap();
        assert!(db.connection.execute("INSERT INTO barrier_release_revocations VALUES(?1,?2)", params![released.barrier_id, unrelated]).is_err());
        let before = head_of(&db);
        db.connection.execute_batch("CREATE TRIGGER fail_revocation BEFORE INSERT ON barrier_release_revocations BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
        assert!(db.revoke_barrier(&released.barrier_id, before).is_err());
        assert_eq!(head_of(&db), before);
        assert_eq!(db.frozen_barrier(&released.barrier_id).unwrap(), Some(released.clone()));
        db.connection.execute_batch("DROP TRIGGER fail_revocation").unwrap();
        let revoked = db.revoke_barrier(&released.barrier_id, head_of(&db))
            .expect("a released barrier must remain revocable without deleting its receipt");
        assert_eq!(revoked.released_seq, released.released_seq);
        assert!(revoked.revoked_seq.is_some());
        assert_eq!(db.revoke_barrier(&released.barrier_id, 0).unwrap(), revoked);
        assert!(db.connection.execute("UPDATE barrier_release_revocations SET sequence=sequence", []).is_err());
        assert!(db.connection.execute("DELETE FROM barrier_release_revocations", []).is_err());
        assert_eq!(attempt_row(&db.connection, &member.attempt_id), before_attempt);
        assert!(super::super::satisfaction::record_verified_result(&db.connection, &member.result_id).is_err());
        assert_eq!(db.release_authorized_barrier(&prepared).unwrap(), revoked);
        let raw: Vec<u8> = db.connection.query_row("SELECT raw FROM barrier_release_authorizations WHERE barrier_id=?1", [&released.barrier_id], |row| row.get(0)).unwrap();
        assert_eq!(raw, prepared.raw);
        let brief = db.record_stale_brief(&released.barrier_id, &member.attempt_id, "late released-wave brief", head_of(&db)).unwrap();
        assert!(!brief.accepted);
        assert!(db.accept_stale_brief(&brief.brief_id).is_err());
    }

    #[test]
    fn ancestor_memory_expiry_during_later_release_rolls_back_publication() {
        use std::{sync::{Arc,atomic::{AtomicBool,Ordering}},time::Duration};
        let expires=jiff::Timestamp::now().as_millisecond()+2000;
        let (_dir,mut db,reservation,ancestor,submission)=downstream_result_fixture_with_expiry(Some(expires));
        let submitted=db.submit_result(&serde_json::to_vec(&submission).unwrap()).unwrap();
        let target=db.load_verify_target(&submitted.submission_id,"policy-1").unwrap();
        let payload="ed".repeat(32);let tree="d".repeat(40);
        let receipt=crate::verification::testing_receipt(&target,&payload,"expiry-check",&target.candidate_oid,&tree);
        let (run,receipt)=db.commit_verification(&target,super::super::verification::RunDraft {
            idempotency_key:"expiry-check".into(),payload_digest:payload,argv:vec![],libraries:vec![],tree_oid:Some(tree),exit_status:Some(0),reason:None,receipt:Some(receipt),
        }).unwrap();
        let result=receipt.unwrap().result_id().to_string();
        let frozen=db.freeze_barrier(&[BarrierMember {task_id:"a".into(),contract_revision:1,attempt_id:reservation.record.attempt.as_str().into(),result_id:result,verification_id:run.run_id,integration_id:None,proposal_dispositions:vec![]}],head_of(&db)).unwrap();
        let profile=reservation.record.inputs.effective_profile.as_ref().unwrap();
        let document=db.draft_barrier_release(&frozen.barrier_id,profile.permission_policy.clone(),reservation.record.inputs.config.digest.as_deref().unwrap(),jiff::Timestamp::now().as_millisecond()+60_000).unwrap();
        let prepared=PreparedBarrierRelease::parse_verified(&serde_json::to_vec(&document).unwrap()).unwrap();
        let before=head_of(&db);let before_attempt=attempt_row(&db.connection,reservation.record.attempt.as_str());
        let reached=Arc::new(AtomicBool::new(false));let observed=reached.clone();
        db.connection.update_hook(Some(move |_:rusqlite::hooks::Action,_:&str,table:&str,_:i64| {
            if table=="barrier_release_authorizations" {observed.store(true,Ordering::SeqCst);while jiff::Timestamp::now().as_millisecond()<=expires {std::thread::sleep(Duration::from_millis(5));}}
        }));
        let result=db.release_authorized_barrier(&prepared);
        db.connection.update_hook(None::<fn(rusqlite::hooks::Action,&str,&str,i64)>);
        assert!(reached.load(Ordering::SeqCst));
        assert!(matches!(result,Err(StoreError::Invalid(ref reason)) if reason.contains("memory evidence expired")),"{result:?}");
        assert_eq!(head_of(&db),before);
        assert!(db.frozen_barrier(&frozen.barrier_id).unwrap().unwrap().released_seq.is_none());
        assert_eq!(db.frozen_barrier(&ancestor.barrier_id).unwrap().unwrap().release_reference,Some(ancestor));
        assert_eq!(attempt_row(&db.connection,reservation.record.attempt.as_str()),before_attempt);
    }

    #[test]
    fn consumed_memory_expiry_during_release_rolls_back_but_optional_noise_does_not() {
        use std::{sync::{Arc,atomic::{AtomicBool,Ordering}},time::Duration};
        for mode in ["direct","source","unrelated","optional_contract"] {
            let consumed=matches!(mode,"direct"|"source");
            let (_dir,mut db,original)=authorized_fixture();
            optional_head(&db.connection,if mode=="optional_contract" {"contract"} else {"observation"});
            let expires=jiff::Timestamp::now().as_millisecond()+1000;
            let expiring=if mode=="source" {
                db.connection.execute("INSERT INTO memory_records SELECT 'source','source',scope_id,kind,is_hard FROM memory_records WHERE id='note'",[]).unwrap();
                db.connection.execute("INSERT INTO memory_revisions SELECT 'source',revision,body_hash,provenance_hash,promoted_seq,applicability FROM memory_revisions WHERE record_id='note'",[]).unwrap();
                db.connection.execute("INSERT INTO memory_validity SELECT 'source',revision,state,reason,expiry_unix_ms,evaluated_seq FROM memory_validity WHERE record_id='note'",[]).unwrap();
                db.connection.execute("INSERT INTO memory_heads SELECT 'source',revision,status,row_revision FROM memory_heads WHERE record_id='note'",[]).unwrap();
                db.connection.execute("INSERT INTO memory_dependencies VALUES('note',1,'source',1,'supports')",[]).unwrap();
                "source"
            } else {"note"};
            db.connection.execute("UPDATE memory_validity SET expiry_unix_ms=?1 WHERE record_id=?2",params![expires,expiring]).unwrap();
            if consumed {db.connection.execute("INSERT INTO snapshot_entries VALUES('snap-alpha',1,'note',1,'optional','fixture')",[]).unwrap();}
            let members=db.frozen_barrier(&original.document.barrier_id).unwrap().unwrap().members;
            let frozen=db.freeze_barrier(&members,head_of(&db)).unwrap();
            let document=db.draft_barrier_release(&frozen.barrier_id,original.document.authority.clone(),&original.document.config_digest,jiff::Timestamp::now().as_millisecond()+60_000).unwrap();
            let prepared=PreparedBarrierRelease::parse_verified(&serde_json::to_vec(&document).unwrap()).unwrap();
            let before=head_of(&db);let before_attempt=attempt_row(&db.connection,"attempt-alpha");
            let reached=Arc::new(AtomicBool::new(false));let observed=reached.clone();
            db.connection.update_hook(Some(move |_:rusqlite::hooks::Action,_:&str,table:&str,_:i64| {
                if table=="barrier_release_authorizations" {
                    observed.store(true,Ordering::SeqCst);
                    while jiff::Timestamp::now().as_millisecond()<=expires {std::thread::sleep(Duration::from_millis(5));}
                }
            }));
            let result=db.release_authorized_barrier(&prepared);
            db.connection.update_hook(None::<fn(rusqlite::hooks::Action,&str,&str,i64)>);
            assert!(reached.load(Ordering::SeqCst),"fixture did not reach publication");
            if consumed {
                assert!(result.is_err(),"release committed after consumed memory expired");
                assert_eq!(head_of(&db),before);
                assert!(db.frozen_barrier(&frozen.barrier_id).unwrap().unwrap().released_seq.is_none());
                assert_eq!(db.connection.query_row("SELECT count(*) FROM barrier_release_authorizations",[],|row|row.get::<_,u64>(0)).unwrap(),0);
            } else {
                let released=result.expect("unconsumed optional expiry must not block release");
                assert_eq!(db.release_authorized_barrier(&prepared).unwrap(),released);
                assert_eq!(head_of(&db),before+1);
            }
            assert_eq!(attempt_row(&db.connection,"attempt-alpha"),before_attempt);
        }
    }

    #[test]
    fn authorized_barrier_release_expiry_includes_receipt_publication() {
        use std::{sync::{Arc,atomic::{AtomicBool,Ordering}},time::Duration};
        for delay_publication in [true,false] {
            let (_dir,mut db,mut prepared)=authorized_fixture();
            prepared.document.expires_unix_ms=jiff::Timestamp::now().as_millisecond()+1000;
            let prepared=PreparedBarrierRelease::parse_verified(&serde_json::to_vec(&prepared.document).unwrap()).unwrap();
            let expires=prepared.document.expires_unix_ms;
            let before=head_of(&db);
            let before_attempt=attempt_row(&db.connection,"attempt-alpha");
            let reached=Arc::new(AtomicBool::new(false));let observed=reached.clone();
            if delay_publication {
                // Hold the actual receipt insertion after readiness and header
                // publication. No forged expiry or clock change is involved.
                db.connection.update_hook(Some(move |_:rusqlite::hooks::Action,_:&str,table:&str,_:i64| {
                    if table=="barrier_release_authorizations" {
                        observed.store(true,Ordering::SeqCst);
                        while jiff::Timestamp::now().as_millisecond()<=expires {std::thread::sleep(Duration::from_millis(5));}
                    }
                }));
            }
            let result=db.release_authorized_barrier(&prepared);
            db.connection.update_hook(None::<fn(rusqlite::hooks::Action,&str,&str,i64)>);
            if delay_publication {
                assert!(reached.load(Ordering::SeqCst),"fixture did not reach receipt insertion");
                assert!(matches!(result,Err(StoreError::Conflict)),"expired publication was accepted: {result:?}");
                assert_eq!(head_of(&db),before);
                assert!(db.frozen_barrier(&prepared.document.barrier_id).unwrap().unwrap().released_seq.is_none());
                assert_eq!(db.connection.query_row("SELECT count(*) FROM barrier_release_authorizations",[],|row|row.get::<_,u64>(0)).unwrap(),0);
            } else {
                let released=result.unwrap();
                while jiff::Timestamp::now().as_millisecond()<=expires {std::thread::sleep(Duration::from_millis(5));}
                assert_eq!(db.release_authorized_barrier(&prepared).unwrap(),released,"expiry must not erase a committed historical receipt");
                assert_eq!(head_of(&db),before+1);
            }
            assert_eq!(attempt_row(&db.connection,"attempt-alpha"),before_attempt);
        }
    }

    #[test]
    fn authorized_barrier_release_expiry_includes_sqlite_lock_wait() {
        let (_dir, mut store, mut prepared) = authorized_fixture();
        store.connection.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
        let path = store.connection.path().unwrap().to_owned();
        let (locked_send, locked_recv) = std::sync::mpsc::channel();
        let (expiry_send, expiry_recv) = std::sync::mpsc::channel::<i64>();
        let holder = std::thread::spawn(move || {
            let mut connection = Connection::open(path).unwrap();
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate).unwrap();
            locked_send.send(()).unwrap();
            let expires = expiry_recv.recv().unwrap();
            while jiff::Timestamp::now().as_millisecond() <= expires {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            tx.commit().unwrap();
        });
        locked_recv.recv().unwrap();
        prepared.document.expires_unix_ms = jiff::Timestamp::now().as_millisecond()+250;
        let prepared = PreparedBarrierRelease::parse_verified(&serde_json::to_vec(&prepared.document).unwrap()).unwrap();
        let before = head(&store.connection).unwrap();
        expiry_send.send(prepared.document.expires_unix_ms).unwrap();
        let result = store.release_authorized_barrier(&prepared);
        holder.join().unwrap();
        assert!(result.is_err());
        assert_eq!(head(&store.connection).unwrap(), before);
        assert!(load(&store.connection, &prepared.document.barrier_id).unwrap().unwrap().released_seq.is_none());
    }

    #[test]
    fn authorized_barrier_release_is_atomic_exact_and_replayable() {
        let (_dir, mut store, prepared) = authorized_fixture();
        let before = head(&store.connection).unwrap();
        store.connection.execute_batch("CREATE TRIGGER fail_authorization BEFORE INSERT ON barrier_release_authorizations BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
        assert!(store.release_authorized_barrier(&prepared).is_err());
        assert_eq!(head(&store.connection).unwrap(), before);
        assert!(load(&store.connection, &prepared.document.barrier_id).unwrap().unwrap().released_seq.is_none());
        store.connection.execute_batch("DROP TRIGGER fail_authorization").unwrap();
        let released = store.release_authorized_barrier(&prepared).unwrap();
        let after = head(&store.connection).unwrap();
        assert_eq!(after, before+1);
        assert_eq!(released.released_seq, Some(after));
        assert_eq!(store.release_authorized_barrier(&prepared).unwrap(), released);
        assert_eq!(head(&store.connection).unwrap(), after);
        let raw: Vec<u8> = store.connection.query_row("SELECT raw FROM barrier_release_authorizations", [], |row| row.get(0)).unwrap();
        assert_eq!(raw, prepared.raw);
        let mut different = prepared.raw.clone();
        different.push(b' ');
        let different = PreparedBarrierRelease::parse_verified(&different).unwrap();
        assert!(store.release_authorized_barrier(&different).is_err());
        assert!(store.connection.execute("UPDATE barrier_release_authorizations SET raw=raw", []).is_err());
        assert!(store.connection.execute("DELETE FROM barrier_release_authorizations", []).is_err());
    }

    #[test]
    fn authorized_barrier_release_rechecks_every_fence_before_publication() {
        for fault in ["path", "incarnation", "config", "revision", "epoch", "paused", "reconcile", "head", "manifest", "generation", "future", "expired", "evidence", "revoked", "historical"] {
            let (_dir, mut store, mut prepared) = authorized_fixture();
            match fault {
                "path" => prepared.document.project_store = "/different/state.db".into(),
                "incarnation" => prepared.document.store_incarnation = "11".repeat(32),
                "config" => { store.connection.execute("UPDATE project_control SET config_digest=?1", ["12".repeat(32)]).unwrap(); },
                "revision" => { store.connection.execute("UPDATE project_control SET revision=revision+1", []).unwrap(); },
                "epoch" => { store.connection.execute("UPDATE project_control SET epoch=epoch+1", []).unwrap(); },
                "paused" => { store.connection.execute("UPDATE project_control SET state='paused'", []).unwrap(); },
                "reconcile" => { store.connection.execute("UPDATE project_control SET reconciliation_required=1", []).unwrap(); },
                "head" => prepared.document.expected_head += 1,
                "manifest" => prepared.document.memory_manifest_digest = "13".repeat(32),
                "generation" => prepared.document.required_set_generation += 1,
                "future" => { prepared.document.issued_unix_ms += 120_000; prepared.document.expires_unix_ms += 120_000; },
                "expired" => { prepared.document.issued_unix_ms = 0; prepared.document.expires_unix_ms = 1; },
                "evidence" => { store.connection.execute("UPDATE tasks SET revision=revision+1 WHERE id='alpha'", []).unwrap(); },
                "revoked" => { store.revoke_barrier(&prepared.document.barrier_id, head(&store.connection).unwrap()).unwrap(); },
                "historical" => {
                    let barrier = load(&store.connection, &prepared.document.barrier_id).unwrap().unwrap();
                    store.release_barrier(&barrier.barrier_id, &barrier.release_token, head(&store.connection).unwrap(), jiff::Timestamp::now().as_millisecond()).unwrap();
                },
                _ => unreachable!(),
            }
            let prepared = PreparedBarrierRelease::parse_verified(&serde_json::to_vec(&prepared.document).unwrap()).unwrap();
            let before = head(&store.connection).unwrap();
            assert!(store.release_authorized_barrier(&prepared).is_err(), "{fault}");
            assert_eq!(head(&store.connection).unwrap(), before, "{fault}");
            assert_eq!(store.connection.query_row("SELECT count(*) FROM barrier_release_authorizations", [], |row| row.get::<_,i64>(0)).unwrap(), 0, "{fault}");
        }
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
        crate::store::test_schema::historical(&db.connection, 39).unwrap();
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
        assert_eq!(user_version(&db.connection), crate::store::SCHEMA);
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
        assert_eq!(user_version(&reopened.connection), crate::store::SCHEMA);
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
    fn barrier_rechecks_latest_contract_before_freeze_and_release() {
        let (_dir, mut db) = open_store();
        let seeded = seed(&db.connection, "alpha", "verify_only");
        let member = member_of(&seeded, vec![]);
        let frozen = db.freeze_barrier(&[member.clone()], head_of(&db)).unwrap();
        let next =
            PreparedContract::parse_verified(&contract_bytes("alpha", "verify_only", 2)).unwrap();
        db.connection.execute(
            "INSERT INTO task_contracts SELECT task_id,2,plan_revision,project_store,expected_head,repository,base_oid,object_format,memory_snapshot_id,route,?1,?2,installed_seq FROM task_contracts WHERE task_id='alpha' AND contract_revision=1",
            params![next.raw,next.digest],
        ).unwrap();
        db.connection
            .execute(
                "INSERT INTO acceptance_policies VALUES('alpha',2,'policy-1','{}')",
                [],
            )
            .unwrap();
        let before = head_of(&db);
        let error = db
            .release_barrier(&frozen.barrier_id, &frozen.release_token, before, 1)
            .unwrap_err();
        assert!(
            matches!(error,StoreError::Invalid(ref message) if message.contains("contract is no longer current")),
            "{error:?}"
        );
        assert!(db.freeze_barrier(&[member], before).is_err());
        assert_eq!(head_of(&db), before);
        assert!(expanded_released(&db, &frozen.barrier_id).is_none());
    }

    #[test]
    fn barrier_rechecks_frozen_memory_manifest_at_release() {
        let (_dir, mut db) = open_store();
        let seeded = seed(&db.connection, "alpha", "verify_only");
        let member = member_of(&seeded, vec![]);
        let frozen = db.freeze_barrier(&[member.clone()], head_of(&db)).unwrap();
        db.connection.execute(
            "INSERT INTO memory_snapshots SELECT 'replacement-snapshot',task_id,task_revision,profile_name,profile_digest,config_digest,selection_policy_version,estimator,sequence,required_bytes,optional_bytes,budget_bytes,omitted_optional_count,?1,scope_digest FROM memory_snapshots WHERE id='snap-alpha'",
            ["22".repeat(32)],
        ).unwrap();
        db.connection.execute("UPDATE attempts SET snapshot='replacement-snapshot',revision=revision+1 WHERE id=?1",[&seeded.attempt]).unwrap();
        let before = head_of(&db);
        let error = db
            .release_barrier(&frozen.barrier_id, &frozen.release_token, before, 1)
            .unwrap_err();
        assert!(
            matches!(error,StoreError::Invalid(ref message) if message.contains("memory manifest changed")),
            "{error:?}"
        );
        assert_eq!(head_of(&db), before);
        assert!(expanded_released(&db, &frozen.barrier_id).is_none());
        let fresh = db.freeze_barrier(&[member], before).unwrap();
        assert_ne!(fresh.barrier_id, frozen.barrier_id);
    }

    #[test]
    fn barrier_rejects_verification_not_bound_to_signed_policy_bytes() {
        let (_dir, mut db) = open_store();
        let seeded = seed(&db.connection, "alpha", "verify_only");
        // Fault injection: even internally consistent result/run copies cannot
        // replace the digest of the policy retained in the signed contract.
        db.connection.execute_batch("DROP TRIGGER verification_runs_no_update; DROP TRIGGER verified_results_no_update;").unwrap();
        db.connection
            .execute(
                "UPDATE verification_runs SET policy_digest=?1",
                ["33".repeat(32)],
            )
            .unwrap();
        db.connection
            .execute(
                "UPDATE verified_results SET policy_digest=?1",
                ["33".repeat(32)],
            )
            .unwrap();
        let before = head_of(&db);
        let error = db
            .freeze_barrier(&[member_of(&seeded, vec![])], before)
            .unwrap_err();
        assert!(
            matches!(error,StoreError::Invalid(ref message) if message.contains("policy digest")),
            "{error:?}"
        );
        assert_eq!(head_of(&db), before);
    }

    #[test]
    fn barrier_rejects_mismatched_provenance_at_freeze_and_release() {
        for mutation in [
            "UPDATE verified_results SET receipt_digest=lower(hex(zeroblob(32)))",
            "UPDATE verified_results SET tree_oid=lower(hex(zeroblob(20)))",
            "UPDATE verified_results SET memory_fence=memory_fence+1",
            "UPDATE verification_runs SET contract_digest=lower(hex(zeroblob(32)))",
            "UPDATE verification_runs SET policy_id='unsigned-policy'",
            "UPDATE result_submissions SET candidate_oid=lower(hex(zeroblob(20)))",
        ] {
            let (_dir, mut db) = open_store();
            let seeded = seed(&db.connection, "alpha", "verify_only");
            let member = member_of(&seeded, vec![]);
            let frozen = db.freeze_barrier(&[member.clone()], head_of(&db)).unwrap();
            // Inject inconsistent projections/receipts without changing the
            // retained signed contract, to exercise each consumption boundary.
            db.connection.execute_batch("DROP TRIGGER verification_runs_no_update; DROP TRIGGER verified_results_no_update; DROP TRIGGER result_submissions_no_update;").unwrap();
            db.connection
                .execute(
                    "INSERT INTO acceptance_policies VALUES('alpha',1,'unsigned-policy','{}')",
                    [],
                )
                .unwrap();
            db.connection.execute_batch(mutation).unwrap();
            let before = head_of(&db);
            assert!(db.freeze_barrier(&[member], before).is_err(), "{mutation}");
            assert!(
                db.release_barrier(&frozen.barrier_id, &frozen.release_token, before, 1)
                    .is_err(),
                "{mutation}"
            );
            assert_eq!(head_of(&db), before);
            assert!(expanded_released(&db, &frozen.barrier_id).is_none());
        }
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
        assert!(
            super::super::satisfaction::dependency_blocker(
                &db.connection,
                "downstream",
                &predecessor,
                DependencyRequirement::VerifiedResult,
                true
            )
            .unwrap()
            .is_none()
        );
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
        assert!(
            super::super::satisfaction::dependency_blocker(
                &db.connection,
                "downstream",
                &predecessor,
                DependencyRequirement::VerifiedResult,
                true
            )
            .unwrap()
            .is_some()
        );
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
        assert!(
            db.connection
                .execute("UPDATE barrier_stale_briefs SET accepted=1", [])
                .is_err()
        );
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

    fn valid_satisfactions(db: &Connection, predecessor: &str) -> i64 {
        db.query_row(
            "SELECT count(*) FROM dependency_satisfactions WHERE predecessor_task=?1 AND state='valid'",
            [predecessor],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn revoked_barrier_blocks_reattach_and_later_evidence_until_a_later_release() {
        let (_dir, mut db) = open_store();
        let seeded = seed(&db.connection, "alpha", "verify_only");
        satisfy(&db.connection, "downstream", "alpha", &seeded.result);
        let head = head_of(&db);
        let frozen = db
            .freeze_barrier(&[member_of(&seeded, vec![])], head)
            .unwrap();
        let head = head_of(&db);
        db.revoke_barrier(&frozen.barrier_id, head).unwrap();
        let valid = valid_satisfactions(&db.connection, "alpha");
        let same =
            super::super::satisfaction::record_verified_result(&db.connection, &seeded.result);
        assert!(
            matches!(same, Err(StoreError::Invalid(ref message)) if message.contains("dependency blocked")),
            "{same:?}"
        );
        let (later_result, _) = add_result(&db.connection, &seeded, "alpha-later");
        let later =
            super::super::satisfaction::record_verified_result(&db.connection, &later_result);
        assert!(
            matches!(later, Err(StoreError::Invalid(ref message)) if message.contains("dependency blocked")),
            "{later:?}"
        );
        assert_eq!(valid_satisfactions(&db.connection, "alpha"), valid);
        let predecessor = Task {
            id: TaskId::new("alpha").unwrap(),
            revision: 1,
            state: TaskState::Running,
            title: "alpha".into(),
            active_attempt: Some(AttemptId::new(&seeded.attempt).unwrap()),
        };
        assert!(
            super::super::satisfaction::dependency_blocker(
                &db.connection,
                "downstream",
                &predecessor,
                DependencyRequirement::VerifiedResult,
                true
            )
            .unwrap()
            .is_some()
        );
        let other = seed(&db.connection, "beta", "verify_only");
        let head = head_of(&db);
        let released = db
            .freeze_barrier(
                &[member_of(&seeded, vec![]), member_of(&other, vec![])],
                head,
            )
            .unwrap();
        let head = head_of(&db);
        db.release_barrier(&released.barrier_id, &released.release_token, head, 1)
            .unwrap();
        super::super::satisfaction::record_verified_result(&db.connection, &later_result).unwrap();
        assert!(valid_satisfactions(&db.connection, "alpha") >= valid);
        assert!(
            super::super::satisfaction::dependency_blocker(
                &db.connection,
                "downstream",
                &predecessor,
                DependencyRequirement::VerifiedResult,
                true
            )
            .unwrap()
            .is_none()
        );
        // A superseding release is useful only while it remains applicable.
        db.revoke_barrier(&released.barrier_id, head_of(&db)).unwrap();
        assert!(super::super::satisfaction::record_verified_result(&db.connection, &later_result).is_err());
    }

    #[test]
    fn cleared_revocation_and_deleted_revision_are_rejected() {
        let (_dir, mut db) = open_store();
        let seeded = seed(&db.connection, "alpha", "verify_only");
        let head = head_of(&db);
        let frozen = db
            .freeze_barrier(&[member_of(&seeded, vec![])], head)
            .unwrap();
        let head = head_of(&db);
        db.revoke_barrier(&frozen.barrier_id, head).unwrap();
        assert!(
            db.connection
                .execute(
                    "UPDATE barrier_revisions SET revoked_seq=NULL WHERE barrier_id=?1",
                    [&frozen.barrier_id],
                )
                .is_err()
        );
        assert!(db
            .connection
            .execute(
                "UPDATE barrier_revisions SET released_seq=revoked_seq, revoked_seq=NULL WHERE barrier_id=?1",
                [&frozen.barrier_id],
            )
            .is_err());
        db.connection
            .execute_batch("PRAGMA foreign_keys=OFF")
            .unwrap();
        assert!(
            db.connection
                .execute("DELETE FROM barrier_revisions", [])
                .is_err()
        );
        let still: i64 = db
            .connection
            .query_row("SELECT count(*) FROM barrier_revisions", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(still, 1);
    }
}

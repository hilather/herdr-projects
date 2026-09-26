//! Immutable review decisions and atomic promotion. Delivery ack is T06.3.
use super::*;
use crate::domain::{DelegationAction, MemoryKind, MemoryRecordId, NewRevision, ObjectId, PreparedDelegation, PromotionReceipt, ProposalDocument, ReviewDecision};
use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};

fn invalid(s:&str)->StoreError {StoreError::Invalid(s.into())}
fn schema(db:&Connection)->Result<()> {
    check_schema(db)?;let n:u32=db.query_row("PRAGMA user_version",[],|r|r.get(0))?;
    if n<22 {return Err(StoreError::UnsupportedSchema(n));}Ok(())
}
// A truncated queue would hide a proposal this grant may approve.
const REVIEWER_QUEUE_LIMIT: usize = 1_000;

fn result_id_ok(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
fn load_review_grant(db: &Connection, grant_id: &str, now: i64) -> Result<PreparedDelegation> {
    super::delivery::now_check(now)?;
    let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < 34 { return Err(StoreError::UnsupportedSchema(version)); }
    if !result_id_ok(grant_id) { return Err(invalid("delegation grant is missing")); }
    let loaded: Option<(Vec<u8>, String)> = db.query_row(
        "SELECT raw_bytes, raw_digest FROM delegation_grants WHERE id=?1",
        [grant_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional()?;
    let Some((raw, digest)) = loaded else { return Err(invalid("delegation grant is missing")); };
    if digest != grant_id || format!("{:x}", Sha256::digest(&raw)) != digest {
        return Err(StoreError::Corrupt("delegation grant digest mismatch".into()));
    }
    let parsed = PreparedDelegation::parse_verified(&raw).map_err(|error| invalid(&error))?;
    if parsed.digest != digest { return Err(StoreError::Corrupt("delegation grant digest mismatch".into())); }
    let path = db.path().ok_or_else(|| invalid("delegation requires a file-backed store"))?;
    let actual = std::fs::canonicalize(path).map(|path| path.to_string_lossy().into_owned()).map_err(|_| invalid("delegation store path unavailable"))?;
    if parsed.project_store != actual { return Err(invalid("delegation belongs to another project")); }
    if now >= parsed.expires_unix_ms { return Err(invalid("delegation grant is expired")); }
    let revoked: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM delegation_revocations WHERE grant_id=?1)", [grant_id], |row| row.get(0))?;
    if revoked { return Err(invalid("delegation grant is revoked")); }
    if !parsed.actions.contains(&DelegationAction::ReviewMemory) {
        return Err(invalid("delegation does not grant review_memory"));
    }
    Ok(parsed)
}
fn reviewer_owns_attempt(db: &Connection, attempt_id: &str, task_id: &str, subject: &str) -> Result<bool> {
    // `worker:<attempt_id>` is the reservation slot name, not the grant subject.
    let reservation: Option<String> = db.query_row(
        "SELECT reservation FROM attempts WHERE id=?1 AND task_id=?2",
        params![attempt_id, task_id],
        |row| row.get(0),
    ).optional()?;
    let Some(reservation) = reservation else { return Err(invalid("producer attempt is missing")); };
    Ok(attempt_id == subject || reservation == subject || reservation == format!("worker:{subject}"))
}
fn proposal_targets_hard(db: &Connection, doc: &ProposalDocument) -> Result<bool> {
    for change in &doc.changes {
        if matches!(change.kind.as_str(), "constraint" | "hard_memory") { return Ok(true); }
        let row: Option<(i64, String)> = db.query_row(
            "SELECT is_hard, kind FROM memory_records WHERE record_key=?1",
            [change.record_key.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?;
        if row.is_some_and(|(hard, kind)| hard == 1 || matches!(kind.as_str(), "constraint" | "hard_memory")) {
            return Ok(true);
        }
    }
    Ok(false)
}
fn deny_reviewer_proposal(db: &Connection, attempt_id: &str, task_id: &str, payload: &str, grant: &PreparedDelegation) -> Result<()> {
    let doc: ProposalDocument = serde_json::from_str(payload).map_err(|_| invalid("stored proposal is unreadable"))?;
    if doc.producer.attempt_id != attempt_id || doc.producer.task_id != task_id {
        return Err(invalid("proposal identity mismatch"));
    }
    if reviewer_owns_attempt(db, attempt_id, task_id, &grant.subject)? {
        return Err(invalid("reviewer cannot approve their own attempt"));
    }
    if proposal_targets_hard(db, &doc)? { return Err(invalid("reviewer cannot approve a hard rule")); }
    // Review scope is the repository path; the proposal does not carry a git ref.
    if doc.repository.as_ref().is_some_and(|repo| repo.dirty || !grant.repositories.iter().any(|scope| scope.repository == repo.id)) {
        return Err(invalid("delegation grant is for the wrong repo"));
    }
    Ok(())
}
// `Grant` is constructed by the controller entry points below. Those have no CLI caller yet.
#[allow(dead_code)]
enum PromotionAuthority<'a> {
    Owner(Option<&'a PreparedMemoryReview>),
    Grant(&'a str),
}
impl SqliteStore {
    pub(crate) fn insert_review_decision(&mut self,row:&ReviewDecision,authorization:Option<&PreparedMemoryReview>)->Result<()> {
        if row.id.is_empty()||row.id.len()>128||!matches!(row.decision.as_str(),"approve"|"reject"|"narrow")
            || row.payload_digest.len()!=64 || row.reason.is_empty()||row.reason.len()>4096 {
            return Err(invalid("invalid review decision"));
        }
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;schema(&tx)?;
        if let Some(auth)=authorization {
            if head(&tx)?!=auth.document.expected_head || control::read(&tx)?.config_digest!=auth.config_digest {return Err(StoreError::Conflict);}
            if auth.document.read_set_version==Some(2) {
                let signed=auth.document.read_set.as_ref().ok_or_else(||invalid("v2 review requires a read set"))?;
                super::read_set::require_match(&tx,signed)?;
            }
        } else if !cfg!(test) {return Err(invalid("verified reviewer authority required"));}
        tx.execute(
            "INSERT INTO review_decisions VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![row.id,row.proposal_id,row.payload_digest,row.decision,row.classification,row.reviewed_heads,row.reason,row.created_unix_ms]
        )?;
        tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('memory.review_recorded',?1,1,1,?2)",
            params![row.id,serde_json::json!({"proposal":row.proposal_id,"decision":row.decision}).to_string()])?;
        tx.commit()?;Ok(())
    }
    pub fn review_decision(&mut self,id:&str)->Result<Option<ReviewDecision>> {
        let tx=self.connection.transaction()?;schema(&tx)?;
        Ok(tx.query_row(
            "SELECT id,proposal_id,payload_digest,decision,classification,reviewed_heads,reason,created_unix_ms FROM review_decisions WHERE id=?1",
            [id],|r|Ok(ReviewDecision{id:r.get(0)?,proposal_id:r.get(1)?,payload_digest:r.get(2)?,decision:r.get(3)?,classification:r.get(4)?,reviewed_heads:r.get(5)?,reason:r.get(6)?,created_unix_ms:r.get(7)?})
        ).optional()?)
    }
    pub fn memory_promotion(&mut self,proposal_id:&str)->Result<Option<PromotionReceipt>> {
        let tx=self.connection.transaction()?;schema(&tx)?;
        Ok(tx.query_row(
            "SELECT proposal_id,decision_id,sequence,change_ids FROM memory_promotions WHERE proposal_id=?1",
            [proposal_id],|r| {
                let change_ids:String=r.get(3)?;
                Ok(PromotionReceipt{proposal_id:r.get(0)?,decision_id:r.get(1)?,sequence:r.get(2)?,change_ids:serde_json::from_str(&change_ids).unwrap_or_default(),reused:true})
            }
        ).optional()?)
    }
    pub(crate) fn verified_result_claim(&self, result_id: &str) -> Result<Option<(String, String, String)>> {
        if !result_id_ok(result_id) { return Ok(None); }
        let version: u32 = self.connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version < 27 { return Ok(None); }
        self.connection.query_row(
            "SELECT r.commit_oid, r.tree_oid, s.repository
             FROM verified_results r
             JOIN verification_runs v ON v.run_id = r.run_id
             JOIN result_submissions s ON s.submission_id = r.submission_id
             JOIN task_contracts c ON c.task_id = v.task_id AND c.contract_revision = v.contract_revision
             WHERE r.result_id=?1 AND r.submission_id=v.submission_id AND v.state='accepted'
               AND s.candidate_oid=r.commit_oid AND s.repository=c.repository
               AND r.commit_oid=v.commit_oid AND r.tree_oid=v.tree_oid
               AND r.object_format=v.object_format AND s.object_format=r.object_format",
            [result_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional().map_err(StoreError::from)
    }
    /// Proposals this review_memory grant may see. Not a promotion write.
    #[allow(dead_code)] // Controller entry; no CLI in this change.
    pub(crate) fn reviewer_queue(&mut self, grant_id: &str, now_unix_ms: i64) -> Result<Vec<String>> {
        let tx = self.connection.transaction()?;
        schema(&tx)?;
        let grant = load_review_grant(&tx, grant_id, now_unix_ms)?;
        let repos = serde_json::to_string(&grant.repositories.iter().map(|repo| repo.repository.as_str()).collect::<Vec<_>>())
            .map_err(|_| invalid("delegation repository encoding failed"))?;
        let mut stmt = tx.prepare(
            "SELECT p.id FROM memory_proposals p
             WHERE p.review_state='validated' AND json_valid(p.payload)
               AND EXISTS (SELECT 1 FROM attempts a WHERE a.id=p.attempt_id AND a.task_id=p.task_id)
               AND p.attempt_id <> ?1
               -- worker:<attempt_id> is the reservation slot name, not the grant subject.
               AND NOT EXISTS (
                 SELECT 1 FROM attempts a
                 WHERE a.id=p.attempt_id AND (a.reservation=?1 OR a.reservation='worker:' || ?1)
               )
               AND COALESCE(json_extract(p.payload, '$.repository.dirty'), 0)=0
               -- Review scope is the repository path; the proposal does not carry a git ref.
               AND (
                 json_extract(p.payload, '$.repository.id') IS NULL
                 OR EXISTS (SELECT 1 FROM json_each(?2) repo WHERE repo.value=json_extract(p.payload, '$.repository.id'))
               )
               AND NOT EXISTS (
                 SELECT 1 FROM json_each(p.payload, '$.changes') c
                 LEFT JOIN memory_records r ON r.record_key=json_extract(c.value, '$.record_key')
                 WHERE json_extract(c.value, '$.kind') IN ('constraint', 'hard_memory')
                    OR r.is_hard=1 OR r.kind IN ('constraint', 'hard_memory')
               )
             ORDER BY p.id LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![grant.subject, repos, (REVIEWER_QUEUE_LIMIT as i64) + 1], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        if rows.len() > REVIEWER_QUEUE_LIMIT {
            return Err(StoreError::Limit("reviewer queue exceeds 1000 proposals".into()));
        }
        tx.commit()?;
        Ok(rows)
    }
    /// Records an approve decision. The grant is not the promotion write.
    #[allow(dead_code)] // Controller entry; no CLI in this change.
    pub(crate) fn approve_with_reviewer_grant(&mut self, grant_id: &str, proposal_id: &str, reason: &str, now_unix_ms: i64) -> Result<ReviewDecision> {
        if reason.is_empty() || reason.len() > 4096 || reason.chars().any(char::is_control) {
            return Err(invalid("invalid review reason"));
        }
        let id = format!("rev-{:x}", Sha256::digest(format!("{proposal_id}\0{grant_id}\0{now_unix_ms}\0{reason}").as_bytes()));
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema(&tx)?;
        let grant = load_review_grant(&tx, grant_id, now_unix_ms)?;
        let row: Option<(String, String, String, String, String)> = tx.query_row(
            "SELECT payload_digest, attempt_id, task_id, payload, review_state FROM memory_proposals WHERE id=?1",
            [proposal_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        ).optional()?;
        let Some((digest, attempt_id, task_id, payload, review_state)) = row else {
            return Err(invalid("proposal missing"));
        };
        if review_state != "validated" { return Err(invalid("proposal is not validated")); }
        deny_reviewer_proposal(&tx, &attempt_id, &task_id, &payload, &grant)?;
        if let Some(existing) = tx.query_row(
            "SELECT id,proposal_id,payload_digest,decision,classification,reviewed_heads,reason,created_unix_ms FROM review_decisions WHERE id=?1",
            [&id],
            |r| Ok(ReviewDecision { id: r.get(0)?, proposal_id: r.get(1)?, payload_digest: r.get(2)?, decision: r.get(3)?, classification: r.get(4)?, reviewed_heads: r.get(5)?, reason: r.get(6)?, created_unix_ms: r.get(7)? }),
        ).optional()? {
            let bound = serde_json::from_str::<serde_json::Value>(&existing.reviewed_heads).ok()
                .and_then(|value| value.get("reviewer_grant_id").and_then(|id| id.as_str()).map(str::to_owned));
            if existing.proposal_id != proposal_id || existing.payload_digest != digest || existing.decision != "approve" || existing.reason != reason || bound.as_deref() != Some(grant_id) {
                return Err(invalid("review identity mismatch"));
            }
            tx.commit()?;
            return Ok(existing);
        }
        let event_head = head(&tx)?;
        let decision = ReviewDecision {
            id,
            proposal_id: proposal_id.into(),
            payload_digest: digest,
            decision: "approve".into(),
            classification: "[]".into(),
            reviewed_heads: serde_json::json!({"event_head": event_head, "reviewer_grant_id": grant_id}).to_string(),
            reason: reason.into(),
            created_unix_ms: now_unix_ms,
        };
        tx.execute(
            "INSERT INTO review_decisions VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![decision.id, decision.proposal_id, decision.payload_digest, decision.decision, decision.classification, decision.reviewed_heads, decision.reason, decision.created_unix_ms],
        )?;
        tx.execute(
            "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('memory.review_recorded',?1,1,1,?2)",
            params![decision.id, serde_json::json!({"proposal": decision.proposal_id, "decision": decision.decision}).to_string()],
        )?;
        tx.commit()?;
        Ok(decision)
    }
    pub(crate) fn promote_reviewed_proposal(&mut self,proposal_id:&str,decision:&ReviewDecision,changes:&[crate::domain::NewRevision],invalidations:&[(String,String,String,String)],now_unix_ms:i64,authorization:Option<&PreparedMemoryReview>)->Result<PromotionReceipt> {
        self.finish_promotion(proposal_id, decision, changes, invalidations, now_unix_ms, PromotionAuthority::Owner(authorization))
    }
    #[allow(dead_code)] // Controller entry; no CLI in this change.
    pub(crate) fn promote_with_reviewer_grant(&mut self, proposal_id: &str, decision: &ReviewDecision, changes: &[crate::domain::NewRevision], invalidations: &[(String, String, String, String)], now_unix_ms: i64, grant_id: &str) -> Result<PromotionReceipt> {
        self.finish_promotion(proposal_id, decision, changes, invalidations, now_unix_ms, PromotionAuthority::Grant(grant_id))
    }
    fn finish_promotion(&mut self,proposal_id:&str,decision:&ReviewDecision,changes:&[crate::domain::NewRevision],invalidations:&[(String,String,String,String)],now_unix_ms:i64,authority:PromotionAuthority<'_>)->Result<PromotionReceipt> {
        if decision.decision!="approve" {return Err(invalid("promotion requires an approve decision"));}
        if decision.proposal_id!=proposal_id {return Err(invalid("decision does not belong to proposal"));}
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;schema(&tx)?;
        if let Some(existing)=tx.query_row(
            "SELECT proposal_id,decision_id,sequence,change_ids FROM memory_promotions WHERE proposal_id=?1",
            [proposal_id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,u64>(2)?,r.get::<_,String>(3)?))
        ).optional()? {
            if existing.1!=decision.id {return Err(invalid("proposal already promoted under a different decision"));}
            tx.commit()?;
            return Ok(PromotionReceipt{proposal_id:existing.0,decision_id:existing.1,sequence:existing.2,change_ids:serde_json::from_str(&existing.3).unwrap_or_default(),reused:true});
        }
        let persisted: Option<(String,String,String,String)> = tx.query_row(
            "SELECT proposal_id,payload_digest,decision,reviewed_heads FROM review_decisions WHERE id=?1",
            [&decision.id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
        if persisted != Some((proposal_id.into(),decision.payload_digest.clone(),decision.decision.clone(),decision.reviewed_heads.clone())) {
            return Err(invalid("review identity mismatch"));
        }
        let reviewed: serde_json::Value = serde_json::from_str(&decision.reviewed_heads)
            .map_err(|_| invalid("invalid reviewed state"))?;
        let mut bound_changes: Option<Vec<NewRevision>> = None;
        let mut bound_invalidations: Option<Vec<(String, String, String, String)>> = None;
        let authorization = match authority {
            PromotionAuthority::Owner(Some(auth)) => {
                let covered:MemoryReviewAuthorization=serde_json::from_value(reviewed["authorization"].clone()).map_err(|_|invalid("review authorization missing"))?;
                if covered.expires_unix_ms<=now_unix_ms {
                    return Err(invalid("review authorization is expired"));
                }
                if covered!=auth.document || control::read(&tx)?.config_digest!=auth.config_digest {
                    return Err(invalid("review authority changed"));
                }
                if auth.document.read_set_version==Some(2) {
                    let signed=auth.document.read_set.as_ref().ok_or_else(||invalid("v2 review requires a read set"))?;
                    super::read_set::require_match(&tx,signed)?;
                }
                Some(auth)
            }
            PromotionAuthority::Owner(None) => {
                if !cfg!(test) {return Err(invalid("verified reviewer authority required"));}
                None
            }
            PromotionAuthority::Grant(grant_id) => {
                if reviewed.get("reviewer_grant_id").and_then(|value| value.as_str()) != Some(grant_id) {
                    return Err(invalid("reviewer grant is required"));
                }
                let grant = load_review_grant(&tx, grant_id, now_unix_ms)?;
                let (attempt_id, task_id, payload_digest, payload, review_state): (String, String, String, String, String) = tx.query_row(
                    "SELECT attempt_id, task_id, payload_digest, payload, review_state FROM memory_proposals WHERE id=?1",
                    [proposal_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
                )?;
                if review_state != "validated" { return Err(invalid("proposal is not validated")); }
                if payload_digest != decision.payload_digest {
                    return Err(invalid("review does not cover current proposal digest"));
                }
                deny_reviewer_proposal(&tx, &attempt_id, &task_id, &payload, &grant)?;
                let doc: ProposalDocument = serde_json::from_str(&payload).map_err(|_| invalid("stored proposal is unreadable"))?;
                if doc.changes.is_empty() || changes.is_empty() {
                    return Err(invalid("promotion requires the stored proposal changes"));
                }
                let prov_bytes = serde_json::json!({"proposal": proposal_id, "decision": &decision.id}).to_string();
                let prov = ObjectId::from_hex(format!("{:x}", Sha256::digest(prov_bytes.as_bytes()))).map_err(|error| invalid(&error))?;
                super::objects::require_available(&tx, prov.as_str())?;
                let mut derived = Vec::new();
                let mut derived_inv = Vec::new();
                for change in &doc.changes {
                    let body = ObjectId::parse(&change.body_object).map_err(|error| invalid(&error))?;
                    super::objects::require_available(&tx, body.as_str())?;
                    let existing: Option<(String, i64)> = tx.query_row(
                        "SELECT id, is_hard FROM memory_records WHERE record_key=?1",
                        [change.record_key.as_str()],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    ).optional()?;
                    let (id, expected) = if let Some((record_id, hard)) = existing {
                        if hard == 1 { return Err(invalid("reviewer cannot approve a hard rule")); }
                        let (revision, status): (i64, String) = tx.query_row(
                            "SELECT revision, status FROM memory_heads WHERE record_id=?1",
                            [&record_id],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        ).optional()?.ok_or_else(|| invalid("promotion head missing"))?;
                        if status != "active" { return Err(StoreError::Conflict); }
                        let revision = u64::try_from(revision).map_err(|_| StoreError::Corrupt("memory head revision is corrupt".into()))?;
                        if change.expected.as_ref().is_some_and(|expected| expected.revision != revision || expected.record_id != record_id) {
                            return Err(StoreError::Conflict);
                        }
                        let id = MemoryRecordId::new(record_id).map_err(|error| invalid(&error))?;
                        (id, change.expected.as_ref().map(|expected| expected.revision))
                    } else if change.expected.is_some() {
                        return Err(invalid("expected base record is missing"));
                    } else {
                        let id = MemoryRecordId::new(format!("mem-{:x}", Sha256::digest(change.record_key.as_bytes()))).map_err(|error| invalid(&error))?;
                        (id, None)
                    };
                    let kind = crate::domain::parse_kind(&change.kind).map_err(|error| invalid(&error))?;
                    if matches!(kind, MemoryKind::Constraint | MemoryKind::HardMemory) {
                        return Err(invalid("reviewer cannot approve a hard rule"));
                    }
                    let mut dependencies = Vec::new();
                    for dep in &change.based_on {
                        dependencies.push((MemoryRecordId::new(dep.record_id.clone()).map_err(|error| invalid(&error))?, dep.revision, "based_on".into()));
                    }
                    let record_id = id.as_str().to_owned();
                    derived.push(NewRevision {
                        id, record_key: change.record_key.clone(), scope_id: "project".into(), kind,
                        body_hash: body, provenance_hash: prov.clone(), applicability: change.scope.clone(),
                        dependencies, expected, expiry_unix_ms: None, validity_state: "valid".into(), validity_reason: "promoted".into(),
                    });
                    let inv_id = format!("inv-{:x}", Sha256::digest(format!("{proposal_id}:{record_id}").as_bytes()));
                    derived_inv.push((inv_id, task_id.clone(), record_id, change.impact.clone()));
                }
                let matches_proposal = derived.len() == changes.len() && derived_inv.len() == invalidations.len()
                    && derived.iter().zip(changes).all(|(want, got)| {
                        want.id == got.id && want.record_key == got.record_key && want.scope_id == got.scope_id && want.kind == got.kind
                            && want.body_hash == got.body_hash && want.provenance_hash == got.provenance_hash
                            && want.applicability == got.applicability && want.dependencies == got.dependencies
                            && want.expected == got.expected && want.expiry_unix_ms == got.expiry_unix_ms
                            && want.validity_state == got.validity_state && want.validity_reason == got.validity_reason
                    })
                    && derived_inv.iter().zip(invalidations).all(|(want, got)| want == got);
                if !matches_proposal {
                    return Err(invalid("promotion revisions do not match the stored proposal"));
                }
                bound_changes = Some(derived);
                bound_invalidations = Some(derived_inv);
                None
            }
        };
        let changes = bound_changes.as_deref().unwrap_or(changes);
        let invalidations = bound_invalidations.as_deref().unwrap_or(invalidations);
        let expected = reviewed.get("event_head").and_then(|v| v.as_u64())
            .and_then(|v| v.checked_add(1)).ok_or_else(|| invalid("review event fence missing"))?;
        // Documents without read_set_version 2 stay on the whole-state fence.
        // The only intervening event allowed for those is this review itself.
        let v2=authorization.is_some_and(|auth| auth.document.read_set_version==Some(2));
        if !v2 && head(&tx)? != expected { return Err(StoreError::Conflict); }
        let review_event: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM events WHERE sequence=?1 AND kind='memory.review_recorded' AND entity=?2)",
            params![integer(expected)?,decision.id], |r| r.get(0))?;
        if !review_event { return Err(StoreError::Conflict); }
        for next in changes {
            if super::memory::record(&tx, next.id.as_str())?.is_some_and(|r| r.is_hard || matches!(r.kind, crate::domain::MemoryKind::Constraint | crate::domain::MemoryKind::HardMemory)) {
                return Err(invalid("worker promotion cannot change mandatory rules"));
            }
            if let Some(revision) = next.expected {
                let valid: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM memory_validity WHERE record_id=?1 AND revision=?2 AND state='valid' AND (expiry_unix_ms IS NULL OR expiry_unix_ms>?3))",
                    params![next.id.as_str(),integer(revision)?,now_unix_ms], |r| r.get(0))?;
                if !valid { return Err(StoreError::Conflict); }
            }
            for (source, revision, _) in &next.dependencies {
                let valid: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM memory_heads h JOIN memory_validity v ON v.record_id=h.record_id AND v.revision=h.revision WHERE h.record_id=?1 AND h.revision=?2 AND h.status='active' AND v.state='valid' AND (v.expiry_unix_ms IS NULL OR v.expiry_unix_ms>?3))",
                    params![source.as_str(),integer(*revision)?,now_unix_ms], |r| r.get(0))?;
                if !valid { return Err(StoreError::Conflict); }
            }
        }
        let mut change_ids=Vec::new();
        let mut last_seq=head(&tx)?;
        for next in changes {
            let (head,seq)=super::memory::apply_memory_revision_in_tx(&tx,next)?;
            last_seq=seq;
            change_ids.push(format!("{}:{}",head.record_id.as_str(),head.revision));
            let severity=invalidations.iter().find(|(_,_,id,_)|id==head.record_id.as_str()).map(|(_,_,_,severity)|severity.as_str()).ok_or_else(||invalid("promotion change lacks severity"))?;
            super::memory_delivery::record_change(&tx,proposal_id,head.record_id.as_str(),head.revision,severity,seq)?;
        }
        for (id,task_id,record_id,severity) in invalidations {
            if !matches!(severity.as_str(),"informational"|"reconcile_before_completion"|"stop_at_checkpoint") {
                return Err(invalid("invalid invalidation severity"));
            }
            tx.execute(
                "INSERT INTO memory_invalidations VALUES(?1,?2,?3,?4,?5,?6,NULL,?7)",
                params![id,task_id,proposal_id,record_id,severity,integer(last_seq)?,"promoted"]
            )?;
        }
        let encoded=serde_json::to_string(&change_ids).map_err(|e|invalid(&e.to_string()))?;
        tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('memory.promoted',?1,1,1,?2)",
            params![proposal_id,serde_json::json!({"decision":decision.id,"sequence":last_seq,"changes":change_ids}).to_string()])?;
        last_seq=head(&tx)?;
        tx.execute("INSERT INTO memory_promotions VALUES(?1,?2,?3,?4,?5,?6)",
            params![proposal_id,decision.id,decision.payload_digest,integer(last_seq)?,encoded,now_unix_ms])?;
        tx.commit()?;
        Ok(PromotionReceipt{proposal_id:proposal_id.into(),decision_id:decision.id.clone(),sequence:last_seq,change_ids,reused:false})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn schema21_upgrade_adds_empty_review_tables() {
        let temp=tempfile::tempdir().unwrap();let path=temp.path().join("state.db");
        let mut db=SqliteStore::create(&path).unwrap();
        db.connection.execute_batch("DROP TRIGGER IF EXISTS memory_change_receipts_no_update; DROP TRIGGER IF EXISTS memory_change_receipts_no_delete; DROP TRIGGER IF EXISTS update_package_members_no_update; DROP TRIGGER IF EXISTS update_package_members_no_delete; DROP TRIGGER IF EXISTS update_packages_no_update; DROP TRIGGER IF EXISTS update_packages_no_delete; DROP TABLE IF EXISTS memory_change_receipts; DROP TABLE IF EXISTS update_package_members; ALTER TABLE consumer_bindings DROP COLUMN applied_cursor; DROP TABLE IF EXISTS update_packages; DROP TRIGGER IF EXISTS memory_read_set_on_record; DROP TRIGGER IF EXISTS memory_read_set_on_reclassify; DROP TRIGGER IF EXISTS memory_read_set_on_revision; DROP TRIGGER IF EXISTS memory_read_set_on_validity; DROP TRIGGER IF EXISTS memory_read_set_on_head; DROP TABLE IF EXISTS memory_scope_catalog; DROP TABLE IF EXISTS memory_required_generation; DROP TABLE IF EXISTS consumer_binding_undeliverable; DROP TABLE IF EXISTS consumer_binding_obligations; DROP TABLE IF EXISTS consumer_bindings; DROP TABLE IF EXISTS wait_replay_events; DROP TABLE IF EXISTS replan_requests; DROP TABLE IF EXISTS replan_budget_resets; DROP TABLE IF EXISTS attempt_infrastructure_retries; DROP TABLE IF EXISTS wait_conditions; DROP TABLE IF EXISTS resource_claims; DROP TABLE IF EXISTS delegation_stop_obligations; DROP TABLE IF EXISTS delegation_revocations; DROP TABLE IF EXISTS delegation_grants; DROP TABLE IF EXISTS capability_evidence; DROP TABLE IF EXISTS contract_named_resources; DROP TABLE IF EXISTS contract_scope_paths; DROP TABLE IF EXISTS plan_revisions; DROP TABLE IF EXISTS plan_proposals; DROP TABLE IF EXISTS dependency_satisfactions; DROP TABLE IF EXISTS factory_admission_policies; ALTER TABLE project_control DROP COLUMN factory_admission; DROP TABLE IF EXISTS feedback_claims; DROP TABLE IF EXISTS feedback_items; DROP TABLE IF EXISTS integrated_commits; DROP TABLE IF EXISTS integration_candidates; DROP TABLE IF EXISTS integration_operations; DROP TABLE IF EXISTS integration_target_leases; DROP TABLE IF EXISTS integration_targets; DROP TABLE IF EXISTS verified_results; DROP TABLE IF EXISTS verification_runs; DROP TABLE IF EXISTS result_objects; DROP TABLE IF EXISTS result_submissions; DROP TABLE IF EXISTS acceptance_policies; DROP TABLE IF EXISTS task_contracts; DROP TABLE IF EXISTS native_profiles; DROP TABLE IF EXISTS memory_update_receipts; DROP TABLE IF EXISTS memory_delivery_intents; DROP TABLE IF EXISTS memory_import_decisions; DROP TABLE IF EXISTS memory_import_candidates; DROP TABLE IF EXISTS memory_snapshot_inputs; DROP TABLE memory_invalidations; DROP TABLE memory_promotions; DROP TABLE review_decisions; UPDATE store_meta SET schema_version=21; PRAGMA user_version=21;").unwrap();
        let mut before=db.read_snapshot(None).unwrap();db.upgrade_v1().unwrap();before.schema_version=39;
        assert_eq!(db.read_snapshot(None).unwrap(),before);
        assert!(db.memory_promotion("mp-x").unwrap().is_none());
        db.integrity_check().unwrap();
    }

    use crate::domain::*;
    use crate::memory::MemoryStore;
    use sha2::{Digest, Sha256};

    fn setup() -> (tempfile::TempDir, MemoryStore, String) {
        let root = tempfile::tempdir().unwrap();
        let db = root.path().join("state.db");
        let mut store = SqliteStore::create(&db).unwrap();
        let task = Task { id: TaskId::new("task-api").unwrap(), revision: 1, state: TaskState::Draft, title: "api".into(), active_attempt: None };
        store.commit(Commit { expected_head: 0, mutations: vec![Mutation::Task { expected: None, next: task }] }).unwrap();
        let attempt = Attempt { id: AttemptId::new("att-api-2").unwrap(), task: TaskId::new("task-api").unwrap(), revision: 1, state: AttemptState::Running, snapshot: None, reservation: "held".into(), termination_observed: false };
        let head = store.read_snapshot(None).unwrap().head;
        store.commit(Commit { expected_head: head, mutations: vec![Mutation::Attempt { expected: None, next: attempt }] }).unwrap();
        let mut memory = MemoryStore::from_sqlite(store, db.parent().unwrap().join("objects"));
        let body = memory.ingest_object(&b"claim-body"[..]).unwrap();
        let req = SnapshotRequest { schema_version: 1, task_id: "task-api".into(), profile: "implementation".into(), domains: vec![], paths: vec![], pinned_keys: vec![], sensitivity: "default".into() };
        let snap = memory.create_task_snapshot(req, "implementation", &"a".repeat(64), None, 32_000, "instructions", 1_000, None).unwrap();
        let state = memory.store.read_snapshot(None).unwrap();
        let mut attempt = state.attempts[0].clone();
        attempt.revision += 1;
        attempt.snapshot = Some(snap.id.as_str().into());
        memory.store.commit(Commit { expected_head: state.head, mutations: vec![Mutation::Attempt { expected: Some(1), next: attempt }] }).unwrap();
        let doc = ProposalDocument {
            schema_version: 1,
            proposal_id: "mp-api-errors-01".into(),
            producer: ProposalProducer { task_id: "task-api".into(), attempt_id: "att-api-2".into() },
            input_snapshot_id: snap.id.as_str().into(),
            observed_revisions: vec![],
            repository: None,
            changes: vec![ProposalChange {
                record_key: "api.error-envelope".into(),
                expected: None,
                kind: "observation".into(),
                scope: Applicability { domains: vec!["api".into()], paths: vec!["src/api".into()] },
                claim: "Validation errors contain code, message, and request_id.".into(),
                body_object: format!("sha256:{}", body.as_str()),
                evidence: vec![],
                based_on: vec![],
                impact: "reconcile_before_completion".into(),
            }],
        };
        let receipt = memory.propose(&serde_json::to_vec(&doc).unwrap(), 1_000).unwrap();
        assert_eq!(receipt.validation, "accepted");
        (root, memory, body.as_str().to_string())
    }
    fn install_grant(memory: &mut MemoryStore, actions: &[&str]) -> String {
        let project = std::fs::canonicalize(memory.store.connection.path().unwrap()).unwrap().display().to_string();
        let bytes = serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "issuer": "owner",
            "subject": "delegate",
            "subject_public_key": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIDelegationSubjectKeyExampleValue1234567890",
            "action_classes": actions,
            "repositories": [{"repository": "/tmp/repo", "ref": "refs/heads/factory"}],
            "profile_kinds": ["codex"],
            "max_concurrent_attempts": 1,
            "expires_unix_ms": 9_000_000_000_000i64,
            "revocation_epoch": 1,
            "child_delegation": "forbidden",
            "policy_revision": 1,
            "project_store": project,
            "authority": {"id": "owner-approval-policy", "revision": 1, "digest": "ab".repeat(32)}
        })).unwrap();
        let prepared = PreparedDelegation::parse_verified(&bytes).unwrap();
        memory.store.install_delegation(&prepared, 1_000).unwrap()
    }
    fn stored_promotion(memory: &mut MemoryStore, decision_id: &str) -> (NewRevision, (String, String, String, String)) {
        let prov_bytes = serde_json::json!({"proposal": "mp-api-errors-01", "decision": decision_id}).to_string();
        let prov = memory.ingest_object(prov_bytes.as_bytes()).unwrap();
        let (_, payload) = memory.store.memory_proposal_payload("mp-api-errors-01").unwrap().unwrap();
        let doc: ProposalDocument = serde_json::from_str(&payload).unwrap();
        let change = &doc.changes[0];
        let body = ObjectId::parse(&change.body_object).unwrap();
        let id = MemoryRecordId::new(format!("mem-{:x}", Sha256::digest(change.record_key.as_bytes()))).unwrap();
        let record_id = id.as_str().to_owned();
        let revision = NewRevision {
            id,
            record_key: change.record_key.clone(),
            scope_id: "project".into(),
            kind: MemoryKind::Observation,
            body_hash: body,
            provenance_hash: prov,
            applicability: change.scope.clone(),
            dependencies: vec![],
            expected: None,
            expiry_unix_ms: None,
            validity_state: "valid".into(),
            validity_reason: "promoted".into(),
        };
        let inv_id = format!("inv-{:x}", Sha256::digest(format!("mp-api-errors-01:{record_id}").as_bytes()));
        (revision, (inv_id, doc.producer.task_id, record_id, change.impact.clone()))
    }
    fn revision(body: &str) -> NewRevision {
        NewRevision {
            id: MemoryRecordId::new("mem-errors").unwrap(),
            record_key: "api.error-envelope".into(),
            scope_id: "project".into(),
            kind: MemoryKind::Observation,
            body_hash: ObjectId::from_hex(body).unwrap(),
            provenance_hash: ObjectId::from_hex(body).unwrap(),
            applicability: Applicability { domains: vec!["api".into()], paths: vec!["src/api".into()] },
            dependencies: vec![],
            expected: None,
            expiry_unix_ms: None,
            validity_state: "valid".into(),
            validity_reason: "promoted".into(),
        }
    }

    #[test]
    fn reviewer_cannot_approve_own_attempt() {
        let (_root, mut memory, _body) = setup();
        let grant = install_grant(&mut memory, &["review_memory"]);
        assert_eq!(memory.store.reviewer_queue(&grant, 1_000).unwrap(), vec!["mp-api-errors-01".to_string()]);
        memory.store.connection.execute("UPDATE attempts SET reservation='delegate' WHERE id='att-api-2'", []).unwrap();
        assert!(memory.store.reviewer_queue(&grant, 1_000).unwrap().is_empty());
        let err = memory.store.approve_with_reviewer_grant(&grant, "mp-api-errors-01", "evidence supports the claim", 1_000).unwrap_err();
        assert!(matches!(err, StoreError::Invalid(ref message) if message.contains("own attempt")), "{err:?}");
        assert!(memory.store.memory_promotion("mp-api-errors-01").unwrap().is_none());
    }

    #[test]
    fn reviewer_cannot_approve_a_hard_rule() {
        let (_root, mut memory, body) = setup();
        let grant = install_grant(&mut memory, &["review_memory"]);
        assert_eq!(memory.store.reviewer_queue(&grant, 1_000).unwrap(), vec!["mp-api-errors-01".to_string()]);
        memory.insert_revision(&ControlContext { now_unix_ms: 1_000 }, revision(&body)).unwrap();
        let head = memory.store.read_snapshot(None).unwrap().head;
        memory.store.apply_memory_op(MemoryPolicyOp::HardRule, "api.error-envelope", head).unwrap();
        assert!(memory.store.reviewer_queue(&grant, 1_000).unwrap().is_empty());
        let err = memory.store.approve_with_reviewer_grant(&grant, "mp-api-errors-01", "evidence supports the claim", 1_000).unwrap_err();
        assert!(matches!(err, StoreError::Invalid(ref message) if message.contains("hard rule")), "{err:?}");
        assert!(memory.store.memory_promotion("mp-api-errors-01").unwrap().is_none());
    }

    #[test]
    fn unsigned_promotion_is_still_absent() {
        let (_root, mut memory, _body) = setup();
        let worker = install_grant(&mut memory, &["reserve_attempt"]);
        let err = memory.store.approve_with_reviewer_grant(&worker, "mp-api-errors-01", "evidence supports the claim", 1_000).unwrap_err();
        assert!(matches!(err, StoreError::Invalid(ref message) if message.contains("does not grant review_memory")), "{err:?}");
        let stored = memory.store.memory_proposal("mp-api-errors-01").unwrap().unwrap();
        let row = ReviewDecision {
            id: "rev-unsigned".into(),
            proposal_id: "mp-api-errors-01".into(),
            payload_digest: stored.payload_digest,
            decision: "approve".into(),
            classification: "[]".into(),
            reviewed_heads: r#"{"event_head":1}"#.into(),
            reason: "unsigned".into(),
            created_unix_ms: 1_000,
        };
        memory.store.insert_review_decision(&row, None).unwrap();
        let grant = install_grant(&mut memory, &["review_memory"]);
        let err = memory.store.promote_with_reviewer_grant("mp-api-errors-01", &row, &[], &[], 1_000, &grant).unwrap_err();
        assert!(matches!(err, StoreError::Invalid(ref message) if message.contains("reviewer grant is required")), "{err:?}");
        assert!(memory.store.memory_promotion("mp-api-errors-01").unwrap().is_none());
        assert!(memory.store.memory_record_by_key("api.error-envelope").unwrap().is_none());
    }

    #[test]
    fn reviewer_grant_does_not_promote_until_the_controller_writes() {
        let (_root, mut memory, body) = setup();
        let grant = install_grant(&mut memory, &["review_memory"]);
        assert_eq!(memory.store.reviewer_queue(&grant, 1_000).unwrap(), vec!["mp-api-errors-01".to_string()]);
        let decision = memory.store.approve_with_reviewer_grant(&grant, "mp-api-errors-01", "evidence supports the claim", 1_000).unwrap();
        let replay = memory.store.approve_with_reviewer_grant(&grant, "mp-api-errors-01", "evidence supports the claim", 1_000).unwrap();
        assert_eq!(replay.id, decision.id);
        assert!(memory.store.memory_promotion("mp-api-errors-01").unwrap().is_none());
        assert!(memory.store.memory_record_by_key("api.error-envelope").unwrap().is_none());
        let (good, inv) = stored_promotion(&mut memory, &decision.id);
        let empty = memory.store.promote_with_reviewer_grant("mp-api-errors-01", &decision, &[], &[], 1_000, &grant).unwrap_err();
        assert!(matches!(empty, StoreError::Invalid(_)), "{empty:?}");
        let mut wrong_key = good.clone();
        wrong_key.record_key = "other.key".into();
        let wrong = memory.store.promote_with_reviewer_grant("mp-api-errors-01", &decision, &[wrong_key], &[inv.clone()], 1_000, &grant).unwrap_err();
        assert!(matches!(wrong, StoreError::Invalid(_)), "{wrong:?}");
        let mut wrong_body = good.clone();
        wrong_body.body_hash = ObjectId::from_hex("ab".repeat(32)).unwrap();
        let wrong = memory.store.promote_with_reviewer_grant("mp-api-errors-01", &decision, &[wrong_body], &[inv.clone()], 1_000, &grant).unwrap_err();
        assert!(matches!(wrong, StoreError::Invalid(_)), "{wrong:?}");
        assert!(memory.store.memory_promotion("mp-api-errors-01").unwrap().is_none());
        assert!(memory.store.memory_record_by_key("api.error-envelope").unwrap().is_none());
        assert!(memory.store.memory_record_by_key("other.key").unwrap().is_none());
        let first = memory.store.promote_with_reviewer_grant("mp-api-errors-01", &decision, &[good.clone()], &[inv.clone()], 1_000, &grant).unwrap();
        assert!(!first.reused);
        assert_eq!(first.change_ids, vec![format!("{}:1", good.id.as_str())]);
        let again = memory.store.promote_with_reviewer_grant("mp-api-errors-01", &decision, &[good.clone()], &[inv], 1_000, &grant).unwrap();
        assert!(again.reused);
        assert_eq!(again.sequence, first.sequence);
        let record = memory.store.memory_record_by_key("api.error-envelope").unwrap().unwrap();
        assert_eq!(record.id, good.id);
        assert_eq!(record.kind, MemoryKind::Observation);
        assert!(!record.is_hard);
        let stored = memory.store.memory_revision(record.id.as_str(), 1).unwrap().unwrap();
        assert_eq!(stored.body_hash.as_str(), body);
        assert_eq!(stored.body_hash, good.body_hash);
    }
}

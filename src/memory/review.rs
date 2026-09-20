//! Semantic review outside the promotion transaction; atomic head CAS on promote.
use super::{MemoryError, MemoryStore};
use crate::domain::*;
use sha2::{Digest, Sha256};

fn record_id_for(key: &str) -> Result<MemoryRecordId, MemoryError> {
    MemoryRecordId::new(key.replace('/', ".")).map_err(MemoryError::Invalid)
}

impl MemoryStore {
    pub fn review(&mut self, bytes: &[u8], now_unix_ms: i64) -> Result<ReviewDecision, MemoryError> {
        if bytes.len() > PROPOSAL_LIMIT { return Err(MemoryError::Invalid("review document exceeds 64 KiB".into())); }
        let doc: ReviewDocument = serde_json::from_slice(bytes).map_err(|_| MemoryError::Invalid("malformed review JSON (contents withheld)".into()))?;
        if doc.schema_version != 1 { return Err(MemoryError::Invalid("unsupported review schema_version".into())); }
        if !matches!(doc.decision.as_str(),"approve"|"reject"|"narrow") { return Err(MemoryError::Invalid("invalid review decision".into())); }
        if doc.reason.is_empty()||doc.reason.len()>4096||doc.reason.chars().any(char::is_control) {
            return Err(MemoryError::Invalid("invalid review reason".into()));
        }
        let (stored, payload) = self.store.memory_proposal_payload(&doc.proposal_id).map_err(MemoryError::from)?
            .ok_or_else(|| MemoryError::Invalid("proposal missing".into()))?;
        if stored.review_state != "validated" { return Err(MemoryError::Invalid("proposal is not validated".into())); }
        if stored.id != doc.proposal_id { return Err(MemoryError::Invalid("proposal identity mismatch".into())); }
        let proposal: ProposalDocument = serde_json::from_str(&payload).map_err(|_| MemoryError::Invalid("stored proposal is unreadable".into()))?;
        let snapshot = self.store.read_snapshot(None).map_err(MemoryError::from)?;
        let mut classes = Vec::new();
        let mut heads = serde_json::Map::new();
        for change in &proposal.changes {
            let current = self.store.memory_record_by_key(&change.record_key).map_err(MemoryError::from)?;
            let class = match &current {
                None => "disjoint",
                Some(rec) => {
                    let head = self.store.memory_head(rec.id.as_str()).map_err(MemoryError::from)?;
                    if let Some(head)=&head { heads.insert(rec.id.as_str().into(), serde_json::json!(head.revision)); }
                    let body = change.body_object.strip_prefix("sha256:").unwrap_or(&change.body_object);
                    let rev = head.as_ref().and_then(|h| self.store.memory_revision(rec.id.as_str(), h.revision).ok().flatten());
                    if rev.as_ref().is_some_and(|r| r.body_hash.as_str()==body) { "compatible" } else { "contradictory" }
                }
            };
            classes.push(serde_json::json!({"record_key":change.record_key,"class":class}));
        }
        let reviewed_heads = serde_json::json!({"event_head":snapshot.head,"records":heads}).to_string();
        let classification = serde_json::to_string(&classes).map_err(|e| MemoryError::Invalid(e.to_string()))?;
        let id = format!("rev-{:x}", Sha256::digest(format!("{}:{}:{}:{}", stored.id, stored.payload_digest, doc.decision, now_unix_ms).as_bytes()));
        let row = ReviewDecision {
            id: id.clone(), proposal_id: stored.id, payload_digest: stored.payload_digest,
            decision: doc.decision, classification, reviewed_heads, reason: doc.reason, created_unix_ms: now_unix_ms,
        };
        self.store.insert_review_decision(&row).map_err(MemoryError::from)?;
        Ok(row)
    }

    pub fn promote(&mut self, proposal_id: &str, decision_id: &str, now_unix_ms: i64) -> Result<PromotionReceipt, MemoryError> {
        if let Some(existing)=self.store.memory_promotion(proposal_id).map_err(MemoryError::from)? {
            if existing.decision_id==decision_id { return Ok(existing); }
            return Err(MemoryError::Invalid("proposal already promoted under a different decision".into()));
        }
        let decision = self.store.review_decision(decision_id).map_err(MemoryError::from)?
            .ok_or_else(|| MemoryError::Invalid("review decision missing".into()))?;
        if decision.proposal_id != proposal_id { return Err(MemoryError::Invalid("decision does not belong to proposal".into())); }
        if decision.decision != "approve" { return Err(MemoryError::Invalid("promotion requires an approve decision".into())); }
        let (stored, payload) = self.store.memory_proposal_payload(proposal_id).map_err(MemoryError::from)?
            .ok_or_else(|| MemoryError::Invalid("proposal missing".into()))?;
        if stored.payload_digest != decision.payload_digest { return Err(MemoryError::Invalid("review does not cover current proposal digest".into())); }
        if stored.review_state != "validated" { return Err(MemoryError::Invalid("proposal is not validated".into())); }
        let proposal: ProposalDocument = serde_json::from_str(&payload).map_err(|_| MemoryError::Invalid("stored proposal is unreadable".into()))?;
        let _snapshot = self.store.read_snapshot(None).map_err(MemoryError::from)?;
        let reviewed: serde_json::Value = serde_json::from_str(&decision.reviewed_heads).unwrap_or(serde_json::json!({}));
        let mut revisions = Vec::new();
        let mut invalidations = Vec::new();
        let prov = self.ingest_object(serde_json::json!({"proposal":proposal_id,"decision":decision_id}).to_string().as_bytes())?;
        for change in &proposal.changes {
            let body = ObjectId::parse(&change.body_object).map_err(MemoryError::Invalid)?;
            if !self.store.object_available(body.as_str()).map_err(MemoryError::from)? {
                return Err(MemoryError::EvidenceUnavailable { object: body });
            }
            let existing = self.store.memory_record_by_key(&change.record_key).map_err(MemoryError::from)?;
            if let Some(rec)=&existing {
                let head = self.store.memory_head(rec.id.as_str()).map_err(MemoryError::from)?
                    .ok_or_else(|| MemoryError::Invalid("promotion head missing".into()))?;
                let reviewed_rev = reviewed.get("records").and_then(|m| m.get(rec.id.as_str())).and_then(|v| v.as_u64());
                if reviewed_rev != Some(head.revision) { return Err(MemoryError::RevisionConflict { current: vec![(rec.id.clone(), head.revision)] }); }
                if let Some(expected)=&change.expected {
                    if expected.revision != head.revision || expected.record_id != rec.id.as_str() {
                        return Err(MemoryError::RevisionConflict { current: vec![(rec.id.clone(), head.revision)] });
                    }
                }
                let current = self.store.memory_revision(rec.id.as_str(), head.revision).map_err(MemoryError::from)?;
                if current.as_ref().is_some_and(|r| r.body_hash.as_str()==body.as_str()) { continue; }
            } else if change.expected.is_some() {
                return Err(MemoryError::Invalid("expected base record is missing".into()));
            }
            let id = match &existing {
                Some(rec)=>rec.id.clone(),
                None=>record_id_for(&change.record_key)?,
            };
            let kind = parse_kind(&change.kind).map_err(MemoryError::Invalid)?;
            let expected = existing.as_ref().and_then(|_| change.expected.as_ref().map(|e| e.revision));
            let mut dependencies = Vec::new();
            for dep in &change.based_on {
                dependencies.push((MemoryRecordId::new(dep.record_id.clone()).map_err(MemoryError::Invalid)?, dep.revision, "based_on".into()));
            }
            revisions.push(NewRevision {
                id, record_key: change.record_key.clone(), scope_id: "project".into(), kind,
                body_hash: body, provenance_hash: prov.clone(), applicability: change.scope.clone(),
                dependencies, expected, expiry_unix_ms: None, validity_state: "valid".into(), validity_reason: "promoted".into(),
            });
            let inv_id = format!("inv-{:x}", Sha256::digest(format!("{}:{}:{}", proposal_id, change.record_key, now_unix_ms).as_bytes()));
            invalidations.push((inv_id, stored.task_id.clone(), change.record_key.clone(), change.impact.clone()));
        }
        self.store.promote_reviewed_proposal(proposal_id, &decision, &revisions, &invalidations, now_unix_ms).map_err(MemoryError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SqliteStore;
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
        let doc = ProposalDocument {
            schema_version: 1, proposal_id: "mp-api-errors-01".into(),
            producer: ProposalProducer { task_id: "task-api".into(), attempt_id: "att-api-2".into() },
            input_snapshot_id: snap.id.as_str().into(), observed_revisions: vec![], repository: None,
            changes: vec![ProposalChange {
                record_key: "api.error-envelope".into(), expected: None, kind: "contract".into(),
                scope: Applicability { domains: vec!["api".into()], paths: vec!["src/api".into()] },
                claim: "Validation errors contain code, message, and request_id.".into(),
                body_object: format!("sha256:{}", body.as_str()), evidence: vec![], based_on: vec![],
                impact: "reconcile_before_completion".into(),
            }],
        };
        memory.propose(&serde_json::to_vec(&doc).unwrap(), 1_000).unwrap();
        (root, memory, body.as_str().to_string())
    }
    fn approve(proposal: &str) -> Vec<u8> {
        serde_json::to_vec(&ReviewDocument { schema_version: 1, proposal_id: proposal.into(), decision: "approve".into(), reason: "evidence supports the claim".into() }).unwrap()
    }
    #[test]
    fn competing_promotions_conflict_and_stale_review_must_be_repeated() {
        let (_root, mut memory, body) = setup();
        let decision = memory.review(&approve("mp-api-errors-01"), 1_000).unwrap();
        assert!(decision.classification.contains("disjoint"));
        let first = memory.promote("mp-api-errors-01", &decision.id, 1_000).unwrap();
        assert!(!first.reused);
        assert!(!first.change_ids.is_empty());
        let replay = memory.promote("mp-api-errors-01", &decision.id, 1_000).unwrap();
        assert!(replay.reused);
        let rec = memory.store.memory_record_by_key("api.error-envelope").unwrap().unwrap();
        assert_eq!(rec.kind, MemoryKind::Contract);
        assert!(!rec.is_hard);
        let snap = memory.store.memory_proposal("mp-api-errors-01").unwrap().unwrap().snapshot_id.unwrap();
        let rec = memory.store.memory_record_by_key("api.error-envelope").unwrap().unwrap();
        let other = ProposalDocument {
            schema_version: 1, proposal_id: "mp-api-errors-02".into(),
            producer: ProposalProducer { task_id: "task-api".into(), attempt_id: "att-api-2".into() },
            input_snapshot_id: snap.clone(), observed_revisions: vec![], repository: None,
            changes: vec![ProposalChange {
                record_key: "api.error-envelope".into(),
                expected: Some(ObservedRevision { record_id: rec.id.as_str().into(), revision: 1 }),
                kind: "contract".into(),
                scope: Applicability { domains: vec!["api".into()], paths: vec!["src/api".into()] },
                claim: "other claim".into(), body_object: format!("sha256:{body}"), evidence: vec![], based_on: vec![],
                impact: "reconcile_before_completion".into(),
            }],
        };
        memory.propose(&serde_json::to_vec(&other).unwrap(), 2_000).unwrap();
        let d2 = memory.review(&approve("mp-api-errors-02"), 2_000).unwrap();
        let newer = NewRevision {
            id: rec.id.clone(), record_key: "api.error-envelope".into(), scope_id: "project".into(),
            kind: MemoryKind::Contract, body_hash: ObjectId::from_hex(body.clone()).unwrap(), provenance_hash: ObjectId::from_hex(body.clone()).unwrap(),
            applicability: Applicability { domains: vec!["api".into()], paths: vec!["src/api".into()] },
            dependencies: vec![], expected: Some(1), expiry_unix_ms: None, validity_state: String::new(), validity_reason: String::new(),
        };
        memory.insert_revision(&ControlContext { now_unix_ms: 2_000 }, newer).unwrap();
        assert!(matches!(memory.promote("mp-api-errors-02", &d2.id, 2_000).unwrap_err(), MemoryError::RevisionConflict { .. }));
        let head = memory.store.memory_head(rec.id.as_str()).unwrap().unwrap();
        assert_eq!(head.revision, 2);
    }
    #[test]
    fn aborted_promotion_leaves_no_partial_revision_or_intent() {
        let (_root, mut memory, _body) = setup();
        let decision = memory.review(&approve("mp-api-errors-01"), 1_000).unwrap();
        let db = _root.path().join("state.db");
        let raw = rusqlite::Connection::open(&db).unwrap();
        raw.execute_batch("CREATE TRIGGER fail_promo BEFORE INSERT ON events WHEN NEW.kind='memory.promoted' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
        drop(raw);
        assert!(memory.promote("mp-api-errors-01", &decision.id, 1_000).is_err());
        assert!(memory.store.memory_record_by_key("api.error-envelope").unwrap().is_none());
        assert!(memory.store.memory_promotion("mp-api-errors-01").unwrap().is_none());
        let raw = rusqlite::Connection::open(&db).unwrap();
        raw.execute_batch("DROP TRIGGER fail_promo;").unwrap();
        drop(raw);
        let ok = memory.promote("mp-api-errors-01", &decision.id, 1_000).unwrap();
        assert!(!ok.change_ids.is_empty());
    }
}

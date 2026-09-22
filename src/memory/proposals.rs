//! Accept and validate worker memory proposals. Does not promote or change heads.
use super::{MemoryError, MemoryStore};
use crate::domain::*;
use sha2::{Digest, Sha256};

const ALLOWED_KINDS: &[&str] = &["observation","assumption","contract","task_local"];
const ALLOWED_IMPACT: &[&str] = &["informational","reconcile_before_completion","stop_at_checkpoint"];

fn digest_bytes(bytes: &[u8]) -> String { format!("{:x}", Sha256::digest(bytes)) }

fn parse_object(value: &str) -> Result<ObjectId, MemoryError> {
    ObjectId::parse(value).map_err(MemoryError::Invalid)
}

fn validate_document(doc: &ProposalDocument) -> Result<(), MemoryError> {
    if doc.schema_version != 1 { return Err(MemoryError::Invalid("unsupported proposal schema_version".into())); }
    ProposalId::new(doc.proposal_id.clone()).map_err(MemoryError::Invalid)?;
    TaskId::new(doc.producer.task_id.clone()).map_err(|_| MemoryError::Invalid("invalid producer task_id".into()))?;
    AttemptId::new(doc.producer.attempt_id.clone()).map_err(|_| MemoryError::Invalid("invalid producer attempt_id".into()))?;
    SnapshotId::new(doc.input_snapshot_id.clone()).map_err(|_| MemoryError::Invalid("invalid input_snapshot_id".into()))?;
    if doc.changes.is_empty() || doc.changes.len() > 32 { return Err(MemoryError::Invalid("proposal must contain 1–32 changes".into())); }
    if doc.observed_revisions.len() > 256 { return Err(MemoryError::Invalid("observed_revisions exceed bounds".into())); }
    for change in &doc.changes {
        if change.record_key.is_empty() || change.record_key.len() > 512 || change.record_key.chars().any(char::is_control)
            || std::path::Path::new(&change.record_key).is_absolute()
            || change.record_key.split('/').any(|p| p.is_empty() || p=="." || p=="..") {
            return Err(MemoryError::Invalid("invalid proposal record_key".into()));
        }
        if !ALLOWED_KINDS.contains(&change.kind.as_str()) { return Err(MemoryError::Invalid("proposal kind is not allowed for workers".into())); }
        if !ALLOWED_IMPACT.contains(&change.impact.as_str()) { return Err(MemoryError::Invalid("invalid proposal impact".into())); }
        if change.claim.is_empty() || change.claim.len() > 8_192 || change.claim.chars().any(char::is_control) {
            return Err(MemoryError::Invalid("invalid proposal claim".into()));
        }
        change.scope.validate().map_err(MemoryError::Invalid)?;
        parse_object(&change.body_object)?;
        if change.evidence.len() > 32 || change.based_on.len() > 32 { return Err(MemoryError::Invalid("proposal evidence or based_on exceed bounds".into())); }
        for ev in &change.evidence {
            if let Some(object)=&ev.object { parse_object(object)?; }
        }
    }
    if let Some(repo)=&doc.repository {
        if repo.id.is_empty()||repo.id.len()>128||repo.commit.len()>128||repo.tree.len()>128 {
            return Err(MemoryError::Invalid("invalid proposal repository identity".into()));
        }
    }
    Ok(())
}

impl MemoryStore {
    pub fn propose(&mut self, bytes: &[u8], now_unix_ms: i64) -> Result<ProposalReceipt, MemoryError> {
        if bytes.len() > PROPOSAL_LIMIT { return Err(MemoryError::Invalid("proposal exceeds 64 KiB".into())); }
        let doc: ProposalDocument = serde_json::from_slice(bytes).map_err(|_| MemoryError::Invalid("malformed proposal JSON (contents withheld)".into()))?;
        validate_document(&doc)?;
        let digest = digest_bytes(bytes);
        let snapshot = self.store.read_snapshot(None).map_err(MemoryError::from)?;
        if snapshot.schema_version < 21 { return Err(MemoryError::UnsupportedSchema); }
        let (review_state, reason, heads) = match self.check_proposal(&doc, &snapshot) {
            Ok(heads) => (String::from("validated"), String::from("accepted"), heads),
            Err(MemoryError::Invalid(reason)) => (String::from("rejected"), reason, String::from("[]")),
            Err(MemoryError::AuthorityDenied) => (String::from("rejected"), String::from("permission elevation refused"), String::from("[]")),
            Err(MemoryError::EvidenceUnavailable { .. }) => (String::from("rejected"), String::from("missing evidence object"), String::from("[]")),
            Err(other) => return Err(other),
        };
        self.store.insert_proposal(
            &doc.proposal_id, &digest, &doc.producer.task_id, &doc.producer.attempt_id,
            Some(&doc.input_snapshot_id), &review_state, std::str::from_utf8(bytes).map_err(|e| MemoryError::Invalid(e.to_string()))?,
            &reason, &heads, now_unix_ms,
        ).map_err(MemoryError::from)
    }

    fn check_proposal(&mut self, doc: &ProposalDocument, snapshot: &crate::domain::Snapshot) -> Result<String, MemoryError> {
        let task = snapshot.tasks.iter().find(|t| t.id.as_str()==doc.producer.task_id)
            .ok_or_else(|| MemoryError::Invalid("producer task is missing".into()))?;
        let attempt = snapshot.attempts.iter().find(|a| a.id.as_str()==doc.producer.attempt_id)
            .ok_or_else(|| MemoryError::Invalid("producer attempt is missing".into()))?;
        if attempt.task != task.id { return Err(MemoryError::Invalid("producer attempt does not belong to task".into())); }
        let snap = self.store.read_memory_snapshot(&doc.input_snapshot_id).map_err(|_| MemoryError::Invalid("input snapshot is missing".into()))?;
        if snap.task_id != doc.producer.task_id { return Err(MemoryError::Invalid("input snapshot does not belong to producer task".into())); }
        if attempt.snapshot.as_deref()!=Some(doc.input_snapshot_id.as_str()) {return Err(MemoryError::Invalid("proposal snapshot was not consumed by this attempt".into()));}
        if doc.repository.is_some() {return Err(MemoryError::Invalid("repository claims require verified repository evidence; unsupported until revision-bound validation is available".into()));}
        for observed in doc.observed_revisions.iter().chain(doc.changes.iter().flat_map(|c|c.based_on.iter())) {
            if !snap.entries.iter().any(|e|e.record_id.as_str()==observed.record_id && e.revision==observed.revision) {
                return Err(MemoryError::Invalid("observed/dependency revision is absent from attempt snapshot".into()));
            }
        }
        for observed in &doc.observed_revisions {
            let rec = self.store.memory_record(&observed.record_id).map_err(MemoryError::from)?
                .ok_or_else(|| MemoryError::Invalid("observed revision record is missing".into()))?;
            let _ = rec;
            if self.store.memory_revision(&observed.record_id, observed.revision).map_err(MemoryError::from)?.is_none() {
                return Err(MemoryError::Invalid("observed revision is missing".into()));
            }
        }
        for change in &doc.changes {
            let body = parse_object(&change.body_object)?;
            if !self.store.object_available(body.as_str()).map_err(MemoryError::from)? {
                return Err(MemoryError::EvidenceUnavailable { object: body });
            }
            super::read_object(&self.objects, &body)?;
            for ev in &change.evidence {
                if ev.validation_id.is_some() {return Err(MemoryError::Invalid("typed validation evidence is not available; unverified validation IDs are refused".into()));}
                if let Some(object)=&ev.object {
                    let id = parse_object(object)?;
                    if !self.store.object_available(id.as_str()).map_err(MemoryError::from)? {
                        return Err(MemoryError::EvidenceUnavailable { object: id });
                    }
                    super::read_object(&self.objects, &id)?;
                }
            }
            for dep in &change.based_on {
                if self.store.memory_revision(&dep.record_id, dep.revision).map_err(MemoryError::from)?.is_none() {
                    return Err(MemoryError::Invalid("based_on revision is missing".into()));
                }
            }
            if let Some(expected)=&change.expected {
                let rec = self.store.memory_record_by_key(&change.record_key).map_err(MemoryError::from)?
                    .ok_or_else(|| MemoryError::Invalid("expected base record is missing".into()))?;
                if rec.id.as_str() != expected.record_id { return Err(MemoryError::Invalid("expected record_id does not match record_key".into())); }
                let head = self.store.memory_head(rec.id.as_str()).map_err(MemoryError::from)?
                    .ok_or_else(|| MemoryError::Invalid("expected base head is missing".into()))?;
                if head.revision != expected.revision || head.status != "active" {
                    return Err(MemoryError::Invalid("stale proposal base".into()));
                }
                if rec.is_hard && change.impact == "informational" { return Err(MemoryError::AuthorityDenied); }
                if rec.kind == MemoryKind::Constraint || rec.kind == MemoryKind::HardMemory { return Err(MemoryError::AuthorityDenied); }
            } else if self.store.memory_record_by_key(&change.record_key).map_err(MemoryError::from)?.is_some() {
                return Err(MemoryError::Invalid("existing record requires expected base".into()));
            }
        }
        let heads = serde_json::to_string(&serde_json::json!({"event_head":snapshot.head,"task":task.revision})).unwrap_or_else(|_| "{}".into());
        Ok(heads)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SqliteStore;
    fn fixture() -> (tempfile::TempDir, MemoryStore, ObjectId, String) {
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
        let state=memory.store.read_snapshot(None).unwrap();
        let mut attempt=state.attempts[0].clone();attempt.revision+=1;attempt.snapshot=Some(snap.id.as_str().into());
        memory.store.commit(Commit{expected_head:state.head,mutations:vec![Mutation::Attempt{expected:Some(1),next:attempt}]}).unwrap();
        (root, memory, body, snap.id.as_str().to_string())
    }
    fn doc(snap: &str, body: &str, attempt: &str, expected: Option<ObservedRevision>) -> ProposalDocument {
        ProposalDocument {
            schema_version: 1, proposal_id: "mp-api-errors-01".into(),
            producer: ProposalProducer { task_id: "task-api".into(), attempt_id: attempt.into() },
            input_snapshot_id: snap.into(), observed_revisions: vec![], repository: None,
            changes: vec![ProposalChange {
                record_key: "api.error-envelope".into(), expected, kind: "contract".into(),
                scope: Applicability { domains: vec!["api".into()], paths: vec!["src/api".into()] },
                claim: "Validation errors contain code, message, and request_id.".into(),
                body_object: format!("sha256:{body}"), evidence: vec![], based_on: vec![],
                impact: "reconcile_before_completion".into(),
            }],
        }
    }
    #[test]
    fn duplicate_proposal_reuses_result_and_changed_bytes_conflict() {
        let (_root, mut memory, body, snap) = fixture();
        let first = serde_json::to_vec(&doc(&snap, body.as_str(), "att-api-2", None)).unwrap();
        let a = memory.propose(&first, 1_000).unwrap();
        let b = memory.propose(&first, 1_000).unwrap();
        assert_eq!(a.proposal_id, b.proposal_id);
        assert_eq!(a.payload_digest, b.payload_digest);
        assert!(b.reused);
        assert_eq!(a.validation, "accepted");
        let mut other = doc(&snap, body.as_str(), "att-api-2", None);
        other.changes[0].claim = "different".into();
        let err = memory.propose(&serde_json::to_vec(&other).unwrap(), 1_000).unwrap_err();
        assert!(matches!(err, MemoryError::IdempotencyKeyConflict));
    }
    #[test]
    fn spoofed_attempt_missing_evidence_stale_base_and_elevation_are_rejected() {
        let (_root, mut memory, body, snap) = fixture();
        let mut spoof_doc = doc(&snap, body.as_str(), "att-missing", None);
        spoof_doc.proposal_id = "mp-spoof".into();
        let rejected = memory.propose(&serde_json::to_vec(&spoof_doc).unwrap(), 1_000).unwrap();
        assert_eq!(rejected.validation, "rejected");
        assert!(rejected.reason.contains("attempt"));
        let mut missing = doc(&snap, &"ab".repeat(32), "att-api-2", None);
        missing.proposal_id = "mp-missing".into();
        let rejected = memory.propose(&serde_json::to_vec(&missing).unwrap(), 1_000).unwrap();
        assert_eq!(rejected.validation, "rejected");
        assert!(rejected.reason.contains("evidence") || rejected.reason.contains("missing"));
        let rec = NewRevision {
            id: MemoryRecordId::new("mem-errors").unwrap(), record_key: "api.error-envelope".into(), scope_id: "project".into(),
            kind: MemoryKind::Contract, body_hash: body.clone(), provenance_hash: body.clone(),
            applicability: Applicability { domains: vec!["api".into()], paths: vec!["src/api".into()] },
            dependencies: vec![], expected: None, expiry_unix_ms: None, validity_state: String::new(), validity_reason: String::new(),
        };
        memory.insert_revision(&ControlContext { now_unix_ms: 1_000 }, rec).unwrap();
        let mut stale = doc(&snap, body.as_str(), "att-api-2", Some(ObservedRevision { record_id: "mem-errors".into(), revision: 99 }));
        stale.proposal_id = "mp-stale".into();
        let rejected = memory.propose(&serde_json::to_vec(&stale).unwrap(), 1_000).unwrap();
        assert_eq!(rejected.validation, "rejected");
        assert!(rejected.reason.contains("stale"));
        let head = memory.store.read_snapshot(None).unwrap().head;
        memory.store.apply_memory_op(MemoryPolicyOp::HardRule, "api.error-envelope", head).unwrap();
        let mut elev = doc(&snap, body.as_str(), "att-api-2", Some(ObservedRevision { record_id: "mem-errors".into(), revision: 1 }));
        elev.proposal_id = "mp-elev".into();
        elev.changes[0].impact = "informational".into();
        let rejected = memory.propose(&serde_json::to_vec(&elev).unwrap(), 1_000).unwrap();
        assert_eq!(rejected.validation, "rejected");
        assert!(rejected.reason.contains("elevation") || rejected.reason.contains("permission"));
        let mut hard = doc(&snap, body.as_str(), "att-api-2", None);
        hard.proposal_id = "mp-hard".into();
        hard.changes[0].kind = "constraint".into();
        hard.changes[0].record_key = "api.new-constraint".into();
        assert!(memory.propose(&serde_json::to_vec(&hard).unwrap(), 1_000).is_err());
    }
    #[test]
    fn oversized_and_malformed_proposals_are_rejected() {
        let (_root, mut memory, _body, _snap) = fixture();
        assert!(memory.propose(&vec![b'x'; PROPOSAL_LIMIT + 1], 1_000).is_err());
        assert!(memory.propose(br#"{"schema_version":1}"#, 1_000).is_err());
        let hostile = br#"{"schema_version":1,"proposal_id":"mp-x","producer":{"task_id":"task-api","attempt_id":"att-api-2"},"input_snapshot_id":"snap-x","changes":[{"record_key":"k","kind":"observation","scope":{"domains":[],"paths":[]},"claim":"c","body_object":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","evidence":[],"based_on":[],"impact":"informational","is_hard":true}]}"#;
        assert!(memory.propose(hostile, 1_000).is_err());
    }
}

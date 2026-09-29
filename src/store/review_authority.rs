//! Delegated code-review authority (docs/telemetry/contracts-review.md §10,
//! factory plan F2.5). A `code_review` grant is an owner-signed policy of its
//! own class (`code_review_authority.v1`): one reviewer principal and its
//! public key, the project, repositories, task contract revisions and review
//! kinds it covers, permitted actions, a decision limit, a validity interval
//! and the effects it never grants. The reviewer decides one review
//! completion at a time with a request it signs itself
//! (`review_acceptance.v1`); an owner-signed `code_review_revocation.v1`
//! stops later decisions. Signatures are verified by `crate::authority`
//! against the pinned owner policy before any value here is constructed; the
//! store keeps the exact signed bytes and re-derives every field from them.
//! Nothing here launches, reserves, triages or changes a requirement.
use super::*;
use serde::{Deserialize, Serialize};

pub const GRANT_SCHEMA: &str = "code_review_authority.v1";
pub const REVOCATION_SCHEMA: &str = "code_review_revocation.v1";
pub const ACCEPTANCE_SCHEMA: &str = "review_acceptance.v1";
/// Recorded on every acceptance decision made under a grant.
pub const AUTHORITY: &str = "delegated_code_review.v1";
pub const SCOPE: &str = "code_review";
/// The only action a grant may permit (triage stays the owner's, §5).
pub const ACCEPT_ACTION: &str = "accept_review_completion";
/// Effects a grant never confers; the owner signs the full list.
pub const PROHIBITED_EFFECTS: [&str; 5] = ["alter_requirements", "approve_author_attempt", "approve_own_work", "child_delegation", "increase_permissions"];
pub const REVOCATION_REASONS: [&str; 4] = ["compromised", "issued_in_error", "reviewer_retired", "scope_changed"];
pub const REJECTION_REASONS: [&str; 4] = ["evidence_missing", "insufficient_coverage", "protocol_violation", "wrong_scope"];
const MAX_VALIDITY_MS: i64 = 366 * 86_400_000;
const MAX_BYTES: usize = 65_536;

fn invalid(message: impl Into<String>) -> StoreError { StoreError::Invalid(message.into()) }
fn digest(bytes: &[u8]) -> String { format!("sha256:{:x}", Sha256::digest(bytes)) }
fn hex64(value: &str) -> bool { value.len() == 64 && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) }
fn sha_ref(value: &str) -> bool { value.strip_prefix("sha256:").is_some_and(hex64) }
fn token(value: &str) -> bool { !value.is_empty() && value.len() <= 64 && value.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b)) }
fn absolute(value: &str) -> bool { value.starts_with('/') && value.len() <= 4096 && !value.chars().any(char::is_control) }
fn sorted_unique<T: Ord>(values: &[T]) -> bool { values.windows(2).all(|w| w[0] < w[1]) }
fn ssh_ed25519_key(value: &str) -> bool {
    let mut parts = value.split(' ');
    parts.next() == Some("ssh-ed25519")
        && parts.next().is_some_and(|body| (32..=256).contains(&body.len()) && body.bytes().all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b)))
        && parts.next().is_none()
}
fn reference_ok(r: &VersionedReference) -> bool { !r.id.is_empty() && r.id.len() <= 512 && r.revision > 0 && r.revision <= i64::MAX as u64 && hex64(&r.digest) }

/// One task contract revision a grant covers.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskScope { pub task_id: String, pub contract_revision: i64 }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantDocument {
    schema: String,
    scope: String,
    issuer: String,
    subject: String,
    subject_public_key: String,
    subject_configurations: Vec<String>,
    project_store: String,
    repositories: Vec<String>,
    tasks: Vec<TaskScope>,
    kinds: Vec<String>,
    review_configurations: Vec<String>,
    actions: Vec<String>,
    max_decisions: u32,
    valid_from_unix_ms: i64,
    expires_unix_ms: i64,
    prohibited_effects: Vec<String>,
    authority: VersionedReference,
}

/// An owner-signed `code_review` grant. Constructed only from bytes whose
/// owner signature `crate::authority` has verified; JSON cannot build it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedReviewAuthority {
    pub(crate) raw: Vec<u8>,
    pub(crate) grant_id: String,
    pub(crate) subject: String,
    pub(crate) subject_public_key: String,
    pub(crate) subject_configurations: Vec<String>,
    pub(crate) project_store: String,
    pub(crate) repositories: Vec<String>,
    pub(crate) tasks: Vec<TaskScope>,
    pub(crate) kinds: Vec<String>,
    pub(crate) review_configurations: Vec<String>,
    pub(crate) actions: Vec<String>,
    pub(crate) max_decisions: u32,
    pub(crate) valid_from_unix_ms: i64,
    pub(crate) expires_unix_ms: i64,
    pub(crate) authority: VersionedReference,
}

impl PreparedReviewAuthority {
    /// Parse bytes whose owner signature was already verified. Checks only shape.
    pub(crate) fn parse_verified(raw: &[u8]) -> std::result::Result<Self, String> {
        if raw.len() > MAX_BYTES { return Err("review authority document exceeds 65536 bytes".into()); }
        let d: GrantDocument = serde_json::from_slice(raw).map_err(|_| "invalid review authority document".to_string())?;
        if d.schema != GRANT_SCHEMA || d.scope != SCOPE { return Err(format!("a review authority grant is schema {GRANT_SCHEMA}, scope {SCOPE}")); }
        if d.issuer != "owner" { return Err("a review authority grant is issued by the owner".into()); }
        if !d.subject.strip_prefix("reviewer:").is_some_and(token) {
            return Err("the grant subject is a reviewer principal `reviewer:<token>`, never a worker, import or operator".into());
        }
        if !ssh_ed25519_key(&d.subject_public_key) { return Err("invalid reviewer public key".into()); }
        if d.subject_configurations.len() > 16 || !d.subject_configurations.iter().all(|c| sha_ref(c)) || !sorted_unique(&d.subject_configurations) {
            return Err("subject configurations are at most 16 sorted, distinct configuration ids".into());
        }
        if d.review_configurations.len() > 16 || !d.review_configurations.iter().all(|c| sha_ref(c)) || !sorted_unique(&d.review_configurations) {
            return Err("review configurations are at most 16 sorted, distinct configuration ids".into());
        }
        if d.review_configurations.iter().any(|c| d.subject_configurations.contains(c)) {
            return Err("a reviewer cannot be granted reviews run by its own configuration".into());
        }
        if !absolute(&d.project_store) { return Err("project_store must be an absolute path".into()); }
        if d.repositories.is_empty() || d.repositories.len() > 32 || !d.repositories.iter().all(|r| absolute(r)) || !sorted_unique(&d.repositories) {
            return Err("repositories are 1 to 32 sorted, distinct absolute paths".into());
        }
        if d.tasks.is_empty() || d.tasks.len() > 128 || !sorted_unique(&d.tasks)
            || d.tasks.iter().any(|t| TaskId::new(&t.task_id).is_err() || t.contract_revision < 1) {
            return Err("tasks are 1 to 128 sorted, distinct task contract revisions".into());
        }
        if d.kinds.is_empty() || !sorted_unique(&d.kinds) || !d.kinds.iter().all(|k| super::review_capture::KINDS.contains(&k.as_str())) {
            return Err("kinds are sorted, distinct review kinds".into());
        }
        if d.actions != [ACCEPT_ACTION] {
            return Err(format!("a code_review grant permits only `{ACCEPT_ACTION}`: triage, requirement changes and permissions stay the owner's"));
        }
        if d.prohibited_effects != PROHIBITED_EFFECTS {
            return Err(format!("prohibited_effects must list exactly {}", PROHIBITED_EFFECTS.join(", ")));
        }
        if !(1..=1024).contains(&d.max_decisions) { return Err("max_decisions is 1 to 1024".into()); }
        if d.valid_from_unix_ms < 0 || d.expires_unix_ms <= d.valid_from_unix_ms || d.expires_unix_ms - d.valid_from_unix_ms > MAX_VALIDITY_MS {
            return Err("a grant is valid for a positive interval of at most 366 days".into());
        }
        if !reference_ok(&d.authority) { return Err("invalid authority policy reference".into()); }
        Ok(Self { grant_id: digest(raw), raw: raw.to_vec(), subject: d.subject, subject_public_key: d.subject_public_key,
            subject_configurations: d.subject_configurations, project_store: d.project_store, repositories: d.repositories, tasks: d.tasks,
            kinds: d.kinds, review_configurations: d.review_configurations, actions: d.actions, max_decisions: d.max_decisions,
            valid_from_unix_ms: d.valid_from_unix_ms, expires_unix_ms: d.expires_unix_ms, authority: d.authority })
    }
    pub fn grant_id(&self) -> &str { &self.grant_id }
    pub(crate) fn subject_public_key(&self) -> &str { &self.subject_public_key }
    pub(crate) fn authority(&self) -> &VersionedReference { &self.authority }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RevocationDocument { schema: String, grant_id: String, project_store: String, reason: String, authority: VersionedReference }

/// An owner-signed revocation of one grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedReviewRevocation {
    pub(crate) raw: Vec<u8>,
    pub(crate) digest: String,
    pub(crate) grant_id: String,
    pub(crate) project_store: String,
    pub(crate) reason: String,
    pub(crate) authority: VersionedReference,
}

impl PreparedReviewRevocation {
    pub(crate) fn parse_verified(raw: &[u8]) -> std::result::Result<Self, String> {
        if raw.len() > MAX_BYTES { return Err("review authority revocation exceeds 65536 bytes".into()); }
        let d: RevocationDocument = serde_json::from_slice(raw).map_err(|_| "invalid review authority revocation".to_string())?;
        if d.schema != REVOCATION_SCHEMA { return Err(format!("a revocation is schema {REVOCATION_SCHEMA}")); }
        if !sha_ref(&d.grant_id) || !absolute(&d.project_store) || !reference_ok(&d.authority) { return Err("invalid review authority revocation".into()); }
        if !REVOCATION_REASONS.contains(&d.reason.as_str()) { return Err(format!("revocation reason is one of {}", REVOCATION_REASONS.join(", "))); }
        Ok(Self { digest: digest(raw), raw: raw.to_vec(), grant_id: d.grant_id, project_store: d.project_store, reason: d.reason, authority: d.authority })
    }
    pub(crate) fn authority(&self) -> &VersionedReference { &self.authority }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AcceptanceDocument {
    schema: String,
    grant_id: String,
    subject: String,
    project_store: String,
    session_id: String,
    receipt_digest: String,
    decision: String,
    #[serde(default)]
    reason: Option<String>,
}

/// A reviewer's decision on one review completion, signed with the grant
/// subject's key. Parsing an unverified request only selects the grant whose
/// key must verify it; it confers nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedReviewAcceptance {
    pub(crate) raw: Vec<u8>,
    pub(crate) digest: String,
    pub(crate) grant_id: String,
    pub(crate) subject: String,
    pub(crate) project_store: String,
    pub(crate) session_id: String,
    pub(crate) receipt_digest: String,
    pub(crate) decision: String,
    pub(crate) reason: Option<String>,
}

impl PreparedReviewAcceptance {
    pub(crate) fn parse_unverified(raw: &[u8]) -> std::result::Result<Self, String> {
        if raw.len() > MAX_BYTES { return Err("review acceptance request exceeds 65536 bytes".into()); }
        let d: AcceptanceDocument = serde_json::from_slice(raw).map_err(|e| format!("invalid review acceptance request: {e}"))?;
        if d.schema != ACCEPTANCE_SCHEMA { return Err(format!("an acceptance request is schema {ACCEPTANCE_SCHEMA}")); }
        if !sha_ref(&d.grant_id) || !sha_ref(&d.session_id) || !sha_ref(&d.receipt_digest) || !absolute(&d.project_store)
            || !d.subject.strip_prefix("reviewer:").is_some_and(token) {
            return Err("invalid review acceptance request".into());
        }
        match (d.decision.as_str(), &d.reason) {
            ("accepted", None) => {}
            ("rejected", Some(r)) if REJECTION_REASONS.contains(&r.as_str()) => {}
            ("rejected", _) => return Err(format!("a rejection names one reason of {}", REJECTION_REASONS.join(", "))),
            ("accepted", Some(_)) => return Err("an acceptance has no reason".into()),
            _ => return Err("decision is `accepted` or `rejected`".into()),
        }
        Ok(Self { digest: digest(raw), raw: raw.to_vec(), grant_id: d.grant_id, subject: d.subject, project_store: d.project_store,
            session_id: d.session_id, receipt_digest: d.receipt_digest, decision: d.decision, reason: d.reason })
    }
    pub(crate) fn grant_id(&self) -> &str { &self.grant_id }
    pub(crate) fn subject(&self) -> &str { &self.subject }
    pub(crate) fn session_id(&self) -> &str { &self.session_id }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReviewAuthorityInstall { pub grant_id: String, pub subject: String, pub expires_unix_ms: i64, pub installed: bool }

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReviewAcceptance {
    pub session_id: String,
    pub decision: String,
    pub reason: Option<String>,
    pub authority_principal: String,
    pub grant_id: String,
    pub authority: String,
    pub receipt_digest: String,
    pub request_digest: String,
    pub decided_unix_ms: i64,
    /// True when this exact request was already recorded.
    pub replayed: bool,
}

/// Whether this store has the review authority tables (the reviewer-authority migration).
pub(crate) fn present(db: &Connection) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='review_authority_grants')", [], |r| r.get(0))?)
}

fn require(db: &Connection) -> Result<()> {
    check_schema(db)?;
    if !present(db)? { return Err(StoreError::UnsupportedSchema(db.query_row("PRAGMA user_version", [], |r| r.get(0))?)); }
    Ok(())
}

fn project_store(db: &Connection) -> Result<String> {
    let path = db.path().ok_or_else(|| invalid("review authority requires a file-backed store"))?;
    std::fs::canonicalize(path).map(|p| p.to_string_lossy().into_owned()).map_err(|_| invalid("store path unavailable"))
}

fn json_text<T: Serialize>(value: &T) -> Result<String> { serde_json::to_string(value).map_err(|e| invalid(e.to_string())) }

/// The stored grant, re-derived from its bytes: `(grant, owner signature)`.
fn load_grant(db: &Connection, grant_id: &str) -> Result<(PreparedReviewAuthority, Vec<u8>)> {
    let row: Option<(Vec<u8>, Vec<u8>)> = db.query_row("SELECT raw_bytes,signature FROM review_authority_grants WHERE grant_id=?1", [grant_id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
    let (raw, signature) = row.ok_or_else(|| invalid(format!("no review authority grant {grant_id}")))?;
    if digest(&raw) != grant_id { return Err(StoreError::Corrupt("review authority grant digest mismatch".into())); }
    let grant = PreparedReviewAuthority::parse_verified(&raw).map_err(|e| StoreError::Corrupt(format!("stored review authority grant: {e}")))?;
    Ok((grant, signature))
}

/// A session's reviewer attempt, configuration, same-attempt flag, completion
/// outcome, receipt and time, task, kind, contract revision, repository,
/// author attempt and opportunity.
type SessionScope = (String, Option<String>, bool, Option<String>, String, i64, String, String, i64, String, String, String);

impl SqliteStore {
    /// Install one verified grant. The same bytes replay; an expired grant or
    /// one for another project is refused. A grant never changes: a wider or
    /// longer one is a new document the owner signs.
    pub fn install_review_authority(&mut self, grant: &PreparedReviewAuthority, signature: &[u8], now: i64) -> Result<ReviewAuthorityInstall> {
        let reparsed = PreparedReviewAuthority::parse_verified(&grant.raw).map_err(invalid)?;
        if reparsed != *grant { return Err(invalid("changed review authority bytes")); }
        if signature.is_empty() || signature.len() > 8192 { return Err(invalid("invalid review authority signature")); }
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require(&tx)?;
        if grant.project_store != project_store(&tx)? { return Err(invalid("review authority grant belongs to another project")); }
        if now >= grant.expires_unix_ms { return Err(invalid("review authority grant is expired")); }
        let existing: Option<Vec<u8>> = tx.query_row("SELECT raw_bytes FROM review_authority_grants WHERE grant_id=?1", [&grant.grant_id], |r| r.get(0)).optional()?;
        let installed = existing.is_none();
        if installed {
            tx.execute("INSERT INTO review_authority_grants(grant_id,raw_bytes,signature,scope,issuer,subject,subject_public_key,project_store,actions,repositories,tasks,kinds,review_configurations,subject_configurations,max_decisions,valid_from_unix_ms,expires_unix_ms,authority_revision,authority_digest,installed_unix_ms)
                VALUES(?1,?2,?3,?4,'owner',?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)",
                params![grant.grant_id, grant.raw, signature, SCOPE, grant.subject, grant.subject_public_key, grant.project_store, json_text(&grant.actions)?,
                    json_text(&grant.repositories)?, json_text(&grant.tasks)?, json_text(&grant.kinds)?, json_text(&grant.review_configurations)?,
                    json_text(&grant.subject_configurations)?, grant.max_decisions, grant.valid_from_unix_ms, grant.expires_unix_ms,
                    integer(grant.authority.revision)?, grant.authority.digest, now])?;
            tx.commit()?;
        }
        Ok(ReviewAuthorityInstall { grant_id: grant.grant_id.clone(), subject: grant.subject.clone(), expires_unix_ms: grant.expires_unix_ms, installed })
    }

    /// The stored grant and its owner signature, for verification before use.
    pub fn review_authority_grant(&self, grant_id: &str) -> Result<(PreparedReviewAuthority, Vec<u8>)> {
        require(&self.connection)?;
        load_grant(&self.connection, grant_id)
    }

    /// Record an owner-signed revocation. Later decisions under the grant are
    /// refused; earlier decisions stay. The same revocation replays.
    pub fn revoke_review_authority(&mut self, revocation: &PreparedReviewRevocation, signature: &[u8], now: i64) -> Result<serde_json::Value> {
        let reparsed = PreparedReviewRevocation::parse_verified(&revocation.raw).map_err(invalid)?;
        if reparsed != *revocation { return Err(invalid("changed revocation bytes")); }
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require(&tx)?;
        if revocation.project_store != project_store(&tx)? { return Err(invalid("review authority revocation belongs to another project")); }
        load_grant(&tx, &revocation.grant_id)?;
        let existing: Option<(String, i64)> = tx.query_row("SELECT revocation_digest,revoked_unix_ms FROM review_authority_revocations WHERE grant_id=?1", [&revocation.grant_id],
            |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let (revoked, replayed) = match existing {
            Some((stored, _)) if stored != revocation.digest => return Err(invalid(format!("review authority grant {} is already revoked", revocation.grant_id))),
            Some((_, at)) => (at, true),
            None => {
                tx.execute("INSERT INTO review_authority_revocations(grant_id,raw_bytes,signature,revocation_digest,reason,revoked_unix_ms) VALUES(?1,?2,?3,?4,?5,?6)",
                    params![revocation.grant_id, revocation.raw, signature, revocation.digest, revocation.reason, now])?;
                (now, false)
            }
        };
        tx.commit()?;
        Ok(serde_json::json!({"grant_id": revocation.grant_id, "reason": revocation.reason, "revoked_unix_ms": revoked, "replayed": replayed,
            "stops": "later_decisions", "undoes_earlier_decisions": false}))
    }

    /// Record the reviewer's decision on one completed review, under `grant`
    /// (owner signature verified) and `request` (verified with the grant
    /// subject's key). Refused unless the grant covers it now; the trigger
    /// repeats the rule on raw rows. The same request replays.
    pub fn accept_review(&mut self, grant: &PreparedReviewAuthority, request: &PreparedReviewAcceptance, request_signature: &[u8], now: i64) -> Result<ReviewAcceptance> {
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require(&tx)?;
        let (stored, _) = load_grant(&tx, &grant.grant_id)?;
        if stored != *grant { return Err(invalid("changed review authority bytes")); }
        let reparsed = PreparedReviewAcceptance::parse_unverified(&request.raw).map_err(invalid)?;
        if reparsed != *request { return Err(invalid("changed acceptance request bytes")); }
        if request.grant_id != grant.grant_id || request.subject != grant.subject { return Err(invalid("the acceptance request names another grant or principal")); }
        let store = project_store(&tx)?;
        if grant.project_store != store || request.project_store != store { return Err(invalid("review authority belongs to another project")); }
        let done: Option<(String, String, Option<String>, String, i64)> = tx.query_row("SELECT request_digest,decision,reason,receipt_digest,decided_unix_ms FROM review_acceptances WHERE session_id=?1",
            [&request.session_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).optional()?;
        let result = |decision: String, reason: Option<String>, receipt: String, at: i64, replayed: bool| ReviewAcceptance { session_id: request.session_id.clone(),
            decision, reason, authority_principal: grant.subject.clone(), grant_id: grant.grant_id.clone(), authority: AUTHORITY.into(),
            receipt_digest: receipt, request_digest: request.digest.clone(), decided_unix_ms: at, replayed };
        if let Some((digest, decision, reason, receipt, at)) = done {
            // A replay reads back; any other request is a second decision.
            if digest == request.digest { return Ok(result(decision, reason, receipt, at, true)); }
            return Err(invalid(format!("review session {} already has a decision", request.session_id)));
        }
        if tx.query_row("SELECT EXISTS(SELECT 1 FROM review_authority_revocations WHERE grant_id=?1)", [&grant.grant_id], |r| r.get::<_, bool>(0))? {
            return Err(invalid("review authority grant is revoked"));
        }
        if now < grant.valid_from_unix_ms { return Err(invalid("review authority grant is not valid yet")); }
        if now >= grant.expires_unix_ms { return Err(invalid("review authority grant is expired")); }
        let row: Option<SessionScope> = tx.query_row(
            "SELECT r.attempt_id,r.configuration_id,r.same_attempt_as_author,c.outcome,c.receipt_digest,c.completed_unix_ms,o.task_id,o.kind,o.contract_revision,s.repository,s.attempt_id,o.opportunity_id
             FROM review_sessions r LEFT JOIN review_completions c ON c.session_id=r.session_id JOIN review_opportunities o ON o.opportunity_id=r.opportunity_id
             JOIN result_submissions s ON s.submission_id=o.submission_id WHERE r.session_id=?1", [&request.session_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get::<_, Option<String>>(4)?.unwrap_or_default(), r.get::<_, Option<i64>>(5)?.unwrap_or(0),
                r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?, r.get(11)?))).optional()?;
        let Some((reviewer, configuration, same_attempt, outcome, receipt, completed, task, kind, revision, repository, author, _)) = row else {
            return Err(invalid(format!("no review session {}", request.session_id)));
        };
        match outcome.as_deref() {
            None => return Err(invalid(format!("review session {} has no completion to decide", request.session_id))),
            Some("completed") => {}
            Some(other) => return Err(invalid(format!("only a completed review is accepted or rejected; this session ended {other}"))),
        }
        if receipt != request.receipt_digest { return Err(invalid("the acceptance request names another receipt than the session's completion")); }
        if now < completed { return Err(invalid("a decision cannot precede the completion")); }
        // Independence: never the reviewer's own review, never the author attempt's work.
        let bare = grant.subject.trim_start_matches("reviewer:");
        let author_configuration: Option<String> = tx.query_row("SELECT chosen_configuration_id FROM dispatch_decisions WHERE attempt_id=?1", [&author], |r| r.get(0)).optional()?;
        let own = bare == reviewer || configuration.as_ref().is_some_and(|c| grant.subject_configurations.contains(c));
        let authors = same_attempt || bare == author || author_configuration.as_ref().is_some_and(|c| grant.subject_configurations.contains(c));
        if own || authors {
            return Err(invalid("a reviewer cannot accept its own review or a review of work by the author attempt or its own configuration"));
        }
        let scope = |ok: bool, what: &str| if ok { Ok(()) } else { Err(invalid(format!("the session is outside the grant's scope: {what}"))) };
        scope(grant.actions.iter().any(|a| a == ACCEPT_ACTION), "action")?;
        scope(grant.repositories.contains(&repository), "repository")?;
        scope(grant.tasks.contains(&TaskScope { task_id: task, contract_revision: revision }), "task contract revision")?;
        scope(grant.kinds.contains(&kind), "review kind")?;
        scope(grant.review_configurations.is_empty() || configuration.as_ref().is_some_and(|c| grant.review_configurations.contains(c)), "reviewer configuration")?;
        let used: i64 = tx.query_row("SELECT count(*) FROM review_acceptances WHERE authority_ref=?1", [&grant.grant_id], |r| r.get(0))?;
        if used >= i64::from(grant.max_decisions) { return Err(invalid(format!("the grant's decision limit ({}) is reached", grant.max_decisions))); }
        tx.execute("INSERT INTO review_acceptances(session_id,decision,reason,authority_principal,authority_ref,authority,receipt_digest,request_digest,request_bytes,request_signature,decided_unix_ms)
            VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![request.session_id, request.decision, request.reason, grant.subject, grant.grant_id, AUTHORITY, receipt, request.digest, request.raw, request_signature, now])?;
        tx.commit()?;
        Ok(result(request.decision.clone(), request.reason.clone(), receipt, now, false))
    }
}

/// Grants, revocations and decisions as JSON at `now` (`review authority show`). Read-only.
pub fn review_authority_state(db: &Connection, now: i64) -> Result<Option<serde_json::Value>> {
    if !present(db)? { return Ok(None); }
    let ids: Vec<String> = db.prepare("SELECT grant_id FROM review_authority_grants ORDER BY installed_unix_ms,grant_id")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    let mut grants = Vec::with_capacity(ids.len());
    for id in ids {
        let (g, _) = load_grant(db, &id)?;
        let installed: i64 = db.query_row("SELECT installed_unix_ms FROM review_authority_grants WHERE grant_id=?1", [&id], |r| r.get(0))?;
        let revocation: Option<(String, i64)> = db.query_row("SELECT reason,revoked_unix_ms FROM review_authority_revocations WHERE grant_id=?1", [&id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let used: i64 = db.query_row("SELECT count(*) FROM review_acceptances WHERE authority_ref=?1", [&id], |r| r.get(0))?;
        let status = if revocation.is_some() { "revoked" } else if now >= g.expires_unix_ms { "expired" } else if now < g.valid_from_unix_ms { "not_yet_valid" }
            else if used >= i64::from(g.max_decisions) { "exhausted" } else { "active" };
        grants.push(serde_json::json!({"grant_id": id, "scope": SCOPE, "subject": g.subject, "status": status, "subject_configurations": g.subject_configurations,
            "repositories": g.repositories, "tasks": g.tasks, "kinds": g.kinds, "review_configurations": g.review_configurations, "actions": g.actions,
            "prohibited_effects": PROHIBITED_EFFECTS, "max_decisions": g.max_decisions, "decisions": used, "valid_from_unix_ms": g.valid_from_unix_ms,
            "expires_unix_ms": g.expires_unix_ms, "authority": g.authority, "installed_unix_ms": installed,
            "revocation": revocation.map(|(reason, at)| serde_json::json!({"reason": reason, "revoked_unix_ms": at}))}));
    }
    let decisions: Vec<serde_json::Value> = db.prepare("SELECT session_id,decision,reason,authority_principal,authority_ref,authority,receipt_digest,request_digest,decided_unix_ms FROM review_acceptances ORDER BY decided_unix_ms,session_id")?
        .query_map([], |r| Ok(serde_json::json!({"session_id": r.get::<_, String>(0)?, "decision": r.get::<_, String>(1)?, "reason": r.get::<_, Option<String>>(2)?,
            "authority_principal": r.get::<_, String>(3)?, "grant_id": r.get::<_, String>(4)?, "authority": r.get::<_, String>(5)?,
            "receipt_digest": r.get::<_, String>(6)?, "request_digest": r.get::<_, String>(7)?, "decided_unix_ms": r.get::<_, i64>(8)?})))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(Some(serde_json::json!({"grants": grants, "decisions": decisions, "authority": AUTHORITY})))
}

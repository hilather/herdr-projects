//! Finding triage and duplicate history (migration 0055,
//! docs/telemetry/contracts-review.md §5, plan TM3.2). A review completion's
//! finding references become submissions (proposals); a submission holds a
//! revisioned claim set (split, restore); the triage authority decides each
//! claim (validated, rejected, duplicate, pending), mints canonical findings
//! and merges or unmerges them. Every change is one append-only `finding_log`
//! row, and [`finding_state`] replays the log to any sequence. A worker or an
//! import can only propose: only the project owner at the CLI
//! ([`TRIAGE_PRINCIPAL`]) triages until a scoped reviewer-authority producer
//! exists. Titles are contracts §7 excerpts; no finding text is stored.
use super::*;
use rusqlite::OptionalExtension;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// The only principal that may triage: the project owner at the CLI.
pub const TRIAGE_PRINCIPAL: &str = "operator:cli";
/// Authority recorded on the owner's triage rows.
pub const TRIAGE_AUTHORITY: &str = "operator_owner.v1";
pub const SEVERITY_POLICY: &str = "finding_severity.v1";
pub const SEVERITIES: [&str; 5] = ["critical", "high", "medium", "low", "informational"];
pub const REJECT_REASONS: [&str; 3] = ["insufficient_evidence", "intended_behavior", "out_of_scope"];
pub const PENDING_REASONS: [&str; 2] = ["reopened", "decided_in_error"];
const MAX_CLAIMS: usize = 32;
const MAX_EVIDENCE: usize = 64;

/// Where a validated claim points: a newly minted canonical finding, or an existing one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindingTarget { New { title: Option<String> }, Existing(String) }

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriageOutcome {
    Validated { target: FindingTarget, severity: String },
    Rejected { reason: String },
    Duplicate { of: String },
    /// Back to pending (a correction): `reopened` or `decided_in_error`.
    Pending { reason: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriageRequest {
    pub outcome: TriageOutcome,
    /// `sha256:<hex64>` or `verification_run:<hex64>`; a validation needs one.
    pub evidence: Vec<String>,
    /// Refuse unless the history head is still this sequence.
    pub expected_seq: Option<i64>,
}

/// One appended history row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FindingEvent {
    pub seq: i64,
    pub kind: String,
    pub principal: String,
    pub authority: String,
    pub expected_seq: Option<i64>,
    pub recorded_unix_ms: i64,
    /// What the row changed, e.g. `{"claim_id": 3, "outcome": "validated", ...}`.
    pub subject: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClaimState {
    pub claim_id: i64,
    pub ordinal: i64,
    pub title: Option<String>,
    /// Latest decision at the watermark, or `None` when never decided.
    pub decision_seq: Option<i64>,
    /// The decision as recorded (`pending` when none).
    pub decided: String,
    /// Derived: `validated` only for the discovery claim of its canonical group.
    pub outcome: String,
    pub finding_id: Option<String>,
    /// The finding's group root at the watermark.
    pub canonical_finding: Option<String>,
    pub severity: Option<String>,
    pub reason: Option<String>,
    pub evidence_refs: Vec<String>,
    /// Linked to a seed (§8) at the watermark: an evaluation artefact, outside
    /// discovery and validation credit (contracts-review.md §9).
    pub seed_linked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SubmissionState {
    pub submission_id: i64,
    pub seq: i64,
    pub session_id: String,
    pub finding_ref: String,
    pub opportunity_id: String,
    pub reporter_attempt_id: String,
    pub title: Option<String>,
    pub recorded_unix_ms: i64,
    pub trust: String,
    pub claim_revision: i64,
    pub claims: Vec<ClaimState>,
    /// `pending`, `validated_only`, `rejected_only`, `duplicate_only` or `mixed`.
    pub outcome: String,
    pub has_validated_claim: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FindingGroupState {
    pub finding_id: String,
    pub title: Option<String>,
    pub minted_seq: i64,
    /// Active merge target at the watermark.
    pub merged_into: Option<String>,
    pub root: String,
    /// `validated` (a root with a discovery claim), `merged` or `unvalidated`.
    pub status: String,
    pub discovery_claim: Option<i64>,
    pub validated_claims: Vec<i64>,
    pub duplicate_claims: Vec<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct FindingSummary {
    pub submissions: usize,
    pub pending: usize,
    pub adjudicated: usize,
    pub validated_only: usize,
    pub rejected_only: usize,
    pub duplicate_only: usize,
    pub mixed: usize,
    pub has_validated_claim: usize,
    /// Claim-level drill-down; never a substitute for submission counts.
    pub claims: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FindingState {
    pub head_seq: i64,
    pub as_of_seq: i64,
    pub unique_findings: usize,
    pub summary: FindingSummary,
    pub submissions: Vec<SubmissionState>,
    pub findings: Vec<FindingGroupState>,
    pub history: Vec<FindingEvent>,
}

impl FindingSummary {
    /// Exclusive submission buckets of `submissions`.
    pub fn of<'a>(submissions: impl IntoIterator<Item = &'a SubmissionState>) -> Self {
        let mut s = FindingSummary { claims: ["pending", "validated", "rejected", "duplicate"].into_iter().map(|k| (k.to_owned(), 0)).collect(), ..Default::default() };
        for sub in submissions {
            s.submissions += 1;
            for claim in &sub.claims { *s.claims.entry(claim.outcome.clone()).or_default() += 1; }
            match sub.outcome.as_str() {
                "pending" => { s.pending += 1; continue; }
                "validated_only" => s.validated_only += 1,
                "rejected_only" => s.rejected_only += 1,
                "duplicate_only" => s.duplicate_only += 1,
                _ => s.mixed += 1,
            }
            s.adjudicated += 1;
            if sub.has_validated_claim { s.has_validated_claim += 1; }
        }
        s
    }
}

impl FindingSummary {
    /// Buckets of `submissions` without their seed-linked claims (evaluation
    /// artefacts, contracts-review.md §9): each submission's outcome is derived
    /// again from its other claims; a submission with no other claim leaves the
    /// buckets. Also returns `(submissions left out, claims left out)`.
    pub fn without_seed_links<'a>(submissions: impl IntoIterator<Item = &'a SubmissionState>) -> (Self, usize, usize) {
        let (mut kept, mut left_out, mut claims) = (Vec::new(), 0, 0);
        for sub in submissions {
            let before = sub.claims.len();
            let mut rest = sub.clone();
            rest.claims.retain(|c| !c.seed_linked);
            claims += before - rest.claims.len();
            if rest.claims.is_empty() { left_out += 1; continue; }
            derive_outcome(&mut rest);
            kept.push(rest);
        }
        (Self::of(&kept), left_out, claims)
    }
}

/// A submission's exclusive outcome from its claims' derived outcomes.
fn derive_outcome(sub: &mut SubmissionState) {
    let has = |o: &str| sub.claims.iter().any(|c| c.outcome == o);
    let all = |o: &str| sub.claims.iter().all(|c| c.outcome == o);
    sub.has_validated_claim = has("validated");
    sub.outcome = if has("pending") { "pending" } else if all("validated") { "validated_only" } else if all("rejected") { "rejected_only" }
        else if all("duplicate") { "duplicate_only" } else { "mixed" }.to_owned();
}

fn invalid(message: String) -> StoreError { StoreError::Invalid(message) }

fn schema_55(tx: &Connection) -> Result<()> {
    check_schema(tx)?;
    let version: u32 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 55 { return Err(StoreError::UnsupportedSchema(version)); }
    Ok(())
}

pub(super) fn hex64(value: &str) -> bool { value.len() == 64 && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) }
fn evidence_ref(value: &str) -> bool { value.strip_prefix("sha256:").or_else(|| value.strip_prefix("verification_run:")).is_some_and(hex64) }

pub(super) fn evidence(values: &[String]) -> Result<Vec<String>> {
    if values.len() > MAX_EVIDENCE { return Err(invalid(format!("at most {MAX_EVIDENCE} evidence references"))); }
    let mut sorted = values.to_vec();
    sorted.sort();
    sorted.dedup();
    if sorted.len() != values.len() { return Err(invalid("duplicate evidence reference".into())); }
    if let Some(bad) = sorted.iter().position(|v| !evidence_ref(v)) { return Err(invalid(format!("evidence reference {} is not an allowed reference", bad + 1))); }
    Ok(sorted)
}

/// A contracts §7 excerpt of a title (rules 1–5, applied before any write).
pub(super) fn title_excerpt(text: &str) -> Option<String> {
    let home = std::env::var("HOME").ok();
    crate::domain::excerpt(text, home.as_deref())
}

/// Refuse every principal but the triage authority. A worker (`worker:*` or
/// any attempt's identity) and an import (`import:*`) can only propose.
pub(super) fn triage_authority(tx: &Connection, principal: &str) -> Result<()> {
    if principal.is_empty() || principal.len() > 128 { return Err(invalid("invalid principal".into())); }
    let bare = principal.strip_prefix("worker:").unwrap_or(principal);
    if principal.starts_with("worker:") || tx.query_row("SELECT EXISTS(SELECT 1 FROM attempts WHERE id=?1)", [bare], |r| r.get::<_, bool>(0))? {
        return Err(invalid("a worker cannot triage findings: a worker's report is a proposal".into()));
    }
    if principal.starts_with("import:") { return Err(invalid("an untrusted import cannot triage findings: imported reports stay proposals".into())); }
    if principal != TRIAGE_PRINCIPAL {
        return Err(invalid(format!("no finding triage authority for {principal}: only {TRIAGE_PRINCIPAL} (the project owner) triages until a reviewer-authority producer exists")));
    }
    Ok(())
}

/// Ledgers sharing the one ordering, each once its migration has run:
/// `finding_log`, `fix_log` (contracts-review.md §6), `protocol_log` (0057), `seed_log`
/// (seeded defects, 0058) and `review_log` (review lifecycle, 0059).
const LEDGERS: [&str; 5] = ["finding_log", "fix_log", "protocol_log", "seed_log", "review_log"];

/// Head of the one ordering of the ledgers present: the replay watermark of all.
pub(super) fn head(tx: &Connection) -> Result<i64> {
    let mut head = 0;
    for ledger in LEDGERS {
        let present: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)", [ledger], |r| r.get(0))?;
        if present { head = head.max(tx.query_row(&format!("SELECT coalesce(max(seq),0) FROM {ledger}"), [], |r| r.get::<_, i64>(0))?); }
    }
    Ok(head)
}

/// Append the owner's history row after the authority and expected-head checks.
fn log(tx: &Connection, kind: &str, principal: &str, expected: Option<i64>, now: i64) -> Result<i64> {
    triage_authority(tx, principal)?;
    if let Some(expected) = expected {
        let head = head(tx)?;
        if head != expected { return Err(invalid(format!("finding history moved: head is {head}, expected {expected}"))); }
    }
    let seq = head(tx)? + 1;
    tx.execute("INSERT INTO finding_log(seq,kind,principal,authority,expected_seq,recorded_unix_ms) VALUES(?1,?2,?3,?4,?5,?6)", params![seq, kind, principal, TRIAGE_AUTHORITY, expected, now])?;
    Ok(seq)
}

fn event(tx: &Connection, seq: i64, subject: serde_json::Value) -> Result<FindingEvent> {
    Ok(tx.query_row("SELECT kind,principal,authority,expected_seq,recorded_unix_ms FROM finding_log WHERE seq=?1", [seq],
        |r| Ok(FindingEvent { seq, kind: r.get(0)?, principal: r.get(1)?, authority: r.get(2)?, expected_seq: r.get(3)?, recorded_unix_ms: r.get(4)?, subject }))?)
}

/// Record one submission (initial claim set of one claim) per finding
/// reference of a just-recorded completion, as proposals. Returns their ids.
pub(super) fn record_submissions(tx: &Connection, session: &str, refs: &[String], titles: &BTreeMap<String, String>, principal: &str, now: i64) -> Result<Vec<i64>> {
    let mut ids = Vec::with_capacity(refs.len());
    for finding in refs {
        let seq = head(tx)? + 1;
        tx.execute("INSERT INTO finding_log(seq,kind,principal,authority,expected_seq,recorded_unix_ms) VALUES(?1,'submitted',?2,'proposal',NULL,?3)", params![seq, principal, now])?;
        tx.execute("INSERT INTO finding_submissions(seq,session_id,finding_ref,title,source,trust) VALUES(?1,?2,?3,?4,'review_receipt','proposal')",
            params![seq, session, finding, titles.get(finding)])?;
        let submission = tx.last_insert_rowid();
        tx.execute("INSERT INTO finding_claim_sets(seq,submission_id,revision,kind,restores) VALUES(?1,?2,1,'initial',NULL)", params![seq, submission])?;
        tx.execute("INSERT INTO finding_claims(submission_id,revision,ordinal,title) VALUES(?1,1,1,NULL)", [submission])?;
        ids.push(submission);
    }
    Ok(ids)
}

/// Submission ids of a session's completion (empty before migration 0055).
pub(super) fn session_submissions(tx: &Connection, session: &str) -> Result<Vec<i64>> {
    let present: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='finding_submissions')", [], |r| r.get(0))?;
    if !present { return Ok(Vec::new()); }
    Ok(tx.prepare("SELECT submission_id FROM finding_submissions WHERE session_id=?1 ORDER BY submission_id")?.query_map([session], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
}

/// `(current revision, effective revision whose claims apply)` of a submission, up to `as_of`.
fn claim_revision(tx: &Connection, submission: i64, as_of: i64) -> Result<Option<(i64, i64)>> {
    Ok(tx.query_row("SELECT revision,coalesce(restores,revision) FROM finding_claim_sets WHERE submission_id=?1 AND seq<=?2 ORDER BY seq DESC LIMIT 1",
        params![submission, as_of], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)
}

fn finding_exists(tx: &Connection, finding: &str) -> Result<()> {
    if tx.query_row("SELECT EXISTS(SELECT 1 FROM canonical_findings WHERE finding_id=?1)", [finding], |r| r.get::<_, bool>(0))? { Ok(()) }
    else { Err(invalid(format!("no canonical finding {finding}"))) }
}

/// Active merge edges `source -> target` up to `as_of`.
pub(super) fn merges(tx: &Connection, as_of: i64) -> Result<BTreeMap<String, (String, i64)>> {
    let mut active = BTreeMap::new();
    let rows: Vec<(i64, String, String, String, Option<i64>)> = tx.prepare("SELECT seq,kind,source_finding,target_finding,reverses FROM finding_relationships WHERE seq<=?1 ORDER BY seq")?
        .query_map([as_of], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect::<rusqlite::Result<_>>()?;
    for (seq, kind, source, target, _) in rows {
        if kind == "merge" { active.insert(source, (target, seq)); } else { active.remove(&source); }
    }
    Ok(active)
}

pub(super) fn root(edges: &BTreeMap<String, (String, i64)>, finding: &str) -> String {
    let mut at = finding.to_owned();
    let mut steps = 0;
    while let Some((next, _)) = edges.get(&at) {
        at = next.clone();
        steps += 1;
        if steps > edges.len() { break; } // cycles are refused on write; never loop on a corrupt store
    }
    at
}

impl SqliteStore {
    /// Decide one claim of a submission's current claim set. Only the triage
    /// authority decides; a later decision on the claim supersedes the earlier
    /// one without deleting it. Validation needs evidence and a severity and
    /// mints a canonical finding unless it names an existing one.
    pub fn triage_finding_claim(&mut self, claim: i64, request: &TriageRequest, principal: &str, now: i64) -> Result<FindingEvent> {
        let evidence = evidence(&request.evidence)?;
        let (outcome, target, reason, severity) = match &request.outcome {
            TriageOutcome::Validated { target, severity } => {
                if !SEVERITIES.contains(&severity.as_str()) { return Err(invalid(format!("unknown severity {severity}"))); }
                if evidence.is_empty() { return Err(invalid("a validated finding needs at least one evidence reference".into())); }
                ("validated", Some(target.clone()), None, Some(severity.clone()))
            }
            TriageOutcome::Rejected { reason } => {
                if !REJECT_REASONS.contains(&reason.as_str()) { return Err(invalid(format!("unknown rejection reason {reason}"))); }
                ("rejected", None, Some(reason.clone()), None)
            }
            TriageOutcome::Duplicate { of } => ("duplicate", Some(FindingTarget::Existing(of.clone())), None, None),
            TriageOutcome::Pending { reason } => {
                if let Some(reason) = reason && !PENDING_REASONS.contains(&reason.as_str()) { return Err(invalid(format!("unknown pending reason {reason}"))); }
                ("pending", None, reason.clone(), None)
            }
        };
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema_55(&tx)?;
        triage_authority(&tx, principal)?;
        let (submission, revision): (i64, i64) = tx.query_row("SELECT submission_id,revision FROM finding_claims WHERE claim_id=?1", [claim], |r| Ok((r.get(0)?, r.get(1)?))).optional()?
            .ok_or_else(|| invalid(format!("no finding claim {claim}")))?;
        let current = claim_revision(&tx, submission, i64::MAX)?.map(|c| c.1);
        if current != Some(revision) { return Err(invalid(format!("claim {claim} is not in submission {submission}'s current claim set"))); }
        if let Some(FindingTarget::Existing(finding)) = &target { finding_exists(&tx, finding)?; }
        let previous: Option<i64> = tx.query_row("SELECT max(seq) FROM finding_decisions WHERE claim_id=?1", [claim], |r| r.get(0))?;
        let seq = log(&tx, "decided", principal, request.expected_seq, now)?;
        let finding = match target {
            None => None,
            Some(FindingTarget::Existing(finding)) => Some(finding),
            Some(FindingTarget::New { title }) => {
                let finding = format!("finding:canonical-{seq}");
                let title = title.as_deref().and_then(title_excerpt).or(tx.query_row("SELECT coalesce(c.title,s.title) FROM finding_claims c JOIN finding_submissions s ON s.submission_id=c.submission_id WHERE c.claim_id=?1",
                    [claim], |r| r.get::<_, Option<String>>(0))?);
                tx.execute("INSERT INTO canonical_findings(finding_id,seq,title) VALUES(?1,?2,?3)", params![finding, seq, title])?;
                Some(finding)
            }
        };
        let policy = severity.as_ref().map(|_| SEVERITY_POLICY);
        tx.execute("INSERT INTO finding_decisions(seq,claim_id,outcome,finding_id,reason,severity,severity_policy,evidence_refs,supersedes) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![seq, claim, outcome, finding, reason, severity, policy, serde_json::json!(evidence).to_string(), previous])?;
        let subject = serde_json::json!({"claim_id": claim, "submission_id": submission, "outcome": outcome, "finding_id": finding, "reason": reason,
            "severity": severity, "evidence_refs": evidence, "supersedes": previous});
        let out = event(&tx, seq, subject)?;
        tx.commit()?;
        Ok(out)
    }

    /// Split a submission into 2–32 claims (a new claim revision). The
    /// submission keeps its identity; earlier claims and decisions stay in history.
    pub fn split_finding_submission(&mut self, submission: i64, titles: &[Option<String>], expected_seq: Option<i64>, principal: &str, now: i64) -> Result<FindingEvent> {
        if titles.len() < 2 || titles.len() > MAX_CLAIMS { return Err(invalid(format!("a split has 2 to {MAX_CLAIMS} claims"))); }
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema_55(&tx)?;
        triage_authority(&tx, principal)?;
        let (current, _) = claim_revision(&tx, submission, i64::MAX)?.ok_or_else(|| invalid(format!("no finding submission {submission}")))?;
        let seq = log(&tx, "split", principal, expected_seq, now)?;
        let revision = current + 1;
        tx.execute("INSERT INTO finding_claim_sets(seq,submission_id,revision,kind,restores) VALUES(?1,?2,?3,'split',NULL)", params![seq, submission, revision])?;
        let mut claims = Vec::with_capacity(titles.len());
        for (i, title) in titles.iter().enumerate() {
            tx.execute("INSERT INTO finding_claims(submission_id,revision,ordinal,title) VALUES(?1,?2,?3,?4)",
                params![submission, revision, i as i64 + 1, title.as_deref().and_then(title_excerpt)])?;
            claims.push(tx.last_insert_rowid());
        }
        let out = event(&tx, seq, serde_json::json!({"submission_id": submission, "revision": revision, "claims": claims}))?;
        tx.commit()?;
        Ok(out)
    }

    /// Make an earlier split or initial claim revision current again (a new
    /// revision that reuses its claims and so their decisions).
    pub fn restore_finding_claims(&mut self, submission: i64, revision: i64, expected_seq: Option<i64>, principal: &str, now: i64) -> Result<FindingEvent> {
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema_55(&tx)?;
        triage_authority(&tx, principal)?;
        let (current, effective) = claim_revision(&tx, submission, i64::MAX)?.ok_or_else(|| invalid(format!("no finding submission {submission}")))?;
        let kind: Option<String> = tx.query_row("SELECT kind FROM finding_claim_sets WHERE submission_id=?1 AND revision=?2", params![submission, revision], |r| r.get(0)).optional()?;
        match kind.as_deref() {
            None => return Err(invalid(format!("submission {submission} has no claim revision {revision}"))),
            Some("restore") => return Err(invalid(format!("claim revision {revision} is itself a restore; name the revision it restored"))),
            _ if revision == effective => return Err(invalid(format!("claim revision {revision} is already current"))),
            _ => {}
        }
        let seq = log(&tx, "restored", principal, expected_seq, now)?;
        tx.execute("INSERT INTO finding_claim_sets(seq,submission_id,revision,kind,restores) VALUES(?1,?2,?3,'restore',?4)", params![seq, submission, current + 1, revision])?;
        let out = event(&tx, seq, serde_json::json!({"submission_id": submission, "revision": current + 1, "restores": revision}))?;
        tx.commit()?;
        Ok(out)
    }

    /// Merge canonical finding `source` into `target` (same root cause).
    /// Refuses cycles and findings already grouped together.
    pub fn merge_findings(&mut self, source: &str, target: &str, expected_seq: Option<i64>, principal: &str, now: i64) -> Result<FindingEvent> {
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema_55(&tx)?;
        triage_authority(&tx, principal)?;
        finding_exists(&tx, source)?;
        finding_exists(&tx, target)?;
        let edges = merges(&tx, i64::MAX)?;
        if let Some((into, _)) = edges.get(source) { return Err(invalid(format!("finding {source} is already merged into {into}"))); }
        if root(&edges, target) == root(&edges, source) { return Err(invalid(format!("findings {source} and {target} are already one group; a merge would form a cycle"))); }
        let seq = log(&tx, "merged", principal, expected_seq, now)?;
        tx.execute("INSERT INTO finding_relationships(seq,kind,source_finding,target_finding,reverses) VALUES(?1,'merge',?2,?3,NULL)", params![seq, source, target])?;
        let out = event(&tx, seq, serde_json::json!({"source_finding": source, "target_finding": target}))?;
        tx.commit()?;
        Ok(out)
    }

    /// Reverse the active merge recorded at `merge_seq`.
    pub fn unmerge_findings(&mut self, merge_seq: i64, expected_seq: Option<i64>, principal: &str, now: i64) -> Result<FindingEvent> {
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema_55(&tx)?;
        triage_authority(&tx, principal)?;
        let (source, target): (String, String) = tx.query_row("SELECT source_finding,target_finding FROM finding_relationships WHERE seq=?1 AND kind='merge'", [merge_seq],
            |r| Ok((r.get(0)?, r.get(1)?))).optional()?.ok_or_else(|| invalid(format!("no finding merge at seq {merge_seq}")))?;
        if merges(&tx, i64::MAX)?.get(&source).map(|e| e.1) != Some(merge_seq) { return Err(invalid(format!("the merge at seq {merge_seq} is not active"))); }
        let seq = log(&tx, "unmerged", principal, expected_seq, now)?;
        tx.execute("INSERT INTO finding_relationships(seq,kind,source_finding,target_finding,reverses) VALUES(?1,'unmerge',?2,?3,?4)", params![seq, source, target, merge_seq])?;
        let out = event(&tx, seq, serde_json::json!({"source_finding": source, "target_finding": target, "reverses": merge_seq}))?;
        tx.commit()?;
        Ok(out)
    }
}

/// Replay the finding history up to `as_of` (default: its head) on any
/// connection, read-only. `None` before migration 0055.
pub fn finding_state(db: &Connection, as_of: Option<i64>) -> Result<Option<FindingState>> {
    let present: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='finding_log')", [], |r| r.get(0))?;
    if !present { return Ok(None); }
    let head = head(db)?;
    let at = as_of.unwrap_or(head);
    if at < 0 || at > head { return Err(invalid(format!("as-of seq {at} is outside the finding history 0..={head}"))); }
    let edges = merges(db, at)?;
    let seed_linked = super::review_ledger::seed_linked_claims(db, at)?;
    // Latest decision per claim at the watermark: (seq, outcome, finding, reason, severity, evidence).
    type Decision = (i64, String, Option<String>, Option<String>, Option<String>, String);
    let mut decisions: BTreeMap<i64, Decision> = BTreeMap::new();
    for row in db.prepare("SELECT claim_id,seq,outcome,finding_id,reason,severity,evidence_refs FROM finding_decisions WHERE seq<=?1 ORDER BY seq")?
        .query_map([at], |r| Ok((r.get::<_, i64>(0)?, (r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))))? {
        let (claim, decision) = row?;
        decisions.insert(claim, decision);
    }
    type Row = (i64, i64, String, String, Option<String>, String, String, String, i64);
    let rows: Vec<Row> = db.prepare("SELECT s.submission_id,s.seq,s.session_id,s.finding_ref,s.title,s.trust,r.opportunity_id,r.attempt_id,l.recorded_unix_ms
        FROM finding_submissions s JOIN finding_log l ON l.seq=s.seq JOIN review_sessions r ON r.session_id=s.session_id WHERE s.seq<=?1 ORDER BY s.seq")?
        .query_map([at], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut submissions = Vec::with_capacity(rows.len());
    // Validated claims per group root, keyed by discovery order (submission seq, ordinal, claim).
    let mut groups: BTreeMap<String, BTreeSet<(i64, i64, i64)>> = BTreeMap::new();
    for (submission_id, seq, session_id, finding_ref, title, trust, opportunity_id, reporter, recorded) in rows {
        let (revision, effective) = claim_revision(db, submission_id, at)?.ok_or_else(|| StoreError::Corrupt(format!("finding submission {submission_id} has no claim set")))?;
        let claims: Vec<(i64, i64, Option<String>)> = db.prepare("SELECT claim_id,ordinal,title FROM finding_claims WHERE submission_id=?1 AND revision=?2 ORDER BY ordinal")?
            .query_map(params![submission_id, effective], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
        let claims = claims.into_iter().map(|(claim_id, ordinal, claim_title)| {
            let d = decisions.get(&claim_id);
            let decided = d.map_or("pending".to_owned(), |d| d.1.clone());
            let finding_id = d.and_then(|d| d.2.clone());
            let canonical = finding_id.as_deref().map(|f| root(&edges, f));
            if decided == "validated" && let Some(r) = &canonical { groups.entry(r.clone()).or_default().insert((seq, ordinal, claim_id)); }
            ClaimState { claim_id, ordinal, title: claim_title.or_else(|| title.clone()), decision_seq: d.map(|d| d.0), outcome: decided.clone(), decided, finding_id,
                canonical_finding: canonical, severity: d.and_then(|d| d.4.clone()), reason: d.and_then(|d| d.3.clone()),
                evidence_refs: d.map(|d| serde_json::from_str(&d.5).unwrap_or_default()).unwrap_or_default(), seed_linked: seed_linked.contains(&claim_id) }
        }).collect();
        submissions.push(SubmissionState { submission_id, seq, session_id, finding_ref, opportunity_id, reporter_attempt_id: reporter, title, recorded_unix_ms: recorded, trust,
            claim_revision: revision, claims, outcome: String::new(), has_validated_claim: false });
    }
    // Only a group's earliest validated claim is its discovery; later ones are duplicates.
    let discovery: BTreeMap<&String, i64> = groups.iter().filter_map(|(r, set)| set.first().map(|first| (r, first.2))).collect();
    for sub in &mut submissions {
        for claim in &mut sub.claims {
            if claim.decided == "validated" && claim.canonical_finding.as_ref().and_then(|r| discovery.get(r)) != Some(&claim.claim_id) { claim.outcome = "duplicate".into(); }
        }
        derive_outcome(sub);
    }
    let minted: Vec<(String, i64, Option<String>)> = db.prepare("SELECT finding_id,seq,title FROM canonical_findings WHERE seq<=?1 ORDER BY seq")?
        .query_map([at], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
    let findings: Vec<FindingGroupState> = minted.into_iter().map(|(finding_id, minted_seq, title)| {
        let root = root(&edges, &finding_id);
        let members = |outcome: &str| submissions.iter().flat_map(|s| &s.claims).filter(|c| c.finding_id.as_deref() == Some(&finding_id) && c.outcome == outcome).map(|c| c.claim_id).collect::<Vec<_>>();
        let discovery_claim = if root == finding_id { discovery.get(&finding_id).copied() } else { None };
        let status = if root != finding_id { "merged" } else if discovery_claim.is_some() { "validated" } else { "unvalidated" };
        FindingGroupState { merged_into: edges.get(&finding_id).map(|e| e.0.clone()), status: status.into(), discovery_claim, validated_claims: members("validated"),
            duplicate_claims: members("duplicate"), finding_id, title, minted_seq, root }
    }).collect();
    let unique_findings = findings.iter().filter(|f| f.status == "validated").count();
    let history = history(db, at)?;
    Ok(Some(FindingState { head_seq: head, as_of_seq: at, unique_findings, summary: FindingSummary::of(&submissions), submissions, findings, history }))
}

fn history(db: &Connection, at: i64) -> Result<Vec<FindingEvent>> {
    let rows: Vec<(i64, String, String, String, Option<i64>, i64)> = db.prepare("SELECT seq,kind,principal,authority,expected_seq,recorded_unix_ms FROM finding_log WHERE seq<=?1 ORDER BY seq")?
        .query_map([at], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::with_capacity(rows.len());
    for (seq, kind, principal, authority, expected_seq, recorded_unix_ms) in rows {
        let subject = match kind.as_str() {
            "submitted" => db.query_row("SELECT submission_id,session_id,finding_ref FROM finding_submissions WHERE seq=?1", [seq],
                |r| Ok(serde_json::json!({"submission_id": r.get::<_, i64>(0)?, "session_id": r.get::<_, String>(1)?, "finding_ref": r.get::<_, String>(2)?})))?,
            "decided" => db.query_row("SELECT claim_id,outcome,finding_id,reason,severity,supersedes FROM finding_decisions WHERE seq=?1", [seq],
                |r| Ok(serde_json::json!({"claim_id": r.get::<_, i64>(0)?, "outcome": r.get::<_, String>(1)?, "finding_id": r.get::<_, Option<String>>(2)?,
                    "reason": r.get::<_, Option<String>>(3)?, "severity": r.get::<_, Option<String>>(4)?, "supersedes": r.get::<_, Option<i64>>(5)?})))?,
            "split" | "restored" => db.query_row("SELECT submission_id,revision,restores FROM finding_claim_sets WHERE seq=?1", [seq],
                |r| Ok(serde_json::json!({"submission_id": r.get::<_, i64>(0)?, "revision": r.get::<_, i64>(1)?, "restores": r.get::<_, Option<i64>>(2)?})))?,
            _ => db.query_row("SELECT source_finding,target_finding,reverses FROM finding_relationships WHERE seq=?1", [seq],
                |r| Ok(serde_json::json!({"source_finding": r.get::<_, String>(0)?, "target_finding": r.get::<_, String>(1)?, "reverses": r.get::<_, Option<i64>>(2)?})))?,
        };
        out.push(FindingEvent { seq, kind, principal, authority, expected_seq, recorded_unix_ms, subject });
    }
    Ok(out)
}

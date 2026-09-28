//! Local integrator. Checks run on the candidate checkout, not the worker branch.
//! The configured ref moves only through `update-ref` with the expected old oid.
//! A lost reply confirms that exact oid and no other. Dependencies are not satisfied.
//! Ownership covers the claim, the merge and the publication; the candidate
//! policy checks hold only the shared root and the work directory's fence, and
//! everything publication depends on is rechecked once ownership is regained.
mod git;

use std::{fs, path::{Path, PathBuf}, time::Duration};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::{
    domain::OperationId,
    operations::Claim,
    runner::{RealRunner, Runner},
    store::{
        SqliteStore,
        integration::{
            IntegrationBegin, IntegrationFinish, IntegrationView, LEASE_OWNER, VerifiedIntegration,
        },
    },
    verification::{self, CheckOwnership, parse_checks, supervise},
};

use git::{BuiltCommit, GitRepo};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    None,
    #[cfg(test)]
    StaleBase,
    #[cfg(test)]
    CrashAfterRefUpdate,
    #[cfg(test)]
    CrashBeforeBuild,
    #[cfg(test)]
    CrashBeforeChecks,
    #[cfg(test)]
    UpdateRefRejected,
    #[cfg(test)]
    FailBuild,
    #[cfg(test)]
    ExpireBeforeRecord,
}

impl Default for Fault {
    fn default() -> Self {
        Self::None
    }
}

pub struct IntegrateRequest {
    pub result_id: String,
    pub idempotency_key: String,
    pub repository: PathBuf,
    pub work_dir: PathBuf,
    pub fault: Fault,
}

#[derive(Debug, serde::Serialize)]
pub struct IntegrateOutcome {
    pub operation_id: String,
    pub state: String,
    pub commit_oid: Option<String>,
    pub reason: Option<String>,
    /// Acceptance policies rechecked on the candidate, by id, with their verdicts.
    pub policies: Vec<PolicyCheck>,
}

#[derive(Debug, serde::Serialize)]
pub struct PolicyCheck {
    pub policy_id: String,
    pub passed: bool,
}

fn payload_digest(key: &str, result_id: &str, repository: &str, reference: &str) -> String {
    format!(
        "{:x}",
        Sha256::digest(format!("{key}\0{result_id}\0{repository}\0{reference}").as_bytes())
    )
}

fn outcome_of(view: &IntegrationView) -> IntegrateOutcome {
    IntegrateOutcome {
        operation_id: view.operation_id.clone(),
        state: view.state.clone(),
        commit_oid: view.commit_oid.clone(),
        reason: view.reason.clone(),
        policies: Vec::new(),
    }
}

pub fn configure_integration_ref(
    store: &mut SqliteStore,
    repository: &PathBuf,
    ref_name: &str,
) -> Result<()> {
    let repo = GitRepo::open(repository)?;
    store
        .configure_integration_ref(&repo.identity, ref_name)
        .map_err(anyhow::Error::from)
}

/// Explicit operator ingress; these commands never implicitly upgrade a store.
pub fn configure_project(project: &Path, repository: &PathBuf, reference: &str) -> Result<()> {
    let _guard = crate::migration::runtime_mutation(project)?;
    let mut store = crate::migration::open_active(project)?;
    if reference.len() > 1024 || !crate::store::integration::valid_ref_name(reference) { bail!("integration ref is invalid"); }
    let repo = GitRepo::open(repository)?;
    if repo.ref_oid(reference)?.is_none() { bail!("integration ref is missing"); }
    if repo.is_checked_out(reference)? { bail!("integration ref is checked out"); }
    store.configure_integration_ref(&repo.identity, reference)?;
    Ok(())
}

pub fn integrate_project(project: &Path, request: &IntegrateRequest) -> Result<IntegrateOutcome> {
    use std::os::unix::fs::DirBuilderExt;
    if !request.work_dir.is_absolute() { bail!("integration work directory must be absolute"); }
    validate_key(&request.idempotency_key)?;
    let scratch = crate::execution_guard::Resource::new("scratch", request.work_dir.display().to_string())?;
    let guard = crate::migration::runtime_mutation(project)?;
    let mut store = crate::migration::open_active(project)?;
    fs::DirBuilder::new().mode(0o700).create(&request.work_dir)
        .context("integration work directory must be new and have an existing parent")?;
    struct Work(Option<PathBuf>);
    impl Drop for Work { fn drop(&mut self) { if let Some(path) = &self.0 { let _ = fs::remove_dir_all(path); } } }
    let mut work = Work(Some(request.work_dir.clone()));
    // The work directory is this call's own, so it is removed with or without ownership.
    let mut ownership = verification::OperatorOwnership::new(guard, project, scratch, REGAIN);
    let outcome = integrate_at(&mut store, request, None, Some(&mut ownership));
    let cleanup = fs::remove_dir_all(&request.work_dir);
    if cleanup.is_ok() { work.0 = None; }
    let outcome = outcome?;
    cleanup.context("integration recorded but scratch cleanup failed")?;
    Ok(outcome)
}

fn validate_key(key: &str) -> Result<()> {
    if key.is_empty() || key.len() > 128 || key.chars().any(char::is_control) {
        bail!("integration idempotency key must be 1..128 bytes without control characters");
    }
    Ok(())
}

pub fn reconcile_project(project: &Path, repository: &PathBuf, key: &str) -> Result<IntegrateOutcome> {
    validate_key(key)?;
    let _guard = crate::migration::runtime_mutation(project)?;
    let mut store = crate::migration::open_active(project)?;
    reconcile_integration(&mut store, repository, key)
}

/// Each acceptance policy gets its own candidate check budget, like the
/// verifier's per-check timeout; one slow policy cannot starve the next.
pub const POLICY_TIMEOUT: Duration = Duration::from_secs(30);
/// Most policies whose checks fit the claim lease (the store allows 300 s) and
/// the automatic job budget (240 s) with room for the merge and publication.
pub const MAX_POLICIES: usize = 6;
/// The claim lease the checks run under: every policy's budget plus a margin
/// for recording the verdicts.
fn checks_lease_ms(policies: usize) -> i64 {
    (policies as u64 * POLICY_TIMEOUT.as_millis() as u64 + 30_000) as i64
}

/// How long an operator integration waits to regain ownership after its checks.
const REGAIN: Duration = Duration::from_secs(60);

/// Something publication depends on changed while the candidate checks ran
/// without project ownership; nothing was recorded or published.
#[derive(Debug)]
pub struct InputsChanged(pub String);
impl std::fmt::Display for InputsChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "integration inputs changed during the candidate check: {}; nothing was published", self.0)
    }
}
impl std::error::Error for InputsChanged {}

/// A refusal before any build or ref update.
#[derive(Debug)]
pub enum Refused {
    CheckedOut,
    TargetMoved { expected: String, current: String },
    TooManyPolicies { count: usize },
}
impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CheckedOut => f.write_str("integration ref is checked out"),
            Self::TargetMoved { expected, current } => write!(f, "integration target moved since verification: ref is at {current}, expected {expected}; blocked for an operator or replan"),
            Self::TooManyPolicies { count } => write!(f, "integration refused: {count} acceptance policies need {count} x {}s of candidate checks, more than one integration claim can cover (at most {MAX_POLICIES} policies); nothing was built; replan the contract with fewer policies", POLICY_TIMEOUT.as_secs()),
        }
    }
}
impl std::error::Error for Refused {}

pub fn integrate(store: &mut SqliteStore, request: &IntegrateRequest) -> Result<IntegrateOutcome> {
    integrate_at(store, request, None, None)
}

/// Automatic ingress. A new integration starts only when the target is at the
/// tip the controller expects (its latest recorded integration, else the base
/// the result was verified against); an existing one resumes under its key.
/// `ownership` is released only around the candidate policy checks.
pub fn integrate_job(store: &mut SqliteStore, request: &IntegrateRequest, submission_id: &str, ownership: &mut dyn CheckOwnership) -> Result<IntegrateOutcome> {
    let repo = GitRepo::open(&request.repository)?;
    let Some(reference) = store.integration_ref(&repo.identity)? else {
        bail!("integration ref is not configured");
    };
    let expected = store.expected_integration_tip(&repo.identity, &reference, submission_id)?;
    integrate_at(store, request, Some(&expected), Some(ownership))
}

/// A lost reply: classify the operation named by `key`, if any, without building.
pub fn observe_job(store: &mut SqliteStore, repository: &PathBuf, key: &str) -> Result<Option<IntegrateOutcome>> {
    let repo = GitRepo::open(repository)?;
    if store.find_integration(&repo.identity, key)?.is_none() {
        return Ok(None);
    }
    reconcile_integration(store, repository, key).map(Some)
}

fn integrate_at(store: &mut SqliteStore, request: &IntegrateRequest, expected_base: Option<&str>, ownership: Option<&mut dyn CheckOwnership>) -> Result<IntegrateOutcome> {
    let outcome = integrate_unlisted(store, request, expected_base, ownership)?;
    with_policies(store, outcome)
}

/// Attach the stored per-policy verdicts of the operation's candidate.
fn with_policies(store: &mut SqliteStore, mut outcome: IntegrateOutcome) -> Result<IntegrateOutcome> {
    outcome.policies = store
        .integration_policy_checks(&outcome.operation_id)?
        .into_iter()
        .map(|(policy_id, passed)| PolicyCheck { policy_id, passed })
        .collect();
    Ok(outcome)
}

fn integrate_unlisted(store: &mut SqliteStore, request: &IntegrateRequest, expected_base: Option<&str>, ownership: Option<&mut dyn CheckOwnership>) -> Result<IntegrateOutcome> {
    let repo = GitRepo::open(&request.repository)?;
    let Some(reference) = store.integration_ref(&repo.identity)? else {
        bail!("integration ref is not configured");
    };
    let verified = store.load_verified_for_integration(&request.result_id)?;
    if verified.repository != repo.identity {
        bail!("verified result belongs to another repository");
    }
    if verified.route != "verify_then_integrate" {
        bail!("verified result is not routed to integration");
    }
    if verified.policies.len() > MAX_POLICIES {
        return Err(Refused::TooManyPolicies { count: verified.policies.len() }.into());
    }
    let digest = payload_digest(
        &request.idempotency_key,
        &request.result_id,
        &repo.identity,
        &reference,
    );
    if let Some(existing) = store.find_integration(&repo.identity, &request.idempotency_key)? {
        if existing.payload_digest != digest || existing.verified_result_id != request.result_id {
            bail!("integration idempotency conflict");
        }
        // An existing generation is classified from the ref. Do not discard a candidate
        // that is already the ref tip.
        return resume(store, &repo, request, existing, false, ownership);
    }
    if repo.is_checked_out(&reference)? {
        return Err(Refused::CheckedOut.into());
    }
    let Some(base) = repo.ref_oid(&reference)? else {
        bail!("integration ref is missing");
    };
    if let Some(expected) = expected_base.filter(|expected| *expected != base) {
        return Err(Refused::TargetMoved { expected: expected.to_owned(), current: base }.into());
    }
    if base.len() != oid_len(&verified.object_format)? {
        bail!("integration ref oid does not match object format");
    }
    let operation_id = store.begin_integration(&IntegrationBegin {
        repository: repo.identity.clone(),
        ref_name: reference,
        expected_old_oid: base.clone(),
        verified: verified.clone(),
        idempotency_key: request.idempotency_key.clone(),
        payload_digest: digest,
    })?;
    let now = now_ms();
    let operation = OperationId::new(operation_id.clone()).map_err(anyhow::Error::msg)?;
    let claim = match store.claim_operation(&operation, 1, LEASE_OWNER, now, 60_000) {
        Ok(claim) => claim,
        Err(error) => {
            let _ = store.abandon_unclaimed(&operation_id);
            return Err(error.into());
        }
    };
    drive_new(store, &repo, request, &verified, &base, claim, ownership)
}

pub fn reconcile_integration(
    store: &mut SqliteStore,
    repository: &PathBuf,
    idempotency_key: &str,
) -> Result<IntegrateOutcome> {
    let repo = GitRepo::open(repository)?;
    let Some(view) = store.find_integration(&repo.identity, idempotency_key)? else {
        bail!("integration operation is missing");
    };
    let request = IntegrateRequest {
        result_id: view.verified_result_id.clone(),
        idempotency_key: idempotency_key.to_string(),
        repository: repository.clone(),
        work_dir: PathBuf::new(),
        fault: Fault::None,
    };
    // Reconcile never builds another merge. It only classifies the ref we already named.
    let outcome = resume(store, &repo, &request, view, true, None)?;
    with_policies(store, outcome)
}

fn resume(
    store: &mut SqliteStore,
    repo: &GitRepo,
    request: &IntegrateRequest,
    view: IntegrationView,
    reconcile: bool,
    ownership: Option<&mut dyn CheckOwnership>,
) -> Result<IntegrateOutcome> {
    if matches!(
        view.state.as_str(),
        "integrated" | "blocked" | "discarded" | "reconciliation_required"
    ) {
        return Ok(outcome_of(&view));
    }
    if !view.checks_passed {
        // Checks have not passed, so there is no candidate checkout to run.
        // An empty reconcile work directory must not discard a stored commit.
        if reconcile {
            return unprepared_status(store, repo, &view);
        }
        return resume_incomplete(store, repo, request, &view, ownership);
    }
    settle(store, repo, request, &view, true)
}

fn unprepared_status(
    store: &mut SqliteStore,
    repo: &GitRepo,
    view: &IntegrationView,
) -> Result<IntegrateOutcome> {
    if view.state != "candidate_prepared" {
        let loaded = store.load_integration_operation(&view.operation_id)?;
        return Ok(outcome_of(&loaded));
    }
    let oid = view
        .commit_oid
        .clone()
        .context("integration candidate is missing")?;
    let tree = view
        .tree_oid
        .clone()
        .context("integration candidate is missing")?;
    if repo.has_object(&oid)? && repo.has_object(&tree)? {
        let loaded = store.load_integration_operation(&view.operation_id)?;
        return Ok(outcome_of(&loaded));
    }
    discard_absent_candidate(store, repo, view)
}

fn discard_absent_candidate(
    store: &mut SqliteStore,
    repo: &GitRepo,
    view: &IntegrationView,
) -> Result<IntegrateOutcome> {
    let Some(current) = repo.ref_oid(&view.ref_name)? else {
        bail!("integration ref is missing");
    };
    let claim = fresh_claim(store, &view.operation_id).ok();
    if current == view.expected_old_oid {
        return finish(
            store,
            &view.operation_id,
            claim.as_ref(),
            IntegrationFinish::Discarded {
                reason: "candidate_missing",
            },
        );
    }
    finish(
        store,
        &view.operation_id,
        claim.as_ref(),
        IntegrationFinish::Reconciliation {
            reason: "ambiguous_ref",
        },
    )
}

fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

fn drive_new(
    store: &mut SqliteStore,
    repo: &GitRepo,
    request: &IntegrateRequest,
    verified: &VerifiedIntegration,
    base: &str,
    claim: Claim,
    ownership: Option<&mut dyn CheckOwnership>,
) -> Result<IntegrateOutcome> {
    #[cfg(test)]
    if request.fault == Fault::CrashBeforeBuild {
        let view = store.load_integration_operation(claim.operation.as_str())?;
        return Ok(outcome_of(&view));
    }
    #[cfg(test)]
    if request.fault == Fault::FailBuild {
        let _ = ensure_retryable(store, claim.operation.as_str());
        bail!("integration build failed");
    }
    let built = match repo.build_merge(&request.work_dir, base, &verified.commit_oid) {
        Ok(built) => built,
        Err(error) => {
            // The pre-build lease may already be dead. Requeue so the next retry can record.
            let _ = ensure_retryable(store, claim.operation.as_str());
            return Err(error);
        }
    };
    let operation_id = claim.operation.as_str().to_string();
    let BuiltCommit::Ready {
        oid,
        tree,
        checkout,
    } = built
    else {
        let renewed = fresh_claim(store, &operation_id).ok();
        return finish(
            store,
            &operation_id,
            renewed.as_ref(),
            IntegrationFinish::Blocked {
                reason: "merge_conflict",
            },
        );
    };
    // The merge can outlive the 60s lease. Renew immediately before recording M.
    #[cfg(test)]
    if request.fault == Fault::ExpireBeforeRecord {
        store.testing_expire_lease(claim.operation.as_str())?;
        store.expire_claims(1)?;
    }
    let claim = match fresh_claim(store, &operation_id) {
        Ok(claim) => claim,
        Err(_) => {
            return finish(
                store,
                &operation_id,
                None,
                IntegrationFinish::Discarded {
                    reason: "claim_expired",
                },
            );
        }
    };
    // The repository must hold M before the row exists, so resume does not depend on the work dir.
    repo.copy_objects_from(&checkout)?;
    if !repo.has_object(&oid)? || !repo.has_object(&tree)? {
        let _ = ensure_retryable(store, &operation_id);
        bail!("candidate commit was not stored");
    }
    let now = now_ms();
    if let Err(error) = store.record_candidate(&claim, &oid, &tree, &verified.commit_oid, now) {
        let _ = ensure_retryable(store, &operation_id);
        return Err(error.into());
    }
    #[cfg(test)]
    if request.fault == Fault::CrashBeforeChecks {
        let view = store.load_integration_operation(claim.operation.as_str())?;
        return Ok(outcome_of(&view));
    }
    let (passed, claim) = policies_pass(store, claim, &request.work_dir, &checkout, verified, &oid, &tree, ownership)?;
    if !passed {
        return finish(
            store,
            claim.operation.as_str(),
            Some(&claim),
            IntegrationFinish::Blocked {
                reason: "checks_failed",
            },
        );
    }
    store.mark_checks_passed(&claim, now)?;
    repo.copy_objects_from(&checkout)?;
    if !repo.commit_matches(&oid, &tree, base, &verified.commit_oid)? {
        bail!("candidate objects are not in the repository");
    }
    let view = store.load_integration_operation(claim.operation.as_str())?;
    settle(store, repo, request, &view, true)
}

fn resume_incomplete(
    store: &mut SqliteStore,
    repo: &GitRepo,
    request: &IntegrateRequest,
    view: &IntegrationView,
    ownership: Option<&mut dyn CheckOwnership>,
) -> Result<IntegrateOutcome> {
    let Some(current) = repo.ref_oid(&view.ref_name)? else {
        bail!("integration ref is missing");
    };
    let candidate = view.commit_oid.clone();
    if current != view.expected_old_oid && candidate.as_deref() != Some(current.as_str()) {
        let claim = fresh_claim(store, &view.operation_id)?;
        return finish(
            store,
            &view.operation_id,
            Some(&claim),
            IntegrationFinish::Discarded { reason: "stale_base" },
        );
    }
    let claim = fresh_claim(store, &view.operation_id)?;
    if view.state == "effect_pending" {
        let verified = store.load_verified_for_integration(&view.verified_result_id)?;
        return drive_new(
            store,
            repo,
            request,
            &verified,
            &view.expected_old_oid,
            claim,
            ownership,
        );
    }
    let oid = view
        .commit_oid
        .clone()
        .context("integration candidate is missing")?;
    let tree = view
        .tree_oid
        .clone()
        .context("integration candidate is missing")?;
    if !repo.has_object(&oid)? || !repo.has_object(&tree)? {
        return discard_absent_candidate(store, repo, view);
    }
    let verified = store.load_verified_for_integration(&view.verified_result_id)?;
    // Checkout failure leaves candidate_prepared. Only a missing object discards it.
    let checkout = repo.checkout_candidate(&request.work_dir, &oid)?;
    let (passed, claim) = policies_pass(store, claim, &request.work_dir, &checkout, &verified, &oid, &tree, ownership)?;
    if !passed {
        return finish(
            store,
            claim.operation.as_str(),
            Some(&claim),
            IntegrationFinish::Blocked {
                reason: "checks_failed",
            },
        );
    }
    store.mark_checks_passed(&claim, now_ms())?;
    repo.copy_objects_from(&checkout)?;
    if !repo.commit_matches(&oid, &tree, &view.expected_old_oid, &verified.commit_oid)? {
        bail!("candidate objects are not in the repository");
    }
    let view = store.load_integration_operation(&view.operation_id)?;
    settle(store, repo, request, &view, true)
}

/// Classify the ref, then publish only under a fresh claim. The update-ref status is not the decision.
fn settle(
    store: &mut SqliteStore,
    repo: &GitRepo,
    request: &IntegrateRequest,
    view: &IntegrationView,
    may_publish: bool,
) -> Result<IntegrateOutcome> {
    #[cfg(not(test))]
    let _ = request;
    if view.generation <= 0 || view.parent_base.as_deref() != Some(view.expected_old_oid.as_str()) {
        return finish_flexible(
            store,
            view,
            None,
            IntegrationFinish::Reconciliation {
                reason: "ambiguous_ref",
            },
        );
    }
    #[cfg(test)]
    if request.fault == Fault::StaleBase && may_publish {
        repo.advance_ref(&view.ref_name, &view.expected_old_oid)?;
    }
    let commit = view
        .commit_oid
        .clone()
        .context("integration candidate is missing")?;
    let tree = view
        .tree_oid
        .clone()
        .context("integration candidate is missing")?;
    let parent = view
        .parent_verified
        .clone()
        .context("integration candidate is missing")?;
    let Some(current) = repo.ref_oid(&view.ref_name)? else {
        bail!("integration ref is missing");
    };
    // Recheck even a historical prepared candidate whose policy checks passed
    // before required outputs were enforced on the combined tree.
    let required = store.integration_required_outputs(&view.verified_result_id)?;
    if !repo.required_outputs_present(&commit, &required)? {
        let finish = if current == commit || view.reason.as_deref() == Some("publish_attempted") {
            IntegrationFinish::Reconciliation { reason: "required_output_missing" }
        } else {
            IntegrationFinish::Blocked { reason: "required_output_missing" }
        };
        return finish_flexible(store, view, None, finish);
    }
    if repo.commit_matches(&commit, &tree, &view.expected_old_oid, &parent)? && current == commit {
        return finish_flexible(store, view, None, IntegrationFinish::Confirm);
    }
    if current == view.expected_old_oid {
        if !may_publish {
            let loaded = store.load_integration_operation(&view.operation_id)?;
            return Ok(outcome_of(&loaded));
        }
        let other = store.other_integration_generation(
            &view.repository,
            &view.ref_name,
            &view.operation_id,
        )?;
        if other {
            let loaded = store.load_integration_operation(&view.operation_id)?;
            return Ok(outcome_of(&loaded));
        }
        if repo.is_checked_out(&view.ref_name)? {
            return Err(Refused::CheckedOut.into());
        }
        let claim = fresh_claim(store, &view.operation_id)?;
        store.mark_publish_attempted(&claim, now_ms())?;
        #[cfg(test)]
        if request.fault == Fault::UpdateRefRejected {
            let loaded = store.load_integration_operation(&view.operation_id)?;
            return Ok(outcome_of(&loaded));
        }
        // A non-zero exit is not the classification. The following read-back is.
        let _ = repo.cas_ref(&view.ref_name, &commit, &view.expected_old_oid)?;
        #[cfg(test)]
        if request.fault == Fault::CrashAfterRefUpdate {
            let after = repo.ref_oid(&view.ref_name)?;
            if after.as_deref() == Some(commit.as_str()) {
                let loaded = store.load_integration_operation(&view.operation_id)?;
                return Ok(outcome_of(&loaded));
            }
        }
        return classify_readback(store, repo, view, &commit, &tree, &parent);
    }
    if view.reason.as_deref() == Some("publish_attempted") || current == commit {
        return finish_flexible(
            store,
            view,
            None,
            IntegrationFinish::Reconciliation {
                reason: "ambiguous_ref",
            },
        );
    }
    let claim = fresh_claim(store, &view.operation_id).ok();
    finish_flexible(
        store,
        view,
        claim.as_ref(),
        IntegrationFinish::Discarded { reason: "stale_base" },
    )
}

fn classify_readback(
    store: &mut SqliteStore,
    repo: &GitRepo,
    view: &IntegrationView,
    commit: &str,
    tree: &str,
    parent: &str,
) -> Result<IntegrateOutcome> {
    let Some(after) = repo.ref_oid(&view.ref_name)? else {
        bail!("integration ref is missing");
    };
    if after == commit && repo.commit_matches(commit, tree, &view.expected_old_oid, parent)? {
        return finish_flexible(store, view, None, IntegrationFinish::Confirm);
    }
    if after == view.expected_old_oid {
        // The ref did not move. A later claim may retry update-ref.
        let loaded = store.load_integration_operation(&view.operation_id)?;
        return Ok(outcome_of(&loaded));
    }
    finish_flexible(
        store,
        view,
        None,
        IntegrationFinish::Reconciliation {
            reason: "ambiguous_ref",
        },
    )
}

fn ensure_retryable(store: &mut SqliteStore, operation_id: &str) -> Result<()> {
    let now = now_ms();
    if store.held_claim(operation_id, now).is_ok() {
        return Ok(());
    }
    if store.requeue_expired_lease(operation_id, now).is_ok() {
        return Ok(());
    }
    store
        .finish_integration_observed(
            operation_id,
            IntegrationFinish::Discarded {
                reason: "claim_expired",
            },
            now,
        )
        .map_err(anyhow::Error::from)
}

fn fresh_claim(store: &mut SqliteStore, operation_id: &str) -> Result<Claim> {
    let now = now_ms();
    if let Ok(claim) = store.held_claim(operation_id, now) {
        store.validate_claim(&claim, now)?;
        return Ok(claim);
    }
    let revision = store.requeue_expired_lease(operation_id, now)?;
    let id = OperationId::new(operation_id.to_string()).map_err(anyhow::Error::msg)?;
    let claim = store.claim_operation(&id, revision, LEASE_OWNER, now, 60_000)?;
    store.validate_claim(&claim, now)?;
    Ok(claim)
}

fn finish_flexible(
    store: &mut SqliteStore,
    view: &IntegrationView,
    claim: Option<&Claim>,
    kind: IntegrationFinish,
) -> Result<IntegrateOutcome> {
    let now = now_ms();
    if let Some(claim) = claim {
        if store.held_claim(claim.operation.as_str(), now).is_ok() {
            store.finish_integration(claim, kind, now)?;
            let loaded = store.load_integration_operation(&view.operation_id)?;
            return Ok(outcome_of(&loaded));
        }
    }
    store.finish_integration_observed(&view.operation_id, kind, now)?;
    let loaded = store.load_integration_operation(&view.operation_id)?;
    Ok(outcome_of(&loaded))
}

fn finish(
    store: &mut SqliteStore,
    operation_id: &str,
    claim: Option<&Claim>,
    kind: IntegrationFinish,
) -> Result<IntegrateOutcome> {
    let view = store.load_integration_operation(operation_id)?;
    finish_flexible(store, &view, claim, kind)
}

/// Every acceptance policy of the contract revision runs on the candidate, in
/// id order, each within its own `POLICY_TIMEOUT`; the first failure stops the
/// check. The claim is first extended to cover every policy's budget, and each
/// verdict is recorded under it. Returns the extended claim. With `ownership`,
/// the checks run without project ownership, and nothing is recorded unless
/// ownership is regained and every input is [`unchanged`].
#[allow(clippy::too_many_arguments)]
fn policies_pass(
    store: &mut SqliteStore,
    claim: Claim,
    work: &Path,
    checkout: &Path,
    verified: &VerifiedIntegration,
    commit: &str,
    tree: &str,
    ownership: Option<&mut dyn CheckOwnership>,
) -> Result<(bool, Claim)> {
    if verified.policies.len() > MAX_POLICIES {
        bail!("{}", Refused::TooManyPolicies { count: verified.policies.len() });
    }
    let claim = store.extend_integration_lease(&claim, checks_lease_ms(verified.policies.len()), now_ms())?;
    let run = || -> Result<(Vec<(String, String, bool)>, bool)> {
        let mut checks = Vec::new();
        let mut all = !verified.policies.is_empty();
        for (index, (policy_id, body)) in verified.policies.iter().enumerate() {
            let passed = check_passes(work, checkout, index, body, POLICY_TIMEOUT, commit, tree)?;
            checks.push((policy_id.clone(), format!("{:x}", Sha256::digest(body.as_bytes())), passed));
            if !passed {
                all = false;
                break;
            }
        }
        Ok((checks, all))
    };
    // The checks touch only the candidate checkout in the fenced work directory.
    let ran = match ownership {
        Some(ownership) => {
            ownership.release()?;
            let ran = run();
            ownership.reacquire(store)?;
            unchanged(store, &claim, verified, commit, tree)?;
            ran
        }
        None => run(),
    };
    let (checks, all) = ran?;
    store.record_policy_checks(&claim, &checks, now_ms())?;
    Ok((all, claim))
}

/// Everything publication depends on, reloaded under regained ownership: the
/// claim (lease and task revision), the verified result with its contract and
/// policies, and the recorded candidate. `settle` then classifies the target
/// ref, and the compare-and-swap enforces its expected old oid.
fn unchanged(store: &mut SqliteStore, claim: &Claim, verified: &VerifiedIntegration, commit: &str, tree: &str) -> Result<()> {
    let changed = |what: String| anyhow::Error::from(InputsChanged(what));
    store.validate_claim(claim, now_ms()).map_err(|error| changed(format!("integration claim no longer holds: {error}")))?;
    let fresh = store.load_verified_for_integration(&verified.result_id).map_err(|error| changed(error.to_string()))?;
    if fresh != *verified {
        return Err(changed("task, contract, policies or verified result changed".into()));
    }
    let view = store.load_integration_operation(claim.operation.as_str())?;
    if view.state != "candidate_prepared" || view.checks_passed || view.commit_oid.as_deref() != Some(commit) || view.tree_oid.as_deref() != Some(tree) {
        return Err(changed("the candidate changed".into()));
    }
    Ok(())
}

fn check_passes(
    work: &Path,
    checkout: &Path,
    index: usize,
    body: &str,
    timeout: Duration,
    commit: &str,
    tree: &str,
) -> Result<bool> {
    let checks = match parse_checks(body.as_bytes()) {
        Ok(checks) => checks,
        Err(_) => return Ok(false),
    };
    let unshare = std::path::PathBuf::from("/usr/bin/unshare");
    if !verification::unshare_ready(&unshare) {
        return Ok(false);
    }
    let policy_path = work.join(format!("integration-policy-{index}.json"));
    std::fs::write(&policy_path, body.as_bytes()).context("policy file")?;
    let policy_digest = format!("{:x}", Sha256::digest(body.as_bytes()));
    // The child copies the candidate checkout into its tmpfs. Do not point it at the host branch.
    let launch = supervise::launch(&supervise::Spec {
        unshare_program: unshare,
        timeout,
        checkout: checkout.to_path_buf(),
        policy: policy_path,
        scratch: work.join(format!("ns-root-{index}")),
        checks,
        commit: commit.to_string(),
        tree: tree.to_string(),
        policy_digest,
    });
    let Ok(launch) = launch else {
        return Ok(false);
    };
    let output = match RealRunner.run(&launch.cmd) {
        Ok(output) => output,
        Err(_) => return Ok(false),
    };
    Ok(verification::isolated_check_ok(&output, commit, tree))
}

fn oid_len(format: &str) -> Result<usize> {
    match format {
        "sha1" => Ok(40),
        "sha256" => Ok(64),
        _ => bail!("unsupported object format"),
    }
}

#[cfg(test)]
mod tests;

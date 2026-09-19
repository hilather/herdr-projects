//! One durable attempt. Production adapters must retain resource ownership guards
//! and enforce lifecycle/reconciliation policy; this service never grants either.
use anyhow::{Context, Result};
use crate::{domain::{Operation, OperationId}, store::SqliteStore};
use super::{Claim, Delivery, Outcome};

pub trait DeliveryAdapter {
    type Prepared: PreparedDelivery;
    /// No external effect. Validate kind/version, lifecycle, authority and target
    /// identity, and acquire an exclusive resource guard retained by Prepared.
    fn prepare(&mut self, operation: &Operation) -> Result<Self::Prepared>;
}

pub trait PreparedDelivery {
    /// Recheck policy/liveness after durable claim. Must not perform the effect.
    fn revalidate(&mut self, operation: &Operation) -> Result<()>;
    /// Runs outside a SQLite transaction. A bare error is always ambiguous.
    /// Retryable requires proof of no effect; success requires a receipt/identity.
    /// Adapter timeout must fit the remaining lease. Keep ownership until dropped.
    fn deliver(&mut self, operation: &Operation, claim: &Claim) -> Result<Outcome>;
}

pub struct DispatchRequest<'a> {
    pub operation: &'a OperationId,
    pub expected_revision: u64,
    pub owner: &'a str,
    pub lease_ms: i64,
}

#[derive(Debug)]
pub enum DispatchResult {
    Recorded(Delivery),
    /// The effect may already exist. Do not call deliver again. Expiry and
    /// explicit observation must resolve this claim, including after restart.
    Unrecorded { claim: Claim, error: String },
}

pub fn dispatch_one<A: DeliveryAdapter>(
    store: &mut SqliteStore,
    request: DispatchRequest<'_>,
    adapter: &mut A,
    mut clock: impl FnMut() -> i64,
) -> Result<DispatchResult> {
    let operation = store.read_snapshot(None)?.operations.into_iter()
        .find(|op| &op.id == request.operation).context("operation not found")?;
    let mut prepared = adapter.prepare(&operation)?;
    let claim = store.claim_operation(request.operation, request.expected_revision,
        request.owner, clock(), request.lease_ms)?;
    let outcome = if prepared.revalidate(&operation).is_err() {
        Outcome::Retryable { no_effect_evidence: "adapter authorization withdrawn before deliver was called".into() }
    } else {
        // Revalidation may take time or allow another writer to change the task.
        if let Err(error) = store.validate_claim(&claim, clock()) {
            return Ok(DispatchResult::Unrecorded { claim, error: error.to_string() });
        }
        prepared.deliver(&operation, &claim).unwrap_or_else(|_|
            Outcome::Ambiguous { observation_required: "adapter failed after delivery began; observe effect before retry (details withheld)".into() })
    };
    let result = match store.finish_operation(&claim, outcome, clock()) {
        Ok(delivery) => DispatchResult::Recorded(delivery),
        Err(error) => DispatchResult::Unrecorded { claim, error: error.to_string() },
    };
    // Keep the adapter's resource guard alive through receipt persistence.
    drop(prepared);
    Ok(result)
}

#[cfg(test)]
mod tests;

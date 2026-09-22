//! Bounded advancement of an explicitly selected, durably prepared launch.
use super::*;

/// Continue creation, release and naming using their one-use durable boundaries.
/// A missing resource returns `None` and retains capacity. Errors leave the last
/// committed boundary intact; a subsequent call observes it instead of replaying
/// an uncertain request. Callers must fetch the current delivery revision again.
/// This service does not admit tasks, fabricate profiles or enable scheduling.
pub fn advance_launch(
    project: &Path,
    operation: &OperationId,
    expected_revision: u64,
    deadline: Instant,
    cancellation: Cancellation,
) -> Result<Option<LaunchStartedReceipt>> {
    use crate::operations::DeliveryState;
    let deadline = deadline.min(Instant::now() + Duration::from_secs(45));
    check(deadline, &cancellation)?;
    let project = project.canonicalize()?;
    let state = crate::runtime::snapshot(&project)?;
    let delivery = state
        .deliveries
        .iter()
        .find(|d| &d.operation == operation)
        .context("launch delivery missing")?;
    ensure!(
        delivery.revision == expected_revision,
        "launch delivery changed"
    );
    ensure!(
        state
            .operations
            .iter()
            .any(|o| &o.id == operation && o.kind == "runtime.launch"),
        "operation is not a worker launch"
    );
    if delivery.state == DeliveryState::Confirmed {
        return super::reconcile_start(
            &project,
            operation,
            expected_revision,
            deadline,
            cancellation,
        )
        .map(Some);
    }
    let revision = if delivery.state == DeliveryState::Pending && delivery.attempts == 0 {
        let profile = state
            .attempt_inputs
            .iter()
            .find(|r| &r.operation == operation)
            .and_then(|r| r.inputs.effective_profile.as_ref())
            .context("launch profile missing")?;
        super::resources::validate_execution_home(profile, &project)?;
        super::create_resource(
            &project,
            operation,
            expected_revision,
            deadline,
            cancellation.clone(),
        )?;
        // Creation takes exactly one claim. Never adopt an unrelated new revision.
        expected_revision
            .checked_add(1)
            .context("launch revision overflow")?
    } else {
        ensure!(
            delivery.attempts == 1
                && matches!(
                    delivery.state,
                    DeliveryState::Claimed | DeliveryState::Ambiguous
                ),
            "launch has no recoverable original claim"
        );
        if delivery.state==DeliveryState::Claimed
            && state.events.iter().any(|e|e.entity==operation.as_str()&&e.kind=="runtime.worktrees_creation")
            && !state.events.iter().any(|e|e.entity==operation.as_str()&&e.kind=="runtime.launch_creation") {
            super::create_resource(&project,operation,expected_revision,deadline,cancellation.clone())?;
        }
        if delivery.state == DeliveryState::Claimed
            && state
                .events
                .iter()
                .any(|e| e.entity == operation.as_str() && e.kind == "runtime.launch_workspace")
            && !state.events.iter().any(|e| {
                e.entity == operation.as_str()
                    && matches!(
                        e.kind.as_str(),
                        "runtime.launch_layout"
                            | "runtime.launch_target"
                            | "runtime.launch_started"
                    )
            })
        {
            super::continue_workspace_layout(
                &project,
                operation,
                expected_revision,
                deadline,
                cancellation.clone(),
            )?;
        }
        if super::reconcile_resource(
            &project,
            operation,
            expected_revision,
            deadline,
            cancellation.clone(),
        )?
        .is_none()
        {
            return Ok(None);
        }
        expected_revision
    };
    check(deadline, &cancellation)?;
    let state = crate::runtime::snapshot(&project)?;
    let delivery = state
        .deliveries
        .iter()
        .find(|d| &d.operation == operation)
        .context("launch delivery missing")?;
    ensure!(
        delivery.revision == revision,
        "launch changed during advancement"
    );
    let has = |kind: &str| {
        state
            .events
            .iter()
            .any(|e| e.entity == operation.as_str() && e.kind == kind)
    };
    if !has("runtime.launch_release") {
        super::release_gate(
            &project,
            operation,
            revision,
            deadline,
            cancellation.clone(),
        )?;
    }
    check(deadline, &cancellation)?;
    // The naming service performs an effect only for an unnamed exact worker
    // under current authority, and refuses any prior naming intent. An uncertain
    // naming request, or expired authority, takes the observation-only path.
    if has("runtime.launch_name")
        || delivery.state != DeliveryState::Claimed
        || delivery.lease_until_ms.is_none_or(|until| until <= now())
    {
        super::reconcile_start(&project, operation, revision, deadline, cancellation).map(Some)
    } else {
        super::name_started_agent(&project, operation, revision, deadline, cancellation).map(Some)
    }
}

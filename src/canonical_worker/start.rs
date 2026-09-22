//! Start observation and explicit, one-use deterministic naming.
use super::*;

pub fn reconcile_start(
    project: &Path,
    operation: &OperationId,
    expected_revision: u64,
    deadline: Instant,
    cancellation: Cancellation,
) -> Result<LaunchStartedReceipt> {
    finish_start(
        project,
        operation,
        expected_revision,
        deadline,
        cancellation,
        false,
    )
}

/// Assign the expected name to an unnamed, exact live agent once, then observe
/// start. A lost acknowledgment is recovered by observation, never rename replay.
pub fn name_started_agent(
    project: &Path,
    operation: &OperationId,
    expected_revision: u64,
    deadline: Instant,
    cancellation: Cancellation,
) -> Result<LaunchStartedReceipt> {
    finish_start(
        project,
        operation,
        expected_revision,
        deadline,
        cancellation,
        true,
    )
}

fn finish_start(
    project: &Path,
    operation: &OperationId,
    expected_revision: u64,
    deadline: Instant,
    cancellation: Cancellation,
    allow_name: bool,
) -> Result<LaunchStartedReceipt> {
    let deadline = deadline.min(Instant::now() + Duration::from_secs(45));
    check(deadline, &cancellation)?;
    let project = project.canonicalize()?;
    let guard = crate::execution_guard::RootGuard::exclusive(
        project.parent().context("project root missing")?,
    )?;
    let mut db = crate::migration::open_active(&project)?;
    let state = db.read_snapshot(None)?;
    let delivery = state
        .deliveries
        .iter()
        .find(|d| &d.operation == operation)
        .context("launch delivery missing")?;
    ensure!(
        delivery.revision == expected_revision,
        "launch delivery changed"
    );
    if delivery.state == crate::operations::DeliveryState::Confirmed {
        let event = state
            .events
            .iter()
            .find(|e| e.kind == "runtime.launch_started" && e.entity == operation.as_str())
            .context("confirmed start receipt missing")?;
        let receipt: LaunchStartedReceipt = serde_json::from_value(event.payload.clone())?;
        ensure!(
            receipt.operation == *operation
                && delivery.last_outcome
                    == Some(crate::operations::Outcome::Confirmed {
                        observed_identity: serde_json::to_string(&receipt)?
                    }),
            "confirmed start identity mismatch"
        );
        return Ok(receipt);
    }
    ensure!(
        delivery.attempts == 1
            && matches!(
                delivery.state,
                crate::operations::DeliveryState::Claimed
                    | crate::operations::DeliveryState::Ambiguous
            ),
        "launch has no recoverable claim"
    );
    let record = state
        .attempt_inputs
        .iter()
        .find(|r| &r.operation == operation)
        .context("launch inputs missing")?;
    let worktrees = crate::worktree_preparation::pin_started_held(
        &project,
        &state,
        record,
        deadline,
        cancellation.clone(),
    )?;
    let profile = record
        .inputs
        .effective_profile
        .as_ref()
        .context("retained profile missing")?;
    profile.validate_for_launch().map_err(anyhow::Error::msg)?;
    ensure!(
        profile.herdr.version == "0.9.1",
        "start observation requires Herdr 0.9.1"
    );
    let event = state
        .events
        .iter()
        .find(|e| e.kind == "runtime.launch_target" && e.entity == operation.as_str())
        .context("retained target missing")?;
    let target: LaunchTarget = serde_json::from_value(event.payload.clone())?;
    let event = state
        .events
        .iter()
        .find(|e| e.kind == "runtime.launch_release" && e.entity == operation.as_str())
        .context("durable gate release missing")?;
    let release: LaunchReleaseIntent = serde_json::from_value(event.payload.clone())?;
    ensure!(
        target.version == 2
            && target.operation == *operation
            && target.attempt == record.attempt
            && release.version == 1
            && release.target == target
            && release.observed_unix_ms >= target.observed_unix_ms
            && release.observed_unix_ms <= now(),
        "gate release identity mismatch"
    );
    let identity = target
        .supervisor
        .as_ref()
        .context("retained supervisor missing")?;
    let supervisor = crate::worker_supervision::SupervisorObservation::reconnect(identity)?;
    executable(&profile.agent, deadline, &cancellation)?;
    let process =
        supervisor.agent_process(Path::new(&profile.agent.path), &profile.arguments_digest)?;
    let mut receipt = LaunchStartedReceipt {
        version: 2,
        attempt: target.attempt.clone(),
        operation: operation.clone(),
        route: target.route.clone(),
        terminal: target.terminal.clone(),
        session: target.session.clone(),
        agent: AgentIdentity {
            kind: profile.kind.clone(),
            name: worker_agent_name(&record.attempt),
        },
        supervisor: target.supervisor.clone(),
        observed_unix_ms: now(),
    };
    let native = Native {
        executable: &profile.herdr,
        start: &receipt,
        deadline,
        cancellation: cancellation.clone(),
        locks: guard.inherit()?,
    };
    let result = native.call(operation.as_str(), "agent.list", json!({}))?;
    ensure!(
        result["type"].as_str() == Some("agent_list"),
        "invalid native agent inventory"
    );
    let agents = result["agents"]
        .as_array()
        .context("agent inventory missing")?;
    ensure!(
        agents.len() <= 256,
        "agent inventory exceeds recovery limit"
    );
    let matching: Vec<_> = agents
        .iter()
        .filter(|a| a["pane_id"].as_str() == Some(&target.route.pane_id))
        .collect();
    ensure!(matching.len() == 1, "agent absent or ambiguous");
    let agent = matching[0];
    if allow_name && agent["name"].as_str() != Some(&receipt.agent.name) {
        ensure!(
            agent.get("name").is_none_or(Value::is_null),
            "refusing to replace an existing agent name"
        );
        let mut candidate = agent.clone();
        candidate["name"] = json!(receipt.agent.name);
        native.agent(&candidate, false)?;
        ensure!(
            agent
                .get("launch_pending")
                .is_none_or(|v| v.as_bool() == Some(false)),
            "native launch remains pending"
        );
        let claim = crate::operations::Claim {
            operation: operation.clone(),
            revision: delivery.revision,
            epoch: delivery.epoch,
            owner: delivery.owner.clone().context("claim owner missing")?,
            lease_until_ms: delivery.lease_until_ms.context("claim lease missing")?,
        };
        inventory::check(
            &project,
            &record.inputs.binding,
            &target.route,
            deadline,
            cancellation.clone(),
        )?;
        db.validate_claim(&claim, now())?;
        let rename_native = Native {
            deadline: deadline.min(
                Instant::now()
                    + Duration::from_millis(
                        claim.lease_until_ms.saturating_sub(now()).max(0) as u64
                    ),
            ),
            locks: guard.inherit()?,
            cancellation: cancellation.clone(),
            ..native
        };
        rename_native.call_checked(
            operation.as_str(),
            "agent.rename",
            json!({"target":target.route.pane_id,"name":receipt.agent.name}),
            || {
                worktrees.check()?;
                process.check()?;
                let current = rename_native.call(operation.as_str(), "agent.list", json!({}))?;
                ensure!(
                    current == result,
                    "native agent inventory changed before naming"
                );
                worktrees.check()?;
                process.check()?;
                executable(&profile.agent, deadline, &cancellation)?;
                db.record_launch_name(
                    &claim,
                    &PreparedLaunchName {
                        intent: LaunchReleaseIntent {
                            version: 1,
                            target: target.clone(),
                            observed_unix_ms: now(),
                        },
                    },
                    now(),
                )?;
                Ok(())
            },
        )?;
        // The mutation reply alone cannot certify start. Re-read native identity.
        let result = native.call(operation.as_str(), "agent.list", json!({}))?;
        ensure!(
            result["type"].as_str() == Some("agent_list"),
            "invalid native agent inventory"
        );
        let agents = result["agents"]
            .as_array()
            .context("agent inventory missing")?;
        ensure!(
            agents.len() <= 256,
            "agent inventory exceeds recovery limit"
        );
        let matches: Vec<_> = agents
            .iter()
            .filter(|a| a["pane_id"].as_str() == Some(&target.route.pane_id))
            .collect();
        ensure!(matches.len() == 1, "agent absent or ambiguous");
        native.agent(matches[0], false)?;
        ensure!(
            matches[0]
                .get("launch_pending")
                .is_none_or(|v| v.as_bool() == Some(false)),
            "native launch remains pending"
        );
    } else {
        native.agent(agent, false)?;
    }
    ensure!(
        matching[0]
            .get("launch_pending")
            .is_none_or(|v| v.as_bool() == Some(false)),
        "native launch remains pending"
    );
    process.check()?;
    inventory::check(
        &project,
        &record.inputs.binding,
        &target.route,
        deadline,
        cancellation.clone(),
    )?;
    native.fence()?;
    process.check()?;
    check(deadline, &cancellation)?;
    worktrees.check()?;
    receipt.observed_unix_ms = now();
    let head = db.read_snapshot(None)?.head;
    db.observe_launch_started(
        &PreparedLaunchStarted {
            receipt: receipt.clone(),
        },
        expected_revision,
        head,
        now(),
    )?;
    Ok(receipt)
}

/// Continue observation across target discovery and start confirmation. Each
/// service reacquires execution ownership and checks its original revision.
pub fn reconcile_launch(
    project: &Path,
    operation: &OperationId,
    expected_revision: u64,
    deadline: Instant,
    cancellation: Cancellation,
) -> Result<bool> {
    check(deadline, &cancellation)?;
    let state = crate::runtime::snapshot(project)?;
    if state.deliveries.iter().any(|d| {
        &d.operation == operation && d.state == crate::operations::DeliveryState::Confirmed
    }) {
        reconcile_start(
            project,
            operation,
            expected_revision,
            deadline,
            cancellation,
        )?;
        return Ok(true);
    }
    let target = super::reconcile_resource(
        project,
        operation,
        expected_revision,
        deadline,
        cancellation.clone(),
    )?;
    if target.is_none() {
        return Ok(false);
    }
    check(deadline, &cancellation)?;
    let state = crate::runtime::snapshot(project)?;
    if state
        .events
        .iter()
        .any(|e| e.kind == "runtime.launch_release" && e.entity == operation.as_str())
    {
        reconcile_start(
            project,
            operation,
            expected_revision,
            deadline,
            cancellation,
        )?;
    }
    Ok(true)
}

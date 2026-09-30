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
/// start. A lost acknowledgment is recovered by observation; recovery repeats
/// only a recorded rename that never took effect on the same unnamed agent.
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
    let guard = crate::execution_guard::RootGuard::exclusive_by(
        project.parent().context("project root missing")?,
        deadline,
        &cancellation,
    )?;
    let control = crate::store::controlled::ReadControl::new(deadline, cancellation.clone());
    let mut db = crate::migration::open_active_scoped(&project, control)?;
    let state = db.launch_start_selection(operation, expected_revision)?;
    let delivery = &state.delivery;
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
    let record = &state.record;
    let worktrees = crate::worktree_preparation::pin_started_events_held(
        &project,
        &state.events,
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
    // After release the gate still sets up the worker's filesystem view before
    // it execs the agent; wait (bounded) while that setup is observed, instead
    // of racing it. A gate still waiting for release fails at once as before.
    let settle = Instant::now() + Duration::from_secs(10);
    let process = loop {
        match supervisor.agent_process(Path::new(&profile.agent.path), &profile.arguments_digest) {
            Ok(process) => break process,
            Err(error) if Instant::now() >= settle || !supervisor.agent_setup_pending()? => return Err(error),
            Err(_) => {
                check(deadline, &cancellation)?;
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    };
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
    let agent = target_agent(&result, &target.route.pane_id, &receipt.agent.name)?;
    let unnamed = agent.get("name").is_none_or(Value::is_null);
    // A recorded naming intent lets recovery finish a rename that never took
    // effect. The same name on the same exact agent is idempotent.
    let intent = match state.events.iter().find(|e| {
        e.kind == "runtime.launch_name" && e.entity == operation.as_str()
    }) {
        Some(event) => {
            let intent: LaunchReleaseIntent = serde_json::from_value(event.payload.clone())?;
            ensure!(
                intent.version == 1
                    && intent.target == target
                    && intent.observed_unix_ms >= release.observed_unix_ms,
                "naming intent identity mismatch"
            );
            true
        }
        None => false,
    };
    let rename = allow_name && agent["name"].as_str() != Some(&receipt.agent.name);
    // The claim expired before any naming intent, so no rename was ever sent
    // and nothing may send one now. Retrying cannot help: hand it to an operator.
    if !allow_name
        && !intent
        && agent["name"].as_str() != Some(&receipt.agent.name)
        && delivery.state == crate::operations::DeliveryState::Ambiguous
    {
        let diagnostic = format!(
            "start_unnamed: the launch claim expired before a naming intent was recorded, so worker pane {} was never named and recovery has no authority to name it; the worker keeps running and holding capacity; reconcile it with `hp task <slug> cancel-attempt {}`",
            target.route.pane_id,
            record.attempt.as_str()
        );
        db.block_unnamed_start(operation, delivery.revision, &diagnostic, now())?;
        anyhow::bail!("{diagnostic}");
    }
    if rename || (intent && unnamed) {
        ensure!(unnamed, "refusing to replace an existing agent name");
        let mut candidate = agent.clone();
        candidate["name"] = json!(receipt.agent.name);
        native.agent(&candidate, false)?;
        ensure!(
            agent
                .get("launch_pending")
                .is_none_or(|v| v.as_bool() == Some(false)),
            "native launch remains pending"
        );
        let claim = if rename {
            Some(crate::operations::Claim {
                operation: operation.clone(),
                revision: delivery.revision,
                epoch: delivery.epoch,
                owner: delivery.owner.clone().context("claim owner missing")?,
                lease_until_ms: delivery.lease_until_ms.context("claim lease missing")?,
            })
        } else {
            None
        };
        inventory::check(
            &project,
            &record.inputs.binding,
            &target.route,
            deadline,
            cancellation.clone(),
        )?;
        let mut rename_deadline = deadline;
        if let Some(claim) = &claim {
            db.validate_launch_claim(claim, now())?;
            rename_deadline = deadline.min(
                Instant::now()
                    + Duration::from_millis(
                        claim.lease_until_ms.saturating_sub(now()).max(0) as u64
                    ),
            );
        }
        let rename_native = Native {
            deadline: rename_deadline,
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
                // Only the target's own entry is fenced; other agents on the
                // same server may change status meanwhile, and the target's
                // own display state moves while the agent starts.
                let current = rename_native.call(operation.as_str(), "agent.list", json!({}))?;
                let now_agent = target_agent(&current, &target.route.pane_id, &receipt.agent.name)?;
                ensure!(
                    same_agent(&agent, &now_agent),
                    "native agent changed before naming ({})",
                    entry_change(&agent, &now_agent)
                );
                worktrees.check()?;
                process.check()?;
                executable(&profile.agent, deadline, &cancellation)?;
                if let Some(claim) = &claim {
                    db.record_launch_name(
                        claim,
                        &PreparedLaunchName {
                            intent: LaunchReleaseIntent {
                                version: 1,
                                target: target.clone(),
                                observed_unix_ms: now(),
                            },
                        },
                        now(),
                    )?;
                }
                Ok(())
            },
        )?;
        // The mutation reply alone cannot certify start. Re-read native identity.
        let result = native.call(operation.as_str(), "agent.list", json!({}))?;
        let named = target_agent(&result, &target.route.pane_id, &receipt.agent.name)?;
        native.agent(&named, false)?;
        ensure!(
            named
                .get("launch_pending")
                .is_none_or(|v| v.as_bool() == Some(false)),
            "native launch remains pending"
        );
    } else {
        native.agent(&agent, false)?;
    }
    ensure!(
        agent
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
    let head = db.current_head()?;
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

/// Fields of the target's own entry that an agent changes while it starts:
/// its terminal title (Codex animates a spinner there), detected status, focus
/// and the server's change counters. Every other field, including any Herdr adds
/// later, is identity and must stay exactly equal.
const VOLATILE_AGENT_FIELDS: [&str; 6] = [
    "agent_status",
    "terminal_title",
    "terminal_title_stripped",
    "focused",
    "state_change_seq",
    "revision",
];

/// The same live agent: identity fields are equal, and change counters only
/// move forward (a reset means a different incarnation).
fn same_agent(before: &Value, after: &Value) -> bool {
    let (Some(b), Some(a)) = (before.as_object(), after.as_object()) else {
        return false;
    };
    let identity = |o: &serde_json::Map<String, Value>| {
        o.iter()
            .filter(|(k, _)| !VOLATILE_AGENT_FIELDS.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<serde_json::Map<_, _>>()
    };
    let forward = |key: &str| match (b.get(key), a.get(key)) {
        (None, None) => true,
        (Some(old), Some(new)) => old
            .as_u64()
            .zip(new.as_u64())
            .is_some_and(|(old, new)| new >= old),
        _ => false,
    };
    identity(b) == identity(a) && forward("state_change_seq") && forward("revision")
}

/// Name the fields that differ (bounded); values only for small state flags.
fn entry_change(before: &Value, after: &Value) -> String {
    let empty = serde_json::Map::new();
    let (b, a) = (
        before.as_object().unwrap_or(&empty),
        after.as_object().unwrap_or(&empty),
    );
    let mut keys: Vec<&String> = b.keys().chain(a.keys()).collect();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .filter(|k| b.get(*k) != a.get(*k))
        .take(16)
        .map(|k| {
            let show = |v: Option<&Value>| match v {
                Some(v @ (Value::Bool(_) | Value::Null | Value::Number(_))) => v.to_string(),
                Some(Value::String(s))
                    if s.len() <= 16
                        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') =>
                {
                    s.clone()
                }
                Some(_) => "<value>".into(),
                None => "<absent>".into(),
            };
            format!(
                "{}: {} -> {}",
                k.chars().take(40).collect::<String>(),
                show(b.get(k)),
                show(a.get(k))
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Select the target pane's single agent entry. Other agents on the same server
/// may change freely, but none may hold the target's deterministic name.
fn target_agent(result: &Value, pane: &str, name: &str) -> Result<Value> {
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
    let (matching, others): (Vec<_>, Vec<_>) = agents
        .iter()
        .partition(|a| a["pane_id"].as_str() == Some(pane));
    ensure!(matching.len() == 1, "agent absent or ambiguous");
    ensure!(
        others.iter().all(|a| a["name"].as_str() != Some(name)),
        "worker name held by another agent"
    );
    Ok(matching[0].clone())
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
    let select = || -> Result<_> {
        let control = crate::store::controlled::ReadControl::new(deadline, cancellation.clone());
        let mut db = crate::migration::open_active_scoped(project, control)?;
        Ok(db.launch_advancement_selection(operation, expected_revision)?)
    };
    let state = select()?;
    if state.delivery.state == crate::operations::DeliveryState::Confirmed {
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
    let state = select()?;
    if state.kinds.contains("runtime.launch_release") {
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

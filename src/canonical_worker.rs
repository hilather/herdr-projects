//! Concrete local worker effects. Public input selects a durable operation, not
//! executable arguments, a prompt, caller-supplied evidence or a runtime route.
use crate::{
    domain::*,
    runner::{Cancellation, Cmd, InheritedLock},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    path::Path,
    time::{Duration, Instant},
};

pub(crate) mod inventory;
#[cfg(target_os = "linux")]
mod launch;
#[cfg(target_os = "linux")]
pub use launch::advance_launch;
mod resources;
pub(crate) use resources::validate_server_creation;
#[cfg(target_os = "linux")]
mod start;
#[cfg(target_os = "linux")]
pub use resources::{continue_workspace_layout, create_resource, reconcile_resource, release_gate};
#[cfg(target_os = "linux")]
pub use start::{name_started_agent, reconcile_launch, reconcile_start};
#[cfg(all(test, target_os = "linux"))]
mod tests;

pub(crate) fn now() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

#[cfg(target_os = "linux")]
pub(crate) fn validate_preparation_inputs(profile:&FrozenProfile,project:&Path,prompt_chars:u64,deadline:Instant,cancellation:&Cancellation)->Result<()> {
    profile.validate_for_launch().map_err(anyhow::Error::msg)?;
    crate::profile_config::frozen_definition(profile)?.validate_gated_preparation(prompt_chars)?;
    resources::validate_execution_home(profile,project)?;
    executable(&profile.agent,deadline,cancellation)?;
    executable(&profile.herdr,deadline,cancellation)
}

pub(crate) fn session_identity(path: &Path) -> Result<ResourceIdentity> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    ensure!(
        path.is_absolute() && path.canonicalize()? == path,
        "worker session requires a canonical absolute socket"
    );
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.file_type().is_socket(),
        "worker endpoint is not a socket"
    );
    let born = metadata.created()?.duration_since(std::time::UNIX_EPOCH)?;
    Ok(ResourceIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        born_secs: born.as_secs(),
        born_nanos: born.subsec_nanos(),
    })
}

fn check(deadline: Instant, cancellation: &Cancellation) -> Result<()> {
    ensure!(
        !cancellation.is_cancelled() && Instant::now() < deadline,
        "worker effect cancelled or expired"
    );
    Ok(())
}

fn executable(
    identity: &ExecutableIdentity,
    deadline: Instant,
    cancellation: &Cancellation,
) -> Result<()> {
    use std::{
        io::Read,
        os::unix::fs::{MetadataExt, OpenOptionsExt},
    };
    check(deadline, cancellation)?;
    let path = Path::new(&identity.path);
    ensure!(
        path.is_absolute() && path.canonicalize()? == path,
        "frozen executable path changed"
    );
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let before = file.metadata()?;
    ensure!(
        before.is_file() && before.mode() & 0o111 != 0 && before.len() <= 512 * 1024 * 1024,
        "invalid frozen executable"
    );
    let mut hash = Sha256::new();
    let mut count = 0u64;
    let mut bytes = [0u8; 65536];
    loop {
        check(deadline, cancellation)?;
        let n = file.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        count += n as u64;
        ensure!(count <= 512 * 1024 * 1024, "executable exceeds read budget");
        hash.update(&bytes[..n]);
    }
    let after = file.metadata()?;
    let named = std::fs::symlink_metadata(path)?;
    ensure!(
        count == before.len()
            && before.len() == after.len()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec()
            && named.is_file()
            && (before.dev(), before.ino()) == (named.dev(), named.ino())
            && format!("{:x}", hash.finalize()) == identity.digest,
        "frozen executable changed"
    );
    Ok(())
}

struct Native<'a> {
    executable: &'a ExecutableIdentity,
    start: &'a LaunchStartedReceipt,
    deadline: Instant,
    cancellation: Cancellation,
    locks: Vec<InheritedLock>,
}
impl Native<'_> {
    fn fence(&self) -> Result<()> {
        check(self.deadline, &self.cancellation)?;
        ensure!(
            self.start.version == 2,
            "native worker effects require a retained supervisor incarnation"
        );
        let identity = self
            .start
            .supervisor
            .as_ref()
            .context("worker supervisor identity missing")?;
        #[cfg(target_os = "linux")]
        {
            crate::worker_supervision::SupervisorObservation::reconnect(identity)?;
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = identity;
            anyhow::bail!("native worker effects require Linux supervisor observation");
        }
        ensure!(
            session_identity(Path::new(&self.start.route.socket))? == self.start.session,
            "worker session incarnation changed"
        );
        Ok(())
    }
    fn call(&self, id: &str, method: &str, params: Value) -> Result<Value> {
        self.call_checked(id, method, params, || Ok(()))
    }
    fn call_checked(
        &self,
        id: &str,
        method: &str,
        params: Value,
        preflight: impl FnOnce() -> Result<()>,
    ) -> Result<Value> {
        self.fence()?;
        executable(self.executable, self.deadline, &self.cancellation)?;
        let mut cmd = Cmd::new(&self.executable.path, Duration::from_secs(20))
            .arg("remote-api-bridge")
            .env("HERDR_SOCKET_PATH", &self.start.route.socket)
            .env_remove("HERDR_SESSION")
            .stdin(
                serde_json::to_string(&json!({"id":id,"method":method,"params":params}))? + "\n",
            );
        cmd.capture_limit = 1024 * 1024;
        cmd.env_clear = true;
        cmd.env.push(("PATH".into(), "/usr/bin:/bin".into()));
        // Hashing and serialization may outlast a lease, approval or memory TTL.
        // Recheck live identity and authority after preparing the actual command.
        self.fence()?;
        preflight()?;
        check(self.deadline, &self.cancellation)?;
        let output =
            crate::supervision::run(cmd, self.deadline, self.cancellation.clone(), &self.locks)?;
        self.fence()?;
        ensure!(
            output.success(),
            "native worker request failed; details withheld"
        );
        let reply: Value = serde_json::from_slice(&output.stdout_bytes)
            .map_err(|_| anyhow::anyhow!("invalid native worker reply"))?;
        ensure!(
            reply["id"].as_str() == Some(id) && reply.get("error").is_none(),
            "native worker reply mismatch or rejection"
        );
        reply
            .get("result")
            .cloned()
            .context("native worker reply lacks result")
    }
    fn agent(&self, value: &Value, ready: bool) -> Result<()> {
        validate_native_agent(
            value,
            &self.start.route,
            &self.start.terminal,
            &self.start.agent.kind,
            Some(&self.start.agent.name),
            ready,
        )
    }
    fn ready(&self, id: &str) -> Result<()> {
        let result = self.call(id, "agent.list", json!({}))?;
        ensure!(
            result["type"].as_str() == Some("agent_list"),
            "native agent inventory type mismatch"
        );
        let agents = result["agents"]
            .as_array()
            .context("native worker inventory missing")?;
        ensure!(
            agents.len() <= 256,
            "native readiness inventory exceeds bounds"
        );
        let matching = agents
            .iter()
            .filter(|a| a["pane_id"].as_str() == Some(&self.start.route.pane_id))
            .collect::<Vec<_>>();
        ensure!(matching.len() == 1, "worker agent absent or ambiguous");
        self.agent(matching[0], true)?;
        // interactive_ready is a managed-launch flag, not a readiness signal
        // for our direct-executable layout. Require a positive bundled detector
        // match; the native default idle state alone is deliberately insufficient.
        let explain = self.call(
            id,
            "agent.explain",
            json!({"target":self.start.route.pane_id}),
        )?;
        ensure!(
            explain["type"].as_str() == Some("agent_explain"),
            "invalid native readiness response"
        );
        validate_visible_readiness(&self.start.agent.kind, &explain["explain"])?;
        let current = self.call(id, "agent.list", json!({}))?;
        ensure!(
            current["type"].as_str() == Some("agent_list"),
            "invalid native readiness inventory"
        );
        let agents = current["agents"]
            .as_array()
            .context("native agent inventory missing")?;
        ensure!(
            agents.len() <= 256,
            "native readiness inventory exceeds bounds"
        );
        let matching: Vec<_> = agents
            .iter()
            .filter(|a| a["pane_id"].as_str() == Some(&self.start.route.pane_id))
            .collect();
        ensure!(matching.len() == 1, "worker agent absent or ambiguous");
        self.agent(matching[0], true)
    }
}

pub(crate) fn validate_native_agent(
    value: &Value,
    route: &RuntimeRoute,
    terminal: &str,
    kind: &str,
    name: Option<&str>,
    ready: bool,
) -> Result<()> {
    for (field, expected) in [
        ("pane_id", route.pane_id.as_str()),
        ("workspace_id", route.workspace_id.as_str()),
        ("tab_id", route.tab_id.as_str()),
        ("cwd", route.cwd.as_str()),
        ("terminal_id", terminal),
        ("agent", kind),
    ] {
        ensure!(
            value[field].as_str() == Some(expected),
            "native worker identity mismatch for {field}"
        );
    }
    if let Some(name) = name {
        ensure!(
            value["name"].as_str() == Some(name),
            "native worker name mismatch"
        );
    }
    if ready {
        ensure!(
            value["agent_status"].as_str() == Some("idle")
                && value
                    .get("launch_pending")
                    .is_none_or(|v| v.as_bool() == Some(false)),
            "worker is not ready for its brief"
        );
    }
    Ok(())
}

pub(crate) fn validate_visible_readiness(kind: &str, explain: &Value) -> Result<()> {
    ensure!(
        explain["agent"].as_str() == Some(kind)
            && explain["state"].as_str() == Some("idle")
            && explain["manifest_source"].as_str() == Some("bundled")
            && explain["manifest_version"]
                .as_str()
                .is_some_and(|v| !v.is_empty() && v.len() <= 96)
            && explain["matched_rule"]["state"].as_str() == Some("idle")
            && explain["matched_rule"]["id"]
                .as_str()
                .is_some_and(|v| !v.is_empty() && v.len() <= 256)
            && explain["visible_idle"].as_bool() == Some(true)
            && [
                "visible_blocker",
                "visible_working",
                "screen_detection_skipped",
                "skip_state_update",
                "local_override_shadowing_remote"
            ]
            .iter()
            .all(|key| explain[*key].as_bool() == Some(false))
            && explain.get("fallback_reason").is_none_or(Value::is_null)
            && explain.get("warning").is_none_or(Value::is_null),
        "worker lacks verified visible prompt readiness"
    );
    Ok(())
}

/// Dispatch an already prepared initial brief once. A claimed operation is
/// retained on any failure; expiry records ambiguity and never retries it.
/// Queue workers pass their original deadline and cancellation identity here.
pub fn deliver_brief(
    project: &Path,
    operation: &OperationId,
    expected_revision: u64,
    deadline: Instant,
    cancellation: Cancellation,
) -> Result<crate::operations::Delivery> {
    let deadline = deadline.min(Instant::now() + Duration::from_secs(45));
    check(deadline, &cancellation)?;
    let project = project.canonicalize()?;
    let guard = crate::execution_guard::RootGuard::exclusive(
        project.parent().context("project root missing")?,
    )?;
    let mut db = crate::migration::open_active(&project)?;
    let state = db.read_snapshot(None)?;
    ensure!(
        state.deliveries.iter().any(|d| &d.operation == operation
            && d.revision == expected_revision
            && d.state == crate::operations::DeliveryState::Pending
            && d.attempts == 0
            && d.epoch == 0),
        "brief delivery is stale or was already claimed"
    );
    let op = state
        .operations
        .iter()
        .find(|o| &o.id == operation)
        .context("brief operation missing")?;
    ensure!(
        op.kind == "runtime.worker_brief" && op.payload_version == 1,
        "not a canonical worker brief"
    );
    let intent: WorkerBriefIntent = serde_json::from_value(op.payload.clone())?;
    let input = state
        .attempt_inputs
        .iter()
        .find(|r| r.attempt == intent.attempt && r.operation == intent.launch)
        .context("brief launch inputs missing")?;
    let profile = input
        .inputs
        .effective_profile
        .as_ref()
        .context("brief frozen profile missing")?;
    profile.validate_for_launch().map_err(anyhow::Error::msg)?;
    ensure!(
        profile.herdr.version == "0.9.1",
        "native brief adapter requires Herdr 0.9.1"
    );
    let event = state
        .events
        .iter()
        .find(|e| e.kind == "runtime.launch_started" && e.entity == intent.launch.as_str())
        .context("typed launch receipt missing")?;
    let start: LaunchStartedReceipt = serde_json::from_value(event.payload.clone())?;
    ensure!(
        start.route.machine.is_empty(),
        "native brief adapter supports local workers only"
    );
    inventory::check(
        &project,
        &intent.binding,
        &start.route,
        deadline,
        cancellation.clone(),
    )?;
    let brief =
        crate::memory::render_attempt_brief_held(&project, intent.attempt.as_str(), &mut db)?;
    ensure!(
        brief.prompt_digest == intent.prompt_digest
            && brief.prompt_chars == intent.prompt_chars
            && intent.knowledge.as_ref().map(|r| r.id.as_str()) == Some(brief.snapshot_id.as_str()),
        "retained brief differs from prepared delivery"
    );
    executable(&profile.agent, deadline, &cancellation)?;
    let mut native = Native {
        executable: &profile.herdr,
        start: &start,
        deadline,
        cancellation: cancellation.clone(),
        locks: guard.inherit()?,
    };
    native.ready(&format!("{}:ready", operation.as_str()))?;
    // Readiness failures precede the claim. Any subsequent preflight failure
    // retains this one-use claim rather than reopening prompt submission.
    let claim = db.claim_operation(
        operation,
        expected_revision,
        "canonical-brief-adapter",
        now(),
        30_000,
    )?;
    native.deadline = native.deadline.min(
        Instant::now()
            + Duration::from_millis(claim.lease_until_ms.saturating_sub(now()).max(0) as u64),
    );
    native.fence()?;
    let result = native.call_checked(
        operation.as_str(),
        "agent.prompt",
        json!({"target":start.agent.name,"text":brief.text}),
        || {
            native.ready(&format!("{}:preflight-ready", operation.as_str()))?;
            executable(&profile.agent, deadline, &cancellation)?;
            Ok(db.validate_claim(&claim, now())?)
        },
    )?;
    ensure!(
        result["type"].as_str() == Some("agent_prompted"),
        "native brief acknowledgment type mismatch"
    );
    native.agent(&result["agent"], false)?;
    let receipt = PreparedWorkerBriefReceipt {
        receipt: WorkerBriefReceipt {
            version: 1,
            operation: operation.clone(),
            intent,
            session: start.session.clone(),
            terminal: start.terminal.clone(),
            agent: start.agent.clone(),
            observed_unix_ms: now(),
        },
    };
    Ok(db.record_worker_brief_delivered(&claim, &receipt, now())?)
}

/// Fulfill desired cancellation, or reconcile a naturally exited worker. Stop
/// remains available while paused/revoked because it narrows existing execution.
/// Unknown or inaccessible process identity retains capacity. Files, worktrees,
/// panes and runtime ownership remain in place for explicit preservation/review.
#[cfg(target_os = "linux")]
pub fn reconcile_termination(
    project: &Path,
    attempt: &AttemptId,
    expected_revision: u64,
    deadline: Instant,
    cancellation: Cancellation,
) -> Result<Option<Attempt>> {
    use crate::worker_supervision::SupervisorObservation;
    let deadline = deadline.min(Instant::now() + Duration::from_secs(45));
    let stop_deadline=deadline.min(Instant::now()+Duration::from_secs(10));
    check(deadline, &cancellation)?;
    let project = project.canonicalize()?;
    let _guard = crate::execution_guard::RootGuard::exclusive(
        project.parent().context("project root missing")?,
    )?;
    let mut db = crate::migration::open_active(&project)?;
    let state = db.read_snapshot(None)?;
    let current = state
        .attempts
        .iter()
        .find(|a| &a.id == attempt)
        .context("worker attempt missing")?;
    ensure!(
        current.revision == expected_revision,
        "worker attempt revision changed"
    );
    if current.termination_observed {
        return Ok(Some(current.clone()));
    }
    let record = state
        .attempt_inputs
        .iter()
        .find(|r| &r.attempt == attempt)
        .context("canonical attempt inputs missing")?;
    if state.events.iter().any(|e|e.kind=="runtime.worktrees_creation" && e.entity==record.operation.as_str())
        && !state.events.iter().any(|e|e.kind.starts_with("runtime.launch_") && e.entity==record.operation.as_str()) {
        let mut budget=crate::store::identity_inventory::Budget::new(50*1024*1024,1024,deadline,cancellation.clone())?;
        crate::migration::read_worktree_inventory(&project,&mut budget)?;
        check(deadline,&cancellation)?;
        return Ok(db.stop_worktree_preparation(attempt,expected_revision,state.head,&_guard,|record,intent| {
            crate::worktree_preservation::capture_preparation_held(&project,&state,record,intent,&_guard,&crate::source_tree::Control{deadline,cancellation:cancellation.clone()})
                .map_err(|error|crate::store::StoreError::Io(format!("preparation preservation failed: {error:#}")))
        },now())?);
    }
    if !state
        .events
        .iter()
        .any(|e| e.kind == "runtime.launch_started" && e.entity == record.operation.as_str())
    {
        let event = state
            .events
            .iter()
            .find(|e| e.kind == "runtime.launch_target" && e.entity == record.operation.as_str())
            .context("staged worker target missing")?;
        let target: LaunchTarget = serde_json::from_value(event.payload.clone())?;
        ensure!(
            target.version == 2
                && target.attempt == *attempt
                && target.operation == record.operation,
            "staged worker target mismatch"
        );
        let delivery = state
            .deliveries
            .iter()
            .find(|d| d.operation == record.operation)
            .context("staged launch delivery missing")?;
        let binding = state
            .runtime_bindings
            .iter()
            .find(|b| b.id == record.inputs.binding)
            .context("staged binding missing")?;
        ensure!(
            current.state == AttemptState::Reserved
                && current.retains_capacity()
                && delivery.attempts == 1
                && delivery.state != crate::operations::DeliveryState::Confirmed
                && binding.revision == record.inputs.binding_revision
                && crate::store::ownership::identity_digest(binding)?
                    == record.inputs.binding_digest
                && state
                    .tasks
                    .iter()
                    .any(|t| t.id == current.task && t.active_attempt.as_ref() == Some(attempt))
                && !state.ownership.iter().any(|o| o.binding == binding.id)
                && state.approvals.iter().any(|g| g
                    .consumed
                    .as_ref()
                    .is_some_and(|u| u.operation == record.operation)),
            "staged launch no longer owns the original reservation"
        );
        let identity = target
            .supervisor
            .as_ref()
            .context("staged worker supervisor missing")?;
        if !SupervisorObservation::recover_exited(identity)? {
            if !state.cancellations.iter().any(|c| &c.attempt == attempt) {
                return Ok(None);
            }
            SupervisorObservation::stop_recorded(identity, stop_deadline, &cancellation)?;
        }
        ensure!(
            SupervisorObservation::recover_exited(identity)?,
            "staged worker termination unresolved"
        );
        let host_reboot=SupervisorObservation::observe_reboot(identity)?;
        db.validate_workspace_quiescence(&record.operation,identity,host_reboot.as_ref())?;
        let repository_snapshots=preserve_terminated_repositories(&project,&state,record,&_guard,deadline,cancellation.clone())?;
        let output_snapshot=Some(crate::worktree_preservation::capture_outputs_held(&project,record,&crate::source_tree::Control{deadline,cancellation:cancellation.clone()})?);
        check(deadline,&cancellation)?;
        return Ok(Some(db.record_launch_stopped(
            &PreparedLaunchStopped {
                receipt: LaunchStoppedReceipt {
                    version: 1,
                    host_reboot,
                    repository_snapshots,
                    output_snapshot,
                    target,
                    observed_unix_ms: now(),
                },
            },
            expected_revision,
            state.head,
            now(),
        )?));
    }
    let owned = state
        .ownership
        .iter()
        .find(|o| o.binding == record.inputs.binding)
        .context("worker ownership missing")?;
    ensure!(
        owned.origin == "launched" && owned.attempt.as_ref() == Some(attempt),
        "stop cannot adopt another worker"
    );
    let binding = state
        .runtime_bindings
        .iter()
        .find(|b| b.id == owned.binding)
        .context("worker runtime binding missing")?;
    ensure!(
        binding.revision == owned.binding_revision
            && crate::store::ownership::identity_digest(binding)? == owned.identity_digest,
        "worker ownership changed"
    );
    let event = state
        .events
        .iter()
        .find(|e| e.kind == "runtime.launch_started" && e.entity == record.operation.as_str())
        .context("worker launch receipt missing")?;
    let start: LaunchStartedReceipt = serde_json::from_value(event.payload.clone())?;
    ensure!(
        start.version == 2 && &start.attempt == attempt && start.operation == record.operation,
        "worker lacks exact supervisor evidence"
    );
    let launch = state
        .deliveries
        .iter()
        .find(|d| d.operation == record.operation)
        .context("worker launch delivery missing")?;
    ensure!(
        launch.state == crate::operations::DeliveryState::Confirmed
            && launch.last_outcome
                == Some(crate::operations::Outcome::Confirmed {
                    observed_identity: serde_json::to_string(&start)?
                }),
        "worker supervisor evidence differs from confirmed launch"
    );
    let identity = start
        .supervisor
        .context("worker supervisor identity missing")?;
    let cancelled = state.cancellations.iter().any(|c| &c.attempt == attempt);
    if !SupervisorObservation::recover_exited(&identity)? {
        if !cancelled {
            return Ok(None);
        }
        check(deadline, &cancellation)?;
        SupervisorObservation::stop_recorded(&identity, stop_deadline, &cancellation)?;
    }
    ensure!(
        SupervisorObservation::recover_exited(&identity)?,
        "worker termination is still unresolved"
    );
    // Do not require the old control epoch or grant to be renewed to acknowledge
    // an existing stop. The atomic store service checks exact retained ownership.
    let host_reboot=SupervisorObservation::observe_reboot(&identity)?;
    db.validate_workspace_quiescence(&record.operation,&identity,host_reboot.as_ref())?;
    let repository_snapshots=preserve_terminated_repositories(&project,&state,record,&_guard,deadline,cancellation.clone())?;
    let output_snapshot=Some(crate::worktree_preservation::capture_outputs_held(&project,record,&crate::source_tree::Control{deadline,cancellation:cancellation.clone()})?);
    let prepared = PreparedWorkerTermination {
        receipt: WorkerTerminationReceipt {
            version: 1,
            attempt: attempt.clone(),
            launch: record.operation.clone(),
            binding: binding.id.clone(),
            binding_revision: binding.revision,
            ownership_revision: owned.revision,
            host_reboot,
            repository_snapshots,
            output_snapshot,
            supervisor: identity,
            retained_resources: binding.identity.clone(),
            cause: if cancelled {
                WorkerTerminationCause::Cancellation
            } else {
                WorkerTerminationCause::ProcessExit
            },
            observed_unix_ms: now(),
        },
    };
    check(deadline,&cancellation)?;
    Ok(Some(db.record_worker_termination(
        &prepared,
        expected_revision,
        state.head,
        now(),
    )?))
}

#[cfg(target_os="linux")]
fn preserve_terminated_repositories(project:&Path,state:&Snapshot,record:&AttemptInputRecord,guard:&crate::execution_guard::RootGuard,deadline:Instant,cancellation:Cancellation)->Result<Vec<WorktreeSnapshotReference>> {
    let snapshots=crate::worktree_preservation::capture_held(project,state,record,guard,&crate::source_tree::Control{deadline,cancellation},true)?;
    Ok(snapshots.into_iter().map(|s|WorktreeSnapshotReference{plan:s.manifest.worktree.plan,digest:s.digest}).collect())
}

/// Recover the durable start-to-brief handoff without sending any terminal input.
/// The head check fences changes between observation and retained-memory rendering.
pub fn prepare_brief(
    project: &Path,
    attempt: &AttemptId,
    expected_revision: u64,
    deadline: Instant,
    cancellation: Cancellation,
) -> Result<Operation> {
    check(deadline, &cancellation)?;
    let state = crate::runtime::snapshot(project)?;
    let current = state
        .attempts
        .iter()
        .find(|a| &a.id == attempt)
        .context("worker attempt missing")?;
    ensure!(
        current.revision == expected_revision
            && current.state == AttemptState::Launching
            && current.retains_capacity(),
        "worker attempt changed before brief preparation"
    );
    let record = state
        .attempt_inputs
        .iter()
        .find(|r| &r.attempt == attempt)
        .context("sealed worker inputs missing")?;
    let event = state
        .events
        .iter()
        .find(|e| e.kind == "runtime.launch_started" && e.entity == record.operation.as_str())
        .context("worker start receipt missing")?;
    let start: LaunchStartedReceipt = serde_json::from_value(event.payload.clone())?;
    ensure!(
        start.version == 2
            && start.attempt == *attempt
            && start.operation == record.operation
            && start.supervisor.is_some(),
        "worker lacks supervised start evidence"
    );
    crate::memory::enqueue_attempt_brief_controlled(
        project,
        attempt.as_str(),
        state.head,
        deadline,
        cancellation,
    )
}

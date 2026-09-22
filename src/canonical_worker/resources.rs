//! Creation of a gated local terminal from already approved immutable inputs.
//! No agent input is released by this service. Failed/uncertain creation retains
//! the original claim; callers must reconcile its resources instead of replaying.
use super::*;
#[cfg(test)]
pub(super) use crate::profile_config::ProfileDefinition as Definition;

fn command(
    profile: &FrozenProfile,
    operation: &OperationId,
    prompt_chars: u64,
) -> Result<Vec<String>> {
    let definition = crate::profile_config::frozen_definition(profile)?;
    let wall = definition.validate_gated_preparation(prompt_chars)?;
    if let Some(home) = &profile.execution_home {
        return crate::worker_supervision::isolated_gated_command(
            Path::new(&profile.agent.path),
            &definition.extra_args,
            wall,
            &release_token(operation),
            Path::new(home),
        );
    }
    crate::worker_supervision::gated_command(
        Path::new(&profile.agent.path),
        &definition.extra_args,
        wall,
        &release_token(operation),
    )
}
fn release_token(operation: &OperationId) -> String {
    format!("release-{}", operation.as_str())
}

/// Only stable route/incarnation fields participate in the observation fence;
/// readiness, title and display revisions may legitimately change meanwhile.
#[derive(Debug, PartialEq, Eq, serde::Deserialize)]
struct PaneIdentity {
    pane_id: String,
    workspace_id: String,
    tab_id: String,
    terminal_id: String,
    cwd: Option<String>,
}
impl PaneIdentity {
    fn validate(&self, route: &RuntimeRoute, pane: &str) -> Result<()> {
        ensure!(
            self.pane_id == pane && self.workspace_id == route.workspace_id,
            "observed pane route mismatch"
        );
        for id in [
            &self.pane_id,
            &self.workspace_id,
            &self.tab_id,
            &self.terminal_id,
        ] {
            ensure!(
                !id.is_empty() && id.len() <= 512 && !id.chars().any(char::is_control),
                "invalid observed pane identity"
            );
        }
        Ok(())
    }
}

struct Api<'a> {
    executable: &'a ExecutableIdentity,
    socket: &'a str,
    session: &'a ResourceIdentity,
    deadline: Instant,
    cancellation: Cancellation,
    locks: Vec<InheritedLock>,
}
impl Api<'_> {
    fn require_workspace_command(&self, operation: &OperationId) -> Result<()> {
        let reply=self.call(operation.as_str(), "ping", json!({}), || Ok(()))?;
        ensure!(reply["type"].as_str()==Some("pong")
            && reply["version"].as_str()==Some(self.executable.version.as_str())
            && reply["capabilities"]["workspace_create_command"].as_bool()==Some(true),
            "connected Herdr server does not advertise the verified workspace.create_command contract");
        Ok(())
    }

    fn pane_identity(
        &self,
        operation: &OperationId,
        pane: &str,
        route: &RuntimeRoute,
    ) -> Result<PaneIdentity> {
        let result = self.call(
            operation.as_str(),
            "pane.get",
            json!({"pane_id":pane}),
            || Ok(()),
        )?;
        let identity: PaneIdentity = serde_json::from_value(result["pane"].clone())
            .map_err(|_| anyhow::anyhow!("invalid native pane identity"))?;
        identity.validate(route, pane)?;
        Ok(identity)
    }

    fn call(
        &self,
        id: &str,
        method: &str,
        params: Value,
        preflight: impl FnOnce() -> Result<()>,
    ) -> Result<Value> {
        check(self.deadline, &self.cancellation)?;
        executable(self.executable, self.deadline, &self.cancellation)?;
        ensure!(
            session_identity(Path::new(self.socket))? == *self.session,
            "worker session replaced during resource creation"
        );
        let mut cmd = Cmd::new(&self.executable.path, Duration::from_secs(15))
            .arg("remote-api-bridge")
            .env("HERDR_SOCKET_PATH", self.socket)
            .env("PATH", "/usr/bin:/bin")
            .stdin(
                serde_json::to_string(&json!({"id":id,"method":method,"params":params}))? + "\n",
            );
        cmd.env_clear = true;
        cmd.capture_limit = 1024 * 1024;
        preflight()?;
        check(self.deadline, &self.cancellation)?;
        let output =
            crate::supervision::run(cmd, self.deadline, self.cancellation.clone(), &self.locks)?;
        ensure!(
            session_identity(Path::new(self.socket))? == *self.session,
            "worker session changed during resource creation"
        );
        ensure!(
            output.success(),
            "native resource request failed; retain launch claim"
        );
        let value: Value = serde_json::from_slice(&output.stdout_bytes)
            .map_err(|_| anyhow::anyhow!("invalid native resource acknowledgment"))?;
        ensure!(
            value["id"].as_str() == Some(id) && value.get("error").is_none(),
            "native resource acknowledgment mismatch for {method}"
        );
        value
            .get("result")
            .cloned()
            .context("native resource acknowledgment missing")
    }
}

/// Create a directly supervised workspace root, or a gated tab in an existing
/// workspace. The operation must have never been claimed. This does not release
/// the gate or report agent start.
#[cfg(target_os = "linux")]
pub fn create_resource(
    project: &Path,
    operation: &OperationId,
    expected_revision: u64,
    deadline: Instant,
    cancellation: Cancellation,
) -> Result<LaunchTarget> {
    let deadline = deadline.min(Instant::now() + Duration::from_secs(45));
    check(deadline, &cancellation)?;
    let state=crate::runtime::snapshot(project)?;
    let record=state.attempt_inputs.iter().find(|r|&r.operation==operation).context("launch inputs missing")?;
    let expected_revision=if !record.inputs.repositories.is_empty() {
        let initial=state.deliveries.iter().find(|d|&d.operation==operation).context("launch delivery missing")?;
        ensure!(initial.revision==expected_revision,"launch revision changed");
        crate::worktree_preparation::prepare(project,operation,expected_revision,deadline,cancellation.clone())?;
        if initial.attempts==0 {expected_revision.checked_add(1).context("launch revision overflow")?}else{expected_revision}
    }else{expected_revision};
    create_resource_inner(
        project,
        operation,
        expected_revision,
        deadline,
        cancellation,
        false,
        false,
    )
}

/// Continue only an acknowledged workspace whose gated layout was never attempted.
#[cfg(target_os = "linux")]
pub fn continue_workspace_layout(
    project: &Path,
    operation: &OperationId,
    expected_revision: u64,
    deadline: Instant,
    cancellation: Cancellation,
) -> Result<LaunchTarget> {
    create_resource_inner(
        project,
        operation,
        expected_revision,
        deadline,
        cancellation,
        true,
        true,
    )
}

#[cfg(target_os = "linux")]
fn create_resource_inner(
    project: &Path,
    operation: &OperationId,
    expected_revision: u64,
    deadline: Instant,
    cancellation: Cancellation,
    continuing: bool,
    legacy_workspace: bool,
) -> Result<LaunchTarget> {
    ensure!(
        !legacy_workspace || continuing || cfg!(test),
        "legacy workspace creation is disabled"
    );
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
        .context("launch operation missing")?;
    let worktree_continuation=!continuing && delivery.state==crate::operations::DeliveryState::Claimed && delivery.attempts==1
        && state.events.iter().any(|e|e.entity==operation.as_str()&&e.kind=="runtime.worktrees_ready")
        && !state.events.iter().any(|e|e.entity==operation.as_str()&&matches!(e.kind.as_str(),"runtime.launch_creation"|"runtime.launch_target"|"runtime.launch_started"));
    ensure!(
        delivery.revision == expected_revision
            && if continuing {
                delivery.state == crate::operations::DeliveryState::Claimed
                    && delivery.attempts == 1
                    && state.events.iter().any(|e| {
                        e.entity == operation.as_str() && e.kind == "runtime.launch_workspace"
                    })
                    && !state.events.iter().any(|e| {
                        e.entity == operation.as_str()
                            && matches!(
                                e.kind.as_str(),
                                "runtime.launch_layout"
                                    | "runtime.launch_target"
                                    | "runtime.launch_started"
                            )
                    })
            } else {
                worktree_continuation || (delivery.state == crate::operations::DeliveryState::Pending
                    && delivery.attempts == 0
                    && delivery.epoch == 0)
            },
        "resource creation was already attempted or changed"
    );
    let record = state
        .attempt_inputs
        .iter()
        .find(|i| &i.operation == operation)
        .context("sealed launch inputs missing")?;
    let profile = record
        .inputs
        .effective_profile
        .as_ref()
        .context("effective launch profile missing")?;
    profile.validate_for_launch().map_err(anyhow::Error::msg)?;
    ensure!(
        profile.herdr.version == "0.9.1",
        "resource adapter requires Herdr 0.9.1"
    );
    let binding = state
        .runtime_bindings
        .iter()
        .find(|b| b.id == record.inputs.binding)
        .context("launch binding missing")?;
    let (route,_) = db.worktree_execution_route(record,binding)?;
    route.validate().map_err(anyhow::Error::msg)?;
    ensure!(
        binding.identity.worktree_path.is_empty()
            && route.machine.is_empty()
            && route.tab_id.is_empty()
            && route.pane_id.is_empty(),
        "resource creation requires local execution with no existing tab, pane or worktree binding"
    );
    ensure!(
        Path::new(&route.cwd).is_absolute()
            && Path::new(&route.cwd).canonicalize()? == Path::new(&route.cwd)
            && Path::new(&route.cwd).is_dir(),
        "worker working directory changed"
    );
    let worktrees=crate::worktree_preparation::verify_held(&project,&state,record,deadline,cancellation.clone(),guard.inherit()?)?;
    // Refuse unusable retained knowledge before creating any external resource.
    let brief =
        crate::memory::render_attempt_brief_held(&project, record.attempt.as_str(), &mut db)?;
    let argv = command(profile, operation, brief.prompt_chars)?;
    executable(&profile.agent, deadline, &cancellation)?;
    let session = session_identity(Path::new(&route.socket))?;
    let mut api = Api {
        executable: &profile.herdr,
        socket: &route.socket,
        session: &session,
        deadline,
        cancellation: cancellation.clone(),
        locks: guard.inherit()?,
    };
    let direct_root = route.workspace_id.is_empty() && !legacy_workspace;
    if direct_root { api.require_workspace_command(operation)?; }
    let creation = LaunchCreationIntent {
        version: if direct_root { 2 } else { 1 },
        operation: operation.clone(),
        attempt: record.attempt.clone(),
        route: route.clone(),
        session: session.clone(),
        command_digest: format!("{:x}", Sha256::digest(serde_json::to_vec(&argv)?)),
        workspace_token: if route.workspace_id.is_empty() && !direct_root {
            if continuing {
                serde_json::from_value::<LaunchCreationIntent>(
                    state
                        .events
                        .iter()
                        .find(|e| {
                            e.entity == operation.as_str() && e.kind == "runtime.launch_creation"
                        })
                        .context("creation intent missing")?
                        .payload
                        .clone(),
                )?
                .workspace_token
            } else {
                use std::io::Read;
                let mut bytes = [0u8; 32];
                std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
                Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
            }
        } else {
            None
        },
        usage_warning: Some(LaunchUsageWarning::ProviderUsageUnavailable),
    };
    if continuing {
        let previous: LaunchCreationIntent = serde_json::from_value(
            state
                .events
                .iter()
                .find(|e| e.entity == operation.as_str() && e.kind == "runtime.launch_creation")
                .context("creation intent missing")?
                .payload
                .clone(),
        )?;
        ensure!(
            previous == creation,
            "workspace continuation inputs changed"
        );
    }
    // The one-use claim, approval consumption and recovery intent commit together
    // before the only creation request. There is no retry after a lost reply,
    // even if the gate never received a release line.
    let claim = if continuing || worktree_continuation {
        let claim = crate::operations::Claim {
            operation: operation.clone(),
            revision: delivery.revision,
            epoch: delivery.epoch,
            owner: delivery
                .owner
                .clone()
                .context("launch claim owner missing")?,
            lease_until_ms: delivery
                .lease_until_ms
                .context("launch claim lease missing")?,
        };
        db.validate_claim(&claim, now())?;
        if worktree_continuation {db.continue_worktree_launch_creation(&claim,&PreparedLaunchCreation{intent:creation.clone()},now())?;}
        claim
    } else {
        db.claim_launch_creation(
            expected_revision,
            &PreparedLaunchCreation { intent: creation.clone() },
            now(),
            30_000,
        )?
    };
    api.deadline = api.deadline.min(
        Instant::now()
            + Duration::from_millis(claim.lease_until_ms.saturating_sub(now()).max(0) as u64),
    );
    if direct_root {
        let created = api.call(
            operation.as_str(),
            "workspace.create_command",
            json!({"cwd":route.cwd,"label":worker_agent_name(&record.attempt),
                "focus":false,"command":argv,"env":{}}),
            || {worktrees.check()?;Ok(db.validate_claim(&claim, now())?)},
        )?;
        ensure!(
            created["type"].as_str() == Some("workspace_created"),
            "invalid direct workspace response"
        );
        let workspace = created["workspace"]["workspace_id"]
            .as_str()
            .context("created workspace missing")?;
        let pane = created["root_pane"]["pane_id"]
            .as_str()
            .context("created root pane missing")?;
        let route = RuntimeRoute {
            workspace_id: workspace.into(),
            ..route.clone()
        };
        let (target, _observation) = loop {
            check(api.deadline, &cancellation)?;
            if let Some(found) = find_created_target(&api, &creation, &route)? {
                ensure!(
                    found.0.route.pane_id == pane,
                    "supervisor is not the acknowledged root pane"
                );
                break found;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        inventory::check(
            &project,
            &record.inputs.binding,
            &target.route,
            api.deadline,
            cancellation.clone(),
        )?;
        retain_observed_target(&mut db, &target)?;
        return Ok(target);
    }
    let workspace = if route.workspace_id.is_empty() {
        let workspace = if continuing {
            serde_json::from_value::<LaunchTarget>(
                state
                    .events
                    .iter()
                    .find(|e| {
                        e.entity == operation.as_str() && e.kind == "runtime.launch_workspace"
                    })
                    .context("workspace receipt missing")?
                    .payload
                    .clone(),
            )?
        } else {
            let created=api.call(operation.as_str(),"workspace.create",
                json!({"cwd":route.cwd,"label":worker_agent_name(&record.attempt),"focus":false,
                    "env":{"HP_WORKSPACE_CREATION":creation.workspace_token.as_ref().context("workspace creation marker missing")?}}),
                ||Ok(db.validate_claim(&claim,now())?))?;
            ensure!(
                created["type"].as_str() == Some("workspace_created"),
                "invalid workspace creation response"
            );
            let id = created["workspace"]["workspace_id"]
                .as_str()
                .context("created workspace missing")?;
            let pane = created["root_pane"]["pane_id"]
                .as_str()
                .context("workspace bootstrap pane missing")?;
            let selected = RuntimeRoute {
                workspace_id: id.into(),
                ..route.clone()
            };
            let native = api.pane_identity(operation, pane, &selected)?;
            ensure!(
                native.cwd.as_deref() == Some(route.cwd.as_str()),
                "workspace bootstrap directory differs"
            );
            let target = LaunchTarget {
                version: 1,
                attempt: record.attempt.clone(),
                operation: operation.clone(),
                route: RuntimeRoute {
                    pane_id: pane.into(),
                    tab_id: native.tab_id,
                    ..selected
                },
                terminal: native.terminal_id,
                session: session.clone(),
                supervisor: None,
                observed_unix_ms: now(),
            };
            db.record_launch_workspace(
                &claim,
                &PreparedLaunchWorkspace {
                    target: target.clone(),
                },
                now(),
            )?;
            target
        };
        ensure!(
            workspace.operation == *operation
                && workspace.attempt == record.attempt
                && workspace.session == session,
            "workspace continuation identity changed"
        );
        let bootstrap = api.pane_identity(operation, &workspace.route.pane_id, &workspace.route)?;
        ensure!(
            bootstrap.terminal_id == workspace.terminal
                && bootstrap.tab_id == workspace.route.tab_id
                && bootstrap.cwd.as_deref() == Some(route.cwd.as_str()),
            "workspace bootstrap identity changed"
        );
        Some(workspace)
    } else {
        ensure!(
            !continuing,
            "workspace continuation requires an owned workspace"
        );
        None
    };
    let route = RuntimeRoute {
        workspace_id: workspace.as_ref().map_or_else(
            || route.workspace_id.clone(),
            |w| w.route.workspace_id.clone(),
        ),
        ..route.clone()
    };
    let result = api.call(
        operation.as_str(),
        "layout.apply",
        json!({"workspace_id":route.workspace_id,
        "tab_label":worker_agent_name(&record.attempt),"focus":false,
        "root":{"type":"pane","cwd":route.cwd,"command":argv,"env":{}}}),
        || {
            db.validate_claim(&claim, now())?;
            if let Some(workspace) = &workspace {
                let bootstrap =
                    api.pane_identity(operation, &workspace.route.pane_id, &workspace.route)?;
                ensure!(
                    bootstrap.terminal_id == workspace.terminal
                        && bootstrap.tab_id == workspace.route.tab_id,
                    "workspace changed before layout submission"
                );
                db.record_launch_layout(
                    &claim,
                    &PreparedLaunchLayout {
                        workspace: workspace.clone(),
                    },
                    now(),
                )?;
            }
            Ok(())
        },
    )?;
    let layout = &result["layout"];
    ensure!(
        layout["workspace_id"].as_str() == Some(route.workspace_id.as_str()),
        "created workspace identity mismatch"
    );
    let pane = layout["focused_pane_id"]
        .as_str()
        .context("created pane identity missing")?;
    let tab = layout["tab_id"]
        .as_str()
        .context("created tab identity missing")?;
    let before = api.pane_identity(operation, pane, &route)?;
    ensure!(
        before.tab_id == tab && before.cwd.as_deref() == Some(route.cwd.as_str()),
        "created tab or working directory mismatch"
    );
    let observation = loop {
        check(api.deadline, &cancellation)?;
        let processes = api.call(
            &format!("{}:process", operation.as_str()),
            "pane.process_info",
            json!({"pane_id":pane}),
            || Ok(()),
        )?;
        let info = &processes["process_info"];
        ensure!(
            info["pane_id"].as_str() == Some(pane),
            "created process belongs to another pane"
        );
        let matches: Vec<_> = info["foreground_processes"]
            .as_array()
            .context("process inventory missing")?
            .iter()
            .filter(|p| p["argv"] == json!(argv))
            .collect();
        ensure!(matches.len() <= 1, "ambiguous gated supervisor");
        if let Some(process) = matches.first() {
            let pid = u32::try_from(process["pid"].as_u64().context("supervisor PID missing")?)?;
            if let Ok(observed) =
                crate::worker_supervision::SupervisorObservation::observe(pid, &argv)
            {
                break observed;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let observed_unix_ms = now();
    ensure!(
        api.pane_identity(operation, pane, &route)? == before,
        "created terminal changed during process observation"
    );
    let target = LaunchTarget {
        version: 2,
        attempt: record.attempt.clone(),
        operation: operation.clone(),
        route: RuntimeRoute {
            pane_id: pane.into(),
            tab_id: tab.into(),
            ..route.clone()
        },
        terminal: before.terminal_id,
        session: session.clone(),
        supervisor: Some(observation.identity().clone()),
        observed_unix_ms,
    };
    inventory::check(
        &project,
        &record.inputs.binding,
        &target.route,
        api.deadline,
        cancellation.clone(),
    )?;
    retain_observed_target(&mut db, &target)?;
    Ok(target)
}

/// Recover only an exact live resource from a durably prepared creation. Missing
/// resources retain uncertainty and capacity; this never creates or releases one.
#[cfg(target_os = "linux")]
pub fn reconcile_resource(
    project: &Path,
    operation: &OperationId,
    expected_revision: u64,
    deadline: Instant,
    cancellation: Cancellation,
) -> Result<Option<LaunchTarget>> {
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
        delivery.revision == expected_revision
            && delivery.attempts == 1
            && delivery.state != crate::operations::DeliveryState::Confirmed,
        "launch recovery no longer matches claim history"
    );
    let record = state
        .attempt_inputs
        .iter()
        .find(|i| &i.operation == operation)
        .context("launch inputs missing")?;
    ensure!(
        state.attempts.iter().any(|a| a.id == record.attempt
            && a.state == AttemptState::Reserved
            && a.retains_capacity()),
        "launch recovery no longer owns a reservation"
    );
    if let Some(event) = state
        .events
        .iter()
        .find(|e| e.kind == "runtime.launch_target" && e.entity == operation.as_str())
    {
        let target: LaunchTarget = serde_json::from_value(event.payload.clone())?;
        ensure!(
            target.operation == *operation && target.attempt == record.attempt,
            "recorded target mismatch"
        );
        return Ok(Some(target));
    }
    let event = state
        .events
        .iter()
        .find(|e| e.kind == "runtime.launch_creation" && e.entity == operation.as_str())
        .context("pre-effect creation identity missing; retain uncertainty")?;
    let intent: LaunchCreationIntent = serde_json::from_value(event.payload.clone())?;
    let binding = state
        .runtime_bindings
        .iter()
        .find(|b| b.id == record.inputs.binding)
        .context("launch binding missing")?;
    ensure!(
        matches!(intent.version, 1 | 2)
            && (intent.version != 2
                || (intent.route.workspace_id.is_empty() && intent.workspace_token.is_none()))
            && intent.operation == *operation
            && intent.attempt == record.attempt
            && intent.route == db.worktree_execution_route(record,binding)?.0
            && binding.revision == record.inputs.binding_revision
            && crate::store::ownership::identity_digest(binding)? == record.inputs.binding_digest
            && state.approvals.iter().any(|g| g
                .consumed
                .as_ref()
                .is_some_and(|u| u.operation == *operation)),
        "creation recovery provenance changed"
    );
    intent.route.validate().map_err(anyhow::Error::msg)?;
    ensure!(
        intent.route.machine.is_empty()
            && intent.route.pane_id.is_empty()
            && intent.route.tab_id.is_empty(),
        "unsupported creation recovery route"
    );
    let profile = record
        .inputs
        .effective_profile
        .as_ref()
        .context("launch profile missing")?;
    let api = Api {
        executable: &profile.herdr,
        socket: &intent.route.socket,
        session: &intent.session,
        deadline,
        cancellation: cancellation.clone(),
        locks: guard.inherit()?,
    };
    let route = if intent.route.workspace_id.is_empty() && intent.version == 1 {
        let Some(event) = state
            .events
            .iter()
            .find(|e| e.entity == operation.as_str() && e.kind == "runtime.launch_workspace")
        else {
            let Some((workspace, proof)) =
                recover_workspace(&api, &intent, deadline, &cancellation)?
            else {
                return Ok(None);
            };
            inventory::check(
                &project,
                &record.inputs.binding,
                &workspace.route,
                deadline,
                cancellation.clone(),
            )?;
            proof.check()?;
            db.observe_launch_workspace(
                &PreparedLaunchWorkspace { target: workspace },
                expected_revision,
                state.head,
                now(),
            )?;
            return Ok(None);
        };
        let workspace: LaunchTarget = serde_json::from_value(event.payload.clone())?;
        ensure!(
            workspace.operation == *operation
                && workspace.attempt == record.attempt
                && workspace.session == intent.session
                && workspace.route.socket == intent.route.socket
                && workspace.route.cwd == intent.route.cwd,
            "workspace recovery identity changed"
        );
        if !state.events.iter().any(|e| {
            e.entity == operation.as_str()
                && e.kind == "runtime.launch_layout"
                && e.payload == event.payload
        }) {
            return Ok(None);
        }
        RuntimeRoute {
            workspace_id: workspace.route.workspace_id,
            ..intent.route.clone()
        }
    } else {
        intent.route.clone()
    };
    let found = if intent.version == 2 {
        recover_supervised_root(&api, &intent)?
    } else {
        find_created_target(&api, &intent, &route)?
    };
    let Some((target, _observation)) = found else {
        return Ok(None);
    };
    inventory::check(
        &project,
        &record.inputs.binding,
        &target.route,
        deadline,
        cancellation.clone(),
    )?;
    check(deadline, &cancellation)?;
    // The pinned observation established this exact incarnation while alive.
    // Its subsequent exit does not invalidate that evidence. Persist it so the
    // termination service can prove quiescence instead of losing its identity.
    // Keep the OS handles through commit; never infer death from a missing pane.
    db.observe_launch_target(
        &PreparedLaunchTarget {
            target: target.clone(),
        },
        expected_revision,
        state.head,
        now(),
    )?;
    Ok(Some(target))
}

#[cfg(target_os = "linux")]
fn find_created_target(
    api: &Api<'_>,
    intent: &LaunchCreationIntent,
    route: &RuntimeRoute,
) -> Result<
    Option<(
        LaunchTarget,
        crate::worker_supervision::SupervisorObservation,
    )>,
> {
    let list = api.call(
        intent.operation.as_str(),
        "pane.list",
        json!({"workspace_id":route.workspace_id}),
        || Ok(()),
    )?;
    let panes = list["panes"].as_array().context("pane inventory missing")?;
    ensure!(
        panes.len() <= 256,
        "creation recovery pane inventory exceeds limit"
    );
    ensure!(
        intent.version != 2 || panes.len() == 1,
        "direct workspace root layout changed"
    );
    let mut found = None;
    let mut seen = std::collections::BTreeSet::new();
    for pane in panes {
        check(api.deadline, &api.cancellation)?;
        let id = pane["pane_id"].as_str().context("pane identity missing")?;
        ensure!(
            seen.insert(id) && pane["workspace_id"].as_str() == Some(route.workspace_id.as_str()),
            "duplicate or foreign pane in creation recovery"
        );
        let before = api.pane_identity(&intent.operation, id, &route)?;
        let result = api.call(
            intent.operation.as_str(),
            "pane.process_info",
            json!({"pane_id":id}),
            || Ok(()),
        )?;
        ensure!(
            result["process_info"]["pane_id"].as_str() == Some(id),
            "process inventory route mismatch"
        );
        // Herdr omits an empty foreground_processes vector.
        let processes = result["process_info"].get("foreground_processes");
        let Some(processes) = processes else {
            continue;
        };
        let processes = processes.as_array().context("invalid process inventory")?;
        ensure!(
            processes.len() <= 256,
            "creation recovery process inventory exceeds limit"
        );
        for process in processes {
            let argv: Vec<String> = serde_json::from_value(process["argv"].clone())?;
            ensure!(
                argv.len() <= 160 && argv.iter().map(String::len).sum::<usize>() <= 65536,
                "creation recovery arguments exceed limit"
            );
            if format!("{:x}", Sha256::digest(serde_json::to_vec(&argv)?)) != intent.command_digest
            {
                continue;
            }
            ensure!(
                before.cwd.as_deref() == Some(route.cwd.as_str()),
                "recovered pane working directory mismatch"
            );
            ensure!(
                found.is_none(),
                "multiple resources match the uncertain creation"
            );
            let pid = u32::try_from(process["pid"].as_u64().context("supervisor PID missing")?)?;
            let observation =
                crate::worker_supervision::SupervisorObservation::observe(pid, &argv)?;
            let observed_unix_ms = now();
            ensure!(
                api.pane_identity(&intent.operation, id, &route)? == before,
                "recovered terminal changed during process observation"
            );
            let target = LaunchTarget {
                version: 2,
                operation: intent.operation.clone(),
                attempt: intent.attempt.clone(),
                route: RuntimeRoute {
                    pane_id: id.into(),
                    tab_id: before.tab_id.clone(),
                    ..route.clone()
                },
                terminal: before.terminal_id.clone(),
                session: intent.session.clone(),
                supervisor: Some(observation.identity().clone()),
                observed_unix_ms,
            };
            found = Some((target, observation));
        }
    }
    Ok(found)
}

#[cfg(target_os = "linux")]
fn recover_supervised_root(
    api: &Api<'_>,
    intent: &LaunchCreationIntent,
) -> Result<
    Option<(
        LaunchTarget,
        crate::worker_supervision::SupervisorObservation,
    )>,
> {
    let list = api.call(
        intent.operation.as_str(),
        "workspace.list",
        json!({}),
        || Ok(()),
    )?;
    ensure!(
        list["type"].as_str() == Some("workspace_list"),
        "invalid workspace inventory"
    );
    let workspaces = list["workspaces"]
        .as_array()
        .context("workspace inventory missing")?;
    ensure!(
        workspaces.len() <= 128,
        "workspace recovery inventory exceeds limit"
    );
    let mut seen = std::collections::BTreeSet::new();
    let mut found = None;
    for workspace in workspaces {
        check(api.deadline, &api.cancellation)?;
        let id = workspace["workspace_id"]
            .as_str()
            .context("workspace identity missing")?;
        ensure!(
            !id.is_empty() && id.len() <= 512 && seen.insert(id),
            "invalid or duplicate workspace identity"
        );
        if workspace["label"].as_str() != Some(worker_agent_name(&intent.attempt).as_str()) {
            continue;
        }
        ensure!(
            workspace["pane_count"].as_u64() == Some(1)
                && workspace["tab_count"].as_u64() == Some(1),
            "direct workspace layout changed; retain uncertainty"
        );
        let route = RuntimeRoute {
            workspace_id: id.into(),
            ..intent.route.clone()
        };
        if let Some(candidate) = find_created_target(api, intent, &route)? {
            ensure!(
                found.is_none(),
                "multiple workspaces match the uncertain creation"
            );
            found = Some(candidate);
        }
    }
    Ok(found)
}

// Reproduce the historical creation boundary only for legacy recovery tests.
#[cfg(all(test, target_os = "linux"))]
pub(super) fn create_legacy_resource(
    project: &Path,
    operation: &OperationId,
    expected_revision: u64,
    deadline: Instant,
    cancellation: Cancellation,
) -> Result<LaunchTarget> {
    create_resource_inner(
        project,
        operation,
        expected_revision,
        deadline,
        cancellation,
        false,
        true,
    )
}

/// Submit the gate line once under the original launch claim. An OK reply is
/// not a start receipt. The controller observes the agent independently afterward.
#[cfg(target_os = "linux")]
pub fn release_gate(
    project: &Path,
    operation: &OperationId,
    expected_revision: u64,
    deadline: Instant,
    cancellation: Cancellation,
) -> Result<()> {
    let mut deadline = deadline.min(Instant::now() + Duration::from_secs(45));
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
        delivery.revision == expected_revision
            && delivery.state == crate::operations::DeliveryState::Claimed
            && delivery.attempts == 1,
        "gate release requires the original claimed launch"
    );
    ensure!(
        !state.events.iter().any(|e| e.entity == operation.as_str()
            && matches!(
                e.kind.as_str(),
                "runtime.launch_release" | "runtime.launch_started"
            )),
        "gate input was already attempted; observe without replay"
    );
    let claim = crate::operations::Claim {
        operation: operation.clone(),
        revision: delivery.revision,
        epoch: delivery.epoch,
        owner: delivery.owner.clone().context("claim owner missing")?,
        lease_until_ms: delivery.lease_until_ms.context("claim lease missing")?,
    };
    db.validate_claim(&claim, now())?;
    deadline = deadline.min(
        Instant::now()
            + Duration::from_millis(claim.lease_until_ms.saturating_sub(now()).max(0) as u64),
    );
    let record = state
        .attempt_inputs
        .iter()
        .find(|r| &r.operation == operation)
        .context("launch inputs missing")?;
    let profile = record
        .inputs
        .effective_profile
        .as_ref()
        .context("launch profile missing")?;
    profile.validate_for_launch().map_err(anyhow::Error::msg)?;
    ensure!(
        profile.herdr.version == "0.9.1",
        "gate release requires Herdr 0.9.1"
    );
    validate_execution_home(profile, &project)?;
    let target: LaunchTarget = serde_json::from_value(
        state
            .events
            .iter()
            .find(|e| e.kind == "runtime.launch_target" && e.entity == operation.as_str())
            .context("gate target missing")?
            .payload
            .clone(),
    )?;
    ensure!(
        target.version == 2 && target.operation == *operation && target.attempt == record.attempt,
        "gate target does not match launch"
    );
    let brief =
        crate::memory::render_attempt_brief_held(&project, record.attempt.as_str(), &mut db)?;
    let worktrees=crate::worktree_preparation::verify_held(&project,&state,record,deadline,cancellation.clone(),guard.inherit()?)?;
    let argv = command(profile, operation, brief.prompt_chars)?;
    executable(&profile.agent, deadline, &cancellation)?;
    let supervisor = crate::worker_supervision::SupervisorObservation::reconnect(
        target
            .supervisor
            .as_ref()
            .context("gate supervisor missing")?,
    )?;
    let gate = supervisor.waiting_gate(&argv)?;
    let api = Api {
        executable: &profile.herdr,
        socket: &target.route.socket,
        session: &target.session,
        deadline,
        cancellation: cancellation.clone(),
        locks: guard.inherit()?,
    };
    let pane = api.pane_identity(operation, &target.route.pane_id, &target.route)?;
    ensure!(
        pane.terminal_id == target.terminal
            && pane.tab_id == target.route.tab_id
            && pane.cwd.as_deref() == Some(target.route.cwd.as_str()),
        "gate terminal identity changed"
    );
    inventory::check(
        &project,
        &record.inputs.binding,
        &target.route,
        deadline,
        cancellation.clone(),
    )?;
    let result = api.call(operation.as_str(), "pane.send_input",
        json!({"pane_id":target.route.pane_id,"text":format!("{}\n", release_token(operation)),"keys":[]}), || {
            worktrees.check()?;
            gate.check()?;
            executable(&profile.agent, deadline, &cancellation)?;
            let current = api.pane_identity(operation, &target.route.pane_id, &target.route)?;
            ensure!(current == pane, "gate terminal changed immediately before release");
            gate.check()?;
            db.record_launch_release(&claim, &PreparedLaunchRelease {intent: LaunchReleaseIntent {
                version: 1, target: target.clone(), observed_unix_ms: now()}}, now())?;
            Ok(())
        })?;
    ensure!(
        result["type"].as_str() == Some("ok"),
        "uncertain gate input acknowledgment; observe without replay"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
pub(super) fn validate_execution_home(profile: &FrozenProfile, project: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let home = Path::new(
        profile
            .execution_home
            .as_ref()
            .context("explicit execution environment required before gate release")?,
    );
    let metadata = std::fs::symlink_metadata(home)?;
    ensure!(
        home.canonicalize()? == home
            && metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o022 == 0
            && !home.starts_with(project),
        "execution home must be a canonical owner-controlled directory outside the project"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn recover_workspace(
    api: &Api<'_>,
    intent: &LaunchCreationIntent,
    deadline: Instant,
    cancellation: &Cancellation,
) -> Result<
    Option<(
        LaunchTarget,
        crate::worker_supervision::ProcessMarkerObservation,
    )>,
> {
    let Some(token) = &intent.workspace_token else {
        return Ok(None);
    };
    let list = api.call(
        intent.operation.as_str(),
        "workspace.list",
        json!({}),
        || Ok(()),
    )?;
    ensure!(
        list["type"].as_str() == Some("workspace_list"),
        "invalid workspace inventory"
    );
    let workspaces = list["workspaces"]
        .as_array()
        .context("workspace inventory missing")?;
    ensure!(
        workspaces.len() <= 128,
        "workspace recovery inventory exceeds limit"
    );
    let mut seen = std::collections::BTreeSet::new();
    let mut found = None;
    for workspace in workspaces {
        check(deadline, cancellation)?;
        let id = workspace["workspace_id"]
            .as_str()
            .context("workspace identity missing")?;
        ensure!(
            !id.is_empty() && id.len() <= 512 && seen.insert(id),
            "invalid or duplicate workspace identity"
        );
        if workspace["label"].as_str() != Some(worker_agent_name(&intent.attempt).as_str()) {
            continue;
        }
        ensure!(
            workspace["pane_count"].as_u64() == Some(1)
                && workspace["tab_count"].as_u64() == Some(1),
            "uncertain workspace layout changed; retain uncertainty"
        );
        let route = RuntimeRoute {
            workspace_id: id.into(),
            ..intent.route.clone()
        };
        let list = api.call(
            intent.operation.as_str(),
            "pane.list",
            json!({"workspace_id":id}),
            || Ok(()),
        )?;
        let panes = list["panes"]
            .as_array()
            .context("workspace pane inventory missing")?;
        ensure!(
            panes.len() == 1 && panes[0]["workspace_id"].as_str() == Some(id),
            "workspace bootstrap pane changed"
        );
        let pane = panes[0]["pane_id"]
            .as_str()
            .context("bootstrap pane missing")?;
        let before = api.pane_identity(&intent.operation, pane, &route)?;
        ensure!(
            before.cwd.as_deref() == Some(intent.route.cwd.as_str()),
            "workspace directory changed"
        );
        let info = api.call(
            intent.operation.as_str(),
            "pane.process_info",
            json!({"pane_id":pane}),
            || Ok(()),
        )?;
        ensure!(
            info["process_info"]["pane_id"].as_str() == Some(pane),
            "bootstrap process route changed"
        );
        let Some(processes) = info["process_info"].get("foreground_processes") else {
            continue;
        };
        let processes = processes
            .as_array()
            .context("invalid bootstrap process inventory")?;
        ensure!(
            processes.len() <= 256,
            "bootstrap process inventory exceeds limit"
        );
        let mut proof = None;
        for process in processes {
            check(deadline, cancellation)?;
            let pid = u32::try_from(process["pid"].as_u64().context("bootstrap PID missing")?)?;
            if let Some(observed) = crate::worker_supervision::ProcessMarkerObservation::observe(
                pid,
                token,
                Path::new(&intent.route.cwd),
            )? {
                proof = Some(observed);
            }
        }
        let Some(proof) = proof else {
            continue;
        };
        ensure!(
            found.is_none(),
            "multiple workspaces carry the creation marker"
        );
        ensure!(
            api.pane_identity(&intent.operation, pane, &route)? == before,
            "bootstrap terminal changed during recovery"
        );
        proof.check()?;
        found = Some((
            LaunchTarget {
                version: 1,
                operation: intent.operation.clone(),
                attempt: intent.attempt.clone(),
                route: RuntimeRoute {
                    tab_id: before.tab_id,
                    pane_id: pane.into(),
                    ..route
                },
                terminal: before.terminal_id,
                session: intent.session.clone(),
                supervisor: None,
                observed_unix_ms: now(),
            },
            proof,
        ));
    }
    if let Some((target, proof)) = &found {
        let current = api.pane_identity(&intent.operation, &target.route.pane_id, &target.route)?;
        ensure!(
            current.terminal_id == target.terminal
                && current.tab_id == target.route.tab_id
                && current.cwd.as_deref() == Some(target.route.cwd.as_str()),
            "recovered bootstrap changed before commit"
        );
        proof.check()?;
    }
    Ok(found)
}

/// Read-only admission before worktree creation consumes the launch approval.
pub(crate) fn validate_server_creation(profile:&FrozenProfile,route:&RuntimeRoute,operation:&OperationId,deadline:Instant,cancellation:Cancellation,locks:Vec<InheritedLock>)->Result<()> {
    if !route.workspace_id.is_empty() { return Ok(()); }
    let session=session_identity(Path::new(&route.socket))?;
    Api{executable:&profile.herdr,socket:&route.socket,session:&session,deadline,cancellation,locks}.require_workspace_command(operation)
}

/// Creation has already happened. Retain the exact observed incarnation even if
/// the original claim expired or was revoked while the native reply arrived.
/// This is observation only; it cannot renew permission to release the gate.
fn retain_observed_target(db:&mut crate::store::SqliteStore,target:&LaunchTarget)->Result<()> {
    let current=db.read_snapshot(None)?;
    let delivery=current.deliveries.iter().find(|d|d.operation==target.operation).context("observed launch delivery missing")?;
    db.observe_launch_target(&PreparedLaunchTarget{target:target.clone()},delivery.revision,current.head,now())?;
    Ok(())
}

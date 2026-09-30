//! Advisory canonical pane decoration. No operation, claim or receipt is written.
use crate::{
    runner::{Cmd, InheritedLock, Output, Runner},
    source_tree::Control,
};
use anyhow::{Context, Result, ensure};
use herdr_projects::{domain::*, execution_guard::ProjectGuard, runtime};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
const JOB: &str = "\0herdr-projects-attempt-tokens";
const BUDGET: Duration = Duration::from_secs(45);
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    project: PathBuf,
    identity: (u64, u64),
    binding: RuntimeBinding,
    ownership: RuntimeOwnership,
    started: LaunchStartedReceipt,
    executable: ExecutableIdentity,
    config: herdr_projects::migration::ConfigReference,
}
fn snapshot(path: &Path, control: &Control) -> Result<Snapshot> {
    control.check()?;
    runtime::snapshot_controlled(
        path,
        &herdr_projects::store::controlled::ReadControl::new(
            control.deadline,
            control.cancellation.clone(),
        ),
    )
}
impl Input {
    // A revoked claim permits only erasure on the unchanged retained route.
    // A replacement claim or binding forbids even erasure: that pane is no
    // longer ours. The native TTL bounds any decoration on an unreachable route.
    fn current(&self, guard: &ProjectGuard, control: &Control) -> Result<bool> {
        guard.check_project(&self.project)?;
        let m = std::fs::metadata(&self.project)?;
        ensure!(
            (m.dev(), m.ino()) == self.identity && self.project.canonicalize()? == self.project,
            "attempt token project changed"
        );
        ensure!(
            herdr_projects::canonical_worker::session_identity(Path::new(
                &self.binding.identity.socket
            ))? == self.started.session,
            "attempt token session changed"
        );
        ensure!(
            herdr_projects::migration::config_reference(Path::new(&self.config.path))?
                == self.config,
            "attempt token configuration changed"
        );
        herdr_projects::canonical_worker::check_advisory_pane_aliases(
            &self.project,
            &self.binding.id,
            &self.started.route,
            control.deadline,
            control.cancellation.clone(),
        )?;
        let state = snapshot(&self.project, control)?;
        ensure!(
            state
                .runtime_bindings
                .iter()
                .find(|b| b.id == self.binding.id)
                == Some(&self.binding),
            "attempt token binding changed"
        );
        ensure!(
            !state
                .runtime_bindings
                .iter()
                .any(|b| b.id != self.binding.id
                    && b.identity.socket == self.binding.identity.socket
                    && b.identity.machine == self.binding.identity.machine
                    && b.identity.pane_id == self.binding.identity.pane_id),
            "attempt token pane is shared"
        );
        let owner = state
            .ownership
            .iter()
            .find(|o| o.binding == self.binding.id);
        ensure!(
            owner.is_none() || owner == Some(&self.ownership),
            "attempt token ownership changed"
        );
        let attempt = state
            .attempts
            .iter()
            .find(|a| a.id == self.started.attempt)
            .context("attempt token attempt missing")?;
        let collector_revoked = herdr_projects::telemetry::collectors::binding_revoked(
            &self.project,
            self.started.attempt.as_str(),
        )?;
        control.check()?;
        Ok(!collector_revoked
            && owner.is_some()
            && matches!(
                attempt.state,
                AttemptState::Running | AttemptState::AwaitingInput
            )
            && !attempt.termination_observed
            && state
                .control
                .as_ref()
                .is_some_and(|c| c.state == ProjectState::Active && !c.reconciliation_required))
    }
    fn call(
        &self,
        method: &str,
        params: Value,
        control: &Control,
        locks: &[InheritedLock],
    ) -> Result<Value> {
        self.call_checked(method, params, control, locks, || Ok(()))
    }
    fn call_checked(
        &self,
        method: &str,
        params: Value,
        control: &Control,
        locks: &[InheritedLock],
        preflight: impl FnOnce() -> Result<()>,
    ) -> Result<Value> {
        control.check()?;
        herdr_projects::canonical_worker::executable(
            &self.executable,
            control.deadline,
            &control.cancellation,
        )?;
        let id = format!("attempt-tokens-{method}");
        let mut cmd = Cmd::new(&self.executable.path, Duration::from_secs(15))
            .arg("remote-api-bridge")
            .env("HERDR_SOCKET_PATH", &self.binding.identity.socket)
            .env("PATH", "/usr/bin:/bin")
            .stdin(
                serde_json::to_string(&json!({"id":id,"method":method,"params":params}))? + "\n",
            );
        cmd.env_clear = true;
        cmd.capture_limit = 1024 * 1024;
        preflight()?;
        ensure!(
            herdr_projects::canonical_worker::session_identity(Path::new(
                &self.binding.identity.socket
            ))? == self.started.session,
            "attempt token session changed before request"
        );
        let out = herdr_projects::supervision::run(
            cmd,
            control.deadline,
            control.cancellation.clone(),
            locks,
        )?;
        control.check()?;
        ensure!(
            herdr_projects::canonical_worker::session_identity(Path::new(
                &self.binding.identity.socket
            ))? == self.started.session,
            "attempt token session changed during request"
        );
        ensure!(out.success(), "attempt token native request failed");
        let reply: Value = serde_json::from_slice(&out.stdout_bytes)?;
        ensure!(
            reply["id"] == id && reply.get("error").is_none(),
            "attempt token acknowledgement mismatch"
        );
        reply
            .get("result")
            .cloned()
            .context("attempt token result missing")
    }
    fn observe(&self, control: &Control, locks: &[InheritedLock], publishing: bool) -> Result<()> {
        let panes = self.call("pane.list", json!({}), control, locks)?;
        let panes = panes["panes"]
            .as_array()
            .context("attempt token pane inventory missing")?;
        ensure!(
            panes.len() <= 4096,
            "attempt token inventory exceeds bounds"
        );
        let matched: Vec<_> = panes
            .iter()
            .filter(|p| p["pane_id"] == self.binding.identity.pane_id)
            .collect();
        ensure!(matched.len() == 1, "attempt token pane absent or ambiguous");
        let pane = matched[0];
        for (key, expected) in [
            ("workspace_id", &self.started.route.workspace_id),
            ("tab_id", &self.started.route.tab_id),
            ("terminal_id", &self.started.terminal),
            ("cwd", &self.started.route.cwd),
        ] {
            ensure!(
                pane[key].as_str() == Some(expected),
                "attempt token pane identity changed"
            );
        }
        let agents = self.call("agent.list", json!({}), control, locks)?;
        let agents = agents["agents"]
            .as_array()
            .context("attempt token agent inventory missing")?;
        ensure!(
            agents.len() <= 4096,
            "attempt token agent inventory exceeds bounds"
        );
        let matched: Vec<_> = agents
            .iter()
            .filter(|a| a["pane_id"] == self.binding.identity.pane_id)
            .collect();
        ensure!(
            matched.len() <= 1 && (!publishing || matched.len() == 1),
            "attempt token agent absent or ambiguous"
        );
        for agent in matched {
            for key in ["workspace_id", "tab_id", "terminal_id", "cwd"] {
                ensure!(agent[key] == pane[key], "attempt token agent route changed");
            }
            ensure!(
                agent["agent"] == self.started.agent.kind
                    && agent["name"] == self.started.agent.name,
                "attempt token agent changed"
            );
        }
        Ok(())
    }
}
pub fn requests(path: &Path, control: &Control) -> Result<Vec<crate::executor::Request>> {
    let state = snapshot(path, control)?;
    let project = path.canonicalize()?;
    let m = std::fs::metadata(&project)?;
    let mut requests = Vec::new();
    // Retained launched ownership also permits cleanup after relinquishment and
    // across ticker restarts. Never substitute a newer binding's pane.
    for binding in &state.runtime_bindings {
        let Some(event) = state
            .events
            .iter()
            .rev()
            .find(|e| e.kind == "runtime.launched" && e.entity == binding.id)
        else {
            continue;
        };
        let ownership: RuntimeOwnership = serde_json::from_value(event.payload.clone())?;
        if ownership.binding_revision != binding.revision
            || ownership.identity_digest
                != crate::thread::sha256_hex(&serde_json::to_vec(&binding.identity)?)
            || !binding.identity.machine.is_empty()
        {
            continue;
        }
        let Some(attempt) = ownership.attempt.clone() else {
            continue;
        };
        // There is no suffix to erase before the first running observation.
        if state
            .control
            .as_ref()
            .is_some_and(|c| c.state == ProjectState::Active)
            && state.attempts.iter().any(|a| {
                a.id == attempt
                    && matches!(a.state, AttemptState::Reserved | AttemptState::Launching)
            })
            && state.ownership.iter().any(|o| o == &ownership)
        {
            continue;
        }
        let Some(record) = state.attempt_inputs.iter().find(|r| r.attempt == attempt) else {
            continue;
        };
        let Some(event) = state
            .events
            .iter()
            .find(|e| e.kind == "runtime.launch_started" && e.entity == record.operation.as_str())
        else {
            continue;
        };
        let started: LaunchStartedReceipt = serde_json::from_value(event.payload.clone())?;
        ensure!(
            started.attempt == attempt
                && RuntimeRoute::from_identity(&binding.identity) == started.route
                && ownership.session.as_ref() == Some(&started.session),
            "attempt token retained route differs"
        );
        let Some(profile) = &record.inputs.effective_profile else {
            continue;
        };
        let input = Input {
            project: project.clone(),
            identity: (m.dev(), m.ino()),
            binding: binding.clone(),
            ownership,
            started,
            executable: profile.herdr.clone(),
            config: record.inputs.config.clone(),
        };
        let deadline = Instant::now() + BUDGET;
        let mut command = Cmd::new(JOB, BUDGET).stdin(serde_json::to_string(&input)?);
        command.deadline = Some(deadline);
        requests.push(crate::executor::Request {
            identity: crate::executor::Identity {
                operation: format!("tokens:attempt:{}", attempt.as_str()),
                revision: binding.revision,
                project: project.display().to_string(),
                machine: format!("brief-root:{}", project.parent().unwrap().display()),
                terminal: Some(binding.identity.pane_id.clone()),
            },
            lane: crate::executor::Lane::Control,
            deadline,
            command,
        });
    }
    Ok(requests)
}
fn execute(input: &Input, control: &Control) -> Result<()> {
    ensure!(
        input.project.is_absolute() && input.ownership.origin == "launched",
        "invalid attempt token input"
    );
    let guard = ProjectGuard::acquire(&input.project)?;
    let locks = guard.inherit_transfer()?;
    let publishing = input.current(&guard, control)?;
    input.observe(control, &locks, publishing)?;
    let slug = input
        .project
        .file_name()
        .and_then(|s| s.to_str())
        .context("attempt token slug missing")?;
    let fleet = herdr_projects::telemetry::workspace::snapshot(&input.project, slug);
    let active = fleet["active"].as_array().and_then(|a| {
        a.iter()
            .find(|a| a["attempt_id"] == input.started.attempt.as_str())
    });
    let suffix = crate::threads::sidebar_suffix(
        &input.started.agent.kind,
        active.and_then(|a| a["coverage"].as_str()),
        active
            .filter(|a| a["waiting"]["open"] == true)
            .and_then(|a| a["waiting"]["open_observed_ms"].as_i64())
            .map(|ms| ms.max(0) / 1000),
    );
    control.check()?;
    // The project guard spans all observations and the effect; store mutations
    // and route replacements cannot interleave with the final authority check.
    let publishing = input.current(&guard, control)?;
    input.observe(control, &locks, publishing)?;
    let publishing = input.current(&guard, control)?;
    let tokens = if publishing {
        json!({"telemetry":suffix})
    } else {
        json!({"telemetry":null})
    };
    let result = input.call_checked("pane.report_metadata", json!({"pane_id":input.binding.identity.pane_id,"source":crate::herdr::SOURCE,"ttl_ms":crate::coordinator::TOKEN_TTL.as_millis() as u64,"tokens":tokens}), control, &locks, || {
        ensure!(input.current(&guard, control)? == publishing, "attempt token lifecycle changed before send");
        Ok(())
    })?;
    ensure!(
        result["type"] == "ok",
        "attempt token response has wrong type"
    );
    let publishing = input.current(&guard, control)?;
    input.observe(control, &locks, publishing)?;
    Ok(())
}
pub struct JobRunner {
    pub inner: Arc<dyn Runner + Send + Sync>,
}
impl Runner for JobRunner {
    fn run(&self, cmd: &Cmd) -> Result<Output> {
        if cmd.program != JOB {
            return self.inner.run(cmd);
        }
        let entered = Instant::now();
        ensure!(
            !cmd.timeout.is_zero() && cmd.timeout <= BUDGET,
            "invalid attempt token budget"
        );
        let text = cmd
            .stdin
            .as_deref()
            .context("attempt token input missing")?;
        ensure!(
            text.len() <= 64 * 1024,
            "attempt token input exceeds bounds"
        );
        let control = Control {
            deadline: cmd
                .deadline
                .context("attempt token deadline missing")?
                .min(entered + cmd.timeout),
            cancellation: cmd
                .cancellation
                .clone()
                .context("attempt token cancellation missing")?,
        };
        execute(&serde_json::from_str(text)?, &control)?;
        Ok(Output {
            code: Some(0),
            elapsed: entered.elapsed(),
            ..Default::default()
        })
    }
    fn socket_request(&self, p: &Path, s: &str, t: Duration) -> Result<String> {
        self.inner.socket_request(p, s, t)
    }
}

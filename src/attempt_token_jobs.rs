//! Advisory canonical pane decoration. No operation, claim or receipt is written.
use crate::{
    runner::{Cmd, Output, RealRunner, Runner},
    source_tree::Control,
};
use anyhow::{Context, Result, ensure};
use herdr_projects::{domain::*, execution_guard::ProjectGuard};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
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
fn rows(
    path: &Path,
    binding: Option<&str>,
    control: &Control,
) -> Result<herdr_projects::store::attempt_tokens::Rows> {
    control.check()?;
    herdr_projects::migration::open_active_scoped(
        path,
        herdr_projects::store::controlled::ReadControl::new(
            control.deadline,
            control.cancellation.clone(),
        ),
    )?
    .attempt_tokens(
        binding,
        jiff::Timestamp::now().as_millisecond() - crate::coordinator::TOKEN_TTL.as_millis() as i64,
    )
    .map_err(Into::into)
}
// Volatile scheduling hints only: bounded by project/attempt inventory and TTL.
#[derive(Default)]
struct Hints {
    cursors: BTreeMap<PathBuf, (String, Instant)>,
    sent: BTreeMap<String, (String, Instant)>,
}
fn hints() -> &'static Mutex<Hints> {
    static HINTS: OnceLock<Mutex<Hints>> = OnceLock::new();
    HINTS.get_or_init(|| Mutex::new(Hints::default()))
}
impl Input {
    // A revoked claim permits only erasure on the unchanged retained route.
    // A replacement claim or binding forbids even erasure: that pane is no
    // longer ours. The native TTL bounds any decoration on an unreachable route.
    fn current(&self, control: &Control) -> Result<Option<bool>> {
        let guard = ProjectGuard::acquire(&self.project)?;
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
        let state = rows(&self.project, Some(&self.binding.id), control)?;
        let entry = state
            .entries
            .first()
            .context("attempt token binding missing")?;
        ensure!(
            entry.binding == self.binding,
            "attempt token binding changed"
        );
        ensure!(
            entry.retained == self.ownership
                && (entry.owner.is_none() || entry.owner.as_ref() == Some(&self.ownership)),
            "attempt token ownership changed"
        );
        ensure!(
            entry.started == self.started,
            "attempt token launch changed"
        );
        control.check()?;
        Ok((entry.publishing
            || entry.cleanup_ms.is_some_and(|ms| {
                jiff::Timestamp::now().as_millisecond() - ms
                    < crate::coordinator::TOKEN_TTL.as_millis() as i64
            }))
        .then_some(entry.publishing))
    }

    fn call(&self, method: &str, params: Value, control: &Control) -> Result<Value> {
        self.call_checked(method, params, control, || Ok(()))
    }
    fn call_checked(
        &self,
        method: &str,
        params: Value,
        control: &Control,
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
        // Advisory bridge requests use the normal bounded process runner,
        // whose spawns pass through GatedSpawn. They transfer no effect locks.
        cmd.deadline = Some(control.deadline);
        cmd.cancellation = Some(control.cancellation.clone());
        let out = RealRunner.run(&cmd)?;
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
    fn observe(&self, control: &Control, publishing: bool) -> Result<bool> {
        let panes = self.call("pane.list", json!({}), control)?;
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
        if matched.is_empty() && !publishing {
            return Ok(false);
        }
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
        let agents = self.call("agent.list", json!({}), control)?;
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
        Ok(true)
    }
}
pub fn requests(path: &Path, control: &Control) -> Result<Vec<crate::executor::Request>> {
    let state = rows(path, None, control)?;
    let project = path.canonicalize()?;
    let m = std::fs::metadata(&project)?;
    let mut requests = Vec::new();
    for entry in state.entries {
        let binding = entry.binding;
        let ownership = entry.retained;
        if entry
            .owner
            .as_ref()
            .is_some_and(|owner| owner != &ownership)
        {
            continue;
        }
        if ownership.binding_revision != binding.revision
            || ownership.identity_digest
                != crate::thread::sha256_hex(&serde_json::to_vec(&binding.identity)?)
            || !binding.identity.machine.is_empty()
        {
            continue;
        }
        let attempt = entry.attempt.id;
        let record = entry.input;
        let started = entry.started;
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
    requests.sort_by(|a, b| a.identity.operation.cmp(&b.identity.operation));
    let mut hints = hints().lock().unwrap_or_else(|e| e.into_inner());
    hints
        .cursors
        .retain(|_, (_, at)| at.elapsed() < crate::coordinator::TOKEN_TTL);
    if let Some((last, _)) = hints.cursors.get(&project) {
        let offset = requests.partition_point(|r| r.identity.operation <= *last);
        let len = requests.len();
        if len > 0 {
            requests.rotate_left(offset % len);
        }
    }
    requests.truncate(16);
    if let Some(last) = requests.last() {
        hints
            .cursors
            .insert(project, (last.identity.operation.clone(), Instant::now()));
    }
    Ok(requests)
}
fn execute(input: &Input, control: &Control) -> Result<()> {
    ensure!(
        input.project.is_absolute() && input.ownership.origin == "launched",
        "invalid attempt token input"
    );
    if input.current(control)?.is_none() {
        return Ok(());
    }
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
    let Some(publishing) = input.current(control)? else {
        return Ok(());
    };
    let key = crate::thread::sha256_hex(&serde_json::to_vec(input)?);
    let decoration = if publishing {
        suffix.clone()
    } else {
        String::new()
    };
    {
        let mut hints = hints().lock().unwrap_or_else(|e| e.into_inner());
        hints
            .sent
            .retain(|_, (_, at)| at.elapsed() < crate::coordinator::TOKEN_TTL);
        if hints.sent.get(&key).is_some_and(|(sent, at)| {
            sent == &decoration && at.elapsed() < crate::coordinator::TOKEN_TTL / 3
        }) {
            return Ok(());
        }
    }
    if !input.observe(control, publishing)? {
        hints()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sent
            .insert(key, (decoration, Instant::now()));
        return Ok(());
    }
    let publishing_now = input.current(control)?;
    ensure!(
        publishing_now == Some(publishing),
        "attempt token lifecycle changed during observation"
    );
    // Short guarded checks fence store generations, but no effect lock spans
    // native I/O. A lifecycle/route change can race the send after the last
    // check. This advisory suffix grants no authority, records no success, and
    // expires within TOKEN_TTL; the post-send check detects the race. Native
    // route/session observations also reject replacements before each send.
    let tokens = if publishing {
        json!({"telemetry":suffix})
    } else {
        json!({"telemetry":null})
    };
    let result = input.call_checked("pane.report_metadata", json!({"pane_id":input.binding.identity.pane_id,"source":crate::herdr::SOURCE,"ttl_ms":crate::coordinator::TOKEN_TTL.as_millis() as u64,"tokens":tokens}), control, || {
        ensure!(input.current(control)? == Some(publishing), "attempt token lifecycle changed before send");
        Ok(())
    })?;
    ensure!(
        result["type"] == "ok",
        "attempt token response has wrong type"
    );
    // This is only a volatile cadence hint, even if the following checks fail
    // or the native endpoint ignored the update. A changed suffix bypasses it.
    hints()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .sent
        .insert(key, (decoration, Instant::now()));
    ensure!(
        input.current(control)? == Some(publishing),
        "attempt token lifecycle changed during send"
    );
    input.observe(control, publishing)?;
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
        if let Err(error) = execute(&serde_json::from_str(text)?, &control) {
            // The canonical controller owns priority. A nonblocking guard
            // conflict is a skipped decoration tick, not a native failure.
            if !matches!(
                error.downcast_ref::<std::fs::TryLockError>(),
                Some(std::fs::TryLockError::WouldBlock)
            ) {
                return Err(error);
            }
        }
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

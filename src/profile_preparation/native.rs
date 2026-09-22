//! Native transport and optional one-use prompt observations; never workflow certification.
use super::*;
use crate::{
    runner::InheritedLock,
    worker_supervision::{SupervisorIdentity, SupervisorObservation},
};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, FileTypeExt},
    os::unix::process::CommandExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
};

/// Only the native verifier constructs this evidence. JSON is a report, not an
/// importable capability grant.
/// ```compile_fail
/// use herdr_projects::profile_preparation::NativePreparation;
/// let _: NativePreparation = serde_json::from_str("{}").unwrap();
/// ```
#[derive(Serialize)]
pub struct NativePreparation {
    pub(crate) preparation: ProfilePreparation,
    pub(crate) evidence: NativeEvidence,
    #[serde(rename = "source_store")]
    pub(super) source: (PathBuf, u64, u64),
    // O_PATH pins the inode without opening it for I/O or interfering with
    // SQLite's POSIX advisory locks when this descriptor closes.
    #[serde(skip)]
    pub(super) source_file: fs::File,
}

impl NativePreparation {
    pub(crate) fn check_store(&self, path: &Path) -> Result<()> {
        let path = path.canonicalize()?;
        let metadata = fs::symlink_metadata(&path)?;
        let original = self.source_file.metadata()?;
        ensure!(
            path == self.source.0 && metadata.is_file()
                && (metadata.dev(), metadata.ino()) == (self.source.1, self.source.2)
                && (original.dev(), original.ino()) == (self.source.1, self.source.2),
            "native verification belongs to another or replaced project store"
        );
        Ok(())
    }

    /// Retain this verifier-produced result; JSON cannot invoke this authority.
    /// Retention does not reserve a worker or certify a workflow.
    pub fn retain(&self, project: &Path) -> Result<VersionedReference> {
        let project = project.canonicalize()?;
        let _guard = crate::execution_guard::RootGuard::exclusive(
            project.parent().context("project root missing")?,
        )?;
        let path = project.join(".state/state.db");
        self.check_store(&path)?;
        let profile = &self.preparation.profile;
        ensure!(
            crate::authority::routine_policy(&project)?
                == (profile.permission_policy.clone(), profile.config.clone()),
            "native profile policy changed before retention"
        );
        crate::store::SqliteStore::open(&path)?.retain_native_profile(self)?;
        Ok(self.preparation.reference.clone())
    }
}

#[derive(Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeEvidence {
    pub version: u32,
    pub prepared_profile: VersionedReference,
    pub supervisor: SupervisorIdentity,
    pub native_kind: String,
    pub observed_unix_ms: i64,
    pub stopped_unix_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interaction: Option<InteractionEvidence>,
}

#[derive(Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionEvidence {
    pub session: ResourceIdentity,
    pub terminal: String,
    pub readiness_manifest: String,
    pub prompt_digest: String,
    pub acknowledged_unix_ms: i64,
}

pub(super) struct Lab {
    pub(super) root: PathBuf,
    server: Option<Child>,
    worker: Option<SupervisorIdentity>,
}
impl Lab {
    pub(super) fn new() -> Result<Self> {
        let mut bytes = [0u8; 32];
        fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        let root = std::env::temp_dir()
            .canonicalize()?
            .join(format!("hp-native-{}", digest(&bytes)));
        fs::DirBuilder::new().mode(0o700).create(&root)?;
        let lab = Self {
            root,
            server: None,
            worker: None,
        };
        for dir in ["home", "runtime", "work"] {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(lab.root.join(dir))?;
        }
        fs::write(
            lab.root.join("config.toml"),
            "onboarding = false\n[terminal]\ndefault_shell = '/bin/sh'\nshell_mode = 'non_login'\n[update]\nversion_check = false\nmanifest_check = false\n",
        )?;
        Ok(lab)
    }
    fn stop_worker(&mut self) -> Result<()> {
        if let Some(identity) = &self.worker {
            SupervisorObservation::stop_recorded(
                identity,
                Instant::now() + Duration::from_secs(5),
                &Cancellation::default(),
            )?;
            ensure!(
                SupervisorObservation::recover_exited(identity)?,
                "native probe worker termination unproven"
            );
        }
        self.worker = None;
        Ok(())
    }
}
impl Drop for Lab {
    fn drop(&mut self) {
        let _ = self.stop_worker();
        if let Some(server) = &mut self.server {
            if matches!(server.try_wait(), Ok(None)) {
                // This unreaped live child is the group leader created below.
                // Never signal a group after reaping its leader.
                unsafe {
                    libc::kill(-(server.id() as i32), libc::SIGKILL);
                }
            }
            let until = Instant::now() + Duration::from_secs(2);
            while matches!(server.try_wait(), Ok(None)) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct Api<'a> {
    profile: &'a FrozenProfile,
    socket: PathBuf,
    session: ResourceIdentity,
    deadline: Instant,
    cancellation: Cancellation,
    locks: Vec<InheritedLock>,
}
impl Api<'_> {
    fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.call_checked(method, params, || Ok(()))
    }
    fn call_checked(
        &self,
        method: &str,
        params: Value,
        preflight: impl FnOnce() -> Result<()>,
    ) -> Result<Value> {
        check(self.deadline, &self.cancellation)?;
        let expected = (&self.profile.herdr.path, &self.profile.herdr.digest);
        let actual = executable(Path::new(expected.0), self.deadline, &self.cancellation)?;
        ensure!(
            (&actual.0, &actual.1) == expected,
            "native probe executable changed"
        );
        ensure!(
            crate::canonical_worker::session_identity(&self.socket)? == self.session,
            "native probe session changed"
        );
        let mut command = Cmd::new(&self.profile.herdr.path, Duration::from_secs(10))
            .arg("remote-api-bridge")
            .env(
                "HERDR_SOCKET_PATH",
                self.socket.to_str().context("socket encoding")?,
            )
            .env("PATH", "/usr/bin:/bin")
            .stdin(
                serde_json::to_string(
                    &json!({"id":"native-profile-probe","method":method,"params":params}),
                )? + "\n",
            );
        command.env_clear = true;
        command.capture_limit = 1024 * 1024;
        preflight()?;
        check(self.deadline, &self.cancellation)?;
        let output = crate::supervision::run(
            command,
            self.deadline,
            self.cancellation.clone(),
            &self.locks,
        )?;
        ensure!(
            output.success() && !output.stdout_truncated,
            "native probe request {method} failed (output withheld)"
        );
        ensure!(
            crate::canonical_worker::session_identity(&self.socket)? == self.session,
            "native probe session changed"
        );
        let value: Value =
            serde_json::from_slice(&output.stdout_bytes).context("invalid native probe reply")?;
        ensure!(
            value["id"] == "native-profile-probe" && value.get("error").is_none(),
            "native probe rejected {method}"
        );
        value
            .get("result")
            .cloned()
            .context("native probe result missing")
    }
}

/// Start the exact prepared agent in a disposable native workspace, then prove
/// its supervised process tree stopped. No prompt or task content is submitted.
/// The returned launch/stop evidence cannot fill in the remaining capabilities.
pub fn verify_native(
    project: &Path,
    name: &str,
    herdr_path: &Path,
    agent_path: &Path,
    execution_home: &Path,
    deadline: Instant,
    cancellation: Cancellation,
) -> Result<NativePreparation> {
    verify(
        project,
        name,
        herdr_path,
        agent_path,
        execution_home,
        deadline,
        cancellation,
        false,
        Lab::new()?,
    )
}

/// Verify readiness and submit one fixed, content-free diagnostic prompt. Native
/// acknowledgment proves submission only, not completion or the worker protocol.
pub fn verify_interaction(
    project: &Path,
    name: &str,
    herdr_path: &Path,
    agent_path: &Path,
    execution_home: &Path,
    deadline: Instant,
    cancellation: Cancellation,
) -> Result<NativePreparation> {
    verify(
        project,
        name,
        herdr_path,
        agent_path,
        execution_home,
        deadline,
        cancellation,
        true,
        Lab::new()?,
    )
}

pub(super) fn verify(
    project: &Path,
    name: &str,
    herdr_path: &Path,
    agent_path: &Path,
    execution_home: &Path,
    deadline: Instant,
    cancellation: Cancellation,
    interaction: bool,
    mut lab: Lab,
) -> Result<NativePreparation> {
    let deadline = deadline.min(Instant::now() + Duration::from_secs(120));
    let mut preparation = prepare(
        project,
        name,
        herdr_path,
        agent_path,
        execution_home,
        deadline,
        cancellation.clone(),
    )?;
    let project = project.canonicalize()?;
    let guard = crate::execution_guard::RootGuard::exclusive(
        project.parent().context("project root missing")?,
    )?;
    let store_path = project.join(".state/state.db").canonicalize()?;
    let source_file = fs::OpenOptions::new().read(true)
        .custom_flags(libc::O_PATH | libc::O_NOFOLLOW).open(&store_path)?;
    let store_metadata = source_file.metadata()?;
    ensure!(store_metadata.is_file(), "native probe store must be a regular file");
    let source = (store_path, store_metadata.dev(), store_metadata.ino());
    let profile = &preparation.profile;
    let config = crate::migration::read_plan_file(Path::new(&profile.config.path))?;
    ensure!(
        profile.config.digest.as_deref() == Some(digest(&config).as_str()),
        "native probe configuration changed"
    );
    let value: toml::Value =
        toml::from_str(std::str::from_utf8(&config).context("invalid profile encoding")?)
            .map_err(|_| anyhow::anyhow!("invalid profile configuration (contents withheld)"))?;
    let definition: crate::profile_config::ProfileDefinition = value
        .get("profiles")
        .and_then(|v| v.get(name))
        .context("profile missing")?
        .clone()
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid profile fields (contents withheld)"))?;
    ensure!(
        digest(&serde_json::to_vec(&definition)?) == profile.definition_digest,
        "native probe definition changed"
    );
    let wall = definition.validate_gated_preparation(0)?.min(120);
    // Positional text and vendor subcommands can execute a task at startup.
    // Until their interactive mapping is verified, a transport check must not
    // treat arbitrary owner-configured arguments as harmless display options.
    ensure!(
        definition.extra_args.is_empty(),
        "native verification requires empty extra_args until argument mappings are verified"
    );
    crate::supervision::trusted_helpers()?;
    let socket = lab.root.join("native.sock");
    let binary = executable(Path::new(&profile.herdr.path), deadline, &cancellation)?;
    ensure!(
        binary.1 == profile.herdr.digest,
        "native probe executable changed"
    );
    lab.server = Some(
        Command::new("/usr/bin/timeout")
            .args(["--foreground", "--signal=TERM", "--kill-after=5s", "125s"])
            .arg(&profile.herdr.path)
            .arg("server")
            .process_group(0)
            .env_clear()
            .env("HOME", lab.root.join("home"))
            .env("PATH", "/usr/bin:/bin")
            .env("SHELL", "/bin/sh")
            .env("TERM", "xterm-256color")
            .env("LANG", "C.UTF-8")
            .env("XDG_RUNTIME_DIR", lab.root.join("runtime"))
            .env("HERDR_CONFIG_PATH", lab.root.join("config.toml"))
            .env("HERDR_SOCKET_PATH", &socket)
            .current_dir(&lab.root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
    );
    while !fs::symlink_metadata(&socket).is_ok_and(|m| m.file_type().is_socket()) {
        check(deadline, &cancellation)?;
        ensure!(
            lab.server
                .as_mut()
                .context("probe server missing")?
                .try_wait()?
                .is_none(),
            "native probe server exited"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    let api = Api {
        profile,
        socket: socket.clone(),
        session: crate::canonical_worker::session_identity(&socket)?,
        deadline,
        cancellation: cancellation.clone(),
        locks: guard.inherit()?,
    };
    let token = format!("probe-{}", digest(lab.root.as_os_str().as_encoded_bytes()));
    let argv = crate::worker_supervision::isolated_gated_command(
        Path::new(&profile.agent.path),
        &definition.extra_args,
        wall,
        &token,
        execution_home,
    )?;
    let cwd = lab.root.join("work");
    let created=api.call("workspace.create_command",json!({"cwd":cwd,"command":argv,"focus":false,"label":"Profile transport verification","env":{}}))?;
    ensure!(
        created["type"] == "workspace_created" && created["workspace"]["pane_count"] == 1,
        "invalid native probe workspace"
    );
    let pane = created["root_pane"]["pane_id"]
        .as_str()
        .context("probe root pane missing")?;
    let workspace = created["workspace"]["workspace_id"]
        .as_str()
        .context("probe workspace missing")?;
    let before = api.call("pane.get", json!({"pane_id":pane}))?;
    let before = &before["pane"];
    ensure!(
        before["pane_id"] == pane
            && before["workspace_id"] == workspace
            && before["cwd"] == json!(cwd),
        "probe pane route changed"
    );
    let terminal = before["terminal_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("probe terminal missing")?;
    let observation = loop {
        check(deadline, &cancellation)?;
        let processes = api.call("pane.process_info", json!({"pane_id":pane}))?;
        ensure!(
            processes["process_info"]["pane_id"] == pane,
            "probe process route changed"
        );
        if let Some(rows) = processes["process_info"]["foreground_processes"].as_array() {
            ensure!(rows.len() <= 256, "probe process inventory too large");
            let found: Vec<_> = rows.iter().filter(|p| p["argv"] == json!(argv)).collect();
            ensure!(found.len() <= 1, "ambiguous native probe supervisor");
            if let Some(process) = found.first() {
                let pid = u32::try_from(
                    process["pid"]
                        .as_u64()
                        .context("probe process PID missing")?,
                )?;
                if let Ok(observed) = SupervisorObservation::observe(pid, &argv) {
                    break observed;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    lab.worker = Some(observation.identity().clone());
    observation.waiting_gate(&argv)?.check()?;
    let agent = executable(Path::new(&profile.agent.path), deadline, &cancellation)?;
    ensure!(
        agent.1 == profile.agent.digest,
        "native probe agent changed"
    );
    ensure!(
        crate::authority::routine_policy(&project)?
            == (profile.permission_policy.clone(), profile.config.clone()),
        "native probe policy changed"
    );
    let home = fs::symlink_metadata(execution_home)?;
    ensure!(
        execution_home.canonicalize()? == execution_home
            && home.is_dir()
            && home.uid() == unsafe { libc::geteuid() }
            && home.mode() & 0o022 == 0,
        "native probe execution home changed"
    );
    let sent = api.call(
        "pane.send_input",
        json!({"pane_id":pane,"text":format!("{token}\n"),"keys":[]}),
    )?;
    ensure!(sent["type"] == "ok", "uncertain native probe gate release");
    loop {
        check(deadline, &cancellation)?;
        if let Ok(process) =
            observation.agent_process(Path::new(&profile.agent.path), &profile.arguments_digest)
        {
            let native = api.call("pane.get", json!({"pane_id":pane}))?;
            let native = &native["pane"];
            ensure!(
                native["pane_id"] == pane
                    && native["workspace_id"] == workspace
                    && native["terminal_id"] == terminal
                    && native["cwd"] == json!(cwd),
                "native probe terminal changed"
            );
            if native["agent"].as_str() == Some(profile.kind.as_str()) {
                process.check()?;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let interaction = if interaction {
        let route = RuntimeRoute {
            socket: socket.display().to_string(),
            workspace_id: workspace.into(),
            tab_id: before["tab_id"]
                .as_str()
                .context("probe tab missing")?
                .into(),
            pane_id: pane.into(),
            cwd: cwd.display().to_string(),
            ..Default::default()
        };
        Some(verify_prompt(
            &api,
            &observation,
            &route,
            terminal,
            &project,
            &token,
        )?)
    } else {
        None
    };
    let observed_unix_ms = crate::canonical_worker::now();
    let supervisor = observation.identity().clone();
    lab.stop_worker()?;
    let stopped_unix_ms = crate::canonical_worker::now();
    ensure!(
        executable(Path::new(&profile.agent.path), deadline, &cancellation)?.1
            == profile.agent.digest
            && executable(Path::new(&profile.herdr.path), deadline, &cancellation)?.1
                == profile.herdr.digest
            && crate::authority::routine_policy(&project)?
                == (profile.permission_policy.clone(), profile.config.clone()),
        "native probe inputs changed"
    );
    let evidence = NativeEvidence {
        version: 2,
        prepared_profile: preparation.reference.clone(),
        supervisor,
        native_kind: profile.kind.clone(),
        observed_unix_ms,
        stopped_unix_ms,
        interaction,
    };
    apply_evidence(&mut preparation, &evidence)?;
    Ok(NativePreparation {
        preparation,
        evidence,
        source,
        source_file,
    })
}

// Use exactly the same derivation at initial verification and retained replay.
pub(super) fn apply_evidence(preparation: &mut ProfilePreparation, evidence: &NativeEvidence) -> Result<()> {
    let c = &preparation.profile.capabilities;
    ensure!(preparation.profile.reference().map_err(anyhow::Error::msg)? == preparation.reference
        && preparation.reference == evidence.prepared_profile
        && !preparation.launchable && !preparation.protocol_capable && !preparation.certified
        && preparation.profile.workflow_certificate.is_none()
        && [&c.launch, &c.readiness_observation, &c.prompt_submission, &c.stop,
            &c.checkpoint_acknowledgment, &c.structured_usage, &c.resume]
            .iter().all(|c| matches!(c, CapabilityEvidence::Unknown)),
        "native evidence requires its exact unverified baseline");
    let hash = digest(&serde_json::to_vec(evidence)?);
    let reference = VersionedReference {
        id: format!("native-transport-{hash}"),
        revision: 1,
        digest: hash,
    };
    preparation.profile.capabilities.launch = CapabilityEvidence::Supported {
        evidence: reference.clone(),
    };
    preparation.profile.capabilities.stop = CapabilityEvidence::Supported {
        evidence: reference.clone(),
    };
    if evidence.interaction.is_some() {
        preparation.profile.capabilities.readiness_observation = CapabilityEvidence::Supported {
            evidence: reference.clone(),
        };
        preparation.profile.capabilities.prompt_submission = CapabilityEvidence::Supported {
            evidence: reference,
        };
        preparation
            .profile
            .validate_for_launch()
            .map_err(anyhow::Error::msg)?;
        preparation.launchable = true;
    }
    preparation.reference = preparation
        .profile
        .reference()
        .map_err(anyhow::Error::msg)?;
    Ok(())
}

fn ready(api: &Api<'_>, route: &RuntimeRoute, terminal: &str) -> Result<String> {
    fn agent(api: &Api<'_>, route: &RuntimeRoute, terminal: &str) -> Result<()> {
        let result = api.call("agent.list", json!({}))?;
        ensure!(
            result["type"] == "agent_list",
            "invalid native agent inventory"
        );
        let agents = result["agents"]
            .as_array()
            .context("native agent inventory missing")?;
        ensure!(agents.len() <= 256, "native agent inventory exceeds limit");
        let matching: Vec<_> = agents
            .iter()
            .filter(|a| a["pane_id"].as_str() == Some(&route.pane_id))
            .collect();
        ensure!(
            matching.len() == 1,
            "native probe agent absent or ambiguous"
        );
        crate::canonical_worker::validate_native_agent(
            matching[0],
            route,
            terminal,
            &api.profile.kind,
            None,
            true,
        )
    }
    agent(api, route, terminal)?;
    let explanation = api.call("agent.explain", json!({"target":route.pane_id}))?;
    ensure!(
        explanation["type"] == "agent_explain",
        "invalid native readiness response"
    );
    crate::canonical_worker::validate_visible_readiness(
        &api.profile.kind,
        &explanation["explain"],
    )?;
    let manifest = explanation["explain"]["manifest_version"]
        .as_str()
        .context("readiness manifest missing")?
        .to_owned();
    agent(api, route, terminal)?;
    Ok(manifest)
}

fn verify_prompt(
    api: &Api<'_>,
    observation: &SupervisorObservation,
    route: &RuntimeRoute,
    terminal: &str,
    project: &Path,
    token: &str,
) -> Result<InteractionEvidence> {
    let wait_until = api.deadline.min(Instant::now() + Duration::from_secs(30));
    let mut readiness_failure = None;
    loop {
        if let Err(error) = check(wait_until, &api.cancellation) {
            #[cfg(test)]
            if let Ok(screen) = api.call("pane.read", json!({"pane_id":route.pane_id,"source":"visible"})) {
                // Report only fixed diagnostic categories, never terminal text,
                // login details, tokens or provider responses.
                let text = screen.to_string().to_lowercase();
                for category in ["trust", "theme", "sign in", "log in", "update available", "press enter", "welcome", "network", "model"] {
                    if text.contains(category) { eprintln!("Native readiness screen category: {category}"); }
                }
            }
            return Err(error).context(format!("native probe did not establish prompt readiness: {}", readiness_failure.as_deref().unwrap_or("no readiness observation")));
        }
        observation
            .agent_process(
                Path::new(&api.profile.agent.path),
                &api.profile.arguments_digest,
            ).context("native probe process unavailable before prompt readiness")?
            .check().context("native probe process changed before prompt readiness")?;
        match ready(api, route, terminal) {
            Ok(_) => break,
            Err(error) => readiness_failure = Some(format!("{error:#}")),
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let prompt = format!(
        "Transport verification only. Do not use tools, inspect files, or change anything. Reply with exactly: {token}"
    );
    let mut manifest = None;
    let response = api.call_checked(
        "agent.prompt",
        json!({"target":route.pane_id,"text":prompt}),
        || {
            manifest = Some(ready(api, route, terminal)?);
            let process = observation.agent_process(
                Path::new(&api.profile.agent.path),
                &api.profile.arguments_digest,
            )?;
            ensure!(
                executable(
                    Path::new(&api.profile.agent.path),
                    api.deadline,
                    &api.cancellation
                )?
                .1 == api.profile.agent.digest,
                "native probe executable changed before prompt"
            );
            ensure!(
                crate::authority::routine_policy(project)?
                    == (
                        api.profile.permission_policy.clone(),
                        api.profile.config.clone()
                    ),
                "native probe policy changed before prompt"
            );
            process.check()
        },
    )?;
    // There is deliberately no prompt retry after rejection, loss or ambiguity.
    validate_prompt_response(&response, route, terminal, &api.profile.kind)?;
    observation
        .agent_process(
            Path::new(&api.profile.agent.path),
            &api.profile.arguments_digest,
        )?
        .check()?;
    Ok(InteractionEvidence {
        session: api.session.clone(),
        terminal: terminal.into(),
        readiness_manifest: manifest.context("readiness observation missing")?,
        prompt_digest: digest(prompt.as_bytes()),
        acknowledged_unix_ms: crate::canonical_worker::now(),
    })
}

pub(super) fn validate_prompt_response(
    response: &Value,
    route: &RuntimeRoute,
    terminal: &str,
    kind: &str,
) -> Result<()> {
    ensure!(
        response["type"].as_str() == Some("agent_prompted"),
        "uncertain native probe prompt acknowledgment"
    );
    crate::canonical_worker::validate_native_agent(
        &response["agent"],
        route,
        terminal,
        kind,
        None,
        false,
    )
}

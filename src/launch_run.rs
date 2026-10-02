//! `launch PROJECT run`: the operator sequence that takes one task on a migrated
//! project from nothing to a reserved canonical attempt, using only the
//! existing library steps. Every step checks the current state first, so a
//! rerun skips what is already done, and the first failing step stops the run
//! with its name. The owner signs through `ssh-keygen -Y sign` with a key path
//! the operator passes; this program never reads a private key itself.
use crate::paths::Ctx;
use anyhow::{Context, Result, bail, ensure};
use herdr_projects::{
    authority, domain::{ProjectState, RuntimeRoute, TaskId, VersionedReference}, execution_guard::GatedSpawn,
    launch_preparation::{self, LaunchSelection}, migration, runtime,
};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::{fs::DirBuilderExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub struct Args {
    pub task: String,
    pub profile: String,
    pub repository: PathBuf,
    pub sign_with: Option<PathBuf>,
    pub validity_seconds: u64,
    pub title: Option<String>,
    /// Planning task: its single deliverable, a Markdown document under `docs/`.
    pub plan_output: Option<String>,
    /// The task's instructions (appended to PROJECT.md in the retained brief).
    pub prompt_file: Option<PathBuf>,
    /// A complete unsigned contract document, instead of the planning one.
    pub contract_file: Option<PathBuf>,
    /// Existing, not-checked-out branch that verified results integrate into.
    pub integration_ref: Option<String>,
    pub base: String,
    pub max_active_workers: u32,
    /// A Herdr server the operator already runs for this task (its control
    /// socket); by default a dedicated server is started.
    pub herdr_socket: Option<PathBuf>,
    /// Stop after the binding is reconciled and the project active, before any
    /// attempt is reserved (see the binding note in `steps`).
    pub prepare_only: bool,
}

struct Step {
    name: &'static str,
    outcome: &'static str,
    detail: Value,
}

struct Run<'a> {
    ctx: &'a Ctx<'a>,
    project: PathBuf,
    slug: String,
    steps: Vec<Step>,
}

impl Run<'_> {
    fn done(&mut self, name: &'static str, detail: Value) {
        self.steps.push(Step { name, outcome: "done", detail });
    }
    fn skipped(&mut self, name: &'static str, detail: Value) {
        self.steps.push(Step { name, outcome: "already_done", detail });
    }
    fn head(&self) -> Result<u64> {
        Ok(runtime::snapshot(&self.project)?.head)
    }
    fn dir(&self, task: &str) -> Result<PathBuf> {
        let dir = self.ctx.root.join(".herdr-run").join(format!("{}-{task}", self.slug));
        fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        Ok(dir)
    }
}

fn git(repository: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("/usr/bin/git").arg("-C").arg(repository).args(args).output_gated()?;
    ensure!(output.status.success(), "git {} failed in {}", args.join(" "), repository.display());
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

/// Sign `file` for `namespace` into `file.sig` (a key the operator names).
fn sign(key: &Path, namespace: &str, file: &Path) -> Result<PathBuf> {
    let signature = PathBuf::from(format!("{}.sig", file.display()));
    let _ = fs::remove_file(&signature);
    // Inherited terminal: a passphrase-protected key prompts the operator.
    let status = Command::new("ssh-keygen").args(["-Y", "sign", "-n", namespace, "-f"]).arg(key).arg(file).stdout(Stdio::null()).status_gated()
        .context("ssh-keygen could not be started")?;
    ensure!(status.success() && signature.is_file(), "ssh-keygen did not sign {}", file.display());
    Ok(signature)
}

fn planning_contract(args: &Args, repository: &Path, head: u64, kind: &str, project: &Path) -> Result<Vec<u8>> {
    let output = args.plan_output.as_deref().context("planning output missing")?;
    ensure!(
        output.starts_with("docs/") && output.ends_with(".md") && !output.contains("..") && !output.contains("//")
            && output.len() <= 200 && output.bytes().all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b)),
        "--plan-output must be a Markdown path under docs/ (letters, digits, `._-/`)"
    );
    let format = git(repository, &["rev-parse", "--show-object-format"])?;
    let base = git(repository, &["rev-parse", "--verify", &format!("{}^{{commit}}", args.base)])?;
    let route = if args.integration_ref.is_some() { "verify_then_integrate" } else { "verify_only" };
    let store = project.join(".state/state.db").canonicalize()?;
    // The check passes only for a document that exists and has content.
    let policy = serde_json::to_string(&json!({"version":1,"checks":["/usr/bin/git","grep","--quiet","--no-index","-e",".","--",output]}))?;
    let mut bytes = serde_json::to_vec_pretty(&json!({
        "version":3,"outputs":[{"path":output,"kind":"git_file"}],"scope":{"paths":[{"path":output,"access":"write"}]},
        "project_store":store,"expected_head":head,"task_id":args.task,"contract_revision":1,
        "deliverable":format!("The planning document {output}, written in the working tree."),
        "non_goals":"No code, test or configuration change; no file other than the deliverable.",
        "acceptance_policies":[{"id":"document-present","text":policy}],
        "repository":repository,"base_oid":base,"object_format":format,"dependencies":[],"capability_flags":[],
        "profile_kind":kind,"retry_class":"none","result_schema_id":"result-v1","route":route,
        "authority":authority::policy_reference(project)?
    }))?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Record current observations and make the project active, unless it already
/// is and needs no reconciliation (or `force` after a new binding).
fn activate(run: &mut Run, name: &'static str, force: bool) -> Result<()> {
    let project = run.project.clone();
    let control = runtime::snapshot(&project)?.control.context("project has no control state")?;
    if !force && control.state == ProjectState::Active && !control.reconciliation_required {
        run.skipped(name, json!({"state":"active"}));
        return Ok(());
    }
    crate::reconcile_live::run(run.ctx, &project, true)?;
    let control = runtime::snapshot(&project)?.control.context("project has no control state")?;
    if control.state != ProjectState::Active {
        let config = std::path::absolute(run.ctx.config_dir.join("config.toml"))?;
        runtime::set_state(&project, run.head()?, control.revision, ProjectState::Active, &config)?;
    }
    run.done(name, json!({"state":"active"}));
    Ok(())
}

fn herdr_server(run: &mut Run, herdr: &Path, task: &str, existing: Option<&Path>) -> Result<PathBuf> {
    use std::os::unix::fs::FileTypeExt;
    if let Some(socket) = existing {
        ensure!(socket.is_absolute() && fs::symlink_metadata(socket).is_ok_and(|m| m.file_type().is_socket()), "--herdr-socket must be the absolute path of an existing Herdr control socket");
        run.done("herdr_server", json!({"socket":socket,"managed_by":"operator"}));
        return Ok(socket.to_owned());
    }
    let directory = run.dir(task)?.join("herdr");
    for part in ["", "home", "runtime"] {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(directory.join(part))?;
    }
    // The socket lives in a short private directory (sun_path is 108 bytes);
    // logs and configuration stay beside the run.
    let socket = herdr_projects::short_socket::stable(&directory)?.join("s");
    herdr_projects::short_socket::check_length(&socket)?;
    if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
        run.skipped("herdr_server", json!({"socket":socket}));
        return Ok(socket);
    }
    let _ = fs::remove_file(&socket);
    fs::write(
        directory.join("config.toml"),
        "onboarding = false\n[terminal]\ndefault_shell = '/bin/sh'\nshell_mode = 'non_login'\n[update]\nversion_check = false\nmanifest_check = false\n",
    )?;
    let log = fs::File::create(directory.join("server.log"))?;
    // Its own process group: the server outlives this command.
    Command::new(herdr)
        .arg("server")
        .env_clear()
        .env("HOME", directory.join("home"))
        .env("PATH", "/usr/bin:/bin")
        .env("SHELL", "/bin/sh")
        .env("TERM", "xterm-256color")
        .env("LANG", "C.UTF-8")
        .env("XDG_RUNTIME_DIR", directory.join("runtime"))
        .env("HERDR_CONFIG_PATH", directory.join("config.toml"))
        .env("HERDR_SOCKET_PATH", &socket)
        .current_dir(&directory)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .process_group(0)
        .spawn_gated()
        .context("Herdr server could not be started")?;
    let deadline = Instant::now() + Duration::from_secs(20);
    while std::os::unix::net::UnixStream::connect(&socket).is_err() {
        ensure!(Instant::now() < deadline, "Herdr server did not open {} (see server.log beside it)", socket.display());
        std::thread::sleep(Duration::from_millis(50));
    }
    run.done("herdr_server", json!({"socket":socket,"log":directory.join("server.log")}));
    Ok(socket)
}

pub fn run(ctx: &Ctx, slug: &str, args: Args) -> Result<Value> {
    let project = ctx.root.join(slug).canonicalize().with_context(|| format!("project {slug} not found"))?;
    let mut run = Run { ctx, project: project.clone(), slug: slug.to_owned(), steps: Vec::new() };
    match steps(&mut run, &args) {
        Ok(report) => Ok(report),
        Err(error) => {
            let failed = run.steps.len() + 1;
            let done: Vec<String> = run.steps.iter().map(|s| format!("{} ({})", s.name, s.outcome)).collect();
            bail!("launch run stopped at step {failed} ({error:#}); completed before it: [{}]. Fix the cause and rerun the same command; finished steps are skipped.", done.join(", "))
        }
    }
}

fn steps(run: &mut Run, args: &Args) -> Result<Value> {
    let ctx = run.ctx;
    let project = run.project.clone();
    let task_id = TaskId::new(args.task.clone()).map_err(anyhow::Error::msg)?;
    let herdr = PathBuf::from(ctx.env.herdr_bin());
    ensure!(herdr.is_absolute(), "set HERDR_BIN_PATH to the absolute path of the Herdr executable the profile was verified with");
    let repository = args.repository.canonicalize().context("--repository must exist")?;
    ensure!(args.plan_output.is_some() != args.contract_file.is_some()
        || runtime::task_contract(&project, &task_id)?.is_some(),
        "give --plan-output (planning task) or --contract-file (your own contract), not both");

    // 1. The retained, launchable profile evidence.
    let (profile, kind) = herdr_projects::store::SqliteStore::open(&project.join(".state/state.db"))?
        .latest_launchable_native_profile(&args.profile)?
        .with_context(|| format!("no launchable evidence retained for profile {}: run `profile verify-interaction {} {} --retain ...` first", args.profile, run.slug, args.profile))?;
    run.done("profile_evidence", json!({"profile":profile,"kind":kind}));

    // 2. The owner configuration must be acknowledged by an active project
    // before a signed contract can be installed.
    activate(run, "project_control", false)?;

    // 3. The task and its signed contract.
    let snapshot = runtime::snapshot(&project)?;
    if snapshot.tasks.iter().any(|t| t.id == task_id) {
        run.skipped("task", json!({"task":args.task}));
    } else {
        let head = runtime::add_task(&project, task_id.clone(), args.title.clone().unwrap_or_else(|| args.task.clone()), snapshot.head)?;
        run.done("task", json!({"task":args.task,"head":head}));
    }
    if let Some(installed) = runtime::task_contract(&project, &task_id)? {
        run.skipped("contract", json!({"installed":installed}));
    } else {
        let dir = run.dir(&args.task)?;
        let bytes = match &args.contract_file {
            Some(path) => migration::read_plan_file(path)?,
            None => planning_contract(args, &repository, run.head()?, &kind, &project)?,
        };
        let document = dir.join("contract.json");
        fs::write(&document, &bytes)?;
        let key = args.sign_with.as_deref().with_context(|| format!("the contract needs the owner's signature: pass --sign-with KEY, or sign {} with `ssh-keygen -Y sign -n {} -f KEY` and install it with `task contract put`", document.display(), authority::CONTRACT_SIGNATURE_NAMESPACE))?;
        let signature = sign(key, authority::CONTRACT_SIGNATURE_NAMESPACE, &document)?;
        let installed = authority::import_contract(&project, &document, &signature)?;
        run.done("contract", json!({"task":installed.task_id,"revision":installed.contract_revision,"digest":installed.digest}));
    }

    // 4. Capacity, queue and the integration target.
    let snapshot = runtime::snapshot(&project)?;
    let scheduler = snapshot.scheduler.as_ref().context("project has no scheduler state: migrate it first")?;
    if snapshot.scheduler.as_ref().is_some_and(|s| s.queue.iter().any(|q| q.task == task_id)) {
        run.skipped("queue", json!({"task":args.task}));
    } else {
        let task = snapshot.tasks.iter().find(|t| t.id == task_id).context("task missing")?;
        let request = herdr_projects::domain::QueueRequest { priority: 0, dependencies: vec![] };
        let head = runtime::queue_task(&project, &task_id, task.revision, snapshot.head, &request)?;
        run.done("queue", json!({"head":head}));
    }
    let scheduler_policy = &scheduler.policy;
    if scheduler_policy.max_active_workers >= args.max_active_workers {
        run.skipped("scheduler_capacity", json!({"max_active_workers":scheduler_policy.max_active_workers}));
    } else {
        let head = runtime::scheduler_policy(&project, run.head()?, scheduler_policy.revision, args.max_active_workers, scheduler_policy.max_attempts_per_task.max(1))?;
        run.done("scheduler_capacity", json!({"max_active_workers":args.max_active_workers,"head":head}));
    }
    if let Some(reference) = &args.integration_ref {
        herdr_projects::integration::configure_project(&project, &repository, reference)?;
        herdr_projects::store::set_project_result_automation(&project, run.head()?, Some(true), Some(true))?;
        run.done("integration_target", json!({"repository":repository,"reference":reference,"automation":"verify and integrate on"}));
    }

    // 5. Herdr server, binding, reconciliation and activation.
    let socket = herdr_server(run, &herdr, &args.task, args.herdr_socket.as_deref())?;
    let snapshot = runtime::snapshot(&project)?;
    let existing = snapshot.runtime_bindings.iter().find(|b| b.task.as_ref() == Some(&task_id) && b.identity.socket == socket.display().to_string());
    let (binding, created) = match existing {
        Some(binding) => {
            run.skipped("binding", json!({"binding":binding.id}));
            (binding.id.clone(), false)
        }
        None => {
            // A new runtime binding pauses the project, and it resumes only when
            // no attempt is unfinished. Refuse before the binding exists rather
            // than leave a project with a live worker paused.
            if let Some(busy) = snapshot.tasks.iter().find(|t| t.active_attempt.is_some()) {
                bail!("task {} already has a reserved or running attempt, and a new runtime binding pauses the project until every attempt is finished; prepare every task first with --prepare-only, then reserve them", busy.id.as_str());
            }
            let task = snapshot.tasks.iter().find(|t| t.id == task_id).context("task missing")?;
            let route = RuntimeRoute { socket: socket.display().to_string(), cwd: repository.display().to_string(), ..Default::default() };
            let change = runtime::create_binding(&project, Some(&task_id), Some(task.revision), snapshot.head, &route)?;
            run.done("binding", json!({"binding":change.binding.id,"socket":socket}));
            (change.binding.id, true)
        }
    };
    activate(run, "reconcile_and_activate", created)?;

    if args.prepare_only {
        return Ok(report(run, &args.task, &profile, &kind, &herdr, &socket, None, None));
    }

    // 6. Already reserved? Then stop here: a rerun never reserves a second attempt.
    let snapshot = runtime::snapshot(&project)?;
    if let Some(attempt) = snapshot.tasks.iter().find(|t| t.id == task_id).and_then(|t| t.active_attempt.clone()) {
        run.skipped("reserve", json!({"attempt":attempt}));
        return Ok(report(run, &args.task, &profile, &kind, &herdr, &socket, Some(attempt.as_str().to_owned()), None));
    }

    // 7. Knowledge snapshot, draft, owner approval, import, reservation.
    let dir = run.dir(&args.task)?;
    let mut instructions = String::from_utf8(migration::read_plan_file(&project.join("PROJECT.md"))?).map_err(|_| anyhow::anyhow!("project instructions are not UTF-8"))?;
    if let Some(path) = &args.prompt_file {
        let text = String::from_utf8(migration::read_plan_file(path)?).map_err(|_| anyhow::anyhow!("--prompt-file is not UTF-8"))?;
        instructions.push_str(&format!("\n\n---\n\n# Task {}\n\n{text}\n", args.task));
    }
    if let Some(output) = &args.plan_output {
        instructions.push_str(&format!(
            "\n## Deliverable\n\nWrite the document `{output}` in the current directory (a disposable git worktree), with the plan as its content. Do not commit, do not change any other file, do not push and do not use the network. When the document is complete, stop and reply DONE.\n"));
    }
    let knowledge = {
        let _guard = herdr_projects::memory::mutation_guard(&project)?;
        let resolved = crate::agents::resolve::resolve(&args.profile, &ctx.config_dir.join("config.toml"), None)?;
        let request: herdr_projects::domain::SnapshotRequest = serde_json::from_value(json!({
            "schema_version":1,"task_id":args.task,"profile":args.profile,"domains":[],"paths":[],"pinned_keys":[],"sensitivity":"default"}))?;
        let mut memory = herdr_projects::memory::MemoryStore::from_sqlite(migration::open_active(&project)?, project.join(".state/objects"));
        let created = memory.create_worker_snapshot(request, &resolved.name, &resolved.definition_digest, Some(&resolved.config_digest), resolved.budget.soft_input_chars, &instructions, jiff::Timestamp::now().as_millisecond(), None)?;
        serde_json::to_value(&created)?
    };
    let selection = LaunchSelection {
        task: task_id.clone(),
        binding,
        profile: profile.clone(),
        knowledge: VersionedReference { id: knowledge["id"].as_str().context("snapshot id missing")?.to_owned(), revision: 1, digest: knowledge["manifest_hash"].as_str().context("snapshot digest missing")?.to_owned() },
        repositories: vec![repository.clone()],
        reason: None,
        note: None,
    };
    run.done("knowledge_snapshot", json!({"snapshot":selection.knowledge}));
    let deadline = Instant::now() + herdr_projects::profile_preparation::BUDGET;
    let drafted = launch_preparation::draft(&project, &selection, run.head()?, Duration::from_secs(args.validity_seconds), deadline, Default::default())?;
    let approval = dir.join("approval.json");
    fs::write(&approval, serde_json::to_vec_pretty(&drafted.approval)?)?;
    run.done("draft", json!({"approval_document":approval,"brief_chars":drafted.brief.prompt_chars}));
    let key = args.sign_with.as_deref().with_context(|| format!("the launch approval needs the owner's signature: pass --sign-with KEY, or sign {} with `ssh-keygen -Y sign -n {} -f KEY`, then `approval import` and `launch reserve`", approval.display(), authority::SIGNATURE_NAMESPACE))?;
    let signature = sign(key, authority::SIGNATURE_NAMESPACE, &approval)?;
    let imported = authority::import_signed(&project, &approval, &signature, run.head()?)?;
    run.done("approval_import", json!({"approval":imported}));
    let approval_reference = VersionedReference { id: format!("approval-{}", imported.digest), revision: 1, digest: imported.digest.clone() };
    let reservation = launch_preparation::reserve(&project, &selection, &approval_reference, run.head()?, deadline, Default::default())?;
    anyhow::ensure!(reservation.record.inputs == drafted.inputs, "the reservation does not carry the drafted inputs");
    let attempt = reservation.record.attempt.clone();
    let worktree = herdr_projects::domain::worktree_plans(&drafted.inputs, &attempt).map_err(anyhow::Error::msg)?.into_iter().next().map(|p| p.path);
    run.done("reserve", json!({"attempt":attempt}));
    Ok(report(run, &args.task, &profile, &kind, &herdr, &socket, Some(attempt.as_str().to_owned()), worktree))
}

#[allow(clippy::too_many_arguments)]
fn report(run: &Run, task: &str, profile: &VersionedReference, kind: &str, herdr: &Path, socket: &Path, attempt: Option<String>, worktree: Option<String>) -> Value {
    json!({
        "project":run.slug,"task":task,"kind":kind,"profile":profile,"attempt":attempt,"worktree":worktree,
        "herdr_socket":socket,
        "steps":run.steps.iter().map(|s| json!({"step":s.name,"outcome":s.outcome,"detail":s.detail})).collect::<Vec<_>>(),
        "next":[
            format!("The ticker launches the worker; run it with HERDR_BIN_PATH={} so it uses the verified Herdr.", herdr.display()),
            format!("Watch it with `scheduler {} inspect` and `operations {} inspect`; the worker's edits are captured with `result {} capture <attempt>`.", run.slug, run.slug, run.slug)
        ]
    })
}

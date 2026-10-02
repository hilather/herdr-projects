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

/// Beside a dedicated server's logs: its process id and socket.
const SERVER_RECORD: &str = "server.json";

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

/// The closing section of the brief: commit the declared outputs and submit
/// the result through the worker's submission spool, with every value the
/// contract fixes already filled in. Without it nothing ever enters
/// verification or integration.
fn finish_instructions(root: &Path, slug: &str, contract: &Value, reference: &VersionedReference) -> Result<String> {
    let text = |key: &str| contract[key].as_str().with_context(|| format!("contract field {key} missing"));
    let outputs: Vec<&str> = contract["outputs"].as_array().context("contract outputs missing")?.iter().filter_map(|o| o["path"].as_str()).collect();
    ensure!(!outputs.is_empty() && outputs.len() <= 8, "a launched task declares between one and eight outputs");
    let safe = |value: &str| !value.is_empty() && !value.contains(['$', '`', '\\', '\'', '"', '\n']);
    let (task, repository, base, format) = (text("task_id")?, text("repository")?, text("base_oid")?, text("object_format")?);
    ensure!([task, repository, base, format, reference.digest.as_str()].iter().all(|v| safe(v)) && safe(&root.display().to_string()) && outputs.iter().all(|o| safe(o)),
        "a path or value in the task contract contains a character the submission script cannot carry");
    let mut script = String::from("set -eu\nattempt=$(basename \"$HERDR_PROJECTS_SUBMISSION_SPOOL\")\n");
    script.push_str(&format!("git add --{}\n", outputs.iter().map(|o| format!(" '{o}'")).collect::<String>()));
    script.push_str("git -c user.name=worker -c user.email=worker@invalid commit -q -m 'Deliverable' || true\n");
    script.push_str(&format!("candidate=$(git rev-parse HEAD)\n[ \"$candidate\" != '{base}' ] || {{ echo 'nothing is committed: write the deliverable first' >&2; exit 1; }}\n"));
    let mut manifest = Vec::new();
    for (index, output) in outputs.iter().enumerate() {
        script.push_str(&format!("blob{index}=$(git rev-parse \"HEAD:{output}\")\n"));
        manifest.push(format!(r#"{{"path":"{output}","oid":"$blob{index}"}}"#));
    }
    // The verifier materializes the base and the candidate in a fresh repository
    // from the staged objects alone, so every tree and blob of both is listed.
    script.push_str(&"list() { git rev-list --objects --no-object-names --no-walk 'BASE' \"$candidate\" | sort -u; }\n\
n=$(list | wc -l)\n[ \"$n\" -le LIMIT ] || { echo \"the base and candidate trees hold $n objects; a submission carries at most LIMIT\" >&2; exit 1; }\n\
objects=$(list | sed 's#^\\(..\\)\\(.*\\)$#{\"oid\":\"\\1\\2\",\"relative_path\":\"\\1/\\2\"}#' | paste -sd, -)\n"
        .replace("BASE", base).replace("LIMIT", &herdr_projects::store::SUBMISSION_OBJECT_LIMIT.to_string()));
    script.push_str("key=result-$(printf %s \"$attempt\" | cut -c1-100)\ndocument=$(mktemp)\ncat > \"$document\" <<EOF\n");
    script.push_str(&format!(
        r#"{{"idempotency_key":"$key","task_id":"{task}","contract_revision":{},"contract_digest":"{}","attempt_id":"$attempt","repository":"{repository}","base_oid":"{base}","candidate_oid":"$candidate","object_format":"{format}","artifact_manifest":[{}],"claimed_checks":[],"objects":[$objects]}}"#,
        reference.revision, reference.digest, manifest.join(",")));
    script.push_str(&format!("\nEOF\nherdr-projects --root '{}' result {slug} submit --input-file \"$document\"\n", root.display()));
    Ok(format!(
        "\n## When the deliverable is complete: submit it\n\nA result that is not submitted is never verified or integrated. Run exactly this script in the current directory. It commits the declared output(s) on your attempt branch and submits the result through your submission spool (the product's own command; it needs no network and is safe to run again). It must print a submission receipt containing `submission_id`; if it fails, fix what it reports and run it again. Only then stop and reply DONE.\n\n```sh\n{script}```\n"))
}

/// Record current observations and make the project active, unless it already
/// is, needs no reconciliation and acknowledges the owner configuration as it is
/// now (or `force` after a new binding). A configuration edited since control
/// was last made active is re-acknowledged: the signed contract, approvals and
/// reservations all require the active control to carry the current digest.
fn activate(run: &mut Run, name: &'static str, force: bool) -> Result<()> {
    let project = run.project.clone();
    let config = std::path::absolute(run.ctx.config_dir.join("config.toml"))?;
    let current = migration::config_reference(&config)?.digest;
    let control = runtime::snapshot(&project)?.control.context("project has no control state")?;
    if !force && control.state == ProjectState::Active && !control.reconciliation_required && control.config_digest == current {
        run.skipped(name, json!({"state":"active"}));
        return Ok(());
    }
    let reacknowledged = control.state == ProjectState::Active && control.config_digest != current;
    crate::reconcile_live::run(run.ctx, &project, true)?;
    let control = runtime::snapshot(&project)?.control.context("project has no control state")?;
    if control.state != ProjectState::Active || control.config_digest != current {
        runtime::set_state(&project, run.head()?, control.revision, ProjectState::Active, &config)?;
    }
    run.done(name, json!({"state":"active","owner_configuration_reacknowledged":reacknowledged}));
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
    let child = Command::new(herdr)
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
    // The record `launch stop` and the ticker's sweep use to find (and prove
    // they have found) this server again.
    fs::write(directory.join(SERVER_RECORD), serde_json::to_vec_pretty(&json!({"pid":child.id(),"socket":socket}))?)?;
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
    let mut store = herdr_projects::store::SqliteStore::open(&project.join(".state/state.db"))?;
    let (profile, kind) = store
        .latest_launchable_native_profile(&args.profile)?
        .with_context(|| format!("no launchable evidence retained for profile {}: run `profile verify-interaction {} {} --retain ...` first", args.profile, run.slug, args.profile))?;
    // A worker that cannot log in would only 401 after launch: refuse now.
    let retained = store.native_profile_report(&profile)?.context("retained native profile not found")?;
    let frozen: herdr_projects::domain::FrozenProfile = serde_json::from_value(retained["preparation"]["profile"].clone()).context("retained native profile is unreadable")?;
    herdr_projects::profile_config::check_worker_login(&frozen, &project)?;
    drop(store);
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
            "\n## Deliverable\n\nWrite the document `{output}` in the current directory (a disposable git worktree), with the plan as its content. Do not change any other file, do not push and do not use the network.\n"));
    }
    let contract: Value = serde_json::from_slice(&fs::read(dir.join("contract.json")).context("the installed contract document is missing from the run directory")?)?;
    let reference = runtime::task_contract(&project, &task_id)?.context("the task has no installed contract")?;
    instructions.push_str(&finish_instructions(&ctx.root, run.slug.as_str(), &contract, &reference)?);
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
            format!("Watch it with `scheduler {} inspect` and `operations {} inspect`. The worker's brief ends with the submission it must make; if it finishes without submitting, `result {} submit-captured <attempt>` captures its worktree and records the submission, and automatic verification and integration take it from there.", run.slug, run.slug, run.slug),
            format!("When the task is finished the ticker stops its dedicated Herdr server; `launch {} stop --task {task}` does it explicitly.", run.slug)
        ]
    })
}

/// Whether `pid` is still the dedicated server `launch run` started on `socket`:
/// a `herdr server` process of ours whose environment names that socket. A
/// recycled process id never matches.
fn is_our_server(pid: i32, socket: &Path) -> bool {
    let read = |name: &str| fs::read(format!("/proc/{pid}/{name}")).unwrap_or_default();
    let cmdline = read("cmdline");
    let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).filter(|a| !a.is_empty()).collect();
    let wanted = format!("HERDR_SOCKET_PATH={}", socket.display());
    args.last() == Some(&b"server".as_slice()) && read("environ").split(|b| *b == 0).any(|e| e == wanted.as_bytes())
}

fn signal(pid: i32, signal: i32) -> bool {
    // SAFETY: kill(2) on a process id this module just proved to be its server.
    unsafe { libc::kill(pid, signal) == 0 }
}

/// Stop the dedicated Herdr server of `task` and remove its socket directory.
/// Refuses while the task's attempt still holds its worker, unless `force`.
/// A task with no dedicated server (an operator-managed `--herdr-socket`) is
/// left alone. Idempotent.
pub fn stop(ctx: &Ctx, slug: &str, task: &str, force: bool) -> Result<Value> {
    let project = ctx.root.join(slug).canonicalize().with_context(|| format!("project {slug} not found"))?;
    let task_id = TaskId::new(task.to_owned()).map_err(anyhow::Error::msg)?;
    let directory = ctx.root.join(".herdr-run").join(format!("{slug}-{task}")).join("herdr");
    let record = directory.join(SERVER_RECORD);
    let Ok(text) = fs::read_to_string(&record) else {
        return Ok(json!({"task":task,"stopped":false,"reason":"no dedicated Herdr server is recorded for this task"}));
    };
    let record_value: Value = serde_json::from_str(&text).context("server record is unreadable")?;
    let snapshot = runtime::snapshot(&project)?;
    let held = snapshot.tasks.iter().find(|t| t.id == task_id).and_then(|t| t.active_attempt.as_ref())
        .and_then(|id| snapshot.attempts.iter().find(|a| &a.id == id)).filter(|a| a.retains_capacity());
    if let (Some(attempt), false) = (held, force) {
        bail!("attempt {} of task {task} still holds its worker; stop it first (or pass --force)", attempt.id.as_str());
    }
    let pid = record_value["pid"].as_i64().context("server record has no pid")? as i32;
    let socket = PathBuf::from(record_value["socket"].as_str().context("server record has no socket")?);
    let mut stopped = false;
    if pid > 1 && is_our_server(pid, &socket) {
        signal(pid, libc::SIGTERM);
        let until = Instant::now() + Duration::from_secs(5);
        while Path::new(&format!("/proc/{pid}")).exists() && is_our_server(pid, &socket) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(50));
        }
        if is_our_server(pid, &socket) {
            signal(pid, libc::SIGKILL);
            std::thread::sleep(Duration::from_millis(100));
        }
        stopped = true;
    }
    // The socket directory is the short private one named by the socket itself.
    if let Some(dir) = socket.parent().filter(|d| d.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with('r')) && socket.file_name().is_some_and(|n| n == "s")) {
        let _ = fs::remove_file(&socket);
        let _ = fs::remove_dir(dir);
    }
    fs::remove_file(&record)?;
    Ok(json!({"task":task,"stopped":stopped,"socket":socket,"socket_directory_removed":!socket.exists()}))
}

/// Stop the dedicated server of every task of `slug` whose attempts are all
/// finished (worker termination observed). The ticker calls this each pass; it
/// does nothing unless a server record exists. One line per stopped server or
/// failure, for the ticker log.
pub fn sweep_servers(ctx: &Ctx, slug: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let prefix = format!("{slug}-");
    let Ok(entries) = fs::read_dir(ctx.root.join(".herdr-run")) else { return lines };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(task) = name.strip_prefix(&prefix) else { continue };
        if !entry.path().join("herdr").join(SERVER_RECORD).is_file() {
            continue;
        }
        let Ok(project) = ctx.root.join(slug).canonicalize() else { continue };
        let Ok(snapshot) = runtime::snapshot(&project) else { continue };
        let Some(record) = snapshot.tasks.iter().find(|t| t.id.as_str() == task) else { continue };
        // A task that never reserved an attempt has not used its server yet.
        if record.active_attempt.is_some() || !snapshot.attempts.iter().any(|a| a.task.as_str() == task) {
            continue;
        }
        match stop(ctx, slug, task, false) {
            Ok(report) if report["stopped"] == true => lines.push(format!("stopped the dedicated Herdr server of finished task {task}")),
            Ok(_) => {}
            Err(error) => lines.push(format!("dedicated Herdr server of task {task} not stopped: {error:#}")),
        }
    }
    lines
}

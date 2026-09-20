//! The ticker: one background loop per projects root.
//!
//! Everything it does is "check on an interval, compare with last time, act".
//! It exits on request through a stop file, never through signals.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::coordinator::{self, MAX_LAUNCH_ATTEMPTS};
use crate::herdr::{Agent, Herdr, Pane};
use crate::paths::Ctx;
use crate::project::{self, Project, Status};
use crate::steps::{self, Memory, Transition};
use crate::{inbox, thread, threads};

pub const TICK: Duration = Duration::from_secs(15);
const STOP_WAIT: Duration = Duration::from_secs(60);
const IDLE_EXIT: Duration = Duration::from_secs(300);
const LOG_CAP: u64 = 1_000_000;

fn lock_path(root: &Path) -> PathBuf {
    root.join(".ticker.lock")
}

fn stop_path(root: &Path) -> PathBuf {
    root.join(".ticker.stop")
}

fn log_path(root: &Path) -> PathBuf {
    root.join(".ticker.log")
}

/// What the lock holder writes into the lock file, for `ticker status` and
/// `doctor`. The pid is for display only; nothing signals it.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct Info {
    pub version: String,
    pub pid: u32,
    pub root: String,
    pub started: String,
    /// Where the ticker resolves its tools from its own environment, which may
    /// differ from the user's shell.
    pub tools: Vec<(String, String)>,
}

#[derive(Debug, PartialEq)]
pub enum LockState {
    Free,
    Held(Info),
}

/// Probes the lock without keeping it. The file is never created here.
pub fn lock_state(root: &Path) -> LockState {
    let Ok(mut file) = File::options().read(true).write(true).open(lock_path(root)) else {
        return LockState::Free;
    };
    match file.try_lock() {
        Ok(()) => LockState::Free,
        Err(_) => {
            let mut text = String::new();
            let _ = file.read_to_string(&mut text);
            LockState::Held(serde_json::from_str(&text).unwrap_or_default())
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum StartAction {
    Spawn,
    Nothing,
    StopThenSpawn,
}

/// The `ticker start` decision. A healthy ticker of the same version is never
/// replaced; a different version, or a stop in progress, is stopped first so
/// `open` never ends with no ticker.
pub fn decide_start(lock: &LockState, my_version: &str, stop_file_exists: bool) -> StartAction {
    match lock {
        LockState::Free => StartAction::Spawn,
        LockState::Held(info) if info.version == my_version && !stop_file_exists => StartAction::Nothing,
        LockState::Held(_) => StartAction::StopThenSpawn,
    }
}

/// Spawns the detached loop unless there is nothing to watch. It creates
/// nothing when the root does not exist or contains no projects, so a linked
/// plugin's `[[startup]]` is harmless in sessions that have no projects.
pub fn start(ctx: &Ctx) -> Result<()> {
    let root = &ctx.root;
    if !ctx.detached_ticker || project::list_slugs(root).is_empty() {
        return Ok(());
    }
    let stop_exists = stop_path(root).exists();
    match decide_start(&lock_state(root), crate::VERSION, stop_exists) {
        StartAction::Nothing => Ok(()),
        StartAction::Spawn => {
            // A leftover stop file would make the new ticker exit at once.
            let _ = std::fs::remove_file(stop_path(root));
            spawn(root)
        }
        StartAction::StopThenSpawn => {
            stop(root)?;
            spawn(root)
        }
    }
}

unsafe extern "C" {
    fn setsid() -> i32;
}

/// `ticker run`, detached: null stdio and a new session, so it does not die
/// with the process group of whatever started it (an agent's shell tool).
fn spawn(root: &Path) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let binary = std::env::current_exe().context("could not find this binary's own path")?;
    let mut command = Command::new(binary);
    command
        .arg("--root")
        .arg(root)
        .args(["ticker", "run"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Pane variables belong to whoever started us, not to the ticker: every
    // project carries its own recorded socket.
    for key in ["HERDR_SOCKET_PATH", "HERDR_SESSION", "HERDR_PANE_ID", "HERDR_TAB_ID", "HERDR_WORKSPACE_ID"] {
        command.env_remove(key);
    }
    // SAFETY: setsid is async-signal-safe and touches no memory.
    unsafe {
        command.pre_exec(|| {
            setsid();
            Ok(())
        });
    }
    command.spawn().context("could not start the ticker")?;
    Ok(())
}

/// Asks the running ticker to exit and waits for the lock to be released.
pub fn stop(root: &Path) -> Result<()> {
    if lock_state(root) == LockState::Free {
        let _ = std::fs::remove_file(stop_path(root));
        return Ok(());
    }
    std::fs::write(stop_path(root), b"")?;
    let deadline = Instant::now() + STOP_WAIT;
    while Instant::now() < deadline {
        if lock_state(root) == LockState::Free {
            let _ = std::fs::remove_file(stop_path(root));
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let _ = std::fs::remove_file(stop_path(root));
    bail!("the ticker did not exit within {} seconds", STOP_WAIT.as_secs())
}

pub fn status(root: &Path) -> Result<()> {
    match lock_state(root) {
        LockState::Free => println!("ticker: not running (root {})", root.display()),
        LockState::Held(info) => {
            println!("ticker: running");
            println!("  version: {}", info.version);
            println!("  pid:     {}", info.pid);
            println!("  root:    {}", info.root);
            println!("  started: {}", info.started);
            for (tool, path) in &info.tools {
                println!("  {tool:<6} {path}");
            }
            if info.version != crate::VERSION {
                println!("  note: this binary is {}; `ticker start` replaces the running one", crate::VERSION);
            }
        }
    }
    Ok(())
}

/// Where a tool resolves from this process's own `PATH`.
fn which(tool: &str, path_var: &str) -> String {
    if tool.contains('/') {
        return tool.to_string();
    }
    std::env::split_paths(path_var)
        .map(|dir| dir.join(tool))
        .find(|candidate| candidate.is_file())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(not found)".to_string())
}

pub struct Log {
    path: PathBuf,
}

impl Log {
    pub fn line(&self, text: &str) {
        let Ok(mut file) = File::options().create(true).append(true).open(&self.path) else {
            return;
        };
        let _ = writeln!(file, "{} {}", project::now(), text.replace('\n', " "));
        // Size cap: keep the newer half.
        if file.metadata().map(|m| m.len()).unwrap_or(0) > LOG_CAP
            && let Ok(mut reader) = File::open(&self.path)
        {
            let mut tail = Vec::new();
            if reader.seek(SeekFrom::End(-((LOG_CAP / 2) as i64))).is_ok() && reader.read_to_end(&mut tail).is_ok() {
                let start = tail.iter().position(|b| *b == b'\n').map_or(0, |i| i + 1);
                let _ = project::write_atomic(&self.path, &tail[start..]);
            }
        }
    }
}

/// The loop. Exits when another ticker holds the lock, when the stop file
/// appears, or when no project has had a reachable session or enabled canonical
/// routine for five minutes.
pub fn run(ctx: &Ctx) -> Result<()> {
    let root = &ctx.root;
    if project::list_slugs(root).is_empty() {
        return Ok(());
    }
    let mut lock = File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path(root))?;
    if lock.try_lock().is_err() {
        return Ok(());
    }
    let path_var = ctx.env.var("PATH").unwrap_or("").to_string();
    let info = Info {
        version: crate::VERSION.to_string(),
        pid: std::process::id(),
        root: root.display().to_string(),
        started: project::now(),
        tools: ["herdr", "git", "gh", "ssh", "scp", "rsync"]
            .iter()
            .map(|tool| {
                let name = if *tool == "herdr" { ctx.env.herdr_bin() } else { tool.to_string() };
                (tool.to_string(), which(&name, &path_var))
            })
            .collect(),
    };
    lock.set_len(0)?;
    lock.write_all(serde_json::to_string_pretty(&info)?.as_bytes())?;
    lock.flush()?;

    let log = Log { path: log_path(root) };
    log.line(&format!("ticker {} started (pid {})", info.version, info.pid));
    let mut last_reachable = Instant::now();
    let mut memory = Memory::new(ctx);
    let runner:std::sync::Arc<dyn crate::runner::Runner+Send+Sync>=std::sync::Arc::new(crate::remote_polling::ProbeRunner{inner:std::sync::Arc::new(crate::runner::RealRunner)});
    #[cfg(feature="state-store")]
    let runner:std::sync::Arc<dyn crate::runner::Runner+Send+Sync>=std::sync::Arc::new(crate::routine_jobs::JobRunner{inner:runner});
    let runner:std::sync::Arc<dyn crate::runner::Runner+Send+Sync>=std::sync::Arc::new(crate::copy_jobs::JobRunner{inner:runner});
    let runner:std::sync::Arc<dyn crate::runner::Runner+Send+Sync>=std::sync::Arc::new(crate::legacy_routine_jobs::JobRunner{inner:runner});
    let runner:std::sync::Arc<dyn crate::runner::Runner+Send+Sync>=std::sync::Arc::new(crate::brief_jobs::JobRunner{inner:runner});
    let runner:std::sync::Arc<dyn crate::runner::Runner+Send+Sync>=std::sync::Arc::new(crate::coordinator_jobs::JobRunner{inner:runner});
    let runner:std::sync::Arc<dyn crate::runner::Runner+Send+Sync>=std::sync::Arc::new(crate::token_jobs::JobRunner{inner:runner});
    let executor=std::sync::Arc::new(crate::executor::Executor::new(crate::executor::Limits::default(),runner)?);
    #[cfg(feature="state-store")]
    {memory.routine_jobs=Some(crate::routine_jobs::Queue::new(executor.clone()));}
    if cfg!(target_os="linux") {memory.copy_jobs=Some(crate::copy_jobs::Queue::new(executor.clone()));}
    memory.local_reports=Some(crate::local_reports::Reads::new(executor.clone()));
    memory.pr_reads=Some(crate::pr_polling::Reads::with_executor(executor.clone()));
    memory.remote_reads=Some(crate::remote_polling::Reads::new(executor));
    loop {
        if stop_path(root).exists() {
            log.line("stop file found; cancelling and draining shared executor");
            return memory.pr_reads.as_mut().expect("ticker shared executor").stop();
        }
        let wake = Instant::now() + TICK;
        if tick(ctx, &log, &mut memory) {
            last_reachable = Instant::now();
        } else if last_reachable.elapsed() > IDLE_EXIT {
            log.line("no reachable session or enabled canonical routine for five minutes; draining shared executor");
            return memory.pr_reads.as_mut().expect("ticker shared executor").stop();
        }
        // Sleep in short slices so a stop request is honoured promptly.
        while Instant::now() < wake {
            if stop_path(root).exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }
}

/// One pass over every active project. Cheap work (state, prompts, tokens)
/// comes first for every project, then slow work (copies, launches), so one
/// slow project does not delay the others' sidebar. Returns whether any
/// project's session was reachable or canonical scheduled work remains. A failure in one project never stops the
/// others.
pub fn tick(ctx: &Ctx, log: &Log, memory: &mut Memory) -> bool {
    memory.tick += 1;
    if let Some(reads)=memory.local_reports.as_mut(){reads.begin_pass();}
    if let Some(queue)=memory.copy_jobs.as_mut() {for error in queue.drain(){log.line(&error);}}
    #[cfg(feature="state-store")]
    if let Some(queue)=memory.routine_jobs.as_mut() {for error in queue.drain(){log.line(&error);}}
    let mut reachable = Vec::new();
    #[cfg(feature="state-store")]
    let mut canonical = Vec::new();
    for slug in project::list_slugs(&ctx.root) {
        #[cfg(feature="state-store")]
        if project::ensure_legacy(&ctx.root.join(&slug)).is_err() {
            canonical.push(slug);continue;
        }
        let Ok(project) = Project::load(&ctx.root, &slug) else {
            continue;
        };
        if project.status() != Status::Active {
            continue;
        }
        if let Some(queue)=memory.copy_jobs.as_mut() {
            // Recovery is independent of source/session reachability. The worker
            // re-resolves remote routing and checks retained authority itself.
            let recovered=(||->Result<()> {
                if project.try_coordinator()?.is_some_and(|c|c.prime_claim.as_ref().is_some_and(|claim|claim.delivery.phase==herdr_projects::prompt_claim::Phase::Pending||!claim.delivery.notified)
                    ||c.launch_claim.as_ref().is_some_and(|claim|claim.phase==herdr_projects::launch_claim::Phase::Pending||!claim.notified)) {
                    let guard=herdr_projects::execution_guard::ProjectGuard::acquire(&project.dir())?;
                    crate::coordinator_jobs::recover(&project,&guard)?;
                }
                Ok(())
            })();
            if let Err(error)=recovered {log.line(&format!("{slug}: coordinator prime recovery: {error:#}"));continue;}
            let recovered=(||->Result<()> {
                let state=steps::try_load_state(&project)?;
                if state.notification_claim.as_ref().is_some_and(|c|c.phase==herdr_projects::notification_claim::Phase::Pending)
                    ||(state.notification_claim.is_none()&&!state.notification_retry.hash.is_empty()&&state.notification_retry.retry.attempts>0) {
                    let guard=herdr_projects::execution_guard::ProjectGuard::acquire(&project.dir())?;
                    crate::coordinator_jobs::recover_notification(&project,&guard)?;
                }Ok(())
            })();
            if let Err(error)=recovered{log.line(&format!("{slug}: notification recovery: {error:#}"));continue;}
            let (threads,diagnostics)=thread::list_with_diagnostics(&project);
            for error in diagnostics {log.line(&format!("{slug}: {error}"));}
            if threads.iter().any(|t|t.launch_claim.as_ref().is_some_and(|claim|claim.phase==thread::launch_delivery::Phase::Pending||!claim.notified)) {
                let recovered=(||->Result<()> {let guard=herdr_projects::execution_guard::ProjectGuard::acquire(&project.dir())?;thread::launch_delivery::recover(&project,&guard)})();
                if let Err(error)=recovered {log.line(&format!("{slug}: launch recovery: {error:#}"));continue;}
            }
            if threads.iter().any(|t|t.prompt_claim.as_ref().is_some_and(|claim|claim.phase==thread::prompt_delivery::Phase::Pending||!claim.notified)) {
                let recovered=(||->Result<()> {let guard=herdr_projects::execution_guard::ProjectGuard::acquire(&project.dir())?;thread::prompt_delivery::recover(&project,&guard)})();
                if let Err(error)=recovered {log.line(&format!("{slug}: brief recovery: {error:#}"));continue;}
            }
            for t in &threads {
                if let Some(intent)=&t.pending_final_copy {
                    if let Err(error)=queue.offer_final(ctx,&project,t,None,intent.purpose.clone(),intent.operation.clone()){log.line(&format!("{slug}: final-copy recovery: {error:#}"));}
                }
            }
            for t in threads.iter().filter(|t|t.pending_live_copy.is_some()) {
                if let Err(error)=queue.offer(ctx,&project,t,None){log.line(&format!("{slug}: copy recovery: {error:#}"));}
            }
        }
        match tick_cheap_reports(ctx, &project,memory.local_reports.as_mut(),memory.copy_jobs.as_mut()) {
            Ok(Some(seen)) => reachable.push((project, seen)),
            Ok(None) => {}
            Err(error) => log.line(&format!("{slug}: {error:#}")),
        }
    }
    if !reachable.is_empty() {
        let first = (memory.tick.saturating_sub(1) % reachable.len() as u64) as usize;
        reachable.rotate_left(first);
    }
    for (project, seen) in &reachable {
        for error in tick_slow(ctx, project, seen, memory) {
            log.line(&format!("{}: {error:#}", project.slug));
        }
    }
    let any_reachable = !reachable.is_empty();
    #[cfg(feature="state-store")]
    {
        let mut any_reachable=any_reachable;
        if !canonical.is_empty() {let first=(memory.tick.saturating_sub(1)%canonical.len() as u64) as usize;canonical.rotate_left(first);}
        for slug in &canonical {
            match crate::canonical_controller::poll(ctx,&ctx.root.join(&slug),memory.tick.saturating_sub(1)) {
                Ok(result)=>{any_reachable|=result.reachable||result.scheduled_work;if let Some(error)=result.operation_error {log.line(&format!("{slug}: canonical operation: {error}"));}},
                Err(error)=>log.line(&format!("{slug}: canonical controller: {error:#}")),
            }
        }
        admit_background(ctx,log,memory,canonical.into_iter().map(|slug|ctx.root.join(slug)).collect());
        if let Some(reads)=memory.local_reports.as_mut(){for error in reads.admit(){log.line(&error);}}
        any_reachable|=memory.routine_jobs.as_ref().is_some_and(|q|q.pending());
        any_reachable|=memory.copy_jobs.as_ref().is_some_and(|q|q.pending()||q.offered());
        return any_reachable;
    }
    #[cfg(not(feature="state-store"))]
    {
        admit_background(ctx,log,memory,Vec::new());
        if let Some(reads)=memory.local_reports.as_mut(){for error in reads.admit(){log.line(&error);}}
        any_reachable||memory.copy_jobs.as_ref().is_some_and(|q|q.pending()||q.offered())
    }

}
/// Drain happens only at tick entry. Even a completed ticket holds its turn until
/// then, preserving a full project effect pass before another background job.
fn admit_background(_ctx:&Ctx,log:&Log,memory:&mut Memory,canonical:Vec<PathBuf>) {
    if memory.copy_jobs.as_ref().is_some_and(|q|q.pending()){return;}
    #[cfg(feature="state-store")]
    if memory.routine_jobs.as_ref().is_some_and(|q|q.pending()){return;}
    if memory.prefer_copy {
        if let Some(queue)=memory.copy_jobs.as_mut(){for error in queue.admit(){log.line(&error);}if queue.pending(){memory.prefer_copy=false;return;}}
    }
    #[cfg(feature="state-store")]
    if let Some(queue)=memory.routine_jobs.as_mut(){for error in queue.admit_projects(canonical){log.line(&error);}if queue.pending(){memory.prefer_copy=true;return;}}
    #[cfg(not(feature="state-store"))]
    let _=canonical;
    if let Some(queue)=memory.copy_jobs.as_mut(){for error in queue.admit(){log.line(&error);}if queue.pending(){memory.prefer_copy=false;}}
}


#[cfg(test)]
pub fn tick_for_test(ctx: &Ctx, memory: &mut Memory) -> bool {
    memory.advance_clock(TICK);
    let dir = std::env::temp_dir().join(format!("hp-test-log-{}", std::process::id()));
    tick(ctx, &Log { path: dir }, memory)
}

/// What the cheap pass saw, handed to the slow pass so herdr is asked once.
pub struct Seen {
    notification_error: Option<String>,
    local_reports:Option<std::collections::BTreeMap<String,crate::local_reports::Sample>>,
    report_errors:Vec<String>,
    socket: String,
    agents: Vec<Agent>,
    panes: Vec<Pane>,
    /// Group changes of this tick, turned into inbox items after the copies.
    transitions: Vec<Transition>,
    /// The session answered, the project has at least two recorded local
    /// panes, and every one of them is missing: herdr was restarted.
    session_lost: bool,
}

/// Both passes for one project; `Ok(false)` when its session is unreachable.
#[cfg(test)]
pub fn tick_project(ctx: &Ctx, project: &Project) -> Result<bool> {
    tick_project_with(ctx, project, &mut Memory::new(ctx))
}

#[cfg(test)]
pub fn tick_project_with(ctx: &Ctx, project: &Project, memory: &mut Memory) -> Result<bool> {
    if let Some(reads)=memory.local_reports.as_mut(){reads.begin_pass();}
    match tick_cheap_reports(ctx, project,memory.local_reports.as_mut(),memory.copy_jobs.as_mut())? {
        Some(seen) => match tick_slow(ctx, project, &seen, memory).into_iter().next() {
            Some(error) => Err(error),
            None => Ok(true),
        },
        None => Ok(false),
    }
}

/// State, pending prompts, group and tokens for a set of threads that live in
/// one herdr server (the local session, or one remote machine).
struct Pass {
    transitions: Vec<Transition>,
    recorded_panes: usize,
    missing_panes: usize,
    error: Option<anyhow::Error>,
}

fn thread_pass(project: &Project, herdr: &Herdr, threads: &[thread::Thread], agents: &[Agent], panes: &[Pane], hashes: Option<&std::collections::BTreeMap<String, String>>,defer_briefs:bool) -> Result<Pass> {
    let slug = &project.slug;
    let now = jiff::Timestamp::now();
    let mut pass = Pass { transitions: Vec::new(), recorded_panes: 0, missing_panes: 0, error: None };
    for t in threads {
        if t.status == thread::Status::Starting {
            if thread::seconds_since(&t.created, now) >= thread::STARTING_TIMEOUT_SECS {
                thread::update(project, &t.id, |t| {
                    t.status = thread::Status::Failed;
                    t.error = "still starting after five minutes".into();
                })?;
            }
            continue;
        }
        let mut live = thread::live_state(t, agents, panes, now);
        if !t.pane_id.is_empty() {
            pass.recorded_panes += 1;
            pass.missing_panes += usize::from(!live.pane_exists);
        }
        let state = live.agent_state.clone().unwrap_or_default();
        if state != t.last_state {
            live.state_secs = 0;
        }
        // A remote thread is polled once a minute, so `blocked` at a poll
        // already counts: there is no finer clock to debounce against.
        if t.is_remote() && state == "blocked" {
            live.state_secs = live.state_secs.max(thread::BLOCKED_DEBOUNCE_SECS);
        }

        let mut delivered = false;
        if !defer_briefs && thread::prompt_delivery::ready(t).is_ok() && live.agent_state.as_deref().is_some_and(crate::herdr::ready_state) {
            match herdr.agent_prompt(&t.pane_id, &thread::launch_prompt(slug, &t.id)) {
                Ok(()) => delivered = true,
                Err(error) => pass.error = pass.error.or(Some(anyhow::anyhow!("{}: brief prompt: {error}", t.id))),
            }
        }

        // A report written this tick counts for the group at once; the copy
        // home follows. Otherwise a finished thread would show as Idle for one
        // tick before it shows as Ready for review.
        let fresh_hash = match hashes {
            Some(hashes) => hashes.get(&t.id).cloned(),
            None => thread::local_report_hash(t),
        };
        let report_hash = fresh_hash.unwrap_or_else(|| t.report_hash.clone());
        let after = thread::Thread { prompt_pending: t.prompt_pending && !delivered, report_hash, ..t.clone() };
        // In the tick that delivers a prompt the agent still reads as idle; it
        // has just been given work, so it is Working, not Idle.
        let group = if delivered { thread::Group::Working } else { thread::group(&after, &live, now) };
        if !t.last_group.is_empty() && group.token() != t.last_group {
            let note = if !live.pane_exists { "pane closed".to_string() } else if state.is_empty() { "no agent".to_string() } else { state.clone() };
            pass.transitions.push(Transition { id: t.id.clone(), to: group, note });
        }
        if delivered || state != t.last_state || group.token() != t.last_group {
            thread::update(project, &t.id, |t| {
                if delivered {
                    t.prompt_pending = false;
                }
                if state != t.last_state {
                    t.last_state = state.clone();
                    t.last_state_change = project::now();
                }
                t.last_group = group.token().to_string();
            })?;
        }
        if live.pane_exists && !defer_briefs {
            threads::report_thread_tokens(herdr, t, slug, group);
        }
    }
    Ok(pass)
}

/// Launches pending threads whose pane is at a shell prompt. At most one
/// `agent start` per project per tick (`may_start`), and never a start and a
/// prompt for the same pane in one tick: prompts only go to agents that were
/// already listed before any start.
fn offer_launches(ctx:&Ctx,project:&Project,threads:&[thread::Thread],agents:&[Agent],panes:&[Pane],queue:&mut crate::copy_jobs::Queue,route:Option<&crate::remote_api::Route>,errors:&mut Vec<anyhow::Error>) {
    for t in threads.iter().filter(|t|thread::prompt_delivery::ready(t).is_ok()) {
        if !agents.iter().any(|a|a.pane_id==t.pane_id)&&panes.iter().any(|p|thread::pane_matches(t,p)) {
            if t.launch_attempts>=thread::MAX_LAUNCH_ATTEMPTS&&t.launch_claim.as_ref().is_none_or(|c|c.generation!=t.lifecycle_generation) {
                errors.extend(thread::update(project,&t.id,|t|{t.status=thread::Status::Failed;t.error=format!("no `{}` agent appeared in the pane after {} launch attempts",t.agent,thread::MAX_LAUNCH_ATTEMPTS);}).err());
            }else if thread::launch_delivery::ready(t).is_ok(){errors.extend(queue.offer_launch(ctx,project,t,route).err());}
        }
    }
}
fn launch_pass(ctx: &Ctx, project: &Project, herdr: &Herdr, threads: &[thread::Thread], agents: &[Agent], panes: &[Pane], may_start: &mut bool, errors: &mut Vec<anyhow::Error>) {
    let now = jiff::Timestamp::now();
    for t in threads {
        if thread::prompt_delivery::ready(t).is_err(){continue;}
        if t.status != thread::Status::Open || !t.prompt_pending {
            continue;
        }
        let live = thread::live_state(t, agents, panes, now);
        if live.agent_state.is_some() || !live.pane_exists {
            continue;
        }
        if t.launch_attempts >= thread::MAX_LAUNCH_ATTEMPTS&&t.launch_claim.as_ref().is_none_or(|c|c.generation!=t.lifecycle_generation) {
            let failed = thread::update(project, &t.id, |t| {
                t.status = thread::Status::Failed;
                t.error = format!("no `{}` agent appeared in the pane after {} launch attempts", t.agent, thread::MAX_LAUNCH_ATTEMPTS);
            });
            errors.extend(failed.err());
            continue;
        }
        if thread::launch_delivery::ready(t).is_err()||!*may_start {
            continue;
        }
        let launched = (|| -> Result<()> {
            let safety = project.safety(&ctx.config_dir)?;
            let agent_args=safety.worker_arguments(&t.agent)?;
            *may_start=false;
            thread::update(project, &t.id, |t| t.launch_attempts += 1)?;
            herdr.on_machine(&t.machine).agent_start(&t.agent_name, &t.agent, &t.pane_id, agent_args)?;
            Ok(())
        })();
        errors.extend(launched.err().map(|e| e.context(format!("{}: launch", t.id))));
    }
}

fn open_threads(project: &Project, remote: bool) -> Vec<thread::Thread> {
    thread::list(project)
        .into_iter()
        .filter(|t| t.removal.is_none() && t.is_remote() == remote && matches!(t.status, thread::Status::Open | thread::Status::Starting))
        .collect()
}

/// Returns `Ok(None)` when the project's session cannot be reached: then no
/// state is read, so nothing is ever reported as gone.
mod status_observation;

#[cfg(test)]
fn tick_cheap(ctx: &Ctx, project: &Project) -> Result<Option<Seen>> {tick_cheap_reports(ctx,project,None,None)}
fn tick_cheap_reports(ctx: &Ctx, project: &Project,mut reads:Option<&mut crate::local_reports::Reads>,mut copies:Option<&mut crate::copy_jobs::Queue>) -> Result<Option<Seen>> {
    let lease = crate::cleanup::lease(&ctx.root);
    let _project_guard = if lease.is_err() {
        Some(herdr_projects::execution_guard::ProjectGuard::acquire(&project.dir())?)
    } else {None};
    if project.try_status()? != Status::Active { return Ok(None); }
    status_observation::deliver(project)?;
    thread::copy_delivery::deliver(project)?;
    thread::review_delivery::deliver(project)?;
    let Some(record) = project.try_coordinator()? else {
        return Ok(None);
    };
    if record.socket.is_empty() || !Path::new(&record.socket).exists() {
        return Ok(None);
    }
    let herdr = Herdr::new(ctx.env.herdr_bin(), &record.socket, ctx.runner);
    let Ok(agents) = herdr.agent_list() else {
        return Ok(None);
    };
    let Ok(panes) = herdr.pane_list() else {
        return Ok(None);
    };
    if lease.is_err() {
        let threads=open_threads(project,false);
        let recorded=threads.iter().filter(|t|!t.pane_id.is_empty()).count()+usize::from(!record.pane_id.is_empty());
        let missing=threads.iter().filter(|t|!t.pane_id.is_empty() && !thread::live_state(t,&agents,&panes,jiff::Timestamp::now()).pane_exists).count()
            +usize::from(!record.pane_id.is_empty() && !agents.iter().any(|a|coordinator::agent_matches(&record,a)) && !panes.iter().any(|p|coordinator::pane_matches(&record,p)));
        let session_lost=recorded>=2 && missing==recorded;
        // Session-loss notification remains separately deduplicated by the
        // exclusive slow pass. Avoid manufacturing per-thread idle notices.
        if !session_lost {status_observation::observe(project,&agents,&panes)?;}
        return Ok(Some(Seen{notification_error:None,local_reports:reads.as_ref().map(|_|Default::default()),report_errors:Vec::new(),socket:record.socket,agents,panes,transitions:Vec::new(),session_lost}));
    }
    let slug = &project.slug;
    // Do not perform delivery or overwrite durable obligations when state is
    // unreadable. Other projects still receive their own tick.
    let mut state = steps::try_load_state(project)?;
    let mut first_error = None;

    // The coordinator: deliver a pending priming prompt, refresh its tokens.
    let agent = agents.iter().find(|a| coordinator::agent_matches(&record, a));
    if let Some(agent) = agent {
        if crate::coordinator_jobs::ready(&record).is_ok() && agent.ready() {
            if let Some(queue)=copies.as_deref_mut() {
                first_error=queue.offer_coordinator_prime(ctx,project,&record).err();
            } else {
            let prefix = coordinator::current_prefix(&ctx.root)?;
            match herdr.agent_prompt(&record.pane_id, &coordinator::priming_prompt(&prefix, slug)) {
                Ok(()) => {
                    project.update_coordinator(|c| c.prime_pending = false)?;
                }
                Err(error) => first_error = Some(anyhow::anyhow!("priming prompt: {error}")),
            }
        }
        }
        if let Some(queue)=copies.as_deref_mut() {first_error=first_error.or(queue.offer_tokens(ctx,project,None,None).err());}
        else {coordinator::report_tokens(&herdr, slug, &record.pane_id);}
    }

    let local=open_threads(project,false);let mut reports=std::collections::BTreeMap::new();let mut report_errors=Vec::new();
    if let Some(reads)=reads.as_mut() {
        for t in local.iter().filter(|t|t.status==thread::Status::Open&&t.pending_live_copy.is_none()&&t.pending_final_copy.is_none()) {
            match reads.poll(project,t) {
                Ok(crate::local_reports::Poll::Ready(sample))=>{reports.insert(t.id.clone(),sample);},
                Ok(crate::local_reports::Poll::Pending)=>{},
                Ok(crate::local_reports::Poll::Failed(error))=>report_errors.push(format!("{}: report source: {error}",t.id)),
                Err(error)=>report_errors.push(format!("{}: report observation: {error:#}",t.id)),
            }
        }
    }
    let hashes=reports.iter().filter_map(|(id,s)|s.hash.as_ref().map(|hash|(id.clone(),hash.clone()))).collect();
    if let Some(queue)=copies.as_deref_mut() {
        for t in local.iter().filter(|t|thread::prompt_delivery::ready(t).is_ok()) {
            if agents.iter().any(|a|thread::agent_matches(t,a)&&a.ready()) {
                report_errors.extend(queue.offer_brief(ctx,project,t).err().map(|e|format!("{}: brief admission: {e:#}",t.id)));
            }
        }
    }
    let pass = thread_pass(project, &herdr, &local, &agents, &panes, reads.as_ref().map(|_|&hashes),copies.is_some())?;
    if let Some(queue)=copies.as_deref_mut() {
        for t in local.iter().filter(|t|matches!(t.status,thread::Status::Open|thread::Status::Failed)&&t.removal.is_none()&&t.pending_live_copy.is_none()&&t.pending_final_copy.is_none()&&thread::live_state(t,&agents,&panes,jiff::Timestamp::now()).pane_exists) {
            report_errors.extend(queue.offer_tokens(ctx,project,Some(t),None).err().map(|e|format!("{}: token admission: {e:#}",t.id)));
        }
    }
    first_error = first_error.or(pass.error);
    let coordinator_recorded = usize::from(!record.pane_id.is_empty());
    let coordinator_missing = usize::from(coordinator_recorded == 1 && agent.is_none() && !panes.iter().any(|p| coordinator::pane_matches(&record, p)));
    let recorded_panes = pass.recorded_panes + coordinator_recorded;
    let missing_panes = pass.missing_panes + coordinator_missing;

    // Nudge (or notify) about inbox items `context` has not shown yet.
    let mut notification_error = None;
    if let Some(queue)=copies.as_deref_mut() {
        if crate::coordinator_jobs::notification_needed(project,&state)&&let Err(error)=queue.offer_notification(ctx,project,&record){notification_error=Some(format!("notification admission: {error:#}"));}
    } else if let Ok((settings, _)) = project.read_project_md() {
        let before = state.clone();
        let primed=project.try_coordinator()?.is_some_and(|c|!c.prime_pending);
        let ready_pane = agent.filter(|a| a.ready()&&primed).map(|_| record.pane_id.as_str());
        if let Err(error) = steps::nudge(project, &mut state, &settings, &herdr, ready_pane) {
            notification_error = Some(format!("nudge: {error:#}"));
        }
        if state != before {
            steps::save_state(project, &state)?;
        }
    }

    match first_error {
        Some(error) => Err(error),
        None => Ok(Some(Seen {
            notification_error,
            local_reports:reads.as_ref().map(|_|reports),
            report_errors,
            socket: record.socket,
            agents,
            panes,
            transitions: pass.transitions,
            session_lost: recorded_panes >= 2 && missing_panes == recorded_panes,
        })),
    }
}

/// One remote machine: one `agent list` (and `pane list`) through
/// `herdr --machine`, one ssh call for every report hash, then the same thread
/// pass, copies and launches as for local threads. If the machine cannot be
/// reached nothing is read: no state, no group change, no copy, no inbox item.
fn remote_pass(ctx: &Ctx, project: &Project, herdr: &Herdr, machine: &str, threads: &[thread::Thread], may_start: &mut bool, errors: &mut Vec<anyhow::Error>,copies:Option<&mut crate::copy_jobs::Queue>,deferred:&mut std::collections::BTreeSet<String>,observed:&mut std::collections::BTreeSet<String>) -> Result<Vec<Transition>, String> {
    let remote = herdr.on_machine(machine);
    let agents = remote.agent_list().map_err(|e| e.to_string())?;
    let panes = remote.pane_list().map_err(|e| e.to_string())?;
    let target = crate::remote::ssh_target(ctx.runner, &ctx.env.herdr_bin(), &ctx.config_dir, machine).map_err(|e| format!("{e:#}"))?;
    let dirs: Vec<(String, String)> = threads.iter().filter(|t| !t.thread_dir.is_empty()).map(|t| (t.id.clone(), t.thread_dir.clone())).collect();
    let hashes = crate::remote::report_hashes(ctx.runner, &target, &dirs).map_err(|e| format!("{e:#}"))?;

    apply_remote(ctx,project,herdr,machine,threads,crate::remote_polling::Observation{agents,panes,target,route:None,hashes},false,may_start,errors,copies,deferred,observed)
}
fn apply_remote(ctx:&Ctx,project:&Project,herdr:&Herdr,machine:&str,threads:&[thread::Thread],observation:crate::remote_polling::Observation,asynchronous:bool,may_start:&mut bool,errors:&mut Vec<anyhow::Error>,mut copies:Option<&mut crate::copy_jobs::Queue>,deferred:&mut std::collections::BTreeSet<String>,observed:&mut std::collections::BTreeSet<String>)->Result<Vec<Transition>,String> {
    let remote=herdr.on_machine(machine);
    let crate::remote_polling::Observation{mut agents,mut panes,target,route,hashes}=observation;
    // Queue observations cannot authorize starts or prompts. Concrete workers
    // perform their own fresh target checks under inherited ownership.
    if asynchronous&&copies.is_none()&&threads.iter().any(|t|t.prompt_pending||hashes.get(&t.id).is_some_and(|hash|hash!=&t.report_hash)) {
        let current=crate::remote::ssh_target(ctx.runner,&ctx.env.herdr_bin(),&ctx.config_dir,machine).map_err(|e|format!("{e:#}"))?;
        if current!=target {return Err("remote route changed after observation; retry before effects".into());}
    }
    if asynchronous&&copies.is_none()&&threads.iter().any(|t|t.prompt_pending) {
        agents=remote.agent_list().map_err(|e|e.to_string())?;
        panes=remote.pane_list().map_err(|e|e.to_string())?;
    }
    if let Some(queue)=copies.as_deref_mut() {
        for t in threads.iter().filter(|t|thread::prompt_delivery::ready(t).is_ok()) {
            if agents.iter().any(|a|thread::agent_matches(t,a)&&a.ready()) {
                errors.extend(match route.as_ref() {Some(route)=>queue.offer_remote_brief(ctx,project,t,route),None=>Err(anyhow::anyhow!("{}: saved remote session contract unavailable for brief",t.id))}.err());
            }
        }
    }
    let pass = thread_pass(project, &remote, threads, &agents, &panes, Some(&hashes),copies.is_some()).map_err(|e| format!("{e:#}"))?;
    errors.extend(pass.error);
    if let Some(queue)=copies.as_deref_mut() {
        for t in threads.iter().filter(|t|matches!(t.status,thread::Status::Open|thread::Status::Failed)&&t.removal.is_none()&&t.pending_live_copy.is_none()&&t.pending_final_copy.is_none()&&thread::live_state(t,&agents,&panes,jiff::Timestamp::now()).pane_exists) {
            errors.extend(match route.as_ref(){Some(route)=>queue.offer_tokens(ctx,project,Some(t),Some(route)),None=>Err(anyhow::anyhow!("{}: saved remote session contract unavailable for tokens",t.id))}.err());
        }
    }

    for t in threads.iter().filter(|t| t.status == thread::Status::Open) {
        observed.insert(t.id.clone());
        if t.pending_final_copy.is_some() {deferred.insert(t.id.clone());continue;}
        let changed=hashes.get(&t.id).is_some_and(|h|*h!=t.report_hash);
        if let Some(queue)=copies.as_deref_mut() {
            if changed||t.pending_live_copy.is_some() {
                deferred.insert(t.id.clone());
                if t.pending_live_copy.is_none() {if let Err(error)=thread::copy_delivery::ready(t){errors.push(error);continue;}}
                errors.extend(queue.offer(ctx,project,t,Some(&target)).err());
            }else {queue.clear(project,&t.id);}
            continue;
        }
        if !changed {continue;}
        if let Err(error) = thread::copy_delivery::ready(t) { errors.push(error); continue; }
        let copied = thread::copy_home_remote(project, t, true, ctx.runner, &target);
        errors.extend(thread::copy_delivery::record(project, t, &copied).map_err(|e|e.context(format!("{}: copy receipt", t.id))).err());
    }
    if let Some(queue)=copies.as_deref_mut(){offer_launches(ctx,project,threads,&agents,&panes,queue,route.as_ref(),errors);}else{launch_pass(ctx, project, herdr, threads, &agents, &panes, may_start, errors);}
    Ok(pass.transitions)
}

/// Copies and launches, remote machines, then inbox items, pull requests,
/// routines, auto-resolve and housekeeping.
fn tick_slow(ctx: &Ctx, project: &Project, seen: &Seen, memory: &mut Memory) -> Vec<anyhow::Error> {
    let _lease = match crate::cleanup::lease(&ctx.root) { Ok(lease) => lease, Err(error) => return vec![error] };
    match project.try_status() { Ok(Status::Active) => {}, Ok(_) => return Vec::new(), Err(error) => return vec![error] }
    let mut errors = seen.report_errors.iter().map(|e|anyhow::anyhow!("{e}")).collect::<Vec<_>>();
    let mut state = match steps::try_load_state(project) {
        Ok(state) => state,
        Err(error) => return vec![error],
    };
    let before = state.clone();
    errors.extend(crate::legacy_routine_jobs::deliver(project,&mut state).err());
    if let Some(error) = &seen.notification_error { errors.push(anyhow::anyhow!("{error}")); }
    let herdr = Herdr::new(ctx.env.herdr_bin(), &seen.socket, ctx.runner);
    let now = jiff::Timestamp::now();
    let mut may_start = true;
    let mut transitions = seen.transitions.clone();
    let mut deferred=std::collections::BTreeSet::new();
    let mut observed=std::collections::BTreeSet::new();

    if let Some(record) = project.coordinator().filter(|c| crate::coordinator_jobs::ready_start(c).is_ok()) {
        let pane_alive = seen.panes.iter().any(|p| coordinator::pane_matches(&record, p));
        let pane_has_agent = seen.agents.iter().any(|a| a.pane_id == record.pane_id);
        if pane_alive && !pane_has_agent && record.launch_attempts < MAX_LAUNCH_ATTEMPTS {
            if let Some(queue)=memory.copy_jobs.as_mut() {
                errors.extend(queue.offer_coordinator_start(ctx,project,&record).err());
            } else {
            let started = (|| -> Result<()> {
                let (settings, _) = project.read_project_md()?;
                let safety = project.safety(&ctx.config_dir)?;
                let agent_args=safety.coordinator_arguments(&settings.coordinator_agent)?;
                may_start=false;
                project.update_coordinator(|c| c.launch_attempts += 1)?;
                herdr.agent_start(&record.agent_name, &settings.coordinator_agent, &record.pane_id, agent_args)?;
                Ok(())
            })();
            errors.extend(started.err());
            }
        }
    }

    // Local threads: copy home when the report changed, then launches.
    let local = open_threads(project, false);
    for t in local.iter().filter(|t| t.status == thread::Status::Open) {
        if t.pending_final_copy.is_some() {deferred.insert(t.id.clone());continue;}
        if t.pending_live_copy.is_some()&&let Some(queue)=memory.copy_jobs.as_mut() {
            deferred.insert(t.id.clone());errors.extend(queue.offer(ctx,project,t,None).err());continue;
        }
        let hash=if let Some(reports)=&seen.local_reports {
            let Some(sample)=reports.get(&t.id).filter(|sample|sample.matches(t)) else {deferred.insert(t.id.clone());continue;};
            sample.hash.clone()
        }else {match thread::try_local_report_hash(t){Ok(hash)=>hash,Err(error)=>{deferred.insert(t.id.clone());errors.push(error.context(format!("{}: report source",t.id)));continue;}}};
        if let Some(queue)=memory.copy_jobs.as_mut() {
            if hash.as_ref().is_some_and(|hash|hash!=&t.report_hash) {
                deferred.insert(t.id.clone());
                if let Err(error)=thread::copy_delivery::ready(t){errors.push(error);continue;}
                errors.extend(queue.offer(ctx,project,t,None).err());
            }else {queue.clear(project,&t.id);}
            continue;
        }
        if let Some(hash) = hash
            && hash != t.report_hash
        {
            if let Err(error) = thread::copy_delivery::ready(t) { errors.push(error); continue; }
            let copied = thread::copy_home_local(project, t, true, ctx.runner);
            errors.extend(thread::copy_delivery::record(project, t, &copied).map_err(|e|e.context(format!("{}: copy receipt", t.id))).err());
        }
    }
    if let Some(queue)=memory.copy_jobs.as_mut(){offer_launches(ctx,project,&local,&seen.agents,&seen.panes,queue,None,&mut errors);}else{launch_pass(ctx, project, &herdr, &local, &seen.agents, &seen.panes, &mut may_start, &mut errors);}

    // Remote threads, one machine at a time, on elapsed-time deadlines.
    let remote_threads = open_threads(project, true);
    let mut machines: Vec<String> = remote_threads.iter().map(|t| t.machine.clone()).collect();
    machines.sort();
    machines.dedup();
    for machine in machines {
        let key = steps::MachineKey {
            project: project.canonical_dir(),
            socket: seen.socket.clone(),
            machine: machine.clone(),
        };
        let pending=memory.remote_reads.as_ref().is_some_and(|reads|reads.pending(&key));
        if !pending&&!memory.machine_is_due(&key) {continue;}
        let threads: Vec<thread::Thread> = remote_threads.iter().filter(|t| t.machine == machine).cloned().collect();
        let outcome=if let Some(reads)=memory.remote_reads.as_mut() {
            match reads.poll(&key,&ctx.env.herdr_bin(),&ctx.config_dir.join("config.toml"),&threads) {
                Ok(crate::remote_polling::Poll::Pending)=>continue,
                Ok(crate::remote_polling::Poll::Ready(Ok(observation)))=>apply_remote(ctx,project,&herdr,&machine,&threads,observation,true,&mut may_start,&mut errors,memory.copy_jobs.as_mut(),&mut deferred,&mut observed),
                Ok(crate::remote_polling::Poll::Ready(Err(error)))=>Err(format!("{error:#}")),
                Err(error)=>{memory.machines.remove(&key);errors.push(error.context("remote observation admission"));continue;},
            }
        }else{remote_pass(ctx, project, &herdr, &machine, &threads, &mut may_start, &mut errors,memory.copy_jobs.as_mut(),&mut deferred,&mut observed)};
        let outage_error = outcome.as_ref().err().map(String::as_str);
        memory.record_machine(&key, outage_error);
        errors.extend(steps::write_machine_outage(project, &mut state, &key, outage_error, now, memory.outage_secs).err());
        match outcome {
            Ok(found) => transitions.extend(found),
            Err(error) => errors.push(anyhow::anyhow!("{machine}: unreachable this tick: {error}")),
        }
    }

    errors.extend(thread::copy_delivery::deliver(project).err());
    errors.extend(steps::write_thread_items_ready(project, &mut state, &transitions, seen.session_lost,|t| {
        !deferred.contains(&t.id)&&(t.is_remote()||seen.local_reports.as_ref().is_none_or(|reports|reports.get(&t.id).is_some_and(|sample|sample.matches(t))))&&memory.copy_jobs.as_ref().is_none_or(|q|!q.outstanding(project,&t.id)&&(!t.is_remote()||observed.contains(&t.id)))
    }).err());
    errors.extend(steps::pull_requests(ctx, project, &mut state, memory, now));
    let zoned = jiff::Zoned::now();
    match project.read_project_md() {
        Ok((settings, _)) => {
            let commands = project.safety(&ctx.config_dir).map(|s| s.routine_commands).unwrap_or(false);
            errors.extend(steps::routines_queued(ctx, project, &mut state, commands, None, &zoned,memory.copy_jobs.as_mut()));
            errors.extend(steps::auto_resolve_queued(ctx, project, &settings, memory.started, &state, now,memory.copy_jobs.as_mut()));
        }
        Err(error) => {
            let text = std::fs::read(project.project_md()).unwrap_or_default();
            let problem = Some((thread::sha256_hex(&text), format!("{error:#}")));
            errors.extend(steps::routines_queued(ctx, project, &mut state, false, problem, &zoned,memory.copy_jobs.as_mut()));
        }
    }
    inbox::prune_done(project, steps::DONE_RETENTION_DAYS);
    if state != before {
        errors.extend(steps::save_state(project, &state).err());
    }
    errors
}

#[cfg(test)]
mod copy_admission;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Env;
    use crate::runner::fake::{FakeRunner, fail, ok};

    fn held(version: &str) -> LockState {
        LockState::Held(Info {
            version: version.into(),
            ..Info::default()
        })
    }

    #[test]
    fn start_decisions() {
        assert_eq!(decide_start(&LockState::Free, "v1", false), StartAction::Spawn);
        assert_eq!(decide_start(&LockState::Free, "v1", true), StartAction::Spawn);
        assert_eq!(decide_start(&held("v1"), "v1", false), StartAction::Nothing);
        assert_eq!(decide_start(&held("v0"), "v1", false), StartAction::StopThenSpawn);
        // A stop in progress: finish it, then spawn.
        assert_eq!(decide_start(&held("v1"), "v1", true), StartAction::StopThenSpawn);
    }

    #[test]
    fn start_and_run_create_nothing_without_projects() {
        let home = tempfile::tempdir().unwrap();
        let missing = home.path().join("root");
        let env = Env::for_test(home.path(), &[]);
        let runner = FakeRunner::new();
        let ctx = Ctx { env: &env, root: missing.clone(), config_dir: home.path().join("cfg"), runner: &runner, detached_ticker: true };
        start(&ctx).unwrap();
        assert!(!missing.exists());
        run(&ctx).unwrap();
        assert!(!missing.exists());

        std::fs::create_dir(&missing).unwrap();
        start(&ctx).unwrap();
        run(&ctx).unwrap();
        assert_eq!(std::fs::read_dir(&missing).unwrap().count(), 0);
    }

    #[test]
    fn lock_probe_sees_a_holder_and_its_version() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(lock_state(root.path()), LockState::Free);
        let mut file = File::options().create(true).write(true).truncate(false).open(lock_path(root.path())).unwrap();
        file.lock().unwrap();
        file.write_all(br#"{"version":"v9","pid":1}"#).unwrap();
        match lock_state(root.path()) {
            LockState::Held(info) => assert_eq!(info.version, "v9"),
            LockState::Free => panic!("lock should be held"),
        }
        drop(file);
        // Another test may fork a child at this instant; until that child execs,
        // it shares the locked descriptor. Real callers poll too (`ticker stop`).
        let deadline = Instant::now() + Duration::from_secs(2);
        while lock_state(root.path()) != LockState::Free && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(lock_state(root.path()), LockState::Free);
    }

    #[test]
    fn stop_with_a_free_lock_removes_a_stale_stop_file() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(stop_path(root.path()), b"").unwrap();
        stop(root.path()).unwrap();
        assert!(!stop_path(root.path()).exists());
    }

    const AGENT_READY: &str = r#"{"result":{"agents":[{"pane_id":"w1:p1","tab_id":"w1:t1","workspace_id":"w1","name":"hp-demo-coordinator","agent":"claude","agent_status":"idle","cwd":"CWD"}]}}"#;
    const AGENT_BLOCKED: &str = r#"{"result":{"agents":[{"pane_id":"w1:p1","tab_id":"w1:t1","workspace_id":"w1","name":"hp-demo-coordinator","agent":"claude","agent_status":"blocked","cwd":"CWD"}]}}"#;
    const NO_AGENTS: &str = r#"{"result":{"agents":[]}}"#;
    const PANE: &str = r#"{"result":{"panes":[{"pane_id":"w1:p1","tab_id":"w1:t1","workspace_id":"w1","cwd":"CWD"}]}}"#;

    struct Fixture {
        _home: tempfile::TempDir,
        env: Env,
        root: PathBuf,
        project: Project,
    }

    fn fixture(pending: bool) -> Fixture {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        let project = project::create(&root, "demo", "", vec![]).unwrap();
        let socket = home.path().join("herdr.sock");
        std::fs::write(&socket, b"").unwrap();
        let cwd = project.dir().to_string_lossy().into_owned();
        project
            .update_coordinator(|c| {
                c.socket = socket.to_string_lossy().into_owned();
                c.workspace_id = "w1".into();
                c.tab_id = "w1:t1".into();
                c.pane_id = "w1:p1".into();
                c.agent_name = "hp-demo-coordinator".into();
                c.cwd = cwd;
                c.prime_pending = pending;
            })
            .unwrap();
        let env = Env::for_test(home.path(), &[]);
        Fixture { _home: home, env, root, project }
    }

    fn with_cwd(json: &str, fixture: &Fixture) -> String {
        json.replace("CWD", &fixture.project.dir().to_string_lossy())
    }

    #[test]
    fn shared_root_fallback_observes_without_prompts_metadata_or_copy_receipts() {
        let f=fixture(true);let other=project::create(&f.root,"other","",vec![]).unwrap();
        let t=thread::allocate(&f.project,|t| {
            t.status=thread::Status::Open;t.last_group="working".into();t.last_state="working".into();
            t.workspace_id="w1".into();t.tab_id="w1:t1".into();t.pane_id="w1:p1".into();t.agent_name="hp-demo-coordinator".into();t.cwd=f.project.dir().to_string_lossy().into_owned();
            t.thread_dir=f.project.dir().join("worker").to_string_lossy().into_owned();
        }).unwrap();
        std::fs::create_dir(&t.thread_dir).unwrap();std::fs::write(t.report_path(),"new uncollected report").unwrap();
        let pending=thread::allocate(&f.project,|t| {t.status=thread::Status::Open;t.prompt_pending=true;t.pane_id="w1:p1".into();}).unwrap();
        let state=br#"{"event_sequence":7,"pending_events":{"saved":{"id":"saved","kind":"test","subject":"saved","summary":"keep","body":"keep","retry":{"attempts":2,"blocked":true}}},"session_item_written":true}"#;
        std::fs::write(f.project.dir().join(".state/ticker.json"),state).unwrap();
        let runner=FakeRunner::new();runner.on("agent list",ok(&with_cwd(AGENT_READY,&f)));runner.on("pane list",ok(&with_cwd(PANE,&f)));
        let ctx=Ctx{env:&f.env,root:f.root.clone(),config_dir:f.root.join("cfg"),runner:&runner,detached_ticker:false};
        let guard=herdr_projects::execution_guard::ProjectGuard::acquire(&other.dir()).unwrap();
        assert!(tick_cheap(&ctx,&f.project).unwrap().is_some());
        let current=thread::load(&f.project,&t.id).unwrap();assert_eq!(current.last_state,"idle");assert_eq!(current.last_group,"idle");assert_eq!(current.status_notice_sequence,1);assert!(current.pending_status_notice.is_none());
        assert!(current.report_hash.is_empty() && current.last_review_item_hash.is_empty());
        assert!(thread::load(&f.project,&pending.id).unwrap().prompt_pending);assert!(f.project.coordinator().unwrap().prime_pending);
        assert_eq!(runner.calls.borrow().len(),2);assert_eq!(std::fs::read(f.project.dir().join(".state/ticker.json")).unwrap(),state);
        assert!(tick_cheap(&ctx,&f.project).unwrap().is_some());assert_eq!(thread::load(&f.project,&t.id).unwrap().status_notice_sequence,1);
        // Same-project ownership and exclusive root maintenance still refuse.
        let own=herdr_projects::execution_guard::ProjectGuard::acquire(&f.project.dir()).unwrap();assert!(tick_cheap(&ctx,&f.project).is_err());drop(own);drop(guard);
        let root=crate::cleanup::lease(&f.root).unwrap();assert!(tick_cheap(&ctx,&f.project).is_err());drop(root);
    }

    #[test]
    fn inactive_status_is_rechecked_inside_each_pass_lease() {
        for status in [Status::Paused, Status::Archived] {
            let f = fixture(true);
            let runner = FakeRunner::new();
            runner.on("agent list", ok(r#"{"result":{"agents":[]}}"#));
            runner.on("pane list", ok(&with_cwd(PANE, &f)));
            runner.on("report-metadata", ok("{}"));
            runner.on("agent start", ok(r#"{"result":{}}"#));
            let ctx = Ctx { env: &f.env, root: f.root.clone(), config_dir: f.root.join("cfg"), runner: &runner, detached_ticker: false };
            f.project.set_status(status).unwrap();
            assert!(tick_cheap(&ctx, &f.project).unwrap().is_none());
            assert!(runner.calls.borrow().is_empty());
            f.project.set_status(Status::Active).unwrap();
            let seen = tick_cheap(&ctx, &f.project).unwrap().unwrap();
            let calls = runner.calls.borrow().len();
            // A user pause/archive completes after cheap observations but before
            // the slow pass obtains its lease and could start the coordinator.
            f.project.set_status(status).unwrap();
            assert!(tick_slow(&ctx, &f.project, &seen, &mut Memory::new(&ctx)).is_empty());
            assert_eq!(runner.calls.borrow().len(), calls);
            assert_eq!(runner.count("agent start"), 0);
            assert!(f.project.coordinator().unwrap().prime_pending);
        }
    }

    #[test]
    fn pending_prime_is_delivered_only_to_a_ready_agent() {
        let f = fixture(true);
        let runner = FakeRunner::new();
        runner.on("agent list", ok(&with_cwd(AGENT_BLOCKED, &f)));
        runner.on("pane list", ok(&with_cwd(PANE, &f)));
        runner.on("report-metadata", ok("{}"));
        let ctx = Ctx { env: &f.env, root: f.root.clone(), config_dir: f.root.join("cfg"), runner: &runner, detached_ticker: false };
        assert!(tick_project(&ctx, &f.project).unwrap());
        assert_eq!(runner.count("agent prompt"), 0);
        assert!(f.project.coordinator().unwrap().prime_pending);

        let runner = FakeRunner::new();
        runner.on("agent list", ok(&with_cwd(AGENT_READY, &f)));
        runner.on("pane list", ok(&with_cwd(PANE, &f)));
        runner.on("agent prompt", ok(r#"{"result":{}}"#));
        runner.on("report-metadata", ok("{}"));
        let ctx = Ctx { env: &f.env, root: f.root.clone(), config_dir: f.root.join("cfg"), runner: &runner, detached_ticker: false };
        assert!(tick_project(&ctx, &f.project).unwrap());
        assert_eq!(runner.count("agent prompt"), 1);
        assert!(!f.project.coordinator().unwrap().prime_pending);
        // The prompt went to the recorded socket.
        let calls = runner.calls.borrow();
        let prompt = calls.iter().find(|c| c.display().contains("agent prompt")).unwrap();
        assert!(prompt.env.iter().any(|(k, v)| k == "HERDR_SOCKET_PATH" && v == &f.project.coordinator().unwrap().socket));
    }

    #[test]
    fn rejected_prime_stays_pending() {
        let f = fixture(true);
        let runner = FakeRunner::new();
        runner.on("agent list", ok(&with_cwd(AGENT_READY, &f)));
        runner.on("pane list", ok(&with_cwd(PANE, &f)));
        runner.on("agent prompt", fail(1, r#"{"error":{"code":"agent_blocked","message":"blocked"}}"#));
        let ctx = Ctx { env: &f.env, root: f.root.clone(), config_dir: f.root.join("cfg"), runner: &runner, detached_ticker: false };
        assert!(tick_project(&ctx, &f.project).is_err());
        assert!(f.project.coordinator().unwrap().prime_pending);
    }

    #[test]
    fn a_pane_with_other_identity_is_left_alone() {
        let f = fixture(true);
        let runner = FakeRunner::new();
        // Same ids, different working directory: not our pane.
        runner.on("agent list", ok(&AGENT_READY.replace("CWD", "/somewhere/else")));
        runner.on("pane list", ok(&PANE.replace("CWD", "/somewhere/else")));
        let ctx = Ctx { env: &f.env, root: f.root.clone(), config_dir: f.root.join("cfg"), runner: &runner, detached_ticker: false };
        assert!(tick_project(&ctx, &f.project).unwrap());
        assert_eq!(runner.count("agent prompt"), 0);
        assert_eq!(runner.count("agent start"), 0);
        assert_eq!(runner.count("report-metadata"), 0);
    }

    #[test]
    fn shell_prompt_pane_gets_at_most_three_launch_attempts() {
        let f = fixture(true);
        let runner = FakeRunner::new();
        runner.on("agent list", ok(NO_AGENTS));
        runner.on("pane list", ok(&with_cwd(PANE, &f)));
        runner.on("agent start", fail(1, r#"{"error":{"code":"timeout","message":"no agent"}}"#));
        let ctx = Ctx { env: &f.env, root: f.root.clone(), config_dir: f.root.join("cfg"), runner: &runner, detached_ticker: false };
        for _ in 0..5 {
            let _ = tick_project(&ctx, &f.project);
        }
        assert_eq!(runner.count("agent start"), 3);
        assert_eq!(runner.count("agent prompt"), 0);
    }

    #[test]
    fn unreachable_session_reads_no_state() {
        let f = fixture(true);
        let runner = FakeRunner::new();
        runner.on("agent list", fail(1, "connection refused"));
        let ctx = Ctx { env: &f.env, root: f.root.clone(), config_dir: f.root.join("cfg"), runner: &runner, detached_ticker: false };
        assert!(!tick_project(&ctx, &f.project).unwrap());

        // A socket file that is gone is not even called.
        std::fs::remove_file(f.project.coordinator().unwrap().socket).unwrap();
        let runner = FakeRunner::new();
        let ctx = Ctx { env: &f.env, root: f.root.clone(), config_dir: f.root.join("cfg"), runner: &runner, detached_ticker: false };
        assert!(!tick_project(&ctx, &f.project).unwrap());
        assert!(runner.calls.borrow().is_empty());
    }

    #[test]
    fn log_is_capped() {
        let dir = tempfile::tempdir().unwrap();
        let log = Log { path: dir.path().join("log") };
        let long = "x".repeat(10_000);
        for _ in 0..150 {
            log.line(&long);
        }
        let size = std::fs::metadata(&log.path).unwrap().len();
        assert!(size <= LOG_CAP, "{size}");
        assert!(size > LOG_CAP / 4);
    }
}

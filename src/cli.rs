use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use crate::coordinator::{self, OpenOptions};
use crate::paths::{self, Ctx, Env, SessionFlags};
use crate::project::{self, Project, Status};
use crate::runner::RealRunner;
use crate::threads::{self, ResolveArgs, StartArgs};
use crate::{actions, adopt, doctor, inbox, lifecycle, overview, routine, ticker};

#[derive(Parser)]
#[command(name = "herdr-projects", version = crate::VERSION, about = "Projects for herdr")]
struct Cli {
    /// Projects root (default: $HERDR_PROJECTS_ROOT, then config.toml, then ~/.herdr-projects)
    #[arg(long, global = true, value_name = "DIR")]
    root: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Args, Clone, Default)]
pub struct SessionArgs {
    /// herdr session name
    #[arg(long, value_name = "NAME", conflicts_with = "socket")]
    session: Option<String>,
    /// herdr socket path
    #[arg(long, value_name = "PATH")]
    socket: Option<PathBuf>,
}

impl From<SessionArgs> for SessionFlags {
    fn from(args: SessionArgs) -> Self {
        SessionFlags {
            session: args.session,
            socket: args.socket,
        }
    }
}

#[derive(Subcommand)]
enum RepairCommand {
    /// Print machine-readable diagnostics without changing records
    Inspect,
    /// Restore a record after checking its inspected SHA-256; ticker must be stopped
    Restore {
        path: PathBuf,
        #[arg(long)] from: PathBuf,
        #[arg(long)] expected_hash: String,
    },
}

#[derive(Subcommand)]
enum ProfileCommand {
    /// Validate one named profile and print redacted JSON; does not launch an agent
    Inspect { name: String },
    /// Resolve a named profile or unique kind into a budget envelope; does not launch
    Resolve {
        name: Option<String>,
        /// Unique named profile whose kind field equals KIND; never profiles.<kind>
        #[arg(long)] agent: Option<String>,
    },
    /// Run bounded version probes against explicit local executables; no agent session
    Probe {
        name: String,
        #[arg(long)] herdr_executable: PathBuf,
        #[arg(long)] agent_executable: PathBuf,
    },
}

#[cfg(feature="state-store")]
#[derive(Subcommand)]
enum ApprovalCommand {
    Policy,
    Inspect,
    Import { document:PathBuf, signature:PathBuf, #[arg(long)] expected_head:u64 },
    Revoke { id:String, #[arg(long)] expected_head:u64, #[arg(long)] reason:String },
    Denials,
}

#[cfg(feature="state-store")]
#[derive(Subcommand)]
enum BudgetCommand {
    Inspect,
    Import { document:PathBuf, signature:PathBuf, #[arg(long)] expected_head:u64 },
}
#[cfg(feature="state-store")]
#[derive(Subcommand)]
enum MemoryCommand {
    Inspect,
    Import {
        document:Option<PathBuf>,
        signature:Option<PathBuf>,
        #[arg(long)] expected_head:Option<u64>,
        #[arg(long)] file:Option<PathBuf>,
        #[arg(long)] expected_revision:Option<u64>,
    },
    Preview { #[arg(long)] file:PathBuf },
    Plan { #[arg(long)] output:PathBuf },
    Cutover {
        #[arg(long)] plan:PathBuf,
        document:PathBuf,
        signature:PathBuf,
        #[arg(long)] expected_head:u64,
        #[arg(long)] writers_stopped:bool,
    },
    Snapshot { #[arg(long)] task:String, #[arg(long)] profile:String, #[arg(long)] input_file:PathBuf },
    Propose { #[arg(long)] input:PathBuf },
    Review { #[arg(long)] proposal:String, #[arg(long)] decision_file:PathBuf },
    Promote { #[arg(long)] proposal:String, #[arg(long)] decision:String },
}
#[cfg(feature="state-store")]
#[derive(Subcommand)]
enum RoutineStoreCommand {
    Inspect,
    Import { document:PathBuf, signature:PathBuf, #[arg(long)] expected_head:u64 },
    Schedule { name:String, #[arg(long)] expected_head:u64 },
    Execute { operation:String, #[arg(long)] expected_head:u64 },
}

#[derive(Subcommand)]
enum NotificationCommand {
    Inspect,
    /// Suppress another delivery after inspecting the existing outcome
    Acknowledge {#[arg(long)] sequence:u64},
    /// Explicitly authorize a retry that may duplicate the previous delivery
    Retry {#[arg(long)] sequence:u64,#[arg(long)] accept_possible_duplicate:bool},
}

#[derive(Subcommand)]
enum Command {
    /// Inspect or explicitly reconcile uncertain legacy inbox delivery
    Notification {slug:String,#[command(subcommand)] command:NotificationCommand},
    /// Inspect owner signing policy and manage signed launch approvals
    #[cfg(feature="state-store")]
    Approval { slug:String, #[command(subcommand)] command:ApprovalCommand },
    #[cfg(feature="state-store")]
    Budget { slug:String, #[command(subcommand)] command:BudgetCommand },
    /// Memory inspect, signed policy, markdown import, snapshot and cutover
    #[cfg(feature="state-store")]
    Memory { slug:String, #[command(subcommand)] command:MemoryCommand },
    /// Signed durable routine control (does not execute scripts)
    #[cfg(feature="state-store")]
    RoutineStore { slug:String, #[command(subcommand)] command:RoutineStoreCommand },
    /// Inspect user-owned profile configuration and unresolved capability requirements
    Profile { #[command(subcommand)] command: ProfileCommand },
    /// Inspect dependency queue and configure per-project scheduling limits
    #[cfg(feature="state-store")]
    Scheduler { slug:String, #[command(subcommand)] command:SchedulerCommand },
    /// Inspect or explicitly rebind migrated runtime routing without granting ownership
    #[cfg(feature="state-store")]
    Runtime { slug:String, #[command(subcommand)] command:RuntimeCommand },
    /// Observe recorded runtime identities; --record persists evidence without dispatch
    #[cfg(feature="state-store")]
    Reconcile { slug:String, #[arg(long,conflicts_with="plan")] record:bool, #[arg(long)] plan:bool },
    /// Inspect or edit migrated task records without starting execution
    #[cfg(feature="state-store")]
    Task { slug:String, #[command(subcommand)] command:TaskCommand },
    /// Inspect durable delivery state (no external effects)
    #[cfg(feature="state-store")]
    Operations { slug:String, #[command(subcommand)] command:OperationsCommand },
    /// Inspect or explicitly migrate a paused project into the opt-in store
    #[cfg(feature = "state-store")]
    Migration {
        slug: String,
        #[command(subcommand)] command: MigrationCommand,
    },
    /// Inspect damaged records or restore a validated replacement with a backup
    Repair {
        slug: String,
        #[command(subcommand)]
        command: RepairCommand,
    },
    /// Create a project folder with its skeleton files
    New {
        name: String,
        #[arg(long, default_value = "")]
        goal: String,
        /// A repository, as PATH or PATH@MACHINE; repeatable
        #[arg(long = "repo", value_name = "PATH[@MACHINE]")]
        repos: Vec<String>,
    },
    /// List projects
    List {
        /// Include archived projects
        #[arg(long)]
        all: bool,
    },
    /// Open a project: its workspace, coordinator tab and coordinator agent
    Open {
        slug: String,
        /// Send the priming prompt again
        #[arg(long)]
        reprime: bool,
        /// Move the project to this session when its recorded socket no longer exists
        #[arg(long)]
        rebind: bool,
        #[command(flatten)]
        session: SessionArgs,
    },
    /// Print the digest the coordinator reads at the start of every turn
    Context {
        slug: String,
        /// Print without recording the inbox items as seen
        #[arg(long)]
        peek: bool,
        /// Named profile; defaults to profiles.planner. Never kind-default.
        #[arg(long)]
        profile: Option<String>,
        /// Acknowledge a checkpoint id from a previous context
        #[arg(long, conflicts_with="peek")]
        ack: Option<String>,
    },
    /// Print threads grouped by what needs you
    Overview {
        slug: Option<String>,
        /// Wait for Enter before exiting (only when on a terminal; used by the popup)
        #[arg(long)]
        wait: bool,
    },
    /// Show only one project's panes in the sidebar, sorted by attention
    Focus { slug: Option<String> },
    /// Clear the sidebar view (herdr holds one, so this clears any tool's view)
    Unfocus {
        #[command(flatten)]
        session: SessionArgs,
    },
    /// Inbox items
    Inbox {
        #[command(subcommand)]
        command: InboxCommand,
    },
    /// Threads: the project's worker agents
    Thread {
        #[command(subcommand)]
        command: ThreadCommand,
    },
    /// Routines: scheduled prompts and watched commands
    Routine {
        #[command(subcommand)]
        command: RoutineCommand,
    },
    /// Pause a project: the ticker skips it and `thread start` is refused
    Pause { slug: String },
    /// Make a paused project active again
    Resume { slug: String },
    /// Archive a project: paused, hidden, tokens cleared, `open` refused
    Archive { slug: String },
    /// Make an archived project active again
    Unarchive { slug: String },
    /// Move a project folder to the trash (no worktree, branch or PR is touched)
    Delete {
        slug: String,
        /// Delete even though coordinator or thread panes are alive
        #[arg(long)]
        force: bool,
    },
    /// Continue the current workspace's agent pane as a new project
    AdoptWorkspace {
        /// Project name (default: the workspace label herdr passes to the action)
        #[arg(long)]
        name: String,
        #[arg(long, default_value = "")]
        goal: String,
        /// The agent pane to adopt
        #[arg(long)]
        pane: String,
        /// The workspace's directory (the project's repo when it is a git repository)
        #[arg(long, default_value = "")]
        workspace_cwd: String,
        #[command(flatten)]
        session: SessionArgs,
    },
    /// Run by herdr's action menu
    #[command(hide = true)]
    Action { id: String },
    /// Run inside a plugin popup pane
    #[command(hide = true)]
    Pane { id: String },
    /// Read-only bounded report observation, before environment resolution.
    #[command(hide = true)]
    ReportHash { #[arg(long)] path:PathBuf },
    /// Versioned binary artifact transport for remote preservation.
    #[command(hide = true)]
    ArtifactStream {
        #[arg(long)]
        live: bool,
        #[arg(long, conflicts_with = "path")]
        probe: bool,
        #[arg(long, required_unless_present = "probe")]
        path: Option<PathBuf>,
    },
    /// Safety settings
    Safety {
        #[command(subcommand)]
        command: SafetyCommand,
    },
    /// Print the coordinator skill
    Skill,
    /// Check the setup: versions, tools, root, ticker and each project's session
    Doctor {
        #[command(flatten)]
        session: SessionArgs,
    },
    /// The background ticker
    Ticker {
        #[command(subcommand)]
        command: TickerCommand,
    },
}

#[derive(Subcommand)]
enum InboxCommand {
    #[cfg(feature="state-store")]
    /// Inspect canonical inbox records for a migrated project
    List { slug:String },
    /// Move handled items to inbox/done/
    Done {
        slug: String,
        #[arg(value_name = "ITEM_ID", required_unless_present = "all")]
        ids: Vec<String>,
        #[arg(long, conflicts_with = "ids")]
        all: bool,
    },
}

#[derive(Subcommand)]
enum ThreadCommand {
    /// Start a thread: a worktree workspace for --repo, else a tab in the project workspace
    Start {
        slug: String,
        #[arg(long)]
        title: String,
        #[arg(long, value_name = "PATH")]
        repo: Option<String>,
        #[arg(long, value_name = "LABEL")]
        machine: Option<String>,
        /// Agent kind (default: thread_agent in PROJECT.md)
        #[arg(long, value_name = "KIND")]
        agent: Option<String>,
        #[arg(long, value_name = "REF")]
        base: Option<String>,
        /// The task; `-` reads standard input
        #[arg(long, value_name = "FILE")]
        task_file: String,
    },
    /// Bring back a thread whose pane is gone or whose start failed
    Restart { slug: String, id: String },
    /// Send a follow-up to a thread's agent
    Prompt {
        slug: String,
        id: String,
        /// The text; `-` reads standard input
        #[arg(long, value_name = "FILE")]
        text_file: String,
    },
    /// List threads with live state and group
    List { slug: String },
    /// Show one thread's record
    Show { slug: String, id: String },
    /// Record an existing local agent pane as a thread of this project
    Adopt {
        slug: String,
        #[arg(long, value_name = "ID")]
        pane: String,
        #[arg(long)]
        title: String,
        /// Optional task; `-` reads standard input
        #[arg(long, value_name = "FILE")]
        task_file: Option<String>,
    },
    /// Record that the user has seen the current report
    Ack { slug: String, id: String },
    /// Resolve a thread (final copy first), or reopen a resolved one
    Resolve {
        slug: String,
        id: String,
        #[arg(long, conflicts_with_all = ["remove_worktree", "skip_copy", "discard_uncopied"])]
        reopen: bool,
        /// Remove an exclusively owned local worktree after verified preservation
        #[arg(long)]
        remove_worktree: bool,
        /// Confirm all known artifact writers have stopped; Linux process checks still apply
        #[arg(long, requires = "remove_worktree")]
        writers_stopped: bool,
        /// Resolve even though the final copy cannot be made
        #[arg(long)]
        skip_copy: bool,
        /// Accept uncopied artifacts; does not bypass writer or ownership checks
        #[arg(long, requires = "remove_worktree")]
        discard_uncopied: bool,
    },
}

/// `-` is standard input; a relative path is relative to the caller's directory.
fn read_text(file: &str) -> Result<String> {
    use std::io::Read;
    if file == "-" {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        Ok(text)
    } else {
        std::fs::read_to_string(file).map_err(|e| anyhow::anyhow!("could not read {file}: {e}"))
    }
}

#[derive(Subcommand)]
enum RoutineCommand {
    /// Approve a routine's command (a person at a terminal only)
    Approve { slug: String, name: String },
    /// List routines with their approval status
    List { slug: String },
}

#[derive(Subcommand)]
enum SafetyCommand {
    /// Print the effective safety settings and the config.toml table to edit
    Show { slug: String },
}

#[derive(Subcommand)]
enum TickerCommand {
    /// Start the ticker if it is not running (does nothing when there are no projects)
    Start,
    /// Run the ticker loop in the foreground
    Run,
    /// Ask the running ticker to exit and wait for it
    Stop,
    /// Show the running ticker's version, root and tool resolution
    Status,
}

#[cfg(feature = "state-store")]
#[derive(Subcommand)]
enum MigrationCommand {
    /// Inspect imported execution identities; none imply verified resource ownership
    Bindings,
    Inspect,
    /// Read-only storage, config and recorded live identity diagnostics
    Preflight,
    /// Explicitly upgrade a supported migrated store schema
    UpgradeStore,
    /// Produce a deterministic dry-run plan; output must be outside the project
    Plan { #[arg(long)] output: PathBuf },
    Apply { #[arg(long)] plan: PathBuf, #[arg(long)] writers_stopped: bool },
    Recover { #[arg(long)] writers_stopped: bool },
    Status,
    /// Cancel before cutover, preserving all staged data and backups
    Abort,
    Export,
    /// Restore verified legacy bytes into a new, separate recovery directory
    Restore { #[arg(long)] destination: PathBuf },
}

#[cfg(feature="state-store")]
#[derive(Subcommand)]
enum TaskCommand {
    /// Record cancellation; release capacity only with proof launch was never claimed
    CancelAttempt { attempt:String, #[arg(long)] expected_revision:u64, #[arg(long)] expected_head:u64, #[arg(long)] reason:String },
    Queue { id:String, #[arg(long)] input_file:PathBuf, #[arg(long)] expected_revision:u64, #[arg(long)] expected_head:u64 },
    List,
    Show { id:String },
    Add { id:String, #[arg(long)] title:String, #[arg(long)] expected_head:u64 },
    Rename { id:String, #[arg(long)] title:String, #[arg(long)] expected_revision:u64, #[arg(long)] expected_head:u64 },
}
#[cfg(feature="state-store")]
#[derive(Subcommand)]
enum SchedulerCommand {
    Inspect,
    Policy { #[arg(long)] max_active_workers:u32, #[arg(long)] max_attempts_per_task:u32, #[arg(long)] expected_revision:u64, #[arg(long)] expected_head:u64 },
}
#[cfg(feature="state-store")]
#[derive(Subcommand)]
enum OperationsCommand { Inspect,
    /// Queue preservation of a recorded local artifact source; completion awaits review
    Finalize { binding:String, #[arg(long)] reason:String, #[arg(long)] expected_head:u64 },
    DeliverFinalization { id:String, #[arg(long)] expected_revision:u64 },
    /// Verify a durable snapshot receipt after interrupted delivery, without recopying
    ObserveFinalization { id:String, #[arg(long)] expected_revision:u64, #[arg(long)] expected_head:u64 },
    /// Explicitly authorize a session notification for the current unseen inbox set
    Notify { task:String, #[arg(long)] expected_head:u64 },
    /// Deliver one authorized session notification; uncertain outcomes are not replayed
    DeliverNotification { id:String, #[arg(long)] expected_revision:u64 },
    /// Stop future delivery without claiming an earlier effect is absent
    Retire { id:String, #[arg(long)] reason:String, #[arg(long)] expected_revision:u64, #[arg(long)] expected_head:u64 },
    /// Preview exact legacy receipts; missing evidence never authorizes retry
    ReceiptPlan,
    /// Confirm only imported operations with matching durable legacy receipts
    ObserveImported { #[arg(long)] expected_head:u64 },
    /// Mark expired claims ambiguous; never replay external effects
    Expire,
    /// Deliver only internal inbox obligations atomically; no terminal/network effects
    DrainInbox { #[arg(long)] expected_head:u64 },
}

#[cfg(feature="state-store")]
#[derive(Subcommand)]
enum RuntimeCommand {
    /// Withdraw an ownership claim without stopping or deleting any resource
    Relinquish { id:String, #[arg(long)] expected_revision:u64, #[arg(long)] expected_head:u64, #[arg(long)] reason:String },
    Inspect,
    /// Adopt exactly observed local resources; no prompt, launch or destructive cleanup
    Adopt { id:String, #[arg(long)] expected_revision:u64, #[arg(long)] expected_head:u64 },
    /// Register a new coordinator (no task) or task binding; does not adopt resources
    Create { #[arg(long,requires="task_revision")] task:Option<String>, #[arg(long,requires="task")] task_revision:Option<u64>, #[arg(long)] route:PathBuf, #[arg(long)] expected_head:u64 },
    Admission,
    /// Set canonical lifecycle state; resume requires current reconciliation evidence
    State { #[arg(value_parser=["paused","active","archived"])] state:String, #[arg(long)] expected_revision:u64, #[arg(long)] expected_head:u64 },
    /// Read replacement routing JSON; require the exact binding revision and event head
    Rebind { id:String, #[arg(long)] route:PathBuf, #[arg(long)] expected_revision:u64, #[arg(long)] expected_head:u64 },
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    if let Command::ReportHash {path}=&cli.command {
        println!("{}",serde_json::to_string(&crate::local_reports::Observation{hash:crate::source_tree::report_hash(path)?})?);return Ok(());
    }
    if let Command::ArtifactStream { probe, path, live } = &cli.command {
        if *probe { crate::artifacts::probe(); return Ok(()); }
        if *live {return crate::artifacts::live::export(path.as_ref().context("artifact source path is required")?, &mut std::io::stdout().lock());}
        return crate::artifacts::export(path.as_ref().context("artifact source path is required")?, &mut std::io::stdout().lock());
    }
    let env = Env::from_process()?;
    let config_dir = env.config_dir();
    if let Command::Profile { command } = &cli.command {
        let path = config_dir.join("config.toml");
        let value = match command {
            ProfileCommand::Inspect { name } => serde_json::to_value(crate::agents::profiles::inspect(&path, name)?)?,
            ProfileCommand::Resolve { name, agent } => {
                let resolved = match (name.as_deref(), agent.as_deref()) {
                    (Some(name), None) => crate::agents::resolve::resolve(name, &path, None)?,
                    (None, Some(kind)) => crate::agents::resolve::resolve_agent_kind(kind, &path, None)?,
                    _ => bail!("profile resolve requires NAME or --agent KIND"),
                };
                serde_json::to_value(resolved)?
            }
            ProfileCommand::Probe { name, herdr_executable, agent_executable } =>
                serde_json::to_value(crate::agents::probe::probe(&path, name, herdr_executable, agent_executable, &RealRunner)?)?,
        };
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    let root = paths::resolve_root(cli.root.as_deref(), &env, &config_dir)?;
    let runner = RealRunner;
    let ctx = Ctx {
        env: &env,
        root,
        config_dir,
        runner: &runner,
        detached_ticker: true,
    };

    match cli.command {
        Command::Notification{slug,command}=>{
            let p=project::Project::load(&ctx.root,&slug)?;
            match command {
                NotificationCommand::Inspect=>println!("{}",serde_json::to_string_pretty(&crate::coordinator_jobs::inspect_notification(&p)?)?),
                NotificationCommand::Acknowledge{sequence}=>{crate::coordinator_jobs::reconcile_notification(&ctx,&p,sequence,false)?;println!("Notification {sequence} acknowledged without another delivery.");},
                NotificationCommand::Retry{sequence,accept_possible_duplicate}=>{anyhow::ensure!(accept_possible_duplicate,"retry may duplicate delivery; pass --accept-possible-duplicate after inspecting the outcome");crate::coordinator_jobs::reconcile_notification(&ctx,&p,sequence,true)?;crate::ticker::start(&ctx)?;println!("Explicit notification retry queued.");},
            }Ok(())
        },
        #[cfg(feature="state-store")]
        Command::Memory{slug,command}=>{
            project::validate_slug(&slug)?;let dir=ctx.root.join(slug);
            let value=match command {
                MemoryCommand::Inspect=>{
                    let s=herdr_projects::runtime::snapshot(&dir)?;
                    let mut db=herdr_projects::migration::open_active(&dir)?;
                    let records=db.memory_records().unwrap_or_default();
                    let facts=db.active_facts(jiff::Timestamp::now().as_millisecond()).unwrap_or_default();
                    let authority=herdr_projects::migration::read_format(&dir).map(|f|f.memory).unwrap_or_else(|_| "legacy-markdown".into());
                    serde_json::json!({"head":s.head,"authority":authority,"policies":s.memory_policies,"records":records,"active_facts":facts})
                },
                MemoryCommand::Import{document,signature,expected_head,file,expected_revision}=>{
                    if let Some(file)=file {
                        serde_json::to_value(herdr_projects::memory::import_file(&dir,&file,expected_revision)?)?
                    } else if document.is_none() && signature.is_none() {
                        let plan=herdr_projects::memory::plan(&dir)?;
                        serde_json::to_value(herdr_projects::memory::import_plan(&dir,&plan)?)?
                    } else {
                        let document=document.context("memory import requires a signed document, --file, or no arguments to import the current inventory")?;
                        let signature=signature.context("memory import requires a signature or --file")?;
                        let expected_head=expected_head.context("signed memory import requires --expected-head")?;
                        serde_json::to_value(herdr_projects::authority::import_memory(&dir,&document,&signature,expected_head)?)?
                    }
                },
                MemoryCommand::Preview{file}=>serde_json::to_value(herdr_projects::memory::preview(&dir,&file)?)?,
                MemoryCommand::Plan{output}=>{
                    let plan=herdr_projects::memory::plan(&dir)?;
                    std::fs::write(&output,serde_json::to_vec_pretty(&plan)?)?;
                    serde_json::to_value(plan)?
                },
                MemoryCommand::Cutover{plan,document,signature,expected_head,writers_stopped}=>
                    serde_json::to_value(herdr_projects::authority::cutover_memory(&dir,&plan,&document,&signature,expected_head,writers_stopped)?)?,
                MemoryCommand::Snapshot{task,profile,input_file}=>{
                    anyhow::ensure!(task!="coordinator","--task coordinator is reserved for the coordinator constructor");
                    let resolved=crate::agents::resolve::resolve(&profile,&ctx.config_dir.join("config.toml"),None)?;
                    let request:herdr_projects::domain::SnapshotRequest=serde_json::from_slice(&herdr_projects::migration::read_plan_file(&input_file)?).map_err(|_|anyhow::anyhow!("invalid snapshot scope JSON (contents withheld)"))?;
                    anyhow::ensure!(request.task_id==task,"scope task_id must match --task");
                    let instructions=std::fs::read_to_string(dir.join("PROJECT.md")).unwrap_or_default();
                    let mut memory=herdr_projects::memory::MemoryStore::from_sqlite(herdr_projects::migration::open_active(&dir)?,dir.join(".state/objects"));
                    serde_json::to_value(memory.create_task_snapshot(request,&resolved.name,&resolved.definition_digest,Some(&resolved.config_digest),resolved.budget.soft_input_chars,&instructions,jiff::Timestamp::now().as_millisecond(),None)?)?
                },
                MemoryCommand::Propose{input}=>{
                    let bytes=herdr_projects::migration::read_plan_file(&input)?;
                    let mut memory=herdr_projects::memory::MemoryStore::from_sqlite(herdr_projects::migration::open_active(&dir)?,dir.join(".state/objects"));
                    serde_json::to_value(memory.propose(&bytes,jiff::Timestamp::now().as_millisecond())?)?
                },
                MemoryCommand::Review{proposal,decision_file}=>{
                    let bytes=herdr_projects::migration::read_plan_file(&decision_file)?;
                    let mut memory=herdr_projects::memory::MemoryStore::from_sqlite(herdr_projects::migration::open_active(&dir)?,dir.join(".state/objects"));
                    let decision=memory.review(&bytes,jiff::Timestamp::now().as_millisecond())?;
                    anyhow::ensure!(decision.proposal_id==proposal,"decision-file proposal_id must match --proposal");
                    serde_json::to_value(decision)?
                },
                MemoryCommand::Promote{proposal,decision}=>{
                    let mut memory=herdr_projects::memory::MemoryStore::from_sqlite(herdr_projects::migration::open_active(&dir)?,dir.join(".state/objects"));
                    serde_json::to_value(memory.promote(&proposal,&decision,jiff::Timestamp::now().as_millisecond())?)?
                },
            };println!("{}",serde_json::to_string_pretty(&value)?);Ok(())
        },
        #[cfg(feature="state-store")]
        Command::RoutineStore{slug,command}=>{
            project::validate_slug(&slug)?;let dir=ctx.root.join(slug);
            let value=match command {
                RoutineStoreCommand::Inspect=>{let s=herdr_projects::runtime::snapshot(&dir)?;serde_json::json!({"head":s.head,"revisions":s.routine_revisions,"occurrences":s.routine_occurrences,"receipts":s.routine_receipts,"execution_enabled":cfg!(target_os="linux"),"automatic_dispatch":cfg!(target_os="linux")})},
                RoutineStoreCommand::Import{document,signature,expected_head}=>serde_json::to_value(herdr_projects::authority::import_routine(&dir,&document,&signature,expected_head)?)?,
                RoutineStoreCommand::Schedule{name,expected_head}=>serde_json::to_value(herdr_projects::routines::schedule(&dir,&name,expected_head)?)?,
                RoutineStoreCommand::Execute{operation,expected_head}=>serde_json::to_value(herdr_projects::routines::execute(&dir,&herdr_projects::domain::OperationId::new(operation).map_err(anyhow::Error::msg)?,expected_head)?)?,
            };println!("{}",serde_json::to_string_pretty(&value)?);Ok(())
        },
        #[cfg(feature="state-store")]
        Command::Approval { slug, command } => {
            project::validate_slug(&slug)?;
            let dir=ctx.root.join(slug);
            let value=match command {
                ApprovalCommand::Policy=>serde_json::to_value(herdr_projects::authority::policy_reference(&dir)?)?,
                ApprovalCommand::Inspect=>serde_json::to_value(herdr_projects::runtime::snapshot(&dir)?.approvals)?,
                ApprovalCommand::Import { document,signature,expected_head }=>serde_json::to_value(herdr_projects::authority::import_signed(&dir,&document,&signature,expected_head)?)?,
                ApprovalCommand::Revoke { id,expected_head,reason }=>serde_json::json!({"head":herdr_projects::authority::revoke(&dir,&id,expected_head,&reason)?}),
                ApprovalCommand::Denials=>serde_json::to_value(herdr_projects::authority::denials(&dir)?)?,
            };
            println!("{}",serde_json::to_string_pretty(&value)?);
            Ok(())
        },
        Command::Profile { .. } => unreachable!("profile inspection handled before project resolution"),
        #[cfg(feature="state-store")]
        Command::Runtime{slug,command}=>{
            project::validate_slug(&slug)?;let dir=ctx.root.join(slug);
            match command {
                RuntimeCommand::Inspect=>{let snapshot=herdr_projects::runtime::snapshot(&dir)?;anyhow::ensure!(snapshot.schema_version>=5,"upgrade-store is required for runtime bindings");println!("{}",serde_json::to_string_pretty(&serde_json::json!({"head":snapshot.head,"bindings":snapshot.runtime_bindings,"observations":snapshot.observations,"ownership":snapshot.ownership,"control":snapshot.control}))?);},
                RuntimeCommand::Relinquish{id,expected_revision,expected_head,reason}=>println!("{}",herdr_projects::runtime::relinquish(&dir,&id,expected_revision,expected_head,&reason)?),
                RuntimeCommand::Adopt{id,expected_revision,expected_head}=>println!("{}",serde_json::to_string_pretty(&crate::runtime_ownership::adopt(&ctx,&dir,&id,expected_revision,expected_head)?)?),
                RuntimeCommand::Create{task,task_revision,route,expected_head}=>{
                    let task=task.map(herdr_projects::domain::TaskId::new).transpose().map_err(anyhow::Error::msg)?;
                    let bytes=herdr_projects::migration::read_plan_file(&route)?;
                    let route=serde_json::from_slice(&bytes).map_err(|_|anyhow::anyhow!("invalid runtime route JSON (contents withheld)"))?;
                    println!("{}",serde_json::to_string_pretty(&herdr_projects::runtime::create_binding(&dir,task.as_ref(),task_revision,expected_head,&route)?)?);
                },
                RuntimeCommand::Admission=>println!("{}",serde_json::to_string_pretty(&herdr_projects::runtime::admission(&dir,&std::path::absolute(ctx.config_dir.join("config.toml"))?)?)?),
                RuntimeCommand::State{state,expected_revision,expected_head}=>{
                    use herdr_projects::domain::ProjectState;
                    let state=match state.as_str(){"active"=>ProjectState::Active,"archived"=>ProjectState::Archived,_=>ProjectState::Paused};
                    println!("{}",serde_json::to_string_pretty(&herdr_projects::runtime::set_state(&dir,expected_head,expected_revision,state,&std::path::absolute(ctx.config_dir.join("config.toml"))?)?)?);
                },
                RuntimeCommand::Rebind{id,route,expected_revision,expected_head}=>{
                    let bytes=herdr_projects::migration::read_plan_file(&route)?;
                    let route=serde_json::from_slice(&bytes).map_err(|_|anyhow::anyhow!("invalid runtime route JSON (contents withheld)"))?;
                    println!("{}",serde_json::to_string_pretty(&herdr_projects::runtime::rebind(&dir,&id,expected_revision,expected_head,&route)?)?);
                },
            }
            Ok(())
        },
        #[cfg(feature="state-store")]
        Command::Reconcile{slug,record,plan}=>{
            project::validate_slug(&slug)?;
            if plan {println!("{}",serde_json::to_string_pretty(&crate::reconcile_live::plan(&ctx,&ctx.root.join(slug))?)?);}else{println!("{}",serde_json::to_string_pretty(&crate::reconcile_live::run(&ctx,&ctx.root.join(slug),record)?)?);}
            Ok(())
        },
        #[cfg(feature="state-store")]
        Command::Scheduler{slug,command}=>{
            project::validate_slug(&slug)?;let dir=ctx.root.join(&slug);
            match command {
                SchedulerCommand::Inspect=>println!("{}",serde_json::to_string_pretty(&herdr_projects::runtime::queue_report(&dir)?)?),
                SchedulerCommand::Policy{max_active_workers,max_attempts_per_task,expected_revision,expected_head}=>println!("{}",herdr_projects::runtime::scheduler_policy(&dir,expected_head,expected_revision,max_active_workers,max_attempts_per_task)?),
            }Ok(())
        },
        #[cfg(feature="state-store")]
        Command::Task { slug,command } => {
            use herdr_projects::{domain::TaskId,runtime};
            project::validate_slug(&slug)?; let dir=ctx.root.join(&slug);
            match command {
                TaskCommand::Queue{id,input_file,expected_revision,expected_head}=>{
                    let bytes=herdr_projects::migration::read_plan_file(&input_file)?;
                    let request=serde_json::from_slice(&bytes).map_err(|_|anyhow::anyhow!("invalid queue request JSON"))?;
                    println!("{}",runtime::queue_task(&dir,&TaskId::new(id).map_err(anyhow::Error::msg)?,expected_revision,expected_head,&request)?);
                },
                TaskCommand::CancelAttempt{attempt,expected_revision,expected_head,reason}=>println!("{}",serde_json::to_string_pretty(&runtime::cancel_attempt(&dir,&herdr_projects::domain::AttemptId::new(attempt).map_err(anyhow::Error::msg)?,expected_revision,expected_head,&reason)?)?),
                TaskCommand::List=>println!("{}",serde_json::to_string_pretty(&runtime::snapshot(&dir)?)?),
                TaskCommand::Show{id}=>{
                    let id=TaskId::new(id).map_err(anyhow::Error::msg)?;
                    let task=runtime::snapshot(&dir)?.tasks.into_iter().find(|t|t.id==id).context("task not found")?;
                    println!("{}",serde_json::to_string_pretty(&task)?);
                },
                TaskCommand::Add{id,title,expected_head}=>println!("Committed task at event head {}. Use migration export to generate the new view.",runtime::add_task(&dir,TaskId::new(id).map_err(anyhow::Error::msg)?,title,expected_head)?),
                TaskCommand::Rename{id,title,expected_revision,expected_head}=>println!("Committed task at event head {}. Use migration export to generate the new view.",runtime::rename_task(&dir,&TaskId::new(id).map_err(anyhow::Error::msg)?,title,expected_revision,expected_head)?),
            }
            Ok(())
        },
        #[cfg(feature="state-store")]
        Command::Operations {slug,command}=>{
            project::validate_slug(&slug)?;
            let dir=ctx.root.join(&slug);
            match command {
                OperationsCommand::Finalize{binding,reason,expected_head}=>println!("{}",serde_json::to_string_pretty(&crate::finalization_delivery::enqueue(&ctx,&dir,&binding,expected_head,reason)?)?),
                OperationsCommand::DeliverFinalization{id,expected_revision}=>{
                    let id=herdr_projects::domain::OperationId::new(id).map_err(anyhow::Error::msg)?;
                    match crate::finalization_delivery::deliver(&ctx,&dir,&id,expected_revision)? {
                        herdr_projects::operations::dispatch::DispatchResult::Recorded(delivery)=>println!("{}",serde_json::to_string_pretty(&delivery)?),
                        herdr_projects::operations::dispatch::DispatchResult::Unrecorded{claim,..}=>anyhow::bail!("finalization outcome was not recorded; operation {} claim epoch {} requires receipt observation",claim.operation.as_str(),claim.epoch),
                    }
                },
                OperationsCommand::ObserveFinalization{id,expected_revision,expected_head}=>println!("{}",serde_json::to_string_pretty(&crate::finalization_delivery::observe(&ctx,&dir,&herdr_projects::domain::OperationId::new(id).map_err(anyhow::Error::msg)?,expected_revision,expected_head)?)?),
                OperationsCommand::Notify{task,expected_head}=>{
                    let task=herdr_projects::domain::TaskId::new(task).map_err(anyhow::Error::msg)?;
                    println!("{}",serde_json::to_string_pretty(&crate::notification_delivery::enqueue(&ctx,&dir,&task,expected_head)?)?);
                },
                OperationsCommand::DeliverNotification{id,expected_revision}=>{
                    let id=herdr_projects::domain::OperationId::new(id).map_err(anyhow::Error::msg)?;
                    match crate::notification_delivery::deliver(&ctx,&dir,&id,expected_revision)? {
                        herdr_projects::operations::dispatch::DispatchResult::Recorded(delivery)=>println!("{}",serde_json::to_string_pretty(&delivery)?),
                        herdr_projects::operations::dispatch::DispatchResult::Unrecorded{claim,..}=>anyhow::bail!("notification outcome was not recorded; operation {} claim epoch {} requires observation; do not retry",claim.operation.as_str(),claim.epoch),
                    }
                },
                OperationsCommand::Retire{id,reason,expected_revision,expected_head}=>println!("{}",serde_json::to_string_pretty(&herdr_projects::runtime::retire_operation(&dir,&herdr_projects::domain::OperationId::new(id).map_err(anyhow::Error::msg)?,expected_revision,expected_head,&reason)?)?),
                OperationsCommand::Inspect=>println!("{}",serde_json::to_string_pretty(&herdr_projects::migration::open_active(&dir)?.deliveries()?)?),
                OperationsCommand::ReceiptPlan=>println!("{}",serde_json::to_string_pretty(&herdr_projects::runtime::observe_imported_receipts(&dir,None)?)?),
                OperationsCommand::ObserveImported{expected_head}=>println!("{}",serde_json::to_string_pretty(&herdr_projects::runtime::observe_imported_receipts(&dir,Some(expected_head))?)?),
                OperationsCommand::Expire=>println!("{} expired claim(s) require observation",herdr_projects::runtime::expire_operations(&dir)?),
                OperationsCommand::DrainInbox{expected_head}=>println!("{} inbox obligation(s) delivered",herdr_projects::runtime::drain_inbox(&dir,expected_head)?),
            }
            Ok(())
        },
        #[cfg(feature = "state-store")]
        Command::Migration { slug, command } => {
            use herdr_projects::{migration, projections};
            project::validate_slug(&slug)?;
            let dir = ctx.root.join(&slug);
            match command {
                MigrationCommand::Bindings=>{
                    let snapshot=herdr_projects::runtime::snapshot(&dir)?;
                    anyhow::ensure!(snapshot.schema_version>=5,"upgrade-store is required for runtime bindings");
                    println!("{}",serde_json::to_string_pretty(&serde_json::json!({"head":snapshot.head,"bindings":snapshot.runtime_bindings}))?);
                },
                MigrationCommand::UpgradeStore => { migration::upgrade_active(&dir)?; println!("Store schema upgraded; dispatch remains blocked pending reconciliation."); },
                MigrationCommand::Preflight=>println!("{}",serde_json::to_string_pretty(&crate::migration_preflight::inspect(&ctx,&dir)?)?),
                MigrationCommand::Inspect => println!("{}", serde_json::to_string_pretty(&crate::migration_preflight::plan(&ctx,&dir)?)?),
                MigrationCommand::Plan { output } => {
                    let parent = output.parent().filter(|p|!p.as_os_str().is_empty()).unwrap_or(std::path::Path::new(".")).canonicalize()?;
                    anyhow::ensure!(!parent.starts_with(dir.canonicalize()?), "write the plan outside the source project");
                    let plan = crate::migration_preflight::plan(&ctx,&dir)?;
                    use std::io::Write;
                    use std::os::unix::fs::OpenOptionsExt;
                    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(output)?;
                    file.write_all(&serde_json::to_vec_pretty(&plan)?)?; file.sync_all()?;
                    println!("Plan written: {} blockers; source digest {}", plan.blockers.len(), plan.digest);
                },
                MigrationCommand::Apply { plan, writers_stopped } => {
                    let input = migration::read_plan_file(&plan)?;
                    anyhow::ensure!(input.len() <= 16*1024*1024,"plan exceeds 16 MiB");
                    let plan:migration::Plan=serde_json::from_slice(&input)?;
                    migration::require_config_path(&plan,&std::path::absolute(ctx.config_dir.join("config.toml"))?)?;
                    println!("{}",serde_json::to_string_pretty(&migration::apply(&dir,&plan,writers_stopped)?)?);
                },
                MigrationCommand::Recover { writers_stopped } => {
                    let journal=migration::status(&dir)?;
                    if journal.plan.version==2 && journal.phase!=migration::Phase::Active {
                        migration::require_config_path(&journal.plan,&std::path::absolute(ctx.config_dir.join("config.toml"))?)?;
                    }
                    println!("{}",serde_json::to_string_pretty(&migration::recover(&dir,writers_stopped)?)?);
                },
                MigrationCommand::Abort => println!("Preserved aborted migration at {}",migration::abort(&dir)?.display()),
                MigrationCommand::Status => println!("{}",serde_json::to_string_pretty(&migration::status(&dir)?)?),
                MigrationCommand::Export => {
                    let path = projections::export(&dir,&mut migration::open_active(&dir)?)?;
                    println!("{}",path.display());
                },
                MigrationCommand::Restore { destination } => { migration::restore_backup(&dir,&destination)?; println!("Restored verified backup to {}. Reconcile before running workers.",destination.display()); },
            }
            Ok(())
        },
        Command::Repair { slug, command } => {
            let project = Project::load(&ctx.root, &slug)?;
            match command {
                RepairCommand::Inspect => println!("{}", serde_json::to_string_pretty(&crate::repair::inspect(&project)?)?),
                RepairCommand::Restore { path, from, expected_hash } => {
                    let backup = crate::repair::restore(&ctx.root, &project, &path, &from, &expected_hash)?;
                    println!("Restored {}. Original saved at {backup}", path.display());
                }
            }
            Ok(())
        }
        #[cfg(feature="state-store")]
        Command::Budget { slug,command }=>{
            project::validate_slug(&slug)?;
            let dir=ctx.root.join(&slug);
            let value=match command {
                BudgetCommand::Inspect=>serde_json::to_value(herdr_projects::migration::open_active(&dir)?.budget_report()?)?,
                BudgetCommand::Import{document,signature,expected_head}=>serde_json::to_value(herdr_projects::authority::import_budget(&dir,&document,&signature,expected_head)?)?,
            };
            println!("{}",serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        Command::ReportHash {..}|Command::ArtifactStream { .. } => unreachable!("artifact transport handled before environment resolution"),
        Command::New { name, goal, repos } => {
            let repos = repos.iter().map(|arg| project::parse_repo_arg(arg)).collect();
            let project = project::create(&ctx.root, &name, &goal, repos)?;
            println!("created `{}` at {}", project.slug, project.dir().display());
            println!("next: {} open {}", coordinator::current_prefix(&ctx.root)?, project.slug);
            Ok(())
        }
        Command::List { all } => {
            for slug in project::list_slugs(&ctx.root) {
                if let Err(error) = project::ensure_legacy(&ctx.root.join(&slug)) {
                    println!("{slug}\tstore/maintenance\t{error}");
                    continue;
                }
                let project = Project::load(&ctx.root, &slug)?;
                let status = project.status();
                if status == Status::Archived && !all {
                    continue;
                }
                let mut counts = std::collections::BTreeMap::new();
                for row in threads::rows(&ctx, &project) {
                    *counts.entry(row.group.rank()).or_insert((row.group.label(), 0)) = (row.group.label(), counts.get(&row.group.rank()).map_or(0, |c: &(&str, usize)| c.1) + 1);
                }
                let summary: Vec<String> = counts.values().map(|(label, n)| format!("{label}: {n}")).collect();
                println!("{slug}\t{status}\t{}", if summary.is_empty() { "no threads".to_string() } else { summary.join(", ") });
            }
            Ok(())
        }
        Command::Open { slug, reprime, rebind, session } => coordinator::open(
            &ctx,
            &slug,
            &OpenOptions {
                session: session.into(),
                reprime,
                rebind,
            },
        ),
        Command::Context { slug, peek, profile, ack } => {
            #[cfg(feature="state-store")]
            {
                project::validate_slug(&slug)?;
                if project::ensure_legacy(&ctx.root.join(&slug)).is_err() {
                    let dir=ctx.root.join(&slug);
                    if let Some(checkpoint)=ack {
                        let acked=herdr_projects::runtime::ack_checkpoint(&dir,&checkpoint)?;
                        println!("Checkpoint {} acknowledged (cursor_seq={}).",acked.id,acked.through_seq);
                        return Ok(());
                    }
                    let config=ctx.config_dir.join("config.toml");
                    let resolved=match profile.as_deref() {
                        Some(name)=>crate::agents::resolve::resolve(name,&config,None)
                            .with_context(||format!("context --profile {name} is not a named profile"))?,
                        None=>crate::agents::resolve::resolve("planner",&config,None)
                            .context("context requires profiles.planner or --profile NAME")?,
                    };
                    let herdr_session=project::Project::load(&ctx.root,&slug).ok()
                        .and_then(|p|p.coordinator())
                        .map(|c|c.session)
                        .filter(|s|!s.is_empty())
                        .unwrap_or_else(||"default".into());
                    let instructions=std::fs::read_to_string(dir.join("PROJECT.md")).unwrap_or_default();
                    let profile=herdr_projects::domain::CheckpointProfile{
                        name:resolved.name,digest:resolved.definition_digest,config_digest:Some(resolved.config_digest),
                        budget_chars:resolved.budget.soft_input_chars,
                    };
                    let result=herdr_projects::runtime::coordinator_context(&dir,&herdr_session,&profile,&instructions)
                        .context("legacy runtime is disabled; migrated context could not be read")?;
                    println!("{}",result.text);
                    if !peek&&!result.unseen.is_empty(){herdr_projects::runtime::update_inbox(&dir,result.head,&result.unseen,false)?;}
                    return Ok(());
                }
            }
            let _=(profile,ack);
            coordinator::context(&ctx,&slug,peek)
        },
        Command::Overview { slug, wait } => overview::run(&ctx, slug.as_deref(), wait),
        Command::Focus { slug } => overview::focus(&ctx, slug.as_deref()),
        Command::Unfocus { session } => overview::unfocus(&ctx, &session.into()),
        Command::Inbox { command } => match command {
            #[cfg(feature="state-store")]
            InboxCommand::List{slug}=>{
                project::validate_slug(&slug)?;
                println!("{}",serde_json::to_string_pretty(&herdr_projects::runtime::snapshot(&ctx.root.join(&slug))?.inbox)?);Ok(())
            },
            InboxCommand::Done { slug, ids, all } => {
                #[cfg(feature="state-store")]
                {
                    project::validate_slug(&slug)?;let dir=ctx.root.join(&slug);
                    if project::ensure_legacy(&dir).is_err() {
                        let snapshot=herdr_projects::runtime::snapshot(&dir)?;
                        let ids=if all {snapshot.inbox.iter().filter(|i|!i.done).map(|i|i.content.id.clone()).collect()}else{ids};
                        println!("{} inbox item(s) marked done",herdr_projects::runtime::update_inbox(&dir,snapshot.head,&ids,true)?);
                        return Ok(());
                    }
                }
                let project = Project::load(&ctx.root, &slug)?;
                let moved = inbox::done(&project, &ids, all)?;
                println!("{moved} item(s) moved to inbox/done");
                Ok(())
            }
        },
        Command::Thread { command } => match command {
            ThreadCommand::Start { slug, title, repo, machine, agent, base, task_file } => {
                let task = read_text(&task_file)?;
                let thread = threads::start(&ctx, &slug, StartArgs { title, repo, machine, agent, base, task })?;
                println!("{}", serde_json::json!({ "id": thread.id, "kind": thread.kind, "branch": thread.branch, "pane_id": thread.pane_id }));
                Ok(())
            }
            ThreadCommand::Restart { slug, id } => {
                let thread = threads::restart(&ctx, &slug, &id)?;
                println!("{} is back in pane {}; the ticker launches its agent", thread.id, thread.pane_id);
                Ok(())
            }
            ThreadCommand::Prompt { slug, id, text_file } => {
                let text = read_text(&text_file)?;
                let state = threads::prompt(&ctx, &slug, &id, &text)?;
                println!("sent to {id} (agent was {state})");
                Ok(())
            }
            ThreadCommand::Adopt { slug, pane, title, task_file } => {
                let task = task_file.map(|file| read_text(&file)).transpose()?;
                let thread = adopt::adopt(&ctx, &slug, &pane, &title, task)?;
                println!("{}", serde_json::json!({ "id": thread.id, "kind": thread.kind, "pane_id": thread.pane_id, "prompt_pending": thread.prompt_pending }));
                Ok(())
            }
            ThreadCommand::List { slug } => threads::print_list(&ctx, &slug),
            ThreadCommand::Show { slug, id } => threads::print_show(&ctx, &slug, &id),
            ThreadCommand::Ack { slug, id } => threads::ack(&ctx, &slug, &id),
            ThreadCommand::Resolve { slug, id, reopen, remove_worktree, writers_stopped, skip_copy, discard_uncopied } => {
                threads::resolve(&ctx, &slug, &id, &ResolveArgs { reopen, remove_worktree, writers_stopped, skip_copy, discard_uncopied })
            }
        },
        Command::Routine { command } => match command {
            RoutineCommand::Approve { slug, name } => {
                let project = Project::load(&ctx.root, &slug)?;
                routine::approve(&ctx.config_dir, &project, &name)
            }
            RoutineCommand::List { slug } => {
                let project = Project::load(&ctx.root, &slug)?;
                let commands = project.safety(&ctx.config_dir)?.routine_commands;
                routine::print_list(&ctx.config_dir, &project, commands);
                Ok(())
            }
        },
        Command::Pause { slug } => lifecycle::set_status(&ctx, &slug, Status::Paused),
        Command::Resume { slug } => {
            if Project::load(&ctx.root, &slug)?.status() == Status::Archived {
                bail!("`{slug}` is archived; use `unarchive`");
            }
            lifecycle::set_status(&ctx, &slug, Status::Active)
        }
        Command::Archive { slug } => lifecycle::set_status(&ctx, &slug, Status::Archived),
        Command::Unarchive { slug } => lifecycle::set_status(&ctx, &slug, Status::Active),
        Command::Delete { slug, force } => lifecycle::delete(&ctx, &slug, force),
        Command::AdoptWorkspace { name, goal, pane, workspace_cwd, session } => {
            adopt::adopt_workspace(&ctx, &adopt::AdoptWorkspace { name, goal, pane, workspace_cwd, session: session.into() })
        }
        Command::Action { id } => actions::run_action(&ctx, &id),
        Command::Pane { id } => actions::run_pane(&ctx, &id),
        Command::Safety { command } => match command {
            SafetyCommand::Show { slug } => {
                let project = Project::load(&ctx.root, &slug)?;
                let safety = project.safety(&ctx.config_dir)?;
                println!("Effective safety settings for `{slug}`:");
                println!("  start_threads = {:?}", safety.start_threads);
                println!("  coordinator_agent_args = {:?}", safety.coordinator_agent_args);
                println!("  thread_agent_args = {:?}", safety.thread_agent_args);
                println!("  coordinator_agent_args_kind = {:?}", safety.coordinator_agent_args_kind);
                println!("  thread_agent_args_kind = {:?}", safety.thread_agent_args_kind);
                println!("  routine_commands = {}", safety.routine_commands);
                println!();
                println!("To change one, edit {} by hand and add:", ctx.config_dir.join("config.toml").display());
                println!();
                println!("[safety.{:?}]", project.canonical_dir().to_string_lossy());
                Ok(())
            }
        },
        Command::Skill => {
            print!("{}", include_str!("../skill/COORDINATOR.md"));
            Ok(())
        }
        Command::Doctor { session } => {
            if !doctor::run(&ctx, &session.into())? {
                bail!("some checks failed");
            }
            Ok(())
        }
        Command::Ticker { command } => match command {
            TickerCommand::Start => ticker::start(&ctx),
            TickerCommand::Run => ticker::run(&ctx),
            TickerCommand::Stop => ticker::stop(&ctx.root),
            TickerCommand::Status => ticker::status(&ctx.root),
        },
    }
}

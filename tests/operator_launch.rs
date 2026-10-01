#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! The operator path for canonical workers on a real project, through the
//! compiled CLI only: `profile verify-interaction --retain` produces the
//! launchable evidence (in a directory the agent trusts, with the owner's login
//! shared into the isolated home and the profile's model and effort pinned in
//! the agent's own configuration), then `launch PROJECT run` takes a planning
//! task from nothing to a reserved attempt, for a Codex and a Claude Code
//! profile, and is safe to repeat.
//!
//! Herdr is a Python stand-in that serves the native session API for real
//! (`server` binds a Unix socket); the agents are tiny binaries that refuse to
//! start unless their login is shared and their directory trusted, as the real
//! ones do. Tests marked "socket" bind Unix sockets and cannot run in a
//! sandbox that forbids it.
use herdr_projects::{domain::*, worker_supervision::{ProcessIncarnation, SupervisorIdentity}};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Output},
};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-projects");

/// The stand-in Herdr: `--version`, probes and the API bridge as in the other
/// suites, plus `server`, which serves one native workspace backed by a real
/// process (stdin is a FIFO the gate reads its release token from).
const HERDR: &str = r#"#!/usr/bin/python3
import json,os,socket,subprocess,sys
args=sys.argv[1:]
if args==['--version']:print('herdr 0.9.1');sys.exit(0)
def serve(path):
 root=os.path.dirname(path);s={}
 server=socket.socket(socket.AF_UNIX);server.bind(path);server.listen()
 while True:
  c,_=server.accept();f=c.makefile('rw');line=f.readline()
  if not line:c.close();continue
  r=json.loads(line);m=r['method'];p=r.get('params') or {}
  live='pid' in s
  kind=s.get('kind','claude')
  screen={'codex':'model: gpt-6.1-sol low   /model to change','claude':'Sonnet 5.5 · Claude Max'}[kind]
  pane={'pane_id':'w1:p1','workspace_id':'w1','tab_id':'w1:t1','terminal_id':'term1','cwd':s.get('cwd')}
  agent=dict(pane,agent=kind,interactive_ready=True,agent_status='idle',**({'name':s['name']} if 'name' in s else {}))
  res=None
  if m=='ping':res={'type':'pong','version':'0.9.1','capabilities':{'workspace_create_command':True}}
  elif m=='workspace.create_command' and not live:
   fifo=os.path.join(root,'input');os.mkfifo(fifo);fd=os.open(fifo,os.O_RDWR)
   child=subprocess.Popen(p['command'],cwd=p['cwd'],stdin=fd,stdout=subprocess.DEVNULL,stderr=open(os.path.join(root,'agent.log'),'w'),start_new_session=True,env={'PATH':'/usr/bin:/bin'})
   s.update(pid=child.pid,argv=p['command'],cwd=p['cwd'],label=p['label'],fifo=fifo,kind='codex' if any(a.endswith('/codex') for a in p['command']) else 'claude')
   res={'type':'workspace_created','workspace':{'workspace_id':'w1','pane_count':1},'root_pane':{'pane_id':'w1:p1'}}
  elif m=='workspace.list':res={'type':'workspace_list','workspaces':[{'workspace_id':'w1','label':s['label'],'pane_count':1,'tab_count':1}] if live else []}
  elif m=='pane.list':res={'panes':[pane] if live else []}
  elif m=='pane.get':res={'pane':dict(pane,agent=kind)}
  elif m=='pane.read':res={'type':'pane_read','text':screen}
  elif m=='pane.process_info':res={'process_info':{'pane_id':'w1:p1','foreground_processes':[{'pid':s['pid'],'argv':s['argv']}] if live else []}}
  elif m=='pane.send_input':
   fd=os.open(s['fifo'],os.O_WRONLY);os.write(fd,p['text'].encode());os.close(fd);s['released']=True;res={'type':'ok'}
  elif m=='agent.list':res={'type':'agent_list','agents':[agent] if live and s.get('released') else []}
  elif m=='agent.explain':res={'type':'agent_explain','explain':{'agent':kind,'state':'idle','manifest_source':'bundled','manifest_version':'2026.09.14.1',
   'matched_rule':{'id':'prompt','state':'idle'},'visible_idle':True,'visible_blocker':False,'visible_working':False,'screen_detection_skipped':False,
   'skip_state_update':False,'local_override_shadowing_remote':False,'fallback_reason':None,'warning':None}}
  elif m=='agent.prompt':res={'type':'agent_prompted','agent':agent}
  if res is not None:f.write(json.dumps({'id':r['id'],'result':res})+'\n');f.flush()
  c.close()
if args==['server']:serve(os.environ['HERDR_SOCKET_PATH']);sys.exit(0)
probe={('pane','list'):'pane.list',('agent','list'):'agent.list'}.get(tuple(args))
assert probe or args==['remote-api-bridge']
line=json.dumps({'id':'probe','method':probe}).encode()+b'\n' if probe else sys.stdin.buffer.readline()
c=socket.socket(socket.AF_UNIX);c.connect(os.environ['HERDR_SOCKET_PATH']);c.sendall(line)
reply=c.makefile('rb').readline()
if not reply:sys.exit(1)
sys.stdout.buffer.write(json.dumps({'result':json.loads(reply)['result']}).encode() if probe else reply)
"#;

/// A Herdr that serves no session: it answers its version and the empty
/// pane/agent probes, for runs against an operator-managed server socket.
const STATIC_HERDR: &str = r#"#!/bin/sh
[ "$1" = --version ] && echo 'herdr 0.9.1' && exit 0
[ "$1 $2" = 'pane list' ] && echo '{"result":{"panes":[]}}' && exit 0
[ "$1 $2" = 'agent list' ] && echo '{"result":{"type":"agent_list","agents":[]}}' && exit 0
exit 3
"#;

/// A stand-in agent: answers `--version`; otherwise it must find the shared
/// login in its home and its working directory trusted in its own configuration.
fn agent_source(version: &str, login: &str, trust: &str) -> String {
    format!(
        r#"fn main(){{
 if std::env::args().nth(1).as_deref()==Some("--version"){{println!({version:?});return}}
 let home=std::env::var("HOME").unwrap_or_default();
 if std::fs::read_to_string(format!("{{home}}/{login}")).unwrap_or_default().trim()!="shared-login"{{eprintln!("not logged in");std::process::exit(7)}}
 let cwd=std::env::current_dir().unwrap();
 if !std::fs::read_to_string(format!("{{home}}/{trust}")).unwrap_or_default().contains(cwd.to_str().unwrap()){{eprintln!("untrusted directory");std::process::exit(8)}}
 loop{{std::thread::park()}}
}}"#
    )
}

struct Lab {
    _top: tempfile::TempDir,
    home: PathBuf,
    root: PathBuf,
    project: PathBuf,
    repo: PathBuf,
    key: PathBuf,
}

impl Lab {
    /// A migrated, paused project `demo`, two worker profiles pinning model and
    /// effort (Codex `gpt-6.1-sol`/low, Claude `claude-sonnet-5-5`/low), a Git
    /// repository with an `integration` branch, and the owner's logins.
    fn new() -> Self { Self::with_herdr(HERDR) }
    /// As `new`, with `herdr` as the Herdr executable's source.
    fn with_herdr(herdr: &str) -> Self {
        // Not under /tmp: the worker sandbox keeps /tmp private, and the owner's
        // home (with its logins) is where the shared login files live.
        let base = Path::new(env!("CARGO_TARGET_TMPDIR"));
        fs::create_dir_all(base).unwrap();
        let top = tempfile::tempdir_in(base).unwrap();
        let home = top.path().canonicalize().unwrap().join("home");
        for dir in ["", "bin", "repo", "agent-home-codex", "agent-home-claude", ".codex", ".claude"] {
            fs::create_dir_all(home.join(dir)).unwrap();
        }
        for login in [".codex/auth.json", ".claude/.credentials.json"] {
            fs::write(home.join(login), "shared-login").unwrap();
            fs::set_permissions(home.join(login), fs::Permissions::from_mode(0o600)).unwrap();
        }
        let key = home.join("owner");
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&key).output().unwrap().status.success());
        let public = fs::read_to_string(key.with_extension("pub")).unwrap().split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        let config = home.join(".config/herdr-projects/config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        let budget = "max_wall_seconds=600\nunknown_usage='allow_with_warning'\n";
        fs::write(&config, format!(
            "[authority]\nversion=1\nrevision=1\napproval_public_key={public:?}\n\
[profiles.codex-sol]\nkind='codex'\npermission_policy='interactive'\nmodel='gpt-6.1-sol'\nreasoning_effort='low'\n[profiles.codex-sol.budget]\n{budget}\
[profiles.claude-sonnet]\nkind='claude'\npermission_policy='interactive'\nmodel='claude-sonnet-5-5'\nreasoning_effort='low'\n[profiles.claude-sonnet.budget]\n{budget}")).unwrap();
        let lab = Lab { root: home.join("root"), project: home.join("root/demo"), repo: home.join("repo"), home, key, _top: top };
        for command in ["new", "pause"] {
            lab.ok(&[command, "demo"]);
        }
        herdr_projects::migration::apply(&lab.project, &herdr_projects::migration::inspect_with_config(&lab.project, &config).unwrap(), true).unwrap();
        fs::write(lab.home.join("bin/herdr"), herdr).unwrap();
        for (name, version, login, trust) in [("codex", "codex-cli 0.154.0", ".codex/auth.json", ".codex/config.toml"), ("claude", "2.1.0 (Claude Code)", ".claude/.credentials.json", ".claude.json")] {
            let source = lab.home.join(format!("bin/{name}.rs"));
            fs::write(&source, agent_source(version, login, trust)).unwrap();
            let built = Command::new("rustc").args(["--edition", "2021", "-o"]).arg(lab.home.join("bin").join(name)).arg(&source).output().unwrap();
            assert!(built.status.success(), "{}", String::from_utf8_lossy(&built.stderr));
        }
        for name in ["herdr", "codex", "claude"] {
            fs::set_permissions(lab.home.join("bin").join(name), fs::Permissions::from_mode(0o700)).unwrap();
        }
        lab.git(&["init", "-q", "-b", "master"]);
        for (key, value) in [("gc.auto", "0"), ("gc.autoDetach", "false"), ("maintenance.auto", "false")] {
            lab.git(&["config", "--local", key, value]);
        }
        lab.git(&["commit", "-q", "--allow-empty", "-m", "start"]);
        lab.git(&["branch", "integration"]);
        fs::write(lab.project.join("PROJECT.md"), "Shadow trial project. Follow the task.\n").unwrap();
        lab
    }
    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("/usr/bin/git").env_clear().env("PATH", "/usr/bin:/bin").env("HOME", &self.home)
            .env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "fixture").env("GIT_AUTHOR_EMAIL", "fixture@example.com")
            .env("GIT_COMMITTER_NAME", "fixture").env("GIT_COMMITTER_EMAIL", "fixture@example.com")
            .current_dir(&self.repo).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }
    fn cli(&self, args: &[&str]) -> Output {
        // Verification's disposable server socket lives under the temporary directory.
        Command::new(BIN).env_clear().env("HOME", &self.home).env("PATH", "/usr/bin:/bin").env("HERDR_BIN_PATH", self.home.join("bin/herdr"))
            .env("TMPDIR", std::env::var_os("TMPDIR").unwrap_or("/tmp".into()))
            .args(["--root", self.root.to_str().unwrap()]).args(args).output().unwrap()
    }
    fn ok(&self, args: &[&str]) -> Value {
        let out = self.cli(args);
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
    }
    fn fail(&self, args: &[&str]) -> String {
        let out = self.cli(args);
        assert!(!out.status.success(), "{args:?} succeeded: {}", String::from_utf8_lossy(&out.stdout));
        String::from_utf8_lossy(&out.stderr).into_owned()
    }
    /// `profile verify-interaction --retain` for `profile`, entirely through the CLI.
    fn verify(&self, profile: &str, kind: &str) -> Value {
        let (herdr, agent, home) = (self.home.join("bin/herdr"), self.home.join("bin").join(kind), self.home.join(format!("agent-home-{kind}")));
        self.ok(&["profile", "verify-interaction", "demo", profile, "--herdr-executable", herdr.to_str().unwrap(),
            "--agent-executable", agent.to_str().unwrap(), "--execution-home", home.to_str().unwrap(), "--retain"])
    }
    /// Launchable evidence for `profile` as the existing suites plant it, for
    /// runs that must not need a live agent session (the evidence a real
    /// `verify-interaction --retain` produces is covered by the socket test).
    fn plant_launchable(&self, profile: &str, kind: &str, model: &str) {
        #[derive(serde::Serialize)] struct Pinned { model: String, reasoning_effort: String, model_on_screen: bool }
        #[derive(serde::Serialize)] struct Interaction { session: ResourceIdentity, terminal: &'static str, readiness_manifest: &'static str, prompt_digest: String, acknowledged_unix_ms: i64, pinned: Pinned }
        #[derive(serde::Serialize)] struct Evidence { version: u32, prepared_profile: VersionedReference, supervisor: SupervisorIdentity, native_kind: String, observed_unix_ms: i64, stopped_unix_ms: i64, interaction: Interaction }
        let (herdr, agent, home) = (self.home.join("bin/herdr"), self.home.join("bin").join(kind), self.home.join(format!("agent-home-{kind}")));
        let prepared = self.ok(&["profile", "prepare", "demo", profile, "--herdr-executable", herdr.to_str().unwrap(),
            "--agent-executable", agent.to_str().unwrap(), "--execution-home", home.to_str().unwrap()]);
        let mut frozen: FrozenProfile = serde_json::from_value(prepared["profile"].clone()).unwrap();
        let evidence = Evidence { version: 2, prepared_profile: frozen.reference().unwrap(), native_kind: kind.into(), observed_unix_ms: 1000, stopped_unix_ms: 1001,
            supervisor: SupervisorIdentity { version: 1, boot_id: "00000000-0000-0000-0000-000000000001".into(), host_id: None, observer_namespace: (1, 2), worker_namespace: (1, 3),
                outer: ProcessIncarnation { pid: 20, device: 1, inode: 4 }, init: ProcessIncarnation { pid: 21, device: 1, inode: 5 } },
            interaction: Interaction { session: ResourceIdentity { device: 1, inode: 2, born_secs: 1, born_nanos: 0 }, terminal: "fixture-terminal", readiness_manifest: "fixture-manifest",
                prompt_digest: "a".repeat(64), acknowledged_unix_ms: 999, pinned: Pinned { model: model.into(), reasoning_effort: "low".into(), model_on_screen: true } } };
        let hash = format!("{:x}", Sha256::digest(serde_json::to_vec(&evidence).unwrap()));
        let supported = CapabilityEvidence::Supported { evidence: VersionedReference { id: format!("native-transport-{hash}"), revision: 1, digest: hash } };
        let c = &mut frozen.capabilities;
        (c.launch, c.stop, c.readiness_observation, c.prompt_submission) = (supported.clone(), supported.clone(), supported.clone(), supported);
        let reference = frozen.reference().unwrap();
        let store = self.project.join(".state/state.db").canonicalize().unwrap();
        let metadata = fs::metadata(&store).unwrap();
        let report = serde_json::json!({"preparation":{"profile":frozen,"reference":reference,"launchable":true,"protocol_capable":false,"certified":false},
            "evidence":evidence,"source_store":[store,metadata.dev(),metadata.ino()]}).to_string();
        rusqlite::Connection::open(&store).unwrap().execute("INSERT INTO native_profiles(profile_digest,report,report_digest,sequence) VALUES(?1,?2,?3,(SELECT max(sequence) FROM events))",
            rusqlite::params![reference.digest, report, format!("{:x}", Sha256::digest(report.as_bytes()))]).unwrap();
    }
    /// A control-socket inode for an operator-managed server (nothing listens on it).
    fn socket_inode_once(&self, name: &str) -> PathBuf {
        let path = self.home.join(name);
        if path.exists() {
            return path;
        }
        let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mknod(c.as_ptr(), libc::S_IFSOCK | 0o600, 0) }, 0);
        path
    }
    fn run_args<'a>(&'a self, task: &'a str, profile: &'a str, output: &'a str, prompt: &'a str) -> Vec<&'a str> {
        vec!["launch", "demo", "run", "--task", task, "--profile", profile, "--repository", self.repo.to_str().unwrap(), "--plan-output", output,
            "--prompt-file", prompt, "--integration-ref", "refs/heads/integration", "--sign-with", self.key.to_str().unwrap(),
            "--validity-seconds", "600", "--max-active-workers", "2"]
    }
}

/// Without retained launchable evidence the run stops at its first step and
/// leaves the project exactly as it was: no task, contract, server or binding.
#[test]
fn launch_run_refuses_loudly_at_the_first_failing_step_and_changes_nothing() {
    let lab = Lab::new();
    let before = herdr_projects::runtime::snapshot(&lab.project).unwrap();
    let prompt = lab.home.join("prompt.txt");
    fs::write(&prompt, "Plan the next milestone.").unwrap();
    let error = lab.fail(&lab.run_args("plan-codex", "codex-sol", "docs/plan-codex.md", prompt.to_str().unwrap()));
    assert!(error.contains("stopped at step 1") && error.contains("no launchable evidence retained for profile codex-sol") && error.contains("verify-interaction"), "{error}");
    assert_eq!(herdr_projects::runtime::snapshot(&lab.project).unwrap(), before, "a refused run must not write");
    assert!(!lab.root.join(".herdr-run").exists(), "no Herdr server directory before the first step passes");
    // HERDR_BIN_PATH must name the verified Herdr by absolute path.
    let out = Command::new(BIN).env_clear().env("HOME", &lab.home).env("PATH", "/usr/bin:/bin").env("HERDR_BIN_PATH", "herdr")
        .args(["--root", lab.root.to_str().unwrap()]).args(lab.run_args("plan-codex", "codex-sol", "docs/plan-codex.md", prompt.to_str().unwrap())).output().unwrap();
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("HERDR_BIN_PATH"), "{}", String::from_utf8_lossy(&out.stderr));
}

/// socket: verification trusts its own directory under the execution home,
/// pins the model and effort in the agent's configuration and checks them
/// after readiness, runs the agent as the owner's logged-in CLI, and retains
/// launchable evidence, with no SQL and no hand-edited trust.
#[test]
fn verify_interaction_produces_launchable_evidence_for_codex_and_claude_from_the_cli() {
    let lab = Lab::new();
    for (profile, kind, model) in [("codex-sol", "codex", "gpt-6.1-sol"), ("claude-sonnet", "claude", "claude-sonnet-5-5")] {
        let report = lab.verify(profile, kind);
        assert_eq!(report["preparation"]["launchable"], true, "{kind}: {report}");
        assert_eq!(report["preparation"]["profile"]["kind"], kind);
        let pinned = &report["evidence"]["interaction"]["pinned"];
        assert_eq!((pinned["model"].as_str(), pinned["reasoning_effort"].as_str(), pinned["model_on_screen"].as_bool()), (Some(model), Some("low"), Some(true)), "{report}");
        let digest = report["preparation"]["reference"]["digest"].as_str().unwrap();
        let retained = lab.ok(&["profile", "retained", "demo", digest]);
        assert_eq!(retained["preparation"]["launchable"], true);
        // The agent's own configuration in the isolated home carries the pins and the trust.
        let home = lab.home.join(format!("agent-home-{kind}"));
        let work = home.join(".hp-verify-work");
        let config = fs::read_to_string(if kind == "codex" { home.join(".codex/config.toml") } else { home.join(".claude/settings.json") }).unwrap();
        assert!(config.contains(model) && config.contains("low"), "{config}");
        let trust = fs::read_to_string(if kind == "codex" { home.join(".codex/config.toml") } else { home.join(".claude.json") }).unwrap();
        assert!(trust.contains(work.to_str().unwrap()), "{trust}");
        // The login stays the owner's single file: the home only has an empty mount point.
        let login = home.join(if kind == "codex" { ".codex/auth.json" } else { ".claude/.credentials.json" });
        assert_eq!(fs::read_to_string(login).unwrap(), "", "no copy of the login in the execution home");
    }
}

/// The one operator command takes a planning task from nothing to a reserved
/// attempt for each kind (an operator-managed server socket, launchable
/// evidence planted), signs through ssh-keygen with the owner's key, carries
/// the task text and deliverable into the retained brief, and a rerun skips every
/// finished step without reserving again.
#[test]
fn launch_run_reserves_a_planning_task_for_each_kind_and_reruns_safely() {
    let lab = Lab::with_herdr(STATIC_HERDR);
    let prompt = lab.home.join("prompt.txt");
    fs::write(&prompt, "Plan the next milestone of the tactics game.").unwrap();
    let jobs = [("plan-codex", "codex-sol", "codex", "gpt-6.1-sol"), ("plan-claude", "claude-sonnet", "claude", "claude-sonnet-5-5")];
    let arguments = |lab: &Lab, (task, profile, ..): (&str, &str, &str, &str), prompt: &str| {
        let socket = lab.socket_inode_once(&format!("{task}.sock"));
        let mut args: Vec<String> = lab.run_args(task, profile, &format!("docs/{task}.md"), prompt).into_iter().map(str::to_owned).collect();
        args.extend(["--herdr-socket".into(), socket.display().to_string()]);
        args
    };
    // A new binding pauses the project until no attempt is unfinished, so both
    // tasks are prepared (bound, reconciled, active) before either is reserved.
    for job in jobs {
        lab.plant_launchable(job.1, job.2, job.3);
        let mut args = arguments(&lab, job, prompt.to_str().unwrap());
        args.push("--prepare-only".into());
        let report = lab.ok(&args.iter().map(String::as_str).collect::<Vec<_>>());
        assert!(report["attempt"].is_null(), "{report}");
    }
    let mut attempts = Vec::new();
    for job @ (task, _, kind, _) in jobs {
        let args = arguments(&lab, job, prompt.to_str().unwrap());
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = format!("docs/{task}.md");
        let report = lab.ok(&args);
        assert_eq!(report["kind"], kind, "{report}");
        let attempt = report["attempt"].as_str().unwrap().to_owned();
        assert!(report["worktree"].as_str().unwrap().contains(".state/worktrees"), "{report}");
        let names: Vec<_> = report["steps"].as_array().unwrap().iter().map(|s| s["step"].as_str().unwrap()).collect();
        for step in ["profile_evidence", "project_control", "task", "contract", "queue", "scheduler_capacity", "integration_target", "herdr_server", "binding", "reconcile_and_activate", "knowledge_snapshot", "draft", "approval_import", "reserve"] {
            assert!(names.contains(&step), "{step} missing from {names:?}");
        }
        let state = herdr_projects::runtime::snapshot(&lab.project).unwrap();
        let record = state.tasks.iter().find(|t| t.id.as_str() == task).unwrap();
        assert_eq!(record.active_attempt.as_ref().map(|a| a.as_str()), Some(attempt.as_str()));
        // The retained brief carries the project instructions, the task text and the deliverable.
        let brief = lab.ok(&["memory", "demo", "attempt-brief", "--attempt", &attempt]).to_string();
        assert!(brief.contains("Shadow trial project") && brief.contains("Plan the next milestone") && brief.contains(&output), "{brief}");
        // Safe to repeat: finished steps are skipped and no second attempt appears.
        let again = lab.ok(&args);
        assert_eq!(again["attempt"], report["attempt"], "{again}");
        for step in again["steps"].as_array().unwrap() {
            if ["project_control", "task", "contract", "queue", "scheduler_capacity", "binding", "reserve"].contains(&step["step"].as_str().unwrap()) {
                assert_eq!(step["outcome"], "already_done", "{again}");
            }
        }
        assert_eq!(herdr_projects::runtime::snapshot(&lab.project).unwrap().attempts.len(), attempts.len() + 1);
        attempts.push(attempt);
    }
    assert_ne!(attempts[0], attempts[1]);
    let queue = lab.ok(&["scheduler", "demo", "inspect"]);
    assert_eq!(queue["policy"]["max_active_workers"], 2, "{queue}");
    // A task needing a new binding is refused before anything changes while
    // attempts are unfinished (the binding would pause the project).
    let before = herdr_projects::runtime::snapshot(&lab.project).unwrap();
    let mut third = lab.run_args("plan-third", "codex-sol", "docs/plan-third.md", prompt.to_str().unwrap()).into_iter().map(str::to_owned).collect::<Vec<_>>();
    let socket = lab.socket_inode_once("plan-third.sock");
    third.extend(["--herdr-socket".into(), socket.display().to_string()]);
    let error = lab.fail(&third.iter().map(String::as_str).collect::<Vec<_>>());
    assert!(error.contains("--prepare-only") && error.contains("pauses the project"), "{error}");
    let after = herdr_projects::runtime::snapshot(&lab.project).unwrap();
    assert_eq!(after.control.as_ref().unwrap().state, ProjectState::Active, "the project must stay active");
    assert_eq!(after.runtime_bindings.len(), before.runtime_bindings.len(), "no binding was created");
}

/// socket: the same operator sequence after real `verify-interaction`, with a
/// dedicated Herdr server started per task (the production shape).
#[test]
fn launch_run_with_a_dedicated_server_after_verify_interaction_reserves_both_kinds() {
    let lab = Lab::new();
    lab.verify("codex-sol", "codex");
    lab.verify("claude-sonnet", "claude");
    let prompt = lab.home.join("prompt.txt");
    fs::write(&prompt, "Plan the next milestone of the tactics game.").unwrap();
    let jobs = [("plan-codex", "codex-sol"), ("plan-claude", "claude-sonnet")];
    // Prepare both (each gets its own started Herdr server), then reserve both.
    for (task, profile) in jobs {
        let output = format!("docs/{task}.md");
        let mut args = lab.run_args(task, profile, &output, prompt.to_str().unwrap());
        args.push("--prepare-only");
        let report = lab.ok(&args);
        assert!(report["attempt"].is_null(), "{report}");
        assert!(Path::new(report["herdr_socket"].as_str().unwrap()).exists(), "{report}");
    }
    let mut attempts = Vec::new();
    for (task, profile) in jobs {
        let output = format!("docs/{task}.md");
        let report = lab.ok(&lab.run_args(task, profile, &output, prompt.to_str().unwrap()));
        let attempt = report["attempt"].as_str().unwrap().to_owned();
        assert!(report["worktree"].as_str().unwrap().contains(".state/worktrees"), "{report}");
        let again = lab.ok(&lab.run_args(task, profile, &output, prompt.to_str().unwrap()));
        assert_eq!(again["attempt"], report["attempt"], "{again}");
        attempts.push(attempt);
    }
    assert_ne!(attempts[0], attempts[1]);
    let state = herdr_projects::runtime::snapshot(&lab.project).unwrap();
    assert_eq!(state.attempts.len(), 2);
    assert_eq!(state.control.unwrap().state, ProjectState::Active);
}

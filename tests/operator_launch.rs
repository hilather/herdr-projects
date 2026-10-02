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
use herdr_farm::{domain::*, worker_supervision::{ProcessIncarnation, SupervisorIdentity}};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Output},
};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-farm");

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
  agent=dict(pane,agent=kind,interactive_ready=True,agent_status='working' if s.get('accepted') else 'idle',**({'name':s['name']} if 'name' in s else {}))
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
  elif m=='agent.prompt':s['accepted']=True;res={'type':'agent_prompted','agent':agent}
  elif m=='agent.rename':s['name']=agent['name']=p['name'];res={'type':'agent_info','agent':agent}
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

/// The stand-in agent's login check: a Codex agent must find the shared login
/// file (`login`) in its home; a Claude agent (`login` empty) must find the
/// setup token in its environment and no credentials file.
fn login_check(login: &str) -> String {
    if login.is_empty() {
        r#"if std::env::var("CLAUDE_CODE_OAUTH_TOKEN").as_deref()!=Ok("fixture-setup-token")||std::path::Path::new(&format!("{home}/.claude/.credentials.json")).exists(){eprintln!("not logged in");std::process::exit(7)}"#.to_owned()
    } else {
        format!(r#"if std::fs::read_to_string(format!("{{home}}/{login}")).unwrap_or_default().trim()!="shared-login"{{eprintln!("not logged in");std::process::exit(7)}}"#)
    }
}

/// A stand-in agent: answers `--version`; otherwise it must be logged in (see
/// `login_check`) and its working directory trusted in its own configuration.
fn agent_source(version: &str, login: &str, trust: &str) -> String {
    let check = login_check(login);
    format!(
        r#"fn main(){{
 if std::env::args().nth(1).as_deref()==Some("--version"){{println!({version:?});return}}
 let home=std::env::var("HOME").unwrap_or_default();
 {check}
 let cwd=std::env::current_dir().unwrap();
 if !std::fs::read_to_string(format!("{{home}}/{trust}")).unwrap_or_default().contains(cwd.to_str().unwrap()){{eprintln!("untrusted directory");std::process::exit(8)}}
 loop{{std::thread::park()}}
}}"#
    )
}

/// A stand-in agent that, like the real ones, writes into its execution home:
/// on `--version` (when it is given a HOME) and during the session. `version_extra`
/// is extra Rust run on `--version` with `home` bound, for hostile variants.
fn writing_agent_source(version: &str, login: &str, trust: &str, version_dir: &str, version_file: &str, session_file: &str, version_extra: &str) -> String {
    let check = login_check(login);
    format!(
        r#"fn main(){{
 let home=std::env::var("HOME").unwrap_or_default();
 if std::env::args().nth(1).as_deref()==Some("--version"){{
  if !home.is_empty(){{std::fs::create_dir_all(format!("{{home}}/{version_dir}")).unwrap();std::fs::write(format!("{{home}}/{version_dir}/{version_file}"),"probe").unwrap();}}
  {version_extra}
  println!({version:?});return
 }}
 {check}
 let cwd=std::env::current_dir().unwrap();
 if !std::fs::read_to_string(format!("{{home}}/{trust}")).unwrap_or_default().contains(cwd.to_str().unwrap()){{eprintln!("untrusted directory");std::process::exit(8)}}
 let session=format!("{{home}}/{session_file}");
 std::fs::create_dir_all(std::path::Path::new(&session).parent().unwrap()).unwrap();
 std::fs::write(session,"{{}}").unwrap();
 loop{{std::thread::park()}}
}}"#
    )
}

struct Lab {
    _top: tempfile::TempDir,
    /// The private runtime directory every CLI run uses for its Herdr sockets,
    /// so a test sees (and never shares) the socket directories a run leaves.
    runtime: tempfile::TempDir,
    home: PathBuf,
    root: PathBuf,
    project: PathBuf,
    repo: PathBuf,
    key: PathBuf,
    extra_env: Vec<(String, PathBuf)>,
}

impl Drop for Lab {
    /// Safety net: a failed test must not leave a stand-in Herdr server behind.
    fn drop(&mut self) {
        let _ = Command::new("/usr/bin/pkill").args(["-KILL", "-f"]).arg(format!("{} server", self.home.join("bin/herdr").display())).status();
    }
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
        // The Claude worker's login: a long-lived setup-token file, outside the
        // project and the agent directories (the owner's credentials file is
        // never shared with a worker).
        let token = home.join("claude-setup-token");
        fs::write(&token, "fixture-setup-token\n").unwrap();
        fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
        for login in [".codex/auth.json", ".claude/.credentials.json"] {
            fs::write(home.join(login), "shared-login").unwrap();
            fs::set_permissions(home.join(login), fs::Permissions::from_mode(0o600)).unwrap();
        }
        let key = home.join("owner");
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&key).output().unwrap().status.success());
        let public = fs::read_to_string(key.with_extension("pub")).unwrap().split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        let config = home.join(".config/herdr-farm/config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        let budget = "max_wall_seconds=600\nunknown_usage='allow_with_warning'\n";
        fs::write(&config, format!(
            "[authority]\nversion=1\nrevision=1\napproval_public_key={public:?}\n\
[worker_isolation.login]\nclaude_token_file={token:?}\n\
[profiles.codex-sol]\nkind='codex'\npermission_policy='interactive'\nmodel='gpt-6.1-sol'\nreasoning_effort='low'\n[profiles.codex-sol.budget]\n{budget}\
[profiles.claude-sonnet]\nkind='claude'\npermission_policy='interactive'\nmodel='claude-sonnet-5-5'\nreasoning_effort='low'\n[profiles.claude-sonnet.budget]\n{budget}")).unwrap();
        let runtime = tempfile::Builder::new().prefix("hp").tempdir_in(std::env::temp_dir()).unwrap();
        fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let lab = Lab { runtime, root: home.join("root"), project: home.join("root/demo"), repo: home.join("repo"), home, key, _top: top, extra_env: Vec::new() };
        for command in ["new", "pause"] {
            lab.ok(&[command, "demo"]);
        }
        herdr_farm::migration::apply(&lab.project, &herdr_farm::migration::inspect_with_config(&lab.project, &config).unwrap(), true).unwrap();
        fs::write(lab.home.join("bin/herdr"), herdr).unwrap();
        for (name, version, login, trust) in [("codex", "codex-cli 0.154.0", ".codex/auth.json", ".codex/config.toml"), ("claude", "2.1.0 (Claude Code)", "", ".claude.json")] {
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
    /// Replace the stand-in agent `name` with one built from `source`.
    fn build_agent(&self, name: &str, source: &str) {
        let file = self.home.join(format!("bin/{name}.rs"));
        fs::write(&file, source).unwrap();
        let built = Command::new("rustc").args(["--edition", "2021", "-o"]).arg(self.home.join("bin").join(name)).arg(&file).output().unwrap();
        assert!(built.status.success(), "{}", String::from_utf8_lossy(&built.stderr));
        fs::set_permissions(self.home.join("bin").join(name), fs::Permissions::from_mode(0o700)).unwrap();
    }
    fn prepare(&self, profile: &str, kind: &str) -> Output {
        let (herdr, agent, home) = (self.home.join("bin/herdr"), self.home.join("bin").join(kind), self.home.join(format!("agent-home-{kind}")));
        self.cli(&["profile", "prepare", "demo", profile, "--herdr-executable", herdr.to_str().unwrap(),
            "--agent-executable", agent.to_str().unwrap(), "--execution-home", home.to_str().unwrap()])
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
        Command::new(BIN).env_clear().env("HOME", &self.home).env("HERDR_PROJECTS_OWNER_HOME", &self.home).env("PATH", "/usr/bin:/bin").env("HERDR_BIN_PATH", self.home.join("bin/herdr"))
            .env("TMPDIR", std::env::var_os("TMPDIR").unwrap_or("/tmp".into())).env("XDG_RUNTIME_DIR", self.runtime.path())
            .envs(self.extra_env.iter().map(|(k, v)| (k.as_str(), v.as_path())))
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
    /// Follow the closing script of the attempt's retained brief as the worker
    /// would: edit the deliverable in a worktree of the repository and run the
    /// script. `herdr-farm` resolves to the product CLI with the spool
    /// variable removed, so outside any sandbox it records the submission
    /// directly (inside the sandbox the same command goes through the spool).
    fn follow_brief(&self, attempt: &str, output: &str, branch: &str) -> Output {
        let brief = self.ok(&["memory", "demo", "attempt-brief", "--attempt", attempt]);
        let text = brief["text"].as_str().unwrap();
        let script = text.split("```sh\n").nth(1).and_then(|rest| rest.split("```").next()).unwrap_or_else(|| panic!("the brief has no submission script:\n{text}"));
        let worktree = self.home.join(format!("worker-{branch}"));
        self.git(&["worktree", "add", "-q", "-b", branch, worktree.to_str().unwrap()]);
        fs::create_dir_all(worktree.join(output).parent().unwrap()).unwrap();
        fs::write(worktree.join(output), "The plan.\n").unwrap();
        let shim = self.home.join("shim");
        fs::create_dir_all(&shim).unwrap();
        fs::write(shim.join("herdr-farm"), format!("#!/bin/sh\nunset HERDR_FARM_SUBMISSION_SPOOL\nexec {BIN} \"$@\"\n")).unwrap();
        fs::set_permissions(shim.join("herdr-farm"), fs::Permissions::from_mode(0o700)).unwrap();
        Command::new("/bin/sh").arg("-c").arg(script).current_dir(&worktree).env_clear()
            .env("HOME", &self.home).env("HERDR_PROJECTS_OWNER_HOME", &self.home).env("PATH", format!("{}:/usr/bin:/bin", shim.display()))
            .env("TMPDIR", std::env::var_os("TMPDIR").unwrap_or("/tmp".into())).env("XDG_RUNTIME_DIR", self.runtime.path())
            .env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("HERDR_FARM_SUBMISSION_SPOOL", self.project.join(".state/spool").join(attempt)).output().unwrap()
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
    let before = herdr_farm::runtime::snapshot(&lab.project).unwrap();
    let prompt = lab.home.join("prompt.txt");
    fs::write(&prompt, "Plan the next milestone.").unwrap();
    let error = lab.fail(&lab.run_args("plan-codex", "codex-sol", "docs/plan-codex.md", prompt.to_str().unwrap()));
    assert!(error.contains("stopped at step 1") && error.contains("no launchable evidence retained for profile codex-sol") && error.contains("verify-interaction"), "{error}");
    assert_eq!(herdr_farm::runtime::snapshot(&lab.project).unwrap(), before, "a refused run must not write");
    assert!(!lab.root.join(".herdr-run").exists(), "no Herdr server directory before the first step passes");
    // HERDR_BIN_PATH must name the verified Herdr by absolute path.
    let out = Command::new(BIN).env_clear().env("HOME", &lab.home).env("HERDR_PROJECTS_OWNER_HOME", &lab.home).env("PATH", "/usr/bin:/bin").env("HERDR_BIN_PATH", "herdr")
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
        // Codex's login stays the owner's single file: the home only has an empty
        // mount point. Claude's is the setup token in the agent's environment:
        // no credentials file and no copy of the token anywhere in the home.
        if kind == "codex" {
            assert_eq!(fs::read_to_string(home.join(".codex/auth.json")).unwrap(), "", "no copy of the login in the execution home");
        } else {
            assert!(!home.join(".claude/.credentials.json").exists(), "no Claude credentials file in the execution home");
            let leaked = Command::new("/usr/bin/grep").args(["-r", "-l", "fixture-setup-token"]).arg(&home).output().unwrap();
            assert!(leaked.stdout.is_empty(), "the token is in no file of the execution home: {}", String::from_utf8_lossy(&leaked.stdout));
        }
        let report_text = report.to_string();
        assert!(!report_text.contains("fixture-setup-token"), "the token never appears in a report");
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
        let state = herdr_farm::runtime::snapshot(&lab.project).unwrap();
        let record = state.tasks.iter().find(|t| t.id.as_str() == task).unwrap();
        assert_eq!(record.active_attempt.as_ref().map(|a| a.as_str()), Some(attempt.as_str()));
        // The retained brief carries the project instructions, the task text and the deliverable.
        let brief = lab.ok(&["memory", "demo", "attempt-brief", "--attempt", &attempt]).to_string();
        assert!(brief.contains("Shadow trial project") && brief.contains("Plan the next milestone") && brief.contains(&output), "{brief}");
        // The brief ends with the exact submission the worker must make; following
        // it records one submission bound to this attempt's contract.
        let brief_text = lab.ok(&["memory", "demo", "attempt-brief", "--attempt", &attempt])["text"].as_str().unwrap().to_owned();
        assert!(brief_text.contains("herdr-farm --root") && brief_text.contains("submission_id") && brief_text.contains("result demo submit"), "{brief_text}");
        let branch = format!("worker-{task}");
        let followed = lab.follow_brief(&attempt, &output, &branch);
        assert!(followed.status.success() && String::from_utf8_lossy(&followed.stdout).contains("submission_id"), "{}{}", String::from_utf8_lossy(&followed.stdout), String::from_utf8_lossy(&followed.stderr));
        let shown = lab.ok(&["result", "demo", "show"]);
        let mine: Vec<_> = shown.as_array().unwrap().iter().filter(|r| r["attempt_id"] == attempt.as_str()).collect();
        assert_eq!(mine.len(), 1, "{shown}");
        assert_eq!(mine[0]["task_id"], task);
        assert_eq!(mine[0]["artifact_manifest"][0]["path"], output.as_str(), "{shown}");
        assert_eq!(mine[0]["contract_revision"], 1);
        // Safe to repeat: finished steps are skipped and no second attempt appears.
        let again = lab.ok(&args);
        assert_eq!(again["attempt"], report["attempt"], "{again}");
        for step in again["steps"].as_array().unwrap() {
            if ["project_control", "task", "contract", "queue", "scheduler_capacity", "binding", "reserve"].contains(&step["step"].as_str().unwrap()) {
                assert_eq!(step["outcome"], "already_done", "{again}");
            }
        }
        assert_eq!(herdr_farm::runtime::snapshot(&lab.project).unwrap().attempts.len(), attempts.len() + 1);
        attempts.push(attempt);
    }
    assert_ne!(attempts[0], attempts[1]);
    let queue = lab.ok(&["scheduler", "demo", "inspect"]);
    assert_eq!(queue["policy"]["max_active_workers"], 2, "{queue}");
    // A task needing a new binding is refused before anything changes while
    // attempts are unfinished (the binding would pause the project).
    let before = herdr_farm::runtime::snapshot(&lab.project).unwrap();
    let mut third = lab.run_args("plan-third", "codex-sol", "docs/plan-third.md", prompt.to_str().unwrap()).into_iter().map(str::to_owned).collect::<Vec<_>>();
    let socket = lab.socket_inode_once("plan-third.sock");
    third.extend(["--herdr-socket".into(), socket.display().to_string()]);
    let error = lab.fail(&third.iter().map(String::as_str).collect::<Vec<_>>());
    assert!(error.contains("--prepare-only") && error.contains("pauses the project"), "{error}");
    let after = herdr_farm::runtime::snapshot(&lab.project).unwrap();
    assert_eq!(after.control.as_ref().unwrap().state, ProjectState::Active, "the project must stay active");
    assert_eq!(after.runtime_bindings.len(), before.runtime_bindings.len(), "no binding was created");
}

/// A Claude profile whose pinned owner configuration names no setup-token file
/// is refused before any server starts or anything is
/// reserved, with a message that says what to configure, rather than launching
/// a worker that can only answer "login expired".
#[test]
fn a_claude_profile_without_a_setup_token_file_is_refused_by_verification_and_launch_run() {
    let lab = Lab::with_herdr(STATIC_HERDR);
    let config = lab.home.join(".config/herdr-farm/config.toml");
    let token = lab.home.join("claude-setup-token");
    let with_token = fs::read_to_string(&config).unwrap();
    let without = with_token.replace(&format!("[worker_isolation.login]\nclaude_token_file={token:?}\n"), "");
    assert_ne!(with_token, without);
    fs::write(&config, &without).unwrap();
    lab.plant_launchable("claude-sonnet", "claude", "claude-sonnet-5-5");
    let prompt = lab.home.join("prompt.txt");
    fs::write(&prompt, "Plan the next milestone.").unwrap();
    let socket = lab.socket_inode_once("plan-claude.sock");
    let mut args: Vec<String> = lab.run_args("plan-claude", "claude-sonnet", "docs/plan-claude.md", prompt.to_str().unwrap()).into_iter().map(str::to_owned).collect();
    args.extend(["--herdr-socket".into(), socket.display().to_string()]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let before = herdr_farm::runtime::snapshot(&lab.project).unwrap();
    let error = lab.fail(&args);
    assert!(error.contains("stopped at step 1") && error.contains("claude_token_file") && error.contains("claude setup-token"), "{error}");
    assert_eq!(herdr_farm::runtime::snapshot(&lab.project).unwrap(), before, "a refused run must not write");
    // Verification refuses the same profile before it starts any server.
    let (herdr, agent, home) = (lab.home.join("bin/herdr"), lab.home.join("bin/claude"), lab.home.join("agent-home-claude"));
    let verify = ["profile", "verify-interaction", "demo", "claude-sonnet", "--herdr-executable", herdr.to_str().unwrap(),
        "--agent-executable", agent.to_str().unwrap(), "--execution-home", home.to_str().unwrap(), "--retain"];
    let error = lab.fail(&verify);
    assert!(error.contains("claude_token_file"), "{error}");
}

/// The owner edits the configuration after the project was made active: the next
/// `launch run` must re-acknowledge it (not report `project_control` as already
/// done and then fail at the contract with "owner configuration is not
/// acknowledged by project control").
#[test]
fn launch_run_reacknowledges_an_owner_configuration_edited_since_control_was_activated() {
    let lab = Lab::with_herdr(STATIC_HERDR);
    let prompt = lab.home.join("prompt.txt");
    fs::write(&prompt, "Plan the next milestone of the tactics game.").unwrap();
    let prepare = |lab: &Lab, task: &str| {
        let socket = lab.socket_inode_once(&format!("{task}.sock"));
        let mut args: Vec<String> = lab.run_args(task, "codex-sol", &format!("docs/{task}.md"), prompt.to_str().unwrap()).into_iter().map(str::to_owned).collect();
        args.extend(["--herdr-socket".into(), socket.display().to_string(), "--prepare-only".into()]);
        lab.ok(&args.iter().map(String::as_str).collect::<Vec<_>>())
    };
    let step = |report: &Value, name: &str| report["steps"].as_array().unwrap().iter().find(|s| s["step"] == name).cloned().unwrap();
    let acknowledged = |lab: &Lab| herdr_farm::runtime::snapshot(&lab.project).unwrap().control.unwrap().config_digest;
    lab.plant_launchable("codex-sol", "codex", "gpt-6.1-sol");
    let first = prepare(&lab, "plan-one");
    assert_eq!(step(&first, "project_control")["outcome"], "done", "{first}");
    let before = acknowledged(&lab);
    assert!(before.is_some());

    // The owner edits the configuration; the profile is prepared against the new bytes.
    let config = lab.home.join(".config/herdr-farm/config.toml");
    let mut text = fs::read_to_string(&config).unwrap();
    text.push_str("\n# edited by the owner after the project was activated\n");
    fs::write(&config, text).unwrap();
    lab.plant_launchable("codex-sol", "codex", "gpt-6.1-sol");
    let second = prepare(&lab, "plan-two");
    let control = step(&second, "project_control");
    assert_eq!((control["outcome"].as_str(), control["detail"]["owner_configuration_reacknowledged"].as_bool()), (Some("done"), Some(true)), "{second}");
    assert_eq!(step(&second, "contract")["outcome"], "done", "{second}");
    let after = acknowledged(&lab);
    assert_ne!(after, before, "control acknowledges the edited configuration");
    assert_eq!(after, herdr_farm::migration::config_reference(&config).unwrap().digest);
    // Repeating changes nothing: the configuration is acknowledged now.
    let again = prepare(&lab, "plan-two");
    assert_eq!(step(&again, "project_control")["outcome"], "already_done", "{again}");
}

/// socket: explicit cancellation permits owner re-acknowledgment and a new attempt,
/// while an unobserved live worker still fences step 2.
#[test]
fn launch_run_retries_after_termination_but_refuses_an_unobserved_live_worker() {
    struct FixtureTicker(std::process::Child);
    impl Drop for FixtureTicker {
        fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); }
    }
    let lab = Lab::new();
    lab.verify("codex-sol", "codex");
    let prompt = lab.home.join("prompt.txt");
    fs::write(&prompt, "Plan the next milestone.").unwrap();
    let args = lab.run_args("plan-retry", "codex-sol", "docs/retry.md", prompt.to_str().unwrap());
    let first = lab.ok(&args);
    let attempt = first["attempt"].as_str().unwrap();
    let mut ticker = FixtureTicker(Command::new(BIN).env_clear().env("HOME", &lab.home)
        .env("HERDR_PROJECTS_OWNER_HOME", &lab.home).env("PATH", "/usr/bin:/bin")
        .env("HERDR_BIN_PATH", lab.home.join("bin/herdr"))
        .env("XDG_RUNTIME_DIR", lab.runtime.path())
        .args(["--root", lab.root.to_str().unwrap(), "ticker", "run"])
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap());
    let wait = |stage: &str, done: &dyn Fn() -> bool| {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !done() {
            assert!(std::time::Instant::now() < until,
                "timed out waiting for {stage}\nattempt states: {:?}\nticker log:\n{}",
                herdr_farm::runtime::snapshot(&lab.project).map(|s| s.attempts),
                fs::read_to_string(lab.root.join(".ticker.log")).unwrap_or_default());
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    };
    wait("first attempt Running", &|| herdr_farm::runtime::snapshot(&lab.project).is_ok_and(|s|
        s.attempts.iter().any(|a| a.id.as_str() == attempt && a.state == AttemptState::Running)));
    // Stop only this fixture's ticker: the worker remains alive and its old
    // observation cannot acknowledge edited owner configuration.
    ticker.0.kill().unwrap();ticker.0.wait().unwrap();
    let config = lab.home.join(".config/herdr-farm/config.toml");
    let original = fs::read_to_string(&config).unwrap();
    fs::write(&config, format!("{original}\n# owner edit\n")).unwrap();
    lab.plant_launchable("codex-sol", "codex", "gpt-6.1-sol");
    let error = lab.fail(&args);
    assert!(error.contains("step 2") && error.contains("fresh resource identity evidence required"), "{error}");
    // Return to the launch's authorized config so the controller can stop it.
    fs::write(&config, &original).unwrap();
    let mut ticker = FixtureTicker(Command::new(BIN).env_clear().env("HOME", &lab.home)
        .env("HERDR_PROJECTS_OWNER_HOME", &lab.home).env("PATH", "/usr/bin:/bin")
        .env("HERDR_BIN_PATH", lab.home.join("bin/herdr"))
        .env("XDG_RUNTIME_DIR", lab.runtime.path())
        .args(["--root", lab.root.to_str().unwrap(), "ticker", "run"])
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap());
    // The worker dies on its own, as the live trial's did (its budget ended it):
    // the attempt fails and the task is left blocked, not cancelled.
    let agent = fs::canonicalize(lab.home.join("bin/codex")).unwrap();
    let mut killed = 0;
    for entry in fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else { continue };
        if fs::read_link(entry.path().join("exe")).is_ok_and(|exe| exe == agent) {
            assert!(Command::new("/bin/kill").args(["-KILL", &pid.to_string()]).status().unwrap().success());
            killed += 1;
        }
    }
    assert_eq!(killed, 1, "exactly the fixture's worker agent is killed");
    wait("first attempt termination observed after the worker died", &|| herdr_farm::runtime::snapshot(&lab.project).is_ok_and(|s|
        s.attempts.iter().any(|a| a.id.as_str() == attempt && a.termination_observed)));
    ticker.0.kill().unwrap();ticker.0.wait().unwrap();
    lab.ok(&["launch", "demo", "stop", "--task", "plan-retry"]);
    let old_worktree = PathBuf::from(first["worktree"].as_str().unwrap());
    assert!(old_worktree.is_dir());
    let ended = herdr_farm::runtime::snapshot(&lab.project).unwrap();
    assert_eq!(ended.attempts.iter().find(|a| a.id.as_str() == attempt).unwrap().state, AttemptState::Failed);
    assert_eq!(ended.tasks.iter().find(|t| t.id.as_str() == "plan-retry").unwrap().state, TaskState::Blocked);
    fs::write(&config, format!("{original}\n# owner edit after termination\n")).unwrap();
    lab.plant_launchable("codex-sol", "codex", "gpt-6.1-sol");
    let second = lab.ok(&args);
    assert_ne!(second["attempt"], first["attempt"]);
    assert_ne!(second["worktree"], first["worktree"]);
    assert!(old_worktree.is_dir(), "old attempt worktree remains evidence");
    let state = herdr_farm::runtime::snapshot(&lab.project).unwrap();
    assert_eq!(state.attempts.len(), 2);
    let binding = state.runtime_bindings.iter().find(|b| b.task.as_ref().is_some_and(|t| t.as_str() == "plan-retry")).unwrap();
    assert!(binding.identity.pane_id.is_empty() && binding.identity.worktree_path.is_empty(), "reservation uses the reset binding");
    assert!(state.attempts.iter().any(|a| a.id.as_str() == attempt && a.termination_observed));
    assert_eq!(state.tasks.iter().find(|t| t.id.as_str() == "plan-retry").unwrap().active_attempt.as_ref().unwrap().as_str(), second["attempt"].as_str().unwrap());
}

/// A control socket that could not be bound is refused before any server
/// starts, naming the path and its length, never as a vague "server exited".
#[test]
fn verify_interaction_refuses_a_too_long_socket_path_naming_path_and_length() {
    let mut lab = Lab::new();
    let long = lab.home.join("a-runtime-directory-name-that-is-deliberately-far-too-long-for-a-unix-socket-path-to-fit-in-sun-path");
    fs::create_dir(&long).unwrap();
    fs::set_permissions(&long, fs::Permissions::from_mode(0o700)).unwrap();
    lab.extra_env.push(("XDG_RUNTIME_DIR".into(), long.clone()));
    let (herdr, agent, home) = (lab.home.join("bin/herdr"), lab.home.join("bin/codex"), lab.home.join("agent-home-codex"));
    let error = lab.fail(&["profile", "verify-interaction", "demo", "codex-sol", "--herdr-executable", herdr.to_str().unwrap(),
        "--agent-executable", agent.to_str().unwrap(), "--execution-home", home.to_str().unwrap(), "--retain"]);
    assert!(error.contains("Unix socket path") && error.contains(long.to_str().unwrap()) && error.contains("bytes; the limit is 107"), "{error}");
    assert!(fs::read_dir(&long).unwrap().next().is_none(), "no socket directory is left behind");
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
    let state = herdr_farm::runtime::snapshot(&lab.project).unwrap();
    assert_eq!(state.attempts.len(), 2);
    assert_eq!(state.control.unwrap().state, ProjectState::Active);

    // Each task's dedicated server is still running while its attempt holds the
    // worker, so `launch stop` refuses; with --force it stops the server and
    // removes its socket directory. Nothing is left running or on disk.
    let servers = || String::from_utf8(Command::new("/usr/bin/pgrep").args(["-f", &format!("{} server", lab.home.join("bin/herdr").display())]).output().unwrap().stdout).unwrap().lines().count();
    assert_eq!(servers(), 2, "one dedicated server per task");
    let refused = lab.fail(&["launch", "demo", "stop", "--task", "plan-codex"]);
    assert!(refused.contains("still holds its worker"), "{refused}");
    for (task, _) in jobs {
        let report = lab.ok(&["launch", "demo", "stop", "--task", task, "--force"]);
        assert_eq!((report["stopped"].as_bool(), report["socket_directory_removed"].as_bool()), (Some(true), Some(true)), "{report}");
        let again = lab.ok(&["launch", "demo", "stop", "--task", task, "--force"]);
        assert_eq!(again["stopped"], false, "stopping twice is harmless: {again}");
    }
    assert_eq!(servers(), 0, "no stand-in Herdr server is left running");
    let left: Vec<_> = fs::read_dir(lab.runtime.path()).unwrap().flatten().map(|e| e.file_name()).collect();
    assert!(left.is_empty(), "no socket directory is left behind: {left:?}");
}

/// socket: agents that write into their execution home (`.codex/tmp/arg0` on
/// `--version`, a rollout during the session; `.claude.json`/`.claude/projects`)
/// must not fail verification; the retained evidence revalidates.
#[test]
fn verify_interaction_tolerates_agents_writing_into_their_execution_home() {
    let lab = Lab::new();
    lab.build_agent("codex", &writing_agent_source("codex-cli 0.159.2", ".codex/auth.json", ".codex/config.toml", ".codex/tmp", "arg0", ".codex/sessions/x.jsonl", ""));
    lab.build_agent("claude", &writing_agent_source("2.1.0 (Claude Code)", "", ".claude.json", ".claude/projects", "probe.jsonl", ".claude/projects/session.jsonl", ""));
    for (profile, kind) in [("codex-sol", "codex"), ("claude-sonnet", "claude")] {
        let report = lab.verify(profile, kind);
        assert_eq!(report["preparation"]["launchable"], true, "{kind}: {report}");
        let digest = report["preparation"]["reference"]["digest"].as_str().unwrap();
        // The home's contents changed (the agent wrote into it); revalidation still accepts it.
        let home = lab.home.join(format!("agent-home-{kind}"));
        assert!(if kind == "codex" { home.join(".codex/tmp/arg0").exists() } else { home.join(".claude/projects").exists() });
        let revalidated = lab.ok(&["profile", "revalidate", "demo", digest]);
        assert_eq!(revalidated["preparation"]["launchable"], true, "{kind}: {revalidated}");
    }
}

/// A home written to by the version probe is accepted by `profile prepare`.
#[test]
fn prepare_accepts_a_home_the_version_probe_wrote_into() {
    let lab = Lab::new();
    lab.build_agent("codex", &writing_agent_source("codex-cli 0.159.2", ".codex/auth.json", ".codex/config.toml", ".codex/tmp", "arg0", ".codex/sessions/x.jsonl", ""));
    let out = lab.prepare("codex-sol", "codex");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(lab.home.join("agent-home-codex/.codex/tmp/arg0").exists(), "the probe really wrote into the home");
}

/// A home swapped (renamed away and recreated at the same path) or made
/// group-writable while the agent is probed is still refused.
#[test]
fn prepare_refuses_a_swapped_or_repermissioned_execution_home() {
    let lab = Lab::new();
    let swap = r#"{ let h=std::path::Path::new(&home);let moved=format!("{home}.moved");std::fs::rename(h,&moved).unwrap();std::fs::create_dir(h).unwrap();std::fs::set_permissions(h,std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap(); }"#;
    lab.build_agent("codex", &writing_agent_source("codex-cli 0.159.2", ".codex/auth.json", ".codex/config.toml", ".codex/tmp", "arg0", ".codex/sessions/x.jsonl", swap));
    let out = lab.prepare("codex-sol", "codex");
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("execution home changed"), "{}", String::from_utf8_lossy(&out.stderr));
    fs::remove_dir_all(lab.home.join("agent-home-codex")).unwrap();
    fs::rename(lab.home.join("agent-home-codex.moved"), lab.home.join("agent-home-codex")).ok();
    fs::create_dir_all(lab.home.join("agent-home-codex")).unwrap();
    let chmod = r#"std::fs::set_permissions(&home,std::os::unix::fs::PermissionsExt::from_mode(0o770)).unwrap();"#;
    lab.build_agent("codex", &writing_agent_source("codex-cli 0.159.2", ".codex/auth.json", ".codex/config.toml", ".codex/tmp", "arg0", ".codex/sessions/x.jsonl", chmod));
    let out = lab.prepare("codex-sol", "codex");
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("execution home changed"), "{}", String::from_utf8_lossy(&out.stderr));
}

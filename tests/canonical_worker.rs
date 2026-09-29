#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! Canonical worker launch, brief and termination through the compiled CLI and
//! `ticker run`. A task is queued, drafted, owner-signed and reserved through
//! the CLI; the ticker then creates the worker on a Herdr stand-in server that
//! really runs the supervised command, briefs it and later stops it. The server
//! logs every request, so each test counts external effects from the outside.
use herdr_projects::{authority, domain::*, migration, operations::DeliveryState, runtime};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{fs, os::unix::fs::{MetadataExt, PermissionsExt}, path::PathBuf, process::{Child, Command, Output, Stdio}, time::{Duration, Instant}};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-projects");

/// A Herdr server on `argv[1]` that runs `workspace.create_command` for real
/// and logs each request to `requests` beside the socket. `ping.json` there
/// replaces the ping reply; `lose-create` runs the command but drops the reply;
/// `drop-release` drops gate-release input unsent and unanswered; `vanish`
/// closes the worker's workspace, pane and agent without touching its process.
const SERVER: &str = r#"
import json,os,sys,socket,subprocess
path=sys.argv[1];root=os.path.dirname(path);s={}
server=socket.socket(socket.AF_UNIX);server.bind(path);server.listen()
while True:
 c,_=server.accept();f=c.makefile('rw');r=json.loads(f.readline());m=r['method'];p=r.get('params') or {}
 live='pid' in s and not os.path.exists(os.path.join(root,'vanish'))
 with open(os.path.join(root,'requests'),'a') as log:log.write(json.dumps({'method':m,'params':p})+'\n')
 pane={'pane_id':'w1:p1','workspace_id':'w1','tab_id':'w1:t1','terminal_id':'term1','cwd':s.get('cwd')}
 agent=dict(pane,agent='claude',interactive_ready=True,agent_status='idle',**({'name':s['name']} if 'name' in s else {}))
 res=None
 if m=='ping':
  override=os.path.join(root,'ping.json')
  res=json.load(open(override)) if os.path.exists(override) else {'type':'pong','version':'0.9.1','capabilities':{'workspace_create_command':True}}
 elif m=='workspace.create_command' and 'pid' not in s:
  fifo=os.path.join(root,'input');os.mkfifo(fifo);fd=os.open(fifo,os.O_RDWR)
  child=subprocess.Popen(p['command'],cwd=p['cwd'],stdin=fd,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,start_new_session=True,env={'PATH':'/usr/bin:/bin'})
  s.update(pid=child.pid,argv=p['command'],cwd=p['cwd'],label=p['label'],fifo=fifo)
  if not os.path.exists(os.path.join(root,'lose-create')):res={'type':'workspace_created','workspace':{'workspace_id':'w1'},'root_pane':{'pane_id':'w1:p1'}}
 elif m=='workspace.list':res={'type':'workspace_list','workspaces':[{'workspace_id':'w1','label':s['label'],'pane_count':1,'tab_count':1}] if live else []}
 elif m=='pane.list':res={'panes':[pane] if live else []}
 elif m=='pane.get':res={'pane':pane}
 elif m=='pane.process_info':res={'process_info':{'pane_id':'w1:p1','foreground_processes':[{'pid':s['pid'],'argv':s['argv']}] if live else []}}
 elif m=='pane.send_input' and os.path.exists(os.path.join(root,'drop-release')):pass
 elif m=='pane.send_input':
  fd=os.open(s['fifo'],os.O_WRONLY);os.write(fd,p['text'].encode());os.close(fd);s['released']=True;res={'type':'ok'}
 elif m=='agent.list':res={'type':'agent_list','agents':[agent] if live and s.get('released') else []}
 elif m=='agent.rename':s['name']=agent['name']=p['name'];res={'type':'agent_info','agent':agent}
 elif m=='agent.explain':res={'type':'agent_explain','explain':{'agent':'claude','state':'idle','manifest_source':'bundled','manifest_version':'2026.09.14.1',
  'matched_rule':{'id':'prompt','state':'idle'},'visible_idle':True,'visible_blocker':False,'visible_working':False,'screen_detection_skipped':False,
  'skip_state_update':False,'local_override_shadowing_remote':False,'fallback_reason':None,'warning':None}}
 elif m=='agent.prompt':res={'type':'agent_prompted','agent':agent}
 if res is not None:f.write(json.dumps({'id':r['id'],'result':res})+'\n');f.flush()
 c.close()
"#;

struct Lab { home: tempfile::TempDir, project: PathBuf, key: PathBuf, repo: PathBuf, herdr: PathBuf, profile: VersionedReference, binding: String, server: Option<Child> }

impl Drop for Lab {
    fn drop(&mut self) {
        let _ = Command::new("/usr/bin/pkill").args(["-KILL", "-f"]).arg(self.home.path().join("agent-home")).status();
        if let Some(server) = &mut self.server { let _ = server.kill(); let _ = server.wait(); }
    }
}

struct Ticker(Child);
impl Drop for Ticker { fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); } }

impl Lab {
    /// An active project with an owner key, a SHA-256 repository, the queued
    /// task `work` bound to the lab server, and a `worker` profile whose
    /// budget table is `budget`. The server runs only once `serve` is called.
    fn new(budget: &str) -> Self { Self::bound(budget, |repo| repo.to_owned()) }
    /// As `new`, with the binding's working directory `cwd(repository)`.
    fn bound(budget: &str, cwd: impl Fn(&std::path::Path) -> PathBuf) -> Self {
        let home = tempfile::tempdir().unwrap();
        let key = home.path().join("owner");
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&key).output().unwrap().status.success());
        let public = fs::read_to_string(key.with_extension("pub")).unwrap().split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        let config = home.path().join(".config/herdr-projects/config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, format!("[authority]\nversion=1\nrevision=1\napproval_public_key={public:?}\n[profiles.worker]\nkind='claude'\npermission_policy='interactive'\n[profiles.worker.budget]\nmax_wall_seconds=600\n{budget}\n")).unwrap();
        for dir in ["repo", "bin", "agent-home", "lab"] { fs::create_dir(home.path().join(dir)).unwrap(); }
        let mut lab = Lab { project: home.path().join("root/demo"), key, repo: home.path().join("repo"), herdr: home.path().join("bin/herdr"),
            profile: VersionedReference { id: String::new(), revision: 1, digest: String::new() }, binding: String::new(), server: None, home };
        for command in ["new", "pause"] { lab.ok(&[command, "demo"]); }
        migration::apply(&lab.project, &migration::inspect_with_config(&lab.project, &config).unwrap(), true).unwrap();
        lab.git(&["init", "-q", "--object-format=sha256"]);
        lab.git(&["commit", "-q", "--allow-empty", "-m", "base"]);
        lab.ok(&["task", "demo", "add", "work", "--title", "work", "--expected-head", &lab.head().to_string()]);
        let request = lab.path("queue.json");
        fs::write(&request, r#"{"priority":0,"dependencies":[]}"#).unwrap();
        lab.ok(&["task", "demo", "queue", "work", "--input-file", request.to_str().unwrap(), "--expected-revision", "1", "--expected-head", &lab.head().to_string()]);
        let policy = lab.state().scheduler.unwrap().policy.revision.to_string();
        lab.ok(&["scheduler", "demo", "policy", "--max-active-workers", "1", "--max-attempts-per-task", "3", "--expected-revision", &policy, "--expected-head", &lab.head().to_string()]);
        let route = RuntimeRoute { socket: lab.socket().display().to_string(), cwd: cwd(&lab.repo.canonicalize().unwrap()).display().to_string(), ..Default::default() };
        let id = TaskId::new("work").unwrap();
        let revision = runtime::snapshot(&lab.project).unwrap().tasks.into_iter().find(|t| t.id == id).unwrap().revision;
        let change = runtime::create_binding(&lab.project, Some(&id), Some(revision), lab.head(), &route).unwrap();
        lab.binding = change.binding.id;
        lab.resume();
        lab.write_binaries();
        lab
    }
    /// Record a fresh observation of every (resource-free) binding and set the project active.
    fn resume(&self) {
        let state = self.state();
        let config = self.path(".config/herdr-projects/config.toml");
        let observations = state.runtime_bindings.iter().map(|binding| herdr_projects::reconcile::RuntimeObservation { binding: binding.id.clone(), binding_revision: binding.revision,
            task_revision: binding.task.as_ref().map(|id| state.tasks.iter().find(|t| &t.id == id).unwrap().revision),
            observed_unix_ms: jiff::Timestamp::now().as_millisecond(), collector: "herdr-git-v2".into(),
            config_digest: migration::config_reference(&config).unwrap().digest.clone(), ..Default::default() }).collect::<Vec<_>>();
        migration::open_active(&self.project).unwrap().record_observations(state.head, &observations).unwrap();
        let control = self.state().control.unwrap().revision.to_string();
        self.ok(&["runtime", "demo", "state", "active", "--expected-revision", &control, "--expected-head", &self.head().to_string()]);
    }
    fn path(&self, name: &str) -> PathBuf { self.home.path().join(name) }
    fn socket(&self) -> PathBuf { self.path("lab/native.sock") }
    fn cli(&self, args: &[&str]) -> Output {
        Command::new(BIN).env_clear().env("HOME", self.home.path()).env("PATH", "/usr/bin:/bin")
            .args(["--root", self.path("root").to_str().unwrap()]).args(args).output().unwrap()
    }
    fn ok(&self, args: &[&str]) -> Value {
        let out = self.cli(args);
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
    }
    /// `ok` against a running ticker. Its own writes can move the head, and its
    /// project turns can hold the lock, so the arguments are rebuilt from the
    /// current state and the command retried for a bounded time.
    fn ok_live(&self, args: &dyn Fn() -> Vec<String>) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let args = args();
            let args = args.iter().map(String::as_str).collect::<Vec<_>>();
            let out = self.cli(&args);
            if out.status.success() { return serde_json::from_slice(&out.stdout).unwrap_or(Value::Null); }
            assert!(Instant::now() < deadline, "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    /// A refused command writes nothing; returns its stderr.
    fn refused(&self, args: &[&str]) -> String {
        let before = self.state();
        let out = self.cli(args);
        assert!(!out.status.success(), "{args:?} accepted: {}", String::from_utf8_lossy(&out.stdout));
        assert_eq!(self.state(), before, "{args:?} was refused but wrote");
        String::from_utf8_lossy(&out.stderr).into_owned()
    }
    /// The store may be mid-publication under a running ticker; read again.
    fn state(&self) -> Snapshot {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop { match runtime::snapshot(&self.project) { Ok(s) => return s, Err(e) => { assert!(Instant::now() < deadline, "{e:#}"); std::thread::sleep(Duration::from_millis(20)); } } }
    }
    /// Since `before`, the ticker recorded runtime observations and nothing else.
    fn assert_only_observed(&self, before: &Snapshot) {
        let now = self.state();
        let written = now.events.iter().filter(|e| e.sequence > before.head && e.kind != "runtime.observed").map(|e| &e.kind).collect::<Vec<_>>();
        assert!(written.is_empty(), "{written:?}");
        assert_eq!((&now.tasks, &now.attempts, &now.operations, &now.deliveries, &now.approvals, &now.ownership, &now.control),
            (&before.tasks, &before.attempts, &before.operations, &before.deliveries, &before.approvals, &before.ownership, &before.control));
    }
    fn head(&self) -> u64 { self.state().head }
    fn events(&self, kind: &str) -> Vec<Event> { self.state().events.into_iter().filter(|e| e.kind == kind).collect() }
    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("/usr/bin/git").env_clear().env("PATH", "/usr/bin:/bin").env("HOME", self.home.path())
            .env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "fixture").env("GIT_AUTHOR_EMAIL", "fixture@example.com")
            .env("GIT_COMMITTER_NAME", "fixture").env("GIT_COMMITTER_EMAIL", "fixture@example.com")
            .current_dir(&self.repo).args(args).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }
    /// A Herdr bridge to the lab server and a `claude` that stays up. The agent
    /// must be the exact executable it names, so build a tiny one.
    fn write_binaries(&self) {
        fs::write(&self.herdr, "#!/usr/bin/python3\nimport os,socket,sys,json\nif sys.argv[1:]==['--version']:print('herdr 0.9.1');sys.exit(0)\n\
probe={('pane','list'):'pane.list',('agent','list'):'agent.list'}.get(tuple(sys.argv[1:]))\nassert probe or sys.argv[1:]==['remote-api-bridge']\n\
line=json.dumps({'id':'probe','method':probe}).encode()+b'\\n' if probe else sys.stdin.buffer.readline()\n\
c=socket.socket(socket.AF_UNIX);c.connect(os.environ['HERDR_SOCKET_PATH']);c.sendall(line)\nreply=c.makefile('rb').readline()\nif not reply:sys.exit(1)\n\
sys.stdout.buffer.write(json.dumps({'result':json.loads(reply)['result']}).encode() if probe else reply)\n").unwrap();
        let (agent, source) = (self.path("bin/claude"), self.path("bin/claude.rs"));
        fs::write(&source, "fn main(){if std::env::args().nth(1).as_deref()==Some(\"--version\"){println!(\"2.1.0 (Claude Code)\");return}loop{std::thread::park()}}").unwrap();
        assert!(Command::new("rustc").args(["--edition", "2021", "-o"]).arg(&agent).arg(&source).status().unwrap().success());
        for path in [&self.herdr, &agent] { fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap(); }
    }
    /// Prepare `worker` over the lab binaries; only the native interaction
    /// evidence, which needs a real agent session, is planted.
    fn prepare_profile(&mut self) {
        use herdr_projects::worker_supervision::{ProcessIncarnation, SupervisorIdentity};
        let prepared = self.ok(&["profile", "prepare", "demo", "worker", "--herdr-executable", self.herdr.to_str().unwrap(),
            "--agent-executable", self.path("bin/claude").to_str().unwrap(), "--execution-home", self.path("agent-home").to_str().unwrap()]);
        let mut profile: FrozenProfile = serde_json::from_value(prepared["profile"].clone()).unwrap();
        #[derive(serde::Serialize)] struct Interaction { session: ResourceIdentity, terminal: &'static str, readiness_manifest: &'static str, prompt_digest: String, acknowledged_unix_ms: i64 }
        #[derive(serde::Serialize)] struct Evidence { version: u32, prepared_profile: VersionedReference, supervisor: SupervisorIdentity, native_kind: String, observed_unix_ms: i64, stopped_unix_ms: i64, interaction: Interaction }
        let evidence = Evidence { version: 2, prepared_profile: profile.reference().unwrap(), native_kind: profile.kind.clone(), observed_unix_ms: 1000, stopped_unix_ms: 1001,
            supervisor: SupervisorIdentity { version: 1, boot_id: "00000000-0000-0000-0000-000000000001".into(), host_id: None, observer_namespace: (1, 2), worker_namespace: (1, 3),
                outer: ProcessIncarnation { pid: 20, device: 1, inode: 4 }, init: ProcessIncarnation { pid: 21, device: 1, inode: 5 } },
            interaction: Interaction { session: ResourceIdentity { device: 1, inode: 2, born_secs: 1, born_nanos: 0 }, terminal: "fixture-terminal", readiness_manifest: "fixture-manifest", prompt_digest: "a".repeat(64), acknowledged_unix_ms: 999 } };
        let hash = format!("{:x}", Sha256::digest(serde_json::to_vec(&evidence).unwrap()));
        let supported = CapabilityEvidence::Supported { evidence: VersionedReference { id: format!("native-transport-{hash}"), revision: 1, digest: hash } };
        let c = &mut profile.capabilities;
        (c.launch, c.stop, c.readiness_observation, c.prompt_submission) = (supported.clone(), supported.clone(), supported.clone(), supported);
        let reference = profile.reference().unwrap();
        let store = self.project.join(".state/state.db").canonicalize().unwrap();
        let metadata = fs::metadata(&store).unwrap();
        let report = json!({"preparation":{"profile":profile,"reference":reference,"launchable":true,"protocol_capable":false,"certified":false},
            "evidence":evidence,"source_store":[store,metadata.dev(),metadata.ino()]}).to_string();
        rusqlite::Connection::open(&store).unwrap().execute("INSERT INTO native_profiles(profile_digest,report,report_digest,sequence) VALUES(?1,?2,?3,(SELECT max(sequence) FROM events))",
            rusqlite::params![reference.digest, report, format!("{:x}", Sha256::digest(report.as_bytes()))]).unwrap();
        self.profile = reference;
    }
    /// Retain `instructions` as worker knowledge and write the launch selection.
    fn selection(&self, instructions: &str) -> PathBuf { self.selection_for("work", &self.binding, instructions) }
    /// As `selection`, for `task` on `binding`.
    fn selection_for(&self, task: &str, binding: &str, instructions: &str) -> PathBuf {
        fs::write(self.project.join("PROJECT.md"), instructions).unwrap();
        let scope = self.path("scope.json");
        fs::write(&scope, json!({"schema_version":1,"task_id":task,"profile":"worker","domains":[],"paths":[],"pinned_keys":[],"sensitivity":"default"}).to_string()).unwrap();
        let snapshot = self.ok(&["memory", "demo", "snapshot", "--task", task, "--profile", "worker", "--input-file", scope.to_str().unwrap(), "--worker"]);
        let selection = self.path("selection.json");
        fs::write(&selection, json!({"task":task,"binding":binding,"profile":self.profile,
            "knowledge":{"id":snapshot["id"],"revision":1,"digest":snapshot["manifest_hash"]},"repositories":[self.repo.canonicalize().unwrap()]}).to_string()).unwrap();
        selection
    }
    /// Draft, owner-sign, import and reserve a launch; returns (approval id, attempt).
    fn reserve(&mut self, instructions: &str) -> (String, AttemptId) {
        self.prepare_profile();
        let selection = self.selection(instructions);
        let drafted = self.ok(&["launch", "demo", "draft", "--selection", selection.to_str().unwrap(), "--expected-head", &self.head().to_string()]);
        let document = self.path("approval.json");
        fs::write(&document, serde_json::to_vec_pretty(&drafted["approval"]).unwrap()).unwrap();
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-Y", "sign", "-f"]).arg(&self.key).args(["-n", authority::SIGNATURE_NAMESPACE]).arg(&document).output().unwrap().status.success());
        let approval = self.ok(&["approval", "demo", "import", document.to_str().unwrap(), self.path("approval.json.sig").to_str().unwrap(), "--expected-head", &self.head().to_string()]);
        let reservation = self.ok(&["launch", "demo", "reserve", "--selection", selection.to_str().unwrap(), "--approval-digest", approval["digest"].as_str().unwrap(), "--expected-head", &self.head().to_string()]);
        let id = self.state().approvals.iter().find(|a| a.reference.digest == approval["digest"].as_str().unwrap()).unwrap().reference.id.clone();
        (id, AttemptId::new(reservation["record"]["attempt"].as_str().unwrap()).unwrap())
    }
    fn serve(&mut self) {
        self.server = Some(Command::new("/usr/bin/python3").args(["-c", SERVER]).arg(self.socket()).spawn().unwrap());
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.socket().exists() { assert!(Instant::now() < deadline); std::thread::sleep(Duration::from_millis(10)); }
    }
    /// Every request the server has received, as (method, params).
    fn requests(&self) -> Vec<(String, Value)> {
        fs::read_to_string(self.path("lab/requests")).unwrap_or_default().lines()
            .map(|line| { let v: Value = serde_json::from_str(line).unwrap(); (v["method"].as_str().unwrap().to_owned(), v["params"].clone()) }).collect()
    }
    fn count(&self, method: &str) -> usize { self.requests().iter().filter(|(m, _)| m == method).count() }
    fn attempt(&self, id: &AttemptId) -> Attempt { self.state().attempts.into_iter().find(|a| &a.id == id).unwrap() }
    fn spawn(&self) -> Ticker {
        Ticker(Command::new(BIN).env_clear().env("HOME", self.home.path()).env("PATH", "/usr/bin:/bin").env("HERDR_BIN_PATH", &self.herdr)
            .args(["--root", self.path("root").to_str().unwrap(), "ticker", "run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap())
    }
    fn wait(&self, ticker: &mut Ticker, seconds: u64, predicate: &dyn Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        // Predicates read `runtime::snapshot` (whole-store integrity check plus
        // a full read); at a fixed 20 ms they competed with the ticker being
        // waited on. Back off to 250 ms; the deadline is unchanged.
        let mut pause = Duration::from_millis(20);
        while !predicate() {
            assert!(ticker.0.try_wait().unwrap().is_none(), "ticker exited");
            assert!(Instant::now() < deadline, "{}", fs::read_to_string(self.path("root/.ticker.log")).unwrap_or_default());
            std::thread::sleep(pause);
            pause = (pause * 2).min(Duration::from_millis(250));
        }
    }
    fn stop(&self, mut ticker: Ticker) {
        let stop = self.path("root/.ticker.stop");
        fs::write(&stop, b"").unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        while ticker.0.try_wait().unwrap().is_none() { assert!(Instant::now() < deadline); std::thread::sleep(Duration::from_millis(10)); }
        fs::remove_file(&stop).unwrap();
    }
    /// Run a fresh ticker for `passes` completed passes (each pass republishes
    /// the executor metrics file), then stop it.
    fn run_passes(&self, passes: usize) {
        let metrics = self.path("root/.ticker-metrics.json");
        let inode = || fs::metadata(&metrics).map(|m| m.ino()).ok();
        let (mut last, mut seen) = (inode(), 0);
        let mut ticker = self.spawn();
        self.wait(&mut ticker, 60, &|| inode() != last);
        while seen < passes {
            self.wait(&mut ticker, 60, &|| inode() != last);
            (last, seen) = (inode(), seen + 1);
        }
        self.stop(ticker);
    }
    /// Run a fresh ticker until `done` holds, failing if `passes` passes
    /// complete first, then stop it. A pass publishes before the
    /// background work it admitted (an observation) finishes, and stopping
    /// the ticker cancels that work, so wait for its outcome, not a pass.
    fn run_until(&self, passes: usize, done: &dyn Fn() -> bool) {
        let metrics = self.path("root/.ticker-metrics.json");
        let inode = || fs::metadata(&metrics).map(|m| m.ino()).ok();
        let (mut last, mut seen) = (inode(), 0);
        let mut ticker = self.spawn();
        while !done() {
            self.wait(&mut ticker, 60, &|| done() || inode() != last);
            if inode() != last && !done() {
                (last, seen) = (inode(), seen + 1);
                assert!(seen < passes, "not done within {passes} passes: {}", fs::read_to_string(self.path("root/.ticker.log")).unwrap_or_default());
            }
        }
        self.stop(ticker);
    }
    /// Run a fresh ticker until the server has seen `passes` more `method`
    /// requests, then stop it.
    fn run_for(&self, method: &str, passes: usize) {
        let seen = self.count(method);
        let mut ticker = self.spawn();
        self.wait(&mut ticker, 60, &|| self.count(method) >= seen + passes);
        self.stop(ticker);
    }
}

/// Replaces the unit tests `native_brief_submits_retained_bytes_once_and_commits_running_state`,
/// `native_resource_creation_records_gated_target_once_and_cancellation_retains_it`,
/// `controller_hints_rotate_brief_and_termination_without_granting_authority`,
/// `live_worker_without_cancellation_is_observed_without_being_stopped` and
/// `desired_stop_works_while_paused_and_revoked_and_retains_resources`.
#[test]
fn ticker_launches_and_briefs_once_then_stops_a_cancelled_worker_while_paused_and_revoked() {
    let mut lab = Lab::new("unknown_usage='allow_with_warning'");
    let (approval, attempt) = lab.reserve("Retained instructions");
    // Instructions changed after approval are not what the worker is sent.
    fs::write(lab.project.join("PROJECT.md"), "Replacement must not be sent").unwrap();
    lab.serve();
    let briefed = || { let s = lab.state(); s.operations.iter().any(|o| o.kind == "runtime.worker_brief" && s.deliveries.iter().any(|d| d.operation == o.id && d.state == DeliveryState::Confirmed)) };
    let mut ticker = lab.spawn();
    lab.wait(&mut ticker, 120, &briefed);
    lab.stop(ticker);

    // One gated creation with the usage warning, one target in the created pane, one brief.
    assert_eq!((lab.count("workspace.create_command"), lab.count("agent.prompt")), (1, 1));
    // The worker runs in its own namespace, killed with it, under the
    // profile's 600 s wall deadline, and ends by running the exact agent
    // executable.
    let argv: Vec<String> = serde_json::from_value(lab.requests().into_iter().find(|(m, _)| m == "workspace.create_command").unwrap().1["command"].clone()).unwrap();
    let agent = lab.path("bin/claude").display().to_string();
    assert!(argv.iter().any(|a| a == "--kill-child=KILL"), "{argv:?}");
    let wall = argv.iter().position(|a| a == "600s").unwrap_or_else(|| panic!("{argv:?}"));
    assert!(argv[..wall].ends_with(&["--".to_owned()]) && argv.last() == Some(&agent), "{argv:?}");
    // The native agent name is bounded and derived from the full attempt id.
    let names: Vec<String> = lab.requests().into_iter().filter(|(m, _)| m == "agent.rename").map(|(_, p)| p["name"].as_str().unwrap().to_owned()).collect();
    assert!(!names.is_empty() && names.iter().all(|n| n == &names[0]), "{names:?}");
    let name = &names[0];
    assert!(name.len() == 32 && name.as_bytes()[0].is_ascii_lowercase() && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-_".contains(&b)), "{name}");
    assert!(attempt.as_str().len() > 32 && !attempt.as_str().starts_with(name.as_str()), "{} {name}", attempt.as_str());
    let creation = &lab.events("runtime.launch_creation")[0];
    assert_eq!(creation.payload["usage_warning"], "provider_usage_unavailable");
    let target: LaunchTarget = serde_json::from_value(lab.events("runtime.launch_target")[0].payload.clone()).unwrap();
    assert_eq!((target.version, target.route.pane_id.as_str(), &target.attempt), (2, "w1:p1", &attempt));
    let text = lab.requests().into_iter().find(|(m, _)| m == "agent.prompt").unwrap().1["text"].as_str().unwrap().to_owned();
    assert!(text.contains("Retained instructions") && !text.contains("Replacement must not be sent"), "{text}");
    let state = lab.state();
    let brief = state.operations.iter().find(|o| o.kind == "runtime.worker_brief").unwrap();
    assert_eq!(brief.payload["prompt_digest"], format!("{:x}", Sha256::digest(text.as_bytes())));
    let running = lab.attempt(&attempt);
    assert_eq!((running.state, running.retains_capacity(), running.termination_observed), (AttemptState::Running, true, false));
    let started: LaunchStartedReceipt = serde_json::from_value(lab.events("runtime.launch_started")[0].payload.clone()).unwrap();
    let supervisor = started.supervisor.unwrap();

    // Later passes observe the live worker without stopping or briefing it again.
    let observed = lab.state();
    let listed = lab.count("agent.list");
    let mut ticker = lab.spawn();
    lab.wait(&mut ticker, 60, &|| lab.count("agent.list") >= listed + 2);
    assert_eq!((lab.count("workspace.create_command"), lab.count("agent.prompt")), (1, 1));
    assert_eq!(lab.attempt(&attempt), running);
    assert!(lab.events("runtime.worker_terminated").is_empty());
    assert_eq!((lab.state().tasks, lab.state().deliveries), (observed.tasks, observed.deliveries));
    assert!(!herdr_projects::worker_supervision::SupervisorObservation::recover_exited(&supervisor).unwrap(), "the worker is running");

    // The operator cancels the attempt, revokes its approval and pauses the
    // project while that same ticker keeps running. The ticker keeps its store
    // reads' connections across passes, and its next passes must still see
    // these writes.
    let report = lab.project.join("REPORT.md");
    fs::write(&report, "retain this report").unwrap();
    lab.ok_live(&|| ["task", "demo", "cancel-attempt", attempt.as_str(), "--expected-revision", &running.revision.to_string(), "--expected-head", &lab.head().to_string(), "--reason", "operator stop"].map(String::from).to_vec());
    lab.ok_live(&|| ["approval", "demo", "revoke", &approval, "--expected-head", &lab.head().to_string(), "--reason", "stop execution"].map(String::from).to_vec());
    lab.ok_live(&|| { let s = lab.state(); ["runtime", "demo", "state", "paused", "--expected-revision", &s.control.unwrap().revision.to_string(), "--expected-head", &s.head.to_string()].map(String::from).to_vec() });
    let before = lab.state();
    lab.wait(&mut ticker, 60, &|| lab.attempt(&attempt).termination_observed);
    lab.stop(ticker);
    let after = lab.state();
    assert!(herdr_projects::worker_supervision::SupervisorObservation::recover_exited(&supervisor).unwrap(), "the worker has exited");
    let stopped = lab.attempt(&attempt);
    assert_eq!((stopped.state, stopped.retains_capacity()), (AttemptState::Cancelled, false));
    assert_eq!((after.tasks[0].state, after.tasks[0].active_attempt.as_ref()), (TaskState::Cancelled, None));
    assert_eq!((&after.control, &after.ownership, &after.runtime_bindings), (&before.control, &before.ownership, &before.runtime_bindings));
    assert_eq!(fs::read_to_string(&report).unwrap(), "retain this report");
    assert_eq!(lab.events("runtime.worker_resources_retained").len(), 1);
    assert_eq!((lab.count("workspace.create_command"), lab.count("agent.prompt")), (1, 1));
    // A later pass has nothing left to stop.
    lab.run_for("agent.list", 1);
    lab.assert_only_observed(&after);
}


/// Replaces `lost_resource_creation_reply_retains_claim_and_never_creates_again`.
#[test]
fn ticker_recovers_a_lost_creation_reply_without_creating_again() {
    let mut lab = Lab::new("unknown_usage='allow_with_warning'");
    let (_, attempt) = lab.reserve("Retained instructions");
    fs::write(lab.path("lab/lose-create"), "").unwrap();
    lab.serve();
    // The creation reply is lost. The claim is kept and the exact workspace is
    // recovered from observation; nothing is created twice, across a restart.
    let mut ticker = lab.spawn();
    lab.wait(&mut ticker, 60, &|| lab.count("workspace.create_command") == 1);
    lab.stop(ticker);
    let launch = lab.state().deliveries.into_iter().find(|d| d.operation.as_str().starts_with("launch-")).unwrap();
    assert_eq!((launch.attempts, launch.state == DeliveryState::Pending), (1, false));
    assert!(lab.attempt(&attempt).retains_capacity());
    let briefed = || { let s = lab.state(); s.operations.iter().any(|o| o.kind == "runtime.worker_brief" && s.deliveries.iter().any(|d| d.operation == o.id && d.state == DeliveryState::Confirmed)) };
    let mut ticker = lab.spawn();
    lab.wait(&mut ticker, 120, &briefed);
    lab.stop(ticker);
    assert_eq!((lab.count("workspace.create_command"), lab.count("agent.prompt")), (1, 1));
    assert!(lab.count("workspace.list") >= 1);
    assert_eq!((lab.events("runtime.launch_target").len(), lab.events("runtime.launch_started").len()), (1, 1));
    assert_eq!(lab.attempt(&attempt).state, AttemptState::Running);
}

/// Replaces `unsupported_connected_server_refuses_creation_before_approval_or_worktrees`
/// and, with the next test, the paused and cancelled cases of
/// `prepared_launch_selection_rotates_stages_and_excludes_cancelled_or_expired_effects`.
#[test]
fn ticker_launches_nothing_on_a_server_without_the_launch_contract_or_while_paused() {
    let mut lab = Lab::new("unknown_usage='allow_with_warning'");
    let (approval, attempt) = lab.reserve("Retained instructions");
    lab.serve();
    let before = lab.state();
    for reply in [
        json!({"type":"pong","version":"0.9.1","capabilities":{"workspace_create_command":"true"}}),
        json!({"type":"pong","version":"0.9.2","capabilities":{"workspace_create_command":true}}),
        json!({"type":"unknown","version":"0.9.1","capabilities":{"workspace_create_command":true}}),
    ] {
        fs::write(lab.path("lab/ping.json"), reply.to_string()).unwrap();
        lab.run_for("ping", 2);
        assert!(lab.requests().iter().all(|(m, _)| m == "ping"), "{reply}: {:?}", lab.requests());
        lab.assert_only_observed(&before);
        assert!(!lab.project.join(".state/worktrees").exists());
    }
    assert!(lab.state().approvals.iter().any(|a| a.reference.id == approval && a.consumed.is_none()));
    assert_eq!(lab.attempt(&attempt).state, AttemptState::Reserved);
    // A capable server, but the project is paused: passes run, nothing is dispatched.
    fs::remove_file(lab.path("lab/ping.json")).unwrap();
    let control = lab.state().control.unwrap().revision.to_string();
    lab.ok(&["runtime", "demo", "state", "paused", "--expected-revision", &control, "--expected-head", &lab.head().to_string()]);
    let (paused, asked) = (lab.state(), lab.requests().len());
    lab.run_passes(2);
    assert_eq!(lab.requests().len(), asked, "{:?}", &lab.requests()[asked..]);
    lab.assert_only_observed(&paused);
}

#[test]
fn ticker_does_not_dispatch_a_launch_cancelled_before_creation() {
    let mut lab = Lab::new("unknown_usage='allow_with_warning'");
    let (approval, attempt) = lab.reserve("Retained instructions");
    let reserved = lab.attempt(&attempt);
    lab.ok(&["task", "demo", "cancel-attempt", attempt.as_str(), "--expected-revision", &reserved.revision.to_string(), "--expected-head", &lab.head().to_string(), "--reason", "cancel selected launch"]);
    lab.serve();
    let cancelled = lab.state();
    lab.run_passes(2);
    assert!(lab.requests().is_empty(), "{:?}", lab.requests());
    lab.assert_only_observed(&cancelled);
    assert!(lab.state().approvals.iter().any(|a| a.reference.id == approval && a.consumed.is_none()));
    assert!(!lab.project.join(".state/worktrees").exists());
}

/// Replaces `resource_preparation_enforces_profile_budget_before_claim_or_external_effect`.
#[test]
fn profile_budgets_refuse_preparation_and_draft_before_any_approval() {
    // The retained knowledge fits in 500 tokens; the whole brief does not.
    let mut small = Lab::new("soft_input_tokens=500\nunknown_usage='allow_with_warning'");
    small.prepare_profile();
    let selection = small.selection("Retained instructions");
    let error = small.refused(&["launch", "demo", "draft", "--selection", selection.to_str().unwrap(), "--expected-head", &small.head().to_string()]);
    assert!(error.contains("exceeding the captured budget"), "{error}");
    assert!(small.state().approvals.is_empty() && small.state().attempts.is_empty());

    let blocking = Lab::new("unknown_usage='block'");
    let error = blocking.refused(&["profile", "prepare", "demo", "worker", "--herdr-executable", blocking.herdr.to_str().unwrap(),
        "--agent-executable", blocking.path("bin/claude").to_str().unwrap(), "--execution-home", blocking.path("agent-home").to_str().unwrap()]);
    assert!(error.contains("verified usage telemetry is required"), "{error}");
}

/// Replaces `native_resource_creation_records_gated_target_once_and_cancellation_retains_it`.
#[test]
fn ticker_retires_a_cancelled_gated_worker_without_starting_it() {
    let mut lab = Lab::new("unknown_usage='allow_with_warning'");
    let (_, attempt) = lab.reserve("Retained instructions");
    fs::write(lab.path("lab/drop-release"), "").unwrap();
    lab.serve();
    let mut ticker = lab.spawn();
    lab.wait(&mut ticker, 60, &|| !lab.events("runtime.launch_target").is_empty() && lab.count("pane.send_input") >= 1);
    lab.stop(ticker);
    let target: LaunchTarget = serde_json::from_value(lab.events("runtime.launch_target")[0].payload.clone()).unwrap();
    let supervisor = target.supervisor.clone().unwrap();
    assert!(!herdr_projects::worker_supervision::SupervisorObservation::recover_exited(&supervisor).unwrap(), "the gated worker is waiting");
    let staged = lab.attempt(&attempt);
    assert_eq!((staged.state, staged.retains_capacity()), (AttemptState::Reserved, true));
    lab.ok(&["task", "demo", "cancel-attempt", attempt.as_str(), "--expected-revision", &staged.revision.to_string(), "--expected-head", &lab.head().to_string(), "--reason", "stop staged resource"]);
    let mut ticker = lab.spawn();
    lab.wait(&mut ticker, 60, &|| lab.attempt(&attempt).termination_observed);
    lab.stop(ticker);
    let stopped = lab.attempt(&attempt);
    assert_eq!((stopped.state, stopped.retains_capacity()), (AttemptState::Cancelled, false));
    assert!(herdr_projects::worker_supervision::SupervisorObservation::recover_exited(&supervisor).unwrap(), "the gated worker has exited");
    assert!(lab.events("runtime.launch_started").is_empty() && lab.state().ownership.is_empty());
    assert_eq!(serde_json::from_value::<LaunchTarget>(lab.events("runtime.launch_target")[0].payload.clone()).unwrap(), target);
    assert_eq!((lab.count("workspace.create_command"), lab.count("agent.prompt")), (1, 0));
}

impl Lab {
    /// The worktree `launch draft` planned for `attempt`, as `memory attempt-input` reports it.
    fn planned_worktree(&self, attempt: &AttemptId) -> PathBuf {
        PathBuf::from(self.ok(&["memory", "demo", "attempt-input", "--attempt", attempt.as_str()])["worktrees"][0]["path"].as_str().unwrap())
    }
    /// Ticker passes refuse to prepare the reserved launch with `reason`: no
    /// worktree, no Herdr request, the approval unused and the attempt reserved.
    fn assert_preparation_refused(&mut self, approval: &str, attempt: &AttemptId, reason: &str) {
        let worktree = self.planned_worktree(attempt);
        self.serve();
        let before = self.state();
        let log = || fs::read_to_string(self.path("root/.ticker.log")).unwrap_or_default();
        let mut ticker = self.spawn();
        self.wait(&mut ticker, 60, &|| log().matches(reason).count() >= 2);
        self.stop(ticker);
        assert_eq!(self.count("workspace.create_command"), 0, "{:?}", self.requests());
        assert!(!worktree.exists(), "{}", worktree.display());
        self.assert_only_observed(&before);
        assert!(self.state().approvals.iter().any(|a| a.reference.id == approval && a.consumed.is_none()));
        assert_eq!(self.attempt(attempt).state, AttemptState::Reserved);
    }
}

/// Replaces `draft_preflight_rejects_closed_capacity_without_writing_approval_or_task_state`.
#[test]
fn draft_is_refused_while_the_scheduler_admits_no_workers() {
    let mut lab = Lab::new("unknown_usage='allow_with_warning'");
    lab.prepare_profile();
    let selection = lab.selection("Retained instructions");
    let policy = lab.state().scheduler.unwrap().policy.revision.to_string();
    lab.ok(&["scheduler", "demo", "policy", "--max-active-workers", "0", "--max-attempts-per-task", "3", "--expected-revision", &policy, "--expected-head", &lab.head().to_string()]);
    let error = lab.refused(&["launch", "demo", "draft", "--selection", selection.to_str().unwrap(), "--expected-head", &lab.head().to_string()]);
    assert!(error.contains("project worker capacity is full"), "{error}");
    let state = lab.state();
    assert!(state.approvals.is_empty() && state.attempts.is_empty() && state.operations.is_empty());
    assert_eq!(state.tasks[0].active_attempt, None);
}

/// Replaces `worktree_route_maps_root_and_subdirectory_and_refuses_foreign_sources`.
#[test]
fn a_subdirectory_binding_runs_in_the_same_subdirectory_of_the_new_worktree() {
    let mut lab = Lab::bound("unknown_usage='allow_with_warning'", |repo| repo.join("subdir"));
    fs::create_dir(lab.repo.join("subdir")).unwrap();
    fs::write(lab.repo.join("subdir/file"), "tracked\n").unwrap();
    lab.git(&["add", "subdir/file"]);
    lab.git(&["commit", "-q", "-m", "subdir"]);
    let (_, attempt) = lab.reserve("Retained instructions");
    let worktree = lab.planned_worktree(&attempt);
    lab.serve();
    let mut ticker = lab.spawn();
    lab.wait(&mut ticker, 60, &|| lab.count("workspace.create_command") == 1);
    lab.stop(ticker);
    let creation = lab.requests().into_iter().find(|(m, _)| m == "workspace.create_command").unwrap().1;
    assert_eq!(creation["cwd"].as_str(), worktree.join("subdir").to_str(), "{creation}");
    assert_eq!(fs::read_to_string(worktree.join("subdir/file")).unwrap(), "tracked\n");

    // A binding outside every selected repository cannot even be drafted.
    let mut foreign = Lab::bound("unknown_usage='allow_with_warning'", |repo| repo.parent().unwrap().join("lab"));
    foreign.prepare_profile();
    let selection = foreign.selection("Retained instructions");
    let error = foreign.refused(&["launch", "demo", "draft", "--selection", selection.to_str().unwrap(), "--expected-head", &foreign.head().to_string()]);
    assert!(error.contains("working directory is outside the approved repositories"), "{error}");
}

/// Replaces `source_only_working_directory_is_refused_before_approval_consumption`.
#[test]
fn an_untracked_working_directory_is_refused_before_the_approval_is_used() {
    let mut lab = Lab::bound("unknown_usage='allow_with_warning'", |repo| repo.join("untracked-dir"));
    fs::create_dir(lab.repo.join("untracked-dir")).unwrap();
    let (approval, attempt) = lab.reserve("Retained instructions");
    lab.assert_preparation_refused(&approval, &attempt, "working directory is absent from the approved repository tree");
}

/// Replaces `legacy_worktree_reference_blocks_new_creation_before_consuming_approval`.
#[test]
fn a_legacy_thread_holding_the_planned_worktree_blocks_its_creation() {
    let mut lab = Lab::new("unknown_usage='allow_with_warning'");
    let (approval, attempt) = lab.reserve("Retained instructions");
    let neighbor = lab.path("root/legacy-neighbor");
    for dir in [".state", "threads"] { fs::create_dir_all(neighbor.join(dir)).unwrap(); }
    fs::write(neighbor.join("PROJECT.md"), "legacy fixture").unwrap();
    fs::write(neighbor.join("threads/t-0001.toml"), format!("id='t-0001'\nworktree_path={}\n", json!(lab.planned_worktree(&attempt)))).unwrap();
    lab.assert_preparation_refused(&approval, &attempt, "worktree path is referenced");
}

/// Replaces `worker_limits_and_literal_vector_are_explicit` with
/// `ticker_launches_and_briefs_once_then_stops_a_cancelled_worker_while_paused_and_revoked`,
/// which checks the worker's argument vector.
///
/// A worker profile whose wall deadline is below one second or above seven
/// days cannot be prepared.
#[test]
fn a_worker_wall_deadline_outside_one_second_to_seven_days_is_refused() {
    let lab = Lab::new("unknown_usage='allow_with_warning'");
    let config = lab.path(".config/herdr-projects/config.toml");
    let original = fs::read_to_string(&config).unwrap();
    for (wall, reason) in [("0", "budget limits must be positive bounded integers"), ("604801", "worker wall deadline exceeds supported bounds")] {
        fs::write(&config, original.replace("max_wall_seconds=600", &format!("max_wall_seconds={wall}"))).unwrap();
        let error = lab.refused(&["profile", "prepare", "demo", "worker", "--herdr-executable", lab.herdr.to_str().unwrap(),
            "--agent-executable", lab.path("bin/claude").to_str().unwrap(), "--execution-home", lab.path("agent-home").to_str().unwrap()]);
        assert!(error.contains(reason), "{wall}: {error}");
    }
    fs::write(&config, original.replace("max_wall_seconds=600", "max_wall_seconds=604800")).unwrap();
    lab.ok(&["profile", "prepare", "demo", "worker", "--herdr-executable", lab.herdr.to_str().unwrap(),
        "--agent-executable", lab.path("bin/claude").to_str().unwrap(), "--execution-home", lab.path("agent-home").to_str().unwrap()]);
}

/// A worker whose end is proven (here: a cancelled attempt the ticker stops
/// while the project is active) leaves the project admitted after its pane
/// closes: the ownership claim is retired with an audit event and the next
/// task can be drafted. A pane that vanishes under a live worker, with no
/// termination evidence, still pauses the project and keeps the claim.
#[test]
fn a_proven_worker_end_keeps_the_project_admitted_but_an_unexplained_pane_loss_pauses_it() {
    let mut lab = Lab::new("unknown_usage='allow_with_warning'");
    // A second queued task on its own resource-free binding.
    lab.ok(&["task", "demo", "add", "next", "--title", "next", "--expected-head", &lab.head().to_string()]);
    let request = lab.path("queue.json");
    lab.ok(&["task", "demo", "queue", "next", "--input-file", request.to_str().unwrap(), "--expected-revision", "1", "--expected-head", &lab.head().to_string()]);
    let id = TaskId::new("next").unwrap();
    let revision = lab.state().tasks.into_iter().find(|t| t.id == id).unwrap().revision;
    let route = RuntimeRoute { socket: lab.socket().display().to_string(), cwd: lab.repo.canonicalize().unwrap().display().to_string(), ..Default::default() };
    let next = runtime::create_binding(&lab.project, Some(&id), Some(revision), lab.head(), &route).unwrap().binding.id;
    lab.resume();
    let (_, attempt) = lab.reserve("Retained instructions");
    lab.serve();
    let mut ticker = lab.spawn();
    lab.wait(&mut ticker, 120, &|| lab.attempt(&attempt).state == AttemptState::Running);
    lab.stop(ticker);
    let running = lab.attempt(&attempt);
    lab.ok(&["task", "demo", "cancel-attempt", attempt.as_str(), "--expected-revision", &running.revision.to_string(), "--expected-head", &lab.head().to_string(), "--reason", "operator stop"]);
    let mut ticker = lab.spawn();
    lab.wait(&mut ticker, 60, &|| lab.attempt(&attempt).termination_observed);
    lab.stop(ticker);
    // Herdr closes the ended worker's pane; a pass observes its absence and
    // later passes no longer track the retired binding. The observation a
    // pass admits commits after the pass, so wait for each commit. A pass
    // whose 100 ms observation-head read overruns (under load) offers none.
    fs::write(lab.path("lab/vanish"), b"").unwrap();
    lab.run_until(4, &|| !lab.events("runtime.relinquished").is_empty());
    let observed = lab.events("runtime.observed").len();
    lab.run_until(4, &|| lab.events("runtime.observed").len() > observed);
    let state = lab.state();
    let control = state.control.clone().unwrap();
    assert_eq!((control.state, control.reconciliation_required), (ProjectState::Active, false), "{:?}", lab.events("project.reconciliation_invalidated"));
    assert!(state.ownership.iter().all(|o| o.binding != lab.binding), "{:?}", state.ownership);
    let retired = lab.events("runtime.relinquished");
    assert_eq!(retired.len(), 1);
    assert_eq!((retired[0].entity.as_str(), &retired[0].payload["termination"]["attempt"]), (lab.binding.as_str(), &json!(attempt.as_str())));
    // The binding and its resource references are retained.
    assert_eq!(state.runtime_bindings.iter().find(|b| b.id == lab.binding).unwrap().identity.pane_id, "w1:p1");
    let selection = lab.selection_for("next", &next, "Next instructions");
    lab.ok(&["launch", "demo", "draft", "--selection", selection.to_str().unwrap(), "--expected-head", &lab.head().to_string()]);

    // Without termination evidence the same disappearance is unexplained.
    let mut lab = Lab::new("unknown_usage='allow_with_warning'");
    let (_, attempt) = lab.reserve("Retained instructions");
    lab.serve();
    let mut ticker = lab.spawn();
    lab.wait(&mut ticker, 120, &|| lab.attempt(&attempt).state == AttemptState::Running);
    lab.stop(ticker);
    fs::write(lab.path("lab/vanish"), b"").unwrap();
    lab.run_for("pane.list", 2);
    let state = lab.state();
    let control = state.control.clone().unwrap();
    assert_eq!((control.state, control.reconciliation_required), (ProjectState::Paused, true));
    assert!(state.ownership.iter().any(|o| o.binding == lab.binding && o.attempt.as_ref() == Some(&attempt)));
    assert!(lab.events("runtime.relinquished").is_empty());
    let live = lab.attempt(&attempt);
    assert!(live.retains_capacity() && !live.termination_observed);
}

/// Another holder of the shared root barrier (a store service, a CLI command)
/// comes and goes while a worker launches. Launch and brief take the exclusive
/// root afresh at each durable stage; each waits out the holder instead of
/// failing the job and backing off, and while such an effect is admitted the
/// ticker's own store services stay off the root. The worker still reaches
/// Running, and no job or service lost the root to the contention.
#[test]
fn a_launch_reaches_running_while_another_holder_takes_the_shared_root_intermittently() {
    use std::sync::{Arc, atomic::{AtomicBool, AtomicUsize, Ordering}};
    let mut lab = Lab::new("unknown_usage='allow_with_warning'");
    let (_, attempt) = lab.reserve("Retained instructions");
    lab.serve();
    // Hold the root shared for 300 ms of every 400 ms, as a busy neighbour would.
    let (stop, held) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicUsize::new(0)));
    let holder = {
        let (lock, stop, held) = (lab.path("root/.execution.lock"), stop.clone(), held.clone());
        std::thread::spawn(move || while !stop.load(Ordering::SeqCst) {
            let file = fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&lock).unwrap();
            if file.try_lock_shared().is_ok() {
                held.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(300));
                file.unlock().unwrap();
            }
            std::thread::sleep(Duration::from_millis(100));
        })
    };
    let mut ticker = lab.spawn();
    lab.wait(&mut ticker, 120, &|| lab.attempt(&attempt).state == AttemptState::Running);
    lab.stop(ticker);
    stop.store(true, Ordering::SeqCst);
    holder.join().unwrap();
    assert!(held.load(Ordering::SeqCst) >= 5, "the holder took the root {} times", held.load(Ordering::SeqCst));
    let log = fs::read_to_string(lab.path("root/.ticker.log")).unwrap();
    assert!(!log.contains(".execution.lock; retry"), "an effect lost the root: {log}");
    assert!(!log.contains("root maintenance or exclusive external operation is active"), "a service contended with an admitted effect: {log}");
}

// ---- Review launch (docs/telemetry/contracts-review.md §11, card D9) ----

/// The reviewed work's identities, planted so a leak into a reviewer's brief shows.
const AUTHOR_ATTEMPT: &str = "author-sentinel-attempt-0001";
const AUTHOR_TITLE: &str = "AUTHOR-TITLE-SENTINEL";
const PROJECT_SENTINEL: &str = "PROJECT-INSTRUCTIONS-SENTINEL";
const AUTHOR_CONFIGURATION_JSON: &str = r#"{"kind":"codex","schema":"agent_configuration.v1","sentinel":"AUTHOR-CONFIGURATION-SENTINEL"}"#;

/// A review world on the lab: task `authored` (title `AUTHOR_TITLE`) with an
/// owner-signed contract, whose attempt `AUTHOR_ATTEMPT` (dispatched as a
/// `codex` configuration) submitted candidate commit C on branch `author`;
/// review opportunity O (`code`, `review-protocol.v1`, budget 600000 ms,
/// prior finding `finding:prior-a`) blindly assigned to the lab's `worker`
/// profile; and the queued task `work` turned into O's review task by a
/// worker snapshot built with `--review-opportunity`. PROJECT.md names the
/// author. Nothing is reserved yet.
struct ReviewWorld { opportunity: String, submission: String, candidate: String, base: String, repository: String, store: String,
    author_configuration: String, author_profile_digest: String, snapshot: Value, selection: PathBuf }

impl Lab {
    fn review_world(&mut self) -> ReviewWorld {
        self.prepare_profile();
        let head = self.head().to_string();
        self.ok(&["task", "demo", "add", "authored", "--title", AUTHOR_TITLE, "--expected-head", &head]);
        let base = self.git(&["rev-parse", "HEAD"]);
        self.git(&["checkout", "-qb", "author"]);
        fs::write(self.repo.join("lib.rs"), "pub fn answer() -> u32 { 42 }\n").unwrap();
        self.git(&["add", "."]);
        self.git(&["commit", "-qm", "candidate"]);
        let candidate = self.git(&["rev-parse", "HEAD"]);
        self.git(&["checkout", "-q", "-"]);
        let repository = self.repo.canonicalize().unwrap().display().to_string();
        let store = self.project.join(".state/state.db").canonicalize().unwrap().display().to_string();
        let mut document = serde_json::to_vec_pretty(&json!({
            "version": 3, "outputs": [{"path": "lib.rs", "kind": "git_file"}], "scope": {"paths": [{"path": "lib.rs", "access": "write"}]},
            "project_store": store, "expected_head": self.head(), "task_id": "authored", "contract_revision": 1, "deliverable": "answer", "non_goals": "none",
            "acceptance_policies": [{"id": "clean", "text": r#"{"version":1,"checks":["/usr/bin/git","diff","--quiet"]}"#}], "repository": repository, "base_oid": base,
            "object_format": "sha256", "dependencies": [], "capability_flags": [], "profile_kind": "codex", "retry_class": "none", "result_schema_id": "result-v1",
            "route": "verify_only", "authority": authority::policy_reference(&self.project).unwrap()})).unwrap();
        document.push(b'\n');
        let contract = self.path("authored-contract.json");
        fs::write(&contract, &document).unwrap();
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-Y", "sign", "-f"]).arg(&self.key).args(["-n", authority::CONTRACT_SIGNATURE_NAMESPACE]).arg(&contract).output().unwrap().status.success());
        let installed = self.ok(&["task", "demo", "contract", "put", "--input-file", contract.to_str().unwrap(), "--signature", self.path("authored-contract.json.sig").to_str().unwrap()]);
        // The author attempt ran and ended; its dispatch chose a `codex` configuration.
        let author_configuration = format!("sha256:{:x}", Sha256::digest(AUTHOR_CONFIGURATION_JSON.as_bytes()));
        let author_profile_digest = "e".repeat(64);
        let db = rusqlite::Connection::open(self.project.join(".state/state.db")).unwrap();
        db.execute("INSERT INTO attempts(id,task_id,revision,state,snapshot,reservation,termination_observed) VALUES(?1,'authored',1,'completed',NULL,?1,1)", [AUTHOR_ATTEMPT]).unwrap();
        db.execute("INSERT INTO agent_configurations VALUES(?1,?2,1)", rusqlite::params![author_configuration, AUTHOR_CONFIGURATION_JSON]).unwrap();
        let eligible = json!([{"configuration_id": author_configuration, "probability_ppm": 1_000_000, "profile_digest": author_profile_digest, "status": "chosen"}]).to_string();
        db.execute("INSERT INTO dispatch_decisions(attempt_id,task_id,task_revision,contract_revision,chosen_configuration_id,eligible,chooser_kind,chooser_principal,reason_codes,decided_unix_ms)
            VALUES(?1,'authored',1,1,?2,?3,'operator','operator:cli','[\"unspecified\"]',1)", rusqlite::params![AUTHOR_ATTEMPT, author_configuration, eligible]).unwrap();
        drop(db);
        let objects: Vec<Value> = self.git(&["rev-list", "--objects", "--all"]).lines()
            .map(|line| { let oid = line.split_whitespace().next().unwrap(); json!({"oid": oid, "relative_path": format!("{}/{}", &oid[..2], &oid[2..])}) }).collect();
        let result = self.path("authored-result.json");
        fs::write(&result, json!({"idempotency_key": "authored-key", "task_id": "authored", "contract_revision": 1, "contract_digest": installed["digest"],
            "attempt_id": AUTHOR_ATTEMPT, "repository": repository, "base_oid": base, "candidate_oid": candidate, "object_format": "sha256",
            "artifact_manifest": [{"path": "lib.rs", "oid": candidate}], "claimed_checks": [], "objects": objects}).to_string()).unwrap();
        let submission = self.ok(&["result", "demo", "submit", "--input-file", result.to_str().unwrap()])["submission_id"].as_str().unwrap().to_owned();
        let opportunity = self.ok(&["telemetry", "demo", "review", "open", &submission, "--protocol", "review-protocol.v1", "--budget-ms", "600000",
            "--prior-finding", "finding:prior-a"])["opportunity"]["opportunity_id"].as_str().unwrap().to_owned();
        let assignment = self.ok(&["telemetry", "demo", "review", "assign", &opportunity, "--blind", "--candidate", "worker"])["assignment"].clone();
        assert_eq!((&assignment["author_attempt_id"], &assignment["reason"], &assignment["same_family"], &assignment["blind"]),
            (&json!(AUTHOR_ATTEMPT), &json!("cross_provider"), &json!(false), &json!(true)));
        // The review task's snapshot: the blind brief replaces PROJECT.md, which names the author.
        fs::write(self.project.join("PROJECT.md"), format!("{PROJECT_SENTINEL} written by {AUTHOR_ATTEMPT}")).unwrap();
        let scope = self.path("review-scope.json");
        fs::write(&scope, json!({"schema_version":1,"task_id":"work","profile":"worker","domains":[],"paths":[],"pinned_keys":[],"sensitivity":"default"}).to_string()).unwrap();
        let snapshot = self.ok(&["memory", "demo", "snapshot", "--task", "work", "--profile", "worker", "--input-file", scope.to_str().unwrap(), "--worker", "--review-opportunity", &opportunity]);
        let selection = self.path("review-selection.json");
        fs::write(&selection, json!({"task":"work","binding":self.binding,"profile":self.profile,
            "knowledge":{"id":snapshot["id"],"revision":1,"digest":snapshot["manifest_hash"]},"repositories":[self.repo.canonicalize().unwrap()]}).to_string()).unwrap();
        ReviewWorld { opportunity, submission, candidate, base, repository, store, author_configuration, author_profile_digest, snapshot, selection }
    }
    /// Draft, owner-sign, import and reserve `selection`; returns (draft, attempt).
    fn reserve_selection(&self, selection: &std::path::Path) -> (Value, AttemptId) {
        let drafted = self.ok(&["launch", "demo", "draft", "--selection", selection.to_str().unwrap(), "--expected-head", &self.head().to_string()]);
        let document = self.path("review-approval.json");
        fs::write(&document, serde_json::to_vec_pretty(&drafted["approval"]).unwrap()).unwrap();
        let _ = fs::remove_file(self.path("review-approval.json.sig"));
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-Y", "sign", "-f"]).arg(&self.key).args(["-n", authority::SIGNATURE_NAMESPACE]).arg(&document).output().unwrap().status.success());
        let approval = self.ok(&["approval", "demo", "import", document.to_str().unwrap(), self.path("review-approval.json.sig").to_str().unwrap(), "--expected-head", &self.head().to_string()]);
        let reservation = self.ok(&["launch", "demo", "reserve", "--selection", selection.to_str().unwrap(), "--approval-digest", approval["digest"].as_str().unwrap(), "--expected-head", &self.head().to_string()]);
        (drafted, AttemptId::new(reservation["record"]["attempt"].as_str().unwrap()).unwrap())
    }
    /// The CLI as the reviewing worker runs it: `HOME` is the profile's execution home.
    fn worker_cli(&self, args: &[&str]) -> Output {
        Command::new(BIN).env_clear().env("HOME", self.path("agent-home")).env("PATH", "/usr/bin:/bin")
            .args(["--root", self.path("root").to_str().unwrap()]).args(args).output().unwrap()
    }
    fn review_show(&self, as_of: Option<i64>) -> Value {
        let mut args = vec!["telemetry".to_owned(), "demo".into(), "review".into(), "show".into()];
        if let Some(seq) = as_of { args.extend(["--as-of".into(), seq.to_string()]); }
        self.ok(&args.iter().map(String::as_str).collect::<Vec<_>>())
    }
}

/// The retained instructions inside a rendered worker prompt.
fn instructions(prompt: &str) -> &str {
    let start = prompt.find("# Project instructions\n\n").unwrap() + "# Project instructions\n\n".len();
    &prompt[start..start + prompt[start..].find("\n\n# Task\n\n").unwrap()]
}

/// O is assigned blindly to `worker`; task `work` becomes its review task
/// through `memory snapshot --review-opportunity O`, whose retained
/// instructions are the blind brief (bound with its digest, prior findings
/// withheld: the protocol is unregistered). A plain PROJECT.md snapshot of
/// the same task cannot launch it. The ordinary `launch draft`/approval/
/// `launch reserve` path reserves the review attempt, and that reservation
/// records the session: recorder `service:launch`, the assigned
/// configuration, ledger `started` at seq 1. The ticker then launches and
/// briefs the worker on the Herdr stand-in; the delivered prompt is the
/// drafted brief, names O's exact candidate, and carries none of the planted
/// author identities (attempt, configuration, profile digest, the authored
/// task's title, PROJECT.md).
#[test]
fn review_assignment_launches_with_blind_brief_and_records_session() {
    let mut lab = Lab::new("unknown_usage='allow_with_warning'");
    let world = lab.review_world();
    let bound = &world.snapshot["review_brief"];
    assert_eq!((&bound["opportunity_id"], &bound["task_id"], &bound["snapshot_id"], &bound["prior_disclosure"], &bound["principal"], &bound["replayed"]),
        (&json!(world.opportunity), &json!("work"), &world.snapshot["id"], &json!("withheld"), &json!("operator:cli"), &json!(false)));

    // A snapshot of the review task that is not its blind brief never launches it.
    let plain = lab.path("plain-scope.json");
    fs::write(&plain, json!({"schema_version":1,"task_id":"work","profile":"worker","domains":[],"paths":[],"pinned_keys":[],"sensitivity":"default"}).to_string()).unwrap();
    let project_snapshot = lab.ok(&["memory", "demo", "snapshot", "--task", "work", "--profile", "worker", "--input-file", plain.to_str().unwrap(), "--worker"]);
    let wrong = lab.path("plain-selection.json");
    fs::write(&wrong, json!({"task":"work","binding":lab.binding,"profile":lab.profile,
        "knowledge":{"id":project_snapshot["id"],"revision":1,"digest":project_snapshot["manifest_hash"]},"repositories":[lab.repo.canonicalize().unwrap()]}).to_string()).unwrap();
    let refused = lab.refused(&["launch", "demo", "draft", "--selection", wrong.to_str().unwrap(), "--expected-head", &lab.head().to_string()]);
    assert!(refused.contains("a review task launches only with its blind review brief snapshot"), "{refused}");

    let (drafted, attempt) = lab.reserve_selection(&world.selection);
    let shown = lab.review_show(None);
    let session = shown["opportunities"][0]["sessions"][0].clone();
    assert_eq!(shown["opportunities"][0]["status"], json!("in_progress"));
    let configuration = shown["opportunities"][0]["assignment"]["reviewer_configuration_id"].clone();
    assert_eq!((&session["ordinal"], &session["attempt_id"], &session["recorder_principal"], &session["configuration_id"], &session["matches_assignment"], &session["same_attempt_as_author"], &session["completion"]),
        (&json!(1), &json!(attempt.as_str()), &json!("service:launch"), &configuration, &json!(true), &json!(false), &Value::Null));
    // The start is the first row of the shared ledger, in the reservation's transaction.
    assert_eq!(lab.review_show(Some(0))["opportunities"][0]["sessions"], json!([]));
    assert_eq!(lab.review_show(Some(1))["opportunities"][0]["sessions"][0]["session_id"], session["session_id"]);
    let db = rusqlite::Connection::open(lab.project.join(".state/state.db")).unwrap();
    let launched: (String, String) = db.query_row("SELECT l.attempt_id,l.snapshot_id FROM review_session_launches l WHERE l.session_id=?1", [session["session_id"].as_str().unwrap()], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!((launched.0.as_str(), launched.1.as_str()), (attempt.as_str(), world.snapshot["id"].as_str().unwrap()));
    drop(db);

    lab.serve();
    let briefed = || { let s = lab.state(); s.operations.iter().any(|o| o.kind == "runtime.worker_brief" && s.deliveries.iter().any(|d| d.operation == o.id && d.state == DeliveryState::Confirmed)) };
    let mut ticker = lab.spawn();
    lab.wait(&mut ticker, 120, &briefed);
    lab.stop(ticker);
    assert_eq!((lab.count("workspace.create_command"), lab.count("agent.prompt")), (1, 1));
    let text = lab.requests().into_iter().find(|(m, _)| m == "agent.prompt").unwrap().1["text"].as_str().unwrap().to_owned();
    assert_eq!(text, drafted["brief"]["text"].as_str().unwrap(), "the worker receives exactly the drafted brief");
    let brief = instructions(&text);
    assert_eq!(bound["brief_digest"], json!(format!("sha256:{:x}", Sha256::digest(brief.as_bytes()))));
    assert!(brief.starts_with("# Blind review (review_brief.v1)\n\n"), "{brief}");
    let view: Value = serde_json::from_str(&brief[brief.find("{\n").unwrap()..=brief.find("\n}").unwrap() + 1]).unwrap();
    assert_eq!(view, json!({"opportunity_id": world.opportunity, "task_id": "authored", "contract_revision": 1, "repository": world.repository,
        "base_oid": world.base, "candidate_oid": world.candidate, "object_format": "sha256", "scope": "candidate_diff", "kind": "code",
        "protocol": "review-protocol.v1", "budget_ms": 600000}));
    assert!(text.contains(&format!("Attempt: {}", attempt.as_str())) && text.contains("review submit --input-file FILE"), "{text}");
    for sentinel in [AUTHOR_ATTEMPT, AUTHOR_TITLE, PROJECT_SENTINEL, "AUTHOR-CONFIGURATION-SENTINEL", world.author_configuration.as_str(),
        world.author_profile_digest.as_str(), "finding:prior-a", world.submission.as_str()] {
        assert!(!text.contains(sentinel), "the brief names {sentinel}");
    }
}

/// Write `receipt` beside the lab and return its path.
fn receipt_file(lab: &Lab, name: &str, receipt: &Value) -> String {
    let path = lab.path(name);
    fs::write(&path, receipt.to_string()).unwrap();
    path.display().to_string()
}

/// The reviewing worker (HOME = its execution home) finds its launched
/// session and submits its `review_receipt.v1` through `review submit`: a
/// proposal (`trust: proposal`, declared coverage) recorded as
/// `worker:<attempt>`, with its finding a pending submission; the same
/// receipt replays and another is refused. A receipt carrying `accepted` is
/// refused. In that context `review accept`, `review accept draft`, `review
/// complete` and finding triage are refused, writing nothing; the session
/// stays undecided.
#[test]
fn reviewer_worker_submits_proposal_receipt_but_cannot_accept() {
    let mut lab = Lab::new("unknown_usage='allow_with_warning'");
    let world = lab.review_world();
    let (_, attempt) = lab.reserve_selection(&world.selection);
    let worker = |args: &[&str]| -> Result<Value, String> {
        let out = lab.worker_cli(args);
        if out.status.success() { Ok(serde_json::from_slice(&out.stdout).unwrap()) } else { Err(String::from_utf8_lossy(&out.stderr).into_owned()) }
    };
    let mine = worker(&["telemetry", "demo", "review", "session", "--attempt", attempt.as_str()]).unwrap()["session"].clone();
    let session = mine["session_id"].as_str().unwrap().to_owned();
    assert_eq!(mine, json!({"session_id": session, "opportunity_id": world.opportunity, "ordinal": 1, "submission_id": world.submission,
        "candidate_oid": world.candidate, "completed": false, "receipt_schema": "review_receipt.v1"}));

    let mut receipt = json!({"schema": "review_receipt.v1", "session_id": session, "submission_id": world.submission, "candidate_oid": world.candidate,
        "outcome": "completed", "findings": [{"ref": "finding:w1", "title": "Answer is hard-coded"}], "evidence": [format!("sha256:{}", "c".repeat(64))]});
    let mut claimed = receipt.clone();
    claimed["accepted"] = json!(true);
    let err = worker(&["telemetry", "demo", "review", "submit", "--input-file", &receipt_file(&lab, "claimed.json", &claimed)]).unwrap_err();
    assert!(err.contains("unknown field `accepted`"), "{err}");
    let path = receipt_file(&lab, "receipt.json", &receipt);
    let done = worker(&["telemetry", "demo", "review", "submit", "--input-file", &path]).unwrap()["completion"].clone();
    assert_eq!((&done["outcome"], &done["trust"], &done["coverage_basis"], &done["recorder_principal"], &done["findings_submitted"], &done["replayed"]),
        (&json!("completed"), &json!("proposal"), &json!("declared"), &json!(format!("worker:{}", attempt.as_str())), &json!(1), &json!(false)));
    assert_eq!(worker(&["telemetry", "demo", "review", "submit", "--input-file", &path]).unwrap()["completion"]["replayed"], json!(true));
    receipt["findings"] = json!([]);
    let err = worker(&["telemetry", "demo", "review", "submit", "--input-file", &receipt_file(&lab, "other.json", &receipt)]).unwrap_err();
    assert!(err.contains("already has a different completion"), "{err}");

    // The worker decides nothing: every deciding or owner command refuses its context.
    let before = rusqlite::Connection::open(lab.project.join(".state/state.db")).unwrap()
        .query_row("SELECT (SELECT count(*) FROM review_acceptances)+(SELECT count(*) FROM finding_decisions)+(SELECT count(*) FROM review_log)", [], |r| r.get::<_, i64>(0)).unwrap();
    for args in [vec!["review", "accept", session.as_str(), "--document", path.as_str(), "--signature", path.as_str()],
        vec!["review", "accept", "draft", session.as_str(), "--grant", "sha256:0", "--output", "/dev/null"],
        vec!["review", "complete", "--input-file", path.as_str()],
        vec!["review", "findings", "reject", "1", "--reason", "out_of_scope"]] {
        let mut all = vec!["telemetry", "demo"];
        all.extend(args);
        let err = worker(&all).unwrap_err();
        assert!(err.contains("refuses to run inside a worker execution context: HOME is a worker execution home"), "{all:?}: {err}");
    }
    let after = rusqlite::Connection::open(lab.project.join(".state/state.db")).unwrap()
        .query_row("SELECT (SELECT count(*) FROM review_acceptances)+(SELECT count(*) FROM finding_decisions)+(SELECT count(*) FROM review_log)", [], |r| r.get::<_, i64>(0)).unwrap();
    assert_eq!(after, before, "refused commands write nothing");
    let completion = lab.review_show(None)["opportunities"][0]["sessions"][0]["completion"].clone();
    assert_eq!((&completion["trust"], &completion["acceptance"], &completion["recorder_principal"]),
        (&json!("proposal"), &Value::Null, &json!(format!("worker:{}", attempt.as_str()))));
    let findings = lab.ok(&["telemetry", "demo", "review", "findings", "show"])["findings"]["submissions"].clone();
    assert_eq!((&findings[0]["finding_ref"], &findings[0]["outcome"], &findings[0]["title"]), (&json!("finding:w1"), &json!("pending"), &json!("Answer is hard-coded")));
}

/// Owner-signed grant G makes `reviewer:carol` a delegated reviewer of task
/// `authored`. After the launched session (seq 1) and the worker's receipt
/// (completion seq 2, its finding seq 3), `review accept draft` writes the
/// exact canonical request, byte for byte; carol signs it offline with her
/// own key and `review accept` records the decision at seq 4 of the shared
/// ledger. `review show --as-of` places it: undecided at 3, accepted from 4
/// (`ledger_seq` 4); the owner's later triage takes seq 5, after it.
#[test]
fn acceptance_decision_replays_in_the_ledger_as_of() {
    let mut lab = Lab::new("unknown_usage='allow_with_warning'");
    let world = lab.review_world();
    let (_, attempt) = lab.reserve_selection(&world.selection);
    let session = lab.review_show(None)["opportunities"][0]["sessions"][0]["session_id"].as_str().unwrap().to_owned();
    let receipt = json!({"schema": "review_receipt.v1", "session_id": session, "submission_id": world.submission, "candidate_oid": world.candidate,
        "outcome": "completed", "findings": ["finding:w1"], "evidence": []});
    let out = lab.worker_cli(&["telemetry", "demo", "review", "submit", "--input-file", &receipt_file(&lab, "receipt.json", &receipt)]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let receipt_digest = serde_json::from_slice::<Value>(&out.stdout).unwrap()["completion"]["receipt_digest"].as_str().unwrap().to_owned();
    assert_eq!(lab.ok(&["telemetry", "demo", "review", "findings", "show"])["findings"]["head_seq"], json!(3));

    let carol = lab.path("carol");
    assert!(Command::new("/usr/bin/ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&carol).output().unwrap().status.success());
    let carol_public = fs::read_to_string(carol.with_extension("pub")).unwrap().split_whitespace().take(2).collect::<Vec<_>>().join(" ");
    let now = jiff::Timestamp::now().as_millisecond();
    let grant = json!({"schema": "code_review_authority.v1", "scope": "code_review", "issuer": "owner", "subject": "reviewer:carol",
        "subject_public_key": carol_public, "subject_configurations": [], "project_store": world.store, "repositories": [world.repository],
        "tasks": [{"task_id": "authored", "contract_revision": 1}], "kinds": ["code"], "review_configurations": [], "actions": ["accept_review_completion"],
        "max_decisions": 1, "valid_from_unix_ms": now - 60_000, "expires_unix_ms": now + 3_600_000,
        "prohibited_effects": ["alter_requirements", "approve_author_attempt", "approve_own_work", "child_delegation", "increase_permissions"],
        "authority": authority::policy_reference(&lab.project).unwrap()});
    let grant_file = lab.path("grant.json");
    fs::write(&grant_file, serde_json::to_vec_pretty(&grant).unwrap()).unwrap();
    assert!(Command::new("/usr/bin/ssh-keygen").args(["-Y", "sign", "-f"]).arg(&lab.key).args(["-n", "code-review-authority@herdr-projects"]).arg(&grant_file).output().unwrap().status.success());
    let grant_id = lab.ok(&["telemetry", "demo", "review", "authority", "import", grant_file.to_str().unwrap(), lab.path("grant.json.sig").to_str().unwrap()])["grant"]["grant_id"].as_str().unwrap().to_owned();

    // The product drafts the exact bytes; it holds no reviewer key and signs nothing.
    let request = lab.path("request.json");
    let drafted = lab.ok(&["telemetry", "demo", "review", "accept", "draft", &session, "--grant", &grant_id, "--output", request.to_str().unwrap()])["draft"].clone();
    let expected = format!(r#"{{"decision":"accepted","grant_id":"{grant_id}","project_store":"{}","reason":null,"receipt_digest":"{receipt_digest}","schema":"review_acceptance.v1","session_id":"{session}","subject":"reviewer:carol"}}"#, world.store);
    assert_eq!(fs::read_to_string(&request).unwrap(), expected);
    assert_eq!((&drafted["request_digest"], &drafted["signer"], &drafted["namespace"]),
        (&json!(format!("sha256:{:x}", Sha256::digest(expected.as_bytes()))), &json!("reviewer:carol"), &json!("review-acceptance@herdr-projects")));
    assert!(!lab.cli(&["telemetry", "demo", "review", "accept", "draft", &session, "--grant", &grant_id, "--output", request.to_str().unwrap()]).status.success(), "the draft never overwrites");
    assert_eq!(lab.ok(&["telemetry", "demo", "review", "findings", "show"])["findings"]["head_seq"], json!(3), "a draft writes nothing");

    assert!(Command::new("/usr/bin/ssh-keygen").args(["-Y", "sign", "-f"]).arg(&carol).args(["-n", "review-acceptance@herdr-projects"]).arg(&request).output().unwrap().status.success());
    let accepted = lab.ok(&["telemetry", "demo", "review", "accept", &session, "--document", request.to_str().unwrap(), "--signature", lab.path("request.json.sig").to_str().unwrap()])["acceptance"].clone();
    assert_eq!((&accepted["decision"], &accepted["authority_principal"], &accepted["ledger_seq"], &accepted["receipt_digest"], &accepted["replayed"]),
        (&json!("accepted"), &json!("reviewer:carol"), &json!(4), &json!(receipt_digest), &json!(false)));

    let at = |seq: i64| lab.review_show(Some(seq));
    let s1 = at(1);
    assert_eq!((&s1["head_seq"], &s1["as_of_seq"], &s1["opportunities"][0]["status"], &s1["opportunities"][0]["sessions"][0]["attempt_id"], &s1["opportunities"][0]["sessions"][0]["completion"]),
        (&json!(4), &json!(1), &json!("in_progress"), &json!(attempt.as_str()), &Value::Null));
    let s3 = at(3)["opportunities"][0]["sessions"][0]["completion"].clone();
    assert_eq!((&s3["outcome"], &s3["acceptance"]), (&json!("completed"), &Value::Null));
    let s4 = at(4)["opportunities"][0]["sessions"][0]["completion"]["acceptance"].clone();
    assert_eq!((&s4["decision"], &s4["grant_id"], &s4["ledger_seq"], &s4["authority"]), (&json!("accepted"), &json!(grant_id), &json!(4), &json!("delegated_code_review.v1")));
    let err = String::from_utf8_lossy(&lab.cli(&["telemetry", "demo", "review", "show", "--as-of", "5"]).stderr).into_owned();
    assert!(err.contains("as-of seq 5 is outside the review history 0..=4"), "{err}");

    // The owner's triage follows the decision in the one ordering.
    let claim = lab.ok(&["telemetry", "demo", "review", "findings", "show"])["findings"]["submissions"][0]["claims"][0]["claim_id"].as_i64().unwrap();
    let event = lab.ok(&["telemetry", "demo", "review", "findings", "reject", &claim.to_string(), "--reason", "intended_behavior"])["event"].clone();
    assert_eq!(event["seq"], json!(5));
    assert_eq!(at(4)["opportunities"][0]["sessions"][0]["completion"]["acceptance"]["decision"], json!("accepted"));
}

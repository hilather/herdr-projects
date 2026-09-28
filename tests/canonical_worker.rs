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
/// `drop-release` drops gate-release input unsent and unanswered.
const SERVER: &str = r#"
import json,os,sys,socket,subprocess
path=sys.argv[1];root=os.path.dirname(path);s={}
server=socket.socket(socket.AF_UNIX);server.bind(path);server.listen()
while True:
 c,_=server.accept();f=c.makefile('rw');r=json.loads(f.readline());m=r['method'];p=r.get('params') or {}
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
 elif m=='workspace.list':res={'type':'workspace_list','workspaces':[{'workspace_id':'w1','label':s['label'],'pane_count':1,'tab_count':1}] if 'pid' in s else []}
 elif m=='pane.list':res={'panes':[pane] if 'pid' in s else []}
 elif m=='pane.get':res={'pane':pane}
 elif m=='pane.process_info':res={'process_info':{'pane_id':'w1:p1','foreground_processes':[{'pid':s['pid'],'argv':s['argv']}] if 'pid' in s else []}}
 elif m=='pane.send_input' and os.path.exists(os.path.join(root,'drop-release')):pass
 elif m=='pane.send_input':
  fd=os.open(s['fifo'],os.O_WRONLY);os.write(fd,p['text'].encode());os.close(fd);s['released']=True;res={'type':'ok'}
 elif m=='agent.list':res={'type':'agent_list','agents':[agent] if s.get('released') else []}
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
    fn new(budget: &str) -> Self {
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
        let route = RuntimeRoute { socket: lab.socket().display().to_string(), cwd: lab.repo.canonicalize().unwrap().display().to_string(), ..Default::default() };
        let id = TaskId::new("work").unwrap();
        let revision = runtime::snapshot(&lab.project).unwrap().tasks.into_iter().find(|t| t.id == id).unwrap().revision;
        let change = runtime::create_binding(&lab.project, Some(&id), Some(revision), lab.head(), &route).unwrap();
        lab.binding = change.binding.id;
        lab.resume();
        lab.write_binaries();
        lab
    }
    /// Record a fresh observation of the binding and set the project active.
    fn resume(&self) {
        let state = self.state();
        let binding = state.runtime_bindings.iter().find(|b| b.id == self.binding).unwrap();
        let task = state.tasks.iter().find(|t| t.id.as_str() == "work").unwrap();
        let config = self.path(".config/herdr-projects/config.toml");
        let observation = herdr_projects::reconcile::RuntimeObservation { binding: binding.id.clone(), binding_revision: binding.revision,
            task_revision: Some(task.revision), observed_unix_ms: jiff::Timestamp::now().as_millisecond(), collector: "herdr-git-v2".into(),
            config_digest: migration::config_reference(&config).unwrap().digest, ..Default::default() };
        migration::open_active(&self.project).unwrap().record_observations(state.head, &[observation]).unwrap();
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
    fn selection(&self, instructions: &str) -> PathBuf {
        fs::write(self.project.join("PROJECT.md"), instructions).unwrap();
        let scope = self.path("scope.json");
        fs::write(&scope, r#"{"schema_version":1,"task_id":"work","profile":"worker","domains":[],"paths":[],"pinned_keys":[],"sensitivity":"default"}"#).unwrap();
        let snapshot = self.ok(&["memory", "demo", "snapshot", "--task", "work", "--profile", "worker", "--input-file", scope.to_str().unwrap(), "--worker"]);
        let selection = self.path("selection.json");
        fs::write(&selection, json!({"task":"work","binding":self.binding,"profile":self.profile,
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
        while !predicate() {
            assert!(ticker.0.try_wait().unwrap().is_none(), "ticker exited");
            assert!(Instant::now() < deadline, "{}", fs::read_to_string(self.path("root/.ticker.log")).unwrap_or_default());
            std::thread::sleep(Duration::from_millis(20));
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
    lab.run_for("agent.list", 2);
    assert_eq!((lab.count("workspace.create_command"), lab.count("agent.prompt")), (1, 1));
    assert_eq!(lab.attempt(&attempt), running);
    assert!(lab.events("runtime.worker_terminated").is_empty());
    assert_eq!((lab.state().tasks, lab.state().deliveries), (observed.tasks, observed.deliveries));
    assert!(!herdr_projects::worker_supervision::SupervisorObservation::recover_exited(&supervisor).unwrap(), "the worker is running");

    // The operator cancels the attempt, revokes its approval and pauses the project.
    let report = lab.project.join("REPORT.md");
    fs::write(&report, "retain this report").unwrap();
    lab.ok(&["task", "demo", "cancel-attempt", attempt.as_str(), "--expected-revision", &running.revision.to_string(), "--expected-head", &lab.head().to_string(), "--reason", "operator stop"]);
    lab.ok(&["approval", "demo", "revoke", &approval, "--expected-head", &lab.head().to_string(), "--reason", "stop execution"]);
    let control = lab.state().control.unwrap().revision.to_string();
    lab.ok(&["runtime", "demo", "state", "paused", "--expected-revision", &control, "--expected-head", &lab.head().to_string()]);
    let before = lab.state();
    let mut ticker = lab.spawn();
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

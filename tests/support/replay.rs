#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! Shared replay workflow harness for end-to-end CLI tests.
#![allow(dead_code)]
//!
//! A project's accepted history is built from `tests/fixtures/replay/sample-history.json`
//! through the real contract, `result submit`, `result verify` and
//! `result integrate` paths (each source attempt is planted as a launch would
//! write it). `replay extract` turns it into suite v1; `replay run` creates
//! ordinary replay tasks; the owner signs their drafted contracts; candidates
//! are verified by the hidden checks in the isolated verifier. One candidate
//! is launched for real through the ticker on a Herdr stand-in and probes its
//! sandbox for the hidden checks.
use herdr_projects::{authority, domain::*, migration, runtime};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, io::Write as _, os::unix::fs::{MetadataExt, PermissionsExt}, path::{Path, PathBuf}, process::{Child, Command, Output, Stdio}, time::{Duration, Instant}};

pub(crate) const BIN: &str = env!("CARGO_BIN_EXE_herdr-projects");
const HISTORY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/replay/sample-history.json");
pub(crate) const GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/replay/suite-v1.golden.json");
pub(crate) const CLEAN: &str = r#"{"version":1,"checks":["/usr/bin/git","diff","--quiet"]}"#;

/// The Herdr stand-in of tests/canonical_worker.rs: runs `workspace.create_command`
/// for real and logs each request beside the socket.
const SERVER: &str = r#"
import json,os,sys,socket,subprocess
path=sys.argv[1];root=os.path.dirname(path);s={}
server=socket.socket(socket.AF_UNIX);server.bind(path);server.listen()
while True:
 c,_=server.accept();f=c.makefile('rw');r=json.loads(f.readline());m=r['method'];p=r.get('params') or {}
 live='pid' in s
 with open(os.path.join(root,'requests'),'a') as log:log.write(json.dumps({'method':m,'params':p})+'\n')
 pane={'pane_id':'w1:p1','workspace_id':'w1','tab_id':'w1:t1','terminal_id':'term1','cwd':s.get('cwd')}
 agent=dict(pane,agent='claude',interactive_ready=True,agent_status='idle',**({'name':s['name']} if 'name' in s else {}))
 res=None
 if m=='ping':res={'type':'pong','version':'0.9.1','capabilities':{'workspace_create_command':True}}
 elif m=='workspace.create_command' and 'pid' not in s:
  fifo=os.path.join(root,'input');os.mkfifo(fifo);fd=os.open(fifo,os.O_RDWR)
  child=subprocess.Popen(p['command'],cwd=p['cwd'],stdin=fd,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,start_new_session=True,env={'PATH':'/usr/bin:/bin'})
  s.update(pid=child.pid,argv=p['command'],cwd=p['cwd'],label=p['label'],fifo=fifo)
  res={'type':'workspace_created','workspace':{'workspace_id':'w1'},'root_pane':{'pane_id':'w1:p1'}}
 elif m=='workspace.list':res={'type':'workspace_list','workspaces':[{'workspace_id':'w1','label':s['label'],'pane_count':1,'tab_count':1}] if live else []}
 elif m=='pane.list':res={'panes':[pane] if live else []}
 elif m=='pane.get':res={'pane':pane}
 elif m=='pane.process_info':res={'process_info':{'pane_id':'w1:p1','foreground_processes':[{'pid':s['pid'],'argv':s['argv']}] if live else []}}
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

pub(crate) struct Lab { pub(crate) home: tempfile::TempDir, pub(crate) project: PathBuf, pub(crate) key: PathBuf, pub(crate) repo: PathBuf, pub(crate) herdr: PathBuf, pub(crate) profile: VersionedReference, pub(crate) server: Option<Child>, pub(crate) history: History }

/// Names for every oid and id the fixture history produced, so extraction output can be compared with the golden file.
#[derive(Default)]
pub(crate) struct History { pub(crate) names: BTreeMap<String, String>, pub(crate) fixture: Value }

impl Drop for Lab {
    fn drop(&mut self) {
        let _ = Command::new("/usr/bin/pkill").args(["-KILL", "-f"]).arg(self.home.path().join("agent-home")).status();
        if let Some(server) = &mut self.server { let _ = server.kill(); let _ = server.wait(); }
    }
}

pub(crate) struct Ticker(pub(crate) Child);
impl Drop for Ticker { fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); } }

pub(crate) fn sign(key: &Path, namespace: &str, document: &Path) {
    assert!(Command::new("/usr/bin/ssh-keygen").args(["-Y", "sign", "-f"]).arg(key).args(["-n", namespace]).arg(document).output().unwrap().status.success());
}

impl Lab {
    /// An active project `demo` with an owner key, a SHA-256 repository `~/repo`,
    /// the queued task `work` bound to the lab server, and a `worker` profile
    /// whose budget table is `budget`. The owner configuration hides nothing
    /// extra: a replay candidate's own sandbox hides `~/repo` at launch.
    pub(crate) fn new(budget: &str) -> Self {
        let home = tempfile::tempdir().unwrap();
        let key = home.path().join("owner");
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&key).output().unwrap().status.success());
        let public = fs::read_to_string(key.with_extension("pub")).unwrap().split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        let config = home.path().join(".config/herdr-projects/config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, format!("[authority]\nversion=1\nrevision=1\napproval_public_key={public:?}\n[profiles.worker]\nkind='claude'\npermission_policy='interactive'\n[profiles.worker.budget]\nmax_wall_seconds=600\n{budget}\n")).unwrap();
        for dir in ["repo", "bin", "agent-home", "lab"] { fs::create_dir(home.path().join(dir)).unwrap(); }
        let lab = Lab { project: home.path().join("root/demo"), key, repo: home.path().join("repo"), herdr: home.path().join("bin/herdr"),
            profile: VersionedReference { id: String::new(), revision: 1, digest: String::new() }, server: None, history: History::default(), home };
        for command in ["new", "pause"] { lab.ok(&[command, "demo"]); }
        migration::apply(&lab.project, &migration::inspect_with_config(&lab.project, &config).unwrap(), true).unwrap();
        lab.git(&["init", "-q", "--object-format=sha256", "-b", "master"]);
        lab.git(&["commit", "-q", "--allow-empty", "-m", "empty"]);
        lab.ok(&["task", "demo", "add", "work", "--title", "work", "--expected-head", &lab.head().to_string()]);
        let request = lab.path("queue.json");
        fs::write(&request, r#"{"priority":0,"dependencies":[]}"#).unwrap();
        lab.ok(&["task", "demo", "queue", "work", "--input-file", request.to_str().unwrap(), "--expected-revision", "1", "--expected-head", &lab.head().to_string()]);
        let policy = lab.state().scheduler.unwrap().policy.revision.to_string();
        lab.ok(&["scheduler", "demo", "policy", "--max-active-workers", "1", "--max-attempts-per-task", "3", "--expected-revision", &policy, "--expected-head", &lab.head().to_string()]);
        lab.bind("work", &lab.repo.canonicalize().unwrap());
        lab.observe();
        lab.write_binaries();
        lab
    }
    pub(crate) fn path(&self, name: &str) -> PathBuf { self.home.path().join(name) }
    pub(crate) fn root(&self) -> PathBuf { self.path("root") }
    pub(crate) fn socket(&self) -> PathBuf { self.path("lab/native.sock") }
    pub(crate) fn db(&self) -> rusqlite::Connection { rusqlite::Connection::open(self.project.join(".state/state.db")).unwrap() }
    pub(crate) fn store(&self) -> String { self.project.join(".state/state.db").canonicalize().unwrap().display().to_string() }
    pub(crate) fn cli(&self, args: &[&str]) -> Output {
        Command::new(BIN).env_clear().env("HOME", self.home.path()).env("PATH", "/usr/bin:/bin")
            .args(["--root", self.root().to_str().unwrap()]).args(args).output().unwrap()
    }
    pub(crate) fn ok(&self, args: &[&str]) -> Value {
        let out = self.cli(args);
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
    }
    pub(crate) fn fail(&self, args: &[&str]) -> String {
        let out = self.cli(args);
        assert!(!out.status.success(), "{args:?} succeeded: {}", String::from_utf8_lossy(&out.stdout));
        String::from_utf8_lossy(&out.stderr).into_owned()
    }
    pub(crate) fn replay(&self, args: &[&str]) -> Value { self.ok(&[&["replay", "demo"], args].concat()) }
    pub(crate) fn state(&self) -> Snapshot {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop { match runtime::snapshot(&self.project) { Ok(s) => return s, Err(e) => { assert!(Instant::now() < deadline, "{e:#}"); std::thread::sleep(Duration::from_millis(20)); } } }
    }
    pub(crate) fn head(&self) -> u64 { self.state().head }
    pub(crate) fn git_in(&self, dir: &Path, args: &[&str]) -> String {
        let out = Command::new("/usr/bin/git").env_clear().env("PATH", "/usr/bin:/bin").env("HOME", self.home.path())
            .env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "fixture").env("GIT_AUTHOR_EMAIL", "fixture@example.com").env("GIT_COMMITTER_NAME", "fixture").env("GIT_COMMITTER_EMAIL", "fixture@example.com")
            .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z").env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
            .current_dir(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }
    pub(crate) fn git(&self, args: &[&str]) -> String { self.git_in(&self.repo.clone(), args) }
    /// A runtime binding for `task` routed to the lab server in `cwd`.
    pub(crate) fn bind(&self, task: &str, cwd: &Path) -> String {
        let route = RuntimeRoute { socket: self.socket().display().to_string(), cwd: cwd.display().to_string(), ..Default::default() };
        let id = TaskId::new(task).unwrap();
        let revision = self.state().tasks.into_iter().find(|t| t.id == id).unwrap().revision;
        runtime::create_binding(&self.project, Some(&id), Some(revision), self.head(), &route).unwrap().binding.id
    }
    /// A fresh observation of every binding, then the project active.
    pub(crate) fn observe(&self) {
        let state = self.state();
        let config = self.path(".config/herdr-projects/config.toml");
        let observations = state.runtime_bindings.iter().map(|binding| herdr_projects::reconcile::RuntimeObservation { binding: binding.id.clone(), binding_revision: binding.revision,
            task_revision: binding.task.as_ref().map(|id| state.tasks.iter().find(|t| &t.id == id).unwrap().revision),
            observed_unix_ms: jiff::Timestamp::now().as_millisecond(), collector: "herdr-git-v2".into(),
            config_digest: migration::config_reference(&config).unwrap().digest.clone(), ..Default::default() }).collect::<Vec<_>>();
        migration::open_active(&self.project).unwrap().record_observations(state.head, &observations).unwrap();
        // A new binding needs reconciliation: observed, the owner admits the project again.
        let control = self.state().control.unwrap();
        if control.state != ProjectState::Active {
            self.ok(&["runtime", "demo", "state", "active", "--expected-revision", &control.revision.to_string(), "--expected-head", &self.head().to_string()]);
        }
    }
    pub(crate) fn write_binaries(&self) {
        fs::write(&self.herdr, "#!/usr/bin/python3\nimport os,socket,sys,json\nif sys.argv[1:]==['--version']:print('herdr 0.9.1');sys.exit(0)\n\
probe={('pane','list'):'pane.list',('agent','list'):'agent.list'}.get(tuple(sys.argv[1:]))\nassert probe or sys.argv[1:]==['remote-api-bridge']\n\
line=json.dumps({'id':'probe','method':probe}).encode()+b'\\n' if probe else sys.stdin.buffer.readline()\n\
c=socket.socket(socket.AF_UNIX);c.connect(os.environ['HERDR_SOCKET_PATH']);c.sendall(line)\nreply=c.makefile('rb').readline()\nif not reply:sys.exit(1)\n\
sys.stdout.buffer.write(json.dumps({'result':json.loads(reply)['result']}).encode() if probe else reply)\n").unwrap();
        fs::set_permissions(&self.herdr, fs::Permissions::from_mode(0o700)).unwrap();
        self.build_agent("fn main(){if std::env::args().nth(1).as_deref()==Some(\"--version\"){println!(\"2.1.0 (Claude Code)\");return}loop{std::thread::park()}}");
    }
    pub(crate) fn build_agent(&self, source: &str) {
        let (agent, file) = (self.path("bin/claude"), self.path("bin/agent.rs"));
        fs::write(&file, source).unwrap();
        let built = Command::new("rustc").args(["--edition", "2021", "-o"]).arg(&agent).arg(&file).output().unwrap();
        assert!(built.status.success(), "{}", String::from_utf8_lossy(&built.stderr));
        fs::set_permissions(&agent, fs::Permissions::from_mode(0o700)).unwrap();
    }
    /// `profile prepare` over the lab binaries; only the native interaction evidence is planted.
    pub(crate) fn prepare_profile(&mut self) {
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
        self.db().execute("INSERT INTO native_profiles(profile_digest,report,report_digest,sequence) VALUES(?1,?2,?3,(SELECT max(sequence) FROM events))",
            rusqlite::params![reference.digest, report, format!("{:x}", Sha256::digest(report.as_bytes()))]).unwrap();
        self.profile = reference;
    }
    /// Retained knowledge for `task` and its launch selection over `repository`.
    pub(crate) fn selection(&self, task: &str, binding: &str, repository: &Path, reason: Option<&str>) -> PathBuf {
        fs::write(self.project.join("PROJECT.md"), "Retained instructions").unwrap();
        let scope = self.path(&format!("{task}-scope.json"));
        fs::write(&scope, json!({"schema_version":1,"task_id":task,"profile":"worker","domains":[],"paths":[],"pinned_keys":[],"sensitivity":"default"}).to_string()).unwrap();
        let snapshot = self.ok(&["memory", "demo", "snapshot", "--task", task, "--profile", "worker", "--input-file", scope.to_str().unwrap(), "--worker"]);
        let selection = self.path(&format!("{task}-selection.json"));
        let mut body = json!({"task":task,"binding":binding,"profile":self.profile,"knowledge":{"id":snapshot["id"],"revision":1,"digest":snapshot["manifest_hash"]},"repositories":[repository]});
        if let Some(reason) = reason { body["reason"] = json!(reason); }
        fs::write(&selection, body.to_string()).unwrap();
        selection
    }
    /// Draft, owner-sign, import and reserve a launch of `selection`; returns the attempt.
    pub(crate) fn reserve(&self, selection: &Path) -> AttemptId {
        let drafted = self.ok(&["launch", "demo", "draft", "--selection", selection.to_str().unwrap(), "--expected-head", &self.head().to_string()]);
        let document = self.path("approval.json");
        fs::write(&document, serde_json::to_vec_pretty(&drafted["approval"]).unwrap()).unwrap();
        sign(&self.key, authority::SIGNATURE_NAMESPACE, &document);
        let approval = self.ok(&["approval", "demo", "import", document.to_str().unwrap(), self.path("approval.json.sig").to_str().unwrap(), "--expected-head", &self.head().to_string()]);
        let reservation = self.ok(&["launch", "demo", "reserve", "--selection", selection.to_str().unwrap(), "--approval-digest", approval["digest"].as_str().unwrap(), "--expected-head", &self.head().to_string()]);
        AttemptId::new(reservation["record"]["attempt"].as_str().unwrap()).unwrap()
    }
    pub(crate) fn serve(&mut self) {
        self.server = Some(Command::new("/usr/bin/python3").args(["-c", SERVER]).arg(self.socket()).spawn().unwrap());
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.socket().exists() { assert!(Instant::now() < deadline); std::thread::sleep(Duration::from_millis(10)); }
    }
    pub(crate) fn spawn(&self) -> Ticker {
        Ticker(Command::new(BIN).env_clear().env("HOME", self.home.path()).env("PATH", "/usr/bin:/bin").env("HERDR_BIN_PATH", &self.herdr)
            .args(["--root", self.root().to_str().unwrap(), "ticker", "run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap())
    }
    pub(crate) fn wait(&self, ticker: &mut Ticker, seconds: u64, predicate: &dyn Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        let mut pause = Duration::from_millis(20);
        while !predicate() {
            assert!(ticker.0.try_wait().unwrap().is_none(), "ticker exited");
            assert!(Instant::now() < deadline, "{}", fs::read_to_string(self.path("root/.ticker.log")).unwrap_or_default());
            std::thread::sleep(pause);
            pause = (pause * 2).min(Duration::from_millis(250));
        }
    }
    pub(crate) fn stop(&self, mut ticker: Ticker) {
        let stop = self.path("root/.ticker.stop");
        fs::write(&stop, b"").unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        while ticker.0.try_wait().unwrap().is_none() { assert!(Instant::now() < deadline); std::thread::sleep(Duration::from_millis(10)); }
        fs::remove_file(&stop).unwrap();
    }
    /// `ok` against a running ticker, rebuilding arguments from current state and retrying for a bounded time.
    pub(crate) fn ok_live(&self, args: &dyn Fn() -> Vec<String>) -> Value {
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

    // ---- the fixture history -------------------------------------------------

    /// Signed contract revision 1 of `task` (the document `body` without head/store/authority); returns its digest.
    pub(crate) fn install_contract(&self, task: &str, mut body: Value) -> String {
        body["project_store"] = json!(self.store());
        body["expected_head"] = json!(self.head());
        body["authority"] = serde_json::to_value(authority::policy_reference(&self.project).unwrap()).unwrap();
        let document = self.path(&format!("{task}-contract.json"));
        let mut bytes = serde_json::to_vec_pretty(&body).unwrap();
        bytes.push(b'\n');
        fs::write(&document, bytes).unwrap();
        sign(&self.key, authority::CONTRACT_SIGNATURE_NAMESPACE, &document);
        self.ok(&["task", "demo", "contract", "put", "--input-file", document.to_str().unwrap(), "--signature", document.with_extension("json.sig").to_str().unwrap()])["digest"]
            .as_str().unwrap().to_owned()
    }
    /// A running attempt of `task`, as a launch would write it.
    pub(crate) fn plant_attempt(&self, task: &str, attempt: &str) {
        self.db().execute("INSERT INTO attempts(id,task_id,revision,state,snapshot,reservation,termination_observed) VALUES(?1,?2,1,'running',NULL,?1,0)", [attempt, task]).unwrap();
    }
    /// The attempt ended, as the controller records a proven termination.
    pub(crate) fn end_attempt(&self, attempt: &str) {
        self.db().execute("UPDATE attempts SET state='completed',termination_observed=1,revision=revision+1 WHERE id=?1", [attempt]).unwrap();
    }
    /// `result submit` of `candidate` in `repository` for `attempt`; returns the submission id.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn submit(&self, task: &str, attempt: &str, digest: &str, repository: &Path, base: &str, candidate: &str, outputs: &[&str], key: &str) -> String {
        let objects: Vec<Value> = self.git_in(repository, &["rev-list", "--objects", candidate]).lines()
            .map(|line| { let oid = line.split_whitespace().next().unwrap(); json!({"oid": oid, "relative_path": format!("{}/{}", &oid[..2], &oid[2..])}) }).collect();
        // Integrations may make ancestors reachable through packs/alternates.
        // The submission API requires real loose files for the listed objects.
        let loose = self.path(&format!("{key}-loose"));
        fs::create_dir_all(&loose).unwrap();
        for object in &objects {
            let oid = object["oid"].as_str().unwrap();
            let relative = object["relative_path"].as_str().unwrap();
            let target = repository.join(".git/objects").join(relative);
            if target.exists() { continue; }
            let kind = self.git_in(repository, &["cat-file", "-t", oid]);
            let bytes = Command::new("/usr/bin/git").env_clear().env("PATH", "/usr/bin:/bin")
                .args(["-C", repository.to_str().unwrap(), "cat-file", &kind, oid]).output().unwrap();
            assert!(bytes.status.success());
            let mut writer = Command::new("/usr/bin/git").env_clear().env("PATH", "/usr/bin:/bin")
                .env("GIT_OBJECT_DIRECTORY", &loose).args(["-C", repository.to_str().unwrap(), "hash-object", "-w", "-t", &kind, "--stdin"])
                .stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
            writer.stdin.take().unwrap().write_all(&bytes.stdout).unwrap();
            let written = writer.wait_with_output().unwrap();
            assert!(written.status.success());
            assert_eq!(String::from_utf8(written.stdout).unwrap().trim(), oid);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::copy(loose.join(relative), target).unwrap();
        }
        let file = self.path(&format!("{key}-submit.json"));
        fs::write(&file, json!({"idempotency_key": key, "task_id": task, "contract_revision": 1, "contract_digest": digest, "attempt_id": attempt,
            "repository": repository.canonicalize().unwrap().display().to_string(), "base_oid": base, "candidate_oid": candidate, "object_format": "sha256",
            "artifact_manifest": outputs.iter().map(|p| json!({"path": p, "oid": candidate})).collect::<Vec<_>>(), "claimed_checks": [], "objects": objects}).to_string()).unwrap();
        self.ok(&["result", "demo", "submit", "--input-file", file.to_str().unwrap()])["submission_id"].as_str().unwrap().to_owned()
    }
    /// `result verify` of `submission` against policy `id` whose exact text is `policy`.
    /// The recorded verdict is printed either way; a rejection also exits non-zero.
    pub(crate) fn verify(&self, submission: &str, id: &str, policy: &str, key: &str) -> Value {
        let file = self.path(&format!("{key}-policy.json"));
        fs::write(&file, policy).unwrap();
        let out = self.cli(&["result", "demo", "verify", submission, "--policy-id", id, "--policy-file", file.to_str().unwrap(), "--idempotency-key", key,
            "--work-dir", self.path(&format!("{key}-work")).to_str().unwrap(), "--timeout-seconds", "60"]);
        let verdict: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&out.stderr)));
        assert_eq!(out.status.success(), verdict["state"] == "accepted", "{verdict}");
        verdict
    }
    /// Commit `files` on a new branch `branch` of `repository` from `from`; returns the commit.
    pub(crate) fn commit_files(&self, repository: &Path, branch: &str, from: &str, files: &BTreeMap<String, String>) -> String {
        self.git_in(repository, &["checkout", "-q", "-B", branch, from]);
        for (path, text) in files {
            let path = repository.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        self.git_in(repository, &["add", "-A"]);
        self.git_in(repository, &["commit", "-q", "-m", branch]);
        self.git_in(repository, &["rev-parse", "HEAD"])
    }
    pub(crate) fn name(&mut self, value: &str, name: &str) { self.history.names.insert(value.to_owned(), name.to_owned()); }

    /// Accept every fixture change: contract, planted attempt, submission,
    /// verification and integration, each change based on the integration tip.
    pub(crate) fn build_history(&mut self) {
        let fixture: Value = serde_json::from_str(&fs::read_to_string(HISTORY).unwrap()).unwrap();
        let files = |value: &Value| value.as_object().unwrap().iter().map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned())).collect::<BTreeMap<_, _>>();
        let empty = self.git(&["rev-parse", "HEAD"]);
        let base = self.commit_files(&self.repo.clone(), "fixture-base", &empty, &files(&fixture["base"]));
        self.name(&base, "<base>");
        self.git(&["branch", "integration", &base]);
        let repository = self.repo.canonicalize().unwrap();
        self.ok(&["result", "demo", "configure-integration", "--repository", repository.to_str().unwrap(), "--reference", "refs/heads/integration"]);
        for change in fixture["changes"].as_array().unwrap() {
            let task = change["task"].as_str().unwrap();
            let tip = self.git(&["rev-parse", "refs/heads/integration"]);
            self.ok(&["task", "demo", "add", task, "--title", change["title"].as_str().unwrap(), "--expected-head", &self.head().to_string()]);
            let scope: Vec<Value> = change["scope"].as_array().unwrap().iter().map(|p| json!({"path": p, "access": "write"})).collect();
            let outputs: Vec<&str> = change["outputs"].as_array().unwrap().iter().map(|p| p.as_str().unwrap()).collect();
            let digest = self.install_contract(task, json!({"version": 3, "task_id": task, "contract_revision": 1, "deliverable": change["title"],
                "non_goals": "none", "acceptance_policies": [{"id": "clean", "text": CLEAN}], "repository": repository, "base_oid": tip, "object_format": "sha256",
                "dependencies": [], "capability_flags": [], "profile_kind": "claude", "retry_class": "none", "result_schema_id": "result-v1", "route": "verify_then_integrate",
                "scope": {"paths": scope}, "outputs": outputs.iter().map(|p| json!({"path": p, "kind": "git_file"})).collect::<Vec<_>>()}));
            self.name(&digest, &format!("<{task} contract>"));
            let attempt = format!("{task}-attempt");
            self.plant_attempt(task, &attempt);
            let candidate = self.commit_files(&repository, &format!("change-{task}"), &tip, &files(&change["files"]));
            self.name(&candidate, &format!("<{task} candidate>"));
            let submission = self.submit(task, &attempt, &digest, &repository, &tip, &candidate, &outputs, &format!("{task}-submit"));
            self.name(&submission, &format!("<{task} submission>"));
            let verified = self.verify(&submission, "clean", CLEAN, &format!("{task}-verify"));
            assert_eq!(verified["state"], "accepted", "{verified}");
            let result = verified["receipt"]["result_id"].as_str().unwrap().to_owned();
            self.name(&result, &format!("<{task} result>"));
            let integrated = self.ok(&["result", "demo", "integrate", &result, "--repository", repository.to_str().unwrap(), "--idempotency-key", &format!("{task}-integrate"),
                "--work-dir", self.path(&format!("{task}-integrate")).to_str().unwrap()]);
            assert_eq!(integrated["state"], "integrated", "{integrated}");
            self.name(&self.git(&["rev-parse", "refs/heads/integration"]), &format!("<{task} integrated>"));
            self.end_attempt(&attempt);
        }
        self.git(&["checkout", "-q", "--detach", "fixture-base"]);
        fs::write(self.project.join("PROJECT.md"), fixture["project_md"].as_str().unwrap()).unwrap();
        self.name(&repository.display().to_string(), "<source repository>");
        self.history.fixture = fixture;
    }
    /// The fixture change of `case` (`<task>.r1`).
    pub(crate) fn change(&self, case: &str) -> Value {
        let task = case.strip_suffix(".r1").unwrap();
        self.history.fixture["changes"].as_array().unwrap().iter().find(|c| c["task"] == task).unwrap().clone()
    }
    /// The solution files of `case` (its changed files outside tests/).
    pub(crate) fn solution(&self, case: &str) -> BTreeMap<String, String> {
        self.change(case)["files"].as_object().unwrap().iter().filter(|(k, _)| !k.starts_with("tests/")).map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned())).collect()
    }
    /// Replace every recorded oid or id in `value` by its fixture name.
    pub(crate) fn normalize(&self, value: &Value) -> Value {
        match value {
            Value::String(s) => json!(self.history.names.get(s).cloned().unwrap_or_else(|| s.clone())),
            Value::Array(a) => Value::Array(a.iter().map(|v| self.normalize(v)).collect()),
            Value::Object(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), self.normalize(v))).collect()),
            other => other.clone(),
        }
    }
    /// Draft `task`'s replay contract, sign it as the owner and install it; returns (digest, document).
    pub(crate) fn install_replay_contract(&self, task: &str) -> (String, Value) { self.install_replay_contract_routed(task, None) }
    /// As `install_replay_contract`, the owner having changed the drafted route to `route`.
    pub(crate) fn install_replay_contract_routed(&self, task: &str, route: Option<&str>) -> (String, Value) {
        let document = self.path(&format!("{task}-replay-contract.json"));
        self.replay(&["contract", task, "--output", document.to_str().unwrap()]);
        if let Some(route) = route {
            let mut drafted: Value = serde_json::from_slice(&fs::read(&document).unwrap()).unwrap();
            drafted["route"] = json!(route);
            let mut bytes = serde_json::to_vec_pretty(&drafted).unwrap();
            bytes.push(b'\n');
            fs::write(&document, bytes).unwrap();
        }
        sign(&self.key, authority::CONTRACT_SIGNATURE_NAMESPACE, &document);
        let installed = self.ok(&["task", "demo", "contract", "put", "--input-file", document.to_str().unwrap(), "--signature", document.with_extension("json.sig").to_str().unwrap()]);
        (installed["digest"].as_str().unwrap().to_owned(), serde_json::from_slice(&fs::read(&document).unwrap()).unwrap())
    }
}

pub(crate) fn contains(haystack: &[u8], needle: &str) -> bool { haystack.windows(needle.len()).any(|w| w == needle.as_bytes()) }


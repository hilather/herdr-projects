#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! Replay evaluation suite (TM4.6) end to end through the compiled CLI.
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
use std::{collections::BTreeMap, fs, os::unix::fs::{MetadataExt, PermissionsExt}, path::{Path, PathBuf}, process::{Child, Command, Output, Stdio}, time::{Duration, Instant}};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-projects");
const HISTORY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/replay/sample-history.json");
const GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/replay/suite-v1.golden.json");
const CLEAN: &str = r#"{"version":1,"checks":["/usr/bin/git","diff","--quiet"]}"#;
/// The source repository holds every accepted change: replay workers must not see it.
const HIDE_SOURCE: &str = "\n[worker_isolation]\nhide=['~/repo']";

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

struct Lab { home: tempfile::TempDir, project: PathBuf, key: PathBuf, repo: PathBuf, herdr: PathBuf, profile: VersionedReference, server: Option<Child>, history: History }

/// Names for every oid and id the fixture history produced, so extraction output can be compared with the golden file.
#[derive(Default)]
struct History { names: BTreeMap<String, String>, fixture: Value }

impl Drop for Lab {
    fn drop(&mut self) {
        let _ = Command::new("/usr/bin/pkill").args(["-KILL", "-f"]).arg(self.home.path().join("agent-home")).status();
        if let Some(server) = &mut self.server { let _ = server.kill(); let _ = server.wait(); }
    }
}

struct Ticker(Child);
impl Drop for Ticker { fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); } }

fn sign(key: &Path, namespace: &str, document: &Path) {
    assert!(Command::new("/usr/bin/ssh-keygen").args(["-Y", "sign", "-f"]).arg(key).args(["-n", namespace]).arg(document).output().unwrap().status.success());
}

impl Lab {
    /// An active project `demo` with an owner key, a SHA-256 repository `~/repo`,
    /// the queued task `work` bound to the lab server, and a `worker` profile
    /// whose budget table is `budget` (the owner configuration hides `~/repo`
    /// from workers).
    fn new(budget: &str) -> Self {
        let home = tempfile::tempdir().unwrap();
        let key = home.path().join("owner");
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&key).output().unwrap().status.success());
        let public = fs::read_to_string(key.with_extension("pub")).unwrap().split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        let config = home.path().join(".config/herdr-projects/config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, format!("[authority]\nversion=1\nrevision=1\napproval_public_key={public:?}\n[profiles.worker]\nkind='claude'\npermission_policy='interactive'\n[profiles.worker.budget]\nmax_wall_seconds=600\n{budget}{HIDE_SOURCE}\n")).unwrap();
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
    fn path(&self, name: &str) -> PathBuf { self.home.path().join(name) }
    fn root(&self) -> PathBuf { self.path("root") }
    fn socket(&self) -> PathBuf { self.path("lab/native.sock") }
    fn db(&self) -> rusqlite::Connection { rusqlite::Connection::open(self.project.join(".state/state.db")).unwrap() }
    fn store(&self) -> String { self.project.join(".state/state.db").canonicalize().unwrap().display().to_string() }
    fn cli(&self, args: &[&str]) -> Output {
        Command::new(BIN).env_clear().env("HOME", self.home.path()).env("PATH", "/usr/bin:/bin")
            .args(["--root", self.root().to_str().unwrap()]).args(args).output().unwrap()
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
    fn replay(&self, args: &[&str]) -> Value { self.ok(&[&["replay", "demo"], args].concat()) }
    fn state(&self) -> Snapshot {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop { match runtime::snapshot(&self.project) { Ok(s) => return s, Err(e) => { assert!(Instant::now() < deadline, "{e:#}"); std::thread::sleep(Duration::from_millis(20)); } } }
    }
    fn head(&self) -> u64 { self.state().head }
    fn git_in(&self, dir: &Path, args: &[&str]) -> String {
        let out = Command::new("/usr/bin/git").env_clear().env("PATH", "/usr/bin:/bin").env("HOME", self.home.path())
            .env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "fixture").env("GIT_AUTHOR_EMAIL", "fixture@example.com").env("GIT_COMMITTER_NAME", "fixture").env("GIT_COMMITTER_EMAIL", "fixture@example.com")
            .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z").env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
            .current_dir(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }
    fn git(&self, args: &[&str]) -> String { self.git_in(&self.repo.clone(), args) }
    /// A runtime binding for `task` routed to the lab server in `cwd`.
    fn bind(&self, task: &str, cwd: &Path) -> String {
        let route = RuntimeRoute { socket: self.socket().display().to_string(), cwd: cwd.display().to_string(), ..Default::default() };
        let id = TaskId::new(task).unwrap();
        let revision = self.state().tasks.into_iter().find(|t| t.id == id).unwrap().revision;
        runtime::create_binding(&self.project, Some(&id), Some(revision), self.head(), &route).unwrap().binding.id
    }
    /// A fresh observation of every binding, then the project active.
    fn observe(&self) {
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
    fn write_binaries(&self) {
        fs::write(&self.herdr, "#!/usr/bin/python3\nimport os,socket,sys,json\nif sys.argv[1:]==['--version']:print('herdr 0.9.1');sys.exit(0)\n\
probe={('pane','list'):'pane.list',('agent','list'):'agent.list'}.get(tuple(sys.argv[1:]))\nassert probe or sys.argv[1:]==['remote-api-bridge']\n\
line=json.dumps({'id':'probe','method':probe}).encode()+b'\\n' if probe else sys.stdin.buffer.readline()\n\
c=socket.socket(socket.AF_UNIX);c.connect(os.environ['HERDR_SOCKET_PATH']);c.sendall(line)\nreply=c.makefile('rb').readline()\nif not reply:sys.exit(1)\n\
sys.stdout.buffer.write(json.dumps({'result':json.loads(reply)['result']}).encode() if probe else reply)\n").unwrap();
        fs::set_permissions(&self.herdr, fs::Permissions::from_mode(0o700)).unwrap();
        self.build_agent("fn main(){if std::env::args().nth(1).as_deref()==Some(\"--version\"){println!(\"2.1.0 (Claude Code)\");return}loop{std::thread::park()}}");
    }
    fn build_agent(&self, source: &str) {
        let (agent, file) = (self.path("bin/claude"), self.path("bin/agent.rs"));
        fs::write(&file, source).unwrap();
        let built = Command::new("rustc").args(["--edition", "2021", "-o"]).arg(&agent).arg(&file).output().unwrap();
        assert!(built.status.success(), "{}", String::from_utf8_lossy(&built.stderr));
        fs::set_permissions(&agent, fs::Permissions::from_mode(0o700)).unwrap();
    }
    /// `profile prepare` over the lab binaries; only the native interaction evidence is planted.
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
        self.db().execute("INSERT INTO native_profiles(profile_digest,report,report_digest,sequence) VALUES(?1,?2,?3,(SELECT max(sequence) FROM events))",
            rusqlite::params![reference.digest, report, format!("{:x}", Sha256::digest(report.as_bytes()))]).unwrap();
        self.profile = reference;
    }
    /// Retained knowledge for `task` and its launch selection over `repository`.
    fn selection(&self, task: &str, binding: &str, repository: &Path, reason: Option<&str>) -> PathBuf {
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
    fn reserve(&self, selection: &Path) -> AttemptId {
        let drafted = self.ok(&["launch", "demo", "draft", "--selection", selection.to_str().unwrap(), "--expected-head", &self.head().to_string()]);
        let document = self.path("approval.json");
        fs::write(&document, serde_json::to_vec_pretty(&drafted["approval"]).unwrap()).unwrap();
        sign(&self.key, authority::SIGNATURE_NAMESPACE, &document);
        let approval = self.ok(&["approval", "demo", "import", document.to_str().unwrap(), self.path("approval.json.sig").to_str().unwrap(), "--expected-head", &self.head().to_string()]);
        let reservation = self.ok(&["launch", "demo", "reserve", "--selection", selection.to_str().unwrap(), "--approval-digest", approval["digest"].as_str().unwrap(), "--expected-head", &self.head().to_string()]);
        AttemptId::new(reservation["record"]["attempt"].as_str().unwrap()).unwrap()
    }
    fn serve(&mut self) {
        self.server = Some(Command::new("/usr/bin/python3").args(["-c", SERVER]).arg(self.socket()).spawn().unwrap());
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.socket().exists() { assert!(Instant::now() < deadline); std::thread::sleep(Duration::from_millis(10)); }
    }
    fn spawn(&self) -> Ticker {
        Ticker(Command::new(BIN).env_clear().env("HOME", self.home.path()).env("PATH", "/usr/bin:/bin").env("HERDR_BIN_PATH", &self.herdr)
            .args(["--root", self.root().to_str().unwrap(), "ticker", "run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap())
    }
    fn wait(&self, ticker: &mut Ticker, seconds: u64, predicate: &dyn Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(seconds);
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
    /// `ok` against a running ticker, rebuilding arguments from current state and retrying for a bounded time.
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

    // ---- the fixture history -------------------------------------------------

    /// Signed contract revision 1 of `task` (the document `body` without head/store/authority); returns its digest.
    fn install_contract(&self, task: &str, mut body: Value) -> String {
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
    fn plant_attempt(&self, task: &str, attempt: &str) {
        self.db().execute("INSERT INTO attempts(id,task_id,revision,state,snapshot,reservation,termination_observed) VALUES(?1,?2,1,'running',NULL,?1,0)", [attempt, task]).unwrap();
    }
    /// The attempt ended, as the controller records a proven termination.
    fn end_attempt(&self, attempt: &str) {
        self.db().execute("UPDATE attempts SET state='completed',termination_observed=1,revision=revision+1 WHERE id=?1", [attempt]).unwrap();
    }
    /// `result submit` of `candidate` in `repository` for `attempt`; returns the submission id.
    #[allow(clippy::too_many_arguments)]
    fn submit(&self, task: &str, attempt: &str, digest: &str, repository: &Path, base: &str, candidate: &str, outputs: &[&str], key: &str) -> String {
        let objects: Vec<Value> = self.git_in(repository, &["rev-list", "--objects", candidate]).lines()
            .map(|line| { let oid = line.split_whitespace().next().unwrap(); json!({"oid": oid, "relative_path": format!("{}/{}", &oid[..2], &oid[2..])}) }).collect();
        let file = self.path(&format!("{key}-submit.json"));
        fs::write(&file, json!({"idempotency_key": key, "task_id": task, "contract_revision": 1, "contract_digest": digest, "attempt_id": attempt,
            "repository": repository.canonicalize().unwrap().display().to_string(), "base_oid": base, "candidate_oid": candidate, "object_format": "sha256",
            "artifact_manifest": outputs.iter().map(|p| json!({"path": p, "oid": candidate})).collect::<Vec<_>>(), "claimed_checks": [], "objects": objects}).to_string()).unwrap();
        self.ok(&["result", "demo", "submit", "--input-file", file.to_str().unwrap()])["submission_id"].as_str().unwrap().to_owned()
    }
    /// `result verify` of `submission` against policy `id` whose exact text is `policy`.
    /// The recorded verdict is printed either way; a rejection also exits non-zero.
    fn verify(&self, submission: &str, id: &str, policy: &str, key: &str) -> Value {
        let file = self.path(&format!("{key}-policy.json"));
        fs::write(&file, policy).unwrap();
        let out = self.cli(&["result", "demo", "verify", submission, "--policy-id", id, "--policy-file", file.to_str().unwrap(), "--idempotency-key", key,
            "--work-dir", self.path(&format!("{key}-work")).to_str().unwrap(), "--timeout-seconds", "60"]);
        let verdict: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&out.stderr)));
        assert_eq!(out.status.success(), verdict["state"] == "accepted", "{verdict}");
        verdict
    }
    /// Commit `files` on a new branch `branch` of `repository` from `from`; returns the commit.
    fn commit_files(&self, repository: &Path, branch: &str, from: &str, files: &BTreeMap<String, String>) -> String {
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
    fn name(&mut self, value: &str, name: &str) { self.history.names.insert(value.to_owned(), name.to_owned()); }

    /// Accept every fixture change: contract, planted attempt, submission,
    /// verification and integration, each change based on the integration tip.
    fn build_history(&mut self) {
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
    fn change(&self, case: &str) -> Value {
        let task = case.strip_suffix(".r1").unwrap();
        self.history.fixture["changes"].as_array().unwrap().iter().find(|c| c["task"] == task).unwrap().clone()
    }
    /// The solution files of `case` (its changed files outside tests/).
    fn solution(&self, case: &str) -> BTreeMap<String, String> {
        self.change(case)["files"].as_object().unwrap().iter().filter(|(k, _)| !k.starts_with("tests/")).map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned())).collect()
    }
    /// Replace every recorded oid or id in `value` by its fixture name.
    fn normalize(&self, value: &Value) -> Value {
        match value {
            Value::String(s) => json!(self.history.names.get(s).cloned().unwrap_or_else(|| s.clone())),
            Value::Array(a) => Value::Array(a.iter().map(|v| self.normalize(v)).collect()),
            Value::Object(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), self.normalize(v))).collect()),
            other => other.clone(),
        }
    }
    /// Draft `task`'s replay contract, sign it as the owner and install it; returns (digest, document).
    fn install_replay_contract(&self, task: &str) -> (String, Value) { self.install_replay_contract_routed(task, None) }
    /// As `install_replay_contract`, the owner having changed the drafted route to `route`.
    fn install_replay_contract_routed(&self, task: &str, route: Option<&str>) -> (String, Value) {
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

fn contains(haystack: &[u8], needle: &str) -> bool { haystack.windows(needle.len()).any(|w| w == needle.as_bytes()) }

/// Suite v1 golden: the fixture's five accepted changes yield three eligible
/// cases (two `code`, one `docs`), one contaminated case (`secret-flag`: its
/// solution line is pasted in PROJECT.md, found by the metadata-only scan)
/// and one exclusion (`readme-typo` has no test file, so no hidden check).
/// Each case's base is the integration tip before the accepted change, its
/// reference the accepted candidate; the hidden checks' bytes live only in
/// the owner's check store outside the project, never in the store.
/// Extraction is deterministic (a second suite version records the same
/// cases), a suite version is immutable, the stratified subset is
/// reproducible and covers both strata, and a retired case leaves the draw.
/// A run is refused while the owner configuration does not hide the source
/// repository from workers.
#[test]
fn extraction_golden_contamination_counts_and_reproducible_subset() {
    let mut lab = Lab::new("unknown_usage='allow_with_warning'");
    lab.build_history();
    let extracted = lab.replay(&["extract", "--suite", "v1"]);
    let golden: Value = serde_json::from_str(&fs::read_to_string(GOLDEN).unwrap()).unwrap();
    let normalized = json!({"suite": {"suite_version": extracted["suite"]["suite_version"], "extractor": extracted["suite"]["extractor"], "exclusions": extracted["suite"]["exclusions"]},
        "cases": lab.normalize(&extracted["cases"])});
    assert_eq!(normalized, golden, "{}", serde_json::to_string_pretty(&normalized).unwrap());
    // Hidden checks: private, content-addressed, outside the project, with the test file's exact bytes.
    let checks = lab.root().join(".replay/demo/checks");
    assert_eq!(fs::metadata(&checks).unwrap().mode() & 0o777, 0o700);
    for case in extracted["cases"].as_array().unwrap() {
        let change = lab.change(case["case_id"].as_str().unwrap());
        for check in case["hidden_checks"].as_array().unwrap() {
            let path = checks.join(check["sha256"].as_str().unwrap());
            let expected = change["files"][format!("tests/expected/{}", check["target"].as_str().unwrap())].as_str().unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), expected);
            assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
            for file in ["state.db", "state.db-wal"] {
                assert!(!contains(&fs::read(lab.project.join(".state").join(file)).unwrap_or_default(), expected.trim()), "hidden check content in {file}");
            }
        }
    }
    // Deterministic: another version over the same history records the same cases.
    let again = lab.replay(&["extract", "--suite", "v1-again"]);
    assert_eq!(again["cases"], extracted["cases"]);
    assert_eq!(again["suite"]["exclusions"], extracted["suite"]["exclusions"]);
    assert!(lab.fail(&["replay", "demo", "extract", "--suite", "v1"]).contains("immutable"));
    // Reproducible stratified subset: same seed, same draw; both strata; never the contaminated case.
    let subset = |n: &str, seed: &str| lab.replay(&["subset", "--suite", "v1", "--subset", &format!("stratified:{n}"), "--seed", seed])["cases"].clone();
    let two = subset("2", "alpha");
    assert_eq!(two, subset("2", "alpha"));
    let strata: Vec<&str> = two.as_array().unwrap().iter().map(|c| if c == "usage-docs.r1" { "docs" } else { "code" }).collect();
    assert_eq!(strata.iter().filter(|s| **s == "docs").count(), 1, "{two}");
    assert_eq!(subset("9", "alpha").as_array().unwrap().len(), 3);
    assert!(!subset("9", "beta").as_array().unwrap().contains(&json!("secret-flag.r1")));
    // Retirement: the case stays readable, leaves every later draw, and is append-only.
    lab.replay(&["retire", "--suite", "v1", "--case", "farewell.r1", "--reason", "farewell test no longer applies"]);
    let shown = lab.replay(&["show", "--suite", "v1"]);
    assert_eq!((shown["cases"].as_array().unwrap().len(), &shown["retired"][0]["case_id"]), (4, &json!("farewell.r1")));
    let mut drawn: Vec<String> = subset("9", "alpha").as_array().unwrap().iter().map(|c| c.as_str().unwrap().to_owned()).collect();
    drawn.sort();
    assert_eq!(drawn, ["greet-name.r1", "usage-docs.r1"]);
    assert!(lab.fail(&["replay", "demo", "retire", "--suite", "v1", "--case", "farewell.r1", "--reason", "again"]).contains("already retired"));
    let db = lab.db();
    for sql in ["UPDATE replay_cases SET status='eligible'", "DELETE FROM replay_retirements", "UPDATE replay_suites SET exclusions='{}'"] {
        assert!(db.execute(sql, []).unwrap_err().to_string().contains("append-only"), "{sql}");
    }
    drop(db);
    // Without the owner hiding the source repository from workers, no run starts and nothing is written.
    let config = lab.path(".config/herdr-projects/config.toml");
    let pinned = fs::read_to_string(&config).unwrap();
    fs::write(&config, pinned.replace(HIDE_SOURCE, "")).unwrap();
    let before = lab.state();
    let refused = lab.fail(&["replay", "demo", "run", "--suite", "v1", "--configuration", "alpha", "--subset", "stratified:1", "--seed", "alpha", "--expected-head", &lab.head().to_string()]);
    assert!(refused.contains("does not hide source repository"), "{refused}");
    assert_eq!(lab.state(), before);
    assert_eq!(lab.db().query_row("SELECT count(*) FROM replay_runs", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
    fs::write(&config, pinned).unwrap();
}

/// Run one replay of v1 and install its drafted contract: the task is an
/// ordinary queued task whose contract is verify-only over a replay
/// repository at the case's base, with one hidden-check policy.
fn replay_one(lab: &mut Lab, configuration: &str, seed: &str, route: Option<&str>) -> (Value, String, String, Value) {
    lab.build_history();
    lab.replay(&["extract", "--suite", "v1"]);
    let run = lab.replay(&["run", "--suite", "v1", "--configuration", configuration, "--subset", "stratified:1", "--seed", seed, "--expected-head", &lab.head().to_string()]);
    let task = run["tasks"][0]["task_id"].as_str().unwrap().to_owned();
    let (digest, document) = lab.install_replay_contract_routed(&task, route);
    (run, task, digest, document)
}

/// A replay candidate is verified by its hidden check in the isolated
/// verifier (a wrong candidate fails, the reference solution passes, and a
/// hidden file changed on disk is refused as unavailable), yet it never
/// integrates: the operator's `result integrate` is refused before any write,
/// the automatic producer never enqueues it and raw SQL cannot create an
/// integration job, lease or operation for it. Nothing can depend on it:
/// `task queue` naming it as a predecessor and a raw satisfaction row are refused.
#[test]
fn replay_candidate_is_verified_by_hidden_checks_but_never_integrates_or_releases_dependents() {
    let mut lab = Lab::new("unknown_usage='allow_with_warning'");
    let (run, task, digest, document) = replay_one(&mut lab, "manual", "gamma", Some("verify_then_integrate"));
    let case = run["cases"][0].as_str().unwrap().to_owned();
    let repository = PathBuf::from(run["tasks"][0]["repository"].as_str().unwrap());
    let base = run["tasks"][0]["base_oid"].as_str().unwrap().to_owned();
    assert!(repository.starts_with(lab.root().canonicalize().unwrap().join(".replay/demo/repos/v1")));
    // The replay repository holds only the base history: neither the reference solution nor its tests.
    let shown = lab.replay(&["show", "--suite", "v1"]);
    let record = shown["cases"].as_array().unwrap().iter().find(|c| c["case_id"] == case.as_str()).unwrap().clone();
    assert_eq!(lab.git_in(&repository, &["rev-parse", "HEAD"]), base);
    let all = lab.git_in(&repository, &["rev-list", "--all"]);
    assert!(!all.contains(record["reference_oid"].as_str().unwrap()) && !all.contains(record["integrated_oid"].as_str().unwrap()));
    let tree = lab.git_in(&repository, &["ls-tree", "-r", "--name-only", "HEAD"]);
    for check in record["hidden_checks"].as_array().unwrap() {
        assert!(!tree.lines().any(|p| p == format!("tests/expected/{}", check["target"].as_str().unwrap())), "{tree}");
    }
    // The contract is verify-only with one policy per hidden check; it names the hidden file by path and digest only.
    // (The owner re-routed this one to integration before signing: the guard does not rest on the route.)
    assert!(document["non_goals"].as_str().unwrap().contains("never integrated"), "{document}");
    assert_eq!((&document["route"], &document["base_oid"], document["acceptance_policies"].as_array().unwrap().len()), (&json!("verify_then_integrate"), &json!(base), 1));
    let policy = document["acceptance_policies"][0].clone();
    let text = policy["text"].as_str().unwrap().to_owned();
    let expected = record["hidden_checks"][0]["sha256"].as_str().unwrap();
    let hidden = lab.root().canonicalize().unwrap().join(".replay/demo/checks").join(expected);
    let content = fs::read_to_string(&hidden).unwrap();
    assert!(text.contains(expected) && text.contains(hidden.to_str().unwrap()) && !text.contains(content.trim()), "{text}");
    // The task is an ordinary queued task; the run granted nothing.
    let state = lab.state();
    assert!(state.scheduler.as_ref().unwrap().queue.iter().any(|q| q.task.as_str() == task));
    assert!(state.approvals.is_empty() && state.attempts.iter().all(|a| a.task.as_str() != task));

    let attempt = format!("{task}-attempt");
    lab.plant_attempt(&task, &attempt);
    let outputs: Vec<String> = lab.solution(&case).keys().cloned().collect();
    let outputs: Vec<&str> = outputs.iter().map(String::as_str).collect();
    let wrong = lab.commit_files(&repository, "wrong", &base, &lab.solution(&case).keys().map(|k| (k.clone(), "not the expected content\n".to_owned())).collect());
    let wrong = lab.submit(&task, &attempt, &digest, &repository, &base, &wrong, &outputs, "replay-wrong");
    let rejected = lab.verify(&wrong, "hidden-1", &text, "verify-wrong");
    assert_eq!((&rejected["state"], &rejected["reason"]), (&json!("rejected"), &json!("checks_failed")), "{rejected}");
    let right = lab.commit_files(&repository, "right", &base, &lab.solution(&case));
    let right = lab.submit(&task, &attempt, &digest, &repository, &base, &right, &outputs, "replay-right");
    // A hidden file that no longer has its pinned digest is refused before any check runs.
    fs::write(&hidden, "tampered\n").unwrap();
    let unavailable = lab.verify(&right, "hidden-1", &text, "verify-tampered");
    assert_eq!((&unavailable["state"], &unavailable["reason"]), (&json!("rejected"), &json!("hidden_check_unavailable")), "{unavailable}");
    fs::write(&hidden, &content).unwrap();
    let accepted = lab.verify(&right, "hidden-1", &text, "verify-right");
    assert_eq!(accepted["state"], "accepted", "{accepted}");
    let result = accepted["receipt"]["result_id"].as_str().unwrap().to_owned();

    // Never integrates. The replay repository is the configured target's repository only for this refusal.
    lab.git_in(&repository, &["branch", "integration", &base]);
    lab.ok(&["result", "demo", "configure-integration", "--repository", repository.to_str().unwrap(), "--reference", "refs/heads/integration"]);
    let head = lab.head().to_string();
    lab.ok(&["result", "demo", "auto", "--integrate", "on", "--expected-head", &head]);
    let pending = || lab.db().query_row("SELECT count(*) FROM pending_integration_work WHERE submission_id=?1", [&right], |r| r.get::<_, i64>(0)).unwrap();
    assert_eq!(pending(), 1, "the verified replay submission enters the pending projection");
    let turn = herdr_projects::store::service_project_integration_jobs(&lab.project).unwrap();
    assert_eq!((turn.enqueued, pending()), (0, 0), "the producer drops it");
    let work = lab.path("integrate-replay");
    let refused = lab.fail(&["result", "demo", "integrate", &result, "--repository", repository.to_str().unwrap(), "--idempotency-key", "integrate-replay", "--work-dir", work.to_str().unwrap()]);
    assert!(refused.contains("a replay candidate never integrates"), "{refused}");
    assert!(!work.exists());
    assert_eq!(lab.git_in(&repository, &["rev-parse", "refs/heads/integration"]), base);
    let db = lab.db();
    let count = |sql: &str| db.query_row(sql, [&task], |r| r.get::<_, i64>(0)).unwrap();
    assert_eq!(count("SELECT count(*) FROM operations WHERE task_id=?1 AND kind IN ('integration.run','integration.lease')"), 0);
    let hash = "d".repeat(64);
    for (id, kind, payload) in [("raw-job", "integration.run", json!({"submission_id": right})), ("raw-lease", "integration.lease", json!({"result_id": result}))] {
        let error = db.execute("INSERT INTO operations(id,task_id,kind,target,payload_version,payload,payload_hash,expected_revision,due_unix_ms,idempotency_key)
            VALUES(?1,'work',?2,'refs/heads/integration',1,?3,?4,1,0,?1)", rusqlite::params![id, kind, payload.to_string(), hash]).unwrap_err();
        assert!(error.to_string().contains("a replay candidate never integrates"), "{kind}: {error}");
    }
    let error = db.execute("INSERT INTO integration_operations(operation_id,project_store,idempotency_key,payload_digest,repository,ref_name,expected_old_oid,verified_result_id,candidate_id,state,generation,object_format,checks_passed,reason,created_unix_ms)
        VALUES('raw-op',?1,'raw-op',?2,?3,'refs/heads/integration',?4,?5,NULL,'effect_pending',1,'sha256',0,NULL,1)",
        rusqlite::params![lab.store(), hash, repository.display().to_string(), base, result]).unwrap_err();
    assert!(error.to_string().contains("a replay candidate never integrates"), "{error}");
    // Never releases a dependent.
    let error = db.execute("INSERT INTO dependency_satisfactions(satisfaction_id,task_id,predecessor_task,requirement,state,evidence_kind,evidence_id,created_unix_ms)
        VALUES(?1,'work',?2,'verified_result','valid','verified_result',?3,1)", rusqlite::params![hash, task, result]).unwrap_err();
    assert!(error.to_string().contains("never releases dependents"), "{error}");
    for sql in ["UPDATE replay_candidates SET run_id=run_id", "DELETE FROM replay_candidates"] {
        assert!(db.execute(sql, []).unwrap_err().to_string().contains("append-only"), "{sql}");
    }
    drop(db);
    lab.ok(&["task", "demo", "add", "consumer", "--title", "consumer", "--expected-head", &lab.head().to_string()]);
    let request = lab.path("consumer-queue.json");
    fs::write(&request, json!({"priority": 0, "dependencies": [{"predecessor": task, "requirement": "verified_result"}]}).to_string()).unwrap();
    let refused = lab.fail(&["task", "demo", "queue", "consumer", "--input-file", request.to_str().unwrap(), "--expected-revision", "1", "--expected-head", &lab.head().to_string()]);
    assert!(refused.contains("never releases dependents"), "{refused}");
    // The source tasks integrated normally; only the replay candidate is held back.
    assert_eq!(lab.db().query_row("SELECT count(*) FROM integrated_commits", [], |r| r.get::<_, i64>(0)).unwrap(), 5);
}

/// A probe agent for a replay candidate: it tries to read each hidden check
/// file, list the owner's replay directories and the source repository, and
/// searches everything it can read (its project, its worktree and every
/// revision of its repository) for the hidden check's content. Then, as a
/// deterministic "pass" agent, it writes the solution it was built with,
/// commits, publishes `probe-1.txt` (report, `head`, `objects`), waits for
/// the owner's `submit.json` and submits through its spool (`probe-2.txt`).
const PROBE_AGENT: &str = r#"
use std::{fs, path::Path, process::Command, time::Duration};
fn publish(name: &str, text: &str) { fs::write(format!("{name}.tmp"), text).unwrap(); fs::rename(format!("{name}.tmp"), name).unwrap(); }
fn run(program: &str, args: &[&str]) -> (bool, String) {
    match Command::new(program).args(args).output() {
        Ok(out) => (out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)).replace('\n', "|")),
        Err(error) => (false, error.to_string()),
    }
}
fn walk(dir: &Path, needle: &[u8], hits: &mut Vec<String>, seen: &mut usize) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        *seen += 1;
        if *seen > 20000 { return }
        let path = entry.path();
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() { walk(&path, needle, hits, seen); }
        else if kind.is_file() {
            if let Ok(bytes) = fs::read(&path) { if bytes.windows(needle.len()).any(|w| w == needle) { hits.push(path.display().to_string()); } }
        }
    }
}
fn main() {
    if std::env::args().nth(1).as_deref() == Some("--version") { println!("2.1.0 (Claude Code)"); return }
    let mut report = String::new();
    for path in READS {
        report += &format!("read {path} {}\n", match fs::read(path) { Ok(_) => "OK".to_owned(), Err(error) => format!("ERR:{:?}", error.kind()) });
    }
    for dir in LISTS {
        let listed = fs::read_dir(dir).map(|d| { let mut n: Vec<String> = d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect(); n.sort(); n.join(",") });
        report += &format!("list {dir} {}\n", match listed { Ok(names) => format!("OK:{names}"), Err(error) => format!("ERR:{:?}", error.kind()) });
    }
    let mut hits = Vec::new();
    let mut seen = 0;
    for dir in SEARCH { walk(Path::new(dir), SENTINEL.as_bytes(), &mut hits, &mut seen); }
    walk(Path::new("."), SENTINEL.as_bytes(), &mut hits, &mut seen);
    report += &format!("hits {}\nsearched {}\n", hits.join(","), seen);
    let git = |args: &[&str]| run("/usr/bin/git", &[&["-c", "user.name=worker", "-c", "user.email=worker@example.invalid"], args].concat());
    let revisions = git(&["rev-list", "--all"]).1;
    report += &format!("reference {}\n", revisions.contains(REFERENCE));
    let grep = git(&["grep", "-F", "-l", SENTINEL.trim(), "--all-match", "HEAD"]);
    report += &format!("grep {}\n", grep.0);
    for (path, text) in SOLUTION {
        if let Some(parent) = Path::new(path).parent() { fs::create_dir_all(parent).unwrap(); }
        fs::write(path, text).unwrap();
    }
    // Control: the same search finds the content once the solution is written.
    let mut control = Vec::new();
    walk(Path::new("."), SENTINEL.as_bytes(), &mut control, &mut 0);
    report += &format!("control {}\n", control.len());
    let (added, _) = git(&["add", "-A"]);
    let (committed, output) = git(&["commit", "-qm", "replay candidate"]);
    let head = git(&["rev-parse", "HEAD"]).1.trim_end_matches('|').to_owned();
    report += &format!("commit {} {}\nhead {head}\n", added && committed, output);
    report += &format!("objects {}\n", git(&["rev-list", "--objects", &head]).1.trim_end_matches('|').split('|').map(|l| l.split(' ').next().unwrap()).collect::<Vec<_>>().join(","));
    publish("probe-1.txt", &report);
    while !Path::new("submit.json").exists() { std::thread::sleep(Duration::from_millis(50)); }
    let mut last = (false, String::new());
    for _ in 0..200 {
        last = run(BIN, &["--root", ROOT, "result", "demo", "submit", "--input-file", "submit.json"]);
        if last.0 { break }
        std::thread::sleep(Duration::from_millis(100));
    }
    publish("probe-2.txt", &format!("submitted {} {}\n", last.0, last.1));
    loop { std::thread::park() }
}
"#;

/// A replay candidate launched for real: `replay run`, the owner-signed
/// contract, a binding on the replay repository, `launch draft`, the owner's
/// signed approval and `launch reserve`, then the ticker creates the worker
/// on the Herdr stand-in. Inside its sandbox the candidate cannot read the
/// hidden check file, list the owner's replay directories or read the source
/// repository; the hidden check's content is nowhere it can read (its
/// project, worktree, retained knowledge or any revision of its repository),
/// and the reference solution is not in its repository. It still commits
/// and submits through its spool; the hidden check then passes in the
/// isolated verifier, and the report counts it for the profile's
/// configuration (dispatched by the operator with reason `replay`) under suite v1.
#[test]
fn launched_replay_candidate_cannot_read_hidden_checks_and_is_verified_by_them() {
    let mut lab = Lab::new("unknown_usage='allow_with_warning'");
    let (run, task, digest, document) = replay_one(&mut lab, "probe", "delta", None);
    let case = run["cases"][0].as_str().unwrap().to_owned();
    let repository = PathBuf::from(run["tasks"][0]["repository"].as_str().unwrap());
    let base = run["tasks"][0]["base_oid"].as_str().unwrap().to_owned();
    let record = lab.replay(&["show", "--suite", "v1"])["cases"].as_array().unwrap().iter().find(|c| c["case_id"] == case.as_str()).unwrap().clone();
    let checks = lab.root().canonicalize().unwrap().join(".replay/demo/checks");
    let hidden = checks.join(record["hidden_checks"][0]["sha256"].as_str().unwrap());
    let sentinel = fs::read_to_string(&hidden).unwrap();
    let text = document["acceptance_policies"][0]["text"].as_str().unwrap().to_owned();
    let binding = lab.bind(&task, &repository);
    lab.observe();
    let home = lab.home.path().canonicalize().unwrap();
    let quoted = |paths: &[PathBuf]| paths.iter().map(|p| format!("{:?}", p.to_str().unwrap())).collect::<Vec<_>>().join(",");
    let reads = [hidden.clone(), home.join("repo/.git/HEAD"), home.join(format!("repo/{}", lab.change(&case)["files"].as_object().unwrap().keys().next().unwrap()))];
    let lists = [checks.clone(), lab.root().canonicalize().unwrap().join(".replay/demo"), home.join("repo")];
    let search = [lab.project.canonicalize().unwrap(), repository.clone()];
    let solution: Vec<String> = lab.solution(&case).iter().map(|(k, v)| format!("({k:?}, {v:?})")).collect();
    lab.build_agent(&format!("{PROBE_AGENT}\nconst READS: &[&str] = &[{}];\nconst LISTS: &[&str] = &[{}];\nconst SEARCH: &[&str] = &[{}];\nconst SENTINEL: &str = {sentinel:?};\n\
        const REFERENCE: &str = {:?};\nconst SOLUTION: &[(&str, &str)] = &[{}];\nconst ROOT: &str = {:?};\nconst BIN: &str = {BIN:?};\n",
        quoted(&reads), quoted(&lists), quoted(&search), record["reference_oid"].as_str().unwrap(), solution.join(","), lab.root().canonicalize().unwrap().to_str().unwrap()));
    lab.prepare_profile();
    let selection = lab.selection(&task, &binding, &repository, Some("replay"));
    let attempt = lab.reserve(&selection);
    let worktree = PathBuf::from(lab.ok(&["memory", "demo", "attempt-input", "--attempt", attempt.as_str()])["worktrees"][0]["path"].as_str().unwrap());
    lab.serve();
    let mut ticker = lab.spawn();
    lab.wait(&mut ticker, 180, &|| worktree.join("probe-1.txt").exists());
    let report = fs::read_to_string(worktree.join("probe-1.txt")).unwrap();
    for path in &reads { assert!(report.contains(&format!("read {} ERR:", path.display())), "{} was readable:\n{report}", path.display()); }
    // Only the replay repository's own mount point shows under the owner's replay directory.
    for dir in &lists {
        let line = report.lines().find(|l| l.starts_with(&format!("list {} ", dir.display()))).unwrap();
        assert!(line.contains(" ERR:") || line.ends_with(" OK:") || line.ends_with(" OK:repos"), "{line}");
    }
    assert!(report.contains("\nhits \n") && report.contains("\nreference false\n") && report.contains("\ngrep false\n"), "{report}");
    let searched: usize = report.lines().find_map(|l| l.strip_prefix("searched ")).unwrap().parse().unwrap();
    assert!(searched > 20 && report.contains("\ncontrol 1\n"), "{report}");
    assert!(report.contains("\ncommit true"), "{report}");
    let candidate = report.lines().find_map(|l| l.strip_prefix("head ")).unwrap().to_owned();
    let objects: Vec<Value> = report.lines().find_map(|l| l.strip_prefix("objects ")).unwrap().split(',')
        .map(|oid| json!({"oid": oid, "relative_path": format!("{}/{}", &oid[..2], &oid[2..])})).collect();
    let outputs: Vec<Value> = lab.solution(&case).keys().map(|p| json!({"path": p, "oid": candidate})).collect();
    fs::write(worktree.join("submit.json.tmp"), json!({"idempotency_key": "replay-probe", "task_id": task, "contract_revision": 1, "contract_digest": digest,
        "attempt_id": attempt.as_str(), "repository": repository.display().to_string(), "base_oid": base, "candidate_oid": candidate, "object_format": "sha256",
        "artifact_manifest": outputs, "claimed_checks": [], "objects": objects}).to_string()).unwrap();
    fs::rename(worktree.join("submit.json.tmp"), worktree.join("submit.json")).unwrap();
    lab.wait(&mut ticker, 60, &|| worktree.join("probe-2.txt").exists());
    let submitted = fs::read_to_string(worktree.join("probe-2.txt")).unwrap();
    assert!(submitted.starts_with("submitted true"), "{submitted}");
    let submission = lab.ok_live(&|| ["result", "demo", "show"].map(String::from).to_vec()).as_array().unwrap().iter()
        .find(|s| s["task_id"] == task.as_str()).unwrap()["submission_id"].as_str().unwrap().to_owned();
    let policy = lab.path("hidden-policy.json");
    fs::write(&policy, &text).unwrap();
    let tries = std::cell::Cell::new(0);
    let verified = lab.ok_live(&|| { tries.set(tries.get() + 1); ["result", "demo", "verify", &submission, "--policy-id", "hidden-1", "--policy-file", policy.to_str().unwrap(),
        "--idempotency-key", "replay-probe-verify", "--work-dir", &lab.path(&format!("probe-verify-{}", tries.get())).display().to_string()].map(String::from).to_vec() });
    assert_eq!(verified["state"], "accepted", "{verified}");
    // The launch was the operator's, with the ordinary budget reservation and reason `replay`.
    let (configuration, chooser, reasons): (String, String, String) = lab.db().query_row("SELECT chosen_configuration_id,chooser_kind,reason_codes FROM dispatch_decisions WHERE attempt_id=?1",
        [attempt.as_str()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
    assert_eq!((chooser.as_str(), reasons.as_str()), ("operator", r#"["replay"]"#));
    let report = lab.replay(&["report", "--suite", "v1"]);
    assert_eq!(report["configurations"], json!([{"configuration_id": configuration, "labels": ["probe"], "suite_version": "v1", "numerator": 1, "denominator": 1,
        "value": "1/1", "n": 1, "failed": 0, "pending": 0, "by_stratum": {record["stratum"].as_str().unwrap(): {"numerator": 1, "denominator": 1, "value": "1/1"}}}]));
    lab.ok_live(&|| ["task", "demo", "cancel-attempt", attempt.as_str(), "--expected-revision", &lab.state().attempts.iter().find(|a| a.id == attempt).unwrap().revision.to_string(),
        "--expected-head", &lab.head().to_string(), "--reason", "probe done"].map(String::from).to_vec());
    lab.wait(&mut ticker, 60, &|| lab.state().attempts.iter().find(|a| a.id == attempt).unwrap().termination_observed);
    lab.stop(ticker);
    // Never integrated: the source repository and the replay repository are unchanged.
    assert_eq!(lab.git_in(&repository, &["rev-parse", "refs/heads/main"]), base);
    assert_eq!(lab.db().query_row("SELECT count(*) FROM integrated_commits", [], |r| r.get::<_, i64>(0)).unwrap(), 5);
}

/// Budget and authority are those of ordinary work. `replay run` writes
/// only ordinary tasks and queue entries (no approval, attempt, reservation or
/// operation). A replay contract installs only with the owner's signature.
/// Under a profile budget the brief exceeds, `launch draft` refuses the replay
/// task with exactly the ordinary task's refusal, before any approval.
#[test]
fn replay_work_has_the_budget_and_authority_of_ordinary_work() {
    let mut lab = Lab::new("soft_input_tokens=500\nunknown_usage='allow_with_warning'");
    lab.build_history();
    lab.replay(&["extract", "--suite", "v1"]);
    let before = lab.state();
    let run = lab.replay(&["run", "--suite", "v1", "--configuration", "budget", "--subset", "stratified:1", "--seed", "gamma", "--expected-head", &lab.head().to_string()]);
    let task = run["tasks"][0]["task_id"].as_str().unwrap().to_owned();
    let after = lab.state();
    assert_eq!((&after.approvals, &after.attempts, &after.operations, &after.deliveries, &after.ownership), (&before.approvals, &before.attempts, &before.operations, &before.deliveries, &before.ownership));
    let added: Vec<&str> = after.tasks.iter().filter(|t| !before.tasks.iter().any(|b| b.id == t.id)).map(|t| t.id.as_str()).collect();
    assert_eq!(added, [task.as_str()]);
    // Unsigned, or signed by another key: refused, nothing installed.
    let document = lab.path("replay-contract.json");
    lab.replay(&["contract", &task, "--output", document.to_str().unwrap()]);
    let stranger = lab.path("stranger");
    assert!(Command::new("/usr/bin/ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&stranger).output().unwrap().status.success());
    sign(&stranger, authority::CONTRACT_SIGNATURE_NAMESPACE, &document);
    lab.fail(&["task", "demo", "contract", "put", "--input-file", document.to_str().unwrap(), "--signature", document.with_extension("json.sig").to_str().unwrap()]);
    assert_eq!(lab.db().query_row("SELECT count(*) FROM task_contracts WHERE task_id=?1", [&task], |r| r.get::<_, i64>(0)).unwrap(), 0);
    lab.install_replay_contract(&task);
    // Same budget refusal as the ordinary task, before any approval.
    let repository = PathBuf::from(run["tasks"][0]["repository"].as_str().unwrap());
    let binding = lab.bind(&task, &repository);
    lab.observe();
    lab.prepare_profile();
    let draft = |selection: &Path| lab.fail(&["launch", "demo", "draft", "--selection", selection.to_str().unwrap(), "--expected-head", &lab.head().to_string()]);
    let replay_refusal = draft(&lab.selection(&task, &binding, &repository, Some("replay")));
    let work_binding = lab.state().runtime_bindings.iter().find(|b| b.task.as_ref().is_some_and(|t| t.as_str() == "work")).unwrap().id.clone();
    let work_refusal = draft(&lab.selection("work", &work_binding, &lab.repo.canonicalize().unwrap(), None));
    for refusal in [&replay_refusal, &work_refusal] { assert!(refusal.contains("exceeding the captured budget"), "{refusal}"); }
    assert!(lab.state().approvals.is_empty() && lab.state().attempts.iter().all(|a| a.task.as_str() != task));
}

/// Suite v1 report comparing two configurations on the same reproducible
/// subset (all three eligible cases): configuration `alpha`'s fake agent
/// writes each case's solution, `beta`'s writes a wrong file. Each replay
/// task's attempt is dispatched with its configuration (planted as the
/// reservation writes it), submits through `result submit` and is verified
/// by its hidden check. By hand: alpha 3/3 (code 2/2, docs 1/1), beta 0/3;
/// excluded: one contaminated case, one source without a hidden check.
/// `telemetry query M49` serves the latest suite's pooled 3/6 with the
/// per-configuration cells, and the registry reports M49 active.
#[test]
fn report_compares_two_configurations_with_suite_version() {
    let mut lab = Lab::new("unknown_usage='allow_with_warning'");
    lab.build_history();
    lab.replay(&["extract", "--suite", "v1"]);
    // Two content-addressed agent configurations, as the dispatch log records them.
    let mut configurations = BTreeMap::new();
    for name in ["alpha", "beta"] {
        let canonical = json!({"schema": "agent_configuration.v1", "agent_digest": format!("{:x}", Sha256::digest(name.as_bytes())), "kind": "claude"}).to_string();
        let id = format!("sha256:{:x}", Sha256::digest(canonical.as_bytes()));
        lab.db().execute("INSERT INTO agent_configurations(configuration_id,canonical_json,first_decided_unix_ms) VALUES(?1,?2,1)", rusqlite::params![id, canonical]).unwrap();
        configurations.insert(name, id);
    }
    for name in ["alpha", "beta"] {
        let run = lab.replay(&["run", "--suite", "v1", "--configuration", name, "--subset", "stratified:3", "--seed", "compare", "--expected-head", &lab.head().to_string()]);
        assert_eq!(run["cases"].as_array().unwrap().len(), 3);
        for (index, planned) in run["tasks"].as_array().unwrap().iter().enumerate() {
            let task = planned["task_id"].as_str().unwrap();
            let case = planned["case_id"].as_str().unwrap();
            let repository = PathBuf::from(planned["repository"].as_str().unwrap());
            let base = planned["base_oid"].as_str().unwrap();
            let (digest, document) = lab.install_replay_contract(task);
            let attempt = format!("{task}-attempt");
            lab.plant_attempt(task, &attempt);
            lab.db().execute("INSERT INTO dispatch_decisions(attempt_id,task_id,task_revision,contract_revision,chosen_configuration_id,eligible,chooser_kind,chooser_principal,reason_codes,decided_unix_ms)
                VALUES(?1,?2,1,1,?3,json_array(?3),'operator','operator:cli','[\"replay\"]',?4)", rusqlite::params![attempt, task, configurations[name], 1_000 + index as i64]).unwrap();
            let files = match name { "alpha" => lab.solution(case), _ => lab.solution(case).keys().map(|k| (k.clone(), "a different greeting entirely\n".to_owned())).collect() };
            let candidate = lab.commit_files(&repository, &format!("{name}-{index}"), base, &files);
            let outputs: Vec<String> = files.keys().cloned().collect();
            let submission = lab.submit(task, &attempt, &digest, &repository, base, &candidate, &outputs.iter().map(String::as_str).collect::<Vec<_>>(), &format!("{task}-submit"));
            for policy in document["acceptance_policies"].as_array().unwrap() {
                let verdict = lab.verify(&submission, policy["id"].as_str().unwrap(), policy["text"].as_str().unwrap(), &format!("{task}-{}", policy["id"].as_str().unwrap()));
                assert_eq!(verdict["state"], if name == "alpha" { "accepted" } else { "rejected" }, "{verdict}");
            }
            lab.end_attempt(&attempt);
        }
    }
    let report = lab.replay(&["report", "--suite", "v1"]);
    let cell = |id: &str, labels: &str, p: u64, n: u64, code: Value, docs: Value| json!({"configuration_id": id, "labels": [labels], "suite_version": "v1",
        "numerator": p, "denominator": n, "value": format!("{p}/{n}"), "n": n, "failed": n - p, "pending": 0, "by_stratum": {"code": code, "docs": docs}});
    let mut expected = vec![cell(&configurations["alpha"], "alpha", 3, 3, json!({"numerator": 2, "denominator": 2, "value": "2/2"}), json!({"numerator": 1, "denominator": 1, "value": "1/1"})),
        cell(&configurations["beta"], "beta", 0, 3, json!({"numerator": 0, "denominator": 2, "value": "0/2"}), json!({"numerator": 0, "denominator": 1, "value": "0/1"}))];
    expected.sort_by_key(|c| c["configuration_id"].as_str().unwrap().to_owned());
    assert_eq!(report["configurations"], json!(expected));
    assert_eq!((&report["suite_version"], &report["metric"], &report["definition"]), (&json!("v1"), &json!("M49"), &json!("M49.v1")));
    assert_eq!(report["exclusions"]["extraction"], json!({"sources": 5, "no_hidden_check": 1, "no_solution_paths": 0, "hidden_check_too_large": 0, "contaminated": 1}));
    assert_eq!((&report["exclusions"]["contaminated_cases"], &report["cases"]), (&json!(1), &json!({"recorded": 4, "eligible": 3, "retired": 0})));
    assert_eq!(report["uncertainty"]["method"], "raw_with_n");
    // M49 through the registry and the query service.
    let registry = lab.ok(&["telemetry", "demo", "metrics", "registry", "--json"]);
    let m49 = registry["metrics"].as_array().unwrap().iter().find(|m| m["id"] == "M49").unwrap().clone();
    assert_eq!((&m49["active"], &m49["definition"]), (&json!(true), &json!("M49.v1")));
    let query = lab.ok(&["telemetry", "demo", "query", "--metric", "M49", "--json"]);
    let text = query.to_string();
    assert!(text.contains(r#""value":"3/6""#) && text.contains(&configurations["alpha"]) && text.contains(r#""suite_version":"v1""#), "{query}");
}

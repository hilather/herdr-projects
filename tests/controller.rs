#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! The canonical controller through `ticker run`: waits, notifications,
//! signed routines (also scheduled directly with `routine-store schedule`)
//! and automatic admission over a disposable migrated
//! project, set up through the compiled CLI. A Herdr bridge stand-in answers
//! `notification.show` and logs every request. Each ticker pass republishes
//! the executor metrics file, so waits count passes, never elapsed time.
use herdr_projects::{authority, domain::*, migration, operations::DeliveryState, runtime};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{fs, os::unix::fs::{MetadataExt, PermissionsExt}, path::PathBuf, process::{Child, Command, Output, Stdio}, time::{Duration, Instant}};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-projects");

/// Answers the bridge check and `notification.show`, logging each request
/// and the time of each show. While `$HOME/busy` exists, one show is answered
/// as not shown (`busy`) and the file is removed. While `$HOME/fail` exists a
/// show fails without a reply. Each show logs to `during` the delivery states
/// the project store holds at that moment.
const HERDR: &str = "#!/usr/bin/python3\nimport json,os,sqlite3,sys,time\nhome=os.environ['HOME']\n\
if sys.argv[1:]==['--version']:print('herdr 0.9.1');sys.exit(0)\n\
if sys.argv[1:]==['remote-api-bridge','--check']:print('herdr-api-bridge-v1');sys.exit(0)\n\
if sys.argv[1:]!=['remote-api-bridge']:sys.exit(2)\n\
r=json.loads(sys.stdin.readline())\nopen(home+'/requests','a').write(r['method']+'\\n')\n\
if r['method']!='notification.show':sys.exit(3)\n\
open(home+'/shows','a').write(str(int(time.time()*1000))+'\\n')\n\
db=sqlite3.connect('file:'+home+'/root/demo/.state/state.db?mode=ro',uri=True)\n\
open(home+'/during','a').write(','.join(s for (s,) in db.execute('SELECT state FROM operation_delivery ORDER BY operation_id'))+'\\n')\n\
if os.path.exists(home+'/fail'):sys.exit(1)\n\
busy=os.path.exists(home+'/busy')\n\
if busy:os.remove(home+'/busy')\n\
print(json.dumps({'id':r['id'],'result':{'type':'notification_show','shown':not busy,'reason':'busy' if busy else 'shown'}}))\n";

struct Lab { home: tempfile::TempDir, project: PathBuf, key: PathBuf, _socket: std::os::unix::net::UnixListener }
struct Ticker(Child);
impl Drop for Ticker { fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); } }

impl Lab {
    /// A paused migrated project `demo` with an owner key and one unseen
    /// inbox item; `config` is
    /// appended to the owner's configuration.
    fn new(config: &str) -> Self {
        let home = tempfile::tempdir().unwrap();
        let key = home.path().join("owner");
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&key).output().unwrap().status.success());
        let socket = std::os::unix::net::UnixListener::bind(home.path().join("session.sock")).unwrap();
        let lab = Lab { project: home.path().join("root/demo"), key, home, _socket: socket };
        for command in ["new", "pause"] { lab.ok(&[command, "demo"]); }
        let public = fs::read_to_string(lab.key.with_extension("pub")).unwrap().split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        let path = lab.config();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, format!("[authority]\nversion=1\nrevision=1\napproval_public_key={public:?}\n{}", config.replace("PROJECT", &format!("{:?}", lab.project.display().to_string())))).unwrap();
        fs::write(lab.path("herdr"), HERDR).unwrap();
        fs::set_permissions(lab.path("herdr"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(lab.project.join("inbox/message.md"), "+++\nid='message'\nsummary='private text'\n+++\n").unwrap();
        migration::apply(&lab.project, &migration::inspect_with_config(&lab.project, &path).unwrap(), true).unwrap();
        lab
    }
    /// Set the paused project active under the owner's configuration.
    fn activate(&self) {
        let s = self.state();
        runtime::set_state(&self.project, s.head, s.control.unwrap().revision, ProjectState::Active, &self.config()).unwrap();
    }
    fn path(&self, name: &str) -> PathBuf { self.home.path().join(name) }
    fn config(&self) -> PathBuf { self.path(".config/herdr-projects/config.toml") }
    fn store(&self) -> PathBuf { self.project.join(".state/state.db") }
    fn cli(&self, args: &[&str]) -> Output {
        Command::new(BIN).env_clear().env("HOME", self.home.path()).env("PATH", "/usr/bin:/bin")
            .args(["--root", self.path("root").to_str().unwrap()]).args(args).output().unwrap()
    }
    fn ok(&self, args: &[&str]) -> Value {
        let out = self.cli(args);
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
    }
    /// The store may be mid-publication under a running ticker; read again.
    fn state(&self) -> Snapshot {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop { match runtime::snapshot(&self.project) { Ok(s) => return s, Err(e) => { assert!(Instant::now() < deadline, "{e:#}"); std::thread::sleep(Duration::from_millis(20)); } } }
    }
    fn head(&self) -> String { self.state().head.to_string() }
    fn set_state(&self, state: &str) {
        let control = self.state().control.unwrap().revision.to_string();
        self.ok(&["runtime", "demo", "state", state, "--expected-revision", &control, "--expected-head", &self.head()]);
    }
    fn add(&self, task: &str) { self.ok(&["task", "demo", "add", task, "--title", task, "--expected-head", &self.head()]); }
    fn count(&self, sql: &str) -> i64 { rusqlite::Connection::open(self.store()).unwrap().query_row(sql, [], |row| row.get(0)).unwrap() }
    /// Owner-sign `bytes` written to `name`; returns (document, signature).
    fn sign(&self, name: &str, bytes: &[u8], namespace: &str) -> (String, String) {
        let path = self.path(name);
        fs::write(&path, bytes).unwrap();
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-Y", "sign", "-f"]).arg(&self.key).args(["-n", namespace]).arg(&path).output().unwrap().status.success());
        (path.display().to_string(), format!("{}.sig", path.display()))
    }
    /// Record a coordinator route to the lab session (while paused).
    fn route(&self) {
        runtime::create_binding(&self.project, None, None, self.state().head, &RuntimeRoute { socket: self.path("session.sock").display().to_string(), ..Default::default() }).unwrap();
        self.ok(&["reconcile", "demo", "--record"]);
    }
    /// Queue a coordinator notification about the new task `task`.
    fn notify(&self, task: &str) -> OperationId {
        self.add(task);
        OperationId::new(self.ok(&["operations", "demo", "notify", task, "--expected-head", &self.head()])["id"].as_str().unwrap()).unwrap()
    }
    fn delivery(&self, operation: &OperationId) -> herdr_projects::operations::Delivery {
        self.state().deliveries.into_iter().find(|d| &d.operation == operation).unwrap()
    }
    fn shown(&self) -> usize { fs::read_to_string(self.path("requests")).unwrap_or_default().lines().filter(|l| *l == "notification.show").count() }
    fn log(&self) -> String { fs::read_to_string(self.path("root/.ticker.log")).unwrap_or_default() }
    fn spawn(&self) -> Ticker {
        Ticker(Command::new(BIN).env_clear().env("HOME", self.home.path()).env("PATH", "/usr/bin:/bin").env("HERDR_BIN_PATH", self.path("herdr"))
            .args(["--root", self.path("root").to_str().unwrap(), "ticker", "run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap())
    }
    fn wait(&self, ticker: &mut Ticker, predicate: &dyn Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !predicate() {
            assert!(ticker.0.try_wait().unwrap().is_none(), "ticker exited");
            assert!(Instant::now() < deadline, "{}", self.log());
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn stop(&self, mut ticker: Ticker) {
        let stop = self.path("root/.ticker.stop");
        fs::write(&stop, b"").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while ticker.0.try_wait().unwrap().is_none() { assert!(Instant::now() < deadline); std::thread::sleep(Duration::from_millis(10)); }
        fs::remove_file(&stop).unwrap();
    }
    /// Wait for `passes` more completed passes of `ticker`.
    fn passes(&self, ticker: &mut Ticker, passes: usize) {
        let metrics = self.path("root/.ticker-metrics.json");
        let inode = || fs::metadata(&metrics).map(|m| m.ino()).ok();
        for _ in 0..passes {
            let last = inode();
            self.wait(ticker, &|| inode() != last);
        }
    }
    /// Run a fresh ticker until `until` holds and `passes` passes more are done.
    fn run(&self, passes: usize, until: &dyn Fn() -> bool) {
        let mut ticker = self.spawn();
        self.wait(&mut ticker, until);
        self.passes(&mut ticker, passes);
        self.stop(ticker);
    }
}

/// Replaces `controller_services_a_wait_without_reserving_or_repeating_the_notice`.
///
/// A wait whose deadline has passed is woken by the first pass and notified
/// once: later passes and a restarted ticker add no notice, and servicing
/// it reserves no attempt and prompts nobody.
#[test]
fn ticker_notifies_an_expired_wait_once_and_reserves_nothing() {
    let lab = Lab::new("");
    lab.activate();
    lab.add("waiting");
    let wait = lab.ok(&["plan", "wait", "demo", "register", "--task", "waiting", "--condition", "user_decision", "--deadline", "2000-01-01T00:00:00Z"]);
    let id = wait["wait_id"].as_str().unwrap().to_owned();
    let notices = || lab.count(&format!("SELECT count(*) FROM events WHERE kind='wait.notified' AND entity='{id}'"));
    lab.run(2, &|| notices() == 1);
    lab.run(1, &|| true);
    assert_eq!(notices(), 1);
    let inbox: Vec<_> = lab.state().inbox.into_iter().filter(|i| i.content.kind == "wait-wake").collect();
    assert_eq!(inbox.len(), 1, "{inbox:?}");
    assert!(inbox[0].content.body.contains(&id) && inbox[0].content.body.contains("deadline expired"), "{:?}", inbox[0].content.body);
    assert!(lab.state().attempts.is_empty());
    assert!(!lab.path("requests").exists(), "servicing a wait touched Herdr");
}

/// Replaces `paused_control_blocks_notification_and_execution_lease_blocks_the_pass`
/// and `ticker_delivers_accepted_notification_under_leadership_and_restart_does_not_replay`.
///
/// While another command holds the root's exclusive execution lease the
/// ticker leaves a queued notification pending and shows nothing. Released,
/// it shows it once, and a restarted ticker does not show it again. In another
/// project, a notification queued before the project is paused stays pending.
#[test]
fn ticker_delivers_a_notification_once_only_while_active_and_unleased() {
    let lab = Lab::new("");
    lab.route();
    lab.activate();
    let op = lab.notify("notify");
    let before = lab.state();
    {
        let lease = fs::File::options().create(true).truncate(false).read(true).write(true).open(lab.path("root/.execution.lock")).unwrap();
        lease.lock().unwrap();
        lab.run(2, &|| true);
    }
    let after = lab.state();
    assert_eq!((&after.deliveries, &after.operations, &after.attempts, &after.control), (&before.deliveries, &before.operations, &before.attempts, &before.control));
    assert_eq!(lab.shown(), 0);

    lab.run(0, &|| lab.delivery(&op).state == DeliveryState::Confirmed);
    lab.run(2, &|| true);
    assert_eq!((lab.delivery(&op).attempts, lab.shown()), (1, 1));

    let paused = Lab::new("");
    paused.route();
    paused.activate();
    let op = paused.notify("notify");
    paused.set_state("paused");
    paused.run(2, &|| true);
    assert_eq!((paused.delivery(&op).state, paused.delivery(&op).attempts, paused.shown()), (DeliveryState::Pending, 0, 0));
}

/// Replaces `ticker_rotates_signed_routines_records_once_and_keeps_future_work_alive`.
///
/// Two owner-signed routines are due; one script is edited after approval.
/// The ticker refuses the edited one without running it, still runs the
/// other once and delivers a queued notification. A restart runs nothing
/// again.
#[test]
fn ticker_runs_an_approved_routine_once_beside_one_edited_after_approval() {
    let lab = Lab::new("[safety.PROJECT]\nroutine_commands=true\n");
    lab.route();
    lab.activate();
    let project = lab.project.canonicalize().unwrap();
    for (name, script) in [("a-broken", "printf ran >> BROKEN_MARKER\n"), ("b-healthy", "printf once >> HEALTHY_MARKER\n")] {
        let path = project.join(format!("{name}.sh"));
        fs::write(&path, script).unwrap();
        let definition = RoutineDefinition { version: 1, name: name.into(), revision: 1, project_store: lab.store().canonicalize().unwrap().display().to_string(),
            authority: authority::policy_reference(&project).unwrap(), config: migration::config_reference(&lab.config()).unwrap(), enabled: true,
            schedule: "every 1h".into(), timezone: "UTC".into(), start_unix_ms: jiff::Timestamp::now().as_millisecond() - 1000, missed: MissedRunPolicy::CoalesceLatest,
            overlap: OverlapPolicy::Skip, script: path.display().to_string(), script_sha256: format!("{:x}", Sha256::digest(script)), cwd: project.display().to_string(),
            deadline_ms: 1000, output_cap_bytes: 4000 };
        let (doc, sig) = lab.sign(&format!("{name}.json"), &serde_json::to_vec(&definition).unwrap(), authority::ROUTINE_SIGNATURE_NAMESPACE);
        lab.ok(&["routine-store", "demo", "import", &doc, &sig, "--expected-head", &lab.head()]);
    }
    fs::write(project.join("a-broken.sh"), "printf edited >> BROKEN_MARKER\n").unwrap();
    let op = lab.notify("notify");
    let ran = || lab.state().routine_receipts.len();
    lab.run(1, &|| ran() == 1 && lab.delivery(&op).state == DeliveryState::Confirmed);
    let state = lab.state();
    assert_eq!(state.routine_occurrences.iter().map(|o| o.routine.id.as_str()).collect::<Vec<_>>(), ["routine-b-healthy"]);
    assert!(state.routine_receipts[0].succeeded);
    assert!(lab.log().contains("a-broken"), "{}", lab.log());
    lab.run(2, &|| true);
    let after = lab.state();
    assert_eq!((after.routine_occurrences, after.routine_receipts), (state.routine_occurrences, state.routine_receipts));
    assert_eq!(fs::read(project.join("HEALTHY_MARKER")).unwrap(), b"once");
    assert!(!project.join("BROKEN_MARKER").exists());
    assert_eq!(lab.shown(), 1);
}

/// Replaces `poll_reserves_one_dependent_only_when_factory_admission_is_already_on`.
///
/// A queued dependent whose predecessor has an accepted verification, worker
/// knowledge and an owner-signed launch approval is reserved by the ticker
/// only once factory admission is on, and then exactly once.
#[test]
fn ticker_reserves_a_ready_dependent_once_only_with_factory_admission_on() {
    const POLICY: &str = r#"{"version":1,"checks":["/usr/bin/git","diff","--quiet"]}"#;
    let lab = Lab::new("[profiles.worker]\nkind='codex'\npermission_policy='interactive'\n[profiles.worker.budget]\nmax_wall_seconds=60\nunknown_usage='allow_with_warning'\n");
    let repo = lab.path("repo");
    fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| -> String {
        let out = Command::new("/usr/bin/git").env_clear().env("PATH", "/usr/bin:/bin").env("HOME", lab.home.path())
            .env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_AUTHOR_NAME", "fixture").env("GIT_AUTHOR_EMAIL", "fixture@example.com")
            .env("GIT_COMMITTER_NAME", "fixture").env("GIT_COMMITTER_EMAIL", "fixture@example.com").current_dir(&repo).args(args).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    };
    git(&["init", "-q", "--object-format=sha256"]);
    git(&["commit", "-q", "--allow-empty", "-m", "base"]);
    let (base, repo) = (git(&["rev-parse", "HEAD"]), repo.canonicalize().unwrap());
    let store = lab.store().canonicalize().unwrap().display().to_string();

    lab.add("pred");
    lab.add("dep");
    let request = lab.path("queue.json");
    fs::write(&request, json!({"priority":0,"dependencies":[{"predecessor":"pred","requirement":"verified_result"}]}).to_string()).unwrap();
    lab.ok(&["task", "demo", "queue", "dep", "--input-file", request.to_str().unwrap(), "--expected-revision", "1", "--expected-head", &lab.head()]);
    let policy = lab.state().scheduler.unwrap().policy.revision.to_string();
    lab.ok(&["scheduler", "demo", "policy", "--max-active-workers", "1", "--max-attempts-per-task", "3", "--expected-revision", &policy, "--expected-head", &lab.head()]);
    let dep = TaskId::new("dep").unwrap();
    let state = lab.state();
    let binding = runtime::create_binding(&lab.project, Some(&dep), Some(state.tasks.iter().find(|t| t.id == dep).unwrap().revision), state.head,
        &RuntimeRoute { socket: lab.path("session.sock").display().to_string(), cwd: repo.display().to_string(), ..Default::default() }).unwrap();
    migration::open_active(&lab.project).unwrap().record_observations(lab.state().head, &[herdr_projects::reconcile::RuntimeObservation {
        binding: binding.binding.id.clone(), binding_revision: binding.binding.revision, task_revision: binding.task_revision,
        observed_unix_ms: jiff::Timestamp::now().as_millisecond(), collector: "herdr-git-v2".into(),
        config_digest: migration::config_reference(&lab.config()).unwrap().digest, ..Default::default() }]).unwrap();
    lab.activate();
    let profile = launchable_profile(&lab);

    // The predecessor's worker submits and the signed policy accepts it.
    let plant = |state: AttemptState, terminated: bool| {
        let mut db = migration::open_active(&lab.project).unwrap();
        let expected = lab.state().attempts.into_iter().find(|a| a.id.as_str() == "pred-attempt").map(|a| a.revision);
        let next = Attempt { id: AttemptId::new("pred-attempt").unwrap(), task: TaskId::new("pred").unwrap(), revision: expected.map_or(1, |r| r + 1), state,
            snapshot: None, reservation: "pred-slot".into(), termination_observed: terminated };
        db.commit(Commit { expected_head: db.current_head().unwrap(), mutations: vec![Mutation::Attempt { expected, next }] }).unwrap();
    };
    plant(AttemptState::Running, false);
    let contract = json!({"version":1,"project_store":store,"expected_head":lab.state().head,"task_id":"pred","contract_revision":1,
        "deliverable":"predecessor","non_goals":"no worker launch","acceptance_policies":[{"id":"builds","text":POLICY}],
        "repository":repo,"base_oid":base,"object_format":"sha256","dependencies":[],"scope":{"paths":[],"named_resources":[]},
        "capability_flags":[],"profile_kind":"codex","retry_class":"none","result_schema_id":"result-v1","route":"verify_only",
        "authority":authority::policy_reference(&lab.project).unwrap()});
    let (doc, sig) = lab.sign("contract.json", &serde_json::to_vec(&contract).unwrap(), authority::CONTRACT_SIGNATURE_NAMESPACE);
    let digest = lab.ok(&["task", "demo", "contract", "put", "--input-file", &doc, "--signature", &sig])["digest"].clone();
    let objects: Vec<_> = git(&["rev-list", "--objects", "--all"]).lines().map(|line| {
        let oid = line.split_whitespace().next().unwrap();
        json!({"oid":oid,"relative_path":format!("{}/{}", &oid[..2], &oid[2..])})
    }).collect();
    let submission = lab.path("submission.json");
    fs::write(&submission, json!({"idempotency_key":"pred-1","task_id":"pred","contract_revision":1,"contract_digest":digest,"attempt_id":"pred-attempt",
        "repository":repo,"base_oid":base,"candidate_oid":base,"object_format":"sha256","artifact_manifest":[],"claimed_checks":[],"objects":objects}).to_string()).unwrap();
    let submitted = lab.ok(&["result", "demo", "submit", "--input-file", submission.to_str().unwrap()]);
    fs::write(lab.path("policy.json"), POLICY).unwrap();
    let run = lab.ok(&["result", "demo", "verify", submitted["submission_id"].as_str().unwrap(), "--policy-id", "builds", "--policy-file", lab.path("policy.json").to_str().unwrap(),
        "--idempotency-key", "pred-verify", "--work-dir", lab.path("verify-work").to_str().unwrap()]);
    assert_eq!(run["state"], "accepted", "{run}");
    plant(AttemptState::Completed, true);

    // Worker knowledge and an owner-signed launch approval for the dependent,
    // without repositories, as automatic admission prepares it.
    let scope = lab.path("scope.json");
    fs::write(&scope, json!({"schema_version":1,"task_id":"dep","profile":"worker","domains":[],"paths":[],"pinned_keys":[],"sensitivity":"default"}).to_string()).unwrap();
    let snapshot = lab.ok(&["memory", "demo", "snapshot", "--task", "dep", "--profile", "worker", "--input-file", scope.to_str().unwrap(), "--worker"]);
    let selection = lab.path("selection.json");
    fs::write(&selection, json!({"task":"dep","binding":binding.binding.id,"profile":profile,
        "knowledge":{"id":snapshot["id"],"revision":1,"digest":snapshot["manifest_hash"]},"repositories":[]}).to_string()).unwrap();
    let drafted = lab.ok(&["launch", "demo", "draft", "--selection", selection.to_str().unwrap(), "--expected-head", &lab.head()]);
    let (doc, sig) = lab.sign("approval.json", &serde_json::to_vec_pretty(&drafted["approval"]).unwrap(), authority::SIGNATURE_NAMESPACE);
    lab.ok(&["approval", "demo", "import", &doc, &sig, "--expected-head", &lab.head()]);

    let reserved = || lab.state().attempts.iter().filter(|a| a.task == dep).count();
    lab.run(2, &|| true);
    assert_eq!(reserved(), 0, "admission is off: {}", lab.log());
    // Fixture only: automatic admission is enabled by a signed factory manifest.
    rusqlite::Connection::open(lab.store()).unwrap().execute("UPDATE project_control SET factory_admission='on' WHERE singleton=1", []).unwrap();
    lab.run(2, &|| reserved() == 1);
    assert_eq!(reserved(), 1);
    assert_eq!(lab.state().attempts.len(), 2, "no other task gained an attempt");
    let logged: Vec<Value> = lab.log().lines().filter_map(|line| line.find('{').and_then(|at| serde_json::from_str(&line[at..]).ok())).collect();
    assert!(logged.iter().any(|line| line["reason"] == "reserved"), "{}", lab.log());
}

/// Prepare `worker` over fake binaries; only the native interaction evidence,
/// which needs a real agent session, is planted.
fn launchable_profile(lab: &Lab) -> VersionedReference {
    use herdr_projects::worker_supervision::{ProcessIncarnation, SupervisorIdentity};
    let (bin, agent_home) = (lab.path("bin"), lab.path("agent-home"));
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&agent_home).unwrap();
    let (herdr, agent) = (bin.join("herdr"), bin.join("codex"));
    fs::write(&herdr, "#!/usr/bin/python3\nimport sys,json\nif sys.argv[1:]==['--version']:print('herdr 0.9.1');sys.exit(0)\nr=json.loads(sys.stdin.readline())\nprint(json.dumps({'id':r['id'],'result':{'type':'pong','version':'0.9.1','capabilities':{'workspace_create_command':True}}}))\n").unwrap();
    fs::write(&agent, "#!/bin/sh\n[ \"$#\" = 1 ] && [ \"$1\" = --version ] || exit 2\nprintf '%s\\n' 'codex-cli 0.154.0'\n").unwrap();
    for path in [&herdr, &agent] { fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap(); }
    let prepared = lab.ok(&["profile", "prepare", "demo", "worker", "--herdr-executable", herdr.to_str().unwrap(),
        "--agent-executable", agent.to_str().unwrap(), "--execution-home", agent_home.to_str().unwrap()]);
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
    let store = lab.store().canonicalize().unwrap();
    let metadata = fs::metadata(&store).unwrap();
    let report = json!({"preparation":{"profile":profile,"reference":reference,"launchable":true,"protocol_capable":false,"certified":false},
        "evidence":evidence,"source_store":[store,metadata.dev(),metadata.ino()]}).to_string();
    rusqlite::Connection::open(&store).unwrap().execute("INSERT INTO native_profiles(profile_digest,report,report_digest,sequence) VALUES(?1,?2,?3,(SELECT max(sequence) FROM events))",
        rusqlite::params![reference.digest, report, format!("{:x}", Sha256::digest(report.as_bytes()))]).unwrap();
    reference
}

impl Lab {
    /// Owner-sign and import revision `revision` of routine `name` (every
    /// `schedule` from `start`), whose script appends `once` to `<name>.marker`.
    fn routine(&self, name: &str, revision: u64, enabled: bool, schedule: &str, start: i64, missed: MissedRunPolicy) -> Output {
        let project = self.project.canonicalize().unwrap();
        let script = project.join(format!("{name}.sh"));
        let body = format!("printf once >> {name}.marker\n");
        fs::write(&script, &body).unwrap();
        let definition = RoutineDefinition { version: 1, name: name.into(), revision, project_store: self.store().canonicalize().unwrap().display().to_string(),
            authority: authority::policy_reference(&project).unwrap(), config: migration::config_reference(&self.config()).unwrap(), enabled,
            schedule: schedule.into(), timezone: "UTC".into(), start_unix_ms: start, missed, overlap: OverlapPolicy::Skip,
            script: script.display().to_string(), script_sha256: format!("{:x}", Sha256::digest(&body)), cwd: project.display().to_string(),
            deadline_ms: 1000, output_cap_bytes: 4000 };
        let (doc, sig) = self.sign(&format!("{name}-{revision}.json"), &serde_json::to_vec(&definition).unwrap(), authority::ROUTINE_SIGNATURE_NAMESPACE);
        self.cli(&["routine-store", "demo", "import", &doc, &sig, "--expected-head", &self.head()])
    }
    fn schedule(&self, name: &str) -> Output { self.cli(&["routine-store", "demo", "schedule", name, "--expected-head", &self.head()]) }
    fn marker(&self, name: &str) -> Option<String> { fs::read_to_string(self.project.join(format!("{name}.marker"))).ok() }
}

/// Replaces `missed_policy_skips_a_bounded_window_and_revisions_do_not_reuse_occurrences`.
///
/// Three due slots under `coalesce_latest` enqueue one run. A new revision
/// retires that unrun operation; under `skip` its own three missed slots are
/// recorded as skipped with no operation. A later revision records a fresh
/// occurrence for the same slot, and re-importing it is refused.
#[test]
fn missed_slots_are_skipped_or_coalesced_and_a_revision_never_reuses_an_occurrence() {
    let lab = Lab::new("[safety.PROJECT]\nroutine_commands=true\n");
    lab.activate();
    let start = jiff::Timestamp::now().as_millisecond() - 150_000;
    let schedule = |revision: u64, missed: MissedRunPolicy| {
        assert!(lab.routine("check", revision, true, "every 1m", start, missed).status.success());
        let out = lab.schedule("check");
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        lab.state().routine_occurrences.into_iter().find(|o| o.routine.revision == revision).unwrap()
    };
    let first = schedule(1, MissedRunPolicy::CoalesceLatest);
    assert_eq!((first.slots, first.disposition.clone()), (3, RoutineDisposition::Enqueued));
    let operation = first.operation.clone().unwrap();
    let skipped = schedule(2, MissedRunPolicy::Skip);
    assert_eq!((skipped.slots, skipped.disposition, skipped.operation), (3, RoutineDisposition::SkippedMissed, None));
    assert_ne!(skipped.id, first.id);
    assert_eq!(lab.delivery(&operation).state, DeliveryState::PermanentFailure, "a new revision left the old run deliverable");
    let next = schedule(3, MissedRunPolicy::CoalesceLatest);
    assert_eq!((next.disposition.clone(), next.scheduled_unix_ms), (RoutineDisposition::Enqueued, first.scheduled_unix_ms));
    assert!(next.id != first.id && next.operation.is_some() && next.operation != first.operation);
    let before = lab.state();
    assert!(!lab.routine("check", 3, true, "every 1m", start, MissedRunPolicy::CoalesceLatest).status.success());
    assert_eq!(lab.state(), before);
    assert!(before.tasks.is_empty() && before.operations.iter().all(|o| o.task.is_none()));
}

/// Replaces `held_planning_known_inactive_state_returns_no_cursor_or_liveness`,
/// `project_scope_uses_control_revision_without_inventing_a_task` and
/// `planning_selection_does_not_revive_an_enabled_historical_revision`.
/// With `ticker_runs_an_approved_routine_once_beside_one_edited_after_approval`
/// it also replaces `synchronous_turn_keeps_the_same_rotation_and_withdrawal_rules`
/// and `held_planning_rotates_after_withdrawn_script_and_only_records_intent`.
///
/// A routine run is project work: it names no task and none is invented.
/// While the project is paused the ticker neither plans routines nor runs a
/// queued run, which must be retired before the project resumes. Once active
/// the ticker plans and runs the other enabled routine once; a routine whose
/// latest revision is disabled is never planned from its enabled first one.
#[test]
fn the_ticker_runs_routines_only_while_active_and_never_revives_a_disabled_revision() {
    let lab = Lab::new("[safety.PROJECT]\nroutine_commands=true\n");
    lab.route();
    lab.activate();
    let start = jiff::Timestamp::now().as_millisecond() - 1000;
    for (name, revision, enabled) in [("idle", 1, true), ("idle", 2, false), ("check", 1, true), ("later", 1, true)] {
        assert!(lab.routine(name, revision, enabled, "every 1h", start, MissedRunPolicy::CoalesceLatest).status.success());
    }
    assert!(lab.schedule("check").status.success());
    let queued = lab.state();
    let operation = queued.routine_occurrences[0].operation.clone().unwrap();
    assert!(queued.operations.iter().all(|o| o.task.is_none()) && queued.tasks.is_empty());
    lab.set_state("paused");
    lab.run(2, &|| true);
    let paused = lab.state();
    assert_eq!(paused.routine_occurrences, queued.routine_occurrences, "a paused project planned a routine");
    assert!(paused.routine_receipts.is_empty());
    let delivery = lab.delivery(&operation);
    assert_eq!((delivery.state, delivery.attempts), (DeliveryState::Pending, 0));
    assert_eq!((lab.marker("check"), lab.marker("later")), (None, None));
    lab.ok(&["operations", "demo", "retire", operation.as_str(), "--reason", "paused", "--expected-revision", &delivery.revision.to_string(), "--expected-head", &lab.head()]);

    lab.ok(&["reconcile", "demo", "--record"]);
    lab.activate();
    lab.run(2, &|| lab.state().routine_receipts.len() == 1);
    let active = lab.state();
    let mut planned: Vec<&str> = active.routine_occurrences.iter().map(|o| o.routine.id.as_str()).collect();
    planned.sort();
    assert_eq!(planned, ["routine-check", "routine-later"], "the disabled routine was planned");
    assert_eq!((lab.marker("later").as_deref(), lab.marker("check"), lab.marker("idle")), (Some("once"), None, None));
    assert!(active.tasks.is_empty() && active.attempts.is_empty());
}

/// Replaces `targeted_notification_candidate_matches_the_snapshot_and_a_future_due_stays_blocked`.
///
/// A notification Herdr reports as not shown (`busy`) is retried, but not
/// before its retry is due: the second show comes at least the one-second
/// backoff after the first, and it is then confirmed once.
#[test]
fn a_notification_retry_is_not_delivered_before_it_is_due() {
    let lab = Lab::new("");
    lab.route();
    lab.activate();
    fs::write(lab.path("busy"), b"").unwrap();
    let op = lab.notify("notify");
    lab.run(2, &|| lab.delivery(&op).state == DeliveryState::Confirmed);
    let shows: Vec<i64> = fs::read_to_string(lab.path("shows")).unwrap().lines().map(|l| l.parse().unwrap()).collect();
    assert_eq!(shows.len(), 2, "{shows:?}");
    assert!(shows[1] - shows[0] >= 1000, "retried before due: {shows:?}");
    assert_eq!(lab.delivery(&op).attempts, 2);
}

impl Lab {
    /// Rewrite the owner's configuration with `extra` appended to the
    /// authority table, then record and reactivate under it.
    fn reconfigure(&self, extra: &str) {
        let config = fs::read_to_string(self.config()).unwrap();
        let base = config.split("\n[").next().unwrap();
        fs::write(self.config(), format!("{base}\n{}", extra.replace("PROJECT", &format!("{:?}", self.project.display().to_string())))).unwrap();
        self.ok(&["reconcile", "demo", "--record"]);
        self.activate();
    }
    /// A delivery state per notification operation, as stored.
    fn deliveries(&self) -> Vec<(String, DeliveryState)> {
        let state = self.state();
        state.deliveries.iter().map(|d| (d.operation.as_str().to_owned(), d.state)).collect()
    }
}

/// Replaces the dispatch test `service_claims_before_effect_records_receipt_and_refuses_replay`.
///
/// While Herdr shows the notification the store already records the
/// delivery as claimed; afterwards it is confirmed with one attempt, and an
/// explicit operator delivery of the same operation is refused and shows
/// nothing, as is a restarted ticker.
#[test]
fn a_notification_is_claimed_before_it_is_shown_and_never_shown_twice() {
    let lab = Lab::new("");
    lab.route();
    lab.activate();
    let op = lab.notify("notify");
    lab.run(0, &|| lab.delivery(&op).state == DeliveryState::Confirmed);
    assert_eq!(fs::read_to_string(lab.path("during")).unwrap(), "claimed\n");
    let delivery = lab.delivery(&op);
    assert_eq!((delivery.attempts, lab.shown()), (1, 1));
    let out = Command::new(BIN).env_clear().env("HOME", lab.home.path()).env("PATH", "/usr/bin:/bin").env("HERDR_BIN_PATH", lab.path("herdr"))
        .args(["--root", lab.path("root").to_str().unwrap(), "operations", "demo", "deliver-notification", op.as_str(), "--expected-revision", &delivery.revision.to_string()]).output().unwrap();
    assert!(!out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    lab.run(2, &|| true);
    assert_eq!((lab.delivery(&op), lab.shown()), (delivery, 1));
}

/// Replaces `notification_typed_safety_is_checked_even_when_generic_config_is_admitted`.
///
/// Owner configuration whose project safety table is invalid still records
/// and activates, but a notification is refused and nothing is queued; with
/// the table fixed the same notification is accepted.
#[test]
fn a_notification_is_refused_while_the_project_safety_settings_are_invalid() {
    let lab = Lab::new("");
    lab.route();
    lab.reconfigure("[safety.PROJECT]\nstart_threads='invalid'\n");
    lab.add("notify");
    let before = lab.state();
    let out = lab.cli(&["operations", "demo", "notify", "notify", "--expected-head", &lab.head()]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && stderr.contains("invalid notification safety configuration"), "{stderr}");
    assert_eq!(lab.state(), before);
    lab.reconfigure("[safety.PROJECT]\nstart_threads='propose'\n");
    lab.ok(&["operations", "demo", "notify", "notify", "--expected-head", &lab.head()]);
    assert_eq!(lab.deliveries().len(), 1);
    assert!(!lab.path("requests").exists());
}

/// Replaces `canonical_notification_malformed_unresolved_history_is_not_assumed_disjoint`.
///
/// A notification whose show failed stays ambiguous. Once its item is handled
/// a new item may be notified, but not while the stored payload of the
/// ambiguous one cannot be read as a valid notification: its items are then
/// unknown and may overlap. Restored, the new notification is accepted.
#[test]
fn a_malformed_ambiguous_notification_blocks_new_notifications() {
    let lab = Lab::new("");
    lab.route();
    lab.activate();
    fs::write(lab.path("fail"), b"").unwrap();
    let old = lab.notify("notify");
    lab.run(0, &|| lab.delivery(&old).state == DeliveryState::Ambiguous);
    lab.ok(&["inbox", "done", "demo", "message"]);
    lab.add("waiting");
    lab.ok(&["plan", "wait", "demo", "register", "--task", "waiting", "--condition", "user_decision", "--deadline", "2000-01-01T00:00:00Z"]);
    lab.run(0, &|| lab.state().inbox.iter().any(|i| i.content.kind == "wait-wake"));
    assert_eq!(lab.shown(), 1);

    let raw = rusqlite::Connection::open(lab.store()).unwrap();
    let original: String = raw.query_row("SELECT payload FROM operations WHERE id=?1", [old.as_str()], |r| r.get(0)).unwrap();
    let store = |payload: &str| {
        raw.execute("UPDATE operations SET payload=?1,payload_hash=?2 WHERE id=?3", rusqlite::params![payload, format!("{:x}", Sha256::digest(payload)), old.as_str()]).unwrap();
    };
    let valid: Value = serde_json::from_str(&original).unwrap();
    for (pointer, value) in [("", json!({"invalid": "old notification"})), ("/inbox_ids", json!(["../bad"])), ("/binding_revision", json!(0)),
        ("/config/path", json!("relative")), ("/config/digest", json!("not-a-hash"))] {
        let mut bad = valid.clone();
        *bad.pointer_mut(pointer).unwrap() = value;
        store(&bad.to_string());
        let before = lab.state();
        let out = lab.cli(&["operations", "demo", "notify", "waiting", "--expected-head", &lab.head()]);
        assert!(!out.status.success(), "{pointer}: accepted {}", String::from_utf8_lossy(&out.stdout));
        assert_eq!(lab.state(), before, "{pointer}");
    }
    store(&original);
    lab.ok(&["operations", "demo", "notify", "waiting", "--expected-head", &lab.head()]);
    assert_eq!(lab.deliveries().iter().filter(|(_, s)| *s == DeliveryState::Pending).count(), 1);
}

/// Replaces the schedule test `interval_windows_are_anchored_bounded_and_restart_safe`.
///
/// Interval slots are anchored at the routine's start: three due minutes
/// schedule the last of them, and scheduling again before the next slot adds
/// nothing. A start decades back counts every missed slot at once, and an
/// interval longer than all of time has exactly its first slot.
#[test]
fn interval_slots_are_anchored_at_the_start_counted_in_bulk_and_never_rescheduled() {
    let lab = Lab::new("[safety.PROJECT]\nroutine_commands=true\n");
    lab.activate();
    let start = jiff::Timestamp::now().as_millisecond() - 150_000;
    assert!(lab.routine("check", 1, true, "every 1m", start, MissedRunPolicy::CoalesceLatest).status.success());
    assert!(lab.schedule("check").status.success());
    let first = lab.state().routine_occurrences;
    assert_eq!((first.len(), first[0].slots, first[0].first_unix_ms, first[0].scheduled_unix_ms), (1, 3, start, start + 120_000));
    let before = lab.state();
    let again = lab.schedule("check");
    assert_eq!(lab.state(), before, "{}", String::from_utf8_lossy(&again.stdout));

    for (name, schedule, slots) in [("decades", "every 1m", jiff::Timestamp::now().as_millisecond() / 60_000 + 1), ("forever", "every 100000000000d", 1)] {
        assert!(lab.routine(name, 1, true, schedule, 0, MissedRunPolicy::CoalesceLatest).status.success(), "{name}");
        let started = Instant::now();
        let out = lab.schedule(name);
        assert!(out.status.success() && started.elapsed() < Duration::from_secs(5), "{name}: {}", String::from_utf8_lossy(&out.stderr));
        let occurrence = lab.state().routine_occurrences.into_iter().find(|o| o.routine.id == format!("routine-{name}")).unwrap();
        assert_eq!(occurrence.first_unix_ms, 0, "{name}");
        assert!(occurrence.slots.abs_diff(slots as u64) <= 1, "{name}: {} slots, expected {slots}", occurrence.slots);
    }
}

/// Replaces the watchdog test `other_store_errors_do_not_pause`.
///
/// With automatic admission on, a store error that is neither a full disk
/// nor a busy database is logged by every pass as an admission error, but
/// admission is not paused: no pause file is written and `status` reports
/// none.
#[test]
fn a_store_error_that_is_not_a_full_disk_or_busy_database_does_not_pause_admission() {
    let lab = Lab::new("");
    lab.activate();
    let raw = rusqlite::Connection::open(lab.store()).unwrap();
    // Fixture only: automatic admission is enabled by a signed factory manifest.
    raw.execute("UPDATE project_control SET factory_admission='on' WHERE singleton=1", []).unwrap();
    raw.execute_batch("ALTER TABLE pending_verification_work RENAME TO held_aside").unwrap();
    let errors = || lab.log().lines().filter(|line| line.contains(r#""reason":"error""#)).count();
    lab.run(0, &|| errors() >= 2);
    assert!(!lab.project.join(".state/admission-paused.json").exists(), "{}", lab.log());
    raw.execute_batch("ALTER TABLE held_aside RENAME TO pending_verification_work").unwrap();
    let status = lab.ok(&["factory", "status", "demo"]);
    assert_eq!((&status["admission_paused"], &status["pause_reason"]), (&json!(false), &Value::Null), "{status}");
}

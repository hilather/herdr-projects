#![cfg(target_os = "linux")]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! The background ticker through the compiled CLI: `ticker start/status/stop`
//! and `ticker run` against fake herdr sessions. One fake serves every
//! project in a root; it tells sessions apart by the socket each call names
//! (`HERDR_SOCKET_PATH`), answers from per-session files and logs each call.
use serde_json::{json, Value};
use std::{fs, os::unix::{fs::PermissionsExt, net::UnixListener}, path::{Path, PathBuf}, process::{Command, Output, Stdio}, time::{Duration, Instant}};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-projects");

/// Session `NAME` is the socket `$HOME/NAME.sock`; it lists `NAME.agents` and
/// `NAME.panes` (`NAME@MACHINE.*` through `--machine`; saved machines are
/// `machines.json`). Bridge requests are answered per `NAME.reply`: `ack` echoes
/// the matching agent, `reject` returns an error. Every call is logged to
/// `calls` as `NAME ARGS [METHOD]`.
const FAKE_HERDR: &str = r#"#!/usr/bin/python3
import json,os,pathlib,sys
home=pathlib.Path(os.environ['HOME']);args=sys.argv[1:]
name=pathlib.Path(os.environ.get('HERDR_SOCKET_PATH','none')).stem
machine=None
if args[:1]==['--machine']:machine=args[1];args=args[2:]
if machine:name=name+'@'+machine
def read(kind):
    p=home/f'{name}.{kind}'
    return json.loads(p.read_text()) if p.exists() else []
line=name+' '+' '.join(args)
request=None
if args==['remote-api-bridge']:
    request=json.loads(sys.stdin.readline());line+=' '+request['method']
with open(home/'calls','a') as f:f.write(line+'\n')
if (home/f'{name}.down').exists():print('connection refused',file=sys.stderr);sys.exit(1)
if args==['machine','list','--json']:print((home/'machines.json').read_text())
elif args==['--version']:print('herdr 0.9.1')
elif args==['remote-api-bridge','--check']:print('herdr-api-bridge-v1')
elif args==['agent','list']:print(json.dumps({'result':{'agents':read('agents')}}))
elif args==['pane','list']:print(json.dumps({'result':{'panes':read('panes')}}))
elif request is not None and request['method'] in ('agent.list','pane.list'):
    kind=request['method'].split('.')[0]+'s';print(json.dumps({'id':request['id'],'result':{kind:read(kind)}}))
elif request is not None:
    mode=(home/f'{name}.reply').read_text() if (home/f'{name}.reply').exists() else 'ack'
    if mode=='reject':print(json.dumps({'id':request['id'],'error':{'code':'agent_blocked','message':'refused by fixture'}}));sys.exit(0)
    params=request['params']
    if request['method']=='agent.prompt':agent=next(a for a in read('agents') if a['pane_id']==params['target'])
    else:agent=dict(next(p for p in read('panes') if p['pane_id']==params['pane_id']),name=params['name'],launch_pending=True,agent_status='unknown')
    kind={'agent.prompt':'agent_prompted','agent.start':'agent_started'}[request['method']]
    print(json.dumps({'id':request['id'],'result':{'type':kind,'agent':agent,'argv':['claude']}}))
elif args[:2] in (['agent','prompt'],['agent','start']):print('{"error":{"code":"unsupported","message":"synchronous effect"}}');sys.exit(2)
else:print('{"result":{}}')
"#;

struct Lab { home: tempfile::TempDir, fake: PathBuf, listeners: Vec<UnixListener> }

impl Lab {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let fake = home.path().join("herdr");
        fs::write(&fake, FAKE_HERDR).unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
        Lab { home, fake, listeners: Vec::new() }
    }
    fn path(&self, name: &str) -> PathBuf { self.home.path().join(name) }
    fn root(&self) -> PathBuf { self.path("root") }
    fn project(&self, slug: &str) -> PathBuf { self.root().join(slug) }
    fn command(&self) -> Command {
        let mut command = Command::new(BIN);
        command.env_clear().env("HOME", self.home.path()).env("PATH", "/usr/bin:/bin").env("HERDR_BIN_PATH", &self.fake)
            .arg("--root").arg(self.root());
        command
    }
    fn cli(&self, args: &[&str]) -> Output { self.command().args(args).output().unwrap() }
    fn ok(&self, args: &[&str]) -> String {
        let out = self.cli(args);
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }
    /// A new project whose coordinator lives in session `slug`, pane `p`.
    fn project_in_session(&mut self, slug: &str, reachable: bool, extra: Value) -> PathBuf {
        self.ok(&["new", slug]);
        let socket = self.path(&format!("{slug}.sock"));
        if reachable { self.listeners.push(UnixListener::bind(&socket).unwrap()); }
        let mut record = json!({"socket": socket, "workspace_id": "w", "tab_id": "w:t", "pane_id": "p", "agent_name": "coordinator",
            "cwd": self.project(slug), "prime_pending": true, "prime_request": 1});
        for (k, v) in extra.as_object().unwrap() { record[k] = v.clone(); }
        fs::write(self.project(slug).join(".state/coordinator.json"), record.to_string()).unwrap();
        self.project(slug)
    }
    /// What session `name` lists: panes, and agents with the given statuses.
    fn session(&self, name: &str, agents: &[Value], panes: &[Value]) {
        fs::write(self.path(&format!("{name}.agents")), Value::from(agents.to_vec()).to_string()).unwrap();
        fs::write(self.path(&format!("{name}.panes")), Value::from(panes.to_vec()).to_string()).unwrap();
    }
    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.path("calls")).unwrap_or_default().lines().map(str::to_string).collect()
    }
    fn calls_for(&self, name: &str) -> Vec<String> {
        self.calls().into_iter().filter(|c| c.split(' ').next() == Some(name)).collect()
    }
    fn coordinator(&self, slug: &str) -> Value {
        serde_json::from_slice(&fs::read(self.project(slug).join(".state/coordinator.json")).unwrap()).unwrap()
    }
    fn inbox(&self, slug: &str, prefix: &str) -> usize {
        fs::read_dir(self.project(slug).join("inbox")).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with(prefix)).count()
    }
    /// Every thread record in the root, for failure messages.
    fn threads(&self) -> String {
        let projects = fs::read_dir(self.root()).unwrap().flatten().filter_map(|p| fs::read_dir(p.path().join("threads")).ok());
        projects.flat_map(|d| d.flatten()).map(|e| format!("{}:\n{}", e.path().display(), fs::read_to_string(e.path()).unwrap_or_default())).collect()
    }
    fn run_ticker(&self) -> Ticker<'_> {
        let child = self.command().args(["ticker", "run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        Ticker { lab: self, child }
    }
}

/// A `ticker run` child; stopped through its stop file, killed on panic.
struct Ticker<'a> { lab: &'a Lab, child: std::process::Child }
impl Ticker<'_> {
    fn wait_for(&mut self, what: &str, limit: u64, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(limit);
        while !done() {
            assert!(self.child.try_wait().unwrap().is_none(), "ticker exited while waiting for {what}");
            assert!(Instant::now() < deadline, "timed out waiting for {what}; ticker log:\n{}\nthreads:\n{}",
                fs::read_to_string(self.lab.root().join(".ticker.log")).unwrap_or_default(), self.lab.threads());
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn stop(mut self) {
        let stop = self.lab.root().join(".ticker.stop");
        fs::write(&stop, b"").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "ticker ignored its stop file");
            std::thread::sleep(Duration::from_millis(20));
        }
        fs::remove_file(stop).unwrap();
    }
}
impl Drop for Ticker<'_> { fn drop(&mut self) { let _ = self.child.kill(); let _ = self.child.wait(); } }

fn pane(cwd: &Path) -> Value { pane_at("p", cwd) }
fn agent(cwd: &Path, name: &str, status: &str) -> Value { agent_at("p", cwd, name, status) }
/// Pane `id` in workspace `w`, tab `w:t`.
fn pane_at(id: &str, cwd: &Path) -> Value { json!({"workspace_id": "w", "tab_id": "w:t", "pane_id": id, "terminal_id": format!("term-{id}"), "cwd": cwd}) }
fn agent_at(id: &str, cwd: &Path, name: &str, status: &str) -> Value {
    let mut agent = pane_at(id, cwd);
    agent["name"] = json!(name);
    agent["agent"] = json!("claude");
    agent["agent_status"] = json!(status);
    agent
}

/// Seven projects share one root and one ticker. Each coordinator session
/// shows a different situation; the ticker primes only a ready (not blocked)
/// coordinator of its own, never resends a refused prime, never starts one
/// past the launch cap, and reads nothing from a session it cannot reach.
#[test]
fn ticker_primes_ready_coordinators_and_leaves_other_sessions_alone() {
    let mut lab = Lab::new();
    let ready = lab.project_in_session("ready", true, json!({}));
    let blocked = lab.project_in_session("blocked", true, json!({}));
    let refused = lab.project_in_session("refused", true, json!({}));
    lab.project_in_session("stranger", true, json!({}));
    let shell = lab.project_in_session("shell", true, json!({"launch_attempts": 3}));
    lab.project_in_session("down", true, json!({}));
    lab.project_in_session("gone", false, json!({}));
    lab.session("ready", &[agent(&ready, "coordinator", "idle")], &[pane(&ready)]);
    lab.session("blocked", &[agent(&blocked, "coordinator", "blocked")], &[pane(&blocked)]);
    lab.session("refused", &[agent(&refused, "coordinator", "idle")], &[pane(&refused)]);
    fs::write(lab.path("refused.reply"), "reject").unwrap();
    // Same pane ids, another working directory: not this project's pane.
    let elsewhere = lab.path("elsewhere");
    lab.session("stranger", &[agent(&elsewhere, "coordinator", "idle")], &[pane(&elsewhere)]);
    // A shell prompt in the coordinator's pane after three launch attempts.
    lab.session("shell", &[], &[pane(&shell)]);
    fs::write(lab.path("down.down"), "").unwrap();
    // Unreachable sessions keep a thread whose pane would otherwise read as closed.
    let thread = json!({"id": "t-0001", "title": "Busy", "status": "open", "kind": "tab", "created": "2026-01-01T00:00:00Z", "agent": "claude",
        "agent_name": "hp-t", "workspace_id": "w", "tab_id": "w:t", "pane_id": "q", "cwd": "/wt", "last_state": "working", "last_group": "working"});
    for slug in ["down", "gone"] { fs::write(lab.project(slug).join("threads/t-0001.toml"), toml::to_string(&thread).unwrap()).unwrap(); }
    // The coordinator record, thread records and inbox of a project, as text.
    let records = |slug: &str| ["/.state/coordinator.json", "/threads", "/inbox"].iter().flat_map(|part| {
        let path = PathBuf::from(format!("{}{part}", lab.project(slug).display()));
        let files = if path.is_dir() { fs::read_dir(&path).unwrap().flatten().map(|e| e.path()).collect() } else { vec![path] };
        files.into_iter().filter(|f: &PathBuf| f.is_file()).map(|f| format!("{}: {}", f.display(), fs::read_to_string(&f).unwrap())).collect::<Vec<_>>()
    }).collect::<Vec<_>>();
    let before: Vec<_> = ["stranger", "shell", "down", "gone"].iter().map(|s| records(s)).collect();

    let mut ticker = lab.run_ticker();
    ticker.wait_for("the ready coordinator's prime", 60, || lab.coordinator("ready")["prime_pending"] == false);
    ticker.wait_for("the refused prime's notice", 60, || lab.inbox("refused", "coordinator-prime-") == 1);
    // The blocked session was observed alongside; one more observation after both effects.
    let polls = |name: &str| lab.calls_for(name).iter().filter(|c| c.ends_with(" agent list")).count();
    let seen = (polls("blocked"), polls("stranger"));
    ticker.wait_for("another poll", 60, || polls("blocked") > seen.0 && polls("stranger") > seen.1);
    let effect = |c: &String| c.ends_with("agent.prompt") || c.ends_with("agent.start");
    assert!(lab.coordinator("blocked")["prime_pending"] == true);
    assert!(!lab.calls_for("blocked").iter().any(effect), "{:?}", lab.calls_for("blocked"));

    ticker.stop();

    // The ready coordinator got exactly one prime, in its own session.
    let prompts = |name: &str| lab.calls_for(name).iter().filter(|c| c.ends_with("agent.prompt")).count();
    assert_eq!((prompts("ready"), prompts("blocked")), (1, 0));
    assert_eq!(lab.coordinator("ready")["prime_claim"]["delivery"]["phase"], "confirmed");
    assert!(lab.coordinator("ready")["prime_claim"]["delivery"]["prompt"].as_str().unwrap().contains("context ready"));
    // A refused prime stays pending, is sent once, and asks for inspection.
    assert_eq!(prompts("refused"), 1);
    assert_eq!(lab.coordinator("refused")["prime_pending"], true);
    assert_eq!(lab.coordinator("refused")["prime_claim"]["delivery"]["phase"], "uncertain");
    // No effect of any kind reached the foreign pane or the exhausted shell,
    // and nothing asked a synchronous prompt or start of anyone.
    for name in ["stranger", "shell"] {
        let effects: Vec<_> = lab.calls_for(name).into_iter().filter(|c| effect(c) || c.contains("report-metadata")).collect();
        assert!(effects.is_empty(), "{name}: {effects:?}");
    }
    assert!(!lab.calls().iter().any(|c| c.contains(" agent prompt") || c.contains(" agent start")), "{:?}", lab.calls());
    // A missing socket is never called; a refusing session gets no further call.
    assert!(lab.calls_for("gone").is_empty(), "{:?}", lab.calls_for("gone"));
    assert!(lab.calls_for("down").iter().all(|c| c.ends_with("agent list") || c.ends_with("pane list") || c.ends_with("--version")), "{:?}", lab.calls_for("down"));
    let after: Vec<_> = ["stranger", "shell", "down", "gone"].iter().map(|s| records(s)).collect();
    assert_eq!(before, after, "records of foreign, exhausted or unreachable sessions changed");
    assert_eq!(lab.coordinator("shell")["launch_attempts"], 3);
}

fn status_field(status: &str, field: &str) -> String {
    status.lines().find_map(|l| l.trim().strip_prefix(field)).map(|v| v.trim().to_string()).unwrap_or_default()
}

/// Holds the root's ticker lock the way a running ticker of `version` does.
fn hold_lock(root: &Path, version: &str) -> fs::File {
    use std::io::Write;
    let mut file = fs::File::options().create(true).write(true).truncate(false).open(root.join(".ticker.lock")).unwrap();
    file.lock().unwrap();
    file.set_len(0).unwrap();
    file.write_all(json!({"version": version, "pid": 1, "root": root, "started": "fixture", "tools": []}).to_string().as_bytes()).unwrap();
    file
}

/// `ticker start/status/stop` keep one current ticker per root: nothing is
/// created without projects, a stale stop request is cleared, a running
/// ticker of this version is kept, and one of another version, or one that
/// is already being stopped, is stopped and replaced.
#[test]
fn ticker_commands_keep_one_current_ticker_per_root() {
    let lab = Lab::new();
    let root = lab.root();
    struct StopOnDrop<'a>(&'a Lab);
    impl Drop for StopOnDrop<'_> { fn drop(&mut self) { let _ = self.0.cli(&["ticker", "stop"]); } }
    let _cleanup = StopOnDrop(&lab);
    let version = lab.ok(&["--version"]).trim().rsplit(' ').next().unwrap().to_string();
    let stop_file = root.join(".ticker.stop");

    // Without projects neither `start` nor `run` creates anything.
    lab.ok(&["ticker", "start"]);
    lab.ok(&["ticker", "run"]);
    assert!(!root.exists());
    fs::create_dir(&root).unwrap();
    lab.ok(&["ticker", "start"]);
    lab.ok(&["ticker", "run"]);
    assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    assert!(lab.ok(&["ticker", "status"]).starts_with("ticker: not running"));

    // A stop request left behind by a ticker that already exited is cleared.
    fs::write(&stop_file, "").unwrap();
    lab.ok(&["ticker", "stop"]);
    assert!(!stop_file.exists());

    // With a project, `start` runs one detached ticker, even past a stale stop file.
    lab.ok(&["new", "demo"]);
    fs::write(&stop_file, "").unwrap();
    lab.ok(&["ticker", "start"]);
    let running = || {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let status = lab.ok(&["ticker", "status"]);
            if status.starts_with("ticker: running") && status_field(&status, "pid:") != "1" { return status; }
            assert!(Instant::now() < deadline, "no ticker took the lock: {status}");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let status = running();
    assert_eq!(status_field(&status, "version:"), version);
    assert_eq!(status_field(&status, "root:"), root.display().to_string());
    assert!(!status.contains("note:"), "{status}");
    let pid = status_field(&status, "pid:");
    // A second `start` keeps the healthy ticker of the same version.
    lab.ok(&["ticker", "start"]);
    assert_eq!(status_field(&running(), "pid:"), pid);
    lab.ok(&["ticker", "stop"]);
    assert!(lab.ok(&["ticker", "status"]).starts_with("ticker: not running"));
    assert!(!Path::new(&format!("/proc/{pid}")).exists() || fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default().contains(") Z"));

    // A ticker of another version is reported, then stopped through its stop file and replaced.
    let old = hold_lock(&root, "0.0.0+old");
    let status = lab.ok(&["ticker", "status"]);
    assert_eq!(status_field(&status, "version:"), "0.0.0+old");
    assert!(status.contains(&format!("note: this binary is {version}; `ticker start` replaces the running one")), "{status}");
    let mut start = lab.command().args(["ticker", "start"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !stop_file.exists() {
        assert!(start.try_wait().unwrap().is_none(), "`ticker start` returned without stopping the old ticker");
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(old);
    assert!(start.wait().unwrap().success());
    let status = running();
    assert_eq!(status_field(&status, "version:"), version);
    lab.ok(&["ticker", "stop"]);

    // A same-version ticker that is already being stopped is also replaced:
    // `start` renews the stop request and waits for the lock.
    let stopping = hold_lock(&root, &version);
    fs::File::create(&stop_file).unwrap().set_modified(std::time::SystemTime::UNIX_EPOCH).unwrap();
    let mut start = lab.command().args(["ticker", "start"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while fs::metadata(&stop_file).and_then(|m| m.modified()).is_ok_and(|m| m == std::time::SystemTime::UNIX_EPOCH) {
        assert!(start.try_wait().unwrap().is_none(), "`ticker start` kept a ticker that was being stopped");
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(stopping);
    assert!(start.wait().unwrap().success());
    assert_eq!(status_field(&running(), "version:"), version);
    lab.ok(&["ticker", "stop"]);
    assert!(lab.ok(&["ticker", "status"]).starts_with("ticker: not running"));
    assert!(!stop_file.exists());
}

fn thread_record(id: &str, pane: &str, cwd: &Path, extra: Value) -> String {
    let mut record = json!({"id": id, "title": id, "status": "open", "kind": "adopted", "created": jiff::Timestamp::now().to_string(),
        "agent": "claude", "agent_name": format!("worker-{pane}"), "workspace_id": "w", "tab_id": "w:t", "pane_id": pane, "cwd": cwd, "thread_dir": cwd});
    for (k, v) in extra.as_object().unwrap() { record[k] = v.clone(); }
    toml::to_string(&record).unwrap()
}
fn read_thread(project: &Path, id: &str) -> toml::Value {
    toml::from_str(&fs::read_to_string(project.join(format!("threads/{id}.toml"))).unwrap()).unwrap()
}

/// Two launches waiting at a shell prompt. One has used all three attempts
/// and fails visibly; the other gets its third, acknowledged start, and the
/// agent that has not appeared yet does not count as a fourth failure.
#[test]
fn ticker_launch_cap_fails_exhausted_threads_but_not_an_acknowledged_last_start() {
    let mut lab = Lab::new();
    let project = lab.project_in_session("demo", true, json!({"prime_pending": false}));
    let (a, b) = (lab.path("a"), lab.path("b"));
    fs::create_dir(&a).unwrap();
    fs::create_dir(&b).unwrap();
    fs::write(project.join("threads/t-0001.toml"), thread_record("t-0001", "pa", &a, json!({"prompt_pending": true, "launch_attempts": 3}))).unwrap();
    fs::write(project.join("threads/t-0002.toml"), thread_record("t-0002", "pb", &b, json!({"prompt_pending": true, "launch_attempts": 2}))).unwrap();
    lab.session("demo", &[agent(&project, "coordinator", "idle")], &[pane(&project), pane_at("pa", &a), pane_at("pb", &b)]);

    let mut ticker = lab.run_ticker();
    ticker.wait_for("the exhausted launch to fail", 60, || read_thread(&project, "t-0001")["status"].as_str() == Some("failed"));
    ticker.wait_for("the last start's acknowledgement", 60, || read_thread(&project, "t-0002").get("launch_claim").and_then(|c| c.get("phase")).and_then(|p| p.as_str()) == Some("confirmed"));
    let polls = || lab.calls_for("demo").iter().filter(|c| c.ends_with(" pane list")).count();
    let seen = polls();
    ticker.wait_for("two more polls without the agent", 60, || polls() >= seen + 2);
    ticker.stop();

    let failed = read_thread(&project, "t-0001");
    assert_eq!(failed["error"].as_str(), Some("no `claude` agent appeared in the pane after 3 launch attempts"));
    assert_eq!(failed["launch_attempts"].as_integer(), Some(3));
    let pending = read_thread(&project, "t-0002");
    assert_eq!((pending["status"].as_str(), pending["launch_attempts"].as_integer(), pending["prompt_pending"].as_bool()), (Some("open"), Some(3), Some(true)));
    let starts: Vec<_> = lab.calls_for("demo").into_iter().filter(|c| c.ends_with("agent.start") || c.contains(" agent start")).collect();
    assert_eq!(starts, ["demo remote-api-bridge agent.start"]);
    let list = lab.ok(&["thread", "list", "demo"]);
    let row = |id: &str| list.lines().find(|l| l.starts_with(id)).unwrap_or_default().to_string();
    assert!(row("t-0001").contains("failed: no `claude` agent appeared"), "{list}");
    assert!(row("t-0002").contains("start acknowledged; no agent observed"), "{list}");
}

fn sha(bytes: &[u8]) -> String { use sha2::Digest; format!("{:x}", sha2::Sha256::digest(bytes)) }

/// A worker copies home a first report while still working; the ticker stops
/// before anyone is told. The report then changes and the agent goes idle:
/// the thread is announced for review once, for the fresh copy only, never
/// for the stale receipt it still held when it became ready.
#[test]
fn ticker_announces_review_only_for_a_fresh_copy_of_the_changed_report() {
    let mut lab = Lab::new();
    let project = lab.project_in_session("demo", true, json!({"prime_pending": false}));
    let source = lab.path("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("report.md"), "old report\n").unwrap();
    fs::write(project.join("threads/t-0001.toml"), thread_record("t-0001", "pw", &source, json!({}))).unwrap();
    let worker = |status| lab.session("demo", &[agent(&project, "coordinator", "idle"), agent_at("pw", &source, "worker-pw", status)], &[pane(&project), pane_at("pw", &source)]);
    let record = || read_thread(&project, "t-0001");
    let text = |key: &str| record().get(key).and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let reviews = || lab.inbox("demo", "review-");
    worker("working");
    let mut ticker = lab.run_ticker();
    ticker.wait_for("the first copy home", 60, || text("report_hash") == sha(b"old report\n") && record().get("copy_receipt").is_some());
    ticker.stop();
    assert_eq!(fs::read_to_string(project.join("threads/t-0001.md")).unwrap(), "old report\n");
    assert_eq!((reviews(), text("last_review_item_hash")), (0, String::new()));

    fs::write(source.join("report.md"), "new report\n").unwrap();
    worker("idle");
    let mut ticker = lab.run_ticker();
    ticker.wait_for("the review notice", 120, || !text("last_review_item_hash").is_empty());
    let polls = || lab.calls_for("demo").iter().filter(|c| c.ends_with(" pane list")).count();
    let seen = polls();
    ticker.wait_for("another poll", 60, || polls() > seen);
    ticker.stop();
    assert_eq!(text("last_review_item_hash"), sha(b"new report\n"));
    assert_eq!(text("report_hash"), sha(b"new report\n"));
    assert_eq!(fs::read_to_string(project.join("threads/t-0001.md")).unwrap(), "new report\n");
    assert_eq!(record()["copy_receipt"]["sequence"].as_integer(), Some(2));
    assert_eq!(reviews(), 1);
    assert_eq!(lab.ok(&["thread", "list", "demo"]).split('\t').nth(1), Some("Ready for review"));
}

/// Remote threads on a machine whose saved listing has no session contract
/// (no profile id or session): the ticker observes them, but neither sends
/// the brief to the idle agent nor starts one at the shell prompt, and it
/// never falls back to a synchronous `agent prompt` or `agent start`.
#[test]
fn ticker_holds_remote_briefs_and_launches_without_a_saved_session_contract() {
    let mut lab = Lab::new();
    let project = lab.project_in_session("demo", true, json!({"prime_pending": false}));
    let bin = lab.path("bin");
    fs::create_dir(&bin).unwrap();
    fs::write(bin.join("ssh"), "#!/bin/sh\n[ \"$(eval echo \\${$(($#-1))})\" = fixture.invalid ] || exit 9\neval \"exec /bin/sh -c \\\"\\${$#}\\\"\"\n").unwrap();
    fs::set_permissions(bin.join("ssh"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(lab.path("machines.json"), json!([{"label": "legacy", "target": "fixture.invalid", "enabled": true}]).to_string()).unwrap();
    let (r1, r2) = (lab.path("r1"), lab.path("r2"));
    for (id, pane, dir) in [("t-0001", "r1", &r1), ("t-0002", "r2", &r2)] {
        fs::create_dir(dir).unwrap();
        fs::write(project.join(format!("threads/{id}.toml")), thread_record(id, pane, dir, json!({"machine": "legacy", "prompt_pending": true, "launch_attempts": 0}))).unwrap();
    }
    lab.session("demo", &[agent(&project, "coordinator", "idle")], &[pane(&project)]);
    lab.session("demo@legacy", &[agent_at("r1", &r1, "worker-r1", "idle")], &[pane_at("r1", &r1), pane_at("r2", &r2)]);
    let before: Vec<_> = ["t-0001", "t-0002"].iter().map(|id| read_thread(&project, id)).collect();

    let mut ticker = lab.command().env("PATH", format!("{}:/usr/bin:/bin", bin.display())).args(["ticker", "run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().map(|child| Ticker { lab: &lab, child }).unwrap();
    let log = || fs::read_to_string(lab.root().join(".ticker.log")).unwrap_or_default();
    ticker.wait_for("the refused brief and launch", 60, || log().contains("t-0001: saved remote session contract unavailable for brief") && log().contains("brief route kind mismatch"));
    ticker.stop();

    let remote = lab.calls_for("demo@legacy");
    assert!(remote.iter().any(|c| c.ends_with(" agent list")), "the machine was never observed: {remote:?}");
    let effects: Vec<_> = lab.calls().into_iter().filter(|c| c.contains("agent.prompt") || c.contains("agent.start") || c.contains(" agent prompt") || c.contains(" agent start")).collect();
    assert!(effects.is_empty(), "{effects:?}");
    let after: Vec<_> = ["t-0001", "t-0002"].iter().map(|id| read_thread(&project, id)).collect();
    for (before, after) in before.iter().zip(&after) {
        for key in ["status", "prompt_pending", "launch_attempts", "launch_claim", "prompt_claim"] { assert_eq!(before.get(key), after.get(key), "{key}"); }
    }
}

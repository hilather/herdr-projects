#![cfg(target_os = "linux")]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! Adopting running agent panes through the compiled CLI: `thread adopt`,
//! `adopt-workspace` and the prompt a foreground `ticker run` delivers later.
//! One fake herdr serves every session in the root, told apart by the socket
//! each call names; it answers from per-session files and logs every call.
use serde_json::{json, Value};
use std::{fs, os::unix::{fs::PermissionsExt, net::UnixListener}, path::{Path, PathBuf}, process::{Command, Output, Stdio}, time::{Duration, Instant}};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-projects");

/// Session `NAME` is the socket `$HOME/NAME.sock`; it lists `NAME.agents` and
/// `NAME.panes`. Prompts, synchronous or through the bridge, are acknowledged.
/// Every call is logged to `calls` as `NAME ARGS [METHOD]`.
const FAKE_HERDR: &str = r#"#!/usr/bin/python3
import json,os,pathlib,sys
home=pathlib.Path(os.environ['HOME']);args=sys.argv[1:]
name=pathlib.Path(os.environ.get('HERDR_SOCKET_PATH','none')).stem
def read(kind):
    p=home/f'{name}.{kind}'
    return json.loads(p.read_text()) if p.exists() else []
line=name+' '+' '.join(args);request=None
if args==['remote-api-bridge']:request=json.loads(sys.stdin.readline());line+=' '+request['method']
with open(home/'calls','a') as f:f.write(line+'\n')
if args==['--version']:print('herdr 0.9.1')
elif args==['remote-api-bridge','--check']:print('herdr-api-bridge-v1')
elif args in (['agent','list'],['pane','list']):print(json.dumps({'result':{args[0]+'s':read(args[0]+'s')}}))
elif request is not None and request['method'] in ('agent.list','pane.list'):
    kind=request['method'].split('.')[0]+'s';print(json.dumps({'id':request['id'],'result':{kind:read(kind)}}))
elif args[:2]==['agent','prompt']:
    agent=next(a for a in read('agents') if a['pane_id']==args[2])
    print(json.dumps({'result':{'type':'agent_prompted','agent':agent}}))
elif request is not None and request['method']=='agent.prompt':
    agent=next(a for a in read('agents') if a['pane_id']==request['params']['target'])
    print(json.dumps({'id':request['id'],'result':{'type':'agent_prompted','agent':agent}}))
elif request is not None and request['method']=='notification.show':print(json.dumps({'id':request['id'],'result':{'type':'notification_show','shown':True,'reason':'shown'}}))
elif request is not None and request['method']=='pane.report_metadata':print(json.dumps({'id':request['id'],'result':{'type':'ok'}}))
elif request is not None:print(json.dumps({'id':request['id'],'error':{'code':'unsupported','message':'not in fixture'}}))
elif args[:2]==['agent','start']:print('{"error":{"code":"unsupported","message":"not in fixture"}}');sys.exit(2)
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
    fn socket(&self, name: &str) -> PathBuf { self.path(&format!("{name}.sock")) }
    fn command(&self) -> Command {
        let mut command = Command::new(BIN);
        command.env_clear().env("HOME", self.home.path()).env("PATH", "/usr/bin:/bin").env("HERDR_BIN_PATH", &self.fake).arg("--root").arg(self.root());
        command
    }
    fn cli(&self, args: &[&str]) -> Output { self.command().args(args).output().unwrap() }
    /// A command that succeeds, retried while a ticker pass holds the root lock.
    fn ok(&self, args: &[&str]) -> String {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let out = self.cli(args);
            if out.status.success() { return String::from_utf8(out.stdout).unwrap(); }
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(stderr.contains("another operation owns lock") && Instant::now() < deadline, "{args:?}: {stderr}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    /// A command that fails for another reason than the root lock; its stderr.
    fn refused(&self, args: &[&str]) -> String {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let out = self.cli(args);
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            assert!(!out.status.success(), "{args:?} succeeded");
            if !stderr.contains("another operation owns lock") { return stderr; }
            assert!(Instant::now() < deadline, "{args:?}: {stderr}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    /// A project whose idle coordinator is pane `p` of session `session`.
    fn project_in_session(&mut self, slug: &str, session: &str) -> PathBuf {
        self.ok(&["new", slug]);
        let project = self.project(slug);
        if !self.socket(session).exists() { self.listeners.push(UnixListener::bind(self.socket(session)).unwrap()); }
        fs::write(project.join(".state/coordinator.json"), json!({"socket": self.socket(session), "workspace_id": "w", "tab_id": "w:t", "pane_id": "p",
            "agent_name": "coordinator", "cwd": project}).to_string()).unwrap();
        project
    }
    /// What session `name` lists: the coordinator of `project` plus `agents`.
    fn session(&self, name: &str, project: &Path, agents: &[Value]) {
        let mut all = vec![agent("p", project, "coordinator", "idle")];
        all.extend(agents.iter().cloned());
        let panes: Vec<Value> = all.iter().map(|a| json!({"workspace_id": a["workspace_id"], "tab_id": a["tab_id"], "pane_id": a["pane_id"], "terminal_id": a["terminal_id"], "cwd": a["cwd"]})).collect();
        fs::write(self.path(&format!("{name}.agents")), Value::from(all).to_string()).unwrap();
        fs::write(self.path(&format!("{name}.panes")), Value::from(panes).to_string()).unwrap();
    }
    fn calls(&self) -> Vec<String> { fs::read_to_string(self.path("calls")).unwrap_or_default().lines().map(str::to_owned).collect() }
    fn count(&self, needle: &str) -> usize { self.calls().iter().filter(|c| c.contains(needle)).count() }
    fn thread(&self, slug: &str, id: &str) -> toml::Value {
        toml::from_str(&fs::read_to_string(self.project(slug).join(format!("threads/{id}.toml"))).unwrap()).unwrap()
    }
    fn threads(&self, slug: &str) -> usize {
        fs::read_dir(self.project(slug).join("threads")).unwrap().flatten().filter(|e| e.path().extension().is_some_and(|x| x == "toml")).count()
    }
    /// A foreground `ticker run`, so `thread adopt` finds it holding the lock
    /// instead of spawning a detached one.
    fn ticker(&self) -> Ticker<'_> {
        let spawn = || self.command().args(["ticker", "run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        let mut child = spawn();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !String::from_utf8_lossy(&self.cli(&["ticker", "status"]).stdout).contains("ticker: running") {
            assert!(Instant::now() < deadline, "ticker never took its lock");
            if child.try_wait().unwrap().is_some() { child = spawn(); }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ticker { lab: self, child }
    }
}

struct Ticker<'a> { lab: &'a Lab, child: std::process::Child }
impl Ticker<'_> {
    fn wait_for(&mut self, what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !done() {
            assert!(self.child.try_wait().unwrap().is_none(), "ticker exited while waiting for {what}");
            assert!(Instant::now() < deadline, "timed out waiting for {what}; ticker log:\n{}\ncalls:\n{}",
                fs::read_to_string(self.lab.root().join(".ticker.log")).unwrap_or_default(), self.lab.calls().join("\n"));
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    /// Waits until session `name` has been listed twice more.
    fn another_pass(&mut self, name: &str) {
        let polls = || self.lab.calls().iter().filter(|c| c.starts_with(&format!("{name} ")) && c.contains("pane")).count();
        let seen = polls();
        self.wait_for("two more observations", || polls() >= seen + 2);
    }
}
impl Drop for Ticker<'_> { fn drop(&mut self) { let _ = self.child.kill(); let _ = self.child.wait(); } }

fn pane(id: &str, cwd: &Path) -> Value { json!({"workspace_id": "w5", "tab_id": "w5:t1", "pane_id": id, "terminal_id": format!("term-{id}"), "cwd": cwd}) }
fn agent(id: &str, cwd: &Path, name: &str, status: &str) -> Value {
    let mut agent = pane(id, cwd);
    if id == "p" { agent["workspace_id"] = json!("w"); agent["tab_id"] = json!("w:t"); }
    agent["name"] = json!(name);
    agent["agent"] = json!("claude");
    agent["agent_status"] = json!(status);
    agent
}
fn launch_line(slug: &str, id: &str) -> String { format!("agent prompt w5:p1 Read .herdr-project/{slug}-{id}/brief.md and do what it says.") }

/// Replaces `adopting_a_ready_agent_writes_a_brief_and_prompts_it`.
///
/// A ready agent is adopted with a task file: its brief carries the task, it
/// gets the launch line once, and nothing is started. A second agent in the
/// same directory, which herdr reports without a name, gets its own thread
/// directory and keeps the empty name.
#[test]
fn adopting_ready_agents_briefs_and_prompts_each_in_its_own_directory() {
    let mut lab = Lab::new();
    let project = lab.project_in_session("demo", "a");
    let work = lab.path("work");
    fs::create_dir(&work).unwrap();
    lab.session("a", &project, &[agent("w5:p1", &work, "my-agent", "idle"), agent("w5:p2", &work, "", "idle")]);
    fs::write(lab.path("task.md"), "Finish the refactor.").unwrap();
    let _ticker = lab.ticker();

    let first: Value = serde_json::from_str(&lab.ok(&["thread", "adopt", "demo", "--pane", "w5:p1", "--title", "Adopted work", "--task-file", lab.path("task.md").to_str().unwrap()])).unwrap();
    let record = lab.thread("demo", "t-0001");
    assert_eq!((first["id"].as_str(), record["kind"].as_str(), record["status"].as_str(), record.get("prompt_pending").and_then(|v| v.as_bool()).unwrap_or(false)),
        (Some("t-0001"), Some("adopted"), Some("open"), false), "{record}");
    assert_eq!(record["agent_name"].as_str(), Some("my-agent"));
    let dir = work.join(".herdr-project/demo-t-0001");
    assert_eq!(record["thread_dir"].as_str(), dir.to_str());
    let brief = fs::read_to_string(dir.join("brief.md")).unwrap();
    assert!(brief.contains("Finish the refactor."), "{brief}");
    assert_eq!(fs::read_to_string(project.join("threads/t-0001.task.md")).unwrap(), "Finish the refactor.");
    assert!(dir.join("library").is_dir());

    let second: Value = serde_json::from_str(&lab.ok(&["thread", "adopt", "demo", "--pane", "w5:p2", "--title", "Second"])).unwrap();
    assert_eq!(second["id"].as_str(), Some("t-0002"));
    let record = lab.thread("demo", "t-0002");
    assert_eq!(record["thread_dir"].as_str(), work.join(".herdr-project/demo-t-0002").to_str());
    assert_eq!(record.get("agent_name").and_then(|v| v.as_str()).unwrap_or_default(), "");
    assert!(work.join(".herdr-project/demo-t-0002/brief.md").is_file());

    assert_eq!(lab.count(&launch_line("demo", "t-0001")), 1, "{:?}", lab.calls());
    assert_eq!(lab.count("agent prompt w5:p2 Read .herdr-project/demo-t-0002/brief.md"), 1, "{:?}", lab.calls());
    assert_eq!(lab.count("agent start") + lab.count("agent.start"), 0, "{:?}", lab.calls());
}

/// Replaces `a_busy_agent_gets_its_prompt_later_even_when_it_ends_in_done`.
///
/// A working agent is adopted without a prompt. The ticker holds the launch
/// line while it works and sends it once the agent ends in `done`; it never
/// starts an agent in the adopted pane.
#[test]
fn a_busy_adopted_agent_is_prompted_by_the_ticker_once_it_is_done() {
    let mut lab = Lab::new();
    let project = lab.project_in_session("demo", "a");
    let work = lab.path("work");
    fs::create_dir(&work).unwrap();
    lab.session("a", &project, &[agent("w5:p1", &work, "my-agent", "working")]);
    let mut ticker = lab.ticker();

    let adopted: Value = serde_json::from_str(&lab.ok(&["thread", "adopt", "demo", "--pane", "w5:p1", "--title", "Busy"])).unwrap();
    assert_eq!(adopted["prompt_pending"].as_bool(), Some(true));
    ticker.another_pass("a");
    assert_eq!(lab.count("agent prompt") + lab.count("agent.prompt"), 0, "{:?}", lab.calls());
    assert_eq!(lab.thread("demo", "t-0001")["prompt_pending"].as_bool(), Some(true));

    lab.session("a", &project, &[agent("w5:p1", &work, "my-agent", "done")]);
    ticker.wait_for("the delivered launch line", || lab.thread("demo", "t-0001").get("prompt_pending").and_then(|v| v.as_bool()) != Some(true));
    ticker.another_pass("a");
    assert_eq!(lab.count("agent prompt") + lab.count("agent.prompt"), 1, "{:?}", lab.calls());
    assert_eq!(lab.count("agent start") + lab.count("agent.start"), 0, "{:?}", lab.calls());
}

/// Replaces `refusals_happen_before_anything_is_created`.
///
/// A pane without a detected agent and a pane that is already a thread are
/// refused before any thread is recorded. The same pane id in another
/// session is another pane, and is adoptable there.
#[test]
fn adopt_refuses_panes_without_an_agent_or_already_adopted_before_recording_anything() {
    let mut lab = Lab::new();
    let demo = lab.project_in_session("demo", "a");
    let other = lab.project_in_session("other", "b");
    let work = lab.path("work");
    fs::create_dir(&work).unwrap();
    lab.session("a", &demo, &[agent("w5:p1", &work, "my-agent", "idle")]);
    lab.session("b", &other, &[agent("w5:p1", &work, "my-agent", "idle")]);
    let _ticker = lab.ticker();

    let stderr = lab.refused(&["thread", "adopt", "demo", "--pane", "w9:p9", "--title", "x"]);
    assert!(stderr.contains("no agent is detected in pane w9:p9"), "{stderr}");
    assert_eq!(lab.threads("demo"), 0);

    lab.ok(&["thread", "adopt", "demo", "--pane", "w5:p1", "--title", "first"]);
    let stderr = lab.refused(&["thread", "adopt", "demo", "--pane", "w5:p1", "--title", "again"]);
    assert!(stderr.contains("pane w5:p1 is already thread t-0001 of `demo`"), "{stderr}");
    assert_eq!(lab.threads("demo"), 1);
    assert!(!work.join(".herdr-project/demo-t-0002").exists());

    let adopted: Value = serde_json::from_str(&lab.ok(&["thread", "adopt", "other", "--pane", "w5:p1", "--title", "other session"])).unwrap();
    assert_eq!(adopted["id"].as_str(), Some("t-0001"));
    assert_eq!(lab.thread("other", "t-0001")["pane_id"].as_str(), Some("w5:p1"));
}

/// Replaces `adopt_workspace_refuses_without_an_agent_and_creates_nothing`.
#[test]
fn adopt_workspace_without_an_agent_creates_no_project() {
    let mut lab = Lab::new();
    let demo = lab.project_in_session("demo", "a");
    lab.session("a", &demo, &[]);
    let out = lab.cli(&["adopt-workspace", "--name", "From Workspace", "--pane", "w9:p9", "--socket", lab.socket("a").to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no agent is detected in pane w9:p9"), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!lab.project("from-workspace").exists());
    assert!(lab.calls().iter().all(|c| c.ends_with("agent list") || c.ends_with("--version")), "{:?}", lab.calls());
}

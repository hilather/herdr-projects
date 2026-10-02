#![cfg(target_os = "linux")]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! Agent starts through the compiled CLI: `ticker run` passes and `thread
//! restart`, against a fake herdr that serves its API bridge from files and
//! logs every start. Waits are on persisted records and logged calls.
use serde_json::{json, Value};
use std::{fs, os::unix::{fs::PermissionsExt, net::UnixListener}, path::PathBuf, process::{Command, Stdio}, time::{Duration, Instant}};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-farm");

/// Lists `agents.json` and `panes.json`. `agent.start` over the bridge is
/// logged to `started` and puts a launching agent in the pane; its reply is
/// lost (the bridge exits) while `lost` exists.
const FAKE_HERDR: &str = r#"#!/usr/bin/python3
import json,os,pathlib,sys
home=pathlib.Path(os.environ['HOME']);args=sys.argv[1:]
agents=json.loads((home/'agents.json').read_text());panes=json.loads((home/'panes.json').read_text())
if args[-2:]==['pane','list']:
    with open(home/'polls','a') as f:f.write('poll\n')
if args==['--version']:print('herdr 0.9.1')
elif args==['remote-api-bridge','--check']:print('herdr-api-bridge-v1')
elif args in (['agent','list'],['pane','list']):print(json.dumps({'result':{args[0]+'s':agents if args[0]=='agent' else panes}}))
elif args==['remote-api-bridge']:
    r=json.loads(sys.stdin.readline());method=r['method']
    if method=='agent.list':result={'agents':agents}
    elif method=='pane.list':result={'panes':panes}
    elif method=='agent.start':
        with open(home/'started','a') as f:f.write(json.dumps(r['params'])+'\n')
        agent=dict(panes[0],name=r['params']['name'],agent='claude',agent_status='blocked',launch_pending=True)
        (home/'agents.json').write_text(json.dumps([agent]))
        if (home/'lost').exists():sys.exit(1)
        agent=dict(agent,agent_status='unknown');del agent['agent']
        result={'type':'agent_started','agent':agent,'argv':['claude']}
    else:
        print(json.dumps({'id':r['id'],'error':{'code':'unsupported','message':'not in fixture'}}));sys.exit(0)
    print(json.dumps({'id':r['id'],'result':result}))
else:print('{"result":{"shown":true}}')
"#;

struct Lab { home: tempfile::TempDir, _listener: UnixListener }

impl Lab {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let socket = home.path().join("session.sock");
        let lab = Lab { _listener: UnixListener::bind(&socket).unwrap(), home };
        fs::write(lab.path("herdr"), FAKE_HERDR).unwrap();
        fs::set_permissions(lab.path("herdr"), fs::Permissions::from_mode(0o700)).unwrap();
        let out = lab.command().args(["new", "demo"]).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        fs::write(lab.project().join(".state/coordinator.json"), json!({"socket": socket}).to_string()).unwrap();
        lab
    }
    fn path(&self, name: &str) -> PathBuf { self.home.path().join(name) }
    fn project(&self) -> PathBuf { self.path("root/demo") }
    fn command(&self) -> Command {
        let mut command = Command::new(BIN);
        command.env_clear().env("HOME", self.home.path()).env("PATH", "/usr/bin:/bin").env("HERDR_BIN_PATH", self.path("herdr"))
            .arg("--root").arg(self.path("root"));
        command
    }
    fn record(&self) -> toml::Value { toml::from_str(&fs::read_to_string(self.project().join("threads/t-0001.toml")).unwrap()).unwrap() }
    fn get(&self, path: &[&str]) -> Option<toml::Value> { path.iter().try_fold(self.record(), |v, k| v.get(k).cloned()) }
    fn starts(&self) -> Vec<Value> {
        fs::read_to_string(self.path("started")).unwrap_or_default().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }
    fn polls(&self) -> usize { fs::read_to_string(self.path("polls")).unwrap_or_default().lines().count() }
    fn notices(&self) -> usize {
        fs::read_dir(self.project().join("inbox")).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with("launch-")).count()
    }
    /// Whether a ticker holds this root's lock.
    fn ticker_running(&self) -> bool {
        !Command::new("flock").args(["-n", "-s"]).arg(self.path("root/.ticker.lock")).arg("true").status().is_ok_and(|s| s.success())
    }
    /// Waits for `done` while a ticker that this test or `thread restart`
    /// started runs, then stops it through its stop file.
    fn until(&self, what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(120);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}; ticker log:\n{}", fs::read_to_string(self.path("root/.ticker.log")).unwrap_or_default());
            std::thread::sleep(Duration::from_millis(20));
        }
        let polls = self.polls();
        while self.polls() < polls + 2 { assert!(Instant::now() < deadline, "no pass after {what}"); std::thread::sleep(Duration::from_millis(20)); }
        fs::write(self.path("root/.ticker.stop"), b"").unwrap();
        while self.ticker_running() {
            assert!(Instant::now() < deadline, "ticker ignored its stop file");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// A ticker that `thread restart` detached is stopped even when a test fails.
impl Drop for Lab {
    fn drop(&mut self) {
        let _ = fs::write(self.path("root/.ticker.stop"), b"");
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && self.ticker_running() {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// An agent start whose reply is lost fails the thread once, with one notice,
/// and the ticker never starts it again by itself, even after a restart.
/// `thread restart` is the explicit new execution: its ticker starts the
/// agent once more, and that start is confirmed.
#[test]
fn a_lost_start_is_reported_once_and_only_a_restart_starts_the_agent_again() {
    let lab = Lab::new();
    let source = lab.path("source");
    fs::create_dir(&source).unwrap();
    fs::write(lab.path("agents.json"), "[]").unwrap();
    fs::write(lab.path("panes.json"), json!([{"workspace_id": "w", "tab_id": "tab", "pane_id": "p", "terminal_id": "terminal", "cwd": source}]).to_string()).unwrap();
    fs::write(lab.project().join("threads/t-0001.toml"), toml::to_string(&json!({"id": "t-0001", "title": "Task", "status": "open", "kind": "tab",
        "prompt_pending": true, "thread_dir": source, "cwd": source, "workspace_id": "w", "tab_id": "tab", "pane_id": "p", "agent": "claude",
        "agent_name": "worker", "created": jiff::Timestamp::now().to_string()})).unwrap()).unwrap();
    fs::write(lab.path("lost"), b"").unwrap();

    for _ in 0..2 {
        let mut child = lab.command().args(["ticker", "run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        lab.until("the reported lost start", || lab.get(&["launch_claim", "notified"]) == Some(true.into()));
        child.wait().unwrap();
        let _ = fs::remove_file(lab.path("root/.ticker.stop"));
    }
    assert_eq!(lab.starts().len(), 1);
    assert_eq!((lab.get(&["status"]), lab.get(&["launch_claim", "phase"])), (Some("failed".into()), Some("uncertain".into())));
    assert_eq!(lab.notices(), 1);
    let generation = lab.get(&["lifecycle_generation"]).and_then(|g| g.as_integer()).unwrap_or(0);

    // The user closes the half-started agent and restarts the thread.
    fs::write(lab.path("agents.json"), "[]").unwrap();
    fs::remove_file(lab.path("lost")).unwrap();
    let out = lab.command().args(["thread", "restart", "demo", "t-0001"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    lab.until("the second start", || lab.get(&["launch_claim", "phase"]) == Some("confirmed".into()));
    let _ = fs::remove_file(lab.path("root/.ticker.stop"));
    let starts = lab.starts();
    assert_eq!(starts.len(), 2, "{starts:?}");
    assert_eq!(starts[1]["name"], "hp-demo-t-0001");
    assert_eq!((lab.get(&["status"]), lab.get(&["launch_sequence"])), (Some("open".into()), Some(2.into())));
    assert_eq!(lab.get(&["launch_claim", "generation"]), lab.get(&["lifecycle_generation"]));
    assert!(lab.get(&["lifecycle_generation"]).and_then(|g| g.as_integer()).unwrap() > generation);
    assert_eq!(lab.notices(), 1);
}

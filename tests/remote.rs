#![cfg(target_os = "linux")]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! Which SSH target a saved machine resolves to, through the compiled CLI:
//! `doctor` resolves every machine an open remote thread uses, exactly as the
//! copies and report polls do. herdr is a fake whose `machine list --json`
//! answers from `machines.json` (and fails when it is missing).
use serde_json::{json, Value};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, process::Command};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-projects");

const FAKE_HERDR: &str = "#!/bin/sh\ncase \"$*\" in\n--version) echo 'herdr 0.9.1';;\n'machine list --json') cat \"$HOME/machines.json\" || exit 1;;\n*) echo '{\"result\":{}}';;\nesac\n";

struct Lab { home: tempfile::TempDir }

impl Lab {
    /// Project `demo` with one open remote thread on each machine in `machines`.
    fn new(machines: &[&str]) -> Self {
        let lab = Lab { home: tempfile::tempdir().unwrap() };
        fs::write(lab.path("herdr"), FAKE_HERDR).unwrap();
        fs::set_permissions(lab.path("herdr"), fs::Permissions::from_mode(0o700)).unwrap();
        assert!(lab.command().args(["new", "demo"]).output().unwrap().status.success());
        for (n, machine) in machines.iter().enumerate() {
            let id = format!("t-{:04}", n + 1);
            let record = json!({"id": id, "title": machine, "status": "open", "kind": "adopted", "created": jiff::Timestamp::now().to_string(),
                "machine": machine, "agent": "claude", "workspace_id": "w", "tab_id": "w:t", "pane_id": format!("p{n}"), "cwd": "/remote/work"});
            fs::write(lab.path("root/demo/threads").join(format!("{id}.toml")), toml::to_string(&record).unwrap()).unwrap();
        }
        lab
    }
    fn path(&self, name: &str) -> PathBuf { self.home.path().join(name) }
    fn command(&self) -> Command {
        let mut command = Command::new(BIN);
        command.env_clear().env("HOME", self.home.path()).env("PATH", "/usr/bin:/bin").env("HERDR_BIN_PATH", self.path("herdr")).arg("--root").arg(self.path("root"));
        command
    }
    fn listing(&self, machines: Option<Value>) {
        match machines { Some(list) => fs::write(self.path("machines.json"), list.to_string()).unwrap(), None => { let _ = fs::remove_file(self.path("machines.json")); } }
    }
    fn config(&self, text: &str) {
        let dir = self.path(".config/herdr-projects");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("config.toml"), text).unwrap();
    }
    /// `doctor`'s verdict on each machine: `ok TARGET` or `FAIL ERROR`.
    fn machines(&self) -> std::collections::BTreeMap<String, String> {
        let out = self.command().arg("doctor").output().unwrap();
        String::from_utf8(out.stdout).unwrap().lines().filter_map(|line| {
            let (mark, rest) = line.strip_prefix('[')?.split_once("] machine ")?;
            let (machine, detail) = rest.split_once(": ")?;
            Some((machine.to_string(), format!("{} {}", mark.trim(), detail.strip_prefix("ssh target ").unwrap_or(detail))))
        }).collect()
    }
}

fn expect(lab: &Lab, cases: &[(&str, &str)]) {
    let found = lab.machines();
    for (machine, verdict) in cases {
        let got = found.get(*machine).unwrap_or_else(|| panic!("no verdict for {machine}: {found:?}"));
        assert!(got.starts_with(verdict), "{machine}: expected {verdict}, got {got}");
    }
}

/// Replaces `target_comes_from_herdr_then_from_config` and
/// `saved_route_selection_matches_id_precedence_and_refuses_ambiguous_or_disabled_profiles`.
///
/// herdr's saved machines are matched by profile id first, then by label; a
/// machine herdr does not list falls back to `[machines.NAME] ssh` in
/// `config.toml`, also when the listing itself fails. An ambiguous label, a
/// disabled profile or a listing with duplicate ids is refused and never
/// falls back to the config file.
#[test]
fn saved_machines_resolve_by_id_then_label_then_config_and_refuse_ambiguity() {
    let lab = Lab::new(&["m1", "abc", "chosen", "box", "nope", "same", "off"]);
    lab.config("[machines.box]\nssh = \"me@box.local\"\n[machines.same]\nssh = \"config-same\"\n[machines.off]\nssh = \"config-off\"\n[machines.chosen]\nssh = \"config-chosen\"\n");
    lab.listing(Some(json!([
        {"id": "abc", "label": "m1", "target": "m1.local", "session": "default"},
        {"id": "chosen", "label": "first", "target": "correct.local"},
        {"id": "other", "label": "chosen", "target": "wrong.local"},
        {"id": "s1", "label": "same", "target": "a.local"},
        {"id": "s2", "label": "same", "target": "b.local"},
        {"id": "o1", "label": "off", "target": "off.local", "enabled": false},
    ])));
    expect(&lab, &[
        ("m1", "ok m1.local"), ("abc", "ok m1.local"), ("chosen", "ok correct.local"), ("box", "ok me@box.local"),
        ("nope", "FAIL machine `nope` has no SSH target: it is not in `herdr machine list`, and config.toml has no [machines.nope] ssh"),
        ("same", "FAIL ambiguous machine label; use its profile ID"), ("off", "FAIL saved machine is disabled"),
    ]);

    // herdr cannot list its machines: only the config file answers.
    lab.listing(None);
    expect(&lab, &[("box", "ok me@box.local"), ("same", "ok config-same"), ("chosen", "ok config-chosen"), ("m1", "FAIL machine `m1` has no SSH target")]);

    // Duplicate profile ids make the whole listing untrustworthy.
    lab.listing(Some(json!([{"id": "a", "label": "m1", "target": "a.local"}, {"id": "a", "label": "other", "target": "b.local"}])));
    expect(&lab, &[("m1", "FAIL duplicate saved machine ID"), ("box", "FAIL duplicate saved machine ID")]);
}

/// A project whose adopted thread on saved machine `machine` waits for its
/// brief, served by a `ticker run`. The local herdr answers `machine list
/// --json` from `routes.json` (failing when it is missing) and lists the
/// remote agent through `--machine`; the remote bridge is reached through a
/// fake `ssh` that logs its argv and runs the command it was given.
struct Brief { home: tempfile::TempDir, ticker: Option<std::process::Child>, _listener: std::os::unix::net::UnixListener }

const BRIEF_HERDR: &str = r#"#!/usr/bin/python3
import os,sys,pathlib
root=pathlib.Path(os.environ['HOME']);args=sys.argv[1:]
if args[:1]==['--machine']:args=args[2:]
if args==['machine','list','--json']:print((root/'routes.json').read_text())
elif args in (['agent','list'],['pane','list']):
 with open(root/'polls','a') as f:f.write('poll\n')
 print((root/(args[0]+'s.json')).read_text())
elif args[:2] in [['agent','prompt'],['agent','start']]:(root/'WRONG_SYNC_EFFECT').touch();sys.exit(2)
else:print('{"result":{"shown":true}}')
"#;

const BRIEF_BRIDGE: &str = r#"#!/usr/bin/python3
import os,json,sys,pathlib
root=pathlib.Path(os.environ['HOME'])
with open(root/'bridge-calls','a') as f:f.write(json.dumps(sys.argv[1:])+'\n')
assert sys.argv[1:4]==['--session',(root/'session').read_text(),'remote-api-bridge'] and 'HERDR_SESSION' not in os.environ
if sys.argv[4:]==['--check']:print('herdr-api-bridge-v1');sys.exit(0)
r=json.loads(sys.stdin.readline())
if r['method'] in ('agent.list','pane.list'):result=json.loads((root/(r['method'].split('.')[0]+'s.json')).read_text())['result']
elif r['method']=='agent.prompt':
 with open(root/'sent','a') as f:f.write('send')
 result={'type':'agent_prompted','agent':json.loads((root/'agents.json').read_text())['result']['agents'][0]}
else:sys.exit(3)
print(json.dumps({'id':r['id'],'result':result}))
"#;

const BRIEF_SSH: &str = r#"#!/usr/bin/python3
import os,json,sys,subprocess,pathlib
root=pathlib.Path(os.environ['HOME'])
with open(root/'ssh-calls','a') as f:f.write(json.dumps(sys.argv[1:])+'\n')
if sys.argv[-2]!='fixture.invalid':sys.exit(255)
sys.exit(subprocess.call(sys.argv[-1],shell=True,env=dict(os.environ,HERDR_SESSION='inherited')))
"#;

impl Brief {
    fn new(machine: &str, routes: Option<Value>, bridge: &str, session: &str) -> Self {
        let home = tempfile::tempdir().unwrap();
        let path = |name: &str| home.path().join(name);
        let write = |name: &str, text: &str| { fs::write(path(name), text).unwrap(); fs::set_permissions(path(name), fs::Permissions::from_mode(0o700)).unwrap(); };
        fs::create_dir(path("bin")).unwrap();
        write("herdr", BRIEF_HERDR);
        write("bin/ssh", BRIEF_SSH);
        write("remote herdr's bin", BRIEF_BRIDGE);
        fs::write(path("session"), session).unwrap();
        if let Some(routes) = routes { fs::write(path("routes.json"), routes.to_string()).unwrap(); }
        let source = path("source");
        fs::create_dir(&source).unwrap();
        let agent = json!({"workspace_id":"w","tab_id":"tab","pane_id":"p","cwd":source,"name":"worker","agent":"claude","agent_status":"idle"});
        fs::write(path("agents.json"), json!({"result":{"agents":[agent]}}).to_string()).unwrap();
        fs::write(path("panes.json"), json!({"result":{"panes":[{"workspace_id":"w","tab_id":"tab","pane_id":"p","cwd":source}]}}).to_string()).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(path("session.sock")).unwrap();
        let mut brief = Brief { home, ticker: None, _listener: listener };
        assert!(brief.command().args(["new", "demo"]).status().unwrap().success());
        let project = brief.path("root/demo");
        fs::write(project.join(".state/coordinator.json"), json!({"socket": brief.path("session.sock")}).to_string()).unwrap();
        fs::write(project.join("threads/t-0001.toml"), toml::to_string(&json!({"id":"t-0001","status":"open","kind":"adopted","prompt_pending":true,"machine":machine,
            "thread_dir":source,"cwd":source,"workspace_id":"w","tab_id":"tab","pane_id":"p","agent":"claude","agent_name":"worker","created":jiff::Timestamp::now().to_string()})).unwrap()).unwrap();
        let bridge = if bridge.is_empty() { brief.path("remote herdr's bin").display().to_string() } else { bridge.to_owned() };
        brief.ticker = Some(brief.command().env("HERDR_PROJECTS_REMOTE_HERDR_BIN", bridge).args(["ticker", "run"])
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap());
        brief
    }
    fn path(&self, name: &str) -> PathBuf { self.home.path().join(name) }
    fn command(&self) -> Command {
        let mut command = Command::new(BIN);
        command.env_clear().env("HOME", self.home.path()).env("PATH", format!("{}:/usr/bin:/bin", self.path("bin").display()))
            .env("HERDR_BIN_PATH", self.path("herdr")).arg("--root").arg(self.path("root"));
        command
    }
    fn polls(&self) -> usize { fs::read_to_string(self.path("polls")).unwrap_or_default().lines().count() }
    fn sent(&self) -> bool { self.path("sent").exists() }
    fn log(&self, name: &str) -> Vec<Vec<String>> {
        fs::read_to_string(self.path(name)).unwrap_or_default().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }
    fn pending(&self) -> Option<bool> {
        let record: toml::Value = toml::from_str(&fs::read_to_string(self.path("root/demo/threads/t-0001.toml")).unwrap()).unwrap();
        record.get("prompt_pending").and_then(|v| v.as_bool())
    }
    fn wait(&mut self, what: &str, done: impl Fn(&Self) -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !done(self) {
            assert!(self.ticker.as_mut().unwrap().try_wait().unwrap().is_none(), "ticker exited");
            assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}: {}", fs::read_to_string(self.path("root/.ticker.log")).unwrap_or_default());
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}
impl Drop for Brief {
    fn drop(&mut self) { if let Some(child) = self.ticker.as_mut() { let _ = child.kill(); let _ = child.wait(); } }
}

/// Replaces `unavailable_ambiguous_disabled_and_malformed_inventory_refuse`
/// and `bridge_command_freezes_host_session_and_quotes_remote_binary`.
///
/// A remote brief is delivered only over a route from a trustworthy saved
/// machine listing: an ambiguous label, a disabled machine, a listing with an
/// invalid session, a duplicate id or an older field set, a failed listing
/// and a remote herdr path that looks like an option all deliver nothing.
/// The one route that works reaches the bridge through
/// `ssh -T -o StrictHostKeyChecking=yes`, names its session even when it is
/// `default`, clears inherited session selectors and quotes a remote path
/// with a space and a quote.
#[test]
fn remote_briefs_use_only_a_trustworthy_route_and_a_quoted_bridge() {
    let route = |id: &str, label: &str, session: &str, enabled: bool| json!({"id": id.repeat(32), "label": label, "target": "fixture.invalid", "session": session, "enabled": enabled});
    let good = route("a", "good", "default", true);
    let refused = [
        Brief::new("same", Some(json!([route("a", "same", "default", true), route("b", "same", "default", true)])), "", "default"),
        Brief::new("off", Some(json!([route("a", "off", "default", false)])), "", "default"),
        Brief::new("good", Some(json!([good, route("b", "other", "../another", true)])), "", "default"),
        Brief::new("good", Some(json!([good, route("a", "other", "default", true)])), "", "default"),
        Brief::new("good", Some(json!([{"id": "a", "label": "good", "target": "fixture.invalid"}])), "", "default"),
        Brief::new("good", None, "", "default"),
        Brief::new("good", Some(json!([good])), "--malicious", "default"),
    ];
    let mut delivered = Brief::new(&"a".repeat(32), Some(json!([good])), "", "default");
    delivered.wait("the brief", |b| b.pending() == Some(false));
    assert!(delivered.sent());
    let ssh = delivered.log("ssh-calls");
    let bridge: Vec<_> = ssh.iter().filter(|argv| argv.last().unwrap().contains("remote-api-bridge")).collect();
    assert!(!bridge.is_empty() && bridge.iter().all(|argv| argv[..3] == ["-T", "-o", "StrictHostKeyChecking=yes"] && argv[argv.len() - 2] == "fixture.invalid"), "{ssh:?}");
    assert!(ssh.iter().flatten().all(|arg| arg != "--machine"), "{ssh:?}");
    assert!(delivered.log("bridge-calls").iter().any(|argv| argv == &["--session", "default", "remote-api-bridge"]));
    assert!(!delivered.path("WRONG_SYNC_EFFECT").exists());

    // The refused projects started together with the delivered one; each
    // gets at least one more pass than it needed.
    let seen: Vec<usize> = refused.iter().map(Brief::polls).collect();
    for (n, (mut brief, seen)) in refused.into_iter().zip(seen).enumerate() {
        brief.wait("another pass", |b| b.polls() >= seen + 4);
        assert!(!brief.sent() && brief.pending() == Some(true), "case {n} delivered its brief");
        assert!(brief.log("bridge-calls").iter().all(|argv| argv.last().map(String::as_str) == Some("--check") || argv.len() == 3), "case {n}");
        assert!(!brief.path("WRONG_SYNC_EFFECT").exists(), "case {n}");
    }
}

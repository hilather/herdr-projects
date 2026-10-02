#![cfg(target_os = "linux")]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! Routine files through the compiled CLI: `routine list` and `routine
//! approve`, and `ticker run` writing inbox items for due routines and for
//! routine files that do not parse, against a fake herdr session.
use serde_json::{json, Value};
use std::{fs, os::unix::{fs::PermissionsExt, net::UnixListener}, path::PathBuf, process::{Command, Output, Stdio}, time::{Duration, Instant}};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-farm");

/// One session: lists `$HOME/agents` and `$HOME/panes`, and acknowledges
/// nothing else (no test here needs an effect).
const FAKE_HERDR: &str = r#"#!/usr/bin/python3
import json,os,pathlib,sys
home=pathlib.Path(os.environ['HOME']);args=sys.argv[1:]
def read(kind):
    p=home/kind
    return json.loads(p.read_text()) if p.exists() else []
with open(home/'calls','a') as f:f.write(' '.join(args)+'\n')
if args==['--version']:print('herdr 0.9.1')
elif args==['remote-api-bridge','--check']:print('herdr-api-bridge-v1')
elif args==['agent','list']:print(json.dumps({'result':{'agents':read('agents')}}))
elif args==['pane','list']:print(json.dumps({'result':{'panes':read('panes')}}))
elif args==['remote-api-bridge']:
    request=json.loads(sys.stdin.readline());kind=request['method'].split('.')[0]+'s'
    print(json.dumps({'id':request['id'],'result':{kind:read(kind)}}))
else:print('{"result":{}}')
"#;

struct Lab { home: tempfile::TempDir, _listener: Option<UnixListener> }

impl Lab {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        fs::write(home.path().join("herdr"), FAKE_HERDR).unwrap();
        fs::set_permissions(home.path().join("herdr"), fs::Permissions::from_mode(0o700)).unwrap();
        let lab = Lab { home, _listener: None };
        lab.ok(&["new", "demo"]);
        lab
    }
    fn path(&self, rel: &str) -> PathBuf { self.home.path().join(rel) }
    fn project(&self) -> PathBuf { self.path("root/demo") }
    fn command(&self) -> Command {
        let mut command = Command::new(BIN);
        command.env_clear().env("HOME", self.home.path()).env("PATH", "/usr/bin:/bin").env("TZ", "UTC")
            .env("HERDR_BIN_PATH", self.path("herdr")).arg("--root").arg(self.path("root"));
        command
    }
    fn cli(&self, args: &[&str]) -> Output { self.command().args(args).stdin(Stdio::null()).output().unwrap() }
    fn ok(&self, args: &[&str]) -> String {
        let out = self.cli(args);
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }
    fn routine(&self, name: &str, text: &str) { fs::write(self.project().join(format!("routines/{name}.md")), text).unwrap(); }
    fn safety(&self, routine_commands: bool) {
        let canonical = fs::canonicalize(self.project()).unwrap();
        fs::create_dir_all(self.path(".config/herdr-farm")).unwrap();
        fs::write(self.path(".config/herdr-farm/config.toml"), format!("[safety.{:?}]\nroutine_commands = {routine_commands}\n", canonical.display().to_string())).unwrap();
    }
    fn list(&self) -> Vec<String> { self.ok(&["routine", "list", "demo"]).lines().map(str::to_string).collect() }
    /// A reachable coordinator session for the project, idle in pane `p`.
    fn session(&mut self) {
        let socket = self.path("demo.sock");
        self._listener = Some(UnixListener::bind(&socket).unwrap());
        let pane = json!({"workspace_id": "w", "tab_id": "w:t", "pane_id": "p", "terminal_id": "term-p", "cwd": self.project()});
        let mut agent = pane.clone();
        for (k, v) in [("name", "coordinator"), ("agent", "claude"), ("agent_status", "idle")] { agent[k] = json!(v); }
        fs::write(self.path("panes"), json!([pane]).to_string()).unwrap();
        fs::write(self.path("agents"), json!([agent]).to_string()).unwrap();
        fs::write(self.project().join(".state/coordinator.json"), json!({"socket": socket, "workspace_id": "w", "tab_id": "w:t", "pane_id": "p",
            "agent_name": "coordinator", "cwd": self.project(), "prime_pending": false}).to_string()).unwrap();
    }
    /// Inbox items as (kind, subject, summary, body).
    fn items(&self) -> Vec<(String, String, String, String)> {
        let mut items: Vec<_> = fs::read_dir(self.project().join("inbox")).unwrap().flatten().filter(|e| e.path().is_file()).map(|e| {
            let text = fs::read_to_string(e.path()).unwrap();
            let (front, body) = text.trim_start_matches("+++\n").split_once("+++\n").unwrap();
            let front: toml::Value = toml::from_str(front).unwrap();
            let field = |k: &str| front[k].as_str().unwrap().to_string();
            (field("kind"), field("subject"), field("summary"), body.trim().to_string())
        }).collect();
        items.sort();
        items
    }
    fn count(&self, kind: &str, subject: &str) -> usize { self.items().iter().filter(|i| i.0 == kind && i.1 == subject).count() }
    fn state(&self) -> Value { serde_json::from_slice(&fs::read(self.project().join(".state/ticker.json")).unwrap_or_else(|_| b"{}".to_vec())).unwrap() }
}

/// A `ticker run` child; stopped through its stop file, killed on panic.
struct Ticker<'a> { lab: &'a Lab, child: std::process::Child }
impl Ticker<'_> {
    fn start(lab: &Lab) -> Ticker<'_> {
        Ticker { lab, child: lab.command().args(["ticker", "run"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap() }
    }
    fn wait_for(&mut self, what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(90);
        while !done() {
            assert!(self.child.try_wait().unwrap().is_none(), "ticker exited while waiting for {what}");
            assert!(Instant::now() < deadline, "timed out waiting for {what}; items: {:?}\nlog:\n{}", self.lab.items(),
                fs::read_to_string(self.lab.path("root/.ticker.log")).unwrap_or_default());
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn stop(mut self) {
        let stop = self.lab.path("root/.ticker.stop");
        fs::write(&stop, b"").unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        while self.child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "ticker ignored its stop file");
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = fs::remove_file(stop);
    }
}
impl Drop for Ticker<'_> { fn drop(&mut self) { let _ = self.child.kill(); let _ = self.child.wait(); } }

const BAD_SCHEDULES: [&str; 11] = ["", "every", "every 0m", "every -1h", "every 5x", "every m", "daily 24:00", "daily 7", "daily 07:60", "hourly", "* * * * *"];

/// `routine list` shows each usable routine with its schedule, whether it is
/// enabled and what its command may do, and each unusable file as a
/// `config-error` line naming what is wrong.
#[test]
fn routine_list_reports_schedules_commands_and_unusable_files() {
    let lab = Lab::new();
    assert_eq!(lab.list(), ["no routines"]);
    lab.routine("quick", "+++\nschedule = \"every 5m\"\n+++\nLook around.\n");
    lab.routine("slow", "+++\nschedule = \" every 2h \"\nenabled = false\n+++\n");
    lab.routine("day", "+++\nschedule = \"every 1d\"\n+++");
    lab.routine("nightly", "+++\nschedule = \"daily 07:30\"\ncommand = \"./check.sh\"\n+++\n\nLook at the output.\n");
    for (i, bad) in BAD_SCHEDULES.iter().enumerate() { lab.routine(&format!("bad-{i:02}"), &format!("+++\nschedule = {bad:?}\n+++\n")); }
    lab.routine("Bad_Name", "+++\nschedule = \"every 1h\"\n+++\nP");
    lab.routine("no-front", "no front matter");
    lab.routine("unclosed", "+++\nschedule = \"every 1h\"\n");
    lab.routine("not-toml", "+++\nschedule = \n+++\n");
    fs::write(lab.project().join("routines/notes.txt"), "not a routine").unwrap();

    let list = lab.list();
    let usable = &list[..4];
    assert_eq!(usable, [
        "day\tevery 1d\tenabled\tprompt only",
        "nightly\tdaily 07:30\tenabled\tcommand: routine_commands is false, so it does not run",
        "quick\tevery 5m\tenabled\tprompt only",
        "slow\tevery 2h\tdisabled\tprompt only",
    ]);
    let error = |file: &str| list.iter().find_map(|l| l.strip_prefix(&format!("routines/{file}.md\tconfig-error: "))).unwrap_or_else(|| panic!("{file}: {list:#?}")).to_string();
    for (i, bad) in BAD_SCHEDULES.iter().enumerate() {
        assert!(error(&format!("bad-{i:02}")).starts_with(&format!("bad schedule `{bad}`")), "{bad}: {}", error(&format!("bad-{i:02}")));
    }
    assert!(error("Bad_Name").contains("slug rule"), "{}", error("Bad_Name"));
    assert!(error("no-front").contains("must start with a `+++` line"));
    assert!(error("unclosed").contains("no closing `+++` line"));
    assert!(error("not-toml").contains("front matter does not parse"));
    let reported = list.iter().filter(|l| l.contains("\tconfig-error: ")).count();
    assert_eq!(reported, BAD_SCHEDULES.len() + 4, "notes.txt is not a routine: {list:#?}");

    lab.safety(true);
    assert!(lab.list().contains(&"nightly\tdaily 07:30\tenabled\tcommand: NOT approved (or edited since approval)".to_string()));
}

/// `routine approve` refuses when standard input is not a terminal, before
/// it asks anything, and stores no approval.
#[test]
fn routine_approve_refuses_without_a_terminal() {
    let lab = Lab::new();
    lab.safety(true);
    lab.routine("watch", "+++\nschedule = \"every 1m\"\ncommand = \"echo hi\"\n+++\n");
    for stdin in [Stdio::null(), Stdio::piped()] {
        let mut child = lab.command().args(["routine", "approve", "demo", "watch"]).stdin(stdin).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        if let Some(mut input) = child.stdin.take() { use std::io::Write; input.write_all(b"watch\n").unwrap(); }
        let out = child.wait_with_output().unwrap();
        assert!(!out.status.success());
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("must be run by a person at a terminal"), "{stderr}");
        assert!(!String::from_utf8_lossy(&out.stdout).contains("echo hi"), "the command was shown for approval");
    }
    assert!(!lab.path(".config/herdr-farm/approved-routines.json").exists());
    assert_eq!(lab.list(), ["watch\tevery 1m\tenabled\tcommand: NOT approved (or edited since approval)"]);
}

fn at(offset_secs: i64) -> jiff::Timestamp { jiff::Timestamp::now() + jiff::SignedDuration::from_secs(offset_secs) }
/// `daily HH:MM` for the UTC time `offset_secs` from now.
fn daily(offset_secs: i64) -> String { format!("daily {}", at(offset_secs).to_zoned(jiff::tz::TimeZone::UTC).strftime("%H:%M")) }

/// The ticker writes one `routine` item, with the routine's prompt, for each
/// enabled prompt routine that is due against its stored last run: `every`
/// after its interval, `daily` once its local time has passed since the last
/// run (once, however many days were missed). A routine seen for the first
/// time only records now. Unusable files get one `config-error` item per
/// distinct content, however many passes see them.
#[test]
fn ticker_writes_items_for_due_routines_and_unusable_files() {
    let mut lab = Lab::new();
    lab.session();
    let hour = 3600;
    let routines = [
        ("hourly", "every 1h".to_string(), Some(-2 * hour), true),
        ("hourly-fresh", "every 1h".to_string(), Some(-hour / 2), true),
        ("daily-due", daily(-hour), Some(-26 * hour), true),
        ("daily-ran", daily(-hour), Some(-hour / 2), true),
        ("daily-later", daily(hour), Some(-2 * hour), true),
        ("daily-missed", daily(-3 * hour), Some(-3 * 24 * hour), true),
        ("first-seen", "every 1m".to_string(), None, true),
        ("off", "every 1m".to_string(), Some(-2 * hour), false),
    ];
    let mut state = json!({"routines": {}});
    for (name, schedule, last, enabled) in &routines {
        lab.routine(name, &format!("+++\nschedule = {schedule:?}\nenabled = {enabled}\n+++\nPrompt for {name}.\n"));
        if let Some(last) = last { state["routines"][name] = json!({"last_run": at(*last).to_string()}); }
    }
    let seeded = state.clone();
    fs::write(lab.project().join(".state/ticker.json"), state.to_string()).unwrap();
    lab.routine("broken", "+++\nschedule = \"every 5x\"\n+++\n");

    let started = jiff::Timestamp::now();
    let mut ticker = Ticker::start(&lab);
    ticker.wait_for("the first pass", || lab.count("config-error", "broken") == 1 && lab.state()["routines"]["first-seen"]["last_run"].is_string());
    let due = ["daily-due", "daily-missed", "hourly"];
    let routine_items = || lab.items().into_iter().filter(|i| i.0 == "routine").collect::<Vec<_>>();
    assert_eq!(routine_items(), due.map(|n| ("routine".to_string(), n.to_string(), format!("routine `{n}` is due"), format!("Prompt for {n}."))));

    // A later pass: fixing nothing repeats nothing; a different broken text is reported once more.
    lab.routine("broken", "+++\nschedule = \"every 5y\"\n+++\n");
    ticker.wait_for("the changed broken file's item", || lab.count("config-error", "broken") == 2);
    ticker.stop();

    assert_eq!(routine_items().len(), 3, "{:?}", lab.items());
    let errors: Vec<_> = lab.items().into_iter().filter(|i| i.0 == "config-error").map(|i| i.2).collect();
    assert_eq!(errors, ["routines/broken.md is not usable: bad schedule `every 5x`: use `every <N>m`, `<N>h` or `<N>d`",
        "routines/broken.md is not usable: bad schedule `every 5y`: use `every <N>m`, `<N>h` or `<N>d`"]);
    assert_eq!(lab.items().len(), 5, "{:?}", lab.items());

    // Due routines and the first-seen one now record this run; the rest keep their last run.
    let state = lab.state();
    let last = |name: &str| state["routines"][name]["last_run"].as_str().unwrap().parse::<jiff::Timestamp>().unwrap();
    for name in due.iter().chain(&["first-seen"]) { assert!(last(name) >= started, "{name}: {}", last(name)); }
    for name in ["hourly-fresh", "daily-ran", "daily-later", "off"] {
        assert_eq!(state["routines"][name]["last_run"], seeded["routines"][name]["last_run"], "{name}");
    }
    assert_eq!(state["config_errors"].as_array().map(Vec::len), Some(2));
}

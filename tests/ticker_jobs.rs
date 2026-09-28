#![cfg(target_os = "linux")]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! Background coordinator, token and remote-machine jobs through the compiled
//! CLI: `ticker run` and `open` against one fake herdr that serves several
//! sessions in a root, told apart by the socket each call names. Every call
//! is logged with its time, so cadence and backoff are read from the log.
use serde_json::{json, Value};
use std::{fs, os::unix::{fs::PermissionsExt, net::UnixListener}, path::{Path, PathBuf}, process::{Command, Output, Stdio}, time::{Duration, Instant}};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-projects");

/// Session `NAME` is the socket `$HOME/NAME.sock`; it lists `NAME.agents` and
/// `NAME.panes` (`NAME@MACHINE.*` through `--machine`; saved machines are
/// `machines.json`), and fails every call while `NAME.down` exists. Bridge
/// requests follow `NAME.reply`: `ack`, `null` (a start acknowledged without
/// an agent kind), `lost` (no reply) or `unsupported` (no bridge at all).
/// A notification is always shown.
/// Every call is logged to `calls` as `TIME NAME ARGS [METHOD]`, and every
/// token refresh's parameters to `tokens`.
const FAKE_HERDR: &str = r#"#!/usr/bin/python3
import json,os,pathlib,sys,time
home=pathlib.Path(os.environ['HOME']);args=sys.argv[1:]
name=pathlib.Path(os.environ.get('HERDR_SOCKET_PATH','none')).stem
machine=None
if args[:1]==['--machine']:machine=args[1];args=args[2:]
if machine:name=name+'@'+machine
def read(kind):
    p=home/f'{name}.{kind}'
    return json.loads(p.read_text()) if p.exists() else []
mode=(home/f'{name}.reply').read_text() if (home/f'{name}.reply').exists() else 'ack'
line=f'{time.time():.3f} {name} '+' '.join(args)
request=None
if args==['remote-api-bridge']:
    request=json.loads(sys.stdin.readline());line+=' '+request['method']
with open(home/'calls','a') as f:f.write(line+'\n')
if (home/f'{name}.down').exists():print('connection refused',file=sys.stderr);sys.exit(1)
if args==['machine','list','--json']:print((home/'machines.json').read_text())
elif args==['--version']:print('herdr 0.9.1')
elif args==['remote-api-bridge','--check']:print('unsupported' if mode=='unsupported' else 'herdr-api-bridge-v1')
elif args==['agent','list']:print(json.dumps({'result':{'agents':read('agents')}}))
elif args==['pane','list']:print(json.dumps({'result':{'panes':read('panes')}}))
elif request is not None and request['method'] in ('agent.list','pane.list'):
    kind=request['method'].split('.')[0]+'s';print(json.dumps({'id':request['id'],'result':{kind:read(kind)}}))
elif request is not None and request['method']=='notification.show':print(json.dumps({'id':request['id'],'result':{'type':'notification_show','shown':True,'reason':'shown'}}))
elif request is not None and request['method']=='pane.report_metadata':
    with open(home/'tokens','a') as f:f.write(json.dumps(dict(request['params'],session=name))+'\n')
    print(json.dumps({'id':request['id'],'result':{'type':'ok'}}))
elif request is not None:
    if mode=='lost':sys.exit(1)
    params=request['params']
    if request['method']=='agent.prompt':agent=next(a for a in read('agents') if a['pane_id']==params['target'])
    else:agent=dict(next(p for p in read('panes') if p['pane_id']==params['pane_id']),name=params['name'],launch_pending=True,agent_status='unknown',agent=None if mode=='null' else 'claude')
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
        // Remote bridges run through `ssh TARGET COMMAND`; run the command here.
        let bin = home.path().join("bin");
        fs::create_dir(&bin).unwrap();
        fs::write(bin.join("ssh"), "#!/bin/sh\n[ \"$(eval echo \\${$(($#-1))})\" = fixture.invalid ] || exit 9\neval \"exec /bin/sh -c \\\"\\${$#}\\\"\"\n").unwrap();
        for path in [fake.clone(), bin.join("ssh")] { fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap(); }
        Lab { home, fake, listeners: Vec::new() }
    }
    fn path(&self, name: &str) -> PathBuf { self.home.path().join(name) }
    fn root(&self) -> PathBuf { self.path("root") }
    fn project(&self, slug: &str) -> PathBuf { self.root().join(slug) }
    fn command(&self) -> Command {
        let mut command = Command::new(BIN);
        command.env_clear().env("HOME", self.home.path()).env("PATH", format!("{}:/usr/bin:/bin", self.path("bin").display()))
            .env("HERDR_BIN_PATH", &self.fake).arg("--root").arg(self.root());
        command
    }
    fn cli(&self, args: &[&str]) -> Output { self.command().args(args).output().unwrap() }
    fn ok(&self, args: &[&str]) -> String {
        let out = self.cli(args);
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }
    /// A new project whose coordinator lives in session `slug`, pane `p`.
    fn project_in_session(&mut self, slug: &str, extra: Value) -> PathBuf {
        self.ok(&["new", slug]);
        self.listeners.push(UnixListener::bind(self.socket(slug)).unwrap());
        let mut record = json!({"socket": self.socket(slug), "workspace_id": "w", "tab_id": "w:t", "pane_id": "p", "agent_name": "coordinator",
            "cwd": self.project(slug), "prime_pending": true, "prime_request": 1});
        for (k, v) in extra.as_object().unwrap() { record[k] = v.clone(); }
        fs::write(self.project(slug).join(".state/coordinator.json"), record.to_string()).unwrap();
        self.project(slug)
    }
    fn socket(&self, slug: &str) -> PathBuf { self.path(&format!("{slug}.sock")) }
    /// What session `name` lists: panes, and agents with the given statuses.
    fn session(&self, name: &str, agents: &[Value], panes: &[Value]) {
        fs::write(self.path(&format!("{name}.agents")), Value::from(agents.to_vec()).to_string()).unwrap();
        fs::write(self.path(&format!("{name}.panes")), Value::from(panes.to_vec()).to_string()).unwrap();
    }
    fn set(&self, file: &str, contents: Option<&str>) {
        match contents { Some(text) => fs::write(self.path(file), text).unwrap(), None => { let _ = fs::remove_file(self.path(file)); } }
    }
    /// The times of every logged call of session `name` ending with `suffix`.
    fn times(&self, name: &str, suffix: &str) -> Vec<f64> {
        fs::read_to_string(self.path("calls")).unwrap_or_default().lines().filter_map(|line| {
            let (time, rest) = line.split_once(' ')?;
            let (session, call) = rest.split_once(' ')?;
            (session == name && call.ends_with(suffix)).then(|| time.parse().unwrap())
        }).collect()
    }
    fn coordinator(&self, slug: &str) -> Value {
        serde_json::from_slice(&fs::read(self.project(slug).join(".state/coordinator.json")).unwrap()).unwrap()
    }
    /// Inbox items of `kind` (named `TIME-KIND-SUBJECT-...`), in time order.
    fn inbox(&self, slug: &str, kind: &str) -> Vec<String> {
        let mut entries: Vec<_> = fs::read_dir(self.project(slug).join("inbox")).unwrap().flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(&format!("-{kind}-"))).map(|e| e.path()).collect();
        entries.sort();
        entries.iter().map(|path| fs::read_to_string(path).unwrap()).collect()
    }
    fn run_ticker(&self, env: &[(&str, &str)]) -> Ticker<'_> {
        let child = self.command().envs(env.iter().copied()).args(["ticker", "run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
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
            assert!(Instant::now() < deadline, "timed out waiting for {what}; ticker log:\n{}\ncalls:\n{}",
                fs::read_to_string(self.lab.root().join(".ticker.log")).unwrap_or_default(), fs::read_to_string(self.lab.path("calls")).unwrap_or_default());
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    /// Wait for the next completed pass (each republishes the metrics file).
    fn next_pass(&mut self) {
        use std::os::unix::fs::MetadataExt;
        let metrics = self.lab.root().join(".ticker-metrics.json");
        let inode = || fs::metadata(&metrics).map(|m| m.ino()).ok();
        let before = inode();
        self.wait_for("the next pass", 30, || inode() != before);
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

/// Pane `id` in workspace `w`, tab `w:t`.
fn pane_at(id: &str, cwd: &Path) -> Value { json!({"workspace_id": "w", "tab_id": "w:t", "pane_id": id, "terminal_id": format!("term-{id}"), "cwd": cwd}) }
fn agent_at(id: &str, cwd: &Path, name: &str, status: &str) -> Value {
    let mut agent = pane_at(id, cwd);
    agent["name"] = json!(name);
    agent["agent"] = json!("claude");
    agent["agent_status"] = json!(status);
    agent
}
fn gaps(times: &[f64]) -> Vec<f64> { times.windows(2).map(|w| w[1] - w[0]).collect() }

/// Replaces `open_retains_launch_history_and_queues_a_new_request_without_starting`
/// and `plain_open_cannot_turn_a_missing_agent_observation_into_another_launch_request`.
///
/// The ticker starts a coordinator once: the reply is lost (the claim ends
/// uncertain) or acknowledged without an agent kind (confirmed), and no agent
/// appears. A plain `open` then refuses and writes nothing; `open --reprime`
/// only queues a new prime request, keeping the launch history. Neither
/// starts or prompts anything.
#[test]
fn open_after_a_coordinator_start_keeps_its_claim_and_never_starts_again() {
    for reply in ["lost", "null"] {
        let mut lab = Lab::new();
        let project = lab.project_in_session("demo", json!({}));
        lab.session("demo", &[], &[pane_at("p", &project)]);
        lab.set("demo.reply", Some(reply));
        let phase = || lab.coordinator("demo")["launch_claim"]["phase"].as_str().unwrap_or_default().to_owned();
        let mut ticker = lab.run_ticker(&[]);
        ticker.wait_for("the start claim", 30, || phase() == if reply == "lost" { "pending" } else { "confirmed" });
        ticker.stop();
        if reply == "lost" {
            let mut ticker = lab.run_ticker(&[]);
            ticker.wait_for("the uncertain start notice", 30, || lab.coordinator("demo")["launch_claim"]["notified"] == true);
            ticker.stop();
            assert_eq!(phase(), "uncertain");
        }
        let starts = || lab.times("demo", "agent.start").len();
        assert_eq!(starts(), 1, "{reply}");

        let record = lab.project("demo").join(".state/coordinator.json");
        let before = fs::read(&record).unwrap();
        let socket = lab.socket("demo");
        let plain = lab.cli(&["open", "demo", "--socket", socket.to_str().unwrap()]);
        assert!(!plain.status.success());
        assert!(String::from_utf8_lossy(&plain.stderr).contains("open --reprime"), "{}", String::from_utf8_lossy(&plain.stderr));
        assert_eq!(fs::read(&record).unwrap(), before, "a refused open wrote the record");

        // A ticker of this version already runs, so `open` leaves it be.
        let version = lab.ok(&["--version"]).trim().rsplit(' ').next().unwrap().to_string();
        let _ticker = hold_lock(&lab.root(), &version);
        lab.ok(&["open", "demo", "--socket", socket.to_str().unwrap(), "--reprime"]);
        let (before, after): (Value, Value) = (serde_json::from_slice(&before).unwrap(), lab.coordinator("demo"));
        assert_eq!((&after["prime_request"], &after["prime_pending"], &after["launch_attempts"]), (&json!(2), &json!(true), &json!(0)), "{reply}");
        assert_eq!((&after["launch_claim"], &after["launch_sequence"]), (&before["launch_claim"], &before["launch_sequence"]), "{reply}");
        assert_eq!(starts(), 1, "{reply}");
        assert!(lab.times("demo", "agent.prompt").is_empty() && lab.times("demo", "agent prompt").is_empty() && lab.times("demo", "agent start").is_empty());
    }
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

/// Two projects share one ticker, which admits one job per pass and rotates
/// between projects; a job admitted in one pass is settled in the next. So a
/// 30 s cooldown puts a job's next run three passes (45 s) later, while
/// without one it would run again two passes (30 s) later.
fn cooldown_lab(slugs: [&str; 2]) -> Lab {
    let mut lab = Lab::new();
    for slug in slugs {
        let project = lab.project_in_session(slug, json!({"prime_pending": slug != "tokens"}));
        let agents = if slug == "tokens" { vec![agent_at("p", &project, "coordinator", "idle")] } else { vec![] };
        lab.session(slug, &agents, &[pane_at("p", &project)]);
    }
    lab
}
/// Asserts every run of `own` is at least three passes after the previous,
/// and that `other` work ran in between.
fn assert_cooled_down(what: &str, own: &[f64], other: &[f64]) {
    assert!(own.len() >= 2 && gaps(own).iter().all(|gap| *gap >= 40.0), "{what} ran again before its cooldown: {own:?}");
    assert!(other.iter().any(|t| own[0] < *t && *t < own[1]), "nothing else ran while {what} cooled down: {own:?} {other:?}");
}

/// Replaces `coordinator_start_confirms_submission_once_without_certifying_readiness`
/// and `token_refreshes_cool_down_without_blocking_other_work`.
///
/// `tokens` refreshes its coordinator's tokens, then cools down for 30 s.
/// Meanwhile `fresh` starts its coordinator once on an acknowledgement without
/// an agent kind, and primes it only once the agent appears ready.
#[test]
fn token_refreshes_cool_down_while_another_coordinator_starts_and_primes() {
    let lab = cooldown_lab(["tokens", "fresh"]);
    lab.set("fresh.reply", Some("null"));
    let mut ticker = lab.run_ticker(&[]);
    ticker.wait_for("the acknowledged start", 30, || lab.coordinator("fresh")["launch_claim"]["phase"] == "confirmed");
    assert!(lab.times("fresh", "agent.prompt").is_empty(), "primed before the agent appeared");
    lab.session("fresh", &[agent_at("p", &lab.project("fresh"), "coordinator", "idle")], &[pane_at("p", &lab.project("fresh"))]);
    ticker.wait_for("the prime and a third refresh", 120, || lab.coordinator("fresh")["prime_pending"] == false && lab.times("tokens", "pane.report_metadata").len() >= 3);
    ticker.stop();
    let fresh = [lab.times("fresh", "agent.start"), lab.times("fresh", "agent.prompt")];
    assert_eq!((fresh[0].len(), fresh[1].len(), &lab.coordinator("fresh")["launch_sequence"]), (1, 1, &json!(1)));
    assert_cooled_down("a token refresh", &lab.times("tokens", "pane.report_metadata"), &fresh.concat());
}

/// Replaces `errors_back_off_without_resetting_history_and_failed_submission_does_not_advance`.
///
/// `refused` has no bridge, so each coordinator start fails before claiming
/// anything and is retried only after its 30 s backoff, while `tokens` keeps
/// refreshing.
#[test]
fn a_failed_coordinator_start_backs_off_while_another_project_works() {
    let lab = cooldown_lab(["refused", "tokens"]);
    lab.set("refused.reply", Some("unsupported"));
    let checks = || lab.times("refused", "remote-api-bridge --check");
    let mut ticker = lab.run_ticker(&[]);
    ticker.wait_for("a third start attempt", 120, || checks().len() >= 3);
    ticker.stop();
    assert_cooled_down("a failed start", &checks(), &lab.times("tokens", "pane.report_metadata"));
    let refused = lab.coordinator("refused");
    assert!(refused["launch_claim"].is_null() && refused["launch_attempts"].as_u64().unwrap_or(0) == 0, "{refused}");
    assert!(lab.times("refused", "agent.start").is_empty());
}

/// Replaces `machine_cadence_and_backoff_are_scoped_to_project_and_session`,
/// `remote_deadlines_ignore_tick_counts_and_backoff_starts_after_failure`,
/// `short_outages_write_nothing_and_long_ones_write_one_item_each_way` and
/// `f02_shared_machine_serves_both_project_sessions`.
///
/// Projects `a` and `b` each watch a remote thread on a machine both call
/// `box`. While `a`'s machine is down, `b`'s is still polled on its own
/// one-minute deadline, never on every 15 s pass, and records what it saw;
/// `a`'s waits for its two-minute retry. With a zero outage threshold `a` gets one outage item,
/// and one more when it is back after a restart; `b`'s short outage, below the
/// threshold, writes nothing either way.
#[test]
fn remote_machines_poll_on_their_own_deadlines_and_only_long_outages_are_reported() {
    let mut lab = Lab::new();
    fs::write(lab.path("machines.json"), json!([{"label": "box", "target": "fixture.invalid", "enabled": true}]).to_string()).unwrap();
    for slug in ["a", "b"] {
        let project = lab.project_in_session(slug, json!({"prime_pending": false}));
        let dir = lab.path(&format!("{slug}-remote"));
        fs::create_dir(&dir).unwrap();
        fs::write(project.join("threads/t-0001.toml"), toml::to_string(&json!({"id": "t-0001", "title": "remote", "status": "open", "kind": "adopted",
            "created": jiff::Timestamp::now().to_string(), "agent": "claude", "agent_name": "worker", "workspace_id": "w", "tab_id": "w:t", "pane_id": "r1",
            "cwd": dir, "thread_dir": dir, "machine": "box"})).unwrap()).unwrap();
        lab.session(slug, &[agent_at("p", &project, "coordinator", "idle")], &[pane_at("p", &project)]);
        lab.session(&format!("{slug}@box"), &[agent_at("r1", &dir, "worker", "working")], &[pane_at("r1", &dir)]);
    }
    let polls = |slug: &str| lab.times(&format!("{slug}@box"), "agent list");
    let outages = |slug: &str| lab.inbox(slug, "outage");

    lab.set("a@box.down", Some(""));
    let mut ticker = lab.run_ticker(&[("HERDR_PROJECTS_OUTAGE_SECS", "0")]);
    ticker.wait_for("b's second poll", 120, || polls("b").len() >= 2);
    ticker.next_pass();
    ticker.stop();
    let b = polls("b");
    assert!(gaps(&b).iter().all(|gap| *gap >= 59.0), "b was polled before its deadline: {b:?}");
    assert_eq!(polls("a").len(), 1, "a was retried before its backoff");
    assert_eq!(outages("a").len(), 1, "{:?}", outages("a"));
    assert!(outages("a")[0].contains("`box` has been unreachable"), "{:?}", outages("a"));
    assert!(outages("b").is_empty());
    let state = |slug: &str| fs::read_to_string(lab.project(slug).join("threads/t-0001.toml")).unwrap();
    assert!(state("b").contains("last_state = \"working\"") && !state("a").contains("last_state = \"working\""), "{}\n{}", state("a"), state("b"));

    // A restart polls at once. `a` is still down and reported, `b` goes down
    // below a one-hour threshold; then both come back.
    lab.set("b@box.down", Some(""));
    for down in [true, false] {
        if !down { lab.set("a@box.down", None); lab.set("b@box.down", None); }
        let (a, b) = (polls("a").len(), polls("b").len());
        let mut ticker = lab.run_ticker(&[("HERDR_PROJECTS_OUTAGE_SECS", "3600")]);
        ticker.wait_for("both machines polled", 60, || polls("a").len() > a && polls("b").len() > b && (down || outages("a").len() == 2));
        ticker.next_pass();
        ticker.stop();
    }
    let a = outages("a");
    assert_eq!(a.len(), 2, "{a:?}");
    assert!(a.iter().any(|item| item.contains("reachable again")), "{a:?}");
    assert!(outages("b").is_empty(), "{:?}", outages("b"));
    assert!(["a", "b"].iter().all(|slug| state(slug).contains("last_state = \"working\"")), "{}\n{}", state("a"), state("b"));
}

/// Replaces `native_ok_refresh_is_repeatable_and_does_not_write_execution_state`
/// and `refresh_uses_current_group_and_allows_busy_or_empty_owned_thread_panes`.
///
/// The ticker refreshes each thread pane's tokens from the group the thread
/// is in when the refresh is sent: an idle agent, or a pane whose agent has
/// gone, with an unacknowledged report is `ready-for-review` (a working agent's
/// `working` is asserted in tests/cli.rs). A coordinator is refreshed only while its agent is
/// listed. Refreshes change no thread or coordinator execution state.
#[test]
fn token_refreshes_follow_each_panes_current_group_and_write_no_execution_state() {
    let mut lab = Lab::new();
    let project = lab.project_in_session("tok", json!({"prime_pending": false}));
    use sha2::Digest;
    let lone = lab.project_in_session("lone", json!({"prime_pending": false}));
    lab.session("lone", &[], &[pane_at("p", &lone)]);
    let mut agents = vec![agent_at("p", &project, "coordinator", "idle")];
    let mut panes = vec![pane_at("p", &project)];
    for (n, status) in [(1, Some("idle")), (2, None)] {
        let (id, pane) = (format!("t-000{n}"), format!("p{n}"));
        let dir = lab.path(&format!("work-{n}"));
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("report.md"), format!("report {n}")).unwrap();
        fs::write(project.join(format!("threads/{id}.toml")), toml::to_string(&json!({"id": id, "title": id, "status": "open", "kind": "adopted",
            "created": jiff::Timestamp::now().to_string(), "agent": "claude", "agent_name": "worker", "workspace_id": "w", "tab_id": "w:t", "pane_id": pane,
            "cwd": dir, "thread_dir": dir, "report_hash": format!("{:x}", sha2::Sha256::digest(format!("report {n}")))})).unwrap()).unwrap();
        panes.push(pane_at(&pane, &dir));
        if let Some(status) = status { agents.push(agent_at(&pane, &dir, "worker", status)); }
    }
    lab.session("tok", &agents, &panes);
    let execution = || -> Vec<Value> {
        let mut state = vec![lab.coordinator("tok"), lab.coordinator("lone")];
        for n in 1..=2 {
            let record: Value = toml::from_str(&fs::read_to_string(project.join(format!("threads/t-000{n}.toml"))).unwrap()).unwrap();
            let field = |key: &str, default: Value| record.get(key).cloned().unwrap_or(default);
            state.push(json!([record["status"], record["pane_id"], field("prompt_pending", json!(false)), field("prompt_sequence", json!(0)),
                field("launch_sequence", json!(0)), field("lifecycle_generation", json!(0)), field("report_hash", json!(""))]));
        }
        state
    };
    let before = execution();
    let tokens = || -> Vec<Value> { fs::read_to_string(lab.path("tokens")).unwrap_or_default().lines().map(|l| serde_json::from_str(l).unwrap()).collect() };
    let review = |thread: &str| tokens().iter().rev().find(|t| t["tokens"]["thread"] == thread).map(|t| t["tokens"]["review"].clone());
    let mut ticker = lab.run_ticker(&[]);
    ticker.wait_for("a refresh of every pane", 120, || {
        review("t-0001") == Some(json!("ready-for-review")) && review("t-0002") == Some(json!("ready-for-review"))
            && tokens().iter().any(|t| t["tokens"]["thread"] == "coordinator")
    });
    ticker.next_pass();
    ticker.stop();
    for token in tokens() {
        assert_eq!((token["session"].as_str(), token["tokens"]["project"].as_str(), token["ttl_ms"].as_u64()), (Some("tok"), Some("tok"), Some(300_000)), "{token}");
    }
    let coordinator = tokens().into_iter().find(|t| t["tokens"]["thread"] == "coordinator").unwrap();
    assert_eq!((coordinator["pane_id"].as_str(), coordinator["tokens"]["rank"].as_str()), (Some("p"), Some("0")));
    assert_eq!(execution(), before);
}

/// Replaces `concrete_prime_confirms_once_and_requires_an_explicit_new_request`.
///
/// The ticker primes a ready coordinator once. Marking the record pending
/// again, without a new request, primes nothing; `open --reprime` asks for a
/// new prime, which the ticker delivers once more.
#[test]
fn a_primed_coordinator_is_primed_again_only_after_an_explicit_reprime() {
    let mut lab = Lab::new();
    let project = lab.project_in_session("demo", json!({}));
    lab.session("demo", &[agent_at("p", &project, "coordinator", "idle")], &[pane_at("p", &project)]);
    let prompts = || lab.times("demo", "agent.prompt").len();
    let settle = |what: &str, done: &dyn Fn() -> bool| {
        let mut ticker = lab.run_ticker(&[]);
        ticker.wait_for(what, 60, done);
        ticker.next_pass();
        ticker.next_pass();
        ticker.stop();
    };
    settle("the prime", &|| lab.coordinator("demo")["prime_pending"] == false);
    let primed = lab.coordinator("demo");
    assert_eq!((prompts(), &primed["prime_claim"]["delivery"]["phase"], &primed["prime_sequence"]), (1, &json!("confirmed"), &json!(1)), "{primed}");

    let mut pending = primed.clone();
    pending["prime_pending"] = json!(true);
    fs::write(project.join(".state/coordinator.json"), pending.to_string()).unwrap();
    settle("two passes", &|| true);
    assert_eq!(prompts(), 1, "primed again without a new request");

    {
        let version = lab.ok(&["--version"]).trim().rsplit(' ').next().unwrap().to_string();
        let _ticker = hold_lock(&lab.root(), &version);
        lab.ok(&["open", "demo", "--socket", lab.socket("demo").to_str().unwrap(), "--reprime"]);
    }
    assert_eq!(lab.coordinator("demo")["prime_request"], json!(2));
    settle("the second prime", &|| prompts() == 2 && lab.coordinator("demo")["prime_pending"] == false);
    let reprimed = lab.coordinator("demo");
    assert_eq!((prompts(), &reprimed["prime_claim"]["delivery"]["phase"], &reprimed["prime_sequence"]), (2, &json!("confirmed"), &json!(2)), "{reprimed}");
}

/// Replaces `concrete_launch_acknowledges_submission_without_waiting_for_interactive_readiness`.
///
/// A thread waiting in an empty pane is started once. The acknowledgement
/// confirms the start, with or without an agent kind, although no ready
/// agent appears: later passes neither start it again nor brief it.
#[test]
fn a_thread_start_is_confirmed_on_acknowledgement_without_waiting_for_the_agent() {
    for reply in ["ack", "null"] {
        let mut lab = Lab::new();
        let project = lab.project_in_session("demo", json!({"prime_pending": false}));
        let work = lab.path("work");
        fs::create_dir(&work).unwrap();
        fs::write(project.join("threads/t-0001.toml"), toml::to_string(&json!({"id": "t-0001", "title": "task", "status": "open", "kind": "tab",
            "created": jiff::Timestamp::now().to_string(), "agent": "claude", "agent_name": "worker", "workspace_id": "w", "tab_id": "w:t", "pane_id": "pw",
            "cwd": work, "thread_dir": work, "prompt_pending": true})).unwrap()).unwrap();
        lab.session("demo", &[agent_at("p", &project, "coordinator", "idle")], &[pane_at("p", &project), pane_at("pw", &work)]);
        lab.set("demo.reply", Some(reply));
        let thread = || -> toml::Value { toml::from_str(&fs::read_to_string(project.join("threads/t-0001.toml")).unwrap()).unwrap() };
        let mut ticker = lab.run_ticker(&[]);
        ticker.wait_for("the confirmed start", 60, || thread().get("launch_claim").and_then(|c| c.get("phase")).and_then(|p| p.as_str()) == Some("confirmed"));
        ticker.next_pass();
        ticker.next_pass();
        ticker.stop();
        let record = thread();
        assert_eq!(lab.times("demo", "agent.start").len(), 1, "{reply}");
        assert_eq!((record["prompt_pending"].as_bool(), record["status"].as_str()), (Some(true), Some("open")), "{reply}: {record}");
        assert!(lab.times("demo", "agent.prompt").is_empty(), "{reply}");
    }
}

/// Replaces `ticker_offers_without_advancing_until_worker_claim_and_then_delivers`.
///
/// A due, approved legacy routine runs in the background: while its command
/// runs the ticker has advanced the routine to this occurrence and recorded
/// its claim but delivered nothing, and it keeps passing. Once the command
/// ends its output is delivered as one inbox item, and nothing runs again.
#[test]
fn a_due_legacy_routine_is_claimed_once_and_delivered_after_its_command_ends() {
    use sha2::{Digest, Sha256};
    let mut lab = Lab::new();
    let project = lab.project_in_session("demo", json!({"prime_pending": false}));
    lab.session("demo", &[agent_at("p", &project, "coordinator", "idle")], &[pane_at("p", &project)]);
    let command = "printf once >> count; while [ ! -e release ]; do sleep 0.05; done; printf routine-result";
    fs::write(project.join("routines/check.md"), format!("+++\nschedule = \"every 24h\"\ncommand = {}\n+++\nInspect output.\n", json!(command))).unwrap();
    let config = lab.path(".config/herdr-projects");
    fs::create_dir_all(&config).unwrap();
    fs::write(config.join("config.toml"), format!("[safety.{:?}]\nroutine_commands = true\n", project.display().to_string())).unwrap();
    fs::write(config.join("approved-routines.json"), json!([{"project": project, "routine": "check",
        "command_sha256": format!("{:x}", Sha256::digest(command.as_bytes())), "approved": "fixture"}]).to_string()).unwrap();
    let previous = "2026-01-01T00:00:00Z";
    fs::write(project.join(".state/ticker.json"), json!({"routines": {"check": {"last_run": previous}}}).to_string()).unwrap();
    let routine = || -> Value { serde_json::from_slice::<Value>(&fs::read(project.join(".state/ticker.json")).unwrap()).unwrap()["routines"]["check"].clone() };
    let delivered = || -> Vec<String> { fs::read_dir(project.join("inbox")).unwrap().flatten().filter(|e| e.path().is_file())
        .map(|e| fs::read_to_string(e.path()).unwrap()).filter(|text| text.contains("routine")).collect() };

    let mut ticker = lab.run_ticker(&[]);
    ticker.wait_for("the routine command", 60, || project.join("count").exists());
    ticker.next_pass();
    let running = routine();
    assert!(running["last_run"].as_str().is_some_and(|run| run != previous), "{running}");
    assert!(running["dispatch"].is_object() && running["dispatch"]["result"].is_null(), "{running}");
    assert!(delivered().is_empty());
    fs::write(project.join("release"), b"").unwrap();
    ticker.wait_for("the routine result", 60, || delivered().len() == 1);
    ticker.wait_for("the delivery acknowledged", 60, || routine()["dispatch"].is_null());
    ticker.next_pass();
    ticker.stop();
    let mut ticker = lab.run_ticker(&[]);
    ticker.next_pass();
    ticker.next_pass();
    ticker.stop();
    assert_eq!(fs::read(project.join("count")).unwrap(), b"once");
    let items = delivered();
    assert_eq!(items.len(), 1);
    assert!(items[0].contains("routine-result"), "{}", items[0]);
    assert_eq!(routine()["last_run"], running["last_run"]);
}

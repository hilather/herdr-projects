#![cfg(target_os = "linux")]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! Recovery of merged-PR finalization and inbox notifications through the
//! compiled CLI: `ticker run` passes, `thread resolve` and `doctor`. herdr, gh
//! and ssh are fakes; the final copy is the binary's own `artifact-stream`.
//! Waits are on observed records, calls and polls, never on elapsed time.
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{fs, os::unix::{fs::PermissionsExt, net::UnixListener}, path::{Path, PathBuf}, process::{Command, Output, Stdio}, time::{Duration, Instant}};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-projects");

/// Session `NAME` is the socket `$HOME/NAME.sock` (`NAME@MACHINE` through
/// `--machine`); it lists `NAME.agents` and `NAME.panes`. A toast is shown
/// unless `NAME.toast` says `disabled`; each one is logged to `NAME.toasts`
/// with its time. Every call is logged to `calls` as `NAME ARGS [METHOD]`.
const FAKE_HERDR: &str = r#"#!/usr/bin/python3
import json,os,pathlib,sys,time
home=pathlib.Path(os.environ['HOME']);args=sys.argv[1:]
name=pathlib.Path(os.environ.get('HERDR_SOCKET_PATH','none')).stem
if args[:1]==['--machine']:name=name+'@'+args[1];args=args[2:]
def read(kind):
    p=home/f'{name}.{kind}'
    return json.loads(p.read_text()) if p.exists() else []
line=name+' '+' '.join(args);request=None
if args==['remote-api-bridge']:request=json.loads(sys.stdin.readline());line+=' '+request['method']
with open(home/'calls','a') as f:f.write(line+'\n')
if args==['machine','list','--json']:print((home/'machines.json').read_text())
elif args==['--version']:print('herdr 0.9.1')
elif args==['remote-api-bridge','--check']:print('herdr-api-bridge-v1')
elif args in (['agent','list'],['pane','list']):print(json.dumps({'result':{args[0]+'s':read(args[0]+'s')}}))
elif request is not None and request['method'] in ('agent.list','pane.list'):
    kind=request['method'].split('.')[0]+'s';print(json.dumps({'id':request['id'],'result':{kind:read(kind)}}))
elif request is not None and request['method']=='notification.show':
    with open(home/f'{name}.toasts','a') as f:f.write(f'{time.time()}\n')
    toast=home/f'{name}.toast';shown=not toast.exists() or toast.read_text()!='disabled'
    print(json.dumps({'id':request['id'],'result':{'type':'notification_show','shown':shown,'reason':'shown' if shown else 'disabled'}}))
elif request is not None and request['method']=='pane.report_metadata':print(json.dumps({'id':request['id'],'result':{'type':'ok'}}))
elif request is not None:print(json.dumps({'id':request['id'],'error':{'code':'unsupported','message':'not in fixture'}}))
else:print('{"result":{}}')
"#;

const BRANCH: &str = "hp/demo/t-0001-task";

struct Lab { home: tempfile::TempDir, fake: PathBuf, listeners: Vec<UnixListener> }

impl Lab {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let lab = Lab { fake: home.path().join("herdr"), home, listeners: Vec::new() };
        lab.script("herdr", FAKE_HERDR);
        fs::create_dir(lab.path("bin")).unwrap();
        fs::create_dir(lab.path("gh")).unwrap();
        // gh answers `pr view` for pull request N from `gh/N.json` and logs the URL.
        lab.script("bin/gh", &format!("#!/usr/bin/python3\nimport sys\nurl=sys.argv[-1]\nopen({:?},'a').write(url+'\\n')\nprint(open({:?}+'/'+url.rsplit('/',1)[1]+'.json').read())\n",
            lab.path("gh-calls").to_str().unwrap(), lab.path("gh").to_str().unwrap()));
        // ssh to `fixture.invalid` runs its script here.
        lab.script("bin/ssh", "#!/bin/sh\n[ \"$(eval echo \\${$(($#-1))})\" = fixture.invalid ] || exit 9\neval \"exec /bin/sh -c \\\"\\${$#}\\\"\"\n");
        lab
    }
    fn path(&self, name: &str) -> PathBuf { self.home.path().join(name) }
    fn script(&self, name: &str, body: &str) {
        fs::write(self.path(name), body).unwrap();
        fs::set_permissions(self.path(name), fs::Permissions::from_mode(0o700)).unwrap();
    }
    fn root(&self) -> PathBuf { self.path("root") }
    fn project(&self, slug: &str) -> PathBuf { self.root().join(slug) }
    fn command(&self) -> Command {
        let mut command = Command::new(BIN);
        command.env_clear().env("HOME", self.home.path()).env("PATH", format!("{}:/usr/bin:/bin", self.path("bin").display()))
            .env("HERDR_BIN_PATH", &self.fake).env("HERDR_PROJECTS_REMOTE_BIN", self.path("helper")).arg("--root").arg(self.root());
        command
    }
    fn cli(&self, args: &[&str]) -> Output { self.command().args(args).output().unwrap() }
    fn ok(&self, args: &[&str]) -> String {
        let out = self.cli(args);
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }
    /// A new project whose idle coordinator lives in session `slug`, pane `p`.
    fn project_in_session(&mut self, slug: &str) -> PathBuf {
        self.ok(&["new", slug]);
        let project = self.project(slug);
        let socket = self.path(&format!("{slug}.sock"));
        self.listeners.push(UnixListener::bind(&socket).unwrap());
        fs::write(project.join(".state/coordinator.json"), json!({"socket": socket, "workspace_id": "w", "tab_id": "w:t", "pane_id": "p",
            "agent_name": "coordinator", "cwd": project}).to_string()).unwrap();
        self.session(slug, &[agent("p", &project, "coordinator")], &[pane("p", &project)]);
        project
    }
    fn session(&self, name: &str, agents: &[Value], panes: &[Value]) {
        fs::write(self.path(&format!("{name}.agents")), Value::from(agents.to_vec()).to_string()).unwrap();
        fs::write(self.path(&format!("{name}.panes")), Value::from(panes.to_vec()).to_string()).unwrap();
    }
    /// Pull request `number` as `gh pr view` reports it for the thread's branch.
    fn pull_request(&self, number: u32, state: &str) {
        fs::write(self.path(&format!("gh/{number}.json")), json!({"state": state, "reviewDecision": "", "headRefName": BRANCH,
            "headRepository": {"name": "app"}, "headRepositoryOwner": {"login": "owner"}}).to_string()).unwrap();
    }
    /// An idle worker thread in session `slug`, pane `pw`, whose home and
    /// source reports both name pull request `number`, already copied home.
    fn merged_thread(&self, slug: &str, number: u32, extra: Value) -> PathBuf {
        let project = self.project(slug);
        let source = self.path(&format!("{slug}-source"));
        fs::create_dir_all(source.join("library")).unwrap();
        fs::write(source.join("library/item"), b"binary\0\xff").unwrap();
        let report = format!("PR: {}\n## Report\ndone\n", url(number));
        fs::write(source.join("report.md"), &report).unwrap();
        fs::write(project.join("threads/t-0001.md"), &report).unwrap();
        let hash = format!("{:x}", Sha256::digest(report.as_bytes()));
        let mut record = json!({"id": "t-0001", "title": "Task", "status": "open", "kind": "adopted", "created": jiff::Timestamp::now().to_string(),
            "agent": "claude", "agent_name": "worker-pw", "workspace_id": "w", "tab_id": "w:t", "pane_id": "pw", "cwd": source, "thread_dir": source,
            "branch": BRANCH, "origin": "git@github.com:Owner/App.git", "report_hash": hash, "acked_report_hash": hash,
            "last_review_item_hash": hash, "last_state": "idle", "last_group": "idle"});
        for (k, v) in extra.as_object().unwrap() { record[k] = v.clone(); }
        fs::write(project.join("threads/t-0001.toml"), toml::to_string(&record).unwrap()).unwrap();
        self.pull_request(number, "MERGED");
        source
    }
    fn thread(&self, slug: &str) -> toml::Value {
        toml::from_str(&fs::read_to_string(self.project(slug).join("threads/t-0001.toml")).unwrap()).unwrap()
    }
    fn text(&self, slug: &str, key: &str) -> String { self.thread(slug).get(key).and_then(|v| v.as_str()).unwrap_or_default().to_string() }
    /// The ticker's retry record of a project.
    fn state(&self, slug: &str) -> Value {
        fs::read(self.project(slug).join(".state/ticker.json")).ok().map_or(json!({}), |b| serde_json::from_slice(&b).unwrap())
    }
    fn states(&self) -> String {
        fs::read_dir(self.root()).unwrap().flatten().filter(|e| e.path().is_dir()).map(|e| {
            let slug = e.file_name().to_string_lossy().into_owned();
            format!("{slug}: {}\n{}\n", self.state(&slug), fs::read_to_string(e.path().join("threads/t-0001.toml")).unwrap_or_default())
        }).collect()
    }
    fn edit_state(&self, slug: &str, edit: impl FnOnce(&mut Value)) {
        let mut state = self.state(slug);
        edit(&mut state);
        fs::write(self.project(slug).join(".state/ticker.json"), state.to_string()).unwrap();
    }
    fn finalization(&self, slug: &str) -> Value { self.state(slug)["finalizations"]["t-0001"].clone() }
    /// Inbox items of a project whose file name starts with `prefix`, as text.
    fn inbox(&self, slug: &str, prefix: &str) -> Vec<String> {
        fs::read_dir(self.project(slug).join("inbox")).unwrap().flatten().filter(|e| e.path().is_file() && e.file_name().to_string_lossy().starts_with(prefix))
            .map(|e| fs::read_to_string(e.path()).unwrap()).collect()
    }
    fn gh_calls(&self, number: u32) -> usize {
        fs::read_to_string(self.path("gh-calls")).unwrap_or_default().lines().filter(|l| *l == url(number)).count()
    }
    fn polls(&self, name: &str) -> usize {
        fs::read_to_string(self.path("calls")).unwrap_or_default().lines().filter(|l| l.starts_with(&format!("{name} ")) && l.ends_with(" pane list")).count()
    }
    fn run_ticker(&self) -> Ticker<'_> {
        let child = self.command().args(["ticker", "run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        Ticker { lab: self, child }
    }
}

/// A `ticker run` child; stopped through its stop file, killed on panic.
struct Ticker<'a> { lab: &'a Lab, child: std::process::Child }
impl Ticker<'_> {
    fn wait_for(&mut self, what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(300);
        while !done() {
            assert!(self.child.try_wait().unwrap().is_none(), "ticker exited while waiting for {what}");
            assert!(Instant::now() < deadline, "timed out waiting for {what}; ticker log:\n{}\nretry records:\n{}",
                fs::read_to_string(self.lab.root().join(".ticker.log")).unwrap_or_default(), self.lab.states());
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    /// Waits until session `name` has been observed twice more.
    fn another_pass(&mut self, name: &str) {
        let seen = self.lab.polls(name);
        self.wait_for("two more observations", || self.lab.polls(name) >= seen + 2);
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

fn url(number: u32) -> String { format!("https://github.com/owner/app/pull/{number}") }
fn pane(id: &str, cwd: &Path) -> Value { json!({"workspace_id": "w", "tab_id": "w:t", "pane_id": id, "terminal_id": format!("term-{id}"), "cwd": cwd}) }
fn agent(id: &str, cwd: &Path, name: &str) -> Value {
    let mut agent = pane(id, cwd);
    agent["name"] = json!(name);
    agent["agent"] = json!("claude");
    agent["agent_status"] = json!("idle");
    agent
}
/// Moves a thread's source aside (the copy cannot find it) or back.
fn source_present(source: &Path, yes: bool) {
    let aside = source.with_extension("aside");
    if yes { fs::rename(aside, source).unwrap() } else { fs::rename(source, aside).unwrap() }
}

/// Three merged pull requests in one root. A copy that cannot find its
/// source keeps its finalization pending and visible in `doctor`; a thread
/// resolved and reopened by hand, or whose report now names another pull
/// request, is never finalized for the old one, even once the copy would
/// succeed. A library holding a symbolic link resolves with one durable
/// notice naming what was left out.
#[test]
fn merged_finalization_retries_failed_copies_and_is_withdrawn_by_reopen_or_a_new_report() {
    let mut lab = Lab::new();
    let mut sources = Vec::new();
    for (slug, number) in [("reopen", 1), ("rewrite", 2), ("partial", 4)] {
        let project = lab.project_in_session(slug);
        let source = lab.merged_thread(slug, number, json!({}));
        lab.session(slug, &[agent("p", &project, "coordinator"), agent("pw", &source, "worker-pw")], &[pane("p", &project), pane("pw", &source)]);
        sources.push(source);
    }
    for source in &sources[..2] { source_present(source, false); }
    std::os::unix::fs::symlink("item", sources[2].join("library/link")).unwrap();
    lab.pull_request(3, "OPEN");

    let mut ticker = lab.run_ticker();
    ticker.wait_for("the partial copy's notice", || lab.text("partial", "status") == "resolved" && lab.thread("partial").get("pending_final_notice").is_none());
    let attempts = |slug: &str| lab.finalization(slug)["retry"]["attempts"].as_u64().unwrap_or(0);
    ticker.wait_for("a second attempt at each failing copy", || attempts("reopen") >= 2 && attempts("rewrite") >= 2);
    ticker.stop();

    // The partial copy resolved the thread and left one notice naming the link.
    assert_eq!(lab.text("partial", "resolved_reason"), "merged");
    assert_eq!(lab.text("partial", "artifact_snapshot"), "");
    let notices = lab.inbox("partial", "final-");
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].contains("library/link was omitted (symbolic link)"), "{}", notices[0]);
    assert_eq!(fs::read(lab.project("partial").join("library/t-0001/item")).unwrap(), b"binary\0\xff");
    // The failing copies left the threads open, with the pending copy visible.
    for slug in ["reopen", "rewrite"] {
        assert_eq!((lab.text(slug, "status").as_str(), lab.inbox(slug, "final-").len()), ("open", 0), "{slug}");
        assert_eq!(lab.finalization(slug)["pr"], json!(url(if slug == "reopen" { 1 } else { 2 })));
    }
    let doctor = String::from_utf8(lab.cli(&["doctor"]).stdout).unwrap();
    assert!(doctor.contains("[warn] project reopen: 1 pending finalization(s), 0 pending inbox event(s), notification retry none;"), "{doctor}");
    assert!(doctor.contains(&format!("[warn] project reopen thread t-0001: final copy attempt {};", attempts("reopen"))), "{doctor}");

    // Resolved and reopened by hand; the other report now names pull request 3.
    lab.ok(&["thread", "resolve", "reopen", "t-0001", "--skip-copy"]);
    lab.ok(&["thread", "resolve", "reopen", "t-0001", "--reopen"]);
    fs::write(lab.project("rewrite").join("threads/t-0001.md"), format!("PR: {}\n", url(3))).unwrap();
    // Every copy would now succeed, and the next pass reads every pull request again.
    for source in &sources[..2] { source_present(source, true); }
    for slug in ["reopen", "rewrite", "partial"] { lab.edit_state(slug, |s| s["last_pr_check"] = json!("")); }
    let reads = (lab.gh_calls(1), lab.gh_calls(3));

    let mut ticker = lab.run_ticker();
    ticker.wait_for("pull requests 1 and 3 read again", || lab.gh_calls(1) > reads.0 && lab.gh_calls(3) > reads.1);
    ticker.wait_for("the withdrawn finalizations", || lab.finalization("reopen").is_null() && lab.finalization("rewrite").is_null());
    ticker.another_pass("reopen");
    ticker.another_pass("rewrite");
    ticker.stop();

    for (slug, old) in [("reopen", 1), ("rewrite", 2)] {
        assert_eq!(lab.text(slug, "status"), "open", "{slug}");
        assert_eq!(lab.text(slug, "suppressed_merged_pr"), url(old), "{slug}");
        assert!(lab.inbox(slug, "final-").is_empty(), "{slug}");
        assert!(lab.state(slug)["finalizations"].as_object().is_none_or(|f| f.is_empty()), "{slug}");
    }
    assert_eq!(lab.text("reopen", "pr"), url(1));
    assert_eq!((lab.text("rewrite", "pr"), lab.text("rewrite", "pr_state")), (url(3), "OPEN".to_string()));
    assert_eq!(lab.inbox("partial", "final-").len(), 1);
}

/// A remote thread's merged PR cannot be finalized while the machine lacks the
/// artifact helper: the thread stays open, and `thread resolve` refuses and
/// names itself as the retry. Once the helper is installed, that explicit
/// resolve copies and preserves the thread, and the ticker drops its retry.
#[test]
fn remote_merged_finalization_waits_for_the_helper_and_an_explicit_resolve() {
    let mut lab = Lab::new();
    let project = lab.project_in_session("demo");
    fs::write(lab.path("machines.json"), json!([{"label": "box", "target": "fixture.invalid", "enabled": true}]).to_string()).unwrap();
    let source = lab.merged_thread("demo", 7, json!({"machine": "box"}));
    lab.session("demo@box", &[agent("pw", &source, "worker-pw")], &[pane("pw", &source)]);

    let mut ticker = lab.run_ticker();
    ticker.wait_for("the merged PR's finalization", || lab.finalization("demo")["retry"]["attempts"].as_u64() >= Some(2));
    ticker.stop();
    assert_eq!(lab.text("demo", "status"), "open");
    assert!(!project.join("library/t-0001").exists());
    assert!(lab.inbox("demo", "final-").is_empty());

    let refused = lab.cli(&["thread", "resolve", "demo", "t-0001"]);
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(!refused.status.success());
    assert!(stderr.contains("[transport-unsupported]") && stderr.contains("then run `thread resolve demo t-0001` to retry"), "{stderr}");
    assert_eq!(lab.text("demo", "status"), "open");

    std::os::unix::fs::symlink(BIN, lab.path("helper")).unwrap();
    lab.ok(&["thread", "resolve", "demo", "t-0001"]);
    assert_eq!((lab.text("demo", "status").as_str(), lab.text("demo", "resolved_reason").as_str()), ("resolved", "manual"));
    assert!(!lab.text("demo", "artifact_snapshot").is_empty());
    assert_eq!(fs::read(project.join("library/t-0001/item")).unwrap(), b"binary\0\xff");

    let mut ticker = lab.run_ticker();
    ticker.wait_for("the dropped retry", || lab.finalization("demo").is_null());
    ticker.stop();
    assert_eq!(lab.text("demo", "status"), "resolved");
    assert!(lab.inbox("demo", "final-").is_empty());
}

/// A toast the session declines to show is retried after a restart, but not
/// before its recorded backoff, and once shown it is not sent again. While
/// it waits, `doctor` reports the retry without failing on it.
#[test]
fn a_declined_toast_is_retried_after_restart_only_once_its_backoff_is_due() {
    let mut lab = Lab::new();
    let project = lab.project_in_session("demo");
    fs::write(project.join("inbox/item-a.md"), "+++\nid='item-a'\nkind='test'\nsummary='Fixture'\n+++\n").unwrap();
    fs::write(lab.path("demo.toast"), "disabled").unwrap();
    let toasts = || fs::read_to_string(lab.path("demo.toasts")).unwrap_or_default().lines().map(|l| l.parse::<f64>().unwrap()).collect::<Vec<_>>();
    let claim = || lab.state("demo")["notification_claim"].clone();

    let mut ticker = lab.run_ticker();
    ticker.wait_for("the declined toast", || claim()["phase"] == "not-shown");
    ticker.stop();
    assert_eq!(toasts().len(), 1);
    let retry = lab.state("demo")["notification_retry"]["retry"].clone();
    assert_eq!(retry["attempts"], 1);
    assert!(retry["last_error"].as_str().unwrap().contains("not shown: disabled"), "{retry}");
    let due: jiff::Timestamp = retry["next_attempt"].as_str().unwrap().parse().unwrap();
    let doctor = lab.cli(&["doctor"]);
    let text = String::from_utf8_lossy(&doctor.stdout);
    assert!(text.contains("[warn] project demo: 0 pending finalization(s), 0 pending inbox event(s), notification retry pending;"), "{text}");
    assert!(text.contains(&format!("[warn] project demo: notification: Native toast not shown: disabled; next {}", retry["next_attempt"].as_str().unwrap())), "{text}");
    assert!(!text.contains("[FAIL] project demo"), "{text}");

    fs::remove_file(lab.path("demo.toast")).unwrap();
    let mut ticker = lab.run_ticker();
    ticker.wait_for("the retried toast", || claim()["phase"] == "confirmed");
    ticker.another_pass("demo");
    ticker.another_pass("demo");
    ticker.stop();
    let sent = toasts();
    assert_eq!(sent.len(), 2);
    assert!(sent[1] >= due.as_millisecond() as f64 / 1000.0, "retried at {} before its backoff {due}", sent[1]);
    assert_eq!((claim()["sequence"].as_u64(), claim()["retry_of"].as_u64()), (Some(2), Some(1)));
    assert_eq!(lab.state("demo")["notification_retry"]["retry"]["attempts"].as_u64().unwrap_or(0), 0);
}

/// Replaces `strict_inventory_captures_sorted_unseen_ids_and_verifies_consumption`.
///
/// A toast claims every unseen inbox item, sorted whatever order they were
/// written in. Items `context` has shown and items handled with `inbox done`
/// are left out of the next toast, which claims only the new unseen item.
#[test]
fn each_toast_claims_the_sorted_unseen_items_and_leaves_out_seen_and_handled_ones() {
    let mut lab = Lab::new();
    let project = lab.project_in_session("demo");
    let write = |id: &str| fs::write(project.join(format!("inbox/{id}.md")), format!("+++\nid='{id}'\nkind='test'\nsummary='Fixture {id}'\n+++\n")).unwrap();
    let claim = || lab.state("demo")["notification_claim"].clone();
    let toasts = || fs::read_to_string(lab.path("demo.toasts")).unwrap_or_default().lines().count();
    write("item-b");
    write("item-a");
    let mut ticker = lab.run_ticker();
    ticker.wait_for("the first toast", || claim()["phase"] == "confirmed");
    ticker.stop();
    assert_eq!(claim()["batch"]["ids"], json!(["item-a", "item-b"]), "{}", claim());

    lab.ok(&["context", "demo"]);
    write("item-d");
    write("item-c");
    lab.ok(&["inbox", "done", "demo", "item-d"]);
    let mut ticker = lab.run_ticker();
    ticker.wait_for("the second toast", || claim()["sequence"] == 2 && claim()["phase"] == "confirmed");
    ticker.another_pass("demo");
    ticker.stop();
    assert_eq!(claim()["batch"]["ids"], json!(["item-c"]), "{}", claim());
    assert_eq!(toasts(), 2);
}

/// `gh` failing for one pull request is reported once the failure has lasted
/// the outage threshold, counted from the first failure even across a
/// restart, and a healthy pull request in the same project does not reset
/// it. Further failures add nothing; the first success adds one recovery item.
#[test]
fn a_gh_outage_outlasts_restarts_and_gives_one_item_each_way() {
    let mut lab = Lab::new();
    let project = lab.project_in_session("demo");
    let source = lab.merged_thread("demo", 5, json!({}));
    fs::remove_file(lab.path("gh/5.json")).unwrap();
    let mut other = lab.thread("demo");
    other.as_table_mut().unwrap().extend([("id".into(), "t-0002".into()), ("pane_id".into(), "pw2".into())]);
    fs::write(project.join("threads/t-0002.toml"), toml::to_string(&other).unwrap()).unwrap();
    fs::write(project.join("threads/t-0002.md"), format!("PR: {}\n", url(6))).unwrap();
    lab.pull_request(6, "OPEN");
    lab.session("demo", &[agent("p", &project, "coordinator"), agent("pw", &source, "worker-pw"), agent("pw2", &source, "worker-pw")],
        &[pane("p", &project), pane("pw", &source), pane("pw2", &source)]);
    let outages = || lab.inbox("demo", "").into_iter().filter(|item| item.contains("kind = \"outage\"")).collect::<Vec<_>>();
    let since = || lab.state("demo")["gh_outages"][url(5)]["failing_since"].as_i64();
    let run = |threshold: &str, done: &dyn Fn() -> bool| {
        lab.edit_state("demo", |s| s["last_pr_check"] = json!(""));
        let reads = (lab.gh_calls(5), lab.gh_calls(6));
        let child = lab.command().env("HERDR_PROJECTS_OUTAGE_SECS", threshold).args(["ticker", "run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        let mut ticker = Ticker { lab: &lab, child };
        ticker.wait_for("both pull requests read", || lab.gh_calls(5) > reads.0 && lab.gh_calls(6) > reads.1 && done());
        ticker.another_pass("demo");
        ticker.stop();
    };

    run("3600", &|| since().is_some());
    let first = since();
    assert!(outages().is_empty());
    for _ in 0..2 {
        run("1", &|| !outages().is_empty());
        assert_eq!(since(), first, "the failure streak was restarted");
        let items = outages();
        assert_eq!(items.len(), 1, "{items:?}");
        assert!(items[0].contains(&format!("`gh` has been failing for 0 minutes for {}", url(5))), "{}", items[0]);
    }
    assert!(lab.state("demo")["gh_outages"].get(url(6)).is_none());

    lab.pull_request(5, "OPEN");
    run("1", &|| outages().len() == 2);
    let items = outages();
    assert!(items.iter().any(|item| item.contains(&format!("`gh` is working again for {}", url(5)))), "{items:?}");
    assert!(lab.state("demo")["gh_outages"].as_object().unwrap().is_empty());
}

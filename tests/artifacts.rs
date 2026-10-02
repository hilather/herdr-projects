#![cfg(target_os = "linux")]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! Artifact preservation, final copies and live copies through the compiled
//! CLI: `thread resolve`, `report-hash` and `ticker run` passes. herdr, gh
//! and ssh are fakes; every copy uses the binary's own `artifact-stream`.
//! Waits are on persisted records and logged polls, never on elapsed time.
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{fs, os::unix::{fs::{symlink, PermissionsExt}, net::UnixListener}, path::{Path, PathBuf}, process::{Command, Output, Stdio}, time::{Duration, Instant}};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-farm");

/// Session `NAME` is the socket `$HOME/NAME.sock` (`NAME@MACHINE` through
/// `--machine`); it lists `NAME.agents` and `NAME.panes`, and acknowledges a
/// prompt for its worker. Every call is logged to `calls` as `NAME ARGS`.
const FAKE_HERDR: &str = r#"#!/usr/bin/python3
import json,os,pathlib,sys
home=pathlib.Path(os.environ['HOME']);args=sys.argv[1:]
name=pathlib.Path(os.environ.get('HERDR_SOCKET_PATH','none')).stem
if args[:1]==['--machine']:name=name+'@'+args[1];args=args[2:]
def read(kind):
    p=home/f'{name}.{kind}'
    return json.loads(p.read_text()) if p.exists() else []
with open(home/'calls','a') as f:f.write(name+' '+' '.join(args)+'\n')
if args==['machine','list','--json']:print((home/'machines.json').read_text())
elif args==['--version']:print('herdr 0.9.1')
elif args in (['agent','list'],['pane','list']):print(json.dumps({'result':{args[0]+'s':read(args[0]+'s')}}))
elif args[:2]==['agent','prompt']:print(json.dumps({'result':{'type':'agent_prompted','agent':[a for a in read('agents') if a['pane_id']!='p'][0]}}))
else:print('{"result":{"shown":true}}')
"#;

struct Lab { home: tempfile::TempDir, listeners: Vec<UnixListener> }

impl Lab {
    fn new() -> Self {
        let lab = Lab { home: tempfile::tempdir().unwrap(), listeners: Vec::new() };
        lab.script("herdr", FAKE_HERDR);
        for dir in ["bin", "gh"] { fs::create_dir(lab.path(dir)).unwrap(); }
        // gh answers `pr view` for pull request N from `gh/N.json`.
        lab.script("bin/gh", &format!("#!/usr/bin/python3\nimport sys\nprint(open({:?}+'/'+sys.argv[-1].rsplit('/',1)[1]+'.json').read())\n", lab.path("gh").to_str().unwrap()));
        // ssh to `fixture.invalid` runs its script here.
        lab.script("bin/ssh", "#!/bin/sh\n[ \"$(eval echo \\${$(($#-1))})\" = fixture.invalid ] || exit 9\neval \"exec /bin/sh -c \\\"\\${$#}\\\"\"\n");
        fs::write(lab.path("machines.json"), json!([{"label": "box", "target": "fixture.invalid", "enabled": true}]).to_string()).unwrap();
        lab
    }
    fn path(&self, name: &str) -> PathBuf { self.home.path().join(name) }
    fn script(&self, name: &str, body: &str) {
        fs::write(self.path(name), body).unwrap();
        fs::set_permissions(self.path(name), fs::Permissions::from_mode(0o700)).unwrap();
    }
    fn project(&self, slug: &str) -> PathBuf { self.path("root").join(slug) }
    fn command(&self) -> Command {
        let mut command = Command::new(BIN);
        command.env_clear().env("HOME", self.home.path()).env("PATH", format!("{}:/usr/bin:/bin", self.path("bin").display()))
            .env("HERDR_BIN_PATH", self.path("herdr")).env("HERDR_PROJECTS_REMOTE_BIN", self.path("helper")).arg("--root").arg(self.path("root"));
        command
    }
    fn cli(&self, args: &[&str]) -> Output { self.command().args(args).output().unwrap() }
    fn ok(&self, args: &[&str]) -> String {
        let out = self.cli(args);
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }
    fn fails(&self, args: &[&str]) -> String {
        let out = self.cli(args);
        assert!(!out.status.success(), "{args:?} succeeded: {}", String::from_utf8_lossy(&out.stdout));
        String::from_utf8(out.stderr).unwrap()
    }
    /// A project whose coordinator lives in session `slug`, and its thread
    /// `t-0001`, an idle worker in pane `pw` (on `machine`, if not empty)
    /// working in `$HOME/<slug>-source`: `report.md` and `library/`.
    fn thread(&mut self, slug: &str, machine: &str, extra: Value) -> PathBuf {
        self.ok(&["new", slug]);
        let project = self.project(slug);
        let socket = self.path(&format!("{slug}.sock"));
        self.listeners.push(UnixListener::bind(&socket).unwrap());
        fs::write(project.join(".state/coordinator.json"), json!({"socket": socket, "workspace_id": "w", "tab_id": "w:t", "pane_id": "p",
            "agent_name": "coordinator", "cwd": project}).to_string()).unwrap();
        let source = self.path(&format!("{slug}-source"));
        fs::create_dir_all(source.join("library/empty")).unwrap();
        fs::write(source.join("report.md"), b"report\0\xff").unwrap();
        fs::write(source.join("library/artifact"), b"version A").unwrap();
        let mut record = json!({"id": "t-0001", "title": "Task", "status": "open", "kind": "adopted", "created": jiff::Timestamp::now().to_string(),
            "agent": "claude", "agent_name": "worker", "workspace_id": "w", "tab_id": "w:t", "pane_id": "pw", "cwd": source, "thread_dir": source,
            "machine": machine, "last_state": "idle", "last_group": "idle"});
        for (k, v) in extra.as_object().unwrap() { record[k] = v.clone(); }
        fs::write(project.join("threads/t-0001.toml"), toml::to_string(&record).unwrap()).unwrap();
        let worker = [agent("pw", &source)];
        let local = [agent("p", &project)];
        let (here, there) = if machine.is_empty() { ([&local[..], &worker[..]].concat(), vec![]) } else { (local.to_vec(), worker.to_vec()) };
        self.session(slug, &here);
        if !machine.is_empty() { self.session(&format!("{slug}@{machine}"), &there); }
        source
    }
    /// Session `name` lists these agents, each in its own pane.
    fn session(&self, name: &str, agents: &[Value]) {
        let panes: Vec<Value> = agents.iter().map(|a| json!({"workspace_id": "w", "tab_id": "w:t", "pane_id": a["pane_id"], "terminal_id": a["terminal_id"], "cwd": a["cwd"]})).collect();
        fs::write(self.path(&format!("{name}.agents")), Value::from(agents.to_vec()).to_string()).unwrap();
        fs::write(self.path(&format!("{name}.panes")), Value::from(panes).to_string()).unwrap();
    }
    fn record(&self, slug: &str) -> toml::Value {
        toml::from_str(&fs::read_to_string(self.project(slug).join("threads/t-0001.toml")).unwrap()).unwrap()
    }
    fn text(&self, slug: &str, key: &str) -> String { self.record(slug).get(key).and_then(|v| v.as_str()).unwrap_or_default().to_string() }
    fn edit(&self, slug: &str, edit: impl FnOnce(&mut toml::Table)) {
        let mut record = self.record(slug);
        edit(record.as_table_mut().unwrap());
        fs::write(self.project(slug).join("threads/t-0001.toml"), toml::to_string(&record).unwrap()).unwrap();
    }
    /// The preserved snapshots of a project's thread, by id.
    fn snapshots(&self, slug: &str) -> Vec<String> {
        let mut ids: Vec<String> = fs::read_dir(self.project(slug).join(".state/artifacts/t-0001")).into_iter().flatten()
            .map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        ids.sort();
        ids
    }
    /// A snapshot's directory, after checking that its id is its manifest's digest.
    fn snapshot(&self, slug: &str, id: &str) -> PathBuf {
        let dir = self.project(slug).join(".state/artifacts/t-0001").join(id);
        assert_eq!(format!("{:x}", Sha256::digest(fs::read(dir.join("manifest.json")).unwrap())), id);
        dir
    }
    fn manifest(&self, slug: &str, id: &str) -> Value { serde_json::from_slice(&fs::read(self.snapshot(slug, id).join("manifest.json")).unwrap()).unwrap() }
    fn inbox(&self, slug: &str, prefix: &str) -> Vec<String> {
        fs::read_dir(self.project(slug).join("inbox")).unwrap().flatten().filter(|e| e.path().is_file() && e.file_name().to_string_lossy().starts_with(prefix))
            .map(|e| fs::read_to_string(e.path()).unwrap()).collect()
    }
    fn polls(&self, name: &str) -> usize {
        fs::read_to_string(self.path("calls")).unwrap_or_default().lines().filter(|l| l.starts_with(&format!("{name} ")) && l.ends_with("pane list")).count()
    }
    fn pull_request(&self, number: u32, state: &str) {
        fs::write(self.path(&format!("gh/{number}.json")), json!({"state": state, "reviewDecision": "", "headRefName": "branch",
            "headRepository": {"name": "app"}, "headRepositoryOwner": {"login": "owner"}}).to_string()).unwrap();
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
            assert!(Instant::now() < deadline, "timed out waiting for {what}; ticker log:\n{}",
                fs::read_to_string(self.lab.path("root/.ticker.log")).unwrap_or_default());
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    /// Waits until session `name` has been observed twice more.
    fn another_pass(&mut self, name: &str) {
        let seen = self.lab.polls(name);
        self.wait_for("two more observations", || self.lab.polls(name) >= seen + 2);
    }
    fn stop(mut self) {
        let stop = self.lab.path("root/.ticker.stop");
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

fn agent(id: &str, cwd: &Path) -> Value {
    json!({"workspace_id": "w", "tab_id": "w:t", "pane_id": id, "terminal_id": format!("term-{id}"), "cwd": cwd,
        "name": if id == "p" { "coordinator" } else { "worker" }, "agent": "claude", "agent_status": "idle"})
}
fn url(number: u32) -> String { format!("https://github.com/owner/app/pull/{number}") }
fn sha(bytes: &[u8]) -> String { format!("{:x}", Sha256::digest(bytes)) }

/// Each final copy preserves the source as a content-addressed snapshot. A
/// `thread resolve` that stops at cleanup (the worker's pane is still open)
/// keeps its snapshot, so resolving unchanged bytes again reuses it, and new
/// bytes of the same size and time get a new one beside it. A source that is
/// gone cannot be resolved, and the last snapshot stays the thread's receipt.
#[test]
fn resolve_preserves_each_version_once_and_a_lost_source_keeps_the_last_snapshot() {
    let mut lab = Lab::new();
    fs::create_dir(lab.path("repo")).unwrap();
    let source = lab.thread("demo", "", json!({"kind": "worktree", "worktree_path": lab.path("demo-source"), "repo": lab.path("repo")}));
    let cleanup = ["thread", "resolve", "demo", "t-0001", "--remove-worktree"];
    for _ in 0..2 {
        assert!(lab.fails(&cleanup).contains("idle is not proof of writer quiescence"));
    }
    let first = lab.text("demo", "artifact_snapshot");
    assert_eq!((lab.text("demo", "status").as_str(), lab.snapshots("demo")), ("open", vec![first.clone()]));

    let artifact = source.join("library/artifact");
    let stamp = fs::metadata(&artifact).unwrap().modified().unwrap();
    fs::write(&artifact, b"version B").unwrap();
    fs::File::options().write(true).open(&artifact).unwrap().set_modified(stamp).unwrap();
    assert!(lab.fails(&cleanup).contains("idle is not proof of writer quiescence"));
    let second = lab.text("demo", "artifact_snapshot");
    assert_ne!(second, first);
    assert_eq!(lab.snapshots("demo").len(), 2);
    for (id, bytes) in [(&first, b"version A"), (&second, b"version B")] {
        let dir = lab.snapshot("demo", id);
        assert_eq!(fs::read(dir.join("library/artifact")).unwrap(), bytes);
        assert_eq!(fs::read(dir.join("report.md")).unwrap(), b"report\0\xff");
        assert!(dir.join("library/empty").is_dir());
    }

    fs::remove_dir_all(&source).unwrap();
    let refused = lab.fails(&["thread", "resolve", "demo", "t-0001"]);
    assert!(refused.contains("artifact source is missing"), "{refused}");
    assert_eq!((lab.text("demo", "status"), lab.text("demo", "artifact_snapshot")), ("open".to_string(), second.clone()));
    assert_eq!(lab.snapshots("demo").len(), 2);
    assert_eq!(fs::read(lab.snapshot("demo", &second).join("library/artifact")).unwrap(), b"version B");
}

/// A remote thread is preserved over the machine's `artifact-stream` helper.
/// Without a compatible helper nothing is written; with one, the snapshot
/// records the machine and holds the exact bytes, empty directories and
/// hostile names, and streaming the same source again reuses it.
#[test]
fn remote_resolve_needs_the_helper_and_streams_exact_bytes_once() {
    let mut lab = Lab::new();
    let source = lab.thread("demo", "box", json!({"kind": "worktree", "worktree_path": lab.path("demo-source"), "repo": "/repo"}));
    fs::write(source.join("library/a 'λ$\nfile"), [0, 255, 254]).unwrap();
    let cleanup = ["thread", "resolve", "demo", "t-0001", "--remove-worktree"];
    assert!(lab.fails(&cleanup).contains("[transport-unsupported]"));
    assert!(!lab.project("demo").join(".state/artifacts").exists());
    assert!(!lab.project("demo").join("library/t-0001").exists());

    symlink(BIN, lab.path("helper")).unwrap();
    for _ in 0..2 {
        assert!(lab.fails(&cleanup).contains("remote writer quiescence cannot be established"));
    }
    let id = lab.text("demo", "artifact_snapshot");
    assert_eq!((lab.text("demo", "status").as_str(), lab.snapshots("demo")), ("open", vec![id.clone()]));
    assert_eq!(lab.manifest("demo", &id)["machine"], "box");
    let dir = lab.snapshot("demo", &id);
    for library in [dir.join("library"), lab.project("demo").join("library/t-0001")] {
        assert_eq!(fs::read(library.join("a 'λ$\nfile")).unwrap(), [0, 255, 254]);
        assert_eq!(fs::read(library.join("artifact")).unwrap(), b"version A");
        assert!(library.join("empty").is_dir());
    }
    assert_eq!(fs::read(dir.join("report.md")).unwrap(), b"report\0\xff");
    assert_eq!(fs::read(lab.project("demo").join("threads/t-0001.md")).unwrap(), b"report\0\xff");
}

impl Lab {
    /// A thread whose home report names merged pull request 1 and was acknowledged.
    fn merged(&mut self, slug: &str) -> PathBuf {
        let home = format!("PR: {}\n## Report\ndone\n", url(1));
        let hash = sha(home.as_bytes());
        let source = self.thread(slug, "", json!({"branch": "branch", "origin": "git@github.com:Owner/App.git", "report_hash": hash,
            "acked_report_hash": hash, "last_review_item_hash": hash}));
        fs::write(self.project(slug).join("threads/t-0001.md"), &home).unwrap();
        fs::write(source.join("report.md"), &home).unwrap();
        source
    }
}

/// Merged pull request 1 finalizes four threads whose sources differ. With
/// no report, the thread resolves and its snapshot holds only the library;
/// the home report and its receipt are left alone. A report that is a link
/// is omitted: the thread resolves with a notice, but no snapshot and no
/// receipt. A report that now names another pull request is copied, yet the
/// thread stays open, with a notice saying why. A final copy whose home library is a link to outside
/// the project stays pending, and no resolve or deletion may run past it;
/// once the link is gone it completes from the bytes it already received,
/// even though the source has been deleted.
#[test]
fn merged_finalization_keeps_what_each_source_shape_proves() {
    let mut lab = Lab::new();
    lab.pull_request(1, "MERGED");
    lab.pull_request(2, "OPEN");
    let old = format!("PR: {}\n## Report\ndone\n", url(1));
    let missing = lab.merged("missing");
    fs::remove_file(missing.join("report.md")).unwrap();
    let omitted = lab.merged("omitted");
    fs::remove_file(omitted.join("report.md")).unwrap();
    symlink("library/artifact", omitted.join("report.md")).unwrap();
    let changed = lab.merged("changed");
    let new = format!("PR: {}\nnew\n", url(2));
    fs::write(changed.join("report.md"), &new).unwrap();
    let blocked = lab.merged("blocked");
    fs::create_dir(lab.path("outside")).unwrap();
    let link = lab.project("blocked").join("library/t-0001");
    symlink(lab.path("outside"), &link).unwrap();

    let mut ticker = lab.run_ticker();
    ticker.wait_for("every finalization", || lab.text("missing", "status") == "resolved" && lab.text("omitted", "status") == "resolved"
        && lab.record("changed")["final_copy_sequence"].as_integer() == Some(1) && lab.record("changed").get("pending_final_copy").is_none()
        && lab.record("blocked").get("pending_final_copy").is_some());
    ticker.another_pass("blocked");
    ticker.stop();

    let dir = lab.snapshot("missing", &lab.text("missing", "artifact_snapshot"));
    assert_eq!((lab.text("missing", "resolved_reason"), lab.text("missing", "report_hash")), ("merged".to_string(), sha(old.as_bytes())));
    assert!(!dir.join("report.md").exists());
    assert_eq!(fs::read(dir.join("library/artifact")).unwrap(), b"version A");
    assert!(lab.record("missing").get("copy_receipt").is_none());
    assert_eq!(fs::read_to_string(lab.project("missing").join("threads/t-0001.md")).unwrap(), old);

    assert_eq!((lab.text("omitted", "resolved_reason").as_str(), lab.text("omitted", "artifact_snapshot").as_str()), ("merged", ""));
    assert_eq!(lab.text("omitted", "report_hash"), sha(old.as_bytes()));
    assert!(lab.record("omitted").get("copy_receipt").is_none());
    let notices = lab.inbox("omitted", "final-");
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].contains("report.md was omitted (symbolic link)"), "{}", notices[0]);

    assert_eq!((lab.text("changed", "status").as_str(), lab.text("changed", "resolved_reason").as_str()), ("open", ""));
    assert_eq!((lab.text("changed", "artifact_snapshot").as_str(), lab.text("changed", "last_finalization").as_str()), ("", ""));
    assert_eq!(lab.record("changed")["copy_receipt"]["report_hash"].as_str(), Some(sha(new.as_bytes()).as_str()));
    let notices = lab.inbox("changed", "final-");
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].contains("changed eligibility prevented automatic resolution"), "{}", notices[0]);

    // The blocked copy is pending; nothing may resolve or delete around it.
    assert_eq!(lab.text("blocked", "status"), "open");
    assert_eq!(fs::read_dir(lab.path("outside")).unwrap().count(), 0);
    let before = fs::read(lab.project("blocked").join("threads/t-0001.toml")).unwrap();
    for args in [&["thread", "resolve", "blocked", "t-0001"][..], &["thread", "resolve", "blocked", "t-0001", "--skip-copy"], &["delete", "blocked"]] {
        let refused = lab.fails(args);
        assert!(refused.contains("recover"), "{args:?}: {refused}");
    }
    assert_eq!(fs::read(lab.project("blocked").join("threads/t-0001.toml")).unwrap(), before);

    fs::remove_dir_all(&blocked).unwrap();
    fs::remove_file(&link).unwrap();
    let mut ticker = lab.run_ticker();
    ticker.wait_for("the recovered final copy", || lab.text("blocked", "status") == "resolved" && lab.record("blocked").get("pending_final_notice").is_none());
    ticker.stop();
    assert_eq!(lab.text("blocked", "resolved_reason"), "merged");
    let dir = lab.snapshot("blocked", &lab.text("blocked", "artifact_snapshot"));
    assert_eq!(fs::read(dir.join("library/artifact")).unwrap(), b"version A");
    assert!(dir.join("library/empty").is_dir());
    assert_eq!(fs::read(link.join("artifact")).unwrap(), b"version A");
    assert_eq!(fs::read_dir(lab.path("outside")).unwrap().count(), 0);
    assert_eq!(lab.inbox("blocked", "final-").len(), 1);
}

/// `report-hash` tells a missing report (no hash) from one it refuses to
/// read: a link, a hard link, a pipe, a directory or one over the size limit.
#[test]
fn report_hash_tells_a_missing_report_from_an_unsafe_one() {
    let lab = Lab::new();
    let source = lab.path("source");
    fs::create_dir(&source).unwrap();
    let hash = |path: &Path| lab.command().args(["report-hash", "--path"]).arg(path).output().unwrap();
    for path in [&source, &lab.path("missing")] {
        let out = hash(path);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), json!({"hash": null}));
    }
    let report = source.join("report.md");
    fs::write(lab.path("original"), b"data").unwrap();
    for kind in ["symlink", "hardlink", "fifo", "oversize", "directory"] {
        match kind {
            "symlink" => symlink("missing", &report).unwrap(),
            "hardlink" => fs::hard_link(lab.path("original"), &report).unwrap(),
            "fifo" => assert!(Command::new("mkfifo").arg(&report).status().unwrap().success()),
            "oversize" => fs::File::create(&report).unwrap().set_len(50 * 1024 * 1024 + 1).unwrap(),
            _ => fs::create_dir(&report).unwrap(),
        }
        let out = hash(&source);
        assert!(!out.status.success(), "{kind}: {}", String::from_utf8_lossy(&out.stdout));
        if kind == "directory" { fs::remove_dir(&report).unwrap() } else { fs::remove_file(&report).unwrap() }
    }
}

/// The ticker copies each new report home as it appears, beside whatever the
/// home library already holds. Every distinct copy gets the next receipt and
/// its own review notice, even when its bytes repeat an earlier report; a
/// copy that omits something says so. A live copy is not preservation: no
/// snapshot is recorded and no stage is left behind. When the worker moves
/// to another pane, the same report is announced again for the new execution.
#[test]
fn live_copies_get_a_receipt_and_review_notice_each_and_a_new_execution_is_announced_again() {
    let mut lab = Lab::new();
    let source = lab.thread("demo", "", json!({}));
    let home = lab.project("demo").join("library/t-0001");
    fs::create_dir_all(&home).unwrap();
    fs::write(home.join("retained"), b"old content").unwrap();
    let receipt = |key: &str| lab.record("demo").get("copy_receipt").and_then(|c| c.get(key)).cloned();
    let mut ticker = lab.run_ticker();
    let reports: [&[u8]; 4] = [b"report\0\xff", b"B", b"report\0\xff", b"C"];
    for (n, report) in (1..).zip(reports) {
        if n == 4 { symlink("artifact", source.join("library/link")).unwrap(); }
        fs::write(source.join("report.md"), report).unwrap();
        ticker.wait_for("the copy and its notice", || receipt("sequence") == Some(n.into()) && lab.text("demo", "last_review_item_hash") == sha(report));
        assert_eq!(receipt("report_hash"), Some(sha(report).into()));
        assert_eq!(fs::read(lab.project("demo").join("threads/t-0001.md")).unwrap(), report);
    }
    ticker.another_pass("demo");
    ticker.stop();
    let notices = lab.inbox("demo", "review-");
    assert_eq!(notices.len(), 4, "{notices:?}");
    for n in 1..=4 {
        let notice = notices.iter().find(|n2| n2.contains(&format!("(copy {n})"))).unwrap();
        assert_eq!(notice.contains("not everything was copied: library/link was omitted (symbolic link)"), n == 4, "{notice}");
    }
    assert_eq!(fs::read(home.join("retained")).unwrap(), b"old content");
    assert_eq!(fs::read(home.join("artifact")).unwrap(), b"version A");
    assert!(home.join("empty").is_dir() && !home.join("link").exists());
    assert_eq!(lab.text("demo", "artifact_snapshot"), "");
    assert!(lab.snapshots("demo").is_empty());
    assert_eq!(fs::read_dir(lab.project("demo").join(".state/live-copies")).unwrap().count(), 0);

    // The worker now runs in pane pw2: a new execution with the same report.
    lab.edit("demo", |t| { t.insert("pane_id".into(), "pw2".into()); t.insert("lifecycle_generation".into(), 1.into()); });
    lab.session("demo", &[agent("p", &lab.project("demo")), agent("pw2", &source)]);
    let mut ticker = lab.run_ticker();
    ticker.wait_for("a notice for the new execution", || lab.inbox("demo", "review-").len() == 5);
    ticker.another_pass("demo");
    ticker.stop();
    assert_eq!(lab.inbox("demo", "review-").len(), 5);
    assert_eq!(lab.text("demo", "last_review_item_hash"), sha(b"C"));
    assert_eq!(receipt("sequence"), Some(4.into()));
}

/// A brief waits while a live copy of the thread is pending: the copy into a
/// home library that links outside the project is held, and the idle agent
/// gets nothing. Once the link is gone the copy completes and the brief is
/// sent once.
#[test]
fn a_brief_waits_for_a_pending_live_copy() {
    let mut lab = Lab::new();
    let source = lab.thread("demo", "", json!({"prompt_pending": true}));
    let mut working = agent("pw", &source);
    working["agent_status"] = json!("working");
    lab.session("demo", &[agent("p", &lab.project("demo")), working]);
    fs::create_dir(lab.path("outside")).unwrap();
    let link = lab.project("demo").join("library/t-0001");
    symlink(lab.path("outside"), &link).unwrap();
    let prompts = || fs::read_to_string(lab.path("calls")).unwrap_or_default().lines().filter(|l| l.contains("agent prompt")).count();

    let mut ticker = lab.run_ticker();
    ticker.wait_for("the pending live copy", || lab.record("demo").get("pending_live_copy").is_some());
    lab.session("demo", &[agent("p", &lab.project("demo")), agent("pw", &source)]);
    ticker.another_pass("demo");
    ticker.another_pass("demo");
    assert_eq!(prompts(), 0);
    assert!(lab.record("demo")["prompt_pending"].as_bool().unwrap() && lab.record("demo").get("prompt_claim").is_none());
    assert_eq!(fs::read_dir(lab.path("outside")).unwrap().count(), 0);

    fs::remove_file(&link).unwrap();
    ticker.wait_for("the brief", || lab.record("demo").get("prompt_claim").and_then(|c| c.get("phase")).and_then(|p| p.as_str()) == Some("confirmed"));
    ticker.another_pass("demo");
    ticker.stop();
    assert_eq!(prompts(), 1);
    assert!(lab.record("demo").get("pending_live_copy").is_none());
    assert_eq!(lab.record("demo")["copy_receipt"]["report_hash"].as_str(), Some(sha(b"report\0\xff").as_str()));
    assert_eq!(fs::read(link.join("artifact")).unwrap(), b"version A");
}

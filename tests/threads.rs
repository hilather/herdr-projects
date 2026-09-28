#![cfg(target_os = "linux")]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! Thread records, groups, briefs and the report copy through the compiled
//! CLI. herdr is a shell fixture that serves `agent list` / `pane list` from
//! files and logs every other call; git, du and rsync are the real tools.
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{fs, os::unix::fs::PermissionsExt, path::{Path, PathBuf}, process::{Command, Output, Stdio}, time::{Duration, Instant}};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-projects");

const FAKE_HERDR: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$HOME/herdr-calls"
case "$1 $2" in
'agent list') cat "$HOME/agents.json";;
'pane list') cat "$HOME/panes.json";;
'worktree create')
  if [ -e "$HOME/refuse-worktree" ]; then rm "$HOME/refuse-worktree"; echo '{"error":{"code":"failed","message":"fixture refused"}}'; exit 1; fi
  mkdir -p "$HOME/wt"; dir="$HOME/wt/$(printf %s "$6" | tr / _)"
  git -C "$4" worktree add -q -b "$6" "$dir" "$8" >&2 || exit 1
  n=$(ls "$HOME/wt" | wc -l)
  printf '{"result":{"root_pane":{"workspace_id":"w%s","tab_id":"w%s:t1","pane_id":"w%s:p1","cwd":"%s"},"worktree":{"path":"%s"}}}\n' "$n" "$n" "$n" "$dir" "$dir";;
'worktree open') printf '{"result":{"root_pane":{"workspace_id":"w90","tab_id":"w90:t1","pane_id":"w90:p1","cwd":"%s"},"worktree":{"path":"%s"}}}\n' "$6" "$6";;
'tab create') printf '{"result":{"root_pane":{"workspace_id":"%s","tab_id":"%s:t9","pane_id":"%s:p9"}}}\n' "$4" "$4" "$4";;
'pane get') echo '{"result":{"pane":{}}}';;
'agent prompt'|'pane report-metadata'|'pane clear-metadata') echo '{"result":{}}';;
*) echo '{"error":{"code":"unsupported","message":"not in fixture"}}'; exit 1;;
esac
"#;

struct Lab { home: tempfile::TempDir, fake: PathBuf, _listener: std::os::unix::net::UnixListener }

impl Lab {
    /// A project whose coordinator session is reachable through the fake herdr.
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let fake = home.path().join("herdr");
        fs::write(&fake, FAKE_HERDR).unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
        let socket = home.path().join("session.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let lab = Lab { home, fake, _listener: listener };
        lab.ok(&["new", "demo"]);
        let coordinator = json!({"socket": socket, "workspace_id": "w0", "tab_id": "w0:t1", "pane_id": "w0:p1", "agent_name": "coordinator", "cwd": lab.project()});
        fs::write(lab.project().join(".state/coordinator.json"), coordinator.to_string()).unwrap();
        lab.session(&[], &[]);
        lab
    }
    fn root(&self) -> PathBuf { self.home.path().join("root") }
    fn project(&self) -> PathBuf { self.root().join("demo") }
    fn path(&self, name: &str) -> PathBuf { self.home.path().join(name) }
    fn cli(&self, args: &[&str]) -> Output {
        Command::new(BIN).env_clear().env("HOME", self.home.path()).env("PATH", "/usr/bin:/bin").env("HERDR_BIN_PATH", &self.fake)
            .args(["--root", self.root().to_str().unwrap()]).args(args).output().unwrap()
    }
    fn ok(&self, args: &[&str]) -> String {
        let out = self.cli(args);
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }
    /// What herdr reports: the coordinator's pane plus `panes`, and `agents`.
    fn session(&self, agents: &[Value], panes: &[Value]) {
        let mut all = vec![json!({"workspace_id": "w0", "tab_id": "w0:t1", "pane_id": "w0:p1", "cwd": self.project()})];
        all.extend(panes.iter().cloned());
        fs::write(self.path("panes.json"), json!({"result": {"panes": all}}).to_string()).unwrap();
        fs::write(self.path("agents.json"), json!({"result": {"agents": agents}}).to_string()).unwrap();
    }
    fn record(&self, id: &str) -> toml::Value {
        toml::from_str(&fs::read_to_string(self.project().join(format!("threads/{id}.toml"))).unwrap()).unwrap()
    }
    fn write_record(&self, id: &str, record: &Value) {
        fs::write(self.project().join(format!("threads/{id}.toml")), toml::to_string(record).unwrap()).unwrap();
    }
    /// `thread list`, as id -> (group, live note).
    fn list(&self) -> std::collections::BTreeMap<String, (String, String)> {
        self.ok(&["thread", "list", "demo"]).lines().map(|line| {
            let cols: Vec<&str> = line.split('\t').collect();
            (cols[0].to_string(), (cols[1].to_string(), cols[2].to_string()))
        }).collect()
    }
    /// A foreground `ticker run`, so commands that end with `ticker start`
    /// find it holding the lock instead of spawning a detached one. A ticker
    /// whose first `try_lock` met the status probe's own lock exits quietly;
    /// it is started again.
    fn ticker(&self) -> Ticker {
        let spawn = || Ticker(Command::new(BIN).env_clear().env("HOME", self.home.path()).env("PATH", "/usr/bin:/bin").env("HERDR_BIN_PATH", &self.fake)
            .args(["--root", self.root().to_str().unwrap(), "ticker", "run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
        let mut ticker = spawn();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !self.ok(&["ticker", "status"]).contains("ticker: running") {
            assert!(Instant::now() < deadline, "ticker never took its lock");
            if ticker.0.try_wait().unwrap().is_some() { ticker = spawn(); }
            std::thread::sleep(Duration::from_millis(20));
        }
        ticker
    }
    /// `ok`, retried while a running ticker's tick holds the execution lock.
    fn ok_beside_ticker(&self, args: &[&str]) -> String {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let out = self.cli(args);
            let stderr = String::from_utf8_lossy(&out.stderr);
            if out.status.success() { return String::from_utf8(out.stdout).unwrap(); }
            assert!(stderr.contains("another operation owns lock") && Instant::now() < deadline, "{args:?}: {stderr}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn calls(&self) -> String { fs::read_to_string(self.path("herdr-calls")).unwrap_or_default() }
    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let out = Command::new("/usr/bin/git").env_clear().env("PATH", "/usr/bin:/bin").env("HOME", self.home.path())
            .env("GIT_AUTHOR_NAME", "fixture").env("GIT_AUTHOR_EMAIL", "fixture@example.com")
            .env("GIT_COMMITTER_NAME", "fixture").env("GIT_COMMITTER_EMAIL", "fixture@example.com")
            .arg("-C").arg(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }
}

fn ago(secs: i64) -> String {
    (jiff::Timestamp::now() - jiff::SignedDuration::from_secs(secs)).to_string()
}

fn sha(bytes: &[u8]) -> String { format!("{:x}", Sha256::digest(bytes)) }

/// One thread per state a pane can be in. `thread list` groups each through
/// the same rules the ticker uses; the table is the observable contract.
#[test]
fn thread_list_groups_every_record_and_live_state() {
    let lab = Lab::new();
    struct Case { title: &'static str, record: Value, agent: Option<Value>, pane: bool, group: &'static str, note: &'static str }
    let case = |title, record: Value, agent: Option<Value>, pane, group, note| Case { title, record, agent, pane, group, note };
    let agent = |status: &str| Some(json!({"agent_status": status}));
    let blocked_for = |secs| json!({"last_state": "blocked", "last_state_change": ago(secs)});
    let cases = [
        case("resolved beats a pending, long-blocked launch", json!({"status": "resolved", "resolved_reason": "manual", "prompt_pending": true, "last_state": "blocked", "last_state_change": ago(999)}), agent("blocked"), true, "Resolved", "manual"),
        case("starting for ten seconds", json!({"status": "starting", "created": ago(10)}), None, false, "Working", "pane closed"),
        case("starting past five minutes", json!({"status": "starting", "created": ago(400)}), None, false, "Waiting on you", "pane closed"),
        case("failed though its agent works", json!({"status": "failed", "error": "boom"}), agent("working"), true, "Waiting on you", "failed: boom"),
        case("launch stuck behind a blocked agent", json!({"prompt_pending": true, "last_state": "blocked", "last_state_change": ago(120)}), agent("blocked"), true, "Waiting on you", "blocked"),
        case("launch stuck behind an unknown agent", json!({"prompt_pending": true, "last_state": "unknown", "last_state_change": ago(120)}), agent("unknown"), true, "Waiting on you", "unknown"),
        case("pane closed without a report", json!({}), None, false, "Waiting on you", "pane closed"),
        case("blocked past the debounce", blocked_for(120), agent("blocked"), true, "Waiting on you", "blocked"),
        case("agent working", json!({}), agent("working"), true, "Working", "working"),
        case("permission prompt answered quickly", blocked_for(10), agent("blocked"), true, "Working", "blocked"),
        case("launch waiting for an agent to appear", json!({"prompt_pending": true}), None, true, "Working", "no agent"),
        case("launch behind a briefly unknown agent", json!({"prompt_pending": true, "last_state": "unknown", "last_state_change": ago(10)}), agent("unknown"), true, "Working", "unknown"),
        case("launch pending on an idle agent", json!({"prompt_pending": true, "last_state": "idle", "last_state_change": ago(500)}), agent("idle"), true, "Working", "idle"),
        case("open and approved pull request", json!({"report_hash": "h", "pr_state": "OPEN", "pr_review": "APPROVED"}), agent("idle"), true, "Landing", "idle"),
        case("open pull request with changes requested", json!({"report_hash": "h", "pr_state": "OPEN", "pr_review": "CHANGES_REQUESTED"}), agent("idle"), true, "Ready for review", "idle"),
        case("unacknowledged report", json!({"report_hash": "h"}), agent("done"), true, "Ready for review", "done"),
        case("acknowledged report", json!({"report_hash": "h", "acked_report_hash": "h"}), agent("done"), true, "Idle", "done"),
        case("acknowledged report with an open pull request", json!({"report_hash": "h", "acked_report_hash": "h", "pr_state": "OPEN"}), agent("done"), true, "Ready for review", "done"),
        case("idle without a report", json!({}), agent("idle"), true, "Idle", "idle"),
        case("working beats a new report", json!({"report_hash": "h"}), agent("working"), true, "Working", "working"),
        case("long block beats an approved pull request", json!({"report_hash": "h", "pr_state": "OPEN", "pr_review": "APPROVED", "last_state": "blocked", "last_state_change": ago(120)}), agent("blocked"), true, "Waiting on you", "blocked"),
        case("pane closed with an unread report", json!({"report_hash": "h"}), None, false, "Ready for review", "pane closed"),
        case("pane closed with a read report", json!({"report_hash": "h", "acked_report_hash": "h"}), None, false, "Idle", "pane closed"),
        case("someone else's agent in the recorded pane", blocked_for(120), Some(json!({"agent_status": "blocked", "name": "hp-demo-t-9999"})), true, "Waiting on you", "pane closed"),
        case("same agent name in another directory", json!({}), Some(json!({"agent_status": "working", "cwd": "/elsewhere"})), false, "Waiting on you", "pane closed"),
        case("adopted agent matched without its name", json!({"kind": "adopted", "agent_name": ""}), Some(json!({"agent_status": "idle", "name": "renamed"})), true, "Idle", "idle"),
        case("adopted agent in another directory", json!({"kind": "adopted", "agent_name": ""}), Some(json!({"agent_status": "working", "name": "renamed", "cwd": "/elsewhere"})), false, "Waiting on you", "pane closed"),
        case("recorded idle, now blocked: the block just began", json!({"last_state": "idle", "last_state_change": ago(120)}), agent("blocked"), true, "Working", "blocked"),
    ];
    let (mut agents, mut panes) = (Vec::new(), Vec::new());
    for (n, c) in cases.iter().enumerate() {
        let (id, ws) = (format!("t-{:04}", n + 1), format!("w{}", n + 1));
        let ids = json!({"workspace_id": ws, "tab_id": format!("{ws}:t1"), "pane_id": format!("{ws}:p1"), "cwd": format!("/wt/{id}")});
        let mut record = json!({"id": id, "title": c.title, "status": "open", "kind": "worktree", "created": ago(3600), "agent": "claude", "agent_name": format!("hp-demo-{id}")});
        for source in [&ids, &c.record] { for (k, v) in source.as_object().unwrap() { record[k] = v.clone(); } }
        lab.write_record(&id, &record);
        if c.pane { panes.push(ids.clone()); }
        if let Some(extra) = &c.agent {
            let mut agent = ids.clone();
            agent["agent"] = json!("claude");
            agent["name"] = json!(format!("hp-demo-{id}"));
            for (k, v) in extra.as_object().unwrap() { agent[k] = v.clone(); }
            agents.push(agent);
        }
    }
    lab.session(&agents, &panes);
    let before = fs::read_dir(lab.project().join("threads")).unwrap().map(|e| fs::read(e.unwrap().path()).unwrap()).collect::<Vec<_>>();
    let listed = lab.list();
    let mismatches: Vec<String> = cases.iter().enumerate().filter_map(|(n, c)| {
        let got = &listed[&format!("t-{:04}", n + 1)];
        (got.0 != c.group || got.1 != c.note).then(|| format!("{}: expected {}/{}, got {}/{}", c.title, c.group, c.note, got.0, got.1))
    }).collect();
    assert!(mismatches.is_empty(), "{mismatches:#?}");
    assert_eq!(listed.len(), cases.len());
    // Listing observes; it never rewrites a record.
    let after = fs::read_dir(lab.project().join("threads")).unwrap().map(|e| fs::read(e.unwrap().path()).unwrap()).collect::<Vec<_>>();
    assert_eq!(before, after);
}

/// A finished thread: its report is read, acknowledged, changed again, then
/// the thread is resolved with a final copy that skips a symbolic link.
#[test]
fn report_review_ack_and_resolve_copy_home() {
    let lab = Lab::new();
    let work = lab.path("work");
    let dir = work.join(".herdr-project/demo-t-0001");
    fs::create_dir_all(dir.join("library")).unwrap();
    fs::write(dir.join("report.md"), "## Report\nok\n").unwrap();
    fs::write(dir.join("library/out.txt"), "data").unwrap();
    let ids = json!({"workspace_id": "w0", "tab_id": "w0:t2", "pane_id": "w0:p2", "cwd": work});
    let mut record = json!({"id": "t-0001", "title": "Summarise", "status": "open", "kind": "tab", "created": ago(3600), "agent": "claude",
        "agent_name": "hp-demo-t-0001", "thread_dir": dir, "report_hash": sha(b"## Report\nok\n"), "last_state": "idle", "last_state_change": ago(60)});
    for (k, v) in ids.as_object().unwrap() { record[k] = v.clone(); }
    lab.write_record("t-0001", &record);
    let mut agent = ids.clone();
    agent["name"] = json!("hp-demo-t-0001");
    agent["agent_status"] = json!("idle");
    lab.session(&[agent], &[ids]);

    assert_eq!(lab.list()["t-0001"].0, "Ready for review");
    assert_eq!(lab.ok(&["thread", "ack", "demo", "t-0001"]), "t-0001: report acknowledged\n");
    assert_eq!(lab.list()["t-0001"].0, "Idle");
    // The acknowledgement is one field of an otherwise untouched record, written atomically.
    let acked = lab.record("t-0001");
    assert_eq!(acked["acked_report_hash"].as_str(), Some(sha(b"## Report\nok\n").as_str()));
    for field in ["title", "pane_id", "thread_dir", "agent_name", "last_state", "last_state_change"] {
        assert_eq!(acked[field], toml::Value::try_from(&record[field]).unwrap(), "{field}");
    }
    let leftovers: Vec<_> = fs::read_dir(lab.project().join("threads")).unwrap().flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp")).map(|e| e.file_name()).collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    // A changed report is unread again, even before the ticker hashes it.
    fs::write(dir.join("report.md"), "## Report\nrevised\n").unwrap();
    assert_eq!(lab.list()["t-0001"].0, "Ready for review");

    std::os::unix::fs::symlink("/etc/passwd", dir.join("library/link")).unwrap();
    let out = lab.ok(&["thread", "resolve", "demo", "t-0001"]);
    assert!(out.contains("the final copy was partial:") && out.contains("t-0001 resolved."), "{out}");
    assert_eq!(fs::read_to_string(lab.project().join("threads/t-0001.md")).unwrap(), "## Report\nrevised\n");
    assert_eq!(fs::read_to_string(lab.project().join("library/t-0001/out.txt")).unwrap(), "data");
    assert!(fs::symlink_metadata(lab.project().join("library/t-0001/link")).is_err());
    let resolved = lab.record("t-0001");
    assert_eq!(resolved["report_hash"].as_str(), Some(sha(b"## Report\nrevised\n").as_str()));
    assert_eq!(resolved["status"].as_str(), Some("resolved"));
    assert_eq!(lab.list()["t-0001"], ("Resolved".to_string(), "manual".to_string()));
    // Resolving takes the thread's labels off its pane.
    let clear = "pane report-metadata w0:p2 --source herdr-projects --clear-token project --clear-token thread --clear-token review --clear-token rank";
    assert!(lab.calls().lines().any(|l| l == clear), "{}", lab.calls());
}

struct Ticker(std::process::Child);
impl Drop for Ticker { fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); } }

/// `thread start`, a failed placement, `thread restart` and `thread adopt`:
/// ids, branch names, thread directories, briefs and the launch line.
#[test]
fn start_restart_and_adopt_write_briefs_branches_and_launch_line() {
    let lab = Lab::new();
    let project = lab.project();
    let md = fs::read_to_string(project.join("PROJECT.md")).unwrap();
    let front = md.split("\n+++\n").next().unwrap();
    fs::write(project.join("PROJECT.md"), format!("{front}\n+++\n\n# Instructions\n\nAlways run the tests.\n")).unwrap();
    fs::write(project.join("MEMORY.md"), "# Memory\n- a\n- b\n- c\n").unwrap();
    fs::write(project.join("memory/a.md"), "alpha fact").unwrap();
    fs::write(project.join("memory/b.md"), "x".repeat(32_000)).unwrap();
    fs::write(project.join("memory/c.md"), "gamma fact").unwrap();
    let repo = lab.path("repo");
    fs::create_dir(&repo).unwrap();
    lab.git(&repo, &["init", "-q", "-b", "main"]);
    lab.git(&repo, &["commit", "-q", "--allow-empty", "-m", "base"]);
    fs::write(lab.path("task.md"), "Do the thing.").unwrap();
    let task = lab.path("task.md");
    let (repo_arg, task_arg) = (repo.to_str().unwrap(), task.to_str().unwrap());

    // A running ticker, so the commands hand it the launch rather than spawning one.
    let _ticker = lab.ticker();

    fs::write(lab.path("refuse-worktree"), "").unwrap();
    let failed = lab.cli(&["thread", "start", "demo", "--title", "Fix the $(login) bug!", "--repo", repo_arg, "--task-file", task_arg]);
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("thread t-0001 failed to start; `thread restart demo t-0001` retries"));
    assert_eq!(lab.record("t-0001")["status"].as_str(), Some("failed"));

    let out = lab.ok_beside_ticker(&["thread", "restart", "demo", "t-0001"]);
    assert!(out.starts_with("t-0001 is back in pane w1:p1"), "{out}");
    let first = lab.record("t-0001");
    let branch = "hp/demo/t-0001-fix-the-login-bug";
    assert_eq!(first["branch"].as_str(), Some(branch));
    assert!(lab.git(&repo, &["branch", "--list", branch]).contains(branch));
    let first_cwd = first["cwd"].as_str().unwrap();
    assert_eq!(first["thread_dir"].as_str().unwrap(), format!("{first_cwd}/.herdr-project/demo-t-0001"));
    assert_eq!(first["agent_name"].as_str(), Some("hp-demo-t-0001"));
    let restarted = fs::read_to_string(Path::new(first["thread_dir"].as_str().unwrap()).join("brief.md")).unwrap();

    let out = lab.ok_beside_ticker(&["thread", "start", "demo", "--title", "???", "--repo", repo_arg, "--task-file", task_arg]);
    let started: Value = serde_json::from_str(&out).unwrap();
    assert_eq!((started["id"].as_str(), started["branch"].as_str()), (Some("t-0002"), Some("hp/demo/t-0002")));
    let fresh = fs::read_to_string(Path::new(lab.record("t-0002")["thread_dir"].as_str().unwrap()).join("brief.md")).unwrap();

    for brief in [&restarted, &fresh] {
        let pos = |needle: &str| brief.find(needle).unwrap_or_else(|| panic!("missing {needle}"));
        assert!(pos("# Thread brief") < pos("Always run the tests."));
        assert!(pos("Always run the tests.") < pos("# Memory"));
        assert!(pos("# Memory") < pos("alpha fact"));
        assert!(pos("alpha fact") < pos("gamma fact"));
        assert!(pos("gamma fact") < pos("Do the thing."));
        assert!(pos("Do the thing.") < pos(".herdr-project/demo-t-000"));
        assert!(brief.contains("Not inlined because project memory is over 32000 characters: memory/b.md."));
        assert!(!brief.contains(&"x".repeat(100)));
    }
    let resumed = restarted.find("previous attempt").expect("restart brief names the previous attempt");
    assert!(restarted.find("# Thread brief").unwrap() < resumed && resumed < restarted.find("Always run the tests.").unwrap());
    assert!(!fresh.contains("previous attempt"));
    // Both placements share the repository's exclude file: one line, and the
    // thread directories never show up as untracked files.
    assert_eq!(fs::read_to_string(repo.join(".git/info/exclude")).unwrap().lines().filter(|l| *l == ".herdr-project/").count(), 1);
    for cwd in [first_cwd, lab.record("t-0002")["cwd"].as_str().unwrap()] {
        assert!(Path::new(cwd).join(".herdr-project").is_dir());
        assert_eq!(lab.git(Path::new(cwd), &["status", "--porcelain", "--untracked-files=all"]), "", "{cwd}");
    }
    // Placement labels the pane for the sidebar: this project, this thread, working.
    let calls = lab.calls();
    for (pane, id) in [("w1:p1", "t-0001"), ("w2:p1", "t-0002")] {
        let line = format!("pane report-metadata {pane} --source herdr-projects --ttl-ms 300000 --token project=demo --token thread={id} --token review=working --token rank=3");
        assert!(calls.lines().any(|l| l == line), "{line}\n{calls}");
    }

    // Adopting a ready agent sends it the one launch line, naming its own brief.
    let adopted_cwd = lab.path("elsewhere");
    fs::create_dir(&adopted_cwd).unwrap();
    let pane = json!({"workspace_id": "w7", "tab_id": "w7:t1", "pane_id": "w7:p1", "cwd": adopted_cwd});
    let mut agent = pane.clone();
    agent["name"] = json!("someone");
    agent["agent"] = json!("claude");
    agent["agent_status"] = json!("idle");
    lab.session(&[agent], &[pane]);
    let adopted: Value = serde_json::from_str(&lab.ok(&["thread", "adopt", "demo", "--pane", "w7:p1", "--title", "Adopted"])).unwrap();
    assert_eq!((adopted["id"].as_str(), adopted["prompt_pending"].as_bool()), (Some("t-0003"), Some(false)));
    let calls = fs::read_to_string(lab.path("herdr-calls")).unwrap();
    assert!(calls.lines().any(|l| l == "agent prompt w7:p1 Read .herdr-project/demo-t-0003/brief.md and do what it says."), "{calls}");
    assert!(adopted_cwd.join(".herdr-project/demo-t-0003/brief.md").is_file());

    // Ids have one form; a well-formed but unknown id is looked up, a malformed one is refused.
    for bad in ["t-1", "t-00a1", "../t-0001", "x-0001"] {
        let out = lab.cli(&["thread", "show", "demo", bad]);
        assert!(String::from_utf8_lossy(&out.stderr).contains("is not a thread id"), "{bad}");
    }
    let out = lab.cli(&["thread", "show", "demo", "t-12345"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("no thread `t-12345`"));
}

/// `thread restart` acts on what the record shows was reached and what herdr
/// shows now: refusals leave every record and herdr untouched; the rest reuse
/// a shell pane, reopen the worktree or tab, or create the placement again.
#[test]
fn restart_follows_what_the_record_reached() {
    let lab = Lab::new();
    let repo = lab.path("repo");
    fs::create_dir(&repo).unwrap();
    lab.git(&repo, &["init", "-q", "-b", "main"]);
    lab.git(&repo, &["commit", "-q", "--allow-empty", "-m", "base"]);
    lab.git(&repo, &["branch", "hp/demo/t-0006-half-made"]);
    let (mut agents, mut panes) = (Vec::new(), Vec::new());
    let mut thread = |n: usize, fields: Value, pane: bool, agent: Option<&str>| {
        let (id, ws) = (format!("t-{n:04}"), format!("w{n}"));
        let cwd = lab.path(&format!("wt{n}"));
        let ids = json!({"workspace_id": ws, "tab_id": format!("{ws}:t1"), "pane_id": format!("{ws}:p1"), "cwd": cwd});
        let mut record = json!({"id": id, "title": "Half made", "status": "open", "kind": "worktree", "created": ago(3600), "agent": "claude",
            "agent_name": format!("hp-demo-{id}"), "repo": repo, "worktree_path": cwd});
        for source in [&ids, &fields] { for (k, v) in source.as_object().unwrap() { record[k] = v.clone(); } }
        lab.write_record(&id, &record);
        if pane { panes.push(ids.clone()); }
        if let Some(status) = agent {
            let mut agent = ids;
            agent["agent"] = json!("claude");
            agent["name"] = json!(format!("hp-demo-{id}"));
            agent["agent_status"] = json!(status);
            agents.push(agent);
        }
    };
    let refusals = [
        (1, "t-0001 is running: its pane has an agent in it"),
        (2, "t-0002 is being launched by the ticker (attempt 1 of 3)"),
        (3, "an adopted thread cannot be restarted"),
        (4, "t-0004 is resolved; `thread resolve --reopen` first"),
        (5, "t-0005 is still starting"),
        (6, "t-0006: no worktree was recorded but its branch already exists"),
    ];
    thread(1, json!({}), true, Some("working"));
    thread(2, json!({"prompt_pending": true, "launch_attempts": 1}), true, None);
    thread(3, json!({"kind": "adopted"}), false, None);
    thread(4, json!({"status": "resolved", "resolved_reason": "manual"}), false, None);
    thread(5, json!({"status": "starting", "created": ago(60), "worktree_path": ""}), false, None);
    thread(6, json!({"status": "failed", "error": "boom", "worktree_path": ""}), false, None);
    // Reuses the pane at its shell prompt; a launch that ran out of attempts is restartable.
    thread(7, json!({}), true, None);
    thread(8, json!({"prompt_pending": true, "launch_attempts": 3}), true, None);
    // Reopens: the worktree through herdr, the tab in the project's `threads/<id>/`.
    thread(9, json!({}), false, None);
    thread(10, json!({"kind": "tab", "worktree_path": ""}), false, None);
    // A start that stalled before creating anything is created again.
    thread(11, json!({"status": "starting", "worktree_path": ""}), false, None);
    lab.session(&agents, &panes);

    let records = || fs::read_dir(lab.project().join("threads")).unwrap().map(|e| fs::read(e.unwrap().path()).unwrap()).collect::<Vec<_>>();
    let before = records();
    for (n, error) in refusals {
        let stderr = String::from_utf8(lab.cli(&["thread", "restart", "demo", &format!("t-{n:04}")]).stderr).unwrap();
        assert!(stderr.contains(error), "t-{n:04}: {stderr}");
    }
    assert_eq!(records(), before, "a refused restart changed a record");
    assert!(lab.calls().lines().all(|l| l == "agent list" || l == "pane list"), "{}", lab.calls());

    let _ticker = lab.ticker();
    let restart = |n: usize| lab.ok_beside_ticker(&["thread", "restart", "demo", &format!("t-{n:04}")]);
    assert_eq!(restart(7), "t-0007 is back in pane w7:p1; the ticker launches its agent\n");
    assert_eq!(restart(8), "t-0008 is back in pane w8:p1; the ticker launches its agent\n");
    assert!(!lab.calls().contains("worktree ") && !lab.calls().contains("tab create"), "{}", lab.calls());
    for n in [7, 8] {
        let record = lab.record(&format!("t-{n:04}"));
        let cwd = lab.path(&format!("wt{n}"));
        assert_eq!(record["thread_dir"].as_str().unwrap(), cwd.join(format!(".herdr-project/demo-t-{n:04}")).to_str().unwrap());
        assert!(cwd.join(format!(".herdr-project/demo-t-{n:04}/brief.md")).is_file());
    }

    assert_eq!(restart(9), "t-0009 is back in pane w90:p1; the ticker launches its agent\n");
    let open = format!("worktree open --cwd {} --path {} --label Half made --no-focus", repo.display(), lab.path("wt9").display());
    assert!(lab.calls().lines().any(|l| l == open), "{}", lab.calls());
    assert_eq!(lab.record("t-0009")["pane_id"].as_str(), Some("w90:p1"));

    assert_eq!(restart(10), "t-0010 is back in pane w0:p9; the ticker launches its agent\n");
    let folder = fs::canonicalize(lab.project().join("threads/t-0010")).unwrap();
    let tab = format!("tab create --workspace w0 --cwd {} --label Half made --no-focus", folder.display());
    assert!(lab.calls().lines().any(|l| l == tab), "{}", lab.calls());
    assert!(folder.join(".herdr-project/demo-t-0010/brief.md").is_file());

    let out = restart(11);
    assert!(out.starts_with("t-0011 is back in pane w") && !out.contains("w11:p1"), "{out}");
    assert!(lab.git(&repo, &["branch", "--list", "hp/demo/t-0011-half-made"]).contains("hp/demo/t-0011-half-made"));
    assert_eq!(lab.record("t-0011")["branch"].as_str(), Some("hp/demo/t-0011-half-made"));
}

/// `thread prompt` sends only to a detected agent that is not waiting on the
/// user, and queues text for one that is working.
#[test]
fn prompt_refuses_a_bare_shell_a_blocked_or_unknown_agent_and_sends_otherwise() {
    let lab = Lab::new();
    let ids = json!({"workspace_id": "w1", "tab_id": "w1:t1", "pane_id": "w1:p1", "cwd": lab.path("work")});
    let mut record = json!({"id": "t-0001", "title": "Adopted", "status": "open", "kind": "adopted", "created": ago(3600), "agent": "claude", "agent_name": ""});
    for (k, v) in ids.as_object().unwrap() { record[k] = v.clone(); }
    lab.write_record("t-0001", &record);
    fs::write(lab.path("text"), "  Also update the changelog.\n").unwrap();
    let text = lab.path("text");
    let prompt = |status: Option<&str>| {
        let agents: Vec<Value> = status.map(|s| { let mut a = ids.clone(); a["name"] = json!("someone"); a["agent_status"] = json!(s); a }).into_iter().collect();
        lab.session(&agents, std::slice::from_ref(&ids));
        lab.cli(&["thread", "prompt", "demo", "t-0001", "--text-file", text.to_str().unwrap()])
    };
    for (status, error) in [(None, "text is never typed at a bare shell prompt"), (Some("unknown"), "t-0001's agent state is unknown; not sending"),
                            (Some("blocked"), "agent_blocked: t-0001 is waiting on the user in its pane (w1:p1)")] {
        let out = prompt(status);
        assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains(error), "{status:?}: {}", String::from_utf8_lossy(&out.stderr));
    }
    assert!(!lab.calls().contains("agent prompt"), "{}", lab.calls());
    for status in ["working", "idle"] {
        let out = prompt(Some(status));
        assert_eq!(String::from_utf8(out.stdout).unwrap(), format!("sent to t-0001 (agent was {status})\n"));
    }
    let sent = lab.calls().lines().filter(|l| *l == "agent prompt w1:p1 Also update the changelog.").count();
    assert_eq!(sent, 2, "{}", lab.calls());
}

/// `open --reprime` on a running coordinator asks for one more priming and
/// keeps every other field of the coordinator record.
#[test]
fn reprime_updates_only_the_priming_fields_of_the_coordinator_record() {
    let lab = Lab::new();
    let state = lab.project().join(".state/coordinator.json");
    let before: Value = serde_json::from_str(&fs::read_to_string(&state).unwrap()).unwrap();
    let agent = json!({"workspace_id": "w0", "tab_id": "w0:t1", "pane_id": "w0:p1", "cwd": lab.project(), "name": "coordinator", "agent": "claude", "agent_status": "working"});
    lab.session(std::slice::from_ref(&agent), &[]);
    let _ticker = lab.ticker();
    let socket = lab.path("session.sock");
    for round in 1..=2 {
        let out = lab.ok_beside_ticker(&["open", "demo", "--reprime", "--socket", socket.to_str().unwrap()]);
        assert!(out.starts_with("coordinator is running in pane w0:p1\n"), "{out}");
        let after: Value = serde_json::from_str(&fs::read_to_string(&state).unwrap()).unwrap();
        for field in ["socket", "workspace_id", "tab_id", "pane_id", "agent_name", "cwd"] {
            assert_eq!(after[field], before[field], "{field}");
        }
        assert_eq!((after["prime_request"].as_u64(), after["prime_pending"].as_bool()), (Some(round), Some(true)));
    }
    let leftovers: Vec<_> = fs::read_dir(lab.project().join(".state")).unwrap().flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp")).map(|e| e.file_name()).collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

/// Replaces `empty_defaults_allow_mixed_kinds_but_flags_require_exact_binding`.
///
/// Without `thread_agent_args` any valid kind starts a thread. Once the
/// owner lists arguments, `thread start` refuses them until they are bound to
/// a kind, and then for every other kind, as well as a malformed kind or an
/// argument with a NUL, before anything is recorded or created in herdr.
#[test]
fn thread_start_uses_agent_arguments_only_for_the_kind_they_are_bound_to() {
    let lab = Lab::new();
    let config = lab.path(".config/herdr-projects/config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    let safety = format!("[safety.{:?}]\n", lab.project().canonicalize().unwrap().display().to_string());
    fs::write(lab.path("task.md"), "Do the thing.").unwrap();
    let task = lab.path("task.md");
    let _ticker = lab.ticker();
    let start = |kind: &str| lab.cli(&["thread", "start", "demo", "--title", kind, "--agent", kind, "--task-file", task.to_str().unwrap()]);
    let refused = |kind: &str| -> String {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let out = start(kind);
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            assert!(!out.status.success(), "{kind} started");
            if !stderr.contains("another operation owns lock") { return stderr; }
            assert!(Instant::now() < deadline, "{stderr}");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let threads = || fs::read_dir(lab.project().join("threads")).unwrap().flatten().filter(|e| e.path().extension().is_some_and(|x| x == "toml")).count();

    for (args, kind, error) in [
        ("thread_agent_args=['--vendor-option']\n", "claude", "thread_agent_args is not bound to an agent kind"),
        ("thread_agent_args=['--vendor-option']\nthread_agent_args_kind='claude'\n", "codex", "thread_agent_args belongs to agent kind `claude`, not requested kind `codex`"),
        ("thread_agent_args=[\"bad\\u0000argument\"]\nthread_agent_args_kind='claude'\n", "claude", "exceeds argument limits or contains NUL"),
        ("", "9bad", "invalid agent kind identifier"),
    ] {
        fs::write(&config, format!("{safety}{args}")).unwrap();
        let stderr = refused(kind);
        assert!(stderr.contains(error), "{kind}: {stderr}");
        assert_eq!(threads(), 0);
        assert!(!lab.calls().contains("tab create"), "{}", lab.calls());
    }

    fs::write(&config, format!("{safety}thread_agent_args=['--vendor-option']\nthread_agent_args_kind='claude'\n")).unwrap();
    lab.ok_beside_ticker(&["thread", "start", "demo", "--title", "claude", "--agent", "claude", "--task-file", task.to_str().unwrap()]);
    assert_eq!(lab.record("t-0001")["agent"].as_str(), Some("claude"));
    fs::write(&config, &safety).unwrap();
    for kind in ["codex", "muse"] {
        lab.ok_beside_ticker(&["thread", "start", "demo", "--title", kind, "--agent", kind, "--task-file", task.to_str().unwrap()]);
    }
    assert_eq!((lab.record("t-0002")["agent"].as_str(), lab.record("t-0003")["agent"].as_str()), (Some("codex"), Some("muse")));
}

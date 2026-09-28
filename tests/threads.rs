#![cfg(target_os = "linux")]
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
    let _ticker = Ticker(Command::new(BIN).env_clear().env("HOME", lab.home.path()).env("PATH", "/usr/bin:/bin").env("HERDR_BIN_PATH", &lab.fake)
        .args(["--root", lab.root().to_str().unwrap(), "ticker", "run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(20);
    while !lab.ok(&["ticker", "status"]).contains("ticker: running") {
        assert!(Instant::now() < deadline, "ticker never took its lock");
        std::thread::sleep(Duration::from_millis(20));
    }

    fs::write(lab.path("refuse-worktree"), "").unwrap();
    let failed = lab.cli(&["thread", "start", "demo", "--title", "Fix the $(login) bug!", "--repo", repo_arg, "--task-file", task_arg]);
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("thread t-0001 failed to start; `thread restart demo t-0001` retries"));
    assert_eq!(lab.record("t-0001")["status"].as_str(), Some("failed"));

    let out = lab.ok(&["thread", "restart", "demo", "t-0001"]);
    assert!(out.starts_with("t-0001 is back in pane w1:p1"), "{out}");
    let first = lab.record("t-0001");
    let branch = "hp/demo/t-0001-fix-the-login-bug";
    assert_eq!(first["branch"].as_str(), Some(branch));
    assert!(lab.git(&repo, &["branch", "--list", branch]).contains(branch));
    let first_cwd = first["cwd"].as_str().unwrap();
    assert_eq!(first["thread_dir"].as_str().unwrap(), format!("{first_cwd}/.herdr-project/demo-t-0001"));
    assert_eq!(first["agent_name"].as_str(), Some("hp-demo-t-0001"));
    let restarted = fs::read_to_string(Path::new(first["thread_dir"].as_str().unwrap()).join("brief.md")).unwrap();

    let out = lab.ok(&["thread", "start", "demo", "--title", "???", "--repo", repo_arg, "--task-file", task_arg]);
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
    assert!(fs::read_to_string(repo.join(".git/info/exclude")).unwrap().lines().any(|l| l == ".herdr-project/"));

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

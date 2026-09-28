#![cfg(target_os = "linux")]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! Worktree cleanup and reopening through the compiled CLI: `thread resolve
//! --remove-worktree --writers-stopped` removes a worktree thread's worktree
//! (keeping its branch), and `thread resolve --reopen` then `thread restart`
//! adds it back. Both refuse while anything else references the path. git is
//! the real tool; herdr is a fake that lists no pane of the thread.
use serde_json::{json, Value};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, process::{Command, Output, Stdio}, time::{Duration, Instant}};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-projects");

const FAKE_HERDR: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$HOME/herdr-calls"
case "$1 $2" in
'agent list') echo '{"result":{"agents":[]}}';;
'pane list') cat "$HOME/panes.json";;
'worktree open') printf '{"result":{"root_pane":{"workspace_id":"w90","tab_id":"w90:t1","pane_id":"w90:p1","cwd":"%s"},"worktree":{"path":"%s"}}}\n' "$6" "$6";;
*) echo '{"result":{}}';;
esac
"#;

struct Lab { home: tempfile::TempDir, _listener: std::os::unix::net::UnixListener }

impl Lab {
    /// Project `demo` with worktree thread `t-0001` on branch `retained`,
    /// whose workspace is already closed, and whose report is in the worktree.
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let listener = std::os::unix::net::UnixListener::bind(home.path().join("session.sock")).unwrap();
        let lab = Lab { home, _listener: listener };
        fs::write(lab.path("herdr"), FAKE_HERDR).unwrap();
        fs::set_permissions(lab.path("herdr"), fs::Permissions::from_mode(0o700)).unwrap();
        lab.ok(&["new", "demo"]);
        fs::write(lab.project("demo").join(".state/coordinator.json"), json!({"socket": lab.path("session.sock"), "workspace_id": "w0", "tab_id": "w0:t1",
            "pane_id": "w0:p1", "agent_name": "coordinator", "cwd": lab.project("demo")}).to_string()).unwrap();
        fs::write(lab.path("panes.json"), json!({"result": {"panes": [{"workspace_id": "w0", "tab_id": "w0:t1", "pane_id": "w0:p1", "cwd": lab.project("demo")}]}}).to_string()).unwrap();
        let repo = lab.path("repo");
        fs::create_dir(&repo).unwrap();
        lab.git(&["init", "-q", "-b", "main"]);
        lab.git(&["commit", "-q", "--allow-empty", "-m", "base"]);
        lab.git(&["worktree", "add", "-q", "-b", "retained", lab.work().to_str().unwrap()]);
        fs::write(repo.join(".git/info/exclude"), ".herdr-project/\n").unwrap();
        let dir = lab.work().join(".herdr-project/demo-t-0001");
        fs::create_dir_all(dir.join("library")).unwrap();
        fs::write(dir.join("report.md"), "retained report\n").unwrap();
        lab.write_record("demo", "t-0001", json!({"id": "t-0001", "title": "Retained", "status": "open", "kind": "worktree", "created": jiff::Timestamp::now().to_string(),
            "agent": "claude", "agent_name": "hp-demo-t-0001", "repo": repo, "branch": "retained", "worktree_path": lab.work(), "cwd": lab.work(), "thread_dir": dir,
            "workspace_id": "w1", "tab_id": "w1:t1", "pane_id": "w1:p1"}));
        lab
    }
    fn path(&self, name: &str) -> PathBuf { self.home.path().join(name) }
    fn root(&self) -> PathBuf { self.path("root") }
    fn project(&self, slug: &str) -> PathBuf { self.root().join(slug) }
    fn work(&self) -> PathBuf { self.path("worktree") }
    fn cli(&self, args: &[&str]) -> Output {
        Command::new(BIN).env_clear().env("HOME", self.home.path()).env("PATH", "/usr/bin:/bin").env("HERDR_BIN_PATH", self.path("herdr"))
            .arg("--root").arg(self.root()).args(args).output().unwrap()
    }
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
    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("/usr/bin/git").env_clear().env("PATH", "/usr/bin:/bin").env("HOME", self.home.path())
            .env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "fixture").env("GIT_AUTHOR_EMAIL", "fixture@example.com")
            .env("GIT_COMMITTER_NAME", "fixture").env("GIT_COMMITTER_EMAIL", "fixture@example.com")
            .arg("-C").arg(self.path("repo")).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }
    fn registered(&self) -> bool { self.git(&["worktree", "list", "--porcelain"]).lines().any(|l| l == format!("worktree {}", self.work().display())) }
    fn write_record(&self, slug: &str, id: &str, record: Value) {
        fs::write(self.project(slug).join(format!("threads/{id}.toml")), toml::to_string(&record).unwrap()).unwrap();
    }
    fn record(&self) -> toml::Value { toml::from_str(&fs::read_to_string(self.project("demo").join("threads/t-0001.toml")).unwrap()).unwrap() }
    fn remove(&self) -> String { self.ok(&["thread", "resolve", "demo", "t-0001", "--remove-worktree", "--writers-stopped"]) }
    /// The removed thread is reopened and restarted; returns the restart's stderr when refused.
    fn reopen(&self) -> Result<String, String> {
        self.ok(&["thread", "resolve", "demo", "t-0001", "--reopen"]);
        let _ticker = self.ticker();
        loop {
            let out = self.cli(&["thread", "restart", "demo", "t-0001"]);
            let stderr = String::from_utf8(out.stderr).unwrap();
            if out.status.success() { return Ok(String::from_utf8(out.stdout).unwrap()); }
            if !stderr.contains("another operation owns lock") { return Err(stderr); }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    /// A foreground `ticker run`, so a restart hands it the launch rather
    /// than spawning a detached one.
    fn ticker(&self) -> Ticker {
        let spawn = || Command::new(BIN).env_clear().env("HOME", self.home.path()).env("PATH", "/usr/bin:/bin").env("HERDR_BIN_PATH", self.path("herdr"))
            .arg("--root").arg(self.root()).args(["ticker", "run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        let mut ticker = Ticker(spawn());
        let deadline = Instant::now() + Duration::from_secs(20);
        while !String::from_utf8_lossy(&self.cli(&["ticker", "status"]).stdout).contains("ticker: running") {
            assert!(Instant::now() < deadline, "ticker never took its lock");
            if ticker.0.try_wait().unwrap().is_some() { ticker = Ticker(spawn()); }
            std::thread::sleep(Duration::from_millis(20));
        }
        ticker
    }
    /// The removed worktree is still gone, unregistered, and its removal
    /// record and preserved report are intact.
    fn still_removed(&self, removal: &toml::Value, what: &str) {
        assert!(fs::symlink_metadata(self.work()).map_or(true, |m| !m.is_dir()), "{what}: the worktree came back");
        assert!(!self.registered(), "{what}: the worktree is registered again");
        assert_eq!(self.record().get("removal"), Some(removal), "{what}");
        let snapshot = self.record()["artifact_snapshot"].as_str().unwrap().to_owned();
        assert_eq!(fs::read_to_string(self.project("demo").join(".state/artifacts/t-0001").join(snapshot).join("report.md")).unwrap(), "retained report\n", "{what}");
    }
}

struct Ticker(std::process::Child);
impl Drop for Ticker { fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); } }

/// Replaces `reopen_refuses_changed_branch_replaced_path_and_new_owner`.
///
/// After the worktree is removed, reopening it adds it back from the kept
/// branch at the recorded commit. It is refused, and nothing is added, when
/// the branch has advanced, when something else now sits at the path, or
/// when another thread has since taken the path as its directory.
#[test]
fn reopen_restores_the_removed_worktree_unless_its_branch_path_or_owner_changed() {
    let lab = Lab::new();
    let out = lab.remove();
    assert!(out.contains(&format!("The worktree {} was removed; the branch retained was kept.", lab.work().display())), "{out}");
    assert!(!lab.work().exists() && !lab.registered());
    let head = lab.git(&["rev-parse", "retained"]);
    let restarted = lab.reopen().unwrap();
    assert!(restarted.starts_with("t-0001 is back in pane w90:p1"), "{restarted}");
    assert!(lab.work().is_dir() && lab.registered());
    assert_eq!(lab.git(&["rev-parse", "retained"]), head);
    assert_eq!((lab.record().get("removal"), lab.record()["worktree_path"].as_str()), (None, lab.work().to_str()));

    for (change, error) in [("branch", "retained branch advanced; inspect before reopening"), ("path", if cfg!(feature = "state-store") { "cleanup reference contains a dangling alias" } else { "reopen path is not a real directory" }),
                            ("owner", if cfg!(feature = "state-store") { "resource is already referenced by demo/thread:t-0002" } else { "reopen worktree is referenced by t-0002 in demo" })] {
        let lab = Lab::new();
        lab.remove();
        let removal = lab.record()["removal"].clone();
        match change {
            "branch" => { lab.git(&["commit", "-q", "--allow-empty", "-m", "advance"]); lab.git(&["update-ref", "refs/heads/retained", "HEAD"]); }
            "path" => std::os::unix::fs::symlink(lab.path("missing"), lab.work()).unwrap(),
            _ => lab.write_record("demo", "t-0002", json!({"id": "t-0002", "title": "Owner", "status": "open", "kind": "adopted", "created": jiff::Timestamp::now().to_string(),
                "agent": "claude", "cwd": lab.work(), "workspace_id": "w2", "tab_id": "w2:t1", "pane_id": "w2:p1"})),
        }
        let stderr = lab.reopen().unwrap_err();
        assert!(stderr.contains(error), "{change}: {stderr}");
        lab.still_removed(&removal, change);
    }
}

/// A neighbour project with a canonical store that records `reference` as
/// a runtime binding's working directory.
#[cfg(feature = "state-store")]
fn canonical_neighbour(lab: &Lab, reference: &std::path::Path, corrupt: bool) {
    lab.ok(&["new", "canonical"]);
    lab.ok(&["pause", "canonical"]);
    let plan = lab.path("plan.json");
    lab.ok(&["migration", "canonical", "plan", "--output", plan.to_str().unwrap()]);
    lab.ok(&["migration", "canonical", "apply", "--plan", plan.to_str().unwrap(), "--writers-stopped"]);
    let head: Value = serde_json::from_str(&lab.ok(&["task", "canonical", "list"])).unwrap();
    let route = lab.path("route.json");
    fs::write(&route, json!({"cwd": reference}).to_string()).unwrap();
    lab.ok(&["runtime", "canonical", "create", "--route", route.to_str().unwrap(), "--expected-head", &head["head"].to_string()]);
    if corrupt { fs::write(lab.project("canonical").join(".state/format.json"), b"invalid").unwrap(); }
}

/// Where a neighbour's binding points, relative to the worktree, for `mode`.
#[cfg(feature = "state-store")]
fn reference(lab: &Lab, mode: &str) -> PathBuf {
    let work = lab.work();
    match mode {
        "missing-child" => work.join("not-created"),
        "parent" => lab.home.path().to_path_buf(),
        "alias-child" => { let alias = lab.path("work-alias"); std::os::unix::fs::symlink(&work, &alias).unwrap(); alias.join("not-created") }
        "dangling-alias" => { let alias = lab.path("work-alias"); std::os::unix::fs::symlink(work.join("not-created"), &alias).unwrap(); alias }
        "parent-alias" => { let alias = lab.path("parent-alias"); std::os::unix::fs::symlink(lab.home.path().to_path_buf(), &alias).unwrap(); alias.join("worktree") }
        "unrelated" | "corrupt" => lab.path("elsewhere"),
        _ => work,
    }
}

/// Replaces `canonical_references_block_cleanup_but_unrelated_projects_do_not`.
///
/// A worktree is kept, with its report and without a removal record, while a
/// migrated project's binding names the worktree, a path inside it, a parent
/// of it, a path through an alias of it or a dangling alias, or while that
/// project's store cannot be read. An unrelated binding does not block it.
#[cfg(feature = "state-store")]
#[test]
fn canonical_references_keep_the_worktree_but_an_unrelated_binding_does_not() {
    for mode in ["same", "missing-child", "parent", "alias-child", "dangling-alias", "unrelated", "corrupt"] {
        let lab = Lab::new();
        canonical_neighbour(&lab, &reference(&lab, mode), mode == "corrupt");
        let out = lab.cli(&["thread", "resolve", "demo", "t-0001", "--remove-worktree", "--writers-stopped"]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        if mode == "unrelated" {
            assert!(out.status.success(), "{mode}: {stderr}");
            assert!(!lab.work().exists() && !lab.registered(), "{mode}");
            continue;
        }
        assert!(!out.status.success(), "{mode}: the worktree was removed");
        match mode {
            "dangling-alias" => assert!(stderr.contains("dangling alias"), "{mode}: {stderr}"),
            "corrupt" => {}
            _ => assert!(stderr.contains("resource is already referenced by canonical/"), "{mode}: {stderr}"),
        }
        assert!(lab.work().is_dir() && lab.registered(), "{mode}");
        assert_eq!(fs::read_to_string(lab.work().join(".herdr-project/demo-t-0001/report.md")).unwrap(), "retained report\n");
        assert!(lab.record().get("removal").is_none(), "{mode}");
        assert_eq!(lab.record()["status"].as_str(), Some("open"), "{mode}");
    }
}

/// Replaces `reopen_checks_new_canonical_references_to_the_absent_path`.
///
/// A migrated project that starts referencing the removed worktree's path
/// after the removal (the path itself, a path inside it, the path through an
/// alias of its parent) or whose store cannot be read blocks the reopen; the
/// worktree stays absent and unregistered. An unrelated binding does not.
#[cfg(feature = "state-store")]
#[test]
fn canonical_references_made_after_removal_block_the_reopen() {
    for mode in ["same", "missing-child", "parent-alias", "unrelated", "corrupt"] {
        let lab = Lab::new();
        lab.remove();
        let removal = lab.record()["removal"].clone();
        canonical_neighbour(&lab, &reference(&lab, mode), mode == "corrupt");
        match lab.reopen() {
            Ok(out) => {
                assert_eq!(mode, "unrelated", "{mode}: reopened: {out}");
                assert!(lab.work().is_dir() && lab.registered());
                assert!(lab.record().get("removal").is_none());
            }
            Err(stderr) => {
                assert_ne!(mode, "unrelated", "{stderr}");
                if mode != "corrupt" { assert!(stderr.contains("resource is already referenced by canonical/"), "{mode}: {stderr}"); }
                lab.still_removed(&removal, mode);
            }
        }
    }
}

#![cfg(target_os = "linux")]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! The legacy inbox through the compiled CLI: `ticker run` writes items for
//! thread state changes and due routines, `context` lists them and records
//! what it showed, and `inbox done` moves them to `inbox/done/`. herdr is a
//! shell fixture that serves `agent list` / `pane list` from files.
use serde_json::{json, Value};
use std::{collections::BTreeSet, fs, os::unix::fs::PermissionsExt, path::PathBuf, process::{Command, Output, Stdio}, time::{Duration, Instant}};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-projects");

const FAKE_HERDR: &str = r#"#!/bin/sh
case "$1 $2" in
'agent list') cat "$HOME/agents.json";;
'pane list') cat "$HOME/panes.json";;
*) echo '{"result":{}}';;
esac
"#;

struct Lab { home: tempfile::TempDir, fake: PathBuf, _listener: std::os::unix::net::UnixListener }

impl Lab {
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
        lab
    }
    fn root(&self) -> PathBuf { self.home.path().join("root") }
    fn project(&self) -> PathBuf { self.root().join("demo") }
    fn command(&self) -> Command {
        let mut command = Command::new(BIN);
        command.env_clear().env("HOME", self.home.path()).env("PATH", "/usr/bin:/bin").env("HERDR_BIN_PATH", &self.fake).arg("--root").arg(self.root());
        command
    }
    fn cli(&self, args: &[&str]) -> Output { self.command().args(args).output().unwrap() }
    fn ok(&self, args: &[&str]) -> String {
        let out = self.cli(args);
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }
    /// Unhandled item ids, as file names in `inbox/`.
    fn items(&self) -> Vec<String> {
        let mut ids: Vec<String> = fs::read_dir(self.project().join("inbox")).unwrap().flatten()
            .filter(|e| e.path().is_file()).filter_map(|e| e.file_name().to_str()?.strip_suffix(".md").map(str::to_owned)).collect();
        ids.sort();
        ids
    }
    /// Run a foreground ticker until `done`, then stop it through its stop file.
    fn tick_until(&self, done: impl Fn() -> bool) {
        struct Child(std::process::Child);
        impl Drop for Child { fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); } }
        let mut child = Child(self.command().args(["ticker", "run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
        let deadline = Instant::now() + Duration::from_secs(60);
        while !done() {
            assert!(child.0.try_wait().unwrap().is_none(), "ticker exited");
            assert!(Instant::now() < deadline, "ticker log: {}", fs::read_to_string(self.root().join(".ticker.log")).unwrap_or_default());
            std::thread::sleep(Duration::from_millis(20));
        }
        let stop = self.root().join(".ticker.stop");
        fs::write(&stop, b"").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while child.0.try_wait().unwrap().is_none() { assert!(Instant::now() < deadline, "ticker ignored its stop file"); std::thread::sleep(Duration::from_millis(20)); }
        fs::remove_file(stop).unwrap();
    }
}

/// Replaces `lists_marks_seen_and_moves_to_done`, `two_events_in_one_tick_get_two_items`
/// and the routine-body half of `routine_items_carry_a_body_and_subjects_are_made_file_safe`.
///
/// One ticker pass sees two working threads turn idle and a prompt-only
/// routine fall due: three items, each with its own counter. `context` shows
/// each summary on one line and the routine's prompt as its body, then records
/// the items as seen; `inbox done` handles one item, then the rest.
#[test]
fn ticker_items_are_listed_seen_once_and_moved_to_done() {
    let lab = Lab::new();
    let (mut agents, mut panes) = (Vec::new(), Vec::new());
    for (n, title) in [(1, "Fix\nparser"), (2, "Write docs")] {
        let (id, ws) = (format!("t-000{n}"), format!("w{n}"));
        let ids = json!({"workspace_id": ws, "tab_id": format!("{ws}:t1"), "pane_id": format!("{ws}:p1"), "cwd": format!("/wt/{id}")});
        let mut record = json!({"id": id, "title": title, "status": "open", "kind": "worktree", "created": "2026-01-01T00:00:00Z", "agent": "claude",
            "agent_name": format!("hp-demo-{id}"), "last_state": "working", "last_group": "working", "last_state_change": "2026-01-01T00:00:00Z"});
        for (k, v) in ids.as_object().unwrap() { record[k] = v.clone(); }
        fs::write(lab.project().join(format!("threads/{id}.toml")), toml::to_string(&record).unwrap()).unwrap();
        let mut agent = ids.clone();
        (agent["agent"], agent["name"], agent["agent_status"]) = (json!("claude"), json!(format!("hp-demo-{id}")), json!("idle"));
        panes.push(ids);
        agents.push(agent);
    }
    fs::write(lab.home.path().join("panes.json"), json!({"result": {"panes": panes}}).to_string()).unwrap();
    fs::write(lab.home.path().join("agents.json"), json!({"result": {"agents": agents}}).to_string()).unwrap();
    fs::write(lab.project().join("routines/nightly.md"), "+++\nschedule = \"every 24h\"\n+++\nCheck the build.\n\n```\nout\n```\n").unwrap();
    fs::write(lab.project().join(".state/ticker.json"), r#"{"routines":{"nightly":{"last_run":"2026-01-01T00:00:00Z"}}}"#).unwrap();

    lab.tick_until(|| lab.items().len() >= 3);
    let items = lab.items();
    assert_eq!(items.len(), 3, "{items:?}");
    let find = |infix: &str| items.iter().find(|id| id.contains(infix)).unwrap_or_else(|| panic!("{infix} missing from {items:?}")).clone();
    let (first, second, routine) = (find("-thread-state-t-0001-"), find("-thread-state-t-0002-"), find("-routine-nightly-"));
    // Items written in one pass never share a name: each carries its own counter.
    let counters: BTreeSet<&str> = [&first, &second, &routine].iter().map(|id| id.rsplit('-').next().unwrap()).collect();
    assert_eq!(counters, BTreeSet::from(["1", "2", "3"]), "{items:?}");

    let seen = lab.project().join(".state/inbox-seen.json");
    let digest = lab.ok(&["context", "demo", "--peek"]);
    assert!(!seen.exists(), "--peek recorded what it showed");
    assert!(digest.contains("## Inbox (3 unhandled)"), "{digest}");
    assert!(digest.contains(&format!("- {first} [thread-state] t-0001: t-0001 \"Fix parser\" is now Idle (idle)\n")), "{digest}");
    assert!(digest.contains(&format!("- {routine} [routine] nightly: routine `nightly` is due\nCheck the build.\n\n```\nout\n```\n")), "{digest}");
    lab.ok(&["context", "demo"]);
    let recorded: BTreeSet<String> = serde_json::from_slice::<Value>(&fs::read(&seen).unwrap()).unwrap().as_array().unwrap()
        .iter().map(|v| v.as_str().unwrap().to_owned()).collect();
    assert_eq!(recorded, items.iter().cloned().collect());

    assert_eq!(lab.ok(&["inbox", "done", "demo", &first]), "1 item(s) moved to inbox/done\n");
    assert!(lab.project().join(format!("inbox/done/{first}.md")).is_file());
    assert_eq!(lab.items(), [second.clone(), routine.clone()].into_iter().collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>());
    let digest = lab.ok(&["context", "demo", "--peek"]);
    assert!(digest.contains("## Inbox (2 unhandled)") && !digest.contains(&first), "{digest}");
    assert_eq!(lab.ok(&["inbox", "done", "demo", "--all"]), "2 item(s) moved to inbox/done\n");
    assert!(lab.items().is_empty());
    assert!(lab.ok(&["context", "demo", "--peek"]).contains("## Inbox (0 unhandled)"));
}

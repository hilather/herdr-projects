#![cfg(target_os = "linux")]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! herdr's action menu and the popups it opens, through the compiled CLI:
//! `action ID` hands its context to the popup it opens with `herdr plugin
//! pane open`, and `pane ID` consumes it. herdr is a shell fixture that logs
//! every call and lists the agents in `agents.json`.
use serde_json::{json, Value};
use std::{fs, io::Write, os::unix::fs::PermissionsExt, path::PathBuf, process::{Command, Output, Stdio}};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-farm");

const FAKE_HERDR: &str = "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$HOME/calls\"\ncase \"$1 $2\" in\n'agent list') cat \"$HOME/agents.json\";;\n*) echo '{\"result\":{}}';;\nesac\n";

struct Lab { home: tempfile::TempDir }

impl Lab {
    fn new() -> Self {
        let lab = Lab { home: tempfile::tempdir().unwrap() };
        fs::write(lab.path("herdr"), FAKE_HERDR).unwrap();
        fs::set_permissions(lab.path("herdr"), fs::Permissions::from_mode(0o700)).unwrap();
        lab
    }
    fn path(&self, name: &str) -> PathBuf { self.home.path().join(name) }
    /// Runs as herdr runs actions and popups: in session `a.sock`, with the
    /// plugin state directory and, for a popup, its handoff id.
    fn run(&self, args: &[&str], env: &[(&str, &str)], stdin: &str) -> Output {
        let mut child = Command::new(BIN).env_clear().env("HOME", self.home.path()).env("PATH", "/usr/bin:/bin")
            .env("HERDR_BIN_PATH", self.path("herdr")).env("HERDR_SOCKET_PATH", self.path("a.sock")).env("HERDR_PLUGIN_STATE_DIR", self.path("state"))
            .envs(env.iter().copied()).arg("--root").arg(self.path("root")).args(args)
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
        child.wait_with_output().unwrap()
    }
    /// The handoff id of the popup the last `action` opened for `entrypoint`.
    fn opened(&self, entrypoint: &str) -> String {
        let calls = fs::read_to_string(self.path("calls")).unwrap();
        let line = calls.lines().rev().find(|l| l.starts_with("plugin pane open") && l.contains(&format!("--entrypoint {entrypoint} "))).unwrap_or_else(|| panic!("{calls}"));
        line.split_whitespace().find_map(|w| w.strip_prefix("HERDR_FARM_HANDOFF=")).unwrap().to_owned()
    }
    fn popup(&self, entrypoint: &str, id: &str) -> Output { self.run(&["pane", entrypoint], &[("HERDR_FARM_HANDOFF", id)], "\n\n") }
    fn status(&self) -> String {
        serde_json::from_slice::<Value>(&fs::read(self.path("root/demo/.state/project.json")).unwrap()).unwrap()["status"].as_str().unwrap().to_owned()
    }
}

/// Replaces `concurrent_actions_keep_separate_context_and_cannot_replay`.
///
/// Two actions open their popups before either runs. Each popup gets the
/// context its own action captured (the command to run, the pane and
/// workspace to adopt), in either order, and running a popup again with the
/// same handoff is refused without doing anything.
#[test]
fn popups_opened_together_each_consume_their_own_context_once() {
    let lab = Lab::new();
    assert!(lab.run(&["new", "demo"], &[], "").status.success());
    let work = lab.path("work");
    fs::create_dir(&work).unwrap();
    let agent = json!({"workspace_id": "w5", "tab_id": "w5:t1", "pane_id": "w5:p1", "terminal_id": "term", "cwd": work, "name": "mine", "agent": "claude", "agent_status": "idle"});
    fs::write(lab.path("agents.json"), json!({"result": {"agents": [agent]}}).to_string()).unwrap();

    let paused = lab.run(&["action", "pause"], &[], "");
    assert!(paused.status.success(), "{}", String::from_utf8_lossy(&paused.stderr));
    let pick = lab.opened("pick");
    let context = json!({"workspace_label": "From Workspace", "workspace_cwd": work}).to_string();
    let adopt = lab.run(&["action", "adopt-workspace"], &[("HERDR_PANE_ID", "w5:p1"), ("HERDR_PLUGIN_CONTEXT_JSON", &context)], "");
    assert!(adopt.status.success(), "{}", String::from_utf8_lossy(&adopt.stderr));
    let adopt = lab.opened("adopt");
    assert_ne!(pick, adopt);
    assert_eq!(lab.status(), "active");

    // The agent is gone by the time the adopt popup runs: the refusal names
    // the pane the action captured, under the label it captured.
    fs::write(lab.path("agents.json"), json!({"result": {"agents": []}}).to_string()).unwrap();
    let out = lab.popup("adopt", &adopt);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(!out.status.success());
    assert!(text.contains("Project name [From Workspace]:") && text.contains("no agent is detected in pane w5:p1"), "{text}");
    assert!(!lab.path("root/from-workspace").exists());

    let out = lab.popup("pick", &pick);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "{text}");
    assert!(text.contains("Which project should `pause` act on?"), "{text}");
    assert_eq!(lab.status(), "paused");

    assert!(lab.run(&["resume", "demo"], &[], "").status.success());
    for (entrypoint, id) in [("pick", &pick), ("adopt", &adopt)] {
        let out = lab.popup(entrypoint, id);
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(!out.status.success());
        assert!(text.contains("popup handoff is missing or already consumed"), "{entrypoint}: {text}");
    }
    assert_eq!(lab.status(), "active");
    assert!(!lab.path("root/from-workspace").exists());
}

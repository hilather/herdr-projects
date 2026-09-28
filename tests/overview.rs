#![cfg(target_os = "linux")]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! `overview`, `focus` and `unfocus` through the compiled CLI: the section
//! order of the overview, which project a herdr workspace resolves to, slug
//! validation, and how a herdr subprocess's output, exit status and absence
//! reach the user. herdr is a shell fixture that serves `agent list`,
//! `pane list` and `session list` from files.
use serde_json::{json, Value};
use std::{fs, io::{BufRead, BufReader, Write}, os::unix::fs::PermissionsExt, path::{Path, PathBuf}, process::{Command, Output}};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-projects");

const FAKE_HERDR: &str = r#"#!/bin/sh
case "$1 $2" in
'agent list') cat "$HOME/agents.json";;
'pane list') cat "$HOME/panes.json";;
'session list')
  [ -e "$HOME/session-noise" ] && printf 'herdr: warming up\n' >&2
  if [ -e "$HOME/session-fail" ]; then printf 'session store is locked by pid 42\n' >&2; exit 3; fi
  cat "$HOME/sessions.json";;
*) echo '{"error":{"code":"unsupported","message":"not in fixture"}}'; exit 1;;
esac
"#;

struct Lab { home: tempfile::TempDir, fake: PathBuf }

impl Lab {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let fake = home.path().join("herdr");
        fs::write(&fake, FAKE_HERDR).unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
        let lab = Lab { home, fake };
        lab.session(&[], &[]);
        lab
    }
    fn root(&self) -> PathBuf { self.home.path().join("root") }
    fn path(&self, name: &str) -> PathBuf { self.home.path().join(name) }
    fn cli_env(&self, env: &[(&str, &str)], args: &[&str]) -> Output {
        Command::new(BIN).env_clear().env("HOME", self.home.path()).env("PATH", "/usr/bin:/bin").env("HERDR_BIN_PATH", &self.fake)
            .envs(env.iter().copied()).args(["--root", self.root().to_str().unwrap()]).args(args).output().unwrap()
    }
    fn ok_env(&self, env: &[(&str, &str)], args: &[&str]) -> String {
        let out = self.cli_env(env, args);
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }
    fn ok(&self, args: &[&str]) -> String { self.ok_env(&[], args) }
    /// A new project whose coordinator runs in workspace `w1` of `socket`.
    fn project(&self, slug: &str, socket: &Path) {
        self.ok(&["new", slug]);
        let coordinator = json!({"socket": socket, "workspace_id": "w1", "tab_id": "w1:t1", "pane_id": "w1:p1", "agent_name": "coordinator", "cwd": self.root().join(slug)});
        fs::write(self.root().join(slug).join(".state/coordinator.json"), coordinator.to_string()).unwrap();
    }
    fn thread(&self, slug: &str, id: &str, extra: &Value) {
        let mut record = json!({"id": id, "title": format!("Title {id}"), "status": "open", "kind": "worktree", "created": ago(3600), "agent": "claude",
            "agent_name": format!("hp-{slug}-{id}")});
        for (k, v) in extra.as_object().unwrap() { record[k] = v.clone(); }
        fs::write(self.root().join(slug).join(format!("threads/{id}.toml")), toml::to_string(&record).unwrap()).unwrap();
    }
    fn session(&self, agents: &[Value], panes: &[Value]) {
        fs::write(self.path("panes.json"), json!({"result": {"panes": panes}}).to_string()).unwrap();
        fs::write(self.path("agents.json"), json!({"result": {"agents": agents}}).to_string()).unwrap();
    }
}

fn ago(secs: i64) -> String {
    (jiff::Timestamp::now() - jiff::SignedDuration::from_secs(secs)).to_string()
}

/// A socket that answers each request with `reply` and records the request lines.
fn serve(socket: &Path, reply: &'static str) -> std::sync::mpsc::Receiver<String> {
    let listener = std::os::unix::net::UnixListener::bind(socket).unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            let _ = send.send(line);
            let _ = stream.write_all(format!("{reply}\n").as_bytes());
        }
    });
    receive
}

/// Replaces `groups_print_in_display_order_not_precedence_order`.
#[test]
fn overview_prints_groups_in_display_order_and_names_the_pane_that_needs_you() {
    let lab = Lab::new();
    let socket = lab.path("session.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    lab.project("demo", &socket);
    // Records are numbered in precedence order; each has its own pane.
    let cases = [
        (json!({"status": "resolved", "resolved_reason": "manual"}), None),
        (json!({}), Some("idle")),
        (json!({}), Some("working")),
        (json!({"last_state": "blocked", "last_state_change": ago(120)}), Some("blocked")),
        (json!({"report_hash": "h"}), Some("done")),
        (json!({"report_hash": "h", "pr_state": "OPEN", "pr_review": "APPROVED"}), Some("idle")),
    ];
    let (mut agents, mut panes) = (Vec::new(), Vec::new());
    for (n, (extra, status)) in cases.iter().enumerate() {
        let (id, ws) = (format!("t-{:04}", n + 1), format!("w{}", n + 2));
        let ids = json!({"workspace_id": ws, "tab_id": format!("{ws}:t1"), "pane_id": format!("{ws}:p1"), "cwd": format!("/wt/{id}")});
        let mut record = ids.clone();
        for (k, v) in extra.as_object().unwrap() { record[k] = v.clone(); }
        lab.thread("demo", &id, &record);
        if let Some(status) = status {
            panes.push(ids.clone());
            let mut agent = ids.clone();
            (agent["agent"], agent["name"], agent["agent_status"]) = (json!("claude"), json!(format!("hp-demo-{id}")), json!(status));
            agents.push(agent);
        }
    }
    lab.session(&agents, &panes);
    let text = lab.ok(&["overview", "demo"]);
    let order: Vec<usize> = ["Ready for review (1)", "Waiting on you (1)", "Working (1)", "Landing (1)", "Idle (1)", "Resolved (1)"].iter()
        .map(|label| text.find(&format!("\n{label}")).unwrap_or_else(|| panic!("{label} missing in\n{text}"))).collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{text}");
    assert!(text.contains("t-0004  Title t-0004  [blocked]") && text.contains("needs you in pane w5:p1"), "{text}");
    assert_eq!(text.matches("needs you in pane").count(), 1, "{text}");
}

/// Replaces `workspace_resolves_through_the_coordinator_or_a_thread_in_the_same_socket_only`.
#[test]
fn overview_without_a_slug_resolves_the_workspace_only_within_its_own_socket() {
    let lab = Lab::new();
    let (a, b) = (lab.path("a.sock"), lab.path("b.sock"));
    lab.project("alpha", &a);
    lab.project("beta", &b);
    lab.thread("alpha", "t-0001", &json!({"workspace_id": "w7", "tab_id": "w7:t1", "pane_id": "w7:p1", "cwd": "/wt/t-0001"}));
    lab.thread("alpha", "t-0002", &json!({"status": "resolved", "resolved_reason": "manual", "workspace_id": "w8", "pane_id": "w8:p1"}));
    let heads = |workspace: &str, socket: &Path| -> Vec<String> {
        lab.ok_env(&[("HERDR_WORKSPACE_ID", workspace), ("HERDR_SOCKET_PATH", socket.to_str().unwrap())], &["overview"])
            .lines().filter(|l| l.starts_with("alpha (") || l.starts_with("beta (")).map(|l| l.split(' ').next().unwrap().to_owned()).collect()
    };
    // Both coordinators record w1; the socket tells them apart.
    assert_eq!(heads("w1", &a), ["alpha"]);
    assert_eq!(heads("w1", &b), ["beta"]);
    // Through an open local thread's workspace, in its own socket only.
    assert_eq!(heads("w7", &a), ["alpha"]);
    // Nothing resolves, and off a terminal every project is shown.
    for (workspace, socket) in [("w7", &b), ("w8", &a), ("w9", &a), ("", &a)] {
        assert_eq!(heads(workspace, socket), ["alpha", "beta"], "{workspace} {}", socket.display());
    }
    // `focus` cannot act on every project, so an unresolved workspace is refused.
    let out = lab.cli_env(&[("HERDR_WORKSPACE_ID", "w9"), ("HERDR_SOCKET_PATH", a.to_str().unwrap())], &["focus"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("pass a slug"), "{}", String::from_utf8_lossy(&out.stderr));
}

/// Replaces `an_explicit_slug_is_validated`.
#[test]
fn overview_and_focus_refuse_a_path_like_slug() {
    let lab = Lab::new();
    lab.ok(&["new", "demo"]);
    let before = fs::read_dir(lab.home.path()).unwrap().count();
    for command in ["overview", "focus"] {
        let out = lab.cli_env(&[], &[command, "../x"]);
        assert!(!out.status.success(), "{command} accepted ../x");
        assert!(String::from_utf8_lossy(&out.stderr).contains("../x"), "{command}: {}", String::from_utf8_lossy(&out.stderr));
    }
    assert!(!lab.path("x").exists());
    assert_eq!(fs::read_dir(lab.home.path()).unwrap().count(), before);
}

/// Replaces the runner tests `captures_output_and_exit_code` and
/// `missing_program_is_an_error`: `unfocus --session` resolves the socket from
/// `herdr session list --json`, so herdr's stdout, stderr and exit status, or
/// its absence, decide what the user sees.
#[test]
fn unfocus_reads_herdr_stdout_reports_its_stderr_on_failure_and_survives_a_missing_binary() {
    let lab = Lab::new();
    let socket = lab.path("named.sock");
    let requests = serve(&socket, r#"{"id":"herdr-projects","result":{}}"#);
    fs::write(lab.path("sessions.json"), json!({"sessions": [{"name": "work", "default": false, "running": true, "socket_path": socket}]}).to_string()).unwrap();
    // Diagnostic noise on stderr does not disturb the JSON read from stdout.
    fs::write(lab.path("session-noise"), "").unwrap();
    assert_eq!(lab.ok(&["unfocus", "--session", "work"]), "sidebar view cleared\n");
    let request: Value = serde_json::from_str(&requests.recv_timeout(std::time::Duration::from_secs(5)).unwrap()).unwrap();
    assert_eq!(request["method"], "agent.view.clear");
    let out = lab.cli_env(&[], &["unfocus", "--session", "other"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("herdr has no session named `other`"), "{}", String::from_utf8_lossy(&out.stderr));

    // A non-zero exit is a failure, and its stderr is the reason given.
    fs::remove_file(lab.path("session-noise")).unwrap();
    fs::write(lab.path("session-fail"), "").unwrap();
    let out = lab.cli_env(&[], &["unfocus", "--session", "work"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("session list` failed: session store is locked by pid 42"), "{stderr}");

    // A herdr that does not exist is an error, not a panic or a hang.
    let missing = lab.path("no-such-herdr");
    let out = Command::new(BIN).env_clear().env("HOME", lab.home.path()).env("PATH", "/usr/bin:/bin").env("HERDR_BIN_PATH", &missing)
        .args(["--root", lab.root().to_str().unwrap(), "unfocus", "--session", "work"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", String::from_utf8_lossy(&out.stderr));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains(&format!("could not run `{}`", missing.display())) && !stderr.contains("panicked"), "{stderr}");
    assert!(requests.try_recv().is_err(), "no request may reach the socket after a failed lookup");
}

/// Replaces the runner socket test `connects_once_to_a_real_path_and_exchanges_one_line`.
///
/// `unfocus --session` opens one connection to the socket path herdr names,
/// writes exactly one request line, and reads the reply up to its newline even
/// when it arrives in pieces and is followed by other bytes.
#[test]
fn unfocus_sends_one_line_over_one_connection_and_reads_one_reply_line() {
    use std::{io::Read, os::unix::net::UnixListener, time::Duration};
    let lab = Lab::new();
    let socket = lab.path("named.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    fs::write(lab.path("sessions.json"), json!({"sessions": [{"name": "work", "default": false, "running": true, "socket_path": socket}]}).to_string()).unwrap();
    let server = std::thread::spawn(move || {
        let mut connections = Vec::new();
        listener.set_nonblocking(true).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            let mut stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => { if !connections.is_empty() { break; } std::thread::sleep(Duration::from_millis(10)); continue; }
                Err(e) => panic!("{e}"),
            };
            stream.set_nonblocking(false).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\n") && stream.read(&mut byte).unwrap() == 1 { request.push(byte[0]); }
            stream.write_all(br#"{"id":"herdr-projects","#).unwrap();
            std::thread::sleep(Duration::from_millis(200));
            stream.write_all(b"\"result\":{}}\n{\"trailing\":").unwrap();
            // The client sends nothing more and closes its end.
            let mut rest = Vec::new();
            let _ = stream.read_to_end(&mut rest);
            connections.push((request, rest));
            // Let a second connection, if any, arrive before deciding.
            std::thread::sleep(Duration::from_millis(300));
        }
        connections
    });
    assert_eq!(lab.ok(&["unfocus", "--session", "work"]), "sidebar view cleared\n");
    let connections = server.join().unwrap();
    assert_eq!(connections.len(), 1, "{connections:?}");
    let (request, rest) = &connections[0];
    let line = String::from_utf8(request.clone()).unwrap();
    assert_eq!(line.matches('\n').count(), 1, "{line:?}");
    assert_eq!(serde_json::from_str::<Value>(&line).unwrap()["method"], "agent.view.clear");
    assert!(rest.is_empty(), "bytes after the request line: {rest:?}");
}

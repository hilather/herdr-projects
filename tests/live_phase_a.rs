//! Run explicitly; never discovers or connects to an existing user session.
#[path = "support/live.rs"]
mod live;
use live::Lab;

#[test]
#[ignore = "requires HP_LIVE_HERDR; creates only a disposable named server"]
fn live_herdr_workspace_and_worktree_contract() {
    let mut lab = Lab::new();
    lab.start();
    let root = lab.path().join("repo");
    std::fs::create_dir(&root).unwrap();
    for args in [vec!["init", "--quiet"], vec!["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--quiet", "--allow-empty", "-m", "base"]] {
        let mut cmd = lab.command("git"); cmd.arg("-C").arg(&root).args(args);
        let (ok, _, err) = lab.run(cmd); assert!(ok, "{err}");
    }
    let result = lab.herdr(&["workspace", "create", "--cwd", root.to_str().unwrap(), "--label", "Acceptance only", "--no-focus"]);
    let workspace = result["result"]["workspace"]["workspace_id"].as_str().unwrap();
    let pane = result["result"]["root_pane"]["pane_id"].as_str().unwrap();
    let listed = lab.herdr(&["pane", "list"]);
    assert!(listed["result"]["panes"].as_array().unwrap().iter().any(|p| p["pane_id"] == pane));
    let work = lab.path().join("work 'λ$ literal");
    let result = lab.herdr(&["worktree", "create", "--cwd", root.to_str().unwrap(), "--path", work.to_str().unwrap(), "--branch", "hp-acceptance", "--base", "HEAD", "--label", "literal 'λ$ label", "--no-focus"]);
    assert_eq!(result["result"]["worktree"]["path"], work.to_str().unwrap());
    assert!(result["result"]["root_pane"]["pane_id"].is_string());
    let reopened = lab.herdr(&["worktree", "open", "--cwd", root.to_str().unwrap(), "--path", work.to_str().unwrap(), "--no-focus"]);
    assert_eq!(reopened["result"]["worktree"]["path"], work.to_str().unwrap());
    lab.hp(&["new", "demo"]);
    let project = lab.path().join("projects/demo");
    let status = lab.herdr(&["status", "server", "--json"]);
    let socket = status["socket"].as_str().unwrap();
    std::fs::write(project.join(".state/coordinator.json"), serde_json::to_vec(&serde_json::json!({
        "socket": socket, "workspace_id": workspace, "pane_id": pane, "cwd": root,
    })).unwrap()).unwrap();
    let thread_dir = work.join(".herdr-project/demo-t-0001");
    std::fs::create_dir_all(&thread_dir).unwrap();
    std::fs::write(thread_dir.join("report.md"), b"Live fixture preserved report").unwrap();
    std::fs::write(root.join(".git/info/exclude"), b".herdr-project/\n").unwrap();
    let thread = serde_json::json!({
        "id": "t-0001", "title": "Live fixture", "kind": "worktree", "status": "open",
        "repo": root, "branch": "hp-acceptance", "worktree_path": work, "cwd": work,
        "thread_dir": thread_dir, "workspace_id": reopened["result"]["root_pane"]["workspace_id"],
        "tab_id": reopened["result"]["root_pane"]["tab_id"], "pane_id": reopened["result"]["root_pane"]["pane_id"],
        "agent": "hp-acceptance-no-agent",
    });
    std::fs::write(project.join("threads/t-0001.toml"), toml::to_string(&thread).unwrap()).unwrap();
    // Close only the fixture's worktree workspace so its shell cannot write.
    lab.herdr(&["workspace", "close", reopened["result"]["root_pane"]["workspace_id"].as_str().unwrap()]);
    let mut removal = lab.command(env!("CARGO_BIN_EXE_herdr-projects"));
    removal.args(["thread", "resolve", "demo", "t-0001", "--remove-worktree", "--writers-stopped"]);
    let (removed_ok, _, error) = lab.run(removal);
    assert!(removed_ok || std::env::var_os("HP_LIVE_REQUIRE_REMOVAL").is_none(), "positive cleanup required: {error}");
    if removed_ok {
        assert!(!work.exists());
    } else {
        assert!(error.contains("cannot establish writer quiescence") && error.contains("Permission denied"), "unexpected cleanup failure: {error}");
        assert!(work.exists(), "unprovable writer state must preserve the worktree");
        assert_eq!(std::fs::read(thread_dir.join("report.md")).unwrap(), b"Live fixture preserved report");
        eprintln!("UNTESTED positive live removal: process visibility denied; source preservation confirmed: {error}");
        lab.hp(&["thread", "resolve", "demo", "t-0001"]);
    }
    let removed: toml::Value = toml::from_str(&std::fs::read_to_string(project.join("threads/t-0001.toml")).unwrap()).unwrap();
    assert_eq!(removed["status"].as_str(), Some("resolved"));
    let snapshot = removed["artifact_snapshot"].as_str().unwrap();
    assert_eq!(std::fs::read(project.join(".state/artifacts/t-0001").join(snapshot).join("report.md")).unwrap(), b"Live fixture preserved report");
    lab.hp(&["thread", "resolve", "demo", "t-0001", "--reopen"]);
    assert_eq!(work.exists(), !removed_ok);
    lab.hp(&["thread", "restart", "demo", "t-0001"]);
    lab.hp(&["ticker", "stop"]);
    assert!(work.is_dir());
    let restarted: toml::Value = toml::from_str(&std::fs::read_to_string(project.join("threads/t-0001.toml")).unwrap()).unwrap();
    assert_eq!(restarted["branch"].as_str(), Some("hp-acceptance"));
    assert!(restarted.get("removal").is_none());
    eprintln!("live Herdr/Git resolve-reopen-restart passed; physical removal={removed_ok}; no coding agent was launched");
}

#[test]
#[ignore = "requires HP_LIVE_HERDR and OpenSSH; starts an ephemeral loopback-only sshd"]
fn live_ssh_native_stream_preserves_literal_paths_and_binary_data() {
    use std::{fs, process::Stdio, time::{Duration, Instant}};
    use sha2::{Digest, Sha256};
    struct Server(std::process::Child);
    impl Drop for Server { fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); } }
    let lab = Lab::new();
    assert!(!std::path::Path::new("/etc/ssh/sshrc").exists(), "fixture refuses system SSH startup hooks");
    let sshd = std::env::var_os("HP_LIVE_SSHD").unwrap_or_else(|| "/usr/bin/sshd".into());
    for name in ["host-key", "client-key"] {
        let mut cmd = lab.command("ssh-keygen");
        cmd.args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(lab.path().join(name));
        let (ok, _, error) = lab.run(cmd); assert!(ok, "{error}");
    }
    let (ok, user, error) = { let mut cmd = lab.command("id"); cmd.arg("-un"); lab.run(cmd) };
    assert!(ok, "{error}");
    let user = user.trim();
    assert!(user.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    fs::copy(lab.path().join("client-key.pub"), lab.path().join("authorized_keys")).unwrap();
    let config = format!("ListenAddress 127.0.0.1\nPort {port}\nHostKey {0}/host-key\nPidFile {0}/sshd.pid\nAuthorizedKeysFile {0}/authorized_keys\nAllowUsers {user}\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nPermitUserRC no\nStrictModes no\nLogLevel VERBOSE\nSetEnv HOME={0}/home BASH_ENV=/dev/null\n", lab.path().display());
    fs::write(lab.path().join("sshd_config"), config).unwrap();
    let log = fs::File::create(lab.path().join("sshd.log")).unwrap();
    let mut command = lab.command(sshd);
    let mut server = Server(command.args(["-D", "-e", "-f"]).arg(lab.path().join("sshd_config")).stdout(log.try_clone().unwrap()).stderr(log).stdin(Stdio::null()).spawn().unwrap());
    let until = Instant::now() + Duration::from_secs(10);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(server.0.try_wait().unwrap().is_none() && Instant::now() < until, "sshd startup failed: {}", fs::read_to_string(lab.path().join("sshd.log")).unwrap());
        std::thread::sleep(Duration::from_millis(25));
    }
    let pubkey = fs::read_to_string(lab.path().join("host-key.pub")).unwrap();
    fs::write(lab.path().join("known_hosts"), format!("[127.0.0.1]:{port} {pubkey}")).unwrap();
    fs::write(lab.path().join("ssh_config"), format!("Host fixture\n HostName 127.0.0.1\n Port {port}\n User {user}\n IdentityFile {0}/client-key\n IdentitiesOnly yes\n BatchMode yes\n StrictHostKeyChecking yes\n UserKnownHostsFile {0}/known_hosts\n GlobalKnownHostsFile /dev/null\n ConnectTimeout 5\n", lab.path().display())).unwrap();
    let marker = lab.path().join("SHOULD_NOT_EXIST");
    let source = lab.path().join(format!("-literal 'λ$; `touch {}` [*]\nsource", marker.display()));
    fs::create_dir_all(source.join("library/empty")).unwrap();
    let payload = vec![255u8; 2 * 1024 * 1024];
    fs::write(source.join("report.md"), &payload).unwrap();
    fs::write(source.join("library/quotes 'λ$\nfile"), b"binary\0\xff").unwrap();
    let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    let script = format!("/usr/bin/env -i HOME={} PATH=/usr/bin:/bin {} artifact-stream --path {}", quote(lab.path().join("home").to_str().unwrap()), quote(env!("CARGO_BIN_EXE_herdr-projects")), quote(source.to_str().unwrap()));
    let mut command = lab.command("ssh");
    command.arg("-F").arg(lab.path().join("ssh_config")).args(["--", "fixture", &format!("sh -c {}", quote(&script))]);
    let (ok, bytes, error) = lab.run_bytes(command);
    assert!(ok, "ssh failed: {error}\n{}", fs::read_to_string(lab.path().join("sshd.log")).unwrap());
    assert_eq!(&bytes[..8], b"HPAR\x01\0\0\0");
    let size = u32::from_be_bytes(bytes[8..12].try_into().unwrap()) as usize;
    let manifest: serde_json::Value = serde_json::from_slice(&bytes[12..12 + size]).unwrap();
    let mut at = 12 + size;
    let mut report_seen = false;
    let mut empty_seen = false;
    for entry in manifest["entries"].as_array().unwrap() {
        if entry["directory"] == true { empty_seen |= entry["path"] == "library/empty"; continue; }
        let end = at + entry["bytes"].as_u64().unwrap() as usize;
        assert_eq!(format!("{:x}", Sha256::digest(&bytes[at..end])), entry["sha256"].as_str().unwrap());
        if entry["path"] == "report.md" { assert_eq!(&bytes[at..end], payload); report_seen = true; }
        at = end;
    }
    assert!(report_seen && empty_seen);
    assert_eq!(at, bytes.len());
    assert!(!marker.exists());
    eprintln!("live loopback OpenSSH native stream passed: literal hostile path, 2 MiB binary payload, hashes and empty directory");
}

#[test]
#[ignore = "requires HP_LIVE_HERDR and a PTY; drives only the disposable client's popup"]
fn live_popup_handoff_routes_input_to_the_requested_action() {
    use std::fs;
    let mut lab = Lab::new();
    let plugin = lab.path().join("plugin");
    fs::create_dir_all(plugin.join("target/release")).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_herdr-projects"), plugin.join("target/release/herdr-projects")).unwrap();
    let mut manifest: toml::Value = toml::from_str(include_str!("../herdr-plugin.toml")).unwrap();
    // This fixture tests the action/popup contract, not package build or startup.
    manifest.as_table_mut().unwrap().remove("build");
    manifest.as_table_mut().unwrap().remove("startup");
    fs::write(plugin.join("herdr-plugin.toml"), toml::to_string(&manifest).unwrap()).unwrap();
    let mut link = lab.command(&lab.herdr);
    link.args(["plugin", "link"]).arg(&plugin);
    let (ok, _, error) = lab.run(link); assert!(ok, "plugin link: {error}");
    lab.start();
    lab.hp(&["new", "demo"]);
    lab.hp(&["new", "z-other"]);
    let other_state = lab.path().join("projects/z-other/.state/project.json");
    let other_before = fs::read(&other_state).unwrap();
    lab.herdr(&["workspace", "create", "--cwd", lab.path().to_str().unwrap(), "--label", "Popup fixture", "--no-focus"]);
    let mut client = live::Client::start(&lab);
    // A real render proves client attachment; no keystrokes target another session.
    assert!(client.wait_for("fixture"), "{}", lab.diagnostics());
    for (action, expected) in [("pause", "paused"), ("resume", "active")] {
        client.clear_output();
        lab.herdr(&["plugin", "action", "invoke", action, "--plugin", "herdr-projects"]);
        assert!(client.wait_for("number:"), "{}", lab.diagnostics());
        client.type_text(b"1\r");
        assert!(client.wait_for("close:"), "{}", lab.diagnostics());
        let state: serde_json::Value = serde_json::from_slice(&fs::read(lab.path().join("projects/demo/.state/project.json")).unwrap()).unwrap();
        assert_eq!(state["status"], expected);
        assert_eq!(fs::read(&other_state).unwrap(), other_before);
        client.type_text(b"\r");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    eprintln!("live popup pause/resume actions consumed distinct handoffs and routed PTY input correctly");
}

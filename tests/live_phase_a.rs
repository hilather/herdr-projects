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

#[test]
#[ignore = "requires HP_LIVE_HERDR; observes only an isolated disposable terminal"]
#[cfg(target_os="linux")]
fn live_canonical_process_contract() {
    let mut lab=Lab::new();lab.start();
    let created=lab.herdr(&["workspace","create","--cwd",lab.path().to_str().unwrap(),"--label","Canonical launch contract","--no-focus"]);
    let pane=created["result"]["root_pane"]["pane_id"].as_str().unwrap();
    let workspace=created["result"]["workspace"]["workspace_id"].as_str().unwrap();
    eprintln!("created: {created}");
    eprintln!("before: {}",lab.herdr(&["pane","process-info","--pane",pane]));
    // pane run is a silent CLI submission, not a typed launch acknowledgment.
    let mut command=lab.command(&lab.herdr);
    command.args(["--session","hp-acceptance","pane","run",pane,"/usr/bin/sleep","30"]);
    let(ok,output,error)=lab.run(command);assert!(ok,"pane run: {error}");
    eprintln!("submission output: {output:?}");
    let deadline=std::time::Instant::now()+std::time::Duration::from_secs(5);
    let pid=loop {
        let running=lab.herdr(&["pane","process-info","--pane",pane]);
        if let Some(process)=running["result"]["process_info"]["foreground_processes"].as_array().unwrap().iter()
            .find(|process|process["argv"][0]=="/usr/bin/sleep") {
            eprintln!("running: {running}");break process["pid"].as_u64().unwrap();
        }
        assert!(std::time::Instant::now()<deadline,"exact sleep process was not observed: {running}");
        std::thread::sleep(std::time::Duration::from_millis(25));
    };
    let closed=lab.herdr(&["workspace","close",workspace]);
    eprintln!("closed: {closed}");
    let listed=lab.herdr(&["pane","list"]);
    assert!(!listed["result"]["panes"].as_array().unwrap().iter().any(|p|p["pane_id"]==pane));
    // This assertion is deliberately limited to our single sleep process. Pane
    // absence alone does not certify cleanup of arbitrary agent descendants.
    let deadline=std::time::Instant::now()+std::time::Duration::from_secs(5);
    while std::path::Path::new(&format!("/proc/{pid}")).exists() {
        let status=std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
        if status.lines().any(|line|line.starts_with("State:")&&line.contains('Z')) {break;}
        assert!(std::time::Instant::now()<deadline,"fixture process remains after workspace close");
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

#[test]
#[cfg(target_os="linux")]
#[ignore = "requires HP_LIVE_HERDR and Linux user/PID namespaces; isolated server only"]
fn live_canonical_supervisor_stops_detached_worker_descendants() {
    let mut lab=Lab::new();lab.start();
    let created=lab.herdr(&["workspace","create","--cwd",lab.path().to_str().unwrap(),"--label","Supervised worker contract","--no-focus"]);
    let pane=created["result"]["root_pane"]["pane_id"].as_str().unwrap();
    let workspace=created["result"]["workspace"]["workspace_id"].as_str().unwrap();
    let heartbeat=lab.path().join("detached-heartbeat");
    let script="/usr/bin/setsid /bin/sh -c 'while :; do printf x >> \"$1\"; /usr/bin/sleep 0.05; done' detached-worker \"$1\" & wait";
    let argv=herdr_projects::worker_supervision::command(std::path::Path::new("/bin/sh"),
        &["-c".into(),script.into(),"worker".into(),heartbeat.to_str().unwrap().into()],10).unwrap();
    let line=herdr_projects::worker_supervision::posix_command(&argv).unwrap();
    let mut command=lab.command(&lab.herdr);
    command.args(["--session","hp-acceptance","pane","run",pane,&line]);
    let(ok,_,error)=lab.run(command);assert!(ok,"supervised submission failed: {error}");
    let deadline=std::time::Instant::now()+std::time::Duration::from_secs(5);
    while std::fs::metadata(&heartbeat).map(|m|m.len()).unwrap_or(0)<2 {
        assert!(std::time::Instant::now()<deadline,"detached namespace fixture did not start: {}",lab.herdr(&["pane","read",pane]));
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let process=lab.herdr(&["pane","process-info","--pane",pane]);
    let outer=process["result"]["process_info"]["foreground_processes"].as_array().unwrap().iter()
        .find(|p|p["argv"][0]=="/usr/bin/unshare").expect("namespace supervisor was not in the pane's foreground process group");
    let pid=outer["pid"].as_u64().unwrap();
    let observation=herdr_projects::worker_supervision::SupervisorObservation::observe(u32::try_from(pid).unwrap(),&argv).unwrap();
    assert!(!observation.exited().unwrap());
    use std::os::unix::fs::MetadataExt;
    let (device,inode)=observation.namespace_identity().unwrap();
    assert_ne!(inode,std::fs::metadata("/proc/self/ns/pid").unwrap().ino());
    lab.herdr(&["workspace","close",workspace]);
    let deadline=std::time::Instant::now()+std::time::Duration::from_secs(5);
    loop {
        let remains=std::fs::read_dir("/proc").unwrap().filter_map(Result::ok).any(|e| {
            let name=e.file_name();let Some(name)=name.to_str() else {return false;};
            name.bytes().all(|b|b.is_ascii_digit()) && std::fs::metadata(e.path().join("ns/pid"))
                .is_ok_and(|m|m.ino()==inode&&m.dev()==device)
        });
        if !remains && observation.exited().unwrap() {break;}
        assert!(std::time::Instant::now()<deadline,"worker namespace survived pane closure");
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let bytes=std::fs::metadata(&heartbeat).unwrap().len();
    std::thread::sleep(std::time::Duration::from_millis(150));
    assert_eq!(std::fs::metadata(&heartbeat).unwrap().len(),bytes,"detached worker is still writing");
}

#[test]
#[cfg(target_os="linux")]
#[ignore = "requires HP_LIVE_HERDR; validates literal argv only in a disposable server"]
fn live_canonical_layout_launches_literal_argv() {
    use std::{io::Write,process::Stdio,time::{Duration,Instant}};
    let mut lab=Lab::new();lab.start();
    let created=lab.herdr(&["workspace","create","--cwd",lab.path().to_str().unwrap(),"--label","Literal argv contract","--no-focus"]);
    let workspace=created["result"]["workspace"]["workspace_id"].as_str().unwrap();
    let literal="space ' $(touch injected); `touch injected` \\ λ";
    let destination=lab.path().join("literal-result");
    let argv=herdr_projects::worker_supervision::isolated_gated_command(std::path::Path::new("/bin/sh"),&[
        "-c".into(),"printf '%s' \"$1\" > \"$2\"; exec /usr/bin/sleep 30".into(),
        "fixture".into(),literal.into(),destination.display().to_string()],15,"release-live-fixture",&lab.path().join("home")).unwrap();
    let request=serde_json::json!({"id":"literal-launch","method":"layout.apply","params":{
        "workspace_id":workspace,"tab_label":"Canonical argv fixture","focus":false,
        "root":{"type":"pane","cwd":lab.path(),"command":argv,"env":{}}}});
    // A regular file avoids introducing an unbounded stdin pipe into this test.
    let input=lab.path().join("request.json");
    writeln!(std::fs::File::create(&input).unwrap(),"{request}").unwrap();
    let mut command=lab.command(&lab.herdr);
    command.args(["--session","hp-acceptance","remote-api-bridge"])
        .stdin(Stdio::from(std::fs::File::open(input).unwrap()));
    let(ok,text,error)=lab.run(command);assert!(ok,"{error}: {text}");
    let response:serde_json::Value=serde_json::from_str(&text).unwrap();
    assert_eq!(response["id"],"literal-launch");assert!(response.get("error").is_none(),"{response}");
    let pane=response["result"]["layout"]["focused_pane_id"].as_str().unwrap();
    // Recovery discovers the pane through the workspace inventory, then matches
    // the retained command digest before accepting its live supervisor.
    let request=serde_json::json!({"id":"recover-list","method":"pane.list","params":{"workspace_id":workspace}});
    let input=lab.path().join("recover-list.json");
    writeln!(std::fs::File::create(&input).unwrap(),"{request}").unwrap();
    let mut command=lab.command(&lab.herdr);
    command.args(["--session","hp-acceptance","remote-api-bridge"])
        .stdin(Stdio::from(std::fs::File::open(input).unwrap()));
    let(ok,text,error)=lab.run(command);assert!(ok,"{error}: {text}");
    let listed:serde_json::Value=serde_json::from_str(&text).unwrap();
    assert_eq!(listed["id"],"recover-list");
    assert!(listed["result"]["panes"].as_array().unwrap().iter()
        .any(|p|p["pane_id"]==pane && p["workspace_id"]==workspace));
    let process=lab.herdr(&["pane","process-info","--pane",pane]);
    let outer=process["result"]["process_info"]["foreground_processes"].as_array().unwrap().iter()
        .find(|p|p["argv"][0]=="/usr/bin/unshare").expect("direct supervisor was not observed");
    let observed=herdr_projects::worker_supervision::SupervisorObservation::observe(u32::try_from(outer["pid"].as_u64().unwrap()).unwrap(),&argv).unwrap();
    let gate=observed.waiting_gate(&argv).unwrap();
    let identity=observed.identity().clone();drop(observed);
    assert!(!herdr_projects::worker_supervision::SupervisorObservation::reconnect(&identity).unwrap().exited().unwrap());
    assert!(!destination.exists(), "worker executed before gate release");
    let release=serde_json::json!({"id":"release-live","method":"pane.send_input",
        "params":{"pane_id":pane,"text":"release-live-fixture\n","keys":[]}});
    let input=lab.path().join("release.json");
    writeln!(std::fs::File::create(&input).unwrap(),"{release}").unwrap();
    let mut command=lab.command(&lab.herdr);
    command.args(["--session","hp-acceptance","remote-api-bridge"])
        .stdin(Stdio::from(std::fs::File::open(input).unwrap()));
    let(ok,text,error)=lab.run(command);assert!(ok,"{error}: {text}");
    let response:serde_json::Value=serde_json::from_str(&text).unwrap();
    assert_eq!(response["id"],"release-live");assert!(response.get("error").is_none(),"{response}");
    assert_eq!(response["result"]["type"],"ok");
    let deadline=Instant::now()+Duration::from_secs(5);
    while !destination.exists() {assert!(Instant::now()<deadline,"literal worker did not start: {response}");std::thread::sleep(Duration::from_millis(25));}
    assert_eq!(std::fs::read_to_string(destination).unwrap(),literal);
    assert!(!lab.path().join("injected").exists());
    assert!(gate.check().is_err(),"gate proof survived agent exec");
    lab.herdr(&["workspace","close",workspace]);
    let deadline=Instant::now()+Duration::from_secs(5);
    while !herdr_projects::worker_supervision::SupervisorObservation::recover_exited(&identity).unwrap() {
        assert!(Instant::now()<deadline,"direct supervisor survived closure");std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
#[ignore = "requires HP_LIVE_HERDR and HP_LIVE_CODEX; disposable unauthenticated startup only"]
#[cfg(target_os = "linux")]
fn live_vendor_direct_exec_and_native_naming_contract() {
    use std::{io::Write, path::PathBuf, process::Stdio, time::{Duration, Instant}};
    use sha2::{Digest, Sha256};
    fn request(lab: &Lab, method: &str, params: serde_json::Value) -> serde_json::Value {
        let request = serde_json::json!({"id":"vendor-contract","method":method,"params":params});
        let file = lab.path().join("vendor-request.json");
        writeln!(std::fs::File::create(&file).unwrap(),"{request}").unwrap();
        let mut cmd = lab.command(&lab.herdr);
        cmd.args(["--session","hp-acceptance","remote-api-bridge"])
            .stdin(Stdio::from(std::fs::File::open(file).unwrap()));
        let (ok,text,error)=lab.run(cmd); assert!(ok,"{error}");
        let reply:serde_json::Value=serde_json::from_str(&text).unwrap();
        assert!(reply.get("error").is_none(),"{reply}");
        assert_eq!(reply["id"],"vendor-contract");
        reply["result"].clone()
    }
    let agent = PathBuf::from(std::env::var_os("HP_LIVE_CODEX").expect("set HP_LIVE_CODEX to an exact native executable"));
    assert!(agent.is_absolute()); let agent=agent.canonicalize().unwrap();
    let mut lab=Lab::new(); lab.start();
    let mut version=lab.command(&agent); version.arg("--version");
    let (ok,version,error)=lab.run(version);assert!(ok,"{error}");
    assert!(herdr_projects::profile_config::observed_version("codex",&version).is_some());
    let workspace=lab.herdr(&["workspace","create","--cwd",lab.path().to_str().unwrap(),"--label","Vendor startup contract","--no-focus"]);
    let workspace=workspace["result"]["workspace"]["workspace_id"].as_str().unwrap();
    // No inherited home, credentials, user configuration or task prompt. This
    // tests startup/naming/termination, never authenticated protocol capability.
    let args:Vec<String>=vec!["--no-alt-screen".into()];
    let argv=herdr_projects::worker_supervision::isolated_gated_command(&agent,&args,45,"release-vendor-contract",&lab.path().join("home")).unwrap();
    let created=request(&lab,"layout.apply",serde_json::json!({"workspace_id":workspace,"tab_label":"Vendor contract","focus":false,
        "root":{"type":"pane","cwd":lab.path(),"command":argv,"env":{}}}));
    let pane=created["layout"]["focused_pane_id"].as_str().unwrap();
    let info=request(&lab,"pane.process_info",serde_json::json!({"pane_id":pane}));
    let outer=info["process_info"]["foreground_processes"].as_array().unwrap().iter()
        .find(|p|p["argv"][0]=="/usr/bin/unshare").unwrap();
    let supervisor=herdr_projects::worker_supervision::SupervisorObservation::observe(u32::try_from(outer["pid"].as_u64().unwrap()).unwrap(),&argv).unwrap();
    let gate=supervisor.waiting_gate(&argv).unwrap();
    let released=request(&lab,"pane.send_input",serde_json::json!({"pane_id":pane,"text":"release-vendor-contract\n","keys":[]}));
    assert_eq!(released["type"],"ok");
    let end=Instant::now()+Duration::from_secs(15);
    let hash=format!("{:x}",Sha256::digest(serde_json::to_vec(&args).unwrap()));
    let process=loop {
        match supervisor.agent_process(&agent,&hash) {
            Ok(process)=>break process,
            Err(error)=>{assert!(Instant::now()<end,"vendor direct-exec observation failed: {error:#}");std::thread::sleep(Duration::from_millis(50));}
        }
    };
    assert!(gate.check().is_err());
    let observed=loop {
        let listed=request(&lab,"agent.list",serde_json::json!({}));
        let matching:Vec<_>=listed["agents"].as_array().unwrap().iter().filter(|a|a["pane_id"]==pane).cloned().collect();
        if matching.len()==1 {break matching[0].clone();}
        assert!(Instant::now()<end,"native vendor agent recognition failed");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(observed["agent"],"codex");
    assert_ne!(observed["interactive_ready"],true,"direct layout unexpectedly acquired managed-launch readiness");
    let explain=request(&lab,"agent.explain",serde_json::json!({"target":pane}));
    assert_eq!(explain["type"],"agent_explain");
    assert_eq!(explain["explain"]["manifest_source"],"bundled");
    assert_ne!(explain["explain"]["visible_idle"],true,"unauthenticated onboarding must not certify prompt readiness");
    assert!(observed.get("name").is_none_or(serde_json::Value::is_null));
    let renamed=request(&lab,"agent.rename",serde_json::json!({"target":pane,"name":"hp-vendor-contract"}));
    assert_eq!(renamed["agent"]["name"],"hp-vendor-contract");
    assert_eq!(renamed["agent"]["terminal_id"],observed["terminal_id"]);
    process.check().unwrap();
    let identity=supervisor.identity().clone();
    lab.herdr(&["workspace","close",workspace]);
    let end=Instant::now()+Duration::from_secs(5);
    while !herdr_projects::worker_supervision::SupervisorObservation::recover_exited(&identity).unwrap() {
        assert!(Instant::now()<end,"vendor supervisor survived workspace closure");
        std::thread::sleep(Duration::from_millis(50));
    }
    eprintln!("Observed {} direct launch, native recognition/naming and namespace termination; authentication/readiness/prompt/workflow remain UNTESTED",version.trim());
}

#[test]
#[ignore = "requires HP_LIVE_HERDR; disposable workspace process-marker contract"]
#[cfg(target_os = "linux")]
fn live_workspace_bootstrap_marker_contract() {
    use std::time::{Duration,Instant};
    let mut lab=Lab::new();lab.start();
    let token="7".repeat(64);
    let environment=format!("HP_WORKSPACE_CREATION={token}");
    let created=lab.herdr(&["workspace","create","--cwd",lab.path().to_str().unwrap(),"--label","Workspace marker contract","--no-focus","--env",&environment]);
    let workspace=created["result"]["workspace"]["workspace_id"].as_str().unwrap();
    let pane=created["result"]["root_pane"]["pane_id"].as_str().unwrap();
    let deadline=Instant::now()+Duration::from_secs(5);
    let proof=loop {
        let info=lab.herdr(&["pane","process-info","--pane",pane]);
        let mut found=None;
        if let Some(processes)=info["result"]["process_info"]["foreground_processes"].as_array() {
            for process in processes {
                let pid=u32::try_from(process["pid"].as_u64().unwrap()).unwrap();
                assert!(herdr_projects::worker_supervision::ProcessMarkerObservation::observe(pid,&"8".repeat(64),lab.path()).unwrap().is_none());
                if let Some(proof)=herdr_projects::worker_supervision::ProcessMarkerObservation::observe(pid,&token,lab.path()).unwrap() {
                    found=Some(proof);break;
                }
            }
        }
        if let Some(proof)=found {break proof;}
        assert!(Instant::now()<deadline,"native bootstrap marker was not observable");
        std::thread::sleep(Duration::from_millis(50));
    };
    proof.check().unwrap();
    lab.herdr(&["workspace","close",workspace]);
    let deadline=Instant::now()+Duration::from_secs(5);
    while proof.check().is_ok() {
        assert!(Instant::now()<deadline,"bootstrap marker proof survived process exit");
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
#[ignore = "requires HP_LIVE_HERDR built with workspace.create_command compatibility patch"]
#[cfg(target_os = "linux")]
fn live_workspace_command_starts_supervised_root_without_bootstrap_shell() {
    use std::{fs, io::Write, os::unix::fs::PermissionsExt, process::Stdio, time::{Duration,Instant}};
    fn raw_request(lab:&Lab, method:&str, params:serde_json::Value)->serde_json::Value {
        let request=serde_json::json!({"id":"workspace-command","method":method,"params":params});
        let file=lab.path().join("workspace-command.json");
        writeln!(fs::File::create(&file).unwrap(),"{request}").unwrap();
        let mut cmd=lab.command(&lab.herdr);
        cmd.args(["--session","hp-acceptance","remote-api-bridge"]).stdin(Stdio::from(fs::File::open(file).unwrap()));
        let (ok,text,error)=lab.run(cmd);assert!(ok,"{error}");
        serde_json::from_str(&text).unwrap()
    }
    fn request(lab:&Lab, method:&str, params:serde_json::Value)->serde_json::Value {
        let response=raw_request(lab,method,params);
        assert!(response.get("error").is_none(),"{response}");
        response["result"].clone()
    }
    let mut lab=Lab::new();
    let marker=lab.path().join("default-shell-ran");
    let shell=lab.path().join("default-shell");
    fs::write(&shell,format!("#!/bin/sh\nprintf 'bootstrap\\n' >> '{}'\nexec /bin/sh\n",marker.display())).unwrap();
    fs::set_permissions(&shell,fs::Permissions::from_mode(0o700)).unwrap();
    let config=lab.path().join("config.toml");
    fs::write(&config,fs::read_to_string(&config).unwrap().replace("default_shell = '/bin/sh'",&format!("default_shell = '{}'",shell.display()))).unwrap();
    lab.start();
    // Positive control: prove the configured shell really writes the marker.
    let control=lab.herdr(&["workspace","create","--cwd",lab.path().to_str().unwrap(),"--no-focus"]);
    let deadline=Instant::now()+Duration::from_secs(5);
    let before=loop {
        let bytes=fs::read(&marker).unwrap_or_default();
        if !bytes.is_empty() { break bytes; }
        assert!(Instant::now()<deadline,"configured shell marker control never ran");
        std::thread::sleep(Duration::from_millis(25));
    };
    lab.herdr(&["workspace","close",control["result"]["workspace"]["workspace_id"].as_str().unwrap()]);
    let argv=herdr_projects::worker_supervision::isolated_gated_command(std::path::Path::new("/usr/bin/sleep"),&["30".into()],15,"release-root-fixture",&lab.path().join("home")).unwrap();
    let inventory=request(&lab,"workspace.list",serde_json::json!({}));
    for command in [serde_json::json!([]),serde_json::json!(["relative"]),serde_json::json!([lab.path().join("missing-executable")])] {
        let response=raw_request(&lab,"workspace.create_command",serde_json::json!({"cwd":lab.path(),"command":command}));
        assert!(response.get("error").is_some(),"invalid execution must fail: {response}");
        assert_eq!(request(&lab,"workspace.list",serde_json::json!({})),inventory,"failed execution created a workspace");
        assert_eq!(fs::read(&marker).unwrap_or_default(),before,"failed execution fell back to the configured shell");
    }
    let created=request(&lab,"workspace.create_command",serde_json::json!({"cwd":lab.path(),"command":argv,"focus":false,"label":"Supervised root","env":{}}));
    assert_eq!(created["type"],"workspace_created");
    assert_eq!(created["workspace"]["pane_count"],1);
    let workspace=created["workspace"]["workspace_id"].as_str().unwrap();
    let pane=created["root_pane"]["pane_id"].as_str().unwrap();
    let info=request(&lab,"pane.process_info",serde_json::json!({"pane_id":pane}));
    let outer=info["process_info"]["foreground_processes"].as_array().unwrap().iter().find(|p|p["argv"]==serde_json::json!(argv)).unwrap();
    let observed=herdr_projects::worker_supervision::SupervisorObservation::observe(u32::try_from(outer["pid"].as_u64().unwrap()).unwrap(),&argv).unwrap();
    observed.waiting_gate(&argv).unwrap().check().unwrap();
    assert_eq!(fs::read(&marker).unwrap_or_default(),before,"workspace creation invoked the default shell");
    let released=request(&lab,"pane.send_input",serde_json::json!({"pane_id":pane,"text":"release-root-fixture\n","keys":[]}));
    assert_eq!(released["type"],"ok");
    let identity=observed.identity().clone();
    lab.herdr(&["workspace","close",workspace]);
    let end=Instant::now()+Duration::from_secs(5);
    while !herdr_projects::worker_supervision::SupervisorObservation::recover_exited(&identity).unwrap() {
        assert!(Instant::now()<end,"supervised root survived workspace closure");
        std::thread::sleep(Duration::from_millis(25));
    }
}

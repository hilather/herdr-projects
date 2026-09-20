//! End-to-end checks of the built binary with a scrubbed environment.

use std::path::Path;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_herdr-projects");

#[test]
fn profile_probe_binds_explicit_binaries_without_launching_profile_arguments() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join(".config/herdr-projects");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("config.toml"), "[profiles.worker]\nkind='codex'\npermission_policy='interactive'\nextra_args=['SECRET']\n").unwrap();
    let herdr = home.path().join("herdr-bin");
    let agent = home.path().join("agent-bin");
    for (path, version) in [(&herdr, "herdr 0.9.1"), (&agent, "codex-cli 0.99.0-preview.3")] {
        std::fs::write(path, format!("#!/bin/sh\n[ \"$#\" = 1 ] && [ \"$1\" = --version ] || exit 2\nprintf '%s\\n' '{version}'\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let output = hp(home.path(), &["profile", "probe", "worker", "--herdr-executable", herdr.to_str().unwrap(), "--agent-executable", agent.to_str().unwrap()]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["scope"], "local_installation_only");
    assert_eq!(report["agent"]["version"], "0.99.0-preview.3");
    assert_eq!(report["herdr"]["version"], "0.9.1");
    assert_eq!(report["profile"]["agent_version"], report["agent"]["version"]);
    assert_eq!(report["profile"]["herdr_version"], report["herdr"]["version"]);
    assert_eq!(report["profile"]["launchable"], false);
    assert_eq!(report["profile"]["capabilities"]["checkpoint_acknowledgment"], "unknown");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("SECRET"));
    assert!(!home.path().join(".herdr-projects").exists());
}

#[test]
fn profile_inspection_is_redacted_read_only_and_refuses_malformed_config() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join(".config/herdr-projects");
    std::fs::create_dir_all(&config).unwrap();
    let path = config.join("config.toml");
    let text = "[profiles.worker]\nkind='codex'\npermission_policy='interactive'\nextra_args=['SECRET_VALUE']\nenvironment=['SECRET_VARIABLE']\n";
    std::fs::write(&path, text).unwrap();
    let output = hp(home.path(), &["profile", "inspect", "worker"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["kind"], "codex");
    assert_eq!(value["launchable"], false);
    assert_eq!(value["capabilities"]["resume"], "unknown");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("SECRET"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    assert!(!home.path().join(".herdr-projects").exists());
    for bad in ["SECRET = [", "[profiles.worker]\nkind='codex'\npermission_policy='interactive'\ncredential='SECRET'"] {
        std::fs::write(&path, bad).unwrap();
        let output = hp(home.path(), &["profile", "inspect", "worker"]);
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("SECRET"));
    }
}

fn hp(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(BIN)
        .env_clear()
        .env("HOME", home)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn context_prints_a_usable_prefix_in_a_scrubbed_environment() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("my root");
    let root_arg = root.to_str().unwrap();
    assert!(hp(home.path(), &["--root", root_arg, "new", "Demo"]).status.success());

    let out = hp(home.path(), &["--root", root_arg, "context", "demo", "--peek"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    let prefix = text.lines().next().unwrap().strip_prefix("Commands: ").unwrap();
    // Fixed shape `<binary> --root <root>`, with the spaced root shell-quoted.
    assert_eq!(prefix, format!("{BIN} --root '{root_arg}'"));

    // The printed prefix works as typed, from a bare shell.
    let listed = Command::new("/bin/sh")
        .env_clear()
        .env("HOME", home.path())
        .args(["-c", &format!("{prefix} list")])
        .output()
        .unwrap();
    assert!(listed.status.success());
    assert_eq!(String::from_utf8_lossy(&listed.stdout), "demo\tactive\tno threads\n");
}

#[test]
fn peek_records_nothing_and_context_records_seen_items() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("root");
    let root_arg = root.to_str().unwrap();
    assert!(hp(home.path(), &["--root", root_arg, "new", "demo"]).status.success());
    let item = "+++\nid = \"20260917T000000Z-routine-r-1\"\nkind = \"routine\"\nsubject = \"r\"\ncreated = \"x\"\nsummary = \"s\"\n+++\n";
    std::fs::write(root.join("demo/inbox/20260917T000000Z-routine-r-1.md"), item).unwrap();
    let seen = root.join("demo/.state/inbox-seen.json");

    assert!(hp(home.path(), &["--root", root_arg, "context", "demo", "--peek"]).status.success());
    assert!(!seen.exists());
    assert!(hp(home.path(), &["--root", root_arg, "context", "demo"]).status.success());
    assert!(std::fs::read_to_string(&seen).unwrap().contains("routine-r-1"));
}

#[test]
fn path_like_names_and_slugs_are_refused() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("root");
    let root_arg = root.to_str().unwrap();
    assert!(!hp(home.path(), &["--root", root_arg, "new", "../x"]).status.success());
    assert!(!hp(home.path(), &["--root", root_arg, "open", "../x"]).status.success());
    assert!(!hp(home.path(), &["--root", root_arg, "context", "../x"]).status.success());
    assert!(!hp(home.path(), &["--root", root_arg, "thread", "list", "../x"]).status.success());
    assert!(!hp(home.path(), &["--root", root_arg, "delete", "../x", "--force"]).status.success());
    assert!(!root.exists());
    assert!(!home.path().join("x").exists());
}

#[test]
fn ticker_start_without_projects_creates_nothing() {
    let home = tempfile::tempdir().unwrap();
    assert!(hp(home.path(), &["ticker", "start"]).status.success());
    assert!(!home.path().join(".herdr-projects").exists());
    assert!(!home.path().join(".config").exists());
}

#[test]
fn repair_is_explicit_hash_checked_and_preserves_original_bytes() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("root");
    let r = root.to_str().unwrap();
    assert!(hp(home.path(), &["--root", r, "new", "demo"]).status.success());
    let target = root.join("demo/threads/t-0001.toml");
    let bad = b"invalid = [";
    std::fs::write(&target, bad).unwrap();
    let output = hp(home.path(), &["--root", r, "repair", "demo", "inspect"]);
    assert!(output.status.success());
    let diagnostics: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let hash = diagnostics[0]["sha256"].as_str().unwrap();
    assert_eq!(diagnostics[0]["path"], "threads/t-0001.toml");
    assert_eq!(std::fs::read(&target).unwrap(), bad);
    let replacement = home.path().join("replacement");
    std::fs::write(&replacement, "id = 't-0002'\n").unwrap();
    let args = ["--root", r, "repair", "demo", "restore", "threads/t-0001.toml", "--from", replacement.to_str().unwrap(), "--expected-hash", hash];
    assert!(!hp(home.path(), &args).status.success());
    std::fs::write(&replacement, "id = 't-0001'\n").unwrap();
    let lock = std::fs::File::options().write(true).create(true).truncate(false).open(root.join(".ticker.lock")).unwrap();
    lock.lock().unwrap();
    assert!(!hp(home.path(), &args).status.success());
    drop(lock);
    let output = hp(home.path(), &args);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(std::fs::read(root.join(format!("demo/.state/repair-backups/{hash}.original"))).unwrap(), bad);
    assert!(!hp(home.path(), &args).status.success()); // stale inspection must not overwrite
    assert!(serde_json::from_slice::<Vec<serde_json::Value>>(&hp(home.path(), &["--root", r, "repair", "demo", "inspect"]).stdout).unwrap().is_empty());
}

#[test]
fn native_artifact_helper_needs_no_configuration_and_preserves_large_binary_payload() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("space 'λ$; source");
    std::fs::create_dir(&path).unwrap();
    let bytes = vec![255u8; 2 * 1024 * 1024];
    std::fs::write(path.join("report.md"), &bytes).unwrap();
    let output = Command::new(BIN).env_clear().args(["artifact-stream", "--probe"]).output().unwrap();
    assert!(output.status.success());
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["schema"], 1);
    let output = Command::new(BIN).env_clear().args(["artifact-stream", "--path", path.to_str().unwrap()]).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(&output.stdout[..8], b"HPAR\x01\0\0\0");
    let size = u32::from_be_bytes(output.stdout[8..12].try_into().unwrap()) as usize;
    assert_eq!(&output.stdout[12 + size..], bytes);
    std::os::unix::fs::symlink("report.md", path.join("library")).unwrap();
    assert!(!Command::new(BIN).env_clear().args(["artifact-stream", "--path", path.to_str().unwrap()]).output().unwrap().status.success());
}

#[test]
fn corrupted_project_lifecycle_is_visible_and_cannot_authorize_execution() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("root");
    let r = root.to_str().unwrap();
    assert!(hp(home.path(), &["--root", r, "new", "demo"]).status.success());
    let target = root.join("demo/.state/project.json");
    for broken in ["{corrupt", "{}", r#"{"status":"future-state"}"#] {
        std::fs::write(&target, broken).unwrap();
        let listed = hp(home.path(), &["--root", r, "list"]);
        assert!(String::from_utf8_lossy(&listed.stdout).contains("invalid"));
        assert!(!hp(home.path(), &["--root", r, "open", "demo"]).status.success());
        assert!(!hp(home.path(), &["--root", r, "resume", "demo"]).status.success());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), broken);
        let diagnostics: serde_json::Value = serde_json::from_slice(&hp(home.path(), &["--root", r, "repair", "demo", "inspect"]).stdout).unwrap();
        assert_eq!(diagnostics[0]["path"], ".state/project.json");
        let replacement = home.path().join("replacement.json");
        std::fs::write(&replacement, r#"{"status":"paused"}"#).unwrap();
        assert!(hp(home.path(), &["--root", r, "repair", "demo", "restore", ".state/project.json", "--from", replacement.to_str().unwrap(), "--expected-hash", diagnostics[0]["sha256"].as_str().unwrap()]).status.success());
        assert!(String::from_utf8_lossy(&hp(home.path(), &["--root", r, "list"]).stdout).contains("paused"));
    }
}

#[test]
fn legacy_commands_refuse_store_ownership_even_without_feature() {
    let home=tempfile::tempdir().unwrap(); let root=home.path().join("root"); let root_arg=root.to_str().unwrap();
    assert!(hp(home.path(),&["--root",root_arg,"new","demo"]).status.success());
    let state=root.join("demo/.state/project.json"); let before=std::fs::read(&state).unwrap();
    std::fs::write(root.join("demo/.state/format.json"),b"unknown or interrupted format").unwrap();
    for command in ["resume","pause","open","context"] {
        let out=hp(home.path(),&["--root",root_arg,command,"demo"]);
        assert!(!out.status.success()); assert!(String::from_utf8_lossy(&out.stderr).contains("legacy runtime is disabled"));
    }
    assert_eq!(std::fs::read(state).unwrap(),before);
    let listed=hp(home.path(),&["--root",root_arg,"list"]); assert!(listed.status.success());
    assert!(String::from_utf8_lossy(&listed.stdout).contains("store/maintenance"));
}

#[test]
#[cfg(feature="state-store")]
fn migration_cli_round_trip_keeps_memory_and_blocks_legacy_mutation() {
    let home=tempfile::tempdir().unwrap(); let root=home.path().join("root"); let root_arg=root.to_str().unwrap();
    for args in [vec!["--root",root_arg,"new","demo"],vec!["--root",root_arg,"pause","demo"]] {
        let out=hp(home.path(),&args);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    }
    let memory=std::fs::read(root.join("demo/MEMORY.md")).unwrap();
    let plan=home.path().join("plan.json"); let plan_arg=plan.to_str().unwrap();
    for args in [vec!["plan","--output",plan_arg],vec!["apply","--plan",plan_arg,"--writers-stopped"],vec!["status"],vec!["export"],vec!["recover","--writers-stopped"]] {
        let mut full=vec!["--root",root_arg,"migration","demo"];full.extend(args);
        let out=hp(home.path(),&full);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    }
    assert_eq!(std::fs::read(root.join("demo/MEMORY.md")).unwrap(),memory);
    assert!(!hp(home.path(),&["--root",root_arg,"resume","demo"]).status.success());
    let restore=home.path().join("restored");
    let out=hp(home.path(),&["--root",root_arg,"migration","demo","restore","--destination",restore.to_str().unwrap()]);
    assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));assert!(restore.join("PROJECT.md").is_file());
}
#[test]
#[cfg(feature="state-store")]
fn migrated_task_commands_use_revisions_and_do_not_touch_legacy_task_file() {
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();
    for args in [vec!["--root",root_arg,"new","demo"],vec!["--root",root_arg,"pause","demo"]] {assert!(hp(home.path(),&args).status.success());}
    let plan=home.path().join("plan.json");
    for args in [vec!["plan","--output",plan.to_str().unwrap()],vec!["apply","--plan",plan.to_str().unwrap(),"--writers-stopped"],vec!["upgrade-store"]] {
        let mut full=vec!["--root",root_arg,"migration","demo"];full.extend(args);let out=hp(home.path(),&full);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    }
    let original=std::fs::read(root.join("demo/TASKS.md")).unwrap();
    let out=hp(home.path(),&["--root",root_arg,"task","demo","list"]);assert!(out.status.success());
    let snapshot:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();let head=snapshot["head"].as_u64().unwrap().to_string();
    let args=["--root",root_arg,"task","demo","add","operator-task","--title","Keep original","--expected-head",&head];
    let out=hp(home.path(),&args);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));assert!(!hp(home.path(),&args).status.success());
    let out=hp(home.path(),&["--root",root_arg,"context","demo","--peek"]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));assert!(String::from_utf8_lossy(&out.stdout).contains("Runtime owner: SQLite"));
    let out=hp(home.path(),&["--root",root_arg,"migration","demo","recover","--writers-stopped"]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    assert_eq!(std::fs::read(root.join("demo/TASKS.md")).unwrap(),original);
    let out=hp(home.path(),&["--root",root_arg,"operations","demo","inspect"]);assert!(out.status.success());assert_eq!(serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap(),serde_json::json!([]));
    assert!(!hp(home.path(),&["--root",root_arg,"resume","demo"]).status.success());
}
#[test]
#[cfg(feature="state-store")]
fn migrated_inbox_cli_drains_once_marks_seen_and_keeps_legacy_files_untouched() {
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();
    for args in [vec!["--root",root_arg,"new","demo"],vec!["--root",root_arg,"pause","demo"]] {assert!(hp(home.path(),&args).status.success());}
    let ticker=root.join("demo/.state/ticker.json");let bytes=br#"{"pending_events":{"notice":{"id":"notice","kind":"notice","subject":"s","summary":"summary","body":"  body"}}}"#;std::fs::write(&ticker,bytes).unwrap();
    let plan=home.path().join("plan.json");for args in [vec!["plan","--output",plan.to_str().unwrap()],vec!["apply","--plan",plan.to_str().unwrap(),"--writers-stopped"]] {let mut full=vec!["--root",root_arg,"migration","demo"];full.extend(args);let out=hp(home.path(),&full);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));}
    let out=hp(home.path(),&["--root",root_arg,"task","demo","list"]);let snapshot:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();let head=snapshot["head"].as_u64().unwrap().to_string();
    let out=hp(home.path(),&["--root",root_arg,"operations","demo","drain-inbox","--expected-head",&head]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    let out=hp(home.path(),&["--root",root_arg,"context","demo","--peek"]);assert!(out.status.success());assert!(String::from_utf8_lossy(&out.stdout).contains("  body"));
    let out=hp(home.path(),&["--root",root_arg,"inbox","list","demo"]);let items:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();assert_eq!(items[0]["seen"],false);
    assert!(hp(home.path(),&["--root",root_arg,"context","demo"]).status.success());
    assert!(hp(home.path(),&["--root",root_arg,"inbox","done","demo","notice"]).status.success());
    let out=hp(home.path(),&["--root",root_arg,"inbox","list","demo"]);let items:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();assert_eq!(items[0]["seen"],true);assert_eq!(items[0]["done"],true);
    assert_eq!(std::fs::read(ticker).unwrap(),bytes);assert!(!root.join("demo/inbox/notice.md").exists());
}
#[test]
#[cfg(feature="state-store")]
fn preflight_refuses_fifo_and_oversized_external_config_without_hanging() {
    use std::{ffi::CString,os::unix::ffi::OsStrExt,process::Stdio,time::{Duration,Instant}};
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();
    assert!(hp(home.path(),&["--root",root_arg,"new","demo"]).status.success());assert!(hp(home.path(),&["--root",root_arg,"pause","demo"]).status.success());
    let config=home.path().join(".config/herdr-projects/config.toml");std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    for fifo in [true,false] {
        if fifo {let path=CString::new(config.as_os_str().as_bytes()).unwrap();
            // SAFETY: valid NUL-terminated disposable path, permissions only.
            assert_eq!(unsafe{libc::mkfifo(path.as_ptr(),0o600)},0);
        } else {std::fs::File::create(&config).unwrap().set_len(16*1024*1024+1).unwrap();}
        let output=std::fs::File::create(home.path().join("preflight.json")).unwrap();
        let mut child=Command::new(BIN).env_clear().env("HOME",home.path()).args(["--root",root_arg,"migration","demo","preflight"]).stdout(output).stderr(Stdio::inherit()).spawn().unwrap();
        let deadline=Instant::now()+Duration::from_secs(5);
        loop {if let Some(status)=child.try_wait().unwrap(){assert!(status.success());break;}if Instant::now()>deadline{let _=child.kill();let _=child.wait();panic!("preflight blocked on config");}std::thread::sleep(Duration::from_millis(10));}
        let report:serde_json::Value=serde_json::from_slice(&std::fs::read(home.path().join("preflight.json")).unwrap()).unwrap();assert!(report["blockers"].as_array().unwrap().iter().any(|v|v.as_str().unwrap().contains("config.toml")));
        std::fs::remove_file(&config).unwrap();
    }
}

#[test]
#[cfg(feature="state-store")]
fn migration_plan_binds_config_and_rejects_legacy_unbound_plans() {
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();
    for command in ["new","pause"] {assert!(hp(home.path(),&["--root",root_arg,command,"demo"]).status.success());}
    let config=home.path().join(".config/herdr-projects/config.toml");std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    let bytes=b"private='do-not-print-this-value'\n";std::fs::write(&config,bytes).unwrap();
    let plan=home.path().join("plan.json");
    let out=hp(home.path(),&["--root",root_arg,"migration","demo","plan","--output",plan.to_str().unwrap()]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    let encoded=std::fs::read(&plan).unwrap();assert!(!String::from_utf8_lossy(&encoded).contains("do-not-print-this-value"));
    std::fs::write(&config,b"private='changed'\n").unwrap();
    let args=["--root",root_arg,"migration","demo","apply","--plan",plan.to_str().unwrap(),"--writers-stopped"];
    assert!(!hp(home.path(),&args).status.success());assert!(!root.join("demo/.state/migration").exists());
    std::fs::write(&config,bytes).unwrap();
    let mut unbound:serde_json::Value=serde_json::from_slice(&encoded).unwrap();unbound["version"]=serde_json::json!(1);unbound.as_object_mut().unwrap().remove("config");
    std::fs::write(&plan,serde_json::to_vec(&unbound).unwrap()).unwrap();
    let out=hp(home.path(),&args);assert!(!out.status.success());assert!(String::from_utf8_lossy(&out.stderr).contains("regenerate"));
    std::fs::write(&plan,encoded).unwrap();assert!(hp(home.path(),&args).status.success());
    assert_eq!(std::fs::read(config).unwrap(),bytes);
}

#[test]
fn root_config_special_files_fail_promptly_without_an_explicit_root() {
    use std::{ffi::CString,os::unix::ffi::OsStrExt,process::Stdio,time::{Duration,Instant}};
    let home=tempfile::tempdir().unwrap();let config=home.path().join(".config/herdr-projects/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    for fifo in [true,false] {
        if fifo {let path=CString::new(config.as_os_str().as_bytes()).unwrap();
            // SAFETY: valid NUL-terminated path in a disposable directory.
            assert_eq!(unsafe{libc::mkfifo(path.as_ptr(),0o600)},0);
        } else {std::fs::File::create(&config).unwrap().set_len(16*1024*1024+1).unwrap();}
        let mut child=Command::new(BIN).env_clear().env("HOME",home.path()).arg("list").stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        let deadline=Instant::now()+Duration::from_secs(5);
        loop {if let Some(status)=child.try_wait().unwrap(){assert!(!status.success());break;}if Instant::now()>deadline{let _=child.kill();let _=child.wait();panic!("root resolution blocked on config");}std::thread::sleep(Duration::from_millis(10));}
        std::fs::remove_file(&config).unwrap();
    }
}

#[test]
#[cfg(feature="state-store")]
fn operation_expiry_is_visible_idempotent_and_never_dispatches() {
    use herdr_projects::{migration,operations::{Outcome,DeliveryState}};
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();
    for command in ["new","pause"] {assert!(hp(home.path(),&["--root",root_arg,command,"demo"]).status.success());}
    let project=root.join("demo");
    std::fs::write(project.join(".state/ticker.json"),br#"{"notification_retry":{"hash":"fixture-hash"}}"#).unwrap();
    let plan=migration::inspect_with_config(&project,&home.path().join(".config/herdr-projects/config.toml")).unwrap();migration::apply(&project,&plan,true).unwrap();
    let mut db=migration::open_active(&project).unwrap();let imported=db.deliveries().unwrap().remove(0);
    let pending=db.observe_operation(&imported.operation,imported.revision,"test-fixture",Outcome::Retryable{no_effect_evidence:"disposable fake effect never attempted".into()},0).unwrap();
    db.claim_operation(&pending.operation,pending.revision,"crashed-fixture",pending.next_due_ms,1).unwrap();drop(db);
    for expected in ["1 expired","0 expired"] {
        let out=hp(home.path(),&["--root",root_arg,"operations","demo","expire"]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));assert!(String::from_utf8_lossy(&out.stdout).contains(expected));
    }
    assert_eq!(migration::open_active(&project).unwrap().deliveries().unwrap()[0].state,DeliveryState::Ambiguous);
}

#[test]
#[cfg(feature="state-store")]
fn imported_receipt_cli_previews_and_confirms_only_matching_completion() {
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();
    for command in ["new","pause"] {assert!(hp(home.path(),&["--root",root_arg,command,"demo"]).status.success());}
    let project=root.join("demo");let bytes=br#"{"nudged":"fixture-hash","notification_retry":{"hash":"fixture-hash"}}"#;
    std::fs::write(project.join(".state/ticker.json"),bytes).unwrap();
    let plan=herdr_projects::migration::inspect(&project).unwrap();herdr_projects::migration::apply(&project,&plan,true).unwrap();
    let out=hp(home.path(),&["--root",root_arg,"operations","demo","receipt-plan"]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    let report:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();assert_eq!(report["confirmed"],0);assert!(report["observations"][0]["receipt"].is_string());let head=report["head"].to_string();
    let args=["--root",root_arg,"operations","demo","observe-imported","--expected-head",&head];
    let out=hp(home.path(),&args);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));assert_eq!(serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["confirmed"],1);
    assert!(!hp(home.path(),&args).status.success());assert_eq!(std::fs::read(project.join(".state/ticker.json")).unwrap(),bytes);
}

#[test]
#[cfg(feature="state-store")]
fn migrated_runtime_bindings_require_explicit_upgrade_and_are_unverified() {
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();
    for command in ["new","pause"] {assert!(hp(home.path(),&["--root",root_arg,command,"demo"]).status.success());}
    let project=root.join("demo");std::fs::write(project.join("threads/t-0001.toml"),"id='t-0001'\nstatus='resolved'\nrepo='/repo'\n").unwrap();
    let plan=herdr_projects::migration::inspect(&project).unwrap();herdr_projects::migration::apply(&project,&plan,true).unwrap();
    let raw=rusqlite::Connection::open(project.join(".state/state.db")).unwrap();raw.execute_batch("DROP TABLE approval_uses; DROP TABLE approval_revocations; DROP TABLE approval_grants; DROP TRIGGER operation_delivery_monotonic; DROP TABLE attempt_cancellations; DROP TABLE attempt_inputs; DROP TABLE task_dependencies; DROP TABLE task_queue; DROP TABLE scheduler_policy; DROP TABLE runtime_ownership; DROP TABLE project_control; DROP TABLE runtime_observations; DROP TABLE runtime_bindings; UPDATE store_meta SET schema_version=4; PRAGMA user_version=4;").unwrap();drop(raw);
    let args=["--root",root_arg,"migration","demo","bindings"];
    let out=hp(home.path(),&args);assert!(!out.status.success());assert!(String::from_utf8_lossy(&out.stderr).contains("upgrade-store"));
    assert!(hp(home.path(),&["--root",root_arg,"migration","demo","upgrade-store"]).status.success());
    let out=hp(home.path(),&args);assert!(out.status.success());let view:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();assert_eq!(view["bindings"][0]["verification"],"unverified");assert_eq!(view["bindings"][0]["identity"]["repo"],"/repo");
    assert!(!hp(home.path(),&["--root",root_arg,"resume","demo"]).status.success());
}

#[test]
#[cfg(feature="state-store")]
fn reconciliation_cli_records_unrecorded_identity_without_authorizing_execution() {
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();
    for command in ["new","pause"] {assert!(hp(home.path(),&["--root",root_arg,command,"demo"]).status.success());}
    let project=root.join("demo");std::fs::write(project.join("threads/t-1.toml"),"id='t-1'\nstatus='resolved'\n").unwrap();
    let plan=herdr_projects::migration::inspect(&project).unwrap();herdr_projects::migration::apply(&project,&plan,true).unwrap();
    let before=herdr_projects::runtime::snapshot(&project).unwrap();
    let out=hp(home.path(),&["--root",root_arg,"reconcile","demo"]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));assert_eq!(herdr_projects::runtime::snapshot(&project).unwrap(),before);
    let out=hp(home.path(),&["--root",root_arg,"reconcile","demo","--record"]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));let report:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();assert_eq!(report["dispatch_allowed"],false);assert!(report["recorded_head"].is_number());assert_eq!(report["observations"][0]["pane"],"unrecorded");
    let after=herdr_projects::runtime::snapshot(&project).unwrap();assert_eq!(after.tasks,before.tasks);assert_eq!(after.observations.len(),1);assert!(!hp(home.path(),&["--root",root_arg,"resume","demo"]).status.success());
}

#[test]
#[cfg(feature="state-store")]
fn runtime_rebind_cli_requires_revisions_and_retains_legacy_bytes() {
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();
    for command in ["new","pause"] {assert!(hp(home.path(),&["--root",root_arg,command,"demo"]).status.success());}
    let project=root.join("demo");let bytes=b"id='t-1'\nstatus='resolved'\n";std::fs::write(project.join("threads/t-1.toml"),bytes).unwrap();let plan=herdr_projects::migration::inspect(&project).unwrap();herdr_projects::migration::apply(&project,&plan,true).unwrap();
    let out=hp(home.path(),&["--root",root_arg,"runtime","demo","inspect"]);assert!(out.status.success());let view:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();let head=view["head"].to_string();
    let path=home.path().join("route.json");std::fs::write(&path,br#"{"socket":"/recorded.sock","workspace_id":"w","tab_id":"t","pane_id":"p","cwd":"/cwd"}"#).unwrap();
    let args=["--root",root_arg,"runtime","demo","rebind","thread:t-1","--route",path.to_str().unwrap(),"--expected-revision","1","--expected-head",&head];
    let out=hp(home.path(),&args);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));let result:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();assert_eq!(result["binding"]["revision"],2);assert_eq!(result["binding"]["verification"],"unverified");assert!(!hp(home.path(),&args).status.success());assert_eq!(std::fs::read(project.join("threads/t-1.toml")).unwrap(),bytes);
}

#[test]
#[cfg(feature="state-store")]
fn canonical_lifecycle_cli_does_not_dual_write_legacy_status() {
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();
    for command in ["new","pause"] {assert!(hp(home.path(),&["--root",root_arg,command,"demo"]).status.success());}
    let project=root.join("demo");let legacy=std::fs::read(project.join(".state/project.json")).unwrap();let plan=herdr_projects::migration::inspect(&project).unwrap();herdr_projects::migration::apply(&project,&plan,true).unwrap();
    for state in ["active","paused","archived"] {
        if state=="paused" {let config=home.path().join(".config/herdr-projects/config.toml");std::fs::create_dir_all(config.parent().unwrap()).unwrap();std::fs::write(config,"invalid config [").unwrap();}
        let snapshot=herdr_projects::runtime::snapshot(&project).unwrap();let head=snapshot.head.to_string();let revision=snapshot.control.as_ref().unwrap().revision.to_string();
        let out=hp(home.path(),&["--root",root_arg,"runtime","demo","state",state,"--expected-head",&head,"--expected-revision",&revision]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));let result:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();assert_eq!(result["control"]["state"],state);
    }
    assert_eq!(std::fs::read(project.join(".state/project.json")).unwrap(),legacy);assert!(hp(home.path(),&["--root",root_arg,"migration","demo","recover","--writers-stopped"]).status.success());
}

#[test]
#[cfg(feature="state-store")]
fn canonical_runtime_create_cli_requires_task_fences_and_does_not_forge_legacy_files() {
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();
    for command in ["new","pause"] {assert!(hp(home.path(),&["--root",root_arg,command,"demo"]).status.success());}
    let project=root.join("demo");let plan=herdr_projects::migration::inspect(&project).unwrap();herdr_projects::migration::apply(&project,&plan,true).unwrap();let task=herdr_projects::domain::TaskId::new("created").unwrap();let before=herdr_projects::runtime::snapshot(&project).unwrap();let head=herdr_projects::runtime::add_task(&project,task,"created task".into(),before.head).unwrap().to_string();
    let route=home.path().join("route.json");std::fs::write(&route,b"{}").unwrap();
    let args=["--root",root_arg,"runtime","demo","create","--task","created","--task-revision","1","--expected-head",&head,"--route",route.to_str().unwrap()];let out=hp(home.path(),&args);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));let result:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();assert_eq!(result["binding"]["id"],"task:created");assert!(result["binding"]["source_path"].is_null());assert_eq!(result["task_revision"],2);assert!(!hp(home.path(),&args).status.success());assert!(!project.join("threads/created.toml").exists());
    let head=result["head"].to_string();let out=hp(home.path(),&["--root",root_arg,"runtime","demo","create","--expected-head",&head,"--route",route.to_str().unwrap()]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));assert_eq!(serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["binding"]["id"],"coordinator");assert!(!project.join(".state/coordinator.json").exists());
}

#[test]
#[cfg(feature="state-store")]
fn canonical_notification_cli_delivers_once_to_recorded_socket() {
    use std::os::unix::fs::PermissionsExt;
    use herdr_projects::{domain::{TaskId,RuntimeRoute,ProjectState},migration,runtime};
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();
    for command in ["new","pause"] {assert!(hp(home.path(),&["--root",root_arg,command,"demo"]).status.success());}
    let project=root.join("demo");std::fs::write(project.join("inbox/message.md"),"+++\nid='message'\nsummary='private text'\n+++\n").unwrap();let plan=migration::inspect(&project).unwrap();migration::apply(&project,&plan,true).unwrap();
    let head=runtime::snapshot(&project).unwrap().head;let task=TaskId::new("notification").unwrap();let head=runtime::add_task(&project,task.clone(),"notification".into(),head).unwrap();runtime::create_binding(&project,None,None,head,&RuntimeRoute{socket:"/explicit/notification.sock".into(),..Default::default()}).unwrap();assert!(hp(home.path(),&["--root",root_arg,"reconcile","demo","--record"]).status.success());let snapshot=runtime::snapshot(&project).unwrap();runtime::set_state(&project,snapshot.head,snapshot.control.unwrap().revision,ProjectState::Active,&home.path().join(".config/herdr-projects/config.toml")).unwrap();
    let head=runtime::snapshot(&project).unwrap().head.to_string();let out=hp(home.path(),&["--root",root_arg,"operations","demo","notify","notification","--expected-head",&head]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));let op:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();let id=op["id"].as_str().unwrap();
    let fake=home.path().join(".local/bin/herdr");std::fs::create_dir_all(fake.parent().unwrap()).unwrap();std::fs::write(&fake,b"#!/bin/sh\nif [ \"$1\" = '--version' ]; then echo 'herdr 0.9.1'; exit 0; fi\n[ \"$HERDR_SOCKET_PATH\" = '/explicit/notification.sock' ] || exit 8\n[ \"$1\" = notification ] && [ \"$2\" = show ] || exit 9\nprintf 'effect\\n' >> \"$HOME/effects\"\nprintf '%s\\n' '{\"result\":{\"shown\":true}}'\n").unwrap();std::fs::set_permissions(&fake,std::fs::Permissions::from_mode(0o700)).unwrap();
    let deliver=|args:&[&str]|Command::new(BIN).env_clear().env("HOME",home.path()).env("HERDR_BIN_PATH",&fake).args(args).output().unwrap();
    let args=["--root",root_arg,"operations","demo","deliver-notification",id,"--expected-revision","1"];let out=deliver(&args);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));assert_eq!(serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["state"],"confirmed");assert!(!deliver(&args).status.success());assert_eq!(std::fs::read_to_string(home.path().join("effects")).unwrap(),"effect\n");let snapshot=runtime::snapshot(&project).unwrap();assert!(!snapshot.inbox[0].seen&&!snapshot.inbox[0].done);
}

#[test]
#[cfg(feature="state-store")]
fn canonical_finalization_cli_preserves_artifacts_and_awaits_review() {
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();for command in ["new","pause"] {assert!(hp(home.path(),&["--root",root_arg,command,"demo"]).status.success());}
    let project=root.join("demo");let source=home.path().join("source");std::fs::create_dir_all(source.join("library")).unwrap();std::fs::write(source.join("report.md"),"report for review\n").unwrap();std::fs::write(source.join("library/result"),"result bytes").unwrap();let original=format!("id='t-0001'\nstatus='resolved'\nthread_dir={}\n",serde_json::to_string(source.to_str().unwrap()).unwrap());std::fs::write(project.join("threads/t-0001.toml"),&original).unwrap();let plan=herdr_projects::migration::inspect(&project).unwrap();herdr_projects::migration::apply(&project,&plan,true).unwrap();let head=herdr_projects::runtime::snapshot(&project).unwrap().head.to_string();
    let out=hp(home.path(),&["--root",root_arg,"operations","demo","finalize","thread:t-0001","--reason","operator review","--expected-head",&head]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));let op:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();let id=op["id"].as_str().unwrap();let args=["--root",root_arg,"operations","demo","deliver-finalization",id,"--expected-revision","1"];let out=hp(home.path(),&args);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));assert_eq!(serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["state"],"confirmed");assert!(!hp(home.path(),&args).status.success());let snapshot=herdr_projects::runtime::snapshot(&project).unwrap();let task=snapshot.tasks.iter().find(|t|t.id.as_str()=="legacy-t-0001").unwrap();assert_eq!(task.state,herdr_projects::domain::TaskState::AwaitingReview);assert_eq!(task.revision,2);assert_eq!(std::fs::read_to_string(project.join("threads/t-0001.toml")).unwrap(),original);assert!(source.join("report.md").is_file());
}

#[test]
#[cfg(feature="state-store")]
fn canonical_ownership_cli_adopts_recorded_coordinator_without_prompting() {
    use std::os::unix::{fs::PermissionsExt,net::UnixListener};use herdr_projects::{migration,runtime,domain::RuntimeRoute};
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();for command in ["new","pause"] {assert!(hp(home.path(),&["--root",root_arg,command,"demo"]).status.success());}
    let project=root.join("demo");let plan=migration::inspect(&project).unwrap();migration::apply(&project,&plan,true).unwrap();let socket=home.path().join("fixture.sock");let _listener=UnixListener::bind(&socket).unwrap();let head=runtime::snapshot(&project).unwrap().head;runtime::create_binding(&project,None,None,head,&RuntimeRoute{socket:socket.to_str().unwrap().into(),workspace_id:"w".into(),tab_id:"t".into(),pane_id:"p".into(),cwd:project.to_str().unwrap().into(),..Default::default()}).unwrap();
    std::fs::write(home.path().join("panes.json"),serde_json::json!({"result":{"panes":[{"pane_id":"p","tab_id":"t","workspace_id":"w","cwd":project}]}}).to_string()).unwrap();std::fs::write(home.path().join("agents.json"),serde_json::json!({"result":{"agents":[{"pane_id":"p","tab_id":"t","workspace_id":"w","cwd":project,"agent":"claude","name":"coordinator","agent_status":"working"}]}}).to_string()).unwrap();let fake=home.path().join("fake-herdr");std::fs::write(&fake,b"#!/bin/sh\ncase \"$1 $2\" in\n'--version ') echo 'herdr 0.9.1';;\n'pane list') cat \"$HOME/panes.json\";;\n'agent list') cat \"$HOME/agents.json\";;\n*) exit 99;;\nesac\n").unwrap();std::fs::set_permissions(&fake,std::fs::Permissions::from_mode(0o700)).unwrap();let command=|args:&[&str]|Command::new(BIN).env_clear().env("HOME",home.path()).env("HERDR_BIN_PATH",&fake).args(args).output().unwrap();
    let before=runtime::snapshot(&project).unwrap();let out=command(&["--root",root_arg,"reconcile","demo","--plan"]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));let plan:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();assert_eq!(plan["dispatch_allowed"],false);assert!(plan["items"].as_array().unwrap().iter().any(|i|i["action"]=="adopt_resources"));assert_eq!(runtime::snapshot(&project).unwrap(),before);assert!(!command(&["--root",root_arg,"reconcile","demo","--plan","--record"]).status.success());
    let head=runtime::snapshot(&project).unwrap().head.to_string();let out=command(&["--root",root_arg,"runtime","demo","adopt","coordinator","--expected-revision","1","--expected-head",&head]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));let change:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();assert_eq!(change["ownership"]["origin"],"adopted");assert!(change["ownership"]["attempt"].is_null());assert!(command(&["--root",root_arg,"reconcile","demo","--record"]).status.success());let snapshot=runtime::snapshot(&project).unwrap();let out=command(&["--root",root_arg,"runtime","demo","state","active","--expected-head",&snapshot.head.to_string(),"--expected-revision",&snapshot.control.unwrap().revision.to_string()]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));assert!(runtime::snapshot(&project).unwrap().attempts.is_empty());
    let before=runtime::snapshot(&project).unwrap();let out=command(&["--root",root_arg,"runtime","demo","relinquish","coordinator","--expected-revision","1","--expected-head",&before.head.to_string(),"--reason","hand back"]);assert!(!out.status.success());assert_eq!(runtime::snapshot(&project).unwrap(),before);
    runtime::set_state(&project,before.head,before.control.unwrap().revision,herdr_projects::domain::ProjectState::Paused,&home.path().join("config.toml")).unwrap();let head=runtime::snapshot(&project).unwrap().head;let out=command(&["--root",root_arg,"runtime","demo","relinquish","coordinator","--expected-revision","1","--expected-head",&head.to_string(),"--reason","hand back"]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));assert!(runtime::snapshot(&project).unwrap().ownership.is_empty());assert!(socket.exists());

}

#[cfg(feature="state-store")]
#[test]
fn scheduler_cli_queues_dependencies_without_launching_or_rewriting_legacy_tasks() {
    use herdr_projects::{migration,runtime,domain::TaskId};
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();for command in ["new","pause"] {assert!(hp(home.path(),&["--root",root_arg,command,"demo"]).status.success());}
    let project=root.join("demo");let original=std::fs::read(project.join("TASKS.md")).unwrap();let plan=migration::inspect(&project).unwrap();migration::apply(&project,&plan,true).unwrap();for id in ["a","b"] {runtime::add_task(&project,TaskId::new(id).unwrap(),id.into(),runtime::snapshot(&project).unwrap().head).unwrap();}
    let request=home.path().join("queue.json");std::fs::write(&request,r#"{"priority":2,"dependencies":[{"predecessor":"b","requirement":"landed_commit"}]}"#).unwrap();let before=runtime::snapshot(&project).unwrap();let out=hp(home.path(),&["--root",root_arg,"task","demo","queue","a","--input-file",request.to_str().unwrap(),"--expected-revision","1","--expected-head",&before.head.to_string()]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    let head=runtime::snapshot(&project).unwrap().head;let out=hp(home.path(),&["--root",root_arg,"scheduler","demo","policy","--max-active-workers","2","--max-attempts-per-task","3","--expected-revision","1","--expected-head",&head.to_string()]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));let out=hp(home.path(),&["--root",root_arg,"scheduler","demo","inspect"]);assert!(out.status.success());let report:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();assert_eq!(report["available_slots"],2);assert_eq!(report["launch_enabled"],false);assert!(report["entries"][0]["blockers"].as_array().unwrap().iter().any(|s|s.as_str().unwrap().contains("verified_dependency_evidence_unavailable:b:landed_commit")));
    std::fs::write(&request,r#"{"priority":0,"dependencies":[{"predecessor":"a","requirement":"verified_result"}]}"#).unwrap();let before=runtime::snapshot(&project).unwrap();let out=hp(home.path(),&["--root",root_arg,"task","demo","queue","b","--input-file",request.to_str().unwrap(),"--expected-revision","1","--expected-head",&before.head.to_string()]);assert!(!out.status.success());assert_eq!(runtime::snapshot(&project).unwrap(),before);assert!(before.attempts.is_empty());assert!(before.operations.is_empty());assert_eq!(std::fs::read(project.join("TASKS.md")).unwrap(),original);
}

#[cfg(feature="state-store")]
#[test]
fn cancellation_cli_audits_request_without_releasing_an_unproven_worker() {
    use herdr_projects::{migration,runtime,domain::*};
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();for command in ["new","pause"] {assert!(hp(home.path(),&["--root",root_arg,command,"demo"]).status.success());}
    let project=root.join("demo");let plan=migration::inspect(&project).unwrap();migration::apply(&project,&plan,true).unwrap();let head=runtime::snapshot(&project).unwrap().head;runtime::add_task(&project,TaskId::new("a").unwrap(),"task".into(),head).unwrap();let head=runtime::snapshot(&project).unwrap().head;let mut db=migration::open_active(&project).unwrap();db.commit(Commit{expected_head:head,mutations:vec![Mutation::Attempt{expected:None,next:Attempt{id:AttemptId::new("adopted").unwrap(),task:TaskId::new("a").unwrap(),revision:1,state:AttemptState::Lost,snapshot:None,reservation:"fixture".into(),termination_observed:false}}]}).unwrap();drop(db);
    let before=runtime::snapshot(&project).unwrap();let args=["--root",root_arg,"task","demo","cancel-attempt","adopted","--expected-revision","1","--expected-head",&before.head.to_string(),"--reason","operator stop request"];let output=hp(home.path(),&args);assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stderr));let result:serde_json::Value=serde_json::from_slice(&output.stdout).unwrap();assert_eq!(result["released"],false);let after=runtime::snapshot(&project).unwrap();assert!(after.attempts[0].retains_capacity());assert_eq!(after.cancellations.len(),1);assert!(!hp(home.path(),&args).status.success());assert_eq!(runtime::snapshot(&project).unwrap(),after);
}

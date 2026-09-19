//! End-to-end checks of the built binary with a scrubbed environment.

use std::path::Path;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_herdr-projects");

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
    let raw=rusqlite::Connection::open(project.join(".state/state.db")).unwrap();raw.execute_batch("DROP TABLE runtime_bindings; UPDATE store_meta SET schema_version=4; PRAGMA user_version=4;").unwrap();drop(raw);
    let args=["--root",root_arg,"migration","demo","bindings"];
    let out=hp(home.path(),&args);assert!(!out.status.success());assert!(String::from_utf8_lossy(&out.stderr).contains("upgrade-store"));
    assert!(hp(home.path(),&["--root",root_arg,"migration","demo","upgrade-store"]).status.success());
    let out=hp(home.path(),&args);assert!(out.status.success());let view:serde_json::Value=serde_json::from_slice(&out.stdout).unwrap();assert_eq!(view["bindings"][0]["verification"],"unverified");assert_eq!(view["bindings"][0]["identity"]["repo"],"/repo");
    assert!(!hp(home.path(),&["--root",root_arg,"resume","demo"]).status.success());
}

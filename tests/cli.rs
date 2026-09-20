//! End-to-end checks of the built binary with a scrubbed environment.

use std::path::Path;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_herdr-projects");

#[cfg(feature="state-store")]
#[test]
fn signed_routine_cli_records_then_explicitly_executes_once_with_durable_cleanup() {
    use herdr_projects::{domain::*,authority,migration,runtime};
    use sha2::{Digest,Sha256};
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let root_arg=root.to_str().unwrap();
    for action in ["new","pause"] {assert!(hp(home.path(),&["--root",root_arg,action,"demo"]).status.success());}
    let key=home.path().join("owner");let output=Command::new("/usr/bin/ssh-keygen").args(["-q","-t","ed25519","-N","","-f"]).arg(&key).output().unwrap();assert!(output.status.success());
    let public=std::fs::read_to_string(key.with_extension("pub")).unwrap().split_whitespace().take(2).collect::<Vec<_>>().join(" ");
    let project=root.join("demo");let config=home.path().join("owner.toml");
    std::fs::write(&config,format!("[authority]\nversion=1\nrevision=1\napproval_public_key={public:?}\n[safety.{:?}]\nroutine_commands=true\n",project.display().to_string())).unwrap();
    let plan=migration::inspect_with_config(&project,&config).unwrap();migration::apply(&project,&plan,true).unwrap();
    let s=runtime::snapshot(&project).unwrap();runtime::set_state(&project,s.head,s.control.unwrap().revision,ProjectState::Active,&config).unwrap();
    let script=project.join("check.sh");let bytes=b"printf once >> ROUTINE_MARKER\nprintf 'result\\033[31m'\n";std::fs::write(&script,bytes).unwrap();
    let definition=RoutineDefinition{version:1,name:"check".into(),revision:1,project_store:project.join(".state/state.db").canonicalize().unwrap().display().to_string(),
        authority:authority::policy_reference(&project).unwrap(),config:migration::config_reference(&config).unwrap(),enabled:true,schedule:"every 1m".into(),timezone:"UTC".into(),start_unix_ms:0,
        missed:MissedRunPolicy::CoalesceLatest,overlap:OverlapPolicy::Skip,script:script.display().to_string(),script_sha256:format!("{:x}",Sha256::digest(bytes)),cwd:project.display().to_string(),deadline_ms:1000,output_cap_bytes:4000};
    let document=home.path().join("routine.json");std::fs::write(&document,serde_json::to_vec(&definition).unwrap()).unwrap();
    let output=Command::new("/usr/bin/ssh-keygen").args(["-Y","sign","-f"]).arg(&key).args(["-n",authority::ROUTINE_SIGNATURE_NAMESPACE]).arg(&document).output().unwrap();assert!(output.status.success());
    let signature=home.path().join("routine.json.sig");let head=runtime::snapshot(&project).unwrap().head.to_string();
    let output=hp(home.path(),&["--root",root_arg,"routine-store","demo","import",document.to_str().unwrap(),signature.to_str().unwrap(),"--expected-head",&head]);
    assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stderr));
    let head=runtime::snapshot(&project).unwrap().head.to_string();let args=["--root",root_arg,"routine-store","demo","schedule","check","--expected-head",&head];
    let output=hp(home.path(),&args);assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stderr));
    let after=runtime::snapshot(&project).unwrap();assert_eq!(after.routine_occurrences.len(),1);assert_eq!(after.operations.len(),1);assert!(after.operations[0].task.is_none());
    assert!(!hp(home.path(),&args).status.success());assert_eq!(runtime::snapshot(&project).unwrap(),after);
    assert!(!project.join("ROUTINE_MARKER").exists());
    #[cfg(target_os="linux")]
    {
        let operation=after.operations[0].id.as_str();let head=after.head.to_string();
        let args=["--root",root_arg,"routine-store","demo","execute",operation,"--expected-head",&head];
        std::fs::write(&script,b"touch WRONG_SCRIPT").unwrap();
        assert!(!hp(home.path(),&args).status.success());assert_eq!(runtime::snapshot(&project).unwrap(),after);
        assert!(!project.join("WRONG_SCRIPT").exists());std::fs::write(&script,bytes).unwrap();
        let output=hp(home.path(),&args);assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stderr));
        let receipt:RoutineReceipt=serde_json::from_slice(&output.stdout).unwrap();assert!(receipt.cleanup_verified && receipt.succeeded,"{receipt:?}");
        assert_eq!(receipt.stdout,b"result\x1b[31m");
        let completed=runtime::snapshot(&project).unwrap();assert_eq!(completed.routine_receipts,vec![receipt]);
        assert_eq!(completed.deliveries[0].state,herdr_projects::operations::DeliveryState::Confirmed);
        let result=completed.inbox.iter().find(|i|i.content.kind=="routine-result").unwrap();assert!(!result.content.body.contains('\x1b'));
        let current=completed.head.to_string();
        assert!(!hp(home.path(),&["--root",root_arg,"routine-store","demo","execute",operation,"--expected-head",&current]).status.success());
        assert_eq!(runtime::snapshot(&project).unwrap(),completed);
        assert_eq!(std::fs::read(project.join("ROUTINE_MARKER")).unwrap(),b"once");
    }
}

#[cfg(feature="state-store")]
#[test]
fn approval_cli_uses_pinned_policy_and_refuses_unsigned_import() {
    use herdr_projects::{authority, migration, runtime};
    let home=tempfile::tempdir().unwrap();
    let caller=tempfile::tempdir().unwrap();
    let root=home.path().join("root");
    let root_arg=root.to_str().unwrap();
    for command in ["new","pause"] {
        assert!(hp(home.path(), &["--root",root_arg,command,"demo"]).status.success());
    }
    let config=home.path().join("owner.toml");
    std::fs::write(&config, "[authority]\nversion=1\nrevision=7\napproval_public_key='ssh-ed25519 AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA'\n").unwrap();
    let project=root.join("demo");
    let plan=migration::inspect_with_config(&project,&config).unwrap();
    migration::apply(&project,&plan,true).unwrap();
    let before=runtime::snapshot(&project).unwrap();
    let output=hp(caller.path(), &["--root",root_arg,"approval","demo","policy"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        serde_json::to_value(authority::policy_reference(&project).unwrap()).unwrap());
    let output=hp(caller.path(), &["--root",root_arg,"approval","demo","inspect"]);
    assert!(output.status.success());
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),serde_json::json!([]));
    let output=hp(caller.path(), &["--root",root_arg,"budget","demo","inspect"]);
    assert!(output.status.success());
    let report:serde_json::Value=serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["provider_tokens"],"unknown");assert!(report["policy"].is_null());
    let output=hp(caller.path(), &["--root",root_arg,"routine-store","demo","inspect"]);
    assert!(output.status.success());
    let report:serde_json::Value=serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["execution_enabled"],cfg!(target_os="linux"));assert_eq!(report["automatic_dispatch"],cfg!(target_os="linux"));assert_eq!(report["occurrences"],serde_json::json!([]));
    let output=hp(caller.path(), &["--root",root_arg,"routine-store","demo","schedule","unknown","--expected-head",&before.head.to_string()]);
    assert!(!output.status.success());assert_eq!(runtime::snapshot(&project).unwrap(),before);
    let document=home.path().join("grant.json");
    let signature=home.path().join("grant.sig");
    std::fs::write(&document,"{}").unwrap();
    std::fs::write(&signature,"unsigned").unwrap();
    let output=hp(caller.path(), &["--root",root_arg,"approval","demo","import",document.to_str().unwrap(),signature.to_str().unwrap(),"--expected-head",&before.head.to_string()]);
    assert!(!output.status.success());
    assert_eq!(runtime::snapshot(&project).unwrap(),before);
    let output=hp(caller.path(), &["--root",root_arg,"budget","demo","import",document.to_str().unwrap(),signature.to_str().unwrap(),"--expected-head",&before.head.to_string()]);
    assert!(!output.status.success());assert_eq!(runtime::snapshot(&project).unwrap(),before);
    let output=hp(caller.path(), &["--root",root_arg,"routine-store","demo","import",document.to_str().unwrap(),signature.to_str().unwrap(),"--expected-head",&before.head.to_string()]);
    assert!(!output.status.success());assert_eq!(runtime::snapshot(&project).unwrap(),before);
    assert!(!caller.path().join(".config").exists());
}

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
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["live_versions"], serde_json::json!([1]));
    let output = Command::new(BIN).env_clear().args(["artifact-stream", "--path", path.to_str().unwrap()]).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(&output.stdout[..8], b"HPAR\x01\0\0\0");
    let size = u32::from_be_bytes(output.stdout[8..12].try_into().unwrap()) as usize;
    assert_eq!(&output.stdout[12 + size..], bytes);
    std::os::unix::fs::symlink("report.md", path.join("library")).unwrap();
    assert!(!Command::new(BIN).env_clear().args(["artifact-stream", "--path", path.to_str().unwrap()]).output().unwrap().status.success());
    let live=Command::new(BIN).env_clear().args(["artifact-stream","--live","--path",path.to_str().unwrap()]).output().unwrap();
    assert!(live.status.success(),"{}",String::from_utf8_lossy(&live.stderr));
    assert_eq!(&live.stdout[..8],b"HPLV\x01\0\0\0");
    let size=u32::from_be_bytes(live.stdout[8..12].try_into().unwrap()) as usize;
    let manifest:serde_json::Value=serde_json::from_slice(&live.stdout[12..12+size]).unwrap();
    assert_eq!(manifest["omissions"][0]["reason"],"symbolic-link");
    assert_eq!(&live.stdout[12+size..],bytes);
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
    let raw=rusqlite::Connection::open(project.join(".state/state.db")).unwrap();raw.execute_batch("DROP TABLE routine_occurrences; DROP TABLE routine_cursors; DROP TABLE routine_revisions; DROP TABLE budget_policies; DROP TABLE approval_uses; DROP TABLE approval_revocations; DROP TABLE approval_grants; DROP TRIGGER operation_delivery_monotonic; DROP TABLE attempt_cancellations; DROP TABLE attempt_inputs; DROP TABLE task_dependencies; DROP TABLE task_queue; DROP TABLE scheduler_policy; DROP TABLE runtime_ownership; DROP TABLE project_control; DROP TABLE runtime_observations; DROP TABLE runtime_bindings; UPDATE store_meta SET schema_version=4; PRAGMA user_version=4;").unwrap();drop(raw);
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

#[cfg(target_os="linux")]
#[test]
fn ticker_native_copy_publishes_announces_and_does_not_recopy_after_restart() {
    use std::{fs,time::{Duration,Instant},process::Stdio,os::unix::fs::PermissionsExt};
    use sha2::{Digest,Sha256};
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let r=root.to_str().unwrap();
    assert!(hp(home.path(),&["--root",r,"new","demo"]).status.success());let project=root.join("demo");
    let source=home.path().join("source '$λ");fs::create_dir_all(source.join("library/empty")).unwrap();
    fs::write(source.join("report.md"),b"new report\0\xff").unwrap();fs::write(source.join("library/item"),b"binary\0\xff").unwrap();
    let hash=format!("{:x}",Sha256::digest(b"new report\0\xff"));
    let socket=home.path().join("session.sock");fs::write(&socket,b"").unwrap();
    fs::write(project.join(".state/coordinator.json"),serde_json::to_vec(&serde_json::json!({"socket":socket,"workspace_id":"w1","tab_id":"w1:t1","pane_id":"w1:p1","agent_name":"coordinator","cwd":project})).unwrap()).unwrap();
    let record=project.join("threads/t-0001.toml");
    fs::write(&record,toml::to_string(&serde_json::json!({"id":"t-0001","status":"open","kind":"adopted","thread_dir":source,"cwd":source,"workspace_id":"w2","tab_id":"w2:t1","pane_id":"w2:p1","agent":"claude","agent_name":"worker","title":"Fixture","created":jiff::Timestamp::now().to_string()})).unwrap()).unwrap();
    fs::write(home.path().join("agents.json"),serde_json::to_vec(&serde_json::json!({"result":{"agents":[{"workspace_id":"w2","tab_id":"w2:t1","pane_id":"w2:p1","cwd":source,"name":"worker","agent":"claude","agent_status":"idle"}]}})).unwrap()).unwrap();
    fs::write(home.path().join("panes.json"),serde_json::to_vec(&serde_json::json!({"result":{"panes":[{"workspace_id":"w1","tab_id":"w1:t1","pane_id":"w1:p1","cwd":project},{"workspace_id":"w2","tab_id":"w2:t1","pane_id":"w2:p1","cwd":source}]}})).unwrap()).unwrap();
    let fake=home.path().join("herdr");fs::write(&fake,b"#!/bin/sh\ncase \"$1 $2\" in\n'agent list') /bin/cat \"$HOME/agents.json\";;\n'pane list') echo poll >> \"$HOME/polls\"; /bin/cat \"$HOME/panes.json\";;\n*) echo '{\"result\":{\"shown\":true}}';;\nesac\n").unwrap();fs::set_permissions(&fake,fs::Permissions::from_mode(0o700)).unwrap();
    struct Child(std::process::Child);
    impl Drop for Child {fn drop(&mut self){let _=self.0.kill();let _=self.0.wait();}}
    let spawn=||Child(Command::new(BIN).env_clear().env("HOME",home.path()).env("PATH","/usr/bin:/bin").env("HERDR_BIN_PATH",&fake).args(["--root",r,"ticker","run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
    let read=||->toml::Value {toml::from_str(&fs::read_to_string(&record).unwrap()).unwrap()};
    let wait=|child:&mut Child,predicate:&dyn Fn()->bool| {
        let deadline=Instant::now()+Duration::from_secs(45);
        while !predicate(){assert!(child.0.try_wait().unwrap().is_none(),"ticker exited");assert!(Instant::now()<deadline,"ticker log: {}",fs::read_to_string(root.join(".ticker.log")).unwrap_or_default());std::thread::sleep(Duration::from_millis(10));}
    };
    let stop=|child:&mut Child| {
        fs::write(root.join(".ticker.stop"),b"").unwrap();let deadline=Instant::now()+Duration::from_secs(5);
        loop {if let Some(status)=child.0.try_wait().unwrap(){assert!(status.success());break;}assert!(Instant::now()<deadline);std::thread::sleep(Duration::from_millis(10));}
        fs::remove_file(root.join(".ticker.stop")).unwrap();
    };
    let mut child=spawn();wait(&mut child,&||read().get("copy_receipt").is_some());stop(&mut child);
    assert_eq!(read()["report_hash"].as_str(),Some(hash.as_str()));assert_eq!(read()["copy_receipt"]["sequence"].as_integer(),Some(1));
    assert_eq!(fs::read(project.join("threads/t-0001.md")).unwrap(),b"new report\0\xff");let item=project.join("library/t-0001/item");assert_eq!(fs::read(&item).unwrap(),b"binary\0\xff");assert!(project.join("library/t-0001/empty").is_dir());
    let mut child=spawn();wait(&mut child,&||read().get("last_review_item_hash").and_then(|v|v.as_str())==Some(hash.as_str()));stop(&mut child);
    let notices=fs::read_dir(project.join("inbox")).unwrap().filter_map(|e|e.ok()).filter(|e|e.file_name().to_string_lossy().starts_with("review-")).count();assert_eq!(notices,1);
    fs::write(&item,b"retained after unchanged report").unwrap();let polls=fs::read(home.path().join("polls")).unwrap().len();
    let mut child=spawn();wait(&mut child,&||fs::read(home.path().join("polls")).unwrap().len()>=polls+10);stop(&mut child);
    assert_eq!(read()["copy_receipt"]["sequence"].as_integer(),Some(1));assert_eq!(read()["live_copy_sequence"].as_integer(),Some(1));assert_eq!(fs::read(item).unwrap(),b"retained after unchanged report");
}

#[test]
fn native_report_hash_is_bounded_binary_and_configuration_independent() {
    use std::fs;use sha2::{Digest,Sha256};
    let home=tempfile::tempdir().unwrap();let source=home.path().join("source '$λ");fs::create_dir(&source).unwrap();
    let run=||Command::new(BIN).env_clear().args(["report-hash","--path"]).arg(&source).output().unwrap();
    let missing=run();assert!(missing.status.success());assert_eq!(serde_json::from_slice::<serde_json::Value>(&missing.stdout).unwrap(),serde_json::json!({"hash":null}));
    fs::write(source.join("report.md"),b"binary\0\xff").unwrap();let output=run();assert!(output.status.success());
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["hash"],format!("{:x}",Sha256::digest(b"binary\0\xff")));
    fs::File::create(source.join("report.md")).unwrap().set_len(50*1024*1024+1).unwrap();assert!(!run().status.success());
    fs::remove_file(source.join("report.md")).unwrap();std::os::unix::fs::symlink("/etc/passwd",source.join("report.md")).unwrap();assert!(!run().status.success());
    let name=std::ffi::CString::new(source.join("report.md").as_os_str().as_encoded_bytes()).unwrap();fs::remove_file(source.join("report.md")).unwrap();assert_eq!(unsafe{libc::mkfifo(name.as_ptr(),0o600)},0);assert!(!run().status.success());
}

#[cfg(target_os="linux")]
#[test]
fn native_ticker_claims_legacy_routine_and_restart_delivers_without_rerun() {
    use std::{fs,time::{Duration,Instant},process::Stdio,os::unix::fs::PermissionsExt};
    use sha2::{Digest,Sha256};
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let r=root.to_str().unwrap();
    assert!(hp(home.path(),&["--root",r,"new","demo"]).status.success());let project=root.join("demo");
    let socket=home.path().join("session.sock");fs::write(&socket,b"").unwrap();
    fs::write(project.join(".state/coordinator.json"),serde_json::to_vec(&serde_json::json!({"socket":socket,"workspace_id":"w1","tab_id":"w1:t1","pane_id":"w1:p1","agent_name":"coordinator","cwd":project})).unwrap()).unwrap();
    let command="printf run >> executions; printf routine-result";
    fs::write(project.join("routines/check.md"),format!("+++\nschedule = \"every 24h\"\ncommand = {}\n+++\nInspect output.\n",serde_json::to_string(command).unwrap())).unwrap();
    let cfg=home.path().join(".config/herdr-projects");fs::create_dir_all(&cfg).unwrap();
    fs::write(cfg.join("config.toml"),format!("[safety.\"{}\"]\nroutine_commands = true\n",project.display())).unwrap();
    fs::write(cfg.join("approved-routines.json"),serde_json::to_vec(&serde_json::json!([{"project":project,"routine":"check","command_sha256":format!("{:x}",Sha256::digest(command.as_bytes())),"approved":"fixture"}])).unwrap()).unwrap();
    let state=project.join(".state/ticker.json");fs::write(&state,b"{\"routines\":{\"check\":{\"last_run\":\"2026-01-01T00:00:00Z\"}}}").unwrap();
    let fake=home.path().join("herdr");fs::write(&fake,b"#!/bin/sh\ncase \"$1 $2\" in\n'agent list') echo '{\"result\":{\"agents\":[]}}';;\n'pane list') echo '{\"result\":{\"panes\":[]}}';;\n*) echo '{\"result\":{\"shown\":true}}';;\nesac\n").unwrap();fs::set_permissions(&fake,fs::Permissions::from_mode(0o700)).unwrap();
    struct Child(std::process::Child);
    impl Drop for Child {fn drop(&mut self){let _=self.0.kill();let _=self.0.wait();}}
    let spawn=||Child(Command::new(BIN).env_clear().env("HOME",home.path()).env("PATH","/usr/bin:/bin").env("HERDR_BIN_PATH",&fake).args(["--root",r,"ticker","run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
    let read=||->serde_json::Value {serde_json::from_slice(&fs::read(&state).unwrap()).unwrap()};
    let wait=|child:&mut Child,predicate:&dyn Fn()->bool| {
        let deadline=Instant::now()+Duration::from_secs(10);
        while !predicate(){assert!(child.0.try_wait().unwrap().is_none(),"ticker exited");assert!(Instant::now()<deadline,"ticker log: {}",fs::read_to_string(root.join(".ticker.log")).unwrap_or_default());std::thread::sleep(Duration::from_millis(10));}
    };
    let stop=|child:&mut Child| {
        fs::write(root.join(".ticker.stop"),b"").unwrap();let end=Instant::now()+Duration::from_secs(8);
        while child.0.try_wait().unwrap().is_none(){assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(10));}
        fs::remove_file(root.join(".ticker.stop")).unwrap();
    };
    let mut child=spawn();wait(&mut child,&||read()["routines"]["check"]["dispatch"]["result"].is_object());stop(&mut child);
    let mut child=spawn();wait(&mut child,&||read()["routines"]["check"]["dispatch"].is_null());stop(&mut child);
    assert_eq!(fs::read(project.join("executions")).unwrap(),b"run");
    let items=fs::read_dir(project.join("inbox")).unwrap().filter_map(Result::ok).filter_map(|entry|fs::read_to_string(entry.path()).ok()).filter(|text|text.contains("routine-result")).count();assert_eq!(items,1);
}

#[cfg(target_os="linux")]
#[test]
fn ticker_native_merged_finalization_resolves_and_replays_notice_after_restart() {
    use std::{fs,time::{Duration,Instant},process::Stdio,os::unix::fs::PermissionsExt};
    use sha2::{Digest,Sha256};
    let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let r=root.to_str().unwrap();
    assert!(hp(home.path(),&["--root",r,"new","demo"]).status.success());let project=root.join("demo");
    let source=home.path().join("source '$λ");fs::create_dir_all(source.join("library/empty")).unwrap();
    fs::write(source.join("report.md"),b"PR: https://github.com/example/repo/pull/1\ncomplete\n").unwrap();fs::write(source.join("library/item"),b"binary\0\xff").unwrap();
    let hash=format!("{:x}",Sha256::digest(b"PR: https://github.com/example/repo/pull/1\ncomplete\n"));
    let socket=home.path().join("session.sock");fs::write(&socket,b"").unwrap();
    fs::write(project.join(".state/coordinator.json"),serde_json::to_vec(&serde_json::json!({"socket":socket,"workspace_id":"w1","tab_id":"w1:t1","pane_id":"w1:p1","agent_name":"coordinator","cwd":project})).unwrap()).unwrap();
    let record=project.join("threads/t-0001.toml");
    fs::write(&record,toml::to_string(&serde_json::json!({"id":"t-0001","status":"open","kind":"adopted","thread_dir":source,"cwd":source,"workspace_id":"w2","tab_id":"w2:t1","pane_id":"w2:p1","agent":"claude","agent_name":"worker","title":"Fixture","created":jiff::Timestamp::now().to_string()})).unwrap()).unwrap();
    fs::write(home.path().join("agents.json"),serde_json::to_vec(&serde_json::json!({"result":{"agents":[{"workspace_id":"w2","tab_id":"w2:t1","pane_id":"w2:p1","cwd":source,"name":"worker","agent":"claude","agent_status":"idle"}]}})).unwrap()).unwrap();
    fs::write(home.path().join("panes.json"),serde_json::to_vec(&serde_json::json!({"result":{"panes":[{"workspace_id":"w1","tab_id":"w1:t1","pane_id":"w1:p1","cwd":project},{"workspace_id":"w2","tab_id":"w2:t1","pane_id":"w2:p1","cwd":source}]}})).unwrap()).unwrap();
    let fake=home.path().join("herdr");fs::write(&fake,b"#!/bin/sh\ncase \"$1 $2\" in\n'agent list') /bin/cat \"$HOME/agents.json\";;\n'pane list') echo poll >> \"$HOME/polls\"; /bin/cat \"$HOME/panes.json\";;\n*) echo '{\"result\":{\"shown\":true}}';;\nesac\n").unwrap();fs::set_permissions(&fake,fs::Permissions::from_mode(0o700)).unwrap();
    struct Child(std::process::Child);
    impl Drop for Child {fn drop(&mut self){let _=self.0.kill();let _=self.0.wait();}}
    let spawn=||Child(Command::new(BIN).env_clear().env("HOME",home.path()).env("PATH","/usr/bin:/bin").env("HERDR_BIN_PATH",&fake).args(["--root",r,"ticker","run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
    let read=||->toml::Value {toml::from_str(&fs::read_to_string(&record).unwrap()).unwrap()};
    let wait=|child:&mut Child,predicate:&dyn Fn()->bool| {
        let deadline=Instant::now()+Duration::from_secs(45);
        while !predicate(){assert!(child.0.try_wait().unwrap().is_none(),"ticker exited");assert!(Instant::now()<deadline,"ticker log: {}",fs::read_to_string(root.join(".ticker.log")).unwrap_or_default());std::thread::sleep(Duration::from_millis(10));}
    };
    let stop=|child:&mut Child| {
        fs::write(root.join(".ticker.stop"),b"").unwrap();let deadline=Instant::now()+Duration::from_secs(5);
        loop {if let Some(status)=child.0.try_wait().unwrap(){assert!(status.success());break;}assert!(Instant::now()<deadline);std::thread::sleep(Duration::from_millis(10));}
        fs::remove_file(root.join(".ticker.stop")).unwrap();
    };
    let url="https://github.com/example/repo/pull/1";
    let mut saved=read();let table=saved.as_table_mut().unwrap();
    for (key,value) in [("pr",url),("pr_state","MERGED"),("report_hash",hash.as_str()),("acked_report_hash",hash.as_str()),("last_review_item_hash",hash.as_str()),("last_state","idle"),("last_group","idle")] {table.insert(key.into(),toml::Value::String(value.into()));}
    fs::write(&record,toml::to_string(&saved).unwrap()).unwrap();fs::write(project.join("threads/t-0001.md"),fs::read(source.join("report.md")).unwrap()).unwrap();
    let fingerprint=format!("{:x}",Sha256::digest(serde_json::json!(["t-0001",saved["created"].as_str().unwrap(),0,"adopted","","","","","",source,"w2","w2:t1","w2:p1","claude","worker",source,url]).to_string().as_bytes()));
    fs::write(project.join(".state/ticker.json"),serde_json::to_vec(&serde_json::json!({"last_pr_check":jiff::Timestamp::now().to_string(),"finalizations":{"t-0001":{"operation_id":format!("merged-{fingerprint}"),"fingerprint":fingerprint,"pr":url,"reason":"merged"}}})).unwrap()).unwrap();
    let mut child=spawn();wait(&mut child,&||read()["status"].as_str()==Some("resolved"));stop(&mut child);
    assert_eq!(read()["resolved_reason"].as_str(),Some("merged"));assert_eq!(read()["final_copy_sequence"].as_integer(),Some(1));assert!(read().get("pending_final_copy").is_none());
    assert!(!read()["artifact_snapshot"].as_str().unwrap().is_empty());assert_eq!(read()["copy_receipt"]["sequence"].as_integer(),Some(1));
    assert_eq!(fs::read(project.join("library/t-0001/item")).unwrap(),b"binary\0\xff");
    let mut child=spawn();wait(&mut child,&||read().get("pending_final_notice").is_none()&&serde_json::from_slice::<serde_json::Value>(&fs::read(project.join(".state/ticker.json")).unwrap()).unwrap()["finalizations"].as_object().is_some_and(|v|v.is_empty()));stop(&mut child);
    assert_eq!(read()["final_copy_sequence"].as_integer(),Some(1));
    assert_eq!(fs::read_dir(project.join("inbox")).unwrap().filter_map(|e|e.ok()).filter(|e|e.file_name().to_string_lossy().starts_with("final-")).count(),1);
}

#[cfg(target_os="linux")]
#[test]
fn ticker_native_briefs_confirm_or_recover_uncertainty_without_replay() {
    use std::{fs,time::{Duration,Instant},process::Stdio,os::unix::{fs::PermissionsExt,net::UnixListener}};
    for outcome in ["confirmed","lost"] {
        let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let r=root.to_str().unwrap();
        assert!(hp(home.path(),&["--root",r,"new","demo"]).status.success());let project=root.join("demo");
        let socket=home.path().join("session.sock");let _listener=UnixListener::bind(&socket).unwrap();
        fs::write(project.join(".state/coordinator.json"),serde_json::json!({"socket":socket}).to_string()).unwrap();
        let source=home.path().join("source");fs::create_dir(&source).unwrap();
        let agent=serde_json::json!({"workspace_id":"w","tab_id":"tab","pane_id":"p","cwd":source,"name":"worker","agent":"claude","agent_status":"idle"});
        let record=project.join("threads/t-0001.toml");fs::write(&record,toml::to_string(&serde_json::json!({"id":"t-0001","status":"open","kind":"adopted","prompt_pending":true,"thread_dir":source,"cwd":source,"workspace_id":"w","tab_id":"tab","pane_id":"p","agent":"claude","agent_name":"worker","created":jiff::Timestamp::now().to_string()})).unwrap()).unwrap();
        fs::write(home.path().join("agents.json"),serde_json::json!({"result":{"agents":[agent.clone()]}}).to_string()).unwrap();
        fs::write(home.path().join("panes.json"),serde_json::json!({"result":{"panes":[{"workspace_id":"w","tab_id":"tab","pane_id":"p","cwd":source}]}}).to_string()).unwrap();
        fs::write(home.path().join("ack.json"),serde_json::json!({"result":{"type":"agent_prompted","agent":agent}}).to_string()).unwrap();
        let response=if outcome=="lost" {"exit 1"}else{"/bin/cat \"$HOME/ack.json\""};
        let fake=home.path().join("herdr");fs::write(&fake,format!("#!/bin/sh\ncase \"$1 $2\" in\n'agent list') /bin/cat \"$HOME/agents.json\";;\n'pane list') echo poll >> \"$HOME/polls\"; /bin/cat \"$HOME/panes.json\";;\n'agent prompt') printf send >> \"$HOME/sent\"; {response};;\n*) echo '{{\"result\":{{\"shown\":true}}}}';;\nesac\n")).unwrap();fs::set_permissions(&fake,fs::Permissions::from_mode(0o700)).unwrap();
        struct Child(std::process::Child);impl Drop for Child {fn drop(&mut self){let _=self.0.kill();let _=self.0.wait();}}
        let spawn=||Child(Command::new(BIN).env_clear().env("HOME",home.path()).env("PATH","/usr/bin:/bin").env("HERDR_BIN_PATH",&fake).args(["--root",r,"ticker","run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
        let read=||->toml::Value {toml::from_str(&fs::read_to_string(&record).unwrap()).unwrap()};
        let wait=|child:&mut Child,predicate:&dyn Fn()->bool| {let end=Instant::now()+Duration::from_secs(45);while !predicate(){assert!(child.0.try_wait().unwrap().is_none());assert!(Instant::now()<end,"{}",fs::read_to_string(root.join(".ticker.log")).unwrap_or_default());std::thread::sleep(Duration::from_millis(10));}};
        let stop=|child:&mut Child| {fs::write(root.join(".ticker.stop"),b"").unwrap();let end=Instant::now()+Duration::from_secs(8);while child.0.try_wait().unwrap().is_none(){assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(10));}fs::remove_file(root.join(".ticker.stop")).unwrap();};
        let mut child=spawn();wait(&mut child,&||fs::read(home.path().join("sent")).is_ok_and(|b|b==b"send")&&read().get("prompt_claim").is_some_and(|c|c.get("phase").and_then(|p|p.as_str())==Some(if outcome=="confirmed"{"confirmed"}else{"pending"})));stop(&mut child);
        let polls=fs::read(home.path().join("polls")).unwrap().len();let mut child=spawn();
        wait(&mut child,&||fs::read(home.path().join("polls")).unwrap().len()>polls&&read()["prompt_claim"]["notified"].as_bool()==Some(true));stop(&mut child);
        assert_eq!(fs::read(home.path().join("sent")).unwrap(),b"send");assert_eq!(read()["prompt_sequence"].as_integer(),Some(1));
        if outcome=="confirmed" {assert_eq!(read()["prompt_pending"].as_bool(),Some(false));}
        else {assert_eq!(read()["status"].as_str(),Some("failed"));assert_eq!(read()["prompt_claim"]["phase"].as_str(),Some("uncertain"));let notices=fs::read_dir(project.join("inbox")).unwrap().filter_map(Result::ok).filter(|e|e.file_name().to_string_lossy().starts_with("brief-")).count();assert_eq!(notices,1);}
    }
}

#[cfg(target_os="linux")]
#[test]
fn ticker_remote_briefs_confirm_or_recover_uncertainty_without_replay() {
    use std::{fs,time::{Duration,Instant},process::Stdio,os::unix::{fs::PermissionsExt,net::UnixListener}};
    for outcome in ["confirmed","lost"] {
        let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let r=root.to_str().unwrap();
        assert!(hp(home.path(),&["--root",r,"new","demo"]).status.success());let project=root.join("demo");
        let socket=home.path().join("session.sock");let _listener=UnixListener::bind(&socket).unwrap();
        fs::write(project.join(".state/coordinator.json"),serde_json::json!({"socket":socket}).to_string()).unwrap();
        let source=home.path().join("source");fs::create_dir(&source).unwrap();
        let agent=serde_json::json!({"workspace_id":"w","tab_id":"tab","pane_id":"p","cwd":source,"name":"worker","agent":"claude","agent_status":"idle"});
        let record=project.join("threads/t-0001.toml");fs::write(&record,toml::to_string(&serde_json::json!({"id":"t-0001","status":"open","kind":"adopted","prompt_pending":true,"machine":"saved","thread_dir":source,"cwd":source,"workspace_id":"w","tab_id":"tab","pane_id":"p","agent":"claude","agent_name":"worker","created":jiff::Timestamp::now().to_string()})).unwrap()).unwrap();
        fs::write(home.path().join("agents.json"),serde_json::json!({"result":{"agents":[agent.clone()]}}).to_string()).unwrap();
        fs::write(home.path().join("panes.json"),serde_json::json!({"result":{"panes":[{"workspace_id":"w","tab_id":"tab","pane_id":"p","cwd":source}]}}).to_string()).unwrap();
        fs::write(home.path().join("ack.json"),serde_json::json!({"result":{"type":"agent_prompted","agent":agent}}).to_string()).unwrap();
        let route=serde_json::json!([{"id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","label":"saved","target":"fixture.invalid","session":"named-session","enabled":true,"selected":false}]);
        fs::write(home.path().join("routes.json"),route.to_string()).unwrap();fs::write(home.path().join("outcome"),outcome).unwrap();
        let fake=home.path().join("herdr");fs::write(&fake,r#"#!/usr/bin/python3
import os,json,sys,pathlib
root=pathlib.Path(os.environ['HOME']);args=sys.argv[1:];remote=args[:2]==['--machine','saved']
if remote:args=args[2:]
if args==['machine','list','--json']:print((root/'routes.json').read_text())
elif args==['agent','list']:print((root/'agents.json').read_text() if remote else '{"result":{"agents":[]}}')
elif args==['pane','list']:
 with open(root/'polls','a') as f:f.write('poll')
 print((root/'panes.json').read_text() if remote else '{"result":{"panes":[]}}')
elif args[:2] in [['agent','prompt'],['agent','start']]:
 (root/'WRONG_SYNC_EFFECT').touch();sys.exit(2)
else:print('{"result":{"shown":true}}')
"#).unwrap();fs::set_permissions(&fake,fs::Permissions::from_mode(0o700)).unwrap();
        let bridge=home.path().join("remote herdr");fs::write(&bridge,r#"#!/usr/bin/python3
import os,json,sys,pathlib
root=pathlib.Path(os.environ['HOME'])
assert sys.argv[1:4]==['--session','named-session','remote-api-bridge']
if sys.argv[4:]==['--check']:print('herdr-api-bridge-v1');sys.exit(0)
r=json.loads(sys.stdin.readline())
if r['method']=='agent.list':result=json.loads((root/'agents.json').read_text())['result']
elif r['method']=='pane.list':result=json.loads((root/'panes.json').read_text())['result']
elif r['method']=='agent.prompt':
 assert r['params']['target']=='p'
 with open(root/'sent','a') as f:f.write('send')
 if (root/'outcome').read_text()=='lost':sys.exit(1)
 result=json.loads((root/'ack.json').read_text())['result']
else:sys.exit(3)
print(json.dumps({'id':r['id'],'result':result}))
"#).unwrap();fs::set_permissions(&bridge,fs::Permissions::from_mode(0o700)).unwrap();
        let ssh=home.path().join("ssh");fs::write(&ssh,r#"#!/usr/bin/python3
import sys,subprocess
assert sys.argv[-2]=='fixture.invalid'
if 'remote-api-bridge' in sys.argv[-1]:assert sys.argv[1:4]==['-T','-o','StrictHostKeyChecking=yes']
sys.exit(subprocess.call(sys.argv[-1],shell=True))
"#).unwrap();fs::set_permissions(&ssh,fs::Permissions::from_mode(0o700)).unwrap();
        struct Child(std::process::Child);impl Drop for Child {fn drop(&mut self){let _=self.0.kill();let _=self.0.wait();}}
        let spawn=||Child(Command::new(BIN).env_clear().env("HOME",home.path()).env("PATH",format!("{}:/usr/bin:/bin",home.path().display())).env("HERDR_BIN_PATH",&fake).env("HERDR_PROJECTS_REMOTE_HERDR_BIN",&bridge).args(["--root",r,"ticker","run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
        let read=||->toml::Value {toml::from_str(&fs::read_to_string(&record).unwrap()).unwrap()};
        let wait=|child:&mut Child,predicate:&dyn Fn()->bool| {let end=Instant::now()+Duration::from_secs(45);while !predicate(){assert!(child.0.try_wait().unwrap().is_none());assert!(Instant::now()<end,"{}",fs::read_to_string(root.join(".ticker.log")).unwrap_or_default());std::thread::sleep(Duration::from_millis(10));}};
        let stop=|child:&mut Child| {fs::write(root.join(".ticker.stop"),b"").unwrap();let end=Instant::now()+Duration::from_secs(8);while child.0.try_wait().unwrap().is_none(){assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(10));}fs::remove_file(root.join(".ticker.stop")).unwrap();};
        let mut child=spawn();wait(&mut child,&||fs::read(home.path().join("sent")).is_ok_and(|b|b==b"send")&&read().get("prompt_claim").is_some_and(|c|c.get("phase").and_then(|p|p.as_str())==Some(if outcome=="confirmed"{"confirmed"}else{"pending"})));stop(&mut child);
        let polls=fs::read(home.path().join("polls")).unwrap().len();let mut child=spawn();
        wait(&mut child,&||fs::read(home.path().join("polls")).unwrap().len()>polls&&read()["prompt_claim"]["notified"].as_bool()==Some(true));stop(&mut child);
        assert!(!home.path().join("WRONG_SYNC_EFFECT").exists());assert_eq!(fs::read(home.path().join("sent")).unwrap(),b"send");assert_eq!(read()["prompt_sequence"].as_integer(),Some(1));
        if outcome=="confirmed" {assert_eq!(read()["prompt_pending"].as_bool(),Some(false));}
        else {assert_eq!(read()["status"].as_str(),Some("failed"));assert_eq!(read()["prompt_claim"]["phase"].as_str(),Some("uncertain"));let notices=fs::read_dir(project.join("inbox")).unwrap().filter_map(Result::ok).filter(|e|e.file_name().to_string_lossy().starts_with("brief-")).count();assert_eq!(notices,1);}
    }
}

#[cfg(target_os="linux")]
#[test]
fn ticker_local_and_remote_launches_acknowledge_once_and_recover_lost_replies() {
    use std::{fs,time::{Duration,Instant},process::Stdio,os::unix::{fs::PermissionsExt,net::UnixListener}};
    for is_remote in [false,true] {
    for outcome in ["confirmed","lost"] {
        let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let r=root.to_str().unwrap();
        assert!(hp(home.path(),&["--root",r,"new","demo"]).status.success());let project=root.join("demo");
        let socket=home.path().join("session.sock");let _listener=UnixListener::bind(&socket).unwrap();
        fs::write(project.join(".state/coordinator.json"),serde_json::json!({"socket":socket}).to_string()).unwrap();
        let source=home.path().join("source");fs::create_dir(&source).unwrap();
        let agent=serde_json::json!({"workspace_id":"w","tab_id":"tab","pane_id":"p","cwd":source,"name":"worker","agent":"claude","agent_status":"blocked","terminal_id":"terminal","launch_pending":true});
        let record=project.join("threads/t-0001.toml");fs::write(&record,toml::to_string(&serde_json::json!({"id":"t-0001","status":"open","kind":"adopted","prompt_pending":true,"machine":if is_remote{"saved"}else{""},"thread_dir":source,"cwd":source,"workspace_id":"w","tab_id":"tab","pane_id":"p","agent":"claude","agent_name":"worker","created":jiff::Timestamp::now().to_string()})).unwrap()).unwrap();
        fs::write(home.path().join("agents.json"),serde_json::json!({"result":{"agents":[agent.clone()]}}).to_string()).unwrap();
        fs::write(home.path().join("panes.json"),serde_json::json!({"result":{"panes":[{"workspace_id":"w","tab_id":"tab","pane_id":"p","terminal_id":"terminal","cwd":source}]}}).to_string()).unwrap();
        fs::write(home.path().join("ack.json"),serde_json::json!({"result":{"type":"agent_started","agent":agent,"argv":["claude"]}}).to_string()).unwrap();
        let route=serde_json::json!([{"id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","label":"saved","target":"fixture.invalid","session":"named-session","enabled":true,"selected":false}]);
        fs::write(home.path().join("routes.json"),route.to_string()).unwrap();fs::write(home.path().join("outcome"),outcome).unwrap();fs::write(home.path().join("remote-mode"),if is_remote{"yes"}else{"no"}).unwrap();
        let fake=home.path().join("herdr");fs::write(&fake,r#"#!/usr/bin/python3
import os,json,sys,pathlib
root=pathlib.Path(os.environ['HOME']);args=sys.argv[1:];remote=args[:2]==['--machine','saved']
if remote:args=args[2:]
remote_mode=(root/'remote-mode').read_text()=='yes'
if args[:1]==['remote-api-bridge']:os.execv(str(root/'remote herdr'),[str(root/'remote herdr'),*args])
if args==['machine','list','--json']:print((root/'routes.json').read_text())
elif args==['agent','list']:print((root/'agents.json').read_text() if (remote or not remote_mode) and (root/'started').exists() else '{"result":{"agents":[]}}')
elif args==['pane','list']:
 with open(root/'polls','a') as f:f.write('poll')
 print((root/'panes.json').read_text() if remote or not remote_mode else '{"result":{"panes":[]}}')
elif args[:2] in [['agent','prompt'],['agent','start']]:
 (root/'WRONG_SYNC_EFFECT').touch();sys.exit(2)
else:print('{"result":{"shown":true}}')
"#).unwrap();fs::set_permissions(&fake,fs::Permissions::from_mode(0o700)).unwrap();
        let bridge=home.path().join("remote herdr");fs::write(&bridge,r#"#!/usr/bin/python3
import os,json,sys,pathlib
root=pathlib.Path(os.environ['HOME'])
args=sys.argv[1:]
if args[:2]==['--session','named-session']:args=args[2:]
assert args[0]=='remote-api-bridge'
if args[1:]==['--check']:print('herdr-api-bridge-v1');sys.exit(0)
r=json.loads(sys.stdin.readline())
if r['method']=='agent.list':result=json.loads((root/'agents.json').read_text())['result'] if (root/'started').exists() else {'agents':[]}
elif r['method']=='pane.list':result=json.loads((root/'panes.json').read_text())['result']
elif r['method']=='agent.start':
 assert r['params']=={'name':'worker','kind':'claude','pane_id':'p','args':[],'timeout_ms':20000}
 with open(root/'started','a') as f:f.write('start')
 if (root/'outcome').read_text()=='lost':sys.exit(1)
 result=json.loads((root/'ack.json').read_text())['result'];del result['agent']['agent'];result['agent']['agent_status']='unknown'
else:sys.exit(3)
print(json.dumps({'id':r['id'],'result':result}))
"#).unwrap();fs::set_permissions(&bridge,fs::Permissions::from_mode(0o700)).unwrap();
        let ssh=home.path().join("ssh");fs::write(&ssh,r#"#!/usr/bin/python3
import sys,subprocess
assert sys.argv[-2]=='fixture.invalid'
if 'remote-api-bridge' in sys.argv[-1]:assert sys.argv[1:4]==['-T','-o','StrictHostKeyChecking=yes']
sys.exit(subprocess.call(sys.argv[-1],shell=True))
"#).unwrap();fs::set_permissions(&ssh,fs::Permissions::from_mode(0o700)).unwrap();
        struct Child(std::process::Child);impl Drop for Child {fn drop(&mut self){let _=self.0.kill();let _=self.0.wait();}}
        let spawn=||Child(Command::new(BIN).env_clear().env("HOME",home.path()).env("PATH",format!("{}:/usr/bin:/bin",home.path().display())).env("HERDR_BIN_PATH",&fake).env("HERDR_PROJECTS_REMOTE_HERDR_BIN",&bridge).args(["--root",r,"ticker","run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
        let read=||->toml::Value {toml::from_str(&fs::read_to_string(&record).unwrap()).unwrap()};
        let wait=|child:&mut Child,predicate:&dyn Fn()->bool| {let end=Instant::now()+Duration::from_secs(45);while !predicate(){assert!(child.0.try_wait().unwrap().is_none());assert!(Instant::now()<end,"{}",fs::read_to_string(root.join(".ticker.log")).unwrap_or_default());std::thread::sleep(Duration::from_millis(10));}};
        let stop=|child:&mut Child| {fs::write(root.join(".ticker.stop"),b"").unwrap();let end=Instant::now()+Duration::from_secs(8);while child.0.try_wait().unwrap().is_none(){assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(10));}fs::remove_file(root.join(".ticker.stop")).unwrap();};
        let mut child=spawn();wait(&mut child,&||fs::read(home.path().join("started")).is_ok_and(|b|b==b"start")&&read().get("launch_claim").is_some_and(|c|c.get("phase").and_then(|p|p.as_str())==Some(if outcome=="confirmed"{"confirmed"}else{"pending"})));stop(&mut child);
        let polls=fs::read(home.path().join("polls")).unwrap().len();let mut child=spawn();
        wait(&mut child,&||fs::read(home.path().join("polls")).unwrap().len()>polls&&read()["launch_claim"]["notified"].as_bool()==Some(true));stop(&mut child);
        assert!(!home.path().join("WRONG_SYNC_EFFECT").exists());assert_eq!(fs::read(home.path().join("started")).unwrap(),b"start");assert_eq!(read()["launch_sequence"].as_integer(),Some(1));
        if outcome=="confirmed" {assert_eq!(read()["prompt_pending"].as_bool(),Some(true));assert_eq!(read()["launch_claim"]["phase"].as_str(),Some("confirmed"));}
        else {assert_eq!(read()["status"].as_str(),Some("failed"));assert_eq!(read()["launch_claim"]["phase"].as_str(),Some("uncertain"));let notices=fs::read_dir(project.join("inbox")).unwrap().filter_map(Result::ok).filter(|e|e.file_name().to_string_lossy().starts_with("launch-")).count();assert_eq!(notices,1);}
    }
}

}

#[test]
#[cfg(target_os="linux")]
fn ticker_coordinator_prime_confirms_or_recovers_once_across_restart() {
    use std::{fs,time::{Duration,Instant},process::Stdio,os::unix::{fs::PermissionsExt,net::UnixListener}};
    for outcome in ["confirmed","lost"] {
        let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let r=root.to_str().unwrap();
        assert!(hp(home.path(),&["--root",r,"new","demo"]).status.success());let project=root.join("demo");
        let socket=home.path().join("session.sock");let _listener=UnixListener::bind(&socket).unwrap();
        let record=project.join(".state/coordinator.json");
        fs::write(&record,serde_json::json!({"socket":socket,"workspace_id":"w","tab_id":"tab","pane_id":"p","cwd":project,"agent_name":"coordinator","prime_pending":true,"prime_request":1}).to_string()).unwrap();
        let a=serde_json::json!({"workspace_id":"w","tab_id":"tab","pane_id":"p","cwd":project,"name":"coordinator","agent":"claude","agent_status":"idle","terminal_id":"terminal"});
        fs::write(home.path().join("agent.json"),a.to_string()).unwrap();fs::write(home.path().join("outcome"),outcome).unwrap();
        let fake=home.path().join("herdr");fs::write(&fake,r#"#!/usr/bin/python3
import os,json,sys,pathlib
root=pathlib.Path(os.environ['HOME']);args=sys.argv[1:];a=json.loads((root/'agent.json').read_text())
if args==['remote-api-bridge','--check']:print('herdr-api-bridge-v1')
elif args==['remote-api-bridge']:
 r=json.load(sys.stdin);assert r['method']=='agent.prompt';assert r['params']['target']=='p';assert ' context demo' in r['params']['text']
 with open(root/'sent','a') as f:f.write('send')
 if (root/'outcome').read_text()=='lost':sys.exit(1)
 print(json.dumps({'id':r['id'],'result':{'type':'agent_prompted','agent':a}}))
elif args==['agent','list']:print(json.dumps({'result':{'agents':[a]}}))
elif args==['pane','list']:
 with open(root/'polls','a') as f:f.write('poll')
 print(json.dumps({'result':{'panes':[a]}}))
elif args[:2] in [['agent','prompt'],['agent','start']]:
 (root/'WRONG_SYNC_EFFECT').touch();sys.exit(2)
else:print('{"result":{"shown":true}}')
"#).unwrap();fs::set_permissions(&fake,fs::Permissions::from_mode(0o700)).unwrap();
        struct Child(std::process::Child);impl Drop for Child {fn drop(&mut self){let _=self.0.kill();let _=self.0.wait();}}
        let spawn=||Child(Command::new(BIN).env_clear().env("HOME",home.path()).env("PATH","/usr/bin:/bin").env("HERDR_BIN_PATH",&fake).args(["--root",r,"ticker","run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
        let read=||->serde_json::Value{serde_json::from_str(&fs::read_to_string(&record).unwrap()).unwrap()};
        let wait=|child:&mut Child,predicate:&dyn Fn()->bool|{let end=Instant::now()+Duration::from_secs(30);while !predicate(){assert!(child.0.try_wait().unwrap().is_none());assert!(Instant::now()<end,"{}",fs::read_to_string(root.join(".ticker.log")).unwrap_or_default());std::thread::sleep(Duration::from_millis(10));}};
        let stop=|child:&mut Child|{fs::write(root.join(".ticker.stop"),b"").unwrap();let end=Instant::now()+Duration::from_secs(8);while child.0.try_wait().unwrap().is_none(){assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(10));}fs::remove_file(root.join(".ticker.stop")).unwrap();};
        let mut child=spawn();wait(&mut child,&||home.path().join("sent").exists()&&read()["prime_claim"]["delivery"]["phase"].as_str()==Some(if outcome=="confirmed"{"confirmed"}else{"pending"}));stop(&mut child);
        let polls=fs::read(home.path().join("polls")).unwrap().len();let mut child=spawn();wait(&mut child,&||fs::read(home.path().join("polls")).unwrap().len()>polls&&read()["prime_claim"]["delivery"]["notified"].as_bool()==Some(true));stop(&mut child);
        assert!(!home.path().join("WRONG_SYNC_EFFECT").exists());assert_eq!(fs::read(home.path().join("sent")).unwrap(),b"send");assert_eq!(read()["prime_sequence"],1);assert_eq!(read()["prime_pending"],outcome=="lost");
        if outcome=="lost" {assert_eq!(read()["prime_claim"]["delivery"]["phase"],"uncertain");assert_eq!(fs::read_dir(project.join("inbox")).unwrap().filter_map(Result::ok).filter(|e|e.file_name().to_string_lossy().starts_with("coordinator-prime-")).count(),1);}
    }
}

#[test]
#[cfg(target_os="linux")]
fn ticker_coordinator_start_then_prime_recover_without_replaying_start() {
    use std::{fs,time::{Duration,Instant},process::Stdio,os::unix::{fs::PermissionsExt,net::UnixListener}};
    for outcome in ["confirmed","lost"] {
        let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let r=root.to_str().unwrap();
        assert!(hp(home.path(),&["--root",r,"new","demo"]).status.success());let project=root.join("demo");
        let socket=home.path().join("session.sock");let _listener=UnixListener::bind(&socket).unwrap();
        let record=project.join(".state/coordinator.json");
        fs::write(&record,serde_json::json!({"socket":socket,"workspace_id":"w","tab_id":"tab","pane_id":"p","cwd":project,"agent_name":"coordinator","prime_pending":true,"prime_request":1}).to_string()).unwrap();
        let a=serde_json::json!({"workspace_id":"w","tab_id":"tab","pane_id":"p","cwd":project,"name":"coordinator","agent":"claude","agent_status":"idle","terminal_id":"terminal"});
        fs::write(home.path().join("agent.json"),a.to_string()).unwrap();fs::write(home.path().join("outcome"),outcome).unwrap();
        let fake=home.path().join("herdr");fs::write(&fake,r#"#!/usr/bin/python3
import os,json,sys,pathlib
root=pathlib.Path(os.environ['HOME']);args=sys.argv[1:];a=json.loads((root/'agent.json').read_text())
if args==['remote-api-bridge','--check']:print('herdr-api-bridge-v1')
elif args==['remote-api-bridge']:
 r=json.load(sys.stdin)
 if r['method']=='agent.start':
  assert r['params']=={'name':'coordinator','kind':'claude','pane_id':'p','args':[],'timeout_ms':20000}
  with open(root/'sent','a') as f:f.write('start')
  if (root/'outcome').read_text()=='lost':sys.exit(1)
  del a['agent'];a['launch_pending']=True;a['agent_status']='unknown'
  result={'type':'agent_started','agent':a,'argv':['claude']}
 elif r['method']=='agent.prompt':
  assert r['params']['target']=='p'
  with open(root/'primed','a') as f:f.write('prime')
  result={'type':'agent_prompted','agent':a}
 else:sys.exit(3)
 print(json.dumps({'id':r['id'],'result':result}))
elif args==['agent','list']:print(json.dumps({'result':{'agents':[a] if (root/'sent').exists() else []}}))
elif args==['pane','list']:
 with open(root/'polls','a') as f:f.write('poll')
 print(json.dumps({'result':{'panes':[a]}}))
elif args[:2] in [['agent','prompt'],['agent','start']]:
 (root/'WRONG_SYNC_EFFECT').touch();sys.exit(2)
else:print('{"result":{"shown":true}}')
"#).unwrap();fs::set_permissions(&fake,fs::Permissions::from_mode(0o700)).unwrap();
        struct Child(std::process::Child);impl Drop for Child {fn drop(&mut self){let _=self.0.kill();let _=self.0.wait();}}
        let spawn=||Child(Command::new(BIN).env_clear().env("HOME",home.path()).env("PATH","/usr/bin:/bin").env("HERDR_BIN_PATH",&fake).args(["--root",r,"ticker","run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
        let read=||->serde_json::Value{serde_json::from_str(&fs::read_to_string(&record).unwrap()).unwrap()};
        let wait=|child:&mut Child,predicate:&dyn Fn()->bool|{let end=Instant::now()+Duration::from_secs(30);while !predicate(){assert!(child.0.try_wait().unwrap().is_none());assert!(Instant::now()<end,"{}",fs::read_to_string(root.join(".ticker.log")).unwrap_or_default());std::thread::sleep(Duration::from_millis(10));}};
        let stop=|child:&mut Child|{fs::write(root.join(".ticker.stop"),b"").unwrap();let end=Instant::now()+Duration::from_secs(8);while child.0.try_wait().unwrap().is_none(){assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(10));}fs::remove_file(root.join(".ticker.stop")).unwrap();};
        let mut child=spawn();wait(&mut child,&||home.path().join("sent").exists()&&read()["launch_claim"]["phase"].as_str()==Some(if outcome=="confirmed"{"confirmed"}else{"pending"}));stop(&mut child);
        if outcome=="confirmed" {let mut child=spawn();wait(&mut child,&||read()["prime_pending"]==false);stop(&mut child);assert_eq!(fs::read(home.path().join("primed")).unwrap(),b"prime");}else{assert!(!home.path().join("primed").exists());}
        let polls=fs::read(home.path().join("polls")).unwrap().len();let mut child=spawn();wait(&mut child,&||fs::read(home.path().join("polls")).unwrap().len()>polls&&read()["launch_claim"]["notified"].as_bool()==Some(true));stop(&mut child);
        assert!(!home.path().join("WRONG_SYNC_EFFECT").exists());assert_eq!(fs::read(home.path().join("sent")).unwrap(),b"start");assert_eq!(read()["launch_sequence"],1);assert_eq!(read()["prime_pending"],outcome=="lost");
        if outcome=="lost" {assert_eq!(read()["launch_claim"]["phase"],"uncertain");assert_eq!(fs::read_dir(project.join("inbox")).unwrap().filter_map(Result::ok).filter(|e|e.file_name().to_string_lossy().starts_with("coordinator-start-")).count(),1);}
    }
}

#[test]
#[cfg(target_os="linux")]
fn ticker_notifications_recover_across_restart_and_reconcile_through_cli() {
    use std::{fs,time::{Duration,Instant},process::Stdio,os::unix::{fs::PermissionsExt,net::UnixListener}};
    for nudge in [false,true] {for outcome in ["confirmed","lost"] {
        let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let r=root.to_str().unwrap();assert!(hp(home.path(),&["--root",r,"new","demo"]).status.success());let p=root.join("demo");
        if nudge {let path=p.join("PROJECT.md");fs::write(&path,fs::read_to_string(&path).unwrap().replace("nudge = false","nudge = true")).unwrap();}
        let socket=home.path().join("session.sock");let _listener=UnixListener::bind(&socket).unwrap();fs::write(p.join(".state/coordinator.json"),serde_json::json!({"socket":socket,"workspace_id":"w","tab_id":"tab","pane_id":"p","cwd":p,"agent_name":"coordinator"}).to_string()).unwrap();
        let item=|id:&str|fs::write(p.join(format!("inbox/{id}.md")),format!("+++\nid='{id}'\nkind='test'\nsummary='Fixture'\n+++\n")).unwrap();item("item-a");
        let a=serde_json::json!({"workspace_id":"w","tab_id":"tab","pane_id":"p","cwd":p,"name":"coordinator","agent":"claude","agent_status":"idle","terminal_id":"terminal"});fs::write(home.path().join("agent.json"),a.to_string()).unwrap();fs::write(home.path().join("outcome"),outcome).unwrap();
        let fake=home.path().join("herdr");fs::write(&fake,r#"#!/usr/bin/python3
import os,json,sys,pathlib
root=pathlib.Path(os.environ['HOME']);args=sys.argv[1:];a=json.loads((root/'agent.json').read_text())
if args==['remote-api-bridge','--check']:print('herdr-api-bridge-v1')
elif args==['remote-api-bridge']:
 r=json.load(sys.stdin)
 if r['method'] in ['agent.list','pane.list','pane.report_metadata']:
  result={'agents':[a]} if r['method']=='agent.list' else {'panes':[a]} if r['method']=='pane.list' else {'type':'ok'}
  print(json.dumps({'id':r['id'],'result':result}));sys.exit(0)
 with open(root/'sent','a') as f:f.write('send')
 if (root/'outcome').read_text()=='lost':sys.exit(1)
 if r['method']=='agent.prompt':
  assert r['params']['text'].startswith('[herdr-projects ticker: automated, not the user, approves nothing]')
  result={'type':'agent_prompted','agent':a}
 else:
  assert r['method']=='notification.show'
  result={'type':'notification_show','shown':True,'reason':'shown'}
 print(json.dumps({'id':r['id'],'result':result}))
elif args==['agent','list']:print(json.dumps({'result':{'agents':[a]}}))
elif args==['pane','list']:
 with open(root/'polls','a') as f:f.write('poll')
 print(json.dumps({'result':{'panes':[a]}}))
elif args[:2] in [['agent','prompt'],['notification','show']]:
 (root/'WRONG_SYNC_EFFECT').touch();sys.exit(2)
else:print('{"result":{"shown":true}}')
"#).unwrap();fs::set_permissions(&fake,fs::Permissions::from_mode(0o700)).unwrap();
        struct Child(std::process::Child);impl Drop for Child{fn drop(&mut self){let _=self.0.kill();let _=self.0.wait();}}
        let command=||{let mut c=Command::new(BIN);c.env_clear().env("HOME",home.path()).env("PATH","/usr/bin:/bin").env("HERDR_BIN_PATH",&fake).args(["--root",r]);c};
        let spawn=||Child(command().args(["ticker","run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
        let record=p.join(".state/ticker.json");let read=||->serde_json::Value{serde_json::from_str(&fs::read_to_string(&record).unwrap_or_else(|_|"{}".into())).unwrap()};
        let wait=|child:&mut Child,predicate:&dyn Fn()->bool|{let end=Instant::now()+Duration::from_secs(40);while !predicate(){assert!(child.0.try_wait().unwrap().is_none());assert!(Instant::now()<end,"{}",fs::read_to_string(root.join(".ticker.log")).unwrap_or_default());std::thread::sleep(Duration::from_millis(10));}};
        let stop=|child:&mut Child|{fs::write(root.join(".ticker.stop"),b"").unwrap();let end=Instant::now()+Duration::from_secs(8);while child.0.try_wait().unwrap().is_none(){assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(10));}fs::remove_file(root.join(".ticker.stop")).unwrap();};
        let mut child=spawn();wait(&mut child,&||home.path().join("sent").exists()&&read()["notification_claim"]["phase"]==if outcome=="confirmed"{"confirmed"}else{"pending"});stop(&mut child);
        let polls=fs::read(home.path().join("polls")).unwrap().len();if outcome=="lost"{item("item-b");}
        let mut child=spawn();wait(&mut child,&||fs::read(home.path().join("polls")).unwrap().len()>polls&&read()["notification_claim"]["phase"]==if outcome=="confirmed"{"confirmed"}else{"uncertain"});
        assert_eq!(fs::read(home.path().join("sent")).unwrap(),b"send");
        if outcome=="lost" {
            let inspected=command().args(["notification","demo","inspect"]).output().unwrap();assert!(inspected.status.success());let value:serde_json::Value=serde_json::from_slice(&inspected.stdout).unwrap();assert_eq!(value["claim"]["phase"],"uncertain");
            let refused=command().args(["notification","demo","retry","--sequence","1"]).output().unwrap();assert!(!refused.status.success());assert_eq!(read()["notification_sequence"],1);
            fs::write(home.path().join("outcome"),"confirmed").unwrap();
            let args=if nudge{vec!["notification","demo","retry","--sequence","1","--accept-possible-duplicate"]}else{vec!["notification","demo","acknowledge","--sequence","1"]};
            let end=Instant::now()+Duration::from_secs(5);loop{let o=command().args(&args).output().unwrap();if o.status.success(){break;}assert!(Instant::now()<end,"{}",String::from_utf8_lossy(&o.stderr));std::thread::sleep(Duration::from_millis(20));}
            stop(&mut child);let mut child=spawn();wait(&mut child,&||read()["notification_claim"]["phase"]=="confirmed"&&read()["notification_sequence"]==2);stop(&mut child);
            assert_eq!(fs::read(home.path().join("sent")).unwrap(),b"sendsend");
            if nudge{assert_eq!(read()["notification_claim"]["retry_of"],1);}else{assert_eq!(read()["notification_claim"]["batch"]["ids"],serde_json::json!(["item-b"]));assert_eq!(read()["notification_suppressed"],serde_json::json!(["item-a"]));}
        }else{stop(&mut child);}
        assert!(!home.path().join("WRONG_SYNC_EFFECT").exists());
    }}
}

#[cfg(target_os="linux")]
#[test]
fn ticker_tokens_use_supervised_local_remote_and_coordinator_refreshes_after_restart() {
    use std::{fs,time::{Duration,Instant},process::Stdio,os::unix::{fs::PermissionsExt,net::UnixListener}};
    for mode in ["local","remote","coordinator"] {
        let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let r=root.to_str().unwrap();assert!(hp(home.path(),&["--root",r,"new","demo"]).status.success());let p=root.join("demo");
        let socket=home.path().join("session.sock");let _listener=UnixListener::bind(&socket).unwrap();let source=home.path().join("source");fs::create_dir(&source).unwrap();
        let a=serde_json::json!({"workspace_id":"w","tab_id":"tab","pane_id":"p","terminal_id":"terminal","cwd":source,"name":"worker","agent":"claude","agent_status":"working"});fs::write(home.path().join("agent.json"),a.to_string()).unwrap();fs::write(home.path().join("mode"),mode).unwrap();
        let mut c=serde_json::json!({"socket":socket});
        if mode=="coordinator" {for key in ["workspace_id","tab_id","pane_id","cwd"]{c[key]=a[key].clone();}c["agent_name"]="worker".into();}
        else{fs::write(p.join("threads/t-0001.toml"),toml::to_string(&serde_json::json!({"id":"t-0001","status":"open","kind":"adopted","machine":if mode=="remote"{"saved"}else{""},"thread_dir":source,"cwd":source,"workspace_id":"w","tab_id":"tab","pane_id":"p","agent":"claude","agent_name":"worker","created":jiff::Timestamp::now().to_string()})).unwrap()).unwrap();}
        fs::write(p.join(".state/coordinator.json"),c.to_string()).unwrap();
        fs::write(home.path().join("routes.json"),serde_json::json!([{"id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","label":"saved","target":"fixture.invalid","session":"named-session","enabled":true}]).to_string()).unwrap();
        let fake=home.path().join("herdr");fs::write(&fake,r#"#!/usr/bin/python3
import os,sys,json,pathlib
root=pathlib.Path(os.environ['HOME']);args=sys.argv[1:];a=json.loads((root/'agent.json').read_text());mode=(root/'mode').read_text()
remote=args[:2]==['--machine','saved']
if remote:args=args[2:]
if args[:2]==['--session','named-session']:args=args[2:];remote=True
if args==['machine','list','--json']:print((root/'routes.json').read_text())
elif args==['remote-api-bridge','--check']:print('herdr-api-bridge-v1')
elif args==['remote-api-bridge']:
 assert (mode=='remote')==remote
 r=json.load(sys.stdin)
 if r['method']=='agent.list':result={'agents':[a]}
 elif r['method']=='pane.list':result={'panes':[a]}
 elif r['method']=='pane.report_metadata':
  assert set(r['params'])=={'pane_id','source','ttl_ms','tokens'}
  assert r['params']['ttl_ms']==300000 and r['params']['source']=='herdr-projects'
  with open(root/'tokens','a') as f:f.write(json.dumps(r['params'])+'\n')
  result={'type':'ok'}
 else:sys.exit(3)
 print(json.dumps({'id':r['id'],'result':result}))
elif args==['agent','list']:print(json.dumps({'result':{'agents':[a] if remote or mode!='remote' else []}}))
elif args==['pane','list']:print(json.dumps({'result':{'panes':[a] if remote or mode!='remote' else []}}))
elif args[:2] in [['pane','report-metadata'],['agent','prompt'],['agent','start'],['notification','show']]:
 (root/'WRONG_SYNC_EFFECT').touch();sys.exit(2)
else:print('{"result":{"shown":true}}')
"#).unwrap();fs::set_permissions(&fake,fs::Permissions::from_mode(0o700)).unwrap();
        let ssh=home.path().join("ssh");fs::write(&ssh,"#!/usr/bin/python3\nimport sys,subprocess\nassert sys.argv[-2]=='fixture.invalid'\nif 'remote-api-bridge' in sys.argv[-1]:assert sys.argv[1:4]==['-T','-o','StrictHostKeyChecking=yes']\nsys.exit(subprocess.call(sys.argv[-1],shell=True))\n").unwrap();fs::set_permissions(&ssh,fs::Permissions::from_mode(0o700)).unwrap();
        struct Child(std::process::Child);impl Drop for Child{fn drop(&mut self){let _=self.0.kill();let _=self.0.wait();}}
        let read=||fs::read_to_string(home.path().join("tokens")).unwrap_or_default().lines().map(|s|serde_json::from_str::<serde_json::Value>(s).unwrap()).collect::<Vec<_>>();
        for expected in 1..=2 {
            let mut child=Child(Command::new(BIN).env_clear().env("HOME",home.path()).env("PATH",format!("{}:/usr/bin:/bin",home.path().display())).env("HERDR_BIN_PATH",&fake).env("HERDR_PROJECTS_REMOTE_HERDR_BIN",&fake).args(["--root",r,"ticker","run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
            let end=Instant::now()+Duration::from_secs(45);while read().len()<expected {assert!(child.0.try_wait().unwrap().is_none());assert!(Instant::now()<end,"{mode}: {}",fs::read_to_string(root.join(".ticker.log")).unwrap_or_default());std::thread::sleep(Duration::from_millis(10));}
            fs::write(root.join(".ticker.stop"),b"").unwrap();let end=Instant::now()+Duration::from_secs(8);while child.0.try_wait().unwrap().is_none(){assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(10));}fs::remove_file(root.join(".ticker.stop")).unwrap();
        }
        let values=read();assert_eq!(values.len(),2);assert_eq!(values[0],values[1]);assert_eq!(values[0]["tokens"]["thread"],if mode=="coordinator"{"coordinator"}else{"t-0001"});if mode!="coordinator"{assert_eq!(values[0]["tokens"]["review"],"working");}
        assert!(!home.path().join("WRONG_SYNC_EFFECT").exists());
    }
}

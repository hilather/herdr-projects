//! Surviving Linux supervision for fixed transfer commands. This contains local
//! descendants; it does not roll back filesystem or remote effects.
use std::{path::Path,time::{Duration,Instant}};
use anyhow::{Result,ensure,Context};
use crate::runner::{Cmd,InheritedLock,Cancellation,Output,RealRunner,Runner};

const CLEANUP_MARGIN:Duration=Duration::from_secs(5);
const ENVIRONMENT:&[&str]=&["HOME","USER","LOGNAME","PATH","LANG","LC_ALL","LC_CTYPE","TZ","TMPDIR",
    "SSH_AUTH_SOCK","SSH_AGENT_PID","XDG_CONFIG_HOME","XDG_CACHE_HOME","XDG_DATA_HOME","XDG_RUNTIME_DIR"];
const SCRIPT:&str=r#"duration=$1
shift
/usr/bin/timeout --foreground --signal=KILL -- "$duration" "$@"
result=$?
if [ "$result" -eq 0 ]; then exit 0; else exit 200; fi"#;

/// The original argv/stdin/output sink stay literal. Only the fixed supervisor
/// interprets shell syntax. Nonzero target exits normalize to 200; callers must
/// not depend on a vendor-specific failure status or infer durable success here.
/// Bootstrap always clears the environment and restores only bounded connection,
/// locale and user-path settings. Unsupported explicit environment settings fail.
pub fn run(target:Cmd,deadline:Instant,cancellation:Cancellation,locks:&[InheritedLock])->Result<Output> {
    // Materialize only at worker execution ingress, never at queue admission.
    // The living collector enforces the absolute deadline. After owner death,
    // the surviving helper's bound is relative to its start; no absolute orphan
    // deadline is claimed across arbitrary OS scheduling/bootstrap delays.
    RealRunner.run(&command(target,deadline,cancellation,locks)?)
}
fn command(mut target:Cmd,deadline:Instant,cancellation:Cancellation,locks:&[InheritedLock])->Result<Cmd> {
    ensure!(cfg!(target_os="linux"),"transfer supervision requires Linux PID namespaces");
    ensure!(!locks.is_empty()&&locks.len()<=16,"transfer execution ownership is required");
    ensure!(!cancellation.is_cancelled(),"transfer cancelled before command");
    ensure!(target.own_group&&!target.timeout.is_zero(),"transfer command requires bounded process ownership");
    target.env=safe_environment(&target)?;
    target.env_clear=true;target.env_remove.clear();
    trusted_helpers()?;
    let deadline=target.deadline.map_or(deadline,|end|end.min(deadline));
    let remaining=deadline.checked_duration_since(Instant::now()).context("transfer deadline elapsed")?;
    let duration=target.timeout.min(remaining.checked_sub(CLEANUP_MARGIN).context("insufficient transfer cleanup budget")?);
    ensure!(duration>=Duration::from_millis(1),"insufficient transfer execution budget");
    // Floor to milliseconds, never extend the caller's absolute budget.
    let millis=duration.as_millis();let seconds=format!("{}.{:03}s",millis/1000,millis%1000);
    let mut args=["--user","--map-root-user","--pid","--fork","--mount-proc","--kill-child=KILL","--",
        "/bin/sh","-c",SCRIPT,"transfer-supervisor"].map(str::to_owned).to_vec();
    args.push(seconds);args.push(target.program);args.extend(target.args);
    target.program="/usr/bin/unshare".into();target.args=args;
    target.timeout=duration+CLEANUP_MARGIN;target.deadline=Some(deadline);
    target.cancellation=Some(cancellation);target.inherited_locks=locks.to_vec();
    Ok(target)
}

fn safe_environment(target:&Cmd)->Result<Vec<(String,String)>> {
    let mut values=std::collections::BTreeMap::from([("PATH".to_string(),"/usr/bin:/bin".to_string()),("LANG".into(),"C".into()),("LC_ALL".into(),"C".into())]);
    if !target.env_clear {
        for &key in ENVIRONMENT {
            if target.env_remove.iter().any(|removed|removed==key){continue;}
            match std::env::var(key){Ok(value)=>{values.insert(key.into(),value);},Err(std::env::VarError::NotPresent)=>{},Err(_)=>anyhow::bail!("transfer environment variable {key} is not UTF-8")}
        }
    }
    for (key,value) in &target.env {
        // Session routing is allowed only when explicitly bound by the caller;
        // it is never inherited from the ambient supervisor environment.
        // Only fixed restrictive Git settings are admitted, explicitly, for the
        // system Git executable in an otherwise clean environment. They never
        // become ambient inheritance or a general GIT_* escape.
        let fixed_git=target.program=="/usr/bin/git" && target.env_clear && matches!((key.as_str(),value.as_str()),
            ("GIT_CONFIG_NOSYSTEM","1") | ("GIT_CONFIG_GLOBAL","/dev/null") |
            ("GIT_TERMINAL_PROMPT","0") | ("GIT_NO_LAZY_FETCH","1") | ("GIT_NO_REPLACE_OBJECTS","1"));
        ensure!(ENVIRONMENT.contains(&key.as_str())||key=="HERDR_SOCKET_PATH"||fixed_git,"unsupported transfer environment variable (name/value withheld)");
        values.insert(key.clone(),value.clone());
    }
    ensure!(values.values().all(|v|v.len()<=8192&&!v.contains('\0'))&&values.values().map(String::len).sum::<usize>()<=65536,"transfer environment exceeds bounds");
    Ok(values.into_iter().collect())
}

pub(crate) fn trusted_helpers()->Result<()> {
    for path in ["/usr/bin/unshare","/usr/bin/timeout","/bin/sh"] {trusted_helper(Path::new(path))?;}
    Ok(())
}
fn trusted_helper(path:&Path)->Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata=std::fs::metadata(path)?;
    // In a containing user namespace host root may map to the overflow uid.
    // This is a sanity check under a trusted OS, not independent attestation.
    let system_owner=std::fs::metadata("/")?.uid();
    ensure!(metadata.is_file()&&metadata.uid()==system_owner&&metadata.mode()&0o022==0&&metadata.mode()&0o111!=0,
        "supervision helper must have trusted system ownership and executable file mode");
    Ok(())
}

#[cfg(all(test,target_os="linux"))]
mod tests {
    use super::*;
    use crate::execution_guard::{ProjectGuard,RootGuard};
    fn fixture()->(tempfile::TempDir,std::path::PathBuf,ProjectGuard) {
        let root=tempfile::tempdir().unwrap();let project=root.path().join("project");std::fs::create_dir_all(project.join(".state")).unwrap();
        let guard=ProjectGuard::acquire(&project).unwrap();(root,project,guard)
    }
    #[test]
    fn explicit_session_routing_is_bounded_and_never_an_inherited_setting() {
        assert!(!ENVIRONMENT.contains(&"HERDR_SOCKET_PATH"));
        let command=Cmd::new("herdr",Duration::from_secs(1)).env("HERDR_SOCKET_PATH","/tmp/recorded.sock");
        assert!(safe_environment(&command).unwrap().contains(&("HERDR_SOCKET_PATH".into(),"/tmp/recorded.sock".into())));
        assert!(safe_environment(&command.clone().env("HERDR_SESSION","ambient")).is_err());
        assert!(safe_environment(&Cmd::new("herdr",Duration::from_secs(1)).env("HERDR_SOCKET_PATH","x".repeat(8193))).is_err());
    }
    #[test]
    fn literal_arguments_stdin_environment_and_binary_sink_survive_supervision() {
        let(_root,project,guard)=fixture();let locks=guard.inherit_transfer().unwrap();
        let literal="space ' $(touch should-not-exist); --option";
        let mut target=Cmd::new("/usr/bin/python3",Duration::from_secs(1)).args(["-c","import os,sys; sys.stdout.buffer.write(sys.stdin.buffer.read()+b'|'+sys.argv[1].encode()+b'|'+os.environ['SSH_AUTH_SOCK'].encode()+bytes([0,255]))",literal])
            .stdin("input").cwd(&project).env("SSH_AUTH_SOCK","literal-value");
        target.env_clear=true;let destination=project.join("binary");target.stdout_file=Some((destination.clone(),4096));
        let output=run(target,Instant::now()+Duration::from_secs(8),Cancellation::default(),&locks).unwrap();assert!(output.success(),"{}",output.error_text());
        let mut expected=format!("input|{literal}|literal-value").into_bytes();expected.extend([0,255]);
        assert_eq!(std::fs::read(destination).unwrap(),expected);assert!(!project.join("should-not-exist").exists());
    }
    #[test]
    fn failures_caps_cancellation_and_original_deadline_are_preserved() {
        let(_root,project,guard)=fixture();let locks=guard.inherit_transfer().unwrap();
        let make=||Cmd::new("/bin/sh",Duration::from_millis(100)).args(["-c","printf 123456789; exit 7"]).cwd(&project);
        let token=Cancellation::default();token.cancel();assert!(command(make(),Instant::now()+Duration::from_secs(8),token,&locks).is_err());
        let mut expired=make();expired.deadline=Some(Instant::now());assert!(command(expired,Instant::now()+Duration::from_secs(8),Cancellation::default(),&locks).is_err());
        assert!(command(make(),Instant::now()+Duration::from_secs(4),Cancellation::default(),&locks).is_err());
        let mut target=make();target.capture_limit=4;
        let out=run(target,Instant::now()+Duration::from_secs(8),Cancellation::default(),&locks).unwrap();
        assert_eq!(out.code,Some(200));assert_eq!(out.stdout_bytes,b"1234");assert!(out.stdout_truncated);
        let target=Cmd::new("/bin/sh",Duration::from_millis(100)).args(["-c","sleep 10"]).cwd(&project);
        let began=Instant::now();let out=run(target,Instant::now()+Duration::from_secs(8),Cancellation::default(),&locks).unwrap();
        assert_eq!(out.code,Some(200));assert!(began.elapsed()<Duration::from_secs(2));
        let token=Cancellation::default();let cancel=token.clone();
        let worker=std::thread::spawn(move||{std::thread::sleep(Duration::from_millis(100));cancel.cancel();});
        let target=Cmd::new("/bin/sh",Duration::from_secs(10)).args(["-c","sleep 10"]).cwd(&project);
        let out=run(target,Instant::now()+Duration::from_secs(20),token,&locks).unwrap();worker.join().unwrap();assert!(out.cancelled);
    }
    #[test]
    fn environment_helper() {
        let Some(project)=std::env::var_os("HP_TRANSFER_ENVIRONMENT_PROJECT") else{return;};let project=std::path::PathBuf::from(project);
        let guard=ProjectGuard::acquire(&project).unwrap();let locks=guard.inherit_transfer().unwrap();
        let target=Cmd::new("/usr/bin/env",Duration::from_secs(1)).cwd(&project);
        let wrapped=command(target,Instant::now()+Duration::from_secs(8),Cancellation::default(),&locks).unwrap();
        assert!(wrapped.env_clear);assert!(wrapped.env.iter().all(|(name,_)|ENVIRONMENT.contains(&name.as_str())));
        let out=RealRunner.run(&wrapped).unwrap();assert!(out.success());
        assert!(!out.stdout.lines().any(|line|["LD_","BASH_ENV=","ENV=","GLIBC_TUNABLES="].iter().any(|key|line.starts_with(key))));
    }
    #[test]
    fn loader_and_shell_settings_do_not_reach_supervision_and_explicit_overrides_refuse() {
        let(_root,project,guard)=fixture();let locks=guard.inherit_transfer().unwrap();
        for key in ["LD_PRELOAD","LD_LIBRARY_PATH","BASH_ENV","ENV","GLIBC_TUNABLES","UNKNOWN_SECRET"] {
            let target=Cmd::new("/bin/true",Duration::from_secs(1)).env(key,"sensitive-value");
            let error=run(target,Instant::now()+Duration::from_secs(8),Cancellation::default(),&locks).unwrap_err().to_string();
            assert!(!error.contains("sensitive-value")&&!error.contains(key));
        }
        drop(locks);drop(guard);
        let startup=project.join("startup");std::fs::write(&startup,"touch STARTUP_MUST_NOT_RUN").unwrap();
        let status=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","supervision::tests::environment_helper","--nocapture"])
            .env("HP_TRANSFER_ENVIRONMENT_PROJECT",&project).env("LD_LIBRARY_PATH","/nonexistent-transfer-fixture")
            .env("BASH_ENV",&startup).env("ENV",&startup).current_dir(&project)
            .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().unwrap();
        assert!(status.success());assert!(!project.join("STARTUP_MUST_NOT_RUN").exists());
    }
    #[test]
    fn restrictive_git_settings_require_exact_values_clean_environment_and_system_git() {
        let allowed=Cmd::new("/usr/bin/git",Duration::from_secs(1)).env("GIT_CONFIG_GLOBAL","/dev/null");
        let mut allowed=allowed;allowed.env_clear=true;
        assert!(safe_environment(&allowed).unwrap().contains(&("GIT_CONFIG_GLOBAL".into(),"/dev/null".into())));
        let mut inherited=allowed.clone();inherited.env_clear=false;assert!(safe_environment(&inherited).is_err());
        let mut foreign=allowed.clone();foreign.program="/tmp/git".into();assert!(safe_environment(&foreign).is_err());
        for (key,value) in [("GIT_CONFIG_GLOBAL","/tmp/private"),("GIT_NO_REPLACE_OBJECTS","0"),("GIT_SSH_COMMAND","private-command")] {
            let mut altered=allowed.clone();altered.env=vec![(key.into(),value.into())];
            let error=safe_environment(&altered).unwrap_err().to_string();assert!(!error.contains(value));
        }
    }
    #[test]
    fn owner_death_helper() {
        let Some(project)=std::env::var_os("HP_TRANSFER_OWNER_DEATH_PROJECT") else{return;};let project=std::path::PathBuf::from(project);
        let guard=ProjectGuard::acquire(&project).unwrap();let locks=guard.inherit_transfer().unwrap();
        // The target deliberately closes all non-stdio FDs, as an SSH client
        // may do. Ownership must live in the fixed supervising processes.
        let script="import os,time\nos.closerange(3,1024)\nopen('started','w').close()\nif os.fork()==0:\n os.setsid()\n time.sleep(2)\n open('escaped','w').close()\n os._exit(0)\ntime.sleep(10)";
        let target=Cmd::new("/usr/bin/python3",Duration::from_millis(700)).args(["-c",script]).cwd(&project);
        run(target,Instant::now()+Duration::from_secs(8),Cancellation::default(),&locks).unwrap();
    }
    #[test]
    fn owner_sigkill_keeps_exclusion_until_bounded_cleanup_even_when_target_closes_fds() {
        let(root,project,guard)=fixture();drop(guard);
        let other=root.path().join("other");std::fs::create_dir_all(other.join(".state")).unwrap();
        let mut owner=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","supervision::tests::owner_death_helper","--nocapture"])
            .env("HP_TRANSFER_OWNER_DEATH_PROJECT",&project).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
        let deadline=Instant::now()+Duration::from_secs(4);
        while !project.join("started").exists() {
            if owner.try_wait().unwrap().is_some(){panic!("transfer owner exited before target started");}
            if Instant::now()>=deadline{let _=owner.kill();let _=owner.wait();panic!("transfer start deadline");}
            std::thread::sleep(Duration::from_millis(5));
        }
        owner.kill().unwrap();owner.wait().unwrap();
        assert!(RootGuard::exclusive(root.path()).is_err());assert!(ProjectGuard::acquire(&project).is_err());
        let independent=ProjectGuard::acquire(&other).unwrap();assert!(independent.inherit_transfer().is_err());drop(independent);
        let deadline=Instant::now()+Duration::from_secs(4);
        loop {if let Ok(guard)=RootGuard::exclusive(root.path()){drop(guard);break;}assert!(Instant::now()<deadline,"orphaned transfer held ownership past supervisor deadline");std::thread::sleep(Duration::from_millis(10));}
        std::thread::sleep(Duration::from_millis(1500));assert!(!project.join("escaped").exists());
        let guard=ProjectGuard::acquire(&project).unwrap();assert!(guard.inherit_transfer().is_ok());
    }
}

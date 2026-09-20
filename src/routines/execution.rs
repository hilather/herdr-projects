//! Linux process containment for approved routine scripts. This is not a
//! filesystem/network sandbox and does not roll back external effects.
use std::{path::Path, time::Duration};
use anyhow::{Result, ensure};
use crate::runner::{Cancellation, Cmd, Output, RealRunner, Runner};

// Only the trusted namespace init emits these exit statuses. All script
// failures, including timeout's status, are normalized away from unshare's
// bootstrap/wait error status (1). Never parse script output as cleanup proof.
const SUPERVISOR: &str = r#"/usr/bin/timeout --foreground --signal=KILL "$1" /bin/sh -s
result=$?
if [ "$result" -eq 0 ]; then exit 0; else exit 200; fi"#;
const CLEANUP_MARGIN: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub(super) struct Completion {
    pub(super) output: Output,
    pub(super) cleanup_verified: bool,
    pub(super) succeeded: bool,
}

/// No injected Runner or public constructor may mint a cleanup result.
/// The caller retains ownership, validates the durable claim immediately before
/// entry, and commits the result bound to that claim before releasing overlap.
#[cfg(test)]
fn run(script: &[u8], cwd: &Path, deadline_ms: u64, cap: u32,
    cancellation: Cancellation) -> Result<Completion>
{
    run_until(script,cwd,deadline_ms,cap,cancellation,None,Vec::new())
}

pub(super) fn run_until(script:&[u8],cwd:&Path,deadline_ms:u64,cap:u32,cancellation:Cancellation,
    deadline:Option<std::time::Instant>,inherited_locks:Vec<crate::runner::InheritedLock>)->Result<Completion>
{
    preflight(script,deadline_ms,cap)?;
    let script=std::str::from_utf8(script).map_err(|_|anyhow::anyhow!("routine script must be UTF-8"))?;
    let mut command=command(script,cwd,deadline_ms,cap,cancellation);
    command.deadline=deadline;
    command.inherited_locks=inherited_locks;
    if let Some(end)=deadline {command.timeout=command.timeout.min(end.saturating_duration_since(std::time::Instant::now()));}
    run_command(&command)
}

fn command(script:&str,cwd:&Path,deadline_ms:u64,cap:u32,cancellation:Cancellation)->Cmd {
    let duration=format!("{}.{:03}s",deadline_ms/1000,deadline_ms%1000);
    let mut command=Cmd::new("/usr/bin/unshare",Duration::from_millis(deadline_ms)+CLEANUP_MARGIN)
        .args(["--user","--map-root-user","--pid","--fork","--mount-proc","--kill-child=KILL","--",
            "/bin/sh","-c",SUPERVISOR,"routine-supervisor",&duration])
        .cwd(cwd).stdin(script).env("PATH","/usr/bin:/bin").env("LANG","C").env("LC_ALL","C");
    // Clear before exec: `env -i` inside the helper would be too late to exclude
    // inherited dynamic-loader variables such as LD_PRELOAD.
    command.env_clear=true;
    command.capture_limit=cap as usize;
    command.cancellation=Some(cancellation);
    command
}

fn run_command(command:&Cmd)->Result<Completion> {
    let output=RealRunner.run(command)?;
    let cleanup_verified=!output.timed_out && !output.cancelled && matches!(output.code,Some(0|200));
    let succeeded=cleanup_verified && output.code==Some(0);
    Ok(Completion{output,cleanup_verified,succeeded})
}

pub(super) fn preflight(script:&[u8],deadline_ms:u64,cap:u32)->Result<()> {
    ensure!(cfg!(target_os="linux"), "verified routine execution requires Linux PID namespaces");
    ensure!((1..=60_000).contains(&deadline_ms) && (1..=65_536).contains(&cap), "invalid routine execution bounds");
    ensure!(script.len()<=65_536 && !script.contains(&0), "invalid routine script bytes");
    std::str::from_utf8(script).map_err(|_|anyhow::anyhow!("routine script must be UTF-8"))?;
    // Fixed system helpers, not PATH resolution or project-provided wrappers.
    // The OS installation is trusted; pinning every linked library is outside
    // the signed script-byte contract.
    crate::supervision::trusted_helpers()
}

#[cfg(all(test,target_os="linux"))]
mod tests {
    use super::*;
    #[test]
    fn normal_failure_and_output_caps_keep_cleanup_separate_from_success() {
        let dir=tempfile::tempdir().unwrap();
        let ok=run(b"printf hello; printf error >&2",dir.path(),1000,32,Cancellation::default()).unwrap();
        assert!(ok.cleanup_verified && ok.succeeded,"{ok:?}");
        assert_eq!(ok.output.stdout_bytes,b"hello");assert_eq!(ok.output.stderr_bytes,b"error");
        let failed=run(b"exit 1",dir.path(),1000,32,Cancellation::default()).unwrap();
        assert!(failed.cleanup_verified && !failed.succeeded,"{failed:?}");
        assert_eq!(failed.output.code,Some(200));
        let capped=run(b"printf 123456789; printf abcdefghi >&2",dir.path(),1000,4,Cancellation::default()).unwrap();
        assert!(capped.cleanup_verified && capped.succeeded,"{capped:?}");
        assert_eq!(capped.output.stdout_bytes,b"1234");assert_eq!(capped.output.stderr_bytes,b"abcd");
        assert!(capped.output.stdout_truncated && capped.output.stderr_truncated);
    }

    #[test]
    fn normal_exit_and_deadline_kill_detached_descendants_even_with_closed_pipes() {
        let dir=tempfile::tempdir().unwrap();
        // No numeric-PID cleanup: a surviving setsid child would write this
        // marker after the supervisor returns. Test open AND redirected pipes.
        for (index,ending) in ["exit 0","wait"].iter().enumerate() {
            let script=format!("setsid /bin/sh -c 'sleep 0.5; echo escaped > escaped-{index}' >/dev/null 2>&1 &\n{ending}\n");
            let out=run(script.as_bytes(),dir.path(),100,32,Cancellation::default()).unwrap();
            assert!(out.cleanup_verified,"{out:?}");assert_eq!(out.succeeded,index==0);
        }
        let out=run(b"setsid /bin/sh -c 'sleep 0.5; echo escaped > open-pipes' &\nwait",dir.path(),100,32,Cancellation::default()).unwrap();
        assert!(out.cleanup_verified && !out.succeeded,"{out:?}");
        std::thread::sleep(Duration::from_millis(650));
        assert!(!dir.path().join("escaped-0").exists());assert!(!dir.path().join("escaped-1").exists());
        assert!(!dir.path().join("open-pipes").exists());
    }

    #[test]
    fn cancellation_never_certifies_cleanup_and_monitor_failure_is_not_success() {
        let dir=tempfile::tempdir().unwrap();let token=Cancellation::default();token.cancel();
        let before=run(b"echo unsafe > marker",dir.path(),1000,32,token).unwrap();
        assert!(!before.cleanup_verified && before.output.cancelled);assert!(!dir.path().join("marker").exists());
        // PID1 ignores signals from inside its own namespace. Killing the
        // timeout monitor instead is a failed run with verified cleanup.
        let killed=run(b"kill -KILL $PPID",dir.path(),1000,32,Cancellation::default()).unwrap();
        assert!(killed.cleanup_verified && !killed.succeeded,"{killed:?}");
        let token=Cancellation::default();let other=token.clone();
        let worker=std::thread::spawn(move || {std::thread::sleep(Duration::from_millis(100));other.cancel();});
        let during=run(b"sleep 10",dir.path(),1000,32,token).unwrap();worker.join().unwrap();
        assert!(!during.cleanup_verified && during.output.cancelled,"{during:?}");
    }

    #[test]
    fn bootstrap_refusal_and_outer_deadline_leave_cleanup_unknown() {
        let dir=tempfile::tempdir().unwrap();
        let mut bootstrap=command("echo unsafe > marker",dir.path(),1000,128,Cancellation::default());
        bootstrap.args.insert(0,"--invalid-routine-fixture-option".into());
        let result=run_command(&bootstrap).unwrap();
        assert_eq!(result.output.code,Some(1));assert!(!result.cleanup_verified && !result.succeeded);
        assert!(!dir.path().join("marker").exists());
        let mut outer=command("trap '' TERM INT; setsid /bin/sh -c 'sleep 0.5; echo unsafe > escaped' >/dev/null 2>&1 & wait",dir.path(),10_000,128,Cancellation::default());
        outer.timeout=Duration::from_millis(100);
        let result=run_command(&outer).unwrap();assert!(result.output.timed_out);
        assert!(!result.cleanup_verified && !result.succeeded);
        std::thread::sleep(Duration::from_millis(650));
        assert!(!dir.path().join("escaped").exists());
    }

    #[test]
    fn invalid_inputs_refuse_before_any_script_and_environment_is_explicit() {
        let dir=tempfile::tempdir().unwrap();
        for script in [b"\xff".as_slice(),b"\0"] {assert!(run(script,dir.path(),1000,32,Cancellation::default()).is_err());}
        for deadline in [0,60_001] {assert!(run(b"true",dir.path(),deadline,32,Cancellation::default()).is_err());}
        let out=run(b"/usr/bin/env",dir.path(),1000,1024,Cancellation::default()).unwrap();
        assert!(out.cleanup_verified && out.succeeded,"{out:?}");
        for line in out.output.stdout.lines() {assert!(line.starts_with("PATH=")||line=="LANG=C"||line=="LC_ALL=C"||line.starts_with("PWD=")||line=="_=/usr/bin/env"||line=="SHLVL=2","unexpected inherited environment: {line}");}
    }
}

//! Executes policy repetitions in the one copied checkout. The outer runner
//! owns the single deadline, cancellation and namespace-wide child cleanup.
use crate::{domain::verification_policy::ExecutionPolicy, execution_guard::GatedSpawn};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    path::Path,
    process::{Child, Command, Stdio},
    thread,
};

/// CLOCK_MONOTONIC is shared across the existing user/pid/mount namespaces.
pub(super) fn monotonic_ms() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes a valid timespec; this Linux clock is available.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } != 0 {
        return 0;
    }
    (time.tv_sec as u64)
        .saturating_mul(1000)
        .saturating_add(time.tv_nsec as u64 / 1_000_000)
}
fn budget() -> std::io::Result<()> {
    let deadline = std::env::var("HP_VERIFY_DEADLINE_MONOTONIC_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok());
    if deadline.is_none_or(|deadline| monotonic_ms() >= deadline) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "verification budget exhausted",
        ));
    }
    Ok(())
}

struct Running {
    child: Child,
    stdout: thread::JoinHandle<(Vec<u8>, bool)>,
    stderr: thread::JoinHandle<()>,
    observation: Value,
}
fn start(
    args: &[String],
    checkout: &Path,
    sequence: &mut u64,
    kind: &str,
    name: &str,
    repetition: u8,
    source_sequence: Option<u64>,
) -> std::io::Result<Running> {
    budget()?;
    let observation = json!({"sequence": *sequence, "kind": kind, "check": name,
        "repetition": repetition, "source_sequence": source_sequence, "outcome": "running", "exit_status": null,
        "load": super::evidence::repetition_load(),
        "tests": {"status":"unavailable","reason":"incomplete","results":[]}});
    *sequence += 1;
    eprintln!("hp-verify observation={observation}");
    let mut child = Command::new(&args[0])
        .args(&args[1..])
        .current_dir(checkout)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn_gated()?;
    let mut output = child.stdout.take().expect("piped stdout");
    let stdout = thread::spawn(move || {
        let mut retained = Vec::new();
        let mut truncated = false;
        let mut buffer = [0; 4096];
        while let Ok(n) = output.read(&mut buffer) {
            if n == 0 {
                break;
            }
            let _ = std::io::stdout().write_all(&buffer[..n]);
            let remaining = 8192_usize.saturating_sub(retained.len());
            retained.extend_from_slice(&buffer[..n.min(remaining)]);
            truncated |= n > remaining;
        }
        (retained, truncated)
    });
    // Check stderr cannot inject supervisor observation records.
    let mut errors = child.stderr.take().expect("piped stderr");
    let stderr = thread::spawn(move || {
        let _ = std::io::copy(&mut errors, &mut std::io::sink());
    });
    Ok(Running {
        child,
        stdout,
        stderr,
        observation,
    })
}
fn finish(mut running: Running) -> std::io::Result<Option<i32>> {
    let status = running.child.wait()?;
    let (bytes, truncated) = running.stdout.join().unwrap_or_default();
    let _ = running.stderr.join();
    let code = status.code();
    running.observation["outcome"] = json!(if status.success() { "pass" } else { "fail" });
    running.observation["exit_status"] = json!(status.code());
    let mut tests = if truncated {
        json!({"status":"unavailable","reason":"output_limit","results":[]})
    } else {
        super::evidence::test_results(&String::from_utf8_lossy(&bytes))
    };
    // At most 651 observations; keep the complete metadata under its 2 MiB cap.
    if serde_json::to_vec(&tests).map_or(true, |v| v.len() > 1024) {
        tests = json!({"status":"unavailable","reason":"metadata_limit","results":[]});
    }
    running.observation["tests"] = tests;
    eprintln!("hp-verify observation={}", running.observation);
    budget()?;
    Ok(code)
}
fn check(
    policy: &ExecutionPolicy,
    args: &[String],
    checkout: &Path,
    sequence: &mut u64,
    kind: &str,
    name: &str,
    repetition: u8,
) -> std::io::Result<Option<i32>> {
    let source_sequence = *sequence;
    let first = finish(start(
        args, checkout, sequence, kind, name, repetition, None,
    )?)?;
    if first != Some(0) {
        reruns(policy, args, checkout, sequence, source_sequence, name)?;
    }
    Ok(first)
}
fn reruns(
    policy: &ExecutionPolicy,
    args: &[String],
    checkout: &Path,
    sequence: &mut u64,
    source_sequence: u64,
    name: &str,
) -> std::io::Result<()> {
    for rerun in 1..=policy.rerun_on_failure {
        let code = finish(start(
            args,
            checkout,
            sequence,
            "flake",
            name,
            rerun,
            Some(source_sequence),
        )?)?;
        if code == Some(0) {
            break;
        }
    }
    Ok(())
}
pub(super) fn execute(policy: &ExecutionPolicy, checkout: &Path) -> std::io::Result<Option<i32>> {
    let mut sequence = 0;
    let mut code = check(
        policy,
        &policy.checks,
        checkout,
        &mut sequence,
        "check",
        "checks",
        0,
    )?;
    if let Some(stress) = &policy.stress {
        for repetition in 1..=stress.repetitions {
            for name in &stress.checks {
                let args = &policy.named_checks[name];
                let load = stress.load.as_deref().unwrap_or(args);
                let mut children = Vec::new();
                for _ in 0..stress.concurrency {
                    children.push(start(
                        load,
                        checkout,
                        &mut sequence,
                        "load",
                        name,
                        repetition,
                        None,
                    )?);
                }
                let result = check(
                    policy,
                    args,
                    checkout,
                    &mut sequence,
                    "stress",
                    name,
                    repetition,
                )?;
                if code == Some(0) && result != Some(0) {
                    code = result;
                }
                for child in children {
                    let source_sequence = child.observation["sequence"]
                        .as_u64()
                        .expect("native sequence");
                    let result = finish(child)?;
                    if result != Some(0) {
                        reruns(policy, load, checkout, &mut sequence, source_sequence, name)?;
                    }
                    if code == Some(0) && result != Some(0) {
                        code = result;
                    }
                }
            }
        }
    }
    budget()?;
    Ok(code)
}

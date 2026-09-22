//! Linux worker command construction for a persistent Herdr terminal. A dedicated
//! PID namespace keeps detached descendants inside the worker's lifetime.
//! Submission/observation/approval belong to the canonical launch service.
use anyhow::{Result, ensure};
use std::path::Path;

#[cfg(target_os = "linux")]
mod observation;
#[cfg(target_os = "linux")]
pub use observation::{
    AgentProcessObservation, GateObservation, ProcessMarkerObservation, SupervisorObservation,
};

/// Persistent Linux pidfs identities are meaningful only within the recorded
/// boot and observer PID namespace. They are evidence data, not stop authority.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupervisorIdentity {
    pub version: u32,
    pub boot_id: String,
    /// Hashed local machine ID. Historical v1 records remain usable only in
    /// their original boot; they cannot prove same-host reboot termination.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    pub observer_namespace: (u64, u64),
    pub worker_namespace: (u64, u64),
    pub outer: ProcessIncarnation,
    pub init: ProcessIncarnation,
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessIncarnation {
    pub pid: u32,
    pub device: u64,
    pub inode: u64,
}

/// Audit data produced by a local same-host reboot observation. Data alone is
/// not authority; termination accepts it only through its sealed producer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostRebootEvidence {
    pub version: u32,
    pub host_id: String,
    pub previous_boot_id: String,
    pub current_boot_id: String,
}
impl HostRebootEvidence {
    pub fn validate_for(&self, identity:&SupervisorIdentity)->Result<()> {
        identity.validate()?;
        let mut current=identity.clone();current.boot_id=self.current_boot_id.clone();current.validate()?;
        ensure!(self.version==1 && identity.version==2
            && identity.host_id.as_deref()==Some(self.host_id.as_str())
            && identity.boot_id==self.previous_boot_id && self.current_boot_id!=self.previous_boot_id,
            "reboot evidence differs from retained supervisor host or boot");
        Ok(())
    }
}

impl SupervisorIdentity {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            ((self.version == 1 && self.host_id.is_none())
                || (self.version == 2 && self.host_id.as_ref().is_some_and(|id|
                    id.len() == 64 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))))
                && self.boot_id.len() == 36
                && self
                    .boot_id
                    .bytes()
                    .enumerate()
                    .all(|(i, b)| if [8, 13, 18, 23].contains(&i) {
                        b == b'-'
                    } else {
                        b.is_ascii_hexdigit()
                    })
                && self.observer_namespace.1 > 0
                && self.worker_namespace.1 > 0
                && self.observer_namespace != self.worker_namespace
                && self.outer.pid > 1
                && self.init.pid > 1
                && self.outer.pid <= i32::MAX as u32
                && self.init.pid <= i32::MAX as u32
                && self.outer.pid != self.init.pid
                && self.outer.inode > 0
                && self.init.inode > 0
                && (self.outer.device, self.outer.inode) != (self.init.device, self.init.inode),
            "invalid supervisor incarnation"
        );
        Ok(())
    }
}

/// Literal argv for the trusted Linux supervisor. This is not a shell command
/// and does not execute, grant authority, or assert that the kernel supports it.
pub fn command(
    executable: &Path,
    arguments: &[String],
    max_wall_seconds: u64,
) -> Result<Vec<String>> {
    ensure!(
        cfg!(target_os = "linux"),
        "canonical worker supervision requires Linux"
    );
    let executable = executable
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("worker executable path is not UTF-8"))?;
    ensure!(
        Path::new(executable).is_absolute()
            && executable.len() <= 4096
            && !executable.chars().any(char::is_control),
        "worker executable requires a bounded absolute path"
    );
    ensure!(
        arguments.len() <= 128
            && arguments.iter().map(String::len).sum::<usize>() <= 32768
            && arguments
                .iter()
                .all(|arg| !arg.chars().any(char::is_control)),
        "worker arguments exceed native protocol bounds"
    );
    ensure!(
        (1..=7 * 24 * 60 * 60).contains(&max_wall_seconds),
        "worker wall deadline must be between one second and seven days"
    );
    // timeout remains namespace PID 1. Once it exits, the kernel removes all
    // namespace descendants, including children that call setsid or double-fork.
    // The host-side unshare process waits for PID 1 and kills it on parent death.
    let mut argv = vec![
        "/usr/bin/unshare".into(),
        "--user".into(),
        "--map-root-user".into(),
        "--pid".into(),
        "--fork".into(),
        "--mount-proc".into(),
        "--kill-child=KILL".into(),
        "--".into(),
        "/usr/bin/timeout".into(),
        "--foreground".into(),
        "--signal=TERM".into(),
        "--kill-after=5s".into(),
        "--".into(),
        format!("{max_wall_seconds}s"),
        executable.into(),
    ];
    argv.extend_from_slice(arguments);
    Ok(argv)
}

/// A literal-argv bootstrap that cannot execute the agent before a single exact
/// release line arrives. The namespace wall deadline includes the waiting stage.
/// The token is a routing fence, not a substitute for durable launch authority.
pub fn gated_command(
    executable: &Path,
    arguments: &[String],
    max_wall_seconds: u64,
    token: &str,
) -> Result<Vec<String>> {
    command(executable, arguments, max_wall_seconds)?;
    ensure!(
        !token.is_empty()
            && token.len() <= 256
            && token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)),
        "invalid worker release fence"
    );
    let mut args = vec![
        "-c".into(),
        "IFS= read -r release && [ \"$release\" = \"$1\" ] || exit 125; shift; exec \"$@\"".into(),
        "herdr-projects-worker-gate".into(),
        token.into(),
        executable
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("worker executable path is not UTF-8"))?
            .into(),
    ];
    args.extend_from_slice(arguments);
    command(Path::new("/bin/sh"), &args, max_wall_seconds)
}

/// Explicit baseline environment for the agent. Only the credential-store home
/// is variable; secrets and arbitrary inherited loader/config hooks are excluded.
/// The waiting gate still runs in the native terminal's environment, but its
/// eventual exec clears that environment before executing the approved agent.
pub fn isolated_gated_command(
    executable: &Path,
    arguments: &[String],
    wall: u64,
    token: &str,
    home: &Path,
) -> Result<Vec<String>> {
    let home = home
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("execution home is not UTF-8"))?;
    ensure!(
        Path::new(home).is_absolute() && home.len() <= 4096 && !home.chars().any(char::is_control),
        "invalid execution home"
    );
    command(executable, arguments, wall)?;
    let mut args = vec![
        "-i".into(),
        format!("HOME={home}"),
        "PATH=/usr/bin:/bin".into(),
        "LANG=C.UTF-8".into(),
        "LC_ALL=C.UTF-8".into(),
        "TERM=xterm-256color".into(),
        executable
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("agent executable is not UTF-8"))?
            .into(),
    ];
    args.extend_from_slice(arguments);
    gated_command(Path::new("/usr/bin/env"), &args, wall, token)
}

/// Native pane.run joins command arguments as shell source. A dispatcher must
/// supply one fully quoted command, after verifying a POSIX-compatible shell.
/// This encoder is not valid for fish, PowerShell, or cmd.exe.
pub fn posix_command(argv: &[String]) -> Result<String> {
    ensure!(
        !argv.is_empty()
            && argv.len() <= 160
            && argv.iter().map(String::len).sum::<usize>() <= 65536
            && argv.iter().all(|arg| !arg.contains('\0')),
        "invalid worker command vector"
    );
    Ok(argv
        .iter()
        .map(|arg| format!("'{}'", arg.replace('\'', "'\\''")))
        .collect::<Vec<_>>()
        .join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[cfg(target_os = "linux")]
    fn isolated_gate_passes_only_the_frozen_baseline_environment() {
        use std::{
            io::Write,
            process::{Command, Stdio},
        };
        let home = tempfile::tempdir().unwrap();
        let argv = isolated_gated_command(
            Path::new("/usr/bin/env"),
            &[],
            5,
            "release-env",
            home.path(),
        )
        .unwrap();
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .env("UNAPPROVED_VARIABLE", "must-not-reach-agent")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"release-env\n")
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        let actual: std::collections::BTreeMap<_, _> = std::str::from_utf8(&output.stdout)
            .unwrap()
            .lines()
            .map(|line| line.split_once('=').unwrap())
            .collect();
        assert_eq!(
            actual,
            [
                ("HOME", home.path().to_str().unwrap()),
                ("PATH", "/usr/bin:/bin"),
                ("LANG", "C.UTF-8"),
                ("LC_ALL", "C.UTF-8"),
                ("TERM", "xterm-256color")
            ]
            .into_iter()
            .collect()
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn gate_requires_exact_release_and_preserves_literal_arguments() {
        use std::{
            io::Write,
            process::{Command, Stdio},
            time::Duration,
        };
        let root = tempfile::tempdir().unwrap();
        let result = root.path().join("result");
        let literal = "space ' $(touch injected); `touch injected` \\ λ";
        for release in [Some("wrong\n"), None, Some("release-test\n")] {
            let argv = gated_command(
                Path::new("/bin/sh"),
                &[
                    "-c".into(),
                    "printf '%s' \"$1\" > \"$2\"".into(),
                    "fixture".into(),
                    literal.into(),
                    result.display().to_string(),
                ],
                2,
                "release-test",
            )
            .unwrap();
            let mut child = Command::new(&argv[0])
                .args(&argv[1..])
                .current_dir(root.path())
                .stdin(Stdio::piped())
                .spawn()
                .unwrap();
            std::thread::sleep(Duration::from_millis(60));
            assert!(child.try_wait().unwrap().is_none(), "gate failed to wait");
            assert!(!result.exists(), "agent executed before release");
            let mut input = child.stdin.take().unwrap();
            if let Some(line) = release {
                input.write_all(line.as_bytes()).unwrap();
            }
            drop(input);
            assert_eq!(
                child.wait().unwrap().success(),
                release == Some("release-test\n")
            );
            assert_eq!(result.exists(), release == Some("release-test\n"));
        }
        assert_eq!(std::fs::read_to_string(result).unwrap(), literal);
        assert!(!root.path().join("injected").exists());
        assert!(gated_command(Path::new("/usr/bin/true"), &[], 1, "bad\ntoken").is_err());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn gate_wait_is_included_in_wall_deadline() {
        use std::{
            process::{Command, Stdio},
            time::{Duration, Instant},
        };
        let root = tempfile::tempdir().unwrap();
        let result = root.path().join("should-not-exist");
        let argv = gated_command(
            Path::new("/usr/bin/touch"),
            &[result.display().to_string()],
            1,
            "release-test",
        )
        .unwrap();
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let start = Instant::now();
        assert!(!child.wait().unwrap().success());
        assert!(start.elapsed() < Duration::from_secs(8));
        assert!(!result.exists());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn deadline_stops_detached_descendants_without_a_controller_stop() {
        use crate::runner::{Cmd, RealRunner, Runner};
        use std::time::{Duration, Instant};
        let root = tempfile::tempdir().unwrap();
        let heartbeat = root.path().join("heartbeat");
        let script = "/usr/bin/setsid /bin/sh -c 'while :; do printf x >> \"$1\"; /usr/bin/sleep 0.05; done' child \"$1\" & wait";
        let argv = command(
            Path::new("/bin/sh"),
            &[
                "-c".into(),
                script.into(),
                "worker".into(),
                heartbeat.to_str().unwrap().into(),
            ],
            1,
        )
        .unwrap();
        let started = Instant::now();
        let mut cmd = Cmd::new(&argv[0], Duration::from_secs(10)).args(argv[1..].iter().cloned());
        cmd.env_clear = true;
        let output = RealRunner.run(&cmd).unwrap();
        assert!(
            !output.success(),
            "wall deadline should end the infinite fixture"
        );
        assert!(started.elapsed() < Duration::from_secs(8));
        let bytes = std::fs::metadata(&heartbeat)
            .expect("namespace fixture did not start")
            .len();
        assert!(bytes > 0);
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(
            std::fs::metadata(heartbeat).unwrap().len(),
            bytes,
            "detached descendant survived the deadline"
        );
    }
    #[test]
    fn literal_arguments_are_not_shell_programs() {
        let root = tempfile::tempdir().unwrap();
        let payload = "space ' $(touch injected); `touch injected` \\ λ";
        let line = posix_command(&["/usr/bin/printf".into(), "%s".into(), payload.into()]).unwrap();
        let output = std::process::Command::new("/bin/sh")
            .args(["-c", &line])
            .current_dir(root.path())
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap(), payload);
        assert!(!root.path().join("injected").exists());
        assert!(posix_command(&[]).is_err());
        assert!(posix_command(&["bad\0argument".into()]).is_err());
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn worker_limits_and_literal_vector_are_explicit() {
        let argv = command(
            Path::new("/fixture/agent"),
            &["--literal".into(), "space value".into()],
            123,
        )
        .unwrap();
        assert_eq!(
            &argv[argv.len() - 4..],
            &["123s", "/fixture/agent", "--literal", "space value"]
        );
        assert!(argv.iter().any(|a| a == "--kill-child=KILL"));
        assert!(command(Path::new("relative"), &[], 1).is_err());
        for wall in [0, 604801, u64::MAX] {
            assert!(command(Path::new("/fixture/agent"), &[], wall).is_err());
        }
        assert!(
            command(
                Path::new("/fixture/agent"),
                &["newline\nargument".into()],
                1
            )
            .is_err()
        );
    }
}

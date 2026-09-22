//! Live Linux handles, never a deserializable assertion of worker termination.
use super::{ProcessIncarnation, SupervisorIdentity};
use anyhow::{Context, Result, ensure};
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

/// Retains pidfds and a namespace descriptor so PID or inode reuse cannot turn
/// a later unrelated process into this observed supervisor. This establishes
/// OS process structure only, not launch authority, agent readiness, artifact
/// preservation, or permission to release a reservation.
#[derive(Debug)]
pub struct SupervisorObservation {
    outer: File,
    init: File,
    namespace: File,
    identity: SupervisorIdentity,
}

/// Live proof of the unreleased gate child, never serialized launch authority.
/// Keep these handles until the native input request has completed or failed.
pub struct GateObservation {
    supervisor: SupervisorObservation,
    child: File,
    pid: u32,
    command: Vec<String>,
}
impl GateObservation {
    pub fn check(&self) -> Result<()> {
        ensure!(
            !self.supervisor.exited()? && !exited(&self.child)?,
            "gate process exited"
        );
        verify_child_membership(self.pid, self.supervisor.identity())?;
        verify_command(self.pid, &self.command)?;
        ensure!(
            !self.supervisor.exited()? && !exited(&self.child)?,
            "gate changed during observation"
        );
        Ok(())
    }
}

/// Exact direct executable under the retained namespace init. This deliberately
/// refuses script/launcher indirection that cannot match the frozen executable.
pub struct AgentProcessObservation {
    supervisor: SupervisorObservation,
    child: File,
    pid: u32,
    command: Vec<String>,
}
impl AgentProcessObservation {
    pub fn check(&self) -> Result<()> {
        ensure!(
            !self.supervisor.exited()? && !exited(&self.child)?,
            "observed agent exited"
        );
        verify_child_membership(self.pid, self.supervisor.identity())?;
        verify_command(self.pid, &self.command)?;
        ensure!(
            !self.supervisor.exited()? && !exited(&self.child)?,
            "agent changed during observation"
        );
        Ok(())
    }
}

/// A live process carrying the exact creation marker. Environment contents are
/// used only for matching, never returned, serialized, or included in errors.
pub struct ProcessMarkerObservation {
    process: File,
    pid: u32,
    marker: String,
    cwd: std::path::PathBuf,
}
impl ProcessMarkerObservation {
    pub fn observe(pid: u32, token: &str, cwd: &Path) -> Result<Option<Self>> {
        ensure!(
            token.len() == 64
                && token
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid workspace marker"
        );
        let process = pidfd(pid)?;
        ensure!(!exited(&process)?, "workspace process exited");
        let observation = Self {
            process,
            pid,
            marker: format!("HP_WORKSPACE_CREATION={token}"),
            cwd: cwd.to_owned(),
        };
        if !observation.matches()? {
            return Ok(None);
        }
        observation.check()?;
        Ok(Some(observation))
    }
    fn matches(&self) -> Result<bool> {
        ensure!(!exited(&self.process)?, "workspace process exited");
        let directory = format!("/proc/{}", self.pid);
        ensure!(
            std::fs::metadata(&directory)?.uid() == unsafe { libc::geteuid() },
            "workspace process owner differs"
        );
        let bytes = read_bounded(&format!("{directory}/environ"), 1024 * 1024)?;
        let matched = bytes
            .split(|b| *b == 0)
            .filter(|entry| *entry == self.marker.as_bytes())
            .count()
            == 1;
        let cwd = std::fs::read_link(format!("{directory}/cwd"))?;
        ensure!(
            !exited(&self.process)?,
            "workspace process changed during observation"
        );
        Ok(matched && cwd == self.cwd)
    }
    pub fn check(&self) -> Result<()> {
        ensure!(
            self.matches()?,
            "workspace process marker or directory changed"
        );
        Ok(())
    }
}

impl SupervisorObservation {
    #[cfg(feature="state-store")]
    pub(crate) fn observe_reboot(identity:&SupervisorIdentity)->Result<Option<super::HostRebootEvidence>> {
        let current_boot=boot_id()?;
        let current_host=identity.host_id.as_ref().map(|_|host_id()).transpose()?;
        if !classify_boot(identity,&current_boot,current_host.as_deref())? {return Ok(None);}
        let evidence=super::HostRebootEvidence {
            version:1,host_id:current_host.context("reboot host evidence missing")?,
            previous_boot_id:identity.boot_id.clone(),current_boot_id:current_boot,
        };
        evidence.validate_for(identity)?;
        Ok(Some(evidence))
    }
    /// Observe a command produced by `worker_supervision::command` after the
    /// adapter has obtained its foreground PID from the exact owned terminal.
    /// This function sends no signals and makes no runtime changes.
    pub fn observe(outer_pid: u32, expected_command: &[String]) -> Result<Self> {
        ensure!(
            expected_command.len() >= 15,
            "incomplete supervisor command"
        );
        let wall = expected_command[13]
            .strip_suffix('s')
            .context("invalid supervisor wall deadline")?
            .parse()?;
        ensure!(
            super::command(
                Path::new(&expected_command[14]),
                &expected_command[15..],
                wall
            )? == expected_command,
            "command is not the canonical supervisor vector"
        );
        let outer = pidfd(outer_pid)?;
        ensure!(!exited(&outer)?, "supervisor already exited");
        verify_command(outer_pid, expected_command)?;
        let children = read_bounded(
            &format!("/proc/{outer_pid}/task/{outer_pid}/children"),
            4096,
        )?;
        let children = std::str::from_utf8(&children)?
            .split_whitespace()
            .map(str::parse::<u32>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ensure!(
            children.len() == 1,
            "supervisor must have exactly one namespace init child"
        );
        let init_pid = children[0];
        let init = pidfd(init_pid)?;
        ensure!(!exited(&init)?, "namespace init already exited");
        verify_command(init_pid, &expected_command[8..])?;
        let status = read_bounded(&format!("/proc/{init_pid}/status"), 65536)?;
        let status = std::str::from_utf8(&status)?;
        let parent: u32 = status
            .lines()
            .find_map(|s| s.strip_prefix("PPid:"))
            .context("missing supervisor parent")?
            .trim()
            .parse()?;
        ensure!(parent == outer_pid, "namespace init parent changed");
        let namespace_pids = status
            .lines()
            .find_map(|s| s.strip_prefix("NSpid:"))
            .context("kernel does not expose namespace PID identity")?
            .split_whitespace()
            .map(str::parse::<u32>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ensure!(
            namespace_pids.len() >= 2
                && namespace_pids.first() == Some(&init_pid)
                && namespace_pids.last() == Some(&1),
            "supervisor child is not a nested namespace init"
        );
        let namespace = File::open(format!("/proc/{init_pid}/ns/pid"))?;
        let identity = namespace.metadata()?;
        let outer_namespace = std::fs::metadata(format!("/proc/{outer_pid}/ns/pid"))?;
        let own_namespace = std::fs::metadata("/proc/self/ns/pid")?;
        let persistent = SupervisorIdentity {
            version: 2,
            boot_id: boot_id()?,
            host_id: Some(host_id()?),
            observer_namespace: (own_namespace.dev(), own_namespace.ino()),
            worker_namespace: (identity.dev(), identity.ino()),
            outer: incarnation(outer_pid, &outer)?,
            init: incarnation(init_pid, &init)?,
        };
        persistent.validate()?;
        ensure!(
            (identity.dev(), identity.ino()) != (own_namespace.dev(), own_namespace.ino())
                && (identity.dev(), identity.ino())
                    != (outer_namespace.dev(), outer_namespace.ino()),
            "worker namespace must differ from controller and supervisor namespaces"
        );
        // If either process exited while procfs was read, those reads cannot
        // establish the identity held by our pidfd. Refuse the entire sample.
        ensure!(
            !exited(&outer)? && !exited(&init)?,
            "supervisor changed during observation"
        );
        verify_command(outer_pid, expected_command)?;
        verify_command(init_pid, &expected_command[8..])?;
        ensure!(
            !exited(&outer)? && !exited(&init)?,
            "supervisor exited during observation"
        );
        Ok(Self {
            outer,
            init,
            namespace,
            identity: persistent,
        })
    }

    /// Require the exact waiting shell, not merely a still-live outer supervisor.
    /// After shell exec, the supervisor stays alive but this proof must fail.
    pub fn waiting_gate(&self, expected: &[String]) -> Result<GateObservation> {
        ensure!(expected.len() >= 20, "incomplete gated supervisor command");
        let wall = expected[13]
            .strip_suffix('s')
            .context("invalid gate wall deadline")?
            .parse()?;
        ensure!(
            super::gated_command(
                Path::new(&expected[19]),
                &expected[20..],
                wall,
                &expected[18]
            )? == expected,
            "command is not the canonical gate vector"
        );
        let supervisor = Self::reconnect(&self.identity)?;
        verify_command(self.identity.outer.pid, expected)?;
        verify_command(self.identity.init.pid, &expected[8..])?;
        let children = read_bounded(
            &format!("/proc/{0}/task/{0}/children", self.identity.init.pid),
            4096,
        )?;
        let children = std::str::from_utf8(&children)?
            .split_whitespace()
            .map(str::parse::<u32>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ensure!(
            children.len() == 1,
            "gate must be the sole namespace-init child"
        );
        let pid = children[0];
        let observation = GateObservation {
            supervisor,
            child: pidfd(pid)?,
            pid,
            command: expected[14..].to_vec(),
        };
        observation.check()?;
        Ok(observation)
    }

    pub fn agent_process(
        &self,
        executable: &Path,
        arguments_digest: &str,
    ) -> Result<AgentProcessObservation> {
        use sha2::{Digest, Sha256};
        let supervisor = Self::reconnect(&self.identity)?;
        let children = read_bounded(
            &format!("/proc/{0}/task/{0}/children", self.identity.init.pid),
            4096,
        )?;
        let children = std::str::from_utf8(&children)?
            .split_whitespace()
            .map(str::parse::<u32>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ensure!(
            children.len() <= 256,
            "agent child inventory exceeds bounds"
        );
        let mut found = None;
        let mut seen = std::collections::BTreeSet::new();
        for pid in children {
            ensure!(seen.insert(pid), "duplicate agent child identity");
            let child = match pidfd(pid) {
                Ok(child) => child,
                Err(error)
                    if error
                        .downcast_ref::<io::Error>()
                        .is_some_and(|e| e.raw_os_error() == Some(libc::ESRCH)) =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            };
            if exited(&child)? {
                continue;
            }
            let bytes = match read_bounded(&format!("/proc/{pid}/cmdline"), 65536) {
                Ok(bytes) => bytes,
                Err(_) if exited(&child)? => continue,
                Err(error) => return Err(error),
            };
            if exited(&child)? {
                continue;
            }
            ensure!(bytes.last() == Some(&0), "incomplete agent child command");
            let words: Vec<_> = bytes[..bytes.len() - 1].split(|b| *b == 0).collect();
            if words.first().copied() != executable.to_str().map(str::as_bytes) {
                continue;
            }
            let command = words
                .into_iter()
                .map(|s| std::str::from_utf8(s).map(str::to_owned))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            if command.len() > 129
                || format!("{:x}", Sha256::digest(serde_json::to_vec(&command[1..])?))
                    != arguments_digest
            {
                continue;
            }
            verify_child_membership(pid, &self.identity)?;
            ensure!(!exited(&child)?, "agent changed during selection");
            ensure!(
                found.is_none(),
                "multiple exact agents under namespace init"
            );
            found = Some((pid, child, command));
        }
        let (pid, child, command) = found.context("exact agent absent from namespace init")?;
        let observation = AgentProcessObservation {
            supervisor,
            child,
            pid,
            command,
        };
        observation.check()?;
        Ok(observation)
    }

    pub fn identity(&self) -> &SupervisorIdentity {
        &self.identity
    }

    /// Reattach only to the exact still-living processes and namespace. Neither
    /// executable names nor coarse /proc start-time ticks establish identity.
    pub fn reconnect(identity: &SupervisorIdentity) -> Result<Self> {
        identity.validate()?;
        ensure_view(identity)?;
        let outer = pidfd(identity.outer.pid)?;
        let init = pidfd(identity.init.pid)?;
        ensure!(
            incarnation(identity.outer.pid, &outer)? == identity.outer
                && incarnation(identity.init.pid, &init)? == identity.init
                && !exited(&outer)?
                && !exited(&init)?,
            "recorded supervisor has exited or been replaced"
        );
        let namespace = File::open(format!("/proc/{}/ns/pid", identity.init.pid))?;
        let metadata = namespace.metadata()?;
        ensure!(
            (metadata.dev(), metadata.ino()) == identity.worker_namespace,
            "recorded worker namespace changed"
        );
        ensure!(
            !exited(&outer)? && !exited(&init)?,
            "recorded supervisor exited during reconnect"
        );
        Ok(Self {
            outer,
            init,
            namespace,
            identity: identity.clone(),
        })
    }

    /// Observation only. Inaccessible procfs or a different observer namespace
    /// is an error, never evidence of death. A changed kernel boot requires
    /// matching retained host identity before it is evidence of termination.
    pub fn recover_exited(identity: &SupervisorIdentity) -> Result<bool> {
        identity.validate()?;
        if rebooted(identity)? {
            return Ok(true);
        }
        ensure_view(identity)?;
        Ok(incarnation_exited(&identity.outer)? && incarnation_exited(&identity.init)?)
    }

    /// Called only after canonical stop authorization. Recover each original
    /// process independently: one may already have exited while its peer remains.
    #[cfg(feature = "state-store")]
    pub(crate) fn stop_recorded(
        identity: &SupervisorIdentity,
        deadline: std::time::Instant,
        cancellation: &crate::runner::Cancellation,
    ) -> Result<()> {
        use std::time::{Duration, Instant};
        ensure!(
            !cancellation.is_cancelled() && Instant::now() < deadline,
            "worker stop cancelled or expired"
        );
        identity.validate()?;
        if rebooted(identity)? {
            return Ok(());
        }
        ensure_view(identity)?;
        let outer = live_incarnation(&identity.outer)?;
        let init = live_incarnation(&identity.init)?;
        let handles = [init.as_ref(), outer.as_ref()];
        let all_exited = || -> Result<bool> {
            for handle in handles.iter().flatten() {
                if !exited(handle)? {
                    return Ok(false);
                }
            }
            Ok(true)
        };
        if all_exited()? {
            return Ok(());
        }
        ensure!(
            !cancellation.is_cancelled() && Instant::now() < deadline,
            "worker stop cancelled or expired"
        );
        if let Some(init) = &init {
            signal(init, libc::SIGTERM)?;
        }
        // Keep half the remaining budget for forced exit observation. A short
        // original deadline must not postpone SIGKILL until after that deadline.
        let started = Instant::now();
        let grace = Duration::from_secs(2).min(deadline.saturating_duration_since(started) / 2);
        let force_at = started + grace;
        let mut forced = false;
        while !all_exited()? {
            ensure!(
                Instant::now() < deadline,
                "worker termination remains unobserved"
            );
            if !forced && (Instant::now() >= force_at || cancellation.is_cancelled()) {
                for handle in handles.iter().flatten() {
                    signal(handle, libc::SIGKILL)?;
                }
                forced = true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }

    /// Both exact processes have exited. Linux kills a PID namespace's remaining
    /// processes when its init exits; idle/absent terminal observations are not
    /// used as evidence here. Callers must separately meet preservation and
    /// durable attempt ownership requirements before releasing capacity.
    pub fn exited(&self) -> Result<bool> {
        Ok(exited(&self.outer)? && exited(&self.init)?)
    }

    /// Descriptor remains owned by this observation for its entire lifetime.
    pub fn namespace_identity(&self) -> Result<(u64, u64)> {
        let metadata = self.namespace.metadata()?;
        Ok((metadata.dev(), metadata.ino()))
    }
}

fn boot_id() -> Result<String> {
    Ok(
        std::str::from_utf8(&read_bounded("/proc/sys/kernel/random/boot_id", 64)?)?
            .trim()
            .into(),
    )
}
fn host_id() -> Result<String> {
    use sha2::{Digest, Sha256};
    let bytes=read_bounded("/etc/machine-id",128)?;
    let machine=std::str::from_utf8(&bytes)?.trim();
    ensure!(machine.len()==32 && machine.bytes().all(|b|b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && machine.bytes().any(|b|b!=b'0'),"local machine identity is unavailable");
    Ok(format!("{:x}",Sha256::digest(format!("herdr-projects-host-v1:{machine}"))))
}
fn classify_boot(identity:&SupervisorIdentity,current_boot:&str,current_host:Option<&str>)->Result<bool> {
    identity.validate()?;
    if let Some(expected)=identity.host_id.as_deref() {
        ensure!(current_host==Some(expected),"supervisor belongs to another host or host identity changed");
    }
    let changed=current_boot!=identity.boot_id;
    ensure!(!changed || identity.host_id.is_some(),"historical supervisor lacks same-host reboot evidence; retain uncertainty");
    Ok(changed)
}
fn rebooted(identity:&SupervisorIdentity)->Result<bool> {
    let host=identity.host_id.as_ref().map(|_|host_id()).transpose()?;
    classify_boot(identity,&boot_id()?,host.as_deref())
}
fn ensure_view(identity: &SupervisorIdentity) -> Result<()> {
    let view = std::fs::metadata("/proc/self/ns/pid")?;
    ensure!(
        !rebooted(identity)? && (view.dev(), view.ino()) == identity.observer_namespace,
        "supervisor belongs to another boot or observer PID namespace"
    );
    Ok(())
}
fn incarnation(pid: u32, file: &File) -> Result<ProcessIncarnation> {
    ensure!(
        cfg!(target_pointer_width = "64"),
        "persistent supervisor observation requires 64-bit pidfs"
    );
    let mut filesystem = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: fstatfs initializes the structure on success; file owns its FD.
    if unsafe { libc::fstatfs(file.as_raw_fd(), filesystem.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let filesystem = unsafe { filesystem.assume_init() };
    ensure!(
        filesystem.f_type as u64 == 0x50494446,
        "kernel pidfds lack persistent pidfs identity"
    );
    let metadata = file.metadata()?;
    Ok(ProcessIncarnation {
        pid,
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}
fn incarnation_exited(expected: &ProcessIncarnation) -> Result<bool> {
    // ESRCH is specific evidence that the old numeric PID has no live process.
    // EPERM, unavailable syscalls and all other errors remain unresolved.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, expected.pid as libc::pid_t, 0) };
    if fd < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(true);
        }
        return Err(error.into());
    }
    let file = unsafe { File::from_raw_fd(fd as i32) };
    Ok(incarnation(expected.pid, &file)? != *expected || exited(&file)?)
}
#[cfg(feature = "state-store")]
fn live_incarnation(identity: &ProcessIncarnation) -> Result<Option<File>> {
    let handle = match pidfd(identity.pid) {
        Ok(handle) => handle,
        Err(error)
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|e| e.raw_os_error() == Some(libc::ESRCH)) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    // A different pidfs inode proves only that the original exited. Never signal
    // the process that now happens to use its numeric PID.
    if incarnation(identity.pid, &handle)? != *identity || exited(&handle)? {
        return Ok(None);
    }
    Ok(Some(handle))
}

#[cfg(feature = "state-store")]
fn signal(handle: &File, signal: libc::c_int) -> Result<()> {
    // SAFETY: pidfd_send_signal uses a live descriptor and no siginfo pointer.
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            handle.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if result < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error.into());
        }
    }
    Ok(())
}

fn pidfd(pid: u32) -> Result<File> {
    ensure!(pid > 1 && pid <= i32::MAX as u32, "invalid supervisor PID");
    // SAFETY: pidfd_open takes scalar arguments and returns a new owned FD.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error()).context("pidfd observation unavailable");
    }
    Ok(unsafe { File::from_raw_fd(fd as i32) })
}

fn exited(handle: &File) -> Result<bool> {
    let mut poll = libc::pollfd {
        fd: handle.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: handle owns the descriptor and remains live throughout poll.
    let ready = unsafe { libc::poll(&mut poll, 1, 0) };
    if ready < 0 {
        return Err(io::Error::last_os_error().into());
    }
    ensure!(
        poll.revents & (libc::POLLNVAL | libc::POLLERR) == 0,
        "invalid supervisor pidfd"
    );
    // POLLHUP is reported after reaping on newer kernels. Both states refer to
    // the original process, even after its numeric PID becomes reusable.
    Ok(poll.revents & (libc::POLLIN | libc::POLLHUP) != 0)
}

fn read_bounded(path: &str, limit: usize) -> Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "process observation exceeds bounds");
    Ok(bytes)
}

fn verify_child_membership(pid: u32, supervisor: &SupervisorIdentity) -> Result<()> {
    let status = read_bounded(&format!("/proc/{pid}/status"), 65536)?;
    let parent: u32 = std::str::from_utf8(&status)?
        .lines()
        .find_map(|line| line.strip_prefix("PPid:"))
        .context("agent parent missing")?
        .trim()
        .parse()?;
    let namespace = std::fs::metadata(format!("/proc/{pid}/ns/pid"))?;
    ensure!(
        parent == supervisor.init.pid
            && (namespace.dev(), namespace.ino()) == supervisor.worker_namespace,
        "agent child moved outside the recorded supervisor"
    );
    Ok(())
}

fn verify_command(pid: u32, expected: &[String]) -> Result<()> {
    let bytes = read_bounded(&format!("/proc/{pid}/cmdline"), 65536)?;
    ensure!(bytes.last() == Some(&0), "incomplete supervisor argv");
    let words = bytes[..bytes.len() - 1]
        .split(|b| *b == 0)
        .collect::<Vec<_>>();
    ensure!(
        words.len() == expected.len()
            && words.iter().zip(expected).all(|(a, b)| *a == b.as_bytes()),
        "supervisor argv changed"
    );
    let installed = std::fs::metadata(&expected[0])?;
    let running = std::fs::metadata(format!("/proc/{pid}/exe"))?;
    ensure!(
        installed.is_file() && (installed.dev(), installed.ino()) == (running.dev(), running.ino()),
        "supervisor executable differs from installed helper"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    #[test]
    fn reboot_classification_requires_same_host_and_preserves_legacy_uncertainty() {
        let old_boot="00000000-0000-0000-0000-000000000001";
        let new_boot="00000000-0000-0000-0000-000000000002";
        let mut identity=SupervisorIdentity {
            version:1,boot_id:old_boot.into(),host_id:None,
            observer_namespace:(1,2),worker_namespace:(1,3),
            outer:ProcessIncarnation{pid:20,device:1,inode:4},
            init:ProcessIncarnation{pid:21,device:1,inode:5},
        };
        let legacy=serde_json::to_value(&identity).unwrap();
        assert!(legacy.get("host_id").is_none());
        assert_eq!(serde_json::from_value::<SupervisorIdentity>(legacy).unwrap(),identity);
        assert!(!classify_boot(&identity,old_boot,None).unwrap());
        assert!(classify_boot(&identity,new_boot,None).is_err());
        let host="a".repeat(64);
        identity.version=2;identity.host_id=Some(host.clone());
        assert!(!classify_boot(&identity,old_boot,Some(&host)).unwrap());
        assert!(classify_boot(&identity,new_boot,Some(&host)).unwrap());
        let evidence=super::super::HostRebootEvidence{version:1,host_id:host.clone(),previous_boot_id:old_boot.into(),current_boot_id:new_boot.into()};
        evidence.validate_for(&identity).unwrap();
        for field in ["host","previous","current","version"] {
            let mut bad=evidence.clone();
            match field {"host"=>bad.host_id="b".repeat(64),"previous"=>bad.previous_boot_id=new_boot.into(),"current"=>bad.current_boot_id=old_boot.into(),_=>bad.version=2};
            assert!(bad.validate_for(&identity).is_err());
        }
        let mut malformed=evidence.clone();malformed.current_boot_id="not-a-boot-id".into();
        assert!(malformed.validate_for(&identity).is_err());
        for boot in [old_boot,new_boot] {
            assert!(classify_boot(&identity,boot,Some(&"b".repeat(64))).is_err());
            assert!(classify_boot(&identity,boot,None).is_err());
        }
        identity.version=1;assert!(identity.validate().is_err());
        identity.version=2;identity.host_id=None;assert!(identity.validate().is_err());
    }

    #[test]
    fn ordinary_process_cannot_be_observed_as_a_supervisor() {
        let argv = super::super::command(Path::new("/usr/bin/sleep"), &["10".into()], 1).unwrap();
        assert!(SupervisorObservation::observe(std::process::id(), &argv).is_err());
        assert!(SupervisorObservation::observe(0, &argv).is_err());
        assert!(SupervisorObservation::observe(std::process::id(), &[]).is_err());
    }

    #[test]
    fn observation_pins_exact_lifetime_across_exit_and_reaping() {
        let argv = super::super::command(Path::new("/usr/bin/sleep"), &["10".into()], 2).unwrap();
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        let observation = loop {
            match SupervisorObservation::observe(child.id(), &argv) {
                Ok(observation) => break observation,
                Err(error) => {
                    if Instant::now() >= deadline || child.try_wait().unwrap().is_some() {
                        let _ = child.kill();
                        let _ = child.wait();
                        panic!("supervisor fixture unavailable: {error:#}");
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        };
        assert!(!observation.exited().unwrap());
        let persistent = observation.identity().clone();
        assert_eq!(persistent.version,2);
        assert_eq!(persistent.host_id.as_deref(),Some(host_id().unwrap().as_str()));
        assert!(!SupervisorObservation::recover_exited(&persistent).unwrap());
        let mut foreign=persistent.clone();foreign.host_id=Some("0".repeat(64));
        assert!(SupervisorObservation::recover_exited(&foreign).is_err());
        assert!(SupervisorObservation::reconnect(&foreign).is_err());
        #[cfg(feature="state-store")]
        assert!(SupervisorObservation::stop_recorded(&foreign,deadline,&Default::default()).is_err());
        assert!(!observation.exited().unwrap());
        let reconnected = SupervisorObservation::reconnect(&persistent).unwrap();
        assert_eq!(reconnected.identity(), &persistent);
        let mut replaced = persistent.clone();
        replaced.init.inode += 1;
        assert!(SupervisorObservation::reconnect(&replaced).is_err());
        let mut wrong_view = persistent.clone();
        wrong_view.observer_namespace.1 += 1;
        assert!(SupervisorObservation::recover_exited(&wrong_view).is_err());
        let namespace = observation.namespace_identity().unwrap();
        let mut wrong = argv.clone();
        wrong[13] = "3s".into();
        assert!(SupervisorObservation::observe(child.id(), &wrong).is_err());
        // The owned child has a two-second deadline even if an assertion fails.
        child.wait().unwrap();
        assert!(observation.exited().unwrap());
        assert!(reconnected.exited().unwrap());
        drop(reconnected);
        assert!(SupervisorObservation::recover_exited(&persistent).unwrap());
        assert!(SupervisorObservation::reconnect(&persistent).is_err());
        assert_eq!(observation.namespace_identity().unwrap(), namespace);
        assert!(
            observation.exited().unwrap(),
            "reaping must not change identity"
        );
    }
    #[test]
    #[cfg(feature = "state-store")]
    fn recovered_stop_handles_partial_exit_and_reserves_time_for_forced_cleanup() {
        use std::{
            process::{Command, Stdio},
            time::{Duration, Instant},
        };
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        for init_exited in [false, true] {
            let argv =
                super::super::command(Path::new("/usr/bin/sleep"), &["30".into()], 10).unwrap();
            let mut child = Child(
                Command::new(&argv[0])
                    .args(&argv[1..])
                    .env_clear()
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap(),
            );
            let end = Instant::now() + Duration::from_secs(2);
            let observation = loop {
                match SupervisorObservation::observe(child.0.id(), &argv) {
                    Ok(value) => break value,
                    Err(error) => {
                        assert!(
                            Instant::now() < end && child.0.try_wait().unwrap().is_none(),
                            "{error:#}"
                        );
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
            };
            let identity = observation.identity().clone();
            let cancelled = crate::runner::Cancellation::default();
            cancelled.cancel();
            assert!(
                SupervisorObservation::stop_recorded(
                    &identity,
                    Instant::now() + Duration::from_secs(1),
                    &cancelled
                )
                .is_err()
            );
            assert!(
                SupervisorObservation::stop_recorded(
                    &identity,
                    Instant::now(),
                    &Default::default()
                )
                .is_err()
            );
            assert!(!observation.exited().unwrap());
            signal(&observation.outer, libc::SIGSTOP).unwrap();
            signal(
                &observation.init,
                if init_exited {
                    libc::SIGKILL
                } else {
                    libc::SIGSTOP
                },
            )
            .unwrap();
            if init_exited {
                let end = Instant::now() + Duration::from_secs(2);
                while !exited(&observation.init).unwrap() {
                    assert!(Instant::now() < end);
                    std::thread::sleep(Duration::from_millis(10));
                }
                assert!(SupervisorObservation::reconnect(&identity).is_err());
            }
            drop(observation);
            let started = Instant::now();
            SupervisorObservation::stop_recorded(
                &identity,
                started + Duration::from_secs(1),
                &Default::default(),
            )
            .unwrap();
            assert!(started.elapsed() < Duration::from_secs(1));
            assert!(SupervisorObservation::recover_exited(&identity).unwrap());
            child.0.wait().unwrap();
            // Repeated stop after reaping is a read-only completed observation.
            SupervisorObservation::stop_recorded(
                &identity,
                Instant::now() + Duration::from_secs(1),
                &Default::default(),
            )
            .unwrap();
        }
    }
    #[test]
    #[cfg(feature = "state-store")]
    fn agent_selection_allows_orphans_but_refuses_duplicate_exact_agents() {
        use sha2::{Digest,Sha256};
        for duplicate in [false,true] {
            let root=tempfile::tempdir().unwrap();
            let ready=root.path().join("ready");
            let script=r#"import os,sys,time
middle=os.fork()
if middle==0:
    if os.fork()!=0: os._exit(0)
    if sys.argv[1]=='helper': os.execl('/usr/bin/sleep','/usr/bin/sleep','30')
    while True: time.sleep(1)
os.waitpid(middle,0)
open(sys.argv[2],'w').write('ready')
while True: time.sleep(1)
"#;
            let script_path=root.path().join("agent.py");
            std::fs::write(&script_path,script).unwrap();
            let arguments=vec![script_path.display().to_string(),if duplicate {"duplicate".into()} else {"helper".into()},ready.display().to_string()];
            // The fixture is passed directly to the executable, not through a shell.
            let executable=Path::new("/usr/bin/python3");
            let argv=super::super::command(executable,&arguments,10).unwrap();
            let mut child=Command::new(&argv[0]).args(&argv[1..]).env_clear()
                .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
            let deadline=Instant::now()+Duration::from_secs(3);
            let observation=loop {
                if ready.exists() {
                    if let Ok(observed)=SupervisorObservation::observe(child.id(),&argv) {break observed;}
                }
                assert!(Instant::now()<deadline && child.try_wait().unwrap().is_none());
                std::thread::sleep(Duration::from_millis(10));
            };
            let digest=format!("{:x}",Sha256::digest(serde_json::to_vec(&arguments).unwrap()));
            if duplicate {
                let error=observation.agent_process(executable,&digest).err().expect("duplicate agent must be refused");
                assert!(error.to_string().contains("multiple exact agents"),"{error:#}");
            } else {
                let agent=loop {
                    match observation.agent_process(executable,&digest) {
                        Ok(agent)=>break agent,
                        Err(error)=> {assert!(Instant::now()<deadline,"{error:#}");std::thread::sleep(Duration::from_millis(10));}
                    }
                };
                agent.check().unwrap();
                assert!(verify_child_membership(std::process::id(),observation.identity()).is_err());
            }
            SupervisorObservation::stop_recorded(observation.identity(),Instant::now()+Duration::from_secs(3),&Default::default()).unwrap();
            child.wait().unwrap();
            assert!(observation.exited().unwrap());
        }
    }

}

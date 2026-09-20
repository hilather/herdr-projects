//! Every external command (herdr, git, gh, ssh, scp, rsync, sh) goes through `Runner`.

use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// Shared cancellation identity; cloning a command keeps the same token.
#[derive(Debug, Clone, Default)]
pub struct Cancellation(Arc<AtomicBool>);

impl PartialEq for Cancellation {
    fn eq(&self, other: &Self) -> bool { Arc::ptr_eq(&self.0, &other.0) }
}

impl Cancellation {
    #[allow(dead_code)] // Used by future executor queues as well as real-process tests.
    pub fn cancel(&self) { self.0.store(true, Ordering::Release); }
    pub fn is_cancelled(&self) -> bool { self.0.load(Ordering::Acquire) }
}

pub const CAPTURE_LIMIT: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Trusted helpers can start with an explicit environment, including before
    /// the dynamic loader runs. Defaults to inheriting the caller environment.
    pub env_clear: bool,
    pub env_remove: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub stdin: Option<String>,
    pub timeout: Duration,
    /// Spawn in its own process group and kill the whole group on timeout.
    pub own_group: bool,
    pub capture_limit: usize,
    /// Exclusive regular-file sink for bounded binary transport; no stdout
    /// text/byte buffer is retained when present. Caller owns partial cleanup.
    pub stdout_file: Option<(PathBuf, usize)>,
    pub cancellation: Option<Cancellation>,
}

impl Cmd {
    pub fn new(program: impl Into<String>, timeout: Duration) -> Self {
        Cmd {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            env_clear: false,
            env_remove: Vec::new(),
            cwd: None,
            stdin: None,
            timeout,
            // Runner commands are finite. Detached services use a separate
            // spawn path with redirected descriptors (see ticker::start).
            own_group: true,
            capture_limit: CAPTURE_LIMIT,
            stdout_file: None,
            cancellation: None,
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn env_remove(mut self, key: impl Into<String>) -> Self {
        self.env_remove.push(key.into());
        self
    }

    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn stdin(mut self, text: impl Into<String>) -> Self {
        self.stdin = Some(text.into());
        self
    }

    pub fn own_group(mut self) -> Self {
        self.own_group = true;
        self
    }

    /// The command as one line; the scripted fake matches on it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn display(&self) -> String {
        let mut line = self.program.clone();
        for arg in &self.args {
            line.push(' ');
            line.push_str(arg);
        }
        line
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Output {
    /// `None` when the process was killed (timeout or signal).
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub cancelled: bool,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    /// Exact captured bytes; the strings above are compatibility text views.
    pub stdout_bytes: Vec<u8>,
    pub stderr_bytes: Vec<u8>,
    /// Bytes actually drained, including excess discarded at the capture cap.
    pub stdout_total_bytes: u64,
    pub stderr_total_bytes: u64,
    pub elapsed: Duration,
}

impl Output {
    pub fn success(&self) -> bool {
        self.code == Some(0) && !self.timed_out && !self.cancelled
            && !self.stdout_truncated && !self.stderr_truncated
    }

    /// stderr when it has text, else stdout, trimmed; for error messages.
    pub fn error_text(&self) -> String {
        if self.cancelled {
            return "cancelled".to_string();
        }
        if self.timed_out {
            return "timed out".to_string();
        }
        if self.stdout_truncated || self.stderr_truncated {
            return "command output exceeded capture limit".to_string();
        }
        let text = if self.stderr.trim().is_empty() {
            self.stdout.trim()
        } else {
            self.stderr.trim()
        };
        text.to_string()
    }
}

pub trait Runner {
    /// `Err` means spawning or pipe I/O failed. A non-zero exit, truncation,
    /// cancellation or timeout is an `Ok(Output)` with success() == false.
    fn run(&self, cmd: &Cmd) -> Result<Output>;

    /// One JSON line to a herdr socket, one line back. The single exception to
    /// "talk to herdr through its CLI" (client decision during the build):
    /// herdr 0.9.1 has no CLI command for `agent.view.set` / `agent.view.clear`.
    fn socket_request(&self, socket: &Path, line: &str, timeout: Duration) -> Result<String>;
}

pub struct RealRunner;

const POLL: Duration = Duration::from_millis(20);

impl Runner for RealRunner {
    fn run(&self, cmd: &Cmd) -> Result<Output> {
        let started = Instant::now();
        if cmd.cancellation.as_ref().is_some_and(Cancellation::is_cancelled) {
            return Ok(Output { cancelled: true, ..Output::default() });
        }
        let mut command = Command::new(&cmd.program);
        command.args(&cmd.args);
        if cmd.env_clear { command.env_clear(); }
        for key in &cmd.env_remove {
            command.env_remove(key);
        }
        for (key, value) in &cmd.env {
            command.env(key, value);
        }
        if let Some(cwd) = &cmd.cwd {
            command.current_dir(cwd);
        }
        command
            .stdin(if cmd.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        if cmd.own_group {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }

        let mut child = command
            .spawn()
            .with_context(|| format!("could not run `{}`", cmd.program))?;

        let result = collect(&mut child, cmd, started);
        if result.is_err() {
            terminate(&mut child, cmd.own_group);
        }
        result
    }

    fn socket_request(&self, socket: &Path, line: &str, timeout: Duration) -> Result<String> {
        socket_round_trip(socket, line, timeout)
    }
}

fn socket_round_trip(socket: &Path, line: &str, timeout: Duration) -> Result<String> {
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixStream;
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("could not connect to {}", socket.display()))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    Ok(reply)
}

fn nonblocking(pipe: &impl AsRawFd) -> io::Result<()> {
    let fd = pipe.as_raw_fd();
    // SAFETY: the live pipe owns fd; fcntl does not take ownership or access
    // Rust memory. Preserve existing flags when enabling nonblocking I/O.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[derive(Default)]
struct Capture { bytes: Vec<u8>, truncated: bool, total: u64, stored: usize, sink: Option<std::fs::File> }

impl Capture {
    fn drain(&mut self, pipe: &mut Option<impl Read>, limit: usize) -> io::Result<()> {
        let mut buf = [0_u8; 8192];
        // Per-turn work is bounded even when a producer never stops writing.
        // Discard excess bytes while draining, so a full pipe cannot deadlock.
        for _ in 0..8 {
            let Some(reader) = pipe.as_mut() else { break; };
            match reader.read(&mut buf) {
                Ok(0) => { *pipe = None; break; }
                Ok(n) => {
                    self.total = self.total.saturating_add(n as u64);
                    let keep = n.min(limit.saturating_sub(self.stored));
                    if let Some(file) = &mut self.sink { file.write_all(&buf[..keep])?; }
                    else { self.bytes.extend_from_slice(&buf[..keep]); }
                    self.stored += keep;
                    self.truncated |= keep < n;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

fn collect(child: &mut std::process::Child, cmd: &Cmd, started: Instant) -> Result<Output> {
    let mut input = child.stdin.take();
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    if let Some(pipe) = &input { nonblocking(pipe)?; }
    if let Some(pipe) = &stdout { nonblocking(pipe)?; }
    if let Some(pipe) = &stderr { nonblocking(pipe)?; }
    let bytes = cmd.stdin.as_deref().unwrap_or("").as_bytes();
    let mut written = 0;
    let (mut out, mut err) = (Capture::default(), Capture::default());
    let out_limit = if let Some((path, limit)) = &cmd.stdout_file {
        use std::os::unix::fs::OpenOptionsExt;
        out.sink = Some(std::fs::OpenOptions::new().create_new(true).write(true).mode(0o600).open(path)?);
        *limit
    } else { cmd.capture_limit };
    let mut status = None;
    let mut result = Output::default();
    loop {
        result.cancelled = cmd.cancellation.as_ref().is_some_and(Cancellation::is_cancelled);
        result.timed_out = !result.cancelled && started.elapsed() >= cmd.timeout;
        if result.cancelled || result.timed_out {
            drop(input.take());
            terminate(child, cmd.own_group);
            // One final bounded drain; detached/unowned descendants may still
            // have descriptors open. Dropping the pipes never waits for them.
            let _ = out.drain(&mut stdout, out_limit);
            let _ = err.drain(&mut stderr, cmd.capture_limit);
            break;
        }
        if written == bytes.len() { input = None; }
        if let Some(pipe) = &mut input {
            let end = written.saturating_add(65_536).min(bytes.len());
            match pipe.write(&bytes[written..end]) {
                Ok(0) => input = None,
                Ok(n) => written += n,
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => input = None,
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {},
                Err(e) => return Err(e.into()),
            }
        }
        out.drain(&mut stdout, out_limit)?;
        err.drain(&mut stderr, cmd.capture_limit)?;
        if out.sink.is_some() && out.truncated {
            drop(input.take());
            terminate(child, cmd.own_group);
            break;
        }
        if status.is_none() { status = child.try_wait()?; }
        if status.is_some() && input.is_none() && stdout.is_none() && stderr.is_none() {
            result.code = status.and_then(|s| s.code());
            break;
        }

        let mut fds = Vec::with_capacity(3);
        for fd in [stdout.as_ref().map(AsRawFd::as_raw_fd), stderr.as_ref().map(AsRawFd::as_raw_fd)].into_iter().flatten() {
            fds.push(libc::pollfd { fd, events: libc::POLLIN, revents: 0 });
        }
        if let Some(pipe) = &input {
            fds.push(libc::pollfd { fd: pipe.as_raw_fd(), events: libc::POLLOUT, revents: 0 });
        }
        let wait = cmd.timeout.saturating_sub(started.elapsed()).min(POLL);
        // SAFETY: fds contains initialized entries and remains live throughout
        // poll; no other thread closes these descriptors.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, wait.as_millis() as i32) };
        if ready < 0 {
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted { return Err(e.into()); }
        }
    }
    if let Some(file) = &out.sink { file.sync_all()?; }
    result.stdout = String::from_utf8_lossy(&out.bytes).into_owned();
    result.stderr = String::from_utf8_lossy(&err.bytes).into_owned();
    result.stdout_bytes = out.bytes;
    result.stderr_bytes = err.bytes;
    result.stdout_truncated = out.truncated;
    result.stderr_truncated = err.truncated;
    result.stdout_total_bytes = out.total;
    result.stderr_total_bytes = err.total;
    result.elapsed = started.elapsed();
    Ok(result)
}

fn terminate(child: &mut std::process::Child, own_group: bool) {
    if own_group {
        let group = -(child.id() as libc::pid_t);
        // SAFETY: process_group(0) made this child's PID the owned group ID.
        // Direct signals avoid spawning a potentially blocking kill helper.
        unsafe { libc::kill(group, libc::SIGTERM); }
        std::thread::sleep(Duration::from_millis(200));
        unsafe { libc::kill(group, libc::SIGKILL); }
    }
    // Child::kill refuses to signal a child already reaped by try_wait.
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
#[path = "runner/fake.rs"]
pub mod fake;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_output_and_exit_code() {
        let out = RealRunner
            .run(&Cmd::new("sh", Duration::from_secs(5)).args(["-c", "echo hi; echo err >&2; exit 3"]))
            .unwrap();
        assert_eq!(out.code, Some(3));
        assert_eq!(out.stdout, "hi\n");
        assert_eq!(out.stderr, "err\n");
        assert!(!out.success());
    }

    #[test]
    fn passes_stdin() {
        let out = RealRunner
            .run(&Cmd::new("cat", Duration::from_secs(5)).stdin("hello"))
            .unwrap();
        assert_eq!(out.stdout, "hello");
    }

    #[test]
    fn missing_program_is_an_error() {
        assert!(
            RealRunner
                .run(&Cmd::new("hp-no-such-program", Duration::from_secs(1)))
                .is_err()
        );
    }

    #[test]
    fn times_out_a_chatty_child() {
        // `yes` fills the pipe far past its buffer; nonblocking reads keep it
        // drained so the deadline still fires.
        let start = Instant::now();
        let out = RealRunner
            .run(&Cmd::new("yes", Duration::from_millis(300)))
            .unwrap();
        assert!(out.timed_out);
        assert!(!out.success());
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(out.stdout.len() > 65_536);
        assert!(out.stdout_bytes.len() <= CAPTURE_LIMIT);
        assert!(out.stdout_truncated);
    }

    #[test]
    fn drains_both_streams_after_capture_limits_without_deadlocking() {
        let mut cmd = Cmd::new("sh", Duration::from_secs(5))
            .args(["-c", "head -c 131072 /dev/zero; head -c 131072 /dev/zero >&2"]);
        cmd.capture_limit = 4096;
        let out = RealRunner.run(&cmd).unwrap();
        assert_eq!(out.code, Some(0));
        assert!(!out.timed_out);
        assert_eq!(out.stdout_bytes.len(), 4096);
        assert_eq!(out.stderr_bytes.len(), 4096);
        assert_eq!(out.stdout_total_bytes, 131072);
        assert_eq!(out.stderr_total_bytes, 131072);
        assert!(out.stdout_truncated && out.stderr_truncated);
        assert!(!out.success(), "partial command output must not pass a copy/JSON consumer");
        assert!(out.error_text().contains("capture limit"));
    }

    #[test]
    fn captures_raw_bytes_without_lossy_transport() {
        let out = RealRunner.run(&Cmd::new("sh", Duration::from_secs(2))
            .args(["-c", "printf '\\377\\000a'; printf '\\376' >&2"])).unwrap();
        assert!(out.success());
        assert_eq!(out.stdout_bytes, [255, 0, b'a']);
        assert_eq!(out.stderr_bytes, [254]);
        assert_eq!(out.stdout, "\u{fffd}\0a");
    }

    #[test]
    fn stdin_larger_than_pipe_capacity_is_written_and_closed() {
        let text = "x".repeat(256 * 1024);
        let out = RealRunner.run(&Cmd::new("cat", Duration::from_secs(3)).stdin(&text)).unwrap();
        assert!(out.success());
        assert_eq!(out.stdout, text);
    }

    #[test]
    fn blocked_stdin_obeys_deadline() {
        let start = Instant::now();
        let out = RealRunner.run(&Cmd::new("sh", Duration::from_millis(100))
            .args(["-c", "sleep 2"]).stdin("x".repeat(256 * 1024))).unwrap();
        assert!(out.timed_out);
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn cancellation_interrupts_blocked_io_and_is_distinct_from_timeout() {
        let mut cmd = Cmd::new("sh", Duration::from_secs(5)).args(["-c", "sleep 2"]);
        let token = Cancellation::default();
        cmd.cancellation = Some(token.clone());
        let cancel = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            token.cancel();
        });
        let start = Instant::now();
        let out = RealRunner.run(&cmd).unwrap();
        cancel.join().unwrap();
        assert!(out.cancelled && !out.timed_out && !out.success());
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(out.error_text(), "cancelled");
    }

    #[test]
    fn pre_cancelled_command_is_not_spawned() {
        let mut cmd = Cmd::new("hp-no-such-program", Duration::from_secs(1));
        let token = Cancellation::default();
        token.cancel();
        cmd.cancellation = Some(token);
        assert!(RealRunner.run(&cmd).unwrap().cancelled);
    }

    #[test]
    fn hard_kill_handles_ignored_term() {
        let start = Instant::now();
        let out = RealRunner.run(&Cmd::new("sh", Duration::from_millis(100))
            .args(["-c", "trap '' TERM; printf ready; while :; do sleep 1; done"])).unwrap();
        assert!(out.timed_out);
        assert_eq!(out.stdout, "ready");
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn unowned_descendant_cannot_hold_pipe_collection_open() {
        let mut cmd = Cmd::new("sh", Duration::from_millis(100)).args(["-c", "sleep 1 &"]);
        cmd.own_group = false;
        let start = Instant::now();
        assert!(RealRunner.run(&cmd).unwrap().timed_out);
        assert!(start.elapsed() < Duration::from_millis(800));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn escaped_process_group_cannot_hold_pipe_collection_open() {
        let start = Instant::now();
        let out = RealRunner.run(&Cmd::new("sh", Duration::from_millis(100))
            .args(["-c", "setsid sh -c 'printf escaped; sleep 1' &"])).unwrap();
        assert_eq!(out.stdout, "escaped", "setsid fixture must actually start");
        assert!(out.timed_out);
        assert!(start.elapsed() < Duration::from_millis(800));
    }

    #[test]
    fn short_lived_descendant_is_drained_normally() {
        let out = RealRunner.run(&Cmd::new("sh", Duration::from_secs(2))
            .args(["-c", "(sleep 0.05; printf finished) &"])).unwrap();
        assert!(out.success());
        assert_eq!(out.stdout, "finished");
    }

    #[test]
    fn group_kill_reaches_grandchildren() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("survived");
        let script = format!("(sleep 2; touch '{}') & wait", marker.display());
        let start = Instant::now();
        let out = RealRunner
            .run(
                &Cmd::new("sh", Duration::from_millis(300))
                    .args(["-c", &script])
                    .own_group(),
            )
            .unwrap();
        assert!(out.timed_out);
        assert!(start.elapsed() < Duration::from_secs(2));
        std::thread::sleep(Duration::from_millis(2300));
        assert!(!marker.exists(), "grandchild outlived the group kill");
    }
}

#[cfg(test)]
mod streaming_tests {
    use super::*;
    #[test]
    fn file_sink_streams_beyond_capture_limit_and_caps_producers() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let bytes = vec![255u8; 2 * 1024 * 1024];
        std::fs::write(&source, &bytes).unwrap();
        let mut cmd = Cmd::new("cat", Duration::from_secs(5)).arg(source.to_str().unwrap());
        let target = dir.path().join("target");
        cmd.stdout_file = Some((target.clone(), bytes.len()));
        let output = RealRunner.run(&cmd).unwrap();
        assert!(output.success());
        assert!(output.stdout_bytes.is_empty());
        assert_eq!(std::fs::read(&target).unwrap(), bytes);
        assert!(RealRunner.run(&cmd).is_err()); // exclusive sink; never overwrite
        let capped = dir.path().join("capped");
        let mut cmd = Cmd::new("sh", Duration::from_secs(5)).args(["-c", "while :; do printf 0123456789; done"]);
        cmd.stdout_file = Some((capped.clone(), 100));
        let output = RealRunner.run(&cmd).unwrap();
        assert!(!output.success());
        assert!(output.stdout_truncated);
        assert!(!output.timed_out);
        assert_eq!(std::fs::metadata(capped).unwrap().len(), 100);
    }
}

//! Bounded commands and isolated state for explicitly selected acceptance tests.
use std::{fs, path::{Path, PathBuf}, process::{Child, Command, Stdio}, time::{Duration, Instant}};
use serde_json::Value;

pub struct Lab {
    pub temp: tempfile::TempDir,
    pub herdr: PathBuf,
    server: Option<Child>,
    sequence: std::cell::Cell<u64>,
}
impl Lab {
    pub fn new() -> Self {
        let herdr = PathBuf::from(std::env::var_os("HP_LIVE_HERDR").expect("set HP_LIVE_HERDR to the absolute Herdr binary path"));
        assert!(herdr.is_absolute() && herdr.is_file());
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("home")).unwrap();
        fs::create_dir(temp.path().join("runtime")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(temp.path().join("runtime"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(temp.path().join("config.toml"), "onboarding = false\n[terminal]\ndefault_shell = '/bin/sh'\nshell_mode = 'non_login'\n[update]\nversion_check = false\nmanifest_check = false\n").unwrap();
        Self { temp, herdr, server: None, sequence: std::cell::Cell::new(0) }
    }
    pub fn path(&self) -> &Path { self.temp.path() }
    pub fn command(&self, bin: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut cmd = Command::new(bin);
        cmd.env_clear().current_dir(self.path()).env("HOME", self.path().join("home"))
            .env("PATH", format!("{}:/usr/local/bin:/usr/bin:/bin", self.herdr.parent().unwrap().display()))
            .env("SHELL", "/bin/sh").env("TERM", "xterm-256color").env("LANG", "C.UTF-8")
            .env("XDG_RUNTIME_DIR", self.path().join("runtime"))
            .env("HERDR_CONFIG_PATH", self.path().join("config.toml"))
            .env("HERDR_PROJECTS_ROOT", self.path().join("projects"))
            .env("HERDR_BIN_PATH", &self.herdr)
            .stdin(Stdio::null());
        cmd
    }
    pub fn run(&self, cmd: Command) -> (bool, String, String) {
        let (ok, bytes, error) = self.run_bytes(cmd);
        (ok, String::from_utf8(bytes).expect("text command returned non-UTF-8"), error)
    }
    pub fn run_bytes(&self, mut cmd: Command) -> (bool, Vec<u8>, String) {
        let n = self.sequence.get(); self.sequence.set(n + 1);
        let out = self.path().join(format!("command-{n}.out"));
        let err = self.path().join(format!("command-{n}.err"));
        let mut child = cmd.stdout(fs::File::create(&out).unwrap()).stderr(fs::File::create(&err).unwrap()).spawn().unwrap();
        let until = Instant::now() + Duration::from_secs(20);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() { break status; }
            if Instant::now() >= until { let _ = child.kill(); let _ = child.wait(); panic!("acceptance command exceeded 20 seconds: {cmd:?}"); }
            std::thread::sleep(Duration::from_millis(25));
        };
        (status.success(), fs::read(out).unwrap(), fs::read_to_string(err).unwrap())
    }
    pub fn herdr(&self, args: &[&str]) -> Value {
        let mut cmd = self.command(&self.herdr);
        cmd.args(["--session", "hp-acceptance"]).args(args);
        let (ok, out, err) = self.run(cmd);
        assert!(ok, "herdr {args:?}: {err}\n{out}");
        serde_json::from_str(&out).unwrap_or_else(|error| panic!("invalid Herdr JSON: {error}: {out}"))
    }
    pub fn diagnostics(&self) -> String {
        let mut cmd = self.command(&self.herdr);
        cmd.args(["--session", "hp-acceptance", "plugin", "log", "--plugin", "herdr-projects"]);
        let (_, output, error) = self.run(cmd);
        let mut files = Vec::new();
        let mut pending = vec![self.path().join("home/.config/herdr")];
        while let Some(dir) = pending.pop() {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if entry.file_type().is_ok_and(|kind| kind.is_dir()) { pending.push(path); }
                    else if path.extension().is_some_and(|ext| ext == "log" || ext == "json" || ext == "consumed") {
                        files.push(format!("{}: {}", path.display(), fs::read_to_string(&path).unwrap_or_default()));
                    }
                }
            }
        }
        format!("server: {}\nplugin: {output}\n{error}\nfiles: {}\nsnapshot: {}", fs::read_to_string(self.path().join("server.log")).unwrap_or_default(), files.join("\n"), self.herdr(&["api", "snapshot"]))
    }
    pub fn hp(&self, args: &[&str]) -> String {
        let mut cmd = self.command(env!("CARGO_BIN_EXE_herdr-projects"));
        cmd.args(args);
        let (ok, out, err) = self.run(cmd);
        assert!(ok, "herdr-projects {args:?}: {err}\n{out}");
        out
    }
    pub fn start(&mut self) {
        let log = fs::File::create(self.path().join("server.log")).unwrap();
        let mut cmd = self.command(&self.herdr);
        self.server = Some(cmd.args(["--session", "hp-acceptance", "server"]).stdout(log.try_clone().unwrap()).stderr(log).spawn().unwrap());
        let until = Instant::now() + Duration::from_secs(15);
        loop {
            let mut cmd = self.command(&self.herdr);
            cmd.args(["--session", "hp-acceptance", "status", "server", "--json"]);
            let (ok, out, _) = self.run(cmd);
            if ok {
                let status: Value = serde_json::from_str(&out).unwrap();
                if status["running"] == true {
                    eprintln!("isolated server status: {status}");
                    return;
                }
            }
            if self.server.as_mut().unwrap().try_wait().unwrap().is_some() || Instant::now() >= until {
                panic!("server failed: {}", fs::read_to_string(self.path().join("server.log")).unwrap());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
impl Drop for Lab {
    fn drop(&mut self) {
        if self.path().join("projects/.ticker.lock").exists() {
            let mut cmd = self.command(env!("CARGO_BIN_EXE_herdr-projects"));
            cmd.args(["ticker", "stop"]);
            let _ = self.run(cmd);
        }
        if self.server.is_some() {
            // This command is pinned to the fixture's home/config and named session.
            let mut cmd = self.command(&self.herdr);
            cmd.args(["--session", "hp-acceptance", "server", "stop"]);
            let _ = self.run(cmd);
            if let Some(mut child) = self.server.take() { let _ = child.kill(); let _ = child.wait(); }
        }
    }
}

/// A real terminal client attached only to the fixture server. The reader drains
/// rendering output so a full PTY buffer cannot deadlock the acceptance commands.
pub struct Client {
    child: Child,
    input: Option<fs::File>,
    output: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    reader: Option<std::thread::JoinHandle<()>>,
}
impl Client {
    pub fn start(lab: &Lab) -> Self {
        use std::os::fd::FromRawFd;
        use std::os::unix::process::CommandExt;
        let (mut master, mut slave) = (-1, -1);
        let size = libc::winsize { ws_row: 30, ws_col: 100, ws_xpixel: 0, ws_ypixel: 0 };
        // SAFETY: initialized output pointers and valid winsize; no terminal name requested.
        assert_eq!(unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null(), &size) }, 0);
        // SAFETY: successful openpty returns two distinct owned descriptors.
        let input = unsafe { fs::File::from_raw_fd(master) };
        let terminal = unsafe { fs::File::from_raw_fd(slave) };
        let mut cmd = lab.command(&lab.herdr);
        cmd.args(["--session", "hp-acceptance"])
            .stdin(terminal.try_clone().unwrap()).stdout(terminal.try_clone().unwrap()).stderr(terminal);
        // SAFETY: only async-signal-safe libc operations execute between fork and exec.
        unsafe { cmd.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 { return Err(std::io::Error::last_os_error()); }
            Ok(())
        }); }
        let child = cmd.spawn().unwrap();
        drop(cmd);
        let output = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = output.clone();
        let mut stream = input.try_clone().unwrap();
        let reader = std::thread::spawn(move || {
            use std::io::Read;
            let mut buffer = [0u8; 8192];
            while let Ok(n) = stream.read(&mut buffer) {
                if n == 0 { break; }
                let mut bytes = captured.lock().unwrap();
                bytes.extend_from_slice(&buffer[..n]);
                if bytes.len() > 1024 * 1024 { let discard = bytes.len() - 1024 * 1024; bytes.drain(..discard); }
            }
        });
        Self { child, input: Some(input), output, reader: Some(reader) }
    }
    pub fn type_text(&mut self, text: &[u8]) {
        use std::io::Write;
        self.input.as_mut().unwrap().write_all(text).unwrap();
    }
    pub fn wait_for(&self, expected: &str) -> bool {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            let bytes = self.output.lock().unwrap();
            let text = String::from_utf8_lossy(&bytes);
            if text.contains(expected) { return true; }
            if Instant::now() >= until { eprintln!("client never displayed {expected:?}: {text}"); return false; }
            drop(bytes);
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    pub fn clear_output(&self) { self.output.lock().unwrap().clear(); }
}
impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.input.take();
        if let Some(reader) = self.reader.take() { let _ = reader.join(); }
    }
}

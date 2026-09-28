//! Root, config directory, herdr binary and socket resolution.
//!
//! Nothing here reads the process environment directly: callers pass an `Env`,
//! so resolution order is testable and never depends on plugin variables.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::herdr;
use crate::runner::Runner;

#[derive(Debug, Clone)]
pub struct Env {
    vars: BTreeMap<String, String>,
    pub home: PathBuf,
}

impl Env {
    /// A worker needs only its frozen home and Herdr selection, not ambient
    /// credentials or unrelated process-environment values.
    #[cfg(feature="state-store")]
    pub(crate) fn for_observation(home:&Path,bin:&str)->Self {
        Self{home:home.to_path_buf(),vars:BTreeMap::from([("HERDR_BIN_PATH".into(),bin.into())])}
    }
    pub fn from_process() -> Result<Self> {
        let vars: BTreeMap<String, String> = std::env::vars().collect();
        let home = vars
            .get("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .context("HOME is not set")?;
        Ok(Env { vars, home })
    }

    #[cfg(test)]
    pub fn for_test(home: &Path, vars: &[(&str, &str)]) -> Self {
        Env {
            vars: vars
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            home: home.to_path_buf(),
        }
    }

    /// A variable's value; an empty value counts as unset.
    pub fn var(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(String::as_str).filter(|v| !v.is_empty())
    }

    /// The fixed user-level config directory, `~/.config/herdr-projects`.
    pub fn config_dir(&self) -> PathBuf {
        self.home.join(".config").join("herdr-projects")
    }

    /// `HERDR_BIN_PATH` when set, else `herdr` on `PATH`.
    pub fn herdr_bin(&self) -> String {
        self.var("HERDR_BIN_PATH").unwrap_or("herdr").to_string()
    }

    fn expand_tilde(&self, path: &str) -> PathBuf {
        match path.strip_prefix("~/") {
            Some(rest) => self.home.join(rest),
            None if path == "~" => self.home.clone(),
            None => PathBuf::from(path),
        }
    }
}

/// What every subcommand works from: the environment, the resolved root and
/// config directory, and the runner all external commands go through.
pub struct Ctx<'a> {
    pub env: &'a Env,
    pub root: PathBuf,
    pub config_dir: PathBuf,
    pub runner: &'a dyn Runner,
    /// False in tests, so commands that ensure a ticker never spawn a process.
    pub detached_ticker: bool,
}

/// The part of `config.toml` that resolution needs. Safety tables are read by
/// the `project` module from the same file.
#[derive(Debug, Default, Deserialize)]
struct RootConfig {
    root: Option<String>,
}

/// Projects root: `--root`, then `HERDR_PROJECTS_ROOT`, then `root` in
/// `<config_dir>/config.toml`, then `~/.herdr-projects`.
pub fn resolve_root(flag: Option<&Path>, env: &Env, config_dir: &Path) -> Result<PathBuf> {
    if let Some(flag) = flag {
        return absolute(flag);
    }
    if let Some(var) = env.var("HERDR_PROJECTS_ROOT") {
        return absolute(&env.expand_tilde(var));
    }
    let config_file = config_dir.join("config.toml");
    if let Some(text) = read_root_config(&config_file)? {
        let config: RootConfig = toml::from_str(&text)
            .map_err(|_| anyhow::anyhow!("{} does not parse (contents withheld)", config_file.display()))?;
        if let Some(root) = config.root.filter(|r| !r.is_empty()) {
            return absolute(&env.expand_tilde(&root));
        }
    }
    Ok(env.home.join(".herdr-projects"))
}

// Root resolution runs before migration's own reader. Bound this read as well,
// including a FIFO supplied where a config file should have been.
pub(crate) fn read_root_config(path: &Path) -> Result<Option<String>> {
    use std::{io::Read, os::unix::fs::OpenOptionsExt};
    let file = match std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("cannot read root config"),
    };
    anyhow::ensure!(file.metadata()?.is_file(), "root config is not a regular file");
    let mut bytes = Vec::new();
    file.take(16 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() <= 16 * 1024 * 1024, "root config exceeds 16 MiB");
    Ok(Some(String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("root config is not UTF-8 (contents withheld)"))?))
}

/// Private control records must never block on FIFOs or allocate from an
/// unbounded file. Missing is distinct from malformed or unsupported content.
pub(crate) fn read_control_text(path:&Path,limit:usize)->Result<Option<String>> {
    use std::{io::Read,os::unix::fs::{OpenOptionsExt,MetadataExt}};
    let mut file=match std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK|libc::O_NOFOLLOW).open(path) {
        Ok(file)=>file,Err(error) if error.kind()==std::io::ErrorKind::NotFound=>return Ok(None),
        Err(error)=>return Err(error).with_context(||format!("cannot read {}",path.display())),
    };
    let before=file.metadata()?;
    anyhow::ensure!(before.is_file()&&before.nlink()==1,"{} is not a regular single-link control file",path.display());
    anyhow::ensure!(before.len()<=limit as u64,"{} exceeds its {limit}-byte limit",path.display());
    let mut bytes=Vec::new();(&mut file).take(limit as u64+1).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len()<=limit,"{} exceeds its {limit}-byte limit",path.display());
    crate::source_tree::unchanged(&file,&before)?;
    Ok(Some(String::from_utf8(bytes).with_context(||format!("{} is not UTF-8 (contents withheld)",path.display()))?))
}

fn absolute(path: &Path) -> Result<PathBuf> {
    std::path::absolute(path).with_context(|| format!("bad path {}", path.display()))
}

/// Which herdr session a command should talk to, as given on the command line.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionFlags {
    pub session: Option<String>,
    pub socket: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub socket: PathBuf,
    /// Known only when the session was chosen by name.
    pub name: Option<String>,
}

/// `--session`, then `--socket`, then `HERDR_SOCKET_PATH`, then `HERDR_SESSION`,
/// then herdr's default socket. A name is turned into a socket path by asking
/// herdr (`session list --json`), never by guessing herdr's directory layout.
pub fn resolve_session(flags: &SessionFlags, env: &Env, runner: &dyn Runner) -> Result<Session> {
    if flags.session.is_some() && flags.socket.is_some() {
        bail!("pass --session or --socket, not both");
    }
    if let Some(name) = &flags.session {
        return session_by_name(name, env, runner);
    }
    if let Some(socket) = &flags.socket {
        return Ok(Session {
            socket: absolute(socket)?,
            name: None,
        });
    }
    if let Some(socket) = env.var("HERDR_SOCKET_PATH") {
        return Ok(Session {
            socket: PathBuf::from(socket),
            name: None,
        });
    }
    if let Some(name) = env.var("HERDR_SESSION") {
        return session_by_name(name, env, runner);
    }
    let sessions = herdr::session_list(&env.herdr_bin(), runner).unwrap_or_default();
    let socket = sessions
        .into_iter()
        .find(|s| s.default)
        .map(|s| s.socket_path)
        .unwrap_or_else(|| env.home.join(".config/herdr/herdr.sock"));
    Ok(Session { socket, name: None })
}

fn session_by_name(name: &str, env: &Env, runner: &dyn Runner) -> Result<Session> {
    let sessions = herdr::session_list(&env.herdr_bin(), runner)?;
    match sessions.into_iter().find(|s| s.name == name) {
        Some(found) => Ok(Session {
            socket: found.socket_path,
            name: Some(name.to_string()),
        }),
        None => bail!("herdr has no session named `{name}`; start it with `herdr --session {name}`"),
    }
}

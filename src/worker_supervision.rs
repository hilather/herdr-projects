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

/// Filesystem view of an isolated agent: what the sandbox hides and, when the
/// projects root is covered, which paths under it stay visible. Built only by
/// [`Isolation::for_agent`] so every launch path derives it the same way; it is
/// part of the literal supervisor argv, not of any approval digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Isolation {
    root: String,
    expose: Vec<String>,
    /// Private tmpfs directories and, for each, the needed entries in it that
    /// are bound back (in [`PRIVATE_DIRS`] order).
    private: Vec<(String, Vec<String>)>,
    /// Read-only anchors (`false`) and writable exposures (`true`), parents
    /// first: each later entry is mounted on top of the earlier ones.
    plan: Vec<(String, bool)>,
    hide: Vec<String>,
    /// The worker's own project directory (canonical).
    project: String,
    /// The attempt's submission spool, when the agent is a canonical attempt
    /// (see [`Isolation::with_submission_spool`]).
    spool: Option<String>,
}

/// Names the agent's submission spool directory in its baseline environment.
/// `result submit` and the `review` worker channel write a request there and
/// wait for the ticker's receipt instead of opening the project store, which
/// the sandbox leaves read-only (`crate::submission_spool`).
pub const SUBMISSION_SPOOL_ENV: &str = "HERDR_PROJECTS_SUBMISSION_SPOOL";

/// Owner-writable scratch directories replaced by a private empty tmpfs, so
/// the worker neither reads the owner's temporary files and sockets nor plants
/// files the owner later uses. Needed paths inside are bound back.
const PRIVATE_DIRS: &[&str] = &["/tmp", "/var/tmp", "/dev/shm"];

/// The paths of a repository's Git common directory that a commit on a linked
/// worktree's branch writes: new loose objects, the branch ref (`refs/heads`,
/// or the `reftable` stack) and its reflog. Everything else in the common
/// directory (`config`, `hooks/`, `info/`, `packed-refs`, other refs) stays
/// read-only, as do `objects/pack` and `objects/info` (alternates) inside.
const GIT_WRITABLE: &[&str] = &["objects", "refs/heads", "logs/refs/heads", "reftable"];
const GIT_READ_ONLY: &[&str] = &["objects/pack", "objects/info"];
/// Files of the worktree's own Git directory that tie it to the repository
/// and its creation lock; the rest (index, HEAD, per-worktree logs) is written.
const WORKTREE_READ_ONLY: &[&str] = &["commondir", "gitdir", "locked"];

/// Owner-home entries hidden from every isolated agent: signing and SSH keys,
/// the owner's own agent credentials, the product configuration (owner policy
/// and the reviewer-signer directory under it), Herdr's control sockets and
/// common credential stores. A missing entry is skipped at setup time.
const OWNER_SECRETS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".codex",
    ".claude/.credentials.json",
    ".config/herdr-projects",
    ".config/herdr",
    ".config/gh",
    ".aws",
    ".docker",
    ".kube",
    ".password-store",
    ".local/share/keyrings",
    ".git-credentials",
    ".netrc",
];

/// The sandbox's setup program, run by `/bin/sh` inside the supervisor's user
/// and mount namespaces after gate release. As namespace root it:
///
/// 1. covers the projects root with an empty tmpfs and binds back only the
///    exposed paths (opened before the cover, so the bind reaches the real
///    inodes and shared locks stay shared);
/// 2. replaces each private scratch directory (`/tmp`, `/var/tmp`, `/dev/shm`)
///    with an empty tmpfs, recursively binding back only the needed entries
///    (opened before the cover); a missing or symlinked directory is skipped;
/// 3. walks the read-only/writable plan, parents first: a read-only anchor
///    (owner homes, source repositories and their Git common directories, the
///    own project) is bound onto itself recursively read-only (`ro=recursive`,
///    applied by libmount with mount_setattr), a writable exposure (execution
///    home, its own worktrees, the Git paths a worktree commit writes, and the
///    attempt's submission spool and output directory under the otherwise
///    read-only project `.state`) is bound onto itself on top, writable;
/// 4. mounts an empty read-only tmpfs over every hidden directory and
///    `/dev/null` over every hidden file, then over the owner's SSH agent and
///    tmux socket directories in `/tmp` (enumerated at setup, so the argv
///    stays fixed; a directory the owner does not own is left alone).
///
/// It re-enters the working directory through the new mount tree and execs
/// the agent in a nested user namespace: the agent keeps root there but holds
/// no capability over the mount namespace that owns these mounts, so it cannot
/// unmount, move or remount them (nor remount a read-only one writable), and
/// a mount namespace it creates itself receives them locked. Any failure
/// exits 125 before the agent runs.
const SANDBOX: &str = concat!(
    r#"set -u; fail() { printf 'herdr-projects: worker isolation refused: %s\n' "$1" >&2; exit 125; }; "#,
    r#"root=$1; shift; cwd=$(pwd -P) || fail cwd; "#,
    r#"if [ -n "$root" ]; then n=3; "#,
    r#"for a in "$@"; do case $a in --) break;; expose:*) [ "$n" -le 9 ] || fail expose; p=${a#expose:}; "#,
    r#"eval "exec $n<\"\$p\"" || fail "$p"; n=$((n+1));; esac; done; "#,
    r#"/usr/bin/mount -t tmpfs -o nosuid,nodev,noexec,mode=0755,size=1m herdr-projects-root "$root" || fail "$root"; n=3; "#,
    r#"for a in "$@"; do case $a in --) break;; expose:*) p=${a#expose:}; rel=${p#"$root"/}; "#,
    r#"case $rel in */*) /usr/bin/mkdir -p -- "$root/${rel%/*}" || fail "$p";; esac; "#,
    r#"if [ -d "/proc/self/fd/$n" ]; then /usr/bin/mkdir -- "$root/$rel" || fail "$p"; else : > "$root/$rel" || fail "$p"; fi; "#,
    r#"/usr/bin/mount -c --bind "/proc/self/fd/$n" "$root/$rel" || fail "$p"; eval "exec $n<&-"; n=$((n+1));; esac; done; "#,
    r#"/usr/bin/mount -o remount,bind,ro,nosuid,nodev,noexec "$root" || fail "$root"; fi; "#,
    r#"for d in "$@"; do case $d in --) break;; private:*) d=${d#private:}; if [ -d "$d" ] && [ ! -L "$d" ]; then n=3; "#,
    r#"for a in "$@"; do case $a in --) break;; keep:"$d"/*) [ "$n" -le 9 ] || fail keep; p=${a#keep:}; "#,
    r#"eval "exec $n<\"\$p\"" || fail "$p"; n=$((n+1));; esac; done; "#,
    r#"/usr/bin/mount -t tmpfs -o nosuid,nodev,mode=1777 herdr-projects-private "$d" || fail "$d"; n=3; "#,
    r#"for a in "$@"; do case $a in --) break;; keep:"$d"/*) p=${a#keep:}; if [ -d "/proc/self/fd/$n" ]; then "#,
    r#"/usr/bin/mkdir -- "$p" && /usr/bin/mount -c --rbind "/proc/self/fd/$n" "$p" || fail "$p"; "#,
    r#"else : > "$p" && /usr/bin/mount -c --bind "/proc/self/fd/$n" "$p" || fail "$p"; fi; "#,
    r#"eval "exec $n<&-"; n=$((n+1));; esac; done; fi;; esac; done; "#,
    r#"for a in "$@"; do case $a in --) break;; "#,
    r#"ro:*) p=${a#ro:}; if [ -e "$p" ]; then /usr/bin/mount --rbind -o ro=recursive "$p" "$p" || fail "$p"; fi;; "#,
    r#"rw:*) p=${a#rw:}; if [ -e "$p" ]; then /usr/bin/mount --rbind -o rw "$p" "$p" || fail "$p"; fi;; "#,
    r#"esac; done; "#,
    r#"for a in "$@"; do case $a in --) break;; hide:*) p=${a#hide:}; "#,
    r#"if [ -d "$p" ]; then /usr/bin/mount -t tmpfs -o ro,nosuid,nodev,noexec,size=4k,mode=0555 herdr-projects-hidden "$p" || fail "$p"; "#,
    r#"elif [ -e "$p" ]; then { /usr/bin/mount --bind /dev/null "$p" && /usr/bin/mount -o remount,bind,ro "$p"; } || fail "$p"; fi;; esac; done; "#,
    r#"for p in /tmp/ssh-* /tmp/tmux-*; do if [ -d "$p" ] && [ -O "$p" ]; then "#,
    r#"/usr/bin/mount -t tmpfs -o ro,nosuid,nodev,noexec,size=4k,mode=0555 herdr-projects-hidden "$p" || fail "$p"; fi; done; "#,
    r#"cd -- "$cwd" || fail "$cwd"; while [ "$1" != -- ]; do shift; done; shift; "#,
    r#"exec /usr/bin/unshare --user --map-root-user -- "$@""#,
);

/// Lexically normal absolute UTF-8 path, bounded and free of control
/// characters, `.` and `..`; the sandbox receives it as a literal argument.
fn normal(path: &Path) -> Result<String> {
    use std::path::Component;
    let text = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("isolation path is not UTF-8"))?;
    ensure!(
        path.is_absolute()
            && text.len() <= 1024
            && !text.chars().any(char::is_control)
            && path
                .components()
                .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
            && path.components().count() > 1,
        "invalid isolation path {text:?}"
    );
    Ok(path.components().collect::<std::path::PathBuf>().to_str().unwrap_or_default().to_owned())
}

/// The path and, when it exists, its canonical target: a check must hold for
/// the name the sandbox mounts on and for the directory it really reaches.
fn forms(path: &str) -> Vec<std::path::PathBuf> {
    let mut forms = vec![std::path::PathBuf::from(path)];
    if let Ok(real) = Path::new(path).canonicalize()
        && real != forms[0]
    {
        forms.push(real);
    }
    forms
}

/// The owner's home directories: the account's passwd entry and, when it
/// differs, the controller's `HOME`. Both are hidden-secret anchors.
fn owner_homes() -> Result<Vec<String>> {
    let mut homes = Vec::new();
    let mut buffer = vec![0u8; 16384];
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found = std::ptr::null_mut();
    // SAFETY: the buffer outlives every pointer getpwuid_r stores in `entry`.
    let status = unsafe {
        libc::getpwuid_r(libc::geteuid(), &mut entry, buffer.as_mut_ptr().cast(), buffer.len(), &mut found)
    };
    if status == 0 && !found.is_null() && !entry.pw_dir.is_null() {
        // SAFETY: pw_dir points into `buffer`, NUL-terminated by getpwuid_r.
        let dir = unsafe { std::ffi::CStr::from_ptr(entry.pw_dir) };
        if let Ok(dir) = dir.to_str()
            && let Ok(dir) = normal(Path::new(dir))
        {
            homes.push(dir);
        }
    }
    if let Some(home) = std::env::var_os("HOME")
        && let Ok(home) = normal(Path::new(&home))
        && !homes.contains(&home)
    {
        homes.push(home);
    }
    ensure!(!homes.is_empty(), "owner home is unknown; worker isolation cannot hide its secrets");
    Ok(homes)
}

impl Isolation {
    /// The sandbox for one agent. `project` is its own project directory: the
    /// projects root above it is covered and only `project` (plus the root's
    /// shared execution lock and any needed path under the root) stays visible.
    /// `socket` is the Herdr control socket that must be unreachable; `config`
    /// the pinned owner configuration file; `extra` the owner-declared hidden
    /// paths from it (absolute, or `~/` relative to each owner home).
    /// `worktrees` are the retained (worktree, Git directory, common directory)
    /// triples of the attempt's linked worktrees: the worktree is writable
    /// except its `.git` pointer, and of the Git directories only the paths a
    /// commit on its branch writes. The owner homes, `repositories` and their
    /// common directories are otherwise read-only, as is `project` with its
    /// whole `.state` (store, locks, objects, other attempts' worktrees and
    /// outputs; [`Self::with_submission_spool`] adds the attempt's own spool
    /// and output directory); `home` stays writable. Refuses, before any effect, when a
    /// hidden path would contain or cover a path the agent needs.
    #[allow(clippy::too_many_arguments)]
    pub fn for_agent(
        project: &Path,
        home: &Path,
        cwd: &Path,
        agent: &Path,
        repositories: &[&Path],
        worktrees: &[(&Path, &Path, &Path)],
        config: Option<&Path>,
        socket: Option<&Path>,
        extra: &[String],
    ) -> Result<Self> {
        let project = normal(&project.canonicalize()?)?;
        let root = normal(Path::new(&project).parent().ok_or_else(|| anyhow::anyhow!("project has no root"))?)?;
        let home = normal(home)?;
        let cwd = normal(cwd)?;
        let agent = normal(agent)?;
        let homes = owner_homes()?;
        let mut hide = Vec::new();
        for owner in &homes {
            for entry in OWNER_SECRETS {
                hide.push(format!("{owner}/{entry}"));
            }
        }
        hide.push(format!("/run/user/{}", unsafe { libc::geteuid() }));
        ensure!(extra.len() <= 16, "too many owner-declared hidden paths");
        for path in extra {
            match path.strip_prefix("~/") {
                Some(relative) => {
                    for owner in &homes {
                        hide.push(normal(&Path::new(owner).join(relative))?);
                    }
                }
                None => hide.push(normal(Path::new(path))?),
            }
        }
        if let Some(config) = config {
            let config = normal(config)?;
            let parent = Path::new(&config).parent().map(normal).transpose()?;
            hide.push(config);
            if let Some(parent) = parent {
                hide.push(format!("{parent}/review-signer"));
            }
        }
        if let Some(socket) = socket {
            let socket = normal(socket)?;
            // A dedicated socket directory is hidden whole, so a socket the
            // server recreates stays hidden; a shared one hides the file only.
            let parent = Path::new(&socket).parent().map(normal).transpose()?;
            let shared = |dir: &str| {
                matches!(dir, "/tmp" | "/var/tmp" | "/run" | "/dev/shm")
                    || dir.starts_with("/run/user/") && dir.matches('/').count() == 3
                    || [&project, &home, &cwd, &root].iter().any(|n| Path::new(n.as_str()).starts_with(dir))
                    || repositories.iter().any(|r| r.starts_with(dir))
                    || homes.iter().any(|h| h == dir)
            };
            if let Some(parent) = parent.filter(|parent| !shared(parent)) {
                hide.push(parent);
            }
            hide.push(socket);
        }
        hide.sort();
        hide.dedup();
        // Needed paths stay visible: under the root they are bound back.
        let mut needed = vec![home.clone(), cwd.clone(), agent.clone(), project.clone()];
        for repository in repositories {
            needed.push(normal(repository)?);
        }
        let mut git = Vec::new();
        for (worktree, directory, common) in worktrees {
            let (worktree, directory, common) = (normal(worktree)?, normal(directory)?, normal(common)?);
            ensure!(
                Path::new(&directory).parent() == Some(Path::new(&common).join("worktrees").as_path())
                    && Path::new(&worktree).starts_with(Path::new(&project).join(".state/worktrees"))
                    && Path::new(&worktree) != Path::new(&project).join(".state/worktrees"),
                "worktree {worktree} or its Git directory {directory} is outside its project or common directory {common}"
            );
            git.push((worktree, directory, common));
        }
        let repositories_end = needed.len();
        for (_, directory, common) in &git {
            needed.extend([directory.clone(), common.clone()]);
        }
        for secret in &hide {
            for secret in forms(secret) {
                for need in &needed {
                    for need in forms(need) {
                        ensure!(
                            !need.starts_with(&secret),
                            "worker isolation would hide {} (inside {}); move it out of the owner's secret locations",
                            need.display(),
                            secret.display()
                        );
                    }
                }
                for dir in [&home, &project] {
                    for dir in forms(dir) {
                        ensure!(
                            !secret.starts_with(&dir),
                            "worker isolation refuses {}: it contains the owner's secret location {}; use a dedicated directory",
                            dir.display(),
                            secret.display()
                        );
                    }
                }
            }
        }
        for form in forms(&cwd) {
            ensure!(
                !form.starts_with(&root) || form.starts_with(&project),
                "worker working directory must be inside its own project or outside the projects root"
            );
        }
        let mut expose = vec![project.clone(), format!("{root}/.execution.lock")];
        for need in [&home, &agent].into_iter().chain(needed[4..repositories_end].iter()) {
            if Path::new(need).starts_with(&root) && !Path::new(need).starts_with(&project) {
                expose.push(need.clone());
            }
        }
        expose.sort();
        expose.dedup();
        let nested = expose.clone();
        expose.retain(|path| !nested.iter().any(|other| other != path && Path::new(path).starts_with(other)));
        ensure!(expose.len() <= 7, "too many paths to expose under the projects root");
        // Read-only anchors and the writable exposures on top of them.
        let mut plan: Vec<(String, bool)> = homes.iter().map(|h| (h.clone(), false)).collect();
        plan.extend(needed[4..repositories_end].iter().map(|r| (r.clone(), false)));
        // The whole project, `.state` included, is read-only: the store is
        // written only by the ticker, which ingests the submission spool.
        plan.push((project.clone(), false));
        for (worktree, directory, common) in &git {
            plan.extend([(worktree.clone(), true), (format!("{worktree}/.git"), false)]);
            plan.push((common.clone(), false));
            plan.extend(GIT_WRITABLE.iter().map(|p| (format!("{common}/{p}"), true)));
            plan.extend(GIT_READ_ONLY.iter().map(|p| (format!("{common}/{p}"), false)));
            plan.push((directory.clone(), true));
            plan.extend(WORKTREE_READ_ONLY.iter().map(|p| (format!("{directory}/{p}"), false)));
        }
        plan.extend([(home.clone(), true), (format!("{root}/.execution.lock"), true)]);
        // Parents first; an exposure the agent needs wins over an equal anchor.
        plan.sort_by(|a, b| Path::new(&a.0).cmp(Path::new(&b.0)).then(a.1.cmp(&b.1)));
        plan.reverse();
        plan.dedup_by(|later, earlier| later.0 == earlier.0);
        plan.reverse();
        // An entry inside one of the same mode adds nothing: every submount
        // already exists when the plan runs, and read-only is recursive.
        let mut kept: Vec<(String, bool)> = Vec::new();
        for entry in plan {
            let enclosing = kept.iter().rev().find(|(p, _)| Path::new(&entry.0).starts_with(p));
            if enclosing.is_none_or(|(_, writable)| *writable != entry.1) {
                kept.push(entry);
            }
        }
        let plan = kept;
        // Private scratch directories keep only the entries needed paths
        // (named or real) lie in.
        let mut private = Vec::new();
        for dir in PRIVATE_DIRS {
            let mut keep = Vec::new();
            for need in needed.iter().chain(std::iter::once(&root)) {
                for form in forms(need) {
                    if let Ok(rest) = form.strip_prefix(dir) {
                        let first = rest.components().next().ok_or_else(|| {
                            anyhow::anyhow!("worker isolation cannot keep {} private: an agent path is {dir} itself", need)
                        })?;
                        keep.push(format!("{dir}/{}", first.as_os_str().to_str().unwrap_or_default()));
                    }
                }
            }
            keep.sort();
            keep.dedup();
            ensure!(keep.len() <= 7, "too many agent paths directly under {dir}");
            private.push(((*dir).to_owned(), keep));
        }
        Ok(Self { root, expose, private, plan, hide, project, spool: None })
    }

    /// Give a canonical attempt its two writable places under the read-only
    /// project `.state`: the submission spool `.state/spool/<attempt>` (named
    /// to the agent in [`SUBMISSION_SPOOL_ENV`]) and its output directory
    /// `.state/worker-output/<attempt>`. Both are created by gate release
    /// before the sandbox runs; a missing one stays read-only (the sandbox
    /// skips missing paths), so submission fails closed. No other attempt's
    /// spool or output is writable to this agent.
    pub fn with_submission_spool(mut self, attempt: &str) -> Result<Self> {
        ensure!(
            !attempt.is_empty()
                && attempt.len() <= 128
                && attempt.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
                && !attempt.starts_with('.'),
            "invalid attempt for the submission spool"
        );
        let spool = format!("{}/.state/spool/{attempt}", self.project);
        let output = format!("{}/.state/worker-output/{attempt}", self.project);
        self.plan.extend([(spool.clone(), true), (output, true)]);
        // Parents first, as `for_agent` orders the plan: both are leaves under
        // the read-only project, mounted after it.
        self.plan.sort_by(|a, b| Path::new(&a.0).cmp(Path::new(&b.0)).then(a.1.cmp(&b.1)));
        self.spool = Some(spool);
        Ok(self)
    }

    fn arguments(&self) -> Vec<String> {
        let mut args = vec![self.root.clone()];
        args.extend(self.expose.iter().map(|p| format!("expose:{p}")));
        for (dir, keep) in &self.private {
            args.extend(keep.iter().map(|p| format!("keep:{p}")));
            args.push(format!("private:{dir}"));
        }
        args.extend(self.plan.iter().map(|(p, writable)| format!("{}:{p}", if *writable { "rw" } else { "ro" })));
        args.extend(self.hide.iter().map(|p| format!("hide:{p}")));
        args
    }
}

/// Explicit baseline environment for the agent. Only the credential-store home
/// is variable; secrets and arbitrary inherited loader/config hooks are excluded.
/// The waiting gate still runs in the native terminal's environment, but its
/// eventual exec clears that environment, applies `isolation` and executes the
/// approved agent in a nested user namespace (see [`Isolation`]).
pub fn isolated_gated_command(
    executable: &Path,
    arguments: &[String],
    wall: u64,
    token: &str,
    home: &Path,
    isolation: &Isolation,
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
        "/bin/sh".into(),
        "-c".into(),
        SANDBOX.into(),
        "herdr-projects-worker-sandbox".into(),
    ];
    args.extend(isolation.arguments());
    args.extend([
        "--".into(),
        "/usr/bin/env".into(),
        "-i".into(),
        format!("HOME={home}"),
        "PATH=/usr/bin:/bin".into(),
        "LANG=C.UTF-8".into(),
        "LC_ALL=C.UTF-8".into(),
        "TERM=xterm-256color".into(),
    ]);
    if let Some(spool) = &isolation.spool {
        args.push(format!("{SUBMISSION_SPOOL_ENV}={spool}"));
    }
    args.push(
        executable
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("agent executable is not UTF-8"))?
            .into(),
    );
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
    use crate::execution_guard::GatedSpawn;
    use super::*;
    #[test]
    #[cfg(target_os = "linux")]
    fn isolated_gate_passes_only_the_frozen_baseline_environment() {
        use std::{
            io::Write,
            process::{Command, Stdio},
        };
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let project = root_path.join("project");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(root_path.join(".execution.lock"), b"").unwrap();
        let isolation = Isolation::for_agent(&project, home.path(), &project, Path::new("/usr/bin/env"), &[], &[], None, None, &[]).unwrap();
        let argv = isolated_gated_command(
            Path::new("/usr/bin/env"),
            &[],
            5,
            "release-env",
            home.path(),
            &isolation,
        )
        .unwrap();
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(&project)
            .env("UNAPPROVED_VARIABLE", "must-not-reach-agent")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn_gated()
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
                .spawn_gated()
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
            .spawn_gated()
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
            .output_gated()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap(), payload);
        assert!(!root.path().join("injected").exists());
        assert!(posix_command(&[]).is_err());
        assert!(posix_command(&["bad\0argument".into()]).is_err());
    }
}

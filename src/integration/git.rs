//! Local git only. `update-ref` always carries the expected old oid.
//! Network transports and branch deletion are rejected before exec.
use std::{
    fs,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};

use crate::runner::{Cmd, Output, RealRunner, Runner};

const ALLOWED: &[&str] = &[
    "init",
    "rev-parse",
    "symbolic-ref",
    "merge-tree",
    "commit-tree",
    "update-ref",
    "cat-file",
    "checkout",
    "worktree",
    "pack-objects",
];

pub(crate) fn command_allowed(args: &[String]) -> bool {
    let Some(command) = git_subcommand(args) else {
        return false;
    };
    if !ALLOWED.contains(&command) {
        return false;
    }
    if args.iter().any(|arg| {
        let lower = arg.to_ascii_lowercase();
        lower.contains("://")
            || matches!(
                lower.as_str(),
                "push" | "fetch" | "pull" | "clone" | "ls-remote" | "remote" | "send-pack"
                    | "receive-pack" | "upload-pack"
            )
    }) {
        return false;
    }
    if command == "update-ref" {
        return cas_args(args);
    }
    true
}

fn git_subcommand(args: &[String]) -> Option<&str> {
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "-c" | "-C" => index += 2,
            other if other.starts_with('-') => return None,
            other => return Some(other),
        }
    }
    None
}

fn cas_args(args: &[String]) -> bool {
    if args.len() != 4 || args[0] != "update-ref" {
        return false;
    }
    let reference = &args[1];
    let new_oid = &args[2];
    let old_oid = &args[3];
    reference.starts_with("refs/heads/")
        && lowercase_oid(new_oid)
        && lowercase_oid(old_oid)
        && new_oid.len() == old_oid.len()
}

fn lowercase_oid(value: &str) -> bool {
    (value.len() == 40 || value.len() == 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn git_env() -> Vec<(String, String)> {
    vec![
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("HOME".into(), "/".into()),
        ("LANG".into(), "C".into()),
        ("LC_ALL".into(), "C".into()),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
        ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
        ("GIT_CONFIG_COUNT".into(), "2".into()),
        ("GIT_CONFIG_KEY_0".into(), "core.hooksPath".into()),
        ("GIT_CONFIG_VALUE_0".into(), "/dev/null".into()),
        ("GIT_CONFIG_KEY_1".into(), "safe.directory".into()),
        ("GIT_CONFIG_VALUE_1".into(), "*".into()),
        ("GIT_TERMINAL_PROMPT".into(), "0".into()),
        ("GIT_NO_LAZY_FETCH".into(), "1".into()),
        ("GIT_NO_REPLACE_OBJECTS".into(), "1".into()),
        ("GIT_OPTIONAL_LOCKS".into(), "0".into()),
        ("GIT_ALLOW_PROTOCOL".into(), "".into()),
        ("GIT_AUTHOR_NAME".into(), "integrator".into()),
        ("GIT_AUTHOR_EMAIL".into(), "integrator@example.com".into()),
        ("GIT_COMMITTER_NAME".into(), "integrator".into()),
        ("GIT_COMMITTER_EMAIL".into(), "integrator@example.com".into()),
    ]
}

fn run_git(cwd: &Path, args: &[String]) -> Result<Output> {
    run_git_stdin(cwd, args, None)
}

fn run_git_stdin(cwd: &Path, args: &[String], stdin: Option<String>) -> Result<Output> {
    if !command_allowed(args) {
        bail!("git command is not a local integration effect");
    }
    let mut command = Cmd::new("/usr/bin/git", Duration::from_secs(30));
    command.args = args.to_vec();
    command.cwd = Some(cwd.to_path_buf());
    command.env_clear = true;
    command.env = git_env();
    command.stdin = stdin;
    RealRunner
        .run(&command)
        .with_context(|| format!("git {args:?}"))
}

fn one_line(output: &Output) -> Result<String> {
    if !output.success() {
        bail!("git failed: {}", output.stderr.trim());
    }
    let text = output.stdout.trim();
    if text.is_empty() || text.contains('\n') {
        bail!("git returned an unexpected line");
    }
    Ok(text.to_string())
}

fn oid_line(output: &Output, len: usize) -> Result<String> {
    let text = one_line(output)?;
    if text.len() != len || !text.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("git returned an unexpected oid");
    }
    Ok(text)
}

pub struct GitRepo {
    pub path: PathBuf,
    pub identity: String,
    git_dir: PathBuf,
}

impl GitRepo {
    pub fn open(path: &Path) -> Result<Self> {
        let path = path.canonicalize().context("integration repository")?;
        let git = path.join(".git");
        let meta = fs::symlink_metadata(&git).context("integration repository git dir")?;
        if !meta.is_dir() || meta.file_type().is_symlink() {
            bail!("integration repository must be a normal local checkout");
        }
        let identity = path
            .to_str()
            .context("repository path is not utf-8")?
            .to_string();
        let git_dir = path.join(".git");
        Ok(Self {
            path,
            identity,
            git_dir,
        })
    }

    pub fn ref_oid(&self, reference: &str) -> Result<Option<String>> {
        let len = self.object_len()?;
        let output = run_git(
            &self.path,
            &[
                "rev-parse".into(),
                "--verify".into(),
                "--end-of-options".into(),
                format!("{reference}^{{commit}}"),
            ],
        )?;
        if !output.success() {
            return Ok(None);
        }
        Ok(Some(oid_line(&output, len)?))
    }

    pub fn is_checked_out(&self, reference: &str) -> Result<bool> {
        let symbolic = run_git(&self.path, &["symbolic-ref".into(), "--quiet".into(), "HEAD".into()])?;
        if symbolic.success() && symbolic.stdout.trim() == reference {
            return Ok(true);
        }
        let listed = run_git(
            &self.path,
            &["worktree".into(), "list".into(), "--porcelain".into()],
        )?;
        if !listed.success() {
            bail!("git worktree list failed: {}", listed.stderr.trim());
        }
        Ok(listed
            .stdout
            .lines()
            .any(|line| line == format!("branch {reference}")))
    }

    pub fn object_len(&self) -> Result<usize> {
        let output = run_git(&self.path, &["rev-parse".into(), "--show-object-format".into()])?;
        match one_line(&output)?.as_str() {
            "sha1" => Ok(40),
            "sha256" => Ok(64),
            other => bail!("unsupported object format {other}"),
        }
    }

    pub fn build_merge(&self, work: &Path, base: &str, verified: &str) -> Result<BuiltCommit> {
        let work = work.canonicalize().context("integration work directory")?;
        if overlaps(&work, &self.path) {
            bail!("integration work directory must stay outside the repository");
        }
        let isolated = work.join("isolated");
        if isolated.exists() {
            fs::remove_dir_all(&isolated).context("reset isolated git dir")?;
        }
        let template = work.join("template");
        fs::create_dir_all(&template).context("git template")?;
        let init = run_git(
            &work,
            &[
                "init".into(),
                "--template".into(),
                template.display().to_string(),
                isolated.display().to_string(),
            ],
        )?;
        if !init.success() {
            bail!("git init failed: {}", init.stderr.trim());
        }
        let alternates = isolated.join(".git/objects/info");
        fs::create_dir_all(&alternates).context("git alternates")?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o644)
            .open(alternates.join("alternates"))
            .context("git alternates")?;
        writeln!(file, "{}", self.git_dir.join("objects").display()).context("git alternates")?;
        let merged = run_git(
            &isolated,
            &[
                "merge-tree".into(),
                "--write-tree".into(),
                base.into(),
                verified.into(),
            ],
        )?;
        if merged.code == Some(1) {
            return Ok(BuiltCommit::Conflict);
        }
        let len = self.object_len()?;
        let tree = oid_line(&merged, len).context("merge-tree")?;
        let commit = run_git(
            &isolated,
            &[
                "commit-tree".into(),
                tree.clone(),
                "-p".into(),
                base.into(),
                "-p".into(),
                verified.into(),
                "-m".into(),
                "integrate".into(),
            ],
        )?;
        let oid = oid_line(&commit, len).context("commit-tree")?;
        // Packs in the source repo are invisible once alternates are removed.
        self.materialize_reachable(&isolated, &oid)?;
        drop_alternates(&isolated, &oid)?;
        let checkout = run_git(
            &isolated,
            &["checkout".into(), "--detach".into(), oid.clone()],
        )?;
        if !checkout.success() {
            bail!("git checkout of the candidate failed: {}", checkout.stderr.trim());
        }
        Ok(BuiltCommit::Ready {
            oid,
            tree,
            checkout: isolated,
        })
    }

    /// Reuse or recreate a detached checkout of a commit already recorded for this generation.
    pub fn checkout_candidate(&self, work: &Path, oid: &str) -> Result<PathBuf> {
        let work = work.canonicalize().context("integration work directory")?;
        if overlaps(&work, &self.path) {
            bail!("integration work directory must stay outside the repository");
        }
        let isolated = work.join("isolated");
        let head = if isolated.join(".git").is_dir() {
            run_git(&isolated, &["rev-parse".into(), "HEAD".into()])?
        } else {
            self.prepare_isolated(&work)?;
            Output::default()
        };
        self.materialize_reachable(&isolated, oid)?;
        drop_alternates(&isolated, oid)?;
        if head.stdout.trim() != oid {
            let checkout = run_git(
                &isolated,
                &["checkout".into(), "--detach".into(), oid.into()],
            )?;
            if !checkout.success() {
                bail!(
                    "candidate checkout is unavailable: {}",
                    checkout.stderr.trim()
                );
            }
        }
        Ok(isolated)
    }

    pub fn has_object(&self, oid: &str) -> Result<bool> {
        let output = run_git(&self.path, &["cat-file".into(), "-e".into(), oid.into()])?;
        Ok(output.success())
    }

    /// Walk the candidate through alternates, including source packs, and store a local pack.
    fn materialize_reachable(&self, isolated: &Path, oid: &str) -> Result<()> {
        fs::create_dir_all(isolated.join(".git/objects/pack")).context("object pack directory")?;
        let packed = run_git_stdin(
            isolated,
            &[
                "pack-objects".into(),
                "--revs".into(),
                ".git/objects/pack/pack".into(),
            ],
            Some(format!("{oid}\n")),
        )?;
        if !packed.success() {
            bail!(
                "could not materialize candidate objects: {}",
                packed.stderr.trim()
            );
        }
        Ok(())
    }

    fn prepare_isolated(&self, work: &Path) -> Result<PathBuf> {
        let isolated = work.join("isolated");
        if isolated.exists() {
            fs::remove_dir_all(&isolated).context("reset isolated git dir")?;
        }
        let template = work.join("template");
        fs::create_dir_all(&template).context("git template")?;
        let init = run_git(
            work,
            &[
                "init".into(),
                "--template".into(),
                template.display().to_string(),
                isolated.display().to_string(),
            ],
        )?;
        if !init.success() {
            bail!("git init failed: {}", init.stderr.trim());
        }
        let alternates = isolated.join(".git/objects/info");
        fs::create_dir_all(&alternates).context("git alternates")?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o644)
            .open(alternates.join("alternates"))
            .context("git alternates")?;
        writeln!(file, "{}", self.git_dir.join("objects").display()).context("git alternates")?;
        Ok(isolated)
    }

    pub fn copy_objects_from(&self, checkout: &Path) -> Result<()> {
        copy_loose(&checkout.join(".git/objects"), &self.git_dir.join("objects"))
    }

    pub fn commit_matches(
        &self,
        oid: &str,
        tree: &str,
        parent_base: &str,
        parent_verified: &str,
    ) -> Result<bool> {
        let len = self.object_len()?;
        if oid.len() != len || tree.len() != len {
            return Ok(false);
        }
        let kind = run_git(&self.path, &["cat-file".into(), "-t".into(), oid.into()])?;
        if !kind.success() || kind.stdout.trim() != "commit" {
            return Ok(false);
        }
        let actual_tree = run_git(
            &self.path,
            &[
                "rev-parse".into(),
                "--verify".into(),
                "--end-of-options".into(),
                format!("{oid}^{{tree}}"),
            ],
        )?;
        let first = run_git(
            &self.path,
            &[
                "rev-parse".into(),
                "--verify".into(),
                "--end-of-options".into(),
                format!("{oid}^1"),
            ],
        )?;
        let second = run_git(
            &self.path,
            &[
                "rev-parse".into(),
                "--verify".into(),
                "--end-of-options".into(),
                format!("{oid}^2"),
            ],
        )?;
        Ok(actual_tree.success()
            && first.success()
            && second.success()
            && actual_tree.stdout.trim() == tree
            && first.stdout.trim() == parent_base
            && second.stdout.trim() == parent_verified)
    }

    /// Compare-and-swap. A missing expected oid is a refused command, not a forced update.
    pub fn cas_ref(&self, reference: &str, new_oid: &str, old_oid: &str) -> Result<bool> {
        let output = run_git(
            &self.path,
            &[
                "update-ref".into(),
                reference.into(),
                new_oid.into(),
                old_oid.into(),
            ],
        )?;
        if output.success() {
            return Ok(true);
        }
        Ok(false)
    }

    #[cfg(test)]
    pub fn advance_ref(&self, reference: &str, old_oid: &str) -> Result<String> {
        let len = self.object_len()?;
        let tree = run_git(
            &self.path,
            &[
                "rev-parse".into(),
                "--verify".into(),
                "--end-of-options".into(),
                format!("{old_oid}^{{tree}}"),
            ],
        )?;
        let tree = oid_line(&tree, len)?;
        let commit = run_git(
            &self.path,
            &[
                "commit-tree".into(),
                tree,
                "-p".into(),
                old_oid.into(),
                "-m".into(),
                "concurrent".into(),
            ],
        )?;
        let oid = oid_line(&commit, len)?;
        if !self.cas_ref(reference, &oid, old_oid)? {
            bail!("could not move the ref to simulate a stale base");
        }
        Ok(oid)
    }
}

fn drop_alternates(isolated: &Path, oid: &str) -> Result<()> {
    let path = isolated.join(".git/objects/info/alternates");
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("drop alternates"),
    }
    // A missing packed tree must fail the build, not a later policy check.
    let kind = run_git(
        isolated,
        &["cat-file".into(), "-t".into(), format!("{oid}^{{tree}}")],
    )?;
    if !kind.success() || kind.stdout.trim() != "tree" {
        bail!("candidate tree was not materialized");
    }
    Ok(())
}

fn copy_loose(source: &Path, dest: &Path) -> Result<()> {
    if !source.is_dir() {
        return Ok(());
    }
    for prefix in fs::read_dir(source).context("git objects")? {
        let prefix = prefix.context("git objects")?;
        let name = prefix.file_name();
        let name = name.to_string_lossy();
        if name.len() != 2 || !name.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let dest_dir = dest.join(name.as_ref());
        fs::create_dir_all(&dest_dir).context("repository objects")?;
        for file in fs::read_dir(prefix.path()).context("git object")? {
            let file = file.context("git object")?;
            let target = dest_dir.join(file.file_name());
            if let Ok(meta) = fs::symlink_metadata(&target) {
                if meta.file_type().is_symlink() || !meta.is_file() {
                    bail!("repository object is not a regular file");
                }
                continue;
            }
            let bytes = fs::read(file.path()).context("read candidate object")?;
            let mut out = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o444)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&target)
                .context("write candidate object")?;
            out.write_all(&bytes).context("write candidate object")?;
            out.sync_all().context("sync candidate object")?;
        }
        fs::File::open(&dest_dir)
            .and_then(|dir| dir.sync_all())
            .context("sync object directory")?;
    }
    Ok(())
}

pub enum BuiltCommit {
    Conflict,
    Ready {
        oid: String,
        tree: String,
        checkout: PathBuf,
    },
}

fn overlaps(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broker_rejects_network_and_non_cas_updates() {
        for command in ["push", "fetch", "pull", "clone", "ls-remote"] {
            assert!(!command_allowed(&[command.into(), "origin".into()]));
        }
        assert!(!command_allowed(&[
            "update-ref".into(),
            "-d".into(),
            "refs/heads/integration".into()
        ]));
        assert!(!command_allowed(&[
            "clone".into(),
            "https://example.com/repo.git".into()
        ]));
        let oid = "ab".repeat(20);
        assert!(command_allowed(&[
            "update-ref".into(),
            "refs/heads/integration".into(),
            oid.clone(),
            oid
        ]));
    }
}

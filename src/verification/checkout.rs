//! Fresh checkout from the signed owner base and retained untrusted objects.
//! Hooks are disabled; the only fetch is from the local owner repository.
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};

use crate::{
    runner::{Cmd, RealRunner, Runner},
    store::verification::RetainedObject,
};

fn overlaps(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

/// Delete only a canonical checkout that does not contain the open database.
fn reset_checkout(checkout: &Path, store_file: &Path, store_dir: &Path) -> Result<()> {
    let meta = match fs::symlink_metadata(checkout) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("checkout path"),
    };
    if meta.file_type().is_symlink() {
        bail!("verification checkout must stay outside the project store");
    }
    let canonical = checkout.canonicalize().context("checkout path")?;
    if overlaps(&canonical, store_file) || overlaps(&canonical, store_dir) {
        bail!("verification checkout must stay outside the project store");
    }
    fs::remove_dir_all(&canonical).context("reset checkout")?;
    Ok(())
}

pub struct Checkout {
    pub path: PathBuf,
    pub commit: String,
    pub tree: String,
}

pub fn materialize(
    work: &Path,
    store_file: &Path,
    objects: &[RetainedObject],
    oid: &str,
    object_format: &str,
    trusted_base: Option<(&Path, &str)>,
) -> Result<Checkout> {
    if !matches!(object_format, "sha1" | "sha256") {
        bail!("unsupported Git object format");
    }
    let work = work.canonicalize().context("verification work directory")?;
    let store_file = store_file.canonicalize().context("project store")?;
    let store_dir = store_file
        .parent()
        .context("project store directory")?
        .canonicalize()
        .context("project store directory")?;
    // Either nesting can make a later delete or a bind include the open database.
    if overlaps(&work, &store_file) || overlaps(&work, &store_dir) {
        bail!("verification checkout must stay outside the project store");
    }
    let path = work.join("checkout");
    if overlaps(&path, &store_file) || overlaps(&path, &store_dir) {
        bail!("verification checkout must stay outside the project store");
    }
    reset_checkout(&path, &store_file, &store_dir)?;
    let template = work.join("template");
    fs::create_dir_all(&template).context("git template")?;
    git(
        &work,
        &[
            "init".into(),
            format!("--object-format={object_format}"),
            "--template".into(),
            template.display().to_string(),
            path.display().to_string(),
        ],
        None,
    )?;
    if let Some((repository, base)) = trusted_base {
        // Only the signed contract's base is imported. No alternate remains,
        // and no candidate is fetched from an attempt or quarantine.
        git(&path, &[
            "-c".into(), "protocol.file.allow=always".into(),
            "-c".into(), "fetch.fsckObjects=true".into(),
            "-c".into(), "core.hooksPath=/dev/null".into(),
            "fetch".into(), "--no-tags".into(), "--no-write-fetch-head".into(),
            "--".into(), repository.display().to_string(), base.into(),
        ], Some(&path)).context("owner repository base is unavailable or invalid")?;
    }
    for object in objects {
        if object
            .relative_path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        {
            bail!("retained object path escapes the checkout");
        }
        let dest = path.join(".git/objects").join(&object.relative_path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).context("git object directory")?;
        }
        fs::write(&dest, &object.bytes).with_context(|| format!("write {}", object.oid))?;
    }
    // fsck checks both object identity and complete connectivity before any
    // candidate files are exposed to acceptance checks.
    git(&path, &["fsck".into(), "--full".into(), "--strict".into(),
        "--no-reflogs".into(), "--no-dangling".into(), oid.into()], Some(&path))
        .context("candidate object hash mismatch or missing referenced object")?;
    git(
        &path,
        &[
            "-c".into(),
            "core.hooksPath=/dev/null".into(),
            "checkout".into(),
            "--detach".into(),
            oid.into(),
        ],
        Some(&path),
    )?;
    let commit = git_text(&path, &["rev-parse".into(), "HEAD".into()])?;
    let tree = git_text(&path, &["rev-parse".into(), format!("{oid}^{{tree}}")])?;
    if commit != oid {
        bail!("checkout did not land on the retained commit");
    }
    let _ = fs::remove_dir_all(path.join(".git/hooks"));
    Ok(Checkout { path, commit, tree })
}

fn git(cwd: &Path, args: &[String], dir: Option<&Path>) -> Result<()> {
    let output = run(cwd, args, dir)?;
    if !output.success() {
        bail!("git checkout failed: {}", output.stderr.trim());
    }
    Ok(())
}

fn git_text(cwd: &Path, args: &[String]) -> Result<String> {
    let output = run(cwd, args, Some(cwd))?;
    if !output.success() {
        bail!("git rev-parse failed: {}", output.stderr.trim());
    }
    let text = output.stdout.trim();
    if text.is_empty() || text.contains('\n') || !text.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("git rev-parse returned an unexpected oid");
    }
    Ok(text.to_string())
}

fn run(cwd: &Path, args: &[String], dir: Option<&Path>) -> Result<crate::runner::Output> {
    let mut command = Cmd::new("/usr/bin/git", Duration::from_secs(30));
    command.args = args.to_vec();
    command.cwd = Some(dir.unwrap_or(cwd).to_path_buf());
    command.env_clear = true;
    command.env = git_env();
    RealRunner.run(&command).context("git failed")
}

fn git_env() -> Vec<(String, String)> {
    vec![
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("HOME".into(), "/".into()),
        ("LANG".into(), "C".into()),
        ("LC_ALL".into(), "C".into()),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
        ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
        ("GIT_TERMINAL_PROMPT".into(), "0".into()),
        ("GIT_NO_LAZY_FETCH".into(), "1".into()),
        ("GIT_NO_REPLACE_OBJECTS".into(), "1".into()),
        ("GIT_OPTIONAL_LOCKS".into(), "0".into()),
        ("GIT_AUTHOR_NAME".into(), "verifier".into()),
        ("GIT_AUTHOR_EMAIL".into(), "verifier@example.com".into()),
        ("GIT_COMMITTER_NAME".into(), "verifier".into()),
        ("GIT_COMMITTER_EMAIL".into(), "verifier@example.com".into()),
    ]
}

/// Compare complete trees without rename folding so both old and new names of
/// a moved file must be authorized. NUL framing preserves arbitrary Git paths.
pub(super) fn changed_paths(checkout: &Path, base: &str, candidate: &str) -> Result<Vec<Vec<u8>>> {
    let output = run(checkout, &[
        "diff-tree".into(), "--no-commit-id".into(), "--name-only".into(),
        "--no-renames".into(), "--no-ext-diff".into(), "--no-textconv".into(),
        "-r".into(), "-z".into(), base.into(), candidate.into(), "--".into(),
    ], Some(checkout))?;
    if !output.success() { bail!("candidate scope diff is unavailable or exceeds capture bounds"); }
    let bytes = output.stdout_bytes;
    if !bytes.is_empty() && bytes.last() != Some(&0) { bail!("candidate scope diff has invalid framing"); }
    let paths: Vec<_> = bytes.split(|byte| *byte == 0).filter(|path| !path.is_empty()).map(Vec::from).collect();
    if paths.len() > 10_000 { bail!("candidate scope diff exceeds 10000 files"); }
    Ok(paths)
}

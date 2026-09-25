//! Fresh checkout from retained objects. Hooks are not installed and git cannot use the network.
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
) -> Result<Checkout> {
    let work = work.canonicalize().context("verification work directory")?;
    let store_dir = store_file
        .parent()
        .context("project store directory")?
        .canonicalize()
        .context("project store directory")?;
    if work.starts_with(&store_dir) {
        bail!("verification checkout must stay outside the project store");
    }
    let path = work.join("checkout");
    let template = work.join("template");
    fs::create_dir_all(&template).context("git template")?;
    if path.exists() {
        fs::remove_dir_all(&path).context("reset checkout")?;
    }
    git(
        &work,
        &[
            "init".into(),
            "--template".into(),
            template.display().to_string(),
            path.display().to_string(),
        ],
        None,
    )?;
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

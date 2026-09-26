//! Libraries the verifier child may bind. Parsed from `ldd`, never a recursive `/usr` mount.
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::runner::{Cmd, RealRunner, Runner};

pub fn git_libraries(git: &Path) -> Result<Vec<PathBuf>> {
    let mut command =
        Cmd::new("/usr/bin/ldd", Duration::from_secs(5)).arg(git.display().to_string());
    command.env_clear = true;
    command.env = vec![
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("LANG".into(), "C".into()),
        ("LC_ALL".into(), "C".into()),
    ];
    let output = RealRunner.run(&command).context("ldd failed")?;
    if !output.success() {
        bail!("ldd failed");
    }
    let libraries = parse_ldd(&output.stdout);
    if libraries.is_empty() || output.stdout.contains("not found") {
        bail!("git library list is incomplete");
    }
    Ok(libraries)
}

pub fn parse_ldd(text: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.contains("linux-vdso") || line.contains("linux-gate") {
            continue;
        }
        if let Some((left, right)) = line.split_once("=>") {
            push_abs(&mut paths, right.split_whitespace().next().unwrap_or(""));
            push_abs(&mut paths, left.trim());
        } else {
            push_abs(&mut paths, line.split_whitespace().next().unwrap_or(""));
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

fn push_abs(paths: &mut Vec<PathBuf>, text: &str) {
    if text.starts_with('/') {
        paths.push(PathBuf::from(text));
    }
}

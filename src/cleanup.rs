//! Cooperative local cleanup checkpoint. No force removal and no process killing.
use std::{fs::{self, File, OpenOptions}, os::unix::fs::OpenOptionsExt, path::Path};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use crate::{paths::Ctx, project::Project, runner::Cmd, thread::{self, Thread}};

/// Excludes supported launch/prompt/adopt/ticker operations within this root.
/// It is deliberately separate from the short project-record transaction lock.
pub struct Lease(File);
impl Drop for Lease { fn drop(&mut self) { let _ = self.0.unlock(); } }
pub fn lease(root: &Path) -> Result<Lease> {
    let file = OpenOptions::new().write(true).create(true).truncate(false).mode(0o600)
        .custom_flags(libc::O_NOFOLLOW).open(root.join(".execution.lock"))?;
    file.try_lock().context("another lifecycle or ticker operation is active; retry after it finishes")?;
    Ok(Lease(file))
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Removal {
    pub operation: String,
    pub repo: String,
    pub path: String,
    pub branch: String,
    pub head: String,
    pub snapshot: String,
    pub generation: u64,
    pub removed: bool,
}
fn git(ctx: &Ctx, repo: &str, args: &[&str]) -> Result<String> {
    let output = ctx.runner.run(&Cmd::new("git", std::time::Duration::from_secs(15)).args(["-C", repo]).args(args.iter().copied()))?;
    ensure!(output.success(), "git {}: {}", args.join(" "), output.error_text());
    Ok(output.stdout.trim_end_matches('\n').into())
}
fn registered(ctx: &Ctx, record: &Thread, path: &str, head: &str) -> Result<bool> {
    let list = git(ctx, &record.repo, &["worktree", "list", "--porcelain", "-z"])?;
    let wanted = format!("worktree {path}");
    let branch = format!("branch refs/heads/{}", record.branch);
    let head = format!("HEAD {head}");
    for block in list.split("\0\0") {
        let fields: Vec<_> = block.split('\0').collect();
        if fields.contains(&wanted.as_str()) {
            ensure!(fields.contains(&branch.as_str()) && fields.contains(&head.as_str()), "registered worktree branch or commit changed");
            ensure!(!fields.iter().any(|f| f.starts_with("locked") || f.starts_with("prunable")), "worktree is locked or prunable");
            return Ok(true);
        }
    }
    Ok(false)
}
#[cfg(target_os = "linux")]
fn mapped_path(value: &str) -> std::path::PathBuf {
    use std::os::unix::ffi::OsStringExt;
    let bytes = value.as_bytes();
    let mut decoded = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() && bytes[i+1..i+4].iter().all(|b| (b'0'..=b'7').contains(b)) {
            decoded.push(((bytes[i+1] - b'0') as u16 * 64 + (bytes[i+2] - b'0') as u16 * 8 + (bytes[i+3] - b'0') as u16) as u8);
            i += 4;
        } else { decoded.push(bytes[i]); i += 1; }
    }
    std::ffi::OsString::from_vec(decoded).into()
}
#[cfg(target_os = "linux")]
pub fn no_process_references(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    // A checkpoint observes all visible same-user processes, not merely Herdr's
    // idle indicator. The operator separately confirms other known writers stopped.
    // SAFETY: geteuid takes no pointers and has no preconditions.
    let uid = unsafe { libc::geteuid() };
    for process in fs::read_dir("/proc")? {
        let process = process?;
        if !process.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()) { continue; }
        let proc = process.path();
        let meta = match fs::metadata(&proc) { Ok(m) => m, Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue, Err(e) => return Err(e.into()) };
        if meta.uid() != uid { continue; }
        let check = || -> Result<()> {
            let cwd = fs::read_link(proc.join("cwd"))?;
            ensure!(!cwd.starts_with(path), "process {} still has its cwd in the worktree", process.file_name().to_string_lossy());
            for fd in fs::read_dir(proc.join("fd"))? {
                match fs::read_link(fd?.path()) {
                    Ok(target) => ensure!(!target.starts_with(path), "process {} still holds a worktree descriptor", process.file_name().to_string_lossy()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
                    Err(e) => return Err(e.into()),
                }
            }
            let maps = fs::read_to_string(proc.join("maps"))?;
            ensure!(!maps.lines().any(|line| line.find('/').is_some_and(|start| mapped_path(&line[start..]).starts_with(path))), "process still maps worktree content");
            Ok(())
        };
        match check() {
            Ok(()) => {},
            Err(error) if error.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) => {
                let gone = !proc.try_exists()?;
                let zombie = fs::read_to_string(proc.join("status")).is_ok_and(|s| s.lines().any(|l| l.starts_with("State:") && l.contains("Z (zombie)")))
                    && fs::read_dir(proc.join("task")).is_ok_and(|tasks| tasks.count() == 1);
                ensure!(gone || zombie, "process identity changed during writer inspection; retry the checkpoint");
            },
            Err(error) => return Err(error).context("cannot establish writer quiescence"),
        }
    }
    Ok(())
}
#[cfg(not(target_os = "linux"))]
pub fn no_process_references(_: &Path) -> Result<()> { anyhow::bail!("writer quiescence inspection is currently supported on Linux only") }

/// Caller holds the lifecycle lease and has checked all project/pane ownership.
pub fn remove(ctx: &Ctx, project: &Project, record: &Thread, writers_stopped: bool) -> Result<()> {
    if let crate::ticker::LockState::Held(info) = crate::ticker::lock_state(&ctx.root) {
        ensure!(info.version == crate::VERSION, "running ticker uses a different checkpoint protocol build; stop or restart it before cleanup");
    }
    ensure!(writers_stopped, "writer quiescence requires --writers-stopped after stopping all known artifact writers; keeping the worktree");
    ensure!(!record.is_remote(), "remote cleanup has no writer checkpoint adapter; keeping the worktree");
    let repo = fs::canonicalize(&record.repo)?.to_str().context("non-UTF-8 repository")?.to_string();
    let path = fs::canonicalize(&record.worktree_path)?.to_str().context("non-UTF-8 worktree")?.to_string();
    ensure!(path != repo && !record.branch.is_empty(), "invalid worktree identity");
    let head = git(ctx, &repo, &["rev-parse", "--verify", &format!("refs/heads/{}", record.branch)])?;
    ensure!(registered(ctx, record, &path, &head)?, "worktree is not registered to this repository");
    ensure!(git(ctx, &path, &["rev-parse", "--show-toplevel"])? == path, "worktree root changed");
    no_process_references(Path::new(&path))?;
    let manifest = crate::artifacts::load(project, record, &record.artifact_snapshot)?;
    crate::artifacts::verify_source(record, &manifest)?;
    let removal = Removal { operation: thread::sha256_hex(format!("{}:{}:{}:{}", record.id, record.lifecycle_generation, path, head).as_bytes()), repo, path, branch: record.branch.clone(), head, snapshot: record.artifact_snapshot.clone(), generation: record.lifecycle_generation, removed: false };
    thread::update_checked(project, &record.id, |current| {
        ensure!(thread::execution_fingerprint(current) == thread::execution_fingerprint(record), "thread changed before removal reservation");
        current.removal = Some(removal.clone());
        Ok(())
    })?;
    File::open(project.dir().join("threads"))?.sync_all()?;
    ensure!(project.status() == crate::project::Status::Active, "project became inactive before removal");
    no_process_references(Path::new(&removal.path))?;
    crate::artifacts::verify_source(record, &manifest)?;
    ensure!(registered(ctx, record, &removal.path, &removal.head)?, "worktree registration changed");
    // Git enforces dirty/untracked/submodule protections. Never retry with force.
    git(ctx, &removal.repo, &["worktree", "remove", "--", &removal.path])?;
    thread::update_checked(project, &record.id, |current| {
        ensure!(current.removal.as_ref() == Some(&removal), "removal reservation changed; reconcile before retrying");
        current.removal.as_mut().unwrap().removed = true;
        Ok(())
    })?;
    Ok(())
}

/// A durable reservation also covers a crash after Git removed the worktree but
/// before its acknowledgement was saved. Existing/mismatched registrations block.
pub fn restore(ctx: &Ctx, project: &Project, record: &Thread) -> Result<()> {
    let removal = record.removal.as_ref().context("no intentional removal record; inspect the incomplete creation manually")?;
    ensure!(!record.is_remote() && record.branch == removal.branch, "retained branch identity changed");
    ensure!(fs::canonicalize(&record.repo)? == Path::new(&removal.repo), "repository identity changed");
    ensure!(git(ctx, &record.repo, &["rev-parse", "--verify", &format!("refs/heads/{}", removal.branch)])? == removal.head, "retained branch advanced; inspect before reopening");
    crate::artifacts::load(project, record, &removal.snapshot)?;
    for slug in crate::project::list_slugs(&ctx.root) {
        let owner = Project::load(&ctx.root, &slug)?;
        let (records, diagnostics) = thread::list_with_diagnostics(&owner);
        ensure!(diagnostics.is_empty(), "cannot establish reopen ownership: {}", diagnostics.join("; "));
        for other in records {
            if other.is_remote() || (owner.canonical_dir() == project.canonical_dir() && other.id == record.id) { continue; }
            for location in [&other.worktree_path, &other.cwd] {
                if location.is_empty() { continue; }
                let location = fs::canonicalize(location).unwrap_or_else(|_| location.into());
                ensure!(!location.starts_with(&removal.path) && !Path::new(&removal.path).starts_with(&location), "reopen worktree is referenced by {} in {slug}", other.id);
            }
        }
    }
    let exists = match fs::symlink_metadata(&removal.path) {
        Ok(meta) => { ensure!(meta.is_dir(), "reopen path is not a real directory"); true },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    let parent = Path::new(&removal.path).parent().context("reopen path has no parent")?;
    ensure!(fs::canonicalize(parent)? == parent, "reopen parent identity changed");
    if exists {
        // Idempotent retry after worktree add succeeded and acknowledgement failed.
        ensure!(fs::symlink_metadata(&removal.path)?.is_dir() && fs::canonicalize(&removal.path)? == Path::new(&removal.path), "reopen path was replaced");
        ensure!(registered(ctx, record, &removal.path, &removal.head)?, "reopen path already exists without matching registration");
    } else {
        ensure!(!registered(ctx, record, &removal.path, &removal.head)?, "absent path is still registered; reconcile Git metadata first");
        git(ctx, &record.repo, &["worktree", "add", "--", &removal.path, &removal.branch])?;
    }
    thread::update_checked(project, &record.id, |current| {
        ensure!(current.removal.as_ref() == Some(removal), "removal record changed during reopen");
        current.worktree_path = removal.path.clone();
        current.cwd = removal.path.clone();
        current.removal = None;
        Ok(())
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{paths::Env, project, runner::RealRunner, thread::{Kind, Status}};
    fn fixture() -> (tempfile::TempDir, Project, Env, Thread) {
        let root = tempfile::tempdir().unwrap();
        let project = project::create(root.path(), "demo", "", vec![]).unwrap();
        let env = Env::for_test(root.path(), &[]);
        let repo = root.path().join("repo");
        fs::create_dir(&repo).unwrap();
        let ctx = Ctx { root: root.path().into(), config_dir: root.path().join("cfg"), env: &env, runner: &RealRunner, detached_ticker: false };
        let r = repo.to_str().unwrap();
        git(&ctx, r, &["init", "--quiet"]).unwrap();
        git(&ctx, r, &["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--quiet", "--allow-empty", "-m", "base"]).unwrap();
        let work = root.path().join("worktree");
        git(&ctx, r, &["worktree", "add", "-b", "retained", work.to_str().unwrap()]).unwrap();
        let source = work.join(".herdr-project");
        fs::write(repo.join(".git/info/exclude"), b".herdr-project/\n").unwrap();
        fs::create_dir(&source).unwrap();
        fs::write(source.join("report.md"), b"retained report").unwrap();
        let record = thread::allocate(&project, |t| {
            t.kind = Kind::Worktree; t.status = Status::Open; t.repo = r.into(); t.branch = "retained".into();
            t.worktree_path = work.to_str().unwrap().into(); t.thread_dir = source.to_str().unwrap().into();
        }).unwrap();
        let snapshot = crate::artifacts::capture_local(&project, &record).unwrap();
        let record = thread::update(&project, &record.id, |t| t.artifact_snapshot = snapshot.id).unwrap();
        (root, project, env, record)
    }
    #[test]
    fn lease_excludes_concurrent_lifecycle_and_releases_explicitly() {
        let root = tempfile::tempdir().unwrap();
        let held = lease(root.path()).unwrap();
        assert!(lease(root.path()).is_err());
        drop(held);
        assert!(lease(root.path()).is_ok());
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn retained_branch_reopens_and_git_refusal_keeps_source() {
        let (root, project, env, record) = fixture();
        let ctx = Ctx { root: root.path().into(), config_dir: root.path().join("cfg"), env: &env, runner: &RealRunner, detached_ticker: false };
        let _lease = lease(root.path()).unwrap();
        fs::write(Path::new(&record.worktree_path).join("untracked"), b"do not lose").unwrap();
        assert!(remove(&ctx, &project, &record, true).is_err());
        assert!(Path::new(&record.worktree_path).join("untracked").exists());
        fs::remove_file(Path::new(&record.worktree_path).join("untracked")).unwrap();
        remove(&ctx, &project, &record, true).unwrap();
        assert!(!Path::new(&record.worktree_path).exists());
        crate::artifacts::load(&project, &record, &record.artifact_snapshot).unwrap();
        let removed = thread::load(&project, &record.id).unwrap();
        assert!(removed.removal.as_ref().unwrap().removed);
        // Simulate lost acknowledgement after Git removal.
        let ambiguous = thread::update(&project, &record.id, |t| t.removal.as_mut().unwrap().removed = false).unwrap();
        restore(&ctx, &project, &ambiguous).unwrap();
        assert!(Path::new(&record.worktree_path).exists());
        assert_eq!(git(&ctx, &record.repo, &["rev-parse", "retained"]).unwrap(), removed.removal.unwrap().head);
        assert!(thread::load(&project, &record.id).unwrap().removal.is_none());
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn reopen_refuses_changed_branch_replaced_path_and_new_owner() {
        for change in ["branch", "path", "owner"] {
            let (root, project, env, record) = fixture();
            let ctx = Ctx { root: root.path().into(), config_dir: root.path().join("cfg"), env: &env, runner: &RealRunner, detached_ticker: false };
            let _lease = lease(root.path()).unwrap();
            remove(&ctx, &project, &record, true).unwrap();
            let removed = thread::load(&project, &record.id).unwrap();
            match change {
                "branch" => {
                    git(&ctx, &record.repo, &["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--quiet", "--allow-empty", "-m", "advance"]).unwrap();
                    git(&ctx, &record.repo, &["update-ref", "refs/heads/retained", "HEAD"]).unwrap();
                }
                "path" => std::os::unix::fs::symlink(root.path().join("missing"), &record.worktree_path).unwrap(),
                _ => { thread::allocate(&project, |t| { t.kind = Kind::Adopted; t.cwd = record.worktree_path.clone(); }).unwrap(); }
            }
            assert!(restore(&ctx, &project, &removed).is_err(), "{change}");
            assert!(thread::load(&project, &record.id).unwrap().removal.is_some());
            assert!(!Path::new(&record.worktree_path).is_dir());
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn open_descriptor_blocks_writer_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let _writer = File::create(dir.path().join("still-writing")).unwrap();
        assert!(no_process_references(dir.path()).is_err());
    }
}

//! Controller import of a worker's Git quarantine.
//!
//! A sandboxed worker sees each repository's Git common directory through an
//! overlay whose only writable layer is its attempt worktree's quarantine
//! (`worker_supervision::git_quarantine`), so its commits, ref moves and any
//! other Git writes never reach the shared repository. The controller imports
//! from `<quarantine>/upper` exactly one thing: the closure of one commit (the
//! attempt branch tip after the worker ended, or a verified candidate before
//! integration). Every object is re-hashed and fsck-checked before any byte
//! reaches the shared store, and only the attempt's own branch ever moves,
//! by fast-forward compare-and-swap:
//!
//! 1. copy regular loose objects and packs (never a link, device or FIFO,
//!    bounded) into a private bare repository `T` whose alternate is the
//!    shared object store, indexing each copied pack afresh (its object names
//!    are recomputed from content; the worker's `.idx` is ignored);
//! 2. fetch the commit from `T` into a second private bare repository `V`
//!    with `fetch.fsckObjects`: the receiving side names every object by the
//!    hash of its content, runs fsck and checks connectivity, so a swapped,
//!    corrupt or missing object refuses the import;
//! 3. require the commit to descend from the attempt's approved base;
//! 4. fetch the verified closure from `V` into the shared repository as loose
//!    objects (Git never overwrites an object it already has) and, for the
//!    branch, `update-ref` from the current tip only if it is an ancestor.
//!
//! Objects in the quarantine that the imported commit does not reach are
//! never imported. Everything runs through the supervised, bounded Git runner
//! with local transports only.
use crate::{domain::*, runner::Cancellation, worktree_preparation::Git};
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Instant,
};

/// Most object bytes and files copied out of one quarantine.
const BYTE_LIMIT: u64 = 1024 * 1024 * 1024;
const FILE_LIMIT: usize = 100_000;
/// Loose objects below this count; the shared repository keeps the layout a
/// direct worker commit had.
const UNPACK_LIMIT: &str = "fetch.unpackLimit=1000000";
/// Local fetches only: no network transport even if the owner's config
/// rewrites the path.
const LOCAL_ONLY: [&str; 4] = ["-c", "protocol.allow=never", "-c", "protocol.file.allow=always"];

/// What an import did. `Refused` is a verdict on the quarantine's content and
/// never changes the shared repository.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Outcome {
    /// Nothing to import: no quarantine, or its branch did not move.
    Unchanged,
    Imported { commit: String },
    Refused { reason: String },
}

#[derive(Clone, Copy)]
enum Target<'a> {
    Branch,
    Commit(&'a str),
}

fn refused(reason: impl Into<String>) -> Result<Outcome> {
    Ok(Outcome::Refused { reason: reason.into() })
}

fn live(git: &Git) -> bool {
    !git.cancellation.is_cancelled() && Instant::now() < git.deadline
}

fn is_oid(value: &str, len: usize) -> bool {
    value.len() == len && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A regular file of at most `limit` bytes, opened without following links
/// or blocking on a FIFO. The worker shapes its quarantine, so anything else
/// (absent, a link, device or whiteout, unreadable, oversized) is `None`: an
/// object skipped this way is missing from the copy and refuses the import.
fn read_regular(path: &Path, limit: u64) -> Result<Option<Vec<u8>>> {
    let Ok(file) = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path) else { return Ok(None) };
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > limit {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    Ok((bytes.len() as u64 <= limit).then_some(bytes))
}

/// The attempt branch as the worker left it in its quarantine: its loose ref
/// in `upper`, else its line in a packed-refs file the worker rewrote.
/// `None` means the branch is as the shared repository has it (or deleted).
fn quarantined_tip(upper: &Path, branch: &str, len: usize) -> Result<std::result::Result<Option<String>, String>> {
    let name = format!("refs/heads/{branch}");
    if let Some(bytes) = read_regular(&upper.join(&name), 4096)? {
        let text = String::from_utf8(bytes).unwrap_or_default();
        let text = text.trim_end_matches('\n');
        return Ok(if is_oid(text, len) { Ok(Some(text.to_owned())) } else { Err(format!("attempt branch {name} is not a commit id")) });
    }
    let Some(packed) = read_regular(&upper.join("packed-refs"), 64 * 1024 * 1024)? else { return Ok(Ok(None)) };
    let Ok(packed) = String::from_utf8(packed) else { return Ok(Err("quarantined packed-refs is not UTF-8".into())) };
    for line in packed.lines() {
        if let Some((oid, reference)) = line.split_once(' ')
            && reference == name
        {
            return Ok(if is_oid(oid, len) { Ok(Some(oid.to_owned())) } else { Err(format!("attempt branch {name} is not a commit id")) });
        }
    }
    Ok(Ok(None))
}

struct Copy { bytes: u64, files: usize, packs: Vec<PathBuf> }
impl Copy {
    fn file(&mut self, from: &Path, to: &Path) -> Result<std::result::Result<bool, String>> {
        let Some(bytes) = read_regular(from, BYTE_LIMIT)? else { return Ok(Ok(false)) };
        self.files += 1;
        self.bytes += bytes.len() as u64;
        if self.files > FILE_LIMIT || self.bytes > BYTE_LIMIT {
            return Ok(Err(format!("quarantine exceeds {FILE_LIMIT} files or {BYTE_LIMIT} bytes")));
        }
        let mut file = OpenOptions::new().write(true).create_new(true).mode(0o444).custom_flags(libc::O_NOFOLLOW).open(to)?;
        file.write_all(&bytes)?;
        Ok(Ok(true))
    }
}
/// Names in a quarantine directory, without following it if it is a link;
/// none if it cannot be listed (the worker may have made it unreadable).
fn names(dir: &Path) -> Vec<String> {
    match fs::symlink_metadata(dir) {
        Ok(meta) if meta.is_dir() => {}
        _ => return vec![],
    }
    let mut names: Vec<String> = fs::read_dir(dir).into_iter().flatten().flatten()
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned)).collect();
    names.sort();
    names
}
/// Copy the quarantine's loose objects and packs into `objects`.
fn copy_objects(upper: &Path, objects: &Path, len: usize) -> Result<std::result::Result<Vec<PathBuf>, String>> {
    let hex = |s: &str| s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    let source = upper.join("objects");
    let mut copy = Copy { bytes: 0, files: 0, packs: vec![] };
    for prefix in names(&source) {
        if prefix.len() == 2 && hex(&prefix) {
            let dir = source.join(&prefix);
            let mut created = false;
            for name in names(&dir) {
                if name.len() != len - 2 || !hex(&name) {
                    continue;
                }
                if !created {
                    fs::create_dir_all(objects.join(&prefix))?;
                    created = true;
                }
                if let Err(reason) = copy.file(&dir.join(&name), &objects.join(&prefix).join(&name))? {
                    return Ok(Err(reason));
                }
            }
        }
    }
    for name in names(&source.join("pack")) {
        if name.ends_with(".pack") {
            let to = objects.join("pack").join(format!("pack-quarantine-{}.pack", copy.packs.len()));
            match copy.file(&source.join("pack").join(&name), &to)? {
                Err(reason) => return Ok(Err(reason)),
                Ok(true) => copy.packs.push(to),
                Ok(false) => {}
            }
        }
    }
    Ok(Ok(copy.packs))
}

fn remove_tree(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(meta) if !meta.is_dir() => Ok(fs::remove_file(path)?),
        Ok(_) => {
            // Git writes read-only object files and directories.
            for entry in fs::read_dir(path)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o700))?;
                }
                remove_tree(&entry.path())?;
            }
            Ok(fs::remove_dir(path)?)
        }
    }
}

/// A private bare repository borrowing the shared object store.
fn private_repository(git: &Git, scratch: &Path, name: &str, format: &str, shared_objects: &Path) -> Result<PathBuf> {
    let path = scratch.join(name);
    let text = path.to_str().context("quarantine path is not UTF-8")?;
    git.run(scratch, &["init", "--quiet", "--bare", "--template=", &format!("--object-format={format}"), text])?;
    fs::write(path.join("objects/info/alternates"), format!("{}\n", shared_objects.display()))?;
    Ok(path)
}

/// Import `target` from the quarantine of `receipt`'s worktree. `Err` is an
/// operational failure (deadline, lock, I/O, the shared repository refusing a
/// verified fetch) and is retried; content problems return `Refused`.
fn import(git: &Git, project: &Path, receipt: &WorktreeReceipt, target: Target) -> Result<Outcome> {
    let plan = &receipt.plan;
    let Some(quarantine) = crate::worker_supervision::git_quarantine(project, Path::new(&plan.path)) else {
        bail!("worktree {} has no Git quarantine", plan.path);
    };
    match fs::symlink_metadata(&quarantine) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Outcome::Unchanged),
        Err(error) => return Err(error.into()),
        Ok(meta) => ensure!(meta.is_dir() && meta.uid() == unsafe { libc::geteuid() }, "Git quarantine {} is not the controller's directory", quarantine.display()),
    }
    let lock = crate::execution_guard::exclusive_file(&quarantine.join("import.lock"))?;
    let git = Git { deadline: git.deadline, cancellation: git.cancellation.clone(), locks: vec![crate::runner::InheritedLock::new(lock.transfer()?)] };
    let repository = Path::new(&plan.source.repository);
    let format = git.run(repository, &["rev-parse", "--show-object-format"])?;
    let len = match format.as_str() { "sha1" => 40, "sha256" => 64, _ => bail!("unsupported Git object format") };
    let upper = quarantine.join("upper");
    let branch = format!("refs/heads/{}", plan.branch);
    let tip = match target {
        Target::Commit(oid) => {
            ensure!(is_oid(oid, len), "invalid commit id");
            oid.to_owned()
        }
        Target::Branch => {
            if fs::symlink_metadata(upper.join("reftable")).is_ok() {
                return refused("the quarantine holds a reftable ref store; only the files backend can be imported");
            }
            match quarantined_tip(&upper, &plan.branch, len)? {
                Err(reason) => return refused(reason),
                Ok(None) => return Ok(Outcome::Unchanged),
                Ok(Some(tip)) => tip,
            }
        }
    };
    let current = match target {
        Target::Branch => match git.run(repository, &["rev-parse", "--verify", "--end-of-options", &branch]) {
            Ok(current) => Some(current),
            Err(_) if live(&git) => return refused(format!("attempt branch {branch} is missing from the shared repository")),
            Err(error) => return Err(error),
        },
        Target::Commit(_) => None,
    };
    if current.as_deref() == Some(tip.as_str()) {
        return Ok(Outcome::Unchanged);
    }
    let base = &plan.source.commit;
    let present = git.run(repository, &["cat-file", "-e", &format!("{tip}^{{commit}}")]).is_ok();
    if !present
        && let verdict @ Outcome::Refused { .. } = verify_and_fetch(&git, &quarantine, &upper, repository, &receipt.common_directory, &format, len, &tip, base)?
    {
        return Ok(verdict);
    }
    if git.run(repository, &["merge-base", "--is-ancestor", base, &tip]).is_err() {
        ensure!(live(&git), "Git quarantine import cancelled or deadline expired");
        return refused(format!("commit {tip} does not descend from the attempt's base {base}"));
    }
    if let Some(current) = current {
        if git.run(repository, &["merge-base", "--is-ancestor", &current, &tip]).is_err() {
            ensure!(live(&git), "Git quarantine import cancelled or deadline expired");
            return refused(format!("attempt branch moved to {current} outside the worker; {tip} does not extend it"));
        }
        git.run(repository, &["update-ref", "-m", "herdr-projects: import attempt branch from its Git quarantine", &branch, &tip, &current])?;
    }
    Ok(Outcome::Imported { commit: tip })
}

#[allow(clippy::too_many_arguments)]
fn verify_and_fetch(git: &Git, quarantine: &Path, upper: &Path, repository: &Path, common: &str, format: &str, len: usize, tip: &str, base: &str) -> Result<Outcome> {
    let scratch = quarantine.join("import");
    remove_tree(&scratch)?;
    fs::create_dir(&scratch)?;
    fs::set_permissions(&scratch, fs::Permissions::from_mode(0o700))?;
    let result = (|| -> Result<Outcome> {
        let shared = Path::new(common).join("objects");
        let copied = private_repository(git, &scratch, "copied", format, &shared)?;
        let packs = match copy_objects(upper, &copied.join("objects"), len)? {
            Err(reason) => return refused(reason),
            Ok(packs) => packs,
        };
        // Content checks: a failure is a verdict unless the budget ran out.
        let check = |args: &[&str], reason: &str| -> Result<Option<Outcome>> {
            match git.run(&scratch, args) {
                Ok(_) => Ok(None),
                Err(_) if live(git) => Ok(Some(Outcome::Refused { reason: reason.into() })),
                Err(error) => Err(error),
            }
        };
        let copied_dir = format!("--git-dir={}", copied.display());
        for pack in &packs {
            let pack = pack.to_str().context("quarantine path is not UTF-8")?;
            if let Some(verdict) = check(&[&copied_dir, "index-pack", pack], "a quarantined pack is corrupt")? {
                return Ok(verdict);
            }
        }
        if let Some(verdict) = check(&[&copied_dir, "update-ref", "refs/heads/import", tip], &format!("commit {tip} is missing from the quarantine"))? {
            return Ok(verdict);
        }
        let verified = private_repository(git, &scratch, "verified", format, &shared)?;
        let verified_dir = format!("--git-dir={}", verified.display());
        let copied_path = copied.to_str().context("quarantine path is not UTF-8")?;
        let mut fetch = vec![verified_dir.as_str()];
        fetch.extend(LOCAL_ONLY);
        fetch.extend(["-c", "fetch.fsckObjects=true", "-c", "fetch.fsck.hasDotgit=error", "fetch", "--quiet", "--no-tags",
            "--no-write-fetch-head", "--no-recurse-submodules", copied_path, "+refs/heads/import:refs/heads/import"]);
        if let Some(verdict) = check(&fetch, &format!("objects reachable from {tip} fail re-hashing, fsck or connectivity"))? {
            return Ok(verdict);
        }
        if let Some(verdict) = check(&[&verified_dir, "merge-base", "--is-ancestor", base, tip], &format!("commit {tip} does not descend from the attempt's base {base}"))? {
            return Ok(verdict);
        }
        // Verified: only now does anything reach the shared repository.
        let verified_path = verified.to_str().context("quarantine path is not UTF-8")?;
        let mut fetch = LOCAL_ONLY.to_vec();
        fetch.extend(["-c", UNPACK_LIMIT, "fetch", "--quiet", "--no-tags", "--no-write-fetch-head", "--no-recurse-submodules", verified_path, "refs/heads/import"]);
        git.run(repository, &fetch)?;
        git.run(repository, &["cat-file", "-e", &format!("{tip}^{{commit}}")])?;
        Ok(Outcome::Imported { commit: tip.to_owned() })
    })();
    let cleanup = remove_tree(&scratch);
    let outcome = result?;
    cleanup?;
    Ok(outcome)
}

/// Record the latest branch import beside the quarantine for the owner.
fn record(quarantine: &Path, outcome: &Outcome) -> Result<()> {
    let temporary = quarantine.join("import.json.tmp");
    let _ = fs::remove_file(&temporary);
    let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&temporary)?;
    file.write_all(&serde_json::to_vec(outcome)?)?;
    file.sync_all()?;
    fs::rename(&temporary, quarantine.join("import.json"))?;
    File::open(quarantine)?.sync_all()?;
    Ok(())
}

/// After proven worker termination: import each worktree's attempt branch.
/// Each outcome is recorded in `<quarantine>/import.json`; after a refusal the
/// shared repository and branch stay as they were.
pub(crate) fn import_terminated(project: &Path, events: &[Event], attempt: &AttemptInputRecord, deadline: Instant, cancellation: Cancellation) -> Result<()> {
    let Some(ready) = events.iter().find(|e| e.kind == "runtime.worktrees_ready" && e.entity == attempt.operation.as_str()) else { return Ok(()) };
    let receipts: Vec<WorktreeReceipt> = serde_json::from_value(ready.payload.clone())?;
    for receipt in &receipts {
        let Some(quarantine) = crate::worker_supervision::git_quarantine(project, Path::new(&receipt.plan.path)) else { continue };
        if fs::symlink_metadata(&quarantine).is_err() {
            continue;
        }
        let git = Git { deadline, cancellation: cancellation.clone(), locks: vec![] };
        let outcome = import(&git, project, receipt, Target::Branch)?;
        record(&quarantine, &outcome)?;
        // The overlay is gone with the worker's namespace; its work directory
        // (which the kernel leaves unreadable) has no further use.
        remove_tree(&quarantine.join("work"))?;
    }
    Ok(())
}

/// Before integration: make `commit` (a verified candidate of an attempt whose
/// worktrees are `receipts`) available in `repository`, importing it from the
/// matching worktree's quarantine if the shared store lacks it. The attempt
/// branch is not moved; that happens after the worker ends.
pub(crate) fn import_commit(project: &Path, receipts: &[WorktreeReceipt], repository: &str, commit: &str, deadline: Instant) -> Result<()> {
    let receipt = receipts.iter().find(|r| r.plan.source.repository == repository)
        .context("the candidate's attempt has no worktree of this repository")?;
    let git = Git { deadline, cancellation: Cancellation::default(), locks: vec![] };
    match import(&git, project, receipt, Target::Commit(commit))? {
        Outcome::Imported { .. } => Ok(()),
        Outcome::Unchanged => bail!("candidate {commit} is not in the repository and its attempt left no Git quarantine"),
        Outcome::Refused { reason } => bail!("candidate {commit} was refused from its attempt's Git quarantine: {reason}"),
    }
}

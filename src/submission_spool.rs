//! The worker submission spool (docs/reviews/2026-09-29-worker-isolation.md,
//! "Submission spool"). An isolated worker sees its project's `.state`
//! read-only, so it cannot write `state.db`, `factory-objects/` or another
//! attempt's files. Its only writable places there are its output directory
//! and `.state/spool/<attempt>/`, bound writable to that attempt alone.
//!
//! Worker side: `result submit` and the review worker channel (`review
//! submit`, `review session`, `review present`) run with
//! [`ENV`] set write one canonical request `<sha256>.request` (a temporary
//! created `O_CREAT|O_EXCL|O_NOFOLLOW`, mode 0600, renamed into place) and
//! wait, bounded, for `<sha256>.receipt`, which carries the exact stdout or
//! error the command prints without a sandbox. The worker may forge its own
//! receipt; that changes only what it prints to itself.
//!
//! Ticker side ([`ingest`]): every pass, for each attempt directory, read
//! each request that has no receipt without following links (regular file,
//! one link, size-capped), parse it strictly (canonical bytes, name = digest,
//! `attempt_id` = the directory), check the attempt is live and that the
//! document is its own, then call the same store path the CLI calls
//! (`submit_result`'s idempotent key/digest replay, the review worker
//! channel). The receipt is written to an exclusive temporary and renamed
//! over the receipt name, so a link the worker planted there is replaced,
//! never followed. Refusals are recorded as `spool.request_denied` events.
//! A crash between the store commit and the receipt re-runs the request on
//! the next pass, which replays (`replayed: true`).
use crate::source_tree::{Budget, Directory};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    ffi::{CString, OsStr, OsString},
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::OpenOptionsExt,
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub use crate::worker_supervision::SUBMISSION_SPOOL_ENV as ENV;

/// A request carries at most one 256 KiB result document, JSON-escaped.
const REQUEST_LIMIT: u64 = 1024 * 1024;
const RECEIPT_LIMIT: u64 = 1024 * 1024;
const RESULT_LIMIT: usize = 256 * 1024;
/// Entries one attempt's spool may hold; beyond it the spool is refused.
const ENTRY_LIMIT: usize = 256;
/// Requests handled per attempt and pass.
const PASS_LIMIT: usize = 8;
/// Denials recorded in the store per attempt (later ones are only logged).
const DENIAL_RECORDS: u64 = 16;
/// How long the worker's command waits for the ticker's receipt.
const WAIT: Duration = Duration::from_secs(180);

/// The commands a worker runs through its spool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    ResultSubmit,
    ReviewSubmit,
    ReviewSession,
    ReviewPresent,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::ResultSubmit => "result_submit",
            Self::ReviewSubmit => "review_submit",
            Self::ReviewSession => "review_session",
            Self::ReviewPresent => "review_present",
        }
    }
}

/// The canonical request: its bytes are exactly `serde_json::to_vec` of
/// this value and its file name is their SHA-256.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u32,
    kind: Kind,
    attempt_id: String,
    /// The submitted document (result submission or review receipt), UTF-8.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    document: Option<String>,
    /// `review session`'s attempt or `review present`'s opportunity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    argument: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    version: u32,
    request_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stdout: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn valid_attempt(attempt: &str) -> bool {
    !attempt.is_empty()
        && attempt.len() <= 128
        && !attempt.starts_with('.')
        && attempt.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

/// The spool this process was started with, if it runs as an isolated worker.
pub fn worker_spool() -> Option<PathBuf> {
    std::env::var_os(ENV).map(PathBuf::from).filter(|path| path.is_absolute())
}

/// `result submit` from inside the sandbox: the document is bounded and
/// checked as the store checks it first, then exchanged through the spool.
pub fn submit_result(spool: &Path, document: &Path) -> Result<String> {
    let bytes = crate::migration::read_plan_file(document)
        .map_err(|error| crate::store::StoreError::Invalid(error.to_string()))?;
    if bytes.is_empty() || bytes.len() > RESULT_LIMIT {
        return Err(crate::store::StoreError::Invalid("result submission exceeds 256 KiB".into()).into());
    }
    exchange(spool, Kind::ResultSubmit, Some(bytes), None)
}

/// Write one request into `spool` and wait for its receipt: the stdout the
/// command prints, or its error.
pub fn exchange(spool: &Path, kind: Kind, document: Option<Vec<u8>>, argument: Option<String>) -> Result<String> {
    let attempt = spool
        .file_name()
        .and_then(OsStr::to_str)
        .filter(|a| valid_attempt(a))
        .context("the submission spool does not name an attempt")?
        .to_owned();
    let document = document
        .map(|bytes| {
            String::from_utf8(bytes).map_err(|_| match kind {
                Kind::ResultSubmit => anyhow::Error::from(crate::store::StoreError::Invalid("invalid result submission".into())),
                _ => anyhow!("invalid review receipt: not UTF-8"),
            })
        })
        .transpose()?;
    let bytes = serde_json::to_vec(&Request { version: 1, kind, attempt_id: attempt, document, argument })?;
    ensure!(bytes.len() as u64 <= REQUEST_LIMIT, "spooled request exceeds {REQUEST_LIMIT} bytes");
    let digest = sha256_hex(&bytes);
    let receipt = spool.join(format!("{digest}.receipt"));
    // A receipt of an earlier identical request is stale: this run asks again,
    // and the store's idempotent replay answers it.
    match std::fs::remove_file(&receipt) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("clear {}", receipt.display())),
    }
    let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let temporary = spool.join(format!(".{digest}.{}.{nonce}.tmp", std::process::id()));
    let written = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, spool.join(format!("{digest}.request")))?;
        Ok(())
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written.with_context(|| format!("write the request into the submission spool {}", spool.display()))?;
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(answer) = read_receipt(&receipt, &digest)? {
            return match answer {
                Receipt { stdout: Some(stdout), error: None, .. } => Ok(stdout),
                Receipt { stdout: None, error: Some(error), .. } => Err(anyhow!(error)),
                _ => bail!("invalid submission spool receipt {}", receipt.display()),
            };
        }
        ensure!(
            Instant::now() < deadline,
            "no receipt from the ticker within {} s for spooled request {digest}; it stays queued, and the same command waits again",
            WAIT.as_secs()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn read_receipt(path: &Path, digest: &str) -> Result<Option<Receipt>> {
    let file = match OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let metadata = file.metadata()?;
    ensure!(metadata.is_file() && metadata.len() <= RECEIPT_LIMIT, "{} is not a bounded regular file", path.display());
    let mut bytes = Vec::new();
    file.take(RECEIPT_LIMIT + 1).read_to_end(&mut bytes)?;
    let receipt: Receipt = serde_json::from_slice(&bytes).with_context(|| format!("invalid receipt {}", path.display()))?;
    ensure!(receipt.version == 1 && receipt.request_sha256 == digest, "receipt {} answers another request", path.display());
    Ok(Some(receipt))
}

/// Create the attempt's spool and output directory (owner-only) under the
/// project's `.state`, before the sandbox binds them writable. Existing real
/// directories are kept; a link or other entry in their place is refused.
pub fn prepare(project: &Path, attempt: &crate::domain::AttemptId) -> Result<()> {
    ensure!(valid_attempt(attempt.as_str()), "invalid attempt for the submission spool");
    let state = Directory::open(&project.join(".state"))?;
    for parent in ["spool", "worker-output"] {
        state.create_dir(OsStr::new(parent))?.create_dir(OsStr::new(attempt.as_str()))?;
    }
    Ok(())
}

/// A directory opened without following its last component; entries are
/// reached only relative to it, never by path.
struct Dir(File);

impl Dir {
    fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .with_context(|| format!("open {}", path.display()))?;
        Ok(Self(file))
    }
    fn name(name: &str) -> Result<CString> {
        ensure!(!name.is_empty() && !name.contains('/') && name != "." && name != "..", "invalid spool entry name");
        Ok(CString::new(name)?)
    }
    fn child(&self, name: &str) -> Result<Self> {
        let c = Self::name(name)?;
        let fd = unsafe { libc::openat(self.0.as_raw_fd(), c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error()).with_context(|| format!("open spool directory {name}"));
        }
        // SAFETY: a successful openat returns a new descriptor owned here.
        Ok(Self(unsafe { File::from_raw_fd(fd) }))
    }
    fn names(&self) -> Result<Vec<OsString>> {
        Directory::from_file(self.0.try_clone()?)?.names(&Budget::new())
    }
    /// The entry's file type bits, without following a link.
    fn kind(&self, name: &str) -> Result<Option<(libc::mode_t, u64)>> {
        let c = Self::name(name)?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstatat(self.0.as_raw_fd(), c.as_ptr(), stat.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) } < 0 {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::NotFound { Ok(None) } else { Err(error.into()) };
        }
        // SAFETY: fstatat succeeded and initialised `stat`.
        let stat = unsafe { stat.assume_init() };
        Ok(Some((stat.st_mode & libc::S_IFMT, stat.st_nlink)))
    }
    /// A regular single-link file of at most `limit` bytes.
    fn read(&self, name: &str, limit: u64) -> std::result::Result<Vec<u8>, String> {
        let c = Self::name(name).map_err(|e| e.to_string())?;
        let fd = unsafe { libc::openat(self.0.as_raw_fd(), c.as_ptr(), libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC) };
        if fd < 0 {
            let error = io::Error::last_os_error();
            return Err(if error.raw_os_error() == Some(libc::ELOOP) { "the request is a symbolic link".into() } else { format!("the request cannot be opened: {error}") });
        }
        // SAFETY: a successful openat returns a new descriptor owned here.
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata().map_err(|e| e.to_string())?;
        if !metadata.is_file() || std::os::unix::fs::MetadataExt::nlink(&metadata) != 1 {
            return Err("the request is not a regular single-link file".into());
        }
        if metadata.len() > limit {
            return Err(format!("the request exceeds {limit} bytes"));
        }
        let mut bytes = Vec::new();
        file.take(limit + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
        if bytes.len() as u64 > limit {
            return Err(format!("the request exceeds {limit} bytes"));
        }
        Ok(bytes)
    }
    fn unlink(&self, name: &str, directory: bool) -> Result<()> {
        let c = Self::name(name)?;
        if unsafe { libc::unlinkat(self.0.as_raw_fd(), c.as_ptr(), if directory { libc::AT_REMOVEDIR } else { 0 }) } < 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }
    /// Write `bytes` to an exclusive owner-only temporary and rename it over
    /// `name`: whatever entry the worker left there (a link included) is
    /// replaced, never followed or truncated.
    fn replace(&self, name: &str, bytes: &[u8]) -> Result<()> {
        let target = Self::name(name)?;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        for _ in 0..16 {
            let temporary = Self::name(&format!(
                ".ticker-{}-{}.tmp",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ))?;
            let fd = unsafe {
                libc::openat(self.0.as_raw_fd(), temporary.as_ptr(), libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC, 0o600)
            };
            if fd < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::AlreadyExists {
                    continue;
                }
                return Err(error.into());
            }
            // SAFETY: a successful openat returns a new descriptor owned here.
            let mut file = unsafe { File::from_raw_fd(fd) };
            let result = (|| -> Result<()> {
                file.write_all(bytes)?;
                file.sync_all()?;
                if unsafe { libc::renameat(self.0.as_raw_fd(), temporary.as_ptr(), self.0.as_raw_fd(), target.as_ptr()) } < 0 {
                    return Err(io::Error::last_os_error().into());
                }
                self.0.sync_all()?;
                Ok(())
            })();
            if result.is_err() {
                unsafe { libc::unlinkat(self.0.as_raw_fd(), temporary.as_ptr(), 0) };
            }
            return result;
        }
        bail!("no free receipt temporary name")
    }
}

/// What the store says about the spool's attempt.
enum Liveness {
    Live,
    /// Ended; `true` once its termination was observed (the spool is removed).
    Ended(bool),
    Missing,
}

fn liveness(db: &rusqlite::Connection, attempt: &str) -> Result<Liveness> {
    use rusqlite::OptionalExtension;
    let row: Option<(String, bool)> = db
        .query_row("SELECT state,termination_observed FROM attempts WHERE id=?1", [attempt], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    Ok(match row {
        None => Liveness::Missing,
        Some((state, false)) if ["reserved", "launching", "running", "awaiting_input"].contains(&state.as_str()) => Liveness::Live,
        Some((_, terminated)) => Liveness::Ended(terminated),
    })
}

fn table(db: &rusqlite::Connection, name: &str) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)", [name], |r| r.get(0))?)
}

/// The attempt a review session was launched for, or the opportunity of an
/// attempt's launched session.
fn launched(db: &rusqlite::Connection, sql: &str, key: &str) -> Result<Option<String>> {
    use rusqlite::OptionalExtension;
    if !table(db, "review_session_launches")? {
        return Ok(None);
    }
    Ok(db.query_row(sql, [key], |r| r.get(0)).optional()?)
}

/// Why a well-formed request of this attempt is refused before the store
/// sees it: it names another attempt's work.
fn foreign(db: &rusqlite::Connection, attempt: &str, request: &Request) -> Result<Option<String>> {
    let field = |document: &Option<String>, key: &str| -> Option<String> {
        serde_json::from_str::<serde_json::Value>(document.as_deref()?).ok()?.get(key)?.as_str().map(str::to_owned)
    };
    Ok(match request.kind {
        Kind::ResultSubmit => field(&request.document, "attempt_id")
            .filter(|named| named != attempt)
            .map(|named| format!("the result submission names attempt {named}, not this spool's attempt")),
        Kind::ReviewSubmit => match field(&request.document, "session_id") {
            Some(session) => launched(db, "SELECT attempt_id FROM review_session_launches WHERE session_id=?1", &session)?
                .filter(|owner| owner != attempt)
                .map(|_| format!("review session {session} was launched for another attempt")),
            None => None,
        },
        Kind::ReviewSession => match &request.argument {
            Some(named) if named == attempt => None,
            _ => Some("review session asks for another attempt's session".into()),
        },
        Kind::ReviewPresent => {
            let own = launched(db, "SELECT r.opportunity_id FROM review_session_launches l JOIN review_sessions r ON r.session_id=l.session_id WHERE l.attempt_id=?1", attempt)?;
            (own.is_none() || own != request.argument).then(|| "review present asks for an opportunity this attempt does not review".into())
        }
    })
}

/// Run the command a request stands for, exactly as the CLI runs it outside
/// a sandbox; its stdout, or its error as the CLI prints it.
fn run(at: &At<'_>, request: &Request) -> std::result::Result<String, String> {
    let document = || request.document.as_deref().map(str::as_bytes).ok_or_else(|| "the request carries no document".to_owned());
    let argument = || request.argument.as_deref().ok_or_else(|| "the request carries no argument".to_owned());
    let result = match request.kind {
        Kind::ResultSubmit => crate::store::submit_untrusted_result_bytes(at.held, at.project, document()?)
            .map_err(anyhow::Error::from)
            .and_then(|receipt| Ok(serde_json::to_string_pretty(&receipt)? + "\n")),
        Kind::ReviewSubmit => crate::telemetry::review::worker_submit(at.project, document()?),
        Kind::ReviewSession => crate::telemetry::review::worker_session(at.project, argument()?),
        Kind::ReviewPresent => crate::telemetry::review::worker_present(at.project, argument()?),
    };
    result.map_err(|error| format!("{error:#}"))
}

fn is_digest(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Whether some attempt's spool holds a request with no entry at its
/// receipt name: a cheap directory scan, no store access. The ticker checks
/// it between passes so a waiting worker is answered within a second. A
/// request whose receipt name the worker occupied waits for the next pass.
pub fn pending(project: &Path) -> bool {
    let Ok(root) = Dir::open(&project.join(".state/spool")) else { return false };
    let Ok(attempts) = root.names() else { return false };
    attempts.into_iter().filter_map(|n| n.into_string().ok()).filter(|n| valid_attempt(n)).any(|attempt| {
        let Ok(dir) = root.child(&attempt) else { return false };
        let Ok(names) = dir.names() else { return false };
        names.len() <= ENTRY_LIMIT
            && names.iter().filter_map(|n| n.to_str()?.strip_suffix(".request")).filter(|d| is_digest(d)).any(|d| matches!(dir.kind(&format!("{d}.receipt")), Ok(None)))
    })
}

/// One pass over `project`'s spools. Returns log lines; a failure in one
/// attempt's spool does not stop the others.
pub fn ingest(project: &Path) -> Result<Vec<String>> {
    let root_path = project.join(".state/spool");
    match std::fs::symlink_metadata(&root_path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
        Ok(metadata) => ensure!(metadata.is_dir(), "{} is not a directory", root_path.display()),
    }
    let root = Dir::open(&root_path)?;
    let attempts: Vec<String> = root.names()?.into_iter().filter_map(|n| n.into_string().ok()).filter(|n| valid_attempt(n)).collect();
    let mut lines = Vec::new();
    if attempts.is_empty() {
        return Ok(lines);
    }
    for attempt in attempts {
        if let Err(error) = ingest_attempt(project, &root, &attempt, &mut lines) {
            lines.push(format!("spool {attempt}: {error:#}"));
        }
    }
    Ok(lines)
}

/// A short read-only view of the store (no side files, no writer blocked);
/// closed before any write of this pass.
fn read<T>(project: &Path, f: impl FnOnce(&rusqlite::Connection) -> Result<T>) -> Result<T> {
    let db = crate::telemetry::read_only(&project.join(".state/state.db"))?;
    f(&db)
}

/// The project a request runs in, and the runtime guard the ticker holds
/// for it (the one `result submit` takes): store writes of a pass run under it.
struct At<'a> {
    project: &'a Path,
    held: &'a crate::migration::Maintenance,
}

fn ingest_attempt(project: &Path, root: &Dir, attempt: &str, lines: &mut Vec<String>) -> Result<()> {
    if !matches!(root.kind(attempt)?, Some((libc::S_IFDIR, _))) {
        lines.push(format!("spool {attempt}: not a directory; ignored"));
        return Ok(());
    }
    let dir = root.child(attempt)?;
    let status = read(project, |db| liveness(db, attempt))?;
    let names: Option<Vec<String>> = dir
        .names()
        .ok()
        .filter(|names| names.len() <= ENTRY_LIMIT)
        .map(|names| names.into_iter().filter_map(|n| n.into_string().ok()).collect());
    let pending: Vec<String> = names
        .iter()
        .flatten()
        .filter_map(|n| n.strip_suffix(".request"))
        .filter(|d| is_digest(d))
        .filter(|d| !matches!(dir.kind(&format!("{d}.receipt")), Ok(Some((libc::S_IFREG, 1)))))
        .map(str::to_owned)
        .take(PASS_LIMIT)
        .collect();
    if names.is_none() || !pending.is_empty() {
        // While an effect or maintenance holds the project, the requests stay
        // queued and the worker keeps waiting; a later pass answers them.
        let Ok(held) = crate::migration::runtime_mutation(project) else { return Ok(()) };
        let at = At { project, held: &held };
        if names.is_none() {
            deny(&at, &dir, attempt, None, None, &format!("the spool holds more than {ENTRY_LIMIT} entries"), lines);
        }
        for digest in &pending {
            handle(&at, &dir, attempt, &status, digest, lines);
        }
    }
    if matches!(status, Liveness::Ended(true)) {
        // The worker is gone: remove what it left, never following a link.
        for name in dir.names()?.into_iter().filter_map(|n| n.into_string().ok()) {
            let directory = matches!(dir.kind(&name), Ok(Some((libc::S_IFDIR, _))));
            if dir.unlink(&name, directory).is_err() {
                lines.push(format!("spool {attempt}: left {name:?} in place"));
            }
        }
        if root.unlink(attempt, true).is_ok() {
            lines.push(format!("spool {attempt}: removed after the attempt ended"));
        }
    }
    Ok(())
}

fn handle(at: &At<'_>, dir: &Dir, attempt: &str, status: &Liveness, digest: &str, lines: &mut Vec<String>) {
    let receipt = format!("{digest}.receipt");
    let request_name = format!("{digest}.request");
    let bytes = match dir.read(&request_name, REQUEST_LIMIT) {
        Ok(bytes) => bytes,
        Err(reason) => return deny(at, dir, attempt, Some(digest), None, &reason, lines),
    };
    if sha256_hex(&bytes) != digest {
        return deny(at, dir, attempt, Some(digest), None, "the request name is not the SHA-256 of its content", lines);
    }
    let request = match serde_json::from_slice::<Request>(&bytes) {
        Ok(request) if serde_json::to_vec(&request).is_ok_and(|canonical| canonical == bytes) && request.version == 1 => request,
        _ => return deny(at, dir, attempt, Some(digest), None, "the request is not a canonical spool request", lines),
    };
    let kind = Some(request.kind.name());
    if request.attempt_id != attempt {
        return deny(at, dir, attempt, Some(digest), kind, "the request names another attempt than its spool", lines);
    }
    match status {
        Liveness::Live => {}
        _ => return deny(at, dir, attempt, Some(digest), kind, "the attempt is not live", lines),
    }
    match read(at.project, |db| foreign(db, attempt, &request)) {
        Ok(None) => {}
        Ok(Some(reason)) => return deny(at, dir, attempt, Some(digest), kind, &reason, lines),
        Err(error) => return lines.push(format!("spool {attempt}: {digest}: {error:#}; retried next pass")),
    }
    if let Some((mode, _)) = dir.kind(&receipt).ok().flatten() {
        // Only the ticker writes receipts; a planted entry is removed first
        // (a link is unlinked, not followed).
        if mode != libc::S_IFREG {
            record(at, attempt, Some(digest), kind, "the receipt name held an entry the ticker did not write", lines);
            if mode == libc::S_IFDIR && dir.unlink(&receipt, true).is_err() {
                return;
            }
        }
    }
    let answer = run(at, &request);
    let outcome = match &answer {
        Ok(_) => "answered".to_owned(),
        Err(error) => format!("refused by the store: {error}"),
    };
    let (stdout, error) = match answer {
        Ok(stdout) => (Some(stdout), None),
        Err(error) => (None, Some(error)),
    };
    write_receipt(dir, attempt, digest, Receipt { version: 1, request_sha256: digest.into(), stdout, error }, lines);
    lines.push(format!("spool {attempt}: {} {digest} {outcome}", request.kind.name()));
}

fn write_receipt(dir: &Dir, attempt: &str, digest: &str, receipt: Receipt, lines: &mut Vec<String>) {
    let written = serde_json::to_vec(&receipt).map_err(anyhow::Error::from).and_then(|bytes| dir.replace(&format!("{digest}.receipt"), &bytes));
    if let Err(error) = written {
        lines.push(format!("spool {attempt}: receipt for {digest} not written: {error:#}"));
    }
}

/// Refuse a request: record it (bounded per attempt), log it, and answer
/// the worker so its command ends with the reason.
fn deny(at: &At<'_>, dir: &Dir, attempt: &str, digest: Option<&str>, kind: Option<&str>, reason: &str, lines: &mut Vec<String>) {
    record(at, attempt, digest, kind, reason, lines);
    if let Some(digest) = digest {
        let error = format!("submission spool refused the request: {reason}");
        write_receipt(dir, attempt, digest, Receipt { version: 1, request_sha256: digest.into(), stdout: None, error: Some(error) }, lines);
    }
}

/// Record and log a refusal (bounded per attempt) without answering it.
fn record(at: &At<'_>, attempt: &str, digest: Option<&str>, kind: Option<&str>, reason: &str, lines: &mut Vec<String>) {
    let recorded = match crate::store::record_spool_denial(at.held, at.project, attempt, digest.unwrap_or(""), kind, reason, DENIAL_RECORDS) {
        Ok(true) => "recorded",
        Ok(false) => "not recorded: denial cap reached",
        Err(error) => {
            lines.push(format!("spool {attempt}: denial not recorded: {error:#}"));
            "not recorded"
        }
    };
    lines.push(format!("spool {attempt}: denied {}: {reason} ({recorded})", digest.unwrap_or("spool")));
}

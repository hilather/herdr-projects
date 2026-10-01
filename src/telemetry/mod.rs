//! Telemetry analytics and read-only projections (docs/telemetry/contracts.md).
//! Never grants launch, accepts results or changes budgets.
pub mod accounting;
pub mod background;
pub mod analytics;
pub mod codex;
pub mod collectors;
pub mod export;
pub mod health;
pub mod ingest;
pub mod maintenance;
pub mod metrics;
pub mod outcome;
pub mod otlp;
mod gemini;
pub mod panel;
pub mod policies;
pub mod views;
pub mod workspace;
pub mod quality;
pub mod review;
pub mod sanitize;
pub mod sidecar;

/// One phase-2 lane's hooks (docs/telemetry/phase2-lanes.md), each defined in
/// the lane's own module and registered only here: its sidecar stream and
/// migrations, metrics merged into `metrics::report`, and ticker work. The
/// `analytics` lane (TM4.1) holds the query service's aggregate revisions and
/// adds no report keys; it runs after every stream it reads. The `health` lane
/// (TM4.5) holds alert state and adds no report keys either.
pub struct Lane {
    pub stream: &'static str,
    pub migrations: &'static [&'static str],
    pub metrics: fn(&std::path::Path, Option<i64>) -> anyhow::Result<std::collections::BTreeMap<String, serde_json::Value>>,
    pub tick: fn(&std::path::Path, codex::Budget) -> anyhow::Result<()>,
}

macro_rules! lane {
    ($module:ident) => { Lane { stream: $module::STREAM, migrations: $module::MIGRATIONS, metrics: $module::metrics, tick: $module::tick } };
}

pub const LANES: [Lane; 8] = [lane!(collectors), lane!(accounting), lane!(quality), lane!(review), lane!(policies), lane!(analytics), lane!(health), lane!(otlp)];

/// A read-only connection that writes and creates nothing (contracts §0 "Reads").
/// Fields drop in order: the connection closes before the lock is released.
#[derive(Clone)]
pub(crate) struct ReadOnly {
    db: std::rc::Rc<rusqlite::Connection>,
    _lock: std::rc::Rc<std::fs::File>,
}

impl std::ops::Deref for ReadOnly {
    type Target = rusqlite::Connection;
    fn deref(&self) -> &rusqlite::Connection { &self.db }
}

// Providers reopen stores independently. During refresh, route every read on
// this thread to the same pinned WAL snapshots, including nested provider reads.
thread_local! {
    static EVALUATION_READS: std::cell::RefCell<std::collections::BTreeMap<std::path::PathBuf, ReadOnly>> = const { std::cell::RefCell::new(std::collections::BTreeMap::new()) };
}

pub(crate) struct ReadTransaction<'a> {
    db: &'a rusqlite::Connection,
    _transaction: Option<rusqlite::Transaction<'a>>,
}
impl std::ops::Deref for ReadTransaction<'_> {
    type Target = rusqlite::Connection;
    fn deref(&self) -> &Self::Target { self.db }
}
impl ReadOnly {
    // A nested reader borrows the existing snapshot; only its owner rolls back.
    pub(crate) fn unchecked_transaction(&self) -> rusqlite::Result<ReadTransaction<'_>> {
        Ok(ReadTransaction { db: &self.db,
            _transaction: self.db.is_autocommit().then(|| self.db.unchecked_transaction()).transpose()? })
    }
}

pub(crate) struct EvaluationReads(std::marker::PhantomData<std::rc::Rc<()>>);
impl EvaluationReads {
    pub(crate) fn begin(project: &std::path::Path) -> anyhow::Result<Self> {
        anyhow::ensure!(EVALUATION_READS.with(|reads| reads.borrow().is_empty()), "nested evaluation snapshots");
        let guard = Self(std::marker::PhantomData);
        for path in [project.join(".state/state.db"), sidecar::path(project)] {
            let db = read_only(&path)?;
            db.execute_batch("BEGIN DEFERRED")?;
            // BEGIN alone does not establish a WAL snapshot. Pin it now.
            db.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get::<_, i64>(0))?;
            EVALUATION_READS.with(|reads| reads.borrow_mut().insert(path, db));
        }
        Ok(guard)
    }
}
impl Drop for EvaluationReads {
    fn drop(&mut self) {
        EVALUATION_READS.with(|reads| {
            for db in reads.borrow_mut().values() { let _ = db.execute_batch("ROLLBACK"); }
            reads.borrow_mut().clear();
        });
    }
}

/// Open a WAL database for reading without creating side files. A shared lock
/// on SQLite's SHARED byte range is taken first: while it is held no other
/// connection can take EXCLUSIVE, so none can checkpoint-on-close into the main
/// file or delete `-wal`/`-shm`. Then, with both `-wal` and `-shm` present, a
/// plain read-only open reads committed WAL frames through the existing
/// `-shm`; otherwise no connection has the database open and it is read
/// `immutable` (a plain read-only open would create `-wal`/`-shm` that only a
/// writer removes).
pub(crate) fn read_only(path: &std::path::Path) -> anyhow::Result<ReadOnly> {
    if let Some(db) = EVALUATION_READS.with(|reads| reads.borrow().get(path).cloned()) { return Ok(db); }
    read_only_fresh(path)
}

pub(crate) fn read_only_fresh(path: &std::path::Path) -> anyhow::Result<ReadOnly> {
    use anyhow::Context;
    use rusqlite::OpenFlags;
    use std::os::unix::fs::OpenOptionsExt;
    let lock = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path)
        .with_context(|| format!("open {}", path.display()))?;
    anyhow::ensure!(lock.metadata()?.is_file(), "{} is not a regular file", path.display());
    shared_lock(&lock).with_context(|| format!("lock {}", path.display()))?;
    let side = |suffix: &str| {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        std::fs::symlink_metadata(std::path::PathBuf::from(name)).is_ok()
    };
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let opened = if side("-wal") && side("-shm") {
        rusqlite::Connection::open_with_flags(path, flags)
    } else {
        let url = format!("file:{}?immutable=1", path.to_str().context("database path is not UTF-8")?.replace('%', "%25").replace('?', "%3f").replace('#', "%23"));
        rusqlite::Connection::open_with_flags(url, flags | OpenFlags::SQLITE_OPEN_URI)
    };
    let db = opened.with_context(|| format!("open {}", path.display()))?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(ReadOnly { db: std::rc::Rc::new(db), _lock: std::rc::Rc::new(lock) })
}

/// A read lock on SQLite's SHARED range (`PENDING_BYTE + 2`, 510 bytes), as an
/// open-file-description lock where available so it is released only with this
/// file, never by another descriptor of the same database closing. Retried for
/// up to 5 s while a writer holds EXCLUSIVE.
fn shared_lock(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    #[cfg(target_os = "linux")]
    const SET_LOCK: libc::c_int = libc::F_OFD_SETLK;
    #[cfg(not(target_os = "linux"))]
    const SET_LOCK: libc::c_int = libc::F_SETLK;
    // SAFETY: a zeroed flock is a valid value; every field used is set below.
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = libc::F_RDLCK as _;
    lock.l_whence = libc::SEEK_SET as _;
    lock.l_start = 0x4000_0002;
    lock.l_len = 510;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        // SAFETY: `file` is an open descriptor and `lock` a valid flock.
        if unsafe { libc::fcntl(file.as_raw_fd(), SET_LOCK, &lock) } == 0 { return Ok(()); }
        let error = std::io::Error::last_os_error();
        let busy = matches!(error.raw_os_error(), Some(libc::EAGAIN) | Some(libc::EACCES));
        if !busy || std::time::Instant::now() > deadline { return Err(error); }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

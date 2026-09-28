//! Telemetry analytics and read-only projections (docs/telemetry/contracts.md).
//! Never grants launch, accepts results or changes budgets.
pub mod codex;
pub mod metrics;
pub mod outcome;
pub mod panel;
pub mod sidecar;

/// Open a WAL database for reading without creating side files. A plain
/// read-only open creates `-wal`/`-shm` that only a writer removes; without a
/// `-wal` no connection has it open, so it is read `immutable` instead.
pub(crate) fn read_only(path: &std::path::Path) -> anyhow::Result<rusqlite::Connection> {
    use anyhow::Context;
    use rusqlite::OpenFlags;
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let opened = if std::fs::symlink_metadata(&wal).is_ok() {
        rusqlite::Connection::open_with_flags(path, flags)
    } else {
        std::fs::symlink_metadata(path)?;
        let url = format!("file:{}?immutable=1", path.to_str().context("database path is not UTF-8")?.replace('%', "%25").replace('?', "%3f").replace('#', "%23"));
        rusqlite::Connection::open_with_flags(url, flags | OpenFlags::SQLITE_OPEN_URI)
    };
    opened.with_context(|| format!("open {}", path.display()))
}

//! Writer admission for foreground telemetry and disposable ticker turns.
use std::time::{Duration, Instant};
use rusqlite::{Connection, Transaction, TransactionBehavior};

/// A foreground request spends at most thirty seconds waiting for writers,
/// across all its transactions. Ticker turns try once and defer on contention.
pub(crate) struct WriterWait {
    remaining: Duration,
    foreground: bool,
    jitter: u64,
}

impl WriterWait {
    pub(crate) fn foreground() -> Self {
        Self { remaining: Duration::from_secs(30), foreground: true, jitter: seed() }
    }

    pub(crate) fn ticker() -> Self {
        Self { remaining: Duration::ZERO, foreground: false, jitter: seed() }
    }

    pub(crate) fn retry_plan(&self) -> bool { self.foreground }

    pub(crate) fn acquire<'a>(&mut self, db: &'a Connection) -> rusqlite::Result<Transaction<'a>> {
        // Take over only writer admission, restoring the connection's normal
        // busy policy for statements after admission (and on every error).
        let timeout: u64 = db.query_row("PRAGMA busy_timeout", [], |r| r.get(0))?;
        db.busy_timeout(Duration::ZERO)?;
        let started = Instant::now();
        let budget = self.remaining;
        let result = loop {
            match Transaction::new_unchecked(db, TransactionBehavior::Immediate) {
                Ok(tx) => break Ok(tx),
                Err(error) if matches!(error.sqlite_error_code(),
                    Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
                    && started.elapsed() < budget => {
                    // Distinct process/request seeds avoid synchronized retry
                    // waves. Sleep without a writer transaction or lock.
                    self.jitter ^= self.jitter << 13;
                    self.jitter ^= self.jitter >> 7;
                    self.jitter ^= self.jitter << 17;
                    let delay = Duration::from_millis(10 + self.jitter % 41).min(budget.saturating_sub(started.elapsed()));
                    std::thread::sleep(delay);
                }
                Err(error) => break Err(error),
            }
        };
        self.remaining = self.remaining.saturating_sub(started.elapsed());
        db.busy_timeout(Duration::from_millis(timeout))?;
        result
    }
}

fn seed() -> u64 {
    // No global RNG, new crate, process or shared lock is needed for jitter.
    let time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default().as_nanos() as u64;
    (time ^ u64::from(std::process::id())).max(1)
}

/// Only SQLite writer contention is disposable; other failures stay visible.
pub(crate) fn busy(error: &anyhow::Error) -> bool {
    error.chain().filter_map(|cause| cause.downcast_ref::<rusqlite::Error>()).any(|error|
        matches!(error.sqlite_error_code(), Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)))
}

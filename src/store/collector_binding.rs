//! Canonical collector bindings (migration 0052, docs/telemetry/contracts-collection.md).
//! Analytics only: nothing here is read to grant launch. Revisions are
//! append-only; a revocation never rewrites or deletes an earlier revision.
use super::*;
use rusqlite::OptionalExtension;

/// One revision of an attempt's collector binding.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CollectorBinding {
    pub attempt_id: String,
    pub revision: u64,
    /// `active`, `revoked` or `predates_binding` (attempts that existed before 0052).
    pub state: String,
    /// The launched profile kind; `None` for `predates_binding`.
    pub collector: Option<String>,
    pub unix_ms: i64,
}

fn latest(tx: &Connection, attempt: &str) -> Result<Option<(CollectorBinding, Option<String>)>> {
    Ok(tx.query_row("SELECT revision,state,collector,execution_home,unix_ms FROM collector_bindings WHERE attempt_id=?1 ORDER BY revision DESC LIMIT 1",
        [attempt], |r| Ok((CollectorBinding { attempt_id: attempt.to_owned(), revision: r.get::<_, i64>(0)? as u64, state: r.get(1)?, collector: r.get(2)?, unix_ms: r.get(4)? }, r.get(3)?)))
        .optional()?)
}

/// Revision `active` for a launched attempt, in the `apply_launch_started`
/// transaction. Stores before 0052 have no binding table and record nothing.
pub(super) fn bind(tx: &Connection, attempt: &AttemptId, profile: &FrozenProfile, now: i64) -> Result<()> {
    let version: u32 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 52 { return Ok(()); }
    let revision = latest(tx, attempt.as_str())?.map_or(0, |(b, _)| b.revision) + 1;
    tx.execute("INSERT INTO collector_bindings(attempt_id,revision,state,collector,execution_home,unix_ms,source) VALUES(?1,?2,'active',?3,?4,?5,'apply_launch_started')",
        params![attempt.as_str(), integer(revision)?, profile.kind, profile.execution_home, now])?;
    Ok(())
}

impl SqliteStore {
    /// Append a `revoked` revision to the attempt's active collector binding.
    /// A revoked binding is returned unchanged (`false`: nothing was written).
    pub fn revoke_collector_binding(&mut self, attempt: &str, now: i64) -> Result<(CollectorBinding, bool)> {
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_schema(&tx)?;
        let version: u32 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 52 { return Err(StoreError::UnsupportedSchema(version)); }
        let Some((current, home)) = latest(&tx, attempt)? else {
            return Err(StoreError::Invalid(format!("attempt {attempt} has no collector binding")));
        };
        match current.state.as_str() {
            "revoked" => return Ok((current, false)),
            "active" => {}
            _ => return Err(StoreError::Invalid(format!("attempt {attempt} predates collector bindings"))),
        }
        let revoked = CollectorBinding { revision: current.revision + 1, state: "revoked".into(), unix_ms: now, ..current };
        tx.execute("INSERT INTO collector_bindings(attempt_id,revision,state,collector,execution_home,unix_ms,source) VALUES(?1,?2,'revoked',?3,?4,?5,'revoke_collector_binding')",
            params![attempt, integer(revoked.revision)?, revoked.collector, home, now])?;
        tx.commit()?;
        Ok((revoked, true))
    }
}

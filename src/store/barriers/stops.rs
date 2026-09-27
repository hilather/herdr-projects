//! Bounded routing from immutable barrier invalidations to desired cancellation.
//! Physical stop and capacity release remain the existing reconciler's job.
use super::*;

#[derive(Debug, Serialize)]
pub struct BarrierStopTurn {
    pub pending: bool,
    pub requested: usize,
}

impl SqliteStore {
    pub(crate) fn service_barrier_stops(&mut self, now: i64, budget: &read_budget::ReadBudget) -> Result<BarrierStopTurn> {
        budget.check()?;
        let version: u32 = self.connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version < 43 { return Ok(BarrierStopTurn { pending: false, requested: 0 }); }
        let after: Option<String> = self.connection.query_row("SELECT attempt_id FROM barrier_stop_cursor WHERE singleton=1", [], |row| row.get(0)).optional()?;
        let mut requests = Vec::new();
        for cursor in [after.as_deref().unwrap_or(""), ""] {
            let mut query = self.connection.prepare("SELECT p.attempt_id,a.revision,i.revocation_sequence
                FROM barrier_pending_stops p JOIN attempts a ON a.id=p.attempt_id
                JOIN attempt_barrier_invalidations i ON i.attempt_id=p.attempt_id
                WHERE p.attempt_id>?1 ORDER BY p.attempt_id LIMIT 8")?;
            let mut rows = query.query([cursor])?;
            while let Some(row) = rows.next()? {
                budget.row(row, &[])?;
                requests.push((row.get::<_, String>(0)?, row.get::<_, u64>(1)?, row.get::<_, u64>(2)?));
            }
            if !requests.is_empty() || cursor.is_empty() { break; }
        }
        let mut requested = 0;
        for (attempt, revision, sequence) in &requests {
            budget.check()?;
            // Advisory rotation survives failure/restart, so one broken stop
            // cannot starve all later consumers. The obligation itself remains.
            self.connection.execute("INSERT INTO barrier_stop_cursor VALUES(1,?1) ON CONFLICT(singleton) DO UPDATE SET attempt_id=excluded.attempt_id", [attempt])?;
            let id = AttemptId::new(attempt.clone()).map_err(StoreError::Corrupt)?;
            self.cancel_attempt_with_budget(&id, *revision, head(&self.connection)?,
                &format!("required barrier revoked at event {sequence}"), now, Some(budget))?;
            requested += 1;
        }
        budget.check()?;
        Ok(BarrierStopTurn { pending: !requests.is_empty(), requested })
    }
}

pub fn service_project_barrier_stops(project: &Path) -> anyhow::Result<BarrierStopTurn> {
    let _guard = crate::migration::runtime_mutation(project)?;
    let control = controlled::ReadControl::new(std::time::Instant::now()+Duration::from_secs(2), Default::default());
    Ok(crate::migration::open_active_scoped(project, control)?.service_barrier_stops(jiff::Timestamp::now().as_millisecond())?)
}

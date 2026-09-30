//! Scoped launch receipts and lifecycle observations for expiring decorations.
use super::*;
use rusqlite::OptionalExtension;

pub struct Rows {
    pub entries: Vec<Entry>,
}
pub struct Entry {
    pub binding: RuntimeBinding,
    pub owner: Option<RuntimeOwnership>,
    pub retained: RuntimeOwnership,
    pub attempt: Attempt,
    pub input: AttemptInputRecord,
    pub started: LaunchStartedReceipt,
    pub publishing: bool,
    pub cleanup_ms: Option<i64>,
}
impl SqliteStore {
    pub(crate) fn attempt_token_rows(
        &mut self,
        selected: Option<&str>,
        since: i64,
        budget: &read_budget::ReadBudget,
    ) -> Result<Rows> {
        budget.check()?;
        let tx = self.connection.transaction()?;
        let schema: u32 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if schema < 52 {
            return Ok(Rows {
                entries: Vec::new(),
            });
        }
        let control = control::read_with_budget(&tx, Some(budget))?;
        let paused: Option<i64> = if control.state != ProjectState::Active
            || control.reconciliation_required
        {
            tx.query_row("SELECT json_extract(payload,'$.observed_unix_ms') FROM events WHERE entity='project' AND kind IN ('project.control_changed','project.reconciliation_invalidated') ORDER BY sequence DESC LIMIT 1", [], |r| r.get(0)).optional()?.flatten()
        } else {
            None
        };
        // Only launched bindings; never decode unrelated historical events or tasks.
        let mut query = tx.prepare("SELECT entity,payload FROM events e WHERE kind='runtime.launched' AND (?1 IS NULL OR entity=?1) AND sequence=(SELECT max(sequence) FROM events WHERE kind='runtime.launched' AND entity=e.entity) AND (?1 IS NOT NULL
            OR EXISTS(SELECT 1 FROM runtime_ownership o JOIN attempts a ON a.id=o.attempt_id WHERE o.binding_id=e.entity AND a.state IN ('running','awaiting_input') AND a.termination_observed=0)
            OR EXISTS(SELECT 1 FROM attempt_lifecycle l WHERE l.attempt_id=json_extract(e.payload,'$.attempt') AND l.state IN ('completed','failed','cancelled','lost') AND l.unix_ms>=?2)
            OR EXISTS(SELECT 1 FROM events t WHERE t.kind='runtime.worker_terminated' AND t.entity=json_extract(e.payload,'$.attempt') AND json_extract(t.payload,'$.observed_unix_ms')>=?2)
            OR EXISTS(SELECT 1 FROM events t WHERE t.kind='runtime.relinquished' AND t.entity=e.entity AND json_extract(t.payload,'$.observed_unix_ms')>=?2)
            OR EXISTS(SELECT 1 FROM collector_bindings c WHERE c.attempt_id=json_extract(e.payload,'$.attempt') AND c.state='revoked' AND c.unix_ms>=?2)
            OR ?3>=?2) ORDER BY entity")?;
        let mut rows = query.query(params![selected, since, paused])?;
        let mut entries = Vec::new();
        while let Some(row) = rows.next()? {
            budget.row(row, &[(1, 1)])?;
            let id: String = row.get(0)?;
            let retained: RuntimeOwnership = serde_json::from_str(&row.get::<_, String>(1)?)
                .map_err(|e| StoreError::Corrupt(e.to_string()))?;
            let Some(attempt_id) = &retained.attempt else {
                continue;
            };
            let attempt = read_attempt_with_budget(&tx, attempt_id, Some(budget))?;
            let owner = ownership::read_binding(&tx, &id, Some(budget))?;
            let revoked: Option<(String,i64)> = tx.query_row("SELECT state,unix_ms FROM collector_bindings WHERE attempt_id=?1 ORDER BY revision DESC LIMIT 1", [attempt_id.as_str()], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
            let ended: Option<i64> = tx.query_row("SELECT json_extract(payload,'$.observed_unix_ms') FROM events WHERE kind='runtime.worker_terminated' AND entity=?1 ORDER BY sequence DESC LIMIT 1", [attempt_id.as_str()], |r| r.get(0)).optional()?.flatten();
            let ended = if ended.is_some() {
                ended
            } else {
                tx.query_row("SELECT unix_ms FROM attempt_lifecycle WHERE attempt_id=?1 AND state IN ('completed','failed','cancelled','lost') ORDER BY unix_ms DESC LIMIT 1", [attempt_id.as_str()], |r| r.get(0)).optional()?
            };
            let relinquished: Option<i64> = tx.query_row("SELECT json_extract(payload,'$.observed_unix_ms') FROM events WHERE kind='runtime.relinquished' AND entity=?1 ORDER BY sequence DESC LIMIT 1", [&id], |r| r.get(0)).optional()?.flatten();
            let publishing = owner.as_ref() == Some(&retained)
                && matches!(
                    attempt.state,
                    AttemptState::Running | AttemptState::AwaitingInput
                )
                && !attempt.termination_observed
                && control.state == ProjectState::Active
                && !control.reconciliation_required
                && !revoked.as_ref().is_some_and(|r| r.0 == "revoked");
            let cleanup_ms = [
                ended,
                relinquished,
                paused,
                revoked.filter(|r| r.0 == "revoked").map(|r| r.1),
            ]
            .into_iter()
            .flatten()
            .max();
            if selected.is_none() && !publishing && !cleanup_ms.is_some_and(|ms| ms >= since) {
                continue;
            }
            let Some(binding) = runtime::read_binding(&tx, &id, Some(budget))? else {
                continue;
            };
            let input = reservations::read_attempt_input(&tx, attempt_id.as_str(), Some(budget))?;
            let payload: String = read_budget::one(
                &tx,
                "SELECT payload FROM events WHERE kind='runtime.launch_started' AND entity=?1 ORDER BY sequence LIMIT 1",
                [input.operation.as_str()],
                Some(budget),
                &[(0, 1)],
                |r| r.get(0),
            )?;
            let started =
                serde_json::from_str(&payload).map_err(|e| StoreError::Corrupt(e.to_string()))?;
            entries.push(Entry {
                binding,
                owner,
                retained,
                attempt,
                input,
                started,
                publishing,
                cleanup_ms,
            });
        }
        Ok(Rows { entries })
    }
}

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
}
impl SqliteStore {
    pub(crate) fn attempt_token_rows(
        &mut self,
        selected: &[String],
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
        // Explicit cleanup ids come only from this ticker's bounded publish memory.
        let explicit = selected;
        let selected = serde_json::to_string(selected).map_err(|e| StoreError::Invalid(e.to_string()))?;
        let mut query = tx.prepare("SELECT entity,payload FROM events e WHERE kind='runtime.launched' AND sequence=(SELECT max(sequence) FROM events WHERE kind='runtime.launched' AND entity=e.entity) AND (
            entity IN (SELECT value FROM json_each(?1))
            OR (?2 AND EXISTS(SELECT 1 FROM runtime_ownership o JOIN attempts a ON a.id=o.attempt_id WHERE o.binding_id=e.entity AND a.state IN ('running','awaiting_input') AND a.termination_observed=0)
            AND NOT EXISTS(SELECT 1 FROM collector_bindings c WHERE c.attempt_id=json_extract(e.payload,'$.attempt') AND c.revision=(SELECT max(revision) FROM collector_bindings WHERE attempt_id=c.attempt_id) AND c.state='revoked'))) ORDER BY entity")?;
        let mut rows = query.query(params![selected, control.state == ProjectState::Active && !control.reconciliation_required])?;
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
            let publishing = owner.as_ref() == Some(&retained)
                && matches!(
                    attempt.state,
                    AttemptState::Running | AttemptState::AwaitingInput
                )
                && !attempt.termination_observed
                && control.state == ProjectState::Active
                && !control.reconciliation_required
                && !revoked.as_ref().is_some_and(|r| r.0 == "revoked");
            if !publishing && !explicit.contains(&id) { continue; }
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
            });
        }
        Ok(Rows { entries })
    }
}

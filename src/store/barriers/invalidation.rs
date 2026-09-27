//! Route published memory changes independently of active worker transports.
use super::*;

fn collect(
    db: &Connection,
    sql: &str,
    values: impl rusqlite::Params,
    ids: &mut std::collections::BTreeSet<String>,
) -> Result<()> {
    let mut query = db.prepare(sql)?;
    let mut rows = query.query(values)?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        if !hex64(&id) {
            return Err(StoreError::Corrupt(
                "invalid barrier routing identity".into(),
            ));
        }
        ids.insert(id);
        if ids.len() > 1000 {
            return Err(StoreError::Limit(
                "memory change affects more than 1000 barriers".into(),
            ));
        }
    }
    Ok(())
}

pub(super) fn changed(db: &Connection, record: Option<&str>, sequence: u64) -> Result<()> {
    let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < 43 {
        return Ok(());
    }
    // A barrier's v2 catalog vector is conservatively global. Required/contract
    // changes and policy changes therefore invalidate every open barrier.
    let global = match record {
        None => true,
        Some(record) => db.query_row("SELECT is_hard=1 OR kind IN ('constraint','hard_memory','contract') FROM memory_records WHERE id=?1", [record], |row| row.get(0))?,
    };
    let mut ids = std::collections::BTreeSet::new();
    if global {
        collect(
            db,
            "SELECT barrier_id FROM barrier_open_members GROUP BY barrier_id ORDER BY barrier_id LIMIT 1001",
            [],
            &mut ids,
        )?;
    } else {
        collect(
            db,
            "SELECT barrier_id FROM barrier_open_memory_records WHERE record_id=?1 ORDER BY barrier_id LIMIT 1001",
            [record.unwrap()],
            &mut ids,
        )?;
        // Old read sets cannot prove an optional record irrelevant. Keep their
        // history, but conservatively revoke applicability on a memory change.
        collect(
            db,
            "SELECT barrier_id FROM barrier_open_memory_unknown ORDER BY barrier_id LIMIT 1001",
            [],
            &mut ids,
        )?;
    }
    // Collect and bound all IDs before mutating projections or publishing events.
    for id in ids {
        // An earlier ancestor in this same batch may already have withdrawn it.
        let applicable: bool = db.query_row("SELECT revoked_seq IS NULL FROM barrier_current_status WHERE barrier_id=?1", [&id], |row| row.get(0))?;
        if !applicable { continue; }
        insert_event(
            db,
            "barrier.revoked",
            &id,
            &serde_json::json!({
                "reason":"frozen_memory_changed", "record_id":record, "triggering_seq":sequence
            }),
        )?;
    }
    Ok(())
}

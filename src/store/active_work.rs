//! Rebuildable index of bindings that still need reconciliation.
//! Retired bindings remain in `runtime_bindings` and are not copied here.
use super::*;
use rusqlite::StatementStatus;

pub const ACTIVE_WORK_PAGE: usize = 64;
const SCHEMA_VERSION: u32 = 41;
const EMPTY_REVISION: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveCoverage {
    Incomplete,
    Complete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveWorkItem {
    pub ordinal: i64,
    pub binding: RuntimeBinding,
    pub task_revision: Option<u64>,
    /// Retained attempts for this task. The task's active attempt is first when it is one of them.
    pub attempt_ids: Vec<String>,
    pub retains_capacity: bool,
    pub ownership: Option<RuntimeOwnership>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveWorkPage {
    pub incarnation: String,
    pub inventory_revision: u64,
    pub cursor: Option<i64>,
    pub coverage: ActiveCoverage,
    pub reached_end: bool,
    pub missing: bool,
    pub items: Vec<ActiveWorkItem>,
    pub rows_read: u64,
    pub fullscan_steps: i64,
    pub head: u64,
    pub capacity_release_allowed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveWorkRun {
    pub incarnation: String,
    pub inventory_revision: u64,
    pub coverage: ActiveCoverage,
    pub items: Vec<ActiveWorkItem>,
    pub rows_read: u64,
    pub fullscan_steps: i64,
    pub head: u64,
    pub capacity_release_allowed: bool,
}

#[derive(Clone)]
struct Meta {
    incarnation: String,
    projection_revision: String,
    inventory_revision: u64,
    /// True when any attempt still has termination_observed = 0, even with no binding.
    retains_attempt_capacity: bool,
}

struct Account {
    pub rows: u64,
    pub fullscan: i64,
}

struct IndexRow {
    ordinal: i64,
    binding_id: String,
    attempt_ids: String,
    retains_capacity: bool,
}

fn require_schema(db: &Connection) -> Result<()> {
    check_schema(db)?;
    let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchema(version));
    }
    Ok(())
}

fn note(stmt: &rusqlite::Statement<'_>, account: &mut Account) {
    account.fullscan += i64::from(stmt.get_status(StatementStatus::FullscanStep));
}

fn query<T>(
    db: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
    account: &mut Account,
    mut map: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>> {
    let mut stmt = db.prepare(sql)?;
    let mut rows = stmt.query(params)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        account.rows += 1;
        out.push(map(row)?);
    }
    drop(rows);
    note(&stmt, account);
    Ok(out)
}

fn fingerprint(rows: &[(String, String, i64)]) -> String {
    let mut hasher = Sha256::new();
    for (id, task, revision) in rows {
        hasher.update(id.as_bytes());
        hasher.update([0]);
        hasher.update(task.as_bytes());
        hasher.update(revision.to_le_bytes());
        hasher.update([0xff]);
    }
    format!("{:x}", hasher.finalize())
}

fn load_meta(db: &Connection, account: &mut Account) -> Result<Meta> {
    let rows = query(
        db,
        "SELECT incarnation, projection_revision, inventory_revision FROM active_work_meta WHERE singleton=1",
        [],
        account,
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        },
    )?;
    let Some((incarnation, projection_revision, inventory_revision)) = rows.into_iter().next()
    else {
        return Err(StoreError::Corrupt(
            "active work projection is missing".into(),
        ));
    };
    if incarnation.len() != 64 || projection_revision.len() != 64 || inventory_revision < 0 {
        return Err(StoreError::Corrupt(
            "active work projection is invalid".into(),
        ));
    }
    Ok(Meta {
        incarnation,
        projection_revision,
        inventory_revision: inventory_revision as u64,
        retains_attempt_capacity: false,
    })
}

fn retained_rows(db: &Connection, account: &mut Account) -> Result<Vec<(String, String, i64)>> {
    query(
        db,
        "SELECT id, task_id, revision FROM attempts WHERE termination_observed = 0 ORDER BY id",
        [],
        account,
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
}

fn ordered_attempt_ids(
    task_id: Option<&str>,
    by_task: &std::collections::BTreeMap<String, Vec<String>>,
    active_attempt: Option<&str>,
) -> Vec<String> {
    let Some(task_id) = task_id else {
        return Vec::new();
    };
    let Some(ids) = by_task.get(task_id) else {
        return Vec::new();
    };
    let mut ordered = Vec::with_capacity(ids.len());
    if let Some(active) = active_attempt {
        if ids.iter().any(|id| id == active) {
            ordered.push(active.to_string());
        }
    }
    for id in ids {
        if ordered.first().map(String::as_str) != Some(id.as_str()) {
            ordered.push(id.clone());
        }
    }
    ordered
}

/// The projection is disposable. A fingerprint mismatch rebuilds it from attempts
/// that still retain capacity, bindings with no attempt history, and bindings
/// that still have an ownership row.
fn ensure(db: &Connection, account: &mut Account) -> Result<Meta> {
    require_schema(db)?;
    let mut meta = load_meta(db, account)?;
    let retained = retained_rows(db, account)?;
    meta.retains_attempt_capacity = !retained.is_empty();
    let next = fingerprint(&retained);
    if meta.projection_revision == next {
        return Ok(meta);
    }
    let mut by_task = std::collections::BTreeMap::<String, Vec<String>>::new();
    for (id, task, _) in &retained {
        by_task.entry(task.clone()).or_default().push(id.clone());
    }
    let candidates = query(
        db,
        "SELECT b.id, b.task_id,
                EXISTS(SELECT 1 FROM attempts a WHERE a.task_id = b.task_id),
                EXISTS(SELECT 1 FROM runtime_ownership o WHERE o.binding_id = b.id),
                t.active_attempt
         FROM runtime_bindings b LEFT JOIN tasks t ON t.id = b.task_id ORDER BY b.id",
        [],
        account,
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        },
    )?;
    db.execute("DELETE FROM active_work_index", [])?;
    let mut ordinal = 0i64;
    for (id, task_id, has_attempt, owned, active_attempt) in candidates {
        let attempt_ids =
            ordered_attempt_ids(task_id.as_deref(), &by_task, active_attempt.as_deref());
        // A still-owned binding stays observable until relinquish, even if every attempt is terminated.
        let include =
            task_id.is_none() || !attempt_ids.is_empty() || has_attempt == 0 || owned == 1;
        if !include {
            continue;
        }
        let encoded = serde_json::to_string(&attempt_ids)
            .map_err(|error| StoreError::Invalid(error.to_string()))?;
        db.execute(
            "INSERT INTO active_work_index(ordinal, binding_id, task_id, attempt_ids, retains_capacity) VALUES(?1,?2,?3,?4,?5)",
            params![ordinal, id, task_id, encoded, i64::from(!attempt_ids.is_empty())],
        )?;
        ordinal += 1;
    }
    let inventory = meta
        .inventory_revision
        .checked_add(1)
        .ok_or_else(|| StoreError::Invalid("active inventory revision exhausted".into()))?;
    db.execute(
        "UPDATE active_work_meta SET projection_revision=?1, inventory_revision=?2 WHERE singleton=1",
        params![next, i64::try_from(inventory).map_err(|_| StoreError::Invalid("active inventory revision exhausted".into()))?],
    )?;
    meta.projection_revision = next;
    meta.inventory_revision = inventory;
    Ok(meta)
}

pub(super) fn sync_projection(db: &Connection) -> Result<()> {
    let mut account = Account {
        rows: 0,
        fullscan: 0,
    };
    ensure(db, &mut account)?;
    Ok(())
}

pub(super) fn recorded_bindings(db: &Connection, ids: &[String]) -> Result<Vec<RuntimeBinding>> {
    let mut account = Account {
        rows: 0,
        fullscan: 0,
    };
    let session = session_digest(db, &mut account)?;
    let mut bindings = Vec::with_capacity(ids.len());
    for id in ids {
        bindings.push(load_binding(db, id, session.as_deref(), &mut account)?);
    }
    Ok(bindings)
}

pub(super) fn invalidate(db: &Connection) -> Result<()> {
    let present: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='active_work_meta')",
        [],
        |row| row.get(0),
    )?;
    if !present {
        return Ok(());
    }
    db.execute(
        "UPDATE active_work_meta SET projection_revision=?1 WHERE singleton=1",
        [EMPTY_REVISION],
    )?;
    Ok(())
}

fn session_digest(db: &Connection, account: &mut Account) -> Result<Option<String>> {
    let rows = query(
        db,
        "SELECT digest, bytes FROM legacy_sources WHERE path='.state/coordinator.json' AND kind='runtime'",
        [],
        account,
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
    )?;
    let Some((digest, bytes)) = rows.into_iter().next() else {
        return Ok(None);
    };
    if format!("{:x}", Sha256::digest(&bytes)) != digest {
        return Err(StoreError::Corrupt(
            "runtime session source hash mismatch".into(),
        ));
    }
    Ok(Some(digest))
}

fn decode_binding(
    id: String,
    task: Option<String>,
    revision: u64,
    source: Option<String>,
    payload: String,
    hash: String,
    digest: Option<String>,
    bytes: Option<Vec<u8>>,
    session: Option<&str>,
) -> Result<RuntimeBinding> {
    if format!("{:x}", Sha256::digest(payload.as_bytes())) != hash {
        return Err(StoreError::Corrupt(
            "runtime binding payload hash mismatch".into(),
        ));
    }
    let binding: RuntimeBinding = serde_json::from_str(&payload)
        .map_err(|_| StoreError::Corrupt("invalid runtime binding payload".into()))?;
    if binding.id != id
        || binding.task.as_ref().map(TaskId::as_str) != task.as_deref()
        || binding.revision != revision
        || binding.source_path != source
        || binding.source_digest != digest
    {
        return Err(StoreError::Corrupt(
            "runtime binding row identity mismatch".into(),
        ));
    }
    if source.is_some() {
        let digest = digest
            .as_ref()
            .ok_or_else(|| StoreError::Corrupt("runtime source missing".into()))?;
        let bytes = bytes
            .as_ref()
            .ok_or_else(|| StoreError::Corrupt("runtime source bytes missing".into()))?;
        if format!("{:x}", Sha256::digest(bytes)) != *digest {
            return Err(StoreError::Corrupt("runtime source hash mismatch".into()));
        }
    }
    let expected_session = if binding.source_path.is_some() && binding.task.is_some() {
        session
    } else {
        None
    };
    if binding.session_source_digest.as_deref() != expected_session {
        return Err(StoreError::Corrupt(
            "runtime session provenance mismatch".into(),
        ));
    }
    if binding.source_path.is_none() {
        if binding.source_digest.is_some()
            || binding.session_source_digest.is_some()
            || binding.identity.execution_fingerprint.is_some()
        {
            return Err(StoreError::Corrupt(
                "canonical runtime has fabricated import provenance".into(),
            ));
        }
        let expected_id = binding
            .task
            .as_ref()
            .map(|task| format!("task:{}", task.as_str()))
            .unwrap_or_else(|| "coordinator".into());
        if binding.id != expected_id {
            return Err(StoreError::Corrupt(
                "canonical runtime identity mismatch".into(),
            ));
        }
        RuntimeRoute::from_identity(&binding.identity)
            .validate()
            .map_err(StoreError::Corrupt)?;
    }
    Ok(binding)
}

fn load_binding(
    db: &Connection,
    id: &str,
    session: Option<&str>,
    account: &mut Account,
) -> Result<RuntimeBinding> {
    let rows = query(
        db,
        "SELECT b.id, b.task_id, b.revision, b.source_path, b.payload, b.payload_hash, s.digest, s.bytes
         FROM runtime_bindings b LEFT JOIN legacy_sources s ON s.path = b.source_path WHERE b.id = ?1",
        params![id],
        account,
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<Vec<u8>>>(7)?,
            ))
        },
    )?;
    let Some((binding_id, task, revision, source, payload, hash, digest, bytes)) =
        rows.into_iter().next()
    else {
        return Err(StoreError::Corrupt(
            "active inventory binding is missing".into(),
        ));
    };
    let revision = u64::try_from(revision)
        .map_err(|_| StoreError::Corrupt("active inventory binding revision is invalid".into()))?;
    decode_binding(
        binding_id, task, revision, source, payload, hash, digest, bytes, session,
    )
}

fn load_ownership(
    db: &Connection,
    id: &str,
    account: &mut Account,
) -> Result<Option<RuntimeOwnership>> {
    let rows = query(
        db,
        "SELECT binding_id, revision, binding_revision, attempt_id, payload, payload_hash FROM runtime_ownership WHERE binding_id=?1",
        params![id],
        account,
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        },
    )?;
    let Some((binding_id, revision, binding_revision, attempt, payload, hash)) =
        rows.into_iter().next()
    else {
        return Ok(None);
    };
    if format!("{:x}", Sha256::digest(payload.as_bytes())) != hash {
        return Err(StoreError::Corrupt(
            "ownership payload hash mismatch".into(),
        ));
    }
    let owned: RuntimeOwnership = serde_json::from_str(&payload)
        .map_err(|_| StoreError::Corrupt("invalid ownership payload".into()))?;
    let revision = u64::try_from(revision)
        .map_err(|_| StoreError::Corrupt("ownership row identity mismatch".into()))?;
    let binding_revision = u64::try_from(binding_revision)
        .map_err(|_| StoreError::Corrupt("ownership row identity mismatch".into()))?;
    if owned.binding != binding_id
        || owned.revision != revision
        || owned.binding_revision != binding_revision
        || owned.attempt.as_ref().map(AttemptId::as_str) != attempt.as_deref()
        || !matches!(owned.origin.as_str(), "adopted" | "launched")
        || !crate::operations::finalization::hash(&owned.identity_digest)
        || owned.observed_unix_ms < 0
    {
        return Err(StoreError::Corrupt(
            "ownership row identity mismatch".into(),
        ));
    }
    Ok(Some(owned))
}

fn task_revision(db: &Connection, task: &str, account: &mut Account) -> Result<u64> {
    let rows = query(
        db,
        "SELECT revision FROM tasks WHERE id=?1",
        params![task],
        account,
        |row| row.get::<_, i64>(0),
    )?;
    let Some(revision) = rows.into_iter().next() else {
        return Err(StoreError::Corrupt(
            "active inventory task is missing".into(),
        ));
    };
    u64::try_from(revision)
        .map_err(|_| StoreError::Corrupt("active inventory task revision is invalid".into()))
}

fn ordinal_exists(db: &Connection, ordinal: i64, account: &mut Account) -> Result<bool> {
    let rows = query(
        db,
        "SELECT EXISTS(SELECT 1 FROM active_work_index WHERE ordinal=?1)",
        params![ordinal],
        account,
        |row| row.get::<_, i64>(0),
    )?;
    Ok(rows.first().copied().unwrap_or(0) == 1)
}

fn read_page(
    db: &Connection,
    cursor: Option<i64>,
    account: &mut Account,
) -> Result<(Vec<ActiveWorkItem>, bool, bool)> {
    let start = cursor.unwrap_or(-1);
    let index_rows = query(
        db,
        "SELECT ordinal, binding_id, attempt_ids, retains_capacity FROM active_work_index WHERE ordinal > ?1 ORDER BY ordinal LIMIT ?2",
        params![start, ACTIVE_WORK_PAGE as i64],
        account,
        |row| {
            Ok(IndexRow {
                ordinal: row.get(0)?,
                binding_id: row.get(1)?,
                attempt_ids: row.get(2)?,
                retains_capacity: row.get::<_, i64>(3)? == 1,
            })
        },
    )?;
    if index_rows.is_empty() {
        if cursor.is_none() {
            return Ok((Vec::new(), true, false));
        }
        let missing = !ordinal_exists(db, start, account)?;
        return Ok((Vec::new(), !missing, missing));
    }
    let session = session_digest(db, account)?;
    let mut items = Vec::new();
    let mut expected = start + 1;
    for row in index_rows {
        if row.ordinal != expected {
            return Ok((items, false, true));
        }
        let binding = load_binding(db, &row.binding_id, session.as_deref(), account)?;
        let task_revision = match binding.task.as_ref() {
            Some(task) => Some(task_revision(db, task.as_str(), account)?),
            None => None,
        };
        let ownership = load_ownership(db, &row.binding_id, account)?;
        expected = row.ordinal + 1;
        let attempt_ids = serde_json::from_str(&row.attempt_ids)
            .map_err(|_| StoreError::Corrupt("active inventory attempt ids are invalid".into()))?;
        items.push(ActiveWorkItem {
            ordinal: row.ordinal,
            binding,
            task_revision,
            attempt_ids,
            retains_capacity: row.retains_capacity,
            ownership,
        });
    }
    let reached_end = items.len() < ACTIVE_WORK_PAGE;
    Ok((items, reached_end, false))
}

fn release_allowed(coverage: ActiveCoverage, retains_attempt_capacity: bool) -> bool {
    // Retained attempts block release even when no binding is indexed.
    coverage == ActiveCoverage::Complete && !retains_attempt_capacity
}

fn page_from(
    meta: &Meta,
    cursor: Option<i64>,
    items: Vec<ActiveWorkItem>,
    reached_end: bool,
    missing: bool,
    account: &Account,
    head: u64,
) -> ActiveWorkPage {
    // A page is complete only when it is the whole inventory from the origin.
    let coverage = if missing || !reached_end || cursor.is_some() {
        ActiveCoverage::Incomplete
    } else {
        ActiveCoverage::Complete
    };
    let capacity_release_allowed = release_allowed(coverage, meta.retains_attempt_capacity);
    ActiveWorkPage {
        incarnation: meta.incarnation.clone(),
        inventory_revision: meta.inventory_revision,
        cursor: items.last().map(|item| item.ordinal),
        coverage,
        reached_end,
        missing,
        capacity_release_allowed,
        rows_read: account.rows,
        fullscan_steps: account.fullscan,
        head,
        items,
    }
}

impl SqliteStore {
    pub fn current_head(&self) -> Result<u64> {
        check_schema(&self.connection)?;
        head(&self.connection)
    }

    pub fn active_work_page(&mut self, cursor: Option<i64>) -> Result<ActiveWorkPage> {
        let tx = self.connection.transaction()?;
        let mut account = Account {
            rows: 0,
            fullscan: 0,
        };
        let meta = ensure(&tx, &mut account)?;
        let (items, reached_end, missing) = read_page(&tx, cursor, &mut account)?;
        let head = head(&tx)?;
        let page = page_from(&meta, cursor, items, reached_end, missing, &account, head);
        tx.commit()?;
        Ok(page)
    }

    /// Walk active pages until coverage is complete or `max_pages` stops the caller.
    /// Stopping early, or a hole in the ordinals, is incomplete and cannot release capacity.
    pub fn reconcile_active_work(&mut self, max_pages: Option<u32>) -> Result<ActiveWorkRun> {
        let tx = self.connection.transaction()?;
        let mut account = Account {
            rows: 0,
            fullscan: 0,
        };
        let meta = ensure(&tx, &mut account)?;
        let mut cursor = None;
        let mut items = Vec::new();
        let mut pages = 0u32;
        let mut reached_end = false;
        let mut missing = false;
        let mut stopped = false;
        loop {
            if max_pages.is_some_and(|limit| pages >= limit) {
                stopped = true;
                break;
            }
            let (page_items, end, hole) = read_page(&tx, cursor, &mut account)?;
            pages += 1;
            if hole {
                missing = true;
                items.extend(page_items);
                break;
            }
            cursor = page_items.last().map(|item| item.ordinal).or(cursor);
            items.extend(page_items);
            if end {
                reached_end = true;
                break;
            }
        }
        let head = head(&tx)?;
        tx.commit()?;
        let coverage = if missing || stopped || !reached_end {
            ActiveCoverage::Incomplete
        } else {
            ActiveCoverage::Complete
        };
        let capacity_release_allowed = release_allowed(coverage, meta.retains_attempt_capacity);
        Ok(ActiveWorkRun {
            incarnation: meta.incarnation,
            inventory_revision: meta.inventory_revision,
            coverage,
            capacity_release_allowed,
            rows_read: account.rows,
            fullscan_steps: account.fullscan,
            head,
            items,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_version(db: &Connection) -> u32 {
        db.query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }

    fn binding_json(task: &str) -> String {
        let id = format!("task:{task}");
        format!(
            r#"{{"id":"{id}","task":"{task}","revision":1,"source_path":null,"source_digest":null,"session_source_digest":null,"verification":"unverified","identity":{{}}}}"#
        )
    }

    fn insert_task(db: &Connection, task: &str) {
        db.execute(
            "INSERT INTO tasks(id,revision,state,title,active_attempt) VALUES(?1,1,'queued',?1,NULL)",
            [task],
        )
        .unwrap();
    }

    fn insert_binding(db: &Connection, task: &str) {
        let payload = binding_json(task);
        let hash = format!("{:x}", Sha256::digest(payload.as_bytes()));
        db.execute(
            "INSERT INTO runtime_bindings(id,task_id,revision,source_path,payload,payload_hash) VALUES(?1,?2,1,NULL,?3,?4)",
            params![format!("task:{task}"), task, payload, hash],
        )
        .unwrap();
    }

    fn insert_attempt(db: &Connection, task: &str, retained: bool) {
        db.execute(
            "INSERT INTO attempts(id,task_id,revision,state,snapshot,reservation,termination_observed) VALUES(?1,?2,1,?3,NULL,?1,?4)",
            params![format!("attempt-{task}"), task, if retained { "running" } else { "completed" }, i64::from(!retained)],
        )
        .unwrap();
    }

    fn seed_pair(db: &Connection, task: &str, retained: bool) {
        insert_task(db, task);
        insert_attempt(db, task, retained);
        insert_binding(db, task);
    }

    #[test]
    fn create_ends_at_41_and_upgrade_from_40_reaches_41() {
        let fresh = tempfile::tempdir().unwrap();
        let created = SqliteStore::create(&fresh.path().join("state.db")).unwrap();
        assert_eq!(user_version(&created.connection), 41);
        assert_eq!(
            created
                .connection
                .query_row("SELECT schema_version FROM store_meta", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            41
        );
        for table in ["active_work_meta", "active_work_index"] {
            let sql: String = created
                .connection
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(sql.contains("STRICT"), "{table}");
        }
        let indexed: i64 = created
            .connection
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='index' AND name='attempts_retained_by_id'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(indexed, 1);
        let open_fn = include_str!("mod.rs")
            .split("pub fn open")
            .nth(1)
            .unwrap()
            .split("pub fn integrity_check")
            .next()
            .unwrap();
        assert!(!open_fn.contains("upgrade_v1"));
        assert!(!open_fn.contains("0041_active_work"));
        drop(created);

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.db");
        let db = SqliteStore::create(&path).unwrap();
        db.connection
            .execute("INSERT INTO tasks(id,revision,state,title,active_attempt) VALUES('kept',1,'draft','kept',NULL)", [])
            .unwrap();
        insert_binding(&db.connection, "kept");
        let payload: String = db
            .connection
            .query_row(
                "SELECT payload FROM runtime_bindings WHERE id='task:kept'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        db.connection
            .execute_batch(
                "DROP INDEX IF EXISTS attempts_retained_by_id; DROP TABLE IF EXISTS active_work_index; DROP TABLE IF EXISTS active_work_meta; UPDATE store_meta SET schema_version=40; PRAGMA user_version=40;",
            )
            .unwrap();
        drop(db);
        let mut db = SqliteStore::open(&path).unwrap();
        assert_eq!(user_version(&db.connection), 40);
        assert!(matches!(
            db.import_legacy(&"ab".repeat(32), &[], &[]),
            Err(StoreError::UnsupportedSchema(40))
        ));
        db.upgrade_v1().unwrap();
        assert_eq!(user_version(&db.connection), 41);
        let after: String = db
            .connection
            .query_row(
                "SELECT payload FROM runtime_bindings WHERE id='task:kept'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(after, payload);
        let title: String = db
            .connection
            .query_row("SELECT title FROM tasks WHERE id='kept'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(title, "kept");
        drop(db);
        let reopened = SqliteStore::open(&path).unwrap();
        assert_eq!(user_version(&reopened.connection), 41);
    }

    #[test]
    fn empty_inventory_is_complete_and_a_missing_page_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SqliteStore::create(&dir.path().join("state.db")).unwrap();
        let empty = db.reconcile_active_work(None).unwrap();
        assert_eq!(empty.coverage, ActiveCoverage::Complete);
        assert!(empty.items.is_empty());
        assert!(empty.capacity_release_allowed);
        for index in 0..3 {
            let task = format!("idle-{index}");
            insert_task(&db.connection, &task);
            insert_binding(&db.connection, &task);
        }
        // Raw inserts do not bump the retained-attempt fingerprint.
        invalidate(&db.connection).unwrap();
        let primed = db.reconcile_active_work(None).unwrap();
        assert_eq!(primed.items.len(), 3);
        db.connection
            .execute("DELETE FROM active_work_index WHERE ordinal=0", [])
            .unwrap();
        let missing = db.reconcile_active_work(None).unwrap();
        assert_eq!(missing.coverage, ActiveCoverage::Incomplete);
        assert!(!missing.capacity_release_allowed);
        let jumped = db.active_work_page(Some(10_000)).unwrap();
        assert_eq!(jumped.coverage, ActiveCoverage::Incomplete);
        assert!(jumped.items.is_empty());
        assert!(!jumped.capacity_release_allowed);
    }

    #[test]
    fn one_hundred_twenty_nine_active_bindings_page_instead_of_refusing() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SqliteStore::create(&dir.path().join("state.db")).unwrap();
        for index in 0..129 {
            let task = format!("b-{index:03}");
            insert_task(&db.connection, &task);
            insert_binding(&db.connection, &task);
        }
        let first = db.active_work_page(None).unwrap();
        assert_eq!(first.items.len(), ACTIVE_WORK_PAGE);
        assert_eq!(first.coverage, ActiveCoverage::Incomplete);
        assert!(!first.capacity_release_allowed);
        let stopped = db.reconcile_active_work(Some(1)).unwrap();
        assert_eq!(stopped.coverage, ActiveCoverage::Incomplete);
        assert_eq!(stopped.items.len(), ACTIVE_WORK_PAGE);
        assert!(!stopped.capacity_release_allowed);
        let run = db.reconcile_active_work(None).unwrap();
        assert_eq!(run.coverage, ActiveCoverage::Complete);
        assert_eq!(run.items.len(), 129);
        assert!(run.capacity_release_allowed);
        assert!(!format!("{run:?}").contains("more than 128 runtime bindings"));
    }

    #[test]
    fn dropped_projection_rebuilds_the_same_active_set() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SqliteStore::create(&dir.path().join("state.db")).unwrap();
        seed_pair(&db.connection, "live-a", true);
        seed_pair(&db.connection, "live-b", true);
        seed_pair(&db.connection, "old", false);
        let first = db.reconcile_active_work(None).unwrap();
        let ids: Vec<_> = first
            .items
            .iter()
            .map(|item| item.binding.id.clone())
            .collect();
        assert_eq!(
            ids,
            vec!["task:live-a".to_string(), "task:live-b".to_string()]
        );
        let incarnation = first.incarnation.clone();
        db.connection
            .execute("DELETE FROM active_work_index", [])
            .unwrap();
        db.connection
            .execute(
                "UPDATE active_work_meta SET projection_revision=?1",
                [EMPTY_REVISION],
            )
            .unwrap();
        let rebuilt = db.reconcile_active_work(None).unwrap();
        let again: Vec<_> = rebuilt
            .items
            .iter()
            .map(|item| item.binding.id.clone())
            .collect();
        assert_eq!(again, ids);
        assert_eq!(rebuilt.incarnation, incarnation);
        assert_eq!(rebuilt.coverage, ActiveCoverage::Complete);
    }

    #[test]
    fn reconcile_covers_sixty_four_active_among_ten_thousand_retired() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SqliteStore::create(&dir.path().join("state.db")).unwrap();
        {
            let tx = db.connection.transaction().unwrap();
            let mut task_stmt = tx
                .prepare("INSERT INTO tasks(id,revision,state,title,active_attempt) VALUES(?1,1,'queued',?1,NULL)")
                .unwrap();
            let mut attempt_stmt = tx
                .prepare("INSERT INTO attempts(id,task_id,revision,state,snapshot,reservation,termination_observed) VALUES(?1,?2,1,?3,NULL,?1,?4)")
                .unwrap();
            let mut binding_stmt = tx
                .prepare("INSERT INTO runtime_bindings(id,task_id,revision,source_path,payload,payload_hash) VALUES(?1,?2,1,NULL,?3,?4)")
                .unwrap();
            for index in 0..64 {
                let task = format!("k-{index:02}");
                task_stmt.execute([&task]).unwrap();
                attempt_stmt
                    .execute(params![format!("attempt-{task}"), &task, "running", 0])
                    .unwrap();
                let payload = binding_json(&task);
                let hash = format!("{:x}", Sha256::digest(payload.as_bytes()));
                binding_stmt
                    .execute(params![format!("task:{task}"), &task, payload, hash])
                    .unwrap();
            }
            for index in 0..10_000 {
                let task = format!("r-{index:05}");
                task_stmt.execute([&task]).unwrap();
                attempt_stmt
                    .execute(params![format!("attempt-{task}"), &task, "completed", 1])
                    .unwrap();
                let payload = binding_json(&task);
                let hash = format!("{:x}", Sha256::digest(payload.as_bytes()));
                binding_stmt
                    .execute(params![format!("task:{task}"), &task, payload, hash])
                    .unwrap();
            }
            drop(binding_stmt);
            drop(attempt_stmt);
            drop(task_stmt);
            tx.commit().unwrap();
        }
        let total: i64 = db
            .connection
            .query_row("SELECT count(*) FROM runtime_bindings", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(total, 10_064);
        let _prime = db.reconcile_active_work(None).unwrap();
        let hot = db.reconcile_active_work(None).unwrap();
        assert_eq!(hot.coverage, ActiveCoverage::Complete);
        assert_eq!(hot.items.len(), 64);
        assert!(hot.items.iter().all(|item| item.retains_capacity));
        assert!(!hot.capacity_release_allowed);
        assert!(
            hot.rows_read <= ACTIVE_WORK_PAGE as u64 * 8 && hot.rows_read < 10_064,
            "hot rows {} fullscan {}",
            hot.rows_read,
            hot.fullscan_steps
        );
        // SQLite may count the retained-attempt index walk as fullscan steps.
        // It must stay on the active page, not the retired history.
        assert!(
            hot.fullscan_steps <= ACTIVE_WORK_PAGE as i64 * 4,
            "fullscan {} rows {}",
            hot.fullscan_steps,
            hot.rows_read
        );
        assert!(
            hot.items
                .iter()
                .all(|item| !item.binding.id.starts_with("task:r-"))
        );
    }

    fn insert_coordinator(db: &Connection) {
        let payload = r#"{"id":"coordinator","task":null,"revision":1,"source_path":null,"source_digest":null,"session_source_digest":null,"verification":"unverified","identity":{}}"#;
        let hash = format!("{:x}", Sha256::digest(payload.as_bytes()));
        db.execute(
            "INSERT INTO runtime_bindings(id,task_id,revision,source_path,payload,payload_hash) VALUES('coordinator',NULL,1,NULL,?1,?2)",
            params![payload, hash],
        )
        .unwrap();
    }

    #[test]
    fn retained_attempt_blocks_release_without_an_indexed_binding() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SqliteStore::create(&dir.path().join("state.db")).unwrap();
        insert_task(&db.connection, "held");
        insert_attempt(&db.connection, "held", true);
        let empty = db.reconcile_active_work(None).unwrap();
        assert!(empty.items.is_empty());
        assert_eq!(empty.coverage, ActiveCoverage::Complete);
        assert!(!empty.capacity_release_allowed);

        let dir = tempfile::tempdir().unwrap();
        let mut db = SqliteStore::create(&dir.path().join("state.db")).unwrap();
        insert_task(&db.connection, "held");
        insert_attempt(&db.connection, "held", true);
        insert_coordinator(&db.connection);
        let coordinator = db.reconcile_active_work(None).unwrap();
        assert_eq!(coordinator.coverage, ActiveCoverage::Complete);
        assert_eq!(coordinator.items.len(), 1);
        assert_eq!(coordinator.items[0].binding.id, "coordinator");
        assert!(!coordinator.items[0].retains_capacity);
        assert!(!coordinator.capacity_release_allowed);
    }

    #[test]
    fn terminated_attempt_commit_removes_the_binding_from_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SqliteStore::create(&dir.path().join("state.db")).unwrap();
        insert_task(&db.connection, "gone");
        insert_binding(&db.connection, "gone");
        let primed = db.reconcile_active_work(None).unwrap();
        assert_eq!(primed.items.len(), 1);
        db.commit(Commit {
            expected_head: primed.head,
            mutations: vec![Mutation::Attempt {
                expected: None,
                next: Attempt {
                    id: AttemptId::new("attempt-gone").unwrap(),
                    task: TaskId::new("gone").unwrap(),
                    revision: 1,
                    state: AttemptState::Completed,
                    snapshot: None,
                    reservation: "attempt-gone".into(),
                    termination_observed: true,
                },
            }],
        })
        .unwrap();
        let after = db.reconcile_active_work(None).unwrap();
        assert!(after.items.is_empty());
        assert_eq!(after.coverage, ActiveCoverage::Complete);
    }

    #[test]
    fn index_keeps_every_retained_attempt_and_prefers_the_active_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SqliteStore::create(&dir.path().join("state.db")).unwrap();
        insert_task(&db.connection, "z");
        insert_binding(&db.connection, "z");
        for name in ["attempt-z-a", "attempt-z-b"] {
            db.connection
                .execute(
                    "INSERT INTO attempts(id,task_id,revision,state,snapshot,reservation,termination_observed) VALUES(?1,'z',1,'running',NULL,?1,0)",
                    [name],
                )
                .unwrap();
        }
        db.connection
            .execute(
                "UPDATE tasks SET active_attempt='attempt-z-b' WHERE id='z'",
                [],
            )
            .unwrap();
        let run = db.reconcile_active_work(None).unwrap();
        assert_eq!(run.items.len(), 1);
        assert_eq!(
            run.items[0].attempt_ids,
            vec!["attempt-z-b".to_string(), "attempt-z-a".to_string()]
        );
        assert!(!run.capacity_release_allowed);
    }

    #[test]
    fn owned_binding_stays_active_after_every_attempt_is_terminated() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SqliteStore::create(&dir.path().join("state.db")).unwrap();
        seed_pair(&db.connection, "old", false);
        let owned = RuntimeOwnership {
            binding: "task:old".into(),
            revision: 1,
            binding_revision: 1,
            identity_digest: "ab".repeat(32),
            origin: "adopted".into(),
            attempt: Some(AttemptId::new("attempt-old").unwrap()),
            session: None,
            worktree: None,
            agent: None,
            config_digest: None,
            observed_unix_ms: 1,
        };
        let payload = serde_json::to_string(&owned).unwrap();
        let hash = format!("{:x}", Sha256::digest(payload.as_bytes()));
        db.connection
            .execute(
                "INSERT INTO runtime_ownership(binding_id,revision,binding_revision,attempt_id,payload,payload_hash) VALUES(?1,1,1,?2,?3,?4)",
                params!["task:old", "attempt-old", payload, hash],
            )
            .unwrap();
        let run = db.reconcile_active_work(None).unwrap();
        assert_eq!(run.items.len(), 1);
        assert_eq!(run.items[0].binding.id, "task:old");
        assert!(!run.items[0].retains_capacity);
        assert!(run.capacity_release_allowed);
    }
}

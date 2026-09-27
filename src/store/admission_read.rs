//! Scheduling projections: scoped reads, never a partial administrative Snapshot.
use super::*;
use rusqlite::OptionalExtension;

pub(crate) const PAGE_SIZE: usize = 64;

pub(crate) struct Header {
    pub head: u64,
    pub policy: SchedulerPolicy,
    pub control: ProjectControl,
    pub budget: Option<VersionedReference>,
    pub retained: u64,
}
#[derive(Clone)]
pub(crate) struct Cursor {
    score: i64,
    sequence: u64,
    task: String,
    rank_time: i64,
}
pub(crate) struct Entry {
    pub task: Task,
    pub score: i64,
    pub binding: Option<RuntimeBinding>,
}
pub(crate) struct Page {
    pub entries: Vec<Entry>,
    pub next: Option<Cursor>,
}

pub(super) fn attempt_count(db: &Connection, task: &str, cap: u32) -> Result<u64> {
    db.query_row(
        "SELECT count(*) FROM (SELECT id FROM attempts WHERE task_id=?1 LIMIT ?2)",
        params![task, cap],
        |r| r.get(0),
    )
    .map_err(StoreError::from)
}

pub(super) fn queue_record_with_budget(db:&Connection,task:&str,budget:Option<&read_budget::ReadBudget>)->Result<QueueRecord> {
    let row: Option<(i32, i64, u64)> = db
        .query_row(
            "SELECT priority,enqueued_unix_ms,enqueue_sequence FROM task_queue WHERE task_id=?1",
            [task],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let (priority, enqueued_unix_ms, enqueue_sequence) = row.ok_or(StoreError::Conflict)?;
    let mut stmt = db.prepare("SELECT predecessor_id,requirement FROM task_dependencies WHERE task_id=?1 ORDER BY predecessor_id LIMIT 257")?;
    let mut rows = stmt.query([task])?;
    let mut dependencies = Vec::new();
    while let Some(r) = rows.next()? {
        if let Some(budget)=budget {budget.row(r,&[])?;}
        if dependencies.len() == 256 {
            return Err(StoreError::Limit("task dependency limit exceeded".into()));
        }
        let predecessor = TaskId::new(r.get::<_, String>(0)?).map_err(StoreError::Corrupt)?;
        let requirement = match r.get::<_, String>(1)?.as_str() {
            "verified_result" => DependencyRequirement::VerifiedResult,
            "integrated_commit" => DependencyRequirement::IntegratedCommit,
            "integration_candidate" => DependencyRequirement::IntegrationCandidate,
            "landed_commit" => DependencyRequirement::LandedCommit,
            _ => return Err(StoreError::Corrupt("unknown dependency requirement".into())),
        };
        dependencies.push(Dependency {
            predecessor,
            requirement,
        });
    }
    Ok(QueueRecord {
        task: TaskId::new(task).map_err(StoreError::Corrupt)?,
        priority,
        enqueued_unix_ms,
        enqueue_sequence,
        dependencies,
    })
}

fn unused_binding(db: &Connection, task: &str, budget: Option<&read_budget::ReadBudget>) -> Result<Option<RuntimeBinding>> {
    let mut stmt =
        db.prepare("SELECT id FROM runtime_bindings WHERE task_id=?1 ORDER BY id LIMIT 65")?;
    let mut rows=stmt.query([task])?;let mut ids=Vec::new();
    while let Some(row)=rows.next()? {
        if let Some(budget)=budget {budget.row(row,&[])?;}
        ids.push(row.get::<_,String>(0)?);
    }
    if ids.len() > 64 {
        return Err(StoreError::Limit(
            "task runtime binding limit exceeded".into(),
        ));
    }
    for id in ids {
        let binding = super::runtime::read_binding(db, &id, budget)?.ok_or(StoreError::Conflict)?;
        if !binding.identity.pane_id.is_empty()
            || !binding.identity.tab_id.is_empty()
            || !binding.identity.machine.is_empty()
            || !binding.identity.worktree_path.is_empty()
        {
            continue;
        }
        if super::ownership::read_binding(db, &id, budget)?
            .is_some_and(|o| o.attempt.is_some() || o.session.is_some() || o.agent.is_some())
        {
            continue;
        }
        return Ok(Some(binding));
    }
    Ok(None)
}

impl SqliteStore {
    #[cfg(test)]
    pub(crate) fn admission_cursor(&self, header: &Header) -> Result<Option<Cursor>> {
        self.admission_cursor_with_budget(header, None)
    }
    pub(crate) fn admission_cursor_with_budget(&self, header: &Header, budget: Option<&read_budget::ReadBudget>) -> Result<Option<Cursor>> {
        let mut statement = self.connection.prepare("SELECT score,enqueue_sequence,task_id,rank_time FROM admission_scan_cursor WHERE singleton=1 AND control_epoch=?1 AND policy_revision=?2")?;
        let mut rows = statement.query(params![header.control.epoch, header.policy.revision])?;
        let Some(row) = rows.next()? else { return Ok(None); };
        if let Some(budget) = budget { budget.row(row, &[])?; }
        let task: String = row.get(2)?;
        TaskId::new(&task).map_err(StoreError::Corrupt)?;
        Ok(Some(Cursor { score: row.get(0)?, sequence: row.get(1)?, task, rank_time: row.get(3)? }))
    }
    pub(crate) fn advance_admission_cursor(
        &mut self,
        header: &Header,
        next: Option<&Cursor>,
    ) -> Result<()> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if head(&tx)? != header.head {
            return Err(StoreError::Conflict);
        }
        if let Some(next) = next {
            tx.execute("INSERT INTO admission_scan_cursor VALUES(1,?1,?2,?3,?4,?5,?6) ON CONFLICT(singleton) DO UPDATE SET control_epoch=excluded.control_epoch,policy_revision=excluded.policy_revision,rank_time=excluded.rank_time,score=excluded.score,enqueue_sequence=excluded.enqueue_sequence,task_id=excluded.task_id",params![header.control.epoch,header.policy.revision,next.rank_time,next.score,next.sequence,next.task])?;
        } else {
            tx.execute("DELETE FROM admission_scan_cursor", [])?;
        }
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn admission_backlog_ages(&self) -> Result<(Option<i64>, Option<i64>)> {
        let verification = self.connection.query_row(
            "SELECT MIN(created_unix_ms) FROM pending_verification_work",
            [],
            |r| r.get(0),
        )?;
        let integration=self.connection.query_row("SELECT MIN(created_unix_ms) FROM integration_operations WHERE state IN ('effect_pending','candidate_prepared','validating')",[],|r|r.get(0))?;
        Ok((verification, integration))
    }

    #[cfg(test)]
    pub(crate) fn admission_header(&mut self) -> Result<Header> { self.admission_header_with_budget(None) }
    pub(crate) fn admission_header_with_budget(&mut self, read_budget:Option<&read_budget::ReadBudget>) -> Result<Header> {
        let tx = self.connection.transaction()?;
        check_schema(&tx)?;
        let version: u32 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 43 {
            return Err(StoreError::UnsupportedSchema(version));
        }
        let policy = super::scheduler::read_policy(&tx, read_budget)?;
        let control = super::control::read_with_budget(&tx,read_budget)?;
        let budget = super::budget::current_with_budget(&tx,read_budget)?
            .map(|p| p.reference())
            .transpose()
            .map_err(StoreError::Corrupt)?;
        // The policy maximum is 1,024; no larger retained set can admit work.
        let retained = tx.query_row("SELECT count(*) FROM (SELECT id FROM attempts WHERE termination_observed=0 LIMIT 1025)", [], |r| r.get(0))?;
        let value = Header {
            head: head(&tx)?,
            policy,
            control,
            budget,
            retained,
        };
        tx.commit()?;
        Ok(value)
    }

    #[cfg(test)]
    pub(crate) fn admission_page(
        &mut self,
        header: &Header,
        now: i64,
        after: Option<&Cursor>,
    ) -> Result<Page> { self.admission_page_with_budget(header,now,after,None) }
    pub(crate) fn admission_page_with_budget(&mut self,header:&Header,now:i64,after:Option<&Cursor>,budget:Option<&read_budget::ReadBudget>)->Result<Page> {
        let tx = self.connection.transaction()?;
        if head(&tx)? != header.head {
            return Err(StoreError::Conflict);
        }
        let mut stmt = tx.prepare(
            "SELECT t.id, ( (?5-q.enqueued_unix_ms)/60000 + q.priority ) AS score, q.enqueue_sequence
             FROM tasks t JOIN task_queue q ON q.task_id=t.id
             WHERE t.state='queued' AND t.active_attempt IS NULL AND q.enqueued_unix_ms<=?1
               AND (?2 IS NULL OR score<?2 OR (score=?2 AND (q.enqueue_sequence>?3 OR (q.enqueue_sequence=?3 AND t.id>?4))))
             ORDER BY score DESC,q.enqueue_sequence,t.id LIMIT 65")?;
        let rank_time = after.map_or(now, |c| c.rank_time);
        let mut rows = stmt.query(params![
            now,
            after.map(|c| c.score),
            after.map(|c| c.sequence),
            after.map(|c| c.task.as_str()),
            rank_time
        ])?;
        let mut cursors = Vec::new();
        while let Some(r) = rows.next()? {
            if let Some(budget)=budget {budget.row(r,&[])?;}
            cursors.push(Cursor {
                task: r.get(0)?,
                score: r.get(1)?,
                sequence: r.get(2)?,
                rank_time,
            });
        }
        let next = if cursors.len() > PAGE_SIZE {
            cursors.pop();
            cursors.last().cloned()
        } else {
            None
        };
        drop(rows);
        drop(stmt);
        let mut entries = Vec::new();
        for cursor in cursors {
            if attempt_count(&tx, &cursor.task, header.policy.max_attempts_per_task)?
                >= u64::from(header.policy.max_attempts_per_task)
            {
                continue;
            }
            let retained: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM attempts WHERE task_id=?1 AND termination_observed=0)",
                [&cursor.task],
                |r| r.get(0),
            )?;
            if retained {
                continue;
            }
            entries.push(Entry {
                task: super::read_task_with_budget(&tx, &cursor.task,budget)?,
                score: cursor.score,
                binding: unused_binding(&tx, &cursor.task,budget)?,
            });
        }
        tx.commit()?;
        Ok(Page { entries, next })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn world() -> (tempfile::TempDir, SqliteStore) {
        let temp = tempfile::tempdir().unwrap();
        let db = SqliteStore::create(&temp.path().join("state.db")).unwrap();
        db.connection.execute_batch("
            UPDATE scheduler_policy SET max_active_workers=64,max_attempts_per_task=3;
            WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM n WHERE x<129)
            INSERT INTO tasks(id,revision,state,title,active_attempt) SELECT printf('ready-%04d',x),1,'queued','ready',NULL FROM n;
            INSERT INTO task_queue(task_id,priority,enqueued_unix_ms,enqueue_sequence)
            SELECT id,0,1000,1 FROM tasks;
            WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM n WHERE x<9999)
            INSERT INTO tasks(id,revision,state,title,active_attempt) SELECT printf('retired-%05d',x),1,'succeeded','retired',NULL FROM n;
            INSERT INTO runtime_bindings(id,task_id,revision,source_path,payload,payload_hash)
            SELECT 'task:'||id,id,1,NULL,'{}',printf('%064d',0) FROM tasks WHERE id LIKE 'retired-%';
            WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM n WHERE x<1023)
            INSERT INTO attempts(id,task_id,revision,state,snapshot,reservation,termination_observed)
            SELECT printf('old-%04d',x),'retired-00000',1,'completed',NULL,printf('old:%04d',x),1 FROM n;
        ").unwrap();
        (temp, db)
    }
    #[test]
    fn pages_exclude_retired_history_and_cursor_survives_unrelated_events_and_clock_ticks() {
        let (temp, mut db) = world();
        let header = db.admission_header().unwrap();
        assert_eq!(header.retained, 0);
        let first = db.admission_page(&header, 2000, None).unwrap();
        assert_eq!(first.entries.len(), PAGE_SIZE);
        assert_eq!(first.entries[0].task.id.as_str(), "ready-0000");
        assert_eq!(first.entries.last().unwrap().task.id.as_str(), "ready-0063");
        assert!(first.entries.iter().all(|e| e.binding.is_none()));
        db.advance_admission_cursor(&header, first.next.as_ref())
            .unwrap();
        db.connection.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('task.noted','unrelated',1,1,'{}')",[]).unwrap();
        drop(db);
        let mut db = SqliteStore::open(&temp.path().join("state.db")).unwrap();
        let header = db.admission_header().unwrap();
        let cursor = db.admission_cursor(&header).unwrap().unwrap();
        let second = db.admission_page(&header, 62001, Some(&cursor)).unwrap();
        assert_eq!(second.entries.len(), PAGE_SIZE);
        assert_eq!(second.entries[0].task.id.as_str(), "ready-0064");
        let third = db
            .admission_page(&header, 63001, second.next.as_ref())
            .unwrap();
        assert_eq!(third.entries.len(), 2);
        assert_eq!(third.entries[0].task.id.as_str(), "ready-0128");
        assert!(third.next.is_none());
        db.advance_admission_cursor(&header, None).unwrap();
        assert!(db.admission_cursor(&header).unwrap().is_none());
        // Full admin inventory still diagnoses the deliberately corrupt retired
        // binding fixture. Scheduling never decodes those irrelevant payloads.
        assert!(matches!(
            db.read_snapshot(None),
            Err(StoreError::Corrupt(_))
        ));
    }
    #[test]
    fn saved_cursor_is_budgeted_and_validated_before_resuming_admission() {
        let (_temp, mut db) = world();
        let header = db.admission_header().unwrap();
        let first = db.admission_page(&header, 2000, None).unwrap();
        db.advance_admission_cursor(&header, first.next.as_ref()).unwrap();
        let budget = read_budget::ReadBudget::new(controlled::ReadControl::new(
            std::time::Instant::now() + std::time::Duration::from_secs(10),
            crate::runner::Cancellation::default(),
        ));
        let before = budget.remaining_units();
        assert!(db.admission_cursor_with_budget(&header, Some(&budget)).unwrap().is_some());
        assert!(budget.remaining_units() < before);
        db.connection.execute("UPDATE admission_scan_cursor SET task_id=CAST(zeroblob(16777217) AS TEXT)", []).unwrap();
        assert!(matches!(db.admission_cursor_with_budget(&header, Some(&budget)), Err(StoreError::Limit(_))));
        db.connection.execute("UPDATE admission_scan_cursor SET task_id='../invalid'", []).unwrap();
        assert!(matches!(db.admission_cursor_with_budget(&header, Some(&budget)), Err(StoreError::Corrupt(_))));
        // A cursor from an earlier control epoch is irrelevant, even if corrupt.
        db.connection.execute("UPDATE admission_scan_cursor SET control_epoch=control_epoch+1", []).unwrap();
        assert!(db.admission_cursor_with_budget(&header, Some(&budget)).unwrap().is_none());
    }
    #[test]
    fn page_refuses_changed_head_and_policy_or_epoch_resets_saved_cursor() {
        let (_temp, mut db) = world();
        let header = db.admission_header().unwrap();
        let first = db.admission_page(&header, 2000, None).unwrap();
        db.advance_admission_cursor(&header, first.next.as_ref())
            .unwrap();
        db.connection.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('task.noted','unrelated',1,1,'{}')",[]).unwrap();
        assert!(matches!(
            db.admission_page(&header, 2000, None),
            Err(StoreError::Conflict)
        ));
        db.connection
            .execute("UPDATE scheduler_policy SET revision=revision+1", [])
            .unwrap();
        let header = db.admission_header().unwrap();
        assert!(db.admission_cursor(&header).unwrap().is_none());
    }
}

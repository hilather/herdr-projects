use super::*;

#[derive(Debug,Serialize)]
pub struct WaitTurn {
    pub pending: bool,
    pub notified: usize,
}

impl SqliteStore {
    /// Start one successor after reevaluation of a terminal advisory wake.
    /// The predecessor and its notification remain immutable evidence.
    pub fn rearm_wait(&mut self, previous: &str, deadline: Option<i64>) -> Result<WaitRegistration> {
        self.rearm_wait_with_budget(previous, deadline, None)
    }
    pub(crate) fn rearm_wait_with_budget(&mut self, previous: &str, deadline: Option<i64>, budget: Option<&read_budget::ReadBudget>) -> Result<WaitRegistration> {
        if let Some(budget) = budget { budget.check()?; }
        if !identifier(previous) || deadline.is_some_and(|value| value < 0 || jiff::Timestamp::from_millisecond(value).is_err()) {
            return Err(invalid("invalid wait rearm request"));
        }
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_schema(&tx)?;
        let version: u32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version < 43 { return Err(StoreError::UnsupportedSchema(version)); }
        let parent: Option<(String, Option<String>, String, i64, i64)> = read_budget::optional(&tx,
            "SELECT task_id,attempt_id,condition,plan_revision,wake_requested FROM wait_conditions WHERE wait_id=?1",
            [previous], budget, &[], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)))?;
        let Some((task, attempt, condition, revision, woken)) = parent else {
            return Err(invalid("wait is not registered"));
        };
        if revision != integer(current_revision(&tx)?)? || woken != 1 {
            return Err(invalid("only a terminal wait in the current plan can be rearmed"));
        }
        let trigger=wait_triggers::load(&tx,previous,budget)?;
        let wait_id = sha256_hex(format!("wait-rearm\0{previous}\0{deadline:?}").as_bytes());
        let existing: Option<(String, i64)> = read_budget::optional(&tx,
            "SELECT w.wait_id,w.cursor_sequence FROM wait_rearms r JOIN wait_conditions w ON w.wait_id=r.successor WHERE r.predecessor=?1",
            [previous], budget, &[], |row| Ok((row.get(0)?,row.get(1)?)))?;
        if let Some((id, cursor_sequence)) = existing {
            if id != wait_id { return Err(invalid("wait already rearmed with a different deadline")); }
            tx.commit()?;
            return Ok(WaitRegistration { wait_id, cursor_sequence, already_registered: true });
        }
        tx.execute(
            "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('wait.registered',?1,1,1,?2)",
            params![wait_id,serde_json::json!({"task_id":task,"attempt_id":attempt,"condition":condition,"plan_revision":revision,"deadline_unix_ms":deadline,"predecessor":previous,"trigger":trigger}).to_string()])?;
        let cursor_sequence = i64::try_from(head(&tx)?).map_err(|_| invalid("wait cursor exceeds range"))?;
        tx.execute(
            "INSERT INTO wait_conditions(wait_id,task_id,attempt_id,condition,plan_revision,cursor_sequence,state,replayed_through,wake_requested,created_unix_ms,deadline_unix_ms) VALUES(?1,?2,?3,?4,?5,?6,'waiting',NULL,0,?7,?8)",
            params![wait_id,task,attempt,condition,revision,cursor_sequence,jiff::Timestamp::now().as_millisecond(),deadline])?;
        tx.execute("INSERT INTO wait_rearms(predecessor,successor) VALUES(?1,?2)", params![previous,wait_id])?;
        if let Some(trigger)=&trigger {wait_triggers::insert(&tx,&wait_id,trigger,budget)?;}
        // As with initial registration, completion before subscription must not
        // be lost. Only currently relevant retained evidence requests a wake.
        if let Some((kind, entity, sequence)) = current_wait_evidence(&tx,&task,attempt.as_deref(),&condition,trigger.as_ref(),budget)? {
            if relevant_wait_event(&tx,&task,attempt.as_deref(),&condition,trigger.as_ref(),&kind,&entity,sequence,budget)? {
                tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('wait.wake',?1,1,1,?2)",
                    params![wait_id,serde_json::json!({"source_sequence":sequence,"proved":false}).to_string()])?;
            }
        }
        if let Some(budget) = budget { budget.check()?; }
        tx.commit()?;
        Ok(WaitRegistration { wait_id, cursor_sequence, already_registered: false })
    }

    pub(crate) fn service_waits(&mut self,budget:&read_budget::ReadBudget)->Result<WaitTurn> {
        let version:u32=self.connection.query_row("PRAGMA user_version",[],|row|row.get(0))?;
        if version<43 {return Ok(WaitTurn{pending:false,notified:0});}
        let revision=current_revision(&self.connection)?;
        let after:Option<String>=self.connection.query_row(
            "SELECT wait_id FROM wait_service_cursor WHERE singleton=1 AND plan_revision=?1",
            [integer(revision)?],|row|row.get(0)).optional()?;
        let mut ids=Vec::new();
        for cursor in [after.as_deref().unwrap_or(""),""] {
            let mut stmt=self.connection.prepare("SELECT wait_id FROM wait_conditions WHERE wake_requested=0 AND plan_revision=?1 AND wait_id>?2 ORDER BY wait_id LIMIT 8")?;
            let mut rows=stmt.query(params![integer(revision)?,cursor])?;
            while let Some(row)=rows.next()? {budget.row(row,&[])?;ids.push(row.get::<_,String>(0)?);}
            if !ids.is_empty()||cursor.is_empty(){break;}
        }
        let mut notified=0;
        for id in &ids {
            budget.check()?;
            // Advisory rotation survives restart. Advancing before replay means
            // one corrupt subscription cannot starve every later registration;
            // a crash leaves its events available on the next rotation.
            self.connection.execute("INSERT INTO wait_service_cursor VALUES(1,?1,?2) ON CONFLICT(singleton) DO UPDATE SET plan_revision=excluded.plan_revision,wait_id=excluded.wait_id",params![integer(revision)?,id])?;
            let replay=self.replay_wait_with_budget(id,Some(budget))?;
            notified+=usize::from(replay.wake_requested&&!replay.already_replayed);
        }
        Ok(WaitTurn{pending:!ids.is_empty(),notified})
    }
}

pub fn register_project_wait(project:&Path,task:&str,attempt:Option<&str>,condition:&str)->anyhow::Result<WaitRegistration> {
    register_project_wait_with_deadline(project,task,attempt,condition,None)
}
pub fn register_project_wait_with_deadline(project:&Path,task:&str,attempt:Option<&str>,condition:&str,deadline:Option<i64>)->anyhow::Result<WaitRegistration> {
    register_project_wait_with_trigger(project,task,attempt,condition,deadline,None)
}
pub fn register_project_wait_with_trigger(project:&Path,task:&str,attempt:Option<&str>,condition:&str,deadline:Option<i64>,trigger:Option<&WaitTrigger>)->anyhow::Result<WaitRegistration> {
    let _guard=crate::migration::runtime_mutation(project)?;
    let control=super::super::controlled::ReadControl::new(std::time::Instant::now()+Duration::from_secs(2),crate::runner::Cancellation::default());
    Ok(crate::migration::open_active_scoped(project,control)?.register_wait(task,attempt,condition,deadline,trigger)?)
}
pub fn replay_project_wait(project:&Path,id:&str)->anyhow::Result<WaitReplay> {
    let _guard=crate::migration::runtime_mutation(project)?;
    let control=super::super::controlled::ReadControl::new(std::time::Instant::now()+Duration::from_secs(2),crate::runner::Cancellation::default());
    Ok(crate::migration::open_active_scoped(project,control)?.replay_wait(id)?)
}
pub fn rearm_project_wait(project:&Path,id:&str,deadline:Option<i64>)->anyhow::Result<WaitRegistration> {
    let _guard=crate::migration::runtime_mutation(project)?;
    let control=super::super::controlled::ReadControl::new(std::time::Instant::now()+Duration::from_secs(2),crate::runner::Cancellation::default());
    Ok(crate::migration::open_active_scoped(project,control)?.rearm_wait(id,deadline)?)
}
pub fn request_project_replan(project:&Path,feedback:&str)->anyhow::Result<ReplanDecision> {
    let _guard=crate::migration::runtime_mutation(project)?;
    Ok(crate::migration::open_active(project)?.request_replan(feedback)?)
}
pub fn service_project_waits(project:&Path)->anyhow::Result<WaitTurn> {
    let _guard=crate::migration::runtime_mutation(project)?;
    let control=super::super::controlled::ReadControl::new(std::time::Instant::now()+Duration::from_secs(2),crate::runner::Cancellation::default());
    Ok(crate::migration::open_active_scoped(project,control)?.service_waits()?)
}

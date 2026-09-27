use super::*;

#[derive(Debug,Serialize,PartialEq,Eq)]
pub struct AutoReplanControl { pub revision:u64, pub enabled:bool }
#[derive(Debug,Default,Serialize)]
pub struct ReplanTurn { pub pending:bool, pub processed:usize }

pub(super) fn enabled(db:&Connection)->Result<bool> {
    Ok(db.query_row("SELECT enabled=1 AND EXISTS(SELECT 1 FROM project_control WHERE singleton=1 AND state='active') FROM auto_replan_control WHERE singleton=1",[],|row|row.get(0))?)
}
pub(super) fn available(db:&Connection,id:&str,now:i64)->Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM feedback_items WHERE feedback_id=?1 AND state IN ('open','claimed')) AND NOT EXISTS(SELECT 1 FROM feedback_claims WHERE feedback_id=?1 AND state='active' AND lease_until_ms>?2 AND owner!=?3)",params![id,now,REPLAN_OWNER],|row|row.get(0))?)
}
pub(super) fn link(db:&Connection,feedback:&str,decision:&ReplanDecision)->Result<()> {
    let version:u32=db.query_row("PRAGMA user_version",[],|row|row.get(0))?;
    if version<43 {return Ok(());}
    let id=match decision {ReplanDecision::Automatic{replan_id,..}|ReplanDecision::Escalated{replan_id,..}=>replan_id};
    db.execute("INSERT INTO replan_feedback_links(feedback_id,replan_id) VALUES(?1,?2)",params![feedback,id])?;
    db.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('replan.feedback_linked',?1,1,1,?2)",params![feedback,serde_json::json!({"replan_id":id}).to_string()])?;
    Ok(())
}

impl SqliteStore {
    pub fn set_auto_replans(&mut self,expected_head:u64,on:bool)->Result<AutoReplanControl> {
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;check_schema(&tx)?;
        let version:u32=tx.query_row("PRAGMA user_version",[],|row|row.get(0))?;
        if version<43 {return Err(StoreError::UnsupportedSchema(version));}
        if head(&tx)?!=expected_head {return Err(StoreError::Conflict);}
        let (revision,old):(u64,bool)=tx.query_row("SELECT revision,enabled FROM auto_replan_control WHERE singleton=1",[],|row|Ok((row.get(0)?,row.get(1)?)))?;
        let revision=if old==on {revision}else {
            let next=revision.checked_add(1).ok_or(StoreError::Conflict)?;
            tx.execute("UPDATE auto_replan_control SET revision=?1,enabled=?2 WHERE singleton=1",params![integer(next)?,on])?;
            tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('replan.control_changed','project',?1,1,?2)",params![integer(next)?,serde_json::json!({"enabled":on}).to_string()])?;
            next
        };
        tx.commit()?;Ok(AutoReplanControl{revision,enabled:on})
    }
    pub(crate) fn service_replans(&mut self,budget:&read_budget::ReadBudget)->Result<ReplanTurn> {
        budget.check()?;
        let version:u32=self.connection.query_row("PRAGMA user_version",[],|row|row.get(0))?;
        if version<43 || !enabled(&self.connection)? {return Ok(ReplanTurn::default());}
        let after:Option<String>=read_budget::optional(&self.connection,"SELECT feedback_id FROM replan_service_cursor WHERE singleton=1",[],Some(budget),&[],|row|row.get(0))?;
        let mut ids=Vec::new();
        for cursor in [after.as_deref().unwrap_or(""),""] {
            let mut stmt=self.connection.prepare("SELECT feedback_id FROM replan_pending_feedback WHERE feedback_id>?1 ORDER BY feedback_id LIMIT 8")?;
            let mut rows=stmt.query([cursor])?;
            while let Some(row)=rows.next()? {budget.row(row,&[])?;ids.push(row.get::<_,String>(0)?);}
            if !ids.is_empty()||cursor.is_empty(){break;}
        }
        let mut processed=0;
        for id in &ids {
            budget.check()?;
            // Persist rotation before processing so a poisoned record or a live
            // lease cannot monopolize the bounded service after restart.
            self.connection.execute("INSERT INTO replan_service_cursor VALUES(1,?1) ON CONFLICT(singleton) DO UPDATE SET feedback_id=excluded.feedback_id",[id])?;
            if !available(&self.connection,id,jiff::Timestamp::now().as_millisecond())? {continue;}
            self.request_replan_selected(id,Some(budget),true)?;
            processed+=1;
        }
        Ok(ReplanTurn{pending:!ids.is_empty(),processed})
    }
}

pub fn set_project_auto_replans(project:&Path,expected_head:u64,on:bool)->anyhow::Result<AutoReplanControl> {
    let _guard=crate::migration::runtime_mutation(project)?;
    let control=super::super::controlled::ReadControl::new(std::time::Instant::now()+Duration::from_secs(2),Default::default());
    Ok(crate::migration::open_active_scoped(project,control)?.set_auto_replans(expected_head,on)?)
}
pub fn service_project_replans(project:&Path)->anyhow::Result<ReplanTurn> {
    let _guard=crate::migration::runtime_mutation(project)?;
    let control=super::super::controlled::ReadControl::new(std::time::Instant::now()+Duration::from_secs(2),Default::default());
    Ok(crate::migration::open_active_scoped(project,control)?.service_replans()?)
}

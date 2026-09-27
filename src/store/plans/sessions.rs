//! Immutable planner inputs. Sessions carry context, never launch authority.
use super::*;

#[derive(Debug,Clone,Serialize,Deserialize,PartialEq,Eq)]
#[serde(deny_unknown_fields)]
pub struct PlannerEventInput {
    pub sequence:u64,
    pub kind:String,
    pub entity:String,
    pub revision:u64,
    pub payload_version:u64,
    pub payload:String,
}

#[derive(Debug,Clone,Serialize,Deserialize,PartialEq,Eq)]
#[serde(deny_unknown_fields)]
pub struct PlannerInput {
    pub version:u32,
    pub session_id:String,
    pub project_store:String,
    pub store_incarnation:String,
    pub parent_plan_revision:u64,
    pub input_cursor:u64,
    pub intent:String,
    pub intent_digest:String,
    pub evidence:Vec<PlannerEventInput>,
}

#[derive(Debug,Serialize)]
pub struct PlannerSession {
    pub input_digest:String,
    pub input:PlannerInput,
}

#[derive(Debug,Clone,Deserialize,Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PlannerBinding {
    pub session_id:String,
    pub input_digest:String,
    pub rationale:String,
}

fn schema(db:&Connection)->Result<()> {
    check_schema(db)?;
    let version:u32=db.query_row("PRAGMA user_version",[],|r|r.get(0))?;
    if version<43 {return Err(StoreError::UnsupportedSchema(version));}
    Ok(())
}

fn load(db:&Connection,id:&str,budget:Option<&read_budget::ReadBudget>)->Result<Option<PlannerSession>> {
    schema(db)?;
    let row:Option<(Vec<u8>,String)>=read_budget::optional(db,
        "SELECT input,input_digest FROM planner_sessions WHERE session_id=?1",[id],budget,&[],|r|Ok((r.get(0)?,r.get(1)?)))?;
    let Some((raw,digest))=row else{return Ok(None);};
    if raw.len()>PROPOSAL_LIMIT || sha256_hex(&raw)!=digest {return Err(StoreError::Corrupt("planner input digest mismatch".into()));}
    let input:PlannerInput=serde_json::from_slice(&raw).map_err(|_|StoreError::Corrupt("invalid planner input".into()))?;
    if input.version!=1 || input.session_id!=id || sha256_hex(input.intent.as_bytes())!=input.intent_digest {
        return Err(StoreError::Corrupt("planner input identity mismatch".into()));
    }
    Ok(Some(PlannerSession{input_digest:digest,input}))
}

fn identity(db:&Connection,input:&PlannerInput,path:&str)->Result<()> {
    let incarnation:String=db.query_row("SELECT incarnation FROM active_work_meta WHERE singleton=1",[],|r|r.get(0))?;
    if input.project_store!=path || input.store_incarnation!=incarnation {return Err(StoreError::Conflict);}
    Ok(())
}

pub(super) fn validate_binding(db:&Connection,binding:&PlannerBinding,parent:u64,path:&str,budget:Option<&read_budget::ReadBudget>)->Result<()> {
    if !identifier(&binding.session_id) || binding.input_digest.len()!=64 || !plain(&binding.rationale,16*1024) {
        return Err(invalid("invalid planner session binding"));
    }
    let session=load(db,&binding.session_id,budget)?.ok_or_else(||invalid("planner session is missing"))?;
    identity(db,&session.input,path)?;
    if session.input_digest!=binding.input_digest || session.input.parent_plan_revision!=parent {
        return Err(StoreError::Conflict);
    }
    Ok(())
}

impl SqliteStore {
    pub(crate) fn create_planner_session(&mut self,id:&str,intent:&str,evidence:&[u64],parent:u64,cursor:u64,budget:&read_budget::ReadBudget)->Result<PlannerSession> {
        budget.check()?;
        if !identifier(id) || intent.trim().is_empty() || intent.len()>64*1024 || intent.contains('\0')
            || evidence.len()>64 || evidence.windows(2).any(|w|w[0]>=w[1])
            || evidence.iter().any(|sequence|*sequence==0 || *sequence>cursor) {
            return Err(invalid("invalid planner input: require bounded intent and at most 64 increasing evidence event IDs"));
        }
        let path=store_path(&self.connection)?.to_string_lossy().into_owned();
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema(&tx)?;
        if let Some(session)=load(&tx,id,Some(budget))? {
            identity(&tx,&session.input,&path)?;
            if session.input.intent!=intent || session.input.input_cursor!=cursor || session.input.parent_plan_revision!=parent
                || session.input.evidence.iter().map(|e|e.sequence).ne(evidence.iter().copied()) {return Err(StoreError::Conflict);}
            budget.check()?;tx.commit()?;return Ok(session);
        }
        if current_revision(&tx)?!=parent {return Err(StoreError::StalePlanParent(current_revision(&tx)?));}
        if head(&tx)?!=cursor {return Err(StoreError::Conflict);}
        let mut inputs=Vec::new();
        let mut retained_bytes=intent.len();
        for sequence in evidence {
            budget.check()?;
            let event=read_budget::optional(&tx,
                "SELECT sequence,kind,entity,revision,payload_version,payload FROM events WHERE sequence=?1 AND length(CAST(payload AS BLOB))<=262144",
                [integer(*sequence)?],Some(budget),&[],|r|Ok(PlannerEventInput{sequence:r.get(0)?,kind:r.get(1)?,entity:r.get(2)?,revision:r.get(3)?,payload_version:r.get(4)?,payload:r.get(5)?}))?
                .ok_or_else(||invalid("planner evidence event missing or oversized"))?;
            retained_bytes+=event.payload.len()+event.kind.len()+event.entity.len();
            if retained_bytes>PROPOSAL_LIMIT {return Err(invalid("planner input exceeds 256 KiB"));}
            inputs.push(event);
        }
        let input=PlannerInput{version:1,session_id:id.into(),project_store:path,
            store_incarnation:tx.query_row("SELECT incarnation FROM active_work_meta WHERE singleton=1",[],|r|r.get(0))?,
            parent_plan_revision:parent,input_cursor:cursor,intent:intent.into(),intent_digest:sha256_hex(intent.as_bytes()),evidence:inputs};
        let raw=serde_json::to_vec(&input).map_err(|e|invalid(&e.to_string()))?;
        if raw.len()>PROPOSAL_LIMIT {return Err(invalid("planner input exceeds 256 KiB"));}
        let digest=sha256_hex(&raw);
        tx.execute("INSERT INTO planner_sessions(session_id,input,input_digest) VALUES(?1,?2,?3)",params![id,raw,digest])?;
        tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('planner.session_created',?1,1,1,?2)",
            params![id,serde_json::json!({"input_digest":digest,"input_cursor":cursor,"parent_plan_revision":parent}).to_string()])?;
        budget.check()?;tx.commit()?;
        Ok(PlannerSession{input_digest:digest,input})
    }

    pub(crate) fn planner_session(&self,id:&str,budget:&read_budget::ReadBudget)->Result<PlannerSession> {
        budget.check()?;
        if !identifier(id) {return Err(invalid("invalid planner session ID"));}
        let session=load(&self.connection,id,Some(budget))?.ok_or_else(||invalid("planner session is missing"))?;
        identity(&self.connection,&session.input,&store_path(&self.connection)?.to_string_lossy())?;
        budget.check()?;Ok(session)
    }
}

pub fn create_project_planner_session(project:&Path,id:&str,intent_file:&Path,evidence:&[u64],parent:u64,cursor:u64)->anyhow::Result<PlannerSession> {
    let _guard=crate::migration::runtime_mutation(project)?;
    let bytes=crate::migration::read_plan_file(intent_file)?;
    let intent=std::str::from_utf8(&bytes)?;
    let control=controlled::ReadControl::new(std::time::Instant::now()+Duration::from_secs(2),Default::default());
    Ok(crate::migration::open_active_scoped(project,control)?.create_planner_session(id,intent,evidence,parent,cursor)?)
}

pub fn show_project_planner_session(project:&Path,id:&str)->anyhow::Result<PlannerSession> {
    let _guard=crate::migration::runtime_mutation(project)?;
    let control=controlled::ReadControl::new(std::time::Instant::now()+Duration::from_secs(2),Default::default());
    Ok(crate::migration::open_active_scoped(project,control)?.planner_session(id)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture()->(tempfile::TempDir,SqliteStore) {
        let root=tempfile::tempdir().unwrap();let db=SqliteStore::create(&root.path().join("state.db")).unwrap();(root,db)
    }
    fn budget()->read_budget::ReadBudget {
        read_budget::ReadBudget::new(controlled::ReadControl::new(std::time::Instant::now()+Duration::from_secs(5),Default::default()))
    }
    fn proposal(session:&PlannerSession)->Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"version":2,"planner":{"session_id":session.input.session_id,"input_digest":session.input_digest,"rationale":"Use the retained intent and evidence"},"contracts":[{"task_id":"task","text":"Deliver the requested change","dependencies":[]}]})).unwrap()
    }
    #[test]
    fn session_retains_exact_inputs_and_acceptance_across_restart() {
        let (_root,mut db)=fixture();
        db.connection.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('fixture.evidence','task',1,1,'{\"message\":\"untrusted text\"}')",[]).unwrap();
        let cursor=db.current_head().unwrap();
        let first=db.create_planner_session("session","User intent\nsecond line",&[cursor],0,cursor,&budget()).unwrap();
        assert_eq!(first.input.evidence[0].payload,"{\"message\":\"untrusted text\"}");
        let after=db.current_head().unwrap();
        assert_eq!(db.create_planner_session("session","User intent\nsecond line",&[cursor],0,cursor,&budget()).unwrap().input_digest,first.input_digest);
        assert_eq!(db.current_head().unwrap(),after);
        assert!(matches!(db.create_planner_session("session","changed",&[cursor],0,cursor,&budget()),Err(StoreError::Conflict)));
        let raw=proposal(&first);let accepted=db.apply_plan_proposal(&raw,0,"response").unwrap();
        let head=db.current_head().unwrap();let path=PathBuf::from(db.connection.path().unwrap());drop(db);
        let mut db=SqliteStore::open(&path).unwrap();
        assert_eq!(db.planner_session("session",&budget()).unwrap().input,first.input);
        let replay=db.apply_plan_proposal(&raw,0,"response").unwrap();assert!(replay.replayed);assert_eq!(replay.proposal_id,accepted.proposal_id);
        assert_eq!(db.current_head().unwrap(),head);
        for table in ["planner_sessions","planner_proposal_inputs","plan_proposals","plan_revisions"] {
            assert_eq!(db.connection.query_row(&format!("SELECT count(*) FROM {table}"),[],|r|r.get::<_,u64>(0)).unwrap(),1);
            assert!(db.connection.execute(&format!("DELETE FROM {table}"),[]).is_err());
        }
        assert_eq!(db.connection.query_row("SELECT count(*) FROM attempts",[],|r|r.get::<_,u64>(0)).unwrap(),0);
    }

    #[test]
    fn session_proposal_binding_is_atomic_and_cannot_rebase() {
        let (_root,mut db)=fixture();let cursor=db.current_head().unwrap();
        let first=db.create_planner_session("first","intent",&[],0,cursor,&budget()).unwrap();
        let second=db.create_planner_session("second","intent",&[],0,db.current_head().unwrap(),&budget()).unwrap();
        let head=db.current_head().unwrap();let raw=proposal(&first);
        for field in ["session_id","input_digest","rationale"] {
            let mut value:serde_json::Value=serde_json::from_slice(&raw).unwrap();
            value["planner"][field]=serde_json::Value::String(if field=="input_digest" {"0".repeat(64)}else{String::new()});
            assert!(db.apply_plan_proposal(&serde_json::to_vec(&value).unwrap(),0,"bad").is_err());
            assert_eq!(db.current_head().unwrap(),head);
        }
        db.connection.execute_batch("CREATE TEMP TRIGGER fail_binding BEFORE INSERT ON planner_proposal_inputs BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
        assert!(db.apply_plan_proposal(&raw,0,"first-response").is_err());assert_eq!(db.current_head().unwrap(),head);
        assert_eq!(current_revision(&db.connection).unwrap(),0);
        db.connection.execute_batch("DROP TRIGGER fail_binding;").unwrap();
        db.apply_plan_proposal(&raw,0,"first-response").unwrap();
        let mut other=SqliteStore::open(Path::new(db.connection.path().unwrap())).unwrap();
        assert!(matches!(other.apply_plan_proposal(&proposal(&second),0,"second-response"),Err(StoreError::StalePlanParent(1))));
        assert!(matches!(other.apply_plan_proposal(&proposal(&second),1,"second-response"),Err(StoreError::Conflict)));
        assert_eq!(current_revision(&other.connection).unwrap(),1);
    }

    #[test]
    fn competing_sessions_accept_one_revision_and_refuse_foreign_store_inputs() {
        let (_root,mut db)=fixture();
        let first=db.create_planner_session("first","intent",&[],0,db.current_head().unwrap(),&budget()).unwrap();
        let second=db.create_planner_session("second","intent",&[],0,db.current_head().unwrap(),&budget()).unwrap();
        let barrier=std::sync::Arc::new(std::sync::Barrier::new(3));
        let mut handles=Vec::new();
        for (session,key) in [(&first,"first"),(&second,"second")] {
            let gate=barrier.clone();let raw=proposal(session);let path=PathBuf::from(db.connection.path().unwrap());
            handles.push(std::thread::spawn(move||{
                let mut db=SqliteStore::open(&path).unwrap();gate.wait();db.apply_plan_proposal(&raw,0,key)
            }));
        }
        barrier.wait();let results:Vec<_>=handles.into_iter().map(|h|h.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|r|r.is_ok()).count(),1,"{results:?}");
        assert_eq!(results.iter().filter(|r|matches!(r,Err(StoreError::StalePlanParent(1)))).count(),1,"{results:?}");
        assert_eq!(current_revision(&db.connection).unwrap(),1);
        let (_foreign_root,mut foreign)=fixture();
        foreign.connection.execute("INSERT INTO planner_sessions VALUES(?1,?2,?3)",params![first.input.session_id,serde_json::to_vec(&first.input).unwrap(),first.input_digest]).unwrap();
        assert!(matches!(foreign.planner_session("first",&budget()),Err(StoreError::Conflict)));
        assert!(matches!(foreign.apply_plan_proposal(&proposal(&first),0,"foreign"),Err(StoreError::Conflict)));
        assert_eq!(current_revision(&foreign.connection).unwrap(),0);
    }

    #[test]
    fn session_rejects_missing_future_oversized_and_changed_inputs() {
        let (_root,mut db)=fixture();let head=db.current_head().unwrap();
        for evidence in [vec![0],vec![head+1],vec![head;65]] {
            assert!(db.create_planner_session("bad","intent",&evidence,0,head,&budget()).is_err());
        }
        assert!(db.create_planner_session("bad",&"x".repeat(65537),&[],0,head,&budget()).is_err());
        assert!(db.create_planner_session("bad","intent",&[],0,head+1,&budget()).is_err());
        let expired=read_budget::ReadBudget::new(controlled::ReadControl::new(std::time::Instant::now(),Default::default()));
        assert!(matches!(db.create_planner_session("bad","intent",&[],0,head,&expired),Err(StoreError::Deadline)));
        assert_eq!(db.current_head().unwrap(),head);
        db.connection.execute("INSERT INTO events(sequence,kind,entity,revision,payload_version,payload) VALUES(?1,'fixture','task',1,1,'{}')",[integer(head+100).unwrap()]).unwrap();
        let cursor=db.current_head().unwrap();
        assert!(db.create_planner_session("missing","intent",&[cursor-1],0,cursor,&budget()).is_err());
        assert!(db.create_planner_session("duplicate","intent",&[cursor,cursor],0,cursor,&budget()).is_err());
        db.connection.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('fixture','task',1,1,?1)",[serde_json::json!({"large":"x".repeat(PROPOSAL_LIMIT)}).to_string()]).unwrap();
        let cursor=db.current_head().unwrap();assert!(db.create_planner_session("oversize","intent",&[cursor],0,cursor,&budget()).is_err());
        let session=db.create_planner_session("valid","intent",&[],0,cursor,&budget()).unwrap();
        db.connection.execute_batch("DROP TRIGGER planner_sessions_no_update; UPDATE planner_sessions SET input_digest=printf('%064d',0);").unwrap();
        assert!(matches!(db.planner_session("valid",&budget()),Err(StoreError::Corrupt(_))));
        assert!(matches!(db.apply_plan_proposal(&proposal(&session),0,"corrupt"),Err(StoreError::Corrupt(_))));
    }
}

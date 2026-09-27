//! Bounded, revision-fenced inspection of accepted intent, not execution state.
use super::*;
use std::collections::BTreeMap;

const PAGE_BYTES:usize=1024*1024;

#[derive(Debug,Serialize)]
pub struct PlanIntent {
    pub task_id:TaskId,
    pub proposal_id:String,
    pub source_plan_revision:u64,
    pub payload_digest:String,
    pub text:String,
    pub dependencies:Vec<Dependency>,
    #[serde(skip_serializing_if="Option::is_none")]
    pub cancellation_reason:Option<String>,
}

#[derive(Debug,Serialize)]
pub struct PlanIntentPage {
    pub plan_revision:u64,
    pub entries:Vec<PlanIntent>,
    pub next_after:Option<String>,
}

impl SqliteStore {
    pub(crate) fn inspect_plan(&mut self,expected:Option<u64>,after:Option<&str>,limit:usize,budget:&read_budget::ReadBudget)->Result<PlanIntentPage> {
        budget.check()?;
        if !(1..=64).contains(&limit) || (after.is_some() && expected.is_none()) {
            return Err(invalid("plan inspection requires a limit of 1..64 and the expected plan revision for continuation"));
        }
        if let Some(after)=after {TaskId::new(after).map_err(StoreError::Invalid)?;}
        let tx=self.connection.transaction()?;
        check_schema(&tx)?;
        let version:u32=tx.query_row("PRAGMA user_version",[],|row|row.get(0))?;
        if version<43 {return Err(StoreError::UnsupportedSchema(version));}
        let revision=current_revision(&tx)?;
        if expected.is_some_and(|expected|expected!=revision) {return Err(StoreError::StalePlanParent(revision));}
        let mut entries=Vec::new();let mut next_after=None;let mut bytes=0;
        let mut sources:BTreeMap<String,(String,u64,ParsedProposal)>=BTreeMap::new();
        {
            let mut stmt=tx.prepare("SELECT task_id,proposal_id,plan_revision,dependencies,cancellation_reason FROM plan_task_intents WHERE task_id>?1 ORDER BY task_id LIMIT ?2")?;
            let mut rows=stmt.query(params![after.unwrap_or(""),(limit+1) as u64])?;
            while let Some(row)=rows.next()? {
                budget.row(row,&[(3,1)])?;
                if entries.len()==limit {
                    next_after=entries.last().map(|entry:&PlanIntent|entry.task_id.as_str().to_owned());break;
                }
                let cancellation_reason:Option<String>=row.get(4)?;
                let task=TaskId::new(row.get::<_,String>(0)?).map_err(StoreError::Corrupt)?;
                let proposal_id:String=row.get(1)?;
                let source_revision:u64=row.get(2)?;
                let dependencies:Vec<Dependency>=serde_json::from_str(&row.get::<_,String>(3)?).map_err(|_|StoreError::Corrupt("invalid projected plan dependencies".into()))?;
                if !sources.contains_key(&proposal_id) {
                    let (raw,digest,source_revision):(Vec<u8>,String,u64)=read_budget::optional(&tx,
                        "SELECT payload,payload_digest,plan_revision FROM plan_proposals WHERE proposal_id=?1",
                        [&proposal_id],Some(budget),&[(0,2)],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?
                        .ok_or_else(||StoreError::Corrupt("plan intent source is missing".into()))?;
                    if sha256_hex(&raw)!=digest {return Err(StoreError::Corrupt("plan intent source digest mismatch".into()));}
                    let parsed=parse_proposal(&raw).map_err(|_|StoreError::Corrupt("invalid plan intent source".into()))?;
                    sources.insert(proposal_id.clone(),(digest,source_revision,parsed));
                }
                let (digest,retained_revision,source)=sources.get(&proposal_id).unwrap();
                let contract=source.contracts.iter().find(|contract|contract.task_id==task)
                    .ok_or_else(||StoreError::Corrupt("plan intent task is absent from its source".into()))?;
                if source_revision!=*retained_revision || source_revision>revision || dependencies!=contract.dependencies || cancellation_reason!=contract.cancellation_reason {
                    return Err(StoreError::Corrupt("plan intent projection disagrees with its source".into()));
                }
                let entry=PlanIntent{task_id:task,proposal_id,source_plan_revision:source_revision,payload_digest:digest.clone(),text:contract.text.clone(),dependencies,cancellation_reason};
                let length=serde_json::to_vec(&entry).map_err(|error|invalid(&error.to_string()))?.len();
                if bytes+length>PAGE_BYTES {
                    next_after=entries.last().map(|entry:&PlanIntent|entry.task_id.as_str().to_owned());
                    if next_after.is_none() {return Err(StoreError::Limit("plan intent exceeds page byte budget".into()));}
                    break;
                }
                bytes+=length;entries.push(entry);
            }
        }
        budget.check()?;tx.commit()?;
        Ok(PlanIntentPage{plan_revision:revision,entries,next_after})
    }
}

pub fn inspect_project_plan(project:&Path,expected:Option<u64>,after:Option<&str>,limit:usize)->anyhow::Result<PlanIntentPage> {
    let _guard=crate::migration::runtime_mutation(project)?;
    let control=controlled::ReadControl::new(std::time::Instant::now()+Duration::from_secs(2),Default::default());
    Ok(crate::migration::open_active_scoped(project,control)?.inspect_plan(expected,after,limit)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture()->(tempfile::TempDir,SqliteStore) {
        let root=tempfile::tempdir().unwrap();let db=SqliteStore::create(&root.path().join("state.db")).unwrap();(root,db)
    }
    fn budget()->read_budget::ReadBudget {read_budget::ReadBudget::new(controlled::ReadControl::new(std::time::Instant::now()+Duration::from_secs(5),Default::default()))}
    fn proposal(task:&str,text:&str)->Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"version":1,"contracts":[{"task_id":task,"text":text,"dependencies":[]}]})).unwrap()
    }

    #[test]
    fn inspection_work_is_independent_of_superseded_history() {
        use std::sync::{Arc,atomic::{AtomicUsize,Ordering}};
        let mut work=Vec::new();
        for history in [false,true] {
            let (_root,mut db)=fixture();let raw=proposal("a","retained intent");
            db.apply_plan_proposal(&raw,0,"baseline").unwrap();
            if history {
                let path=store_path(&db.connection).unwrap().to_string_lossy().into_owned();
                db.connection.execute("WITH RECURSIVE n(x) AS (SELECT 2 UNION ALL SELECT x+1 FROM n WHERE x<10001) INSERT INTO plan_proposals SELECT printf('%064d',x),?1,printf('history-%d',x),?2,?3,x-1,x,0 FROM n",params![path,sha256_hex(&raw),raw]).unwrap();
                db.connection.execute("INSERT INTO plan_revisions SELECT plan_revision,parent_revision,proposal_id,payload_digest,'[{\"task_id\":\"a\",\"text\":\"retained intent\"}]',0 FROM plan_proposals WHERE plan_revision>1",[]).unwrap();
            }
            let steps=Arc::new(AtomicUsize::new(0));let counter=steps.clone();
            db.connection.progress_handler(1,Some(move||{counter.fetch_add(1,Ordering::Relaxed);false}));
            let page=db.inspect_plan(None,None,32,&budget()).unwrap();
            db.connection.progress_handler(0,None::<fn()->bool>);work.push(steps.load(Ordering::Relaxed));
            assert_eq!(page.entries.len(),1);assert_eq!(page.entries[0].text,"retained intent");assert!(page.next_after.is_none());
        }
        eprintln!("plan inspection SQL steps at 0/10000 superseded proposals: {work:?}");
        assert!(work[1]<=work[0]+100,"superseded history increased inspection work: {work:?}");
    }

    #[test]
    fn inspection_checks_selected_source_and_projection_without_decoding_cold_sources() {
        let (_root,mut db)=fixture();
        db.apply_plan_proposal(&proposal("a","selected"),0,"first").unwrap();
        let cold=db.apply_plan_proposal(&proposal("z","cold"),1,"second").unwrap();
        db.connection.execute_batch("DROP TRIGGER plan_proposals_no_update;").unwrap();
        db.connection.execute("UPDATE plan_proposals SET payload_digest=?1 WHERE proposal_id=?2",params!["0".repeat(64),cold.proposal_id]).unwrap();
        let head=db.current_head().unwrap();
        assert_eq!(db.inspect_plan(None,None,1,&budget()).unwrap().entries[0].text,"selected");
        assert!(matches!(db.inspect_plan(Some(2),Some("a"),1,&budget()),Err(StoreError::Corrupt(_))));
        db.connection.execute("UPDATE plan_task_intents SET dependencies='[{\"predecessor\":\"z\",\"requirement\":\"verified_result\"}]' WHERE task_id='a'",[]).unwrap();
        assert!(matches!(db.inspect_plan(None,None,1,&budget()),Err(StoreError::Corrupt(_))));
        assert_eq!(db.current_head().unwrap(),head);
    }

    #[test]
    fn inspection_byte_pages_never_skip_the_first_omitted_task() {
        let (_root,mut db)=fixture();let text="x".repeat(230_000);
        for n in 0..8 {db.apply_plan_proposal(&proposal(&format!("task-{n}"),&text),n,&format!("proposal-{n}")).unwrap();}
        let head=db.current_head().unwrap();let mut after=None;let mut ids=Vec::new();let mut pages=0;
        loop {
            let page=db.inspect_plan(Some(8),after.as_deref(),64,&budget()).unwrap();
            assert!(page.entries.iter().map(|entry|serde_json::to_vec(entry).unwrap().len()).sum::<usize>()<=PAGE_BYTES);
            ids.extend(page.entries.iter().map(|entry|entry.task_id.as_str().to_owned()));pages+=1;
            after=page.next_after;if after.is_none(){break;}
            assert!(pages<8);
        }
        assert_eq!(pages,2);assert_eq!(ids,(0..8).map(|n|format!("task-{n}")).collect::<Vec<_>>());
        for limit in [0,65] {assert!(db.inspect_plan(None,None,limit,&budget()).is_err());}
        let expired=read_budget::ReadBudget::new(controlled::ReadControl::new(std::time::Instant::now(),Default::default()));
        assert!(matches!(db.inspect_plan(None,None,32,&expired),Err(StoreError::Deadline)));
        assert_eq!(db.current_head().unwrap(),head);
    }
}

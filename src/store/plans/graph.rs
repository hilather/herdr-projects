//! Read only the bounded dependency graph, not task bodies or scheduler history.
use super::*;
use crate::domain::DependencyRequirement;
use std::collections::{BTreeMap,BTreeSet};

fn check(budget:Option<&read_budget::ReadBudget>)->Result<()> {
    if let Some(budget)=budget {budget.check()?;}Ok(())
}
fn validate_graph(nodes:&BTreeSet<TaskId>,queue:&BTreeMap<TaskId,Vec<Dependency>>,budget:Option<&read_budget::ReadBudget>)->Result<()> {
    check(budget)?;
    if queue.len()>10_000 {return Err(invalid("queued task inventory exceeds 10000"));}
    if queue.values().map(Vec::len).sum::<usize>()>100_000 {return Err(invalid("dependency inventory exceeds bounds"));}
    // The shared graph validator needs only identity. Do not decode titles,
    // active attempts or unrelated retained task records to provide that input.
    let tasks:Vec<_>=nodes.iter().map(|id|Task{id:id.clone(),revision:1,state:TaskState::Draft,title:String::new(),active_attempt:None}).collect();
    let records:Vec<_>=queue.iter().map(|(id,dependencies)|QueueRecord{task:id.clone(),priority:0,enqueued_unix_ms:0,enqueue_sequence:0,dependencies:dependencies.clone()}).collect();
    scheduler::graph(&tasks,&records)?;check(budget)
}

pub(super) fn validate(db:&Connection,proposal:&ParsedProposal,budget:Option<&read_budget::ReadBudget>)->Result<()> {
    check(budget)?;
    let mut nodes=BTreeSet::new();let mut queue=BTreeMap::new();
    let mut stmt=db.prepare("SELECT task_id,EXISTS(SELECT 1 FROM tasks t WHERE t.id=q.task_id) FROM task_queue q ORDER BY task_id LIMIT 10001")?;
    let mut rows=stmt.query([])?;
    while let Some(row)=rows.next()? {
        if let Some(budget)=budget {budget.row(row,&[])?;}
        if queue.len()==10_000 {return Err(invalid("queued task inventory exceeds 10000"));}
        if !row.get::<_,bool>(1)? {return Err(StoreError::Corrupt("queued task is missing".into()));}
        let id=TaskId::new(row.get::<_,String>(0)?).map_err(StoreError::Corrupt)?;
        nodes.insert(id.clone());queue.insert(id,Vec::new());
    }
    let mut stmt=db.prepare("SELECT task_id,predecessor_id,requirement,EXISTS(SELECT 1 FROM tasks t WHERE t.id=d.predecessor_id) FROM task_dependencies d ORDER BY task_id,predecessor_id LIMIT 100001")?;
    let mut rows=stmt.query([])?;let mut edges=0;
    while let Some(row)=rows.next()? {
        if let Some(budget)=budget {budget.row(row,&[])?;}
        edges+=1;if edges>100_000 {return Err(invalid("dependency inventory exceeds bounds"));}
        let task=TaskId::new(row.get::<_,String>(0)?).map_err(StoreError::Corrupt)?;
        let predecessor=TaskId::new(row.get::<_,String>(1)?).map_err(StoreError::Corrupt)?;
        if !row.get::<_,bool>(3)? {return Err(StoreError::Corrupt("dependency task is missing".into()));}
        let requirement=match row.get::<_,String>(2)?.as_str() {
            "verified_result"=>DependencyRequirement::VerifiedResult,
            "integrated_commit"=>DependencyRequirement::IntegratedCommit,
            "integration_candidate"=>DependencyRequirement::IntegrationCandidate,
            "landed_commit"=>DependencyRequirement::LandedCommit,
            _=>return Err(StoreError::Corrupt("unknown dependency requirement".into())),
        };
        let dependencies=queue.get_mut(&task).ok_or_else(||StoreError::Corrupt("dependency has no queue record".into()))?;
        if dependencies.len()==256 {return Err(invalid("dependency inventory exceeds bounds"));}
        nodes.insert(predecessor.clone());dependencies.push(Dependency{predecessor,requirement});
    }
    validate_graph(&nodes,&queue,budget)?;
    // Accepted proposals describe intended work even before tasks or queue rows
    // are installed. Preserve unmentioned intent across successive revisions.
    let version:u32=db.query_row("PRAGMA user_version",[],|row|row.get(0))?;
    if version>=43 {
        let mut stmt=db.prepare("SELECT task_id,dependencies FROM plan_task_intents ORDER BY task_id LIMIT 10001")?;
        let mut rows=stmt.query([])?;let mut count=0;let mut planned_edges=0;
        while let Some(row)=rows.next()? {
            if let Some(budget)=budget {budget.row(row,&[(1,1)])?;}
            count+=1;if count>10_000 {return Err(invalid("planned task inventory exceeds 10000"));}
            let id=TaskId::new(row.get::<_,String>(0)?).map_err(StoreError::Corrupt)?;
            let dependencies:Vec<Dependency>=serde_json::from_str(&row.get::<_,String>(1)?).map_err(|_|StoreError::Corrupt("invalid planned dependency".into()))?;
            if dependencies.len()>256 {return Err(invalid("dependency inventory exceeds bounds"));}
            planned_edges+=dependencies.len();
            if planned_edges>100_000 {return Err(invalid("planned dependency inventory exceeds bounds"));}
            nodes.insert(id.clone());queue.insert(id,dependencies);
        }
    }
    for contract in &proposal.contracts {nodes.insert(contract.task_id.clone());}
    for contract in &proposal.contracts {
        check(budget)?;
        queue.insert(contract.task_id.clone(),contract.dependencies.clone());
    }
    for dependencies in queue.values() {
        check(budget)?;
        for edge in dependencies {
            if !nodes.contains(&edge.predecessor) {
                let exists:bool=read_budget::one(db,"SELECT EXISTS(SELECT 1 FROM tasks WHERE id=?1)",[edge.predecessor.as_str()],budget,&[],|r|r.get(0))?;
                if !exists {return Err(invalid("missing dependency task"));}
                nodes.insert(edge.predecessor.clone());
            }
        }
    }
    validate_graph(&nodes,&queue,budget)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc,atomic::{AtomicUsize,Ordering}};
    fn fixture()->(tempfile::TempDir,SqliteStore) {
        let root=tempfile::tempdir().unwrap();let db=SqliteStore::create(&root.path().join("state.db")).unwrap();(root,db)
    }
    fn budget()->read_budget::ReadBudget {read_budget::ReadBudget::new(controlled::ReadControl::new(std::time::Instant::now()+Duration::from_secs(5),Default::default()))}
    fn proposal(task:&str,dependencies:&[&str])->Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"version":1,"contracts":[{"task_id":task,"text":"proposed change","dependencies":dependencies.iter().map(|id|serde_json::json!({"predecessor":id,"requirement":"verified_result"})).collect::<Vec<_>>()}]})).unwrap()
    }
    #[test]
    fn accepted_intent_replacement_is_atomic_and_backfills_from_schema_42() {
        let (_root,mut db)=fixture();
        crate::store::test_schema::historical(&db.connection,42).unwrap();
        db.connection.execute_batch("INSERT INTO tasks VALUES('a',1,'draft','a',NULL),('b',1,'draft','b',NULL);").unwrap();
        db.apply_plan_proposal(&proposal("a",&["b"]),0,"first").unwrap();
        db.apply_plan_proposal(&proposal("a",&[]),1,"replace").unwrap();
        db.apply_plan_proposal(&proposal("c",&["a"]),2,"unmaterialized").unwrap();
        db.upgrade_v1().unwrap();
        assert_eq!(db.connection.query_row("SELECT dependencies FROM plan_task_intents WHERE task_id='a'",[],|r|r.get::<_,String>(0)).unwrap(),"[]");
        db.apply_plan_proposal(&proposal("d",&["c"]),3,"after-upgrade").unwrap();
        let head=db.current_head().unwrap();
        db.connection.execute_batch("CREATE TEMP TRIGGER fail_intent BEFORE UPDATE ON plan_task_intents BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
        assert!(db.apply_plan_proposal(&proposal("a",&["b"]),4,"failed-replacement").is_err());
        assert_eq!(db.current_head().unwrap(),head);assert_eq!(current_revision(&db.connection).unwrap(),4);
        assert_eq!(db.connection.query_row("SELECT dependencies FROM plan_task_intents WHERE task_id='a'",[],|r|r.get::<_,String>(0)).unwrap(),"[]");
        db.connection.execute_batch("DROP TRIGGER fail_intent;").unwrap();
        db.apply_plan_proposal(&proposal("a",&["b"]),4,"replacement").unwrap();
        assert!(db.apply_plan_proposal(&proposal("b",&["d"]),5,"cycle-through-history").is_err());
        assert_eq!(db.connection.query_row("SELECT count(*) FROM task_queue",[],|r|r.get::<_,u64>(0)).unwrap(),0);
    }

    #[test]
    fn accepted_intent_selection_ignores_superseded_proposal_history() {
        let mut work=Vec::new();
        for history in [false,true] {
            let (_root,mut db)=fixture();let old=proposal("old",&[]);
            db.apply_plan_proposal(&old,0,"baseline").unwrap();
            if history {
                // Modeled accepted history, not 10,000 inference jobs. The
                // production projection trigger processes every inserted row.
                db.connection.execute("WITH RECURSIVE n(x) AS (SELECT 2 UNION ALL SELECT x+1 FROM n WHERE x<10001) INSERT INTO plan_proposals SELECT printf('%064d',x),'fixture',printf('history-%d',x),?1,?2,x-1,x,0 FROM n",params![sha256_hex(&old),old]).unwrap();
                db.connection.execute("INSERT INTO plan_revisions SELECT plan_revision,parent_revision,proposal_id,payload_digest,'[{\"task_id\":\"old\",\"text\":\"proposed change\"}]',0 FROM plan_proposals WHERE project_store='fixture'",[]).unwrap();
            }
            assert_eq!(db.connection.query_row("SELECT count(*) FROM plan_task_intents",[],|r|r.get::<_,u64>(0)).unwrap(),1);
            let parent=current_revision(&db.connection).unwrap();
            let steps=Arc::new(AtomicUsize::new(0));let counter=steps.clone();
            db.connection.progress_handler(1,Some(move||{counter.fetch_add(1,Ordering::Relaxed);false}));
            db.apply_plan_proposal_with_budget(&proposal("new",&["old"]),parent,"new",Some(&budget())).unwrap();
            db.connection.progress_handler(0,None::<fn()->bool>);work.push(steps.load(Ordering::Relaxed));
        }
        eprintln!("proposal acceptance SQL steps at 0/10000 superseded proposals: {work:?}");
        assert!(work[1]<=work[0]+100,"superseded history increased proposal work: {work:?}");
    }

    #[test]
    fn successive_proposals_cannot_hide_a_cycle_in_accepted_intent() {
        let (_root,mut db)=fixture();
        db.connection.execute_batch("INSERT INTO tasks VALUES('a',1,'draft','a',NULL),('b',1,'draft','b',NULL);").unwrap();
        db.apply_plan_proposal(&proposal("a",&["b"]),0,"first").unwrap();
        let head=db.current_head().unwrap();
        assert!(matches!(db.apply_plan_proposal(&proposal("b",&["a"]),1,"second"),Err(StoreError::Invalid(message)) if message.contains("cycle")));
        assert_eq!(db.current_head().unwrap(),head);assert_eq!(current_revision(&db.connection).unwrap(),1);
    }

    #[test]
    fn proposal_work_is_independent_of_unqueued_task_history() {
        let mut work=Vec::new();
        for history in [false,true] {
            let (_root,mut db)=fixture();
            db.connection.execute_batch("INSERT INTO tasks VALUES('predecessor',1,'succeeded','retained evidence task',NULL);").unwrap();
            if history {db.connection.execute_batch("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<10000) INSERT INTO tasks SELECT printf('history-%05d',x),1,'succeeded','unrelated historical task',NULL FROM n;").unwrap();}
            let raw=proposal("new-task",&["predecessor"]);let mut phases=Vec::new();
            for phase in 0..3 {
                let steps=Arc::new(AtomicUsize::new(0));let counter=steps.clone();
                db.connection.progress_handler(1,Some(move||{counter.fetch_add(1,Ordering::Relaxed);false}));
                let result=db.apply_plan_proposal_with_budget(&raw,0,if phase==2 {"stale"}else{"accept"},Some(&budget()));
                db.connection.progress_handler(0,None::<fn()->bool>);
                phases.push(steps.load(Ordering::Relaxed));
                match phase {0=>assert!(!result.unwrap().replayed),1=>assert!(result.unwrap().replayed),_=>assert!(matches!(result,Err(StoreError::StalePlanParent(1))))}
            }
            work.push(phases);
        }
        eprintln!("proposal accept/replay/stale SQL steps at 0/10000 unqueued tasks: {work:?}");
        for phase in 0..3 {assert!(work[1][phase]<=work[0][phase]+100,"unrelated history increased proposal work: {work:?}");}
    }
    #[test]
    fn proposal_graph_preserves_unmentioned_edges_and_refuses_bad_dependencies() {
        let (_root,mut db)=fixture();
        db.connection.execute_batch("INSERT INTO tasks VALUES('a',1,'queued','a',NULL),('b',1,'succeeded','b',NULL); INSERT INTO task_queue VALUES('a',0,0,1); INSERT INTO task_dependencies(task_id,predecessor_id,requirement) VALUES('a','b','verified_result');").unwrap();
        let head=db.current_head().unwrap();
        for raw in [proposal("b",&["a"]),proposal("c",&["missing"]),proposal("c",&["b","b"]),proposal("c",&["c"])] {
            assert!(db.apply_plan_proposal_with_budget(&raw,0,"bad",Some(&budget())).is_err());assert_eq!(db.current_head().unwrap(),head);
        }
        let expired=read_budget::ReadBudget::new(controlled::ReadControl::new(std::time::Instant::now(),Default::default()));
        assert!(matches!(db.apply_plan_proposal_with_budget(&proposal("c",&["b"]),0,"expired",Some(&expired)),Err(StoreError::Deadline)));
        assert_eq!(db.current_head().unwrap(),head);
        let raw=serde_json::to_vec(&serde_json::json!({"version":1,"contracts":[{"task_id":"a","text":"replace proposed edges","dependencies":[{"predecessor":"c","requirement":"verified_result"}]},{"task_id":"c","text":"new predecessor","dependencies":[]}]})).unwrap();
        db.apply_plan_proposal_with_budget(&raw,0,"valid",Some(&budget())).unwrap();
        assert_eq!(db.connection.query_row("SELECT predecessor_id FROM task_dependencies WHERE task_id='a'",[],|r|r.get::<_,String>(0)).unwrap(),"b");
    }
    #[test]
    fn proposal_graph_refuses_overfull_queue_before_acceptance() {
        let (_root,mut db)=fixture();
        db.connection.execute_batch("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<10001) INSERT INTO tasks SELECT printf('q-%05d',x),1,'queued','queued',NULL FROM n; INSERT INTO task_queue SELECT id,0,0,1 FROM tasks;").unwrap();
        let head=db.current_head().unwrap();
        let error=db.apply_plan_proposal_with_budget(&proposal("new",&[]),0,"overfull",Some(&budget())).unwrap_err();
        assert!(matches!(error,StoreError::Invalid(message) if message.contains("10000")));
        assert_eq!(db.current_head().unwrap(),head);assert_eq!(current_revision(&db.connection).unwrap(),0);
    }
}

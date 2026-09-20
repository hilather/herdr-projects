//! Active publication validation for a bounded, read-only identity inventory.
use super::*;
use crate::{domain::RuntimeBinding,store::identity_inventory::{Budget,Publication}};
fn publication(project:&Path,budget:&mut Budget)->Result<(std::path::PathBuf,Publication)> {
    budget.check()?;let project=checked_project(project)?;
    ensure!(fs::symlink_metadata(project.join(".state/migration"))?.is_dir(),"migration directory must be real");
    let journal:Journal=serde_json::from_slice(&budget.read(&journal_path(&project))?)?;
    ensure!(journal.version==1&&journal.phase==Phase::Active,"migration is not active");
    ensure!(Path::new(&journal.plan.project)==project,"identity journal project mismatch");
    ensure!(references::plan_digest(&journal.plan)?==journal.plan.digest&&journal.plan.sources.iter().all(|s|safe_relative(&s.path)),"identity journal inventory mismatch");budget.check()?;
    let marker:Format=serde_json::from_slice(&budget.read(&project.join(".state/format.json"))?)?;
    ensure!(marker==Format{version:1,runtime:"sqlite-v2".into(),memory:"legacy-markdown".into(),migration:journal.plan.digest.clone(),reconciliation_required:marker.reconciliation_required},"identity ownership marker mismatch");
    let publication=Publication{digest:journal.plan.digest,sources:journal.plan.sources.iter().filter(|s|s.kind!="backup").count() as u64,tasks:journal.plan.tasks.len() as u64,operations:journal.plan.operations.len() as u64,reconciliation_required:marker.reconciliation_required};
    Ok((project,publication))
}
pub fn read_identity_inventory(project:&Path,budget:&mut Budget)->Result<Vec<RuntimeBinding>> {
    let(project,publication)=publication(project,budget)?;
    crate::store::identity_inventory::read(&project.join(".state/state.db"),&publication,budget)
}
pub fn read_routine_execution_hint(project:&Path,budget:&mut Budget,last:Option<&crate::domain::OperationId>,now:i64)->Result<Option<crate::store::controller_hint::RoutineExecutionHint>> {
    let(project,publication)=publication(project,budget)?;
    crate::store::controller_hint::read_routine(&project.join(".state/state.db"),&publication,budget,last,now)
}
pub fn read_controller_effect_hint(project:&Path,budget:&mut Budget,turn:u64,now:i64)->Result<Option<crate::store::controller_hint::ControllerEffectHint>> {
    let(project,publication)=publication(project,budget)?;
    crate::store::controller_hint::read(&project.join(".state/state.db"),&publication,budget,turn,now)
}
pub fn read_observation_head(project:&Path,budget:&mut Budget)->Result<u64> {
    let(project,publication)=publication(project,budget)?;
    crate::store::identity_inventory::read_head(&project.join(".state/state.db"),&publication,budget)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration,Instant};
    pub(super) fn fixture()->(tempfile::TempDir,std::path::PathBuf) {
        let root=tempfile::tempdir().unwrap();let p=root.path().join("project");fs::create_dir_all(p.join(".state")).unwrap();fs::create_dir_all(p.join("threads")).unwrap();
        fs::write(p.join("PROJECT.md"),"+++\nname = 'Fixture'\n+++\n").unwrap();fs::write(p.join(".state/project.json"),"{\"status\":\"paused\"}").unwrap();fs::write(p.join(".state/coordinator.json"),"{}").unwrap();
        let plan=inspect(&p).unwrap();assert!(plan.blockers.is_empty(),"{:?}",plan.blockers);apply(&p,&plan,true).unwrap();(root,p)
    }
    fn budget()->Budget {Budget::new(50*1024*1024,1024,Instant::now()+Duration::from_secs(10),Default::default()).unwrap()}
    #[test]
    fn observation_head_reads_no_historical_payload_and_fences_publication() {
        let(_root,p)=fixture();let db=rusqlite::Connection::open(p.join(".state/state.db")).unwrap();
        db.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('unrelated','x',1,1,?1)",[serde_json::to_string(&"x".repeat(20*1024*1024)).unwrap()]).unwrap();
        let expected=db.query_row("SELECT max(sequence) FROM events",[],|r|r.get::<_,u64>(0)).unwrap();
        let mut limits=Budget::new(2*1024*1024,0,Instant::now()+Duration::from_millis(100),Default::default()).unwrap();
        assert_eq!(read_observation_head(&p,&mut limits).unwrap(),expected);assert!(limits.used()<100_000);
        let path=p.join(".state/format.json");let mut marker:Format=serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();marker.reconciliation_required=!marker.reconciliation_required;fs::write(path,serde_json::to_vec(&marker).unwrap()).unwrap();
        assert!(read_observation_head(&p,&mut budget()).is_err());
    }
    #[test]
    fn observation_head_sqlite_work_is_interrupted_by_its_original_budget() {
        let(_root,p)=fixture();let db=rusqlite::Connection::open(p.join(".state/state.db")).unwrap();
        db.execute_batch("ALTER TABLE events RENAME TO original_events; CREATE VIEW events AS SELECT * FROM original_events WHERE (WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<100000000) SELECT sum(x) FROM n)>0;").unwrap();
        let mut limits=Budget::new(2*1024*1024,0,Instant::now()+Duration::from_millis(100),Default::default()).unwrap();let started=Instant::now();
        assert!(read_observation_head(&p,&mut limits).is_err());assert!(started.elapsed()<Duration::from_secs(2));
        let mut cancelled=budget();cancelled.cancellation.cancel();assert!(read_observation_head(&p,&mut cancelled).is_err());
    }
    #[test]
    fn identity_inventory_matches_snapshot_without_materializing_unrelated_history() {
        let(_root,p)=fixture();let expected=crate::runtime::snapshot(&p).unwrap().runtime_bindings;
        // Large unrelated history is outside the identity materialization budget.
        let db=rusqlite::Connection::open(p.join(".state/state.db")).unwrap();
        db.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('unrelated','x',1,1,?1)",[serde_json::to_string(&"x".repeat(20*1024*1024)).unwrap()]).unwrap();
        let mut limits=budget();assert_eq!(read_identity_inventory(&p,&mut limits).unwrap(),expected);assert!(limits.used()<100_000);
    }
    #[test]
    fn budget_and_corrupt_provenance_refuse_before_identity_is_returned() {
        let(_root,p)=fixture();let mut zero=Budget::new(0,1024,Instant::now()+Duration::from_secs(10),Default::default()).unwrap();assert!(read_identity_inventory(&p,&mut zero).is_err());
        let mut no_records=Budget::new(50*1024*1024,0,Instant::now()+Duration::from_secs(10),Default::default()).unwrap();assert!(read_identity_inventory(&p,&mut no_records).is_err());
        let db=rusqlite::Connection::open(p.join(".state/state.db")).unwrap();db.execute("UPDATE runtime_bindings SET payload_hash=?1",["0".repeat(64)]).unwrap();assert!(read_identity_inventory(&p,&mut budget()).is_err());
    }
    #[test]
    fn oversized_payload_cancelled_budget_and_publication_mismatch_refuse() {
        let(_root,p)=fixture();let mut cancelled=budget();cancelled.cancellation.cancel();assert!(read_identity_inventory(&p,&mut cancelled).is_err());
        let db=rusqlite::Connection::open(p.join(".state/state.db")).unwrap();db.execute("UPDATE runtime_bindings SET payload=?1",[serde_json::json!({"oversized":"x".repeat(16*1024*1024)}).to_string()]).unwrap();
        let error=read_identity_inventory(&p,&mut budget()).unwrap_err().to_string();assert!(error.contains("16 MiB"),"{error}");
        let(_root,p)=fixture();let path=p.join(".state/format.json");let mut marker:Format=serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();marker.migration="0".repeat(64);fs::write(path,serde_json::to_vec(&marker).unwrap()).unwrap();assert!(read_identity_inventory(&p,&mut budget()).is_err());
    }
    #[test]
    fn sqlite_progress_interrupts_work_inside_a_query() {
        let(_root,p)=fixture();let db=rusqlite::Connection::open(p.join(".state/state.db")).unwrap();
        // Force work before the first inventory row; a between-row check alone
        // cannot interrupt this query. This is only a disposable corrupt fixture.
        db.execute_batch("ALTER TABLE runtime_bindings RENAME TO original_bindings; CREATE VIEW runtime_bindings AS SELECT * FROM original_bindings WHERE (WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<100000000) SELECT sum(x) FROM n)>0;").unwrap();
        let mut budget=Budget::new(50*1024*1024,1024,Instant::now()+Duration::from_millis(100),Default::default()).unwrap();let started=Instant::now();
        let error=read_identity_inventory(&p,&mut budget).unwrap_err().to_string();assert!(started.elapsed()<Duration::from_secs(2),"{error}");assert!(error.contains("interrupt")||error.contains("expired"),"{error}");
    }

    #[test]
    fn deleted_canonical_binding_cannot_hide_retained_resource_references() {
        use crate::domain::{TaskId,RuntimeRoute,RuntimeOwnership};
        for table in ["runtime_ownership","runtime_observations"] {
            let(_root,p)=fixture();let task=TaskId::new("native").unwrap();let head=crate::runtime::snapshot(&p).unwrap().head;
            crate::runtime::add_task(&p,task.clone(),"native task".into(),head).unwrap();let head=crate::runtime::snapshot(&p).unwrap().head;
            let binding=crate::runtime::create_binding(&p,Some(&task),Some(1),head,&RuntimeRoute::default()).unwrap().binding;
            let db=rusqlite::Connection::open(p.join(".state/state.db")).unwrap();db.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
            if table=="runtime_ownership" {
                let owned=RuntimeOwnership{binding:binding.id.clone(),revision:1,binding_revision:binding.revision,identity_digest:hash(&serde_json::to_vec(&binding.identity).unwrap()),origin:"adopted".into(),attempt:None,session:None,worktree:None,agent:None,config_digest:None,observed_unix_ms:0};
                let payload=serde_json::to_string(&owned).unwrap();db.execute("INSERT INTO runtime_ownership VALUES(?1,1,1,NULL,?2,?3)",rusqlite::params![binding.id,payload,hash(payload.as_bytes())]).unwrap();
            }else {db.execute("INSERT INTO runtime_observations VALUES(?1,1,1,0,'{}',?2)",rusqlite::params![binding.id,"0".repeat(64)]).unwrap();}
            db.execute("DELETE FROM runtime_bindings WHERE id=?1",[&binding.id]).unwrap();let error=read_identity_inventory(&p,&mut budget()).unwrap_err().to_string();assert!(error.contains("dangling"),"{table}: {error}");
        }
    }

}

#[cfg(test)]
#[path="controller_hint_tests.rs"]
mod controller_hint_tests;

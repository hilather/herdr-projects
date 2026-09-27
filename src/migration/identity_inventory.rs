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
    ensure!(published_format_matches(&marker,&journal),"identity ownership marker mismatch");
    let publication=Publication{digest:journal.plan.digest,sources:journal.plan.sources.iter().filter(|s|s.kind!="backup").count() as u64,tasks:journal.plan.tasks.len() as u64,operations:journal.plan.operations.len() as u64,reconciliation_required:marker.reconciliation_required};
    Ok((project,publication))
}
/// Parse bounded publication expectations, then validate them on the exact
/// controlled writable handle returned to the caller. No intervening readonly
/// connection can certify a subsequently reopened database.
pub fn open_active_controlled(project:&Path,control:crate::store::controlled::ReadControl)->Result<crate::store::controlled::ControlledStore> {
    open_active_checked(project,control,true)
}
/// Foreground selected-row reads retain publication and file-identity checks.
/// Whole-store integrity verification remains available through the administrative opener.
pub fn open_active_scoped(project:&Path,control:crate::store::controlled::ReadControl)->Result<crate::store::controlled::ControlledStore> {
    open_active_checked(project,control,false)
}
fn open_active_checked(project:&Path,control:crate::store::controlled::ReadControl,integrity:bool)->Result<crate::store::controlled::ControlledStore> {
    control.check()?;
    let mut budget=Budget::new(50*1024*1024,0,control.deadline(),control.cancellation())?;
    let(project,expected)=publication(project,&mut budget)?;control.check()?;
    let path=project.join(".state/state.db");
    let db=if integrity {crate::store::controlled::ControlledStore::open(&path,control.clone())?}
        else {crate::store::controlled::ControlledStore::open_scoped(&path,control.clone())?};
    ensure!(db.import_operation_count()?==expected.operations,"store imported operation count mismatch");
    ensure!(db.import_receipt()?==(expected.digest,expected.sources,expected.tasks),"store import identity mismatch");
    ensure!(db.project_control()?.map(|c|c.reconciliation_required).unwrap_or(true)==expected.reconciliation_required,"control/format publication interrupted; run migration recover before runtime commands");
    control.check()?;Ok(db)
}
/// DB-first publication under the caller's retained project and record locks.
/// Cancellation can leave the DB committed and marker unpublished; recovery
/// reads the authoritative DB rather than interpreting this as rollback.
pub(crate) fn publish_control_marker_controlled(project:&Path,db:&crate::store::controlled::ControlledStore,control:&crate::store::controlled::ReadControl)->Result<()> {
    control.check()?;
    let mut budget=Budget::new(50*1024*1024,0,control.deadline(),control.cancellation())?;
    let(project,old)=publication(project,&mut budget)?;
    let required=db.project_control()?.map(|c|c.reconciliation_required).unwrap_or(true);
    if required==old.reconciliation_required {control.check()?;return Ok(());}
    let current:Format=serde_json::from_slice(&budget.read(&project.join(".state/format.json"))?)?;
    ensure!(memory_owner_ok(&current.memory)&&current.version==1&&current.runtime=="sqlite-v2"&&current.migration==old.digest,"identity ownership marker mismatch");
    let marker=Format{version:1,runtime:"sqlite-v2".into(),memory:current.memory,migration:old.digest,reconciliation_required:required};
    let temporary=project.join(".state/migration/control-format.next");
    control.check()?;
    if exists(&temporary){ensure!(fs::symlink_metadata(&temporary)?.is_file(),"invalid control marker temporary");fs::remove_file(&temporary)?;}
    control.check()?;write_new(&temporary,&serde_json::to_vec_pretty(&marker)?)?;
    control.check()?;fs::rename(temporary,project.join(".state/format.json"))?;
    // Once renamed, finish the existing durability boundary. Do not claim the
    // marker was unpublished if cancellation arrives during these fsync calls.
    sync_dir(&project.join(".state"))?;sync_dir(&project.join(".state/migration"))?;Ok(())
}
pub fn read_identity_inventory(project:&Path,budget:&mut Budget)->Result<Vec<RuntimeBinding>> {
    let(project,publication)=publication(project,budget)?;
    crate::store::identity_inventory::read(&project.join(".state/state.db"),&publication,budget)
}
/// Candidate bindings for a pane conflict check; malformed pane identities
/// remain candidates and must pass the ordinary binding/provenance validator.
pub(crate) fn read_pane_bindings(project:&Path,pane:&str,budget:&mut Budget)->Result<Vec<RuntimeBinding>> {
    let(project,publication)=publication(project,budget)?;
    crate::store::identity_inventory::read_pane_bindings(&project.join(".state/state.db"),&publication,budget,pane)
}
/// Local worktree references plus malformed machine/path identities. Retained
/// nonempty paths remain references regardless of task/attempt terminal state.
pub(crate) fn read_worktree_bindings(project:&Path,budget:&mut Budget)->Result<Vec<RuntimeBinding>> {
    let(project,publication)=publication(project,budget)?;
    crate::store::identity_inventory::read_worktree_bindings(&project.join(".state/state.db"),&publication,budget)
}
/// Matching staged panes plus malformed identities, with complete selected provenance.
pub(crate) fn read_pane_targets(project:&Path,pane:&str,budget:&mut Budget)->Result<Vec<(String,crate::domain::LaunchTarget)>> {
    let(project,publication)=publication(project,budget)?;
    crate::store::identity_inventory::read_pane_targets(&project.join(".state/state.db"),&publication,budget,pane)
}
pub fn read_launch_target_inventory(project:&Path,budget:&mut Budget)->Result<Vec<(String,crate::domain::LaunchTarget)>> {
    let(project,publication)=publication(project,budget)?;
    crate::store::identity_inventory::read_launch_targets(&project.join(".state/state.db"),&publication,budget)
}
/// Historical worktree paths remain references even when receipt publication
/// failed or the attempt stopped. Filesystem absence is not a release receipt.
pub fn read_worktree_inventory(project:&Path,budget:&mut Budget)->Result<Vec<(String,crate::domain::WorktreePlan)>> {
    let(project,publication)=publication(project,budget)?;
    crate::store::identity_inventory::read_worktrees(&project.join(".state/state.db"),&publication,budget)
}
/// Exact launch provenance for verification/recovery after allocation. Does not
/// replace the root-wide conflict inventory used when allocating resources.
pub(crate) fn read_worktree_operation(project:&Path,operation:&crate::domain::OperationId,budget:&mut Budget)->Result<Vec<(String,crate::domain::WorktreePlan)>> {
    let(project,publication)=publication(project,budget)?;
    crate::store::identity_inventory::read_worktree_operation(&project.join(".state/state.db"),&publication,budget,operation)
}
pub fn read_routine_execution_hint(project:&Path,budget:&mut Budget,last:Option<&crate::domain::OperationId>,now:i64)->Result<Option<crate::store::controller_hint::RoutineExecutionHint>> {
    let(project,publication)=publication(project,budget)?;
    crate::store::controller_hint::read_routine(&project.join(".state/state.db"),&publication,budget,last,now)
}
pub fn read_controller_effect_hint(project:&Path,budget:&mut Budget,turn:u64,now:i64)->Result<Option<crate::store::controller_hint::ControllerEffectHint>> {
    let(project,publication)=publication(project,budget)?;
    crate::store::controller_hint::read(&project.join(".state/state.db"),&publication,budget,turn,now)
}
/// Include already-reserved launch advancement in the bounded controller rotation.
/// This read-only selection grants no authority to create or resume resources.
pub fn read_controller_dispatch_hint(project:&Path,budget:&mut Budget,turn:u64,now:i64,include_launches:bool)->Result<Option<crate::store::controller_hint::ControllerEffectHint>> {
    let(project,publication)=publication(project,budget)?;
    crate::store::controller_hint::read_with_launches(&project.join(".state/state.db"),&publication,budget,turn,now,include_launches)
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
    fn controlled_open_matches_snapshot_and_rejects_bad_publication() {
        use crate::store::controlled::ReadControl;
        let(_root,p)=fixture();let expected=crate::runtime::snapshot(&p).unwrap();let control=||ReadControl::new(Instant::now()+Duration::from_secs(5),Default::default());
        assert_eq!(open_active_controlled(&p,control()).unwrap().read_snapshot(None).unwrap(),expected);
        assert_eq!(open_active_scoped(&p,control()).unwrap().read_snapshot(None).unwrap(),expected);
        let marker=p.join(".state/format.json");let original=fs::read(&marker).unwrap();let mut value:serde_json::Value=serde_json::from_slice(&original).unwrap();value["reconciliation_required"]=false.into();fs::write(&marker,serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(open_active_controlled(&p,control()).is_err());
        assert!(open_active_scoped(&p,control()).is_err());fs::write(marker,original).unwrap();
        let raw=rusqlite::Connection::open(p.join(".state/state.db")).unwrap();raw.execute("UPDATE migration_receipt SET source_digest=?1",["0".repeat(64)]).unwrap();
        assert!(open_active_controlled(&p,control()).is_err());
        assert!(open_active_scoped(&p,control()).is_err());
    }
    #[test]
    fn controlled_open_keeps_cancellation_and_publication_byte_limits() {
        use crate::store::{controlled::ReadControl,StoreError};
        let(_root,p)=fixture();let cancellation=crate::runner::Cancellation::default();cancellation.cancel();
        let error=open_active_controlled(&p,ReadControl::new(Instant::now()+Duration::from_secs(5),cancellation)).err().unwrap();assert!(matches!(error.downcast_ref::<StoreError>(),Some(StoreError::Cancelled)));
        let cancellation=crate::runner::Cancellation::default();cancellation.cancel();
        let error=open_active_scoped(&p,ReadControl::new(Instant::now()+Duration::from_secs(5),cancellation)).err().unwrap();assert!(matches!(error.downcast_ref::<StoreError>(),Some(StoreError::Cancelled)));
        fs::write(journal_path(&p),vec![b' ';16*1024*1024+1]).unwrap();let error=open_active_controlled(&p,ReadControl::new(Instant::now()+Duration::from_secs(5),Default::default())).err().unwrap();assert!(error.to_string().contains("budget"));
        let error=open_active_scoped(&p,ReadControl::new(Instant::now()+Duration::from_secs(5),Default::default())).err().unwrap();assert!(error.to_string().contains("budget"));
    }
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
    fn pane_candidates_ignore_retired_bindings_but_reject_unknown_routes() {
        use std::sync::{Arc,atomic::{AtomicU64,Ordering}};
        let(_root,p)=fixture();
        let mut raw=rusqlite::Connection::open(p.join(".state/state.db")).unwrap();
        let binding=crate::runtime::snapshot(&p).unwrap().runtime_bindings.remove(0);
        let mut work=Vec::new();let mut worktree_work=Vec::new();
        for history in [0,10_000] {
            let tx=raw.transaction().unwrap();
            for n in 0..history {
                let mut retired=binding.clone();
                let task=crate::domain::TaskId::new(format!("retired-{n}")).unwrap();
                retired.id=format!("task:{}",task.as_str());retired.task=Some(task.clone());
                retired.source_path=None;retired.source_digest=None;retired.session_source_digest=None;
                retired.identity=Default::default();
                tx.execute("INSERT INTO tasks(id,revision,state,title) VALUES(?1,1,'cancelled','retired')",[task.as_str()]).unwrap();
                let payload=serde_json::to_string(&retired).unwrap();
                tx.execute("INSERT INTO runtime_bindings(id,task_id,revision,source_path,payload,payload_hash) VALUES(?1,?2,?3,NULL,?4,?5)",rusqlite::params![retired.id,task.as_str(),retired.revision,payload,hash(payload.as_bytes())]).unwrap();
                // Retained reference metadata must not turn the completeness
                // check into a full-history scan before pane selection.
                tx.execute("INSERT INTO runtime_observations VALUES(?1,1,1,0,'{}',?2)",rusqlite::params![retired.id,"0".repeat(64)]).unwrap();
                tx.execute("INSERT INTO runtime_ownership VALUES(?1,1,1,NULL,'{}',?2)",rusqlite::params![retired.id,"0".repeat(64)]).unwrap();
                tx.execute("INSERT INTO legacy_sources VALUES(?1,'inbox',?2,?3)",rusqlite::params![format!("inbox/cold-{n}.json"),hash(b"{}"),b"{}".as_slice()]).unwrap();
            }
            tx.commit().unwrap();
            let mut limits=budget();let steps=Arc::new(AtomicU64::new(0));limits.sql_steps=Some(steps.clone());
            assert!(read_pane_bindings(&p,"wanted-pane",&mut limits).unwrap().is_empty());
            work.push(steps.load(Ordering::Relaxed));
            let mut limits=budget();let steps=Arc::new(AtomicU64::new(0));limits.sql_steps=Some(steps.clone());
            assert!(read_worktree_bindings(&p,&mut limits).unwrap().is_empty());
            worktree_work.push(steps.load(Ordering::Relaxed));
        }
        eprintln!("pane candidate SQL steps with 0/10000 retired bindings: {work:?}");
        assert!(work[1]<=work[0]+100,"pane inventory scanned retired bindings: {work:?}");
        eprintln!("worktree binding SQL steps with 0/10000 retired bindings and references: {worktree_work:?}");
        assert!(worktree_work[1]<=worktree_work[0]+100,"worktree binding inventory scanned history: {worktree_work:?}");
        assert!(read_identity_inventory(&p,&mut budget()).is_err(),"full inventory retains its record bound");
        let mut selected=binding.clone();selected.identity.pane_id="wanted-pane".into();
        let payload=serde_json::to_string(&selected).unwrap();
        raw.execute("UPDATE runtime_bindings SET payload=?1,payload_hash=?2 WHERE id=?3",rusqlite::params![payload,hash(payload.as_bytes()),selected.id]).unwrap();
        assert_eq!(read_pane_bindings(&p,"wanted-pane",&mut budget()).unwrap(),vec![selected.clone()]);
        raw.execute("UPDATE runtime_bindings SET payload_hash=?1 WHERE id=?2",rusqlite::params!["0".repeat(64),selected.id]).unwrap();
        assert!(read_pane_bindings(&p,"wanted-pane",&mut budget()).is_err());
        for bad in [serde_json::Value::Null,serde_json::json!(17),serde_json::json!([])] {
            let mut payload=serde_json::to_value(&selected).unwrap();payload["identity"]["pane_id"]=bad;
            let payload=payload.to_string();
            raw.execute("UPDATE runtime_bindings SET payload=?1,payload_hash=?2 WHERE id=?3",rusqlite::params![payload,hash(payload.as_bytes()),selected.id]).unwrap();
            assert!(read_pane_bindings(&p,"wanted-pane",&mut budget()).is_err(),"unknown pane must remain uncertainty");
        }
        let mut cancelled=budget();cancelled.cancellation.cancel();
        assert!(read_pane_bindings(&p,"wanted-pane",&mut cancelled).is_err());
        raw.execute("DELETE FROM runtime_bindings WHERE id=?1",[&selected.id]).unwrap();
        assert!(read_pane_bindings(&p,"wanted-pane",&mut budget()).unwrap_err().to_string().contains("missing imported bindings"));
    }

    #[test]
    fn identity_gap_backfill_and_imported_source_mutations_preserve_refusals() {
        let(_root,p)=fixture();let binding=crate::runtime::snapshot(&p).unwrap().runtime_bindings.remove(0);
        let mut raw=rusqlite::Connection::open(p.join(".state/state.db")).unwrap();
        crate::store::test_schema::historical(&raw,42).unwrap();
        let source:(String,String,Vec<u8>)=raw.query_row("SELECT kind,digest,bytes FROM legacy_sources WHERE path='.state/coordinator.json'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
        raw.execute("DELETE FROM runtime_bindings WHERE id=?1",[&binding.id]).unwrap();
        assert!(read_pane_bindings(&p,"unrelated-pane",&mut budget()).unwrap_err().to_string().contains("missing imported"));
        // Exercise SQL backfill on a historical fault fixture, independently
        // of the administrative upgrade service's integrity refusals.
        let tx=raw.transaction().unwrap();tx.execute_batch(include_str!("../../migrations/0043_admission_indexes.sql")).unwrap();tx.commit().unwrap();
        assert!(read_pane_bindings(&p,"unrelated-pane",&mut budget()).unwrap_err().to_string().contains("missing imported"));
        raw.execute("UPDATE legacy_sources SET kind='inbox' WHERE path='.state/coordinator.json'",[]).unwrap();
        assert!(read_pane_bindings(&p,"unrelated-pane",&mut budget()).unwrap().is_empty());
        raw.execute("UPDATE legacy_sources SET kind='runtime' WHERE path='.state/coordinator.json'",[]).unwrap();
        assert!(read_pane_bindings(&p,"unrelated-pane",&mut budget()).is_err());
        raw.execute("DELETE FROM legacy_sources WHERE path='.state/coordinator.json'",[]).unwrap();
        let gaps:u64=raw.query_row("SELECT count(*) FROM identity_reference_gaps",[],|r|r.get(0)).unwrap();assert_eq!(gaps,0);
        raw.execute("INSERT INTO legacy_sources VALUES('.state/coordinator.json',?1,?2,?3)",rusqlite::params![source.0,source.1,source.2]).unwrap();
        assert!(read_pane_bindings(&p,"unrelated-pane",&mut budget()).is_err());
        let payload=serde_json::to_string(&binding).unwrap();
        raw.execute("INSERT INTO runtime_bindings VALUES(?1,?2,?3,?4,?5,?6)",rusqlite::params![binding.id,binding.task.as_ref().map(crate::domain::TaskId::as_str),binding.revision,binding.source_path,payload,hash(payload.as_bytes())]).unwrap();
        assert!(read_pane_bindings(&p,"unrelated-pane",&mut budget()).unwrap().is_empty());
    }

    #[test]
    fn local_worktree_selection_retains_paths_and_refuses_unknown_identity() {
        let(_root,p)=fixture();let mut binding=crate::runtime::snapshot(&p).unwrap().runtime_bindings.remove(0);
        binding.identity.worktree_path=p.join("retained-but-absent").display().to_string();
        let raw=rusqlite::Connection::open(p.join(".state/state.db")).unwrap();
        let write=|binding:&crate::domain::RuntimeBinding| {
            let payload=serde_json::to_string(binding).unwrap();
            raw.execute("UPDATE runtime_bindings SET payload=?1,payload_hash=?2 WHERE id=?3",rusqlite::params![payload,hash(payload.as_bytes()),binding.id]).unwrap();
        };
        write(&binding);assert_eq!(read_worktree_bindings(&p,&mut budget()).unwrap(),vec![binding.clone()]);
        binding.identity.machine="remote".into();write(&binding);
        assert!(read_worktree_bindings(&p,&mut budget()).unwrap().is_empty());
        for field in ["machine","worktree_path"] {
            let mut value=serde_json::to_value(&binding).unwrap();value["identity"][field]=serde_json::Value::Null;
            let payload=value.to_string();raw.execute("UPDATE runtime_bindings SET payload=?1,payload_hash=?2 WHERE id=?3",rusqlite::params![payload,hash(payload.as_bytes()),binding.id]).unwrap();
            assert!(read_worktree_bindings(&p,&mut budget()).is_err());
        }
        binding.identity.machine.clear();write(&binding);
        raw.execute("UPDATE runtime_bindings SET payload_hash=?1 WHERE id=?2",rusqlite::params!["0".repeat(64),binding.id]).unwrap();
        assert!(read_worktree_bindings(&p,&mut budget()).is_err());write(&binding);
        let mut cancelled=budget();cancelled.cancellation.cancel();assert!(read_worktree_bindings(&p,&mut cancelled).is_err());
        crate::store::test_schema::historical(&raw,42).unwrap();
        assert_eq!(read_worktree_bindings(&p,&mut budget()).unwrap(),vec![binding]);
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
            assert!(read_pane_bindings(&p,"unrelated-pane",&mut budget()).unwrap_err().to_string().contains("dangling"));
            let payload=serde_json::to_string(&binding).unwrap();
            db.execute("INSERT INTO runtime_bindings VALUES(?1,?2,?3,?4,?5,?6)",rusqlite::params![binding.id,binding.task.as_ref().map(TaskId::as_str),binding.revision,binding.source_path,payload,hash(payload.as_bytes())]).unwrap();
            assert!(read_pane_bindings(&p,"unrelated-pane",&mut budget()).unwrap().is_empty());
            db.execute_batch("SAVEPOINT reference_move").unwrap();
            db.execute(&format!("UPDATE {table} SET binding_id='missing-reference' WHERE binding_id=?1"),[&binding.id]).unwrap();
            let gaps:u64=db.query_row("SELECT count(*) FROM identity_reference_gaps",[],|r|r.get(0)).unwrap();assert_eq!(gaps,1);
            // A separate reader sees committed evidence, not this savepoint.
            assert!(read_pane_bindings(&p,"unrelated-pane",&mut budget()).unwrap().is_empty());
            db.execute_batch("ROLLBACK TO reference_move; RELEASE reference_move").unwrap();
            let gaps:u64=db.query_row("SELECT count(*) FROM identity_reference_gaps",[],|r|r.get(0)).unwrap();assert_eq!(gaps,0);
            db.execute(&format!("UPDATE {table} SET binding_id='missing-reference' WHERE binding_id=?1"),[&binding.id]).unwrap();
            assert!(read_pane_bindings(&p,"unrelated-pane",&mut budget()).unwrap_err().to_string().contains("dangling"));
            db.execute(&format!("DELETE FROM {table} WHERE binding_id='missing-reference'"),[]).unwrap();
            assert!(read_pane_bindings(&p,"unrelated-pane",&mut budget()).unwrap().is_empty());
        }
    }

}

#[cfg(test)]
#[path="controller_hint_tests.rs"]
mod controller_hint_tests;

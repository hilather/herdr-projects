//! Root-scoped resource conflict checks. Cross-root capacity is deliberately not promised.
use std::{fs,path::{Path,PathBuf}};
use anyhow::{Context,Result,ensure};
use crate::{paths::Ctx,project::{self,Coordinator},thread::Thread};
use herdr_projects::{domain::RuntimeIdentity,migration,runtime};

fn exists(path:&Path)->Result<bool> {match fs::symlink_metadata(path){Ok(_)=>Ok(true),Err(e) if e.kind()==std::io::ErrorKind::NotFound=>Ok(false),Err(e)=>Err(e.into())}}
fn optional_json<T:serde::de::DeserializeOwned>(path:&Path)->Result<Option<T>> {if !exists(path)?{return Ok(None);}Ok(Some(serde_json::from_slice(&migration::read_plan_file(path)?)?))}
fn location(path:&str)->Result<PathBuf> {
    let path=Path::new(path);ensure!(path.is_absolute(),"recorded resource path must be absolute");
    match fs::canonicalize(path) {Ok(p)=>Ok(p),Err(e) if e.kind()==std::io::ErrorKind::NotFound=>{ensure!(path.components().all(|c|matches!(c,std::path::Component::RootDir|std::path::Component::Normal(_))),"unresolved resource path contains aliases");Ok(path.into())},Err(e)=>Err(e.into())}
}
fn conflict(a:&RuntimeIdentity,b:&RuntimeIdentity)->Result<bool> {
    if !a.machine.is_empty()||!b.machine.is_empty(){return Ok(false);}
    if !a.pane_id.is_empty()&&a.pane_id==b.pane_id {
        ensure!(!b.socket.is_empty(),"another pane reference lacks a recorded session socket");
        if location(&a.socket)?==location(&b.socket)? {return Ok(true);}
    }
    if !a.worktree_path.is_empty()&&!b.worktree_path.is_empty() {
        let a=location(&a.worktree_path)?;let b=location(&b.worktree_path)?;
        if a.starts_with(&b)||b.starts_with(&a){return Ok(true);}
    }
    Ok(false)
}

/// The caller holds the execution lease. Corrupt, migrating or over-limit roots
/// refuse instead of silently skipping records. References are conservative,
/// including resources left attached to resolved legacy threads.
pub(crate) fn check_conflicts(ctx:&Ctx,current:&Path,skip:Option<&str>,candidate:&RuntimeIdentity)->Result<()> {
    check_references(ctx,current,skip,|identity|conflict(candidate,identity))
}

// Resolve existing ancestors too: a missing child below a symlink still
// references its real parent. Dangling aliases cannot establish non-overlap.
fn removal_location(value:&str)->Result<PathBuf> {
    let path=Path::new(value);
    ensure!(path.is_absolute()&&path.components().all(|c|matches!(c,std::path::Component::RootDir|std::path::Component::Normal(_))),"cleanup reference contains unresolved aliases");
    let mut prefix=path;let mut suffix=Vec::new();
    loop {
        match prefix.canonicalize() {
            Ok(mut resolved)=>{for part in suffix.iter().rev(){resolved.push(part);}return Ok(resolved);}
            Err(error) if error.kind()==std::io::ErrorKind::NotFound=>{
                match fs::symlink_metadata(prefix) {
                    Err(missing) if missing.kind()==std::io::ErrorKind::NotFound=>{},
                    Ok(_)=>anyhow::bail!("cleanup reference contains a dangling alias"),
                    Err(error)=>return Err(error.into()),
                }
                suffix.push(prefix.file_name().context("cleanup reference has no resolvable ancestor")?);
                prefix=prefix.parent().context("cleanup reference has no parent")?;
            }
            Err(error)=>return Err(error.into()),
        }
    }
}

/// Cleanup and reopen include cwd/output references, not just worktree claims.
/// Retained canonical plans still protect files after their attempt stops.
pub(crate) fn check_worktree_references(ctx:&Ctx,current:&Path,id:&str,path:&Path)->Result<()> {
    let path=removal_location(path.to_str().context("worktree path is not UTF-8")?)?;
    check_references(ctx,current,Some(&format!("thread:{id}")),|identity| {
        if !identity.machine.is_empty(){return Ok(false);}
        for reference in [&identity.worktree_path,&identity.cwd,&identity.thread_dir] {
            if reference.is_empty(){continue;}
            let other=removal_location(reference)?;
            if path.starts_with(&other)||other.starts_with(&path){return Ok(true);}
        }
        Ok(false)
    })
}

fn check_references(ctx:&Ctx,current:&Path,skip:Option<&str>,conflicts:impl Fn(&RuntimeIdentity)->Result<bool>)->Result<()> {
    if !exists(&ctx.root)? { return Ok(()); }
    let current=current.canonicalize()?;let mut projects=0;let mut records=0;
    let mut target_budget=herdr_projects::store::identity_inventory::Budget::new(50*1024*1024,1024,std::time::Instant::now()+std::time::Duration::from_secs(10),Default::default())?;
    for entry in fs::read_dir(&ctx.root)?.take(1025) {
        let entry=entry?;projects+=1;ensure!(projects<=1024,"root enumeration exceeds 1024 entries");
        let kind=entry.file_type()?;if !kind.is_dir()&&!kind.is_symlink(){continue;}
        let name=entry.file_name();let name=name.to_str().context("non-UTF-8 root entry")?;
        if name.starts_with('.') {continue;}
        let dir=entry.path();let marker=exists(&dir.join("PROJECT.md"))?;
        let recognized=exists(&dir.join(".state/format.json"))?||exists(&dir.join(".state/migration"))?||exists(&dir.join(".state/project.json"))?||exists(&dir.join(".state/coordinator.json"))?;
        ensure!(marker||!recognized,"recognizable project {name} is missing PROJECT.md; repair it before adoption");
        if !marker{continue;}
        project::validate_slug(name)?;ensure!(kind.is_dir(),"project root contains a symlink or non-directory");
        ensure!(fs::symlink_metadata(dir.join("PROJECT.md"))?.is_file(),"PROJECT.md must be a regular file");
        ensure!(fs::symlink_metadata(dir.join(".state"))?.is_dir(),"project state directory must be real");
        let dir=dir.canonicalize()?;
        let bindings:Vec<(String,RuntimeIdentity)>=if exists(&dir.join(".state/format.json"))?||exists(&dir.join(".state/migration"))? {
            let mut identities:Vec<_>=migration::read_identity_inventory(&dir,&mut target_budget)?.into_iter().map(|b|(b.id,b.identity)).collect();
            for (owner,target) in migration::read_launch_target_inventory(&dir,&mut target_budget)? {
                identities.push((owner,RuntimeIdentity{machine:target.route.machine,socket:target.route.socket,workspace_id:target.route.workspace_id,tab_id:target.route.tab_id,pane_id:target.route.pane_id,cwd:target.route.cwd,..Default::default()}));
            }
            for (owner,plan) in migration::read_worktree_inventory(&dir,&mut target_budget)? {
                identities.push((owner,RuntimeIdentity{repo:plan.source.repository,branch:plan.branch,worktree_path:plan.path,..Default::default()}));
            }
            identities
        }else{
            let coordinator:Option<Coordinator>=optional_json(&dir.join(".state/coordinator.json"))?;
            let socket=coordinator.as_ref().map(|c|c.socket.clone()).unwrap_or_default();
            let mut result=Vec::new();if let Some(c)=coordinator {result.push(("coordinator".into(),RuntimeIdentity{socket:c.socket,workspace_id:c.workspace_id,tab_id:c.tab_id,pane_id:c.pane_id,cwd:c.cwd,agent_name:c.agent_name,..Default::default()}));}
            ensure!(fs::symlink_metadata(dir.join("threads"))?.is_dir(),"thread directory must be real");
            for (n,file) in fs::read_dir(dir.join("threads"))?.enumerate() {
                ensure!(n<256,"thread inventory exceeds 256 entries");let file=file?;let path=file.path();if path.extension().is_none_or(|s|s!="toml"){continue;}
                let thread:Thread=toml::from_str(std::str::from_utf8(&migration::read_plan_file(&path)?)?)?;crate::thread::validate_id(&thread.id)?;ensure!(path.file_stem().and_then(|s|s.to_str())==Some(thread.id.as_str()),"thread filename identity mismatch");
                result.push((format!("thread:{}",thread.id),RuntimeIdentity{machine:thread.machine,socket:socket.clone(),workspace_id:thread.workspace_id,tab_id:thread.tab_id,pane_id:thread.pane_id,cwd:thread.cwd,worktree_path:thread.worktree_path,thread_dir:thread.thread_dir,..Default::default()}));
            }
            result
        };
        for (id,identity) in bindings {records+=1;ensure!(records<=1024,"resource inventory exceeds 1024 bindings");if dir==current&&skip==Some(id.as_str()){continue;}ensure!(!conflicts(&identity)?,"resource is already referenced by {name}/{id}");}
    }
    Ok(())
}

pub fn adopt(ctx:&Ctx,path:&Path,id:&str,revision:u64,head:u64)->Result<herdr_projects::store::OwnershipChange> {
    let path=path.canonicalize()?;let _lease=crate::cleanup::lease(path.parent().context("project has no root")?)?;
    let snapshot=runtime::snapshot(&path)?;ensure!(snapshot.head==head,"project head changed");
    let binding=snapshot.runtime_bindings.iter().find(|b|b.id==id&&b.revision==revision).context("binding revision changed")?;
    let config=crate::notification_delivery::config(ctx,&path)?;
    check_conflicts(ctx,&path,Some(id),&binding.identity)?;
    let batch=crate::reconcile_live::collect(ctx,&path)?;ensure!(batch.expected_head==head,"project changed during adoption");
    ensure!(migration::config_reference(Path::new(&config.path))?==config,"config changed during adoption");
    let head=runtime::record_observations_held(&path,&batch)?;
    runtime::adopt_observed(&path,id,revision,head,&config)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use crate::{scenarios::World,runner::fake::ok};
    use herdr_projects::domain::{ProjectState,RuntimeRoute};
    pub(crate) fn fixture()->(World,PathBuf,UnixListener) {
        let world=World::new();let project=project::create(&world.root,"owned","",vec![]).unwrap();project.set_status(project::Status::Paused).unwrap();fs::write(project.dir().join("threads/t-0001.toml"),"id='t-0001'\nstatus='resolved'\n").unwrap();let path=project.dir().canonicalize().unwrap();let plan=migration::inspect(&path).unwrap();migration::apply(&path,&plan,true).unwrap();
        let socket=world.home.path().join("session.sock");let listener=UnixListener::bind(&socket).unwrap();let cwd=world.home.path().join("work");fs::create_dir(&cwd).unwrap();let snapshot=runtime::snapshot(&path).unwrap();runtime::rebind(&path,"thread:t-0001",1,snapshot.head,&RuntimeRoute{socket:socket.to_str().unwrap().into(),workspace_id:"w".into(),tab_id:"t".into(),pane_id:"p".into(),cwd:cwd.to_str().unwrap().into(),..Default::default()}).unwrap();
        *world.panes.borrow_mut()=serde_json::json!([{"pane_id":"p","workspace_id":"w","tab_id":"t","cwd":cwd}]).to_string();*world.agents.borrow_mut()=serde_json::json!([{"pane_id":"p","workspace_id":"w","tab_id":"t","cwd":cwd,"agent":"claude","name":"fixture-agent","agent_status":"working"}]).to_string();world.runner.on("--version",ok("herdr 0.9.1"));(world,path,listener)
    }
    #[test]
    fn adoption_counts_live_worker_and_resume_requires_fresh_post_adoption_evidence() {
        let(world,path,_listener)=fixture();let head=runtime::snapshot(&path).unwrap().head;let change=adopt(&world.ctx(),&path,"thread:t-0001",2,head).unwrap();assert_eq!(change.ownership.origin,"adopted");assert!(change.ownership.session.is_some());assert!(change.ownership.attempt.is_some());let after=runtime::snapshot(&path).unwrap();assert_eq!(after.attempts.len(),1);assert!(after.attempts[0].retains_capacity());assert_eq!(after.tasks.iter().find(|t|t.id==after.attempts[0].task).unwrap().active_attempt.as_ref(),Some(&after.attempts[0].id));
        let config=world.ctx().config_dir.join("config.toml");assert!(runtime::set_state(&path,after.head,after.control.unwrap().revision,ProjectState::Active,&config).is_err());crate::reconcile_live::run(&world.ctx(),&path,true).unwrap();let before=runtime::snapshot(&path).unwrap();assert!(runtime::admission(&path,&config).unwrap().blockers.is_empty());let active=runtime::set_state(&path,before.head,before.control.unwrap().revision,ProjectState::Active,&config).unwrap();assert_eq!(active.control.state,ProjectState::Active);
        let head=runtime::snapshot(&path).unwrap().head;let again=adopt(&world.ctx(),&path,"thread:t-0001",2,head).unwrap();assert_eq!(again.ownership,change.ownership);assert_eq!(runtime::snapshot(&path).unwrap().attempts.len(),1);assert_eq!(world.runner.count("agent prompt"),0);
    }
    #[test]
    fn replaced_session_and_changed_agent_pause_without_releasing_capacity() {
        for change in ["session","agent"] {
            let(world,path,listener)=fixture();let head=runtime::snapshot(&path).unwrap().head;adopt(&world.ctx(),&path,"thread:t-0001",2,head).unwrap();crate::reconcile_live::run(&world.ctx(),&path,true).unwrap();let before=runtime::snapshot(&path).unwrap();let active=runtime::set_state(&path,before.head,before.control.unwrap().revision,ProjectState::Active,&world.ctx().config_dir.join("config.toml")).unwrap();
            let _replacement=if change=="session" {drop(listener);fs::remove_file(world.home.path().join("session.sock")).unwrap();Some(UnixListener::bind(world.home.path().join("session.sock")).unwrap())}else{let changed=world.agents.borrow().replace("fixture-agent","replacement-agent");*world.agents.borrow_mut()=changed;None};
            crate::reconcile_live::run(&world.ctx(),&path,true).unwrap();let after=runtime::snapshot(&path).unwrap();assert_eq!(after.control.as_ref().unwrap().state,ProjectState::Paused);assert!(after.control.as_ref().unwrap().epoch>active.control.epoch);assert!(after.attempts[0].retains_capacity());assert!(!runtime::admission(&path,&world.ctx().config_dir.join("config.toml")).unwrap().blockers.is_empty());assert!(runtime::rebind(&path,"thread:t-0001",2,after.head,&RuntimeRoute::default()).is_err());
        }
    }
    #[test]
    fn adoption_blocks_canonical_legacy_corrupt_and_alias_conflicts() {
        for other in ["canonical","legacy","corrupt","alias","missing-marker"] {
            let(world,path,_listener)=fixture();let p=project::create(&world.root,"other","",vec![]).unwrap();p.set_status(project::Status::Paused).unwrap();let socket=world.home.path().join("session.sock");let route=if other=="alias" {let alias=world.home.path().join("alias.sock");std::os::unix::fs::symlink(&socket,&alias).unwrap();alias}else{socket};
            fs::write(p.dir().join(".state/coordinator.json"),serde_json::json!({"socket":route,"pane_id":"p","workspace_id":"w","tab_id":"t","cwd":world.home.path().join("work")}).to_string()).unwrap();
            if other=="corrupt" {fs::write(p.dir().join("threads/t-0001.toml"),"bad [").unwrap();}
            if matches!(other,"canonical"|"missing-marker") {
                fs::write(p.dir().join(".state/coordinator.json"),"{}").unwrap();let plan=migration::inspect(&p.dir()).unwrap();migration::apply(&p.dir(),&plan,true).unwrap();let snapshot=runtime::snapshot(&p.dir()).unwrap();let binding=runtime::snapshot(&path).unwrap().runtime_bindings.remove(0);runtime::rebind(&p.dir(),"coordinator",1,snapshot.head,&RuntimeRoute::from_identity(&binding.identity)).unwrap();
            }
            if other=="missing-marker" {fs::remove_file(p.project_md()).unwrap();}
            let before=runtime::snapshot(&path).unwrap();assert!(adopt(&world.ctx(),&path,"thread:t-0001",2,before.head).is_err(),"{other}");assert_eq!(runtime::snapshot(&path).unwrap(),before);
        }
    }
    #[test]
    fn ownership_refuses_older_schema_neighbor_and_owned_coordinator_rebind() {
        let(world,path,_listener)=fixture();let neighbor=project::create(&world.root,"old","",vec![]).unwrap();neighbor.set_status(project::Status::Paused).unwrap();let plan=migration::inspect(&neighbor.dir()).unwrap();migration::apply(&neighbor.dir(),&plan,true).unwrap();let raw=rusqlite::Connection::open(neighbor.state_dir().join("state.db")).unwrap();raw.execute_batch("DROP TABLE IF EXISTS native_profiles; DROP TABLE IF EXISTS memory_update_receipts; DROP TABLE IF EXISTS memory_delivery_intents; DROP TABLE IF EXISTS memory_import_decisions; DROP TABLE IF EXISTS memory_import_candidates; DROP TABLE IF EXISTS memory_snapshot_inputs; DROP TABLE memory_invalidations; DROP TABLE memory_promotions; DROP TABLE review_decisions; DROP TABLE proposal_validations; DROP TABLE memory_proposals; DROP TABLE coordinator_checkpoints; DROP TABLE coordinator_sessions; DROP TABLE memory_subscriptions; DROP TABLE snapshot_entries; DROP TABLE memory_snapshots; DROP TABLE memory_validity; DROP TABLE memory_dependencies; DROP TABLE memory_heads; DROP TABLE memory_revisions; DROP TABLE memory_records; DROP TABLE objects; DROP TABLE authority_denials; DROP TABLE memory_policies; DROP TABLE routine_occurrences; DROP TABLE routine_cursors; DROP TABLE routine_revisions; DROP TABLE budget_policies; DROP TABLE approval_uses; DROP TABLE approval_revocations; DROP TABLE approval_grants; DROP TRIGGER operation_delivery_monotonic; DROP TABLE attempt_cancellations; DROP TABLE attempt_inputs; DROP TABLE task_dependencies; DROP TABLE task_queue; DROP TABLE scheduler_policy; DROP TABLE runtime_ownership; DROP TABLE project_control; DROP TABLE runtime_observations; DROP TABLE runtime_bindings; UPDATE store_meta SET schema_version=4; PRAGMA user_version=4;").unwrap();let before=runtime::snapshot(&path).unwrap();assert!(adopt(&world.ctx(),&path,"thread:t-0001",2,before.head).is_err());assert_eq!(runtime::snapshot(&path).unwrap(),before);migration::upgrade_active(&neighbor.dir()).unwrap();
        let route=RuntimeRoute::from_identity(&before.runtime_bindings[0].identity);runtime::rebind(&path,"thread:t-0001",2,before.head,&RuntimeRoute::default()).unwrap();let head=runtime::snapshot(&path).unwrap().head;runtime::create_binding(&path,None,None,head,&route).unwrap();let head=runtime::snapshot(&path).unwrap().head;adopt(&world.ctx(),&path,"coordinator",1,head).unwrap();let before=runtime::snapshot(&path).unwrap();assert!(runtime::rebind(&path,"coordinator",1,before.head,&RuntimeRoute::default()).is_err());assert_eq!(runtime::snapshot(&path).unwrap(),before);
    }
    #[test]
    fn ownership_verifies_native_worktree_association_and_replacement() {
        use crate::runner::{Runner,RealRunner,Cmd};use std::time::Duration;
        for mode in ["valid","prunable","replace"] {
            let world=World::new();let repo=world.home.path().join("repo");let tree=world.home.path().join("tree");fs::create_dir(&repo).unwrap();
            for args in [vec!["init","--quiet"],vec!["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","commit","--quiet","--allow-empty","-m","base"],vec!["worktree","add","--quiet","-b","topic",tree.to_str().unwrap()]] {assert!(RealRunner.run(&Cmd::new("git",Duration::from_secs(10)).args(["-C",repo.to_str().unwrap()]).args(args)).unwrap().success());}
            let project=project::create(&world.root,"tree-owner","",vec![]).unwrap();project.set_status(project::Status::Paused).unwrap();let thread=Thread{id:"t-0001".into(),status:crate::thread::Status::Resolved,repo:repo.to_str().unwrap().into(),branch:"topic".into(),worktree_path:tree.to_str().unwrap().into(),..Default::default()};fs::write(project.dir().join("threads/t-0001.toml"),toml::to_string(&thread).unwrap()).unwrap();let path=project.dir().canonicalize().unwrap();let plan=migration::inspect(&path).unwrap();migration::apply(&path,&plan,true).unwrap();
            if mode=="prunable" {fs::remove_file(tree.join(".git")).unwrap();}
            let replace=mode=="replace";let tree_for_hook=tree.clone();let once=std::cell::Cell::new(false);world.runner.on_fn(|c|c.program=="git",move |c|{let output=RealRunner.run(c)?;if replace&&!once.get()&&c.args.iter().any(|a|a=="worktree")&&c.args.iter().any(|a|a=="list") {once.set(true);let gitfile=fs::read(tree_for_hook.join(".git"))?;fs::rename(&tree_for_hook,tree_for_hook.with_file_name("previous-tree"))?;fs::create_dir(&tree_for_hook)?;fs::write(tree_for_hook.join(".git"),gitfile)?;}Ok(output)});
            let before=runtime::snapshot(&path).unwrap();let result=adopt(&world.ctx(),&path,"thread:t-0001",1,before.head);let after=runtime::snapshot(&path).unwrap();
            if mode=="valid" {assert!(result.is_ok(),"{result:?}");assert!(after.ownership[0].worktree.is_some());assert!(after.attempts.is_empty());let revision=after.tasks[0].revision;runtime::relinquish(&path,"thread:t-0001",1,after.head,"return adopted tree").unwrap();let released=runtime::snapshot(&path).unwrap();assert_eq!(released.tasks[0].revision,revision+1);assert!(released.ownership.is_empty());assert!(tree.join(".git").exists());}else{assert!(result.is_err(),"{mode}");assert!(after.ownership.is_empty());assert_eq!(after.tasks,before.tasks);}
        }
    }
    #[test]
    fn relinquishment_retains_worker_and_uncertain_attempt_capacity() {
        let(world,path,_listener)=fixture();let head=runtime::snapshot(&path).unwrap().head;adopt(&world.ctx(),&path,"thread:t-0001",2,head).unwrap();
        let before=runtime::snapshot(&path).unwrap();assert!(runtime::relinquish(&path,"thread:t-0001",1,before.head,"hand back resources").is_err());assert_eq!(runtime::snapshot(&path).unwrap(),before);
        let mut db=migration::open_active(&path).unwrap();let mut attempt=before.attempts[0].clone();attempt.state=herdr_projects::domain::AttemptState::Lost;attempt.revision+=1;let mut task=before.tasks.iter().find(|t|t.id==attempt.task).unwrap().clone();let rev=task.revision;task.revision+=1;task.state=herdr_projects::domain::TaskState::Blocked;task.active_attempt=None;
        db.commit(herdr_projects::domain::Commit{expected_head:before.head,mutations:vec![herdr_projects::domain::Mutation::Attempt{expected:Some(1),next:attempt},herdr_projects::domain::Mutation::Task{expected:Some(rev),next:task}]}).unwrap();
        let before=runtime::snapshot(&path).unwrap();assert!(runtime::relinquish(&path,"thread:t-0001",1,before.head,"uncertain worker").is_err());assert_eq!(runtime::snapshot(&path).unwrap(),before);
    }
    #[test]
    fn relinquishment_is_atomic_fenced_and_never_reuses_claim_generation() {
        let(world,path,_listener)=fixture();let before=runtime::snapshot(&path).unwrap();let route=RuntimeRoute::from_identity(&before.runtime_bindings[0].identity);runtime::rebind(&path,"thread:t-0001",2,before.head,&RuntimeRoute::default()).unwrap();let head=runtime::snapshot(&path).unwrap().head;runtime::create_binding(&path,None,None,head,&route).unwrap();let head=runtime::snapshot(&path).unwrap().head;adopt(&world.ctx(),&path,"coordinator",1,head).unwrap();
        let before=runtime::snapshot(&path).unwrap();for (rev,head,reason) in [(2,before.head,"reason"),(1,before.head-1,"reason"),(1,before.head,"")] {assert!(runtime::relinquish(&path,"coordinator",rev,head,reason).is_err());assert_eq!(runtime::snapshot(&path).unwrap(),before);}
        let raw=rusqlite::Connection::open(path.join(".state/state.db")).unwrap();raw.execute_batch("CREATE TRIGGER reject_release BEFORE INSERT ON events WHEN NEW.kind='runtime.relinquished' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();assert!(runtime::relinquish(&path,"coordinator",1,before.head,"hand back").is_err());assert_eq!(runtime::snapshot(&path).unwrap(),before);raw.execute_batch("DROP TRIGGER reject_release").unwrap();
        runtime::relinquish(&path,"coordinator",1,before.head,"hand back").unwrap();let after=runtime::snapshot(&path).unwrap();assert!(after.ownership.is_empty());assert!(after.observations.iter().all(|o|o.binding!="coordinator"));assert_eq!(after.runtime_bindings,before.runtime_bindings);assert!(Path::new(&route.socket).exists());assert!(after.events.iter().any(|e|e.kind=="runtime.relinquished"&&e.payload["reason"]=="hand back"));assert!(after.control.as_ref().unwrap().epoch>before.control.as_ref().unwrap().epoch);
        let change=adopt(&world.ctx(),&path,"coordinator",1,after.head).unwrap();assert_eq!(change.ownership.revision,2);let head=runtime::snapshot(&path).unwrap().head;runtime::relinquish(&path,"coordinator",2,head,"release for rebind").unwrap();let head=runtime::snapshot(&path).unwrap().head;runtime::rebind(&path,"coordinator",1,head,&RuntimeRoute::default()).unwrap();
    }
    #[test]
    fn recovery_plan_is_read_only_and_keeps_live_or_changed_workers_reserved() {
        use herdr_projects::reconcile::plan::RepairAction;
        let(world,path,_listener)=fixture();let before=runtime::snapshot(&path).unwrap();let report=crate::reconcile_live::plan(&world.ctx(),&path).unwrap();assert!(report.items.iter().any(|i|i.entity=="thread:t-0001"&&i.action==RepairAction::AdoptResources));assert_eq!(runtime::snapshot(&path).unwrap(),before);
        adopt(&world.ctx(),&path,"thread:t-0001",2,before.head).unwrap();let before=runtime::snapshot(&path).unwrap();let report=crate::reconcile_live::plan(&world.ctx(),&path).unwrap();assert_eq!(report.retained_attempts,1);assert!(report.items.iter().any(|i|i.entity_kind=="attempt"&&i.action==RepairAction::None));assert_eq!(runtime::snapshot(&path).unwrap(),before);
        *world.agents.borrow_mut()="[]".into();let report=crate::reconcile_live::plan(&world.ctx(),&path).unwrap();assert_eq!(report.retained_attempts,1);assert!(!report.dispatch_allowed);assert!(report.items.iter().any(|i|i.entity_kind=="attempt"&&i.action==RepairAction::InspectTerminationEvidence));assert_eq!(runtime::snapshot(&path).unwrap(),before);assert_eq!(world.runner.count("agent prompt"),0);
    }
    #[test]
    fn remote_outage_remains_unknown_and_retains_lost_capacity_across_repeated_polling() {
        use herdr_projects::domain::{Attempt,AttemptId,AttemptState,Commit,Mutation};
        let(world,path,_socket)=fixture();let before=runtime::snapshot(&path).unwrap();let mut route=RuntimeRoute::from_identity(&before.runtime_bindings[0].identity);route.machine="offline-host".into();runtime::rebind(&path,"thread:t-0001",2,before.head,&route).unwrap();let before=runtime::snapshot(&path).unwrap();let task=before.runtime_bindings[0].task.clone().unwrap();
        let attempt=Attempt{id:AttemptId::new("remote-lost").unwrap(),task,revision:1,state:AttemptState::Lost,snapshot:None,reservation:"remote-slot".into(),termination_observed:false};migration::open_active(&path).unwrap().commit(Commit{expected_head:before.head,mutations:vec![Mutation::Attempt{expected:None,next:attempt}]}).unwrap();
        let remote_runner=crate::runner::fake::FakeRunner::new();remote_runner.on("--version",ok("herdr 0.9.1"));remote_runner.on("--machine offline-host",crate::runner::fake::fail(255,"fixture remote unavailable"));let ctx=Ctx{runner:&remote_runner,..world.ctx()};
        let before=runtime::snapshot(&path).unwrap();for _ in 0..2 {
            let batch=crate::reconcile_live::collect(&ctx,&path).unwrap();assert_eq!(batch.observations[0].pane,herdr_projects::reconcile::ResourceState::Unknown);assert!(!batch.observations[0].agent_present);assert!(batch.observations[0].session_identity.is_none());assert!(!crate::canonical_controller::poll(&ctx,&path,0).unwrap().reachable);
            let after=runtime::snapshot(&path).unwrap();assert_eq!(after.attempts,before.attempts);assert_eq!(after.runtime_bindings,before.runtime_bindings);assert_eq!(after.tasks,before.tasks);assert!(after.ownership.is_empty());let plan=crate::reconcile_live::plan(&ctx,&path).unwrap();assert_eq!(plan.retained_attempts,1);assert!(!plan.dispatch_allowed);
        }
        assert!(remote_runner.calls.borrow().iter().any(|c|c.args.windows(2).any(|a|a==["--machine","offline-host"])));assert_eq!(remote_runner.count("agent prompt"),0);assert_eq!(remote_runner.count("agent start"),0);
    }
    #[test]
    fn legacy_adoption_cannot_steal_a_canonical_reference() {
        let(world,path,_listener)=fixture();let binding=runtime::snapshot(&path).unwrap().runtime_bindings.remove(0);let herdr=crate::herdr::Herdr::new(world.env.herdr_bin(),&binding.identity.socket,&world.runner);assert!(crate::adopt::adoptable_agent(&world.ctx(),&herdr,&binding.identity.socket,"p").is_err());
    }
}

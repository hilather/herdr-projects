//! Read-only collection from recorded endpoints. This module never grants dispatch.
use std::{collections::BTreeMap,path::Path,time::Duration};
use anyhow::{Result,ensure};
use crate::{paths::Ctx,herdr::{self,Herdr,Pane,Agent},runner::Cmd};
use herdr_projects::{migration,reconcile::{ObservationBatch,RuntimeObservation,ResourceState as State},runtime};

fn pane_state(identity:&herdr_projects::domain::RuntimeIdentity,state:&Result<(Vec<Pane>,Vec<Agent>),String>)->(State,bool) {
    let Ok((panes,agents))=state else{return (State::Unknown,false);};
    let matching=panes.iter().filter(|p|p.pane_id==identity.pane_id).collect::<Vec<_>>();
    let agents=agents.iter().filter(|a|a.pane_id==identity.pane_id).collect::<Vec<_>>();
    if matching.len()>1 || agents.len()>1 {return (State::Unknown,false);}
    let Some(pane)=matching.first() else{return(if agents.is_empty(){State::Absent}else{State::Unknown},false);};
    if identity.workspace_id.is_empty()||identity.tab_id.is_empty()||identity.cwd.is_empty(){return(State::Unknown,false);}
    if pane.workspace_id!=identity.workspace_id||pane.tab_id!=identity.tab_id||pane.cwd!=identity.cwd{return(State::Mismatch,false);}
    if let Some(agent)=agents.first() {
        if agent.workspace_id!=identity.workspace_id||agent.tab_id!=identity.tab_id
            ||(!agent.cwd.is_empty()&&agent.cwd!=identity.cwd)
            ||(!identity.agent.is_empty()&&agent.agent!=identity.agent)
            ||(!identity.agent_name.is_empty()&&agent.name!=identity.agent_name) {return(State::Unknown,false);}
    }
    (State::Present,agents.len()==1)
}

pub fn collect(ctx:&Ctx,project:&Path)->Result<ObservationBatch> {
    let snapshot=runtime::snapshot(project)?;
    ensure!(snapshot.schema_version>=6,"upgrade-store is required for reconciliation observations");
    ensure!(snapshot.runtime_bindings.len()<=128,"more than 128 runtime bindings; collector refuses an incomplete batch");
    let config=std::path::absolute(ctx.config_dir.join("config.toml"))?;
    let config=migration::config_reference(&config)?;
    let started=jiff::Timestamp::now().as_millisecond();
    let needs_herdr=snapshot.runtime_bindings.iter().any(|b|!b.identity.pane_id.is_empty());
    let supported=!needs_herdr||herdr::version(&ctx.env.herdr_bin(),ctx.runner).is_ok_and(|v|v>=herdr::MIN_VERSION);
    let mut sessions:BTreeMap<(String,String),Result<(Vec<Pane>,Vec<Agent>),String>>=BTreeMap::new();
    let mut worktrees:BTreeMap<String,Option<Vec<(String,String)>>>=BTreeMap::new();
    let mut observations=Vec::new();
    for binding in &snapshot.runtime_bindings {
        let identity=&binding.identity;
        let (pane,agent_present)=if identity.pane_id.is_empty(){(State::Unrecorded,false)}
        else if !supported||!Path::new(&identity.socket).is_absolute(){(State::Unknown,false)}
        else {
            let key=(identity.socket.clone(),identity.machine.clone());
            if !sessions.contains_key(&key)&&sessions.len()<16 {
                let base=Herdr::new(ctx.env.herdr_bin(),&identity.socket,ctx.runner);let h=base.on_machine(&identity.machine);
                let state=h.pane_list().and_then(|panes|h.agent_list().map(|agents|(panes,agents))).map_err(|_|"session unavailable".into());sessions.insert(key.clone(),state);
            }
            sessions.get(&key).map(|s|pane_state(identity,s)).unwrap_or((State::Unknown,false))
        };
        let worktree=if identity.worktree_path.is_empty(){State::Unrecorded}
        else if !identity.machine.is_empty()||!Path::new(&identity.repo).is_absolute()||!Path::new(&identity.worktree_path).is_absolute()||identity.branch.is_empty(){State::Unknown}
        else {
            if !worktrees.contains_key(&identity.repo)&&worktrees.len()<16 {
                let output=ctx.runner.run(&Cmd::new("git",Duration::from_secs(10)).args(["-C",&identity.repo,"worktree","list","--porcelain","-z"]));
                let records=output.ok().filter(|o|o.success()).and_then(|o|parse_worktrees(&o.stdout));worktrees.insert(identity.repo.clone(),records);
            }
            match worktrees.get(&identity.repo).and_then(Option::as_ref) {
                None=>State::Unknown,
                Some(records)=>{
                    let matching=records.iter().filter(|(path,_)|path==&identity.worktree_path).collect::<Vec<_>>();
                    match matching.as_slice(){[]=>State::Absent,[(_,branch)]if *branch==format!("refs/heads/{}",identity.branch)=>State::Present,[_]=>State::Mismatch,_=>State::Unknown}
                }
            }
        };
        observations.push(RuntimeObservation{binding:binding.id.clone(),binding_revision:binding.revision,task_revision:binding.task.as_ref().and_then(|id|snapshot.tasks.iter().find(|t|&t.id==id).map(|t|t.revision)),observed_unix_ms:started,pane,worktree,agent_present,collector:"herdr-git-v1".into(),config_digest:config.digest.clone(),diagnostic:"Observation only: pane absence/idle is not termination, worktree identity is not preservation, and remote worktrees remain unverified. No ownership, capacity release or dispatch authorized.".into()});
    }
    ensure!(migration::config_reference(Path::new(&config.path))?==config,"config changed during observation; retry");
    ensure!(runtime::snapshot(project)?.head==snapshot.head,"project changed during observation; retry");
    Ok(ObservationBatch{expected_head:snapshot.head,observations,dispatch_allowed:false,recorded_head:None})
}

fn parse_worktrees(text:&str)->Option<Vec<(String,String)>> {
    // Empty/truncated output is not proof of absence. Git emits a NUL-separated
    // record terminator even for a single registered worktree.
    if !text.ends_with("\0\0") {return None;}
    let mut records=Vec::new();
    for block in text.strip_suffix("\0\0")?.split("\0\0") {
        let mut path=None;let mut branch=None;let mut detached=false;
        for field in block.split('\0') {
            if let Some(value)=field.strip_prefix("worktree "){if path.replace(value.to_string()).is_some(){return None;}}
            if let Some(value)=field.strip_prefix("branch "){if branch.replace(value.to_string()).is_some(){return None;}}
            if field=="detached"||field=="bare"{if detached{return None;}detached=true;}
        }
        if detached&&branch.is_some(){return None;}
        records.push((path.filter(|p|Path::new(p).is_absolute())?,if detached{String::new()}else{branch?}));
    }
    Some(records)
}

pub fn run(ctx:&Ctx,project:&Path,apply:bool)->Result<ObservationBatch> {
    let mut batch=collect(ctx,project)?;
    if apply {batch.recorded_head=Some(runtime::record_observations(project,&batch)?);}
    Ok(batch)
}


#[cfg(test)]
mod tests {
    use super::*;
    use herdr_projects::domain::RuntimeIdentity;
    #[test]
    fn pane_evidence_distinguishes_absence_unknown_mismatch_and_inconsistent_agents() {
        let identity=RuntimeIdentity{pane_id:"p".into(),workspace_id:"w".into(),tab_id:"t".into(),cwd:"/cwd".into(),..Default::default()};
        let pane=Pane{pane_id:"p".into(),workspace_id:"w".into(),tab_id:"t".into(),cwd:"/cwd".into()};
        let agent=Agent{pane_id:"p".into(),workspace_id:"w".into(),tab_id:"t".into(),cwd:"/cwd".into(),..Default::default()};
        assert_eq!(pane_state(&identity,&Err("unreachable".into())),(State::Unknown,false));
        assert_eq!(pane_state(&identity,&Ok((vec![],vec![]))),(State::Absent,false));
        assert_eq!(pane_state(&identity,&Ok((vec![],vec![agent.clone()]))),(State::Unknown,false));
        assert_eq!(pane_state(&identity,&Ok((vec![pane.clone(),pane.clone()],vec![]))),(State::Unknown,false));
        assert_eq!(pane_state(&identity,&Ok((vec![pane.clone()],vec![agent.clone()]))),(State::Present,true));
        let mut wrong=agent;wrong.workspace_id="wrong".into();assert_eq!(pane_state(&identity,&Ok((vec![pane.clone()],vec![wrong]))),(State::Unknown,false));
        let mut wrong=pane;wrong.cwd="/other".into();assert_eq!(pane_state(&identity,&Ok((vec![wrong],vec![]))),(State::Mismatch,false));
    }
    #[test]
    fn worktree_parser_rejects_incomplete_or_duplicate_identity() {
        assert!(parse_worktrees("worktree /a\0branch refs/heads/a\0detached\0\0").is_none());
        assert!(parse_worktrees("worktree /a\0bare\0bare\0\0").is_none());
        assert!(parse_worktrees("").is_none());assert!(parse_worktrees("worktree /a\0branch refs/heads/a\0").is_none());
        assert!(parse_worktrees("worktree /a\0worktree /b\0branch refs/heads/a\0\0").is_none());
        assert_eq!(parse_worktrees("worktree /a\0HEAD abc\0branch refs/heads/a\0\0").unwrap(),vec![("/a".into(),"refs/heads/a".into())]);
        assert_eq!(parse_worktrees("worktree /a\0HEAD abc\0detached\0\0").unwrap(),vec![("/a".into(),String::new())]);
    }
    #[test]
    fn collector_uses_store_provenance_and_never_calls_effect_commands() {
        use crate::{paths::Env,runner::fake::{FakeRunner,ok}};
        let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let project=crate::project::create(&root,"demo","",vec![]).unwrap();project.set_status(crate::project::Status::Paused).unwrap();
        std::fs::write(project.dir().join("threads/t-1.toml"),"id='t-1'\nstatus='resolved'\nrepo='/repo'\nbranch='topic'\nworktree_path='/worktree'\n").unwrap();
        let plan=migration::inspect(&project.dir()).unwrap();migration::apply(&project.dir(),&plan,true).unwrap();
        std::fs::write(project.dir().join("threads/t-1.toml"),"changed").unwrap();
        let env=Env::for_test(home.path(),&[]);let runner=FakeRunner::new();runner.on("worktree list",ok("worktree /worktree\0HEAD abc\0branch refs/heads/topic\0\0"));
        let ctx=Ctx{env:&env,root,config_dir:env.config_dir(),runner:&runner,detached_ticker:false};
        let batch=run(&ctx,&project.dir(),true).unwrap();assert!(batch.recorded_head.is_some());assert!(!batch.dispatch_allowed);assert_eq!(batch.observations[0].worktree,State::Present);assert_eq!(batch.observations[0].pane,State::Unrecorded);
        assert!(runner.calls.borrow().iter().all(|c|c.display().contains("worktree list")));
        assert_eq!(runtime::snapshot(&project.dir()).unwrap().observations,batch.observations);
    }
}

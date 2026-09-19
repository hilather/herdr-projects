//! Bounded, read-only migration observations. Never treats unreachable as absent.
use anyhow::{Result,Context};
use serde::Serialize;
use sha2::{Digest,Sha256};
use std::{collections::BTreeMap,path::Path};
use crate::{paths::Ctx,herdr::{self,Herdr,Pane,Agent},runner::Cmd};
use herdr_projects::migration;

#[derive(Debug,Serialize)]
pub struct Reference {pub path:String,pub present:bool,pub sha256:Option<String>,pub keys:Vec<String>}
#[derive(Debug,Serialize)]
pub struct Observation {pub record:String,pub machine:String,pub pane:String,pub state:String,pub diagnostic:String}
#[derive(Debug,Serialize)]
pub struct Report {
    pub source_digest:String,pub storage:migration::storage::Storage,pub references:Vec<Reference>,
    pub herdr_version:Option<String>,pub observations:Vec<Observation>,pub blockers:Vec<String>,pub warnings:Vec<String>,
}
fn reference(path:&Path,project:&Path)->Result<Reference> {
    match std::fs::symlink_metadata(path) {
        Err(e)if e.kind()==std::io::ErrorKind::NotFound=>Ok(Reference{path:path.display().to_string(),present:false,sha256:None,keys:Vec::new()}),
        Err(e)=>Err(e.into()),
        Ok(_)=>{
            let bytes=migration::read_plan_file(path)?;
            let value:toml::Value=toml::from_str(std::str::from_utf8(&bytes)?)?;
            crate::project::parse_safety(std::str::from_utf8(&bytes)?,project)?;
            let keys=value.as_table().context("config is not a TOML table")?.keys().cloned().collect();
            Ok(Reference{path:path.display().to_string(),present:true,sha256:Some(format!("{:x}",Sha256::digest(&bytes))),keys})
        }
    }
}
pub fn inspect(ctx:&Ctx,project:&Path)->Result<Report> {
    let plan=migration::inspect(project)?;
    let storage=migration::storage::inspect(&project.join(".state"),plan.sources.iter().map(|s|s.bytes).sum())?;
    let mut blockers=plan.blockers.clone();let mut warnings=plan.warnings.clone();
    if !storage.supported_type{blockers.push("filesystem type is unverified for SQLite".into());}
    if !storage.sufficient_space{blockers.push("available space is below the conservative migration estimate".into());}
    let mut references=Vec::new();
    match reference(&ctx.config_dir.join("config.toml"),&project.canonicalize()?) {Ok(r)=>references.push(r),Err(_)=>blockers.push("external config.toml is unreadable or invalid (contents withheld)".into())}
    let coordinator=project.join(".state/coordinator.json");
    let coordinator=if std::fs::symlink_metadata(&coordinator).is_ok(){Some(serde_json::from_slice::<serde_json::Value>(&migration::read_plan_file(&coordinator)?)?)}else{None};
    let socket=coordinator.as_ref().and_then(|c|c["socket"].as_str()).unwrap_or("");
    let mut records=Vec::new();
    if let Some(c)=&coordinator {
        if c["pane_id"].as_str().is_some_and(|p|!p.is_empty()){records.push((".state/coordinator.json".to_string(),String::new(),c["pane_id"].as_str().unwrap().to_string(),c["workspace_id"].as_str().unwrap_or("").to_string(),c["tab_id"].as_str().unwrap_or("").to_string(),c["cwd"].as_str().unwrap_or("").to_string()));}
    }
    if plan.sources.iter().filter(|s|s.kind=="thread").count()>128 {blockers.push("more than 128 thread records; detailed probes are capped".into());}
    for source in plan.sources.iter().filter(|s|s.kind=="thread").take(128) {
        let text=migration::read_plan_file(&project.join(&source.path))?;
        let record:crate::thread::Thread=match toml::from_str(std::str::from_utf8(&text)?) {Ok(r)=>r,Err(_)=>{blockers.push(format!("{}: typed thread validation failed",source.path));continue;}};
        if !record.pane_id.is_empty(){records.push((source.path.clone(),record.machine.clone(),record.pane_id,record.workspace_id,record.tab_id,record.cwd));}
        if !record.worktree_path.is_empty() {
            if !record.machine.is_empty(){blockers.push(format!("{}: remote worktree identity remains unverified",source.path));continue;}
            let output=ctx.runner.run(&Cmd::new("git",std::time::Duration::from_secs(10)).args(["-C",&record.repo,"worktree","list","--porcelain","-z"]));
            let verified=output.ok().filter(|o|o.success()).is_some_and(|o|o.stdout.split("\0\0").any(|block| {
                let fields=block.split('\0').collect::<Vec<_>>();fields.contains(&format!("worktree {}",record.worktree_path).as_str())&&fields.contains(&format!("branch refs/heads/{}",record.branch).as_str())
            }));
            if !verified{blockers.push(format!("{}: worktree path/branch ownership is absent or unverified",source.path));}
        }
    }
    let version=if records.is_empty(){None}else{match herdr::version(&ctx.env.herdr_bin(),ctx.runner){Ok(v)if v>=herdr::MIN_VERSION=>Some(v.to_string()),_=>{blockers.push("Herdr version unavailable or unsupported".into());None}}};
    type State=std::result::Result<(Vec<Pane>,Vec<Agent>),String>;
    let mut cache:BTreeMap<String,State>=BTreeMap::new();let mut observations=Vec::new();
    if records.len()>128{blockers.push("more than 128 recorded pane identities; split the preflight".into());records.truncate(128);}
    for(record,machine,pane,workspace,tab,cwd)in records {
        if !cache.contains_key(&machine) {
            let result=if socket.is_empty()||!Path::new(socket).is_absolute()||version.is_none(){Err("recorded absolute socket and compatible version required".into())}
            else if cache.len()>=16{Err("session observation limit exceeded".into())}
            else {
                let base=Herdr::new(ctx.env.herdr_bin(),socket,ctx.runner);let h=base.on_machine(&machine);
                h.pane_list().and_then(|panes|h.agent_list().map(|agents|(panes,agents))).map_err(|_|"session unreachable or malformed response; absence not established".into())
            };
            cache.insert(machine.clone(),result);
        }
        let(state,diagnostic)=match &cache[&machine] {
            Err(error)=>("unknown",error.clone()),
            Ok((panes,agents))=>match panes.iter().find(|p|p.pane_id==pane) {
                None if agents.iter().any(|a|a.pane_id==pane)=>("unknown","inconsistent pane/agent snapshots; repeat observation".into()),
                None=>("absent","pane absent from successfully queried recorded session; historical ownership still requires reconciliation".into()),
                Some(p)if workspace.is_empty()||tab.is_empty()||p.workspace_id!=workspace||p.tab_id!=tab||(!cwd.is_empty()&&p.cwd!=cwd)=>("mismatch","pane identity/cwd differs; do not reuse it".into()),
                Some(_)=>if agents.iter().any(|a|a.pane_id==pane){("present_agent","matching pane has an agent; idle UI does not prove quiescence".into())}else{("present_pane","matching pane exists; shell/process writer status remains unverified".into())},
            },
        };
        observations.push(Observation{record,machine,pane,state:state.into(),diagnostic});
    }
    warnings.push("Observations are a read-only snapshot, not cutover authorization; live identities remain blocked. Config fingerprints are diagnostic references, not copied settings or an authority grant.".into());
    if migration::inspect(project)?.digest!=plan.digest{blockers.push("project sources changed during preflight; repeat inspection".into());}
    Ok(Report{source_digest:plan.digest,storage,references,herdr_version:version,observations,blockers,warnings})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{paths::Env,runner::fake::{FakeRunner,ok,fail}};
    #[test]
    fn preflight_distinguishes_absent_unreachable_and_reused_panes_without_mutation() {
        for state in ["absent","unknown","mismatch","present_pane"] {
            let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let project=crate::project::create(&root,"demo","",vec![]).unwrap();project.set_status(crate::project::Status::Paused).unwrap();
            std::fs::write(project.state_dir().join("coordinator.json"),br#"{"socket":"/recorded/session.sock","pane_id":"p1","workspace_id":"w1","tab_id":"t1","cwd":"/repo"}"#).unwrap();
            let env=Env::for_test(home.path(),&[("HERDR_SESSION","wrong-inherited-session")]);let runner=FakeRunner::new();runner.on("--version",ok("herdr 0.9.1"));
            match state {
                "unknown"=>{runner.on("pane list",fail(1,"unreachable"));},
                "absent"=>{runner.on("pane list",ok(r#"{"result":{"panes":[]}}"#));},
                "mismatch"=>{runner.on("pane list",ok(r#"{"result":{"panes":[{"pane_id":"p1","workspace_id":"other","tab_id":"t1","cwd":"/repo"}]}}"#));},
                _=>{runner.on("pane list",ok(r#"{"result":{"panes":[{"pane_id":"p1","workspace_id":"w1","tab_id":"t1","cwd":"/repo"}]}}"#));},
            }
            runner.on("agent list",ok(r#"{"result":{"agents":[]}}"#));
            let ctx=Ctx{env:&env,root:root.clone(),config_dir:env.config_dir(),runner:&runner,detached_ticker:false};
            let before=migration::inspect(&project.dir()).unwrap();let report=inspect(&ctx,&project.dir()).unwrap();assert_eq!(report.observations[0].state,state);assert!(!report.blockers.is_empty());assert_eq!(migration::inspect(&project.dir()).unwrap(),before);
            assert!(runner.calls.borrow().iter().all(|c|!c.display().contains("start")&&!c.display().contains("prompt")));
            for call in runner.calls.borrow().iter().filter(|c|!c.display().contains("--version")) {assert!(call.env.iter().any(|(k,v)|k=="HERDR_SOCKET_PATH"&&v=="/recorded/session.sock"));}
            assert!(!project.state_dir().join("migration").exists());
        }
    }
    #[test]
    fn config_reports_fingerprints_not_values() {
        let home=tempfile::tempdir().unwrap();let root=home.path().join("root");let project=crate::project::create(&root,"demo","",vec![]).unwrap();project.set_status(crate::project::Status::Paused).unwrap();
        let env=Env::for_test(home.path(),&[]);std::fs::create_dir_all(env.config_dir()).unwrap();std::fs::write(env.config_dir().join("config.toml"),"private_value='secret-must-not-print'\n").unwrap();
        let runner=FakeRunner::new();let ctx=Ctx{env:&env,root,config_dir:env.config_dir(),runner:&runner,detached_ticker:false};let report=inspect(&ctx,&project.dir()).unwrap();
        let json=serde_json::to_string(&report).unwrap();assert!(json.contains("private_value"));assert!(!json.contains("secret-must-not-print"));assert!(report.references[0].sha256.is_some());assert!(runner.calls.borrow().is_empty());assert_eq!(report.storage.destination,project.state_dir().display().to_string());
    }
}

/// Validate safety against exactly the same fingerprint bound into the plan.
pub fn plan(ctx:&Ctx,project:&Path)->Result<migration::Plan> {
    let config=std::path::absolute(ctx.config_dir.join("config.toml"))?;
    let validated=reference(&config,&project.canonicalize()?)
        .map_err(|_|anyhow::anyhow!("external config is unreadable or invalid (contents withheld)"))?;
    let plan=migration::inspect_with_config(project,&config)?;
    anyhow::ensure!(plan.config.as_ref().unwrap().digest==validated.sha256,"external config changed during planning; repeat inspection");
    Ok(plan)
}

//! Canonical read-only probes commit under the ownership used for collection.
//! Queue replies report liveness only; they are never observation authority.
use std::{path::{Path,PathBuf},os::unix::fs::MetadataExt,sync::Arc,time::{Duration,Instant}};
use anyhow::{Result,Context,ensure};
use serde::{Serialize,Deserialize};
use crate::{paths::{Ctx,Env},runner::{Runner,Cmd,Output},source_tree::Control};
use herdr_projects::{runtime,migration,execution_guard::ProjectGuard,reconcile::ResourceState};
const JOB:&str="\0herdr-projects-canonical-observation";
const BUDGET:Duration=Duration::from_secs(15);
const LIMIT:usize=1024*1024;

fn observation_head(path:&Path)->Result<u64> {
    let mut budget=herdr_projects::store::identity_inventory::Budget::new(2*1024*1024,0,Instant::now()+Duration::from_millis(100),Default::default())?;
    migration::read_observation_head(path,&mut budget)
}

#[derive(Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {project:PathBuf,identity:(u64,u64),home:PathBuf,bin:String,config:PathBuf,config_digest:Option<String>}
impl Input {
    fn new(ctx:&Ctx,path:&Path)->Result<Self> {
        let project=path.canonicalize()?;let metadata=std::fs::metadata(&project)?;
        let config=std::path::absolute(&ctx.config_dir)?;let reference=migration::config_reference(&config.join("config.toml"))?;
        Ok(Self{project,identity:(metadata.dev(),metadata.ino()),home:std::path::absolute(&ctx.env.home)?,bin:ctx.env.herdr_bin(),config,config_digest:reference.digest})
    }
    fn current(&self,control:&Control)->Result<()> {
        control.check()?;ensure!(self.project.is_absolute()&&self.home.is_absolute()&&self.config.is_absolute()&&!self.bin.is_empty(),"invalid canonical observation selection");
        let metadata=std::fs::metadata(&self.project)?;
        ensure!(self.project.canonicalize()?==self.project&&(metadata.dev(),metadata.ino())==self.identity,"canonical observation project changed");
        ensure!(migration::config_reference(&self.config.join("config.toml"))?.digest==self.config_digest,"canonical observation config changed");control.check()
    }
}

struct Probes<'a> {control:&'a Control}
impl Runner for Probes<'_> {
    fn run(&self,command:&Cmd)->Result<Output> {
        self.control.check()?;let mut command=command.clone();
        command.deadline=Some(command.deadline.map_or(self.control.deadline,|d|d.min(self.control.deadline)));
        command.cancellation=Some(self.control.cancellation.clone());command.capture_limit=LIMIT;
        let output=crate::runner::RealRunner.run(&command)?;self.control.check()?;
        ensure!(!output.stdout_truncated&&!output.stderr_truncated&&!output.timed_out&&!output.cancelled&&output.code.is_some(),"canonical observation probe incomplete");Ok(output)
    }
    fn socket_request(&self,_:&Path,_:&str,_:Duration)->Result<String>{anyhow::bail!("canonical observations require bounded CLI probes")}
}

#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Sample {reachable:bool,head:u64}
fn collect(input:&Input,control:&Control)->Result<Sample> {
    input.current(control)?;
    let guard=ProjectGuard::acquire(&input.project)?;
    input.current(control)?;
    ensure!(runtime::snapshot(&input.project)?.schema_version>=9,"upgrade-store is required before canonical controller polling");
    let env=Env::for_observation(&input.home,&input.bin);
    let probes=Probes{control};
    let ctx=Ctx{env:&env,root:input.project.parent().context("canonical project has no root")?.to_path_buf(),config_dir:input.config.clone(),runner:&probes,detached_ticker:false};
    let batch=crate::reconcile_live::collect(&ctx,&input.project)?;
    input.current(control)?;guard.check_project(&input.project)?;
    let reachable=batch.observations.iter().any(|o|o.pane==ResourceState::Present||o.worktree==ResourceState::Present);
    control.check()?;
    runtime::record_controller_observations_guarded(&input.project,&batch,&guard)?;
    Ok(Sample{reachable,head:observation_head(&input.project)?})
}

pub struct ProbeRunner {pub inner:Arc<dyn Runner+Send+Sync>}
impl Runner for ProbeRunner {
    fn run(&self,command:&Cmd)->Result<Output> {
        if command.program!=JOB{return self.inner.run(command);}
        let entered=Instant::now();ensure!(!command.timeout.is_zero()&&command.timeout<=BUDGET,"invalid canonical observation budget");
        let text=command.stdin.as_deref().context("canonical observation input missing")?;ensure!(text.len()<=64*1024,"canonical observation input exceeds bounds");
        let control=Control{deadline:command.deadline.context("canonical observation deadline missing")?.min(entered+command.timeout),cancellation:command.cancellation.clone().context("canonical observation cancellation missing")?};
        let reachable=collect(&serde_json::from_str(text)?,&control)?;
        Ok(Output{code:Some(0),stdout:serde_json::to_string(&reachable)?,elapsed:entered.elapsed(),..Output::default()})
    }
    fn socket_request(&self,path:&Path,line:&str,timeout:Duration)->Result<String>{self.inner.socket_request(path,line,timeout)}
}

use std::collections::BTreeMap;
use crate::{executor::{Executor,Request,Identity,Lane,Ticket},fair_admission::{Cursor,Key}};
const OFFER_LIMIT:usize=128;
const PENDING_LIMIT:usize=16;
const SAMPLE_AGE:Duration=Duration::from_secs(60);
pub enum Poll {Pending,Ready(bool),Failed(String)}
struct Candidate {fingerprint:String,request:Request}
struct Pending {fingerprint:String,identity:Identity,ticket:Ticket,deadline:Instant}
pub struct Reads {
    executor:Arc<Executor>,pending:BTreeMap<Key,Pending>,offers:BTreeMap<Key,Candidate>,
    ready:BTreeMap<Key,(String,Instant,Result<Sample,String>)>,
    classified:BTreeMap<Key,(String,u64,bool,Instant,bool)>,cursor:Cursor,unknown:bool,
}
impl Reads {
    pub fn new(executor:Arc<Executor>)->Self {Self{executor,pending:BTreeMap::new(),offers:BTreeMap::new(),ready:BTreeMap::new(),classified:BTreeMap::new(),cursor:Cursor::default(),unknown:false}}
    pub fn unknown(&self)->bool {self.unknown}
    pub fn begin_pass(&mut self) {
        self.ready.clear();self.offers.clear();self.unknown=false;for (_,_,_,_,touched) in self.classified.values_mut(){*touched=false;}
        let completed=self.pending.iter().filter_map(|(key,p)|match p.ticket.try_recv(){Ok(None)=>None,result=>Some((key.clone(),result))}).collect::<Vec<_>>();let mut serviced=Vec::new();
        for (key,result) in completed {
            let pending=self.pending.remove(&key).unwrap();
            if let Ok(Some(c))=&result&&c.runner_entered{serviced.push((c.started_at,key.clone()));}
            let result=(||->Result<Sample>{let completion=result?.context("canonical observation completion missing")?;ensure!(completion.identity==pending.identity&&completion.runner_entered,"canonical observation did not enter its assigned runner");let output=completion.result?;ensure!(output.success()&&output.stdout.len()<=256,"invalid canonical observation completion");Ok(serde_json::from_str(&output.stdout)?)})();
            if Instant::now()<pending.deadline{self.ready.insert(key,(pending.fingerprint,pending.deadline,result.map_err(|e|format!("{e:#}"))));}
        }
        serviced.sort_by_key(|(started,_)|*started);for(_,key)in serviced{self.cursor.accepted(&key);}
    }
    pub fn poll(&mut self,ctx:&Ctx,path:&Path)->Result<Poll> {
        let input=match Input::new(ctx,path){Ok(input)=>input,Err(error)=>{self.unknown=true;return Err(error);}};
        let head=match observation_head(path){Ok(head)=>head,Err(error)=>{self.unknown=true;return Err(error);}};
        let text=serde_json::to_string(&input)?;ensure!(text.len()<=64*1024,"canonical observation input exceeds bounds");
        let fingerprint=crate::thread::sha256_hex(text.as_bytes());let key=(input.project.display().to_string(),"canonical-observation".into());
        if let Some((_,_,_,_,touched))=self.classified.get_mut(&key){*touched=true;}
        let mut result=Poll::Pending;
        if let Some((expected,deadline,completed))=self.ready.remove(&key)&&expected==fingerprint&&Instant::now()<deadline {
            match completed {
                Ok(sample) if sample.head==head=>{
                    let reachable=sample.reachable;
                    if self.classified.contains_key(&key)||self.classified.len()<OFFER_LIMIT{self.classified.insert(key.clone(),(fingerprint.clone(),sample.head,reachable,deadline,true));}else{self.unknown=true;}
                    result=Poll::Ready(reachable);
                },
                Ok(_)=>{self.classified.remove(&key);self.unknown=true;},
                Err(error)=>{self.classified.remove(&key);self.unknown=true;result=Poll::Failed(error);},
            }
        }
        if self.classified.get(&key).is_none_or(|(expected,revision,reachable,expires,_)|expected!=&fingerprint||*revision!=head||*reachable||Instant::now()>=*expires){self.unknown=true;}
        if let Some(pending)=self.pending.get(&key){if pending.fingerprint!=fingerprint{pending.ticket.cancel();}return Ok(result);}
        let deadline=Instant::now()+BUDGET;let mut command=Cmd::new(JOB,BUDGET).stdin(text);command.deadline=Some(deadline);command.capture_limit=LIMIT;
        let identity=Identity{operation:"canonical-observation".into(),revision:1,project:key.0.clone(),machine:format!("canonical-project:{}",key.0),terminal:None};
        if !self.offers.contains_key(&key)&&self.offers.len()>=OFFER_LIMIT {
            let last=self.offers.keys().max_by(|a,b|self.cursor.compare(a,b)).unwrap().clone();
            if self.cursor.compare(&key,&last)!=std::cmp::Ordering::Less{self.unknown=true;return Ok(result);}self.offers.remove(&last);
        }
        self.offers.insert(key,Candidate{fingerprint,request:Request{identity,lane:Lane::Control,deadline,command}});Ok(result)
    }
    pub fn admit(&mut self)->Vec<String> {
        self.classified.retain(|_,(_,_,_,deadline,touched)|*touched&&Instant::now()<*deadline);let mut errors=Vec::new();let mut cursor=self.cursor.clone();
        // Drain the whole observation batch before replenishing it. Every job
        // holds the shared root barrier; overlapping generations could otherwise
        // starve the existing root-exclusive effect adapters indefinitely.
        if !self.pending.is_empty(){return errors;}
        while self.pending.len()<PENDING_LIMIT {
            let Some(key)=self.offers.keys().min_by(|a,b|cursor.compare(a,b)).cloned()else{break;};let candidate=self.offers.remove(&key).unwrap();let identity=candidate.request.identity.clone();let deadline=candidate.request.deadline+(SAMPLE_AGE-BUDGET);
            match self.executor.submit(candidate.request) {
                Ok(ticket)=>{cursor.accepted(&key);self.pending.insert(key,Pending{fingerprint:candidate.fingerprint,identity,ticket,deadline});},
                Err(error)=>{self.unknown=true;errors.push(format!("{}: canonical observation admission: {error:#}",key.0));},
            }
        }errors
    }
}
impl Drop for Reads {fn drop(&mut self){for pending in self.pending.values(){pending.ticket.cancel();}}}

#[cfg(test)]
#[path="canonical_observations_tests.rs"]
mod tests;

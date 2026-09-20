//! Canonical planning and read-only probes share owned background service.
//! Queue replies carry liveness/rotation only, never mutation authority.
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
struct Input {project:PathBuf,identity:(u64,u64),home:PathBuf,bin:String,config:PathBuf,config_digest:Option<String>,#[serde(default)]last_selected:Option<String>}
impl Input {
    fn new(ctx:&Ctx,path:&Path)->Result<Self> {
        let project=path.canonicalize()?;let metadata=std::fs::metadata(&project)?;
        let config=std::path::absolute(&ctx.config_dir)?;let reference=migration::config_reference(&config.join("config.toml"))?;
        Ok(Self{project,identity:(metadata.dev(),metadata.ino()),home:std::path::absolute(&ctx.env.home)?,bin:ctx.env.herdr_bin(),config,config_digest:reference.digest,last_selected:None})
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

#[derive(Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sample {
    pub reachable:Option<bool>,pub scheduled_work:Option<bool>,
    head:Option<u64>,selected_name:Option<String>,pub diagnostic:Option<String>,
}
fn diagnostic(errors:Vec<String>)->Option<String> {
    if errors.is_empty(){return None;}
    // Diagnostics are bookkeeping, not an unbounded serialized error chain.
    Some(errors.join("; ").chars().take(1000).collect())
}
fn collect(input:&Input,control:&Control)->Result<Sample> {collect_with(input,control,observe)}
fn collect_with(input:&Input,control:&Control,observe:impl FnOnce(&Input,&Control,&ProjectGuard)->Result<bool>)->Result<Sample> {
    input.current(control)?;
    let guard=ProjectGuard::acquire(&input.project)?;
    input.current(control)?;
    ensure!(runtime::snapshot(&input.project)?.schema_version>=9,"upgrade-store is required before canonical controller polling");
    control.check()?;
    let mut errors=Vec::new();
    // Planning precedes probes so an offline endpoint cannot monopolize service.
    // Cooperative checks preserve the remaining budget for observations; full
    // SQLite materialization still needs its own interruption bounds.
    let planned=herdr_projects::routines::schedule_next_guarded(&input.project,input.last_selected.as_deref(),&guard,&control.cancellation,control.deadline.min(Instant::now()+Duration::from_secs(5)));
    let (mut scheduled_work,selected_name)=match planned {
        Ok(report)=>{if let Some(error)=report.diagnostic{errors.push(format!("routine planning: {error}"));}(Some(report.active),report.selected_name)},
        Err(error)=>{errors.push(format!("routine planning: {error:#}"));(None,None)},
    };
    let observed=observe(input,control,&guard);
    let reachable=match observed {Ok(value)=>Some(value),Err(error)=>{errors.push(format!("canonical observation: {error:#}"));None}};
    let head=(||->Result<(u64,bool)>{
        input.current(control)?;guard.check_project(&input.project)?;
        let db=migration::open_active(&input.project)?;control.check()?;
        let state=db.project_control()?.context("canonical control missing")?;
        let active=state.state==herdr_projects::domain::ProjectState::Active&&!state.reconciliation_required;
        let head=observation_head(&input.project)?;input.current(control)?;Ok((head,active))
    })();
    let head=match head {Ok((head,active))=>{if scheduled_work==Some(true)&&!active{scheduled_work=Some(false);}Some(head)},Err(error)=>{errors.push(format!("maintenance result: {error:#}"));None}};
    // Even an expired probe cannot erase the planner's completed selection.
    // Only a final matching head permits either outcome to classify liveness.
    Ok(Sample{reachable,scheduled_work,head,selected_name,diagnostic:diagnostic(errors)})
}

fn observe(input:&Input,control:&Control,guard:&ProjectGuard)->Result<bool> {
    input.current(control)?;
    let env=Env::for_observation(&input.home,&input.bin);let probes=Probes{control};
    let ctx=Ctx{env:&env,root:input.project.parent().context("canonical project has no root")?.to_path_buf(),config_dir:input.config.clone(),runner:&probes,detached_ticker:false};
    let batch=crate::reconcile_live::collect(&ctx,&input.project)?;
    input.current(control)?;guard.check_project(&input.project)?;
    let reachable=batch.observations.iter().any(|o|o.pane==ResourceState::Present||o.worktree==ResourceState::Present);
    control.check()?;
    runtime::record_controller_observations_guarded(&input.project,&batch,guard)?;Ok(reachable)
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
pub enum Poll {Pending,Ready(Sample),Failed(String)}
struct Classification {fingerprint:String,head:u64,known_negative:bool,expires:Instant,touched:bool}
struct Rotation {identity:(u64,u64),last:Option<String>}
const ROTATION_LIMIT:usize=1024;
struct Candidate {fingerprint:String,request:Request}
struct Pending {fingerprint:String,identity:Identity,ticket:Ticket,deadline:Instant}
pub struct Reads {
    executor:Arc<Executor>,pending:BTreeMap<Key,Pending>,offers:BTreeMap<Key,Candidate>,
    ready:BTreeMap<Key,(String,Instant,Result<Sample,String>)>,
    classified:BTreeMap<Key,Classification>,rotation:BTreeMap<Key,Rotation>,cursor:Cursor,unknown:bool,
}
impl Reads {
    pub fn new(executor:Arc<Executor>)->Self {Self{executor,pending:BTreeMap::new(),offers:BTreeMap::new(),ready:BTreeMap::new(),classified:BTreeMap::new(),rotation:BTreeMap::new(),cursor:Cursor::default(),unknown:false}}
    pub fn unknown(&self)->bool {self.unknown}
    pub fn pending_project(&self,project:&str)->bool {self.pending.keys().any(|key|key.0==project)}
    pub fn begin_pass(&mut self) {
        self.ready.clear();self.offers.clear();self.unknown=false;for entry in self.classified.values_mut(){entry.touched=false;}
        let completed=self.pending.iter().filter_map(|(key,p)|match p.ticket.try_recv(){Ok(None)=>None,result=>Some((key.clone(),result))}).collect::<Vec<_>>();let mut serviced=Vec::new();
        for (key,result) in completed {
            let pending=self.pending.remove(&key).unwrap();
            if let Ok(Some(c))=&result&&c.runner_entered{serviced.push((c.started_at,key.clone()));}
            let result=(||->Result<Sample>{let completion=result?.context("canonical observation completion missing")?;ensure!(completion.identity==pending.identity&&completion.runner_entered,"canonical observation did not enter its assigned runner");let output=completion.result?;ensure!(output.success()&&output.stdout.len()<=8192,"invalid canonical observation completion");let sample:Sample=serde_json::from_str(&output.stdout)?;ensure!(sample.selected_name.as_ref().is_none_or(|name|!name.is_empty()&&name.len()<=64&&name.bytes().all(|b|b.is_ascii_alphanumeric()||b"-_".contains(&b))),"invalid planning rotation name");Ok(sample)})();
            if Instant::now()<pending.deadline{self.ready.insert(key,(pending.fingerprint,pending.deadline,result.map_err(|e|format!("{e:#}"))));}
        }
        serviced.sort_by_key(|(started,_)|*started);for(_,key)in serviced{self.cursor.accepted(&key);}
    }
    pub fn poll(&mut self,ctx:&Ctx,path:&Path)->Result<Poll> {
        let mut input=match Input::new(ctx,path){Ok(input)=>input,Err(error)=>{self.unknown=true;return Err(error);}};
        let head=match observation_head(path){Ok(head)=>head,Err(error)=>{self.unknown=true;return Err(error);}};
        let text=serde_json::to_string(&input)?;ensure!(text.len()<=64*1024,"canonical observation input exceeds bounds");
        let fingerprint=crate::thread::sha256_hex(text.as_bytes());let key=(input.project.display().to_string(),"canonical-observation".into());
        if !self.rotation.contains_key(&key)&&self.rotation.len()>=ROTATION_LIMIT {self.unknown=true;anyhow::bail!("canonical planning registry exceeds 1024 project paths; restart ticker after reducing inventory");}
        let rotation=self.rotation.entry(key.clone()).or_insert(Rotation{identity:input.identity,last:None});
        if rotation.identity!=input.identity{*rotation=Rotation{identity:input.identity,last:None};}
        if let Some(entry)=self.classified.get_mut(&key){entry.touched=true;}
        let mut result=Poll::Pending;
        if let Some((expected,deadline,completed))=self.ready.remove(&key)&&expected==fingerprint&&Instant::now()<deadline {
            match completed {
                Ok(sample)=>{
                    if let Some(name)=&sample.selected_name {self.rotation.get_mut(&key).unwrap().last=Some(name.clone());}
                    if sample.head==Some(head) {
                        let known_negative=sample.reachable==Some(false)&&sample.scheduled_work==Some(false);
                        if self.classified.contains_key(&key)||self.classified.len()<OFFER_LIMIT{self.classified.insert(key.clone(),Classification{fingerprint:fingerprint.clone(),head,known_negative,expires:deadline,touched:true});}else{self.unknown=true;}
                        result=Poll::Ready(sample);
                    }else{self.classified.remove(&key);self.unknown=true;if let Some(error)=sample.diagnostic{result=Poll::Failed(error);}}
                },
                Err(error)=>{self.classified.remove(&key);self.unknown=true;result=Poll::Failed(error);},
            }
        }
        if self.classified.get(&key).is_none_or(|c|c.fingerprint!=fingerprint||c.head!=head||!c.known_negative||Instant::now()>=c.expires){self.unknown=true;}
        if let Some(pending)=self.pending.get(&key){if pending.fingerprint!=fingerprint{pending.ticket.cancel();}return Ok(result);}
        input.last_selected=self.rotation.get(&key).and_then(|r|r.last.clone());
        let text=serde_json::to_string(&input)?;ensure!(text.len()<=64*1024,"canonical maintenance input exceeds bounds");
        let deadline=Instant::now()+BUDGET;let mut command=Cmd::new(JOB,BUDGET).stdin(text);command.deadline=Some(deadline);command.capture_limit=LIMIT;
        let identity=Identity{operation:"canonical-observation".into(),revision:1,project:key.0.clone(),machine:format!("canonical-project:{}",key.0),terminal:None};
        if !self.offers.contains_key(&key)&&self.offers.len()>=OFFER_LIMIT {
            let last=self.offers.keys().max_by(|a,b|self.cursor.compare(a,b)).unwrap().clone();
            if self.cursor.compare(&key,&last)!=std::cmp::Ordering::Less{self.unknown=true;return Ok(result);}self.offers.remove(&last);
        }
        self.offers.insert(key,Candidate{fingerprint,request:Request{identity,lane:Lane::Control,deadline,command}});Ok(result)
    }
    #[cfg(test)]
    pub fn admit(&mut self)->Vec<String> {
        self.admit_where(|_|true)
    }
    pub fn admit_where(&mut self,allowed:impl Fn(&str)->bool)->Vec<String> {
        self.classified.retain(|_,entry|entry.touched&&Instant::now()<entry.expires);let mut errors=Vec::new();let mut cursor=self.cursor.clone();
        // Drain the whole observation batch before replenishing it. Every job
        // holds the shared root barrier; overlapping generations could otherwise
        // starve the existing root-exclusive effect adapters indefinitely.
        if !self.pending.is_empty(){return errors;}
        while self.pending.len()<PENDING_LIMIT {
            let Some(key)=self.offers.keys().filter(|key|allowed(&key.0)).min_by(|a,b|cursor.compare(a,b)).cloned()else{break;};let candidate=self.offers.remove(&key).unwrap();let identity=candidate.request.identity.clone();let deadline=candidate.request.deadline+(SAMPLE_AGE-BUDGET);
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

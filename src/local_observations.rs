//! Bounded read-only local session observations. Samples never authorize effects.
use std::{collections::{BTreeMap,BTreeSet},path::{Path,PathBuf},os::unix::fs::{MetadataExt,FileTypeExt},sync::Arc,time::{Duration,Instant}};
use anyhow::{Result,Context,ensure};
use serde::{Serialize,Deserialize};
use crate::{paths::{self,Ctx},project::{self,Project},thread,herdr::{Herdr,Agent,Pane},runner::{Cmd,Output,Runner},source_tree::Control};
const JOB:&str="\0herdr-projects-local-observation";
const BUDGET:Duration=Duration::from_secs(30);
const LIMIT:usize=1024*1024;
// Collection remains <=30 s. A separate admission-based age allows the next
// 15 s ticker pass to apply a late successful collection, without renewing it.
const SAMPLE_AGE:Duration=Duration::from_secs(60);
#[derive(Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {project:PathBuf,identity:(u64,u64),socket:PathBuf,socket_identity:Option<(u64,u64)>,bin:String,config:PathBuf,config_digest:Option<String>,bindings:String}
#[derive(Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Observation {agents:Vec<Agent>,panes:Vec<Pane>}
fn digest(p:&Path)->Result<Option<String>>{Ok(paths::read_control_text(p,LIMIT)?.map(|s|thread::sha256_hex(s.as_bytes())))}
fn socket_identity(p:&Path)->Result<Option<(u64,u64)>>{let m=match std::fs::symlink_metadata(p){Ok(m)=>m,Err(e) if e.kind()==std::io::ErrorKind::NotFound=>return Ok(None),Err(e)=>return Err(e.into())};ensure!(m.file_type().is_socket(),"observation endpoint is not a socket");Ok(Some((m.dev(),m.ino())))}
fn bindings(p:&Project,control:&Control)->Result<String> {
    let c=p.try_coordinator()?.context("observation coordinator missing")?;
    let mut records=BTreeMap::new();let mut budget=50*1024*1024usize;
    let dir=p.dir().join("threads");ensure!(std::fs::symlink_metadata(&dir)?.is_dir(),"observation thread directory is aliased");
    for (n,entry) in std::fs::read_dir(&dir)?.enumerate(){control.check()?;ensure!(n<256,"observation thread inventory exceeds bounds");let path=entry?.path();if path.extension().is_none_or(|s|s!="toml"){continue;}
        let text=paths::read_control_text(&path,budget.min(16*1024*1024))?.context("observation thread disappeared")?;budget=budget.checked_sub(text.len()).context("observation bindings exceed byte budget")?;
        let t:thread::Thread=toml::from_str(&text).map_err(|_|anyhow::anyhow!("invalid observation thread record"))?;thread::validate_id(&t.id)?;ensure!(path.file_stem().and_then(|s|s.to_str())==Some(&t.id),"observation thread filename mismatch");
        if t.machine.is_empty(){records.insert(t.id.clone(),thread::execution_fingerprint(&t));}
    }
    control.check()?;Ok(thread::sha256_hex(&serde_json::to_vec(&(&c.socket,&c.session,&c.workspace_id,&c.tab_id,&c.pane_id,&c.cwd,&c.agent_name,c.prime_request,records))?))
}
impl Input {
    fn new(ctx:&Ctx,p:&Project,control:&Control)->Result<Self> {
        let project=p.dir().canonicalize()?;let m=std::fs::metadata(&project)?;let c=p.try_coordinator()?.context("observation coordinator missing")?;let socket=PathBuf::from(c.socket);ensure!(socket.is_absolute(),"observation socket must be absolute");let config=std::path::absolute(&ctx.config_dir)?;
        Ok(Self{project,identity:(m.dev(),m.ino()),socket_identity:socket_identity(&socket)?,socket,bin:ctx.env.herdr_bin(),config_digest:digest(&config.join("config.toml"))?,config,bindings:bindings(p,control)?})
    }
    fn fingerprint(&self)->Result<String>{Ok(thread::sha256_hex(&serde_json::to_vec(self)?))}
    fn current(&self,control:&Control)->Result<()> {
        control.check()?;ensure!(self.project.is_absolute()&&self.socket.is_absolute()&&self.config.is_absolute()&&!self.bin.is_empty(),"invalid observation route");project::ensure_legacy(&self.project)?;
        let m=std::fs::metadata(&self.project)?;ensure!((m.dev(),m.ino())==self.identity,"observation project changed");
        let p=Project::load(self.project.parent().context("observation root missing")?,self.project.file_name().and_then(|s|s.to_str()).context("invalid observation project")?)?;
        ensure!(p.try_status()?==project::Status::Active,"observation project is inactive");let c=p.try_coordinator()?.context("observation session missing")?;
        ensure!(Path::new(&c.socket)==self.socket&&socket_identity(&self.socket)?==self.socket_identity,"observation session changed");
        ensure!(digest(&self.config.join("config.toml"))?==self.config_digest&&bindings(&p,control)?==self.bindings,"observation configuration or bindings changed");control.check()
    }
}
fn validate(observation:&Observation)->Result<()> {
    ensure!(observation.agents.len()<=4096&&observation.panes.len()<=4096,"observation inventory exceeds bounds");
    for ids in [observation.agents.iter().map(|a|(&a.pane_id,&a.workspace_id,&a.tab_id)).collect::<Vec<_>>(),observation.panes.iter().map(|p|(&p.pane_id,&p.workspace_id,&p.tab_id)).collect()] {
        let mut found=BTreeSet::new();for (pane,workspace,tab) in ids {ensure!([pane,workspace,tab].iter().all(|s|!s.is_empty()&&s.len()<=256&&!s.chars().any(char::is_control))&&found.insert(pane),"observation has malformed or duplicate resource identity");}
    }
    for a in &observation.agents {let p=observation.panes.iter().find(|p|p.pane_id==a.pane_id).context("agent is absent from pane observation")?;ensure!(a.workspace_id==p.workspace_id&&a.tab_id==p.tab_id&&a.cwd==p.cwd,"agent and pane observations disagree");}Ok(())
}
#[derive(Serialize,Deserialize)]
#[serde(tag="outcome",content="sample",rename_all="snake_case",deny_unknown_fields)]
enum ProbeOutcome {Ready(Observation),Unavailable}
fn collect(input:&Input,control:&Control)->Result<ProbeOutcome> {
    input.current(control)?;if input.socket_identity.is_none(){input.current(control)?;return Ok(ProbeOutcome::Unavailable);}let herdr=Herdr::new(&input.bin,&input.socket,&crate::runner::RealRunner);
    let call=|args:[&str;2]|->Result<Option<serde_json::Value>>{control.check()?;let mut cmd=herdr.cmd(crate::herdr::CALL_TIMEOUT).args(args);cmd.capture_limit=LIMIT;
        cmd.deadline=Some(control.deadline);cmd.cancellation=Some(control.cancellation.clone());let out=crate::runner::RealRunner.run(&cmd)?;control.check()?;ensure!(!out.stdout_truncated&&!out.stderr_truncated&&!out.timed_out&&!out.cancelled&&out.code.is_some(),"local observation was incomplete");if !out.success(){return Ok(None);}let value:serde_json::Value=serde_json::from_str(&out.stdout)?;ensure!(value.get("error").is_none(),"local observation rejected");Ok(Some(value.get("result").cloned().context("local observation result missing")?))};
    let Some(first)=call(["agent","list"])? else{input.current(control)?;return Ok(ProbeOutcome::Unavailable);};
    let agents=serde_json::from_value(first["agents"].clone())?;
    let panes=serde_json::from_value(call(["pane","list"])?.context("partial local observation")?["panes"].clone())?;
    let observation=Observation{agents,panes};validate(&observation)?;input.current(control)?;Ok(ProbeOutcome::Ready(observation))
}
pub struct ProbeRunner {pub inner:Arc<dyn Runner+Send+Sync>}
impl Runner for ProbeRunner {
    fn run(&self,cmd:&Cmd)->Result<Output>{if cmd.program!=JOB{return self.inner.run(cmd);}let entered=Instant::now();ensure!(!cmd.timeout.is_zero()&&cmd.timeout<=BUDGET,"invalid observation budget");let text=cmd.stdin.as_deref().context("observation input missing")?;ensure!(text.len()<=64*1024,"observation input exceeds bounds");let control=Control{deadline:cmd.deadline.context("observation deadline missing")?.min(entered+cmd.timeout),cancellation:cmd.cancellation.clone().context("observation cancellation missing")?};let observation=collect(&serde_json::from_str(text)?,&control)?;let stdout=serde_json::to_string(&observation)?;ensure!(stdout.len()<=LIMIT,"combined observation exceeds bounds");Ok(Output{code:Some(0),stdout,elapsed:entered.elapsed(),..Output::default()})}
    fn socket_request(&self,p:&Path,s:&str,t:Duration)->Result<String>{self.inner.socket_request(p,s,t)}
}

use crate::{executor::{Executor,Request,Identity,Lane,Ticket},fair_admission::{Cursor,Key}};
const OFFER_LIMIT:usize=128;
const PENDING_LIMIT:usize=16;
#[derive(Clone)]
pub struct Sample {input:Input,deadline:Instant,pub agents:Vec<Agent>,pub panes:Vec<Pane>}
impl Sample {
    pub fn current(&self,ctx:&Ctx,p:&Project)->Result<()> {
        let control=Control{deadline:self.deadline,cancellation:Default::default()};control.check()?;
        ensure!(self.input.bin==ctx.env.herdr_bin()&&self.input.config==std::path::absolute(&ctx.config_dir)?&&self.input.project==p.dir().canonicalize()?,"observation selection changed");self.input.current(&control)
    }
}
pub enum Poll {Pending,Ready(Sample),Unavailable,Failed(String)}
struct Candidate {input:Input,fingerprint:String,request:Request}
struct Pending {input:Input,fingerprint:String,identity:Identity,ticket:Ticket,deadline:Instant}
#[derive(Clone,Copy,PartialEq)]
enum Classification {Reachable,Failed}
pub struct Reads {
    executor:Arc<Executor>,pending:BTreeMap<Key,Pending>,offers:BTreeMap<Key,Candidate>,
    ready:BTreeMap<Key,(Input,String,Instant,std::result::Result<ProbeOutcome,String>)>,
    classified:BTreeMap<Key,(String,Classification,bool)>,cursor:Cursor,unknown:bool,
}
impl Reads {
    pub fn new(executor:Arc<Executor>)->Self {Self{executor,pending:BTreeMap::new(),offers:BTreeMap::new(),ready:BTreeMap::new(),classified:BTreeMap::new(),cursor:Cursor::default(),unknown:false}}
    /// Unknown eligible identities veto idle exit; they never reset reachability.
    pub fn unknown(&self)->bool {self.unknown}
    pub fn begin_pass(&mut self) {
        self.ready.clear();self.offers.clear();self.unknown=false;for (_,_,touched) in self.classified.values_mut(){*touched=false;}
        let completed=self.pending.iter().filter_map(|(k,p)|match p.ticket.try_recv(){Ok(None)=>None,result=>Some((k.clone(),result))}).collect::<Vec<_>>();let mut serviced=Vec::new();
        for (key,result) in completed {
            let p=self.pending.remove(&key).unwrap();
            if let Ok(Some(c))=&result&&c.runner_entered{serviced.push((c.started_at,key.clone()));}
            let result=(||->Result<ProbeOutcome>{let completion=result?.context("observation completion missing")?;ensure!(completion.identity==p.identity,"observation completion identity mismatch");ensure!(completion.runner_entered,"observation expired before execution");let output=completion.result?;ensure!(output.success()&&output.stdout.len()<=LIMIT,"observation completion failed or exceeded bounds");let value:ProbeOutcome=serde_json::from_str(&output.stdout)?;if let ProbeOutcome::Ready(observation)=&value{validate(observation)?;}Ok(value)})();
            if Instant::now()>=p.deadline{continue;}
            self.ready.insert(key,(p.input,p.fingerprint,p.deadline,result.map_err(|e|format!("{e:#}"))));
        }
        serviced.sort_by(|a,b|a.0.cmp(&b.0));for(_,key)in serviced{self.cursor.accepted(&key);}
    }
    pub fn poll(&mut self,ctx:&Ctx,p:&Project)->Result<Poll> {
        // Failed admission cannot supply evidence that this eligible project is
        // unreachable. Fail closed for the idle-exit decision as well.
        let control=Control{deadline:Instant::now()+Duration::from_secs(10),cancellation:Default::default()};
        let input=match Input::new(ctx,p,&control){Ok(input)=>input,Err(error)=>{self.unknown=true;return Err(error);}};
        let key=(input.project.display().to_string(),"local-observation".into());let fingerprint=input.fingerprint()?;
        if let Some((_,_,touched))=self.classified.get_mut(&key){*touched=true;}
        if let Some((_,expected,_,_))=self.ready.get(&key)&&expected!=&fingerprint {self.ready.remove(&key);}
        if let Some((input,_,deadline,result))=self.ready.remove(&key) {
            if Instant::now()<deadline {
                match result {
                    Ok(ProbeOutcome::Ready(observation))=>{
                        self.classify(&key,&fingerprint,Classification::Reachable);self.unknown=true;
                        self.offer(key.clone(),input.clone(),fingerprint.clone())?;
                        return Ok(Poll::Ready(Sample{input,deadline,agents:observation.agents,panes:observation.panes}));
                    },
                    Ok(ProbeOutcome::Unavailable)=>{self.classify(&key,&fingerprint,Classification::Failed);return Ok(Poll::Unavailable);},
                    Err(error)=>{self.classified.remove(&key);self.unknown=true;return Ok(Poll::Failed(error));},
                }
            }
        }
        if self.classified.get(&key).is_none_or(|(expected,class,_)|expected!=&fingerprint||*class!=Classification::Failed){self.unknown=true;}
        if let Some(pending)=self.pending.get(&key){if pending.fingerprint!=fingerprint{pending.ticket.cancel();}return Ok(Poll::Pending);}
        self.offer(key,input,fingerprint)?;Ok(Poll::Pending)
    }
    fn offer(&mut self,key:Key,input:Input,fingerprint:String)->Result<()> {
        let deadline=Instant::now()+BUDGET;let text=serde_json::to_string(&input)?;ensure!(text.len()<=64*1024,"observation input exceeds bounds");let mut command=Cmd::new(JOB,BUDGET).stdin(text);command.deadline=Some(deadline);command.capture_limit=LIMIT;
        let identity=Identity{operation:"local-observation".into(),revision:1,project:key.0.clone(),machine:format!("local-session:{}",thread::sha256_hex(&serde_json::to_vec(&(&input.socket,input.socket_identity))?)),terminal:None};
        if !self.offers.contains_key(&key)&&self.offers.len()>=OFFER_LIMIT {let last=self.offers.keys().max_by(|a,b|self.cursor.compare(a,b)).unwrap().clone();if self.cursor.compare(&key,&last)!=std::cmp::Ordering::Less{return Ok(());}self.offers.remove(&last);}
        self.offers.insert(key,Candidate{input,fingerprint,request:Request{identity,lane:Lane::Control,deadline,command}});Ok(())
    }
    fn classify(&mut self,key:&Key,fingerprint:&str,class:Classification) {
        if !self.classified.contains_key(key)&&self.classified.len()>=OFFER_LIMIT {self.unknown=true;return;}
        self.classified.insert(key.clone(),(fingerprint.into(),class,true));
    }
    pub fn admit(&mut self)->Vec<String> {
        self.classified.retain(|_,(_,_,touched)|*touched);let mut errors=Vec::new();let mut cursor=self.cursor.clone();
        while self.pending.len()<PENDING_LIMIT {let Some(key)=self.offers.keys().min_by(|a,b|cursor.compare(a,b)).cloned()else{break;};let candidate=self.offers.remove(&key).unwrap();let identity=candidate.request.identity.clone();let deadline=candidate.request.deadline+(SAMPLE_AGE-BUDGET);
            match self.executor.submit(candidate.request){Ok(ticket)=>{cursor.accepted(&key);self.pending.insert(key,Pending{input:candidate.input,fingerprint:candidate.fingerprint,identity,ticket,deadline});},Err(error)=>{self.unknown=true;errors.push(format!("{}: local observation admission: {error:#}",key.0));}}
        }errors
    }
}
impl Drop for Reads {fn drop(&mut self){for p in self.pending.values(){p.ticket.cancel();}}}
#[cfg(all(test,target_os="linux"))]
#[path="local_observations_tests.rs"]
mod tests;

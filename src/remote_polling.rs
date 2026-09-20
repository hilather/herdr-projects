//! Bounded read-only remote observation batches. The internal job marker is never
//! executed as a program; terminal and copy effects stay in the guarded ticker.
use std::{collections::BTreeMap,path::Path,sync::Arc,time::{Duration,Instant}};
use anyhow::{Context,Result,ensure};
use serde::{Deserialize,Serialize};
use crate::{executor::{Executor,Identity,Lane,Request,Ticket},herdr::{Herdr,Agent,Pane},runner::{Cmd,Output,Runner,Cancellation},steps::MachineKey,thread,remote};
const JOB:&str="\0herdr-projects-remote-observation";
#[derive(Debug,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Probe {bin:String,socket:String,machine:String,fallback:Option<String>,directories:Vec<(String,String)>}
#[derive(Debug,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {pub agents:Vec<Agent>,pub panes:Vec<Pane>,pub target:String,pub hashes:BTreeMap<String,String>}
pub struct ProbeRunner {pub inner:Arc<dyn Runner+Send+Sync>}
struct Budget<'a> {runner:&'a dyn Runner,deadline:Instant,cancellation:Cancellation}
impl Runner for Budget<'_> {
    fn run(&self,cmd:&Cmd)->Result<Output> {ensure!(!self.cancellation.is_cancelled(),"remote observation cancelled");let remaining=self.deadline.checked_duration_since(Instant::now()).context("remote observation deadline elapsed")?;ensure!(!remaining.is_zero(),"remote observation deadline elapsed");let mut cmd=cmd.clone();cmd.timeout=cmd.timeout.min(remaining);cmd.cancellation=Some(self.cancellation.clone());self.runner.run(&cmd)}
    fn socket_request(&self,_:&Path,_:&str,_:Duration)->Result<String>{anyhow::bail!("socket effects are not part of remote observations")}
}
impl Runner for ProbeRunner {
    fn run(&self,cmd:&Cmd)->Result<Output> {
        if cmd.program!=JOB {return self.inner.run(cmd);}
        let probe:Probe=serde_json::from_str(cmd.stdin.as_deref().context("remote observation input missing")?)?;
        ensure!(probe.directories.len()<=256,"remote observation exceeds 256 threads");
        let budget=Budget{runner:self.inner.as_ref(),deadline:Instant::now()+cmd.timeout,cancellation:cmd.cancellation.clone().context("remote observation cancellation missing")?};
        let herdr=Herdr::new(&probe.bin,&probe.socket,&budget);let machine=herdr.on_machine(&probe.machine);
        let agents=machine.agent_list()?;let panes=machine.pane_list()?;
        let listed=budget.run(&Cmd::new(&probe.bin,remote::SSH_TIMEOUT).args(["machine","list","--json"])).ok();
        let target=remote::target_from_listing(listed,||probe.fallback,&probe.machine)?;
        let hashes=remote::report_hashes(&budget,&target,&probe.directories)?;
        let stdout=serde_json::to_string(&Observation{agents,panes,target,hashes})?;ensure!(stdout.len()<=crate::runner::CAPTURE_LIMIT,"remote observation exceeds capture limit");
        Ok(Output{code:Some(0),stdout,..Output::default()})
    }
    fn socket_request(&self,socket:&Path,line:&str,timeout:Duration)->Result<String>{self.inner.socket_request(socket,line,timeout)}
}
pub enum Poll {Pending,Ready(Result<Observation>)}
struct Entry {fingerprint:String,queued:Instant,ticket:Ticket,identity:Identity}
pub struct Reads {executor:Arc<Executor>,entries:BTreeMap<MachineKey,Entry>,sequence:u64}
impl Reads {
    pub fn new(executor:Arc<Executor>)->Self {Self{executor,entries:BTreeMap::new(),sequence:0}}
    pub fn pending(&self,key:&MachineKey)->bool {self.entries.contains_key(key)}
    pub fn poll(&mut self,key:&MachineKey,bin:&str,config:&Path,threads:&[thread::Thread])->Result<Poll> {
        ensure!(threads.len()<=256,"remote observation exceeds 256 threads");
        let now=Instant::now();let stale=self.entries.iter().filter(|(_,e)|now.duration_since(e.queued)>=Duration::from_secs(60)).map(|(k,_)|k.clone()).collect::<Vec<_>>();for key in stale {self.remove(&key);}
        let config=crate::paths::read_root_config(config)?;
        let bytes=config.as_deref().unwrap_or("").as_bytes();ensure!(bytes.len()<=1024*1024,"config exceeds remote observation limit");
        let probe=Probe{bin:bin.into(),socket:key.socket.clone(),machine:key.machine.clone(),fallback:remote::configured_target_bytes(bytes,&key.machine),directories:threads.iter().filter(|t|!t.thread_dir.is_empty()).map(|t|(t.id.clone(),t.thread_dir.clone())).collect()};
        let input=serde_json::to_string(&probe)?;let fingerprint=thread::sha256_hex(serde_json::to_string(&(input.as_str(),threads,config.as_ref().map(|s|thread::sha256_hex(s.as_bytes()))))?.as_bytes());
        if self.entries.get(key).is_some_and(|e|e.fingerprint!=fingerprint){self.remove(key);}
        if let Some(entry)=self.entries.get(key) {
            let Some(completion)=entry.ticket.try_recv()? else{return Ok(Poll::Pending);};
            let identity=entry.identity.clone();self.entries.remove(key);
            ensure!(completion.identity==identity,"remote observation identity mismatch");ensure!(completion.runner_entered,"remote observation expired in the local queue; retry without outage");
            return Ok(Poll::Ready(completion.result.and_then(|output|{ensure!(output.success(),"remote observation: {}",output.error_text());Ok(serde_json::from_str(&output.stdout)?)})));
        }
        ensure!(self.entries.len()<128,"remote observation inventory is full");self.sequence=self.sequence.checked_add(1).context("remote observation sequence exhausted")?;
        let identity=Identity{operation:format!("remote-read-{}",self.sequence),revision:1,project:key.project.display().to_string(),machine:format!("remote:{}",key.machine),terminal:None};
        let ticket=self.executor.submit(Request{identity:identity.clone(),lane:Lane::Control,deadline:now+Duration::from_secs(30),command:Cmd::new(JOB,Duration::from_secs(30)).stdin(input)})?;
        self.entries.insert(key.clone(),Entry{fingerprint,queued:now,ticket,identity});Ok(Poll::Pending)
    }
    fn remove(&mut self,key:&MachineKey){if let Some(entry)=self.entries.remove(key){entry.ticket.cancel();}}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::Limits;
    #[test]
    fn config_fifo_and_oversize_refuse_without_queueing_or_waiting() {
        let temp=tempfile::tempdir().unwrap();let path=temp.path().join("config.toml");let name=std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();assert_eq!(unsafe{libc::mkfifo(name.as_ptr(),0o600)},0);
        let executor=Arc::new(Executor::new(Limits::default(),Arc::new(crate::runner::RealRunner)).unwrap());let mut reads=Reads::new(executor.clone());let key=MachineKey{project:temp.path().into(),socket:"socket".into(),machine:"box".into()};let now=Instant::now();assert!(reads.poll(&key,"herdr",&path,&[]).is_err());assert!(now.elapsed()<Duration::from_secs(1));std::fs::remove_file(&path).unwrap();std::fs::write(&path,vec![b' ';1024*1024+1]).unwrap();assert!(reads.poll(&key,"herdr",&path,&[]).is_err());assert_eq!(executor.metrics().high_water,[0,0]);assert!(executor.stop(Duration::from_secs(1)));
    }
    #[test]
    fn a_resolved_machine_never_reads_the_fallback_config() {
        let output=Output{code:Some(0),stdout:r#"[{"label":"box","target":"me@box"}]"#.into(),..Output::default()};assert_eq!(remote::target_from_listing(Some(output),||panic!("fallback must remain lazy"),"box").unwrap(),"me@box");
    }
}

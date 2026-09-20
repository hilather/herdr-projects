//! Asynchronous read-only PR polling. Applying observations stays in the guarded
//! ticker pass; thread/report changes discard pending observations before use.
use std::{collections::BTreeMap,path::{Path,PathBuf},sync::Arc,time::{Duration,Instant}};
use anyhow::{Result,ensure};
use crate::{executor::{Executor,Limits,Request,Identity,Lane,Ticket},runner::Runner,pr};
const RETAIN:Duration=Duration::from_secs(120);
const MAX_ENTRIES:usize=128;
pub enum Poll {Pending,NotDue,Ready(Result<String>)}
struct Entry {fingerprint:String,url:String,touched:Instant,state:ReadState}
enum ReadState {Pending{ticket:Ticket,identity:Identity},Consumed{until:Instant}}
pub struct Reads {executor:Executor,entries:BTreeMap<(PathBuf,String),Entry>,sequence:u64}
impl Reads {
    pub fn new(runner:Arc<dyn Runner+Send+Sync>)->Result<Self> {Ok(Self{executor:Executor::new(Limits::default(),runner)?,entries:BTreeMap::new(),sequence:0})}
    pub fn poll(&mut self,project:&Path,thread:&str,fingerprint:&str,url:&str)->Result<Poll> {
        let now=Instant::now();self.prune(now);
        let key=(project.to_path_buf(),thread.to_string());
        if self.entries.get(&key).is_some_and(|e|e.fingerprint!=fingerprint||e.url!=url) {self.remove(&key);}
        if let Some(entry)=self.entries.get_mut(&key) {
            entry.touched=now;
            match &entry.state {
                ReadState::Consumed{until} if now<*until=>return Ok(Poll::NotDue),
                ReadState::Consumed{..}=>{},
                ReadState::Pending{ticket,identity}=>{
                    let Some(completion)=ticket.try_recv()? else{return Ok(Poll::Pending);};
                    ensure!(completion.identity==*identity,"PR completion identity mismatch");
                    entry.state=ReadState::Consumed{until:if completion.runner_entered {now+RETAIN}else{now}};
                    ensure!(completion.runner_entered,"PR read expired or was cancelled in the local queue; retry without recording a remote outage");
                    return Ok(Poll::Ready(completion.result.and_then(pr::view_output)));
                }
            }
            self.remove(&key);
        }
        ensure!(self.entries.len()<MAX_ENTRIES,"PR observation inventory is full");
        let command=pr::view_command(url)?;
        self.sequence=self.sequence.checked_add(1).ok_or_else(||anyhow::anyhow!("PR read sequence exhausted"))?;
        let identity=Identity{operation:format!("pr-read-{}",self.sequence),revision:1,project:project.display().to_string(),machine:pr_host(url),terminal:None};
        let ticket=self.executor.submit(Request{identity:identity.clone(),lane:Lane::Control,deadline:now+Duration::from_secs(30),command})?;
        self.entries.insert(key,Entry{fingerprint:fingerprint.into(),url:url.into(),touched:now,state:ReadState::Pending{ticket,identity}});Ok(Poll::Pending)
    }
    pub fn stop(&mut self)->Result<()> {ensure!(self.executor.stop(Duration::from_secs(2)),"PR executor cleanup remains uncertain; inspect before restart");Ok(())}
    fn remove(&mut self,key:&(PathBuf,String)) {if let Some(Entry{state:ReadState::Pending{ticket,..},..})=self.entries.remove(key){ticket.cancel();}}
    fn prune(&mut self,now:Instant) {let stale=self.entries.iter().filter(|(_,e)|now.duration_since(e.touched)>=RETAIN).map(|(k,_)|k.clone()).collect::<Vec<_>>();for key in stale {self.remove(&key);}}
}
fn pr_host(url:&str)->String {url.strip_prefix("https://").unwrap_or(url).split('/').next().unwrap_or("github").to_ascii_lowercase()}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{Cmd,Output};
    #[test]
    fn queued_expiry_is_local_retry_not_a_remote_failure() {
        struct Slow(std::sync::mpsc::Sender<()>);
        impl Runner for Slow {
            fn run(&self,_:&Cmd)->Result<Output>{self.0.send(()).unwrap();std::thread::sleep(Duration::from_millis(100));Ok(Output{code:Some(0),stdout:"{}".into(),..Output::default()})}
            fn socket_request(&self,_:&Path,_:&str,_:Duration)->Result<String>{unreachable!()}
        }
        let(tx,rx)=std::sync::mpsc::channel();let mut reads=Reads::new(Arc::new(Slow(tx))).unwrap();let url="https://github.com/owner/repo/pull/1";
        assert!(matches!(reads.poll(Path::new("/project"),"first","fingerprint",url).unwrap(),Poll::Pending));rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let identity=Identity{operation:"expire".into(),revision:1,project:"/project".into(),machine:pr_host(url),terminal:None};let ticket=reads.executor.submit(Request{identity:identity.clone(),lane:Lane::Control,deadline:Instant::now()+Duration::from_millis(5),command:pr::view_command(url).unwrap()}).unwrap();
        reads.entries.insert(("/project".into(),"second".into()),Entry{fingerprint:"fingerprint".into(),url:url.into(),touched:Instant::now(),state:ReadState::Pending{ticket,identity}});
        let deadline=Instant::now()+Duration::from_secs(1);loop {match reads.poll(Path::new("/project"),"second","fingerprint",url){Err(e)=>{assert!(e.to_string().contains("local queue"));break;},Ok(Poll::Pending)=>{},_=>panic!("queued expiry became remote outcome")};assert!(Instant::now()<deadline);std::thread::sleep(Duration::from_millis(5));}
        reads.stop().unwrap();
    }
}

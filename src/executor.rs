//! Root-local command admission. Durable claims and terminal ownership remain with
//! callers; completion carries the immutable identity needed for a fenced commit.
use std::{collections::{BTreeMap, BTreeSet, VecDeque}, sync::{Arc, Condvar, Mutex, mpsc}, thread::JoinHandle, time::{Duration, Instant}};
use anyhow::{Result, ensure};
use crate::runner::{Cancellation, Cmd, Output, Runner};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane { Control, Transfer }
impl Lane { fn index(self)->usize {match self {Self::Control=>0,Self::Transfer=>1}} }
#[derive(Debug, Clone)]
pub struct Limits {
    pub workers:[usize;2],
    /// Includes running commands, preventing admission from exceeding the bound.
    pub outstanding:[usize;2],
    pub per_project:usize,
    pub per_machine:usize,
}
impl Default for Limits {fn default()->Self {Self{workers:[2,2],outstanding:[64,32],per_project:2,per_machine:1}}}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {pub operation:String,pub revision:u64,pub project:String,pub machine:String,pub terminal:Option<String>}
#[derive(Debug, Clone, Default)]
pub struct Metrics {pub queued:[usize;2],pub running:[usize;2],pub high_water:[usize;2],pub completed:[u64;2],pub max_queue_delay:Duration,pub uncertain:bool}
pub struct Request {pub identity:Identity,pub lane:Lane,pub deadline:Instant,pub command:Cmd}
#[derive(Debug)]
pub struct Completion {pub identity:Identity,pub queue_delay:Duration,pub elapsed:Duration,pub runner_entered:bool,pub result:Result<Output>}
pub struct Ticket {receiver:mpsc::Receiver<Completion>,cancellation:Cancellation}
impl Ticket {
    pub fn cancel(&self) {self.cancellation.cancel();}
    pub fn try_recv(&self)->Result<Option<Completion>> {match self.receiver.try_recv() {Ok(value)=>Ok(Some(value)),Err(mpsc::TryRecvError::Empty)=>Ok(None),Err(mpsc::TryRecvError::Disconnected)=>anyhow::bail!("executor completion channel disconnected")}}
    pub fn recv_timeout(&self,timeout:Duration)->Result<Completion> {Ok(self.receiver.recv_timeout(timeout)?)}
}
struct Job {request:Request,queued:Instant,token:Cancellation,reply:mpsc::SyncSender<Completion>}
struct State {
    queue:[VecDeque<Job>;2],running:BTreeMap<(String,String),(Identity,Cancellation,usize)>,
    admitted:[usize;2],metrics:Metrics,last_project:[Option<String>;2],stopping:bool,
}
struct Shared {state:Mutex<State>,wake:Condvar,limits:Limits}
pub struct Executor {shared:Arc<Shared>,threads:Vec<JoinHandle<()>>}
impl Executor {
    pub fn new(limits:Limits,runner:Arc<dyn Runner+Send+Sync>)->Result<Self> {
        ensure!(limits.workers.iter().all(|n| (1..=16).contains(n))&&limits.outstanding.iter().all(|n|(1..=1024).contains(n))&&limits.per_project>0&&limits.per_machine>0,"invalid executor bounds");
        ensure!((0..2).all(|i|limits.workers[i]<=limits.outstanding[i]),"worker count exceeds outstanding limit");
        let shared=Arc::new(Shared{state:Mutex::new(State{queue:[VecDeque::new(),VecDeque::new()],running:BTreeMap::new(),admitted:[0,0],metrics:Metrics::default(),last_project:[None,None],stopping:false}),wake:Condvar::new(),limits});
        let mut pool=Self{shared,threads:vec![]};
        for lane in 0..2 {for index in 0..pool.shared.limits.workers[lane] {
            let shared=pool.shared.clone();let runner=runner.clone();
            // If spawning fails, Drop cancels and drains the already-created workers.
            pool.threads.push(std::thread::Builder::new().name(format!("hp-executor-{lane}-{index}")).spawn(move||worker(shared,runner,lane))?);
        }}Ok(pool)
    }
    pub fn submit(&self,mut request:Request)->Result<Ticket> {
        let cmd=&request.command;
        ensure!(cmd.own_group,"executor commands require an owned process group for cancellation");
        let bytes=cmd.program.len().saturating_add(cmd.args.iter().chain(cmd.env_remove.iter()).map(String::len).fold(0usize,usize::saturating_add)).saturating_add(cmd.env.iter().map(|(k,v)|k.len().saturating_add(v.len())).fold(0usize,usize::saturating_add)).saturating_add(cmd.stdin.as_ref().map_or(0,String::len));
        ensure!(bytes<=1024*1024&&cmd.args.len()<=4096&&cmd.env.len()<=4096&&cmd.env_remove.len()<=4096&&cmd.capture_limit<=crate::runner::CAPTURE_LIMIT,"command exceeds executor input/output bounds");
        let id=&request.identity;
        ensure!(id.revision>0&&[&id.operation,&id.project,&id.machine].iter().all(|s|!s.is_empty()&&s.len()<=4096&&!s.chars().any(char::is_control))&&id.terminal.as_ref().is_none_or(|s|!s.is_empty()&&s.len()<=4096&&!s.chars().any(char::is_control)),"invalid command identity");
        ensure!(request.deadline>Instant::now()&&!request.command.timeout.is_zero(),"command deadline elapsed");
        let lane=request.lane.index();let mut state=self.shared.state.lock().unwrap();
        ensure!(!state.stopping,"executor is stopping");ensure!(state.admitted[lane]<self.shared.limits.outstanding[lane],"executor queue is full");
        ensure!(!state.running.contains_key(&(id.project.clone(),id.operation.clone()))&&!state.queue.iter().flatten().any(|j|j.request.identity.project==id.project&&j.request.identity.operation==id.operation),"operation already admitted");
        let token=request.command.cancellation.clone().unwrap_or_default();request.command.cancellation=Some(token.clone());
        let(reply,receiver)=mpsc::sync_channel(1);state.queue[lane].push_back(Job{request,queued:Instant::now(),token:token.clone(),reply});state.admitted[lane]+=1;state.metrics.high_water[lane]=state.metrics.high_water[lane].max(state.admitted[lane]);
        self.shared.wake.notify_all();Ok(Ticket{receiver,cancellation:token})
    }
    pub fn metrics(&self)->Metrics {
        let state=self.shared.state.lock().unwrap();let mut metrics=state.metrics.clone();
        for lane in 0..2 {metrics.queued[lane]=state.queue[lane].len();metrics.running[lane]=state.running.values().filter(|(_,_,l)|*l==lane).count();}metrics
    }
    /// Stop admission and cancel queued/running work. Returns false if a runner
    /// has not finished cleanup by the deadline; its threads remain owned here.
    pub fn stop(&mut self,timeout:Duration)->bool {
        {let mut state=self.shared.state.lock().unwrap();state.stopping=true;for job in state.queue.iter().flatten(){job.token.cancel();}for (_,token,_) in state.running.values(){token.cancel();}self.shared.wake.notify_all();}
        let deadline=Instant::now()+timeout.min(Duration::from_secs(86400));
        while self.threads.iter().any(|t|!t.is_finished())&&Instant::now()<deadline {std::thread::sleep(Duration::from_millis(5));}
        if self.threads.iter().any(|t|!t.is_finished()){return false;}
        for thread in self.threads.drain(..){let _=thread.join();}!self.shared.state.lock().unwrap().metrics.uncertain
    }
}
impl Drop for Executor {
    fn drop(&mut self) {
        self.stop(Duration::ZERO);
        // Never detach a worker that could still issue commands after ownership
        // guards are dropped. Production Runner must honour its cancellation.
        for thread in self.threads.drain(..){let _=thread.join();}
    }
}
fn select(state:&State,limits:&Limits,lane:usize)->Option<usize> {
    let mut projects=BTreeMap::<&str,usize>::new();let mut machines=BTreeMap::<&str,usize>::new();let mut terminals=BTreeSet::new();
    for (id,_,running_lane) in state.running.values(){if *running_lane==lane {*projects.entry(&id.project).or_default()+=1;*machines.entry(&id.machine).or_default()+=1;}if let Some(terminal)=&id.terminal {terminals.insert(terminal);}}
    let eligible=|j:&Job|{let id=&j.request.identity;j.token.is_cancelled()||Instant::now()>=j.request.deadline||(projects.get(id.project.as_str()).copied().unwrap_or(0)<limits.per_project&&machines.get(id.machine.as_str()).copied().unwrap_or(0)<limits.per_machine&&id.terminal.as_ref().is_none_or(|t|!terminals.contains(t)))};
    // Oldest queued project wins, rotating the last served project behind peers.
    // Among eligible entries, existing queue age wins within each project.
    let first=state.queue[lane].iter().position(eligible)?;
    state.queue[lane].iter().enumerate().find(|(_,j)|eligible(j)&&Some(&j.request.identity.project)!=state.last_project[lane].as_ref()).map(|(i,_)|i).or(Some(first))
}
fn worker(shared:Arc<Shared>,runner:Arc<dyn Runner+Send+Sync>,lane:usize) {
    loop {
        let job={let mut state=shared.state.lock().unwrap();loop {
            if let Some(index)=select(&state,&shared.limits,lane) {let job=state.queue[lane].remove(index).unwrap();state.last_project[lane]=Some(job.request.identity.project.clone());state.running.insert((job.request.identity.project.clone(),job.request.identity.operation.clone()),(job.request.identity.clone(),job.token.clone(),lane));break job;}
            if state.stopping&&state.queue[lane].is_empty(){return;}
            state=shared.wake.wait_timeout(state,Duration::from_millis(20)).unwrap().0;
        }};
        let started=Instant::now();let mut command=job.request.command;
        let mut runner_entered=false;
        let result=if job.token.is_cancelled(){Ok(Output{cancelled:true,..Output::default()})}
        else if let Some(remaining)=job.request.deadline.checked_duration_since(started) {
            command.timeout=command.timeout.min(remaining);
            runner_entered=true;
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(||runner.run(&command))) {
                Ok(result)=>result,
                Err(_)=>{
                    // A panic could leave a child alive. Stop the entire executor;
                    // no successor command may enter an uncertain terminal.
                    let mut state=shared.state.lock().unwrap();state.stopping=true;state.metrics.uncertain=true;
                    for job in state.queue.iter().flatten(){job.token.cancel();}
                    for (_,token,_) in state.running.values(){token.cancel();}
                    shared.wake.notify_all();
                    Err(anyhow::anyhow!("executor runner panicked; external outcome is uncertain; executor quarantined"))
                }
            }
        }else{Ok(Output{timed_out:true,..Output::default()})};
        let identity=job.request.identity;let completion=Completion{identity:identity.clone(),queue_delay:started.duration_since(job.queued),elapsed:started.elapsed(),runner_entered,result};
        {let mut state=shared.state.lock().unwrap();state.running.remove(&(identity.project.clone(),identity.operation.clone()));state.admitted[lane]-=1;state.metrics.completed[lane]=state.metrics.completed[lane].saturating_add(1);state.metrics.max_queue_delay=state.metrics.max_queue_delay.max(completion.queue_delay);shared.wake.notify_all();}
        let _=job.reply.send(completion);
    }
}
#[cfg(test)]
mod tests;

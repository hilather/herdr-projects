//! Read-only report hashes in the shared transfer pool. Observations may prompt
//! a trusted copy request; they never certify copied bytes or a durable receipt.
use std::{collections::BTreeMap,sync::Arc,time::{Duration,Instant}};
use anyhow::{Result,Context,ensure};
use serde::{Serialize,Deserialize};
use crate::{executor::{Executor,Request,Identity,Lane,Ticket},fair_admission::{Cursor,Key},project::Project,runner::Cmd,thread::{self,Thread}};
const PENDING_LIMIT:usize=16; // Leaves transfer admission room for guarded effects.
const OFFER_LIMIT:usize=128;
const QUEUE_BUDGET:Duration=Duration::from_secs(30);
#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {pub hash:Option<String>}
#[derive(Clone)]
pub struct Sample {fingerprint:String,deadline:Instant,pub hash:Option<String>}
impl Sample {pub fn matches(&self,t:&Thread)->bool {Instant::now()<self.deadline&&self.fingerprint==fingerprint(t)}}
pub enum Poll {Pending,Ready(Sample),Failed(String)}
fn fingerprint(t:&Thread)->String {
    thread::sha256_hex(serde_json::json!([thread::execution_fingerprint(t),t.report_hash,t.copy_receipt,t.pending_live_copy,t.pending_final_copy]).to_string().as_bytes())
}
fn key(project:&Project,t:&Thread)->Key {(project.dir().display().to_string(),t.id.clone())}
struct Candidate {fingerprint:String,request:Request}
struct Pending {fingerprint:String,identity:Identity,ticket:Ticket,deadline:Instant}
pub struct Reads {executor:Arc<Executor>,pending:BTreeMap<Key,Pending>,ready:BTreeMap<Key,(String,Instant,std::result::Result<Option<String>,String>)>,offers:BTreeMap<Key,Candidate>,cursor:Cursor}
impl Reads {
    pub fn new(executor:Arc<Executor>)->Self {Self{executor,pending:BTreeMap::new(),ready:BTreeMap::new(),offers:BTreeMap::new(),cursor:Cursor::default()}}
    /// Snapshot completions once before the project pass. Ready values survive
    /// both cheap and slow work, then expire; missed values cannot authorize work.
    pub fn begin_pass(&mut self) {
        self.ready.clear();self.offers.clear();
        let completed=self.pending.iter().filter_map(|(key,p)|match p.ticket.try_recv(){Ok(None)=>None,result=>Some((key.clone(),result))}).collect::<Vec<_>>();
        let mut serviced=Vec::new();
        for (key,result) in completed {
            let pending=self.pending.remove(&key).unwrap();
            if let Ok(Some(completion))=&result&&completion.runner_entered {serviced.push((completion.started_at,key.clone()));}
            if Instant::now()>=pending.deadline {continue;}

            let result=(||->Result<Option<String>> {
                let completion=result?.context("local report completion missing")?;
                ensure!(completion.identity==pending.identity,"local report observation identity mismatch");
                let output=completion.result?;ensure!(output.success(),"local report observation failed: {}",output.error_text());
                ensure!(output.stdout.len()<=1024,"local report observation exceeds bounds");
                let observation:Observation=serde_json::from_str(&output.stdout)?;
                ensure!(observation.hash.as_ref().is_none_or(|hash|hash.len()==64&&hash.bytes().all(|b|b.is_ascii_hexdigit())),"invalid local report hash");Ok(observation.hash)
            })().map_err(|e|format!("{e:#}"));
            self.ready.insert(key,(pending.fingerprint,pending.deadline,result));
        }
        // Admission that expires before entering Runner did not receive service.
        // Advancing past it could repeatedly strand the same tail of a batch.
        serviced.sort_by(|a,b|a.0.cmp(&b.0));for (_,key) in serviced {self.cursor.accepted(&key);}
    }
    pub fn poll(&mut self,project:&Project,t:&Thread)->Result<Poll> {
        ensure!(!t.is_remote(),"local report observation cannot read a remote thread");
        let key=key(project,t);let fingerprint=fingerprint(t);
        if t.thread_dir.is_empty() {return Ok(Poll::Ready(Sample{fingerprint,deadline:Instant::now()+QUEUE_BUDGET,hash:None}));}
        if let Some((expected,deadline,result))=self.ready.get(&key)&&expected==&fingerprint&&Instant::now()<*deadline {
            return Ok(match result {Ok(hash)=>Poll::Ready(Sample{fingerprint,deadline:*deadline,hash:hash.clone()}),Err(error)=>Poll::Failed(error.clone())});
        }
        if let Some(pending)=self.pending.get(&key) {
            if pending.fingerprint!=fingerprint {pending.ticket.cancel();}
            return Ok(Poll::Pending);
        }
        ensure!(t.thread_dir.len()<=4096&&std::path::Path::new(&t.thread_dir).is_absolute(),"local report source must be a bounded absolute path");
        let mut command=Cmd::new(std::env::current_exe()?.to_str().context("report helper path is not UTF-8")?,Duration::from_secs(10)).args(["report-hash","--path",&t.thread_dir]);
        command.env_clear=true;command.capture_limit=1024;
        let deadline=Instant::now()+QUEUE_BUDGET;command.deadline=Some(deadline);
        let identity=Identity{operation:format!("local-report:{}",t.id),revision:1,project:key.0.clone(),machine:"local-report-read".into(),terminal:None};
        if !self.offers.contains_key(&key)&&self.offers.len()>=OFFER_LIMIT {
            let last=self.offers.keys().max_by(|a,b|self.cursor.compare(a,b)).unwrap().clone();
            if self.cursor.compare(&key,&last)!=std::cmp::Ordering::Less {return Ok(Poll::Pending);}
            self.offers.remove(&last);
        }
        self.offers.insert(key,Candidate{fingerprint,request:Request{identity,lane:Lane::Transfer,deadline,command}});Ok(Poll::Pending)
    }
    /// Called after guarded copy/routine admission, so reads cannot refill the
    /// pool ahead of an effect that became eligible during this pass.
    pub fn admit(&mut self)->Vec<String> {
        let mut errors=Vec::new();let mut admission=self.cursor.clone();
        while self.pending.len()<PENDING_LIMIT {
            let Some(key)=self.offers.keys().min_by(|a,b|admission.compare(a,b)).cloned() else {break;};
            let candidate=self.offers.remove(&key).unwrap();let identity=candidate.request.identity.clone();let deadline=candidate.request.deadline;
            match self.executor.submit(candidate.request) {
                Ok(ticket)=>{admission.accepted(&key);self.pending.insert(key,Pending{fingerprint:candidate.fingerprint,identity,ticket,deadline});},
                Err(error)=>errors.push(format!("{} {}: report observation admission: {error:#}",key.0,key.1)),
            }
        }
        errors
    }
}
impl Drop for Reads {fn drop(&mut self){for entry in self.pending.values(){entry.ticket.cancel();}}}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{executor::Limits,runner::{Runner,Output}};
    use std::{path::Path,sync::atomic::{AtomicBool,Ordering}};
    struct Reader {release:Arc<AtomicBool>,entered:Arc<AtomicBool>}
    impl Runner for Reader {
        fn run(&self,cmd:&Cmd)->Result<Output> {
            self.entered.store(true,Ordering::SeqCst);
            while !self.release.load(Ordering::SeqCst)&&!cmd.cancellation.as_ref().unwrap().is_cancelled(){std::thread::sleep(Duration::from_millis(2));}
            Ok(Output{code:Some(0),stdout:serde_json::json!({"hash":thread::sha256_hex(b"new")}).to_string(),..Output::default()})
        }
        fn socket_request(&self,_:&Path,_:&str,_:Duration)->Result<String>{unreachable!()}
    }
    fn fixture()->(crate::scenarios::World,Project,Thread,Reads,Arc<AtomicBool>,Arc<AtomicBool>) {
        let world=crate::scenarios::World::new();let project=world.project("demo","session.sock");let t=world.thread(&project,world.home.path(),|_|{});
        let release=Arc::new(AtomicBool::new(false));let entered=Arc::new(AtomicBool::new(false));
        let pool=Arc::new(Executor::new(Limits::default(),Arc::new(Reader{release:release.clone(),entered:entered.clone()})).unwrap());
        (world,project,t,Reads::new(pool),release,entered)
    }
    fn wait(predicate:impl Fn()->bool) {let deadline=Instant::now()+Duration::from_secs(3);while !predicate(){assert!(Instant::now()<deadline);std::thread::sleep(Duration::from_millis(2));}}
    #[test]
    fn observation_is_offered_then_admitted_once_and_cannot_become_copy_receipt() {
        let (_world,project,t,mut reads,release,entered)=fixture();
        assert!(matches!(reads.poll(&project,&t).unwrap(),Poll::Pending));assert_eq!(reads.executor.metrics().high_water[1],0);
        assert!(reads.admit().is_empty());wait(||entered.load(Ordering::SeqCst));
        assert!(crate::cleanup::lease(&project.root).is_ok(),"read-only reader cannot exclude project effects");
        for _ in 0..10 {assert!(matches!(reads.poll(&project,&t).unwrap(),Poll::Pending));}
        assert_eq!(reads.pending.len(),1);release.store(true,Ordering::SeqCst);wait(||reads.executor.metrics().completed[1]==1);
        assert!(matches!(reads.poll(&project,&t).unwrap(),Poll::Pending),"completion only enters the next pass snapshot");
        reads.begin_pass();let Poll::Ready(sample)=reads.poll(&project,&t).unwrap() else {panic!("hash not ready");};assert!(sample.matches(&t));assert_eq!(sample.hash,Some(thread::sha256_hex(b"new")));
        assert!(thread::load(&project,&t.id).unwrap().copy_receipt.is_none());assert_eq!(reads.executor.metrics().high_water[1],1);
        reads.begin_pass();assert!(matches!(reads.poll(&project,&t).unwrap(),Poll::Pending),"consumed snapshots must expire");assert!(reads.executor.stop(Duration::from_secs(1)));
    }
    #[test]
    fn execution_and_receipt_changes_invalidate_ready_and_running_observations() {
        let (_world,project,t,mut reads,release,entered)=fixture();reads.poll(&project,&t).unwrap();reads.admit();wait(||entered.load(Ordering::SeqCst));
        let mut replacement=t.clone();replacement.lifecycle_generation+=1;
        assert!(matches!(reads.poll(&project,&replacement).unwrap(),Poll::Pending));wait(||reads.executor.metrics().completed[1]==1);release.store(true,Ordering::SeqCst);reads.begin_pass();
        assert!(matches!(reads.poll(&project,&replacement).unwrap(),Poll::Pending));
        let mut copied=t.clone();copied.report_hash=thread::sha256_hex(b"copied");assert!(matches!(reads.poll(&project,&copied).unwrap(),Poll::Pending));
        assert!(reads.executor.stop(Duration::from_secs(1)));
    }
    #[test]
    fn late_completed_hash_and_aged_samples_never_become_fresh_evidence() {
        let (_world,project,t,mut reads,release,entered)=fixture();reads.poll(&project,&t).unwrap();reads.admit();wait(||entered.load(Ordering::SeqCst));
        release.store(true,Ordering::SeqCst);wait(||reads.executor.metrics().completed[1]==1);
        reads.pending.values_mut().next().unwrap().deadline=Instant::now();reads.begin_pass();
        assert!(reads.ready.is_empty());assert!(matches!(reads.poll(&project,&t).unwrap(),Poll::Pending));
        let sample=Sample{fingerprint:fingerprint(&t),deadline:Instant::now(),hash:Some(thread::sha256_hex(b"new"))};assert!(!sample.matches(&t));
        assert!(reads.executor.stop(Duration::from_secs(1)));
    }
    #[test]
    fn expired_queue_tails_eventually_enter_runner_instead_of_losing_their_turns() {
        use std::sync::Mutex;
        struct Slow(Arc<Mutex<std::collections::BTreeSet<String>>>);
        impl Runner for Slow {
            fn run(&self,cmd:&Cmd)->Result<Output> {
                self.0.lock().unwrap().insert(cmd.args.last().unwrap().clone());
                // Consume the short fixture budget. Every later command in this
                // serialized batch expires before Runner; only actual service
                // may advance the persistent fairness cursor.
                while Instant::now()<cmd.deadline.unwrap(){std::thread::sleep(Duration::from_millis(1));}
                Ok(Output{timed_out:true,..Output::default()})
            }
            fn socket_request(&self,_:&Path,_:&str,_:Duration)->Result<String>{unreachable!()}
        }
        let world=crate::scenarios::World::new();let project=world.project("demo","session.sock");let t=world.thread(&project,world.home.path(),|_|{});
        let seen=Arc::new(Mutex::new(std::collections::BTreeSet::new()));let pool=Arc::new(Executor::new(Limits::default(),Arc::new(Slow(seen.clone()))).unwrap());let mut reads=Reads::new(pool.clone());
        for _ in 0..8 {
            for n in 1..=5 {let mut t=t.clone();t.id=format!("t-{n:04}");t.thread_dir=format!("/reports/{n}");reads.poll(&project,&t).unwrap();}
            let deadline=Instant::now()+Duration::from_millis(100);
            for offer in reads.offers.values_mut(){offer.request.deadline=deadline;offer.request.command.deadline=Some(deadline);}
            let completed=pool.metrics().completed[1]+reads.offers.len() as u64;assert!(reads.admit().is_empty());
            wait(||pool.metrics().completed[1]>=completed);reads.begin_pass();assert!(reads.ready.is_empty());
            if seen.lock().unwrap().len()==5 {break;}
        }
        assert_eq!(seen.lock().unwrap().len(),5);assert!(pool.stop(Duration::from_secs(1)));
    }
    #[test]
    fn running_hash_does_not_block_other_project_status_or_allow_stale_review() {
        let (world,project,t,reads,release,entered)=fixture();let other=world.project("other","other.sock");
        let other_t=world.thread(&other,world.home.path(),|_|{});
        *world.panes.borrow_mut()=format!("[{},{}]",world.coordinator_pane(&project),crate::scenarios::pane_json("w2","w2:t1","w2:p1",&t.cwd));
        *world.agents.borrow_mut()=format!("[{},{}]",crate::scenarios::agent_json("w2","w2:t1","w2:p1",&t.cwd,&t.agent_name,"working"),crate::scenarios::agent_json("w2","w2:t1","w2:p1",&other_t.cwd,&other_t.agent_name,"working"));
        let pool=reads.executor.clone();let ctx=world.ctx();let mut memory=crate::steps::Memory::new(&ctx);memory.local_reports=Some(reads);
        assert!(crate::ticker::tick_for_test(&ctx,&mut memory));wait(||entered.load(Ordering::SeqCst));
        assert_eq!(thread::load(&other,&other_t.id).unwrap().last_state,"working");
        assert!(thread::load(&project,&t.id).unwrap().pending_review_notice.is_none());
        assert!(crate::ticker::tick_for_test(&ctx,&mut memory));assert!(pool.metrics().running[1]>0);assert!(crate::cleanup::lease(&world.root).is_ok());
        release.store(true,Ordering::SeqCst);assert!(pool.stop(Duration::from_secs(1)));
    }
}

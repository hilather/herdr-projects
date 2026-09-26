//! Volatile admission and fairness only; worker ingress owns all durable writes.
use super::*;
use std::collections::{BTreeMap,BTreeSet};
use herdr_projects::execution_guard::Resource;
type Key=(String,String);
const LIMIT:usize=128;
/// Local report reads share Transfer and keep 16 slots (`local_reports::PENDING_LIMIT`).
const REPORT_RESERVE:usize=16;
const RETENTION:Duration=Duration::from_secs(180);
fn failure_delay(identity:&Identity)->Duration {
    // Launch retries observe durable boundaries under the original 30s lease.
    // Generic backoff would consume that entire lease after one lost reply.
    if identity.operation.starts_with("canonical-launch:") {Duration::from_secs(1)} else {Duration::from_secs(30)}
}
struct Entry {work:Option<Request>,resources:Vec<Resource>,not_before:Instant,touched:Instant,last:u64,needed:bool}
struct Pending {key:Key,identity:Identity,ticket:crate::executor::Ticket,resources:Vec<Resource>}
pub struct Queue {executor:Arc<crate::executor::Executor>,entries:BTreeMap<Key,Entry>,pending:Option<Pending>,pending_sets:BTreeMap<Key,Pending>,sequence:u64,cursor:crate::fair_admission::Cursor}
#[cfg(feature="state-store")]
pub(crate) fn exclusive_root(identity:&Identity)->bool {
    identity.operation.starts_with("canonical-launch:")||identity.operation.starts_with("canonical-worker:")
}
fn is_exclusive(identity:&Identity)->bool {
    #[cfg(feature="state-store")]
    {exclusive_root(identity)}
    #[cfg(not(feature="state-store"))]
    {let _=identity;false}
}
fn completion_of(ticket:&crate::executor::Ticket,identity:&Identity)->Option<Result<()>> {
    match ticket.try_recv() {
        Ok(None)=>None,
        Ok(Some(completion))=>Some(if completion.identity!=*identity {Err(anyhow::anyhow!("background completion identity mismatch"))} else {completion.result.and_then(|o|{ensure!(o.success(),"background worker failed");Ok(())})}),
        Err(error)=>Some(Err(error)),
    }
}
impl Queue {
    pub fn new(executor:Arc<crate::executor::Executor>)->Self {Self{executor,entries:BTreeMap::new(),pending:None,pending_sets:BTreeMap::new(),sequence:0,cursor:crate::fair_admission::Cursor::default()}}
    pub fn pending(&self)->bool {self.pending.is_some()||!self.pending_sets.is_empty()}
    pub fn single_pending(&self)->bool {self.pending.is_some()}
    pub fn declared_pending(&self)->bool {!self.pending_sets.is_empty()}
    #[cfg(feature="state-store")]
    pub fn pending_exclusive_root(&self)->bool {self.pending.as_ref().is_some_and(|p|exclusive_root(&p.identity))}
    #[cfg(feature="state-store")]
    pub fn canonical_work_pending(&self)->bool {
        self.pending_exclusive_root()||self.entries.values().any(|e|e.work.as_ref().is_some_and(|r|r.deadline>Instant::now()&&exclusive_root(&r.identity)&&(Instant::now()>=e.not_before||r.identity.operation.starts_with("canonical-launch:"))))
    }
    #[cfg(feature="state-store")]
    pub fn offered_exclusive_root(&self)->bool {self.entries.values().any(|e|Instant::now()>=e.not_before&&e.work.as_ref().is_some_and(|r|r.deadline>Instant::now()&&exclusive_root(&r.identity)))}

    #[cfg(feature="state-store")]
    pub fn pending_project(&self,project:&str)->bool {self.pending.as_ref().is_some_and(|p|p.key.0==project)||self.pending_sets.keys().any(|key|key.0==project)}
    fn held(&self,key:&Key)->bool {self.pending.as_ref().is_some_and(|p|&p.key==key)||self.pending_sets.contains_key(key)}
    pub fn outstanding(&self,project:&Project,id:&str)->bool {
        ["live-copy","final-copy"].iter().any(|kind|{
            let key=(project.canonical_dir().display().to_string(),format!("{kind}:{id}"));
            self.held(&key)||self.entries.get(&key).is_some_and(|e|e.needed)
        })
    }
    pub fn clear(&mut self,project:&Project,id:&str) {
        let key=(project.canonical_dir().display().to_string(),format!("live-copy:{id}"));
        if self.held(&key){return;}
        if let Some(entry)=self.entries.get_mut(&key){entry.work=None;entry.needed=false;}
    }
    pub fn offered(&self)->bool {self.entries.values().any(|e|e.work.is_some())}
    fn prune(&mut self) {
        let now=Instant::now();let pending=self.pending.as_ref().map(|p|p.key.clone());
        let declared:BTreeSet<_>=self.pending_sets.keys().cloned().collect();
        self.entries.retain(|key,e| {
            if e.work.as_ref().is_some_and(|r|r.deadline<=now){e.work=None;}
            Some(key)==pending.as_ref()||declared.contains(key)||now<e.touched+RETENTION
        });
    }
    pub fn offer(&mut self,ctx:&Ctx<'_>,project:&Project,t:&Thread,target:Option<&str>)->Result<()> {
        let (work,resources)=super::request_parts(ctx,project,t,target,None)?;self.offer_scoped(work,resources)
    }
    pub fn offer_final(&mut self,ctx:&Ctx,project:&Project,t:&Thread,target:Option<&str>,purpose:Purpose,operation:String)->Result<()> {
        let (work,resources)=super::request_final_parts(ctx,project,t,target,purpose,operation)?;self.offer_scoped(work,resources)
    }
    pub fn offer_tokens(&mut self,ctx:&Ctx,project:&Project,t:Option<&Thread>,route:Option<&crate::remote_api::Route>)->Result<()> {
        self.offer_request(crate::token_jobs::request(ctx,project,t,route)?)
    }
    pub fn offer_notification(&mut self,ctx:&Ctx,project:&Project,c:&crate::project::Coordinator)->Result<()> {
        self.offer_request(crate::coordinator_jobs::request_notification(ctx,project,c)?)
    }
    #[cfg(feature="state-store")]
    pub fn offer_canonical_notification(&mut self,ctx:&Ctx,path:&Path,operation:&herdr_projects::domain::Operation,revision:u64,socket:&str)->Result<()> {
        self.offer_request(crate::canonical_notification_jobs::request(ctx,path,operation,revision,socket)?)
    }
    #[cfg(feature="state-store")]
    pub fn offer_canonical_finalization(&mut self,ctx:&Ctx,path:&Path,operation:&herdr_projects::domain::Operation,revision:u64,mode:crate::canonical_finalization_jobs::Mode)->Result<()> {
        self.offer_request(crate::canonical_finalization_jobs::request(ctx,path,operation,revision,mode)?)
    }
    #[cfg(feature="state-store")]
    pub fn offer_canonical_brief(&mut self,path:&Path,operation:&herdr_projects::domain::Operation,revision:u64)->Result<()> {
        self.offer_request(crate::canonical_brief_jobs::request(path,operation,revision)?)
    }
    #[cfg(feature="state-store")]
    pub fn offer_canonical_launch(&mut self,path:&Path,operation:&herdr_projects::domain::Operation,revision:u64)->Result<()> {
        self.offer_request(crate::canonical_brief_jobs::request_launch(path,operation,revision)?)
    }
    pub fn offer_coordinator_start(&mut self,ctx:&Ctx,project:&Project,c:&crate::project::Coordinator)->Result<()> {
        self.offer_request(crate::coordinator_jobs::request_start(ctx,project,c)?)
    }
    pub fn offer_coordinator_prime(&mut self,ctx:&Ctx,project:&Project,c:&crate::project::Coordinator)->Result<()> {
        self.offer_request(crate::coordinator_jobs::request(ctx,project,c)?)
    }
    pub fn offer_brief(&mut self,ctx:&Ctx,project:&Project,t:&Thread)->Result<()> {
        self.offer_request(crate::brief_jobs::request(ctx,project,t)?)
    }
    pub fn offer_remote_brief(&mut self,ctx:&Ctx,project:&Project,t:&Thread,route:&crate::remote_api::Route)->Result<()> {
        self.offer_request(crate::brief_jobs::request_remote(ctx,project,t,route)?)
    }
    pub fn offer_launch(&mut self,ctx:&Ctx,project:&Project,t:&Thread,route:Option<&crate::remote_api::Route>)->Result<()> {
        self.offer_request(crate::brief_jobs::request_launch(ctx,project,t,route)?)
    }
    pub fn offer_routine(&mut self,ctx:&Ctx,project:&Project,routine:&crate::routine::Routine,previous:&str,occurrence:&str)->Result<()> {
        self.offer_request(crate::legacy_routine_jobs::request(ctx,project,routine,previous,occurrence)?)
    }
    fn offer_request(&mut self,work:Request)->Result<()> {self.offer_scoped(work,Vec::new())}
    fn offer_scoped(&mut self,work:Request,resources:Vec<Resource>)->Result<()> {
        for resource in &resources {Resource::new(&resource.class,&resource.identity)?;}
        self.prune();let key=(work.identity.project.clone(),work.identity.operation.clone());
        if self.held(&key){return Ok(());}
        if !self.entries.contains_key(&key)&&self.entries.len()>=LIMIT {
            // Offers are volatile hints, not recovery authority. Rotating a full
            // inventory must not let refreshed failures exclude every new key.
            let now=Instant::now();
            let priority=|key:&Key| {let entry=&self.entries[key];(entry.work.is_none(),entry.not_before>now)};
            let victim=self.entries.keys().filter(|key|!self.held(key)).max_by(|a,b|priority(a).cmp(&priority(b)).then_with(||self.compare(a,b))).cloned().context("copy inventory has no evictable offer")?;
            if priority(&victim)==(false,false)&&self.compare(&key,&victim)!=std::cmp::Ordering::Less {return Ok(());}
            self.entries.remove(&victim);
        }
        let now=Instant::now();let entry=self.entries.entry(key).or_insert(Entry{work:None,resources:Vec::new(),not_before:now,touched:now,last:0,needed:true});
        entry.work=Some(work);entry.resources=resources;entry.touched=now;entry.needed=true;Ok(())
    }
    pub fn drain(&mut self)->Vec<String> {
        self.prune();
        let mut finished=Vec::new();
        if let Some(pending)=&self.pending {
            if let Some(result)=completion_of(&pending.ticket,&pending.identity) {finished.push((pending.key.clone(),result));}
        }
        for pending in self.pending_sets.values() {
            if let Some(result)=completion_of(&pending.ticket,&pending.identity) {finished.push((pending.key.clone(),result));}
        }
        let mut errors=Vec::new();
        for (key,result) in finished {
            let pending=if self.pending.as_ref().is_some_and(|p|p.key==key) {self.pending.take().unwrap()} else {self.pending_sets.remove(&key).unwrap()};
            errors.extend(self.settle(pending,result));
        }
        errors
    }
    fn settle(&mut self,pending:Pending,result:Result<()>)->Vec<String> {
        let now=Instant::now();
        // Recovery/termination observations may find no new evidence. Poll them
        // at idle cadence instead of competing with every launch stage.
        if let Some(entry)=self.entries.get_mut(&pending.key) {entry.not_before=now+if result.is_err()||pending.identity.operation=="notification"||pending.identity.operation.starts_with("tokens:"){failure_delay(&pending.identity)}else if pending.identity.operation.starts_with("canonical-worker:terminate-")||pending.identity.operation.starts_with("canonical-worker:recover:"){Duration::from_secs(15)}else{Duration::ZERO};entry.touched=now;entry.needed=result.is_err();}
        result.err().map(|e|format!("{} {}: background queue: {e:#}",pending.key.0,pending.key.1)).into_iter().collect()
    }
    #[cfg(any(test,not(feature="state-store")))]
    pub fn admit(&mut self)->Vec<String> {
        self.admit_where(|_|true)
    }
    #[cfg(any(test,not(feature="state-store")))]
    pub fn admit_where(&mut self,allowed:impl Fn(&str)->bool)->Vec<String> {
        self.admit_matching(|identity|allowed(&identity.project))
    }
    pub fn admit_matching(&mut self,allowed:impl Fn(&Identity)->bool)->Vec<String> {
        self.prune();let mut errors=Vec::new();if self.single_pending(){return errors;}
        if self.declared_pending() {self.admit_declared(&allowed,&mut errors);return errors;}
        let Some(key)=self.next_where(&allowed) else {return errors;};
        let declared=self.entries.get(&key).is_some_and(|entry|!entry.resources.is_empty()&&entry.work.as_ref().is_some_and(|work|!is_exclusive(&work.identity)));
        if declared {self.admit_declared(&allowed,&mut errors);} else {self.admit_one(&key,&mut errors,false);}
        errors
    }
    fn declared_limit(&self)->usize {self.executor.outstanding(crate::executor::Lane::Transfer).saturating_sub(REPORT_RESERVE)}
    fn admit_declared(&mut self,allowed:&impl Fn(&Identity)->bool,errors:&mut Vec<String>) {
        while self.pending_sets.len()<self.declared_limit() {
            let Some(key)=self.next_declared(allowed) else {break;};
            if !self.admit_one(&key,errors,true) {break;}
        }
    }
    /// Returns false when admission must stop. A full lane keeps the offer.
    fn admit_one(&mut self,key:&Key,errors:&mut Vec<String>,declared:bool)->bool {
        let next=match self.sequence.checked_add(1){Some(n)=>n,None=>{errors.push("copy admission sequence exhausted".into());return false;}};
        let (work,identity,resources)={
            let Some(entry)=self.entries.get(key) else {return false;};
            let Some(work)=entry.work.clone() else {return false;};
            let identity=work.identity.clone();let resources=entry.resources.clone();
            (work,identity,resources)
        };
        match self.executor.submit(work) {
            Ok(ticket)=>{
                let entry=self.entries.get_mut(key).unwrap();entry.work=None;entry.last=next;self.sequence=next;self.cursor.accepted(key);
                let pending=Pending{key:key.clone(),identity,ticket,resources};
                if declared {self.pending_sets.insert(key.clone(),pending);} else {self.pending=Some(pending);}
                true
            },
            Err(error) if error.to_string().contains("executor queue is full") => false,
            Err(error)=>{
                let entry=self.entries.get_mut(key).unwrap();entry.work=None;entry.not_before=Instant::now()+failure_delay(&identity);
                errors.push(format!("{} {}: background admission: {error:#}",key.0,key.1));true
            },
        }
    }
    fn next_declared(&self,allowed:&impl Fn(&Identity)->bool)->Option<Key> {
        let now=Instant::now();
        let held:Vec<&Resource>=self.pending_sets.values().flat_map(|pending|pending.resources.iter()).collect();
        self.entries.iter().filter(|(_,entry)| {
            let Some(work)=&entry.work else {return false;};
            now>=entry.not_before&&allowed(&work.identity)&&!is_exclusive(&work.identity)&&!entry.resources.is_empty()
                &&entry.resources.iter().all(|resource|!held.iter().any(|held| *held==resource))
        }).min_by(|(a,_),(b,_)|self.compare(a,b)).map(|(key,_)|key.clone())
    }
    fn compare(&self,a:&Key,b:&Key)->std::cmp::Ordering {self.cursor.compare(a,b)}
    fn next_where(&self,allowed:&impl Fn(&Identity)->bool)->Option<Key> {
        let now=Instant::now();
        self.entries.iter().filter(|(_,e)|e.work.as_ref().is_some_and(|r|allowed(&r.identity))&&now>=e.not_before)
            .min_by(|(a,_),(b,_)|self.compare(a,b)).map(|(key,_)|key.clone())
    }

}
impl Drop for Queue {fn drop(&mut self){if let Some(pending)=&self.pending {pending.ticket.cancel();}for pending in self.pending_sets.values(){pending.ticket.cancel();}}}

#[cfg(test)]
mod tests {
    use super::*;
    struct Immediate;
    impl Runner for Immediate {
        fn run(&self,cmd:&Cmd)->Result<Output>{if cmd.program=="fail" {anyhow::bail!("fixture failure");}Ok(Output{code:Some(0),..Output::default()})}
        fn socket_request(&self,_:&Path,_:&str,_:Duration)->Result<String>{unreachable!()}
    }
    fn work(project:&str,thread:&str)->Request {
        let deadline=Instant::now()+BUDGET;
        Request{identity:Identity{operation:thread.into(),revision:1,project:project.into(),machine:"root".into(),terminal:None},lane:Lane::Transfer,deadline,command:Cmd::new("ok",BUDGET)}
    }
    fn drain(queue:&mut Queue)->Vec<String> {
        let deadline=Instant::now()+Duration::from_secs(3);let mut errors=Vec::new();
        while queue.pending(){errors.extend(queue.drain());assert!(Instant::now()<deadline);std::thread::sleep(Duration::from_millis(2));}errors
    }
    #[test]
    fn token_refreshes_cool_down_without_blocking_other_work() {
        let pool=Arc::new(crate::executor::Executor::new(crate::executor::Limits::default(),Arc::new(Immediate)).unwrap());let mut queue=Queue::new(pool.clone());
        queue.offer_request(work("a","tokens:1")).unwrap();queue.admit();assert!(drain(&mut queue).is_empty());
        let key=("a".into(),"tokens:1".into());assert!(queue.entries[&key].not_before>Instant::now()+Duration::from_secs(29));
        queue.offer_request(work("a","tokens:1")).unwrap();queue.admit();assert!(!queue.pending());
        queue.offer_request(work("b","brief:1")).unwrap();queue.admit();assert_eq!(queue.pending.as_ref().unwrap().key,("b".into(),"brief:1".into()));assert!(drain(&mut queue).is_empty());
        assert!(pool.stop(Duration::from_secs(1)));
    }
    #[test]
    fn projects_and_threads_rotate_only_on_admission_and_completed_tickets_hold_turn() {
        let pool=Arc::new(crate::executor::Executor::new(crate::executor::Limits::default(),Arc::new(Immediate)).unwrap());let mut queue=Queue::new(pool.clone());
        let mut found=Vec::new();
        for _ in 0..5 {
            for p in ["a","b"] {for t in ["1","2"] {queue.offer_request(work(p,t)).unwrap();}}
            assert!(queue.admit().is_empty());let key=queue.pending.as_ref().unwrap().key.clone();found.push(key.clone());
            let last=queue.sequence;
            for _ in 0..20 {queue.offer_request(work(&key.0,&key.1)).unwrap();queue.admit();}
            assert_eq!(queue.sequence,last);assert!(queue.entries[&key].work.is_none(),"refresh cannot replace an admitted request");
            assert!(drain(&mut queue).is_empty());
        }
        assert_eq!(found,vec![("a".into(),"1".into()),("b".into(),"1".into()),("a".into(),"2".into()),("b".into(),"2".into()),("a".into(),"1".into())]);
        assert!(pool.stop(Duration::from_secs(1)));
    }
    #[test]
    fn bounded_inventory_prunes_expired_offers_but_keeps_pending_ticket() {
        let pool=Arc::new(crate::executor::Executor::new(crate::executor::Limits::default(),Arc::new(Immediate)).unwrap());let mut queue=Queue::new(pool.clone());
        for n in 0..LIMIT {queue.offer_request(work("project",&format!("{n:04}"))).unwrap();}
        queue.offer_request(work("overflow","1")).unwrap();assert_eq!(queue.entries.len(),LIMIT);queue.admit();let pending=queue.pending.as_ref().unwrap().key.clone();
        for entry in queue.entries.values_mut(){entry.touched=Instant::now()-RETENTION;}
        queue.prune();assert_eq!(queue.entries.len(),1);assert!(queue.entries.contains_key(&pending));assert!(queue.pending());
        assert!(drain(&mut queue).is_empty());assert!(pool.stop(Duration::from_secs(1)));
    }
    #[test]
    fn continuously_refreshed_saturated_inventory_cannot_exclude_later_projects() {
        let pool=Arc::new(crate::executor::Executor::new(crate::executor::Limits::default(),Arc::new(Immediate)).unwrap());let mut queue=Queue::new(pool.clone());
        let mut seen=std::collections::BTreeSet::new();
        for turn in 0..(LIMIT+1)*2 {
            // Rotate scan order independently of admission. Every key remains
            // continuously offered; none can become eligible through expiry.
            for offset in 0..=LIMIT {let n=if turn<LIMIT+1 {offset}else{(turn+offset)%(LIMIT+1)};queue.offer_request(work(&format!("p{n:04}"),"1")).unwrap();}
            queue.admit();seen.insert(queue.pending.as_ref().unwrap().key.0.clone());assert!(drain(&mut queue).is_empty());
            assert!(queue.entries.len()<=LIMIT);assert!(queue.cursor.history_len()<=LIMIT);
        }
        assert_eq!(seen.len(),LIMIT+1);assert!(pool.stop(Duration::from_secs(1)));
    }
    #[test]
    fn overflowing_project_history_rotates_every_thread_instead_of_forgetting_its_turn() {
        let pool=Arc::new(crate::executor::Executor::new(crate::executor::Limits::default(),Arc::new(Immediate)).unwrap());let mut queue=Queue::new(pool.clone());
        let mut seen=std::collections::BTreeSet::new();
        for _ in 0..(LIMIT+1)*4 {
            for n in 0..=LIMIT {for id in ["1","2"] {queue.offer_request(work(&format!("p{n:04}"),id)).unwrap();}}
            queue.admit();seen.insert(queue.pending.as_ref().unwrap().key.clone());assert!(drain(&mut queue).is_empty());
            assert!(queue.entries.len()<=LIMIT);assert!(queue.cursor.history_len()<=LIMIT);
        }
        assert!(queue.cursor.overflow());assert_eq!(seen.len(),(LIMIT+1)*2);assert!(pool.stop(Duration::from_secs(1)));
    }
    #[test]
    fn errors_back_off_without_resetting_history_and_failed_submission_does_not_advance() {
        let pool=Arc::new(crate::executor::Executor::new(crate::executor::Limits::default(),Arc::new(Immediate)).unwrap());let mut queue=Queue::new(pool.clone());
        let mut failed=work("a","1");failed.command.program="fail".into();queue.offer_request(failed).unwrap();queue.admit();assert_eq!(drain(&mut queue).len(),1);
        queue.offer_request(work("a","1")).unwrap();assert!(queue.admit().is_empty());assert!(!queue.pending());assert!(queue.entries[&("a".into(),"1".into())].needed);
        queue.offer_request(work("b","1")).unwrap();queue.admit();assert_eq!(queue.pending.as_ref().unwrap().key.0,"b");assert!(drain(&mut queue).is_empty());
        assert!(pool.stop(Duration::from_secs(1)));let last=queue.sequence;
        queue.offer_request(work("c","1")).unwrap();assert_eq!(queue.admit().len(),1);assert_eq!(queue.sequence,last);assert_eq!(queue.entries[&("c".into(),"1".into())].last,0);
    }
    #[cfg(feature="state-store")]
    #[test]
    fn termination_poll_cools_down_without_delaying_launch_service() {
        let pool=Arc::new(crate::executor::Executor::new(crate::executor::Limits::default(),Arc::new(Immediate)).unwrap());let mut queue=Queue::new(pool.clone());
        let key:Key=("a".into(),"canonical-worker:terminate-attempt".into());
        queue.offer_request(work("a",&key.1)).unwrap();assert!(queue.canonical_work_pending());
        queue.admit();assert!(queue.pending_exclusive_root());assert!(drain(&mut queue).is_empty());
        assert_eq!(queue.entries[&key].not_before.duration_since(queue.entries[&key].touched),Duration::from_secs(15));
        queue.offer_request(work("a",&key.1)).unwrap();
        assert!(!queue.canonical_work_pending());assert!(!queue.offered_exclusive_root());
        queue.offer_request(work("b","canonical-launch:operation")).unwrap();
        assert!(queue.canonical_work_pending());assert!(queue.offered_exclusive_root());
        queue.admit_matching(|identity|!exclusive_root(identity));assert!(!queue.pending());
        queue.admit();assert_eq!(queue.pending.as_ref().unwrap().key.0,"b");assert!(drain(&mut queue).is_empty());
        queue.entries.get_mut(&key).unwrap().not_before=Instant::now();
        assert!(queue.canonical_work_pending());queue.admit();assert_eq!(queue.pending.as_ref().unwrap().key,key);assert!(drain(&mut queue).is_empty());
        assert!(pool.stop(Duration::from_secs(1)));
    }

    #[cfg(feature="state-store")]
    #[test]
    fn recovery_observations_cool_down_but_original_claim_can_still_advance() {
        let pool=Arc::new(crate::executor::Executor::new(crate::executor::Limits::default(),Arc::new(Immediate)).unwrap());let mut queue=Queue::new(pool.clone());
        let key:Key=("a".into(),"canonical-worker:recover:launch-operation".into());
        queue.offer_request(work("a",&key.1)).unwrap();queue.admit();assert!(queue.pending_exclusive_root());assert!(drain(&mut queue).is_empty());
        assert_eq!(queue.entries[&key].not_before.duration_since(queue.entries[&key].touched),Duration::from_secs(15));
        queue.offer_request(work("a",&key.1)).unwrap();assert!(!queue.canonical_work_pending());assert!(!queue.offered_exclusive_root());
        queue.offer_request(work("a","canonical-launch:launch-operation")).unwrap();assert!(queue.canonical_work_pending());
        queue.admit();assert_eq!(queue.pending.as_ref().unwrap().identity.operation,"canonical-launch:launch-operation");assert!(drain(&mut queue).is_empty());
        queue.entries.get_mut(&key).unwrap().not_before=Instant::now();queue.admit();assert_eq!(queue.pending.as_ref().unwrap().key,key);assert!(drain(&mut queue).is_empty());
        assert!(pool.stop(Duration::from_secs(1)));
    }

    #[test]
    fn canonical_launch_failure_keeps_short_backoff_and_other_projects_rotate() {
        let pool=Arc::new(crate::executor::Executor::new(crate::executor::Limits::default(),Arc::new(Immediate)).unwrap());let mut queue=Queue::new(pool.clone());
        let mut failed=work("a","canonical-launch:operation");failed.command.program="fail".into();
        let identity=failed.identity.clone();queue.offer_request(failed).unwrap();queue.admit();
        assert_eq!(drain(&mut queue).len(),1);
        let key=("a".into(),"canonical-launch:operation".into());
        let entry=&queue.entries[&key];
        assert_eq!(entry.not_before.duration_since(entry.touched),Duration::from_secs(1));
        assert!(entry.needed);
        assert_eq!(failure_delay(&identity),Duration::from_secs(1));
        queue.offer_request(work("a","canonical-launch:operation")).unwrap();queue.admit();assert!(!queue.pending());
        queue.offer_request(work("b","other")).unwrap();queue.admit();assert_eq!(queue.pending.as_ref().unwrap().key.0,"b");assert!(drain(&mut queue).is_empty());
        queue.entries.get_mut(&key).unwrap().not_before=Instant::now();
        queue.admit();assert_eq!(queue.pending.as_ref().unwrap().key,key);assert!(drain(&mut queue).is_empty());
        assert!(pool.stop(Duration::from_secs(1)));
    }

    struct FakeClock {tick:std::sync::Mutex<u64>}
    struct Gate {
        clock:std::sync::Arc<FakeClock>,started:std::sync::Mutex<std::sync::mpsc::Sender<String>>,
        release:std::sync::Mutex<bool>,wake:std::sync::Condvar,spans:std::sync::Mutex<Vec<(String,u64,u64)>>,
        saw_cancel:std::sync::atomic::AtomicBool,
    }
    impl Runner for Gate {
        fn run(&self,cmd:&Cmd)->Result<Output> {
            if cmd.program=="cancel" {self.started.lock().unwrap().send("cancel".into()).unwrap();return Ok(Output{code:Some(0),..Output::default()});}
            let _root=if cmd.program=="launch" {Some(herdr_projects::execution_guard::RootGuard::exclusive(std::path::Path::new(cmd.stdin.as_deref().unwrap()))?)} else {None};
            let start=*self.clock.tick.lock().unwrap();
            self.started.lock().unwrap().send(cmd.program.clone()).unwrap();
            let mut released=self.release.lock().unwrap();
            while !*released {
                if cmd.cancellation.as_ref().is_some_and(|token|token.is_cancelled()) {
                    let control=crate::source_tree::Control{deadline:cmd.deadline.unwrap(),cancellation:cmd.cancellation.clone().unwrap()};
                    self.saw_cancel.store(control.check().is_err(),std::sync::atomic::Ordering::SeqCst);
                    return Ok(Output{cancelled:true,..Output::default()});
                }
                released=self.wake.wait(released).unwrap();
            }
            let end=*self.clock.tick.lock().unwrap();
            self.spans.lock().unwrap().push((cmd.program.clone(),start,end));
            Ok(Output{code:Some(0),..Output::default()})
        }
        fn socket_request(&self,_:&std::path::Path,_:&str,_:Duration)->Result<String>{unreachable!()}
    }
    fn gate()->(std::sync::Arc<Gate>,std::sync::mpsc::Receiver<String>) {
        let (sender,receiver)=std::sync::mpsc::channel();
        let gate=std::sync::Arc::new(Gate{clock:std::sync::Arc::new(FakeClock{tick:std::sync::Mutex::new(0)}),started:std::sync::Mutex::new(sender),release:std::sync::Mutex::new(false),wake:std::sync::Condvar::new(),spans:std::sync::Mutex::new(Vec::new()),saw_cancel:std::sync::atomic::AtomicBool::new(false)});
        (gate,receiver)
    }
    fn footprint(project:&str,git:&str)->Vec<Resource> {
        vec![Resource::new("artifact",project).unwrap(),Resource::new("git",git).unwrap()]
    }
    fn transfer(project:&str,operation:&str,git:&str)->Request {
        let mut request=work(project,operation);request.identity.machine=format!("git:{git}");request.command.program=project.into();request
    }
    fn release(gate:&Gate,tick:u64) {
        *gate.clock.tick.lock().unwrap()=tick;*gate.release.lock().unwrap()=true;gate.wake.notify_all();
    }
    fn await_idle(queue:&mut Queue) {
        let deadline=Instant::now()+Duration::from_secs(2);
        while queue.pending() {assert!(Instant::now()<deadline,"transfer ticket did not finish");let _=queue.drain();std::thread::yield_now();}
    }
    #[test]
    fn declared_admission_leaves_the_report_reserve() {
        let pool=Arc::new(crate::executor::Executor::new(crate::executor::Limits::default(),Arc::new(Immediate)).unwrap());
        let queue=Queue::new(pool.clone());
        assert_eq!(queue.declared_limit(),queue.executor.outstanding(crate::executor::Lane::Transfer)-REPORT_RESERVE);
        assert_eq!(queue.declared_limit(),16);
        assert!(pool.stop(Duration::from_secs(1)));
    }
    #[test]
    fn transfers_on_different_repos_overlap_and_the_same_git_dir_does_not() {
        let (gate,started)=gate();
        let limits=crate::executor::Limits{workers:[1,2],outstanding:[4,18],per_project:2,per_machine:2};
        let pool=Arc::new(crate::executor::Executor::new(limits,gate.clone()).unwrap());let mut queue=Queue::new(pool.clone());
        queue.offer_scoped(transfer("/projects/a","live-copy:1","/repos/a"),footprint("/projects/a","/repos/a")).unwrap();
        queue.offer_scoped(transfer("/projects/b","live-copy:1","/repos/b"),footprint("/projects/b","/repos/b")).unwrap();
        assert!(queue.admit().is_empty());
        let mut names=vec![started.recv_timeout(Duration::from_secs(2)).unwrap(),started.recv_timeout(Duration::from_secs(2)).unwrap()];names.sort();
        assert_eq!(names,vec!["/projects/a".to_string(),"/projects/b".to_string()]);
        assert!(started.try_recv().is_err());assert_eq!(queue.pending_sets.len(),2);assert!(!queue.single_pending());
        release(&gate,1);await_idle(&mut queue);
        let spans=gate.spans.lock().unwrap().clone();
        assert!(spans.iter().all(|(_,start,end)| *start==0&&*end==1),"different repositories must overlap on the fake clock: {spans:?}");
        *gate.release.lock().unwrap()=false;*gate.clock.tick.lock().unwrap()=2;gate.spans.lock().unwrap().clear();
        queue.offer_scoped(transfer("/projects/a","live-copy:2","/repos/a"),footprint("/projects/a","/repos/a")).unwrap();
        queue.offer_scoped(transfer("/projects/c","live-copy:2","/repos/a"),footprint("/projects/c","/repos/a")).unwrap();
        assert!(queue.admit().is_empty());
        let first=started.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(started.try_recv().is_err());assert_eq!(queue.pending_sets.len(),1);
        release(&gate,3);await_idle(&mut queue);
        *gate.release.lock().unwrap()=false;*gate.clock.tick.lock().unwrap()=4;
        assert!(queue.admit().is_empty());
        let second=started.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_ne!(first,second);release(&gate,5);await_idle(&mut queue);
        let mut spans=gate.spans.lock().unwrap().clone();spans.sort_by_key(|span|span.1);
        assert_eq!(spans.len(),2);assert!(spans[0].2<=spans[1].1,"the same git directory overlapped: {spans:?}");
        assert!(pool.stop(Duration::from_secs(1)));
    }
    #[cfg(feature="state-store")]
    #[test]
    fn launch_observes_a_single_root_exclusive_ticket() {
        let root=tempfile::tempdir().unwrap();let (gate,started)=gate();
        let limits=crate::executor::Limits{workers:[2,2],outstanding:[4,18],per_project:2,per_machine:2};
        let pool=Arc::new(crate::executor::Executor::new(limits,gate.clone()).unwrap());let mut queue=Queue::new(pool.clone());
        for (project,operation) in [("/projects/a","canonical-launch:one"),("/projects/b","canonical-launch:two")] {
            let mut request=work(project,operation);request.command.program="launch".into();request.command.stdin=Some(root.path().display().to_string());request.lane=Lane::Control;request.identity.machine=format!("launch:{project}");
            queue.offer_request(request).unwrap();
        }
        queue.offer_scoped(transfer("/projects/c","live-copy:1","/repos/c"),footprint("/projects/c","/repos/c")).unwrap();
        assert!(queue.admit().is_empty());
        assert_eq!(started.recv_timeout(Duration::from_secs(2)).unwrap(),"launch");
        assert!(started.try_recv().is_err());
        assert!(queue.pending_exclusive_root());assert!(queue.single_pending());assert!(!queue.declared_pending());
        assert_eq!(queue.pending_sets.len(),0);assert!(herdr_projects::execution_guard::RootGuard::exclusive(root.path()).is_err());
        assert!(queue.admit().is_empty());assert!(started.try_recv().is_err(),"a second root-exclusive ticket was admitted");
        release(&gate,1);await_idle(&mut queue);
        assert!(herdr_projects::execution_guard::RootGuard::exclusive(root.path()).is_ok());
        assert!(pool.stop(Duration::from_secs(1)));
    }
    #[test]
    fn cancellation_while_the_transfer_queue_is_saturated_still_runs_on_control() {
        let (gate,started)=gate();
        let limits=crate::executor::Limits{workers:[1,1],outstanding:[4,17],per_project:4,per_machine:4};
        let pool=Arc::new(crate::executor::Executor::new(limits,gate.clone()).unwrap());let mut queue=Queue::new(pool.clone());
        queue.offer_scoped(transfer("/projects/a","live-copy:1","/repos/a"),footprint("/projects/a","/repos/a")).unwrap();
        queue.offer_scoped(transfer("/projects/b","live-copy:1","/repos/b"),footprint("/projects/b","/repos/b")).unwrap();
        assert!(queue.admit().is_empty());
        assert_eq!(started.recv_timeout(Duration::from_secs(2)).unwrap(),"/projects/a");
        assert_eq!(queue.pending_sets.len(),1);assert!(queue.entries.values().any(|entry| entry.work.is_some()),"the saturated offer stays queued");
        let mut cancel=work("/control","cancel-transfer");cancel.command.program="cancel".into();cancel.lane=Lane::Control;cancel.identity.machine="control".into();
        let ticket=pool.submit(cancel).unwrap();
        assert!(ticket.recv_timeout(Duration::from_secs(2)).unwrap().result.unwrap().success());
        assert_eq!(started.recv_timeout(Duration::from_secs(2)).unwrap(),"cancel");
        assert!(queue.declared_pending(),"control ran before the transfer was released");
        queue.pending_sets.values().next().unwrap().ticket.cancel();gate.wake.notify_all();await_idle(&mut queue);
        assert!(gate.saw_cancel.load(std::sync::atomic::Ordering::SeqCst),"cancelled transfer did not observe Control");
        assert!(pool.stop(Duration::from_secs(1)));
    }

}

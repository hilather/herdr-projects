//! Volatile admission and fairness only; worker ingress owns all durable writes.
use super::*;
use std::collections::BTreeMap;
type Key=(String,String);
const LIMIT:usize=128;
const RETENTION:Duration=Duration::from_secs(180);
struct Entry {work:Option<Request>,not_before:Instant,touched:Instant,last:u64,needed:bool}
struct Pending {key:Key,identity:Identity,ticket:crate::executor::Ticket}
pub struct Queue {executor:Arc<crate::executor::Executor>,entries:BTreeMap<Key,Entry>,pending:Option<Pending>,sequence:u64,cursor:crate::fair_admission::Cursor}
impl Queue {
    pub fn new(executor:Arc<crate::executor::Executor>)->Self {Self{executor,entries:BTreeMap::new(),pending:None,sequence:0,cursor:crate::fair_admission::Cursor::default()}}
    pub fn pending(&self)->bool {self.pending.is_some()}
    pub fn outstanding(&self,project:&Project,id:&str)->bool {
        ["live-copy","final-copy"].iter().any(|kind|{
            let key=(project.canonical_dir().display().to_string(),format!("{kind}:{id}"));
            self.pending.as_ref().is_some_and(|p|p.key==key)||self.entries.get(&key).is_some_and(|e|e.needed)
        })
    }
    pub fn clear(&mut self,project:&Project,id:&str) {
        let key=(project.canonical_dir().display().to_string(),format!("live-copy:{id}"));
        if self.pending.as_ref().is_some_and(|p|p.key==key){return;}
        if let Some(entry)=self.entries.get_mut(&key){entry.work=None;entry.needed=false;}
    }
    pub fn offered(&self)->bool {self.entries.values().any(|e|e.work.is_some())}
    fn prune(&mut self) {
        let now=Instant::now();let pending=self.pending.as_ref().map(|p|&p.key);
        self.entries.retain(|key,e| {
            if e.work.as_ref().is_some_and(|r|r.deadline<=now){e.work=None;}
            Some(key)==pending||now<e.touched+RETENTION
        });
    }
    pub fn offer(&mut self,ctx:&Ctx<'_>,project:&Project,t:&Thread,target:Option<&str>)->Result<()> {
        self.offer_request(request(ctx,project,t,target)?)
    }
    pub fn offer_final(&mut self,ctx:&Ctx,project:&Project,t:&Thread,target:Option<&str>,purpose:Purpose,operation:String)->Result<()> {
        self.offer_request(request_final(ctx,project,t,target,purpose,operation)?)
    }
    pub fn offer_tokens(&mut self,ctx:&Ctx,project:&Project,t:Option<&Thread>,route:Option<&crate::remote_api::Route>)->Result<()> {
        self.offer_request(crate::token_jobs::request(ctx,project,t,route)?)
    }
    pub fn offer_notification(&mut self,ctx:&Ctx,project:&Project,c:&crate::project::Coordinator)->Result<()> {
        self.offer_request(crate::coordinator_jobs::request_notification(ctx,project,c)?)
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
    fn offer_request(&mut self,work:Request)->Result<()> {
        self.prune();let key=(work.identity.project.clone(),work.identity.operation.clone());
        if self.pending.as_ref().is_some_and(|p|p.key==key){return Ok(());}
        if !self.entries.contains_key(&key)&&self.entries.len()>=LIMIT {
            // Offers are volatile hints, not recovery authority. Rotating a full
            // inventory must not let refreshed failures exclude every new key.
            let now=Instant::now();
            let priority=|key:&Key| {let entry=&self.entries[key];(entry.work.is_none(),entry.not_before>now)};
            let victim=self.entries.keys().filter(|key|self.pending.as_ref().is_none_or(|p|&p.key!=*key)).max_by(|a,b|priority(a).cmp(&priority(b)).then_with(||self.compare(a,b))).cloned().context("copy inventory has no evictable offer")?;
            if priority(&victim)==(false,false)&&self.compare(&key,&victim)!=std::cmp::Ordering::Less {return Ok(());}
            self.entries.remove(&victim);
        }
        let now=Instant::now();let entry=self.entries.entry(key).or_insert(Entry{work:None,not_before:now,touched:now,last:0,needed:true});
        entry.work=Some(work);entry.touched=now;entry.needed=true;Ok(())
    }
    pub fn drain(&mut self)->Vec<String> {
        self.prune();let Some(pending)=&self.pending else {return Vec::new();};
        let result=match pending.ticket.try_recv() {
            Ok(None)=>return Vec::new(),
            Ok(Some(completion))=>{
                if completion.identity!=pending.identity {Err(anyhow::anyhow!("background completion identity mismatch"))}
                else {completion.result.and_then(|o|{ensure!(o.success(),"background worker failed");Ok(())})}
            },
            Err(error)=>Err(error),
        };
        let pending=self.pending.take().unwrap();let now=Instant::now();
        if let Some(entry)=self.entries.get_mut(&pending.key) {entry.not_before=now+if result.is_err()||pending.identity.operation=="notification"||pending.identity.operation.starts_with("tokens:"){Duration::from_secs(30)}else{Duration::ZERO};entry.touched=now;entry.needed=result.is_err();}
        result.err().map(|e|format!("{} {}: background queue: {e:#}",pending.key.0,pending.key.1)).into_iter().collect()
    }
    pub fn admit(&mut self)->Vec<String> {
        self.prune();let mut errors=Vec::new();if self.pending(){return errors;}
        while let Some(key)=self.next() {
            let next=match self.sequence.checked_add(1){Some(n)=>n,None=>{errors.push("copy admission sequence exhausted".into());break;}};
            let entry=self.entries.get_mut(&key).unwrap();let work=entry.work.take().unwrap();let identity=work.identity.clone();
            match self.executor.submit(work) {
                Ok(ticket)=>{
                    self.sequence=next;entry.last=next;self.cursor.accepted(&key);
                    self.pending=Some(Pending{key,identity,ticket});break;
                },
                Err(error)=>{entry.not_before=Instant::now()+Duration::from_secs(30);errors.push(format!("{} {}: background admission: {error:#}",key.0,key.1));},
            }
        }
        errors
    }
    fn compare(&self,a:&Key,b:&Key)->std::cmp::Ordering {self.cursor.compare(a,b)}
    fn next(&self)->Option<Key> {
        let now=Instant::now();
        self.entries.iter().filter(|(_,e)|e.work.is_some()&&now>=e.not_before)
            .min_by(|(a,_),(b,_)|self.compare(a,b)).map(|(key,_)|key.clone())
    }

}
impl Drop for Queue {fn drop(&mut self){if let Some(pending)=&self.pending {pending.ticket.cancel();}}}

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
}

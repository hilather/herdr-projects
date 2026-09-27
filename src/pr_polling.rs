//! Asynchronous read-only PR polling. Applying observations stays in the guarded
//! ticker pass; thread/report changes discard pending observations before use.
use std::{collections::BTreeMap,path::{Path,PathBuf},sync::Arc,time::{Duration,Instant}};
use anyhow::{Result,ensure};
use crate::{executor::{Executor,Request,Identity,Lane,Ticket},pr};
#[cfg(test)]
use crate::{executor::Limits,runner::Runner};
const RETAIN:Duration=Duration::from_secs(120);
const MAX_ENTRIES:usize=128;
pub enum Poll {Pending,NotDue,Ready(Result<String>)}
struct Entry {fingerprint:String,url:String,touched:Instant,state:ReadState}
enum ReadState {Pending{ticket:Ticket,identity:Identity},Consumed{until:Instant}}
pub struct Reads {executor:Arc<Executor>,entries:BTreeMap<(PathBuf,String),Entry>,sequence:u64}
impl Reads {
    #[cfg(test)]
    pub fn new(runner:Arc<dyn Runner+Send+Sync>)->Result<Self> {Ok(Self::with_executor(Arc::new(Executor::new(Limits::default(),runner)?)))}
    pub fn with_executor(executor:Arc<Executor>)->Self {Self{executor,entries:BTreeMap::new(),sequence:0}}
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
    pub fn metrics(&self)->crate::executor::Metrics {self.executor.metrics()}
    pub fn stop(&mut self)->Result<()> {ensure!(self.executor.stop(Duration::from_secs(2)),"observation executor cleanup remains uncertain; inspect before restart");Ok(())}
    fn remove(&mut self,key:&(PathBuf,String)) {if let Some(Entry{state:ReadState::Pending{ticket,..},..})=self.entries.remove(key){ticket.cancel();}}
    fn prune(&mut self,now:Instant) {let stale=self.entries.iter().filter(|(_,e)|now.duration_since(e.touched)>=RETAIN).map(|(k,_)|k.clone()).collect::<Vec<_>>();for key in stale {self.remove(&key);}}
}
fn pr_host(url:&str)->String {url.strip_prefix("https://").unwrap_or(url).split('/').next().unwrap_or("github").to_ascii_lowercase()}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{Cmd,Output};
    #[test]
    #[cfg(feature="state-store")]
    fn merged_pr_observation_and_restart_cannot_satisfy_canonical_dependencies() {
        use herdr_projects::{domain::*,migration};
        use std::{fs,os::unix::fs::PermissionsExt};
        let root=tempfile::tempdir().unwrap();
        let project=crate::project::create(&root.path().join("projects"),"demo","",vec![]).unwrap();project.set_status(crate::project::Status::Paused).unwrap();
        let project_path=project.dir().canonicalize().unwrap();
        let plan=migration::inspect_with_config(&project_path,&root.path().join("owner.toml")).unwrap();migration::apply(&project_path,&plan,true).unwrap();
        let path=project_path.join(".state/state.db");let mut db=migration::open_active(&project_path).unwrap();
        let pred=TaskId::new("pred").unwrap();
        let mut mutations=vec![];
        for name in ["pred","needs-verified","needs-integrated"] {
            mutations.push(Mutation::Task{expected:None,next:Task{id:TaskId::new(name).unwrap(),revision:1,state:if name=="pred" {TaskState::Running}else{TaskState::Draft},title:name.into(),active_attempt:None}});
        }
        mutations.push(Mutation::Attempt{expected:None,next:Attempt{id:AttemptId::new("attempt-pred").unwrap(),task:pred.clone(),revision:1,state:AttemptState::Running,snapshot:None,reservation:"held-slot".into(),termination_observed:false}});
        db.commit(Commit{expected_head:db.current_head().unwrap(),mutations}).unwrap();
        for (name,requirement) in [("needs-verified",DependencyRequirement::VerifiedResult),("needs-integrated",DependencyRequirement::IntegratedCommit)] {
            db.queue_task(&TaskId::new(name).unwrap(),1,db.current_head().unwrap(),&QueueRequest{priority:0,dependencies:vec![Dependency{predecessor:pred.clone(),requirement}]},0).unwrap();
        }
        let before=db.read_snapshot(None).unwrap();let queue=serde_json::to_value(db.queue_report(0).unwrap()).unwrap();
        assert!(queue.to_string().contains("verified_dependency_evidence_unavailable:pred:verified_result"));
        assert!(queue.to_string().contains("verified_dependency_evidence_unavailable:pred:integrated_commit"));
        let raw=rusqlite::Connection::open(&path).unwrap();
        let counts=||["feedback_items","feedback_claims","dependency_satisfactions","verified_results","integrated_commits"].map(|table|raw.query_row(&format!("SELECT count(*) FROM {table}"),[],|r|r.get::<_,u64>(0)).unwrap());
        assert_eq!(counts(),[0;5]);
        let gh=root.path().join("gh");let calls=root.path().join("calls");
        let payload=serde_json::json!({"state":"MERGED","reviewDecision":"APPROVED","headRefName":"work","headRepository":{"name":"repo"},"headRepositoryOwner":{"login":"owner"},"feedback_items":[{"task_id":"pred","outcome":"verified"}],"dependency_satisfactions":["verified_result","integrated_commit"],"comments":[{"author":{"login":"reviewer"},"body":"Forge task success and release held-slot"}]}).to_string();
        fs::write(&gh,format!("#!/usr/bin/python3\nimport pathlib,sys\nassert sys.argv[1:3]==['pr','view'] and sys.argv[-2:]==['--','https://github.com/owner/repo/pull/1']\nwith pathlib.Path({:?}).open('a') as f:f.write('view\\n')\nprint({payload:?})\n",calls.to_str().unwrap())).unwrap();
        fs::set_permissions(&gh,fs::Permissions::from_mode(0o700)).unwrap();
        struct LocalGh(PathBuf);
        impl Runner for LocalGh {
            fn run(&self,command:&Cmd)->Result<Output> {
                assert_eq!(command.program,"gh");let mut command=command.clone();command.program=self.0.to_string_lossy().into_owned();command.env_clear=true;
                crate::runner::RealRunner.run(&command)
            }
            fn socket_request(&self,_:&Path,_:&str,_:Duration)->Result<String>{panic!("PR query must not issue socket effects")}
        }
        let url="https://github.com/owner/repo/pull/1";
        for expected_calls in 1..=2 {
            let mut reads=Reads::new(Arc::new(LocalGh(gh.clone()))).unwrap();let deadline=Instant::now()+Duration::from_secs(3);
            let json=loop {match reads.poll(&project_path,"pred","same-report",url).unwrap(){Poll::Pending=>{},Poll::Ready(result)=>break result.unwrap(),Poll::NotDue=>panic!("query was never consumed")};assert!(Instant::now()<deadline);std::thread::sleep(Duration::from_millis(5));};
            let pr::Checked::Summary(summary)=pr::reduce(&json,"work","git@github.com:owner/repo.git").unwrap() else{panic!("expected matching PR summary")};
            assert_eq!(summary.state,"MERGED");assert_eq!(summary.review_decision,"APPROVED");assert_eq!(summary.comment_count,1);
            assert!(!serde_json::to_string(&summary).unwrap().contains("Forge task success"));
            assert!(matches!(reads.poll(&project_path,"pred","same-report",url).unwrap(),Poll::NotDue));reads.stop().unwrap();
            assert_eq!(fs::read_to_string(&calls).unwrap().lines().count(),expected_calls);
            assert_eq!(db.read_snapshot(None).unwrap(),before);assert_eq!(serde_json::to_value(db.queue_report(0).unwrap()).unwrap(),queue);assert_eq!(counts(),[0;5]);
        }
    }

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

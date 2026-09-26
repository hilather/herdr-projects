use super::*;
use crate::runner::RealRunner;
fn request(id:&str,project:&str,machine:&str,lane:Lane,script:&str)->Request {Request{identity:Identity{operation:id.into(),revision:7,project:project.into(),machine:machine.into(),terminal:None},lane,deadline:Instant::now()+Duration::from_secs(10),command:Cmd::new("sh",Duration::from_secs(10)).args(["-c",script])}}
fn limits()->Limits {Limits{workers:[2,1],outstanding:[8,8],per_project:1,per_machine:1}}
#[test]
fn slow_transfer_does_not_block_control_and_stop_cancels_owned_processes() {
    let pool=Executor::new(limits(),Arc::new(RealRunner)).unwrap();let slow=pool.submit(request("copy","a","local",Lane::Transfer,"sleep 9")).unwrap();let start=Instant::now();let status=pool.submit(request("status","a","local",Lane::Control,"printf ready")).unwrap().recv_timeout(Duration::from_secs(2)).unwrap();assert!(status.result.unwrap().success());assert_eq!(status.identity.revision,7);assert!(start.elapsed()<Duration::from_secs(2));assert!(pool.stop(Duration::from_secs(2)));assert!(slow.recv_timeout(Duration::from_secs(1)).unwrap().result.unwrap().cancelled);assert!(pool.submit(request("late","a","local",Lane::Control,"true")).is_err());
}
struct Gate {started:mpsc::Sender<String>,release:Mutex<mpsc::Receiver<()>>}
impl Runner for Gate {
    fn run(&self,cmd:&Cmd)->Result<Output> {self.started.send(cmd.program.clone()).unwrap();if cmd.program=="hold" {loop {if cmd.cancellation.as_ref().unwrap().is_cancelled(){return Ok(Output{cancelled:true,..Output::default()});}match self.release.lock().unwrap().recv_timeout(Duration::from_millis(5)){Ok(())=>break,Err(mpsc::RecvTimeoutError::Timeout)=>{},Err(_)=>break}}}Ok(Output{code:Some(0),..Output::default()})}
    fn socket_request(&self,_:&std::path::Path,_:&str,_:Duration)->Result<String>{unreachable!()}
}
fn fake_request(id:&str,project:&str,machine:&str,program:&str)->Request {let mut r=request(id,project,machine,Lane::Control,"");r.command=Cmd::new(program,Duration::from_secs(10));r}
#[test]
fn queue_bounds_machine_isolation_expiry_and_cancellation_prevent_spawns() {
    let(started,rx)=mpsc::channel();let(release,gate)=mpsc::channel();let mut bounds=limits();bounds.outstanding[0]=4;let pool=Executor::new(bounds,Arc::new(Gate{started,release:Mutex::new(gate)})).unwrap();
    let hold=pool.submit(fake_request("hold","a","bad-host","hold")).unwrap();assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(),"hold");
    let mut expiring=fake_request("expires","b","bad-host","must-not-run");expiring.deadline=Instant::now()+Duration::from_millis(40);let expired=pool.submit(expiring).unwrap();
    let cancel=pool.submit(fake_request("cancel","c","bad-host","must-not-run")).unwrap();cancel.cancel();
    let live=pool.submit(fake_request("healthy","d","good-host","healthy")).unwrap();assert!(live.recv_timeout(Duration::from_secs(1)).unwrap().result.unwrap().success());assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(),"healthy");let expired=expired.recv_timeout(Duration::from_secs(1)).unwrap();assert!(!expired.runner_entered);assert!(expired.result.unwrap().timed_out);assert!(cancel.recv_timeout(Duration::from_secs(1)).unwrap().result.unwrap().cancelled);
    assert!(pool.submit(fake_request("hold","a","bad-host","duplicate")).is_err());release.send(()).unwrap();assert!(hold.recv_timeout(Duration::from_secs(1)).unwrap().result.unwrap().success());assert!(rx.try_recv().is_err());assert!(pool.stop(Duration::from_secs(1)));
}
#[test]
fn oldest_other_project_gets_a_turn_and_terminal_is_serialized_across_lanes() {
    let(started,rx)=mpsc::channel();let(release,gate)=mpsc::channel();let mut bounds=limits();bounds.workers=[1,1];bounds.outstanding=[3,1];let pool=Executor::new(bounds,Arc::new(Gate{started,release:Mutex::new(gate)})).unwrap();
    let mut first=fake_request("first","a","host","hold");first.identity.terminal=Some("socket:pane".into());let first=pool.submit(first).unwrap();assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(),"hold");
    let a=pool.submit(fake_request("second","a","host","a")).unwrap();let b=pool.submit(fake_request("third","b","host2","b")).unwrap();assert!(pool.submit(fake_request("full","c","host3","overflow")).is_err());
    let mut terminal=fake_request("terminal","c","host3","terminal");terminal.lane=Lane::Transfer;terminal.identity.terminal=Some("socket:pane".into());let terminal=pool.submit(terminal).unwrap();assert!(rx.recv_timeout(Duration::from_millis(40)).is_err());release.send(()).unwrap();first.recv_timeout(Duration::from_secs(1)).unwrap();
    let order=(0..3).map(|_|rx.recv_timeout(Duration::from_secs(1)).unwrap()).collect::<Vec<_>>();assert!(order.iter().position(|s|s=="b").unwrap()<order.iter().position(|s|s=="a").unwrap());for ticket in [a,b,terminal]{assert!(ticket.recv_timeout(Duration::from_secs(1)).unwrap().result.unwrap().success());}assert!(pool.stop(Duration::from_secs(1)));
}
#[test]
fn panicking_adapter_quarantines_executor_and_reports_uncertain_cleanup() {
    struct Panics;
    impl Runner for Panics {
        fn run(&self,_:&Cmd)->Result<Output>{panic!("fixture adapter failure");}
        fn socket_request(&self,_:&std::path::Path,_:&str,_:Duration)->Result<String>{unreachable!()}
    }
    let pool=Executor::new(limits(),Arc::new(Panics)).unwrap();let failed=pool.submit(fake_request("panic","a","local","panic")).unwrap().recv_timeout(Duration::from_secs(1)).unwrap();assert!(failed.result.unwrap_err().to_string().contains("uncertain"));assert!(pool.submit(fake_request("successor","a","local","true")).is_err());assert!(!pool.stop(Duration::from_secs(1)));assert!(pool.metrics().uncertain);
}
#[test]
fn operation_ids_are_project_scoped_and_dropped_consumers_do_not_block_drain() {
    let(started,rx)=mpsc::channel();let(release,gate)=mpsc::channel();let pool=Executor::new(limits(),Arc::new(Gate{started,release:Mutex::new(gate)})).unwrap();let first=pool.submit(fake_request("same-id","a","host1","hold")).unwrap();rx.recv_timeout(Duration::from_secs(1)).unwrap();let second=pool.submit(fake_request("same-id","b","host2","true")).unwrap();assert!(second.recv_timeout(Duration::from_secs(1)).unwrap().result.unwrap().success());drop(first);release.send(()).unwrap();assert!(pool.stop(Duration::from_secs(1)));assert_eq!(pool.metrics().completed,[2,0]);assert_eq!(pool.metrics().running,[0,0]);
}
#[test]
fn oversized_command_or_capture_is_rejected_before_admission() {
    let pool=Executor::new(limits(),Arc::new(RealRunner)).unwrap();let mut r=request("large","a","local",Lane::Control,"");r.command.stdin=Some("x".repeat(1024*1024+1));assert!(pool.submit(r).is_err());let mut r=request("capture","a","local",Lane::Control,"");r.command.capture_limit=usize::MAX;assert!(pool.submit(r).is_err());let mut r=request("unowned","a","local",Lane::Control,"");r.command.own_group=false;assert!(pool.submit(r).is_err());assert_eq!(pool.metrics().high_water,[0,0]);assert!(pool.stop(Duration::from_secs(1)));
}
#[test]
fn bounded_burst_reports_observed_concurrency_and_queue_delay() {
    use std::sync::atomic::{AtomicUsize,Ordering};
    struct Load {active:AtomicUsize,peak:AtomicUsize}
    impl Runner for Load {
        fn run(&self,_:&Cmd)->Result<Output>{let active=self.active.fetch_add(1,Ordering::SeqCst)+1;self.peak.fetch_max(active,Ordering::SeqCst);std::thread::sleep(Duration::from_millis(5));self.active.fetch_sub(1,Ordering::SeqCst);Ok(Output{code:Some(0),..Output::default()})}
        fn socket_request(&self,_:&std::path::Path,_:&str,_:Duration)->Result<String>{unreachable!()}
    }
    let runner=Arc::new(Load{active:AtomicUsize::new(0),peak:AtomicUsize::new(0)});let mut bounds=limits();bounds.workers=[4,1];bounds.outstanding=[32,1];let pool=Executor::new(bounds,runner.clone()).unwrap();let tickets=(0..32).map(|i|pool.submit(fake_request(&format!("op-{i}"),&format!("project-{}",i%8),&format!("host-{}",i%8),"load")).unwrap()).collect::<Vec<_>>();for ticket in tickets {assert!(ticket.recv_timeout(Duration::from_secs(2)).unwrap().result.unwrap().success());}assert!(pool.stop(Duration::from_secs(1)));let metrics=pool.metrics();assert_eq!(metrics.completed,[32,0]);assert!(metrics.high_water[0]<=32);assert!(metrics.max_queue_delay>Duration::ZERO);assert!((1..=4).contains(&runner.peak.load(Ordering::SeqCst)));assert_eq!(runner.active.load(Ordering::SeqCst),0);
}

#[test]
fn absolute_deadline_survives_delay_between_worker_and_process_entry() {
    struct Delayed {expected:Instant}
    impl Runner for Delayed {
        fn run(&self,cmd:&Cmd)->Result<Output> {
            assert_eq!(cmd.deadline,Some(self.expected));
            std::thread::sleep(Duration::from_millis(250));
            RealRunner.run(cmd)
        }
        fn socket_request(&self,_:&std::path::Path,_:&str,_:Duration)->Result<String>{unreachable!()}
    }
    let dir=tempfile::tempdir().unwrap();let end=Instant::now()+Duration::from_millis(150);
    let pool=Executor::new(limits(),Arc::new(Delayed{expected:end})).unwrap();
    let mut work=request("absolute","project","local",Lane::Control,"touch marker");
    work.command.cwd=Some(dir.path().to_path_buf());work.command.deadline=Some(end);
    let completion=pool.submit(work).unwrap().recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(completion.runner_entered);assert!(completion.result.unwrap().timed_out);
    assert!(!dir.path().join("marker").exists());assert!(pool.stop(Duration::from_secs(1)));
}
struct Pause {release:Mutex<mpsc::Receiver<()>>,deadline:Instant,order:Mutex<Vec<String>>,bad:Mutex<Vec<Option<Instant>>>}
impl Runner for Pause {
    fn run(&self,cmd:&Cmd)->Result<Output> {
        self.order.lock().unwrap().push(cmd.program.clone());
        if cmd.program!="hold"&&cmd.deadline!=Some(self.deadline) {self.bad.lock().unwrap().push(cmd.deadline);}
        if cmd.program=="hold"||cmd.program=="transfer"||cmd.program=="ordinary-a"||cmd.program=="ordinary-b" {
            while !cmd.cancellation.as_ref().unwrap().is_cancelled()&&cmd.deadline.is_none_or(|deadline|Instant::now()<deadline) {
                if self.release.lock().unwrap().recv_timeout(Duration::from_millis(5)).is_ok() {break;}
            }
        }
        Ok(Output{code:Some(0),..Output::default()})
    }
    fn socket_request(&self,_:&std::path::Path,_:&str,_:Duration)->Result<String>{unreachable!()}
}
#[test]
fn eight_projects_receive_service_within_two_fairness_rounds() {
    let (release,gate)=mpsc::channel();let deadline=Instant::now()+Duration::from_secs(30);
    let probe=Arc::new(Pause{release:Mutex::new(gate),deadline,order:Mutex::new(Vec::new()),bad:Mutex::new(Vec::new())});
    let bounds=Limits{workers:[1,1],outstanding:[64,8],per_project:8,per_machine:8};
    let pool=Executor::new(bounds,probe.clone()).unwrap();
    let mut hold=request("hold","gate","host",Lane::Control,"");hold.command=Cmd::new("hold",Duration::from_secs(30));hold.deadline=deadline;
    pool.submit(hold).unwrap();
    let ready=Instant::now()+Duration::from_secs(2);
    while probe.order.lock().unwrap().is_empty() {assert!(Instant::now()<ready);std::thread::sleep(Duration::from_millis(5));}
    for project in 0..8 {for n in 0..3 {
        let mut work=request(&format!("op-{project}-{n}"),&format!("project-{project}"),"host",Lane::Control,"");
        work.command=Cmd::new(format!("p{project}"),Duration::from_secs(30));work.deadline=deadline;work.command.deadline=Some(deadline);
        pool.submit(work).unwrap();
    }}
    release.send(()).unwrap();
    let ready=Instant::now()+Duration::from_secs(3);
    while probe.order.lock().unwrap().len()<17 {assert!(Instant::now()<ready,"start order stalled: {:?}",probe.order.lock().unwrap());std::thread::sleep(Duration::from_millis(2));}
    let order=probe.order.lock().unwrap().clone();
    let started:Vec<_>=order.into_iter().filter(|program|program!="hold").take(16).collect();
    assert_eq!(started.len(),16,"{started:?}");
    let first:std::collections::BTreeSet<_>=started.iter().take(8).cloned().collect();
    assert_eq!(first,["p0","p1","p2","p3","p4","p5","p6","p7"].into_iter().map(str::to_string).collect(),"first round: {started:?}");
    assert!(probe.bad.lock().unwrap().is_empty(),"deadline moved: {:?}",probe.bad.lock().unwrap());
    assert!(pool.stop(Duration::from_secs(1)));
}
#[test]
fn full_transfer_queue_does_not_block_cancel_selection() {
    let (release,gate)=mpsc::channel();let deadline=Instant::now()+Duration::from_secs(10);
    let probe=Arc::new(Pause{release:Mutex::new(gate),deadline,order:Mutex::new(Vec::new()),bad:Mutex::new(Vec::new())});
    let bounds=Limits{workers:[2,1],outstanding:[8,1],per_project:4,per_machine:4};
    let pool=Executor::new(bounds,probe.clone()).unwrap();
    let mut transfer=request("live-copy:1","transfer","transfer-host",Lane::Transfer,"");transfer.command=Cmd::new("transfer",Duration::from_secs(10));transfer.deadline=deadline;
    pool.submit(transfer).unwrap();
    let ready=Instant::now()+Duration::from_secs(2);
    while !probe.order.lock().unwrap().iter().any(|program|program=="transfer") {assert!(Instant::now()<ready);std::thread::sleep(Duration::from_millis(5));}
    for (operation,project,program) in [("bulk-a","a","ordinary-a"),("bulk-b","b","ordinary-b"),("canonical-cancel:1","c","cancel")] {
        let mut work=request(operation,project,&format!("host-{project}"),Lane::Control,"");work.command=Cmd::new(program,Duration::from_secs(10));work.deadline=deadline;pool.submit(work).unwrap();
    }
    let ready=Instant::now()+Duration::from_secs(2);let mut control=Vec::new();
    while control.len()<2&&Instant::now()<ready {
        let seen=probe.order.lock().unwrap().iter().filter(|program|program.starts_with("ordinary")||*program=="cancel").cloned().collect::<Vec<_>>();
        if seen.len()>=2 {control=seen;break;}
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(control.contains(&"ordinary-a".to_string()),"{control:?}");
    assert!(control.contains(&"cancel".to_string()),"cancel was not selected while Transfer was full: {control:?}");
    assert!(!control.contains(&"ordinary-b".to_string()),"ordinary work took the reserved Control slot: {control:?}");
    release.send(()).unwrap();assert!(pool.stop(Duration::from_secs(1)));
}
#[test]
fn original_deadlines_are_unchanged_across_retries() {
    let (release,gate)=mpsc::channel();let deadline=Instant::now()+Duration::from_secs(30);
    let probe=Arc::new(Pause{release:Mutex::new(gate),deadline,order:Mutex::new(Vec::new()),bad:Mutex::new(Vec::new())});
    let pool=Executor::new(Limits{workers:[1,1],outstanding:[8,8],per_project:4,per_machine:4},probe.clone()).unwrap();
    let mut hold=request("hold","gate","host",Lane::Control,"");hold.command=Cmd::new("hold",Duration::from_secs(30));hold.deadline=deadline;pool.submit(hold).unwrap();
    let ready=Instant::now()+Duration::from_secs(2);
    while probe.order.lock().unwrap().is_empty() {assert!(Instant::now()<ready);std::thread::sleep(Duration::from_millis(5));}
    let mut retry=request("retry-1","project","host",Lane::Control,"");retry.command=Cmd::new("retry",Duration::from_secs(30));retry.deadline=deadline;retry.command.deadline=Some(deadline);pool.submit(retry).unwrap();
    std::thread::sleep(Duration::from_millis(80));
    release.send(()).unwrap();
    let ready=Instant::now()+Duration::from_secs(2);
    while !probe.order.lock().unwrap().iter().any(|program|program=="retry") {assert!(Instant::now()<ready);std::thread::sleep(Duration::from_millis(5));}
    assert!(probe.bad.lock().unwrap().is_empty(),"retry deadline changed: {:?}",probe.bad.lock().unwrap());
    assert!(pool.stop(Duration::from_secs(1)));
}

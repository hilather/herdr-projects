//! Notification adapter retaining the root execution lease through durable receipt.
use std::{path::{Path,PathBuf},time::Duration};
use anyhow::{Context,Result,ensure};
use crate::{cleanup,herdr::{self,Herdr},paths::Ctx,project};
use herdr_projects::{domain::{Operation,OperationId,TaskId},migration::{self,ConfigReference},operations::{Claim,Outcome,dispatch::{self,DeliveryAdapter,PreparedDelivery,DispatchRequest,DispatchResult},notification::Notification},runtime};

pub(crate) fn config(ctx:&Ctx,project:&Path)->Result<ConfigReference> {
    let path=std::path::absolute(ctx.config_dir.join("config.toml"))?;
    let reference=migration::config_reference(&path)?;
    let text=if let Some(digest)=&reference.digest {
        let bytes=migration::read_plan_file(&path)?;
        ensure!(crate::thread::sha256_hex(&bytes)==*digest,"config changed before typed policy validation");
        String::from_utf8(bytes)?
    }else{String::new()};
    project::parse_safety(&text,project).map_err(|_|anyhow::anyhow!("invalid notification safety configuration (contents withheld)"))?;
    ensure!(migration::config_reference(&path)?==reference,"config changed while reading notification policy");Ok(reference)
}

pub fn enqueue(ctx:&Ctx,project:&Path,task:&TaskId,head:u64)->Result<Operation> {
    let project=project.canonicalize()?;
    runtime::enqueue_notification(&project,task,head,&config(ctx,&project)?)
}

struct Adapter<'a,'b> {ctx:&'a Ctx<'b>,project:PathBuf}
struct Prepared<'a,'b> {ctx:&'a Ctx<'b>,project:PathBuf,notification:Notification,socket:String,_lease:cleanup::Lease}
impl<'a,'b> DeliveryAdapter for Adapter<'a,'b> {
    type Prepared=Prepared<'a,'b>;
    fn prepare(&mut self,op:&Operation)->Result<Self::Prepared> {
        let lease=cleanup::lease(self.project.parent().context("project has no root")?)?;
        let notification=Notification::decode(op)?;
        let reference=config(self.ctx,&self.project)?;
        let snapshot=runtime::snapshot(&self.project)?;
        let socket=notification.validate(op,&snapshot,&reference)?.identity.socket.clone();
        ensure!(herdr::version(&self.ctx.env.herdr_bin(),self.ctx.runner)?>=herdr::MIN_VERSION,"unsupported Herdr version");
        Ok(Prepared{ctx:self.ctx,project:self.project.clone(),notification,socket,_lease:lease})
    }
}
impl PreparedDelivery for Prepared<'_,'_> {
    fn revalidate(&mut self,op:&Operation)->Result<()> {
        let reference=config(self.ctx,&self.project)?;
        let snapshot=runtime::snapshot(&self.project)?;
        ensure!(self.notification.validate(op,&snapshot,&reference)?.identity.socket==self.socket,"notification socket changed");Ok(())
    }
    fn deliver(&mut self,op:&Operation,claim:&Claim)->Result<Outcome> {
        // Reserve time for the entire bounded Herdr call and receipt commit.
        let now=jiff::Timestamp::now().as_millisecond();
        if claim.lease_until_ms-now<(herdr::CALL_TIMEOUT+Duration::from_secs(2)).as_millis() as i64 {
            return Ok(Outcome::Retryable{no_effect_evidence:"notification was not called: insufficient remaining lease".into()});
        }
        let h=Herdr::new(self.ctx.env.herdr_bin(),&self.socket,self.ctx.runner);
        h.notification_show(&self.notification.title,&self.notification.body)?;
        Ok(Outcome::Confirmed{observed_identity:format!("herdr.notification.shown:{}:binding-{}:epoch-{}",op.id.as_str(),self.notification.binding_revision,self.notification.control_epoch)})
    }
}

pub fn deliver(ctx:&Ctx,project:&Path,id:&OperationId,revision:u64)->Result<DispatchResult> {
    let project=project.canonicalize()?;
    let mut db=migration::open_active(&project)?;
    let mut adapter=Adapter{ctx,project};
    dispatch::dispatch_one(&mut db,DispatchRequest{operation:id,expected_revision:revision,owner:"operator.notification",lease_ms:60_000},&mut adapter,||jiff::Timestamp::now().as_millisecond())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{scenarios::World,runner::fake::{ok,fail}};
    use herdr_projects::{domain::{ProjectState,RuntimeRoute},operations::DeliveryState};
    pub(crate) fn fixture()->(World,PathBuf,TaskId) {
        fixture_with_items(1)
    }
    pub(crate) fn fixture_with_items(count:usize)->(World,PathBuf,TaskId) {
        let world=World::new();let project=project::create(&world.root,"notify","",vec![]).unwrap();project.set_status(project::Status::Paused).unwrap();
        for _ in 0..count{crate::inbox::write(&project,"test","fixture","private inbox contents are not sent","").unwrap();}
        let dir=project.dir().canonicalize().unwrap();let plan=migration::inspect(&dir).unwrap();migration::apply(&dir,&plan,true).unwrap();
        let task=TaskId::new("notification-task").unwrap();let head=runtime::snapshot(&dir).unwrap().head;let head=runtime::add_task(&dir,task.clone(),"notify".into(),head).unwrap();
        runtime::create_binding(&dir,None,None,head,&RuntimeRoute{socket:"/explicit/notification.sock".into(),..Default::default()}).unwrap();
        crate::reconcile_live::run(&world.ctx(),&dir,true).unwrap();let snapshot=runtime::snapshot(&dir).unwrap();runtime::set_state(&dir,snapshot.head,snapshot.control.unwrap().revision,ProjectState::Active,&world.ctx().config_dir.join("config.toml")).unwrap();
        (world,dir,task)
    }
    fn queued(world:&World,dir:&Path,task:&TaskId)->Operation {enqueue(&world.ctx(),dir,task,runtime::snapshot(dir).unwrap().head).unwrap()}
    #[test]
    fn notification_receipt_prevents_replay_and_lease_excludes_supported_mutation() {
        let(world,dir,task)=fixture();world.runner.on("--version",ok("herdr 0.9.1"));let operation=queued(&world,&dir,&task);let held_dir=dir.clone();
        world.runner.on_fn(|c|c.args.first().is_some_and(|s|s=="notification"),move |cmd|{
            assert!(cmd.env.iter().any(|(k,v)|k=="HERDR_SOCKET_PATH"&&v=="/explicit/notification.sock"));assert!(cmd.env_remove.iter().any(|k|k=="HERDR_SESSION"));assert!(!cmd.display().contains("private inbox contents"));
            let snapshot=runtime::snapshot(&held_dir).unwrap();assert!(runtime::rebind(&held_dir,"coordinator",1,snapshot.head,&RuntimeRoute::default()).is_err());
            // A separate write proves the effect is outside a DB transaction.
            let mut db=migration::open_active(&held_dir).unwrap();assert_eq!(db.deliveries().unwrap()[0].state,DeliveryState::Claimed);assert_eq!(db.expire_claims(0).unwrap(),0);
            Ok(ok(r#"{"result":{"shown":true}}"#))
        });
        assert!(matches!(deliver(&world.ctx(),&dir,&operation.id,1).unwrap(),DispatchResult::Recorded(d) if d.state==DeliveryState::Confirmed));
        migration::recover(&dir,true).unwrap();assert!(deliver(&world.ctx(),&dir,&operation.id,1).is_err());assert!(enqueue(&world.ctx(),&dir,&task,runtime::snapshot(&dir).unwrap().head).is_err());assert_eq!(world.runner.count("notification show"),1);
    }
    #[test]
    fn notification_failure_or_unconfirmed_response_remains_ambiguous_after_restart() {
        for response in [fail(1,"private failure"),ok(r#"{"result":{"shown":false}}"#)] {
            let(world,dir,task)=fixture();world.runner.on("--version",ok("herdr 0.9.1")).on("notification show",response);let operation=queued(&world,&dir,&task);
            let result=deliver(&world.ctx(),&dir,&operation.id,1).unwrap();assert!(!format!("{result:?}").contains("private failure"));assert!(matches!(result,DispatchResult::Recorded(d) if d.state==DeliveryState::Ambiguous));migration::recover(&dir,true).unwrap();assert!(deliver(&world.ctx(),&dir,&operation.id,3).is_err());assert_eq!(world.runner.count("notification show"),1);
        }
    }
    #[test]
    fn notification_fences_changed_policy_route_task_and_inbox_without_sending() {
        for change in ["pause","route","task","inbox","config","invalid-safety"] {
            let(world,dir,task)=fixture();world.runner.on("--version",ok("herdr 0.9.1"));let operation=queued(&world,&dir,&task);let snapshot=runtime::snapshot(&dir).unwrap();
            match change {
                "pause"=>{runtime::set_state(&dir,snapshot.head,snapshot.control.unwrap().revision,ProjectState::Paused,&world.ctx().config_dir.join("config.toml")).unwrap();},
                "route"=>{runtime::rebind(&dir,"coordinator",1,snapshot.head,&RuntimeRoute{socket:"/replacement.sock".into(),..Default::default()}).unwrap();},
                "task"=>{runtime::rename_task(&dir,&task,"changed".into(),1,snapshot.head).unwrap();},
                "inbox"=>{runtime::update_inbox(&dir,snapshot.head,&[snapshot.inbox[0].content.id.clone()],false).unwrap();},
                _=>{let config=world.ctx().config_dir;std::fs::create_dir_all(&config).unwrap();std::fs::write(config.join("config.toml"),if change=="config"{"# changed"}else{"[safety]\ninvalid=7"}).unwrap();},
            }
            assert!(deliver(&world.ctx(),&dir,&operation.id,1).is_err(),"{change}");assert_eq!(world.runner.count("notification show"),0);assert_eq!(migration::open_active(&dir).unwrap().deliveries().unwrap()[0].attempts,0);
        }
    }
    #[test]
    fn notification_typed_safety_is_checked_even_when_generic_config_is_admitted() {
        let(world,dir,task)=fixture();let config_path=world.ctx().config_dir.join("config.toml");std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path,format!("[safety.{}]\nstart_threads='invalid'\n",serde_json::to_string(dir.to_str().unwrap()).unwrap())).unwrap();
        crate::reconcile_live::run(&world.ctx(),&dir,true).unwrap();let snapshot=runtime::snapshot(&dir).unwrap();runtime::set_state(&dir,snapshot.head,snapshot.control.unwrap().revision,ProjectState::Active,&config_path).unwrap();
        assert!(enqueue(&world.ctx(),&dir,&task,runtime::snapshot(&dir).unwrap().head).is_err());assert!(migration::open_active(&dir).unwrap().deliveries().unwrap().is_empty());assert_eq!(world.runner.count("notification show"),0);
    }
    #[test]
    fn notification_policy_withdrawn_after_prepare_records_proven_no_effect() {
        let(world,dir,task)=fixture();let operation=queued(&world,&dir,&task);let config_path=world.ctx().config_dir.join("config.toml");
        world.runner.on_fn(|c|c.args==["--version"],move |_|{std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();std::fs::write(&config_path,"# changed after prepare").unwrap();Ok(ok("herdr 0.9.1"))});
        let result=deliver(&world.ctx(),&dir,&operation.id,1).unwrap();assert!(matches!(result,DispatchResult::Recorded(d) if d.state==DeliveryState::Pending));assert_eq!(world.runner.count("notification show"),0);
    }
}

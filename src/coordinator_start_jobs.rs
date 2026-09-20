//! Coordinator startup under the same ownership and frozen inputs as priming.
use super::*;
use herdr_projects::launch_claim::{Claim as LaunchClaim,Phase as LaunchPhase};

pub(super) fn ready(c:&Coordinator)->Result<()> {
    super::ready(c)?;
    ensure!(c.launch_attempts<crate::coordinator::MAX_LAUNCH_ATTEMPTS,"coordinator launch attempts exhausted");
    if let Some(claim)=&c.launch_claim {
        ensure!(claim.phase!=LaunchPhase::Pending&&claim.notified&&claim.generation!=c.prime_request,"coordinator start already claimed; inspect before open --reprime");
    }
    Ok(())
}
fn arguments(input:&Input,p:&Project,kind:&str,text:Option<&str>)->Result<Vec<String>> {
    ensure!(digest(text)==input.config_digest,"coordinator launch configuration changed");
    let safety=project::parse_safety(text.unwrap_or(""),&p.canonical_dir()).map_err(|_|anyhow::anyhow!("invalid coordinator launch configuration (contents withheld)"))?;
    let args=safety.coordinator_arguments(kind)?.to_vec();
    ensure!(args.len()<=128&&args.iter().map(String::len).sum::<usize>()<=32768,"coordinator launch arguments exceed bounds");Ok(args)
}
pub(super) fn execute(input:&Input,control:&Control)->Result<()> {execute_with(input,control,||Ok(()))}
fn execute_with(input:&Input,control:&Control,after_claim:impl FnOnce()->Result<()>)->Result<()> {
    ensure!(input.project.is_absolute()&&input.config.is_absolute()&&input.socket.is_absolute()&&!input.herdr.is_empty(),"invalid coordinator start input");
    control.check()?;let guard=ProjectGuard::acquire(&input.project)?;let locks=guard.inherit_transfer()?;
    let p=Project::load(input.project.parent().context("coordinator root missing")?,input.project.file_name().and_then(|s|s.to_str()).context("invalid coordinator project")?)?;
    let(c,kind)=current(input,&p,&guard,control)?;ready(&c)?;ensure!(c.launch_sequence==input.sequence,"coordinator launch sequence changed");
    crate::brief_jobs::ownership::check_coordinator(&p,&c.pane_id,&input.socket,control)?;
    let h=crate::herdr::Herdr::new(&input.herdr,&input.socket,&crate::runner::RealRunner);
    let probe=herdr_projects::supervision::run(h.cmd(crate::herdr::CALL_TIMEOUT).args(["remote-api-bridge","--check"]),control.deadline,control.cancellation.clone(),&locks)?;
    control.check()?;ensure!(probe.success()&&probe.stdout.trim()=="herdr-api-bridge-v1","coordinator JSON API bridge unavailable");
    let agents:Vec<crate::herdr::Agent>=serde_json::from_value(run(h.cmd(crate::herdr::CALL_TIMEOUT).args(["agent","list"]),control,&locks)?["agents"].clone())?;
    ensure!(!agents.iter().any(|a|a.pane_id==c.pane_id),"coordinator pane already has an agent");
    let panes=run(h.cmd(crate::herdr::CALL_TIMEOUT).args(["pane","list"]),control,&locks)?;
    let panes=panes["panes"].as_array().context("coordinator pane inventory missing")?;
    let panes=panes.iter().filter(|v|v["pane_id"].as_str()==Some(&c.pane_id)).collect::<Vec<_>>();
    ensure!(panes.len()==1,"coordinator pane absent or ambiguous");let pane:crate::herdr::Pane=serde_json::from_value(panes[0].clone())?;
    ensure!(crate::coordinator::pane_matches(&c,&pane),"coordinator pane changed");
    let terminal=panes[0]["terminal_id"].as_str().context("coordinator terminal identity unavailable")?;
    let(c,_)=current(input,&p,&guard,control)?;ready(&c)?;
    let cfg=paths::read_control_text(&input.config.join("config.toml"),1024*1024)?;let args=arguments(input,&p,&kind,cfg.as_deref())?;
    let claim=LaunchClaim{sequence:c.launch_sequence.checked_add(1).context("coordinator launch sequence exhausted")?,generation:c.prime_request,execution:input.execution.clone(),arguments_digest:thread::sha256_hex(&serde_json::to_vec(&args)?),route_digest:thread::sha256_hex(&serde_json::to_vec(&(&input.socket,input.socket_identity))?),terminal:terminal.into(),phase:LaunchPhase::Pending,error:String::new(),notified:false};
    claim.validate(claim.sequence)?;
    p.update_coordinator_checked(|stored|{control.check()?;ensure!(stored==&c,"coordinator changed before launch claim");stored.launch_sequence=claim.sequence;stored.launch_claim=Some(claim.clone());stored.launch_attempts+=1;Ok(())})?;after_claim()?;
    let(stored,_)=current(input,&p,&guard,control)?;ensure!(stored.prime_pending&&stored.launch_claim.as_ref()==Some(&claim),"coordinator launch claim changed before send");
    let id=format!("coordinator-start-{}-{}",claim.generation,claim.sequence);
    let payload=serde_json::to_string(&serde_json::json!({"id":id,"method":"agent.start","params":{"name":c.agent_name,"kind":kind,"pane_id":c.pane_id,"args":args,"timeout_ms":20000}}))?+"\n";
    ensure!(payload.len()<=64*1024,"coordinator start frame exceeds bounds");
    let out=herdr_projects::supervision::run(h.cmd(crate::herdr::CALL_TIMEOUT).arg("remote-api-bridge").stdin(payload),control.deadline,control.cancellation.clone(),&locks)?;
    control.check()?;ensure!(out.success(),"coordinator start command failed");let reply:serde_json::Value=serde_json::from_str(&out.stdout)?;
    let result=&reply["result"];ensure!(reply["id"].as_str()==Some(&id)&&reply.get("error").is_none()&&result["type"].as_str()==Some("agent_started")&&result["agent"]["terminal_id"].as_str()==Some(&claim.terminal),"coordinator start acknowledgement mismatch");
    for (field,expected) in [("pane_id",&c.pane_id),("tab_id",&c.tab_id),("workspace_id",&c.workspace_id),("cwd",&c.cwd),("name",&c.agent_name)] {
        ensure!(result["agent"][field].as_str()==Some(expected.as_str()),"coordinator start acknowledgement names another execution");
    }
    ensure!(result["agent"]["agent"].as_str()==Some(&kind)||(result["agent"]["agent"].is_null()&&result["agent"]["launch_pending"].as_bool()==Some(true)),"coordinator start acknowledgement has foreign or unavailable kind");
    let argv:Vec<String>=serde_json::from_value(result["argv"].clone()).map_err(|_|anyhow::anyhow!("coordinator start acknowledgement lacks valid command vector (contents withheld)"))?;
    ensure!(argv.len()==args.len()+1&&!argv[0].is_empty()&&argv[0].len()<=4096&&!argv[0].chars().any(char::is_control)&&argv[1..]==args,"coordinator start acknowledgement arguments changed");
    let(stored,_)=current(input,&p,&guard,control)?;ensure!(stored.launch_claim.as_ref()==Some(&claim)&&stored.prime_pending,"coordinator launch claim changed after send");
    p.update_coordinator_checked(|stored|{control.check()?;ensure!(stored.launch_claim.as_ref()==Some(&claim)&&execution(stored)==input.execution&&stored.prime_pending,"coordinator launch confirmation authority changed");let mut confirmed=claim.clone();confirmed.phase=LaunchPhase::Confirmed;confirmed.notified=true;stored.launch_claim=Some(confirmed);Ok(())})?;Ok(())
}
pub(super) fn recover(p:&Project,guard:&ProjectGuard)->Result<()> {
    guard.check_project(&p.dir())?;let Some(c)=p.try_coordinator()? else{return Ok(());};super::validate(&c)?;
    let Some(mut claim)=c.launch_claim else{return Ok(());};
    if claim.phase==LaunchPhase::Pending {
        let pending=claim.clone();claim.phase=LaunchPhase::Uncertain;claim.error="Coordinator startup was interrupted and may already have been submitted. Inspect the pane before explicitly running open --reprime.".into();
        p.update_coordinator_checked(|c|{ensure!(c.launch_claim.as_ref()==Some(&pending),"coordinator launch claim changed during recovery");c.launch_claim=Some(claim.clone());Ok(())})?;
    }
    if claim.phase==LaunchPhase::Uncertain&&!claim.notified {
        let id=format!("coordinator-start-{}-{}",claim.generation,claim.sequence);
        crate::inbox::write_once(p,&id,"coordinator-state","coordinator","coordinator startup needs reconciliation",&claim.error)?;std::fs::File::open(p.dir().join("inbox"))?.sync_all()?;
        p.update_coordinator_checked(|c|{ensure!(c.launch_claim.as_ref()==Some(&claim),"coordinator launch claim changed before notice acknowledgement");c.launch_claim.as_mut().unwrap().notified=true;Ok(())})?;
    }Ok(())
}

#[cfg(all(test,target_os="linux"))]
#[path="coordinator_start_jobs_tests.rs"]
mod tests;

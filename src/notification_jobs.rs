//! Inbox delivery under coordinator ownership; queue results are never receipts.
use super::*;
use herdr_projects::notification_claim::{Batch,Claim as Notice,Mode,Phase as NoticePhase};
use crate::steps::{self,State};
const UNCERTAIN:&str="Inbox delivery may already have occurred. Inspect the coordinator and notification history, then use notification acknowledge or notification retry with the recorded sequence.";
fn validate(state:&State)->Result<()> {
    ensure!(state.notification_sequence<=i64::MAX as u64,"notification sequence exhausted");
    Batch::new(state.notification_suppressed.iter().cloned().collect())?;
    if let Some(claim)=&state.notification_claim{claim.validate(state.notification_sequence)?;}Ok(())
}
fn capture(p:&Project,state:&State,control:&Control)->Result<Batch> {
    let raw=crate::notification_inventory::capture(p,control)?;
    Batch::new(raw.ids.into_iter().filter(|id|!state.notification_suppressed.contains(id)).collect())
}
fn mode(input:&Input,p:&Project)->Result<Mode> {
    let text=paths::read_control_text(&p.project_md(),1024*1024)?.context("project settings missing")?;
    ensure!(thread::sha256_hex(text.as_bytes())==input.settings_digest,"notification settings changed");
    let(settings,_)=project::parse_project_md(&text).map_err(|_|anyhow::anyhow!("invalid notification settings (contents withheld)"))?;
    Ok(if settings.nudge{Mode::Nudge}else{Mode::Toast})
}
fn payload(p:&Project,c:&Coordinator,batch:&Batch,mode:Mode)->Result<String> {
    Ok(serde_json::to_string(&match mode {
        Mode::Nudge=>serde_json::json!({"target":c.pane_id,"text":steps::NUDGE_TEXT}),
        Mode::Toast=>serde_json::json!({"title":format!("herdr-projects: {}",p.slug),"body":format!("{} new inbox item(s). The coordinator reads them at its next turn.",batch.ids.len())}),
        Mode::Legacy=>anyhow::bail!("legacy uncertainty cannot dispatch"),
    })?)
}
fn authority(input:&Input,payload:&str)->Result<String>{Ok(thread::sha256_hex(&serde_json::to_vec(&(&input.project,input.identity,&input.socket,input.socket_identity,&input.herdr,&input.config,&input.execution,&input.config_digest,&input.settings_digest,&input.prompt,payload))?))}
fn save(p:&Project,state:&State)->Result<()> {validate(state)?;steps::save_state(p,state)}
pub(super) fn recover(p:&Project,guard:&ProjectGuard)->Result<()> {
    guard.check_project(&p.dir())?;let mut state=steps::try_load_state(p)?;validate(&state)?;
    if state.notification_claim.is_none()&&!state.notification_retry.hash.is_empty()&&state.notification_retry.retry.attempts>0 {
        let sequence=state.notification_sequence.checked_add(1).context("notification sequence exhausted")?;
        state.notification_sequence=sequence;state.notification_claim=Some(Notice{sequence,batch:None,mode:Mode::Legacy,authority:String::new(),payload:String::new(),phase:NoticePhase::Uncertain,retry_of:None,error:UNCERTAIN.into()});
        state.notification_retry.retry.blocked=true;state.notification_retry.retry.last_error=UNCERTAIN.into();save(p,&state)?;
    }
    if state.notification_claim.as_ref().is_some_and(|c|c.phase==NoticePhase::Pending) {
        let claim=state.notification_claim.as_mut().unwrap();claim.phase=NoticePhase::Uncertain;claim.error=UNCERTAIN.into();
        state.notification_retry.retry.blocked=true;state.notification_retry.retry.last_error=UNCERTAIN.into();save(p,&state)?;
    }Ok(())
}
fn primed(c:&Coordinator)->Result<()> {
    super::validate(c)?;ensure!(!c.prime_pending,"coordinator priming is pending");
    ensure!(c.launch_claim.as_ref().is_none_or(|claim|claim.phase!=herdr_projects::launch_claim::Phase::Pending&&(claim.phase!=herdr_projects::launch_claim::Phase::Uncertain||claim.generation!=c.prime_request))
        &&c.prime_claim.as_ref().is_none_or(|claim|claim.delivery.phase!=Phase::Pending&&(claim.delivery.phase!=Phase::Uncertain||claim.request!=c.prime_request)),"coordinator needs reconciliation before nudging");Ok(())
}
pub(super) fn execute(input:&Input,control:&Control)->Result<()> {execute_with(input,control,||Ok(()))}
fn execute_with(input:&Input,control:&Control,after_claim:impl FnOnce()->Result<()>)->Result<()> {
    ensure!(input.notification&&!input.start&&input.project.is_absolute()&&input.config.is_absolute()&&input.socket.is_absolute(),"invalid notification worker input");control.check()?;
    let guard=ProjectGuard::acquire(&input.project)?;let locks=guard.inherit_transfer()?;
    let p=Project::load(input.project.parent().context("notification root missing")?,input.project.file_name().and_then(|s|s.to_str()).context("invalid notification project")?)?;
    recover(&p,&guard)?;let(c,kind)=current(input,&p,&guard,control)?;let mode=mode(input,&p)?;
    let mut state=steps::try_load_state(&p)?;validate(&state)?;
    let raw=crate::notification_inventory::capture(&p,control)?;
    let retained=state.notification_suppressed.iter().filter(|id|raw.ids.binary_search(id).is_ok()).cloned().collect();
    if state.notification_suppressed!=retained{state.notification_suppressed=retained;save(&p,&state)?;}
    let batch=Batch::new(raw.ids.into_iter().filter(|id|!state.notification_suppressed.contains(id)).collect())?;
    if let Some(claim)=state.notification_claim.as_ref().filter(|c|c.phase==NoticePhase::Uncertain) {
        let consumed=if let Some(old)=&claim.batch{crate::notification_inventory::consumed(&p,&old.ids,control)?}else{false};
        ensure!(consumed,"{UNCERTAIN}");
        let claim=state.notification_claim.as_mut().unwrap();claim.phase=NoticePhase::Suppressed;claim.error="Claimed inbox items were consumed; no retry was sent.".into();state.nudged=claim.batch.as_ref().unwrap().hash.clone();state.notification_retry=Default::default();save(&p,&state)?;
    }
    let retry=state.notification_claim.as_ref().is_some_and(|c|c.phase==NoticePhase::Ready);
    if batch.ids.is_empty(){return Ok(());}
    if !retry&&state.nudged==batch.hash{return Ok(());}
    if !retry&&!state.notification_retry.retry.due(jiff::Timestamp::now()){return Ok(());}
    let payload=payload(&p,&c,&batch,mode)?;let authority=authority(input,&payload)?;
    if retry {let approved=state.notification_claim.as_ref().unwrap();ensure!(approved.batch.as_ref()==Some(&batch)&&approved.mode==mode&&approved.payload==payload&&approved.authority==authority,"approved notification retry inputs changed; reconcile again");}
    let h=crate::herdr::Herdr::new(&input.herdr,&input.socket,&crate::runner::RealRunner);
    let probe=herdr_projects::supervision::run(h.cmd(crate::herdr::CALL_TIMEOUT).args(["remote-api-bridge","--check"]),control.deadline,control.cancellation.clone(),&locks)?;control.check()?;
    ensure!(probe.success()&&probe.stdout.trim()=="herdr-api-bridge-v1","notification JSON API bridge unavailable");
    let mut terminal=None;
    if mode==Mode::Nudge {
        primed(&c)?;crate::brief_jobs::ownership::check_coordinator(&p,&c.pane_id,&input.socket,control)?;
        let agents:Vec<crate::herdr::Agent>=serde_json::from_value(run(h.cmd(crate::herdr::CALL_TIMEOUT).args(["agent","list"]),control,&locks)?["agents"].clone())?;
        let matches=agents.iter().filter(|a|a.pane_id==c.pane_id).collect::<Vec<_>>();ensure!(matches.len()==1&&crate::coordinator::agent_matches(&c,matches[0])&&matches[0].agent==kind&&matches[0].ready(),"notification coordinator is absent, ambiguous, changed or busy");
        let panes=run(h.cmd(crate::herdr::CALL_TIMEOUT).args(["pane","list"]),control,&locks)?;let panes=panes["panes"].as_array().context("notification pane inventory missing")?;
        let panes=panes.iter().filter(|v|v["pane_id"].as_str()==Some(&c.pane_id)).collect::<Vec<_>>();ensure!(panes.len()==1,"notification pane is absent or ambiguous");
        let pane:crate::herdr::Pane=serde_json::from_value(panes[0].clone())?;ensure!(crate::coordinator::pane_matches(&c,&pane),"notification pane changed");
        terminal=Some(panes[0]["terminal_id"].as_str().filter(|s|!s.is_empty()&&s.len()<=256).context("notification terminal identity unavailable")?.to_string());
    }
    let(fresh,_)=current(input,&p,&guard,control)?;if mode==Mode::Nudge{primed(&fresh)?;}
    ensure!(capture(&p,&state,control)?==batch,"notification inbox changed before claim");
    let mut fresh=steps::try_load_state(&p)?;ensure!(fresh.notification_claim==state.notification_claim&&fresh.notification_sequence==state.notification_sequence&&fresh.notification_suppressed==state.notification_suppressed&&fresh.nudged==state.nudged,"notification authority changed before claim");
    let claim=if retry {let mut claim=fresh.notification_claim.clone().unwrap();claim.phase=NoticePhase::Pending;claim}
        else {Notice{sequence:fresh.notification_sequence.checked_add(1).context("notification sequence exhausted")?,batch:Some(batch.clone()),mode,authority,payload,phase:NoticePhase::Pending,retry_of:fresh.notification_claim.as_ref().filter(|c|c.phase==NoticePhase::NotShown).map(|c|c.sequence),error:String::new()}};
    fresh.notification_sequence=claim.sequence;fresh.notification_claim=Some(claim.clone());fresh.notification_retry.hash=batch.hash.clone();fresh.notification_retry.retry.blocked=false;fresh.notification_retry.retry.reserve(jiff::Timestamp::now(),&batch.hash);fresh.notification_retry.retry.last_error="notification submission claimed; outcome pending".into();save(&p,&fresh)?;after_claim()?;
    let(fresh_c,_)=current(input,&p,&guard,control)?;if mode==Mode::Nudge{primed(&fresh_c)?;}
    ensure!(steps::try_load_state(&p)?.notification_claim.as_ref()==Some(&claim)&&capture(&p,&fresh,control)?==batch,"notification claim or inbox changed before send");
    let id=format!("notification-{}",claim.sequence);let params:serde_json::Value=serde_json::from_str(&claim.payload)?;
    let frame=serde_json::to_string(&serde_json::json!({"id":id,"method":if mode==Mode::Nudge{"agent.prompt"}else{"notification.show"},"params":params}))?+"\n";ensure!(frame.len()<=64*1024,"notification frame exceeds bounds");
    let out=herdr_projects::supervision::run(h.cmd(crate::herdr::CALL_TIMEOUT).arg("remote-api-bridge").stdin(frame),control.deadline,control.cancellation.clone(),&locks)?;control.check()?;ensure!(out.success(),"notification command failed");
    let reply:serde_json::Value=serde_json::from_str(&out.stdout)?;ensure!(reply["id"].as_str()==Some(&id)&&reply.get("error").is_none(),"notification acknowledgement mismatch");let result=&reply["result"];
    let not_shown=if mode==Mode::Nudge {
        ensure!(result["type"].as_str()==Some("agent_prompted")&&result["agent"]["terminal_id"].as_str()==terminal.as_deref(),"notification prompt acknowledgement mismatch");let agent:crate::herdr::Agent=serde_json::from_value(result["agent"].clone())?;ensure!(crate::coordinator::agent_matches(&c,&agent)&&agent.agent==kind,"notification prompt acknowledged another agent");None
    }else {
        ensure!(result["type"].as_str()==Some("notification_show"),"notification toast acknowledgement mismatch");
        match(result["shown"].as_bool(),result["reason"].as_str()) {
            (Some(true),Some("shown"))=>None,
            (Some(false),Some(reason @ ("disabled"|"rate_limited"|"no_foreground_client"|"busy")))=>Some(reason.to_string()),
            _=>anyhow::bail!("notification toast outcome is inconsistent"),
        }
    };
    current(input,&p,&guard,control)?;let mut state=steps::try_load_state(&p)?;ensure!(state.notification_claim.as_ref()==Some(&claim),"notification claim changed before receipt");
    let outcome=state.notification_claim.as_mut().unwrap();if let Some(reason)=not_shown {outcome.phase=NoticePhase::NotShown;outcome.error=format!("Native toast not shown: {reason}");state.notification_retry.retry.last_error=outcome.error.clone();}
    else {outcome.phase=NoticePhase::Confirmed;state.nudged=batch.hash;state.notification_retry=Default::default();}
    control.check()?;save(&p,&state)
}
pub(super) fn inspect(p:&Project)->Result<serde_json::Value>{let state=steps::try_load_state(p)?;validate(&state)?;Ok(serde_json::json!({"sequence":state.notification_sequence,"claim":state.notification_claim,"retry":state.notification_retry,"suppressed_item_ids":state.notification_suppressed}))}
pub(super) fn reconcile(ctx:&Ctx,p:&Project,sequence:u64,retry:bool)->Result<()> {
    let guard=ProjectGuard::acquire(&p.dir())?;recover(p,&guard)?;let mut state=steps::try_load_state(p)?;validate(&state)?;
    let previous=state.notification_claim.clone().context("no notification claim to reconcile")?;ensure!(previous.sequence==sequence,"notification sequence changed; inspect again");
    ensure!(matches!(previous.phase,NoticePhase::Uncertain|NoticePhase::Ready|NoticePhase::NotShown),"notification claim does not need reconciliation");
    if retry {
        let c=p.try_coordinator()?.context("coordinator record missing")?;let work=super::request_notification(ctx,p,&c)?;let input:Input=serde_json::from_str(work.command.stdin.as_ref().unwrap())?;
        current(&input,p,&guard,&Control::default())?;let batch=capture(p,&state,&Control::default())?;ensure!(!batch.ids.is_empty(),"no unseen inbox items to retry");
        let mode=mode(&input,p)?;let payload=payload(p,&c,&batch,mode)?;
        let next=state.notification_sequence.checked_add(1).context("notification sequence exhausted")?;
        state.notification_claim=Some(Notice{sequence:next,batch:Some(batch),mode,authority:authority(&input,&payload)?,payload,phase:NoticePhase::Ready,retry_of:Some(sequence),error:String::new()});state.notification_sequence=next;
    }else {
        let batch=crate::notification_inventory::capture(p,&Control::default())?;
        state.notification_suppressed.retain(|id|batch.ids.binary_search(id).is_ok());
        let acknowledged=previous.batch.as_ref().map_or_else(||batch.ids.clone(),|b|b.ids.iter().filter(|id|batch.ids.binary_search(id).is_ok()).cloned().collect());
        state.notification_suppressed.extend(acknowledged);
        let claim=state.notification_claim.as_mut().unwrap();claim.phase=NoticePhase::Suppressed;claim.error="Explicitly acknowledged without another delivery.".into();
    }
    state.notification_retry=Default::default();save(p,&state)
}

#[cfg(all(test,target_os="linux"))]
#[path="notification_jobs_tests.rs"]
mod tests;

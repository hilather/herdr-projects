//! Frozen saved-machine routing for Herdr's non-bootstrapping JSON API bridge.
use anyhow::{Result,ensure,Context};
use serde::{Serialize,Deserialize};
use crate::{runner::{Cmd,Output,InheritedLock},source_tree::Control,remote};
use std::time::Duration;
const LIMIT:usize=1024*1024;
#[derive(Debug,Clone,Serialize,Deserialize,PartialEq,Eq)]
#[serde(deny_unknown_fields)]
pub struct Route {pub id:String,pub label:String,pub target:String,pub session:String,pub enabled:bool,#[serde(default)]pub selected:bool}
impl Route {
    pub fn validate(&self)->Result<()> {
        ensure!(self.id.len()==32&&self.id.bytes().all(|b|b.is_ascii_digit()||(b'a'..=b'f').contains(&b)),"invalid saved-machine profile ID");
        ensure!(!self.label.trim().is_empty()&&self.label.len()<=128&&!self.label.chars().any(char::is_control),"invalid saved-machine label");
        ensure!(!self.session.is_empty()&&self.session.len()<=64&&!matches!(self.session.as_str(),"."|"..")&&self.session.bytes().all(|b|b.is_ascii_alphanumeric()||matches!(b,b'.'|b'_'|b'-')),"invalid saved remote session");
        ensure!(self.target.len()<=1024,"saved SSH target exceeds bounds");
        ensure!(self.target.strip_prefix("ssh://").unwrap_or(&self.target).rsplit_once('@').is_none_or(|(user,_)|!user.contains(':')),"saved SSH target contains a password");remote::ssh_command(&self.target,"true",Duration::from_secs(1))?;Ok(())
    }
    /// Selected is UI state, never execution authority. Labels may be renamed;
    /// a changed host, session, ID or enabled flag withdraws the frozen route.
    pub fn same_destination(&self,other:&Self)->bool {self.id==other.id&&self.target==other.target&&self.session==other.session&&self.enabled&&other.enabled}
    fn command(&self,binary:&str,probe:bool)->Result<Cmd> {
        self.validate()?;ensure!(self.enabled,"saved machine is disabled");
        ensure!(!binary.is_empty()&&!binary.starts_with('-')&&binary.len()<=4096&&!binary.chars().any(char::is_control),"invalid remote Herdr executable");
        // Always name the session, including `default`, and clear inherited
        // selectors. Input JSON stays on stdin, outside the SSH command string.
        let script=format!("unset HERDR_SESSION HERDR_SOCKET_PATH; exec {} --session {} remote-api-bridge{}",remote::quote(binary),remote::quote(&self.session),if probe{" --check"}else{""});
        let mut command=remote::ssh_command(&self.target,&script,remote::SSH_TIMEOUT)?;
        command.args.splice(0..0,["-T".into(),"-o".into(),"StrictHostKeyChecking=yes".into()]);
        Ok(command)
    }
}
pub fn resolve(output:&Output,selector:&str)->Result<Route> {
    ensure!(output.success()&&output.stdout.len()<=LIMIT,"saved-machine listing unavailable or oversized");
    let routes:Vec<Route>=serde_json::from_str(&output.stdout).context("saved-machine listing contract changed")?;
    ensure!(routes.len()<=64,"saved-machine inventory exceeds 64 profiles");let mut ids=std::collections::BTreeSet::new();
    for route in &routes {route.validate()?;ensure!(ids.insert(&route.id),"duplicate saved-machine profile ID");}
    let found=if let Some(route)=routes.iter().find(|r|r.id==selector){route}else{
        let mut matches=routes.iter().filter(|r|r.label==selector);let route=matches.next().context("saved machine not found")?;ensure!(matches.next().is_none(),"ambiguous machine label; use its profile ID");route
    };
    ensure!(found.enabled,"saved machine is disabled");Ok(found.clone())
}
fn execute(mut command:Cmd,control:&Control,locks:&[InheritedLock])->Result<Output> {
    control.check()?;command.capture_limit=LIMIT;
    let output=herdr_projects::supervision::run(command,control.deadline,control.cancellation.clone(),locks)?;
    control.check()?;ensure!(output.success(),"remote API bridge command failed");Ok(output)
}
pub fn probe(route:&Route,binary:&str,control:&Control,locks:&[InheritedLock])->Result<()> {
    let output=execute(route.command(binary,true)?,control,locks)?;
    ensure!(output.stdout.trim()=="herdr-api-bridge-v1","remote binary lacks JSON API bridge v1");Ok(())
}
/// The caller must own effects and durably claim non-idempotent submissions.
/// This transport performs one request and never retries or changes destination.
pub fn request(route:&Route,binary:&str,id:&str,method:&str,params:serde_json::Value,control:&Control,locks:&[InheritedLock])->Result<serde_json::Value> {
    ensure!(!id.is_empty()&&id.len()<=256&&!id.chars().any(char::is_control),"invalid remote API request identity");
    ensure!(matches!(method,"ping"|"agent.list"|"pane.list"|"agent.prompt"|"agent.start"|"pane.report_metadata"),"unsupported remote worker method");
    let mut payload=serde_json::to_string(&serde_json::json!({"id":id,"method":method,"params":params}))?;ensure!(payload.len()<=64*1024,"remote API input exceeds bounds");payload.push('\n');
    send(route.command(binary,false)?.stdin(payload),id,control,locks)
}
fn send(command:Cmd,id:&str,control:&Control,locks:&[InheritedLock])->Result<serde_json::Value> {
    let output=execute(command,control,locks)?;
    let reply:serde_json::Value=serde_json::from_str(&output.stdout).context("invalid remote API reply")?;
    ensure!(reply["id"].as_str()==Some(id)&&reply.get("error").is_none(),"remote API acknowledgement mismatch or rejection");
    reply.get("result").cloned().context("remote API reply has no result")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn route()->Route {Route{id:"a".repeat(32),label:"fixture".into(),target:"fixture.invalid".into(),session:"named-session".into(),enabled:true,selected:false}}
    fn output(routes:&[Route])->Output {Output{code:Some(0),stdout:serde_json::to_string(routes).unwrap(),..Default::default()}}
    #[test]
    fn frozen_route_uses_exact_id_precedence_and_separates_ui_from_authority() {
        let r=route();let mut shadow=r.clone();shadow.id="b".repeat(32);shadow.label=r.id.clone();shadow.target="wrong.invalid".into();
        assert_eq!(resolve(&output(&[shadow,r.clone()]),&r.id).unwrap(),r);
        let mut changed=r.clone();changed.selected=true;changed.label="renamed".into();assert!(r.same_destination(&changed));
        changed.session="other".into();assert!(!r.same_destination(&changed));changed=r.clone();changed.enabled=false;assert!(!r.same_destination(&changed));
    }
    #[test]
    fn unavailable_ambiguous_disabled_and_malformed_inventory_refuse() {
        let r=route();let mut other=r.clone();other.id="b".repeat(32);
        assert!(resolve(&output(&[r.clone(),other]),&r.label).is_err());assert!(resolve(&output(&[r.clone(),r.clone()]),&r.id).is_err());
        let mut disabled=r.clone();disabled.enabled=false;assert!(resolve(&output(&[disabled]),&r.id).is_err());
        let mut changed=r.clone();changed.session="../another".into();assert!(resolve(&output(&[changed]),&r.id).is_err());
        let mut failed=output(&[r.clone()]);failed.code=Some(1);assert!(resolve(&failed,&r.id).is_err());
        let old=Output{code:Some(0),stdout:r#"[{"id":"a","label":"fixture","target":"host"}]"#.into(),..Default::default()};assert!(resolve(&old,"fixture").is_err());
    }
    #[test]
    fn bridge_command_freezes_host_session_and_quotes_remote_binary() {
        let r=route();let cmd=r.command("/opt/Herdr's bin/herdr",false).unwrap();
        assert_eq!(cmd.program,"ssh");assert_eq!(&cmd.args[..3],["-T","-o","StrictHostKeyChecking=yes"]);assert_eq!(cmd.args[8],r.target);assert!(cmd.args[9].contains("remote-api-bridge"));assert!(cmd.args[9].contains("named-session"));assert!(cmd.stdin.is_none());
        assert!(!cmd.args.iter().any(|a|a=="--machine"));assert!(r.command("--malicious",false).is_err());
        let mut default=r;default.session="default".into();assert!(default.command("herdr",true).unwrap().args.last().unwrap().contains("--session default remote-api-bridge --check"));
    }
    #[cfg(target_os="linux")]
    #[test]
    fn concrete_transport_keeps_json_literal_and_rejects_lost_or_wrong_acknowledgement() {
        use std::{fs,os::unix::fs::PermissionsExt};
        let root=tempfile::tempdir().unwrap();let project=root.path().join("project");fs::create_dir_all(project.join(".state")).unwrap();let guard=herdr_projects::execution_guard::ProjectGuard::acquire(&project).unwrap();let locks=guard.inherit_transfer().unwrap();
        let helper=root.path().join("ssh");let captured=root.path().join("input");let text=format!("brief '$() `touch {}`\nnext line",root.path().join("BAD").display());let payload=serde_json::json!({"id":"delivery-1","method":"agent.prompt","params":{"target":"pane","text":text}}).to_string()+"\n";
        for mode in ["ok","wrong","lost"] {
            let reply=serde_json::json!({"id":if mode=="wrong"{"other"}else{"delivery-1"},"result":{"type":"agent_prompted"}}).to_string();
            let body=format!("#!/bin/sh\n/bin/cat > {}\nprintf '%s\\n' {}\nexit {}\n",remote::quote(captured.to_str().unwrap()),remote::quote(&reply),if mode=="lost"{1}else{0});fs::write(&helper,body).unwrap();fs::set_permissions(&helper,fs::Permissions::from_mode(0o700)).unwrap();
            let mut command=route().command("herdr",false).unwrap().stdin(payload.clone());assert!(!command.args.iter().any(|a|a.contains(&text)));command.program=helper.display().to_string();
            let result=send(command,"delivery-1",&Control::default(),&locks);assert_eq!(result.is_ok(),mode=="ok");assert_eq!(fs::read_to_string(&captured).unwrap(),payload);assert!(!root.path().join("BAD").exists());
        }
        fs::remove_file(&captured).unwrap();let control=Control::default();control.cancellation.cancel();let mut command=route().command("herdr",false).unwrap().stdin(payload);command.program=helper.display().to_string();assert!(send(command,"delivery-1",&control,&locks).is_err());assert!(!captured.exists());
    }
}

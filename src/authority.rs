//! Signed owner-control ingress. Documents cannot declare their own trusted role.
use std::{fs::{self, OpenOptions}, io::Write, os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt}, path::{Path,PathBuf}, time::Duration};
use anyhow::{Result, Context, ensure};
use serde::{Deserialize,Serialize};
use sha2::{Digest,Sha256};
use crate::{domain::{ApprovalGrant,PreparedApproval,VersionedReference}, migration, runner::{Cmd,Runner,RealRunner}};

pub const SIGNATURE_NAMESPACE: &str = "approval@herdr-projects";
pub const BUDGET_SIGNATURE_NAMESPACE: &str = "budget@herdr-projects";
pub const ROUTINE_SIGNATURE_NAMESPACE: &str = "routine@herdr-projects";

#[derive(Deserialize,Serialize)]
#[serde(deny_unknown_fields)]
struct Policy { version:u32, revision:u64, approval_public_key:String }

impl Policy {
    fn reference(&self)->Result<VersionedReference> {
        ensure!(self.version==1 && self.revision>0 && self.revision<=i64::MAX as u64,"unsupported authority policy version or revision");
        let parts=self.approval_public_key.split(' ').collect::<Vec<_>>();
        ensure!(parts.len()==2 && parts[0]=="ssh-ed25519" && (32..=256).contains(&parts[1].len())
            && parts[1].bytes().all(|b|b.is_ascii_alphanumeric()||b"+/=".contains(&b)),"authority requires one Ed25519 public key without comments or options");
        Ok(VersionedReference{id:"owner-approval-policy".into(),revision:self.revision,digest:format!("{:x}",Sha256::digest(serde_json::to_vec(self)?))})
    }
}

struct VerificationFiles(PathBuf);
impl VerificationFiles {
    fn new()->Result<Self> {
        static NEXT:std::sync::atomic::AtomicU64=std::sync::atomic::AtomicU64::new(0);
        for _ in 0..128 {
            let serial=NEXT.fetch_add(1,std::sync::atomic::Ordering::Relaxed);
            let stamp=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos();
            let path=PathBuf::from("/tmp").join(format!("herdr-approval-{}-{stamp}-{serial}",std::process::id()));
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(())=>return Ok(Self(path)),
                Err(e) if e.kind()==std::io::ErrorKind::AlreadyExists=>continue,
                Err(_)=>anyhow::bail!("cannot create private signature verification directory"),
            }
        }
        anyhow::bail!("cannot allocate signature verification directory")
    }
    fn write(&self,name:&str,bytes:&[u8])->Result<PathBuf> {
        let path=self.0.join(name);
        OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?.write_all(bytes)?;
        Ok(path)
    }
}
impl Drop for VerificationFiles {fn drop(&mut self){let _=fs::remove_dir_all(&self.0);}}

fn verify(policy:&Policy,payload:&[u8],signature:&[u8],runner:&dyn Runner)->Result<PreparedApproval> {
    let reference=policy.reference()?;
    ensure!(payload.len()<=65_536 && signature.len()<=8192,"approval document or signature exceeds bounds");
    let grant:ApprovalGrant=serde_json::from_slice(payload).map_err(|_|anyhow::anyhow!("invalid approval document (contents withheld)"))?;
    grant.validate().map_err(|_|anyhow::anyhow!("invalid approval grant"))?;
    ensure!(grant.policy==reference,"approval names a different authority policy");
    verify_signature(policy,payload,signature,SIGNATURE_NAMESPACE,runner)?;
    Ok(PreparedApproval{grant})
}

fn verify_signature(policy:&Policy,payload:&[u8],signature:&[u8],namespace:&str,runner:&dyn Runner)->Result<()> {
    policy.reference()?;
    ensure!(payload.len()<=65_536 && signature.len()<=8192,"signed document or signature exceeds bounds");
    let files=VerificationFiles::new()?;
    let allowed=files.write("allowed_signers",format!("owner {}\n",policy.approval_public_key).as_bytes())?;
    let sig=files.write("signature",signature)?;
    let mut command=Cmd::new("/usr/bin/ssh-keygen",Duration::from_secs(5))
        .args(["-Y","verify","-f"]).arg(allowed.to_str().context("invalid verification path")?)
        .args(["-I","owner","-n",namespace,"-s"]).arg(sig.to_str().context("invalid verification path")?)
        .stdin(std::str::from_utf8(payload).map_err(|_|anyhow::anyhow!("approval must be UTF-8"))?);
    command.capture_limit=4096;
    let result=runner.run(&command).map_err(|_|anyhow::anyhow!("approval signature verification failed"))?;
    ensure!(result.success()&&!result.stdout_truncated&&!result.stderr_truncated,"approval signature verification failed");
    Ok(())
}

fn policy(project:&Path)->Result<(Policy,migration::ConfigReference)> {
    let journal=migration::status(project)?;
    let original=journal.plan.config.context("migration has no pinned owner config path")?;
    policy_at(project,&original)
}

fn policy_at(project:&Path,original:&migration::ConfigReference)->Result<(Policy,migration::ConfigReference)> {
    let path=Path::new(&original.path);
    let canonical=fs::canonicalize(path).map_err(|_|anyhow::anyhow!("owner configuration unavailable"))?;
    ensure!(!canonical.starts_with(project.canonicalize()?),"authority configuration must be outside the project");
    let metadata=fs::metadata(&canonical)?;
    ensure!(metadata.uid()==unsafe{libc::geteuid()} && metadata.mode()&0o022==0,"owner configuration must be owned by this user and not group/world writable");
    let reference=migration::config_reference(path)?;
    let bytes=migration::read_plan_file(path).map_err(|_|anyhow::anyhow!("owner configuration unreadable"))?;
    ensure!(bytes.len()<=1_048_576 && reference.digest.as_deref()==Some(format!("{:x}",Sha256::digest(&bytes)).as_str()),"owner configuration changed or exceeds bounds");
    let text=std::str::from_utf8(&bytes).map_err(|_|anyhow::anyhow!("invalid owner configuration"))?;
    let value:toml::Value=toml::from_str(text).map_err(|_|anyhow::anyhow!("invalid owner configuration (contents withheld)"))?;
    let policy:Policy=value.get("authority").context("owner configuration has no authority policy")?.clone().try_into()
        .map_err(|_|anyhow::anyhow!("invalid authority policy (contents withheld)"))?;
    policy.reference()?;
    Ok((policy,reference))
}

/// Report the pinned owner policy identity for signing; never grants approval.
pub fn policy_reference(project:&Path)->Result<VersionedReference> {policy(project)?.0.reference()}
pub(crate) fn routine_policy(project:&Path)->Result<(VersionedReference,migration::ConfigReference)> {
    let (policy,config)=policy(project)?;Ok((policy.reference()?,config))
}

/// Verify an owner signature against pinned configuration and install the grant.
/// No caller-supplied actor, key, config path, verifier or clock can authorize it.
pub fn import_signed(project:&Path,document:&Path,signature:&Path,expected_head:u64)->Result<VersionedReference> {
    let _guard=migration::runtime_mutation(project)?;
    let mut db=migration::open_active(project)?;
    let snapshot=db.read_snapshot(Some(expected_head))?;
    let (policy,config)=policy(project)?;
    let control=snapshot.control.context("control schema upgrade required")?;
    ensure!(control.config_digest==config.digest,"owner configuration must be acknowledged by project control first");
    let payload=migration::read_plan_file(document).map_err(|_|anyhow::anyhow!("approval document unreadable"))?;
    let signature=migration::read_plan_file(signature).map_err(|_|anyhow::anyhow!("approval signature unreadable"))?;
    let prepared=verify(&policy,&payload,&signature,&RealRunner)?;
    ensure!(migration::config_reference(Path::new(&config.path))?==config,"owner configuration changed during verification");
    Ok(db.install_approval(&prepared,expected_head,jiff::Timestamp::now().as_millisecond())?)
}

pub fn revoke(project:&Path,id:&str,head:u64,reason:&str)->Result<u64> {
    let _guard=migration::runtime_mutation(project)?;
    Ok(migration::open_active(project)?.revoke_approval(id,head,jiff::Timestamp::now().as_millisecond(),reason)?)
}

/// Change admission policy only through an exact owner-signed, revision-bound document.
pub fn import_budget(project:&Path,document:&Path,signature:&Path,expected_head:u64)->Result<VersionedReference> {
    let _guard=migration::runtime_mutation(project)?;
    let mut db=migration::open_active(project)?;
    let snapshot=db.read_snapshot(Some(expected_head))?;
    let (owner,config)=policy(project)?;
    ensure!(snapshot.control.context("control schema upgrade required")?.config_digest==config.digest,
        "owner configuration is not acknowledged by project control");
    let bytes=migration::read_plan_file(document)?;
    ensure!(bytes.len()<=65_536,"budget document exceeds bounds");
    let policy:crate::domain::BudgetPolicy=serde_json::from_slice(&bytes).map_err(|_|anyhow::anyhow!("invalid budget document (contents withheld)"))?;
    policy.validate().map_err(anyhow::Error::msg)?;
    ensure!(policy.authority==owner.reference()?,"budget names a different authority policy");
    let signature=migration::read_plan_file(signature)?;
    verify_signature(&owner,&bytes,&signature,BUDGET_SIGNATURE_NAMESPACE,&RealRunner)?;
    ensure!(migration::config_reference(Path::new(&config.path))?==config,"owner configuration changed during verification");
    Ok(db.install_budget(&crate::domain::PreparedBudget{policy},expected_head)?)
}

pub fn import_routine(project:&Path,document:&Path,signature:&Path,expected_head:u64)->Result<VersionedReference> {
    let _guard=migration::runtime_mutation(project)?;
    let mut db=migration::open_active(project)?;
    let snapshot=db.read_snapshot(Some(expected_head))?;
    let(owner,config)=policy(project)?;
    ensure!(snapshot.control.context("control schema upgrade required")?.config_digest==config.digest,"owner configuration is not acknowledged by project control");
    let bytes=migration::read_plan_file(document)?;ensure!(bytes.len()<=65_536,"routine document exceeds bounds");
    let definition:crate::domain::RoutineDefinition=serde_json::from_slice(&bytes).map_err(|_|anyhow::anyhow!("invalid routine document (contents withheld)"))?;
    definition.validate().map_err(anyhow::Error::msg)?;
    ensure!(definition.project_store==project.join(".state/state.db").canonicalize()?.to_string_lossy(),"routine belongs to another project");
    ensure!(definition.authority==owner.reference()?&&definition.config==config,"routine names another owner policy or configuration");
    let signature=migration::read_plan_file(signature)?;
    verify_signature(&owner,&bytes,&signature,ROUTINE_SIGNATURE_NAMESPACE,&RealRunner)?;
    crate::routines::validate_current(&definition)?;
    Ok(db.install_routine(&crate::domain::PreparedRoutine{definition},expected_head)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ApprovalScope,ApprovalClass,TaskId,ProjectState};

    fn key(dir:&Path,name:&str)->(PathBuf,Policy) {
        let path=dir.join(name);
        let out=RealRunner.run(&Cmd::new("/usr/bin/ssh-keygen",Duration::from_secs(5)).args(["-q","-t","ed25519","-N","","-f"]).arg(path.to_str().unwrap())).unwrap();
        assert!(out.success());
        let text=fs::read_to_string(path.with_extension("pub")).unwrap();
        let public=text.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        (path,Policy{version:1,revision:1,approval_public_key:public})
    }
    fn sign(key:&Path,payload:&[u8],namespace:&str)->Vec<u8> {
        let out=RealRunner.run(&Cmd::new("/usr/bin/ssh-keygen",Duration::from_secs(5)).args(["-Y","sign","-f"]).arg(key.to_str().unwrap()).args(["-n",namespace]).stdin(std::str::from_utf8(payload).unwrap())).unwrap();
        assert!(out.success());out.stdout_bytes
    }
    fn grant(policy:&Policy)->ApprovalGrant {
        let now=jiff::Timestamp::now().as_millisecond();
        ApprovalGrant{version:1,scope:ApprovalScope{version:1,class:ApprovalClass::RuntimeLaunch,project_store:"/fixture/state.db".into(),task:TaskId::new("task").unwrap(),task_revision:2,target:"task:task".into(),action_digest:"a".repeat(64)},policy:policy.reference().unwrap(),issued_unix_ms:now-1000,expires_unix_ms:now+60_000}
    }
    #[test]
    fn signed_routines_require_enabled_config_and_recheck_script_before_effect() {
        use crate::domain::*;
        for enabled in [false,true] {
            let dir=tempfile::tempdir().unwrap();let(key,owner)=key(dir.path(),"owner");
            let project=dir.path().join("project");fs::create_dir(&project).unwrap();
            for child in [".state","threads","inbox"] {fs::create_dir(project.join(child)).unwrap();}
            fs::write(project.join("PROJECT.md"),"+++\nname='Project'\n+++\n").unwrap();
            fs::write(project.join("TASKS.md"),"").unwrap();fs::write(project.join("MEMORY.md"),"").unwrap();
            fs::write(project.join(".state/project.json"),r#"{"status":"paused"}"#).unwrap();
            let config=dir.path().join("owner.toml");
            let config_text=format!("[authority]\nversion=1\nrevision=1\napproval_public_key={:?}\n[safety.{:?}]\nroutine_commands={enabled}\n",owner.approval_public_key,project.display().to_string());
            fs::write(&config,&config_text).unwrap();
            let plan=migration::inspect_with_config(&project,&config).unwrap();migration::apply(&project,&plan,true).unwrap();
            let before=crate::runtime::snapshot(&project).unwrap();
            crate::runtime::set_state(&project,before.head,before.control.unwrap().revision,ProjectState::Active,&config).unwrap();
            let script=project.join("check.sh");let script_bytes=b"echo never executed\n";fs::write(&script,script_bytes).unwrap();
            let mut definition=RoutineDefinition{version:1,name:"check".into(),revision:1,project_store:project.join(".state/state.db").canonicalize().unwrap().display().to_string(),
                authority:owner.reference().unwrap(),config:migration::config_reference(&config).unwrap(),enabled:true,schedule:"every 1m".into(),timezone:"UTC".into(),start_unix_ms:0,
                missed:MissedRunPolicy::CoalesceLatest,overlap:OverlapPolicy::Skip,script:script.display().to_string(),script_sha256:format!("{:x}",Sha256::digest(script_bytes)),cwd:project.display().to_string(),deadline_ms:1000,output_cap_bytes:4000};
            let bytes=serde_json::to_vec(&definition).unwrap();let document=dir.path().join("routine.json");let sig=dir.path().join("routine.sig");fs::write(&document,&bytes).unwrap();
            let before=crate::runtime::snapshot(&project).unwrap();
            fs::write(&sig,sign(&key,&bytes,BUDGET_SIGNATURE_NAMESPACE)).unwrap();
            assert!(import_routine(&project,&document,&sig,before.head).is_err());assert_eq!(crate::runtime::snapshot(&project).unwrap(),before);
            fs::write(&sig,sign(&key,&bytes,ROUTINE_SIGNATURE_NAMESPACE)).unwrap();
            if !enabled {assert!(import_routine(&project,&document,&sig,before.head).is_err());assert_eq!(crate::runtime::snapshot(&project).unwrap(),before);continue;}
            let mut other=definition.clone();other.project_store=dir.path().join("other/.state/state.db").display().to_string();
            let other_bytes=serde_json::to_vec(&other).unwrap();fs::write(&document,&other_bytes).unwrap();fs::write(&sig,sign(&key,&other_bytes,ROUTINE_SIGNATURE_NAMESPACE)).unwrap();
            assert!(import_routine(&project,&document,&sig,before.head).is_err());assert_eq!(crate::runtime::snapshot(&project).unwrap(),before);
            fs::write(&document,&bytes).unwrap();fs::write(&sig,sign(&key,&bytes,ROUTINE_SIGNATURE_NAMESPACE)).unwrap();
            import_routine(&project,&document,&sig,before.head).unwrap();
            let before=crate::runtime::snapshot(&project).unwrap();assert!(import_routine(&project,&document,&sig,before.head).is_err());assert_eq!(crate::runtime::snapshot(&project).unwrap(),before);
            let occurrence=crate::routines::schedule(&project,"check",before.head).unwrap().unwrap();assert!(occurrence.slots>1);
            let mut db=migration::open_active(&project).unwrap();let now=jiff::Timestamp::now().as_millisecond();
            let claim=db.claim_operation(occurrence.operation.as_ref().unwrap(),1,"fixture",now,30_000).unwrap();
            db.validate_claim(&claim,now+1).unwrap();
            fs::write(&script,"changed script\n").unwrap();let before=db.read_snapshot(None).unwrap();
            assert!(db.validate_claim(&claim,now+2).is_err());assert_eq!(db.read_snapshot(None).unwrap(),before);
            fs::write(&script,script_bytes).unwrap();fs::write(&config,format!("{config_text}\n# changed\n")).unwrap();
            assert!(db.validate_claim(&claim,now+3).is_err());assert_eq!(db.read_snapshot(None).unwrap(),before);
            fs::write(&config,&config_text).unwrap();db.validate_claim(&claim,now+4).unwrap();
            definition.revision+=1;definition.enabled=false;let bytes=serde_json::to_vec(&definition).unwrap();
            fs::write(&document,&bytes).unwrap();fs::write(&sig,sign(&key,&bytes,ROUTINE_SIGNATURE_NAMESPACE)).unwrap();drop(db);
            import_routine(&project,&document,&sig,before.head).unwrap();
            let mut db=migration::open_active(&project).unwrap();assert!(db.validate_claim(&claim,now+5).is_err());
            assert_eq!(db.deliveries().unwrap()[0].state,crate::operations::DeliveryState::Claimed);
            let snapshot=db.read_snapshot(None).unwrap();assert!(crate::routines::schedule(&project,"check",snapshot.head).unwrap().is_none());
            assert_eq!(db.read_snapshot(None).unwrap(),snapshot);
        }
    }
    #[test]
    fn owner_policy_refuses_project_local_writable_and_symlink_files() {
        use std::os::unix::fs::{PermissionsExt,symlink};
        let dir=tempfile::tempdir().unwrap();let (_,policy)=key(dir.path(),"owner");
        let project=dir.path().join("project");fs::create_dir(&project).unwrap();
        let text=format!("[authority]\nversion=1\nrevision=1\napproval_public_key={:?}\n",policy.approval_public_key);
        let outside=dir.path().join("owner.toml");let inside=project.join("owner.toml");
        fs::write(&outside,&text).unwrap();fs::write(&inside,&text).unwrap();
        let reference=|path:&Path|migration::ConfigReference{path:path.display().to_string(),digest:None};
        policy_at(&project,&reference(&outside)).unwrap();
        assert!(policy_at(&project,&reference(&inside)).is_err());
        fs::set_permissions(&outside,fs::Permissions::from_mode(0o666)).unwrap();
        assert!(policy_at(&project,&reference(&outside)).is_err());
        fs::set_permissions(&outside,fs::Permissions::from_mode(0o600)).unwrap();
        let link=dir.path().join("link.toml");symlink(&outside,&link).unwrap();
        assert!(policy_at(&project,&reference(&link)).is_err());
    }
    #[test]
    fn real_signatures_require_exact_document_key_and_namespace() {
        let dir=tempfile::tempdir().unwrap();let (key,policy)=key(dir.path(),"owner");
        let payload=serde_json::to_vec(&grant(&policy)).unwrap();let signature=sign(&key,&payload,SIGNATURE_NAMESPACE);
        verify(&policy,&payload,&signature,&RealRunner).unwrap();
        let mut changed=payload.clone();changed.push(b' ');
        assert!(verify(&policy,&changed,&signature,&RealRunner).is_err());
        assert!(verify(&policy,&payload,&sign(&key,&payload,"wrong-namespace"),&RealRunner).is_err());
        let (other,_)=self::key(dir.path(),"other");
        assert!(verify(&policy,&payload,&sign(&other,&payload,SIGNATURE_NAMESPACE),&RealRunner).is_err());
        assert!(verify(&policy,&payload,b"invalid signature",&RealRunner).is_err());
    }
    #[test]
    fn signed_owner_ingress_uses_pinned_config_and_preserves_invalid_requests() {
        let dir=tempfile::tempdir().unwrap();let (key,policy)=key(dir.path(),"owner");
        let project=dir.path().join("project");fs::create_dir(&project).unwrap();
        for child in [".state","threads","inbox"] {fs::create_dir(project.join(child)).unwrap();}
        fs::write(project.join("PROJECT.md"),"+++\nname='Project'\n+++\n").unwrap();
        fs::write(project.join("TASKS.md"),"- [ ] work\n").unwrap();
        fs::write(project.join("MEMORY.md"),"").unwrap();
        fs::write(project.join(".state/project.json"),r#"{"status":"paused"}"#).unwrap();
        let config=dir.path().join("owner.toml");
        fs::write(&config,format!("[authority]\nversion=1\nrevision=1\napproval_public_key={:?}\n",policy.approval_public_key)).unwrap();
        let plan=migration::inspect_with_config(&project,&config).unwrap();migration::apply(&project,&plan,true).unwrap();
        assert_eq!(policy_reference(&project).unwrap(),policy.reference().unwrap());
        let mut db=migration::open_active(&project).unwrap();let snapshot=db.read_snapshot(None).unwrap();
        drop(db);crate::runtime::set_state(&project,snapshot.head,snapshot.control.unwrap().revision,ProjectState::Active,&config).unwrap();let mut db=migration::open_active(&project).unwrap();
        let snapshot=db.read_snapshot(None).unwrap();let mut grant=grant(&policy);
        grant.scope.project_store=project.join(".state/state.db").canonicalize().unwrap().display().to_string();grant.scope.task=snapshot.tasks[0].id.clone();grant.scope.task_revision=snapshot.tasks[0].revision+1;
        let payload=serde_json::to_vec(&grant).unwrap();let document=dir.path().join("grant.json");let sig=dir.path().join("grant.sig");
        fs::write(&document,&payload).unwrap();fs::write(&sig,sign(&key,&payload,"wrong")).unwrap();drop(db);
        assert!(import_signed(&project,&document,&sig,snapshot.head).is_err());assert_eq!(crate::runtime::snapshot(&project).unwrap(),snapshot);
        fs::write(&sig,sign(&key,&payload,SIGNATURE_NAMESPACE)).unwrap();
        let reference=import_signed(&project,&document,&sig,snapshot.head).unwrap();assert_eq!(reference,grant.reference().unwrap());
        let installed=crate::runtime::snapshot(&project).unwrap();assert_eq!(installed.approvals.len(),1);
        let budget=crate::domain::BudgetPolicy{version:1,revision:1,project_store:grant.scope.project_store.clone(),authority:policy.reference().unwrap(),
            limits:crate::domain::BudgetLimits{max_attempts:Some(5),max_provider_tokens:Some(100),unknown_usage:crate::domain::UnknownUsagePolicy::Refuse}};
        let bytes=serde_json::to_vec(&budget).unwrap();
        fs::write(&document,&bytes).unwrap();fs::write(&sig,sign(&key,&bytes,SIGNATURE_NAMESPACE)).unwrap();
        assert!(import_budget(&project,&document,&sig,installed.head).is_err());
        assert_eq!(crate::runtime::snapshot(&project).unwrap(),installed);
        fs::write(&sig,sign(&key,&bytes,BUDGET_SIGNATURE_NAMESPACE)).unwrap();
        assert_eq!(import_budget(&project,&document,&sig,installed.head).unwrap(),budget.reference().unwrap());
        let installed=crate::runtime::snapshot(&project).unwrap();
        assert_eq!(installed.budget_policies,vec![budget.clone()]);
        assert!(import_budget(&project,&document,&sig,installed.head).is_err());
        assert_eq!(crate::runtime::snapshot(&project).unwrap(),installed);
        let mut other=budget.clone();other.revision=2;other.project_store=dir.path().join("other/state.db").display().to_string();
        let bytes=serde_json::to_vec(&other).unwrap();fs::write(&document,&bytes).unwrap();fs::write(&sig,sign(&key,&bytes,BUDGET_SIGNATURE_NAMESPACE)).unwrap();
        assert!(import_budget(&project,&document,&sig,installed.head).is_err());
        assert_eq!(crate::runtime::snapshot(&project).unwrap(),installed);
        other.project_store=budget.project_store;
        let bytes=serde_json::to_vec(&other).unwrap();fs::write(&document,&bytes).unwrap();fs::write(&sig,sign(&key,&bytes,BUDGET_SIGNATURE_NAMESPACE)).unwrap();
        fs::write(&config,format!("[authority]\nversion=1\nrevision=2\napproval_public_key={:?}\n",policy.approval_public_key)).unwrap();
        assert!(import_budget(&project,&document,&sig,installed.head).is_err());
        assert!(import_signed(&project,&document,&sig,installed.head).is_err());assert_eq!(crate::runtime::snapshot(&project).unwrap(),installed);
    }
}

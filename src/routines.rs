//! Signed routine scheduling and explicit, claim-fenced command execution.
use std::path::Path;
use anyhow::{Result,Context,ensure};
use sha2::{Digest,Sha256};
use crate::{domain::{RoutineDefinition,RoutineOccurrence,PreparedRoutineTick},migration};

mod execution;

/// Sealed in-process result. Serialized output cannot be submitted as proof.
pub(crate) struct CompletedRoutine {pub(crate) receipt:crate::domain::RoutineReceipt}

/// Explicit, synchronous dispatch. Runtime mutation ownership spans the claim,
/// command and receipt commit. Ticker dispatch must use an owned asynchronous
/// job rather than calling this while holding its scan loop.
pub fn execute(project:&Path,operation:&crate::domain::OperationId,expected_head:u64)->Result<crate::domain::RoutineReceipt> {
    use crate::{operations::DeliveryState,runner::Cancellation};
    let _guard=migration::runtime_mutation(project)?;
    let mut db=migration::open_active(project)?;
    let snapshot=db.read_snapshot(Some(expected_head))?;
    let occurrence=snapshot.routine_occurrences.iter().find(|o|o.operation.as_ref()==Some(operation)).context("routine occurrence not found")?;
    let definition=snapshot.routine_revisions.iter().find(|d|d.reference().ok().as_ref()==Some(&occurrence.routine)).context("routine definition not found")?;
    let delivery=snapshot.deliveries.iter().find(|d|&d.operation==operation).context("routine delivery not found")?;
    ensure!(delivery.state==DeliveryState::Pending && delivery.attempts==0,"routine already claimed; execution cannot be replayed");
    let script=load_current(definition)?.context("routine is disabled")?;
    execution::preflight(&script,definition.deadline_ms,definition.output_cap_bytes)?;
    let now=jiff::Timestamp::now().as_millisecond();
    let claim=db.claim_operation(operation,delivery.revision,"routine-linux-namespace-v1",now,definition.deadline_ms as i64+30_000)?;
    db.validate_claim(&claim,jiff::Timestamp::now().as_millisecond())?;
    let completion=execution::run(&script,Path::new(&definition.cwd),definition.deadline_ms,definition.output_cap_bytes,Cancellation::default());
    let (output,cleanup_verified,succeeded)=match completion {
        Ok(c)=>(c.output,c.cleanup_verified,c.succeeded),
        // An I/O error after spawn cannot certify cleanup. Diagnostics never
        // include a script body or inherited environment.
        Err(_)=>(crate::runner::Output::default(),false,false),
    };
    let receipt=crate::domain::RoutineReceipt {operation:operation.clone(),routine:occurrence.routine.clone(),claim,
        finished_unix_ms:jiff::Timestamp::now().as_millisecond(),cleanup_verified,succeeded,
        stdout:output.stdout_bytes,stderr:output.stderr_bytes,stdout_truncated:output.stdout_truncated,stderr_truncated:output.stderr_truncated,
        stdout_total_bytes:output.stdout_total_bytes,stderr_total_bytes:output.stderr_total_bytes,
        elapsed_ms:u64::try_from(output.elapsed.as_millis()).unwrap_or(u64::MAX)};
    db.complete_routine(&CompletedRoutine{receipt:receipt.clone()})?;
    Ok(receipt)
}

fn parse_config(bytes:&[u8],expected:&str)->Result<toml::Value> {
    ensure!(format!("{:x}",Sha256::digest(bytes))==expected,"routine configuration bytes do not match signed identity");
    let text=std::str::from_utf8(bytes).context("invalid routine configuration")?;
    toml::from_str(text).map_err(|_|anyhow::anyhow!("invalid routine configuration (contents withheld)"))
}

pub(crate) fn validate_current(definition:&RoutineDefinition)->Result<()> {
    load_current(definition).map(|_| ())
}

/// Return the exact verified bytes for execution; never reopen the script after
/// this check. Disabled definitions intentionally do not require a script.
fn load_current(definition:&RoutineDefinition)->Result<Option<Vec<u8>>> {
    definition.validate().map_err(anyhow::Error::msg)?;
    let store=Path::new(&definition.project_store);
    let project=store.parent().and_then(Path::parent).context("routine project unavailable")?;
    ensure!(store.file_name().is_some_and(|n|n=="state.db")&&store.parent().and_then(Path::file_name).is_some_and(|n|n==".state"),"routine requires canonical store layout");
    let (authority,config)=crate::authority::routine_policy(project)?;
    ensure!(authority==definition.authority && config==definition.config,"routine owner authority or configuration changed");
    ensure!(project.canonicalize()?==Path::new(&definition.cwd).canonicalize()?,"routine working directory must be its project");
    let bytes=migration::read_plan_file(Path::new(&config.path))?;
    let config_value=parse_config(&bytes,definition.config.digest.as_deref().context("routine configuration identity missing")?)?;
    let project_path=project.canonicalize()?;
    let enabled=config_value.get("safety").and_then(|s|s.get(project_path.to_string_lossy().as_ref())).and_then(|s|s.get("routine_commands")).and_then(toml::Value::as_bool)==Some(true);
    let script=if definition.enabled {
        ensure!(enabled,"routine commands are not enabled in owner configuration");
        let bytes=migration::read_plan_file(Path::new(&definition.script)).map_err(|_|anyhow::anyhow!("routine script unavailable"))?;
        ensure!(bytes.len()<=65_536 && format!("{:x}",Sha256::digest(&bytes))==definition.script_sha256,"routine script changed or exceeds bounds");
        ensure!(!bytes.contains(&0),"routine script contains NUL");
        Some(bytes)
    } else {None};
    ensure!(migration::config_reference(Path::new(&config.path))?==definition.config,"routine configuration changed during validation");
    Ok(script)
}

pub fn schedule(project:&Path,name:&str,expected_head:u64)->Result<Option<RoutineOccurrence>> {
    let _guard=migration::runtime_mutation(project)?;
    let mut db=migration::open_active(project)?;
    let snapshot=db.read_snapshot(Some(expected_head))?;
    let definition=snapshot.routine_revisions.into_iter().rev().find(|d|d.name==name).context("routine not found")?;
    ensure!(definition.project_store==project.join(".state/state.db").canonicalize()?.to_string_lossy(),"routine belongs to another project");
    validate_current(&definition)?;
    let now=jiff::Timestamp::now().as_millisecond();
    Ok(db.schedule_routine(&PreparedRoutineTick{definition,now},expected_head)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn permission_parse_binds_exact_bytes_not_only_before_and_after_path_checks() {
        let denied=b"[safety.fixture]\nroutine_commands=false\n";
        let allowed=b"[safety.fixture]\nroutine_commands=true\n";
        let expected=format!("{:x}",Sha256::digest(denied));
        assert_eq!(parse_config(denied,&expected).unwrap()["safety"]["fixture"]["routine_commands"].as_bool(),Some(false));
        assert!(parse_config(allowed,&expected).is_err());
        assert!(parse_config(denied,&expected).is_ok());
    }
}

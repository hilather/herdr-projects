//! Supported routine control path. These functions record work; they do not run scripts.
use std::path::Path;
use anyhow::{Result,Context,ensure};
use sha2::{Digest,Sha256};
use crate::{domain::{RoutineDefinition,RoutineOccurrence,PreparedRoutineTick},migration};

fn parse_config(bytes:&[u8],expected:&str)->Result<toml::Value> {
    ensure!(format!("{:x}",Sha256::digest(bytes))==expected,"routine configuration bytes do not match signed identity");
    let text=std::str::from_utf8(bytes).context("invalid routine configuration")?;
    toml::from_str(text).map_err(|_|anyhow::anyhow!("invalid routine configuration (contents withheld)"))
}

pub(crate) fn validate_current(definition:&RoutineDefinition)->Result<()> {
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
    if definition.enabled {
        ensure!(enabled,"routine commands are not enabled in owner configuration");
        let bytes=migration::read_plan_file(Path::new(&definition.script)).map_err(|_|anyhow::anyhow!("routine script unavailable"))?;
        ensure!(bytes.len()<=65_536 && format!("{:x}",Sha256::digest(&bytes))==definition.script_sha256,"routine script changed or exceeds bounds");
        ensure!(!bytes.contains(&0),"routine script contains NUL");
    }
    ensure!(migration::config_reference(Path::new(&config.path))?==definition.config,"routine configuration changed during validation");
    Ok(())
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

//! Signed routine revisions and immutable scheduling decisions; output is never authority.
use serde::{Deserialize,Serialize};
use sha2::{Digest,Sha256};
use super::{OperationId,VersionedReference};

#[derive(Debug,Clone,Copy,PartialEq,Eq,Serialize,Deserialize)]
#[serde(rename_all="snake_case")]
pub enum MissedRunPolicy { Skip, CoalesceLatest }
#[derive(Debug,Clone,Copy,PartialEq,Eq,Serialize,Deserialize)]
#[serde(rename_all="snake_case")]
pub enum OverlapPolicy { Skip }

#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutineDefinition {
    pub version:u32,
    pub name:String,
    pub revision:u64,
    pub project_store:String,
    pub authority:VersionedReference,
    pub config:crate::migration::ConfigReference,
    pub enabled:bool,
    pub schedule:String,
    pub timezone:String,
    pub start_unix_ms:i64,
    pub missed:MissedRunPolicy,
    pub overlap:OverlapPolicy,
    /// Exact script bytes are pinned, not arbitrary shell dependency contents.
    pub script:String,
    pub script_sha256:String,
    pub cwd:String,
    pub deadline_ms:u64,
    pub output_cap_bytes:u32,
}
fn hash(s:&str)->bool {s.len()==64&&s.bytes().all(|b|b.is_ascii_digit()||(b'a'..=b'f').contains(&b))}
fn path(s:&str)->bool {std::path::Path::new(s).is_absolute()&&s.len()<=4096&&!s.chars().any(char::is_control)}
impl RoutineDefinition {
    pub fn validate(&self)->Result<(),String> {
        let a=&self.authority;
        if self.version!=1||self.name.is_empty()||self.name.len()>64||!self.name.bytes().all(|b|b.is_ascii_alphanumeric()||b"-_".contains(&b))
            ||self.revision==0||self.revision>i64::MAX as u64||!path(&self.project_store)||!path(&self.script)||!path(&self.cwd)
            ||!path(&self.config.path)||!self.config.digest.as_deref().is_some_and(hash)||!hash(&self.script_sha256)
            ||a.id.is_empty()||a.id.len()>512||a.id.chars().any(char::is_control)||a.revision==0||a.revision>i64::MAX as u64||!hash(&a.digest)
            ||self.start_unix_ms<0||jiff::Timestamp::from_millisecond(self.start_unix_ms).is_err()
            ||!(1..=60_000).contains(&self.deadline_ms)||!(1..=65_536).contains(&self.output_cap_bytes)
            ||self.schedule.len()>128||self.timezone.len()>128 {
            return Err("invalid routine definition".into());
        }
        crate::schedule::parse_schedule(&self.schedule).map_err(|_|"invalid routine schedule")?;
        jiff::tz::TimeZone::get(&self.timezone).map_err(|_|"invalid routine timezone")?;
        Ok(())
    }
    pub fn reference(&self)->Result<VersionedReference,String> {
        self.validate()?;
        Ok(VersionedReference{id:format!("routine-{}",self.name),revision:self.revision,
            digest:format!("{:x}",Sha256::digest(serde_json::to_vec(self).map_err(|_|"routine encoding failed")?))})
    }
}
pub struct PreparedRoutine {pub(crate) definition:RoutineDefinition}
pub struct PreparedRoutineTick {pub(crate) definition:RoutineDefinition,pub(crate) now:i64}

#[derive(Debug,Clone,Copy,PartialEq,Eq,Serialize,Deserialize)]
#[serde(rename_all="snake_case")]
pub enum RoutineDisposition { Enqueued, SkippedMissed, SkippedOverlap }
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutineOccurrence {
    pub id:String,
    pub routine:VersionedReference,
    pub first_unix_ms:i64,
    pub scheduled_unix_ms:i64,
    /// Calendar slots may share an instant after an entire date is skipped.
    pub slots:u64,
    pub observed_unix_ms:i64,
    pub control_revision:u64,
    pub disposition:RoutineDisposition,
    pub operation:Option<OperationId>,
}
impl RoutineOccurrence {
    pub fn identity(definition:&RoutineDefinition,scheduled:i64)->Result<String,String> {
        Ok(format!("routine-{:x}",Sha256::digest(serde_json::to_vec(&(&definition.project_store,&definition.name,definition.revision,scheduled)).map_err(|_|"occurrence encoding failed")?)))
    }
}

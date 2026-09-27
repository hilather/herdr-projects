//! Barrier membership publication; release authority lives in signed ingress.
use anyhow::Result;
use std::{path::Path,time::{Duration,Instant}};
use crate::store::controlled::ReadControl;

pub fn freeze_memory_barrier(
    project: &Path,
    members: &[crate::store::BarrierMember],
    expected_head: u64,
) -> Result<crate::store::FrozenBarrier> {
    let control=ReadControl::new(Instant::now()+Duration::from_secs(2),Default::default());
    let _guard = crate::migration::runtime_mutation(project)?;
    Ok(crate::migration::open_active_scoped(project,control)?.freeze_barrier(members, expected_head)?)
}

/// Read and account the CLI membership document before decoding and publication.
pub fn freeze_memory_barrier_file(project:&Path,input:&Path,expected_head:u64)->Result<crate::store::FrozenBarrier> {
    let control=ReadControl::new(Instant::now()+Duration::from_secs(2),Default::default());
    let _guard=crate::migration::runtime_mutation(project)?;
    control.check()?;
    let raw=crate::migration::read_plan_file(input)?;
    Ok(crate::migration::open_active_scoped(project,control)?.freeze_barrier_json(&raw,expected_head)?)
}

pub fn inspect_memory_barrier(project:&Path,id:&str)->Result<Option<crate::store::FrozenBarrier>> {
    let control=ReadControl::new(Instant::now()+Duration::from_secs(2),Default::default());
    let _guard=crate::migration::runtime_mutation(project)?;
    Ok(crate::migration::open_active_scoped(project,control)?.frozen_barrier(id)?)
}

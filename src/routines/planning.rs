//! Routine intent planning under ownership already held by a background job.
use std::{path::Path,time::Instant};
use anyhow::{Result,ensure};
use crate::{domain::{PreparedRoutineTick,RoutineDefinition},execution_guard::{ProjectGuard,exclusive_file},migration,runner::Cancellation};

/// Liveness and rotation bookkeeping, never permission to execute a command.
#[derive(Debug,Default)]
pub struct PlannedRoutine {
    pub active:bool,
    pub diagnostic:Option<String>,
    pub selected_name:Option<String>,
}

/// Select strictly after the last serviced routine, wrapping by name. The caller
/// retains its ProjectGuard across planning and any subsequent observations.
/// Cancellation is checked before commit; a successful commit is not relabelled
/// as a no-write cancellation if cancellation arrives afterward. SQL shares the
/// original control; aggregate decoded materialization still needs bounds.
pub fn schedule_next_guarded(project:&Path,last:Option<&str>,guard:&ProjectGuard,cancellation:&Cancellation,deadline:Instant)->Result<PlannedRoutine> {
    plan(project,last,guard,cancellation,deadline,super::validate_current)
}

fn check(cancellation:&Cancellation,deadline:Instant)->Result<()> {
    ensure!(!cancellation.is_cancelled(),"routine planning cancelled");
    ensure!(Instant::now()<deadline,"routine planning deadline elapsed");Ok(())
}

fn plan(project:&Path,last:Option<&str>,guard:&ProjectGuard,cancellation:&Cancellation,deadline:Instant,validate:impl FnOnce(&RoutineDefinition)->Result<()>)->Result<PlannedRoutine> {
    check(cancellation,deadline)?;guard.check_project(project)?;
    let mut db=migration::open_active_scoped(project,crate::store::controlled::ReadControl::new(deadline,cancellation.clone()))?;
    check(cancellation,deadline)?;
    let (head,definition)=db.routine_planning_selection(last)?;
    check(cancellation,deadline)?;
    let Some(definition)=definition else{return Ok(PlannedRoutine::default());};
    ensure!(definition.project_store==project.join(".state/state.db").canonicalize()?.to_string_lossy(),"routine belongs to another project");
    let validated=validate(&definition);
    check(cancellation,deadline)?;guard.check_project(project)?;
    let report=PlannedRoutine{active:true,diagnostic:None,selected_name:Some(definition.name.clone())};
    if let Err(error)=validated{return Ok(PlannedRoutine{diagnostic:Some(format!("{}: {error:#}",definition.name)),..report});}
    // Input validation holds only project ownership. The short record lock is
    // acquired afterward; the transaction must still match the original head.
    let _record=exclusive_file(&project.join(".state/lock"))?;
    check(cancellation,deadline)?;guard.check_project(project)?;
    db.schedule_routine(&PreparedRoutineTick{definition,now:jiff::Timestamp::now().as_millisecond()},head)?;
    Ok(report)
}

#[cfg(test)]
mod tests;

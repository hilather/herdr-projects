//! Conservative local/remote alias checks across canonical and legacy projects.
use super::*;
use crate::store::identity_inventory::Budget;
use std::fs;

// Decode only routing fields, but never turn malformed values into absence.
// Other legacy fields remain outside this bounded ownership projection.
#[derive(Default, serde::Deserialize)]
#[serde(default)]
struct CoordinatorIdentity {
    socket: String,
    pane_id: String,
}
#[derive(serde::Deserialize)]
struct ThreadIdentity {
    id: String,
    #[serde(default)]
    machine: String,
    #[serde(default)]
    pane_id: String,
}

fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}
fn location(value: &str) -> Result<std::path::PathBuf> {
    let path = Path::new(value);
    ensure!(
        path.is_absolute(),
        "resource reference requires an absolute path"
    );
    match path.canonicalize() {
        Ok(p) => Ok(p),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            ensure!(
                path.components().all(|p| matches!(
                    p,
                    std::path::Component::RootDir | std::path::Component::Normal(_)
                )),
                "unresolved socket alias"
            );
            Ok(path.into())
        }
        Err(e) => Err(e.into()),
    }
}
pub(super) fn check(
    project: &Path,
    binding: &str,
    target: &RuntimeRoute,
    deadline: Instant,
    cancellation: Cancellation,
) -> Result<()> {
    let mut budget = Budget::new(50 * 1024 * 1024, 1024, deadline, cancellation)?;
    let endpoint = location(&target.socket)?;
    let reference = |machine: &str, socket: &str, pane: &str| -> Result<()> {
        if !pane.is_empty() && pane == target.pane_id {
            ensure!(
                machine.is_empty() && target.machine.is_empty(),
                "terminal reference has unresolved remote identity"
            );
            ensure!(
                location(socket)? != endpoint,
                "worker pane is referenced by another binding"
            );
        }
        Ok(())
    };
    for (n, entry) in fs::read_dir(project.parent().context("project root missing")?)?.enumerate() {
        budget.check()?;
        ensure!(n < 1024, "root identity inventory exceeds bounds");
        let entry = entry?;
        let kind = entry.file_type()?;
        ensure!(
            !kind.is_symlink(),
            "root identity inventory contains a symlink"
        );
        if !kind.is_dir() {
            continue;
        }
        let dir = entry.path();
        if !exists(&dir.join(".state"))? {
            continue;
        }
        ensure!(
            fs::symlink_metadata(dir.join(".state"))?.is_dir(),
            "project state is aliased"
        );
        let canonical =
            exists(&dir.join(".state/format.json"))? || exists(&dir.join(".state/migration"))?;
        if canonical {
            for other in crate::migration::read_identity_inventory(&dir, &mut budget)? {
                if dir == project && other.id == binding {
                    continue;
                }
                reference(
                    &other.identity.machine,
                    &other.identity.socket,
                    &other.identity.pane_id,
                )?;
            }
            for (owner, target) in
                crate::migration::read_launch_target_inventory(&dir, &mut budget)?
            {
                if dir == project && owner == binding {
                    continue;
                }
                reference(
                    &target.route.machine,
                    &target.route.socket,
                    &target.route.pane_id,
                )?;
            }
            continue;
        }
        let coordinator = dir.join(".state/coordinator.json");
        let coordinator: CoordinatorIdentity = if exists(&coordinator)? {
            let value: Value = serde_json::from_slice(&budget.read(&coordinator)?)
                .map_err(|_| anyhow::anyhow!("invalid legacy coordinator identity"))?;
            ensure!(
                value.is_object(),
                "legacy coordinator identity must be an object"
            );
            serde_json::from_value(value)
                .map_err(|_| anyhow::anyhow!("invalid legacy coordinator identity fields"))?
        } else {
            CoordinatorIdentity::default()
        };
        let socket = coordinator.socket.as_str();
        budget.record()?;
        reference("", socket, &coordinator.pane_id)?;
        let threads = dir.join("threads");
        ensure!(
            fs::symlink_metadata(&threads)?.is_dir(),
            "legacy thread inventory missing or aliased"
        );
        for (n, entry) in fs::read_dir(threads)?.enumerate() {
            budget.check()?;
            ensure!(n < 256, "legacy thread inventory exceeds bounds");
            let path = entry?.path();
            if path.extension().is_none_or(|s| s != "toml") {
                continue;
            }
            let bytes = budget.read(&path)?;
            let record: ThreadIdentity = toml::from_str(
                std::str::from_utf8(&bytes)
                    .map_err(|_| anyhow::anyhow!("legacy thread identity is not UTF-8"))?,
            )
            .map_err(|_| anyhow::anyhow!("invalid legacy thread identity"))?;
            let digits = record.id.strip_prefix("t-").unwrap_or("");
            ensure!(
                digits.len() >= 4 && digits.bytes().all(|b| b.is_ascii_digit()),
                "invalid legacy thread identifier"
            );
            ensure!(
                path.file_stem().and_then(|s| s.to_str()) == Some(record.id.as_str()),
                "legacy thread identity mismatch"
            );
            budget.record()?;
            reference(&record.machine, socket, &record.pane_id)?;
        }
    }
    budget.check()
}

/// Root-wide path references, including unacknowledged creation intents. The
/// caller holds the root barrier through subsequent worktree creation.
pub(crate) fn check_worktrees(
    project:&Path,binding:&str,plans:&[WorktreePlan],deadline:Instant,cancellation:Cancellation,
)->Result<()> {
    let mut budget=Budget::new(50*1024*1024,1024,deadline,cancellation)?;
    let candidates=plans.iter().map(|p|location(&p.path)).collect::<Result<Vec<_>>>()?;
    let check=|machine:&str,path:&str|->Result<()> {
        if !machine.is_empty()||path.is_empty(){return Ok(());}
        let other=location(path)?;
        ensure!(!candidates.iter().any(|p|p.starts_with(&other)||other.starts_with(p)),"worktree path is referenced by another binding");Ok(())
    };
    for (n,entry) in fs::read_dir(project.parent().context("project root missing")?)?.enumerate() {
        budget.check()?;ensure!(n<1024,"worktree root inventory exceeds bounds");
        let entry=entry?;let kind=entry.file_type()?;ensure!(!kind.is_symlink(),"worktree root inventory contains a symlink");
        if !kind.is_dir(){continue;}let dir=entry.path();if !exists(&dir.join(".state"))?{continue;}
        ensure!(fs::symlink_metadata(dir.join(".state"))?.is_dir(),"worktree project state is aliased");
        if exists(&dir.join(".state/format.json"))?||exists(&dir.join(".state/migration"))? {
            for other in crate::migration::read_identity_inventory(&dir,&mut budget)? {
                if dir==project&&other.id==binding{continue;}
                check(&other.identity.machine,&other.identity.worktree_path)?;
            }
            for (owner,plan) in crate::migration::read_worktree_inventory(&dir,&mut budget)? {
                if dir==project&&owner==binding{continue;}check("",&plan.path)?;
            }
        } else {
            let threads=dir.join("threads");ensure!(fs::symlink_metadata(&threads)?.is_dir(),"legacy worktree thread inventory unavailable");
            #[derive(serde::Deserialize)]
            struct Tree {id:String,#[serde(default)]machine:String,#[serde(default)]worktree_path:String}
            for (n,entry) in fs::read_dir(threads)?.enumerate() {
                budget.check()?;ensure!(n<256,"legacy worktree inventory exceeds bounds");let path=entry?.path();
                if path.extension().is_none_or(|e|e!="toml"){continue;}budget.record()?;
                let bytes=budget.read(&path)?;let tree:Tree=toml::from_str(std::str::from_utf8(&bytes)?)?;
                let digits=tree.id.strip_prefix("t-").unwrap_or("");
                ensure!(digits.len()>=4&&digits.bytes().all(|b|b.is_ascii_digit()),"invalid legacy worktree thread identifier");
                ensure!(path.file_stem().and_then(|p|p.to_str())==Some(tree.id.as_str()),"legacy worktree identity mismatch");
                check(&tree.machine,&tree.worktree_path)?;
            }
        }
    }
    budget.check()
}

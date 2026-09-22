//! Deterministic repository resources for a retained launch. Plans are data;
//! only the canonical producer can seal creation or observation evidence.
use super::*;
use std::path::{Component, Path};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorktreePlan {
    pub source: RepositoryInput,
    pub path: String,
    pub branch: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorktreeCreation {
    pub version: u32,
    pub operation: OperationId,
    pub attempt: AttemptId,
    pub plans: Vec<WorktreePlan>,
    pub token: String,
}
pub struct PreparedWorktreeCreation {
    pub(crate) intent: WorktreeCreation,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorktreeReceipt {
    pub plan: WorktreePlan,
    pub directory: ResourceIdentity,
    pub git_directory: String,
    pub git_identity: ResourceIdentity,
    pub common_directory: String,
    pub common_identity: ResourceIdentity,
}
pub struct PreparedWorktreeReceipts {
    pub(crate) intent: WorktreeCreation,
    pub(crate) receipts: Vec<WorktreeReceipt>,
}

/// Durable repository-state manifest retained before terminal disposition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorktreeSnapshotReference {
    pub plan: WorktreePlan,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptOutputReference {
    pub source: String,
    /// None records an observed absent output directory, not a missing receipt.
    pub digest: Option<String>,
}

/// None means the planned directory, branch and Git registration were absent.
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparationSnapshotReference {pub plan:WorktreePlan,pub digest:Option<String>}

/// Derived only from approved immutable inputs. No branch or directory is
/// accepted from the caller and no filesystem mutation occurs here.
pub fn worktree_plans(
    inputs: &LaunchInputs,
    attempt: &AttemptId,
) -> Result<Vec<WorktreePlan>, String> {
    if inputs.repositories.len() > 64 {
        return Err("too many worktree inputs".into());
    }
    let store = Path::new(&inputs.project_store);
    if !store.is_absolute()
        || store.file_name().and_then(|s| s.to_str()) != Some("state.db")
        || store
            .parent()
            .and_then(Path::file_name)
            .and_then(|s| s.to_str())
            != Some(".state")
        || store
            .components()
            .any(|p| !matches!(p, Component::RootDir | Component::Normal(_)))
    {
        return Err("worktree plan requires canonical project store path".into());
    }
    let suffix = attempt
        .as_str()
        .strip_prefix("attempt-")
        .ok_or("invalid worktree attempt")?;
    if suffix.len() != 64
        || !suffix
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return Err("invalid worktree attempt".into());
    }
    let root = store
        .parent()
        .unwrap()
        .join("worktrees")
        .join(attempt.as_str());
    let mut seen = std::collections::BTreeSet::new();
    inputs
        .repositories
        .iter()
        .enumerate()
        .map(|(index, source)| {
            if !Path::new(&source.repository).is_absolute()
                || !seen.insert(&source.repository)
                || [&source.commit, &source.tree].iter().any(|s| {
                    !matches!(s.len(), 40 | 64)
                        || !s
                            .bytes()
                            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
                })
            {
                return Err("invalid worktree repository input".into());
            }
            Ok(WorktreePlan {
                source: source.clone(),
                path: root
                    .join(format!("repo-{index:02}"))
                    .to_str()
                    .ok_or("worktree path is not UTF-8")?
                    .into(),
                branch: format!("hp-{suffix}"),
            })
        })
        .collect()
}

/// Map the selected source working directory into its isolated checkout. Extra
/// repositories remain explicitly mapped resources; no implicit fallback to a
/// source directory is permitted for repository-backed launches.
pub fn worktree_execution_route(
    inputs: &LaunchInputs,
    attempt: &AttemptId,
    identity: &RuntimeIdentity,
) -> Result<(RuntimeRoute, Option<WorktreePlan>), String> {
    let mut route = RuntimeRoute::from_identity(identity);
    if inputs.repositories.is_empty() {
        if !identity.repo.is_empty() || !identity.worktree_path.is_empty() {
            return Err("repository binding lacks approved worktree inputs".into());
        }
        return Ok((route, None));
    }
    if !route.machine.is_empty() || !identity.worktree_path.is_empty() {
        return Err("worktree launch requires an unused local binding".into());
    }
    let cwd = Path::new(&route.cwd);
    if !cwd.is_absolute()
        || cwd
            .components()
            .any(|c| !matches!(c, Component::RootDir | Component::Normal(_)))
    {
        return Err("worktree working directory must be canonical".into());
    }
    let plans = worktree_plans(inputs, attempt)?;
    let selected = plans
        .into_iter()
        .filter(|p| {
            (identity.repo.is_empty() || identity.repo == p.source.repository)
                && cwd.starts_with(&p.source.repository)
        })
        .max_by_key(|p| Path::new(&p.source.repository).components().count())
        .ok_or("working directory is outside the approved repositories")?;
    let relative = cwd
        .strip_prefix(&selected.source.repository)
        .map_err(|_| "working directory does not belong to selected repository")?;
    route.cwd = if relative.as_os_str().is_empty() {
        selected.path.clone()
    } else {
        Path::new(&selected.path)
            .join(relative.components().collect::<std::path::PathBuf>())
            .to_str()
            .ok_or("worktree working directory is not UTF-8")?
            .into()
    };
    Ok((route, Some(selected)))
}

/// Per-attempt report/library destination, independent of shared source cwd.
pub fn worker_output_path(inputs: &LaunchInputs, attempt: &AttemptId) -> Result<String,String> {
    // Reuse canonical store/attempt validation even for non-repository tasks.
    worktree_plans(inputs,attempt)?;
    let state=Path::new(&inputs.project_store).parent().ok_or("worker output state directory missing")?;
    let path=state.join("worker-output").join(attempt.as_str());
    let path=path.to_str().ok_or("worker output path is not UTF-8")?;
    if path.len()>4096 || path.chars().any(char::is_control) {return Err("worker output path exceeds filesystem bounds".into());}
    Ok(path.into())
}

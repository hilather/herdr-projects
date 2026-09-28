//! Controller-side capture of a worker's edits. Workers run sandboxed and never
//! commit: Git metadata stays read-only to them. The controller snapshots the
//! attempt's own worktree into a commit on the attempt's own branch with a fixed
//! identity. The commit is still an untrusted candidate; only verification of a
//! submission that names it counts as evidence.
use crate::{domain::*, runner::Cancellation, worktree_preparation::Git};
use anyhow::{bail, ensure, Context, Result};
use serde::Serialize;
use std::{
    path::{Component, Path},
    time::{Duration, Instant},
};

pub const CAPTURE_NAME: &str = "herdr-projects";
pub const CAPTURE_EMAIL: &str = "capture@herdr-projects.invalid";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CaptureReceipt {
    pub attempt: String,
    pub task: String,
    pub repository: String,
    pub branch: String,
    pub base_oid: String,
    pub candidate_oid: String,
    /// False when the worktree already matched the branch tip.
    pub captured: bool,
}

/// Capture with a bounded 60 s deadline.
pub fn capture_project(project: &Path, attempt: &str, message: Option<&str>) -> Result<CaptureReceipt> {
    capture(project, &AttemptId::new(attempt.to_owned()).map_err(anyhow::Error::msg)?, message,
        Instant::now() + Duration::from_secs(60), Cancellation::default())
}

pub fn capture(project: &Path, attempt: &AttemptId, message: Option<&str>, deadline: Instant, cancellation: Cancellation) -> Result<CaptureReceipt> {
    let message = message.map(str::to_owned).unwrap_or_else(|| format!("Capture attempt {}", attempt.as_str()));
    ensure!(!message.trim().is_empty() && message.len() <= 4096 && !message.contains('\0'), "capture message must be 1..4096 bytes without NUL");
    let project = project.canonicalize()?;
    let guard = crate::execution_guard::RootGuard::exclusive(project.parent().context("project root missing")?)?;
    let mut db = crate::migration::open_active_scoped(&project, crate::store::controlled::ReadControl::new(deadline, cancellation.clone()))?;
    let state = db.read_snapshot(None)?;
    ensure!(state.attempts.iter().any(|a| &a.id == attempt), "attempt missing");
    let record = state.attempt_inputs.iter().find(|r| &r.attempt == attempt)
        .context("capture requires the attempt's retained launch inputs")?;
    ensure!(record.inputs.repositories.len() == 1, "capture requires exactly one repository worktree");
    let scopes = db.attempt_contract(record.inputs.task.as_str(), record.inputs.task_contract.as_ref())?.and_then(|contract| contract.write_scopes());
    drop(db);
    // Exact provenance: the worktree, its gitdir, lock and branch are the ones
    // recorded when this attempt's worktree was prepared.
    let proof = crate::worktree_preparation::pin_started_events_held(&project, &state.events, record, deadline, cancellation.clone())?;
    let ready = state.events.iter().find(|e| e.kind == "runtime.worktrees_ready" && e.entity == record.operation.as_str())
        .context("ready worktree evidence missing")?;
    let receipts: Vec<WorktreeReceipt> = serde_json::from_value(ready.payload.clone())?;
    let plan = &receipts.first().context("ready worktree evidence missing")?.plan;
    let git = Git { deadline, cancellation, locks: guard.inherit()? };
    let path = Path::new(&plan.path);
    let pinned = format!("core.worktree={}", plan.path);
    let branch = format!("refs/heads/{}", plan.branch);
    let identity = [format!("user.name={CAPTURE_NAME}"), format!("user.email={CAPTURE_EMAIL}")];
    let run = |args: &[&str]| -> Result<String> {
        let mut all = vec!["-c", pinned.as_str(), "-c", identity[0].as_str(), "-c", identity[1].as_str()];
        all.extend_from_slice(args);
        let bytes = git.capture(path, &all, None, 4 * 1024 * 1024)?;
        Ok(String::from_utf8(bytes).context("capture Git output is not UTF-8")?.trim_end_matches('\n').to_owned())
    };
    let base = plan.source.commit.clone();
    let head = run(&["rev-parse", "--verify", &format!("{branch}^{{commit}}")])?;
    run(&["merge-base", "--is-ancestor", &base, &head]).context("attempt branch no longer descends from its base")?;

    // Stage tracked and untracked files (honouring .gitignore). Whatever the
    // outcome, the index is reset to the branch tip afterwards; files are never touched.
    let staged = (|| -> Result<String> {
        run(&["add", "--all", "--", "."])?;
        let tree = run(&["write-tree"])?;
        check_changes(&run(&["diff-tree", "-r", "-z", "--no-renames", "--no-ext-diff", "--no-textconv", &base, &tree])?,
            scopes.as_deref(), |oid| run(&["cat-file", "blob", oid]))?;
        Ok(tree)
    })();
    if staged.is_err() {
        run(&["reset", "--quiet", "--mixed"])?;
    }
    let tree = staged?;
    let receipt = |candidate: String, captured: bool| CaptureReceipt {
        attempt: attempt.as_str().into(), task: record.inputs.task.as_str().into(), repository: plan.source.repository.clone(),
        branch: branch.clone(), base_oid: base.clone(), candidate_oid: candidate, captured,
    };
    if run(&["rev-parse", "--verify", &format!("{head}^{{tree}}")])? == tree {
        ensure!(head != base, "worktree has no changes to capture");
        proof.check()?;
        return Ok(receipt(head, false));
    }
    let commit = run(&["commit-tree", &tree, "-p", &head, "-m", &message])?;
    proof.check()?;
    // Compare-and-swap on the attempt's own branch only.
    run(&["update-ref", "-m", "herdr-projects capture", &branch, &commit, &head])?;
    proof.check()?;
    Ok(receipt(commit, true))
}

/// Refuse gitlinks, symlinks that leave the worktree, and changes outside the
/// contract's write scope. `raw` is `git diff-tree -r -z` output.
fn check_changes(raw: &str, scopes: Option<&[String]>, blob: impl Fn(&str) -> Result<String>) -> Result<()> {
    let mut fields = raw.split('\0').filter(|f| !f.is_empty());
    while let Some(header) = fields.next() {
        let path = fields.next().context("invalid capture diff")?;
        let words: Vec<_> = header.trim_start_matches(':').split(' ').collect();
        ensure!(words.len() == 5, "invalid capture diff");
        if let Some(scopes) = scopes {
            ensure!(crate::domain::in_write_scope(scopes, path.as_bytes()), "change outside the contract's write scope: {path}");
        }
        match words[1] {
            "160000" => bail!("nested repository cannot be captured: {path}"),
            "120000" => {
                let target = blob(words[3])?;
                let mut depth = Path::new(path).components().count() as i64 - 1;
                for component in Path::new(&target).components() {
                    match component {
                        Component::Normal(_) => depth += 1,
                        Component::CurDir => {}
                        Component::ParentDir => depth -= 1,
                        _ => depth = -1,
                    }
                    ensure!(depth >= 0, "symlink leaves the worktree: {path}");
                }
            }
            _ => {}
        }
    }
    Ok(())
}

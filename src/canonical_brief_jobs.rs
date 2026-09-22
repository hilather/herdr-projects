//! Queue inputs choose immutable operation IDs. The native library ingress,
//! rather than executor output or a caller-supplied Runner, certifies delivery.
use crate::{
    executor::{Identity, Lane, Request},
    runner::{Cmd, Output, Runner},
};
use anyhow::{Context, Result, ensure};
use herdr_projects::domain::{AttemptId, Operation, OperationId};
use serde::{Deserialize, Serialize};
use std::{
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
const JOB: &str = "\0herdr-projects-canonical-brief";
const BUDGET: Duration = Duration::from_secs(45);
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    project: PathBuf,
    identity: (u64, u64),
    operation: OperationId,
    termination: Option<AttemptId>,
    preparation: Option<AttemptId>,
    revision: u64,
    #[serde(default)]
    resource_recovery: bool,
    #[serde(default)]
    launch_advance: bool,
}

pub fn request(project: &Path, operation: &Operation, revision: u64) -> Result<Request> {
    request_inner(project,operation,revision,false)
}
pub fn request_launch(project:&Path,operation:&Operation,revision:u64)->Result<Request> {
    ensure!(operation.kind=="runtime.launch","launch request requires a launch operation");
    request_inner(project,operation,revision,true)
}
fn request_inner(project:&Path,operation:&Operation,revision:u64,launch_advance:bool)->Result<Request> {
    ensure!(
        matches!(
            operation.kind.as_str(),
            "runtime.worker_brief"
                | "runtime.worker_termination"
                | "runtime.worker_brief_prepare"
                | "runtime.launch"
        ) && revision > 0,
        "invalid canonical brief hint"
    );
    let project = project.canonicalize()?;
    let metadata = std::fs::metadata(&project)?;
    let input = Input {
        project: project.clone(),
        identity: (metadata.dev(), metadata.ino()),
        operation: operation.id.clone(),
        termination: if operation.kind == "runtime.worker_termination" {
            Some(AttemptId::new(operation.target.clone()).map_err(anyhow::Error::msg)?)
        } else {
            None
        },
        preparation: if operation.kind == "runtime.worker_brief_prepare" {
            Some(AttemptId::new(operation.target.clone()).map_err(anyhow::Error::msg)?)
        } else {
            None
        },
        revision,
        resource_recovery: operation.kind == "runtime.launch" && !launch_advance,
        launch_advance,
    };
    let text = serde_json::to_string(&input)?;
    ensure!(text.len() <= 64 * 1024, "brief queue input exceeds bounds");
    let deadline = Instant::now() + BUDGET;
    let mut command = Cmd::new(JOB, BUDGET).stdin(text);
    command.deadline = Some(deadline);
    Ok(Request {
        identity: Identity {
            operation: format!("{}:{}", if launch_advance {"canonical-launch"} else if input.resource_recovery {"canonical-worker:recover"} else {"canonical-worker"}, operation.id.as_str()),
            revision,
            project: project.display().to_string(),
            machine: "local-canonical-terminal".into(),
            terminal: None,
        },
        lane: Lane::Control,
        deadline,
        command,
    })
}
pub struct JobRunner {
    pub inner: Arc<dyn Runner + Send + Sync>,
}
impl Runner for JobRunner {
    fn run(&self, command: &Cmd) -> Result<Output> {
        if command.program != JOB {
            return self.inner.run(command);
        }
        let entered = Instant::now();
        ensure!(
            !command.timeout.is_zero() && command.timeout <= BUDGET,
            "invalid canonical brief queue budget"
        );
        let text = command
            .stdin
            .as_deref()
            .context("brief queue input missing")?;
        ensure!(text.len() <= 64 * 1024, "brief queue input exceeds bounds");
        let input: Input = serde_json::from_str(text)?;
        let deadline = command
            .deadline
            .context("brief queue deadline missing")?
            .min(entered + command.timeout);
        let cancellation = command
            .cancellation
            .clone()
            .context("brief queue cancellation missing")?;
        ensure!(
            !cancellation.is_cancelled() && Instant::now() < deadline,
            "brief queue expired or cancelled"
        );
        ensure!(
            input.project.is_absolute()
                && input.project.canonicalize()? == input.project
                && input.revision > 0,
            "invalid canonical brief project"
        );
        let metadata = std::fs::metadata(&input.project)?;
        ensure!(
            (metadata.dev(), metadata.ino()) == input.identity,
            "canonical brief project replaced"
        );
        ensure!(
            input.preparation.is_none() || input.termination.is_none(),
            "ambiguous worker action"
        );
        ensure!(
            !input.resource_recovery
                || (input.preparation.is_none() && input.termination.is_none()),
            "ambiguous resource recovery action"
        );
        ensure!(!input.launch_advance || (!input.resource_recovery && input.preparation.is_none() && input.termination.is_none()),"ambiguous launch advancement action");
        if input.launch_advance {
            #[cfg(target_os="linux")]
            { herdr_projects::canonical_worker::advance_launch(&input.project,&input.operation,input.revision,deadline,cancellation)?; }
            #[cfg(not(target_os="linux"))]
            anyhow::bail!("canonical launch requires Linux pidfs");
        } else if input.resource_recovery {
            #[cfg(target_os = "linux")]
            {
                herdr_projects::canonical_worker::reconcile_launch(
                    &input.project,
                    &input.operation,
                    input.revision,
                    deadline,
                    cancellation,
                )?;
            }
            #[cfg(not(target_os = "linux"))]
            anyhow::bail!("canonical resource recovery requires Linux pidfs");
        } else if let Some(attempt) = input.preparation {
            herdr_projects::canonical_worker::prepare_brief(
                &input.project,
                &attempt,
                input.revision,
                deadline,
                cancellation,
            )?;
        } else if let Some(attempt) = input.termination {
            #[cfg(target_os = "linux")]
            {
                herdr_projects::canonical_worker::reconcile_termination(
                    &input.project,
                    &attempt,
                    input.revision,
                    deadline,
                    cancellation,
                )?;
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = (attempt, cancellation);
                anyhow::bail!("canonical termination requires Linux pidfs");
            }
        } else {
            herdr_projects::canonical_worker::deliver_brief(
                &input.project,
                &input.operation,
                input.revision,
                deadline,
                cancellation,
            )?;
        }
        Ok(Output {
            code: Some(0),
            elapsed: entered.elapsed(),
            ..Output::default()
        })
    }
    fn socket_request(&self, path: &Path, line: &str, timeout: Duration) -> Result<String> {
        self.inner.socket_request(path, line, timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct NoExternalRunner;
    impl Runner for NoExternalRunner {
        fn run(&self,_:&Cmd)->Result<Output> {panic!("canonical jobs must not delegate effects to caller runner")}
        fn socket_request(&self,_:&Path,_:&str,_:Duration)->Result<String> {panic!("canonical jobs must not delegate sockets to caller runner")}
    }
    #[test]
    fn launch_queue_preserves_deadline_and_rejects_cancelled_or_conflicting_actions() {
        let root=tempfile::tempdir().unwrap();
        let operation=Operation{id:OperationId::new("launch-test").unwrap(),task:None,kind:"runtime.launch".into(),target:"binding".into(),payload_version:1,payload:serde_json::json!({}),expected_revision:1,due_unix_ms:0,idempotency_key:"launch-test".into()};
        let mut request=request_launch(root.path(),&operation,1).unwrap();
        let input:Input=serde_json::from_str(request.command.stdin.as_ref().unwrap()).unwrap();
        assert!(input.launch_advance);assert!(!input.resource_recovery);
        assert_eq!(request.deadline,request.command.deadline.unwrap());
        let runner=JobRunner{inner:Arc::new(NoExternalRunner)};
        let cancellation=crate::runner::Cancellation::default();cancellation.cancel();
        request.command.cancellation=Some(cancellation);
        assert!(runner.run(&request.command).unwrap_err().to_string().contains("expired or cancelled"));
        request.command.cancellation=Some(Default::default());
        let original_deadline=request.command.deadline;
        request.command.deadline=Some(Instant::now()-Duration::from_secs(1));
        assert!(runner.run(&request.command).unwrap_err().to_string().contains("expired or cancelled"));
        request.command.deadline=original_deadline;
        let mut conflict=input;conflict.resource_recovery=true;
        request.command.stdin=Some(serde_json::to_string(&conflict).unwrap());
        assert!(runner.run(&request.command).unwrap_err().to_string().contains("ambiguous launch advancement"));
        let recovery=super::request(root.path(),&operation,1).unwrap();
        let input:Input=serde_json::from_str(recovery.command.stdin.as_ref().unwrap()).unwrap();
        assert!(input.resource_recovery);assert!(!input.launch_advance);
        assert_eq!(recovery.identity.operation,"canonical-worker:recover:launch-test");
        assert_eq!(request.identity.operation,"canonical-launch:launch-test");
    }
}

//! Conservative interpretation of durable legacy completion receipts. No liveness
//! inference, no blind replay, and no promotion of task results to verified success.
use crate::{domain::Operation, store::ImportedSource};
use serde::Serialize;
use sha2::{Digest,Sha256};

#[derive(Debug,Clone,Serialize)]
pub struct ReceiptObservation {
    pub operation: crate::domain::OperationId,
    pub receipt: Option<String>,
    pub blocked: Option<String>,
}
#[derive(Debug,Serialize)]
pub struct ReceiptReport {
    pub previous_head:u64,
    pub head:u64,
    pub confirmed:usize,
    pub observations:Vec<ReceiptObservation>,
}

/// Mirrors the legacy execution identity; missing fields use legacy defaults.
/// Invalid field types refuse rather than inventing an identity.
pub fn legacy_execution_fingerprint(thread:&toml::Value)->Option<String> {
    let mut identity=Vec::new();
    for field in ["id","created","lifecycle_generation","kind","repo","origin","branch","machine","worktree_path","thread_dir","workspace_id","tab_id","pane_id","agent","agent_name","cwd","pr"] {
        if field=="lifecycle_generation" {
            let generation=match thread.get(field) {None=>0,Some(v)=>u64::try_from(v.as_integer()?).ok()?};
            identity.push(serde_json::json!(generation));
        } else {
            let default=if field=="kind" {"worktree"}else{""};
            identity.push(serde_json::json!(match thread.get(field){None=>default,Some(v)=>v.as_str()?}));
        }
    }
    Some(format!("{:x}",Sha256::digest(serde_json::Value::Array(identity).to_string().as_bytes())))
}

pub(crate) struct ReceiptEvidence<'a> {
    sources:std::collections::BTreeMap<&'a str,&'a ImportedSource>,
    ticker:Option<serde_json::Value>,
}
impl<'a> ReceiptEvidence<'a> {
    pub(crate) fn new(sources:&'a [ImportedSource])->Self {
        let sources:std::collections::BTreeMap<_,_>=sources.iter().map(|s|(s.path.as_str(),s)).collect();
        let ticker=sources.get(".state/ticker.json").filter(|s|s.kind=="runtime").and_then(|s|serde_json::from_slice(&s.bytes).ok());
        Self{sources,ticker}
    }
    pub(crate) fn observe(&self,op:&Operation)->ReceiptObservation {
    let receipt=self.receipt(op);
    ReceiptObservation{operation:op.id.clone(),blocked:if receipt.is_none(){Some("no matching durable imported receipt; external observation required".into())}else{None},receipt}
}
fn receipt(&self,op:&Operation)->Option<String> {
    if op.payload_version!=1 || op.expected_revision!=1
        || op.id.as_str()!=super::legacy_id(&op.kind,&op.target)
        || op.idempotency_key!=format!("{}:{}",op.kind,op.target) {return None;}
    let ticker=self.sources.get(".state/ticker.json")?;
    let value=self.ticker.as_ref()?;
    if op.kind=="legacy.notify" {
        if op.task.as_str()!="legacy-project-obligations" || op.target.is_empty()
            || value.get("notification_retry")?!=&op.payload
            || op.payload.get("hash")?.as_str()?!=op.target
            || value.get("nudged")?.as_str()?!=op.target {return None;}
        return Some(format!("legacy-notification:{}:source:{}",op.target,ticker.digest));
    }
    if op.kind!="legacy.finalize" {return None;}
    let id=op.task.as_str().strip_prefix("legacy-")?;
    if value.get("finalizations")?.get(id)?!=&op.payload || op.target.is_empty()
        || op.payload.get("operation_id")?.as_str()?!=op.target {return None;}
    let source=self.sources.get(format!("threads/{id}.toml").as_str()).filter(|s|s.kind=="thread")?;
    let thread:toml::Value=toml::from_str(std::str::from_utf8(&source.bytes).ok()?).ok()?;
    let fingerprint=legacy_execution_fingerprint(&thread)?;
    let pr=op.payload.get("pr")?.as_str()?;
    let reason=op.payload.get("reason")?.as_str()?;
    if pr.is_empty() || reason.is_empty() || thread.get("id")?.as_str()?!=id
        || thread.get("status")?.as_str()?!="resolved"
        || thread.get("last_finalization")?.as_str()?!=op.target
        || thread.get("pr")?.as_str()?!=pr
        || thread.get("resolved_reason")?.as_str()?!=reason
        || op.payload.get("fingerprint")?.as_str()?!=fingerprint {return None;}
    Some(format!("legacy-finalization:{}:source:{}:ticker:{}",op.target,source.digest,ticker.digest))
}
}

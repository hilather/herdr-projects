//! Immutable legacy review-announcement intent, shared with migration.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use crate::copy_receipt::CopyReceipt;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReviewNotice {
    pub id: String,
    pub kind: String,
    pub subject: String,
    pub summary: String,
    pub body: String,
    pub execution: String,
    pub report_hash: String,
    pub sequence: u64,
    pub copy_receipt: Option<CopyReceipt>,
}
impl ReviewNotice {
    pub fn validate(&self, thread: &str, sequence: u64) -> Result<()> {
        ensure!(self.sequence > 0 && self.sequence <= i64::MAX as u64 && self.sequence == sequence, "invalid review notice sequence");
        ensure!(self.execution.len() == 64 && self.execution.bytes().all(|b|b.is_ascii_hexdigit()), "invalid review execution");
        // Historical records have an opaque report-hash string. Typed copy
        // receipts additionally enforce the current SHA-256 representation.
        ensure!(!self.report_hash.is_empty() && self.report_hash.len() <= 256 && !self.report_hash.chars().any(char::is_control), "invalid review report hash");
        ensure!(self.subject == thread && self.kind == "thread-state"
            && self.id == format!("review-{thread}-{}-{sequence}",self.execution), "invalid review notice identity");
        ensure!(!self.summary.is_empty() && self.summary.len() <= 65536 && self.body == self.expected_body(), "invalid review notice payload");
        if let Some(receipt) = &self.copy_receipt {
            receipt.validate()?;
            ensure!(receipt.execution == self.execution && receipt.report_hash == self.report_hash, "review notice receipt mismatch");
        }
        Ok(())
    }
    pub fn expected_body(&self) -> String {
        let copy = self.copy_receipt.as_ref().map_or("legacy copy".into(), |r| format!("copy {}",r.sequence));
        format!("Review report `{}` for execution `{}` ({copy}). The home path `threads/{}.md` is mutable and may now contain a later report; this notice identifies the report at preparation time.",self.report_hash,self.execution,self.subject)
    }
}

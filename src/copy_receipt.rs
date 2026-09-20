//! Durable evidence and warning delivery for legacy live copies.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CopyReceipt {
    pub sequence: u64,
    pub execution: String,
    pub report_hash: String,
    pub notes: Vec<String>,
}
impl CopyReceipt {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.sequence > 0 && self.sequence <= i64::MAX as u64, "invalid copy sequence");
        for hash in [&self.execution, &self.report_hash] {
            ensure!(hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()), "invalid copy receipt hash");
        }
        ensure!(self.notes.len() <= 128 && self.notes.iter().all(|n| n.len() <= 4096)
            && self.notes.iter().map(String::len).sum::<usize>() <= 32768, "copy notes exceed bounds");
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CopyNotice {
    pub id: String,
    pub kind: String,
    pub subject: String,
    pub summary: String,
    pub body: String,
    pub receipt: CopyReceipt,
}
impl CopyNotice {
    pub fn new(thread: &str, receipt: CopyReceipt) -> Result<Self> {
        receipt.validate()?;
        ensure!(!receipt.notes.is_empty(), "copy warning has no notes");
        Ok(Self {
            id: format!("copy-{thread}-{}-{}", receipt.execution, receipt.sequence),
            kind: "copy".into(), subject: thread.into(),
            summary: format!("{thread}: not everything was copied: {}", receipt.notes.join("; ")),
            body: format!("Copied report `{}` for execution `{}` (copy {}). This warning describes that copy, even if the thread has since changed.", receipt.report_hash, receipt.execution, receipt.sequence),
            receipt,
        })
    }
    pub fn validate(&self, thread: &str, receipt: &CopyReceipt) -> Result<()> {
        ensure!(&self.receipt == receipt && self == &Self::new(thread, receipt.clone())?, "invalid copy notice identity or payload");
        Ok(())
    }
}

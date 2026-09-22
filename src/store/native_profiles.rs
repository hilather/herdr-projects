//! Immutable verifier-produced reports. Retrieval never grants launch authority.
use super::*;
use crate::profile_preparation::NativePreparation;
use rusqlite::OptionalExtension;
use std::os::unix::fs::MetadataExt;

fn schema(db: &Connection) -> Result<()> {
    check_schema(db)?;
    let version: u32 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 25 { return Err(StoreError::UnsupportedSchema(version)); }
    Ok(())
}

impl SqliteStore {
    /// Preflight before an expensive probe; upgrades must remain explicit.
    pub fn check_native_profile_retention(&self) -> Result<()> {
        schema(&self.connection)
    }
    /// Called only with the native verifier's sealed, project-bound result.
    pub(crate) fn retain_native_profile(&mut self, prepared: &NativePreparation) -> Result<()> {
        let path = Path::new(self.connection.path().ok_or_else(|| StoreError::Invalid("store path missing".into()))?);
        prepared.check_store(path).map_err(|e| StoreError::Invalid(e.to_string()))?;
        let reference = prepared.preparation.profile.reference().map_err(StoreError::Invalid)?;
        if reference != prepared.preparation.reference {
            return Err(StoreError::Invalid("native profile reference mismatch".into()));
        }
        let report = serde_json::to_string(prepared).map_err(|e| StoreError::Invalid(e.to_string()))?;
        if report.len() > MAX_RECORD_BYTES { return Err(StoreError::Limit("native profile report exceeds limit".into())); }
        let digest = format!("{:x}", Sha256::digest(report.as_bytes()));
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema(&tx)?;
        let old: Option<(String,String)> = tx.query_row(
            "SELECT report,report_digest FROM native_profiles WHERE profile_digest=?1",
            [&reference.digest], |r| Ok((r.get(0)?,r.get(1)?)),
        ).optional()?;
        if let Some((old, old_digest)) = old {
            if old != report || old_digest != digest { return Err(StoreError::Conflict); }
            tx.commit()?;
            return Ok(());
        }
        tx.execute(
            "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('profile.native_retained',?1,1,1,?2)",
            params![reference.id, serde_json::to_string(&reference).map_err(|e|StoreError::Invalid(e.to_string()))?],
        )?;
        tx.execute("INSERT INTO native_profiles VALUES(?1,?2,?3,?4)", params![reference.digest,report,digest,head(&tx)?])?;
        tx.commit()?;
        Ok(())
    }

    /// Read-only audit report. Callers cannot turn this JSON into NativePreparation.
    pub fn native_profile_report(&mut self, reference: &VersionedReference) -> Result<Option<serde_json::Value>> {
        if reference.revision != 1 || reference.digest.len() != 64
            || !reference.digest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || reference.id != format!("profile-{}", reference.digest) {
            return Err(StoreError::Invalid("invalid native profile reference".into()));
        }
        let path = Path::new(self.connection.path().ok_or_else(||StoreError::Invalid("store path missing".into()))?)
            .canonicalize().map_err(|e|StoreError::Io(e.to_string()))?;
        let metadata = std::fs::metadata(&path).map_err(|e|StoreError::Io(e.to_string()))?;
        let tx = self.connection.transaction()?;
        schema(&tx)?;
        let row: Option<(String,String)> = tx.query_row(
            "SELECT report,report_digest FROM native_profiles WHERE profile_digest=?1 AND length(report)<=1048576",
            [&reference.digest], |r| Ok((r.get(0)?,r.get(1)?)),
        ).optional()?;
        let Some((report, digest)) = row else { return Ok(None); };
        if format!("{:x}", Sha256::digest(report.as_bytes())) != digest {
            return Err(StoreError::Corrupt("native profile report digest mismatch".into()));
        }
        let value: serde_json::Value = serde_json::from_str(&report).map_err(|_|StoreError::Corrupt("invalid native profile report".into()))?;
        if value["source_store"] != serde_json::json!([path, metadata.dev(), metadata.ino()]) {
            return Err(StoreError::Invalid("native report belongs to another or replaced project store".into()));
        }
        let profile: FrozenProfile = serde_json::from_value(value["preparation"]["profile"].clone()).map_err(|_|StoreError::Corrupt("invalid retained profile".into()))?;
        if profile.reference().map_err(StoreError::Corrupt)? != *reference
            || value["preparation"]["reference"] != serde_json::to_value(reference).map_err(|e| StoreError::Corrupt(e.to_string()))? {
            return Err(StoreError::Corrupt("retained profile reference mismatch".into()));
        }
        Ok(Some(value))
    }
}

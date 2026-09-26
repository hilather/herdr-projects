//! Signed factory-admission install. The column changes only in this writer.
//! Git is checked before the transaction: a ref read must not sit inside it.
use super::*;
use crate::domain::{AdmissionInstall, PreparedAdmission};
use crate::runner::Runner;
use rusqlite::{params, OptionalExtension};
use sha2::{Digest, Sha256};
use std::path::Path;

const SCHEMA_VERSION: u32 = 30;

fn invalid(message: &str) -> StoreError {
    StoreError::Invalid(message.into())
}
fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn store_path(db: &Connection) -> Result<std::path::PathBuf> {
    let path = db.path().ok_or_else(|| invalid("store path missing"))?;
    std::fs::canonicalize(path).map_err(|error| StoreError::Io(error.to_string()))
}
fn user_version(db: &Connection) -> Result<u32> {
    Ok(db.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

fn git_output(repo: &str, args: &[&str]) -> Result<crate::runner::Output> {
    let mut command = crate::runner::Cmd::repository_git_command(Path::new(repo), args)
        .map_err(|_| invalid("integration ref is missing"))?;
    command.deadline = Some(std::time::Instant::now() + std::time::Duration::from_secs(5));
    crate::runner::RealRunner
        .run(&command)
        .map_err(|_| invalid("integration ref is missing"))
}

fn normal_checkout(repo: &str) -> bool {
    let git = Path::new(repo).join(".git");
    let Ok(meta) = std::fs::symlink_metadata(&git) else {
        return false;
    };
    meta.is_dir() && !meta.file_type().is_symlink()
}

fn ref_checked_out(repo: &str, reference: &str) -> Result<bool> {
    let symbolic = git_output(repo, &["symbolic-ref", "--quiet", "HEAD"])?;
    if symbolic.success() && symbolic.stdout.trim() == reference {
        return Ok(true);
    }
    let listed = git_output(repo, &["worktree", "list", "--porcelain"])?;
    if !listed.success() {
        return Err(invalid("integration ref is missing"));
    }
    Ok(listed
        .stdout
        .lines()
        .any(|line| line == format!("branch {reference}")))
}

fn ref_exists(repo: &str, reference: &str) -> Result<bool> {
    let spec = format!("{reference}^{{commit}}");
    let output = git_output(repo, &["rev-parse", "--verify", "--end-of-options", &spec])?;
    if !output.success() {
        return Ok(false);
    }
    let text = output.stdout.trim();
    Ok(matches!(text.len(), 40 | 64)
        && text.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()))
}

/// Enable only when every configured ref exists and is not checked out.
fn require_integration_refs(db: &Connection) -> Result<()> {
    let mut stmt = db.prepare(
        "SELECT repository, ref_name FROM integration_targets ORDER BY repository",
    )?;
    let rows = stmt
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if rows.is_empty() {
        return Err(invalid("integration ref is missing"));
    }
    for (repository, reference) in rows {
        if !normal_checkout(&repository) || !ref_exists(&repository, &reference)? {
            return Err(invalid("integration ref is missing"));
        }
        if ref_checked_out(&repository, &reference)? {
            return Err(invalid("integration ref is checked out"));
        }
    }
    Ok(())
}

fn evidence_passes(bytes: &[u8], digest: &str) -> Result<()> {
    // Hash the file the operator passed. A reserialized manifest is a different digest.
    if sha256_hex(bytes) != digest {
        return Err(invalid("evidence digest does not match"));
    }
    let manifest: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|_| invalid("admission evidence is not a manifest"))?;
    if manifest.get("vertical_slice").and_then(|value| value.as_str()) != Some("pass") {
        return Err(invalid("vertical slice evidence is not pass"));
    }
    Ok(())
}

impl SqliteStore {
    pub(crate) fn install_admission_policy(
        &mut self,
        prepared: &PreparedAdmission,
        evidence: Option<&[u8]>,
    ) -> Result<AdmissionInstall> {
        let parsed = PreparedAdmission::parse_verified(&prepared.raw).map_err(|error| invalid(&error))?;
        if parsed.digest != prepared.digest || parsed.digest != sha256_hex(&prepared.raw) {
            return Err(invalid("changed admission policy bytes"));
        }
        let path = store_path(&self.connection)?;
        if parsed.project_store != path.to_string_lossy() {
            return Err(invalid("admission policy belongs to another project"));
        }
        let version = user_version(&self.connection)?;
        if version < SCHEMA_VERSION {
            return Err(StoreError::UnsupportedSchema(version));
        }
        if parsed.enabled {
            let evidence = evidence.ok_or_else(|| invalid("admission evidence is required"))?;
            evidence_passes(evidence, &parsed.evidence_digest)?;
            require_integration_refs(&self.connection)?;
        }
        let tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let version = user_version(&tx)?;
        if version < SCHEMA_VERSION {
            return Err(StoreError::UnsupportedSchema(version));
        }
        let existing: Option<(Vec<u8>, String)> = tx
            .query_row(
                "SELECT raw_bytes, evidence_digest FROM factory_admission_policies WHERE policy_digest=?1",
                [&parsed.digest],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let replayed = if let Some((raw, evidence_digest)) = existing {
            if sha256_hex(&raw) != parsed.digest
                || raw != parsed.raw
                || evidence_digest != parsed.evidence_digest
            {
                return Err(StoreError::Corrupt("admission policy digest mismatch".into()));
            }
            true
        } else {
            let now = jiff::Timestamp::now().as_millisecond();
            tx.execute(
                "INSERT INTO factory_admission_policies(policy_digest, project_store, raw_bytes, evidence_digest, created_unix_ms) VALUES(?1,?2,?3,?4,?5)",
                params![parsed.digest, parsed.project_store, parsed.raw, parsed.evidence_digest, now],
            )?;
            false
        };
        let value = if parsed.enabled { "on" } else { "off" };
        let updated = tx.execute(
            "UPDATE project_control SET factory_admission=?1 WHERE singleton=1",
            [value],
        )?;
        if updated != 1 {
            return Err(StoreError::Conflict);
        }
        tx.commit()?;
        Ok(AdmissionInstall {
            enabled: parsed.enabled,
            factory_admission: value.into(),
            policy_digest: parsed.digest,
            replayed,
        })
    }
}

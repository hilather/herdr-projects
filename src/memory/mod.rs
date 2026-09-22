//! Memory service: revisions, snapshots, import and cutover to sqlite-v1.
use std::{fs::{self, File, OpenOptions}, io::{Read, Write}, os::unix::fs::OpenOptionsExt, path::{Path, PathBuf}};
use sha2::{Digest, Sha256};
use crate::domain::*;
use crate::store::{SqliteStore, StoreError};

const OBJECT_LIMIT: u64 = 16 * 1024 * 1024;

// All object publishers and collectors serialize filesystem/row transitions.
// The lock is released by the OS on process death; GC resumes durable claims.
fn object_io_lock(objects: &Path) -> Result<File, MemoryError> {
    fs::create_dir_all(objects).map_err(io_error)?;
    let file = OpenOptions::new().read(true).write(true).create(true).truncate(false)
        .mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(objects.join(".io.lock")).map_err(io_error)?;
    if !file.metadata().map_err(io_error)?.is_file() {
        return Err(MemoryError::Invalid("object lock is not a regular file".into()));
    }
    file.lock().map_err(io_error)?;
    Ok(file)
}
fn io_error(error: std::io::Error) -> MemoryError { MemoryError::Invalid(error.to_string()) }
fn sync_directory(path: &Path) -> Result<(), MemoryError> {
    File::open(path).and_then(|file| file.sync_all()).map_err(io_error)
}
/// Read the entire bounded object and verify its identity before exposing bytes.
pub(crate) fn read_object(objects: &Path, id: &ObjectId) -> Result<Vec<u8>, MemoryError> {
    read_object_with_budget(objects,id,OBJECT_LIMIT)
}
pub(crate) fn read_object_with_budget(objects: &Path, id: &ObjectId, remaining:u64) -> Result<Vec<u8>, MemoryError> {
    let unavailable = || MemoryError::EvidenceUnavailable { object: id.clone() };
    let path = objects.join("sha256").join(&id.as_str()[..2]).join(id.as_str());
    let file = OpenOptions::new().read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path).map_err(|_| unavailable())?;
    let metadata = file.metadata().map_err(|_| unavailable())?;
    if !metadata.is_file() || metadata.len() > OBJECT_LIMIT { return Err(unavailable()); }
    let limit=remaining.min(OBJECT_LIMIT);
    if metadata.len()>limit {return Err(MemoryError::RequiredContentTooLarge {required_bytes:metadata.len(),budget_bytes:limit});}
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes).map_err(|_| unavailable())?;
    if bytes.len() as u64 > limit || bytes.len() as u64 != metadata.len()
        || format!("{:x}", Sha256::digest(&bytes)) != id.as_str() { return Err(unavailable()); }
    Ok(bytes)
}

#[derive(Debug)]
pub enum MemoryError {
    RevisionConflict { current: Vec<(MemoryRecordId, u64)> },
    EvidenceUnavailable { object: ObjectId },
    RequiredContentTooLarge { required_bytes: u64, budget_bytes: u64 },
    AuthorityDenied,
    ScopeConflict,
    IdempotencyKeyConflict,
    UnsupportedSchema,
    Invalid(String),
    Storage(StoreError),
}
impl std::fmt::Display for MemoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "{self:?}") }
}
impl std::error::Error for MemoryError {}
impl From<StoreError> for MemoryError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::Conflict => Self::RevisionConflict { current: Vec::new() },
            StoreError::UnsupportedSchema(_) => Self::UnsupportedSchema,
            StoreError::Invalid(s) if s.contains("idempotency key conflict") => Self::IdempotencyKeyConflict,
            StoreError::Invalid(s) if s.contains("missing object") => Self::EvidenceUnavailable { object: ObjectId::from_hex("0".repeat(64)).unwrap() },
            other => Self::Storage(other),
        }
    }
}

mod retrieval;
mod snapshot;
mod import;
mod checkpoint;
mod delivery;
mod worker_brief;
pub use worker_brief::{WorkerBrief, render_attempt_brief, enqueue_attempt_brief};
pub(crate) use worker_brief::{render_attempt_brief_held, preview_worker_brief};
pub(crate) use worker_brief::{WORKER_BRIEF_ESTIMATOR, framing_chars as worker_brief_framing_chars};
pub use delivery::*;
mod proposals;
mod review;
pub use retrieval::*;
pub use import::*;
pub use checkpoint::*;

/// Hold project operation exclusion across a complete CLI memory mutation.
pub struct MemoryMutationGuard { _guard: crate::migration::Maintenance }
pub fn mutation_guard(project: &Path) -> anyhow::Result<MemoryMutationGuard> {
    Ok(MemoryMutationGuard { _guard: crate::migration::runtime_mutation(project)? })
}

/// Memory changes enter through proposal review or owner-signed policy services.
/// Raw revision writes are not a public authority bypass.
///
/// ```compile_fail
/// use herdr_projects::{memory::MemoryStore, domain::{ControlContext, NewRevision}};
/// fn unsigned_write(store: &mut MemoryStore, next: NewRevision) {
///     store.insert_revision(&ControlContext { now_unix_ms: 1 }, next).unwrap();
/// }
/// ```
pub struct MemoryStore { pub(crate) store: SqliteStore, pub(crate) objects: PathBuf }
impl MemoryStore {
    pub fn from_sqlite(store: SqliteStore, objects: PathBuf) -> Self { Self { store, objects } }
    pub fn ingest_object<R: Read>(&mut self, mut bytes: R) -> Result<ObjectId, MemoryError> {
        let _lock = object_io_lock(&self.objects)?;
        // Exclusive creation also protects against abandoned staging files after a crash.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let (tmp, mut file) = loop {
            let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = self.objects.join(format!(".ingest-{}-{serial}.tmp", std::process::id()));
            match OpenOptions::new().write(true).create_new(true).mode(0o600)
                .custom_flags(libc::O_NOFOLLOW).open(&path) {
                Ok(file) => break (path, file),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(io_error(error)),
            }
        };
        let mut digest = Sha256::new();
        let mut size = 0u64;
        let mut buf = [0u8; 65536];
        let result = (|| -> Result<ObjectId, MemoryError> {
            loop {
                let n = bytes.read(&mut buf).map_err(|e| MemoryError::Invalid(e.to_string()))?;
                if n == 0 { break; }
                size = size.checked_add(n as u64).filter(|n| *n <= OBJECT_LIMIT).ok_or_else(|| MemoryError::Invalid("object exceeds 16 MiB".into()))?;
                digest.update(&buf[..n]);
                file.write_all(&buf[..n]).map_err(|e| MemoryError::Invalid(e.to_string()))?;
            }
            file.sync_all().map_err(|e| MemoryError::Invalid(e.to_string()))?;
            let hash = format!("{:x}", digest.finalize());
            let id = ObjectId::from_hex(hash.clone()).map_err(MemoryError::Invalid)?;
            let prefix = self.objects.join("sha256").join(&hash[..2]);
            fs::create_dir_all(&prefix).map_err(|e| MemoryError::Invalid(e.to_string()))?;
            let dest = prefix.join(&hash);
            if dest.try_exists().map_err(io_error)? {
                read_object(&self.objects, &id)?;
                fs::remove_file(&tmp).map_err(io_error)?;
            } else {
                fs::rename(&tmp, &dest).map_err(|e| MemoryError::Invalid(e.to_string()))?;
            }
            sync_directory(&prefix)?;
            sync_directory(&self.objects.join("sha256"))?;
            sync_directory(&self.objects)?;
            self.store.upsert_object(id.as_str(), size as i64).map_err(MemoryError::from)?;
            Ok(id)
        })();
        if result.is_err() { let _ = fs::remove_file(&tmp); }
        result
    }
    pub fn pin(&mut self, id: &ObjectId) -> Result<(), MemoryError> { self.store.pin_object(id.as_str()).map_err(Into::into) }
    pub(crate) fn insert_revision(&mut self, _ctx: &ControlContext, rec: NewRevision) -> Result<MemoryHead, MemoryError> {
        read_object(&self.objects, &rec.body_hash)?;
        read_object(&self.objects, &rec.provenance_hash)?;
        match self.store.insert_memory_revision(&rec) {
            Ok((head,_)) => Ok(head),
            Err(StoreError::Invalid(s)) if s.contains("missing object") => Err(MemoryError::EvidenceUnavailable { object: rec.body_hash }),
            Err(error) => Err(error.into()),
        }
    }
    #[cfg(test)]
    pub(crate) fn revoke(&mut self, ctx: &ControlContext, id: &MemoryRecordId, expected: u64) -> Result<(), MemoryError> {
        self.store.revoke_memory(id, expected, ctx.now_unix_ms).map_err(Into::into)
    }
    pub fn active_facts(&mut self, now: i64) -> Result<Vec<ActiveFact>, MemoryError> {
        self.store.active_facts(now).map_err(Into::into)
    }

    pub fn collect_unreferenced(&mut self) -> Result<usize, MemoryError> {
        let _lock = object_io_lock(&self.objects)?;
        let claimed = self.store.claim_gc(128).map_err(MemoryError::from)?;
        let mut deleted = 0;
        for (hash, token) in claimed {
            if !self.store.begin_gc_delete(&hash, token).map_err(MemoryError::from)? { continue; }
            let path = self.objects.join("sha256").join(&hash[..2]).join(&hash);
            match fs::remove_file(&path) {
                Ok(()) => sync_directory(path.parent().unwrap())?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
                Err(error) => return Err(io_error(error)),
            }
            self.store.finish_gc_purge(&hash, token).map_err(MemoryError::from)?;
            deleted += 1;
        }
        Ok(deleted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, MemoryStore) {
        let root = tempfile::tempdir().unwrap();
        let db = root.path().join("state.db");
        let store = SqliteStore::create(&db).unwrap();
        (root, MemoryStore::from_sqlite(store, db.parent().unwrap().join("objects")))
    }
    fn ctx() -> ControlContext { ControlContext { now_unix_ms: 1_000 } }
    fn revision(id: &str, body: ObjectId, provenance: ObjectId, expected: Option<u64>) -> NewRevision {
        NewRevision {
            id: MemoryRecordId::new(id).unwrap(), record_key: format!("memory/{id}.md"), scope_id: "project".into(),
            kind: MemoryKind::Observation, body_hash: body, provenance_hash: provenance,
            applicability: Applicability { domains: vec!["ui".into()], paths: vec![format!("memory/{id}.md")] },
            dependencies: vec![], expected, expiry_unix_ms: None, validity_state: String::new(), validity_reason: String::new(),
        }
    }
    #[test]
    fn concurrent_cas_has_one_winner_and_hashes_survive_reopen() {
        let (root, mut memory) = fixture();
        let body = memory.ingest_object(&b"body"[..]).unwrap();
        let prov = memory.ingest_object(&b"prov"[..]).unwrap();
        let first = memory.insert_revision(&ctx(), revision("api", body.clone(), prov.clone(), None)).unwrap();
        assert_eq!(first.revision, 1);
        let db = root.path().join("state.db");
        let mut a = MemoryStore::from_sqlite(SqliteStore::open(&db).unwrap(), memory.objects.clone());
        let mut b = MemoryStore::from_sqlite(SqliteStore::open(&db).unwrap(), memory.objects.clone());
        let next = revision("api", body.clone(), prov.clone(), Some(1));
        let ra = a.insert_revision(&ctx(), next.clone());
        let rb = b.insert_revision(&ctx(), next);
        let wins = [&ra, &rb].iter().filter(|r| r.is_ok()).count();
        let conflicts = [&ra, &rb].iter().filter(|r| matches!(r, Err(MemoryError::RevisionConflict { .. }))).count();
        assert_eq!(wins, 1); assert_eq!(conflicts, 1);
        drop(a); drop(b);
        let mut reopened = MemoryStore::from_sqlite(SqliteStore::open(&db).unwrap(), memory.objects.clone());
        let facts = reopened.active_facts(1_000).unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].revision.body_hash.as_str(), body.as_str());
        assert_eq!(facts[0].revision.applicability.domains, ["ui"]);
    }
    #[test]
    fn missing_expiry_and_revocation_are_not_active_facts() {
        let (_root, mut memory) = fixture();
        let body = memory.ingest_object(&b"body"[..]).unwrap();
        let prov = memory.ingest_object(&b"prov"[..]).unwrap();
        let missing = ObjectId::from_hex("ab".repeat(32)).unwrap();
        assert!(matches!(memory.insert_revision(&ctx(), revision("gone", missing, prov.clone(), None)), Err(MemoryError::EvidenceUnavailable { .. })));
        let mut expired = revision("old", body.clone(), prov.clone(), None); expired.expiry_unix_ms = Some(10);
        memory.insert_revision(&ctx(), expired).unwrap();
        assert!(memory.active_facts(11).unwrap().is_empty());
        assert_eq!(memory.active_facts(5).unwrap().len(), 1);
        let live = memory.insert_revision(&ctx(), revision("live", body, prov, None)).unwrap();
        memory.revoke(&ctx(), &MemoryRecordId::new("live").unwrap(), live.revision).unwrap();
        assert!(memory.active_facts(1_000).unwrap().iter().all(|f| f.record.id.as_str() != "live"));
    }
    #[test]
    fn ingest_cancels_gc_claim_and_unreferenced_objects_are_purged() {
        let (_root, mut memory) = fixture();
        let body = memory.ingest_object(&b"collect-me"[..]).unwrap();
        let prov = memory.ingest_object(&b"prov"[..]).unwrap();
        let head = memory.insert_revision(&ctx(), revision("tmp", body.clone(), prov.clone(), None)).unwrap();
        memory.revoke(&ctx(), &MemoryRecordId::new("tmp").unwrap(), head.revision).unwrap();
        // Still referenced by immutable revision history, so GC must not delete.
        assert_eq!(memory.collect_unreferenced().unwrap(), 0);
        let orphan = memory.ingest_object(&b"orphan-bytes"[..]).unwrap();
        assert_eq!(memory.collect_unreferenced().unwrap(), 1);
        let path = memory.objects.join("sha256").join(&orphan.as_str()[..2]).join(orphan.as_str());
        assert!(!path.exists());
    }
}

#[cfg(test)]
mod regression_tests;

pub(crate) use worker_brief::enqueue_attempt_brief_controlled;

pub(crate) use import::render_knowledge_snapshot_held;

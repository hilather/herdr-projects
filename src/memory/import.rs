//! Legacy Markdown import, memory-journal cutover, and projection rendering.
use super::{MemoryError, MemoryStore};
use crate::domain::*;
use crate::migration::{self, Format, Phase, MEMORY_LEGACY, MEMORY_SQLITE, hash, memory_journal_path, read, write_new, sync_dir, exists, safe_relative, safe_join, checked_project, memory_owner_ok};
use anyhow::{Context, Result, ensure, bail};
use serde::{Deserialize, Serialize};

use std::{fs::{self, OpenOptions}, io::Read, os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt}, path::Path};

const FILE_LIMIT: u64 = 64 * 1024;
const TOTAL_LIMIT: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemorySource {
    pub path: String,
    pub digest: String,
    pub bytes: u64,
    pub headings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryPlan {
    pub version: u32,
    pub project: String,
    pub digest: String,
    pub sources: Vec<MemorySource>,
    pub expected_memory_owner: String,
    pub runtime_migration: String,
    pub destination_schema: u32,
    pub owner_policy_digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryJournal {
    pub version: u32,
    pub phase: Phase,
    pub plan: MemoryPlan,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportResult {
    pub record_id: String,
    pub record_key: String,
    pub revision: u64,
    pub body_hash: String,
    pub reused: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryPreview {
    pub record_key: String,
    pub current_revision: Option<u64>,
    pub current_digest: Option<String>,
    pub file_digest: String,
    pub conflict: bool,
}

fn read_memory_file(path: &Path) -> Result<Vec<u8>> {
    let file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?;
    ensure!(file.metadata()?.is_file(), "{} is not a regular file", path.display());
    let mut bytes = Vec::new();
    file.take(FILE_LIMIT + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= FILE_LIMIT, "{} exceeds 64 KiB", path.display());
    ensure!(!bytes.contains(&0), "{} contains a NUL byte", path.display());
    std::str::from_utf8(&bytes).context("memory file is not UTF-8")?;
    Ok(bytes)
}

fn headings(text: &str) -> Vec<String> {
    text.lines().filter_map(|line| {
        let line = line.trim_end();
        let rest = line.strip_prefix('#')?;
        let level = 1 + rest.chars().take_while(|c| *c == '#').count();
        if level > 6 { return None; }
        let title = rest.trim_start_matches('#').strip_prefix(' ')?.trim();
        if title.is_empty() { None } else { Some(title.to_string()) }
    }).collect()
}

fn record_id_for(path: &str) -> Result<MemoryRecordId, MemoryError> {
    MemoryRecordId::new(path.replace('/', ".")).map_err(MemoryError::Invalid)
}

fn refuse_special(path: &Path, name: &str) -> Result<()> {
    let meta = fs::symlink_metadata(path).with_context(|| format!("inspect {name}"))?;
    ensure!(!meta.file_type().is_symlink(), "symlink memory source blocks import: {name}");
    ensure!(meta.is_file(), "memory source is not a regular file: {name}");
    ensure!(!meta.file_type().is_fifo() && !meta.file_type().is_socket() && !meta.file_type().is_block_device() && !meta.file_type().is_char_device(), "unsupported memory source type: {name}");
    Ok(())
}

fn inventory(project: &Path) -> Result<Vec<MemorySource>> {
    let mut sources = Vec::new();
    let mut total = 0u64;
    let index = project.join("MEMORY.md");
    if exists(&index) {
        refuse_special(&index, "MEMORY.md")?;
        let bytes = read_memory_file(&index)?;
        total = total.checked_add(bytes.len() as u64).filter(|n| *n <= TOTAL_LIMIT).context("memory inventory exceeds 1 MiB")?;
        let text = std::str::from_utf8(&bytes)?;
        sources.push(MemorySource { path: "MEMORY.md".into(), digest: hash(&bytes), bytes: bytes.len() as u64, headings: headings(text) });
    }
    let dir = project.join("memory");
    if exists(&dir) {
        ensure!(fs::symlink_metadata(&dir)?.is_dir(), "memory/ must be a real directory");
        let mut entries = fs::read_dir(&dir)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name();
            let name = name.to_str().context("non-UTF-8 memory filename")?;
            ensure!(!name.starts_with('.'), "hidden memory file blocks import: {name}");
            ensure!(name.ends_with(".md"), "unsupported memory filename: {name}");
            ensure!(!name.contains('/') && !name.contains('\\'), "unsafe memory filename");
            let rel = format!("memory/{name}");
            ensure!(safe_relative(&rel), "unsafe memory path");
            refuse_special(&entry.path(), &rel)?;
            let bytes = read_memory_file(&entry.path())?;
            total = total.checked_add(bytes.len() as u64).filter(|n| *n <= TOTAL_LIMIT).context("memory inventory exceeds 1 MiB")?;
            let text = std::str::from_utf8(&bytes)?;
            sources.push(MemorySource { path: rel, digest: hash(&bytes), bytes: bytes.len() as u64, headings: headings(text) });
        }
    }
    Ok(sources)
}

/// Read-only inventory bound to the current runtime format marker.
pub fn plan(project: &Path) -> Result<MemoryPlan> {
    let project = checked_project(project)?;
    let mut db = migration::open_active(&project)?;
    let snapshot = db.read_snapshot(None)?;
    ensure!(snapshot.schema_version >= 18, "memory import requires schema 18 or newer");
    let marker = migration::read_format(&project)?;
    ensure!(marker.memory == MEMORY_LEGACY, "memory plan requires format.memory=legacy-markdown");
    ensure!(memory_owner_ok(&marker.memory), "unknown memory owner");
    let digest = snapshot.control.as_ref().and_then(|c| c.config_digest.clone()).unwrap_or_default();
    let sources = inventory(&project)?;
    let encoded = serde_json::to_vec(&sources)?;
    Ok(MemoryPlan {
        version: 1,
        project: project.to_str().context("non-UTF-8 project path")?.into(),
        digest: hash(&encoded),
        sources,
        expected_memory_owner: MEMORY_LEGACY.into(),
        runtime_migration: marker.migration,
        destination_schema: snapshot.schema_version,
        owner_policy_digest: digest,
    })
}

fn save_journal(project: &Path, journal: &MemoryJournal) -> Result<()> {
    let path = memory_journal_path(project);
    let tmp = path.with_extension("memory-next");
    if exists(&tmp) {
        ensure!(fs::symlink_metadata(&tmp)?.is_file(), "invalid memory journal temporary file");
        fs::remove_file(&tmp)?;
    }
    write_new(&tmp, &serde_json::to_vec_pretty(journal)?)?;
    fs::rename(tmp, &path)?;
    sync_dir(path.parent().context("no parent")?)
}

fn load_journal(project: &Path) -> Result<MemoryJournal> {
    let journal: MemoryJournal = serde_json::from_slice(&read(&memory_journal_path(project))?)?;
    ensure!(journal.version == 1, "unknown memory journal version");
    ensure!(Path::new(&journal.plan.project) == project, "memory journal project identity mismatch");
    ensure!(journal.plan.expected_memory_owner == MEMORY_LEGACY, "memory journal expected owner mismatch");
    Ok(journal)
}

fn objects_dir(project: &Path) -> std::path::PathBuf { project.join(".state/objects") }

use super::read_object;

fn import_bytes(memory: &mut MemoryStore, rel: &str, bytes: &[u8], migration_id: &str, expected: Option<u64>) -> Result<ImportResult, MemoryError> {
    if rel != "MEMORY.md" && !rel.starts_with("memory/") {
        return Err(MemoryError::Invalid("import path must be MEMORY.md or memory/*.md".into()));
    }
    if !safe_relative(rel) { return Err(MemoryError::Invalid("unsafe import path".into())); }
    let digest = hash(bytes);
    if let Some(existing)=memory.store.memory_record_by_key(rel)? {
        let head=memory.store.memory_head(existing.id.as_str())?.ok_or_else(||MemoryError::Invalid("import head missing".into()))?;
        if expected.is_some_and(|e|e!=head.revision) {return Err(MemoryError::RevisionConflict{current:vec![(existing.id,head.revision)]});}
        let revision=memory.store.memory_revision(existing.id.as_str(),head.revision)?.ok_or_else(||MemoryError::Invalid("import revision missing".into()))?;
        if !memory.store.shadow_import_replaceable(existing.id.as_str())? {
            if revision.body_hash.as_str()==digest {
                return Ok(ImportResult{record_id:existing.id.as_str().into(),record_key:rel.into(),revision:head.revision,body_hash:digest,reused:true});
            }
            return Err(MemoryError::Invalid("approved memory requires a staged import and signed review".into()));
        }
    }
    let provenance = serde_json::json!({"path": rel, "digest": digest, "span": [0, bytes.len()], "memory_migration_id": migration_id});
    let body = memory.ingest_object(bytes)?;
    let prov = memory.ingest_object(provenance.to_string().as_bytes())?;
    memory.pin(&body)?;
    memory.pin(&prov)?;
    let id = record_id_for(rel)?;
    if let Ok(Some(existing)) = memory.store.memory_record_by_key(rel) {
        if let Ok(Some(head)) = memory.store.memory_head(existing.id.as_str()) {
            if let Ok(Some(rev)) = memory.store.memory_revision(existing.id.as_str(), head.revision) {
                if rev.body_hash.as_str() == body.as_str() {
                    if let Ok(prev) = read_object(&memory.objects, &rev.provenance_hash) {
                        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&prev) {
                            if value.get("memory_migration_id").and_then(|v| v.as_str()) == Some(migration_id)
                                && value.get("digest").and_then(|v| v.as_str()) == Some(digest.as_str()) {
                                return Ok(ImportResult { record_id: existing.id.as_str().into(), record_key: rel.into(), revision: head.revision, body_hash: body.as_str().into(), reused: true });
                            }
                        }
                    }
                }
            }
        }
    }
    let expected = match expected {
        Some(n) => Some(n),
        None => memory.store.memory_record_by_key(rel).ok().flatten()
            .and_then(|existing| memory.store.memory_head(existing.id.as_str()).ok().flatten().map(|h| h.revision)),
    };
    let head = memory.insert_revision(&ControlContext { now_unix_ms: jiff::Timestamp::now().as_millisecond() }, NewRevision {
        id: id.clone(), record_key: rel.into(), scope_id: "project".into(), kind: MemoryKind::Observation,
        body_hash: body.clone(), provenance_hash: prov,
        applicability: Applicability { domains: vec![], paths: vec![rel.into()] },
        dependencies: vec![], expected, expiry_unix_ms: None,
        validity_state: "stale".into(), validity_reason: "unverified_import".into(),
    })?;
    Ok(ImportResult { record_id: id.as_str().into(), record_key: rel.into(), revision: head.revision, body_hash: body.as_str().into(), reused: false })
}

fn contained_relative(project: &Path, file: &Path) -> Result<String> {
    let project = project.canonicalize()?;
    let meta = fs::symlink_metadata(file).context("import file missing")?;
    ensure!(!meta.file_type().is_symlink(), "import file must not be a symlink");
    let canonical = if meta.is_file() {
        file.parent().context("import file has no parent")?.canonicalize()?.join(file.file_name().context("import file has no name")?)
    } else {
        bail!("import file is not a regular file");
    };
    let rel = canonical.strip_prefix(&project).map_err(|_| anyhow::anyhow!("import file is outside the project"))?;
    let rel = rel.to_str().context("non-UTF-8 import path")?;
    ensure!(rel == "MEMORY.md" || (rel.starts_with("memory/") && rel.ends_with(".md") && rel.matches('/').count() == 1), "import path must be MEMORY.md or memory/*.md");
    ensure!(safe_relative(rel), "unsafe import path");
    Ok(rel.into())
}

pub fn import_file(project: &Path, file: &Path, expected_revision: Option<u64>) -> Result<MemoryImportCandidate> {
    let project = checked_project(project)?;
    let _guard = migration::runtime_mutation(&project)?;
    let rel = contained_relative(&project, file)?;
    let bytes = read_memory_file(&project.join(&rel))?;
    let mut memory = MemoryStore::from_sqlite(migration::open_active(&project)?, objects_dir(&project));
    let existing = memory.store.memory_record_by_key(&rel)?;
    if existing.is_some() { ensure!(expected_revision.is_some(), "manual edit requires --expected-revision from memory preview"); }
    let id = match existing { Some(r)=>r.id, None=>record_id_for(&rel)? };
    let body = memory.ingest_object(bytes.as_slice())?;
    let provenance = memory.ingest_object(serde_json::json!({"path":rel,"body":body,"expected_revision":expected_revision,"source":"manual_import_candidate"}).to_string().as_bytes())?;
    let candidate = MemoryImportCandidate {
        id:format!("candidate-{}",hash(serde_json::json!([id,expected_revision,body,provenance]).to_string().as_bytes())),
        record_id:id, record_key:rel, expected_revision, body_hash:body, provenance_hash:provenance,
        created_unix_ms:jiff::Timestamp::now().as_millisecond(),
    };
    Ok(memory.store.stage_memory_import(&candidate)?)
}

/// Preview retained bytes, independent of later edits to the source file.
pub fn import_candidate_preview(project: &Path, id: &str) -> Result<serde_json::Value> {
    let mut db = migration::open_active(project)?;
    let candidate = db.memory_import_candidate(id)?.context("import candidate missing")?;
    let proposed = String::from_utf8(read_object(&objects_dir(project),&candidate.body_hash)?)?;
    let original = match candidate.expected_revision {
        Some(revision) => {
            let old=db.memory_revision(candidate.record_id.as_str(),revision)?.context("candidate base missing")?;
            Some(String::from_utf8(read_object(&objects_dir(project),&old.body_hash)?)?)
        }, None=>None,
    };
    Ok(serde_json::json!({"candidate":candidate,"original":original,"proposed":proposed}))
}

fn import_plan_held(project: &Path, plan: &MemoryPlan) -> Result<Vec<ImportResult>> {
    ensure!(Path::new(&plan.project) == project, "memory plan project mismatch");
    let current = crate::memory::plan(project)?;
    ensure!(serde_json::to_value(&current)? == serde_json::to_value(plan)?, "memory plan does not match the current inventory; regenerate the plan");
    let mut journal = MemoryJournal { version: 1, phase: Phase::Prepared, plan: plan.clone() };
    save_journal(project, &journal)?;
    let mut memory = MemoryStore::from_sqlite(migration::open_active(project)?, objects_dir(project));
    let mut results = Vec::new();
    for source in &plan.sources {
        let bytes = read_memory_file(&project.join(&source.path))?;
        ensure!(hash(&bytes) == source.digest, "memory source changed during import: {}", source.path);
        results.push(import_bytes(&mut memory, &source.path, &bytes, &plan.digest, None)?);
    }
    journal.phase = Phase::Imported;
    save_journal(project, &journal)?;
    Ok(results)
}

pub fn import_plan(project: &Path, plan: &MemoryPlan) -> Result<Vec<ImportResult>> {
    let project = checked_project(project)?;
    let _guard = migration::runtime_mutation(&project)?;
    import_plan_held(&project, plan)
}

pub fn preview(project: &Path, file: &Path) -> Result<MemoryPreview> {
    let project = checked_project(project)?;
    let rel = contained_relative(&project, file)?;
    let bytes = read_memory_file(&project.join(&rel))?;
    let file_digest = hash(&bytes);
    let mut db = migration::open_active(&project)?;
    let current = db.memory_record_by_key(&rel)?;
    let (current_revision, current_digest) = if let Some(rec) = current {
        let head = db.memory_head(rec.id.as_str())?;
        let rev = head.as_ref().and_then(|h| db.memory_revision(rec.id.as_str(), h.revision).ok().flatten());
        (head.map(|h| h.revision), rev.map(|r| r.body_hash.as_str().to_string()))
    } else { (None, None) };
    let conflict = current_digest.as_ref().is_some_and(|d| d != &file_digest);
    Ok(MemoryPreview { record_key: rel, current_revision, current_digest, file_digest, conflict })
}

fn backup_memory(project: &Path, plan: &MemoryPlan) -> Result<()> {
    let dest = project.join(".state/migration/memory-backup");
    if !exists(&dest) {
        fs::create_dir(&dest)?;
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o700))?;
    }
    for source in &plan.sources {
        let bytes = read_memory_file(&project.join(&source.path))?;
        ensure!(hash(&bytes) == source.digest, "source changed before memory backup: {}", source.path);
        let path = safe_join(&dest, &source.path)?;
        fs::create_dir_all(path.parent().unwrap())?;
        if !exists(&path) { write_new(&path, &bytes)?; }
        ensure!(hash(&read(&path)?) == source.digest, "memory backup verification failed: {}", source.path);
    }
    sync_dir(&dest)
}

fn set_memory_owner(project: &Path, memory: &str) -> Result<()> {
    ensure!(memory_owner_ok(memory), "unknown memory owner");
    let path = project.join(".state/format.json");
    let mut marker: Format = serde_json::from_slice(&read(&path)?)?;
    ensure!(marker.memory == MEMORY_LEGACY, "cutover requires format.memory=legacy-markdown");
    marker.memory = memory.into();
    let temporary = project.join(".state/migration/memory-format.next");
    if exists(&temporary) {
        ensure!(fs::symlink_metadata(&temporary)?.is_file(), "invalid memory marker temporary");
        fs::remove_file(&temporary)?;
    }
    write_new(&temporary, &serde_json::to_vec_pretty(&marker)?)?;
    fs::rename(temporary, path)?;
    sync_dir(&project.join(".state"))?;
    sync_dir(&project.join(".state/migration"))?;
    Ok(())
}

fn projection_text(record_id: &str, revision: u64, sequence: u64, fingerprint: &str, body: &str) -> String {
    format!("<!-- herdr-projects memory projection; record={record_id}; revision={revision}; sequence={sequence}; fingerprint={fingerprint} -->\n<!-- Do not edit this file. Submit candidates with `herdr-projects memory import --file` and signed memory@ review. -->\n\n{body}")
}

fn publish_projection(path: &Path, bytes: &[u8], original: Option<&[u8]>) -> Result<()> {
    if exists(path) {
        let existing = read_memory_file(path).or_else(|_| read(path))?;
        if existing == bytes { return Ok(()); }
        if original.is_some_and(|o| o == existing.as_slice()) {
            let tmp = path.with_extension("projection-next");
            if exists(&tmp) { ensure!(fs::symlink_metadata(&tmp)?.is_file(), "invalid projection temporary"); fs::remove_file(&tmp)?; }
            write_new(&tmp, bytes)?;
            fs::rename(tmp, path)?;
            if let Some(parent) = path.parent() { sync_dir(parent)?; }
            return Ok(());
        }
        bail!("edited memory projection preserved at {}; export/import review required", path.display());
    }
    if let Some(parent) = path.parent() {
        if !exists(parent) {
            fs::create_dir(parent)?;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
    }
    write_new(path, bytes)
}

fn render_projections(project: &Path, plan: &MemoryPlan, sequence: u64) -> Result<()> {
    let mut memory = MemoryStore::from_sqlite(migration::open_active(project)?, objects_dir(project));
    for source in &plan.sources {
        let rec = memory.store.memory_record_by_key(&source.path)?.context("imported memory record missing")?;
        let head = memory.store.memory_head(rec.id.as_str())?.context("imported memory head missing")?;
        let rev = memory.store.memory_revision(rec.id.as_str(), head.revision)?.context("imported memory revision missing")?;
        let body = read_object(&memory.objects, &rev.body_hash).map_err(|e| anyhow::anyhow!("{e}"))?;
        let text = std::str::from_utf8(&body).context("imported memory body is not UTF-8")?;
        let rendered = projection_text(rec.id.as_str(), head.revision, sequence, rev.body_hash.as_str(), text);
        let original = read(&safe_join(&project.join(".state/migration/memory-backup"), &source.path)?).ok();
        publish_projection(&project.join(&source.path), rendered.as_bytes(), original.as_deref())?;
    }
    Ok(())
}

/// Switch `format.memory` to sqlite-v1. Runtime journal remains Active.
pub fn cutover(project: &Path, plan: &MemoryPlan, policy: &PreparedMemoryPolicy, writers_stopped: bool) -> Result<MemoryJournal> {
    ensure!(writers_stopped, "confirm known writers are stopped with --writers-stopped");
    let project = checked_project(project)?;
    let _maintenance = migration::maintenance(&project)?;
    ensure!(policy.policy.op == MemoryPolicyOp::Cutover, "cutover requires a signed cutover document");
    ensure!(policy.policy.memory_plan_digest.as_deref() == Some(plan.digest.as_str()), "cutover memory_plan_digest does not match the plan");
    ensure!(policy.policy.expected_memory_owner.as_deref() == Some(MEMORY_LEGACY), "cutover expected_memory_owner must be legacy-markdown");
    let marker = migration::read_format(&project)?;
    ensure!(marker.migration == plan.runtime_migration, "W03 migration digest changed; regenerate the memory plan");
    ensure!(exists(&memory_journal_path(&project)), "import memory before cutover");
    let mut journal = load_journal(&project)?;
    ensure!(journal.plan == *plan, "memory journal plan mismatch");
    let snapshot=migration::open_active(&project)?.read_snapshot(None)?;
    let installed=snapshot.memory_policies.iter().find(|p|p.revision==policy.policy.revision);
    if let Some(installed)=installed {ensure!(installed==&policy.policy,"cutover authorization does not match committed policy");}
    if journal.phase==Phase::Active {
        ensure!(installed.is_some() && marker.memory==MEMORY_SQLITE,"active cutover receipt or owner missing");
        return Ok(journal);
    }
    ensure!(matches!(journal.phase, Phase::Imported | Phase::Verified | Phase::CutoverPending), "memory journal is not ready for cutover");
    if installed.is_none() {
        ensure!(marker.memory == MEMORY_LEGACY, "memory authority changed without the signed cutover receipt");
        let current = crate::memory::plan(&project)?;
        ensure!(current == *plan, "memory plan does not match current inventory; regenerate the plan");
        backup_memory(&project, plan)?;
    } else {
        ensure!(journal.phase==Phase::CutoverPending,"committed cutover has no pending recovery journal");
    }
    for source in &plan.sources {
        let bytes = read(&safe_join(&project.join(".state/migration/memory-backup"), &source.path)?)?;
        ensure!(hash(&bytes) == source.digest, "memory backup changed: {}", source.path);
    }
    // Verify the complete imported inventory before changing either policy or owner.
    let mut verification = migration::open_active(&project)?;
    for source in &plan.sources {
        let record = verification.memory_record_by_key(&source.path)?.context("imported record missing")?;
        let head = verification.memory_head(record.id.as_str())?.context("imported head missing")?;
        ensure!(head.status == "active", "imported head is not active");
        let revision = verification.memory_revision(record.id.as_str(), head.revision)?.context("imported revision missing")?;
        let body = read_object(&objects_dir(&project), &revision.body_hash)?;
        ensure!(hash(&body) == source.digest, "imported body differs from planned source");
        std::str::from_utf8(&body).context("imported body is not UTF-8")?;
        let provenance = read_object(&objects_dir(&project), &revision.provenance_hash)?;
        let provenance: serde_json::Value = serde_json::from_slice(&provenance)?;
        ensure!(provenance["path"].as_str() == Some(source.path.as_str())
            && provenance["digest"].as_str() == Some(source.digest.as_str())
            && provenance["memory_migration_id"].as_str() == Some(plan.digest.as_str()), "imported provenance does not match plan");
    }
    drop(verification);
    if journal.phase == Phase::Imported {
        journal.phase = Phase::Verified;
        save_journal(&project, &journal)?;
    }
    if journal.phase == Phase::Verified {
        journal.phase = Phase::CutoverPending;
        save_journal(&project, &journal)?;
    }
    let mut db = migration::open_active(&project)?;
    let snapshot = db.read_snapshot(None)?;
    ensure!(snapshot.schema_version >= 18, "memory cutover requires schema 18 or newer");
    if installed.is_none() {db.install_memory_policy(policy, snapshot.head)?;}
    if migration::read_format(&project)?.memory==MEMORY_LEGACY {set_memory_owner(&project, MEMORY_SQLITE)?;}
    let marker = migration::read_format(&project)?;
    ensure!(marker.memory == MEMORY_SQLITE && marker.runtime == "sqlite-v2" && marker.migration == plan.runtime_migration, "memory owner publication interrupted");
    // The policy event is the stable publication identity across every retry.
    let sequence = policy.policy.expected_head.checked_add(1).context("cutover sequence exhausted")?;
    render_projections(&project, plan, sequence)?;
    let db = migration::open_active(&project)?;
    migration::publish_control_marker(&project, &db)?;
    ensure!(migration::read_format(&project)?.memory == MEMORY_SQLITE, "publish_control_marker reverted memory owner");
    let _ = db;
    journal.phase=Phase::Active;
    save_journal(&project,&journal)?;
    Ok(journal)
}

/// Abort only before the signed policy transaction changes authority. Imported
/// objects and backups remain available; nothing is silently restored over edits.
pub fn abort_cutover(project: &Path, plan: &MemoryPlan, writers_stopped: bool) -> Result<MemoryJournal> {
    ensure!(writers_stopped,"confirm known writers are stopped with --writers-stopped");
    let project=checked_project(project)?;
    let _guard=migration::maintenance(&project)?;
    let mut journal=load_journal(&project)?;
    ensure!(journal.plan==*plan,"memory journal plan mismatch");
    ensure!(migration::read_format(&project)?.memory==MEMORY_LEGACY,"memory authority already changed; forward recovery is required");
    let snapshot=migration::open_active(&project)?.read_snapshot(None)?;
    ensure!(!snapshot.memory_policies.iter().any(|p|p.op==MemoryPolicyOp::Cutover && p.memory_plan_digest.as_deref()==Some(plan.digest.as_str())),"signed cutover policy already committed; forward recovery is required");
    ensure!(matches!(journal.phase,Phase::Imported|Phase::Verified|Phase::CutoverPending),"memory cutover cannot be aborted in this phase");
    journal.phase=Phase::Imported;
    save_journal(&project,&journal)?;
    Ok(journal)
}

/// Conservative fallback for callers without a sealed task/attempt binding.
/// Always read canonical SQLite facts, never another task's snapshot or projections.
pub fn load_brief_memory(project: &Path, _profile: &str, budget_chars: u64) -> Result<(String, Vec<(String, String)>), MemoryError> {
    let mut db = migration::open_active(project).map_err(|e| MemoryError::Invalid(e.to_string()))?;
    let mut facts = db.active_facts(jiff::Timestamp::now().as_millisecond())?;
    facts.retain(|f| f.record.scope_id == "project" && f.record.kind != MemoryKind::TaskLocal);
    facts.sort_by_key(|f| (!(f.record.is_hard || matches!(f.record.kind, MemoryKind::Constraint | MemoryKind::HardMemory)), f.record.id.as_str().to_string()));
    let mut rendered = String::new();
    for fact in facts {
        let mandatory = fact.record.is_hard || matches!(fact.record.kind, MemoryKind::Constraint | MemoryKind::HardMemory);
        // Without task scope, only optional project-global knowledge is eligible.
        if !mandatory && (!fact.revision.applicability.domains.is_empty() || !fact.revision.applicability.paths.is_empty()) { continue; }
        let bytes = read_object(&objects_dir(project), &fact.revision.body_hash)?;
        let body = std::str::from_utf8(&bytes).map_err(|_| MemoryError::Invalid("memory body is not UTF-8".into()))?;
        let block = format!("\n## {}\n\n{body}\n", fact.record.record_key);
        let required = rendered.chars().count() as u64 + block.chars().count() as u64;
        if required > budget_chars {
            if mandatory { return Err(MemoryError::RequiredContentTooLarge { required_bytes: required, budget_bytes: budget_chars }); }
            continue;
        }
        rendered.push_str(&block);
    }
    // A single pre-budgeted block keeps compose_brief from discarding mandatory
    // files after first packing an optional MEMORY.md index.
    Ok((rendered, Vec::new()))
}

/// Reconstruct the recorded knowledge input without reading current project files.
pub fn render_knowledge_snapshot(project: &Path, id: &str) -> Result<serde_json::Value> {
    let mut db=migration::open_active(project)?;
    render_knowledge_snapshot_held(project,id,&mut db)
}

/// Caller supplies the canonical store and holds execution ownership. Controlled
/// callers retain their SQL interruption hooks through the complete rendering.
pub(crate) fn render_knowledge_snapshot_held(project: &Path, id: &str, db: &mut crate::store::SqliteStore) -> Result<serde_json::Value> {
    let snapshot=db.read_memory_snapshot(id)?;
    let inputs=db.memory_snapshot_inputs(id)?;
    let (memory,_)=render_snapshot(project,db,&snapshot,snapshot.budget_bytes)?;
    let text=format!("# Project instructions\n\n{}\n\n# Task\n\n{}\n\n# Memory\n{}",inputs.instructions,inputs.task_text,memory);
    ensure!(text.chars().count() as u64<=snapshot.budget_bytes,"retained knowledge input exceeds snapshot budget");
    Ok(serde_json::json!({"snapshot":snapshot,"inputs":inputs,"text":text}))
}

/// Render launch knowledge solely from sealed attempt inputs and retained bytes.
/// This does not launch a worker or turn profile probes into execution authority.
pub fn render_attempt_knowledge(project:&Path,attempt:&str)->Result<serde_json::Value> {
    let _guard=super::mutation_guard(project)?;
    let mut db=migration::open_active(project)?;
    render_attempt_knowledge_held(project,attempt,&mut db)
}
/// Caller retains project or root execution ownership across rendering and use.
pub(crate) fn render_attempt_knowledge_held(project:&Path,attempt:&str,db:&mut crate::store::SqliteStore)->Result<serde_json::Value> {
    let snapshot=db.attempt_knowledge_snapshot(attempt,jiff::Timestamp::now().as_millisecond())?;
    let state=db.read_snapshot(None)?;
    let sealed=state.attempt_inputs.iter().find(|r|r.attempt.as_str()==attempt).context("sealed attempt inputs missing")?;
    ensure!(migration::config_reference(Path::new(&sealed.inputs.config.path))?==sealed.inputs.config,"worker configuration changed since approval");
    let mut evidence_bytes=0usize;
    for object in db.memory_consumed_objects(sealed.inputs.task.as_str())? {
        evidence_bytes=evidence_bytes.checked_add(super::read_object_with_budget(&objects_dir(project),&object,(64*1024*1024-evidence_bytes) as u64)?.len()).context("worker evidence byte count overflow")?;
        ensure!(evidence_bytes<=64*1024*1024,"worker evidence exceeds 64 MiB read budget");
    }
    let rendered=render_knowledge_snapshot(project,snapshot.id.as_str())?;
    // Object reads occur outside SQL. Fence authoritative state again before
    // returning a launchable input; a concurrent revocation must not pass through.
    let current=db.attempt_knowledge_snapshot(attempt,jiff::Timestamp::now().as_millisecond())?;
    ensure!(current==snapshot,"attempt knowledge changed while rendering");
    let worktrees=if sealed.inputs.repositories.is_empty() {vec![]} else {crate::domain::worktree_plans(&sealed.inputs,&sealed.attempt).map_err(anyhow::Error::msg)?};
    Ok(serde_json::json!({"estimator":snapshot.estimator,"output_directory":crate::domain::worker_output_path(&sealed.inputs,&sealed.attempt).map_err(anyhow::Error::msg)?,"worktrees":worktrees,"attempt_id":attempt,"snapshot_id":snapshot.id,"profile":snapshot.profile_name,"profile_digest":snapshot.profile_digest,"config_digest":snapshot.config_digest,"budget_chars":snapshot.budget_bytes,"text":rendered["text"]}))
}

/// Read only the immutable snapshot actually bound to this attempt.
pub fn load_attempt_memory(project: &Path, task_id: &str, attempt_id: &str, profile: &str, profile_digest: &str, budget_chars: u64) -> Result<(String, Vec<(String, String)>), MemoryError> {
    let mut db = migration::open_active(project).map_err(|e| MemoryError::Invalid(e.to_string()))?;
    let state = db.read_snapshot(None)?;
    let attempt = state.attempts.iter().find(|a| a.id.as_str() == attempt_id && a.task.as_str() == task_id)
        .ok_or_else(|| MemoryError::Invalid("attempt/task binding missing".into()))?;
    let id = attempt.snapshot.as_ref().ok_or_else(|| MemoryError::Invalid("attempt snapshot binding missing".into()))?;
    let snap = db.read_memory_snapshot(id.as_str())?;
    if snap.task_id != task_id || snap.profile_name != profile || snap.profile_digest != profile_digest {
        return Err(MemoryError::Invalid("attempt snapshot profile/task mismatch".into()));
    }
    render_snapshot(project, &mut db, &snap, budget_chars)
}

fn render_snapshot(project: &Path, db: &mut crate::store::SqliteStore, snap: &MemorySnapshot, budget_chars: u64) -> Result<(String, Vec<(String, String)>), MemoryError> {
    let objects = objects_dir(project);
    let mut rendered = String::new();
    let mut used_chars = 0u64;
    for entry in &snap.entries {
        let rev = db.memory_revision(entry.record_id.as_str(), entry.revision)?
            .ok_or_else(|| MemoryError::Invalid("snapshot revision missing".into()))?;
        let rec = db.memory_record(entry.record_id.as_str())?
            .ok_or_else(|| MemoryError::Invalid("snapshot record missing".into()))?;
        let header = format!("\n## {}\n\n", rec.record_key);
        let overhead = header.chars().count() as u64 + 1;
        let remaining = budget_chars.saturating_sub(used_chars);
        if overhead > remaining {
            return Err(MemoryError::RequiredContentTooLarge { required_bytes: used_chars.saturating_add(overhead), budget_bytes: budget_chars });
        }
        if rendered.len() as u64 + header.len() as u64 + 1 > 64 * 1024 * 1024 {
            return Err(MemoryError::RequiredContentTooLarge { required_bytes: rendered.len() as u64 + header.len() as u64 + 1, budget_bytes: 64 * 1024 * 1024 });
        }
        let remaining_bytes = (64 * 1024 * 1024u64).saturating_sub(rendered.len() as u64)
            .saturating_sub(header.len() as u64 + 1)
            .min((remaining - overhead).saturating_mul(4));
        let body = super::read_object_with_budget(&objects, &rev.body_hash, remaining_bytes)?;
        let text = String::from_utf8(body).map_err(|_| MemoryError::Invalid("memory body is not UTF-8".into()))?;
        let required = used_chars.saturating_add(overhead).saturating_add(text.chars().count() as u64);
        if required > budget_chars {
            return Err(MemoryError::RequiredContentTooLarge { required_bytes: required, budget_bytes: budget_chars });
        }
        rendered.push_str(&header);
        rendered.push_str(&text);
        rendered.push('\n');
        used_chars = required;
    }
    let required = rendered.chars().count() as u64;
    if required > budget_chars {
        return Err(MemoryError::RequiredContentTooLarge { required_bytes: required, budget_bytes: budget_chars });
    }
    Ok((rendered, Vec::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{MemoryPolicy, VersionedReference};
    use crate::store::SqliteStore;
    fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let temp = tempfile::TempDir::new().unwrap();
        let project = temp.path().join("demo");
        fs::create_dir(&project).unwrap();
        fs::create_dir(project.join(".state")).unwrap();
        fs::create_dir(project.join("threads")).unwrap();
        fs::create_dir(project.join("inbox")).unwrap();
        fs::create_dir(project.join("memory")).unwrap();
        fs::write(project.join("PROJECT.md"), "+++\nname = 'Demo'\n+++\nInstructions\n").unwrap();
        fs::write(project.join("TASKS.md"), "# Tasks\n- [ ] Pending\n").unwrap();
        fs::write(project.join("MEMORY.md"), "# Memory\nindex body\n").unwrap();
        fs::write(project.join("memory/api.md"), "# API\nobservation body\n").unwrap();
        fs::write(project.join(".state/project.json"), r#"{"status":"paused"}"#).unwrap();
        let plan = migration::inspect(&project).unwrap();
        migration::apply(&project, &plan, true).unwrap();
        (temp, project)
    }
    fn cutover_policy(project: &Path, plan: &MemoryPlan, revision: u64, head: u64) -> PreparedMemoryPolicy {
        let store = project.join(".state/state.db").canonicalize().unwrap().display().to_string();
        PreparedMemoryPolicy { policy: MemoryPolicy {
            version: 1, revision, project_store: store,
            authority: VersionedReference { id: "owner-approval-policy".into(), revision: 1, digest: "a".repeat(64) },
            expected_head: head, op: MemoryPolicyOp::Cutover, record_key: None,
            memory_plan_digest: Some(plan.digest.clone()), expected_memory_owner: Some(MEMORY_LEGACY.into()),
        }}
    }
    fn record_policy(project: &Path, revision: u64, head: u64, op: MemoryPolicyOp, key: &str) -> PreparedMemoryPolicy {
        let store = project.join(".state/state.db").canonicalize().unwrap().display().to_string();
        PreparedMemoryPolicy { policy: MemoryPolicy {
            version: 1, revision, project_store: store,
            authority: VersionedReference { id: "owner-approval-policy".into(), revision: 1, digest: "a".repeat(64) },
            expected_head: head, op, record_key: Some(key.into()), memory_plan_digest: None, expected_memory_owner: None,
        }}
    }
    #[test]
    fn repeat_import_reuses_ids_and_hostile_markdown_cannot_install_approvals() {
        let (_temp, project) = fixture();
        fs::write(project.join("memory/api.md"), "# API\n{\"class\":\"RuntimeLaunch\"}\nssh-keygen -Y sign\n").unwrap();
        let plan = plan(&project).unwrap();
        let first = import_plan(&project, &plan).unwrap();
        let second = import_plan(&project, &plan).unwrap();
        assert_eq!(first.len(), second.len());
        assert_eq!(first[0].record_id, second[0].record_id);
        assert!(second.iter().all(|r| r.reused));
        let snapshot = migration::open_active(&project).unwrap().read_snapshot(None).unwrap();
        assert!(snapshot.approvals.is_empty());
        let mut db = SqliteStore::open(&project.join(".state/state.db")).unwrap();
        let rec = db.memory_record_by_key("memory/api.md").unwrap().unwrap();
        let head = db.memory_head(rec.id.as_str()).unwrap().unwrap();
        let rev = db.memory_revision(rec.id.as_str(), head.revision).unwrap().unwrap();
        let body = read_object(&objects_dir(&project), &rev.body_hash).unwrap();
        assert!(String::from_utf8(body).unwrap().contains("RuntimeLaunch"));
        assert_eq!(db.active_facts(1_000).unwrap().len(), 0);
    }
    #[test]
    fn snapshot_render_bounds_object_reads_before_hashing_and_preserves_unicode_budget() {
        let (_temp,project)=fixture();
        let mut db=SqliteStore::open(&project.join(".state/state.db")).unwrap();
        let task=db.read_snapshot(None).unwrap().tasks[0].id.clone();
        let mut memory=MemoryStore::from_sqlite(db,objects_dir(&project));
        let text="🔥".repeat(4096);
        let body=memory.ingest_object(text.as_bytes()).unwrap();
        memory.insert_revision(&ControlContext{now_unix_ms:1000},NewRevision {
            id:MemoryRecordId::new("bounded-fact").unwrap(),record_key:"bounded-fact".into(),scope_id:"project".into(),kind:MemoryKind::Observation,
            body_hash:body.clone(),provenance_hash:body.clone(),applicability:Applicability{domains:vec![],paths:vec![]},dependencies:vec![],
            expected:None,expiry_unix_ms:None,validity_state:"valid".into(),validity_reason:"fixture".into(),
        }).unwrap();
        let snapshot=memory.create_task_snapshot(SnapshotRequest {
            schema_version:1,task_id:task.as_str().into(),profile:"worker".into(),domains:vec![],paths:vec![],pinned_keys:vec![],sensitivity:"default".into(),
        },"worker",&"a".repeat(64),None,100000,"",1000,None).unwrap();
        assert_eq!(snapshot.entries.len(),1);
        let path=objects_dir(&project).join("sha256").join(&body.as_str()[..2]).join(body.as_str());
        fs::write(&path,vec![b'x';200000]).unwrap();
        assert!(matches!(render_snapshot(&project,&mut memory.store,&snapshot,20),Err(MemoryError::RequiredContentTooLarge{..})));
        fs::write(&path,text.as_bytes()).unwrap();
        let (rendered,_)=render_snapshot(&project,&mut memory.store,&snapshot,5000).unwrap();
        assert!(rendered.contains(&text));
        assert!(rendered.chars().count()<=5000 && rendered.len()>5000);
    }
    #[test]
    fn traversal_symlink_oversize_and_nul_are_refused() {
        let (_temp, project) = fixture();
        assert!(contained_relative(&project, &project.join("PROJECT.md")).is_err());
        assert!(contained_relative(&project, &project.join("memory/../PROJECT.md")).is_err());
        fs::remove_file(project.join("memory/api.md")).unwrap();
        std::os::unix::fs::symlink("../PROJECT.md", project.join("memory/api.md")).unwrap();
        assert!(plan(&project).is_err());
        fs::remove_file(project.join("memory/api.md")).unwrap();
        fs::write(project.join("memory/api.md"), vec![b'x'; (FILE_LIMIT as usize) + 1]).unwrap();
        assert!(plan(&project).is_err());
        fs::write(project.join("memory/api.md"), b"ok\0bad").unwrap();
        assert!(plan(&project).is_err());
    }
    #[test]
    fn cutover_switches_owner_preserves_runtime_and_rejects_divergent_projection() {
        let (_temp, project) = fixture();
        let plan = plan(&project).unwrap();
        assert_eq!(migration::read_format(&project).unwrap().memory, MEMORY_LEGACY);
        import_plan(&project, &plan).unwrap();
        let head = migration::open_active(&project).unwrap().read_snapshot(None).unwrap().head;
        let journal = cutover(&project, &plan, &cutover_policy(&project, &plan, 1, head), true).unwrap();
        assert_eq!(journal.phase, Phase::Active);
        let marker = migration::read_format(&project).unwrap();
        assert_eq!(marker.memory, MEMORY_SQLITE);
        assert_eq!(marker.runtime, "sqlite-v2");
        assert_eq!(marker.migration, plan.runtime_migration);
        assert!(migration::open_active(&project).is_ok());
        let db = migration::open_active(&project).unwrap();
        migration::publish_control_marker(&project, &db).unwrap();
        assert_eq!(migration::read_format(&project).unwrap().memory, MEMORY_SQLITE);
        let projected = fs::read_to_string(project.join("MEMORY.md")).unwrap();
        assert!(projected.contains("memory projection"));
        assert!(projected.contains("index body"));
        fs::write(project.join("MEMORY.md"), "manual edit").unwrap();
        assert!(render_projections(&project, &plan, 1).is_err());
        assert_eq!(fs::read_to_string(project.join("MEMORY.md")).unwrap(), "manual edit");
        let (index, files) = load_brief_memory(&project, "", 32_000).unwrap();
        assert!(!index.contains("manual edit"));
        assert!(files.iter().all(|(_, text)| !text.contains("manual edit")));
        let _ = db;
    }
    #[test]
    fn hard_rule_without_import_ack_is_an_active_fact_and_gc_keeps_pins() {
        let (_temp, project) = fixture();
        let plan = plan(&project).unwrap();
        import_plan(&project, &plan).unwrap();
        let mut db = migration::open_active(&project).unwrap();
        let head = db.read_snapshot(None).unwrap().head;
        db.install_memory_policy(&record_policy(&project, 1, head, MemoryPolicyOp::HardRule, "memory/api.md"), head).unwrap();
        let facts = db.active_facts(1_000).unwrap();
        assert!(facts.iter().any(|f| f.record.record_key == "memory/api.md" && f.record.is_hard && f.validity.state == "valid"));
        let rec = db.memory_record_by_key("memory/api.md").unwrap().unwrap();
        let rev = db.memory_revision(rec.id.as_str(), 1).unwrap().unwrap();
        let mut memory = MemoryStore::from_sqlite(db, objects_dir(&project));
        assert_eq!(memory.collect_unreferenced().unwrap(), 0);
        let path = objects_dir(&project).join("sha256").join(&rev.body_hash.as_str()[..2]).join(rev.body_hash.as_str());
        assert!(path.exists());
    }
    #[test]
    fn brief_without_snapshot_succeeds_and_oversized_snapshot_fails_closed() {
        let (_temp, project) = fixture();
        fs::write(project.join("MEMORY.md"), format!("# Memory\n{}\n", "x".repeat(500))).unwrap();
        let plan = plan(&project).unwrap();
        import_plan(&project, &plan).unwrap();
        let mut db = migration::open_active(&project).unwrap();
        let head = db.read_snapshot(None).unwrap().head;
        db.install_memory_policy(&record_policy(&project, 1, head, MemoryPolicyOp::HardRule, "MEMORY.md"), head).unwrap();
        let head = db.read_snapshot(None).unwrap().head;
        drop(db);
        cutover(&project, &plan, &cutover_policy(&project, &plan, 2, head), true).unwrap();
        let (index, _files) = load_brief_memory(&project, "missing-profile", 32_000).unwrap();
        assert!(index.contains("projection") || index.contains("Memory"));
        let mut memory = MemoryStore::from_sqlite(migration::open_active(&project).unwrap(), objects_dir(&project));
        let head = memory.store.read_snapshot(None).unwrap().head;
        let task = crate::domain::Task { id: crate::domain::TaskId::new("task-ui").unwrap(), revision: 1, state: crate::domain::TaskState::Draft, title: "ui".into(), active_attempt: None };
        memory.store.commit(crate::domain::Commit { expected_head: head, mutations: vec![crate::domain::Mutation::Task { expected: None, next: task }] }).unwrap();
        let req = SnapshotRequest { schema_version: 1, task_id: "task-ui".into(), profile: "implementation".into(), domains: vec![], paths: vec![], pinned_keys: vec![], sensitivity: "default".into() };
        memory.create_task_snapshot(req, "implementation", &"a".repeat(64), None, 100_000, "instructions", 1_000, None).unwrap();
        let err = load_brief_memory(&project, "implementation", 100).unwrap_err();
        assert!(matches!(err, MemoryError::RequiredContentTooLarge { .. }));
    }
    #[test]
    fn writers_must_stop_and_unknown_memory_owner_is_refused() {
        let (_temp, project) = fixture();
        let plan = plan(&project).unwrap();
        import_plan(&project, &plan).unwrap();
        let head = migration::open_active(&project).unwrap().read_snapshot(None).unwrap().head;
        assert!(cutover(&project, &plan, &cutover_policy(&project, &plan, 1, head), false).is_err());
        let mut marker: Format = serde_json::from_slice(&read(&project.join(".state/format.json")).unwrap()).unwrap();
        marker.memory = "other".into();
        fs::write(project.join(".state/format.json"), serde_json::to_vec(&marker).unwrap()).unwrap();
        assert!(migration::open_active(&project).is_err());
    }
    #[test]
    fn sealed_worker_knowledge_uses_retained_inputs_and_refuses_corrupt_bytes() {
        let (_temp,project)=fixture();let now=jiff::Timestamp::now().as_millisecond();
        let mut db=migration::open_active(&project).unwrap();let state=db.read_snapshot(None).unwrap();
        let task=TaskId::new("worker").unwrap();
        db.commit(Commit {expected_head:state.head,mutations:vec![Mutation::Task {expected:None,next:Task {id:task.clone(),revision:1,state:TaskState::Draft,title:"Captured task".into(),active_attempt:None}}]}).unwrap();
        let state=db.read_snapshot(None).unwrap();db.create_runtime(Some(&task),Some(1),state.head,&RuntimeRoute::default()).unwrap();
        let state=db.read_snapshot(None).unwrap();db.queue_task(&task,2,state.head,&QueueRequest {priority:0,dependencies:vec![]},now).unwrap();
        let state=db.read_snapshot(None).unwrap();db.set_scheduler_policy(state.head,state.scheduler.unwrap().policy.revision,1,3).unwrap();
        let state=db.read_snapshot(None).unwrap();let binding=state.runtime_bindings.iter().find(|b|b.task.as_ref()==Some(&task)).unwrap().clone();
        db.record_observations(state.head,&[crate::reconcile::RuntimeObservation {binding:binding.id.clone(),binding_revision:binding.revision,task_revision:Some(3),observed_unix_ms:now,collector:"herdr-git-v1".into(),..Default::default()}]).unwrap();
        let state=db.read_snapshot(None).unwrap();crate::runtime::set_state(&project,state.head,state.control.unwrap().revision,ProjectState::Active,&project.join("fixture-config.toml")).unwrap();
        let config=migration::ConfigReference {path:project.join("fixture-config.toml").display().to_string(),digest:None};
        let profile=crate::domain::profile::fixture(config.clone());
        let mut memory=MemoryStore::from_sqlite(migration::open_active(&project).unwrap(),objects_dir(&project));
        let body=memory.ingest_object(&b"Captured memory"[..]).unwrap();
        memory.insert_revision(&ControlContext {now_unix_ms:now},NewRevision {id:MemoryRecordId::new("worker-fact").unwrap(),record_key:"worker-fact".into(),scope_id:"project".into(),kind:MemoryKind::Observation,body_hash:body.clone(),provenance_hash:body.clone(),applicability:Applicability {domains:vec![],paths:vec![]},dependencies:vec![],expected:None,expiry_unix_ms:None,validity_state:"valid".into(),validity_reason:"fixture".into()}).unwrap();
        let snapshot=memory.create_worker_snapshot(SnapshotRequest {schema_version:1,task_id:"worker".into(),profile:profile.name.clone(),domains:vec![],paths:vec![],pinned_keys:vec![],sensitivity:"default".into()},&profile.name,&profile.definition_digest,None,32000,"Captured instructions",now,None).unwrap();
        let state=db.read_snapshot(None).unwrap();
        let mut inputs=LaunchInputs {version:2,project_store:project.join(".state/state.db").canonicalize().unwrap().display().to_string(),task:task.clone(),task_revision:3,scheduler_revision:state.scheduler.unwrap().policy.revision,control_epoch:state.control.unwrap().epoch,binding:binding.id.clone(),binding_revision:binding.revision,binding_digest:crate::store::ownership::identity_digest(&binding).unwrap(),profile:profile.reference().unwrap(),effective_profile:Some(profile.clone()),approval:VersionedReference {id:"placeholder".into(),revision:1,digest:"a".repeat(64)},config,repositories:vec![],dependencies:vec![],memory:Some(VersionedReference {id:snapshot.id.as_str().into(),revision:1,digest:snapshot.manifest_hash}),budget:None};
        let grant=ApprovalGrant {version:1,scope:ApprovalScope::for_launch(&inputs).unwrap(),policy:profile.permission_policy,issued_unix_ms:now,expires_unix_ms:now+60000};
        inputs.approval=db.install_approval(&PreparedApproval {grant},state.head,now).unwrap();
        let head=db.read_snapshot(None).unwrap().head;let reserved=db.reserve_prepared(&[PreparedLaunch {inputs}],head,now).unwrap();
        fs::write(project.join("PROJECT.md"),"Edited after reservation").unwrap();
        let rendered=render_attempt_knowledge(&project,reserved.record.attempt.as_str()).unwrap();
        let text=rendered["text"].as_str().unwrap();
        for value in ["Captured instructions","Captured task","Captured memory"] {assert!(text.contains(value));}
        assert!(!text.contains("Edited after reservation"));assert!(text.chars().count()<=32000);
        let brief=super::super::render_attempt_brief(&project,reserved.record.attempt.as_str()).unwrap();
        assert!(brief.text.contains(text));
        assert_eq!(brief.output_directory,rendered["output_directory"].as_str().unwrap());
        assert!(brief.prompt_chars<=brief.budget_chars);
        assert_eq!(brief.attempt_id,reserved.record.attempt.as_str());
        assert_eq!(brief.snapshot_id,snapshot.id.as_str());
        assert!(!brief.text.contains("Edited after reservation"));
        fs::write(objects_dir(&project).join("sha256").join(&body.as_str()[..2]).join(body.as_str()),"corrupt").unwrap();
        assert!(render_attempt_knowledge(&project,reserved.record.attempt.as_str()).is_err());
        assert!(super::super::render_attempt_brief(&project,reserved.record.attempt.as_str()).is_err());
    }

}

//! Schema 26 contract install and untrusted result ingress.
//! Schema 32 scope rows are written only by that signed install. Overlap is stored, not locked.
//! Object bytes are fsynced before the submission row so a crash retries as one row.
//! Claimed checks are stored and are not evidence.
use super::*;
use crate::domain::{
    AcceptancePolicy, AttemptId, ContractInstall, ObjectFormat, PreparedContract, ResultObjectView,
    ResultReceipt, ResultView, TaskId,
};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    },
    path::{Component, Path, PathBuf},
};

// Bound the total pack/index snapshot bytes per submission, copied lazily once.
const PACK_COPY_LIMIT: u64 = 1024 * 1024 * 1024;
const OBJECT_LIMIT: u64 = 16 * 1024 * 1024;
const SUBMISSION_LIMIT: usize = 256 * 1024;
const SCHEMA_VERSION: u32 = 26;
const SCOPE_SCHEMA: u32 = 32;
const CLAIMS_SCHEMA: u32 = 35;

fn invalid(message: &str) -> StoreError {
    StoreError::Invalid(message.into())
}
fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn schema26(db: &Connection) -> Result<()> {
    check_schema(db)?;
    let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchema(version));
    }
    Ok(())
}
fn store_path(db: &Connection) -> Result<PathBuf> {
    let path = db.path().ok_or_else(|| invalid("store path missing"))?;
    std::fs::canonicalize(path).map_err(|error| StoreError::Io(error.to_string()))
}
fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| StoreError::Io(error.to_string()))
}
fn object_lock(objects: &Path) -> Result<File> {
    fs::create_dir_all(objects).map_err(|error| StoreError::Io(error.to_string()))?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(objects.join(".io.lock"))
        .map_err(|error| StoreError::Io(error.to_string()))?;
    if !file
        .metadata()
        .map_err(|error| StoreError::Io(error.to_string()))?
        .is_file()
    {
        return Err(invalid("result object lock is not a regular file"));
    }
    file.lock()
        .map_err(|error| StoreError::Io(error.to_string()))?;
    Ok(file)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UntrustedObject {
    oid: String,
    relative_path: String,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UntrustedArtifact {
    path: String,
    oid: String,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UntrustedSubmission {
    idempotency_key: String,
    task_id: TaskId,
    contract_revision: u64,
    contract_digest: String,
    attempt_id: AttemptId,
    repository: String,
    base_oid: String,
    candidate_oid: String,
    object_format: ObjectFormat,
    #[serde(default)]
    memory_snapshot_id: Option<String>,
    artifact_manifest: Vec<UntrustedArtifact>,
    claimed_checks: Vec<String>,
    objects: Vec<UntrustedObject>,
}

struct ParsedSubmission {
    idempotency_key: String,
    task_id: TaskId,
    contract_revision: u64,
    contract_digest: String,
    attempt_id: AttemptId,
    repository: String,
    base_oid: String,
    candidate_oid: String,
    object_format: ObjectFormat,
    memory_snapshot_id: Option<String>,
    artifact_manifest: String,
    claimed_checks: String,
    objects: Vec<UntrustedObject>,
}

fn plain(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}
fn hex_oid(value: &str, format: ObjectFormat) -> bool {
    value.len() == format.oid_len()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn parse_submission(raw: &[u8]) -> Result<ParsedSubmission> {
    if raw.is_empty() || raw.len() > SUBMISSION_LIMIT {
        return Err(invalid("result submission exceeds 256 KiB"));
    }
    if std::str::from_utf8(raw).is_err() {
        return Err(invalid("invalid result submission"));
    }
    let document: UntrustedSubmission =
        serde_json::from_slice(raw).map_err(|_| invalid("invalid result submission"))?;
    if !plain(&document.idempotency_key, 128)
        || document.contract_revision == 0
        || document.contract_revision > i64::MAX as u64
        || !hex_oid(&document.contract_digest, ObjectFormat::Sha256)
    {
        return Err(invalid("invalid result submission"));
    }
    if !hex_oid(&document.base_oid, document.object_format)
        || !hex_oid(&document.candidate_oid, document.object_format)
        || !std::path::Path::new(&document.repository).is_absolute()
        || !plain(&document.repository, 4_096)
    {
        return Err(invalid("invalid result repository or oid"));
    }
    if let Some(snapshot) = &document.memory_snapshot_id {
        if !plain(snapshot, 128) {
            return Err(invalid("invalid result snapshot"));
        }
    }
    if document.objects.is_empty()
        || document.objects.len() > 64
        || document.artifact_manifest.len() > 64
        || document.claimed_checks.len() > 32
    {
        return Err(invalid("result submission exceeds bounds"));
    }
    let mut seen = std::collections::BTreeSet::new();
    for object in &document.objects {
        if !hex_oid(&object.oid, document.object_format) || !seen.insert(object.oid.clone()) {
            return Err(invalid("invalid result object"));
        }
        let loose = format!("{}/{}", &object.oid[..2], &object.oid[2..]);
        if object.relative_path != loose {
            return Err(invalid("path traversal"));
        }
    }
    if !document
        .objects
        .iter()
        .any(|object| object.oid == document.base_oid)
        || !document
            .objects
            .iter()
            .any(|object| object.oid == document.candidate_oid)
    {
        return Err(invalid("missing object"));
    }
    for artifact in &document.artifact_manifest {
        if !plain(&artifact.path, 512) || !hex_oid(&artifact.oid, document.object_format) {
            return Err(invalid("invalid artifact manifest"));
        }
    }
    for check in &document.claimed_checks {
        if !plain(check, 256) {
            return Err(invalid("invalid claimed check"));
        }
    }
    Ok(ParsedSubmission {
        idempotency_key: document.idempotency_key,
        task_id: document.task_id,
        contract_revision: document.contract_revision,
        contract_digest: document.contract_digest,
        attempt_id: document.attempt_id,
        repository: document.repository,
        base_oid: document.base_oid,
        candidate_oid: document.candidate_oid,
        object_format: document.object_format,
        memory_snapshot_id: document.memory_snapshot_id,
        artifact_manifest: serde_json::to_string(&document.artifact_manifest)
            .map_err(|error| invalid(&error.to_string()))?,
        claimed_checks: serde_json::to_string(&document.claimed_checks)
            .map_err(|error| invalid(&error.to_string()))?,
        objects: document.objects,
    })
}

struct ContractRow {
    required_outputs: Vec<String>,
    raw_digest: String,
    repository: String,
    base_oid: String,
    object_format: String,
    memory_snapshot_id: Option<String>,
}

fn load_contract(db: &Connection, task: &str, revision: u64) -> Result<ContractRow> {
    let row = db.query_row(
        "SELECT raw_bytes, raw_digest, repository, base_oid, object_format, memory_snapshot_id FROM task_contracts WHERE task_id=?1 AND contract_revision=?2",
        params![task, integer(revision)?],
        |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?, row.get::<_, String>(4)?, row.get::<_, Option<String>>(5)?)),
    ).optional()?;
    let Some((raw, digest, repository, base_oid, object_format, memory_snapshot_id)) = row else {
        return Err(invalid("missing contract"));
    };
    if sha256_hex(&raw) != digest {
        return Err(StoreError::Corrupt("task contract digest mismatch".into()));
    }
    let required_outputs = PreparedContract::parse_verified(&raw).map_err(|error| invalid(&error))?.required_outputs;
    Ok(ContractRow {
        required_outputs,
        raw_digest: digest,
        repository,
        base_oid,
        object_format,
        memory_snapshot_id,
    })
}

fn attempt_matches(db: &Connection, attempt: &str, task: &str) -> Result<()> {
    let owner: Option<String> = db
        .query_row(
            "SELECT task_id FROM attempts WHERE id=?1",
            [attempt],
            |row| row.get(0),
        )
        .optional()?;
    match owner {
        Some(owner) if owner == task => Ok(()),
        _ => Err(invalid("wrong attempt")),
    }
}

fn read_nofollow(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| StoreError::Io(error.to_string()))?;
    let meta = file
        .metadata()
        .map_err(|error| StoreError::Io(error.to_string()))?;
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > limit {
        return Err(invalid("invalid git metadata file"));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| StoreError::Io(error.to_string()))?;
    if bytes.len() as u64 > limit {
        return Err(invalid("invalid git metadata file"));
    }
    Ok(bytes)
}

fn one_line(bytes: &[u8]) -> Result<&str> {
    let text = std::str::from_utf8(bytes).map_err(|_| invalid("invalid git metadata file"))?;
    let text = text.strip_suffix('\n').unwrap_or(text);
    if text.is_empty() || text.contains('\n') || text.contains('\0') {
        return Err(invalid("invalid git metadata file"));
    }
    Ok(text)
}

/// Absolute gitdir targets only. A symlink at any component is refused before canonicalize.
fn refuse_symlink_path(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Err(invalid("gitdir target is not absolute"));
    }
    let mut cursor = PathBuf::new();
    for component in path.components() {
        cursor.push(component);
        match fs::symlink_metadata(&cursor) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(invalid("gitdir target is a symlink"));
            }
            Ok(_) => {}
            Err(_) => return Err(invalid("gitdir target is unavailable")),
        }
    }
    Ok(())
}

fn read_gitdir(gitfile: &Path) -> Result<PathBuf> {
    let bytes = read_nofollow(gitfile, 4_096)?;
    let line = one_line(&bytes)?;
    let target = line
        .strip_prefix("gitdir: ")
        .ok_or_else(|| invalid("invalid gitdir file"))?;
    let target = Path::new(target);
    refuse_symlink_path(target)?;
    let canonical = target
        .canonicalize()
        .map_err(|_| invalid("gitdir target is unavailable"))?;
    let meta =
        fs::symlink_metadata(&canonical).map_err(|_| invalid("gitdir target is unavailable"))?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(invalid("gitdir target is unavailable"));
    }
    Ok(canonical)
}

fn common_directory(gitdir: &Path, gitfile: &Path) -> Result<PathBuf> {
    let bytes = read_nofollow(&gitdir.join("commondir"), 4_096)?;
    let text = one_line(&bytes)?;
    // Only a relative walk from <common>/worktrees/<id> is a linked worktree.
    if Path::new(text).is_absolute() {
        return Err(invalid("commondir target is absolute"));
    }
    let mut cursor = gitdir.to_path_buf();
    for component in Path::new(text).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !cursor.pop() {
                    return Err(invalid("commondir escapes its git directory"));
                }
            }
            Component::Normal(part) => {
                cursor.push(part);
                match fs::symlink_metadata(&cursor) {
                    Ok(meta) if meta.file_type().is_symlink() => {
                        return Err(invalid("commondir is a symlink"));
                    }
                    Ok(_) => {}
                    Err(_) => return Err(invalid("commondir is unavailable")),
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(invalid("commondir target is absolute"));
            }
        }
    }
    let meta = fs::symlink_metadata(&cursor).map_err(|_| invalid("commondir is unavailable"))?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(invalid("commondir is unavailable"));
    }
    if gitdir.parent() != Some(cursor.join("worktrees").as_path()) {
        return Err(invalid(
            "commondir is not this worktree's common git directory",
        ));
    }
    let back_bytes = read_nofollow(&gitdir.join("gitdir"), 4_096)?;
    let back = one_line(&back_bytes)?;
    let gitfile = gitfile
        .to_str()
        .ok_or_else(|| invalid("repository .git path is not utf-8"))?;
    if back != gitfile {
        return Err(invalid("gitdir does not point back at this repository"));
    }
    Ok(cursor)
}

fn git_objects(repository: &Path) -> Result<PathBuf> {
    let git = repository.join(".git");
    let meta =
        fs::symlink_metadata(&git).map_err(|_| invalid("repository is not a git directory"))?;
    if meta.file_type().is_symlink() {
        return Err(invalid("symlink .git is refused"));
    }
    // Linked worktrees keep loose objects under the common git directory, not the checkout.
    let objects = if meta.is_dir() {
        git.join("objects")
    } else if meta.is_file() {
        common_directory(&read_gitdir(&git)?, &git)?.join("objects")
    } else {
        return Err(invalid("repository is not a git directory"));
    };
    let objects_meta =
        fs::symlink_metadata(&objects).map_err(|_| invalid("missing git object directory"))?;
    if !objects_meta.is_dir() || objects_meta.file_type().is_symlink() {
        return Err(invalid("missing git object directory"));
    }
    Ok(objects)
}

fn map_join(error: anyhow::Error) -> StoreError {
    let text = error.to_string();
    if text.contains("unsafe") || text.contains("symlink") {
        invalid("path traversal")
    } else {
        StoreError::Io(text)
    }
}

fn read_object_file(path: &Path) -> Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                invalid("missing object")
            } else {
                StoreError::Io(error.to_string())
            }
        })?;
    let meta = file
        .metadata()
        .map_err(|error| StoreError::Io(error.to_string()))?;
    if !meta.is_file() {
        return Err(invalid("missing object"));
    }
    if meta.len() > OBJECT_LIMIT {
        return Err(invalid("object exceeds 16 MiB"));
    }
    let mut bytes = Vec::new();
    file.take(OBJECT_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| StoreError::Io(error.to_string()))?;
    if bytes.len() as u64 > OBJECT_LIMIT {
        return Err(invalid("object exceeds 16 MiB"));
    }
    Ok(bytes)
}

/// Pin the source directories and copy only regular pack/index pairs from open
/// descriptors. Git never sees this worker-controlled storage or its alternates.
fn copy_private_packs(objects: &Path, destination: &Path) -> anyhow::Result<()> {
    let objects = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(objects)?;
    let open_at = |directory: &File, name: &std::ffi::OsStr, flags| -> std::io::Result<File> {
        let name = std::ffi::CString::new(name.as_bytes())?;
        // SAFETY: the directory descriptor and NUL-terminated name remain valid
        // through openat; a successful descriptor is owned by the returned File.
        let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    };
    let pack = open_at(
        &objects,
        std::ffi::OsStr::new("pack"),
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC,
    )?;
    let mut total = 0;
    // /proc exposes the pinned directory for enumeration only. File contents
    // are opened relative to its descriptor, never by re-resolving source paths.
    for entry in fs::read_dir(format!("/proc/self/fd/{}", pack.as_raw_fd()))? {
        let name = entry?.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(stem) = name
            .strip_prefix("pack-")
            .and_then(|s| s.strip_suffix(".pack"))
        else {
            continue;
        };
        for name in [format!("pack-{stem}.pack"), format!("pack-{stem}.idx")] {
            let source = open_at(
                &pack,
                std::ffi::OsStr::new(&name),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )?;
            let metadata = source.metadata()?;
            anyhow::ensure!(metadata.is_file(), "non-regular pack file");
            anyhow::ensure!(
                metadata.len() <= PACK_COPY_LIMIT - total,
                "pack snapshot exceeds 1 GiB"
            );
            let mut target = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(destination.join(name))?;
            let copied = std::io::copy(&mut source.take(PACK_COPY_LIMIT - total + 1), &mut target)?;
            total += copied;
            anyhow::ensure!(total <= PACK_COPY_LIMIT, "pack snapshot exceeds 1 GiB");
        }
    }
    Ok(())
}

/// Reconstruct a loose object using Git's own encoder, in a private repository.
/// Re-hashing the exact binary content refuses forged pack indexes and replace refs.
fn read_packed_object(
    repository: &Path,
    scratch: &Path,
    oid: &str,
    format: ObjectFormat,
) -> Result<Vec<u8>> {
    use crate::runner::{Cmd, RealRunner, Runner};
    // Resolve storage without invoking worker-controlled Git configuration.
    let verified_objects = (|| -> Result<PathBuf> {
        let objects = git_objects(repository)?;
        let common = objects
            .parent()
            .ok_or_else(|| invalid("missing object"))?
            .canonicalize()
            .map_err(|_| invalid("missing object"))?;
        let resolved = objects
            .canonicalize()
            .map_err(|_| invalid("missing object"))?;
        if !resolved.starts_with(&common) {
            return Err(invalid("missing object"));
        }
        match fs::symlink_metadata(objects.join("pack")) {
            Ok(meta) if meta.file_type().is_symlink() => return Err(invalid("missing object")),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(invalid("missing object")),
        }
        match fs::symlink_metadata(objects.join("info/alternates")) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => return Err(invalid("missing object")),
        }
        Ok(resolved)
    })()
    .map_err(|_| invalid("missing object"))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    // These private-repository operations own no transfer resources. RealRunner
    // supplies the GatedSpawn gate, process-group cleanup and capture bounds.
    let git = |path: &Path,
               args: &[&str],
               stdin: Option<String>,
               limit: usize|
     -> anyhow::Result<Vec<u8>> {
        let mut cmd = Cmd::new("/usr/bin/git", std::time::Duration::from_secs(20))
            .args([
                "--no-pager",
                "--no-optional-locks",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "gc.auto=0",
                "-c",
                "maintenance.auto=false",
            ])
            .args(args.iter().copied());
        cmd.cwd = Some(
            path.to_str()
                .ok_or_else(|| anyhow::anyhow!("invalid Git path"))?
                .into(),
        );
        cmd.env_clear = true;
        cmd.env = [
            ("PATH", "/usr/bin:/bin"),
            ("LANG", "C"),
            ("LC_ALL", "C"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_TERMINAL_PROMPT", "0"),
            ("GIT_NO_LAZY_FETCH", "1"),
            ("GIT_NO_REPLACE_OBJECTS", "1"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
        if path.join("objects").is_dir() {
            cmd.env.push((
                "GIT_OBJECT_DIRECTORY".into(),
                path.join("objects")
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("invalid object directory"))?
                    .into(),
            ));
        }
        cmd.deadline = Some(deadline);
        cmd.stdin = stdin;
        cmd.capture_limit = limit;
        let output = RealRunner.run(&cmd)?;
        anyhow::ensure!(
            output.success(),
            "object Git read failed or exceeded bounds"
        );
        Ok(output.stdout_bytes)
    };
    let read = || -> anyhow::Result<Vec<u8>> {
        if !scratch.exists() {
            fs::create_dir(scratch)?;
            git(
                scratch,
                &[
                    "init",
                    "--bare",
                    &format!("--object-format={}", format.as_str()),
                ],
                None,
                8192,
            )?;
            copy_private_packs(&verified_objects, &scratch.join("objects/pack"))?;
            let encoder = scratch.join("encoder");
            fs::create_dir(&encoder)?;
            git(
                &encoder,
                &[
                    "init",
                    "--bare",
                    &format!("--object-format={}", format.as_str()),
                ],
                None,
                8192,
            )?;
        }
        anyhow::ensure!(
            matches!(fs::symlink_metadata(scratch.join("objects/info/alternates")),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound),
            "private repository has alternates"
        );
        let output = git(
            scratch,
            &["cat-file", "--batch"],
            Some(format!("{oid}\n")),
            OBJECT_LIMIT as usize + 256,
        )?;
        let newline = output
            .iter()
            .position(|b| *b == b'\n')
            .ok_or_else(|| anyhow::anyhow!("missing header"))?;
        let header = std::str::from_utf8(&output[..newline])?;
        let fields: Vec<_> = header.split(' ').collect();
        anyhow::ensure!(fields.len() == 3 && fields[0] == oid, "missing object");
        anyhow::ensure!(
            ["blob", "tree", "commit", "tag"].contains(&fields[1]),
            "invalid object type"
        );
        let size: usize = fields[2].parse()?;
        anyhow::ensure!(
            size <= OBJECT_LIMIT as usize
                && output.len() == newline + size + 2
                && output.last() == Some(&b'\n'),
            "invalid object size"
        );
        // Keep the submission's pack snapshot intact. A separate repository
        // without packs forces hash-object to write the verified loose bytes.
        let encoder = scratch.join("encoder");
        let content = scratch.join("content");
        write_file(&content, &output[newline + 1..newline + 1 + size])
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let hashed = git(
            &encoder,
            &[
                "hash-object",
                "--literally",
                "-w",
                "-t",
                fields[1],
                "--",
                content
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("invalid scratch path"))?,
            ],
            None,
            8192,
        );
        fs::remove_file(content)?;
        anyhow::ensure!(
            hashed? == format!("{oid}\n").as_bytes(),
            "object identity mismatch"
        );
        read_object_file(&encoder.join("objects").join(&oid[..2]).join(&oid[2..]))
            .map_err(|e| anyhow::anyhow!("{e}"))
    };
    read().map_err(|_| invalid("missing object"))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct StagedObject {
    oid: String,
    relative_path: String,
    byte_sha256: String,
    size: u64,
}
#[derive(Debug, Serialize, Deserialize)]
struct StageManifest {
    payload_digest: String,
    objects: Vec<StagedObject>,
}

fn staging_dir(objects: &Path, key: &str) -> PathBuf {
    objects.join("staging").join(sha256_hex(key.as_bytes()))
}

fn hash_staged(path: &Path) -> Result<(String, u64)> {
    let bytes = read_object_file(path)?;
    Ok((sha256_hex(&bytes), bytes.len() as u64))
}

fn reconcile_stage(
    dir: &Path,
    payload_digest: &str,
    submission: &ParsedSubmission,
) -> Result<Option<Vec<StagedObject>>> {
    let manifest_path = dir.join("manifest.json");
    if !manifest_path.is_file() {
        if dir.exists() {
            fs::remove_dir_all(dir).map_err(|error| StoreError::Io(error.to_string()))?;
        }
        return Ok(None);
    }
    let bytes = read_object_file(&manifest_path).map_err(|_| invalid("corrupt result stage"))?;
    let manifest: StageManifest =
        serde_json::from_slice(&bytes).map_err(|_| invalid("corrupt result stage"))?;
    if manifest.payload_digest != payload_digest {
        return Err(StoreError::Conflict);
    }
    if manifest.objects.len() != submission.objects.len() {
        return Err(invalid("corrupt result stage"));
    }
    for (staged, wanted) in manifest.objects.iter().zip(submission.objects.iter()) {
        if staged.oid != wanted.oid || staged.relative_path != wanted.relative_path {
            return Err(invalid("corrupt result stage"));
        }
        let (hash, size) = hash_staged(&dir.join(&staged.byte_sha256))?;
        if hash != staged.byte_sha256 || size != staged.size {
            return Err(invalid("corrupt result stage"));
        }
    }
    Ok(Some(manifest.objects))
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| StoreError::Io(error.to_string()))?;
    file.write_all(bytes)
        .map_err(|error| StoreError::Io(error.to_string()))?;
    file.sync_all()
        .map_err(|error| StoreError::Io(error.to_string()))?;
    Ok(())
}

fn stage_objects(
    objects_root: &Path,
    submission: &ParsedSubmission,
    payload_digest: &str,
) -> Result<Vec<StagedObject>> {
    let source = git_objects(Path::new(&submission.repository))?;
    let dir = staging_dir(objects_root, &submission.idempotency_key);
    if let Some(existing) = reconcile_stage(&dir, payload_digest, submission)? {
        return Ok(existing);
    }
    fs::create_dir_all(&dir).map_err(|error| StoreError::Io(error.to_string()))?;
    let scratch = dir.join("packed-reconstruction");
    // A snapshot left by an interrupted earlier staging is never reused.
    if scratch.exists() {
        fs::remove_dir_all(&scratch).map_err(|error| StoreError::Io(error.to_string()))?;
    }
    // A single exit from reconstruction ensures cleanup on every refusal, too.
    let reconstruction = (|| -> Result<Vec<StagedObject>> {
        let mut staged = Vec::new();
        for object in &submission.objects {
            let source_path =
                crate::migration::safe_join(&source, &object.relative_path).map_err(map_join)?;
            let bytes = match read_object_file(&source_path) {
                Ok(bytes) => bytes,
                Err(StoreError::Invalid(message))
                    if message == "missing object" && !source_path.exists() =>
                {
                    read_packed_object(
                        Path::new(&submission.repository),
                        &scratch,
                        &object.oid,
                        submission.object_format,
                    )?
                }
                Err(error) => return Err(error),
            };
            let byte_sha256 = sha256_hex(&bytes);
            let dest = dir.join(&byte_sha256);
            if dest.is_file() {
                let (hash, size) = hash_staged(&dest)?;
                if hash != byte_sha256 || size != bytes.len() as u64 {
                    return Err(invalid("staged object bytes changed"));
                }
            } else {
                write_file(&dest, &bytes)?;
            }
            staged.push(StagedObject {
                oid: object.oid.clone(),
                relative_path: object.relative_path.clone(),
                byte_sha256,
                size: bytes.len() as u64,
            });
        }
        Ok(staged)
    })();
    let cleanup = if scratch.exists() {
        fs::remove_dir_all(&scratch).map_err(|_| invalid("missing object"))
    } else {
        Ok(())
    };
    cleanup?;
    let staged = reconstruction?;
    let manifest = StageManifest {
        payload_digest: payload_digest.to_string(),
        objects: staged.clone(),
    };
    let encoded = serde_json::to_vec(&manifest).map_err(|error| invalid(&error.to_string()))?;
    write_file(&dir.join("manifest.json"), &encoded)?;
    sync_dir(&dir)?;
    sync_dir(dir.parent().unwrap())?;
    sync_dir(objects_root)?;
    // Rehash what was just fsynced instead of trusting the manifest we wrote.
    reconcile_stage(&dir, payload_digest, submission)?
        .ok_or_else(|| invalid("corrupt result stage"))
}

fn lookup_submission(
    db: &Connection,
    project_store: &str,
    key: &str,
) -> Result<Option<(String, String)>> {
    db.query_row(
        "SELECT submission_id, payload_digest FROM result_submissions WHERE project_store=?1 AND idempotency_key=?2",
        params![project_store, key],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional().map_err(StoreError::from)
}

fn receipt(id: String, key: &str, digest: &str, replayed: bool) -> ResultReceipt {
    ResultReceipt {
        submission_id: id,
        idempotency_key: key.to_string(),
        payload_digest: digest.to_string(),
        replayed,
    }
}

impl SqliteStore {
    pub fn install_contract(&mut self, prepared: &PreparedContract) -> Result<ContractInstall> {
        let parsed =
            PreparedContract::parse_verified(&prepared.raw).map_err(|error| invalid(&error))?;
        if parsed.digest != prepared.digest || parsed.digest != sha256_hex(&prepared.raw) {
            return Err(invalid("changed contract bytes"));
        }
        let path = store_path(&self.connection)?;
        if parsed.project_store != path.to_string_lossy() {
            return Err(invalid("contract belongs to another project"));
        }
        let repository = fs::canonicalize(&parsed.repository)
            .map_err(|_| invalid("contract repository is unavailable"))?;
        if repository.to_string_lossy() != parsed.repository {
            return Err(invalid("contract repository path is not canonical"));
        }
        let tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        schema26(&tx)?;
        let existing: Option<(Vec<u8>, String)> = tx.query_row(
            "SELECT raw_bytes, raw_digest FROM task_contracts WHERE task_id=?1 AND contract_revision=?2",
            params![parsed.task_id.as_str(), integer(parsed.contract_revision)?],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?;
        if let Some((raw, digest)) = existing {
            if sha256_hex(&raw) != digest {
                return Err(StoreError::Corrupt("task contract digest mismatch".into()));
            }
            if raw != parsed.raw || digest != parsed.digest {
                return Err(invalid("changed contract bytes"));
            }
            tx.commit()?;
            return Ok(ContractInstall {
                task_id: parsed.task_id.as_str().to_string(),
                contract_revision: parsed.contract_revision,
                digest,
                replayed: true,
            });
        }
        let current: Option<i64> = tx.query_row(
            "SELECT max(contract_revision) FROM task_contracts WHERE task_id=?1",
            [parsed.task_id.as_str()],
            |row| row.get(0),
        )?;
        if current.unwrap_or(0) + 1 != integer(parsed.contract_revision)? {
            return Err(invalid("contract revision must advance exactly once"));
        }
        if head(&tx)? != parsed.expected_head {
            return Err(StoreError::Conflict);
        }
        let task_exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM tasks WHERE id=?1)",
            [parsed.task_id.as_str()],
            |row| row.get(0),
        )?;
        if !task_exists {
            return Err(invalid("contract task is missing"));
        }
        let version: u32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version < SCOPE_SCHEMA
            && (!parsed.scope_paths.is_empty() || !parsed.named_resources.is_empty())
        {
            return Err(StoreError::UnsupportedSchema(version));
        }
        super::contract_binding::validate_dependency_graph(&tx, &parsed)?;
        tx.execute(
            "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('contract.installed',?1,?2,1,?3)",
            params![parsed.task_id.as_str(), integer(parsed.contract_revision)?, serde_json::json!({"digest": parsed.digest, "route": parsed.route.as_str()}).to_string()],
        )?;
        let sequence = head(&tx)?;
        tx.execute(
            "INSERT INTO task_contracts(task_id,contract_revision,plan_revision,project_store,expected_head,repository,base_oid,object_format,memory_snapshot_id,route,raw_bytes,raw_digest,installed_seq) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![parsed.task_id.as_str(), integer(parsed.contract_revision)?, parsed.plan_revision.map(integer).transpose()?, parsed.project_store, integer(parsed.expected_head)?, parsed.repository, parsed.base_oid, parsed.object_format.as_str(), parsed.memory_snapshot_id, parsed.route.as_str(), parsed.raw, parsed.digest, integer(sequence)?],
        )?;
        for AcceptancePolicy { id, text } in &parsed.acceptance_policies {
            tx.execute(
                "INSERT INTO acceptance_policies(task_id,contract_revision,policy_id,body) VALUES(?1,?2,?3,?4)",
                params![parsed.task_id.as_str(), integer(parsed.contract_revision)?, id, text],
            )?;
        }
        if version >= SCOPE_SCHEMA {
            for (ordinal, path) in parsed.scope_paths.iter().enumerate() {
                tx.execute(
                    "INSERT INTO contract_scope_paths(task_id,contract_revision,ordinal,path,access,certainty) VALUES(?1,?2,?3,?4,?5,?6)",
                    params![parsed.task_id.as_str(), integer(parsed.contract_revision)?, integer(ordinal as u64)?, path.path, path.access.as_str(), path.certainty.as_str()],
                )?;
            }
            for resource in &parsed.named_resources {
                tx.execute(
                    "INSERT INTO contract_named_resources(task_id,contract_revision,name,access) VALUES(?1,?2,?3,?4)",
                    params![parsed.task_id.as_str(), integer(parsed.contract_revision)?, resource.name.as_str(), resource.access.as_str()],
                )?;
            }
        }
        // Same scope, durable for admission. Not a lock and not released on cancel.
        if version >= CLAIMS_SCHEMA {
            for (ordinal, path) in parsed.scope_paths.iter().enumerate() {
                tx.execute(
                    "INSERT INTO resource_claims(task_id,contract_revision,ordinal,kind,resource,access,certainty) VALUES(?1,?2,?3,'path',?4,?5,?6)",
                    params![parsed.task_id.as_str(), integer(parsed.contract_revision)?, integer(ordinal as u64)?, path.path, path.access.as_str(), path.certainty.as_str()],
                )?;
            }
            for (index, resource) in parsed.named_resources.iter().enumerate() {
                tx.execute(
                    "INSERT INTO resource_claims(task_id,contract_revision,ordinal,kind,resource,access,certainty) VALUES(?1,?2,?3,'named',?4,?5,'exact')",
                    params![parsed.task_id.as_str(), integer(parsed.contract_revision)?, integer((64 + index) as u64)?, resource.name.as_str(), resource.access.as_str()],
                )?;
            }
        }
        if version >= 30 {
            super::satisfaction::attach_stored_receipts(&tx, parsed.task_id.as_str())?;
        }
        tx.commit()?;
        Ok(ContractInstall {
            task_id: parsed.task_id.as_str().to_string(),
            contract_revision: parsed.contract_revision,
            digest: parsed.digest,
            replayed: false,
        })
    }

    /// Copy git objects, then insert one untrusted submission. Same key and bytes replay.
    pub fn submit_result(&mut self, raw: &[u8]) -> Result<ResultReceipt> {
        let submission = parse_submission(raw)?;
        let payload_digest = sha256_hex(raw);
        let path = store_path(&self.connection)?;
        let objects_root = path
            .parent()
            .ok_or_else(|| invalid("store path missing"))?
            .join("factory-objects");
        let _lock = object_lock(&objects_root)?;
        {
            let tx = self.connection.transaction()?;
            schema26(&tx)?;
            if let Some((id, digest)) =
                lookup_submission(&tx, &path.to_string_lossy(), &submission.idempotency_key)?
            {
                if digest != payload_digest {
                    return Err(StoreError::Conflict);
                }
                tx.commit()?;
                return Ok(receipt(
                    id,
                    &submission.idempotency_key,
                    &payload_digest,
                    true,
                ));
            }
            check_submission(&tx, &submission)?;
            tx.commit()?;
        }
        let staged = stage_objects(&objects_root, &submission, &payload_digest)?;
        let tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        schema26(&tx)?;
        if let Some((id, digest)) =
            lookup_submission(&tx, &path.to_string_lossy(), &submission.idempotency_key)?
        {
            if digest != payload_digest {
                return Err(StoreError::Conflict);
            }
            tx.commit()?;
            return Ok(receipt(
                id,
                &submission.idempotency_key,
                &payload_digest,
                true,
            ));
        }
        check_submission(&tx, &submission)?;
        let project_store = path.to_string_lossy().into_owned();
        let submission_id = sha256_hex(
            format!(
                "{project_store}\0{}\0{payload_digest}",
                submission.idempotency_key
            )
            .as_bytes(),
        );
        let payload = std::str::from_utf8(raw).map_err(|_| invalid("invalid result submission"))?;
        tx.execute(
            "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('result.submitted',?1,1,1,?2)",
            params![submission_id, serde_json::json!({"payload_digest": payload_digest, "task_id": submission.task_id.as_str(), "attempt_id": submission.attempt_id.as_str()}).to_string()],
        )?;
        tx.execute(
            "INSERT INTO result_submissions(submission_id,project_store,idempotency_key,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,memory_snapshot_id,artifact_manifest,claimed_checks,created_unix_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
            params![submission_id, project_store, submission.idempotency_key, payload_digest, payload, submission.task_id.as_str(), integer(submission.contract_revision)?, submission.contract_digest, submission.attempt_id.as_str(), submission.repository, submission.base_oid, submission.candidate_oid, submission.object_format.as_str(), submission.memory_snapshot_id, submission.artifact_manifest, submission.claimed_checks, jiff::Timestamp::now().as_millisecond()],
        )?;
        for object in &staged {
            tx.execute(
                "INSERT INTO result_objects(submission_id,oid,object_format,relative_path,byte_sha256,size) VALUES(?1,?2,?3,?4,?5,?6)",
                params![submission_id, object.oid, submission.object_format.as_str(), object.relative_path, object.byte_sha256, integer(object.size)?],
            )?;
        }
        tx.commit()?;
        Ok(receipt(
            submission_id,
            &submission.idempotency_key,
            &payload_digest,
            false,
        ))
    }

    /// Fsync object bytes and stop before the submission insert. Retry converges.
    #[cfg(test)]
    pub(crate) fn stage_result_for_retry(&mut self, raw: &[u8]) -> Result<()> {
        let submission = parse_submission(raw)?;
        let payload_digest = sha256_hex(raw);
        let path = store_path(&self.connection)?;
        let objects_root = path
            .parent()
            .ok_or_else(|| invalid("store path missing"))?
            .join("factory-objects");
        let _lock = object_lock(&objects_root)?;
        {
            let tx = self.connection.transaction()?;
            schema26(&tx)?;
            check_submission(&tx, &submission)?;
            tx.commit()?;
        }
        stage_objects(&objects_root, &submission, &payload_digest)?;
        Ok(())
    }

    pub fn show_results(&mut self, id: Option<&str>) -> Result<Vec<ResultView>> {
        let path = store_path(&self.connection)?;
        let tx = self.connection.transaction()?;
        schema26(&tx)?;
        let mut stmt = tx.prepare("SELECT submission_id,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,memory_snapshot_id,artifact_manifest,claimed_checks,payload,payload_digest FROM result_submissions WHERE project_store=?1 AND (?2 IS NULL OR submission_id=?2) ORDER BY submission_id")?;
        let rows = stmt
            .query_map(params![path.to_string_lossy(), id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, String>(12)?,
                    row.get::<_, String>(13)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        let mut views = Vec::new();
        for (
            submission_id,
            task_id,
            contract_revision,
            contract_digest,
            attempt_id,
            repository,
            base_oid,
            candidate_oid,
            object_format,
            memory_snapshot_id,
            artifact_manifest,
            claimed_checks,
            payload,
            digest,
        ) in rows
        {
            if sha256_hex(payload.as_bytes()) != digest {
                return Err(StoreError::Corrupt("result payload digest mismatch".into()));
            }
            let mut objects_stmt = tx.prepare("SELECT oid,relative_path,byte_sha256,size FROM result_objects WHERE submission_id=?1 ORDER BY oid")?;
            let objects = objects_stmt
                .query_map([&submission_id], |object| {
                    Ok(ResultObjectView {
                        oid: object.get(0)?,
                        relative_path: object.get(1)?,
                        byte_sha256: object.get(2)?,
                        size: object.get::<_, i64>(3)? as u64,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            views.push(ResultView {
                submission_id,
                task_id,
                contract_revision,
                contract_digest,
                attempt_id,
                repository,
                base_oid,
                candidate_oid,
                object_format,
                memory_snapshot_id,
                artifact_manifest: serde_json::from_str(&artifact_manifest)
                    .map_err(|_| StoreError::Corrupt("invalid artifact manifest".into()))?,
                claimed_checks: serde_json::from_str(&claimed_checks)
                    .map_err(|_| StoreError::Corrupt("invalid claimed checks".into()))?,
                objects,
            });
        }
        if id.is_some() && views.is_empty() {
            return Err(invalid("result submission not found"));
        }
        Ok(views)
    }
}

fn check_submission(db: &Connection, submission: &ParsedSubmission) -> Result<()> {
    let contract = load_contract(
        db,
        submission.task_id.as_str(),
        submission.contract_revision,
    )?;
    if contract.raw_digest != submission.contract_digest {
        return Err(invalid("changed contract bytes"));
    }
    super::contract_binding::require_result_barrier(db, submission.task_id.as_str(), submission.contract_revision,
        &submission.contract_digest, submission.attempt_id.as_str(), jiff::Timestamp::now().as_millisecond())?;
    if contract.repository != submission.repository
        || contract.base_oid != submission.base_oid
        || contract.object_format != submission.object_format.as_str()
        || contract.memory_snapshot_id != submission.memory_snapshot_id
    {
        return Err(invalid("result does not match the installed contract"));
    }
    let manifest: Vec<UntrustedArtifact> = serde_json::from_str(&submission.artifact_manifest)
        .map_err(|_| invalid("invalid retained artifact manifest"))?;
    if contract.required_outputs.iter().any(|output| !manifest.iter().any(|artifact| &artifact.path == output)) {
        return Err(invalid("result manifest omits a required output"));
    }
    let canonical = fs::canonicalize(&submission.repository)
        .map_err(|_| invalid("result repository is unavailable"))?;
    if canonical.to_string_lossy() != submission.repository
        || canonical.to_string_lossy() != contract.repository
    {
        return Err(invalid("result repository path is not canonical"));
    }
    attempt_matches(
        db,
        submission.attempt_id.as_str(),
        submission.task_id.as_str(),
    )?;
    Ok(())
}

pub fn submit_untrusted_result(project: &Path, document: &Path) -> Result<ResultReceipt> {
    let _guard =
        crate::migration::runtime_mutation(project).map_err(|error| invalid(&error.to_string()))?;
    let bytes =
        crate::migration::read_plan_file(document).map_err(|error| invalid(&error.to_string()))?;
    let mut db =
        crate::migration::open_active(project).map_err(|error| invalid(&error.to_string()))?;
    db.submit_result(&bytes)
}

/// `submit_untrusted_result` for document bytes the ticker read from a
/// worker's submission spool, under the runtime guard (`_held`) the ticker
/// already holds: the same store and idempotent insert.
pub(crate) fn submit_untrusted_result_bytes(_held: &crate::migration::Maintenance, project: &Path, bytes: &[u8]) -> Result<ResultReceipt> {
    let mut db =
        crate::migration::open_active(project).map_err(|error| invalid(&error.to_string()))?;
    db.submit_result(bytes)
}

/// Record one refused submission-spool request as a `spool.request_denied`
/// event of `attempt` (revision = its denial ordinal). At most `cap` are
/// recorded per attempt, so a worker cannot grow the store without bound;
/// returns whether this one was recorded.
pub(crate) fn record_spool_denial(_held: &crate::migration::Maintenance, project: &Path, attempt: &str, request: &str, kind: Option<&str>, reason: &str, cap: u64) -> Result<bool> {
    let mut db = crate::migration::open_active_unchecked(project)
        .map_err(|error| invalid(&error.to_string()))?;
    let tx = db.connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let count: u64 = tx.query_row(
        "SELECT count(*) FROM events WHERE kind='spool.request_denied' AND entity=?1",
        [attempt],
        |row| row.get(0),
    )?;
    if count >= cap {
        return Ok(false);
    }
    tx.execute(
        "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('spool.request_denied',?1,?2,1,?3)",
        params![attempt, integer(count + 1)?, serde_json::json!({"attempt_id": attempt, "request_sha256": request, "kind": kind,
            "reason": reason, "recorded_unix_ms": jiff::Timestamp::now().as_millisecond()}).to_string()],
    )?;
    tx.commit()?;
    Ok(true)
}

pub fn show_results(project: &Path, id: Option<&str>) -> Result<Vec<ResultView>> {
    let mut db =
        crate::migration::open_active(project).map_err(|error| invalid(&error.to_string()))?;
    db.show_results(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::*;
    use std::fs::{self, File};

    fn task_row(db: &Connection) -> Vec<(String, i64, String, String, Option<String>)> {
        let mut stmt = db
            .prepare("SELECT id,revision,state,title,active_attempt FROM tasks ORDER BY id")
            .unwrap();
        stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .unwrap()
        .map(|row| row.unwrap())
        .collect()
    }
    fn user_version(db: &Connection) -> u32 {
        db.query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }
    fn table_exists(db: &Connection, name: &str) -> bool {
        db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?1)",
            [name],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn schema_26_upgrade_preserves_tasks_and_import_requires_current_schema() {
        let fresh = tempfile::tempdir().unwrap();
        let fresh_path = fresh.path().join("state.db");
        let mut created = SqliteStore::create(&fresh_path).unwrap();
        assert_eq!(user_version(&created.connection), crate::store::SCHEMA);
        assert_eq!(
            created
                .connection
                .query_row("SELECT schema_version FROM store_meta", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            crate::store::SCHEMA
        );
        created.import_legacy(&"ab".repeat(32), &[], &[]).unwrap();
        drop(created);

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.db");
        let mut db = SqliteStore::create(&path).unwrap();
        let task = Task {
            id: TaskId::new("kept").unwrap(),
            revision: 1,
            state: TaskState::Queued,
            title: "do not rewrite".into(),
            active_attempt: None,
        };
        db.commit(Commit {
            expected_head: 0,
            mutations: vec![Mutation::Task {
                expected: None,
                next: task,
            }],
        })
        .unwrap();
        let before = task_row(&db.connection);
        drop(db);
        let raw = rusqlite::Connection::open(&path).unwrap();
        crate::store::test_schema::historical(&raw, 25).unwrap();
        drop(raw);
        let mut db = SqliteStore::open(&path).unwrap();
        assert_eq!(user_version(&db.connection), 25);
        assert!(!table_exists(&db.connection, "task_contracts"));
        assert_eq!(task_row(&db.connection), before);
        assert!(matches!(
            db.import_legacy(&"cd".repeat(32), &[], &[]),
            Err(StoreError::UnsupportedSchema(25))
        ));
        assert_eq!(task_row(&db.connection), before);
        db.upgrade_v1().unwrap();
        assert_eq!(user_version(&db.connection), crate::store::SCHEMA);
        assert_eq!(
            db.connection
                .query_row("SELECT schema_version FROM store_meta", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            crate::store::SCHEMA
        );
        assert!(table_exists(&db.connection, "task_contracts"));
        assert!(table_exists(&db.connection, "result_submissions"));
        assert_eq!(task_row(&db.connection), before);
        check_schema(&db.connection).unwrap();
        let snapshot = db.read_snapshot(None).unwrap();
        assert_eq!(snapshot.schema_version, crate::store::SCHEMA);
        assert_eq!(snapshot.tasks.len(), 1);
        assert_eq!(snapshot.tasks[0].title, "do not rewrite");
        drop(db);
        let mut reopened = SqliteStore::open(&path).unwrap();
        check_schema(&reopened.connection).unwrap();
        assert_eq!(
            reopened.read_snapshot(None).unwrap().tasks[0].title,
            "do not rewrite"
        );
    }

    fn write_loose(repo: &Path, bytes: &[u8]) -> (String, String) {
        let oid = sha256_hex(bytes);
        let relative = format!("{}/{}", &oid[..2], &oid[2..]);
        let path = repo.join(".git/objects").join(&relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
        (oid, relative)
    }
    struct Fixture {
        _root: tempfile::TempDir,
        db_path: PathBuf,
        repo: PathBuf,
        task: TaskId,
        attempt: AttemptId,
        head: u64,
        base: String,
        candidate: String,
    }
    fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let db_path = root.path().join("state.db");
        let mut db = SqliteStore::create(&db_path).unwrap();
        let task = TaskId::new("task").unwrap();
        let attempt = AttemptId::new("attempt-1").unwrap();
        let other = AttemptId::new("attempt-other").unwrap();
        db.commit(Commit {
            expected_head: 0,
            mutations: vec![
                Mutation::Task {
                    expected: None,
                    next: Task {
                        id: task.clone(),
                        revision: 1,
                        state: TaskState::Running,
                        title: "work".into(),
                        active_attempt: None,
                    },
                },
                Mutation::Task {
                    expected: None,
                    next: Task {
                        id: TaskId::new("other").unwrap(),
                        revision: 1,
                        state: TaskState::Running,
                        title: "other".into(),
                        active_attempt: None,
                    },
                },
                Mutation::Attempt {
                    expected: None,
                    next: Attempt {
                        id: attempt.clone(),
                        task: task.clone(),
                        revision: 1,
                        state: AttemptState::Running,
                        snapshot: None,
                        reservation: "slot-1".into(),
                        termination_observed: false,
                    },
                },
                Mutation::Attempt {
                    expected: None,
                    next: Attempt {
                        id: other,
                        task: TaskId::new("other").unwrap(),
                        revision: 1,
                        state: AttemptState::Running,
                        snapshot: None,
                        reservation: "slot-2".into(),
                        termination_observed: false,
                    },
                },
            ],
        })
        .unwrap();
        let head = db.read_snapshot(None).unwrap().head;
        drop(db);
        let repo = root.path().join("repo");
        fs::create_dir_all(repo.join(".git/objects")).unwrap();
        let (base, _) = write_loose(&repo, b"base-object");
        let (candidate, _) = write_loose(&repo, b"candidate-object");
        Fixture {
            _root: root,
            db_path,
            repo,
            task,
            attempt,
            head,
            base,
            candidate,
        }
    }
    fn contract_bytes(fixture: &Fixture, deliverable: &str) -> Vec<u8> {
        let mut bytes = serde_json::to_vec_pretty(&serde_json::json!({
            "version": 1,
            "project_store": fixture.db_path.canonicalize().unwrap().to_string_lossy(),
            "expected_head": fixture.head,
            "task_id": fixture.task.as_str(),
            "contract_revision": 1,
            "deliverable": deliverable,
            "non_goals": "no launch",
            "acceptance_policies": [{"id": "builds", "text": "tests pass"}],
            "repository": fixture.repo.canonicalize().unwrap().to_string_lossy(),
            "base_oid": fixture.base,
            "object_format": "sha256",
            "dependencies": [],
            "capability_flags": [],
            "profile_kind": "codex",
            "retry_class": "none",
            "result_schema_id": "result-v1",
            "route": "verify_only",
            "authority": {"id": "owner-approval-policy", "revision": 1, "digest": "ab".repeat(32)}
        }))
        .unwrap();
        bytes.push(b'\n');
        bytes
    }
    fn install(db: &mut SqliteStore, bytes: &[u8]) -> ContractInstall {
        let prepared = PreparedContract::parse_verified(bytes).unwrap();
        db.install_contract(&prepared).unwrap()
    }
    fn submission(fixture: &Fixture, key: &str, attempt: &str, contract_digest: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "idempotency_key": key,
            "task_id": fixture.task.as_str(),
            "contract_revision": 1,
            "contract_digest": contract_digest,
            "attempt_id": attempt,
            "repository": fixture.repo.canonicalize().unwrap().to_string_lossy(),
            "base_oid": fixture.base,
            "candidate_oid": fixture.candidate,
            "object_format": "sha256",
            "artifact_manifest": [{"path": "src/lib.rs", "oid": fixture.candidate}],
            "claimed_checks": ["cargo test"],
            "objects": [
                {"oid": fixture.base, "relative_path": format!("{}/{}", &fixture.base[..2], &fixture.base[2..])},
                {"oid": fixture.candidate, "relative_path": format!("{}/{}", &fixture.candidate[..2], &fixture.candidate[2..])}
            ]
        })).unwrap()
    }
    fn submission_count(db: &Connection) -> i64 {
        db.query_row("SELECT count(*) FROM result_submissions", [], |row| {
            row.get(0)
        })
        .unwrap()
    }


    #[test]
    fn result_ingress_refuses_wrong_attempt_changed_contract_missing_escape_and_oversize() {
        let fixture = fixture();
        let mut db = SqliteStore::open(&fixture.db_path).unwrap();
        let original = contract_bytes(&fixture, "ship the widget");
        let installed = install(&mut db, &original);
        let good = submission(
            &fixture,
            "key-1",
            fixture.attempt.as_str(),
            &installed.digest,
        );
        let wrong_attempt = submission(&fixture, "key-wrong", "attempt-other", &installed.digest);
        assert!(
            matches!(db.submit_result(&wrong_attempt), Err(StoreError::Invalid(message)) if message.contains("wrong attempt"))
        );
        let missing_attempt = submission(
            &fixture,
            "key-missing",
            "attempt-missing",
            &installed.digest,
        );
        assert!(
            matches!(db.submit_result(&missing_attempt), Err(StoreError::Invalid(message)) if message.contains("wrong attempt"))
        );
        let changed = submission(
            &fixture,
            "key-changed",
            fixture.attempt.as_str(),
            &"cd".repeat(32),
        );
        assert!(
            matches!(db.submit_result(&changed), Err(StoreError::Invalid(message)) if message.contains("changed contract bytes"))
        );
        let mut missing = serde_json::from_slice::<serde_json::Value>(&good).unwrap();
        missing["idempotency_key"] = serde_json::json!("key-missing-object");
        missing["objects"].as_array_mut().unwrap().push(serde_json::json!({"oid": "ab".repeat(32), "relative_path": format!("{}/{}", "ab", "ab".repeat(31))}));
        assert!(
            matches!(db.submit_result(&serde_json::to_vec(&missing).unwrap()), Err(StoreError::Invalid(message)) if message.contains("missing object"))
        );
        let mut escape = serde_json::from_slice::<serde_json::Value>(&good).unwrap();
        escape["idempotency_key"] = serde_json::json!("key-escape");
        escape["objects"][0]["relative_path"] = serde_json::json!("../../secret");
        fs::write(fixture._root.path().join("secret"), b"nope").unwrap();
        assert!(
            matches!(db.submit_result(&serde_json::to_vec(&escape).unwrap()), Err(StoreError::Invalid(message)) if message.contains("path traversal"))
        );
        let base_path = fixture
            .repo
            .join(".git/objects")
            .join(&fixture.base[..2])
            .join(&fixture.base[2..]);
        let saved = fs::read(&base_path).unwrap();
        File::create(&base_path)
            .unwrap()
            .set_len(OBJECT_LIMIT + 1)
            .unwrap();
        let mut oversized = serde_json::from_slice::<serde_json::Value>(&good).unwrap();
        oversized["idempotency_key"] = serde_json::json!("key-huge");
        assert!(
            matches!(db.submit_result(&serde_json::to_vec(&oversized).unwrap()), Err(StoreError::Invalid(message)) if message.contains("16 MiB"))
        );
        fs::write(&base_path, saved).unwrap();
        assert_eq!(submission_count(&db.connection), 0);
        let receipt = db.submit_result(&good).unwrap();
        assert!(!receipt.replayed);
        let again = db.submit_result(&good).unwrap();
        assert!(again.replayed);
        assert_eq!(again.submission_id, receipt.submission_id);
        let mut conflict = serde_json::from_slice::<serde_json::Value>(&good).unwrap();
        conflict["claimed_checks"] = serde_json::json!(["different"]);
        assert!(matches!(
            db.submit_result(&serde_json::to_vec(&conflict).unwrap()),
            Err(StoreError::Conflict)
        ));
        assert_eq!(submission_count(&db.connection), 1);
        let shown = db.show_results(Some(&receipt.submission_id)).unwrap();
        let value = serde_json::to_value(&shown[0]).unwrap();
        assert!(value.get("verified").is_none());
        assert_eq!(value["claimed_checks"], serde_json::json!(["cargo test"]));
    }

    #[test]
    fn kill_between_stage_and_commit_retries_as_one_row() {
        let fixture = fixture();
        let mut db = SqliteStore::open(&fixture.db_path).unwrap();
        let original = contract_bytes(&fixture, "ship the widget");
        let installed = install(&mut db, &original);
        let raw = submission(
            &fixture,
            "crash-key",
            fixture.attempt.as_str(),
            &installed.digest,
        );
        db.stage_result_for_retry(&raw).unwrap();
        assert_eq!(submission_count(&db.connection), 0);
        let loose = fixture
            .repo
            .join(".git/objects")
            .join(&fixture.candidate[..2])
            .join(&fixture.candidate[2..]);
        fs::remove_file(&loose).unwrap();
        drop(db);
        let mut db = SqliteStore::open(&fixture.db_path).unwrap();
        let first = db.submit_result(&raw).unwrap();
        let second = db.submit_result(&raw).unwrap();
        assert_eq!(first.submission_id, second.submission_id);
        assert!(second.replayed);
        assert_eq!(submission_count(&db.connection), 1);
    }

    #[test]
    fn gitfile_worktree_stages_common_loose_objects_and_skips_dirty_files() {
        let fixture = fixture();
        let gitdir = fixture.repo.join(".git/worktrees/wt");
        fs::create_dir_all(&gitdir).unwrap();
        fs::write(gitdir.join("commondir"), "../..\n").unwrap();
        let gitdir = gitdir.canonicalize().unwrap();
        let worktree = fixture._root.path().join("wt");
        fs::create_dir_all(&worktree).unwrap();
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", gitdir.display()),
        )
        .unwrap();
        let dirty = b"uncommitted worktree bytes";
        fs::write(worktree.join("dirty.txt"), dirty).unwrap();
        let worktree = worktree.canonicalize().unwrap();
        fs::write(
            gitdir.join("gitdir"),
            format!("{}\n", worktree.join(".git").display()),
        )
        .unwrap();
        let mut db = SqliteStore::open(&fixture.db_path).unwrap();
        let mut contract = serde_json::from_slice::<serde_json::Value>(&contract_bytes(
            &fixture,
            "ship the widget",
        ))
        .unwrap();
        contract["repository"] = serde_json::json!(worktree.display().to_string());
        let mut contract_bytes = serde_json::to_vec_pretty(&contract).unwrap();
        contract_bytes.push(b'\n');
        let installed = install(&mut db, &contract_bytes);
        let mut raw = serde_json::from_slice::<serde_json::Value>(&submission(
            &fixture,
            "gitfile",
            fixture.attempt.as_str(),
            &installed.digest,
        ))
        .unwrap();
        raw["repository"] = serde_json::json!(worktree.display().to_string());
        let receipt = db
            .submit_result(&serde_json::to_vec(&raw).unwrap())
            .unwrap();
        let shown = db.show_results(Some(&receipt.submission_id)).unwrap();
        let loose = fs::read(
            fixture
                .repo
                .join(".git/objects")
                .join(&fixture.base[..2])
                .join(&fixture.base[2..]),
        )
        .unwrap();
        assert!(shown[0].objects.iter().any(|object| {
            object.byte_sha256 == sha256_hex(&loose) && object.size == loose.len() as u64
        }));
        let staged = super::staging_dir(
            &fixture.db_path.parent().unwrap().join("factory-objects"),
            "gitfile",
        );
        for entry in fs::read_dir(&staged).unwrap() {
            let path = entry.unwrap().path();
            if path.is_file() {
                assert_ne!(fs::read(&path).unwrap(), dirty);
            }
        }
        let linked = fixture._root.path().join("linked");
        fs::create_dir_all(&linked).unwrap();
        std::os::unix::fs::symlink(fixture.repo.join(".git"), linked.join(".git")).unwrap();
        assert!(matches!(
            super::git_objects(&linked),
            Err(StoreError::Invalid(message)) if message.contains("symlink")
        ));
        let relative = fixture._root.path().join("relative");
        fs::create_dir_all(&relative).unwrap();
        fs::write(relative.join(".git"), "gitdir: ../repo/.git/worktrees/wt\n").unwrap();
        assert!(matches!(
            super::git_objects(&relative),
            Err(StoreError::Invalid(message)) if message.contains("not absolute")
        ));
        let absolute_wt = fixture._root.path().join("abs-wt");
        fs::create_dir_all(&absolute_wt).unwrap();
        let absolute_gitdir = fixture.repo.join(".git/worktrees/abs");
        fs::create_dir_all(&absolute_gitdir).unwrap();
        let absolute_gitdir = absolute_gitdir.canonicalize().unwrap();
        let absolute_wt = absolute_wt.canonicalize().unwrap();
        fs::write(
            absolute_gitdir.join("commondir"),
            format!(
                "{}\n",
                fixture.repo.join(".git").canonicalize().unwrap().display()
            ),
        )
        .unwrap();
        fs::write(
            absolute_wt.join(".git"),
            format!("gitdir: {}\n", absolute_gitdir.display()),
        )
        .unwrap();
        fs::write(
            absolute_gitdir.join("gitdir"),
            format!("{}\n", absolute_wt.join(".git").display()),
        )
        .unwrap();
        assert!(matches!(
            super::git_objects(&absolute_wt),
            Err(StoreError::Invalid(message)) if message.contains("absolute")
        ));
        let other = fixture._root.path().join("other-repo");
        fs::create_dir_all(other.join(".git/objects")).unwrap();
        fs::write(other.join(".git/objects/stolen"), b"from-another-repo").unwrap();
        let foreign_gitdir = other.join("nested/wt");
        fs::create_dir_all(&foreign_gitdir).unwrap();
        fs::write(foreign_gitdir.join("commondir"), "../..\n").unwrap();
        let foreign_gitdir = foreign_gitdir.canonicalize().unwrap();
        let foreign_wt = fixture._root.path().join("foreign-wt");
        fs::create_dir_all(&foreign_wt).unwrap();
        let foreign_wt = foreign_wt.canonicalize().unwrap();
        fs::write(
            foreign_wt.join(".git"),
            format!("gitdir: {}\n", foreign_gitdir.display()),
        )
        .unwrap();
        fs::write(
            foreign_gitdir.join("gitdir"),
            format!("{}\n", foreign_wt.join(".git").display()),
        )
        .unwrap();
        assert!(matches!(
            super::git_objects(&foreign_wt),
            Err(StoreError::Invalid(message)) if message.contains("common git directory")
        ));
    }

    fn count(db: &Connection, sql: &str) -> i64 {
        db.query_row(sql, [], |row| row.get(0)).unwrap()
    }



    #[test]
    fn upgrade_v1_from_31_to_32_and_create_end_at_user_version_32() {
        let fresh = tempfile::tempdir().unwrap();
        let mut created = SqliteStore::create(&fresh.path().join("state.db")).unwrap();
        assert_eq!(user_version(&created.connection), crate::store::SCHEMA);
        assert_eq!(
            created
                .connection
                .query_row("SELECT schema_version FROM store_meta", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            crate::store::SCHEMA
        );
        assert!(table_exists(&created.connection, "contract_scope_paths"));
        assert!(table_exists(
            &created.connection,
            "contract_named_resources"
        ));
        created.import_legacy(&"ab".repeat(32), &[], &[]).unwrap();
        drop(created);

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.db");
        let db = SqliteStore::create(&path).unwrap();
        drop(db);
        let raw = rusqlite::Connection::open(&path).unwrap();
        crate::store::test_schema::historical(&raw, 31)
        .unwrap();
        drop(raw);
        let mut db = SqliteStore::open(&path).unwrap();
        assert_eq!(user_version(&db.connection), 31);
        assert!(!table_exists(&db.connection, "contract_scope_paths"));
        assert!(!table_exists(&db.connection, "contract_named_resources"));
        assert!(matches!(
            db.import_legacy(&"cd".repeat(32), &[], &[]),
            Err(StoreError::UnsupportedSchema(31))
        ));
        assert_eq!(user_version(&db.connection), 31);
        assert!(!table_exists(&db.connection, "contract_scope_paths"));
        db.upgrade_v1().unwrap();
        assert_eq!(user_version(&db.connection), crate::store::SCHEMA);
        assert_eq!(
            db.connection
                .query_row("SELECT schema_version FROM store_meta", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            crate::store::SCHEMA
        );
        assert!(table_exists(&db.connection, "contract_scope_paths"));
        assert!(table_exists(&db.connection, "contract_named_resources"));
        assert_eq!(
            count(&db.connection, "SELECT count(*) FROM contract_scope_paths"),
            0
        );
        assert_eq!(
            count(
                &db.connection,
                "SELECT count(*) FROM contract_named_resources"
            ),
            0
        );
        check_schema(&db.connection).unwrap();
        db.import_legacy(&"cd".repeat(32), &[], &[]).unwrap();
        drop(db);
        let reopened = SqliteStore::open(&path).unwrap();
        assert_eq!(user_version(&reopened.connection), crate::store::SCHEMA);
        assert!(table_exists(&reopened.connection, "contract_scope_paths"));
    }
}

//! Durable memory-review obligations for worker `## Remember` sections.
//!
//! A finished report becomes an inbox `thread-state` item that names only a
//! report hash. Archiving that item must not drop the memory decision. This
//! module stores one obligation per (thread, report hash, Remember hash) in
//! `.state/memory-review.json`, shows unresolved work in `context`, and links
//! an explicit `proposed` disposition to a durably saved candidate file.
//!
//! Remember text is evidence, never instructions. Ingest never edits
//! `MEMORY.md`, `memory/*.md` authority files, or SQLite heads. Legacy
//! candidates live under `memory/candidates/`; SQLite-memory projects use
//! `.state/memory-review-candidates/` so projections are never mistaken for
//! authority. Coordinator summaries never forge worker `task_id`,
//! `attempt_id`, or `input_snapshot_id`; `memory propose` remains a
//! state-store-only worker intake with genuine identities.
//!
//! Revised reports retain prior dispositions: a new report hash adds its own
//! obligation and never auto-clears an earlier unresolved one. The same hash
//! re-ingested never stacks a duplicate.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

const STATE_VERSION: u32 = 1;
const STATE_FILE: &str = ".state/memory-review.json";
const LOCK_FILE: &str = ".state/memory-review.lock";
const LEGACY_CANDIDATE_DIR: &str = "memory/candidates";
const SQLITE_CANDIDATE_DIR: &str = ".state/memory-review-candidates";
/// Content-addressed Remember evidence. `threads/<id>.md` is replaced on the
/// next copy, so the full Remember section is retained here under its digest;
/// the obligation keeps `remember_hash` and reads verify it.
const EVIDENCE_DIR: &str = ".state/memory-review-evidence";
const MAX_OBLIGATIONS: usize = 512;
const MAX_REPORT_BYTES: u64 = 1024 * 1024;
const MAX_REMEMBER_CHARS: usize = 32_000;
const MAX_EXCERPT_CHARS: usize = 800;
const MAX_REASON_CHARS: usize = 2000;
const MAX_TITLE_CHARS: usize = 200;
const MAX_BODY_BYTES: u64 = 64 * 1024;
/// Bounded deferred reminders: first notice plus at most two follow-ups.
pub const MAX_NOTIFICATIONS: u32 = 3;
/// Deferred follow-ups wait at least a day so the ticker cannot spam.
const REMINDER_COOLDOWN_SECS: i64 = 24 * 3600;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pending,
    Proposed,
    Rejected,
    Deferred,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Status::Pending => "pending",
            Status::Proposed => "proposed",
            Status::Rejected => "rejected",
            Status::Deferred => "deferred",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Obligation {
    pub id: String,
    pub thread_id: String,
    pub report_path: String,
    pub report_hash: String,
    pub remember_hash: String,
    /// First 800 sanitized chars of the Remember section: data, not instructions.
    pub excerpt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_notice: Option<String>,
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<String>,
    /// SHA-256 of the candidate file bytes at link time. Later reads reject
    /// the link when the file bytes differ, so post-link edits cannot change
    /// the evidence the disposition names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub notified: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_notified: Option<String>,
    pub created: String,
    pub updated: String,
}

impl Default for Obligation {
    fn default() -> Self {
        Obligation {
            id: String::new(),
            thread_id: String::new(),
            report_path: String::new(),
            report_hash: String::new(),
            remember_hash: String::new(),
            excerpt: String::new(),
            source_notice: None,
            status: Status::Pending,
            candidate: None,
            candidate_digest: None,
            reason: None,
            notified: 0,
            last_notified: None,
            created: String::new(),
            updated: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default, deny_unknown_fields)]
struct State {
    version: u32,
    obligations: Vec<Obligation>,
}

/// Memory owner for candidate routing. Read directly from the marker so this
/// module works with and without the `state-store` feature and never creates
/// ownership markers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryOwner {
    Legacy,
    Sqlite,
}

pub fn memory_owner(project_dir: &Path) -> Result<MemoryOwner> {
    // Half-migrated journals refuse with a canonical recovery pointer.
    for relative in [
        ".state/migration/journal.json",
        ".state/migration/memory-journal.json",
    ] {
        if project_dir.join(relative).exists() && !project_dir.join(".state/format.json").exists() {
            bail!(
                "project migration is incomplete ({} exists without .state/format.json); run `migration status` then `migration recover --writers-stopped`, or `migration abort` before cutover",
                relative
            );
        }
    }
    let marker = project_dir.join(".state/format.json");
    if !marker.exists() {
        return Ok(MemoryOwner::Legacy);
    }
    let bytes = bounded_read(&marker, 64 * 1024)
        .with_context(|| format!("cannot read {}", marker.display()))?;
    let text = std::str::from_utf8(&bytes).context("ownership marker is not UTF-8")?;
    let value: serde_json::Value =
        serde_json::from_str(text).context("ownership marker does not parse")?;
    match value.get("memory").and_then(|v| v.as_str()) {
        None | Some("legacy-markdown") => Ok(MemoryOwner::Legacy),
        Some("sqlite-v1") => Ok(MemoryOwner::Sqlite),
        Some(other) => bail!("unknown format.memory `{other}`; preserve and repair .state/format.json"),
    }
}

pub fn candidate_dir(project_dir: &Path) -> Result<PathBuf> {
    Ok(match memory_owner(project_dir)? {
        MemoryOwner::Legacy => project_dir.join(LEGACY_CANDIDATE_DIR),
        MemoryOwner::Sqlite => project_dir.join(SQLITE_CANDIDATE_DIR),
    })
}

fn bounded_read(path: &Path, limit: u64) -> Result<Vec<u8>> {
    use std::io::Read;
    let file = std::fs::File::open(path)?;
    ensure!(file.metadata()?.is_file(), "{} is not a regular file", path.display());
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= limit, "{} exceeds bounds", path.display());
    Ok(bytes)
}

fn state_path(project_dir: &Path) -> PathBuf {
    project_dir.join(STATE_FILE)
}

fn lock_state(project_dir: &Path) -> Result<std::fs::File> {
    let path = project_dir.join(LOCK_FILE);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::File::options().create(true).write(true).open(&path)?;
    file.lock()?;
    Ok(file)
}

fn load_locked(project_dir: &Path) -> Result<State> {
    let path = state_path(project_dir);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(State { version: STATE_VERSION, obligations: Vec::new() }),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    ensure!(bytes.len() <= 1024 * 1024, "{} exceeds 1 MiB; preserve and repair it", path.display());
    let state: State = serde_json::from_slice(&bytes)
        .with_context(|| format!("{} is invalid; preserve and repair it before resuming", path.display()))?;
    ensure!(state.version == STATE_VERSION, "{} has unknown version {}", path.display(), state.version);
    ensure!(state.obligations.len() <= MAX_OBLIGATIONS, "obligation store exceeds bounds");
    Ok(state)
}

fn save_locked(project_dir: &Path, state: &State) -> Result<()> {
    ensure!(state.obligations.len() <= MAX_OBLIGATIONS, "obligation store exceeds bounds");
    let mut bytes = serde_json::to_vec_pretty(state)?;
    bytes.push(b'\n');
    ensure!(bytes.len() <= 1024 * 1024, "obligation store exceeds 1 MiB");
    crate::project::write_atomic(&state_path(project_dir), &bytes)
}

/// Read-only load for `context` and `doctor`. Never creates or repairs state.
pub fn load(project_dir: &Path) -> Result<Vec<Obligation>> {
    let path = state_path(project_dir);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    if bytes.len() > 1024 * 1024 {
        bail!("{} exceeds 1 MiB; preserve and repair it", path.display());
    }
    let state: State = serde_json::from_slice(&bytes)
        .with_context(|| format!("{} is invalid; preserve and repair it", path.display()))?;
    ensure!(state.version == STATE_VERSION, "{} has unknown version {}", path.display(), state.version);
    Ok(state.obligations)
}

/// Unresolved obligations for `context`: the shown page plus the true total.
/// Load errors propagate so context prints them instead of a false `(none)`.
pub struct PendingView {
    pub shown: Vec<Obligation>,
    pub total: usize,
}

pub const CONTEXT_PAGE: usize = 20;

pub fn pending_for_context(project_dir: &Path) -> Result<PendingView> {
    let mut all: Vec<Obligation> = load(project_dir)?
        .into_iter()
        .filter(|o| matches!(o.status, Status::Pending | Status::Deferred))
        .collect();
    all.sort_by(|a, b| a.created.cmp(&b.created).then(a.id.cmp(&b.id)));
    let total = all.len();
    all.truncate(CONTEXT_PAGE);
    Ok(PendingView { shown: all, total })
}

fn evidence_path(project_dir: &Path, remember_hash: &str) -> Result<PathBuf> {
    ensure!(
        remember_hash.len() == 64 && remember_hash.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid Remember digest"
    );
    Ok(project_dir.join(EVIDENCE_DIR).join(format!("{remember_hash}.txt")))
}

/// Retain the full Remember section under its digest. Immutable: an existing
/// file with different bytes is corruption and fails closed.
fn retain_evidence(project_dir: &Path, remember_hash: &str, remember: &str) -> Result<()> {
    let dir = project_dir.join(EVIDENCE_DIR);
    std::fs::create_dir_all(&dir)?;
    let path = evidence_path(project_dir, remember_hash)?;
    if path.is_file() {
        let existing = bounded_read(&path, MAX_BODY_BYTES * 4)?;
        ensure!(
            crate::thread::sha256_hex(&existing) == remember_hash,
            "{} is corrupt; preserve and repair it",
            path.display()
        );
        return Ok(());
    }
    crate::project::write_atomic(&path, remember.as_bytes())
}

/// Read retained Remember bytes after verifying the digest.
pub fn read_evidence(project_dir: &Path, remember_hash: &str) -> Result<String> {
    let path = evidence_path(project_dir, remember_hash)?;
    let bytes = bounded_read(&path, MAX_BODY_BYTES * 4)?;
    ensure!(
        crate::thread::sha256_hex(&bytes) == remember_hash,
        "{} does not match its digest; preserve and repair it",
        path.display()
    );
    String::from_utf8(bytes).context("retained Remember evidence is not UTF-8")
}

/// Extract the `## Remember` section body, if non-empty. The match is the
/// literal second-level heading; content ends at the next `## ` heading.
pub fn extract_remember(report: &str) -> Option<String> {
    let mut lines = report.lines();
    let mut body: Option<Vec<&str>> = None;
    for line in &mut lines {
        if body.is_none() {
            if line.trim() == "## Remember" {
                body = Some(Vec::new());
            }
            continue;
        }
        let current = body.as_mut().unwrap();
        if line.starts_with("## ") {
            break;
        }
        current.push(line);
    }
    let text = body?.join("\n").trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

fn sanitize_excerpt(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() && c != '\n' && c != '\t' { ' ' } else { c })
        .collect::<String>()
        .chars()
        .take(MAX_EXCERPT_CHARS)
        .collect::<String>()
        .trim()
        .to_string()
}

fn sanitize_reason(reason: &str) -> Result<String> {
    let cleaned: String = reason
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string();
    ensure!(!cleaned.is_empty(), "a reason is required");
    ensure!(cleaned.chars().count() <= MAX_REASON_CHARS, "reason exceeds {MAX_REASON_CHARS} characters");
    Ok(cleaned)
}

fn validate_thread_id(thread_id: &str) -> Result<()> {
    crate::thread::validate_id(thread_id)
}

fn validate_obligation_id(id: &str) -> Result<()> {
    ensure!(!id.is_empty() && id.len() <= 64, "invalid obligation id");
    ensure!(!id.starts_with('.') && !id.contains("..") && !id.contains('/'), "invalid obligation id");
    ensure!(id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'), "invalid obligation id");
    Ok(())
}

fn validate_candidate_id(id: &str) -> Result<()> {
    ensure!(!id.is_empty() && id.len() <= 64, "invalid candidate id");
    ensure!(!id.starts_with('.') && !id.contains("..") && !id.contains('/') && !id.contains('\\'), "invalid candidate id");
    ensure!(id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'), "invalid candidate id");
    Ok(())
}

pub fn obligation_id(thread_id: &str, report_hash: &str, remember_hash: &str) -> String {
    let digest = crate::thread::sha256_hex(format!("{thread_id}\n{report_hash}\n{remember_hash}").as_bytes());
    format!("mr-{thread_id}-{}", &digest[..12])
}

/// Ingest one home report. Returns the obligation when the report carries a
/// non-empty `## Remember`, or `None` when there is nothing to review.
/// Re-ingesting the same (thread, report hash, Remember hash) reuses the
/// existing obligation; a revised report hash adds a new one and retains the
/// prior disposition.
pub fn ingest_report(
    project_dir: &Path,
    thread_id: &str,
    report_hash: &str,
    report_text: &str,
    source_notice: Option<&str>,
) -> Result<Option<Obligation>> {
    validate_thread_id(thread_id)?;
    ensure!(!report_hash.is_empty() && report_hash.len() <= 256, "invalid report hash");
    let Some(remember) = extract_remember(report_text) else { return Ok(None) };
    ensure!(remember.chars().count() <= MAX_REMEMBER_CHARS, "## Remember exceeds {MAX_REMEMBER_CHARS} characters");
    let remember_hash = crate::thread::sha256_hex(remember.as_bytes());
    // Retain the source bytes before linking: the home report is mutable and
    // the excerpt alone cannot be re-verified later.
    retain_evidence(project_dir, &remember_hash, &remember)?;
    let id = obligation_id(thread_id, report_hash, &remember_hash);
    let excerpt = sanitize_excerpt(&remember);
    let now = crate::project::now();

    let _lock = lock_state(project_dir)?;
    let mut state = load_locked(project_dir)?;
    if let Some(existing) = state.obligations.iter().find(|o| o.id == id) {
        return Ok(Some(existing.clone()));
    }
    // Same-thread/same-hash retries must not stack: an existing obligation for
    // this exact report content is the same review, even if the notice differs.
    if let Some(existing) = state
        .obligations
        .iter()
        .find(|o| o.thread_id == thread_id && o.report_hash == report_hash && o.remember_hash == remember_hash)
    {
        return Ok(Some(existing.clone()));
    }
    ensure!(state.obligations.len() < MAX_OBLIGATIONS, "obligation store is full");
    let obligation = Obligation {
        id: id.clone(),
        thread_id: thread_id.to_string(),
        report_path: format!("threads/{thread_id}.md"),
        report_hash: report_hash.to_string(),
        remember_hash,
        excerpt,
        source_notice: source_notice.map(|s| s.to_string()),
        status: Status::Pending,
        candidate: None,
        candidate_digest: None,
        reason: None,
        notified: 0,
        last_notified: None,
        created: now.clone(),
        updated: now,
    };
    state.obligations.push(obligation.clone());
    save_locked(project_dir, &state)?;
    Ok(Some(obligation))
}

/// Ingest the home copy `threads/<id>.md` for one thread. Pins the file digest
/// of the bytes actually parsed: the tick writes the home file before updating
/// the thread record, and fixtures hand-write home files, so record/file
/// disagreement is a normal state, not a refusal signal. Pinning the digest of
/// the parsed bytes (with retained evidence verified against it) is always
/// truthful; the record only supplies a review-notice id when that notice also
/// names the pinned bytes.
pub fn ingest_thread(project_dir: &Path, thread_id: &str) -> Result<Option<Obligation>> {
    validate_thread_id(thread_id)?;
    let report_file = project_dir.join(format!("threads/{thread_id}.md"));
    if !report_file.is_file() {
        return Ok(None);
    }
    let bytes = bounded_read(&report_file, MAX_REPORT_BYTES)
        .with_context(|| format!("cannot read {}", report_file.display()))?;
    let text = std::str::from_utf8(&bytes).context("home report is not UTF-8")?;
    let file_hash = crate::thread::sha256_hex(&bytes);
    let source_notice = std::fs::read_to_string(project_dir.join(format!("threads/{thread_id}.toml")))
        .ok()
        .and_then(|toml| toml::from_str::<toml::Value>(&toml).ok())
        .and_then(|v| {
            let pending = v.get("pending_review_notice")?;
            let hash = pending.get("report_hash")?.as_str()?;
            // A notice for older bytes must not attach to this obligation.
            if hash == file_hash {
                pending.get("id")?.as_str().map(|s| s.to_string())
            } else {
                None
            }
        });
    ingest_report(project_dir, thread_id, &file_hash, text, source_notice.as_deref())
}

/// Ingest every home report `threads/t-*.md`. Per-thread isolated like the
/// tick's copy pass: every thread is attempted, and failures are all reported
/// instead of aborting the rest, so one bad thread cannot block the others.
pub fn ingest_all(project_dir: &Path) -> Result<Vec<Obligation>> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(project_dir.join("threads")) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e.into()),
    };
    let mut ids: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(id) = name.strip_suffix(".md").filter(|s| s.starts_with("t-")) {
            if crate::thread::validate_id(id).is_ok() {
                ids.push(id.to_string());
            }
        }
    }
    ids.sort();
    ids.truncate(1024);
    let mut failures = Vec::new();
    for id in ids {
        match ingest_thread(project_dir, &id) {
            Ok(Some(obligation)) => out.push(obligation),
            Ok(None) => {}
            Err(error) => failures.push(format!("{id}: {error:#}")),
        }
    }
    ensure!(failures.is_empty(), "memory-review ingest: {}", failures.join("; "));
    Ok(out)
}

pub fn get(project_dir: &Path, id: &str) -> Result<Obligation> {
    validate_obligation_id(id)?;
    let obligation = load(project_dir)?
        .into_iter()
        .find(|o| o.id == id)
        .with_context(|| format!("no memory-review obligation `{id}`"))?;
    // Retained evidence must verify against the pinned digest on every read:
    // the home report is mutable, so only the content-addressed copy proves
    // what the excerpt was taken from.
    read_evidence(project_dir, &obligation.remember_hash)?;
    // A `proposed` link names exact bytes: reject the read when the file is
    // missing, moved, or changed after the link instead of showing a stale id.
    if obligation.status == Status::Proposed {
        let candidate = obligation.candidate.as_deref().context("proposed obligation has no candidate; preserve and repair the store")?;
        let digest = obligation.candidate_digest.as_deref().context("proposed obligation has no pinned digest; preserve and repair the store")?;
        validate_candidate_id(candidate)?;
        let path = candidate_dir(project_dir)?.join(format!("{candidate}.md"));
        let current = verify_candidate_file(&path, candidate, &obligation)?;
        ensure!(
            current == digest,
            "candidate `{candidate}` changed after it was linked; preserve the file and re-review"
        );
    }
    Ok(obligation)
}

/// Candidate front matter. The `obligation` field must match the disposition
/// target; a missing or mismatched file fails instead of linking an arbitrary id.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct CandidateFront {
    // Clone supports retry comparison: the saved `created` timestamp is
    // substituted into the expected rendering so identical bytes reuse the
    // file while any other difference stays a conflict.
    id: String,
    obligation: String,
    thread: String,
    report_hash: String,
    source: String,
    title: String,
    created: String,
}

fn parse_candidate_bytes(bytes: &[u8], path: &Path) -> Result<(CandidateFront, String)> {
    let text = std::str::from_utf8(bytes).context("candidate is not UTF-8")?;
    let rest = text.strip_prefix("+++\n").with_context(|| format!("{} must start with a `+++` line", path.display()))?;
    let (front, body) = rest.split_once("\n+++\n").with_context(|| format!("{} front matter has no closing `+++` line", path.display()))?;
    let front: CandidateFront = toml::from_str(front).context("candidate front matter does not parse")?;
    Ok((front, body.to_string()))
}

/// Stable identity fields. `created` is deliberately excluded: a retry of the
/// same bytes must reuse the file instead of failing on a new timestamp.
fn front_stable(front: &CandidateFront) -> (&str, &str, &str, &str, &str, &str) {
    (&front.id, &front.obligation, &front.thread, &front.report_hash, &front.source, &front.title)
}

fn candidate_id_for(obligation: &Obligation, title: &str, body: &str) -> String {
    let digest = crate::thread::sha256_hex(
        format!("{}\n{title}\n{}", obligation.id, crate::thread::sha256_hex(body.as_bytes())).as_bytes(),
    );
    format!(
        "cand-{}-{}",
        &obligation.id[3..].chars().take(12).collect::<String>().replace('-', ""),
        &digest[..8]
    )
}

fn render_candidate(front: &CandidateFront, body: &str, obligation: &Obligation, source: &str) -> Result<String> {
    Ok(format!(
        "+++\n{}+++\n\n{}\n\n## Provenance\n\n- Obligation: `{}`\n- Thread: `{}` report `{}` hash `{}`\n- Source: `{source}` (untrusted candidate; not memory until a user or signed-control decision)\n",
        toml::to_string(front)?,
        body.trim(),
        front.obligation,
        obligation.thread_id,
        obligation.report_path,
        obligation.report_hash,
    ))
}

/// Write the candidate file, or reuse an identical one. The saved `created`
/// timestamp is substituted into the expected rendering, so a retry of the
/// same bytes reuses the file (a crash between the write and the disposition
/// link can be retried) while any other difference stays a conflict.
fn write_candidate_file(dir: &Path, front: &CandidateFront, body: &str, obligation: &Obligation, source: &str) -> Result<PathBuf> {
    let path = dir.join(format!("{}.md", front.id));
    if path.is_file() {
        let existing_bytes = bounded_read(&path, MAX_BODY_BYTES + 4096)?;
        let (existing, _) = parse_candidate_bytes(&existing_bytes, &path)?;
        ensure!(
            front_stable(&existing) == front_stable(front),
            "candidate `{}` already exists with different content",
            front.id
        );
        let mut expected_front = front.clone();
        expected_front.created = existing.created.clone();
        let expected = render_candidate(&expected_front, body, obligation, source)?;
        ensure!(
            existing_bytes == expected.as_bytes(),
            "candidate `{}` already exists with different content",
            front.id
        );
        return Ok(path);
    }
    let text = render_candidate(front, body, obligation, source)?;
    crate::project::write_atomic(&path, text.as_bytes())?;
    Ok(path)
}

fn read_candidate_body(body_file: &Path) -> Result<String> {
    let body = bounded_read(body_file, MAX_BODY_BYTES)
        .with_context(|| format!("cannot read {}", body_file.display()))?;
    let body = std::str::from_utf8(&body).context("candidate body is not UTF-8")?.trim().to_string();
    ensure!(!body.is_empty(), "candidate body is empty");
    ensure!(body.len() as u64 <= MAX_BODY_BYTES, "candidate body exceeds bounds");
    Ok(body)
}

fn check_title(title: &str) -> Result<String> {
    let title = title.trim().to_string();
    ensure!(!title.is_empty() && title.chars().count() <= MAX_TITLE_CHARS, "title must contain 1–{MAX_TITLE_CHARS} characters");
    ensure!(!title.chars().any(|c| c.is_control()), "title must not contain control characters");
    Ok(title)
}

fn check_source(source: &str) -> Result<()> {
    ensure!(matches!(source, "user" | "coordinator" | "worker"), "source must be user, coordinator, or worker");
    Ok(())
}

/// Verify a saved candidate belongs to the obligation and return its digest.
/// Thread and report hash must match; the digest pins the exact bytes so a
/// post-link edit is detected instead of silently changing the evidence.
fn verify_candidate_file(path: &Path, candidate_id: &str, obligation: &Obligation) -> Result<String> {
    ensure!(path.is_file(), "candidate `{candidate_id}` is missing; save it first");
    let bytes = bounded_read(path, MAX_BODY_BYTES + 4096)?;
    let (front, _) = parse_candidate_bytes(&bytes, path)?;
    ensure!(
        front.id == candidate_id
            && front.obligation == obligation.id
            && front.thread == obligation.thread_id
            && front.report_hash == obligation.report_hash,
        "candidate `{candidate_id}` does not belong to obligation `{}`",
        obligation.id
    );
    Ok(crate::thread::sha256_hex(&bytes))
}

/// Create a Markdown-compatible candidate file for an obligation and return its id.
/// `source` is one of `user`, `coordinator`, `worker`; worker/coordinator text
/// stays a candidate until a user or signed-control decision promotes it.
/// Retrying the same bytes reuses the file; link it with `dispose_proposed`.
pub fn create_candidate(
    project_dir: &Path,
    obligation_id: &str,
    title: &str,
    body_file: &Path,
    source: &str,
) -> Result<String> {
    validate_obligation_id(obligation_id)?;
    check_source(source)?;
    let title = check_title(title)?;
    let obligation = get(project_dir, obligation_id)?;
    ensure!(matches!(obligation.status, Status::Pending | Status::Deferred), "obligation `{obligation_id}` is already {}", obligation.status);
    let body = read_candidate_body(body_file)?;
    let dir = candidate_dir(project_dir)?;
    std::fs::create_dir_all(&dir)?;
    let id = candidate_id_for(&obligation, &title, &body);
    validate_candidate_id(&id)?;
    let front = CandidateFront {
        id: id.clone(),
        obligation: obligation_id.to_string(),
        thread: obligation.thread_id.clone(),
        report_hash: obligation.report_hash.clone(),
        source: source.to_string(),
        title,
        created: crate::project::now(),
    };
    write_candidate_file(&dir, &front, &body, &obligation, source)?;
    Ok(id)
}

/// Save a candidate file and link it as the proposed disposition in one locked
/// section, so a crash cannot leave the file saved without the link (and a
/// retry of the same bytes still reuses the file).
pub fn propose_with_body(
    project_dir: &Path,
    obligation_id: &str,
    title: &str,
    body_file: &Path,
    source: &str,
) -> Result<Obligation> {
    validate_obligation_id(obligation_id)?;
    let _lock = lock_state(project_dir)?;
    let id = create_candidate(project_dir, obligation_id, title, body_file, source)?;
    let mut state = load_locked(project_dir)?;
    let position = state.obligations.iter().position(|o| o.id == obligation_id).with_context(|| format!("no memory-review obligation `{obligation_id}`"))?;
    let obligation = state.obligations[position].clone();
    ensure!(matches!(obligation.status, Status::Pending | Status::Deferred), "obligation `{obligation_id}` is already {}", obligation.status);
    let dir = candidate_dir(project_dir)?;
    let path = dir.join(format!("{id}.md"));
    let digest = verify_candidate_file(&path, &id, &obligation)?;
    state.obligations[position].status = Status::Proposed;
    state.obligations[position].candidate = Some(id);
    state.obligations[position].candidate_digest = Some(digest);
    state.obligations[position].reason = None;
    state.obligations[position].updated = crate::project::now();
    let out = state.obligations[position].clone();
    save_locked(project_dir, &state)?;
    Ok(out)
}

/// Link an obligation to an already saved candidate file. The file must exist
/// in the mode-appropriate directory with matching obligation, thread, and
/// report hash; its digest is pinned on the obligation.
pub fn dispose_proposed(project_dir: &Path, obligation_id: &str, candidate_id: &str) -> Result<Obligation> {
    validate_obligation_id(obligation_id)?;
    validate_candidate_id(candidate_id)?;
    let dir = candidate_dir(project_dir)?;

    let _lock = lock_state(project_dir)?;
    let mut state = load_locked(project_dir)?;
    let position = state.obligations.iter().position(|o| o.id == obligation_id).with_context(|| format!("no memory-review obligation `{obligation_id}`"))?;
    let obligation = state.obligations[position].clone();
    let path = dir.join(format!("{candidate_id}.md"));
    let digest = verify_candidate_file(&path, candidate_id, &obligation)?;
    if obligation.status == Status::Proposed && obligation.candidate.as_deref() == Some(candidate_id) {
        // Idempotent relink: still fail closed when the file changed after
        // the first link instead of silently accepting new bytes.
        ensure!(
            obligation.candidate_digest.as_deref() == Some(digest.as_str()),
            "candidate `{candidate_id}` changed after it was linked; preserve the file and re-review"
        );
        return Ok(obligation);
    }
    ensure!(matches!(obligation.status, Status::Pending | Status::Deferred), "obligation `{obligation_id}` is already {}", obligation.status);
    state.obligations[position].status = Status::Proposed;
    state.obligations[position].candidate = Some(candidate_id.to_string());
    state.obligations[position].candidate_digest = Some(digest);
    state.obligations[position].reason = None;
    state.obligations[position].updated = crate::project::now();
    let out = state.obligations[position].clone();
    save_locked(project_dir, &state)?;
    Ok(out)
}

pub fn dispose_rejected(project_dir: &Path, obligation_id: &str, reason: &str) -> Result<Obligation> {
    validate_obligation_id(obligation_id)?;
    let reason = sanitize_reason(reason)?;
    let _lock = lock_state(project_dir)?;
    let mut state = load_locked(project_dir)?;
    let position = state.obligations.iter().position(|o| o.id == obligation_id).with_context(|| format!("no memory-review obligation `{obligation_id}`"))?;
    let current = &state.obligations[position];
    if current.status == Status::Rejected && current.reason.as_deref() == Some(reason.as_str()) {
        return Ok(current.clone());
    }
    ensure!(matches!(current.status, Status::Pending | Status::Deferred), "obligation `{obligation_id}` is already {}", current.status);
    state.obligations[position].status = Status::Rejected;
    state.obligations[position].reason = Some(reason);
    state.obligations[position].candidate = None;
    state.obligations[position].candidate_digest = None;
    state.obligations[position].updated = crate::project::now();
    let out = state.obligations[position].clone();
    save_locked(project_dir, &state)?;
    Ok(out)
}

pub fn dispose_deferred(project_dir: &Path, obligation_id: &str, reason: &str) -> Result<Obligation> {
    validate_obligation_id(obligation_id)?;
    let reason = sanitize_reason(reason)?;
    let _lock = lock_state(project_dir)?;
    let mut state = load_locked(project_dir)?;
    let position = state.obligations.iter().position(|o| o.id == obligation_id).with_context(|| format!("no memory-review obligation `{obligation_id}`"))?;
    let current = &state.obligations[position];
    ensure!(matches!(current.status, Status::Pending | Status::Deferred), "obligation `{obligation_id}` is already {}", current.status);
    // Re-deferring with the same reason is a no-op, not a new reminder.
    if current.status == Status::Deferred && current.reason.as_deref() == Some(reason.as_str()) {
        return Ok(current.clone());
    }
    state.obligations[position].status = Status::Deferred;
    state.obligations[position].reason = Some(reason);
    state.obligations[position].updated = crate::project::now();
    let out = state.obligations[position].clone();
    save_locked(project_dir, &state)?;
    Ok(out)
}

/// Record an explicit user decision directly into legacy Markdown memory with
/// provenance to that user instruction. Refused on SQLite-memory projects:
/// projections are not authority and signed control still applies.
pub fn record_user_memory(project_dir: &Path, title: &str, body_file: &Path, provenance: &str) -> Result<PathBuf> {
    ensure!(memory_owner(project_dir)? == MemoryOwner::Legacy, "project memory is SQLite-owned; do not edit Markdown projections. Use `memory PROJECT import --file` with owner-signed `memory-import-review@herdr-projects` review instead");
    let title = title.trim();
    ensure!(!title.is_empty() && title.chars().count() <= MAX_TITLE_CHARS, "title must contain 1–{MAX_TITLE_CHARS} characters");
    ensure!(!title.chars().any(|c| c.is_control()), "title must not contain control characters");
    let provenance = provenance.trim();
    ensure!(!provenance.is_empty() && provenance.chars().count() <= 500, "provenance to the user instruction is required (at most 500 characters)");
    ensure!(!provenance.chars().any(|c| c.is_control()), "provenance must not contain control characters");
    let body = bounded_read(body_file, MAX_BODY_BYTES)?;
    let body = std::str::from_utf8(&body).context("memory body is not UTF-8")?.trim().to_string();
    ensure!(!body.is_empty(), "memory body is empty");

    let slug = crate::project::slugify(title);
    ensure!(!slug.is_empty(), "title has no letters or digits for a file name");
    let name = format!("{slug}.md");
    ensure!(!name.starts_with('.') && name.len() <= 128, "invalid memory file name");
    let dir = project_dir.join("memory");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(&name);
    ensure!(!path.exists(), "memory/{name} already exists; edit it explicitly instead of overwriting");
    let now = crate::project::now();
    let text = format!("<!-- herdr-projects user memory; source=user; created={now}; provenance={provenance} -->\n\n# {title}\n\n{}\n", body.trim());
    crate::project::write_atomic(&path, text.as_bytes())?;

    // Keep MEMORY.md as an index with one line per file.
    let index = project_dir.join("MEMORY.md");
    let current = std::fs::read_to_string(&index).unwrap_or_else(|_| "# Memory\n\n".to_string());
    let line = format!("- [{title}](memory/{name}): user decision {now}");
    if !current.contains(&format!("memory/{name}")) {
        let mut updated = current.trim_end().to_string();
        updated.push('\n');
        updated.push_str(&line);
        updated.push('\n');
        crate::project::write_atomic(&index, updated.as_bytes())?;
    }
    Ok(path)
}

fn seconds_since(timestamp: &str, now: jiff::Timestamp) -> Option<i64> {
    timestamp.parse::<jiff::Timestamp>().ok().map(|then| now.as_second() - then.as_second())
}

/// Obligations due for a bounded, deduplicated nudge. Proposed and rejected
/// never remind. Pending notifies once; deferred reminds at most
/// `MAX_NOTIFICATIONS` total with a one-day cooldown.
pub fn due_notifications(project_dir: &Path, now: jiff::Timestamp, max: usize) -> Vec<Obligation> {
    let mut out: Vec<Obligation> = load(project_dir)
        .unwrap_or_default()
        .into_iter()
        .filter(|o| match o.status {
            Status::Pending => o.notified == 0,
            Status::Deferred => {
                o.notified < MAX_NOTIFICATIONS
                    && o.last_notified.as_ref().and_then(|t| seconds_since(t, now)).is_none_or(|age| age >= REMINDER_COOLDOWN_SECS)
            }
            Status::Proposed | Status::Rejected => false,
        })
        .collect();
    out.sort_by(|a, b| a.created.cmp(&b.created).then(a.id.cmp(&b.id)));
    out.truncate(max);
    out
}

/// Stable inbox id for the next notification of an obligation. Retries before
/// `mark_notified` reuse the same id and content, so `write_once` dedupes.
pub fn notification_id(obligation: &Obligation) -> String {
    if obligation.notified == 0 {
        format!("memory-review-{}", obligation.id)
    } else {
        format!("memory-review-{}-r{}", obligation.id, obligation.notified + 1)
    }
}

pub fn notification_summary(project_slug: &str, obligation: &Obligation) -> String {
    let base = format!(
        "{} has an unresolved memory review ({}, {}): run `memory-review {} show {}`; report {}",
        obligation.thread_id,
        obligation.id,
        obligation.status,
        project_slug,
        obligation.id,
        obligation.report_path
    );
    base.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

/// Emit due legacy inbox reminders (at most 5 per call). Stable ids make
/// ticker retries idempotent via `write_once`; archiving the item never
/// clears the obligation itself.
pub fn emit_due_legacy(project: &crate::project::Project) -> Result<usize> {
    let dir = project.dir();
    let now = jiff::Timestamp::now();
    let due = due_notifications(&dir, now, 5);
    let mut emitted = 0;
    for obligation in due {
        let item = notification_id(&obligation);
        let summary = notification_summary(&project.slug, &obligation);
        crate::inbox::write_once(project, &item, "memory-review", &obligation.thread_id, &summary, "")?;
        mark_notified(&dir, &obligation.id, &now.to_string())?;
        emitted += 1;
    }
    Ok(emitted)
}

/// Deliver due reminders to the canonical SQLite inbox (at most 5 per call)
/// and return their stable ids and summaries. `notified` advances only after
/// a committed delivery row, so a crash between the insert commit and the
/// counter update retries the same stable id and the store reports
/// `AlreadyDelivered`: exactly one advancement per row, never a duplicate,
/// never a skipped count. Failed or rejected delivery advances nothing, so
/// the cap is never consumed without delivery. Per-obligation isolated: every
/// due obligation is attempted and failures are all reported.
#[cfg(feature = "state-store")]
pub fn deliver_migrated(project_dir: &Path, project_slug: &str) -> Result<Vec<(String, String)>> {
    let now = jiff::Timestamp::now();
    let due = due_notifications(project_dir, now, 5);
    let mut out = Vec::new();
    let mut failures = Vec::new();
    for obligation in due {
        let item = notification_id(&obligation);
        let summary = notification_summary(project_slug, &obligation);
        let delivered = deliver_one_migrated(project_dir, &item, &obligation.thread_id, &summary)
            .and_then(|()| mark_notified(project_dir, &obligation.id, &now.to_string()).map(|_| ()));
        match delivered {
            Ok(()) => out.push((item, summary)),
            Err(error) => failures.push(format!("{}: {error:#}", obligation.id)),
        }
    }
    ensure!(failures.is_empty(), "migrated reminders failed: {}", failures.join("; "));
    Ok(out)
}

/// Insert one reminder row under project ownership with a bounded
/// head-conflict retry. The store resolves an already-committed row
/// (`AlreadyDelivered`) or divergent bytes (an error, no retry) before its
/// head check, and reads only the head and that row, never a whole snapshot.
#[cfg(feature = "state-store")]
fn deliver_one_migrated(project_dir: &Path, item: &str, thread_id: &str, summary: &str) -> Result<()> {
    use herdr_projects::domain::InboxContent;
    let content = InboxContent {
        id: item.into(),
        kind: "memory-review".into(),
        subject: thread_id.into(),
        created: String::new(),
        summary: summary.into(),
        body: String::new(),
    };
    let _ownership = herdr_projects::execution_guard::ProjectGuard::acquire(project_dir)?;
    for _ in 0..3 {
        let mut db = herdr_projects::migration::open_active_unchecked(project_dir)?;
        let head = db.current_head()?;
        let now_ms = jiff::Timestamp::now().as_millisecond();
        match db.deliver_memory_review_reminder(head, &content, now_ms) {
            // Both outcomes mean a committed row with this stable id.
            Ok(_) => return Ok(()),
            Err(herdr_projects::store::StoreError::Conflict) => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(anyhow::anyhow!("store head moved on every attempt; retry"))
}

pub fn mark_notified(project_dir: &Path, obligation_id: &str, now: &str) -> Result<Obligation> {
    validate_obligation_id(obligation_id)?;
    let _lock = lock_state(project_dir)?;
    let mut state = load_locked(project_dir)?;
    let position = state.obligations.iter().position(|o| o.id == obligation_id).with_context(|| format!("no memory-review obligation `{obligation_id}`"))?;
    state.obligations[position].notified = state.obligations[position].notified.saturating_add(1).min(MAX_NOTIFICATIONS);
    state.obligations[position].last_notified = Some(now.to_string());
    state.obligations[position].updated = now.to_string();
    let out = state.obligations[position].clone();
    save_locked(project_dir, &state)?;
    Ok(out)
}

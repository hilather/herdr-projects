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
    check_source(source)?;
    let title = check_title(title)?;
    let body = read_candidate_body(body_file)?;
    let dir = candidate_dir(project_dir)?;
    std::fs::create_dir_all(&dir)?;

    let _lock = lock_state(project_dir)?;
    let mut state = load_locked(project_dir)?;
    let position = state.obligations.iter().position(|o| o.id == obligation_id).with_context(|| format!("no memory-review obligation `{obligation_id}`"))?;
    let obligation = state.obligations[position].clone();
    ensure!(matches!(obligation.status, Status::Pending | Status::Deferred), "obligation `{obligation_id}` is already {}", obligation.status);
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
    let path = write_candidate_file(&dir, &front, &body, &obligation, source)?;
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

/// Record due reminders on a migrated project and return their stable ids and
/// summaries for the caller to display. The DB inbox is separate from legacy
/// files, so delivery here is the durable `notified` advancement plus the
/// returned record; obligations stay visible in context either way. Same cap
/// and cooldown as legacy, so repeats do not re-emit the same set.
pub fn remind_migrated(project_dir: &Path, project_slug: &str) -> Result<Vec<(String, String)>> {
    let now = jiff::Timestamp::now();
    let due = due_notifications(project_dir, now, 5);
    let mut out = Vec::new();
    for obligation in due {
        let item = notification_id(&obligation);
        let summary = notification_summary(project_slug, &obligation);
        mark_notified(project_dir, &obligation.id, &now.to_string())?;
        out.push((item, summary));
    }
    Ok(out)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let project = crate::project::create(root.path(), "demo", "", vec![]).unwrap();
        (root, project.dir())
    }

    const REPORT: &str = "## Report\n\ndid work\n\n## Remember\n\nAlways run the linter.\n\n## Next\n\nmore\n";

    #[test]
    fn remember_extraction_ignores_missing_empty_and_stops_at_next_heading() {
        assert_eq!(extract_remember(REPORT).as_deref(), Some("Always run the linter."));
        assert!(extract_remember("## Report\n\nno lesson\n").is_none());
        assert!(extract_remember("## Remember\n\n   \n").is_none());
        assert_eq!(extract_remember("## Remember\n\none\n## Remember\n\ntwo\n").as_deref(), Some("one"));
    }

    #[test]
    fn ingest_dedupes_retries_keeps_revisions_and_never_touches_authority() {
        let (_root, dir) = fixture();
        let memory_before = std::fs::read_to_string(dir.join("MEMORY.md")).unwrap();
        let none = ingest_report(&dir, "t-0001", &"a".repeat(64), "## Report\n\nnothing\n", None).unwrap();
        assert!(none.is_none());
        assert!(!dir.join(STATE_FILE).exists());
        let first = ingest_report(&dir, "t-0001", &"a".repeat(64), REPORT, None).unwrap().unwrap();
        let retry = ingest_report(&dir, "t-0001", &"a".repeat(64), REPORT, Some("review-x")).unwrap().unwrap();
        assert_eq!(first.id, retry.id);
        assert_eq!(load(&dir).unwrap().len(), 1);
        let revised = ingest_report(&dir, "t-0001", &"b".repeat(64), REPORT, None).unwrap().unwrap();
        assert_ne!(first.id, revised.id);
        // Revised reports retain the prior disposition instead of clearing it.
        assert_eq!(load(&dir).unwrap().len(), 2);
        assert!(load(&dir).unwrap().iter().all(|o| o.status == Status::Pending));
        assert_eq!(std::fs::read_to_string(dir.join("MEMORY.md")).unwrap(), memory_before);
        assert!(!dir.join("memory/api.md").exists());
        assert!(!dir.join(".state/format.json").exists());
    }

    #[test]
    fn incident_regression_archive_keeps_obligation_visible_after_reload() {
        let root = tempfile::tempdir().unwrap();
        let project = crate::project::create(root.path(), "demo", "", vec![]).unwrap();
        let dir = project.dir();
        std::fs::write(dir.join("threads/t-0001.md"), REPORT).unwrap();
        let obligation = ingest_thread(&dir, "t-0001").unwrap().unwrap();
        crate::inbox::write(&project, "thread-state", "t-0001", "t-0001 has a new report", "").unwrap();
        emit_due_legacy(&project).unwrap();
        // Archive both the report notice and the memory reminder.
        assert_eq!(crate::inbox::done(&project, &[], true).unwrap(), 2);
        assert!(crate::inbox::unhandled(&project).is_empty());
        // Reload from disk as a restarted coordinator/ticker would.
        let pending = pending_for_context(&dir).unwrap();
        assert_eq!(pending.total, 1);
        assert_eq!(pending.shown.len(), 1);
        assert_eq!(pending.shown[0].id, obligation.id);
        // A ticker retry of the same report hash does not stack another.
        let again = ingest_thread(&dir, "t-0001").unwrap().unwrap();
        assert_eq!(again.id, obligation.id);
        assert_eq!(load(&dir).unwrap().len(), 1);
    }

    #[test]
    fn disposition_persists_and_missing_candidate_fails() {
        let (_root, dir) = fixture();
        let o = ingest_report(&dir, "t-0002", &"c".repeat(64), REPORT, None).unwrap().unwrap();
        assert!(dispose_proposed(&dir, &o.id, "cand-missing").is_err());
        assert_eq!(get(&dir, &o.id).unwrap().status, Status::Pending);
        let body = dir.join("cand-body.md");
        std::fs::write(&body, "Run the linter before review.").unwrap();
        let saved = create_candidate(&dir, &o.id, "Linter", &body, "worker").unwrap();
        let proposed = dispose_proposed(&dir, &o.id, &saved).unwrap();
        assert_eq!(proposed.status, Status::Proposed);
        // Reload survives restarts; re-linking the same candidate is idempotent.
        assert_eq!(get(&dir, &o.id).unwrap().status, Status::Proposed);
        assert!(dispose_proposed(&dir, &o.id, &saved).is_ok());
        // A candidate for another obligation cannot be linked.
        let other = ingest_report(&dir, "t-0003", &"d".repeat(64), REPORT, None).unwrap().unwrap();
        assert!(dispose_proposed(&dir, &other.id, &saved).is_err());
        assert!(dispose_rejected(&dir, &o.id, "late").is_err());
        let rejected = dispose_rejected(&dir, &other.id, "transient status").unwrap();
        assert_eq!(rejected.status, Status::Rejected);
        assert_eq!(get(&dir, &other.id).unwrap().reason.as_deref(), Some("transient status"));
    }

    #[test]
    fn deferred_reminders_are_bounded_and_resolved_never_remind() {
        let (_root, dir) = fixture();
        let deferred = ingest_report(&dir, "t-0004", &"e".repeat(64), REPORT, None).unwrap().unwrap();
        let now = jiff::Timestamp::now();
        assert_eq!(due_notifications(&dir, now, 5).len(), 1);
        mark_notified(&dir, &deferred.id, &now.to_string()).unwrap();
        // Pending notifies once; an immediate second tick emits nothing.
        assert!(due_notifications(&dir, now, 5).is_empty());
        dispose_deferred(&dir, &deferred.id, "ask owner next week").unwrap();
        // Deferred waits a day before the next reminder.
        assert!(due_notifications(&dir, now, 5).is_empty());
        let later = now.as_second() + REMINDER_COOLDOWN_SECS + 1;
        let later = jiff::Timestamp::from_second(later).unwrap();
        assert_eq!(due_notifications(&dir, later, 5).len(), 1);
        mark_notified(&dir, &deferred.id, &later.to_string()).unwrap();
        mark_notified(&dir, &deferred.id, &later.to_string()).unwrap();
        // Cap reached: no further reminders even after another cooldown.
        let far = jiff::Timestamp::from_second(later.as_second() + REMINDER_COOLDOWN_SECS + 1).unwrap();
        assert!(due_notifications(&dir, far, 5).is_empty());
        assert_eq!(get(&dir, &deferred.id).unwrap().notified, MAX_NOTIFICATIONS);
        // Proposed and rejected never remind.
        let p = ingest_report(&dir, "t-0005", &"f".repeat(64), REPORT, None).unwrap().unwrap();
        let body = dir.join("b.md");
        std::fs::write(&body, "x").unwrap();
        let cand = create_candidate(&dir, &p.id, "T", &body, "coordinator").unwrap();
        dispose_proposed(&dir, &p.id, &cand).unwrap();
        let r = ingest_report(&dir, "t-0006", &"1".repeat(64), REPORT, None).unwrap().unwrap();
        dispose_rejected(&dir, &r.id, "duplicate").unwrap();
        assert!(due_notifications(&dir, far, 10).iter().all(|o| o.id != p.id && o.id != r.id));
        // The deferred obligation itself stays visible in context.
        let view = pending_for_context(&dir).unwrap();
        assert!(view.shown.iter().any(|o| o.id == deferred.id));
        assert!(!view.shown.iter().any(|o| o.id == p.id || o.id == r.id));
    }

    #[test]
    fn legacy_and_sqlite_candidate_routing_and_record_boundaries() {
        let (_root, dir) = fixture();
        let o = ingest_report(&dir, "t-0007", &"2".repeat(64), REPORT, None).unwrap().unwrap();
        let body = dir.join("body.md");
        std::fs::write(&body, "candidate").unwrap();
        let cand = create_candidate(&dir, &o.id, "Title", &body, "worker").unwrap();
        assert!(dir.join("memory/candidates").join(format!("{cand}.md")).is_file());
        let provenance = dir.join("decision.md");
        std::fs::write(&provenance, "Use Postgres for billing.").unwrap();
        let recorded = record_user_memory(&dir, "Use Postgres", &provenance, "user chat: remember our DB choice").unwrap();
        assert!(recorded.starts_with(dir.join("memory")));
        assert!(std::fs::read_to_string(dir.join("MEMORY.md")).unwrap().contains("memory/use-postgres.md"));

        // SQLite-memory projects route candidates away from projections and
        // refuse direct Markdown recording (signed control still applies).
        std::fs::create_dir_all(dir.join(".state")).unwrap();
        std::fs::write(dir.join(".state/format.json"), r#"{"version":1,"runtime":"sqlite-v2","memory":"sqlite-v1","migration":"abc","reconciliation_required":true}"#).unwrap();
        assert_eq!(memory_owner(&dir).unwrap(), MemoryOwner::Sqlite);
        let s = ingest_report(&dir, "t-0008", &"3".repeat(64), REPORT, None).unwrap().unwrap();
        let cand2 = create_candidate(&dir, &s.id, "Title", &body, "coordinator").unwrap();
        assert!(dir.join(".state/memory-review-candidates").join(format!("{cand2}.md")).is_file());
        assert!(record_user_memory(&dir, "X", &body, "user chat: x").is_err());
        // Half-migrated journals fail closed with recovery guidance.
        std::fs::remove_file(dir.join(".state/format.json")).unwrap();
        std::fs::create_dir_all(dir.join(".state/migration")).unwrap();
        std::fs::write(dir.join(".state/migration/journal.json"), "{}").unwrap();
        assert!(memory_owner(&dir).unwrap_err().to_string().contains("migration recover"));
    }

    #[test]
    fn invalid_and_untrusted_inputs_fail_closed() {
        let (_root, dir) = fixture();
        assert!(ingest_report(&dir, "../x", &"a".repeat(64), REPORT, None).is_err());
        assert!(ingest_report(&dir, "t-0009", "", REPORT, None).is_err());
        let o = ingest_report(&dir, "t-0009", &"4".repeat(64), REPORT, None).unwrap().unwrap();
        assert!(dispose_rejected(&dir, &o.id, "   ").is_err());
        assert!(dispose_deferred(&dir, &o.id, &"x".repeat(MAX_REASON_CHARS + 1)).is_err());
        assert!(dispose_proposed(&dir, &o.id, "../escape").is_err());
        assert!(dispose_proposed(&dir, "../escape", "cand-x").is_err());
        let body = dir.join("evil.md");
        std::fs::write(&body, "x").unwrap();
        assert!(create_candidate(&dir, &o.id, "T", &body, "admin").is_err());
        assert!(create_candidate(&dir, &o.id, "", &body, "worker").is_err());
        assert!(record_user_memory(&dir, "T", &body, "").is_err());
        // Corrupt state is preserved, never auto-repaired or overwritten.
        std::fs::write(dir.join(STATE_FILE), "{broken").unwrap();
        assert!(load(&dir).is_err());
        assert!(ingest_report(&dir, "t-0010", &"5".repeat(64), REPORT, None).is_err());
        assert_eq!(std::fs::read(dir.join(STATE_FILE)).unwrap(), b"{broken");
    }

    #[test]
    fn candidate_retry_reuses_bytes_and_links() {
        let (_root, dir) = fixture();
        let o = ingest_report(&dir, "t-0011", &"6".repeat(64), REPORT, None).unwrap().unwrap();
        let body = dir.join("retry.md");
        std::fs::write(&body, "Same bytes.").unwrap();
        let first = create_candidate(&dir, &o.id, "Retry", &body, "worker").unwrap();
        // A retry of the same bytes (new `created` timestamp) reuses the file
        // instead of failing, so a crash before the link can be recovered.
        std::thread::sleep(std::time::Duration::from_millis(5));
        let second = create_candidate(&dir, &o.id, "Retry", &body, "worker").unwrap();
        assert_eq!(first, second);
        dispose_proposed(&dir, &o.id, &second).unwrap();
        assert_eq!(get(&dir, &o.id).unwrap().status, Status::Proposed);
        // An unknown obligation fails before any file write.
        std::fs::write(&body, "Different bytes.").unwrap();
        assert!(create_candidate(&dir, "mr-t-0012-000000000000", "Other", &body, "worker").is_err());
    }

    #[test]
    fn atomic_propose_links_file_and_disposition_together() {
        let (_root, dir) = fixture();
        let o = ingest_report(&dir, "t-0013", &"7".repeat(64), REPORT, None).unwrap().unwrap();
        let body = dir.join("atomic.md");
        std::fs::write(&body, "Atomic summary.").unwrap();
        let linked = propose_with_body(&dir, &o.id, "Atomic", &body, "coordinator").unwrap();
        assert_eq!(linked.status, Status::Proposed);
        let candidate = linked.candidate.clone().unwrap();
        assert!(linked.candidate_digest.is_some());
        assert!(dir.join("memory/candidates").join(format!("{candidate}.md")).is_file());
        assert_eq!(get(&dir, &o.id).unwrap().candidate_digest, linked.candidate_digest);
    }

    #[test]
    fn linked_candidate_is_pinned_and_tamper_fails_closed() {
        let (_root, dir) = fixture();
        let o = ingest_report(&dir, "t-0014", &"8".repeat(64), REPORT, None).unwrap().unwrap();
        let body = dir.join("pin.md");
        std::fs::write(&body, "Pinned.").unwrap();
        let saved = create_candidate(&dir, &o.id, "Pin", &body, "worker").unwrap();
        dispose_proposed(&dir, &o.id, &saved).unwrap();
        let path = dir.join("memory/candidates").join(format!("{saved}.md"));
        // Post-link edits are detected on read and on relink.
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("\nForged addition.\n");
        std::fs::write(&path, text).unwrap();
        assert!(get(&dir, &o.id).is_err());
        assert!(dispose_proposed(&dir, &o.id, &saved).is_err());
        // A hand-made candidate naming the obligation but the wrong thread or
        // report hash cannot be linked.
        let forged = format!(
            "+++\nid = \"cand-forged\"\nobligation = \"{}\"\nthread = \"t-9999\"\nreport_hash = \"{}\"\nsource = \"worker\"\ntitle = \"Forged\"\ncreated = \"now\"\n+++\n\nx\n",
            o.id, o.report_hash
        );
        std::fs::write(dir.join("memory/candidates").join("cand-forged.md"), forged).unwrap();
        assert!(dispose_proposed(&dir, &o.id, "cand-forged").is_err());
    }

    #[test]
    fn ingest_pins_the_file_digest_and_evidence_is_verified() {
        let root = tempfile::tempdir().unwrap();
        let project = crate::project::create(root.path(), "demo", "", vec![]).unwrap();
        let dir = project.dir();
        std::fs::write(dir.join("threads/t-0015.md"), REPORT).unwrap();
        // A stale record hash (the tick writes the home file before updating
        // the record) must not poison ingest: the obligation pins the digest
        // of the bytes actually parsed, never the record's claim.
        std::fs::write(
            dir.join("threads/t-0015.toml"),
            format!("id = \"t-0015\"\nreport_hash = \"{}\"\n", "9".repeat(64)),
        )
        .unwrap();
        let o = ingest_thread(&dir, "t-0015").unwrap().unwrap();
        let file_digest = crate::thread::sha256_hex(std::fs::read(dir.join("threads/t-0015.md")).unwrap().as_slice());
        assert_eq!(o.report_hash, file_digest);
        assert_ne!(o.report_hash, "9".repeat(64));
        assert!(o.source_notice.is_none());
        assert_eq!(read_evidence(&dir, &o.remember_hash).unwrap(), "Always run the linter.");
        // Tampered evidence fails verification.
        let evidence = dir.join(EVIDENCE_DIR).join(format!("{}.txt", o.remember_hash));
        std::fs::write(&evidence, "forged").unwrap();
        assert!(read_evidence(&dir, &o.remember_hash).is_err());
    }

    #[test]
    fn ingest_all_is_per_thread_isolated() {
        let (_root, dir) = fixture();
        // One good report with Remember...
        std::fs::write(dir.join("threads/t-0020.md"), REPORT).unwrap();
        // ...and one unreadable-as-UTF-8 report that must not block it.
        std::fs::write(dir.join("threads/t-0021.md"), [0xff, 0xfe, 0x00]).unwrap();
        let err = ingest_all(&dir).unwrap_err();
        assert!(err.to_string().contains("t-0021"), "{err:#}");
        let obligations = load(&dir).unwrap();
        assert_eq!(obligations.len(), 1);
        assert_eq!(obligations[0].thread_id, "t-0020");
    }

    #[test]
    fn reminder_names_the_project_slug_and_migrated_reminders_are_bounded() {
        let (_root, dir) = fixture();
        let o = ingest_report(&dir, "t-0016", &"a0".repeat(32), REPORT, None).unwrap().unwrap();
        let summary = notification_summary("demo", &o);
        assert!(summary.contains("`memory-review demo show"), "{summary}");
        assert!(!summary.contains("memory-review t-0016 show"), "{summary}");
        // Migrated delivery records durably: the first call emits, an immediate
        // repeat emits nothing (same cap/cooldown as legacy).
        let first = remind_migrated(&dir, "demo").unwrap();
        assert_eq!(first.len(), 1);
        assert!(first[0].0.starts_with("memory-review-"), "{}", first[0].0);
        assert!(first[0].1.contains("memory-review demo show"), "{}", first[0].1);
        assert!(remind_migrated(&dir, "demo").unwrap().is_empty());
        assert_eq!(get(&dir, &o.id).unwrap().notified, 1);
    }

    #[test]
    fn context_view_reports_true_total_truncation_and_load_errors() {
        let (_root, dir) = fixture();
        for n in 0..CONTEXT_PAGE + 1 {
            let thread = format!("t-{:04}", 100 + n);
            let hash = format!("{n:064}");
            ingest_report(&dir, &thread, &hash, REPORT, None).unwrap();
        }
        let view = pending_for_context(&dir).unwrap();
        assert_eq!(view.total, CONTEXT_PAGE + 1);
        assert_eq!(view.shown.len(), CONTEXT_PAGE);
        std::fs::write(dir.join(STATE_FILE), "{broken").unwrap();
        assert!(pending_for_context(&dir).is_err());
    }
}

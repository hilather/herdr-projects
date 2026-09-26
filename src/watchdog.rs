//! Pauses new admission for one project. Does not release attempts or mark one terminated.
use crate::store::StoreError;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const PAUSE_NAME: &str = "admission-paused.json";
const MAX_PAUSE_BYTES: u64 = 256;

pub use crate::store::BUSY_RETRY_BOUND as BUSY_BOUND;

/// True while the handler should keep waiting. `false` means the retry bound has passed.
pub fn busy_handler_should_retry(started: Instant, bound: Duration) -> bool {
    started.elapsed() < bound
}

pub fn pause_path(project: &Path) -> PathBuf {
    project.join(".state").join(PAUSE_NAME)
}

/// `disk_full` or `database_busy` after the handler gives up. Other errors do not pause.
pub fn cause(error: &StoreError) -> Option<&'static str> {
    match error {
        StoreError::DiskFull => Some("disk_full"),
        StoreError::Busy => Some("database_busy"),
        _ => None,
    }
}

/// Allowlisted reason only. A corrupt or non-regular file still pauses, without echoing its bytes.
pub fn pause_reason(project: &Path) -> Option<&'static str> {
    let path = pause_path(project);
    let meta = fs::symlink_metadata(&path).ok()?;
    if !meta.file_type().is_file() || meta.nlink() != 1 || meta.len() > MAX_PAUSE_BYTES {
        return Some("admission_paused");
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)
        .ok()?;
    let mut bytes = Vec::new();
    if file.take(MAX_PAUSE_BYTES + 1).read_to_end(&mut bytes).is_err() || bytes.len() as u64 > MAX_PAUSE_BYTES {
        return Some("admission_paused");
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Some("admission_paused");
    };
    match value.get("reason").and_then(|reason| reason.as_str()) {
        Some("disk_full") => Some("disk_full"),
        Some("database_busy") => Some("database_busy"),
        _ => Some("admission_paused"),
    }
}

pub fn is_paused(project: &Path) -> bool {
    pause_reason(project).is_some()
}

/// Record a pause. The first reason sticks. This writes a side file and does not open attempt rows.
pub fn note(project: &Path, error: &StoreError) -> std::io::Result<bool> {
    let Some(reason) = cause(error) else { return Ok(false) };
    if is_paused(project) {
        return Ok(true);
    }
    write_pause(project, reason)
}

fn write_pause(project: &Path, reason: &'static str) -> std::io::Result<bool> {
    let path = pause_path(project);
    if let Ok(meta) = fs::symlink_metadata(&path) {
        if meta.is_file() {
            return Ok(true);
        }
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "admission pause path is not a regular file",
        ));
    }
    let dir = project.join(".state");
    let tmp = dir.join(format!(".admission-paused.{}.tmp", std::process::id()));
    let body = serde_json::json!({
        "reason": reason,
        "paused_unix_ms": jiff::Timestamp::now().as_millisecond(),
    });
    let bytes = serde_json::to_vec(&body).map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)?;
    let result = (|| -> std::io::Result<()> {
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, &path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map(|()| true)
}

/// One ticker line per admission decision. Reason and task id are allowlisted; environment is not copied.
pub fn admission_log_line(reason: &str, task_id: Option<&str>, duration_ms: u128) -> String {
    let reason = match reason {
        "disk_full" | "database_busy" | "admission_paused" | "admission_off" | "idle" | "reserved"
        | "authority_missing" | "verification_backlog" | "integration_backlog" | "capacity_full" | "error" => reason,
        _ => "error",
    };
    let task_id = task_id.filter(|id| {
        !id.is_empty()
            && id.len() <= 128
            && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    });
    serde_json::json!({
        "reason": reason,
        "task_id": task_id,
        "duration_ms": u64::try_from(duration_ms).unwrap_or(u64::MAX),
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::*;
    use crate::store::{SqliteStore, StoreError, BUSY_RETRY_BOUND};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static BUSY_CALLS: AtomicUsize = AtomicUsize::new(0);

    fn give_up_past_bound(_retries: i32) -> bool {
        BUSY_CALLS.fetch_add(1, Ordering::SeqCst);
        // A zero bound has already passed, so the handler must not keep the lock.
        busy_handler_should_retry(Instant::now(), Duration::ZERO)
    }

    fn project_with_attempt() -> (tempfile::TempDir, std::path::PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir_all(project.join(".state")).unwrap();
        let mut db = SqliteStore::create(&project.join(".state/state.db")).unwrap();
        db.commit(Commit {
            expected_head: 0,
            mutations: vec![
                Mutation::Task {
                    expected: None,
                    next: Task {
                        id: TaskId::new("kept").unwrap(),
                        revision: 1,
                        state: TaskState::Running,
                        title: "SECRET_TOKEN_DO_NOT_LEAK".into(),
                        active_attempt: None,
                    },
                },
                Mutation::Attempt {
                    expected: None,
                    next: Attempt {
                        id: AttemptId::new("attempt-kept").unwrap(),
                        task: TaskId::new("kept").unwrap(),
                        revision: 1,
                        state: AttemptState::Running,
                        snapshot: None,
                        reservation: "slot-kept".into(),
                        termination_observed: false,
                    },
                },
            ],
        })
        .unwrap();
        drop(db);
        (temp, project)
    }

    fn attempts(project: &Path) -> Vec<(String, i64)> {
        let connection = rusqlite::Connection::open_with_flags(
            project.join(".state/state.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .unwrap();
        let mut statement = connection
            .prepare("SELECT id, termination_observed FROM attempts ORDER BY id")
            .unwrap();
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(|row| row.unwrap())
            .collect()
    }

    #[test]
    fn busy_bound_matches_the_connection_handler() {
        let source = include_str!("store/mod.rs");
        assert!(source.contains("busy_timeout(BUSY_RETRY_BOUND)"), "{source}");
        assert_eq!(BUSY_RETRY_BOUND, Duration::from_millis(250));
        assert_eq!(BUSY_BOUND, BUSY_RETRY_BOUND);
        let started = Instant::now();
        assert!(busy_handler_should_retry(started, BUSY_RETRY_BOUND));
        assert!(!busy_handler_should_retry(started, Duration::ZERO));
    }

    #[test]
    fn other_store_errors_do_not_pause() {
        let (_temp, project) = project_with_attempt();
        let before = attempts(&project);
        assert!(!note(&project, &StoreError::Conflict).unwrap());
        assert!(!is_paused(&project));
        assert_eq!(attempts(&project), before);
    }

    #[test]
    fn disk_full_fixture_pauses_admission_and_retains_attempts() {
        let (_temp, project) = project_with_attempt();
        let before = attempts(&project);
        assert_eq!(before, vec![("attempt-kept".into(), 0)]);
        let connection = rusqlite::Connection::open(project.join(".state/state.db")).unwrap();
        let pages: i64 = connection.query_row("PRAGMA page_count", [], |row| row.get(0)).unwrap();
        connection.pragma_update(None, "max_page_count", pages).unwrap();
        let error = connection.execute("CREATE TABLE disk_full_probe(x)", []).unwrap_err();
        let store_error = StoreError::from(error);
        assert!(matches!(store_error, StoreError::DiskFull), "{store_error:?}");
        drop(connection);
        assert!(note(&project, &store_error).unwrap());
        assert_eq!(pause_reason(&project), Some("disk_full"));
        assert!(note(&project, &StoreError::Busy).unwrap());
        assert_eq!(pause_reason(&project), Some("disk_full"), "the first pause reason sticks");
        assert_eq!(attempts(&project), before);
        let paused = std::fs::read_to_string(pause_path(&project)).unwrap();
        assert!(!paused.contains("SECRET_TOKEN_DO_NOT_LEAK"), "{paused}");
        #[cfg(target_os = "linux")]
        {
            let block = crate::admission::admit_once(&project).unwrap().expect("paused");
            assert_eq!(block.blocker, "admission_paused");
            assert_eq!(block.reason, "disk_full");
            assert!(!crate::admission::wake_enabled(&project));
            assert_eq!(attempts(&project), before);
        }
    }

    #[test]
    fn busy_handler_fixture_pauses_admission_and_retains_attempts() {
        let (_temp, project) = project_with_attempt();
        let before = attempts(&project);
        let holder = rusqlite::Connection::open(project.join(".state/state.db")).unwrap();
        holder.execute_batch("BEGIN IMMEDIATE").unwrap();
        let blocked = rusqlite::Connection::open(project.join(".state/state.db")).unwrap();
        BUSY_CALLS.store(0, Ordering::SeqCst);
        blocked.busy_handler(Some(give_up_past_bound)).unwrap();
        let error = blocked.execute_batch("BEGIN IMMEDIATE").unwrap_err();
        assert!(BUSY_CALLS.load(Ordering::SeqCst) >= 1, "busy handler was not invoked");
        let store_error = StoreError::from(error);
        assert!(matches!(store_error, StoreError::Busy), "{store_error:?}");
        assert!(note(&project, &store_error).unwrap());
        assert_eq!(pause_reason(&project), Some("database_busy"));
        drop(blocked);
        drop(holder);
        assert_eq!(attempts(&project), before);
        assert!(before.iter().all(|(_, observed)| *observed == 0));
    }

    #[test]
    fn admission_log_line_keeps_reason_and_task_id_only() {
        let line = admission_log_line("disk_full", Some("task-1"), 4);
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["reason"], "disk_full");
        assert_eq!(value["task_id"], "task-1");
        assert_eq!(value["duration_ms"], 4);
        assert!(value.get("environment").is_none());
        assert!(value.get("argv").is_none());
        let dirty = admission_log_line("SECRET_TOKEN_DO_NOT_LEAK=1", Some("not a task"), 1);
        assert!(!dirty.contains("SECRET_TOKEN_DO_NOT_LEAK"), "{dirty}");
        assert!(dirty.contains("\"reason\":\"error\""));
        assert!(dirty.contains("\"task_id\":null"));
    }
}

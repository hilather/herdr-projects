//! Pauses new admission for one project. Does not release attempts or mark one terminated.
use crate::store::StoreError;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const PAUSE_NAME: &str = "admission-paused.json";
const MAX_PAUSE_BYTES: u64 = 256;

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
    // The file was a regular in-bounds pause. NotFound means it disappeared.
    // EACCES, EMFILE, or ELOOP must not resume admission while it is still there.
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => return Some("admission_paused"),
    };
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
        Some(INTEGRITY) => Some(INTEGRITY),
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

const INTEGRITY: &str = "integrity_check_failed";

/// The periodic whole-store check failed. Like other pauses, only an operator
/// removes it, after preserving and restoring the store.
pub fn pause_integrity(project: &Path) -> std::io::Result<bool> {
    if is_paused(project) {
        return Ok(true);
    }
    write_pause(project, INTEGRITY)
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
    admission_log_line_observed(reason,task_id,duration_ms,None)
}

pub fn admission_log_line_observed(reason: &str, task_id: Option<&str>, duration_ms: u128,
    sql: Option<crate::store::controlled::SqlWorkMetrics>) -> String {
    let reason = match reason {
        "disk_full" | "database_busy" | "admission_paused" | "admission_off" | "idle" | "reserved"
        | "authority_missing" | "scan_incomplete" | "verification_backlog" | "integration_backlog" | "capacity_full" | "error" => reason,
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
        "sql_work": sql,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::*;
    use crate::store::{SqliteStore, StoreError};
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;

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
    fn unreadable_pause_file_stays_paused_until_it_is_gone() {
        let (_temp, project) = project_with_attempt();
        note(&project, &StoreError::DiskFull).unwrap();
        let path = pause_path(&project);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if unsafe { libc::geteuid() } != 0 {
            assert_eq!(pause_reason(&project), Some("admission_paused"));
            assert!(is_paused(&project));
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(pause_reason(&project), None);
        assert!(!is_paused(&project));
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
        connection.execute("CREATE TABLE disk_full_probe(x)", []).unwrap();
        let pages: i64 = connection.query_row("PRAGMA page_count", [], |row| row.get(0)).unwrap();
        let page_size: i64 = connection.query_row("PRAGMA page_size", [], |row| row.get(0)).unwrap();
        connection.pragma_update(None, "max_page_count", pages).unwrap();
        // Rebuilding tables during migration can leave reusable pages. Force an
        // allocation larger than the entire database, including its freelist.
        let error = connection
            .execute("INSERT INTO disk_full_probe VALUES (zeroblob(?1))", [(pages + 1) * page_size])
            .unwrap_err();
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
    fn production_store_busy_timeout_pauses_without_writes_or_capacity_release() {
        let (_temp, project) = project_with_attempt();
        let before = attempts(&project);
        let path=project.join(".state/state.db");
        let mut blocked=SqliteStore::open(&path).unwrap();let head=blocked.current_head().unwrap();
        let holder=rusqlite::Connection::open(&path).unwrap();holder.execute_batch("BEGIN IMMEDIATE").unwrap();
        let pending=||Commit{expected_head:head,mutations:vec![Mutation::Task{expected:None,next:Task{
            id:TaskId::new("pending").unwrap(),revision:1,state:TaskState::Draft,title:"pending write".into(),active_attempt:None,
        }}]};
        let started=Instant::now();let error=blocked.commit(pending()).unwrap_err();
        let elapsed=started.elapsed();
        assert!(matches!(error,StoreError::Busy),"{error:?}");
        assert!(elapsed>=std::time::Duration::from_millis(100) && elapsed<std::time::Duration::from_secs(2),"unexpected production busy wait: {elapsed:?}");
        assert!(note(&project,&error).unwrap());assert_eq!(pause_reason(&project),Some("database_busy"));
        assert_eq!(blocked.current_head().unwrap(),head);assert_eq!(attempts(&project),before);
        assert_eq!(holder.query_row("SELECT count(*) FROM tasks WHERE id='pending'",[],|r|r.get::<_,u64>(0)).unwrap(),0);
        holder.execute_batch("ROLLBACK").unwrap();
        assert_eq!(blocked.commit(pending()).unwrap(),head+1);
        assert_eq!(attempts(&project),before);assert!(is_paused(&project));
        assert!(before.iter().all(|(_,observed)|*observed==0));
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
        assert!(value["sql_work"].is_null());
        let metrics=crate::store::controlled::SqlWorkMetrics {connection_observed:true,sqlite_rows_returned:17,sqlite_vm_steps:231};
        let measured=admission_log_line_observed("reserved",Some("task-1"),4,Some(metrics));
        let measured:serde_json::Value=serde_json::from_str(&measured).unwrap();
        assert_eq!(measured["sql_work"]["sqlite_rows_returned"],17);
        assert_eq!(measured["sql_work"]["sqlite_vm_steps"],231);
        let dirty = admission_log_line("SECRET_TOKEN_DO_NOT_LEAK=1", Some("not a task"), 1);
        assert!(!dirty.contains("SECRET_TOKEN_DO_NOT_LEAK"), "{dirty}");
        assert!(dirty.contains("\"reason\":\"error\""));
        assert!(dirty.contains("\"task_id\":null"));
    }
}

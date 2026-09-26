//! Read-only factory status. Counters are bounded and carry no environment, argv, or secrets.
use crate::store::{FactoryNumbers, SqliteStore, StoreError, SCHEMA};
use crate::watchdog;
use rusqlite::OpenFlags;
use serde::Serialize;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

#[derive(Debug, Serialize)]
pub struct QueueAge {
    pub control: Option<i64>,
    /// In-memory transfer queues are not durable. Missing age stays null rather than zero.
    pub transfer: Option<i64>,
    pub verification: Option<i64>,
    pub integration: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct Counters {
    pub queue_age_ms: QueueAge,
    pub lock_wait_ms: Option<u64>,
    pub rows_decoded: u64,
    pub observation_age_ms: Option<i64>,
    pub ambiguous_effects: u64,
    pub retained_slots: u64,
    pub verification_backlog_age_ms: Option<i64>,
    pub integration_backlog_age_ms: Option<i64>,
    pub promotion_conflicts: u64,
    pub coordinator_checkpoint_chars: u64,
}

#[derive(Debug, Serialize)]
pub struct FactoryStatus {
    pub schema: u32,
    pub feature: &'static str,
    pub prepared_dispatch: bool,
    pub factory_admission: String,
    pub sqlite_version: &'static str,
    pub platform: &'static str,
    pub admission_paused: bool,
    pub pause_reason: Option<&'static str>,
    pub blockers: Vec<&'static str>,
    pub counters: Counters,
}

#[derive(Debug)]
pub enum ReportError {
    Store(StoreError),
    Io(String),
    /// Newer than this binary. The value is the only JSON that may be printed.
    Newer(serde_json::Value),
}

impl std::fmt::Display for ReportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => write!(f, "{error}"),
            Self::Io(error) => write!(f, "{error}"),
            Self::Newer(_) => write!(f, "store schema is newer than this binary"),
        }
    }
}
impl std::error::Error for ReportError {}

fn platform() -> &'static str {
    if cfg!(target_os = "linux") { "linux" } else { "unsupported" }
}

fn user_version(path: &Path) -> Result<u32, ReportError> {
    let connection = rusqlite::Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|error| ReportError::Io(error.to_string()))?;
    connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|error| ReportError::Store(StoreError::from(error)))
}

fn blockers(numbers: &FactoryNumbers, paused: bool, promotion_pages: bool) -> Vec<&'static str> {
    let mut blockers = Vec::new();
    if paused {
        blockers.push("admission_paused");
    }
    if numbers.integration_reconciliation_stale {
        blockers.push("integration_reconciliation_stale");
    }
    if numbers.retained_at_cap {
        blockers.push("retained_at_cap");
    }
    if promotion_pages {
        blockers.push("promotion_conflict");
    }
    blockers
}

/// Operator rate file, digits only. Absent means report the count and do not page.
fn promotion_pages(project: &Path, conflicts: u64) -> bool {
    let path = project.join(".state/promotion-conflict-rate");
    let Ok(meta) = std::fs::symlink_metadata(&path) else { return false };
    if !meta.is_file() || meta.nlink() != 1 || meta.len() > 16 {
        return false;
    }
    let Ok(text) = std::fs::read_to_string(&path) else { return false };
    let text = text.trim();
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    text.parse::<u64>().is_ok_and(|rate| conflicts > rate)
}

fn counters_from(numbers: FactoryNumbers) -> Counters {
    Counters {
        queue_age_ms: QueueAge {
            control: numbers.queue_age_control_ms,
            transfer: None,
            verification: numbers.queue_age_verification_ms,
            integration: numbers.queue_age_integration_ms,
        },
        lock_wait_ms: None,
        rows_decoded: numbers.rows_decoded,
        observation_age_ms: numbers.observation_age_ms,
        ambiguous_effects: numbers.ambiguous_effects,
        retained_slots: numbers.retained_slots,
        verification_backlog_age_ms: numbers.verification_backlog_age_ms,
        integration_backlog_age_ms: numbers.integration_backlog_age_ms,
        promotion_conflicts: numbers.promotion_conflicts,
        coordinator_checkpoint_chars: numbers.coordinator_checkpoint_chars,
    }
}

/// Read-only status. Does not launch, admit, or copy environment values.
pub fn report(project: &Path, prepared_dispatch: bool) -> Result<FactoryStatus, ReportError> {
    let db_path = project.join(".state/state.db");
    let version = user_version(&db_path)?;
    if version == 0 || version > SCHEMA {
        return Err(ReportError::Newer(serde_json::json!({
            "schema": version,
            "feature": "state-store",
            "prepared_dispatch": prepared_dispatch,
            "sqlite_version": rusqlite::version(),
            "platform": platform(),
            "error": "unsupported_schema",
        })));
    }
    let mut db = SqliteStore::open(&db_path).map_err(ReportError::Store)?;
    let now = jiff::Timestamp::now().as_millisecond();
    let numbers = db.factory_counters(now).map_err(ReportError::Store)?;
    let pause_reason = watchdog::pause_reason(project);
    let paused = pause_reason.is_some();
    let promotion_pages = promotion_pages(project, numbers.promotion_conflicts);
    let blockers = blockers(&numbers, paused, promotion_pages);
    let schema = numbers.schema;
    let factory_admission = numbers.factory_admission.clone();
    Ok(FactoryStatus {
        schema,
        feature: "state-store",
        prepared_dispatch,
        factory_admission,
        sqlite_version: rusqlite::version(),
        platform: platform(),
        admission_paused: paused,
        pause_reason,
        blockers,
        counters: counters_from(numbers),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::*;
    use crate::store::SqliteStore;

    fn forbid(value: &serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, child) in map {
                    let lower = key.to_ascii_lowercase();
                    assert!(
                        !matches!(lower.as_str(), "env" | "environment" | "argv" | "secret" | "secrets" | "path"),
                        "{key}"
                    );
                    forbid(child);
                }
            }
            serde_json::Value::Array(items) => items.iter().for_each(forbid),
            serde_json::Value::String(text) => {
                assert!(!text.contains("SECRET_TOKEN_DO_NOT_LEAK"), "{text}");
            }
            _ => {}
        }
    }

    #[test]
    fn status_json_omits_environment_and_counts_rows_from_the_active_page() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir_all(project.join(".state")).unwrap();
        let mut db = SqliteStore::create(&project.join(".state/state.db")).unwrap();
        let mut mutations = vec![
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
        ];
        for index in 0..40 {
            mutations.push(Mutation::Task {
                expected: None,
                next: Task {
                    id: TaskId::new(format!("retired-{index:02}")).unwrap(),
                    revision: 1,
                    state: TaskState::Succeeded,
                    title: format!("SECRET_TOKEN_DO_NOT_LEAK-{index}").into(),
                    active_attempt: None,
                },
            });
            mutations.push(Mutation::Attempt {
                expected: None,
                next: Attempt {
                    id: AttemptId::new(format!("attempt-retired-{index:02}")).unwrap(),
                    task: TaskId::new(format!("retired-{index:02}")).unwrap(),
                    revision: 1,
                    state: AttemptState::Completed,
                    snapshot: None,
                    reservation: format!("slot-retired-{index:02}"),
                    termination_observed: true,
                },
            });
        }
        db.commit(Commit { expected_head: 0, mutations }).unwrap();
        drop(db);
        std::fs::write(
            project.join(".state/admission-paused.json"),
            r#"{"reason":"disk_full","token":"SECRET_TOKEN_DO_NOT_LEAK"}"#,
        )
        .unwrap();
        let status = report(&project, true).unwrap();
        assert!(status.prepared_dispatch);
        assert_eq!(status.factory_admission, "off");
        assert!(status.admission_paused);
        assert_eq!(status.pause_reason, Some("disk_full"));
        assert!(status.blockers.contains(&"admission_paused"));
        assert!(!status.blockers.contains(&"promotion_conflict"));
        assert_eq!(status.counters.retained_slots, 1);
        assert!(status.counters.rows_decoded > 0);
        assert!(
            status.counters.rows_decoded < 41,
            "rows_decoded {} decoded retired attempts",
            status.counters.rows_decoded
        );
        let now = jiff::Timestamp::now().as_millisecond();
        let mut db = SqliteStore::open(&project.join(".state/state.db")).unwrap();
        let rows = db.hot_path_rows_decoded(now).unwrap();
        assert_eq!(status.counters.rows_decoded, rows);
        let value = serde_json::to_value(&status).unwrap();
        forbid(&value);
        assert!(value.get("argv").is_none());
        assert!(value.get("environment").is_none());
        let text = serde_json::to_string(&value).unwrap();
        assert!(!text.contains("SECRET_TOKEN_DO_NOT_LEAK"), "{text}");

        let status_source = include_str!("factory_status.rs");
        let snapshot_reader = ["read", "_snapshot"].concat();
        assert!(status_source.contains("hot_path_rows_decoded") || include_str!("store/observability.rs").contains("hot_path_rows_decoded"));
        assert!(!status_source.contains(&snapshot_reader));
        assert!(!include_str!("store/observability.rs").contains(&snapshot_reader));
        let active = include_str!("store/active_work.rs");
        let start = active.find("fn hot_path_page_rows").unwrap();
        let body = &active[start..];
        let body = body.split("\n    pub fn ").next().unwrap();
        assert!(body.contains("drop(tx)"), "{body}");
        assert!(!body.contains(&snapshot_reader), "{body}");
        let watchdog_source = include_str!("watchdog.rs");
        let production = watchdog_source.split("#[cfg(test)]").next().unwrap();
        let terminated = ["termination", "_observed"].concat();
        assert!(!production.contains(&terminated), "watchdog must not write termination");
        assert!(!production.contains("UPDATE attempts"));
        let targeted = include_str!("store/targeted.rs");
        let start = targeted.find("fn hot_path_rows_decoded").unwrap();
        let body = &targeted[start..targeted[start..].find("\n    fn ").map(|end| start + end).unwrap_or(targeted.len())];
        assert!(body.contains("hot_path_page_rows"), "{body}");
        assert!(!body.contains(&snapshot_reader), "{body}");
    }

    #[test]
    fn newer_schema_does_not_guess_counters_or_leak_task_text() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir_all(project.join(".state")).unwrap();
        let db = SqliteStore::create(&project.join(".state/state.db")).unwrap();
        drop(db);
        let connection = rusqlite::Connection::open(project.join(".state/state.db")).unwrap();
        connection.execute_batch("PRAGMA user_version = 99").unwrap();
        drop(connection);
        let error = report(&project, false).unwrap_err();
        let ReportError::Newer(value) = error else { panic!("expected newer schema") };
        assert_eq!(value["schema"], 99);
        assert_eq!(value["error"], "unsupported_schema");
        assert_eq!(value["prepared_dispatch"], false);
        assert!(value.get("counters").is_none(), "{value}");
        forbid(&value);
    }
}

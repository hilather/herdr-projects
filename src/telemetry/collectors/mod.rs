//! Lane A (docs/telemetry/phase2-lanes.md): sidecar stream `ingest`. Hooks
//! registered centrally in `super::LANES`; this lane adds subcommands, metrics,
//! tick work and `migrations/telemetry/ingest/NNNN_*.sql` here only.
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

pub const STREAM: &str = "ingest";
/// `include_str!` of `migrations/telemetry/ingest/`, in order; index + 1 is the stream version.
pub const MIGRATIONS: &[&str] = &[include_str!("../../../migrations/telemetry/ingest/0001_source_bindings.sql")];

/// `herdr-projects telemetry <slug> collectors ...`
#[derive(clap::Subcommand)]
pub enum Command {
    /// Stream version of this lane's sidecar tables. Read-only.
    Status,
    /// Canonical collector binding revisions and why each rollout is bound or not. Read-only.
    Bindings,
    /// Append a `revoked` revision to the attempt's active collector binding:
    /// rollouts that start from now on are not bound to it (contracts-collection.md).
    Revoke { attempt: String },
}

/// The command's stdout.
pub fn run(project: &Path, command: Command) -> Result<String> {
    let value = match command {
        Command::Status => super::sidecar::status(project, STREAM)?,
        Command::Bindings => bindings(project)?,
        Command::Revoke { attempt } => {
            let mut store = crate::store::SqliteStore::open(&project.join(".state/state.db"))?;
            let (binding, written) = store.revoke_collector_binding(&attempt, jiff::Timestamp::now().as_millisecond())?;
            json!({"binding": binding, "written": written})
        }
    };
    Ok(serde_json::to_string_pretty(&value)? + "\n")
}

fn exists(db: &rusqlite::Connection, table: &str) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)", [table], |r| r.get(0))?)
}

/// `{bindings, sources}`: every canonical revision, and each rollout source's
/// binding with its `basis` (`null` until a collect of this binary). Read-only.
fn bindings(project: &Path) -> Result<Value> {
    let mut bindings = Vec::new();
    let state = project.join(".state/state.db");
    if state.exists() {
        let db = super::read_only(&state)?;
        if exists(&db, "collector_bindings")? {
            let mut stmt = db.prepare("SELECT attempt_id,revision,state,collector,unix_ms FROM collector_bindings ORDER BY attempt_id,revision")?;
            for row in stmt.query_map([], |r| Ok(json!({"attempt_id": r.get::<_, String>(0)?, "revision": r.get::<_, i64>(1)?,
                "state": r.get::<_, String>(2)?, "collector": r.get::<_, Option<String>>(3)?, "unix_ms": r.get::<_, i64>(4)?})))? { bindings.push(row?); }
        }
    }
    let mut sources = Vec::new();
    if let Some(db) = super::sidecar::read(project)? {
        let basis = if exists(&db, "source_bindings")? { "(SELECT basis FROM source_bindings b WHERE b.path_digest=s.path_digest)" } else { "NULL" };
        let mut stmt = db.prepare(&format!("SELECT s.session_id,s.binding,s.attempt_id,{basis} FROM rollout_sources s ORDER BY s.session_id,s.path_digest"))?;
        for row in stmt.query_map([], |r| Ok(json!({"session_id": r.get::<_, String>(0)?, "binding": r.get::<_, String>(1)?,
            "attempt_id": r.get::<_, Option<String>>(2)?, "basis": r.get::<_, Option<String>>(3)?})))? { sources.push(row?); }
    }
    Ok(json!({"bindings": bindings, "sources": sources}))
}

/// Metrics merged into `telemetry <slug> report` (`super::metrics::report`).
pub fn metrics(_project: &Path, _since: Option<i64>) -> Result<BTreeMap<String, Value>> { Ok(BTreeMap::new()) }

/// Ticker telemetry pass, after the Codex collect, within `budget`. Writes only the sidecar.
pub fn tick(_project: &Path, _budget: super::codex::Budget) -> Result<()> { Ok(()) }

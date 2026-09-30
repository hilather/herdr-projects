//! Replay evaluation suite registry (migration 0064, plan TM4.6,
//! docs/telemetry/contracts-replay.md). The replay owner records a
//! versioned suite of cases built from accepted historical tasks and the
//! runs that replay a subset of them. A run's tasks are ordinary tasks
//! registered as replay candidates: they launch, verify and consume budget
//! through the ordinary paths, but a replay candidate is an evaluation
//! artefact, like a seeded candidate (`seeded_defects.rs`): it never
//! integrates ([`refuse_replay_integration`], the `integration_jobs`
//! eligibility predicate and the migration's triggers) and never releases a
//! dependent (no dependency edge or satisfaction can name it). Every row is
//! append-only; the hidden checks themselves are never stored here.
use super::*;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

/// The only replay principal: the project owner at the CLI.
pub const REPLAY_PRINCIPAL: &str = "operator:cli";
/// Authority recorded on every `replay_log` row.
pub const REPLAY_AUTHORITY: &str = "replay_owner.v1";
/// Appended to the integration producer's eligibility once the registry exists.
pub(super) const NOT_REPLAY: &str = " AND NOT EXISTS(SELECT 1 FROM replay_candidates x WHERE x.task_id=s.task_id)";

/// Whether the replay registry's migration has run.
pub(super) fn registry_present(tx: &Connection) -> Result<bool> {
    Ok(tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='replay_candidates')", [], |r| r.get(0))?)
}

/// Refuse to begin integrating a verified result of a replay candidate.
pub(super) fn refuse_replay_integration(tx: &Connection, result_id: &str) -> Result<()> {
    if registry_present(tx)? && tx.query_row("SELECT EXISTS(SELECT 1 FROM verified_results r JOIN result_submissions s ON s.submission_id=r.submission_id
        JOIN replay_candidates c ON c.task_id=s.task_id WHERE r.result_id=?1)", [result_id], |r| r.get::<_, bool>(0))? {
        return Err(StoreError::Invalid("a replay candidate never integrates".into()));
    }
    Ok(())
}

/// Refuse a dependency edge on a replay candidate (the migration's triggers repeat it).
pub(super) fn refuse_replay_predecessor(tx: &Connection, predecessor: &str) -> Result<()> {
    if registry_present(tx)? && tx.query_row("SELECT EXISTS(SELECT 1 FROM replay_candidates WHERE task_id=?1)", [predecessor], |r| r.get::<_, bool>(0))? {
        return Err(StoreError::Invalid("a replay candidate never releases dependents".into()));
    }
    Ok(())
}

/// One accepted historical task: its latest integrated submission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReplaySource {
    pub task_id: String,
    pub title: String,
    pub attempt_ids: Vec<String>,
    pub submission_id: String,
    pub result_id: String,
    pub contract_revision: i64,
    pub contract_digest: String,
    pub repository: String,
    pub object_format: String,
    pub base_oid: String,
    pub reference_oid: String,
    pub integrated_oid: String,
    pub route: String,
    #[serde(skip)]
    pub contract_raw: Vec<u8>,
    /// `(path, uncertain)` of each write scope path.
    pub write_paths: Vec<(String, bool)>,
    pub write_named_resources: Vec<String>,
    pub dependencies: usize,
    /// The TM0.6 classification recorded for this contract revision, if any.
    pub classification: Option<serde_json::Value>,
}

/// A suite version as recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplaySuiteRecord {
    pub suite_version: String,
    pub extractor: String,
    pub manifest_digest: String,
    pub exclusions: serde_json::Value,
}

/// One case as recorded (JSON columns carried as values).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayCaseRecord {
    pub case_id: String,
    pub ordinal: i64,
    pub source_task_id: String,
    pub source_submission_id: String,
    pub source_result_id: String,
    pub contract_revision: i64,
    pub contract_digest: String,
    pub repository: String,
    pub object_format: String,
    pub base_oid: String,
    pub reference_oid: String,
    pub integrated_oid: String,
    pub hidden_check_ref: String,
    pub hidden_checks: serde_json::Value,
    pub solution_paths: serde_json::Value,
    pub classification: serde_json::Value,
    pub stratum: String,
    pub contamination: serde_json::Value,
    pub status: String,
}

/// A replay candidate's outcome for the report (M49).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReplayCandidateStatus {
    pub task_id: String,
    pub run_id: String,
    pub suite_version: String,
    pub case_id: String,
    pub stratum: String,
    /// The run's configuration label.
    pub configuration: String,
    /// The configuration its first attempt was dispatched with (dispatch log).
    pub configuration_id: Option<String>,
    pub attempt_id: Option<String>,
    pub decided_unix_ms: Option<i64>,
    /// `passed`, `failed`, `pending`, `not_launched` or `retired`.
    pub status: String,
}

fn invalid(message: String) -> StoreError { StoreError::Invalid(message) }

fn schema_replay(tx: &Connection) -> Result<()> {
    check_schema(tx)?;
    if !registry_present(tx)? { return Err(StoreError::UnsupportedSchema(tx.query_row("PRAGMA user_version", [], |r| r.get(0))?)); }
    Ok(())
}

/// Refuse every principal but the replay owner.
fn replay_authority(tx: &Connection, principal: &str) -> Result<()> {
    let bare = principal.strip_prefix("worker:").unwrap_or(principal);
    if principal.starts_with("worker:") || tx.query_row("SELECT EXISTS(SELECT 1 FROM attempts WHERE id=?1)", [bare], |r| r.get::<_, bool>(0))? {
        return Err(invalid("a worker cannot record replay cases or runs: only the replay owner can".into()));
    }
    if principal != REPLAY_PRINCIPAL { return Err(invalid(format!("no replay authority for {principal}: only {REPLAY_PRINCIPAL} (the project owner) is"))); }
    Ok(())
}

fn log(tx: &Connection, kind: &str, principal: &str, now: i64) -> Result<i64> {
    replay_authority(tx, principal)?;
    tx.execute("INSERT INTO replay_log(kind,principal,authority,recorded_unix_ms) VALUES(?1,?2,?3,?4)", params![kind, principal, REPLAY_AUTHORITY, now])?;
    Ok(tx.last_insert_rowid())
}

fn text(value: &serde_json::Value) -> String { value.to_string() }

/// Accepted historical tasks, one per task (its latest integrated
/// submission), ordered by task id. Replay candidates are never sources.
pub fn replay_sources(db: &Connection) -> Result<Vec<ReplaySource>> {
    let replay = registry_present(db)?;
    let exclude = if replay { "AND NOT EXISTS(SELECT 1 FROM replay_candidates x WHERE x.task_id=s.task_id)" } else { "" };
    let mut stmt = db.prepare(&format!("SELECT s.task_id,t.title,s.submission_id,r.result_id,s.contract_revision,s.contract_digest,s.repository,s.object_format,
            s.base_oid,s.candidate_oid,ic.commit_oid,c.route,c.raw_bytes
        FROM integrated_commits ic JOIN integration_operations i ON i.operation_id=ic.operation_id AND i.state='integrated'
        JOIN verified_results r ON r.result_id=i.verified_result_id JOIN result_submissions s ON s.submission_id=r.submission_id
        JOIN task_contracts c ON c.task_id=s.task_id AND c.contract_revision=s.contract_revision JOIN tasks t ON t.id=s.task_id
        WHERE 1=1 {exclude} ORDER BY s.task_id,ic.created_unix_ms DESC,ic.integrated_id"))?;
    let rows = stmt.query_map([], |r| Ok(ReplaySource { task_id: r.get(0)?, title: r.get(1)?, attempt_ids: Vec::new(), submission_id: r.get(2)?, result_id: r.get(3)?,
        contract_revision: r.get(4)?, contract_digest: r.get(5)?, repository: r.get(6)?, object_format: r.get(7)?, base_oid: r.get(8)?, reference_oid: r.get(9)?,
        integrated_oid: r.get(10)?, route: r.get(11)?, contract_raw: r.get(12)?, write_paths: Vec::new(), write_named_resources: Vec::new(), dependencies: 0, classification: None }))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut sources: Vec<ReplaySource> = Vec::new();
    for mut source in rows {
        if sources.last().is_some_and(|s| s.task_id == source.task_id) { continue; }
        source.attempt_ids = db.prepare("SELECT id FROM attempts WHERE task_id=?1 ORDER BY id")?.query_map([&source.task_id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        source.write_paths = db.prepare("SELECT path,certainty FROM contract_scope_paths WHERE task_id=?1 AND contract_revision=?2 AND access='write' ORDER BY ordinal LIMIT 64")?
            .query_map(params![source.task_id, source.contract_revision], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)? == "uncertain")))?.collect::<rusqlite::Result<_>>()?;
        source.write_named_resources = db.prepare("SELECT name FROM contract_named_resources WHERE task_id=?1 AND contract_revision=?2 AND access='write' ORDER BY name LIMIT 3")?
            .query_map(params![source.task_id, source.contract_revision], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        source.dependencies = db.query_row("SELECT count(*) FROM task_dependencies WHERE task_id=?1", [&source.task_id], |r| r.get::<_, i64>(0))? as usize;
        source.classification = db.query_row("SELECT classification_id,class,band,classifier FROM task_classifications WHERE task_id=?1 AND contract_revision=?2 AND taxonomy=?3
            ORDER BY revision DESC LIMIT 1", params![source.task_id, source.contract_revision, TASK_TAXONOMY],
            |r| Ok(serde_json::json!({"classification_id": r.get::<_, String>(0)?, "class": r.get::<_, String>(1)?, "band": r.get::<_, String>(2)?,
                "classifier": r.get::<_, String>(3)?, "source": "task_classifications"}))).optional()?;
        sources.push(source);
    }
    Ok(sources)
}

/// Worker brief payloads `(operation id, payload)`, for the contamination scan.
pub fn replay_brief_payloads(db: &Connection) -> Result<Vec<(String, String)>> {
    Ok(db.prepare("SELECT id,payload FROM operations WHERE kind='runtime.worker_brief' ORDER BY id LIMIT 4096")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?)
}

fn case_row(r: &rusqlite::Row) -> rusqlite::Result<ReplayCaseRecord> {
    let json = |i: usize| -> rusqlite::Result<serde_json::Value> { Ok(serde_json::from_str(&r.get::<_, String>(i)?).unwrap_or(serde_json::Value::Null)) };
    Ok(ReplayCaseRecord { case_id: r.get(0)?, ordinal: r.get(1)?, source_task_id: r.get(2)?, source_submission_id: r.get(3)?, source_result_id: r.get(4)?,
        contract_revision: r.get(5)?, contract_digest: r.get(6)?, repository: r.get(7)?, object_format: r.get(8)?, base_oid: r.get(9)?, reference_oid: r.get(10)?,
        integrated_oid: r.get(11)?, hidden_check_ref: r.get(12)?, hidden_checks: json(13)?, solution_paths: json(14)?, classification: json(15)?,
        stratum: r.get(16)?, contamination: json(17)?, status: r.get(18)? })
}
const CASE_COLUMNS: &str = "case_id,ordinal,source_task_id,source_submission_id,source_result_id,contract_revision,contract_digest,repository,object_format,
    base_oid,reference_oid,integrated_oid,hidden_check_ref,hidden_checks,solution_paths,classification,stratum,contamination,status";

/// A suite version and its cases in ordinal order, if recorded.
pub fn replay_suite(db: &Connection, suite: &str) -> Result<Option<(ReplaySuiteRecord, Vec<ReplayCaseRecord>)>> {
    if !registry_present(db)? { return Ok(None); }
    let Some(record) = db.query_row("SELECT suite_version,extractor,manifest_digest,exclusions FROM replay_suites WHERE suite_version=?1", [suite],
        |r| Ok(ReplaySuiteRecord { suite_version: r.get(0)?, extractor: r.get(1)?, manifest_digest: r.get(2)?,
            exclusions: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or(serde_json::Value::Null) })).optional()? else { return Ok(None) };
    let cases = db.prepare(&format!("SELECT {CASE_COLUMNS} FROM replay_cases WHERE suite_version=?1 ORDER BY ordinal"))?
        .query_map([suite], case_row)?.collect::<rusqlite::Result<_>>()?;
    Ok(Some((record, cases)))
}

/// Retired cases of `suite`: `(case id, reason, seq)`.
pub fn replay_retirements(db: &Connection, suite: &str) -> Result<Vec<(String, String, i64)>> {
    if !registry_present(db)? { return Ok(Vec::new()); }
    Ok(db.prepare("SELECT case_id,reason,seq FROM replay_retirements WHERE suite_version=?1 ORDER BY seq")?
        .query_map([suite], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?)
}

/// Runs of `suite`: `(seq, run id, configuration, subset, seed, cases, recorded)`.
#[allow(clippy::type_complexity)]
pub fn replay_runs(db: &Connection, suite: &str) -> Result<Vec<(i64, String, String, String, String, Vec<String>, i64)>> {
    if !registry_present(db)? { return Ok(Vec::new()); }
    Ok(db.prepare("SELECT r.seq,r.run_id,r.configuration,r.subset,r.seed,r.cases,l.recorded_unix_ms FROM replay_runs r JOIN replay_log l ON l.seq=r.seq WHERE r.suite_version=?1 ORDER BY r.seq")?
        .query_map([suite], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or_default(), r.get(6)?)))?
        .collect::<rusqlite::Result<_>>()?)
}

/// The replay registration of task `task`: `(run id, suite, case id, repository)`.
pub fn replay_candidate(db: &Connection, task: &str) -> Result<Option<(String, String, String, String)>> {
    if !registry_present(db)? { return Ok(None); }
    Ok(db.query_row("SELECT run_id,suite_version,case_id,repository FROM replay_candidates WHERE task_id=?1", [task],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()?)
}

/// The raw signed contract bytes of `(task, revision)`.
pub fn replay_contract_raw(db: &Connection, task: &str, revision: i64) -> Result<Option<Vec<u8>>> {
    Ok(db.query_row("SELECT raw_bytes FROM task_contracts WHERE task_id=?1 AND contract_revision=?2", params![task, revision], |r| r.get(0)).optional()?)
}

/// Every replay candidate (of `suite`, or all) with its outcome, in run and case order.
pub fn replay_candidate_statuses(db: &Connection, suite: Option<&str>) -> Result<Vec<ReplayCandidateStatus>> {
    if !registry_present(db)? { return Ok(Vec::new()); }
    let decisions: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='dispatch_decisions')", [], |r| r.get(0))?;
    let rows: Vec<(String, String, String, String, String, String, bool)> = db.prepare("SELECT c.task_id,c.run_id,c.suite_version,c.case_id,r.configuration,x.stratum,
            EXISTS(SELECT 1 FROM replay_retirements t WHERE t.suite_version=c.suite_version AND t.case_id=c.case_id)
        FROM replay_candidates c JOIN replay_runs r ON r.run_id=c.run_id JOIN replay_cases x ON x.suite_version=c.suite_version AND x.case_id=c.case_id
        WHERE (?1 IS NULL OR c.suite_version=?1) ORDER BY r.seq,x.ordinal")?
        .query_map([suite], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::with_capacity(rows.len());
    for (task_id, run_id, suite_version, case_id, configuration, stratum, retired) in rows {
        let first: Option<(String, String, i64)> = if decisions {
            db.query_row("SELECT chosen_configuration_id,attempt_id,decided_unix_ms FROM dispatch_decisions WHERE task_id=?1 ORDER BY decided_unix_ms,attempt_id LIMIT 1",
                [&task_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?
        } else { None };
        // Passed: one submission of the current contract revision with an
        // accepted verification for every (hidden) acceptance policy.
        let passed: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM result_submissions s
            WHERE s.task_id=?1 AND s.contract_revision=(SELECT max(contract_revision) FROM task_contracts WHERE task_id=?1)
              AND EXISTS(SELECT 1 FROM acceptance_policies p WHERE p.task_id=s.task_id AND p.contract_revision=s.contract_revision)
              AND NOT EXISTS(SELECT 1 FROM acceptance_policies p WHERE p.task_id=s.task_id AND p.contract_revision=s.contract_revision
                  AND NOT EXISTS(SELECT 1 FROM verification_runs v WHERE v.submission_id=s.submission_id AND v.policy_id=p.policy_id AND v.state='accepted')))",
            [&task_id], |r| r.get(0))?;
        // Done: the task is terminal, or it has attempts and every one ended.
        let done: bool = db.query_row("SELECT (SELECT state FROM tasks WHERE id=?1) IN ('succeeded','failed','cancelled')
            OR (EXISTS(SELECT 1 FROM attempts WHERE task_id=?1) AND NOT EXISTS(SELECT 1 FROM attempts WHERE task_id=?1 AND termination_observed=0))",
            [&task_id], |r| r.get(0))?;
        let status = if retired { "retired" } else if first.is_none() { "not_launched" } else if passed { "passed" } else if done { "failed" } else { "pending" };
        out.push(ReplayCandidateStatus { task_id, run_id, suite_version, case_id, stratum, configuration, configuration_id: first.as_ref().map(|f| f.0.clone()),
            attempt_id: first.as_ref().map(|f| f.1.clone()), decided_unix_ms: first.map(|f| f.2), status: status.into() });
    }
    Ok(out)
}

impl SqliteStore {
    /// Record suite `suite` with its cases in one transaction. A suite version
    /// is immutable: recording an existing version is refused.
    pub fn record_replay_suite(&mut self, suite: &ReplaySuiteRecord, cases: &[ReplayCaseRecord], principal: &str, now: i64) -> Result<i64> {
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema_replay(&tx)?;
        replay_authority(&tx, principal)?;
        if tx.query_row("SELECT EXISTS(SELECT 1 FROM replay_suites WHERE suite_version=?1)", [&suite.suite_version], |r| r.get::<_, bool>(0))? {
            return Err(invalid(format!("replay suite {} is already recorded: a suite version is immutable", suite.suite_version)));
        }
        let seq = log(&tx, "suite", principal, now)?;
        tx.execute("INSERT INTO replay_suites(seq,suite_version,extractor,manifest_digest,exclusions) VALUES(?1,?2,?3,?4,?5)",
            params![seq, suite.suite_version, suite.extractor, suite.manifest_digest, text(&suite.exclusions)])?;
        for c in cases {
            tx.execute(&format!("INSERT INTO replay_cases(seq,suite_version,{CASE_COLUMNS}) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)"),
                params![seq, suite.suite_version, c.case_id, c.ordinal, c.source_task_id, c.source_submission_id, c.source_result_id, c.contract_revision, c.contract_digest,
                    c.repository, c.object_format, c.base_oid, c.reference_oid, c.integrated_oid, c.hidden_check_ref, text(&c.hidden_checks), text(&c.solution_paths),
                    text(&c.classification), c.stratum, text(&c.contamination), c.status])?;
        }
        tx.commit()?;
        Ok(seq)
    }

    /// Retire one case of `suite` whose checks no longer apply.
    pub fn retire_replay_case(&mut self, suite: &str, case: &str, reason: &str, principal: &str, now: i64) -> Result<i64> {
        if reason.trim().is_empty() || reason.len() > 160 || reason.chars().any(char::is_control) { return Err(invalid("a retirement reason is 1 to 160 plain characters".into())); }
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema_replay(&tx)?;
        replay_authority(&tx, principal)?;
        if !tx.query_row("SELECT EXISTS(SELECT 1 FROM replay_cases WHERE suite_version=?1 AND case_id=?2)", [suite, case], |r| r.get::<_, bool>(0))? {
            return Err(invalid(format!("no case {case} in replay suite {suite}")));
        }
        if tx.query_row("SELECT EXISTS(SELECT 1 FROM replay_retirements WHERE suite_version=?1 AND case_id=?2)", [suite, case], |r| r.get::<_, bool>(0))? {
            return Err(invalid(format!("case {case} of replay suite {suite} is already retired")));
        }
        let seq = log(&tx, "retired", principal, now)?;
        tx.execute("INSERT INTO replay_retirements(seq,suite_version,case_id,reason) VALUES(?1,?2,?3,?4)", params![seq, suite, case, reason])?;
        tx.commit()?;
        Ok(seq)
    }

    /// Record one replay run over `cases` (eligible, unretired cases of `suite`).
    #[allow(clippy::too_many_arguments)]
    pub fn record_replay_run(&mut self, run_id: &str, suite: &str, configuration: &str, subset: &str, seed: &str, cases: &[String], principal: &str, now: i64) -> Result<i64> {
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema_replay(&tx)?;
        replay_authority(&tx, principal)?;
        for case in cases {
            let eligible: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM replay_cases c WHERE c.suite_version=?1 AND c.case_id=?2 AND c.status='eligible'
                AND NOT EXISTS(SELECT 1 FROM replay_retirements x WHERE x.suite_version=c.suite_version AND x.case_id=c.case_id))", [suite, case.as_str()], |r| r.get(0))?;
            if !eligible { return Err(invalid(format!("case {case} of replay suite {suite} is not eligible (contaminated, retired or unknown)"))); }
        }
        let seq = log(&tx, "run", principal, now)?;
        tx.execute("INSERT INTO replay_runs(seq,run_id,suite_version,configuration,subset,seed,cases) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![seq, run_id, suite, configuration, subset, seed, serde_json::to_string(cases).unwrap_or_default()])?;
        tx.commit()?;
        Ok(seq)
    }

    /// Register the fresh task `task` as the run's replay candidate for `case`.
    #[allow(clippy::too_many_arguments)]
    /// The source repository of `task`'s replay case, `None` when `task` is
    /// not a replay candidate. Its sandbox hides it (contracts-replay.md §4).
    pub fn replay_source_repository(&self, task: &str) -> Result<Option<String>> {
        if !registry_present(&self.connection)? { return Ok(None); }
        Ok(self.connection.query_row("SELECT c.repository FROM replay_candidates k JOIN replay_cases c ON c.suite_version=k.suite_version AND c.case_id=k.case_id
            WHERE k.task_id=?1", [task], |r| r.get(0)).optional()?)
    }

    pub fn register_replay_candidate(&mut self, task: &str, run_id: &str, suite: &str, case: &str, repository: &str, principal: &str, now: i64) -> Result<()> {
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema_replay(&tx)?;
        replay_authority(&tx, principal)?;
        tx.execute("INSERT INTO replay_candidates(task_id,run_id,suite_version,case_id,repository,registered_unix_ms) VALUES(?1,?2,?3,?4,?5,?6)",
            params![task, run_id, suite, case, repository, now])?;
        tx.commit()?;
        Ok(())
    }
}

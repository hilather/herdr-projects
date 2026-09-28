//! One automatic reservation per wake. Does not launch.
use crate::domain::*;
use crate::launch_preparation::seal_admission_inputs;
use crate::store::{SqliteStore, StoreError};
use crate::store::admission_read::{Header, Cursor};
use crate::store::controlled::{ControlledStore, ReadControl};
use crate::store::read_budget::ReadBudget;
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::Path;

fn store_file(project: &Path) -> Result<std::path::PathBuf> {
    let path = project.join(".state/state.db");
    std::fs::canonicalize(&path).with_context(|| format!("project store missing at {}", path.display()))
}

#[cfg(test)]
fn open_store(project: &Path) -> Result<SqliteStore> {
    SqliteStore::open(&store_file(project)?).map_err(anyhow::Error::from)
}

/// Schema 30+ and `factory_admission=on`. A missing column or older store stays off.
/// This probe does not integrity-check: the off flag is the steady state, and a
/// full `open` on every wake would run before the existing hint path.
pub fn wake_enabled(project: &Path) -> bool {
    if crate::watchdog::is_paused(project) { return false; }
    let Ok(path) = store_file(project) else { return false };
    let Ok(connection) = rusqlite::Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    ) else { return false };
    let Ok(version) = connection.query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0)) else { return false };
    if version < 30 { return false; }
    connection.query_row("SELECT factory_admission FROM project_control WHERE singleton=1", [], |row| row.get::<_, String>(0)).ok().as_deref() == Some("on")
}

fn placeholder_approval() -> VersionedReference {
    VersionedReference { id: "unsigned-launch".into(), revision: 1, digest: "0".repeat(64) }
}

struct Candidate {
    task: Task,
    binding: RuntimeBinding,
    task_contract: Option<VersionedReference>,
    contract: Option<PreparedContract>,
    dependencies: Vec<DependencyInput>,
    repositories: Vec<RepositoryInput>,
    score: i64,
    blocker: Option<&'static str>,
}

fn ranked_page(db: &mut SqliteStore, header: &Header, now: i64, after: Option<&Cursor>, control: &ReadControl, budget:&ReadBudget) -> Result<(Vec<Candidate>, Option<Cursor>)> {
    if header.control.state != ProjectState::Active || header.control.reconciliation_required
        || header.retained >= u64::from(header.policy.max_active_workers) {
        return Ok((Vec::new(), None));
    }
    control.check()?;
    let page = db.admission_page_with_budget(header, now, after,Some(budget))?;
    let claims=if header.retained>0 {Some(db.admission_retained_claims(Some(budget))?)}else{None};
    let mut ranked = Vec::new();
    for entry in page.entries {
        control.check()?;
        let task = entry.task;
        let Some(binding) = entry.binding else { continue };
        let Some(edges) = db.satisfied_edges_with_budget(task.id.as_str(),Some(budget))? else { continue };
        let Some(repositories) = db.contract_pins_with_budget(task.id.as_str(), Some(control),Some(budget))? else { continue };
        let dependencies = edges.into_iter().map(|edge| edge.input()).collect::<Vec<_>>();
        let blocker = if claims.as_ref().map(|claims|db.admission_claim_overlap(task.id.as_str(),claims,Some(budget))).transpose()?.unwrap_or(false) { Some("resource_conflict") } else { None };
        let contract=db.admission_contract(task.id.as_str(),Some(budget))?;
        let task_contract=contract.as_ref().map(|contract|VersionedReference{id:contract.task_id.as_str().into(),revision:contract.contract_revision,digest:contract.digest.clone()});
        ranked.push(Candidate { task_contract, contract, task, binding, dependencies, repositories, score: entry.score, blocker });
    }
    control.check()?;
    Ok((ranked, page.next))
}

fn binding_profiles<'a>(db: &SqliteStore, profiles: &'a [FrozenProfile], control: &ProjectControl, candidate: &Candidate, now: i64,budget:&ReadBudget) -> Result<Vec<&'a FrozenProfile>> {
    let mut matches = Vec::new();
    for profile in profiles {
        if profile.config.digest == control.config_digest
            && (candidate.binding.identity.agent.is_empty() || profile.kind == candidate.binding.identity.agent)
            && db.admission_profile_matches_contract(candidate.contract.as_ref(), profile, now,Some(budget))? {
            matches.push(profile);
        }
    }
    Ok(matches)
}

fn arm(profile: &FrozenProfile, status: &'static str) -> Result<EligibleProfile> {
    Ok(EligibleProfile { configuration: agent_configuration(profile), profile_digest: profile.reference().map_err(anyhow::Error::msg)?.digest, status })
}

/// `None` when the task has no retained worker snapshot for this profile: without
/// knowledge no worker brief can be built, so the candidate is not launched promptless.
fn seal(db: &SqliteStore, project_store: &str, header: &Header, candidate: &Candidate, profile: &FrozenProfile, approval: VersionedReference, budget: &ReadBudget) -> Result<Option<LaunchInputs>> {
    let Some(memory) = db.worker_knowledge_selection(&candidate.task, profile, Some(budget))? else { return Ok(None) };
    let mut inputs = seal_admission_inputs(project_store, &candidate.task, &candidate.binding, header.policy.revision, header.control.epoch, profile, approval, candidate.dependencies.clone(), candidate.repositories.clone(), header.budget.clone(), memory).map_err(anyhow::Error::msg)?;
    inputs.task_contract = candidate.task_contract.clone();
    Ok(Some(inputs))
}

fn knowledge_missing(task: &TaskId) -> anyhow::Error {
    let task = task.as_str();
    anyhow::anyhow!("knowledge_missing: ready task {task} has no worker knowledge snapshot for a matching profile; create one with `memory <slug> snapshot --task {task} --worker`")
}

fn record_missing_grant(db: &mut SqliteStore, task: &TaskId, head: u64, now: i64) -> Result<()> {
    // Stable id: a retry of the same task does not insert another denial.
    let id = format!("admit-{}", &format!("{:x}", Sha256::digest(task.as_str().as_bytes()))[..32]);
    let denial = AuthorityDenial {
        id,
        unix_ms: now,
        class: "approval".into(),
        command: "admit".into(),
        actor_channel: "unknown-rejected".into(),
        reason_code: "authority_missing".into(),
        policy_digest: "0".repeat(64),
        expected_head: Some(head),
        actual_head: Some(head),
    };
    match db.insert_denial(&denial) {
        Ok(()) | Err(StoreError::Conflict) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Inputs for the next candidate, with a placeholder approval. `None` when nothing is ready to sign.
pub fn prepared_admission_inputs(project: &Path) -> Result<Option<LaunchInputs>> {
    let control=ReadControl::new(std::time::Instant::now()+std::time::Duration::from_secs(10),crate::runner::Cancellation::default());
    ControlledStore::open_scoped(&store_file(project)?,control)?.prepare_admission(project)
}
pub(crate) fn prepare_held(project: &Path, db: &mut SqliteStore, read_control: &ReadControl,budget:&ReadBudget) -> Result<Option<LaunchInputs>> {
    let now = jiff::Timestamp::now().as_millisecond();
    let project_store = store_file(project)?;
    let project_store = project_store.to_str().context("project store is not UTF-8")?;
    let header = db.admission_header_with_budget(Some(budget))?;
    if header.control.state!=ProjectState::Active||header.control.reconciliation_required||header.retained>=u64::from(header.policy.max_active_workers) {
        read_control.check()?;return Ok(None);
    }
    let profiles = db.admission_profiles_with_budget(header.control.config_digest.as_deref(),Some(budget))?;
    let mut cursor = None;
    let mut missing = None;
    loop {
        let (candidates, next) = ranked_page(db, &header, now, cursor.as_ref(), read_control,budget)?;
        for candidate in candidates.into_iter().filter(|candidate| candidate.blocker.is_none()) {
            for profile in binding_profiles(&db, &profiles, &header.control, &candidate, now,budget)? {
                if let Some(inputs) = seal(db, project_store, &header, &candidate, profile, placeholder_approval(), budget)? {
                    return Ok(Some(inputs));
                }
                missing.get_or_insert_with(|| candidate.task.id.clone());
            }
        }
        cursor = next;
        if cursor.is_none() { break; }
    }
    match missing { Some(task) => Err(knowledge_missing(&task)), None => Ok(None) }
}

/// `capacity_full` while verify or integrate work is older than the watermark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmissionBlock {
    pub blocker: &'static str,
    pub reason: &'static str,
}

/// No schema column. Every scheduler policy revision uses this default.
fn backlog_watermark_ms(_policy_revision: u64) -> i64 {
    15 * 60 * 1000
}

fn backlog_reason(verification_age: Option<i64>, integration_age: Option<i64>, watermark: i64) -> Option<&'static str> {
    if verification_age.is_some_and(|age| age > watermark) {
        return Some("verification_backlog");
    }
    if integration_age.is_some_and(|age| age > watermark) {
        return Some("integration_backlog");
    }
    None
}

/// Why `admit_once` returned. `block` is set only when admission must not reserve.
pub struct AdmissionDecision {
    pub block: Option<AdmissionBlock>,
    pub reason: &'static str,
    pub task_id: Option<String>,
}

/// Reserve at most one ready attempt through `reserve_prepared`. Does not launch.
/// A verify or integrate backlog older than the watermark returns `capacity_full` and does not reserve.
/// A watchdog pause returns `admission_paused` and does not reserve.
pub fn admit_once(project: &Path) -> Result<Option<AdmissionBlock>> {
    Ok(admit_decision(project)?.block)
}

pub fn admit_decision(project: &Path) -> Result<AdmissionDecision> {
    let control=ReadControl::new(std::time::Instant::now()+std::time::Duration::from_secs(2),crate::runner::Cancellation::default());
    admit_decision_with_control(project,control)
}
/// Observation survives errors, including opening failures and cancellation.
/// SQL counts cover the controlled connection, not filesystem/Git work or the
/// controller's earlier enabled probe. Duration covers this entire call.
pub struct AdmissionObservation {
    pub result: Result<AdmissionDecision>,
    pub sql: crate::store::controlled::SqlWorkMetrics,
    pub duration_ms: u64,
}

pub fn admit_decision_observed(project: &Path) -> AdmissionObservation {
    let control=ReadControl::new(std::time::Instant::now()+std::time::Duration::from_secs(2),crate::runner::Cancellation::default());
    admit_decision_observed_with_control(project,control)
}

pub fn admit_decision_with_control(project: &Path, control: ReadControl) -> Result<AdmissionDecision> {
    admit_decision_observed_with_control(project,control).result
}

pub fn admit_decision_observed_with_control(project: &Path, control: ReadControl) -> AdmissionObservation {
    let started=std::time::Instant::now();
    let work=crate::store::controlled::SqlWork::default();
    let result=(|| {
        control.check()?;
        if let Some(reason) = crate::watchdog::pause_reason(project) {
            return Ok(AdmissionDecision {
                block: Some(AdmissionBlock { blocker: "admission_paused", reason }),
                reason,
                task_id: None,
            });
        }
        ControlledStore::open_scoped_observed(&store_file(project)?,control,work.clone())?.decide_admission(project)
    })();
    AdmissionObservation {result,sql:work.snapshot(),duration_ms:u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)}
}
pub(crate) fn decide_held(project: &Path, db: &mut SqliteStore, read_control: &ReadControl,budget:&ReadBudget) -> Result<AdmissionDecision> {
    if !db.factory_admission_enabled()? {
        return Ok(AdmissionDecision { block: None, reason: "admission_off", task_id: None });
    }
    let now = jiff::Timestamp::now().as_millisecond();
    let project_store = store_file(project)?;
    let project_store = project_store.to_str().context("project store is not UTF-8")?;
    let header = db.admission_header_with_budget(Some(budget))?;
    let revision = header.policy.revision;
    let watermark = backlog_watermark_ms(revision);
    let (verification_oldest, integration_oldest) = db.admission_backlog_ages()?;
    if let Some(reason) = backlog_reason(verification_oldest.map(|created| now.saturating_sub(created)), integration_oldest.map(|created| now.saturating_sub(created)), watermark) {
        return Ok(AdmissionDecision { block: Some(AdmissionBlock { blocker: "capacity_full", reason }), reason, task_id: None });
    }
    let head = header.head;
    if header.control.state!=ProjectState::Active||header.control.reconciliation_required||header.retained>=u64::from(header.policy.max_active_workers) {
        read_control.check()?;return Ok(AdmissionDecision{block:None,reason:"idle",task_id:None});
    }
    let profiles = db.admission_profiles_with_budget(header.control.config_digest.as_deref(),Some(budget))?;
    let mut denied = None;
    let mut missing = None;
    let cursor = db.admission_cursor_with_budget(&header,Some(budget))?;
    let (candidates, next) = ranked_page(db, &header, now, cursor.as_ref(), read_control,budget)?;
    for candidate in candidates.iter().filter(|candidate| candidate.blocker.is_none()) {
        let mut sealed = None;
        let (mut matched, mut knowledge) = (false, false);
        // Telemetry: the status each matched profile reached, in evaluation order. Never consulted to choose.
        let matches = binding_profiles(&db, &profiles, &header.control, candidate, now,budget)?;
        let mut eligible = Vec::with_capacity(matches.len());
        for profile in &matches {
            matched = true;
            let Some(inputs) = seal(db, project_store, &header, candidate, profile, placeholder_approval(), budget)? else { eligible.push(arm(profile, "no_knowledge")?); continue };
            knowledge = true;
            if let Some(reference) = db.matching_launch_approval(&inputs, now,Some(budget))? {
                let mut inputs = inputs;
                inputs.approval = reference;
                sealed = Some(inputs);
                eligible.push(arm(profile, "chosen")?);
                break;
            }
            eligible.push(arm(profile, "no_approval")?);
        }
        if let Some(inputs) = sealed {
            for profile in &matches[eligible.len()..] { eligible.push(arm(profile, "not_evaluated")?); }
            // Head was read before this write. A later mutation conflicts instead of reserving a stale snapshot.
            db.reserve_prepared_controlled(&[PreparedLaunch { inputs }], head, now, read_control,budget,&DispatchContext::Automatic { eligible })?;
            return Ok(AdmissionDecision { block: None, reason: "reserved", task_id: Some(candidate.task.id.as_str().to_string()) });
        }
        if matched && !knowledge {
            // Fail closed: no retained worker snapshot means no brief, so no launch.
            missing.get_or_insert_with(|| candidate.task.id.as_str().to_string());
            continue;
        }
        // One denial for this task, then the next candidate. A grant for another profile is not this miss.
        record_missing_grant(db, &candidate.task.id, head, now)?;
        denied = Some(candidate.task.id.as_str().to_string());
    }
    read_control.check()?;
    db.advance_admission_cursor(&header,next.as_ref())?;
    Ok(AdmissionDecision {
        block: None,
        reason: if next.is_some() { "scan_incomplete" } else if denied.is_some() { "authority_missing" } else if missing.is_some() { "knowledge_missing" } else { "idle" },
        task_id: denied.or(missing),
    })
}

#[cfg(test)]
fn readiness_now(project: &Path) -> Result<Vec<Candidate>> {
    let now = jiff::Timestamp::now().as_millisecond();
    let mut db = open_store(project)?;
    let header = db.admission_header()?;
    let mut candidates = Vec::new();
    let control=ReadControl::new(std::time::Instant::now()+std::time::Duration::from_secs(30),crate::runner::Cancellation::default());
    let budget=ReadBudget::new(control.clone());
    let mut cursor = None;
    loop {
        let (page, next) = ranked_page(&mut db, &header, now, cursor.as_ref(), &control,&budget)?;
        candidates.extend(page);
        cursor = next;
        if cursor.is_none() { break; }
    }
    Ok(candidates)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SqliteStore;

    fn budget() -> ReadBudget { ReadBudget::new(ReadControl::new(std::time::Instant::now()+std::time::Duration::from_secs(30),Default::default())) }

    fn claim(kind: &str, resource: &str, access: &str, certainty: &str) -> crate::store::ResourceClaim {
        crate::store::ResourceClaim { kind: kind.into(), resource: resource.into(), access: access.into(), certainty: certainty.into() }
    }

    #[test]
    fn overlap_rules_block_exclusive_writes_named_resources_and_uncertain_paths() {
        let read = claim("path", "README.md", "read", "exact");
        let write = claim("path", "README.md", "write", "exact");
        assert!(!crate::store::claims_conflict(&read, &read));
        assert!(crate::store::claims_conflict(&write, &read));
        assert!(!crate::store::claims_conflict(&claim("path", "src/a.rs", "write", "exact"), &claim("path", "src/b.rs", "write", "exact")));
        assert!(crate::store::claims_conflict(&claim("path", "migrations/", "read", "uncertain"), &claim("path", "migrations/0035_resource_claims.sql", "read", "exact")));
        assert!(crate::store::claims_conflict(&claim("path", "src/*.rs", "read", "uncertain"), &claim("path", "src/lib.rs", "read", "exact")));
        assert!(!crate::store::claims_conflict(&claim("path", "src/*.rs", "read", "uncertain"), &claim("path", "docs/lib.rs", "read", "exact")));
        assert!(crate::store::claims_conflict(&claim("named", "schema", "write", "exact"), &claim("named", "schema", "read", "exact")));
        assert!(!crate::store::claims_conflict(&claim("named", "schema", "read", "exact"), &claim("named", "schema", "read", "exact")));
        assert!(!crate::store::claims_conflict(&claim("named", "schema", "write", "exact"), &claim("named", "lockfile", "write", "exact")));
        assert!(!crate::store::claims_conflict(&claim("path", "migrations/", "write", "uncertain"), &claim("named", "schema", "write", "exact")));
    }

    fn user_version(path: &Path) -> u32 {
        rusqlite::Connection::open(path).unwrap().query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap()
    }
    fn table_exists(path: &Path, name: &str) -> bool {
        rusqlite::Connection::open(path).unwrap().query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?1)", [name], |row| row.get(0)).unwrap()
    }

    #[test]
    fn upgrade_v1_from_34_to_35_and_create_end_at_user_version_35() {
        let fresh = tempfile::tempdir().unwrap();
        let created_path = fresh.path().join("state.db");
        let created = SqliteStore::create(&created_path).unwrap();
        drop(created);
        assert_eq!(user_version(&created_path), crate::store::SCHEMA);
        assert!(table_exists(&created_path, "resource_claims"));

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.db");
        let mut db = SqliteStore::create(&path).unwrap();
        db.commit(Commit { expected_head: 0, mutations: vec![Mutation::Task { expected: None, next: Task { id: TaskId::new("kept").unwrap(), revision: 1, state: TaskState::Draft, title: "kept".into(), active_attempt: None } }] }).unwrap();
        drop(db);
        let raw = rusqlite::Connection::open(&path).unwrap();
        let sequence: i64 = raw.query_row("SELECT MAX(sequence) FROM events", [], |row| row.get(0)).unwrap();
        crate::store::test_schema::historical(&raw, 34).unwrap();
        raw.execute(
            "INSERT INTO task_contracts(task_id,contract_revision,plan_revision,project_store,expected_head,repository,base_oid,object_format,memory_snapshot_id,route,raw_bytes,raw_digest,installed_seq) VALUES('kept',1,NULL,'/tmp/project',0,'/tmp/repo','aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa','sha1',NULL,'verify_only',x'61',?1,?2)",
            rusqlite::params!["ab".repeat(32), sequence],
        ).unwrap();
        raw.execute("INSERT INTO contract_scope_paths(task_id,contract_revision,ordinal,path,access,certainty) VALUES('kept',1,0,'migrations/','write','uncertain')", []).unwrap();
        raw.execute("INSERT INTO contract_scope_paths(task_id,contract_revision,ordinal,path,access,certainty) VALUES('kept',1,1,'src/lib.rs','read','exact')", []).unwrap();
        raw.execute("INSERT INTO contract_named_resources(task_id,contract_revision,name,access) VALUES('kept',1,'schema','write')", []).unwrap();
        drop(raw);
        let mut db = SqliteStore::open(&path).unwrap();
        assert_eq!(user_version(&path), 34);
        assert!(!table_exists(&path, "resource_claims"));
        assert_eq!(db.read_snapshot(None).unwrap().tasks[0].title, "kept");
        db.upgrade_v1().unwrap();
        assert_eq!(user_version(&path), crate::store::SCHEMA);
        let claims: Vec<(i64, String, String, String, String)> = {
            let raw = rusqlite::Connection::open(&path).unwrap();
            let mut stmt = raw.prepare("SELECT ordinal, kind, resource, access, certainty FROM resource_claims ORDER BY ordinal").unwrap();
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))).unwrap().map(|row| row.unwrap()).collect()
        };
        assert_eq!(claims, vec![
            (0, "path".into(), "migrations/".into(), "write".into(), "uncertain".into()),
            (1, "path".into(), "src/lib.rs".into(), "read".into(), "exact".into()),
            (64, "named".into(), "schema".into(), "write".into(), "exact".into()),
        ]);
        assert!(rusqlite::Connection::open(&path).unwrap().execute("DELETE FROM resource_claims", []).is_err());
        let mut reopened = SqliteStore::open(&path).unwrap();
        assert_eq!(user_version(&path), crate::store::SCHEMA);
        assert_eq!(reopened.read_snapshot(None).unwrap().schema_version, crate::store::SCHEMA);
    }

    struct Spec<'a> {
        id: &'a str,
        priority: i32,
        age_ms: i64,
        paths: &'static [(&'static str, &'static str)],
        named: &'static [(&'static str, &'static str)],
    }

    fn git_commit(repo: &std::path::Path) -> String {
        let git = |args: &[&str]| {
            let output = std::process::Command::new("/usr/bin/git").arg("-C").arg(repo).args(args).env_clear().env("PATH", "/usr/bin:/bin").env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@example.com").env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@example.com").output().unwrap();
            assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        };
        git(&["init", "-b", "main"]);
        std::fs::write(repo.join("README.md"), "a").unwrap();
        git(&["add", "README.md"]);
        git(&["commit", "-m", "a"]);
        git(&["rev-parse", "HEAD"])
    }

    fn plant_profile(db_path: &Path) {
        use sha2::{Digest, Sha256};
        use std::os::unix::fs::MetadataExt;
        let canonical = std::fs::canonicalize(db_path).unwrap();
        let metadata = std::fs::metadata(&canonical).unwrap();
        let profile = crate::domain::profile::fixture(crate::migration::ConfigReference { path: "/no/such/admission-config.toml".into(), digest: None });
        let reference = profile.reference().unwrap();
        let report = serde_json::json!({"preparation":{"profile":profile,"reference":reference,"launchable":true,"protocol_capable":false,"certified":false},"source_store":[canonical, metadata.dev(), metadata.ino()]});
        let text = serde_json::to_string(&report).unwrap();
        let digest = format!("{:x}", Sha256::digest(text.as_bytes()));
        let sequence: i64 = rusqlite::Connection::open(db_path).unwrap().query_row("SELECT MAX(sequence) FROM events", [], |row| row.get(0)).unwrap();
        rusqlite::Connection::open(db_path).unwrap().execute("INSERT INTO native_profiles(profile_digest,report,report_digest,sequence) VALUES(?1,?2,?3,?4)", rusqlite::params![reference.digest, text, digest, sequence]).unwrap();
    }

    fn admission_on(db_path: &Path) {
        let column = ["factory", "_admission"].concat();
        rusqlite::Connection::open(db_path).unwrap().execute(&format!("UPDATE project_control SET {column}=?1 WHERE singleton=1"), ["on"]).unwrap();
    }

    fn world(cap: u32, specs: &[Spec]) -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let repo = root.path().join("repo");
        std::fs::create_dir_all(project.join(".state")).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        let oid = git_commit(&repo);
        let repo = std::fs::canonicalize(&repo).unwrap();
        let db_path = project.join(".state/state.db");
        let mut db = SqliteStore::create(&db_path).unwrap();
        let now = jiff::Timestamp::now().as_millisecond();
        db.commit(Commit { expected_head: 0, mutations: specs.iter().map(|spec| Mutation::Task { expected: None, next: Task { id: TaskId::new(spec.id).unwrap(), revision: 1, state: TaskState::Draft, title: spec.id.into(), active_attempt: None } }).collect() }).unwrap();
        for spec in specs {
            let id = TaskId::new(spec.id).unwrap();
            let head = db.read_snapshot(None).unwrap().head;
            db.create_runtime(Some(&id), Some(1), head, &RuntimeRoute::default()).unwrap();
            let head = db.read_snapshot(None).unwrap().head;
            db.queue_task(&id, 2, head, &QueueRequest { priority: spec.priority, dependencies: vec![] }, now - spec.age_ms).unwrap();
        }
        let snapshot = db.read_snapshot(None).unwrap();
        db.set_scheduler_policy(snapshot.head, snapshot.scheduler.unwrap().policy.revision, cap, 3).unwrap();
        let snapshot = db.read_snapshot(None).unwrap();
        let observations = snapshot.runtime_bindings.iter().map(|binding| {
            let revision = binding.task.as_ref().and_then(|id| snapshot.tasks.iter().find(|task| &task.id == id).map(|task| task.revision));
            crate::reconcile::RuntimeObservation { binding: binding.id.clone(), binding_revision: binding.revision, task_revision: revision, observed_unix_ms: now, collector: "herdr-git-v1".into(), ..crate::reconcile::RuntimeObservation::default() }
        }).collect::<Vec<_>>();
        db.record_observations(snapshot.head, &observations).unwrap();
        let snapshot = db.read_snapshot(None).unwrap();
        db.set_project_state(snapshot.head, snapshot.control.unwrap().revision, ProjectState::Active, now, None).unwrap();
        let store = std::fs::canonicalize(&db_path).unwrap().display().to_string();
        for spec in specs {
            let head = db.read_snapshot(None).unwrap().head;
            let mut bytes = serde_json::to_vec_pretty(&serde_json::json!({
                "version": 1,
                "project_store": store,
                "expected_head": head,
                "task_id": spec.id,
                "contract_revision": 1,
                "deliverable": spec.id,
                "non_goals": "no launch",
                "acceptance_policies": [{"id": "builds", "text": "tests pass"}],
                "repository": repo.display().to_string(),
                "base_oid": oid,
                "object_format": "sha1",
                "dependencies": [],
                "scope": {
                    "paths": spec.paths.iter().map(|(path, access)| serde_json::json!({"path": path, "access": access})).collect::<Vec<_>>(),
                    "named_resources": spec.named.iter().map(|(name, access)| serde_json::json!({"name": name, "access": access})).collect::<Vec<_>>()
                },
                "capability_flags": [],
                "profile_kind": "claude",
                "retry_class": "none",
                "result_schema_id": "result-v1",
                "route": "verify_only",
                "authority": {"id": "owner-approval-policy", "revision": 1, "digest": "ab".repeat(32)}
            })).unwrap();
            bytes.push(b'\n');
            db.install_contract(&PreparedContract::parse_verified(&bytes).unwrap()).unwrap();
        }
        drop(db);
        plant_profile(&db_path);
        admission_on(&db_path);
        for spec in specs { worker_snapshot(&project, spec.id); }
        (root, project)
    }

    /// The retained worker knowledge an automatic launch binds for its brief.
    fn worker_snapshot(project: &Path, task: &str) -> VersionedReference {
        let profile = crate::domain::profile::fixture(crate::migration::ConfigReference { path: "/no/such/admission-config.toml".into(), digest: None });
        let db = SqliteStore::open(&project.join(".state/state.db")).unwrap();
        let mut memory = crate::memory::MemoryStore::from_sqlite(db, project.join(".state/objects"));
        let snapshot = memory.create_worker_snapshot(SnapshotRequest {
            schema_version: 1, task_id: task.into(), profile: profile.name.clone(), domains: vec![], paths: vec![], pinned_keys: vec![], sensitivity: "default".into(),
        }, &profile.name, &profile.definition_digest, None, 32000, "Admission fixture instructions", jiff::Timestamp::now().as_millisecond(), None).unwrap();
        VersionedReference { id: snapshot.id.as_str().into(), revision: 1, digest: snapshot.manifest_hash }
    }

    fn blockers(project: &Path) -> Vec<(String, i64, Option<&'static str>)> {
        readiness_now(project).unwrap().into_iter().map(|candidate| (candidate.task.id.as_str().to_string(), candidate.score, candidate.blocker)).collect()
    }
    fn attempt_tasks(project: &Path) -> Vec<String> {
        let mut db = SqliteStore::open(&project.join(".state/state.db")).unwrap();
        let mut tasks = db.read_snapshot(None).unwrap().attempts.into_iter().map(|attempt| attempt.task.as_str().to_string()).collect::<Vec<_>>();
        tasks.sort();
        tasks
    }
    fn grant_and_admit(project: &Path) -> Option<String> {
        let inputs = prepared_admission_inputs(project).unwrap()?;
        let task = inputs.task.as_str().to_string();
        let mut db = SqliteStore::open(&project.join(".state/state.db")).unwrap();
        let now = jiff::Timestamp::now().as_millisecond();
        let grant = ApprovalGrant { version: 1, scope: ApprovalScope::for_launch(&inputs).unwrap(), policy: inputs.effective_profile.unwrap().permission_policy, issued_unix_ms: 0, expires_unix_ms: now + 3_600_000 };
        let head = db.read_snapshot(None).unwrap().head;
        db.install_approval(&PreparedApproval { grant }, head, now).unwrap();
        drop(db);
        admit_once(project).unwrap();
        Some(task)
    }

    #[test]
    fn disjoint_tasks_stay_ready_and_overlapping_paths_do_not_both_reserve() {
        let (_root, project) = world(2, &[
            Spec { id: "early", priority: 0, age_ms: 120 * 60_000, paths: &[("migrations/", "write")], named: &[] },
            Spec { id: "late", priority: 20, age_ms: 0, paths: &[("migrations/0035_resource_claims.sql", "write")], named: &[] },
            Spec { id: "other", priority: 0, age_ms: 0, paths: &[("src/other.rs", "write")], named: &[] },
        ]);
        let ranked = blockers(&project);
        assert_eq!(ranked.iter().map(|(task, _, _)| task.as_str()).collect::<Vec<_>>(), vec!["early", "late", "other"]);
        assert!(ranked.iter().all(|(_, _, blocker)| blocker.is_none()));
        assert!(ranked[0].1 > ranked[1].1, "aged priority still outranks a newer higher priority");
        assert_eq!(grant_and_admit(&project).as_deref(), Some("early"));
        let ranked = blockers(&project);
        assert_eq!(ranked.iter().find(|(task, _, _)| task == "late").unwrap().2, Some("resource_conflict"));
        assert_eq!(ranked.iter().find(|(task, _, _)| task == "other").unwrap().2, None);
        assert_eq!(attempt_tasks(&project), vec!["early".to_string()]);
        assert_eq!(grant_and_admit(&project).as_deref(), Some("other"));
        assert_eq!(attempt_tasks(&project), vec!["early".to_string(), "other".to_string()]);
        assert!(grant_and_admit(&project).is_none());
        assert_eq!(attempt_tasks(&project), vec!["early".to_string(), "other".to_string()]);
    }

    fn install_empty_revision(project: &Path, task: &str) {
        let db_path = project.join(".state/state.db");
        let (repository, base): (String, String) = {
            let raw = rusqlite::Connection::open(&db_path).unwrap();
            raw.query_row(
                "SELECT repository, base_oid FROM task_contracts WHERE task_id=?1 AND contract_revision=1",
                [task],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ).unwrap()
        };
        let mut db = SqliteStore::open(&db_path).unwrap();
        let head = db.read_snapshot(None).unwrap().head;
        let store = std::fs::canonicalize(&db_path).unwrap().display().to_string();
        let mut bytes = serde_json::to_vec_pretty(&serde_json::json!({
            "version": 1,
            "project_store": store,
            "expected_head": head,
            "task_id": task,
            "contract_revision": 2,
            "deliverable": "cleared scope",
            "non_goals": "no launch",
            "acceptance_policies": [{"id": "builds", "text": "tests pass"}],
            "repository": repository,
            "base_oid": base,
            "object_format": "sha1",
            "dependencies": [],
            "capability_flags": [],
            "profile_kind": "claude",
            "retry_class": "none",
            "result_schema_id": "result-v1",
            "route": "verify_only",
            "authority": {"id": "owner-approval-policy", "revision": 1, "digest": "ab".repeat(32)}
        })).unwrap();
        bytes.push(b'\n');
        db.install_contract(&PreparedContract::parse_verified(&bytes).unwrap()).unwrap();
    }

    fn force_reserve(project: &Path, task: &str) -> Result<(), String> {
        let candidate = readiness_now(project).unwrap().into_iter().find(|candidate| candidate.task.id.as_str() == task).unwrap();
        let mut db = open_store(project).unwrap();
        let state = db.admission_header().unwrap();
        let profiles = db.admission_profiles().unwrap();
        let control = state.control.clone();
        let now = jiff::Timestamp::now().as_millisecond();
        let profile = binding_profiles(&db, &profiles, &control, &candidate, now,&budget()).unwrap().into_iter().next().unwrap();
        let store = store_file(project).unwrap();
        let store = store.to_str().unwrap().to_string();
        let mut inputs = seal(&db, &store, &state, &candidate, profile, placeholder_approval(), &budget()).unwrap().unwrap();
        let now = jiff::Timestamp::now().as_millisecond();
        let grant = ApprovalGrant {
            version: 1,
            scope: ApprovalScope::for_launch(&inputs).unwrap(),
            policy: inputs.effective_profile.clone().unwrap().permission_policy,
            issued_unix_ms: 0,
            expires_unix_ms: now + 3_600_000,
        };
        let head = db.read_snapshot(None).unwrap().head;
        inputs.approval = db.install_approval(&PreparedApproval { grant }, head, now).unwrap();
        let head = db.read_snapshot(None).unwrap().head;
        db.reserve_prepared(&[PreparedLaunch { inputs }], head, now).map(|_| ()).map_err(|error| error.to_string())
    }

    #[test]
    fn later_empty_contract_does_not_drop_the_reserved_claim() {
        let (_root, project) = world(2, &[
            Spec { id: "early", priority: 0, age_ms: 120 * 60_000, paths: &[("migrations/", "write")], named: &[] },
            Spec { id: "late", priority: 20, age_ms: 0, paths: &[("migrations/0035_resource_claims.sql", "write")], named: &[] },
        ]);
        assert_eq!(grant_and_admit(&project).as_deref(), Some("early"));
        install_empty_revision(&project, "early");
        let ranked = blockers(&project);
        assert_eq!(ranked.iter().find(|(task, _, _)| task == "late").unwrap().2, Some("resource_conflict"));
        let error = force_reserve(&project, "late").unwrap_err();
        assert!(error.contains("resource_conflict"), "{error}");
        assert_eq!(attempt_tasks(&project), vec!["early".to_string()]);
    }

    #[test]
    fn shared_reads_proceed_and_named_write_blocks_the_later_reader() {
        let (_root, project) = world(2, &[
            Spec { id: "read_old", priority: 0, age_ms: 120 * 60_000, paths: &[("README.md", "read")], named: &[("schema", "read")] },
            Spec { id: "read_new", priority: 20, age_ms: 0, paths: &[("README.md", "read")], named: &[("schema", "read")] },
        ]);
        assert!(blockers(&project).iter().all(|(_, _, blocker)| blocker.is_none()));
        assert_eq!(grant_and_admit(&project).as_deref(), Some("read_old"));
        assert_eq!(grant_and_admit(&project).as_deref(), Some("read_new"));
        assert_eq!(attempt_tasks(&project), vec!["read_new".to_string(), "read_old".to_string()]);

        let (_root, project) = world(2, &[
            Spec { id: "writer", priority: 0, age_ms: 120 * 60_000, paths: &[], named: &[("schema", "write")] },
            Spec { id: "reader", priority: 20, age_ms: 0, paths: &[], named: &[("schema", "read")] },
        ]);
        assert_eq!(grant_and_admit(&project).as_deref(), Some("writer"));
        assert_eq!(blockers(&project)[0], ("reader".into(), blockers(&project)[0].1, Some("resource_conflict")));
        assert!(grant_and_admit(&project).is_none());
        assert_eq!(attempt_tasks(&project), vec!["writer".to_string()]);
    }

    #[test]
    fn cancel_does_not_free_a_slot_or_a_claim_before_termination() {
        let (_root, project) = world(2, &[
            Spec { id: "early", priority: 0, age_ms: 120 * 60_000, paths: &[("migrations/", "write")], named: &[] },
            Spec { id: "late", priority: 20, age_ms: 0, paths: &[("migrations/0035_resource_claims.sql", "read")], named: &[] },
            Spec { id: "other", priority: 0, age_ms: 0, paths: &[("src/other.rs", "write")], named: &[] },
        ]);
        assert_eq!(grant_and_admit(&project).as_deref(), Some("early"));
        let db_path = project.join(".state/state.db");
        let mut db = SqliteStore::open(&db_path).unwrap();
        let snapshot = db.read_snapshot(None).unwrap();
        let attempt = snapshot.attempts.iter().find(|attempt| attempt.task.as_str() == "early").unwrap().clone();
        let operation = snapshot.attempt_inputs.iter().find(|input| input.attempt == attempt.id).unwrap().operation.clone();
        let now = jiff::Timestamp::now().as_millisecond();
        db.claim_operation(&operation, 1, "worker", now, 1_000).unwrap();
        let snapshot = db.read_snapshot(None).unwrap();
        let attempt = snapshot.attempts.iter().find(|attempt| attempt.task.as_str() == "early").unwrap().clone();
        let cancelled = db.cancel_attempt(&attempt.id, attempt.revision, snapshot.head, "stop before termination", now).unwrap();
        assert!(!cancelled.released);
        let snapshot = db.read_snapshot(None).unwrap();
        assert!(snapshot.attempts.iter().any(|attempt| attempt.task.as_str() == "early" && attempt.retains_capacity()));
        let report = db.queue_report(now).unwrap();
        assert_eq!(report.retained_attempts, 1);
        assert_eq!(report.available_slots, 1);
        drop(db);
        assert_eq!(blockers(&project).iter().find(|(task, _, _)| task == "late").unwrap().2, Some("resource_conflict"));
        assert_eq!(grant_and_admit(&project).as_deref(), Some("other"));
        assert!(snapshot_retains(&project, "early"));
        assert_eq!(attempt_tasks(&project).len(), 2);
        rusqlite::Connection::open(&db_path).unwrap().execute("UPDATE attempts SET termination_observed=1 WHERE task_id='early'", []).unwrap();
        assert!(!snapshot_retains(&project, "early"));
        assert_eq!(blockers(&project).iter().find(|(task, _, _)| task == "late").unwrap().2, None);
        assert_eq!(grant_and_admit(&project).as_deref(), Some("late"));
        assert!(attempt_tasks(&project).contains(&"late".to_string()));
    }

    fn plant_finished_attempt(project: &Path) {
        let mut db = SqliteStore::open(&project.join(".state/state.db")).unwrap();
        let head = db.read_snapshot(None).unwrap().head;
        db.commit(Commit { expected_head: head, mutations: vec![Mutation::Attempt { expected: None, next: Attempt {
            id: AttemptId::new("attempt-1").unwrap(), task: TaskId::new("early").unwrap(), revision: 1, state: AttemptState::Completed, snapshot: None, reservation: "slot-backlog".into(), termination_observed: true,
        } }] }).unwrap();
    }
    fn db(project: &Path) -> rusqlite::Connection {
        let connection = rusqlite::Connection::open(project.join(".state/state.db")).unwrap();
        connection.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        connection
    }
    fn insert_submission(connection: &rusqlite::Connection, project_store: &str, repository: &str, oid: &str, created_unix_ms: i64) -> String {
        let digest = "ab".repeat(32);
        let submission = "12".repeat(32);
        connection.execute(
            "INSERT INTO result_submissions(submission_id,project_store,idempotency_key,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,memory_snapshot_id,artifact_manifest,claimed_checks,created_unix_ms) VALUES(?1,?2,'submit-1',?3,'{}','early',1,?3,'attempt-1',?4,?5,?5,'sha1',NULL,'{}','[]',?6)",
            rusqlite::params![submission, project_store, digest, repository, oid, created_unix_ms],
        ).unwrap();
        submission
    }
    fn insert_verified(connection: &rusqlite::Connection, project_store: &str, submission: &str, oid: &str, created_unix_ms: i64) -> String {
        let digest = "ab".repeat(32);
        let run = "34".repeat(32);
        let result = "56".repeat(32);
        connection.execute(
            "INSERT INTO verification_runs(run_id,project_store,idempotency_key,payload_digest,submission_id,task_id,contract_revision,contract_digest,attempt_id,policy_id,policy_digest,commit_oid,tree_oid,object_format,memory_fence,isolation,argv,library_manifest,state,reason,exit_status,receipt_digest,store_device,store_inode,created_unix_ms) VALUES(?1,?2,'verify-1',?3,?4,'early',1,?3,'attempt-1','builds',?3,?5,?5,'sha1',0,'linux-unshare-user-pid-mount-v1','[]','{}','accepted',NULL,0,?3,1,1,?6)",
            rusqlite::params![run, project_store, digest, submission, oid, created_unix_ms],
        ).unwrap();
        connection.execute(
            "INSERT INTO verified_results(result_id,run_id,submission_id,commit_oid,tree_oid,object_format,policy_digest,receipt_digest,isolation,memory_fence,created_unix_ms) VALUES(?1,?2,?3,?4,?4,'sha1',?5,?5,'linux-unshare-user-pid-mount-v1',0,?6)",
            rusqlite::params![result, run, submission, oid, digest, created_unix_ms],
        ).unwrap();
        result
    }
    fn insert_integration(project: &Path, state: &str, created_unix_ms: i64) {
        plant_finished_attempt(project);
        let connection = db(project);
        let (project_store, repository, oid): (String, String, String) = connection.query_row(
            "SELECT project_store, repository, base_oid FROM task_contracts WHERE task_id='early' AND contract_revision=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).unwrap();
        let submission = insert_submission(&connection, &project_store, &repository, &oid, created_unix_ms);
        let result = insert_verified(&connection, &project_store, &submission, &oid, created_unix_ms);
        let digest = format!("{:x}", sha2::Sha256::digest(b"{}"));
        connection.execute(
            "INSERT INTO operations(id,task_id,kind,target,payload_version,payload,payload_hash,expected_revision,due_unix_ms,idempotency_key) VALUES('op-1','early','integration.lease','refs/heads/queue',1,'{}',?1,1,0,'op-1')",
            [&digest],
        ).unwrap();
        connection.execute(
            "INSERT INTO integration_targets(repository,ref_name,created_unix_ms) VALUES(?1,'refs/heads/queue',?2)",
            rusqlite::params![repository, created_unix_ms],
        ).unwrap();
        connection.execute(
            "INSERT INTO integration_target_leases(repository,ref_name,operation_id,generation) VALUES(?1,'refs/heads/queue','op-1',1)",
            [repository.as_str()],
        ).unwrap();
        connection.execute(
            "INSERT INTO integration_operations(operation_id,project_store,idempotency_key,payload_digest,repository,ref_name,expected_old_oid,verified_result_id,candidate_id,state,generation,object_format,checks_passed,reason,created_unix_ms) VALUES('op-1',?1,'integrate-1',?2,?3,'refs/heads/queue',?4,?5,NULL,?6,1,'sha1',0,NULL,?7)",
            rusqlite::params![project_store, digest, repository, oid, result, state, created_unix_ms],
        ).unwrap();
    }

    #[test]
    fn backlog_watermark_is_fifteen_minutes_and_equality_does_not_block() {
        let watermark = backlog_watermark_ms(1);
        assert_eq!(watermark, 15 * 60 * 1000);
        assert_eq!(backlog_watermark_ms(4), watermark);
        assert_eq!(backlog_reason(Some(watermark), Some(watermark), watermark), None);
        assert_eq!(backlog_reason(Some(watermark + 1), None, watermark), Some("verification_backlog"));
        assert_eq!(backlog_reason(None, Some(watermark + 1), watermark), Some("integration_backlog"));
        assert_eq!(backlog_reason(Some(watermark + 1), Some(watermark + 5), watermark), Some("verification_backlog"));
    }

    fn attempt_count(project: &Path) -> usize {
        SqliteStore::open(&project.join(".state/state.db")).unwrap().read_snapshot(None).unwrap().attempts.len()
    }

    #[test]
    fn verification_backlog_returns_capacity_full_and_does_not_start_work() {
        let (_root, project) = world(1, &[Spec { id: "early", priority: 0, age_ms: 0, paths: &[("README.md", "write")], named: &[] }]);
        plant_finished_attempt(&project);
        let connection = db(&project);
        let (project_store, repository, oid): (String, String, String) = connection.query_row(
            "SELECT project_store, repository, base_oid FROM task_contracts WHERE task_id='early' AND contract_revision=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).unwrap();
        let now = jiff::Timestamp::now().as_millisecond();
        insert_submission(&connection, &project_store, &repository, &oid, now - backlog_watermark_ms(1) - 60_000);
        drop(connection);
        let block = admit_once(&project).unwrap().expect("verification backlog");
        assert_eq!(block, AdmissionBlock { blocker: "capacity_full", reason: "verification_backlog" });
        assert!(prepared_admission_inputs(&project).unwrap().is_some());
        assert_eq!(attempt_count(&project), 1);
        assert!(grant_and_admit(&project).is_some());
        assert_eq!(attempt_count(&project), 1);
    }

    #[test]
    fn integration_backlog_returns_capacity_full_and_terminal_rows_do_not() {
        let old = jiff::Timestamp::now().as_millisecond() - backlog_watermark_ms(1) - 60_000;
        for state in ["integrated", "discarded", "blocked", "reconciliation_required"] {
            let (_root, project) = world(1, &[Spec { id: "early", priority: 0, age_ms: 0, paths: &[("README.md", "write")], named: &[] }]);
            insert_integration(&project, state, old);
            assert_eq!(admit_once(&project).unwrap(), None, "{state} is terminal and must not block");
            assert_eq!(attempt_count(&project), 1, "{state}");
            assert_eq!(grant_and_admit(&project).as_deref(), Some("early"), "{state}");
            assert_eq!(attempt_count(&project), 2, "{state}");
        }
        for state in ["effect_pending", "candidate_prepared", "validating"] {
            let (_root, project) = world(1, &[Spec { id: "early", priority: 0, age_ms: 0, paths: &[("README.md", "write")], named: &[] }]);
            insert_integration(&project, state, old);
            let block = admit_once(&project).unwrap().expect(state);
            assert_eq!(block, AdmissionBlock { blocker: "capacity_full", reason: "integration_backlog" }, "{state}");
            assert_eq!(attempt_count(&project), 1, "{state}");
            assert!(grant_and_admit(&project).is_some(), "{state}");
            assert_eq!(attempt_count(&project), 1, "{state} still reserved");
        }
    }

    fn snapshot_retains(project: &Path, task: &str) -> bool {
        let mut db = SqliteStore::open(&project.join(".state/state.db")).unwrap();
        db.read_snapshot(None).unwrap().attempts.iter().any(|attempt| attempt.task.as_str() == task && attempt.retains_capacity())
    }
    #[test]
    fn review_probe_signed_contract_dependency_is_enforced() {
        let (_root, project) = world(2, &[
            Spec { id: "early", priority: 10, age_ms: 0, paths: &[], named: &[] },
            Spec { id: "late", priority: 0, age_ms: 0, paths: &[], named: &[] },
        ]);
        let path = project.join(".state/state.db");
        let raw = rusqlite::Connection::open(&path).unwrap();
        let bytes: Vec<u8> = raw.query_row("SELECT raw_bytes FROM task_contracts WHERE task_id='early'", [], |r|r.get(0)).unwrap();
        let mut doc: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let mut db = SqliteStore::open(&path).unwrap();
        doc["contract_revision"] = 2.into();
        doc["expected_head"] = db.current_head().unwrap().into();
        doc["dependencies"] = serde_json::json!([{"predecessor":"late", "edge":"verified_result", "policy_id":"builds"}]);
        let prepared = PreparedContract::parse_verified(&serde_json::to_vec(&doc).unwrap()).unwrap();
        db.install_contract(&prepared).unwrap();
        assert_eq!(raw.query_row("SELECT count(*) FROM verified_results",[],|r|r.get::<_,i64>(0)).unwrap(),0);
        let selected = grant_and_admit(&project);
        assert_ne!(selected.as_deref(), Some("early"), "reserved a task whose signed predecessor has no verified result");
    }

    #[test]
    fn changed_contract_rejects_an_already_prepared_launch() {
        let (_root, project) = world(1, &[Spec { id: "task", priority: 0, age_ms: 0, paths: &[], named: &[] }]);
        let mut inputs = prepared_admission_inputs(&project).unwrap().unwrap();
        assert_eq!(inputs.task_contract.as_ref().unwrap().revision, 1);
        let path = project.join(".state/state.db");
        let mut db = SqliteStore::open(&path).unwrap();
        let now = jiff::Timestamp::now().as_millisecond();
        let grant = ApprovalGrant { version: 1, scope: ApprovalScope::for_launch(&inputs).unwrap(), policy: inputs.effective_profile.as_ref().unwrap().permission_policy.clone(), issued_unix_ms: 0, expires_unix_ms: now + 3_600_000 };
        inputs.approval = db.install_approval(&PreparedApproval { grant }, db.current_head().unwrap(), now).unwrap();
        let raw = rusqlite::Connection::open(&path).unwrap();
        let bytes: Vec<u8> = raw.query_row("SELECT raw_bytes FROM task_contracts WHERE task_id='task'", [], |r| r.get(0)).unwrap();
        let mut doc: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        doc["contract_revision"] = 2.into();
        doc["expected_head"] = db.current_head().unwrap().into();
        doc["deliverable"] = "Changed work requires a new approval".into();
        db.install_contract(&PreparedContract::parse_verified(&serde_json::to_vec(&doc).unwrap()).unwrap()).unwrap();
        let head = db.current_head().unwrap();
        let error = db.reserve_prepared(&[PreparedLaunch { inputs }], head, now).unwrap_err();
        assert!(error.to_string().contains("task contract changed"), "{error}");
        assert_eq!(db.current_head().unwrap(), head);
        assert!(attempt_tasks(&project).is_empty());
        assert_eq!(admit_decision(&project).unwrap().reason, "authority_missing");
    }

    #[test]
    fn contract_routes_by_profile_kind_and_required_capabilities() {
        for (kind, flags) in [("codex", serde_json::json!([])), ("claude", serde_json::json!(["workflow-certified"]))] {
            let (_root, project) = world(2, &[
                Spec { id: "early", priority: 10, age_ms: 0, paths: &[], named: &[] },
                Spec { id: "late", priority: 0, age_ms: 0, paths: &[], named: &[] },
            ]);
            let path = project.join(".state/state.db");
            let raw = rusqlite::Connection::open(&path).unwrap();
            let bytes: Vec<u8> = raw.query_row("SELECT raw_bytes FROM task_contracts WHERE task_id='early'", [], |r| r.get(0)).unwrap();
            let mut doc: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            let mut db = SqliteStore::open(&path).unwrap();
            doc["contract_revision"] = 2.into();
            doc["expected_head"] = db.current_head().unwrap().into();
            doc["profile_kind"] = kind.into();
            doc["capability_flags"] = flags;
            db.install_contract(&PreparedContract::parse_verified(&serde_json::to_vec(&doc).unwrap()).unwrap()).unwrap();
            assert_eq!(grant_and_admit(&project).as_deref(), Some("late"));
            assert_eq!(attempt_tasks(&project), vec!["late"]);
        }
    }

    #[test]
    fn admission_refuses_dense_profile_and_binding_payloads_before_reservation() {
        for profile in [true,false] {
            let(_root,project)=world(1,&[Spec{id:"bounded",priority:0,age_ms:0,paths:&[],named:&[]}]);
            let raw=rusqlite::Connection::open(store_file(&project).unwrap()).unwrap();
            let before:u64=raw.query_row("SELECT MAX(sequence) FROM events",[],|row|row.get(0)).unwrap();
            let (table,key,column,hash_column,trigger)=if profile {
                ("native_profiles","profile_digest","report","report_digest","native_profiles_no_update")
            } else {("runtime_bindings","id","payload","payload_hash","")};
            let(id,payload):(String,String)=raw.query_row(&format!("SELECT {key},{column} FROM {table} LIMIT 1"),[],|row|Ok((row.get(0)?,row.get(1)?))).unwrap();
            let mut value:serde_json::Value=serde_json::from_str(&payload).unwrap();
            value["dense_extension"]=serde_json::json!(vec![0;150_000]);
            let payload=serde_json::to_string(&value).unwrap();
            let digest=format!("{:x}",Sha256::digest(payload.as_bytes()));
            if !trigger.is_empty(){raw.execute_batch(&format!("DROP TRIGGER {trigger}")).unwrap();}
            raw.execute(&format!("UPDATE {table} SET {column}=?1,{hash_column}=?2 WHERE {key}=?3"),rusqlite::params![payload,digest,id]).unwrap();
            let observation=admit_decision_observed(&project);
            assert!(observation.sql.connection_observed);
            assert!(observation.sql.sqlite_rows_returned>0);
            assert!(observation.sql.sqlite_vm_steps>0);
            let error=observation.result.err().expect("dense JSON must exceed shared accounting before decoding");
            assert!(matches!(error.downcast_ref::<StoreError>(),Some(StoreError::Limit(_))),"{error:#}");
            let head:u64=raw.query_row("SELECT MAX(sequence) FROM events",[],|row|row.get(0)).unwrap();assert_eq!(head,before);
            let attempts:u64=raw.query_row("SELECT count(*) FROM attempts",[],|row|row.get(0)).unwrap();assert_eq!(attempts,0);
        }
    }

    #[test]
    fn admission_resumes_after_a_full_page_of_unsigned_candidates() {
        let names=(0..65).map(|n|format!("task-{n:03}")).collect::<Vec<_>>();
        let specs=names.iter().map(|name|Spec{id:name,priority:0,age_ms:0,paths:&[],named:&[]}).collect::<Vec<_>>();
        let (_root,project)=world(1,&specs);
        let candidates=readiness_now(&project).unwrap();
        let late=candidates.iter().find(|c|c.task.id.as_str()=="task-064").unwrap();
        let path=store_file(&project).unwrap();
        let mut db=SqliteStore::open(&path).unwrap();
        let header=db.admission_header().unwrap();
        let profiles=db.admission_profiles().unwrap();
        let now=jiff::Timestamp::now().as_millisecond();
        let profile=binding_profiles(&db,&profiles,&header.control,late,now,&budget()).unwrap()[0];
        let inputs=seal(&db,path.to_str().unwrap(),&header,late,profile,placeholder_approval(),&budget()).unwrap().unwrap();
        let first=admit_decision(&project).unwrap();
        assert_eq!(first.reason,"scan_incomplete");
        assert!(attempt_tasks(&project).is_empty());
        // Approval arrival changes the global head, but must not lose scan progress.
        let grant=ApprovalGrant{version:1,scope:ApprovalScope::for_launch(&inputs).unwrap(),policy:inputs.effective_profile.unwrap().permission_policy,issued_unix_ms:0,expires_unix_ms:now+3_600_000};
        db.install_approval(&PreparedApproval{grant},db.current_head().unwrap(),now).unwrap();
        drop(db);
        let observation=admit_decision_observed(&project);
        assert!(observation.sql.connection_observed);
        assert!(observation.sql.sqlite_rows_returned>0);
        assert!(observation.sql.sqlite_vm_steps>0);
        let second=observation.result.unwrap();
        assert_eq!(second.reason,"reserved");
        assert_eq!(second.task_id.as_deref(),Some("task-064"));
        assert_eq!(attempt_tasks(&project),vec!["task-064"]);
    }

    #[test]
    fn admission_preserves_the_callers_deadline_and_cancellation() {
        use std::time::{Duration,Instant};
        let (_root,project)=world(1,&[Spec{id:"task",priority:0,age_ms:0,paths:&[],named:&[]}]);
        let cancellation=crate::runner::Cancellation::default();
        cancellation.cancel();
        let observation=admit_decision_observed_with_control(&project,ReadControl::new(Instant::now()+Duration::from_secs(1),cancellation));
        assert!(!observation.sql.connection_observed);
        assert_eq!(observation.sql.sqlite_rows_returned,0);
        let error=observation.result.err().unwrap();
        assert!(matches!(error.downcast_ref::<StoreError>(),Some(StoreError::Cancelled)));
        let error=admit_decision_with_control(&project,ReadControl::new(Instant::now()-Duration::from_millis(1),Default::default())).err().unwrap();
        assert!(matches!(error.downcast_ref::<StoreError>(),Some(StoreError::Deadline)));
        assert!(attempt_tasks(&project).is_empty());
    }

    mod delegated { use super::*; include!("admission_delegated_tests.rs"); }

}

//! One automatic reservation per wake. Does not launch.
use crate::domain::*;
use crate::launch_preparation::seal_admission_inputs;
use crate::store::{SqliteStore, StoreError};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::Path;

fn store_file(project: &Path) -> Result<std::path::PathBuf> {
    let path = project.join(".state/state.db");
    std::fs::canonicalize(&path).with_context(|| format!("project store missing at {}", path.display()))
}

fn open_store(project: &Path) -> Result<SqliteStore> {
    SqliteStore::open(&store_file(project)?).map_err(anyhow::Error::from)
}

/// Schema 30+ and `factory_admission=on`. A missing column or older store stays off.
/// This probe does not integrity-check: the off flag is the steady state, and a
/// full `open` on every wake would run before the existing hint path.
pub fn wake_enabled(project: &Path) -> bool {
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
    dependencies: Vec<DependencyInput>,
    repositories: Vec<RepositoryInput>,
    score: i64,
    sequence: u64,
    blocker: Option<&'static str>,
}

fn unused_binding(state: &Snapshot, task: &TaskId) -> Option<RuntimeBinding> {
    state.runtime_bindings.iter().find(|binding| {
        binding.task.as_ref() == Some(task)
            && binding.identity.pane_id.is_empty()
            && binding.identity.tab_id.is_empty()
            && binding.identity.machine.is_empty()
            && binding.identity.worktree_path.is_empty()
            && !state.ownership.iter().any(|owned| {
                owned.binding == binding.id && (owned.attempt.is_some() || owned.session.is_some() || owned.agent.is_some())
            })
    }).cloned()
}

fn rank_candidates(db: &mut SqliteStore, state: &Snapshot, now: i64) -> Result<Vec<Candidate>> {
    let scheduler = state.scheduler.as_ref().context("scheduler missing")?;
    let control = state.control.as_ref().context("project control missing")?;
    if control.state != ProjectState::Active || control.reconciliation_required {
        return Ok(Vec::new());
    }
    let retained = state.attempts.iter().filter(|attempt| attempt.retains_capacity()).count();
    if retained >= scheduler.policy.max_active_workers as usize {
        return Ok(Vec::new());
    }
    // Consult claims only while admission is on. A retaining attempt keeps the
    // revision from its reservation until termination_observed. Cancel does not clear it.
    let admission_on = db.factory_admission_enabled()?;
    let mut ranked = Vec::new();
    for record in &scheduler.queue {
        let Some(task) = state.tasks.iter().find(|task| task.id == record.task && task.state == TaskState::Queued && task.active_attempt.is_none()) else { continue };
        if record.enqueued_unix_ms > now { continue }
        if state.attempts.iter().any(|attempt| attempt.task == task.id && attempt.retains_capacity()) { continue }
        if state.attempts.iter().filter(|attempt| attempt.task == task.id).count() >= scheduler.policy.max_attempts_per_task as usize { continue }
        let Some(edges) = db.satisfied_edges(task.id.as_str())? else { continue };
        let Some(binding) = unused_binding(state, &task.id) else { continue };
        // A contract whose tree cannot be read is not reserved.
        let Some(repositories) = db.contract_pins(task.id.as_str())? else { continue };
        let dependencies = edges.into_iter().map(|edge| DependencyInput {
            task: edge.predecessor,
            task_revision: edge.predecessor_revision,
            requirement: edge.requirement,
            evidence: VersionedReference { id: edge.satisfaction_id.clone(), revision: 1, digest: edge.satisfaction_id },
        }).collect::<Vec<_>>();
        let score = (now - record.enqueued_unix_ms) / 60_000 + i64::from(record.priority);
        let blocker = if admission_on && db.retained_claim_overlap(task.id.as_str())? {
            Some("resource_conflict")
        } else {
            None
        };
        ranked.push(Candidate { task: task.clone(), binding, dependencies, repositories, score, sequence: record.enqueue_sequence, blocker });
    }
    ranked.sort_by(|left, right| right.score.cmp(&left.score).then(left.sequence.cmp(&right.sequence)).then(left.task.id.cmp(&right.task.id)));
    Ok(ranked)
}

fn ready_candidates(db: &mut SqliteStore, state: &Snapshot, now: i64) -> Result<Vec<Candidate>> {
    Ok(rank_candidates(db, state, now)?.into_iter().filter(|candidate| candidate.blocker.is_none()).collect())
}

fn binding_profiles<'a>(profiles: &'a [FrozenProfile], control: &ProjectControl, binding: &RuntimeBinding) -> Vec<&'a FrozenProfile> {
    profiles.iter().filter(|profile| {
        profile.config.digest == control.config_digest && (binding.identity.agent.is_empty() || profile.kind == binding.identity.agent)
    }).collect()
}

fn seal(project_store: &str, state: &Snapshot, candidate: &Candidate, profile: &FrozenProfile, approval: VersionedReference) -> Result<LaunchInputs> {
    let scheduler = state.scheduler.as_ref().context("scheduler missing")?;
    let control = state.control.as_ref().context("project control missing")?;
    let budget = state.budget_policies.last().map(BudgetPolicy::reference).transpose().map_err(anyhow::Error::msg)?;
    seal_admission_inputs(project_store, &candidate.task, &candidate.binding, scheduler.policy.revision, control.epoch, profile, approval, candidate.dependencies.clone(), candidate.repositories.clone(), budget).map_err(anyhow::Error::msg)
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

fn accepted_grant(db: &SqliteStore, state: &Snapshot, inputs: &LaunchInputs, now: i64) -> Result<Option<VersionedReference>> {
    for approval in &state.approvals {
        if approval.revoked.is_some() || approval.consumed.is_some() { continue; }
        if now < approval.grant.issued_unix_ms || now >= approval.grant.expires_unix_ms { continue; }
        let mut candidate = inputs.clone();
        candidate.approval = approval.reference.clone();
        if approval.grant.matches_launch(&candidate, &inputs.project_store, now).is_err() { continue; }
        if db.preparation_grant_accepted(&candidate, now)? {
            return Ok(Some(approval.reference.clone()));
        }
    }
    Ok(None)
}

/// Inputs for the next candidate, with a placeholder approval. `None` when nothing is ready to sign.
pub fn prepared_admission_inputs(project: &Path) -> Result<Option<LaunchInputs>> {
    let mut db = open_store(project)?;
    let now = jiff::Timestamp::now().as_millisecond();
    let project_store = store_file(project)?;
    let project_store = project_store.to_str().context("project store is not UTF-8")?;
    let state = db.read_snapshot(None)?;
    let profiles = db.admission_profiles()?;
    let control = state.control.as_ref().context("project control missing")?;
    let Some(candidate) = ready_candidates(&mut db, &state, now)?.into_iter().next() else { return Ok(None) };
    let Some(profile) = binding_profiles(&profiles, control, &candidate.binding).into_iter().next() else { return Ok(None) };
    Ok(Some(seal(project_store, &state, &candidate, profile, placeholder_approval())?))
}

/// Reserve at most one ready attempt through `reserve_prepared`. Does not launch.
pub fn admit_once(project: &Path) -> Result<()> {
    let mut db = open_store(project)?;
    if !db.factory_admission_enabled()? {
        return Ok(());
    }
    let now = jiff::Timestamp::now().as_millisecond();
    let project_store = store_file(project)?;
    let project_store = project_store.to_str().context("project store is not UTF-8")?;
    let state = db.read_snapshot(None)?;
    let head = state.head;
    let profiles = db.admission_profiles()?;
    let control = state.control.clone().context("project control missing")?;
    let candidates = ready_candidates(&mut db, &state, now)?;
    for candidate in &candidates {
        let mut sealed = None;
        for profile in binding_profiles(&profiles, &control, &candidate.binding) {
            let inputs = seal(project_store, &state, candidate, profile, placeholder_approval())?;
            if let Some(reference) = accepted_grant(&db, &state, &inputs, now)? {
                let mut inputs = inputs;
                inputs.approval = reference;
                sealed = Some(inputs);
                break;
            }
        }
        if let Some(inputs) = sealed {
            // Head was read before this write. A later mutation conflicts instead of reserving a stale snapshot.
            db.reserve_prepared(&[PreparedLaunch { inputs }], head, now)?;
            return Ok(());
        }
        // One denial for this task, then the next candidate. A grant for another profile is not this miss.
        record_missing_grant(&mut db, &candidate.task.id, head, now)?;
    }
    Ok(())
}

#[cfg(test)]
fn readiness_now(project: &Path) -> Result<Vec<Candidate>> {
    let now = jiff::Timestamp::now().as_millisecond();
    let mut db = open_store(project)?;
    let state = db.read_snapshot(None)?;
    rank_candidates(&mut db, &state, now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SqliteStore;

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
        assert_eq!(user_version(&created_path), 35);
        assert!(table_exists(&created_path, "resource_claims"));
        let open_fn = include_str!("store/mod.rs").split("pub fn open").nth(1).unwrap().split("pub fn integrity_check").next().unwrap();
        assert!(!open_fn.contains("upgrade_v1"));
        assert!(!open_fn.contains("0035_resource_claims"));

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.db");
        let mut db = SqliteStore::create(&path).unwrap();
        db.commit(Commit { expected_head: 0, mutations: vec![Mutation::Task { expected: None, next: Task { id: TaskId::new("kept").unwrap(), revision: 1, state: TaskState::Draft, title: "kept".into(), active_attempt: None } }] }).unwrap();
        drop(db);
        let raw = rusqlite::Connection::open(&path).unwrap();
        let sequence: i64 = raw.query_row("SELECT MAX(sequence) FROM events", [], |row| row.get(0)).unwrap();
        raw.execute_batch("DROP TABLE resource_claims; UPDATE store_meta SET schema_version=34; PRAGMA user_version=34;").unwrap();
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
        assert_eq!(user_version(&path), 35);
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
        assert_eq!(user_version(&path), 35);
        assert_eq!(reopened.read_snapshot(None).unwrap().schema_version, 35);
    }

    struct Spec {
        id: &'static str,
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
                "profile_kind": "codex",
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
        (root, project)
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
            "profile_kind": "codex",
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
        let state = db.read_snapshot(None).unwrap();
        let profiles = db.admission_profiles().unwrap();
        let control = state.control.clone().unwrap();
        let profile = binding_profiles(&profiles, &control, &candidate.binding).into_iter().next().unwrap();
        let store = store_file(project).unwrap();
        let store = store.to_str().unwrap().to_string();
        let mut inputs = seal(&store, &state, &candidate, profile, placeholder_approval()).unwrap();
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

    fn snapshot_retains(project: &Path, task: &str) -> bool {
        let mut db = SqliteStore::open(&project.join(".state/state.db")).unwrap();
        db.read_snapshot(None).unwrap().attempts.iter().any(|attempt| attempt.task.as_str() == task && attempt.retains_capacity())
    }
}

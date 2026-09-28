//! Deterministic factory harness: disposable git repos and local fixtures only,
//! no network, agent, or slept timing.

#![cfg(feature = "state-store")]

use herdr_projects::{
    domain::*,
    store::SqliteStore,
};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

// `verify` execs `current_exe` inside `unshare`. This test binary is that
// executable, and the cargo harness has no verification-setup command.
#[cfg(target_os = "linux")]
#[used]
#[unsafe(link_section = ".init_array")]
static VERTICAL_SLICE_VERIFY_HOOK: unsafe extern "C" fn() = vertical_slice_verify_hook;

#[cfg(target_os = "linux")]
unsafe extern "C" fn vertical_slice_verify_hook() {
    if std::env::args().any(|arg| arg == "verification-setup") {
        std::process::exit(herdr_projects::verification::setup_main());
    }
}

const SEED: u64 = 7;

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "factory")
        .env("GIT_AUTHOR_EMAIL", "factory@example.invalid")
        .env("GIT_AUTHOR_DATE", "2020-01-01T00:00:00Z")
        .env("GIT_COMMITTER_NAME", "factory")
        .env("GIT_COMMITTER_EMAIL", "factory@example.invalid")
        .env("GIT_COMMITTER_DATE", "2020-01-01T00:00:00Z")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} status {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn build_repo(root: &Path, seed: u64) -> (PathBuf, String) {
    let repo = root.join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "factory", "--template="]);
    fs::write(
        repo.join("README"),
        format!("factory-fixture\nseed={seed}\n"),
    )
    .unwrap();
    git(&repo, &["add", "--", "README"]);
    git(
        &repo,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "fixture",
        ],
    );
    let commit = git(&repo, &["rev-parse", "HEAD"]).trim().to_string();
    assert_eq!(commit.len(), 40, "{commit}");
    (repo, commit)
}

fn workspace_git_sha() -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(env!("CARGO_MANIFEST_DIR"))
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}


#[cfg(target_os = "linux")]
#[test]
fn vertical_slice() {
    slice::run();
}

/// Disposable A/B/C/D slice. `admit_once` is only the flag-off and
/// `integration_missing` cases. The success wake is the in-process
/// `poll_queued_effects` call in the binary test.
#[cfg(target_os = "linux")]
mod slice {
    use super::git;
    use herdr_projects::{
        admission,
        domain::*,
        integration,
        migration::ConfigReference,
        reconcile::RuntimeObservation,
        verification::{self, VerifyRequest},
    };
    use sha2::{Digest, Sha256};
    use std::{fs, os::unix::fs::MetadataExt, path::Path, time::Duration};

    const PROSE: &str = "worker prose says the dependency is satisfied";
    const TARGET: &str = "refs/heads/integration";
    const STATE_BASE: &str = "fn=add\nkeep-1\nkeep-2\nkeep-3\nkeep-4\nguard=ok\n";
    const STATE_GUARD: &str = "fn=add\nkeep-1\nkeep-2\nkeep-3\nkeep-4\nguard=no\n";
    const STATE_FN: &str = "fn=mul\nkeep-1\nkeep-2\nkeep-3\nkeep-4\nguard=ok\n";

    fn oid_of(repo: &Path, spec: &str) -> String {
        git(repo, &["rev-parse", spec]).trim().to_string()
    }

    fn policy(checks: &[&str]) -> String {
        serde_json::json!({"version": 1, "checks": checks}).to_string()
    }

    fn reachable_objects(repo: &Path, specs: &[&str]) -> Vec<(String, String)> {
        let mut args = vec!["rev-list", "--objects"];
        args.extend_from_slice(specs);
        let mut objects = Vec::new();
        for line in git(repo, &args).lines() {
            let oid = line.split_whitespace().next().unwrap().to_string();
            if objects.iter().any(|(seen, _)| seen == &oid) {
                continue;
            }
            let relative = format!("{}/{}", &oid[..2], &oid[2..]);
            assert!(
                repo.join(".git/objects").join(&relative).is_file(),
                "loose object {oid} missing"
            );
            objects.push((oid, relative));
        }
        assert!(
            objects.len() <= 64,
            "submission object cap {}",
            objects.len()
        );
        objects
    }

    fn install_contract(
        db_path: &Path,
        task: &str,
        project_store: &str,
        repository: &str,
        base: &str,
        route: &str,
        body: &str,
    ) -> String {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        let predecessors: &[(&str, &str)] = match task {
            "c" => &[("a", "verified_result")],
            "b" => &[("a", "integrated_commit")],
            "d" | "dstale" => &[("a", "integrated_commit"), ("p", "integrated_commit")],
            _ => &[],
        };
        let dependencies = predecessors.iter().map(|(pred, edge)| {
            let policy: String = conn.query_row("SELECT body FROM acceptance_policies WHERE task_id=?1 AND contract_revision=1 AND policy_id='builds'", [pred], |r| r.get(0)).unwrap();
            serde_json::json!({"predecessor":pred,"edge":edge,"policy_id":"builds","policy_digest":format!("{:x}",Sha256::digest(policy.as_bytes()))})
        }).collect::<Vec<_>>();
        let raw = serde_json::json!({
            "version": 1,
            "project_store": project_store,
            "expected_head": 0,
            "deliverable": "fixture task",
            "non_goals": "no provider calls",
            "dependencies": dependencies,
            "capability_flags": [],
            "profile_kind": "claude",
            "retry_class": "none",
            "result_schema_id": "result-v1",
            "authority": {"id":"fixture-owner","revision":1,"digest":"ab".repeat(32)},
            "task_id": task,
            "contract_revision": 1,
            "repository": repository,
            "base_oid": base,
            "object_format": "sha1",
            "route": route,
            "acceptance_policies": [{"id": "builds", "text": body}]
        })
        .to_string();
        let digest = format!("{:x}", Sha256::digest(raw.as_bytes()));
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        let sequence: i64 = conn
            .query_row("SELECT MAX(sequence) FROM events", [], |row| row.get(0))
            .unwrap();
        conn.execute(
            "INSERT INTO task_contracts(task_id,contract_revision,plan_revision,project_store,expected_head,repository,base_oid,object_format,memory_snapshot_id,route,raw_bytes,raw_digest,installed_seq) VALUES(?1,1,NULL,?2,0,?3,?4,'sha1',NULL,?5,?6,?7,?8)",
            rusqlite::params![task, project_store, repository, base, route, raw.as_bytes(), digest, sequence],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO acceptance_policies(task_id,contract_revision,policy_id,body) VALUES(?1,1,'builds',?2)",
            rusqlite::params![task, body],
        )
        .unwrap();
        digest
    }

    fn satisfaction_text(db_path: &Path) -> String {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        let mut stmt = conn
            .prepare("SELECT satisfaction_id, task_id, predecessor_task, requirement, state, evidence_kind, evidence_id FROM dependency_satisfactions ORDER BY task_id, predecessor_task")
            .unwrap();
        let rows: Vec<String> = stmt
            .query_map([], |row| {
                Ok(format!(
                    "{} {} {} {} {} {} {}",
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?
                ))
            })
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        rows.join("\n")
    }

    fn count(db_path: &Path, sql: &str) -> i64 {
        rusqlite::Connection::open(db_path)
            .unwrap()
            .query_row(sql, [], |row| row.get(0))
            .unwrap()
    }

    fn attempts_for(db_path: &Path, task: &str) -> i64 {
        rusqlite::Connection::open(db_path)
            .unwrap()
            .query_row(
                "SELECT count(*) FROM attempts WHERE task_id=?1",
                [task],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn submit_and_verify(
        db_path: &Path,
        repo: &Path,
        work: &Path,
        task: &str,
        attempt: &str,
        base: &str,
        candidate: &str,
        digest: &str,
        body: &str,
        key: &str,
    ) -> String {
        let objects = reachable_objects(repo, &[base, candidate]);
        assert!(objects.iter().any(|(oid, _)| oid == base));
        assert!(objects.iter().any(|(oid, _)| oid == candidate));
        let submission = serde_json::json!({
            "idempotency_key": format!("submit-{key}"),
            "task_id": task,
            "contract_revision": 1,
            "contract_digest": digest,
            "attempt_id": attempt,
            "repository": repo,
            "base_oid": base,
            "candidate_oid": candidate,
            "object_format": "sha1",
            "artifact_manifest": [{"path": "src/fn.txt", "oid": candidate}],
            "claimed_checks": [PROSE],
            "objects": objects.iter().map(|(oid, relative)| serde_json::json!({"oid": oid, "relative_path": relative})).collect::<Vec<_>>()
        });
        let mut db = herdr_projects::store::SqliteStore::open(db_path).unwrap();
        let before = count(db_path, "SELECT count(*) FROM dependency_satisfactions");
        db.submit_result(&serde_json::to_vec(&submission).unwrap())
            .unwrap();
        assert_eq!(
            count(db_path, "SELECT count(*) FROM dependency_satisfactions"),
            before,
            "worker prose created a satisfaction row"
        );
        assert!(
            !satisfaction_text(db_path).contains(PROSE),
            "satisfaction stored worker prose"
        );
        let policy_path = work.join(format!("policy-{key}.json"));
        fs::write(&policy_path, body).unwrap();
        let request = VerifyRequest::new(
            db.show_results(None)
                .unwrap()
                .into_iter()
                .find(|view| view.attempt_id == attempt)
                .unwrap()
                .submission_id,
            "builds",
            &policy_path,
            format!("verify-{key}"),
            Duration::from_secs(60),
            work,
        );
        let outcome = verification::verify(&mut db, &request).unwrap();
        assert_eq!(
            outcome.state, "accepted",
            "{key} {:?} stdout={} argv={:?}",
            outcome.reason, outcome.stdout, outcome.argv
        );
        let result = outcome
            .receipt
            .expect("verified receipt")
            .result_id()
            .to_string();
        assert!(
            !satisfaction_text(db_path).contains(PROSE),
            "verifier stored worker prose as satisfaction"
        );
        result
    }

    fn integrate(
        db_path: &Path,
        repo: &Path,
        work: &Path,
        result_id: &str,
        key: &str,
    ) -> integration::IntegrateOutcome {
        let mut db = herdr_projects::store::SqliteStore::open(db_path).unwrap();
        integration::integrate(
            &mut db,
            &integration::IntegrateRequest {
                result_id: result_id.to_string(),
                idempotency_key: key.to_string(),
                repository: repo.to_path_buf(),
                work_dir: work.to_path_buf(),
                fault: integration::Fault::None,
            },
        )
        .unwrap()
    }

    fn retain_profile(db_path: &Path) {
        let canonical = fs::canonicalize(db_path).unwrap();
        let metadata = fs::metadata(&canonical).unwrap();
        let evidence = VersionedReference {
            id: "test-only-evidence".into(),
            revision: 1,
            digest: "a".repeat(64),
        };
        let supported = CapabilityEvidence::Supported {
            evidence: evidence.clone(),
        };
        let profile = FrozenProfile {
            version: 1,
            name: "fixture".into(),
            kind: "claude".into(),
            definition_digest: "b".repeat(64),
            config: ConfigReference {
                path: "/no/such/admission-config.toml".into(),
                digest: None,
            },
            arguments_digest: "c".repeat(64),
            environment_names: vec![],
            execution_home: None,
            permission_policy: evidence.clone(),
            adapter: evidence.clone(),
            agent: ExecutableIdentity {
                path: "/usr/bin/git".into(),
                digest: "d".repeat(64),
                version: "1.0.0".into(),
            },
            herdr: ExecutableIdentity {
                path: "/usr/bin/git".into(),
                digest: "e".repeat(64),
                version: "0.9.1".into(),
            },
            capabilities: ProfileCapabilities {
                launch: supported.clone(),
                readiness_observation: supported.clone(),
                prompt_submission: supported.clone(),
                stop: supported,
                checkpoint_acknowledgment: CapabilityEvidence::Unknown,
                structured_usage: CapabilityEvidence::Unknown,
                resume: CapabilityEvidence::Unknown,
            },
            workflow_certificate: None,
        };
        let reference = profile.reference().unwrap();
        let report = serde_json::json!({
            "preparation": {
                "profile": profile,
                "reference": reference,
                "launchable": true,
                "protocol_capable": false,
                "certified": false
            },
            "source_store": [canonical, metadata.dev(), metadata.ino()]
        });
        let text = serde_json::to_string(&report).unwrap();
        let report_digest = format!("{:x}", Sha256::digest(text.as_bytes()));
        let conn = rusqlite::Connection::open(db_path).unwrap();
        let sequence: i64 = conn
            .query_row("SELECT MAX(sequence) FROM events", [], |row| row.get(0))
            .unwrap();
        conn.execute(
            "INSERT INTO native_profiles(profile_digest,report,report_digest,sequence) VALUES(?1,?2,?3,?4)",
            rusqlite::params![reference.digest, text, report_digest, sequence],
        )
        .unwrap();
    }

    fn install_grant(db_path: &Path, project: &Path) -> String {
        super::worker_snapshots(project);
        let inputs = admission::prepared_admission_inputs(project)
            .unwrap()
            .unwrap_or_else(|| panic!("no ready candidate\n{}", satisfaction_text(db_path)));
        let now = jiff::Timestamp::now().as_millisecond();
        let grant = ApprovalGrant {
            version: 1,
            scope: ApprovalScope::for_launch(&inputs).unwrap(),
            policy: inputs
                .effective_profile
                .as_ref()
                .unwrap()
                .permission_policy
                .clone(),
            issued_unix_ms: 0,
            expires_unix_ms: now + 3_600_000,
        };
        let approval = grant.reference().unwrap();
        let payload = String::from_utf8(serde_json::to_vec(&grant).unwrap()).unwrap();
        rusqlite::Connection::open(db_path)
            .unwrap()
            .execute(
                "INSERT INTO approval_grants(id,payload,payload_hash) VALUES(?1,?2,?3)",
                rusqlite::params![approval.id, payload, approval.digest],
            )
            .unwrap();
        inputs.task.as_str().to_string()
    }

    fn enable_admission(db_path: &Path) {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        let off: String = conn
            .query_row(
                "SELECT factory_admission FROM project_control WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            off, "off",
            "migration default must stay off until the fixture UPDATE"
        );
        let updated = conn
            .execute(
                "UPDATE project_control SET factory_admission='on' WHERE singleton=1",
                [],
            )
            .unwrap();
        assert_eq!(updated, 1);
    }

    pub fn run() {
        let (root, project, db_path) = through_d();
        let mut db = herdr_projects::store::SqliteStore::open(&db_path).unwrap();
        let snapshot = db.read_snapshot(None).unwrap();
        let revision = snapshot
            .tasks
            .iter()
            .find(|task| task.id.as_str() == "dstale")
            .unwrap()
            .revision;
        db.queue_task(
            &TaskId::new("dstale").unwrap(),
            revision,
            snapshot.head,
            &QueueRequest {
                priority: 20,
                dependencies: vec![
                    Dependency {
                        predecessor: TaskId::new("a").unwrap(),
                        requirement: DependencyRequirement::IntegratedCommit,
                    },
                    Dependency {
                        predecessor: TaskId::new("p").unwrap(),
                        requirement: DependencyRequirement::IntegratedCommit,
                    },
                ],
            },
            jiff::Timestamp::now().as_millisecond(),
        )
        .unwrap();
        drop(db);
        enable_admission(&db_path);
        assert!(admission::wake_enabled(&project));
        assert_eq!(install_grant(&db_path, &project), "dstale");
        let stale = admission::admit_once(&project).unwrap_err();
        assert!(
            stale.to_string().contains("integration_missing"),
            "{stale:#}"
        );
        assert_eq!(attempts_for(&db_path, "dstale"), 0);
        assert_eq!(attempts_for(&db_path, "b"), 0);
        assert_eq!(attempts_for(&db_path, "c"), 0);
        assert_eq!(attempts_for(&db_path, "d"), 0);

        let manifest = serde_json::json!({
            "vertical_slice": "pass",
            "git_sha": super::workspace_git_sha(),
            "combined_tree": "checks_failed",
            "restarted_after_cas": true,
            "worker_prose_satisfaction_rows": 0
        });
        let path = root.path().join("vertical-slice-manifest.json");
        let text = serde_json::to_vec_pretty(&manifest).unwrap();
        fs::write(&path, &text).unwrap();
        let read: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(read["vertical_slice"], "pass");
        assert_eq!(read["git_sha"], super::workspace_git_sha());
        assert!(read["git_sha"].as_str().unwrap().len() >= 40);
    }

    /// `d` after `through_d`: 3 exact + 1 uncertain write paths (plus a read
    /// path), 2 dependencies, 1 repository, `verify_then_integrate`.
    pub fn classify_first_attempt() {
        let (_root, project, db_path) = through_d();
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        for (ordinal, path, access, certainty) in [(0, "src/fn.txt", "write", "exact"), (1, "src/peer.txt", "write", "exact"),
            (2, "src/state.txt", "write", "exact"), (3, "src/gen/", "write", "uncertain"), (4, "README", "read", "exact")] {
            conn.execute("INSERT INTO contract_scope_paths(task_id,contract_revision,ordinal,path,access,certainty) VALUES('d',1,?1,?2,?3,?4)",
                rusqlite::params![ordinal, path, access, certainty]).unwrap();
        }
        drop(conn);
        enable_admission(&db_path);
        assert_eq!(install_grant(&db_path, &project), "d");
        assert_eq!(count(&db_path, "SELECT count(*) FROM task_classifications"), 0, "a draft wrote a classification");
        let before = jiff::Timestamp::now().as_millisecond();
        admission::admit_once(&project).unwrap();
        let after = jiff::Timestamp::now().as_millisecond();
        assert_eq!(attempts_for(&db_path, "d"), 1);
        let rows = super::classifications(&db_path);
        assert_eq!(rows.len(), 1, "{rows:?}");
        let (id, task, revision, class, band, features, created) = rows[0].clone();
        assert_eq!((task.as_str(), revision, class.as_str(), band.as_str()), ("d", Some(1), "code", "large"));
        assert_eq!(features, r#"{"dependencies":2,"repositories":1,"route":"verify_then_integrate","uncertain_write_paths":1,"write_named_resources":0,"write_paths":4}"#);
        assert!((before..=after).contains(&created), "{created} outside {before}..={after}");
        let record = format!(r#"{{"band":"large","class":"code","classifier":"rule:task-taxonomy.v1","contract_revision":1,"created_unix_ms":{created},"features":{features},"reason":null,"revision":1,"task_id":"d","taxonomy":"task-taxonomy.v1"}}"#);
        assert_eq!(id, format!("sha256:{:x}", Sha256::digest(record.as_bytes())));
    }

    /// Shared prefix: a and p verified and integrated, `d` queued behind both
    /// (route `verify_then_integrate`) and not yet reserved.
    pub fn through_d() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let repo = root.path().join("repo");
        let work = root.path().join("work");
        fs::create_dir_all(project.join(".state")).unwrap();
        fs::create_dir_all(&work).unwrap();
        fs::create_dir_all(&repo).unwrap();
        let db_path = project.join(".state/state.db");

        git(&repo, &["init", "-q", "-b", "factory", "--template="]);
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::write(repo.join("src/fn.txt"), "fn=add\n").unwrap();
        fs::write(repo.join("src/peer.txt"), "peer=1\n").unwrap();
        fs::write(repo.join("src/state.txt"), STATE_BASE).unwrap();
        git(&repo, &["add", "--", "src"]);
        git(
            &repo,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "base"],
        );
        git(&repo, &["branch", "integration"]);
        let base = oid_of(&repo, "HEAD");

        git(&repo, &["checkout", "-q", "-b", "worker-a"]);
        fs::write(repo.join("src/fn.txt"), "fn=mul\n").unwrap();
        fs::write(repo.join("WORKER.md"), format!("{PROSE}\n")).unwrap();
        git(&repo, &["add", "--", "src/fn.txt", "WORKER.md"]);
        git(
            &repo,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "a"],
        );
        let commit_a = oid_of(&repo, "HEAD");
        git(&repo, &["checkout", "-q", "factory"]);

        git(&repo, &["checkout", "-q", "-b", "worker-p"]);
        fs::write(repo.join("src/peer.txt"), "peer=2\n").unwrap();
        fs::write(repo.join("src/state.txt"), STATE_GUARD).unwrap();
        git(&repo, &["add", "--", "src/peer.txt", "src/state.txt"]);
        git(
            &repo,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "p"],
        );
        let commit_p = oid_of(&repo, "HEAD");
        git(&repo, &["checkout", "-q", "factory"]);

        git(&repo, &["checkout", "-q", "-b", "worker-s"]);
        fs::write(repo.join("src/state.txt"), STATE_FN).unwrap();
        git(&repo, &["add", "--", "src/state.txt"]);
        git(
            &repo,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "s"],
        );
        let commit_s = oid_of(&repo, "HEAD");
        git(&repo, &["checkout", "-q", "factory"]);
        assert_eq!(oid_of(&repo, TARGET), base);
        assert_eq!(
            git(&repo, &["symbolic-ref", "HEAD"]).trim(),
            "refs/heads/factory"
        );
        assert!(git(&repo, &["show", &format!("{commit_a}:WORKER.md")]).contains(PROSE));

        let repo = repo.canonicalize().unwrap();
        fs::create_dir_all(project.join(".state")).unwrap();
        let db = herdr_projects::store::SqliteStore::create(&db_path).unwrap();
        drop(db);
        let db_path = db_path.canonicalize().unwrap();
        let project_store = db_path.to_str().unwrap().to_string();
        let repository = repo.to_str().unwrap().to_string();
        let mut db = herdr_projects::store::SqliteStore::open(&db_path).unwrap();
        let producers = ["a", "p", "s"];
        let mut mutations: Vec<Mutation> = ["a", "p", "s", "b", "c", "d", "w", "dstale"]
            .into_iter()
            .map(|id| Mutation::Task {
                expected: None,
                next: Task {
                    id: TaskId::new(id).unwrap(),
                    revision: 1,
                    state: TaskState::Draft,
                    title: id.into(),
                    active_attempt: None,
                },
            })
            .collect();
        for id in producers {
            mutations.push(Mutation::Attempt {
                expected: None,
                next: Attempt {
                    id: AttemptId::new(format!("attempt-{id}")).unwrap(),
                    task: TaskId::new(id).unwrap(),
                    revision: 1,
                    state: AttemptState::Completed,
                    snapshot: None,
                    reservation: format!("slot-{id}"),
                    termination_observed: true,
                },
            });
        }
        db.commit(Commit {
            expected_head: 0,
            mutations,
        })
        .unwrap();
        drop(db);
        let admission_off: String = rusqlite::Connection::open(&db_path)
            .unwrap()
            .query_row(
                "SELECT factory_admission FROM project_control WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(admission_off, "off");

        let policy_a = policy(&["/usr/bin/git", "grep", "-q", "^fn=mul$", "--", "src/fn.txt"]);
        let policy_p = policy(&[
            "/usr/bin/git",
            "grep",
            "-q",
            "^peer=2$",
            "--",
            "src/peer.txt",
        ]);
        let policy_s = policy(&[
            "/usr/bin/git",
            "grep",
            "--all-match",
            "-q",
            "-e",
            "^fn=mul$",
            "-e",
            "^guard=ok$",
            "--",
            "src/state.txt",
        ]);
        let policy_consumer = policy(&["/usr/bin/git", "rev-parse", "HEAD"]);
        let digest_a = install_contract(
            &db_path,
            "a",
            &project_store,
            &repository,
            &base,
            "verify_then_integrate",
            &policy_a,
        );
        let digest_p = install_contract(
            &db_path,
            "p",
            &project_store,
            &repository,
            &base,
            "verify_then_integrate",
            &policy_p,
        );
        let digest_s = install_contract(
            &db_path,
            "s",
            &project_store,
            &repository,
            &base,
            "verify_then_integrate",
            &policy_s,
        );
        for task in ["b", "c", "dstale"] {
            install_contract(
                &db_path,
                task,
                &project_store,
                &repository,
                &base,
                "verify_only",
                &policy_consumer,
            );
        }
        let mut db = herdr_projects::store::SqliteStore::open(&db_path).unwrap();
        integration::configure_integration_ref(&mut db, &repo, TARGET).unwrap();
        drop(db);

        let result_a = submit_and_verify(
            &db_path,
            &repo,
            &work,
            "a",
            "attempt-a",
            &base,
            &commit_a,
            &digest_a,
            &policy_a,
            "a",
        );
        assert_eq!(
            count(&db_path, "SELECT count(*) FROM dependency_satisfactions"),
            0
        );
        let result_p = submit_and_verify(
            &db_path,
            &repo,
            &work,
            "p",
            "attempt-p",
            &base,
            &commit_p,
            &digest_p,
            &policy_p,
            "p",
        );
        let result_s = submit_and_verify(
            &db_path,
            &repo,
            &work,
            "s",
            "attempt-s",
            &base,
            &commit_s,
            &digest_s,
            &policy_s,
            "s",
        );
        assert_eq!(
            count(&db_path, "SELECT count(*) FROM dependency_satisfactions"),
            0
        );

        let mut db = herdr_projects::store::SqliteStore::open(&db_path).unwrap();
        for id in ["b", "c", "d", "dstale"] {
            let snapshot = db.read_snapshot(None).unwrap();
            let revision = snapshot
                .tasks
                .iter()
                .find(|task| task.id.as_str() == id)
                .unwrap()
                .revision;
            db.create_runtime(
                Some(&TaskId::new(id).unwrap()),
                Some(revision),
                snapshot.head,
                &RuntimeRoute::default(),
            )
            .unwrap();
        }
        let snapshot = db.read_snapshot(None).unwrap();
        let now = jiff::Timestamp::now().as_millisecond();
        let observations: Vec<RuntimeObservation> = snapshot
            .runtime_bindings
            .iter()
            .map(|binding| RuntimeObservation {
                binding: binding.id.clone(),
                binding_revision: binding.revision,
                task_revision: binding.task.as_ref().map(|task| {
                    snapshot
                        .tasks
                        .iter()
                        .find(|item| item.id == *task)
                        .unwrap()
                        .revision
                }),
                observed_unix_ms: now,
                collector: "herdr-git-v1".into(),
                ..RuntimeObservation::default()
            })
            .collect();
        let head = db
            .record_observations(snapshot.head, &observations)
            .unwrap();
        let snapshot = db.read_snapshot(None).unwrap();
        db.set_project_state(
            head,
            snapshot.control.as_ref().unwrap().revision,
            ProjectState::Active,
            now,
            None,
        )
        .unwrap();
        let snapshot = db.read_snapshot(None).unwrap();
        db.set_scheduler_policy(
            snapshot.head,
            snapshot.scheduler.as_ref().unwrap().policy.revision,
            4,
            3,
        )
        .unwrap();
        drop(db);
        retain_profile(&db_path);

        let mut db = herdr_projects::store::SqliteStore::open(&db_path).unwrap();
        let snapshot = db.read_snapshot(None).unwrap();
        let revision = snapshot
            .tasks
            .iter()
            .find(|task| task.id.as_str() == "c")
            .unwrap()
            .revision;
        db.queue_task(
            &TaskId::new("c").unwrap(),
            revision,
            snapshot.head,
            &QueueRequest {
                priority: 0,
                dependencies: vec![Dependency {
                    predecessor: TaskId::new("a").unwrap(),
                    requirement: DependencyRequirement::VerifiedResult,
                }],
            },
            jiff::Timestamp::now().as_millisecond(),
        )
        .unwrap();
        drop(db);
        let rows = satisfaction_text(&db_path);
        assert!(rows.contains(&result_a), "{rows}");
        assert!(
            rows.contains("c a verified_result valid verified_result"),
            "{rows}"
        );
        assert!(!rows.contains(PROSE), "{rows}");
        assert!(!rows.contains("integrated_commit"), "{rows}");
        assert_eq!(attempts_for(&db_path, "c"), 0);
        admission::admit_once(&project).unwrap();
        assert_eq!(attempts_for(&db_path, "c"), 0, "flag off must not reserve");
        assert!(!admission::wake_enabled(&project));

        rusqlite::Connection::open(&db_path)
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER vertical_slice_crash_receipt BEFORE INSERT ON integrated_commits BEGIN SELECT RAISE(ABORT, 'vertical-slice-crash'); END;",
            )
            .unwrap();
        let mut db = herdr_projects::store::SqliteStore::open(&db_path).unwrap();
        let crashed = integration::integrate(
            &mut db,
            &integration::IntegrateRequest {
                result_id: result_a.clone(),
                idempotency_key: "integrate-a".into(),
                repository: repo.clone(),
                work_dir: work.clone(),
                fault: integration::Fault::None,
            },
        )
        .unwrap_err();
        drop(db);
        let moved = oid_of(&repo, TARGET);
        assert_ne!(
            moved, base,
            "CAS did not publish before the crash: {crashed:#}"
        );
        assert_eq!(
            count(&db_path, "SELECT count(*) FROM integrated_commits"),
            0,
            "{crashed:#}"
        );
        assert_eq!(
            count(
                &db_path,
                "SELECT count(*) FROM dependency_satisfactions WHERE evidence_kind='integrated_commit'"
            ),
            0
        );
        rusqlite::Connection::open(&db_path)
            .unwrap()
            .execute_batch("DROP TRIGGER vertical_slice_crash_receipt;")
            .unwrap();
        let mut db = herdr_projects::store::SqliteStore::open(&db_path).unwrap();
        let confirmed = integration::reconcile_integration(&mut db, &repo, "integrate-a").unwrap();
        drop(db);
        assert_eq!(confirmed.state, "integrated", "{:?}", confirmed.reason);
        assert_eq!(confirmed.commit_oid.as_deref(), Some(moved.as_str()));
        assert_eq!(oid_of(&repo, TARGET), moved);
        assert_eq!(
            count(&db_path, "SELECT count(*) FROM integrated_commits"),
            1
        );

        let mut db = herdr_projects::store::SqliteStore::open(&db_path).unwrap();
        let snapshot = db.read_snapshot(None).unwrap();
        let revision = snapshot
            .tasks
            .iter()
            .find(|task| task.id.as_str() == "b")
            .unwrap()
            .revision;
        db.queue_task(
            &TaskId::new("b").unwrap(),
            revision,
            snapshot.head,
            &QueueRequest {
                priority: 0,
                dependencies: vec![Dependency {
                    predecessor: TaskId::new("a").unwrap(),
                    requirement: DependencyRequirement::IntegratedCommit,
                }],
            },
            jiff::Timestamp::now().as_millisecond(),
        )
        .unwrap();
        drop(db);
        assert!(
            satisfaction_text(&db_path).contains("b a integrated_commit valid integrated_commit")
        );
        assert_eq!(attempts_for(&db_path, "b"), 0);

        let integrated_p = integrate(&db_path, &repo, &work, &result_p, "integrate-p");
        assert_eq!(
            integrated_p.state, "integrated",
            "{:?}",
            integrated_p.reason
        );
        let tip = oid_of(&repo, TARGET);
        assert_eq!(integrated_p.commit_oid.as_deref(), Some(tip.as_str()));
        assert_ne!(tip, moved);

        let mut db = herdr_projects::store::SqliteStore::open(&db_path).unwrap();
        let snapshot = db.read_snapshot(None).unwrap();
        let revision = snapshot
            .tasks
            .iter()
            .find(|task| task.id.as_str() == "w")
            .unwrap()
            .revision;
        db.queue_task(
            &TaskId::new("w").unwrap(),
            revision,
            snapshot.head,
            &QueueRequest {
                priority: 0,
                dependencies: vec![Dependency {
                    predecessor: TaskId::new("s").unwrap(),
                    requirement: DependencyRequirement::IntegratedCommit,
                }],
            },
            jiff::Timestamp::now().as_millisecond(),
        )
        .unwrap();
        drop(db);
        let broken = integrate(&db_path, &repo, &work, &result_s, "integrate-s");
        assert_eq!(broken.state, "blocked", "{:?}", broken.reason);
        assert_eq!(broken.reason.as_deref(), Some("checks_failed"));
        assert_eq!(
            oid_of(&repo, TARGET),
            tip,
            "failed combined tree must not move the ref"
        );
        assert_eq!(
            count(
                &db_path,
                "SELECT count(*) FROM dependency_satisfactions WHERE task_id='w'"
            ),
            0,
            "combined-tree failure created a satisfaction row"
        );
        assert!(!satisfaction_text(&db_path).contains(PROSE));

        install_contract(
            &db_path,
            "d",
            &project_store,
            &repository,
            &tip,
            "verify_then_integrate",
            &policy_consumer,
        );
        let mut db = herdr_projects::store::SqliteStore::open(&db_path).unwrap();
        let snapshot = db.read_snapshot(None).unwrap();
        let revision = snapshot
            .tasks
            .iter()
            .find(|task| task.id.as_str() == "d")
            .unwrap()
            .revision;
        db.queue_task(
            &TaskId::new("d").unwrap(),
            revision,
            snapshot.head,
            &QueueRequest {
                priority: 1,
                dependencies: vec![
                    Dependency {
                        predecessor: TaskId::new("a").unwrap(),
                        requirement: DependencyRequirement::IntegratedCommit,
                    },
                    Dependency {
                        predecessor: TaskId::new("p").unwrap(),
                        requirement: DependencyRequirement::IntegratedCommit,
                    },
                ],
            },
            jiff::Timestamp::now().as_millisecond(),
        )
        .unwrap();
        drop(db);
        let rows = satisfaction_text(&db_path);
        assert!(rows.contains("d a integrated_commit"), "{rows}");
        assert!(rows.contains("d p integrated_commit"), "{rows}");
        assert_eq!(attempts_for(&db_path, "d"), 0);
        (root, project, db_path)
    }
}

#[cfg(target_os = "linux")]
#[test]
fn classification_is_written_before_first_attempt() {
    slice::classify_first_attempt();
}

/// `(classification_id, task_id, contract_revision, class, band, features, created_unix_ms)`
/// for every revision-1 `task-taxonomy.v1` row.
#[cfg(target_os = "linux")]
type ClassificationRow = (String, String, Option<i64>, String, String, String, i64);
#[cfg(target_os = "linux")]
fn classifications(db_path: &Path) -> Vec<ClassificationRow> {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    let mut stmt = conn.prepare("SELECT classification_id,task_id,contract_revision,class,band,features,created_unix_ms FROM task_classifications WHERE taxonomy='task-taxonomy.v1' AND classifier='rule:task-taxonomy.v1' AND revision=1 AND reason IS NULL ORDER BY task_id").unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))).unwrap().map(Result::unwrap).collect()
}

/// One queued task on an active project with a retained `sim` profile and
/// automatic admission on. `contract_path` installs the one-path fixture contract.
#[cfg(target_os = "linux")]
fn classification_project(tmp: &Path, task: &str, contract_path: Option<&str>) -> PathBuf {
    let project = tmp.join("project");
    fs::create_dir_all(project.join(".state")).unwrap();
    let db_path = project.join(".state/state.db");
    let (repo, oid) = build_repo(tmp, SEED);
    let repository = fs::canonicalize(&repo).unwrap().display().to_string();
    let config_path = tmp.join("owner.toml");
    fs::write(&config_path, "version = 1\n").unwrap();
    let config = herdr_projects::migration::config_reference(&config_path).unwrap();
    let digest = config.digest.clone().unwrap();
    let mut db = SqliteStore::create(&db_path).unwrap();
    let id = TaskId::new(task).unwrap();
    db.commit(Commit { expected_head: 0, mutations: vec![Mutation::Task { expected: None,
        next: Task { id: id.clone(), revision: 1, state: TaskState::Draft, title: task.into(), active_attempt: None } }] }).unwrap();
    let head = db.current_head().unwrap();
    db.create_runtime(Some(&id), Some(1), head, &RuntimeRoute::default()).unwrap();
    let revision = db.read_snapshot(None).unwrap().tasks[0].revision;
    db.queue_task(&id, revision, db.current_head().unwrap(), &QueueRequest { priority: 0, dependencies: Vec::new() }, unix_ms() - 1_000).unwrap();
    let snapshot = db.read_snapshot(None).unwrap();
    db.set_scheduler_policy(snapshot.head, snapshot.scheduler.unwrap().policy.revision, 1, 2).unwrap();
    let now = unix_ms();
    let snapshot = db.read_snapshot(None).unwrap();
    let binding = &snapshot.runtime_bindings[0];
    db.record_observations(snapshot.head, &[herdr_projects::reconcile::RuntimeObservation { binding: binding.id.clone(), binding_revision: binding.revision,
        task_revision: Some(snapshot.tasks[0].revision), observed_unix_ms: now, collector: "herdr-git-v1".into(), config_digest: Some(digest.clone()),
        ..herdr_projects::reconcile::RuntimeObservation::default() }]).unwrap();
    let snapshot = db.read_snapshot(None).unwrap();
    db.set_project_state(snapshot.head, snapshot.control.unwrap().revision, ProjectState::Active, now, Some(&digest)).unwrap();
    drop(db);
    if let Some(path) = contract_path {
        let object_format = if oid.len() == 40 { "sha1" } else { "sha256" };
        install_fixture_contract(&db_path, task, &repository, &oid, object_format, path, &["discovered", "launchable"]);
    }
    plant_profile(&db_path, &config);
    SqliteStore::open(&db_path).unwrap().record_native_capability_evidence(unix_ms(), unix_ms() + 3_600_000).unwrap();
    // Fixture only. Production code has no writer for this column.
    rusqlite::Connection::open(&db_path).unwrap().execute("UPDATE project_control SET factory_admission='on' WHERE singleton=1", []).unwrap();
    project
}

/// Automatic admission of the one ready task, with its grant.
#[cfg(target_os = "linux")]
fn admit_ready(project: &Path) {
    let db_path = project.join(".state/state.db");
    worker_snapshots(project);
    let inputs = herdr_projects::admission::prepared_admission_inputs(project).unwrap().expect("a ready candidate");
    insert_grant(&db_path, &inputs);
    let before = sql_count(&db_path, "SELECT count(*) FROM attempts");
    herdr_projects::admission::admit_once(project).unwrap();
    assert_eq!(sql_count(&db_path, "SELECT count(*) FROM attempts"), before + 1, "admission did not reserve");
}

#[cfg(target_os = "linux")]
#[test]
fn second_attempt_reuses_classification() {
    let tmp = tempfile::tempdir().unwrap();
    let project = classification_project(tmp.path(), "retry", Some("src/retry.rs"));
    let db_path = project.join(".state/state.db");
    admit_ready(&project);
    let first = classifications(&db_path);
    assert_eq!(first.len(), 1, "{first:?}");
    assert_eq!((first[0].3.as_str(), first[0].4.as_str()), ("code", "small"));
    assert_eq!(first[0].5, r#"{"dependencies":0,"repositories":1,"route":"verify_only","uncertain_write_paths":0,"write_named_resources":0,"write_paths":1}"#);

    requeue(&db_path);
    admit_ready(&project);
    assert_eq!(sql_count(&db_path, "SELECT count(*) FROM attempts WHERE task_id='retry'"), 2);
    assert_eq!(sql_count(&db_path, "SELECT count(*) FROM task_classifications"), 1);
    assert_eq!(classifications(&db_path), first);
}

/// Cancel the one task's reserved attempt before any launch claim and queue the task again.
#[cfg(target_os = "linux")]
fn requeue(db_path: &Path) {
    let mut db = SqliteStore::open(db_path).unwrap();
    let attempt: String = rusqlite::Connection::open(db_path).unwrap().query_row("SELECT id FROM attempts WHERE state='reserved'", [], |r| r.get(0)).unwrap();
    let cancelled = db.cancel_attempt(&AttemptId::new(attempt).unwrap(), 1, db.current_head().unwrap(), "retry the task", unix_ms()).unwrap();
    assert!(cancelled.released);
    // No production path re-opens a cancelled task; the fixture blocks it as a failed attempt would.
    let snapshot = db.read_snapshot(None).unwrap();
    let mut task = snapshot.tasks[0].clone();
    assert_eq!(task.state, TaskState::Cancelled);
    let expected = task.revision;
    task.revision += 1;task.state = TaskState::Blocked;
    db.commit(Commit { expected_head: snapshot.head, mutations: vec![Mutation::Task { expected: Some(expected), next: task.clone() }] }).unwrap();
    db.queue_task(&task.id, task.revision, db.current_head().unwrap(), &QueueRequest { priority: 0, dependencies: Vec::new() }, unix_ms()).unwrap();
}

/// Automatic admission where only `granted` holds a launch approval. Knowledge
/// is retained for `granted` first so the prepared inputs are its own.
#[cfg(target_os = "linux")]
fn admit_granted(project: &Path, granted: &str) {
    let db_path = project.join(".state/state.db");
    worker_snapshots_for(project, Some(granted));
    let inputs = herdr_projects::admission::prepared_admission_inputs(project).unwrap().expect("a ready candidate");
    assert_eq!(inputs.effective_profile.as_ref().unwrap().name, granted);
    insert_grant(&db_path, &inputs);
    worker_snapshots_for(project, None);
    let before = sql_count(&db_path, "SELECT count(*) FROM attempts");
    herdr_projects::admission::admit_once(project).unwrap();
    assert_eq!(sql_count(&db_path, "SELECT count(*) FROM attempts"), before + 1, "admission did not reserve");
}

#[cfg(target_os = "linux")]
/// A further retained profile with the capability evidence the fixture contract requires.
fn plant_arm(db_path: &Path, config: &herdr_projects::migration::ConfigReference, name: &str, version: &str) -> FrozenProfile {
    let profile = plant_profile_as(db_path, sim_profile(config, name, version));
    SqliteStore::open(db_path).unwrap().record_native_capability_evidence(unix_ms(), unix_ms() + 3_600_000).unwrap();
    profile
}

#[cfg(target_os = "linux")]
/// Contracts §2 bytes of a `sim_profile` fixture, written out by hand.
fn sim_configuration(version: &str) -> String {
    format!(concat!(r#"{{"adapter":{{"digest":"{a}","id":"sim-evidence","revision":1}},"agent_digest":"{e}","agent_version":"{version}","#,
        r#""arguments_digest":"{c}","definition_digest":"{b}","environment_names":[],"kind":"codex","#,
        r#""permission_policy":{{"digest":"{d}","id":"sim-policy","revision":1}},"reasoning_effort":null,"reasoning_effort_reason":"mapping_unverified","#,
        r#""requested_model":null,"requested_model_reason":"mapping_unverified","schema":"agent_configuration.v1"}}"#),
        a = "a".repeat(64), b = "b".repeat(64), c = "c".repeat(64), d = "d".repeat(64), e = "e".repeat(64), version = version)
}

#[cfg(target_os = "linux")]
fn sha256_id(bytes: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes.as_bytes()))
}

#[cfg(target_os = "linux")]
/// `(task_id, task_revision, contract_revision, classification_id, chosen_configuration_id, eligible,
/// chooser_kind, chooser_principal, reason_codes, note, policy, seed)` in insertion order.
type DecisionRow = (String, i64, Option<i64>, Option<String>, String, String, String, String, String, Option<String>, Option<String>, Option<String>);
#[cfg(target_os = "linux")]
fn decisions(db_path: &Path) -> Vec<DecisionRow> {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    let mut stmt = conn.prepare("SELECT task_id,task_revision,contract_revision,classification_id,chosen_configuration_id,eligible,chooser_kind,chooser_principal,reason_codes,note,policy,seed FROM dispatch_decisions ORDER BY rowid").unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?, r.get(11)?))).unwrap().map(Result::unwrap).collect()
}

#[cfg(target_os = "linux")]
fn configurations(db_path: &Path) -> Vec<(String, String)> {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    let mut stmt = conn.prepare("SELECT configuration_id,canonical_json FROM agent_configurations ORDER BY configuration_id").unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect()
}

#[cfg(target_os = "linux")]
#[test]
fn automatic_admission_logs_eligible_profiles() {
    let tmp = tempfile::tempdir().unwrap();
    let project = classification_project(tmp.path(), "pick", Some("src/pick.rs"));
    let db_path = project.join(".state/state.db");
    let config = herdr_projects::migration::config_reference(&tmp.path().join("owner.toml")).unwrap();
    let sim = sim_profile(&config, "sim", "1.0.0");
    let next = plant_arm(&db_path, &config, "sim-next", "0.154.1");
    // Admission evaluates retained profiles in profile-digest order; the second holds the only grant.
    let mut arms = [(sim.reference().unwrap().digest, "sim", sim_configuration("1.0.0")), (next.reference().unwrap().digest, "sim-next", sim_configuration("0.154.1"))];
    arms.sort();
    admit_granted(&project, arms[1].1);
    let eligible = format!(concat!(r#"[{{"configuration_id":"{}","probability_ppm":0,"profile_digest":"{}","status":"no_approval"}},"#,
        r#"{{"configuration_id":"{}","probability_ppm":1000000,"profile_digest":"{}","status":"chosen"}}]"#),
        sha256_id(&arms[0].2), arms[0].0, sha256_id(&arms[1].2), arms[1].0);
    let task_revision = sql_count(&db_path, "SELECT revision FROM tasks WHERE id='pick'");
    let classification = classifications(&db_path)[0].0.clone();
    assert_eq!(decisions(&db_path), [("pick".into(), task_revision, Some(1), Some(classification), sha256_id(&arms[1].2), eligible,
        "automatic_admission".into(), "rule:automatic-admission.v1".into(), r#"["first_matching_approval"]"#.into(), None, None, None)]);
    let mut expected = vec![(sha256_id(&arms[0].2), arms[0].2.clone()), (sha256_id(&arms[1].2), arms[1].2.clone())];
    expected.sort();
    assert_eq!(configurations(&db_path), expected);
}

#[cfg(target_os = "linux")]
#[test]
fn configuration_identity_is_stable_and_versioned() {
    let tmp = tempfile::tempdir().unwrap();
    let project = classification_project(tmp.path(), "arm", Some("src/arm.rs"));
    let db_path = project.join(".state/state.db");
    let mut db = SqliteStore::open(&db_path).unwrap();
    let snapshot = db.read_snapshot(None).unwrap();
    db.set_scheduler_policy(snapshot.head, snapshot.scheduler.unwrap().policy.revision, 1, 3).unwrap();
    drop(db);
    let current = sim_configuration("1.0.0");
    admit_ready(&project);
    requeue(&db_path);
    admit_ready(&project);
    // The same profile on two attempts is one arm, stored as the exact canonical bytes.
    assert_eq!(configurations(&db_path), [(sha256_id(&current), current.clone())]);
    requeue(&db_path);
    let config = herdr_projects::migration::config_reference(&tmp.path().join("owner.toml")).unwrap();
    plant_arm(&db_path, &config, "sim-next", "0.154.1");
    admit_granted(&project, "sim-next");
    let upgraded = sim_configuration("0.154.1");
    let mut expected = vec![(sha256_id(&current), current.clone()), (sha256_id(&upgraded), upgraded.clone())];
    expected.sort();
    assert_eq!(configurations(&db_path), expected, "an agent version change is a new arm");
    let chosen = decisions(&db_path).into_iter().map(|row| row.4).collect::<Vec<_>>();
    assert_eq!(chosen, [sha256_id(&current), sha256_id(&current), sha256_id(&upgraded)]);
    assert_eq!(sql_count(&db_path, "SELECT count(*) FROM attempts"), 3);
}

#[cfg(target_os = "linux")]
#[test]
fn schema_write_classifies_schema_change() {
    let tmp = tempfile::tempdir().unwrap();
    let project = classification_project(tmp.path(), "schema", Some("migrations/0001_init.sql"));
    let db_path = project.join(".state/state.db");
    rusqlite::Connection::open(&db_path).unwrap().execute_batch(
        "INSERT INTO contract_named_resources(task_id,contract_revision,name,access) VALUES('schema',1,'schema','write'),('schema',1,'lockfile','read');").unwrap();
    admit_ready(&project);
    let rows = classifications(&db_path);
    assert_eq!(rows.len(), 1, "{rows:?}");
    // min(1,8) + 3 x 1 named write = 4.
    assert_eq!((rows[0].1.as_str(), rows[0].2, rows[0].3.as_str(), rows[0].4.as_str()), ("schema", Some(1), "schema_change", "medium"));
    assert_eq!(rows[0].5, r#"{"dependencies":0,"repositories":1,"route":"verify_only","uncertain_write_paths":0,"write_named_resources":1,"write_paths":1}"#);
}

#[cfg(target_os = "linux")]
#[test]
fn task_without_contract_is_unscoped() {
    let tmp = tempfile::tempdir().unwrap();
    let project = classification_project(tmp.path(), "loose", None);
    let db_path = project.join(".state/state.db");
    admit_ready(&project);
    let rows = classifications(&db_path);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!((rows[0].1.as_str(), rows[0].2, rows[0].3.as_str(), rows[0].4.as_str()), ("loose", None, "unscoped", "unknown"));
    // Contract properties are unknown, never zero.
    assert_eq!(rows[0].5, r#"{"dependencies":0,"repositories":0,"route":{"reason":"no_contract","status":"unavailable"},"uncertain_write_paths":{"reason":"no_contract","status":"unavailable"},"write_named_resources":{"reason":"no_contract","status":"unavailable"},"write_paths":{"reason":"no_contract","status":"unavailable"}}"#);
    let record = format!(r#"{{"band":"unknown","class":"unscoped","classifier":"rule:task-taxonomy.v1","contract_revision":null,"created_unix_ms":{},"features":{},"reason":null,"revision":1,"task_id":"loose","taxonomy":"task-taxonomy.v1"}}"#, rows[0].6, rows[0].5);
    assert_eq!(rows[0].0, format!("sha256:{:x}", Sha256::digest(record.as_bytes())));
}

/// `herdr-projects telemetry <project> attempts --json`, the project under its parent root.
#[cfg(target_os = "linux")]
fn telemetry_attempts(project: &Path) -> serde_json::Value {
    let root = project.parent().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_herdr-projects")).env_clear().env("HOME", root)
        .args(["--root", root.to_str().unwrap(), "telemetry", project.file_name().unwrap().to_str().unwrap(), "attempts", "--json"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

/// `(state, attempt_revision, unix_ms, source)` lifecycle marks of the only attempt, in insertion order.
#[cfg(target_os = "linux")]
fn lifecycle_marks(db_path: &Path) -> Vec<(String, i64, i64, String)> {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    let mut stmt = conn.prepare("SELECT state,attempt_revision,unix_ms,source FROM attempt_lifecycle ORDER BY rowid").unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap().map(Result::unwrap).collect()
}

/// Cancel the one reserved attempt before any launch claim; returns its ID.
#[cfg(target_os = "linux")]
fn cancel_reserved(db_path: &Path) -> String {
    let attempt: String = rusqlite::Connection::open(db_path).unwrap().query_row("SELECT id FROM attempts WHERE state='reserved'", [], |r| r.get(0)).unwrap();
    let mut db = SqliteStore::open(db_path).unwrap();
    assert!(db.cancel_attempt(&AttemptId::new(attempt.clone()).unwrap(), 1, db.current_head().unwrap(), "not needed", unix_ms()).unwrap().released);
    attempt
}

#[cfg(target_os = "linux")]
fn has_numeric_zero(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Number(n) => n.as_i64() == Some(0),
        serde_json::Value::Array(items) => items.iter().any(has_numeric_zero),
        serde_json::Value::Object(map) => map.values().any(has_numeric_zero),
        _ => false,
    }
}

#[cfg(target_os = "linux")]
#[test]
fn cancelled_before_launch_is_censored_not_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let project = classification_project(tmp.path(), "gone", Some("src/gone.rs"));
    let db_path = project.join(".state/state.db");
    admit_ready(&project);
    let attempt = cancel_reserved(&db_path);
    let marks = lifecycle_marks(&db_path);
    assert_eq!(marks.iter().map(|(s, r, _, source)| (s.as_str(), *r, source.as_str())).collect::<Vec<_>>(),
        [("reserved", 1, "admit_prepared"), ("cancelled", 2, "cancel_attempt_in_transaction")]);
    let classification = classifications(&db_path)[0].0.clone();
    let report = telemetry_attempts(&project);
    assert_eq!(report, serde_json::json!({"attempts": [{
        "accepted": false,
        "active_ms": {"reason": "not_running", "status": "unavailable"},
        "attempt_id": attempt,
        "attention": {"reason": "attention_not_collected", "status": "unavailable"},
        "classification": {"band": "small", "class": "code", "classification_id": classification},
        "configuration_id": sha256_id(&sim_configuration("1.0.0")),
        "integration": {"state": "not_applicable"},
        "launching_unix_ms": null,
        "queue_to_launch_ms": {"reason": "cancelled", "status": "censored"},
        "reserved_unix_ms": marks[0].2,
        "result": {"state": "not_submitted"},
        "running_unix_ms": null,
        "task_id": "gone",
        "terminal_state": "cancelled",
        "terminal_unix_ms": marks[1].2,
        "usage": {"reason": "collection_not_run", "status": "unavailable"},
        "verification": {"state": "not_submitted"}
    }]}));
    assert!(!has_numeric_zero(&report), "unknown durations are never 0: {report}");
}

#[cfg(target_os = "linux")]
#[test]
fn outcome_rejected_verification_reason_is_excerpted() {
    let tmp = tempfile::tempdir().unwrap();
    let project = classification_project(tmp.path(), "rej", Some("src/rej.rs"));
    let db_path = project.join(".state/state.db");
    admit_ready(&project);
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let (attempt, digest, repository, oid): (String, String, String, String) = conn.query_row(
        "SELECT a.id,c.raw_digest,c.repository,c.base_oid FROM attempts a JOIN task_contracts c ON c.task_id=a.task_id", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap();
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    let policy = work.join("policy.json");
    fs::write(&policy, b"{\"version\":1,\"checks\":[\"/bin/false\"]}").unwrap();
    let mut db = SqliteStore::open(&db_path).unwrap();
    let submission = serde_json::json!({"idempotency_key": "submit-rej", "task_id": "rej", "contract_revision": 1, "contract_digest": digest,
        "attempt_id": attempt, "repository": repository, "base_oid": oid, "candidate_oid": oid, "object_format": "sha1",
        "artifact_manifest": [{"path": "README", "oid": oid}], "claimed_checks": ["worker prose is not evidence"],
        "objects": [{"oid": oid, "relative_path": format!("{}/{}", &oid[..2], &oid[2..])}]});
    let submitted = db.submit_result(&serde_json::to_vec(&submission).unwrap()).unwrap().submission_id;
    let request = herdr_projects::verification::VerifyRequest::new(submitted.clone(), "builds", &policy, "verify-rej", Duration::from_secs(5), &work);
    let outcome = herdr_projects::verification::verify(&mut db, &request).unwrap();
    assert_eq!((outcome.state.as_str(), outcome.reason.as_deref()), ("rejected", Some("policy_digest_mismatch")));
    let submitted_ms: i64 = conn.query_row("SELECT created_unix_ms FROM result_submissions", [], |r| r.get(0)).unwrap();
    let report = telemetry_attempts(&project);
    let record = &report["attempts"][0];
    assert_eq!(record["result"], serde_json::json!({"candidate_oid": oid, "created_unix_ms": submitted_ms, "state": "submitted", "submission_id": submitted}));
    assert_eq!(record["verification"], serde_json::json!({"reason": "policy_digest_mismatch", "state": "rejected"}));
    assert_eq!((&record["integration"], &record["accepted"]), (&serde_json::json!({"state": "not_applicable"}), &serde_json::json!(false)));
    // Verifier reasons are fixed codes today; a free-text reason is still shown only as an excerpt.
    conn.execute("INSERT INTO verification_runs(run_id,project_store,idempotency_key,payload_digest,submission_id,task_id,contract_revision,contract_digest,attempt_id,policy_id,policy_digest,commit_oid,tree_oid,object_format,memory_fence,isolation,argv,library_manifest,state,reason,exit_status,receipt_digest,store_device,store_inode,created_unix_ms)
        SELECT ?1,project_store,'verify-later',payload_digest,submission_id,task_id,contract_revision,contract_digest,attempt_id,policy_id,policy_digest,commit_oid,tree_oid,object_format,memory_fence,isolation,argv,library_manifest,'rejected',?2,exit_status,NULL,store_device,store_inode,created_unix_ms+1 FROM verification_runs",
        rusqlite::params!["f".repeat(64), "check failed in /home/alice/src/x.rs?token=abc123def\nsecond line is dropped"]).unwrap();
    let report = telemetry_attempts(&project);
    assert_eq!(report["attempts"][0]["verification"], serde_json::json!({"reason": "check failed in ~/src/x.rs", "state": "rejected"}));
}

#[cfg(target_os = "linux")]
#[path = "../src/store/test_schema.rs"]
mod test_schema;

#[cfg(target_os = "linux")]
#[test]
fn pre_0051_attempt_reports_predates_lifecycle_log() {
    let tmp = tempfile::tempdir().unwrap();
    let project = classification_project(tmp.path(), "old", Some("src/old.rs"));
    let db_path = project.join(".state/state.db");
    // Schema 50 is 0051's predecessor (S2's dispatch log already exists there).
    test_schema::historical(&rusqlite::Connection::open(&db_path).unwrap(), 50).unwrap();
    admit_ready(&project);
    assert_eq!(sql_count(&db_path, "SELECT count(*) FROM dispatch_decisions"), 1);
    SqliteStore::open(&db_path).unwrap().upgrade_v1().unwrap();
    assert_eq!(sql_count(&db_path, "PRAGMA user_version"), 51);
    let attempt = cancel_reserved(&db_path);
    let marks = lifecycle_marks(&db_path);
    assert_eq!(marks.iter().map(|(s, r, _, source)| (s.as_str(), *r, source.as_str())).collect::<Vec<_>>(),
        [("cancelled", 2, "cancel_attempt_in_transaction")]);
    let predates = serde_json::json!({"reason": "predates_lifecycle_log", "status": "unavailable"});
    let report = telemetry_attempts(&project);
    let record = &report["attempts"][0];
    assert_eq!(record["attempt_id"], attempt.as_str());
    assert_eq!(record["configuration_id"], sha256_id(&sim_configuration("1.0.0")).as_str());
    for key in ["reserved_unix_ms", "launching_unix_ms", "running_unix_ms", "active_ms", "queue_to_launch_ms"] {
        assert_eq!(record[key], predates, "{key}");
    }
    assert_eq!((&record["terminal_state"], &record["terminal_unix_ms"]), (&serde_json::json!("cancelled"), &serde_json::json!(marks[0].2)));
    assert!(!has_numeric_zero(&report), "{report}");
}

#[cfg(target_os = "linux")]
fn unix_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

fn sql_count(path: &Path, query: &str) -> i64 {
    rusqlite::Connection::open(path)
        .unwrap()
        .query_row(query, [], |row| row.get(0))
        .unwrap()
}

#[cfg(target_os = "linux")]
fn install_fixture_contract(
    db_path: &Path,
    task: &str,
    repository: &str,
    oid: &str,
    object_format: &str,
    path: &str,
    capability_flags: &[&str],
) -> String {
    let store = fs::canonicalize(db_path).unwrap().display().to_string();
    let conn = rusqlite::Connection::open(db_path).unwrap();
    let expected_head: i64 = conn.query_row("SELECT COALESCE(MAX(sequence),0) FROM events", [], |r| r.get(0)).unwrap();
    let raw = serde_json::to_vec(&serde_json::json!({
        "version": 1, "project_store": store, "expected_head": expected_head,
        "task_id": task, "contract_revision": 1,
        "deliverable": "fixture work", "non_goals": "no provider calls",
        "repository": repository, "base_oid": oid, "object_format": object_format,
        "scope": {"paths":[{"path":path,"access":"write"}]},
        "acceptance_policies": [{"id":"builds","text":PLANNING_POLICY}],
        "dependencies": [], "capability_flags": capability_flags, "profile_kind":"codex",
        "retry_class":"none", "result_schema_id":"result-v1", "route":"verify_only",
        "authority":{"id":"fixture-owner","revision":1,"digest":"ab".repeat(32)}
    })).unwrap();
    let digest = format!("{:x}", Sha256::digest(&raw));
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute(
        "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('contract.installed',?1,1,1,?2)",
        rusqlite::params![task, serde_json::json!({"digest": digest, "route": "verify_only"}).to_string()],
    )
    .unwrap();
    let sequence = conn.last_insert_rowid();
    let head: i64 = conn
        .query_row("SELECT MAX(sequence) FROM events", [], |row| row.get(0))
        .unwrap();
    conn.execute(
        "INSERT INTO task_contracts(task_id,contract_revision,plan_revision,project_store,expected_head,repository,base_oid,object_format,memory_snapshot_id,route,raw_bytes,raw_digest,installed_seq) VALUES(?1,1,NULL,?2,?3,?4,?5,?6,NULL,'verify_only',?7,?8,?9)",
        rusqlite::params![task, store, expected_head, repository, oid, object_format, raw, digest, sequence],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO acceptance_policies(task_id,contract_revision,policy_id,body) VALUES(?1,1,'builds',?2)",
        rusqlite::params![task, PLANNING_POLICY],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO contract_scope_paths(task_id,contract_revision,ordinal,path,access,certainty) VALUES(?1,1,0,?2,'write','exact')",
        rusqlite::params![task, path],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO resource_claims(task_id,contract_revision,ordinal,kind,resource,access,certainty) VALUES(?1,1,0,'path',?2,'write','exact')",
        rusqlite::params![task, path],
    )
    .unwrap();
    digest
}

/// Retained worker knowledge for every queued task at its current revision and
/// every retained profile; automatic admission binds it to build the brief.
fn worker_snapshots(project: &Path) {
    worker_snapshots_for(project, None);
}

/// `worker_snapshots` for one retained profile name only (`None`: every profile).
fn worker_snapshots_for(project: &Path, only: Option<&str>) {
    let db_path = project.join(".state/state.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let profiles = conn.prepare("SELECT report FROM native_profiles").unwrap()
        .query_map([], |row| row.get::<_, String>(0)).unwrap()
        .map(|report| serde_json::from_value::<FrozenProfile>(serde_json::from_str::<serde_json::Value>(&report.unwrap()).unwrap()["preparation"]["profile"].clone()).unwrap())
        .collect::<Vec<_>>();
    let tasks = conn.prepare("SELECT id,revision FROM tasks WHERE state='queued'").unwrap()
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
    for (task, revision) in tasks {
        for profile in profiles.iter().filter(|profile| only.is_none_or(|name| profile.name == name)) {
            let exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM memory_snapshots WHERE task_id=?1 AND task_revision=?2 AND profile_name=?3 AND profile_digest=?4)",
                rusqlite::params![task, revision, profile.name, profile.definition_digest], |row| row.get(0)).unwrap();
            if exists { continue; }
            let mut memory = herdr_projects::memory::MemoryStore::from_sqlite(SqliteStore::open(&db_path).unwrap(), project.join(".state/objects"));
            memory.create_worker_snapshot(SnapshotRequest { schema_version: 1, task_id: task.clone(), profile: profile.name.clone(), domains: vec![], paths: vec![], pinned_keys: vec![], sensitivity: "default".into() },
                &profile.name, &profile.definition_digest, profile.config.digest.as_deref(), 32000, "Factory fixture instructions", unix_ms(), None).unwrap();
        }
    }
}

#[cfg(target_os = "linux")]
fn plant_profile(
    db_path: &Path,
    config: &herdr_projects::migration::ConfigReference,
) -> FrozenProfile {
    plant_profile_as(db_path, sim_profile(config, "sim", "1.0.0"))
}

/// The retained `sim` fixture profile under another name and agent version.
#[cfg(target_os = "linux")]
fn sim_profile(config: &herdr_projects::migration::ConfigReference, name: &str, version: &str) -> FrozenProfile {
    let evidence = VersionedReference {
        id: "sim-evidence".into(),
        revision: 1,
        digest: "a".repeat(64),
    };
    let supported = CapabilityEvidence::Supported {
        evidence: evidence.clone(),
    };
    FrozenProfile {
        version: 1,
        name: name.into(),
        kind: "codex".into(),
        definition_digest: "b".repeat(64),
        config: config.clone(),
        arguments_digest: "c".repeat(64),
        environment_names: Vec::new(),
        execution_home: None,
        permission_policy: VersionedReference {
            id: "sim-policy".into(),
            revision: 1,
            digest: "d".repeat(64),
        },
        adapter: evidence,
        agent: ExecutableIdentity {
            path: "/usr/bin/git".into(),
            digest: "e".repeat(64),
            version: version.into(),
        },
        herdr: ExecutableIdentity {
            path: "/usr/bin/git".into(),
            digest: "f".repeat(64),
            version: "1.0.0".into(),
        },
        capabilities: ProfileCapabilities {
            launch: supported.clone(),
            readiness_observation: supported.clone(),
            prompt_submission: supported.clone(),
            stop: supported,
            checkpoint_acknowledgment: CapabilityEvidence::Unknown,
            structured_usage: CapabilityEvidence::Unknown,
            resume: CapabilityEvidence::Unknown,
        },
        workflow_certificate: None,
    }
}

#[cfg(target_os = "linux")]
fn plant_profile_as(db_path: &Path, profile: FrozenProfile) -> FrozenProfile {
    use std::os::unix::fs::MetadataExt;
    profile.validate_for_launch().unwrap();
    let reference = profile.reference().unwrap();
    let returned = profile.clone();
    let canonical = fs::canonicalize(db_path).unwrap();
    let metadata = fs::metadata(&canonical).unwrap();
    let report = serde_json::json!({
        "preparation": {
            "profile": profile,
            "reference": reference,
            "launchable": true,
            "protocol_capable": false,
            "certified": false
        },
        "source_store": [canonical, metadata.dev(), metadata.ino()]
    });
    let text = serde_json::to_string(&report).unwrap();
    let report_digest = format!("{:x}", Sha256::digest(text.as_bytes()));
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute(
        "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('profile.native_retained',?1,1,1,?2)",
        rusqlite::params![reference.id, serde_json::to_string(&reference).unwrap()],
    )
    .unwrap();
    let sequence = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO native_profiles(profile_digest,report,report_digest,sequence) VALUES(?1,?2,?3,?4)",
        rusqlite::params![reference.digest, text, report_digest, sequence],
    )
    .unwrap();
    returned
}

#[cfg(target_os = "linux")]
fn insert_grant(db_path: &Path, inputs: &LaunchInputs) {
    let profile = inputs.effective_profile.as_ref().unwrap();
    let grant = ApprovalGrant {
        version: 1,
        scope: ApprovalScope::for_launch(inputs).unwrap(),
        policy: profile.permission_policy.clone(),
        issued_unix_ms: 0,
        expires_unix_ms: unix_ms() + 3_600_000,
    };
    let reference = grant.reference().unwrap();
    let payload = serde_json::to_vec(&grant).unwrap();
    assert_eq!(format!("{:x}", Sha256::digest(&payload)), reference.digest);
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute(
        "INSERT INTO approval_grants(id,payload,payload_hash) VALUES(?1,?2,?3)",
        rusqlite::params![
            reference.id,
            String::from_utf8(payload).unwrap(),
            reference.digest
        ],
    )
    .unwrap();
}

#[cfg(target_os = "linux")]
const PLANNING_POLICY: &str = "{\"version\":1,\"checks\":[\"/usr/bin/git\",\"diff\",\"--quiet\"]}";


#[cfg(target_os = "linux")]
#[test]
fn planning_gate_ten_logical_workers() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    fs::create_dir_all(project.join(".state")).unwrap();
    let db_path = project.join(".state/state.db");
    let (repo, oid) = build_repo(tmp.path(), SEED);
    let repo = fs::canonicalize(&repo).unwrap();
    let repository = repo.display().to_string();
    let object_format = if oid.len() == 40 { "sha1" } else { "sha256" };
    assert!(matches!(oid.len(), 40 | 64), "{oid}");
    let object_path = format!("{}/{}", &oid[..2], &oid[2..]);
    assert!(
        repo.join(".git/objects").join(&object_path).is_file(),
        "commit object missing"
    );

    let config_path = tmp.path().join("owner.toml");
    fs::write(&config_path, "version = 1\n").unwrap();
    let config = herdr_projects::migration::config_reference(&config_path).unwrap();
    let digest = config.digest.clone().unwrap();

    let mut db = SqliteStore::create(&db_path).unwrap();
    assert_eq!(db.read_snapshot(None).unwrap().schema_version, herdr_projects::store::SCHEMA);
    let workers: Vec<String> = (0..10).map(|index| format!("w-{index:02}")).collect();
    db.commit(Commit {
        expected_head: 0,
        mutations: workers
            .iter()
            .map(|id| Mutation::Task {
                expected: None,
                next: Task {
                    id: TaskId::new(id).unwrap(),
                    revision: 1,
                    state: TaskState::Draft,
                    title: format!("logical worker {id}"),
                    active_attempt: None,
                },
            })
            .collect(),
    })
    .unwrap();
    for id in &workers {
        let snapshot = db.read_snapshot(None).unwrap();
        let task = snapshot
            .tasks
            .iter()
            .find(|task| task.id.as_str() == id)
            .unwrap();
        db.create_runtime(
            Some(&task.id),
            Some(task.revision),
            snapshot.head,
            &RuntimeRoute::default(),
        )
        .unwrap();
    }
    let queued_at = unix_ms() - 1_000;
    for id in &workers {
        let snapshot = db.read_snapshot(None).unwrap();
        let task = snapshot
            .tasks
            .iter()
            .find(|task| task.id.as_str() == id)
            .unwrap();
        db.queue_task(
            &task.id,
            task.revision,
            snapshot.head,
            &QueueRequest {
                priority: 0,
                dependencies: Vec::new(),
            },
            queued_at,
        )
        .unwrap();
    }
    let snapshot = db.read_snapshot(None).unwrap();
    let policy = snapshot.scheduler.as_ref().unwrap().policy.clone();
    db.set_scheduler_policy(
        snapshot.head,
        policy.revision,
        10,
        policy.max_attempts_per_task,
    )
    .unwrap();
    let now = unix_ms();
    let snapshot = db.read_snapshot(None).unwrap();
    let observations = snapshot
        .runtime_bindings
        .iter()
        .map(|binding| {
            let task_revision = binding.task.as_ref().and_then(|id| {
                snapshot
                    .tasks
                    .iter()
                    .find(|task| &task.id == id)
                    .map(|task| task.revision)
            });
            herdr_projects::reconcile::RuntimeObservation {
                binding: binding.id.clone(),
                binding_revision: binding.revision,
                task_revision,
                observed_unix_ms: now,
                collector: "herdr-git-v1".into(),
                config_digest: Some(digest.clone()),
                ..herdr_projects::reconcile::RuntimeObservation::default()
            }
        })
        .collect::<Vec<_>>();
    db.record_observations(snapshot.head, &observations)
        .unwrap();
    let snapshot = db.read_snapshot(None).unwrap();
    let control = snapshot.control.unwrap();
    db.set_project_state(
        snapshot.head,
        control.revision,
        ProjectState::Active,
        now,
        Some(&digest),
    )
    .unwrap();
    drop(db);

    for (index, id) in workers.iter().enumerate() {
        let path = if index >= 8 {
            "shared/gate.rs".to_string()
        } else {
            format!("src/w{index:02}.rs")
        };
        install_fixture_contract(&db_path, id, &repository, &oid, object_format, &path, &["discovered","launchable"]);
    }
    plant_profile(&db_path, &config);
    SqliteStore::open(&db_path).unwrap().record_native_capability_evidence(unix_ms(),unix_ms()+3_600_000).unwrap();
    // Exercise the public admission workflow with more cold reports than the
    // inventory limit. Neither a replaced store nor an old config is eligible.
    {
        let mut conn=rusqlite::Connection::open(&db_path).unwrap();
        let text:String=conn.query_row("SELECT report FROM native_profiles LIMIT 1",[],|row|row.get(0)).unwrap();
        let retained:serde_json::Value=serde_json::from_str(&text).unwrap();
        let tx=conn.transaction().unwrap();
        for index in 0..300 {
            let mut report=retained.clone();
            let mut profile:FrozenProfile=serde_json::from_value(report["preparation"]["profile"].clone()).unwrap();
            profile.name=format!("historical-{index}");
            if index%2==0 {profile.config.digest=Some("f".repeat(64));}
            else {report["source_store"][2]=(report["source_store"][2].as_u64().unwrap()+1).into();}
            let reference=profile.reference().unwrap();
            report["preparation"]["profile"]=serde_json::to_value(&profile).unwrap();
            report["preparation"]["reference"]=serde_json::to_value(&reference).unwrap();
            let text=report.to_string();
            let report_digest=format!("{:x}",Sha256::digest(text.as_bytes()));
            tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('profile.native_retained',?1,1,1,'{}')",[&reference.id]).unwrap();
            tx.execute("INSERT INTO native_profiles VALUES(?1,?2,?3,?4)",rusqlite::params![reference.digest,text,report_digest,tx.last_insert_rowid()]).unwrap();
        }
        tx.commit().unwrap();
    }
    assert!(!herdr_projects::admission::wake_enabled(&project));
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let flag: String = conn
            .query_row(
                "SELECT factory_admission FROM project_control WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(flag, "off");
        // Fixture only. Production code has no writer for this column.
        conn.execute(
            "UPDATE project_control SET factory_admission='on' WHERE singleton=1",
            [],
        )
        .unwrap();
    }
    assert!(herdr_projects::admission::wake_enabled(&project));

    // Compare actual public admission SQL work before/after retained evidence
    // grows. Repeated observations must not multiply capability lookup work.
    herdr_projects::admission::admit_decision_observed(&project).result.unwrap();
    let baseline=herdr_projects::admission::admit_decision_observed(&project);
    let baseline_reason=baseline.result.unwrap().reason;
    {
        let mut conn=rusqlite::Connection::open(&db_path).unwrap();
        let template:String=conn.query_row("SELECT evidence_id FROM capability_evidence ORDER BY evidence_id LIMIT 1",[],|row|row.get(0)).unwrap();
        let tx=conn.transaction().unwrap();
        for index in 0..30_000 {
            let id=format!("{:x}",Sha256::digest(format!("historical-capability-{index}").as_bytes()));
            tx.execute("INSERT INTO capability_evidence SELECT ?1,adapter_kind,binary_digest,os_name,profile_digest,profile_kind,level,'retained-window',CASE WHEN ?2%2=0 THEN 0 ELSE observed_unix_ms END,CASE WHEN ?2%2=0 THEN 1 ELSE expires_unix_ms END,live FROM capability_evidence WHERE evidence_id=?3",rusqlite::params![id,index,template]).unwrap();
        }
        tx.commit().unwrap();
    }
    let retained=herdr_projects::admission::admit_decision_observed(&project);
    assert_eq!(retained.result.unwrap().reason,baseline_reason);
    assert!(baseline.sql.connection_observed && retained.sql.connection_observed);
    println!("capability history SQL VM steps: baseline={}, retained={}",baseline.sql.sqlite_vm_steps,retained.sql.sqlite_vm_steps);
    assert!(retained.sql.sqlite_vm_steps<=baseline.sql.sqlite_vm_steps*2+10_000,
        "capability history multiplied admission work: baseline={}, retained={}",baseline.sql.sqlite_vm_steps,retained.sql.sqlite_vm_steps);

    let mut reserved = Vec::new();
    for _ in 0..workers.len() {
        worker_snapshots(&project);
        let Some(inputs) = herdr_projects::admission::prepared_admission_inputs(&project).unwrap()
        else {
            break;
        };
        let task = inputs.task.as_str().to_string();
        insert_grant(&db_path, &inputs);
        let before = sql_count(&db_path, "SELECT count(*) FROM attempts");
        herdr_projects::admission::admit_once(&project).unwrap();
        let after = sql_count(&db_path, "SELECT count(*) FROM attempts");
        assert_eq!(after, before + 1, "admission did not reserve {task}");
        reserved.push(task);
    }
    assert!(
        herdr_projects::admission::prepared_admission_inputs(&project)
            .unwrap()
            .is_none(),
        "a blocked worker was still ready"
    );
    herdr_projects::admission::admit_once(&project).unwrap();
    assert_eq!(
        reserved.len(),
        9,
        "expected one overlap to leave a single worker unreserved, got {reserved:?}"
    );
    assert!(reserved.contains(&"w-08".to_string()), "{reserved:?}");
    assert!(!reserved.contains(&"w-09".to_string()), "{reserved:?}");
    assert_eq!(sql_count(&db_path, "SELECT count(*) FROM attempts"), 9);
    assert_eq!(sql_count(&db_path, "SELECT count(*) FROM native_profiles"), 301);
    assert_eq!(
        sql_count(
            &db_path,
            "SELECT count(*) FROM (SELECT id FROM attempts GROUP BY id HAVING count(*)>1)"
        ),
        0
    );

    let (attempt, digest): (String, String) = {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let attempt = conn
            .query_row("SELECT id FROM attempts WHERE task_id='w-00'", [], |row| {
                row.get(0)
            })
            .unwrap();
        let digest = conn
            .query_row(
                "SELECT raw_digest FROM task_contracts WHERE task_id='w-00' AND contract_revision=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        (attempt, digest)
    };
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    let mismatch = work.join("policy.json");
    fs::write(&mismatch, b"{\"version\":1,\"checks\":[\"/bin/false\"]}").unwrap();
    let mut db = SqliteStore::open(&db_path).unwrap();
    for index in 1..=3 {
        let submission = serde_json::json!({
            "idempotency_key": format!("submit-{index}"),
            "task_id": "w-00",
            "contract_revision": 1,
            "contract_digest": digest,
            "attempt_id": attempt,
            "repository": repository,
            "base_oid": oid,
            "candidate_oid": oid,
            "object_format": object_format,
            "artifact_manifest": [{"path": "README", "oid": oid}],
            "claimed_checks": ["worker prose is not evidence"],
            "objects": [{"oid": oid, "relative_path": object_path}]
        });
        let receipt = db
            .submit_result(&serde_json::to_vec(&submission).unwrap())
            .unwrap();
        assert!(!receipt.replayed);
        let request = herdr_projects::verification::VerifyRequest::new(
            receipt.submission_id,
            "builds",
            &mismatch,
            format!("verify-{index}"),
            Duration::from_secs(5),
            &work,
        );
        let outcome = herdr_projects::verification::verify(&mut db, &request).unwrap();
        assert_eq!(outcome.state, "rejected", "{:?}", outcome.reason);
        assert_eq!(outcome.reason.as_deref(), Some("policy_digest_mismatch"));
        assert!(outcome.receipt.is_none());
    }
    let feedback: Vec<String> = {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let mut stmt = conn
            .prepare("SELECT feedback_id FROM feedback_items ORDER BY created_unix_ms, feedback_id")
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .map(|row| row.unwrap())
            .collect()
    };
    assert_eq!(feedback.len(), 3);
    assert_eq!(
        sql_count(
            &db_path,
            "SELECT count(DISTINCT task_id) FROM feedback_items"
        ),
        1
    );
    assert_eq!(
        sql_count(
            &db_path,
            "SELECT count(*) FROM feedback_items WHERE task_id='w-00' AND category='verifier_rejection' AND reason='policy_digest_mismatch'"
        ),
        3
    );
    assert_eq!(
        sql_count(&db_path, "SELECT count(*) FROM plan_proposals"),
        0
    );
    let first = db.request_replan(&feedback[0]).unwrap();
    let replayed = db.request_replan(&feedback[0]).unwrap();
    assert_eq!(format!("{first:?}"), format!("{replayed:?}"));
    assert!(
        format!("{first:?}").contains("automatic_count: 1"),
        "{first:?}"
    );
    let second = db.request_replan(&feedback[1]).unwrap();
    assert!(
        format!("{second:?}").contains("automatic_count: 2"),
        "{second:?}"
    );
    let third = db.request_replan(&feedback[2]).unwrap();
    assert!(format!("{third:?}").contains("Escalated"), "{third:?}");
    let again = db.request_replan(&feedback[2]).unwrap();
    assert_eq!(format!("{third:?}"), format!("{again:?}"));
    assert_eq!(
        sql_count(
            &db_path,
            "SELECT count(*) FROM replan_requests WHERE outcome='automatic'"
        ),
        2
    );
    assert_eq!(
        sql_count(
            &db_path,
            "SELECT count(*) FROM replan_requests WHERE outcome='escalated'"
        ),
        1
    );
    assert_eq!(
        sql_count(&db_path, "SELECT count(*) FROM plan_proposals"),
        0
    );
    assert_eq!(sql_count(&db_path, "SELECT count(*) FROM inbox_items"), 3);
    let kind: String = rusqlite::Connection::open(&db_path)
        .unwrap()
        .query_row(
            "SELECT json_extract(payload, '$.kind') FROM inbox_items WHERE json_extract(payload, '$.kind')='replan-escalation'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(kind, "replan-escalation");

    let waits = [
        ("w-01", "user_decision"),
        ("w-02", "resource_availability"),
        ("w-03", "adapter_recovery"),
        ("w-09", "validation_completion"),
    ];
    let registered = db
        .register_wait("w-00", Some(&attempt), "dependency_evidence")
        .unwrap();
    let duplicate = db
        .register_wait("w-00", Some(&attempt), "dependency_evidence")
        .unwrap();
    assert!(
        format!("{registered:?}").contains("already_registered: false"),
        "{registered:?}"
    );
    assert!(
        format!("{duplicate:?}").contains("already_registered: true"),
        "{duplicate:?}"
    );
    for (task, condition) in waits {
        db.register_wait(task, None, condition).unwrap();
    }
    assert_eq!(
        sql_count(
            &db_path,
            "SELECT count(DISTINCT condition) FROM wait_conditions"
        ),
        5
    );
    let wait_id: String = rusqlite::Connection::open(&db_path)
        .unwrap()
        .query_row(
            "SELECT wait_id FROM wait_conditions WHERE task_id='w-00'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let replay = db.replay_wait(&wait_id).unwrap();
    assert!(
        format!("{replay:?}").contains("proved: false"),
        "{replay:?}"
    );
    assert!(
        format!("{replay:?}").contains("already_replayed: false"),
        "{replay:?}"
    );
    let second_replay = db.replay_wait(&wait_id).unwrap();
    assert!(
        format!("{second_replay:?}").contains("already_replayed: false"),
        "{second_replay:?}"
    );
    assert!(
        format!("{second_replay:?}").contains("proved: false"),
        "{second_replay:?}"
    );

    assert_eq!(second_replay.events_applied, 0);
    assert!(!second_replay.wake_requested);
    rusqlite::Connection::open(&db_path).unwrap().execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('wait.wake',?1,1,1,'{}')",[&wait_id]).unwrap();
    assert!(db.replay_wait(&wait_id).unwrap().wake_requested);
    assert!(db.replay_wait(&wait_id).unwrap().already_replayed);
    let attempts_before = sql_count(&db_path, "SELECT count(*) FROM attempts");
    let retry = db.retry_infrastructure("w-00", &attempt).unwrap();
    assert!(format!("{retry:?}").contains("ordinal: 1"), "{retry:?}");
    assert_eq!(
        sql_count(&db_path, "SELECT count(*) FROM attempts"),
        attempts_before
    );
    assert_eq!(
        sql_count(
            &db_path,
            "SELECT count(*) FROM attempt_infrastructure_retries"
        ),
        1
    );
    drop(db);

    assert_eq!(sql_count(&db_path, "SELECT count(*) FROM attempts"), 9);
    assert_eq!(
        sql_count(&db_path, "SELECT count(DISTINCT id) FROM attempts"),
        sql_count(&db_path, "SELECT count(*) FROM attempts")
    );
    assert_eq!(
        sql_count(
            &db_path,
            "SELECT count(*) FROM (SELECT id FROM attempts GROUP BY id HAVING count(*)>1)"
        ),
        0
    );
    assert_eq!(
        sql_count(
            &db_path,
            "SELECT count(*) FROM attempts WHERE task_id='w-09'"
        ),
        0
    );
    assert_eq!(
        sql_count(&db_path, "SELECT count(*) FROM dependency_satisfactions"),
        0
    );
    assert_eq!(
        sql_count(&db_path, "SELECT count(*) FROM verified_results"),
        0
    );
    assert_eq!(
        sql_count(
            &db_path,
            "SELECT count(*) FROM verification_runs WHERE state='accepted'"
        ),
        0
    );
    assert_eq!(
        sql_count(
            &db_path,
            "SELECT count(*) FROM verification_runs WHERE state='rejected' AND task_id='w-00'"
        ),
        3
    );
    assert_eq!(
        sql_count(
            &db_path,
            "SELECT count(*) FROM verification_runs WHERE task_id!='w-00'"
        ),
        0
    );
    assert_eq!(
        sql_count(
            &db_path,
            "SELECT count(*) FROM result_submissions WHERE claimed_checks LIKE '%not evidence%'"
        ),
        3
    );
    assert_eq!(
        sql_count(&db_path, "SELECT count(*) FROM replan_requests"),
        3
    );
}

#[cfg(target_os = "linux")]
struct MemoryProject {
    tmp: tempfile::TempDir,
    project: PathBuf,
    key: PathBuf,
}

#[cfg(target_os = "linux")]
fn memory_project() -> MemoryProject {
    let tmp = tempfile::tempdir().unwrap();
    let key = tmp.path().join("owner");
    assert!(
        Command::new("/usr/bin/ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key)
            .status()
            .unwrap()
            .success()
    );
    let public = fs::read_to_string(key.with_extension("pub"))
        .unwrap()
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    let config = tmp.path().join("config.toml");
    fs::write(
        &config,
        format!("[authority]\nversion=1\nrevision=1\napproval_public_key={public:?}\n"),
    )
    .unwrap();
    let project = tmp.path().join("project");
    fs::create_dir(&project).unwrap();
    for name in [".state", "threads", "inbox"] {
        fs::create_dir(project.join(name)).unwrap();
    }
    fs::create_dir_all(project.join(".state/objects")).unwrap();
    fs::write(
        project.join("PROJECT.md"),
        "+++\nname='Demo'\n+++\nOriginal instructions\n",
    )
    .unwrap();
    fs::write(project.join("TASKS.md"), "").unwrap();
    fs::write(project.join("MEMORY.md"), "Original memory").unwrap();
    fs::write(
        project.join(".state/project.json"),
        r#"{"status":"paused"}"#,
    )
    .unwrap();
    let plan = herdr_projects::migration::inspect_with_config(&project, &config).unwrap();
    herdr_projects::migration::apply(&project, &plan, true).unwrap();
    let state = herdr_projects::runtime::snapshot(&project).unwrap();
    herdr_projects::runtime::set_state(
        &project,
        state.head,
        state.control.unwrap().revision,
        ProjectState::Active,
        &config,
    )
    .unwrap();
    assert_eq!(
        herdr_projects::runtime::snapshot(&project)
            .unwrap()
            .schema_version,
        herdr_projects::store::SCHEMA
    );
    MemoryProject { tmp, project, key }
}

#[cfg(target_os = "linux")]
fn state_db(project: &Path) -> PathBuf {
    project.join(".state/state.db")
}

#[cfg(target_os = "linux")]
fn assert_admission_off(project: &Path) {
    let flag: String = rusqlite::Connection::open(state_db(project))
        .unwrap()
        .query_row(
            "SELECT factory_admission FROM project_control WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(flag, "off");
}

#[cfg(target_os = "linux")]
fn sign_review(key: &Path, path: &Path, bytes: &[u8]) -> PathBuf {
    fs::write(path, bytes).unwrap();
    let sig = PathBuf::from(format!("{}.sig", path.display()));
    let _ = fs::remove_file(&sig);
    assert!(
        Command::new("/usr/bin/ssh-keygen")
            .args(["-Y", "sign", "-f"])
            .arg(key)
            .args(["-n", herdr_projects::authority::MEMORY_REVIEW_NAMESPACE])
            .arg(path)
            .status()
            .unwrap()
            .success()
    );
    sig
}

#[cfg(target_os = "linux")]
fn memory_read_set(project: &Path) -> MemoryReadSet {
    let conn = rusqlite::Connection::open(state_db(project)).unwrap();
    let required_set_generation: i64 = conn
        .query_row(
            "SELECT generation FROM memory_required_generation WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let policy_revision: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(revision), 0) FROM memory_policies",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let load = |sql: &str| -> Vec<(String, i64)> {
        let mut stmt = conn.prepare(sql).unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(|row| row.unwrap())
            .collect()
    };
    let record_heads = load(
        "SELECT r.id, h.revision FROM memory_records r
         JOIN memory_heads h ON h.record_id = r.id
         WHERE h.status = 'active'
           AND (r.is_hard = 1 OR r.kind IN ('constraint', 'hard_memory', 'contract'))
         ORDER BY r.id",
    )
    .into_iter()
    .map(|(record_id, revision)| ReadSetHead {
        record_id,
        revision: u64::try_from(revision).unwrap(),
    })
    .collect();
    let validity_revisions = load(
        "SELECT v.record_id, v.revision FROM memory_validity v
         JOIN memory_heads h ON h.record_id = v.record_id AND h.revision = v.revision
         JOIN memory_records r ON r.id = v.record_id
         WHERE h.status = 'active'
           AND (r.is_hard = 1 OR r.kind IN ('constraint', 'hard_memory', 'contract'))
         ORDER BY v.record_id",
    )
    .into_iter()
    .map(|(record_id, revision)| ReadSetValidity {
        record_id,
        revision: u64::try_from(revision).unwrap(),
    })
    .collect();
    let scope_catalog_generations =
        load("SELECT scope_id, generation FROM memory_scope_catalog ORDER BY scope_id")
            .into_iter()
            .map(|(scope_id, generation)| ScopeCatalogGeneration {
                scope_id,
                generation: u64::try_from(generation).unwrap(),
            })
            .collect();
    MemoryReadSet {
        record_heads,
        validity_revisions,
        reviewer_grant_id: None,
        revocation_epoch: 0,
        policy_revision: u64::try_from(policy_revision).unwrap(),
        required_set_generation: u64::try_from(required_set_generation).unwrap(),
        scope_catalog_generations,
    }
}

#[cfg(target_os = "linux")]
fn add_task_snapshot(project: &Path, task: &str, domains: &[&str], instructions: &str) -> String {
    let head = herdr_projects::runtime::snapshot(project).unwrap().head;
    herdr_projects::runtime::add_task(project, TaskId::new(task).unwrap(), task.into(), head)
        .unwrap();
    let mut memory = herdr_projects::memory::MemoryStore::from_sqlite(
        herdr_projects::migration::open_active(project).unwrap(),
        project.join(".state/objects"),
    );
    let snap = memory
        .create_task_snapshot(
            SnapshotRequest {
                schema_version: 1,
                task_id: task.into(),
                profile: "worker".into(),
                domains: domains.iter().map(|domain| (*domain).to_string()).collect(),
                paths: Vec::new(),
                pinned_keys: Vec::new(),
                sensitivity: "default".into(),
            },
            "worker",
            &"a".repeat(64),
            None,
            32_000,
            instructions,
            1,
            None,
        )
        .unwrap();
    snap.id.as_str().to_string()
}

#[cfg(target_os = "linux")]
fn bind_attempt(project: &Path, task: &str, attempt: &str, snapshot: &str) {
    let mut db = herdr_projects::migration::open_active(project).unwrap();
    let head = db.read_snapshot(None).unwrap().head;
    db.commit(Commit {
        expected_head: head,
        mutations: vec![Mutation::Attempt {
            expected: None,
            next: Attempt {
                id: AttemptId::new(attempt).unwrap(),
                task: TaskId::new(task).unwrap(),
                revision: 1,
                state: AttemptState::Running,
                snapshot: Some(snapshot.into()),
                reservation: attempt.into(),
                termination_observed: false,
            },
        }],
    })
    .unwrap();
}

#[cfg(target_os = "linux")]
fn propose_observation(
    project: &Path,
    proposal: &str,
    task: &str,
    attempt: &str,
    snapshot: &str,
    key: &str,
    domain: &str,
) -> String {
    let mut memory = herdr_projects::memory::MemoryStore::from_sqlite(
        herdr_projects::migration::open_active(project).unwrap(),
        project.join(".state/objects"),
    );
    let body = memory.ingest_object(key.as_bytes()).unwrap();
    let doc = ProposalDocument {
        schema_version: 1,
        proposal_id: proposal.into(),
        producer: ProposalProducer {
            task_id: task.into(),
            attempt_id: attempt.into(),
        },
        input_snapshot_id: snapshot.into(),
        observed_revisions: Vec::new(),
        repository: None,
        changes: vec![ProposalChange {
            record_key: key.into(),
            expected: None,
            kind: "observation".into(),
            scope: Applicability {
                domains: vec![domain.into()],
                paths: Vec::new(),
            },
            claim: format!("note {key}"),
            body_object: body.as_str().into(),
            evidence: Vec::new(),
            based_on: Vec::new(),
            impact: "informational".into(),
        }],
    };
    let receipt = memory
        .propose(&serde_json::to_vec(&doc).unwrap(), 1_000)
        .unwrap();
    assert_eq!(receipt.validation, "accepted", "{}", receipt.reason);
    receipt.payload_digest
}

#[cfg(target_os = "linux")]
fn review_proposal(
    fixture: &MemoryProject,
    proposal: &str,
    digest: &str,
    key: &str,
    read_set_version: Option<u32>,
) -> ReviewDecision {
    let head = herdr_projects::runtime::snapshot(&fixture.project)
        .unwrap()
        .head;
    let doc = MemoryReviewAuthorization {
        version: 1,
        project_store: state_db(&fixture.project)
            .canonicalize()
            .unwrap()
            .display()
            .to_string(),
        authority: herdr_projects::authority::policy_reference(&fixture.project).unwrap(),
        expected_head: head,
        expires_unix_ms: jiff::Timestamp::now().as_millisecond() + 60_000,
        proposal_digest: digest.into(),
        record_keys: vec![key.into()],
        review: ReviewDocument {
            schema_version: 1,
            proposal_id: proposal.into(),
            decision: "approve".into(),
            reason: "evidence supports the claim".into(),
        },
        read_set_version,
        read_set: read_set_version
            .filter(|version| *version == 2)
            .map(|_| memory_read_set(&fixture.project)),
    };
    let path = fixture.tmp.path().join(format!("{proposal}.json"));
    let sig = sign_review(&fixture.key, &path, &serde_json::to_vec(&doc).unwrap());
    herdr_projects::authority::review_memory_proposal(&fixture.project, proposal, &path, &sig, head)
        .unwrap()
}

#[cfg(target_os = "linux")]
fn event_head(decision: &ReviewDecision) -> u64 {
    let reviewed: serde_json::Value = serde_json::from_str(&decision.reviewed_heads).unwrap();
    reviewed["event_head"].as_u64().unwrap()
}

#[cfg(target_os = "linux")]
fn intents_for(db: &mut SqliteStore, cause: &str) -> Vec<serde_json::Value> {
    db.memory_delivery_intents()
        .unwrap()
        .into_iter()
        .filter(|row| row["cause_id"] == cause)
        .collect()
}

#[cfg(target_os = "linux")]
#[test]
fn memory_two_domains_promote_and_same_head_conflicts() {
    let fixture = memory_project();
    let project = &fixture.project;
    assert_admission_off(project);
    let api_snap = add_task_snapshot(project, "api-task", &["api"], "api instructions");
    bind_attempt(project, "api-task", "api-attempt", &api_snap);
    let ui_snap = add_task_snapshot(project, "ui-task", &["ui"], "ui instructions");
    bind_attempt(project, "ui-task", "ui-attempt", &ui_snap);
    let api_digest = propose_observation(
        project,
        "mp-api",
        "api-task",
        "api-attempt",
        &api_snap,
        "api.error-envelope",
        "api",
    );
    let ui_digest = propose_observation(
        project,
        "mp-ui",
        "ui-task",
        "ui-attempt",
        &ui_snap,
        "ui.copy",
        "ui",
    );
    // v2 fences on the read set, not the event head, so an intervening review
    // of the other domain does not block either promotion.
    let api_review = review_proposal(
        &fixture,
        "mp-api",
        &api_digest,
        "api.error-envelope",
        Some(2),
    );
    let ui_review = review_proposal(&fixture, "mp-ui", &ui_digest, "ui.copy", Some(2));
    let api_promoted =
        herdr_projects::authority::promote_memory_proposal(project, "mp-api", &api_review.id)
            .unwrap();
    let ui_promoted =
        herdr_projects::authority::promote_memory_proposal(project, "mp-ui", &ui_review.id)
            .unwrap();
    assert!(!api_promoted.reused);
    assert!(!ui_promoted.reused);
    assert!(
        api_promoted
            .change_ids
            .iter()
            .all(|id| !ui_promoted.change_ids.contains(id))
    );
    let mut db = herdr_projects::migration::open_active(project).unwrap();
    let api = db
        .memory_record_by_key("api.error-envelope")
        .unwrap()
        .unwrap();
    let ui = db.memory_record_by_key("ui.copy").unwrap().unwrap();
    assert_ne!(api.id, ui.id);
    assert_eq!(
        db.memory_head(api.id.as_str()).unwrap().unwrap().revision,
        1
    );
    assert_eq!(db.memory_head(ui.id.as_str()).unwrap().unwrap().revision, 1);
    assert!(db.memory_promotion("mp-api").unwrap().is_some());
    assert!(db.memory_promotion("mp-ui").unwrap().is_some());
    drop(db);

    let same_a = propose_observation(
        project,
        "mp-same-a",
        "api-task",
        "api-attempt",
        &api_snap,
        "api.same-a",
        "api",
    );
    let same_b = propose_observation(
        project,
        "mp-same-b",
        "api-task",
        "api-attempt",
        &api_snap,
        "api.same-b",
        "api",
    );
    let first_review = review_proposal(&fixture, "mp-same-a", &same_a, "api.same-a", None);
    let shared_head = event_head(&first_review);
    // Each review stamps a new event head. The second approval is stored at the
    // first decision's head so both promotions expect that same fence.
    {
        let template: serde_json::Value =
            serde_json::from_str(&first_review.reviewed_heads).unwrap();
        let mut reviewed = template.clone();
        reviewed["records"] = serde_json::json!({});
        reviewed["authorization"]["proposal_digest"] = serde_json::json!(same_b);
        reviewed["authorization"]["record_keys"] = serde_json::json!(["api.same-b"]);
        reviewed["authorization"]["review"]["proposal_id"] = serde_json::json!("mp-same-b");
        assert_eq!(reviewed["event_head"].as_u64(), Some(shared_head));
        let conn = rusqlite::Connection::open(state_db(project)).unwrap();
        conn.execute(
            "INSERT INTO review_decisions(id,proposal_id,payload_digest,decision,classification,reviewed_heads,reason,created_unix_ms) VALUES('rev-same-b','mp-same-b',?1,'approve','[]',?2,'same event head',1)",
            rusqlite::params![same_b, reviewed.to_string()],
        )
        .unwrap();
    }
    let stored = {
        let mut db = herdr_projects::migration::open_active(project).unwrap();
        db.review_decision("rev-same-b").unwrap().unwrap()
    };
    assert_eq!(event_head(&stored), shared_head);
    let promoted =
        herdr_projects::authority::promote_memory_proposal(project, "mp-same-a", &first_review.id)
            .unwrap();
    assert!(!promoted.reused);
    let conflict =
        herdr_projects::authority::promote_memory_proposal(project, "mp-same-b", "rev-same-b")
            .unwrap_err();
    let text = format!("{conflict:#}").to_ascii_lowercase();
    assert!(text.contains("conflict"), "{conflict:#}");
    let mut db = herdr_projects::migration::open_active(project).unwrap();
    assert!(db.memory_promotion("mp-same-a").unwrap().is_some());
    assert!(db.memory_promotion("mp-same-b").unwrap().is_none());
    assert!(db.memory_record_by_key("api.same-b").unwrap().is_none());
    assert_eq!(
        sql_count(&state_db(project), "SELECT count(*) FROM memory_promotions"),
        3
    );
    assert_admission_off(project);
}

#[cfg(target_os = "linux")]
#[test]
fn memory_retired_subscription_keeps_the_pending_obligation() {
    let fixture = memory_project();
    let project = &fixture.project;
    let reader = add_task_snapshot(project, "reader", &["notes"], "reader instructions");
    let writer = add_task_snapshot(project, "writer", &["notes"], "writer instructions");
    bind_attempt(project, "writer", "writer-attempt", &writer);
    let first_digest = propose_observation(
        project,
        "mp-note-1",
        "writer",
        "writer-attempt",
        &writer,
        "notes.one",
        "notes",
    );
    let first_review = review_proposal(&fixture, "mp-note-1", &first_digest, "notes.one", None);
    herdr_projects::authority::promote_memory_proposal(project, "mp-note-1", &first_review.id)
        .unwrap();
    let successor = {
        let mut memory = herdr_projects::memory::MemoryStore::from_sqlite(
            herdr_projects::migration::open_active(project).unwrap(),
            project.join(".state/objects"),
        );
        memory
            .create_task_snapshot(
                SnapshotRequest {
                    schema_version: 1,
                    task_id: "reader".into(),
                    profile: "worker".into(),
                    domains: vec!["notes".into()],
                    paths: Vec::new(),
                    pinned_keys: Vec::new(),
                    sensitivity: "default".into(),
                },
                "worker",
                &"a".repeat(64),
                None,
                32_000,
                "successor instructions",
                1,
                None,
            )
            .unwrap()
            .id
            .as_str()
            .to_string()
    };
    let mut db = herdr_projects::migration::open_active(project).unwrap();
    let pending = intents_for(&mut db, "mp-note-1");
    let delivery = pending
        .iter()
        .find(|row| row["snapshot_id"] == reader)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        pending.iter().find(|row| row["id"] == delivery).unwrap()["state"],
        "pending"
    );
    let retired = db.consumer_binding_for_snapshot(&reader).unwrap().unwrap();
    let next = db
        .consumer_binding_for_snapshot(&successor)
        .unwrap()
        .unwrap();
    db.retire_consumer_binding(&retired.binding_id, Some(&next.binding_id))
        .unwrap();
    db.retire_consumer_binding(&retired.binding_id, Some(&next.binding_id))
        .unwrap();
    let retired = db.consumer_binding(&retired.binding_id).unwrap().unwrap();
    assert!(retired.retired);
    assert!(!retired.active);
    assert_eq!(
        retired.successor_binding_id.as_deref(),
        Some(next.binding_id.as_str())
    );
    assert!(
        db.binding_obligations(&next.binding_id)
            .unwrap()
            .contains(&delivery)
    );
    assert_eq!(
        intents_for(&mut db, "mp-note-1")
            .iter()
            .filter(|row| row["id"] == delivery)
            .count(),
        1
    );
    drop(db);
    // The legacy subscription survives retirement; production fan-out must use
    // current bindings, or the following promotion would redeliver to this reader.
    let raw=rusqlite::Connection::open(project.join(".state/state.db")).unwrap();
    assert!(raw.query_row("SELECT count(*) FROM memory_subscriptions WHERE snapshot_id=?1",[&reader],|r|r.get::<_,u64>(0)).unwrap()>0);
    drop(raw);
    let second_digest = propose_observation(
        project,
        "mp-note-2",
        "writer",
        "writer-attempt",
        &writer,
        "notes.two",
        "notes",
    );
    let second_review = review_proposal(&fixture, "mp-note-2", &second_digest, "notes.two", None);
    herdr_projects::authority::promote_memory_proposal(project, "mp-note-2", &second_review.id)
        .unwrap();
    let mut db = herdr_projects::migration::open_active(project).unwrap();
    let fresh = intents_for(&mut db, "mp-note-2");
    assert!(fresh.iter().all(|row| row["snapshot_id"] != reader));
    assert!(fresh.iter().any(|row| row["snapshot_id"] == successor));
    let fresh_ids: Vec<String> = fresh
        .iter()
        .map(|row| row["id"].as_str().unwrap().to_string())
        .collect();
    let retired_obligations = db.binding_obligations(&retired.binding_id).unwrap();
    assert!(fresh_ids.iter().all(|id| !retired_obligations.contains(id)));
    let successor_obligations = db.binding_obligations(&next.binding_id).unwrap();
    assert!(
        fresh_ids
            .iter()
            .any(|id| successor_obligations.contains(id))
    );
    assert!(successor_obligations.contains(&delivery));
    assert_eq!(
        intents_for(&mut db, "mp-note-1")
            .iter()
            .filter(|row| row["id"] == delivery && row["state"] == "pending")
            .count(),
        1
    );
    assert_admission_off(project);
}

#[cfg(target_os = "linux")]
#[test]
fn memory_package_gap_and_coordinator_ack_without_an_attempt() {
    let fixture = memory_project();
    let project = &fixture.project;
    let writer = add_task_snapshot(project, "writer", &["pkg"], "writer instructions");
    bind_attempt(project, "writer", "writer-attempt", &writer);
    let coordinator = {
        let mut memory = herdr_projects::memory::MemoryStore::from_sqlite(
            herdr_projects::migration::open_active(project).unwrap(),
            project.join(".state/objects"),
        );
        memory
            .create_coordinator_snapshot(
                "gate",
                "planner",
                &"a".repeat(64),
                None,
                32_000,
                "Coordinate",
                1,
            )
            .unwrap()
            .id
            .as_str()
            .to_string()
    };
    let first_digest = propose_observation(
        project,
        "mp-pkg-a",
        "writer",
        "writer-attempt",
        &writer,
        "pkg.a",
        "pkg",
    );
    let first_review = review_proposal(&fixture, "mp-pkg-a", &first_digest, "pkg.a", None);
    herdr_projects::authority::promote_memory_proposal(project, "mp-pkg-a", &first_review.id)
        .unwrap();
    let mut db = herdr_projects::migration::open_active(project).unwrap();
    let binding = db
        .consumer_binding_for_snapshot(&coordinator)
        .unwrap()
        .unwrap();
    assert!(binding.attempt_id.is_none());
    assert!(binding.task_id.is_none());
    let first = db.materialize_update_package(&binding.binding_id).unwrap();
    assert_eq!(first.change_ids.len(), 1);
    let old_id = first.change_ids[0].clone();
    drop(db);
    let second_digest = propose_observation(
        project,
        "mp-pkg-b",
        "writer",
        "writer-attempt",
        &writer,
        "pkg.b",
        "pkg",
    );
    let second_review = review_proposal(&fixture, "mp-pkg-b", &second_digest, "pkg.b", None);
    herdr_projects::authority::promote_memory_proposal(project, "mp-pkg-b", &second_review.id)
        .unwrap();
    let mut db = herdr_projects::migration::open_active(project).unwrap();
    let new_id = intents_for(&mut db, "mp-pkg-b")
        .iter()
        .find(|row| row["snapshot_id"] == coordinator)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(!first.change_ids.contains(&new_id));
    let attempts = sql_count(&state_db(project), "SELECT count(*) FROM attempts");
    assert_eq!(attempts, 1);
    let seen = herdr_projects::store::UpdatePackageAck {
        schema_version: 1,
        package_id: first.package_id.clone(),
        manifest_hash: first.manifest_hash.clone(),
        change_ids: first.change_ids.clone(),
        disposition: "seen".into(),
    };
    db.acknowledge_update_package(&seen, 1).unwrap();
    let mut applied = seen;
    applied.disposition = "applied".into();
    db.acknowledge_update_package(&applied, 1).unwrap();
    assert!(
        db.applied_cursor_covers(&binding.binding_id, &old_id)
            .unwrap()
    );
    assert!(
        !db.applied_cursor_covers(&binding.binding_id, &new_id)
            .unwrap()
    );
    let reloaded = db.consumer_binding(&binding.binding_id).unwrap().unwrap();
    assert!(reloaded.attempt_id.is_none());
    drop(db);
    assert_eq!(
        sql_count(&state_db(project), "SELECT count(*) FROM attempts"),
        attempts
    );
    let applied_new: i64 = rusqlite::Connection::open(state_db(project))
        .unwrap()
        .query_row(
            "SELECT count(*) FROM memory_change_receipts WHERE change_id=?1 AND disposition='applied'",
            [&new_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(applied_new, 0);
    assert_admission_off(project);
}

#[cfg(target_os = "linux")]
struct BarrierSeed {
    task: String,
    attempt: String,
    result: String,
    verification: String,
}

#[cfg(target_os = "linux")]
fn seed_barrier_member(path: &Path, task: &str) -> BarrierSeed {
    let conn = rusqlite::Connection::open(path).unwrap();
    let seq: i64 = conn
        .query_row("SELECT coalesce(max(sequence),0) FROM events", [], |row| {
            row.get(0)
        })
        .unwrap();
    if seq == 0 {
        conn.execute(
            "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('fixture',?1,1,1,'{}')",
            [task],
        )
        .unwrap();
    }
    let installed: i64 = conn
        .query_row("SELECT coalesce(max(sequence),0) FROM events", [], |row| {
            row.get(0)
        })
        .unwrap();
    let hash = "11".repeat(32);
    // Valid retained contract bytes and linked provenance; the receipt rows
    // remain explicit store fixtures, not live verifier certification.
    let contract = serde_json::to_vec(&serde_json::json!({
        "version":1,"project_store":"/tmp/project","expected_head":0,"task_id":task,"contract_revision":1,
        "deliverable":"Barrier fixture","non_goals":"No external work","acceptance_policies":[{"id":"policy-1","text":"{}"}],
        "repository":"/tmp/repo","base_oid":"b".repeat(40),"object_format":"sha1","dependencies":[],"capability_flags":[],
        "profile_kind":"codex","retry_class":"none","result_schema_id":"result-v1","route":"verify_only",
        "authority":{"id":"owner-approval-policy","revision":1,"digest":"ab".repeat(32)}
    })).unwrap();
    let contract_digest = format!("{:x}",Sha256::digest(&contract));
    let policy_digest = format!("{:x}",Sha256::digest(b"{}"));
    let attempt = format!("attempt-{task}");
    let snapshot = format!("snap-{task}");
    conn.execute(
        "INSERT INTO tasks(id,revision,state,title,active_attempt) VALUES(?1,1,'running',?1,NULL)",
        [task],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO memory_snapshots(id,task_id,task_revision,profile_name,profile_digest,config_digest,selection_policy_version,estimator,sequence,required_bytes,optional_bytes,budget_bytes,omitted_optional_count,manifest_hash,scope_digest)
         VALUES(?1,?2,1,'worker',?3,NULL,1,'test',1,0,0,0,0,?3,?3)",
        rusqlite::params![snapshot, task, hash],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO attempts(id,task_id,revision,state,snapshot,reservation,termination_observed) VALUES(?1,?2,1,'running',?3,?4,0)",
        rusqlite::params![attempt, task, snapshot, format!("slot-{task}")],
    )
    .unwrap();
    conn.execute(
        "UPDATE tasks SET active_attempt=?2 WHERE id=?1",
        rusqlite::params![task, attempt],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO task_contracts(task_id,contract_revision,plan_revision,project_store,expected_head,repository,base_oid,object_format,memory_snapshot_id,route,raw_bytes,raw_digest,installed_seq)
         VALUES(?1,1,NULL,'/tmp/project',0,'/tmp/repo',?2,'sha1',NULL,'verify_only',?5,?3,?4)",
        rusqlite::params![task, "b".repeat(40), contract_digest, installed, contract],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO acceptance_policies(task_id,contract_revision,policy_id,body) VALUES(?1,1,'policy-1','{}')",
        [task],
    )
    .unwrap();
    let submission = format!(
        "{:x}",
        Sha256::digest(format!("submission-{task}").as_bytes())
    );
    let result = format!("{:x}", Sha256::digest(format!("result-{task}").as_bytes()));
    let verification = format!("{:x}", Sha256::digest(format!("run-{task}").as_bytes()));
    conn.execute(
        "INSERT INTO result_submissions(submission_id,project_store,idempotency_key,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,memory_snapshot_id,artifact_manifest,claimed_checks,created_unix_ms)
         VALUES(?1,'/tmp/project',?2,?3,'{}',?4,1,?7,?5,'/tmp/repo',?6,?8,'sha1',NULL,'[]','[]',1)",
        rusqlite::params![submission, format!("submit-{task}"), hash, task, attempt, "b".repeat(40), contract_digest, "c".repeat(40)],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO verification_runs(run_id,project_store,idempotency_key,payload_digest,submission_id,task_id,contract_revision,contract_digest,attempt_id,policy_id,policy_digest,commit_oid,tree_oid,object_format,memory_fence,isolation,argv,library_manifest,state,reason,exit_status,receipt_digest,store_device,store_inode,created_unix_ms)
         VALUES(?1,'/tmp/project',?2,?3,?4,?5,1,?8,?6,'policy-1',?9,?7,?7,'sha1',0,'linux-unshare-user-pid-mount-v1','[]','[]','accepted',NULL,0,?3,0,0,1)",
        rusqlite::params![verification, format!("verify-{task}"), hash, submission, task, attempt, "c".repeat(40), contract_digest, policy_digest],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO verified_results(result_id,run_id,submission_id,commit_oid,tree_oid,object_format,policy_digest,receipt_digest,isolation,memory_fence,created_unix_ms)
         VALUES(?1,?2,?3,?4,?4,'sha1',?6,?5,'linux-unshare-user-pid-mount-v1',0,1)",
        rusqlite::params![result, verification, submission, "c".repeat(40), hash, policy_digest],
    )
    .unwrap();
    // This barrier fixture represents a current, natively verified receipt.
    conn.execute("INSERT INTO verification_contract_checks VALUES(?1,2)",[&result]).unwrap();
    BarrierSeed {
        task: task.into(),
        attempt,
        result,
        verification,
    }
}

#[cfg(target_os = "linux")]
fn barrier_member(seed: &BarrierSeed) -> herdr_projects::store::BarrierMember {
    herdr_projects::store::BarrierMember {
        task_id: seed.task.clone(),
        contract_revision: 1,
        attempt_id: seed.attempt.clone(),
        result_id: seed.result.clone(),
        verification_id: seed.verification.clone(),
        integration_id: None,
        proposal_dispositions: Vec::new(),
    }
}


fn insert_scale_history(path: &Path, events: usize) {
    let mut conn = rusqlite::Connection::open(path).unwrap();
    let tx = conn.transaction().unwrap();
    let mut remaining = events;
    while remaining > 0 {
        let batch = remaining.min(500);
        tx.execute(
            "WITH RECURSIVE c(n) AS (
                SELECT 1
                UNION ALL
                SELECT n + 1 FROM c WHERE n < ?1
            )
            INSERT INTO events(kind, entity, revision, payload_version, payload)
            SELECT 'scale.history', 'history', 1, 1, '{}' FROM c",
            [i64::try_from(batch).unwrap()],
        )
        .unwrap();
        remaining -= batch;
    }
    tx.commit().unwrap();
}

// Retirement currently requires terminated attempt history for each canonical
// binding. Those 10,000 supporting attempts are separate from the additional
// 256/1,024 retained-history dimension; report both rather than conflating them.
fn insert_scale_inventory(path: &Path, additional_attempts: usize) {
    let mut connection=rusqlite::Connection::open(path).unwrap();
    connection.execute_batch("PRAGMA foreign_keys=ON").unwrap();
    let tx=connection.transaction().unwrap();
    {
        let mut task=tx.prepare("INSERT INTO tasks(id,revision,state,title,active_attempt) VALUES(?1,1,'succeeded','retired scale fixture',NULL)").unwrap();
        let mut attempt=tx.prepare("INSERT INTO attempts(id,task_id,revision,state,snapshot,reservation,termination_observed) VALUES(?1,?2,1,'completed',NULL,?1,1)").unwrap();
        let mut binding=tx.prepare("INSERT INTO runtime_bindings(id,task_id,revision,source_path,payload,payload_hash) VALUES(?1,?2,1,NULL,?3,?4)").unwrap();
        for index in 0..10_000 {
            let id=TaskId::new(format!("retired-{index:05}")).unwrap();
            task.execute([id.as_str()]).unwrap();
            attempt.execute(rusqlite::params![format!("retirement-{index:05}"),id.as_str()]).unwrap();
            let record=RuntimeBinding {id:format!("task:{}",id.as_str()),task:Some(id.clone()),revision:1,
                source_path:None,source_digest:None,session_source_digest:None,
                verification:RuntimeVerification::Unverified,identity:RuntimeIdentity::default()};
            let payload=serde_json::to_string(&record).unwrap();
            let hash=format!("{:x}",Sha256::digest(payload.as_bytes()));
            binding.execute(rusqlite::params![record.id,id.as_str(),payload,hash]).unwrap();
        }
        task.execute(["retained-history"]).unwrap();
        for index in 0..additional_attempts {
            attempt.execute(rusqlite::params![format!("history-attempt-{index:04}"),"retained-history"]).unwrap();
        }
    }
    // Direct SQL fixture loading must invalidate the rebuildable inventory just
    // as canonical runtime mutations do. No authority or grant is fabricated.
    tx.execute("UPDATE active_work_meta SET projection_revision=?1",["0".repeat(64)]).unwrap();
    tx.commit().unwrap();
}

/// History is event rows the hot path must not have to snapshot. Workers are the
/// active set. A satisfaction row without a verified receipt is not evidence.
fn scale_case(history_events: usize, workers: usize, additional_attempts: usize) -> (u64,u64) {
    assert!(matches!(additional_attempts,256|1024));
    assert!(matches!(workers, 32 | 64));
    assert!(matches!(history_events, 1_000 | 100_000));
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir(tmp.path().join(".state")).unwrap();
    let path = tmp.path().join(".state/state.db");
    SqliteStore::create(&path).unwrap();
    insert_scale_history(&path, history_events);
    let mut db = SqliteStore::open(&path).unwrap();
    assert_eq!(db.current_head().unwrap(), history_events as u64);
    assert_eq!(
        db.read_targeted_hot_path(0, true).unwrap(),
        herdr_projects::store::SCHEMA
    );

    let ids: Vec<TaskId> = (0..workers)
        .map(|index| TaskId::new(format!("w-{index:04}")).unwrap())
        .collect();
    let mutations = ids
        .iter()
        .map(|id| Mutation::Task {
            expected: None,
            next: Task {
                id: id.clone(),
                revision: 1,
                state: TaskState::Draft,
                title: format!("logical worker {}", id.as_str()),
                active_attempt: None,
            },
        })
        .collect();
    db.commit(Commit {
        expected_head: db.current_head().unwrap(),
        mutations,
    })
    .unwrap();
    let mut revision = vec![1u64; workers];
    for (index, id) in ids.iter().enumerate() {
        let change = db
            .create_runtime(
                Some(id),
                Some(revision[index]),
                db.current_head().unwrap(),
                &RuntimeRoute::default(),
            )
            .unwrap();
        revision[index] = change.task_revision.unwrap();
    }
    let now = 1_700_000_000_000;
    for (index, id) in ids.iter().enumerate() {
        let dependencies = if index == 0 {
            Vec::new()
        } else {
            vec![Dependency {
                predecessor: ids[0].clone(),
                requirement: DependencyRequirement::VerifiedResult,
            }]
        };
        db.queue_task(
            id,
            revision[index],
            db.current_head().unwrap(),
            &QueueRequest {
                priority: 0,
                dependencies,
            },
            now,
        )
        .unwrap();
        revision[index] += 1;
    }
    let predecessor = ids[0].clone();
    db.commit(Commit {
        expected_head: db.current_head().unwrap(),
        mutations: vec![Mutation::Task {
            expected: Some(revision[0]),
            next: Task {
                id: predecessor.clone(),
                revision: revision[0] + 1,
                state: TaskState::Succeeded,
                title: format!("logical worker {}", predecessor.as_str()),
                active_attempt: None,
            },
        }],
    })
    .unwrap();
    assert_eq!(
        sql_count(&path, "SELECT count(*) FROM dependency_satisfactions"),
        0,
        "narrative success wrote a satisfaction"
    );
    assert_eq!(
        sql_count(&path, "SELECT count(*) FROM verified_results"),
        0
    );
    let before_policy = db.queue_report(now).unwrap();
    assert_eq!(
        before_policy.policy.max_active_workers, 0,
        "max_active_workers default changed"
    );
    assert!(!before_policy.launch_enabled);
    let evidence = format!(
        "verified_dependency_evidence_unavailable:{}:verified_result",
        predecessor.as_str()
    );
    assert!(before_policy.entries.iter().any(|entry| entry.task != predecessor
        && entry.blockers.iter().any(|blocker| blocker == &evidence)));

    let head = db
        .set_scheduler_policy(
            db.current_head().unwrap(),
            before_policy.policy.revision,
            u32::try_from(workers).unwrap(),
            before_policy.policy.max_attempts_per_task,
        )
        .unwrap();
    assert_eq!(head, db.current_head().unwrap());
    let open_slots = db.queue_report(now).unwrap();
    assert_eq!(open_slots.available_slots, workers);
    assert_eq!(open_slots.retained_attempts, 0);

    let attempts: Vec<Mutation> = ids
        .iter()
        .enumerate()
        .map(|(index, id)| Mutation::Attempt {
            expected: None,
            next: Attempt {
                id: AttemptId::new(format!("attempt-{index:04}")).unwrap(),
                task: id.clone(),
                revision: 1,
                state: AttemptState::Running,
                snapshot: None,
                reservation: format!("slot-{index:04}"),
                termination_observed: false,
            },
        })
        .collect();
    db.commit(Commit {
        expected_head: db.current_head().unwrap(),
        mutations: attempts,
    })
    .unwrap();
    insert_scale_inventory(&path,additional_attempts);
    // Restart with the combined workload present; the administrative opener
    // checks database integrity before exercising the active readers.
    drop(db);
    let mut db=SqliteStore::open(&path).unwrap();
    assert_eq!(sql_count(&path,"SELECT count(*) FROM runtime_bindings"),10_000+workers as i64);
    assert_eq!(sql_count(&path,"SELECT count(*) FROM attempts WHERE termination_observed=1"),10_000+additional_attempts as i64);
    let held = db.queue_report(now).unwrap();
    assert_eq!(held.retained_attempts, workers);
    assert_eq!(held.available_slots, 0);
    assert!(held
        .entries
        .iter()
        .filter(|entry| entry.task != predecessor)
        .all(|entry| entry.blockers.iter().any(|blocker| blocker == &evidence)));
    assert_eq!(
        sql_count(&path, "SELECT count(*) FROM dependency_satisfactions"),
        0
    );

    let admission: String = rusqlite::Connection::open(&path)
        .unwrap()
        .query_row(
            "SELECT factory_admission FROM project_control WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(admission, "off");
    // A row with no verified receipt must not clear the blocker.
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute(
            "INSERT INTO dependency_satisfactions(satisfaction_id,task_id,predecessor_task,requirement,state,evidence_kind,evidence_id,created_unix_ms) VALUES(?1,?2,?3,'verified_result','valid','verified_result',?4,?5)",
            rusqlite::params!["ab".repeat(32), ids[1].as_str(), predecessor.as_str(), "cd".repeat(32), now],
        )
        .unwrap();
    let forged = db.queue_report(now).unwrap();
    assert!(forged
        .entries
        .iter()
        .filter(|entry| entry.task != predecessor)
        .all(|entry| entry.blockers.iter().any(|blocker| blocker == &evidence)));
    assert_eq!(
        sql_count(&path, "SELECT count(*) FROM verified_results"),
        0
    );
    assert_eq!(forged.available_slots, 0);

    // One page that does not reach the end is not coverage and cannot release a slot.
    let stopped = db.reconcile_active_work(Some(1)).unwrap();
    assert!(!stopped.capacity_release_allowed, "slot released early");
    if workers >= herdr_projects::store::ACTIVE_WORK_PAGE {
        assert_eq!(
            stopped.coverage,
            herdr_projects::store::ActiveCoverage::Incomplete
        );
    }
    let covered = db.reconcile_active_work(None).unwrap();
    assert_eq!(
        covered.coverage,
        herdr_projects::store::ActiveCoverage::Complete
    );
    assert_eq!(covered.items.len(), workers);
    assert!(covered.items.iter().all(|item| item.retains_capacity));
    assert!(!covered.capacity_release_allowed, "slot released early");
    let seen = covered
        .items
        .iter()
        .map(|item| item.binding.id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    for id in &ids {
        assert!(seen.contains(&format!("task:{}", id.as_str())));
    }
    assert_eq!(
        db.read_targeted_hot_path(now, true).unwrap(),
        herdr_projects::store::SCHEMA
    );

    let cancelled = db
        .cancel_attempt(
            &AttemptId::new("attempt-0001").unwrap(),
            1,
            db.current_head().unwrap(),
            "stop does not release the slot",
            now,
        )
        .unwrap();
    assert!(!cancelled.released, "slot released early");
    let after_cancel = db.queue_report(now).unwrap();
    assert_eq!(after_cancel.retained_attempts, workers);
    assert_eq!(after_cancel.available_slots, 0);
    assert!(!db
        .reconcile_active_work(None)
        .unwrap()
        .capacity_release_allowed);

    db.commit(Commit {
        expected_head: db.current_head().unwrap(),
        mutations: vec![Mutation::Attempt {
            expected: Some(cancelled.attempt_revision),
            next: Attempt {
                id: cancelled.attempt.clone(),
                task: ids[1].clone(),
                revision: cancelled.attempt_revision + 1,
                state: AttemptState::Cancelled,
                snapshot: None,
                reservation: "slot-0001".into(),
                termination_observed: true,
            },
        }],
    })
    .unwrap();
    let released_one = db.queue_report(now).unwrap();
    assert_eq!(released_one.retained_attempts, workers - 1);
    assert_eq!(released_one.available_slots, 1);
    let after = db.reconcile_active_work(None).unwrap();
    assert_eq!(
        after.coverage,
        herdr_projects::store::ActiveCoverage::Complete
    );
    assert_eq!(after.items.len(), workers - 1);
    assert!(!after.capacity_release_allowed, "remaining slots released early");
    assert!(after
        .items
        .iter()
        .all(|item| item.binding.id != "task:w-0001"));
    assert!(released_one
        .entries
        .iter()
        .any(|entry| entry.task.as_str() == "w-0002"
            && entry.blockers.iter().any(|blocker| blocker == &evidence)));
    assert_eq!(
        sql_count(
            &path,
            "SELECT count(*) FROM attempts WHERE termination_observed=0"
        ),
        i64::try_from(workers - 1).unwrap()
    );
    assert_eq!(
        sql_count(
            &path,
            "SELECT count(*) FROM events WHERE kind='scale.history'"
        ),
        i64::try_from(history_events).unwrap()
    );
    assert!(sql_count(&path, "SELECT count(*) FROM events") > i64::try_from(history_events).unwrap());

    // Exercise the real enabled admission reader on this same workload after
    // cancellation releases one slot. Synthetic fixture activation supplies no
    // signed contracts or grants, so it must never reserve another attempt.
    drop(db);
    let connection=rusqlite::Connection::open(&path).unwrap();
    connection.execute("UPDATE project_control SET state='active',reconciliation_required=0,factory_admission='on' WHERE singleton=1",[]).unwrap();
    let before=sql_count(&path,"SELECT count(*) FROM attempts");
    let mut samples=Vec::new();
    let mut measured_work=None;
    for _ in 0..5 {
        let started=std::time::Instant::now();
        let observation=herdr_projects::admission::admit_decision_observed(tmp.path());
        let elapsed_us=started.elapsed().as_micros();
        let decision=observation.result.unwrap();
        assert_ne!(decision.reason,"reserved");
        assert!(observation.sql.connection_observed);
        assert!(observation.sql.sqlite_vm_steps>0);
        let counters=(observation.sql.sqlite_rows_returned,observation.sql.sqlite_vm_steps);
        if let Some(first)=measured_work {assert_eq!(counters,first,"reopening the same workload must not add SQL work");}
        measured_work=Some(counters);
        samples.push(serde_json::json!({"duration_us":elapsed_us,"sql_work":observation.sql,"reason":decision.reason}));
    }
    assert_eq!(sql_count(&path,"SELECT count(*) FROM attempts"),before);
    connection.execute("UPDATE project_control SET factory_admission='off' WHERE singleton=1",[]).unwrap();
    println!("scale_admission_sample {}",serde_json::json!({
        "workers":workers,"retained_capacity_after_cancellation":workers-1,"historical_events":history_events,
        "retired_bindings":10_000,"retirement_support_attempts":10_000,
        "additional_terminated_attempts":additional_attempts,"samples":samples,
        "scope":"unsigned candidate decision after cancellation; not launch or freshness certification"
    }));
    measured_work.unwrap()
}

#[test]
fn scale_gate_for_32_and_64_workers() {
    assert_eq!(
        herdr_projects::store::HOT_PATH_READ,
        herdr_projects::store::HotPathRead::Targeted
    );
    assert!(
        !herdr_projects::store::hot_path_uses_snapshot(),
        "hot path is still shadow / Snapshot"
    );
    let mut work_by_workers=BTreeMap::new();
    for history in [1_000usize, 100_000] {
        for workers in [32usize, 64] {
            for attempts in [256usize,1024] {
                let work=scale_case(history, workers, attempts);
                if let Some(previous)=work_by_workers.insert(workers,work) {
                    assert_eq!(work,previous,"historical event/attempt growth must not add admission SQL work");
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn operation(id: &str, task: &TaskId, key: &str, due: i64) -> Operation {
    Operation {
        id: OperationId::new(id).unwrap(),
        task: Some(task.clone()),
        kind: "notify".into(),
        target: "local".into(),
        payload_version: 1,
        payload: serde_json::json!({"notice": key}),
        expected_revision: 1,
        due_unix_ms: due,
        idempotency_key: key.into(),
    }
}

#[cfg(target_os = "linux")]
#[test]
fn fault_campaign_and_restore_rehearsal() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("campaign");
    fs::create_dir_all(project.join(".state")).unwrap();
    let db_path = project.join(".state/state.db");
    let (repo, oid) = build_repo(tmp.path(), SEED);
    let repo = fs::canonicalize(&repo).unwrap();
    let repository = repo.display().to_string();
    let object_path = format!("{}/{}", &oid[..2], &oid[2..]);
    assert!(repo.join(".git/objects").join(&object_path).is_file());

    let task_id = TaskId::new("t-old").unwrap();
    let old_attempt = AttemptId::new("attempt-old").unwrap();
    let due = unix_ms();
    let mut db = SqliteStore::create(&db_path).unwrap();
    assert_eq!(db.read_snapshot(None).unwrap().schema_version, herdr_projects::store::SCHEMA);
    let op = operation("op-1", &task_id, "env-1", due);
    db.commit(Commit {
        expected_head: 0,
        mutations: vec![
            Mutation::Task {
                expected: None,
                next: Task {
                    id: task_id.clone(),
                    revision: 1,
                    state: TaskState::Running,
                    title: "old generation".into(),
                    active_attempt: Some(old_attempt.clone()),
                },
            },
            Mutation::Attempt {
                expected: None,
                next: Attempt {
                    id: old_attempt.clone(),
                    task: task_id.clone(),
                    revision: 1,
                    state: AttemptState::Running,
                    snapshot: None,
                    reservation: "slot-old".into(),
                    termination_observed: false,
                },
            },
            Mutation::Enqueue(op.clone()),
        ],
    })
    .unwrap();
    let duplicate = db.commit(Commit {
        expected_head: db.current_head().unwrap(),
        mutations: vec![Mutation::Enqueue(operation("op-2", &task_id, "env-1", due))],
    });
    assert!(
        matches!(duplicate, Err(herdr_projects::store::StoreError::Conflict)),
        "{duplicate:?}"
    );
    assert_eq!(db.read_snapshot(None).unwrap().operations.len(), 1);
    let digest = format!("{:x}", Sha256::digest(b"proposal-a"));
    let first = db
        .insert_proposal(
            "prop-1",
            &digest,
            "t-old",
            "attempt-old",
            None,
            "rejected",
            "{}",
            "duplicate envelope",
            "[]",
            due,
        )
        .unwrap();
    assert!(!first.reused);
    let replayed = db
        .insert_proposal(
            "prop-1",
            &digest,
            "t-old",
            "attempt-old",
            None,
            "rejected",
            "{}",
            "duplicate envelope",
            "[]",
            due,
        )
        .unwrap();
    assert!(replayed.reused);
    let conflict = db.insert_proposal(
        "prop-1",
        &"ab".repeat(32),
        "t-old",
        "attempt-old",
        None,
        "rejected",
        "{\"other\":true}",
        "conflicting payload",
        "[]",
        due,
    );
    assert!(
        matches!(conflict, Err(herdr_projects::store::StoreError::Invalid(ref message)) if message.contains("idempotency")),
        "{conflict:?}"
    );
    assert_eq!(
        sql_count(&db_path, "SELECT count(*) FROM memory_proposals"),
        1
    );

    let claim = db
        .claim_operation(&op.id, 1, "campaign", due, 1_000)
        .unwrap();
    let before_cancel = db.queue_report(due).unwrap();
    assert_eq!(before_cancel.policy.max_active_workers, 0);
    assert_eq!(before_cancel.retained_attempts, 1);
    let cancelled = db
        .cancel_attempt(
            &old_attempt,
            1,
            db.current_head().unwrap(),
            "stop does not release the slot",
            due,
        )
        .unwrap();
    assert!(
        !cancelled.released,
        "slot released without termination evidence"
    );
    let after_cancel = db.queue_report(due).unwrap();
    assert_eq!(after_cancel.retained_attempts, 1);
    assert_eq!(after_cancel.available_slots, 0);
    assert!(db.validate_claim(&claim, due).is_err());
    assert!(db
        .finish_operation(
            &claim,
            herdr_projects::operations::Outcome::Confirmed {
                observed_identity: "old-attempt".into(),
            },
            due,
        )
        .is_err());
    assert_eq!(db.expire_claims(due + 1_000).unwrap(), 1);
    assert_eq!(db.expire_claims(due + 1_000).unwrap(), 0);
    let expired = db.read_snapshot(None).unwrap();
    let delivery = expired
        .deliveries
        .iter()
        .find(|row| row.operation == op.id)
        .unwrap();
    assert_eq!(
        delivery.state,
        herdr_projects::operations::DeliveryState::Ambiguous
    );
    assert_eq!(delivery.attempts, 1);
    assert!(db
        .claim_operation(&op.id, delivery.revision, "campaign", due + 1_000, 1_000)
        .is_err());
    assert_eq!(expired.operations.len(), 1);
    assert!(expired
        .attempts
        .iter()
        .any(|attempt| attempt.id == old_attempt && !attempt.termination_observed));

    db.commit(Commit {
        expected_head: db.current_head().unwrap(),
        mutations: vec![Mutation::Attempt {
            expected: Some(cancelled.attempt_revision),
            next: Attempt {
                id: old_attempt.clone(),
                task: task_id.clone(),
                revision: cancelled.attempt_revision + 1,
                state: AttemptState::Cancelled,
                snapshot: None,
                reservation: "slot-old".into(),
                termination_observed: true,
            },
        }],
    })
    .unwrap();
    let head = db.current_head().unwrap();
    let task = db
        .read_snapshot(None)
        .unwrap()
        .tasks
        .into_iter()
        .find(|task| task.id == task_id)
        .unwrap();
    let new_attempt = AttemptId::new("attempt-new").unwrap();
    db.commit(Commit {
        expected_head: head,
        mutations: vec![
            Mutation::Attempt {
                expected: None,
                next: Attempt {
                    id: new_attempt.clone(),
                    task: task_id.clone(),
                    revision: 1,
                    state: AttemptState::Running,
                    snapshot: None,
                    reservation: "slot-new".into(),
                    termination_observed: false,
                },
            },
            Mutation::Task {
                expected: Some(task.revision),
                next: Task {
                    id: task_id.clone(),
                    revision: task.revision + 1,
                    state: TaskState::Running,
                    title: "new generation".into(),
                    active_attempt: Some(new_attempt.clone()),
                },
            },
        ],
    })
    .unwrap();
    let retry = db.retry_infrastructure("t-old", "attempt-old").unwrap();
    assert_eq!(retry.attempt_id, "attempt-old");
    assert_eq!(retry.ordinal, 1);
    assert_eq!(sql_count(&db_path, "SELECT count(*) FROM attempts"), 2);
    drop(db);

    let digest =
        install_fixture_contract(&db_path, "t-old", &repository, &oid, "sha1", "src/old.rs", &[]);
    let submission = serde_json::json!({
        "idempotency_key": "submit-old",
        "task_id": "t-old",
        "contract_revision": 1,
        "contract_digest": digest,
        "attempt_id": "attempt-old",
        "repository": repository,
        "base_oid": oid,
        "candidate_oid": oid,
        "object_format": "sha1",
        "artifact_manifest": [{"path": "README", "oid": oid}],
        "claimed_checks": ["old attempt is diagnostic"],
        "objects": git(&repo, &["rev-list", "--objects", &oid]).lines().map(|line| {
            let oid = line.split_whitespace().next().unwrap();
            serde_json::json!({"oid":oid,"relative_path":format!("{}/{}", &oid[..2], &oid[2..])})
        }).collect::<Vec<_>>()
    });
    let raw = serde_json::to_vec(&submission).unwrap();
    let mut db = SqliteStore::open(&db_path).unwrap();
    let stored = db.submit_result(&raw).unwrap();
    assert!(!stored.replayed);
    let again = db.submit_result(&raw).unwrap();
    assert!(again.replayed);
    assert_eq!(again.submission_id, stored.submission_id);
    let mut conflicting = submission.clone();
    conflicting["claimed_checks"] = serde_json::json!(["different payload"]);
    let rejected = db.submit_result(&serde_json::to_vec(&conflicting).unwrap());
    assert!(
        matches!(rejected, Err(herdr_projects::store::StoreError::Conflict)),
        "{rejected:?}"
    );
    assert_eq!(
        sql_count(&db_path, "SELECT count(*) FROM result_submissions"),
        1
    );
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    let mismatch = work.join("policy.json");
    fs::write(&mismatch, b"{\"version\":1,\"checks\":[\"/bin/false\"]}").unwrap();
    let other = work.join("other-policy.json");
    fs::write(&other, b"{\"version\":1,\"checks\":[\"/bin/true\"]}").unwrap();
    let request = herdr_projects::verification::VerifyRequest::new(
        stored.submission_id.clone(),
        "builds",
        &mismatch,
        "verify-old",
        Duration::from_secs(5),
        &work,
    );
    let outcome = herdr_projects::verification::verify(&mut db, &request).unwrap();
    assert_eq!(outcome.state, "rejected");
    assert!(!outcome.replayed);
    let replay = herdr_projects::verification::verify(&mut db, &request).unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.run_id, outcome.run_id);
    let mut changed = request;
    changed.policy_path = other;
    match herdr_projects::verification::verify(&mut db, &changed) {
        Err(conflict) => assert!(
            conflict.to_string().contains("idempotency conflict"),
            "{conflict}"
        ),
        Ok(_) => panic!("conflicting verification payload was accepted"),
    }
    assert_eq!(
        sql_count(&db_path, "SELECT count(*) FROM feedback_items"),
        1
    );
    // Real verification supplies contract-check evidence; hand-written receipt
    // rows cannot stand in for the verifier's scope/output checks.
    let policy = tmp.path().join("accepted-policy.json");
    fs::write(&policy, PLANNING_POLICY).unwrap();
    let verify_receipt = |db: &mut SqliteStore, key: &str| {
        fs::create_dir(tmp.path().join(key)).unwrap();
        let request = herdr_projects::verification::VerifyRequest::new(
            stored.submission_id.clone(), "builds", &policy, key,
            Duration::from_secs(30), tmp.path().join(key),
        );
        let outcome = herdr_projects::verification::verify(db, &request).unwrap();
        assert_eq!(outcome.state, "accepted", "{:?}", outcome.reason);
        outcome.receipt.unwrap().result_id().to_string()
    };
    let point_active = |db: &mut SqliteStore, attempt: &AttemptId| {
        let snapshot = db.read_snapshot(None).unwrap();
        let task = snapshot
            .tasks
            .iter()
            .find(|task| task.id == task_id)
            .unwrap()
            .clone();
        db.commit(Commit {
            expected_head: snapshot.head,
            mutations: vec![Mutation::Task {
                expected: Some(task.revision),
                next: Task {
                    id: task_id.clone(),
                    revision: task.revision + 1,
                    state: TaskState::Running,
                    title: task.title,
                    active_attempt: Some(attempt.clone()),
                },
            }],
        })
        .unwrap();
    };
    // The stored writer accepts this receipt only while attempt-old is current.
    point_active(&mut db, &old_attempt);
    let first_old = verify_receipt(&mut db, "first-old-receipt");
    let need = TaskId::new("t-need").unwrap();
    db.commit(Commit {
        expected_head: db.current_head().unwrap(),
        mutations: vec![Mutation::Task {
            expected: None,
            next: Task {
                id: need.clone(),
                revision: 1,
                state: TaskState::Draft,
                title: "needs the predecessor".into(),
                active_attempt: None,
            },
        }],
    })
    .unwrap();
    let queued = db.read_snapshot(None).unwrap();
    db.queue_task(
        &need,
        1,
        queued.head,
        &QueueRequest {
            priority: 0,
            dependencies: vec![Dependency {
                predecessor: task_id.clone(),
                requirement: DependencyRequirement::VerifiedResult,
            }],
        },
        due,
    )
    .unwrap();
    let evidence = |query: &str| -> String {
        rusqlite::Connection::open(&db_path)
            .unwrap()
            .query_row(query, [], |row| row.get(0))
            .unwrap()
    };
    assert_eq!(
        evidence(
            "SELECT evidence_id FROM dependency_satisfactions WHERE task_id='t-need' AND state='valid'"
        ),
        first_old
    );
    assert_eq!(
        sql_count(
            &db_path,
            "SELECT count(*) FROM task_dependencies WHERE task_id='t-need' AND predecessor_id='t-old' AND requirement='verified_result'"
        ),
        1
    );
    let counted = db.queue_report(due).unwrap();
    assert!(counted.entries.iter().any(|entry| entry.task == need
        && entry
            .blockers
            .iter()
            .any(|blocker| blocker == "admission_disabled:verified_result")));
    point_active(&mut db, &new_attempt);
    let later_old = verify_receipt(&mut db, "later-old-receipt");
    assert_ne!(later_old, first_old);
    let again = db.read_snapshot(None).unwrap();
    let need_revision = again
        .tasks
        .iter()
        .find(|task| task.id == need)
        .unwrap()
        .revision;
    db.queue_task(
        &need,
        need_revision,
        again.head,
        &QueueRequest {
            priority: 1,
            dependencies: vec![Dependency {
                predecessor: task_id.clone(),
                requirement: DependencyRequirement::VerifiedResult,
            }],
        },
        due,
    )
    .unwrap();
    assert_eq!(
        evidence(
            "SELECT evidence_id FROM dependency_satisfactions WHERE task_id='t-need' AND state='valid'"
        ),
        first_old,
        "old attempt replaced the satisfaction"
    );
    assert_eq!(
        sql_count(
            &db_path,
            "SELECT count(*) FROM dependency_satisfactions WHERE task_id='t-need' AND state='valid'"
        ),
        1
    );
    let unavailable = db.queue_report(due).unwrap();
    let blockers = &unavailable
        .entries
        .iter()
        .find(|entry| entry.task == need)
        .unwrap()
        .blockers;
    assert!(blockers.iter().any(|blocker| {
        blocker == "verified_dependency_evidence_unavailable:t-old:verified_result"
    }));
    assert!(!blockers
        .iter()
        .any(|blocker| blocker == "admission_disabled:verified_result"));
    assert_eq!(
        db.read_snapshot(None)
            .unwrap()
            .tasks
            .iter()
            .find(|task| task.id == task_id)
            .unwrap()
            .active_attempt
            .as_ref(),
        Some(&new_attempt)
    );
    drop(db);
    assert_admission_off(&project);

    let barrier_path = tmp.path().join("barrier.db");
    SqliteStore::create(&barrier_path).unwrap();
    let alpha = seed_barrier_member(&barrier_path, "alpha");
    let beta = seed_barrier_member(&barrier_path, "beta");
    let mut db = SqliteStore::open(&barrier_path).unwrap();
    let head = db.read_snapshot(None).unwrap().head;
    let frozen_alpha = db.freeze_barrier(&[barrier_member(&alpha)], head).unwrap();
    let head = db.read_snapshot(None).unwrap().head;
    let frozen_beta = db.freeze_barrier(&[barrier_member(&beta)], head).unwrap();
    let head = db.read_snapshot(None).unwrap().head;
    let revoked = db.revoke_barrier(&frozen_alpha.barrier_id, head).unwrap();
    assert!(revoked.revoked_seq.is_some());
    let head = db.read_snapshot(None).unwrap().head;
    let release = db.release_barrier(
        &frozen_alpha.barrier_id,
        &frozen_alpha.release_token,
        head,
        1,
    );
    assert!(
        matches!(release, Err(herdr_projects::store::StoreError::Invalid(ref message)) if message.contains("revoked")),
        "{release:?}"
    );
    let head = db.read_snapshot(None).unwrap().head;
    let unrelated = db.freeze_barrier(&[barrier_member(&beta)], head).unwrap();
    assert_eq!(unrelated.barrier_id, frozen_beta.barrier_id);
    assert!(unrelated.revoked_seq.is_none());
    assert!(db
        .read_snapshot(None)
        .unwrap()
        .attempts
        .iter()
        .all(|attempt| !attempt.termination_observed));
    let admission: String = rusqlite::Connection::open(&barrier_path)
        .unwrap()
        .query_row(
            "SELECT factory_admission FROM project_control WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(admission, "off");
    drop(db);

    let root = tmp.path().join("alias-root");
    for name in ["a", "b", "sneaky"] {
        fs::create_dir_all(root.join(name).join(".state")).unwrap();
    }
    let git_dir = root.join("repos/real");
    fs::create_dir_all(&git_dir).unwrap();
    let alias = root.join("repos/alias");
    std::os::unix::fs::symlink(&git_dir, &alias).unwrap();
    let git_dir = fs::canonicalize(&git_dir).unwrap();
    assert_eq!(fs::canonicalize(&alias).unwrap(), git_dir);
    let footprint = |project: &Path| {
        vec![
            herdr_projects::execution_guard::Resource::new(
                "artifact",
                project.canonicalize().unwrap().to_str().unwrap(),
            )
            .unwrap(),
            herdr_projects::execution_guard::Resource::new("git", git_dir.to_str().unwrap())
                .unwrap(),
        ]
    };
    let owner = herdr_projects::execution_guard::ProjectSharedGuard::acquire(
        &root.join("a"),
        &footprint(&root.join("a")),
    )
    .unwrap();
    assert!(
        herdr_projects::execution_guard::ProjectSharedGuard::acquire(
            &root.join("b"),
            &footprint(&root.join("b"))
        )
        .is_err(),
        "aliased git directory accepted a second owner"
    );
    drop(owner);
    let project_lock =
        herdr_projects::execution_guard::ProjectGuard::acquire(&root.join("b")).unwrap();
    herdr_projects::execution_guard::ProjectSharedGuard::acquire(
        &root.join("a"),
        &footprint(&root.join("a")),
    )
    .expect("project lock on b took the aliased git fence");
    drop(project_lock);
    fs::remove_dir_all(root.join("sneaky/.state")).unwrap();
    std::os::unix::fs::symlink(root.join("a/.state"), root.join("sneaky/.state")).unwrap();
    assert!(herdr_projects::execution_guard::ProjectGuard::acquire(&root.join("sneaky")).is_err());
    assert!(
        herdr_projects::execution_guard::ProjectSharedGuard::acquire(
            &root.join("sneaky"),
            &footprint(&root.join("sneaky"))
        )
        .is_err()
    );

    let busy_project = tmp.path().join("busy");
    fs::create_dir_all(busy_project.join(".state")).unwrap();
    let busy_path = busy_project.join(".state/state.db");
    let mut db = SqliteStore::create(&busy_path).unwrap();
    let busy_task = TaskId::new("t-busy").unwrap();
    let busy_attempt = AttemptId::new("attempt-busy").unwrap();
    db.commit(Commit {
        expected_head: 0,
        mutations: vec![
            Mutation::Task {
                expected: None,
                next: Task {
                    id: busy_task.clone(),
                    revision: 1,
                    state: TaskState::Running,
                    title: "busy".into(),
                    active_attempt: Some(busy_attempt.clone()),
                },
            },
            Mutation::Attempt {
                expected: None,
                next: Attempt {
                    id: busy_attempt.clone(),
                    task: busy_task,
                    revision: 1,
                    state: AttemptState::Running,
                    snapshot: None,
                    reservation: "slot-busy".into(),
                    termination_observed: false,
                },
            },
        ],
    })
    .unwrap();
    let before = db.read_snapshot(None).unwrap();
    let raw = rusqlite::Connection::open(&busy_path).unwrap();
    raw.execute_batch("BEGIN IMMEDIATE").unwrap();
    let started = std::time::Instant::now();
    let busy = db.commit(Commit {
        expected_head: before.head,
        mutations: vec![Mutation::Task {
            expected: None,
            next: Task {
                id: TaskId::new("t-busy-2").unwrap(),
                revision: 1,
                state: TaskState::Draft,
                title: "blocked write".into(),
                active_attempt: None,
            },
        }],
    });
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(
        matches!(busy, Err(herdr_projects::store::StoreError::Busy)),
        "{busy:?}"
    );
    assert_eq!(db.read_snapshot(None).unwrap().tasks, before.tasks);
    raw.execute_batch("ROLLBACK").unwrap();
    drop(raw);
    assert!(herdr_projects::watchdog::note(
        &busy_project,
        &herdr_projects::store::StoreError::Busy
    )
    .unwrap());
    assert_eq!(
        herdr_projects::watchdog::pause_reason(&busy_project),
        Some("database_busy")
    );
    assert_eq!(sql_count(&busy_path, "SELECT count(*) FROM attempts"), 1);
    assert_eq!(
        sql_count(
            &busy_path,
            "SELECT count(*) FROM attempts WHERE id='attempt-busy'"
        ),
        1
    );
    assert_eq!(db.read_snapshot(None).unwrap().attempts.len(), 1);
    drop(db);

    let published = memory_project();
    let published_db = state_db(&published.project);
    let old_store = fs::canonicalize(&published_db)
        .unwrap()
        .display()
        .to_string();
    let bad = published.tmp.path().join("bad-contract.json");
    fs::write(&bad, b"{}\n").unwrap();
    assert!(Command::new("/usr/bin/ssh-keygen")
        .args(["-Y", "sign", "-f"])
        .arg(&published.key)
        .args(["-n", herdr_projects::authority::SIGNATURE_NAMESPACE])
        .arg(&bad)
        .status()
        .unwrap()
        .success());
    let signature = PathBuf::from(format!("{}.sig", bad.display()));
    assert!(
        herdr_projects::authority::import_contract(&published.project, &bad, &signature).is_err()
    );
    let denials = herdr_projects::authority::denials(&published.project).unwrap();
    assert!(denials.iter().any(|denial| {
        denial.class == "contract"
            && denial.command == "put"
            && denial.reason_code == "signature_failed"
    }));
    assert_eq!(
        sql_count(&published_db, "SELECT count(*) FROM task_contracts"),
        0
    );
    assert_admission_off(&published.project);
    let live_bytes = fs::read(&published_db).unwrap();

    let destination = published.tmp.path().join("restored");
    herdr_projects::migration::restore_backup(&published.project, &destination).unwrap();
    assert!(!destination.join(".state/state.db").exists());
    assert!(!destination.join(".state/format.json").exists());
    assert_eq!(fs::read(&published_db).unwrap(), live_bytes);
    assert!(herdr_projects::migration::restore_backup(&published.project, &destination).is_err());
    let new_store = destination.join(".state/state.db");
    assert!(new_store.is_absolute());
    assert!(!new_store.exists());
    assert_ne!(new_store.display().to_string(), old_store);
    let config_path = tmp.path().join("grant-owner.toml");
    fs::write(&config_path, "version = 1\n").unwrap();
    let config = herdr_projects::migration::config_reference(&config_path).unwrap();
    let profile = plant_profile(&db_path, &config);
    let mut inputs: LaunchInputs =
        serde_json::from_str(include_str!("fixtures/launch-inputs-v1.json")).unwrap();
    inputs.version = 2;
    inputs.project_store = old_store.clone();
    inputs.profile = profile.reference().unwrap();
    inputs.effective_profile = Some(profile.clone());
    let now = unix_ms();
    let grant = ApprovalGrant {
        version: 1,
        scope: ApprovalScope::for_launch(&inputs).unwrap(),
        policy: profile.permission_policy.clone(),
        issued_unix_ms: now - 1_000,
        expires_unix_ms: now + 3_600_000,
    };
    inputs.approval = grant.reference().unwrap();
    grant
        .matches_launch(&inputs, &old_store, now)
        .expect("old grant matches its own store");
    let moved = grant.matches_launch(&inputs, &new_store.display().to_string(), now);
    assert!(
        matches!(moved, Err(ref message) if message.contains("different action")),
        "{moved:?}"
    );
    let mut reissue = inputs.clone();
    reissue.project_store = new_store.display().to_string();
    assert!(grant
        .matches_launch(&reissue, &reissue.project_store, now)
        .is_err());
    assert_ne!(
        ApprovalScope::for_launch(&reissue).unwrap().project_store,
        grant.scope.project_store
    );

    fs::remove_file(published.project.join(".state/format.json")).unwrap();
    assert!(herdr_projects::migration::open_active(&published.project).is_err());
    assert!(herdr_projects::migration::recover(&published.project, true).is_err());
    assert_eq!(fs::read(&published_db).unwrap(), live_bytes);
    assert!(published_db.is_file());
    assert_eq!(
        herdr_projects::migration::status(&published.project)
            .unwrap()
            .phase,
        herdr_projects::migration::Phase::Active
    );
    assert!(published
        .project
        .join(".state/migration/journal.json")
        .is_file());
    assert!(herdr_projects::migration::abort(&published.project).is_err());
}

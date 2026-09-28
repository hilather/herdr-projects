use crate::execution_guard::GatedSpawn;
use super::*;
use crate::domain::*;
use crate::store::SqliteStore;
use std::{fs, path::Path, process::Command};

const TARGET: &str = "refs/heads/integration";

fn git(repo: &Path, args: &[&str]) {
    let banned = ["push", "fetch", "pull", "clone", "ls-remote", "remote"];
    assert!(
        !args.iter().any(|arg| {
            banned.contains(arg) || arg.contains("://") || arg.starts_with("git@")
        }),
        "network git command: {args:?}"
    );
    let status = Command::new("/usr/bin/git")
        .args(args)
        .current_dir(repo)
        .env("GIT_AUTHOR_NAME", "integrator")
        .env("GIT_AUTHOR_EMAIL", "integrator@example.com")
        .env("GIT_COMMITTER_NAME", "integrator")
        .env("GIT_COMMITTER_EMAIL", "integrator@example.com")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .status_gated()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn git_ok(repo: &Path, args: &[&str]) -> bool {
    Command::new("/usr/bin/git")
        .args(args)
        .current_dir(repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .status_gated()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn git_text(repo: &Path, args: &[&str]) -> String {
    git(repo, args);
    let output = Command::new("/usr/bin/git")
        .args(args)
        .current_dir(repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output_gated()
        .unwrap();
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

struct Fixture {
    _root: tempfile::TempDir,
    _work: tempfile::TempDir,
    db_path: PathBuf,
    repo: PathBuf,
    work: PathBuf,
    base: String,
    verified: String,
    main_oid: String,
    head_branch: String,
    worker_oid: String,
    result_id: String,
}

fn fixture() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let db_path = root.path().join("state.db");
    let mut db = SqliteStore::create(&db_path).unwrap();
    let task = TaskId::new("task").unwrap();
    let pred = TaskId::new("pred").unwrap();
    let attempt = AttemptId::new("attempt-1").unwrap();
    db.commit(Commit {
        expected_head: 0,
        mutations: vec![
            Mutation::Task {
                expected: None,
                next: Task {
                    id: task.clone(),
                    revision: 1,
                    state: TaskState::Running,
                    title: "integrate".into(),
                    active_attempt: None,
                },
            },
            Mutation::Task {
                expected: None,
                next: Task {
                    id: pred.clone(),
                    revision: 1,
                    state: TaskState::Draft,
                    title: "pred".into(),
                    active_attempt: None,
                },
            },
            Mutation::Attempt {
                expected: None,
                next: Attempt {
                    id: attempt.clone(),
                    task: task.clone(),
                    revision: 1,
                    state: AttemptState::Running,
                    snapshot: None,
                    reservation: "slot-1".into(),
                    termination_observed: false,
                },
            },
        ],
    })
    .unwrap();
    let head = db.read_snapshot(None).unwrap().head;
    let repo = root.path().join("repo");
    let template = root.path().join("template");
    fs::create_dir_all(&template).unwrap();
    git(
        root.path(),
        &[
            "init",
            "--template",
            template.to_str().unwrap(),
            repo.to_str().unwrap(),
        ],
    );
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(repo.join("src/file.txt"), b"base\n").unwrap();
    git(&repo, &["add", "src/file.txt"]);
    git(&repo, &["commit", "-m", "base"]);
    let head_branch = git_text(&repo, &["symbolic-ref", "HEAD"]);
    assert_ne!(head_branch, TARGET);
    git(&repo, &["branch", "integration"]);
    git(&repo, &["checkout", "-b", "worker"]);
    fs::write(repo.join("src/other.txt"), b"verified\n").unwrap();
    git(&repo, &["add", "src/other.txt"]);
    git(&repo, &["commit", "-m", "verified"]);
    let verified = git_text(&repo, &["rev-parse", "HEAD"]);
    git(
        &repo,
        &["checkout", head_branch.strip_prefix("refs/heads/").unwrap()],
    );
    let base = git_text(&repo, &["rev-parse", TARGET]);
    let main_oid = git_text(&repo, &["rev-parse", "HEAD"]);
    assert_eq!(base, main_oid);
    let repo = repo.canonicalize().unwrap();
    let db_canon = db_path.canonicalize().unwrap();
    let policy = serde_json::json!({
        "version": 1,
        "checks": ["/usr/bin/git", "rev-parse", "HEAD"]
    })
    .to_string();
    let mut bytes = serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "project_store": db_canon,
        "expected_head": head,
        "task_id": task.as_str(),
        "contract_revision": 1,
        "deliverable": "integrated commit",
        "non_goals": "no dependency satisfaction",
        "acceptance_policies": [{"id": "builds", "text": policy}],
        "repository": repo,
        "base_oid": base,
        "object_format": "sha1",
        "dependencies": [],
        "capability_flags": [],
        "profile_kind": "codex",
        "retry_class": "none",
        "result_schema_id": "result-v1",
        "route": "verify_then_integrate",
        "authority": {"id": "owner-approval-policy", "revision": 1, "digest": "ab".repeat(32)}
    }))
    .unwrap();
    bytes.push(b'\n');
    let prepared = PreparedContract::parse_verified(&bytes).unwrap();
    let installed = db.install_contract(&prepared).unwrap();
    let objects = [base.clone(), verified.clone()]
        .into_iter()
        .map(|oid| {
            let relative = format!("{}/{}", &oid[..2], &oid[2..]);
            assert!(repo.join(".git/objects").join(&relative).is_file(), "{relative}");
            serde_json::json!({"oid": oid, "relative_path": relative})
        })
        .collect::<Vec<_>>();
    let submission = serde_json::json!({
        "idempotency_key": "submit-1",
        "task_id": task.as_str(),
        "contract_revision": 1,
        "contract_digest": installed.digest,
        "attempt_id": attempt.as_str(),
        "repository": repo,
        "base_oid": base,
        "candidate_oid": verified,
        "object_format": "sha1",
        "artifact_manifest": [{"path": "src/other.txt", "oid": verified}],
        "claimed_checks": ["ignored"],
        "objects": objects
    });
    let receipt = db
        .submit_result(&serde_json::to_vec(&submission).unwrap())
        .unwrap();
    let tree = git_text(&repo, &["rev-parse", &format!("{verified}^{{tree}}")]);
    let result_id = db
        .testing_accept_verified_result(&receipt.submission_id, "builds", &verified, &tree)
        .unwrap();
    db.testing_insert_dependency("task", "pred", "landed_commit")
        .unwrap();
    db.testing_insert_dependency("pred", "task", "integration_candidate")
        .unwrap();
    configure_integration_ref(&mut db, &repo, TARGET).unwrap();
    drop(db);
    let work_path = work.path().to_path_buf();
    Fixture {
        _root: root,
        _work: work,
        db_path,
        repo,
        work: work_path,
        base,
        verified: verified.clone(),
        main_oid,
        head_branch,
        worker_oid: verified,
        result_id,
    }
}

fn request(fixture: &Fixture, key: &str, fault: Fault) -> IntegrateRequest {
    IntegrateRequest {
        result_id: fixture.result_id.clone(),
        idempotency_key: key.into(),
        repository: fixture.repo.clone(),
        work_dir: fixture.work.clone(),
        fault,
    }
}

fn open(fixture: &Fixture) -> SqliteStore {
    SqliteStore::open(&fixture.db_path).unwrap()
}

fn assert_local(fixture: &Fixture) {
    let config = fs::read_to_string(fixture.repo.join(".git/config")).unwrap_or_default();
    assert!(!config.contains("[remote "), "{config}");
    assert_eq!(git_text(&fixture.repo, &["rev-parse", "HEAD"]), fixture.main_oid);
    assert_eq!(
        git_text(&fixture.repo, &["rev-parse", "refs/heads/worker"]),
        fixture.worker_oid
    );
}

fn dependencies(db: &SqliteStore) -> Vec<(String, String, String)> {
    db.testing_dependencies().unwrap()
}

#[test]
fn stale_base_does_not_publish() {
    let fixture = fixture();
    let mut db = open(&fixture);
    let before = dependencies(&db);
    let outcome = integrate(&mut db, &request(&fixture, "integrate-stale", Fault::StaleBase)).unwrap();
    assert_eq!(outcome.state, "discarded");
    assert_eq!(outcome.reason.as_deref(), Some("stale_base"));
    let published = outcome.commit_oid.expect("candidate oid");
    assert_ne!(git_text(&fixture.repo, &["rev-parse", TARGET]), published);
    assert!(db.testing_integrated_oids().unwrap().is_empty());
    assert_eq!(
        db.testing_candidate_states("integrate-stale").unwrap(),
        vec![("discarded".into(), published)]
    );
    assert_eq!(dependencies(&db), before);
    assert_local(&fixture);
}

#[test]
fn packed_ancestor_tree_still_publishes() {
    let fixture = fixture();
    git(&fixture.repo, &["repack", "-a", "-d"]);
    let tree = git_text(
        &fixture.repo,
        &["rev-parse", &format!("{}^{{tree}}", fixture.base)],
    );
    let loose = fixture
        .repo
        .join(".git/objects")
        .join(&tree[..2])
        .join(&tree[2..]);
    if loose.exists() {
        fs::remove_file(&loose).unwrap();
    }
    git(&fixture.repo, &["cat-file", "-t", &tree]);
    let mut db = open(&fixture);
    let outcome = integrate(&mut db, &request(&fixture, "integrate-pack", Fault::None)).unwrap();
    assert_eq!(outcome.state, "integrated");
    assert_eq!(
        git_text(&fixture.repo, &["rev-parse", TARGET]),
        outcome.commit_oid.unwrap()
    );
    assert_local(&fixture);
}

#[test]
fn missing_candidate_object_discards_prepared_generation() {
    let fixture = fixture();
    let mut db = open(&fixture);
    let prepared = integrate(
        &mut db,
        &request(&fixture, "integrate-missing", Fault::CrashBeforeChecks),
    )
    .unwrap();
    assert_eq!(prepared.state, "candidate_prepared");
    let oid = prepared.commit_oid.expect("candidate");
    // Remove only the candidate commit. Its tree may be shared with the verified commit.
    let path = fixture
        .repo
        .join(".git/objects")
        .join(&oid[..2])
        .join(&oid[2..]);
    fs::remove_file(&path).unwrap();
    assert!(!git_ok(&fixture.repo, &["cat-file", "-e", &oid]));
    let discarded = integrate(&mut db, &request(&fixture, "integrate-missing", Fault::None)).unwrap();
    assert_eq!(discarded.state, "discarded");
    assert_eq!(discarded.reason.as_deref(), Some("candidate_missing"));
    assert_eq!(git_text(&fixture.repo, &["rev-parse", TARGET]), fixture.base);
    let published = integrate(&mut db, &request(&fixture, "integrate-again", Fault::None)).unwrap();
    assert_eq!(published.state, "integrated");
    assert_local(&fixture);
}

#[test]
fn reconcile_keeps_prepared_generation_when_objects_exist() {
    let fixture = fixture();
    let mut db = open(&fixture);
    let prepared = integrate(
        &mut db,
        &request(&fixture, "integrate-reconcile", Fault::CrashBeforeChecks),
    )
    .unwrap();
    assert_eq!(prepared.state, "candidate_prepared");
    let oid = prepared.commit_oid.clone().expect("candidate");
    let reconciled = reconcile_integration(&mut db, &fixture.repo, "integrate-reconcile").unwrap();
    assert_eq!(reconciled.state, "candidate_prepared");
    assert_eq!(reconciled.commit_oid.as_deref(), Some(oid.as_str()));
    assert_eq!(git_text(&fixture.repo, &["rev-parse", TARGET]), fixture.base);
    let published = integrate(&mut db, &request(&fixture, "integrate-reconcile", Fault::None)).unwrap();
    assert_eq!(published.state, "integrated");
    assert_eq!(published.commit_oid.as_deref(), Some(oid.as_str()));
    assert_local(&fixture);
}

#[test]
fn expired_claim_after_build_still_records() {
    let fixture = fixture();
    let mut db = open(&fixture);
    let outcome = integrate(
        &mut db,
        &request(&fixture, "integrate-renew", Fault::ExpireBeforeRecord),
    )
    .unwrap();
    assert_eq!(outcome.state, "integrated");
    assert_ne!(outcome.commit_oid.as_deref(), Some(fixture.base.as_str()));
    let failed = integrate(&mut db, &request(&fixture, "integrate-fail", Fault::FailBuild));
    assert!(failed.is_err());
    let resumed = integrate(&mut db, &request(&fixture, "integrate-fail", Fault::None)).unwrap();
    assert_eq!(resumed.state, "integrated");
    assert_local(&fixture);
}

#[test]
fn retry_integrate_confirms_candidate_already_at_ref() {
    let fixture = fixture();
    let mut db = open(&fixture);
    let outcome = integrate(
        &mut db,
        &request(&fixture, "integrate-retry", Fault::CrashAfterRefUpdate),
    )
    .unwrap();
    let new_oid = outcome.commit_oid.expect("candidate oid");
    assert_eq!(git_text(&fixture.repo, &["rev-parse", TARGET]), new_oid);
    assert!(db.testing_integrated_oids().unwrap().is_empty());
    let confirmed = integrate(&mut db, &request(&fixture, "integrate-retry", Fault::None)).unwrap();
    assert_eq!(confirmed.state, "integrated");
    assert_eq!(confirmed.commit_oid.as_deref(), Some(new_oid.as_str()));
    assert_eq!(db.testing_integrated_oids().unwrap(), vec![new_oid.clone()]);
    assert_eq!(git_text(&fixture.repo, &["rev-parse", TARGET]), new_oid);
    assert_local(&fixture);
}

#[test]
fn update_ref_failure_at_old_oid_stays_retryable() {
    let fixture = fixture();
    let mut db = open(&fixture);
    let outcome = integrate(
        &mut db,
        &request(&fixture, "integrate-reject", Fault::UpdateRefRejected),
    )
    .unwrap();
    assert_eq!(outcome.state, "validating");
    assert_eq!(git_text(&fixture.repo, &["rev-parse", TARGET]), fixture.base);
    assert!(db.testing_integrated_oids().unwrap().is_empty());
    let confirmed = integrate(&mut db, &request(&fixture, "integrate-reject", Fault::None)).unwrap();
    assert_eq!(confirmed.state, "integrated");
    assert_ne!(confirmed.commit_oid.as_deref(), Some(fixture.base.as_str()));
    assert_eq!(
        git_text(&fixture.repo, &["rev-parse", TARGET]),
        confirmed.commit_oid.unwrap()
    );
    assert_local(&fixture);
}

#[test]
fn expired_claim_confirms_exact_oid_without_claimed_state() {
    let fixture = fixture();
    let mut db = open(&fixture);
    let outcome = integrate(
        &mut db,
        &request(&fixture, "integrate-expired", Fault::CrashAfterRefUpdate),
    )
    .unwrap();
    let new_oid = outcome.commit_oid.expect("candidate oid");
    db.testing_expire_lease(&outcome.operation_id).unwrap();
    assert_eq!(db.expire_claims(1).unwrap(), 1);
    let confirmed = integrate(&mut db, &request(&fixture, "integrate-expired", Fault::None)).unwrap();
    assert_eq!(confirmed.state, "integrated");
    assert_eq!(confirmed.commit_oid.as_deref(), Some(new_oid.as_str()));
    assert_eq!(git_text(&fixture.repo, &["rev-parse", TARGET]), new_oid);
    assert_local(&fixture);
}

#[test]
fn crash_before_checks_resumes_or_discards_instead_of_staying_pending() {
    let fixture = fixture();
    let mut db = open(&fixture);
    let pending = integrate(
        &mut db,
        &request(&fixture, "integrate-pending", Fault::CrashBeforeBuild),
    )
    .unwrap();
    assert_eq!(pending.state, "effect_pending");
    let resumed = integrate(&mut db, &request(&fixture, "integrate-pending", Fault::None)).unwrap();
    assert_ne!(resumed.state, "effect_pending");
    assert_ne!(resumed.state, "candidate_prepared");
    assert_eq!(resumed.state, "integrated");

    let prepared = integrate(
        &mut db,
        &request(&fixture, "integrate-prepared", Fault::CrashBeforeChecks),
    )
    .unwrap();
    assert_eq!(prepared.state, "candidate_prepared");
    db.testing_expire_lease(&prepared.operation_id).unwrap();
    db.expire_claims(1).unwrap();
    let again = integrate(&mut db, &request(&fixture, "integrate-prepared", Fault::None)).unwrap();
    assert_ne!(again.state, "effect_pending");
    assert_ne!(again.state, "candidate_prepared");
    assert_eq!(again.state, "integrated");
    assert_local(&fixture);
}

#[test]
fn crash_after_ref_update_confirms_only_exact_new_oid() {
    let fixture = fixture();
    let mut db = open(&fixture);
    let before = dependencies(&db);
    let outcome = integrate(
        &mut db,
        &request(&fixture, "integrate-crash", Fault::CrashAfterRefUpdate),
    )
    .unwrap();
    assert_eq!(outcome.state, "validating");
    let new_oid = outcome.commit_oid.expect("candidate oid");
    assert_eq!(git_text(&fixture.repo, &["rev-parse", TARGET]), new_oid);
    assert!(db.testing_integrated_oids().unwrap().is_empty());
    let confirmed = reconcile_integration(&mut db, &fixture.repo, "integrate-crash").unwrap();
    assert_eq!(confirmed.state, "integrated");
    assert_eq!(confirmed.commit_oid.as_deref(), Some(new_oid.as_str()));
    assert_eq!(db.testing_integrated_oids().unwrap(), vec![new_oid.clone()]);
    assert_eq!(git_text(&fixture.repo, &["rev-parse", TARGET]), new_oid);
    assert_eq!(git_text(&fixture.repo, &["rev-parse", &format!("{new_oid}^1")]), fixture.base);
    assert_eq!(
        git_text(&fixture.repo, &["rev-parse", &format!("{new_oid}^2")]),
        fixture.verified
    );
    assert_eq!(dependencies(&db), before);
    assert_local(&fixture);
}

#[test]
fn ambiguous_ref_enters_reconciliation_required() {
    let fixture = fixture();
    let mut db = open(&fixture);
    let outcome = integrate(
        &mut db,
        &request(&fixture, "integrate-ambiguous", Fault::CrashAfterRefUpdate),
    )
    .unwrap();
    let new_oid = outcome.commit_oid.expect("candidate oid");
    assert_eq!(git_text(&fixture.repo, &["rev-parse", TARGET]), new_oid);
    let elsewhere = git::GitRepo::open(&fixture.repo)
        .unwrap()
        .advance_ref(TARGET, &new_oid)
        .unwrap();
    assert_ne!(elsewhere, new_oid);
    let reconciled = reconcile_integration(&mut db, &fixture.repo, "integrate-ambiguous").unwrap();
    assert_eq!(reconciled.state, "reconciliation_required");
    assert_eq!(reconciled.reason.as_deref(), Some("ambiguous_ref"));
    assert!(db.testing_integrated_oids().unwrap().is_empty());
    assert_eq!(git_text(&fixture.repo, &["rev-parse", TARGET]), elsewhere);
    let blocked = integrate(&mut db, &request(&fixture, "integrate-other", Fault::None));
    assert!(blocked.is_err(), "a second generation must not publish over an ambiguous ref");
    assert!(db.testing_integrated_oids().unwrap().is_empty());
    assert_eq!(git_text(&fixture.repo, &["rev-parse", TARGET]), elsewhere);
    assert_local(&fixture);
}

#[test]
fn refuses_checked_out_ref_and_missing_configuration() {
    let fixture = fixture();
    let fresh_path = fixture._root.path().join("fresh.db");
    let mut fresh = SqliteStore::create(&fresh_path).unwrap();
    let missing = integrate(
        &mut fresh,
        &IntegrateRequest {
            result_id: fixture.result_id.clone(),
            idempotency_key: "missing".into(),
            repository: fixture.repo.clone(),
            work_dir: fixture.work.clone(),
            fault: Fault::None,
        },
    )
    .unwrap_err()
    .to_string();
    assert!(
        missing.contains("integration ref is not configured"),
        "{missing}"
    );
    assert_eq!(git_text(&fixture.repo, &["rev-parse", TARGET]), fixture.base);
    let mut db = open(&fixture);
    let held = git_text(&fixture.repo, &["rev-parse", TARGET]);
    git(&fixture.repo, &["checkout", "integration"]);
    let refused = integrate(&mut db, &request(&fixture, "checked-out", Fault::None))
        .unwrap_err()
        .to_string();
    assert!(refused.contains("integration ref is checked out"), "{refused}");
    assert_eq!(git_text(&fixture.repo, &["rev-parse", TARGET]), held);
    assert!(db.testing_integrated_oids().unwrap().is_empty());
    git(
        &fixture.repo,
        &[
            "checkout",
            fixture.head_branch.strip_prefix("refs/heads/").unwrap(),
        ],
    );
    assert_local(&fixture);
}

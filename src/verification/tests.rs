use super::*;
use crate::domain::*;
use crate::store::SqliteStore;
use std::{
    fs,
    os::unix::{process::CommandExt,fs::MetadataExt},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

struct World {
    _root: tempfile::TempDir,
    _work: tempfile::TempDir,
    db_path: PathBuf,
    repo: PathBuf,
    work: PathBuf,
    head: u64,
    oid: String,
    task: TaskId,
    attempt: AttemptId,
}

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("/usr/bin/git")
        .args(args)
        .current_dir(repo)
        .env("GIT_AUTHOR_NAME", "verifier")
        .env("GIT_AUTHOR_EMAIL", "verifier@example.com")
        .env("GIT_COMMITTER_NAME", "verifier")
        .env("GIT_COMMITTER_EMAIL", "verifier@example.com")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn compile_probe(dest: &Path) {
    let source = dest.with_extension("c");
    fs::write(
        &source,
        r#"
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
int main(int argc, char **argv) {
    if (argc < 2) return 2;
    if (strcmp(argv[1], "sleep") == 0) { sleep(30); return 0; }
    if (strcmp(argv[1], "orphan") == 0) {
        pid_t pid = fork();
        if (pid < 0) return 3;
        if (pid == 0) { close(0); close(1); close(2); sleep(30); _exit(0); }
        return 0;
    }
    if (strcmp(argv[1], "probe") == 0) {
        if (argc != 5) return 2;
        for (int i = 2; i < 5; i++) {
            errno = 0;
            int fd = open(argv[i], O_RDONLY);
            int err = fd < 0 ? errno : 0;
            if (fd >= 0) close(fd);
            printf("%d\n", err);
            if (err != ENOENT) return 4;
        }
        return 0;
    }
    return 2;
}
"#,
    )
    .unwrap();
    let status = Command::new("gcc")
        .args(["-O2", "-o"])
        .arg(dest)
        .arg(&source)
        .status()
        .unwrap();
    assert!(status.success(), "gcc probe");
}

fn loose_objects(repo: &Path) -> Vec<(String, String, Vec<u8>)> {
    let mut found = Vec::new();
    for prefix in fs::read_dir(repo.join(".git/objects")).unwrap() {
        let prefix = prefix.unwrap();
        let name = prefix.file_name().into_string().unwrap();
        if !prefix.file_type().unwrap().is_dir()
            || name.len() != 2
            || name == "info"
            || name == "pack"
        {
            continue;
        }
        for file in fs::read_dir(prefix.path()).unwrap() {
            let file = file.unwrap();
            let rest = file.file_name().into_string().unwrap();
            found.push((
                format!("{name}{rest}"),
                format!("{name}/{rest}"),
                fs::read(file.path()).unwrap(),
            ));
        }
    }
    found
}

fn world(with_probe: bool) -> World {
    let root = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let db_path = root.path().join("state.db");
    let mut db = SqliteStore::create(&db_path).unwrap();
    let task = TaskId::new("task").unwrap();
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
                    title: "verify".into(),
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
    drop(db);
    let repo = root.path().join("repo");
    let template = root.path().join("template");
    fs::create_dir_all(&repo).unwrap();
    fs::create_dir_all(&template).unwrap();
    git(
        &repo,
        &[
            "init",
            "--template",
            template.to_str().unwrap(),
            repo.to_str().unwrap(),
        ],
    );
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(repo.join("src/file.txt"), b"hello\n").unwrap();
    if with_probe {
        compile_probe(&repo.join("probe"));
    }
    git(
        &repo,
        &[
            "-c",
            "user.name=verifier",
            "-c",
            "user.email=verifier@example.com",
            "add",
            "src/file.txt",
        ],
    );
    if with_probe {
        git(
            &repo,
            &[
                "-c",
                "user.name=verifier",
                "-c",
                "user.email=verifier@example.com",
                "add",
                "probe",
            ],
        );
    }
    git(
        &repo,
        &[
            "-c",
            "user.name=verifier",
            "-c",
            "user.email=verifier@example.com",
            "commit",
            "-m",
            "first",
        ],
    );
    let oid = String::from_utf8(
        Command::new("/usr/bin/git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&repo)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let oid = oid.trim().to_string();
    assert_eq!(oid.len(), 40, "expected a sha1 git repository");
    let work_path = work.path().to_path_buf();
    World {
        _root: root,
        _work: work,
        db_path,
        repo,
        work: work_path,
        head,
        oid,
        task,
        attempt,
    }
}

fn install_and_submit(world: &World, policy_body: &str) -> (String, String) {
    install_and_submit_outputs(world, policy_body, None)
}

fn install_and_submit_outputs(world: &World, policy_body: &str, outputs: Option<&[&str]>) -> (String, String) {
    let db_path = world.db_path.canonicalize().unwrap();
    let repo = world.repo.canonicalize().unwrap();
    let mut document = serde_json::json!({
        "version": 1,
        "project_store": db_path,
        "expected_head": world.head,
        "task_id": world.task.as_str(),
        "contract_revision": 1,
        "deliverable": "verified checkout",
        "non_goals": "no dependency release",
        "acceptance_policies": [{"id": "builds", "text": policy_body}],
        "repository": repo,
        "base_oid": world.oid,
        "object_format": "sha1",
        "dependencies": [],
        "capability_flags": [],
        "profile_kind": "codex",
        "retry_class": "none",
        "result_schema_id": "result-v1",
        "route": "verify_only",
        "authority": {"id": "owner-approval-policy", "revision": 1, "digest": "ab".repeat(32)}
    });
    if let Some(outputs) = outputs {
        document["version"] = 3.into();
        document["outputs"] = serde_json::json!(outputs.iter().map(|path| serde_json::json!({"path":path,"kind":"git_file"})).collect::<Vec<_>>());
        document["scope"] = serde_json::json!({"paths":[{"path":"src/","access":"write"}]});
    }
    let mut bytes = serde_json::to_vec(&document).unwrap();
    bytes.push(b'\n');
    let mut db = SqliteStore::open(&world.db_path).unwrap();
    let prepared = PreparedContract::parse_verified(&bytes).unwrap();
    let installed = db.install_contract(&prepared).unwrap();
    let objects = loose_objects(&repo);
    assert!(objects.iter().any(|(oid, _, _)| oid == &world.oid));
    let submission = serde_json::json!({
        "idempotency_key": "submit-1",
        "task_id": world.task.as_str(),
        "contract_revision": 1,
        "contract_digest": installed.digest,
        "attempt_id": world.attempt.as_str(),
        "repository": repo,
        "base_oid": world.oid,
        "candidate_oid": world.oid,
        "object_format": "sha1",
        "artifact_manifest": outputs.unwrap_or(&["src/file.txt"]).iter().map(|path| serde_json::json!({"path":path,"oid":world.oid})).collect::<Vec<_>>(),
        "claimed_checks": ["ignored"],
        "objects": objects.iter().map(|(oid, relative, _)| serde_json::json!({"oid": oid, "relative_path": relative})).collect::<Vec<_>>()
    });
    let receipt = db
        .submit_result(&serde_json::to_vec(&submission).unwrap())
        .unwrap();
    (receipt.submission_id, installed.digest)
}

fn policy_file(world: &World, body: &str) -> PathBuf {
    let path = world.work.join("policy.json");
    fs::write(&path, body).unwrap();
    path
}

fn request(world: &World, body: &str, timeout: Duration) -> VerifyRequest {
    VerifyRequest::new(
        "pending",
        "builds",
        policy_file(world, body),
        "verify-1",
        timeout,
        &world.work,
    )
}

fn require_unshare() -> bool {
    if Path::new("/usr/bin/unshare").is_file() {
        return true;
    }
    eprintln!("ignored: /usr/bin/unshare is missing; no verification receipt was written");
    false
}

fn git_diff_policy(checkout: &Path) -> String {
    serde_json::json!({
        "version": 1,
        "checks": ["/usr/bin/git", "-c", "core.hooksPath=/dev/null", "-C", checkout, "diff", "--quiet"]
    })
    .to_string()
}

fn run_verify(
    world: &World,
    body: &str,
    timeout: Duration,
    fault: Fault,
) -> (VerifyOutcome, SqliteStore) {
    let mut request = request(world, body, timeout);
    let mut db = SqliteStore::open(&world.db_path).unwrap();
    let (submission_id, _) = install_and_submit(world, body);
    assert!(db.admission_backlog_ages().unwrap().0.is_some());
    request.submission_id = submission_id;
    request.fault = fault;
    let before = db.capacity_fingerprint().unwrap();
    let dependencies = db.dependency_count().unwrap();
    let parent_mount_state=|| {
        let root=fs::metadata("/").unwrap();
        (fs::read_link("/proc/self/ns/mnt").unwrap(),fs::read("/proc/self/mountinfo").unwrap(),root.dev(),root.ino())
    };
    let parent_before=parent_mount_state();
    let outcome = verify(&mut db, &request).unwrap();
    assert_eq!(parent_mount_state(),parent_before,"verification changed the parent's mount namespace, mounts or root");
    assert_eq!(db.admission_backlog_ages().unwrap().0,None,"a committed verifier run finishes the pending job");
    assert_eq!(db.capacity_fingerprint().unwrap(), before);
    assert_eq!(db.dependency_count().unwrap(), dependencies);
    let held: i64 = db
        .capacity_fingerprint()
        .unwrap()
        .iter()
        .map(|(_, observed, _)| observed)
        .sum();
    assert_eq!(held, 0);
    (outcome, db)
}

#[test]
fn unread_namespace_does_not_allow_mount() {
    assert!(!setup::namespace_is_new(None, "mnt:[1]"));
    assert!(!setup::namespace_is_new(Some("mnt:[1]"), "mnt:[1]"));
    assert!(setup::namespace_is_new(Some("mnt:[2]"), "mnt:[1]"));
}

#[test]
fn materialize_refuses_to_delete_a_nested_store() {
    let root = tempfile::tempdir().unwrap();
    let work = root.path().join("work");
    fs::create_dir_all(work.join("checkout")).unwrap();
    let store = work.join("checkout").join("state.db");
    fs::write(&store, b"database").unwrap();
    let Err(error) = checkout::materialize(&work, &store, &[], "abcdef", "sha1") else {
        panic!("materialize deleted a nested store");
    };
    assert!(
        error.to_string().contains("outside the project store"),
        "{error}"
    );
    assert_eq!(fs::read(&store).unwrap(), b"database");

    let store_dir = root.path().join("state");
    fs::create_dir_all(&store_dir).unwrap();
    let nested = store_dir.join("state.db");
    fs::write(&nested, b"kept").unwrap();
    let inside = store_dir.join("work");
    fs::create_dir_all(&inside).unwrap();
    assert!(checkout::materialize(&inside, &nested, &[], "abcdef", "sha1").is_err());
    assert_eq!(fs::read(&nested).unwrap(), b"kept");
}

#[test]
fn staged_and_untracked_changes_reject_without_a_result() {
    if !require_unshare() {
        return;
    }
    let diff_for = |world: &World| git_diff_policy(&world.work.join("checkout"));
    {
        let world = world(false);
        let body = diff_for(&world);
        let (outcome, db) = run_verify(&world, &body, Duration::from_secs(30), Fault::StageChange);
        assert_eq!(outcome.reason.as_deref(), Some("tampered_tree"));
        assert!(outcome.receipt.is_none());
        assert_eq!(db.verified_result_count().unwrap(), 0);
    }
    {
        let world = world(false);
        let body = diff_for(&world);
        let (outcome, db) = run_verify(&world, &body, Duration::from_secs(30), Fault::Untracked);
        assert_eq!(outcome.reason.as_deref(), Some("tampered_tree"));
        assert!(outcome.receipt.is_none());
        assert_eq!(db.verified_result_count().unwrap(), 0);
    }
}


#[test]
fn same_namespace_exits_without_calling_mount() {
    let mnt = fs::read_link("/proc/self/ns/mnt").unwrap();
    let mnt = mnt.to_string_lossy().into_owned();
    let exe = std::env::current_exe().unwrap();
    let mut command = Command::new(&exe);
    command.args([
        "verification-setup",
        "--host-mnt",
        &mnt,
        "--checkout",
        "/tmp/not-mounted",
        "--policy",
        "/tmp/not-mounted",
        "--git",
        "/usr/bin/git",
        "--",
        "/usr/bin/git",
        "--version",
    ]);
    let filter = kill_on_mount_filter();
    unsafe {
        command.pre_exec(move || install_kill_on_mount(&filter));
    }
    let status = command.status().unwrap();
    assert_eq!(
        status.code(),
        Some(71),
        "same-namespace setup must exit before mount"
    );
}

#[test]
fn policy_digest_mismatch_rejects_without_a_result() {
    if !require_unshare() {
        return;
    }
    let world = world(false);
    let checkout = world.work.join("checkout");
    let body = git_diff_policy(&checkout);
    let (mut outcome_request, submission) = {
        let (submission, _) = install_and_submit(&world, &body);
        (request(&world, &body, Duration::from_secs(20)), submission)
    };
    fs::write(
        world.work.join("policy.json"),
        b"{\"version\":1,\"checks\":[\"/usr/bin/git\",\"status\"]}",
    )
    .unwrap();
    outcome_request.submission_id = submission;
    let mut db = SqliteStore::open(&world.db_path).unwrap();
    let outcome = verify(&mut db, &outcome_request).unwrap();
    assert_eq!(outcome.state, "rejected");
    assert_eq!(outcome.reason.as_deref(), Some("policy_digest_mismatch"));
    assert!(outcome.receipt.is_none());
    assert_eq!(db.verified_result_count().unwrap(), 0);
}

#[test]
fn tampered_tree_timeout_leftover_and_setup_failure_reject_without_a_result() {
    if !require_unshare() {
        return;
    }
    {
        let world = world(true);
        let diff = git_diff_policy(&world.work.join("checkout"));
        let (outcome, db) = run_verify(&world, &diff, Duration::from_secs(30), Fault::DirtyTree);
        assert_eq!(outcome.reason.as_deref(), Some("tampered_tree"));
        assert!(outcome.receipt.is_none());
        assert_eq!(db.verified_result_count().unwrap(), 0);
    }
    {
        let world = world(true);
        let sleep =
            serde_json::json!({"version":1,"checks":[probe_path(&world), "sleep"]}).to_string();
        let (outcome, db) = run_verify(&world, &sleep, Duration::from_secs(8), Fault::None);
        assert_eq!(outcome.reason.as_deref(), Some("timeout"));
        assert!(outcome.receipt.is_none());
        assert_eq!(db.verified_result_count().unwrap(), 0);
    }
    {
        let world = world(true);
        let orphan =
            serde_json::json!({"version":1,"checks":[probe_path(&world), "orphan"]}).to_string();
        let (outcome, db) = run_verify(&world, &orphan, Duration::from_secs(30), Fault::None);
        assert_eq!(outcome.reason.as_deref(), Some("leftover_child"));
        assert!(outcome.receipt.is_none());
        assert_eq!(db.verified_result_count().unwrap(), 0);
    }
    {
        let world = world(false);
        let diff = git_diff_policy(&world.work.join("checkout"));
        let (outcome, db) = run_verify(&world, &diff, Duration::from_secs(30), Fault::DropPolicy);
        assert_eq!(outcome.reason.as_deref(), Some("isolation_setup_failed"));
        assert!(outcome.receipt.is_none());
        assert_eq!(db.verified_result_count().unwrap(), 0);
    }
    {
        let world = world(false);
        let diff = git_diff_policy(&world.work.join("checkout"));
        let request = {
            let (submission, _) = install_and_submit(&world, &diff);
            let mut request = request(&world, &diff, Duration::from_secs(20));
            request.submission_id = submission;
            request.unshare_program = PathBuf::from("/no/such/unshare");
            request
        };
        let mut db = SqliteStore::open(&world.db_path).unwrap();
        let outcome = verify(&mut db, &request).unwrap();
        assert_eq!(outcome.reason.as_deref(), Some("isolation_setup_failed"));
        assert!(outcome.receipt.is_none());
        assert_eq!(db.verified_result_count().unwrap(), 0);
        let dash = outcome.argv.iter().position(|arg| arg == "--").unwrap();
        assert!(outcome.argv[dash + 1].contains('/'));
        assert_eq!(outcome.argv[dash + 2], "verification-setup");
    }
}

#[test]
fn candidate_inside_the_namespace_gets_enoent_for_hidden_paths() {
    if !require_unshare() {
        return;
    }
    let world = world(true);
    let db_path = world.db_path.canonicalize().unwrap();
    let config = world.work.join("owner-config.toml");
    let credential = world.work.join("sentinel-credential");
    fs::write(&config, b"owner = true\n").unwrap();
    fs::write(&credential, b"secret\n").unwrap();
    let probe = probe_path(&world);
    let body = serde_json::json!({
        "version": 1,
        "checks": [probe, "probe", db_path, config, credential]
    })
    .to_string();
    let (outcome, db) = run_verify(&world, &body, Duration::from_secs(30), Fault::None);
    assert_eq!(
        outcome.state, "accepted",
        "stderr-less failure: {:?}",
        outcome.reason
    );
    assert_eq!(outcome.stdout, "2\n2\n2\n");
    let receipt = outcome.receipt.expect("accepted run returns a receipt");
    assert_eq!(receipt.isolation(), ISOLATION);
    assert_eq!(db.verified_result_count().unwrap(), 1);
    let dash = outcome.argv.iter().position(|arg| arg == "--").unwrap();
    assert!(!outcome.argv[dash + 1].is_empty());
}

#[test]
fn happy_path_checks_out_retained_objects_and_keeps_capacity() {
    if !require_unshare() {
        return;
    }
    let world = world(false);
    let body = git_diff_policy(&world.work.join("checkout"));
    let (outcome, mut db) = run_verify(&world, &body, Duration::from_secs(30), Fault::None);
    assert_eq!(outcome.state, "accepted", "{:?}", outcome.reason);
    let receipt = outcome.receipt.expect("receipt");
    assert_eq!(receipt.isolation(), "linux-unshare-user-pid-mount-v1");
    assert_eq!(receipt.commit_oid(), world.oid);
    assert_eq!(receipt.exit_status(), 0);
    assert_eq!(db.verified_result_count().unwrap(), 1);
    // Preserve a real accepted run and its receipt across the historical
    // planning/wait upgrades, rather than looking for SQL writer names.
    let raw=rusqlite::Connection::open(&world.db_path).unwrap();
    let retained=|| {
        ["verification_runs","verified_results"].map(|table| {
            let mut statement=raw.prepare(&format!("SELECT * FROM {table}")).unwrap();let columns=statement.column_count();
            statement.query_map([],|row|(0..columns).map(|column|row.get::<_,rusqlite::types::Value>(column)).collect::<rusqlite::Result<Vec<_>>>()).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap()
        })
    };
    let verification_before=retained();assert_eq!(verification_before[0].len(),1);assert_eq!(verification_before[1].len(),1);
    drop(db);crate::store::test_schema::historical(&raw,35).unwrap();
    assert_eq!(retained(),verification_before);
    let mut db=SqliteStore::open(&world.db_path).unwrap();
    assert_eq!(raw.query_row("PRAGMA user_version",[],|r|r.get::<_,u32>(0)).unwrap(),35);
    db.upgrade_v1().unwrap();assert_eq!(retained(),verification_before);
    assert_eq!(raw.query_row("PRAGMA user_version",[],|r|r.get::<_,u32>(0)).unwrap(),crate::store::SCHEMA);
    // Completion before registration must still produce one advisory wake.
    let wait=db.register_wait(world.task.as_str(),Some(world.attempt.as_str()),"validation_completion").unwrap();
    let replay=db.replay_wait(&wait.wait_id).unwrap();
    assert!(replay.wake_requested&&!replay.proved);
    assert!(db.replay_wait(&wait.wait_id).unwrap().already_replayed);
    assert!(db.read_snapshot(None).unwrap().attempts.iter().any(|attempt|attempt.id==world.attempt&&!attempt.termination_observed));
    let dash = outcome
        .argv
        .iter()
        .position(|arg| arg == "--")
        .expect("argv");
    assert!(outcome.argv[dash + 1].contains('/'));
    assert_eq!(outcome.argv[dash + 2], "verification-setup");
}

#[test]
fn moved_memory_fence_does_not_mint_a_receipt() {
    if !require_unshare() {
        return;
    }
    let world = world(false);
    let body = git_diff_policy(&world.work.join("checkout"));
    let (outcome, db) = run_verify(&world, &body, Duration::from_secs(30), Fault::BumpFence);
    assert_eq!(outcome.state, "rejected");
    assert_eq!(outcome.reason.as_deref(), Some("stale_verification"));
    assert!(outcome.receipt.is_none());
    assert_eq!(db.verified_result_count().unwrap(), 0);
}

fn probe_path(world: &World) -> String {
    world
        .work
        .join("checkout")
        .join("probe")
        .display()
        .to_string()
}

fn kill_on_mount_filter() -> Vec<libc::sock_filter> {
    const LD: u16 = 0x00;
    const W: u16 = 0x00;
    const ABS: u16 = 0x20;
    const JMP: u16 = 0x05;
    const JEQ: u16 = 0x10;
    const K: u16 = 0x00;
    const RET: u16 = 0x06;
    const ARCH: u32 = 0xc000_003e;
    let kill = libc::SECCOMP_RET_KILL_PROCESS;
    let allow = libc::SECCOMP_RET_ALLOW;
    let mut filter = vec![
        libc::sock_filter {
            code: LD | W | ABS,
            jt: 0,
            jf: 0,
            k: 4,
        },
        libc::sock_filter {
            code: JMP | JEQ | K,
            jt: 1,
            jf: 0,
            k: ARCH,
        },
        libc::sock_filter {
            code: RET | K,
            jt: 0,
            jf: 0,
            k: kill,
        },
        libc::sock_filter {
            code: LD | W | ABS,
            jt: 0,
            jf: 0,
            k: 0,
        },
    ];
    for syscall in [libc::SYS_mount, libc::SYS_pivot_root, libc::SYS_umount2] {
        filter.push(libc::sock_filter {
            code: JMP | JEQ | K,
            jt: 0,
            jf: 1,
            k: syscall as u32,
        });
        filter.push(libc::sock_filter {
            code: RET | K,
            jt: 0,
            jf: 0,
            k: kill,
        });
    }
    filter.push(libc::sock_filter {
        code: RET | K,
        jt: 0,
        jf: 0,
        k: allow,
    });
    filter
}

fn install_kill_on_mount(filter: &[libc::sock_filter]) -> std::io::Result<()> {
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let program = libc::sock_fprog {
        len: filter.len() as libc::c_ushort,
        filter: filter.as_ptr() as *mut libc::sock_filter,
    };
    if unsafe {
        libc::prctl(
            libc::PR_SET_SECCOMP,
            libc::SECCOMP_MODE_FILTER,
            &program as *const _ as libc::c_ulong,
            0,
            0,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[test]
fn review_probe_sha256_retained_checkout() {
    let world = world(false);
    fs::remove_dir_all(world.repo.join(".git")).unwrap();
    git(&world.repo, &["init", "--object-format=sha256"]);
    git(&world.repo, &["add", "src/file.txt"]);
    git(&world.repo, &["commit", "-m", "sha256"]);
    let output=Command::new("/usr/bin/git").args(["rev-parse","HEAD"]).current_dir(&world.repo).output().unwrap();
    let oid=String::from_utf8(output.stdout).unwrap().trim().to_string();
    assert_eq!(oid.len(),64);
    let objects = loose_objects(&world.repo).into_iter().map(|(oid,relative_path,bytes)|crate::store::verification::RetainedObject{oid,relative_path,bytes}).collect::<Vec<_>>();
    let checkout=super::checkout::materialize(&world.work,&world.db_path,&objects,&oid,"sha256");
    assert!(checkout.is_ok(), "valid retained SHA256 objects failed: {:?}", checkout.err());
}

fn review_install_two_policies(world: &World, policy_body: &str) -> (String, String) {
    let db_path = world.db_path.canonicalize().unwrap();
    let repo = world.repo.canonicalize().unwrap();
    let mut bytes = serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "project_store": db_path,
        "expected_head": world.head,
        "task_id": world.task.as_str(),
        "contract_revision": 1,
        "deliverable": "verified checkout",
        "non_goals": "no dependency release",
        "acceptance_policies": [{"id": "builds", "text": policy_body}, {"id":"required-tests", "text": "{\"version\":1,\"checks\":[\"/usr/bin/git\",\"rev-parse\",\"--verify\",\"refs/heads/missing\"]}"}],
        "repository": repo,
        "base_oid": world.oid,
        "object_format": "sha1",
        "dependencies": [],
        "capability_flags": [],
        "profile_kind": "codex",
        "retry_class": "none",
        "result_schema_id": "result-v1",
        "route": "verify_only",
        "authority": {"id": "owner-approval-policy", "revision": 1, "digest": "ab".repeat(32)}
    }))
    .unwrap();
    bytes.push(b'\n');
    let mut db = SqliteStore::open(&world.db_path).unwrap();
    let prepared = PreparedContract::parse_verified(&bytes).unwrap();
    let installed = db.install_contract(&prepared).unwrap();
    let objects = loose_objects(&repo);
    assert!(objects.iter().any(|(oid, _, _)| oid == &world.oid));
    let submission = serde_json::json!({
        "idempotency_key": "submit-1",
        "task_id": world.task.as_str(),
        "contract_revision": 1,
        "contract_digest": installed.digest,
        "attempt_id": world.attempt.as_str(),
        "repository": repo,
        "base_oid": world.oid,
        "candidate_oid": world.oid,
        "object_format": "sha1",
        "artifact_manifest": [{"path": "src/file.txt", "oid": world.oid}],
        "claimed_checks": ["ignored"],
        "objects": objects.iter().map(|(oid, relative, _)| serde_json::json!({"oid": oid, "relative_path": relative})).collect::<Vec<_>>()
    });
    let receipt = db
        .submit_result(&serde_json::to_vec(&submission).unwrap())
        .unwrap();
    (receipt.submission_id, installed.digest)
}

#[test]
fn review_probe_dependency_requires_selected_acceptance_policy() {
    check_dependency_policy("required-tests", false, false, false, false);
}

#[test]
fn dependency_policy_body_and_late_contract_are_bound() {
    check_dependency_policy("builds", true, false, false, false);
    check_dependency_policy("builds", false, false, true, false);
    check_dependency_policy("builds", false, true, true, false);
    check_dependency_policy("builds", false, false, true, true);
    check_dependency_policy("required-tests", false, true, false, false);
}

fn check_dependency_policy(policy_id: &str, change_body: bool, late_contract: bool, expected: bool, late_wait:bool) {
    let mut world = world(false);
    let mut db = SqliteStore::open(&world.db_path).unwrap();
    db.commit(Commit{expected_head:world.head, mutations:vec![Mutation::Task{expected:None,next:Task{id:TaskId::new("consumer").unwrap(),revision:1,state:TaskState::Draft,title:"consumer".into(),active_attempt:None}}]}).unwrap();
    db.queue_task(&TaskId::new("consumer").unwrap(),1,db.current_head().unwrap(),&QueueRequest{priority:0,dependencies:vec![Dependency{predecessor:world.task.clone(),requirement:DependencyRequirement::VerifiedResult}]},0).unwrap();
    world.head=db.current_head().unwrap();
    let policy=git_diff_policy(&world.work.join("checkout"));
    let (submission, _) = review_install_two_policies(&world,&policy);
    let raw=rusqlite::Connection::open(&world.db_path).unwrap();
    let bytes:Vec<u8>=raw.query_row("SELECT raw_bytes FROM task_contracts WHERE task_id='task'",[],|r|r.get(0)).unwrap();
    let mut contract:serde_json::Value=serde_json::from_slice(&bytes).unwrap();
    contract["task_id"]="consumer".into();
    contract["expected_head"]=db.current_head().unwrap().into();
    contract["dependencies"]=serde_json::json!([{"predecessor":"task","edge":"verified_result","policy_id":policy_id}]);
    if change_body { contract["acceptance_policies"][0]["text"] = "different checks".into(); }
    if !late_contract {
        db.install_contract(&PreparedContract::parse_verified(&serde_json::to_vec(&contract).unwrap()).unwrap()).unwrap();
    }
    let wait=if late_wait {None} else {Some(db.register_wait("consumer",None,"dependency_evidence").unwrap())};
    if let Some(wait)=&wait {assert!(!db.replay_wait(&wait.wait_id).unwrap().wake_requested);}
    let mut req=request(&world,&policy,Duration::from_secs(10));
    req.submission_id=submission;
    let outcome=verify(&mut db,&req).unwrap();
    assert_eq!(outcome.state,"accepted", "{:?}",outcome.reason);
    if late_contract {
        // Consume the verifier event while policy binding is still missing.
        // Installing the contract must itself trigger a later reevaluation.
        assert!(!db.replay_wait(&wait.as_ref().unwrap().wait_id).unwrap().wake_requested);
        contract["expected_head"] = db.current_head().unwrap().into();
        db.install_contract(&PreparedContract::parse_verified(&serde_json::to_vec(&contract).unwrap()).unwrap()).unwrap();
    }
    // Consumption must revalidate even when a legacy queue-only receipt predates
    // the contract and therefore remains in the append-only satisfaction history.
    assert_eq!(db.satisfied_edges("consumer").unwrap().is_some(), expected);
    let wait=wait.unwrap_or_else(||db.register_wait("consumer",None,"dependency_evidence").unwrap());
    let replay=db.replay_wait(&wait.wait_id).unwrap();
    assert_eq!(replay.wake_requested,expected,"only complete policy-bound dependency evidence may wake the parent");
    assert!(!replay.proved);
    if !late_contract {
        let rows:i64=raw.query_row("SELECT count(*) FROM dependency_satisfactions WHERE task_id='consumer' AND state='valid'",[],|r|r.get(0)).unwrap();
        assert_eq!(rows, i64::from(expected), "policy-bound publication disagrees");
    }
}

#[test]
fn required_outputs_are_checked_against_the_retained_candidate() {
    for (output, accepted) in [("src/file.txt", true), ("src/missing.txt", false), ("src/link.txt", false), ("src/alias/file.txt", false), ("src/folder", false)] {
        let mut world = world(false);
        fs::create_dir_all(world.repo.join("src/folder")).unwrap();
        fs::write(world.repo.join("src/folder/child.txt"), "child").unwrap();
        std::os::unix::fs::symlink("file.txt", world.repo.join("src/link.txt")).unwrap();
        std::os::unix::fs::symlink(".", world.repo.join("src/alias")).unwrap();
        git(&world.repo, &["add", "src"]);
        git(&world.repo, &["commit", "-qm", "output candidates"]);
        let head = Command::new("/usr/bin/git").args(["rev-parse", "HEAD"]).current_dir(&world.repo).output().unwrap();
        assert!(head.status.success());
        world.oid = String::from_utf8(head.stdout).unwrap().trim().to_string();
        let body = git_diff_policy(&world.work.join("checkout"));
        let (submission_id, _) = install_and_submit_outputs(&world, &body, Some(&[output]));
        let mut request = request(&world, &body, Duration::from_secs(30));
        request.submission_id = submission_id;
        let mut db = SqliteStore::open(&world.db_path).unwrap();
        let before = db.capacity_fingerprint().unwrap();
        let outcome = verify(&mut db, &request).unwrap();
        assert_eq!(outcome.receipt.is_some(), accepted, "{output}: {:?}", outcome.reason);
        if !accepted { assert_eq!(outcome.reason.as_deref(), Some("required_output_missing")); }
        assert_eq!(db.verified_result_count().unwrap(), if accepted { 1 } else { 0 });
        assert_eq!(db.capacity_fingerprint().unwrap(), before);
    }
}

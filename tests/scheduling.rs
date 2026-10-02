#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! Queue ordering, capacity, queue blockers, resource claims and the active
//! work inventory, over a disposable project driven through the compiled CLI:
//! `task queue`, `scheduler policy/inspect`, `launch draft/reserve`, signed
//! contracts and approvals, `runtime adopt/relinquish` and `reconcile`. No
//! worker is launched. Attempts no reservation produced (a worker's own
//! success, lost or terminated attempts) are recorded with the public generic
//! commit, and one queue entry is back-dated through the public store API
//! because the binary has no clock override.
use herdr_farm::{authority, domain::*, migration, runtime};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::{BTreeMap, BTreeSet}, fs, os::unix::fs::{MetadataExt, PermissionsExt}, path::{Path, PathBuf}, process::{Command, Output}};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-farm");

fn now() -> i64 { jiff::Timestamp::now().as_millisecond() }

struct Factory {
    home: tempfile::TempDir, project: PathBuf, key: PathBuf, repo: PathBuf, store: String, base: String,
    profile: VersionedReference, bindings: BTreeMap<String, String>, _socket: std::os::unix::net::UnixListener,
}

impl Factory {
    /// An active project with an owner key, a SHA-256 repository, the draft
    /// tasks `tasks`, a local runtime binding for each task in `bound`, and a
    /// launchable `worker` profile over fake Herdr and agent binaries.
    fn new(max_workers: u32, tasks: &[&str], bound: &[&str]) -> Self {
        let home = tempfile::tempdir().unwrap();
        let key = home.path().join("owner");
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&key).output().unwrap().status.success());
        let public = fs::read_to_string(key.with_extension("pub")).unwrap().split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        let config = home.path().join(".config/herdr-farm/config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, format!("[authority]\nversion=1\nrevision=1\napproval_public_key={public:?}\n[profiles.worker]\nkind='codex'\npermission_policy='interactive'\n[profiles.worker.budget]\nmax_wall_seconds=60\nunknown_usage='allow_with_warning'\n")).unwrap();
        let repo = home.path().join("repo");
        fs::create_dir(&repo).unwrap();
        let socket = std::os::unix::net::UnixListener::bind(home.path().join("native.sock")).unwrap();
        let mut f = Factory { project: home.path().join("root/demo"), key, repo, store: String::new(), base: String::new(),
            profile: VersionedReference { id: String::new(), revision: 1, digest: String::new() }, bindings: BTreeMap::new(), _socket: socket, home };
        for command in ["new", "pause"] { f.ok(&[command, "demo"]); }
        migration::apply(&f.project, &migration::inspect_with_config(&f.project, &config).unwrap(), true).unwrap();
        f.store = f.project.join(".state/state.db").canonicalize().unwrap().display().to_string();
        f.git(&["init", "-q", "--object-format=sha256"]);
        f.git(&["commit", "-q", "--allow-empty", "-m", "base"]);
        f.base = f.git(&["rev-parse", "HEAD"]);
        for task in tasks { f.ok(&["task", "demo", "add", task, "--title", task, "--expected-head", &f.head().to_string()]); }
        f.policy(max_workers);
        let route = RuntimeRoute { socket: f.path("native.sock").display().to_string(), cwd: f.repo.canonicalize().unwrap().display().to_string(), ..Default::default() };
        let mut observations = Vec::new();
        for task in bound {
            let id = TaskId::new(*task).unwrap();
            let change = runtime::create_binding(&f.project, Some(&id), Some(f.revision(task)), f.head(), &route).unwrap();
            observations.push(herdr_farm::reconcile::RuntimeObservation { binding: change.binding.id.clone(), binding_revision: change.binding.revision,
                task_revision: change.task_revision, observed_unix_ms: now(), collector: "herdr-git-v2".into(),
                config_digest: migration::config_reference(&config).unwrap().digest, ..Default::default() });
            f.bindings.insert(task.to_string(), change.binding.id);
        }
        migration::open_active(&f.project).unwrap().record_observations(f.head(), &observations).unwrap();
        let snapshot = runtime::snapshot(&f.project).unwrap();
        runtime::set_state(&f.project, snapshot.head, snapshot.control.unwrap().revision, ProjectState::Active, &config).unwrap();
        f.profile = f.launchable_profile();
        f
    }
    fn cli(&self, args: &[&str]) -> Output {
        Command::new(BIN).env_clear().env("HOME", self.home.path()).env("PATH", "/usr/bin:/bin").env("HERDR_BIN_PATH", self.path("herdr-session"))
            .args(["--root", self.home.path().join("root").to_str().unwrap()]).args(args).output().unwrap()
    }
    fn ok(&self, args: &[&str]) -> Value {
        let out = self.cli(args);
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
    }
    /// A refused command writes nothing; returns its stderr.
    fn refused(&self, args: &[&str]) -> String {
        let before = runtime::snapshot(&self.project).unwrap();
        let out = self.cli(args);
        assert!(!out.status.success(), "{args:?} accepted: {}", String::from_utf8_lossy(&out.stdout));
        assert_eq!(runtime::snapshot(&self.project).unwrap(), before, "{args:?} was refused but wrote");
        String::from_utf8_lossy(&out.stderr).into_owned()
    }
    fn head(&self) -> u64 { runtime::snapshot(&self.project).unwrap().head }
    fn path(&self, name: &str) -> PathBuf { self.home.path().join(name) }
    fn attempts(&self) -> Vec<Attempt> { runtime::snapshot(&self.project).unwrap().attempts }
    fn revision(&self, task: &str) -> u64 { runtime::snapshot(&self.project).unwrap().tasks.into_iter().find(|t| t.id.as_str() == task).unwrap().revision }
    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("/usr/bin/git").env_clear().env("PATH", "/usr/bin:/bin").env("HOME", self.home.path())
            .env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "fixture").env("GIT_AUTHOR_EMAIL", "fixture@example.com")
            .env("GIT_COMMITTER_NAME", "fixture").env("GIT_COMMITTER_EMAIL", "fixture@example.com")
            .current_dir(&self.repo).args(args).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }
    /// Owner-sign `bytes` written to `name`; returns (document, signature).
    fn sign(&self, name: &str, bytes: &[u8], namespace: &str) -> (String, String) {
        let path = self.path(name);
        fs::write(&path, bytes).unwrap();
        let signature = format!("{}.sig", path.display());
        let _ = fs::remove_file(&signature);
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-Y", "sign", "-f"]).arg(&self.key).args(["-n", namespace]).arg(&path).output().unwrap().status.success());
        (path.display().to_string(), signature)
    }
    /// Prepare `worker` over fake binaries; only the native interaction
    /// evidence, which needs a real agent session, is planted.
    fn launchable_profile(&self) -> VersionedReference {
        use herdr_farm::worker_supervision::{ProcessIncarnation, SupervisorIdentity};
        let bin = self.path("bin");
        let agent_home = self.path("agent-home");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&agent_home).unwrap();
        let (herdr, agent) = (bin.join("herdr"), bin.join("codex"));
        fs::write(&herdr, "#!/usr/bin/python3\nimport sys,json\nif sys.argv[1:]==['--version']:print('herdr 0.9.1');sys.exit(0)\nr=json.loads(sys.stdin.readline())\nprint(json.dumps({'id':r['id'],'result':{'type':'pong','version':'0.9.1','capabilities':{'workspace_create_command':True}}}))\n").unwrap();
        fs::write(&agent, "#!/bin/sh\n[ \"$#\" = 1 ] && [ \"$1\" = --version ] || exit 2\nprintf '%s\\n' 'codex-cli 0.154.0'\n").unwrap();
        for path in [&herdr, &agent] { fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap(); }
        let prepared = self.ok(&["profile", "prepare", "demo", "worker", "--herdr-executable", herdr.to_str().unwrap(),
            "--agent-executable", agent.to_str().unwrap(), "--execution-home", agent_home.to_str().unwrap()]);
        let mut profile: FrozenProfile = serde_json::from_value(prepared["profile"].clone()).unwrap();
        #[derive(serde::Serialize)] struct Interaction { session: ResourceIdentity, terminal: &'static str, readiness_manifest: &'static str, prompt_digest: String, acknowledged_unix_ms: i64 }
        #[derive(serde::Serialize)] struct Evidence { version: u32, prepared_profile: VersionedReference, supervisor: SupervisorIdentity, native_kind: String, observed_unix_ms: i64, stopped_unix_ms: i64, interaction: Interaction }
        let evidence = Evidence { version: 2, prepared_profile: profile.reference().unwrap(), native_kind: profile.kind.clone(), observed_unix_ms: 1000, stopped_unix_ms: 1001,
            supervisor: SupervisorIdentity { version: 1, boot_id: "00000000-0000-0000-0000-000000000001".into(), host_id: None, observer_namespace: (1, 2), worker_namespace: (1, 3),
                outer: ProcessIncarnation { pid: 20, device: 1, inode: 4 }, init: ProcessIncarnation { pid: 21, device: 1, inode: 5 } },
            interaction: Interaction { session: ResourceIdentity { device: 1, inode: 2, born_secs: 1, born_nanos: 0 }, terminal: "fixture-terminal", readiness_manifest: "fixture-manifest", prompt_digest: "a".repeat(64), acknowledged_unix_ms: 999 } };
        let hash = format!("{:x}", Sha256::digest(serde_json::to_vec(&evidence).unwrap()));
        let supported = CapabilityEvidence::Supported { evidence: VersionedReference { id: format!("native-transport-{hash}"), revision: 1, digest: hash } };
        let c = &mut profile.capabilities;
        (c.launch, c.stop, c.readiness_observation, c.prompt_submission) = (supported.clone(), supported.clone(), supported.clone(), supported);
        let reference = profile.reference().unwrap();
        let metadata = fs::metadata(&self.store).unwrap();
        let report = json!({"preparation":{"profile":profile,"reference":reference,"launchable":true,"protocol_capable":false,"certified":false},
            "evidence":evidence,"source_store":[self.store,metadata.dev(),metadata.ino()]}).to_string();
        rusqlite::Connection::open(&self.store).unwrap().execute("INSERT INTO native_profiles(profile_digest,report,report_digest,sequence) VALUES(?1,?2,?3,(SELECT max(sequence) FROM events))",
            rusqlite::params![reference.digest, report, format!("{:x}", Sha256::digest(report.as_bytes()))]).unwrap();
        reference
    }
    fn queue_args(&self, task: &str, priority: i32, dependencies: Value) -> Vec<String> {
        let request = self.path(&format!("{task}-queue.json"));
        fs::write(&request, json!({"priority":priority,"dependencies":dependencies}).to_string()).unwrap();
        ["task", "demo", "queue", task, "--input-file", request.to_str().unwrap(), "--expected-revision", &self.revision(task).to_string(), "--expected-head", &self.head().to_string()].map(String::from).to_vec()
    }
    fn queue(&self, task: &str, priority: i32, dependencies: Value) { self.ok(&strs(&self.queue_args(task, priority, dependencies))); }
    fn policy(&self, max_workers: u32) -> Value {
        let revision = runtime::snapshot(&self.project).unwrap().scheduler.unwrap().policy.revision.to_string();
        self.ok(&["scheduler", "demo", "policy", "--max-active-workers", &max_workers.to_string(), "--max-attempts-per-task", "3", "--expected-revision", &revision, "--expected-head", &self.head().to_string()])
    }
    fn inspect(&self) -> Value { self.ok(&["scheduler", "demo", "inspect"]) }
    fn blockers(&self, task: &str) -> Vec<String> {
        let queue = self.inspect();
        let entry = queue["entries"].as_array().unwrap().iter().find(|e| e["task"] == task).unwrap().clone();
        serde_json::from_value(entry["blockers"].clone()).unwrap()
    }
    /// Retain worker knowledge for `task` and write its launch selection.
    fn selection(&self, task: &str) -> PathBuf {
        let scope = self.path(&format!("{task}-scope.json"));
        fs::write(&scope, json!({"schema_version":1,"task_id":task,"profile":"worker","domains":[],"paths":[],"pinned_keys":[],"sensitivity":"default"}).to_string()).unwrap();
        let snapshot = self.ok(&["memory", "demo", "snapshot", "--task", task, "--profile", "worker", "--input-file", scope.to_str().unwrap(), "--worker"]);
        let selection = self.path(&format!("{task}-selection.json"));
        fs::write(&selection, json!({"task":task,"binding":self.bindings[task],"profile":self.profile,
            "knowledge":{"id":snapshot["id"],"revision":1,"digest":snapshot["manifest_hash"]},"repositories":[self.repo.canonicalize().unwrap()]}).to_string()).unwrap();
        selection
    }
    fn draft_args(&self, selection: &Path) -> Vec<String> {
        ["launch", "demo", "draft", "--selection", selection.to_str().unwrap(), "--expected-head", &self.head().to_string()].map(String::from).to_vec()
    }
    /// Draft and owner-sign the launch; returns the installed approval digest.
    fn approve(&self, selection: &Path) -> String {
        let drafted = self.ok(&strs(&self.draft_args(selection)));
        let (doc, sig) = self.sign("approval.json", &serde_json::to_vec_pretty(&drafted["approval"]).unwrap(), authority::SIGNATURE_NAMESPACE);
        self.ok(&["approval", "demo", "import", &doc, &sig, "--expected-head", &self.head().to_string()])["digest"].as_str().unwrap().to_owned()
    }
    fn reserve_args(&self, selection: &Path, approval: &str) -> Vec<String> {
        ["launch", "demo", "reserve", "--selection", selection.to_str().unwrap(), "--approval-digest", approval, "--expected-head", &self.head().to_string()].map(String::from).to_vec()
    }
    /// Owner-sign a contract for `task` whose scope writes `path`.
    fn contract(&self, task: &str, path: &str) { self.scoped_contract(task, json!({"paths":[{"path":path,"access":"write"}],"named_resources":[]})); }
    /// Owner-sign a contract for `task` with `scope`.
    fn scoped_contract(&self, task: &str, scope: Value) {
        let body = json!({"version":1,"project_store":self.store,"expected_head":self.head(),"task_id":task,"contract_revision":1,
            "deliverable":"scheduling fixture","non_goals":"no worker launch","acceptance_policies":[{"id":"builds","text":r#"{"version":1,"checks":["/usr/bin/git","diff","--quiet"]}"#}],
            "repository":self.repo.canonicalize().unwrap(),"base_oid":self.base,"object_format":"sha256","dependencies":[],"scope":scope,
            "capability_flags":[],"profile_kind":"codex","retry_class":"none","result_schema_id":"result-v1","route":"verify_only",
            "authority":authority::policy_reference(&self.project).unwrap()});
        let (doc, sig) = self.sign(&format!("{task}-contract.json"), &serde_json::to_vec(&body).unwrap(), authority::CONTRACT_SIGNATURE_NAMESPACE);
        self.ok(&["task", "demo", "contract", "put", "--input-file", &doc, "--signature", &sig]);
    }
    /// Record an attempt no reservation produced, and optionally the task's own
    /// state, through the generic commit.
    fn plant(&self, id: &str, task: &str, state: AttemptState, terminated: bool, task_state: Option<TaskState>) {
        let mut db = migration::open_active(&self.project).unwrap();
        let snapshot = db.read_snapshot(None).unwrap();
        let expected = snapshot.attempts.iter().find(|a| a.id.as_str() == id).map(|a| a.revision);
        let next = Attempt { id: AttemptId::new(id).unwrap(), task: TaskId::new(task).unwrap(), revision: expected.map_or(1, |r| r + 1), state,
            snapshot: None, reservation: format!("{id}-slot"), termination_observed: terminated };
        let mut mutations = vec![Mutation::Attempt { expected, next }];
        if let Some(state) = task_state {
            let mut next = snapshot.tasks.iter().find(|t| t.id.as_str() == task).unwrap().clone();
            let expected = next.revision;
            (next.revision, next.state, next.active_attempt) = (expected + 1, state, None);
            mutations.push(Mutation::Task { expected: Some(expected), next });
        }
        db.commit(Commit { expected_head: snapshot.head, mutations }).unwrap();
    }
    /// Bindings `reconcile` observes: the active work inventory. Observes only.
    fn observed(&self) -> Vec<String> {
        let before = runtime::snapshot(&self.project).unwrap();
        let batch = self.ok(&["reconcile", "demo"]);
        assert_eq!(runtime::snapshot(&self.project).unwrap(), before);
        batch["observations"].as_array().unwrap().iter().map(|o| o["binding"].as_str().unwrap().to_owned()).collect()
    }
}

fn strs(args: &[String]) -> Vec<&str> { args.iter().map(String::as_str).collect() }

/// A queued task's age outweighs a newer task's priority, and requeueing it
/// with a new priority keeps its original enqueue time; an identical requeue
/// writes nothing.
#[test]
fn queue_order_ages_and_a_requeue_keeps_the_original_age() {
    let f = Factory::new(1, &["old", "new"], &[]);
    // Queued 41 minutes ago. No CLI path back-dates a queue entry.
    let mut db = migration::open_active(&f.project).unwrap();
    db.queue_task(&TaskId::new("old").unwrap(), f.revision("old"), f.head(), &QueueRequest { priority: -20, dependencies: vec![] }, now() - 41 * 60_000).unwrap();
    f.queue("new", 20, json!([]));
    let order = |f: &Factory| f.inspect()["entries"].as_array().unwrap().iter().map(|e| (e["task"].as_str().unwrap().to_owned(), e["effective_priority"].as_i64().unwrap())).collect::<Vec<_>>();
    assert_eq!(order(&f), [("old".to_owned(), 21), ("new".to_owned(), 20)]);
    assert_eq!(f.inspect()["launch_enabled"], false);
    f.queue("old", -19, json!([]));
    assert_eq!(order(&f), [("old".to_owned(), 22), ("new".to_owned(), 20)], "a requeue restarted the age");
    let before = runtime::snapshot(&f.project).unwrap();
    let replay = f.cli(&strs(&f.queue_args("old", -19, json!([]))));
    assert!(replay.status.success(), "{}", String::from_utf8_lossy(&replay.stderr));
    assert_eq!(runtime::snapshot(&f.project).unwrap(), before);
}

/// Awaiting-input, lost and completed-but-unobserved attempts all hold a slot.
/// Lowering the worker cap below them revokes nothing: the queue reports full
/// capacity and the attempts are untouched.
#[test]
fn every_unterminated_attempt_holds_capacity_and_a_lower_cap_revokes_nothing() {
    let f = Factory::new(5, &["next", "busy"], &[]);
    f.queue("next", 0, json!([]));
    for (id, state) in [("waiting", AttemptState::AwaitingInput), ("vanished", AttemptState::Lost), ("finished", AttemptState::Completed)] {
        f.plant(id, "busy", state, false, None);
    }
    let attempts = f.attempts();
    f.policy(2);
    let queue = f.inspect();
    assert_eq!((&queue["retained_attempts"], &queue["available_slots"]), (&json!(3), &json!(0)), "{queue}");
    assert!(f.blockers("next").contains(&"capacity_full".to_owned()));
    assert_eq!(f.attempts(), attempts);
    let refused = f.refused(&strs(&f.queue_args("busy", 0, json!([]))));
    assert!(!refused.is_empty());
}

/// A predecessor that only reports success, with no verified result, never
/// satisfies a verified-result dependency: the dependent stays blocked and
/// cannot be drafted.
#[test]
fn a_succeeded_predecessor_without_a_verified_result_keeps_its_dependent_blocked() {
    let f = Factory::new(2, &["pred", "dep"], &["dep"]);
    f.queue("dep", 0, json!([{"predecessor":"pred","requirement":"verified_result"}]));
    f.plant("pred-attempt", "pred", AttemptState::Completed, true, Some(TaskState::Succeeded));
    assert_eq!(f.ok(&["task", "demo", "show", "pred"])["state"], "succeeded");
    assert!(f.blockers("dep").contains(&"verified_dependency_evidence_unavailable:pred:verified_result".to_owned()));
    let refused = f.refused(&strs(&f.draft_args(&f.selection("dep"))));
    assert!(refused.contains("not released"), "{refused}");
    assert_eq!(f.attempts().len(), 1);
}

/// The reserve blockers clear only for a task with a retained reserved,
/// launching, running or awaiting-input attempt (a lost one keeps them), and
/// the signature blocker clears only for a task holding an unused signed grant.
#[test]
fn queue_blockers_follow_reservations_unused_grants_and_live_attempts() {
    let f = Factory::new(5, &["a", "b", "c", "d", "e"], &["a", "c"]);
    for task in ["a", "b", "c", "d", "e"] { f.queue(task, 0, json!([])); }
    let selection = f.selection("a");
    let approval = f.approve(&selection);
    f.ok(&strs(&f.reserve_args(&selection, &approval)));
    f.approve(&f.selection("c"));
    f.plant("lost-b", "b", AttemptState::Lost, false, None);
    f.plant("launching-d", "d", AttemptState::Launching, false, None);
    f.plant("awaiting-e", "e", AttemptState::AwaitingInput, false, None);
    let queue = f.inspect();
    assert_eq!(queue["launch_enabled"], false);
    let blockers = |task: &str| -> BTreeSet<String> {
        let entry = queue["entries"].as_array().unwrap().iter().find(|e| e["task"] == task).unwrap();
        serde_json::from_value(entry["blockers"].clone()).unwrap()
    };
    let reserve = ["launch_reserve_not_scheduled", "controller_requires_reserved_attempt"];
    for task in ["a", "d", "e"] { assert!(reserve.iter().all(|b| !blockers(task).contains(*b)), "{task}: {:?}", blockers(task)); }
    for task in ["b", "c"] { assert!(reserve.iter().all(|b| blockers(task).contains(*b)), "{task}: {:?}", blockers(task)); }
    // A grant stays unused until its launch is claimed.
    for task in ["b", "d", "e"] { assert!(blockers(task).contains("owner_signature_not_scheduled"), "{task}"); }
    for task in ["a", "c"] { assert!(!blockers(task).contains("owner_signature_not_scheduled"), "{task}"); }
    assert!(queue["entries"].as_array().unwrap().iter().flat_map(|e| e["blockers"].as_array().unwrap()).all(|b| b != "launch_draft_not_scheduled"));
}

/// With automatic admission off (the default), an owner draft or reservation
/// still refuses a task whose signed write scope overlaps a retained attempt,
/// even with a grant signed before the holder was reserved.
#[test]
fn owner_draft_and_reserve_refuse_an_overlapping_write_scope_with_admission_off() {
    let f = Factory::new(2, &["first", "second"], &["first", "second"]);
    for task in ["first", "second"] { f.contract(task, "shared.txt"); f.queue(task, 0, json!([])); }
    let second = f.selection("second");
    let granted = f.approve(&second);
    let first = f.selection("first");
    let approval = f.approve(&first);
    f.ok(&strs(&f.reserve_args(&first, &approval)));
    assert_eq!(f.inspect()["available_slots"], 1);
    let holder = f.attempts()[0].id.as_str().to_owned();
    for args in [f.draft_args(&second), f.reserve_args(&second, &granted)] {
        let refused = f.refused(&strs(&args));
        // The stable code first, then the holder and the overlapping claims.
        let detail = format!("resource_conflict: path shared.txt (write) overlaps path shared.txt (write) held by task first attempt {holder}");
        assert!(refused.contains(&detail), "{refused}");
    }
    assert_eq!(f.attempts().len(), 1);
}

/// Replaces `overlap_rules_block_exclusive_writes_named_resources_and_uncertain_paths`.
///
/// Drafting next to a reserved holder refuses exactly the scopes that could
/// overlap its claims: a read of a file it writes, any access under a
/// directory or glob it names (uncertain paths), and a named resource it
/// writes. Another file, a shared read, a path outside the glob, another
/// named resource and a path spelled like a resource name are drafted.
#[test]
fn drafts_beside_a_reserved_holder_refuse_only_overlapping_claims() {
    let path = |path: &str, access: &str| json!({"scope": {"paths": [{"path": path, "access": access}], "named_resources": []}});
    let named = |name: &str, access: &str| json!({"scope": {"paths": [], "named_resources": [{"name": name, "access": access}]}});
    let cases = [
        ("read-written", path("README.md", "read"), true), ("glob-match", path("src/lib.rs", "read"), true),
        ("dir-match", path("migrations/0035_resource_claims.sql", "read"), true), ("named-read", named("schema", "read"), true),
        ("other-file", path("src2/b.rs", "write"), false), ("shared-read", path("shared.md", "read"), false),
        ("glob-miss", path("docs/lib.rs", "read"), false), ("named-other", named("lockfile", "write"), false),
        ("path-schema", path("schema", "write"), false),
    ];
    let tasks: Vec<&str> = std::iter::once("holder").chain(cases.iter().map(|(task, _, _)| *task)).collect();
    let f = Factory::new(16, &tasks, &tasks);
    f.scoped_contract("holder", json!({"paths": [{"path": "README.md", "access": "write"}, {"path": "shared.md", "access": "read"},
        {"path": "src/*.rs", "access": "read"}, {"path": "migrations/", "access": "read"}], "named_resources": [{"name": "schema", "access": "write"}]}));
    f.queue("holder", 0, json!([]));
    let holder = f.selection("holder");
    let approval = f.approve(&holder);
    f.ok(&strs(&f.reserve_args(&holder, &approval)));
    for (task, scope, conflict) in &cases {
        f.scoped_contract(task, scope["scope"].clone());
        f.queue(task, 0, json!([]));
        let selection = f.selection(task);
        let out = f.cli(&strs(&f.draft_args(&selection)));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(!out.status.success(), *conflict, "{task}: {stderr}");
        if *conflict { assert!(stderr.contains("resource_conflict"), "{task}: {stderr}"); }
    }
    assert_eq!(f.attempts().len(), 1);
}

/// `reconcile` observes a task binding while it has no attempt history or an
/// unterminated attempt (once, however many it retains), and stops once every
/// attempt is terminated. An adopted binding stays observed after its attempt
/// terminates, until the claim is relinquished.
#[test]
fn reconcile_observes_exactly_the_bindings_with_unfinished_work() {
    let f = Factory::new(2, &["gone", "held", "owned"], &["gone", "held"]);
    let observed = |f: &Factory| f.observed().into_iter().collect::<BTreeSet<_>>();
    let set = |ids: &[&str]| ids.iter().map(|id| id.to_string()).collect::<BTreeSet<_>>();
    assert_eq!(observed(&f), set(&["task:gone", "task:held"]));
    f.plant("gone-1", "gone", AttemptState::Completed, true, None);
    for (id, state) in [("held-1", AttemptState::Running), ("held-2", AttemptState::AwaitingInput)] { f.plant(id, "held", state, false, None); }
    let batch = f.observed();
    assert_eq!(batch, ["task:held"], "one observation per binding, and none for a finished task");

    // A worker in a recorded pane is adopted; its attempt later terminates.
    let s = runtime::snapshot(&f.project).unwrap();
    f.ok(&["runtime", "demo", "state", "paused", "--expected-revision", &s.control.unwrap().revision.to_string(), "--expected-head", &s.head.to_string()]);
    let cwd = f.path("owned-work");
    fs::create_dir(&cwd).unwrap();
    let route = f.path("owned-route.json");
    fs::write(&route, json!({"socket":f.path("native.sock"),"workspace_id":"w","tab_id":"t","pane_id":"p","cwd":cwd}).to_string()).unwrap();
    let created = f.ok(&["runtime", "demo", "create", "--task", "owned", "--task-revision", &f.revision("owned").to_string(), "--route", route.to_str().unwrap(), "--expected-head", &f.head().to_string()]);
    let herdr = f.path("herdr-session");
    let pane = json!({"pane_id":"p","workspace_id":"w","tab_id":"t","cwd":cwd});
    let agent = json!({"pane_id":"p","workspace_id":"w","tab_id":"t","cwd":cwd,"agent":"codex","name":"worker","agent_status":"working"});
    fs::write(&herdr, format!("#!/bin/sh\ncase \"$1 $2\" in\n'--version ') echo 'herdr 0.9.1';;\n'pane list') echo '{}';;\n'agent list') echo '{}';;\n*) exit 99;;\nesac\n",
        json!({"result":{"panes":[pane]}}), json!({"result":{"agents":[agent]}}))).unwrap();
    fs::set_permissions(&herdr, fs::Permissions::from_mode(0o700)).unwrap();
    let adopted = f.ok(&["runtime", "demo", "adopt", "task:owned", "--expected-revision", &created["binding"]["revision"].to_string(), "--expected-head", &f.head().to_string()]);
    let attempt = adopted["ownership"]["attempt"].as_str().unwrap().to_owned();
    f.plant(&attempt, "owned", AttemptState::Failed, true, Some(TaskState::Failed));
    assert_eq!(observed(&f), set(&["task:held", "task:owned"]), "an owned binding stays observed after its attempt terminates");
    f.ok(&["runtime", "demo", "relinquish", "task:owned", "--expected-revision", &adopted["ownership"]["revision"].to_string(), "--expected-head", &f.head().to_string(), "--reason", "worker done"]);
    assert_eq!(observed(&f), set(&["task:held"]));
}

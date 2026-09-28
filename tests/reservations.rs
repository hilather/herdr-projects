#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! Reservation, budget and cancellation workflows over a disposable project:
//! tasks are queued, drafted, owner-signed and reserved through the compiled
//! CLI, and inspected through `scheduler`, `budget`, `task` and `plan wait`.
//! No worker is launched. A launch claim is taken through the public store API
//! (the ticker's launch job is its only other caller), and attempts that no
//! reservation produced (adopted, lost, a submitting predecessor) are recorded
//! with the public generic commit.
use herdr_projects::{authority, domain::*, migration, operations::{DeliveryState, Outcome}, runtime};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, os::unix::fs::{MetadataExt, PermissionsExt}, path::{Path, PathBuf}, process::{Command, Output}};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-projects");
const POLICY: &str = r#"{"version":1,"checks":["/usr/bin/git","diff","--quiet"]}"#;

fn now() -> i64 { jiff::Timestamp::now().as_millisecond() }

struct Factory {
    home: tempfile::TempDir, project: PathBuf, key: PathBuf, repo: PathBuf, store: String, base: String,
    profile: VersionedReference, bindings: BTreeMap<String, String>, _socket: std::os::unix::net::UnixListener,
}

impl Factory {
    /// An active project with an owner key, a SHA-256 repository, the plain
    /// tasks `plain` and the queued tasks `queued` (each with its dependency
    /// edges), a local runtime binding per queued task and a launchable
    /// `worker` profile over fake Herdr and agent binaries.
    fn new(max_workers: u32, plain: &[&str], queued: &[(&str, Value)]) -> Self {
        let home = tempfile::tempdir().unwrap();
        let key = home.path().join("owner");
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&key).output().unwrap().status.success());
        let public = fs::read_to_string(key.with_extension("pub")).unwrap().split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        let config = home.path().join(".config/herdr-projects/config.toml");
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
        for task in plain { f.ok(&["task", "demo", "add", task, "--title", task, "--expected-head", &f.head().to_string()]); }
        for (task, dependencies) in queued {
            f.ok(&["task", "demo", "add", task, "--title", task, "--expected-head", &f.head().to_string()]);
            let request = f.path(&format!("{task}-queue.json"));
            fs::write(&request, json!({"priority":0,"dependencies":dependencies}).to_string()).unwrap();
            f.ok(&["task", "demo", "queue", task, "--input-file", request.to_str().unwrap(), "--expected-revision", "1", "--expected-head", &f.head().to_string()]);
        }
        let policy = runtime::snapshot(&f.project).unwrap().scheduler.unwrap().policy.revision.to_string();
        f.ok(&["scheduler", "demo", "policy", "--max-active-workers", &max_workers.to_string(), "--max-attempts-per-task", "3", "--expected-revision", &policy, "--expected-head", &f.head().to_string()]);
        let route = RuntimeRoute { socket: f.path("native.sock").display().to_string(), cwd: f.repo.canonicalize().unwrap().display().to_string(), ..Default::default() };
        let mut observations = Vec::new();
        for (task, _) in queued {
            let id = TaskId::new(*task).unwrap();
            let revision = runtime::snapshot(&f.project).unwrap().tasks.into_iter().find(|t| t.id == id).unwrap().revision;
            let change = runtime::create_binding(&f.project, Some(&id), Some(revision), f.head(), &route).unwrap();
            observations.push(herdr_projects::reconcile::RuntimeObservation { binding: change.binding.id.clone(), binding_revision: change.binding.revision,
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
        Command::new(BIN).env_clear().env("HOME", self.home.path()).env("PATH", "/usr/bin:/bin")
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
        use herdr_projects::worker_supervision::{ProcessIncarnation, SupervisorIdentity};
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
    /// Draft and owner-sign the launch; returns (draft, installed approval digest).
    fn approve(&self, selection: &Path) -> (Value, String) {
        let drafted = self.ok(&self.draft_args(selection).iter().map(String::as_str).collect::<Vec<_>>());
        let (doc, sig) = self.sign("approval.json", &serde_json::to_vec_pretty(&drafted["approval"]).unwrap(), authority::SIGNATURE_NAMESPACE);
        let approval = self.ok(&["approval", "demo", "import", &doc, &sig, "--expected-head", &self.head().to_string()]);
        (drafted, approval["digest"].as_str().unwrap().to_owned())
    }
    fn reserve_at(&self, selection: &Path, approval: &str, head: u64) -> Output {
        self.cli(&["launch", "demo", "reserve", "--selection", selection.to_str().unwrap(), "--approval-digest", approval, "--expected-head", &head.to_string()])
    }
    /// Draft, sign and reserve a launch of `task`; returns the reservation.
    fn reserve(&self, task: &str) -> Value {
        let selection = self.selection(task);
        let (_, approval) = self.approve(&selection);
        let out = self.reserve_at(&selection, &approval, self.head());
        assert!(out.status.success(), "reserve {task}: {}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn cancel(&self, attempt: &str, revision: u64) -> Value {
        self.ok(&["task", "demo", "cancel-attempt", attempt, "--expected-revision", &revision.to_string(), "--expected-head", &self.head().to_string(), "--reason", "operator stop"])
    }
    fn queue(&self) -> Value { self.ok(&["scheduler", "demo", "inspect"]) }
    fn blockers(&self, task: &str) -> Vec<String> {
        let queue = self.queue();
        let entry = queue["entries"].as_array().unwrap().iter().find(|e| e["task"] == task).unwrap().clone();
        serde_json::from_value(entry["blockers"].clone()).unwrap()
    }
    fn task_state(&self, task: &str) -> Value { self.ok(&["task", "demo", "show", task]) }
    /// Owner-sign and import budget policy `revision`.
    fn budget(&self, revision: u64, limits: BudgetLimits) -> Value {
        let policy = BudgetPolicy { version: 1, project_store: self.store.clone(), revision, authority: authority::policy_reference(&self.project).unwrap(), limits };
        let (doc, sig) = self.sign("budget.json", &serde_json::to_vec(&policy).unwrap(), authority::BUDGET_SIGNATURE_NAMESPACE);
        self.ok(&["budget", "demo", "import", &doc, &sig, "--expected-head", &self.head().to_string()]);
        self.ok(&["budget", "demo", "inspect"])
    }
    fn wait(&self, args: &[&str]) -> Value { self.ok(&[&["plan", "wait", "demo"], args].concat()) }
    /// Claim the reserved launch as the ticker's launch job would.
    fn claim(&self, reservation: &Value) -> Result<herdr_projects::operations::Claim, herdr_projects::store::StoreError> {
        let operation = OperationId::new(reservation["record"]["operation"].as_str().unwrap()).unwrap();
        let revision = runtime::snapshot(&self.project).unwrap().deliveries.iter().find(|d| d.operation == operation).unwrap().revision;
        migration::open_active(&self.project).unwrap().claim_operation(&operation, revision, "worker", now(), 60_000)
    }
    fn delivery(&self, reservation: &Value) -> herdr_projects::operations::Delivery {
        runtime::snapshot(&self.project).unwrap().deliveries.into_iter().find(|d| d.operation.as_str() == reservation["record"]["operation"]).unwrap()
    }
    /// Record an attempt no reservation produced, through the generic commit.
    fn plant(&self, id: &str, task: &str, state: AttemptState, terminated: bool) {
        let mut db = migration::open_active(&self.project).unwrap();
        let expected = self.attempts().into_iter().find(|a| a.id.as_str() == id).map(|a| a.revision);
        let next = Attempt { id: AttemptId::new(id).unwrap(), task: TaskId::new(task).unwrap(), revision: expected.map_or(1, |r| r + 1), state,
            snapshot: None, reservation: format!("{id}-slot"), termination_observed: terminated };
        db.commit(Commit { expected_head: db.current_head().unwrap(), mutations: vec![Mutation::Attempt { expected, next }] }).unwrap();
    }
}

fn limits(max_attempts: u64, max_provider_tokens: Option<u64>, unknown_usage: UnknownUsagePolicy) -> BudgetLimits {
    BudgetLimits { max_attempts: Some(max_attempts), max_provider_tokens, unknown_usage }
}

/// Unknown provider usage is never counted as zero: a token cap refuses
/// admission unless the owner accepts incomplete usage, and a zero cap always
/// refuses. The attempt count is lifetime: cancelling a never-claimed
/// reservation frees its slot but not its budget unit, and a launch that
/// reaches the cap may still be claimed.
#[test]
fn budget_limits_gate_reservation_and_cancellation_never_refunds() {
    let f = Factory::new(1, &[], &[("a", json!([])), ("b", json!([]))]);
    let a = f.selection("a");
    for (revision, cap, usage, blocker) in [(1, Some(10), UnknownUsagePolicy::Refuse, "provider_usage_unavailable"),
        (2, Some(0), UnknownUsagePolicy::AllowIncomplete, "provider_token_budget_exhausted")] {
        let report = f.budget(revision, limits(1, cap, usage));
        assert_eq!((&report["provider_tokens"], &report["blockers"]), (&json!("unknown"), &json!([blocker])), "{report}");
        assert!(f.blockers("a").contains(&blocker.to_owned()));
        let refused = f.refused(&f.draft_args(&a).iter().map(String::as_str).collect::<Vec<_>>());
        assert!(refused.contains("budget"), "{refused}");
    }
    let report = f.budget(3, limits(1, Some(10), UnknownUsagePolicy::AllowIncomplete));
    assert_eq!((&report["incomplete"], &report["blockers"], &report["admitted_attempts"]), (&json!(true), &json!([]), &json!(0)), "{report}");
    let first = f.reserve("a");
    let report = f.ok(&["budget", "demo", "inspect"]);
    assert_eq!((&report["admitted_attempts"], &report["blockers"]), (&json!(1), &json!(["attempt_budget_exhausted"])));
    let cancelled = f.cancel(first["record"]["attempt"].as_str().unwrap(), 1);
    assert_eq!(cancelled["released"], true);
    // The slot is free again; the budget unit is not.
    assert_eq!(f.queue()["available_slots"], 1);
    let report = f.ok(&["budget", "demo", "inspect"]);
    assert_eq!((&report["admitted_attempts"], &report["blockers"]), (&json!(1), &json!(["attempt_budget_exhausted"])));
    assert!(f.blockers("b").contains(&"attempt_budget_exhausted".to_owned()));
    let refused = f.refused(&f.draft_args(&f.selection("b")).iter().map(String::as_str).collect::<Vec<_>>());
    assert!(refused.contains("budget"), "{refused}");
    // Raising the cap to exactly two admits `b`; reaching the cap does not refuse its own launch claim.
    f.budget(4, limits(2, Some(10), UnknownUsagePolicy::AllowIncomplete));
    let second = f.reserve("b");
    assert_eq!(f.ok(&["budget", "demo", "inspect"])["blockers"], json!(["attempt_budget_exhausted"]));
    let claim = f.claim(&second).unwrap();
    migration::open_active(&f.project).unwrap().validate_claim(&claim, now()).unwrap();
    assert_eq!(f.attempts().len(), 2);
}

/// Cancellation releases a slot only with proof that no launch was ever
/// claimed; a capacity wait wakes exactly then. A claimed launch, even one whose
/// delivery reported no effect and is due for retry, keeps its slot, cannot be
/// claimed again, and the next queued task stays blocked.
#[test]
fn cancellation_releases_capacity_only_without_a_launch_claim() {
    let f = Factory::new(1, &[], &[("a", json!([])), ("b", json!([])), ("c", json!([]))]);
    let a = f.reserve("a");
    let attempt = a["record"]["attempt"].as_str().unwrap();
    assert_eq!((f.queue()["available_slots"].clone(), f.task_state("a")["state"].clone()), (json!(0), json!("running")));
    assert!(f.blockers("b").contains(&"capacity_full".to_owned()));
    let wait = f.wait(&["register", "--task", "b", "--condition", "resource_availability", "--capacity-attempt", attempt, "--capacity-after-revision", "1"]);
    let wait = wait["wait_id"].as_str().unwrap().to_owned();
    assert_eq!(f.wait(&["replay", &wait])["wake_requested"], false);
    let cancelled = f.cancel(attempt, 1);
    assert_eq!((&cancelled["released"], &cancelled["attempt_revision"]), (&json!(true), &json!(2)));
    assert_eq!(f.wait(&["replay", &wait])["wake_requested"], true);
    assert_eq!(f.queue()["available_slots"], 1);
    let shown = f.task_state("a");
    assert_eq!((&shown["state"], &shown["active_attempt"]), (&json!("cancelled"), &Value::Null), "{shown}");
    assert_eq!(f.delivery(&a).state, DeliveryState::PermanentFailure);
    assert!(f.claim(&a).is_err(), "a cancelled launch is never claimed");
    // Repeating the same request is a replay and writes nothing.
    let before = runtime::snapshot(&f.project).unwrap();
    let replay = f.ok(&["task", "demo", "cancel-attempt", attempt, "--expected-revision", "2", "--expected-head", &before.head.to_string(), "--reason", "operator stop"]);
    assert_eq!(replay["released"], true);
    assert_eq!(runtime::snapshot(&f.project).unwrap(), before);
    let rearmed = f.wait(&["rearm", &wait]);
    assert_eq!(f.wait(&["replay", rearmed["wait_id"].as_str().unwrap()])["wake_requested"], true);

    // `b` is claimed, and its launch reports no effect before cancellation.
    let b = f.reserve("b");
    let attempt = b["record"]["attempt"].as_str().unwrap();
    let claim = f.claim(&b).unwrap();
    migration::open_active(&f.project).unwrap().finish_operation(&claim, Outcome::Retryable { no_effect_evidence: "fixture rejected before dispatch".into() }, now()).unwrap();
    let wait = f.wait(&["register", "--task", "c", "--condition", "resource_availability", "--capacity-attempt", attempt, "--capacity-after-revision", "1"]);
    let cancelled = f.cancel(attempt, 1);
    assert_eq!(cancelled["released"], false);
    assert_eq!(f.wait(&["replay", wait["wait_id"].as_str().unwrap()])["wake_requested"], false);
    let queue = f.queue();
    assert_eq!((&queue["available_slots"], &queue["retained_attempts"]), (&json!(0), &json!(1)));
    assert!(f.attempts().iter().find(|x| x.id.as_str() == attempt).unwrap().retains_capacity());
    assert!(f.claim(&b).is_err(), "a cancelled retry is never claimed again");
    assert!(f.blockers("c").contains(&"capacity_full".to_owned()));
    let refused = f.refused(&f.draft_args(&f.selection("c")).iter().map(String::as_str).collect::<Vec<_>>());
    assert!(refused.contains("capacity"), "{refused}");
}

/// Attempts without launch proof, an adopted running worker and a lost one,
/// hold capacity through cancellation: the only slot stays full.
#[test]
fn unproven_attempts_keep_their_slots_through_cancellation() {
    let f = Factory::new(2, &["adopted", "vanished"], &[("next", json!([]))]);
    f.plant("adopted-attempt", "adopted", AttemptState::Running, false);
    f.plant("lost-attempt", "vanished", AttemptState::Lost, false);
    for attempt in ["adopted-attempt", "lost-attempt"] {
        assert_eq!(f.cancel(attempt, 1)["released"], false, "{attempt}");
    }
    let queue = f.queue();
    assert_eq!((&queue["available_slots"], &queue["retained_attempts"]), (&json!(0), &json!(2)));
    assert!(f.attempts().iter().all(Attempt::retains_capacity));
    assert!(f.blockers("next").contains(&"capacity_full".to_owned()));
    let refused = f.refused(&f.draft_args(&f.selection("next")).iter().map(String::as_str).collect::<Vec<_>>());
    assert!(refused.contains("capacity"), "{refused}");
}

/// A dependent is never reserved before its predecessor's verified result,
/// automatic admission without a grant records `authority_missing` and reserves
/// nothing, and a signed launch binds the current satisfaction exactly once.
#[test]
fn dependent_reserves_once_after_verified_evidence_and_never_without_a_grant() {
    let f = Factory::new(1, &["pred"], &[("dep", json!([{"predecessor":"pred","requirement":"verified_result"}]))]);
    let selection = f.selection("dep");
    assert!(f.blockers("dep").contains(&"verified_dependency_evidence_unavailable:pred:verified_result".to_owned()));
    let refused = f.refused(&f.draft_args(&selection).iter().map(String::as_str).collect::<Vec<_>>());
    assert!(refused.contains("not released"), "{refused}");
    // Fixture only: automatic admission is enabled by a signed factory manifest.
    rusqlite::Connection::open(&f.store).unwrap().execute("UPDATE project_control SET factory_admission='on' WHERE singleton=1", []).unwrap();
    herdr_projects::admission::admit_once(&f.project).unwrap();
    assert!(f.attempts().is_empty());
    assert_eq!(f.ok(&["approval", "demo", "denials"]), json!([]), "a blocked dependent is not a missing grant");

    // The predecessor's worker submits and the signed policy verifies it.
    f.plant("pred-attempt", "pred", AttemptState::Running, false);
    let contract = json!({"version":1,"project_store":f.store,"expected_head":f.head(),"task_id":"pred","contract_revision":1,
        "deliverable":"predecessor","non_goals":"no worker launch","acceptance_policies":[{"id":"builds","text":POLICY}],
        "repository":f.repo.canonicalize().unwrap(),"base_oid":f.base,"object_format":"sha256","dependencies":[],"scope":{"paths":[],"named_resources":[]},
        "capability_flags":[],"profile_kind":"codex","retry_class":"none","result_schema_id":"result-v1","route":"verify_only",
        "authority":authority::policy_reference(&f.project).unwrap()});
    let (doc, sig) = f.sign("contract.json", &serde_json::to_vec(&contract).unwrap(), authority::CONTRACT_SIGNATURE_NAMESPACE);
    let digest = f.ok(&["task", "demo", "contract", "put", "--input-file", &doc, "--signature", &sig])["digest"].clone();
    let objects: Vec<_> = f.git(&["rev-list", "--objects", "--all"]).lines().map(|line| {
        let oid = line.split_whitespace().next().unwrap();
        json!({"oid":oid,"relative_path":format!("{}/{}", &oid[..2], &oid[2..])})
    }).collect();
    let submission = f.path("submission.json");
    fs::write(&submission, json!({"idempotency_key":"pred-1","task_id":"pred","contract_revision":1,"contract_digest":digest,"attempt_id":"pred-attempt",
        "repository":f.repo.canonicalize().unwrap(),"base_oid":f.base,"candidate_oid":f.base,"object_format":"sha256","artifact_manifest":[],"claimed_checks":[],"objects":objects}).to_string()).unwrap();
    let submitted = f.ok(&["result", "demo", "submit", "--input-file", submission.to_str().unwrap()]);
    let policy = f.path("policy.json");
    fs::write(&policy, POLICY).unwrap();
    let run = f.ok(&["result", "demo", "verify", submitted["submission_id"].as_str().unwrap(), "--policy-id", "builds", "--policy-file", policy.to_str().unwrap(),
        "--idempotency-key", "pred-verify", "--work-dir", f.path("verify-work").to_str().unwrap()]);
    assert_eq!(run["state"], "accepted", "{run}");
    f.plant("pred-attempt", "pred", AttemptState::Completed, true);
    assert!(!f.blockers("dep").iter().any(|b| b.starts_with("verified_dependency_evidence_unavailable")));

    // Ready, with knowledge, but no signed grant: a denial and no reservation.
    herdr_projects::admission::admit_once(&f.project).unwrap();
    assert_eq!(f.attempts().len(), 1, "only the predecessor's attempt");
    let denials = f.ok(&["approval", "demo", "denials"]);
    assert!(denials.as_array().unwrap().iter().any(|d| d["reason_code"] == "authority_missing" && d["command"] == "admit"), "{denials}");

    let (drafted, approval) = f.approve(&selection);
    let dependencies = &drafted["inputs"]["dependencies"];
    assert_eq!((dependencies.as_array().unwrap().len(), &dependencies[0]["task"], &dependencies[0]["requirement"]), (1, &json!("pred"), &json!("verified_result")));
    let before = runtime::snapshot(&f.project).unwrap();
    assert!(!f.reserve_at(&selection, &approval, before.head - 1).status.success(), "a stale head is refused");
    assert_eq!(runtime::snapshot(&f.project).unwrap(), before);
    let out = f.reserve_at(&selection, &approval, before.head);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let reserved: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(reserved["record"]["inputs"]["dependencies"], *dependencies);
    herdr_projects::admission::admit_once(&f.project).unwrap();
    assert_eq!(f.attempts().iter().filter(|a| a.task.as_str() == "dep").count(), 1);
    assert!(!f.reserve_at(&selection, &approval, f.head()).status.success(), "the running dependent is not reserved again");
}

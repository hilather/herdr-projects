#![cfg(all(feature = "state-store", target_os = "linux"))]
//! Real CLI/signature ingress over a disposable store. The installed native
//! profile is synthetic; this test does not certify an adapter or start workers.
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
use herdr_farm::{authority, domain::*, migration, runtime};
use sha2::{Digest, Sha256};
use std::{fs, os::unix::fs::MetadataExt, path::{Path, PathBuf}, process::{Command, Output}};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-farm");

fn cli(home: &Path, args: &[&str]) -> Output {
    Command::new(BIN).env_clear().env("HOME", home).env("PATH", "/usr/bin:/bin")
        .args(["--root", home.join("root").to_str().unwrap()]).args(args).output().unwrap()
}
fn accepted(output: Output) -> serde_json::Value {
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).unwrap()
}
fn key(home: &Path, name: &str) -> (PathBuf, String) {
    let path = home.join(name);
    assert!(Command::new("/usr/bin/ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&path).status().unwrap().success());
    let public = fs::read_to_string(path.with_extension("pub")).unwrap().split_whitespace().take(2).collect::<Vec<_>>().join(" ");
    (path, public)
}
fn sign(key: &Path, path: &Path, bytes: &[u8], namespace: &str) -> PathBuf {
    fs::write(path, bytes).unwrap();
    let signature = PathBuf::from(format!("{}.sig", path.display()));
    if signature.exists() { fs::remove_file(&signature).unwrap(); }
    let output = Command::new("/usr/bin/ssh-keygen").args(["-Y", "sign", "-f"]).arg(key)
        .args(["-n", namespace]).arg(path).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    signature
}

/// Retained worker knowledge for `task`; an automatic draft binds it for the brief.
fn worker_snapshot(project: &Path, profile: &FrozenProfile, task: &str) {
    let mut memory = herdr_farm::memory::MemoryStore::from_sqlite(migration::open_active(project).unwrap(), project.join(".state/objects"));
    memory.create_worker_snapshot(SnapshotRequest { schema_version: 1, task_id: task.into(), profile: profile.name.clone(), domains: vec![], paths: vec![], pinned_keys: vec![], sensitivity: "default".into() },
        &profile.name, &profile.definition_digest, profile.config.digest.as_deref(), 32000, "Delegated fixture instructions", jiff::Timestamp::now().as_millisecond(), None).unwrap();
}

fn synthetic_profile(project: &Path, config: &Path) -> FrozenProfile {
    let evidence = VersionedReference { id: "synthetic-cli-fixture".into(), revision: 1, digest: "a".repeat(64) };
    let supported = CapabilityEvidence::Supported { evidence: evidence.clone() };
    let profile = FrozenProfile {
        version: 1, name: "fixture".into(), kind: "codex".into(), definition_digest: "b".repeat(64),
        config: migration::config_reference(config).unwrap(), arguments_digest: "c".repeat(64),
        environment_names: vec![], execution_home: None, permission_policy: authority::policy_reference(project).unwrap(),
        adapter: evidence,
        agent: ExecutableIdentity { path: "/fixture/no-worker".into(), digest: "d".repeat(64), version: "1.0.0".into() },
        herdr: ExecutableIdentity { path: "/fixture/no-herdr".into(), digest: "e".repeat(64), version: "1.0.0".into() },
        capabilities: ProfileCapabilities {
            launch: supported.clone(), readiness_observation: supported.clone(), prompt_submission: supported.clone(), stop: supported,
            checkpoint_acknowledgment: CapabilityEvidence::Unknown, structured_usage: CapabilityEvidence::Unknown, resume: CapabilityEvidence::Unknown,
        }, workflow_certificate: None,
    };
    profile.validate_for_launch().unwrap();
    let path = project.join(".state/state.db").canonicalize().unwrap();
    let metadata = fs::metadata(&path).unwrap();
    let report = serde_json::json!({"preparation":{"profile":profile,"reference":profile.reference().unwrap(),"launchable":true,"protocol_capable":false,"certified":false},"source_store":[path,metadata.dev(),metadata.ino()]});
    let payload = serde_json::to_string(&report).unwrap();
    let db = rusqlite::Connection::open(path).unwrap();
    db.execute("INSERT INTO native_profiles(profile_digest,report,report_digest,sequence) VALUES(?1,?2,?3,(SELECT max(sequence) FROM events))",
        rusqlite::params![profile.reference().unwrap().digest, payload, format!("{:x}", Sha256::digest(payload.as_bytes()))]).unwrap();
    profile
}

#[test]
fn signed_cli_reservation_enforces_subject_scope_quotas_and_replays_after_revocation() {
    delegated_scenario();
}

#[test]
fn delegated_reserve_logs_grant_once() {
    let s = delegated_scenario();
    let p = &s.policy;
    // Contracts §2 bytes of `synthetic_profile`, written out by hand.
    let configuration = format!(concat!(r#"{{"adapter":{{"digest":"{a}","id":"synthetic-cli-fixture","revision":1}},"agent_digest":"{d}","agent_version":"1.0.0","#,
        r#""arguments_digest":"{c}","definition_digest":"{b}","environment_names":[],"kind":"codex","#,
        r#""permission_policy":{{"digest":"{pd}","id":"{pi}","revision":{pr}}},"reasoning_effort":null,"reasoning_effort_reason":"mapping_unverified","#,
        r#""requested_model":null,"requested_model_reason":"mapping_unverified","schema":"agent_configuration.v1"}}"#),
        a = "a".repeat(64), b = "b".repeat(64), c = "c".repeat(64), d = "d".repeat(64), pd = p.digest, pi = p.id, pr = p.revision);
    let id = format!("sha256:{:x}", Sha256::digest(configuration.as_bytes()));
    let raw = rusqlite::Connection::open(&s.db).unwrap();
    assert_eq!(raw.query_row("SELECT configuration_id,canonical_json FROM agent_configurations", [], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).unwrap(), (id.clone(), configuration));
    let eligible = format!(r#"[{{"configuration_id":"{id}","probability_ppm":1000000,"profile_digest":"{}","status":"chosen"}}]"#, s.profile);
    // Two reservations and two replays of the first: one decision per attempt.
    let rows: Vec<(String, String, String, String, String, String, Option<String>)> = raw.prepare("SELECT attempt_id,chosen_configuration_id,eligible,chooser_kind,chooser_principal,reason_codes,note FROM dispatch_decisions ORDER BY rowid").unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))).unwrap().map(Result::unwrap).collect();
    let row = |attempt: &str| (attempt.to_owned(), id.clone(), eligible.clone(), "delegated".to_owned(), format!("grant:{}", s.grant_id), r#"["delegated_grant","only_eligible"]"#.to_owned(), None);
    assert_eq!(rows, [row(&s.attempts[0]), row(&s.attempts[1])]);
}

struct Scenario { _home: tempfile::TempDir, db: PathBuf, grant_id: String, attempts: [String; 2], policy: VersionedReference, profile: String }

const VERIFIER_POLICY: &str = r#"{"version":1,"checks":["/usr/bin/git","diff","--quiet"]}"#;

fn git(home: &Path, repository: &Path, args: &[&str]) -> String {
    let output = Command::new("/usr/bin/git").env_clear().env("PATH", "/usr/bin:/bin")
        .env("HOME", home).env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "fixture").env("GIT_AUTHOR_EMAIL", "fixture@example.com")
        .env("GIT_COMMITTER_NAME", "fixture").env("GIT_COMMITTER_EMAIL", "fixture@example.com")
        .current_dir(repository).args(args).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

struct Installed { home: tempfile::TempDir, owner: PathBuf, subject: PathBuf, project: PathBuf, config: PathBuf, path: PathBuf, db: herdr_farm::store::SqliteStore,
    repository: PathBuf, oid: String, authority: VersionedReference, first_contract: serde_json::Value, grant_id: String, profile: FrozenProfile, document: PathBuf }

/// An active project with queued tasks `a`, `b` and `c`, each with a signed
/// contract writing `writes`, a signed budget, and an installed delegation
/// grant for `concurrent` attempts at once and two in its lifetime.
fn install(concurrent: u32, writes: serde_json::Value) -> Installed {
    let home = tempfile::tempdir().unwrap();
    let (owner, public) = key(home.path(), "owner");
    let (subject, subject_public) = key(home.path(), "subject");
    for command in ["new", "pause"] { assert!(cli(home.path(), &[command, "demo"]).status.success()); }
    let project = home.path().join("root/demo");
    let config = home.path().join("owner.toml");
    fs::write(&config, format!("[authority]\nversion=1\nrevision=1\napproval_public_key={public:?}\n")).unwrap();
    let plan = migration::inspect_with_config(&project, &config).unwrap();
    migration::apply(&project, &plan, true).unwrap();
    let path = project.join(".state/state.db").canonicalize().unwrap();
    let store = path.to_str().unwrap();
    let mut db = migration::open_active(&project).unwrap();
    for name in ["a", "b", "c"] {
        let task = TaskId::new(name).unwrap();
        db.commit(Commit { expected_head: db.current_head().unwrap(), mutations: vec![Mutation::Task { expected: None, next: Task {
            id: task.clone(), revision: 1, state: TaskState::Draft, title: name.into(), active_attempt: None,
        } }] }).unwrap();
        db.create_runtime(Some(&task), Some(1), db.current_head().unwrap(), &RuntimeRoute::default()).unwrap();
        db.queue_task(&task, 2, db.current_head().unwrap(), &QueueRequest { priority: 0, dependencies: vec![] }, 0).unwrap();
    }
    let snapshot = db.read_snapshot(None).unwrap();
    db.set_scheduler_policy(snapshot.head, snapshot.scheduler.unwrap().policy.revision, 2, 3).unwrap();
    let snapshot = db.read_snapshot(None).unwrap();
    let observations = snapshot.runtime_bindings.iter().map(|binding| herdr_farm::reconcile::RuntimeObservation {
        binding: binding.id.clone(), binding_revision: binding.revision, task_revision: Some(3),
        config_digest: migration::config_reference(&config).unwrap().digest,
        observed_unix_ms: jiff::Timestamp::now().as_millisecond(), collector: "herdr-git-v1".into(), ..Default::default()
    }).collect::<Vec<_>>();
    db.record_observations(snapshot.head, &observations).unwrap();
    let snapshot = db.read_snapshot(None).unwrap();
    runtime::set_state(&project, snapshot.head, snapshot.control.unwrap().revision, ProjectState::Active, &config).unwrap();
    let profile = synthetic_profile(&project, &config);
    let repository = home.path().join("repo"); fs::create_dir(&repository).unwrap();
    let git = |args: &[&str]| git(home.path(), &repository, args);
    git(&["init", "--object-format=sha1"]); git(&["commit", "--allow-empty", "-m", "fixture"]);
    let oid = git(&["rev-parse", "HEAD"]);
    let authority = authority::policy_reference(&project).unwrap();
    let document = home.path().join("owner-document.json");
    let mut contracts = Vec::new();
    let mut first_contract = serde_json::Value::Null;
    for task in ["a", "b", "c"] {
        let body = serde_json::json!({"version":1,"project_store":store,"expected_head":db.current_head().unwrap(),"task_id":task,"contract_revision":1,
            "deliverable":"CLI fixture","non_goals":"no worker launch","acceptance_policies":[{"id":"builds","text":VERIFIER_POLICY}],
            "repository":repository,"base_oid":oid,"object_format":"sha1","dependencies":[],"scope":{"paths":writes,"named_resources":[]},
            "capability_flags":[],"profile_kind":"codex","retry_class":"none","result_schema_id":"result-v1","route":"verify_only","authority":authority});
        if task == "a" { first_contract = body.clone(); }
        let signature = sign(&owner, &document, &serde_json::to_vec(&body).unwrap(), authority::CONTRACT_SIGNATURE_NAMESPACE);
        let receipt = accepted(cli(home.path(), &["task","demo","contract","put","--input-file",document.to_str().unwrap(),"--signature",signature.to_str().unwrap()]));
        contracts.push(VersionedReference { id: task.into(), revision: 1, digest: receipt["digest"].as_str().unwrap().into() });
    }
    let policy = BudgetPolicy { version: 1, project_store: store.into(), revision: 1, authority: authority.clone(),
        limits: BudgetLimits { max_attempts: Some(10), max_provider_tokens: None, unknown_usage: UnknownUsagePolicy::Refuse } };
    let signature = sign(&owner, &document, &serde_json::to_vec(&policy).unwrap(), authority::BUDGET_SIGNATURE_NAMESPACE);
    accepted(cli(home.path(), &["budget","demo","import",document.to_str().unwrap(),signature.to_str().unwrap(),"--expected-head",&db.current_head().unwrap().to_string()]));
    let grant = serde_json::json!({"version":2,"issuer":"owner","subject":"delegate","subject_public_key":subject_public,
        "action_classes":["reserve_attempt"],"repositories":[{"repository":repository,"ref":"refs/heads/factory"}],"profile_kinds":["codex"],
        "max_concurrent_attempts":concurrent,"expires_unix_ms":9_000_000_000_000i64,"revocation_epoch":1,"child_delegation":"forbidden",
        "policy_revision":authority.revision,"project_store":store,"authority":authority,
        "reservation_scope":{"task_contracts":contracts,"profiles":[profile.reference().unwrap()],"budget":policy.reference().unwrap(),
            "repository_bases":[{"repository":repository,"ref":"refs/heads/factory","commit_oid":oid,"object_format":"sha1"}],"max_total_attempts":2}});
    let signature = sign(&owner, &document, &serde_json::to_vec(&grant).unwrap(), authority::DELEGATION_SIGNATURE_NAMESPACE);
    let receipt = accepted(cli(home.path(), &["delegation","demo","import",document.to_str().unwrap(),signature.to_str().unwrap()]));
    let grant_id = receipt["grant_id"].as_str().unwrap().to_owned();
    for task in ["a", "b", "c"] { worker_snapshot(&project, &profile, task); }
    Installed { home, owner, subject, project, config, path, db, repository, oid, authority, first_contract, grant_id, profile, document }
}

/// Signed delegated reservations through the CLI: scope, quota and signature
/// refusals, two accepted reservations and replays of the first.
fn delegated_scenario() -> Scenario {
    let Installed { home, owner, subject, project: _, config, path, mut db, repository, oid, authority, mut first_contract, grant_id, profile, document } = install(1, serde_json::json!([]));
    let (grant_id, verifier_policy) = (grant_id.as_str(), VERIFIER_POLICY);
    let git = |args: &[&str]| git(home.path(), &repository, args);
    let draft = |key: &str| accepted(cli(home.path(), &["delegation","demo","draft",grant_id,"--idempotency-key",key]));
    let request_path = home.path().join("request.json");
    let before_draft = db.read_snapshot(None).unwrap();
    let first = draft("first");
    assert_eq!(first["inputs"]["task"], "a");
    assert_eq!(db.read_snapshot(None).unwrap(), before_draft, "a draft writes nothing");
    let first_bytes = serde_json::to_vec(&first).unwrap();
    let reserve = |path: &Path, signature: &Path| cli(home.path(), &["delegation","demo","reserve",path.to_str().unwrap(),signature.to_str().unwrap()]);
    for (key, namespace) in [(&owner, authority::DELEGATED_RESERVATION_SIGNATURE_NAMESPACE), (&subject, authority::DELEGATION_SIGNATURE_NAMESPACE)] {
        let signature = sign(key, &request_path, &first_bytes, namespace);
        let output = reserve(&request_path, &signature);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("signature"));
        assert!(db.read_snapshot(None).unwrap().attempts.is_empty());
    }
    let mut widened = first.clone(); widened["idempotency_key"] = "out-of-scope".into(); widened["inputs"]["budget"] = serde_json::Value::Null;
    let signature = sign(&subject, &request_path, &serde_json::to_vec(&widened).unwrap(), authority::DELEGATED_RESERVATION_SIGNATURE_NAMESPACE);
    let rejected = reserve(&request_path, &signature);
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("scope"));
    assert!(db.read_snapshot(None).unwrap().attempts.is_empty());
    let signature = sign(&subject, &request_path, &first_bytes, authority::DELEGATED_RESERVATION_SIGNATURE_NAMESPACE);
    let mut changed_bytes = first_bytes.clone(); changed_bytes.push(b'\n'); fs::write(&request_path, changed_bytes).unwrap();
    assert!(!reserve(&request_path, &signature).status.success());
    fs::write(&request_path, &first_bytes).unwrap();
    let first_output = reserve(&request_path, &signature); let first_stdout = first_output.stdout.clone();
    let first_receipt = accepted(first_output);
    assert_eq!(reserve(&request_path, &signature).stdout, first_stdout);
    assert_eq!(db.read_snapshot(None).unwrap().attempts.len(), 1);
    // An owner may retain a newer contract revision, but an already reserved
    // attempt cannot claim that revision as its execution contract.
    let frozen_reference = first_receipt["record"]["inputs"]["task_contract"].clone();
    first_contract["contract_revision"] = 2.into();
    first_contract["expected_head"] = db.current_head().unwrap().into();
    let signature_v2 = sign(&owner, &document, &serde_json::to_vec(&first_contract).unwrap(), authority::CONTRACT_SIGNATURE_NAMESPACE);
    let newer = accepted(cli(home.path(), &["task","demo","contract","put","--input-file",document.to_str().unwrap(),"--signature",signature_v2.to_str().unwrap()]));
    let objects: Vec<_> = git(&["rev-list","--objects","--all"]).lines().map(|line| {
        let oid = line.split_whitespace().next().unwrap();
        serde_json::json!({"oid":oid,"relative_path":format!("{}/{}", &oid[..2], &oid[2..])})
    }).collect();
    let mut result = serde_json::json!({"idempotency_key":"reserved-result","task_id":"a","contract_revision":2,
        "contract_digest":newer["digest"],"attempt_id":first_receipt["record"]["attempt"],"repository":repository,
        "base_oid":oid,"candidate_oid":oid,"object_format":"sha1","artifact_manifest":[],"claimed_checks":[],"objects":objects});
    let result_path = home.path().join("result.json");
    fs::write(&result_path, serde_json::to_vec(&result).unwrap()).unwrap();
    let before_result = db.read_snapshot(None).unwrap();
    let rejected = cli(home.path(), &["result","demo","submit","--input-file",result_path.to_str().unwrap()]);
    assert!(!rejected.status.success(), "accepted a result for a contract not bound by reservation");
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("contract differs from its reservation"));
    assert_eq!(db.read_snapshot(None).unwrap(), before_result);
    // The original signed revision remains valid for the original attempt after
    // supersession. Submission is evidence intake, not termination or success.
    result["contract_revision"] = frozen_reference["revision"].clone();
    result["contract_digest"] = frozen_reference["digest"].clone();
    fs::write(&result_path, serde_json::to_vec(&result).unwrap()).unwrap();
    let receipt = accepted(cli(home.path(), &["result","demo","submit","--input-file",result_path.to_str().unwrap()]));
    let replay = accepted(cli(home.path(), &["result","demo","submit","--input-file",result_path.to_str().unwrap()]));
    assert_eq!(receipt["submission_id"], replay["submission_id"]);
    assert_eq!(replay["replayed"], true);
    let after_result = db.read_snapshot(None).unwrap();
    assert_eq!(after_result.attempts, before_result.attempts);
    assert_eq!(after_result.tasks, before_result.tasks);
    assert_eq!(after_result.attempt_inputs, before_result.attempt_inputs);
    let raw_result = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(raw_result.query_row("SELECT count(*) FROM result_submissions", [], |row| row.get::<_,u64>(0)).unwrap(), 1);
    for table in ["verified_results","dependency_satisfactions"] {
        assert_eq!(raw_result.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row.get::<_,u64>(0)).unwrap(), 0);
    }
    let policy_path = home.path().join("verify-policy.json");
    let work_path = home.path().join("verification-work");
    fs::write(&policy_path, verifier_policy).unwrap();
    let verify = |key: &str| cli(home.path(), &["result","demo","verify",receipt["submission_id"].as_str().unwrap(),
        "--policy-id","builds","--policy-file",policy_path.to_str().unwrap(),"--idempotency-key",key,
        "--work-dir",work_path.to_str().unwrap(),"--timeout-seconds","30"]);
    fs::create_dir(&work_path).unwrap(); fs::write(work_path.join("sentinel"), b"keep").unwrap();
    assert!(!verify("verify-original").status.success());
    assert_eq!(fs::read(work_path.join("sentinel")).unwrap(), b"keep");
    fs::remove_dir_all(&work_path).unwrap();
    fs::write(&policy_path, vec![b' '; 4_001]).unwrap();
    let oversized = verify("oversized-policy");
    assert!(!oversized.status.success()); assert!(!work_path.exists());
    assert!(String::from_utf8_lossy(&oversized.stderr).contains("4000 bytes"));
    let backing = home.path().join("policy-backing.json"); fs::write(&backing, verifier_policy).unwrap();
    fs::remove_file(&policy_path).unwrap(); std::os::unix::fs::symlink(&backing, &policy_path).unwrap();
    assert!(!verify("symlink-policy").status.success()); assert!(!work_path.exists());
    assert_eq!(fs::read_to_string(&backing).unwrap(), verifier_policy);
    fs::remove_file(&policy_path).unwrap();
    assert_eq!(raw_result.query_row("SELECT count(*) FROM verification_runs", [], |row| row.get::<_,u64>(0)).unwrap(), 0);
    // A caller-chosen policy cannot replace the one signed in the contract.
    fs::write(&policy_path, r#"{"version":1,"checks":["/usr/bin/git","status"]}"#).unwrap();
    let rejected = verify("wrong-policy");
    assert!(!rejected.status.success());
    let rejected: serde_json::Value = serde_json::from_slice(&rejected.stdout).unwrap();
    assert_eq!(rejected["state"], "rejected");
    assert_eq!(rejected["reason"], "policy_digest_mismatch");
    assert!(rejected["receipt"].is_null()); assert!(!work_path.exists());
    assert_eq!(raw_result.query_row("SELECT count(*) FROM verified_results", [], |row| row.get::<_,u64>(0)).unwrap(), 0);
    fs::write(&policy_path, verifier_policy).unwrap();
    let verified = accepted(verify("verify-original"));
    assert_eq!(verified["state"], "accepted", "{verified}");
    assert!(verified["receipt"]["result_id"].is_string()); assert!(!work_path.exists());
    let replay = accepted(verify("verify-original"));
    assert_eq!(replay["run_id"], verified["run_id"]); assert_eq!(replay["replayed"], true);
    assert_eq!(raw_result.query_row("SELECT count(*) FROM verified_results", [], |row| row.get::<_,u64>(0)).unwrap(), 1);
    assert_eq!(db.read_snapshot(None).unwrap().attempts, before_result.attempts);
    let second_path = home.path().join("second.json");
    let second = draft("second"); assert_eq!(second["inputs"]["task"], "b");
    let before_second = db.read_snapshot(None).unwrap();
    let second_signature = sign(&subject, &second_path, &serde_json::to_vec(&second).unwrap(), authority::DELEGATED_RESERVATION_SIGNATURE_NAMESPACE);
    let rejected = reserve(&second_path, &second_signature);
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("concurrent"));
    assert_eq!(db.read_snapshot(None).unwrap(), before_second);
    assert!(before_second.attempts[0].retains_capacity());
    let cancelled = accepted(cli(home.path(), &["task","demo","cancel-attempt",first_receipt["record"]["attempt"].as_str().unwrap(),
        "--expected-revision","1","--expected-head",&db.current_head().unwrap().to_string(),"--reason","no launch requested"]));
    assert_eq!(cancelled["released"], true);
    let budget_report=accepted(cli(home.path(), &["budget","demo","inspect"]));
    assert_eq!(budget_report["admitted_attempts"],1,"cancellation must not refund lifetime usage");
    assert_eq!(budget_report["policy"]["limits"]["max_attempts"],10);
    assert_eq!(budget_report["blockers"],serde_json::json!([]));
    let second = draft("second");
    let second_signature = sign(&subject, &second_path, &serde_json::to_vec(&second).unwrap(), authority::DELEGATED_RESERVATION_SIGNATURE_NAMESPACE);
    let second_receipt = accepted(reserve(&second_path, &second_signature));
    assert_eq!(db.read_snapshot(None).unwrap().attempts.len(), 2);
    let budget_report=accepted(cli(home.path(), &["budget","demo","inspect"]));
    assert_eq!(budget_report["admitted_attempts"],2);
    let third = draft("third"); assert_eq!(third["inputs"]["task"], "c");
    let third_path = home.path().join("third.json");
    let third_signature = sign(&subject, &third_path, &serde_json::to_vec(&third).unwrap(), authority::DELEGATED_RESERVATION_SIGNATURE_NAMESPACE);
    let rejected = reserve(&third_path, &third_signature);
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("lifetime"));
    assert_eq!(db.read_snapshot(None).unwrap().attempts.len(), 2);
    accepted(cli(home.path(), &["delegation","demo","revoke",grant_id,"--expected-head",&db.current_head().unwrap().to_string(),"--reason","end fixture"]));
    let operation = OperationId::new(second_receipt["record"]["operation"].as_str().unwrap()).unwrap();
    assert!(db.claim_operation(&operation, 1, "worker", jiff::Timestamp::now().as_millisecond(), 1000).is_err());
    // Even a later configuration change cannot turn replay into a fresh effect.
    let original_config = fs::read_to_string(&config).unwrap(); fs::write(&config, format!("{original_config}\n# changed after acceptance\n")).unwrap();
    let replay = reserve(&request_path, &signature); assert!(replay.status.success()); assert_eq!(replay.stdout, first_stdout);
    let mut altered = first; altered["issued_unix_ms"] = (altered["issued_unix_ms"].as_i64().unwrap()+1).into();
    let signature = sign(&subject, &request_path, &serde_json::to_vec(&altered).unwrap(), authority::DELEGATED_RESERVATION_SIGNATURE_NAMESPACE);
    assert!(!reserve(&request_path, &signature).status.success());
    let raw = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(raw.query_row("SELECT count(*) FROM delegated_reservations", [], |row| row.get::<_,u64>(0)).unwrap(), 2);
    assert_eq!(raw.query_row("SELECT count(*) FROM approval_uses", [], |row| row.get::<_,u64>(0)).unwrap(), 0);
    // Telemetry: one classification per reserved task; replays and refusals add none. Empty scope: read_only, 0 points.
    let classified: Vec<(String, i64, String, String)> = raw.prepare("SELECT task_id,contract_revision,class,band FROM task_classifications ORDER BY task_id").unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap().map(Result::unwrap).collect();
    assert_eq!(classified, [("a".into(), 1, "read_only".into(), "small".into()), ("b".into(), 1, "read_only".into(), "small".into())]);
    assert!(db.read_snapshot(None).unwrap().attempts.iter().any(|attempt| attempt.retains_capacity()));
    let attempts = [&first_receipt, &second_receipt].map(|receipt| receipt["record"]["attempt"].as_str().unwrap().to_owned());
    Scenario { grant_id: grant_id.to_owned(), attempts, policy: authority, profile: profile.reference().unwrap().digest, db: path, _home: home }
}

impl Installed {
    fn cli(&self, args: &[&str]) -> Output { cli(self.home.path(), args) }
    fn draft(&self, key: &str) -> Output { self.cli(&["delegation","demo","draft",&self.grant_id,"--idempotency-key",key]) }
    /// Sign `request` as the delegated subject and submit it.
    fn reserve(&self, request: &serde_json::Value) -> Output {
        let path = self.home.path().join(format!("{}.json", request["idempotency_key"].as_str().unwrap()));
        let signature = sign(&self.subject, &path, &serde_json::to_vec(request).unwrap(), authority::DELEGATED_RESERVATION_SIGNATURE_NAMESPACE);
        self.cli(&["delegation","demo","reserve",path.to_str().unwrap(),signature.to_str().unwrap()])
    }
    fn head(&self) -> String { self.db.current_head().unwrap().to_string() }
    /// Requeue `task` at `priority` so the next draft picks the highest.
    fn requeue(&self, task: &str, priority: i32) {
        let request = self.home.path().join(format!("{task}-queue.json"));
        fs::write(&request, serde_json::json!({"priority":priority,"dependencies":[]}).to_string()).unwrap();
        let revision = runtime::snapshot(&self.project).unwrap().tasks.into_iter().find(|t| t.id.as_str() == task).unwrap().revision.to_string();
        accepted(self.cli(&["task","demo","queue",task,"--input-file",request.to_str().unwrap(),"--expected-revision",&revision,"--expected-head",&self.head()]));
        // A new task revision needs fresh binding observations and knowledge.
        let snapshot = runtime::snapshot(&self.project).unwrap();
        let observations = snapshot.runtime_bindings.iter().map(|binding| herdr_farm::reconcile::RuntimeObservation {
            binding: binding.id.clone(), binding_revision: binding.revision,
            task_revision: snapshot.tasks.iter().find(|t| Some(&t.id) == binding.task.as_ref()).map(|t| t.revision),
            config_digest: migration::config_reference(&self.config).unwrap().digest,
            observed_unix_ms: jiff::Timestamp::now().as_millisecond(), collector: "herdr-git-v1".into(), ..Default::default() }).collect::<Vec<_>>();
        migration::open_active(&self.project).unwrap().record_observations(snapshot.head, &observations).unwrap();
        worker_snapshot(&self.project, &self.profile, task);
    }
}

/// Cancelling a claimed delegated launch does not free its slot, even once the
/// claim's lease has expired: the worker's termination was never observed.
#[test]
fn a_cancelled_claimed_delegated_launch_keeps_its_slot_after_the_lease_expires() {
    let mut s = install(1, serde_json::json!([]));
    let first = accepted(s.reserve(&accepted(s.draft("first"))));
    // The ticker's launch job is the only other caller of the claim.
    let operation = OperationId::new(first["record"]["operation"].as_str().unwrap()).unwrap();
    s.db.claim_operation(&operation, 1, "worker", jiff::Timestamp::now().as_millisecond(), 1_000).unwrap();
    let attempt = first["record"]["attempt"].as_str().unwrap();
    let cancelled = accepted(s.cli(&["task","demo","cancel-attempt",attempt,"--expected-revision","1","--expected-head",&s.head(),"--reason","stop uncertain worker"]));
    assert_eq!(cancelled["released"], false);
    std::thread::sleep(std::time::Duration::from_millis(1_100));
    let second = accepted(s.draft("second"));
    assert_eq!(second["inputs"]["task"], "b");
    let before = runtime::snapshot(&s.project).unwrap();
    let refused = s.reserve(&second);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("concurrent"), "{}", String::from_utf8_lossy(&refused.stderr));
    assert_eq!(runtime::snapshot(&s.project).unwrap(), before);
    assert!(before.attempts.iter().all(Attempt::retains_capacity));
}

/// With automatic admission off (the default), a grant with room for two
/// attempts is offered no draft for, and cannot reserve, a task whose signed
/// write scope overlaps a retained attempt's.
#[test]
fn delegated_draft_and_reserve_refuse_an_overlapping_write_scope_with_admission_off() {
    let s = install(2, serde_json::json!([{"path":"shared.txt","access":"write"}]));
    assert_eq!(accepted(s.cli(&["scheduler","demo","inspect"]))["capability"]["automatic_admission"], false);
    // Draft `b` while it ranks first, then reserve `a` ahead of it.
    s.requeue("b", 1);
    let mut second = accepted(s.draft("second"));
    assert_eq!(second["inputs"]["task"], "b");
    s.requeue("a", 2);
    let first = accepted(s.draft("first"));
    assert_eq!(first["inputs"]["task"], "a");
    accepted(s.reserve(&first));
    let before = runtime::snapshot(&s.project).unwrap();
    // The ranker skips the conflicting task, so no draft is offered for it.
    let refused = s.draft("second-again");
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("no ready candidate"), "{}", String::from_utf8_lossy(&refused.stderr));
    second["expected_head"] = before.head.into();
    let refused = s.reserve(&second);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("resource_conflict"), "{}", String::from_utf8_lossy(&refused.stderr));
    assert_eq!(runtime::snapshot(&s.project).unwrap(), before);
}

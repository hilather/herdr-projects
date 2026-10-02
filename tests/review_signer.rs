//! Trusted reviewer-signer process end to end (contracts-review.md §12, card
//! D10): `telemetry <slug> review signer init|run|status` over a real
//! reviewer launched from an assignment (D9 `launch reserve`) that submits its
//! own receipt, owner-signed grants (D8) and real `ssh-keygen` keys generated
//! in temporary homes, plus the canonical worker sandbox
//! (`worker_supervision::isolated_gated_command`) reading the signer key.

#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.

mod support;

use herdr_farm::{authority, domain::*, migration, runtime};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, os::unix::fs::{MetadataExt, PermissionsExt}, path::{Path, PathBuf}, process::{Command, Output, Stdio}};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-farm");
const GRANT_NS: &str = "code-review-authority@herdr-projects";
const REVOKE_NS: &str = "code-review-revocation@herdr-projects";
const AUTHOR_ATTEMPT: &str = "author-attempt-0001";
const AUTHOR_CONFIGURATION_JSON: &str = r#"{"kind":"codex","schema":"agent_configuration.v1","sentinel":"author"}"#;
/// The policy `signer init` writes, byte for byte, and its sha256 (computed
/// with `sha256sum` outside the crate).
const DEFAULT_POLICY: &str = "{\n  \"schema\": \"review_signer_policy.v1\",\n  \"revision\": 1,\n  \"require_worker_receipt\": true,\n  \"min_evidence_refs\": 0,\n  \"on_failure\": \"reject\"\n}\n";
const DEFAULT_POLICY_DIGEST: &str = "sha256:e35de0ca9b0bdbcfe9f876799d3b10e72984d29dd2810387665df9672d89d4f6";
/// Revision 2: at least one evidence reference.
const POLICY_2: &str = "{\n  \"schema\": \"review_signer_policy.v1\",\n  \"revision\": 2,\n  \"require_worker_receipt\": true,\n  \"min_evidence_refs\": 1,\n  \"on_failure\": \"reject\"\n}\n";
const POLICY_2_DIGEST: &str = "sha256:47539cd4a2be3403818a548417036c514b2ea7c60306f74c20861a9c3c116ec0";

/// An active project with an owner key in a temporary `HOME` (its pinned
/// configuration at `HOME/.config/herdr-farm/config.toml`), a SHA-256
/// repository, the queued task `work` bound to a (never started) Herdr route
/// and the `worker` profile (Claude, execution home `HOME/agent-home`).
struct Lab { home: tempfile::TempDir, project: PathBuf, key: PathBuf, repo: PathBuf, profile: VersionedReference, binding: String }

/// Task `authored`'s submission by `AUTHOR_ATTEMPT` (a `codex` dispatch) at
/// `candidate`, its review opportunity blindly assigned to `worker`, and task
/// `work`'s launch selection with the opportunity's blind brief snapshot.
struct World { opportunity: String, submission: String, candidate: String, repository: String, store: String, selection: PathBuf }

impl Lab {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let key = home.path().join("owner");
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&key).output().unwrap().status.success());
        let public = fs::read_to_string(key.with_extension("pub")).unwrap().split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        let config = home.path().join(".config/herdr-farm/config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, format!("[authority]\nversion=1\nrevision=1\napproval_public_key={public:?}\n[profiles.worker]\nkind='claude'\npermission_policy='interactive'\n[profiles.worker.budget]\nmax_wall_seconds=600\nunknown_usage='allow_with_warning'\n")).unwrap();
        for dir in ["repo", "bin", "agent-home", "lab"] { fs::create_dir(home.path().join(dir)).unwrap(); }
        let mut lab = Lab { project: home.path().join("root/demo"), key, repo: home.path().join("repo"),
            profile: VersionedReference { id: String::new(), revision: 1, digest: String::new() }, binding: String::new(), home };
        for command in ["new", "pause"] { lab.ok(&[command, "demo"]); }
        migration::apply(&lab.project, &migration::inspect_with_config(&lab.project, &config).unwrap(), true).unwrap();
        lab.git(&["init", "-q", "--object-format=sha256"]);
        lab.git(&["commit", "-q", "--allow-empty", "-m", "base"]);
        lab.ok(&["task", "demo", "add", "work", "--title", "work", "--expected-head", &lab.head().to_string()]);
        let request = lab.path("queue.json");
        fs::write(&request, r#"{"priority":0,"dependencies":[]}"#).unwrap();
        lab.ok(&["task", "demo", "queue", "work", "--input-file", request.to_str().unwrap(), "--expected-revision", "1", "--expected-head", &lab.head().to_string()]);
        let policy = runtime::snapshot(&lab.project).unwrap().scheduler.unwrap().policy.revision.to_string();
        lab.ok(&["scheduler", "demo", "policy", "--max-active-workers", "1", "--max-attempts-per-task", "3", "--expected-revision", &policy, "--expected-head", &lab.head().to_string()]);
        let route = RuntimeRoute { socket: lab.path("lab/native.sock").display().to_string(), cwd: lab.repo.canonicalize().unwrap().display().to_string(), ..Default::default() };
        let id = TaskId::new("work").unwrap();
        let revision = runtime::snapshot(&lab.project).unwrap().tasks.into_iter().find(|t| t.id == id).unwrap().revision;
        lab.binding = runtime::create_binding(&lab.project, Some(&id), Some(revision), lab.head(), &route).unwrap().binding.id;
        let state = runtime::snapshot(&lab.project).unwrap();
        let observations = state.runtime_bindings.iter().map(|binding| herdr_farm::reconcile::RuntimeObservation { binding: binding.id.clone(), binding_revision: binding.revision,
            task_revision: binding.task.as_ref().map(|id| state.tasks.iter().find(|t| &t.id == id).unwrap().revision),
            observed_unix_ms: jiff::Timestamp::now().as_millisecond(), collector: "herdr-git-v2".into(),
            config_digest: migration::config_reference(&config).unwrap().digest.clone(), ..Default::default() }).collect::<Vec<_>>();
        migration::open_active(&lab.project).unwrap().record_observations(state.head, &observations).unwrap();
        let control = runtime::snapshot(&lab.project).unwrap().control.unwrap().revision.to_string();
        lab.ok(&["runtime", "demo", "state", "active", "--expected-revision", &control, "--expected-head", &lab.head().to_string()]);
        lab.write_binaries();
        lab.prepare_profile();
        lab
    }
    fn path(&self, name: &str) -> PathBuf { self.home.path().join(name) }
    fn cli_in(&self, home: &Path, args: &[&str]) -> Output {
        Command::new(BIN).env_clear().env("HOME", home).env("PATH", "/usr/bin:/bin")
            .args(["--root", self.path("root").to_str().unwrap()]).args(args).output().unwrap()
    }
    fn cli(&self, args: &[&str]) -> Output { self.cli_in(self.home.path(), args) }
    fn ok(&self, args: &[&str]) -> Value {
        let out = self.cli(args);
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
    }
    fn fail(&self, args: &[&str]) -> String {
        let out = self.cli(args);
        assert!(!out.status.success(), "{args:?} succeeded: {}", String::from_utf8_lossy(&out.stdout));
        String::from_utf8_lossy(&out.stderr).into_owned()
    }
    /// `telemetry demo review signer ARGS` as the owner.
    fn signer(&self, args: &[&str]) -> Value {
        let mut all = vec!["telemetry", "demo", "review", "signer"];
        all.extend_from_slice(args);
        self.ok(&all)
    }
    fn signer_fails(&self, args: &[&str]) -> String {
        let mut all = vec!["telemetry", "demo", "review", "signer"];
        all.extend_from_slice(args);
        self.fail(&all)
    }
    fn head(&self) -> u64 { runtime::snapshot(&self.project).unwrap().head }
    fn db(&self) -> rusqlite::Connection { rusqlite::Connection::open(self.project.join(".state/state.db")).unwrap() }
    fn decisions(&self) -> i64 { self.db().query_row("SELECT count(*) FROM review_acceptances", [], |r| r.get(0)).unwrap() }
    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("/usr/bin/git").env_clear().env("PATH", "/usr/bin:/bin").env("HOME", self.home.path())
            .env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "fixture").env("GIT_AUTHOR_EMAIL", "fixture@example.com")
            .env("GIT_COMMITTER_NAME", "fixture").env("GIT_COMMITTER_EMAIL", "fixture@example.com")
            .current_dir(&self.repo).args(args).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }
    /// A Herdr stand-in and a `claude` that answers `--version`: the profile
    /// must name exact executables.
    fn write_binaries(&self) {
        fs::write(self.path("bin/herdr"), "#!/bin/sh\n[ \"$1\" = --version ] && echo 'herdr 0.9.1'\n").unwrap();
        let (agent, source) = (self.path("bin/claude"), self.path("bin/claude.rs"));
        fs::write(&source, "fn main(){if std::env::args().nth(1).as_deref()==Some(\"--version\"){println!(\"2.1.0 (Claude Code)\");return}loop{std::thread::park()}}").unwrap();
        assert!(Command::new("rustc").args(["--edition", "2021", "-o"]).arg(&agent).arg(&source).status().unwrap().success());
        for path in [self.path("bin/herdr"), agent] { fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap(); }
    }
    /// Prepare `worker` over the lab binaries; only the native interaction
    /// evidence, which needs a real agent session, is planted.
    fn prepare_profile(&mut self) {
        use herdr_farm::worker_supervision::{ProcessIncarnation, SupervisorIdentity};
        let prepared = self.ok(&["profile", "prepare", "demo", "worker", "--herdr-executable", self.path("bin/herdr").to_str().unwrap(),
            "--agent-executable", self.path("bin/claude").to_str().unwrap(), "--execution-home", self.path("agent-home").to_str().unwrap()]);
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
        let store = self.project.join(".state/state.db").canonicalize().unwrap();
        let metadata = fs::metadata(&store).unwrap();
        let report = json!({"preparation":{"profile":profile,"reference":reference,"launchable":true,"protocol_capable":false,"certified":false},
            "evidence":evidence,"source_store":[store,metadata.dev(),metadata.ino()]}).to_string();
        self.db().execute("INSERT INTO native_profiles(profile_digest,report,report_digest,sequence) VALUES(?1,?2,?3,(SELECT max(sequence) FROM events))",
            rusqlite::params![reference.digest, report, format!("{:x}", Sha256::digest(report.as_bytes()))]).unwrap();
        self.profile = reference;
    }
    /// Sign `file`'s exact bytes with `key` under `namespace`; returns the signature path.
    fn sign(&self, key: &Path, namespace: &str, file: &Path) -> String {
        let signature = PathBuf::from(format!("{}.sig", file.display()));
        let _ = fs::remove_file(&signature);
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-Y", "sign", "-f"]).arg(key).args(["-n", namespace]).arg(file).output().unwrap().status.success());
        signature.display().to_string()
    }
    fn review_world(&self) -> World {
        self.ok(&["task", "demo", "add", "authored", "--title", "authored", "--expected-head", &self.head().to_string()]);
        let base = self.git(&["rev-parse", "HEAD"]);
        self.git(&["checkout", "-qb", "author"]);
        fs::write(self.repo.join("lib.rs"), "pub fn answer() -> u32 { 42 }\n").unwrap();
        self.git(&["add", "."]);
        self.git(&["commit", "-qm", "candidate"]);
        let candidate = self.git(&["rev-parse", "HEAD"]);
        self.git(&["checkout", "-q", "-"]);
        let repository = self.repo.canonicalize().unwrap().display().to_string();
        let store = self.project.join(".state/state.db").canonicalize().unwrap().display().to_string();
        let mut document = serde_json::to_vec_pretty(&json!({
            "version": 3, "outputs": [{"path": "lib.rs", "kind": "git_file"}], "scope": {"paths": [{"path": "lib.rs", "access": "write"}]},
            "project_store": store, "expected_head": self.head(), "task_id": "authored", "contract_revision": 1, "deliverable": "answer", "non_goals": "none",
            "acceptance_policies": [{"id": "clean", "text": r#"{"version":1,"checks":["/usr/bin/git","diff","--quiet"]}"#}], "repository": repository, "base_oid": base,
            "object_format": "sha256", "dependencies": [], "capability_flags": [], "profile_kind": "codex", "retry_class": "none", "result_schema_id": "result-v1",
            "route": "verify_only", "authority": authority::policy_reference(&self.project).unwrap()})).unwrap();
        document.push(b'\n');
        let contract = self.path("authored-contract.json");
        fs::write(&contract, &document).unwrap();
        let signature = self.sign(&self.key, authority::CONTRACT_SIGNATURE_NAMESPACE, &contract);
        let installed = self.ok(&["task", "demo", "contract", "put", "--input-file", contract.to_str().unwrap(), "--signature", &signature]);
        // The author attempt ran and ended; its dispatch chose a `codex` configuration.
        let author_configuration = format!("sha256:{:x}", Sha256::digest(AUTHOR_CONFIGURATION_JSON.as_bytes()));
        let db = self.db();
        db.execute("INSERT INTO attempts(id,task_id,revision,state,snapshot,reservation,termination_observed) VALUES(?1,'authored',1,'completed',NULL,?1,1)", [AUTHOR_ATTEMPT]).unwrap();
        db.execute("INSERT INTO agent_configurations VALUES(?1,?2,1)", rusqlite::params![author_configuration, AUTHOR_CONFIGURATION_JSON]).unwrap();
        let eligible = json!([{"configuration_id": author_configuration, "probability_ppm": 1_000_000, "profile_digest": "e".repeat(64), "status": "chosen"}]).to_string();
        db.execute("INSERT INTO dispatch_decisions(attempt_id,task_id,task_revision,contract_revision,chosen_configuration_id,eligible,chooser_kind,chooser_principal,reason_codes,decided_unix_ms)
            VALUES(?1,'authored',1,1,?2,?3,'operator','operator:cli','[\"unspecified\"]',1)", rusqlite::params![AUTHOR_ATTEMPT, author_configuration, eligible]).unwrap();
        drop(db);
        let objects: Vec<Value> = self.git(&["rev-list", "--objects", "--all"]).lines()
            .map(|line| { let oid = line.split_whitespace().next().unwrap(); json!({"oid": oid, "relative_path": format!("{}/{}", &oid[..2], &oid[2..])}) }).collect();
        let result = self.path("authored-result.json");
        fs::write(&result, json!({"idempotency_key": "authored-key", "task_id": "authored", "contract_revision": 1, "contract_digest": installed["digest"],
            "attempt_id": AUTHOR_ATTEMPT, "repository": repository, "base_oid": base, "candidate_oid": candidate, "object_format": "sha256",
            "artifact_manifest": [{"path": "lib.rs", "oid": candidate}], "claimed_checks": [], "objects": objects}).to_string()).unwrap();
        let submission = self.ok(&["result", "demo", "submit", "--input-file", result.to_str().unwrap()])["submission_id"].as_str().unwrap().to_owned();
        let opportunity = self.ok(&["telemetry", "demo", "review", "open", &submission, "--protocol", "review-protocol.v1"])["opportunity"]["opportunity_id"].as_str().unwrap().to_owned();
        self.ok(&["telemetry", "demo", "review", "assign", &opportunity, "--blind", "--candidate", "worker"]);
        let scope = self.path("review-scope.json");
        fs::write(&scope, json!({"schema_version":1,"task_id":"work","profile":"worker","domains":[],"paths":[],"pinned_keys":[],"sensitivity":"default"}).to_string()).unwrap();
        let snapshot = self.ok(&["memory", "demo", "snapshot", "--task", "work", "--profile", "worker", "--input-file", scope.to_str().unwrap(), "--worker", "--review-opportunity", &opportunity]);
        let selection = self.path("review-selection.json");
        fs::write(&selection, json!({"task":"work","binding":self.binding,"profile":self.profile,
            "knowledge":{"id":snapshot["id"],"revision":1,"digest":snapshot["manifest_hash"]},"repositories":[self.repo.canonicalize().unwrap()]}).to_string()).unwrap();
        World { opportunity, submission, candidate, repository, store, selection }
    }
    /// Draft, owner-sign, import and reserve the review launch (D9); returns the attempt.
    fn launch_review(&self, world: &World) -> String {
        let drafted = self.ok(&["launch", "demo", "draft", "--selection", world.selection.to_str().unwrap(), "--expected-head", &self.head().to_string()]);
        let document = self.path("review-approval.json");
        fs::write(&document, serde_json::to_vec_pretty(&drafted["approval"]).unwrap()).unwrap();
        let signature = self.sign(&self.key, authority::SIGNATURE_NAMESPACE, &document);
        let approval = self.ok(&["approval", "demo", "import", document.to_str().unwrap(), &signature, "--expected-head", &self.head().to_string()]);
        let reservation = self.ok(&["launch", "demo", "reserve", "--selection", world.selection.to_str().unwrap(), "--approval-digest", approval["digest"].as_str().unwrap(), "--expected-head", &self.head().to_string()]);
        reservation["record"]["attempt"].as_str().unwrap().to_owned()
    }
    /// The launched reviewer's own receipt through `review submit`, run as the
    /// worker (HOME = its execution home); returns (session, receipt digest).
    fn worker_receipt(&self, world: &World, attempt: &str, evidence: Value) -> (String, String) {
        let session = self.ok(&["telemetry", "demo", "review", "show"])["opportunities"].as_array().unwrap().iter()
            .find(|o| o["opportunity_id"] == world.opportunity.as_str()).unwrap()["sessions"][0]["session_id"].as_str().unwrap().to_owned();
        let receipt = self.path(&format!("{attempt}-receipt.json"));
        fs::write(&receipt, json!({"schema": "review_receipt.v1", "session_id": session, "submission_id": world.submission, "candidate_oid": world.candidate,
            "outcome": "completed", "findings": [], "evidence": evidence}).to_string()).unwrap();
        let out = self.cli_in(&self.path("agent-home"), &["telemetry", "demo", "review", "submit", "--input-file", receipt.to_str().unwrap()]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let completion = serde_json::from_slice::<Value>(&out.stdout).unwrap()["completion"].clone();
        assert_eq!(completion["recorder_principal"], json!(format!("worker:{attempt}")));
        (session, completion["receipt_digest"].as_str().unwrap().to_owned())
    }
    /// A review of the same submission of `kind` that the owner opens,
    /// assigns to `worker`, starts for a planted attempt of task `manual` and
    /// completes itself: not launched through D9. Returns (session, receipt digest).
    fn owner_review(&self, world: &World, kind: &str, attempt: &str) -> (String, String) {
        let opportunity = self.ok(&["telemetry", "demo", "review", "open", &world.submission, "--kind", kind, "--protocol", "review-protocol.v1"])["opportunity"]["opportunity_id"].as_str().unwrap().to_owned();
        self.ok(&["telemetry", "demo", "review", "assign", &opportunity, "--reviewer", "worker"]);
        self.db().execute("INSERT INTO attempts(id,task_id,revision,state,reservation,termination_observed) VALUES(?1,'manual',1,'completed',?1,1)", [attempt]).unwrap();
        let session = self.ok(&["telemetry", "demo", "review", "start", &opportunity, "--attempt", attempt])["session"]["session_id"].as_str().unwrap().to_owned();
        let receipt = self.path(&format!("{attempt}-receipt.json"));
        fs::write(&receipt, json!({"schema": "review_receipt.v1", "session_id": session, "submission_id": world.submission, "candidate_oid": world.candidate,
            "outcome": "completed", "findings": [], "evidence": [format!("sha256:{}", "e".repeat(64))]}).to_string()).unwrap();
        let completion = self.ok(&["telemetry", "demo", "review", "complete", "--input-file", receipt.to_str().unwrap()])["completion"].clone();
        (session, completion["receipt_digest"].as_str().unwrap().to_owned())
    }
    /// Owner-sign `file` as a grant and import it; returns the grant id.
    fn import_grant(&self, file: &Path) -> String {
        let signature = self.sign(&self.key, GRANT_NS, file);
        self.ok(&["telemetry", "demo", "review", "authority", "import", file.to_str().unwrap(), &signature])["grant"]["grant_id"].as_str().unwrap().to_owned()
    }
    fn signer_dir(&self, token: &str) -> PathBuf { self.path(".config/herdr-farm/review-signer").join(token) }
}

fn mode(path: &Path) -> u32 { fs::symlink_metadata(path).unwrap().mode() & 0o777 }
fn audit(lab: &Lab, token: &str) -> Vec<Value> {
    fs::read_to_string(lab.signer_dir(token).join("audit.jsonl")).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

/// `signer init` makes `reviewer:carol`'s key (directory 0700, key 0600)
/// under the owner's config directory, writes the default policy (revision
/// 1, digest by hand) and a draft grant naming carol's new public key for
/// task `authored` revision 1, kind `code`, at most 2 decisions, which the
/// owner signs offline and imports. Reviews: S1, launched from the
/// assignment (D9) with the reviewer's own receipt; S2, an owner-started
/// `code` review with an owner-recorded receipt; S3, a `security` review
/// (outside the grant). Ledger: each review's opening, assignment, start and
/// completion: S1 1–4; S2 5–8; S3 9–12.
/// Pass 1 (`--max 1`) accepts only S1 (rule `all_rules_passed`, ledger 13);
/// the owner then raises the policy to revision 2; pass 2 rejects S2
/// (`protocol_violation`, rule `launched_to_assigned_reviewer`, ledger 14)
/// and the grant is exhausted; pass 3 decides nothing. S3 is never a
/// candidate. Each decision is audited with its policy version and exact
/// request digest; the recorded decisions carry carol's principal and grant.
#[test]
fn signer_accepts_in_scope_reviews_under_policy_and_audits() {
    let lab = Lab::new();
    let world = lab.review_world();
    lab.ok(&["task", "demo", "add", "manual", "--title", "manual", "--expected-head", &lab.head().to_string()]);
    let attempt = lab.launch_review(&world);
    let s1 = lab.worker_receipt(&world, &attempt, json!([]));
    let s2 = lab.owner_review(&world, "code", "manual-code");
    let s3 = lab.owner_review(&world, "security", "manual-security");

    let draft = lab.path("carol-grant.json");
    let init = lab.signer(&["init", "--subject", "reviewer:carol", "--repository", &world.repository, "--task", "authored:1",
        "--max-decisions", "2", "--output", draft.to_str().unwrap()]);
    let dir = lab.signer_dir("carol");
    assert_eq!((mode(dir.parent().unwrap()), mode(&dir), mode(&dir.join("id_ed25519")), mode(&dir.join("policy.json"))), (0o700, 0o700, 0o600, 0o600));
    assert_eq!(fs::read_to_string(dir.join("policy.json")).unwrap(), DEFAULT_POLICY);
    let public = fs::read_to_string(dir.join("id_ed25519.pub")).unwrap().split_whitespace().take(2).collect::<Vec<_>>().join(" ");
    let policy_1 = json!({"schema": "review_signer_policy.v1", "revision": 1, "digest": DEFAULT_POLICY_DIGEST});
    assert_eq!((&init["signer"]["public_key"], &init["signer"]["policy"], &init["draft_grant"]["namespace"]), (&json!(public), &policy_1, &json!(GRANT_NS)));
    let grant: Value = serde_json::from_slice(&fs::read(&draft).unwrap()).unwrap();
    assert_eq!((&grant["subject"], &grant["subject_public_key"], &grant["repositories"], &grant["tasks"], &grant["kinds"], &grant["max_decisions"], &grant["project_store"]),
        (&json!("reviewer:carol"), &json!(public), &json!([world.repository]), &json!([{"task_id": "authored", "contract_revision": 1}]), &json!(["code"]), &json!(2), &json!(world.store)));
    assert_eq!(grant["expires_unix_ms"].as_i64().unwrap() - grant["valid_from_unix_ms"].as_i64().unwrap(), 7 * 86_400_000);
    assert!(lab.signer_fails(&["init", "--subject", "reviewer:carol", "--repository", &world.repository, "--task", "authored:1", "--output", lab.path("again.json").to_str().unwrap()])
        .contains("already exists"), "a signer's key is never replaced");
    // Nothing is decided before the owner signs the grant.
    assert_eq!(lab.signer(&["run", "--subject", "reviewer:carol", "--once"])["decided"], json!([]));
    let grant_id = lab.import_grant(&draft);
    assert_eq!(grant_id, format!("sha256:{:x}", Sha256::digest(fs::read(&draft).unwrap())));

    let request = |session: &(String, String), decision: &str, reason: &str| format!(
        r#"{{"decision":"{decision}","grant_id":"{grant_id}","project_store":"{}","reason":{reason},"receipt_digest":"{}","schema":"review_acceptance.v1","session_id":"{}","subject":"reviewer:carol"}}"#,
        world.store, session.1, session.0);
    let digest = |text: String| format!("sha256:{:x}", Sha256::digest(text.as_bytes()));
    let pass1 = lab.signer(&["run", "--subject", "reviewer:carol", "--once", "--max", "1"]);
    assert_eq!(pass1["decided"], json!([{"session_id": s1.0, "grant_id": grant_id, "decision": "accepted", "reason": null, "rule": "all_rules_passed",
        "request_digest": digest(request(&s1, "accepted", "null")), "ledger_seq": 13, "replayed": false}]));
    assert_eq!((&pass1["grants"][0]["status"], &pass1["grants"][0]["remaining"]), (&json!("active"), &json!(1)));

    fs::write(dir.join("policy.json"), POLICY_2).unwrap();
    let pass2 = lab.signer(&["run", "--subject", "reviewer:carol", "--once", "--max", "5"]);
    assert_eq!(pass2["decided"], json!([{"session_id": s2.0, "grant_id": grant_id, "decision": "rejected", "reason": "protocol_violation", "rule": "launched_to_assigned_reviewer",
        "request_digest": digest(request(&s2, "rejected", "\"protocol_violation\"")), "ledger_seq": 14, "replayed": false}]));
    assert_eq!((&pass2["grants"][0]["status"], &pass2["grants"][0]["remaining"]), (&json!("exhausted"), &json!(0)));
    let pass3 = lab.signer(&["run", "--subject", "reviewer:carol", "--once"]);
    assert_eq!((&pass3["decided"], &pass3["refused"]), (&json!([]), &json!([])), "idempotent: nothing left to decide");

    // The decisions went through the D8 accept path: carol's principal, the grant, the ledger.
    let shown = lab.ok(&["telemetry", "demo", "review", "authority", "show"]);
    let decided: Vec<_> = shown["decisions"].as_array().unwrap().iter()
        .map(|d| (d["session_id"].clone(), d["decision"].clone(), d["authority_principal"].clone(), d["grant_id"].clone(), d["authority"].clone())).collect();
    assert_eq!(decided, [(json!(s1.0), json!("accepted"), json!("reviewer:carol"), json!(grant_id), json!("delegated_code_review.v1")),
        (json!(s2.0), json!("rejected"), json!("reviewer:carol"), json!(grant_id), json!("delegated_code_review.v1"))]);
    assert_eq!(lab.db().query_row("SELECT count(*) FROM review_acceptances WHERE session_id=?1", [&s3.0], |r| r.get::<_, i64>(0)).unwrap(), 0, "S3 is outside the grant");
    let seqs: Vec<(i64, String)> = lab.db().prepare("SELECT seq,session_id FROM review_decision_log ORDER BY seq").unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect();
    assert_eq!(seqs, [(13, s1.0.clone()), (14, s2.0.clone())]);

    // The append-only audit: init, then one line per decision with the policy version it was made under.
    let lines = audit(&lab, "carol");
    assert_eq!(mode(&dir.join("audit.jsonl")), 0o600);
    assert_eq!(lines.iter().map(|l| l["event"].clone()).collect::<Vec<_>>(), [json!("init"), json!("decision"), json!("decision")]);
    assert_eq!((&lines[0]["public_key"], &lines[0]["policy"], &lines[0]["draft_grant_digest"]), (&json!(public), &policy_1, &json!(grant_id)));
    let policy_2 = json!({"schema": "review_signer_policy.v1", "revision": 2, "digest": POLICY_2_DIGEST});
    for (line, session, decision, policy, seq) in [(&lines[1], &s1, "accepted", &policy_1, 13), (&lines[2], &s2, "rejected", &policy_2, 14)] {
        assert_eq!((&line["schema"], &line["subject"], &line["session_id"], &line["decision"], &line["policy"], &line["result"], &line["ledger_seq"]),
            (&json!("review_signer_audit.v1"), &json!("reviewer:carol"), &json!(session.0), &json!(decision), policy, &json!("recorded"), &json!(seq)));
    }
    assert_eq!((&lines[1]["checks"]["launched"], &lines[1]["checks"]["receipt_recorder"]), (&json!(true), &json!(format!("worker:{attempt}"))));
    assert_eq!((&lines[2]["checks"]["launched"], &lines[2]["checks"]["session_recorder"]), (&json!(false), &json!("operator:cli")));

    let status = lab.signer(&["status", "--subject", "reviewer:carol"]);
    assert_eq!((&status["signer"]["key_check"], &status["signer"]["public_key"], &status["signer"]["policy"]), (&json!("ok"), &json!(public), &policy_2));
    assert_eq!((&status["grants"][0]["grant_id"], &status["grants"][0]["status"], &status["grants"][0]["decisions"], &status["grants"][0]["remaining"]),
        (&json!(grant_id), &json!("exhausted"), &json!(2), &json!(0)));
    assert_eq!(status["last_decisions"].as_array().unwrap().iter().map(|d| (d["session_id"].clone(), d["rule"].clone())).collect::<Vec<_>>(),
        [(json!(s1.0), json!("all_rules_passed")), (json!(s2.0), json!("launched_to_assigned_reviewer"))]);
}

/// With a launched, completed review and an installed grant (1 decision),
/// the signer refuses before signing, deciding nothing, when: the key is
/// 0644; the key directory is 0755; the key is a symlink; the policy is
/// group-readable; it runs in a worker execution context (HOME = the
/// `worker` profile's execution home), for `run`, `status` and `init`. A
/// grant for carol with another key is `other_key`, and a grant row nobody
/// signed (raw SQL) is `unverified`; neither is ever used. After the
/// owner revokes carol's grant a pass decides nothing and status shows it
/// revoked. Finally a retained execution home inside the signer directory
/// makes the key's location refused.
#[test]
fn signer_refuses_bad_key_permissions_worker_context_and_revoked_grants() {
    let lab = Lab::new();
    let world = lab.review_world();
    let attempt = lab.launch_review(&world);
    let s1 = lab.worker_receipt(&world, &attempt, json!([]));
    let draft = lab.path("carol-grant.json");
    lab.signer(&["init", "--subject", "reviewer:carol", "--repository", &world.repository, "--task", "authored:1", "--max-decisions", "1", "--output", draft.to_str().unwrap()]);
    let grant_id = lab.import_grant(&draft);
    let dir = lab.signer_dir("carol");
    let key = dir.join("id_ed25519");
    let run = || lab.signer_fails(&["run", "--subject", "reviewer:carol", "--once"]);

    fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(run().contains("must have mode 600 (has 644)"));
    assert!(lab.signer(&["status", "--subject", "reviewer:carol"])["signer"]["key_check"].as_str().unwrap().contains("must have mode 600"));
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(run().contains("must have mode 700 (has 755)"));
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    fs::rename(&key, dir.join("real-key")).unwrap();
    std::os::unix::fs::symlink(dir.join("real-key"), &key).unwrap();
    assert!(run().contains("must be a regular file, not a symlink"));
    fs::remove_file(&key).unwrap();
    fs::rename(dir.join("real-key"), &key).unwrap();
    fs::set_permissions(dir.join("policy.json"), fs::Permissions::from_mode(0o640)).unwrap();
    assert!(run().contains("signer policy"));
    fs::set_permissions(dir.join("policy.json"), fs::Permissions::from_mode(0o600)).unwrap();
    // A worker execution context: HOME is the retained profile's execution home.
    let agent_home = lab.path("agent-home");
    for args in [vec!["run", "--subject", "reviewer:carol", "--once"], vec!["status", "--subject", "reviewer:carol"],
        vec!["init", "--subject", "reviewer:mallory", "--repository", &world.repository, "--task", "authored:1", "--output", "/tmp/never.json"]] {
        let mut all = vec!["telemetry", "demo", "review", "signer"];
        all.extend(args);
        let out = lab.cli_in(&agent_home, &all);
        assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("worker execution context"), "{}", String::from_utf8_lossy(&out.stderr));
    }
    assert!(!agent_home.join(".config/herdr-farm/review-signer").exists());
    assert_eq!(lab.decisions(), 0, "every refusal decided nothing");

    // A grant naming carol with someone else's key is never used by this signer.
    let other = lab.path("other");
    assert!(Command::new("/usr/bin/ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&other).output().unwrap().status.success());
    let mut foreign: Value = serde_json::from_slice(&fs::read(&draft).unwrap()).unwrap();
    foreign["subject_public_key"] = json!(fs::read_to_string(other.with_extension("pub")).unwrap().split_whitespace().take(2).collect::<Vec<_>>().join(" "));
    let foreign_file = lab.path("foreign-grant.json");
    fs::write(&foreign_file, serde_json::to_vec_pretty(&foreign).unwrap()).unwrap();
    let foreign_id = lab.import_grant(&foreign_file);

    // A grant row nobody signed (raw SQL, as a process with write access to
    // the store could add): the signer verifies the owner signature first and
    // never signs under it.
    let mut forged: Value = serde_json::from_slice(&fs::read(&draft).unwrap()).unwrap();
    forged["max_decisions"] = json!(5);
    let forged_bytes = serde_json::to_vec(&forged).unwrap();
    let forged_id = format!("sha256:{:x}", Sha256::digest(&forged_bytes));
    lab.db().execute("INSERT INTO review_authority_grants(grant_id,raw_bytes,signature,scope,issuer,subject,subject_public_key,project_store,actions,repositories,tasks,kinds,review_configurations,subject_configurations,max_decisions,valid_from_unix_ms,expires_unix_ms,authority_revision,authority_digest,installed_unix_ms)
        VALUES(?1,?2,x'6e6f74207369676e6564','code_review','owner','reviewer:carol',?3,?4,'[\"accept_review_completion\"]',?5,'[{\"task_id\":\"authored\",\"contract_revision\":1}]','[\"code\"]','[]','[]',5,?6,?7,1,?8,?9)",
        rusqlite::params![forged_id, forged_bytes, forged["subject_public_key"].as_str().unwrap(), world.store, json!([world.repository]).to_string(),
            forged["valid_from_unix_ms"].as_i64().unwrap(), forged["expires_unix_ms"].as_i64().unwrap(), forged["authority"]["digest"].as_str().unwrap(),
            jiff::Timestamp::now().as_millisecond()]).unwrap();

    // Revocation: the owner revokes carol's grant before any pass.
    let revocation = lab.path("revoke.json");
    fs::write(&revocation, serde_json::to_vec_pretty(&json!({"schema": "code_review_revocation.v1", "grant_id": grant_id, "project_store": world.store,
        "reason": "compromised", "authority": authority::policy_reference(&lab.project).unwrap()})).unwrap()).unwrap();
    let signature = lab.sign(&lab.key, REVOKE_NS, &revocation);
    lab.ok(&["telemetry", "demo", "review", "authority", "revoke", revocation.to_str().unwrap(), &signature]);
    let pass = lab.signer(&["run", "--subject", "reviewer:carol", "--once"]);
    assert_eq!((&pass["decided"], &pass["refused"]), (&json!([]), &json!([])));
    let statuses: Vec<_> = pass["grants"].as_array().unwrap().iter().map(|g| (g["grant_id"].clone(), g["status"].clone())).collect();
    assert_eq!(statuses, [(json!(grant_id), json!("revoked")), (json!(foreign_id), json!("other_key")), (json!(forged_id), json!("unverified"))]);
    assert_eq!(lab.decisions(), 0);
    assert_eq!(lab.ok(&["telemetry", "demo", "review", "show"])["opportunities"][0]["sessions"][0]["session_id"], json!(s1.0));
    assert_eq!(audit(&lab, "carol").len(), 1, "only init: nothing was signed");

    // A retained execution home inside the signer directory: the location is refused.
    let mut profile: FrozenProfile = serde_json::from_value(lab.ok(&["profile", "retained", "demo", &lab.profile.digest])["preparation"]["profile"].clone()).unwrap();
    profile.name = "inside".into();
    profile.execution_home = Some(dir.join("home").display().to_string());
    support::telemetry::plant_profile(&lab.project.join(".state/state.db"), profile);
    assert!(run().contains("overlaps execution home"));
}

/// The owner decision's premise: a process started through the canonical
/// worker sandbox cannot read the signer key. The argv is the one the
/// launch service derives (`Isolation::for_agent` over the retained
/// profile's execution home, the route's working directory and socket, the
/// approved repository and the pinned owner configuration, then
/// `isolated_gated_command`), run from the route's directory and released
/// through the gate. The probe reads the key, the policy and the audit log,
/// lists the signer directory, and runs `review signer status` and `run`.
/// The same probe as the owner, outside the sandbox, reads the key.
#[test]
fn isolated_worker_cannot_read_the_signer_key() {
    use herdr_farm::worker_supervision::{Isolation, isolated_gated_command};
    use std::io::Write;
    let lab = Lab::new();
    let world = lab.review_world();
    lab.signer(&["init", "--subject", "reviewer:carol", "--repository", &world.repository, "--task", "authored:1", "--output", lab.path("g.json").to_str().unwrap()]);
    let dir = lab.signer_dir("carol").canonicalize().unwrap();
    let key = dir.join("id_ed25519");
    let secret = fs::read_to_string(&key).unwrap();
    let body = secret.lines().nth(1).unwrap().to_owned();
    let root = lab.path("root").canonicalize().unwrap();
    // The product binary the worker runs, named through a link in its own
    // directory under /tmp (the sandbox's private scratch directory) beside
    // an owner secret: the binary is exposed as the file itself, read-only,
    // and the sibling stays hidden.
    let bin_tmp = tempfile::Builder::new().prefix("herdr-farm-bin-").tempdir_in("/tmp").unwrap();
    let bin_dir = bin_tmp.path().canonicalize().unwrap();
    let bin = bin_dir.join("herdr-farm");
    std::os::unix::fs::symlink(Path::new(BIN).canonicalize().unwrap(), &bin).unwrap();
    let sibling = bin_dir.join("owner-secret");
    fs::write(&sibling, "SENTINEL-BIN-SIBLING\n").unwrap();
    let probe = format!("for p in {key:?} {policy:?} {audit:?} {sibling:?}; do if [ -s \"$p\" ] && cat \"$p\" >/dev/null 2>&1; then echo \"READ $p\"; cat \"$p\"; else echo \"DENIED $p\"; fi; done; \
        echo LIST $(ls -A {dir:?} 2>&1); \
        echo BINLIST $(ls -A {bin_dir:?} 2>&1); [ -w {bin:?} ] && echo BIN-WRITABLE; \
        {bin:?} --root {root:?} telemetry demo review signer status --subject reviewer:carol; echo status-exit $?; \
        {bin:?} --root {root:?} telemetry demo review signer run --subject reviewer:carol --once; echo run-exit $?",
        policy = dir.join("policy.json"), audit = dir.join("audit.jsonl"));
    // Control: the owner's own process reads it.
    let owner = Command::new("/bin/sh").args(["-c", &probe]).env_clear().env("HOME", lab.home.path()).env("PATH", "/usr/bin:/bin").output().unwrap();
    let owner = String::from_utf8_lossy(&owner.stdout).into_owned();
    assert!(owner.contains(&format!("READ {}", key.display())) && owner.contains(&body), "{owner}");
    assert!(owner.contains(&format!("READ {}", sibling.display())) && owner.contains("BINLIST herdr-farm owner-secret"), "{owner}");
    assert!(owner.contains("BIN-WRITABLE"), "the owner's control writes its binary:\n{owner}");

    let home = lab.path("agent-home").canonicalize().unwrap();
    let cwd = lab.repo.canonicalize().unwrap();
    let config = lab.path(".config/herdr-farm/config.toml");
    // This test's own binary is not the product binary: name it, as the
    // controller's `current_exe` is named in production.
    let isolation = Isolation::for_agent(&lab.project, &home, &cwd, Path::new("/bin/sh"), &[cwd.as_path()], &[], Some(&config), Some(&lab.path("lab/native.sock")), &[])
        .unwrap().with_executable(&bin).unwrap();
    let argv = isolated_gated_command(Path::new("/bin/sh"), &["-c".into(), probe.clone()], 60, "release-signer-probe", &home, &isolation).unwrap();
    let mut child = Command::new(&argv[0]).args(&argv[1..]).current_dir(&cwd).env_clear().env("HOME", lab.home.path()).env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(b"release-signer-probe\n").unwrap();
    let out = child.wait_with_output().unwrap();
    let report = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(!report.contains(&body) && !report.contains("PRIVATE KEY"), "the signer key reached the worker:\n{report}");
    assert!(!report.contains("SENTINEL-BIN-SIBLING") && !report.contains("BIN-WRITABLE"), "{report}");
    assert!(report.contains("\nBINLIST herdr-farm\n"), "only the binary is visible in its directory:\n{report}");
    for path in [&key, &dir.join("policy.json"), &dir.join("audit.jsonl"), &sibling] {
        assert!(report.contains(&format!("DENIED {}", path.display())), "{} was readable:\n{report}", path.display());
    }
    assert!(report.contains("LIST ls: cannot access"), "{report}");
    assert!(report.contains("worker execution context") && report.contains("status-exit 1") && report.contains("run-exit 1"), "{report}");
    assert_eq!(fs::read_to_string(&key).unwrap(), secret, "the key is untouched");
}

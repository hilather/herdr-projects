#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! Capability evidence gates queued tasks, observed through `scheduler inspect`.
//! Contracts are signed and queued through the compiled CLI. The native verifier
//! needs a live agent, so its retained report is written as
//! `retain_native_profile` stores it. Evidence is recorded through the public
//! `record_native_capability_evidence`, which has no CLI command.
use herdr_projects::{authority, domain::*, migration, runtime};
use serde_json::{json, Value};
use std::{fs, os::unix::fs::MetadataExt, path::PathBuf, process::Command};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-projects");
const HOUR: i64 = 3_600_000;

struct Project { home: tempfile::TempDir, project: PathBuf, key: PathBuf, repo: PathBuf, db: PathBuf, oid: String }

impl Project {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let key = home.path().join("owner");
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&key).status().unwrap().success());
        let public = fs::read_to_string(key.with_extension("pub")).unwrap().split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        let config = home.path().join("owner.toml");
        fs::write(&config, format!("[authority]\nversion=1\nrevision=1\napproval_public_key={public:?}\n")).unwrap();
        let repo = home.path().join("repo");
        fs::create_dir(&repo).unwrap();
        let mut p = Project { project: home.path().join("root/demo"), db: PathBuf::new(), key, repo, oid: String::new(), home };
        for command in ["new", "pause"] { p.ok(&[command, "demo"]); }
        migration::apply(&p.project, &migration::inspect_with_config(&p.project, &config).unwrap(), true).unwrap();
        let s = runtime::snapshot(&p.project).unwrap();
        runtime::set_state(&p.project, s.head, s.control.unwrap().revision, ProjectState::Active, &config).unwrap();
        p.db = p.project.join(".state/state.db").canonicalize().unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("/usr/bin/git").env_clear().env("PATH", "/usr/bin:/bin").env("HOME", p.home.path())
                .env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_AUTHOR_NAME", "fixture").env("GIT_AUTHOR_EMAIL", "fixture@example.com")
                .env("GIT_COMMITTER_NAME", "fixture").env("GIT_COMMITTER_EMAIL", "fixture@example.com")
                .current_dir(&p.repo).args(args).output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        };
        git(&["init", "-q", "--object-format=sha1"]);
        git(&["commit", "-q", "--allow-empty", "-m", "base"]);
        p.oid = git(&["rev-parse", "HEAD"]);
        p
    }
    fn ok(&self, args: &[&str]) -> Value {
        let out = Command::new(BIN).env_clear().env("HOME", self.home.path()).env("PATH", "/usr/bin:/bin")
            .args(["--root", self.home.path().join("root").to_str().unwrap()]).args(args).output().unwrap();
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
    }
    fn head(&self) -> u64 { runtime::snapshot(&self.project).unwrap().head }
    fn raw(&self) -> rusqlite::Connection { rusqlite::Connection::open(&self.db).unwrap() }
    /// Add a task, install a signed contract asking for `flags`, and queue it.
    fn ask(&self, task: &str, flags: &[&str]) {
        self.ok(&["task", "demo", "add", task, "--title", task, "--expected-head", &self.head().to_string()]);
        let body = json!({"version":1,"project_store":self.db,"expected_head":self.head(),"task_id":task,"contract_revision":1,
            "deliverable":"show capability","non_goals":"no certification","acceptance_policies":[{"id":"shown","text":"capability evidence"}],
            "repository":self.repo.canonicalize().unwrap(),"base_oid":self.oid,"object_format":"sha1","dependencies":[],
            "capability_flags":flags,"profile_kind":"codex","retry_class":"none","result_schema_id":"result-v1","route":"verify_only",
            "authority":authority::policy_reference(&self.project).unwrap()});
        let doc = self.home.path().join(format!("{task}-contract.json"));
        fs::write(&doc, serde_json::to_vec(&body).unwrap()).unwrap();
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-Y", "sign", "-f"]).arg(&self.key)
            .args(["-n", authority::CONTRACT_SIGNATURE_NAMESPACE]).arg(&doc).output().unwrap().status.success());
        let sig = format!("{}.sig", doc.display());
        self.ok(&["task", "demo", "contract", "put", "--input-file", doc.to_str().unwrap(), "--signature", &sig]);
        let request = self.home.path().join(format!("{task}-queue.json"));
        fs::write(&request, r#"{"priority":0,"dependencies":[]}"#).unwrap();
        self.ok(&["task", "demo", "queue", task, "--input-file", request.to_str().unwrap(), "--expected-revision", "1", "--expected-head", &self.head().to_string()]);
    }
    fn inspect(&self) -> Value { self.ok(&["scheduler", "demo", "inspect"]) }
    fn unsupported(&self, task: &str) -> bool {
        self.inspect()["entries"].as_array().unwrap().iter().find(|e| e["task"] == task).unwrap()["blockers"]
            .as_array().unwrap().iter().any(|b| b == "capability_unsupported")
    }
    /// The report the native verifier retains for `profile` in this store.
    fn retain(&self, profile: &FrozenProfile) -> String {
        let reference = profile.reference().unwrap();
        let meta = fs::metadata(&self.db).unwrap();
        let report = json!({"preparation":{"profile":profile,"reference":reference,"launchable":profile.validate_for_launch().is_ok(),
            "protocol_capable":false,"certified":false},"source_store":[self.db,meta.dev(),meta.ino()]}).to_string();
        let digest = format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(report.as_bytes()));
        let raw = self.raw();
        raw.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('profile.native_retained',?1,1,1,?2)",
            rusqlite::params![reference.id, serde_json::to_string(&reference).unwrap()]).unwrap();
        raw.execute("INSERT INTO native_profiles VALUES(?1,?2,?3,(SELECT max(sequence) FROM events))", rusqlite::params![reference.digest, report, digest]).unwrap();
        // The retained report reads back through the CLI as the verifier's own.
        assert_eq!(self.ok(&["profile", "retained", "demo", &reference.digest])["preparation"]["reference"]["digest"], reference.digest);
        reference.digest
    }
    fn record(&self, observed: i64, expires: i64) {
        migration::open_active(&self.project).unwrap().record_native_capability_evidence(observed, expires).unwrap();
    }
    /// Stored `(level, observed_unix_ms)` rows for one profile digest.
    fn levels(&self, digest: &str) -> Vec<(String, i64)> {
        let raw = self.raw();
        let mut stmt = raw.prepare("SELECT level, observed_unix_ms FROM capability_evidence WHERE profile_digest=?1 ORDER BY observed_unix_ms, level").unwrap();
        stmt.query_map([digest], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect()
    }
}

fn now() -> i64 { jiff::Timestamp::now().as_millisecond() }

/// A codex profile whose launch, readiness, prompt and stop are supported.
fn codex(name: &str, arguments: char) -> FrozenProfile {
    let evidence = VersionedReference { id: "native-fixture".into(), revision: 1, digest: "a".repeat(64) };
    let supported = CapabilityEvidence::Supported { evidence: evidence.clone() };
    FrozenProfile {
        version: 1, name: name.into(), kind: "codex".into(), definition_digest: "b".repeat(64),
        config: migration::ConfigReference { path: "/fixture/codex.toml".into(), digest: None }, arguments_digest: arguments.to_string().repeat(64),
        environment_names: vec![], execution_home: None, permission_policy: evidence.clone(), adapter: evidence,
        agent: ExecutableIdentity { path: "/fixture/codex".into(), digest: "d".repeat(64), version: "0.154.0".into() },
        herdr: ExecutableIdentity { path: "/fixture/herdr".into(), digest: "e".repeat(64), version: "0.9.1".into() },
        capabilities: ProfileCapabilities { launch: supported.clone(), readiness_observation: supported.clone(), prompt_submission: supported.clone(), stop: supported,
            checkpoint_acknowledgment: CapabilityEvidence::Unknown, structured_usage: CapabilityEvidence::Unknown, resume: CapabilityEvidence::Unknown },
        workflow_certificate: None,
    }
}

/// A launchable profile shows `discovered` and `launchable`, never a
/// certification: contracts asking for certification or an unknown level stay
/// `capability_unsupported`, and the append-only rows cannot be promoted.
#[test]
fn native_evidence_shows_launch_levels_but_never_certifies() {
    let p = Project::new();
    for (task, flag) in [("ask-certified", "workflow-certified"), ("ask-launch", "launchable"), ("ask-model", "model")] { p.ask(task, &[flag]); }
    p.ask("ask-nothing", &[]);
    assert!(p.unsupported("ask-launch"), "no retained profile shows no level");
    assert!(!p.unsupported("ask-nothing"));
    let digest = p.retain(&codex("codex", 'c'));
    let t = now();
    p.record(t - 1_000, t + HOUR);
    p.record(t - 1_000, t + HOUR);
    assert_eq!(p.levels(&digest), [("discovered".to_string(), t - 1_000), ("launchable".to_string(), t - 1_000)], "a replay adds no rows");
    let raw = p.raw();
    let (live, os): (i64, String) = raw.query_row("SELECT max(live), min(os_name) FROM capability_evidence", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!((live, os.as_str()), (0, std::env::consts::OS));
    assert!(raw.execute("UPDATE capability_evidence SET level='workflow-certified'", []).is_err());
    assert!(raw.execute("UPDATE capability_evidence SET live=1", []).is_err());
    assert!(raw.execute("DELETE FROM capability_evidence", []).is_err());
    let report = p.inspect();
    let blockers = |task: &str| report["entries"].as_array().unwrap().iter().find(|e| e["task"] == task).unwrap()["blockers"].as_array().unwrap().clone();
    assert!(blockers("ask-certified").iter().any(|b| b == "capability_unsupported"));
    assert!(blockers("ask-model").iter().any(|b| b == "capability_unsupported"));
    assert!(blockers("ask-launch").iter().all(|b| b != "capability_unsupported"));
    let all: Vec<&Value> = report["entries"].as_array().unwrap().iter().flat_map(|e| e["blockers"].as_array().unwrap()).chain(report["capability"]["blockers"].as_array().unwrap()).collect();
    assert!(all.iter().all(|b| !b.as_str().unwrap().contains("certified")), "{all:?}");
    assert_eq!(report["launch_enabled"], false);
}

/// A later retained profile with other arguments and no stop support is a new
/// digest: it shows only `discovered`, and the old digest's launchable row does
/// not carry over to it.
#[test]
fn a_changed_profile_does_not_inherit_the_old_level() {
    let p = Project::new();
    p.ask("ask-launch", &["launchable"]);
    let original = p.retain(&codex("codex", 'c'));
    let t = now();
    p.record(t - 2_000, t + HOUR);
    assert!(!p.unsupported("ask-launch"));
    let mut changed = codex("codex", '9');
    changed.capabilities.stop = CapabilityEvidence::Unknown;
    let changed = p.retain(&changed);
    assert_ne!(original, changed);
    assert!(p.unsupported("ask-launch"), "the selected profile is now the changed one");
    p.record(t - 1_000, t + HOUR);
    assert_eq!(p.levels(&changed), [("discovered".to_string(), t - 1_000)]);
    assert!(p.levels(&original).contains(&("launchable".to_string(), t - 2_000)));
    assert!(p.unsupported("ask-launch"));
}

/// Evidence counts only between its observation and expiry. Recording a new
/// window for digests that already hold other windows adds rows instead of
/// aborting, and a later window does not hide the current one.
#[test]
fn evidence_counts_only_inside_its_observation_window() {
    let p = Project::new();
    p.ask("ask-launch", &["launchable"]);
    let first = p.retain(&codex("codex", 'c'));
    let second = p.retain(&codex("other", '9'));
    let t = now();
    p.record(t + HOUR, t + 2 * HOUR);
    assert!(p.unsupported("ask-launch"), "evidence observed in the future");
    p.record(t - 2 * HOUR, t - HOUR);
    assert!(p.unsupported("ask-launch"), "expired evidence");
    p.record(t - 1_000, t + HOUR);
    for digest in [&first, &second] {
        let launchable: Vec<i64> = p.levels(digest).into_iter().filter(|(l, _)| l == "launchable").map(|(_, at)| at).collect();
        assert_eq!(launchable, [t - 2 * HOUR, t - 1_000, t + HOUR]);
    }
    assert!(!p.unsupported("ask-launch"));
    p.record(t + 2 * HOUR, t + 3 * HOUR);
    assert!(!p.unsupported("ask-launch"), "a later window must not hide the current one");
}

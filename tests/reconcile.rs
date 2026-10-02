#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! Live reconciliation and recovery advice over a migrated project, through
//! the compiled CLI: `migration plan/apply`, `task add/list`, `runtime
//! create/inspect` and `reconcile --record` / `reconcile --plan`. herdr and
//! the `git worktree list` answer are fakes that log every call; every other
//! git call reaches the real git. Attempts, queued operations and their
//! delivery claims have no CLI verb, so they are written with the public
//! store API, as in tests/scheduling.rs.
use herdr_farm::{domain::*, migration};
use serde_json::{json, Value};
use std::{fs, os::unix::{fs::PermissionsExt, net::UnixListener}, path::{Path, PathBuf}, process::{Command, Output}};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-farm");

/// Every session named `S.sock` lists `S.panes` / `S.agents`; a session
/// without them is unreachable. Calls are logged to `herdr-calls`.
const FAKE_HERDR: &str = r#"#!/usr/bin/python3
import json,os,pathlib,sys
home=pathlib.Path(os.environ['HOME']);args=sys.argv[1:]
name=pathlib.Path(os.environ.get('HERDR_SOCKET_PATH','none')).stem
with open(home/'herdr-calls','a') as f:f.write(name+' '+' '.join(args)+'\n')
if args==['--version']:print('herdr 0.9.1');sys.exit(0)
p=home/f'{name}.{args[0]}s'
if args not in (['agent','list'],['pane','list']) or not p.exists():print('connection refused',file=sys.stderr);sys.exit(1)
print(json.dumps({'result':{args[0]+'s':json.loads(p.read_text())}}))
"#;

/// `git worktree list --porcelain -z` answers from `$HOME/worktrees` when it
/// exists; everything else runs the real git. Calls are logged to `git-calls`.
const FAKE_GIT: &str = "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$HOME/git-calls\"\ncase \"$*\" in\n*'worktree list --porcelain -z') [ -e \"$HOME/worktrees\" ] && exec cat \"$HOME/worktrees\";;\nesac\nexec /usr/bin/git \"$@\"\n";

struct Lab { home: tempfile::TempDir, _listener: UnixListener }

impl Lab {
    /// A paused project `demo`, not yet migrated.
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let listener = UnixListener::bind(home.path().join("live.sock")).unwrap();
        let lab = Lab { home, _listener: listener };
        fs::create_dir(lab.path("bin")).unwrap();
        for (name, body) in [("herdr", FAKE_HERDR), ("bin/git", FAKE_GIT)] {
            fs::write(lab.path(name), body).unwrap();
            fs::set_permissions(lab.path(name), fs::Permissions::from_mode(0o700)).unwrap();
        }
        lab.ok(&["new", "demo"]);
        lab.ok(&["pause", "demo"]);
        lab
    }
    fn migrate(&self) {
        let plan = self.path("plan.json");
        self.ok(&["migration", "demo", "plan", "--output", plan.to_str().unwrap()]);
        self.ok(&["migration", "demo", "apply", "--plan", plan.to_str().unwrap(), "--writers-stopped"]);
    }
    fn path(&self, name: &str) -> PathBuf { self.home.path().join(name) }
    fn project(&self) -> PathBuf { self.path("root/demo") }
    fn cli(&self, args: &[&str]) -> Output {
        Command::new(BIN).env_clear().env("HOME", self.home.path()).env("PATH", format!("{}:/usr/bin:/bin", self.path("bin").display()))
            .env("HERDR_BIN_PATH", self.path("herdr")).arg("--root").arg(self.path("root")).args(args).output().unwrap()
    }
    fn ok(&self, args: &[&str]) -> Value {
        let out = self.cli(args);
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
    }
    /// The canonical snapshot as `task list` prints it.
    fn snapshot(&self) -> Value { self.ok(&["task", "demo", "list"]) }
    fn head(&self) -> String { self.snapshot()["head"].to_string() }
    fn add_task(&self, id: &str) { self.ok(&["task", "demo", "add", id, "--title", id, "--expected-head", &self.head()]); }
    fn task_revision(&self, id: &str) -> u64 {
        self.snapshot()["tasks"].as_array().unwrap().iter().find(|t| t["id"] == id).unwrap()["revision"].as_u64().unwrap()
    }
    /// A runtime binding for `task` at `route`; returns its id.
    fn bind(&self, task: &str, route: Value) -> String {
        let file = self.path("route.json");
        fs::write(&file, route.to_string()).unwrap();
        let (head, revision) = (self.head(), self.task_revision(task).to_string());
        let change = self.ok(&["runtime", "demo", "create", "--route", file.to_str().unwrap(), "--expected-head", &head, "--task", task, "--task-revision", &revision]);
        change["binding"]["id"].as_str().unwrap().to_owned()
    }
    fn session(&self, name: &str, panes: Value, agents: Value) {
        fs::write(self.path(&format!("{name}.panes")), panes.to_string()).unwrap();
        fs::write(self.path(&format!("{name}.agents")), agents.to_string()).unwrap();
    }
    fn calls(&self, log: &str) -> Vec<String> { fs::read_to_string(self.path(log)).unwrap_or_default().lines().map(str::to_owned).collect() }
    /// `reconcile --record`; checks that it records exactly what it prints
    /// and never allows dispatch. Returns the observations by binding id.
    fn record(&self) -> std::collections::BTreeMap<String, Value> {
        let batch = self.ok(&["reconcile", "demo", "--record"]);
        assert_eq!((batch["dispatch_allowed"].as_bool(), batch["recorded_head"].is_u64()), (Some(false), true), "{batch}");
        assert_eq!(self.ok(&["runtime", "demo", "inspect"])["observations"], batch["observations"]);
        batch["observations"].as_array().unwrap().iter().map(|o| (o["binding"].as_str().unwrap().to_owned(), o.clone())).collect()
    }
    fn git(&self, dir: &Path, args: &[&str]) {
        let out = Command::new("/usr/bin/git").env_clear().env("PATH", "/usr/bin:/bin").env("HOME", self.home.path())
            .env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "fixture").env("GIT_AUTHOR_EMAIL", "fixture@example.com")
            .env("GIT_COMMITTER_NAME", "fixture").env("GIT_COMMITTER_EMAIL", "fixture@example.com")
            .arg("-C").arg(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }
}

fn now() -> i64 { jiff::Timestamp::now().as_millisecond() }
fn pane(id: &str, cwd: &str) -> Value { json!({"workspace_id": "w", "tab_id": "t", "pane_id": id, "cwd": cwd}) }

/// Replaces `pane_evidence_distinguishes_absence_unknown_mismatch_and_inconsistent_agents`.
///
/// Each recorded pane is judged against what its session lists: missing is
/// absent; listed only as an agent, listed twice, or beside an agent in
/// another workspace is unknown; in another directory is a mismatch; and an
/// unreachable session is unknown. A matching pane is present, with an agent
/// only when a matching agent is listed in it. Only list calls reach herdr.
#[test]
fn reconcile_records_pane_evidence_from_what_each_session_lists() {
    let lab = Lab::new();
    lab.migrate();
    let cases = [("absent", "absent", false), ("agent-only", "unknown", false), ("twice", "unknown", false), ("with-agent", "present", true),
                 ("agent-elsewhere", "unknown", false), ("moved", "mismatch", false), ("bare-shell", "present", false), ("unreachable", "unknown", false)];
    let mut bindings = Vec::new();
    for (n, (case, _, _)) in cases.iter().enumerate() {
        lab.add_task(case);
        let socket = lab.path(if *case == "unreachable" { "down.sock" } else { "live.sock" });
        bindings.push(lab.bind(case, json!({"socket": socket, "workspace_id": "w", "tab_id": "t", "pane_id": format!("p{n}"), "cwd": "/cwd"})));
    }
    let agent = |id: &str, workspace: &str| json!({"workspace_id": workspace, "tab_id": "t", "pane_id": id, "cwd": "/cwd", "agent": "claude", "name": "worker", "agent_status": "idle"});
    lab.session("live", json!([pane("p2", "/cwd"), pane("p2", "/cwd"), pane("p3", "/cwd"), pane("p4", "/cwd"), pane("p5", "/other"), pane("p6", "/cwd")]),
        json!([agent("p1", "w"), agent("p3", "w"), agent("p4", "wrong")]));

    let found = lab.record();
    for ((case, pane, agent), binding) in cases.iter().zip(&bindings) {
        let o = &found[binding];
        assert_eq!((o["pane"].as_str(), o["worktree"].as_str(), o["agent_present"].as_bool()), (Some(*pane), Some("unrecorded"), Some(*agent)), "{case}: {o}");
    }
    assert!(found[&bindings[3]]["agent_identity"]["kind"] == "claude" && found[&bindings[3]]["session_identity"].is_object(), "{}", found[&bindings[3]]);
    let calls = lab.calls("herdr-calls");
    assert!(calls.iter().all(|c| c.ends_with(" --version") || c.ends_with(" pane list") || c.ends_with(" agent list")), "{calls:?}");
}

/// Replaces `worktree_parser_rejects_incomplete_or_duplicate_identity` and
/// `collector_uses_store_provenance_and_never_calls_effect_commands`.
///
/// A resolved thread's worktree, imported by the migration, is judged from
/// `git worktree list`: registered on the recorded branch it is present;
/// detached or on another branch it is a mismatch; not listed it is absent.
/// Truncated, duplicated, contradictory, relative or prunable listings
/// prove nothing and are unknown. The collector reads the identity from the
/// store, not from the legacy record (broken after the migration), and runs
/// only list and `rev-parse` commands.
#[test]
fn reconcile_records_worktree_evidence_only_from_complete_git_listings() {
    let lab = Lab::new();
    let (repo, work) = (lab.path("repo"), lab.path("work"));
    fs::create_dir(&repo).unwrap();
    lab.git(&repo, &["init", "-q", "-b", "main"]);
    lab.git(&repo, &["commit", "-q", "--allow-empty", "-m", "base"]);
    lab.git(&repo, &["worktree", "add", "-q", "-b", "topic", work.to_str().unwrap()]);
    let record = lab.project().join("threads/t-0001.toml");
    fs::write(&record, toml::to_string(&json!({"id": "t-0001", "title": "Done", "status": "resolved", "kind": "worktree", "created": jiff::Timestamp::now().to_string(),
        "repo": repo, "branch": "topic", "worktree_path": work})).unwrap()).unwrap();
    lab.migrate();
    fs::write(&record, "changed").unwrap();

    let w = work.to_str().unwrap();
    let listings: [(Option<String>, &str); 11] = [
        (None, "present"),
        (Some(format!("worktree {w}\0HEAD abc\0detached\0\0")), "mismatch"),
        (Some(format!("worktree {w}\0HEAD abc\0branch refs/heads/other\0\0")), "mismatch"),
        (Some("worktree /elsewhere\0HEAD abc\0branch refs/heads/topic\0\0".into()), "absent"),
        (Some(String::new()), "unknown"),
        (Some(format!("worktree {w}\0HEAD abc\0branch refs/heads/topic\0")), "unknown"),
        (Some(format!("worktree {w}\0branch refs/heads/topic\0detached\0\0")), "unknown"),
        (Some(format!("worktree {w}\0bare\0bare\0\0")), "unknown"),
        (Some(format!("worktree {w}\0worktree /b\0branch refs/heads/topic\0\0")), "unknown"),
        (Some(format!("worktree {w}\0branch refs/heads/topic\0prunable gitdir file points to non-existent location\0\0")), "unknown"),
        (Some("worktree work\0branch refs/heads/topic\0\0".into()), "unknown"),
    ];
    for (listing, expected) in listings {
        match &listing { Some(text) => fs::write(lab.path("worktrees"), text).unwrap(), None => { let _ = fs::remove_file(lab.path("worktrees")); } }
        let o = lab.record()["thread:t-0001"].clone();
        assert_eq!((o["worktree"].as_str(), o["pane"].as_str()), (Some(expected), Some("unrecorded")), "{listing:?}: {o}");
        assert_eq!(o["worktree_identity"].is_object(), expected == "present", "{listing:?}: {o}");
    }
    assert!(lab.calls("herdr-calls").is_empty(), "{:?}", lab.calls("herdr-calls"));
    let calls = lab.calls("git-calls");
    assert!(!calls.is_empty() && calls.iter().all(|c| c.contains(" worktree list --porcelain -z") || c.contains(" rev-parse ")), "{calls:?}");
}

/// Replaces `ambiguous_stale_effects_never_become_retry_candidates_or_release_lost_capacity`,
/// `expired_claims_require_expiry_and_pending_adapters_only_receive_advice`
/// and `terminated_binding_plans_without_a_live_observation`.
///
/// `reconcile --plan` only advises, never allows dispatch and writes
/// nothing, however often it runs. A delivery that may already have happened
/// is inspected, never retried, even once its task has changed; a pending
/// intent for a changed task is retired; a live claim waits and an expired
/// one must be expired first; only a pending notification with a current
/// task becomes a delivery candidate, and an operation kind without an
/// adapter is inspected. A lost attempt keeps its capacity. A binding whose
/// every attempt has ended needs nothing and no live observation.
#[test]
fn recovery_plans_advise_without_retrying_ambiguous_effects_or_releasing_lost_capacity() {
    let lab = Lab::new();
    lab.migrate();
    for task in ["changed", "current", "old"] { lab.add_task(task); }
    let id = |s: &str| OperationId::new(s).unwrap();
    let operation = |name: &str, task: &str, kind: &str| Operation { id: id(name), task: Some(TaskId::new(task).unwrap()), kind: kind.into(), target: "coordinator".into(),
        payload_version: 1, payload: json!({}), expected_revision: 1, due_unix_ms: 0, idempotency_key: name.into() };
    let attempt = |name: &str, task: &str, state: AttemptState, ended: bool| Mutation::Attempt { expected: None, next: Attempt { id: AttemptId::new(name).unwrap(),
        task: TaskId::new(task).unwrap(), revision: 1, state, snapshot: None, reservation: name.into(), termination_observed: ended } };
    let mut db = migration::open_active(&lab.project()).unwrap();
    let head = db.read_snapshot(None).unwrap().head;
    db.commit(Commit { expected_head: head, mutations: vec![
        attempt("lost", "changed", AttemptState::Lost, false), attempt("finished", "old", AttemptState::Completed, true),
        Mutation::Enqueue(operation("ambiguous", "changed", "runtime.notification")), Mutation::Enqueue(operation("stale", "changed", "runtime.notification")),
        Mutation::Enqueue(operation("claimed", "current", "runtime.notification")), Mutation::Enqueue(operation("expired", "current", "runtime.notification")),
        Mutation::Enqueue(operation("pending", "current", "runtime.notification")), Mutation::Enqueue(operation("legacy", "current", "legacy.notification")),
    ] }).unwrap();
    let revision = |db: &mut herdr_farm::store::SqliteStore, name: &str| db.read_snapshot(None).unwrap().deliveries.iter().find(|d| d.operation.as_str() == name).unwrap().revision;
    // A claim whose lease ran out is marked ambiguous by the expiry sweep.
    let r = revision(&mut db, "ambiguous");
    db.claim_operation(&id("ambiguous"), r, "fixture", now(), 1).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    assert_eq!(db.expire_claims(now()).unwrap(), 1);
    // Claims without the sweep: one lease still live, one already over.
    let r = revision(&mut db, "claimed");
    db.claim_operation(&id("claimed"), r, "fixture", now(), 300_000).unwrap();
    let r = revision(&mut db, "expired");
    db.claim_operation(&id("expired"), r, "fixture", now(), 1).unwrap();
    // The task behind `ambiguous` and `stale` changes afterwards.
    let state = db.read_snapshot(None).unwrap();
    let mut changed = state.tasks.iter().find(|t| t.id.as_str() == "changed").unwrap().clone();
    changed.revision += 1;
    db.commit(Commit { expected_head: state.head, mutations: vec![Mutation::Task { expected: Some(1), next: changed }] }).unwrap();
    drop(db);
    let binding = lab.bind("old", json!({}));
    std::thread::sleep(std::time::Duration::from_millis(5));

    let before = lab.snapshot();
    let plan = lab.ok(&["reconcile", "demo", "--plan"]);
    assert_eq!(lab.ok(&["reconcile", "demo", "--plan"]), plan);
    assert_eq!(lab.snapshot(), before, "planning wrote to the store");
    assert_eq!((plan["dispatch_allowed"].as_bool(), plan["retained_attempts"].as_u64()), (Some(false), Some(1)), "{plan}");
    let advice = |kind: &str, entity: &str| {
        let item = plan["items"].as_array().unwrap().iter().find(|i| i["entity_kind"] == kind && i["entity"] == entity).unwrap_or_else(|| panic!("no {kind} {entity}: {plan}"));
        (item["classification"].as_str().unwrap().to_owned(), item["action"].as_str().unwrap().to_owned())
    };
    for (kind, entity, classification, action) in [
        ("operation", "ambiguous", "requires_reconciliation", "inspect_receipt"),
        ("operation", "stale", "cancelled_or_stale", "retire_stale_intent"),
        ("operation", "claimed", "waiting", "wait"),
        ("operation", "expired", "requires_reconciliation", "expire_claim"),
        ("operation", "pending", "retry_candidate", "deliver_after_validation"),
        ("operation", "legacy", "requires_reconciliation", "inspect_unsupported_adapter"),
        ("attempt", "lost", "requires_reconciliation", "inspect_termination_evidence"),
        ("attempt", "finished", "already_completed", "none"),
        ("runtime", binding.as_str(), "already_completed", "none"),
    ] {
        assert_eq!(advice(kind, entity), (classification.to_owned(), action.to_owned()), "{kind} {entity}");
    }
    assert!(lab.calls("herdr-calls").is_empty() && lab.calls("git-calls").is_empty());
}

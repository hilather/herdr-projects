#![cfg(all(feature = "state-store", target_os = "linux"))]
//! Plan proposals, advisory waits and feedback-driven replans through the
//! compiled CLI over a disposable project store. Feedback is real: signed
//! contracts, CLI submissions and sandboxed verification whose policy fails.
//! Attempts are recorded through the public store API because no worker is
//! launched.
use herdr_projects::{authority, domain::*, migration, runtime};
use serde_json::{json, Value};
use std::{fs, path::PathBuf, process::{Command, Output}};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-projects");
/// Every submission is rejected with `checks_failed`: the output never exists.
const POLICY: &str = r#"{"version":1,"checks":["/usr/bin/git","cat-file","-e","HEAD:missing-output"]}"#;

struct Factory { home: tempfile::TempDir, project: PathBuf, key: PathBuf, repo: PathBuf, store: String, oid: String }

impl Factory {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let key = home.path().join("owner");
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&key).status().unwrap().success());
        let public = fs::read_to_string(key.with_extension("pub")).unwrap().split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        let config = home.path().join("owner.toml");
        fs::write(&config, format!("[authority]\nversion=1\nrevision=1\napproval_public_key={public:?}\n")).unwrap();
        let repo = home.path().join("repo");
        fs::create_dir(&repo).unwrap();
        let mut f = Factory { project: home.path().join("root/demo"), key, repo, store: String::new(), oid: String::new(), home };
        for command in ["new", "pause"] { f.ok(&[command, "demo"]); }
        migration::apply(&f.project, &migration::inspect_with_config(&f.project, &config).unwrap(), true).unwrap();
        let s = runtime::snapshot(&f.project).unwrap();
        runtime::set_state(&f.project, s.head, s.control.unwrap().revision, ProjectState::Active, &config).unwrap();
        f.store = f.project.join(".state/state.db").canonicalize().unwrap().display().to_string();
        f.git(&["init", "-q", "--object-format=sha1"]);
        f.git(&["commit", "-q", "--allow-empty", "-m", "base"]);
        f.oid = f.git(&["rev-parse", "HEAD"]);
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
    /// A refused command leaves the event head unchanged; returns stderr.
    fn refused(&self, args: &[&str]) -> String {
        let head = self.head();
        let out = self.cli(args);
        assert!(!out.status.success(), "{args:?} accepted: {}", String::from_utf8_lossy(&out.stdout));
        assert_eq!(self.head(), head, "{args:?} changed state");
        String::from_utf8_lossy(&out.stderr).into_owned()
    }
    fn head(&self) -> u64 { runtime::snapshot(&self.project).unwrap().head }
    fn path(&self, name: &str) -> PathBuf { self.home.path().join(name) }
    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("/usr/bin/git").env_clear().env("PATH", "/usr/bin:/bin").env("HOME", self.home.path())
            .env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "fixture").env("GIT_AUTHOR_EMAIL", "fixture@example.com")
            .env("GIT_COMMITTER_NAME", "fixture").env("GIT_COMMITTER_EMAIL", "fixture@example.com")
            .current_dir(&self.repo).args(args).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }
    /// Read-only view of the persisted store.
    fn count(&self, sql: &str) -> i64 {
        rusqlite::Connection::open(&self.store).unwrap().query_row(sql, [], |row| row.get(0)).unwrap()
    }
    fn add(&self, task: &str) { self.ok(&["task", "demo", "add", task, "--title", task, "--expected-head", &self.head().to_string()]); }
    /// A task with a signed verify-only contract and one running attempt.
    fn task(&self, task: &str) -> String {
        self.add(task);
        let mut db = migration::open_active(&self.project).unwrap();
        let state = db.read_snapshot(None).unwrap();
        let mut next = state.tasks.iter().find(|t| t.id.as_str() == task).unwrap().clone();
        let previous = next.revision;
        next.revision += 1;
        next.active_attempt = Some(AttemptId::new(format!("{task}-attempt")).unwrap());
        db.commit(Commit { expected_head: state.head, mutations: vec![
            Mutation::Attempt { expected: None, next: Attempt { id: AttemptId::new(format!("{task}-attempt")).unwrap(), task: next.id.clone(), revision: 1,
                state: AttemptState::Running, snapshot: None, reservation: format!("{task}-slot"), termination_observed: false } },
            Mutation::Task { expected: Some(previous), next },
        ] }).unwrap();
        drop(db);
        let body = json!({"version":1,"project_store":self.store,"expected_head":self.head(),"task_id":task,"contract_revision":1,
            "deliverable":"plan fixture","non_goals":"no worker launch","acceptance_policies":[{"id":"builds","text":POLICY}],
            "repository":self.repo,"base_oid":self.oid,"object_format":"sha1","dependencies":[],"scope":{"paths":[],"named_resources":[]},
            "capability_flags":[],"profile_kind":"codex","retry_class":"none","result_schema_id":"result-v1","route":"verify_only",
            "authority":authority::policy_reference(&self.project).unwrap()});
        let doc = self.path(&format!("{task}-contract.json"));
        fs::write(&doc, serde_json::to_vec(&body).unwrap()).unwrap();
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-Y", "sign", "-f"]).arg(&self.key).args(["-n", authority::CONTRACT_SIGNATURE_NAMESPACE]).arg(&doc).output().unwrap().status.success());
        let sig = format!("{}.sig", doc.display());
        self.ok(&["task", "demo", "contract", "put", "--input-file", doc.to_str().unwrap(), "--signature", &sig])["digest"].as_str().unwrap().into()
    }
    /// Submit a result for the task's attempt, have the verifier reject it and
    /// return the feedback item that rejection recorded.
    fn reject(&self, task: &str, digest: &str, key: &str) -> String {
        let before: Vec<String> = self.feedback().iter().map(|f| f["feedback_id"].as_str().unwrap().into()).collect();
        let objects: Vec<_> = self.git(&["rev-list", "--objects", "--all"]).lines().map(|line| {
            let oid = line.split_whitespace().next().unwrap();
            json!({"oid":oid,"relative_path":format!("{}/{}", &oid[..2], &oid[2..])})
        }).collect();
        let submission = self.path(&format!("{key}.json"));
        fs::write(&submission, serde_json::to_vec(&json!({"idempotency_key":key,"task_id":task,"contract_revision":1,"contract_digest":digest,
            "attempt_id":format!("{task}-attempt"),"repository":self.repo,"base_oid":self.oid,"candidate_oid":self.oid,
            "object_format":"sha1","artifact_manifest":[],"claimed_checks":[],"objects":objects})).unwrap()).unwrap();
        let receipt = self.ok(&["result", "demo", "submit", "--input-file", submission.to_str().unwrap()]);
        let policy = self.path("policy.json");
        fs::write(&policy, POLICY).unwrap();
        let work = self.path(&format!("{key}-work"));
        let out = self.cli(&["result", "demo", "verify", receipt["submission_id"].as_str().unwrap(), "--policy-id", "builds",
            "--policy-file", policy.to_str().unwrap(), "--idempotency-key", key, "--work-dir", work.to_str().unwrap()]);
        let run: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
        assert_eq!((run["state"].as_str(), run["reason"].as_str()), (Some("rejected"), Some("checks_failed")), "{}", String::from_utf8_lossy(&out.stderr));
        let added: Vec<String> = self.feedback().iter().map(|f| f["feedback_id"].as_str().unwrap().to_owned()).filter(|id| !before.contains(id)).collect();
        assert_eq!(added.len(), 1, "one rejection records one feedback item");
        added[0].clone()
    }
    fn feedback(&self) -> Vec<Value> { self.ok(&["feedback", "demo", "show"]).as_array().unwrap().clone() }
    fn replan(&self, feedback: &str) -> Value { self.ok(&["feedback", "demo", "replan", feedback]) }
    /// Store inbox items as (kind, done).
    fn inbox(&self) -> Vec<(String, bool)> {
        runtime::snapshot(&self.project).unwrap().inbox.into_iter().map(|item| (item.content.kind, item.done)).collect()
    }
    fn inbox_of(&self, kind: &str) -> usize { self.inbox().iter().filter(|(k, _)| k == kind).count() }
    fn propose(&self, contracts: Value, parent: u64, key: &str) -> Output {
        let path = self.path(&format!("proposal-{key}.json"));
        fs::write(&path, json!({"version":1,"contracts":contracts}).to_string()).unwrap();
        self.cli(&["plan", "propose", "demo", "--input-file", path.to_str().unwrap(), "--expected-plan-revision", &parent.to_string(), "--idempotency-key", key])
    }
    fn wait(&self, args: &[&str]) -> Value { self.ok(&[&["plan", "wait", "demo"], args].concat()) }
    fn replay(&self, wait: &Value) -> Value { self.wait(&["replay", wait["wait_id"].as_str().unwrap()]) }
}

fn contract(task: &str, text: &str, after: &[&str]) -> Value {
    json!({"task_id":task,"text":text,"dependencies":after.iter().map(|p| json!({"predecessor":p,"requirement":"verified_result"})).collect::<Vec<_>>()})
}

/// A planner cannot introduce a dependency cycle, whether inside one proposal
/// or by closing an edge already queued; the refusal writes nothing.
#[test]
fn cyclic_proposals_are_refused_without_a_revision_contract_or_queue_change() {
    let f = Factory::new();
    for task in ["a", "b"] { f.add(task); }
    let queue = f.path("queue.json");
    fs::write(&queue, r#"{"priority":0,"dependencies":[{"predecessor":"b","requirement":"verified_result"}]}"#).unwrap();
    f.ok(&["task", "demo", "queue", "a", "--input-file", queue.to_str().unwrap(), "--expected-revision", "1", "--expected-head", &f.head().to_string()]);
    let scheduler = f.ok(&["scheduler", "demo", "inspect"]);
    let tasks = runtime::snapshot(&f.project).unwrap().tasks;
    let refused = |contracts: Value, key: &str| {
        let head = f.head();
        let out = f.propose(contracts, 0, key);
        assert!(!out.status.success(), "{key} accepted");
        assert!(String::from_utf8_lossy(&out.stderr).contains("cycle"), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(f.head(), head);
        let plan = f.ok(&["plan", "inspect", "demo"]);
        assert_eq!((plan["plan_revision"].clone(), plan["entries"].clone()), (json!(0), json!([])));
        assert_eq!(f.ok(&["scheduler", "demo", "inspect"]), scheduler);
        assert_eq!(runtime::snapshot(&f.project).unwrap().tasks, tasks);
        assert_eq!(f.count("SELECT count(*) FROM task_contracts"), 0);
    };
    refused(json!([contract("a", "loop", &["b"]), contract("b", "loop", &["a"])]), "cycle-key");
    refused(json!([contract("b", "closes the queued edge", &["a"])]), "live-cycle");
    // The same task without the back edge is an ordinary first revision.
    let out = f.propose(json!([contract("b", "independent", &[])]), 0, "acyclic");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(f.ok(&["plan", "inspect", "demo"])["entries"][0]["task_id"], "b");
}

/// Repeated verifier rejections: two automatic replan requests, then one
/// inbox escalation that later rejections join. A planner response keyed by
/// the request closes its notice; a new plan revision restores the budget.
/// Every CLI call reopens the store, so each replay is a restart.
#[test]
fn verifier_rejections_replan_twice_then_escalate_until_a_new_plan() {
    let f = Factory::new();
    let digest = f.task("task");
    let ids: Vec<String> = (1..=4).map(|n| f.reject("task", &digest, &format!("reject-{n}"))).collect();
    let first = f.replan(&ids[0]);
    assert_eq!(first["Automatic"]["automatic_count"], 1, "{first}");
    assert_eq!(f.inbox_of("replan-request"), 1);
    assert_eq!(f.replan(&ids[0]), first);
    assert_eq!(f.inbox_of("replan-request"), 1, "a replayed request must not notify twice");
    let second = f.replan(&ids[1]);
    assert_eq!(second["Automatic"]["automatic_count"], 2, "{second}");
    let third = f.replan(&ids[2]);
    let inbox_id = third["Escalated"]["inbox_id"].as_str().unwrap_or_else(|| panic!("third replan must escalate: {third}"));
    assert_eq!(f.inbox_of("replan-escalation"), 1);
    let escalated = f.ok(&["feedback", "demo", "show", "--id", &ids[2]]);
    assert_eq!((escalated[0]["state"].as_str(), &escalated[0]["replan_proposal_id"]), (Some("open"), &Value::Null));
    assert_eq!(f.replan(&ids[3]), third, "a later rejection joins the open escalation");
    assert_eq!(f.replan(&ids[2]), third);
    assert_eq!(f.inbox().len(), 3);
    assert!(runtime::snapshot(&f.project).unwrap().inbox.iter().any(|item| item.content.id == inbox_id));
    assert_eq!(f.ok(&["plan", "inspect", "demo"])["plan_revision"], 0, "no replan wrote a proposal");
    // The planner answers the second request, keyed by its replan id.
    let key = second["Automatic"]["replan_id"].as_str().unwrap();
    let answer = json!([contract("task", "revised approach", &[])]);
    let out = f.propose(answer.clone(), 0, key);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let response: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!((response["plan_revision"].clone(), response["replayed"].clone()), (json!(1), json!(false)));
    assert_eq!(f.inbox().iter().filter(|(kind, done)| kind == "replan-request" && *done).count(), 1, "the answered request's notice is done");
    assert_eq!(f.inbox().iter().filter(|(kind, done)| kind == "replan-request" && !*done).count(), 1);
    let replayed = f.propose(answer, 0, key);
    assert!(replayed.status.success());
    let replayed: Value = serde_json::from_slice(&replayed.stdout).unwrap();
    assert_eq!((replayed["proposal_id"].clone(), replayed["replayed"].clone()), (response["proposal_id"].clone(), json!(true)));
    let head = f.head();
    assert!(!f.propose(json!([contract("task", "different", &[])]), 0, key).status.success());
    assert_eq!(f.head(), head);
    assert_eq!(f.ok(&["plan", "inspect", "demo"])["plan_revision"], 1);
    // Plan revision 1 starts a fresh budget for the same blocker.
    let fifth = f.reject("task", &digest, "reject-5");
    assert_eq!(f.replan(&fifth)["Automatic"]["automatic_count"], 1);
    assert_eq!(f.inbox().len(), 4);
    assert_eq!(f.ok(&["plan", "inspect", "demo"])["plan_revision"], 1, "a replan request is not a proposal");
}

/// Advisory waits on validation: registration validates its inputs, a poll
/// before the verdict keeps waiting, the verdict wakes only its own task, a
/// replay is answered once, and a subscription made after the verdict (fresh
/// or rearmed) observes the retained evidence. No wake releases capacity.
#[test]
fn validation_waits_wake_once_on_their_own_verdict_and_rearm_from_retained_evidence() {
    let f = Factory::new();
    let digest = f.task("task");
    f.task("other");
    fn register<'a>(task: &'a str, extra: &[&'a str]) -> Vec<&'a str> { [&["register", "--task", task][..], extra].concat() }
    // Capacity triggers must name an existing attempt at a reached revision,
    // and only for resource availability.
    let capacity = |attempt: &str, revision: &str, condition: &str| {
        f.refused(&[&["plan", "wait", "demo"][..], &register("task", &["--condition", condition, "--capacity-attempt", attempt, "--capacity-after-revision", revision])].concat())
    };
    capacity("missing-attempt", "1", "resource_availability");
    capacity("task-attempt", "2", "resource_availability");
    capacity("task-attempt", "1", "validation_completion");
    f.refused(&["plan", "wait", "demo", "register", "--task", "missing", "--condition", "validation_completion"]);
    f.refused(&["plan", "wait", "demo", "register", "--task", "task", "--attempt", "other-attempt", "--condition", "validation_completion"]);
    assert_eq!(f.count("SELECT count(*) FROM wait_conditions"), 0);

    let mine = f.wait(&register("task", &["--attempt", "task-attempt", "--condition", "validation_completion"]));
    assert_eq!(mine["already_registered"], false);
    let again = f.wait(&register("task", &["--attempt", "task-attempt", "--condition", "validation_completion"]));
    assert_eq!(again, json!({"wait_id":mine["wait_id"],"cursor_sequence":mine["cursor_sequence"],"already_registered":true}));
    let theirs = f.wait(&register("other", &["--condition", "validation_completion"]));
    let polled = f.replay(&mine);
    assert_eq!((polled["wake_requested"].clone(), polled["proved"].clone()), (json!(false), json!(false)));
    let early = f.refused(&["plan", "wait", "demo", "rearm", mine["wait_id"].as_str().unwrap()]);
    assert!(early.contains("only a terminal wait"), "{early}");

    f.reject("task", &digest, "verdict");
    let woke = f.replay(&mine);
    assert_eq!((woke["wake_requested"].clone(), woke["proved"].clone(), woke["already_replayed"].clone()), (json!(true), json!(false), json!(false)));
    assert!(woke["replayed_through"].as_i64() > polled["replayed_through"].as_i64());
    assert_eq!(f.inbox_of("wait-wake"), 1);
    let replayed = f.replay(&mine);
    assert_eq!(replayed["already_replayed"], true);
    for field in ["cursor_sequence", "replayed_through", "wake_requested"] { assert_eq!(replayed[field], woke[field], "{field}"); }
    assert_eq!(f.inbox_of("wait-wake"), 1, "a replay must not notify again");
    assert_eq!(f.replay(&theirs)["wake_requested"], false, "another task's verdict woke this wait");

    // Subscribing after the verdict must not lose it.
    let late = f.wait(&register("task", &["--condition", "validation_completion"]));
    assert_eq!(f.replay(&late)["wake_requested"], true);
    let previous = mine["wait_id"].as_str().unwrap();
    let successor = f.wait(&["rearm", previous]);
    assert_ne!(successor["wait_id"], mine["wait_id"]);
    assert_eq!(successor["already_registered"], false);
    let conflicting = f.refused(&["plan", "wait", "demo", "rearm", previous, "--deadline", "2100-01-01T00:00:00Z"]);
    assert!(conflicting.contains("different deadline"), "{conflicting}");
    assert_eq!(f.wait(&["rearm", previous]), json!({"wait_id":successor["wait_id"],"cursor_sequence":successor["cursor_sequence"],"already_registered":true}));
    let rearmed = f.replay(&successor);
    assert_eq!((rearmed["wake_requested"].clone(), rearmed["proved"].clone()), (json!(true), json!(false)));
    assert_eq!(f.replay(&mine), replayed, "the predecessor's terminal evidence is unchanged");
    assert_eq!(f.inbox_of("wait-wake"), 3);
    let snapshot = runtime::snapshot(&f.project).unwrap();
    assert_eq!(snapshot.attempts.len(), 2);
    assert!(snapshot.attempts.iter().all(|attempt| attempt.retains_capacity()), "an advisory wake released capacity");
    // The persisted wait history refuses rewriting.
    let raw = rusqlite::Connection::open(&f.store).unwrap();
    assert!(raw.execute("UPDATE wait_conditions SET wake_requested=0", []).is_err());
    assert!(raw.execute("DELETE FROM wait_rearms", []).is_err());
}

/// Automatic replans are the ticker's job, off by default and only for an
/// active project. The ticker drains every pending rejection into the same
/// bounded decisions an operator request would reach, and nothing new is
/// requested once the switch is off again.
#[test]
fn ticker_requests_replans_only_while_enabled_and_active() {
    use std::{process::Stdio, time::{Duration, Instant}};
    let f = Factory::new();
    let digest = f.task("task");
    let root = f.home.path().join("root");
    // Each pass ends with the wait service before the replan service; an
    // expired deadline wait is woken on the first pass, marking it done.
    let pass = |marker: &str| {
        let wait = f.wait(&["register", "--task", "task", "--condition", "user_decision", "--deadline", marker]);
        let mut child = Command::new(BIN).env_clear().env("HOME", f.home.path()).env("PATH", "/usr/bin:/bin")
            .args(["--root", root.to_str().unwrap(), "ticker", "run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let woken = || f.count(&format!("SELECT wake_requested FROM wait_conditions WHERE wait_id='{}'", wait["wait_id"].as_str().unwrap())) == 1;
        while !woken() {
            if Instant::now() > deadline { let _ = child.kill(); panic!("no ticker pass: {}", fs::read_to_string(root.join(".ticker.log")).unwrap_or_default()); }
            std::thread::sleep(Duration::from_millis(20));
        }
        fs::write(root.join(".ticker.stop"), b"").unwrap();
        let status = child.wait().unwrap();
        fs::remove_file(root.join(".ticker.stop")).unwrap();
        assert!(status.success());
    };
    let ids: Vec<String> = (1..=4).map(|n| f.reject("task", &digest, &format!("auto-{n}"))).collect();
    let requested = || f.inbox_of("replan-request") + f.inbox_of("replan-escalation");
    pass("2000-01-01T00:00:00Z");
    assert_eq!(requested(), 0, "automatic replans are opt-in");
    f.ok(&["plan", "auto-replan", "demo", "on", "--expected-head", &f.head().to_string()]);
    // The attempt's observed end, as the controller would record it, lets
    // the project pause and resume.
    let mut db = migration::open_active(&f.project).unwrap();
    let state = db.read_snapshot(None).unwrap();
    let mut attempt = state.attempts[0].clone();
    let mut task = state.tasks.iter().find(|t| t.id.as_str() == "task").unwrap().clone();
    let (attempt_revision, task_revision) = (attempt.revision, task.revision);
    (attempt.revision, attempt.state, attempt.termination_observed) = (attempt_revision + 1, AttemptState::Failed, true);
    (task.revision, task.active_attempt) = (task_revision + 1, None);
    db.commit(Commit { expected_head: state.head, mutations: vec![
        Mutation::Attempt { expected: Some(attempt_revision), next: attempt }, Mutation::Task { expected: Some(task_revision), next: task }] }).unwrap();
    drop(db);
    let control = runtime::snapshot(&f.project).unwrap().control.unwrap();
    f.ok(&["runtime", "demo", "state", "paused", "--expected-revision", &control.revision.to_string(), "--expected-head", &f.head().to_string()]);
    pass("2000-01-01T00:00:01Z");
    assert_eq!(requested(), 0, "a paused project requests nothing");
    let s = runtime::snapshot(&f.project).unwrap();
    runtime::set_state(&f.project, s.head, s.control.unwrap().revision, ProjectState::Active, &f.path("owner.toml")).unwrap();
    pass("2000-01-01T00:00:02Z");
    assert_eq!((f.inbox_of("replan-request"), f.inbox_of("replan-escalation")), (2, 1));
    let decisions: Vec<Value> = ids.iter().map(|id| f.replan(id)).collect();
    let automatic: std::collections::BTreeSet<&str> = decisions.iter().filter(|d| d["Automatic"]["automatic_count"] == 2).filter_map(|d| d["Automatic"]["replan_id"].as_str()).collect();
    assert_eq!(automatic.len(), 2, "two automatic requests exhaust the budget: {decisions:?}");
    let escalations: std::collections::BTreeSet<&str> = decisions.iter().filter_map(|d| d["Escalated"]["inbox_id"].as_str()).collect();
    assert_eq!(escalations.len(), 1, "the rest coalesce into one escalation: {decisions:?}");
    assert_eq!(requested(), 3, "operator replays add nothing");
    // After a new plan, with the switch off, a later rejection waits for the
    // operator, who gets a fresh budget.
    assert!(f.propose(json!([contract("task", "new plan", &[])]), 0, "auto-new-plan").status.success());
    f.ok(&["plan", "auto-replan", "demo", "off", "--expected-head", &f.head().to_string()]);
    let later = f.reject("task", &digest, "later");
    pass("2000-01-01T00:00:03Z");
    assert_eq!(requested(), 3);
    assert_eq!(f.ok(&["feedback", "demo", "show", "--id", &later])[0]["state"], "open");
    assert_eq!(f.replan(&later)["Automatic"]["automatic_count"], 1);
    for (id, decision) in ids.iter().zip(decisions) { assert_eq!(f.replan(id), decision, "decisions survive the new plan"); }
}

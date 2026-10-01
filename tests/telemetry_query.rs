//! TM4.1 query service end to end: the metric registry, fixed terminal and
//! assignment cohorts, correction-aware as-of revisions, byte-identical
//! rebuilds, bounded pagination and the `completed_task` rejection, all
//! through `herdr-projects telemetry` on the CLI over planted canonical rows.
//! Expected values are hand-computed from plan docs 07 §1/§3, 08 §4 and
//! 10 §3–§4 and contracts §6; none is read back from a production aggregate.

#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.

mod support;

use herdr_projects::store::SqliteStore;
use serde_json::{Value, json};
use std::{fs, path::{Path, PathBuf}, process::Command};
use support::telemetry::*;

/// A project `demo` under its own root with a fresh canonical store.
struct Planted { tmp: tempfile::TempDir, root: PathBuf, project: PathBuf }

impl Planted {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let project = root.join("demo");
        fs::create_dir_all(project.join(".state")).unwrap();
        fs::create_dir_all(tmp.path().join("home")).unwrap();
        drop(SqliteStore::create(&project.join(".state/state.db")).unwrap());
        Planted { tmp, root, project }
    }
    fn db(&self) -> rusqlite::Connection {
        let db = rusqlite::Connection::open(self.project.join(".state/state.db")).unwrap();
        db.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
        db
    }
    fn command(&self, args: &[&str]) -> std::process::Output {
        Command::new(BIN).env_clear().env("HOME", self.tmp.path().join("home")).env("PATH", "/usr/bin:/bin")
            .args(["--root", self.root.to_str().unwrap(), "telemetry", "demo"]).args(args).output().unwrap()
    }
    fn raw(&self, args: &[&str]) -> Vec<u8> {
        let out = self.command(args);
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        out.stdout
    }
    fn json(&self, args: &[&str]) -> Value { serde_json::from_slice(&self.raw(args)).unwrap() }
    fn fail(&self, args: &[&str]) -> String {
        let out = self.command(args);
        assert!(!out.status.success(), "{args:?} succeeded: {}", String::from_utf8_lossy(&out.stdout));
        String::from_utf8(out.stderr).unwrap()
    }
    /// The first `results` entry of a JSON query.
    fn query(&self, args: &[&str]) -> Value {
        let mut all = vec!["query", "--json"];
        all.extend_from_slice(args);
        self.json(&all)["results"][0].clone()
    }
    fn state_bytes(&self) -> Vec<u8> { fs::read(self.project.join(".state/state.db")).unwrap() }
}

const OID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
fn hex(seed: &str) -> String { format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(seed.as_bytes())) }

/// One attempt: `(id, state, [(lifecycle state, unix ms)])`.
type AttemptSpec<'a> = (&'a str, &'a str, &'a [(&'a str, i64)]);

/// Plant a task with its attempts and lifecycle marks; `route` gives it a
/// contract. `verified` = (attempt, created ms) plants a submission and a
/// verified result; `integration` = (state, integrated ms) an integration of it.
fn plant(db: &rusqlite::Connection, task: &str, state: &str, route: Option<&str>, attempts: &[AttemptSpec], verified: Option<(&str, i64)>, integration: Option<(&str, Option<i64>)>) {
    db.execute("INSERT INTO tasks(id,revision,state,title) VALUES(?1,1,?2,?1)", [task, state]).unwrap();
    for (id, attempt_state, marks) in attempts {
        db.execute("INSERT INTO attempts(id,task_id,revision,state,reservation,termination_observed) VALUES(?1,?2,2,?3,?1,?4)",
            rusqlite::params![id, task, attempt_state, i64::from(["completed", "failed", "cancelled", "lost"].contains(attempt_state))]).unwrap();
        for (mark, at) in *marks {
            db.execute("INSERT INTO attempt_lifecycle(attempt_id,state,attempt_revision,unix_ms,source) VALUES(?1,?2,1,?3,'fixture')", rusqlite::params![id, mark, at]).unwrap();
        }
    }
    if let Some(route) = route {
        db.execute("INSERT INTO task_contracts(task_id,contract_revision,project_store,expected_head,repository,base_oid,object_format,route,raw_bytes,raw_digest,installed_seq)
            VALUES(?1,1,'store',1,'/repo',?2,'sha1',?3,x'7b7d',?4,1)", rusqlite::params![task, OID, route, hex(&format!("contract-{task}"))]).unwrap();
    }
    if let Some((attempt, at)) = verified {
        let (submission, result) = (hex(&format!("submission-{task}")), hex(&format!("result-{task}")));
        db.execute("INSERT INTO result_submissions(submission_id,project_store,idempotency_key,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,artifact_manifest,claimed_checks,created_unix_ms)
            VALUES(?1,'store',?1,?2,'{}',?3,1,?2,?4,'/repo',?5,?5,'sha1','[]','[]',?6)", rusqlite::params![submission, hex("d"), task, attempt, OID, at - 10]).unwrap();
        db.execute("INSERT INTO verified_results(result_id,run_id,submission_id,commit_oid,tree_oid,object_format,policy_digest,receipt_digest,isolation,memory_fence,created_unix_ms)
            VALUES(?1,?1,?2,?3,?3,'sha1',?4,?4,'linux-unshare-user-pid-mount-v1',0,?5)", rusqlite::params![result, submission, OID, hex("e"), at]).unwrap();
        if let Some((op_state, integrated)) = integration {
            let operation = format!("op-{task}");
            db.execute("INSERT INTO integration_operations(operation_id,project_store,idempotency_key,payload_digest,repository,ref_name,expected_old_oid,verified_result_id,state,generation,object_format,checks_passed,created_unix_ms)
                VALUES(?1,'store',?1,?2,'/repo','refs/heads/main',?3,?4,?5,1,'sha1',1,?6)", rusqlite::params![operation, hex("f"), OID, result, op_state, at + 10]).unwrap();
            if let Some(integrated) = integrated { integrate(db, task, integrated); }
        }
    }
}

fn integrate(db: &rusqlite::Connection, task: &str, at: i64) {
    db.execute("INSERT INTO integrated_commits(integrated_id,candidate_id,operation_id,repository,ref_name,commit_oid,tree_oid,expected_old_oid,object_format,created_unix_ms)
        VALUES(?1,?1,?2,'/repo','refs/heads/main',?3,?3,?3,'sha1',?4)", rusqlite::params![hex(&format!("integrated-{task}")), format!("op-{task}"), OID, at]).unwrap();
}

/// Contracts §6 worked example with lifecycle times: t1 verify_only verified
/// at 2000; t2 integrated at 2600 after a failed and a completed attempt; t3
/// verified, integration blocked (open); t4 failed twice (last end 3100); t5
/// queued with a running attempt (open).
fn worked_example(p: &Planted) {
    let db = p.db();
    plant(&db, "t1", "succeeded", Some("verify_only"), &[("t1-a1", "completed", &[("reserved", 1000), ("completed", 1500)])], Some(("t1-a1", 2000)), None);
    plant(&db, "t2", "succeeded", Some("verify_then_integrate"), &[("t2-a1", "failed", &[("reserved", 1100), ("failed", 1400)]),
        ("t2-a2", "completed", &[("reserved", 1600), ("completed", 1900)])], Some(("t2-a2", 2100)), Some(("integrated", Some(2600))));
    plant(&db, "t3", "blocked", Some("verify_then_integrate"), &[("t3-a1", "completed", &[("reserved", 1200), ("completed", 1800)])], Some(("t3-a1", 2200)), Some(("blocked", None)));
    plant(&db, "t4", "failed", None, &[("t4-a1", "failed", &[("reserved", 1300), ("failed", 2300)]), ("t4-a2", "failed", &[("reserved", 2400), ("failed", 3100)])], None, None);
    plant(&db, "t5", "queued", None, &[("t5-a1", "running", &[("reserved", 1700)])], None, None);
}

fn nd(m: &Value) -> (Value, Value, Value) { (m["numerator"].clone(), m["denominator"].clone(), m["value"].clone()) }

/// Registry fixture: every metric declared once with its definition, family,
/// cohorts and activation; quality families wait for the TM3.5 certificate.
#[test]
fn registry_declares_every_metric_and_gates_families() {
    let p = Planted::new();
    let registry = p.json(&["metrics", "registry", "--json"]);
    assert_eq!(registry["registry"], "analytics-registry.v4");
    assert_eq!(registry["rejected_cohorts"], json!({"completed_task": "ambiguous_cohort"}));
    let metrics = registry["metrics"].as_array().unwrap();
    let ids: Vec<&str> = metrics.iter().map(|m| m["id"].as_str().unwrap()).collect();
    let mut expected: Vec<String> = (1..=50).map(|n| format!("M{n:02}")).collect();
    expected.push("flaky_tests".into());
    expected.push("verification_flip_rate".into());
    expected.sort();
    assert_eq!(ids, expected, "M01-M50 and the lane C flaky-test proxy, once each, in order");
    let get = |id: &str| metrics.iter().find(|m| m["id"] == id).unwrap().clone();
    let m02 = get("M02");
    assert_eq!((&m02["definition"], &m02["family"], &m02["unit"]), (&json!("M02.cohort-v1"), &json!("lifecycle"), &json!("ratio")));
    assert_eq!(m02["versions"][0]["cohorts"], json!(["terminal_cohort", "assignment_cohort"]));
    assert_eq!((&m02["versions"][1]["definition"], &m02["versions"][1]["provider"]), (&json!("M02.slice-v1"), &json!({"kind": "central_report"})));
    assert_eq!(m02["activation"]["certificate"], "docs/telemetry/certificate-core.md");
    assert_eq!((&get("M08")["certification"]["status"], &get("M08")["versions"][0]["provider"]), (&json!("certified-live"), &json!({"kind": "lane", "stream": "accounting"})));
    assert_eq!((&get("M10")["definition"], &get("M10")["versions"][0]["provider"]),
        (&json!("M10.v1"), &json!({"kind": "lane", "stream": "accounting"})));
    assert_eq!(registry["comparison"]["activity_metrics"][0]["estimator"], "ratio_of_token_sums.v1");
    assert_eq!(get("M18")["certification"]["restriction"], "execution_duration_not_exposed");
    // Quality families: active on the TM3.5 fixture certificate, statuses as its §2 table.
    assert_eq!(registry["quality_certificate"], "docs/telemetry/certificate-quality.md");
    for (id, status) in [("M20", "certified-fixture"), ("M22", "certified-fixture"), ("M24", "restricted"), ("M25", "certified-fixture"), ("M29", "certified-fixture"),
        ("M41", "certified-fixture"), ("M42", "certified-fixture"), ("M43", "certified-fixture"), ("M44", "certified-fixture")] {
        let m = get(id);
        assert_eq!((&m["active"], &m["certification"]["status"], &m["activation"]["card"]), (&json!(true), &json!(status), &json!("TM3.5")), "{id}");
        assert_eq!(m["activation"]["certificate"], "docs/telemetry/certificate-quality.md", "{id}");
        assert!(m["activation"]["production"].as_str().unwrap().starts_with("awaiting_producer_certificate"), "{id}: fixture certificate only");
    }
    for (id, status) in [("M45", "certified-fixture"), ("M46", "unavailable"), ("M47", "restricted"), ("M48", "restricted"), ("flaky_tests", "unavailable")] {
        let m = get(id);
        assert_eq!((&m["active"], &m["family"], &m["proxy"], &m["certification"]["status"]), (&json!(true), &json!("proxy"), &json!(true), &json!(status)), "{id}");
    }
    assert_eq!((&get("M30")["certification"]["status"], &get("M30")["versions"][0]["provider"]["reason"]), (&json!("fixture"), &Value::Null));
    assert_eq!((&get("M49")["active"], &get("M49")["activation"]["card"]), (&json!(true), &json!("TM4.6")));
    // Text form: one line per metric.
    let text = String::from_utf8(p.raw(&["metrics", "registry"])).unwrap();
    assert_eq!(text.lines().count(), 53, "{text}");
    assert!(text.contains("M20 review_completion M20.v1 review family=review_quality cohorts=assignment_cohort unit=ratio certification=certified-fixture active"), "{text}");
    assert!(text.contains("M49 replay_suite_pass_rate M49.v1 central family=replay cohorts=activity_window unit=ratio certification=fixture active"), "{text}");

    // The query service honours the gates: an inactive family or an absent
    // producer is unavailable with its reason, never a value; an active one
    // carries the lane's own value and reasons.
    let q = p.json(&["query", "--json", "--metric", "M20,M45,M49,M03"]);
    let reason = |i: usize| (q["results"][i]["status"].clone(), q["results"][i]["reason"].clone());
    assert_ne!(reason(0).1, json!("awaiting_quality_certificate"));
    assert_eq!((&q["results"][0]["detail"]["definition"], &q["results"][0]["certification"]["status"]), (&json!("M20.v1"), &json!("certified-fixture")));
    assert_eq!(reason(1), (json!("unavailable"), json!("collection_not_run")), "the proxy lane's own reason: no sidecar");
    assert_eq!((&q["results"][1]["proxy"], &q["results"][1]["detail"]["source_trust"]), (&json!(true), &json!("proxy_observed")));
    assert_eq!(reason(2), (json!("unavailable"), json!("no_replay_suite")), "the replay producer's own reason: no suite recorded");
    assert_eq!(q["results"][2]["value"], json!({"status": "unavailable", "reason": "no_replay_suite"}));
    assert_eq!(reason(3), (json!("unavailable"), json!("operating_hours_not_recorded")));
}

/// Independent fixture (contracts §6, doc 07 §3) against the query's
/// numerators, denominators and exclusions, windowed and by dimension.
#[test]
fn query_matches_hand_computed_lifecycle_fixture() {
    let p = Planted::new();
    worked_example(&p);
    // T = {t1, t2, t4}, A = {t1, t2}; t3, t5 open. Attempts of T: 1 + 2 + 2.
    let m02 = p.query(&["--metric", "M02"]);
    assert_eq!((&m02["definition"], &m02["cohort"]), (&json!("M02.cohort-v1"), &json!("terminal_cohort")));
    assert_eq!(nd(&m02), (json!(2), json!(3), json!("2/3")));
    assert_eq!(m02["exclusions"], json!({"open": 2}));
    assert_eq!(m02["breakdown"], json!({"accepted": 2, "failed": 1}));
    assert_eq!(m02["coverage"], json!({"state": "complete", "known": 3, "expected": 3, "missing": 0, "reasons": {}}));
    assert_eq!(m02["event_cutoff_unix_ms"], 3100, "t4's last attempt end");
    assert_eq!((&m02["projection"]["mode"], &m02["lag_ms"], &m02["rate_card_revision"]), (&json!("live"), &json!(0), &Value::Null));
    assert!(m02["source_watermarks"]["canonical"]["lifecycle_digest"].as_str().unwrap().starts_with("sha256:"));
    let m07 = p.query(&["--metric", "M07"]);
    assert_eq!(nd(&m07), (json!(5), json!(2), json!("5/2")));
    assert_eq!((&m07["attempts_without_decision"], &m07["unknown_launch_outcome"]), (&json!(5), &json!(0)));
    let m01 = p.query(&["--metric", "M01"]);
    assert_eq!((&m01["value"], &m01["numerator"], &m01["denominator"]), (&json!(2), &json!(2), &Value::Null));
    // M06 nearest rank: t1 2000-1000 = 1000, t2 2600-1100 = 1500; ceil(0.95*2) = 2nd.
    let m06 = p.query(&["--metric", "M06"]);
    assert_eq!((&m06["value"], &m06["samples"], &m06["breakdown"]["failed"]), (&json!(1500), &json!(2), &json!(1)));

    // The certified slice definition is still served, and it equals the report's body.
    let report = p.json(&["report", "--json"]);
    let slice = p.query(&["--metric", "M02.slice-v1"]);
    assert_eq!(nd(&slice), (json!(2), json!(3), json!("2/3")));
    assert_eq!(slice["detail"], report["metrics"]["M02"]);

    // Half-open windows by terminal time: [2000, 3000) holds t1 (2000) and t2 (2600), not t4 (3100).
    let w = p.query(&["--metric", "M02", "--from", "2000", "--to", "3000"]);
    assert_eq!(nd(&w), (json!(2), json!(2), json!("2/2")));
    assert_eq!(w["exclusions"], json!({"open": 2, "outside_window": 1}));
    let w = p.query(&["--metric", "M07", "--from", "2500", "--to", "3200"]);
    assert_eq!(nd(&w), (json!(4), json!(1), json!("4/1")), "t2's two attempts and t4's two over t2");
    let w = p.query(&["--metric", "M02", "--from", "3200"]);
    assert_eq!((&w["value"], &w["reason"], &w["status"]), (&Value::Null, &json!("empty_denominator"), &json!("empty")));

    // Assignment cohort: every assigned task, open ones unfinished (censored), not dropped.
    let a = p.query(&["--metric", "M02", "--cohort", "assignment_cohort"]);
    assert_eq!(nd(&a), (json!(2), json!(5), json!("2/5")));
    assert_eq!((&a["censored"], &a["provisional"]), (&json!({"unfinished": 2}), &json!(true)));
    // Horizon 1200 ms: t2 (1100 -> 2600) and t4 (1300 -> 3100) are unfinished at it.
    let a = p.query(&["--metric", "M02", "--cohort", "assignment_cohort", "--horizon-ms", "1200"]);
    assert_eq!((nd(&a), &a["censored"]), ((json!(1), json!(5), json!("1/5")), &json!({"unfinished": 4})));
    let a = p.query(&["--metric", "M02", "--cohort", "assignment_cohort", "--from", "1000", "--to", "1250"]);
    assert_eq!((nd(&a), &a["exclusions"]), ((json!(2), json!(3), json!("2/3")), &json!({"outside_window": 2})));

    // Bounded dimensions; identities are refused as labels.
    let by = p.query(&["--metric", "M02", "--by", "route"]);
    assert_eq!(by["cells"], json!([
        {"dimension": {"route": "none"}, "numerator": 0, "denominator": 1, "value": "0/1", "reason": null},
        {"dimension": {"route": "verify_only"}, "numerator": 1, "denominator": 1, "value": "1/1", "reason": null},
        {"dimension": {"route": "verify_then_integrate"}, "numerator": 1, "denominator": 1, "value": "1/1", "reason": null}]));
    let by = p.query(&["--metric", "M02", "--by", "task_id"]);
    assert_eq!((&by["status"], &by["reason"]), (&json!("unavailable"), &json!("high_cardinality_dimension")));
    let unsupported = p.query(&["--metric", "M02", "--cohort", "activity_window"]);
    assert_eq!((&unsupported["reason"], &unsupported["diagnostic"]["supported"]), (&json!("cohort_unsupported"), &json!(["terminal_cohort", "assignment_cohort"])));
    let window_end = p.query(&["--metric", "M08", "--to", "5000"]);
    assert_eq!(window_end["reason"], "window_end_unsupported");
    let text = String::from_utf8(p.raw(&["query", "--metric", "M02"])).unwrap();
    assert_eq!(text, "M02 task_acceptance_rate M02.cohort-v1 terminal_cohort 2/3 numerator=2 denominator=3 coverage=complete live\n");
}

/// Doc 10 §3: succeeded + failed + cancelled tasks stay in `T`; the open task is
/// separate; a cancellation with no time is excluded by name from a bounded
/// window, never silently dropped.
#[test]
fn terminal_cohort_keeps_failed_and_cancelled_tasks() {
    let p = Planted::new();
    let db = p.db();
    plant(&db, "s", "succeeded", Some("verify_only"), &[("s-a1", "completed", &[("reserved", 1000), ("completed", 1500)])], Some(("s-a1", 1600)), None);
    plant(&db, "f", "failed", None, &[("f-a1", "failed", &[("reserved", 1100), ("failed", 1700)])], None, None);
    plant(&db, "c", "cancelled", None, &[("c-a1", "cancelled", &[("reserved", 1200), ("cancelled", 1800)])], None, None);
    plant(&db, "c0", "cancelled", None, &[], None, None);
    plant(&db, "o", "running", None, &[("o-a1", "running", &[("reserved", 1300)])], None, None);
    drop(db);
    let all = p.query(&["--metric", "M02", "--cohort", "terminal_cohort"]);
    assert_eq!(nd(&all), (json!(1), json!(4), json!("1/4")));
    assert_eq!((&all["breakdown"], &all["exclusions"]), (&json!({"accepted": 1, "cancelled": 2, "failed": 1}), &json!({"open": 1})));
    let drill = p.json(&["query", "--json", "--metric", "M02", "--drill", "denominator"])["drill"].clone();
    let ids: Vec<&str> = drill["rows"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["c", "c0", "f", "s"], "failed and cancelled tasks are drillable members of T");
    assert_eq!(drill["rows"][2], json!({"entity": "task", "id": "f", "disposition": "failed", "terminal_unix_ms": 1700, "assigned_unix_ms": 1100, "attempts": 1,
        "route": "none", "agent_kind": "unknown", "task_class": "unclassified"}));

    // Doc 10 §3 shape: M02 = 1/3 and M07 = 3/1 over [1000, 2000); c0 has no terminal time.
    let w = p.query(&["--metric", "M02", "--from", "1000", "--to", "2000"]);
    assert_eq!(nd(&w), (json!(1), json!(3), json!("1/3")));
    assert_eq!(w["exclusions"], json!({"open": 1, "terminal_time_unknown": 1}));
    assert_eq!(w["coverage"], json!({"state": "partial", "known": 3, "expected": 4, "missing": 1, "reasons": {"terminal_time_unknown": 1}}));
    assert_eq!(nd(&p.query(&["--metric", "M07", "--from", "1000", "--to", "2000"])), (json!(3), json!(1), json!("3/1")));
    let unknown = p.json(&["query", "--json", "--metric", "M02", "--from", "1000", "--to", "2000", "--drill", "excluded.terminal_time_unknown"])["drill"]["rows"].clone();
    assert_eq!(unknown.as_array().unwrap().iter().map(|r| r["id"].clone()).collect::<Vec<_>>(), [json!("c0")]);
    // The same failed and cancelled tasks are in every cell of a dimension breakdown.
    let by = p.query(&["--metric", "M02", "--by", "agent_kind"]);
    assert_eq!(by["cells"], json!([
        {"dimension": {"agent_kind": "unassigned"}, "numerator": 0, "denominator": 1, "value": "0/1", "reason": null},
        {"dimension": {"agent_kind": "unknown"}, "numerator": 1, "denominator": 3, "value": "1/3", "reason": null}]));
}

/// `completed_task` is ambiguous (doc 08 §4): rejected with a structured
/// diagnostic by the query and by `analytics refresh`, never read as success-only.
#[test]
fn completed_task_cohort_is_rejected() {
    let p = Planted::new();
    worked_example(&p);
    let err = p.fail(&["query", "--json", "--metric", "M02", "--cohort", "completed_task"]);
    assert!(err.contains("query rejected: {\"accepted\":[\"activity_window\",\"terminal_cohort\",\"assignment_cohort\"],\"code\":\"ambiguous_cohort\",\"cohort\":\"completed_task\""), "{err}");
    assert!(p.fail(&["analytics", "refresh", "--metric", "M02", "--cohort", "completed_task"]).contains("ambiguous_cohort"));
    assert!(p.fail(&["query", "--metric", "M02", "--cohort", "succeeded"]).contains("unknown_cohort"));
    assert!(p.fail(&["query", "--metric", "M99"]).contains("unknown_metric"));
    assert!(p.fail(&["query", "--metric", "M02.v9"]).contains("unknown_definition"));
    assert!(p.fail(&["query", "--metric", "M02", "--from", "5", "--to", "5"]).contains("empty_window"));
}

/// Doc 10 §4: a late correction (t3's integration receipt) appends a
/// restatement; the earlier answer stays reproducible by sequence and by
/// knowledge time; nothing is written to `state.db`.
#[test]
fn late_correction_appends_a_restatement() {
    let p = Planted::new();
    worked_example(&p);
    // No sidecar: nothing to record into.
    assert_eq!(p.json(&["analytics", "refresh"]), json!({"status": "unavailable", "reason": "collection_not_run"}));
    p.raw(&["collect"]);
    let state = p.state_bytes();
    let first = p.json(&["analytics", "refresh"]);
    let cell = json!({"by": null, "cohort": "terminal_cohort", "definition": "M02.cohort-v1", "from": null, "horizon_ms": null, "metric": "M02", "to": null});
    let revision = |out: &Value| out["appended"].as_array().unwrap().iter().find(|a| a["cell"] == cell).map(|a| a["revision"].as_i64().unwrap());
    let r1 = revision(&first).expect("the default cells are tracked");
    assert_eq!(first["unchanged"], 0);
    let known_at = unix_ms();
    let again = p.json(&["analytics", "refresh"]);
    assert_eq!((again["appended"].as_array().unwrap().len(), &again["unchanged"]), (0, &first["cells"]), "incremental: an unchanged cell appends nothing");

    let before = p.raw(&["query", "--json", "--metric", "M02", "--as-of-seq", &r1.to_string()]);
    // Late correction: t3's integration lands.
    let db = p.db();
    db.execute("UPDATE integration_operations SET state='integrated' WHERE operation_id='op-t3'", []).unwrap();
    integrate(&db, "t3", 2700);
    drop(db);
    let state = { let now = p.state_bytes(); assert_ne!(now, state); now };
    let live = p.query(&["--metric", "M02"]);
    assert_eq!(nd(&live), (json!(3), json!(4), json!("3/4")));
    assert_eq!(live["projection"]["matches_revision"], Value::Null, "not recorded yet");
    std::thread::sleep(std::time::Duration::from_millis(5));
    let second = p.json(&["analytics", "refresh"]);
    let r2 = revision(&second).expect("the corrected cell is restated");
    let restated = second["appended"].as_array().unwrap().iter().find(|a| a["cell"] == cell).unwrap().clone();
    assert_eq!((&restated["kind"], &restated["supersedes"]), (&json!("restatement"), &json!(r1)));
    assert!(second["appended"].as_array().unwrap().iter().all(|a| a["cell"]["metric"] != "M08"), "only changed cells are restated");
    assert_eq!(p.state_bytes(), state, "refresh never writes state.db");

    let old = p.query(&["--metric", "M02", "--as-of-seq", &r1.to_string()]);
    assert_eq!(nd(&old), (json!(2), json!(3), json!("2/3")));
    assert_eq!((&old["projection"]["restated"], &old["projection"]["superseded_by"], &old["projection"]["current_revision"]), (&json!(true), &json!(r2), &json!(r2)));
    let by_time = p.query(&["--metric", "M02", "--as-of", &known_at.to_string()]);
    assert_eq!((nd(&by_time), &by_time["projection"]["revision"]), ((json!(2), json!(3), json!("2/3")), &json!(r1)));
    assert_eq!(by_time["observation_cutoff_unix_ms"], old["projection"]["recorded_unix_ms"]);
    let new = p.query(&["--metric", "M02", "--as-of-seq", &r2.to_string()]);
    assert_eq!((nd(&new), &new["projection"]["restated"]), ((json!(3), json!(4), json!("3/4")), &json!(false)));
    assert_eq!(p.query(&["--metric", "M02"])["projection"]["matches_revision"], json!(r2));
    let early = p.query(&["--metric", "M02", "--as-of", "1"]);
    assert_eq!(early["reason"], "no_revision_as_of");
    // The earlier answer is reproducible byte for byte; only the query time
    // and the pointers to its later restatement differ.
    let strip = |bytes: &[u8]| {
        let mut v: Value = serde_json::from_slice(bytes).unwrap();
        v["query_unix_ms"] = Value::Null;
        for key in ["superseded_by", "current_revision", "restated"] { v["results"][0]["projection"][key] = Value::Null; }
        v
    };
    assert_eq!(strip(&p.raw(&["query", "--json", "--metric", "M02", "--as-of-seq", &r1.to_string()])), strip(&before));
    assert_eq!(serde_json::from_slice::<Value>(&before).unwrap()["results"][0]["projection"]["restated"], false, "not yet restated then");
    let history = p.json(&["analytics", "revisions", "--metric", "M02"])["revisions"].clone();
    assert_eq!(history.as_array().unwrap().iter().map(|r| (r["revision"].as_i64().unwrap(), r["kind"].as_str().unwrap().to_owned())).collect::<Vec<_>>(),
        [(r1, "initial".to_owned()), (r2, "restatement".to_owned())]);
    assert_ne!(history[0]["watermarks"]["canonical"]["lifecycle_digest"], history[1]["watermarks"]["canonical"]["lifecycle_digest"]);
}

/// A late usage record (doc 10 §4 "observed later") restates the lane metric M08.
#[test]
fn late_usage_record_restates_lane_metric() {
    let f = Fixture::new();
    let path = f.rollout(&f.home, SID, &["head.jsonl"], &f.worktree(), f.decided + 1_000, "0.154.0");
    f.cli("collect");
    let usage = || f.cli_args(&["query", "--json", "--metric", "M08,M09"]).0["results"].as_array().unwrap()
        .iter().map(|r| (r["value"].clone(), r["detail"]["coverage"].clone())).collect::<Vec<_>>();
    let replayed = usage();
    assert_eq!(replayed[0].0, json!(1000));
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(usage(), replayed, "maintained aggregates preserve both totals and coverage");
    let first = f.cli_args(&["analytics", "refresh"]).0;
    let m08 = |out: &Value| out["appended"].as_array().unwrap().iter().find(|a| a["cell"]["metric"] == "M08").map(|a| a["revision"].as_i64().unwrap());
    let r1 = m08(&first).unwrap();
    let tail = fs::read_to_string(Path::new(FIXTURES).join("tail.jsonl")).unwrap();
    let ts = jiff::Timestamp::from_millisecond(f.decided + 1_000).unwrap().to_string();
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    std::io::Write::write_all(&mut file, tail.replace("@SID@", SID).replace("@CWD@", &f.worktree()).replace("@TS@", &ts).as_bytes()).unwrap();
    f.cli("collect");
    assert_eq!(usage()[0].0, json!(1500), "reads remain live before sync and refresh");
    f.cli_args(&["accounting", "sync"]);
    let r2 = m08(&f.cli_args(&["analytics", "refresh"]).0).expect("restated");
    let at = |seq: i64| f.cli_args(&["query", "--json", "--metric", "M08", "--as-of-seq", &seq.to_string()]).0["results"][0].clone();
    let (old, new) = (at(r1), at(r2));
    assert_eq!((&old["value"], &new["value"]), (&json!(1000), &json!(1500)));
    assert_eq!((&old["projection"]["restated"], &new["projection"]["supersedes"]), (&json!(true), &json!(r1)));
    assert_eq!(old["detail"]["definition"], "M08.slice-v1");
    assert!(old["lag_ms"].as_i64().unwrap() >= 0);
    assert!(new["source_watermarks"]["sidecar"]["codex_usage_rowid"].as_i64() > old["source_watermarks"]["sidecar"]["codex_usage_rowid"].as_i64());
    let live = f.cli_args(&["query", "--json", "--metric", "M08"]).0["results"][0].clone();
    assert_eq!((&live["value"], &live["projection"]["matches_revision"]), (&json!(1500), &json!(r2)));
    let before_rebuild = f.cli_args(&["analytics", "snapshot"]).1;
    let verify = f.cli_args(&["analytics", "rebuild", "--verify"]).0;
    assert_eq!(verify["identical"], true, "{verify}");
    assert!(verify["cells"].as_array().unwrap().iter().all(|c| c["stored_intact"] == true));
    f.cli_args(&["analytics", "rebuild"]);
    assert_eq!(f.cli_args(&["analytics", "snapshot"]).1, before_rebuild, "incremental restatement and full evaluation are byte-identical");
    let again = f.cli_args(&["analytics", "refresh"]).0;
    assert_eq!(again["appended"], json!([]));
    assert_eq!(again["unchanged"], again["cells"]);
}

/// Rebuilds reproduce revisions byte for byte: `rebuild --verify` on the same
/// sources, and a second project rebuilt from a copy of the canonical rows.
#[test]
fn rebuild_is_byte_identical() {
    let p = Planted::new();
    worked_example(&p);
    p.raw(&["collect"]);
    p.raw(&["analytics", "refresh", "--metric", "M02", "--cohort", "assignment_cohort", "--horizon-ms", "1200"]);
    let verify = p.json(&["analytics", "rebuild", "--verify"]);
    assert_eq!((&verify["identical"], &verify["appended"]), (&json!(true), &json!([])));
    assert!(verify["cells"].as_array().unwrap().iter().all(|c| c["stored_intact"] == true && c["stored_digest"] == c["rebuilt_digest"]), "{verify}");
    let rebuilt = p.json(&["analytics", "rebuild"]);
    assert_eq!(rebuilt["appended"], json!([]), "nothing to restate");
    let snapshot = p.raw(&["analytics", "snapshot"]);
    let cells: Value = serde_json::from_slice(&snapshot).unwrap();
    let tracked = cells["cells"].as_array().unwrap();
    let assignment = tracked.iter().find(|c| c["cell"]["cohort"] == "assignment_cohort").unwrap();
    assert_eq!((&assignment["body"]["value"], &assignment["body"]["censored"]), (&json!("1/5"), &json!({"unfinished": 4})));
    assert_eq!(assignment["lineage"]["outcome.unfinished"].as_array().unwrap().len(), 4);

    // A second project from the same canonical rows rebuilds the same bytes.
    let q = Planted::new();
    fs::copy(p.project.join(".state/state.db"), q.project.join(".state/state.db")).unwrap();
    q.raw(&["collect"]);
    q.raw(&["analytics", "refresh", "--metric", "M02", "--cohort", "assignment_cohort", "--horizon-ms", "1200"]);
    assert_eq!(String::from_utf8(q.raw(&["analytics", "snapshot"])).unwrap(), String::from_utf8(snapshot).unwrap());
}

/// Bounded, deterministic pagination: pages concatenate to the full list; a
/// live snapshot that changed is `restart_required`; a snapshot pinned to a
/// revision keeps paging the frozen rows despite new arrivals.
#[test]
fn drill_down_pagination_is_stable() {
    let p = Planted::new();
    worked_example(&p);
    let page = |cursor: Option<&str>| {
        let mut args = vec!["query", "--json", "--metric", "M07", "--drill", "numerator", "--page-size", "2"];
        if let Some(cursor) = cursor { args.extend(["--cursor", cursor]); }
        p.json(&args)["drill"].clone()
    };
    let ids = |d: &Value| d["rows"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap().to_owned()).collect::<Vec<_>>();
    let first = page(None);
    assert_eq!((ids(&first), &first["total"], &first["snapshot"]["revision"]), (vec!["t1-a1".to_owned(), "t2-a1".into()], &json!(5), &Value::Null));
    let second = page(first["next_cursor"].as_str());
    assert_eq!(ids(&second), ["t2-a2", "t4-a1"]);
    let third = page(second["next_cursor"].as_str());
    assert_eq!((ids(&third), &third["next_cursor"]), (vec!["t4-a2".to_owned()], &Value::Null));
    let full = p.json(&["query", "--json", "--metric", "M07", "--drill", "numerator", "--page-size", "500"])["drill"].clone();
    assert_eq!([ids(&first), ids(&second), ids(&third)].concat(), ids(&full));
    assert_eq!(full["rows"][0], json!({"entity": "attempt", "id": "t1-a1", "task_id": "t1", "state": "completed", "decided_unix_ms": null}));
    assert_eq!(full["buckets"]["denominator"], 2);

    // Bounds, tampering and a cursor from another request.
    assert!(p.fail(&["query", "--metric", "M07", "--drill", "numerator", "--page-size", "0"]).contains("page_size_out_of_range"));
    assert!(p.fail(&["query", "--metric", "M07", "--drill", "numerator", "--page-size", "501"]).contains("page_size_out_of_range"));
    assert!(p.fail(&["query", "--metric", "M07,M02", "--drill", "numerator"]).contains("drill_needs_one_metric"));
    let cursor = first["next_cursor"].as_str().unwrap();
    let tampered = format!("{}{}", &cursor[..cursor.len() - 1], if cursor.ends_with('0') { '1' } else { '0' });
    assert!(p.fail(&["query", "--metric", "M07", "--drill", "numerator", "--page-size", "2", "--cursor", &tampered]).contains("invalid_cursor"));
    assert!(p.fail(&["query", "--metric", "M07", "--drill", "numerator", "--page-size", "3", "--cursor", cursor]).contains("cursor_mismatch"));
    let lane = p.json(&["query", "--json", "--metric", "M08", "--drill", "numerator"])["drill"].clone();
    assert_eq!(lane["reason"], "drill_unsupported");

    // Unpinned live snapshot: a new attempt between pages means restart, never a mixed page.
    let db = p.db();
    db.execute("INSERT INTO attempts(id,task_id,revision,state,reservation,termination_observed) VALUES('t4-a3','t4',2,'failed','t4-a3',1)", []).unwrap();
    assert!(p.fail(&["query", "--metric", "M07", "--drill", "numerator", "--page-size", "2", "--cursor", cursor]).contains("restart_required"));

    // Pinned: the first page matches a recorded revision, so later pages read it.
    p.raw(&["collect"]);
    p.raw(&["analytics", "refresh"]);
    let first = page(None);
    let pinned = first["snapshot"]["revision"].as_i64().expect("pinned to the recorded revision");
    assert_eq!((ids(&first), &first["total"]), (vec!["t1-a1".to_owned(), "t2-a1".into()], &json!(6)));
    db.execute("INSERT INTO attempts(id,task_id,revision,state,reservation,termination_observed) VALUES('t4-a4','t4',2,'failed','t4-a4',1)", []).unwrap();
    let mut rest = Vec::new();
    let mut next = first["next_cursor"].as_str().map(str::to_owned);
    while let Some(cursor) = next {
        let d = page(Some(&cursor));
        assert_eq!(d["snapshot"]["revision"], pinned);
        rest.extend(ids(&d));
        next = d["next_cursor"].as_str().map(str::to_owned);
    }
    assert_eq!(rest, ["t2-a2", "t4-a1", "t4-a2", "t4-a3"], "the frozen snapshot: no duplicate, no skip, no new arrival");
    assert_eq!(p.query(&["--metric", "M07"])["numerator"], 7, "the live answer moved on");
}

/// Indexed plans: every analytics read is an index search; canonical reads scan
/// only the table they aggregate; other streams' missing indexes are described.
#[test]
fn hot_queries_use_indexes() {
    let p = Planted::new();
    worked_example(&p);
    p.raw(&["collect"]);
    p.raw(&["analytics", "refresh"]);
    let plans = p.json(&["analytics", "plans"]);
    let all = plans["plans"].as_array().unwrap();
    for name in ["as_of_seq", "as_of_time", "latest_revision", "next_revision", "lineage_page", "lineage_buckets", "canonical_head"] {
        let plan = all.iter().find(|q| q["name"] == name).unwrap_or_else(|| panic!("{name}: {plans}"));
        assert_eq!((&plan["verdict"], &plan["scans"]), (&json!("indexed"), &json!([])), "{plan}");
    }
    for name in ["lifecycle_attempts", "lifecycle_contracts", "lifecycle_classes", "lifecycle_replay_candidates"] {
        let plan = all.iter().find(|q| q["name"] == name).unwrap();
        assert_eq!((&plan["verdict"], &plan["unexpected_scans"]), (&json!("full_scan_inherent"), &json!([])), "{plan}");
    }
    // Correlated lookups never scan; a missing persistent index in another
    // owner's store is described with its DDL, never created here.
    for name in ["first_candidate_submissions", "first_candidate_policies", "first_candidate_verdicts"] {
        let plan = all.iter().find(|q| q["name"] == name).unwrap();
        assert_eq!(plan["unexpected_scans"], json!([]), "{plan}");
        assert_eq!(plan["automatic_indexes"], json!([]), "{plan}");
    }
    let acceptance = all.iter().find(|q| q["name"] == "lifecycle_acceptance_times").unwrap();
    assert_eq!(acceptance["unexpected_scans"], json!([]), "{acceptance}");
    for plan in all.iter().filter(|q| q["verdict"] == "needs_index") {
        assert!(!plan["owner"].as_str().unwrap().starts_with("analytics") && plan["proposed_index"].is_string(), "{plan}");
    }
}

/// The stable read contract: every report metric's body is the query's
/// `detail` for the same definition, and `report` output is unchanged by reads.
#[test]
fn report_and_query_share_one_read_path() {
    let p = Planted::new();
    worked_example(&p);
    p.raw(&["collect"]);
    let report = p.json(&["report", "--json"]);
    let definitions: Vec<String> = report["metrics"].as_object().unwrap().values().map(|m| m["definition"].as_str().unwrap().to_owned()).collect();
    let out = p.json(&["query", "--json", "--metric", &definitions.join(",")]);
    for result in out["results"].as_array().unwrap() {
        let id = result["metric_id"].as_str().unwrap();
        assert_eq!(result["detail"], report["metrics"][id], "{id}");
        assert_eq!(result["definition"], report["metrics"][id]["definition"], "{id}");
        for key in ["projection", "source_watermarks", "coverage", "exclusions", "observation_cutoff_unix_ms", "certification"] {
            assert!(result.get(key).is_some(), "{id} {key}");
        }
    }
    let priced = out["results"].as_array().unwrap().iter().find(|r| r["metric_id"] == "M12").unwrap();
    assert_eq!(priced["rate_card_revision"], json!({"status": "unavailable", "reason": "not_priced"}));
    assert_eq!(p.json(&["report", "--json"]), report, "a query writes nothing a report reads");
}

/// A schema change after installation must cause open/refresh to install new
/// input triggers even when every stream is already current. Exercise both
/// creation and a later mutation, whose schema version does not change.
#[test]
fn refresh_tracks_tables_created_after_input_installation() {
    let f = Fixture::new();
    f.cli_args(&["collect"]);
    f.cli_args(&["accounting", "sync"]);
    f.cli_args(&["analytics", "refresh"]);
    let sidecar = f.project.join(".state/telemetry.db");
    let db = rusqlite::Connection::open(&sidecar).unwrap();
    db.execute_batch("CREATE TABLE codex_tool_future(id INTEGER PRIMARY KEY, value TEXT NOT NULL) STRICT").unwrap();
    drop(db);
    let first = f.cli_args(&["analytics", "refresh"]).0;
    assert!(first["evaluated"].as_array().unwrap().iter().any(|c| c["metric"] == "M16"), "{first}");
    let tool_cell = |snapshot: Value| snapshot["cells"].as_array().unwrap().iter().find(|c| c["cell"]["metric"] == "M16").unwrap().clone();
    let before = tool_cell(f.cli_args(&["analytics", "snapshot"]).0);
    let db = rusqlite::Connection::open(&sidecar).unwrap();
    db.execute("INSERT INTO codex_tool_future VALUES(1,'new observation')", []).unwrap();
    drop(db);
    let changed = f.cli_args(&["analytics", "refresh"]).0;
    assert!(changed["evaluated"].as_array().unwrap().iter().any(|c| c["metric"] == "M16"), "new table mutation was missed: {changed}");
    assert!(changed["deferred"].as_array().unwrap().is_empty());
    // The conservative dependency has no defined contribution to M16 yet:
    // re-evaluation preserves all metric bodies, digests and lineage.
    assert_eq!(tool_cell(f.cli_args(&["analytics", "snapshot"]).0), before);
    let settled = f.cli_args(&["analytics", "refresh"]).0;
    assert!(!settled["evaluated"].as_array().unwrap().iter().any(|c| c["metric"] == "M16"), "{settled}");

    // A deterministic writer fixture mutates an input after the read snapshot
    // was captured, as the earlier M08 cell commits its checked timestamp.
    // Later cells must defer rather than mark the old snapshot checked.
    let db = rusqlite::Connection::open(&sidecar).unwrap();
    let checked = || db.query_row("SELECT i.inputs FROM analytics_checked_inputs i JOIN analytics_cells c USING(cell) WHERE c.metric='M16'", [], |r| r.get::<_, String>(0)).unwrap();
    let checked_before = checked();
    db.execute_batch("CREATE TRIGGER fixture_late_input AFTER UPDATE OF checked_unix_ms ON analytics_cells
        WHEN NEW.metric='M08' BEGIN INSERT OR REPLACE INTO codex_tool_future VALUES(2,'late observation'); END").unwrap();
    let racing = f.cli_args(&["analytics", "refresh", "--metric", "M16", "--from", "0"]).0;
    assert!(racing["deferred"].as_array().unwrap().iter().any(|c| c["metric"] == "M16"), "{racing}");
    assert_eq!(racing["comparison_deferred"], true);
    assert_eq!(db.query_row("SELECT count(*) FROM analytics_cells WHERE metric='M16' AND window_from_unix_ms=0 AND checked_unix_ms IS NULL", [], |r| r.get::<_, i64>(0)).unwrap(), 1,
        "a deferred new request stays tracked for the next refresh");
    assert_eq!(checked(), checked_before, "stale evaluation must leave its checked inputs untouched");
    assert_eq!(tool_cell(f.cli_args(&["analytics", "snapshot"]).0), before);
    db.execute_batch("DROP TRIGGER fixture_late_input").unwrap();
    let resumed = f.cli_args(&["analytics", "refresh"]).0;
    assert!(resumed["deferred"].as_array().unwrap().is_empty(), "{resumed}");
    assert!(resumed["evaluated"].as_array().unwrap().iter().any(|c| c["metric"] == "M16"), "{resumed}");
    assert_eq!(db.query_row("SELECT count(*) FROM analytics_cells WHERE metric='M16' AND window_from_unix_ms=0 AND checked_unix_ms IS NOT NULL", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
}

/// DG1: first submission is frozen across retries; only adjudicated first
/// candidates divide the rate. All reads exercise the public CLI.
#[test]
fn first_candidate_verification_submission_cohort() {
    let p = Planted::new();
    let db = p.db();
    let a = format!("sha256:{}", hex("arm-a"));
    let b = format!("sha256:{}", hex("arm-b"));
    for arm in [&a, &b] {
        db.execute("INSERT INTO agent_configurations VALUES(?1,'{}',1)", [arm]).unwrap();
    }
    for (task, arm) in [("clean", &a), ("retry", &b), ("pending", &a), ("partial", &a)] {
        let attempt = format!("{task}-a1");
        plant(&db, task, "running", Some("verify_only"), &[(&attempt, "completed", &[("reserved", 900)])], None, None);
        for policy in ["ci", "review"] {
            db.execute("INSERT INTO acceptance_policies VALUES(?1,1,?2,?2)", [task, policy]).unwrap();
        }
        db.execute("INSERT INTO dispatch_decisions(attempt_id,task_id,task_revision,contract_revision,chosen_configuration_id,eligible,chooser_kind,chooser_principal,reason_codes,decided_unix_ms)
            VALUES(?1,?2,1,1,?3,?4,'operator','fixture','[\"unspecified\"]',900)",
            rusqlite::params![attempt, task, arm, json!([{"configuration_id":arm,"probability_ppm":1000000}]).to_string()]).unwrap();
        db.execute("INSERT INTO task_classifications(classification_id,task_id,contract_revision,taxonomy,class,band,features,classifier,revision,reason,created_unix_ms)
            VALUES(?1,?2,1,'taxonomy.v1',?3,'small','{}','rule:fixture',1,NULL,800)",
            rusqlite::params![format!("sha256:{}", hex(task)), task, if task == "retry" { "docs" } else { "code" }]).unwrap();
        first_candidate_submission(&db, task, &attempt, "first", 1000);
    }
    candidate_verdict(&db, "clean", "first", "ci", true, 3000);
    candidate_verdict(&db, "clean", "first", "review", true, 3001);
    candidate_verdict(&db, "retry", "first", "ci", false, 1500);
    // One accepted required policy is still pending, never a success.
    candidate_verdict(&db, "partial", "first", "ci", true, 1600);
    // A later candidate cannot erase acceptance of the original first one.
    first_candidate_submission(&db, "clean", "clean-a1", "second", 1800);
    // A later successful candidate on a different configuration cannot replace
    // the first rejection or transfer that task's comparison arm.
    db.execute("INSERT INTO attempts(id,task_id,revision,state,reservation,termination_observed) VALUES('retry-a2','retry',2,'completed','retry-a2',1)", []).unwrap();
    db.execute("INSERT INTO dispatch_decisions(attempt_id,task_id,task_revision,contract_revision,chosen_configuration_id,eligible,chooser_kind,chooser_principal,reason_codes,decided_unix_ms)
        VALUES('retry-a2','retry',1,1,?1,?2,'operator','fixture','[\"unspecified\"]',1700)",
        rusqlite::params![a, json!([{"configuration_id":a,"probability_ppm":1000000}]).to_string()]).unwrap();
    first_candidate_submission(&db, "retry", "retry-a2", "second", 1800);
    candidate_verdict(&db, "retry", "second", "ci", true, 1900);
    candidate_verdict(&db, "retry", "second", "review", true, 1901);
    drop(db);
    let m = p.query(&["--metric", "M30", "--from", "1000", "--to", "1001"]);
    assert_eq!(nd(&m), (json!(1), json!(2), json!("1/2")));
    assert_eq!(m["pending"], 2);
    assert_eq!(m["coverage"], json!({"state":"partial","known":2,"expected":4,"missing":2,"reasons":{"pending":2,"policy_unknown":0}}));
    assert_eq!(m["time_basis"], "first_submission_time");
    let policy = p.query(&["--metric", "M30", "--by", "policy"]);
    assert_eq!(nd(&policy["cells"][0]), (json!(1), json!(2), json!("1/2")));
    assert!(policy["cells"][0]["dimension"]["policy"].as_str().unwrap().starts_with("ci,review@sha256:"));
    let class = p.query(&["--metric", "M30", "--by", "task_class"]);
    assert_eq!(class["cells"], json!([
        {"dimension":{"task_class":"code"},"numerator":1,"denominator":1,"value":"1/1","reason":null},
        {"dimension":{"task_class":"docs"},"numerator":0,"denominator":1,"value":"0/1","reason":null}
    ]));
    let compare = p.json(&["compare", "--json", "--metric", "M30", "--by", "configuration", "--from", "1000", "--to", "1001"]);
    let arms = compare["results"][0]["all_classes"]["arms"].as_array().unwrap();
    let arm = |id: &str| arms.iter().find(|v| v["configuration_id"] == id).unwrap();
    assert_eq!((&arm(&a)["numerator"], &arm(&a)["denominator"]), (&json!(1), &json!(1)));
    assert_eq!((&arm(&b)["numerator"], &arm(&b)["denominator"]), (&json!(0), &json!(1)));
    assert_eq!(arm(&a)["value"]["reason"], "insufficient_data");
    assert_eq!(compare["population"]["exclusions"]["pending"], 2);
    let empty = p.query(&["--metric", "M30", "--from", "1001", "--to", "1801"]);
    assert_eq!(nd(&empty), (json!(0), json!(0), Value::Null));
    assert_eq!(empty["reason"], "empty_denominator");
    assert_eq!(p.query(&["--metric", "M30.v1"])["reason"], "no_producer", "old absent definition stays servable");
    let m02 = p.query(&["--metric", "M02"]);
    let m30 = p.query(&["--metric", "M30"]);
    for metrics in ["M02,M30", "M30,M02"] {
        let mixed = p.json(&["query", "--json", "--metric", metrics]);
        for (id, expected) in [("M02", &m02), ("M30", &m30)] {
            let actual = mixed["results"].as_array().unwrap().iter().find(|r| r["metric_id"] == id).unwrap();
            for key in ["value", "numerator", "denominator", "coverage", "exclusions", "content_digest", "source_watermarks"] {
                assert_eq!(actual[key], expected[key], "{metrics}: {id}.{key}");
            }
        }
    }
    let report = p.json(&["report", "--json"]);
    assert_eq!(report["metrics"]["M30"]["value"], "1/2");
    assert_eq!(p.query(&["--metric", "M30"])["detail"], report["metrics"]["M30"]);
    let exported = p.json(&["export", "--metric", "M30"]);
    assert_eq!(exported["metrics"][0]["value"], "1/2", "{exported}");
    p.raw(&["collect"]);
    let refreshed = p.json(&["analytics", "refresh", "--metric", "M30"]);
    let revision = refreshed["appended"].as_array().unwrap().iter().find(|v| v["cell"]["metric"] == "M30").unwrap()["revision"].as_i64().unwrap();
    assert_eq!(nd(&p.query(&["--metric", "M30", "--as-of-seq", &revision.to_string()])), (json!(1), json!(2), json!("1/2")));
    assert!(p.json(&["analytics", "rebuild", "--verify"])["identical"].as_bool().unwrap());
    let cached_report = p.json(&["report", "--json"]);
    assert_eq!(cached_report["metrics"]["M30"], report["metrics"]["M30"]);
    let db = p.db();
    candidate_verdict(&db, "pending", "first", "ci", true, 4000);
    candidate_verdict(&db, "pending", "first", "review", true, 4001);
    drop(db);
    assert_eq!(nd(&p.query(&["--metric", "M30"])), (json!(2), json!(3), json!("2/3")));
    assert_eq!(p.json(&["report", "--json"])["metrics"]["M30"]["value"], "2/3", "canonical changes invalidate maintained report bodies");
    let restated = p.json(&["analytics", "refresh", "--metric", "M30"]);
    let cell = restated["appended"].as_array().unwrap().iter().find(|v| v["cell"]["metric"] == "M30").unwrap();
    assert_eq!(cell["kind"], "restatement");
    assert_eq!(cell["supersedes"], revision);
    assert_eq!(nd(&p.query(&["--metric", "M30", "--as-of-seq", &revision.to_string()])), (json!(1), json!(2), json!("1/2")));
    assert!(p.json(&["analytics", "rebuild", "--verify"])["identical"].as_bool().unwrap());
}

fn first_candidate_submission(db: &rusqlite::Connection, task: &str, attempt: &str, candidate: &str, at: i64) {
    let submission = hex(&format!("{task}-{candidate}"));
    db.execute("INSERT INTO result_submissions(submission_id,project_store,idempotency_key,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,artifact_manifest,claimed_checks,created_unix_ms)
        VALUES(?1,'store',?1,?2,'{}',?3,1,?2,?4,'/repo',?5,?5,'sha1','[]','[]',?6)", rusqlite::params![submission, hex("d"), task, attempt, OID, at]).unwrap();
}

fn candidate_verdict(db: &rusqlite::Connection, task: &str, candidate: &str, policy: &str, accepted: bool, at: i64) {
    let submission = hex(&format!("{task}-{candidate}"));
    let run = hex(&format!("{task}-{candidate}-{policy}"));
    let digest = hex(policy);
    db.execute("INSERT INTO verification_runs(run_id,project_store,idempotency_key,payload_digest,submission_id,task_id,contract_revision,contract_digest,attempt_id,policy_id,policy_digest,commit_oid,tree_oid,object_format,memory_fence,isolation,argv,library_manifest,state,reason,exit_status,receipt_digest,store_device,store_inode,created_unix_ms)
        VALUES(?1,'store',?1,?2,?3,?4,1,?2,?5,?6,?7,?8,?8,'sha1',0,'linux-unshare-user-pid-mount-v1','[]','[]',?9,?10,?11,?12,1,1,?13)",
        rusqlite::params![run, hex("d"), submission, task, if candidate == "first" { format!("{task}-a1") } else { format!("{task}-a2") }, policy, digest, OID,
            if accepted { "accepted" } else { "rejected" }, if accepted { None } else { Some("check_failed") }, if accepted { 0 } else { 1 }, accepted.then_some(&digest), at]).unwrap();
    if accepted {
        db.execute("INSERT INTO verified_results(result_id,run_id,submission_id,commit_oid,tree_oid,object_format,policy_digest,receipt_digest,isolation,memory_fence,created_unix_ms)
            VALUES(?1,?1,?2,?3,?3,'sha1',?4,?4,'linux-unshare-user-pid-mount-v1',0,?5)", rusqlite::params![run, submission, OID, digest, at]).unwrap();
    }
}

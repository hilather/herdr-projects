//! TM4.5 health alerts and advisory recommendations end to end
//! (docs/telemetry/contracts-health.md), through `herdr-projects telemetry`
//! on the CLI over planted canonical and sidecar rows. Every expected value is
//! hand-computed from the planted rows and the declared rule table
//! (`health-rules.v1`); none is read back from a production aggregate.

#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.

mod support;

use herdr_projects::store::SqliteStore;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, path::{Path, PathBuf}, process::Command};
use support::telemetry::*;

fn hex(seed: &str) -> String { format!("{:x}", Sha256::digest(seed.as_bytes())) }
const OID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const MINUTE: i64 = 60_000;
const HOUR: i64 = 60 * MINUTE;

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
    fn sidecar(&self) -> rusqlite::Connection { rusqlite::Connection::open(self.project.join(".state/telemetry.db")).unwrap() }
    fn config_dir(&self) -> PathBuf { self.tmp.path().join("home/.config/herdr-projects") }
    fn command(&self, args: &[&str]) -> std::process::Output {
        Command::new(BIN).env_clear().env("HOME", self.tmp.path().join("home")).env("PATH", "/usr/bin:/bin")
            .args(["--root", self.root.to_str().unwrap(), "telemetry", "demo"]).args(args).output().unwrap()
    }
    fn raw(&self, args: &[&str]) -> Vec<u8> {
        let out = self.command(args);
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        out.stdout
    }
    fn json(&self, args: &[&str]) -> Value { serde_json::from_slice(&self.raw(args)).unwrap_or_else(|e| panic!("{args:?}: {e}")) }
    fn fail(&self, args: &[&str]) -> String {
        let out = self.command(args);
        assert!(!out.status.success(), "{args:?} succeeded: {}", String::from_utf8_lossy(&out.stdout));
        String::from_utf8(out.stderr).unwrap()
    }
    /// `telemetry demo collect` with no execution home: creates and migrates the sidecar, collects nothing.
    fn sidecar_created(&self) { self.raw(&["collect"]); assert!(self.project.join(".state/telemetry.db").is_file()); }
    fn health(&self) -> Value { self.json(&["health", "--json"]) }
    fn evaluate(&self) -> Value { self.json(&["health", "evaluate", "--json"]) }
    fn alerts(&self) -> Value { self.json(&["health", "alerts", "--json"]) }
    /// Every file under `.state` with its sha256: the canonical store and anything beside it.
    fn state_files(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = fs::read_dir(self.project.join(".state")).unwrap().flatten()
            .filter(|e| e.path().is_file())
            .map(|e| (e.file_name().to_string_lossy().into_owned(), format!("{:x}", Sha256::digest(fs::read(e.path()).unwrap())))).collect();
        out.sort();
        out
    }
    fn canonical_bytes(&self) -> Vec<(String, String)> { self.state_files().into_iter().filter(|(n, _)| n.starts_with("state.db")).collect() }
}

/// The state of `rule` (and role) in a `health` or `health evaluate` output.
fn state<'a>(out: &'a Value, rule: &str) -> &'a Value {
    out["states"].as_array().unwrap().iter().find(|s| s["rule"] == rule).unwrap_or_else(|| panic!("no {rule} in {out}"))
}
fn codes(v: &Value) -> Vec<String> { v["reasons"].as_array().unwrap().iter().map(|r| r["code"].as_str().unwrap().to_owned()).collect() }
fn open_alert<'a>(alerts: &'a Value, rule: &str) -> Option<&'a Value> { alerts["open"].as_array().unwrap().iter().find(|a| a["rule"] == rule) }

/// Plant an `agent_configuration.v1` row (contracts §2).
fn configuration(db: &rusqlite::Connection, kind: &str, version: &str) -> String {
    let canonical = json!({"adapter": {"digest": "a".repeat(64), "id": "sim-adapter", "revision": 1}, "agent_digest": "e".repeat(64), "agent_version": version,
        "arguments_digest": "c".repeat(64), "definition_digest": "b".repeat(64), "environment_names": ["SIM_ENV"], "kind": kind,
        "permission_policy": {"digest": "d".repeat(64), "id": "sim-policy", "revision": 1}, "reasoning_effort": null, "reasoning_effort_reason": "mapping_unverified",
        "requested_model": null, "requested_model_reason": "mapping_unverified", "schema": "agent_configuration.v1"}).to_string();
    let id = format!("sha256:{}", hex(&canonical));
    db.execute("INSERT INTO agent_configurations(configuration_id,canonical_json,first_decided_unix_ms) VALUES(?1,?2,1)", [&id, &canonical]).unwrap();
    id
}

/// One attempt of a planted task: its configuration, state and effective profile `(name, kind)`.
struct Attempt<'a> { configuration: &'a str, state: &'a str, profile: (&'a str, &'a str) }

/// Plant task `id` (verify_only contract, class `class`, band `small`) with
/// its attempts, lifecycle marks, dispatch decisions and attempt inputs;
/// `accepted` adds a submission with a verified result.
fn task(db: &rusqlite::Connection, id: &str, outcome: &str, class: &str, attempts: &[Attempt], t0: i64) {
    let state = match outcome { "accepted" => "succeeded", "open" => "running", other => other };
    db.execute("INSERT INTO tasks(id,revision,state,title) VALUES(?1,1,?2,?1)", [id, state]).unwrap();
    db.execute("INSERT INTO task_contracts(task_id,contract_revision,project_store,expected_head,repository,base_oid,object_format,route,raw_bytes,raw_digest,installed_seq)
        VALUES(?1,1,'store',1,'/repo',?2,'sha1','verify_only',x'7b7d',?3,1)", rusqlite::params![id, OID, hex(&format!("contract-{id}"))]).unwrap();
    let classification = format!("sha256:{}", hex(&format!("class-{id}")));
    db.execute("INSERT INTO task_classifications(classification_id,task_id,contract_revision,taxonomy,class,band,features,classifier,revision,reason,created_unix_ms)
        VALUES(?1,?2,1,'taxonomy.v1',?3,'small','{}','rule:fixture',1,NULL,?4)", rusqlite::params![classification, id, class, t0]).unwrap();
    let mut last = String::new();
    for (i, a) in attempts.iter().enumerate() {
        let attempt = format!("{id}-a{i}");
        let at = t0 + 10 * i as i64;
        db.execute("INSERT INTO attempts(id,task_id,revision,state,reservation,termination_observed) VALUES(?1,?2,2,?3,?1,?4)",
            rusqlite::params![attempt, id, a.state, i64::from(a.state != "running")]).unwrap();
        db.execute("INSERT INTO attempt_lifecycle(attempt_id,state,attempt_revision,unix_ms,source) VALUES(?1,'reserved',1,?2,'fixture')", rusqlite::params![attempt, at]).unwrap();
        if a.state != "running" {
            db.execute("INSERT INTO attempt_lifecycle(attempt_id,state,attempt_revision,unix_ms,source) VALUES(?1,?2,2,?3,'fixture')", rusqlite::params![attempt, a.state, at + 5]).unwrap();
        }
        let eligible = json!([{"configuration_id": a.configuration, "profile_digest": "0".repeat(64), "status": "chosen", "probability_ppm": 1_000_000}]).to_string();
        db.execute("INSERT INTO dispatch_decisions(attempt_id,task_id,task_revision,contract_revision,classification_id,chosen_configuration_id,eligible,chooser_kind,chooser_principal,reason_codes,note,policy,seed,decided_unix_ms)
            VALUES(?1,?2,1,1,?3,?4,?5,'operator','approval:fixture','[\"unspecified\"]',NULL,NULL,NULL,?6)", rusqlite::params![attempt, id, classification, a.configuration, eligible, at]).unwrap();
        let inputs = json!({"inputs": {"version": 2, "effective_profile": {"name": a.profile.0, "kind": a.profile.1}}}).to_string();
        db.execute("INSERT INTO attempt_inputs(attempt_id,operation_id,payload,payload_hash) VALUES(?1,?2,?3,?4)",
            rusqlite::params![attempt, format!("op-{attempt}"), inputs, hex(&inputs)]).unwrap();
        last = attempt;
    }
    if outcome == "accepted" {
        let (submission, result) = (hex(&format!("submission-{id}")), hex(&format!("result-{id}")));
        db.execute("INSERT INTO result_submissions(submission_id,project_store,idempotency_key,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,artifact_manifest,claimed_checks,created_unix_ms)
            VALUES(?1,'store',?1,?2,'{}',?3,1,?2,?4,'/repo',?5,?5,'sha1','[]','[]',?6)", rusqlite::params![submission, hex("d"), id, last, OID, t0 + 90]).unwrap();
        db.execute("INSERT INTO verified_results(result_id,run_id,submission_id,commit_oid,tree_oid,object_format,policy_digest,receipt_digest,isolation,memory_fence,created_unix_ms)
            VALUES(?1,?1,?2,?3,?3,'sha1',?4,?4,'linux-unshare-user-pid-mount-v1',0,?5)", rusqlite::params![result, submission, OID, hex("e"), t0 + 100]).unwrap();
    }
}

/// Bound, certified Codex usage for `attempt` (one accepted record of one rollout).
fn usage(sidecar: &rusqlite::Connection, attempt: &str, session: &str, total: i64) {
    let path = hex(&format!("path-{session}"));
    sidecar.execute("INSERT INTO rollout_sources(path_digest,home_digest,session_id,session_unix_ms,cwd,cwd_attempt,cli_version,originator,source,records,thread_usage,token_count_usage,binding,attempt_id,observed_unix_ms)
        VALUES(?1,?2,?3,1,'~/w',NULL,'0.154.0',NULL,NULL,1,NULL,NULL,'bound',?4,1)", rusqlite::params![path, hex("home"), session, attempt]).unwrap();
    sidecar.execute("INSERT INTO codex_usage(session_id,ordinal,path_digest,response_id,turn_id,model,effort,payload_digest,input_tokens,cached_input_tokens,cache_write_input_tokens,output_tokens,reasoning_output_tokens,total_tokens,accepted,reason,observed_unix_ms)
        VALUES(?1,1,?2,NULL,NULL,NULL,NULL,?3,?4,0,0,0,0,?4,1,NULL,1)", rusqlite::params![session, path, hex(&format!("payload-{session}")), total]).unwrap();
}

/// A synced ledger (`accounting sync` ran) with one current quota window of `remaining` percent.
fn quota_window(sidecar: &rusqlite::Connection, remaining: &str, resets: i64, last_observed: i64) {
    sidecar.execute_batch("INSERT OR IGNORE INTO usage_ledger(singleton,normalization_version,synced_unix_ms) VALUES(1,'codex-v1',1); DELETE FROM quota_windows;").unwrap();
    let used = format!("{}", 100.0 - remaining.parse::<f64>().unwrap());
    sidecar.execute("INSERT INTO quota_windows(window_id,service,account,limit_id,window_kind,unit,window_minutes,window_start_unix_ms,resets_unix_ms,start_evidence,
        first_observed_unix_ms,last_observed_unix_ms,first_used,used,remaining,observed_increase,plan_type,observations,flagged)
        VALUES(?1,'codex',?2,'codex','primary','percent',300,?3,?4,'first_observation',?5,?5,'0',?6,?7,?6,'pro',1,0)",
        rusqlite::params![format!("codex:acct:codex:primary:{resets}"), format!("sha256:{}", hex("acct")), resets - 300 * MINUTE, resets, last_observed, used, remaining]).unwrap();
}

// Outages and missing data: unknown with a reason, never ok, never a zero

/// No sidecar: every sidecar-backed rule is `unknown collection_not_run` with
/// an unavailable value (never 0) and `health evaluate` records nothing and
/// creates no sidecar. With a sidecar but no collect, the collector is
/// `unknown no_collect_recorded`; a last collect 20 minutes ago is `warn`
/// (15 min ≤ 20 min < 60 min) and opens one alert, while M08 stays
/// unavailable, never 0 tokens. When the collect record disappears again (an
/// outage) the open alert stays open as `unknown`, never resolved as ok.
#[test]
fn stale_collector_and_missing_sources_are_health_states_never_zero_usage() {
    let p = Planted::new();
    let out = p.health();
    assert_eq!((&out["contract"], &out["rules_version"], &out["project"]), (&json!("telemetry-health.v1"), &json!("health-rules.v1"), &json!("demo")));
    for rule in ["collector_stale", "usage_coverage", "cost_coverage", "accounting_conflict", "quota_headroom", "waiting_on_you"] {
        let s = state(&out, rule);
        assert_eq!((&s["state"], codes(s)), (&json!("unknown"), vec!["collection_not_run".to_owned()]), "{rule}: {s}");
        assert_eq!(s["evidence"]["value"], json!({"status": "unavailable", "reason": "collection_not_run"}), "{rule}");
    }
    assert_eq!(state(&out, "service_throttled")["state"], "unknown");
    assert_eq!(out["alerts"], json!({"status": "unavailable", "reason": "collection_not_run"}));
    assert!(out["states"].as_array().unwrap().iter().all(|s| s["labels"].as_object().unwrap().keys().all(|k| ["project", "family", "rule", "service", "role"].contains(&k.as_str()))));
    let evaluated = p.evaluate();
    assert_eq!(evaluated["recorded"], json!({"status": "unavailable", "reason": "collection_not_run"}));
    assert!(!p.project.join(".state/telemetry.db").exists(), "an evaluation without a sidecar creates none");

    p.sidecar_created();
    let s = state(&p.health(), "collector_stale").clone();
    assert_eq!((&s["state"], codes(&s)), (&json!("unknown"), vec!["no_collect_recorded".to_owned()]));

    let at = unix_ms() - 20 * MINUTE;
    p.sidecar().execute("INSERT INTO collect_offsets(path_digest,device,inode,byte_offset,records,rate_limits,model,effort,updated_unix_ms) VALUES('p',1,1,0,0,0,NULL,NULL,?1)", [at]).unwrap();
    let evaluated = p.evaluate();
    let s = state(&evaluated, "collector_stale");
    assert_eq!((&s["state"], codes(s)), (&json!("warn"), vec!["collector_stale".to_owned()]));
    let age = s["evidence"]["age_ms"].as_i64().unwrap();
    assert!((20 * MINUTE..21 * MINUTE).contains(&age), "{age}");
    assert_eq!(s["evidence"]["last_collect_unix_ms"], json!(at));
    assert_eq!(s["labels"], json!({"project": "demo", "family": "collection", "rule": "collector_stale", "service": "codex"}));
    assert_eq!(s["thresholds"]["warn"], json!(15 * MINUTE));
    let m08 = &p.json(&["query", "--json", "--metric", "M08"])["results"][0];
    assert_eq!((&m08["status"], &m08["value"]), (&json!("unavailable"), &json!({"status": "unavailable", "reason": "no_certified_source"})), "never 0 tokens");
    let opened: Vec<&Value> = evaluated["opened"].as_array().unwrap().iter().filter(|a| a["rule"] == "collector_stale").collect();
    assert_eq!(opened.len(), 1, "{evaluated}");
    // Unknown rules that never had a source open no alert.
    assert!(evaluated["opened"].as_array().unwrap().iter().all(|a| a["state"] != "unknown"), "{evaluated}");

    p.sidecar().execute("DELETE FROM collect_offsets", []).unwrap();
    let evaluated = p.evaluate();
    assert_eq!(state(&evaluated, "collector_stale")["state"], "unknown");
    let alert = open_alert(&p.alerts(), "collector_stale").unwrap().clone();
    assert_eq!((&alert["state"], &alert["occurrences"], &alert["status"]), (&json!("unknown"), &json!(2), &json!("open")), "an outage never resolves the alert: {alert}");
    assert_eq!(alert["reasons"], json!([{"code": "no_collect_recorded"}]));
}

/// Two terminated Codex attempts, one with complete bound usage and one never
/// bound: M13 = 1/2, below 100% (warn) and not below 50% (critical needs
/// 1·1000 < 500·2, false): `warn coverage_loss`. With the bound one gone as
/// well M13 = 0/2: critical.
#[test]
fn coverage_loss_grades_usage_coverage_exactly() {
    let p = Planted::new();
    let config = configuration(&p.db(), "codex", "0.154.0");
    let codex = |state| Attempt { configuration: &config, state, profile: ("coder", "codex") };
    task(&p.db(), "t1", "failed", "code", &[codex("failed")], 1_000);
    task(&p.db(), "t2", "failed", "code", &[codex("failed")], 2_000);
    p.sidecar_created();
    usage(&p.sidecar(), "t1-a0", "s1", 100);
    let out = p.evaluate();
    let s = state(&out, "usage_coverage");
    assert_eq!((&s["state"], &s["evidence"]["value"], &s["evidence"]["numerator"], &s["evidence"]["denominator"]), (&json!("warn"), &json!("1/2"), &json!(1), &json!(2)), "{s}");
    assert_eq!(s["evidence"]["incomplete"], json!({"not_bound": 1}));
    assert_eq!(codes(s), vec!["coverage_loss"]);
    assert_eq!(s["metric"]["definition"], "M13.slice-v1");
    assert_eq!(s["metric"]["registry"], "analytics-registry.v1");

    p.sidecar().execute_batch("DELETE FROM codex_usage; DELETE FROM rollout_sources;").unwrap();
    let out = p.evaluate();
    assert_eq!((&state(&out, "usage_coverage")["state"], &state(&out, "usage_coverage")["evidence"]["value"]), (&json!("critical"), &json!("0/2")));
    let alert = open_alert(&p.alerts(), "usage_coverage").unwrap().clone();
    assert_eq!((&alert["state"], &alert["occurrences"]), (&json!("critical"), &json!(2)), "warn escalated to critical in the same alert: {alert}");
}

/// Ledger dispositions: one `unresolved` (regression_without_reset) is warn,
/// a `conflict` (payload_digest_mismatch) makes it critical; counts by reason.
#[test]
fn unresolved_accounting_discrepancy_stays_open_until_resolved() {
    let p = Planted::new();
    p.sidecar_created();
    let s = state(&p.health(), "accounting_conflict").clone();
    assert_eq!((&s["state"], codes(&s)), (&json!("unknown"), vec!["ledger_not_synced".to_owned()]));
    let entry = |db: &rusqlite::Connection, id: &str, disposition: &str, reason: &str| {
        db.execute("INSERT INTO usage_entries(entry_id,source,session_id,basis,scope,normalization_version,precedence,position,native) VALUES(?1,'codex','s1','cumulative','thread','codex-v1',2,1,'{}')", [id]).unwrap();
        db.execute("INSERT INTO usage_dispositions(entry_id,path_digest,disposition,reason) VALUES(?1,'p1',?2,?3)", [id, disposition, reason]).unwrap();
    };
    let db = p.sidecar();
    db.execute("INSERT INTO usage_ledger(singleton,normalization_version,synced_unix_ms) VALUES(1,'codex-v1',1)", []).unwrap();
    entry(&db, "codex:s1:1", "accepted", "");
    entry(&db, "codex:s1:thread:p1", "unresolved", "regression_without_reset");
    let s = state(&p.evaluate(), "accounting_conflict").clone();
    assert_eq!((&s["state"], &s["evidence"]["conflict"], &s["evidence"]["unresolved"]), (&json!("warn"), &json!(0), &json!(1)), "{s}");
    assert_eq!(s["evidence"]["by_reason"], json!({"unresolved": {"regression_without_reset": 1}}));
    entry(&db, "codex:s1:2", "conflict", "payload_digest_mismatch");
    let s = state(&p.evaluate(), "accounting_conflict").clone();
    assert_eq!((&s["state"], codes(&s)), (&json!("critical"), vec!["conflict_dispositions".to_owned(), "unresolved_dispositions".to_owned()]));
    let alerts = p.alerts();
    assert_eq!(alerts["open"].as_array().unwrap().iter().filter(|a| a["rule"] == "accounting_conflict").count(), 1, "one open alert per labels");
    db.execute("DELETE FROM usage_dispositions WHERE disposition<>'accepted'", []).unwrap();
    let out = p.evaluate();
    assert_eq!(state(&out, "accounting_conflict")["state"], "ok");
    assert_eq!(out["resolved"].as_array().unwrap().iter().filter(|a| a["rule"] == "accounting_conflict").count(), 1);
}

/// A current 300-minute window with 12.5% remaining: below 20 (warn), not
/// below 5 (critical): `warn headroom_low`; 0% remaining is `critical
/// window_exhausted` (the service will throttle); a window that has already
/// reset leaves the headroom unknown (`no_current_window`). M38 stays unknown
/// (`throttling_not_certified`) and never opens an alert.
#[test]
fn low_quota_headroom_and_throttled_service() {
    let p = Planted::new();
    p.sidecar_created();
    let now = unix_ms();
    quota_window(&p.sidecar(), "12.5", now + HOUR, now - 2 * MINUTE);
    let out = p.evaluate();
    let s = state(&out, "quota_headroom");
    assert_eq!((&s["state"], codes(s)), (&json!("warn"), vec!["headroom_low".to_owned()]), "{s}");
    assert_eq!((&s["evidence"]["lowest"]["remaining"], &s["evidence"]["lowest"]["used"], &s["evidence"]["current_windows"]), (&json!("12.5"), &json!("87.5"), &json!(1)));
    assert_eq!(s["labels"], json!({"project": "demo", "family": "services", "rule": "quota_headroom", "service": "codex"}));
    assert!(!s.to_string().contains("sha256:"), "no account identity in a health state: {s}");
    assert_eq!((&s["evidence_window"]["to_unix_ms"], &s["evidence_window"]["semantics"]), (&json!(now + HOUR), &json!("quota_window")));
    let throttled = state(&out, "service_throttled");
    assert_eq!((&throttled["state"], codes(throttled)), (&json!("unknown"), vec!["throttling_not_certified".to_owned()]));
    assert!(open_alert(&p.alerts(), "service_throttled").is_none());

    quota_window(&p.sidecar(), "0", now + HOUR, now - MINUTE);
    let s = state(&p.evaluate(), "quota_headroom").clone();
    assert_eq!((&s["state"], codes(&s)), (&json!("critical"), vec!["window_exhausted".to_owned()]));
    quota_window(&p.sidecar(), "40", now - MINUTE, now - 2 * HOUR);
    let s = state(&p.evaluate(), "quota_headroom").clone();
    assert_eq!((&s["state"], codes(&s)), (&json!("unknown"), vec!["no_current_window".to_owned()]));
    let alert = open_alert(&p.alerts(), "quota_headroom").unwrap().clone();
    assert_eq!((&alert["state"], &alert["occurrences"]), (&json!("unknown"), &json!(3)), "headroom unknown after the reset keeps the alert open: {alert}");
}

/// A running attempt launched at T−8m10s, sampled `blocked` every 30 s from
/// T−8m to T−30s: one open wait of 450000 ms (≥ 5 min: warn, < 30 min).
#[test]
fn long_waiting_on_you_interval_warns() {
    let p = Planted::new();
    let config = configuration(&p.db(), "codex", "0.154.0");
    task(&p.db(), "t1", "open", "code", &[Attempt { configuration: &config, state: "running", profile: ("coder", "codex") }], 1_000);
    p.sidecar_created();
    let now = unix_ms();
    let first = now - 8 * MINUTE;
    let receipt = json!({"version": 2, "attempt": "t1-a0", "operation": "op-launch", "route": {"machine": "", "socket": "/nonexistent/herdr.sock",
        "workspace_id": "w1", "tab_id": "w1:t1", "pane_id": "w1:p1", "cwd": "/w"}, "agent": {"kind": "codex", "name": "coder"}, "observed_unix_ms": first - 10_000});
    p.db().execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('runtime.launch_started','op-launch',1,1,?1)", [receipt.to_string()]).unwrap();
    let s = state(&p.health(), "waiting_on_you").clone();
    assert_eq!((&s["state"], codes(&s)), (&json!("unknown"), vec!["attention_not_collected".to_owned()]));
    for k in 0..16 {
        p.sidecar().execute("INSERT INTO attention_samples(attempt_id,observed_unix_ms,state,gap,interval_ms,source) VALUES('t1-a0',?1,'blocked',NULL,30000,'herdr-agent-list-v1')",
            [first + k * 30_000]).unwrap();
    }
    let out = p.evaluate();
    let s = state(&out, "waiting_on_you");
    assert_eq!((&s["state"], &s["evidence"]["longest_wait_ms"], &s["evidence"]["waiting_attempts"], &s["evidence"]["open_attempts"]),
        (&json!("warn"), &json!(450_000), &json!(1), &json!(1)), "{s}");
    assert_eq!(s["evidence_window"], json!({"from_unix_ms": first, "to_unix_ms": first + 15 * 30_000, "semantics": "open_wait_observed"}));
    assert_eq!(s["labels"], json!({"project": "demo", "family": "attention", "rule": "waiting_on_you"}));
    assert_eq!(out["opened"].as_array().unwrap().iter().filter(|a| a["rule"] == "waiting_on_you").count(), 1);
}

/// Cost and latency shifts over two 7-day windows. Prior week: 5 accepted
/// tasks with 1 attempt each (M07 5/5); last week: 5 accepted tasks with 2
/// attempts each (M07 10/5). Ratio to prior = (10·5)/(5·5) = 2 → 2000‰,
/// ≥ 2000 critical. Lead time is 100 ms in both weeks (M06 ratio 1): ok.
/// With 4 tasks in a window the shift is `unknown insufficient_data`.
#[test]
fn attempt_cost_shift_and_latency_shift_compare_two_windows() {
    let p = Planted::new();
    let config = configuration(&p.db(), "codex", "0.154.0");
    let now = unix_ms();
    let one = |state| Attempt { configuration: &config, state, profile: ("coder", "codex") };
    for i in 0..5 {
        task(&p.db(), &format!("old{i}"), "accepted", "code", &[one("completed")], now - 8 * 24 * HOUR + i * 1_000);
    }
    for i in 0..4 {
        task(&p.db(), &format!("new{i}"), "accepted", "code", &[one("failed"), one("completed")], now - 24 * HOUR + i * 1_000);
    }
    let s = state(&p.health(), "attempt_cost_shift").clone();
    assert_eq!((&s["state"], codes(&s)), (&json!("unknown"), vec!["insufficient_data".to_owned()]), "{s}");
    assert_eq!((&s["evidence"]["current"]["samples"], &s["evidence"]["prior"]["samples"]), (&json!(4), &json!(5)));
    task(&p.db(), "new4", "accepted", "code", &[one("failed"), one("completed")], now - 24 * HOUR + 4_000);
    let out = p.health();
    let s = state(&out, "attempt_cost_shift");
    assert_eq!((&s["state"], &s["evidence"]["current"]["value"], &s["evidence"]["prior"]["value"]), (&json!("critical"), &json!("10/5"), &json!("5/5")), "{s}");
    assert_eq!(s["reasons"], json!([{"code": "shift_up", "ratio_to_prior": "50/25"}]));
    assert_eq!(s["metric"]["definition"], "M07.cohort-v1");
    let latency = state(&out, "latency_shift");
    assert_eq!((&latency["state"], &latency["evidence"]["current"]["value"], &latency["evidence"]["prior"]["value"]), (&json!("ok"), &json!(100), &json!(100)), "{latency}");
    assert_eq!(latency["reasons"], json!([{"code": "no_shift", "ratio_to_prior": "100/100"}]));
}

// Cooldown, deduplication and inbox notices

/// warn → one alert; the same condition again → the same alert
/// (occurrences 2, `deduplicated`); resolved; back within the one-hour
/// cooldown → no alert (suppressed 1); after the cooldown → a second alert.
/// `health notify` writes one inbox notice per alert, once: a second run and
/// a deduplicated re-evaluation write nothing.
#[test]
fn cooldown_dedup_and_inbox_notice_written_once() {
    let p = Planted::new();
    p.sidecar_created();
    let now = unix_ms();
    quota_window(&p.sidecar(), "12.5", now + 2 * HOUR, now - MINUTE);
    let first = p.evaluate();
    let opened: Vec<&Value> = first["opened"].as_array().unwrap().iter().filter(|a| a["rule"] == "quota_headroom").collect();
    assert_eq!(opened.len(), 1);
    let id = opened[0]["alert_id"].as_i64().unwrap();
    let again = p.evaluate();
    assert!(again["opened"].as_array().unwrap().iter().all(|a| a["rule"] != "quota_headroom"), "{again}");
    let update = again["updated"].as_array().unwrap().iter().find(|a| a["rule"] == "quota_headroom").unwrap();
    assert_eq!((&update["alert_id"], &update["deduplicated"]), (&json!(id), &json!(true)));
    let alert = open_alert(&p.alerts(), "quota_headroom").unwrap().clone();
    assert_eq!((&alert["occurrences"], &alert["metric"]["read"], &alert["rules_version"]), (&json!(2), &json!("lane:accounting quota"), &json!("health-rules.v1")));

    // Inbox: one notice for the open alert, never two.
    let before = |p: &Planted| p.db().query_row("SELECT count(*) FROM inbox_items WHERE json_extract(payload,'$.kind')='telemetry-health'", [], |r| r.get::<_, i64>(0)).unwrap();
    assert_eq!(before(&p), 0);
    let receipt = p.json(&["health", "notify"]);
    let notices: Vec<&Value> = receipt["delivered"].as_array().unwrap().iter().filter(|d| d["alert_id"] == json!(id)).collect();
    assert_eq!(notices.len(), 1, "{receipt}");
    let opened_ms = alert["opened_unix_ms"].as_i64().unwrap();
    assert_eq!((&notices[0]["notice_id"], &notices[0]["outcome"]), (&json!(format!("telemetry-health-{id}-{opened_ms}")), &json!("delivered")));
    let rows = before(&p);
    let payload: Value = serde_json::from_str(&p.db().query_row("SELECT payload FROM inbox_items WHERE id=?1", [notices[0]["notice_id"].as_str().unwrap()], |r| r.get::<_, String>(0)).unwrap()).unwrap();
    assert_eq!((&payload["kind"], &payload["subject"], &payload["body"]), (&json!("telemetry-health"), &json!("quota_headroom"), &json!("")));
    assert_eq!(payload["summary"], json!("telemetry health warn: quota_headroom [services service=codex] headroom_low; advisory — `herdr-projects telemetry demo health alerts`"));
    let second = p.json(&["health", "notify"]);
    assert!(second["delivered"].as_array().unwrap().is_empty(), "{second}");
    p.evaluate();
    assert!(p.json(&["health", "notify"])["delivered"].as_array().unwrap().is_empty());
    assert_eq!(before(&p), rows, "the inbox notice is written once");
    assert_eq!(open_alert(&p.alerts(), "quota_headroom").unwrap()["notice_id"], notices[0]["notice_id"]);

    // Resolve, then return within the cooldown: suppressed, no new alert.
    quota_window(&p.sidecar(), "50", now + 2 * HOUR, now - MINUTE);
    let resolved = p.evaluate();
    assert_eq!(resolved["resolved"].as_array().unwrap().iter().filter(|a| a["alert_id"] == json!(id)).count(), 1);
    assert!(open_alert(&p.alerts(), "quota_headroom").is_none());
    quota_window(&p.sidecar(), "12.5", now + 2 * HOUR, now - MINUTE);
    let held = p.evaluate();
    assert!(held["opened"].as_array().unwrap().iter().all(|a| a["rule"] != "quota_headroom"), "no repeat alert within the cooldown: {held}");
    assert_eq!(held["suppressed"].as_array().unwrap().iter().filter(|a| a["rule"] == "quota_headroom").count(), 1);
    assert!(open_alert(&p.alerts(), "quota_headroom").is_none());
    let suppressed: i64 = p.sidecar().query_row("SELECT suppressed FROM health_rule_states WHERE rule='quota_headroom'", [], |r| r.get(0)).unwrap();
    assert_eq!(suppressed, 1);
    let since = p.json(&["health", "alerts", "--since", "0", "--json"]);
    assert_eq!(since["recent"].as_array().unwrap().iter().filter(|a| a["rule"] == "quota_headroom" && a["status"] == "resolved").count(), 1);

    // After the cooldown (the resolved episode moved an hour into the past): a new alert.
    p.sidecar().execute("UPDATE health_alerts SET opened_unix_ms=opened_unix_ms-?2,last_seen_unix_ms=last_seen_unix_ms-?2,resolved_unix_ms=resolved_unix_ms-?2 WHERE alert_id=?1",
        [id, HOUR]).unwrap();
    let later = p.evaluate();
    let reopened: Vec<&Value> = later["opened"].as_array().unwrap().iter().filter(|a| a["rule"] == "quota_headroom").collect();
    assert_eq!(reopened.len(), 1);
    assert_ne!(reopened[0]["alert_id"], json!(id));
    let receipt = p.json(&["health", "notify"]);
    assert_eq!(receipt["delivered"].as_array().unwrap().iter().filter(|d| d["alert_id"] == reopened[0]["alert_id"]).count(), 1);
    assert_eq!(before(&p), rows + 1, "one more notice for the new episode");
}

/// External notices need the deployment's own setting: refused with no file
/// (the default); a directory destination gets one file per open alert, once.
#[test]
fn external_notification_is_disabled_by_default_and_local_only() {
    let p = Planted::new();
    p.sidecar_created();
    quota_window(&p.sidecar(), "3", unix_ms() + HOUR, unix_ms() - MINUTE);
    p.evaluate();
    let err = p.fail(&["health", "notify", "--external"]);
    assert!(err.contains("notify rejected") && err.contains("external_notification_disabled"), "{err}");
    let dir = p.config_dir();
    fs::create_dir_all(&dir).unwrap();
    let outbox = p.tmp.path().join("outbox");
    fs::create_dir_all(&outbox).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&outbox, fs::Permissions::from_mode(0o700)).unwrap();
    let config = dir.join("telemetry-alerts.toml");
    fs::write(&config, "schema = \"telemetry-alerts-config.v1\"\n[external]\nenabled = false\ndestination = \"stdout\"\n").unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(p.fail(&["health", "notify", "--external"]).contains("external_notification_disabled"));
    fs::write(&config, format!("schema = \"telemetry-alerts-config.v1\"\n[external]\nenabled = true\ndestination = \"directory\"\ndirectory = \"{}\"\n", outbox.display())).unwrap();
    let receipt = p.json(&["health", "notify", "--external"]);
    assert_eq!((&receipt["destination"], receipt["written"].as_array().unwrap().len()), (&json!("directory"), 1), "{receipt}");
    let file = fs::read_dir(&outbox).unwrap().next().unwrap().unwrap().path();
    let sent: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    assert_eq!((&sent["contract"], &sent["alert"]["rule"], &sent["alert"]["state"]), (&json!("telemetry-health-alert.v1"), &json!("quota_headroom"), &json!("critical")));
    let again = p.json(&["health", "notify", "--external"]);
    assert_eq!((again["written"].as_array().unwrap().len(), &again["already_written"]), (0, &json!(1)));
    let inbox: i64 = p.db().query_row("SELECT count(*) FROM inbox_items", [], |r| r.get(0)).unwrap();
    assert_eq!(inbox, 0, "an external notice writes no inbox row");
}

// Regressions: a reopened integrated fix

fn rep(c: char) -> String { c.to_string().repeat(64) }
fn oid(c: char) -> String { c.to_string().repeat(40) }
fn evidence(c: char) -> String { format!("sha256:{}", rep(c)) }

struct Factory(rusqlite::Connection);

impl Factory {
    fn open(f: &Fixture) -> Self {
        let db = rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap();
        db.execute_batch("INSERT OR IGNORE INTO acceptance_policies(task_id,contract_revision,policy_id,body) VALUES('work',1,'ci','cargo test');
            INSERT OR IGNORE INTO integration_targets(repository,ref_name,created_unix_ms) VALUES('/repo','refs/heads/main',1);
            INSERT OR IGNORE INTO integration_target_leases(repository,ref_name,operation_id,generation) VALUES('/repo','refs/heads/main',NULL,0);").unwrap();
        Factory(db)
    }
    fn attempt(&self, id: &str, configuration: &str) {
        self.0.execute("INSERT INTO attempts(id,task_id,revision,state,reservation,termination_observed) VALUES(?1,'work',1,'running',?1,0)", [id]).unwrap();
        self.0.execute("INSERT INTO dispatch_decisions(attempt_id,task_id,task_revision,contract_revision,chosen_configuration_id,eligible,chooser_kind,chooser_principal,reason_codes,decided_unix_ms)
            VALUES(?1,'work',1,1,?2,'[\"x\"]','operator','operator:cli','[\"x\"]',1)", [id, configuration]).unwrap();
    }
    fn submission(&self, id: &str, attempt: &str, candidate: &str, at: i64) {
        self.0.execute("INSERT INTO result_submissions(submission_id,project_store,idempotency_key,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,artifact_manifest,claimed_checks,created_unix_ms)
            VALUES(?1,'store',?1,?2,'{}','work',1,?2,?3,'/repo',?4,?5,'sha1','[]','[]',?6)", rusqlite::params![id, rep('d'), attempt, "b".repeat(40), candidate, at]).unwrap();
    }
    fn run(&self, run: &str, submission: &str, attempt: &str, commit: &str, result: &str) {
        self.0.execute("INSERT INTO verification_runs(run_id,project_store,idempotency_key,payload_digest,submission_id,task_id,contract_revision,contract_digest,attempt_id,policy_id,policy_digest,
            commit_oid,tree_oid,object_format,memory_fence,isolation,argv,library_manifest,state,reason,exit_status,receipt_digest,store_device,store_inode,created_unix_ms)
            VALUES(?1,'store',?1,?2,?3,'work',1,?2,?4,'ci',?5,?6,?6,'sha1',0,'linux-unshare-user-pid-mount-v1','[\"x\"]','[]','accepted',NULL,0,?7,1,1,7000)",
            rusqlite::params![run, rep('d'), submission, attempt, rep('9'), commit, rep('8')]).unwrap();
        self.0.execute("INSERT INTO verified_results(result_id,run_id,submission_id,commit_oid,tree_oid,object_format,policy_digest,receipt_digest,isolation,memory_fence,created_unix_ms)
            VALUES(?1,?2,?3,?4,?4,'sha1',?5,?6,'linux-unshare-user-pid-mount-v1',0,7000)", rusqlite::params![result, run, submission, commit, rep('9'), rep('8')]).unwrap();
    }
    fn integration(&self, id: &str, result: &str, parent: &str, commit: &str, at: i64) {
        self.0.execute("INSERT INTO operations(id,task_id,kind,target,payload_version,payload,payload_hash,expected_revision,due_unix_ms,idempotency_key)
            VALUES(?1,'work','integration.run','refs/heads/main',1,'{}',?2,1,0,?1)", rusqlite::params![id, rep('d')]).unwrap();
        self.0.execute("INSERT INTO integration_operations(operation_id,project_store,idempotency_key,payload_digest,repository,ref_name,expected_old_oid,verified_result_id,candidate_id,state,generation,object_format,checks_passed,reason,created_unix_ms)
            VALUES(?1,'store',?1,?2,'/repo','refs/heads/main',?3,?4,?1,'integrated',1,'sha1',1,NULL,?5)", rusqlite::params![id, rep('d'), "b".repeat(40), result, at]).unwrap();
        self.0.execute("INSERT INTO integration_candidates(candidate_id,operation_id,commit_oid,tree_oid,parent_base,parent_verified,strategy,object_format,state,created_unix_ms)
            VALUES(?1,?1,?2,?2,?3,?4,'ort','sha1','published',?5)", rusqlite::params![id, commit, "b".repeat(40), parent, at]).unwrap();
        self.0.execute("INSERT INTO integrated_commits(integrated_id,candidate_id,operation_id,repository,ref_name,commit_oid,tree_oid,expected_old_oid,object_format,created_unix_ms)
            VALUES(?1,?1,?1,'/repo','refs/heads/main',?2,?2,?3,'sha1',?4)", rusqlite::params![id, commit, "b".repeat(40), at]).unwrap();
    }
}

/// Doc 10 §5 fix golden (as tests/telemetry_views.rs): one integrated fix,
/// then a revert reopens it. Before: M27 numerator 0 (the integration is
/// censored inside its 14-day horizon) → `ok none_observed`. After: 1
/// reopened integration (≥ 1 warn, < 3 critical) → `warn`, one alert.
#[test]
fn reopened_fix_increase_raises_the_regression_rule() {
    let f = Fixture::new();
    let db_path = f.project.join(".state/state.db");
    let mut fast = codex_profile(&f.config, "codex", "fast", Some(&f.tmp.path().join("fast-home")));
    fast.arguments_digest = "1".repeat(64);
    plant_profile(&db_path, fast.clone());
    let (sub, candidate) = (rep('1'), "1".repeat(40));
    {
        let db = rusqlite::Connection::open(&db_path).unwrap();
        db.execute("INSERT INTO task_contracts(task_id,contract_revision,plan_revision,project_store,expected_head,repository,base_oid,object_format,memory_snapshot_id,route,raw_bytes,raw_digest,installed_seq)
            VALUES('work',1,NULL,'store',0,'/repo',?1,'sha1',NULL,'verify_only',x'61',?2,(SELECT max(sequence) FROM events))", rusqlite::params!["b".repeat(40), rep('c')]).unwrap();
        db.execute("INSERT INTO result_submissions(submission_id,project_store,idempotency_key,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,artifact_manifest,claimed_checks,created_unix_ms)
            VALUES(?1,'store',?1,?2,'{}','work',1,?2,?3,'/repo',?4,?5,'sha1','[]','[]',1000)", rusqlite::params![sub, rep('d'), f.attempt, "b".repeat(40), candidate]).unwrap();
        db.execute("INSERT INTO attempts(id,task_id,revision,state,reservation,termination_observed) VALUES('rev-a1','work',1,'completed','rev-a1',1)", []).unwrap();
    }
    let opportunity = f.cli_args(&["review", "open", &sub, "--kind", "code", "--protocol", "review-protocol.v1"]).0["opportunity"]["opportunity_id"].as_str().unwrap().to_owned();
    f.cli_args(&["review", "assign", &opportunity, "--reviewer", "fast"]);
    let session = f.cli_args(&["review", "start", &opportunity, "--attempt", "rev-a1"]).0["session"]["session_id"].as_str().unwrap().to_owned();
    let receipt = f.tmp.path().join("receipt.json");
    fs::write(&receipt, json!({"schema": "review_receipt.v1", "session_id": session, "submission_id": sub, "candidate_oid": candidate, "outcome": "completed",
        "findings": ["finding:crash"], "evidence": []}).to_string()).unwrap();
    f.cli_args(&["review", "complete", "--input-file", receipt.to_str().unwrap()]);
    let validated = f.cli_args(&["review", "findings", "validate", "1", "--new", "--severity", "high", "--evidence", &evidence('e')]).0;
    let finding = validated["event"]["subject"]["finding_id"].as_str().unwrap().to_owned();
    let fixes = |args: &[&str]| { let mut all = vec!["review", "fixes"]; all.extend(args); f.cli_args(&all).0 };
    let opened = fixes(&["open", &finding, "--assign", "fast"]);
    let repair = opened["event"]["subject"]["repair_seq"].as_i64().unwrap().to_string();
    let factory = Factory::open(&f);
    factory.attempt("fix-a1", &herdr_projects::domain::agent_configuration(&fast).id);
    fixes(&["bind", &repair, "--attempt", "fix-a1"]);
    factory.submission(&rep('4'), "fix-a1", &oid('4'), 6_000);
    factory.run(&rep('d'), &rep('4'), "fix-a1", &oid('4'), &rep('f'));
    factory.integration(&rep('7'), &rep('f'), &oid('4'), &oid('5'), unix_ms() - 1_000);
    let proposal = fixes(&["propose", &repair, "--submission", &rep('4')])["event"]["seq"].as_i64().unwrap().to_string();
    fixes(&["verify", &proposal, "--run", &rep('d'), "--assurance", "regression_reproduced", "--evidence", &evidence('a')]);
    fixes(&["integrate", &proposal, "--integrated", &rep('7')]);
    fixes(&["close", &repair, "--outcome", "fixed"]);
    f.cli("collect");

    let evaluate = || f.cli_args(&["health", "evaluate", "--json"]).0;
    let out = evaluate();
    let s = state(&out, "fix_reopened");
    assert_eq!((&s["state"], &s["evidence"]["numerator"], &s["evidence"]["censored"], codes(s)), (&json!("ok"), &json!(0), &json!(1), vec!["none_observed".to_owned()]), "{s}");
    assert_eq!((&s["metric"]["metric_id"], &s["metric"]["definition"]), (&json!("M27"), &json!("M27.v1")));
    assert_eq!(s["evidence_window"]["semantics"], "since_only");

    fixes(&["reopen", &finding, "--reason", "reverted", "--observed", &oid('6'), "--evidence", &evidence('d')]);
    let out = evaluate();
    let s = state(&out, "fix_reopened");
    assert_eq!((&s["state"], &s["evidence"]["numerator"], &s["evidence"]["value"]), (&json!("warn"), &json!(1), &json!("1/1")), "{s}");
    assert_eq!(s["reasons"], json!([{"code": "reopened_integrations_observed", "count": 1}]));
    assert_eq!(out["opened"].as_array().unwrap().iter().filter(|a| a["rule"] == "fix_reopened").count(), 1);
    assert_eq!(s["labels"], json!({"project": "demo", "family": "review_quality", "rule": "fix_reopened"}));
}

// Advisory recommendations

/// 20 code tasks on `codex 0.154.0` (profile `coder`) all accepted, and
/// `claude 1.0.0` (profile `reviewer`) with `claude` tasks all failed.
fn ranked_world(p: &Planted, claude_tasks: usize) -> (String, String) {
    let db = p.db();
    let (codex, claude) = (configuration(&db, "codex", "0.154.0"), configuration(&db, "claude", "1.0.0"));
    for i in 0..20 {
        task(&db, &format!("c{i:02}"), "accepted", "code", &[Attempt { configuration: &codex, state: "completed", profile: ("coder", "codex") }], 1_000 + i as i64 * 1_000);
    }
    for i in 0..claude_tasks {
        task(&db, &format!("d{i:02}"), "failed", "code", &[Attempt { configuration: &claude, state: "failed", profile: ("reviewer", "claude") }], 1_500 + i as i64 * 1_000);
    }
    (codex, claude)
}

/// Hand-computed: M02 codex 20/20 (every bootstrap draw 20/20), claude 0/20
/// (every draw 0/20); equal difficulty bands; intervals separate → ranking
/// [codex, claude] and a `recommended` codex with M50 freshness 20/20
/// (lineage profile `coder`, still dispatching codex 0.154.0). A later
/// `coder` dispatch on `codex 0.155.0` changes the configuration: M50 = 0/20
/// < 1/2 → `stale` (configuration_changed codex 0.154.0 → codex 0.155.0), and
/// the health rule warns for role code. Neither command writes `state.db`
/// (byte-identical, no side files) and `recommend` writes no sidecar.
#[test]
fn recommendation_carries_evidence_and_goes_stale_after_a_configuration_change() {
    let p = Planted::new();
    let (codex, claude) = ranked_world(&p, 20);
    let before = p.state_files();
    let rec = p.json(&["recommend", "--role", "code", "--json"]);
    assert_eq!(p.state_files(), before, "recommend writes nothing: state.db byte-identical, no sidecar or side file");
    assert_eq!((&rec["contract"], &rec["status"], &rec["role"]), (&json!("telemetry-recommendation.v1"), &json!("recommended"), &json!("code")), "{rec}");
    assert_eq!(rec["advisory"], json!({"advisory": true, "authority": "none", "writes": "none",
        "routing": "advisory only: a person decides; nothing here is read by dispatch or admission, and it changes no authority, profile, model access, spending limit or acceptance check"}));
    assert_eq!(rec["metric"], json!({"metric_id": "M02", "definition": "M02.cohort-v1", "higher_is_better": true, "registry": "analytics-registry.v1",
        "comparison": "analytics-comparison.v1", "freshness": "M50.recommendation-v1"}));
    assert_eq!(rec["evidence_window"], json!({"cohort": "terminal_cohort", "from_unix_ms": null, "to_unix_ms": null, "semantics": "half_open", "time_basis": "task_terminal_time"}));
    assert_eq!(rec["recommendation"]["configuration_id"], json!(codex));
    assert_eq!((&rec["recommendation"]["label"], &rec["recommendation"]["value"], &rec["recommendation"]["tasks"]), (&json!("codex 0.154.0"), &json!("20/20"), &json!(20)));
    assert_eq!((&rec["uncertainty"]["recommended"]["interval"]["lower"], &rec["uncertainty"]["recommended"]["interval"]["upper"]), (&json!("20/20"), &json!("20/20")));
    assert_eq!((&rec["uncertainty"]["runner_up"]["configuration_id"], &rec["uncertainty"]["runner_up"]["label"]), (&json!(claude), &json!("claude 1.0.0")));
    assert_eq!((&rec["uncertainty"]["runner_up"]["interval"]["lower"], &rec["uncertainty"]["runner_up"]["interval"]["upper"]), (&json!("0/20"), &json!("0/20")));
    assert_eq!((&rec["uncertainty"]["estimator"]["method"], &rec["uncertainty"]["estimator"]["iterations"]), (&json!("percentile_bootstrap.v1"), &json!(1000)));
    let fresh = &rec["freshness"];
    assert_eq!((&fresh["metric_id"], &fresh["definition"], &fresh["value"], &fresh["decimal"], &fresh["state"], &fresh["stale_below"]),
        (&json!("M50"), &json!("M50.recommendation-v1"), &json!("20/20"), &json!("1.0000"), &json!("fresh"), &json!("1/2")));
    assert_eq!(fresh["lineage"], json!({"basis": "profile", "keys": ["coder"]}));
    assert_eq!(fresh["current_configuration_id"], json!(codex));
    assert_eq!(rec["reasons"], json!([{"code": "intervals_separated", "order": ["codex 0.154.0", "claude 1.0.0"]}]));
    let query_m50 = &p.json(&["query", "--json", "--metric", "M50"])["results"][0];
    assert_eq!((&query_m50["definition"], &query_m50["reason"]), (&json!("M50.recommendation-v1"), &json!("per_recommendation")));
    let registry = p.json(&["metrics", "registry", "--json"]);
    let m50 = registry["metrics"].as_array().unwrap().iter().find(|m| m["id"] == "M50").unwrap();
    assert_eq!((&m50["active"], &m50["definition"], &registry["freshness"]["stale_below"]), (&json!(true), &json!("M50.recommendation-v1"), &json!("1/2")));

    // A harness update: profile `coder` now dispatches codex 0.155.0 (an open task, no evidence yet).
    let newer = configuration(&p.db(), "codex", "0.155.0");
    task(&p.db(), "n01", "open", "code", &[Attempt { configuration: &newer, state: "running", profile: ("coder", "codex") }], 90_000);
    let before = p.state_files();
    let rec = p.json(&["recommend", "--role", "code", "--json"]);
    assert_eq!(p.state_files(), before);
    assert_eq!((&rec["status"], &rec["recommendation"]["label"]), (&json!("stale"), &json!("codex 0.154.0")), "{rec}");
    assert_eq!((&rec["freshness"]["value"], &rec["freshness"]["state"], &rec["freshness"]["current_label"], &rec["freshness"]["current_configuration_id"]),
        (&json!("0/20"), &json!("stale"), &json!("codex 0.155.0"), &json!(newer)));
    assert_eq!(rec["reasons"][1]["code"], "configuration_changed");
    assert_eq!((&rec["reasons"][1]["from"], &rec["reasons"][1]["to"]), (&json!("codex 0.154.0"), &json!("codex 0.155.0")));
    let text = String::from_utf8(p.raw(&["recommend", "--role", "code"])).unwrap();
    assert!(text.starts_with("role code · stale · M02 M02.cohort-v1 · advisory\n"), "{text}");
    assert!(text.contains("  freshness M50 0/20 (stale; stale below 1/2)\n"), "{text}");

    p.sidecar_created();
    let canonical = p.canonical_bytes();
    let out = p.evaluate();
    assert_eq!(p.canonical_bytes(), canonical, "health evaluate writes only the sidecar");
    let s = out["states"].as_array().unwrap().iter().find(|s| s["rule"] == "recommendation_stale").unwrap();
    assert_eq!((&s["state"], &s["labels"]), (&json!("warn"), &json!({"project": "demo", "family": "recommendation", "rule": "recommendation_stale", "role": "code"})));
    assert_eq!((&s["evidence"]["recommended"], &s["evidence"]["current"], &s["evidence"]["freshness"]["value"]), (&json!("codex 0.154.0"), &json!("codex 0.155.0"), &json!("0/20")));
    assert_eq!(s["metric"]["definition"], "M50.recommendation-v1");
    assert!(!s["labels"].to_string().contains("sha256:"), "no configuration identity in a label");
}

/// With 19 claude tasks the claude cell is suppressed (below the 20-task
/// minimum): TM4.4 refuses a ranking, so there is no recommendation, with
/// the comparison's reasons. A role without a cell has none either.
#[test]
fn no_recommendation_when_the_comparison_refuses_to_rank() {
    let p = Planted::new();
    let (_, claude) = ranked_world(&p, 19);
    let before = p.state_files();
    let rec = p.json(&["recommend", "--role", "code", "--json"]);
    assert_eq!(p.state_files(), before);
    assert_eq!((&rec["status"], &rec["recommendation"], &rec["freshness"]), (&json!("no_recommendation"), &Value::Null, &Value::Null), "{rec}");
    assert_eq!(rec["reasons"], json!([{"code": "ranking_not_supported", "compare_reasons": [{"insufficient_data": [claude]}]}]));
    let arm = rec["arms"].as_array().unwrap().iter().find(|a| a["configuration_id"] == json!(claude)).unwrap();
    assert_eq!((&arm["status"], &arm["tasks"]), (&json!("suppressed"), &json!(19)));
    let none = p.json(&["recommend", "--role", "docs", "--json"]);
    assert_eq!((&none["status"], &none["reasons"][0]["code"]), (&json!("no_recommendation"), &json!("no_evidence_for_role")));
    assert!(p.fail(&["recommend", "--role", "../x", "--json"]).contains("recommend rejected"));
    assert!(p.fail(&["recommend", "--role", "code", "--metric", "M13"]).contains("comparison_unsupported"));
}

/// The recommendation and health code has no path to launch authority,
/// profiles, model access, budgets or acceptance: the recommendation module
/// opens nothing writable and names no store, admission, launch or profile
/// API, and no dispatch or admission source reads the health lane.
#[test]
fn recommendations_have_no_path_to_grants_profiles_budgets_or_gates() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let code = |path: &str| fs::read_to_string(root.join(path)).unwrap().lines().filter(|l| !l.trim_start().starts_with("//")).collect::<Vec<_>>().join("\n");
    let recommend = code("src/telemetry/health/recommend.rs");
    for forbidden in ["crate::store", "SqliteStore", "crate::admission", "launch_preparation", "profile_config", "accounting::budget", "sidecar::open",
        "Connection::open", ".execute(", "transaction", "crate::migration", "grant", "reserve"] {
        assert!(!recommend.contains(forbidden), "recommend.rs mentions {forbidden}");
    }
    assert!(recommend.contains("crate::telemetry::read_only"), "the dispatch log is read strictly read-only");
    for rules in ["src/telemetry/health/rules.rs", "src/telemetry/health/store.rs", "src/telemetry/health/mod.rs"] {
        let text = code(rules);
        for forbidden in ["SqliteStore", "crate::admission", "launch_preparation", "profile_config", "crate::migration"] {
            assert!(!text.contains(forbidden), "{rules} mentions {forbidden}");
        }
    }
    // The only canonical write of the lane: inbox notices through the store's inbox delivery.
    let notify = code("src/telemetry/health/notify.rs");
    let store_calls: Vec<&str> = notify.lines().filter(|l| l.contains("store.") || l.contains("SqliteStore")).collect();
    assert!(store_calls.iter().all(|l| l.contains("SqliteStore::open") || l.contains("deliver_telemetry_notice") || l.contains("current_head")), "{store_calls:?}");
    let mut dispatch = vec!["src/admission.rs".to_owned(), "src/launch_preparation.rs".to_owned(), "src/fair_admission.rs".to_owned(), "src/profile_config.rs".to_owned()];
    for dir in ["src/store", "src/scheduling"] {
        if let Ok(entries) = fs::read_dir(root.join(dir)) {
            dispatch.extend(entries.flatten().filter(|e| e.path().extension().is_some_and(|x| x == "rs")).map(|e| format!("{dir}/{}", e.file_name().to_string_lossy())));
        }
    }
    for file in dispatch.iter().filter(|f| root.join(f).is_file()) {
        let text = fs::read_to_string(root.join(file)).unwrap();
        assert!(!text.contains("telemetry::health") && !text.contains("recommend::"), "{file} reads the health lane");
    }
}

/// The rule table is declared and bounded; `health` text names every rule.
#[test]
fn rule_table_is_declared_with_bounded_labels() {
    let p = Planted::new();
    let rules = p.json(&["health", "rules"]);
    assert_eq!((&rules["rules_version"], &rules["labels"]), (&json!("health-rules.v1"), &json!(["project", "family", "rule", "service", "role"])));
    let names: Vec<&str> = rules["rules"].as_array().unwrap().iter().map(|r| r["rule"].as_str().unwrap()).collect();
    assert_eq!(names, ["collector_stale", "usage_coverage", "cost_coverage", "accounting_conflict", "budget_exposure", "fix_reopened", "integration_reverted",
        "latency_shift", "attempt_cost_shift", "service_throttled", "quota_headroom", "waiting_on_you", "recommendation_stale"]);
    let quota = rules["rules"].as_array().unwrap().iter().find(|r| r["rule"] == "quota_headroom").unwrap();
    assert_eq!(quota["thresholds"], json!({"direction": "below", "warn": 20000, "critical": 5000, "unit": "millipercent_remaining", "window_ms": null, "cooldown_ms": HOUR}));
    let text = String::from_utf8(p.raw(&["health"])).unwrap();
    assert!(text.starts_with("demo · health · health-rules.v1 · live_read_only\n"), "{text}");
    assert!(text.contains("  collector_stale [collection service=codex]: unknown collection_not_run\n"), "{text}");
    assert!(text.ends_with("alerts n/a (collection_not_run)\n"), "{text}");
    assert_eq!(p.json(&["health", "status"]), json!({"stream": "health", "version": {"status": "unavailable", "reason": "collection_not_run"}}));
    p.sidecar_created();
    assert_eq!(p.json(&["health", "status"]), json!({"stream": "health", "version": 1}));
}

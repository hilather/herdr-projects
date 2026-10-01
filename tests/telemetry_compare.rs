//! TM4.4 configuration comparisons and experiment reports end to end
//! (docs/telemetry/contracts-evaluation.md), through `herdr-projects
//! telemetry` on the CLI over planted canonical and sidecar rows. Expected
//! values are hand-computed from plan docs 07 §6 and 10 §5a; bootstrap bounds
//! come from an independent Python SplitMix64 oracle with exact fractions
//! (the same percentile_bootstrap.v1 rule as tests/telemetry_quality.rs);
//! none is read back from a production aggregate.

#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.

mod support;

use herdr_projects::store::SqliteStore;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf, process::Command};
use support::telemetry::*;

fn unavailable(reason: &str) -> Value { json!({"status": "unavailable", "reason": reason}) }
fn hex(seed: &str) -> String { format!("{:x}", Sha256::digest(seed.as_bytes())) }
const OID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SEED: &str = "0x544d345f636d7072";

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
    fn compare(&self, args: &[&str]) -> Value {
        let mut all = vec!["compare", "--json", "--by", "configuration"];
        all.extend_from_slice(args);
        self.json(&all)
    }
}

/// Plant an `agent_configuration.v1` row (contracts §2); `model` is a declared requested model.
fn configuration(db: &rusqlite::Connection, kind: &str, version: &str, model: Option<&str>) -> String {
    let canonical = json!({"adapter": {"digest": "a".repeat(64), "id": "sim-adapter", "revision": 1}, "agent_digest": "e".repeat(64), "agent_version": version,
        "arguments_digest": "c".repeat(64), "definition_digest": "b".repeat(64), "environment_names": ["SIM_ENV"], "kind": kind,
        "permission_policy": {"digest": "d".repeat(64), "id": "sim-policy", "revision": 1}, "reasoning_effort": null, "reasoning_effort_reason": "mapping_unverified",
        "requested_model": model, "requested_model_reason": if model.is_some() { Value::Null } else { json!("mapping_unverified") }, "schema": "agent_configuration.v1"}).to_string();
    let id = format!("sha256:{}", hex(&canonical));
    db.execute("INSERT INTO agent_configurations(configuration_id,canonical_json,first_decided_unix_ms) VALUES(?1,?2,1)", [&id, &canonical]).unwrap();
    id
}

/// One attempt: `(configuration or "" for no dispatch decision, attempt state, eligible JSON or "" for deterministic)`.
type Attempt = (String, String, String);
fn on(configuration: &str, state: &str) -> Attempt { (configuration.to_owned(), state.to_owned(), String::new()) }

/// Plant task `id` (verify_only contract, classification `class`/`band`)
/// with its attempts, lifecycle marks and dispatch decisions; `outcome`
/// `accepted` adds a submission with a verified result.
fn task(db: &rusqlite::Connection, id: &str, outcome: &str, class: &str, band: &str, attempts: &[Attempt], t0: i64) {
    let state = match outcome { "accepted" => "succeeded", "open" => "running", other => other };
    db.execute("INSERT INTO tasks(id,revision,state,title) VALUES(?1,1,?2,?1)", [id, state]).unwrap();
    db.execute("INSERT INTO task_contracts(task_id,contract_revision,project_store,expected_head,repository,base_oid,object_format,route,raw_bytes,raw_digest,installed_seq)
        VALUES(?1,1,'store',1,'/repo',?2,'sha1','verify_only',x'7b7d',?3,1)", rusqlite::params![id, OID, hex(&format!("contract-{id}"))]).unwrap();
    let classification = format!("sha256:{}", hex(&format!("class-{id}")));
    db.execute("INSERT INTO task_classifications(classification_id,task_id,contract_revision,taxonomy,class,band,features,classifier,revision,reason,created_unix_ms)
        VALUES(?1,?2,1,'taxonomy.v1',?3,?4,'{}','rule:fixture',1,NULL,?5)", rusqlite::params![classification, id, class, band, t0]).unwrap();
    let mut last = String::new();
    for (i, (config, attempt_state, eligible)) in attempts.iter().enumerate() {
        let attempt = format!("{id}-a{i}");
        let at = t0 + 10 * i as i64;
        db.execute("INSERT INTO attempts(id,task_id,revision,state,reservation,termination_observed) VALUES(?1,?2,2,?3,?1,?4)",
            rusqlite::params![attempt, id, attempt_state, i64::from(attempt_state != "running")]).unwrap();
        db.execute("INSERT INTO attempt_lifecycle(attempt_id,state,attempt_revision,unix_ms,source) VALUES(?1,'reserved',1,?2,'fixture')", rusqlite::params![attempt, at]).unwrap();
        if attempt_state != "running" {
            db.execute("INSERT INTO attempt_lifecycle(attempt_id,state,attempt_revision,unix_ms,source) VALUES(?1,?2,2,?3,'fixture')", rusqlite::params![attempt, attempt_state, at + 5]).unwrap();
        }
        if !config.is_empty() {
            let eligible = if eligible.is_empty() { json!([{"configuration_id": config, "profile_digest": "0".repeat(64), "status": "chosen", "probability_ppm": 1_000_000}]).to_string() } else { eligible.clone() };
            db.execute("INSERT INTO dispatch_decisions(attempt_id,task_id,task_revision,contract_revision,classification_id,chosen_configuration_id,eligible,chooser_kind,chooser_principal,reason_codes,note,policy,seed,decided_unix_ms)
                VALUES(?1,?2,1,1,?3,?4,?5,'operator','approval:fixture','[\"unspecified\"]',NULL,NULL,NULL,?6)", rusqlite::params![attempt, id, classification, config, eligible, at]).unwrap();
        }
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

/// Bound Codex usage for `attempt` (session `session`, certified CLI 0.154.0).
fn usage(sidecar: &rusqlite::Connection, attempt: &str, session: &str, total: i64) {
    let path = hex(&format!("path-{session}"));
    sidecar.execute("INSERT INTO rollout_sources(path_digest,home_digest,session_id,session_unix_ms,cwd,cwd_attempt,cli_version,originator,source,records,thread_usage,token_count_usage,binding,attempt_id,observed_unix_ms)
        VALUES(?1,?2,?3,1,'~/w',NULL,'0.154.0',NULL,NULL,1,NULL,NULL,'bound',?4,1)", rusqlite::params![path, hex("home"), session, attempt]).unwrap();
    sidecar.execute("INSERT INTO codex_usage(session_id,ordinal,path_digest,response_id,turn_id,model,effort,payload_digest,input_tokens,cached_input_tokens,cache_write_input_tokens,output_tokens,reasoning_output_tokens,total_tokens,accepted,reason,observed_unix_ms)
        VALUES(?1,1,?2,NULL,NULL,NULL,NULL,?3,?4,0,0,0,0,?4,1,NULL,1)", rusqlite::params![session, path, hex(&format!("payload-{session}")), total]).unwrap();
}

/// Graph node binding `session` to `attempt`, with one model segment per entry of `models`.
fn segments(sidecar: &rusqlite::Connection, attempt: &str, session: &str, models: &[&str]) {
    sidecar.execute("INSERT INTO session_graph_nodes(path_digest,session_id,role,linkage,attempt_id) VALUES(?1,?2,'primary','root',?3)",
        rusqlite::params![hex(&format!("node-{session}")), session, attempt]).unwrap();
    for (i, model) in models.iter().enumerate() {
        let n = i as i64 + 1;
        sidecar.execute("INSERT INTO model_segments(session_id,bucket,segment,model,first_position,last_position,entries,input_tokens,output_tokens,reasoning_tokens,total_tokens)
            VALUES(?1,'model',?2,?3,?2,?2,1,1,1,0,2)", rusqlite::params![session, n, model]).unwrap();
    }
}

fn arm<'a>(cell: &'a Value, configuration: &str) -> &'a Value {
    cell["arms"].as_array().unwrap().iter().find(|a| a["configuration_id"] == configuration).unwrap()
}
fn identity<'a>(report: &'a Value, configuration: &str) -> &'a Value {
    report["configurations"].as_array().unwrap().iter().find(|c| c["configuration_id"] == configuration).unwrap()
}
fn note<'a>(report: &'a Value, code: &str) -> Option<&'a Value> { report["notes"].as_array().unwrap().iter().find(|n| n["code"] == code) }
fn sorted(mut ids: Vec<String>) -> Vec<String> { ids.sort(); ids }

/// Adversarial cohort (plan doc 07 §2/§6, doc 10 §4): survivor bias, small
/// samples, uneven review coverage, missing costs, mixed-model and
/// mixed-configuration allocation, opaque model identity. By hand, class
/// `code`: arm A (codex, opaque) has a1, a2 accepted, a3, a4 failed, a5
/// cancelled: M02 = 2/5, not the survivors' 2/2; arm B (claude, declares
/// `model-x`) b1–b4 accepted: 4/4. Both below the registry minimum of 20:
/// suppressed with counts, no ranking. m1 ran on A then B (mixed
/// configuration) and u1 has no dispatch decision: members of the cohort,
/// in no arm. o1 is open (excluded). Usage is known for a1 (100 tokens) and
/// a2 (50) only: A's cost is a partial 150 over 2 of 5 attempts; B's is
/// unavailable. a1's session switched models (mixed), a2's used one. Only a1
/// was reviewed: A 1/5, B 0/4, uneven.
#[test]
fn adversarial_cohort_exposes_failures_coverage_and_allocations_without_ranking() {
    let p = Planted::new();
    p.raw(&["collect"]);
    let db = p.db();
    let a = configuration(&db, "codex", "0.154.0", None);
    let b = configuration(&db, "claude", "1.0.0", Some("model-x"));
    for (i, (id, outcome)) in [("a1", "accepted"), ("a2", "accepted"), ("a3", "failed"), ("a4", "failed"), ("a5", "cancelled")].iter().enumerate() {
        let state = match *outcome { "accepted" => "completed", other => other };
        task(&db, id, outcome, "code", "small", &[on(&a, state)], 1000 + 100 * i as i64);
    }
    for (i, id) in ["b1", "b2", "b3", "b4"].iter().enumerate() { task(&db, id, "accepted", "code", "small", &[on(&b, "completed")], 2000 + 100 * i as i64); }
    task(&db, "m1", "accepted", "code", "small", &[on(&a, "failed"), on(&b, "completed")], 3000);
    task(&db, "u1", "failed", "code", "small", &[on("", "failed")], 3100);
    task(&db, "o1", "open", "code", "small", &[on(&a, "running")], 3200);
    db.execute("INSERT INTO review_opportunities(opportunity_id,submission_id,task_id,contract_revision,candidate_oid,scope,kind,role,protocol,prior_findings,budget_ms,creator_principal,canonical_json,created_unix_ms)
        VALUES(?1,?2,'a1',1,?3,'candidate_diff','code','evaluation','review-protocol.v1','[]',NULL,'operator:cli','{}',5000)",
        rusqlite::params![format!("sha256:{}", hex("opportunity-a1")), hex("submission-a1"), OID]).unwrap();
    drop(db);
    let sidecar = p.sidecar();
    usage(&sidecar, "a1-a0", "s-a1", 100);
    usage(&sidecar, "a2-a0", "s-a2", 50);
    segments(&sidecar, "a1-a0", "s-a1", &["gpt-5-sim-a", "gpt-5-sim-b"]);
    segments(&sidecar, "a2-a0", "s-a2", &["gpt-5-sim-a"]);
    drop(sidecar);
    let state = fs::read(p.project.join(".state/state.db")).unwrap();

    let raw = p.raw(&["compare", "--json", "--metric", "M02", "--by", "configuration"]);
    let r: Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!((&r["contract"], &r["analysis"]["kind"], &r["analysis"]["causal"]), (&json!("analytics-comparison.v2"), &json!("observational"), &json!(false)));
    assert!(r["analysis"]["routing"].as_str().unwrap().starts_with("never"));
    assert_eq!(r["population"]["members"], 11);
    assert_eq!(r["population"]["allocated"], 9);
    assert_eq!(r["population"]["unallocated"], json!({"configuration_unknown": 1, "mixed_configuration": 1}));
    assert_eq!(r["population"]["exclusions"], json!({"open": 1}));

    // Survivor bias: failed and cancelled tasks stay in the denominator.
    let cell = &r["results"][0]["cells"][0];
    assert_eq!(cell["task_class"], "code");
    let (ca, cb) = (arm(cell, &a), arm(cell, &b));
    assert_eq!((&ca["tasks"], &ca["numerator"], &ca["denominator"], &ca["breakdown"]), (&json!(5), &json!(2), &json!(5), &json!({"accepted": 2, "cancelled": 1, "failed": 2})));
    assert_eq!((&ca["status"], &ca["value"], &ca["interval"], &ca["pooled"]), (&json!("suppressed"), &unavailable("insufficient_data"), &unavailable("insufficient_data"), &unavailable("insufficient_data")));
    assert_eq!(ca["min_sample"], json!({"value": 20, "unit": "terminal_tasks", "source": "analytics-comparison.v2"}));
    assert_eq!((&cb["numerator"], &cb["denominator"], &cb["status"]), (&json!(4), &json!(4), &json!("suppressed")));
    // Small samples: no ranking, in the class or overall.
    assert_eq!(cell["ranking"], json!({"status": "not_supported", "reasons": [{"insufficient_data": sorted(vec![a.clone(), b.clone()])}]}));
    assert_eq!(r["results"][0]["all_classes"]["ranking"], json!({"status": "not_supported", "reasons": ["universal_ranking_not_supported"]}));
    // Deterministic decisions: raw rates only, labelled observational.
    assert_eq!((&cell["propensity"]["reason"], &cell["propensity"]["observational"]), (&json!("deterministic_assignment"), &json!(true)));

    // Assignments and failures per arm, not only successes.
    let (ia, ib) = (identity(&r, &a), identity(&r, &b));
    assert_eq!(ia["assignments"], json!({"tasks": 5, "attempts": 5, "decisions": 5, "chooser_kind": {"operator": 5}}));
    assert_eq!(ia["outcomes"], json!({"accepted": 2, "cancelled": 1, "failed": 2}));
    assert_eq!(ia["attempt_states"], json!({"cancelled": 1, "completed": 2, "failed": 2}));
    assert_eq!((&ia["task_classes"], &ia["difficulty"]["bands"]), (&json!({"code": 5}), &json!({"small": 5})));
    // Missing costs: a labelled partial subtotal, never the total; B has none.
    assert_eq!(ia["cost"], json!({"basis": "usage_tokens", "state": "partial", "attempts": 5, "known": 2, "missing": 3, "reasons": {"not_bound": 3},
        "total_tokens": 150, "subtotal": true, "failed_and_cancelled_attempts_included": true}));
    assert_eq!((&ib["cost"]["state"], &ib["cost"]["total_tokens"], &ib["cost"]["reasons"]), (&json!("unavailable"), &Value::Null, &json!({"not_bound": 4})));
    // Uneven review coverage.
    assert_eq!((&ia["review_coverage"]["value"], &ib["review_coverage"]["value"]), (&json!("1/5"), &json!("0/4")));
    assert_eq!(note(&r, "uneven_review_coverage").unwrap()["arms"], json!(sorted(vec![a.clone(), b.clone()]).iter()
        .map(|c| json!({"configuration_id": c, "value": if *c == a { "1/5" } else { "0/4" }})).collect::<Vec<_>>()));
    assert_eq!(note(&r, "cost_partial").unwrap()["arms"].as_array().unwrap().len(), 2);
    // Mixed-model allocation: flagged, counted, never named.
    assert_eq!((&ia["model_allocation"]["mixed_model_tasks"], &ia["model_allocation"]["single_model_tasks"], &ia["model_allocation"]["unobserved_tasks"],
        &ia["model_allocation"]["mixed_model_allocation"]), (&json!(1), &json!(1), &json!(3), &json!(true)));
    assert_eq!((&ib["model_allocation"]["mixed_model_allocation"], &ib["model_allocation"]["unobserved_tasks"]), (&json!(false), &json!(4)));
    assert_eq!(note(&r, "mixed_model_allocation").unwrap()["arms"], json!([{"configuration_id": a, "tasks": 1}]));
    assert_eq!(note(&r, "unallocated_tasks").unwrap()["counts"], json!({"configuration_unknown": 1, "mixed_configuration": 1}));
    assert_eq!(note(&r, "failures_included").unwrap()["breakdown"], json!({"accepted": 7, "cancelled": 1, "failed": 3}));
    // Product-level opaque results versus a declared model.
    assert_eq!((&ia["label"], &ia["model_identity"]["status"], &ia["model_identity"]["reason"]), (&json!("codex 0.154.0"), &json!("opaque"), &json!("mapping_unverified")));
    assert_eq!(ia["declared_capabilities"]["requested_model"], unavailable("mapping_unverified"));
    assert_eq!((&ib["label"], &ib["model_identity"]), (&json!("claude 1.0.0"), &json!({"status": "declared", "requested_model": "model-x"})));
    assert_eq!(ib["environment"]["environment_names"], json!(["SIM_ENV"]));
    let text = String::from_utf8(raw.clone()).unwrap();
    assert!(!text.contains("gpt-5-sim"), "effective model names never appear");
    // Paired candidate-group analysis rides beside (no closed groups here).
    assert_eq!((&r["paired"]["definition"], &r["paired"]["value"]["reason"], &r["paired"]["pairs"]), (&json!("M42.v1"), &json!("no_closed_groups"), &json!([])));

    // Reproducible bytes, read-only, text form.
    assert_eq!(p.raw(&["compare", "--json", "--metric", "M02", "--by", "configuration"]), raw);
    assert_eq!(fs::read(p.project.join(".state/state.db")).unwrap(), state, "compare writes nothing");
    let text = String::from_utf8(p.raw(&["compare", "--metric", "M02"])).unwrap();
    assert!(text.contains("task_class=code ranking=not_supported") && text.contains("value=unavailable(insufficient_data)") && text.contains("note uneven_review_coverage"), "{text}");

    // Rejections.
    assert!(p.fail(&["compare", "--metric", "M02", "--cohort", "completed_task"]).contains("ambiguous_cohort"));
    assert!(p.fail(&["compare", "--metric", "M02", "--by", "model"]).contains("dimension_unsupported"));
    assert!(p.fail(&["compare", "--metric", "M08"]).contains("comparison_unsupported"));
    assert!(p.fail(&["compare", "--metric", "M02.slice-v1"]).contains("comparison_unsupported"));
    assert!(p.fail(&["compare", "--metric", "M02", "--cohort", "activity_window"]).contains("cohort_unsupported"));
}

/// Plan doc 10 §5a bootstrap clustering and minimum samples, doc 07 §6
/// pooling. Class `code`: A 20 tasks, 18 accepted (a-code-00 took five
/// attempts, four failed), B 20 tasks, 4 accepted. Class `docs`: A and B 10
/// of 20 each, B's half `medium`. M02 A code = 18/20, interval (oracle,
/// registry seed) [15/20, 20/20]; B code 4/20, [1/20, 8/20]: separated, so
/// the code cell ranks A over B, within the class only. Docs: difficulty
/// mixes differ, no ranking. All classes: never ranked. Pooled
/// (beta-binomial EB, κ = 10, prior = the arm's all-class rate): A 28/40 →
/// code (18·40 + 10·28)/(30·40) = 5/6, docs 17/30; B 14/40 → code 1/4,
/// docs 9/20. M07 A code = 24/18, clustered by task: [20/20, 32/17]; B
/// code 20/4, [20/8, 20/1]; lower is better: A then B.
#[test]
fn separated_intervals_rank_only_within_a_class_with_pooled_beside_raw() {
    let p = Planted::new();
    let db = p.db();
    let a = configuration(&db, "codex", "0.154.0", None);
    let b = configuration(&db, "codex", "0.155.0", None);
    let mut t = 1000;
    for (arm_id, prefix, class, accepted) in [(&a, "a-code", "code", 18), (&b, "b-code", "code", 4), (&a, "a-docs", "docs", 10), (&b, "b-docs", "docs", 10)] {
        for i in 0..20 {
            let id = format!("{prefix}-{i:02}");
            let outcome = if i < accepted { "accepted" } else { "failed" };
            let band = if prefix == "b-docs" && i >= 10 { "medium" } else { "small" };
            let last = if outcome == "accepted" { "completed" } else { "failed" };
            let attempts: Vec<Attempt> = if id == "a-code-00" { (0..5).map(|k| on(arm_id, if k < 4 { "failed" } else { "completed" })).collect() } else { vec![on(arm_id, last)] };
            task(&db, &id, outcome, class, band, &attempts, t);
            t += 100;
        }
    }
    drop(db);
    let r = p.compare(&["--metric", "M02,M07"]);
    let interval = |lower: &str, upper: &str, lower_decimal: &str, upper_decimal: &str, clusters: u32| json!({"method": "percentile_bootstrap.v1", "resample": "task",
        "clusters": clusters, "iterations": 1000, "seed": SEED, "level": "0.95", "lower": lower, "upper": upper, "lower_decimal": lower_decimal, "upper_decimal": upper_decimal,
        "source": "analytics-comparison.v2"});
    let m02 = &r["results"][0];
    let (code, docs) = (&m02["cells"][0], &m02["cells"][1]);
    assert_eq!((&code["task_class"], &docs["task_class"]), (&json!("code"), &json!("docs")));
    let (ac, bc) = (arm(code, &a), arm(code, &b));
    assert_eq!((&ac["value"], &ac["decimal"], &ac["status"]), (&json!("18/20"), &json!("0.9000"), &json!("shown")));
    assert_eq!(ac["interval"], interval("15/20", "20/20", "0.7500", "1.0000", 20));
    assert_eq!(bc["interval"], interval("1/20", "8/20", "0.0500", "0.4000", 20));
    assert_eq!(ac["pooled"], json!({"model": "beta_binomial_eb.v1", "value": "5/6", "decimal": "0.8333", "prior_mean": "28/40", "prior_strength": 10, "shrinkage": "1/3"}));
    assert_eq!((&bc["pooled"]["value"], &arm(docs, &a)["pooled"]["value"], &arm(docs, &b)["pooled"]["value"]), (&json!("1/4"), &json!("17/30"), &json!("9/20")));
    assert_eq!(code["ranking"], json!({"status": "intervals_separated", "order": [a, b], "higher_is_better": true, "scope": "this task class and cohort only",
        "universal": false, "observational": true, "causal": false, "routing": "never: advisory evidence only; nothing here is read by dispatch or admission"}));
    assert_eq!(docs["ranking"], json!({"status": "not_supported", "reasons": ["difficulty_mix_differs"]}));
    assert_eq!(arm(docs, &b)["difficulty"], json!({"medium": 10, "small": 10}));
    // All classes: raw and interval shown, never ranked; pooling is per class.
    let all = &m02["all_classes"];
    let aa = all["arms"].as_array().unwrap().iter().find(|x| x["configuration_id"] == a).unwrap();
    assert_eq!((&aa["value"], &aa["interval"], &aa["pooled"]), (&json!("28/40"), &interval("22/40", "33/40", "0.5500", "0.8250", 40), &unavailable("pooling_is_per_class")));
    assert_eq!(all["ranking"], json!({"status": "not_supported", "reasons": ["universal_ranking_not_supported", "task_class_unmatched"]}));
    // M07: whole tasks are resampled; the five-attempt task moves as one cluster.
    let m07 = &r["results"][1];
    let (ac, bc) = (arm(&m07["cells"][0], &a), arm(&m07["cells"][0], &b));
    assert_eq!((&ac["value"], &ac["interval"]), (&json!("24/18"), &interval("20/20", "32/17", "1.0000", "1.8824", 20)));
    assert_eq!((&bc["value"], &bc["interval"]), (&json!("20/4"), &interval("20/8", "20/1", "2.5000", "20.0000", 20)));
    assert_eq!(ac["pooled"], unavailable("pooling_not_declared"));
    assert_eq!((&m07["cells"][0]["ranking"]["status"], &m07["cells"][0]["ranking"]["order"]), (&json!("intervals_separated"), &json!([a, b])));
    // No sidecar: cost and model allocation unavailable, never 0.
    assert_eq!((&identity(&r, &a)["cost"]["state"], &identity(&r, &a)["cost"]["reasons"]), (&json!("unavailable"), &json!({"collection_not_run": 44})));
    assert_eq!(identity(&r, &a)["model_allocation"], unavailable("collection_not_run"));
    assert!(note(&r, "uneven_review_coverage").is_none(), "both arms 0/40 reviewed");

    // Reproducible with the recorded seed; an override is labelled and changes only the draws.
    assert_eq!(p.compare(&["--metric", "M02,M07"]), r);
    let o = p.compare(&["--metric", "M02", "--seed", "7", "--task-class", "code"]);
    assert_eq!(o["estimators"]["bootstrap"]["source"], json!({"source": "override", "registry": {"seed": SEED, "source": "analytics-comparison.v2"}}));
    let bo = arm(&o["results"][0]["cells"][0], &b);
    assert_eq!((&bo["interval"]["seed"], &bo["interval"]["lower"], &bo["interval"]["upper"]), (&json!("0x0000000000000007"), &json!(B_SEED7.0), &json!(B_SEED7.1)));
    // A class filter compares one class: the all-class row equals it and is still not a universal ranking.
    assert_eq!(o["population"]["exclusions"], json!({"task_class_filter": 40}));
    assert_eq!(o["results"][0]["cells"].as_array().unwrap().len(), 1);
    assert_eq!(o["results"][0]["all_classes"]["ranking"]["reasons"], json!(["universal_ranking_not_supported"]));

    // Doc 10 minimum samples: 19 in a cell is suppressed; the 20th unsuppresses it.
    let db = p.db();
    for i in 0..19 { task(&db, &format!("c-tests-{i:02}"), if i < 10 { "accepted" } else { "failed" }, "tests", "small", &[on(&a, if i < 10 { "completed" } else { "failed" })], 90_000 + 100 * i); }
    drop(db);
    let tests = |r: &Value| r["results"][0]["cells"].as_array().unwrap().iter().find(|c| c["task_class"] == "tests").unwrap().clone();
    let cell = tests(&p.compare(&["--metric", "M02"]));
    assert_eq!((&arm(&cell, &a)["status"], &arm(&cell, &a)["numerator"], &arm(&cell, &a)["denominator"]), (&json!("suppressed"), &json!(10), &json!(19)));
    let db = p.db();
    task(&db, "c-tests-19", "failed", "tests", "small", &[on(&a, "failed")], 99_000);
    drop(db);
    let cell = tests(&p.compare(&["--metric", "M02"]));
    assert_eq!((&arm(&cell, &a)["status"], &arm(&cell, &a)["value"]), (&json!("shown"), &json!("10/20")));
    assert_eq!(cell["ranking"], json!({"status": "not_supported", "reasons": ["single_arm"]}));
}

/// Doc 10 §5a propensity labelling: deterministic decisions give no weighted
/// estimate; logged positive probabilities give the Hájek estimate with its
/// effective sample size. Class `tests`: A chosen on 20 tasks, 10 under
/// {A 0.5, B 0.5} (8 accepted) and 10 under {A 0.8, B 0.2} (4 accepted);
/// weights 1/p ∝ 8 : 5, so A = (8·8 + 4·5)/(10·8 + 10·5) = 42/65, ESS =
/// 130²/(10·64 + 10·25) = 18.99. B chosen on 20, 10 under 0.5 and 10 under
/// 0.2, 5 accepted each: weights 2 : 5, B = (5·2 + 5·5)/70 = 1/2, ESS =
/// 70²/(10·4 + 10·25) = 16.90. One deterministic decision in the cell
/// breaks positivity.
#[test]
fn propensity_weighted_estimates_only_with_logged_positive_probabilities() {
    let p = Planted::new();
    let db = p.db();
    let a = configuration(&db, "codex", "0.154.0", None);
    let b = configuration(&db, "codex", "0.155.0", None);
    let eligible = |chosen: &str, pa: i64| json!([{"configuration_id": a, "profile_digest": "0".repeat(64), "status": if chosen == a { "chosen" } else { "not_evaluated" }, "probability_ppm": pa},
        {"configuration_id": b, "profile_digest": "1".repeat(64), "status": if chosen == b { "chosen" } else { "not_evaluated" }, "probability_ppm": 1_000_000 - pa}]).to_string();
    let mut t = 1000;
    for (arm_id, prefix, groups) in [(&a, "a", [(500_000, 8), (800_000, 4)]), (&b, "b", [(500_000, 5), (800_000, 5)])] {
        for (g, (pa, accepted)) in groups.iter().enumerate() {
            for i in 0..10 {
                let outcome = if i < *accepted { "accepted" } else { "failed" };
                task(&db, &format!("{prefix}-{g}-{i:02}"), outcome, "tests", "small",
                    &[(arm_id.to_string(), if outcome == "accepted" { "completed" } else { "failed" }.to_owned(), eligible(arm_id, *pa))], t);
                t += 100;
            }
        }
    }
    drop(db);
    let r = p.compare(&["--metric", "M02"]);
    let cell = &r["results"][0]["cells"][0];
    assert_eq!(cell["propensity"], json!({"status": "available", "method": "hajek_ipw.v1"}));
    assert_eq!((&arm(cell, &a)["value"], &arm(cell, &a)["propensity_weighted"]), (&json!("12/20"),
        &json!({"method": "hajek_ipw.v1", "value": "42/65", "decimal": "0.6462", "tasks": 20, "effective_sample_size": "18.99", "observational": true})));
    assert_eq!((&arm(cell, &b)["value"], &arm(cell, &b)["propensity_weighted"]["value"], &arm(cell, &b)["propensity_weighted"]["effective_sample_size"]),
        (&json!("10/20"), &json!("1/2"), &json!("16.90")));
    assert_eq!(r["analysis"]["causal"], false, "weighted estimates stay observational");
    let db = p.db();
    task(&db, "a-9-00", "failed", "tests", "small", &[on(&a, "failed")], 90_000);
    drop(db);
    let cell = p.compare(&["--metric", "M02"])["results"][0]["cells"][0].clone();
    assert_eq!(cell["propensity"], json!({"status": "unavailable", "reason": "positivity_violated", "decisions_without_positive_probability": 1}));
    assert_eq!(arm(&cell, &a)["propensity_weighted"], cell["propensity"]);
}

/// Interval bounds of B code M02 with `--seed 7` (independent oracle).
const B_SEED7: (&str, &str) = ("1/20", "8/20");

/// Sample-size planning golden (contracts-evaluation.md §6). p₁ = 0.5, d =
/// 0.1, α = 0.05, power 0.80: unpaired n = ⌈(1.959963985·√0.495 +
/// 0.841621234·√0.49)² / 0.01⌉ = ⌈387.34⌉ = 388 per arm (776 tasks); paired
/// with independent arms ψ = 0.5·0.4 + 0.6·0.5 = 0.5: ⌈(1.959963985·√0.5 +
/// 0.841621234·√0.49)²/0.01⌉ = ⌈390.08⌉ = 391 groups; a declared ψ = 0.2
/// gives ⌈154.60⌉ = 155. Clustered, m = 5, ρ = 0.05: design effect 1.2,
/// ⌈465.6⌉ = 466 per arm and, with ψ = 0.2, ⌈186.0⌉ = 186 groups.
#[test]
fn experiment_sample_size_plan_golden() {
    let p = Planted::new();
    let plan = p.json(&["experiments", "plan", "--json", "--metric", "M02", "--baseline-rate", "0.5", "--min-detectable-effect", "0.1"]);
    assert_eq!((&plan["contract"], &plan["metric"], &plan["definition"]), (&json!("experiment-planning.v1"), &json!("M02"), &json!("M02.cohort-v1")));
    assert_eq!(plan["inputs"], json!({"baseline_rate": "0.500000000", "min_detectable_effect": "0.100000000", "alternative_rate": "0.600000000", "alpha": "0.050000000",
        "two_sided": true, "power": "0.800000000"}));
    assert_eq!(plan["z"], json!({"alpha": "1.959963985", "power": "0.841621234"}));
    assert_eq!((&plan["unpaired"]["per_arm"], &plan["unpaired"]["total_tasks"]), (&json!(388), &json!(776)));
    assert_eq!((&plan["paired"]["pairs"], &plan["paired"]["discordance"]), (&json!(391), &json!({"value": "0.500000000", "source": "assumed_independent_arms"})));
    assert_eq!(plan["clustered"], Value::Null);
    let declared = p.json(&["experiments", "plan", "--json", "--metric", "M02", "--baseline-rate", "0.5", "--min-detectable-effect", "0.1", "--discordance", "0.2",
        "--cluster-size", "5", "--icc", "0.05"]);
    assert_eq!((&declared["paired"]["pairs"], &declared["paired"]["discordance"]["source"]), (&json!(155), &json!("declared")));
    assert_eq!(declared["clustered"], json!({"cluster_size": 5, "icc": "0.050000000", "design_effect": "1.200000000", "formula": "n × (1 + (m − 1)ρ), ceiling",
        "unpaired_per_arm": 466, "unpaired_total_tasks": 932, "paired_pairs": 186}));
    // 0.9 power, α = 0.01: ⌈(2.575829304·√0.495 + 1.281551566·0.7)²/0.01⌉ = ⌈734.05⌉.
    assert_eq!(p.json(&["experiments", "plan", "--json", "--metric", "M02", "--baseline-rate", "0.5", "--min-detectable-effect", "0.1", "--alpha", "0.01", "--power", "0.9"])
        ["unpaired"]["per_arm"], json!(STRICT_PER_ARM));
    let text = String::from_utf8(p.raw(&["experiments", "plan", "--metric", "M02", "--baseline-rate", "0.5", "--min-detectable-effect", "0.1"])).unwrap();
    assert_eq!(text, "experiment-planning.v1 M02 baseline=0.500000000 effect=0.100000000 alpha=0.050000000 power=0.800000000 unpaired_per_arm=388 unpaired_total=776 paired_pairs=391 discordance=0.500000000(assumed_independent_arms)\n");
    assert!(p.fail(&["experiments", "plan", "--metric", "M02", "--baseline-rate", "0.5", "--min-detectable-effect", "0.1", "--alpha", "0.2"]).contains("unsupported_alpha"));
    assert!(p.fail(&["experiments", "plan", "--metric", "M01", "--baseline-rate", "0.5", "--min-detectable-effect", "0.1"]).contains("not_a_rate_metric"));
    assert!(p.fail(&["experiments", "plan", "--metric", "M02", "--baseline-rate", "0.95", "--min-detectable-effect", "0.1"]).contains("effect_out_of_range"));
    // The registry publishes the comparison estimators.
    let registry = p.json(&["metrics", "registry", "--json"]);
    assert_eq!((&registry["comparison"]["version"], &registry["comparison"]["bootstrap"]["seed"], &registry["comparison"]["min_sample"]["value"]),
        (&json!("analytics-comparison.v2"), &json!(SEED), &json!(20)));
}

const STRICT_PER_ARM: u64 = 735;

// D4 preregistered experiment reports (contracts-review.md §7) over the review CLI.

fn review_hex(c: char) -> String { c.to_string().repeat(64) }
fn review_oid(c: char) -> String { c.to_string().repeat(40) }
fn evidence(c: char) -> String { format!("sha256:{}", c.to_string().repeat(64)) }
const SKEPTICAL: &str = "skeptical-challenge.v1";

fn input(f: &Fixture, name: &str, body: &Value) -> String {
    let path = f.tmp.path().join(name);
    fs::write(&path, body.to_string()).unwrap();
    path.to_str().unwrap().to_owned()
}

/// Submissions S1–S4 (candidates 1…1 to 4…4), each on its own task w1–w4 by its author attempt,
/// and completed reviewer attempts; `fast` is the reviewer profile.
fn review_world(f: &Fixture, reviewers: &[&str]) {
    let db_path = f.project.join(".state/state.db");
    let mut fast = codex_profile(&f.config, "codex", "fast", Some(&f.tmp.path().join("fast-home")));
    fast.arguments_digest = "1".repeat(64);
    plant_profile(&db_path, fast);
    let db = rusqlite::Connection::open(&db_path).unwrap();
    for (i, c) in ['1', '2', '3', '4'].into_iter().enumerate() {
        let task = format!("w{c}");
        db.execute("INSERT INTO tasks(id,revision,state,title) VALUES(?1,1,'running',?1)", [&task]).unwrap();
        db.execute("INSERT INTO attempts(id,task_id,revision,state,reservation,termination_observed) VALUES(?1,?2,1,'completed',?1,1)", [format!("author-{task}"), task.clone()]).unwrap();
        db.execute("INSERT INTO task_contracts(task_id,contract_revision,plan_revision,project_store,expected_head,repository,base_oid,object_format,memory_snapshot_id,route,raw_bytes,raw_digest,installed_seq)
            VALUES(?1,1,NULL,'store',0,'/repo',?2,'sha1',NULL,'verify_only',x'61',?3,(SELECT max(sequence) FROM events))", rusqlite::params![task, review_oid('b'), review_hex('c')]).unwrap();
        db.execute("INSERT INTO result_submissions(submission_id,project_store,idempotency_key,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,artifact_manifest,claimed_checks,created_unix_ms)
            VALUES(?1,'store',?1,?2,'{}',?3,1,?2,?4,'/repo',?5,?6,'sha1','[]','[]',?7)", rusqlite::params![review_hex(c), review_hex('d'), task, format!("author-{task}"), review_oid('b'), review_oid(c), 1000 + i as i64]).unwrap();
    }
    for attempt in reviewers {
        db.execute("INSERT INTO attempts(id,task_id,revision,state,reservation,termination_observed) VALUES(?1,'work',1,'completed',?1,1)", [attempt]).unwrap();
    }
}

fn open_review(f: &Fixture, c: char, kind: &str, protocol: &str, budget: Option<&str>) -> String {
    let sub = review_hex(c);
    let mut args = vec!["review", "open", sub.as_str(), "--kind", kind, "--protocol", protocol];
    if let Some(budget) = budget { args.extend(["--budget-ms", budget]); }
    f.cli_args(&args).0["opportunity"]["opportunity_id"].as_str().unwrap().to_owned()
}

fn run_review(f: &Fixture, opportunity: &str, c: char, attempt: &str, findings: Value, evidence: Value) {
    f.cli_args(&["review", "assign", opportunity, "--reviewer", "fast"]);
    let session = f.cli_args(&["review", "start", opportunity, "--attempt", attempt]).0["session"]["session_id"].as_str().unwrap().to_owned();
    let receipt = json!({"schema": "review_receipt.v1", "session_id": session, "submission_id": review_hex(c), "candidate_oid": review_oid(c), "outcome": "completed",
        "findings": findings, "evidence": evidence});
    f.cli_args(&["review", "complete", "--input-file", &input(f, &format!("{attempt}.json"), &receipt)]);
}

/// The randomized experiment of contracts-review.md §7 with its four units
/// on four tasks (seed a…a assigns S1, S3 to `standard` and S2, S4 to
/// `skeptical`). Outcomes by hand: S1 1, S3 0 (standard, mean 1/2); S2 2,
/// S4 1 (skeptical, mean 3/2): the intention-to-treat difference is 1 with
/// both arms at the preregistered minimum of 2, so it is a causal estimate
/// of the assigned arm. Interval (task-clustered within arm, oracle): 0 to
/// 2. Excluding S3 leaves the standard arm below the minimum: no estimate,
/// no causal label; the unit stays listed as excluded.
#[test]
fn experiment_report_is_intention_to_treat_and_causal_only_at_the_preregistered_minimum() {
    let f = Fixture::new();
    // Before any experiment: an empty report, never a value.
    assert_eq!(f.cli_args(&["experiments", "report", "--json"]).0["experiments"], json!([]));
    review_world(&f, &["rev-b1", "rev-b2", "rev-b3", "rev-b4", "rev-k2", "rev-k4"]);
    let protocol = json!({"schema": "review_protocol.v1", "protocol": SKEPTICAL, "kind": "skeptical", "scope": "candidate_diff", "role": "evaluation",
        "challenges": ["unsupported_claims"], "failure_classes": ["logic"], "permitted_tools": ["read"], "budget_ms": 1_800_000, "evidence_min": 1,
        "stopping_rule": "checklist_complete", "prior_disclosure": "withheld", "reviewer_profile": null,
        "outcome": {"primary": "new_validated_unique_findings.v1", "adjudication": "owner_triage.v1", "severity_policy": "finding_severity.v1", "min_severity": "low"}});
    f.cli_args(&["review", "protocols", "register", "--input-file", &input(&f, "protocol.json", &protocol)]);
    let experiment = json!({"schema": "review_experiment.v1", "experiment": "skeptical-vs-standard.v1", "design": "randomized", "seed": review_hex('a'),
        "eligibility": {"kind": "code", "scope": "candidate_diff", "role": "evaluation", "protocol": "review-protocol.v1"},
        "arms": [{"arm": "standard", "protocol": null}, {"arm": "skeptical", "protocol": SKEPTICAL}],
        "primary_outcome": "validated_unique_findings.v1", "adjudication": "owner_triage.v1", "horizon_days": 14, "min_units": 2, "stopping_rule": "fixed_horizon", "planned_units": 4});
    f.cli_args(&["review", "experiments", "register", "--input-file", &input(&f, "experiment.json", &experiment)]);
    let base: Vec<String> = ['1', '2', '3', '4'].into_iter().map(|c| open_review(&f, c, "code", "review-protocol.v1", None)).collect();
    let arms: Vec<String> = base.iter().map(|b| f.cli_args(&["review", "experiments", "assign", "skeptical-vs-standard.v1", b]).0["event"]["subject"]["arm"].as_str().unwrap().to_owned()).collect();
    assert_eq!(arms, ["standard", "skeptical", "standard", "skeptical"]);
    run_review(&f, &base[0], '1', "rev-b1", json!(["finding:b1"]), json!([]));
    run_review(&f, &base[1], '2', "rev-b2", json!(["finding:b2"]), json!([]));
    run_review(&f, &base[2], '3', "rev-b3", json!([]), json!([]));
    run_review(&f, &base[3], '4', "rev-b4", json!(["finding:b4"]), json!([]));
    let p2 = open_review(&f, '2', "skeptical", SKEPTICAL, Some("1800000"));
    let p4 = open_review(&f, '4', "skeptical", SKEPTICAL, Some("1800000"));
    f.cli_args(&["review", "protocols", "bind", &p2, "--prior", &base[1]]);
    f.cli_args(&["review", "protocols", "bind", &p4, "--prior", &base[3]]);
    run_review(&f, &p2, '2', "rev-k2", json!(["finding:k2-new"]), json!([evidence('a')]));
    run_review(&f, &p4, '4', "rev-k4", json!([]), json!([evidence('a')]));
    for claim in ["1", "2", "3", "4"] { f.cli_args(&["review", "findings", "validate", claim, "--new", "--severity", "medium", "--evidence", &evidence('e')]); }

    let report = f.cli_args(&["experiments", "report", "--json"]).0;
    assert_eq!(report["contract"], "experiment-report.v1");
    let e = &report["experiments"][0];
    assert_eq!((&e["experiment"], &e["design"], &e["analysis"], &e["reference_arm"], &e["min_units"], &e["label"]),
        (&json!("skeptical-vs-standard.v1"), &json!("randomized"), &json!("intention_to_treat"), &json!("standard"), &json!(2), &json!("causal")));
    assert_eq!((&e["assignment_seed"], &e["preregistered"]["primary_outcome"]), (&json!(review_hex('a')), &json!("validated_unique_findings.v1")));
    assert_eq!((&e["arms"]["standard"]["mean"], &e["arms"]["skeptical"]["mean"]), (&json!("1/2"), &json!("3/2")));
    assert_eq!(e["units"]["by_arm_status"], json!({"skeptical": {"analyzable": 2}, "standard": {"analyzable": 2}}));
    let d = &e["differences"]["skeptical"];
    assert_eq!((&d["value"], &d["analyzable"]), (&json!("1"), &json!([2, 2])));
    assert_eq!(d["interval"], json!({"method": "percentile_bootstrap.v1", "resample": "task_within_arm", "clusters": [2, 2], "iterations": 1000, "seed": SEED,
        "level": "0.95", "lower": "0", "upper": "2", "lower_decimal": "0.0000", "upper_decimal": "2.0000", "source": "analytics-comparison.v2"}));
    assert_eq!((&d["causal"]["status"], &d["causal"]["design"], &d["causal"]["analysis"]), (&json!("causal_estimate"), &json!("randomized"), &json!("intention_to_treat")));
    assert!(e["routing"].as_str().unwrap().starts_with("never"));
    let text = f.text(&["experiments", "report"]);
    assert!(text.contains("skeptical-vs-standard.v1 design=randomized label=causal reference=standard") && text.contains("skeptical difference=\"1\" interval=[0, 2] causal=causal_estimate"), "{text}");

    // Below the preregistered minimum: listed, excluded, no estimate and no causal label.
    f.cli_args(&["review", "experiments", "exclude", "skeptical-vs-standard.v1", &base[2], "--reason", "operator_error"]);
    let e = f.cli_args(&["experiments", "report", "--json"]).0["experiments"][0].clone();
    assert_eq!((&e["label"], &e["arms"]["standard"]["excluded"], &e["units"]["by_arm_status"]["standard"]), (&json!("descriptive"), &json!({"operator_error": 1}), &json!({"analyzable": 1, "excluded": 1})));
    let d = &e["differences"]["skeptical"];
    assert_eq!((&d["value"], &d["interval"], &d["causal"]), (&unavailable("insufficient_data"), &unavailable("insufficient_data"),
        &json!({"status": "unavailable", "reason": "insufficient_data", "min_units": 2})));
}

/// Real collect/refresh/compare workflow: late producer inputs invalidate the
/// supplemental body even before sync, and warm/cold CLI bytes always agree.
#[test]
fn maintained_comparison_never_serves_a_stale_body_after_late_collection() {
    use std::io::Write;
    let f = Fixture::new();
    let path = f.rollout(&f.home, "compare-cache", &["head.jsonl"], &f.worktree(), f.decided + 1, "0.154.0");
    f.cli("collect");
    plant_aggregate_termination(&f);
    f.cli_args(&["accounting", "sync"]);
    let args = ["compare", "--metric", "M02", "--json"];
    let (initial, cold) = f.cli_args(&args);
    assert_eq!(initial["configurations"][0]["cost"]["total_tokens"], 1120);
    f.cli_args(&["analytics", "refresh"]);
    assert_eq!(f.cli_args(&args).1, cold);
    assert_eq!(f.sidecar().query_row("SELECT count(*) FROM analytics_provider_rows WHERE provider='comparison'", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
    // Poison the old body to make any stale serving observable. The collector
    // advances real input generations when it consumes the appended record.
    f.sidecar().execute("UPDATE analytics_provider_rows SET body='{\"stale\":true}',suffix='',m40_revision=NULL,m40_offset=NULL,m40_bytes=NULL WHERE provider='comparison'", []).unwrap();
    let tail = fs::read_to_string(std::path::Path::new(FIXTURES).join("tail.jsonl")).unwrap()
        .replace("@SID@", SID).replace("@CWD@", &f.worktree())
        .replace("@TS@", &jiff::Timestamp::from_millisecond(f.decided + 2).unwrap().to_string()).replace("@VERSION@", "0.154.0");
    fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(tail.as_bytes()).unwrap();
    f.cli("collect");
    let (late, late_bytes) = f.cli_args(&args);
    assert!(late.get("stale").is_none());
    assert_eq!(late["configurations"][0]["cost"]["total_tokens"], 1680);
    assert_ne!(late_bytes, cold, "the real appended usage must change arm diagnostics");
    f.sidecar().execute("DELETE FROM analytics_provider_rows WHERE provider='comparison'", []).unwrap();
    assert_eq!(f.cli_args(&args).1, late_bytes);
    f.cli_args(&["accounting", "sync"]);
    let synced = f.cli_args(&args).1;
    f.cli_args(&["analytics", "refresh"]);
    assert_eq!(f.cli_args(&args).1, synced);
    // A model-only correction invalidates supplemental evidence, while
    // independently pinned metric revisions remain reproducible.
    let mut pinned = f.cli_args(&["query", "--metric", "M02", "--as-of-seq", "1000000", "--json"]).0;
    pinned.as_object_mut().unwrap().remove("query_unix_ms");
    f.sidecar().execute("UPDATE model_segments SET model='fixture-corrected-model' WHERE bucket='model'", []).unwrap();
    let changed = f.cli_args(&args).1;
    f.sidecar().execute("DELETE FROM analytics_provider_rows WHERE provider='comparison'", []).unwrap();
    assert_eq!(f.cli_args(&args).1, changed);
    f.cli_args(&["analytics", "refresh"]);
    rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap()
        .execute("INSERT INTO task_classifications(classification_id,task_id,contract_revision,taxonomy,class,band,features,classifier,revision,reason,created_unix_ms)
            SELECT ?1,task_id,contract_revision,taxonomy,class,'large',features,classifier,revision+1,'fixture correction',created_unix_ms+1
            FROM task_classifications ORDER BY created_unix_ms DESC LIMIT 1", [format!("sha256:{}", hex("classification-correction"))]).unwrap();
    let canonical_changed = f.cli_args(&args).1;
    assert_ne!(canonical_changed, changed);
    f.sidecar().execute("DELETE FROM analytics_provider_rows WHERE provider='comparison'", []).unwrap();
    assert_eq!(f.cli_args(&args).1, canonical_changed);
    let mut historical = f.cli_args(&["query", "--metric", "M02", "--as-of-seq", "1000000", "--json"]).0;
    historical.as_object_mut().unwrap().remove("query_unix_ms");
    assert_eq!(historical, pinned);
}

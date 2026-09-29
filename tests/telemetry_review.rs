//! Lane D review capture end to end (contracts-review.md): opportunities bound
//! to exact candidates, blind assignment, sessions and completions through
//! `herdr-projects telemetry <slug> review ...`, over a real reserved attempt
//! with planted submissions and reviewer attempts.

#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.

mod support;

use herdr_projects::{domain::agent_configuration, store::{FindingTarget, RepairAssignment, SqliteStore, TriageOutcome, TriageRequest}};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs;
use support::telemetry::*;

fn unavailable(reason: &str) -> serde_json::Value { json!({"status": "unavailable", "reason": reason}) }

/// Author attempt `f.attempt` (Codex) submitted S1 (candidate 1…1) and S2
/// (2…2). Three opportunities: O1 code on S1 (blind: fast vs claude), O2
/// security on S2 (blind: fast only), O3 test on S1 (never assigned). O1's
/// session completes with no findings; O2 has no session. By hand: M20 =
/// completed / assigned = 1/2 (O3 is unassigned, not in the denominator),
/// `completed_empty` 1, `no_session` 1; O1's findings are 0, O2's are
/// unavailable, never 0. After O2's only session times out with one finding,
/// M20 stays 1/2 and O2's findings stay unavailable.
#[test]
fn empty_review_counts_and_wrong_candidate_receipt_fails() {
    let f = Fixture::new();
    let db_path = f.project.join(".state/state.db");
    let mut fast = codex_profile(&f.config, "codex", "fast", Some(&f.tmp.path().join("fast-home")));
    fast.arguments_digest = "1".repeat(64);
    let claude = codex_profile(&f.config, "claude", "claude", None);
    let (fast_id, claude_id) = (agent_configuration(&fast).id, agent_configuration(&claude).id);
    let author_id = agent_configuration(&codex_profile(&f.config, "codex", "codex", Some(&f.home))).id;
    plant_profile(&db_path, fast);
    plant_profile(&db_path, claude);
    let hex = |c: char| c.to_string().repeat(64);
    let (s1, s2, oid1, oid2) = (hex('1'), hex('2'), "1".repeat(40), "2".repeat(40));
    {
        // The factory side, as a contract install, result submissions and reviewer launches would write it.
        let db = rusqlite::Connection::open(&db_path).unwrap();
        db.execute("INSERT INTO task_contracts(task_id,contract_revision,plan_revision,project_store,expected_head,repository,base_oid,object_format,memory_snapshot_id,route,raw_bytes,raw_digest,installed_seq)
            VALUES('work',1,NULL,'store',0,'/repo',?1,'sha1',NULL,'verify_only',x'61',?2,(SELECT max(sequence) FROM events))", rusqlite::params!["b".repeat(40), hex('c')]).unwrap();
        for (sub, oid, at) in [(&s1, &oid1, 1_000), (&s2, &oid2, 2_000)] {
            db.execute("INSERT INTO result_submissions(submission_id,project_store,idempotency_key,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,artifact_manifest,claimed_checks,created_unix_ms)
                VALUES(?1,'store',?1,?2,'{}','work',1,?2,?3,'/repo',?4,?5,'sha1','[]','[]',?6)", rusqlite::params![sub, hex('d'), f.attempt, "b".repeat(40), oid, at]).unwrap();
        }
        for attempt in ["rev-a1", "rev-a2"] {
            db.execute("INSERT INTO attempts(id,task_id,revision,state,reservation,termination_observed) VALUES(?1,'work',1,'completed',?1,1)", [attempt]).unwrap();
        }
    }
    let receipt = |name: &str, body: serde_json::Value| {
        let path = f.tmp.path().join(name);
        fs::write(&path, body.to_string()).unwrap();
        path.to_str().unwrap().to_owned()
    };
    let completions = || rusqlite::Connection::open(&db_path).unwrap().query_row("SELECT count(*) FROM review_completions", [], |r| r.get::<_, i64>(0)).unwrap();

    // No opportunity yet: an empty denominator is null, not 0; discovery-credit metrics are unavailable.
    let empty = f.cli_args(&["review", "report"]).0["metrics"].clone();
    assert_eq!((&empty["M20"]["value"], &empty["M20"]["reason"], &empty["M20"]["denominator"]), (&json!(null), &json!("empty_denominator"), &json!(0)));
    for id in ["M22", "M23"] { assert_eq!((&empty[id]["value"], &empty[id]["reason"], &empty[id]["pending"]), (&json!(null), &json!("empty_denominator"), &json!(0)), "{id}"); }
    // No validated finding: discovery credit is an observed 0; review cost is not allocated.
    assert_eq!((&empty["M21"]["value"], &empty["M21"]["unallocated"]), (&json!("0"), &json!("0")));
    assert_eq!(empty["M24"]["value"], unavailable("review_cost_unallocated"));
    for id in ["M25", "M26", "M27", "M29"] { assert_eq!((&empty[id]["value"], &empty[id]["reason"]), (&json!(null), &json!("empty_denominator")), "{id}"); }

    // O1 binds S1's exact task, contract revision and candidate.
    let o1 = f.cli_args(&["review", "open", &s1, "--kind", "code", "--protocol", "review-protocol.v1", "--prior-finding", "finding:known-1"]).0["opportunity"].clone();
    let created = o1["created_unix_ms"].as_i64().unwrap();
    let canonical = format!(r#"{{"budget_ms":null,"candidate_oid":"{oid1}","contract_revision":1,"created_unix_ms":{created},"kind":"code","prior_findings":["finding:known-1"],"protocol":"review-protocol.v1","role":"evaluation","schema":"review_opportunity.v1","scope":"candidate_diff","submission_id":"{s1}","task_id":"work"}}"#);
    let o1_id = format!("sha256:{:x}", Sha256::digest(canonical.as_bytes()));
    assert_eq!((&o1["opportunity_id"], &o1["task_id"], &o1["contract_revision"], &o1["candidate_oid"]), (&json!(o1_id), &json!("work"), &json!(1), &json!(oid1)));
    assert!(f.cli_fail(&["review", "open", &hex('9'), "--protocol", "review-protocol.v1"]).contains("no result submission"));
    assert!(f.cli_fail(&["review", "open", &s1, "--protocol", "Free text"]).contains("lowercase identifier"));
    assert!(f.cli_fail(&["review", "open", &s1, "--protocol", "p.v1", "--prior-finding", "SQL injection in login"]).contains("not an allowed reference"));

    // Blind cross-provider: the author is Codex (openai), so Claude is chosen over fast; same family is a covariate.
    let a1 = f.cli_args(&["review", "assign", &o1_id, "--blind", "--candidate", "fast", "--candidate", "claude"]).0["assignment"].clone();
    assert_eq!((&a1["policy"], &a1["reviewer_configuration_id"], &a1["reviewer_family"], &a1["author_attempt_id"], &a1["author_configuration_id"], &a1["author_family"]),
        (&json!("blind_cross_provider.v1"), &json!(claude_id), &json!("anthropic"), &json!(f.attempt), &json!(author_id), &json!("openai")));
    assert_eq!((&a1["same_family"], &a1["blind"], &a1["reason"]), (&json!(false), &json!(true), &json!("cross_provider")));
    assert_eq!(a1["eligible"].as_array().unwrap().iter().map(|e| (e["configuration_id"].as_str().unwrap(), e["status"].as_str().unwrap())).collect::<Vec<_>>(),
        [(fast_id.as_str(), "same_family"), (claude_id.as_str(), "chosen")]);
    assert!(f.cli_fail(&["review", "assign", &o1_id, "--reviewer", "fast"]).contains("already assigned"));
    // The blind presentation carries the exact candidate, never the author.
    let presented = f.text(&["review", "present", &o1_id]);
    assert!(presented.contains(&oid1) && !presented.contains(&f.attempt) && !presented.contains(&author_id), "{presented}");

    let o2_id = f.cli_args(&["review", "open", &s2, "--kind", "security", "--protocol", "review-protocol.v1"]).0["opportunity"]["opportunity_id"].as_str().unwrap().to_owned();
    let a2 = f.cli_args(&["review", "assign", &o2_id, "--blind", "--candidate", "fast"]).0["assignment"].clone();
    assert_eq!((&a2["reviewer_configuration_id"], &a2["same_family"], &a2["reason"]), (&json!(fast_id), &json!(true), &json!("no_cross_provider_eligible")));
    let o3_id = f.cli_args(&["review", "open", &s1, "--kind", "test", "--protocol", "review-protocol.v1"]).0["opportunity"]["opportunity_id"].as_str().unwrap().to_owned();
    assert!(f.cli_fail(&["review", "start", &o3_id, "--attempt", "rev-a1"]).contains("is not assigned"));

    // Session: the controller records who reviews; rev-a1 ran the assigned configuration.
    // rev-a1 ran the assigned Claude configuration (its launch wrote this decision); rev-a2 predates the dispatch log.
    rusqlite::Connection::open(&db_path).unwrap().execute("INSERT INTO dispatch_decisions(attempt_id,task_id,task_revision,contract_revision,chosen_configuration_id,eligible,chooser_kind,chooser_principal,reason_codes,decided_unix_ms)
        VALUES('rev-a1','work',1,1,?1,'[\"x\"]','operator','operator:cli','[\"x\"]',1)", [&claude_id]).unwrap();
    let session = f.cli_args(&["review", "start", &o1_id, "--attempt", "rev-a1"]).0["session"].clone();
    assert_eq!((&session["ordinal"], &session["configuration_id"], &session["matches_assignment"], &session["same_attempt_as_author"]), (&json!(1), &json!(claude_id), &json!(true), &json!(false)));
    let sid = session["session_id"].as_str().unwrap().to_owned();
    assert!(f.cli_fail(&["review", "start", &o1_id, "--attempt", "rev-a2"]).contains("has no completion yet"));

    // Wrong-candidate receipts fail: another submission, or the right submission at another tree.
    let good = json!({"schema": "review_receipt.v1", "session_id": sid, "submission_id": s1, "candidate_oid": oid1, "outcome": "completed", "findings": [], "evidence": []});
    for (name, field, value) in [("other-sub.json", "submission_id", json!(s2)), ("other-oid.json", "candidate_oid", json!(oid2))] {
        let mut wrong = good.clone();
        wrong[field] = value;
        assert!(f.cli_fail(&["review", "complete", "--input-file", &receipt(name, wrong)]).contains("names another candidate"), "{name}");
    }
    // A worker cannot carry acceptance or content in its receipt.
    for (name, field, value) in [("forged.json", "accepted", json!(true)), ("trust.json", "trust", json!("accepted")), ("content.json", "summary", json!("SENTINEL-REVIEW-TEXT"))] {
        let mut forged = good.clone();
        forged[field] = value;
        assert!(f.cli_fail(&["review", "complete", "--input-file", &receipt(name, forged)]).contains(&format!("unknown field `{field}`")), "{name}");
    }
    assert_eq!(completions(), 0, "refused receipts write nothing");

    // An empty completed review is a real 0, recorded as a proposal with declared coverage; replay is idempotent.
    let path = receipt("good.json", good);
    let done = f.cli_args(&["review", "complete", "--input-file", &path]).0["completion"].clone();
    assert_eq!((&done["outcome"], &done["findings_submitted"], &done["trust"], &done["coverage_basis"], &done["replayed"]), (&json!("completed"), &json!(0), &json!("proposal"), &json!("declared"), &json!(false)));
    assert_eq!(f.cli_args(&["review", "complete", "--input-file", &path]).0["completion"]["replayed"], json!(true));
    assert_eq!(completions(), 1);
    assert!(f.cli_fail(&["review", "start", &o1_id, "--attempt", "rev-a2"]).contains("already completed"));

    // Acceptance is inactive: a worker principal is refused as a worker, the operator as inactive, raw rows by the trigger.
    let mut store = SqliteStore::open(&db_path).unwrap();
    for worker in ["worker:rev-a1", "rev-a1", f.attempt.as_str()] {
        assert!(format!("{:?}", store.accept_review(&sid, worker).unwrap_err()).contains("a worker cannot accept"), "{worker}");
    }
    drop(store);
    assert!(f.cli_fail(&["review", "accept", &sid]).contains("review acceptance is inactive: no_reviewer_authority_producer"));
    let raw = rusqlite::Connection::open(&db_path).unwrap().execute("INSERT INTO review_acceptances VALUES(?1,'accepted','operator:cli','x',1)", [&sid]).unwrap_err();
    assert!(raw.to_string().contains("review acceptance is inactive"), "{raw}");

    let shown = f.cli_args(&["review", "show"]).0;
    assert_eq!(shown["acceptance"], json!({"active": false, "reason": "no_reviewer_authority_producer"}));
    let by_id = |id: &str| shown["opportunities"].as_array().unwrap().iter().find(|o| o["opportunity_id"] == id).unwrap().clone();
    assert_eq!((&by_id(&o1_id)["status"], &by_id(&o1_id)["findings_submitted"]), (&json!("completed"), &json!(0)));
    assert_eq!((&by_id(&o2_id)["status"], &by_id(&o2_id)["findings_submitted"]), (&json!("no_session"), &unavailable("no_session")));
    assert_eq!((&by_id(&o3_id)["status"], &by_id(&o3_id)["findings_submitted"]), (&json!("unassigned"), &unavailable("unassigned")));
    assert_eq!(by_id(&o1_id)["sessions"][0]["completion"]["acceptance"], unavailable("no_reviewer_authority_producer"));

    let m20 = f.cli_args(&["review", "report"]).0["metrics"]["M20"].clone();
    assert_eq!((&m20["numerator"], &m20["denominator"], &m20["value"], &m20["unassigned"]), (&json!(1), &json!(2), &json!("1/2"), &json!(1)));
    assert_eq!(m20["status"], json!({"completed": 1, "completed_empty": 1, "ended_without_completion": 0, "in_progress": 0, "no_session": 1}));
    assert_eq!(m20["by_kind_protocol"], json!({"code/review-protocol.v1": "1/1", "security/review-protocol.v1": "0/1"}));

    // O2's only session times out with a finding: not completed, so its findings stay unavailable.
    let s2_session = f.cli_args(&["review", "start", &o2_id, "--attempt", "rev-a2"]).0["session"].clone();
    assert_eq!((&s2_session["configuration_id"], &s2_session["matches_assignment"]), (&json!(null), &json!(null)), "no dispatch decision: unknown, not a mismatch");
    let timed_out = json!({"schema": "review_receipt.v1", "session_id": s2_session["session_id"], "submission_id": s2, "candidate_oid": oid2,
        "outcome": "timed_out", "reason": "budget_exhausted", "findings": ["finding:f1"], "evidence": [format!("sha256:{}", hex('e'))]});
    f.cli_args(&["review", "complete", "--input-file", &receipt("timeout.json", timed_out)]);
    let report = f.report();
    let m20 = &report["metrics"]["M20"];
    assert_eq!((&m20["value"], &m20["status"]["ended_without_completion"], &m20["status"]["no_session"]), (&json!("1/2"), &json!(1), &json!(0)));
    // Its finding is one untriaged submission: pending, outside the M22 denominator.
    assert_eq!((&report["metrics"]["M22"]["value"], &report["metrics"]["M22"]["pending"]), (&json!(null), &json!(1)));
    let o2 = f.cli_args(&["review", "show"]).0["opportunities"].as_array().unwrap().iter().find(|o| o["opportunity_id"] == o2_id.as_str()).unwrap().clone();
    assert_eq!((&o2["findings_submitted"], &o2["sessions"][0]["completion"]["findings_submitted"]), (&unavailable("ended_without_completion"), &json!(1)));

    // Nothing of the refused content reached the store.
    for file in ["state.db", "state.db-wal"] {
        let bytes = fs::read(f.project.join(".state").join(file)).unwrap_or_default();
        assert!(!bytes.windows(20).any(|w| w == b"SENTINEL-REVIEW-TEXT"), "{file}");
    }
}

/// The factory side of a review world: task contract `work`, result
/// submission S (candidate 1…1) by the author attempt, reviewer attempts
/// `rev-*` and the retained Codex profile `fast` to assign.
fn review_world(f: &Fixture, reviewers: &[&str]) -> (String, String) {
    let db_path = f.project.join(".state/state.db");
    let mut fast = codex_profile(&f.config, "codex", "fast", Some(&f.tmp.path().join("fast-home")));
    fast.arguments_digest = "1".repeat(64);
    plant_profile(&db_path, fast);
    let hex = |c: char| c.to_string().repeat(64);
    let (sub, oid) = (hex('1'), "1".repeat(40));
    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute("INSERT INTO task_contracts(task_id,contract_revision,plan_revision,project_store,expected_head,repository,base_oid,object_format,memory_snapshot_id,route,raw_bytes,raw_digest,installed_seq)
        VALUES('work',1,NULL,'store',0,'/repo',?1,'sha1',NULL,'verify_only',x'61',?2,(SELECT max(sequence) FROM events))", rusqlite::params!["b".repeat(40), hex('c')]).unwrap();
    db.execute("INSERT INTO result_submissions(submission_id,project_store,idempotency_key,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,artifact_manifest,claimed_checks,created_unix_ms)
        VALUES(?1,'store',?1,?2,'{}','work',1,?2,?3,'/repo',?4,?5,'sha1','[]','[]',1000)", rusqlite::params![sub, hex('d'), f.attempt, "b".repeat(40), oid]).unwrap();
    for attempt in reviewers {
        db.execute("INSERT INTO attempts(id,task_id,revision,state,reservation,termination_observed) VALUES(?1,'work',1,'completed',?1,1)", [attempt]).unwrap();
    }
    (sub, oid)
}

/// One review of S by `attempt` (method `kind`) that completes with `findings`; returns the completion.
fn completed_review(f: &Fixture, (sub, oid): &(String, String), kind: &str, attempt: &str, findings: serde_json::Value) -> serde_json::Value {
    let opportunity = f.cli_args(&["review", "open", sub, "--kind", kind, "--protocol", "review-protocol.v1"]).0["opportunity"]["opportunity_id"].as_str().unwrap().to_owned();
    f.cli_args(&["review", "assign", &opportunity, "--reviewer", "fast"]);
    let session = f.cli_args(&["review", "start", &opportunity, "--attempt", attempt]).0["session"]["session_id"].as_str().unwrap().to_owned();
    let path = f.tmp.path().join(format!("{attempt}.json"));
    fs::write(&path, json!({"schema": "review_receipt.v1", "session_id": session, "submission_id": sub, "candidate_oid": oid, "outcome": "completed",
        "findings": findings, "evidence": []}).to_string()).unwrap();
    f.cli_args(&["review", "complete", "--input-file", path.to_str().unwrap()]).0["completion"].clone()
}

fn evidence(c: char) -> String { format!("sha256:{}", c.to_string().repeat(64)) }

/// (outcome, has_validated_claim, [claim ids]) of each submission, in order.
fn submissions(state: &serde_json::Value) -> Vec<(String, bool, Vec<i64>)> {
    state["submissions"].as_array().unwrap().iter().map(|s| (s["outcome"].as_str().unwrap().to_owned(), s["has_validated_claim"].as_bool().unwrap(),
        s["claims"].as_array().unwrap().iter().map(|c| c["claim_id"].as_i64().unwrap()).collect())).collect()
}

/// Two reviews report one defect under different titles; a third reports one
/// broad submission that triage splits into three claims. By hand: submissions
/// 1 (claim 1), 2 (claim 2), 3 (claim 3). Claim 1 mints canonical finding
/// `finding:canonical-4` (history seq 4); claim 2 is validated as the same
/// finding, so it is a duplicate: one unique finding for two titles.
/// Submission 3 splits (seq 6) into claims 4, 5, 6: 4 and 5 mint findings at
/// seq 7 and 8, 6 is rejected. Buckets: validated_only 1 (sub 1),
/// duplicate_only 1 (sub 2), mixed 1 (sub 3); M22 = 2/3 (sub 3 counts once
/// despite two validated claims), M23 = 1/3; unique findings 3; claim
/// drill-down validated 3, duplicate 1, rejected 1, pending 0; 3 submissions,
/// never 5.
#[test]
fn duplicate_titles_one_finding_split_claims_no_inflation() {
    let f = Fixture::new();
    let world = review_world(&f, &["rev-a1", "rev-a2", "rev-a3"]);
    let db_path = f.project.join(".state/state.db");
    let count = |table: &str| rusqlite::Connection::open(&db_path).unwrap().query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get::<_, i64>(0)).unwrap();

    let c1 = completed_review(&f, &world, "code", "rev-a1", json!([{"ref": "finding:null-deref", "title": "Null deref in parse_config when the file is missing"}]));
    assert_eq!((&c1["findings_submitted"], &c1["finding_submissions"]), (&json!(1), &json!([1])));
    // A title is a §7 excerpt before any write: first line only, home prefix as `~`.
    let c2 = completed_review(&f, &world, "security", "rev-a2",
        json!([{"ref": "finding:crash-on-missing", "title": "Loader crashes on missing /home/alice/app.toml\nSENTINEL-FINDING-BODY stack trace"}]));
    assert_eq!(c2["finding_submissions"], json!([2]));
    // A reviewer cannot carry a decision inside a finding: the receipt is refused and writes nothing.
    let opportunity = f.cli_args(&["review", "open", &world.0, "--kind", "skeptical", "--protocol", "review-protocol.v1"]).0["opportunity"]["opportunity_id"].as_str().unwrap().to_owned();
    f.cli_args(&["review", "assign", &opportunity, "--reviewer", "fast"]);
    let session = f.cli_args(&["review", "start", &opportunity, "--attempt", "rev-a3"]).0["session"]["session_id"].as_str().unwrap().to_owned();
    let receipt = |findings: serde_json::Value| {
        let path = f.tmp.path().join("broad.json");
        fs::write(&path, json!({"schema": "review_receipt.v1", "session_id": session, "submission_id": world.0, "candidate_oid": world.1, "outcome": "completed",
            "findings": findings, "evidence": []}).to_string()).unwrap();
        path.to_str().unwrap().to_owned()
    };
    let forged = receipt(json!([{"ref": "finding:broad", "title": "Several problems", "validated": true}]));
    assert!(f.cli_fail(&["review", "complete", "--input-file", &forged]).contains("unknown field `validated` in a finding"));
    assert_eq!((count("review_completions"), count("finding_submissions"), count("canonical_findings")), (2, 2, 0));
    let broad = receipt(json!([{"ref": "finding:broad", "title": "Config loading has several problems"}]));
    assert_eq!(f.cli_args(&["review", "complete", "--input-file", &broad]).0["completion"]["finding_submissions"], json!([3]));

    // Arrival alone decides nothing: three pending proposals.
    let state = f.cli_args(&["review", "findings", "show"]).0["findings"].clone();
    assert_eq!((&state["head_seq"], &state["unique_findings"], &state["summary"]["pending"]), (&json!(3), &json!(0), &json!(3)));
    assert_eq!(state["submissions"][1]["title"], json!("Loader crashes on missing ~/app.toml"));
    assert_eq!((&state["submissions"][0]["trust"], &state["submissions"][0]["finding_ref"], &state["submissions"][0]["reporter_attempt_id"]),
        (&json!("proposal"), &json!("finding:null-deref"), &json!("rev-a1")));

    // Two titles, one defect.
    let minted = f.cli_args(&["review", "findings", "validate", "1", "--new", "--title", "Missing config crashes the loader", "--severity", "high", "--evidence", &evidence('e')]).0["event"].clone();
    assert_eq!((&minted["seq"], &minted["subject"]["finding_id"], &minted["authority"], &minted["principal"]),
        (&json!(4), &json!("finding:canonical-4"), &json!("operator_owner.v1"), &json!("operator:cli")));
    assert!(f.cli_fail(&["review", "findings", "validate", "2", "--new", "--severity", "high"]).contains("needs at least one evidence reference"));
    f.cli_args(&["review", "findings", "validate", "2", "--finding", "finding:canonical-4", "--severity", "high", "--evidence", &evidence('f')]);
    // The broad report becomes three claims beneath the same submission.
    let split = f.cli_args(&["review", "findings", "split", "3", "--claim", "Loader ignores XDG_CONFIG_HOME", "--claim", "Loader leaks a file handle", "--claim", "Prefer TOML over INI"]).0["event"].clone();
    assert_eq!((&split["seq"], &split["subject"]), (&json!(6), &json!({"submission_id": 3, "revision": 2, "claims": [4, 5, 6]})));
    assert!(f.cli_fail(&["review", "findings", "reject", "3", "--reason", "out_of_scope"]).contains("not in submission 3's current claim set"));
    f.cli_args(&["review", "findings", "validate", "4", "--new", "--severity", "medium", "--evidence", &evidence('a')]);
    f.cli_args(&["review", "findings", "validate", "5", "--new", "--severity", "low", "--evidence", &evidence('b')]);
    f.cli_args(&["review", "findings", "reject", "6", "--reason", "out_of_scope"]);

    let state = f.cli_args(&["review", "findings", "show"]).0["findings"].clone();
    assert_eq!(submissions(&state), [("validated_only".to_owned(), true, vec![1]), ("duplicate_only".to_owned(), false, vec![2]), ("mixed".to_owned(), true, vec![4, 5, 6])]);
    assert_eq!(state["summary"], json!({"submissions": 3, "pending": 0, "adjudicated": 3, "validated_only": 1, "rejected_only": 0, "duplicate_only": 1, "mixed": 1,
        "has_validated_claim": 2, "claims": {"duplicate": 1, "pending": 0, "rejected": 1, "validated": 3}}));
    assert_eq!(state["unique_findings"], json!(3));
    let group = state["findings"][0].clone();
    assert_eq!((&group["finding_id"], &group["title"], &group["status"], &group["discovery_claim"], &group["validated_claims"], &group["duplicate_claims"]),
        (&json!("finding:canonical-4"), &json!("Missing config crashes the loader"), &json!("validated"), &json!(1), &json!([1]), &json!([2])));
    let claim2 = &state["submissions"][1]["claims"][0];
    assert_eq!((&claim2["decided"], &claim2["outcome"], &claim2["canonical_finding"]), (&json!("validated"), &json!("duplicate"), &json!("finding:canonical-4")));
    assert_eq!(state["findings"].as_array().unwrap().iter().map(|g| g["finding_id"].as_str().unwrap()).collect::<Vec<_>>(),
        ["finding:canonical-4", "finding:canonical-7", "finding:canonical-8"]);

    let metrics = f.cli_args(&["review", "report"]).0["metrics"].clone();
    assert_eq!((&metrics["M22"]["numerator"], &metrics["M22"]["denominator"], &metrics["M22"]["value"], &metrics["M22"]["basis"]), (&json!(2), &json!(3), &json!("2/3"), &json!("owner_triage")));
    assert_eq!((&metrics["M23"]["value"], &metrics["M23"]["pending"]), (&json!("1/3"), &json!(0)));
    assert_eq!(metrics["M22"]["buckets"], json!({"validated_only": 1, "rejected_only": 0, "duplicate_only": 1, "mixed": 1}));
    // Discovery credit: one per unique finding, to its earliest validated reporter (none has a dispatch decision).
    assert_eq!((&metrics["M21"]["value"], &metrics["M21"]["drilldown"], &metrics["M21"]["by_configuration"], &metrics["M21"]["participation"]),
        (&json!("3"), &json!({"validated_unique_findings": 3}), &json!({"unknown": "3"}), &json!(3)));
    assert_eq!(metrics["M20"]["value"], json!("3/3"));
    // The full report carries the same lane metrics.
    assert_eq!(f.report()["metrics"]["M23"]["value"], json!("1/3"));

    // Only excerpts reached the store.
    for file in ["state.db", "state.db-wal"] {
        let bytes = fs::read(f.project.join(".state").join(file)).unwrap_or_default();
        assert!(!bytes.windows(21).any(|w| w == b"SENTINEL-FINDING-BODY") && !bytes.windows(11).any(|w| w == b"/home/alice"), "{file}");
    }
}

/// Doc 10 §5 denominator/correction golden. Submissions: 1 (rev-a1, claim 1),
/// 2 (rev-a2, claim 2), 3 (rev-a3, claim 3). Seq 4 validates claim 1 as new
/// finding P = `finding:canonical-4`; seq 5 splits submission 2 into claims 4,
/// 5; seq 6 validates claim 4 as new N = `finding:canonical-6`; seq 7 marks
/// claim 5 a duplicate of P; seq 8 rejects claim 3. By hand: validated_only 1,
/// mixed 1, rejected_only 1: M22 = 2/3, M23 = 0/3. Seq 9 merges N into P:
/// submission 2 becomes duplicate_only: M22 = 1/3, M23 = 1/3, unique 1. Seq 10
/// unmerges: 2/3 and 0/3 again, unique 2; as-of 9 still shows the merge.
/// Seq 11 reopens claim 3: pending 1, M22 = 2/2. Seq 12 restores submission
/// 2's unsplit revision 1 (claim 2, never decided): pending 2, M22 = 1/1.
/// Seq 13 restores revision 2: claims 4, 5 and their decisions return. As
/// of seq 5 only submission 1 is adjudicated (pending 2).
#[test]
fn triage_replay_as_of_and_worker_cannot_validate() {
    let f = Fixture::new();
    let world = review_world(&f, &["rev-a1", "rev-a2", "rev-a3"]);
    let db_path = f.project.join(".state/state.db");
    completed_review(&f, &world, "code", "rev-a1", json!(["finding:p"]));
    completed_review(&f, &world, "security", "rev-a2", json!(["finding:s1"]));
    completed_review(&f, &world, "test", "rev-a3", json!(["finding:s2"]));
    let show = |as_of: Option<i64>| {
        let seq = as_of.map(|s| s.to_string());
        let mut args = vec!["review", "findings", "show"];
        if let Some(seq) = &seq { args.extend(["--as-of", seq.as_str()]); }
        f.cli_args(&args).0["findings"].clone()
    };
    let rates = || {
        let m = f.cli_args(&["review", "report"]).0["metrics"].clone();
        (m["M22"]["value"].clone(), m["M23"]["value"].clone(), m["M22"]["pending"].clone())
    };

    // Workers, imports and unknown principals cannot triage; nothing is written.
    let mut store = SqliteStore::open(&db_path).unwrap();
    let validate = TriageRequest { outcome: TriageOutcome::Validated { target: FindingTarget::New { title: None }, severity: "high".into() }, evidence: vec![evidence('e')], expected_seq: None };
    for principal in ["worker:rev-a1", "rev-a1", "rev-a3", f.attempt.as_str()] {
        assert!(format!("{:?}", store.triage_finding_claim(1, &validate, principal, 1).unwrap_err()).contains("a worker cannot triage findings"), "{principal}");
    }
    assert!(format!("{:?}", store.triage_finding_claim(1, &validate, "import:github-review", 1).unwrap_err()).contains("an untrusted import cannot triage"));
    assert!(format!("{:?}", store.triage_finding_claim(1, &validate, "operator:someone", 1).unwrap_err()).contains("no finding triage authority"));
    assert!(format!("{:?}", store.split_finding_submission(2, &[None, None], None, "worker:rev-a2", 1).unwrap_err()).contains("a worker cannot triage"));
    drop(store);
    {
        // Raw rows cannot forge the authority either.
        let db = rusqlite::Connection::open(&db_path).unwrap();
        let forged = db.execute("INSERT INTO finding_log(kind,principal,authority,recorded_unix_ms) VALUES('decided','worker:rev-a1','operator_owner.v1',1)", []).unwrap_err();
        assert!(forged.to_string().contains("CHECK constraint failed"), "{forged}");
        let proposal = db.execute("INSERT INTO finding_decisions(seq,claim_id,outcome,finding_id,reason,severity,severity_policy,evidence_refs) VALUES(1,1,'rejected',NULL,'out_of_scope',NULL,NULL,'[]')", []).unwrap_err();
        assert!(proposal.to_string().contains("finding triage needs the triage authority"), "{proposal}");
        let rewrite = db.execute("UPDATE finding_submissions SET trust='proposal'", []).unwrap_err();
        assert!(rewrite.to_string().contains("append-only"), "{rewrite}");
    }
    assert_eq!(show(None)["head_seq"], json!(3));

    f.cli_args(&["review", "findings", "validate", "1", "--new", "--severity", "high", "--evidence", &evidence('e'), "--expect-seq", "3"]);
    assert!(f.cli_fail(&["review", "findings", "split", "2", "--claim", "a", "--claim", "b", "--expect-seq", "3"]).contains("finding history moved: head is 4, expected 3"));
    f.cli_args(&["review", "findings", "split", "2", "--claim", "Retry loop never ends", "--claim", "Loader crash, as reported before"]);
    let at5 = show(None);
    f.cli_args(&["review", "findings", "validate", "4", "--new", "--severity", "medium", "--evidence", &evidence('a')]);
    f.cli_args(&["review", "findings", "duplicate", "5", "--of", "finding:canonical-4"]);
    f.cli_args(&["review", "findings", "reject", "3", "--reason", "insufficient_evidence"]);
    let at8 = show(None);
    assert_eq!(submissions(&at8), [("validated_only".to_owned(), true, vec![1]), ("mixed".to_owned(), true, vec![4, 5]), ("rejected_only".to_owned(), false, vec![3])]);
    assert_eq!(rates(), (json!("2/3"), json!("0/3"), json!(0)));
    assert_eq!(at8["unique_findings"], json!(2));

    // Merge N into P: submission 2's validated claim becomes a duplicate of the earlier discovery.
    let merge = f.cli_args(&["review", "findings", "merge", "finding:canonical-6", "--into", "finding:canonical-4"]).0["event"].clone();
    assert_eq!(merge["seq"], json!(9));
    assert!(f.cli_fail(&["review", "findings", "merge", "finding:canonical-4", "--into", "finding:canonical-6"]).contains("already one group"));
    let at9 = show(None);
    assert_eq!(submissions(&at9)[1], ("duplicate_only".to_owned(), false, vec![4, 5]));
    assert_eq!((&at9["unique_findings"], &at9["findings"][1]["status"], &at9["findings"][1]["merged_into"]), (&json!(1), &json!("merged"), &json!("finding:canonical-4")));
    assert_eq!(rates(), (json!("1/3"), json!("1/3"), json!(0)));
    f.cli_args(&["review", "findings", "unmerge", "9"]);
    assert!(f.cli_fail(&["review", "findings", "unmerge", "9"]).contains("is not active"));
    assert_eq!(rates(), (json!("2/3"), json!("0/3"), json!(0)));
    assert_eq!(show(None)["unique_findings"], json!(2));

    // Reopen triage and reverse the split, then restore it.
    f.cli_args(&["review", "findings", "reset", "3", "--reason", "reopened"]);
    assert_eq!(rates(), (json!("2/2"), json!("0/2"), json!(1)));
    f.cli_args(&["review", "findings", "restore", "2", "--revision", "1"]);
    let at12 = show(None);
    assert_eq!(submissions(&at12)[1], ("pending".to_owned(), false, vec![2]));
    assert_eq!(rates(), (json!("1/1"), json!("0/1"), json!(2)));
    f.cli_args(&["review", "findings", "restore", "2", "--revision", "2"]);
    let head = show(None);
    assert_eq!((&head["head_seq"], &submissions(&head)[1]), (&json!(13), &("mixed".to_owned(), true, vec![4, 5])));

    // Replay: every earlier view is reproduced exactly at its sequence; the history keeps every row.
    for (seq, view) in [(5, &at5), (8, &at8), (9, &at9), (12, &at12)] {
        let replayed = show(Some(seq));
        for key in ["as_of_seq", "unique_findings", "summary", "submissions", "findings", "history"] { assert_eq!(replayed[key], view[key], "seq {seq} {key}"); }
        assert_eq!(replayed["head_seq"], json!(13));
    }
    assert_eq!(submissions(&at5)[1], ("pending".to_owned(), false, vec![4, 5]));
    assert_eq!((&at5["summary"]["adjudicated"], &at5["summary"]["pending"]), (&json!(1), &json!(2)));
    let kinds: Vec<String> = head["history"].as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap().to_owned()).collect();
    assert_eq!(kinds, ["submitted", "submitted", "submitted", "decided", "split", "decided", "decided", "decided", "merged", "unmerged", "decided", "restored", "restored"]);
    assert_eq!((&head["history"][9]["subject"]["reverses"], &head["history"][10]["subject"]["supersedes"]), (&json!(9), &json!(8)));
    assert!(head["history"].as_array().unwrap()[3..].iter().all(|e| e["principal"] == "operator:cli" && e["authority"] == "operator_owner.v1"));
    assert!(head["history"].as_array().unwrap()[..3].iter().all(|e| e["authority"] == "proposal"));
    assert!(f.cli_fail(&["review", "findings", "show", "--as-of", "14"]).contains("outside the finding history"));
}

/// The factory side of repairs, planted as the launch, result, verification
/// and integration producers would write them.
struct Factory(rusqlite::Connection);

impl Factory {
    fn open(f: &Fixture) -> Self {
        let db = rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap();
        db.execute_batch("INSERT OR IGNORE INTO acceptance_policies(task_id,contract_revision,policy_id,body) VALUES('work',1,'ci','cargo test');
            INSERT OR IGNORE INTO integration_targets(repository,ref_name,created_unix_ms) VALUES('/repo','refs/heads/main',1);
            INSERT OR IGNORE INTO integration_target_leases(repository,ref_name,operation_id,generation) VALUES('/repo','refs/heads/main',NULL,0);").unwrap();
        Factory(db)
    }
    fn configuration(&self, profile: &herdr_projects::domain::FrozenProfile) -> String {
        let c = agent_configuration(profile);
        self.0.execute("INSERT OR IGNORE INTO agent_configurations VALUES(?1,?2,1)", rusqlite::params![c.id, c.canonical_json]).unwrap();
        c.id
    }
    /// A launched attempt of `work` that has no result yet, and its dispatch decision.
    fn attempt(&self, id: &str, configuration: &str) {
        self.0.execute("INSERT INTO attempts(id,task_id,revision,state,reservation,termination_observed) VALUES(?1,'work',1,'running',?1,0)", [id]).unwrap();
        self.decision(id, configuration);
    }
    fn decision(&self, attempt: &str, configuration: &str) {
        self.0.execute("INSERT INTO dispatch_decisions(attempt_id,task_id,task_revision,contract_revision,chosen_configuration_id,eligible,chooser_kind,chooser_principal,reason_codes,decided_unix_ms)
            VALUES(?1,'work',1,1,?2,'[\"x\"]','operator','operator:cli','[\"x\"]',1)", [attempt, configuration]).unwrap();
    }
    fn submission(&self, id: &str, attempt: &str, candidate: &str, at: i64) {
        self.0.execute("INSERT INTO result_submissions(submission_id,project_store,idempotency_key,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,artifact_manifest,claimed_checks,created_unix_ms)
            VALUES(?1,'store',?1,?2,'{}','work',1,?2,?3,'/repo',?4,?5,'sha1','[]','[]',?6)", rusqlite::params![id, hex('d'), attempt, "b".repeat(40), candidate, at]).unwrap();
    }
    /// Verification run `run` of `submission` at `commit`; an accepted run also gets verified result `result`.
    fn run(&self, run: &str, submission: &str, attempt: &str, commit: &str, result: Option<&str>) {
        let accepted = result.is_some();
        self.0.execute("INSERT INTO verification_runs(run_id,project_store,idempotency_key,payload_digest,submission_id,task_id,contract_revision,contract_digest,attempt_id,policy_id,policy_digest,
            commit_oid,tree_oid,object_format,memory_fence,isolation,argv,library_manifest,state,reason,exit_status,receipt_digest,store_device,store_inode,created_unix_ms)
            VALUES(?1,'store',?1,?2,?3,'work',1,?2,?4,'ci',?5,?6,?6,'sha1',0,'linux-unshare-user-pid-mount-v1','[\"x\"]','[]',?7,?8,?9,?10,1,1,7000)",
            rusqlite::params![run, hex('d'), submission, attempt, hex('9'), commit, if accepted { "accepted" } else { "rejected" }, (!accepted).then_some("cargo test failed"),
                if accepted { 0 } else { 101 }, accepted.then(|| hex('8'))]).unwrap();
        if let Some(result) = result {
            self.0.execute("INSERT INTO verified_results(result_id,run_id,submission_id,commit_oid,tree_oid,object_format,policy_digest,receipt_digest,isolation,memory_fence,created_unix_ms)
                VALUES(?1,?2,?3,?4,?4,'sha1',?5,?6,'linux-unshare-user-pid-mount-v1',0,7000)", rusqlite::params![result, run, submission, commit, hex('9'), hex('8')]).unwrap();
        }
    }
    /// Integration `id` of verified result `result` whose candidate merged `parent` as merge commit `commit`.
    fn integration(&self, id: &str, result: &str, parent: &str, commit: &str, at: i64) {
        self.0.execute("INSERT INTO operations(id,task_id,kind,target,payload_version,payload,payload_hash,expected_revision,due_unix_ms,idempotency_key)
            VALUES(?1,'work','integration.run','refs/heads/main',1,'{}',?2,1,0,?1)", rusqlite::params![id, hex('d')]).unwrap();
        self.0.execute("INSERT INTO integration_operations(operation_id,project_store,idempotency_key,payload_digest,repository,ref_name,expected_old_oid,verified_result_id,candidate_id,state,generation,object_format,checks_passed,reason,created_unix_ms)
            VALUES(?1,'store',?1,?2,'/repo','refs/heads/main',?3,?4,?1,'integrated',1,'sha1',1,NULL,?5)", rusqlite::params![id, hex('d'), "b".repeat(40), result, at]).unwrap();
        self.0.execute("INSERT INTO integration_candidates(candidate_id,operation_id,commit_oid,tree_oid,parent_base,parent_verified,strategy,object_format,state,created_unix_ms)
            VALUES(?1,?1,?2,?2,?3,?4,'ort','sha1','published',?5)", rusqlite::params![id, commit, "b".repeat(40), parent, at]).unwrap();
        self.0.execute("INSERT INTO integrated_commits(integrated_id,candidate_id,operation_id,repository,ref_name,commit_oid,tree_oid,expected_old_oid,object_format,created_unix_ms)
            VALUES(?1,?1,?1,'/repo','refs/heads/main',?2,?2,?3,'sha1',?4)", rusqlite::params![id, commit, "b".repeat(40), at]).unwrap();
    }
}

fn hex(c: char) -> String { c.to_string().repeat(64) }
fn oid(c: char) -> String { c.to_string().repeat(40) }

/// The retained `fast` profile `review_world` plants (configuration A).
fn fast_profile(f: &Fixture) -> herdr_projects::domain::FrozenProfile {
    let mut fast = codex_profile(&f.config, "codex", "fast", Some(&f.tmp.path().join("fast-home")));
    fast.arguments_digest = "1".repeat(64);
    fast
}

fn fixes_show(f: &Fixture, as_of: Option<i64>) -> serde_json::Value {
    let seq = as_of.map(|s| s.to_string());
    let mut args = vec!["review", "fixes", "show"];
    if let Some(seq) = &seq { args.extend(["--as-of", seq.as_str()]); }
    f.cli_args(&args).0["fixes"].clone()
}

/// (contributor, configuration, share) of a role credit.
fn credit(role: &serde_json::Value) -> Vec<(String, serde_json::Value, String)> {
    role["shares"].as_array().unwrap().iter().map(|s| (s["contributor"].as_str().unwrap().to_owned(), s["configuration_id"].clone(), s["share"].as_str().unwrap().to_owned())).collect()
}

/// Doc 10 §5 exact-candidate and reopen golden. History by hand: seq 1
/// rev-a1's submission; 2 validates it as F = `finding:canonical-2`; 3 opens
/// repair 3 assigned to `fast` (A); 4 binds fix-a1 (A, no result yet). fix-a1
/// then submits SF1 (3…3, which run `a` verifies) and, after changing its
/// branch, SF2 (4…4). 5 proposes SF2: run `a` (SF1's commit) and the rejected
/// run `c` cannot verify it; 6 verifies it with run `d` on 4…4; an integration
/// of SF1's verified result is not its integration; 7 links integration IG
/// (merge 5…5); 8 closes repair 3 `fixed`. Now M25 = M26 = 1/1 (finding and
/// A's cohort), M27 has no observed integration (1 censored, horizon 14 d),
/// M29 = 5/6 (introduction unattributed). 9 attributes introduction to the
/// author of 1…1 by bisect (blame and the fixer are refused); 10 reopens F
/// after the fix was reverted: M26 = 0/1, M25 stays 1/1, M27 = 1/1; the
/// as-of-9 view still shows the resolution. 11 opens a new unassigned repair,
/// censored within its horizon.
#[test]
fn fix_credit_requires_exact_candidate_and_reopen_removes_current_credit() {
    let f = Fixture::new();
    let world = review_world(&f, &["rev-a1"]);
    let db_path = f.project.join(".state/state.db");
    let fast_id = agent_configuration(&fast_profile(&f)).id;
    let author_id = agent_configuration(&codex_profile(&f.config, "codex", "codex", Some(&f.home))).id;
    completed_review(&f, &world, "code", "rev-a1", json!(["finding:crash"]));
    f.cli_args(&["review", "findings", "validate", "1", "--new", "--severity", "high", "--evidence", &evidence('e')]);
    let finding = "finding:canonical-2";
    let fixes = |args: &[&str]| { let mut all = vec!["review", "fixes"]; all.extend(args); f.cli_args(&all).0["event"].clone() };
    let refused = |args: &[&str], text: &str| { let mut all = vec!["review", "fixes"]; all.extend(args); let e = f.cli_fail(&all); assert!(e.contains(text), "{e}"); };

    // Only the owner attributes: workers and imports are refused and write nothing.
    let mut store = SqliteStore::open(&db_path).unwrap();
    for principal in ["worker:fix-a1", "rev-a1", f.attempt.as_str()] {
        let err = store.open_repair(finding, &RepairAssignment::Unassigned, 86_400_000, None, principal, 1).unwrap_err();
        assert!(format!("{err:?}").contains("a worker cannot triage findings"), "{principal}");
    }
    assert!(format!("{:?}", store.open_repair(finding, &RepairAssignment::Unassigned, 86_400_000, None, "import:github", 1).unwrap_err()).contains("an untrusted import"));
    drop(store);
    assert_eq!(fixes_show(&f, None)["head_seq"], json!(2));

    // Repair 3 is assigned to A before any repair runs.
    let opened = fixes(&["open", finding, "--assign", "fast"]);
    assert_eq!((&opened["seq"], &opened["subject"]["assignment"], &opened["subject"]["configuration_id"], &opened["subject"]["horizon_ms"]),
        (&json!(3), &json!("configuration"), &json!(fast_id), &json!(14 * 86_400_000_i64)));
    refused(&["open", finding, "--unassigned"], "already has open repair opportunity 3");
    refused(&["open", "finding:canonical-9", "--unassigned"], "no canonical finding");

    // Attempts bind before their outcome: a finished attempt or one with a result is refused.
    let factory = Factory::open(&f);
    factory.attempt("fix-a1", &fast_id);
    refused(&["bind", "3", "--attempt", "rev-a1"], "is already completed");
    refused(&["bind", "3", "--attempt", &f.attempt], "already has a result");
    assert_eq!(fixes(&["bind", "3", "--attempt", "fix-a1"])["subject"], json!({"repair_seq": 3, "attempt_id": "fix-a1", "ordinal": 1, "configuration_id": fast_id}));
    let (sf1, sf2) = (hex('3'), hex('4'));
    factory.submission(&sf1, "fix-a1", &oid('3'), 5_000);
    factory.run(&hex('a'), &sf1, "fix-a1", &oid('3'), Some(&hex('b')));
    factory.submission(&sf2, "fix-a1", &oid('4'), 6_000);
    factory.run(&hex('c'), &sf2, "fix-a1", &oid('4'), None);
    factory.run(&hex('d'), &sf2, "fix-a1", &oid('4'), Some(&hex('f')));
    let now = unix_ms();
    factory.integration(&hex('6'), &hex('b'), &oid('3'), &oid('8'), now - 2_000);
    factory.integration(&hex('7'), &hex('f'), &oid('4'), &oid('5'), now - 1_000);
    refused(&["propose", "3", "--submission", &world.0], "is not bound to repair opportunity 3");
    assert_eq!(fixes(&["propose", "3", "--submission", &sf2])["subject"]["candidate_oid"], json!(oid('4')));

    // Passing checks on one commit cannot verify another; a rejected run verifies nothing.
    refused(&["verify", "5", "--run", &hex('a'), "--assurance", "regression_reproduced", "--evidence", &evidence('a')], "passing checks on one commit cannot verify another");
    refused(&["verify", "5", "--run", &hex('c'), "--assurance", "regression_reproduced", "--evidence", &evidence('a')], "was rejected");
    refused(&["verify", "5", "--run", &hex('d'), "--assurance", "regression_reproduced"], "needs at least one evidence reference");
    {
        // Raw rows cannot forge it either: the trigger checks the exact candidate.
        let mut db = rusqlite::Connection::open(&db_path).unwrap();
        let tx = db.transaction().unwrap();
        tx.execute("INSERT INTO fix_log(seq,kind,principal,authority,recorded_unix_ms) VALUES(6,'verified','operator:cli','operator_owner.v1',1)", []).unwrap();
        let forged = tx.execute("INSERT INTO fix_verifications(seq,proposal_seq,run_id,result_id,commit_oid,assurance,evidence_refs) VALUES(6,5,?1,?2,?3,'regression_reproduced','[\"x\"]')",
            rusqlite::params![hex('a'), hex('b'), oid('4')]).unwrap_err();
        assert!(forged.to_string().contains("verified only by an accepted verification of its exact candidate"), "{forged}");
        let worker = tx.execute("INSERT INTO fix_log(seq,kind,principal,authority,recorded_unix_ms) VALUES(7,'credited','worker:fix-a1','operator_owner.v1',1)", []).unwrap_err();
        assert!(worker.to_string().contains("CHECK constraint failed"), "{worker}");
    }
    refused(&["integrate", "5", "--integrated", &hex('7')], "is not verified");
    assert_eq!(fixes(&["verify", "5", "--run", &hex('d'), "--assurance", "regression_reproduced", "--evidence", &evidence('a')])["subject"]["result_id"], json!(hex('f')));
    refused(&["close", "3", "--outcome", "no_fix"], "has a verified fix");
    // An integration of SF1's verified result is not an integration of this fix.
    refused(&["integrate", "5", "--integrated", &hex('6')], "did not integrate the fix's exact verified candidate");
    assert_eq!(fixes(&["integrate", "5", "--integrated", &hex('7')])["subject"]["commit_oid"], json!(oid('5')));
    fixes(&["close", "3", "--outcome", "fixed"]);

    let at8 = fixes_show(&f, None);
    let finding8 = &at8["findings"][0];
    assert_eq!((&finding8["remediation"], &finding8["verified"], &finding8["integrated"], &finding8["currently_resolved"]), (&json!("resolved"), &json!(true), &json!(true), &json!(true)));
    assert_eq!(finding8["resolutions"], json!([{"integration_seq": 7, "proposal_seq": 5, "repair_seq": 3, "integrated_id": hex('7'), "commit_oid": oid('5'),
        "integrated_unix_ms": now - 1_000, "ended_seq": null, "ended_by": null}]));
    // Separate roles: discovery, validation, implementation, verification, integration; introduction unattributed.
    assert_eq!((&finding8["discovery"]["policy"], credit(&finding8["discovery"])), (&json!("earliest_validated.v1"), vec![("attempt:rev-a1".into(), json!(null), "1".into())]));
    assert_eq!(credit(&finding8["validation"]), [("principal:operator:cli".to_owned(), json!(null), "1".to_owned())]);
    assert_eq!((&finding8["implementation"]["policy"], credit(&finding8["implementation"])), (&json!("sole_attempt.v1"), vec![("attempt:fix-a1".into(), json!(fast_id), "1".into())]));
    assert_eq!((credit(&finding8["verification"]), &finding8["verification"]["source_seq"]), (vec![("service:native_verifier".into(), json!(null), "1".into())], &json!(6)));
    assert_eq!(credit(&finding8["integration"]), [("service:integrator".to_owned(), json!(null), "1".to_owned())]);
    assert_eq!((&finding8["introduction"]["status"], &finding8["introduction"]["detection_oid"], &finding8["introduction"]["credit"]["unallocated"], &finding8["introduction"]["credit"]["unallocated_reason"]),
        (&json!("unattributed"), &json!(oid('1')), &json!("1"), &json!("unattributed")));
    assert_eq!((&at8["repairs"][0]["outcome"], &at8["repairs"][0]["closure"]), (&json!("currently_resolved"), &json!("fixed")));

    let m = f.cli_args(&["review", "report"]).0["metrics"].clone();
    assert_eq!((&m["M21"]["value"], &m["M25"]["value"], &m["M26"]["value"]), (&json!("1"), &json!("1/1"), &json!("1/1")));
    assert_eq!(m["M25"]["by_assignment"], json!({fast_id.as_str(): {"numerator": 1, "denominator": 1, "value": "1/1", "not_achieved": 0, "reassigned": 0, "censored": 0}}));
    assert_eq!((&m["M27"]["value"], &m["M27"]["censored"], &m["M27"]["reason"]), (&json!(null), &json!(1), &json!("empty_denominator")));
    assert_eq!((&m["M29"]["value"], &m["M29"]["by_role"]["introduction"]), (&json!("5/6"), &json!({"allocated": "0", "eligible": 1, "value": "0/1"})));

    // Introduction needs causal evidence: blame is refused, and so is charging the fixer through its fix.
    refused(&["introduce", finding, "--commit", &oid('1'), "--method", "blame", "--contributor", &format!("{}=1", f.attempt), "--evidence", &evidence('c')], "is inference, not causal evidence");
    refused(&["introduce", finding, "--commit", &oid('4'), "--method", "reliable_bisect", "--contributor", "fix-a1=1", "--evidence", &evidence('c')], "the fixer is not charged");
    refused(&["introduce", finding, "--commit", &oid('1'), "--method", "reliable_bisect", "--contributor", "rev-a1=1", "--evidence", &evidence('c')], "has no candidate at the introducing commit");
    fixes(&["introduce", finding, "--commit", &oid('1'), "--method", "reliable_bisect", "--contributor", &format!("{}=1", f.attempt), "--evidence", &evidence('c')]);
    let at9 = fixes_show(&f, None);
    assert_eq!((&at9["findings"][0]["introduction"]["status"], credit(&at9["findings"][0]["introduction"]["credit"])),
        (&json!("attributed"), vec![(format!("attempt:{}", f.attempt), json!(author_id), "1".into())]));

    // A revert reopens F: history stays, current-resolution credit goes.
    refused(&["reopen", finding, "--reason", "reverted", "--observed", &oid('6')], "needs at least one evidence reference");
    assert_eq!(fixes(&["reopen", finding, "--reason", "reverted", "--observed", &oid('6'), "--evidence", &evidence('d')])["subject"]["integration_seq"], json!(7));
    refused(&["reopen", finding, "--reason", "regression", "--observed", &oid('6'), "--evidence", &evidence('d')], "is not currently resolved");
    let at10 = fixes_show(&f, None);
    let finding10 = &at10["findings"][0];
    assert_eq!((&finding10["remediation"], &finding10["verified"], &finding10["integrated"], &finding10["currently_resolved"]), (&json!("reopened"), &json!(true), &json!(true), &json!(false)));
    assert_eq!((&finding10["resolutions"][0]["ended_seq"], &finding10["resolutions"][0]["ended_by"], &finding10["reopenings"][0]["reason"]), (&json!(10), &json!("reopened"), &json!("reverted")));
    assert_eq!(at10["repairs"][0]["outcome"], json!("integrated"));
    let m = f.cli_args(&["review", "report"]).0["metrics"].clone();
    assert_eq!((&m["M25"]["value"], &m["M26"]["value"], &m["M26"]["by_assignment"][fast_id.as_str()]["value"]), (&json!("1/1"), &json!("0/1"), &json!("0/1")));
    assert_eq!((&m["M27"]["value"], &m["M27"]["censored"]), (&json!("1/1"), &json!(0)));

    // Replay: the as-of-9 view still has the resolution, byte for byte; one ordering with the triage history.
    let replayed = fixes_show(&f, Some(9));
    for key in ["findings", "repairs", "history", "as_of_seq"] { assert_eq!(replayed[key], at9[key], "{key}"); }
    assert_eq!((&replayed["head_seq"], &replayed["findings"][0]["currently_resolved"]), (&json!(10), &json!(true)));
    assert_eq!(fixes_show(&f, Some(8))["findings"][0]["introduction"]["status"], json!("unattributed"));
    assert_eq!(f.cli_args(&["review", "findings", "show", "--as-of", "10"]).0["findings"]["head_seq"], json!(10));

    // A new repair cycle is its own opportunity, censored until closed or past its horizon.
    fixes(&["open", finding, "--unassigned", "--horizon-days", "7"]);
    let m25 = f.cli_args(&["review", "report"]).0["metrics"]["M25"].clone();
    assert_eq!((&m25["censored"], &m25["by_assignment"]["unassigned"]), (&json!(1), &json!({"numerator": 0, "denominator": 0, "value": null, "not_achieved": 0,
        "reassigned": 0, "censored": 1, "reason": "empty_denominator"})));
    let head = fixes_show(&f, None);
    let kinds: Vec<&str> = head["history"].as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["repair_opened", "attempt_bound", "proposed", "verified", "integrated", "repair_closed", "introduced", "reopened", "repair_opened"]);
    assert!(head["history"].as_array().unwrap().iter().all(|e| e["principal"] == "operator:cli" && e["authority"] == "operator_owner.v1"));
    let rewrite = rusqlite::Connection::open(&db_path).unwrap().execute("UPDATE fix_verifications SET assurance='approved_alternative'", []).unwrap_err();
    assert!(rewrite.to_string().contains("append-only"), "{rewrite}");
}

/// Doc 10 §5 assignment and mixed-credit golden. rev-a1 (A) reports P (seq
/// 1); rev-a2 (B) reports Q and P again (seq 2, 3). Seq 4 validates P =
/// `finding:canonical-4`, 5 validates Q = `finding:canonical-5`, 6 links
/// rev-a2's P report (a duplicate discovery). Both repairs are assigned to A:
/// 7 for P, 8 for Q. 9 binds a1 (A) to repair 7, which fails; 10 closes it
/// `no_fix`. 11 binds a2 (A) to repair 8, which fails; 12 reassigns it to b1
/// (B), whose candidate is proposed (13), verified (14), integrated (15);
/// 16 closes it `fixed`. By hand: A's M25 = M26 = 1/2 (the failed repair
/// stays), B has no assigned opportunity (null, not 1/1); findings 1/2.
/// Implementation credit of Q's fix is mixed and unallocated until 17 splits
/// it b1 2/3, a2 1/3. M21 = 2 (A 1, B 1); 18 shares P's discovery 1/2 + 1/2
/// (A 1/2, B 3/2, still 2 in total); 19 retracts that. M29: 6/9 = 2/3 before
/// 17, 7/9 after. 20 records P's introduction as unattributable.
#[test]
fn failed_repairs_stay_in_denominator_and_mixed_credit_is_split() {
    let f = Fixture::new();
    let world = review_world(&f, &["rev-a1", "rev-a2"]);
    let factory = Factory::open(&f);
    let a = factory.configuration(&fast_profile(&f));
    let mut slow = codex_profile(&f.config, "codex", "slow", Some(&f.tmp.path().join("slow-home")));
    slow.arguments_digest = "2".repeat(64);
    let b = factory.configuration(&slow);
    factory.decision("rev-a1", &a);
    factory.decision("rev-a2", &b);
    completed_review(&f, &world, "code", "rev-a1", json!(["finding:p"]));
    completed_review(&f, &world, "security", "rev-a2", json!(["finding:q", "finding:p-again"]));
    f.cli_args(&["review", "findings", "validate", "1", "--new", "--severity", "high", "--evidence", &evidence('e')]);
    f.cli_args(&["review", "findings", "validate", "2", "--new", "--severity", "medium", "--evidence", &evidence('e')]);
    f.cli_args(&["review", "findings", "validate", "3", "--finding", "finding:canonical-4", "--severity", "high", "--evidence", &evidence('f')]);
    let (p, q) = ("finding:canonical-4", "finding:canonical-5");
    let fixes = |args: &[&str]| { let mut all = vec!["review", "fixes"]; all.extend(args); f.cli_args(&all).0["event"].clone() };
    let refused = |args: &[&str], text: &str| { let mut all = vec!["review", "fixes"]; all.extend(args); let e = f.cli_fail(&all); assert!(e.contains(text), "{e}"); };
    let metrics = || f.cli_args(&["review", "report"]).0["metrics"].clone();

    assert_eq!(fixes(&["open", p, "--assign", "fast"])["seq"], json!(7));
    assert_eq!(fixes(&["open", q, "--assign", "fast"])["seq"], json!(8));
    for (attempt, configuration) in [("a1", &a), ("a2", &a), ("b1", &b)] { factory.attempt(attempt, configuration); }
    fixes(&["bind", "7", "--attempt", "a1"]);
    refused(&["close", "7", "--outcome", "fixed"], "has no verified fix");
    fixes(&["close", "7", "--outcome", "no_fix"]);
    refused(&["bind", "7", "--attempt", "a2"], "repair opportunity 7 is closed");
    fixes(&["bind", "8", "--attempt", "a2"]);
    assert_eq!(fixes(&["bind", "8", "--attempt", "b1"])["subject"], json!({"repair_seq": 8, "attempt_id": "b1", "ordinal": 2, "configuration_id": b}));
    factory.submission(&hex('5'), "b1", &oid('7'), 5_000);
    factory.run(&hex('a'), &hex('5'), "b1", &oid('7'), Some(&hex('b')));
    factory.integration(&hex('6'), &hex('b'), &oid('7'), &oid('9'), unix_ms() - 1_000);
    fixes(&["propose", "8", "--submission", &hex('5')]);
    fixes(&["verify", "13", "--run", &hex('a'), "--assurance", "approved_alternative", "--evidence", &evidence('a')]);
    fixes(&["integrate", "13", "--integrated", &hex('6')]);
    assert_eq!(fixes(&["close", "8", "--outcome", "fixed"])["seq"], json!(16));

    let at16 = fixes_show(&f, None);
    let attempts: Vec<_> = at16["repairs"][1]["attempts"].as_array().unwrap().iter()
        .map(|t| (t["attempt_id"].clone(), t["ordinal"].clone(), t["configuration_id"].clone(), t["reassignment"].clone())).collect();
    assert_eq!(attempts, [(json!("a2"), json!(1), json!(a), json!(false)), (json!("b1"), json!(2), json!(b), json!(true))]);
    assert_eq!((&at16["repairs"][0]["outcome"], &at16["repairs"][0]["closure"], &at16["repairs"][1]["outcome"]), (&json!("no_candidate"), &json!("no_fix"), &json!("currently_resolved")));
    let q16 = &at16["findings"][1];
    assert_eq!((&q16["implementation"]["shares"], &q16["implementation"]["allocated"], &q16["implementation"]["unallocated"], &q16["implementation"]["unallocated_reason"]),
        (&json!([]), &json!("0"), &json!("1"), &json!("mixed_contribution_unallocated")));
    assert_eq!((&at16["findings"][0]["remediation"], &at16["findings"][0]["implementation"]), (&json!("unrepaired"), &json!(null)));

    let m = metrics();
    assert_eq!((&m["M25"]["value"], &m["M25"]["findings"]), (&json!("1/2"), &json!({"label": "finding_outcomes", "numerator": 1, "denominator": 2})));
    let by = json!({a.as_str(): {"numerator": 1, "denominator": 2, "value": "1/2", "not_achieved": 1, "reassigned": 1, "censored": 0},
        b.as_str(): {"numerator": 0, "denominator": 0, "value": null, "not_achieved": 0, "reassigned": 0, "censored": 0, "reason": "no_assigned_opportunities"}});
    assert_eq!((&m["M25"]["by_assignment"], &m["M26"]["by_assignment"], &m["M26"]["value"]), (&by, &by, &json!("1/2")));
    assert_eq!((&m["M21"]["value"], &m["M21"]["by_configuration"], &m["M21"]["participation"]), (&json!("2"), &json!({a.as_str(): "1", b.as_str(): "1"}), &json!(2)));
    assert_eq!((&m["M29"]["value"], &m["M29"]["by_role"]["implementation"]), (&json!("2/3"), &json!({"allocated": "0", "eligible": 1, "value": "0/1"})));

    // Mixed contributions are split, never full credit each.
    refused(&["credit", q, "--role", "implementation", "--proposal", "13", "--share", "b1=2/3", "--share", "a2=2/3", "--evidence", &evidence('c')], "sum to more than 1");
    refused(&["credit", q, "--role", "implementation", "--proposal", "13", "--share", "a1=1/3", "--evidence", &evidence('c')], "did not contribute");
    refused(&["credit", q, "--role", "implementation", "--proposal", "13", "--share", "b1=2/3", "--share", "a2=1/3"], "needs at least one evidence reference");
    assert_eq!(fixes(&["credit", q, "--role", "implementation", "--proposal", "13", "--share", "b1=2/3", "--share", "a2=1/3", "--evidence", &evidence('c')])["seq"], json!(17));
    let q17 = fixes_show(&f, None)["findings"][1]["implementation"].clone();
    assert_eq!((&q17["policy"], credit(&q17), &q17["allocated"], &q17["unallocated"]),
        (&json!("owner_allocation.v1"), vec![("attempt:a2".into(), json!(a), "1/3".into()), ("attempt:b1".into(), json!(b), "2/3".into())], &json!("1"), &json!("0")));
    assert_eq!(metrics()["M29"]["value"], json!("7/9"));

    // Shared discovery: two reporters of P, half each; the total stays one finding each.
    refused(&["credit", p, "--role", "discovery", "--share", "rev-a1=1", "--share", "rev-a2=1/2", "--evidence", &evidence('c')], "sum to more than 1");
    refused(&["credit", q, "--role", "discovery", "--share", "rev-a1=1", "--evidence", &evidence('c')], "did not contribute");
    fixes(&["credit", p, "--role", "discovery", "--share", "rev-a1=1/2", "--share", "rev-a2=1/2", "--evidence", &evidence('c')]);
    let m21 = metrics()["M21"].clone();
    assert_eq!((&m21["value"], &m21["unallocated"], &m21["by_configuration"], &m21["participation"]), (&json!("2"), &json!("0"), &json!({a.as_str(): "1/2", b.as_str(): "3/2"}), &json!(3)));
    // Retraction restores the policy's credit; the allocation stays in the as-of view.
    refused(&["retract", "16"], "only a credit allocation, reopening or introduction");
    fixes(&["retract", "18"]);
    refused(&["retract", "18"], "already retracted");
    assert_eq!(metrics()["M21"]["by_configuration"], json!({a.as_str(): "1", b.as_str(): "1"}));
    assert_eq!((&fixes_show(&f, Some(18))["findings"][0]["discovery"]["policy"], &fixes_show(&f, None)["findings"][0]["discovery"]["policy"]),
        (&json!("owner_allocation.v1"), &json!("earliest_validated.v1")));

    // Unattributable introduction is explicit, not an exoneration and not a charge.
    refused(&["introduce", p, "--unattributable"], "needs at least one evidence reference");
    fixes(&["introduce", p, "--unattributable", "--evidence", &evidence('d')]);
    let intro = fixes_show(&f, None)["findings"][0]["introduction"].clone();
    assert_eq!((&intro["status"], &intro["credit"]["shares"], &intro["credit"]["unallocated"], &intro["credit"]["unallocated_reason"]),
        (&json!("unattributable"), &json!([]), &json!("1"), &json!("unattributable")));
    assert_eq!(fixes_show(&f, None)["head_seq"], json!(20));
}

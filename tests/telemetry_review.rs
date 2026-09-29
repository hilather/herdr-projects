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

/// The factory side of a protocol world: task `work`, one result submission
/// per `c` in `artifacts` (id `c…c`, candidate `c…c`) by the author attempt,
/// reviewer attempts `rev-*` and the retained Codex profile `fast`.
fn artifact_world(f: &Fixture, artifacts: &[char], reviewers: &[&str]) {
    let db_path = f.project.join(".state/state.db");
    plant_profile(&db_path, fast_profile(f));
    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute("INSERT INTO task_contracts(task_id,contract_revision,plan_revision,project_store,expected_head,repository,base_oid,object_format,memory_snapshot_id,route,raw_bytes,raw_digest,installed_seq)
        VALUES('work',1,NULL,'store',0,'/repo',?1,'sha1',NULL,'verify_only',x'61',?2,(SELECT max(sequence) FROM events))", rusqlite::params![oid('b'), hex('c')]).unwrap();
    for (i, c) in artifacts.iter().enumerate() {
        db.execute("INSERT INTO result_submissions(submission_id,project_store,idempotency_key,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,artifact_manifest,claimed_checks,created_unix_ms)
            VALUES(?1,'store',?1,?2,'{}','work',1,?2,?3,'/repo',?4,?5,'sha1','[]','[]',?6)", rusqlite::params![hex(*c), hex('d'), f.attempt, oid('b'), oid(*c), 1000 + i as i64]).unwrap();
    }
    for attempt in reviewers {
        db.execute("INSERT INTO attempts(id,task_id,revision,state,reservation,termination_observed) VALUES(?1,'work',1,'completed',?1,1)", [attempt]).unwrap();
    }
}

const SKEPTICAL: &str = "skeptical-challenge.v1";
const BUDGET: &str = "1800000";

/// The example skeptical protocol of contracts-review.md §7.
fn skeptical_protocol() -> serde_json::Value {
    json!({"schema": "review_protocol.v1", "protocol": SKEPTICAL, "kind": "skeptical", "scope": "candidate_diff", "role": "evaluation",
        "challenges": ["unsupported_claims", "missed_edge_cases", "unsafe_concurrency", "missing_acceptance_criteria", "evidence_gaps"],
        "failure_classes": ["logic", "boundary", "concurrency", "security", "test_weakening", "requirement_omission"],
        "permitted_tools": ["read", "test"], "budget_ms": 1_800_000, "evidence_min": 1, "stopping_rule": "checklist_complete",
        "prior_disclosure": "withheld", "reviewer_profile": null,
        "outcome": {"primary": "new_validated_unique_findings.v1", "adjudication": "owner_triage.v1", "severity_policy": "finding_severity.v1", "min_severity": "low"}})
}

fn input(f: &Fixture, name: &str, body: &serde_json::Value) -> String {
    let path = f.tmp.path().join(name);
    fs::write(&path, body.to_string()).unwrap();
    path.to_str().unwrap().to_owned()
}

/// Open a review of artifact `c` (method `kind`, `protocol`, optional budget); returns its id.
fn open_review(f: &Fixture, c: char, kind: &str, protocol: &str, budget: Option<&str>) -> String {
    let sub = hex(c);
    let mut args = vec!["review", "open", sub.as_str(), "--kind", kind, "--protocol", protocol];
    if let Some(budget) = budget { args.extend(["--budget-ms", budget]); }
    f.cli_args(&args).0["opportunity"]["opportunity_id"].as_str().unwrap().to_owned()
}

/// Assign `fast`, start `attempt`'s session and complete it on artifact `c` with `findings` and `evidence`.
fn run_review(f: &Fixture, opportunity: &str, c: char, attempt: &str, findings: serde_json::Value, evidence: serde_json::Value) -> serde_json::Value {
    f.cli_args(&["review", "assign", opportunity, "--reviewer", "fast"]);
    let session = f.cli_args(&["review", "start", opportunity, "--attempt", attempt]).0["session"]["session_id"].as_str().unwrap().to_owned();
    let receipt = json!({"schema": "review_receipt.v1", "session_id": session, "submission_id": hex(c), "candidate_oid": oid(c), "outcome": "completed",
        "findings": findings, "evidence": evidence});
    f.cli_args(&["review", "complete", "--input-file", &input(f, &format!("{attempt}.json"), &receipt)]).0["completion"].clone()
}

fn protocols_show(f: &Fixture, as_of: Option<i64>) -> serde_json::Value {
    let seq = as_of.map(|s| s.to_string());
    let mut args = vec!["review", "protocols", "show"];
    if let Some(seq) = &seq { args.extend(["--as-of", seq.as_str()]); }
    f.cli_args(&args).0["protocols"].clone()
}

/// TM3.4 incremental yield golden. History by hand: seq 1 registers
/// `skeptical-challenge.v1`; 2 is ordinary review O1's (code, S1) submission
/// `null-deref`; 3 validates it as K = `finding:canonical-3`. 4 binds the
/// skeptical pass P1 (S1, prior O1): same artifact, prior coverage complete,
/// cutoff 3, so K is known. P1 reports `alias`, `leak`, `reworded` and
/// `style` (submissions 2–5 at seq 5–8, claims 2–5). 9 marks `reworded` a
/// duplicate of K; 10 validates `alias` as new A = `finding:canonical-10`;
/// 11 validates `leak` as L = `finding:canonical-11`; 12 rejects `style`.
/// Then P1 yields A and L: M28 = 2/1 (one rediscovery). 13 merges A into K
/// (same root cause, reworded): only L is new, M28 = 1/1, 2 rediscoveries;
/// as of 12 it is still 2. 14 binds P2 on S2 (a later candidate) after O1:
/// `changed_artifact`, its own opportunity; its finding (15, validated at 16)
/// is never incremental. 17 binds P3 on S1 after O2, which never ran:
/// `incomplete_prior_coverage`. Final M28 = 1/1, excluded 1 + 1.
#[test]
fn reworded_duplicates_add_no_incremental_discovery_and_new_artifact_is_new_opportunity() {
    let f = Fixture::new();
    artifact_world(&f, &['1', '2'], &["rev-a1", "rev-k1", "rev-k2", "rev-kz"]);
    let db_path = f.project.join(".state/state.db");
    let protocol = input(&f, "protocol.json", &skeptical_protocol());

    // No protocol store rows yet: M28's denominator is empty, never 0.
    let m28 = f.cli_args(&["review", "report"]).0["metrics"]["M28"].clone();
    assert_eq!((&m28["value"], &m28["reason"], &m28["estimate"]), (&json!(null), &json!("empty_denominator"), &json!("descriptive")));

    // Only the owner registers; a protocol is immutable and carries tokens, never prose.
    let mut store = SqliteStore::open(&db_path).unwrap();
    for principal in ["worker:rev-k1", "rev-k1"] {
        let err = store.register_review_protocol(skeptical_protocol().to_string().as_bytes(), None, principal, 1).unwrap_err();
        assert!(format!("{err:?}").contains("a worker cannot"), "{principal}");
    }
    drop(store);
    let mut prose = skeptical_protocol();
    prose["prompt"] = json!("Challenge every claim the author makes");
    assert!(f.cli_fail(&["review", "protocols", "register", "--input-file", &input(&f, "prose.json", &prose)]).contains("unknown field `prompt`"));
    let mut sentence = skeptical_protocol();
    sentence["challenges"] = json!(["Look for missing edge cases"]);
    assert!(f.cli_fail(&["review", "protocols", "register", "--input-file", &input(&f, "sentence.json", &sentence)]).contains("is not a lowercase identifier"));
    let registered = f.cli_args(&["review", "protocols", "register", "--input-file", &protocol]).0["event"].clone();
    assert_eq!((&registered["seq"], &registered["kind"], &registered["subject"]["protocol"], &registered["authority"]),
        (&json!(1), &json!("protocol_registered"), &json!(SKEPTICAL), &json!("operator_owner.v1")));
    assert!(f.cli_fail(&["review", "protocols", "register", "--input-file", &protocol]).contains("needs a new versioned identifier"));

    // Ordinary review O1 of S1 finds K.
    let o1 = open_review(&f, '1', "code", "review-protocol.v1", None);
    run_review(&f, &o1, '1', "rev-a1", json!(["finding:null-deref"]), json!([]));
    f.cli_args(&["review", "findings", "validate", "1", "--new", "--severity", "high", "--evidence", &evidence('e')]);

    // A pass runs under its protocol's scope, role and assigned budget, and is bound before it starts.
    let no_budget = open_review(&f, '1', "skeptical", SKEPTICAL, None);
    assert!(f.cli_fail(&["review", "protocols", "bind", &no_budget, "--prior", &o1]).contains("assigned budget"));
    let started = open_review(&f, '1', "skeptical", SKEPTICAL, Some("1800000"));
    f.cli_args(&["review", "assign", &started, "--reviewer", "fast"]);
    f.cli_args(&["review", "start", &started, "--attempt", "rev-kz"]);
    assert!(f.cli_fail(&["review", "protocols", "bind", &started, "--prior", &o1]).contains("a pass is bound before its review starts"));
    let p1 = open_review(&f, '1', "skeptical", SKEPTICAL, Some(BUDGET));
    let bound = f.cli_args(&["review", "protocols", "bind", &p1, "--prior", &o1]).0["event"].clone();
    assert_eq!((&bound["seq"], &bound["subject"]["cutoff_seq"], &bound["subject"]["comparability"], &bound["subject"]["prior_coverage"]),
        (&json!(4), &json!(3), &json!("same_artifact"), &json!("complete")));
    assert!(f.cli_fail(&["review", "protocols", "bind", &p1, "--prior", &o1]).contains("already a bound pass"));

    // The skeptical session reports four findings; two are rewordings of K.
    let done = run_review(&f, &p1, '1', "rev-k1", json!([{"ref": "finding:reworded", "title": "Loader dereferences null when the config file is absent"},
        {"ref": "finding:alias", "title": "Missing config file crashes startup"}, "finding:leak", "finding:style"]), json!([evidence('a')]));
    assert_eq!(done["finding_submissions"], json!([2, 3, 4, 5]));
    f.cli_args(&["review", "findings", "duplicate", "4", "--of", "finding:canonical-3"]);
    f.cli_args(&["review", "findings", "validate", "2", "--new", "--severity", "high", "--evidence", &evidence('b')]);
    f.cli_args(&["review", "findings", "validate", "3", "--new", "--severity", "medium", "--evidence", &evidence('c')]);
    f.cli_args(&["review", "findings", "reject", "5", "--reason", "out_of_scope"]);
    let pass = protocols_show(&f, None)["passes"][0].clone();
    assert_eq!((&pass["opportunity_id"], &pass["known_findings"], &pass["new_unique_findings"], &pass["rediscovered"], &pass["eligible"]),
        (&json!(p1), &json!(["finding:canonical-3"]), &json!(["finding:canonical-10", "finding:canonical-11"]), &json!(1), &json!(true)));
    assert_eq!(pass["claims"].as_array().unwrap().iter().map(|c| (c["claim_id"].as_i64().unwrap(), c["incremental"].as_str().unwrap())).collect::<Vec<_>>(),
        [(2, "new"), (3, "new"), (4, "rediscovered"), (5, "rejected")]);
    let m28 = f.cli_args(&["review", "report"]).0["metrics"]["M28"].clone();
    assert_eq!((&m28["numerator"], &m28["denominator"], &m28["value"], &m28["rediscovered"]), (&json!(2), &json!(1), &json!("2/1"), &json!(1)));

    // The owner merges the reworded A into K: A was never a new discovery.
    assert_eq!(f.cli_args(&["review", "findings", "merge", "finding:canonical-10", "--into", "finding:canonical-3"]).0["event"]["seq"], json!(13));
    let pass = protocols_show(&f, None)["passes"][0].clone();
    assert_eq!((&pass["new_unique_findings"], &pass["rediscovered"]), (&json!(["finding:canonical-11"]), &json!(2)));
    assert_eq!(protocols_show(&f, Some(12))["passes"][0]["new_unique_findings"], json!(["finding:canonical-10", "finding:canonical-11"]));

    // A later candidate is another artifact: its pass is its own opportunity, labelled changed.
    let p2 = open_review(&f, '2', "skeptical", SKEPTICAL, Some(BUDGET));
    assert_ne!(p2, p1);
    assert_eq!(f.cli_args(&["review", "protocols", "bind", &p2, "--prior", &o1]).0["event"]["subject"]["comparability"], json!("changed_artifact"));
    f.cli_args(&["review", "assign", &p2, "--reviewer", "fast"]);
    let session = f.cli_args(&["review", "start", &p2, "--attempt", "rev-k2"]).0["session"]["session_id"].as_str().unwrap().to_owned();
    // It cannot report on S1's candidate as if it were the same review.
    let masquerade = json!({"schema": "review_receipt.v1", "session_id": session, "submission_id": hex('1'), "candidate_oid": oid('1'), "outcome": "completed",
        "findings": ["finding:s2-bug"], "evidence": [evidence('a')]});
    assert!(f.cli_fail(&["review", "complete", "--input-file", &input(&f, "masquerade.json", &masquerade)]).contains("names another candidate"));
    let honest = json!({"schema": "review_receipt.v1", "session_id": session, "submission_id": hex('2'), "candidate_oid": oid('2'), "outcome": "completed",
        "findings": ["finding:s2-bug"], "evidence": [evidence('a')]});
    assert_eq!(f.cli_args(&["review", "complete", "--input-file", &input(&f, "honest.json", &honest)]).0["completion"]["finding_submissions"], json!([6]));
    f.cli_args(&["review", "findings", "validate", "6", "--new", "--severity", "high", "--evidence", &evidence('d')]);

    // A pass after a prior that never ran has incomplete prior coverage.
    let o2 = open_review(&f, '1', "code", "review-protocol.v1", None);
    let p3 = open_review(&f, '1', "skeptical", SKEPTICAL, Some(BUDGET));
    let bound = f.cli_args(&["review", "protocols", "bind", &p3, "--prior", &o2]).0["event"].clone();
    assert_eq!((&bound["seq"], &bound["subject"]["prior_coverage"], &bound["subject"]["priors"][0]["status"]), (&json!(17), &json!("incomplete"), &json!("unassigned")));

    let shown = protocols_show(&f, None);
    let by = |id: &str| shown["passes"].as_array().unwrap().iter().find(|p| p["opportunity_id"] == id).unwrap().clone();
    let changed = by(&p2);
    assert_eq!((&changed["candidate_oid"], &changed["priors"][0]["artifact"], &changed["new_unique_findings"], &changed["eligible"], &changed["exclusion"]),
        (&json!(oid('2')), &json!("changed_artifact"), &json!(["finding:canonical-16"]), &json!(false), &json!("changed_artifact")));
    assert_eq!(by(&p3)["exclusion"], json!("incomplete_prior_coverage"));
    assert_eq!(shown["history"].as_array().unwrap().iter().map(|e| (e["seq"].as_i64().unwrap(), e["kind"].as_str().unwrap())).collect::<Vec<_>>(),
        [(1, "protocol_registered"), (4, "pass_bound"), (14, "pass_bound"), (17, "pass_bound")]);

    let m28 = f.report()["metrics"]["M28"].clone();
    assert_eq!((&m28["numerator"], &m28["denominator"], &m28["value"], &m28["rediscovered"]), (&json!(1), &json!(1), &json!("1/1"), &json!(2)));
    assert_eq!((&m28["excluded"], &m28["by_protocol"]), (&json!({"changed_artifact": 1, "incomplete_prior_coverage": 1}),
        &json!({SKEPTICAL: {"value": "1/1", "prior_disclosure": "withheld"}})));
    assert_eq!((&m28["estimate"], &m28["observational"], &m28["causal"]), (&json!("descriptive"), &json!(true), &unavailable("not_randomized")));
    // The triage view still counts every submission once.
    assert_eq!(f.cli_args(&["review", "findings", "show"]).0["findings"]["summary"]["submissions"], json!(6));
}

fn experiments_show(f: &Fixture, as_of: Option<i64>) -> serde_json::Value {
    let seq = as_of.map(|s| s.to_string());
    let mut args = vec!["review", "experiments", "show"];
    if let Some(seq) = &seq { args.extend(["--as-of", seq.as_str()]); }
    f.cli_args(&args).0["experiments"]["experiments"][0].clone()
}

/// TM3.4 preregistration golden. Seq 1 registers the skeptical protocol, 2
/// preregisters the randomized experiment E (seed a…a, reference arm
/// `standard`, `skeptical` adds a pass, min 2 units per arm). A code review X
/// of S1 already completed, so it cannot be assigned. 3–6 assign the base
/// reviews B1–B4 (S1–S4); the seed gives, by
/// `sha256("review_experiment.v1:" + seed + ":" + submission)` mod 2:
/// standard, skeptical, standard, skeptical. Base submissions: B1 `b1` (seq
/// 7), B2 `b2` (8), B3 none, B4 `b4` (9). 10 and 11 bind passes P2 (after B2)
/// and P4 (after B4); P2 reports `k2-new` (12) and `k2-reworded` (13), P4
/// `k4-reworded` (14). 15–18 validate b1, b2, b4 and k2-new as new; 19 and 20
/// mark the rewordings duplicates. Outcomes: S1 1, S2 2 (b2 + k2-new), S3 0,
/// S4 1 (the reworded report adds nothing). Means 1/2 and 3/2: difference
/// 3/2 − 1/2 = 1. As of 14 only S3 is analyzable. The descriptive M28 over
/// P2 and P4 is 1/2. 21 excludes S3: standard has 1 analyzable unit, so the
/// difference is unavailable. 22 binds a pass after B1 (standard):
/// crossover, still analyzed as assigned.
#[test]
fn preregistered_assignment_frozen_before_outcomes_observational_is_descriptive() {
    let f = Fixture::new();
    artifact_world(&f, &['1', '2', '3', '4'], &["rev-x", "rev-b1", "rev-b2", "rev-b3", "rev-b4", "rev-k2", "rev-k4"]);
    let db_path = f.project.join(".state/state.db");
    f.cli_args(&["review", "protocols", "register", "--input-file", &input(&f, "protocol.json", &skeptical_protocol())]);
    let experiment = json!({"schema": "review_experiment.v1", "experiment": "skeptical-vs-standard.v1", "design": "randomized", "seed": hex('a'),
        "eligibility": {"kind": "code", "scope": "candidate_diff", "role": "evaluation", "protocol": "review-protocol.v1"},
        "arms": [{"arm": "standard", "protocol": null}, {"arm": "skeptical", "protocol": SKEPTICAL}],
        "primary_outcome": "validated_unique_findings.v1", "adjudication": "owner_triage.v1", "horizon_days": 14, "min_units": 2, "stopping_rule": "fixed_horizon", "planned_units": 4});

    // Preregistration: owner only, never withholding a gate, arms under registered protocols, a recorded seed.
    let mut store = SqliteStore::open(&db_path).unwrap();
    let err = store.register_review_experiment(experiment.to_string().as_bytes(), None, "worker:rev-b1", 1).unwrap_err();
    assert!(format!("{err:?}").contains("a worker cannot"), "{err:?}");
    drop(store);
    let variant = |name: &str, path: &str, value: serde_json::Value| {
        let mut body = experiment.clone();
        body.pointer_mut(path).map(|v| *v = value).unwrap();
        f.cli_fail(&["review", "experiments", "register", "--input-file", &input(&f, name, &body)])
    };
    assert!(variant("gate.json", "/eligibility/role", json!("gate")).contains("never withheld"));
    assert!(variant("seedless.json", "/seed", json!(null)).contains("records its seed"));
    assert!(variant("unregistered.json", "/arms/1/protocol", json!("skeptical-challenge.v9")).contains("is not registered"));
    let registered = f.cli_args(&["review", "experiments", "register", "--input-file", &input(&f, "experiment.json", &experiment)]).0["event"].clone();
    assert_eq!((&registered["seq"], &registered["subject"]["design"]), (&json!(2), &json!("randomized")));
    // Frozen: a raw rewrite of the preregistration aborts.
    let raw = rusqlite::Connection::open(&db_path).unwrap().execute("UPDATE review_experiments SET min_units=1", []).unwrap_err();
    assert!(raw.to_string().contains("append-only"), "{raw}");

    // Assignment precedes outcomes: a review that already completed cannot be enrolled (store and trigger).
    let x = open_review(&f, '1', "code", "review-protocol.v1", None);
    run_review(&f, &x, '1', "rev-x", json!([]), json!([]));
    assert!(f.cli_fail(&["review", "experiments", "assign", "skeptical-vs-standard.v1", &x]).contains("assignment is frozen before outcomes"));
    {
        let mut db = rusqlite::Connection::open(&db_path).unwrap();
        let tx = db.transaction().unwrap();
        tx.execute("INSERT INTO protocol_log(seq,kind,principal,authority,expected_seq,recorded_unix_ms) VALUES(3,'unit_assigned','operator:cli','operator_owner.v1',NULL,1)", []).unwrap();
        let raw = tx.execute("INSERT INTO experiment_units(seq,experiment_seq,opportunity_id,submission_id,arm,block) VALUES(3,2,?1,?2,'standard',NULL)", [&x, &hex('1')]).unwrap_err();
        assert!(raw.to_string().contains("before any outcome"), "{raw}");
    }
    let ineligible = open_review(&f, '1', "security", "review-protocol.v1", None);
    assert!(f.cli_fail(&["review", "experiments", "assign", "skeptical-vs-standard.v1", &ineligible]).contains("kind security, the preregistration requires code"));

    let base: Vec<String> = ['1', '2', '3', '4'].into_iter().map(|c| open_review(&f, c, "code", "review-protocol.v1", None)).collect();
    assert!(f.cli_fail(&["review", "experiments", "assign", "skeptical-vs-standard.v1", &base[0], "--block", "b", "--arm", "skeptical"]).contains("from its recorded seed"));
    let arms: Vec<(i64, String)> = base.iter().map(|b| {
        let e = f.cli_args(&["review", "experiments", "assign", "skeptical-vs-standard.v1", b]).0["event"].clone();
        (e["seq"].as_i64().unwrap(), e["subject"]["arm"].as_str().unwrap().to_owned())
    }).collect();
    assert_eq!(arms, [(3, "standard".into()), (4, "skeptical".into()), (5, "standard".into()), (6, "skeptical".into())]);
    let again = open_review(&f, '1', "code", "review-protocol.v1", None);
    assert!(f.cli_fail(&["review", "experiments", "assign", "skeptical-vs-standard.v1", &again]).contains("is already a unit"));

    // Base reviews, then the treatment arm's passes.
    run_review(&f, &base[0], '1', "rev-b1", json!(["finding:b1"]), json!([]));
    run_review(&f, &base[1], '2', "rev-b2", json!(["finding:b2"]), json!([]));
    run_review(&f, &base[2], '3', "rev-b3", json!([]), json!([]));
    run_review(&f, &base[3], '4', "rev-b4", json!(["finding:b4"]), json!([]));
    let p2 = open_review(&f, '2', "skeptical", SKEPTICAL, Some(BUDGET));
    let p4 = open_review(&f, '4', "skeptical", SKEPTICAL, Some(BUDGET));
    assert_eq!(f.cli_args(&["review", "protocols", "bind", &p2, "--prior", &base[1]]).0["event"]["seq"], json!(10));
    assert_eq!(f.cli_args(&["review", "protocols", "bind", &p4, "--prior", &base[3]]).0["event"]["seq"], json!(11));
    assert_eq!(run_review(&f, &p2, '2', "rev-k2", json!(["finding:k2-new", "finding:k2-reworded"]), json!([evidence('a')]))["finding_submissions"], json!([4, 5]));
    assert_eq!(run_review(&f, &p4, '4', "rev-k4", json!(["finding:k4-reworded"]), json!([evidence('a')]))["finding_submissions"], json!([6]));
    for claim in ["1", "2", "3", "4"] { f.cli_args(&["review", "findings", "validate", claim, "--new", "--severity", "medium", "--evidence", &evidence('e')]); }
    f.cli_args(&["review", "findings", "duplicate", "5", "--of", "finding:canonical-16"]);
    f.cli_args(&["review", "findings", "duplicate", "6", "--of", "finding:canonical-17"]);

    let e = experiments_show(&f, None);
    assert_eq!(e["units"].as_array().unwrap().iter().map(|u| (u["arm"].as_str().unwrap(), u["status"].as_str().unwrap(), u["outcome"].as_i64().unwrap(), u["treatment_received"].clone()))
        .collect::<Vec<_>>(), [("standard", "analyzable", 1, json!(null)), ("skeptical", "analyzable", 2, json!(true)), ("standard", "analyzable", 0, json!(null)), ("skeptical", "analyzable", 1, json!(true))]);
    assert_eq!(e["units"][1]["new_unique_findings"], json!(["finding:canonical-16", "finding:canonical-18"]));
    let est = &e["estimate"];
    assert_eq!((&est["estimate"], &est["analysis"], &est["reference_arm"], &est["uncertainty"]),
        (&json!("randomized"), &json!("intention_to_treat"), &json!("standard"), &unavailable("interval_not_computed")));
    assert_eq!((&est["arms"]["standard"]["mean"], &est["arms"]["skeptical"]["mean"], &est["arms"]["skeptical"]["outcome_total"]), (&json!("1/2"), &json!("3/2"), &json!(3)));
    assert_eq!(est["differences"], json!({"skeptical": {"value": "1", "analyzable": [2, 2]}}));
    // Replay: before any triage only S3 (no findings) was settled.
    let early = experiments_show(&f, Some(14));
    assert_eq!(early["units"].as_array().unwrap().iter().map(|u| u["status"].as_str().unwrap()).collect::<Vec<_>>(), ["pending", "pending", "analyzable", "pending"]);
    assert_eq!(early["estimate"]["differences"]["skeptical"], json!({"value": unavailable("insufficient_data"), "analyzable": [1, 0]}));

    // Observational M28 over the same passes stays descriptive; the experiment's estimate sits beside it.
    let m28 = f.report()["metrics"]["M28"].clone();
    assert_eq!((&m28["value"], &m28["estimate"], &m28["observational"], &m28["causal"], &m28["rediscovered"], &m28["control_opportunities"]),
        (&json!("1/2"), &json!("descriptive"), &json!(true), &unavailable("not_randomized"), &json!(2), &json!(2)));
    assert_eq!(m28["experiments"]["skeptical-vs-standard.v1"]["differences"]["skeptical"]["value"], json!("1"));
    assert_eq!(m28["experiments"]["skeptical-vs-standard.v1"]["estimate"], json!("randomized"));

    // An exclusion stays listed; below the preregistered minimum there is no estimate.
    assert!(f.cli_fail(&["review", "experiments", "exclude", "skeptical-vs-standard.v1", &base[2], "--reason", "changed_my_mind"]).contains("unknown exclusion reason"));
    assert_eq!(f.cli_args(&["review", "experiments", "exclude", "skeptical-vs-standard.v1", &base[2], "--reason", "operator_error"]).0["event"]["seq"], json!(21));
    let e = experiments_show(&f, None);
    assert_eq!((&e["units"][2]["status"], &e["units"][2]["arm"], &e["units"][2]["exclusion"]), (&json!("excluded"), &json!("standard"), &json!({"seq": 21, "reason": "operator_error"})));
    assert_eq!((&e["estimate"]["arms"]["standard"]["excluded"], &e["estimate"]["differences"]["skeptical"]),
        (&json!({"operator_error": 1}), &json!({"value": unavailable("insufficient_data"), "analyzable": [1, 2]})));

    // Crossover: a pass after a standard unit is recorded and the unit stays in its assigned arm.
    let p1 = open_review(&f, '1', "skeptical", SKEPTICAL, Some(BUDGET));
    assert_eq!(f.cli_args(&["review", "protocols", "bind", &p1, "--prior", &base[0]]).0["event"]["seq"], json!(22));
    let e = experiments_show(&f, None);
    assert_eq!((&e["units"][0]["arm"], &e["units"][0]["crossover"], &e["units"][0]["status"], &e["units"][0]["passes"]), (&json!("standard"), &json!(true), &json!("pending"), &json!([p1])));
    assert_eq!(e["estimate"]["arms"]["standard"]["crossover"], json!(1));

    // A matched design takes the owner's block and arm, never a seed.
    let mut matched = experiment.clone();
    matched["experiment"] = json!("skeptical-matched.v1");
    matched["design"] = json!("matched");
    matched["seed"] = json!(null);
    matched["match_on"] = json!(["task_class", "repository"]);
    f.cli_args(&["review", "experiments", "register", "--input-file", &input(&f, "matched.json", &matched)]);
    assert!(f.cli_fail(&["review", "experiments", "assign", "skeptical-matched.v1", &again]).contains("needs --block and --arm"));
    assert_eq!(f.cli_args(&["review", "experiments", "assign", "skeptical-matched.v1", &again, "--block", "pair-1", "--arm", "skeptical"]).0["event"]["subject"]["block"], json!("pair-1"));
}

/// The synthetic starter seed set (tests only): `(class, seeded source, reproducer)` by id.
fn seed_set() -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/telemetry/seeds/starter-seed-set.json")).unwrap()).unwrap()
}
fn seed_fixture(id: &str) -> serde_json::Value { seed_set()["seeds"].as_array().unwrap().iter().find(|s| s["id"] == id).unwrap().clone() }
/// The registered reproducer reference: its sha256, never its text.
fn reproducer_ref(seed: &serde_json::Value) -> String { format!("sha256:{:x}", Sha256::digest(seed["reproducer"].as_str().unwrap().as_bytes())) }

/// A disposable real project for the integration path: an owner signing key,
/// an active `demo` project, and a SHA-256 git repository whose base holds
/// the starter set's clean source.
struct IntegrationLab { home: tempfile::TempDir, root: std::path::PathBuf, project: std::path::PathBuf, key: std::path::PathBuf, repo: std::path::PathBuf, store: String, base: String }

impl IntegrationLab {
    fn new() -> Self {
        use herdr_projects::{domain::ProjectState, migration, runtime};
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        let lab = |args: &[&str]| std::process::Command::new(BIN).env_clear().env("HOME", home.path()).args(args).output().unwrap();
        for action in ["new", "pause"] { assert!(lab(&["--root", root.to_str().unwrap(), action, "demo"]).status.success()); }
        let key = home.path().join("owner");
        assert!(std::process::Command::new("/usr/bin/ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&key).output().unwrap().status.success());
        let public = fs::read_to_string(key.with_extension("pub")).unwrap().split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        let project = root.join("demo");
        let config = home.path().join(".config/herdr-projects/config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, format!("[authority]\nversion=1\nrevision=1\napproval_public_key={public:?}\n[profiles.worker]\nkind='claude'\npermission_policy='interactive'\n[profiles.worker.budget]\nmax_wall_seconds=60\nunknown_usage='allow_with_warning'\n")).unwrap();
        let plan = migration::inspect_with_config(&project, &config).unwrap();
        migration::apply(&project, &plan, true).unwrap();
        let s = runtime::snapshot(&project).unwrap();
        runtime::set_state(&project, s.head, s.control.unwrap().revision, ProjectState::Active, &config).unwrap();
        let store = project.join(".state/state.db").canonicalize().unwrap().display().to_string();
        let repo = home.path().join("repo");
        fs::create_dir_all(repo.join("src")).unwrap();
        let mut lab = IntegrationLab { home, root, project, key, repo, store, base: String::new() };
        lab.git(&["init", "-q", "--object-format=sha256"]);
        fs::write(lab.repo.join("src/base.txt"), "base\n").unwrap();
        lab.git(&["add", "."]);
        lab.git(&["commit", "-qm", "base"]);
        lab.base = lab.git(&["rev-parse", "HEAD"]);
        lab
    }
    fn hp(&self, args: &[&str]) -> std::process::Output {
        let mut all = vec!["--root", self.root.to_str().unwrap()];
        all.extend_from_slice(args);
        std::process::Command::new(BIN).env_clear().env("HOME", self.home.path()).env("PATH", "/usr/bin:/bin").args(all).output().unwrap()
    }
    fn ok(&self, args: &[&str]) -> serde_json::Value {
        let out = self.hp(args);
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn fail(&self, args: &[&str]) -> String {
        let out = self.hp(args);
        assert!(!out.status.success(), "{args:?} succeeded: {}", String::from_utf8_lossy(&out.stdout));
        String::from_utf8_lossy(&out.stderr).into_owned()
    }
    fn db(&self) -> rusqlite::Connection { rusqlite::Connection::open(self.project.join(".state/state.db")).unwrap() }
    fn git(&self, args: &[&str]) -> String {
        let out = std::process::Command::new("/usr/bin/git").env_clear().env("PATH", "/usr/bin:/bin").env("HOME", self.home.path()).env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "fixture").env("GIT_AUTHOR_EMAIL", "fixture@example.com").env("GIT_COMMITTER_NAME", "fixture").env("GIT_COMMITTER_EMAIL", "fixture@example.com")
            .current_dir(&self.repo).args(args).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }
    /// Test-only seed injection into this disposable repository: branch
    /// `branch` from the base with `src/lib.rs` = `source`; returns the commit.
    fn candidate(&self, branch: &str, source: &str) -> String {
        self.git(&["checkout", "-qb", branch, &self.base]);
        fs::write(self.repo.join("src/lib.rs"), source).unwrap();
        self.git(&["add", "."]);
        self.git(&["commit", "-qm", branch]);
        self.git(&["rev-parse", "HEAD"])
    }
    /// Task `task` with a running attempt, a signed verify-then-integrate contract whose
    /// policy runs `git diff --quiet`, and one submitted result at `candidate`.
    fn submit(&self, task: &str, candidate: &str) -> String {
        use herdr_projects::{authority::CONTRACT_SIGNATURE_NAMESPACE, domain::TaskId, runtime};
        let head = runtime::add_task(&self.project, TaskId::new(task).unwrap(), "work".into(), runtime::snapshot(&self.project).unwrap().head).unwrap();
        self.db().execute("INSERT INTO attempts(id,task_id,revision,state,snapshot,reservation,termination_observed) VALUES(?1,?2,1,'running',NULL,?1,0)", [format!("{task}-attempt"), task.to_owned()]).unwrap();
        let repository = self.repo.canonicalize().unwrap().display().to_string();
        let mut document = serde_json::to_vec_pretty(&json!({
            "version": 3, "outputs": [{"path": "src/lib.rs", "kind": "git_file"}], "scope": {"paths": [{"path": "src/", "access": "write"}]},
            "project_store": self.store, "expected_head": head, "task_id": task, "contract_revision": 1, "deliverable": "ship", "non_goals": "no launch",
            "acceptance_policies": [{"id": "clean", "text": POLICY}], "repository": repository, "base_oid": self.base, "object_format": "sha256",
            "dependencies": [], "capability_flags": [], "profile_kind": "codex", "retry_class": "none", "result_schema_id": "result-v1",
            "route": "verify_then_integrate", "authority": herdr_projects::authority::policy_reference(&self.project).unwrap()})).unwrap();
        document.push(b'\n');
        let doc = self.home.path().join(format!("{task}-contract.json"));
        fs::write(&doc, &document).unwrap();
        assert!(std::process::Command::new("/usr/bin/ssh-keygen").args(["-Y", "sign", "-f"]).arg(&self.key).args(["-n", CONTRACT_SIGNATURE_NAMESPACE]).arg(&doc).status().unwrap().success());
        let installed = self.ok(&["task", "demo", "contract", "put", "--input-file", doc.to_str().unwrap(), "--signature", doc.with_extension("json.sig").to_str().unwrap()]);
        let objects: Vec<serde_json::Value> = self.git(&["rev-list", "--objects", "--all"]).lines()
            .map(|line| { let oid = line.split_whitespace().next().unwrap(); json!({"oid": oid, "relative_path": format!("{}/{}", &oid[..2], &oid[2..])}) }).collect();
        let result = self.home.path().join(format!("{task}-result.json"));
        fs::write(&result, json!({"idempotency_key": format!("{task}-key"), "task_id": task, "contract_revision": 1, "contract_digest": installed["digest"],
            "attempt_id": format!("{task}-attempt"), "repository": repository, "base_oid": self.base, "candidate_oid": candidate, "object_format": "sha256",
            "artifact_manifest": [{"path": "src/lib.rs", "oid": candidate}], "claimed_checks": ["all checks passed"], "objects": objects}).to_string()).unwrap();
        self.ok(&["result", "demo", "submit", "--input-file", result.to_str().unwrap()])["submission_id"].as_str().unwrap().to_owned()
    }
    /// Verify `submission` through the operator CLI; returns its verified result id.
    fn verify(&self, submission: &str, key: &str) -> String {
        let policy = self.home.path().join("policy.json");
        fs::write(&policy, POLICY).unwrap();
        let work = self.home.path().join(format!("{key}-work"));
        let out = self.ok(&["result", "demo", "verify", submission, "--policy-id", "clean", "--policy-file", policy.to_str().unwrap(), "--idempotency-key", key,
            "--work-dir", work.to_str().unwrap(), "--timeout-seconds", "30"]);
        assert_eq!(out["state"], json!("accepted"));
        out["receipt"]["result_id"].as_str().unwrap().to_owned()
    }
    fn telemetry(&self, args: &[&str]) -> serde_json::Value {
        let mut all = vec!["telemetry", "demo"];
        all.extend_from_slice(args);
        self.ok(&all)
    }
}

const POLICY: &str = r#"{"version":1,"checks":["/usr/bin/git","diff","--quiet"]}"#;

/// TM3.6 guard and blindness over the real integration path. Seeded candidate
/// X (starter seed `logic-inverted-guard`, injected into a disposable repo)
/// and clean control C are registered before any review, then both pass
/// verification. With automatic integration on, the producer's turn drops X
/// from the pending projection and enqueues C only (1 job, C's submission).
/// The operator's `result integrate` of X is refused by `begin_integration`
/// before any write (no lease, no integration operation, target unchanged,
/// scratch dir never created), and raw SQL cannot insert an integration job
/// or operation for X. Reviewers' blind presentations of X and C have the
/// same twelve fields and carry nothing about seeds.
#[test]
fn seeded_candidate_never_integrates_and_reviewers_stay_blind() {
    let lab = IntegrationLab::new();
    let seed = seed_fixture("logic-inverted-guard");
    let seeded_oid = lab.candidate("seeded", seed["seeded"].as_str().unwrap());
    let clean_oid = lab.candidate("clean", seed_set()["clean"].as_str().unwrap());
    let x = lab.submit("task-x", &seeded_oid);
    let c = lab.submit("task-c", &clean_oid);
    let reproducer = reproducer_ref(&seed);
    let registered = lab.telemetry(&["review", "seeds", "register", &x, "--seed", &format!("logic={reproducer}")])["event"].clone();
    assert_eq!((&registered["seq"], &registered["kind"], &registered["authority"], &registered["subject"]["arm"], &registered["subject"]["candidate_oid"]),
        (&json!(1), &json!("registered"), &json!("evaluation_owner.v1"), &json!("seeded"), &json!(seeded_oid)));
    assert_eq!(lab.telemetry(&["review", "seeds", "register", &c, "--control"])["event"]["subject"]["arm"], json!("clean_control"));
    let x_result = lab.verify(&x, "verify-x");
    let c_result = lab.verify(&c, "verify-c");

    lab.git(&["branch", "integration", &lab.base]);
    lab.ok(&["result", "demo", "configure-integration", "--repository", lab.repo.to_str().unwrap(), "--reference", "refs/heads/integration"]);
    let head = herdr_projects::runtime::snapshot(&lab.project).unwrap().head.to_string();
    assert_eq!(lab.ok(&["result", "demo", "auto", "--integrate", "on", "--expected-head", &head])["integrate"], json!(true));
    let pending = |db: &rusqlite::Connection| db.query_row("SELECT count(*) FROM pending_integration_work", [], |r| r.get::<_, i64>(0)).unwrap();
    assert_eq!(pending(&lab.db()), 2, "both verified submissions enter the pending projection");
    // The producer (the ticker's integration pass): X was submitted first, yet only C is enqueued.
    let turn = herdr_projects::store::service_project_integration_jobs(&lab.project).unwrap();
    assert_eq!((turn.enqueued, turn.pending), (1, false));
    let db = lab.db();
    let jobs: Vec<String> = db.prepare("SELECT json_extract(payload,'$.submission_id') FROM operations WHERE kind='integration.run'").unwrap()
        .query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!((jobs, pending(&db)), (vec![c.clone()], 0));
    // A later turn re-adds nothing for X.
    assert_eq!(herdr_projects::store::service_project_integration_jobs(&lab.project).unwrap().enqueued, 0);

    // The operator path reaches begin_integration, which refuses before any write.
    let target = lab.git(&["rev-parse", "refs/heads/integration"]);
    let work = lab.home.path().join("integrate-x");
    let refused = lab.fail(&["result", "demo", "integrate", &x_result, "--repository", lab.repo.to_str().unwrap(), "--idempotency-key", "integrate-x", "--work-dir", work.to_str().unwrap()]);
    assert!(refused.contains("a seeded candidate never integrates"), "{refused}");
    assert_eq!(lab.git(&["rev-parse", "refs/heads/integration"]), target);
    assert!(!work.exists());
    let count = |sql: &str| db.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
    assert_eq!((count("SELECT count(*) FROM integration_operations"), count("SELECT count(*) FROM operations WHERE kind='integration.lease'")), (0, 0));
    // Raw SQL cannot create an integration job or operation for X either.
    let job = db.execute("INSERT INTO operations(id,task_id,kind,target,payload_version,payload,payload_hash,expected_revision,due_unix_ms,idempotency_key)
        VALUES('raw-job','task-x','integration.run','refs/heads/integration',1,?1,?2,1,0,'raw-job')", rusqlite::params![json!({"submission_id": x}).to_string(), hex('d')]).unwrap_err();
    assert!(job.to_string().contains("a seeded candidate never integrates"), "{job}");
    let lease = db.execute("INSERT INTO operations(id,task_id,kind,target,payload_version,payload,payload_hash,expected_revision,due_unix_ms,idempotency_key)
        VALUES('raw-lease','task-x','integration.lease','refs/heads/integration',1,?1,?2,1,0,'raw-lease')", rusqlite::params![json!({"result_id": x_result}).to_string(), hex('d')]).unwrap_err();
    assert!(lease.to_string().contains("a seeded candidate never integrates"), "{lease}");
    let operation = db.execute("INSERT INTO integration_operations(operation_id,project_store,idempotency_key,payload_digest,repository,ref_name,expected_old_oid,verified_result_id,candidate_id,state,generation,object_format,checks_passed,reason,created_unix_ms)
        VALUES('raw-op',?1,'raw-op',?2,?3,'refs/heads/integration',?4,?5,NULL,'effect_pending',1,'sha256',0,NULL,1)",
        rusqlite::params![lab.store, hex('d'), lab.repo.canonicalize().unwrap().display().to_string(), lab.base, x_result]).unwrap_err();
    assert!(operation.to_string().contains("a seeded candidate never integrates"), "{operation}");
    // The registry cannot be rewritten to release it.
    assert!(db.execute("UPDATE seeded_candidates SET arm='clean_control'", []).unwrap_err().to_string().contains("append-only"));
    assert!(db.execute("DELETE FROM seeded_candidates", []).unwrap_err().to_string().contains("append-only"));
    // The clean control's verified result stays integrable: the guard names X only.
    assert!(db.query_row("SELECT EXISTS(SELECT 1 FROM verified_results WHERE result_id=?1)", [&c_result], |r| r.get::<_, bool>(0)).unwrap());
    drop(db);

    // Reviewers stay blind: identical field sets, nothing about seeds, arms or reproducers.
    let open = |sub: &str| lab.telemetry(&["review", "open", sub, "--protocol", "review-protocol.v1"])["opportunity"]["opportunity_id"].as_str().unwrap().to_owned();
    let (ox, oc) = (open(&x), open(&c));
    let present = |o: &str| {
        let out = lab.hp(&["telemetry", "demo", "review", "present", o]);
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap()
    };
    let (px, pc) = (present(&ox), present(&oc));
    let keys = |text: &str| serde_json::from_str::<serde_json::Value>(text).unwrap()["presentation"].as_object().unwrap().keys().cloned().collect::<Vec<_>>();
    assert_eq!(keys(&px), keys(&pc));
    assert_eq!(keys(&px).len(), 12);
    let hidden = [reproducer.as_str(), &reproducer[7..], "seed", "control", "logic", "evaluation_owner", "reveal"];
    for text in [&px, &pc] { for word in hidden { assert!(!text.contains(word), "{word} in {text}"); } }
    assert!(px.contains(&seeded_oid) && pc.contains(&clean_oid));
    let shown = String::from_utf8(lab.hp(&["telemetry", "demo", "review", "show"]).stdout).unwrap();
    for word in [&reproducer[7..], "seed", "clean_control"] { assert!(!shown.contains(word), "{word} in review show"); }
    // The owner's view has the arm and the reference, never the seed's source.
    let owner = lab.telemetry(&["review", "seeds", "show"])["seeds"].clone();
    assert_eq!((&owner["candidates"][0]["arm"], &owner["candidates"][0]["seeds"][0]["reproducer_ref"]), (&json!("seeded"), &json!(reproducer)));
    for file in ["state.db", "state.db-wal"] {
        let bytes = fs::read(lab.project.join(".state").join(file)).unwrap_or_default();
        assert!(!bytes.windows(20).any(|w| w == &seed["reproducer"].as_str().unwrap().as_bytes()[..20]), "reproducer text in {file}");
    }
}

/// Doc 10 §5 seeded-review fixture. Seeded candidates S1–S4 (starter seeds
/// logic, boundary, security, test_weakening; seeds 1–4) and clean controls
/// C1, C2 are registered at seq 1–6. Configuration R (`fast`) reviews each
/// once: S1 reports two findings (claims 1, 2; seq 7, 8), S2–S4, C1, C2 one
/// each (claims 3–7; seq 9–13). A second review of S1 by Q (`claude`) times
/// out: not completed, so no trial (`not_completed` 1), never a 0. Before
/// triage every trial is pending: M43 and M44 are null (empty denominator)
/// with pending 4 and 2. Seq 14–20 triage: claims 1–5 and 7 validated as new
/// findings, claim 6 rejected. Seq 21–23 link claims 1, 3, 4 to seeds 1–3.
/// By hand: M43 = 3/4 = 75.00 (S4's seed missed; its validated claim 5 is an
/// ordinary finding), M44 = 1/2 = 50.00 (C1's rejected-only submission; C2's
/// validated finding is not a false alarm), per configuration R the same;
/// each seed class has 1 trial, below `--min-trials 2`, so suppressed with
/// counts. M22 = 6/7: detection changes no triage. Seq 24 resets claim 1:
/// seed 1 pending, M43 = 2/3. Seq 25 validates it again: 3/4. Seq 26
/// retracts detection 23: 2/4; as of 25 still 3/4. Seq 27 re-links it: 3/4.
/// Seq 28 reveals S1 (every review ended), seq 29 discards it.
#[test]
fn seeded_recall_and_clean_control_false_alarms_match_fixture() {
    let f = Fixture::new();
    let world = review_world(&f, &["rev-a1", "rev-a2", "rev-a3", "rev-a4", "rev-a5", "rev-a6", "rev-a7"]);
    let db_path = f.project.join(".state/state.db");
    let claude = codex_profile(&f.config, "claude", "claude", None);
    let (r_id, q_id) = (agent_configuration(&fast_profile(&f)).id, agent_configuration(&claude).id);
    plant_profile(&db_path, claude);
    let factory = Factory::open(&f);
    let subs: Vec<(String, String)> = std::iter::once(world.clone()).chain(['2', '3', '4', '5', '6', '7'].into_iter().map(|c| {
        factory.submission(&hex(c), &f.attempt, &oid(c), 1_000);
        (hex(c), oid(c))
    })).collect();
    let (s, c1, c2, unregistered) = (&subs[..4], &subs[4], &subs[5], &subs[6]);
    let seeds = ["logic-inverted-guard", "boundary-off-by-one", "security-unchecked-index", "test-weakening-ignored-case"].map(seed_fixture);
    let seeds_cmd = |args: &[&str]| { let mut all = vec!["review", "seeds"]; all.extend_from_slice(args); f.cli_args(&all).0 };
    let seeds_fail = |args: &[&str]| { let mut all = vec!["review", "seeds"]; all.extend_from_slice(args); f.cli_fail(&all) };

    // Only the evaluation authority registers; a reproducer is a reference, never the seed.
    let mut store = SqliteStore::open(&db_path).unwrap();
    let arm = herdr_projects::store::EvaluationArm::Seeded(vec![herdr_projects::store::SeedSpec { seed_class: "logic".into(), reproducer_ref: reproducer_ref(&seeds[0]) }]);
    for worker in ["worker:rev-a1", "rev-a1", f.attempt.as_str()] {
        assert!(format!("{:?}", store.register_evaluation_candidate(&s[0].0, &arm, None, worker, 1).unwrap_err()).contains("a worker cannot register"), "{worker}");
    }
    assert!(format!("{:?}", store.register_evaluation_candidate(&s[0].0, &arm, None, "import:x", 1).unwrap_err()).contains("an import cannot"));
    drop(store);
    assert!(seeds_fail(&["register", &s[0].0, "--seed", "logic=min became max in window"]).contains("never the seed itself"));
    assert!(seeds_fail(&["register", &s[0].0, "--seed", &format!("typo={}", reproducer_ref(&seeds[0]))]).contains("unknown seed class"));
    let raw = rusqlite::Connection::open(&db_path).unwrap().execute("INSERT INTO seed_log(seq,kind,principal,authority,recorded_unix_ms) VALUES(1,'registered','worker:rev-a1','evaluation_owner.v1',1)", []).unwrap_err();
    assert!(raw.to_string().contains("CHECK constraint failed"), "{raw}");
    for (i, (sub, seed)) in s.iter().zip(&seeds).enumerate() {
        let event = seeds_cmd(&["register", &sub.0, "--seed", &format!("{}={}", seed["class"].as_str().unwrap(), reproducer_ref(seed))])["event"].clone();
        assert_eq!((&event["seq"], &event["subject"]["seeds"]), (&json!(i + 1), &json!([i + 1])));
    }
    for sub in [c1, c2] { seeds_cmd(&["register", &sub.0, "--control"]); }
    assert!(seeds_fail(&["register", &s[0].0, "--control"]).contains("already registered"));
    // An arm is fixed before any review: a reviewed candidate cannot join the evaluation.
    f.cli_args(&["review", "open", &unregistered.0, "--protocol", "review-protocol.v1"]);
    assert!(seeds_fail(&["register", &unregistered.0, "--control"]).contains("before any review"));

    // Reviews by R; S1 also by Q, whose session times out.
    let findings = [json!(["finding:s1-a", "finding:s1-b"]), json!(["finding:s2"]), json!(["finding:s3"]), json!(["finding:s4"]), json!(["finding:c1"]), json!(["finding:c2"])];
    for (i, (sub, found)) in s.iter().chain([c1, c2]).zip(findings).enumerate() { completed_review(&f, sub, "code", &format!("rev-a{}", i + 1), found); }
    let q = f.cli_args(&["review", "open", &s[0].0, "--kind", "security", "--protocol", "review-protocol.v1"]).0["opportunity"]["opportunity_id"].as_str().unwrap().to_owned();
    f.cli_args(&["review", "assign", &q, "--reviewer", "claude"]);
    let session = f.cli_args(&["review", "start", &q, "--attempt", "rev-a7"]).0["session"]["session_id"].as_str().unwrap().to_owned();
    assert!(seeds_fail(&["reveal", &s[0].0]).contains("has not ended"));
    let path = f.tmp.path().join("q.json");
    fs::write(&path, json!({"schema": "review_receipt.v1", "session_id": session, "submission_id": s[0].0, "candidate_oid": s[0].1, "outcome": "timed_out", "findings": [], "evidence": []}).to_string()).unwrap();
    f.cli_args(&["review", "complete", "--input-file", path.to_str().unwrap()]);

    let report = |extra: &[&str]| { let mut args = vec!["report", "--min-trials", "2"]; args.extend_from_slice(extra); seeds_cmd(&args)["metrics"].clone() };
    let m = report(&[]);
    assert_eq!((&m["M43"]["value"], &m["M43"]["reason"], &m["M43"]["trials"], &m["M43"]["pending"], &m["M43"]["not_completed"]), (&json!(null), &json!("empty_denominator"), &json!(0), &json!(4), &json!(1)));
    assert_eq!((&m["M44"]["value"], &m["M44"]["controls"], &m["M44"]["pending"], &m["M44"]["not_completed"]), (&json!(null), &json!(0), &json!(2), &json!(0)));

    // Detection only through accepted triage, on the seed's own candidate.
    assert!(seeds_fail(&["detect", "1", "--claim", "1", "--evidence", &evidence('e')]).contains("claim 1 is pending"));
    for claim in ["1", "2", "3", "4", "5", "7"] { f.cli_args(&["review", "findings", "validate", claim, "--new", "--severity", "high", "--evidence", &evidence('e')]); }
    f.cli_args(&["review", "findings", "reject", "6", "--reason", "insufficient_evidence"]);
    assert!(seeds_fail(&["detect", "4", "--claim", "6", "--evidence", &evidence('e')]).contains("another candidate"));
    assert!(seeds_fail(&["detect", "1", "--claim", "3", "--evidence", &evidence('e')]).contains("another candidate"));
    assert!(seeds_fail(&["detect", "1", "--claim", "1"]).contains("at least one evidence reference"));
    for (seed, claim, seq) in [("1", "1", 21), ("2", "3", 22), ("3", "4", 23)] {
        assert_eq!(seeds_cmd(&["detect", seed, "--claim", claim, "--evidence", &evidence('a')])["event"]["seq"], json!(seq));
    }
    assert!(seeds_fail(&["detect", "1", "--claim", "1", "--evidence", &evidence('a')]).contains("already detects a seed"));

    let m = report(&[]);
    let cell = |v: &serde_json::Value, n: &str, d: &str| (v[n].clone(), v[d].clone(), v["pending"].clone(), v["value"].clone(), v["percent"].clone());
    assert_eq!(cell(&m["M43"], "detected", "trials"), (json!(3), json!(4), json!(0), json!("3/4"), json!("75.00")));
    assert_eq!(cell(&m["M44"], "false_alarms", "controls"), (json!(1), json!(2), json!(0), json!("1/2"), json!("50.00")));
    let r = &m["M43"]["by_configuration"][&r_id];
    assert_eq!(cell(r, "detected", "trials"), (json!(3), json!(4), json!(0), json!("3/4"), json!("75.00")));
    assert!(m["M43"]["by_configuration"].get(&q_id).is_none(), "Q completed no review: no trial, never 0");
    let suppressed = json!({"status": "unavailable", "reason": "insufficient_data"});
    assert_eq!(r["by_seed_class"]["test_weakening"], json!({"detected": 0, "trials": 1, "pending": 0, "value": suppressed, "percent": suppressed}));
    assert_eq!(m["M43"]["by_seed_class"]["logic"], json!({"detected": 1, "trials": 1, "pending": 0, "value": suppressed, "percent": suppressed}));
    assert_eq!(m["M43"]["by_kind_protocol"]["code/review-protocol.v1"]["value"], json!("3/4"));
    assert_eq!(cell(&m["M44"]["by_configuration"][&r_id], "false_alarms", "controls"), (json!(1), json!(2), json!(0), json!("1/2"), json!("50.00")));
    // At the default minimum (20) the rates are suppressed, counts still shown, also in the full report.
    let full = f.report()["metrics"].clone();
    assert_eq!(cell(&full["M43"], "detected", "trials"), (json!(3), json!(4), json!(0), suppressed.clone(), suppressed.clone()));
    assert_eq!((&full["M44"]["false_alarms"], &full["M44"]["controls"], &full["M44"]["value"]), (&json!(1), &json!(2), &suppressed));
    // Incidental real findings follow the ordinary path: M22 counts every submission.
    assert_eq!(full["M22"]["value"], json!("6/7"));
    let triage = f.cli_args(&["review", "findings", "show"]).0["findings"].clone();
    assert_eq!(triage["unique_findings"], json!(6));

    // Triage corrections and retractions replay through the one ordering.
    f.cli_args(&["review", "findings", "reset", "1", "--reason", "decided_in_error"]);
    assert_eq!(cell(&report(&[])["M43"], "detected", "trials"), (json!(2), json!(3), json!(1), json!("2/3"), json!("66.67")));
    f.cli_args(&["review", "findings", "validate", "1", "--finding", "finding:canonical-14", "--severity", "high", "--evidence", &evidence('e')]);
    assert_eq!(report(&[])["M43"]["value"], json!("3/4"));
    assert_eq!(seeds_cmd(&["retract", "23"])["event"]["seq"], json!(26));
    assert_eq!(report(&[])["M43"]["value"], json!("2/4"));
    assert_eq!((&report(&["--as-of", "25"])["M43"]["value"], &report(&["--as-of", "25"])["M43"]["as_of_seq"]), (&json!("3/4"), &json!(25)));
    seeds_cmd(&["detect", "3", "--claim", "4", "--evidence", &evidence('b')]);

    // Reveal only after every review ended; nothing reviews it afterwards.
    assert!(seeds_fail(&["dispose", &s[0].0, "--disposition", "discarded"]).contains("not revealed yet"));
    assert_eq!(seeds_cmd(&["reveal", &s[0].0])["event"]["seq"], json!(28));
    let again = f.cli_fail(&["review", "open", &s[0].0, "--protocol", "review-protocol.v1"]);
    assert!(again.contains("not reviewed again"), "{again}");
    assert_eq!(seeds_cmd(&["dispose", &s[0].0, "--disposition", "discarded"])["event"]["seq"], json!(29));
    let m = report(&[]);
    assert_eq!((&m["M43"]["value"], &m["M44"]["value"], &m["M43"]["as_of_seq"]), (&json!("3/4"), &json!("1/2"), &json!(29)));
    let shown = seeds_cmd(&["show"])["seeds"].clone();
    let s1 = &shown["candidates"][0];
    assert_eq!((&s1["revealed_seq"], &s1["disposal"], &s1["seeds"][0]["reproducer_ref"], &s1["seeds"][0]["seed_class"]),
        (&json!(28), &json!("discarded"), &json!(reproducer_ref(&seeds[0])), &json!("logic")));
    let kinds: Vec<&str> = shown["history"].as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["registered", "registered", "registered", "registered", "registered", "registered", "detected", "detected", "detected", "retracted", "detected", "revealed", "disposed"]);
    assert_eq!(shown["trials"].as_array().unwrap().iter().map(|t| t["status"].as_str().unwrap()).collect::<Vec<_>>(), ["detected", "detected", "detected", "missed"]);
}

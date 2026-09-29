//! Lane D review capture end to end (contracts-review.md): opportunities bound
//! to exact candidates, blind assignment, sessions and completions through
//! `herdr-projects telemetry <slug> review ...`, over a real reserved attempt
//! with planted submissions and reviewer attempts.

#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.

mod support;

use herdr_projects::{domain::agent_configuration, store::{FindingTarget, SqliteStore, TriageOutcome, TriageRequest}};
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
    for id in ["M21", "M24"] { assert_eq!(empty[id]["value"], unavailable("discovery_credit_unallocated"), "{id}"); }

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
    assert_eq!((&metrics["M21"]["value"], &metrics["M21"]["drilldown"]), (&unavailable("discovery_credit_unallocated"), &json!({"validated_unique_findings": 3})));
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

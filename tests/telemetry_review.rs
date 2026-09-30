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
    // No validated finding: discovery credit is an observed 0; no closed review opportunity: M24 has no denominator.
    assert_eq!((&empty["M21"]["value"], &empty["M21"]["unallocated"]), (&json!("0"), &json!("0")));
    assert_eq!((&empty["M24"]["value"], &empty["M24"]["reason"]), (&json!(null), &json!("empty_denominator")));
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

    // Acceptance needs a delegated code_review grant (§10): none is installed, so a request and a raw row are both refused.
    let request = receipt("accept.json", json!({"schema": "review_acceptance.v1", "grant_id": format!("sha256:{}", hex('a')), "subject": "reviewer:carol",
        "project_store": db_path.canonicalize().unwrap(), "session_id": sid, "receipt_digest": done["receipt_digest"], "decision": "accepted"}));
    f.cli_fail(&["review", "accept", &sid, "--document", &request, "--signature", &request]);
    let decided = || rusqlite::Connection::open(&db_path).unwrap().query_row("SELECT count(*) FROM review_acceptances", [], |r| r.get::<_, i64>(0)).unwrap();
    assert_eq!(decided(), 0);
    let raw = rusqlite::Connection::open(&db_path).unwrap().execute("INSERT INTO review_acceptances(session_id,decision,authority_principal,authority_ref,authority,receipt_digest,request_digest,request_bytes,request_signature,decided_unix_ms)
        VALUES(?1,'accepted','operator:cli',?2,'delegated_code_review.v1',?3,?2,x'61',x'61',?4)", rusqlite::params![sid, format!("sha256:{}", hex('a')), done["receipt_digest"].as_str().unwrap(), unix_ms()]).unwrap_err();
    assert!(raw.to_string().contains("needs a valid, unexpired, unrevoked code_review grant"), "{raw}");

    let shown = f.cli_args(&["review", "show"]).0;
    assert_eq!(shown["acceptance"], json!({"active": true, "authority": "delegated_code_review.v1"}));
    let by_id = |id: &str| shown["opportunities"].as_array().unwrap().iter().find(|o| o["opportunity_id"] == id).unwrap().clone();
    assert_eq!((&by_id(&o1_id)["status"], &by_id(&o1_id)["findings_submitted"]), (&json!("completed"), &json!(0)));
    assert_eq!((&by_id(&o2_id)["status"], &by_id(&o2_id)["findings_submitted"]), (&json!("no_session"), &unavailable("no_session")));
    assert_eq!((&by_id(&o3_id)["status"], &by_id(&o3_id)["findings_submitted"]), (&json!("unassigned"), &unavailable("unassigned")));
    assert_eq!(by_id(&o1_id)["sessions"][0]["completion"]["acceptance"], json!(null), "undecided: still a proposal");

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
/// broad submission that triage splits into three claims. By hand: each
/// review's opening, assignment, session start and completion take one
/// history seq each, then its submission: submissions 1 (claim 1, seq 5), 2
/// (claim 2, seq 10), 3 (claim 3, seq 15; opened 11, assigned 12, the refused
/// receipt's session started at seq 13, its completion is seq 14). Claim 1
/// mints canonical finding `finding:canonical-16` (history seq 16); claim 2
/// is validated as the same finding (seq 17), so it is a duplicate: one
/// unique finding for two titles. Submission 3 splits (seq 18) into claims 4,
/// 5, 6: 4 and 5 mint findings at seq 19 and 20, 6 is rejected. Buckets: validated_only 1 (sub 1),
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
    assert_eq!((&state["head_seq"], &state["unique_findings"], &state["summary"]["pending"]), (&json!(15), &json!(0), &json!(3)));
    assert_eq!(state["submissions"][1]["title"], json!("Loader crashes on missing ~/app.toml"));
    assert_eq!((&state["submissions"][0]["trust"], &state["submissions"][0]["finding_ref"], &state["submissions"][0]["reporter_attempt_id"]),
        (&json!("proposal"), &json!("finding:null-deref"), &json!("rev-a1")));

    // Two titles, one defect.
    let minted = f.cli_args(&["review", "findings", "validate", "1", "--new", "--title", "Missing config crashes the loader", "--severity", "high", "--evidence", &evidence('e')]).0["event"].clone();
    assert_eq!((&minted["seq"], &minted["subject"]["finding_id"], &minted["authority"], &minted["principal"]),
        (&json!(16), &json!("finding:canonical-16"), &json!("operator_owner.v1"), &json!("operator:cli")));
    assert!(f.cli_fail(&["review", "findings", "validate", "2", "--new", "--severity", "high"]).contains("needs at least one evidence reference"));
    f.cli_args(&["review", "findings", "validate", "2", "--finding", "finding:canonical-16", "--severity", "high", "--evidence", &evidence('f')]);
    // The broad report becomes three claims beneath the same submission.
    let split = f.cli_args(&["review", "findings", "split", "3", "--claim", "Loader ignores XDG_CONFIG_HOME", "--claim", "Loader leaks a file handle", "--claim", "Prefer TOML over INI"]).0["event"].clone();
    assert_eq!((&split["seq"], &split["subject"]), (&json!(18), &json!({"submission_id": 3, "revision": 2, "claims": [4, 5, 6]})));
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
        (&json!("finding:canonical-16"), &json!("Missing config crashes the loader"), &json!("validated"), &json!(1), &json!([1]), &json!([2])));
    let claim2 = &state["submissions"][1]["claims"][0];
    assert_eq!((&claim2["decided"], &claim2["outcome"], &claim2["canonical_finding"]), (&json!("validated"), &json!("duplicate"), &json!("finding:canonical-16")));
    assert_eq!(state["findings"].as_array().unwrap().iter().map(|g| g["finding_id"].as_str().unwrap()).collect::<Vec<_>>(),
        ["finding:canonical-16", "finding:canonical-19", "finding:canonical-20"]);

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
/// 2 (rev-a2, claim 2), 3 (rev-a3, claim 3) at seq 5, 10, 15 (each review's
/// opening, assignment, session start and completion take the four seqs
/// before its submission). Seq 16 validates claim 1 as new finding P =
/// `finding:canonical-16`; seq 17 splits submission 2 into claims 4, 5; seq
/// 18 validates claim 4 as new N = `finding:canonical-18`; seq 19 marks claim
/// 5 a duplicate of P; seq 20 rejects claim 3. By hand: validated_only 1,
/// mixed 1, rejected_only 1: M22 = 2/3, M23 = 0/3. Seq 21 merges N into P:
/// submission 2 becomes duplicate_only: M22 = 1/3, M23 = 1/3, unique 1. Seq
/// 22 unmerges: 2/3 and 0/3 again, unique 2; as-of 21 still shows the merge.
/// Seq 23 reopens claim 3: pending 1, M22 = 2/2. Seq 24 restores submission
/// 2's unsplit revision 1 (claim 2, never decided): pending 2, M22 = 1/1. Seq
/// 25 restores revision 2: claims 4, 5 and their decisions return. As of seq
/// 17 only submission 1 is adjudicated (pending 2).
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
    assert_eq!(show(None)["head_seq"], json!(15));

    f.cli_args(&["review", "findings", "validate", "1", "--new", "--severity", "high", "--evidence", &evidence('e'), "--expect-seq", "15"]);
    assert!(f.cli_fail(&["review", "findings", "split", "2", "--claim", "a", "--claim", "b", "--expect-seq", "15"]).contains("finding history moved: head is 16, expected 15"));
    f.cli_args(&["review", "findings", "split", "2", "--claim", "Retry loop never ends", "--claim", "Loader crash, as reported before"]);
    let at17 = show(None);
    f.cli_args(&["review", "findings", "validate", "4", "--new", "--severity", "medium", "--evidence", &evidence('a')]);
    f.cli_args(&["review", "findings", "duplicate", "5", "--of", "finding:canonical-16"]);
    f.cli_args(&["review", "findings", "reject", "3", "--reason", "insufficient_evidence"]);
    let at20 = show(None);
    assert_eq!(submissions(&at20), [("validated_only".to_owned(), true, vec![1]), ("mixed".to_owned(), true, vec![4, 5]), ("rejected_only".to_owned(), false, vec![3])]);
    assert_eq!(rates(), (json!("2/3"), json!("0/3"), json!(0)));
    assert_eq!(at20["unique_findings"], json!(2));

    // Merge N into P: submission 2's validated claim becomes a duplicate of the earlier discovery.
    let merge = f.cli_args(&["review", "findings", "merge", "finding:canonical-18", "--into", "finding:canonical-16"]).0["event"].clone();
    assert_eq!(merge["seq"], json!(21));
    assert!(f.cli_fail(&["review", "findings", "merge", "finding:canonical-16", "--into", "finding:canonical-18"]).contains("already one group"));
    let at21 = show(None);
    assert_eq!(submissions(&at21)[1], ("duplicate_only".to_owned(), false, vec![4, 5]));
    assert_eq!((&at21["unique_findings"], &at21["findings"][1]["status"], &at21["findings"][1]["merged_into"]), (&json!(1), &json!("merged"), &json!("finding:canonical-16")));
    assert_eq!(rates(), (json!("1/3"), json!("1/3"), json!(0)));
    f.cli_args(&["review", "findings", "unmerge", "21"]);
    assert!(f.cli_fail(&["review", "findings", "unmerge", "21"]).contains("is not active"));
    assert_eq!(rates(), (json!("2/3"), json!("0/3"), json!(0)));
    assert_eq!(show(None)["unique_findings"], json!(2));

    // Reopen triage and reverse the split, then restore it.
    f.cli_args(&["review", "findings", "reset", "3", "--reason", "reopened"]);
    assert_eq!(rates(), (json!("2/2"), json!("0/2"), json!(1)));
    f.cli_args(&["review", "findings", "restore", "2", "--revision", "1"]);
    let at24 = show(None);
    assert_eq!(submissions(&at24)[1], ("pending".to_owned(), false, vec![2]));
    assert_eq!(rates(), (json!("1/1"), json!("0/1"), json!(2)));
    f.cli_args(&["review", "findings", "restore", "2", "--revision", "2"]);
    let head = show(None);
    assert_eq!((&head["head_seq"], &submissions(&head)[1]), (&json!(25), &("mixed".to_owned(), true, vec![4, 5])));

    // Replay: every earlier view is reproduced exactly at its sequence; the history keeps every row.
    for (seq, view) in [(17, &at17), (20, &at20), (21, &at21), (24, &at24)] {
        let replayed = show(Some(seq));
        for key in ["as_of_seq", "unique_findings", "summary", "submissions", "findings", "history"] { assert_eq!(replayed[key], view[key], "seq {seq} {key}"); }
        assert_eq!(replayed["head_seq"], json!(25));
    }
    assert_eq!(submissions(&at17)[1], ("pending".to_owned(), false, vec![4, 5]));
    assert_eq!((&at17["summary"]["adjudicated"], &at17["summary"]["pending"]), (&json!(1), &json!(2)));
    let kinds: Vec<String> = head["history"].as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap().to_owned()).collect();
    assert_eq!(kinds, ["submitted", "submitted", "submitted", "decided", "split", "decided", "decided", "decided", "merged", "unmerged", "decided", "restored", "restored"]);
    assert_eq!((&head["history"][9]["subject"]["reverses"], &head["history"][10]["subject"]["supersedes"]), (&json!(21), &json!(20)));
    assert!(head["history"].as_array().unwrap()[3..].iter().all(|e| e["principal"] == "operator:cli" && e["authority"] == "operator_owner.v1"));
    assert!(head["history"].as_array().unwrap()[..3].iter().all(|e| e["authority"] == "proposal"));
    assert!(f.cli_fail(&["review", "findings", "show", "--as-of", "26"]).contains("outside the finding history"));
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

/// Doc 10 §5 exact-candidate and reopen golden. History by hand: seq 1 and 2
/// open and assign rev-a1's review, 3 and 4 are its session start and
/// completion, 5 its submission; 6 validates it as F = `finding:canonical-6`;
/// 7 opens repair 7 assigned to `fast` (A); 8 binds fix-a1 (A, no result
/// yet). fix-a1 then submits SF1 (3…3, which run `a` verifies) and, after
/// changing its branch, SF2 (4…4). 9 proposes SF2: run `a` (SF1's commit) and
/// the rejected run `c` cannot verify it; 10 verifies it with run `d` on 4…4;
/// an integration of SF1's verified result is not its integration; 11 links
/// integration IG (merge 5…5); 12 closes repair 7 `fixed`. Now M25 = M26 =
/// 1/1 (finding and A's cohort), M27 has no observed integration (1 censored,
/// horizon 14 d), M29 = 5/6 (introduction unattributed). 13 attributes
/// introduction to the author of 1…1 by bisect (blame and the fixer are
/// refused); 14 reopens F after the fix was reverted: M26 = 0/1, M25 stays
/// 1/1, M27 = 1/1; the as-of-13 view still shows the resolution. 15 opens a
/// new unassigned repair, censored within its horizon.
#[test]
fn fix_credit_requires_exact_candidate_and_reopen_removes_current_credit() {
    let f = Fixture::new();
    let world = review_world(&f, &["rev-a1"]);
    let db_path = f.project.join(".state/state.db");
    let fast_id = agent_configuration(&fast_profile(&f)).id;
    let author_id = agent_configuration(&codex_profile(&f.config, "codex", "codex", Some(&f.home))).id;
    completed_review(&f, &world, "code", "rev-a1", json!(["finding:crash"]));
    f.cli_args(&["review", "findings", "validate", "1", "--new", "--severity", "high", "--evidence", &evidence('e')]);
    let finding = "finding:canonical-6";
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
    assert_eq!(fixes_show(&f, None)["head_seq"], json!(6));

    // Repair 7 is assigned to A before any repair runs.
    let opened = fixes(&["open", finding, "--assign", "fast"]);
    assert_eq!((&opened["seq"], &opened["subject"]["assignment"], &opened["subject"]["configuration_id"], &opened["subject"]["horizon_ms"]),
        (&json!(7), &json!("configuration"), &json!(fast_id), &json!(14 * 86_400_000_i64)));
    refused(&["open", finding, "--unassigned"], "already has open repair opportunity 7");
    refused(&["open", "finding:canonical-9", "--unassigned"], "no canonical finding");

    // Attempts bind before their outcome: a finished attempt or one with a result is refused.
    let factory = Factory::open(&f);
    factory.attempt("fix-a1", &fast_id);
    refused(&["bind", "7", "--attempt", "rev-a1"], "is already completed");
    refused(&["bind", "7", "--attempt", &f.attempt], "already has a result");
    assert_eq!(fixes(&["bind", "7", "--attempt", "fix-a1"])["subject"], json!({"repair_seq": 7, "attempt_id": "fix-a1", "ordinal": 1, "configuration_id": fast_id}));
    let (sf1, sf2) = (hex('3'), hex('4'));
    factory.submission(&sf1, "fix-a1", &oid('3'), 5_000);
    factory.run(&hex('a'), &sf1, "fix-a1", &oid('3'), Some(&hex('b')));
    factory.submission(&sf2, "fix-a1", &oid('4'), 6_000);
    factory.run(&hex('c'), &sf2, "fix-a1", &oid('4'), None);
    factory.run(&hex('d'), &sf2, "fix-a1", &oid('4'), Some(&hex('f')));
    let now = unix_ms();
    factory.integration(&hex('6'), &hex('b'), &oid('3'), &oid('8'), now - 2_000);
    factory.integration(&hex('7'), &hex('f'), &oid('4'), &oid('5'), now - 1_000);
    refused(&["propose", "7", "--submission", &world.0], "is not bound to repair opportunity 7");
    assert_eq!(fixes(&["propose", "7", "--submission", &sf2])["subject"]["candidate_oid"], json!(oid('4')));

    // Passing checks on one commit cannot verify another; a rejected run verifies nothing.
    refused(&["verify", "9", "--run", &hex('a'), "--assurance", "regression_reproduced", "--evidence", &evidence('a')], "passing checks on one commit cannot verify another");
    refused(&["verify", "9", "--run", &hex('c'), "--assurance", "regression_reproduced", "--evidence", &evidence('a')], "was rejected");
    refused(&["verify", "9", "--run", &hex('d'), "--assurance", "regression_reproduced"], "needs at least one evidence reference");
    {
        // Raw rows cannot forge it either: the trigger checks the exact candidate.
        let mut db = rusqlite::Connection::open(&db_path).unwrap();
        let tx = db.transaction().unwrap();
        tx.execute("INSERT INTO fix_log(seq,kind,principal,authority,recorded_unix_ms) VALUES(10,'verified','operator:cli','operator_owner.v1',1)", []).unwrap();
        let forged = tx.execute("INSERT INTO fix_verifications(seq,proposal_seq,run_id,result_id,commit_oid,assurance,evidence_refs) VALUES(10,9,?1,?2,?3,'regression_reproduced','[\"x\"]')",
            rusqlite::params![hex('a'), hex('b'), oid('4')]).unwrap_err();
        assert!(forged.to_string().contains("verified only by an accepted verification of its exact candidate"), "{forged}");
        let worker = tx.execute("INSERT INTO fix_log(seq,kind,principal,authority,recorded_unix_ms) VALUES(11,'credited','worker:fix-a1','operator_owner.v1',1)", []).unwrap_err();
        assert!(worker.to_string().contains("CHECK constraint failed"), "{worker}");
    }
    refused(&["integrate", "9", "--integrated", &hex('7')], "is not verified");
    assert_eq!(fixes(&["verify", "9", "--run", &hex('d'), "--assurance", "regression_reproduced", "--evidence", &evidence('a')])["subject"]["result_id"], json!(hex('f')));
    refused(&["close", "7", "--outcome", "no_fix"], "has a verified fix");
    // An integration of SF1's verified result is not an integration of this fix.
    refused(&["integrate", "9", "--integrated", &hex('6')], "did not integrate the fix's exact verified candidate");
    assert_eq!(fixes(&["integrate", "9", "--integrated", &hex('7')])["subject"]["commit_oid"], json!(oid('5')));
    fixes(&["close", "7", "--outcome", "fixed"]);

    let at12 = fixes_show(&f, None);
    let finding12 = &at12["findings"][0];
    assert_eq!((&finding12["remediation"], &finding12["verified"], &finding12["integrated"], &finding12["currently_resolved"]), (&json!("resolved"), &json!(true), &json!(true), &json!(true)));
    assert_eq!(finding12["resolutions"], json!([{"integration_seq": 11, "proposal_seq": 9, "repair_seq": 7, "integrated_id": hex('7'), "commit_oid": oid('5'),
        "integrated_unix_ms": now - 1_000, "ended_seq": null, "ended_by": null}]));
    // Separate roles: discovery, validation, implementation, verification, integration; introduction unattributed.
    assert_eq!((&finding12["discovery"]["policy"], credit(&finding12["discovery"])), (&json!("earliest_validated.v1"), vec![("attempt:rev-a1".into(), json!(null), "1".into())]));
    assert_eq!(credit(&finding12["validation"]), [("principal:operator:cli".to_owned(), json!(null), "1".to_owned())]);
    assert_eq!((&finding12["implementation"]["policy"], credit(&finding12["implementation"])), (&json!("sole_attempt.v1"), vec![("attempt:fix-a1".into(), json!(fast_id), "1".into())]));
    assert_eq!((credit(&finding12["verification"]), &finding12["verification"]["source_seq"]), (vec![("service:native_verifier".into(), json!(null), "1".into())], &json!(10)));
    assert_eq!(credit(&finding12["integration"]), [("service:integrator".to_owned(), json!(null), "1".to_owned())]);
    assert_eq!((&finding12["introduction"]["status"], &finding12["introduction"]["detection_oid"], &finding12["introduction"]["credit"]["unallocated"], &finding12["introduction"]["credit"]["unallocated_reason"]),
        (&json!("unattributed"), &json!(oid('1')), &json!("1"), &json!("unattributed")));
    assert_eq!((&at12["repairs"][0]["outcome"], &at12["repairs"][0]["closure"]), (&json!("currently_resolved"), &json!("fixed")));

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
    let at13 = fixes_show(&f, None);
    assert_eq!((&at13["findings"][0]["introduction"]["status"], credit(&at13["findings"][0]["introduction"]["credit"])),
        (&json!("attributed"), vec![(format!("attempt:{}", f.attempt), json!(author_id), "1".into())]));

    // A revert reopens F: history stays, current-resolution credit goes.
    refused(&["reopen", finding, "--reason", "reverted", "--observed", &oid('6')], "needs at least one evidence reference");
    assert_eq!(fixes(&["reopen", finding, "--reason", "reverted", "--observed", &oid('6'), "--evidence", &evidence('d')])["subject"]["integration_seq"], json!(11));
    refused(&["reopen", finding, "--reason", "regression", "--observed", &oid('6'), "--evidence", &evidence('d')], "is not currently resolved");
    let at14 = fixes_show(&f, None);
    let finding14 = &at14["findings"][0];
    assert_eq!((&finding14["remediation"], &finding14["verified"], &finding14["integrated"], &finding14["currently_resolved"]), (&json!("reopened"), &json!(true), &json!(true), &json!(false)));
    assert_eq!((&finding14["resolutions"][0]["ended_seq"], &finding14["resolutions"][0]["ended_by"], &finding14["reopenings"][0]["reason"]), (&json!(14), &json!("reopened"), &json!("reverted")));
    assert_eq!(at14["repairs"][0]["outcome"], json!("integrated"));
    let m = f.cli_args(&["review", "report"]).0["metrics"].clone();
    assert_eq!((&m["M25"]["value"], &m["M26"]["value"], &m["M26"]["by_assignment"][fast_id.as_str()]["value"]), (&json!("1/1"), &json!("0/1"), &json!("0/1")));
    assert_eq!((&m["M27"]["value"], &m["M27"]["censored"]), (&json!("1/1"), &json!(0)));

    // Replay: the as-of-13 view still has the resolution, byte for byte; one ordering with the triage history.
    let replayed = fixes_show(&f, Some(13));
    for key in ["findings", "repairs", "history", "as_of_seq"] { assert_eq!(replayed[key], at13[key], "{key}"); }
    assert_eq!((&replayed["head_seq"], &replayed["findings"][0]["currently_resolved"]), (&json!(14), &json!(true)));
    assert_eq!(fixes_show(&f, Some(12))["findings"][0]["introduction"]["status"], json!("unattributed"));
    assert_eq!(f.cli_args(&["review", "findings", "show", "--as-of", "14"]).0["findings"]["head_seq"], json!(14));

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

/// Doc 10 §5 assignment and mixed-credit golden. rev-a1 (A) reports P
/// (opening 1, assignment 2, session start 3, completion 4, submission seq
/// 5); rev-a2 (B) reports Q and P again (opening 6, assignment 7, start 8,
/// completion 9, submissions seq 10, 11). Seq 12 validates P =
/// `finding:canonical-12`, 13 validates Q = `finding:canonical-13`, 14 links
/// rev-a2's P report (a duplicate discovery). Both repairs are assigned to A:
/// 15 for P, 16 for Q. 17 binds a1 (A) to repair 15, which fails; 18 closes
/// it `no_fix`. 19 binds a2 (A) to repair 16, which fails; 20 reassigns it to
/// b1 (B), whose candidate is proposed (21), verified (22), integrated (23);
/// 24 closes it `fixed`. By hand: A's M25 = M26 = 1/2 (the failed repair
/// stays), B has no assigned opportunity (null, not 1/1); findings 1/2.
/// Implementation credit of Q's fix is mixed and unallocated until 25 splits
/// it b1 2/3, a2 1/3. M21 = 2 (A 1, B 1); 26 shares P's discovery 1/2 + 1/2
/// (A 1/2, B 3/2, still 2 in total); 27 retracts that. M29: 6/9 = 2/3 before
/// 25, 7/9 after. 28 records P's introduction as unattributable.
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
    f.cli_args(&["review", "findings", "validate", "3", "--finding", "finding:canonical-12", "--severity", "high", "--evidence", &evidence('f')]);
    let (p, q) = ("finding:canonical-12", "finding:canonical-13");
    let fixes = |args: &[&str]| { let mut all = vec!["review", "fixes"]; all.extend(args); f.cli_args(&all).0["event"].clone() };
    let refused = |args: &[&str], text: &str| { let mut all = vec!["review", "fixes"]; all.extend(args); let e = f.cli_fail(&all); assert!(e.contains(text), "{e}"); };
    let metrics = || f.cli_args(&["review", "report"]).0["metrics"].clone();

    assert_eq!(fixes(&["open", p, "--assign", "fast"])["seq"], json!(15));
    assert_eq!(fixes(&["open", q, "--assign", "fast"])["seq"], json!(16));
    for (attempt, configuration) in [("a1", &a), ("a2", &a), ("b1", &b)] { factory.attempt(attempt, configuration); }
    fixes(&["bind", "15", "--attempt", "a1"]);
    refused(&["close", "15", "--outcome", "fixed"], "has no verified fix");
    fixes(&["close", "15", "--outcome", "no_fix"]);
    refused(&["bind", "15", "--attempt", "a2"], "repair opportunity 15 is closed");
    fixes(&["bind", "16", "--attempt", "a2"]);
    assert_eq!(fixes(&["bind", "16", "--attempt", "b1"])["subject"], json!({"repair_seq": 16, "attempt_id": "b1", "ordinal": 2, "configuration_id": b}));
    factory.submission(&hex('5'), "b1", &oid('7'), 5_000);
    factory.run(&hex('a'), &hex('5'), "b1", &oid('7'), Some(&hex('b')));
    factory.integration(&hex('6'), &hex('b'), &oid('7'), &oid('9'), unix_ms() - 1_000);
    fixes(&["propose", "16", "--submission", &hex('5')]);
    fixes(&["verify", "21", "--run", &hex('a'), "--assurance", "approved_alternative", "--evidence", &evidence('a')]);
    fixes(&["integrate", "21", "--integrated", &hex('6')]);
    assert_eq!(fixes(&["close", "16", "--outcome", "fixed"])["seq"], json!(24));

    let at24 = fixes_show(&f, None);
    let attempts: Vec<_> = at24["repairs"][1]["attempts"].as_array().unwrap().iter()
        .map(|t| (t["attempt_id"].clone(), t["ordinal"].clone(), t["configuration_id"].clone(), t["reassignment"].clone())).collect();
    assert_eq!(attempts, [(json!("a2"), json!(1), json!(a), json!(false)), (json!("b1"), json!(2), json!(b), json!(true))]);
    assert_eq!((&at24["repairs"][0]["outcome"], &at24["repairs"][0]["closure"], &at24["repairs"][1]["outcome"]), (&json!("no_candidate"), &json!("no_fix"), &json!("currently_resolved")));
    let q24 = &at24["findings"][1];
    assert_eq!((&q24["implementation"]["shares"], &q24["implementation"]["allocated"], &q24["implementation"]["unallocated"], &q24["implementation"]["unallocated_reason"]),
        (&json!([]), &json!("0"), &json!("1"), &json!("mixed_contribution_unallocated")));
    assert_eq!((&at24["findings"][0]["remediation"], &at24["findings"][0]["implementation"]), (&json!("unrepaired"), &json!(null)));

    let m = metrics();
    assert_eq!((&m["M25"]["value"], &m["M25"]["findings"]), (&json!("1/2"), &json!({"label": "finding_outcomes", "numerator": 1, "denominator": 2})));
    let by = json!({a.as_str(): {"numerator": 1, "denominator": 2, "value": "1/2", "not_achieved": 1, "reassigned": 1, "censored": 0},
        b.as_str(): {"numerator": 0, "denominator": 0, "value": null, "not_achieved": 0, "reassigned": 0, "censored": 0, "reason": "no_assigned_opportunities"}});
    assert_eq!((&m["M25"]["by_assignment"], &m["M26"]["by_assignment"], &m["M26"]["value"]), (&by, &by, &json!("1/2")));
    assert_eq!((&m["M21"]["value"], &m["M21"]["by_configuration"], &m["M21"]["participation"]), (&json!("2"), &json!({a.as_str(): "1", b.as_str(): "1"}), &json!(2)));
    assert_eq!((&m["M29"]["value"], &m["M29"]["by_role"]["implementation"]), (&json!("2/3"), &json!({"allocated": "0", "eligible": 1, "value": "0/1"})));

    // Mixed contributions are split, never full credit each.
    refused(&["credit", q, "--role", "implementation", "--proposal", "21", "--share", "b1=2/3", "--share", "a2=2/3", "--evidence", &evidence('c')], "sum to more than 1");
    refused(&["credit", q, "--role", "implementation", "--proposal", "21", "--share", "a1=1/3", "--evidence", &evidence('c')], "did not contribute");
    refused(&["credit", q, "--role", "implementation", "--proposal", "21", "--share", "b1=2/3", "--share", "a2=1/3"], "needs at least one evidence reference");
    assert_eq!(fixes(&["credit", q, "--role", "implementation", "--proposal", "21", "--share", "b1=2/3", "--share", "a2=1/3", "--evidence", &evidence('c')])["seq"], json!(25));
    let q25 = fixes_show(&f, None)["findings"][1]["implementation"].clone();
    assert_eq!((&q25["policy"], credit(&q25), &q25["allocated"], &q25["unallocated"]),
        (&json!("owner_allocation.v1"), vec![("attempt:a2".into(), json!(a), "1/3".into()), ("attempt:b1".into(), json!(b), "2/3".into())], &json!("1"), &json!("0")));
    assert_eq!(metrics()["M29"]["value"], json!("7/9"));

    // Shared discovery: two reporters of P, half each; the total stays one finding each.
    refused(&["credit", p, "--role", "discovery", "--share", "rev-a1=1", "--share", "rev-a2=1/2", "--evidence", &evidence('c')], "sum to more than 1");
    refused(&["credit", q, "--role", "discovery", "--share", "rev-a1=1", "--evidence", &evidence('c')], "did not contribute");
    fixes(&["credit", p, "--role", "discovery", "--share", "rev-a1=1/2", "--share", "rev-a2=1/2", "--evidence", &evidence('c')]);
    let m21 = metrics()["M21"].clone();
    assert_eq!((&m21["value"], &m21["unallocated"], &m21["by_configuration"], &m21["participation"]), (&json!("2"), &json!("0"), &json!({a.as_str(): "1/2", b.as_str(): "3/2"}), &json!(3)));
    // Retraction restores the policy's credit; the allocation stays in the as-of view.
    refused(&["retract", "24"], "only a credit allocation, reopening or introduction");
    fixes(&["retract", "26"]);
    refused(&["retract", "26"], "already retracted");
    assert_eq!(metrics()["M21"]["by_configuration"], json!({a.as_str(): "1", b.as_str(): "1"}));
    assert_eq!((&fixes_show(&f, Some(26))["findings"][0]["discovery"]["policy"], &fixes_show(&f, None)["findings"][0]["discovery"]["policy"]),
        (&json!("owner_allocation.v1"), &json!("earliest_validated.v1")));

    // Unattributable introduction is explicit, not an exoneration and not a charge.
    refused(&["introduce", p, "--unattributable"], "needs at least one evidence reference");
    fixes(&["introduce", p, "--unattributable", "--evidence", &evidence('d')]);
    let intro = fixes_show(&f, None)["findings"][0]["introduction"].clone();
    assert_eq!((&intro["status"], &intro["credit"]["shares"], &intro["credit"]["unallocated"], &intro["credit"]["unallocated_reason"]),
        (&json!("unattributable"), &json!([]), &json!("1"), &json!("unattributable")));
    assert_eq!(fixes_show(&f, None)["head_seq"], json!(28));
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
/// `skeptical-challenge.v1`; 2–5 are ordinary review O1's (code, S1) opening,
/// assignment, session start and completion, 6 its submission `null-deref`; 7
/// validates it as K = `finding:canonical-7`. 8 opens a pass without a budget
/// (its binding is refused). 9–11 open, assign and start a review that began
/// before any binding (so it cannot be bound). 12 opens P1 and 13 binds the
/// skeptical pass P1 (S1, prior O1): same artifact, prior coverage complete,
/// cutoff 12, so K is known. P1 is assigned (14), its session starts (15) and
/// completes (16) reporting `alias`, `leak`, `reworded` and `style`
/// (submissions 2–5 at seq 17–20, claims 2–5). 21 marks `reworded` a
/// duplicate of K; 22 validates `alias` as new A = `finding:canonical-22`; 23
/// validates `leak` as L = `finding:canonical-23`; 24 rejects `style`. Then
/// P1 yields A and L: M28 = 2/1 (one rediscovery). 25 merges A into K (same
/// root cause, reworded): only L is new, M28 = 1/1, 2 rediscoveries; as of 24
/// it is still 2. 26 opens P2 on S2 (a later candidate), 27 binds it after
/// O1: `changed_artifact`, its own opportunity; it is assigned (28), its
/// session (29, 30) reports one finding (31, validated at 32 as
/// `finding:canonical-32`), never incremental. 33 opens O2, which never runs;
/// 34 opens P3 on S1 and 35 binds it after O2: `incomplete_prior_coverage`.
/// Final M28 = 1/1, excluded 1 + 1.
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
        (&json!(13), &json!(12), &json!("same_artifact"), &json!("complete")));
    assert!(f.cli_fail(&["review", "protocols", "bind", &p1, "--prior", &o1]).contains("already a bound pass"));

    // The skeptical session reports four findings; two are rewordings of K.
    let done = run_review(&f, &p1, '1', "rev-k1", json!([{"ref": "finding:reworded", "title": "Loader dereferences null when the config file is absent"},
        {"ref": "finding:alias", "title": "Missing config file crashes startup"}, "finding:leak", "finding:style"]), json!([evidence('a')]));
    assert_eq!(done["finding_submissions"], json!([2, 3, 4, 5]));
    f.cli_args(&["review", "findings", "duplicate", "4", "--of", "finding:canonical-7"]);
    f.cli_args(&["review", "findings", "validate", "2", "--new", "--severity", "high", "--evidence", &evidence('b')]);
    f.cli_args(&["review", "findings", "validate", "3", "--new", "--severity", "medium", "--evidence", &evidence('c')]);
    f.cli_args(&["review", "findings", "reject", "5", "--reason", "out_of_scope"]);
    let pass = protocols_show(&f, None)["passes"][0].clone();
    assert_eq!((&pass["opportunity_id"], &pass["known_findings"], &pass["new_unique_findings"], &pass["rediscovered"], &pass["eligible"]),
        (&json!(p1), &json!(["finding:canonical-7"]), &json!(["finding:canonical-22", "finding:canonical-23"]), &json!(1), &json!(true)));
    assert_eq!(pass["claims"].as_array().unwrap().iter().map(|c| (c["claim_id"].as_i64().unwrap(), c["incremental"].as_str().unwrap())).collect::<Vec<_>>(),
        [(2, "new"), (3, "new"), (4, "rediscovered"), (5, "rejected")]);
    let m28 = f.cli_args(&["review", "report"]).0["metrics"]["M28"].clone();
    assert_eq!((&m28["numerator"], &m28["denominator"], &m28["value"], &m28["rediscovered"]), (&json!(2), &json!(1), &json!("2/1"), &json!(1)));

    // The owner merges the reworded A into K: A was never a new discovery.
    assert_eq!(f.cli_args(&["review", "findings", "merge", "finding:canonical-22", "--into", "finding:canonical-7"]).0["event"]["seq"], json!(25));
    let pass = protocols_show(&f, None)["passes"][0].clone();
    assert_eq!((&pass["new_unique_findings"], &pass["rediscovered"]), (&json!(["finding:canonical-23"]), &json!(2)));
    assert_eq!(protocols_show(&f, Some(24))["passes"][0]["new_unique_findings"], json!(["finding:canonical-22", "finding:canonical-23"]));

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
    assert_eq!((&bound["seq"], &bound["subject"]["prior_coverage"], &bound["subject"]["priors"][0]["status"]), (&json!(35), &json!("incomplete"), &json!("unassigned")));

    let shown = protocols_show(&f, None);
    let by = |id: &str| shown["passes"].as_array().unwrap().iter().find(|p| p["opportunity_id"] == id).unwrap().clone();
    let changed = by(&p2);
    assert_eq!((&changed["candidate_oid"], &changed["priors"][0]["artifact"], &changed["new_unique_findings"], &changed["eligible"], &changed["exclusion"]),
        (&json!(oid('2')), &json!("changed_artifact"), &json!(["finding:canonical-32"]), &json!(false), &json!("changed_artifact")));
    assert_eq!(by(&p3)["exclusion"], json!("incomplete_prior_coverage"));
    assert_eq!(shown["history"].as_array().unwrap().iter().map(|e| (e["seq"].as_i64().unwrap(), e["kind"].as_str().unwrap())).collect::<Vec<_>>(),
        [(1, "protocol_registered"), (13, "pass_bound"), (27, "pass_bound"), (35, "pass_bound")]);

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
/// of S1 already completed (opening 3, assignment 4, session start 5,
/// completion 6), so it cannot be assigned; 7 opens an ineligible security
/// review. 8–11 open the base reviews B1–B4 (S1–S4) and 12–15 assign them;
/// the seed gives, by `sha256("review_experiment.v1:" + seed + ":" +
/// submission)` mod 2: standard, skeptical, standard, skeptical. 16 opens a
/// review that is refused as a second unit. Each base review's assignment,
/// session start and completion precede its submissions: B1 17, 18, 19, `b1`
/// (20); B2 21, 22, 23, `b2` (24); B3 25, 26, 27, none; B4 28, 29, 30, `b4`
/// (31). 32 and 33 open passes P2 and P4, 34 and 35 bind them (after B2 and
/// B4); P2 (36–38) reports `k2-new` (39) and `k2-reworded` (40), P4 (41–43)
/// `k4-reworded` (44). 45–48 validate b1, b2, b4 and k2-new as new
/// (`finding:canonical-45` to `-48`); 49 and 50 mark the rewordings
/// duplicates of b2 and b4. Outcomes: S1 1, S2 2 (b2 + k2-new), S3 0, S4 1
/// (the reworded report adds nothing). Means 1/2 and 3/2: difference 3/2 −
/// 1/2 = 1. As of 44 only S3 is analyzable. The descriptive M28 over P2 and
/// P4 is 1/2. 51 excludes S3: standard has 1 analyzable unit, so the
/// difference is unavailable. 52 opens and 53 binds a pass after B1
/// (standard): crossover, still analyzed as assigned.
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
        tx.execute("INSERT INTO protocol_log(seq,kind,principal,authority,expected_seq,recorded_unix_ms) VALUES(7,'unit_assigned','operator:cli','operator_owner.v1',NULL,1)", []).unwrap();
        let raw = tx.execute("INSERT INTO experiment_units(seq,experiment_seq,opportunity_id,submission_id,arm,block) VALUES(7,2,?1,?2,'standard',NULL)", [&x, &hex('1')]).unwrap_err();
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
    assert_eq!(arms, [(12, "standard".into()), (13, "skeptical".into()), (14, "standard".into()), (15, "skeptical".into())]);
    let again = open_review(&f, '1', "code", "review-protocol.v1", None);
    assert!(f.cli_fail(&["review", "experiments", "assign", "skeptical-vs-standard.v1", &again]).contains("is already a unit"));

    // Base reviews, then the treatment arm's passes.
    run_review(&f, &base[0], '1', "rev-b1", json!(["finding:b1"]), json!([]));
    run_review(&f, &base[1], '2', "rev-b2", json!(["finding:b2"]), json!([]));
    run_review(&f, &base[2], '3', "rev-b3", json!([]), json!([]));
    run_review(&f, &base[3], '4', "rev-b4", json!(["finding:b4"]), json!([]));
    let p2 = open_review(&f, '2', "skeptical", SKEPTICAL, Some(BUDGET));
    let p4 = open_review(&f, '4', "skeptical", SKEPTICAL, Some(BUDGET));
    assert_eq!(f.cli_args(&["review", "protocols", "bind", &p2, "--prior", &base[1]]).0["event"]["seq"], json!(34));
    assert_eq!(f.cli_args(&["review", "protocols", "bind", &p4, "--prior", &base[3]]).0["event"]["seq"], json!(35));
    assert_eq!(run_review(&f, &p2, '2', "rev-k2", json!(["finding:k2-new", "finding:k2-reworded"]), json!([evidence('a')]))["finding_submissions"], json!([4, 5]));
    assert_eq!(run_review(&f, &p4, '4', "rev-k4", json!(["finding:k4-reworded"]), json!([evidence('a')]))["finding_submissions"], json!([6]));
    for claim in ["1", "2", "3", "4"] { f.cli_args(&["review", "findings", "validate", claim, "--new", "--severity", "medium", "--evidence", &evidence('e')]); }
    f.cli_args(&["review", "findings", "duplicate", "5", "--of", "finding:canonical-46"]);
    f.cli_args(&["review", "findings", "duplicate", "6", "--of", "finding:canonical-47"]);

    let e = experiments_show(&f, None);
    assert_eq!(e["units"].as_array().unwrap().iter().map(|u| (u["arm"].as_str().unwrap(), u["status"].as_str().unwrap(), u["outcome"].as_i64().unwrap(), u["treatment_received"].clone()))
        .collect::<Vec<_>>(), [("standard", "analyzable", 1, json!(null)), ("skeptical", "analyzable", 2, json!(true)), ("standard", "analyzable", 0, json!(null)), ("skeptical", "analyzable", 1, json!(true))]);
    assert_eq!(e["units"][1]["new_unique_findings"], json!(["finding:canonical-46", "finding:canonical-48"]));
    let est = &e["estimate"];
    assert_eq!((&est["estimate"], &est["analysis"], &est["reference_arm"], &est["uncertainty"]),
        (&json!("randomized"), &json!("intention_to_treat"), &json!("standard"), &unavailable("interval_not_computed")));
    assert_eq!((&est["arms"]["standard"]["mean"], &est["arms"]["skeptical"]["mean"], &est["arms"]["skeptical"]["outcome_total"]), (&json!("1/2"), &json!("3/2"), &json!(3)));
    assert_eq!(est["differences"], json!({"skeptical": {"value": "1", "analyzable": [2, 2]}}));
    // Replay: before any triage only S3 (no findings) was settled.
    let early = experiments_show(&f, Some(44));
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
    assert_eq!(f.cli_args(&["review", "experiments", "exclude", "skeptical-vs-standard.v1", &base[2], "--reason", "operator_error"]).0["event"]["seq"], json!(51));
    let e = experiments_show(&f, None);
    assert_eq!((&e["units"][2]["status"], &e["units"][2]["arm"], &e["units"][2]["exclusion"]), (&json!("excluded"), &json!("standard"), &json!({"seq": 51, "reason": "operator_error", "retracted_seq": null})));
    assert_eq!((&e["estimate"]["arms"]["standard"]["excluded"], &e["estimate"]["differences"]["skeptical"]),
        (&json!({"operator_error": 1}), &json!({"value": unavailable("insufficient_data"), "analyzable": [1, 2]})));

    // Crossover: a pass after a standard unit is recorded and the unit stays in its assigned arm.
    let p1 = open_review(&f, '1', "skeptical", SKEPTICAL, Some(BUDGET));
    assert_eq!(f.cli_args(&["review", "protocols", "bind", &p1, "--prior", &base[0]]).0["event"]["seq"], json!(53));
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
        let digest = self.contract(task);
        let attempt = format!("{task}-attempt");
        self.attempt(task, &attempt, None);
        self.result(task, &attempt, &digest, candidate)
    }
    /// Task `task` with its signed verify-then-integrate contract (revision 1);
    /// returns the contract digest.
    fn contract(&self, task: &str) -> String {
        use herdr_projects::{authority::CONTRACT_SIGNATURE_NAMESPACE, domain::TaskId, runtime};
        let head = runtime::add_task(&self.project, TaskId::new(task).unwrap(), "work".into(), runtime::snapshot(&self.project).unwrap().head).unwrap();
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
        installed["digest"].as_str().unwrap().to_owned()
    }
    /// A running attempt of `task`, as a launch would write it. With a
    /// `(group, arm, configuration)`, its reservation chose that arm's
    /// configuration and bound it to the arm (candidate_groups::bind).
    fn attempt(&self, task: &str, attempt: &str, arm: Option<(&str, u32, &str)>) {
        let db = self.db();
        db.execute("INSERT INTO attempts(id,task_id,revision,state,snapshot,reservation,termination_observed) VALUES(?1,?2,1,'running',NULL,?1,0)", [attempt, task]).unwrap();
        if let Some((group, arm, configuration)) = arm {
            db.execute("INSERT INTO dispatch_decisions(attempt_id,task_id,task_revision,contract_revision,chosen_configuration_id,eligible,chooser_kind,chooser_principal,reason_codes,decided_unix_ms)
                VALUES(?1,?2,1,1,?3,'[\"x\"]','operator','operator:cli','[\"x\"]',1)", rusqlite::params![attempt, task, configuration]).unwrap();
            db.execute("INSERT INTO candidate_arm_attempts(group_id,arm,attempt_id,bound_unix_ms,source) VALUES(?1,?2,?3,1,'admit_prepared')", rusqlite::params![group, arm, attempt]).unwrap();
        }
    }
    /// Submit `attempt`'s result at `candidate` through `result submit`; returns the submission id.
    fn result(&self, task: &str, attempt: &str, contract_digest: &str, candidate: &str) -> String {
        let repository = self.repo.canonicalize().unwrap().display().to_string();
        let objects: Vec<serde_json::Value> = self.git(&["rev-list", "--objects", "--all"]).lines()
            .map(|line| { let oid = line.split_whitespace().next().unwrap(); json!({"oid": oid, "relative_path": format!("{}/{}", &oid[..2], &oid[2..])}) }).collect();
        let key = if attempt == format!("{task}-attempt") { format!("{task}-key") } else { format!("{attempt}-key") };
        // A later submission of the same attempt needs its own idempotency key.
        let key = if self.db().query_row("SELECT EXISTS(SELECT 1 FROM result_submissions WHERE attempt_id=?1)", [attempt], |r| r.get(0)).unwrap() { format!("{key}-{}", &candidate[..12]) } else { key };
        let result = self.home.path().join(format!("{key}-result.json"));
        fs::write(&result, json!({"idempotency_key": key, "task_id": task, "contract_revision": 1, "contract_digest": contract_digest,
            "attempt_id": attempt, "repository": repository, "base_oid": self.base, "candidate_oid": candidate, "object_format": "sha256",
            "artifact_manifest": [{"path": "src/lib.rs", "oid": candidate}], "claimed_checks": ["all checks passed"], "objects": objects}).to_string()).unwrap();
        self.ok(&["result", "demo", "submit", "--input-file", result.to_str().unwrap()])["submission_id"].as_str().unwrap().to_owned()
    }
    /// Make `attempt` its task's active attempt with a started worker, as a
    /// launch would record it (attempt inputs and `runtime.launch_started`).
    fn started(&self, task: &str, attempt: &str) {
        let db = self.db();
        db.execute("UPDATE tasks SET active_attempt=?2 WHERE id=?1", [task, attempt]).unwrap();
        let operation = format!("op-{attempt}");
        db.execute("INSERT INTO operations(id,task_id,kind,target,payload_version,payload,payload_hash,expected_revision,due_unix_ms,idempotency_key) VALUES(?1,?2,'runtime.launch','binding',1,'{}',?3,1,0,?1)",
            rusqlite::params![operation, task, format!("{:x}", Sha256::digest(b"{}"))]).unwrap();
        let inputs = r#"{"inputs":{"version":2,"effective_profile":{}}}"#;
        db.execute("INSERT INTO attempt_inputs(attempt_id,operation_id,payload,payload_hash) VALUES(?1,?2,?3,?4)", rusqlite::params![attempt, operation, inputs, format!("{:x}", Sha256::digest(inputs.as_bytes()))]).unwrap();
        db.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('runtime.launch_started',?1,1,1,'{}')", [&operation]).unwrap();
    }
    /// `task demo complete <task>` at the task's current revision.
    fn complete(&self, task: &str) -> std::process::Output {
        let revision: i64 = self.db().query_row("SELECT revision FROM tasks WHERE id=?1", [task], |r| r.get(0)).unwrap();
        self.hp(&["task", "demo", "complete", task, "--expected-revision", &revision.to_string()])
    }
    /// The submissions named by `task`'s completion requests.
    fn completion_requests(&self, task: &str) -> Vec<String> {
        self.db().prepare("SELECT json_extract(e.payload,'$.submission') FROM events e JOIN attempts a ON a.id=e.entity WHERE e.kind='attempt.completion_requested' AND a.task_id=?1 ORDER BY e.sequence").unwrap()
            .query_map([task], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap()
    }
    /// Retained native profiles `codex` and `fast` (distinct configurations,
    /// as `profile prepare` would retain them); returns their configuration ids.
    fn arm_profiles(&self) -> (String, String) {
        let config = herdr_projects::migration::config_reference(&self.home.path().join(".config/herdr-projects/config.toml")).unwrap();
        let db_path = self.project.join(".state/state.db");
        let codex = codex_profile(&config, "codex", "codex", Some(&self.home.path().join("codex-home")));
        let mut fast = codex_profile(&config, "codex", "fast", Some(&self.home.path().join("fast-home")));
        fast.arguments_digest = "1".repeat(64);
        let ids = (agent_configuration(&codex).id, agent_configuration(&fast).id);
        plant_profile(&db_path, codex);
        plant_profile(&db_path, fast);
        ids
    }
    /// Queue `consumer` behind `predecessor`'s `verified_result` through `task queue`.
    fn queue_dependent(&self, consumer: &str, predecessor: &str) {
        use herdr_projects::{domain::TaskId, runtime};
        let head = runtime::add_task(&self.project, TaskId::new(consumer).unwrap(), format!("consumer {consumer}"), runtime::snapshot(&self.project).unwrap().head).unwrap();
        let request = self.home.path().join(format!("{consumer}-queue.json"));
        fs::write(&request, json!({"priority": 0, "dependencies": [{"predecessor": predecessor, "requirement": "verified_result"}]}).to_string()).unwrap();
        self.ok(&["task", "demo", "queue", consumer, "--input-file", request.to_str().unwrap(), "--expected-revision", "1", "--expected-head", &head.to_string()]);
    }
    /// `consumer`'s dependency blockers in `scheduler inspect`. Factory
    /// admission stays off (the default), so an edge whose evidence counts
    /// reports only `admission_disabled:<edge>`.
    fn blockers(&self, consumer: &str) -> Vec<String> {
        let report = self.ok(&["scheduler", "demo", "inspect"]);
        let entry = report["entries"].as_array().unwrap().iter().find(|e| e["task"] == consumer).unwrap().clone();
        entry["blockers"].as_array().unwrap().iter().map(|b| b.as_str().unwrap().to_owned())
            .filter(|b| ["verified_dependency_evidence_unavailable:", "predecessor_failed:", "admission_disabled:"].iter().any(|p| b.starts_with(p))).collect()
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

/// Integration hold for candidate groups (contracts-quality.md §3, owner
/// decision 1) over the real integration path. Group G on task `g` seals arms
/// 1 `codex` and 2 `fast`; attempts g-a1 and g-a2 bind to them and both
/// candidates pass verification. Group N on task `n` (same arms) has one
/// verified arm and is closed with no selection. With automatic integration
/// on, the 3 verified submissions enter the pending projection; the
/// producer's turn enqueues 0 jobs and drops all 3 (no selection names any of
/// them). The operator's `result integrate` of g-a1 is refused by
/// `begin_integration` before any write. The rule then selects arm 1 (first
/// accepted in launch order); that transaction queues g-a1's submission (the
/// projection is exactly it) and the next turn enqueues 1 job, for it. The
/// losing arm 2 and N's arm stay refused; the winner integrates.
#[test]
fn unselected_arm_does_not_integrate_until_selection_and_winner_is_queued() {
    let lab = IntegrationLab::new();
    let (codex, fast) = lab.arm_profiles();
    let clean = seed_set()["clean"].as_str().unwrap().to_owned();
    let group = |task: &str| lab.telemetry(&["quality", "groups", "create", task, "--arm", "codex", "--arm", "fast"])["group"]["group_id"].as_str().unwrap().to_owned();
    let g_digest = lab.contract("g");
    let g = group("g");
    lab.attempt("g", "g-a1", Some((&g, 1, &codex)));
    lab.attempt("g", "g-a2", Some((&g, 2, &fast)));
    let g1_oid = lab.candidate("g-arm-1", &format!("{clean}// arm 1\n"));
    let s1 = lab.result("g", "g-a1", &g_digest, &g1_oid);
    let s2 = lab.result("g", "g-a2", &g_digest, &lab.candidate("g-arm-2", &format!("{clean}// arm 2\n")));
    let n_digest = lab.contract("n");
    let n = group("n");
    lab.attempt("n", "n-a1", Some((&n, 1, &codex)));
    let s3 = lab.result("n", "n-a1", &n_digest, &lab.candidate("n-arm-1", &format!("{clean}// n arm 1\n")));
    let (r1, r2, r3) = (lab.verify(&s1, "verify-1"), lab.verify(&s2, "verify-2"), lab.verify(&s3, "verify-3"));
    assert_eq!(lab.telemetry(&["quality", "groups", "select", &n, "--none", "--reason", "none_acceptable"])["selection"]["outcome"], json!("no_selection"));

    lab.git(&["branch", "integration", &lab.base]);
    lab.ok(&["result", "demo", "configure-integration", "--repository", lab.repo.to_str().unwrap(), "--reference", "refs/heads/integration"]);
    let head = herdr_projects::runtime::snapshot(&lab.project).unwrap().head.to_string();
    assert_eq!(lab.ok(&["result", "demo", "auto", "--integrate", "on", "--expected-head", &head])["integrate"], json!(true));
    let sorted = |mut v: Vec<String>| { v.sort(); v };
    let pending = || sorted(lab.db().prepare("SELECT submission_id FROM pending_integration_work").unwrap().query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap());
    let jobs = || lab.db().prepare("SELECT json_extract(payload,'$.submission_id') FROM operations WHERE kind='integration.run'").unwrap()
        .query_map([], |r| r.get(0)).unwrap().collect::<Result<Vec<String>, _>>().unwrap();
    assert_eq!(pending(), sorted(vec![s1.clone(), s2.clone(), s3.clone()]), "every verified arm enters the pending projection");
    let turn = herdr_projects::store::service_project_integration_jobs(&lab.project).unwrap();
    assert_eq!((turn.enqueued, turn.pending), (0, false));
    assert_eq!((jobs(), pending()), (vec![], vec![]), "no arm is eligible before its group's selection names it");

    // The operator path reaches begin_integration, which refuses before any write.
    let target = lab.git(&["rev-parse", "refs/heads/integration"]);
    let count = |sql: &str| lab.db().query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
    let integrate = |result: &str, key: &str| {
        let work = lab.home.path().join(key);
        (lab.hp(&["result", "demo", "integrate", result, "--repository", lab.repo.to_str().unwrap(), "--idempotency-key", key, "--work-dir", work.to_str().unwrap()]), work)
    };
    let refused = |result: &str, key: &str| {
        let (out, work) = integrate(result, key);
        assert!(!out.status.success());
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("a candidate-group arm integrates only as its group's selection"), "{stderr}");
        assert!(!work.exists());
    };
    refused(&r1, "integrate-1-early");
    assert_eq!(lab.git(&["rev-parse", "refs/heads/integration"]), target);
    assert_eq!((count("SELECT count(*) FROM integration_operations"), count("SELECT count(*) FROM operations WHERE kind='integration.lease'")), (0, 0));
    let show = |group: &str| lab.telemetry(&["quality", "groups", "show"])["groups"].as_array().unwrap().iter().find(|x| x["group_id"] == group).unwrap().clone();
    let hold = json!({"enforced": true, "integrated_without_selection": []});
    assert_eq!((&show(&g)["status"], &show(&g)["integration_hold"]), (&json!("open"), &hold));

    // Selection lifts the winner's hold and queues it in the same transaction.
    let selection = lab.telemetry(&["quality", "groups", "select", &g, "--rule"])["selection"].clone();
    assert_eq!((&selection["arm"], &selection["submission_id"], &selection["reason"]), (&json!(1), &json!(s1), &json!("first_passing_verification")));
    assert_eq!(pending(), vec![s1.clone()]);
    let turn = herdr_projects::store::service_project_integration_jobs(&lab.project).unwrap();
    assert_eq!((turn.enqueued, turn.pending), (1, false));
    assert_eq!((jobs(), pending()), (vec![s1.clone()], vec![]));
    assert_eq!(herdr_projects::store::service_project_integration_jobs(&lab.project).unwrap().enqueued, 0, "the loser is never re-added");

    // Losers never integrate: G's arm 2, and N's only arm (closed with no selection).
    refused(&r2, "integrate-2");
    refused(&r3, "integrate-3");
    assert_eq!((count("SELECT count(*) FROM integration_operations"), lab.git(&["rev-parse", "refs/heads/integration"])), (0, target.clone()));
    // The winner passes begin_integration and lands on the target.
    let (out, _) = integrate(&r1, "integrate-1");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let integrated: Vec<String> = lab.db().prepare("SELECT o.verified_result_id FROM integrated_commits i JOIN integration_operations o ON o.operation_id=i.operation_id").unwrap()
        .query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(integrated, vec![r1.clone()]);
    let tip = lab.git(&["rev-parse", "refs/heads/integration"]);
    assert_ne!(tip, target);
    lab.git(&["merge-base", "--is-ancestor", &g1_oid, &tip]);
    assert_eq!((&show(&g)["status"], &show(&g)["integration_hold"]), (&json!("closed"), &hold));
    assert_eq!(show(&n)["integration_hold"], hold);
}

/// D6 follow-up and the group hold on the dependency path. Dependents `bx`,
/// `bc`, `bg`, `bh` are queued on the `verified_result` of seeded candidate X
/// (task `x`), clean control C (`c`), and groups G (task `g`) and H (task
/// `h`), each with arms 1 `codex` and 2 `fast`, before any verification. All
/// six submissions pass verification. `bc` then counts (only
/// `admission_disabled:verified_result`); `bx` never does, though its
/// satisfaction row names X's verified result. Neither group has a selection,
/// so `bg` and `bh` stay blocked although each edge records its latest
/// attempt's (arm 2's) result. The rule selects G's arm 1: the edge moves to
/// arm 1's result and `bg` counts, though arm 1 is not the latest attempt
/// (`selected_earlier_arm_releases_verified_result_dependents`). The operator
/// selects H's arm 2: `bh` counts.
#[test]
fn seeded_or_unselected_result_never_releases_dependents() {
    let lab = IntegrationLab::new();
    let (codex, fast) = lab.arm_profiles();
    let seed = seed_fixture("logic-inverted-guard");
    let clean = seed_set()["clean"].as_str().unwrap().to_owned();
    let x = lab.submit("x", &lab.candidate("seeded", seed["seeded"].as_str().unwrap()));
    let c = lab.submit("c", &lab.candidate("clean", &clean));
    lab.telemetry(&["review", "seeds", "register", &x, "--seed", &format!("logic={}", reproducer_ref(&seed))]);
    lab.telemetry(&["review", "seeds", "register", &c, "--control"]);
    let mut arms = std::collections::BTreeMap::new();
    for task in ["g", "h"] {
        let digest = lab.contract(task);
        let group = lab.telemetry(&["quality", "groups", "create", task, "--arm", "codex", "--arm", "fast"])["group"]["group_id"].as_str().unwrap().to_owned();
        for (arm, configuration) in [(1, &codex), (2, &fast)] {
            let attempt = format!("{task}-a{arm}");
            lab.attempt(task, &attempt, Some((&group, arm, configuration)));
            let submission = lab.result(task, &attempt, &digest, &lab.candidate(&attempt, &format!("{clean}// {attempt}\n")));
            arms.insert((task, arm), submission);
        }
        arms.insert((task, 0), group);
    }
    for (consumer, predecessor) in [("bx", "x"), ("bc", "c"), ("bg", "g"), ("bh", "h")] { lab.queue_dependent(consumer, predecessor); }
    let missing = |p: &str| vec![format!("verified_dependency_evidence_unavailable:{p}:verified_result")];
    let counts = vec!["admission_disabled:verified_result".to_owned()];
    for (consumer, predecessor) in [("bx", "x"), ("bc", "c"), ("bg", "g"), ("bh", "h")] { assert_eq!(lab.blockers(consumer), missing(predecessor), "{consumer}"); }

    let x_result = lab.verify(&x, "verify-x");
    lab.verify(&c, "verify-c");
    let mut results = std::collections::BTreeMap::new();
    for key in [("g", 1), ("g", 2), ("h", 1), ("h", 2)] { results.insert(key, lab.verify(&arms[&key], &format!("verify-{}-{}", key.0, key.1))); }
    let edge = |consumer: &str| lab.db().query_row("SELECT evidence_id FROM dependency_satisfactions WHERE task_id=?1 AND state='valid'", [consumer], |r| r.get::<_, String>(0)).unwrap();
    assert_eq!(lab.blockers("bc"), counts, "the clean control's verified result releases its dependent");
    assert_eq!(edge("bx"), x_result, "the seeded result is recorded on the edge");
    assert_eq!(lab.blockers("bx"), missing("x"), "a seeded candidate never releases a dependent");
    assert_eq!((edge("bg"), edge("bh")), (results[&("g", 2)].clone(), results[&("h", 2)].clone()), "each edge records its latest attempt's result");
    assert_eq!((lab.blockers("bg"), lab.blockers("bh")), (missing("g"), missing("h")), "no arm releases before its group's selection");

    let g = lab.telemetry(&["quality", "groups", "select", &arms[&("g", 0)], "--rule"])["selection"].clone();
    assert_eq!((&g["arm"], &g["submission_id"]), (&json!(1), &json!(arms[&("g", 1)])));
    assert_eq!((edge("bg"), lab.blockers("bg")), (results[&("g", 1)].clone(), counts.clone()), "the selected earlier arm releases; the losing arm 2 does not");
    let h = lab.telemetry(&["quality", "groups", "select", &arms[&("h", 0)], "--arm", "2", "--reason", "operator_judgment"])["selection"].clone();
    assert_eq!((&h["arm"], &h["submission_id"]), (&json!(2), &json!(arms[&("h", 2)])));
    assert_eq!(lab.blockers("bh"), counts, "the selected arm releases its group's dependent");
    assert_eq!((lab.blockers("bx"), lab.blockers("bc")), (missing("x"), counts.clone()));
}

/// Owner decision "integration held until a selection exists" on the
/// dependency path (contracts-quality.md §3). Groups G (task `g`) and S (task
/// `s`) each seal arms 1 `codex` and 2 `fast`, bound to attempts `*-a1` then
/// `*-a2`; `bg` and `bs` are queued on their `verified_result`. G's arm 1
/// verifies first, then arm 2 (the latest attempt): the edge records arm 2's
/// result and `bg` is blocked. The rule selects arm 1: the selection moves the
/// edge to arm 1's result and `bg` counts, though arm 1 is not the latest
/// attempt. Arm 2 then verifies a second submission, and an unbound retry
/// `g-a3` (now the latest attempt) verifies one too: neither displaces the
/// winner; the edge history is arm 2 (invalid) then arm 1 (valid). S's arm 1
/// is a registered seeded candidate and both S arms verify: the edge records
/// arm 2's result (the latest arm), `bs` is blocked while both are held. The
/// operator's selection of arm 1 is refused (a seeded arm is never a winner)
/// and writes nothing; the rule (`first_accepted_in_launch_order.v2`) skips
/// arm 1 (`rule_skip` `seeded_candidate`, no rank) and selects arm 2 (rank
/// 1), which releases `bs`.
#[test]
fn selected_earlier_arm_releases_verified_result_dependents() {
    let lab = IntegrationLab::new();
    let (codex, fast) = lab.arm_profiles();
    let seed = seed_fixture("logic-inverted-guard");
    let clean = seed_set()["clean"].as_str().unwrap().to_owned();
    let (mut digests, mut groups) = (std::collections::BTreeMap::new(), std::collections::BTreeMap::new());
    for task in ["g", "s"] {
        digests.insert(task, lab.contract(task));
        groups.insert(task, lab.telemetry(&["quality", "groups", "create", task, "--arm", "codex", "--arm", "fast"])["group"]["group_id"].as_str().unwrap().to_owned());
        lab.attempt(task, &format!("{task}-a1"), Some((&groups[task], 1, &codex)));
        lab.attempt(task, &format!("{task}-a2"), Some((&groups[task], 2, &fast)));
    }
    lab.queue_dependent("bg", "g");
    lab.queue_dependent("bs", "s");
    let missing = |p: &str| vec![format!("verified_dependency_evidence_unavailable:{p}:verified_result")];
    let counts = vec!["admission_disabled:verified_result".to_owned()];
    let edge = |consumer: &str| lab.db().query_row("SELECT evidence_id FROM dependency_satisfactions WHERE task_id=?1 AND state='valid'", [consumer], |r| r.get::<_, String>(0)).unwrap();
    let submit = |task: &str, attempt: &str, source: &str, branch: &str| lab.result(task, attempt, &digests[task], &lab.candidate(branch, source));

    let g1 = submit("g", "g-a1", &format!("{clean}// g-a1\n"), "g-a1");
    let g2 = submit("g", "g-a2", &format!("{clean}// g-a2\n"), "g-a2");
    let r1 = lab.verify(&g1, "verify-g1");
    let r2 = lab.verify(&g2, "verify-g2");
    assert_eq!((edge("bg"), lab.blockers("bg")), (r2.clone(), missing("g")), "the edge records the latest arm, which is held");
    let selection = lab.telemetry(&["quality", "groups", "select", &groups["g"], "--rule"])["selection"].clone();
    assert_eq!((&selection["arm"], &selection["submission_id"]), (&json!(1), &json!(g1)));
    assert_eq!((edge("bg"), lab.blockers("bg")), (r1.clone(), counts.clone()), "the selected earlier arm releases its dependent");

    // Neither the losing arm nor a later retry of this revision displaces the winner.
    let g2b = submit("g", "g-a2", &format!("{clean}// g-a2 again\n"), "g-a2-again");
    lab.verify(&g2b, "verify-g2b");
    assert_eq!((edge("bg"), lab.blockers("bg")), (r1.clone(), counts.clone()), "arm 2 verified after the selection");
    lab.attempt("g", "g-a3", None);
    let g3 = submit("g", "g-a3", &format!("{clean}// g-a3\n"), "g-a3");
    lab.verify(&g3, "verify-g3");
    assert_eq!((edge("bg"), lab.blockers("bg")), (r1.clone(), counts.clone()), "an unbound later attempt");
    let history: Vec<(String, String)> = lab.db().prepare("SELECT evidence_id,state FROM dependency_satisfactions WHERE task_id='bg' ORDER BY rowid").unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(history, vec![(r2, "invalid".to_owned()), (r1, "valid".to_owned())]);

    // A seeded arm is never a winner (contracts-review.md §8): the rule skips it.
    let s1 = submit("s", "s-a1", seed["seeded"].as_str().unwrap(), "s-seeded");
    let s2 = submit("s", "s-a2", &format!("{clean}// s-a2\n"), "s-a2");
    lab.telemetry(&["review", "seeds", "register", &s1, "--seed", &format!("logic={}", reproducer_ref(&seed))]);
    lab.verify(&s1, "verify-s1");
    let rs2 = lab.verify(&s2, "verify-s2");
    assert_eq!((edge("bs"), lab.blockers("bs")), (rs2.clone(), missing("s")), "both arms are held before a selection");
    let refused = lab.fail(&["telemetry", "demo", "quality", "groups", "select", &groups["s"], "--arm", "1", "--reason", "operator_judgment"]);
    assert!(refused.contains("a seeded arm is an evaluation artefact and never a group's winner"), "{refused}");
    assert_eq!(lab.db().query_row("SELECT count(*) FROM candidate_selections WHERE group_id=?1", [&groups["s"]], |r| r.get::<_, i64>(0)).unwrap(), 0);
    let selection = lab.telemetry(&["quality", "groups", "select", &groups["s"], "--rule"])["selection"].clone();
    assert_eq!((&selection["arm"], &selection["submission_id"], &selection["selector_principal"]), (&json!(2), &json!(s2), &json!("rule:first_accepted_in_launch_order.v2")));
    assert_eq!(selection["evidence"].as_array().unwrap().iter().map(|e| (e["arm"].clone(), e["rank"].clone(), e["rule_skip"].clone())).collect::<Vec<_>>(),
        [(json!(1), json!(null), json!("seeded_candidate")), (json!(2), json!(1), json!(null))]);
    assert_eq!((edge("bs"), lab.blockers("bs")), (rs2, counts.clone()), "the clean arm the rule selects releases its dependent");
    assert_eq!(lab.blockers("bg"), counts);
}

/// `task complete` never ends a task from a seeded candidate or a held arm
/// (contracts-quality.md §3, contracts-review.md §8). Task `x`'s accepted
/// submission is a registered seeded candidate: completion is refused and
/// writes nothing, while clean control `c` completes. Group G (task `g`) binds
/// arms 1 and 2; both verify and arm 2 is the active, started attempt: refused
/// before any selection and again after the rule selects arm 1 (arm 2 lost).
/// Group H (task `h`): arm 2's attempt verifies two submissions and the
/// operator selects its second: completion is allowed and names that selected
/// submission, not the arm's first.
#[test]
fn seeded_or_held_submission_cannot_complete_the_task() {
    let lab = IntegrationLab::new();
    let (codex, fast) = lab.arm_profiles();
    let seed = seed_fixture("logic-inverted-guard");
    let clean = seed_set()["clean"].as_str().unwrap().to_owned();
    let refused = |task: &str, reason: &str| {
        let head = || lab.db().query_row("SELECT max(sequence) FROM events", [], |r| r.get::<_, i64>(0)).unwrap();
        let before = head();
        let out = lab.complete(task);
        assert!(!out.status.success(), "{task} completed");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(reason), "{stderr}");
        assert_eq!((head(), lab.completion_requests(task)), (before, vec![]), "{task}: nothing written");
    };
    let completed = |task: &str| { let out = lab.complete(task); assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr)); };

    let x = lab.submit("x", &lab.candidate("seeded", seed["seeded"].as_str().unwrap()));
    let c = lab.submit("c", &lab.candidate("clean", &clean));
    lab.telemetry(&["review", "seeds", "register", &x, "--seed", &format!("logic={}", reproducer_ref(&seed))]);
    lab.telemetry(&["review", "seeds", "register", &c, "--control"]);
    lab.verify(&x, "verify-x");
    lab.verify(&c, "verify-c");
    let mut groups = std::collections::BTreeMap::new();
    let mut subs = std::collections::BTreeMap::new();
    for task in ["g", "h"] {
        let digest = lab.contract(task);
        groups.insert(task, lab.telemetry(&["quality", "groups", "create", task, "--arm", "codex", "--arm", "fast"])["group"]["group_id"].as_str().unwrap().to_owned());
        for (arm, configuration) in [(1, &codex), (2, &fast)] {
            let attempt = format!("{task}-a{arm}");
            lab.attempt(task, &attempt, Some((&groups[task], arm, configuration)));
            let submission = lab.result(task, &attempt, &digest, &lab.candidate(&attempt, &format!("{clean}// {attempt}\n")));
            lab.verify(&submission, &format!("verify-{attempt}"));
            subs.insert((task, arm), submission);
        }
        if task == "h" {
            let second = lab.result(task, "h-a2", &digest, &lab.candidate("h-a2-second", &format!("{clean}// h-a2 second\n")));
            lab.verify(&second, "verify-h-a2-second");
            subs.insert((task, 3), second);
        }
    }
    // The fixture's launch records are minimal: nothing reads a snapshot after this.
    for (task, attempt) in [("x", "x-attempt"), ("c", "c-attempt"), ("g", "g-a2"), ("h", "h-a2")] { lab.started(task, attempt); }
    refused("x", "a seeded candidate never completes its task");
    completed("c");
    assert_eq!(lab.completion_requests("c"), vec![c]);
    let held = "a candidate group's task completes only from its selected submission";
    refused("g", held);
    refused("h", held);
    let g = lab.telemetry(&["quality", "groups", "select", &groups["g"], "--rule"])["selection"].clone();
    assert_eq!(g["submission_id"], json!(subs[&("g", 1)]));
    refused("g", held);
    let h = lab.telemetry(&["quality", "groups", "select", &groups["h"], "--arm", "2", "--submission", &subs[&("h", 3)], "--reason", "operator_judgment"])["selection"].clone();
    assert_eq!(h["submission_id"], json!(subs[&("h", 3)]));
    completed("h");
    assert_eq!(lab.completion_requests("h"), vec![subs[&("h", 3)].clone()], "completion names the selected submission, not the arm's first");
}

/// Doc 10 §5 seeded-review fixture. Seeded candidates S1–S4 (starter seeds
/// logic, boundary, security, test_weakening; seeds 1–4) and clean controls
/// C1, C2 are registered at seq 1–6; 7 opens a review of an unregistered
/// candidate (which then cannot join). Configuration R (`fast`) reviews each
/// once; each review's opening, assignment, session start and completion take
/// the four seqs before its submissions: S1 (8–11) reports two findings
/// (claims 1, 2; seq 12, 13), S2–S4, C1, C2 one each (claims 3–7; seq 18, 23,
/// 28, 33, 38). A second review of S1 by Q (`claude`) is opened (39) and
/// assigned (40), starts (41) and times out (42): not completed, so no trial
/// (`not_completed` 1), never a 0. Before triage every trial is pending: M43
/// and M44 are null (empty denominator) with pending 4 and 2. Seq 43–49
/// triage: claims 1–5 and 7 validated as new findings (claim 1 as
/// `finding:canonical-43`), claim 6 rejected. Seq 50–52 link claims 1, 3, 4 to
/// seeds 1–3. By hand: M43 = 3/4 = 75.00 (S4's seed missed; its validated
/// claim 5 is an ordinary finding), M44 = 1/2 = 50.00 (C1's rejected-only
/// submission; C2's validated finding is not a false alarm), per
/// configuration R the same; each seed class has 1 trial, below
/// `--min-trials 2`, so suppressed with counts. Detection adds no triage (6
/// unique findings), but the three seed-linked submissions are evaluation
/// artefacts outside M22 and M21 (§9): M22 = 3/4 (claims 2, 5, 7 validated, 6
/// rejected), M21 = 3 of the 6 findings. Seq 53 resets claim 1: seed 1
/// pending, M43 = 2/3. Seq 54 validates it again: 3/4. Seq 55 retracts
/// detection 52: 2/4; as of 54 still 3/4. Seq 56 re-links it: 3/4. Seq 57
/// reveals S1 (every review ended), seq 58 discards it.
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
    for (seed, claim, seq) in [("1", "1", 50), ("2", "3", 51), ("3", "4", 52)] {
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
    // Seed-linked submissions are evaluation artefacts (§9); incidental real findings stay ordinary.
    assert_eq!((&full["M22"]["value"], &full["M22"]["seeded_evaluation"]), (&json!("3/4"), &json!({"submissions": 3, "claims": 3})));
    assert_eq!((&full["M21"]["value"], &full["M21"]["seeded_evaluation"]), (&json!("3"), &json!(3)));
    let triage = f.cli_args(&["review", "findings", "show"]).0["findings"].clone();
    assert_eq!(triage["unique_findings"], json!(6));

    // Triage corrections and retractions replay through the one ordering.
    f.cli_args(&["review", "findings", "reset", "1", "--reason", "decided_in_error"]);
    assert_eq!(cell(&report(&[])["M43"], "detected", "trials"), (json!(2), json!(3), json!(1), json!("2/3"), json!("66.67")));
    f.cli_args(&["review", "findings", "validate", "1", "--finding", "finding:canonical-43", "--severity", "high", "--evidence", &evidence('e')]);
    assert_eq!(report(&[])["M43"]["value"], json!("3/4"));
    assert_eq!(seeds_cmd(&["retract", "52"])["event"]["seq"], json!(55));
    assert_eq!(report(&[])["M43"]["value"], json!("2/4"));
    assert_eq!((&report(&["--as-of", "54"])["M43"]["value"], &report(&["--as-of", "54"])["M43"]["as_of_seq"]), (&json!("3/4"), &json!(54)));
    seeds_cmd(&["detect", "3", "--claim", "4", "--evidence", &evidence('b')]);

    // Reveal only after every review ended; nothing reviews it afterwards.
    assert!(seeds_fail(&["dispose", &s[0].0, "--disposition", "discarded"]).contains("not revealed yet"));
    assert_eq!(seeds_cmd(&["reveal", &s[0].0])["event"]["seq"], json!(57));
    let again = f.cli_fail(&["review", "open", &s[0].0, "--protocol", "review-protocol.v1"]);
    assert!(again.contains("not reviewed again"), "{again}");
    assert_eq!(seeds_cmd(&["dispose", &s[0].0, "--disposition", "discarded"])["event"]["seq"], json!(58));
    let m = report(&[]);
    assert_eq!((&m["M43"]["value"], &m["M44"]["value"], &m["M43"]["as_of_seq"]), (&json!("3/4"), &json!("1/2"), &json!(58)));
    let shown = seeds_cmd(&["show"])["seeds"].clone();
    let s1 = &shown["candidates"][0];
    assert_eq!((&s1["revealed_seq"], &s1["disposal"], &s1["seeds"][0]["reproducer_ref"], &s1["seeds"][0]["seed_class"]),
        (&json!(57), &json!("discarded"), &json!(reproducer_ref(&seeds[0])), &json!("logic")));
    let kinds: Vec<&str> = shown["history"].as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["registered", "registered", "registered", "registered", "registered", "registered", "detected", "detected", "detected", "retracted", "detected", "revealed", "disposed"]);
    assert_eq!(shown["trials"].as_array().unwrap().iter().map(|t| t["status"].as_str().unwrap()).collect::<Vec<_>>(), ["detected", "detected", "detected", "missed"]);
}

// ---- Card D7: review lifecycle in the shared ledger, seed-linked credit,
// protocol retractions and the worker guard (contracts-review.md §9).

#[path = "../src/store/test_schema.rs"]
mod test_schema;

/// Start `attempt`'s session of an assigned opportunity; returns its id.
fn start_session(f: &Fixture, opportunity: &str, attempt: &str) -> String {
    f.cli_args(&["review", "start", opportunity, "--attempt", attempt]).0["session"]["session_id"].as_str().unwrap().to_owned()
}

/// Complete `session` on artifact `c` with `outcome`, `findings` and `evidence`.
fn complete_session(f: &Fixture, session: &str, c: char, outcome: &str, findings: serde_json::Value, evidence: serde_json::Value) -> serde_json::Value {
    let mut receipt = json!({"schema": "review_receipt.v1", "session_id": session, "submission_id": hex(c), "candidate_oid": oid(c), "outcome": outcome,
        "findings": findings, "evidence": evidence});
    if outcome != "completed" { receipt["reason"] = json!("budget_exhausted"); }
    f.cli_args(&["review", "complete", "--input-file", &input(f, &format!("{}.json", &session[7..19]), &receipt)]).0["completion"].clone()
}

/// §9 golden: review status replays with the ledger; owner retractions.
/// History by hand: 1 registers the skeptical protocol; 2 preregisters E
/// (randomized, seed a…a, min 2 units, stopping rule `planned_units` = 2);
/// 3–5 open B1–B3 (S1–S3); 6 and 7 assign B1 (`standard`) and B2
/// (`skeptical`) to E; B3 is refused: E reached its 2 planned units. B1 is
/// assigned its reviewer (8); its first session starts (9) and times out
/// (10); a restart starts (11) and completes (12) with `b1` (submission 1,
/// seq 13), validated at 14 as `finding:canonical-14`. B2 is assigned (15),
/// its session starts (16) and completes with nothing (17). 18 opens pass P2
/// (S2) and 19 binds it after B2 (cutoff 18); P2 is assigned (20), starts
/// (21) and completes (22) with `k2` (submission 2, seq 23), validated at 24
/// as `finding:canonical-24`. By hand, unit S1 is `pending` as of 8 (no
/// session yet) and 11 (restart in progress), `analyzable` with outcome 0 as
/// of 10 (its only session ended without completion), `pending` as of 13 (b1
/// untriaged) and `analyzable` with outcome 1 at the head; the stored status
/// would call it ended at every watermark. P2 is `in_progress`
/// (`not_completed`) as of 21, `completed` but `pending_triage` as of 23, and
/// eligible with one new finding at 24: M28 = 1/1. 25 retracts P2's binding:
/// M28 null (excluded `retracted` 1), unit S2 has no treatment (pending); as
/// of 24 the pass still counts. 26 excludes unit S1, 27 retracts that
/// exclusion: S1 is analyzable again, excluded as of 26.
#[test]
fn review_status_replays_with_the_ledger_and_owner_retracts_protocol_records() {
    let f = Fixture::new();
    artifact_world(&f, &['1', '2', '3'], &["rev-b1", "rev-b1b", "rev-b2", "rev-k2"]);
    let db_path = f.project.join(".state/state.db");
    f.cli_args(&["review", "protocols", "register", "--input-file", &input(&f, "protocol.json", &skeptical_protocol())]);
    let experiment = json!({"schema": "review_experiment.v1", "experiment": "planned.v1", "design": "randomized", "seed": hex('a'),
        "eligibility": {"kind": "code", "scope": "candidate_diff", "role": "evaluation", "protocol": "review-protocol.v1"},
        "arms": [{"arm": "standard", "protocol": null}, {"arm": "skeptical", "protocol": SKEPTICAL}],
        "primary_outcome": "validated_unique_findings.v1", "adjudication": "owner_triage.v1", "horizon_days": 14, "min_units": 2, "stopping_rule": "planned_units", "planned_units": 2});
    let mut unplanned = experiment.clone();
    unplanned["planned_units"] = json!(null);
    assert!(f.cli_fail(&["review", "experiments", "register", "--input-file", &input(&f, "unplanned.json", &unplanned)]).contains("needs planned_units"));
    assert_eq!(f.cli_args(&["review", "experiments", "register", "--input-file", &input(&f, "planned.json", &experiment)]).0["event"]["seq"], json!(2));
    let base: Vec<String> = ['1', '2', '3'].into_iter().map(|c| open_review(&f, c, "code", "review-protocol.v1", None)).collect();
    for (b, seq, arm) in [(&base[0], 6, "standard"), (&base[1], 7, "skeptical")] {
        let e = f.cli_args(&["review", "experiments", "assign", "planned.v1", b]).0["event"].clone();
        assert_eq!((&e["seq"], &e["subject"]["arm"]), (&json!(seq), &json!(arm)));
    }
    // The stopping rule ends assignment at the planned units (store and trigger).
    assert!(f.cli_fail(&["review", "experiments", "assign", "planned.v1", &base[2]]).contains("reached its 2 planned units"));
    {
        let mut db = rusqlite::Connection::open(&db_path).unwrap();
        let tx = db.transaction().unwrap();
        tx.execute("INSERT INTO protocol_log(seq,kind,principal,authority,expected_seq,recorded_unix_ms) VALUES(8,'unit_assigned','operator:cli','operator_owner.v1',NULL,1)", []).unwrap();
        let raw = tx.execute("INSERT INTO experiment_units(seq,experiment_seq,opportunity_id,submission_id,arm,block) VALUES(8,2,?1,?2,'standard',NULL)", [&base[2], &hex('3')]).unwrap_err();
        assert!(raw.to_string().contains("reached its planned units"), "{raw}");
    }

    // B1 times out, then a restart completes with one finding.
    f.cli_args(&["review", "assign", &base[0], "--reviewer", "fast"]);
    let first = start_session(&f, &base[0], "rev-b1");
    complete_session(&f, &first, '1', "timed_out", json!([]), json!([]));
    let restart = start_session(&f, &base[0], "rev-b1b");
    assert_eq!(complete_session(&f, &restart, '1', "completed", json!(["finding:b1"]), json!([]))["finding_submissions"], json!([1]));
    assert_eq!(f.cli_args(&["review", "findings", "show"]).0["findings"]["submissions"][0]["seq"], json!(13));
    f.cli_args(&["review", "findings", "validate", "1", "--new", "--severity", "high", "--evidence", &evidence('e')]);
    let s1 = |as_of: Option<i64>| {
        let unit = experiments_show(&f, as_of)["units"][0].clone();
        (unit["status"].as_str().unwrap().to_owned(), unit["outcome"].clone())
    };
    assert_eq!(s1(Some(8)), ("pending".to_owned(), json!(null)));
    assert_eq!(s1(Some(10)), ("analyzable".to_owned(), json!(0)));
    assert_eq!(s1(Some(11)), ("pending".to_owned(), json!(null)));
    assert_eq!(s1(Some(13)), ("pending".to_owned(), json!(null)));
    assert_eq!(s1(None), ("analyzable".to_owned(), json!(1)));
    assert_eq!(experiments_show(&f, None)["units"][0]["new_unique_findings"], json!(["finding:canonical-14"]));

    // B2, then pass P2 after it.
    f.cli_args(&["review", "assign", &base[1], "--reviewer", "fast"]);
    let b2 = start_session(&f, &base[1], "rev-b2");
    complete_session(&f, &b2, '2', "completed", json!([]), json!([]));
    let p2 = open_review(&f, '2', "skeptical", SKEPTICAL, Some(BUDGET));
    let bound = f.cli_args(&["review", "protocols", "bind", &p2, "--prior", &base[1]]).0["event"].clone();
    assert_eq!((&bound["seq"], &bound["subject"]["cutoff_seq"], &bound["subject"]["prior_coverage"]), (&json!(19), &json!(18), &json!("complete")));
    f.cli_args(&["review", "assign", &p2, "--reviewer", "fast"]);
    let k2 = start_session(&f, &p2, "rev-k2");
    complete_session(&f, &k2, '2', "completed", json!(["finding:k2"]), json!([evidence('a')]));
    f.cli_args(&["review", "findings", "validate", "2", "--new", "--severity", "medium", "--evidence", &evidence('e')]);
    let pass = |as_of: Option<i64>| {
        let p = protocols_show(&f, as_of)["passes"][0].clone();
        (p["status"].clone(), p["exclusion"].clone(), p["new_unique_findings"].clone(), p["retracted_seq"].clone())
    };
    assert_eq!(pass(Some(21)), (json!("in_progress"), json!("not_completed"), json!([]), json!(null)));
    assert_eq!(pass(Some(23)), (json!("completed"), json!("pending_triage"), json!([]), json!(null)));
    assert_eq!(pass(None), (json!("completed"), json!(null), json!(["finding:canonical-24"]), json!(null)));
    assert_eq!(f.cli_args(&["review", "report"]).0["metrics"]["M28"]["value"], json!("1/1"));
    let unit2 = experiments_show(&f, None)["units"][1].clone();
    assert_eq!((&unit2["status"], &unit2["outcome"], &unit2["treatment_received"]), (&json!("analyzable"), &json!(1), &json!(true)));

    // Retracting the pass binding: owner only, once, and only a pass binding.
    let mut store = SqliteStore::open(&db_path).unwrap();
    for worker in ["worker:rev-k2", "rev-k2"] {
        assert!(format!("{:?}", store.retract_protocol_record(19, "pass_bound", None, worker, 1).unwrap_err()).contains("a worker cannot"), "{worker}");
    }
    drop(store);
    assert!(f.cli_fail(&["review", "protocols", "retract", "3"]).contains("no pass binding at seq 3"));
    assert!(f.cli_fail(&["review", "experiments", "retract", "19"]).contains("no unit exclusion at seq 19"));
    let retracted = f.cli_args(&["review", "protocols", "retract", "19"]).0["event"].clone();
    assert_eq!((&retracted["seq"], &retracted["kind"], &retracted["principal"], &retracted["authority"], &retracted["subject"]),
        (&json!(25), &json!("pass_retracted"), &json!("operator:cli"), &json!("operator_owner.v1"), &json!({"reverses": 19})));
    assert!(f.cli_fail(&["review", "protocols", "retract", "19"]).contains("already retracted"));
    assert_eq!(pass(None), (json!("completed"), json!("retracted"), json!(["finding:canonical-24"]), json!(25)));
    assert_eq!(pass(Some(24)), (json!("completed"), json!(null), json!(["finding:canonical-24"]), json!(null)));
    let m28 = f.cli_args(&["review", "report"]).0["metrics"]["M28"].clone();
    assert_eq!((&m28["value"], &m28["reason"], &m28["excluded"]), (&json!(null), &json!("empty_denominator"), &json!({"retracted": 1})));
    let unit2 = experiments_show(&f, None)["units"][1].clone();
    assert_eq!((&unit2["status"], &unit2["passes"], &unit2["treatment_received"]), (&json!("pending"), &json!([]), &json!(false)));

    // Retracting an exclusion returns the unit to the estimate; the exclusion stays listed.
    assert_eq!(f.cli_args(&["review", "experiments", "exclude", "planned.v1", &base[0], "--reason", "operator_error"]).0["event"]["seq"], json!(26));
    assert_eq!(f.cli_args(&["review", "experiments", "retract", "26"]).0["event"]["kind"], json!("exclusion_retracted"));
    assert!(f.cli_fail(&["review", "experiments", "exclude", "planned.v1", &base[0], "--reason", "operator_error"]).contains("a retracted exclusion is not recorded again"));
    let e = experiments_show(&f, None);
    assert_eq!((&e["units"][0]["status"], &e["units"][0]["exclusion"], &e["estimate"]["arms"]["standard"]["excluded"]),
        (&json!("analyzable"), &json!({"seq": 26, "reason": "operator_error", "retracted_seq": 27}), &json!({})));
    let then = experiments_show(&f, Some(26));
    assert_eq!((&then["units"][0]["status"], &then["units"][0]["exclusion"], &then["estimate"]["arms"]["standard"]["excluded"]),
        (&json!("excluded"), &json!({"seq": 26, "reason": "operator_error", "retracted_seq": null}), &json!({"operator_error": 1})));
    assert_eq!(f.cli_args(&["review", "findings", "show"]).0["findings"]["head_seq"], json!(27));

    // Raw rows cannot forge the owner's correction or break the one ordering.
    let db = rusqlite::Connection::open(&db_path).unwrap();
    let forged = db.execute("INSERT INTO review_log(seq,kind,principal,authority,recorded_unix_ms) VALUES(28,'pass_retracted','worker:rev-k2','operator_owner.v1',1)", []).unwrap_err();
    assert!(forged.to_string().contains("CHECK constraint failed"), "{forged}");
    let early = db.execute("INSERT INTO review_log(seq,kind,principal,authority,recorded_unix_ms) VALUES(14,'started','operator:cli','review_capture.v1',1)", []).unwrap_err();
    assert!(early.to_string().contains("one ordering"), "{early}");
    let rewrite = db.execute("UPDATE review_session_events SET seq=seq", []).unwrap_err();
    assert!(rewrite.to_string().contains("append-only"), "{rewrite}");
}

/// §9 seed-linked credit golden. Seq 1 registers S (1…1) as seeded (seed 1,
/// logic). Review R1 is opened (2) and assigned (3), starts (4) and completes
/// (5) with `incidental` (submission 1, claim 1, seq 6) and `seed-report`
/// (submission 2, claim 2, seq 7); R2 is opened (8) and assigned (9), starts
/// (10) and completes (11) with `seed-again` (submission 3, claim 3, seq 12).
/// 13 validates claim 2 as X = `finding:canonical-13`, 14 claim 1 as I =
/// `finding:canonical-14`, 15 marks claim 3 a duplicate of X. By hand: M22 =
/// 2/3, M23 = 1/3, M21 = 2. 16 links claim 2 to seed 1: submission 2 is an
/// evaluation artefact, M22 = 1/2 (1 validated, 1 duplicate-only), M23 = 1/2,
/// seeded 1 submission; X's discovery is linked, so M21 = 1 (I only;
/// seeded 1) and M25 = 0/1. 17 also links claim 3: M22 = 1/1, M23 = 0/1,
/// seeded 2.
/// 18 retracts detection 16: claim 2 is an ordinary discovery again, M22 =
/// 2/2, M23 = 0/2, seeded 1 submission, M21 = 2. As-of views replay the
/// links: claim 2 is linked as of 16 and 17, not as of 15 or 18; X is an
/// evaluation finding as of 16, not at the head.
#[test]
fn seed_linked_findings_stay_outside_discovery_and_validation_credit() {
    let f = Fixture::new();
    let world = review_world(&f, &["rev-a1", "rev-a2"]);
    let seed = seed_fixture("logic-inverted-guard");
    assert_eq!(f.cli_args(&["review", "seeds", "register", &world.0, "--seed", &format!("logic={}", reproducer_ref(&seed))]).0["event"]["seq"], json!(1));
    assert_eq!(completed_review(&f, &world, "code", "rev-a1", json!(["finding:seed-report", "finding:incidental"]))["finding_submissions"], json!([1, 2]));
    completed_review(&f, &world, "security", "rev-a2", json!(["finding:seed-again"]));
    let triage = f.cli_args(&["review", "findings", "show"]).0["findings"].clone();
    assert_eq!(triage["submissions"].as_array().unwrap().iter().map(|s| (s["seq"].as_i64().unwrap(), s["finding_ref"].as_str().unwrap().to_owned())).collect::<Vec<_>>(),
        [(6, "finding:incidental".to_owned()), (7, "finding:seed-report".to_owned()), (12, "finding:seed-again".to_owned())]);
    f.cli_args(&["review", "findings", "validate", "2", "--new", "--severity", "high", "--evidence", &evidence('e')]);
    f.cli_args(&["review", "findings", "validate", "1", "--new", "--severity", "medium", "--evidence", &evidence('e')]);
    f.cli_args(&["review", "findings", "duplicate", "3", "--of", "finding:canonical-13"]);
    let credit_now = || {
        let m = f.cli_args(&["review", "report"]).0["metrics"].clone();
        (m["M22"]["value"].clone(), m["M23"]["value"].clone(), m["M22"]["seeded_evaluation"].clone(), m["M21"]["value"].clone(), m["M21"]["seeded_evaluation"].clone())
    };
    assert_eq!(credit_now(), (json!("2/3"), json!("1/3"), json!({"submissions": 0, "claims": 0}), json!("2"), json!(0)));

    let detect = |claim: &str| f.cli_args(&["review", "seeds", "detect", "1", "--claim", claim, "--evidence", &evidence('a')]).0["event"]["seq"].clone();
    assert_eq!(detect("2"), json!(16));
    assert_eq!(credit_now(), (json!("1/2"), json!("1/2"), json!({"submissions": 1, "claims": 1}), json!("1"), json!(1)));
    let m = f.cli_args(&["review", "report"]).0["metrics"].clone();
    assert_eq!((&m["M21"]["drilldown"], &m["M21"]["by_configuration"], &m["M25"]["value"], &m["M25"]["seeded_evaluation"]),
        (&json!({"validated_unique_findings": 1}), &json!({"unknown": "1"}), &json!("0/1"), &json!(1)));
    // Detection changes no triage: two unique findings, the same claim outcomes.
    assert_eq!(f.cli_args(&["review", "findings", "show"]).0["findings"]["unique_findings"], json!(2));
    assert_eq!(detect("3"), json!(17));
    assert_eq!(credit_now(), (json!("1/1"), json!("0/1"), json!({"submissions": 2, "claims": 2}), json!("1"), json!(1)));
    assert_eq!(f.cli_args(&["review", "seeds", "retract", "16"]).0["event"]["seq"], json!(18));
    assert_eq!(credit_now(), (json!("2/2"), json!("0/2"), json!({"submissions": 1, "claims": 1}), json!("2"), json!(0)));

    // As-of views replay each link and its retraction.
    let linked = |as_of: i64| {
        let state = f.cli_args(&["review", "findings", "show", "--as-of", &as_of.to_string()]).0["findings"].clone();
        state["submissions"].as_array().unwrap().iter().map(|s| s["claims"][0]["seed_linked"].as_bool().unwrap()).collect::<Vec<_>>()
    };
    assert_eq!(linked(15), [false, false, false]);
    assert_eq!(linked(16), [false, true, false]);
    assert_eq!(linked(17), [false, true, true]);
    assert_eq!(linked(18), [false, false, true]);
    let seeded = |as_of: Option<i64>| fixes_show(&f, as_of)["findings"].as_array().unwrap().iter()
        .map(|x| (x["finding_id"].as_str().unwrap().to_owned(), x["seeded_evaluation"].as_bool().unwrap())).collect::<Vec<_>>();
    assert_eq!(seeded(Some(16)), [("finding:canonical-13".to_owned(), true), ("finding:canonical-14".to_owned(), false)]);
    assert_eq!(seeded(None), [("finding:canonical-13".to_owned(), false), ("finding:canonical-14".to_owned(), false)]);
}

/// The owner's review CLI refuses a worker execution context: HOME set to a
/// retained profile's execution home (the author's `codex-home`, also its
/// collector binding's, or `fast`'s) or a working directory inside a task
/// worktree. Refusals write nothing; the blind `present` view stays open to
/// reviewers, and the owner's own context is unaffected.
#[test]
fn review_cli_refuses_worker_execution_context() {
    let f = Fixture::new();
    let world = review_world(&f, &["rev-a1"]);
    let db_path = f.project.join(".state/state.db");
    let opportunity = open_review(&f, '1', "code", "review-protocol.v1", None);
    let worktree = std::path::PathBuf::from(f.worktree());
    fs::create_dir_all(&worktree).unwrap();
    let owner_home = f.tmp.path().join("home");
    let run = |home: &std::path::Path, cwd: &std::path::Path, args: &[&str]| {
        std::process::Command::new(BIN).env_clear().env("HOME", home).env("PATH", "/usr/bin:/bin").current_dir(cwd)
            .args(["--root", f.root.to_str().unwrap(), "telemetry", "demo", "review"]).args(args).output().unwrap()
    };
    // A second, unreviewed submission the owner can still register.
    Factory::open(&f).submission(&hex('2'), &f.attempt, &oid('2'), 2_000);
    let control = hex('2');
    let register = ["seeds", "register", control.as_str(), "--control"];
    let seed_rows = || rusqlite::Connection::open(&db_path).unwrap().query_row("SELECT count(*) FROM seed_log", [], |r| r.get::<_, i64>(0)).unwrap();
    for (home, cwd, marker) in [(f.home.clone(), f.tmp.path().to_path_buf(), "HOME is a worker execution home"),
        (f.tmp.path().join("fast-home"), f.tmp.path().to_path_buf(), "HOME is a worker execution home"),
        (owner_home.clone(), worktree.clone(), "the working directory is a task worktree")] {
        for args in [&register[..], &["seeds", "show"], &["findings", "show"], &["report"], &["open", world.0.as_str(), "--protocol", "review-protocol.v1"]] {
            let out = run(&home, &cwd, args);
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(!out.status.success() && stderr.contains("refuses to run inside a worker execution context") && stderr.contains(marker), "{args:?} {home:?}: {stderr}");
        }
        let presented = run(&home, &cwd, &["present", &opportunity]);
        assert!(presented.status.success(), "{}", String::from_utf8_lossy(&presented.stderr));
    }
    assert_eq!(seed_rows(), 0, "refused commands write nothing");
    let owner = run(&owner_home, f.tmp.path(), &register);
    assert!(owner.status.success(), "{}", String::from_utf8_lossy(&owner.stderr));
    assert_eq!(seed_rows(), 1);
}

/// 0059 and 0063 upgrade golden: review history recorded on a schema-58
/// store, whose ledger held no review status and no opportunities. Seq 1
/// registers the skeptical protocol; O1's submission is seq 2 and its
/// validation (K = `finding:canonical-3`) seq 3; 4 binds pass P1 after O1,
/// which then completes with no finding. The upgrade sequences the four
/// session events after the head, in start/completion order: O1 start 5,
/// completion 6, P1 start 7, completion 8; then (0063) the openings and
/// assignments after that, in time order: O1 opened 9, assigned 10, P1
/// opened 11, assigned 12. Each is `backfilled`, so replay keeps them at
/// every watermark (P1 is `completed` as of 4, as the stored status said; both
/// opportunities and their assignments are listed as of 1). After the
/// upgrade, P2 (S2) is opened at 13 and bound at 14, assigned at 15, and its
/// review starts (16) and completes (17) normally: listed but unassigned as
/// of 13, `in_progress` as of 16, `completed` as of 17.
#[test]
fn review_ledger_upgrade_backfills_sessions_in_completion_order() {
    let f = Fixture::new();
    artifact_world(&f, &['1', '2'], &["rev-a1", "rev-k1", "rev-k2"]);
    let db_path = f.project.join(".state/state.db");
    test_schema::historical(&rusqlite::Connection::open(&db_path).unwrap(), 58).unwrap();
    let raw = rusqlite::Connection::open(&db_path).unwrap();
    let version = || raw.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0)).unwrap();
    assert_eq!(version(), 58);
    f.cli_args(&["review", "protocols", "register", "--input-file", &input(&f, "protocol.json", &skeptical_protocol())]);
    let o1 = open_review(&f, '1', "code", "review-protocol.v1", None);
    assert_eq!(run_review(&f, &o1, '1', "rev-a1", json!(["finding:k"]), json!([]))["finding_submissions"], json!([1]));
    assert_eq!(f.cli_args(&["review", "findings", "validate", "1", "--new", "--severity", "high", "--evidence", &evidence('e')]).0["event"]["subject"]["finding_id"],
        json!("finding:canonical-3"));
    let p1 = open_review(&f, '1', "skeptical", SKEPTICAL, Some(BUDGET));
    assert_eq!(f.cli_args(&["review", "protocols", "bind", &p1, "--prior", &o1]).0["event"]["seq"], json!(4));
    run_review(&f, &p1, '1', "rev-k1", json!([]), json!([evidence('a')]));
    assert_eq!((version(), f.cli_args(&["review", "findings", "show"]).0["findings"]["head_seq"].clone()), (58, json!(4)));

    SqliteStore::open(&db_path).unwrap().upgrade_v1().unwrap();
    assert_eq!(version(), i64::from(herdr_projects::store::SCHEMA));
    let sessions: Vec<String> = raw.prepare("SELECT session_id FROM review_sessions ORDER BY started_unix_ms").unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
    let events = || raw.prepare("SELECT e.seq,e.session_id,e.event,e.backfilled,l.kind,l.authority FROM review_session_events e JOIN review_log l ON l.seq=e.seq ORDER BY e.seq").unwrap()
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?, r.get::<_, String>(4)?, r.get::<_, String>(5)?)))
        .unwrap().map(Result::unwrap).collect::<Vec<_>>();
    let row = |seq: i64, session: &str, event: &str, backfilled: i64| (seq, session.to_owned(), event.to_owned(), backfilled, event.to_owned(), "review_capture.v1".to_owned());
    assert_eq!(events(), [row(5, &sessions[0], "started", 1), row(6, &sessions[0], "completed", 1), row(7, &sessions[1], "started", 1), row(8, &sessions[1], "completed", 1)]);
    let status = |as_of: i64| protocols_show(&f, Some(as_of))["passes"].as_array().unwrap().iter().map(|p| p["status"].as_str().unwrap().to_owned()).collect::<Vec<_>>();
    assert_eq!((status(4), status(8)), (vec!["completed".to_owned()], vec!["completed".to_owned()]));
    let opportunity_rows = || raw.prepare("SELECT seq,opportunity_id,event,backfilled,authority FROM review_opportunity_log ORDER BY seq").unwrap()
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?, r.get::<_, String>(4)?)))
        .unwrap().map(Result::unwrap).collect::<Vec<_>>();
    let opp = |seq: i64, id: &str, event: &str, backfilled: i64| (seq, id.to_owned(), event.to_owned(), backfilled, "review_capture.v1".to_owned());
    assert_eq!(opportunity_rows(), [opp(9, &o1, "opened", 1), opp(10, &o1, "assigned", 1), opp(11, &p1, "opened", 1), opp(12, &p1, "assigned", 1)]);
    // Backfilled openings and assignments are visible at every watermark, with no ledger seq of their own.
    let listed = |as_of: i64| f.cli_args(&["review", "show", "--as-of", &as_of.to_string()]).0["opportunities"].as_array().unwrap().iter()
        .map(|o| (o["opportunity_id"].as_str().unwrap().to_owned(), o["opened_seq"].clone(), o["assignment"]["assigned_seq"].clone(), o["status"].as_str().unwrap().to_owned()))
        .collect::<Vec<_>>();
    assert_eq!(listed(1), [(o1.clone(), json!(null), json!(null), "completed".to_owned()), (p1.clone(), json!(null), json!(null), "completed".to_owned())]);
    {
        // Only the upgrades backfill.
        let mut db = rusqlite::Connection::open(&db_path).unwrap();
        let tx = db.transaction().unwrap();
        tx.execute("INSERT INTO review_log(seq,kind,principal,authority,recorded_unix_ms) VALUES(13,'started','operator:cli','review_capture.v1',1)", []).unwrap();
        let forged = tx.execute("INSERT INTO review_session_events(seq,session_id,event,backfilled) VALUES(13,?1,'started',1)", [&sessions[0]]).unwrap_err();
        assert!(forged.to_string().contains("only the 0059 upgrade backfills"), "{forged}");
    }
    {
        let mut db = rusqlite::Connection::open(&db_path).unwrap();
        let tx = db.transaction().unwrap();
        let (creator, created): (String, i64) = tx.query_row("SELECT creator_principal,created_unix_ms FROM review_opportunities WHERE opportunity_id=?1", [&o1], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        let forged = tx.execute("INSERT INTO review_opportunity_log(seq,opportunity_id,event,principal,authority,recorded_unix_ms,backfilled) VALUES(13,?1,'opened',?2,'review_capture.v1',?3,1)",
            rusqlite::params![o1, creator, created]).unwrap_err();
        assert!(forged.to_string().contains("only the 0063 upgrade backfills review opportunities"), "{forged}");
    }

    // After the upgrade every review event, opening and assignment takes the next seq.
    let p2 = open_review(&f, '2', "skeptical", SKEPTICAL, Some(BUDGET));
    assert_eq!(f.cli_args(&["review", "protocols", "bind", &p2, "--prior", &o1]).0["event"]["seq"], json!(14));
    run_review(&f, &p2, '2', "rev-k2", json!([]), json!([evidence('a')]));
    assert_eq!(opportunity_rows()[4..], [opp(13, &p2, "opened", 0), opp(15, &p2, "assigned", 0)]);
    let p2_session: String = raw.query_row("SELECT session_id FROM review_sessions WHERE opportunity_id=?1", [&p2], |r| r.get(0)).unwrap();
    assert_eq!(events()[4..], [row(16, &p2_session, "started", 0), row(17, &p2_session, "completed", 0)]);
    assert_eq!(listed(12).len(), 2, "P2 is not listed before its opening");
    assert_eq!(listed(13)[2], (p2.clone(), json!(13), json!(null), "unassigned".to_owned()));
    assert_eq!(listed(15)[2], (p2.clone(), json!(13), json!(15), "no_session".to_owned()));
    assert_eq!(status(16), ["completed", "in_progress"]);
    assert_eq!(status(17), ["completed", "completed"]);
}

// Delegated code-review authority (contracts-review.md §10, card D8).

const GRANT_NS: &str = "code-review-authority@herdr-projects";
const REVOKE_NS: &str = "code-review-revocation@herdr-projects";
const ACCEPT_NS: &str = "review-acceptance@herdr-projects";
const PROHIBITED: [&str; 5] = ["alter_requirements", "approve_author_attempt", "approve_own_work", "child_delegation", "increase_permissions"];

/// A migrated project with the owner key (`IntegrationLab`) and a reviewer
/// key `carol` generated here; tasks `work` and `other` (contract revision 1
/// each); author attempt `author` (configuration `codex`) with submissions
/// S1 (work, /repo, 1…1), S2 (other, /repo, 2…2) and S3 (work, /elsewhere,
/// 3…3); reviewer attempts of `work` whose dispatch chose `fast`, the
/// retained profile every review here is assigned to.
struct AuthorityWorld { lab: IntegrationLab, carol: std::path::PathBuf, fast: String, author: String }

impl AuthorityWorld {
    fn new(reviewers: &[&str]) -> Self {
        use herdr_projects::{domain::TaskId, runtime};
        let lab = IntegrationLab::new();
        let mut head = runtime::snapshot(&lab.project).unwrap().head;
        for task in ["work", "other"] { head = runtime::add_task(&lab.project, TaskId::new(task).unwrap(), task.into(), head).unwrap(); }
        let config = herdr_projects::migration::config_reference(&lab.home.path().join(".config/herdr-projects/config.toml")).unwrap();
        let author_profile = codex_profile(&config, "codex", "codex", Some(&lab.home.path().join("codex-home")));
        let mut fast_profile = codex_profile(&config, "codex", "fast", Some(&lab.home.path().join("fast-home")));
        fast_profile.arguments_digest = "1".repeat(64);
        let db = lab.db();
        let mut ids = Vec::new();
        for profile in [&author_profile, &fast_profile] {
            let c = agent_configuration(profile);
            db.execute("INSERT OR IGNORE INTO agent_configurations VALUES(?1,?2,1)", rusqlite::params![c.id, c.canonical_json]).unwrap();
            ids.push(c.id);
        }
        let db_path = lab.project.join(".state/state.db");
        plant_profile(&db_path, author_profile);
        plant_profile(&db_path, fast_profile);
        let decision = |attempt: &str, task: &str, configuration: &str| db.execute("INSERT INTO dispatch_decisions(attempt_id,task_id,task_revision,contract_revision,chosen_configuration_id,eligible,chooser_kind,chooser_principal,reason_codes,decided_unix_ms)
            VALUES(?1,?2,1,1,?3,'[\"x\"]','operator','operator:cli','[\"x\"]',1)", rusqlite::params![attempt, task, configuration]).unwrap();
        for task in ["work", "other"] {
            db.execute("INSERT INTO task_contracts(task_id,contract_revision,plan_revision,project_store,expected_head,repository,base_oid,object_format,memory_snapshot_id,route,raw_bytes,raw_digest,installed_seq)
                VALUES(?1,1,NULL,'store',0,'/repo',?2,'sha1',NULL,'verify_only',x'61',?3,(SELECT max(sequence) FROM events))", rusqlite::params![task, "b".repeat(40), hex('c')]).unwrap();
            let attempt = if task == "work" { "author" } else { "author-other" };
            db.execute("INSERT INTO attempts(id,task_id,revision,state,reservation,termination_observed) VALUES(?1,?2,1,'completed',?1,1)", [attempt, task]).unwrap();
            decision(attempt, task, &ids[0]);
        }
        for (sub, task, repository, attempt) in [(hex('1'), "work", "/repo", "author"), (hex('2'), "other", "/repo", "author-other"), (hex('3'), "work", "/elsewhere", "author")] {
            db.execute("INSERT INTO result_submissions(submission_id,project_store,idempotency_key,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,artifact_manifest,claimed_checks,created_unix_ms)
                VALUES(?1,'store',?1,?2,'{}',?3,1,?2,?4,?5,?6,?7,'sha1','[]','[]',1000)",
                rusqlite::params![sub, hex('d'), task, attempt, repository, "b".repeat(40), sub[..40].to_owned()]).unwrap();
        }
        for attempt in reviewers {
            db.execute("INSERT INTO attempts(id,task_id,revision,state,reservation,termination_observed) VALUES(?1,'work',1,'completed',?1,1)", [attempt]).unwrap();
            decision(attempt, "work", &ids[1]);
        }
        let carol = lab.home.path().join("carol");
        assert!(std::process::Command::new("/usr/bin/ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&carol).output().unwrap().status.success());
        let author = ids.remove(0);
        AuthorityWorld { lab, carol, fast: ids.remove(0), author }
    }
    fn public(key: &std::path::Path) -> String {
        fs::read_to_string(key.with_extension("pub")).unwrap().split_whitespace().take(2).collect::<Vec<_>>().join(" ")
    }
    /// A `code_review` grant for `reviewer:carol` over task `work` revision 1,
    /// repository /repo and kind `code`, valid for an hour, with `changes` merged in.
    fn grant(&self, changes: serde_json::Value) -> serde_json::Value {
        let mut grant = json!({"schema": "code_review_authority.v1", "scope": "code_review", "issuer": "owner", "subject": "reviewer:carol",
            "subject_public_key": Self::public(&self.carol), "subject_configurations": [], "project_store": self.lab.store, "repositories": ["/repo"],
            "tasks": [{"task_id": "work", "contract_revision": 1}], "kinds": ["code"], "review_configurations": [], "actions": ["accept_review_completion"],
            "max_decisions": 2, "valid_from_unix_ms": unix_ms() - 60_000, "expires_unix_ms": unix_ms() + 3_600_000, "prohibited_effects": PROHIBITED,
            "authority": herdr_projects::authority::policy_reference(&self.lab.project).unwrap()});
        for (k, v) in changes.as_object().unwrap() { grant[k] = v.clone(); }
        grant
    }
    /// Write `body` as `name` and sign its exact bytes with `key` under `namespace`; returns (document, signature, digest).
    fn sign(&self, key: &std::path::Path, namespace: &str, name: &str, body: &serde_json::Value) -> (String, String, String) {
        let doc = self.lab.home.path().join(name);
        let bytes = serde_json::to_vec_pretty(body).unwrap();
        fs::write(&doc, &bytes).unwrap();
        let _ = fs::remove_file(doc.with_extension("json.sig"));
        assert!(std::process::Command::new("/usr/bin/ssh-keygen").args(["-Y", "sign", "-f"]).arg(key).args(["-n", namespace]).arg(&doc).output().unwrap().status.success());
        (doc.display().to_string(), doc.with_extension("json.sig").display().to_string(), format!("sha256:{:x}", Sha256::digest(&bytes)))
    }
    /// Install a grant signed by the owner; returns its id.
    fn install(&self, name: &str, grant: &serde_json::Value) -> String {
        let (doc, sig, digest) = self.sign(&self.lab.key, GRANT_NS, name, grant);
        let installed = self.lab.telemetry(&["review", "authority", "import", &doc, &sig])["grant"].clone();
        assert_eq!((&installed["grant_id"], &installed["installed"]), (&json!(digest), &json!(true)));
        digest
    }
    /// Open, assign (`fast`), start by `attempt` and complete one review of `submission`; returns (session, receipt digest).
    fn review(&self, submission: char, kind: &str, attempt: &str, outcome: &str, findings: serde_json::Value) -> (String, String) {
        let sub = hex(submission);
        let opportunity = self.lab.telemetry(&["review", "open", &sub, "--kind", kind, "--protocol", "review-protocol.v1"])["opportunity"]["opportunity_id"].as_str().unwrap().to_owned();
        self.lab.telemetry(&["review", "assign", &opportunity, "--reviewer", "fast"]);
        let session = self.lab.telemetry(&["review", "start", &opportunity, "--attempt", attempt])["session"]["session_id"].as_str().unwrap().to_owned();
        let mut receipt = json!({"schema": "review_receipt.v1", "session_id": session, "submission_id": sub, "candidate_oid": sub[..40].to_owned(),
            "outcome": outcome, "findings": findings, "evidence": []});
        if outcome != "completed" { receipt["reason"] = json!("budget_exhausted"); }
        let path = self.lab.home.path().join(format!("{attempt}-receipt.json"));
        fs::write(&path, receipt.to_string()).unwrap();
        let done = self.lab.telemetry(&["review", "complete", "--input-file", path.to_str().unwrap()])["completion"].clone();
        (session, done["receipt_digest"].as_str().unwrap().to_owned())
    }
    /// A decision request on `session` under `grant` by `subject`.
    fn request(&self, grant: &str, session: &(String, String), decision: &str, reason: Option<&str>) -> serde_json::Value {
        let mut request = json!({"schema": "review_acceptance.v1", "grant_id": grant, "subject": "reviewer:carol", "project_store": self.lab.store,
            "session_id": session.0, "receipt_digest": session.1, "decision": decision});
        if let Some(reason) = reason { request["reason"] = json!(reason); }
        request
    }
    /// `review accept` of `request` signed by `key`: Ok(acceptance) or Err(stderr).
    fn accept(&self, key: &std::path::Path, name: &str, request: &serde_json::Value) -> Result<serde_json::Value, String> {
        let (doc, sig, _) = self.sign(key, ACCEPT_NS, name, request);
        let out = self.lab.hp(&["telemetry", "demo", "review", "accept", request["session_id"].as_str().unwrap(), "--document", &doc, "--signature", &sig]);
        if out.status.success() { Ok(serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["acceptance"].clone()) } else { Err(String::from_utf8_lossy(&out.stderr).into_owned()) }
    }
    fn decisions(&self) -> i64 { self.lab.db().query_row("SELECT count(*) FROM review_acceptances", [], |r| r.get(0)).unwrap() }
    fn grants(&self) -> serde_json::Value { self.lab.telemetry(&["review", "authority", "show"]) }
}

/// Owner-signed grant G1 for `reviewer:carol`: task `work` revision 1,
/// repository /repo, kind `code`, at most 2 decisions. Carol accepts O1's
/// completed review (her request signed with her key): one decision, recorded
/// with `delegated_code_review.v1`; the same request replays, a second one
/// (a rejection) is refused. Refused, writing nothing: her request signed by
/// the owner's key; one naming another reviewer; review O2 of task `other`,
/// O3 of kind `security`, O4 of repository /elsewhere (outside scope); O5,
/// which timed out (only a completed review is decided). Grant G2 names
/// `reviewer:rev-a6`, who cannot accept O6, its own review; grant G3 names
/// the author's configuration, so it cannot accept O7, a review of the
/// author's work. G1 then accepts O7 (2 of 2) and is refused O8 at its limit;
/// a raw acceptance row for O2 under G1 aborts in the trigger.
#[test]
fn delegated_reviewer_accepts_within_scope_and_is_refused_outside() {
    let w = AuthorityWorld::new(&["rev-a1", "rev-a2", "rev-a3", "rev-a4", "rev-a5", "rev-a6", "rev-a7", "rev-a8"]);
    let grant = w.grant(json!({}));
    let g1 = w.install("g1.json", &grant);
    let (doc, sig, _) = w.sign(&w.lab.key, GRANT_NS, "g1.json", &grant);
    assert_eq!(w.lab.telemetry(&["review", "authority", "import", &doc, &sig])["grant"]["installed"], json!(false), "the same grant replays");

    let o1 = w.review('1', "code", "rev-a1", "completed", json!(["finding:x"]));
    let request = w.request(&g1, &o1, "accepted", None);
    let accepted = w.accept(&w.carol, "o1.json", &request).unwrap();
    assert_eq!((&accepted["decision"], &accepted["authority_principal"], &accepted["grant_id"], &accepted["authority"], &accepted["receipt_digest"], &accepted["replayed"]),
        (&json!("accepted"), &json!("reviewer:carol"), &json!(g1), &json!("delegated_code_review.v1"), &json!(o1.1), &json!(false)));
    assert_eq!(w.accept(&w.carol, "o1.json", &request).unwrap()["replayed"], json!(true));
    assert!(w.accept(&w.carol, "o1-again.json", &w.request(&g1, &o1, "rejected", Some("insufficient_coverage"))).unwrap_err().contains("already has a decision"));
    let shown = w.lab.telemetry(&["review", "show"]);
    let completion = shown["opportunities"][0]["sessions"][0]["completion"].clone();
    assert_eq!((&completion["trust"], &completion["acceptance"]["decision"], &completion["acceptance"]["grant_id"]), (&json!("proposal"), &json!("accepted"), &json!(g1)));

    // The owner's key cannot stand in for the reviewer's; a request names exactly its grant's reviewer.
    let o7 = w.review('1', "code", "rev-a7", "completed", json!([]));
    assert!(w.accept(&w.lab.key, "o7-owner.json", &w.request(&g1, &o7, "accepted", None)).unwrap_err().contains("signed with the grant subject's key"));
    let mut dave = w.request(&g1, &o7, "accepted", None);
    dave["subject"] = json!("reviewer:dave");
    assert!(w.accept(&w.carol, "o7-dave.json", &dave).unwrap_err().contains("names another reviewer"));

    // Outside the grant's scope: another task, another kind, another repository.
    let o2 = w.review('2', "code", "rev-a2", "completed", json!([]));
    assert!(w.accept(&w.carol, "o2.json", &w.request(&g1, &o2, "accepted", None)).unwrap_err().contains("outside the grant's scope: task contract revision"));
    let o3 = w.review('1', "security", "rev-a3", "completed", json!([]));
    assert!(w.accept(&w.carol, "o3.json", &w.request(&g1, &o3, "accepted", None)).unwrap_err().contains("outside the grant's scope: review kind"));
    let o4 = w.review('3', "code", "rev-a4", "completed", json!([]));
    assert!(w.accept(&w.carol, "o4.json", &w.request(&g1, &o4, "accepted", None)).unwrap_err().contains("outside the grant's scope: repository"));
    let o5 = w.review('1', "code", "rev-a5", "timed_out", json!([]));
    assert!(w.accept(&w.carol, "o5.json", &w.request(&g1, &o5, "accepted", None)).unwrap_err().contains("only a completed review is accepted or rejected"));

    // Never its own review, never the author attempt's work.
    let g2 = w.install("g2.json", &w.grant(json!({"subject": "reviewer:rev-a6"})));
    let o6 = w.review('1', "code", "rev-a6", "completed", json!([]));
    let mut own = w.request(&g2, &o6, "accepted", None);
    own["subject"] = json!("reviewer:rev-a6");
    assert!(w.accept(&w.carol, "o6.json", &own).unwrap_err().contains("cannot accept its own review"));
    let g3 = w.install("g3.json", &w.grant(json!({"subject": "reviewer:erin", "subject_configurations": [w.author]})));
    let mut erin = w.request(&g3, &o7, "accepted", None);
    erin["subject"] = json!("reviewer:erin");
    assert!(w.accept(&w.carol, "o7-erin.json", &erin).unwrap_err().contains("work by the author attempt"));
    assert_eq!(w.decisions(), 1, "refused decisions write nothing");

    // The limit: G1's second decision is its last.
    assert_eq!(w.accept(&w.carol, "o7.json", &w.request(&g1, &o7, "accepted", None)).unwrap()["decision"], json!("accepted"));
    let o8 = w.review('1', "code", "rev-a8", "completed", json!([]));
    assert!(w.accept(&w.carol, "o8.json", &w.request(&g1, &o8, "rejected", Some("evidence_missing"))).unwrap_err().contains("decision limit (2) is reached"));
    let g = w.grants()["grants"].as_array().unwrap().iter().find(|g| g["grant_id"] == g1.as_str()).unwrap().clone();
    assert_eq!((&g["status"], &g["decisions"], &g["max_decisions"], &g["prohibited_effects"]), (&json!("exhausted"), &json!(2), &json!(2), &json!(PROHIBITED)));

    // Raw SQL: a row for an out-of-scope session under a real grant still aborts.
    let raw = w.lab.db().execute("INSERT INTO review_acceptances(session_id,decision,authority_principal,authority_ref,authority,receipt_digest,request_digest,request_bytes,request_signature,decided_unix_ms)
        VALUES(?1,'accepted','reviewer:carol',?2,'delegated_code_review.v1',?3,?4,x'61',x'61',?5)", rusqlite::params![o2.0, g1, o2.1, format!("sha256:{}", hex('7')), unix_ms()]).unwrap_err();
    assert!(raw.to_string().contains("needs a valid, unexpired, unrevoked code_review grant"), "{raw}");
    assert_eq!(w.decisions(), 2);
}

/// Minting: a grant signed by the reviewer's own key, one signed under the
/// delegation namespace, one whose reviewer key is the owner's, one naming a
/// worker, one adding triage, one leaving out a prohibited effect, and a
/// signed grant whose expiry was extended afterwards are all refused, as is
/// any import or decision run inside a worker execution context (HOME = the
/// retained `fast` profile's execution home): no grant is installed. Then G
/// (4 decisions) accepts O1; the reviewer cannot revoke it, the owner does
/// (replayed once); O2 is refused as revoked, a raw row too, and O1's
/// decision stays. G_future (valid from an hour ahead) refuses O3. G_short
/// (valid 6 s) accepts O3, then refuses O4 once expired; an expired grant no
/// longer imports.
#[test]
fn revoked_or_expired_grant_cannot_accept_and_workers_cannot_mint() {
    let w = AuthorityWorld::new(&["rev-a1", "rev-a2", "rev-a3", "rev-a4"]);
    let refused = |name: &str, key: &std::path::Path, namespace: &str, body: &serde_json::Value| {
        let (doc, sig, _) = w.sign(key, namespace, name, body);
        w.lab.fail(&["telemetry", "demo", "review", "authority", "import", &doc, &sig])
    };
    let grant = w.grant(json!({"max_decisions": 4}));
    assert!(refused("self.json", &w.carol, GRANT_NS, &grant).contains("signature verification failed"));
    assert!(refused("delegation.json", &w.lab.key, "delegation@herdr-projects", &grant).contains("signature verification failed"));
    let owner_key = AuthorityWorld::public(&w.lab.key);
    assert!(refused("owner-subject.json", &w.lab.key, GRANT_NS, &w.grant(json!({"subject_public_key": owner_key}))).contains("self-signature is forbidden"));
    assert!(refused("worker.json", &w.lab.key, GRANT_NS, &w.grant(json!({"subject": "worker:rev-a1"}))).contains("never a worker"));
    assert!(refused("triage.json", &w.lab.key, GRANT_NS, &w.grant(json!({"actions": ["accept_review_completion", "triage_findings"]}))).contains("permits only"));
    let mut fewer = PROHIBITED.to_vec();
    fewer.retain(|e| *e != "increase_permissions");
    assert!(refused("fewer.json", &w.lab.key, GRANT_NS, &w.grant(json!({"prohibited_effects": fewer}))).contains("prohibited_effects must list"));
    let (doc, sig, _) = w.sign(&w.lab.key, GRANT_NS, "extended.json", &grant);
    let extended = fs::read_to_string(&doc).unwrap().replace(&grant["expires_unix_ms"].to_string(), &(grant["expires_unix_ms"].as_i64().unwrap() + 86_400_000).to_string());
    fs::write(&doc, extended).unwrap();
    assert!(w.lab.fail(&["telemetry", "demo", "review", "authority", "import", &doc, &sig]).contains("signature verification failed"));
    // Inside a worker execution context the review CLI refuses before reading anything.
    let worker_home = w.lab.home.path().join("fast-home");
    fs::create_dir_all(&worker_home).unwrap();
    let (doc, sig, _) = w.sign(&w.lab.key, GRANT_NS, "g.json", &grant);
    let in_worker = |args: &[&str]| std::process::Command::new(BIN).env_clear().env("HOME", &worker_home).env("PATH", "/usr/bin:/bin")
        .args(["--root", w.lab.root.to_str().unwrap(), "telemetry", "demo", "review"]).args(args).output().unwrap();
    let out = in_worker(&["authority", "import", &doc, &sig]);
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("worker execution context"));
    assert_eq!(w.grants()["grants"], json!([]), "nothing minted a grant");

    // Revocation stops later decisions and keeps earlier ones.
    let g = w.install("g.json", &grant);
    let o1 = w.review('1', "code", "rev-a1", "completed", json!([]));
    let (request_doc, request_sig, _) = w.sign(&w.carol, ACCEPT_NS, "o1.json", &w.request(&g, &o1, "accepted", None));
    let out = in_worker(&["accept", &o1.0, "--document", &request_doc, "--signature", &request_sig]);
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("worker execution context"));
    assert_eq!(w.accept(&w.carol, "o1.json", &w.request(&g, &o1, "accepted", None)).unwrap()["decision"], json!("accepted"));
    let revocation = json!({"schema": "code_review_revocation.v1", "grant_id": g, "project_store": w.lab.store, "reason": "reviewer_retired",
        "authority": herdr_projects::authority::policy_reference(&w.lab.project).unwrap()});
    let (doc, sig, _) = w.sign(&w.carol, REVOKE_NS, "revoke.json", &revocation);
    assert!(w.lab.fail(&["telemetry", "demo", "review", "authority", "revoke", &doc, &sig]).contains("signature verification failed"));
    let (doc, sig, _) = w.sign(&w.lab.key, REVOKE_NS, "revoke.json", &revocation);
    let revoked = w.lab.telemetry(&["review", "authority", "revoke", &doc, &sig])["revocation"].clone();
    assert_eq!((&revoked["grant_id"], &revoked["reason"], &revoked["replayed"], &revoked["undoes_earlier_decisions"]), (&json!(g), &json!("reviewer_retired"), &json!(false), &json!(false)));
    assert_eq!(w.lab.telemetry(&["review", "authority", "revoke", &doc, &sig])["revocation"]["replayed"], json!(true));
    let o2 = w.review('1', "code", "rev-a2", "completed", json!([]));
    assert!(w.accept(&w.carol, "o2.json", &w.request(&g, &o2, "accepted", None)).unwrap_err().contains("grant is revoked"));
    let raw = w.lab.db().execute("INSERT INTO review_acceptances(session_id,decision,authority_principal,authority_ref,authority,receipt_digest,request_digest,request_bytes,request_signature,decided_unix_ms)
        VALUES(?1,'accepted','reviewer:carol',?2,'delegated_code_review.v1',?3,?4,x'61',x'61',?5)", rusqlite::params![o2.0, g, o2.1, format!("sha256:{}", hex('7')), unix_ms()]).unwrap_err();
    assert!(raw.to_string().contains("needs a valid, unexpired, unrevoked code_review grant"), "{raw}");
    let shown = w.grants();
    assert_eq!((&shown["grants"][0]["status"], &shown["grants"][0]["decisions"], &shown["grants"][0]["revocation"]["reason"]), (&json!("revoked"), &json!(1), &json!("reviewer_retired")));
    assert_eq!(shown["decisions"].as_array().unwrap().iter().map(|d| d["session_id"].clone()).collect::<Vec<_>>(), [json!(o1.0)], "the earlier decision stays");

    // Validity: not before valid_from, not at or after expiry.
    let future = w.install("future.json", &w.grant(json!({"valid_from_unix_ms": unix_ms() + 3_600_000, "expires_unix_ms": unix_ms() + 7_200_000})));
    let o3 = w.review('1', "code", "rev-a3", "completed", json!([]));
    assert!(w.accept(&w.carol, "o3-future.json", &w.request(&future, &o3, "accepted", None)).unwrap_err().contains("not valid yet"));
    let expires = unix_ms() + 6_000;
    let short = w.install("short.json", &w.grant(json!({"expires_unix_ms": expires})));
    assert_eq!(w.accept(&w.carol, "o3.json", &w.request(&short, &o3, "accepted", None)).unwrap()["decision"], json!("accepted"));
    let o4 = w.review('1', "code", "rev-a4", "completed", json!([]));
    while unix_ms() <= expires { std::thread::sleep(std::time::Duration::from_millis(100)); }
    assert!(w.accept(&w.carol, "o4.json", &w.request(&short, &o4, "accepted", None)).unwrap_err().contains("grant is expired"));
    let status = |id: &str| w.grants()["grants"].as_array().unwrap().iter().find(|g| g["grant_id"] == id).unwrap()["status"].clone();
    assert_eq!((status(&short), status(&future)), (json!("expired"), json!("not_yet_valid")));
    assert!(refused("stale.json", &w.lab.key, GRANT_NS, &w.grant(json!({"expires_unix_ms": expires}))).contains("grant is expired"));
    assert_eq!(w.decisions(), 2);
}

/// One Codex rollout for reviewer `attempt`: `input` new input and `output`
/// output tokens of gpt-5.5 in its task worktree, under its execution home,
/// with the attempt inputs and collector binding its launch would record.
fn reviewer_usage(w: &AuthorityWorld, attempt: &str, n: u32, input: u64, output: u64) {
    let home = w.lab.home.path().join(format!("{attempt}-home"));
    let db = w.lab.db();
    let operation = format!("op-{attempt}");
    db.execute("INSERT INTO operations(id,task_id,kind,target,payload_version,payload,payload_hash,expected_revision,due_unix_ms,idempotency_key) VALUES(?1,'work','runtime.launch','binding',1,'{}',?2,1,0,?1)",
        rusqlite::params![operation, format!("{:x}", Sha256::digest(b"{}"))]).unwrap();
    let inputs = json!({"inputs": {"version": 2, "effective_profile": {"kind": "codex", "execution_home": home.display().to_string()}}}).to_string();
    db.execute("INSERT INTO attempt_inputs(attempt_id,operation_id,payload,payload_hash) VALUES(?1,?2,?3,?4)", rusqlite::params![attempt, operation, inputs, format!("{:x}", Sha256::digest(inputs.as_bytes()))]).unwrap();
    db.execute("INSERT INTO collector_bindings(attempt_id,revision,state,collector,execution_home,unix_ms,source) VALUES(?1,1,'active','codex',?2,1,'apply_launch_started')",
        rusqlite::params![attempt, home.display().to_string()]).unwrap();
    let dir = home.join(".codex/sessions/2026/09/28");
    fs::create_dir_all(&dir).unwrap();
    let cwd = format!("{}/.state/worktrees/{attempt}/repo-00", w.lab.project.canonicalize().unwrap().display());
    let ts = jiff::Timestamp::from_millisecond(unix_ms()).unwrap().to_string();
    let id = format!("00000000-0000-4000-8000-0000000d8c0{n}");
    let lines = [json!({"timestamp": ts, "type": "session_meta", "payload": {"id": id, "timestamp": ts, "cwd": cwd, "originator": "codex_exec", "cli_version": "0.154.0", "source": "exec"}}),
        json!({"timestamp": ts, "type": "turn_context", "payload": {"turn_id": "turn-1", "model": "gpt-5.5", "effort": "high"}}),
        json!({"timestamp": ts, "type": "token_usage_record", "payload": {"turn_id": "turn-1", "response_id": "resp-1", "usage": {"input_tokens": input, "cached_input_tokens": 0,
            "cache_write_input_tokens": 0, "output_tokens": output, "reasoning_output_tokens": 0, "total_tokens": input + output}}})];
    fs::write(dir.join(format!("rollout-2026-09-28T00-00-00-{attempt}.jsonl")), lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();
}

/// Review cost (M24) over closed opportunities, with the synthetic card
/// (input 2, output 4 per 10^6 tokens, fixture only). O1 (rev-c1, 1000 in +
/// 500 out = 0.004) completes with finding a, accepted; O2 (rev-c2, 2000 +
/// 1000 = 0.008) completes with none, accepted; O3 (rev-c3, 500 + 250 =
/// 0.002) times out: closed, unsuccessful; O4 (rev-c4, 0.004) completes with
/// finding b, rejected (insufficient_coverage); O5 (rev-c5, 0.1) completes
/// with finding c, undecided: not closed, outside Q. The owner validates a
/// and b; c stays pending. Q = O1–O4, cost 0.004 + 0.008 + 0.002 + 0.004 =
/// 0.018 USD; validated unique findings discovered in Q's accepted reviews =
/// 1 (a; b's review was rejected: `excluded_rejected_review` 1). M24 = 1 /
/// 0.018 = 500/9 findings per USD, complete. O6 (rev-c6, no usage bound)
/// completes empty and is accepted: its session is `no_usage_bound`, the cost
/// stays 0.018 and M24 becomes partial 500/9. Drill-downs: M22's window
/// submissions by decision (a accepted, b rejected, c undecided); M21's F (a,
/// b; credit "2", unchanged) accepted 1, rejected 1.
#[test]
fn review_cost_and_acceptance_metrics() {
    let w = AuthorityWorld::new(&["rev-c1", "rev-c2", "rev-c3", "rev-c4", "rev-c5", "rev-c6"]);
    for (n, (attempt, input, output)) in [("rev-c1", 1000, 500), ("rev-c2", 2000, 1000), ("rev-c3", 500, 250), ("rev-c4", 1000, 500), ("rev-c5", 25_000, 12_500)].into_iter().enumerate() {
        reviewer_usage(&w, attempt, n as u32 + 1, input, output);
    }
    w.lab.telemetry(&["collect"]);
    w.lab.telemetry(&["accounting", "sync"]);
    let card = w.lab.home.path().join("rates-v1.json");
    fs::write(&card, fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/telemetry/accounting/rates-v1.json")).unwrap().replace("@BOUNDARY@", "4102444800000")).unwrap();
    w.lab.telemetry(&["accounting", "import-rate-card", card.to_str().unwrap()]);
    assert_eq!(w.lab.telemetry(&["accounting", "reprice"])["revision"], json!(1));

    let g = w.install("g.json", &w.grant(json!({"max_decisions": 8, "review_configurations": [w.fast]})));
    let o1 = w.review('1', "code", "rev-c1", "completed", json!(["finding:a"]));
    let o2 = w.review('1', "code", "rev-c2", "completed", json!([]));
    w.review('1', "code", "rev-c3", "timed_out", json!([]));
    let o4 = w.review('1', "code", "rev-c4", "completed", json!(["finding:b"]));
    w.review('1', "code", "rev-c5", "completed", json!(["finding:c"]));
    for (session, name, decision, reason) in [(&o1, "o1.json", "accepted", None), (&o2, "o2.json", "accepted", None), (&o4, "o4.json", "rejected", Some("insufficient_coverage"))] {
        assert_eq!(w.accept(&w.carol, name, &w.request(&g, session, decision, reason)).unwrap()["decision"], json!(decision));
    }
    for claim in ["1", "2"] { w.lab.telemetry(&["review", "findings", "validate", claim, "--new", "--severity", "high", "--evidence", &evidence('e')]); }

    let metrics = w.lab.telemetry(&["review", "report"])["metrics"].clone();
    let m24 = &metrics["M24"];
    assert_eq!((&m24["value"], &m24["unit"], &m24["numerator"], &m24["cost"]), (&json!("500/9"), &json!("findings/USD"), &json!(1),
        &json!({"status": "complete", "currency": "USD", "amount": "0.018"})));
    assert_eq!(m24["opportunities"], json!({"closed": 4, "accepted": 2, "rejected": 1, "unsuccessful": 1, "awaiting_acceptance": 1, "awaiting_adjudication": 0, "open": 0}));
    assert_eq!(m24["sessions"], json!({"total": 4, "priced": 4, "partial": 0, "unavailable": {}}));
    assert_eq!((&m24["excluded_rejected_review"], &m24["rate_cards"], &m24["acceptance_authority"], &m24["basis"]),
        (&json!(1), &json!("fixture_only"), &json!("delegated_code_review.v1"), &json!("published_rate_estimate")));
    let drill = json!({"accepted": 1, "rejected": 1, "undecided": 1, "authority": "delegated_code_review.v1"});
    assert_eq!((&metrics["M22"]["review_acceptance"], &metrics["M23"]["review_acceptance"]), (&drill, &drill));
    assert_eq!((&metrics["M21"]["value"], &metrics["M21"]["review_acceptance"]), (&json!("2"), &json!({"accepted": 1, "rejected": 1, "undecided": 0, "authority": "delegated_code_review.v1"})));

    // A reviewer without bound usage: its cost is unknown, never 0, so M24 is partial.
    let o6 = w.review('1', "code", "rev-c6", "completed", json!([]));
    w.accept(&w.carol, "o6.json", &w.request(&g, &o6, "accepted", None)).unwrap();
    let report = w.lab.telemetry(&["report", "--json"]);
    let m24 = &report["metrics"]["M24"];
    assert_eq!((&m24["value"], &m24["cost"]), (&json!({"status": "partial", "value": "500/9", "reasons": ["review_cost_partial"]}),
        &json!({"status": "partial", "currency": "USD", "amount": "0.018"})));
    assert_eq!(m24["sessions"], json!({"total": 5, "priced": 4, "partial": 0, "unavailable": {"no_usage_bound": 1}}));
}

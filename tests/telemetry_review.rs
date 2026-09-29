//! Lane D review capture end to end (contracts-review.md): opportunities bound
//! to exact candidates, blind assignment, sessions and completions through
//! `herdr-projects telemetry <slug> review ...`, over a real reserved attempt
//! with planted submissions and reviewer attempts.

#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.

mod support;

use herdr_projects::{domain::agent_configuration, store::SqliteStore};
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

    // No opportunity yet: an empty denominator is null, not 0; accepted-quality metrics are inactive.
    let empty = f.cli_args(&["review", "report"]).0["metrics"].clone();
    assert_eq!((&empty["M20"]["value"], &empty["M20"]["reason"], &empty["M20"]["denominator"]), (&json!(null), &json!("empty_denominator"), &json!(0)));
    for id in ["M21", "M22", "M23", "M24"] { assert_eq!(empty[id]["value"], unavailable("no_reviewer_authority_producer"), "{id}"); }

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
    assert_eq!(report["metrics"]["M22"]["value"], unavailable("no_reviewer_authority_producer"));
    let o2 = f.cli_args(&["review", "show"]).0["opportunities"].as_array().unwrap().iter().find(|o| o["opportunity_id"] == o2_id.as_str()).unwrap().clone();
    assert_eq!((&o2["findings_submitted"], &o2["sessions"][0]["completion"]["findings_submitted"]), (&unavailable("ended_without_completion"), &json!(1)));

    // Nothing of the refused content reached the store.
    for file in ["state.db", "state.db-wal"] {
        let bytes = fs::read(f.project.join(".state").join(file)).unwrap_or_default();
        assert!(!bytes.windows(20).any(|w| w == b"SENTINEL-REVIEW-TEXT"), "{file}");
    }
}

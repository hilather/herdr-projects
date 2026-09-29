//! Lane B accounting end to end (docs/telemetry/contracts-accounting.md): Codex
//! rollouts collected on the CLI, the usage ledger synced by
//! `telemetry <slug> accounting sync`, the metrics it provides, and
//! published-rate estimates from synthetic rate cards, and quota windows from
//! synthetic Codex rate-limit snapshots.

#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.

mod support;

use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::{fs, process::Command, time::Duration};
use support::telemetry::*;

const RECORD: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/telemetry/accounting/record.jsonl");
const LOWER: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/telemetry/accounting/lower-cumulative.jsonl");
const MODEL_A: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/telemetry/accounting/model-a.jsonl");
const MODEL_B: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/telemetry/accounting/model-b.jsonl");
const GUARDIAN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/telemetry/accounting/guardian.jsonl");
const ACCOUNTING: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/telemetry/accounting");
/// Session ids written literally in the rate-card rollouts.
const BEFORE: &str = "00000000-0000-4000-8000-0000000b3001";
const AFTER: &str = "00000000-0000-4000-8000-0000000b3002";
const STRADDLE: &str = "00000000-0000-4000-8000-0000000b3003";
const CACHE: &str = "00000000-0000-4000-8000-0000000b3004";
/// The valuation policy of revisions appended since A4 metadata is consumed.
const POLICY: &str = "usage_interval=record_time|session_start..first_observed;split=none;provider=checked_when_reported";
/// Session id written literally in `guardian.jsonl`.
const GUARDIAN_SID: &str = "00000000-0000-4000-8000-0000000c0de9";

fn digest(path: &Path) -> String { format!("sha256:{:x}", Sha256::digest(path.as_os_str().as_encoded_bytes())) }

/// Doc 05 §7 "input-inclusive 1,000 contains cache-read 200; output-inclusive
/// 300 contains reasoning 80 → total 1,300; new input 800; reasoning is not
/// added again", observed in two rollouts of one session (§2: one effective
/// invocation, two provenance observations), the second resuming with a
/// thread total that fell without reset evidence (§3: not a reset).
#[test]
fn normalized_totals_match_doc05_golden() {
    let f = Fixture::new();
    let at = f.decided + 1_000;
    let a = f.rollout(&f.home, "a", &[RECORD], &f.worktree(), at, "0.154.0");
    let b = f.rollout(&f.home, "b", &[RECORD, LOWER], &f.worktree(), at, "0.154.0");
    let (a, b) = (digest(&a), digest(&b));
    f.cli("collect");
    assert_eq!(f.cli_args(&["accounting", "entries"]).0, json!({"status": "unavailable", "reason": "ledger_not_synced"}));

    let (synced, _) = f.cli_args(&["accounting", "sync"]);
    assert_eq!(synced, json!({"entries": 4, "dispositions": {"accepted": 3, "duplicate": 1, "unresolved": 1}, "sessions": 1, "model_segments": 1, "quota_windows": 0}));
    let (entries, first) = f.cli_args(&["accounting", "entries"]);
    let entry = |id: String| entries["entries"].as_array().unwrap().iter().find(|e| e["entry_id"] == id.as_str()).cloned()
        .unwrap_or_else(|| panic!("{id} in {entries}"));

    let record = entry(format!("codex:{SID}:1"));
    assert_eq!((&record["basis"], &record["scope"], &record["precedence"], &record["normalization_version"]),
        (&"delta".into(), &"request".into(), &1.into(), &"codex-v1".into()));
    assert_eq!(record["native"], json!({"cache_write_input_tokens": 0, "cached_input_tokens": 200, "input_tokens": 1000,
        "output_tokens": 300, "reasoning_output_tokens": 80, "total_tokens": 1300}));
    assert_eq!(record["normalized"], json!({"input_tokens": 1000, "cache_read_tokens": 200, "new_input_tokens": 800, "cache_write_tokens": 0,
        "output_tokens": 300, "reasoning_tokens": 80, "total_tokens": 1300}));
    let mut provenance = record["provenance"].as_array().unwrap().clone();
    provenance.sort_by_key(|p| p["path_digest"] != a.as_str());
    assert_eq!(provenance, [json!({"path_digest": a, "disposition": "accepted", "reason": null}),
        json!({"path_digest": b, "disposition": "duplicate", "reason": null})], "counted once, observed twice");

    // 100 input (0 cached) + 20 output: total 120, new input 100.
    let second = entry(format!("codex:{SID}:2"));
    assert_eq!((&second["normalized"]["total_tokens"], &second["normalized"]["new_input_tokens"]), (&120.into(), &100.into()));
    assert_eq!(second["provenance"], json!([{"path_digest": b, "disposition": "accepted", "reason": null}]));

    // Cumulative thread totals are a secondary basis: 1300 at position 1, then 1000 at position 2.
    let thread_a = entry(format!("codex:{SID}:thread:{a}"));
    assert_eq!((&thread_a["basis"], &thread_a["scope"], &thread_a["precedence"], &thread_a["position"]),
        (&"cumulative".into(), &"thread".into(), &2.into(), &1.into()));
    assert_eq!(thread_a["normalized"]["total_tokens"], 1300);
    assert_eq!(thread_a["provenance"], json!([{"path_digest": a, "disposition": "accepted", "reason": null}]));
    let thread_b = entry(format!("codex:{SID}:thread:{b}"));
    assert_eq!((&thread_b["position"], &thread_b["normalized"]["total_tokens"]), (&2.into(), &1000.into()));
    assert_eq!(thread_b["provenance"], json!([{"path_digest": b, "disposition": "unresolved", "reason": "regression_without_reset"}]));

    // M08/M09 come from accepted delta entries only: 1000 + 100 input, 300 + 20 output, reasoning 80 a subset.
    let report = f.report();
    let coverage = json!({"certified_sessions": 1, "excluded": {}});
    assert_eq!(report["metrics"]["M08"], json!({"definition": "M08.slice-v1", "name": "input_tokens", "value": 1100, "coverage": coverage}));
    assert_eq!(report["metrics"]["M09"], json!({"definition": "M09.slice-v1", "name": "output_tokens", "value": 320,
        "reasoning_output_tokens": 80, "coverage": coverage}));

    // Replay: a second collect and sync leave the ledger byte-identical.
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(f.cli_args(&["accounting", "entries"]).1, first);
    assert_eq!((f.count("usage_entries"), f.count("usage_dispositions")), (4, 5));
}

/// Doc 05 §4 / TM2.2: a session switching models from gpt-5.5 (50) to
/// gpt-5.5-mini (150) is one session of 200 in two segments. Its first rollout
/// (50), resumed by a second that observes the same record again, is covered
/// by that inclusive parent: the root stays 200, not 250. A guardian
/// (live shape: `guardian_review`, `codex-auto-review`) session of 50 names a
/// parent thread that was not collected, so it is reported apart as an
/// unlinked child (`parent_not_collected`) and never added to the root; its
/// record before any `turn_context` (10) is unallocated and its turn spanning
/// two models (5 + 5) is mixed.
#[test]
fn model_switch_splits_segments_not_task() {
    let f = Fixture::new();
    let at = f.decided + 1_000;
    let a = digest(&f.rollout(&f.home, "a", &[MODEL_A], &f.worktree(), at, "0.154.0"));
    let b = digest(&f.rollout(&f.home, "b", &[MODEL_A, MODEL_B], &f.worktree(), at, "0.154.0"));
    let g = digest(&f.rollout(&f.home, "g", &[GUARDIAN], &f.worktree(), at, "0.154.0"));
    f.cli("collect");
    assert_eq!(f.cli_args(&["accounting", "sessions"]).0, json!({"status": "unavailable", "reason": "ledger_not_synced"}));
    let (synced, _) = f.cli_args(&["accounting", "sync"]);
    assert_eq!(synced, json!({"entries": 8, "dispositions": {"accepted": 8, "duplicate": 1}, "sessions": 2, "model_segments": 5, "quota_windows": 0}));

    let (sessions, first) = f.cli_args(&["accounting", "sessions"]);
    let segment = |n: i64, model: &str, at: i64, [input, output, reasoning, total]: [i64; 4]| json!({"segment": n, "model": model,
        "first_position": at, "last_position": at, "entries": 1, "input_tokens": input, "output_tokens": output, "reasoning_tokens": reasoning, "total_tokens": total});
    let bucket = |first: i64, last: i64, entries: i64, [input, output, reasoning, total]: [i64; 4]| json!({"first_position": first, "last_position": last,
        "entries": entries, "input_tokens": input, "output_tokens": output, "reasoning_tokens": reasoning, "total_tokens": total});
    let none = json!({"first_position": null, "last_position": null, "entries": 0, "input_tokens": 0, "output_tokens": 0, "reasoning_tokens": 0, "total_tokens": 0});
    let attempt = f.attempt.as_str();
    assert_eq!(sessions, json!({
        "sessions": [
            {"session_id": SID, "role": "primary", "linkage": "root", "parent": null, "total_tokens": 200,
             "rollouts": [
                {"path_digest": b, "linkage": "root", "parent": null, "evidence": null, "inclusive_total": 200, "attempt_id": attempt},
                {"path_digest": a, "linkage": "included", "parent": b, "evidence": "same_session_prefix", "inclusive_total": 50, "attempt_id": attempt}],
             "segments": [segment(1, "gpt-5.5", 1, [40, 10, 4, 50]), segment(2, "gpt-5.5-mini", 2, [120, 30, 6, 150])],
             "mixed": none, "unallocated": none},
            {"session_id": GUARDIAN_SID, "role": "guardian", "linkage": "unlinked_child",
             "parent": {"status": "unavailable", "reason": "parent_not_collected", "session_id": PARENT}, "total_tokens": 50,
             "rollouts": [{"path_digest": g, "linkage": "unlinked_child", "parent": null, "evidence": null, "inclusive_total": 50, "attempt_id": attempt}],
             "segments": [segment(1, "codex-auto-review", 2, [25, 5, 1, 30])],
             "mixed": bucket(3, 4, 2, [7, 3, 0, 10]), "unallocated": bucket(1, 1, 1, [8, 2, 0, 10])}],
        "rollup": {"sessions": 200, "linked_children": 0, "unlinked_children": 50, "incomplete_sessions": 0}}));

    // Segments reconcile to the session: 50 + 150 = 200 and 30 + 10 + 10 = 50;
    // the attempt's M08/M09 still count every accepted record once (160 + 40 input, 40 + 10 output).
    let report = f.report();
    assert_eq!((&report["metrics"]["M08"]["value"], &report["metrics"]["M09"]["value"]), (&200.into(), &50.into()));

    // Replay: a second collect and sync leave the graph byte-identical.
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(f.cli_args(&["accounting", "sessions"]).1, first);
    assert_eq!((f.count("session_graph_nodes"), f.count("model_segments")), (3, 5));

    // A third rollout rewriting record 2 of the session quarantines it (contracts §5):
    // inclusion is unknown, so neither the session nor the rollup shows a total.
    f.rollout(&f.home, "c", &[MODEL_A, LOWER], &f.worktree(), at, "0.154.0");
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    let (sessions, _) = f.cli_args(&["accounting", "sessions"]);
    let unknown = json!({"status": "unavailable", "reason": "inclusion_unknown"});
    assert_eq!((&sessions["sessions"][0]["linkage"], &sessions["sessions"][0]["parent"], &sessions["sessions"][0]["total_tokens"]),
        (&"unresolved".into(), &unknown, &unknown));
    let partial = json!({"status": "unavailable", "reason": "incomplete_sessions"});
    assert_eq!(sessions["rollup"], json!({"sessions": partial, "linked_children": partial, "unlinked_children": partial, "incomplete_sessions": 1}));
}

/// Session ids written literally in the child-session rollouts; `ABSENT` is a
/// parent no rollout carries.
const PARENT: &str = "00000000-0000-4000-8000-0000000b7001";
const SPAWNED: &str = "00000000-0000-4000-8000-0000000b7002";
const FORKED: &str = "00000000-0000-4000-8000-0000000b7003";
const ORPHAN: &str = "00000000-0000-4000-8000-0000000b7004";
const ABSENT: &str = "00000000-0000-4000-8000-0000000b7fff";

/// Contracts-collection A4 → B2: a spawned subagent (30) whose
/// `parent_thread_id` names a collected parent (100) is linked under it with
/// that basis, certified `fixture`, and kept in a separate `children`
/// subtotal: the parent stays 100, never 130. A fork (40) of the same parent
/// is linked by `forked_from_id`, but a fork may replay its parent's records,
/// so its inclusion and the children subtotal are `fork_replay_not_certified`,
/// never a sum. A subagent naming a parent that was not collected (20) is
/// `parent_not_collected`; a guardian (50) naming the parent by its thread
/// lineage is linked with that basis, certified `live`, `separate`. Every
/// record still counts once in M08/M09.
#[test]
fn child_sessions_link_to_parent_without_double_count() {
    let f = Fixture::new();
    let at = f.decided + 1_000;
    let part = |name: &str| format!("{ACCOUNTING}/{name}");
    let plant = |name: &str| f.rollout(&f.home, name, &[&part(&format!("{name}.jsonl"))], &f.worktree(), at, "0.154.0");
    plant("parent");
    plant("spawned");
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    let (sessions, _) = f.cli_args(&["accounting", "sessions"]);
    let of = |sessions: &serde_json::Value, sid: &str| sessions["sessions"].as_array().unwrap().iter().find(|s| s["session_id"] == sid).cloned()
        .unwrap_or_else(|| panic!("{sid} in {sessions}"));
    let spawned = json!({"session_id": SPAWNED, "role": "subagent", "link_basis": "parent_thread_id", "certified": "fixture", "total_tokens": 30, "inclusion": "separate"});
    let parent = of(&sessions, PARENT);
    assert_eq!((&parent["role"], &parent["linkage"], &parent["parent"], &parent["total_tokens"]), (&json!("primary"), &json!("root"), &json!(null), &json!(100)));
    assert_eq!(parent["children"], json!({"sessions": [spawned], "total_tokens": 30}));
    let child = of(&sessions, SPAWNED);
    assert_eq!((&child["role"], &child["linkage"], &child["parent"], &child["total_tokens"]), (&json!("subagent"), &json!("linked_child"),
        &json!({"session_id": PARENT, "link_basis": "parent_thread_id", "certified": "fixture"}), &json!(30)));
    assert_eq!(child.get("children"), None);
    assert_eq!(sessions["rollup"], json!({"sessions": 100, "linked_children": 30, "unlinked_children": 0, "incomplete_sessions": 0}), "100 + 30 apart, never 130 in the parent");
    let report = f.report();
    assert_eq!((&report["metrics"]["M08"]["value"], &report["metrics"]["M09"]["value"]), (&json!(80 + 25), &json!(20 + 5)));

    // A fork, a subagent of an uncollected parent and a guardian join.
    plant("forked");
    plant("orphan-child");
    f.rollout(&f.home, "guardian", &[GUARDIAN], &f.worktree(), at, "0.154.0");
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    let (sessions, first) = f.cli_args(&["accounting", "sessions"]);
    let not_certified = json!({"status": "unavailable", "reason": "fork_replay_not_certified"});
    assert_eq!(of(&sessions, PARENT)["total_tokens"], 100);
    assert_eq!(of(&sessions, PARENT)["children"], json!({"sessions": [spawned,
        {"session_id": FORKED, "role": "fork", "link_basis": "forked_from_id", "certified": "fixture", "total_tokens": 40, "inclusion": not_certified},
        {"session_id": GUARDIAN_SID, "role": "guardian", "link_basis": "thread_parent_thread_id", "certified": "live", "total_tokens": 50, "inclusion": "separate"}],
        "total_tokens": not_certified}));
    let fork = of(&sessions, FORKED);
    assert_eq!((&fork["role"], &fork["linkage"], &fork["parent"], &fork["total_tokens"]), (&json!("fork"), &json!("linked_child"),
        &json!({"session_id": PARENT, "link_basis": "forked_from_id", "certified": "fixture"}), &json!(40)));
    let orphan = of(&sessions, ORPHAN);
    assert_eq!((&orphan["role"], &orphan["linkage"], &orphan["parent"], &orphan["total_tokens"]), (&json!("subagent"), &json!("unlinked_child"),
        &json!({"status": "unavailable", "reason": "parent_not_collected", "session_id": ABSENT}), &json!(20)));
    let guardian = of(&sessions, GUARDIAN_SID);
    assert_eq!((&guardian["role"], &guardian["linkage"], &guardian["parent"], &guardian["total_tokens"]), (&json!("guardian"), &json!("linked_child"),
        &json!({"session_id": PARENT, "link_basis": "thread_parent_thread_id", "certified": "live"}), &json!(50)));
    assert_eq!(sessions["rollup"], json!({"sessions": 100, "linked_children": not_certified, "unlinked_children": 20, "incomplete_sessions": 0}));
    // Each record counts once: 80 + 25 + 30 + 15 + 40 input, 20 + 5 + 10 + 5 + 10 output.
    let report = f.report();
    assert_eq!((&report["metrics"]["M08"]["value"], &report["metrics"]["M09"]["value"]), (&json!(190), &json!(50)));
    assert_eq!(report["metrics"]["M08"]["coverage"], json!({"certified_sessions": 5, "excluded": {}}));

    // Replay: a second collect and sync leave the graph byte-identical.
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(f.cli_args(&["accounting", "sessions"]).1, first);
}

/// Session id written literally in `guardian-no-model.jsonl`.
const LINEAGE_ONLY: &str = "00000000-0000-4000-8000-0000000b7005";

/// Contracts-collection A5 → B9 (codex-live-0.154.0-a4.md §5): a live-shape
/// guardian names its parent in `rollout_threads.parent_thread_id` (and its
/// records report the parent's `session_id`, never used as a key). Two
/// guardians of `PARENT`: one of 50 with `codex-auto-review` records and one
/// of 12 whose only guardian evidence is `thread_source = guardian_review`
/// (its `subagent_kind` is `other`). Without the parent collected both are
/// `parent_not_collected` naming it; with it (100) both are linked
/// (`thread_parent_thread_id`, `live`) and `separate`: the parent stays 100,
/// children 50 + 12 = 62, and each record counts once (M08 80 + 40 + 10,
/// M09 20 + 10 + 2). A sidecar from before A5 synced again reads as it did
/// before A5 (no lineage: `no_native_parent_evidence`, the model-less one a
/// plain `subagent`) until the next collect re-reads the lineage.
#[test]
fn guardian_links_to_parent_by_thread_lineage() {
    let f = Fixture::new();
    let at = f.decided + 1_000;
    let part = |name: &str| format!("{ACCOUNTING}/{name}");
    let plant = |name: &str| f.rollout(&f.home, name, &[&part(&format!("{name}.jsonl"))], &f.worktree(), at, "0.154.0");
    plant("guardian");
    plant("guardian-no-model");
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    let head = |sessions: &serde_json::Value, sid: &str| {
        let s = sessions["sessions"].as_array().unwrap().iter().find(|s| s["session_id"] == sid).cloned().unwrap_or_else(|| panic!("{sid} in {sessions}"));
        (s["role"].clone(), s["linkage"].clone(), s["parent"].clone(), s["total_tokens"].clone())
    };
    let (sessions, _) = f.cli_args(&["accounting", "sessions"]);
    let claimed = json!({"status": "unavailable", "reason": "parent_not_collected", "session_id": PARENT});
    assert_eq!(head(&sessions, GUARDIAN_SID), (json!("guardian"), json!("unlinked_child"), claimed.clone(), json!(50)));
    assert_eq!(head(&sessions, LINEAGE_ONLY), (json!("guardian"), json!("unlinked_child"), claimed, json!(12)));
    assert_eq!(sessions["rollup"], json!({"sessions": 0, "linked_children": 0, "unlinked_children": 62, "incomplete_sessions": 0}));

    // The parent is collected: both guardians link to it by thread lineage, apart from its total.
    plant("parent");
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    let (sessions, linked) = f.cli_args(&["accounting", "sessions"]);
    let parent = json!({"session_id": PARENT, "link_basis": "thread_parent_thread_id", "certified": "live"});
    assert_eq!(head(&sessions, PARENT), (json!("primary"), json!("root"), json!(null), json!(100)));
    let child = |sid: &str, total: i64| json!({"session_id": sid, "role": "guardian", "link_basis": "thread_parent_thread_id", "certified": "live",
        "total_tokens": total, "inclusion": "separate"});
    let of_parent = sessions["sessions"].as_array().unwrap().iter().find(|s| s["session_id"] == PARENT).unwrap();
    assert_eq!(of_parent["children"], json!({"sessions": [child(LINEAGE_ONLY, 12), child(GUARDIAN_SID, 50)], "total_tokens": 62}));
    assert_eq!(head(&sessions, GUARDIAN_SID), (json!("guardian"), json!("linked_child"), parent.clone(), json!(50)));
    assert_eq!(head(&sessions, LINEAGE_ONLY), (json!("guardian"), json!("linked_child"), parent, json!(12)));
    assert_eq!(sessions["rollup"], json!({"sessions": 100, "linked_children": 62, "unlinked_children": 0, "incomplete_sessions": 0}), "100 + 62 apart, never 162");
    // Usage stays with each guardian's own rollout, never under the parent id its records report.
    let (entries, _) = f.cli_args(&["accounting", "entries"]);
    let mut ids: Vec<String> = entries["entries"].as_array().unwrap().iter().map(|e| e["entry_id"].as_str().unwrap().to_owned()).collect();
    ids.sort();
    assert_eq!(ids, [format!("codex:{PARENT}:1"), format!("codex:{LINEAGE_ONLY}:1"), format!("codex:{GUARDIAN_SID}:1"),
        format!("codex:{GUARDIAN_SID}:2"), format!("codex:{GUARDIAN_SID}:3"), format!("codex:{GUARDIAN_SID}:4")]);
    let report = f.report();
    assert_eq!((&report["metrics"]["M08"]["value"], &report["metrics"]["M09"]["value"]), (&json!(80 + 40 + 10), &json!(20 + 10 + 2)));
    assert_eq!(report["metrics"]["M08"]["coverage"], json!({"certified_sessions": 3, "excluded": {}}));

    // A sidecar from before A5 (no `rollout_threads`): synced again, it reads as before A5.
    f.sidecar().execute_batch("DROP TABLE rollout_threads; UPDATE telemetry_streams SET version=4 WHERE stream='ingest';").unwrap();
    f.cli_args(&["accounting", "sync"]);
    let (sessions, _) = f.cli_args(&["accounting", "sessions"]);
    let none = json!({"status": "unavailable", "reason": "no_native_parent_evidence"});
    assert_eq!(head(&sessions, GUARDIAN_SID), (json!("guardian"), json!("unlinked_child"), none.clone(), json!(50)));
    assert_eq!(head(&sessions, LINEAGE_ONLY), (json!("subagent"), json!("unlinked_child"), none, json!(12)));
    assert_eq!(sessions["rollup"], json!({"sessions": 100, "linked_children": 0, "unlinked_children": 62, "incomplete_sessions": 0}));
    // The next collect re-reads the lineage: the graph equals the linked one.
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(f.cli_args(&["accounting", "sessions"]).1, linked);
}

/// Rate card fixture `name` (invented synthetic rates) with its rate boundary
/// substituted, written beside the project; returns its path.
fn rate_card(f: &Fixture, name: &str, boundary: i64) -> String {
    let path = f.tmp.path().join(name);
    fs::write(&path, fs::read_to_string(Path::new(ACCOUNTING).join(name)).unwrap().replace("@BOUNDARY@", &boundary.to_string())).unwrap();
    path.display().to_string()
}

/// Doc 05 §5 / TM2.3: usage is priced by the card version effective over its
/// usage interval (session start to first observation), never today's. Card
/// version 1 (before the boundary: input $2/M, output $4/M, no cache-read
/// rate) prices doc 10's golden 1,000 input + 500 output at exactly $0.004;
/// version 2 (from the boundary: input $2/M, cache read $0.50/M, output $8/M)
/// prices doc 05's 1,000 input incl. 200 cached + 300 output at $0.0041
/// ($0.0016 + $0.0001 + $0.0024). A model without a card, a cache read without
/// a rate, a cache write (codex-v1 does not certify its overlap) and a session
/// straddling the boundary are unavailable with a reason, never 0, and leave
/// the attempt partial. A corrected version 3 (output $6/M) and a EUR card
/// append revision 2 while revision 1 stays byte-identical; currencies are
/// never added; measured tokens never change.
#[test]
fn repricing_uses_rate_effective_at_usage_time() {
    let f = Fixture::new();
    let part = |name: &str| format!("{ACCOUNTING}/{name}");
    // Before the boundary: sessions starting at the decision, first observed by this collect.
    f.rollout(&f.home, "before", &[&part("priced-before.jsonl")], &f.worktree(), f.decided, "0.154.0");
    f.rollout(&f.home, "cache", &[&part("cache-read-unrated.jsonl")], &f.worktree(), f.decided, "0.154.0");
    f.cli("collect");
    let boundary = unix_ms() + 1;
    while unix_ms() <= boundary { std::thread::sleep(Duration::from_millis(1)); }
    // From the boundary; and a session started before it but first observed after it.
    f.rollout(&f.home, "after", &[&part("priced-after.jsonl")], &f.worktree(), boundary, "0.154.0");
    f.rollout(&f.home, "straddle", &[&part("straddles-boundary.jsonl")], &f.worktree(), f.decided, "0.154.0");
    f.cli("collect");

    let unavailable = |reason: &str| json!({"status": "unavailable", "reason": reason});
    assert_eq!(f.cli_args(&["accounting", "reprice"]).0, unavailable("ledger_not_synced"));
    f.cli_args(&["accounting", "sync"]);
    let measured = f.cli_args(&["accounting", "entries"]).1;
    assert_eq!(f.cli_args(&["accounting", "cost", "--json"]).0, unavailable("not_priced"));

    let (v1, v2) = (rate_card(&f, "rates-v1.json", boundary), rate_card(&f, "rates-v2.toml", boundary));
    for (file, version) in [(&v1, 1), (&v2, 2)] {
        let (imported, _) = f.cli_args(&["accounting", "import-rate-card", file]);
        assert_eq!((&imported["card_id"], &imported["version"], &imported["imported"]), (&"synthetic-codex".into(), &version.into(), &true.into()));
    }
    assert_eq!(f.cli_args(&["accounting", "import-rate-card", &v1]).0["imported"], false, "the same version again is a no-op");
    fs::write(&v1, fs::read_to_string(&v1).unwrap().replace("\"4.00\"", "\"5\"")).unwrap();
    assert!(f.cli_fail(&["accounting", "import-rate-card", &v1]).contains("rate cards are append-only"));
    let (cards, _) = f.cli_args(&["accounting", "rate-cards"]);
    let rate = |category: &str, rate: &str| json!({"category": category, "cache_tier": "", "rate": rate});
    let cards = cards["rate_cards"].as_array().unwrap();
    assert_eq!(cards.iter().map(|c| (c["version"].clone(), c["effective_from_unix_ms"].clone(), c["effective_to_unix_ms"].clone(), c["rates"].clone())).collect::<Vec<_>>(),
        [(json!(1), json!(0), json!(boundary), json!([rate("input", "2"), rate("output", "4")])),
         (json!(2), json!(boundary), json!(null), json!([rate("cache_read", "0.5"), rate("input", "2"), rate("output", "8")]))],
        "exact decimals, canonical; the refused edit left version 1 as imported");
    assert_eq!((&cards[0]["currency"], &cards[0]["rate_unit"], &cards[0]["models"], &cards[0]["includes"]),
        (&"USD".into(), &1_000_000.into(), &json!(["gpt-5.5"]), &json!({"discounts": false, "taxes": false, "fees": false})));

    assert_eq!(f.cli_args(&["accounting", "reprice"]).0, json!({"revision": 1, "appended": true, "entries": 6}));
    assert_eq!(f.cli_args(&["accounting", "reprice"]).0, json!({"revision": 1, "appended": false, "entries": 6}), "an unchanged result appends nothing");
    let (cost, first) = f.cli_args(&["accounting", "cost", "--json"]);
    let priced = |card: &str, version: i64, currency: &str, amount: &str, components: serde_json::Value| json!({"status": "priced",
        "basis": "published_rate_estimate", "rate_card": {"card_id": card, "version": version}, "currency": currency, "amount": amount, "components": components});
    let session = |cost: &serde_json::Value, sid: &str| cost["sessions"].as_array().unwrap().iter().find(|s| s["session_id"] == sid).cloned().unwrap();
    let valuation = |cost: &serde_json::Value, sid: &str, n: i64| session(cost, sid)["entries"].as_array().unwrap().iter()
        .find(|e| e["entry_id"] == format!("codex:{sid}:{n}")).unwrap()["valuation"].clone();
    assert_eq!((&cost["revision"], &cost["basis"], &cost["policy"]),
        (&1.into(), &"published_rate_estimate".into(), &POLICY.into()));

    // Doc 10 golden: 1000 × 2 / 10^6 + 500 × 4 / 10^6 = 0.004 exactly, by version 1.
    let before = session(&cost, BEFORE);
    assert_eq!(before["entries"][0]["valuation"], priced("synthetic-codex", 1, "USD", "0.004", json!({"input": "0.002", "output": "0.002"})));
    assert_eq!(before["entries"][0]["quantities"], json!({"new_input_tokens": 1000, "cache_read_tokens": 0, "cache_write_tokens": 0, "output_tokens": 500}));
    assert_eq!(before["entries"][0]["usage_interval"]["from_unix_ms"], f.decided);
    assert!(before["entries"][0]["usage_interval"]["to_unix_ms"].as_i64().unwrap() < boundary);
    assert_eq!((&before["estimate"], &before["coverage"], &before["rate_cards"]), (&json!({"status": "complete", "currency": "USD", "amount": "0.004"}),
        &json!({"entries": 1, "priced": 1, "unpriced": {}}), &json!(["synthetic-codex@1"])));
    // Doc 05 golden by version 2: 800 × 2 + 200 × 0.5 + 300 × 8 per 10^6 = 0.0041.
    assert_eq!(valuation(&cost, AFTER, 1), priced("synthetic-codex", 2, "USD", "0.0041", json!({"input": "0.0016", "cache_read": "0.0001", "output": "0.0024"})));
    assert_eq!(session(&cost, AFTER)["entries"][0]["usage_interval"]["from_unix_ms"], boundary);
    assert_eq!(valuation(&cost, AFTER, 2), unavailable("no_rate_card"), "gpt-5.5-mini has no card: unknown, not 0");
    assert_eq!(valuation(&cost, AFTER, 3), unavailable("cache_write_convention_unknown"));
    assert_eq!((&session(&cost, AFTER)["estimate"], &session(&cost, AFTER)["coverage"]),
        (&json!({"status": "partial", "reason": "unpriced_entries", "currency": "USD", "priced_amount": "0.0041"}),
         &json!({"entries": 3, "priced": 1, "unpriced": {"cache_write_convention_unknown": 1, "no_rate_card": 1}})));
    assert_eq!(valuation(&cost, STRADDLE, 1), unavailable("rate_change_within_usage_interval"), "no interval evidence to split");
    assert_eq!(session(&cost, STRADDLE)["estimate"], unavailable("no_priced_entries"));
    assert_eq!(valuation(&cost, CACHE, 1), unavailable("cache_read_rate_missing"), "40 cached tokens and version 1 has no cache-read rate");
    let unpriced = json!({"cache_read_rate_missing": 1, "cache_write_convention_unknown": 1, "no_rate_card": 1, "rate_change_within_usage_interval": 1});
    assert_eq!(cost["attempts"], json!([{"attempt_id": f.attempt, "estimate": {"status": "partial", "reason": "unpriced_entries", "currency": "USD", "priced_amount": "0.0081"},
        "coverage": {"entries": 6, "priced": 2, "unpriced": unpriced}, "unlinked_children": null}]));

    // Text rounds only at presentation: 6 decimal places.
    let out = Command::new(BIN).env_clear().env("HOME", f.tmp.path().join("home")).env("PATH", "/usr/bin:/bin")
        .args(["--root", f.root.to_str().unwrap(), "telemetry", "demo", "accounting", "cost"]).output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    for line in [format!("attempt {}: partial USD 0.008100 priced (2 of 6 entries priced; unpriced: cache_read_rate_missing 1, \
        cache_write_convention_unknown 1, no_rate_card 1, rate_change_within_usage_interval 1)", f.attempt),
        format!("  session {BEFORE} primary: USD 0.004000 (1 of 1 entries priced) cards synthetic-codex@1"),
        format!("  session {STRADDLE} primary: unavailable (no_priced_entries) (0 of 1 entries priced; unpriced: rate_change_within_usage_interval 1)")] {
        assert!(text.lines().any(|l| l == line), "{line:?} in\n{text}");
    }

    // A corrected version 3 (output 6) and a EUR card for gpt-5.5-mini append revision 2.
    for name in ["rates-v3.json", "rates-eur.json"] { f.cli_args(&["accounting", "import-rate-card", &rate_card(&f, name, boundary)]); }
    assert_eq!(f.cli_args(&["accounting", "reprice"]).0, json!({"revision": 2, "appended": true, "entries": 6}));
    let (cost, _) = f.cli_args(&["accounting", "cost", "--json"]);
    assert_eq!(cost["revision"], 2);
    assert_eq!(valuation(&cost, BEFORE, 1), priced("synthetic-codex", 1, "USD", "0.004", json!({"input": "0.002", "output": "0.002"})));
    // 800 × 2 + 200 × 0.5 + 300 × 6 per 10^6 = 0.0035; 100 × 1.5 + 20 × 3 per 10^6 = EUR 0.00021.
    assert_eq!(valuation(&cost, AFTER, 1), priced("synthetic-codex", 3, "USD", "0.0035", json!({"input": "0.0016", "cache_read": "0.0001", "output": "0.0018"})));
    assert_eq!(valuation(&cost, AFTER, 2), priced("synthetic-codex-eur", 1, "EUR", "0.00021", json!({"input": "0.00015", "output": "0.00006"})));
    assert_eq!(session(&cost, AFTER)["estimate"], json!({"status": "unavailable", "reason": "mixed_currency", "priced_by_currency": {"EUR": "0.00021", "USD": "0.0035"}}));
    assert_eq!(valuation(&cost, STRADDLE, 1), unavailable("rate_change_within_usage_interval"));
    assert_eq!(cost["attempts"][0]["estimate"], json!({"status": "unavailable", "reason": "mixed_currency", "priced_by_currency": {"EUR": "0.00021", "USD": "0.0075"}}),
        "USD 0.004 + 0.0035 and EUR 0.00021 are never added");

    // Revision 1 is reproduced byte for byte; a re-sync and reprice append nothing;
    // measured tokens never changed.
    assert_eq!(f.cli_args(&["accounting", "cost", "--json", "--revision", "1"]).1, first);
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(f.cli_args(&["accounting", "reprice"]).0, json!({"revision": 2, "appended": false, "entries": 6}));
    assert_eq!(f.cli_args(&["accounting", "entries"]).1, measured);
    assert_eq!((f.count("valuation_revisions"), f.count("valuations"), f.count("rate_cards")), (2, 12, 4));
}

/// A fixture rollout whose rate-limit snapshots carry `@Tn@` (RFC 3339 observation
/// times, from Unix ms) and `@Rn@` (reset times, Unix seconds), written beside the project.
fn quota_rollout(f: &Fixture, name: &str, fixture: &str, start: i64, times: &[i64], resets: &[i64]) {
    quota_rollout_in(f, &f.home, name, fixture, start, times, resets);
}

/// `quota_rollout` under another execution home.
fn quota_rollout_in(f: &Fixture, home: &Path, name: &str, fixture: &str, start: i64, times: &[i64], resets: &[i64]) {
    let mut text = fs::read_to_string(Path::new(ACCOUNTING).join(fixture)).unwrap();
    for (n, t) in times.iter().enumerate() { text = text.replace(&format!("@T{}@", n + 1), &jiff::Timestamp::from_millisecond(*t).unwrap().to_string()); }
    for (n, r) in resets.iter().enumerate() { text = text.replace(&format!("@R{}@", n + 1), &r.to_string()); }
    let path = f.tmp.path().join(fixture);
    fs::write(&path, text).unwrap();
    f.rollout(home, name, &[path.to_str().unwrap()], &f.worktree(), start, "0.154.0");
}

/// M40 at the fixture's one dispatch decision, per window, from `accounting quota`.
fn headroom(f: &Fixture) -> serde_json::Value {
    let (quota, _) = f.cli_args(&["accounting", "quota", "--json"]);
    let decisions = quota["metrics"]["M40"]["decisions"].as_array().unwrap().clone();
    assert_eq!((decisions.len(), &decisions[0]["attempt_id"], &decisions[0]["decided_unix_ms"]), (1, &json!(f.attempt), &json!(f.decided)));
    decisions[0].clone()
}

/// Doc 05 §5b / TM2.7: one 300-minute Codex `primary` window, in percent, of
/// the attempt's execution home. Before the decision it reads 40 → 55.5, then
/// 50 without a reset (flagged, not subtracted: the high-water mark stays
/// 55.5), then 60, one minute before dispatch: headroom 100 − 60 = 40, age
/// 60,000 ms, fresh; observed increase 60 − 40 = 20. After the reset time the
/// window reports 5 then 12.25 under a later `resets_at`: a new window
/// (increase 7.25, remaining 87.75), never a consumption of 5 − 60 = −55.
/// The rollout reports `secondary: null`, so it is `not_reported`, not 0;
/// M38/M39 have no certified Codex source.
#[test]
fn window_reset_starts_new_window_not_negative() {
    let f = Fixture::new();
    let d = f.decided;
    let unavailable = |reason: &str| json!({"status": "unavailable", "reason": reason});
    assert_eq!(f.cli_args(&["accounting", "quota", "--json"]).0, unavailable("collection_not_run"));
    // Reset of the first window one hour after the decision (whole seconds), the next 300 minutes later.
    let r1 = d / 1000 + 3_600;
    let r2 = r1 + 18_000;
    let (t5, t4) = (r1 * 1000 + 3_600_000, d - 60_000);
    quota_rollout(&f, "quota", "quota-reset.jsonl", d - 10_800_000, &[d - 10_800_000, d - 7_200_000, d - 5_400_000, t4, t5, t5 + 600_000], &[r1, r2]);
    f.cli("collect");
    assert_eq!(f.count("codex_rate_limits"), 6);
    assert_eq!(f.cli_args(&["accounting", "quota", "--json"]).0, unavailable("ledger_not_synced"));
    let (synced, _) = f.cli_args(&["accounting", "sync"]);
    assert_eq!(synced["quota_windows"], 2);

    let (quota, first) = f.cli_args(&["accounting", "quota", "--json"]);
    let account = digest(&f.home);
    let (w1, w2) = (format!("codex:{account}:codex:primary:{}", r1 * 1000), format!("codex:{account}:codex:primary:{}", r2 * 1000));
    assert_eq!(quota["semantics"], "not_certified");
    assert_eq!(quota["windows"], json!([
        {"window_id": w1, "service": "codex", "account": account, "limit_id": "codex", "window_kind": "primary", "unit": "percent", "window_minutes": 300,
         "window_start_unix_ms": r1 * 1000 - 18_000_000, "resets_unix_ms": r1 * 1000, "start_evidence": "first_observation",
         "first_observed_unix_ms": d - 10_800_000, "last_observed_unix_ms": t4, "first_used": "40", "used": "60", "remaining": "40",
         "observed_increase": "20", "plan_type": "pro", "observations": 3, "flagged": 1},
        {"window_id": w2, "service": "codex", "account": account, "limit_id": "codex", "window_kind": "primary", "unit": "percent", "window_minutes": 300,
         "window_start_unix_ms": r2 * 1000 - 18_000_000, "resets_unix_ms": r2 * 1000, "start_evidence": "reset_elapsed",
         "first_observed_unix_ms": t5, "last_observed_unix_ms": t5 + 600_000, "first_used": "5", "used": "12.25", "remaining": "87.75",
         "observed_increase": "7.25", "plan_type": "pro", "observations": 2, "flagged": 0}]));
    assert_eq!(quota["observations"], json!({"primary": {"trusted": 5, "used_decreased_without_reset": 1}, "secondary": {"not_reported": 6}}));
    // The drop is kept as observed and flagged inside its window, never subtracted.
    assert_eq!(f.sidecar().query_row("SELECT used,remaining,trust,window_id FROM quota_window_observations WHERE observed_unix_ms=?1 AND window_kind='primary'", [d - 5_400_000],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))).unwrap(),
        ("50".to_owned(), "50".to_owned(), "used_decreased_without_reset".to_owned(), w1.clone()));

    // Headroom at dispatch: the latest trusted snapshot at or before the decision.
    assert_eq!(headroom(&f), json!({"attempt_id": f.attempt, "decided_unix_ms": d, "service": "codex", "account": account, "account_basis": "execution_home", "windows": [
        {"limit_id": "codex", "window_kind": "primary", "unit": "percent", "window_id": w1, "window_minutes": 300, "resets_unix_ms": r1 * 1000,
         "observed_unix_ms": t4, "age_ms": 60_000, "value": "40", "used": "60", "freshness": "fresh"},
        {"limit_id": "codex", "window_kind": "secondary", "value": unavailable("not_reported")}]}));
    assert_eq!((&quota["metrics"]["M40"]["definition"], &quota["metrics"]["M40"]["stale_after_ms"]), (&json!("M40.quota-windows-v1"), &json!(900_000)));
    let text = f.text(&["accounting", "quota"]);
    for line in [format!("M40 {} codex primary remaining 40% age_ms=60000 fresh", f.attempt), format!("M40 {} codex secondary n/a (not_reported)", f.attempt),
        "M38 throttled_time_share n/a (throttling_not_certified)".to_owned()] {
        assert!(text.lines().any(|l| l == line), "{line:?} in\n{text}");
    }

    // M38/M39 reach the report through the lane hook: unknown, never 0.
    let report = f.report();
    assert_eq!((&report["metrics"]["M38"]["value"], &report["metrics"]["M38"]["name"]), (&unavailable("throttling_not_certified"), &json!("throttled_time_share")));
    assert_eq!((&report["metrics"]["M39"]["value"], &report["metrics"]["M39"]["name"]), (&unavailable("provider_errors_not_certified"), &json!("provider_error_rate")));
    assert_eq!(quota["metrics"]["M38"], report["metrics"]["M38"]);

    // Replay: a second collect and sync leave the windows byte-identical.
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(f.cli_args(&["accounting", "quota", "--json"]).1, first);
    assert_eq!((f.count("quota_window_observations"), f.count("quota_windows")), (12, 2));

    // A snapshot 20 minutes old at dispatch is stale (value kept with its age); one
    // whose window reset before the decision no longer applies.
    let f = Fixture::new();
    let d = f.decided;
    quota_rollout(&f, "stale", "quota-single.jsonl", d - 1_200_000, &[d - 1_200_000], &[d / 1000 + 3_600]);
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    let primary = headroom(&f)["windows"][0].clone();
    assert_eq!((&primary["value"], &primary["used"], &primary["age_ms"], &primary["freshness"]), (&json!("62.5"), &json!("37.5"), &json!(1_200_000), &json!("stale")));
    assert!(f.text(&["accounting", "quota"]).lines().any(|l| l == format!("M40 {} codex primary remaining 62.5% age_ms=1200000 stale", f.attempt)));

    let f = Fixture::new();
    let d = f.decided;
    quota_rollout(&f, "reset", "quota-single.jsonl", d - 7_200_000, &[d - 7_200_000], &[d / 1000 - 3_600]);
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    let primary = headroom(&f)["windows"][0].clone();
    assert_eq!((&primary["value"], &primary["age_ms"], primary.get("freshness")), (&unavailable("window_reset_since_observation"), &json!(7_200_000), None));

    // Missing snapshots: none in the home → no_observation; one without a `primary`
    // window → incomplete, no_trusted_observation. Never a headroom of 0.
    let f = Fixture::new();
    let d = f.decided;
    f.rollout(&f.home, "record", &[RECORD], &f.worktree(), d + 1_000, "0.154.0");
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(headroom(&f)["value"], unavailable("no_observation"));
    quota_rollout(&f, "no-primary", "quota-no-primary.jsonl", d - 60_000, &[d - 60_000], &[]);
    f.cli("collect");
    assert_eq!(f.cli_args(&["accounting", "sync"]).0["quota_windows"], 0);
    let (quota, _) = f.cli_args(&["accounting", "quota", "--json"]);
    assert_eq!((&quota["windows"], &quota["observations"]), (&json!([]), &json!({"primary": {"incomplete": 1}, "secondary": {"not_reported": 1}})));
    assert_eq!(headroom(&f)["value"], unavailable("no_trusted_observation"));
    assert!(f.text(&["accounting", "quota"]).lines().any(|l| l == format!("M40 {} n/a (no_trusted_observation)", f.attempt)));
}

/// Session ids written literally in the record-time rollouts.
const TIMED: &str = "00000000-0000-4000-8000-0000000b7101";
const OTHER_PROVIDER: &str = "00000000-0000-4000-8000-0000000b7102";
const UNVERIFIED: &str = "00000000-0000-4000-8000-0000000b7103";

/// Contracts-collection A4 → B3: one session starting before a rate boundary
/// and first observed after it. With its A4 record times each record is
/// priced by the card at its own time: 1,000 in + 500 out before the boundary
/// at $0.004 (version 1), 800 new + 200 cached + 300 out after it at $0.0041
/// (version 2). Its record without a line time falls back to session start →
/// first observed, which straddles the boundary: unavailable, not split. A
/// session reporting provider `openai` against the `synthetic` cards is
/// `provider_mismatch`; one reporting none is priced ($0.00036) but marked
/// `provider_unverified`. A corrected version 3 appends revision 2 only; the
/// first reads back byte-identical.
#[test]
fn record_times_narrow_rate_card_interval() {
    let f = Fixture::new();
    let d = f.decided;
    let boundary = d + 200;
    quota_rollout(&f, "timed", "timed.jsonl", d, &[d + 100, d + 300], &[]);
    quota_rollout(&f, "other-provider", "other-provider.jsonl", d, &[d + 100], &[]);
    quota_rollout(&f, "unverified", "unverified.jsonl", d, &[d + 300], &[]);
    while unix_ms() <= d + 300 { std::thread::sleep(Duration::from_millis(1)); }
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    let measured = f.cli_args(&["accounting", "entries"]).1;
    for name in ["rates-v1.json", "rates-v2.toml"] { f.cli_args(&["accounting", "import-rate-card", &rate_card(&f, name, boundary)]); }
    assert_eq!(f.cli_args(&["accounting", "reprice"]).0, json!({"revision": 1, "appended": true, "entries": 5}));
    assert_eq!(f.cli_args(&["accounting", "reprice"]).0, json!({"revision": 1, "appended": false, "entries": 5}));
    let (cost, first) = f.cli_args(&["accounting", "cost", "--json"]);
    assert_eq!(cost["policy"], POLICY);
    let entry = |cost: &serde_json::Value, sid: &str, n: i64| cost["sessions"].as_array().unwrap().iter().find(|s| s["session_id"] == sid).unwrap()["entries"]
        .as_array().unwrap().iter().find(|e| e["entry_id"] == format!("codex:{sid}:{n}")).cloned().unwrap();
    let priced = |version: i64, amount: &str, components: serde_json::Value| json!({"status": "priced", "basis": "published_rate_estimate",
        "rate_card": {"card_id": "synthetic-codex", "version": version}, "currency": "USD", "amount": amount, "components": components});
    let at = |t: i64| json!({"from_unix_ms": t, "to_unix_ms": t, "basis": "record_time"});

    // 1000 × 2 + 500 × 4 per 10^6 = 0.004 at d + 100 (version 1).
    let e = entry(&cost, TIMED, 1);
    assert_eq!((&e["usage_interval"], &e["valuation"], &e["provider_check"]),
        (&at(d + 100), &priced(1, "0.004", json!({"input": "0.002", "output": "0.002"})), &json!("matched")));
    // 800 × 2 + 200 × 0.5 + 300 × 8 per 10^6 = 0.0041 at d + 300 (version 2).
    let e = entry(&cost, TIMED, 2);
    assert_eq!((&e["usage_interval"], &e["valuation"], &e["provider_check"]),
        (&at(d + 300), &priced(2, "0.0041", json!({"input": "0.0016", "cache_read": "0.0001", "output": "0.0024"})), &json!("matched")));
    // No line time: session start → first observation, across the boundary.
    let e = entry(&cost, TIMED, 3);
    assert_eq!((&e["usage_interval"]["from_unix_ms"], &e["usage_interval"]["basis"], &e["valuation"], e.get("provider_check")),
        (&json!(d), &json!("session_start..first_observed"), &json!({"status": "unavailable", "reason": "rate_change_within_usage_interval"}), None));
    assert!(e["usage_interval"]["to_unix_ms"].as_i64().unwrap() > boundary);
    assert_eq!(entry(&cost, OTHER_PROVIDER, 1)["valuation"], json!({"status": "unavailable", "reason": "provider_mismatch"}));
    // 100 × 2 + 20 × 8 per 10^6 = 0.00036, no provider to check.
    let e = entry(&cost, UNVERIFIED, 1);
    assert_eq!((&e["valuation"], &e["provider_check"]), (&priced(2, "0.00036", json!({"input": "0.0002", "output": "0.00016"})), &json!("provider_unverified")));
    assert_eq!(cost["attempts"], json!([{"attempt_id": f.attempt, "estimate": {"status": "partial", "reason": "unpriced_entries", "currency": "USD", "priced_amount": "0.00846"},
        "coverage": {"entries": 5, "priced": 3, "unpriced": {"provider_mismatch": 1, "rate_change_within_usage_interval": 1}}, "unlinked_children": null}]));

    // Version 3 (output 6 from the boundary): 800 × 2 + 200 × 0.5 + 300 × 6 = 0.0035; 100 × 2 + 20 × 6 = 0.00032.
    f.cli_args(&["accounting", "import-rate-card", &rate_card(&f, "rates-v3.json", boundary)]);
    assert_eq!(f.cli_args(&["accounting", "reprice"]).0, json!({"revision": 2, "appended": true, "entries": 5}));
    let (cost, _) = f.cli_args(&["accounting", "cost", "--json"]);
    assert_eq!(entry(&cost, TIMED, 1)["valuation"], priced(1, "0.004", json!({"input": "0.002", "output": "0.002"})));
    assert_eq!(entry(&cost, TIMED, 2)["valuation"], priced(3, "0.0035", json!({"input": "0.0016", "cache_read": "0.0001", "output": "0.0018"})));
    assert_eq!(entry(&cost, UNVERIFIED, 1)["valuation"], priced(3, "0.00032", json!({"input": "0.0002", "output": "0.00012"})));
    assert_eq!(cost["attempts"][0]["estimate"]["priced_amount"], "0.00782");
    assert_eq!(f.cli_args(&["accounting", "cost", "--json", "--revision", "1"]).1, first);
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(f.cli_args(&["accounting", "reprice"]).0, json!({"revision": 2, "appended": false, "entries": 5}));
    assert_eq!(f.cli_args(&["accounting", "entries"]).1, measured);
    assert_eq!((f.count("valuation_revisions"), f.count("valuations")), (2, 10));
}

/// Contracts-collection A4 → B4: the snapshot's `secondary` window (10,080
/// minutes) is a second window kind under the primary's trust rules. It reads
/// 10 → 12.5 before dispatch (headroom 87.5, age 120,000 ms, increase 2.5),
/// then 11 without a reset (flagged, not subtracted), then `null`
/// (`not_reported`). The primary resets meanwhile (20 → 30, then 5 → 6 in a
/// new window). `rate_limit_reached_type` is counted as evidence only; M38
/// stays unavailable. Snapshots read before A4 have no secondary row:
/// `not_collected`.
#[test]
fn secondary_window_is_tracked() {
    let f = Fixture::new();
    let d = f.decided;
    let (r1, s1) = (d / 1000 + 3_600, d / 1000 + 86_400);
    let r2 = r1 + 18_000;
    let (t3, t2) = (r1 * 1000 + 60_000, d - 120_000);
    quota_rollout(&f, "secondary", "quota-secondary.jsonl", d - 7_200_000, &[d - 7_200_000, t2, t3, t3 + 60_000], &[r1, r2, s1]);
    f.cli("collect");
    assert_eq!(f.cli_args(&["accounting", "sync"]).0["quota_windows"], 3);
    let (quota, first) = f.cli_args(&["accounting", "quota", "--json"]);
    let account = digest(&f.home);
    let id = |kind: &str, resets: i64| format!("codex:{account}:codex:{kind}:{}", resets * 1000);
    let window = |kind: &str, minutes: i64, resets: i64, evidence: &str, (first, last): (i64, i64), [first_used, used, remaining, increase]: [&str; 4], (observations, flagged): (i64, i64)|
        json!({"window_id": id(kind, resets), "service": "codex", "account": account, "limit_id": "codex", "window_kind": kind, "unit": "percent",
            "window_minutes": minutes, "window_start_unix_ms": resets * 1000 - minutes * 60_000, "resets_unix_ms": resets * 1000, "start_evidence": evidence,
            "first_observed_unix_ms": first, "last_observed_unix_ms": last, "first_used": first_used, "used": used, "remaining": remaining,
            "observed_increase": increase, "plan_type": "pro", "observations": observations, "flagged": flagged});
    assert_eq!(quota["windows"], json!([
        window("primary", 300, r1, "first_observation", (d - 7_200_000, t2), ["20", "30", "70", "10"], (2, 0)),
        window("primary", 300, r2, "reset_elapsed", (t3, t3 + 60_000), ["5", "6", "94", "1"], (2, 0)),
        window("secondary", 10_080, s1, "first_observation", (d - 7_200_000, t2), ["10", "12.5", "87.5", "2.5"], (2, 1))]));
    assert_eq!(quota["observations"], json!({"primary": {"trusted": 4}, "secondary": {"trusted": 2, "used_decreased_without_reset": 1, "not_reported": 1}}));
    assert_eq!(quota["evidence"], json!({"rate_limit_reached_type": {"snapshots": {"primary": 1}, "semantics": "not_certified", "certified": "fixture"}}));
    assert_eq!(quota["metrics"]["M38"]["value"], json!({"status": "unavailable", "reason": "throttling_not_certified"}));

    // Headroom at dispatch per window kind, from the snapshot two minutes before it.
    let at = |kind: &str, minutes: i64, resets: i64, value: &str, used: &str| json!({"limit_id": "codex", "window_kind": kind, "unit": "percent",
        "window_id": id(kind, resets), "window_minutes": minutes, "resets_unix_ms": resets * 1000, "observed_unix_ms": t2, "age_ms": 120_000,
        "value": value, "used": used, "freshness": "fresh"});
    assert_eq!(headroom(&f)["windows"], json!([at("primary", 300, r1, "70", "30"), at("secondary", 10_080, s1, "87.5", "12.5")]));
    assert!(f.text(&["accounting", "quota"]).lines().any(|l| l == format!("M40 {} codex secondary remaining 87.5% age_ms=120000 fresh", f.attempt)));

    // Replay: a second collect and sync leave the windows byte-identical.
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(f.cli_args(&["accounting", "quota", "--json"]).1, first);
    assert_eq!((f.count("quota_window_observations"), f.count("quota_windows")), (8, 3));

    // Fixture only: snapshots stored before A4 have no secondary row.
    f.sidecar().execute_batch("DELETE FROM codex_rate_limit_windows").unwrap();
    f.cli_args(&["accounting", "sync"]);
    let (quota, _) = f.cli_args(&["accounting", "quota", "--json"]);
    assert_eq!(quota["observations"], json!({"primary": {"trusted": 4}}));
    assert_eq!(headroom(&f)["windows"][1], json!({"limit_id": "codex", "window_kind": "secondary", "value": {"status": "unavailable", "reason": "not_collected"}}));
}

/// Live A4 run: two execution homes holding one login reported the same
/// window. Home A (the attempt's) reads 37.5 one minute before dispatch; home B
/// reads 37.5 two minutes before and 40 half a minute before it, for the same
/// `codex` primary window (300 minutes, same `resets_at`), plus a secondary
/// window A reports as `null`. Accounts are keyed by home (`account_basis:
/// execution_home`): two primary windows (increases 0 and 2.5, remaining 62.5
/// and 60), named together as one shared-window candidate, never merged or
/// summed (not 37.5 + 40 = 77.5 used, not 2.5 attributed to A). A's headroom
/// stays its own 62.5, not B's newer 60; the secondary window is not shared.
#[test]
fn shared_window_across_homes_is_flagged_not_summed() {
    let f = Fixture::new();
    let d = f.decided;
    let other = fs::canonicalize(f.tmp.path()).unwrap().join("other-home");
    let (r1, s1) = (d / 1000 + 3_600, d / 1000 + 86_400);
    quota_rollout(&f, "home-a", "quota-single.jsonl", d - 60_000, &[d - 60_000], &[r1]);
    quota_rollout_in(&f, &other, "home-b", "quota-shared.jsonl", d - 120_000, &[d - 120_000, d - 30_000], &[r1, s1]);
    f.cli("collect");
    assert_eq!(f.cli_args(&["accounting", "sync"]).0["quota_windows"], 3);
    let (quota, first) = f.cli_args(&["accounting", "quota", "--json"]);
    let (a, b) = (digest(&f.home), digest(&other));
    let id = |account: &str, kind: &str, resets: i64| format!("codex:{account}:codex:{kind}:{}", resets * 1000);
    let window = |account: &str, kind: &str, minutes: i64, resets: i64, (first, last): (i64, i64), [first_used, used, remaining, increase]: [&str; 4], observations: i64|
        json!({"window_id": id(account, kind, resets), "service": "codex", "account": account, "limit_id": "codex", "window_kind": kind, "unit": "percent",
            "window_minutes": minutes, "window_start_unix_ms": resets * 1000 - minutes * 60_000, "resets_unix_ms": resets * 1000, "start_evidence": "first_observation",
            "first_observed_unix_ms": first, "last_observed_unix_ms": last, "first_used": first_used, "used": used, "remaining": remaining,
            "observed_increase": increase, "plan_type": "pro", "observations": observations, "flagged": 0});
    let a_windows = vec![window(&a, "primary", 300, r1, (d - 60_000, d - 60_000), ["37.5", "37.5", "62.5", "0"], 1)];
    let b_windows = vec![window(&b, "primary", 300, r1, (d - 120_000, d - 30_000), ["37.5", "40", "60", "2.5"], 2),
        window(&b, "secondary", 10_080, s1, (d - 30_000, d - 30_000), ["10", "10", "90", "0"], 1)];
    // Windows are listed by account: the digests' order decides which home comes first.
    let (windows, accounts) = if a < b { ([a_windows, b_windows].concat(), [&a, &b]) } else { ([b_windows, a_windows].concat(), [&b, &a]) };
    assert_eq!(quota["account_basis"], "execution_home");
    assert_eq!(quota["windows"], json!(windows));
    assert_eq!(quota["shared_window_candidates"], json!([{"limit_id": "codex", "window_kind": "primary", "window_minutes": 300, "resets_unix_ms": r1 * 1000,
        "accounts": accounts, "window_ids": accounts.map(|account| id(account, "primary", r1)), "evidence": "same_limit_kind_minutes_resets", "merged": false}]));
    assert_eq!(quota["observations"], json!({"primary": {"trusted": 3}, "secondary": {"trusted": 1, "not_reported": 2}}));

    // A's headroom is its own latest snapshot (62.5), with the candidate named; B's value is not used.
    let expected = json!({"attempt_id": f.attempt, "decided_unix_ms": d, "service": "codex", "account": a, "account_basis": "execution_home", "windows": [
        {"limit_id": "codex", "window_kind": "primary", "unit": "percent", "window_id": id(&a, "primary", r1), "window_minutes": 300, "resets_unix_ms": r1 * 1000,
         "observed_unix_ms": d - 60_000, "age_ms": 60_000, "shared_window_candidates": accounts, "value": "62.5", "used": "37.5", "freshness": "fresh"},
        {"limit_id": "codex", "window_kind": "secondary", "value": {"status": "unavailable", "reason": "not_reported"}}]});
    assert_eq!(headroom(&f), expected);
    assert_eq!(f.report()["metrics"]["M40"]["decisions"], json!([expected]));
    let text = f.text(&["accounting", "quota"]);
    let line = format!("shared window candidate codex primary reset {}: accounts {}, {} (execution homes; not merged, never summed)", r1 * 1000, accounts[0], accounts[1]);
    assert!(text.lines().any(|l| l == line), "{line:?} in\n{text}");
    assert!(text.lines().any(|l| l == format!("M40 {} codex primary remaining 62.5% age_ms=60000 fresh", f.attempt)), "{text}");

    // Replay leaves everything byte-identical.
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(f.cli_args(&["accounting", "quota", "--json"]).1, first);
}

/// Herdr stand-in: logs every call with its socket, answers `agent list` from
/// `$HOME/agents.json`, and exits without a reply when that file is absent
/// (server not answering). Anything else is refused.
const FAKE_HERDR: &str = "#!/bin/sh\nprintf '%s|%s\\n' \"$*\" \"$HERDR_SOCKET_PATH\" >> \"$HOME/calls\"\ncase \"$*\" in\n\
'agent list') [ -f \"$HOME/agents.json\" ] || exit 1; cat \"$HOME/agents.json\";;\n*) echo '{\"error\":{\"code\":\"refused\",\"message\":\"unexpected\"}}'; exit 2;;\nesac\n";

/// Minute `m` after the fixed start (November 2023, so any real "now" is far past the last sample).
const T0: i64 = 1_700_000_000_000;
fn minute(m: f64) -> i64 { T0 + (m * 60_000.0) as i64 }

/// Doc 10 §5a attention golden on two attempts: a1 waits 1–3 and a2 waits
/// 2–6 (minutes) → union 5 minutes, sum 6, observed by the ticker's sampling
/// pass through a Herdr stand-in. a1 waits again at 4 and ends at 4.5: that
/// interval is censored `attempt_ended`, never closed at a guess. a2 waits at
/// 7, Herdr stops answering at 8 (a `herdr_unreachable` gap censors the
/// wait), waits at 9 (after the gap: kept, not counted), works at 10; the
/// ticker is not running 10–13 and not since 13 (`not_observed` gaps). a3
/// was launched and cancelled before any pass: `not_observed`, never 0.
#[test]
fn attention_intervals_union_and_censor() {
    use herdr_projects::store::SqliteStore;
    let tmp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    let (root, home) = (base.join("root"), base.join("home"));
    let project = root.join("demo");
    fs::create_dir_all(project.join(".state")).unwrap();
    fs::create_dir_all(&home).unwrap();
    let herdr = base.join("herdr");
    fs::write(&herdr, FAKE_HERDR).unwrap();
    fs::set_permissions(&herdr, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let socket = base.join("herdr.sock");
    let _server = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let socket = socket.to_str().unwrap().to_owned();

    // Canonical rows, planted as the golden report test does: t1 accepted
    // (verify_only, verified), t2 and t3 open. Each attempt has the launch
    // receipt its start would record, one minute before the first pass.
    let db_path = project.join(".state/state.db");
    drop(SqliteStore::create(&db_path).unwrap());
    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    let hex = |c: char| c.to_string().repeat(64);
    let oid = "a".repeat(40);
    for (task, task_state, attempt, attempt_state, ended) in [("t1", "running", "a1", "running", 0), ("t2", "running", "a2", "running", 0), ("t3", "blocked", "a3", "cancelled", 1)] {
        db.execute("INSERT INTO tasks(id,revision,state,title) VALUES(?1,1,?2,?1)", [task, task_state]).unwrap();
        db.execute("INSERT INTO attempts(id,task_id,revision,state,reservation,termination_observed) VALUES(?1,?2,2,?3,?1,?4)",
            rusqlite::params![attempt, task, attempt_state, ended]).unwrap();
        let n = &attempt[1..];
        let receipt = json!({"version": 2, "attempt": attempt, "operation": format!("op-{attempt}"),
            "route": {"machine": "", "socket": socket, "workspace_id": "w1", "tab_id": "w1:t1", "pane_id": format!("w1:p{n}"), "cwd": format!("/work/{attempt}")},
            "terminal": format!("term-{n}"), "session": {"device": 1, "inode": 2, "born_secs": 3, "born_nanos": 4},
            "agent": {"kind": "codex", "name": format!("worker-{attempt}")}, "observed_unix_ms": minute(-1.0)});
        db.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('runtime.launch_started',?1,1,1,?2)",
            rusqlite::params![format!("op-{attempt}"), receipt.to_string()]).unwrap();
    }
    db.execute("INSERT INTO attempt_lifecycle(attempt_id,state,attempt_revision,unix_ms,source) VALUES('a3','cancelled',2,?1,'fixture')", [minute(-0.5)]).unwrap();
    db.execute("INSERT INTO task_contracts(task_id,contract_revision,project_store,expected_head,repository,base_oid,object_format,route,raw_bytes,raw_digest,installed_seq)
        VALUES('t1',1,'store',1,'/repo',?1,'sha1','verify_only',x'7b7d',?2,1)", rusqlite::params![oid, hex('c')]).unwrap();
    db.execute("INSERT INTO result_submissions(submission_id,project_store,idempotency_key,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,artifact_manifest,claimed_checks,created_unix_ms)
        VALUES(?1,'store',?1,?2,'{}','t1',1,?2,'a1','/repo',?3,?3,'sha1','[]','[]',1000)", rusqlite::params![hex('1'), hex('d'), oid]).unwrap();
    db.execute("INSERT INTO verified_results(result_id,run_id,submission_id,commit_oid,tree_oid,object_format,policy_digest,receipt_digest,isolation,memory_fence,created_unix_ms)
        VALUES(?1,?1,?2,?3,?3,'sha1',?4,?4,'linux-unshare-user-pid-mount-v1',0,2000)", rusqlite::params![hex('4'), hex('1'), oid, hex('e')]).unwrap();

    let cli = |args: &[&str]| -> (serde_json::Value, String) {
        let out = Command::new(BIN).env_clear().env("HOME", &home).env("PATH", "/usr/bin:/bin").env("HERDR_BIN_PATH", &herdr)
            .env("HERDR_PROJECTS_TELEMETRY_COLLECT_SECS", "60").args(["--root", root.to_str().unwrap(), "telemetry", "demo"]).args(args).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let text = String::from_utf8(out.stdout).unwrap();
        (serde_json::from_str(&text).unwrap_or(serde_json::Value::Null), text)
    };
    cli(&["collect"]);
    assert_eq!(cli(&["accounting", "status"]).0, json!({"stream": "accounting", "version": 7}));
    // Before any pass nothing was collected: unavailable, never 0.
    let report = cli(&["report", "--json"]).0;
    for id in ["M31", "M32", "M33"] {
        assert_eq!(report["metrics"][id]["value"], json!({"status": "unavailable", "reason": "attention_not_collected"}), "{id}");
    }

    // Each pass sees the agent list below; the terminal title is screen text that must never be stored.
    let agent = |attempt: &str, status: &str| json!({"pane_id": format!("w1:p{}", &attempt[1..]), "workspace_id": "w1", "tab_id": "w1:t1",
        "cwd": format!("/work/{attempt}"), "agent": "codex", "name": format!("worker-{attempt}"), "agent_status": status, "terminal_title": "SECRET screen text"});
    let pass = |agents: Option<Vec<serde_json::Value>>| {
        match agents {
            Some(agents) => fs::write(home.join("agents.json"), json!({"result": {"agents": agents}}).to_string()).unwrap(),
            None => { let _ = fs::remove_file(home.join("agents.json")); }
        }
        cli(&["accounting", "observe-attention"]).0
    };
    let both = |a1: &str, a2: &str| Some(vec![agent("a1", a1), agent("a2", a2)]);
    assert_eq!(pass(both("working", "working")), json!({"attempts": 2, "states": 2, "gaps": {}}));
    pass(both("blocked", "working"));
    pass(both("blocked", "blocked"));
    pass(both("working", "blocked"));
    pass(both("blocked", "blocked"));
    // a1 completes at minute 4.5 (its terminal mark); later passes no longer sample it.
    db.execute("UPDATE attempts SET state='completed',termination_observed=1 WHERE id='a1'", []).unwrap();
    db.execute("UPDATE tasks SET state='succeeded' WHERE id='t1'", []).unwrap();
    db.execute("INSERT INTO attempt_lifecycle(attempt_id,state,attempt_revision,unix_ms,source) VALUES('a1','completed',2,?1,'fixture')", [minute(4.5)]).unwrap();
    for a2 in ["blocked", "working", "blocked"] { pass(Some(vec![agent("a2", a2)])); }
    assert_eq!(pass(None), json!({"attempts": 1, "states": 0, "gaps": {"herdr_unreachable": 1}}));
    for a2 in ["blocked", "working", "working"] { pass(Some(vec![agent("a2", a2)])); }

    // Read-only: one `agent list` per pass on the recorded socket, nothing else sent to Herdr.
    let calls = fs::read_to_string(home.join("calls")).unwrap();
    assert_eq!(calls.lines().collect::<Vec<_>>(), vec![format!("agent list|{socket}"); 12]);
    // Labels and timestamps only: no screen text, cwd or agent name reaches the sidecar.
    let sidecar = rusqlite::Connection::open(project.join(".state/telemetry.db")).unwrap();
    let dump: Vec<String> = sidecar.prepare("SELECT quote(attempt_id)||quote(observed_unix_ms)||quote(state)||quote(gap)||quote(interval_ms)||quote(source) FROM attention_samples").unwrap()
        .query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
    assert_eq!(dump.len(), 5 * 2 + 7);
    assert!(dump.iter().all(|row| !row.contains("SECRET") && !row.contains("/work") && !row.contains("worker-")), "{dump:?}");
    // Fixture only: the passes ran milliseconds apart; re-time pass k to its planned minute.
    let times: Vec<i64> = sidecar.prepare("SELECT DISTINCT observed_unix_ms FROM attention_samples ORDER BY 1").unwrap()
        .query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
    for (old, m) in times.iter().zip([0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 13.0]) {
        sidecar.execute("UPDATE attention_samples SET observed_unix_ms=?1 WHERE observed_unix_ms=?2", [minute(m), *old]).unwrap();
    }
    drop(sidecar);

    let (attention, _) = cli(&["accounting", "attention", "--json"]);
    let of = |id: &str| attention["attempts"].as_array().unwrap().iter().find(|a| a["attempt_id"] == id).unwrap()["attention"].clone();
    let interval = |opened: f64, start: &str, last: f64, closed: Option<f64>, end: &str, gap: Option<&str>, duration: Option<i64>, counted: bool|
        json!({"opened_unix_ms": minute(opened), "start": start, "last_observed_unix_ms": minute(last), "closed_unix_ms": closed.map(minute), "end": end,
            "gap_reason": gap, "duration_ms": duration, "counted": counted});
    let gap = |from: f64, to: Option<f64>, reason: &str| json!({"from_unix_ms": minute(from), "to_unix_ms": to.map(minute), "reason": reason});
    // a1: observed 0–4 (4 min resolved); waiting 1–3 = 120000; the wait open at its end is censored.
    assert_eq!(of("a1"), json!({"intervals": [
        interval(1.0, "observed_transition", 2.0, Some(3.0), "closed", None, Some(120_000), true),
        interval(4.0, "observed_transition", 4.0, None, "attempt_ended", None, None, true)],
        "gaps": [], "interventions": 2, "uncertain_starts": 0, "waiting_ms": 120_000, "observed_ms": 240_000}));
    // a2: resolved 0–7 (7 min); waiting 2–6 = 240000; 7 censored by the gap; 9–10 has no observed start.
    assert_eq!(of("a2"), json!({"intervals": [
        interval(2.0, "observed_transition", 5.0, Some(6.0), "closed", None, Some(240_000), true),
        interval(7.0, "observed_transition", 7.0, None, "observation_gap", Some("herdr_unreachable"), None, true),
        interval(9.0, "after_gap", 9.0, Some(10.0), "closed", None, None, false)],
        "gaps": [gap(7.0, Some(9.0), "herdr_unreachable"), gap(10.0, Some(13.0), "not_observed"), gap(13.0, None, "not_observed")],
        "interventions": 2, "uncertain_starts": 1, "waiting_ms": 240_000, "observed_ms": 420_000}));
    assert_eq!(of("a3"), json!({"status": "unavailable", "reason": "not_observed", "gaps": [gap(-1.0, Some(-0.5), "not_observed")]}));
    // Union of 1–3 and 2–6 is 5 minutes; the sum is 6.
    assert_eq!(attention["fleet"], json!({"waiting_union_ms": 300_000, "waiting_sum_ms": 360_000, "interventions": 4}));

    // M31: T = {t1} (A = {t1}), a1 fully observed with 2 interventions → 2/1.
    // M32: (120000 + 240000) / (240000 + 420000); a3 unobserved is counted, not 0.
    // M33: Herdr's `blocked` has no typed reason.
    let m31 = json!({"definition": "M31.attention-v1", "name": "human_interventions_per_accepted_task", "reason_type": "blocked_untyped",
        "source": "controller_observed", "scope": "human_routed_waits", "coverage": {"attempts": 1, "complete": 1, "not_observed": 0, "with_gaps": 0}, "numerator": 2, "denominator": 1, "value": "2/1"});
    let m32 = json!({"definition": "M32.attention-v1", "name": "waiting_on_you_share", "unit": "ms", "scope": "human_routed_waits", "waiting_union_ms": 300_000,
        "coverage": {"attempts": 3, "observed": 2, "not_observed": 1, "with_gaps": 1, "censored_intervals": 3},
        "numerator": 360_000, "denominator": 660_000, "value": "360000/660000"});
    let m33 = json!({"definition": "M33.attention-v1", "name": "permission_prompts_per_attempt",
        "value": {"status": "unavailable", "reason": "attention_reason_not_exposed"},
        "detail": "stock Herdr reports `blocked` without a typed reason: a permission prompt is not distinguishable from a question or trust dialog"});
    assert_eq!(attention["metrics"], json!({"M31": m31, "M32": m32, "M33": m33}));
    // Codex approval prompts were certified live (codex-live-0.154.0-a4.md §3); other kinds stay fixture.
    assert_eq!((&attention["signal"]["certified"], &attention["signal"]["certified_by_agent_kind"], &attention["signal"]["other_agent_kinds"], &attention["signal"]["scope"]),
        (&json!("live"), &json!({"codex": "live"}), &json!("fixture"), &json!("human_routed_waits")));
    assert!(attention["attempts"].as_array().unwrap().iter().all(|a| a["certified"] == "live"), "{attention}");
    // The central report takes the lane's M31–M33.
    let report = cli(&["report", "--json"]).0;
    assert_eq!((&report["metrics"]["M31"], &report["metrics"]["M32"], &report["metrics"]["M33"]), (&m31, &m32, &m33));
    let (_, text) = cli(&["accounting", "attention"]);
    assert!(text.lines().any(|l| l == format!("  gap {}..open not_observed", minute(13.0))), "{text}");
    assert!(text.lines().any(|l| l == "M32 waiting_on_you_share 360000/660000"), "{text}");
    assert!(text.lines().any(|l| l == "attempt a3 ended n/a (not_observed)"), "{text}");
    assert!(text.lines().any(|l| l == "signal herdr-agent-list-v1 agent_status=blocked (certified: live for codex; fixture for other agent kinds; human_routed_waits)"), "{text}");
}

/// Session ids written literally in the tool rollouts.
const TOOLS_SID: &str = "00000000-0000-4000-8000-0000000b5001";

/// The sidecar as a pre-A6 binary left it: no A6 tables, ingest stream 5.
fn drop_a6_tables(f: &Fixture) {
    f.sidecar().execute_batch("DROP TABLE codex_tool_sources; DROP TABLE codex_tool_calls; DROP TABLE codex_exec_items;
        UPDATE telemetry_streams SET version=5 WHERE stream='ingest';").unwrap();
}

/// TM2.5 / doc 07 M16–M18 over A6 tool metadata (contracts-accounting.md §9).
/// One bound session, observed by two rollouts (the second resumes the first
/// and adds turn 2), plus an unbound session. Five logical calls (4 `exec`, 1
/// `wait`) whatever the replay; an output whose call was not seen (call-9) is
/// counted apart; call-5 has no output. Six exec items are six execution
/// instances: five are inferred to a call by turn and time, one (turn 3) is
/// unattributed. Outcomes: exit 0 ×3 succeeded, exit 2 failed, a NULL exit
/// code and a `failed` status (not certified) are unknown → M17 3/4. M18 has
/// no honest execution end − start; the call → output times 37010 (approval
/// wait), 1000, 2500 and 300 ms are shown apart: nearest-rank p50 1000, p95
/// 37010. The accepted stage is not exposed.
#[test]
fn tool_volume_success_and_latency_are_honest() {
    let f = Fixture::new();
    let unavailable = |reason: &str| json!({"status": "unavailable", "reason": reason});
    // Before any collect: no sidecar, so nothing is 0.
    assert_eq!(f.cli_args(&["accounting", "tools", "--json"]).0, unavailable("collection_not_run"));
    let report = f.report();
    for id in ["M16", "M17", "M18"] {
        assert_eq!((&report["metrics"][id]["value"], &report["metrics"][id]["definition"]), (&unavailable("collection_not_run"), &json!(format!("{id}.tools-v1"))));
    }

    let at = f.decided + 1_000;
    let part = |name: &str| format!("{ACCOUNTING}/{name}");
    f.rollout(&f.home, "a", &[&part("tools.jsonl")], &f.worktree(), at, "0.154.0");
    f.rollout(&f.home, "b", &[&part("tools.jsonl"), &part("tools-resume.jsonl")], &f.worktree(), at, "0.154.0");
    f.rollout(&f.home, "u", &[&part("tools-unbound.jsonl")], &format!("{}/repo", f.project.display()), at, "0.154.0");
    f.cli("collect");

    let (tools, first) = f.cli_args(&["accounting", "tools", "--json"]);
    let coverage = json!({"sessions": 1, "observed": 1, "pending_reread": 0, "predates_collection": 0, "excluded": {"unbound": 1}});
    assert_eq!(tools["coverage"], coverage);
    assert_eq!(tools["sessions"], json!([{"session_id": TOOLS_SID, "attempt_ids": [f.attempt], "tools": {"issued": 5, "without_output": 1,
        "outputs_without_call": 1, "executed": 6, "attributed": 5, "unattributed": 1, "succeeded": 3, "failed": 1, "unknown": 2}}]));

    let m16 = &tools["metrics"]["M16"];
    assert_eq!((&m16["definition"], &m16["name"]), (&json!("M16.tools-v1"), &json!("tool_call_volume")));
    assert_eq!(m16["value"], json!({"issued": 5, "accepted": unavailable("approval_decision_not_exposed"), "executed": 6}));
    let issued = &m16["issued"];
    assert_eq!((&issued["calls"], &issued["by_name"], &issued["name_unreported"], &issued["by_status"], &issued["status_unreported"],
        &issued["without_output"], &issued["outputs_without_call"]),
        (&json!(5), &json!({"exec": 4, "wait": 1}), &json!(0), &json!({"completed": 4}), &json!(1), &json!(1), &json!(1)));
    let executed = &m16["executed"];
    assert_eq!((&executed["executions"], &executed["scope"], &executed["by_source"], &executed["source_unreported"]),
        (&json!(6), &json!("command_execution"), &json!({"unified_exec_startup": 6}), &json!(0)));
    let attribution = &executed["attribution"];
    assert_eq!((&attribution["basis"], &attribution["by_call_name"], &attribution["name_unreported"], &attribution["unattributed"]),
        (&json!("inferred"), &json!({"exec": 5}), &json!(0), &json!(1)));
    assert_eq!((&m16["certified"]["mcp_calls"], &m16["coverage"]), (&json!("not_collected"), &coverage));

    let m17 = &tools["metrics"]["M17"];
    assert_eq!((&m17["value"], &m17["numerator"], &m17["denominator"], &m17["succeeded"], &m17["failed"]),
        (&json!("3/4"), &json!(3), &json!(4), &json!(3), &json!(1)));
    assert_eq!(m17["unknown"], json!({"executions": 2, "by_reason": {"exit_code_unknown": 1, "status_not_certified": 1}}));
    assert_eq!((&m17["pending_calls"], &m17["cancelled"], &m17["timed_out"]),
        (&json!(1), &unavailable("cancellation_not_exposed"), &unavailable("timeout_not_exposed")));

    let m18 = &tools["metrics"]["M18"];
    assert_eq!((&m18["value"], &m18["queue_time"], &m18["timed_out"], &m18["pending_calls"]),
        (&unavailable("execution_duration_not_exposed"), &unavailable("approval_decision_not_exposed"), &unavailable("timeout_not_exposed"), &json!(1)));
    let wall = &m18["call_to_output_ms"];
    assert_eq!((&wall["samples"], &wall["p50_ms"], &wall["p95_ms"], &wall["max_ms"], &wall["negative_intervals"], &wall["method"], &wall["caveat"]),
        (&json!(4), &json!(1000), &json!(37010), &json!(37010), &json!(0), &json!("nearest_rank"), &json!("includes_approval_wait")));
    assert_eq!(wall["by_name"], json!({"exec": {"samples": 3, "p50_ms": 1000, "p95_ms": 37010, "max_ms": 37010},
        "wait": {"samples": 1, "p50_ms": 2500, "p95_ms": 2500, "max_ms": 2500}}));
    assert_eq!(wall["name_unreported"], json!({"samples": 0, "p50_ms": null, "p95_ms": null, "max_ms": null}));

    // The report takes the lane's M16–M18.
    let report = f.report();
    for id in ["M16", "M17", "M18"] { assert_eq!(report["metrics"][id], tools["metrics"][id], "{id}"); }
    let text = f.text(&["accounting", "tools"]);
    for line in ["coverage 1 sessions: 1 observed, 0 pending_reread, 0 predates_collection; excluded unbound 1".to_owned(),
        format!("session {TOOLS_SID} attempts={}: issued 5 (1 without output, 1 outputs without call), executed 6 (5 inferred to a call, 1 unattributed), succeeded 3 failed 1 unknown 2", f.attempt),
        "M16 tool_call_volume issued 5, accepted n/a (approval_decision_not_exposed), executed 6".to_owned(),
        "M17 tool_execution_success 3/4 (unknown 2 excluded, pending calls 1)".to_owned(),
        "M18 tool_latency_p95 n/a (execution_duration_not_exposed)".to_owned(),
        "call_to_output_ms p95 37010 of 4 calls (includes approval wait; not execution time)".to_owned()] {
        assert!(text.lines().any(|l| l == line), "{line} in {text}");
    }

    // Read-only and replayable: a second collect changes nothing.
    f.cli("collect");
    assert_eq!(f.cli_args(&["accounting", "tools", "--json"]).1, first);
}

/// A sidecar without the A6 tables (read-only, before the collect that
/// migrates it) and a session whose rollout waits for its re-read give
/// M16–M18 `unavailable` with the reason, never 0 calls.
#[test]
fn tool_metrics_before_a6_or_reread_are_unavailable() {
    let f = Fixture::new();
    let unavailable = |reason: &str| json!({"status": "unavailable", "reason": reason});
    let at = f.decided + 1_000;
    let part = |name: &str| format!("{ACCOUNTING}/{name}");
    f.rollout(&f.home, "a", &[&part("tools.jsonl")], &f.worktree(), at, "0.154.0");
    let resumed = f.rollout(&f.home, "b", &[&part("tools.jsonl"), &part("tools-resume.jsonl")], &f.worktree(), at, "0.154.0");
    f.cli("collect");
    let (_, fresh) = f.cli_args(&["accounting", "tools", "--json"]);

    drop_a6_tables(&f);
    let (tools, _) = f.cli_args(&["accounting", "tools", "--json"]);
    let coverage = json!({"sessions": 1, "observed": 0, "pending_reread": 0, "predates_collection": 1, "excluded": {}});
    assert_eq!(tools["coverage"], coverage);
    assert_eq!(tools["sessions"], json!([{"session_id": TOOLS_SID, "attempt_ids": [f.attempt], "tools": unavailable("predates_collection")}]));
    let report = f.report();
    for id in ["M16", "M17", "M18"] {
        let expected = json!({"definition": format!("{id}.tools-v1"), "name": tools["metrics"][id]["name"], "value": unavailable("predates_collection"), "coverage": coverage});
        assert_eq!((&tools["metrics"][id], &report["metrics"][id]), (&expected, &expected), "{id}");
    }
    assert_eq!(f.cli_args(&["collectors", "status"]).0, json!({"stream": "ingest", "version": 5}), "a read does not migrate");
    // The next collect migrates and reads both rollouts again: identical to a fresh collect.
    f.cli("collect");
    assert_eq!(f.cli_args(&["accounting", "tools", "--json"]).1, fresh);

    // The resumed rollout is gone before its re-read: the session is pending, not partial.
    drop_a6_tables(&f);
    fs::remove_file(&resumed).unwrap();
    f.cli("collect");
    let (tools, _) = f.cli_args(&["accounting", "tools", "--json"]);
    let coverage = json!({"sessions": 1, "observed": 0, "pending_reread": 1, "predates_collection": 0, "excluded": {}});
    assert_eq!(tools["coverage"], coverage);
    assert_eq!(tools["sessions"][0]["tools"], unavailable("pending_reread"));
    for id in ["M16", "M17", "M18"] { assert_eq!((&tools["metrics"][id]["value"], &tools["metrics"][id]["coverage"]), (&unavailable("pending_reread"), &coverage)); }
    let text = f.text(&["accounting", "tools"]);
    assert!(text.lines().any(|l| l == "M16 tool_call_volume n/a (pending_reread)"), "{text}");
    assert!(text.lines().any(|l| l == "M17 tool_execution_success n/a (pending_reread)"), "{text}");
}

/// Start of a UTC hour (November 2023): fleet windows are whole UTC hours.
const HOUR0: i64 = 472_223 * 3_600_000;
fn at(hours: i64, minutes: i64) -> i64 { HOUR0 + hours * 3_600_000 + minutes * 60_000 }

/// Canonical rows of the fan-out fixture below in `project`; `class_of(attempt,
/// code, docs)` picks each attempt's classification (`None`: unclassified).
fn plant_fleet(project: &Path, class_of: &dyn Fn(&str, &str, &str) -> Option<String>) -> rusqlite::Connection {
    use herdr_projects::store::SqliteStore;
    fs::create_dir_all(project.join(".state")).unwrap();
    let db_path = project.join(".state/state.db");
    drop(SqliteStore::create(&db_path).unwrap());
    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    let id = |n: u32| format!("{n:064x}");
    let oid = "a".repeat(40);
    let (code, docs) = (format!("sha256:{}", "1".repeat(64)), format!("sha256:{}", "2".repeat(64)));
    for (cls, class) in [(&code, "code"), (&docs, "docs")] {
        db.execute("INSERT INTO task_classifications(classification_id,task_id,contract_revision,taxonomy,class,band,features,classifier,revision,reason,created_unix_ms)
            VALUES(?1,?2,1,'v1',?2,'small','{}','fixture',1,NULL,0)", [cls.as_str(), class]).unwrap();
    }
    let attempt = |attempt: &str, task: &str, state: &str, marks: &[(&str, i64)]| {
        db.execute("INSERT INTO tasks(id,revision,state,title) VALUES(?1,1,'running',?1)", [task]).unwrap();
        db.execute("INSERT INTO attempts(id,task_id,revision,state,reservation,termination_observed) VALUES(?1,?2,2,?3,?1,?4)",
            rusqlite::params![attempt, task, state, i64::from(state != "running")]).unwrap();
        for (mark, unix_ms) in marks {
            db.execute("INSERT INTO attempt_lifecycle(attempt_id,state,attempt_revision,unix_ms,source) VALUES(?1,?2,2,?3,'fixture')", rusqlite::params![attempt, mark, unix_ms]).unwrap();
        }
        if !marks.is_empty() {
            db.execute("INSERT INTO dispatch_decisions(attempt_id,task_id,task_revision,contract_revision,classification_id,chosen_configuration_id,eligible,chooser_kind,chooser_principal,reason_codes,decided_unix_ms)
                VALUES(?1,?2,1,1,?3,'cfg',json_array('cfg'),'operator','owner','[\"operator_preference\"]',?4)", rusqlite::params![attempt, task, class_of(attempt, &code, &docs), marks[0].1]).unwrap();
        }
    };
    for n in 1..=4 { attempt(&format!("a{n}"), &format!("ta{n}"), "completed", &[("reserved", at(0, -1)), ("running", at(0, 0)), ("completed", at(1, 0))]); }
    for n in 1..=8 { attempt(&format!("b{n}"), &format!("tb{n}"), "completed", &[("reserved", at(1, -1)), ("running", at(1, 0)), ("completed", at(2, 0))]); }
    // Predates the lifecycle log and ended before it: no marks, ignored.
    attempt("z1", "tz1", "completed", &[]);
    // Open, running since the start of the current hour: only the incomplete window.
    let now_hour = unix_ms().div_euclid(3_600_000) * 3_600_000;
    attempt("c1", "tc1", "running", &[("reserved", now_hour), ("running", now_hour)]);

    // Results: (task, attempt, route, verified at, operations (ref, state, reason, created, integrated)).
    type Op = (&'static str, &'static str, Option<&'static str>, i64, bool);
    let results: [(&str, &str, &str, i64, Vec<Op>); 7] = [
        ("ta1", "a1", "verify_only", at(0, 50), vec![]),
        ("ta2", "a2", "verify_then_integrate", at(0, 35), vec![("refs/heads/main", "blocked", Some("merge_conflict"), at(0, 40), false),
            ("refs/heads/main", "integrated", None, at(0, 45), true)]),
        ("tb1", "b1", "verify_only", at(1, 50), vec![]),
        ("tb2", "b2", "verify_then_integrate", at(1, 25), vec![("refs/heads/main", "integrated", None, at(1, 30), true)]),
        ("tb3", "b3", "verify_then_integrate", at(1, 15), vec![("refs/heads/main", "discarded", Some("stale_base"), at(1, 20), false),
            ("refs/heads/main", "integrated", None, at(1, 40), true)]),
        ("tb4", "b4", "verify_then_integrate", at(1, 50), vec![("refs/heads/release", "blocked", Some("merge_conflict"), at(1, 55), false)]),
        // Verified but never integrated: reached integration only through tb4 above; tb5 has no operation.
        ("tb5", "b5", "verify_then_integrate", at(1, 10), vec![]),
    ];
    let mut n = 100;
    for (task, attempt, route, verified, ops) in results {
        n += 1;
        let (submission, result) = (id(n), id(n + 1000));
        db.execute("INSERT INTO task_contracts(task_id,contract_revision,project_store,expected_head,repository,base_oid,object_format,route,raw_bytes,raw_digest,installed_seq)
            VALUES(?1,1,'store',1,'/repo',?2,'sha1',?3,x'7b7d',?4,1)", rusqlite::params![task, oid, route, id(9)]).unwrap();
        db.execute("INSERT INTO result_submissions(submission_id,project_store,idempotency_key,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,artifact_manifest,claimed_checks,created_unix_ms)
            VALUES(?1,'store',?1,?2,'{}',?3,1,?2,?4,'/repo',?5,?5,'sha1','[]','[]',?6)", rusqlite::params![submission, id(8), task, attempt, oid, verified - 60_000]).unwrap();
        db.execute("INSERT INTO verified_results(result_id,run_id,submission_id,commit_oid,tree_oid,object_format,policy_digest,receipt_digest,isolation,memory_fence,created_unix_ms)
            VALUES(?1,?1,?2,?3,?3,'sha1',?4,?4,'linux-unshare-user-pid-mount-v1',0,?5)", rusqlite::params![result, submission, oid, id(7), verified]).unwrap();
        for (k, (target, state, reason, created, integrated)) in ops.into_iter().enumerate() {
            let op = format!("op-{task}-{k}");
            db.execute("INSERT INTO integration_operations(operation_id,project_store,idempotency_key,payload_digest,repository,ref_name,expected_old_oid,verified_result_id,candidate_id,state,generation,object_format,checks_passed,reason,created_unix_ms)
                VALUES(?1,'store',?1,?2,'/repo',?3,?4,?5,NULL,?6,1,'sha1',?7,?8,?9)",
                rusqlite::params![op, id(6), target, oid, result, state, i64::from(integrated), reason, created]).unwrap();
            if integrated {
                db.execute("INSERT INTO integrated_commits(integrated_id,candidate_id,operation_id,repository,ref_name,commit_oid,tree_oid,expected_old_oid,object_format,created_unix_ms)
                    VALUES(?1,?2,?2,'/repo',?3,?4,?4,?4,'sha1',?5)", rusqlite::params![id(n + 2000 + k as u32), op, target, oid, created]).unwrap();
            }
        }
    }

    db
}

/// Doc 10 §5a "Fan-out" and M36 on canonical rows planted as the golden
/// report test does. Hour 0: a1–a4 run the whole hour (4 active agents), 2
/// tasks accepted → 2/hour. Hour 1: b1–b8 (8 agents), 3 accepted → 3/hour.
/// M35 at k=8 against k=4 (1/2 per agent) = 3/(8 × 1/2) = 3/4, marginal
/// (3 − 2)/(8 − 4) = 1/4 per added agent. Identical task mix is comparable;
/// a different class for the 8-agent hour labels M35 descriptive. Integration:
/// a2 conflicts then integrates, b2 integrates cleanly, b3 is discarded on a
/// moved target then integrates, b4 conflicts on another branch → M36 3/4.
/// An open attempt only touches the incomplete current hour (censored); a
/// pre-log attempt that ended before the log is ignored, one still open
/// makes every window's concurrency unknown.
#[test]
fn fan_out_buckets_and_integration_conflicts() {
    let tmp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    let (root, home) = (base.join("root"), base.join("home"));
    let project = root.join("demo");
    fs::create_dir_all(&home).unwrap();
    let db = plant_fleet(&project, &|_, code, _| Some(code.to_owned()));
    let cli_in = |slug: &str, args: &[&str]| -> String {
        let out = Command::new(BIN).env_clear().env("HOME", &home).env("PATH", "/usr/bin:/bin")
            .args(["--root", root.to_str().unwrap(), "telemetry", slug]).args(args).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    };
    let cli = |args: &[&str]| -> (serde_json::Value, String) {
        let text = cli_in("demo", args);
        (serde_json::from_str(&text).unwrap_or(serde_json::Value::Null), text)
    };
    // Derived from canonical rows only: no sidecar is needed or created.
    let (fleet, _) = cli(&["accounting", "fleet", "--json"]);
    assert!(!project.join(".state/telemetry.db").exists());
    let f = &fleet["fleet"];
    assert_eq!(f["coverage"], json!({"attempts": 14, "running_intervals": 13, "open_censored": 1, "never_running": 0, "predates_lifecycle_log": 1, "end_unknown": 0}));
    assert_eq!(f["windows"], json!({"bucketed": 2, "excluded": {"incomplete": 1, "concurrency_unknown": 0, "outside_window": 0}}));
    let bucket = |level: i64, active: i64, accepted: i64, per_hour: &str, per_agent: &str, m35: &str, marginal: serde_json::Value, mix: serde_json::Value, tvd: &str|
        json!({"level": level, "windows": 1, "window_ms": 3_600_000, "active_ms": active, "mean_active": level.to_string(), "accepted": accepted,
            "accepted_per_hour": per_hour, "per_agent_per_hour": per_agent, "m35": m35, "marginal_per_added_agent_per_hour": marginal, "mix": mix, "mix_tvd": tvd});
    assert_eq!(f["buckets"], json!([
        bucket(4, 14_400_000, 2, "2", "1/2", "1", serde_json::Value::Null, json!({"code/small": "1"}), "0"),
        bucket(8, 28_800_000, 3, "3", "3/8", "3/4", json!("1/4"), json!({"code/small": "1"}), "0")]));
    let comparable = json!({"test": "class_band_active_time_tvd", "max_tvd": "1/10", "reference": "reference bucket", "label": "comparable", "reasons": []});
    let m35 = json!({"definition": "M35.fanout-v1", "name": "fan_out_efficiency", "window_ms": 3_600_000, "level_rule": "round_half_up(time_weighted_active_attempts)",
        "scope": "worker_attempts", "reference_level": 4, "level": 8, "reference_per_agent_per_hour": "1/2", "marginal_per_added_agent_per_hour": "1/4",
        "comparability": comparable, "label": "comparable", "value": "3/4"});
    assert_eq!(fleet["metrics"]["M35"], m35);
    let ratio = |n: i64, d: i64| json!({"numerator": n, "denominator": d, "value": format!("{n}/{d}")});
    let m36 = json!({"definition": "M36.integration-v1", "name": "integration_conflict_rate", "numerator": 3, "denominator": 4, "value": "3/4",
        "events": {"merge_conflict": 2, "stale_base": 1},
        "by_target": {"refs/heads/main": ratio(2, 3), "refs/heads/release": ratio(1, 1)},
        "by_bucket": {"4": ratio(1, 1), "8": ratio(2, 3)},
        "scope": "integrator_observed", "not_observed": ["worker_side_rebase"],
        "event_rule": "blocked/merge_conflict or discarded/stale_base on an operation created no later than the attempt's first integrated operation"});
    assert_eq!(fleet["metrics"]["M36"], m36);
    assert_eq!((&fleet["metrics"]["M34"]["value"], &fleet["metrics"]["M37"]["value"]),
        (&json!({"status": "unavailable", "reason": "coordinator_usage_not_attributed"}), &json!({"status": "unavailable", "reason": "supersession_reason_not_recorded"})));
    // The report takes the lane's M34–M37 unchanged.
    let report = cli(&["report", "--json"]).0;
    for id in ["M34", "M35", "M36", "M37"] { assert_eq!(report["metrics"][id], fleet["metrics"][id], "{id}"); }
    let (_, text) = cli(&["accounting", "fleet"]);
    assert!(text.lines().any(|l| l == "bucket k=8 windows=1 accepted=3 per_hour=3 per_agent=3/8 m35=3/4 marginal=1/4"), "{text}");
    assert!(text.lines().any(|l| l == "M35 fan_out_efficiency 3/4 (comparable)"), "{text}");
    assert!(text.lines().any(|l| l == "M36 integration_conflict_rate 3/4"), "{text}");
    assert!(text.lines().any(|l| l == "M34 coordinator_overhead n/a (coordinator_usage_not_attributed)"), "{text}");
    // A window from hour 1: only the 8-agent level remains; b2–b4 reached integration in it.
    let since = at(1, 0).to_string();
    let windowed = cli(&["report", "--json", "--since", &since]).0;
    assert_eq!(windowed["metrics"]["M35"]["value"], json!({"status": "unavailable", "reason": "single_concurrency_level"}));
    assert_eq!((&windowed["metrics"]["M36"]["numerator"], &windowed["metrics"]["M36"]["denominator"]), (&json!(2), &json!(3)));

    // Mismatched task mix: the 8-agent hour worked on docs tasks. Same numbers, labelled descriptive.
    plant_fleet(&root.join("mixed"), &|a, code, docs| Some(if a.starts_with('b') { docs } else { code }.to_owned()));
    let fleet = serde_json::from_str::<serde_json::Value>(&cli_in("mixed", &["accounting", "fleet", "--json"])).unwrap();
    assert_eq!((&fleet["metrics"]["M35"]["value"], &fleet["metrics"]["M35"]["label"], &fleet["metrics"]["M35"]["comparability"]["reasons"]),
        (&json!("3/4"), &json!("descriptive"), &json!(["task_mix_differs"])));
    assert_eq!((&fleet["fleet"]["buckets"][1]["mix"], &fleet["fleet"]["buckets"][1]["mix_tvd"]), (&json!({"docs/small": "1"}), &json!("1")));
    // An attempt without a classification makes the mix unknown.
    plant_fleet(&root.join("unknown"), &|a, code, docs| (a != "b8").then(|| if a.starts_with('b') { docs } else { code }.to_owned()));
    let fleet = serde_json::from_str::<serde_json::Value>(&cli_in("unknown", &["accounting", "fleet", "--json"])).unwrap();
    assert_eq!(fleet["metrics"]["M35"]["comparability"]["reasons"], json!(["classification_unknown", "task_mix_differs"]));

    // A pre-log attempt still open was active at an unknown time: no window can be bucketed, never 0.
    db.execute("INSERT INTO tasks(id,revision,state,title) VALUES('tz2',1,'running','tz2')", []).unwrap();
    db.execute("INSERT INTO attempts(id,task_id,revision,state,reservation,termination_observed) VALUES('z2','tz2',2,'running','z2',0)", []).unwrap();
    let (fleet, _) = cli(&["accounting", "fleet", "--json"]);
    assert_eq!(fleet["fleet"]["windows"], json!({"bucketed": 0, "excluded": {"incomplete": 1, "concurrency_unknown": 2, "outside_window": 0}}));
    assert_eq!(fleet["metrics"]["M35"]["value"], json!({"status": "unavailable", "reason": "no_complete_window"}));
    assert_eq!((&fleet["metrics"]["M36"]["value"], &fleet["metrics"]["M36"]["by_bucket"]), (&json!("3/4"), &json!({"unknown": ratio(3, 4)})));
}

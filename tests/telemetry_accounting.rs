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
/// (`codex-auto-review`, `source.subagent`) session of 50 carries no native
/// parent id, so it is reported apart as an unlinked child and never added to
/// the root; its record before any `turn_context` (10) is unallocated and its
/// turn spanning two models (5 + 5) is mixed.
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
             "parent": {"status": "unavailable", "reason": "no_native_parent_evidence"}, "total_tokens": 50,
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
    assert_eq!((f.count("session_nodes"), f.count("model_segments")), (3, 5));

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
/// `parent_not_collected`; a `review` guardian (50) has no parent id. Every
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
        {"session_id": FORKED, "role": "fork", "link_basis": "forked_from_id", "certified": "fixture", "total_tokens": 40, "inclusion": not_certified}],
        "total_tokens": not_certified}));
    let fork = of(&sessions, FORKED);
    assert_eq!((&fork["role"], &fork["linkage"], &fork["parent"], &fork["total_tokens"]), (&json!("fork"), &json!("linked_child"),
        &json!({"session_id": PARENT, "link_basis": "forked_from_id", "certified": "fixture"}), &json!(40)));
    let orphan = of(&sessions, ORPHAN);
    assert_eq!((&orphan["role"], &orphan["linkage"], &orphan["parent"], &orphan["total_tokens"]), (&json!("subagent"), &json!("unlinked_child"),
        &json!({"status": "unavailable", "reason": "parent_not_collected", "session_id": ABSENT}), &json!(20)));
    let guardian = of(&sessions, GUARDIAN_SID);
    assert_eq!((&guardian["role"], &guardian["linkage"], &guardian["parent"], &guardian["total_tokens"]), (&json!("guardian"), &json!("unlinked_child"),
        &json!({"status": "unavailable", "reason": "no_native_parent_evidence"}), &json!(50)));
    assert_eq!(sessions["rollup"], json!({"sessions": 100, "linked_children": not_certified, "unlinked_children": 20 + 50, "incomplete_sessions": 0}));
    // Each record counts once: 80 + 25 + 30 + 15 + 40 input, 20 + 5 + 10 + 5 + 10 output.
    let report = f.report();
    assert_eq!((&report["metrics"]["M08"]["value"], &report["metrics"]["M09"]["value"]), (&json!(190), &json!(50)));
    assert_eq!(report["metrics"]["M08"]["coverage"], json!({"certified_sessions": 5, "excluded": {}}));

    // Replay: a second collect and sync leave the graph byte-identical.
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(f.cli_args(&["accounting", "sessions"]).1, first);
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
    let mut text = fs::read_to_string(Path::new(ACCOUNTING).join(fixture)).unwrap();
    for (n, t) in times.iter().enumerate() { text = text.replace(&format!("@T{}@", n + 1), &jiff::Timestamp::from_millisecond(*t).unwrap().to_string()); }
    for (n, r) in resets.iter().enumerate() { text = text.replace(&format!("@R{}@", n + 1), &r.to_string()); }
    let path = f.tmp.path().join(fixture);
    fs::write(&path, text).unwrap();
    f.rollout(&f.home, name, &[path.to_str().unwrap()], &f.worktree(), start, "0.154.0");
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
    assert_eq!(headroom(&f), json!({"attempt_id": f.attempt, "decided_unix_ms": d, "service": "codex", "account": account, "windows": [
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
    assert_eq!(cli(&["accounting", "status"]).0, json!({"stream": "accounting", "version": 6}));
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
        "source": "controller_observed", "coverage": {"attempts": 1, "complete": 1, "not_observed": 0, "with_gaps": 0}, "numerator": 2, "denominator": 1, "value": "2/1"});
    let m32 = json!({"definition": "M32.attention-v1", "name": "waiting_on_you_share", "unit": "ms", "waiting_union_ms": 300_000,
        "coverage": {"attempts": 3, "observed": 2, "not_observed": 1, "with_gaps": 1, "censored_intervals": 3},
        "numerator": 360_000, "denominator": 660_000, "value": "360000/660000"});
    let m33 = json!({"definition": "M33.attention-v1", "name": "permission_prompts_per_attempt",
        "value": {"status": "unavailable", "reason": "attention_reason_not_exposed"},
        "detail": "stock Herdr reports `blocked` without a typed reason: a permission prompt is not distinguishable from a question or trust dialog"});
    assert_eq!(attention["metrics"], json!({"M31": m31, "M32": m32, "M33": m33}));
    assert_eq!(attention["signal"]["certified"], "fixture");
    // The central report takes the lane's M31–M33.
    let report = cli(&["report", "--json"]).0;
    assert_eq!((&report["metrics"]["M31"], &report["metrics"]["M32"], &report["metrics"]["M33"]), (&m31, &m32, &m33));
    let (_, text) = cli(&["accounting", "attention"]);
    assert!(text.lines().any(|l| l == format!("  gap {}..open not_observed", minute(13.0))), "{text}");
    assert!(text.lines().any(|l| l == "M32 waiting_on_you_share 360000/660000"), "{text}");
    assert!(text.lines().any(|l| l == "attempt a3 ended n/a (not_observed)"), "{text}");
}

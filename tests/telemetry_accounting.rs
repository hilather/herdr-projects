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
        "rollup": {"sessions": 200, "unlinked_children": 50, "incomplete_sessions": 0}}));

    // Segments reconcile to the session: 50 + 150 = 200 and 30 + 10 + 10 = 50;
    // the attempt's M08/M09 still count every accepted record once (160 + 40 input, 40 + 10 output).
    let report = f.report();
    assert_eq!((&report["metrics"]["M08"]["value"], &report["metrics"]["M09"]["value"]), (&200.into(), &50.into()));

    // Replay: a second collect and sync leave the graph byte-identical.
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(f.cli_args(&["accounting", "sessions"]).1, first);
    assert_eq!((f.count("session_graph"), f.count("model_segments")), (3, 5));

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
    assert_eq!(sessions["rollup"], json!({"sessions": partial, "unlinked_children": partial, "incomplete_sessions": 1}));
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
        (&1.into(), &"published_rate_estimate".into(), &"usage_interval=session_start..first_observed;split=none".into()));

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
/// `secondary` is not collected, so it is unavailable, not 0; M38/M39 have no
/// certified Codex source.
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
    assert_eq!(quota["observations"], json!({"trusted": 5, "used_decreased_without_reset": 1}));
    // The drop is kept as observed and flagged inside its window, never subtracted.
    assert_eq!(f.sidecar().query_row("SELECT used,remaining,trust,window_id FROM quota_observations WHERE observed_unix_ms=?1", [d - 5_400_000],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))).unwrap(),
        ("50".to_owned(), "50".to_owned(), "used_decreased_without_reset".to_owned(), w1.clone()));

    // Headroom at dispatch: the latest trusted snapshot at or before the decision.
    assert_eq!(headroom(&f), json!({"attempt_id": f.attempt, "decided_unix_ms": d, "service": "codex", "account": account, "windows": [
        {"limit_id": "codex", "window_kind": "primary", "unit": "percent", "window_id": w1, "window_minutes": 300, "resets_unix_ms": r1 * 1000,
         "observed_unix_ms": t4, "age_ms": 60_000, "value": "40", "used": "60", "freshness": "fresh"},
        {"limit_id": "codex", "window_kind": "secondary", "value": unavailable("not_collected")}]}));
    assert_eq!((&quota["metrics"]["M40"]["definition"], &quota["metrics"]["M40"]["stale_after_ms"]), (&json!("M40.quota-windows-v1"), &json!(900_000)));
    let text = f.text(&["accounting", "quota"]);
    for line in [format!("M40 {} codex primary remaining 40% age_ms=60000 fresh", f.attempt), format!("M40 {} codex secondary n/a (not_collected)", f.attempt),
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
    assert_eq!((f.count("quota_observations"), f.count("quota_windows")), (6, 2));

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
    assert_eq!((&quota["windows"], &quota["observations"]), (&json!([]), &json!({"incomplete": 1})));
    assert_eq!(headroom(&f)["value"], unavailable("no_trusted_observation"));
    assert!(f.text(&["accounting", "quota"]).lines().any(|l| l == format!("M40 {} n/a (no_trusted_observation)", f.attempt)));
}

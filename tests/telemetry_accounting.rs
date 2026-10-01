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

/// DG2: hand-computed mixed native counters, collected from isolated homes.
/// Codex 200/1000 + Claude 240/382 + OpenCode 30/140 = 470/1522.
/// Writes (32 + 10) stay separate. Gemini updates have no reconciled denominator.
#[test]
fn cache_read_share_mixed_adapters_and_configuration_comparison() {
    let mut f = Fixture::new();
    f.rollout(&f.home, "cache-a", &[RECORD], &f.worktree(), f.decided + 1000, "0.154.0");
    f.rollout(&f.home, "cache-resume", &[RECORD], &f.worktree(), f.decided + 1000, "0.154.0");
    let mut configurations = std::collections::BTreeMap::new();
    let canonical = f.project.join(".state/state.db");
    let mut store = herdr_projects::store::SqliteStore::open(&canonical).unwrap();
    let snapshot = store.read_snapshot(None).unwrap();
    store.set_scheduler_policy(snapshot.head, snapshot.scheduler.unwrap().policy.revision, 1, 8).unwrap();
    drop(store);
    configurations.insert("codex", rusqlite::Connection::open(&canonical).unwrap().query_row(
        "SELECT chosen_configuration_id FROM dispatch_decisions WHERE attempt_id=?1", [&f.attempt], |r| r.get::<_, String>(0)).unwrap());
    for (kind, version) in [("claude", "2.1.3"), ("opencode", "1.18.34"), ("gemini", "0.62.0")] {
        let home = f.tmp.path().join(format!("synthetic-{kind}-home"));
        let mut profile = codex_profile(&f.config, kind, kind, Some(&home));
        profile.agent.version = version.into();
        plant_profile(&canonical, profile);
        f.readmit(kind);
        let db = rusqlite::Connection::open(&canonical).unwrap();
        (f.attempt, f.decided) = db.query_row("SELECT a.id,d.decided_unix_ms FROM attempts a JOIN dispatch_decisions d ON d.attempt_id=a.id WHERE a.state='reserved'",
            [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        configurations.insert(kind, db.query_row("SELECT chosen_configuration_id FROM dispatch_decisions WHERE attempt_id=?1",
            [&f.attempt], |r| r.get::<_, String>(0)).unwrap());
        f.home = home;
        db.execute("INSERT INTO collector_bindings(attempt_id,revision,state,collector,execution_home,unix_ms,source)
            VALUES(?1,1,'active',?2,?3,?4,'apply_launch_started')",
            rusqlite::params![f.attempt, kind, f.home.display().to_string(), unix_ms()]).unwrap();
        let at = f.decided + 1000;
        let ts = jiff::Timestamp::from_millisecond(at).unwrap().to_string();
        match kind {
            "claude" => {
                let dir = f.home.join(".claude/projects").join(f.worktree().replace('/', "-")); fs::create_dir_all(&dir).unwrap();
                let text = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/telemetry/claude-code/session.jsonl")).unwrap()
                    .replace("@SID@", "cache-claude").replace("@CWD@", &f.worktree()).replace("@VERSION@", version).replace("@TS@", &ts);
                fs::write(dir.join("cache-claude.jsonl"), &text).unwrap();
                // This source omits cache counters; it must never contribute a zero read.
                let missing = text.replace("cache-claude", "cache-missing")
                    .replace("\"cache_read_input_tokens\":200,", "").replace("\"cache_read_input_tokens\":40", "\"unreported\":null");
                fs::write(dir.join("cache-missing.jsonl"), missing).unwrap();
            }
            "opencode" => {
                let path = f.home.join(".local/share/opencode/opencode.db"); fs::create_dir_all(path.parent().unwrap()).unwrap();
                let native = rusqlite::Connection::open(path).unwrap();
                native.execute_batch("CREATE TABLE session(id TEXT PRIMARY KEY,directory TEXT,version TEXT,time_created INTEGER);
                    CREATE TABLE message(id TEXT PRIMARY KEY,session_id TEXT,time_created INTEGER,time_updated INTEGER,data TEXT);
                    CREATE TABLE part(id TEXT PRIMARY KEY,message_id TEXT,session_id TEXT,time_created INTEGER,time_updated INTEGER,data TEXT);").unwrap();
                native.execute("INSERT INTO session VALUES('cache-open',?1,?2,?3)", rusqlite::params![f.worktree(), version, at]).unwrap();
                let mut raw: serde_json::Value = serde_json::from_str(include_str!("fixtures/telemetry/opencode/assistant.json")).unwrap();
                raw["time"] = json!({"created": at, "completed": at + 10});
                native.execute("INSERT INTO message VALUES('cache-message','cache-open',?1,?1,?2)", rusqlite::params![at, raw.to_string()]).unwrap();
            }
            "gemini" => {
                fs::create_dir_all(f.worktree()).unwrap();
                let dir = f.home.join(".gemini/tmp/synthetic/chats"); fs::create_dir_all(&dir).unwrap();
                fs::write(dir.join("cache-gemini.jsonl"), format!("{}\n{}\n",
                    json!({"sessionId": "cache-gemini", "projectHash": format!("{:x}", Sha256::digest(f.worktree().as_bytes())), "startTime": ts}),
                    json!({"sessionId": "cache-gemini", "type": "gemini", "id": "g1", "timestamp": ts,
                        "tokens": {"input": 100, "cached": 80, "output": 20, "thoughts": 0, "tool": 0, "total": 120}}))).unwrap();
            }
            _ => unreachable!(),
        }
    }
    f.cli("collect");
    assert_eq!(f.report()["metrics"]["M10"]["value"]["reason"], "accounting_sync_required");
    f.cli_args(&["accounting", "sync"]);
    let m10 = f.report()["metrics"]["M10"].clone();
    assert_eq!((&m10["value"], &m10["numerator"], &m10["denominator"], &m10["cache_write_tokens"]),
        (&json!("470/1522"), &json!(470), &json!(1522), &json!(42)), "{m10}");
    assert_eq!(m10["coverage"], json!({"certified_sessions": 3, "accepted_records": 4,
        "excluded": {"cache_denominator_not_reconciled": 1, "records_not_accepted": 1}}));
    let comparison = f.cli_args(&["compare", "--metric", "M10", "--json"]).0;
    for (kind, value, n, d) in [("codex", "200/1000", 200, 1000), ("claude", "240/382", 240, 382), ("opencode", "30/140", 30, 140)] {
        let arm = &comparison["configurations"][&configurations[kind]];
        assert_eq!((&arm["value"], &arm["numerator"], &arm["denominator"]), (&json!(value), &json!(n), &json!(d)));
    }
    assert_eq!(comparison["configurations"][&configurations["gemini"]]["value"]["reason"], "no_eligible_cache_usage");
    assert!(f.text(&["compare", "--metric", "M10"]).contains("470/1522"));
    assert!(f.text(&["compare", "--metric", "M10"]).contains("n/a (no_eligible_cache_usage)"));
    assert_eq!(f.report()["metrics"]["M08"]["value"], 1522);
    f.cli_args(&["query", "--metric", "M10", "--json"]);
    f.cli_args(&["analytics", "refresh"]);
    let before = f.cli_args(&["analytics", "snapshot"]).1;
    assert_eq!(f.cli_args(&["analytics", "rebuild", "--verify"]).0["identical"], true);
    f.cli_args(&["analytics", "rebuild"]);
    assert_eq!(f.cli_args(&["analytics", "snapshot"]).1, before);
    f.cli("collect"); f.cli_args(&["accounting", "sync"]);
    assert_eq!(f.report()["metrics"]["M10"], m10);
}

#[test]
fn cache_share_zero_unknown_and_late_restatement() {
    use std::io::Write;
    let f = Fixture::new();
    assert_eq!(f.report()["metrics"]["M10"]["value"]["reason"], "no_certified_source");
    let path = f.rollout(&f.home, "cache-zero", &[RECORD], &f.worktree(), f.decided + 1000, "0.154.0");
    let text = fs::read_to_string(&path).unwrap().replace("\"cached_input_tokens\":200", "\"cached_input_tokens\":0");
    fs::write(&path, text).unwrap();
    f.cli("collect"); f.cli_args(&["accounting", "sync"]);
    assert_eq!(f.report()["metrics"]["M10"]["value"], "0/1000", "reported zero is known");
    f.cli_args(&["query", "--metric", "M10", "--json"]); f.cli_args(&["analytics", "refresh"]);
    let revision = f.cli_args(&["analytics", "revisions", "--metric", "M10"]).0["revisions"][0]["revision"].as_i64().unwrap().to_string();
    let pinned = f.cli_args(&["query", "--metric", "M10", "--as-of-seq", &revision, "--json"]).0;
    let mut late: serde_json::Value = serde_json::from_str(fs::read_to_string(&path).unwrap().lines().last().unwrap()).unwrap();
    late["payload"]["response_id"] = json!("late-cache");
    late["payload"]["usage"] = json!({"input_tokens": 100, "cached_input_tokens": 80, "cache_write_input_tokens": 5,
        "output_tokens": 20, "reasoning_output_tokens": 0, "total_tokens": 120});
    writeln!(fs::OpenOptions::new().append(true).open(&path).unwrap(), "{late}").unwrap();
    f.cli("collect");
    assert_eq!(f.report()["metrics"]["M10"]["value"]["reason"], "accounting_sync_required");
    f.cli_args(&["accounting", "sync"]); f.cli_args(&["analytics", "refresh"]);
    let m10 = f.report()["metrics"]["M10"].clone();
    assert_eq!((&m10["value"], &m10["numerator"], &m10["denominator"], &m10["cache_write_tokens"]),
        (&json!("80/1100"), &json!(80), &json!(1100), &json!(5)));
    let again = f.cli_args(&["query", "--metric", "M10", "--as-of-seq", &revision, "--json"]).0;
    for field in ["value", "detail", "source_watermarks"] { assert_eq!(again["results"][0][field], pinned["results"][0][field]); }
    assert_eq!(again["results"][0]["projection"]["content_digest"], pinned["results"][0]["projection"]["content_digest"]);
    assert_eq!(f.cli_args(&["analytics", "rebuild", "--verify"]).0["identical"], true);
    f.sidecar().execute("DELETE FROM accounting_cache_totals", []).unwrap();
    assert_eq!(f.report()["metrics"]["M10"]["value"]["reason"], "accounting_sync_required");
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(f.cli_args(&["accounting", "status"]).0["sync"]["rebuild_reason"], "cache_totals_missing");
    assert_eq!(f.report()["metrics"]["M10"], m10, "public sync repairs missing summaries");
    // Explicitly reported zero input has an empty denominator, never a zero ratio.
    let g = Fixture::new();
    let path = g.rollout(&g.home, "empty-cache", &[RECORD], &g.worktree(), g.decided + 1000, "0.154.0");
    let text = fs::read_to_string(&path).unwrap().replace("\"input_tokens\":1000", "\"input_tokens\":0")
        .replace("\"cached_input_tokens\":200", "\"cached_input_tokens\":0").replace("\"total_tokens\":1300", "\"total_tokens\":300");
    fs::write(path, text).unwrap(); g.cli("collect"); g.cli_args(&["accounting", "sync"]);
    assert_eq!(g.report()["metrics"]["M10"]["value"]["reason"], "empty_denominator");
}

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
/// that basis, certified `live` (run2 §2, B12), and kept in a separate
/// `children` subtotal: the parent stays 100, never 130. A fork (40) of the
/// same parent is linked by `forked_from_id` (`live`), but it names no
/// `history_base` (not the live shape, which replays nothing), so its
/// inclusion and the children subtotal stay `fork_replay_not_certified`,
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
    let spawned = json!({"session_id": SPAWNED, "role": "subagent", "link_basis": "parent_thread_id", "certified": "live", "total_tokens": 30, "inclusion": "separate"});
    let parent = of(&sessions, PARENT);
    assert_eq!((&parent["role"], &parent["linkage"], &parent["parent"], &parent["total_tokens"]), (&json!("primary"), &json!("root"), &json!(null), &json!(100)));
    assert_eq!(parent["children"], json!({"sessions": [spawned], "total_tokens": 30}));
    let child = of(&sessions, SPAWNED);
    assert_eq!((&child["role"], &child["linkage"], &child["parent"], &child["total_tokens"]), (&json!("subagent"), &json!("linked_child"),
        &json!({"session_id": PARENT, "link_basis": "parent_thread_id", "certified": "live"}), &json!(30)));
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
        {"session_id": FORKED, "role": "fork", "link_basis": "forked_from_id", "certified": "live", "total_tokens": 40, "inclusion": not_certified},
        {"session_id": GUARDIAN_SID, "role": "guardian", "link_basis": "thread_parent_thread_id", "certified": "live", "total_tokens": 50, "inclusion": "separate"}],
        "total_tokens": not_certified}));
    let fork = of(&sessions, FORKED);
    assert_eq!((&fork["role"], &fork["linkage"], &fork["parent"], &fork["total_tokens"]), (&json!("fork"), &json!("linked_child"),
        &json!({"session_id": PARENT, "link_basis": "forked_from_id", "certified": "live"}), &json!(40)));
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

    assert_eq!(f.report()["metrics"]["M12"]["value"], unavailable("not_priced"), "before the first reprice");
    assert_eq!(f.cli_args(&["accounting", "reprice"]).0, json!({"revision": 1, "appended": true, "entries": 6, "stored": {"changed": 6, "removed": 0}}));
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

    // M12/M14 through the report (§12): the same partial estimate, never the total; 2 of 6 entries priced.
    let report = f.report();
    let (m12, m14) = (&report["metrics"]["M12"], &report["metrics"]["M14"]);
    let partial = json!({"status": "partial", "reason": "unpriced_entries", "currency": "USD", "priced_amount": "0.0081"});
    assert_eq!((&m12["definition"], &m12["name"], &m12["value"], &m12["estimate"], &m12["basis"], &m12["rate_cards"], &m12["revision"], &m12["never_added_to"]),
        (&json!("M12.cost-v1"), &json!("repriced_estimated_spend"), &partial, &partial, &json!("published_rate_estimate"), &json!("fixture_only"), &json!(1), &json!("M11")));
    assert_eq!(m12["coverage"], json!({"entries": 6, "priced": 2, "unpriced": unpriced}));
    assert_eq!((&m14["definition"], &m14["name"], &m14["value"], &m14["numerator"], &m14["denominator"], &m14["unpriced"], &m14["basis"], &m14["unit"]),
        (&json!("M14.cost-v1"), &json!("cost_coverage"), &json!("2/6"), &json!(2), &json!(6), &unpriced, &json!("published_rate_estimate"), &json!("entries")));
    assert!(f.text(&["report"]).lines().any(|l| l == "M14 cost_coverage 2/6"));

    // As a binary before stream 9 left it (§12): revision 1 a full copy in `valuations`, no delta tables.
    f.sidecar().execute_batch("INSERT INTO valuations SELECT revision,entry_id,session_id,role,attempt_id,model,usage_from_unix_ms,usage_to_unix_ms,
            new_input_tokens,cache_read_tokens,cache_write_tokens,output_tokens,status,reason,card_id,card_version,currency,amount,components FROM valuation_deltas;
        INSERT INTO valuation_bases SELECT revision,entry_id,usage_basis,provider_check FROM valuation_deltas;
        DROP TABLE valuation_deltas; DROP TABLE valuation_delta_revisions; DROP TABLE valuation_inputs;
        UPDATE telemetry_streams SET version=8 WHERE stream='accounting';").unwrap();
    assert_eq!(f.cli_args(&["accounting", "cost", "--json", "--revision", "1"]).1, first, "a full copy reads back as the delta did");
    assert_eq!(f.cli_args(&["accounting", "status"]).0, json!({"stream": "accounting", "version": 8}), "a read does not migrate");

    // A corrected version 3 (output 6) and a EUR card for gpt-5.5-mini append revision 2,
    // stored as the two changed valuations only.
    for name in ["rates-v3.json", "rates-eur.json"] { f.cli_args(&["accounting", "import-rate-card", &rate_card(&f, name, boundary)]); }
    assert_eq!(f.cli_args(&["accounting", "reprice"]).0, json!({"revision": 2, "appended": true, "entries": 6, "stored": {"changed": 2, "removed": 0}}));
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

    // 3 of 6 priced now, in two currencies: M12 is unavailable, never a sum.
    let report = f.report();
    assert_eq!((&report["metrics"]["M12"]["value"], &report["metrics"]["M14"]["value"]),
        (&json!({"status": "unavailable", "reason": "mixed_currency", "priced_by_currency": {"EUR": "0.00021", "USD": "0.0075"}}), &json!("3/6")));

    // Revision 1 (the full copy) is reproduced byte for byte; a re-sync and reprice append nothing;
    // measured tokens never changed.
    assert_eq!(f.cli_args(&["accounting", "cost", "--json", "--revision", "1"]).1, first);
    let (again, second) = f.cli_args(&["accounting", "cost", "--json"]);
    assert_eq!(again["revision"], 2);
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(f.cli_args(&["accounting", "reprice"]).0, json!({"revision": 2, "appended": false, "entries": 6}));
    assert_eq!(f.cli_args(&["accounting", "entries"]).1, measured);
    assert_eq!(f.cli_args(&["accounting", "cost", "--json", "--revision", "2"]).1, second);
    assert_eq!((f.count("valuation_revisions"), f.count("valuations"), f.count("valuation_deltas"), f.count("rate_cards")), (2, 6, 2, 4));
}

/// One ticker run over the fixture root: started, left until `done` holds
/// (its first telemetry pass runs on its first tick), then stopped through its stop file.
fn ticker_pass(f: &Fixture, done: &dyn Fn() -> bool) {
    // The ticker serves the projects of its root that have a PROJECT.md; the
    // store format marker makes it a canonical (state-store) project.
    fs::write(f.project.join("PROJECT.md"), "ticker fixture").unwrap();
    fs::write(f.project.join(".state/format.json"), "{}").unwrap();
    let mut child = Command::new(BIN).env_clear().env("HOME", f.tmp.path().join("home")).env("PATH", "/usr/bin:/bin")
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

/// §12: the ticker's accounting pass syncs the ledger and reprices it only
/// when a rate card exists and the cards or the ledger changed. Version 1
/// (input 2, output 4 per 10^6, effective until 2100) prices doc 10's 1,000
/// input + 500 output at 0.004: M12 complete `0.004` USD, M14 `1/1`. A pass
/// with nothing changed syncs again but does not reprice. Version 2 (input
/// 2, cache read 0.5, output 8, effective from 0) → 1,000 × 2 + 500 × 8 =
/// 0.006: revision 2, stored as the one changed valuation.
#[test]
fn ticker_reprices_only_when_inputs_change() {
    let f = Fixture::new();
    f.rollout(&f.home, "before", &[&format!("{ACCOUNTING}/priced-before.jsonl")], &f.worktree(), f.decided, "0.154.0");
    f.cli("collect");
    let synced = || f.sidecar().query_row("SELECT synced_unix_ms FROM usage_ledger", [], |r| r.get::<_, i64>(0)).ok();
    let revisions = || f.count("valuation_revisions");
    // No rate card yet: the pass syncs the ledger and appends no revision.
    ticker_pass(&f, &|| synced().is_some());
    assert_eq!((revisions(), f.report()["metrics"]["M12"]["value"].clone()), (0, json!({"status": "unavailable", "reason": "not_priced"})));

    f.cli_args(&["accounting", "import-rate-card", &rate_card(&f, "rates-v1.json", 4_102_444_800_000)]);
    ticker_pass(&f, &|| revisions() == 1);
    let report = f.report();
    let (m12, m14) = (&report["metrics"]["M12"], &report["metrics"]["M14"]);
    assert_eq!((&m12["value"], &m12["currency"], &m12["estimate"], &m12["basis"]),
        (&json!("0.004"), &json!("USD"), &json!({"status": "complete", "currency": "USD", "amount": "0.004"}), &json!("published_rate_estimate")));
    assert_eq!((&m14["value"], &m14["unpriced"]), (&json!("1/1"), &json!({})));
    assert!(f.text(&["report"]).lines().any(|l| l == "M12 repriced_estimated_spend 0.004"));
    let recorded = || f.sidecar().query_row("SELECT digest,revision,recorded_unix_ms FROM valuation_inputs", [], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))).unwrap();
    let first = recorded();

    // Nothing changed: the next pass syncs again but does not reprice.
    let before = synced();
    ticker_pass(&f, &|| synced() != before);
    assert_eq!((revisions(), recorded()), (1, first.clone()));

    // A new card version changes the inputs: revision 2, one changed valuation stored.
    f.cli_args(&["accounting", "import-rate-card", &rate_card(&f, "rates-v2.toml", 0)]);
    ticker_pass(&f, &|| revisions() == 2);
    assert_eq!(f.report()["metrics"]["M12"]["value"], "0.006");
    assert_eq!(f.sidecar().query_row("SELECT changed,removed FROM valuation_delta_revisions WHERE revision=2", [], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))).unwrap(), (1, 0));
    assert_ne!(recorded().0, first.0);
    assert_eq!(recorded().1, 2);
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
    assert_eq!(f.cli_args(&["accounting", "reprice"]).0, json!({"revision": 1, "appended": true, "entries": 5, "stored": {"changed": 5, "removed": 0}}));
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
    // Stored as a delta: TIMED 2 and UNVERIFIED 1 changed card; the other three are unchanged.
    assert_eq!(f.cli_args(&["accounting", "reprice"]).0, json!({"revision": 2, "appended": true, "entries": 5, "stored": {"changed": 2, "removed": 0}}));
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
    assert_eq!((f.count("valuation_revisions"), f.count("valuations"), f.count("valuation_deltas")), (2, 0, 7));
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
    let path = f.home.join(".codex/sessions/2026/09/28/rollout-2026-09-28T00-00-00-secondary.jsonl");
    let text = fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    fs::write(&path, lines[..3].join("\n") + "\n").unwrap();
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    // Each ordered snapshot extends the windows from the preceding pass.
    for line in &lines[3..] {
        use std::io::Write;
        writeln!(fs::OpenOptions::new().append(true).open(&path).unwrap(), "{line}").unwrap();
        f.cli("collect");
        f.cli_args(&["accounting", "sync"]);
    }
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

    f.sidecar().execute("DELETE FROM usage_ledger", []).unwrap();
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(f.cli_args(&["accounting", "quota", "--json"]).1, first, "ordered incremental windows equal a full replay");
    // Fixture only: snapshots stored before A4 have no secondary row.
    f.sidecar().execute_batch("DELETE FROM codex_rate_limit_windows").unwrap();
    f.cli_args(&["accounting", "sync"]);
    let (quota, _) = f.cli_args(&["accounting", "quota", "--json"]);
    assert_eq!(quota["observations"], json!({"primary": {"trusted": 4}}));
    assert_eq!(headroom(&f)["windows"][1], json!({"limit_id": "codex", "window_kind": "secondary", "value": {"status": "unavailable", "reason": "not_collected"}}));
    use std::io::Write;
    let late = lines[3].replace(&jiff::Timestamp::from_millisecond(t2).unwrap().to_string(),
        &jiff::Timestamp::from_millisecond(d - 3_600_000).unwrap().to_string());
    writeln!(fs::OpenOptions::new().append(true).open(&path).unwrap(), "{late}").unwrap();
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    let late_view = f.cli_args(&["accounting", "quota", "--json"]).1;
    f.sidecar().execute("DELETE FROM usage_ledger", []).unwrap();
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(f.cli_args(&["accounting", "quota", "--json"]).1, late_view, "late quota observations replay in native order");

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

/// Codex-live run 2 §6 (B12): the live `codex` primary window (10,080 min,
/// 43%) reported `resets_at` R in most snapshots but R + 5 s in one, and
/// `plan_type: null` in the `exec` sessions. Home A reads 43 (R, `pro`) → 43
/// (R + 5 s, `pro`) → 44 (R − 5 s, `null`) → 45.5 (R, `null`): one window
/// (`first_observation`, 4 trusted observations, increase 2.5, remaining
/// 54.5, plan `pro`), never `reset_moved` or `window_regressed`. Home B's one
/// snapshot at R + 5 s is the same provider window within the 60 s tolerance:
/// one shared-window candidate (reset R, the earliest), never merged. A's
/// headroom 54.5 is its own. Then A reads 46 at R + 120 s (beyond the
/// tolerance, before R: `reset_moved`, a second window) and 47 back at R
/// (`window_regressed`, no window): headroom 54 from the new window.
#[test]
fn resets_jitter_and_null_plan_stay_one_window() {
    let f = Fixture::new();
    let d = f.decided;
    let other = fs::canonicalize(f.tmp.path()).unwrap().join("other-home");
    let r = d / 1000 + 3_600;
    quota_rollout(&f, "jitter", "quota-jitter.jsonl", d - 240_000, &[d - 240_000, d - 180_000, d - 120_000, d - 60_000], &[r, r + 5, r - 5, r]);
    quota_rollout_in(&f, &other, "jitter-child", "quota-jitter-child.jsonl", d - 90_000, &[d - 90_000], &[r + 5]);
    f.cli("collect");
    assert_eq!(f.cli_args(&["accounting", "sync"]).0["quota_windows"], 2);
    let (quota, _) = f.cli_args(&["accounting", "quota", "--json"]);
    let (a, b) = (digest(&f.home), digest(&other));
    let id = |account: &str, resets: i64| format!("codex:{account}:codex:primary:{}", resets * 1000);
    let window = |account: &str, resets: i64, evidence: &str, (first, last): (i64, i64), [first_used, used, remaining, increase]: [&str; 4], plan: Option<&str>,
        observations: i64| json!({"window_id": id(account, resets), "service": "codex", "account": account, "limit_id": "codex", "window_kind": "primary",
            "unit": "percent", "window_minutes": 10_080, "window_start_unix_ms": resets * 1000 - 604_800_000, "resets_unix_ms": resets * 1000,
            "start_evidence": evidence, "first_observed_unix_ms": first, "last_observed_unix_ms": last, "first_used": first_used, "used": used,
            "remaining": remaining, "observed_increase": increase, "plan_type": plan, "observations": observations, "flagged": 0});
    let of = |quota: &serde_json::Value, account: &str| quota["windows"].as_array().unwrap().iter().filter(|w| w["account"] == account).cloned().collect::<Vec<_>>();
    let a_window = window(&a, r, "first_observation", (d - 240_000, d - 60_000), ["43", "45.5", "54.5", "2.5"], Some("pro"), 4);
    assert_eq!(of(&quota, &a), std::slice::from_ref(&a_window));
    assert_eq!(of(&quota, &b), [window(&b, r + 5, "first_observation", (d - 90_000, d - 90_000), ["43", "43", "57", "0"], Some("pro"), 1)]);
    assert_eq!(quota["observations"], json!({"primary": {"trusted": 5}, "secondary": {"not_reported": 5}}));
    // Every jittered snapshot of A is trusted in its one window, keeping its own reported reset.
    let observed: Vec<(i64, String, String)> = f.sidecar().prepare("SELECT resets_unix_ms,trust,window_id FROM quota_window_observations
        WHERE account=?1 AND window_kind='primary' ORDER BY observed_unix_ms").unwrap()
        .query_map([&a], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).unwrap().map(Result::unwrap).collect();
    assert_eq!(observed, [r, r + 5, r - 5, r].map(|resets| (resets * 1000, "trusted".to_owned(), id(&a, r))));
    let mut members = [(a.clone(), id(&a, r)), (b.clone(), id(&b, r + 5))];
    members.sort();
    assert_eq!(quota["shared_window_candidates"], json!([{"limit_id": "codex", "window_kind": "primary", "window_minutes": 10_080, "resets_unix_ms": r * 1000,
        "accounts": members.clone().map(|m| m.0), "window_ids": members.clone().map(|m| m.1), "evidence": "same_limit_kind_minutes_resets", "merged": false}]));
    let primary = headroom(&f)["windows"][0].clone();
    assert_eq!(primary, json!({"limit_id": "codex", "window_kind": "primary", "unit": "percent", "window_id": id(&a, r), "window_minutes": 10_080,
        "resets_unix_ms": r * 1000, "observed_unix_ms": d - 60_000, "age_ms": 60_000, "shared_window_candidates": members.clone().map(|m| m.0),
        "value": "54.5", "used": "45.5", "freshness": "fresh"}));

    // Beyond the tolerance: a moved reset opens a window; going back to R regresses.
    quota_rollout(&f, "moved", "quota-moved.jsonl", d - 30_000, &[d - 30_000, d - 20_000], &[r + 120, r]);
    f.cli("collect");
    assert_eq!(f.cli_args(&["accounting", "sync"]).0["quota_windows"], 3);
    let (quota, first) = f.cli_args(&["accounting", "quota", "--json"]);
    assert_eq!(of(&quota, &a), [a_window, window(&a, r + 120, "reset_moved", (d - 30_000, d - 30_000), ["46", "46", "54", "0"], None, 1)]);
    assert_eq!(quota["observations"], json!({"primary": {"trusted": 6, "window_regressed": 1}, "secondary": {"not_reported": 7}}));
    let primary = headroom(&f)["windows"][0].clone();
    assert_eq!((&primary["window_id"], &primary["value"], &primary["age_ms"], primary.get("shared_window_candidates")),
        (&json!(id(&a, r + 120)), &json!("54"), &json!(30_000), None));
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
    assert_eq!(cli(&["accounting", "status"]).0, json!({"stream": "accounting", "version": 16}));
    // Stream 8 dropped the superseded projections (v2, v4, v6); their replacements stay.
    let tables: Vec<String> = rusqlite::Connection::open(project.join(".state/telemetry.db")).unwrap()
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name IN ('session_graph','quota_observations','session_nodes','session_graph_nodes','quota_window_observations') ORDER BY name").unwrap()
        .query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
    assert_eq!(tables, ["quota_window_observations", "session_graph_nodes"]);
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
/// unattributed. Outcomes: exit 0 ×3 succeeded, exit 2 failed, `failed` with
/// exit 1 failed (certified live, B12), a NULL exit code unknown → M17 3/5. M18 has
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
        "outputs_without_call": 1, "executed": 6, "attributed": 5, "unattributed": 1, "mcp_calls": 0, "succeeded": 3, "failed": 2, "unknown": 1,
        "declined_or_aborted": 0}}]));

    let m16 = &tools["metrics"]["M16"];
    assert_eq!((&m16["definition"], &m16["name"]), (&json!("M16.tools-v1"), &json!("tool_call_volume")));
    // No attention sample and no guardian: no call is inferred accepted; all 5 stay unknown, never accepted.
    assert_eq!(m16["value"], json!({"issued": 5, "accepted": {"status": "inferred", "count": 0, "unknown": 5, "declined_or_aborted": 0}, "executed": 6}));
    assert_eq!((&m16["accepted"]["label"], &m16["accepted"]["by_basis"], &m16["accepted"]["unknown"]), (&json!("inferred"), &json!({}), &json!(5)));
    let issued = &m16["issued"];
    assert_eq!((&issued["calls"], &issued["by_name"], &issued["name_unreported"], &issued["by_status"], &issued["status_unreported"],
        &issued["without_output"], &issued["outputs_without_call"]),
        (&json!(5), &json!({"exec": 4, "wait": 1}), &json!(0), &json!({"completed": 4}), &json!(1), &json!(1), &json!(1)));
    let executed = &m16["executed"];
    assert_eq!((&executed["executions"], &executed["scope"], &executed["by_source"], &executed["source_unreported"]),
        (&json!(6), &json!(["command_execution", "mcp"]), &json!({"unified_exec_startup": 6}), &json!(0)));
    let attribution = &executed["attribution"];
    assert_eq!((&attribution["basis"], &attribution["by_call_name"], &attribution["name_unreported"], &attribution["unattributed"]),
        (&json!("inferred"), &json!({"exec": 5}), &json!(0), &json!(1)));
    assert_eq!((&executed["by_scope"], &m16["mcp"]["calls"]), (&json!({"command_execution": 6, "mcp": 0}), &json!(0)));
    assert_eq!((&m16["certified"]["mcp_calls"], &m16["coverage"]), (&json!("live"), &coverage));

    let m17 = &tools["metrics"]["M17"];
    assert_eq!((&m17["value"], &m17["numerator"], &m17["denominator"], &m17["succeeded"], &m17["failed"]),
        (&json!("3/5"), &json!(3), &json!(5), &json!(3), &json!(2)));
    assert_eq!(m17["unknown"], json!({"executions": 1, "by_reason": {"exit_code_unknown": 1}}));
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
    // Per host: both rollouts of the session lie under the one execution home.
    assert_eq!((&wall["by_home"], &wall["home_ambiguous"], &wall["host_basis"]),
        (&json!({digest(&f.home): {"samples": 4, "p50_ms": 1000, "p95_ms": 37010, "max_ms": 37010}}),
         &json!({"samples": 0, "p50_ms": null, "p95_ms": null, "max_ms": null}), &json!("execution_home")));

    // The report takes the lane's M16–M18.
    let report = f.report();
    for id in ["M16", "M17", "M18"] { assert_eq!(report["metrics"][id], tools["metrics"][id], "{id}"); }
    let text = f.text(&["accounting", "tools"]);
    for line in ["coverage 1 sessions: 1 observed, 0 pending_reread, 0 predates_collection; excluded unbound 1".to_owned(),
        format!("session {TOOLS_SID} attempts={}: issued 5 (1 without output, 1 outputs without call), executed 6 (5 inferred to a call, 1 unattributed), succeeded 3 failed 2 unknown 1", f.attempt),
        "M16 tool_call_volume issued 5, accepted 0 inferred (5 unknown), executed 6".to_owned(),
        "M17 tool_execution_success 3/5 (unknown 1 excluded, pending calls 1)".to_owned(),
        "M18 tool_latency_p95 n/a (execution_duration_not_exposed)".to_owned(),
        "call_to_output_ms p95 37010 of 4 calls (includes approval wait; not execution time)".to_owned()] {
        assert!(text.lines().any(|l| l == line), "{line} in {text}");
    }
    // F5 (certificate-live.md §5): the report's and the query's text forms
    // print M16's counts as the JSON has them, never `n/a (unknown)`.
    let report = f.text(&["report", "--text"]);
    assert!(report.lines().any(|l| l == "M16 tool_call_volume issued 5, accepted 0 inferred (5 unknown), executed 6"), "{report}");
    let query = f.text(&["query", "--metric", "M16"]);
    assert!(query.starts_with("M16 tool_call_volume M16.tools-v1 activity_window issued 5, accepted 0 inferred (5 unknown), executed 6 "), "{query}");

    // Read-only and replayable: a second collect changes nothing.
    f.cli("collect");
    assert_eq!(f.cli_args(&["accounting", "tools", "--json"]).1, first);
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(f.cli_args(&["accounting", "tools", "--json"]).1, first,
        "the maintained session summaries preserve every count, inference and percentile");
    f.cli_args(&["analytics", "refresh"]);
    assert_eq!(f.cli_args(&["analytics", "rebuild", "--verify"]).0["identical"], true);
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

/// 2030-01-01T00:00:00Z, the base of the tool rollouts' literal line times.
const Y2030: i64 = 1_893_456_000_000;
/// Session id written literally in `tools-guardian.jsonl`.
const TOOLS_GUARDIAN: &str = "00000000-0000-4000-8000-0000000b5003";

/// §9 follow-up: the accepted stage of M16 inferred, labelled `inferred`. The
/// tool session of `tool_volume_success_and_latency_are_honest` (calls at
/// seconds 10→47.01, 50→51, 52→54.5, 55→55.3 and 60 without output). A
/// guardian session (live shape: `guardian_review`, naming the session by
/// thread lineage) starting at 55.1 lies inside call-4 → `auto_review`. The
/// attempt's attention samples (working 0 s, blocked 30 s, working 60 s) give
/// one `blocked` wait 30–30 inside call-1 → `human_routed`. Calls 2, 3 (no
/// wait, no guardian) and 5 (no output) stay unknown: accepted 2 of 5, never 5.
#[test]
fn accepted_stage_is_inferred_from_waits_and_guardians() {
    let f = Fixture::new();
    let at = f.decided + 1_000;
    let part = |name: &str| format!("{ACCOUNTING}/{name}");
    f.rollout(&f.home, "a", &[&part("tools.jsonl")], &f.worktree(), at, "0.154.0");
    f.rollout(&f.home, "b", &[&part("tools.jsonl"), &part("tools-resume.jsonl")], &f.worktree(), at, "0.154.0");
    f.rollout(&f.home, "g", &[&part("tools-guardian.jsonl")], &f.worktree(), Y2030 + 55_100, "0.154.0");
    f.cli("collect");

    // The guardian alone: call-4 is auto-reviewed, the other four unknown.
    let (tools, _) = f.cli_args(&["accounting", "tools", "--json"]);
    assert_eq!(tools["coverage"], json!({"sessions": 2, "observed": 2, "pending_reread": 0, "predates_collection": 0, "excluded": {}}));
    assert_eq!(tools["metrics"]["M16"]["value"], json!({"issued": 5, "accepted": {"status": "inferred", "count": 1, "unknown": 4, "declined_or_aborted": 0}, "executed": 6}));
    assert_eq!(tools["metrics"]["M16"]["accepted"]["by_basis"], json!({"auto_review": 1}));

    // The attempt's launch receipt (as its start records it) and three attention
    // samples 30 s apart, planted as an observation pass would write them.
    let state = rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap();
    let receipt = json!({"version": 2, "attempt": f.attempt, "operation": "op-tools", "route": {"machine": "", "socket": "/nonexistent/herdr.sock",
        "workspace_id": "w1", "tab_id": "w1:t1", "pane_id": "w1:p1", "cwd": f.worktree()}, "terminal": "term-1",
        "session": {"device": 1, "inode": 2, "born_secs": 3, "born_nanos": 4}, "agent": {"kind": "codex", "name": "worker"}, "observed_unix_ms": Y2030});
    state.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    state.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('runtime.launch_started','op-tools',1,1,?1)", [receipt.to_string()]).unwrap();
    for (ms, label) in [(0, "working"), (30_000, "blocked"), (60_000, "working")] {
        f.sidecar().execute("INSERT INTO attention_samples(attempt_id,observed_unix_ms,state,gap,interval_ms,source) VALUES(?1,?2,?3,NULL,30000,'herdr-agent-list-v1')",
            rusqlite::params![f.attempt, Y2030 + ms, label]).unwrap();
    }
    let (attention, _) = f.cli_args(&["accounting", "attention", "--json"]);
    assert_eq!(attention["attempts"][0]["attention"]["intervals"][0]["opened_unix_ms"], Y2030 + 30_000);

    let (tools, first) = f.cli_args(&["accounting", "tools", "--json"]);
    let m16 = &tools["metrics"]["M16"];
    assert_eq!(m16["value"], json!({"issued": 5, "accepted": {"status": "inferred", "count": 2, "unknown": 3, "declined_or_aborted": 0}, "executed": 6}));
    assert_eq!((&m16["accepted"]["label"], &m16["accepted"]["calls"], &m16["accepted"]["by_basis"], &m16["accepted"]["unknown"]),
        (&json!("inferred"), &json!(2), &json!({"auto_review": 1, "human_routed": 1}), &json!(3)));
    // Per host: every sample is of the one execution home.
    assert_eq!(tools["metrics"]["M18"]["call_to_output_ms"]["by_home"], json!({digest(&f.home): {"samples": 4, "p50_ms": 1000, "p95_ms": 37010, "max_ms": 37010}}));
    let report = f.report();
    for id in ["M16", "M17", "M18"] { assert_eq!(report["metrics"][id], tools["metrics"][id], "{id}"); }
    assert!(f.text(&["accounting", "tools"]).lines().any(|l| l == "M16 tool_call_volume issued 5, accepted 2 inferred (3 unknown), executed 6"));
    // Read-only and replayable.
    f.cli("collect");
    assert_eq!(f.cli_args(&["accounting", "tools", "--json"]).1, first);
    assert_eq!(TOOLS_GUARDIAN, tools["sessions"][1]["session_id"]);
}

/// The live run 2 conformance fixtures (codex-live-0.154.0-run2.md): its tool
/// session and its `codex exec fork`, with their session ids.
const LIVE2_TOOLS: &str = "../codex-conformance/live2-tools.jsonl";
const LIVE2_FORK: &str = "../codex-conformance/live2-fork.jsonl";
const LIVE2_SID: &str = "00000000-0000-4000-8000-0000000b2001";
const LIVE2_FORK_SID: &str = "00000000-0000-4000-8000-0000000b2002";

/// Plant a live-2 conformance rollout as session `sid`, bound to the attempt;
/// a fork's `@ORIGIN@` becomes `SID` and `@ORIGIN_END@` the planted origin's length.
fn plant_live2(f: &Fixture, name: &str, fixture: &str, sid: &str, origin: Option<&Path>) -> std::path::PathBuf {
    let mut text = fs::read_to_string(Path::new(FIXTURES).join(fixture)).unwrap().replace("@SID@", sid).replace("@ORIGIN@", SID);
    if let Some(origin) = origin { text = text.replace("@ORIGIN_END@", &fs::metadata(origin).unwrap().len().to_string()); }
    let path = f.tmp.path().join(format!("{name}.jsonl"));
    fs::write(&path, text).unwrap();
    f.rollout(&f.home, name, &[path.to_str().unwrap()], &f.worktree(), f.decided + 1_000, "0.154.0")
}

/// Live run 2 (§3–§4) → B12 over the `live2-tools` shapes: calls m1 (6.7 →
/// 6.765 s, carrying MCP item exec-m1), s1 (12.673 → 15.733, `sleep`,
/// completed exit 0), f1 (17.494 → 17.625, `failed` exit 2), spawn_agent c1
/// (20 → 20.086) and wait_agent c2 (27 → 28.078) in namespace
/// `collaboration`, then turn 2's d1 (71.5 → 80.243) whose declined approval
/// aborted the turn at 80.248. M16: 6 issued (the MCP call once, with its
/// exec call), accepted 0, unknown 5, d1 `declined_or_aborted`; executed 3
/// (2 command executions, 1 MCP call). M17 `2/3`: command 1/2 (the `failed`
/// item is a certified failure), MCP 1/1 (`is_error` false, completed). M18
/// stays unavailable (the MCP duration is not run time); call → output 65,
/// 3060, 131, 86, 1078, 8743 ms → p50 131, p95 8743; the MCP call's carrier
/// 65 ms apart. A `blocked` wait inside d1 does not make it accepted, while
/// one inside s1 does (`human_routed`). Without the A8 tables (read-only) or
/// before a source's re-read, M16–M18 are unavailable, never partial.
#[test]
fn live_run2_failures_aborts_and_mcp_calls_count_once() {
    let f = Fixture::new();
    let unavailable = |reason: &str| json!({"status": "unavailable", "reason": reason});
    plant_live2(&f, "live2-tools", LIVE2_TOOLS, LIVE2_SID, None);
    f.cli("collect");
    let (tools, fresh) = f.cli_args(&["accounting", "tools", "--json"]);
    let coverage = json!({"sessions": 1, "observed": 1, "pending_reread": 0, "predates_collection": 0, "excluded": {}});
    assert_eq!(tools["coverage"], coverage);
    assert_eq!(tools["sessions"], json!([{"session_id": LIVE2_SID, "attempt_ids": [f.attempt], "tools": {"issued": 6, "without_output": 0,
        "outputs_without_call": 0, "executed": 3, "attributed": 2, "unattributed": 0, "mcp_calls": 1, "succeeded": 2, "failed": 1, "unknown": 0,
        "declined_or_aborted": 1}}]));

    let m16 = &tools["metrics"]["M16"];
    assert_eq!(m16["value"], json!({"issued": 6, "accepted": {"status": "inferred", "count": 0, "unknown": 5, "declined_or_aborted": 1}, "executed": 3}));
    let issued = &m16["issued"];
    assert_eq!((&issued["calls"], &issued["by_name"], &issued["by_status"], &issued["status_unreported"], &issued["by_namespace"], &issued["mcp_without_call"]),
        (&json!(6), &json!({"exec": 4, "spawn_agent": 1, "wait_agent": 1}), &json!({"completed": 4}), &json!(2), &json!({"collaboration": 2}), &json!(0)));
    assert_eq!(m16["accepted"]["declined_or_aborted"], json!({"calls": 1, "by_basis": {"last_output_before_abort": 1}}));
    let executed = &m16["executed"];
    assert_eq!((&executed["executions"], &executed["by_scope"], &executed["by_source"], &executed["attribution"]["by_call_name"], &executed["attribution"]["unattributed"]),
        (&json!(3), &json!({"command_execution": 2, "mcp": 1}), &json!({"unified_exec_startup": 2}), &json!({"exec": 2}), &json!(0)));
    assert_eq!((&m16["mcp"]["calls"], &m16["mcp"]["by_server"], &m16["mcp"]["server_or_tool_unreported"], &m16["mcp"]["carrier"]["matched"],
        &m16["mcp"]["carrier"]["unmatched"], &m16["mcp"]["carrier"]["basis"]),
        (&json!(1), &json!({"live2_stub_server": {"live2_noop_tool": 1}}), &json!(0), &json!(1), &json!(0), &json!("inferred")));
    assert_eq!((&m16["collaboration"]["calls"], &m16["collaboration"]["spawned_threads"], &m16["collaboration"]["collab_items"]), (&json!(2), &json!(1), &json!(1)));
    assert_eq!(m16["certified"], json!({"calls": "live", "call_status": "live for custom_tool_call, fixture for function_call", "exec_items": "live",
        "mcp_calls": "live", "turn_aborts": "live", "namespaces": "live"}));

    let m17 = &tools["metrics"]["M17"];
    let none = json!({"executions": 0, "by_reason": {}});
    assert_eq!((&m17["value"], &m17["numerator"], &m17["denominator"], &m17["unknown"], &m17["pending_calls"]),
        (&json!("2/3"), &json!(2), &json!(3), &none, &json!(0)));
    assert_eq!(m17["by_scope"], json!({"command_execution": {"succeeded": 1, "failed": 1, "unknown": none}, "mcp": {"succeeded": 1, "failed": 0, "unknown": none}}));

    let m18 = &tools["metrics"]["M18"];
    assert_eq!((&m18["value"], &m18["mcp_duration"]["value"]), (&unavailable("execution_duration_not_exposed"), &unavailable("execution_duration_not_exposed")));
    let wall = &m18["call_to_output_ms"];
    let dist = |samples: i64, p50: i64, p95: i64, max: i64| json!({"samples": samples, "p50_ms": p50, "p95_ms": p95, "max_ms": max});
    assert_eq!((&wall["samples"], &wall["p50_ms"], &wall["p95_ms"], &wall["max_ms"]), (&json!(6), &json!(131), &json!(8743), &json!(8743)));
    assert_eq!(wall["by_name"], json!({"exec": dist(4, 131, 8743, 8743), "spawn_agent": dist(1, 86, 86, 86), "wait_agent": dist(1, 1078, 1078, 1078)}));
    let mcp = &wall["mcp"];
    assert_eq!((&mcp["samples"], &mcp["p50_ms"], &mcp["p95_ms"], &mcp["max_ms"], &mcp["by_server"]),
        (&json!(1), &json!(65), &json!(65), &json!(65), &json!({"live2_stub_server": {"live2_noop_tool": dist(1, 65, 65, 65)}})));
    let report = f.report();
    for id in ["M16", "M17", "M18"] { assert_eq!(report["metrics"][id], tools["metrics"][id], "{id}"); }
    let text = f.text(&["accounting", "tools"]);
    for line in ["M16 tool_call_volume issued 6, accepted 0 inferred (5 unknown), executed 3",
        "M16 mcp_calls 1 [live2_stub_server/live2_noop_tool 1] (counted once with their exec call), declined_or_aborted 1",
        "M17 tool_execution_success 2/3 (unknown 0 excluded, pending calls 0)",
        "M17 by scope: command_execution 1 succeeded 1 failed 0 unknown; mcp 1 succeeded 0 failed 0 unknown"] {
        assert!(text.lines().any(|l| l == line), "{line} in {text}");
    }

    // A `blocked` wait inside s1 (13–13 s) and one inside d1 (75–75 s), planted as an observation pass would.
    let state = rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap();
    let receipt = json!({"version": 2, "attempt": f.attempt, "operation": "op-live2", "route": {"machine": "", "socket": "/nonexistent/herdr.sock",
        "workspace_id": "w1", "tab_id": "w1:t1", "pane_id": "w1:p1", "cwd": f.worktree()}, "terminal": "term-1",
        "session": {"device": 1, "inode": 2, "born_secs": 3, "born_nanos": 4}, "agent": {"kind": "codex", "name": "worker"}, "observed_unix_ms": Y2030});
    state.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    state.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('runtime.launch_started','op-live2',1,1,?1)", [receipt.to_string()]).unwrap();
    for (ms, label) in [(0, "working"), (13_000, "blocked"), (14_000, "working"), (75_000, "blocked"), (76_000, "working")] {
        f.sidecar().execute("INSERT INTO attention_samples(attempt_id,observed_unix_ms,state,gap,interval_ms,source) VALUES(?1,?2,?3,NULL,30000,'herdr-agent-list-v1')",
            rusqlite::params![f.attempt, Y2030 + ms, label]).unwrap();
    }
    let (tools, waited) = f.cli_args(&["accounting", "tools", "--json"]);
    let m16 = &tools["metrics"]["M16"];
    assert_eq!(m16["value"], json!({"issued": 6, "accepted": {"status": "inferred", "count": 1, "unknown": 4, "declined_or_aborted": 1}, "executed": 3}));
    assert_eq!((&m16["accepted"]["by_basis"], &m16["accepted"]["declined_or_aborted"]["by_basis"]),
        (&json!({"human_routed": 1}), &json!({"last_output_before_abort": 1})));

    // A read-only sidecar without the A8 tables: unavailable, never a count without MCP calls or aborts.
    f.sidecar().execute_batch("DROP TABLE rollout_forks; DROP TABLE rollout_turn_ends; DROP TABLE codex_turn_aborts; DROP TABLE codex_mcp_calls;
        DROP TABLE codex_agent_items; DROP TABLE codex_tool_namespaces; DROP TABLE codex_fork_reconciliation;
        UPDATE telemetry_streams SET version=7 WHERE stream='ingest';").unwrap();
    let (tools, _) = f.cli_args(&["accounting", "tools", "--json"]);
    let coverage = json!({"sessions": 1, "observed": 0, "pending_reread": 0, "predates_collection": 1, "excluded": {}});
    assert_eq!((&tools["coverage"], &tools["sessions"][0]["tools"]), (&coverage, &unavailable("predates_collection")));
    for id in ["M16", "M17", "M18"] { assert_eq!(tools["metrics"][id]["value"], unavailable("predates_collection"), "{id}"); }
    assert_eq!(f.cli_args(&["collectors", "status"]).0, json!({"stream": "ingest", "version": 7}), "a read does not migrate");
    // The next collect migrates and re-reads the rollout: identical to before.
    f.cli("collect");
    assert_eq!(f.cli_args(&["accounting", "tools", "--json"]).1, waited);
    // A source read before A8 (no `rollout_forks` row) waits for its re-read.
    f.sidecar().execute_batch("DELETE FROM rollout_forks").unwrap();
    let (tools, _) = f.cli_args(&["accounting", "tools", "--json"]);
    assert_eq!((&tools["coverage"]["pending_reread"], &tools["metrics"]["M17"]["value"]), (&json!(1), &unavailable("pending_reread")));
    f.cli("collect");
    assert_eq!(f.cli_args(&["accounting", "tools", "--json"]).1, waited);
    assert_ne!(waited, fresh);
}

/// Live run 2 §1 → B12: a `codex exec fork` (`live2-fork`, own record 320)
/// of a collected origin (`complete`, 1680) names `history_base`, the live
/// shape that replays no records, so it is a `linked_child` (`forked_from_id`,
/// `live`) with inclusion `separate` and its reconciliation states: the
/// origin stays 1680, children 320, rollup 1680 / 320 / 0, never 1680 + 2000
/// (the fork's reported thread total, which includes the origin's). Before the
/// origin is collected the fork is `parent_not_collected`. Without the A8
/// tables (read-only) its inclusion, the children total and the linked rollup
/// are `predates_collection`; before its re-read `pending_reread`.
#[test]
fn live_fork_is_separate_and_never_adds_its_reported_totals() {
    let f = Fixture::new();
    let origin = f.tmp.path().join("origin.jsonl");
    let text: String = ["head.jsonl", "tail.jsonl"].iter().map(|p| fs::read_to_string(Path::new(FIXTURES).join(p)).unwrap()).collect();
    fs::write(&origin, text).unwrap();
    // The origin's length as it will be planted (same placeholders substituted), then held back.
    let planted = f.rollout(&f.home, "complete", &[origin.to_str().unwrap()], &f.worktree(), f.decided + 1_000, "0.154.0");
    let held = f.tmp.path().join("complete.held");
    fs::rename(&planted, &held).unwrap();
    plant_live2(&f, "live2-fork", LIVE2_FORK, LIVE2_FORK_SID, Some(&held));
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    let (sessions, _) = f.cli_args(&["accounting", "sessions"]);
    let of = |sessions: &serde_json::Value, sid: &str| sessions["sessions"].as_array().unwrap().iter().find(|s| s["session_id"] == sid).cloned()
        .unwrap_or_else(|| panic!("{sid} in {sessions}"));
    let fork = of(&sessions, LIVE2_FORK_SID);
    assert_eq!((&fork["role"], &fork["linkage"], &fork["parent"], &fork["total_tokens"]), (&json!("fork"), &json!("unlinked_child"),
        &json!({"status": "unavailable", "reason": "parent_not_collected", "session_id": SID}), &json!(320)));
    assert_eq!(sessions["rollup"], json!({"sessions": 0, "linked_children": 0, "unlinked_children": 320, "incomplete_sessions": 0}));

    fs::rename(&held, &planted).unwrap();
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    let (sessions, linked) = f.cli_args(&["accounting", "sessions"]);
    let parent = json!({"session_id": SID, "link_basis": "forked_from_id", "certified": "live"});
    let fork = of(&sessions, LIVE2_FORK_SID);
    assert_eq!((&fork["role"], &fork["linkage"], &fork["parent"], &fork["total_tokens"]), (&json!("fork"), &json!("linked_child"), &parent, &json!(320)));
    let child = |inclusion: serde_json::Value, reconciliation: Option<serde_json::Value>| {
        let mut c = json!({"session_id": LIVE2_FORK_SID, "role": "fork", "link_basis": "forked_from_id", "certified": "live", "total_tokens": 320, "inclusion": inclusion});
        if let Some(r) = reconciliation { c["fork_reconciliation"] = r; }
        c
    };
    let origin_node = of(&sessions, SID);
    assert_eq!((&origin_node["role"], &origin_node["total_tokens"]), (&json!("primary"), &json!(1680)));
    assert_eq!(origin_node["children"], json!({"sessions": [child(json!("separate"), Some(json!({"thread_total": "reconciled", "token_count_total": "reconciled"})))],
        "total_tokens": 320}));
    assert_eq!(sessions["rollup"], json!({"sessions": 1680, "linked_children": 320, "unlinked_children": 0, "incomplete_sessions": 0}), "never 1680 + 2000");
    // Each record once: 1500 + 300 input, 180 + 20 output.
    let report = f.report();
    assert_eq!((&report["metrics"]["M08"]["value"], &report["metrics"]["M09"]["value"]), (&json!(1800), &json!(200)));

    // A read-only sidecar without the A8 tables: the fork's inclusion is unknown, so no linked sum.
    f.sidecar().execute_batch("DROP TABLE rollout_forks; DROP TABLE rollout_turn_ends; DROP TABLE codex_turn_aborts; DROP TABLE codex_mcp_calls;
        DROP TABLE codex_agent_items; DROP TABLE codex_tool_namespaces; DROP TABLE codex_fork_reconciliation;
        UPDATE telemetry_streams SET version=7 WHERE stream='ingest';").unwrap();
    let predates = json!({"status": "unavailable", "reason": "predates_collection"});
    let (sessions, _) = f.cli_args(&["accounting", "sessions"]);
    assert_eq!(of(&sessions, SID)["children"], json!({"sessions": [child(predates.clone(), None)], "total_tokens": predates}));
    assert_eq!(sessions["rollup"], json!({"sessions": 1680, "linked_children": predates, "unlinked_children": 0, "incomplete_sessions": 0}));
    f.cli("collect");
    assert_eq!(f.cli_args(&["accounting", "sessions"]).1, linked);
    // The fork's source read before A8 (no `rollout_forks` row): pending its re-read.
    f.sidecar().execute_batch("DELETE FROM rollout_forks").unwrap();
    let pending = json!({"status": "unavailable", "reason": "pending_reread"});
    let (sessions, _) = f.cli_args(&["accounting", "sessions"]);
    assert_eq!(sessions["rollup"]["linked_children"], pending);
    assert_eq!(of(&sessions, SID)["children"]["sessions"][0]["inclusion"], pending);
    f.cli("collect");
    assert_eq!(f.cli_args(&["accounting", "sessions"]).1, linked);
}

/// Start of a UTC hour (November 2023): fleet windows are whole UTC hours.
const HOUR0: i64 = 472_223 * 3_600_000;
fn at(hours: i64, minutes: i64) -> i64 { HOUR0 + hours * 3_600_000 + minutes * 60_000 }

/// Canonical rows of the fan-out fixture below in `project`; `class_of(attempt,
/// code, docs)` picks each attempt's classification (`None`: unclassified) and
/// `config_of(attempt)` its dispatch decision's configuration id.
fn plant_fleet(project: &Path, class_of: &dyn Fn(&str, &str, &str) -> Option<String>, config_of: &dyn Fn(&str) -> String) -> rusqlite::Connection {
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
                VALUES(?1,?2,1,1,?3,?5,json_array(?5),'operator','owner','[\"operator_preference\"]',?4)",
                rusqlite::params![attempt, task, class_of(attempt, &code, &docs), marks[0].1, config_of(attempt)]).unwrap();
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

/// Run `git` in `dir` with a fixed identity and reflog time `at` (ms).
fn git_at(dir: &Path, home: &Path, at: i64, args: &[&str]) {
    let date = format!("@{} +0000", at / 1000);
    let out = Command::new("git").current_dir(dir).env_clear().env("HOME", home).env("PATH", "/usr/bin:/bin").env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "w").env("GIT_AUTHOR_EMAIL", "w@example.invalid").env("GIT_COMMITTER_NAME", "w").env("GIT_COMMITTER_EMAIL", "w@example.invalid")
        .env("GIT_AUTHOR_DATE", &date).env("GIT_COMMITTER_DATE", &date).args(args).output().unwrap();
    assert!(out.status.success() || args[0] == "merge", "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// Attempt worktrees of the fan-out fixture (`.state/worktrees/<attempt>/repo-00`)
/// with worker-side history. a2 (integrated at 0:45) rebases onto its target
/// at 0:10, resolves a conflicting merge at 0:20 and merges again at 0:50,
/// after integrating. b2 (integrated at 1:30) merges its target cleanly at
/// 1:10. b3 only commits. b4 has no worktree left.
fn worker_worktrees(project: &Path, home: &Path) {
    let commit = |dir: &Path, at: i64, file: &str, text: &str| {
        fs::write(dir.join(file), text).unwrap();
        git_at(dir, home, at, &["add", file]);
        git_at(dir, home, at, &["commit", "-q", "-m", "a message that is never read"]);
    };
    let tree = |attempt: &str, at: i64| {
        let dir = project.join(".state/worktrees").join(attempt).join("repo-00");
        fs::create_dir_all(&dir).unwrap();
        git_at(&dir, home, at, &["init", "-q", "-b", "main"]);
        commit(&dir, at, "f", "base\n");
        git_at(&dir, home, at, &["checkout", "-q", "-b", "work"]);
        dir
    };
    let a2 = tree("a2", at(0, 1));
    commit(&a2, at(0, 2), "w", "work\n");
    git_at(&a2, home, at(0, 3), &["checkout", "-q", "main"]);
    commit(&a2, at(0, 4), "m", "main\n");
    git_at(&a2, home, at(0, 5), &["checkout", "-q", "work"]);
    git_at(&a2, home, at(0, 10), &["rebase", "-q", "main"]);
    commit(&a2, at(0, 11), "f", "worker\n");
    git_at(&a2, home, at(0, 12), &["checkout", "-q", "main"]);
    commit(&a2, at(0, 13), "f", "target\n");
    git_at(&a2, home, at(0, 14), &["checkout", "-q", "work"]);
    git_at(&a2, home, at(0, 15), &["merge", "-q", "main", "-m", "merge"]);
    fs::write(a2.join("f"), "resolved\n").unwrap();
    git_at(&a2, home, at(0, 20), &["add", "f"]);
    git_at(&a2, home, at(0, 20), &["commit", "-q", "--no-edit"]);
    git_at(&a2, home, at(0, 48), &["checkout", "-q", "main"]);
    commit(&a2, at(0, 49), "n", "later\n");
    git_at(&a2, home, at(0, 49), &["checkout", "-q", "work"]);
    git_at(&a2, home, at(0, 50), &["merge", "-q", "--no-ff", "main", "-m", "merge"]);
    let b2 = tree("b2", at(1, 1));
    commit(&b2, at(1, 2), "w", "work\n");
    git_at(&b2, home, at(1, 3), &["checkout", "-q", "main"]);
    commit(&b2, at(1, 4), "m", "main\n");
    git_at(&b2, home, at(1, 5), &["checkout", "-q", "work"]);
    git_at(&b2, home, at(1, 10), &["merge", "-q", "--no-ff", "main", "-m", "merge"]);
    let b3 = tree("b3", at(1, 1));
    commit(&b3, at(1, 2), "w", "work\n");
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
    let db = plant_fleet(&project, &|_, code, _| Some(code.to_owned()), &|_| "cfg".to_owned());
    worker_worktrees(&project, &home);
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
    // One configuration (`cfg`, no display label planted): its split equals the fleet's.
    let by_configuration = json!({"configurations": {"cfg": {"display_label": null, "reference_level": 4, "level": 8, "reference_per_agent_per_hour": "1/2",
        "marginal_per_added_agent_per_hour": "1/4", "label": "comparable", "value": "3/4"}}, "configuration_unknown": {"attempts": 0, "accepted": 0}});
    let m35 = json!({"definition": "M35.fanout-v1", "name": "fan_out_efficiency", "window_ms": 3_600_000, "window_minutes": 60,
        "level_rule": "round_half_up(time_weighted_active_attempts)",
        "scope": "worker_attempts", "reference_level": 4, "level": 8, "reference_per_agent_per_hour": "1/2", "marginal_per_added_agent_per_hour": "1/4",
        "comparability": comparable, "label": "comparable", "value": "3/4", "by_configuration": by_configuration});
    assert_eq!(fleet["metrics"]["M35"], m35);
    let ratio = |n: i64, d: i64| json!({"numerator": n, "denominator": d, "value": format!("{n}/{d}")});
    let m36 = json!({"definition": "M36.integration-v1", "name": "integration_conflict_rate", "numerator": 3, "denominator": 4, "value": "3/4",
        "events": {"merge_conflict": 2, "stale_base": 1},
        "by_target": {"refs/heads/main": ratio(2, 3), "refs/heads/release": ratio(1, 1)},
        "by_bucket": {"4": ratio(1, 1), "8": ratio(2, 3)},
        "scope": "integrator_observed",
        "event_rule": "blocked/merge_conflict or discarded/stale_base on an operation created no later than the attempt's first integrated operation",
        // Worker-observed, apart: a2 rebased and resolved a merge conflict before
        // integrating (its merge after integration is not counted), b2 merged
        // the target cleanly, b3 only committed; b4's worktree is gone.
        "worker_observed": {"scope": "worker_observed", "numerator": 2, "denominator": 3, "value": "2/3",
            "events": {"merge": 1, "merge_conflict_resolved": 1, "rebase": 1}, "coverage": {"observed": 3, "worktree_absent": 1},
            "event_rule": "a rebase, merge, or resolved rebase/merge conflict in the attempt worktree's HEAD reflog, recorded no later than its first integrated operation; counts only, the reflog subject is never kept"}});
    assert_eq!(fleet["metrics"]["M36"], m36);
    // Without a telemetry sidecar there is no cost: M34 and M37 are unavailable, never 0.
    assert_eq!((&fleet["metrics"]["M34"]["value"], &fleet["metrics"]["M37"]["value"], &fleet["metrics"]["M37"]["records"]),
        (&json!({"status": "unavailable", "reason": "collection_not_run"}), &json!({"status": "unavailable", "reason": "collection_not_run"}), &json!({})));
    // The report takes the lane's M34–M37 unchanged.
    let report = cli(&["report", "--json"]).0;
    for id in ["M34", "M35", "M36", "M37"] { assert_eq!(report["metrics"][id], fleet["metrics"][id], "{id}"); }
    let (_, text) = cli(&["accounting", "fleet"]);
    assert!(text.lines().any(|l| l == "bucket k=8 windows=1 accepted=3 per_hour=3 per_agent=3/8 m35=3/4 marginal=1/4"), "{text}");
    assert!(text.lines().any(|l| l == "M35 fan_out_efficiency 3/4 (comparable)"), "{text}");
    assert!(text.lines().any(|l| l == "M36 integration_conflict_rate 3/4"), "{text}");
    assert!(text.lines().any(|l| l == "M34 coordinator_overhead n/a (collection_not_run)"), "{text}");
    // A window from hour 1: only the 8-agent level remains; b2–b4 reached integration in it.
    let since = at(1, 0).to_string();
    let windowed = cli(&["report", "--json", "--since", &since]).0;
    assert_eq!(windowed["metrics"]["M35"]["value"], json!({"status": "unavailable", "reason": "single_concurrency_level"}));
    assert_eq!((&windowed["metrics"]["M36"]["numerator"], &windowed["metrics"]["M36"]["denominator"]), (&json!(2), &json!(3)));

    // A 120-minute window (recorded in the output): hour 0 is 23:00 UTC, so it
    // shares its window 22:00–24:00 with nothing (4 agents × 1 h / 2 h → level
    // 2, 2 accepted → 1/hour) and hour 1 opens 00:00–02:00 (8 × 1 / 2 → level
    // 4, 3 accepted → 3/2 per hour). M35 = (3/2) / (4 × 1/2) = 3/4, marginal
    // (3/2 − 1) / (4 − 2) = 1/4. A window that does not divide a day is refused.
    let (wide, _) = cli(&["accounting", "fleet", "--json", "--window-minutes", "120"]);
    assert_eq!((&wide["fleet"]["window_ms"], &wide["fleet"]["window_minutes"], &wide["metrics"]["M35"]["window_minutes"]),
        (&json!(7_200_000), &json!(120), &json!(120)));
    let levels: Vec<_> = wide["fleet"]["buckets"].as_array().unwrap().iter()
        .map(|b| (b["level"].clone(), b["accepted"].clone(), b["accepted_per_hour"].clone(), b["per_agent_per_hour"].clone())).collect();
    assert_eq!(levels, [(json!(2), json!(2), json!("1"), json!("1/2")), (json!(4), json!(3), json!("3/2"), json!("3/8"))]);
    let m = &wide["metrics"]["M35"];
    assert_eq!((&m["value"], &m["marginal_per_added_agent_per_hour"], &m["label"]), (&json!("3/4"), &json!("1/4"), &json!("comparable")));
    for bad in ["7", "0", "2880"] {
        let out = Command::new(BIN).env_clear().env("HOME", &home).env("PATH", "/usr/bin:/bin")
            .args(["--root", root.to_str().unwrap(), "telemetry", "demo", "accounting", "fleet", "--window-minutes", bad]).output().unwrap();
        assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("--window-minutes must divide 1440"), "{bad}");
    }

    // Two agent configurations, X (a1, a2, b1–b4) and Y (a3, a4, b5–b8), with
    // display labels. X: level 2 with 2 accepted (reference, 1 per agent) and
    // level 4 with 3 (tb1–tb3) → 3 / (4 × 1) = 3/4, marginal 1/2. Y: levels 2
    // and 4 with nothing accepted → no_accepted_throughput. The fleet stays 3/4.
    let (x, y) = (format!("sha256:{}", "a".repeat(64)), format!("sha256:{}", "b".repeat(64)));
    let configured = plant_fleet(&root.join("configs"), &|_, code, _| Some(code.to_owned()),
        &|a| if ["a1", "a2", "b1", "b2", "b3", "b4"].contains(&a) { x.clone() } else { y.clone() });
    for (id, kind, version) in [(&x, "codex", "0.154.0"), (&y, "claude", "2.1.0")] {
        configured.execute("INSERT INTO agent_configurations(configuration_id,canonical_json,first_decided_unix_ms) VALUES(?1,?2,0)",
            rusqlite::params![id, json!({"kind": kind, "agent_version": version, "schema": "agent_configuration.v1"}).to_string()]).unwrap();
    }
    let fleet = serde_json::from_str::<serde_json::Value>(&cli_in("configs", &["accounting", "fleet", "--json"])).unwrap();
    let m35 = &fleet["metrics"]["M35"];
    assert_eq!((&m35["value"], &m35["label"]), (&json!("3/4"), &json!("comparable")));
    assert_eq!(m35["by_configuration"], json!({"configurations": {
        x.as_str(): {"display_label": "codex 0.154.0", "reference_level": 2, "level": 4, "reference_per_agent_per_hour": "1",
            "marginal_per_added_agent_per_hour": "1/2", "label": "comparable", "value": "3/4"},
        y.as_str(): {"display_label": "claude 2.1.0", "reference_level": null, "level": 4, "label": "comparable",
            "value": {"status": "unavailable", "reason": "no_accepted_throughput"}}},
        // c1 (the open attempt) is Y's; every attempt has a configuration.
        "configuration_unknown": {"attempts": 0, "accepted": 0}}));
    let split = &fleet["fleet"]["by_configuration"]["configurations"][x.as_str()];
    assert_eq!((&split["attempts"], &split["windows"]), (&json!(6), &json!({"bucketed": 2, "excluded": {"incomplete": 0, "concurrency_unknown": 0, "outside_window": 0}})));
    let text = cli_in("configs", &["accounting", "fleet"]);
    assert!(text.lines().any(|l| l == format!("M35 configuration {x} (codex 0.154.0) 3/4 (comparable)")), "{text}");
    assert!(text.lines().any(|l| l == format!("M35 configuration {y} (claude 2.1.0) n/a (no_accepted_throughput) (comparable)")), "{text}");

    // Mismatched task mix: the 8-agent hour worked on docs tasks. Same numbers, labelled descriptive.
    plant_fleet(&root.join("mixed"), &|a, code, docs| Some(if a.starts_with('b') { docs } else { code }.to_owned()), &|_| "cfg".to_owned());
    let fleet = serde_json::from_str::<serde_json::Value>(&cli_in("mixed", &["accounting", "fleet", "--json"])).unwrap();
    assert_eq!((&fleet["metrics"]["M35"]["value"], &fleet["metrics"]["M35"]["label"], &fleet["metrics"]["M35"]["comparability"]["reasons"]),
        (&json!("3/4"), &json!("descriptive"), &json!(["task_mix_differs"])));
    assert_eq!((&fleet["fleet"]["buckets"][1]["mix"], &fleet["fleet"]["buckets"][1]["mix_tvd"]), (&json!({"docs/small": "1"}), &json!("1")));
    // An attempt without a classification makes the mix unknown.
    plant_fleet(&root.join("unknown"), &|a, code, docs| (a != "b8").then(|| if a.starts_with('b') { docs } else { code }.to_owned()), &|_| "cfg".to_owned());
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

/// Session id written literally in `charged.jsonl`.
const CHARGED: &str = "00000000-0000-4000-8000-0000000b1301";

/// Wait until the clock has passed `t`, so the next recorded time is later.
fn after(t: i64) { while unix_ms() <= t { std::thread::sleep(Duration::from_millis(1)); } }

/// TM2.3 remainder (§13). Provider charges are their own basis
/// (`provider_billed`), matched to the same usage by response id and never
/// added to estimates: ch-1 USD 0.0100 against the 1,000 in + 500 out
/// estimate of card v2 (2 and 8 per 10^6 → 0.006) differs by 0.004, with the
/// cards' exclusions as evidence; ch-2 0.0041 equals its doc 05 estimate;
/// ch-3 is USD against a EUR estimate (no implicit conversion); ch-4 names
/// an unknown response and ch-5 no request at all (unmatched, apart). Doc 05
/// §7 "accepted charge $0.0100 later corrected to $0.0080": the correction
/// appends −0.002, the current total is 0.008 and `--as-of` the earlier
/// import still shows 0.01. An invoice of 12 is allocated by
/// `by_total_tokens.v1` (2,980 bound + 500 unbound tokens) exactly, and a
/// subscription of 7 whose period also holds an uncounted record is partial
/// (upper bounds), never 0. A dated EUR→USD rate converts the EUR estimate
/// (0.00021 × 1.1 = 0.000231) in a separate view; `cost --as-of` reproduces
/// each revision byte for byte.
#[test]
fn provider_charges_reconcile_allocate_and_convert() {
    let f = Fixture::new();
    let part = |name: &str| format!("{ACCOUNTING}/{name}");
    let (ts, late) = (f.decided + 1_000, f.decided + 5_000);
    f.rollout(&f.home, "charged", &[&part("charged.jsonl")], &f.worktree(), ts, "0.154.0");
    f.rollout(&f.home, "unbound", &[&part("charged-unbound.jsonl")], &f.tmp.path().display().to_string(), ts, "0.154.0");
    f.rollout(&f.home, "uncounted", &[&part("charged-uncounted.jsonl")], &f.worktree(), late, "0.154.0");
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    for (name, boundary) in [("rates-v2.toml", 0), ("rates-eur.json", 0)] { f.cli_args(&["accounting", "import-rate-card", &rate_card(&f, name, boundary)]); }
    let unavailable = |reason: &str| json!({"status": "unavailable", "reason": reason});
    assert_eq!(f.report()["metrics"]["M11"]["value"], unavailable("no_provider_charges"));
    assert_eq!(f.cli_args(&["accounting", "reprice"]).0["revision"], 1);
    let first_cost = f.cli_args(&["accounting", "cost", "--json"]).1;

    // Imports: synthetic only, append-only, revisions in order.
    let charges1 = rate_card(&f, "charges-1.json", late);
    let (imported, _) = f.cli_args(&["accounting", "import-charges", &charges1]);
    assert_eq!(imported["charges"].as_array().unwrap().iter().map(|c| (c["charge_id"].clone(), c["imported"].clone())).collect::<Vec<_>>(),
        ["ch-1", "ch-2", "ch-3", "ch-4", "ch-5"].map(|c| (json!(c), json!(true))));
    assert_eq!(imported["invoices"].as_array().unwrap().len(), 2);
    assert_eq!(f.cli_args(&["accounting", "import-charges", &charges1]).0["charges"][0]["imported"], false, "the same revision again is a no-op");
    let edited = f.tmp.path().join("edited.json");
    fs::write(&edited, fs::read_to_string(&charges1).unwrap().replace("\"0.0100\"", "\"0.0200\"")).unwrap();
    assert!(f.cli_fail(&["accounting", "import-charges", edited.to_str().unwrap()]).contains("append-only"));
    fs::write(&edited, fs::read_to_string(Path::new(ACCOUNTING).join("charges-2.json")).unwrap().replace("\"revision\": 2", "\"revision\": 3")).unwrap();
    assert!(f.cli_fail(&["accounting", "import-charges", edited.to_str().unwrap()]).contains("not the next revision"));
    fs::write(&edited, fs::read_to_string(&charges1).unwrap().replace("\"synthetic\": true", "\"synthetic\": false")).unwrap();
    assert!(f.cli_fail(&["accounting", "import-charges", edited.to_str().unwrap()]).contains("only synthetic fixture files"));

    let (view, _) = f.cli_args(&["accounting", "charges"]);
    let charge = |view: &serde_json::Value, id: &str| view["charges"].as_array().unwrap().iter().find(|c| c["charge_id"] == id).cloned().unwrap();
    let first_import = charge(&view, "ch-1")["history"][0]["imported_unix_ms"].as_i64().unwrap();
    assert_eq!((&view["basis"], &view["valuation_revision"], &view["provider_billed"]), (&json!("provider_billed"), &json!(1), &json!({"USD": "0.5164"})),
        "0.01 + 0.0041 + 0.0003 + 0.002 + 0.5, matched or not; never an estimate");
    let exclusions = json!(["estimate_excludes_discounts", "estimate_excludes_fees", "estimate_excludes_taxes", "rate_cards_fixture_only"]);
    let ch1 = charge(&view, "ch-1");
    assert_eq!((&ch1["amount"], &ch1["revision"]), (&json!("0.01"), &json!(1)));
    assert_eq!(ch1["reconciliation"], json!({"status": "matched", "matched_by": "response_id", "entries": [format!("codex:{CHARGED}:1")],
        "attempt_ids": [f.attempt], "estimate": {"status": "complete", "currency": "USD", "amount": "0.006"},
        "coverage": {"entries": 1, "priced": 1, "unpriced": {}}, "difference": {"currency": "USD", "amount": "0.004"}, "explanations": exclusions}));
    let ch2 = charge(&view, "ch-2")["reconciliation"].clone();
    assert_eq!((&ch2["estimate"]["amount"], &ch2["difference"], &ch2["explanations"]), (&json!("0.0041"), &json!({"currency": "USD", "amount": "0"}), &json!([])));
    assert_eq!(charge(&view, "ch-3")["reconciliation"]["difference"]["reason"], "currency_differs");
    assert_eq!(charge(&view, "ch-3")["reconciliation"]["estimate"], json!({"status": "complete", "currency": "EUR", "amount": "0.00021"}));
    assert_eq!(charge(&view, "ch-4")["reconciliation"], json!({"status": "unmatched", "reason": "no_matching_usage"}));
    assert_eq!(charge(&view, "ch-5")["reconciliation"], json!({"status": "unmatched", "reason": "no_request_identity"}));
    assert_eq!(view["reconciliation"]["matched"], 3);
    assert_eq!(view["reconciliation"]["unmatched"], json!({"no_matching_usage": 1, "no_request_identity": 1}));
    // The cache-write record, the unbound session and the uncounted record: estimates without a charge, apart.
    assert_eq!((&view["reconciliation"]["uncharged_estimates"]["estimate"], &view["reconciliation"]["uncharged_estimates"]["coverage"]),
        (&json!({"status": "partial", "reason": "unpriced_entries", "currency": "USD", "priced_amount": "0.0016"}),
         &json!({"entries": 3, "priced": 1, "unpriced": {"cache_write_convention_unknown": 1, "usage_not_counted": 1}})));
    let m11 = f.report()["metrics"]["M11"].clone();
    assert_eq!((&m11["definition"], &m11["name"], &m11["value"], &m11["currency"], &m11["basis"], &m11["charges"], &m11["invoices_separate"]),
        (&json!("M11.charges-v1"), &json!("reported_spend_subtotal"), &json!("0.5164"), &json!("USD"), &json!("provider_billed"), &json!("fixture_only"), &json!(2)));

    // Doc 05 §7: the charge corrected from 0.0100 to 0.0080 appends −0.002; the earlier view remains.
    after(first_import);
    f.cli_args(&["accounting", "import-charges", &part("charges-2.json")]);
    let (view, _) = f.cli_args(&["accounting", "charges"]);
    let ch1 = charge(&view, "ch-1");
    assert_eq!((&ch1["amount"], &ch1["revision"], &ch1["reconciliation"]["difference"]), (&json!("0.008"), &json!(2), &json!({"currency": "USD", "amount": "0.002"})));
    assert_eq!(ch1["history"].as_array().unwrap().iter().map(|h| (h["revision"].clone(), h["amount"].clone(), h["adjustment"].clone())).collect::<Vec<_>>(),
        [(json!(1), json!("0.01"), json!(null)), (json!(2), json!("0.008"), json!("-0.002"))]);
    assert_eq!(view["provider_billed"], json!({"USD": "0.5144"}));
    assert_eq!(f.report()["metrics"]["M11"]["value"], "0.5144");
    let (earlier, _) = f.cli_args(&["accounting", "charges", "--as-of", &first_import.to_string()]);
    assert_eq!((&charge(&earlier, "ch-1")["amount"], &earlier["provider_billed"]), (&json!("0.01"), &json!({"USD": "0.5164"})));
    // Stream 10 re-runs harmlessly (streams table behind, then 11): every revision stays.
    let current = f.cli_args(&["accounting", "charges"]).1;
    f.sidecar().execute("UPDATE telemetry_streams SET version=9 WHERE stream='accounting'", []).unwrap();
    assert_eq!(f.cli_args(&["accounting", "import-charges", &part("charges-2.json")]).0["charges"][0]["imported"], false);
    assert_eq!((f.cli_args(&["accounting", "status"]).0["version"].clone(), f.cli_args(&["accounting", "charges"]).1), (json!(16), current));

    // Invoice allocation by a named, versioned rule: 12 × 2980/3480 and 12 × 500/3480 in units
    // of 10^-12; the one remaining unit goes to the larger remainder; the sum is exactly 12.
    let (allocation, _) = f.cli_args(&["accounting", "allocate", "inv-1"]);
    assert_eq!((&allocation["basis"], &allocation["rule"]["id"], &allocation["status"], &allocation["tokens"], &allocation["valuation_revision"]),
        (&json!("invoice_allocation"), &json!("by_total_tokens.v1"), &json!("complete"), &json!(3480), &json!(1)));
    assert_eq!(allocation["allocations"], json!([{"attempt_id": f.attempt, "tokens": 2980, "share": "2980/3480", "currency": "USD", "amount": "10.275862068966", "bound": null}]));
    assert_eq!(allocation["unattributed"], json!({"attempt_id": null, "tokens": 500, "share": "500/3480", "currency": "USD", "amount": "1.724137931034", "bound": null}));
    assert_eq!(allocation["coverage"], json!({"entries_in_period": 5, "unknown": 0, "outside_period": 1}), "the uncounted record lies after inv-1's period");
    // The subscription's period holds the uncounted record: partial, known shares are upper bounds, the unknown never 0.
    let (sub, _) = f.cli_args(&["accounting", "allocate", "sub-1"]);
    assert_eq!((&sub["status"], &sub["reason"], &sub["invoice"]["kind"]), (&json!("partial"), &json!("usage_unknown_in_period"), &json!("subscription")));
    assert_eq!(sub["allocations"][0]["amount"], "5.994252873563");
    assert_eq!(sub["allocations"][0]["bound"], "upper");
    assert_eq!(sub["unattributed"]["amount"], "1.005747126437");
    assert_eq!(sub["unknown"], json!([{"attempt_id": f.attempt, "entries": 1, "allocation": unavailable("usage_not_counted")}]));
    assert!(f.cli_fail(&["accounting", "allocate", "inv-1", "--rule", "evenly.v1"]).contains("unknown allocation rule"));
    assert!(f.cli_fail(&["accounting", "allocate", "inv-9"]).contains("no invoice"));

    // Dated conversion: a separate valuation recording its rate; the stored estimate stays EUR.
    let converted_entry = |view: &serde_json::Value, n: i64| view["entries"].as_array().unwrap().iter()
        .find(|e| e["entry_id"] == format!("codex:{CHARGED}:{n}")).unwrap()["converted"].clone();
    let before_fx = unix_ms();
    after(before_fx + 1);
    f.cli_args(&["accounting", "import-fx", &part("fx-1.json")]);
    f.cli_args(&["accounting", "import-fx", &rate_card(&f, "fx-2.toml", ts + 1)]);
    let (fx, _) = f.cli_args(&["accounting", "fx", "--to", "USD"]);
    assert_eq!(converted_entry(&fx, 3), json!({"status": "priced", "currency": "USD", "amount": "0.000231", "conversion": {"from_currency": "EUR", "rate": "1.1",
        "rate_id": "synthetic-fx@1:EUR->USD@0", "table_id": "synthetic-fx", "version": 1, "effective_from_unix_ms": 0, "effective_to_unix_ms": null,
        "dated_by": "usage_interval"}}), "version 2's 1.2 starts after the record: the rate of its date applies");
    assert_eq!(converted_entry(&fx, 1)["conversion"], "same_currency");
    let attempt_fx = |view: &serde_json::Value, a: serde_json::Value| view["attempts"].as_array().unwrap().iter().find(|r| r["attempt_id"] == a).cloned().unwrap();
    assert_eq!(attempt_fx(&fx, json!(f.attempt)), json!({"attempt_id": f.attempt,
        "estimate": {"status": "partial", "reason": "unpriced_entries", "currency": "USD", "priced_amount": "0.010331"},
        "coverage": {"entries": 5, "priced": 3, "unpriced": {"cache_write_convention_unknown": 1, "usage_not_counted": 1}}}));
    assert_eq!(attempt_fx(&fx, json!(null))["estimate"], json!({"status": "complete", "currency": "USD", "amount": "0.0016"}));
    let (fx_before, _) = f.cli_args(&["accounting", "fx", "--to", "USD", "--as-of", &before_fx.to_string()]);
    assert_eq!(converted_entry(&fx_before, 3), unavailable("no_fx_rate"), "no table imported by then");
    let (jpy, _) = f.cli_args(&["accounting", "fx", "--to", "JPY"]);
    assert_eq!(attempt_fx(&jpy, json!(f.attempt))["estimate"], unavailable("no_priced_entries"));
    let cost = f.cli_args(&["accounting", "cost", "--json"]).0;
    assert_eq!(cost["attempts"].as_array().unwrap().iter().find(|a| a["attempt_id"] == f.attempt.as_str()).unwrap()["estimate"],
        json!({"status": "unavailable", "reason": "mixed_currency", "priced_by_currency": {"EUR": "0.00021", "USD": "0.0101"}}), "the stored view never adds currencies");

    // `cost --as-of`: the latest revision computed by that instant, byte-identical to `--revision`.
    let computed1 = cost["computed_unix_ms"].as_i64().unwrap();
    let before_reprice = unix_ms();
    after(before_reprice);
    f.cli_args(&["accounting", "import-rate-card", &rate_card(&f, "rates-v3.json", 0)]);
    assert_eq!(f.cli_args(&["accounting", "reprice"]).0["revision"], 2);
    let (latest, latest_bytes) = f.cli_args(&["accounting", "cost", "--json"]);
    assert_eq!(f.cli_args(&["accounting", "cost", "--json", "--as-of", &computed1.to_string()]).1, first_cost);
    assert_eq!(f.cli_args(&["accounting", "cost", "--json", "--as-of", &latest["computed_unix_ms"].to_string()]).1, latest_bytes);
    assert_eq!(f.cli_args(&["accounting", "cost", "--json", "--as-of", &(computed1 - 1).to_string()]).0, unavailable("not_priced_as_of"));
    assert!(f.text(&["accounting", "cost", "--as-of", &computed1.to_string()]).starts_with("cost revision 1:"));
    assert!(f.cli_fail(&["accounting", "cost", "--revision", "1", "--as-of", "1"]).contains("cannot be used with"));
    // Charges reconcile against the valuation revision of their instant: 0.008 − 0.005 now, 0.008 − 0.006 then.
    assert_eq!(charge(&f.cli_args(&["accounting", "charges"]).0, "ch-1")["reconciliation"]["difference"]["amount"], "0.003");
    let (then, _) = f.cli_args(&["accounting", "charges", "--as-of", &before_reprice.to_string()]);
    assert_eq!((&then["valuation_revision"], &charge(&then, "ch-1")["reconciliation"]["difference"]["amount"]), (&json!(1), &json!("0.002")));
}

/// Attempt id and decision time of the latest dispatch decision.
fn latest_decision(f: &Fixture) -> (String, i64) {
    rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap()
        .query_row("SELECT attempt_id,decided_unix_ms FROM dispatch_decisions ORDER BY rowid DESC LIMIT 1", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap()
}

/// Canonical budget policy `revision` with `limits`, planted as a signed
/// import would store it (fixture only: the owner signature is not under test).
fn plant_budget(f: &Fixture, revision: u64, limits: herdr_projects::domain::BudgetLimits) {
    use herdr_projects::domain::{BudgetPolicy, VersionedReference};
    let db_path = f.project.join(".state/state.db");
    let policy = BudgetPolicy { version: 1, project_store: fs::canonicalize(&db_path).unwrap().display().to_string(), revision,
        authority: VersionedReference { id: "owner".into(), revision: 1, digest: "a".repeat(64) }, limits };
    let payload = serde_json::to_string(&policy).unwrap();
    rusqlite::Connection::open(&db_path).unwrap().execute("INSERT INTO budget_policies(revision,payload,payload_hash) VALUES(?1,?2,?3)",
        rusqlite::params![revision as i64, payload, format!("{:x}", Sha256::digest(payload.as_bytes()))]).unwrap();
}

/// TM2.4 in shadow mode (§14), on doc 05 §7's budget goldens with a synthetic
/// per-token card (input 0.05, output 0.02): a cancelled attempt of 1,000 in
/// and 500 out = $60 accepted and an open attempt reserved at $30 (what-if):
/// with a budget of $100 a new request of $15 would be refused (projected
/// $105). After $10 of covered usage arrives on the open attempt: accepted
/// $70, remaining $20, exposure $90 (no false $100). An open attempt without
/// a reservation is unknown exposure: `refuse` blocks, `allow_incomplete`
/// warns. The canonical token policy (1,850 known tokens) is evaluated as
/// admission would with the ledger: within 5,000 it warns like today; at
/// 1,000 it would block where admission (`allow_incomplete`) does not. M04
/// includes the cancelled attempt: $70 / 1 accepted task. `state.db` is
/// never written.
#[test]
fn shadow_budget_bridge_matches_doc05_goldens() {
    use herdr_projects::domain::{BudgetLimits, UnknownUsagePolicy};
    let f = Fixture::new();
    let part = |name: &str| format!("{ACCOUNTING}/{name}");
    let a1 = f.attempt.clone();
    f.rollout(&f.home, "budget-a", &[&part("budget-a.jsonl")], &f.worktree(), f.decided + 1_000, "0.154.0");
    f.cli("collect");
    f.readmit("codex");
    let (a2, a2_decided) = latest_decision(&f);
    let db_path = f.project.join(".state/state.db");
    rusqlite::Connection::open(&db_path).unwrap().execute("INSERT INTO collector_bindings(attempt_id,revision,state,collector,execution_home,unix_ms,source)
        VALUES(?1,1,'active','codex',?2,?3,'apply_launch_started')", rusqlite::params![a2, f.home.display().to_string(), unix_ms()]).unwrap();
    f.cli_args(&["accounting", "sync"]);
    f.cli_args(&["accounting", "import-rate-card", &part("rates-budget.json")]);
    f.cli_args(&["accounting", "reprice"]);

    let policy = |name: &str, body: serde_json::Value| {
        let path = f.tmp.path().join(name);
        let mut doc = json!({"synthetic": true, "policy_id": "synthetic-what-if", "version": 1, "currency": "USD",
            "source": "INVENTED what-if limits for the doc 05 budget goldens; never installed"});
        doc.as_object_mut().unwrap().extend(body.as_object().unwrap().clone());
        fs::write(&path, doc.to_string()).unwrap();
        path.display().to_string()
    };
    let golden = policy("golden.json", json!({"unknown_usage": "refuse", "project": {"max_amount": "100"},
        "reservations": {a2.as_str(): {"amount": "30"}}, "request": {"task": "work", "amount": "15"}}));
    let state = || fs::read(&db_path).unwrap();
    let files = || { let mut names: Vec<_> = fs::read_dir(f.project.join(".state")).unwrap().map(|e| e.unwrap().file_name()).collect(); names.sort(); names };
    let (before, listing) = (state(), files());
    let shadow = |args: &[&str]| { let mut all = vec!["accounting", "budget-shadow"]; all.extend(args); f.cli_args(&all).0 };
    let evaluation = |view: &serde_json::Value, source: &str, scope: &str, dimension: &str| view["evaluations"].as_array().unwrap().iter()
        .find(|e| e["policy_source"] == source && e["scope"] == scope && e["dimension"] == dimension).cloned()
        .unwrap_or_else(|| panic!("{source} {scope} {dimension} in {view}"));
    let money = |e: &serde_json::Value| ["accepted", "remaining_reserved", "exposure", "new_request", "projected", "decision", "reason"].map(|k| e[k].clone());

    let view = shadow(&["--policy", &golden]);
    assert_eq!((&view["mode"], &view["enforcement"], &view["canonical_writes"]), (&json!("shadow"), &json!("none"), &json!("none")));
    assert_eq!((&view["canonical"]["policy"], &view["canonical"]["reason"], &view["differs_from_canonical"]), (&json!(null), &json!("no_budget_policy"), &json!(null)));
    let project = evaluation(&view, "what_if", "project", "amount");
    assert_eq!(money(&project), [json!("60"), json!("30"), json!("90"), json!("15"), json!("105"), json!("would_block"), json!("projected_exposure_exceeds_limit")],
        "doc 05: budget $100, accepted $60, remaining exposure $30, new request $15 → refuse, projected $105");
    assert_eq!((&project["limit"], &project["currency"], &project["unknown"]), (&json!("100"), &json!("USD"), &json!([])));
    assert_eq!(project["attempts"], json!([{"attempt_id": a1, "task_id": "work", "state": "cancelled", "accepted": "60", "in_flight": null},
        {"attempt_id": a2, "task_id": "work", "state": "reserved", "accepted": "0", "in_flight": {"reservation": "30", "remaining": "30"}}]));
    assert_eq!(view["decision"], json!({"would_block": true, "would_warn": false, "reasons": ["projected_exposure_exceeds_limit"]}));
    assert_eq!(view["provenance"]["valuation_revision"], 1);
    let computed1 = view["provenance"]["valuation_computed_unix_ms"].as_i64().unwrap();

    // $10 of covered usage arrives on the reserved attempt.
    f.rollout(&f.home, "budget-b", &[&part("budget-b.jsonl")], &format!("{}/.state/worktrees/{a2}/repo-00", f.project.display()), a2_decided + 1_000, "0.154.0");
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    after(computed1);
    assert_eq!(f.cli_args(&["accounting", "reprice"]).0["revision"], 2);
    let view = shadow(&["--policy", &golden]);
    assert_eq!(money(&evaluation(&view, "what_if", "project", "amount")),
        [json!("70"), json!("20"), json!("90"), json!("15"), json!("105"), json!("would_block"), json!("projected_exposure_exceeds_limit")],
        "doc 05: accepted $70, remaining $20, combined exposure $90; no false $100");
    // As of the first revision the shadow reproduces the earlier answer.
    let earlier = shadow(&["--policy", &golden, "--as-of", &computed1.to_string()]);
    assert_eq!((&evaluation(&earlier, "what_if", "project", "amount")["accepted"], &earlier["provenance"]["valuation_revision"]), (&json!("60"), &json!(1)));
    // A request of $10 fits exactly.
    let fits = policy("fits.json", json!({"project": {"max_amount": "100"}, "reservations": {a2.as_str(): {"amount": "30"}}, "request": {"amount": "10"}}));
    assert_eq!(money(&evaluation(&shadow(&["--policy", &fits]), "what_if", "project", "amount")),
        [json!("70"), json!("20"), json!("90"), json!("10"), json!("100"), json!("allow"), json!("within_limit")]);
    // No reservation for the open attempt: its in-flight usage is unknown, never 0.
    for (unknown_usage, decision, reason) in [("refuse", "would_block", "provider_usage_unavailable"), ("allow_incomplete", "would_warn", "usage_incomplete")] {
        let open = policy("open.json", json!({"unknown_usage": unknown_usage, "project": {"max_amount": "100"}, "request": {"amount": "10"}}));
        let e = evaluation(&shadow(&["--policy", &open]), "what_if", "project", "amount");
        assert_eq!((&e["exposure"], &e["decision"], &e["reason"], &e["unknown"]), (&json!("70"), &json!(decision), &json!(reason),
            &json!([{"attempt_id": a2, "reason": "in_flight_usage_unknown", "entries": 0}])));
    }
    // A task budget covers every attempt of the task, the cancelled one included.
    let task = policy("task.json", json!({"tasks": {"work": {"max_amount": "80"}}, "reservations": {a2.as_str(): {"amount": "30"}}}));
    let e = evaluation(&shadow(&["--policy", &task]), "what_if", "task:work", "amount");
    assert_eq!((&e["exposure"], &e["decision"], &e["reason"], &e["new_request"]), (&json!("90"), &json!("would_block"), &json!("limit_exceeded"), &json!(null)));

    assert_eq!((state(), files()), (before, listing), "no shadow read writes state.db or creates a file beside it");

    // The canonical policy, as admission reads it today and as the bridge would with the ledger.
    plant_budget(&f, 1, BudgetLimits { max_attempts: None, max_provider_tokens: Some(5_000), unknown_usage: UnknownUsagePolicy::AllowIncomplete });
    let view = shadow(&[]);
    assert_eq!(view["canonical"]["decision_today"], json!({"blockers": [], "incomplete": true, "provider_tokens": "unknown"}));
    assert_eq!(view["canonical"]["task_budgets"], json!({"status": "unavailable", "reason": "no_canonical_task_budget"}));
    let tokens = evaluation(&view, "canonical", "project", "provider_tokens");
    assert_eq!((&tokens["policy_revision"], &tokens["accepted"], &tokens["exposure"], &tokens["new_request"], &tokens["decision"], &tokens["reason"]),
        (&json!(1), &json!(1850), &json!(1850), &json!({"status": "unavailable", "reason": "new_request_usage_unknown"}), &json!("would_warn"), &json!("usage_incomplete")));
    assert_eq!(tokens["unknown"], json!([{"attempt_id": a2, "reason": "in_flight_usage_unknown", "entries": 0},
        {"attempt_id": null, "reason": "new_request_usage_unknown", "entries": 0}]));
    assert_eq!(view["differs_from_canonical"], false);
    assert_eq!(view["attempts"].as_array().unwrap().iter().map(|a| a["pinned_policy_revision"].clone()).collect::<Vec<_>>(), [json!(null), json!(null)],
        "both attempts were admitted before any policy");
    plant_budget(&f, 2, BudgetLimits { max_attempts: None, max_provider_tokens: Some(1_000), unknown_usage: UnknownUsagePolicy::AllowIncomplete });
    let (before, listing) = (state(), files());
    let view = shadow(&[]);
    let tokens = evaluation(&view, "canonical", "project", "provider_tokens");
    assert_eq!((&tokens["policy_revision"], &tokens["decision"], &tokens["reason"]), (&json!(2), &json!("would_block"), &json!("limit_exceeded")));
    assert_eq!((&view["canonical"]["decision_today"]["blockers"], &view["differs_from_canonical"]), (&json!([]), &json!(true)),
        "admission today allows with incomplete usage; the bridge would block");
    // Nothing canonical was written by any shadow read.
    assert_eq!((state(), files()), (before, listing));
    assert_eq!(f.cli_args(&["accounting", "budget-shadow"]).0["mode"], "shadow");

    // M04: no terminal task yet; then task `work` accepted: its cancelled and open attempts' $60 + $10.
    let m04 = f.report()["metrics"]["M04"].clone();
    assert_eq!((&m04["value"], &m04["reason"], &m04["denominator"]), (&json!(null), &json!("empty_denominator"), &json!(0)));
    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    let hex = |c: char| c.to_string().repeat(64);
    db.execute("INSERT INTO task_contracts(task_id,contract_revision,plan_revision,project_store,expected_head,repository,base_oid,object_format,memory_snapshot_id,route,raw_bytes,raw_digest,installed_seq)
        VALUES('work',1,NULL,'store',0,'/repo',?1,'sha1',NULL,'verify_only',x'61',?2,(SELECT max(sequence) FROM events))", rusqlite::params!["b".repeat(40), hex('c')]).unwrap();
    db.execute("INSERT INTO result_submissions(submission_id,project_store,idempotency_key,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,artifact_manifest,claimed_checks,created_unix_ms)
        VALUES(?1,'store',?1,?2,'{}','work',1,?2,?3,'/repo',?4,?4,'sha1','[]','[]',1000)", rusqlite::params![hex('1'), hex('d'), a2, "b".repeat(40)]).unwrap();
    db.execute("INSERT INTO verified_results(result_id,run_id,submission_id,commit_oid,tree_oid,object_format,policy_digest,receipt_digest,isolation,memory_fence,created_unix_ms)
        VALUES(?1,?1,?2,?3,?3,'sha1',?4,?4,'linux-unshare-user-pid-mount-v1',0,2000)", rusqlite::params![hex('2'), hex('1'), "b".repeat(40), hex('7')]).unwrap();
    drop(db);
    let m04 = f.report()["metrics"]["M04"].clone();
    assert_eq!((&m04["definition"], &m04["name"], &m04["value"], &m04["currency"], &m04["denominator"], &m04["numerator"], &m04["basis"]),
        (&json!("M04.cost-v1"), &json!("cost_per_accepted_task"), &json!("70/1"), &json!("USD"), &json!(1),
         &json!({"status": "complete", "currency": "USD", "amount": "70"}), &json!("published_rate_estimate")));
    assert_eq!(m04["coverage"], json!({"entries": 2, "priced": 2, "unpriced": {}, "attempts": 2, "attempts_without_usage": {}}));
    assert!(f.text(&["report"]).lines().any(|l| l == "M04 cost_per_accepted_task 70/1"));
}

/// Session id of the one record in `priced-before.jsonl` (1,000 input + 500 output, gpt-5.5).
const PRICED_SID: &str = "00000000-0000-4000-8000-0000000b3001";

/// Write `priced-before.jsonl` under `home` as session `sid` at `cwd`, its
/// session and record time `ts`, reporting `model`.
fn priced_rollout(home: &Path, name: &str, sid: &str, cwd: &str, ts: i64, model: &str) {
    let dir = home.join(".codex/sessions/2026/09/28");
    fs::create_dir_all(&dir).unwrap();
    let text = fs::read_to_string(Path::new(ACCOUNTING).join("priced-before.jsonl")).unwrap();
    let ts = jiff::Timestamp::from_millisecond(ts).unwrap().to_string();
    fs::write(dir.join(format!("rollout-2026-09-28T00-00-00-{name}.jsonl")),
        text.replace(PRICED_SID, sid).replace("@TS@", &ts).replace("@CWD@", cwd).replace("@VERSION@", "0.154.0").replace("\"gpt-5.5\"", &format!("\"{model}\""))).unwrap();
}

/// Collect, sync and reprice the fixture; returns `accounting fleet --json`,
/// checking that the report shows the same M34 and M37.
fn fleet_after_reprice(f: &Fixture) -> serde_json::Value {
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    f.cli_args(&["accounting", "reprice"]);
    let fleet = f.cli_args(&["accounting", "fleet", "--json"]).0;
    let report = f.report();
    for id in ["M34", "M37"] { assert_eq!(report["metrics"][id], fleet["metrics"][id], "{id}"); }
    fleet
}

/// Doc 10 §5a "Coordinator overhead" and doc 05 §5a, with invented synthetic
/// rates (gpt-5.5: $2000 per 10^6 input, $4000 per 10^6 output, so each
/// 1,000 + 500 record is exactly $4). Four worker sessions of the attempt
/// ($16) and one coordinator session ($4: Codex at the project directory,
/// bound to no attempt) → M34 = 4 / 20 = 1/5; the attempt ran 2 hours → $2 per
/// active worker-thread-hour; rule v1 allocates the $4 to the only task
/// running at its record time. A coordinator record without a rate makes M34
/// partial; a second task running then splits the next allocation evenly
/// ($2 each) and, having no observed usage, keeps the ratio partial.
/// M37: the owner records that the cancelled attempt was superseded because a
/// sibling changed the same area → $16 of $20 (4/5); before that it is
/// unexplained abandonment in its own bucket. Workers cannot record reasons.
#[test]
fn coordinator_overhead_and_overlap_waste_from_accepted_reasons() {
    let f = Fixture::new();
    let other_home = f.home.parent().unwrap().join("other-home");
    let state = f.project.join(".state/state.db");
    let raw = rusqlite::Connection::open(&state).unwrap();
    // Fixture only (no launch): the attempt ran for the two hours before its decision time.
    for (mark, at) in [("running", f.decided - 7_200_000), ("completed", f.decided)] {
        raw.execute("INSERT INTO attempt_lifecycle(attempt_id,state,attempt_revision,unix_ms,source) VALUES(?1,?2,1,?3,'fixture')",
            rusqlite::params![f.attempt, mark, at]).unwrap();
    }
    for n in 1..=4 { priced_rollout(&f.home, &format!("w{n}"), &format!("00000000-0000-4000-8000-00000000f00{n}"), &f.worktree(), f.decided + n, "gpt-5.5"); }
    let card = f.tmp.path().join("rates-fleet.json");
    fs::write(&card, json!({"card_id": "synthetic-fleet", "version": 1, "provider": "synthetic", "product": "codex", "models": ["gpt-5.5"],
        "currency": "USD", "rate_unit": 1_000_000, "effective_from_unix_ms": 0, "includes": {"discounts": false, "taxes": false, "fees": false},
        "source": "INVENTED synthetic test rates (doc 10 §5a coordinator fixture); not a provider price",
        "rates": [{"category": "input", "rate": "2000"}, {"category": "output", "rate": "4000"}]}).to_string()).unwrap();
    f.cli("collect");
    assert_eq!(f.cli_args(&["accounting", "import-rate-card", card.to_str().unwrap()]).0["imported"], true);

    // No session in the coordinator scope: unknown, never 0.
    let fleet = fleet_after_reprice(&f);
    let m34 = &fleet["metrics"]["M34"];
    assert_eq!((&m34["value"], &m34["scope"], &m34["allocation_rule"]),
        (&json!({"status": "unavailable", "reason": "coordinator_usage_not_observed"}), &json!("coordinator-scope-v1"), &json!("coordinator-allocation-v2")));

    // The coordinator: Codex at the project directory, from another scanned home, an hour into the run.
    let coordinator_at = f.decided - 3_600_000;
    priced_rollout(&other_home, "coordinator", "00000000-0000-4000-8000-00000000c001", &f.project.display().to_string(), coordinator_at, "gpt-5.5");
    let fleet = fleet_after_reprice(&f);
    let m34 = &fleet["metrics"]["M34"];
    let usd = |amount: &str| json!({"status": "complete", "currency": "USD", "amount": amount});
    assert_eq!(m34["value"], json!("1/5"), "coordinator $4 / (coordinator $4 + workers $16)");
    assert_eq!(m34["coordinator"], json!({"sessions": 1, "estimate": usd("4"),
        "coverage": {"entries": 1, "priced": 1, "unpriced": 0, "sessions_not_valued": 0, "attempts_without_observed_usage": 0}}));
    assert_eq!((&m34["total_project_lifecycle_cost"]["estimate"], &m34["total_project_lifecycle_cost"]["worker_attempts"]), (&usd("20"), &json!(1)));
    assert_eq!(m34["per_active_worker_thread_hour"], json!({"value": "2", "currency": "USD", "active_worker_thread_ms": 7_200_000, "open_censored": 0}));
    assert_eq!(m34["allocation"], json!({"rule": "coordinator-allocation-v2", "rule_text": "each priced coordinator entry is split evenly across the tasks with an attempt running at its record time (an attempt without a terminal mark runs on, open-ended); none running: unallocated; an unknown activity span over it, or no record time: allocation_unknown",
        "coordinator_total": usd("4"), "unpriced_entries": 0, "currency": "USD", "by_task": {"work": "4"}, "unallocated": "0", "allocation_unknown": "0",
        "allocation_unknown_entries": {}}));
    assert_eq!(m34["excluded_from"], json!(["M35", "per_arm_worker_figures"]));
    assert!(f.text(&["accounting", "fleet"]).lines().any(|l| l == "M34 coordinator_overhead 1/5"));
    // The coordinator's cost is in no attempt's estimate.
    let cost = f.cli_args(&["accounting", "cost", "--json"]).0;
    assert_eq!(cost["attempts"].as_array().unwrap().iter().find(|a| a["attempt_id"] == f.attempt.as_str()).unwrap()["estimate"], usd("16"));
    // M37 before anything ended: the attempt is not superseded, 0 of $16.
    let m37 = &fleet["metrics"]["M37"];
    assert_eq!((&m37["value"], &m37["buckets"]["not_superseded"]), (&json!("0"), &json!({"attempts": 1, "estimate": usd("16")})));

    // The owner cancels the attempt and runs the task again (a second attempt, $4).
    f.readmit("codex");
    let second: String = raw.query_row("SELECT id FROM attempts WHERE state='reserved'", [], |r| r.get(0)).unwrap();
    let decided: i64 = raw.query_row("SELECT decided_unix_ms FROM dispatch_decisions WHERE attempt_id=?1", [&second], |r| r.get(0)).unwrap();
    raw.execute("INSERT INTO collector_bindings(attempt_id,revision,state,collector,execution_home,unix_ms,source) VALUES(?1,1,'active','codex',?2,?3,'apply_launch_started')",
        rusqlite::params![second, f.home.display().to_string(), decided]).unwrap();
    priced_rollout(&f.home, "second", "00000000-0000-4000-8000-00000000f005", &format!("{}/.state/worktrees/{second}/repo-00", f.project.display()), decided + 1, "gpt-5.5");
    let fleet = fleet_after_reprice(&f);
    assert_eq!(fleet["metrics"]["M34"]["value"], json!("1/6"), "$4 / ($4 + $16 + $4)");
    let m37 = &fleet["metrics"]["M37"];
    assert_eq!((&m37["value"], &m37["buckets"]["unexplained_abandonment"], &m37["buckets"]["sibling_changed_same_area"]),
        (&json!("0"), &json!({"attempts": 1, "estimate": usd("16")}), &json!({"attempts": 0, "estimate": {"status": "complete", "currency": null, "amount": "0"}})),
        "a cancelled attempt without a reason is unexplained, never overlap waste");

    // A sibling thread's attempt that changed the same area (planted: it has no usage and never ran here).
    raw.execute_batch("INSERT INTO tasks(id,revision,state,title) VALUES('sibling',1,'succeeded','sibling');
        INSERT INTO attempts(id,task_id,revision,state,reservation,termination_observed) VALUES('s1','sibling',2,'completed','s1',1);").unwrap();
    let request = |attempt: &str, reason: &str, sibling: Option<&str>| herdr_projects::store::SupersessionRequest { attempt: attempt.into(), outcome: "superseded".into(),
        reason: reason.into(), sibling: sibling.map(str::to_owned), evidence: vec!["attempt:s1".into(), "commit:0123abcd".into()] };
    // Workers and imports are refused; so is a forged raw row and any edit.
    let mut store = herdr_projects::store::SqliteStore::open(&state).unwrap();
    for principal in ["worker:w1", f.attempt.as_str(), "import:report"] {
        let err = format!("{:?}", store.record_attempt_supersession(&request(&f.attempt, "sibling_changed_same_area", Some("s1")), principal, 1).unwrap_err());
        assert!(err.contains("cannot record a supersession reason"), "{principal}: {err}");
    }
    assert!(raw.execute("INSERT INTO attempt_supersessions(attempt_id,task_id,outcome,reason,sibling_attempt_id,evidence,principal,authority,canonical_json,recorded_unix_ms)
        VALUES(?1,'work','superseded','sibling_changed_same_area','s1','[\"attempt:s1\"]','worker:w1','operator_owner.v1','{}',1)", [&f.attempt]).is_err());
    let err = format!("{:?}", store.record_attempt_supersession(&request(&second, "duplicate_effort", Some("s1")), "operator:cli", 1).unwrap_err());
    assert!(err.contains("only an ended attempt"), "{err}");
    drop(store);
    // On the CLI a worker execution context is refused before any write.
    let out = Command::new(BIN).env_clear().env("HOME", &f.home).env("PATH", "/usr/bin:/bin")
        .args(["--root", f.root.to_str().unwrap(), "telemetry", "demo", "accounting", "supersede", &f.attempt, "--outcome", "superseded",
            "--reason", "sibling_changed_same_area", "--sibling", "s1", "--evidence", "attempt:s1"]).output().unwrap();
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("HOME is a worker execution home"), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(f.cli_fail(&["accounting", "supersede", &f.attempt, "--outcome", "superseded", "--reason", "sibling_changed_same_area", "--evidence", "attempt:s1"])
        .contains("names the sibling attempt"));
    assert!(f.cli_fail(&["accounting", "supersede", &f.attempt, "--outcome", "superseded", "--reason", "other", "--evidence", "see the chat"])
        .contains("is not a reference"));
    let args = ["accounting", "supersede", &f.attempt, "--outcome", "superseded", "--reason", "sibling_changed_same_area", "--sibling", "s1",
        "--evidence", "commit:0123abcd", "--evidence", "attempt:s1"];
    let (recorded, _) = f.cli_args(&args);
    assert_eq!(recorded["supersession"], json!({"attempt_id": f.attempt, "task_id": "work", "outcome": "superseded", "reason": "sibling_changed_same_area",
        "sibling_attempt_id": "s1", "evidence": ["attempt:s1", "commit:0123abcd"], "principal": "operator:cli", "authority": "operator_owner.v1",
        "recorded_unix_ms": recorded["supersession"]["recorded_unix_ms"], "recorded": true}));
    assert_eq!(f.cli_args(&args).0["supersession"]["recorded"], false, "the same reason again is a no-op");
    assert!(f.cli_fail(&["accounting", "supersede", &f.attempt, "--outcome", "abandoned", "--reason", "other", "--evidence", "attempt:s1"]).contains("append-only"));
    assert!(raw.execute("UPDATE attempt_supersessions SET reason='other'", []).is_err());

    let fleet = f.cli_args(&["accounting", "fleet", "--json"]).0;
    let m37 = &fleet["metrics"]["M37"];
    assert_eq!(m37["value"], json!("4/5"), "$16 superseded by a sibling's change of the same area / $20 lifecycle cost");
    assert_eq!((&m37["records"], &m37["buckets"]["sibling_changed_same_area"], &m37["buckets"]["unexplained_abandonment"]["attempts"], &m37["buckets"]["not_superseded"]),
        (&json!({"sibling_changed_same_area": 1}), &json!({"attempts": 1, "estimate": usd("16")}), &json!(0), &json!({"attempts": 1, "estimate": usd("4")})));
    assert_eq!(m37["total_lifecycle_cost"]["estimate"], usd("20"));
    assert_eq!(f.report()["metrics"]["M37"], m37.clone());

    // A coordinator record without a rate (gpt-5.5-mini): unknown coordinator usage makes M34 partial.
    priced_rollout(&other_home, "coordinator-mini", "00000000-0000-4000-8000-00000000c002", &f.project.display().to_string(), coordinator_at, "gpt-5.5-mini");
    let fleet = fleet_after_reprice(&f);
    let m34 = &fleet["metrics"]["M34"];
    assert_eq!(m34["value"], json!({"status": "partial", "reasons": ["coordinator_entries_unpriced"], "priced_share": "1/6"}));
    assert_eq!(m34["coordinator"]["estimate"], json!({"status": "partial", "gaps": ["entries_unpriced"], "currency": "USD", "priced_amount": "4"}));
    assert_eq!(m34["per_active_worker_thread_hour"]["value"], json!({"status": "partial", "gaps": ["entries_unpriced"], "priced_value": "2"}));
    assert!(f.text(&["accounting", "fleet"]).lines().any(|l| l == "M34 coordinator_overhead partial 1/6 (coordinator_entries_unpriced)"));

    // Doc 05 §5a: a second task running over the coordinator's record → $2 to each task under
    // rule v1, the $4 total still shown; 4 worker-thread-hours → $1 per hour. Its usage was
    // not observed, so the ratios stay partial.
    raw.execute_batch(&format!("INSERT INTO tasks(id,revision,state,title) VALUES('other',1,'running','other');
        INSERT INTO attempts(id,task_id,revision,state,reservation,termination_observed) VALUES('o1','other',2,'completed','o1',1);
        INSERT INTO attempt_lifecycle(attempt_id,state,attempt_revision,unix_ms,source) VALUES('o1','reserved',2,{r},'fixture'),('o1','running',2,{r},'fixture'),('o1','completed',2,{c},'fixture');",
        r = f.decided - 7_200_000, c = f.decided)).unwrap();
    let fleet = f.cli_args(&["accounting", "fleet", "--json"]).0;
    let m34 = &fleet["metrics"]["M34"];
    assert_eq!((&m34["allocation"]["by_task"], &m34["allocation"]["unallocated"], &m34["allocation"]["coordinator_total"]["priced_amount"]),
        (&json!({"other": "2", "work": "2"}), &json!("0"), &json!("4")));
    assert_eq!((&m34["per_active_worker_thread_hour"]["value"]["priced_value"], &m34["per_active_worker_thread_hour"]["active_worker_thread_ms"]), (&json!("1"), &json!(14_400_000)));
    assert_eq!(m34["value"], json!({"status": "partial", "reasons": ["coordinator_entries_unpriced", "worker_usage_not_observed"], "priced_share": "1/6"}));
    assert_eq!(fleet["metrics"]["M37"]["value"], json!({"status": "partial", "reasons": ["usage_not_observed"], "priced_share": "4/5"}));

    // Rule v2 reads only reproducible facts: a coordinator record without a line
    // time (its fallback interval ends at its first observation, which a rebuild
    // moves) is allocation_unknown, never placed by when it was collected.
    let dir = other_home.join(".codex/sessions/2026/09/28");
    let untimed = dir.join("rollout-2026-09-28T00-00-00-coordinator-untimed.jsonl");
    priced_rollout(&other_home, "coordinator-untimed", "00000000-0000-4000-8000-00000000c003", &f.project.display().to_string(), coordinator_at, "gpt-5.5");
    let text: String = fs::read_to_string(&untimed).unwrap().lines().map(|l| {
        let mut v: serde_json::Value = serde_json::from_str(l).unwrap();
        if v["type"] == "token_usage_record" { v.as_object_mut().unwrap().remove("timestamp"); }
        format!("{v}\n")
    }).collect();
    fs::write(&untimed, text).unwrap();
    let fleet = fleet_after_reprice(&f);
    let allocation = &fleet["metrics"]["M34"]["allocation"];
    assert_eq!((&allocation["by_task"], &allocation["unallocated"], &allocation["allocation_unknown"], &allocation["allocation_unknown_entries"]),
        (&json!({"other": "2", "work": "2"}), &json!("0"), &json!("4"), &json!({"usage_time_unknown": 1})));

    // Rebuild: delete the sidecar, collect, sync, re-import the same card and
    // reprice. The allocation (and all of M34) is identical.
    let original = fleet["metrics"]["M34"].clone();
    for name in ["telemetry.db", "telemetry.db-wal", "telemetry.db-shm"] { let _ = fs::remove_file(f.project.join(".state").join(name)); }
    f.cli("collect");
    assert_eq!(f.cli_args(&["accounting", "import-rate-card", card.to_str().unwrap()]).0["imported"], true);
    let rebuilt = fleet_after_reprice(&f);
    assert_eq!(rebuilt["metrics"]["M34"]["allocation"], original["allocation"], "rebuild: the same allocation");
    assert_eq!(rebuilt["metrics"]["M34"], original, "rebuild: the same M34");
}

/// Collect in stages, including an older session and repeated responses, then
/// compare the public projections with a forced rebuild of the same evidence.
#[test]
fn incremental_accounting_matches_full_rebuild_after_late_and_corrected_records() {
    use std::io::Write;
    let f = Fixture::new();
    let path = f.rollout(&f.home, "incremental", &[RECORD], &f.worktree(), f.decided + 10_000, "0.154.0");
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    let original = f.cli_args(&["accounting", "entries"]).1;
    let line = fs::read_to_string(&path).unwrap().lines().last().unwrap().to_owned();
    // Same native response/payload at a later ordinal is still excluded.
    writeln!(fs::OpenOptions::new().append(true).open(&path).unwrap(), "{line}").unwrap();
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(f.cli_args(&["accounting", "status"]).0["sync"]["mode"], "incremental");
    let entries = f.cli_args(&["accounting", "entries"]).0;
    assert!(entries["entries"].as_array().unwrap().iter().any(|e| e["provenance"][0]["reason"] == "response_repeated"));
    // The next unique response arrives out of event-time order in this session.
    let early = jiff::Timestamp::from_millisecond(f.decided + 500).unwrap().to_string();
    let late = line.replace("resp-1", "late-response").replace(
        &jiff::Timestamp::from_millisecond(f.decided + 10_000).unwrap().to_string(), &early);
    writeln!(fs::OpenOptions::new().append(true).open(&path).unwrap(), "{late}").unwrap();
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    // A session arriving later has an earlier native record time.
    let older = f.rollout(&f.home, "older", &[RECORD], &f.worktree(), f.decided + 1_000, "0.154.0");
    fs::write(&older, fs::read_to_string(&older).unwrap().replace(SID, "00000000-0000-4000-8000-00000000c0df")).unwrap();
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(f.report()["metrics"]["M08"]["value"], 3000);
    assert_eq!(f.report()["metrics"]["M09"]["value"], 900);
    let before = (f.cli_args(&["accounting", "entries"]).1, f.cli_args(&["accounting", "sessions"]).1);
    assert_ne!(before.0, original);
    f.sidecar().execute("DELETE FROM usage_ledger", []).unwrap();
    f.cli_args(&["accounting", "sync"]);
    assert_eq!((f.cli_args(&["accounting", "entries"]).1, f.cli_args(&["accounting", "sessions"]).1), before);
    // Replacing a native identity with a contradictory payload quarantines it;
    // a source replacement re-reads byte zero and supersedes its projection.
    fs::write(&path, fs::read_to_string(&path).unwrap().replace("1000", "1100").replace("1300", "1400")).unwrap();
    f.sidecar().execute("UPDATE collect_offsets SET byte_offset=0", []).unwrap();
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    let corrected = f.cli_args(&["accounting", "entries"]).1;
    f.sidecar().execute("DELETE FROM usage_ledger", []).unwrap();
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(f.cli_args(&["accounting", "entries"]).1, corrected);
}

#[test]
fn accounting_reread_and_retention_record_full_rebuild_reasons() {
    let f = Fixture::new();
    f.rollout(&f.home, "reread", &[RECORD], &f.worktree(), f.decided + 1_000, "0.154.0");
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    let first = f.cli_args(&["accounting", "entries"]).1;
    // Removing the collected metadata requests the collector's real backfill
    // path, which resets the source and reads again from byte zero.
    f.sidecar().execute("DELETE FROM rollout_metadata", []).unwrap();
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    let status = f.cli_args(&["accounting", "status"]).0;
    assert_eq!(status["sync"]["mode"], "full_rebuild");
    assert_eq!(status["sync"]["rebuild_reason"], "source_reread_from_zero");
    assert_eq!(f.cli_args(&["accounting", "entries"]).1, first);
    f.cancel_reserved();
    let plan = f.cli_args(&["maintenance", "plan", "--forget-session", SID, "--json"]).0;
    f.cli_args(&["maintenance", "apply", "--forget-session", SID, "--confirm", plan["plan_digest"].as_str().unwrap(), "--json"]);
    f.cli_args(&["accounting", "sync"]);
    let status = f.cli_args(&["accounting", "status"]).0;
    assert_eq!(status["sync"]["mode"], "full_rebuild");
    assert_eq!(status["sync"]["rebuild_reason"], "retention_enforcement");
    assert_eq!(f.cli_args(&["accounting", "entries"]).0, json!({"entries": []}));
    let query = f.cli_args(&["query", "--metric", "M08,M09", "--json"]).0;
    for metric in query["results"].as_array().unwrap() {
        assert_eq!(metric["value"], json!({"status": "unavailable", "reason": "no_certified_source"}));
    }
    f.cli_args(&["analytics", "refresh"]);
    assert_eq!(f.cli_args(&["analytics", "rebuild", "--verify"]).0["identical"], true);
}

#[test]
fn killed_incremental_accounting_sync_resumes_atomically() {
    use std::io::Write;
    let f = Fixture::new();
    let path = f.rollout(&f.home, "crash", &[RECORD], &f.worktree(), f.decided + 1_000, "0.154.0");
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    let first = f.cli_args(&["accounting", "entries"]).1;
    let line = fs::read_to_string(&path).unwrap().lines().last().unwrap().to_owned();
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    for n in 0..8_000 { writeln!(file, "{}", line.replace("resp-1", &format!("crash-{n}")).replace("turn-1", &format!("crash-turn-{n}"))).unwrap(); }
    drop(file);
    f.cli("collect");
    let mark: i64 = f.sidecar().query_row("SELECT watermark FROM accounting_stream", [], |r| r.get(0)).unwrap();
    let mut child = Command::new(BIN).env_clear().env("HOME", f.tmp.path().join("home")).env("PATH", "/usr/bin:/bin")
        .args(["--root", f.root.to_str().unwrap(), "telemetry", "demo", "accounting", "sync"])
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).spawn().unwrap();
    let probe = f.sidecar();
    probe.busy_timeout(Duration::ZERO).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        assert!(child.try_wait().unwrap().is_none(), "sync completed before fault injection");
        match probe.execute_batch("BEGIN IMMEDIATE") {
            Ok(()) => { probe.execute_batch("ROLLBACK").unwrap(); }
            Err(e) if e.sqlite_error_code() == Some(rusqlite::ErrorCode::DatabaseBusy) => break,
            Err(e) => panic!("{e}"),
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    // The writer holds the sync transaction. Give it time to replace rows,
    // then kill before its large session has finished deriving/storing.
    std::thread::sleep(Duration::from_millis(30));
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());
    assert_eq!(f.cli_args(&["accounting", "entries"]).1, first);
    assert_eq!(f.sidecar().query_row("SELECT watermark FROM accounting_stream", [], |r| r.get::<_, i64>(0)).unwrap(), mark);
    f.cli_args(&["accounting", "sync"]);
    let resumed = (f.cli_args(&["accounting", "entries"]).1, f.cli_args(&["accounting", "sessions"]).1);
    f.sidecar().execute("DELETE FROM usage_ledger", []).unwrap();
    f.cli_args(&["accounting", "sync"]);
    assert_eq!((f.cli_args(&["accounting", "entries"]).1, f.cli_args(&["accounting", "sessions"]).1), resumed);
}

#[test]
fn restored_accounting_frontier_requires_a_full_rebuild() {
    let f = Fixture::new();
    f.rollout(&f.home, "backup", &[RECORD], &f.worktree(), f.decided + 1_000, "0.154.0");
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    let first = f.cli_args(&["accounting", "entries"]).1;
    let metrics = || f.cli_args(&["query", "--metric", "M08,M09", "--json"]).0["results"].as_array().unwrap()
        .iter().map(|r| (r["value"].clone(), r["coverage"].clone())).collect::<Vec<_>>();
    let aggregate = metrics();
    assert_eq!(aggregate[0].0, json!(1000));
    assert_eq!(aggregate[1].0, json!(300));
    f.cli_args(&["analytics", "refresh"]);
    let backup = f.tmp.path().join("accounting-backup");
    f.cli_args(&["backup", "create", "--out", backup.to_str().unwrap()]);
    f.cli_args(&["backup", "restore", "--from", backup.to_str().unwrap(), "--force"]);
    assert_eq!(metrics(), aggregate, "invalidated aggregates fall back to native replay");
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(f.cli_args(&["accounting", "status"]).0["sync"]["rebuild_reason"], "sidecar_restore");
    assert_eq!(f.cli_args(&["accounting", "entries"]).1, first);
    assert_eq!(metrics(), aggregate, "restore rebuild preserves the aggregate answers");
    f.sidecar().execute("UPDATE accounting_stream SET watermark=sequence+1", []).unwrap();
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(f.cli_args(&["accounting", "status"]).0["sync"]["rebuild_reason"], "watermark_inconsistent");
    assert_eq!(f.cli_args(&["accounting", "entries"]).1, first);
    // A backup from accounting 11 has no frontier. Restoring it upgrades the
    // sidecar and still records restore as the reason for the first rebuild.
    f.sidecar().execute_batch("UPDATE telemetry_streams SET version=11 WHERE stream='accounting';
        DROP TABLE accounting_dirty_sessions; DROP TABLE accounting_stream;").unwrap();
    let legacy = f.tmp.path().join("legacy-accounting-backup");
    f.cli_args(&["backup", "create", "--out", legacy.to_str().unwrap()]);
    f.cli_args(&["backup", "restore", "--from", legacy.to_str().unwrap(), "--force"]);
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(f.cli_args(&["accounting", "status"]).0["sync"]["rebuild_reason"], "sidecar_restore");
    assert_eq!(f.cli_args(&["accounting", "entries"]).1, first);

}

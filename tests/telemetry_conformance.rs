//! Lane A adapter conformance (TM1.6, docs/telemetry/contracts-collection.md
//! A3): the Codex adapter run on the CLI over a shared corpus of rollout
//! shapes. Each test covers one gate property the other telemetry crates do
//! not already prove: replay of the whole corpus in any chunking, unknown
//! input ignored, malformed lines quarantined with reasons, binding required
//! on every attributing output, version gating, planted sentinels, unknown as
//! unavailable, and `collectors capabilities` matching what is emitted. A4
//! (TM1.3 remainder) adds session metadata, per-record times, subagent and
//! guardian child sessions, model switches, resume across files and the
//! upgrade of a sidecar read before A4.

#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.

mod support;

use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::{fs, path::{Path, PathBuf}};
use support::telemetry::*;

/// `codex-conformance/edge.jsonl`, relative to the Codex fixture directory.
const EDGE: &str = "../codex-conformance/edge.jsonl";
const EDGE_SID: &str = "00000000-0000-4000-8000-0000000a3ed6";
const OLD_SID: &str = "00000000-0000-4000-8000-0000000a3001";
/// A4 child sessions: a `thread_spawn` subagent of the edge session with a
/// model switch, and a guardian (`review` subagent, `codex-auto-review`).
const CHILD: &str = "../codex-conformance/child.jsonl";
const CHILD_SID: &str = "00000000-0000-4000-8000-0000000a4c01";
const GUARDIAN: &str = "../codex-conformance/guardian.jsonl";
const GUARDIAN_SID: &str = "00000000-0000-4000-8000-0000000a4c02";
/// One more turn of a session, appended after its replayed history.
const RESUMED: &str = "../codex-conformance/resumed.jsonl";

/// Where a corpus rollout is written, relative to the fixture's one attempt.
#[derive(Clone, Copy)]
enum Place {
    /// Its home, its worktree, after its decision: bound.
    Bound,
    /// Its home and time, but a cwd outside every attempt worktree.
    CwdOutside,
    /// The retained `other` Codex profile's home, which no attempt uses.
    OtherHome,
    /// Its home and worktree, a minute before the decision.
    BeforeDecision,
}

struct Case {
    name: &'static str,
    sid: &'static str,
    parts: &'static [&'static str],
    version: &'static str,
    place: Place,
}

/// The shared conformance corpus. `complete`, `edge`, `child` and `guardian`
/// are certified and bound; `uncertified` is bound with a version no live run
/// certified; the rest are certified but must stay unbound.
const CASES: &[Case] = &[
    Case { name: "complete", sid: SID, parts: &["head.jsonl", "tail.jsonl"], version: "0.154.0", place: Place::Bound },
    Case { name: "edge", sid: EDGE_SID, parts: &["head.jsonl", EDGE], version: "0.154.0", place: Place::Bound },
    Case { name: "uncertified", sid: OLD_SID, parts: &["head.jsonl", "tail.jsonl"], version: "0.999.0", place: Place::Bound },
    Case { name: "cwd-outside", sid: "00000000-0000-4000-8000-0000000a3002", parts: &["head.jsonl"], version: "0.154.0", place: Place::CwdOutside },
    Case { name: "other-home", sid: "00000000-0000-4000-8000-0000000a3003", parts: &["head.jsonl"], version: "0.154.0", place: Place::OtherHome },
    Case { name: "earlier", sid: "00000000-0000-4000-8000-0000000a3004", parts: &["head.jsonl"], version: "0.154.0", place: Place::BeforeDecision },
    Case { name: "child", sid: CHILD_SID, parts: &[CHILD], version: "0.154.0", place: Place::Bound },
    Case { name: "guardian", sid: GUARDIAN_SID, parts: &[GUARDIAN], version: "0.154.0", place: Place::Bound },
];

fn case(name: &str) -> &'static Case { CASES.iter().find(|c| c.name == name).unwrap() }

/// Write corpus case `name` into the fixture; returns the rollout path.
fn plant(f: &Fixture, name: &str) -> PathBuf {
    let c = case(name);
    let base = f.tmp.path().canonicalize().unwrap();
    let (home, cwd, at) = match c.place {
        Place::Bound => (f.home.clone(), f.worktree(), f.decided + 1_000),
        Place::CwdOutside => (f.home.clone(), format!("{}/repo", f.project.display()), f.decided + 1_000),
        Place::OtherHome => (base.join("other-home"), f.worktree(), f.decided + 1_000),
        Place::BeforeDecision => (f.home.clone(), f.worktree(), f.decided - 60_000),
    };
    let path = f.rollout(&home, c.name, c.parts, &cwd, at, c.version);
    fs::write(&path, fs::read_to_string(&path).unwrap().replace(SID, c.sid)).unwrap();
    path
}

/// Byte offset of each line of `path` (1-based line `n` starts at `[n - 1]`).
fn line_starts(path: &Path) -> Vec<i64> {
    let text = fs::read(path).unwrap();
    std::iter::once(0).chain(text.iter().enumerate().filter(|(_, b)| **b == b'\n').map(|(i, _)| i as i64 + 1)).collect()
}

fn source(path: &Path) -> String {
    use sha2::Digest;
    format!("sha256:{:x}", sha2::Sha256::digest(path.as_os_str().as_encoded_bytes()))
}

/// Every Codex and ingest row except receipt times, in key order.
fn ledger(f: &Fixture) -> Vec<String> {
    let db = f.sidecar();
    let mut out = Vec::new();
    for table in ["source_observations", "ingest_quarantine", "coverage_gaps", "source_cursors", "codex_usage", "codex_turns", "codex_rate_limits",
        "codex_quarantine", "codex_discrepancy", "collect_offsets", "rollout_sources", "source_bindings", "rollout_metadata", "codex_usage_times",
        "codex_rate_limit_windows"] {
        let mut stmt = db.prepare(&format!("SELECT * FROM {table} ORDER BY 1,2")).unwrap();
        let names: Vec<String> = stmt.column_names().into_iter().map(str::to_owned).collect();
        let rows = stmt.query_map([], |r| Ok(names.iter().enumerate().filter(|(_, n)| !matches!(n.as_str(), "observed_unix_ms" | "updated_unix_ms"))
            .map(|(i, n)| format!("{n}={:?}", r.get_ref(i).unwrap())).collect::<Vec<_>>().join(" "))).unwrap();
        out.extend(rows.map(|row| format!("{table}: {}", row.unwrap())));
    }
    out
}

fn remove_sidecar(f: &Fixture) {
    for name in ["telemetry.db", "telemetry.db-wal", "telemetry.db-shm"] {
        let _ = fs::remove_file(f.project.join(".state").join(name));
    }
}

/// `(sequence, reason, bytes)` per quarantined position of `source`.
fn quarantine(f: &Fixture, source: &str) -> Vec<(i64, String, i64)> {
    f.sidecar().prepare("SELECT sequence,reason,bytes FROM ingest_quarantine WHERE source=?1 ORDER BY sequence").unwrap()
        .query_map([source], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().map(Result::unwrap).collect()
}

/// `(sequence, event_kind, payload)` per envelope of `source`.
fn envelopes(f: &Fixture, source: &str) -> Vec<(i64, String, String)> {
    f.sidecar().prepare("SELECT producer_sequence,event_kind,payload FROM source_observations WHERE producer_epoch=?1 ORDER BY producer_sequence").unwrap()
        .query_map([source], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().map(Result::unwrap).collect()
}

fn attempt_usage(report: &Value) -> Value { report["attempts"][0]["usage"].clone() }
fn unavailable(reason: &str) -> Value { json!({"status": "unavailable", "reason": reason}) }

/// `complete` (1000 + 500 input, 400 + 100 cached, 120 + 60 output, 80 + 20
/// reasoning) plus `edge` (1000 + 200 + 100, 400 + 50 + 0, 120 + 30 + 10,
/// 80 + 10 + 0): the bound certified records only.
fn bound_sums() -> Value {
    json!({"input_tokens": 2800, "cached_input_tokens": 950, "cache_write_input_tokens": 0, "output_tokens": 340,
        "reasoning_output_tokens": 190, "total_tokens": 3140, "records": 5})
}

/// Gate: replaying the corpus yields identical identities, quarantines and
/// rows, whether it is read in one pass, collected again, re-read into a
/// fresh sidecar, or appended in pieces cut mid-line (including inside a
/// malformed line) with a collect after each piece.
#[test]
fn corpus_replays_identically_in_any_chunking() {
    let f = Fixture::new();
    let paths: Vec<PathBuf> = CASES.iter().map(|c| plant(&f, c.name)).collect();
    let (first, _) = f.cli("collect");
    // complete 2 + edge 3 + uncertified 2 + three unbound heads + child 2 + guardian 1.
    assert_eq!(first["collected"]["records"], 13);
    let once = ledger(&f);
    let (again, _) = f.cli("collect");
    assert_eq!((&again["collected"]["records"], &again["collected"]["bytes"]), (&0.into(), &0.into()));
    assert!(once == ledger(&f), "a second collect changes nothing");
    remove_sidecar(&f);
    f.cli("collect");
    assert!(once == ledger(&f), "a fresh sidecar re-reads to identical rows");

    // The edge rollout grows in pieces; every cut but the last ends mid-line.
    let edge = &paths[1];
    let full = fs::read(edge).unwrap();
    let starts = line_starts(edge);
    let cuts = [starts[1] as usize + 5, starts[10] as usize + 3, starts[11] as usize + 40, starts[13] as usize + 1, starts[18] as usize + 7, full.len()];
    remove_sidecar(&f);
    fs::write(edge, b"").unwrap();
    let mut written = 0;
    for cut in cuts {
        let mut file = fs::OpenOptions::new().append(true).open(edge).unwrap();
        std::io::Write::write_all(&mut file, &full[written..cut]).unwrap();
        written = cut;
        f.cli("collect");
    }
    assert!(once == ledger(&f), "a rollout read in pieces equals one read whole");
}

/// Unknown record kinds, unknown `event_msg` types and unknown fields at any
/// level are ignored without failing the collect: no envelope, no row and no
/// quarantine for an unknown kind; an allowlisted record keeps exactly its
/// allowlisted fields.
#[test]
fn unknown_kinds_and_fields_are_ignored() {
    let f = Fixture::new();
    let path = plant(&f, "edge");
    let (report, _) = f.cli("collect");
    let at = line_starts(&path);
    let key = source(&path);
    let edge_line = |n: usize| at[7 + n - 1];
    let kinds: Vec<(i64, String)> = envelopes(&f, &key).into_iter().map(|(seq, kind, _)| (seq, kind)).collect();
    // Head lines 1-3 and 5-7 (4 is a `response_item`), then edge lines 3, 4, 11, 14 and 15.
    assert_eq!(kinds, [(at[0], "codex.session_meta.v1".to_owned()), (at[1], "codex.turn_context.v1".into()), (at[2], "codex.task_started.v1".into()),
        (at[4], "codex.token_usage_record.v1".into()), (at[5], "codex.token_count.v1".into()), (at[6], "codex.task_complete.v1".into()),
        (edge_line(3), "codex.turn_context.v1".into()), (edge_line(4), "codex.token_usage_record.v1".into()), (edge_line(11), "codex.token_usage_record.v1".into()),
        (edge_line(14), "codex.task_complete.v1".into()), (edge_line(15), "codex.token_count.v1".into())]);
    let payload = |line: usize| envelopes(&f, &key).into_iter().find(|e| e.0 == edge_line(line)).unwrap().2;
    assert_eq!(payload(3), r#"{"effort":"low","model":"gpt-5.5","turn_id":"turn-3"}"#);
    assert_eq!(payload(4), format!(r#"{{"response_id":"resp-3","session_id":"{EDGE_SID}","thread_token_usage":{{"cache_write_input_tokens":0,"cached_input_tokens":450,"#)
        + r#""input_tokens":1200,"output_tokens":150,"reasoning_output_tokens":90,"total_tokens":1350},"turn_id":"turn-3","usage":{"cache_write_input_tokens":0,"#
        + r#""cached_input_tokens":50,"input_tokens":200,"output_tokens":30,"reasoning_output_tokens":10,"total_tokens":230}}"#);
    assert_eq!(payload(15), r#"{"info":{"total_token_usage":{"cache_write_input_tokens":0,"cached_input_tokens":450,"input_tokens":1300,"output_tokens":160,"#.to_owned()
        + r#""reasoning_output_tokens":90,"total_tokens":1460}},"rate_limits":{"limit_id":"codex","plan_type":"pro","primary":{"resets_at":1790003600,"#
        + r#""used_percent":50,"window_minutes":300},"rate_limit_reached_type":null,"secondary":{"resets_at":1790600000,"used_percent":"1.5","window_minutes":10080}}}"#);
    // No quarantine at an unknown kind (edge lines 1 and 2) or unknown field (3 and 4).
    let quarantined: BTreeSet<i64> = quarantine(&f, &key).into_iter().map(|q| q.0).collect();
    assert!((1..=4).all(|n| !quarantined.contains(&edge_line(n))), "{quarantined:?}");
    // Head record 1000/400/0/120/80/1120, edge records 200/50/0/30/10/230 and
    // 100/0/0/10/0/110: the last reported totals equal the sum, so no discrepancy.
    assert_eq!(attempt_usage(&report), json!({"input_tokens": 1300, "cached_input_tokens": 450, "cache_write_input_tokens": 0, "output_tokens": 160,
        "reasoning_output_tokens": 90, "total_tokens": 1460, "records": 3}));
    assert_eq!(f.count("codex_discrepancy"), 0);
}

/// A complete line that does not parse is quarantined with its position,
/// size and reason, keeps nothing of its content and yields no row or
/// envelope; the lines around it still ingest. `line_malformed`: not a JSON
/// object with a string `type` (cut mid-line, an array, no type, blank).
/// `record_malformed`: a read kind whose typed fields do not parse; it does
/// not take a usage ordinal, and a malformed `turn_context` leaves the model
/// of later records unknown rather than the previous turn's.
#[test]
fn malformed_lines_are_quarantined_with_reasons() {
    let f = Fixture::new();
    let path = plant(&f, "edge");
    let at = line_starts(&path);
    let key = source(&path);
    let line = |n: usize| (at[7 + n - 1], at[7 + n] - at[7 + n - 1]);
    let expected: Vec<(i64, String, i64)> = [(5, "line_malformed"), (6, "line_malformed"), (7, "line_malformed"), (8, "line_malformed"),
        (9, "record_malformed"), (10, "record_malformed"), (12, "record_malformed"), (13, "record_malformed")]
        .into_iter().map(|(n, reason)| (line(n).0, reason.to_owned(), line(n).1)).collect();
    for _ in 0..2 {
        let (report, _) = f.cli("collect");
        assert_eq!(quarantine(&f, &key), expected);
        assert_eq!(f.sidecar().query_row("SELECT count(*) FROM ingest_quarantine WHERE event_id IS NOT NULL OR first_digest IS NOT NULL OR new_digest IS NOT NULL",
            [], |r| r.get::<_, i64>(0)).unwrap(), 0);
        assert_eq!(report["sessions"][0]["records"], 3, "the malformed usage record takes no ordinal");
        assert_eq!(report["collected"]["budget_exhausted"], false);
    }
    type Row = (i64, String, Option<String>, Option<String>, i64);
    let usage: Vec<Row> = f.sidecar().prepare("SELECT ordinal,response_id,model,effort,accepted FROM codex_usage ORDER BY ordinal").unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).unwrap().map(Result::unwrap).collect();
    assert_eq!(usage, [(1, "resp-1".into(), Some("gpt-5.5".into()), Some("high".into()), 1), (2, "resp-3".into(), Some("gpt-5.5".into()), Some("low".into()), 1),
        (3, "resp-4".into(), None, None, 1)]);
    // Head turn-1, then edge turn-3; turn-4's task_complete did not parse.
    let turns: Vec<(String, Option<i64>)> = f.sidecar().prepare("SELECT turn_id,duration_ms FROM codex_turns ORDER BY turn_id").unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect();
    assert_eq!(turns, [("turn-1".to_owned(), Some(4200)), ("turn-3".into(), Some(900))]);
    // Head `37.5`, then edge line 15; line 12's window did not parse.
    let limits: Vec<(i64, String, i64)> = f.sidecar().prepare("SELECT ordinal,used_percent,window_minutes FROM codex_rate_limits ORDER BY ordinal").unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().map(Result::unwrap).collect();
    assert_eq!(limits, [(1, "37.5".to_owned(), 300), (2, "50".into(), 300)]);
    let cursor: (i64, i64) = f.sidecar().query_row("SELECT byte_offset,observations FROM source_cursors WHERE source=?1", [&key], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!(cursor, (fs::metadata(&path).unwrap().len() as i64, 11));
}

/// Binding is required on every output that attributes usage: rollouts that
/// are unbound (cwd outside the worktree, another home, before the decision)
/// are collected and listed, yet counted in no attempt, metric or ledger
/// attribution.
#[test]
fn unbound_rollouts_are_never_attributed() {
    let f = Fixture::new();
    for name in ["complete", "edge", "cwd-outside", "other-home", "earlier"] { plant(&f, name); }
    let (report, _) = f.cli("collect");
    assert_eq!(report["collected"]["records"], 8, "unbound rollouts are still read");
    assert_eq!(attempt_usage(&report), bound_sums());
    assert_eq!(attempt_usage(&f.cli_args(&["usage", "--json"]).0), bound_sums());
    assert_eq!(f.cli_args(&["attempts", "--json"]).0["attempts"][0]["usage"], bound_sums());
    let sessions: Vec<(String, String, Value)> = report["sessions"].as_array().unwrap().iter()
        .map(|s| (s["session_id"].as_str().unwrap().to_owned(), s["binding"].as_str().unwrap().to_owned(), s["attempt_id"].clone())).collect();
    let bound = json!(f.attempt);
    assert_eq!(sessions, [(SID.to_owned(), "bound".to_owned(), bound.clone()), (case("cwd-outside").sid.into(), "unbound".into(), Value::Null),
        (case("other-home").sid.into(), "unbound".into(), Value::Null), (case("earlier").sid.into(), "unbound".into(), Value::Null),
        (EDGE_SID.into(), "bound".into(), bound)]);
    let m08 = f.report()["metrics"]["M08"].clone();
    assert_eq!((&m08["value"], &m08["coverage"]), (&2800.into(), &json!({"certified_sessions": 2, "excluded": {"unbound": 3}})));
    // The accounting session graph keeps every rollout, attributed to an attempt only when bound.
    f.cli_args(&["accounting", "sync"]);
    let (graph, _) = f.cli_args(&["accounting", "sessions"]);
    let attributed: Vec<(String, Value)> = graph["sessions"].as_array().unwrap().iter()
        .flat_map(|s| s["rollouts"].as_array().unwrap().iter().map(|r| (s["session_id"].as_str().unwrap().to_owned(), r["attempt_id"].clone()))).collect();
    let unbound = |name: &str| (case(name).sid.to_owned(), Value::Null);
    assert_eq!(attributed, [(SID.to_owned(), json!(f.attempt)), unbound("cwd-outside"), unbound("other-home"), unbound("earlier"), (EDGE_SID.to_owned(), json!(f.attempt))]);
}

/// A version no live run certified keeps no counters in usage rows, makes its
/// attempt's usage `cli_version_uncertified` rather than a partial sum of its
/// certified sibling, is excluded from the usage metrics, still stores its
/// metadata (turns, rate limits), and its envelopes name the version so a
/// ledger reader can gate on it.
#[test]
fn uncertified_version_is_gated_everywhere() {
    let f = Fixture::new();
    plant(&f, "complete");
    let old = plant(&f, "uncertified");
    let (report, _) = f.cli("collect");
    let gated = unavailable("cli_version_uncertified");
    assert_eq!(attempt_usage(&report), gated, "not the 1500 input of the certified sibling");
    assert_eq!(f.cli_args(&["attempts", "--json"]).0["attempts"][0]["usage"], gated);
    let counters: Vec<(Option<i64>, i64, String)> = f.sidecar().prepare("SELECT input_tokens,accepted,reason FROM codex_usage WHERE session_id=?1 ORDER BY ordinal").unwrap()
        .query_map([OLD_SID], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().map(Result::unwrap).collect();
    let gated_row = (None, 0, "cli_version_uncertified".to_owned());
    assert_eq!(counters, [gated_row.clone(), gated_row]);
    let metadata = |table: &str| f.sidecar().query_row(&format!("SELECT count(*) FROM {table} WHERE session_id=?1"), [OLD_SID], |r| r.get::<_, i64>(0)).unwrap();
    assert_eq!((metadata("codex_turns"), metadata("codex_rate_limits"), metadata("codex_discrepancy")), (2, 2, 0));
    let m08 = f.report()["metrics"]["M08"].clone();
    assert_eq!((&m08["value"], &m08["coverage"]), (&1500.into(), &json!({"certified_sessions": 1, "excluded": {"cli_version_uncertified": 1}})));
    let versions: Vec<String> = f.sidecar().prepare("SELECT DISTINCT json_extract(provenance,'$.adapter_version') FROM source_observations WHERE producer_epoch=?1").unwrap()
        .query_map([source(&old)], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
    assert_eq!(versions, ["0.999.0"]);
    let (capabilities, _) = f.cli_args(&["collectors", "capabilities", "--json"]);
    assert_eq!((&capabilities["adapters"][0]["certified_versions"], &capabilities["adapters"][0]["uncertified_version"]),
        (&json!(["0.154.0"]), &json!("cli_version_uncertified")));
}

fn contains_sentinel(bytes: &[u8]) -> Option<&'static str> {
    let lower = bytes.to_ascii_lowercase();
    ["a3leak", "a4leak", "canary"].into_iter().find(|needle| lower.windows(needle.len()).any(|w| w == needle.as_bytes()))
}

/// Privacy: sentinels planted in content fields, unknown kinds, unknown
/// fields, excluded rate-limit fields and malformed lines of the whole corpus
/// reach neither the sidecar (main file, WAL, shared memory) nor any output of
/// the collector, usage, metrics, collectors and accounting commands.
#[test]
fn planted_sentinels_never_leak() {
    let f = Fixture::new();
    let paths: Vec<PathBuf> = CASES.iter().map(|c| plant(&f, c.name)).collect();
    // Every sentinel is in the corpus (the test is not vacuous).
    let corpus: Vec<u8> = paths.iter().flat_map(|p| fs::read(p).unwrap()).collect();
    for needle in ["A3LEAK_UNKNOWN_KIND", "A3LEAK_UNKNOWN_FIELD", "A3LEAK_TRUNCATED", "A3LEAK_BAD_RECORD", "A3LEAK_CREDITS", "A3LEAK_LIMIT_NAME", "CANARY_USER",
        // A4: subagent names and paths, tool arguments, commands and output, MCP results, guardian transcripts.
        "A4LEAK_AGENT_PATH", "A4LEAK_NICKNAME", "A4LEAK_ROLE", "A4LEAK_ARGUMENTS", "A4LEAK_COMMAND", "A4LEAK_OUTPUT", "A4LEAK_STDERR",
        "A4LEAK_MCP_ARGUMENTS", "A4LEAK_MCP_RESULT", "A4LEAK_CREDITS", "A4LEAK_GUARDIAN_TRANSCRIPT", "A4LEAK_GUARDIAN_VERDICT"] {
        assert!(corpus.windows(needle.len()).any(|w| w == needle.as_bytes()), "{needle}");
    }
    let mut output = f.cli("collect").1;
    // Hold a reader across the next collect so its frames stay in the WAL.
    let reader = f.sidecar();
    let _ = reader.query_row("SELECT count(*) FROM source_observations", [], |r| r.get::<_, i64>(0)).unwrap();
    remove_sidecar_rows(&f);
    output.extend(f.cli("collect").1);
    for args in [&["usage", "--json"][..], &["attempts", "--json"], &["report", "--json"], &["collectors", "status"], &["collectors", "bindings"], &["collectors", "sessions"],
        &["collectors", "capabilities", "--json"], &["accounting", "sync"], &["accounting", "entries"], &["accounting", "sessions"], &["accounting", "quota", "--json"]] {
        output.extend(f.cli_args(args).1);
    }
    for args in [&["usage"][..], &["report"], &["collectors", "capabilities"]] {
        output.extend(f.text(args).into_bytes());
    }
    let state = f.project.join(".state");
    assert!(!fs::read(state.join("telemetry.db-wal")).unwrap().is_empty(), "the second collect wrote through the WAL");
    for name in ["telemetry.db", "telemetry.db-wal", "telemetry.db-shm"] {
        assert_eq!(contains_sentinel(&fs::read(state.join(name)).unwrap()), None, "{name}");
    }
    assert_eq!(contains_sentinel(&output), None, "{}", String::from_utf8_lossy(&output));
    // complete, edge, uncertified, three unbound heads, child 7 and guardian 4.
    assert_eq!(f.count("source_observations"), 10 + 11 + 10 + 3 * 6 + 7 + 4, "the collects read the whole corpus");
    drop(reader);
}

/// Force the second collect to write every row again: drop what the first
/// wrote, as a sidecar written before the ledger would be re-read.
fn remove_sidecar_rows(f: &Fixture) {
    f.sidecar().execute_batch("DELETE FROM source_observations; DELETE FROM ingest_quarantine; DELETE FROM source_cursors; DELETE FROM collect_offsets;").unwrap();
}

/// Unknown is unavailable, never 0: before any collect, for a bound session
/// without a certified version, and for fields the adapter does not collect,
/// which `collectors capabilities` lists as unavailable with a reason instead
/// of a value. A bound session with no usage record at all is an observed 0.
#[test]
fn unknown_is_unavailable_never_zero() {
    let f = Fixture::new();
    assert_eq!(attempt_usage(&f.cli_args(&["usage", "--json"]).0), unavailable("collection_not_run"));
    // `session_meta` and `turn_context` only: observed, with no usage record.
    let path = plant(&f, "complete");
    let text = fs::read_to_string(&path).unwrap();
    fs::write(&path, text.lines().take(2).map(|l| format!("{l}\n")).collect::<String>()).unwrap();
    let (report, _) = f.cli("collect");
    assert_eq!(attempt_usage(&report), json!({"input_tokens": 0, "cached_input_tokens": 0, "cache_write_input_tokens": 0, "output_tokens": 0,
        "reasoning_output_tokens": 0, "total_tokens": 0, "records": 0}), "an observed session without usage is a real 0");
    let (capabilities, _) = f.cli_args(&["collectors", "capabilities", "--json"]);
    for field in capabilities["adapters"][0]["fields"].as_array().unwrap() {
        let collected = field["available"] == true;
        assert_eq!((field["reason"].is_null(), field["basis"] == "unavailable", field["certified"] == "none"), (collected, !collected, !collected), "{field}");
    }
    let g = Fixture::new();
    plant(&g, "uncertified");
    assert_eq!(attempt_usage(&g.cli("collect").0), unavailable("cli_version_uncertified"));
}

/// Contracts §0 finding (steward `sidecar::attempt_usage`, and the M08/M09
/// provider): a bound certified session whose only usage record failed
/// validation reports usage 0 (`records: 0`) and M08 0 although its usage is
/// unknown. Expected once fixed: `unavailable` with the M13 reason
/// `records_not_accepted`, and the session excluded from M08 coverage.
#[test]
fn rejected_records_are_unavailable_not_zero() {
    let f = Fixture::new();
    let path = plant(&f, "complete");
    let text = fs::read_to_string(&path).unwrap().lines().take(7).map(|l| format!("{l}\n")).collect::<String>();
    // total 1121 != input 1000 + output 120.
    fs::write(&path, text.replacen("\"total_tokens\":1120", "\"total_tokens\":1121", 1)).unwrap();
    let (report, _) = f.cli("collect");
    assert_eq!(report["sessions"][0]["accepted"], 0);
    assert_eq!(attempt_usage(&report), unavailable("records_not_accepted"));
    let m08 = f.report()["metrics"]["M08"].clone();
    assert_eq!((&m08["value"], &m08["coverage"]), (&unavailable("no_certified_source"), &json!({"certified_sessions": 0, "excluded": {"records_not_accepted": 1}})));
}

/// Dotted leaf paths of a sanitized payload.
fn leaves(value: &Value, prefix: &str, out: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => for (key, value) in map { leaves(value, &if prefix.is_empty() { key.clone() } else { format!("{prefix}.{key}") }, out) },
        _ => { out.insert(prefix.to_owned()); }
    }
}

/// The capability table, hand-checked against contracts §5, the sanitizer
/// allowlist and docs/telemetry/codex-live-0.154.0.md, printed as text.
const CAPABILITIES: &str = "codex rollout_jsonl certified_versions=0.154.0
  line.timestamp available=true basis=reported certified=live caveat=envelope_occurred_unix_ms
  session_meta.id available=true basis=reported certified=live
  session_meta.timestamp available=true basis=reported_excerpt certified=live
  session_meta.cwd available=true basis=reported_home_redacted certified=live
  session_meta.cli_version available=true basis=reported_excerpt certified=live
  session_meta.originator available=true basis=reported_excerpt certified=live
  session_meta.source available=true basis=reported_excerpt certified=live
  session_meta.model_provider available=true basis=reported_excerpt certified=live
  session_meta.forked_from_id available=true basis=reported certified=fixture caveat=semantics_not_certified
  session_meta.subagent_kind available=true basis=reported_excerpt certified=live caveat=from_source_subagent
  session_meta.subagent_parent_thread_id available=true basis=reported certified=fixture caveat=from_source_subagent
  session_meta.subagent_depth available=true basis=reported certified=fixture caveat=from_source_subagent
  session_meta.forked_from_ordinal_exclusive available=false basis=unavailable certified=none reason=not_collected
  session_meta.agent_nickname available=false basis=unavailable certified=none reason=not_collected
  session_meta.agent_role available=false basis=unavailable certified=none reason=not_collected
  session_meta.base_instructions available=false basis=unavailable certified=none reason=content_forbidden
  turn_context.turn_id available=true basis=reported certified=live
  turn_context.model available=true basis=reported_excerpt certified=live
  turn_context.effort available=true basis=reported_excerpt certified=live
  turn_context.cwd available=false basis=unavailable certified=none reason=not_collected
  turn_context.approval_policy available=false basis=unavailable certified=none reason=not_collected
  turn_context.collaboration_mode available=false basis=unavailable certified=none reason=content_forbidden
  turn_context.user_instructions available=false basis=unavailable certified=none reason=content_forbidden
  task_started.turn_id available=true basis=reported certified=live
  task_started.started_at available=false basis=unavailable certified=none reason=not_collected
  token_usage_record.session_id available=true basis=reported certified=live caveat=guardian_reports_parent_session
  token_usage_record.turn_id available=true basis=reported certified=live
  token_usage_record.response_id available=true basis=reported certified=live
  token_usage_record.usage.cache_write_input_tokens available=true basis=reported certified=live caveat=overlap_with_input_not_certified
  token_usage_record.usage.cached_input_tokens available=true basis=reported certified=live
  token_usage_record.usage.input_tokens available=true basis=reported certified=live
  token_usage_record.usage.output_tokens available=true basis=reported certified=live
  token_usage_record.usage.reasoning_output_tokens available=true basis=reported certified=live
  token_usage_record.usage.total_tokens available=true basis=reported certified=live
  token_usage_record.thread_token_usage.cache_write_input_tokens available=true basis=reported certified=live caveat=reconciliation_only
  token_usage_record.thread_token_usage.cached_input_tokens available=true basis=reported certified=live caveat=reconciliation_only
  token_usage_record.thread_token_usage.input_tokens available=true basis=reported certified=live caveat=reconciliation_only
  token_usage_record.thread_token_usage.output_tokens available=true basis=reported certified=live caveat=reconciliation_only
  token_usage_record.thread_token_usage.reasoning_output_tokens available=true basis=reported certified=live caveat=reconciliation_only
  token_usage_record.thread_token_usage.total_tokens available=true basis=reported certified=live caveat=reconciliation_only
  token_usage_record.thread_id available=false basis=unavailable certified=none reason=not_collected
  token_usage_record.root_turn_id available=false basis=unavailable certified=none reason=not_collected
  token_usage_record.turn_token_usage available=false basis=unavailable certified=none reason=not_collected
  token_count.info.total_token_usage.cache_write_input_tokens available=true basis=reported certified=live caveat=reconciliation_only
  token_count.info.total_token_usage.cached_input_tokens available=true basis=reported certified=live caveat=reconciliation_only
  token_count.info.total_token_usage.input_tokens available=true basis=reported certified=live caveat=reconciliation_only
  token_count.info.total_token_usage.output_tokens available=true basis=reported certified=live caveat=reconciliation_only
  token_count.info.total_token_usage.reasoning_output_tokens available=true basis=reported certified=live caveat=reconciliation_only
  token_count.info.total_token_usage.total_tokens available=true basis=reported certified=live caveat=reconciliation_only
  token_count.rate_limits.limit_id available=true basis=reported_excerpt certified=live caveat=semantics_not_certified
  token_count.rate_limits.plan_type available=true basis=reported_excerpt certified=live caveat=semantics_not_certified
  token_count.rate_limits.primary.used_percent available=true basis=reported certified=live caveat=semantics_not_certified
  token_count.rate_limits.primary.window_minutes available=true basis=reported certified=live caveat=semantics_not_certified
  token_count.rate_limits.primary.resets_at available=true basis=reported certified=live caveat=semantics_not_certified
  token_count.info.last_token_usage available=false basis=unavailable certified=none reason=not_collected
  token_count.info.model_context_window available=false basis=unavailable certified=none reason=not_collected
  token_count.rate_limits.limit_name available=false basis=unavailable certified=none reason=not_collected
  token_count.rate_limits.secondary.used_percent available=true basis=reported certified=fixture caveat=semantics_not_certified
  token_count.rate_limits.secondary.window_minutes available=true basis=reported certified=fixture caveat=semantics_not_certified
  token_count.rate_limits.secondary.resets_at available=true basis=reported certified=fixture caveat=semantics_not_certified
  token_count.rate_limits.rate_limit_reached_type available=true basis=reported_excerpt certified=fixture caveat=semantics_not_certified
  token_count.rate_limits.credits available=false basis=unavailable certified=none reason=not_collected
  task_complete.turn_id available=true basis=reported certified=live
  task_complete.duration_ms available=true basis=reported certified=live
  task_complete.time_to_first_token_ms available=true basis=reported certified=live
  task_complete.started_at available=false basis=unavailable certified=none reason=not_collected
  task_complete.completed_at available=false basis=unavailable certified=none reason=not_collected
  task_complete.last_agent_message available=false basis=unavailable certified=none reason=content_forbidden
  response_item.* available=false basis=unavailable certified=none reason=content_forbidden
  exec_command_end.* available=false basis=unavailable certified=none reason=not_collected
  mcp_tool_call_end.* available=false basis=unavailable certified=none reason=not_collected
";

/// `collectors capabilities` cannot drift from the adapter: over the whole
/// corpus the envelopes carry exactly the fields it lists as available, each
/// with a value in at least one envelope (the fixture evidence), and never a
/// field it lists as unavailable. It reads nothing, so it works on a project
/// that has never collected and creates no sidecar.
#[test]
fn capabilities_match_emitted_fields() {
    let f = Fixture::new();
    assert_eq!(f.text(&["collectors", "capabilities"]), CAPABILITIES);
    let (capabilities, _) = f.cli_args(&["collectors", "capabilities", "--json"]);
    assert!(!f.project.join(".state/telemetry.db").exists());
    let adapter = &capabilities["adapters"][0];
    assert_eq!((&adapter["adapter"], &adapter["interface"]), (&json!("codex"), &json!("rollout_jsonl")));
    assert_eq!(adapter["fields"][2], json!({"kind": "session_meta", "field": "timestamp", "available": true, "basis": "reported_excerpt",
        "certified": "live", "caveat": null, "reason": null}));
    let declared = |available: bool| adapter["fields"].as_array().unwrap().iter().filter(|f| f["available"] == available && f["kind"] != "line")
        .map(|f| format!("{}.{}", f["kind"].as_str().unwrap(), f["field"].as_str().unwrap())).collect::<BTreeSet<_>>();

    for c in CASES { plant(&f, c.name); }
    f.cli("collect");
    let (mut emitted, mut valued) = (BTreeSet::new(), BTreeSet::new());
    let rows: Vec<(String, String, Option<i64>)> = f.sidecar().prepare("SELECT event_kind,payload,occurred_unix_ms FROM source_observations").unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().map(Result::unwrap).collect();
    for (kind, payload, occurred) in &rows {
        let kind = kind.strip_prefix("codex.").and_then(|k| k.strip_suffix(".v1")).unwrap();
        let payload: Value = serde_json::from_str(payload).unwrap();
        let mut paths = BTreeSet::new();
        leaves(&payload, "", &mut paths);
        for path in paths {
            let pointer = format!("/{}", path.replace('.', "/"));
            if !payload.pointer(&pointer).unwrap().is_null() { valued.insert(format!("{kind}.{path}")); }
            emitted.insert(format!("{kind}.{path}"));
        }
        assert!(occurred.is_some(), "line.timestamp is kept for every envelope");
    }
    assert_eq!(emitted, declared(true), "every envelope field is declared available, and every available field is emitted");
    assert_eq!(valued, emitted, "every available field has fixture evidence");
    let unavailable = declared(false);
    assert!(emitted.iter().all(|path| !unavailable.iter().any(|u| path == u || path.starts_with(&format!("{u}.")))));
    assert_eq!(rows.len(), 10 + 11 + 10 + 3 * 6 + 7 + 4);
}

fn rows<T: rusqlite::types::FromSql>(f: &Fixture, sql: &str) -> Vec<Vec<T>> {
    let db = f.sidecar();
    let mut stmt = db.prepare(sql).unwrap();
    let n = stmt.column_count();
    stmt.query_map([], |r| (0..n).map(|i| r.get(i)).collect()).unwrap().map(Result::unwrap).collect()
}

/// A4 session metadata as collected, with the span of the usage record times.
fn session(f: &Fixture, sid: &str, path: &Path, records: i64, forked: Value, subagent: Value, times: [i64; 2]) -> Value {
    json!({"session_id": sid, "path_digest": source(path), "binding": "bound", "attempt_id": f.attempt, "records": records, "model_provider": "openai",
        "forked_from_id": forked, "subagent": subagent, "record_times": {"stored": records, "timed": records, "first_unix_ms": times[0], "last_unix_ms": times[1]}})
}

fn no_subagent() -> Value { json!({"kind": null, "parent_thread_id": null, "depth": null}) }

/// A4 on the CLI: the session's model provider, its fork and subagent parent
/// ids (a `thread_spawn` child names the edge session; a guardian `review`
/// child names none), each usage record's line time, a model switch inside a
/// session, and the secondary rate-limit window with the reached type. Child
/// and guardian rollouts bound to the attempt count in its usage once each.
#[test]
fn session_metadata_record_times_and_child_usage_are_collected() {
    let f = Fixture::new();
    let [complete, edge, child, guardian] = ["complete", "edge", "child", "guardian"].map(|name| plant(&f, name));
    let (report, _) = f.cli("collect");
    // complete 1500/500/0/180/100/1680 + edge 1300/450/0/160/90/1460 + child 30/5/0/9/1/39 + guardian 25/0/0/5/1/30.
    let sums = json!({"input_tokens": 2855, "cached_input_tokens": 955, "cache_write_input_tokens": 0, "output_tokens": 354,
        "reasoning_output_tokens": 192, "total_tokens": 3209, "records": 8});
    assert_eq!(attempt_usage(&report), sums);
    assert_eq!(f.report()["metrics"]["M08"]["value"], 2855);

    // Head lines carry the fixture time (decision + 1 s); the child and guardian
    // lines carry literal times from 2030-01-01T00:00:00Z = 1_893_456_000_000 ms.
    let at = f.decided + 1_000;
    let (sessions, _) = f.cli_args(&["collectors", "sessions"]);
    assert_eq!(sessions, json!({"sessions": [
        session(&f, SID, &complete, 2, Value::Null, no_subagent(), [at, at]),
        session(&f, EDGE_SID, &edge, 3, Value::Null, no_subagent(), [at, at]),
        session(&f, CHILD_SID, &child, 2, json!(EDGE_SID), json!({"kind": "thread_spawn", "parent_thread_id": EDGE_SID, "depth": 1}),
            [1_893_456_001_500, 1_893_456_003_000]),
        session(&f, GUARDIAN_SID, &guardian, 1, Value::Null, json!({"kind": "review", "parent_thread_id": null, "depth": null}),
            [1_893_456_062_000, 1_893_456_062_000]),
    ]}));

    // The child switches from gpt-5.5 to gpt-5.5-mini at its second turn.
    let usage: Vec<Vec<rusqlite::types::Value>> = rows(&f, &format!("SELECT ordinal,turn_id,model,record_unix_ms FROM codex_usage JOIN codex_usage_times USING(session_id,ordinal)
        WHERE session_id='{CHILD_SID}' ORDER BY ordinal"));
    use rusqlite::types::Value::{Integer as I, Null as N, Text as T};
    assert_eq!(usage, [vec![I(1), T("turn-c1".into()), T("gpt-5.5".into()), I(1_893_456_001_500)],
        vec![I(2), T("turn-c2".into()), T("gpt-5.5-mini".into()), I(1_893_456_003_000)]]);
    assert_eq!(rows::<String>(&f, &format!("SELECT model FROM codex_usage WHERE session_id='{GUARDIAN_SID}'")), [vec!["codex-auto-review".to_owned()]]);

    // Primary and secondary windows per snapshot: head lines 6 (and tail 11)
    // report no secondary; edge line 15 reports one; edge line 12 did not parse.
    let limits: Vec<Vec<rusqlite::types::Value>> = rows(&f, "SELECT session_id,used_percent,secondary_used_percent,secondary_window_minutes,secondary_resets_at,
        rate_limit_reached_type FROM codex_rate_limits JOIN codex_rate_limit_windows USING(session_id,ordinal) ORDER BY session_id,ordinal");
    let none = |sid: &str, used: &str| vec![T(sid.into()), T(used.into()), N, N, N, N];
    assert_eq!(limits, [none(SID, "37.5"), none(SID, "42.5"), none(EDGE_SID, "37.5"),
        vec![T(EDGE_SID.into()), T("50".into()), T("1.5".into()), I(10080), I(1_790_600_000), N],
        vec![T(CHILD_SID.into()), T("12.5".into()), T("3".into()), I(10080), I(1_790_600_000), T("primary".into())]]);
    // Each child's reported totals equal its records: no discrepancy.
    assert_eq!(rows::<i64>(&f, &format!("SELECT count(*) FROM codex_discrepancy WHERE session_id IN ('{CHILD_SID}','{GUARDIAN_SID}')")), [vec![0]]);
}

/// Resume across files: a second rollout of the same session that replays
/// its history dedupes by `(session, ordinal)` and payload digest, so only
/// its new record counts and the reported thread total still reconciles. A
/// third that restarts the ordinals with other records is quarantined, and
/// the session's usage becomes unavailable rather than a sum.
#[test]
fn resume_across_files_dedupes_history_and_quarantines_an_ordinal_restart() {
    let f = Fixture::new();
    let complete = plant(&f, "complete");
    let resumed = f.rollout(&f.home, "resumed", &["head.jsonl", "tail.jsonl", RESUMED], &f.worktree(), f.decided + 1_000, "0.154.0");
    let (report, _) = f.cli("collect");
    assert_eq!(report["collected"]["records"], 3, "the replayed history stores nothing");
    // 1500/500/0/180/100/1680 once, plus the resumed record 7/0/0/3/0/10.
    assert_eq!(attempt_usage(&report), json!({"input_tokens": 1507, "cached_input_tokens": 500, "cache_write_input_tokens": 0, "output_tokens": 183,
        "reasoning_output_tokens": 100, "total_tokens": 1690, "records": 3}));
    use rusqlite::types::Value::{Integer as I, Text as T};
    let at = f.decided + 1_000;
    assert_eq!(rows::<rusqlite::types::Value>(&f, "SELECT ordinal,path_digest,response_id,record_unix_ms FROM codex_usage JOIN codex_usage_times USING(session_id,ordinal) ORDER BY ordinal"),
        [vec![I(1), T(source(&complete)), T("resp-1".into()), I(at)], vec![I(2), T(source(&complete)), T("resp-2".into()), I(at)],
            vec![I(3), T(source(&resumed)), T("resp-r".into()), I(1_893_456_121_000)]]);
    assert_eq!(rows::<i64>(&f, "SELECT count(*) FROM codex_discrepancy WHERE kind='thread_total'"), [vec![0]]);
    assert_eq!(f.count("codex_quarantine"), 0);

    // A new file of the same session with only its session_meta and the resumed turn.
    let text = fs::read_to_string(&resumed).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    fs::write(resumed.with_file_name("rollout-2026-09-28T00-00-00-restarted.jsonl"), format!("{}\n{}\n{}\n", lines[0], lines[12], lines[13])).unwrap();
    let (report, _) = f.cli("collect");
    assert_eq!(attempt_usage(&report), unavailable("quarantined"));
    // Its first record takes ordinal 1: resp-r (payload hashed with sha256sum outside the crate) against resp-1.
    assert_eq!(rows::<rusqlite::types::Value>(&f, "SELECT session_id,ordinal,first_digest,new_digest FROM codex_quarantine"),
        [vec![T(SID.into()), I(1), T(DIGEST_1.into()), T("sha256:924252b6d27afbe340fd147fd3747364940dd51917cf08c3ca05be4ad9d394b4".into())]]);
}

/// The sidecar as the A3 binary left it: no A4 tables, ingest stream 3, and
/// envelopes of the narrower `session_meta`/`token_count` allowlist.
fn downgrade_to_a3(f: &Fixture) {
    f.sidecar().execute_batch("DROP TABLE rollout_metadata; DROP TABLE codex_usage_times; DROP TABLE codex_rate_limit_windows;
        UPDATE telemetry_streams SET version=3 WHERE stream='ingest';
        UPDATE source_observations SET payload='{}',payload_digest='sha256:a3',
            measurement='{\"coverage\":\"complete\",\"measurement_basis\":\"reported\",\"normalization_version\":1}'
            WHERE event_kind IN ('codex.session_meta.v1','codex.token_count.v1');").unwrap();
}

/// Upgrade: a sidecar written before A4 is read (read-only) with its A4
/// metadata `unavailable: predates_collection`, never `null`. The next
/// collect migrates it and reads every rollout again: the A4 columns are
/// filled, the narrower envelopes are superseded without a digest conflict,
/// nothing is counted twice, and the ledger equals a fresh collect. A rollout
/// gone before it could be read again stays `pending_reread`.
#[test]
fn rollouts_read_before_a4_gain_their_metadata_on_the_next_collect() {
    let f = Fixture::new();
    let child = ["complete", "edge", "child"].map(|name| plant(&f, name))[2].clone();
    f.cli("collect");
    let (fresh, fresh_usage, fresh_sessions) = (ledger(&f), f.cli_args(&["usage", "--json"]).0, f.cli_args(&["collectors", "sessions"]).0);

    downgrade_to_a3(&f);
    let (before, _) = f.cli_args(&["collectors", "sessions"]);
    let predates = unavailable("predates_collection");
    for s in before["sessions"].as_array().unwrap() {
        assert_eq!([&s["model_provider"], &s["forked_from_id"], &s["subagent"], &s["record_times"]], [&predates; 4], "{s}");
    }
    let (upgraded, _) = f.cli("collect");
    assert_eq!(upgraded["collected"]["records"], 0, "the re-read counts nothing twice");
    assert_eq!(f.cli_args(&["collectors", "status"]).0, json!({"stream": "ingest", "version": 4}));
    assert!(fresh == ledger(&f), "the upgraded sidecar equals a fresh collect");
    assert_eq!(f.cli_args(&["usage", "--json"]).0, fresh_usage);
    assert_eq!(f.cli_args(&["collectors", "sessions"]).0, fresh_sessions);

    downgrade_to_a3(&f);
    fs::remove_file(&child).unwrap();
    f.cli("collect");
    let (sessions, _) = f.cli_args(&["collectors", "sessions"]);
    let pending = unavailable("pending_reread");
    let child_row = sessions["sessions"].as_array().unwrap().iter().find(|s| s["session_id"] == CHILD_SID).unwrap();
    assert_eq!([&child_row["model_provider"], &child_row["forked_from_id"], &child_row["subagent"], &child_row["record_times"]], [&pending; 4]);
    assert_eq!(sessions["sessions"][0]["model_provider"], "openai");
    assert_eq!(f.cli_args(&["usage", "--json"]).0, fresh_usage, "the gone rollout's records still count");
}

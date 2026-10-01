//! Lane A adapter conformance (TM1.6, docs/telemetry/contracts-collection.md
//! A3): the Codex adapter run on the CLI over a shared corpus of rollout
//! shapes. Each test covers one gate property the other telemetry crates do
//! not already prove: replay of the whole corpus in any chunking, unknown
//! input ignored, malformed lines quarantined with reasons, binding required
//! on every attributing output, version gating, planted sentinels, unknown as
//! unavailable, and `collectors capabilities` matching what is emitted. A4
//! (TM1.3 remainder) adds session metadata, per-record times, subagent and
//! guardian child sessions, model switches, resume across files and the
//! upgrade of a sidecar read before A4. A5 adds the thread lineage a rollout
//! reports outside `source` and a guardian shaped like the live one, whose
//! usage records report its parent's session id. A6 adds tool call and exec
//! item metadata in the live 0.154.0 shape, with sentinels in every content
//! field beside it. A7 marks envelopes of an uncertified version and
//! supersedes the mark on certification, records a rollout idle without its
//! last turn's final event (tests/telemetry_collect.rs), and collects the
//! guardian's `source.subagent.other` tag. The second live run adds the MCP,
//! failed-command, subagent, aborted-turn and fork shapes it observed, which
//! A8 collects under the steward's §7 allowlist: MCP call metadata, subagent
//! and collab item ids, an aborted turn as a final event, a function call's
//! namespace, and a fork's point, against which its totals reconcile.

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
/// model switch, and a guardian of the edge session in the live 0.154.0 shape
/// (`other: guardian` subagent, `codex-auto-review`, `parent_thread_id` and
/// `session_id` the edge session's, as are its usage records' `session_id`).
const CHILD: &str = "../codex-conformance/child.jsonl";
const CHILD_SID: &str = "00000000-0000-4000-8000-0000000a4c01";
const GUARDIAN: &str = "../codex-conformance/guardian.jsonl";
const GUARDIAN_SID: &str = "00000000-0000-4000-8000-0000000a4c02";
/// A6: a session with an approved `exec` tool call, a `wait` call, their
/// outputs, `item_completed` items (a `CommandExecution`, an agent and a user
/// message), wrongly typed metadata and an output without a call, shaped like
/// the live census (codex-live-0.154.0-a4.md §4); `A6LEAK_*` in every
/// content field.
const TOOLS: &str = "../codex-conformance/tools.jsonl";
const TOOLS_SID: &str = "00000000-0000-4000-8000-0000000a6001";
/// One more turn of a session, appended after its replayed history.
const RESUMED: &str = "../codex-conformance/resumed.jsonl";
/// Second live run (docs/telemetry/codex-live-0.154.0-run2.md): record shapes
/// Codex 0.154.0 wrote, hand-written with `LIVE2LEAK_*` sentinels in every
/// content field. `live2-tools.jsonl`: an MCP call (an `exec` custom tool call
/// whose only item is an `McpToolCall`), a command that runs about 3 s, a
/// failed command (`status` `failed`, exit 2), a spawned subagent and a wait
/// (`function_call`s with a `namespace` and no `status`, `SubAgentActivity`
/// and `CollabAgentToolCall` items), then a second turn whose declined
/// approval ended in `turn_aborted`. `live2-fork.jsonl`: a `codex exec fork`
/// of `complete` (planted first), which replays none of its origin's records
/// but reports thread totals that include them; its `history_base` ends at
/// `complete`'s length.
const LIVE2_TOOLS: &str = "../codex-conformance/live2-tools.jsonl";
const LIVE2_SID: &str = "00000000-0000-4000-8000-0000000b2001";
const LIVE2_FORK: &str = "../codex-conformance/live2-fork.jsonl";
const LIVE2_FORK_SID: &str = "00000000-0000-4000-8000-0000000b2002";
/// Third live run (docs/telemetry/certificate-live.md §3): `codex exec resume
/// -m <another model>` of a TUI worker's ended thread appends to the same
/// rollout (no new file or `session_meta`, ordinals continue, thread totals
/// continue as the running sum, no replay), after `thread_settings_applied`,
/// a second `world_state` and a new `turn_context` naming the other model.
/// Hand-written with `LIVE3LEAK_*` sentinels; kept out of `CASES`.
const LIVE3_RESUME: &str = "../codex-conformance/live3-resume.jsonl";
const LIVE3_SID: &str = "00000000-0000-4000-8000-0000000b3001";
static LIVE3_CASE: Case = Case { name: "live3-resume", sid: LIVE3_SID, parts: &[LIVE3_RESUME], version: "0.154.0", place: Place::Bound };

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

/// The shared conformance corpus. `complete`, `edge`, `child`, `guardian`,
/// `tools` and the two live-2 cases are certified and bound; `uncertified` is
/// bound with a version no live run certified; the rest are certified but must
/// stay unbound.
const CASES: &[Case] = &[
    Case { name: "complete", sid: SID, parts: &["head.jsonl", "tail.jsonl"], version: "0.154.0", place: Place::Bound },
    Case { name: "edge", sid: EDGE_SID, parts: &["head.jsonl", EDGE], version: "0.154.0", place: Place::Bound },
    Case { name: "uncertified", sid: OLD_SID, parts: &["head.jsonl", "tail.jsonl"], version: "0.999.0", place: Place::Bound },
    Case { name: "cwd-outside", sid: "00000000-0000-4000-8000-0000000a3002", parts: &["head.jsonl"], version: "0.154.0", place: Place::CwdOutside },
    Case { name: "other-home", sid: "00000000-0000-4000-8000-0000000a3003", parts: &["head.jsonl"], version: "0.154.0", place: Place::OtherHome },
    Case { name: "earlier", sid: "00000000-0000-4000-8000-0000000a3004", parts: &["head.jsonl"], version: "0.154.0", place: Place::BeforeDecision },
    Case { name: "child", sid: CHILD_SID, parts: &[CHILD], version: "0.154.0", place: Place::Bound },
    Case { name: "guardian", sid: GUARDIAN_SID, parts: &[GUARDIAN], version: "0.154.0", place: Place::Bound },
    Case { name: "tools", sid: TOOLS_SID, parts: &[TOOLS], version: "0.154.0", place: Place::Bound },
    Case { name: "live2-tools", sid: LIVE2_SID, parts: &[LIVE2_TOOLS], version: "0.154.0", place: Place::Bound },
    Case { name: "live2-fork", sid: LIVE2_FORK_SID, parts: &[LIVE2_FORK], version: "0.154.0", place: Place::Bound },
];

fn case(name: &str) -> &'static Case { CASES.iter().chain([&LIVE3_CASE]).find(|c| c.name == name).unwrap() }

/// Write corpus case `name` into the fixture; returns the rollout path. A
/// fork's `@ORIGIN@` becomes the `complete` case's session id and
/// `@ORIGIN_END@` the planted `complete` rollout's length (its end when forked).
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
    let mut text = fs::read_to_string(&path).unwrap().replace(SID, c.sid).replace("@ORIGIN@", SID);
    if text.contains("@ORIGIN_END@") {
        let origin = path.with_file_name("rollout-2026-09-28T00-00-00-complete.jsonl");
        text = text.replace("@ORIGIN_END@", &fs::metadata(&origin).expect("plant `complete` before its fork").len().to_string());
    }
    fs::write(&path, text).unwrap();
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
        "codex_rate_limit_windows", "rollout_threads", "codex_tool_sources", "codex_tool_calls", "codex_exec_items", "rollout_subagents", "rollout_ingest_state",
        "rollout_forks", "rollout_turn_ends", "codex_turn_aborts", "codex_mcp_calls", "codex_agent_items", "codex_tool_namespaces", "codex_fork_reconciliation"] {
        let order = if table == "codex_tool_sources" { "1" } else { "1,2" };
        let mut stmt = db.prepare(&format!("SELECT * FROM {table} ORDER BY {order}")).unwrap();
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
    // complete 2 + edge 3 + uncertified 2 + three unbound heads + child 2 + guardian 1 + live2-fork 1 (tools and live2-tools have none).
    assert_eq!(first["collected"]["records"], 14);
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
/// ledger reader can gate on it. A7: each such envelope keeps the reported
/// counters as evidence but says `measurement.certified` false; the certified
/// sibling's say true.
#[test]
fn uncertified_version_is_gated_everywhere() {
    let f = Fixture::new();
    let complete = plant(&f, "complete");
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
    let certified = |path: &Path| rows::<i64>(&f, &format!("SELECT json_extract(measurement,'$.certified'),count(*) FROM source_observations
        WHERE producer_epoch='{}' GROUP BY 1", source(path)));
    assert_eq!((certified(&old), certified(&complete)), (vec![vec![0, 10]], vec![vec![1, 10]]));
    // Head line 5: the uncertified record's 1000 input tokens stay in its envelope, and nowhere in `codex_usage`.
    assert_eq!(rows::<i64>(&f, &format!("SELECT json_extract(payload,'$.usage.input_tokens') FROM source_observations WHERE producer_epoch='{}'
        AND event_kind='codex.token_usage_record.v1' ORDER BY producer_sequence", source(&old))), [vec![1000], vec![500]]);
    let (capabilities, _) = f.cli_args(&["collectors", "capabilities", "--json"]);
    assert_eq!((&capabilities["adapters"][0]["certified_versions"], &capabilities["adapters"][0]["uncertified_version"]),
        (&json!(["0.154.0"]), &json!("cli_version_uncertified")));
}

fn contains_sentinel(bytes: &[u8]) -> Option<&'static str> {
    let lower = bytes.to_ascii_lowercase();
    ["a3leak", "a4leak", "a5leak", "a6leak", "live2leak", "canary"].into_iter().find(|needle| lower.windows(needle.len()).any(|w| w == needle.as_bytes()))
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
        "A4LEAK_MCP_ARGUMENTS", "A4LEAK_MCP_RESULT", "A4LEAK_CREDITS", "A4LEAK_GUARDIAN_TRANSCRIPT", "A4LEAK_GUARDIAN_VERDICT",
        // A5: the uncollected `multi_agent_version` beside the collected lineage.
        "A5LEAK_MULTI_AGENT",
        // A6: every content field beside the collected tool/exec metadata, and wrongly typed metadata.
        "A6LEAK_INPUT", "A6LEAK_ARGUMENTS", "A6LEAK_OUTPUT_ARRAY", "A6LEAK_OUTPUT_STRING", "A6LEAK_ORPHAN_OUTPUT", "A6LEAK_COMMAND", "A6LEAK_CWD",
        "A6LEAK_PARSED_CMD", "A6LEAK_STDOUT", "A6LEAK_STDERR", "A6LEAK_AGGREGATED_OUTPUT", "A6LEAK_FORMATTED_OUTPUT", "A6LEAK_AGENT_CONTENT",
        "A6LEAK_USER_CONTENT", "A6LEAK_PHASE", "A6LEAK_CLIENT_ID", "A6LEAK_PROCESS_ID", "A6LEAK_CALL_ITEM_ID", "A6LEAK_MESSAGE_ID", "A6LEAK_NAME_KEY",
        "A6LEAK_STATUS", "A6LEAK_PASSTHROUGH", "A6LEAK_EXIT_CODE", "A6LEAK_DURATION", "A6LEAK_LAST_MESSAGE",
        // A8: MCP arguments and results beside the collected MCP metadata, subagent paths, collab agents and states, the aborted
        // turn's declined output, the fork's settings and `multi_agent_version`.
        "LIVE2LEAK_ARGUMENT_KEY", "LIVE2LEAK_ARGUMENT", "LIVE2LEAK_MCP_RESULT", "LIVE2LEAK_MCP_CODE_INPUT", "LIVE2LEAK_AGENT_PATH", "LIVE2LEAK_RECEIVER_NICKNAME",
        "LIVE2LEAK_RECEIVER_PATH", "LIVE2LEAK_AGENT_STATE", "LIVE2LEAK_SPAWN_ARGUMENTS", "LIVE2LEAK_WAIT_OUTPUT", "LIVE2LEAK_DECLINED_OUTPUT",
        "LIVE2LEAK_STDERR_2", "LIVE2LEAK_SETTINGS_PATH", "LIVE2LEAK_MULTI_AGENT", "LIVE2LEAK_CREDITS"] {
        assert!(corpus.windows(needle.len()).any(|w| w == needle.as_bytes()), "{needle}");
    }
    let mut output = f.cli("collect").1;
    // Hold a reader across the next collect so its frames stay in the WAL.
    let reader = f.sidecar();
    let _ = reader.query_row("SELECT count(*) FROM source_observations", [], |r| r.get::<_, i64>(0)).unwrap();
    remove_sidecar_rows(&f);
    output.extend(f.cli("collect").1);
    for args in [&["usage", "--json"][..], &["attempts", "--json"], &["report", "--json"], &["collectors", "status"], &["collectors", "bindings"], &["collectors", "sessions"],
        &["collectors", "tools", "--json"], &["collectors", "capabilities", "--json"], &["accounting", "sync"], &["accounting", "entries"], &["accounting", "sessions"], &["accounting", "quota", "--json"]] {
        output.extend(f.cli_args(args).1);
    }
    for args in [&["usage"][..], &["report"], &["collectors", "capabilities"], &["collectors", "tools"]] {
        output.extend(f.text(args).into_bytes());
    }
    let state = f.project.join(".state");
    assert!(!fs::read(state.join("telemetry.db-wal")).unwrap().is_empty(), "the second collect wrote through the WAL");
    for name in ["telemetry.db", "telemetry.db-wal", "telemetry.db-shm"] {
        assert_eq!(contains_sentinel(&fs::read(state.join(name)).unwrap()), None, "{name}");
    }
    assert_eq!(contains_sentinel(&output), None, "{}", String::from_utf8_lossy(&output));
    // complete, edge, uncertified, three unbound heads, child 8 (A6: its
    // function_call), guardian 4, tools 13 (every line), live2-tools 25 (every
    // line, A8: its `turn_aborted`) and live2-fork 6.
    assert_eq!(f.count("source_observations"), 10 + 11 + 10 + 3 * 6 + 8 + 4 + 13 + 25 + 6, "the collects read the whole corpus");
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
/// allowlist and the live docs (docs/telemetry/codex-live-0.154.0*.md), printed as text.
const CAPABILITIES: &str = "codex rollout_jsonl certified_versions=0.154.0
  line.timestamp available=true basis=reported certified=live caveat=envelope_occurred_unix_ms
  session_meta.id available=true basis=reported certified=live
  session_meta.timestamp available=true basis=reported_excerpt certified=live
  session_meta.cwd available=true basis=reported_home_redacted certified=live
  session_meta.cli_version available=true basis=reported_excerpt certified=live
  session_meta.originator available=true basis=reported_excerpt certified=live
  session_meta.source available=true basis=reported_excerpt certified=live
  session_meta.model_provider available=true basis=reported_excerpt certified=live
  session_meta.forked_from_id available=true basis=reported certified=live caveat=fork_thread_total_includes_origin
  session_meta.subagent_kind available=true basis=reported_excerpt certified=live caveat=from_source_subagent
  session_meta.subagent_detail available=true basis=reported_excerpt certified=live caveat=observed_guardian_only
  session_meta.subagent_parent_thread_id available=true basis=reported certified=live caveat=from_source_subagent
  session_meta.subagent_depth available=true basis=reported certified=live caveat=from_source_subagent
  session_meta.parent_thread_id available=true basis=reported certified=live caveat=observed_guardian_and_thread_spawn
  session_meta.session_id available=true basis=reported certified=live caveat=child_reports_parent_session
  session_meta.thread_source available=true basis=reported_excerpt certified=live caveat=observed_user_guardian_review_subagent
  session_meta.forked_from_ordinal_exclusive available=true basis=reported certified=live
  session_meta.history_base.thread_id available=true basis=reported certified=live
  session_meta.history_base.end_ordinal_exclusive available=true basis=reported certified=live
  session_meta.history_base.end_byte_offset available=true basis=reported certified=live caveat=origin_file_length_at_fork
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
  token_usage_record.session_id available=true basis=reported certified=live caveat=child_reports_parent_session
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
  turn_aborted.turn_id available=true basis=reported certified=live
  turn_aborted.reason available=true basis=reported_excerpt certified=live
  turn_aborted.duration_ms available=true basis=reported certified=live
  turn_aborted.started_at available=false basis=unavailable certified=none reason=not_collected
  turn_aborted.completed_at available=false basis=unavailable certified=none reason=not_collected
  response_item.message available=false basis=unavailable certified=none reason=content_forbidden
  response_item.reasoning available=false basis=unavailable certified=none reason=content_forbidden
  custom_tool_call.call_id available=true basis=reported certified=live
  custom_tool_call.name available=true basis=reported_excerpt certified=live
  custom_tool_call.status available=true basis=reported_excerpt certified=live
  custom_tool_call.internal_chat_message_metadata_passthrough.turn_id available=true basis=reported certified=live
  custom_tool_call.id available=false basis=unavailable certified=none reason=not_collected
  custom_tool_call.internal_chat_message_metadata_passthrough.create_time available=false basis=unavailable certified=none reason=not_collected
  custom_tool_call.input available=false basis=unavailable certified=none reason=content_forbidden
  function_call.call_id available=true basis=reported certified=live
  function_call.name available=true basis=reported_excerpt certified=live
  function_call.namespace available=true basis=reported_excerpt certified=live
  function_call.status available=true basis=reported_excerpt certified=fixture
  function_call.internal_chat_message_metadata_passthrough.turn_id available=true basis=reported certified=live
  function_call.id available=false basis=unavailable certified=none reason=not_collected
  function_call.arguments available=false basis=unavailable certified=none reason=content_forbidden
  custom_tool_call_output.call_id available=true basis=reported certified=live
  custom_tool_call_output.id available=false basis=unavailable certified=none reason=not_collected
  custom_tool_call_output.output available=false basis=unavailable certified=none reason=content_forbidden
  function_call_output.call_id available=true basis=reported certified=live
  function_call_output.id available=false basis=unavailable certified=none reason=not_collected
  function_call_output.output available=false basis=unavailable certified=none reason=content_forbidden
  item_completed.thread_id available=true basis=reported certified=live
  item_completed.turn_id available=true basis=reported certified=live
  item_completed.item.type available=true basis=reported_excerpt certified=live
  item_completed.item.id available=true basis=reported certified=live caveat=typed_items_only
  item_completed.item.status available=true basis=reported_excerpt certified=live caveat=observed_completed_failed
  item_completed.item.source available=true basis=reported_excerpt certified=live caveat=command_execution_only
  item_completed.item.exit_code available=true basis=reported certified=live caveat=command_execution_only
  item_completed.item.duration.secs available=true basis=reported certified=live caveat=startup_not_run_time
  item_completed.item.duration.nanos available=true basis=reported certified=live caveat=startup_not_run_time
  item_completed.started_at_ms available=false basis=unavailable certified=none reason=not_collected
  item_completed.completed_at_ms available=false basis=unavailable certified=none reason=not_collected
  item_completed.item.process_id available=false basis=unavailable certified=none reason=not_collected
  item_completed.item.cwd available=false basis=unavailable certified=none reason=not_collected
  item_completed.item.client_id available=false basis=unavailable certified=none reason=not_collected
  item_completed.item.phase available=false basis=unavailable certified=none reason=not_collected
  item_completed.item.command available=false basis=unavailable certified=none reason=content_forbidden
  item_completed.item.parsed_cmd available=false basis=unavailable certified=none reason=content_forbidden
  item_completed.item.stdout available=false basis=unavailable certified=none reason=content_forbidden
  item_completed.item.stderr available=false basis=unavailable certified=none reason=content_forbidden
  item_completed.item.aggregated_output available=false basis=unavailable certified=none reason=content_forbidden
  item_completed.item.formatted_output available=false basis=unavailable certified=none reason=content_forbidden
  item_completed.item.content available=false basis=unavailable certified=none reason=content_forbidden
  item_completed.item.server available=true basis=reported_excerpt certified=live caveat=mcp_tool_call_only
  item_completed.item.tool available=true basis=reported_excerpt certified=live caveat=mcp_tool_call_only
  item_completed.item.readOnlyHint available=true basis=reported certified=live caveat=mcp_tool_call_only
  item_completed.item.result.isError available=true basis=reported certified=live caveat=mcp_tool_call_only
  item_completed.item.arguments available=false basis=unavailable certified=none reason=content_forbidden
  item_completed.item.result.content available=false basis=unavailable certified=none reason=content_forbidden
  item_completed.item.agent_thread_id available=true basis=reported certified=live caveat=subagent_activity_only
  item_completed.item.sender_thread_id available=true basis=reported certified=live caveat=collab_agent_tool_call_only
  item_completed.item.receiver_thread_ids available=true basis=reported certified=live caveat=collab_agent_tool_call_only
  item_completed.item.kind available=false basis=unavailable certified=none reason=not_collected
  item_completed.item.agents_states available=false basis=unavailable certified=none reason=not_collected
  item_completed.item.agent_path available=false basis=unavailable certified=none reason=content_forbidden
  item_completed.item.receiver_agents available=false basis=unavailable certified=none reason=content_forbidden
";

/// `collectors capabilities` cannot drift from the adapter: over the whole
/// corpus the envelopes carry exactly the fields it lists as available, each
/// with a value in at least one envelope (the fixture evidence), and never a
/// field it lists as unavailable. It reads no rollout (only the retained
/// profiles, listed after the fields), so it works on a project that has
/// never collected and creates no sidecar.
#[test]
fn capabilities_match_emitted_fields() {
    let f = Fixture::new();
    let text = f.text(&["collectors", "capabilities"]);
    assert_eq!(format!("{}\n", text.split("\nprofile ").next().unwrap()), CAPABILITIES);
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
    assert_eq!(rows.len(), 10 + 11 + 10 + 3 * 6 + 8 + 4 + 13 + 25 + 6);
}

fn rows<T: rusqlite::types::FromSql>(f: &Fixture, sql: &str) -> Vec<Vec<T>> {
    let db = f.sidecar();
    let mut stmt = db.prepare(sql).unwrap();
    let n = stmt.column_count();
    stmt.query_map([], |r| (0..n).map(|i| r.get(i)).collect()).unwrap().map(Result::unwrap).collect()
}

/// A4 session metadata and A5 thread lineage as collected, with the span of
/// the usage record times, and the A7 final event of the rollout's last turn
/// (each rollout here completes its last turn); A8 `fork` `null` (no fork
/// point); F3 `after_termination` `null` (the attempt was never terminated).
#[allow(clippy::too_many_arguments)]
fn session(f: &Fixture, sid: &str, path: &Path, records: i64, forked: Value, subagent: Value, thread: Value, times: [i64; 2], last_turn: &str) -> Value {
    json!({"session_id": sid, "path_digest": source(path), "binding": "bound", "attempt_id": f.attempt, "records": records, "model_provider": "openai",
        "forked_from_id": forked, "subagent": subagent, "thread": thread,
        "record_times": {"stored": records, "timed": records, "first_unix_ms": times[0], "last_unix_ms": times[1]},
        "final_event": {"state": "complete", "turn_id": last_turn}, "fork": null, "after_termination": null})
}

/// `head.jsonl` reports `thread_source` `user` and `session_id` = its `id`
/// (shown `null`); `child.jsonl` reports neither.
fn user_thread() -> Value { json!({"parent_thread_id": null, "session_id": null, "source": "user"}) }

fn no_subagent() -> Value { json!({"kind": null, "detail": null, "parent_thread_id": null, "depth": null}) }

/// A4 on the CLI: the session's model provider, its fork and subagent parent
/// ids (a `thread_spawn` child names the edge session in `source`; the
/// guardian names none there), each usage record's line time, a model switch
/// inside a session, and the secondary rate-limit window with the reached
/// type. A5: the thread lineage (the guardian names the edge session as its
/// parent thread and reported session). Child and guardian rollouts bound to
/// the attempt count in its usage once each.
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
    // A8: the child's `forked_from_ordinal_exclusive`, without a `history_base`.
    let mut child_row = session(&f, CHILD_SID, &child, 2, json!(EDGE_SID), json!({"kind": "thread_spawn", "detail": null, "parent_thread_id": EDGE_SID, "depth": 1}),
        json!({"parent_thread_id": null, "session_id": null, "source": null}), [1_893_456_001_500, 1_893_456_003_000], "turn-c2");
    child_row["fork"] = json!({"forked_from_ordinal_exclusive": 3, "history_base": null, "reconciliation": null});
    assert_eq!(sessions, json!({"sessions": [
        session(&f, SID, &complete, 2, Value::Null, no_subagent(), user_thread(), [at, at], "turn-2"),
        session(&f, EDGE_SID, &edge, 3, Value::Null, no_subagent(), user_thread(), [at, at], "turn-3"),
        child_row,
        // A7: the live guardian's `source.subagent.other` tag.
        session(&f, GUARDIAN_SID, &guardian, 1, Value::Null, json!({"kind": "other", "detail": "guardian", "parent_thread_id": null, "depth": null}),
            json!({"parent_thread_id": EDGE_SID, "session_id": EDGE_SID, "source": "guardian_review"}), [1_893_456_062_000, 1_893_456_062_000], "turn-g1"),
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

/// The sidecar as the A3 binary left it: no A4 (or A5, A6) tables, ingest stream
/// 3, and envelopes of the narrower `session_meta`/`token_count` allowlist.
fn downgrade_to_a3(f: &Fixture) {
    downgrade_to_a5(f);
    f.sidecar().execute_batch("DROP TABLE rollout_metadata; DROP TABLE codex_usage_times; DROP TABLE codex_rate_limit_windows; DROP TABLE rollout_threads;
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
        assert_eq!([&s["model_provider"], &s["forked_from_id"], &s["subagent"], &s["thread"], &s["record_times"]], [&predates; 5], "{s}");
    }
    let (upgraded, _) = f.cli("collect");
    assert_eq!(upgraded["collected"]["records"], 0, "the re-read counts nothing twice");
    assert_eq!(f.cli_args(&["collectors", "status"]).0, json!({"stream": "ingest", "version": 12}));
    assert!(fresh == ledger(&f), "the upgraded sidecar equals a fresh collect");
    assert_eq!(f.cli_args(&["usage", "--json"]).0, fresh_usage);
    assert_eq!(f.cli_args(&["collectors", "sessions"]).0, fresh_sessions);

    downgrade_to_a3(&f);
    fs::remove_file(&child).unwrap();
    f.cli("collect");
    let (sessions, _) = f.cli_args(&["collectors", "sessions"]);
    let pending = unavailable("pending_reread");
    let child_row = sessions["sessions"].as_array().unwrap().iter().find(|s| s["session_id"] == CHILD_SID).unwrap();
    assert_eq!([&child_row["model_provider"], &child_row["forked_from_id"], &child_row["subagent"], &child_row["thread"], &child_row["record_times"]], [&pending; 5]);
    assert_eq!(sessions["sessions"][0]["model_provider"], "openai");
    assert_eq!(f.cli_args(&["usage", "--json"]).0, fresh_usage, "the gone rollout's records still count");
}

/// A5, the live guardian shape (codex-live-0.154.0-a4.md §5): the guardian's
/// usage record reports the edge (parent) session's `session_id`, yet usage
/// is keyed by the rollout's own `session_meta.id` and ordinal. So the
/// guardian's record takes none of the parent's ordinals (no quarantine),
/// the parent's thread total still reconciles, each response is stored once
/// under the rollout that wrote it, and the attempt counts the guardian's 30
/// tokens once beside the parent's, whichever rollout is read first. The
/// envelope keeps the reported parent id under the guardian's own identity.
#[test]
fn guardian_usage_reporting_its_parent_session_stays_with_its_rollout() {
    let f = Fixture::new();
    let [edge, guardian] = ["edge", "guardian"].map(|name| plant(&f, name));
    let (report, _) = f.cli("collect");
    // edge 1300/450/0/160/90/1460 (3 records) + guardian 25/0/0/5/1/30 (1 record).
    let sums = json!({"input_tokens": 1325, "cached_input_tokens": 450, "cache_write_input_tokens": 0, "output_tokens": 165,
        "reasoning_output_tokens": 91, "total_tokens": 1490, "records": 4});
    assert_eq!(attempt_usage(&report), sums);
    use rusqlite::types::Value::{Integer as I, Text as T};
    let usage = |guardian: &Path| {
        let row = |sid: &str, ordinal: i64, path: &Path, response: &str| vec![T(sid.into()), I(ordinal), T(source(path)), T(response.into()), I(1)];
        [row(EDGE_SID, 1, &edge, "resp-1"), row(EDGE_SID, 2, &edge, "resp-3"), row(EDGE_SID, 3, &edge, "resp-4"), row(GUARDIAN_SID, 1, guardian, "resp-g1")]
    };
    let stored = |f: &Fixture| rows::<rusqlite::types::Value>(f, "SELECT session_id,ordinal,path_digest,response_id,accepted FROM codex_usage ORDER BY session_id,ordinal");
    assert_eq!(stored(&f), usage(&guardian));
    assert_eq!((f.count("codex_quarantine"), f.count("codex_discrepancy")), (0, 0));

    // Envelopes: under the guardian's own identity, its reported lineage and the parent's id in the record.
    let guardian_envelope = |kind: &str, path: &str| f.sidecar().query_row(&format!("SELECT identity,json_extract(payload,'{path}') FROM source_observations
        WHERE producer_epoch=?1 AND event_kind=?2"), rusqlite::params![source(&guardian), kind], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).unwrap();
    let identity = format!(r#"{{"session_id":"{GUARDIAN_SID}"}}"#);
    assert_eq!(guardian_envelope("codex.token_usage_record.v1", "$.session_id"), (identity.clone(), EDGE_SID.to_owned()));
    for (path, value) in [("$.parent_thread_id", EDGE_SID), ("$.session_id", EDGE_SID), ("$.thread_source", "guardian_review"), ("$.source", "subagent"),
        ("$.subagent_detail", "guardian")] {
        assert_eq!(guardian_envelope("codex.session_meta.v1", path), (identity.clone(), value.to_owned()));
    }

    // The accounting graph keeps the two apart: the guardian's 30 is not in the edge session's 1460.
    f.cli_args(&["accounting", "sync"]);
    let (graph, _) = f.cli_args(&["accounting", "sessions"]);
    let totals: Vec<(String, String, Value)> = graph["sessions"].as_array().unwrap().iter()
        .map(|s| (s["session_id"].as_str().unwrap().to_owned(), s["role"].as_str().unwrap().to_owned(), s["total_tokens"].clone())).collect();
    assert_eq!(totals, [(EDGE_SID.to_owned(), "primary".to_owned(), json!(1460)), (GUARDIAN_SID.to_owned(), "guardian".to_owned(), json!(30))]);

    // Read the guardian's rollout before its parent's: the same rows and sums.
    remove_sidecar(&f);
    let first = guardian.with_file_name("rollout-2026-09-28T00-00-00-a-guardian.jsonl");
    fs::rename(&guardian, &first).unwrap();
    assert_eq!(attempt_usage(&f.cli("collect").0), sums);
    assert_eq!(stored(&f), usage(&first));
    assert_eq!((f.count("codex_quarantine"), f.count("codex_discrepancy")), (0, 0));
}

/// The sidecar as the A4 binary left it: no `rollout_threads` (or A6 tables), ingest stream
/// 4, and `session_meta` envelopes of the A4 allowlist (normalization 2).
fn downgrade_to_a4(f: &Fixture) {
    downgrade_to_a5(f);
    f.sidecar().execute_batch("DROP TABLE rollout_threads; UPDATE telemetry_streams SET version=4 WHERE stream='ingest';
        UPDATE source_observations SET payload=json_remove(payload,'$.parent_thread_id','$.session_id','$.thread_source'),payload_digest='sha256:a4',
            measurement='{\"coverage\":\"complete\",\"measurement_basis\":\"reported\",\"normalization_version\":2}'
            WHERE event_kind='codex.session_meta.v1';").unwrap();
}

/// Upgrade: a sidecar written before A5 is read (read-only) with its thread
/// lineage `unavailable: predates_collection` and its A4 metadata intact. The
/// next collect migrates it to ingest 5 and reads every rollout again: the
/// lineage is filled, the A4 `session_meta` envelopes are superseded without a
/// digest conflict, nothing is counted twice, and the ledger equals a fresh collect.
#[test]
fn rollouts_read_before_a5_gain_their_thread_lineage_on_the_next_collect() {
    let f = Fixture::new();
    for name in ["complete", "edge", "guardian"] { plant(&f, name); }
    f.cli("collect");
    let (fresh, fresh_usage, fresh_sessions) = (ledger(&f), f.cli_args(&["usage", "--json"]).0, f.cli_args(&["collectors", "sessions"]).0);

    downgrade_to_a4(&f);
    let (before, _) = f.cli_args(&["collectors", "sessions"]);
    let sessions = before["sessions"].as_array().unwrap();
    assert!(sessions.iter().all(|s| s["thread"] == unavailable("predates_collection") && s["model_provider"] == "openai"), "{before}");
    assert_eq!(sessions[2]["subagent"]["kind"], "other");
    let (upgraded, _) = f.cli("collect");
    assert_eq!(upgraded["collected"]["records"], 0, "the re-read counts nothing twice");
    assert_eq!(f.cli_args(&["collectors", "status"]).0, json!({"stream": "ingest", "version": 12}));
    assert!(fresh == ledger(&f), "the upgraded sidecar equals a fresh collect");
    assert_eq!(f.cli_args(&["usage", "--json"]).0, fresh_usage);
    assert_eq!(f.cli_args(&["collectors", "sessions"]).0, fresh_sessions);
}

/// An `item_completed` envelope payload: every allowlisted path (A6 and A8),
/// `null` unless `set` (the item's sorted, comma-separated JSON members, with
/// `duration` and `result` whole) gives it.
fn item_envelope(set: &str, thread: &str, turn: &str) -> String {
    let set: serde_json::Map<String, Value> = serde_json::from_str(&format!("{{{set}}}")).unwrap();
    let mut item = json!({"agent_thread_id": null, "duration": {"nanos": null, "secs": null}, "exit_code": null, "id": null, "readOnlyHint": null,
        "receiver_thread_ids": null, "result": {"isError": null}, "sender_thread_id": null, "server": null, "source": null, "status": null, "tool": null, "type": null});
    for (key, value) in set { item[key] = value; }
    json!({"item": item, "thread_id": thread, "turn_id": turn}).to_string()
}

/// A6 tool call metadata as `collectors tools` prints it: `(call_id,
/// call_kind, name, status, turn_id, called, output_kind, output)`, without an
/// A8 namespace.
#[allow(clippy::too_many_arguments)]
fn call(id: &str, kind: Option<&str>, name: Option<&str>, status: Option<&str>, turn: Option<&str>, called: Option<i64>, output_kind: Option<&str>, output: Option<i64>) -> Value {
    json!({"call_id": id, "call_kind": kind, "name": name, "namespace": null, "status": status, "turn_id": turn, "called_unix_ms": called, "output_kind": output_kind,
        "output_unix_ms": output, "call_to_output_ms": called.zip(output).map(|(c, o)| o - c)})
}

/// A6 on the CLI, in the live 0.154.0 shape (codex-live-0.154.0-a4.md §4):
/// each tool call's id, tool name, status and turn with its call and output
/// line times (the call → output gap measures the call, approval wait
/// included), and each `CommandExecution` item's id, status, source, exit code
/// and startup duration, keyed by the rollout's own session. Wrongly typed
/// metadata is `null` and never quarantines its record; an output without a
/// call keeps its own row; agent and user message items keep only their type.
/// The envelopes carry exactly the allowlist. The child's older-shaped
/// `function_call` counts too; its guessed `exec_command_end` and
/// `mcp_tool_call_end` events stay unread.
#[test]
fn tool_and_exec_metadata_is_collected_without_content() {
    let f = Fixture::new();
    let [tools, child] = ["tools", "child"].map(|name| plant(&f, name));
    f.cli("collect");
    assert_eq!(quarantine(&f, &source(&tools)), [], "wrongly typed metadata is null, never a malformed record");

    // 2030-01-01T00:00:00Z = 1_893_456_000_000 ms; call-t1 waited 47.410 - 10.500 s.
    let t = |ms: i64| Some(1_893_456_000_000 + ms);
    let (listed, _) = f.cli_args(&["collectors", "tools", "--json"]);
    assert_eq!(listed, json!({"sessions": [
        {"session_id": CHILD_SID, "attempt_ids": [f.attempt], "exec_items": [], "mcp_calls": [], "agent_items": [], "turn_aborts": [],
            "tool_calls": [call("call-c1", Some("function_call"), Some("shell"), None, None, t(500), None, None)]},
        {"session_id": TOOLS_SID, "attempt_ids": [f.attempt], "tool_calls": [
            call("call-t1", Some("custom_tool_call"), Some("exec"), Some("completed"), Some("turn-t1"), t(10_500), Some("custom_tool_call_output"), t(47_410)),
            call("call-t2", Some("function_call"), Some("wait"), Some("completed"), Some("turn-t1"), t(48_000), Some("function_call_output"), t(49_250)),
            call("call-t3", Some("custom_tool_call"), None, None, None, t(50_000), None, None),
            call("call-t9", None, None, None, None, None, Some("function_call_output"), t(51_000))],
         "exec_items": [
            {"item_id": "exec-t1", "thread_id": TOOLS_SID, "turn_id": "turn-t1", "status": "completed", "source": "unified_exec_startup", "exit_code": 0,
                "startup_duration": {"secs": 0, "nanos": 2125}, "completed_unix_ms": t(47_400)},
            {"item_id": "exec-t2", "thread_id": TOOLS_SID, "turn_id": "turn-t1", "status": null, "source": null, "exit_code": null,
                "startup_duration": {"secs": null, "nanos": null}, "completed_unix_ms": t(50_500)}],
         "mcp_calls": [], "agent_items": [], "turn_aborts": []},
    ]}));
    assert_eq!(f.cli_args(&["collectors", "tools", "--json"]).0, listed, "a second read is identical");
    let text = f.text(&["collectors", "tools"]);
    assert!(text.contains(&format!("{TOOLS_SID} attempts={}\n", f.attempt)), "{text}");
    assert!(text.contains("  call call-t1 custom_tool_call name=exec status=completed turn=turn-t1 called=1893456010500 output=1893456047410 call_to_output_ms=36910\n"), "{text}");
    assert!(text.contains("  exec exec-t1 status=completed source=unified_exec_startup exit_code=0 turn=turn-t1 completed=1893456047400\n"), "{text}");

    // Envelopes: one per tool line (and none for the child's guessed `*_end`
    // events), each exactly the allowlist.
    let at = line_starts(&tools);
    let key = source(&tools);
    let envelope = |line: usize| envelopes(&f, &key).into_iter().find(|e| e.0 == at[line - 1]).map(|e| (e.1, e.2)).unwrap();
    let kinds: Vec<String> = envelopes(&f, &key).into_iter().map(|e| e.1).collect();
    assert_eq!(kinds, ["session_meta", "turn_context", "custom_tool_call", "item_completed", "custom_tool_call_output", "function_call", "function_call_output",
        "item_completed", "item_completed", "custom_tool_call", "item_completed", "function_call_output", "task_complete"].map(|k| format!("codex.{k}.v1")));
    assert_eq!(envelope(3).1, r#"{"call_id":"call-t1","internal_chat_message_metadata_passthrough":{"turn_id":"turn-t1"},"name":"exec","status":"completed"}"#);
    assert_eq!(envelope(4).1, item_envelope(r#""duration":{"nanos":2125,"secs":0},"exit_code":0,"id":"exec-t1","source":"unified_exec_startup","status":"completed","type":"CommandExecution""#,
        TOOLS_SID, "turn-t1"));
    assert_eq!(envelope(5).1, r#"{"call_id":"call-t1"}"#);
    let message = |kind: &str| item_envelope(&format!(r#""type":"{kind}""#), TOOLS_SID, "turn-t1");
    assert_eq!((envelope(8).1, envelope(9).1), (message("AgentMessage"), message("UserMessage")));
    assert_eq!(envelope(10).1, r#"{"call_id":"call-t3","internal_chat_message_metadata_passthrough":{"turn_id":null},"name":null,"status":null}"#);
    assert_eq!(envelope(6).1, r#"{"call_id":"call-t2","internal_chat_message_metadata_passthrough":{"turn_id":"turn-t1"},"name":"wait","namespace":null,"status":"completed"}"#);
    assert_eq!(envelope(11).1, item_envelope(r#""id":"exec-t2","type":"CommandExecution""#, TOOLS_SID, "turn-t1"));
    let child_kinds: Vec<String> = envelopes(&f, &source(&child)).into_iter().map(|e| e.1).collect();
    assert!(child_kinds.contains(&"codex.function_call.v1".to_owned()) && !child_kinds.iter().any(|k| k.contains("_end")), "{child_kinds:?}");
}

/// The sidecar as the A5 binary left it: no A6 tables, ingest stream 5, and
/// no envelopes of the A6 kinds.
fn downgrade_to_a5(f: &Fixture) {
    downgrade_to_a6(f);
    f.sidecar().execute_batch("DROP TABLE codex_tool_sources; DROP TABLE codex_tool_calls; DROP TABLE codex_exec_items;
        UPDATE telemetry_streams SET version=5 WHERE stream='ingest';
        DELETE FROM source_observations WHERE event_kind IN ('codex.custom_tool_call.v1','codex.function_call.v1','codex.custom_tool_call_output.v1',
            'codex.function_call_output.v1','codex.item_completed.v1');").unwrap();
}

/// Upgrade: a sidecar written before A6 is read (read-only) with its tool
/// metadata `unavailable: predates_collection`, never `[]`. The next collect
/// migrates it to ingest 6 and reads every rollout again: the tool rows and
/// the A6 envelopes appear, nothing is counted twice, and the ledger equals a
/// fresh collect. A session whose rollout is gone before it could be read
/// again stays `pending_reread`.
#[test]
fn rollouts_read_before_a6_gain_their_tool_metadata_on_the_next_collect() {
    let f = Fixture::new();
    let tools = ["complete", "child", "tools"].map(|name| plant(&f, name))[2].clone();
    f.cli("collect");
    let (fresh, fresh_usage, fresh_tools) = (ledger(&f), f.cli_args(&["usage", "--json"]).0, f.cli_args(&["collectors", "tools", "--json"]).0);

    downgrade_to_a5(&f);
    let (before, _) = f.cli_args(&["collectors", "tools", "--json"]);
    let predates = unavailable("predates_collection");
    let listed = before["sessions"].as_array().unwrap();
    assert_eq!(listed.len(), 3);
    assert!(listed.iter().all(|s| s["tool_calls"] == predates && s["exec_items"] == predates), "{before}");
    assert_eq!(f.cli_args(&["collectors", "status"]).0, json!({"stream": "ingest", "version": 5}), "a read does not migrate");
    let (upgraded, _) = f.cli("collect");
    assert_eq!(upgraded["collected"]["records"], 0, "the re-read counts nothing twice");
    assert_eq!(f.cli_args(&["collectors", "status"]).0, json!({"stream": "ingest", "version": 12}));
    assert!(fresh == ledger(&f), "the upgraded sidecar equals a fresh collect");
    assert_eq!(f.cli_args(&["usage", "--json"]).0, fresh_usage);
    assert_eq!(f.cli_args(&["collectors", "tools", "--json"]).0, fresh_tools);

    downgrade_to_a5(&f);
    fs::remove_file(&tools).unwrap();
    f.cli("collect");
    let (after, _) = f.cli_args(&["collectors", "tools", "--json"]);
    let pending = unavailable("pending_reread");
    let row = |sid: &str| after["sessions"].as_array().unwrap().iter().find(|s| s["session_id"] == sid).unwrap().clone();
    assert_eq!((&row(TOOLS_SID)["tool_calls"], &row(TOOLS_SID)["exec_items"]), (&pending, &pending));
    assert_eq!(row(CHILD_SID)["tool_calls"], fresh_tools["sessions"].as_array().unwrap().iter().find(|s| s["session_id"] == CHILD_SID).unwrap()["tool_calls"]);
    assert_eq!(row(SID)["tool_calls"], json!([]), "an observed session without tool calls is empty, not unavailable");
}

/// The sidecar as the A7 binary left it: no A8 tables, ingest stream 7, no
/// `turn_aborted` envelopes, and `session_meta`, `function_call` and
/// `item_completed` envelopes of the A7 allowlists (normalization 4, 1 and 1).
fn downgrade_to_a7(f: &Fixture) {
    f.sidecar().execute_batch("DROP TABLE rollout_forks; DROP TABLE rollout_turn_ends; DROP TABLE codex_turn_aborts; DROP TABLE codex_mcp_calls;
        DROP TABLE codex_agent_items; DROP TABLE codex_tool_namespaces; DROP TABLE codex_fork_reconciliation;
        UPDATE telemetry_streams SET version=7 WHERE stream='ingest';
        DELETE FROM source_observations WHERE event_kind='codex.turn_aborted.v1';
        UPDATE source_observations SET payload=json_remove(payload,'$.forked_from_ordinal_exclusive','$.history_base'),payload_digest='sha256:a7',
            measurement=json_set(measurement,'$.normalization_version',4) WHERE event_kind='codex.session_meta.v1';
        UPDATE source_observations SET payload=json_remove(payload,'$.namespace'),payload_digest='sha256:a7',
            measurement=json_set(measurement,'$.normalization_version',1) WHERE event_kind='codex.function_call.v1';
        UPDATE source_observations SET payload=json_remove(payload,'$.item.server','$.item.tool','$.item.readOnlyHint','$.item.result','$.item.agent_thread_id',
            '$.item.sender_thread_id','$.item.receiver_thread_ids'),payload_digest='sha256:a7',
            measurement=json_set(measurement,'$.normalization_version',1) WHERE event_kind='codex.item_completed.v1';").unwrap();
}

/// Upgrade: a sidecar written before A8 is read (read-only) with the fork
/// point, the final event and the A8 tool lists `unavailable:
/// predates_collection`, never `null`, a state or `[]`; the A7 metadata stays.
/// The next collect migrates it to the current ingest version and reads every rollout again: the
/// aborted turn and the MCP, agent and namespace rows appear, the fork
/// reconciles against its origin, the narrower envelopes are superseded
/// without a digest conflict, nothing is counted twice, and the ledger equals
/// a fresh collect. A rollout gone before it could be read again stays
/// `pending_reread`.
#[test]
fn rollouts_read_before_a8_gain_their_live_run2_metadata_on_the_next_collect() {
    let f = Fixture::new();
    let tools = ["complete", "live2-tools", "live2-fork"].map(|name| plant(&f, name))[1].clone();
    f.cli("collect");
    let (fresh, fresh_usage) = (ledger(&f), f.cli_args(&["usage", "--json"]).0);
    let (fresh_sessions, fresh_tools) = (f.cli_args(&["collectors", "sessions"]).0, f.cli_args(&["collectors", "tools", "--json"]).0);

    downgrade_to_a7(&f);
    let predates = unavailable("predates_collection");
    let (before, _) = f.cli_args(&["collectors", "sessions"]);
    let sessions = before["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 3);
    assert!(sessions.iter().all(|s| s["fork"] == predates && s["final_event"] == predates && s["subagent"]["detail"].is_null()), "{before}");
    let (listed, _) = f.cli_args(&["collectors", "tools", "--json"]);
    for s in listed["sessions"].as_array().unwrap() {
        assert_eq!([&s["mcp_calls"], &s["agent_items"], &s["turn_aborts"]], [&predates; 3], "{s}");
        assert!(s["tool_calls"].as_array().unwrap().iter().all(|c| c["namespace"] == predates), "{s}");
    }
    assert_eq!(f.cli_args(&["collectors", "status"]).0, json!({"stream": "ingest", "version": 7}), "a read does not migrate");
    let (upgraded, _) = f.cli("collect");
    assert_eq!(upgraded["collected"]["records"], 0, "the re-read counts nothing twice");
    assert_eq!(f.cli_args(&["collectors", "status"]).0, json!({"stream": "ingest", "version": 12}));
    assert!(fresh == ledger(&f), "the upgraded sidecar equals a fresh collect");
    assert_eq!(f.count("ingest_quarantine"), 0, "no digest conflict");
    assert_eq!(f.cli_args(&["usage", "--json"]).0, fresh_usage);
    assert_eq!(f.cli_args(&["collectors", "sessions"]).0, fresh_sessions);
    assert_eq!(f.cli_args(&["collectors", "tools", "--json"]).0, fresh_tools);

    downgrade_to_a7(&f);
    fs::remove_file(&tools).unwrap();
    f.cli("collect");
    let pending = unavailable("pending_reread");
    let (after, _) = f.cli_args(&["collectors", "sessions"]);
    let row = |sid: &str| after["sessions"].as_array().unwrap().iter().find(|s| s["session_id"] == sid).unwrap().clone();
    assert_eq!((&row(LIVE2_SID)["fork"], &row(LIVE2_SID)["final_event"]), (&pending, &pending));
    assert_eq!(row(LIVE2_FORK_SID)["fork"]["reconciliation"], json!({"thread_total": "reconciled", "token_count_total": "reconciled"}));
    let (listed, _) = f.cli_args(&["collectors", "tools", "--json"]);
    let live2 = listed["sessions"].as_array().unwrap().iter().find(|s| s["session_id"] == LIVE2_SID).unwrap().clone();
    assert_eq!([&live2["mcp_calls"], &live2["agent_items"], &live2["turn_aborts"]], [&pending; 3]);
    assert_eq!(f.cli_args(&["usage", "--json"]).0, fresh_usage, "the gone rollout's records still count");
}

/// The sidecar as the A6 binary left it: no A7 tables, ingest stream 6,
/// envelopes without `measurement.certified`, and `session_meta` envelopes of
/// the A5 allowlist (normalization 3, no `subagent_detail`).
fn downgrade_to_a6(f: &Fixture) {
    downgrade_to_a7(f);
    f.sidecar().execute_batch("DROP TABLE rollout_subagents; DROP TABLE rollout_ingest_state; UPDATE telemetry_streams SET version=6 WHERE stream='ingest';
        UPDATE source_observations SET measurement=json_remove(measurement,'$.certified');
        UPDATE source_observations SET payload=json_remove(payload,'$.subagent_detail'),payload_digest='sha256:a6',
            measurement=json_set(measurement,'$.normalization_version',3) WHERE event_kind='codex.session_meta.v1';").unwrap();
}

/// Upgrade: a sidecar written before A7 is read (read-only) with the subagent
/// detail and the final event `unavailable: predates_collection`, never `null`
/// or a state. The next collect migrates it to ingest 7 and reads every
/// rollout again: the detail and turn state appear, every envelope gains
/// `measurement.certified` and the A5 `session_meta` envelopes are superseded,
/// all without a digest conflict; nothing is counted twice, and the ledger
/// equals a fresh collect. A rollout gone before it could be read again stays
/// `pending_reread`.
#[test]
fn rollouts_read_before_a7_gain_their_subagent_detail_and_turn_state_on_the_next_collect() {
    let f = Fixture::new();
    let guardian = ["complete", "guardian", "tools"].map(|name| plant(&f, name))[1].clone();
    f.cli("collect");
    let (fresh, fresh_usage, fresh_sessions) = (ledger(&f), f.cli_args(&["usage", "--json"]).0, f.cli_args(&["collectors", "sessions"]).0);

    downgrade_to_a6(&f);
    let (before, _) = f.cli_args(&["collectors", "sessions"]);
    let predates = unavailable("predates_collection");
    let listed = before["sessions"].as_array().unwrap();
    assert_eq!(listed.len(), 3);
    assert!(listed.iter().all(|s| s["subagent"]["detail"] == predates && s["final_event"] == predates && s["model_provider"] == "openai"), "{before}");
    let guardian_row = |sessions: &Value| sessions["sessions"].as_array().unwrap().iter().find(|s| s["session_id"] == GUARDIAN_SID).unwrap().clone();
    assert_eq!(guardian_row(&before)["subagent"]["kind"], "other");
    assert_eq!(f.cli_args(&["collectors", "status"]).0, json!({"stream": "ingest", "version": 6}), "a read does not migrate");
    let (upgraded, _) = f.cli("collect");
    assert_eq!(upgraded["collected"]["records"], 0, "the re-read counts nothing twice");
    assert_eq!(f.cli_args(&["collectors", "status"]).0, json!({"stream": "ingest", "version": 12}));
    assert!(fresh == ledger(&f), "the upgraded sidecar equals a fresh collect");
    assert_eq!(f.count("ingest_quarantine"), 0, "no digest conflict");
    assert_eq!(f.cli_args(&["usage", "--json"]).0, fresh_usage);
    assert_eq!(f.cli_args(&["collectors", "sessions"]).0, fresh_sessions);

    downgrade_to_a6(&f);
    fs::remove_file(&guardian).unwrap();
    f.cli("collect");
    let (after, _) = f.cli_args(&["collectors", "sessions"]);
    let pending = unavailable("pending_reread");
    assert_eq!((&guardian_row(&after)["subagent"]["detail"], &guardian_row(&after)["final_event"]), (&pending, &pending));
    assert_eq!(after["sessions"][0]["final_event"], json!({"state": "complete", "turn_id": "turn-2"}));
    assert_eq!(f.cli_args(&["usage", "--json"]).0, fresh_usage, "the gone rollout's records still count");
}

/// A7: envelopes written while their version was uncertified (as a binary
/// that did not yet certify 0.154.0 wrote them: `measurement.certified`
/// false, NULL usage counters) are re-read once the version is certified.
/// Each envelope's measurement is superseded in place with the same payload
/// and digest, never a `digest_conflict`; a rollout without usage records
/// (`tools`) is re-read for its envelopes alone; nothing is counted twice and
/// the ledger equals a fresh collect.
#[test]
fn envelopes_of_a_version_certified_later_are_superseded_without_conflict() {
    let f = Fixture::new();
    let tools = ["complete", "guardian", "tools"].map(|name| plant(&f, name))[2].clone();
    f.cli("collect");
    let (fresh, fresh_usage) = (ledger(&f), f.cli_args(&["usage", "--json"]).0);
    f.sidecar().execute_batch("UPDATE codex_usage SET accepted=0,reason='cli_version_uncertified',cache_write_input_tokens=NULL,cached_input_tokens=NULL,
        input_tokens=NULL,output_tokens=NULL,reasoning_output_tokens=NULL,total_tokens=NULL;
        UPDATE rollout_sources SET thread_usage=NULL,token_count_usage=NULL; DELETE FROM codex_discrepancy;
        UPDATE source_observations SET measurement=json_set(measurement,'$.certified',json('false'));
        UPDATE rollout_ingest_state SET uncertified_envelopes=1;").unwrap();
    let flags = |f: &Fixture| rows::<i64>(f, "SELECT json_extract(measurement,'$.certified'),count(*) FROM source_observations GROUP BY 1");
    // complete 10 + guardian 4 + tools 13 envelopes.
    assert_eq!(flags(&f), [vec![0, 27]]);
    assert_eq!(attempt_usage(&f.cli_args(&["usage", "--json"]).0), unavailable("cli_version_uncertified"), "a read re-reads nothing");

    let (report, _) = f.cli("collect");
    assert_eq!((&report["collected"]["records"], &report["collected"]["reevaluated"]), (&0.into(), &3.into()));
    assert_eq!(flags(&f), [vec![1, 27]]);
    assert_eq!(rows::<i64>(&f, &format!("SELECT count(*) FROM source_observations WHERE producer_epoch='{}' AND json_extract(measurement,'$.certified')=1",
        source(&tools))), [vec![13]], "a rollout without usage records is re-read for its envelopes");
    assert_eq!(f.count("ingest_quarantine"), 0, "no digest conflict");
    assert!(fresh == ledger(&f), "the re-read sidecar equals a fresh collect");
    // complete 1500/500/0/180/100/1680 + guardian 25/0/0/5/1/30.
    assert_eq!(attempt_usage(&report), json!({"input_tokens": 1525, "cached_input_tokens": 500, "cache_write_input_tokens": 0, "output_tokens": 185,
        "reasoning_output_tokens": 101, "total_tokens": 1710, "records": 3}));
    assert_eq!(f.cli_args(&["usage", "--json"]).0, fresh_usage);
    let (again, _) = f.cli("collect");
    assert_eq!((&again["collected"]["bytes"], &again["collected"]["reevaluated"]), (&0.into(), &0.into()), "certified envelopes are not re-read again");
}

/// A8 on the CLI (docs/telemetry/codex-live-0.154.0-run2.md, the steward's §7
/// allowlist): the MCP call's server and tool names, status, hint, error flag
/// and duration; the failed command's exec row; the subagent and collab items'
/// type, ids and status; the function calls' namespace; the aborted turn as
/// the turn's final event (`aborted`, never `open` or a lost final event);
/// and the fork's point. The fork counts only its own record, and its reported
/// totals, which include its origin's thread total at the fork point,
/// reconcile once that is subtracted: `origin_not_collected` (no discrepancy)
/// while the origin is not collected, `reconciled` once it is, and still
/// `reconciled` after the origin grows past the fork point. Lane B's M16/M17
/// (B12) count the failed command as a failure and the MCP call once. No sentinel
/// (MCP arguments or result content included) leaks.
#[test]
fn live_run2_shapes_are_collected_without_content() {
    let f = Fixture::new();
    let complete = plant(&f, "complete");
    let origin_end = fs::metadata(&complete).unwrap().len() as i64;
    let tools = plant(&f, "live2-tools");
    let fork = plant(&f, "live2-fork");
    let corpus: Vec<u8> = [&tools, &fork].iter().flat_map(|p| fs::read(p).unwrap()).collect();
    for needle in ["LIVE2LEAK_ARGUMENT", "LIVE2LEAK_MCP_RESULT", "LIVE2LEAK_STDERR_2", "LIVE2LEAK_SPAWN_ARGUMENTS", "LIVE2LEAK_AGENT_PATH",
        "LIVE2LEAK_RECEIVER_NICKNAME", "LIVE2LEAK_AGENT_STATE", "LIVE2LEAK_DECLINED_OUTPUT", "LIVE2LEAK_SETTINGS_PATH", "LIVE2LEAK_MULTI_AGENT"] {
        assert!(corpus.windows(needle.len()).any(|w| w == needle.as_bytes()), "{needle}");
    }
    use rusqlite::types::Value::{Integer as I, Null as N, Text as T};
    let reconciliation = |f: &Fixture| rows::<rusqlite::types::Value>(f, &format!("SELECT kind,state,origin_total FROM codex_fork_reconciliation
        WHERE session_id='{LIVE2_FORK_SID}' ORDER BY kind"));
    let fork_discrepancies = |f: &Fixture| rows::<i64>(f, &format!("SELECT count(*) FROM codex_discrepancy WHERE session_id='{LIVE2_FORK_SID}'"))[0][0];

    // The origin not collected yet: no false discrepancy, and none claimed.
    let held = complete.with_file_name("held-complete.bak");
    fs::rename(&complete, &held).unwrap();
    let (report, mut output) = f.cli("collect");
    assert_eq!(reconciliation(&f), [vec![T("thread_total".into()), T("origin_not_collected".into()), N],
        vec![T("token_count_total".into()), T("origin_not_collected".into()), N]]);
    assert_eq!(fork_discrepancies(&f), 0);
    assert_eq!(attempt_usage(&report)["total_tokens"], 320, "the fork's own record only");

    // The origin collected: its thread total at the fork point (1680) is subtracted.
    fs::rename(&held, &complete).unwrap();
    let (report, bytes) = f.cli("collect");
    output.extend(bytes);
    for path in [&complete, &tools, &fork] { assert_eq!(quarantine(&f, &source(path)), [], "no new shape is malformed"); }
    // complete 1500/500/0/180/100/1680 + the fork's own record 300/100/0/20/0/320:
    // the fork's reported thread total (2000), never added.
    assert_eq!(attempt_usage(&report), json!({"input_tokens": 1800, "cached_input_tokens": 600, "cache_write_input_tokens": 0, "output_tokens": 200,
        "reasoning_output_tokens": 100, "total_tokens": 2000, "records": 3}));
    let reconciled = [vec![T("thread_total".into()), T("reconciled".into()), I(1680)], vec![T("token_count_total".into()), T("reconciled".into()), I(1680)]];
    assert_eq!(reconciliation(&f), reconciled);
    assert_eq!(fork_discrepancies(&f), 0, "no false discrepancy for the fork");
    assert_eq!(rows::<i64>(&f, &format!("SELECT count(*) FROM codex_discrepancy WHERE session_id='{LIVE2_SID}'")), [vec![0]]);

    // The origin grows past the fork point: the fork still reconciles against 1680.
    let record = r#"{"timestamp":"2030-01-01T00:10:00.000Z","type":"token_usage_record","payload":{"turn_id":"turn-2","response_id":"resp-after-fork","usage":{"input_tokens":10,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":5,"reasoning_output_tokens":0,"total_tokens":15},"thread_token_usage":{"input_tokens":1510,"cached_input_tokens":500,"cache_write_input_tokens":0,"output_tokens":185,"reasoning_output_tokens":100,"total_tokens":1695}}}"#;
    std::io::Write::write_all(&mut fs::OpenOptions::new().append(true).open(&complete).unwrap(), format!("{record}\n").as_bytes()).unwrap();
    let (report, bytes) = f.cli("collect");
    output.extend(bytes);
    assert_eq!(attempt_usage(&report)["total_tokens"], 2015);
    assert_eq!(reconciliation(&f), reconciled);
    assert_eq!(fork_discrepancies(&f), 0);

    let t = |ms: i64| Some(1_893_456_000_000 + ms);
    let (listed, bytes) = f.cli_args(&["collectors", "tools", "--json"]);
    output.extend(bytes);
    let session = listed["sessions"].as_array().unwrap().iter().find(|s| s["session_id"] == LIVE2_SID).unwrap().clone();
    let exec = Some("exec");
    let collab = |mut call: Value| { call["namespace"] = json!("collaboration"); call };
    let child = "00000000-0000-4000-8000-0000000b2003";
    assert_eq!(session, json!({"session_id": LIVE2_SID, "attempt_ids": [f.attempt], "tool_calls": [
            call("call-m1", Some("custom_tool_call"), exec, Some("completed"), Some("turn-l1"), t(6_700), Some("custom_tool_call_output"), t(6_765)),
            call("call-s1", Some("custom_tool_call"), exec, Some("completed"), Some("turn-l1"), t(12_673), Some("custom_tool_call_output"), t(15_733)),
            call("call-f1", Some("custom_tool_call"), exec, Some("completed"), Some("turn-l1"), t(17_494), Some("custom_tool_call_output"), t(17_625)),
            collab(call("call-c1", Some("function_call"), Some("spawn_agent"), None, Some("turn-l1"), t(20_000), Some("function_call_output"), t(20_086))),
            collab(call("call-c2", Some("function_call"), Some("wait_agent"), None, Some("turn-l1"), t(27_000), Some("function_call_output"), t(28_078))),
            call("call-d1", Some("custom_tool_call"), exec, Some("completed"), Some("turn-l2"), t(71_500), Some("custom_tool_call_output"), t(80_243))],
        "exec_items": [
            {"item_id": "exec-s1", "thread_id": LIVE2_SID, "turn_id": "turn-l1", "status": "completed", "source": "unified_exec_startup", "exit_code": 0,
                "startup_duration": {"secs": 2, "nanos": 879_307_435}, "completed_unix_ms": t(15_729)},
            {"item_id": "exec-f1", "thread_id": LIVE2_SID, "turn_id": "turn-l1", "status": "failed", "source": "unified_exec_startup", "exit_code": 2,
                "startup_duration": {"secs": 0, "nanos": 3610}, "completed_unix_ms": t(17_565)}],
        "mcp_calls": [
            {"item_id": "exec-m1", "thread_id": LIVE2_SID, "turn_id": "turn-l1", "server": "live2_stub_server", "tool": "live2_noop_tool", "status": "completed",
                "read_only_hint": true, "is_error": false, "duration": {"secs": 0, "nanos": 556_587}, "completed_unix_ms": t(6_724)}],
        "agent_items": [
            {"item_id": "call-c1", "type": "SubAgentActivity", "thread_id": LIVE2_SID, "turn_id": "turn-l1", "status": null, "agent_thread_id": child,
                "sender_thread_id": null, "receiver_thread_ids": null, "completed_unix_ms": t(20_036)},
            {"item_id": "subagent-completed-turn-x1", "type": "SubAgentActivity", "thread_id": LIVE2_SID, "turn_id": "turn-l1", "status": null, "agent_thread_id": child,
                "sender_thread_id": null, "receiver_thread_ids": null, "completed_unix_ms": t(28_075)},
            {"item_id": "call-c2", "type": "CollabAgentToolCall", "thread_id": LIVE2_SID, "turn_id": "turn-l1", "status": "completed", "agent_thread_id": null,
                "sender_thread_id": LIVE2_SID, "receiver_thread_ids": [child], "completed_unix_ms": t(28_076)}],
        "turn_aborts": [{"turn_id": "turn-l2", "reason": "interrupted", "duration_ms": 20_290, "aborted_unix_ms": t(80_248)}]}));
    assert!(listed["sessions"].as_array().unwrap().iter().filter(|s| s["session_id"] != LIVE2_SID)
        .all(|s| [&s["tool_calls"], &s["exec_items"], &s["mcp_calls"], &s["agent_items"], &s["turn_aborts"]].iter().all(|l| **l == json!([]))));
    let text = f.text(&["collectors", "tools"]);
    for line in ["  call call-c1 function_call name=spawn_agent namespace=collaboration status=- turn=turn-l1 called=1893456020000 output=1893456020086 call_to_output_ms=86\n",
        "  mcp exec-m1 server=live2_stub_server tool=live2_noop_tool status=completed read_only_hint=true is_error=false turn=turn-l1 completed=1893456006724\n",
        &format!("  agent call-c2 CollabAgentToolCall status=completed agent_thread=- sender={LIVE2_SID} receivers={child} turn=turn-l1 completed=1893456028076\n"),
        "  abort turn-l2 reason=interrupted duration_ms=20290 aborted=1893456080248\n"] {
        assert!(text.contains(line), "{line}{text}");
    }
    output.extend(text.into_bytes());

    // Envelopes: exactly the allowlist per item type; `thread_settings_applied` has none.
    let at = line_starts(&tools);
    let key = source(&tools);
    let envelope = |line: usize| envelopes(&f, &key).into_iter().find(|e| e.0 == at[line - 1]).map(|e| (e.1, e.2)).unwrap();
    let kinds: Vec<String> = envelopes(&f, &key).into_iter().map(|e| e.1).collect();
    let tool = ["custom_tool_call", "item_completed", "custom_tool_call_output"];
    let spawn = ["function_call", "item_completed", "function_call_output"];
    let expected: Vec<String> = [&["session_meta", "task_started", "turn_context"][..], &tool, &tool, &tool, &spawn, &["function_call", "item_completed", "item_completed", "function_call_output"],
        &["task_complete", "task_started", "turn_context", "custom_tool_call", "custom_tool_call_output", "turn_aborted"]].concat().into_iter().map(|k| format!("codex.{k}.v1")).collect();
    assert_eq!(kinds, expected);
    let item = |set: &str| item_envelope(set, LIVE2_SID, "turn-l1");
    assert_eq!(envelope(5).1, item(r#""duration":{"nanos":556587,"secs":0},"id":"exec-m1","readOnlyHint":true,"result":{"isError":false},"server":"live2_stub_server","status":"completed","tool":"live2_noop_tool","type":"McpToolCall""#));
    assert_eq!(envelope(11).1, item(r#""duration":{"nanos":3610,"secs":0},"exit_code":2,"id":"exec-f1","source":"unified_exec_startup","status":"failed","type":"CommandExecution""#));
    assert_eq!(envelope(14).1, item(&format!(r#""agent_thread_id":"{child}","id":"call-c1","type":"SubAgentActivity""#)));
    assert_eq!(envelope(18).1, item(&format!(r#""id":"call-c2","receiver_thread_ids":["{child}"],"sender_thread_id":"{LIVE2_SID}","status":"completed","type":"CollabAgentToolCall""#)));
    assert_eq!(envelope(13).1, r#"{"call_id":"call-c1","internal_chat_message_metadata_passthrough":{"turn_id":"turn-l1"},"name":"spawn_agent","namespace":"collaboration","status":null}"#);
    assert_eq!(envelope(25).1, r#"{"duration_ms":20290,"reason":"interrupted","turn_id":"turn-l2"}"#);
    let fork_envelopes = envelopes(&f, &source(&fork));
    assert_eq!(fork_envelopes.iter().map(|e| e.1.clone()).collect::<Vec<_>>(),
        ["session_meta", "task_started", "turn_context", "token_usage_record", "token_count", "task_complete"].map(|k| format!("codex.{k}.v1")));
    let meta: Value = serde_json::from_str(&fork_envelopes[0].2).unwrap();
    assert_eq!((&meta["forked_from_ordinal_exclusive"], &meta["history_base"]),
        (&json!(37), &json!({"thread_id": SID, "end_ordinal_exclusive": 37, "end_byte_offset": origin_end})));

    // Sessions: the aborted turn is complete-with-abort, never open or missing; the fork names its point.
    let (sessions, bytes) = f.cli_args(&["collectors", "sessions"]);
    output.extend(bytes);
    let by_id = |sid: &str| sessions["sessions"].as_array().unwrap().iter().find(|s| s["session_id"] == sid).unwrap().clone();
    assert_eq!((&by_id(LIVE2_SID)["final_event"], &by_id(LIVE2_SID)["records"], &by_id(LIVE2_SID)["fork"]),
        (&json!({"state": "aborted", "turn_id": "turn-l2"}), &json!(0), &Value::Null));
    let fork_session = by_id(LIVE2_FORK_SID);
    assert_eq!((&fork_session["forked_from_id"], &fork_session["records"], &fork_session["final_event"]),
        (&json!(SID), &json!(1), &json!({"state": "complete", "turn_id": "turn-k1"})));
    assert_eq!(fork_session["fork"], json!({"forked_from_ordinal_exclusive": 37,
        "history_base": {"thread_id": SID, "end_ordinal_exclusive": 37, "end_byte_offset": origin_end},
        "reconciliation": {"thread_total": "reconciled", "token_count_total": "reconciled"}}));

    // An aborted turn left idle past the threshold is no lost final event.
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    fs::File::options().append(true).open(&tools).unwrap().set_modified(old).unwrap();
    f.cli("collect");
    assert_eq!(rows::<i64>(&f, &format!("SELECT count(*) FROM coverage_gaps WHERE source='{key}' AND reason='final_event_missing'")), [vec![0]]);
    assert_eq!(f.cli_args(&["collectors", "sessions"]).0["sessions"].as_array().unwrap().iter().find(|s| s["session_id"] == LIVE2_SID).unwrap()["final_event"]["state"],
        "aborted");

    // Lane B (B12): the failed command is a failure; the MCP call is executed once, beside its exec call.
    let (tools_report, bytes) = f.cli_args(&["accounting", "tools", "--json"]);
    output.extend(bytes);
    let (m16, m17) = (&tools_report["metrics"]["M16"], &tools_report["metrics"]["M17"]);
    assert_eq!((&m16["issued"]["calls"], &m16["issued"]["by_name"], &m16["issued"]["status_unreported"], &m16["value"]["executed"]),
        (&json!(6), &json!({"exec": 4, "spawn_agent": 1, "wait_agent": 1}), &json!(2), &json!(3)));
    assert_eq!((&m17["value"], &m17["unknown"]["by_reason"]), (&json!("2/3"), &json!({})));

    for args in [&["usage", "--json"][..], &["report", "--json"], &["accounting", "sync"], &["accounting", "sessions"], &["accounting", "quota", "--json"]] {
        output.extend(f.cli_args(args).1);
    }
    let state = f.project.join(".state");
    let leaks = |bytes: &[u8]| ["live2leak"].into_iter().find(|needle| bytes.to_ascii_lowercase().windows(needle.len()).any(|w| w == needle.as_bytes()));
    for name in ["telemetry.db", "telemetry.db-wal", "telemetry.db-shm"] {
        if let Ok(bytes) = fs::read(state.join(name)) { assert_eq!(leaks(&bytes), None, "{name}"); }
    }
    assert_eq!(leaks(&output), None, "{}", String::from_utf8_lossy(&output));
}

/// Third live run (certificate-live.md §3), same-file resume with a model
/// switch: turn 1 (`gpt-6-astra`) 100 + 120 input, 40 + 80 cached, 10 + 5
/// output, 2 + 0 reasoning; the resumed turn 2 (`gpt-5.6-luna`) 200/0/7/0.
/// Each record counts once (420/120/22/2, total 442, 3 records), the file's
/// reported thread totals (442) reconcile with no discrepancy, nothing is
/// quarantined, the last turn is complete, and the session has two model
/// segments (235 then 207). A second collect counts nothing again. No
/// sentinel reaches the sidecar or any output.
#[test]
fn live_run3_same_file_resume_with_a_model_switch_counts_each_record_once() {
    let f = Fixture::new();
    let path = plant(&f, "live3-resume");
    let corpus = fs::read(&path).unwrap();
    for needle in ["LIVE3LEAK_BASE_INSTRUCTIONS", "LIVE3LEAK_WORLD_STATE_2", "LIVE3LEAK_SETTINGS_CWD", "LIVE3LEAK_RESUME_PROMPT", "LIVE3LEAK_LAST_MESSAGE_2", "LIVE3LEAK_CREDITS"] {
        assert!(corpus.windows(needle.len()).any(|w| w == needle.as_bytes()), "{needle}");
    }
    let (report, mut output) = f.cli("collect");
    assert_eq!(quarantine(&f, &source(&path)), []);
    let sums = json!({"input_tokens": 420, "cached_input_tokens": 120, "cache_write_input_tokens": 0, "output_tokens": 22,
        "reasoning_output_tokens": 2, "total_tokens": 442, "records": 3});
    assert_eq!(attempt_usage(&report), sums);
    assert_eq!(rows::<i64>(&f, &format!("SELECT count(*) FROM codex_discrepancy WHERE session_id='{LIVE3_SID}'")), [vec![0]]);
    let (sessions, bytes) = f.cli_args(&["collectors", "sessions"]);
    output.extend(bytes);
    let session = sessions["sessions"].as_array().unwrap().iter().find(|s| s["session_id"] == LIVE3_SID).unwrap().clone();
    assert_eq!((&session["records"], &session["final_event"], &session["fork"], &session["forked_from_id"]),
        (&json!(3), &json!({"state": "complete", "turn_id": "turn-r2"}), &Value::Null, &Value::Null));
    output.extend(f.cli_args(&["accounting", "sync"]).1);
    let (accounting, bytes) = f.cli_args(&["accounting", "sessions"]);
    output.extend(bytes);
    let row = accounting["sessions"].as_array().unwrap().iter().find(|s| s["session_id"] == LIVE3_SID).unwrap();
    let segments: Vec<(Value, Value, Value)> = row["segments"].as_array().unwrap().iter()
        .map(|s| (s["model"].clone(), s["entries"].clone(), s["total_tokens"].clone())).collect();
    assert_eq!(segments, [(json!("gpt-6-astra"), json!(2), json!(235)), (json!("gpt-5.6-luna"), json!(1), json!(207))]);
    assert_eq!(row["total_tokens"], json!(442));
    let (again, bytes) = f.cli("collect");
    output.extend(bytes);
    assert_eq!((&again["collected"]["records"], &attempt_usage(&again)), (&json!(0), &sums), "a re-read counts nothing twice");
    for args in [&["usage", "--json"][..], &["report", "--json"], &["collectors", "tools", "--json"], &["accounting", "quota", "--json"]] {
        output.extend(f.cli_args(args).1);
    }
    let state = f.project.join(".state");
    let leaks = |bytes: &[u8]| ["live3leak"].into_iter().find(|needle| bytes.to_ascii_lowercase().windows(needle.len()).any(|w| w == needle.as_bytes()));
    for name in ["telemetry.db", "telemetry.db-wal", "telemetry.db-shm"] {
        if let Ok(bytes) = fs::read(state.join(name)) { assert_eq!(leaks(&bytes), None, "{name}"); }
    }
    assert_eq!(leaks(&output), None, "{}", String::from_utf8_lossy(&output));
}

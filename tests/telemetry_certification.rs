//! TM2.6 core telemetry certification (docs/telemetry/certificate-core.md).
//!
//! An independent suite over plan doc 10's accounting fixtures (§3 golden
//! accounting, §4 replay/time/corrections/budget, §5a coordinator and quota),
//! run end to end: Codex rollouts written here line by line, collected and
//! reported through the `herdr-farm telemetry` CLI. Every expected value
//! is hand-computed from the plan's numbers in the comment beside it, never
//! read back from a production aggregate. Where the product disagrees with
//! the plan the certificate records who was right. Evidence here is
//! `fixture` (synthetic rollouts, real SQLite, real CLI processes); live
//! evidence is only cited from docs/telemetry/codex-live-0.154.0*.md.

#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.

mod support;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, path::{Path, PathBuf}, process::Command, time::Duration};
use support::telemetry::*;

// ---------------------------------------------------------------------------
// A tiny rollout writer. Lines follow the Codex 0.154.0 shapes of contracts §5
// (live-certified keys only), written literally so no fixture file can hide a
// value.

fn iso(ms: i64) -> String { jiff::Timestamp::from_millisecond(ms).unwrap().to_string() }

/// `[input, cached, cache_write, output, reasoning]`; total = input + output.
type Counts = [i64; 5];

fn counters([input, cached, write, output, reasoning]: Counts) -> Value {
    json!({"input_tokens": input, "cached_input_tokens": cached, "cache_write_input_tokens": write, "output_tokens": output,
        "reasoning_output_tokens": reasoning, "total_tokens": input + output})
}

#[derive(Clone)]
struct Rollout { lines: Vec<String>, sid: String }

impl Rollout {
    /// A primary `exec` session `sid` started at `ts` in `cwd`.
    fn new(sid: &str, cwd: &str, ts: i64) -> Self { Self::with(sid, cwd, ts, json!({})) }

    /// A session whose `session_meta` payload also carries `extra`.
    fn with(sid: &str, cwd: &str, ts: i64, extra: Value) -> Self {
        let mut payload = json!({"id": sid, "session_id": sid, "timestamp": iso(ts), "cwd": cwd, "originator": "codex_exec", "cli_version": "0.154.0",
            "source": "exec", "thread_source": "user", "model_provider": "openai"});
        payload.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        Rollout { lines: vec![json!({"timestamp": iso(ts), "type": "session_meta", "payload": payload}).to_string()], sid: sid.into() }
    }

    fn line(mut self, value: Value) -> Self { self.lines.push(value.to_string()); self }

    fn model(self, ts: i64, turn: &str, model: &str) -> Self {
        self.line(json!({"timestamp": iso(ts), "type": "turn_context", "payload": {"turn_id": turn, "model": model, "effort": "low"}}))
    }

    /// One `token_usage_record` (a per-response delta), with the thread's cumulative total when given.
    fn usage(self, ts: i64, turn: &str, response: &str, usage: Counts, thread: Option<Counts>) -> Self {
        let sid = self.sid.clone();
        let mut payload = json!({"thread_id": sid, "session_id": sid, "turn_id": turn, "root_turn_id": turn, "response_id": response, "usage": counters(usage)});
        if let Some(thread) = thread { payload["thread_token_usage"] = counters(thread); }
        self.line(json!({"timestamp": iso(ts), "type": "token_usage_record", "payload": payload}))
    }

    /// A `token_count` event carrying a rate-limit snapshot (`used` percent of a `window`-minute window).
    fn rate(self, ts: i64, used: f64, window: i64, resets_s: i64) -> Self {
        self.line(json!({"timestamp": iso(ts), "type": "event_msg", "payload": {"type": "token_count", "info": null,
            "rate_limits": {"limit_id": "codex", "limit_name": null, "plan_type": "pro", "primary": {"used_percent": used, "window_minutes": window, "resets_at": resets_s},
                "secondary": null, "rate_limit_reached_type": null}}}))
    }

    fn done(self, ts: i64, turn: &str) -> Self {
        self.line(json!({"timestamp": iso(ts), "type": "event_msg", "payload": {"type": "task_complete", "turn_id": turn, "duration_ms": 1000, "time_to_first_token_ms": 100}}))
    }

    fn text(&self) -> String { self.lines.iter().map(|l| format!("{l}\n")).collect() }
}

/// Write `text` as rollout `name` under `home`; returns its path.
fn plant(home: &Path, name: &str, text: &str) -> PathBuf {
    let dir = home.join(".codex/sessions/2026/09/28");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("rollout-2026-09-28T00-00-00-{name}.jsonl"));
    fs::write(&path, text).unwrap();
    path
}

fn path_digest(path: &Path) -> String { format!("sha256:{:x}", Sha256::digest(path.as_os_str().as_encoded_bytes())) }

fn sid(n: u32) -> String { format!("00000000-0000-4000-8000-0000000d{n:04x}") }

fn unavailable(reason: &str) -> Value { json!({"status": "unavailable", "reason": reason}) }

/// Collect and sync; returns the ledger entries by id.
fn sync(f: &Fixture) -> Value {
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    f.cli_args(&["accounting", "entries"]).0
}

fn entry(entries: &Value, id: &str) -> Value {
    entries["entries"].as_array().unwrap().iter().find(|e| e["entry_id"] == id).cloned().unwrap_or_else(|| panic!("{id} in {entries}"))
}

/// `(input, output)` of a ledger entry's normalized counters.
fn io(entry: &Value) -> (i64, i64) { (entry["normalized"]["input_tokens"].as_i64().unwrap(), entry["normalized"]["output_tokens"].as_i64().unwrap()) }

fn dispositions(entry: &Value) -> Vec<(String, Option<String>)> {
    let mut all: Vec<_> = entry["provenance"].as_array().unwrap().iter()
        .map(|p| (p["disposition"].as_str().unwrap().to_owned(), p["reason"].as_str().map(str::to_owned))).collect();
    all.sort();
    all
}

/// `(M08, M09)` values of the report.
fn m08_m09(f: &Fixture) -> (Value, Value) {
    let report = f.report();
    (report["metrics"]["M08"]["value"].clone(), report["metrics"]["M09"]["value"].clone())
}

fn session<'a>(sessions: &'a Value, sid: &str) -> &'a Value {
    sessions["sessions"].as_array().unwrap().iter().find(|s| s["session_id"] == sid).unwrap_or_else(|| panic!("{sid} in {sessions}"))
}

// ---------------------------------------------------------------------------
// Doc 10 §3 golden accounting fixtures.

/// Doc 10 §3 "Cumulative sequence": one epoch reports input/output 100/20 and
/// then 160/35. Codex carries the per-response deltas (100/20, then the
/// second increment 60/15) and the cumulative `thread_token_usage` (100/20,
/// 160/35). Required: total 160/35, second increment 60/15. The cumulative
/// observation reconciles (no discrepancy) and is never added to the deltas.
/// "Overlapping delta": the 60/15 delta and the 160/35 cumulative describe
/// the same interval: no extra usage. A cumulative total contradicting the
/// deltas is a recorded discrepancy, never counted; a record rewritten with
/// other values under the same native key is quarantined, and its session
/// leaves the sums (unknown, never a guess).
#[test]
fn cumulative_sequence_and_overlapping_delta() {
    let f = Fixture::new();
    let t = f.decided + 1_000;
    let (s1, s2) = (sid(0x0101), sid(0x0102));
    let sequence = |sid: &str, last_thread: Counts| Rollout::new(sid, &f.worktree(), t).model(t, "turn-1", "gpt-5.5")
        .usage(t + 1, "turn-1", "resp-1", [100, 0, 0, 20, 0], Some([100, 0, 0, 20, 0]))
        .usage(t + 2, "turn-1", "resp-2", [60, 0, 0, 15, 0], Some(last_thread))
        .done(t + 3, "turn-1");
    let a = plant(&f.home, "s1", &sequence(&s1, [160, 0, 0, 35, 0]).text());
    let entries = sync(&f);
    assert_eq!(io(&entry(&entries, &format!("codex:{s1}:1"))), (100, 20));
    assert_eq!(io(&entry(&entries, &format!("codex:{s1}:2"))), (60, 15), "the second increment");
    let cumulative = entry(&entries, &format!("codex:{s1}:thread:{}", path_digest(&a)));
    assert_eq!((io(&cumulative), &cumulative["basis"], &cumulative["precedence"]), ((160, 35), &json!("cumulative"), &json!(2)));
    assert_eq!(m08_m09(&f), (json!(160), json!(35)), "total 160/35: deltas counted once, the cumulative never added");
    assert_eq!(f.count("codex_discrepancy"), 0, "Σ deltas = the cumulative 160/35");

    // A cumulative total contradicting its deltas (170/40 against Σ 160/35).
    plant(&f.home, "s2", &sequence(&s2, [170, 0, 0, 40, 0]).text());
    sync(&f);
    assert_eq!(m08_m09(&f), (json!(160 + 160), json!(35 + 35)), "the contradicting cumulative adds nothing");
    let (summed, reported): (i64, i64) = f.sidecar().query_row("SELECT summed_total,reported_total FROM codex_discrepancy WHERE session_id=?1 AND kind='thread_total'",
        [&s2], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!((summed, reported), (195, 210), "recorded as a discrepancy: Σ 160 + 35, reported 170 + 40");

    // The same native key (s1, ordinal 2) rewritten with other values: quarantined, never summed.
    fs::remove_file(&a).unwrap();
    plant(&f.home, "s1", &Rollout::new(&s1, &f.worktree(), t).model(t, "turn-1", "gpt-5.5")
        .usage(t + 1, "turn-1", "resp-1", [100, 0, 0, 20, 0], Some([100, 0, 0, 20, 0]))
        .usage(t + 2, "turn-1", "resp-2", [61, 0, 0, 15, 0], Some([161, 0, 0, 35, 0])).text());
    let entries = sync(&f);
    assert_eq!(dispositions(&entry(&entries, &format!("codex:{s1}:2"))), [("conflict".to_owned(), Some("payload_digest_mismatch".to_owned()))]);
    let report = f.report();
    assert_eq!(report["metrics"]["M08"]["value"], 160, "only s2 remains; s1 is unknown, not 60 or 61");
    assert_eq!(report["metrics"]["M08"]["coverage"]["excluded"], json!({"quarantined": 1}));
}

/// Doc 10 §3 "Replay" and §4: repeating an observation never adds usage.
/// The same file collected again, the same file replaced (new inode, re-read
/// from byte 0), a resumed rollout repeating the session's records, and four
/// collectors racing over the same rollouts: every logical record has exactly
/// one `accepted` disposition; the repeats are `duplicate` provenance (kept
/// diagnostically); totals stay 160/35.
#[test]
fn replayed_observations_are_accepted_once() {
    let f = Fixture::new();
    let t = f.decided + 1_000;
    let s1 = sid(0x0201);
    let rollout = Rollout::new(&s1, &f.worktree(), t).model(t, "turn-1", "gpt-5.5")
        .usage(t + 1, "turn-1", "resp-1", [100, 0, 0, 20, 0], Some([100, 0, 0, 20, 0]))
        .usage(t + 2, "turn-1", "resp-2", [60, 0, 0, 15, 0], Some([160, 0, 0, 35, 0]))
        .done(t + 3, "turn-1");
    let a = plant(&f.home, "a", &rollout.text());
    sync(&f);
    let (entries, first) = f.cli_args(&["accounting", "entries"]);
    let (_, usage) = f.cli_args(&["usage", "--json"]);
    assert_eq!(dispositions(&entry(&entries, &format!("codex:{s1}:2"))), [("accepted".to_owned(), None)]);

    // Reread: the same bytes, then the same file replaced under the same path.
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(f.cli_args(&["accounting", "entries"]).1, first);
    fs::remove_file(&a).unwrap();
    plant(&f.home, "a", &rollout.text());
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(f.cli_args(&["accounting", "entries"]).1, first, "re-read from byte 0: every key dedupes");
    assert_eq!(f.cli_args(&["usage", "--json"]).1, usage);
    assert_eq!(f.count("codex_quarantine"), 0);

    // A resumed rollout repeating both records of the session.
    let b = plant(&f.home, "b", &rollout.text());
    let entries = sync(&f);
    for n in [1, 2] {
        assert_eq!(dispositions(&entry(&entries, &format!("codex:{s1}:{n}"))), [("accepted".to_owned(), None), ("duplicate".to_owned(), None)],
            "record {n}: one acceptance, the repeat kept as provenance");
    }
    let cumulative = [path_digest(&a), path_digest(&b)].map(|p| entry(&entries, &format!("codex:{s1}:thread:{p}"))["provenance"][0]["disposition"].clone());
    let mut cumulative = cumulative.to_vec();
    cumulative.sort_by_key(|d| d.to_string());
    assert_eq!(cumulative, [json!("accepted"), json!("duplicate")], "an equal cumulative total duplicates the first");
    assert_eq!(m08_m09(&f), (json!(160), json!(35)));

    // Four collectors race over three new sessions (plus the two rollouts above).
    for n in 0..3u32 {
        let s = sid(0x0210 + n);
        plant(&f.home, &format!("race-{n}"), &Rollout::new(&s, &f.worktree(), t).model(t, "turn-1", "gpt-5.5")
            .usage(t + 1, "turn-1", "resp-1", [10, 0, 0, 2, 0], Some([10, 0, 0, 2, 0])).text());
    }
    assert_eq!(race(&f, &["collect"], 6), Vec::<String>::new(), "racing collectors wait for each other; none fails");
    assert_eq!(race(&f, &["accounting", "sync"], 4), Vec::<String>::new(), "racing syncs");
    let entries = f.cli_args(&["accounting", "entries"]).0;
    let accepted = entries["entries"].as_array().unwrap().iter().filter(|e| e["basis"] == "delta")
        .map(|e| e["provenance"].as_array().unwrap().iter().filter(|p| p["disposition"] == "accepted").count()).collect::<Vec<_>>();
    assert_eq!(accepted, [1; 5], "zero duplicate acceptance: 2 records of s1 and 3 raced sessions, one acceptance each");
    assert_eq!(m08_m09(&f), (json!(160 + 30), json!(35 + 6)));
}

/// Run `n` copies of a telemetry command at once; returns the failures' stderr.
fn race(f: &Fixture, args: &[&str], n: usize) -> Vec<String> {
    let racers: Vec<_> = (0..n).map(|_| Command::new(BIN).env_clear().env("HOME", f.tmp.path().join("home")).env("PATH", "/usr/bin:/bin")
        .args(["--root", f.root.to_str().unwrap(), "telemetry", "demo"]).args(args)
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).spawn().unwrap()).collect();
    racers.into_iter().map(|c| c.wait_with_output().unwrap()).filter(|o| !o.status.success()).map(|o| String::from_utf8_lossy(&o.stderr).into_owned()).collect()
}

/// Doc 10 §3 "Replay" with the event repeated inside the source: the second
/// response written again at the end of the same rollout (same `response_id`,
/// same turn, same counters, same cumulative total). Required: totals
/// unchanged (160/35), the duplicate accounted for diagnostically.
#[test]
fn repeated_response_in_one_rollout_is_not_counted_twice() {
    let f = Fixture::new();
    let t = f.decided + 1_000;
    let s1 = sid(0x0301);
    plant(&f.home, "a", &Rollout::new(&s1, &f.worktree(), t).model(t, "turn-1", "gpt-5.5")
        .usage(t + 1, "turn-1", "resp-1", [100, 0, 0, 20, 0], Some([100, 0, 0, 20, 0]))
        .usage(t + 2, "turn-1", "resp-2", [60, 0, 0, 15, 0], Some([160, 0, 0, 35, 0]))
        .usage(t + 2, "turn-1", "resp-2", [60, 0, 0, 15, 0], Some([160, 0, 0, 35, 0]))
        .done(t + 3, "turn-1").text());
    let entries = sync(&f);
    assert_eq!(m08_m09(&f), (json!(160), json!(35)), "the repeated response adds nothing");
    // Attempt usage (`usage`, `attempts`) skips the repeat too (certificate R3).
    let usage = f.cli_args(&["usage", "--json"]).0;
    let mine = usage["attempts"].as_array().unwrap().iter().find(|a| a["attempt_id"] == f.attempt.as_str()).cloned().unwrap();
    assert_eq!((&mine["usage"]["input_tokens"], &mine["usage"]["output_tokens"], &mine["usage"]["records"]), (&json!(160), &json!(35), &json!(2)));
    assert_eq!(dispositions(&entry(&entries, &format!("codex:{s1}:3"))), [("duplicate".to_owned(), Some("response_repeated".to_owned()))],
        "accounted for diagnostically, not counted");
    assert_eq!(dispositions(&entry(&entries, &format!("codex:{s1}:2"))), [("accepted".to_owned(), None)]);
    let sessions = f.cli_args(&["accounting", "sessions"]).0;
    assert_eq!((&session(&sessions, &s1)["total_tokens"], &session(&sessions, &s1)["segments"][0]["entries"]), (&json!(195), &json!(2)));
    // Never valued either: complete at 100 × 2 + 20 × 4 + 60 × 2 + 15 × 4 per 10^6 = 0.00046, not partial.
    f.cli_args(&["accounting", "import-rate-card", &card(&f, "cert", 1, "USD", &["gpt-5.5"], &[("input", "2"), ("output", "4")])]);
    f.cli_args(&["accounting", "reprice"]);
    let mine = attempt_cost(&f.cli_args(&["accounting", "cost", "--json"]).0, &f.attempt);
    assert_eq!((&mine["estimate"], &mine["coverage"]), (&json!({"status": "complete", "currency": "USD", "amount": "0.00046"}),
        &json!({"entries": 2, "priced": 2, "unpriced": {}})));

    // Not a replay: the same response id with other counters, or another session's response id.
    plant(&f.home, "b", &Rollout::new(&sid(0x0302), &f.worktree(), t).model(t, "turn-1", "gpt-5.5")
        .usage(t + 1, "turn-1", "resp-2", [60, 0, 0, 15, 0], None).usage(t + 2, "turn-1", "resp-2", [7, 0, 0, 1, 0], None).text());
    sync(&f);
    assert_eq!(m08_m09(&f), (json!(160 + 67), json!(35 + 16)), "identity includes the session and the payload, never equal amounts alone");
}

/// Doc 10 §3 "Proven reset": a new runtime epoch starting at 10/2 after the
/// 160/35 epoch → combined 170/37, each epoch's provenance kept apart (for
/// Codex the proof of a new epoch is a new `session_meta.id`). "Unproven
/// decrease": a rollout of the same session whose cumulative total falls
/// (160/35 → 150/30) without any reset evidence is a gap, never negative
/// usage and never an assumed restart; its own new delta still counts.
/// An impossible record (cached > input) is refused, never summed.
#[test]
fn proven_reset_and_unproven_decrease() {
    let f = Fixture::new();
    let t = f.decided + 1_000;
    let (s1, s2, s3) = (sid(0x0401), sid(0x0402), sid(0x0403));
    let epoch = Rollout::new(&s1, &f.worktree(), t).model(t, "turn-1", "gpt-5.5")
        .usage(t + 1, "turn-1", "resp-1", [100, 0, 0, 20, 0], Some([100, 0, 0, 20, 0]))
        .usage(t + 2, "turn-1", "resp-2", [60, 0, 0, 15, 0], Some([160, 0, 0, 35, 0]));
    plant(&f.home, "epoch-1", &epoch.text());
    plant(&f.home, "epoch-2", &Rollout::new(&s2, &f.worktree(), t + 10).model(t + 10, "turn-1", "gpt-5.5")
        .usage(t + 11, "turn-1", "resp-1", [10, 0, 0, 2, 0], Some([10, 0, 0, 2, 0])).text());
    let entries = sync(&f);
    assert_eq!(m08_m09(&f), (json!(170), json!(37)), "combined 170/37");
    assert_eq!(io(&entry(&entries, &format!("codex:{s2}:1"))), (10, 2), "the new epoch keeps its own provenance");
    let sessions = f.cli_args(&["accounting", "sessions"]).0;
    assert_eq!((&session(&sessions, &s1)["total_tokens"], &session(&sessions, &s2)["total_tokens"]), (&json!(195), &json!(12)));

    // The same session again, one more delta (5/1), but its cumulative falls to 150/30.
    let resumed = plant(&f.home, "epoch-1-resumed", &epoch.clone().usage(t + 3, "turn-1", "resp-3", [5, 0, 0, 1, 0], Some([150, 0, 0, 30, 0])).text());
    let entries = sync(&f);
    let fallen = entry(&entries, &format!("codex:{s1}:thread:{}", path_digest(&resumed)));
    assert_eq!(dispositions(&fallen), [("unresolved".to_owned(), Some("regression_without_reset".to_owned()))]);
    assert_eq!(m08_m09(&f), (json!(175), json!(38)), "the delta counts; the decrease subtracts nothing");

    // cached 50 > input 40 violates the subset rule: kept, not accepted, the source excluded from M08.
    plant(&f.home, "invalid", &Rollout::new(&s3, &f.worktree(), t).model(t, "turn-1", "gpt-5.5")
        .usage(t + 1, "turn-1", "resp-1", [40, 50, 0, 10, 0], None).text());
    let entries = sync(&f);
    assert_eq!(dispositions(&entry(&entries, &format!("codex:{s3}:1"))), [("unresolved".to_owned(), Some("invariant_violation".to_owned()))]);
    assert_eq!(entry(&entries, &format!("codex:{s3}:1"))["normalized"], unavailable("not_normalized"));
    let report = f.report();
    assert_eq!(report["metrics"]["M08"]["value"], 175);
    assert_eq!(report["metrics"]["M08"]["coverage"]["excluded"], json!({"records_not_accepted": 1}));
}

/// Doc 10 §3 "Inclusive child" and "Unknown child relation", on Codex's live
/// fork shape (codex-live-0.154.0-run2.md §1): an origin of 50 (40/10) and a
/// fork whose own record is 150 (120/30) but whose reported thread total is
/// 200 (160/40), which includes the origin. Required: project total 200;
/// the fork's exclusive 150 only because the inclusion is evidenced
/// (`history_base` names the origin and reconciles). A fork naming its origin
/// without that evidence: both observations kept, no fabricated sum. A
/// spawned child (30) collected before its parent (100) is reported apart
/// (`parent_not_collected`) and, once the parent arrives, linked without
/// entering the parent's total (live: the parent's total excludes it).
#[test]
fn inclusive_and_unknown_child_relations() {
    let f = Fixture::new();
    let t = f.decided + 1_000;
    let (origin, fork, bare, parent, child) = (sid(0x0501), sid(0x0502), sid(0x0503), sid(0x0504), sid(0x0505));
    let o = plant(&f.home, "origin", &Rollout::new(&origin, &f.worktree(), t).model(t, "turn-1", "gpt-5.5")
        .usage(t + 1, "turn-1", "resp-o1", [40, 0, 0, 10, 0], Some([40, 0, 0, 10, 0])).done(t + 2, "turn-1").text());
    let end = fs::metadata(&o).unwrap().len();
    plant(&f.home, "fork", &Rollout::with(&fork, &f.worktree(), t + 10, json!({"forked_from_id": origin, "forked_from_ordinal_exclusive": 4,
        "history_base": {"thread_id": origin, "end_ordinal_exclusive": 4, "end_byte_offset": end}}))
        .model(t + 10, "turn-k", "gpt-5.5").usage(t + 11, "turn-k", "resp-k1", [120, 0, 0, 30, 0], Some([160, 0, 0, 40, 0])).done(t + 12, "turn-k").text());
    sync(&f);
    let sessions = f.cli_args(&["accounting", "sessions"]).0;
    assert_eq!(session(&sessions, &origin)["total_tokens"], 50);
    let forked = session(&sessions, &fork);
    assert_eq!((&forked["role"], &forked["linkage"], &forked["total_tokens"]), (&json!("fork"), &json!("linked_child"), &json!(150)),
        "exclusive 150 (reported 200 includes the origin's 50)");
    let listed = &session(&sessions, &origin)["children"]["sessions"][0];
    assert_eq!((&listed["inclusion"], &listed["total_tokens"]), (&json!("separate"), &json!(150)));
    assert_eq!(sessions["rollup"], json!({"sessions": 50, "linked_children": 150, "unlinked_children": 0, "incomplete_sessions": 0}));
    assert_eq!(m08_m09(&f), (json!(160), json!(40)), "project total 200 = 160 + 40, never 250 or 400");
    let states: Vec<String> = f.sidecar().prepare("SELECT state FROM codex_fork_reconciliation WHERE session_id=?1 ORDER BY kind").unwrap()
        .query_map([&fork], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
    assert_eq!(states, ["reconciled"], "200 − the origin's 50 = the fork's own 150");
    assert_eq!(f.count("codex_discrepancy"), 0);

    // Unknown relation: a fork naming the origin with no history base.
    plant(&f.home, "bare", &Rollout::with(&bare, &f.worktree(), t + 20, json!({"forked_from_id": origin}))
        .model(t + 20, "turn-b", "gpt-5.5").usage(t + 21, "turn-b", "resp-b1", [120, 0, 0, 30, 0], Some([160, 0, 0, 40, 0])).text());
    sync(&f);
    let sessions = f.cli_args(&["accounting", "sessions"]).0;
    assert_eq!(session(&sessions, &bare)["total_tokens"], 150, "its own observation is kept");
    assert_eq!(session(&sessions, &origin)["children"]["total_tokens"], unavailable("fork_replay_not_certified"));
    assert_eq!(sessions["rollup"]["linked_children"], unavailable("fork_replay_not_certified"), "no fabricated children sum");
    assert_eq!(sessions["rollup"]["sessions"], 50);

    // A delayed child: the spawned subagent (30) arrives before its parent (100).
    plant(&f.home, "child", &Rollout::with(&child, &f.worktree(), t + 30, json!({"source": {"subagent": {"thread_spawn": {"parent_thread_id": parent, "depth": 1}}},
        "parent_thread_id": parent, "session_id": parent, "thread_source": "subagent"}))
        .model(t + 30, "turn-c", "gpt-5.5").usage(t + 31, "turn-c", "resp-c1", [25, 0, 0, 5, 0], Some([25, 0, 0, 5, 0])).text());
    sync(&f);
    let sessions = f.cli_args(&["accounting", "sessions"]).0;
    let waiting = session(&sessions, &child);
    assert_eq!((&waiting["linkage"], &waiting["parent"]), (&json!("unlinked_child"),
        &json!({"status": "unavailable", "reason": "parent_not_collected", "session_id": parent})));
    plant(&f.home, "parent", &Rollout::new(&parent, &f.worktree(), t + 25).model(t + 25, "turn-p", "gpt-5.5")
        .usage(t + 32, "turn-p", "resp-p1", [80, 0, 0, 20, 0], Some([80, 0, 0, 20, 0])).text());
    sync(&f);
    let sessions = f.cli_args(&["accounting", "sessions"]).0;
    assert_eq!((&session(&sessions, &child)["linkage"], &session(&sessions, &parent)["total_tokens"]), (&json!("linked_child"), &json!(100)),
        "linked late; the parent stays 100, never 130");
    assert_eq!(session(&sessions, &parent)["children"]["total_tokens"], 30);
    // Every record once: 40 + 120 + 120 + 25 + 80 input, 10 + 30 + 30 + 5 + 20 output.
    assert_eq!(m08_m09(&f), (json!(385), json!(95)));
}

/// Doc 10 §3 "Cache/reasoning subsets": input 100 includes cached 40, output
/// 30 includes reasoning 10 → totals stay 100/30 (130); cached and reasoning
/// are dimensions, not extra tokens. Event-ID reuse across unrelated scopes:
/// another session reusing the same response and turn ids and the same
/// counters is another invocation (identity includes the session scope;
/// equal amounts never dedupe). Hidden model: a record before any model
/// evidence is `unallocated`, never given the next turn's model, and never
/// counts toward effective-model coverage (M15).
#[test]
fn cache_reasoning_subsets_identity_scope_and_hidden_models() {
    let f = Fixture::new();
    let t = f.decided + 1_000;
    let (s1, s2, s3) = (sid(0x0601), sid(0x0602), sid(0x0603));
    let subsets = |sid: &str| Rollout::new(sid, &f.worktree(), t).model(t, "turn-1", "gpt-5.5")
        .usage(t + 1, "turn-1", "resp-1", [100, 40, 0, 30, 10], Some([100, 40, 0, 30, 10]));
    plant(&f.home, "subsets", &subsets(&s1).text());
    let entries = sync(&f);
    assert_eq!(entry(&entries, &format!("codex:{s1}:1"))["normalized"], json!({"input_tokens": 100, "cache_read_tokens": 40, "new_input_tokens": 60,
        "cache_write_tokens": 0, "output_tokens": 30, "reasoning_tokens": 10, "total_tokens": 130}));
    let report = f.report();
    assert_eq!((&report["metrics"]["M08"]["value"], &report["metrics"]["M09"]["value"], &report["metrics"]["M09"]["reasoning_output_tokens"]),
        (&json!(100), &json!(30), &json!(10)));

    plant(&f.home, "reuse", &subsets(&s2).text());
    sync(&f);
    assert_eq!(m08_m09(&f), (json!(200), json!(60)), "same response id, other session: counted");

    // A record with no model evidence; the model's turn context arrives in a later pass, with a second record.
    let hidden = Rollout::new(&s3, &f.worktree(), t).usage(t + 1, "turn-0", "resp-h0", [10, 0, 0, 5, 0], None);
    let path = plant(&f.home, "hidden", &hidden.text());
    sync(&f);
    fs::write(&path, hidden.model(t + 2, "turn-1", "gpt-5.5").usage(t + 3, "turn-1", "resp-h1", [20, 0, 0, 5, 0], None).text()).unwrap();
    sync(&f);
    assert_eq!(f.count("codex_quarantine"), 0, "the late model evidence rewrites nothing");
    let sessions = f.cli_args(&["accounting", "sessions"]).0;
    let hidden = session(&sessions, &s3);
    assert_eq!((&hidden["unallocated"]["entries"], &hidden["unallocated"]["total_tokens"]), (&json!(1), &json!(15)));
    assert_eq!((&hidden["segments"][0]["model"], &hidden["segments"][0]["total_tokens"]), (&json!("gpt-5.5"), &json!(25)));
    let m15 = &f.report()["metrics"]["M15"];
    assert_eq!((&m15["numerator"], &m15["denominator"]), (&json!(3), &json!(4)), "the model-less record never qualifies: {m15}");
}

// ---------------------------------------------------------------------------
// Doc 10 §3 cost, §4 corrections and time, §5a quota.

/// A synthetic rate card (invented rates, never a provider price) for
/// `models`, effective from 0 with no end; returns its path.
fn card(f: &Fixture, name: &str, version: u32, currency: &str, models: &[&str], rates: &[(&str, &str)]) -> String {
    let path = f.tmp.path().join(format!("{name}-v{version}.json"));
    fs::write(&path, json!({"card_id": name, "version": version, "provider": "openai", "product": "codex", "models": models, "currency": currency,
        "rate_unit": 1_000_000, "effective_from_unix_ms": 0, "includes": {"discounts": false, "taxes": false, "fees": false},
        "source": "INVENTED synthetic test rates for TM2.6 certification; not a provider price",
        "rates": rates.iter().map(|(category, rate)| json!({"category": category, "rate": rate})).collect::<Vec<_>>()}).to_string()).unwrap();
    path.display().to_string()
}

fn attempt_cost(cost: &Value, attempt: &str) -> Value {
    cost["attempts"].as_array().unwrap().iter().find(|a| a["attempt_id"] == attempt).cloned().unwrap_or_else(|| panic!("{attempt} in {cost}"))
}

/// Wait until the clock has passed `t`, so the next recorded time is later.
fn after(t: i64) { while unix_ms() <= t { std::thread::sleep(Duration::from_millis(1)); } }

/// Doc 10 §3 cost: 1,000 input tokens at USD 2/million plus 500 output at
/// USD 4/million = USD 0.004, exact. A missing model rate, a missing cache
/// convention (a cache write) and a missing cache-read rate leave the
/// attempt partial (priced 0.004 of 4 entries), never a total and never 0.
/// A missing currency conversion is unavailable. §4 corrections: a revised
/// rate card (output 6 → 0.005) appends a calculation revision while the
/// earlier one stays byte-identical by revision and by `as_of`; racing
/// reprices append it once. A provider charge of 0.0100 corrected to 0.0080
/// appends −0.002 and `as_of` the first import still shows 0.01. §4 time: a
/// record that happened before revision 2 but was observed after it is
/// excluded from the view as known then and included by the restatement,
/// which names its new cutoff.
#[test]
fn cost_golden_corrections_and_as_of_views() {
    let f = Fixture::new();
    let t = f.decided + 1_000;
    let session = |n: u32, model: &str, usage: Counts, response: &str| {
        let s = sid(0x0700 + n);
        plant(&f.home, &format!("cost-{n}"), &Rollout::new(&s, &f.worktree(), t).model(t, "turn-1", model).usage(t + 1, "turn-1", response, usage, None).text());
        s
    };
    session(1, "gpt-5.5", [1000, 0, 0, 500, 0], "resp-p1");
    session(2, "gpt-5.5-mini", [100, 0, 0, 10, 0], "resp-p2");
    session(3, "gpt-5.5", [100, 0, 10, 10, 0], "resp-p3");
    session(4, "gpt-5.5", [100, 40, 0, 10, 0], "resp-p4");
    sync(&f);
    assert_eq!(f.report()["metrics"]["M12"]["value"], unavailable("not_priced"));
    f.cli_args(&["accounting", "import-rate-card", &card(&f, "cert", 1, "USD", &["gpt-5.5"], &[("input", "2"), ("output", "4")])]);
    assert_eq!(f.cli_args(&["accounting", "reprice"]).0["revision"], 1);
    let (cost, revision1) = f.cli_args(&["accounting", "cost", "--json"]);
    let mine = attempt_cost(&cost, &f.attempt);
    assert_eq!(mine["estimate"], json!({"status": "partial", "reason": "unpriced_entries", "currency": "USD", "priced_amount": "0.004"}),
        "1000 × 2/10^6 + 500 × 4/10^6 = 0.004; the rest unknown, not 0");
    assert_eq!(mine["coverage"], json!({"entries": 4, "priced": 1, "unpriced": {"cache_read_rate_missing": 1, "cache_write_convention_unknown": 1, "no_rate_card": 1}}));
    let report = f.report();
    assert_eq!((&report["metrics"]["M14"]["value"], &report["metrics"]["M12"]["value"]["status"]), (&json!("1/4"), &json!("partial")));
    let fx = f.cli_args(&["accounting", "fx", "--to", "EUR"]).0;
    let converted = fx["entries"].as_array().unwrap().iter().find(|e| e["entry_id"] == format!("codex:{}:1", sid(0x0701))).unwrap()["converted"].clone();
    assert_eq!(converted, unavailable("no_fx_rate"), "no dated conversion: never assumed");

    // A revised card: output 6/10^6 → 0.002 + 0.003 = 0.005. Racing reprices append one revision.
    let computed1 = cost["computed_unix_ms"].as_i64().unwrap();
    after(computed1);
    f.cli_args(&["accounting", "import-rate-card", &card(&f, "cert", 2, "USD", &["gpt-5.5"], &[("input", "2"), ("output", "6")])]);
    assert_eq!(race(&f, &["accounting", "reprice"], 4), Vec::<String>::new());
    assert_eq!(f.count("valuation_revisions"), 2, "one revision for the one change");
    let cost2 = f.cli_args(&["accounting", "cost", "--json"]).0;
    assert_eq!((&cost2["revision"], &attempt_cost(&cost2, &f.attempt)["estimate"]["priced_amount"]), (&json!(2), &json!("0.005")));
    assert_eq!(f.cli_args(&["accounting", "cost", "--json", "--revision", "1"]).1, revision1, "the earlier calculation is reproducible");
    assert_eq!(f.cli_args(&["accounting", "cost", "--json", "--as-of", &computed1.to_string()]).1, revision1);
    let computed2 = cost2["computed_unix_ms"].as_i64().unwrap();

    // A provider charge, then its correction.
    let charges = |revision: u32, amount: &str| {
        let path = f.tmp.path().join(format!("charges-{revision}.json"));
        fs::write(&path, json!({"synthetic": true, "source": "INVENTED synthetic charge for TM2.6 certification; not a provider export", "provider": "openai",
            "product": "codex", "charges": [{"charge_id": "ch-1", "revision": revision, "currency": "USD", "amount": amount, "response_id": "resp-p1"}],
            "invoices": []}).to_string()).unwrap();
        f.cli_args(&["accounting", "import-charges", path.to_str().unwrap()]);
    };
    after(computed2);
    charges(1, "0.0100");
    let view = f.cli_args(&["accounting", "charges"]).0;
    let ch1 = view["charges"][0].clone();
    assert_eq!((&ch1["amount"], &ch1["reconciliation"]["estimate"]["amount"], &ch1["reconciliation"]["difference"]["amount"]),
        (&json!("0.01"), &json!("0.005"), &json!("0.005")), "the charge is matched by response id and compared, never added");
    assert_eq!(f.report()["metrics"]["M11"]["value"], "0.01");
    let first_import = ch1["history"][0]["imported_unix_ms"].as_i64().unwrap();
    after(first_import);
    charges(2, "0.0080");
    let ch1 = f.cli_args(&["accounting", "charges"]).0["charges"][0].clone();
    assert_eq!(ch1["history"].as_array().unwrap().iter().map(|h| (h["amount"].clone(), h["adjustment"].clone())).collect::<Vec<_>>(),
        [(json!("0.01"), json!(null)), (json!("0.008"), json!("-0.002"))], "the correction appends an adjustment; history kept");
    assert_eq!(f.report()["metrics"]["M11"]["value"], "0.008");
    assert_eq!(f.cli_args(&["accounting", "charges", "--as-of", &first_import.to_string()]).0["charges"][0]["amount"], "0.01");

    // A record timed before revision 2 but observed after it.
    let late = session(5, "gpt-5.5", [1000, 0, 0, 500, 0], "resp-p5");
    sync(&f);
    f.cli_args(&["accounting", "reprice"]);
    let (restated, _) = f.cli_args(&["accounting", "cost", "--json"]);
    let has_late = |cost: &Value| cost["sessions"].as_array().unwrap().iter().any(|s| s["session_id"] == late.as_str());
    assert_eq!((&restated["revision"], has_late(&restated)), (&json!(3), true));
    assert!(restated["computed_unix_ms"].as_i64().unwrap() > computed2, "the restatement names its later cutoff");
    assert_eq!(attempt_cost(&restated, &f.attempt)["estimate"]["priced_amount"], "0.01", "0.005 + the late 0.005");
    let (then, _) = f.cli_args(&["accounting", "cost", "--json", "--as-of", &computed2.to_string()]);
    assert_eq!((&then["revision"], has_late(&then)), (&json!(2), false), "as known at revision 2, the late record is not in the view");
}

/// Doc 10 §5a "Quota and throttling": a window reports used 40 then 55 across
/// certified invocations → 15 units consumed (native percent), monetary
/// unknown: no amount is derived. A human-readable limit message with no
/// structured field is not a window observation; throttling stays uncertified.
#[test]
fn quota_window_consumption_is_native_units_only() {
    let f = Fixture::new();
    let d = f.decided;
    let resets = d / 1000 + 3_600;
    plant(&f.home, "quota", &Rollout::new(&sid(0x0801), &f.worktree(), d - 180_000)
        .rate(d - 120_000, 40.0, 300, resets)
        .line(json!({"timestamp": iso(d - 90_000), "type": "event_msg", "payload": {"type": "error", "message": "You've hit your usage limit. Try again later."}}))
        .rate(d - 60_000, 55.0, 300, resets).text());
    sync(&f);
    let quota = f.cli_args(&["accounting", "quota", "--json"]).0;
    let windows = quota["windows"].as_array().unwrap();
    assert_eq!(windows.len(), 1);
    let w = &windows[0];
    assert_eq!([&w["unit"], &w["first_used"], &w["used"], &w["observed_increase"], &w["remaining"], &w["observations"]],
        [&json!("percent"), &json!("40"), &json!("55"), &json!("15"), &json!("45"), &json!(2)], "55 − 40 = 15 percent consumed");
    assert!(w.as_object().unwrap().keys().all(|k| !k.contains("amount") && !k.contains("currency")), "no monetary value: {w}");
    assert_eq!(quota["observations"]["primary"], json!({"trusted": 2}), "the message line is no observation");
    assert_eq!(quota["metrics"]["M38"]["value"], unavailable("throttling_not_certified"));
    let decision = &quota["metrics"]["M40"]["decisions"][0]["windows"][0];
    assert_eq!((&decision["value"], &decision["age_ms"], &decision["freshness"]), (&json!("45"), &json!(60_000), &json!("fresh")));
}

// ---------------------------------------------------------------------------
// Doc 10 §3 allocation/terminal cohort, §5a coordinator overhead, §4 budget
// races and analytics rebuild, over several tasks.

/// Plant attempt `attempt` of a new task `task` (state `task_state`) in the
/// fixture's store as its launch would have left it: retained inputs and the
/// dispatch decision copied from the fixture's real attempt (same Codex
/// profile and execution home), decided at `decided`, with an active
/// collector binding. Fixture only: no launch happens.
fn plant_attempt(f: &Fixture, attempt: &str, task: &str, task_state: &str, attempt_state: &str, decided: i64) {
    let db = rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap();
    db.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    db.execute("INSERT INTO tasks(id,revision,state,title) VALUES(?1,1,?2,?1)", [task, task_state]).unwrap();
    db.execute("INSERT INTO attempts(id,task_id,revision,state,reservation,termination_observed) VALUES(?1,?2,2,?3,?1,?4)",
        rusqlite::params![attempt, task, attempt_state, i64::from(!["running", "reserved"].contains(&attempt_state))]).unwrap();
    db.execute("INSERT INTO attempt_inputs(attempt_id,operation_id,payload,payload_hash) SELECT ?1,?2,payload,payload_hash FROM attempt_inputs WHERE attempt_id=?3",
        rusqlite::params![attempt, format!("op-{attempt}"), f.attempt]).unwrap();
    db.execute("INSERT INTO dispatch_decisions(attempt_id,task_id,task_revision,contract_revision,classification_id,chosen_configuration_id,eligible,chooser_kind,
        chooser_principal,reason_codes,decided_unix_ms) SELECT ?1,?2,1,NULL,NULL,chosen_configuration_id,eligible,chooser_kind,chooser_principal,reason_codes,?3
        FROM dispatch_decisions WHERE attempt_id=?4", rusqlite::params![attempt, task, decided, f.attempt]).unwrap();
    db.execute("INSERT INTO collector_bindings(attempt_id,revision,state,collector,execution_home,unix_ms,source) VALUES(?1,1,'active','codex',?2,?3,'apply_launch_started')",
        rusqlite::params![attempt, f.home.display().to_string(), decided]).unwrap();
}

/// Contracts §6 acceptance evidence for a verify-only task: a verified result of its current contract.
fn accept(f: &Fixture, task: &str, attempt: &str, n: char) {
    let db = rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap();
    db.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    let hex = |c: char| c.to_string().repeat(64);
    db.execute("INSERT INTO task_contracts(task_id,contract_revision,plan_revision,project_store,expected_head,repository,base_oid,object_format,memory_snapshot_id,route,raw_bytes,raw_digest,installed_seq)
        VALUES(?1,1,NULL,'store',0,'/repo',?2,'sha1',NULL,'verify_only',x'61',?3,(SELECT max(sequence) FROM events))", rusqlite::params![task, "b".repeat(40), hex(n)]).unwrap();
    db.execute("INSERT INTO result_submissions(submission_id,project_store,idempotency_key,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,artifact_manifest,claimed_checks,created_unix_ms)
        VALUES(?1,'store',?1,?2,'{}',?3,1,?2,?4,'/repo',?5,?5,'sha1','[]','[]',1000)", rusqlite::params![hex(n), hex('d'), task, attempt, "b".repeat(40)]).unwrap();
    db.execute("INSERT INTO verified_results(result_id,run_id,submission_id,commit_oid,tree_oid,object_format,policy_digest,receipt_digest,isolation,memory_fence,created_unix_ms)
        VALUES(?1,?1,?2,?3,?3,'sha1',?4,?4,'linux-unshare-user-pid-mount-v1',0,2000)", rusqlite::params![hex('e'), hex(n), "b".repeat(40), hex('7')]).unwrap();
}

/// Keys that name when (or in which revision) a rebuilt sidecar computed a view, not what it holds.
const REBUILD_INSTANTS: &[&str] = &["computed_unix_ms", "ledger_synced_unix_ms", "imported_unix_ms", "revision", "valuation_revision"];

/// JSON with every key named in `drop` removed, recursively (instants of a rebuild).
fn without(value: &Value, drop: &[&str]) -> Value {
    match value {
        Value::Object(map) => Value::Object(map.iter().filter(|(k, _)| !drop.contains(&k.as_str())).map(|(k, v)| (k.clone(), without(v, drop))).collect()),
        Value::Array(items) => Value::Array(items.iter().map(|v| without(v, drop)).collect()),
        other => other.clone(),
    }
}

/// Doc 10 §3 `terminal_cohort`: terminal costs $4 succeeded + $3 failed + $1
/// cancelled with one accepted task → M04 = $8 per accepted task; an open
/// task costing $5 does not change it (and is counted as excluded). §5a
/// "Coordinator overhead": coordinator exclusive $4 against workers $16
/// (the open task's second session of $3 makes 4 + 3 + 1 + 5 + 3) → M34 =
/// 4/20 = 20%; unknown coordinator usage → partial. Rates are invented:
/// $2000 per 10^6 input, $4000 per 10^6 output (1,000 input = $2).
/// §4 budget: two in-flight admissions against a shadow budget of $30 (never
/// enforced, `state.db` never written); an admitted attempt with no known
/// size is unknown exposure. §4 rebuild: the sidecar deleted and rebuilt
/// from the same sources gives the same derived views, the same consumption
/// (no double spend, no refund) and the same metrics.
#[test]
fn terminal_cohort_coordinator_budget_race_and_rebuild() {
    let f = Fixture::new();
    let t = f.decided + 1_000;
    let worktree = |attempt: &str| format!("{}/.state/worktrees/{attempt}/repo-00", f.project.display());
    let spend = |name: &str, attempt: &str, n: u32, usage: Counts| {
        plant(&f.home, name, &Rollout::new(&sid(0x0900 + n), &worktree(attempt), t).model(t, "turn-1", "gpt-5.5").usage(t + 1, "turn-1", "resp-1", usage, None).text());
    };
    for (attempt, task, task_state, attempt_state) in [("c-ok", "t-ok", "succeeded", "completed"), ("c-fail", "t-fail", "failed", "failed"),
        ("c-cancel", "t-cancel", "cancelled", "cancelled"), ("c-open", "t-open", "running", "running")] {
        plant_attempt(&f, attempt, task, task_state, attempt_state, f.decided);
    }
    accept(&f, "t-ok", "c-ok", '1');
    spend("ok", "c-ok", 1, [1000, 0, 0, 500, 0]); // $2 + $2 = $4
    spend("fail", "c-fail", 2, [500, 0, 0, 500, 0]); // $1 + $2 = $3
    spend("cancel", "c-cancel", 3, [500, 0, 0, 0, 0]); // $1
    spend("open", "c-open", 4, [1500, 0, 0, 500, 0]); // $3 + $2 = $5
    sync(&f);
    f.cli_args(&["accounting", "import-rate-card", &card(&f, "cohort", 1, "USD", &["gpt-5.5"], &[("input", "2000"), ("output", "4000")])]);
    f.cli_args(&["accounting", "reprice"]);
    let report = f.report();
    let m04 = &report["metrics"]["M04"];
    assert_eq!((&m04["value"], &m04["currency"], &m04["denominator"], &m04["numerator"]),
        (&json!("8/1"), &json!("USD"), &json!(1), &json!({"status": "complete", "currency": "USD", "amount": "8"})), "($4 + $3 + $1) / 1 accepted: {m04}");
    assert_eq!(m04["tasks"], json!({"terminal": 3, "accepted": 1, "open_excluded": 2}), "t-open and the fixture's queued task stay apart");
    assert_eq!((&report["metrics"]["M02"]["value"], &report["metrics"]["M07"]["value"]), (&json!("1/3"), &json!("3/1")));

    // The coordinator ($4, Codex at the project directory from another scanned home) and the open task's second session ($3).
    let other_home = f.home.parent().unwrap().join("other-home");
    plant(&other_home, "coordinator", &Rollout::new(&sid(0x0990), &f.project.display().to_string(), t).model(t, "turn-1", "gpt-5.5")
        .usage(t + 1, "turn-1", "resp-1", [1000, 0, 0, 500, 0], None).text());
    spend("open-2", "c-open", 5, [500, 0, 0, 500, 0]);
    sync(&f);
    f.cli_args(&["accounting", "reprice"]);
    let report = f.report();
    assert_eq!(report["metrics"]["M34"]["value"], "1/5", "4 / (4 + 16)");
    assert_eq!(report["metrics"]["M04"]["value"], "8/1", "the open task's extra spend changes nothing");

    // Budget: $30; accepted 4 + 3 + 1 + 8 = 16 across every attempt; c-open reserved $10 (8 used → 2 left).
    let policy = |name: &str, reservations: Value, request: &str| {
        let path = f.tmp.path().join(name);
        fs::write(&path, json!({"synthetic": true, "policy_id": "cert-what-if", "version": 1, "currency": "USD", "unknown_usage": "refuse",
            "source": "INVENTED what-if limits for TM2.6 certification; never installed", "project": {"max_amount": "30"},
            "reservations": reservations, "request": {"amount": request}}).to_string()).unwrap();
        path.display().to_string()
    };
    let state = f.project.join(".state/state.db");
    let before = fs::read(&state).unwrap();
    let project_eval = |view: &Value| view["evaluations"].as_array().unwrap().iter()
        .find(|e| e["policy_source"] == "what_if" && e["scope"] == "project" && e["dimension"] == "amount").cloned().unwrap();
    let decide = |file: &str| {
        let view = f.cli_args(&["accounting", "budget-shadow", "--policy", file]).0;
        assert_eq!((&view["mode"], &view["enforcement"]), (&json!("shadow"), &json!("none")));
        let e = project_eval(&view);
        ["accepted", "remaining_reserved", "exposure", "projected", "decision", "reason"].map(|k| e[k].clone())
    };
    let both = json!({"c-open": {"amount": "10"}, f.attempt.as_str(): {"amount": "10"}});
    // The second admission's $10 reservation is in flight too: exposure 16 + 2 + 10 = 28.
    assert_eq!(decide(&policy("fits.json", both.clone(), "2")), [json!("16"), json!("12"), json!("28"), json!("30"), json!("allow"), json!("within_limit")]);
    assert_eq!(decide(&policy("over.json", both, "3")), [json!("16"), json!("12"), json!("28"), json!("31"), json!("would_block"), json!("projected_exposure_exceeds_limit")],
        "a third request cannot reuse headroom the in-flight admissions hold");
    let unknown = decide(&policy("unsized.json", json!({"c-open": {"amount": "10"}}), "2"));
    assert_eq!((&unknown[4], &unknown[5]), (&json!("would_block"), &json!("provider_usage_unavailable")), "an admitted attempt without a size is unknown, never 0");
    assert_eq!(fs::read(&state).unwrap(), before, "shadow only: state.db unchanged");

    // Rebuild: delete the sidecar, collect, sync, re-import the same card and reprice.
    let views = |f: &Fixture| {
        let budget = f.cli_args(&["accounting", "budget-shadow", "--policy", &policy("fits.json", json!({"c-open": {"amount": "10"}, f.attempt.as_str(): {"amount": "10"}}), "2")]).0;
        let report = f.report();
        (f.cli_args(&["accounting", "entries"]).1, f.cli_args(&["accounting", "sessions"]).1, f.cli_args(&["usage", "--json"]).1,
         without(&f.cli_args(&["accounting", "cost", "--json"]).0, REBUILD_INSTANTS), project_eval(&budget),
         ["M04", "M08", "M09", "M12", "M14", "M34", "M35", "M37"].map(|id| without(&report["metrics"][id], REBUILD_INSTANTS)))
    };
    let original = views(&f);
    for name in ["telemetry.db", "telemetry.db-wal", "telemetry.db-shm"] { let _ = fs::remove_file(f.project.join(".state").join(name)); }
    sync(&f);
    f.cli_args(&["accounting", "import-rate-card", &card(&f, "cohort", 1, "USD", &["gpt-5.5"], &[("input", "2000"), ("output", "4000")])]);
    f.cli_args(&["accounting", "reprice"]);
    let rebuilt = views(&f);
    assert_eq!((String::from_utf8_lossy(&rebuilt.0), String::from_utf8_lossy(&rebuilt.1), String::from_utf8_lossy(&rebuilt.2)),
        (String::from_utf8_lossy(&original.0), String::from_utf8_lossy(&original.1), String::from_utf8_lossy(&original.2)), "ledger, graph and usage byte-identical");
    assert_eq!(rebuilt.3, original.3, "the same estimates");
    assert_eq!(rebuilt.4, original.4, "the same consumption: nothing spent twice, nothing refunded");
    assert_eq!(rebuilt.5, original.5);
    // The coordinator's record (t + 1) is in c-open's unknown activity span: c-open
    // predates the lifecycle log and has no terminal mark, so under rule v2 the span
    // is open-ended whatever the read time (v1 ended it at "now", and a read before
    // t + 1 made the same $4 unallocated).
    let allocation = &original.5[5]["allocation"];
    assert_eq!((&allocation["rule"], &allocation["unallocated"], &allocation["allocation_unknown"], &allocation["allocation_unknown_entries"]),
        (&json!("coordinator-allocation-v2"), &json!("0"), &json!("4"), &json!({"activity_unknown": 1})));
    assert_eq!(fs::read(&state).unwrap(), before, "no canonical write by a rebuild");

    // Unknown coordinator usage (a model without a rate) → M34 partial, never a ratio of the priced part.
    plant(&other_home, "coordinator-2", &Rollout::new(&sid(0x0991), &f.project.display().to_string(), t).model(t, "turn-1", "gpt-5.5-mini")
        .usage(t + 1, "turn-1", "resp-1", [10, 0, 0, 5, 0], None).text());
    sync(&f);
    f.cli_args(&["accounting", "reprice"]);
    assert_eq!(f.report()["metrics"]["M34"]["value"]["status"], "partial");
}

// ---------------------------------------------------------------------------
// Doc 10 §4 faults: killed collectors, partial lines, outages during active
// work, lost final events, and the sidecar rebuilt from its sources.

fn final_event(f: &Fixture, sid: &str) -> Value {
    let sessions = f.cli_args(&["collectors", "sessions"]).0;
    sessions["sessions"].as_array().unwrap().iter().find(|s| s["session_id"] == sid).unwrap_or_else(|| panic!("{sid} in {sessions}"))["final_event"].clone()
}

/// Collectors killed mid-pass (SIGKILL at growing delays over six rollouts of
/// 400 records) and then resumed give byte-identical usage, ledger and graph to a
/// sidecar rebuilt from scratch: each rollout commits in one transaction with
/// its cursor, so a killed pass leaves either nothing or a whole file. A
/// partial last line (writer mid-write) is left for the next pass, never
/// guessed. Work continuing during a collector outage is collected once the
/// collector returns; a turn idle past the threshold without its final event
/// is an explicit coverage gap (`missing`) that recovers when the event
/// arrives, and never changes a sum.
#[test]
fn killed_collectors_partial_lines_and_outages_replay_identically() {
    let f = Fixture::new();
    let t = f.decided + 1_000;
    let (partial, outage) = (sid(0x0a02), sid(0x0a03));
    // Six rollouts of 400 records (10/2 each): each commits whole with its cursor.
    for file in 0..6u32 {
        let mut rollout = Rollout::new(&sid(0x0a10 + file), &f.worktree(), t).model(t, "turn-1", "gpt-5.5");
        for n in 0..400 { rollout = rollout.usage(t + 1, "turn-1", &format!("resp-{n}"), [10, 0, 0, 2, 0], Some([10 * (n + 1), 0, 0, 2 * (n + 1), 0])); }
        plant(&f.home, &format!("big-{file}"), &rollout.done(t + 2, "turn-1").text());
    }
    // A writer mid-line: the second record is cut before its newline.
    let whole = Rollout::new(&partial, &f.worktree(), t).model(t, "turn-1", "gpt-5.5")
        .usage(t + 1, "turn-1", "resp-1", [7, 0, 0, 3, 0], None).usage(t + 2, "turn-1", "resp-2", [5, 0, 0, 1, 0], None).text();
    let cut = whole.len() - 40;
    let partial_path = plant(&f.home, "partial", &whole[..cut]);
    // Active work whose collector goes away: one record, then idle for 20 minutes without the turn's final event.
    let active = Rollout::new(&outage, &f.worktree(), t).model(t, "turn-1", "gpt-5.5").usage(t + 1, "turn-1", "resp-1", [100, 0, 0, 20, 0], None);
    let outage_path = plant(&f.home, "outage", &active.text());
    let idle = |path: &Path| fs::File::options().write(true).open(path).unwrap().set_modified(std::time::SystemTime::now() - Duration::from_secs(1_200)).unwrap();
    idle(&outage_path);

    // Kill collectors after growing delays until one pass has run to completion; the
    // committed records must always be whole files, and some kill must land mid-pass.
    let committed = |f: &Fixture| if f.project.join(".state/telemetry.db").exists() {
        f.sidecar().query_row("SELECT count(*) FROM codex_usage WHERE session_id LIKE '%0a1_'", [], |r| r.get::<_, i64>(0)).unwrap_or(0) } else { 0 };
    let (mut delay, mut interrupted) = (20, false);
    loop {
        let mut child = Command::new(BIN).env_clear().env("HOME", f.tmp.path().join("home")).env("PATH", "/usr/bin:/bin")
            .args(["--root", f.root.to_str().unwrap(), "telemetry", "demo", "collect"]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
        std::thread::sleep(Duration::from_millis(delay));
        let killed = child.try_wait().unwrap().is_none();
        let _ = child.kill();
        child.wait().unwrap();
        let rows = committed(&f);
        assert_eq!(rows % 400, 0, "whole rollouts only: {rows}");
        interrupted |= killed && (1..2_400).contains(&rows);
        if !killed { break; }
        delay = delay * 3 / 2;
    }
    assert!(interrupted, "a kill landed between the first and the last rollout's commit");
    sync(&f);
    let report = f.report();
    // 2,400 × 10/2, the whole first partial record (7/3), the outage's 100/20.
    assert_eq!((&report["metrics"]["M08"]["value"], &report["metrics"]["M09"]["value"]), (&json!(24_000 + 7 + 100), &json!(4_800 + 3 + 20)));
    assert_eq!(final_event(&f, &outage), json!({"state": "missing", "turn_id": "turn-1"}), "an explicit gap, not a silent stop");
    assert_eq!(f.count("codex_quarantine") + f.count("codex_discrepancy"), 0);

    // The writer finishes its line; the worker resumes after the outage and completes its turn.
    let mut file = fs::OpenOptions::new().append(true).open(&partial_path).unwrap();
    std::io::Write::write_all(&mut file, &whole.as_bytes()[cut..]).unwrap();
    fs::write(&outage_path, active.clone().usage(t + 3, "turn-1", "resp-2", [50, 0, 0, 10, 0], None).done(t + 4, "turn-1").text()).unwrap();
    sync(&f);
    let report = f.report();
    assert_eq!((&report["metrics"]["M08"]["value"], &report["metrics"]["M09"]["value"]), (&json!(24_000 + 12 + 150), &json!(4_800 + 4 + 30)));
    assert_eq!(final_event(&f, &outage)["state"], "complete");
    let recovery: String = f.sidecar().query_row("SELECT recovery FROM coverage_gaps WHERE reason='final_event_missing'", [], |r| r.get(0)).unwrap();
    assert_eq!(recovery, "recovered");
    let views = |f: &Fixture| (f.cli_args(&["usage", "--json"]).1, f.cli_args(&["accounting", "entries"]).1, f.cli_args(&["accounting", "sessions"]).1,
        without(&f.report()["metrics"], &[]));
    let resumed = views(&f);

    // The sidecar deleted and rebuilt from the rollouts: identical derived views.
    for name in ["telemetry.db", "telemetry.db-wal", "telemetry.db-shm"] { let _ = fs::remove_file(f.project.join(".state").join(name)); }
    sync(&f);
    let rebuilt = views(&f);
    assert_eq!(String::from_utf8_lossy(&rebuilt.0), String::from_utf8_lossy(&resumed.0), "usage");
    assert_eq!(String::from_utf8_lossy(&rebuilt.1), String::from_utf8_lossy(&resumed.1), "ledger");
    assert_eq!(String::from_utf8_lossy(&rebuilt.2), String::from_utf8_lossy(&resumed.2), "graph");
    assert_eq!(rebuilt.3, resumed.3, "report metrics");
}

/// Doc 10 §4 "Two collectors racing": the very first collects of a project
/// race to create the sidecar; none fails and each record is accepted once.
#[test]
fn first_collectors_racing_to_create_the_sidecar() {
    let f = Fixture::new();
    let t = f.decided + 1_000;
    plant(&f.home, "a", &Rollout::new(&sid(0x0b01), &f.worktree(), t).model(t, "turn-1", "gpt-5.5")
        .usage(t + 1, "turn-1", "resp-1", [100, 0, 0, 20, 0], None).text());
    assert_eq!(race(&f, &["collect"], 4), Vec::<String>::new());
    sync(&f);
    assert_eq!(m08_m09(&f), (json!(100), json!(20)));
}

// ---------------------------------------------------------------------------
// Doc 10 §5a candidate groups (cost ownership) and the adapter certificate.

/// Doc 10 §5a "Candidate group": arms A, B, C on one task; A verified and
/// selected, B verified but not selected, C produced no candidate. The
/// group's lifecycle cost includes all three arms (C is a failure, not
/// missing); the winner's usage is a drill-down, never the group's cost.
/// Arms here are three Codex configurations (arguments differ) launched as
/// sequential arms (phase2-lanes owner decision 1). Group cost is in native
/// tokens: no priced group figure exists (certificate: restricted).
#[test]
fn candidate_group_cost_includes_every_arm() {
    let f = Fixture::new();
    let db_path = f.project.join(".state/state.db");
    let homes = [f.home.clone(), f.tmp.path().join("fast-home"), f.tmp.path().join("slow-home")];
    for (name, home, args) in [("fast", &homes[1], '1'), ("slow", &homes[2], '2')] {
        let mut profile = codex_profile(&f.config, "codex", name, Some(home));
        profile.arguments_digest = args.to_string().repeat(64);
        plant_profile(&db_path, profile);
    }
    {
        let mut db = herdr_farm::store::SqliteStore::open(&db_path).unwrap();
        let snapshot = db.read_snapshot(None).unwrap();
        db.set_scheduler_policy(snapshot.head, snapshot.scheduler.unwrap().policy.revision, 1, 5).unwrap();
    }
    let group = f.cli_args(&["quality", "groups", "create", "work", "--arm", "codex", "--arm", "fast", "--arm", "slow"]).0["group"]["group_id"].as_str().unwrap().to_owned();
    let sql = || rusqlite::Connection::open(&db_path).unwrap();
    let mut arms = Vec::new();
    for profile in ["codex", "fast", "slow"] {
        f.readmit(profile);
        arms.push(sql().query_row("SELECT attempt_id,decided_unix_ms FROM dispatch_decisions ORDER BY rowid DESC LIMIT 1", [], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))).unwrap());
    }
    f.cancel_reserved(); // C ends without a candidate.
    // A 1000/500, B 600/200, C 300/100 in their own homes and worktrees.
    for (n, ((attempt, decided), usage)) in arms.iter().zip([[1000, 0, 0, 500, 0], [600, 0, 0, 200, 0], [300, 0, 0, 100, 0]]).enumerate() {
        sql().execute("INSERT INTO collector_bindings(attempt_id,revision,state,collector,execution_home,unix_ms,source) VALUES(?1,1,'active','codex',?2,?3,'apply_launch_started')",
            rusqlite::params![attempt, homes[n].display().to_string(), decided]).unwrap();
        let cwd = format!("{}/.state/worktrees/{attempt}/repo-00", f.project.display());
        plant(&homes[n], &format!("arm-{n}"), &Rollout::new(&sid(0x0c01 + n as u32), &cwd, decided + 1_000).model(decided + 1_000, "turn-1", "gpt-5.5")
            .usage(decided + 1_001, "turn-1", "resp-1", usage, None).text());
    }
    // A's and B's candidates both pass the pinned CI (verification accepted).
    let db = sql();
    db.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    let hex = |c: char| c.to_string().repeat(64);
    db.execute("INSERT INTO task_contracts(task_id,contract_revision,plan_revision,project_store,expected_head,repository,base_oid,object_format,memory_snapshot_id,route,raw_bytes,raw_digest,installed_seq)
        VALUES('work',1,NULL,'store',0,'/repo',?1,'sha1',NULL,'verify_only',x'61',?2,(SELECT max(sequence) FROM events))", rusqlite::params!["b".repeat(40), hex('c')]).unwrap();
    db.execute("INSERT INTO acceptance_policies(task_id,contract_revision,policy_id,body) VALUES('work',1,'ci','cargo test')", []).unwrap();
    for (sub, run, (attempt, _), oid) in [('1', 'a', &arms[0], "1".repeat(40)), ('2', 'b', &arms[1], "2".repeat(40))] {
        db.execute("INSERT INTO result_submissions(submission_id,project_store,idempotency_key,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,artifact_manifest,claimed_checks,created_unix_ms)
            VALUES(?1,'store',?1,?2,'{}','work',1,?2,?3,'/repo',?4,?5,'sha1','[]','[]',1000)", rusqlite::params![hex(sub), hex('d'), attempt, "b".repeat(40), oid]).unwrap();
        db.execute("INSERT INTO verification_runs(run_id,project_store,idempotency_key,payload_digest,submission_id,task_id,contract_revision,contract_digest,attempt_id,policy_id,policy_digest,
            commit_oid,tree_oid,object_format,memory_fence,isolation,argv,library_manifest,state,reason,exit_status,receipt_digest,store_device,store_inode,created_unix_ms)
            VALUES(?1,'store',?1,?2,?3,'work',1,?2,?4,'ci',?2,?5,?5,'sha1',0,'linux-unshare-user-pid-mount-v1','[\"x\"]','[]','accepted',NULL,0,?6,1,1,3000)",
            rusqlite::params![hex(run), hex('d'), hex(sub), attempt, oid, hex('8')]).unwrap();
    }
    drop(db);
    f.cli("collect");
    f.cli_args(&["quality", "groups", "select", &group, "--arm", "1", "--reason", "operator_judgment"]);
    let shown = f.cli_args(&["quality", "groups", "show"]).0["groups"][0].clone();
    let outcomes: Vec<_> = shown["arms"].as_array().unwrap().iter().map(|a| (a["role"].clone(), a["outcome"].clone())).collect();
    assert_eq!(outcomes, [(json!("selected"), json!("candidate")), (json!("not_selected"), json!("candidate")), (json!("not_selected"), json!("failure_no_candidate"))],
        "C counts as a failure, not missing");
    let cost = &shown["cost"];
    assert_eq!((&cost["arms_launched"], &cost["arms_not_launched"]), (&json!(3), &json!(0)));
    assert_eq!((&cost["arms_total"]["input_tokens"], &cost["arms_total"]["output_tokens"], &cost["arms_total"]["records"]), (&json!(1900), &json!(800), &json!(3)),
        "1000 + 600 + 300 input, 500 + 200 + 100 output: every arm's cost, the losers' and the failed one's included");
    assert_eq!((&cost["winner_usage"]["input_tokens"], &cost["winner_usage"]["output_tokens"]), (&json!(1000), &json!(500)), "the winner's own usage only");
}

/// Adapter certificate (contracts-collection.md A3, codex-live-0.154.0*.md):
/// every source field the accounting families read is collected, in the unit
/// the ledger assumes, and certified live, or its limitation is declared.
/// Codex is the only live-certified collector. Since the 2026-09-30 owner
/// decision (harness coverage, phase2-lanes.md DG4) the OTLP receiver adds
/// `otlp:<harness>` adapters and DG4b the native `claude-code` adapter, but
/// those are fixture-certified at most; Cursor and Muse native reviewed surfaces are
/// unavailable (`none`). Only adapters with a recorded live report may claim a `live` field. An uncertified Codex version is
/// never summed.
#[test]
fn accounting_fields_match_the_adapter_certificate() {
    let f = Fixture::new();
    let capabilities = f.cli_args(&["collectors", "capabilities", "--json"]).0;
    let adapters = capabilities["adapters"].as_array().unwrap();
    let names: Vec<&str> = adapters.iter().map(|a| a["adapter"].as_str().unwrap()).collect();
    assert_eq!(names[0], "codex", "{names:?}");
    assert!(names[1..].iter().all(|n| *n == "claude-code" || *n == "gemini-cli" || *n == "opencode" || *n == "cursor-agent" || *n == "muse" || *n == "devin" || n.starts_with("otlp:")), "only declared adapters besides Codex: {names:?}");
    assert!(names.contains(&"otlp:grok"), "Grok fixture adapter must be advertised: {names:?}");
    assert!(names.contains(&"otlp:devin") && names.contains(&"devin"), "Devin adapters must be advertised: {names:?}");
    // Recorded live evidence registry. Adding an adapter/version requires a
    // reviewed report, not merely a successful invocation of the live test.
    let recorded_live = [("codex", "0.154.0", "docs/telemetry/certificate-live.md"),
        ("codex", "0.159.2", "docs/telemetry/codex-live-0.159.2.md"),
        ("claude-code", "2.1.286", "docs/telemetry/claude-live-2.1.286.md"),
        ("otlp:grok", "1.0.46", "docs/telemetry/grok-live-1.0.46.md"),
        ("otlp:devin", "3000.11.3", "docs/telemetry/devin-live-3000.11.3.md"),
        ("muse", "1.4.0-R4161.1", "docs/telemetry/muse-live-1.4.0.md")];
    for adapter in adapters {
        let claims_live = adapter["fields"].as_array().into_iter().flatten().any(|f| f["certified"] == "live");
        if claims_live {
            let name = adapter["adapter"].as_str().unwrap();
            let versions = adapter["certified_versions"].as_array().unwrap();
            assert!(!versions.is_empty(), "live fields need a recorded version");
            for version in versions {
                assert!(recorded_live.iter().any(|(a, v, report)| *a == name && Some(*v) == version.as_str() && !report.is_empty()),
                    "{name} {version} needs a recorded live report before claiming live");
            }
        }
    }
    for (name, reason) in [
        ("cursor-agent", "stable_local_usage_format_not_evident"),
        ("otlp:cursor-agent", "protobuf_traces_only_no_usable_logs_or_metrics"),
        ("devin", "local_usage_schema_not_established"),
    ] {
        let adapter = adapters.iter().find(|a| a["adapter"] == name).unwrap();
        assert_eq!(adapter["interface"], "none");
        assert_eq!(adapter["certified_versions"], json!([]));
        let fields = adapter["fields"].as_array().unwrap();
        assert_eq!(fields.iter().map(|f| f["field"].as_str().unwrap()).collect::<Vec<_>>(),
            vec!["input_tokens", "output_tokens", "cost", "tool_calls", "errors"]);
        for field in fields {
            assert_eq!(field["available"], false);
            assert_eq!(field["certified"], "none");
            assert_eq!(field["reason"], reason);
        }
    }
    let codex = &adapters[0];
    assert_eq!(codex["certified_versions"], json!(["0.154.0", "0.159.2"]));
    let field = |kind: &str, name: &str| codex["fields"].as_array().unwrap().iter().find(|x| x["kind"] == kind && x["field"] == name).cloned()
        .unwrap_or_else(|| panic!("{kind}.{name}"));
    let facts = |kind: &str, name: &str| { let x = field(kind, name); (x["available"].clone(), x["basis"].clone(), x["certified"].clone(), x["caveat"].clone()) };
    // Counted tokens (M08/M09, the ledger, estimates): per-response deltas, reported, live.
    for name in ["input_tokens", "cached_input_tokens", "output_tokens", "reasoning_output_tokens", "total_tokens"] {
        assert_eq!(facts("token_usage_record", &format!("usage.{name}")), (json!(true), json!("reported"), json!("live"), json!(null)), "{name}");
        assert_eq!(field("token_usage_record", &format!("thread_token_usage.{name}"))["caveat"], "reconciliation_only", "{name}: never summed");
    }
    assert_eq!(facts("token_usage_record", "usage.cache_write_input_tokens").3, json!("overlap_with_input_not_certified"), "why cache writes stay unpriced");
    assert_eq!(field("token_usage_record", "response_id")["certified"], "live");
    // Model segments and the rate card's provider check; the usage interval from line times.
    assert_eq!((field("turn_context", "model")["certified"].clone(), field("session_meta", "model_provider")["certified"].clone(), field("line", "timestamp")["certified"].clone()),
        (json!("live"), json!("live"), json!("live")));
    // Quota: primary windows live (semantics not certified), secondary and the reached type fixture only.
    for name in ["rate_limits.primary.used_percent", "rate_limits.primary.window_minutes", "rate_limits.primary.resets_at"] {
        assert_eq!(facts("token_count", name).2, json!("live"), "{name}");
        assert_eq!(facts("token_count", name).3, json!("semantics_not_certified"), "{name}");
    }
    for name in ["rate_limits.secondary.used_percent", "rate_limits.rate_limit_reached_type"] { assert_eq!(field("token_count", name)["certified"], "fixture", "{name}"); }

    // An uncertified version is stored without counters and never summed.
    let t = f.decided + 1_000;
    let mut rollout = Rollout::new(&sid(0x0d01), &f.worktree(), t).model(t, "turn-1", "gpt-5.5").usage(t + 1, "turn-1", "resp-1", [100, 0, 0, 20, 0], None).text();
    rollout = rollout.replace("\"cli_version\":\"0.154.0\"", "\"cli_version\":\"0.999.0\"");
    plant(&f.home, "future", &rollout);
    sync(&f);
    let report = f.report();
    assert_eq!(report["metrics"]["M08"]["value"], unavailable("no_certified_source"));
    assert_eq!(report["metrics"]["M08"]["coverage"]["excluded"], json!({"cli_version_uncertified": 1}));
}

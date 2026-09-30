//! Lane A ingest ledger end to end (docs/telemetry/contracts-collection.md,
//! card A2): Codex rollouts collected on the CLI into sanitized source
//! envelopes, replayed after a killed collect, with oversized lines quarantined
//! and an unwritable sidecar recorded as a coverage gap. A7: a rollout idle
//! without its last turn's final event is a coverage gap until the event comes.

#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.

mod support;

use sha2::{Digest, Sha256};
use std::{fs, os::unix::process::CommandExt, path::Path, process::Command};
use support::telemetry::*;

/// sha256 of the sanitized canonical payloads of `head.jsonl` lines 5 and 6,
/// computed with `sha256sum` outside the crate. Line 6 (`token_count`) carries
/// the A4 fields `rate_limits.{secondary.*, rate_limit_reached_type}` as `null`.
const USAGE_DIGEST: &str = "sha256:9768f3bc423b89bc5cf97404ab8900514dc7c98f82e53f06c13aa46fcd1986f8";
const TOKEN_COUNT_DIGEST: &str = "sha256:d6859cca51308b5f0006f990de8ff2f82d23ebe519be5ec4ef1a4da32384d654";

fn digest(path: &Path) -> String { format!("sha256:{:x}", Sha256::digest(path.as_os_str().as_encoded_bytes())) }

fn tail(f: &Fixture, repeats: usize) -> String {
    let ts = jiff::Timestamp::from_millisecond(f.decided + 1_000).unwrap().to_string();
    fs::read_to_string(Path::new(FIXTURES).join("tail.jsonl")).unwrap().replace("@CWD@", &f.worktree()).replace("@TS@", &ts).repeat(repeats)
}

fn append(path: &Path, text: &str) {
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    std::io::Write::write_all(&mut file, text.as_bytes()).unwrap();
}

/// Every ingest ledger and Codex row except receipt times, in key order.
fn ledger(f: &Fixture) -> Vec<String> {
    let db = f.sidecar();
    let mut out = Vec::new();
    for table in ["source_observations", "ingest_quarantine", "coverage_gaps", "source_cursors", "codex_usage", "codex_turns", "codex_rate_limits",
        "codex_quarantine", "codex_discrepancy", "collect_offsets", "rollout_sources", "rollout_metadata", "codex_usage_times", "codex_rate_limit_windows",
        "rollout_threads", "rollout_subagents", "rollout_ingest_state"] {
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

fn collect_command(f: &Fixture) -> Command {
    let mut command = Command::new(BIN);
    command.env_clear().env("HOME", f.tmp.path().join("home")).env("PATH", "/usr/bin:/bin")
        .args(["--root", f.root.to_str().unwrap(), "telemetry", "demo", "collect"]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    command
}

/// A collect killed with SIGKILL mid-run leaves only whole committed rollouts;
/// the next collect completes it, and the result equals an uninterrupted
/// collect row for row. Each allowlisted record is one sanitized envelope.
#[test]
fn envelopes_replay_identically_after_interrupted_collect() {
    const FILES: usize = 6;
    const REPEATS: usize = 400;
    let f = Fixture::new();
    let mut paths = Vec::new();
    for n in 0..FILES {
        let sid = if n == 0 { SID.to_owned() } else { format!("00000000-0000-4000-8000-{n:012}") };
        let path = f.rollout(&f.home, &format!("big-{n:02}"), &["head.jsonl"], &f.worktree(), f.decided + 1_000, "0.154.0");
        fs::write(&path, fs::read_to_string(&path).unwrap().replace(SID, &sid) + &tail(&f, REPEATS).replace("@SID@", &sid)).unwrap();
        paths.push(path);
    }
    let mut child = collect_command(&f).spawn().unwrap();
    let sidecar = f.project.join(".state/telemetry.db");
    let committed = || rusqlite::Connection::open_with_flags(&sidecar, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()
        .and_then(|db| db.query_row("SELECT count(*) FROM source_cursors", [], |r| r.get::<_, i64>(0)).ok()).unwrap_or(0);
    while committed() == 0 {
        assert!(child.try_wait().unwrap().is_none(), "collect finished before it could be interrupted");
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(child.try_wait().unwrap().is_none(), "collect finished before it could be interrupted");
    child.kill().unwrap();
    child.wait().unwrap();
    let per_file = 6 + 4 * REPEATS as i64;
    let partial = f.count("source_observations");
    assert!(partial > 0 && partial < per_file * FILES as i64, "interrupted after {partial} observations");
    assert_eq!(partial % per_file, 0, "only whole rollouts are committed");

    f.cli("collect");
    assert_eq!((f.count("source_observations"), f.count("codex_usage"), f.count("source_cursors")),
        (per_file * FILES as i64, (1 + REPEATS as i64) * FILES as i64, FILES as i64));
    assert_eq!((f.count("ingest_quarantine"), f.count("coverage_gaps")), (0, 0));
    let resumed = ledger(&f);
    remove_sidecar(&f);
    f.cli("collect");
    assert!(resumed == ledger(&f), "an interrupted then resumed collect equals an uninterrupted one");

    // The envelope of `head.jsonl` line 5 (usage) and line 6 (token_count) of the first rollout.
    let text = fs::read_to_string(&paths[0]).unwrap();
    let starts: Vec<usize> = std::iter::once(0).chain(text.match_indices('\n').map(|(i, _)| i + 1)).collect();
    let source = digest(&paths[0]);
    let sid = SID;
    let envelope = |line: usize| f.sidecar().query_row("SELECT event_id,producer_id,producer_epoch,event_kind,identity,provenance,measurement,payload_digest
        FROM source_observations WHERE producer_epoch=?1 AND producer_sequence=?2", rusqlite::params![source, starts[line - 1] as i64],
        |r| (0..8).map(|i| r.get::<_, String>(i)).collect::<rusqlite::Result<Vec<_>>>()).unwrap();
    let provenance = r#"{"adapter":"codex","adapter_version":"0.154.0","interface":"rollout_jsonl","source_trust":"collector_observed"}"#;
    // `token_count` widened its allowlist in A4: normalization version 2. A7:
    // `certified` says whether the adapter version was certified when read.
    let measurement = |version: i64| format!(r#"{{"certified":true,"coverage":"complete","measurement_basis":"reported","normalization_version":{version}}}"#);
    let identity = format!(r#"{{"session_id":"{sid}"}}"#);
    for (line, kind, version, payload_digest) in [(5, "codex.token_usage_record.v1", 1, USAGE_DIGEST), (6, "codex.token_count.v1", 2, TOKEN_COUNT_DIGEST)] {
        assert_eq!(envelope(line), [format!("codex:{source}:{}", starts[line - 1]), format!("codex:{sid}"), source.clone(), kind.into(),
            identity.clone(), provenance.into(), measurement(version), payload_digest.into()]);
    }
    // Line 4 (`response_item`) is not allowlisted: no envelope.
    assert_eq!(f.sidecar().query_row("SELECT count(*) FROM source_observations WHERE producer_epoch=?1 AND producer_sequence=?2",
        rusqlite::params![source, starts[3] as i64], |r| r.get::<_, i64>(0)).unwrap(), 0);
    assert_eq!(f.sidecar().query_row("SELECT byte_offset,observations FROM source_cursors WHERE source=?1", [&source],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))).unwrap(), (text.len() as i64, per_file));
}

/// A line over the 16 MiB parse limit is skipped whole, as before, and leaves a
/// quarantine row with its position, size and reason; the rest still ingests.
#[test]
fn oversized_line_is_quarantined_with_reason() {
    let f = Fixture::new();
    let path = f.rollout(&f.home, SID, &["head.jsonl"], &f.worktree(), f.decided + 1_000, "0.154.0");
    // Allowlisted text passes the contracts §7 excerpt rules.
    append(&path, "{\"type\":\"turn_context\",\"payload\":{\"turn_id\":\"turn-x\",\"effort\":\"Bearer abc\",\"secret\":\"CANARY\",
        \"model\":\"/home/alice/m\\tsk-live1234 https://h.example/p?q=1 second\\nline\"}}\n".replace("\n        ", "").as_str());
    let offset = fs::metadata(&path).unwrap().len() as i64;
    let big = format!("{{\"type\":\"response_item\",\"payload\":{{\"text\":\"{}\"}}}}\n", "x".repeat(17 << 20));
    append(&path, &big);
    append(&path, &tail(&f, 1).replace("@SID@", SID));
    for _ in 0..2 {
        f.cli("collect");
        let rows = f.sidecar().prepare("SELECT source,sequence,reason,bytes FROM ingest_quarantine").unwrap()
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?))).unwrap()
            .map(Result::unwrap).collect::<Vec<_>>();
        assert_eq!(rows, [(digest(&path), offset, "line_oversized".to_owned(), big.len() as i64)]);
        assert_eq!((f.count("codex_usage"), f.count("source_observations"), f.count("coverage_gaps")), (2, 11, 0));
        let payload: String = f.sidecar().query_row("SELECT payload FROM source_observations WHERE payload LIKE '%turn-x%'", [], |r| r.get(0)).unwrap();
        assert_eq!(payload, r#"{"effort":"Bearer [redacted]","model":"~/m [redacted] https://h.example/p second","turn_id":"turn-x"}"#);
    }
}

/// A sidecar that cannot grow (a file-size limit standing in for a full disk)
/// fails the rollout's transaction: collect records a pending coverage gap for
/// the unread range and exits instead of hanging or storing part of it. The
/// next collect with room reads the range and marks the gap recovered. A
/// source read before the ledger existed has a `predates_ingest` gap.
#[test]
fn unwritable_sidecar_records_coverage_gap() {
    const REPEATS: usize = 600;
    let f = Fixture::new();
    let path = f.rollout(&f.home, SID, &["head.jsonl"], &f.worktree(), f.decided + 1_000, "0.154.0");
    f.cli("collect");
    let head = fs::metadata(&path).unwrap().len() as i64;
    append(&path, &tail(&f, REPEATS).replace("@SID@", SID));
    let end = fs::metadata(&path).unwrap().len() as i64;
    let limit = fs::metadata(f.project.join(".state/telemetry.db")).unwrap().len();
    let mut command = collect_command(&f);
    // SAFETY: only async-signal-safe libc calls between fork and exec.
    unsafe {
        command.pre_exec(move || {
            libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
            let rlimit = libc::rlimit { rlim_cur: limit, rlim_max: limit };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &rlimit) != 0 { return Err(std::io::Error::last_os_error()); }
            Ok(())
        });
    }
    let status = command.status().unwrap();
    assert!(status.success(), "{status}");
    let gaps = |f: &Fixture| f.sidecar().prepare("SELECT source,start_offset,end_offset,reason,recovery FROM coverage_gaps").unwrap()
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?))).unwrap()
        .map(Result::unwrap).collect::<Vec<_>>();
    let gap = |recovery: &str| (digest(&path), head, end, "sidecar_write_failed".to_owned(), recovery.to_owned());
    assert_eq!(gaps(&f), [gap("pending")]);
    assert_eq!((f.count("codex_usage"), f.count("source_observations")), (1, 6), "nothing of the failed range is stored");

    f.cli("collect");
    assert_eq!(gaps(&f), [gap("recovered")]);
    assert_eq!((f.count("codex_usage"), f.count("source_observations")), (1 + REPEATS as i64, 6 + 4 * REPEATS as i64));

    // A sidecar collected before stream `ingest` 0002: the range already read has no envelopes.
    f.sidecar().execute_batch("DROP TABLE source_observations; DROP TABLE ingest_quarantine; DROP TABLE coverage_gaps; DROP TABLE source_cursors;
        UPDATE telemetry_streams SET version=1 WHERE stream='ingest'").unwrap();
    append(&path, &tail(&f, 1).replace("@SID@", SID));
    f.cli("collect");
    assert_eq!(gaps(&f), [(digest(&path), 0, end, "predates_ingest".to_owned(), "pending".to_owned())]);
    assert_eq!((f.count("codex_usage"), f.count("source_observations")), (2 + REPEATS as i64, 4));
}

/// Make `path` look unmodified for `secs` seconds.
fn age(path: &Path, secs: u64) {
    fs::File::options().write(true).open(path).unwrap().set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(secs)).unwrap();
}

/// A7 lost final event: a rollout read to its end whose last turn (opened by
/// its `turn_context`) has no `task_complete` is `open` while it was modified
/// within the idle threshold (600 s, twice the ticker pass), and a pending
/// `final_event_missing` coverage gap from the turn's first line to the file's
/// end once it has been idle longer, including when first read idle. Only the
/// file's size and modification time decide, so a later write (here a partial
/// line) widens the gap only once the file is idle again. The event arriving
/// recovers the gap. Usage is unchanged throughout.
#[test]
fn idle_rollout_without_final_event_records_a_coverage_gap() {
    let f = Fixture::new();
    let path = f.rollout(&f.home, SID, &["head.jsonl"], &f.worktree(), f.decided + 1_000, "0.154.0");
    let turn = fs::metadata(&path).unwrap().len() as i64;
    // `tail.jsonl` without its last line, `task_complete` of turn-2.
    let lines: Vec<String> = tail(&f, 1).replace("@SID@", SID).lines().map(|l| format!("{l}\n")).collect();
    append(&path, &lines[..4].concat());
    let end = fs::metadata(&path).unwrap().len() as i64;
    let gaps = |f: &Fixture| f.sidecar().prepare("SELECT source,start_offset,end_offset,reason,recovery FROM coverage_gaps").unwrap()
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?))).unwrap()
        .map(Result::unwrap).collect::<Vec<_>>();
    let gap = |end: i64, recovery: &str| (digest(&path), turn, end, "final_event_missing".to_owned(), recovery.to_owned());
    let final_event = |f: &Fixture| f.cli_args(&["collectors", "sessions"]).0["sessions"][0]["final_event"].clone();
    // 1000 + 500 input, 1120 + 560 total: turn-2's usage record counts, missing event or not.
    let usage = serde_json::json!({"input_tokens": 1500, "cached_input_tokens": 500, "cache_write_input_tokens": 0, "output_tokens": 180,
        "reasoning_output_tokens": 100, "total_tokens": 1680, "records": 2});

    // Just written: in progress.
    let (report, _) = f.cli("collect");
    assert_eq!(gaps(&f), []);
    assert_eq!(final_event(&f), serde_json::json!({"state": "open", "turn_id": "turn-2"}));
    assert_eq!(report["attempts"][0]["usage"], usage);
    // Unmodified for 9 minutes: still open. For 11: missing, with nothing new to read.
    age(&path, 540);
    f.cli("collect");
    assert_eq!(gaps(&f), []);
    age(&path, 660);
    for _ in 0..2 {
        f.cli("collect");
        assert_eq!(gaps(&f), [gap(end, "pending")]);
        assert_eq!(final_event(&f), serde_json::json!({"state": "missing", "turn_id": "turn-2"}));
    }
    // The same gap when the idle rollout is first read.
    remove_sidecar(&f);
    let (report, _) = f.cli("collect");
    assert_eq!(gaps(&f), [gap(end, "pending")]);
    assert_eq!(report["attempts"][0]["usage"], usage);

    // The writer resumes mid-line: the gap is unchanged until the file is idle again, then reaches the new end.
    let complete = &lines[4];
    append(&path, &complete[..40]);
    f.cli("collect");
    assert_eq!(gaps(&f), [gap(end, "pending")]);
    age(&path, 660);
    f.cli("collect");
    assert_eq!(gaps(&f), [gap(end + 40, "pending")]);
    // The final event arrives: recovered, and the turn is complete.
    append(&path, &complete[40..]);
    let (report, _) = f.cli("collect");
    assert_eq!(gaps(&f), [gap(end + 40, "recovered")]);
    assert_eq!(final_event(&f), serde_json::json!({"state": "complete", "turn_id": "turn-2"}));
    assert_eq!(report["attempts"][0]["usage"], usage);
    age(&path, 660);
    f.cli("collect");
    assert_eq!(gaps(&f), [gap(end + 40, "recovered")], "a completed turn is never missing");
}

/// F4 (certificate-live.md §5): the product cancelled the bound attempt while
/// its rollout's last turn was open, and Codex wrote no final event. Once the
/// attempt's termination receipt (`runtime.worker_terminated`, cause
/// `cancellation`) exists, the turn is `ended_by_termination` with the
/// receipt's cause and time, even idle past the threshold: no pending
/// `final_event_missing` gap, and `health evaluate` opens no alert for it.
/// A turn opened after the termination (a later resume) is judged on its own:
/// idle without its final event, it is `missing` again, and (F3) its usage
/// record, written after the receipt, is flagged `after_termination`; the
/// record before it is not. Usage is unchanged until then.
#[test]
fn a_turn_the_product_ended_is_ended_by_termination_not_a_missing_final_event() {
    let f = Fixture::new();
    let path = f.rollout(&f.home, SID, &["head.jsonl"], &f.worktree(), f.decided + 1_000, "0.154.0");
    let lines: Vec<String> = tail(&f, 1).replace("@SID@", SID).lines().map(|l| format!("{l}\n")).collect();
    append(&path, &lines[..4].concat());
    let final_event = |f: &Fixture| f.cli_args(&["collectors", "sessions"]).0["sessions"][0]["final_event"].clone();
    let pending = |f: &Fixture| f.sidecar().query_row("SELECT count(*) FROM coverage_gaps WHERE reason='final_event_missing' AND recovery='pending'", [], |r| r.get::<_, i64>(0)).unwrap();
    let (report, _) = f.cli("collect");
    let usage = report["attempts"][0]["usage"].clone();
    assert_eq!(final_event(&f), serde_json::json!({"state": "open", "turn_id": "turn-2"}));

    // Fixture only: the receipt the controller records when it stops a
    // cancelled worker (tests/canonical_worker.rs covers that writer).
    let terminated = f.decided + 5_000;
    rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap().execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('runtime.worker_terminated',?1,2,1,?2)",
        rusqlite::params![f.attempt, serde_json::json!({"version": 1, "attempt": f.attempt, "cause": "cancellation", "observed_unix_ms": terminated}).to_string()]).unwrap();
    age(&path, 660);
    for _ in 0..2 {
        let (report, _) = f.cli("collect");
        assert_eq!(final_event(&f), serde_json::json!({"state": "ended_by_termination", "turn_id": "turn-2",
            "termination": {"cause": "cancellation", "observed_unix_ms": terminated}}));
        assert_eq!(pending(&f), 0);
        assert_eq!(report["attempts"][0]["usage"], usage);
    }
    let after = |f: &Fixture| f.cli_args(&["collectors", "sessions"]).0["sessions"][0]["after_termination"].clone();
    assert_eq!(after(&f), serde_json::json!({"terminated_unix_ms": terminated, "records": 0, "first_unix_ms": null}));
    let evaluated = f.cli_args(&["health", "evaluate", "--json"]).0;
    assert!(!evaluated.to_string().contains("final_event"), "{evaluated}");
    assert_eq!(f.cli_args(&["health", "alerts", "--json"]).0["open"], serde_json::json!([]));

    // A later turn, opened after the termination, left open and idle: missing.
    let later = jiff::Timestamp::from_millisecond(f.decided + 10_000).unwrap().to_string();
    let original = jiff::Timestamp::from_millisecond(f.decided + 1_000).unwrap().to_string();
    append(&path, &lines[0].replace("turn-2", "turn-3").replace("\"ordinal\":8", "\"ordinal\":13").replace(&original, &later));
    append(&path, &lines[2].replace("turn-2", "turn-3").replace("resp-2", "resp-3").replace("\"ordinal\":10", "\"ordinal\":14").replace(&original, &later));
    age(&path, 660);
    f.cli("collect");
    assert_eq!(final_event(&f), serde_json::json!({"state": "missing", "turn_id": "turn-3"}));
    assert_eq!(pending(&f), 1);
    assert_eq!(after(&f), serde_json::json!({"terminated_unix_ms": terminated, "records": 1, "first_unix_ms": f.decided + 10_000}));
}

/// Timing flags and alerts leave the hand-computed 1000 + 500 + 500 M08 unchanged.
#[test]
fn post_termination_usage_is_visible_without_changing_accounting() {
    let f = Fixture::new();
    let path = f.rollout(&f.home, SID, &["head.jsonl"], &f.worktree(), f.decided + 1_000, "0.154.0");
    let first = f.decided + 10_000;
    let original = jiff::Timestamp::from_millisecond(f.decided + 1_000).unwrap().to_string();
    let later = jiff::Timestamp::from_millisecond(first).unwrap().to_string();
    let tail = tail(&f, 1).replace("@SID@", SID).replace(&original, &later);
    append(&path, &tail);
    append(&path, &tail.replace("turn-2", "turn-3").replace("resp-2", "resp-3")
        .replace("\"ordinal\":8", "\"ordinal\":13").replace("\"ordinal\":9", "\"ordinal\":14")
        .replace("\"ordinal\":10", "\"ordinal\":15").replace("\"ordinal\":11", "\"ordinal\":16").replace("\"ordinal\":12", "\"ordinal\":17"));
    let db = rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap();
    let terminated = f.decided + 5_000;
    db.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('runtime.worker_terminated',?1,2,1,?2)",
        rusqlite::params![f.attempt, serde_json::json!({"version": 1, "attempt": f.attempt, "cause": "cancellation", "observed_unix_ms": terminated}).to_string()]).unwrap();
    f.cli("collect");
    let usage = f.cli_args(&["usage", "--json"]).0;
    assert_eq!(usage["attempts"][0]["after_termination"], serde_json::json!({"records": 2, "first_unix_ms": first, "terminated_unix_ms": terminated}));
    assert!(f.text(&["usage"]).contains("after_termination={"));
    let report = f.report();
    assert_eq!(report["metrics"]["M08"]["value"], 2000);
    assert_eq!(report["after_termination"][0]["accounting"], "still counted in M08");
    let line = format!("attempt {} after_termination records=2 first_unix_ms={first} terminated_unix_ms={terminated}; still counted in M08", f.attempt);
    assert!(f.text(&["report", "--text"]).contains(&line));
    f.cli_args(&["health", "evaluate", "--json"]);
    let alerts = f.cli_args(&["health", "alerts", "--json"]).0;
    let alert = alerts["open"].as_array().unwrap().iter().find(|a| a["rule"] == "usage_after_termination").unwrap();
    assert_eq!(alert["state"], "warn");
    assert_eq!(alert["labels"], serde_json::json!({"project": "demo", "family": "consumption", "rule": "usage_after_termination", "service": "codex"}));
    assert_eq!(alert["evidence"]["records"], 2);

    // Same records, receipt after all of them: no post-termination usage.
    db.execute("UPDATE events SET payload=?1 WHERE kind='runtime.worker_terminated'", [serde_json::json!({"version": 1, "attempt": f.attempt, "cause": "cancellation", "observed_unix_ms": first + 1}).to_string()]).unwrap();
    f.cli("collect");
    assert_eq!(f.cli_args(&["usage", "--json"]).0["attempts"][0]["after_termination"], serde_json::Value::Null);
    let report = f.report();
    assert_eq!(report["metrics"]["M08"]["value"], 2000);
    assert!(report.get("after_termination").is_none());
    assert!(!f.text(&["report", "--text"]).contains("after_termination"));
    let evaluated = f.cli_args(&["health", "evaluate", "--json"]).0;
    assert!(evaluated["resolved"].as_array().unwrap().iter().any(|a| a["rule"] == "usage_after_termination"), "{evaluated}");
    assert!(!f.cli_args(&["health", "alerts", "--json"]).0["open"].as_array().unwrap().iter().any(|a| a["rule"] == "usage_after_termination"));
    // Legacy ingest has usage but no record timestamps: unavailable, never zero.
    f.sidecar().execute_batch("DROP TABLE codex_usage_times; UPDATE telemetry_streams SET version=3 WHERE stream='ingest';").unwrap();
    assert_eq!(f.cli_args(&["usage", "--json"]).0["attempts"][0]["after_termination"],
        serde_json::json!({"status": "unavailable", "reason": "predates_collection"}));
    let health = f.cli_args(&["health", "--json"]).0;
    let state = health["states"].as_array().unwrap().iter().find(|s| s["rule"] == "usage_after_termination").unwrap();
    assert_eq!(state["state"], "unknown");
    assert_eq!(state["reasons"][0]["code"], "predates_collection");
    assert_eq!(f.report()["metrics"]["M08"]["value"], 2000);

}

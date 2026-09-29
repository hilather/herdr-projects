//! Lane A ingest ledger end to end (docs/telemetry/contracts-collection.md,
//! card A2): Codex rollouts collected on the CLI into sanitized source
//! envelopes, replayed after a killed collect, with oversized lines quarantined
//! and an unwritable sidecar recorded as a coverage gap.

#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.

mod support;

use sha2::{Digest, Sha256};
use std::{fs, os::unix::process::CommandExt, path::Path, process::Command};
use support::telemetry::*;

/// sha256 of the sanitized canonical payloads of `head.jsonl` lines 5 and 6,
/// computed with `sha256sum` outside the crate.
const USAGE_DIGEST: &str = "sha256:9768f3bc423b89bc5cf97404ab8900514dc7c98f82e53f06c13aa46fcd1986f8";
const TOKEN_COUNT_DIGEST: &str = "sha256:75b89315857fa6d660a0fc1edfde66c3840c636e1c6abe4247db93c84dea1200";

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
        "codex_quarantine", "codex_discrepancy", "collect_offsets", "rollout_sources"] {
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
        |r| Ok((0..8).map(|i| r.get::<_, String>(i)).collect::<rusqlite::Result<Vec<_>>>()?)).unwrap();
    let provenance = r#"{"adapter":"codex","adapter_version":"0.154.0","interface":"rollout_jsonl","source_trust":"collector_observed"}"#;
    let measurement = r#"{"coverage":"complete","measurement_basis":"reported","normalization_version":1}"#;
    let identity = format!(r#"{{"session_id":"{sid}"}}"#);
    for (line, kind, payload_digest) in [(5, "codex.token_usage_record.v1", USAGE_DIGEST), (6, "codex.token_count.v1", TOKEN_COUNT_DIGEST)] {
        assert_eq!(envelope(line), [format!("codex:{source}:{}", starts[line - 1]), format!("codex:{sid}"), source.clone(), kind.into(),
            identity.clone(), provenance.into(), measurement.into(), payload_digest.into()]);
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

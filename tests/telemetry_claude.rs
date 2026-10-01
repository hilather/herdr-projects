//! DG4b end-to-end: public admission/store workflow and CLI, synthetic homes only.
#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)]
mod support;
use std::{fs, path::PathBuf};
use serde_json::{Value, json};
use support::telemetry::*;

fn claude() -> Fixture {
    let mut f = Fixture::new();
    let home = f.tmp.path().join("claude-execution-home");
    let mut profile = codex_profile(&f.config, "claude", "claude", Some(&home));
    profile.agent.version = "2.1.3".into();
    let path = f.project.join(".state/state.db");
    plant_profile(&path, profile);
    f.readmit("claude");
    let db = rusqlite::Connection::open(path).unwrap();
    (f.attempt, f.decided) = db.query_row("SELECT a.id,d.decided_unix_ms FROM attempts a JOIN dispatch_decisions d ON d.attempt_id=a.id WHERE a.state='reserved'",
        [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    f.home = home;
    db.execute("INSERT INTO collector_bindings(attempt_id,revision,state,collector,execution_home,unix_ms,source)
        VALUES(?1,1,'active','claude',?2,?3,'apply_launch_started')",
        rusqlite::params![f.attempt, f.home.display().to_string(), unix_ms()]).unwrap();
    f
}
fn transcript(f: &Fixture, sid: &str, cwd: &str, version: &str, time: i64) -> PathBuf {
    let dir = f.home.join(".claude/projects").join(cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect::<String>());
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{sid}.jsonl"));
    let text = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/telemetry/claude-code/session.jsonl")).unwrap();
    fs::write(&path, text.replace("@SID@", sid).replace("@CWD@", cwd).replace("@VERSION@", version)
        .replace("@TS@", &jiff::Timestamp::from_millisecond(time).unwrap().to_string())).unwrap();
    path
}
fn attempt_usage(f: &Fixture) -> Value {
    f.cli_args(&["usage", "--json"]).0["attempts"].as_array().unwrap().iter()
        .find(|a| a["attempt_id"] == f.attempt).unwrap()["usage"].clone()
}
fn no_secrets(f: &Fixture) {
    for name in ["telemetry.db", "telemetry.db-wal", "telemetry.db-shm"] {
        if let Ok(bytes) = fs::read(f.project.join(".state").join(name)) {
            assert!(!bytes.windows(b"CLAUDE_SECRET_".len()).any(|w| w == b"CLAUDE_SECRET_"), "content leaked to {name}");
        }
    }
}
#[test]
fn native_claude_usage_tools_sidechains_and_privacy() {
    let f = claude();
    transcript(&f, SID, &f.worktree(), "2.1.3", f.decided + 1000);
    let collected = f.cli("collect").0;
    assert_eq!(collected["collected"]["records"], 2);
    assert_eq!(attempt_usage(&f), json!({"input_tokens":382,"cached_input_tokens":240,"cache_write_input_tokens":32,
        "output_tokens":25,"reasoning_output_tokens":{"status":"unavailable","reason":"reasoning_tokens_not_reported"},"total_tokens":407,"records":2}));
    let attempts = f.cli_args(&["attempts", "--json"]).0;
    let observed = attempts["attempts"].as_array().unwrap().iter().find(|a| a["attempt_id"] == f.attempt).unwrap();
    assert_eq!(observed["usage"]["total_tokens"], 407);
    f.cli_args(&["accounting", "sync"]);
    let ledger = f.cli_args(&["accounting", "entries"]).0;
    let entries = ledger["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["normalization_version"], "claude-code-v1");
    let normalized: Vec<Value> = entries.iter().map(|e| e["normalized"].clone()).collect();
    assert!(normalized.contains(&json!({"input_tokens":330,"cache_read_tokens":200,"new_input_tokens":100,"cache_write_tokens":30,
        "output_tokens":20,"reasoning_tokens":0,"total_tokens":350})));
    let tools = f.cli_args(&["accounting", "tools", "--json"]).0;
    assert_eq!(tools["sessions"][0]["tools"]["issued"], 3);
    assert_eq!(tools["sessions"][0]["tools"]["executed"], 3);
    assert_eq!(tools["sessions"][0]["tools"]["succeeded"], 2);
    assert_eq!(tools["sessions"][0]["tools"]["failed"], 1);
    assert_eq!(tools["metrics"]["M17"]["value"], "2/3");
    assert_eq!(tools["metrics"]["M16"]["collaboration"]["sidechain_turns"], 2);
    let report = f.report();
    assert_eq!(report["metrics"]["M08"]["value"], 382);
    assert_eq!(report["metrics"]["M09"]["value"], 25);
    assert_eq!(report["metrics"]["M09"]["reasoning_output_tokens"]["reason"], "reasoning_tokens_not_reported");
    assert_eq!(report["metrics"]["M15"]["value"], "2/2");
    assert_eq!(f.count("claude_messages"), 2);
    let db = f.sidecar();
    assert_eq!(db.query_row("SELECT count(*) FROM source_observations WHERE json_extract(provenance,'$.adapter')='claude-code'", [], |r| r.get::<_, i64>(0)).unwrap(), 8);
    let unknown: String = db.query_row("SELECT json_extract(payload,'$.unmapped_keys') FROM source_observations WHERE json_extract(payload,'$.line_type')='future-line'", [], |r| r.get(0)).unwrap();
    assert!(unknown.contains("futureField") && unknown.contains("type:future-line"));
    assert_eq!(db.query_row("SELECT json_extract(payload,'$.unmapped_count') FROM source_observations WHERE json_extract(payload,'$.line_type')='future-line'",
        [], |r| r.get::<_, i64>(0)).unwrap(), 2);
    f.cli("collect");
    assert_eq!(f.count("claude_messages"), 2);
    assert_eq!(attempt_usage(&f)["total_tokens"], 407);
    let capabilities = f.cli_args(&["collectors", "capabilities", "--json"]).0;
    let adapter = capabilities["adapters"].as_array().unwrap().iter().find(|a| a["adapter"] == "claude-code").unwrap();
    assert_eq!(adapter["fixture_versions"], json!(["2.1.3", "2.1.286"]));
    // Live only for the fields the 2.1.286 live run observed; tools stay fixture.
    assert_eq!(adapter["certified_versions"], json!(["2.1.286"]));
    let live: Vec<&str> = adapter["fields"].as_array().unwrap().iter().filter(|f| f["certified"] == "live")
        .map(|f| f["field"].as_str().unwrap()).collect();
    assert_eq!(live, ["sessionId", "timestamp", "cwd", "version", "type", "message.model", "message.id",
        "message.usage.input_tokens", "message.usage.output_tokens", "message.usage.cache_creation_input_tokens",
        "message.usage.cache_read_input_tokens"]);
    assert!(adapter["fields"].as_array().unwrap().iter()
        .filter(|f| f["field"].as_str().unwrap().contains("tool") || f["field"] == "isSidechain")
        .all(|f| f["certified"] != "live"));
    no_secrets(&f);
}

#[test]
fn native_claude_partial_lines_and_truncation_replay() {
    let f = claude();
    let path = transcript(&f, SID, &f.worktree(), "2.1.3", f.decided + 1000);
    let text = fs::read_to_string(&path).unwrap();
    let split = text.find("\n{\"type\":\"assistant\",\"uuid\":\"side-1\"").unwrap() + 1;
    fs::write(&path, &text[..split + 30]).unwrap();
    f.cli("collect");
    assert_eq!(attempt_usage(&f)["total_tokens"], 350);
    assert_eq!(f.sidecar().query_row("SELECT byte_offset FROM collect_offsets", [], |r| r.get::<_, i64>(0)).unwrap(), split as i64);
    fs::write(&path, &text).unwrap();
    f.cli("collect");
    assert_eq!(attempt_usage(&f)["total_tokens"], 407);
    // A replacement/truncation replays zero without double counting native message ids.
    fs::write(&path, &text[..split]).unwrap();
    f.cli("collect");
    fs::write(&path, &text).unwrap();
    f.cli("collect");
    assert_eq!(attempt_usage(&f)["total_tokens"], 407);
    assert_eq!(f.count("codex_quarantine"), 0);
    no_secrets(&f);
}

#[test]
fn native_claude_unbound_and_uncertified_stay_unknown() {
    let f = claude();
    transcript(&f, "unbound", "/tmp/synthetic-unrelated-project", "2.1.3", f.decided + 1000);
    transcript(&f, "too-early", &f.worktree(), "2.1.3", f.decided - 1);
    transcript(&f, "uncertified", &f.worktree(), "9.9.9", f.decided + 1000);
    f.cli("collect");
    assert_eq!(attempt_usage(&f)["reason"], "cli_version_uncertified");
    let db = f.sidecar();
    assert_eq!(db.query_row("SELECT count(*) FROM rollout_sources WHERE binding='unbound'", [], |r| r.get::<_, i64>(0)).unwrap(), 2);
    assert_eq!(db.query_row("SELECT count(*) FROM codex_usage WHERE session_id='claude-code:uncertified' AND accepted=0 AND total_tokens IS NULL AND reason='cli_version_uncertified'", [], |r| r.get::<_, i64>(0)).unwrap(), 2);
    no_secrets(&f);
}

#[test]
fn codex_and_claude_native_sources_keep_separate_identity() {
    let f = claude();
    // The preceding real Codex attempt retains its own home and canonical binding.
    let codex_home = f.tmp.path().join("codex-home");
    let db = rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap();
    let (old, decided): (String, i64) = db.query_row("SELECT attempt_id,decided_unix_ms FROM dispatch_decisions WHERE attempt_id<>?1", [&f.attempt], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    let cwd = format!("{}/.state/worktrees/{old}/repo-00", f.project.display());
    f.rollout(&codex_home, SID, &["head.jsonl", "tail.jsonl"], &cwd, decided + 1000, "0.154.0");
    transcript(&f, SID, &f.worktree(), "2.1.3", f.decided + 1000);
    f.cli("collect");
    let usage = f.cli_args(&["usage", "--json"]).0;
    let old_usage = &usage["attempts"].as_array().unwrap().iter().find(|a| a["attempt_id"] == old).unwrap()["usage"];
    assert_eq!(old_usage["input_tokens"], 1500);
    assert_eq!(attempt_usage(&f)["input_tokens"], 382);
    let observed = f.report();
    assert_eq!(observed["metrics"]["M08"]["value"], 1882);
    f.cli_args(&["accounting", "sync"]);
    let aggregated = f.report();
    for id in ["M08", "M09", "M15", "M16", "M17", "M18"] {
        assert_eq!(aggregated["metrics"][id], observed["metrics"][id], "mixed adapters {id}");
    }
    f.cli_args(&["query", "--metric", "M08,M09,M16,M17,M18", "--json"]);
    f.cli_args(&["analytics", "refresh"]);
    assert_eq!(f.cli_args(&["analytics", "rebuild", "--verify"]).0["identical"], true);
    no_secrets(&f);
}

#[test]
fn native_claude_backup_restore_retention_and_tombstones() {
    let f = claude();
    transcript(&f, SID, &f.worktree(), "2.1.3", f.decided + 1000);
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    let backup = f.tmp.path().join("synthetic-claude-backup");
    f.cli_args(&["backup", "create", "--out", backup.to_str().unwrap()]);
    for name in ["telemetry.db", "telemetry.db-wal", "telemetry.db-shm"] {
        let _ = fs::remove_file(f.project.join(".state").join(name));
    }
    let restored = f.cli_args(&["backup", "restore", "--from", backup.to_str().unwrap()]).0;
    assert_eq!(restored["rows"]["claude_messages"], 2);
    assert_eq!(restored["rows"]["claude_tool_results"], 3);
    assert_eq!(f.cli("collect").0["collected"]["records"], 0);
    assert_eq!(attempt_usage(&f)["total_tokens"], 407);
    f.cancel_reserved();
    f.cli_args(&["accounting", "sync"]);
    f.sidecar().execute("UPDATE rollout_sources SET observed_unix_ms=observed_unix_ms-?1", [91_i64 * 86_400_000]).unwrap();
    let plan = f.cli_args(&["maintenance", "plan", "--json"]).0;
    let digest = plan["plan_digest"].as_str().unwrap();
    f.cli_args(&["maintenance", "apply", "--confirm", digest, "--json"]);
    assert_eq!(f.count("claude_messages"), 0);
    assert_eq!(f.count("claude_tool_results"), 0);
    assert_eq!(f.count("codex_usage"), 0);
    for table in ["accounting_usage_totals", "accounting_native_totals",
        "accounting_source_summary", "accounting_tool_summary"] {
        assert_eq!(f.count(table), 0, "retention must purge {table}");
    }
    f.cli("collect");
    assert_eq!(f.count("claude_messages"), 0);
    f.cli_args(&["accounting", "sync"]);
    let purged = f.report();
    assert_eq!(purged["metrics"]["M08"]["value"]["reason"], "no_certified_source");
    assert_eq!(purged["metrics"]["M16"]["coverage"]["sessions"], 0);
    f.cli_args(&["backup", "restore", "--from", backup.to_str().unwrap(), "--force"]);
    assert_eq!(f.count("claude_messages"), 0);
    assert_eq!(f.count("claude_tool_results"), 0);
    no_secrets(&f);
}

#[test]
fn claude_upgrade_preserves_an_existing_codex_ledger() {
    let f = Fixture::new();
    f.rollout(&f.home, SID, &["head.jsonl", "tail.jsonl"], &f.worktree(), f.decided + 1000, "0.154.0");
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    let before = f.cli_args(&["accounting", "entries"]).1;
    // Reconstruct the historical v12 ledger constraints with real collected rows.
    let db = f.sidecar();
    db.execute_batch("CREATE TEMP TABLE saved_entries AS SELECT * FROM usage_entries;
        CREATE TEMP TABLE saved_dispositions AS SELECT * FROM usage_dispositions;
        DELETE FROM usage_dispositions; DROP TABLE usage_entries;").unwrap();
    db.execute_batch(include_str!("../migrations/telemetry/accounting/0001_usage_ledger.sql")).unwrap();
    db.execute_batch("INSERT INTO usage_entries SELECT * FROM saved_entries;
        INSERT INTO usage_dispositions SELECT * FROM saved_dispositions;
        UPDATE telemetry_streams SET version=12 WHERE stream='accounting';").unwrap();
    drop(db);
    // A writable public command upgrades; every Codex byte visible in the ledger stays.
    f.cli("collect");
    assert_eq!(f.cli_args(&["accounting", "status"]).0["version"], 17);
    assert_eq!(f.cli_args(&["accounting", "entries"]).1, before);
    assert_eq!(attempt_usage(&f)["total_tokens"], 1680);
}

#[test]
fn unreported_claude_tool_outcome_is_unknown_in_its_own_scope() {
    let f = claude();
    let path = transcript(&f, SID, &f.worktree(), "2.1.3", f.decided + 1000);
    fs::write(&path, fs::read_to_string(&path).unwrap().replace("\"is_error\":true,", "")).unwrap();
    f.cli("collect");
    let tools = f.cli_args(&["accounting", "tools", "--json"]).0;
    assert_eq!(tools["sessions"][0]["tools"]["executed"], 3);
    assert_eq!(tools["sessions"][0]["tools"]["failed"], 0);
    assert_eq!(tools["sessions"][0]["tools"]["unknown"], 1);
    let metric = &tools["metrics"]["M17"];
    assert_eq!(metric["value"], "2/2");
    assert_eq!(metric["by_scope"]["claude-code"]["unknown"]["executions"], 1);
    assert_eq!(metric["by_scope"]["command_execution"]["unknown"]["executions"], 0);
    no_secrets(&f);
}

#[test]
fn claude_aggregate_reads_match_replay_and_verified_rebuild() {
    let f = claude();
    // A terminal planted attempt also exercises the maintained diagnostic.
    f.cancel_reserved();
    let terminated = unix_ms();
    rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap().execute(
        "INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('runtime.worker_terminated',?1,2,1,?2)",
        rusqlite::params![f.attempt, json!({"version": 1, "attempt": f.attempt,
            "cause": "cancellation", "observed_unix_ms": terminated}).to_string()]).unwrap();
    transcript(&f, SID, &f.worktree(), "2.1.3", terminated + 1000);
    f.cli("collect");
    let reads = || {
        let report = f.report();
        let metrics: Vec<Value> = ["M08", "M09", "M15", "M16", "M17", "M18"]
            .iter().map(|id| report["metrics"][id].clone()).collect();
        (metrics, report["after_termination"].clone(),
            f.cli_args(&["accounting", "tools", "--json"]).0,
            f.cli_args(&["view", "cost", "--json"]).0["rows"].as_array().unwrap().iter()
                .map(|r| json!({"metric_id": r["metric_id"], "value": r["value"],
                    "coverage": r["coverage"], "status": r["status"], "basis": r["basis"],
                    "digest": r["projection"]["content_digest"]})).collect::<Vec<_>>())
    };
    let replay = reads();
    assert_eq!(replay.0[0]["value"], 382);
    assert_eq!(replay.0[1]["value"], 25);
    assert_eq!(replay.0[3]["executed"]["by_scope"]["claude-code"], 3);
    assert_eq!(replay.0[3]["collaboration"]["sidechain_turns"], 2);
    assert_eq!(replay.0[4]["value"], "2/3");
    assert_eq!(replay.1.as_array().unwrap().len(), 1);
    f.cli_args(&["accounting", "sync"]);
    for table in ["accounting_usage_totals", "accounting_native_totals",
        "accounting_source_summary", "accounting_tool_summary"] {
        assert_eq!(f.count(table), 1, "Claude must populate {table}");
    }
    assert_eq!(reads(), replay, "maintained reads equal original derivation");
    f.cli_args(&["accounting", "reprice"]);
    let cost = f.cli_args(&["accounting", "cost", "--json"]).1;
    let cost_metrics = (f.report()["metrics"]["M12"].clone(), f.report()["metrics"]["M14"].clone());

    f.cli_args(&["query", "--metric", "M08,M09,M12,M14,M16,M17,M18", "--json"]);
    f.cli_args(&["analytics", "refresh"]);
    let pinned = f.cli_args(&["analytics", "snapshot"]).1;
    assert_eq!(f.cli_args(&["analytics", "rebuild", "--verify"]).0["identical"], true);
    f.cli_args(&["analytics", "rebuild"]);
    assert_eq!(f.cli_args(&["analytics", "snapshot"]).1, pinned);
    // A tool-only fixture correction must invalidate both the read summary
    // and analytics dependency even without another usage record.
    f.sidecar().execute("UPDATE claude_tool_results SET is_error=NULL WHERE call_id='call-2'", []).unwrap();
    let corrected = reads();
    assert_eq!(corrected.0[4]["value"], "2/2");
    assert_eq!(corrected.0[4]["by_scope"]["claude-code"]["unknown"]["executions"], 1);
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(reads(), corrected);
    let ledger = f.cli_args(&["accounting", "entries"]).1;
    // Removing the public projection marker forces the full sync derivation.
    f.sidecar().execute("DELETE FROM usage_ledger", []).unwrap();
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(f.cli_args(&["accounting", "entries"]).1, ledger);
    assert_eq!(reads(), corrected);
    assert_eq!(f.cli_args(&["accounting", "cost", "--json"]).1, cost);
    assert_eq!((f.report()["metrics"]["M12"].clone(), f.report()["metrics"]["M14"].clone()), cost_metrics);
    f.cli_args(&["analytics", "refresh"]);
    assert_eq!(f.cli_args(&["analytics", "rebuild", "--verify"]).0["identical"], true);
}

#[test]
fn claude_2_1_286_two_turns_in_lossy_project_directory() {
    let f = claude();
    let cwd = f.worktree();
    assert!(cwd.contains("/.state/"));
    let slug: String = cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    assert!(slug.contains("--state-"));
    let dir = f.home.join(".claude/projects").join(slug);
    fs::create_dir_all(&dir).unwrap();
    let text = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/telemetry/claude-2.1.286/live-two-turn.jsonl")).unwrap();
    let lines: Vec<String> = text.lines().map(|line| {
        let mut v: Value = serde_json::from_str(line).unwrap();
        // Free-text sentinels exercise every sanitized subtree in the real skeleton.
        v = serde_json::from_str(&v.to_string().replace("<str>", "CLAUDE_SECRET_LIVE")).unwrap();
        if v.get("cwd").is_some() { v["cwd"] = json!(cwd); }
        if v.get("version").is_some() { v["version"] = json!("2.1.286"); }
        if v.get("timestamp").is_some() {
            v["timestamp"] = json!(jiff::Timestamp::from_millisecond(f.decided + 1000).unwrap().to_string());
        }
        v.to_string()
    }).collect();
    fs::write(dir.join("ID000.jsonl"), format!("{}\n", lines.join("\n"))).unwrap();
    assert_eq!(f.cli("collect").0["collected"]["records"], 2);
    assert_eq!(f.binding(), ("bound".into(), Some(f.attempt.clone())));
    f.cli_args(&["accounting", "sync"]);
    let ledger = f.cli_args(&["accounting", "entries"]).0;
    let entries = ledger["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    for (output, write, read) in [(87, 6928, 0), (41, 151, 6928)] {
        let e = entries.iter().find(|e| e["normalized"]["output_tokens"] == output).unwrap();
        assert_eq!(e["model"], "claude-haiku-4-5-20251001");
        assert_eq!(e["normalized"]["new_input_tokens"], 10);
        assert_eq!(e["normalized"]["cache_write_tokens"], write);
        assert_eq!(e["normalized"]["cache_read_tokens"], read);
    }
    let db = f.sidecar();
    for kind in ["queue-operation", "attachment", "atis-latch", "last-prompt", "cost-state", "mode"] {
        assert!(db.query_row("SELECT count(*) FROM source_observations WHERE json_extract(payload,'$.line_type')=?1 AND json_extract(payload,'$.unmapped_count')>0", [kind], |r| r.get::<_, i64>(0)).unwrap() > 0);
    }
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(f.cli_args(&["accounting", "entries"]).0["entries"].as_array().unwrap().len(), 2);
    // A distinct cwd with the same lossy slug must not inherit the binding.
    let collision = cwd.replace("/.state/", "/_state/");
    let collision_slug: String = collision.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    assert_eq!(dir.file_name().unwrap().to_str().unwrap(), collision_slug);
    fs::write(dir.join("collision.jsonl"), format!("{}\n", json!({"type":"user", "sessionId":"collision", "cwd":collision,
        "version":"2.1.286", "timestamp":jiff::Timestamp::from_millisecond(f.decided + 1000).unwrap().to_string(),
        "message":{"content":"CLAUDE_SECRET_COLLISION"}}))).unwrap();
    f.cli("collect");
    assert_eq!(f.sidecar().query_row("SELECT binding FROM rollout_sources WHERE session_id='claude-code:collision'", [], |r| r.get::<_, String>(0)).unwrap(), "unbound");
    assert_eq!(attempt_usage(&f)["records"], 2);
    no_secrets(&f);
}

#[test]
fn claude_model_identifiers_and_planted_secret_conformance() {
    let f = claude();
    let path = transcript(&f, SID, &f.worktree(), "2.1.3", f.decided + 1000);
    let models = [
        "claude-haiku-4-5-20251001",
        "claude-sonnet-4-5-20250929",
        "claude-opus-4-1",
        "claude-future-9-7-20301231",
        "Bearer sk-ant-CLAUDE_SECRET_MODEL_SPACES",
        "sk-ant-CLAUDE_SECRET_MODEL_TOKEN",
    ];
    let mut lines = String::new();
    for (i, model) in models.iter().enumerate() {
        lines.push_str(&format!("{}\n", json!({"type":"assistant", "sessionId":SID,
            "cwd":f.worktree(), "version":"2.1.3",
            "timestamp":jiff::Timestamp::from_millisecond(f.decided + 1000).unwrap().to_string(),
            "message":{"id":format!("model-{i}"), "model":model,
                "usage":{"input_tokens":10,"output_tokens":1},
                "content":[{"type":"text","text":"CLAUDE_SECRET_MODEL_CONTENT"}]}})));
    }
    fs::write(path, lines).unwrap();
    assert_eq!(f.cli("collect").0["collected"]["records"], 6);
    f.cli_args(&["accounting", "sync"]);
    let ledger = f.cli_args(&["accounting", "entries"]).0;
    let entries = ledger["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 6);
    for model in &models[..4] {
        assert!(entries.iter().any(|entry| entry["model"] == *model), "{ledger}");
    }
    for expected in ["Bearer [redacted]", "[redacted]"] {
        assert!(entries.iter().any(|entry| entry["model"] == expected), "{ledger}");
    }
    let db = f.sidecar();
    for (i, model) in models.iter().take(4).enumerate() {
        let stored: String = db.query_row("SELECT json_extract(payload,'$.model') FROM source_observations WHERE json_extract(payload,'$.message_id')=?1",
            [format!("model-{i}")], |r| r.get(0)).unwrap();
        assert_eq!(stored, *model);
    }
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(f.cli_args(&["accounting", "entries"]).0["entries"].as_array().unwrap().len(), 6);
    no_secrets(&f);
}

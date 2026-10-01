//! DG4c public CLI workflows, synthetic execution homes and SDK 0.62.0 files.
#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)]
mod support;
use herdr_projects::telemetry::otlp;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf};
use support::telemetry::*;

fn gemini(bound: bool, version: &str) -> Fixture {
    let mut f = Fixture::new();
    let home = f.tmp.path().join("synthetic-gemini-home");
    fs::create_dir_all(&home).unwrap();
    // A synthetic retained profile provides the test-only harness identity.
    // Public admission records the attempt; no Gemini executable is launched.
    let mut profile = codex_profile(&f.config, "gemini", "gemini", Some(&home));
    profile.agent.version = version.to_owned();
    let state = f.project.join(".state/state.db");
    plant_profile(&state, profile);
    f.readmit("gemini");
    let db = rusqlite::Connection::open(state).unwrap();
    (f.attempt, f.decided) = db.query_row("SELECT a.id,d.decided_unix_ms FROM attempts a JOIN dispatch_decisions d ON d.attempt_id=a.id WHERE a.state='reserved'",
        [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    f.home = home;
    if bound {
        db.execute("INSERT INTO collector_bindings(attempt_id,revision,state,collector,execution_home,unix_ms,source) VALUES(?1,1,'active','gemini',?2,?3,'synthetic_launch')",
            rusqlite::params![f.attempt,f.home.display().to_string(),f.decided]).unwrap();
    }
    f
}

fn fixture(f: &Fixture, name: &str, at: i64) -> String {
    fs::read_to_string(format!(
        "{}/tests/fixtures/telemetry/gemini-cli/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
    .replace("@SEC@", &(at / 1000).to_string())
    .replace("@NANO@", &((at % 1000) * 1_000_000).to_string())
    // An untrusted native resource or attribute must never grant binding.
    .replace(
        "GEMINI_SECRET_PROMPT",
        &format!("GEMINI_SECRET_PROMPT {}", f.attempt),
    )
}
fn plant(f: &Fixture) -> (PathBuf, String) {
    let text = ["api", "metrics", "tool"]
        .map(|name| fixture(f, name, f.decided + 1000))
        .concat();
    let path = f.home.join("gemini-telemetry.json");
    fs::write(&path, &text).unwrap();
    (path, text)
}
fn no_secrets(f: &Fixture) {
    for file in ["telemetry.db", "telemetry.db-wal", "telemetry.db-shm"] {
        if let Ok(bytes) = fs::read(f.project.join(".state").join(file)) {
            assert!(
                !bytes
                    .windows(b"GEMINI_SECRET_".len())
                    .any(|w| w == b"GEMINI_SECRET_"),
                "leaked content in {file}"
            );
        }
    }
}
#[test]
fn local_sdk_usage_tools_privacy_and_idempotency() {
    let f = gemini(true, "0.62.0");
    plant(&f);
    f.cli("collect");
    let rows = otlp::records(&f.project).unwrap();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 7);
    assert!(rows.iter().all(|r| r["adapter"] == "otlp:gemini-cli"
        && r["attempt_id"] == f.attempt
        && r["binding"] == "exact"
        && r["certified"] == "fixture"));
    let api = rows
        .iter()
        .find(|r| r["native_name"] == "gemini_cli.api_response")
        .unwrap();
    assert_eq!(
        api["attributes"],
        json!({"model":"gemini-2.5-pro","input_token_count":19,"output_token_count":5,
        "cached_content_token_count":4,"thoughts_token_count":2,"tool_token_count":1,"total_token_count":31,"duration_ms":90})
    );
    let tool = rows.iter().find(|r| r["kind"] == "tool").unwrap();
    assert_eq!(
        tool["attributes"],
        json!({"function_name":"read_file","success":false,"duration_ms":12})
    );
    let mut counts: Vec<_> = rows
        .iter()
        .filter(|r| r["native_name"] == "gemini_cli.token.usage")
        .map(|r| {
            assert_eq!(r["aggregationTemporality"], 2);
            (
                r["attributes"]["type"].as_str().unwrap(),
                r["value"].as_i64().unwrap(),
            )
        })
        .collect();
    counts.sort();
    assert_eq!(
        counts,
        vec![
            ("cache", 4),
            ("input", 19),
            ("output", 5),
            ("thought", 2),
            ("tool", 1)
        ]
    );
    let before = otlp::records(&f.project).unwrap();
    f.cli("collect");
    assert_eq!(otlp::records(&f.project).unwrap(), before);
    let capabilities = f.cli_args(&["collectors", "capabilities", "--json"]).0;
    let cap = capabilities["adapters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["adapter"] == "gemini-cli")
        .unwrap();
    assert_eq!(cap["fixture_versions"], json!(["0.62.0"]));
    assert!(
        cap["fields"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v["certified"] != "live")
    );
    assert_eq!(cap["native_session"]["certified"], "fixture");
    no_secrets(&f);
}
#[test]
fn local_sdk_partial_object_newline_and_truncation_replay() {
    let f = gemini(true, "0.62.0");
    let (path, text) = plant(&f);
    let split = text.len() - fixture(&f, "tool", f.decided + 1000).len();
    fs::write(&path, &text[..split + 25]).unwrap();
    f.cli("collect");
    assert_eq!(
        otlp::records(&f.project).unwrap().as_array().unwrap().len(),
        6
    );
    assert_eq!(
        f.sidecar()
            .query_row("SELECT byte_offset FROM gemini_file_cursors", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        split as i64
    );
    fs::write(&path, &text[..text.len() - 1]).unwrap();
    f.cli("collect");
    assert_eq!(
        otlp::records(&f.project).unwrap().as_array().unwrap().len(),
        6
    );
    fs::write(&path, &text).unwrap();
    f.cli("collect");
    assert_eq!(
        otlp::records(&f.project).unwrap().as_array().unwrap().len(),
        7
    );
    fs::write(&path, &text[..split]).unwrap();
    f.cli("collect");
    fs::write(&path, &text).unwrap();
    f.cli("collect");
    assert_eq!(
        otlp::records(&f.project).unwrap().as_array().unwrap().len(),
        7
    );
    no_secrets(&f);
}
#[test]
fn local_sdk_unbound_early_revoked_and_uncertified() {
    for bound in [false, true] {
        let f = gemini(bound, "0.62.0");
        let text = fixture(
            &f,
            "api",
            if bound {
                f.decided - 1
            } else {
                f.decided + 1000
            },
        );
        fs::write(f.home.join("gemini-telemetry.json"), text).unwrap();
        f.cli("collect");
        let rows = otlp::records(&f.project).unwrap();
        assert_eq!(rows[0]["binding"], "unbound");
        assert!(rows[0]["attempt_id"].is_null());
        no_secrets(&f);
    }
    let f = gemini(true, "0.62.0");
    let db = rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap();
    db.execute("INSERT INTO collector_bindings(attempt_id,revision,state,collector,execution_home,unix_ms,source) VALUES(?1,2,'revoked','gemini',?2,?3,'synthetic_revoke')",
        rusqlite::params![f.attempt,f.home.display().to_string(),f.decided+500]).unwrap();
    fs::write(
        f.home.join("gemini-telemetry.json"),
        fixture(&f, "api", f.decided + 1000),
    )
    .unwrap();
    f.cli("collect");
    assert_eq!(otlp::records(&f.project).unwrap()[0]["binding"], "unbound");
    let f = gemini(true, "9.9.9");
    plant(&f);
    f.cli("collect");
    assert_eq!(otlp::records(&f.project).unwrap(), json!([]));
}
#[test]
fn local_sdk_backup_retention_restore_and_stream_upgrade() {
    let f = gemini(true, "0.62.0");
    plant(&f);
    f.cli("collect");
    let before = otlp::records(&f.project).unwrap();
    // Recreate the DG4a historical stream while preserving its real observations.
    f.sidecar().execute_batch("DROP TABLE gemini_file_cursors; UPDATE telemetry_streams SET version=1 WHERE stream='otlp'").unwrap();
    f.cli("collect");
    assert_eq!(otlp::records(&f.project).unwrap(), before);
    assert_eq!(
        f.sidecar()
            .query_row(
                "SELECT version FROM telemetry_streams WHERE stream='otlp'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        2
    );
    let backup = f.tmp.path().join("synthetic-gemini-backup");
    f.cli_args(&["backup", "create", "--out", backup.to_str().unwrap()]);
    for file in ["telemetry.db", "telemetry.db-wal", "telemetry.db-shm"] {
        let _ = fs::remove_file(f.project.join(".state").join(file));
    }
    let restored = f
        .cli_args(&["backup", "restore", "--from", backup.to_str().unwrap()])
        .0;
    assert_eq!(restored["rows"]["gemini_file_cursors"], 1);
    assert_eq!(restored["rows"]["otlp_records"], 7);
    f.cli("collect");
    assert_eq!(otlp::records(&f.project).unwrap(), before);
    let classes = f.cli_args(&["maintenance", "classes", "--json"]).0;
    let class = classes["classes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["class"] == "sidecar.otlp")
        .unwrap();
    assert_eq!(class["action"], "retain");
    no_secrets(&f);
}

fn native(f: &Fixture, hash: &str) -> PathBuf {
    fs::create_dir_all(f.worktree()).unwrap();
    let dir = f.home.join(".gemini/tmp/synthetic-short-id/chats");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("session-synthetic.jsonl");
    let text = fs::read_to_string(format!(
        "{}/tests/fixtures/telemetry/gemini-cli/session.jsonl",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
    .replace("@HASH@", hash)
    .replace(
        "@TS@",
        &jiff::Timestamp::from_millisecond(f.decided + 1000)
            .unwrap()
            .to_string(),
    );
    fs::write(&path, text).unwrap();
    path
}
#[test]
fn native_chat_metadata_updates_binding_privacy_and_retention() {
    let f = gemini(true, "0.62.0");
    let hash = format!("{:x}", Sha256::digest(f.worktree().as_bytes()));
    let path = native(&f, &hash);
    plant(&f);
    f.cli("collect");
    let db = f.sidecar();
    let (binding, attempt): (String, String) = db
        .query_row(
            "SELECT binding,attempt_id FROM rollout_sources WHERE originator='gemini-cli'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(binding, "bound");
    assert_eq!(attempt, f.attempt);
    let rows = || {
        f.sidecar().prepare("SELECT payload FROM source_observations WHERE event_kind='gemini-cli.gemini_line.v1' ORDER BY producer_sequence").unwrap()
        .query_map([],|r|r.get::<_,String>(0)).unwrap().map(|r|serde_json::from_str::<Value>(&r.unwrap()).unwrap()).collect::<Vec<_>>()
    };
    let before = rows();
    assert_eq!(before.len(), 5);
    let usage = before.iter().find(|r| r["total"] == 26).unwrap();
    assert_eq!(
        (
            usage["input"].clone(),
            usage["output"].clone(),
            usage["cached"].clone(),
            usage["thoughts"].clone(),
            usage["tool"].clone()
        ),
        (json!(19), json!(5), json!(4), json!(2), json!(1))
    );
    assert_eq!(usage["tool_names"], json!(["read_file"]));
    assert_eq!(usage["tool_statuses"], json!(["error"]));
    assert_eq!(usage["message_id"], "message-1");
    // An update with no counters is retained as metadata, never a usage delta.
    assert!(
        before
            .iter()
            .any(|r| r["message_id"] == "message-1" && r["total"].is_null())
    );
    f.cli("collect");
    assert_eq!(rows(), before);
    let text = fs::read_to_string(&path).unwrap();
    fs::write(&path, &text[..text.find('\n').unwrap() + 1]).unwrap();
    f.cli("collect");
    fs::write(&path, text).unwrap();
    f.cli("collect");
    assert_eq!(rows(), before);
    no_secrets(&f);
    assert_eq!(f.count("ingest_quarantine"), 0);
    drop(db);
    let backup = f.tmp.path().join("synthetic-native-gemini-backup");
    f.cli_args(&["backup", "create", "--out", backup.to_str().unwrap()]);
    for file in ["telemetry.db", "telemetry.db-wal", "telemetry.db-shm"] {
        let _ = fs::remove_file(f.project.join(".state").join(file));
    }
    f.cli_args(&["backup", "restore", "--from", backup.to_str().unwrap()]);
    f.cli("collect");
    assert_eq!(rows(), before);
    f.cancel_reserved();
    f.cli_args(&["accounting", "sync"]);
    f.sidecar()
        .execute(
            "UPDATE rollout_sources SET observed_unix_ms=observed_unix_ms-?1",
            [91_i64 * 86_400_000],
        )
        .unwrap();
    let plan = f.cli_args(&["maintenance", "plan", "--json"]).0;
    f.cli_args(&[
        "maintenance",
        "apply",
        "--confirm",
        plan["plan_digest"].as_str().unwrap(),
        "--json",
    ]);
    assert_eq!(f.count("rollout_sources"), 0);
    f.cli("collect");
    assert_eq!(f.count("rollout_sources"), 0);
    f.cli_args(&[
        "backup",
        "restore",
        "--from",
        backup.to_str().unwrap(),
        "--force",
    ]);
    assert_eq!(f.count("rollout_sources"), 0);
    assert!(rows().is_empty());
    // OTel observations remain a separate retain class; native pruning cannot erase them.
    assert_eq!(
        otlp::records(&f.project).unwrap().as_array().unwrap().len(),
        7
    );
    no_secrets(&f);
}
#[test]
fn native_chat_unknown_hash_stays_unbound_and_partial_line_waits() {
    let f = gemini(true, "0.62.0");
    let path = native(&f, &"0".repeat(64));
    let text = fs::read_to_string(&path).unwrap();
    let split = text.rfind('\n').unwrap();
    fs::write(&path, &text[..split]).unwrap();
    f.cli("collect");
    assert_eq!(
        f.sidecar()
            .query_row("SELECT binding FROM rollout_sources", [], |r| r
                .get::<_, String>(0))
            .unwrap(),
        "unbound"
    );
    assert_eq!(f.sidecar().query_row("SELECT count(*) FROM source_observations WHERE event_kind='gemini-cli.gemini_line.v1'",[],|r|r.get::<_,i64>(0)).unwrap(),4);
    fs::write(&path, text).unwrap();
    f.cli("collect");
    assert_eq!(f.sidecar().query_row("SELECT count(*) FROM source_observations WHERE event_kind='gemini-cli.gemini_line.v1'",[],|r|r.get::<_,i64>(0)).unwrap(),5);
    no_secrets(&f);
}

#[test]
fn local_gemini_keeps_codex_and_claude_results_unchanged() {
    let mut f = Fixture::new();
    let codex = f.attempt.clone();
    f.rollout(
        &f.home,
        SID,
        &["head.jsonl", "tail.jsonl"],
        &f.worktree(),
        f.decided + 1000,
        "0.154.0",
    );
    let claude_home = f.tmp.path().join("synthetic-claude-home");
    let mut profile = codex_profile(&f.config, "claude", "claude", Some(&claude_home));
    profile.agent.version = "2.1.3".into();
    let state = f.project.join(".state/state.db");
    plant_profile(&state, profile);
    f.readmit("claude");
    let db = rusqlite::Connection::open(&state).unwrap();
    (f.attempt,f.decided) = db.query_row("SELECT a.id,d.decided_unix_ms FROM attempts a JOIN dispatch_decisions d ON d.attempt_id=a.id WHERE a.state='reserved'", [], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
    f.home = claude_home;
    let claude = f.attempt.clone();
    db.execute("INSERT INTO collector_bindings(attempt_id,revision,state,collector,execution_home,unix_ms,source) VALUES(?1,1,'active','claude',?2,?3,'synthetic_launch')",rusqlite::params![f.attempt,f.home.display().to_string(),f.decided]).unwrap();
    let dir = f
        .home
        .join(".claude/projects")
        .join(f.worktree().replace('/', "-"));
    fs::create_dir_all(&dir).unwrap();
    let text = fs::read_to_string(format!(
        "{}/tests/fixtures/telemetry/claude-code/session.jsonl",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
    .replace("@SID@", SID)
    .replace("@CWD@", &f.worktree())
    .replace("@VERSION@", "2.1.3")
    .replace(
        "@TS@",
        &jiff::Timestamp::from_millisecond(f.decided + 1000)
            .unwrap()
            .to_string(),
    );
    fs::write(dir.join("session.jsonl"), text).unwrap();
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    let usage_before = f.cli_args(&["usage", "--json"]).0;
    let ledger_before = f.cli_args(&["accounting", "entries"]).0;
    let tools_before = f.cli_args(&["accounting", "tools", "--json"]).0;
    let gemini_home = f.tmp.path().join("synthetic-gemini-home");
    let mut profile = codex_profile(&f.config, "gemini", "gemini", Some(&gemini_home));
    profile.agent.version = "0.62.0".into();
    plant_profile(&state, profile);
    f.readmit("gemini");
    (f.attempt,f.decided) = db.query_row("SELECT a.id,d.decided_unix_ms FROM attempts a JOIN dispatch_decisions d ON d.attempt_id=a.id WHERE a.state='reserved'", [], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
    f.home = gemini_home;
    fs::create_dir_all(&f.home).unwrap();
    db.execute("INSERT INTO collector_bindings(attempt_id,revision,state,collector,execution_home,unix_ms,source) VALUES(?1,1,'active','gemini',?2,?3,'synthetic_launch')",rusqlite::params![f.attempt,f.home.display().to_string(),f.decided]).unwrap();
    plant(&f);
    native(
        &f,
        &format!("{:x}", Sha256::digest(f.worktree().as_bytes())),
    );
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    let usage_after = f.cli_args(&["usage", "--json"]).0;
    for (attempt, total) in [(codex, 1680), (claude, 407)] {
        let find = |value: &Value| {
            value["attempts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["attempt_id"] == attempt)
                .unwrap()["usage"]
                .clone()
        };
        assert_eq!(find(&usage_after), find(&usage_before));
        assert_eq!(find(&usage_after)["total_tokens"], total);
    }
    assert_eq!(f.cli_args(&["accounting", "entries"]).0, ledger_before);
    // Coverage reports the new native source as not accounting-certified;
    // every existing tool result and aggregate remains byte-for-byte equal.
    let mut metrics_after = f.cli_args(&["accounting", "tools", "--json"]).0["metrics"].clone();
    for metric in metrics_after.as_object_mut().unwrap().values_mut() {
        assert_eq!(
            metric["coverage"]["excluded"]
                .as_object_mut()
                .unwrap()
                .remove("cli_version_uncertified"),
            Some(json!(1))
        );
    }
    assert_eq!(metrics_after, tools_before["metrics"]);
    assert_eq!(
        otlp::records(&f.project).unwrap().as_array().unwrap().len(),
        7
    );
    no_secrets(&f);
}

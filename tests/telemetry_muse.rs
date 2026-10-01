//! DG4i public CLI workflows with sanitized recorded-live Muse fixtures.
#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)]
mod support;
use serde_json::{Value, json};
use std::{fs, path::PathBuf};
use support::telemetry::*;

fn muse() -> Fixture {
    muse_version("1.4.0-R4161.1")
}
fn muse_version(version: &str) -> Fixture {
    let mut f = Fixture::new();
    let home = f.tmp.path().join("muse-execution-home");
    let mut profile = codex_profile(&f.config, "muse", "muse", Some(&home));
    profile.agent.version = version.into();
    let state = f.project.join(".state/state.db");
    plant_profile(&state, profile);
    f.readmit("muse");
    let db = rusqlite::Connection::open(state).unwrap();
    (f.attempt,f.decided) = db.query_row("SELECT a.id,d.decided_unix_ms FROM attempts a JOIN dispatch_decisions d ON d.attempt_id=a.id WHERE a.state='reserved'",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
    f.home = home;
    db.execute("INSERT INTO collector_bindings(attempt_id,revision,state,collector,execution_home,unix_ms,source) VALUES(?1,1,'active','muse',?2,?3,'synthetic_launch')",rusqlite::params![f.attempt,f.home.display().to_string(),f.decided]).unwrap();
    f
}
fn plant(f: &Fixture, cwd: &str, time: i64) -> Vec<PathBuf> {
    let root = f.home.join(".local/share/muse/sessions/2026/10/01/parent");
    let mut paths = Vec::new();
    for (name, dir) in [
        ("live-two-turn-session", root.clone()),
        ("live-subagent-a", root.join("subagent/child-a")),
        ("live-subagent-b", root.join("subagent/child-b")),
    ] {
        fs::create_dir_all(&dir).unwrap();
        let text = fs::read_to_string(format!(
            "{}/tests/fixtures/telemetry/muse-1.4.0/{name}.jsonl",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let mut out = String::new();
        for line in text.lines() {
            let mut v: Value = serde_json::from_str(line).unwrap();
            if let Some(n) = v["recorded_at"].as_i64() {
                v["recorded_at"] = json!(time * 1000 + n - 1790873146589538_i64);
            }
            if v.pointer("/payload/record/workspace_root").is_some() {
                v["payload"]["record"]["workspace_root"] = json!(cwd);
            }
            out.push_str(
                &serde_json::to_string(&v)
                    .unwrap()
                    .replace("<str>", "MUSE_SECRET_CONTENT"),
            );
            out.push('\n');
        }
        let path = dir.join("session.jsonl");
        fs::write(&path, out).unwrap();
        paths.push(path);
    }
    paths
}
fn no_secrets(f: &Fixture) {
    for name in ["telemetry.db", "telemetry.db-wal", "telemetry.db-shm"] {
        if let Ok(bytes) = fs::read(f.project.join(".state").join(name)) {
            assert!(
                !bytes
                    .windows(b"MUSE_SECRET".len())
                    .any(|w| w == b"MUSE_SECRET"),
                "leak in {name}"
            );
        }
    }
}
#[test]
fn native_muse_exact_usage_children_privacy_replay_backup_retention() {
    let f = muse();
    f.cli("collect");
    f.sidecar().execute_batch("DROP TABLE muse_events; UPDATE telemetry_streams SET version=12 WHERE stream='ingest'; UPDATE telemetry_streams SET version=17 WHERE stream='accounting'").unwrap();
    let paths = plant(&f, &f.worktree(), f.decided + 1000);
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    let entries = f.cli_args(&["accounting", "entries"]).0;
    let rows = accepted_delta_entries(&entries);
    assert_eq!(rows.len(), 4);
    assert_eq!(f.sidecar().query_row("SELECT count(*) FROM source_observations WHERE json_extract(provenance,'$.adapter')='muse' AND json_extract(measurement,'$.certification')='fixture'",[],|r|r.get::<_,i64>(0)).unwrap(),7);
    let expected = [
        (19947, 24, 5233, 13),
        (19993, 41, 14065, 30),
        (3168, 1005, 0, 865),
        (3236, 248, 2929, 150),
    ];
    for (i, o, r, t) in expected {
        assert!(rows.iter().any(|e|e["normalized"] == json!({"input_tokens":i,"cache_read_tokens":r,"new_input_tokens":i-r,"cache_write_tokens":0,"output_tokens":o,"reasoning_tokens":t,"total_tokens":i+o})));
    }
    assert!(
        rows.iter()
            .all(|e| e["model"] == "muse-spark-1.3-contributor"
                && e["normalization_version"] == "muse-v1")
    );
    let usage = f.cli_args(&["usage", "--json"]).0;
    let attempt = usage["attempts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["attempt_id"] == f.attempt)
        .unwrap();
    assert_eq!(
        attempt["usage"],
        json!({"input_tokens":46344,"cached_input_tokens":22227,"cache_write_input_tokens":0,"output_tokens":1318,"reasoning_output_tokens":1058,"total_tokens":47662,"records":4})
    );
    assert_eq!(
        f.sidecar()
            .query_row(
                "SELECT count(*) FROM rollout_sources WHERE binding='bound' AND attempt_id=?1",
                [&f.attempt],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        3
    );
    assert_eq!(
        f.sidecar()
            .query_row(
                "SELECT count(*) FROM codex_agent_items WHERE item_type='SubAgentActivity'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        2
    );
    assert_eq!(f.sidecar().query_row("SELECT count(*) FROM codex_agent_items WHERE session_id='muse:parent' AND agent_thread_id IN ('muse:child-a','muse:child-b')",[],|r|r.get::<_,i64>(0)).unwrap(),2);
    let caps = f.cli_args(&["collectors", "capabilities", "--json"]).0;
    let cap = caps["adapters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["adapter"] == "muse")
        .unwrap();
    assert_eq!(cap["fixture_versions"], json!(["1.4.0-R4161.1"]));
    assert_eq!(cap["certified_versions"], json!([]));
    f.cli("collect");
    assert_eq!(f.cli_args(&["accounting", "entries"]).0, entries);
    for path in &paths {
        let text = fs::read_to_string(path).unwrap();
        fs::write(path, &text[..text.find('\n').unwrap() + 1]).unwrap();
        f.cli("collect");
        fs::write(path.with_extension("replacement"), &text).unwrap();
        fs::rename(path.with_extension("replacement"), path).unwrap();
    }
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(f.cli_args(&["accounting", "entries"]).0, entries);
    no_secrets(&f);
    let backup = f.tmp.path().join("backup");
    f.cli_args(&["backup", "create", "--out", backup.to_str().unwrap()]);
    for name in ["telemetry.db", "telemetry.db-wal", "telemetry.db-shm"] {
        let _ = fs::remove_file(f.project.join(".state").join(name));
    }
    f.cli_args(&["backup", "restore", "--from", backup.to_str().unwrap()]);
    f.cli("collect");
    assert_eq!(f.count("muse_events"), 4);
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
    assert_eq!(f.count("muse_events"), 0);
    assert_eq!(f.count("muse_parents"), 0);
    f.cli("collect");
    assert_eq!(f.count("rollout_sources"), 0);
    f.cli_args(&[
        "backup",
        "restore",
        "--from",
        backup.to_str().unwrap(),
        "--force",
    ]);
    assert_eq!(f.count("muse_events"), 0);
    assert_eq!(f.count("muse_parents"), 0);
    no_secrets(&f);
}
#[test]
fn native_muse_unbound_workspace_and_early_session() {
    for early in [false, true] {
        let f = muse();
        let cwd = f.worktree();
        plant(
            &f,
            if early { &cwd } else { "/unrelated/workspace" },
            if early {
                f.decided - 100000
            } else {
                f.decided + 1000
            },
        );
        f.cli("collect");
        assert_eq!(
            f.sidecar()
                .query_row(
                    "SELECT count(*) FROM rollout_sources WHERE binding='bound'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        no_secrets(&f);
    }
}

#[test]
fn native_muse_preserves_existing_codex_ledger() {
    let f = muse();
    let db = rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap();
    let (id,at): (String,i64) = db.query_row("SELECT i.attempt_id,d.decided_unix_ms FROM attempt_inputs i JOIN dispatch_decisions d ON d.attempt_id=i.attempt_id WHERE json_extract(i.payload,'$.inputs.effective_profile.kind')='codex'",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
    let cwd = format!("{}/.state/worktrees/{id}/repo-00", f.project.display());
    f.rollout(
        &f.tmp.path().join("codex-home"),
        SID,
        &["head.jsonl", "tail.jsonl"],
        &cwd,
        at + 1000,
        "0.154.0",
    );
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    let before = f.cli_args(&["accounting", "entries"]).0;
    plant(&f, &f.worktree(), f.decided + 1000);
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    let after = f.cli_args(&["accounting", "entries"]).0;
    let old: Vec<_> = after["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| !e["session_id"].as_str().unwrap().starts_with("muse:"))
        .cloned()
        .collect();
    assert_eq!(json!(old), before["entries"]);
    no_secrets(&f);
}

#[test]
fn native_muse_partial_append_revocation_and_version_gate() {
    let f = muse();
    let paths = plant(&f, &f.worktree(), f.decided + 1000);
    let text = fs::read_to_string(&paths[0]).unwrap();
    let boundary = text
        .lines()
        .take_while(|line| !line.contains("model_completed"))
        .map(|line| line.len() + 1)
        .sum::<usize>();
    fs::write(&paths[0], &text[..boundary + 10]).unwrap();
    f.cli("collect");
    assert_eq!(f.count("muse_events"), 2);
    fs::write(&paths[0], text).unwrap();
    f.cli("collect");
    assert_eq!(f.count("muse_events"), 4);
    let db = rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap();
    db.execute("INSERT INTO collector_bindings(attempt_id,revision,state,collector,execution_home,unix_ms,source) VALUES(?1,2,'revoked','muse',?2,?3,'synthetic_revoke')",rusqlite::params![f.attempt,f.home.display().to_string(),f.decided+500]).unwrap();
    f.cli("collect");
    assert_eq!(
        f.sidecar()
            .query_row(
                "SELECT count(*) FROM rollout_sources WHERE binding='bound'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    no_secrets(&f);
    let f = muse_version("9.9.9");
    plant(&f, &f.worktree(), f.decided + 1000);
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    let entries = f.cli_args(&["accounting", "entries"]).0;
    assert!(accepted_delta_entries(&entries).is_empty());
    no_secrets(&f);
}

#[test]
fn native_muse_child_waits_for_its_exact_parent_path() {
    let f = muse();
    let paths = plant(&f, &f.worktree(), f.decided + 1000);
    let parent = fs::read(&paths[0]).unwrap();
    fs::remove_file(&paths[0]).unwrap();
    // A copied parent with the same session directory name in another date
    // cannot grant the child's workspace binding.
    let other = f
        .home
        .join(".local/share/muse/sessions/2026/10/02/parent/session.jsonl");
    fs::create_dir_all(other.parent().unwrap()).unwrap();
    fs::write(&other, &parent).unwrap();
    f.cli("collect");
    assert_eq!(f.sidecar().query_row("SELECT count(*) FROM rollout_sources WHERE session_id LIKE 'muse:child-%' AND binding='bound'",[],|r|r.get::<_,i64>(0)).unwrap(),0);
    fs::write(&paths[0], parent).unwrap();
    f.cli("collect");
    assert_eq!(f.sidecar().query_row("SELECT count(*) FROM rollout_sources WHERE session_id LIKE 'muse:child-%' AND binding='bound'",[],|r|r.get::<_,i64>(0)).unwrap(),2);
    f.cli_args(&["accounting", "sync"]);
    let entries = f.cli_args(&["accounting", "entries"]).0;
    assert_eq!(accepted_delta_entries(&entries).len(), 4);
    no_secrets(&f);
}

#[test]
fn native_muse_parent_without_a_timestamp_cannot_bind_children() {
    let f = muse();
    let paths = plant(&f, &f.worktree(), f.decided + 1000);
    let text = fs::read_to_string(&paths[0]).unwrap();
    let mut out = String::new();
    for line in text.lines() {
        let mut v: Value = serde_json::from_str(line).unwrap();
        if v["payload_type"] == "runtime.session.metadata" {
            v["recorded_at"] = Value::Null;
        }
        out.push_str(&serde_json::to_string(&v).unwrap());
        out.push('\n');
    }
    fs::write(&paths[0], out).unwrap();
    f.cli("collect");
    assert_eq!(
        f.sidecar()
            .query_row(
                "SELECT count(*) FROM rollout_sources WHERE binding='bound'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    no_secrets(&f);
}

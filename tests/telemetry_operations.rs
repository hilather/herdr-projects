//! TM5.3 retention, deletion, backup and restore end to end, through
//! `herdr-projects telemetry <slug> maintenance|backup` on the CLI over a real
//! reserved Codex attempt, hand-written rollouts and planted canonical rows
//! (docs/telemetry/operations-runbook.md, plan doc 09). Expected values are
//! hand-computed from the fixtures (two usage records 1000/120 and 500/60,
//! planted ages and file sizes); none is read back from a production result.

#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.

mod support;

use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::{fs, process::Command};
use support::telemetry::*;

const DAY: i64 = 86_400_000;

fn command(f: &Fixture, args: &[&str], path: Option<&str>, cwd: Option<&Path>) -> std::process::Output {
    let mut command = Command::new(BIN);
    command.env_clear().env("HOME", f.tmp.path().join("home")).env("PATH", path.unwrap_or("/usr/bin:/bin"))
        .args(["--root", f.root.to_str().unwrap(), "telemetry", "demo"]).args(args);
    if let Some(cwd) = cwd { command.current_dir(cwd); }
    command.output().unwrap()
}

fn ok(f: &Fixture, args: &[&str]) -> String {
    let out = command(f, args, None, None);
    assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

fn json_of(f: &Fixture, args: &[&str]) -> Value { serde_json::from_str(&ok(f, args)).unwrap() }

fn fail(f: &Fixture, args: &[&str]) -> String {
    let out = command(f, args, None, None);
    assert!(!out.status.success(), "{args:?} succeeded: {}", String::from_utf8_lossy(&out.stdout));
    String::from_utf8(out.stderr).unwrap()
}

fn plan(f: &Fixture) -> Value { json_of(f, &["maintenance", "plan", "--json"]) }

fn class<'a>(plan: &'a Value, id: &str) -> &'a Value {
    plan["classes"].as_array().unwrap().iter().find(|c| c["class"] == id).unwrap_or_else(|| panic!("no class {id}"))
}

fn config(f: &Fixture) -> PathBuf {
    let dir = f.tmp.path().join("home/.config/herdr-projects");
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_private(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn state(f: &Fixture) -> rusqlite::Connection {
    let db = rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap();
    db.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    db
}

/// A canonical attempt of its own task with an optional terminal mark (fixture rows).
fn plant_attempt(f: &Fixture, id: &str, attempt_state: &str, observed: bool, mark: Option<(&str, i64)>) {
    let db = state(f);
    db.execute("INSERT INTO tasks(id,revision,state,title) VALUES(?1,1,'failed',?1)", [format!("task-{id}")]).unwrap();
    db.execute("INSERT INTO attempts(id,task_id,revision,state,reservation,termination_observed) VALUES(?1,?2,2,?3,?1,?4)",
        rusqlite::params![id, format!("task-{id}"), attempt_state, observed]).unwrap();
    if let Some((mark, at)) = mark {
        db.execute("INSERT INTO attempt_lifecycle(attempt_id,state,attempt_revision,unix_ms,source) VALUES(?1,?2,2,?3,'fixture')", rusqlite::params![id, mark, at]).unwrap();
    }
}

fn file(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

/// Every file under `dir` (links not followed), sorted.
fn files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else { return out };
    for entry in entries.flatten() {
        let meta = fs::symlink_metadata(entry.path()).unwrap();
        if meta.is_dir() { out.extend(files(&entry.path())); } else if meta.is_file() { out.push(entry.path()); }
    }
    out.sort();
    out
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool { haystack.windows(needle.len()).any(|w| w == needle) }

/// A rollout with both usage records (1000/120 and 500/60), collected and synced.
fn collected() -> Fixture {
    let f = Fixture::new();
    f.rollout(&f.home, SID, &["head.jsonl", "tail.jsonl"], &f.worktree(), f.decided + 1_000, "0.154.0");
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    f
}

/// A rollout of another session (`sid`) bound to the fixture attempt.
fn rollout_as(f: &Fixture, sid: &str, parts: &[&str]) {
    let path = f.rollout(&f.home, sid, parts, &f.worktree(), f.decided + 2_000, "0.154.0");
    fs::write(&path, fs::read_to_string(&path).unwrap().replace(SID, sid)).unwrap();
}

fn remove_sidecar(f: &Fixture) {
    for suffix in ["", "-wal", "-shm"] { let _ = fs::remove_file(format!("{}{suffix}", f.project.join(".state/telemetry.db").display())); }
}

#[test]
fn retention_classes_are_declared_with_doc09_defaults() {
    let f = Fixture::new();
    let classes = json_of(&f, &["maintenance", "classes", "--json"]);
    assert_eq!(classes["policy"], "retention.v1");
    let row = |id: &str| {
        let c = classes["classes"].as_array().unwrap().iter().find(|c| c["class"] == id).unwrap();
        (c["retention_days"].clone(), c["action"].as_str().unwrap().to_owned(), c["basis"].as_str().unwrap().to_owned(), c["destructive"].as_bool().unwrap())
    };
    assert_eq!(row("sidecar.normalized_sessions"), (json!(90), "prune".into(), "derivable_from_native_source".into(), true));
    assert_eq!(row("sidecar.attention_samples"), (json!(90), "prune".into(), "source_of_truth".into(), true));
    assert_eq!(row("sidecar.health_evaluations"), (json!(90), "prune".into(), "derivable".into(), false));
    assert_eq!(row("sidecar.analytics_revisions"), (json!(365), "prune".into(), "derivable".into(), false));
    assert_eq!(row("sidecar.accounting_imports"), (Value::Null, "retain".into(), "source_of_truth".into(), true));
    assert_eq!(row("sidecar.valuation_history"), (Value::Null, "retain".into(), "source_of_truth".into(), true));
    assert_eq!(row("ops.tombstones"), (json!(400), "listed_not_pruned".into(), "source_of_truth".into(), true));
    assert_eq!(row("artefact.git_quarantine"), (json!(7), "prune".into(), "source_of_truth".into(), true));
    assert_eq!(row("artefact.submission_spool"), (json!(7), "prune".into(), "source_of_truth".into(), true));
    assert_eq!(row("artefact.replay_repos"), (json!(30), "prune".into(), "source_of_truth".into(), true));
    assert_eq!(row("artefact.backups"), (json!(30), "prune".into(), "source_of_truth".into(), true));
    assert_eq!(row("optin.external_export_files"), (json!(7), "prune".into(), "source_of_truth".into(), true));
    assert_eq!(row("optin.captured_evidence"), (json!(7), "not_built".into(), "source_of_truth".into(), true));
    assert_eq!(row("canonical.state"), (Value::Null, "external_lifecycle".into(), "canonical".into(), true));
    // A deployment override is explicit policy; tombstones are never shortened.
    write_private(&config(&f).join("telemetry-retention.toml"), "schema = \"telemetry-retention.v1\"\n[days]\n\"sidecar.attention_samples\" = 30\n");
    let classes = json_of(&f, &["maintenance", "classes", "--json"]);
    let attention = classes["classes"].as_array().unwrap().iter().find(|c| c["class"] == "sidecar.attention_samples").unwrap();
    assert_eq!((attention["retention_days"].clone(), attention["policy_source"].clone()), (json!(30), json!("override")));
    write_private(&config(&f).join("telemetry-retention.toml"), "schema = \"telemetry-retention.v1\"\n[days]\n\"ops.tombstones\" = 30\n");
    assert!(fail(&f, &["maintenance", "classes"]).contains("tombstones are kept at least 400 days"));
}

#[test]
fn deleted_sessions_are_tombstoned_and_never_collected_again() {
    let f = collected();
    assert_eq!(f.count("codex_usage"), 2);
    // Fresh and bound to a live attempt: nothing is due.
    assert_eq!(class(&plan(&f), "sidecar.normalized_sessions")["eligible_count"], 0);
    f.sidecar().execute("UPDATE rollout_sources SET observed_unix_ms=observed_unix_ms-?1", [91 * DAY]).unwrap();
    let blocked = plan(&f);
    assert_eq!(class(&blocked, "sidecar.normalized_sessions")["blocked"], json!([{"key": format!("session:{SID}"), "reason": "attempt_not_terminal"}]));
    f.cancel_reserved();
    let canonical = fs::read(f.project.join(".state/state.db")).unwrap();
    let due = plan(&f);
    assert_eq!(class(&due, "sidecar.normalized_sessions")["eligible"], json!([{"key": format!("session:{SID}"), "records": 2, "age_days": 91, "reason": "retention_expired"}]));
    assert_eq!((due["items"].clone(), due["destructive_items"].clone()), (json!(1), json!(1)));
    let digest = due["plan_digest"].as_str().unwrap().to_owned();
    // Destructive: refused without the digest, and with another one.
    let refused = fail(&f, &["maintenance", "apply"]);
    assert!(refused.contains(&format!("--confirm {digest}")), "{refused}");
    assert!(fail(&f, &["maintenance", "apply", "--confirm", "sha256:0"]).contains("the plan changed"));
    let dry = json_of(&f, &["maintenance", "apply", "--dry-run", "--json"]);
    assert_eq!(dry["would_delete"], json!([{"class": "sidecar.normalized_sessions", "key": format!("session:{SID}")}]));
    assert_eq!(f.count("codex_usage"), 2, "a dry run deletes nothing");
    // A hold blocks it; its reason is stored as an excerpt.
    let hold = json_of(&f, &["maintenance", "hold", "add", "--class", "sidecar.normalized_sessions", "--scope", SID, "--reason", "legal hold token=sk-canaryHoldReason0009"]);
    assert_eq!((hold["hold_id"].clone(), hold["reason"].clone()), (json!("hold-1"), json!("legal hold token=[redacted]")));
    let held = plan(&f);
    assert_eq!(class(&held, "sidecar.normalized_sessions")["held"], json!([{"key": format!("session:{SID}"), "hold_id": "hold-1"}]));
    let applied = json_of(&f, &["maintenance", "apply", "--json"]);
    assert_eq!((applied["deleted"].clone(), f.count("codex_usage")), (json!({}), 2), "held: nothing deleted, no confirmation needed");
    json_of(&f, &["maintenance", "hold", "release", "hold-1", "--reason", "matter closed"]);
    // Owner only: a worker context is refused.
    let worktree = f.project.join(".state/worktrees").join(&f.attempt);
    fs::create_dir_all(&worktree).unwrap();
    let out = command(&f, &["maintenance", "apply", "--confirm", &digest], None, Some(&worktree));
    assert!(String::from_utf8_lossy(&out.stderr).contains("refuses to run inside a worker execution context"), "{}", String::from_utf8_lossy(&out.stderr));
    let applied = json_of(&f, &["maintenance", "apply", "--confirm", &digest, "--json"]);
    assert_eq!((applied["deleted"].clone(), applied["tombstones_added"].clone(), applied["canonical_written"].clone()),
        (json!({"sidecar.normalized_sessions": 1}), json!(2), json!(false)));
    for table in ["codex_usage", "rollout_sources", "collect_offsets", "source_observations", "source_cursors", "usage_entries", "usage_dispositions", "codex_turns", "codex_rate_limits"] {
        assert_eq!(f.count(table), 0, "{table}");
    }
    // The rollout is still on disk: collect, a copy of the session under another
    // name, and a rebuilt sidecar never bring it back.
    let (report, _) = f.cli("collect");
    assert_eq!((report["collected"]["records"].clone(), f.count("codex_usage")), (json!(0), 0));
    assert_eq!(report["attempts"][0]["usage"], json!({"status": "unavailable", "reason": "not_bound"}));
    f.rollout(&f.home, "copy", &["head.jsonl", "tail.jsonl"], &f.worktree(), f.decided + 1_000, "0.154.0");
    f.cli("collect");
    assert_eq!((f.count("codex_usage"), f.count("rollout_sources")), (0, 0));
    remove_sidecar(&f);
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    assert_eq!((f.count("codex_usage"), f.count("usage_entries")), (0, 0));
    assert_eq!(fs::read(f.project.join(".state/state.db")).unwrap(), canonical, "maintenance and rebuild never write state.db");
    // The tombstone store keeps no deleted label or unsalted hash.
    let ops = fs::read(f.project.join(".state/telemetry-ops.db")).unwrap();
    assert!(!contains(&ops, SID.as_bytes()) && !contains(&ops, b"sk-canaryHoldReason0009"));
    let after = plan(&f);
    assert_eq!((after["items"].clone(), after["tombstones"]["by_class"].clone()), (json!(0), json!({"sidecar.normalized_sessions": 2})));
}

#[test]
fn accounting_intents_stay_until_their_disposition() {
    let f = Fixture::new();
    f.rollout(&f.home, SID, &["head.jsonl", "tail.jsonl"], &f.worktree(), f.decided + 1_000, "0.154.0");
    f.cli("collect");
    f.cancel_reserved();
    f.sidecar().execute("UPDATE rollout_sources SET observed_unix_ms=observed_unix_ms-?1", [91 * DAY]).unwrap();
    // Accepted usage not yet in the ledger has no disposition: it stays.
    assert_eq!(class(&plan(&f), "sidecar.normalized_sessions")["blocked"], json!([{"key": format!("session:{SID}"), "reason": "ledger_not_synced"}]));
    f.cli_args(&["accounting", "sync"]);
    f.sidecar().execute("UPDATE usage_dispositions SET disposition='unresolved' WHERE rowid=(SELECT min(rowid) FROM usage_dispositions)", []).unwrap();
    assert_eq!(class(&plan(&f), "sidecar.normalized_sessions")["blocked"], json!([{"key": format!("session:{SID}"), "reason": "accounting_disposition_pending"}]));
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(class(&plan(&f), "sidecar.normalized_sessions")["eligible_count"], 1, "the rebuilt ledger disposed of every entry");
    // An operator deletion of a fresh session still needs its dispositions.
    let g = collected();
    g.cancel_reserved();
    let forgotten = json_of(&g, &["maintenance", "plan", "--forget-session", SID, "--json"]);
    assert_eq!(class(&forgotten, "sidecar.normalized_sessions")["eligible"],
        json!([{"key": format!("session:{SID}"), "records": 2, "age_days": 0, "reason": "operator_deletion"}]));
    let digest = forgotten["plan_digest"].as_str().unwrap().to_owned();
    json_of(&g, &["maintenance", "apply", "--forget-session", SID, "--confirm", &digest, "--json"]);
    assert_eq!(g.count("codex_usage"), 0);
}

// Remove only observation-clock fields; all recorded values and as_of stay exact.
fn pane_recorded(mut pane: Value) -> Value {
    fn clocks(value: &mut Value) {
        match value {
            Value::Object(object) => {
                for key in ["query_unix_ms", "lag_ms", "elapsed_ms"] { object.remove(key); }
                for value in object.values_mut() { clocks(value); }
            }
            Value::Array(values) => for value in values { clocks(value); },
            _ => {}
        }
    }
    clocks(&mut pane);
    pane
}

#[test]
fn current_sidecar_opens_while_a_collector_holds_the_write_lock() {
    let f = collected();
    let mut writer = f.sidecar();
    let tx = writer.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).unwrap();
    // The public writable-open path must not request a migration write lock
    // on an already current store. The collector retains its lock throughout.
    let opened = herdr_projects::telemetry::sidecar::open(&f.project, false).unwrap().unwrap();
    let totals: (i64, i64) = opened.query_row("SELECT sum(input_tokens),sum(output_tokens) FROM usage_entries WHERE basis='delta'", [],
        |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!(totals, (1500, 180));
    drop(opened);
    tx.rollback().unwrap();
    f.cli("collect");
    assert_eq!(f.usage().len(), 2, "opening beside a writer preserves collected records");
}

#[test]
fn attention_health_and_analytics_expire_without_losing_current_views() {
    let f = collected();
    let now = unix_ms();
    for (attempt, at) in [(f.attempt.as_str(), now - 100 * DAY), ("gone-attempt", now - 100 * DAY), ("gone-attempt", now - 95 * DAY), ("gone-attempt", now - 10 * DAY)] {
        f.sidecar().execute("INSERT INTO attention_samples(attempt_id,observed_unix_ms,state,gap,interval_ms,source) VALUES(?1,?2,'working',NULL,30000,'herdr-agent-list-v1')",
            rusqlite::params![attempt, at]).unwrap();
    }
    f.cli_args(&["health", "evaluate", "--json"]);
    f.cli_args(&["health", "evaluate", "--json"]);
    f.sidecar().execute("UPDATE health_evaluations SET evaluated_unix_ms=evaluated_unix_ms-?1 WHERE evaluation=(SELECT min(evaluation) FROM health_evaluations)", [91 * DAY]).unwrap();
    // The previous binary's ad-hoc tables were guarded despite being disposable.
    f.sidecar().execute_batch("UPDATE telemetry_streams SET version=1 WHERE stream='analytics';
        CREATE TRIGGER analytics_workspace_metrics_no_delete BEFORE DELETE ON analytics_workspace_metrics BEGIN SELECT RAISE(ABORT,'legacy immutable'); END;
        CREATE TRIGGER analytics_workspace_comparisons_no_delete BEFORE DELETE ON analytics_workspace_comparisons BEGIN SELECT RAISE(ABORT,'legacy immutable'); END;").unwrap();
    f.cli_args(&["analytics", "refresh"]);
    assert_eq!(json_of(&f, &["analytics", "status"])["version"], 2);
    let due = plan(&f);
    assert_eq!(class(&due, "sidecar.attention_samples")["eligible"], json!([{"key": "attempt:gone-attempt", "samples": 2}]));
    assert_eq!(class(&due, "sidecar.attention_samples")["blocked"], json!([{"key": format!("attempt:{}", f.attempt), "reason": "attempt_not_terminal"}]));
    assert_eq!(class(&due, "sidecar.health_evaluations")["eligible"], json!([{"key": "evaluations_before_cutoff", "evaluations": 1}]));
    assert_eq!(class(&due, "sidecar.analytics_revisions")["eligible_count"], 0, "no revision is 365 days old");
    let digest = due["plan_digest"].as_str().unwrap().to_owned();
    json_of(&f, &["maintenance", "apply", "--confirm", &digest, "--json"]);
    assert_eq!(f.count("attention_samples"), 2, "the live attempt's sample and the 10-day-old one stay");
    assert_eq!(f.count("health_evaluations"), 1);
    // Analytics: superseded revisions expire under an explicit policy; each
    // cell keeps its current revision. A second rollout changes the input
    // totals, so its cells get restatements (each supersedes one revision).
    write_private(&config(&f).join("telemetry-retention.toml"), "schema = \"telemetry-retention.v1\"\n[days]\n\"sidecar.analytics_revisions\" = 0\n");
    rollout_as(&f, "00000000-0000-4000-8000-0000000000b2", &["head.jsonl"]);
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    f.cli_args(&["analytics", "refresh"]);
    f.cancel_reserved();
    rollout_as(&f, "00000000-0000-4000-8000-0000000000b3", &["head.jsonl"]);
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    f.cli_args(&["analytics", "refresh"]);
    let restated: i64 = f.sidecar().query_row("SELECT count(*) FROM analytics_revisions r WHERE EXISTS(SELECT 1 FROM analytics_revisions n WHERE n.cell=r.cell AND n.revision>r.revision)", [], |r| r.get(0)).unwrap();
    assert!(restated > 0, "the second rollout restates some cells");
    let comparisons = f.count("analytics_workspace_comparisons");
    assert!(comparisons > 1, "cancelling changes the recorded comparison");
    let cells: i64 = f.sidecar().query_row("SELECT count(DISTINCT cell) FROM analytics_revisions", [], |r| r.get(0)).unwrap();
    let pane = pane_recorded(json_of(&f, &["workspace", "show", "--json"]));
    let revisions = json_of(&f, &["analytics", "revisions", "--metric", "M40"]);
    let latest = revisions["revisions"].as_array().unwrap().last().unwrap();
    assert_eq!(pane["services"]["M40"]["as_of"], json!({"seq": latest["revision"], "unix_ms": latest["recorded_unix_ms"]}));
    let pinned = json_of(&f, &["query", "--metric", "M40", "--as-of-seq", &latest["revision"].to_string(), "--json"]);
    assert_eq!(pane["services"]["M40"]["value"], pinned["results"][0]["value"]);
    let m08 = f.report()["metrics"]["M08"].clone();
    let backup = f.tmp.path().join("before-expiry");
    // Back up a legacy sidecar: restore must migrate the private copy before
    // applying revision/comparison tombstones through its old delete guards.
    f.sidecar().execute_batch("UPDATE telemetry_streams SET version=1 WHERE stream='analytics';
        CREATE TRIGGER analytics_workspace_metrics_no_delete BEFORE DELETE ON analytics_workspace_metrics BEGIN SELECT RAISE(ABORT,'legacy immutable'); END;
        CREATE TRIGGER analytics_workspace_comparisons_no_delete BEFORE DELETE ON analytics_workspace_comparisons BEGIN SELECT RAISE(ABORT,'legacy immutable'); END;").unwrap();
    json_of(&f, &["backup", "create", "--out", backup.to_str().unwrap()]);
    f.cli_args(&["analytics", "refresh"]);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let due = plan(&f);
    assert_eq!(class(&due, "sidecar.analytics_revisions")["eligible_count"], restated + comparisons - 1, "superseded revisions and comparisons");
    json_of(&f, &["maintenance", "apply", "--confirm", due["plan_digest"].as_str().unwrap(), "--json"]);
    assert_eq!(f.count("analytics_revisions"), cells, "one current revision per cell");
    assert_eq!(f.count("analytics_workspace_comparisons"), 1);
    assert!(!f.sidecar().prepare("PRAGMA foreign_key_check(analytics_workspace_metrics)").unwrap().exists([]).unwrap());
    assert_eq!(pane_recorded(json_of(&f, &["workspace", "show", "--json"])), pane);
    assert_eq!(f.report()["metrics"]["M08"], m08, "current views unchanged");
    // A restore of the earlier backup reapplies the revision tombstones.
    let report = json_of(&f, &["backup", "restore", "--from", backup.to_str().unwrap(), "--force"]);
    assert_eq!(report["tombstones"]["reapplied"]["sidecar.analytics_revisions"], restated);
    assert_eq!(f.count("analytics_revisions"), cells);
    assert_eq!(pane_recorded(json_of(&f, &["workspace", "show", "--json"])), pane);
    let orphaned: i64 = f.sidecar().query_row("SELECT count(*) FROM analytics_workspace_metrics w LEFT JOIN analytics_revisions r ON r.revision=w.revision WHERE r.revision IS NULL", [], |r| r.get(0)).unwrap();
    assert_eq!(orphaned, 0);
    let projected = f.count("analytics_workspace_metrics");
    assert!(projected > 0);
    // Reject redundant inserts even when INSERT OR IGNORE would hide them.
    // A public refresh must keep existing rendering rows and their as-of.
    f.sidecar().execute_batch("CREATE TRIGGER reject_redundant_workspace_projection BEFORE INSERT ON analytics_workspace_metrics
        WHEN EXISTS(SELECT 1 FROM analytics_workspace_metrics WHERE revision=NEW.revision)
        BEGIN SELECT RAISE(ABORT,'redundant workspace projection'); END;").unwrap();
    f.cli_args(&["analytics", "refresh"]);
    f.sidecar().execute_batch("DROP TRIGGER reject_redundant_workspace_projection").unwrap();
    assert_eq!(pane_recorded(json_of(&f, &["workspace", "show", "--json"])), pane);
    f.sidecar().execute("DELETE FROM analytics_workspace_metrics", []).unwrap();
    // Unchanged current cells still repair a missing rendering on refresh.
    f.cli_args(&["analytics", "refresh"]);
    assert_eq!(f.count("analytics_workspace_metrics"), projected);
    assert_eq!(pane_recorded(json_of(&f, &["workspace", "show", "--json"])), pane);
    f.sidecar().execute("DELETE FROM analytics_workspace_metrics", []).unwrap();
    f.cli_args(&["analytics", "rebuild"]);
    assert_eq!(f.count("analytics_workspace_metrics"), projected);
    assert_eq!(pane_recorded(json_of(&f, &["workspace", "show", "--json"])), pane);
    // Both disposable tables can be lost and recovered through the public rebuild.
    let mut comparison = pane["configurations"].clone();
    comparison.as_object_mut().unwrap().remove("as_of");
    f.sidecar().execute("DELETE FROM analytics_workspace_comparisons", []).unwrap();
    f.cli_args(&["analytics", "rebuild"]);
    let rebuilt = json_of(&f, &["workspace", "show", "--json"]);
    assert!(rebuilt["configurations"]["as_of"]["unix_ms"].as_i64().unwrap() >= pane["configurations"]["as_of"]["unix_ms"].as_i64().unwrap());
    let mut rebuilt_comparison = rebuilt["configurations"].clone();
    rebuilt_comparison.as_object_mut().unwrap().remove("as_of");
    assert_eq!(rebuilt_comparison, comparison);
    // The immutability guard is back in place.
    assert!(f.sidecar().execute("DELETE FROM analytics_revisions", []).is_err());
}

#[test]
fn backup_restore_reapplies_tombstones_and_never_writes_canonical_state() {
    let f = collected();
    let usage = f.usage();
    let out = f.tmp.path().join("backup-1");
    let created = json_of(&f, &["backup", "create", "--out", out.to_str().unwrap()]);
    assert_eq!(created["encrypted"], false);
    let names: Vec<&str> = created["files"].as_array().unwrap().iter().map(|f| f["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["telemetry.db"]);
    assert_eq!(fs::metadata(&out).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(fs::metadata(out.join("telemetry.db")).unwrap().permissions().mode() & 0o777, 0o600);
    let manifest: Value = serde_json::from_slice(&fs::read(out.join("manifest.json")).unwrap()).unwrap();
    assert_eq!((manifest["rows"]["codex_usage"].clone(), manifest["rows"]["rollout_sources"].clone(), manifest["project"].clone()), (json!(2), json!(1), json!("demo")));
    assert_eq!(json_of(&f, &["backup", "verify", "--from", out.to_str().unwrap()])["verified"], true);
    // A changed byte fails verification.
    let tampered = f.tmp.path().join("tampered");
    fs::create_dir(&tampered).unwrap();
    for name in ["manifest.json", "telemetry.db"] { fs::copy(out.join(name), tampered.join(name)).unwrap(); }
    let mut bytes = fs::read(tampered.join("telemetry.db")).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    fs::write(tampered.join("telemetry.db"), bytes).unwrap();
    assert!(fail(&f, &["backup", "restore", "--from", tampered.to_str().unwrap()]).contains("telemetry.db does not match the backup manifest"));
    // Delete the session, then restore the backup that still holds it.
    f.sidecar().execute("UPDATE rollout_sources SET observed_unix_ms=observed_unix_ms-?1", [91 * DAY]).unwrap();
    f.cancel_reserved();
    let canonical = fs::read(f.project.join(".state/state.db")).unwrap();
    let digest = plan(&f)["plan_digest"].as_str().unwrap().to_owned();
    json_of(&f, &["maintenance", "apply", "--confirm", &digest, "--json"]);
    // Offline only: refused while the ticker holds its lock.
    let ticker = fs::OpenOptions::new().create(true).truncate(false).write(true).open(f.root.join(".ticker.lock")).unwrap();
    ticker.lock().unwrap();
    assert!(fail(&f, &["backup", "restore", "--from", out.to_str().unwrap()]).contains("restore is offline"));
    ticker.unlock().unwrap();
    let report = json_of(&f, &["backup", "restore", "--from", out.to_str().unwrap()]);
    assert_eq!((report["tombstones"]["reapplied"].clone(), report["tombstones"]["total"].clone()), (json!({"sidecar.normalized_sessions": 1}), json!(2)));
    assert_eq!((report["rows"]["codex_usage"].clone(), report["canonical_written"].clone(), report["replaced_newer"].clone()), (json!(0), json!(false), Value::Null));
    assert_eq!(f.count("codex_usage"), 0, "restore never resurrects deleted content");
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(f.count("codex_usage"), 0);
    // A sidecar newer than the backup (a live attention sample) is not replaced without --force.
    f.sidecar().execute("INSERT INTO attention_samples(attempt_id,observed_unix_ms,state,gap,interval_ms,source) VALUES('a-new',?1,'working',NULL,30000,'herdr-agent-list-v1')",
        [unix_ms() + 60_000]).unwrap();
    let refused = fail(&f, &["backup", "restore", "--from", out.to_str().unwrap()]);
    assert!(refused.contains("newer than the backup") && refused.contains("\"attention_samples\":1"), "{refused}");
    let forced = json_of(&f, &["backup", "restore", "--from", out.to_str().unwrap(), "--force"]);
    assert_eq!(forced["replaced_newer"]["not_recoverable"]["attention_samples"], 1);
    assert_eq!((f.count("attention_samples"), f.count("codex_usage")), (0, 0));
    assert_eq!(fs::read(f.project.join(".state/state.db")).unwrap(), canonical, "backup and restore never write state.db");
    assert_eq!(json_of(&f, &["backup", "list"])["backups"].as_array().unwrap().len(), 1);
    // Without any deletion, a restore keeps every native identity: collecting
    // again dedupes, nothing is new usage and nothing is quarantined.
    let g = collected();
    let before = g.report();
    let out = g.tmp.path().join("backup-2");
    json_of(&g, &["backup", "create", "--out", out.to_str().unwrap()]);
    remove_sidecar(&g);
    let report = json_of(&g, &["backup", "restore", "--from", out.to_str().unwrap()]);
    assert_eq!((report["rows"]["codex_usage"].clone(), report["orphans"].clone()), (json!(2), json!(0)));
    assert_eq!(g.usage(), usage);
    let (again, _) = g.cli("collect");
    assert_eq!((again["collected"]["records"].clone(), g.count("codex_quarantine")), (json!(0), 0));
    assert_eq!(again["attempts"][0]["usage"], json!({"input_tokens": 1500, "cached_input_tokens": 500, "cache_write_input_tokens": 0,
        "output_tokens": 180, "reasoning_output_tokens": 100, "total_tokens": 1680, "records": 2}));
    assert_eq!(g.report()["metrics"]["M08"], before["metrics"]["M08"]);
    assert_eq!(g.report()["metrics"]["M08"]["value"], 1500);
}

#[test]
fn encrypted_backups_use_the_operators_age_and_leave_no_plaintext() {
    let f = collected();
    let out = f.tmp.path().join("sealed");
    let refused = fail(&f, &["backup", "create", "--out", out.to_str().unwrap(), "--encrypt-to", "age1fixturerecipient"]);
    assert!(refused.contains("`age` is not installed: nothing was written"), "{refused}");
    assert!(!out.exists());
    // A deterministic local stand-in for `age` (fixture): a header and base64.
    let bin = f.tmp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    write_private(&bin.join("age"), "#!/bin/sh\nset -e\nif [ \"$1\" = -d ]; then test -f \"$3\"; tail -n +2 \"$6\" | base64 -d > \"$5\"; exit 0; fi\n\
        test \"$1\" = -r; test -n \"$2\"; { printf 'age-fixture %s\\n' \"$2\"; base64 -w0 \"$5\"; } > \"$4\"\n");
    fs::set_permissions(bin.join("age"), fs::Permissions::from_mode(0o700)).unwrap();
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let run = |args: &[&str]| { let o = command(&f, args, Some(&path), None); assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr)); serde_json::from_slice::<Value>(&o.stdout).unwrap() };
    json_of(&f, &["maintenance", "hold", "add", "--class", "artefact.backups", "--reason", "keep"]);
    let created = run(&["backup", "create", "--out", out.to_str().unwrap(), "--encrypt-to", "age1fixturerecipient"]);
    assert_eq!(created["encrypted"], true);
    let mut names: Vec<String> = fs::read_dir(&out).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    names.sort();
    assert_eq!(names, ["manifest.json", "telemetry-ops.db.age", "telemetry.db.age"]);
    for file in files(&out) { assert!(!contains(&fs::read(&file).unwrap(), b"SQLite format 3"), "{} holds plaintext", file.display()); }
    let leftovers: Vec<PathBuf> = fs::read_dir(f.project.join(".state")).unwrap().map(|e| e.unwrap().path()).filter(|p| p.file_name().unwrap().to_string_lossy().starts_with(".backup")).collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    let refused = command(&f, &["backup", "restore", "--from", out.to_str().unwrap()], Some(&path), None);
    assert!(String::from_utf8_lossy(&refused.stderr).contains("pass --identity"));
    let identity = f.tmp.path().join("identity.txt");
    write_private(&identity, "AGE-SECRET-KEY-FIXTURE\n");
    let restored = run(&["backup", "restore", "--from", out.to_str().unwrap(), "--identity", identity.to_str().unwrap()]);
    assert_eq!((restored["rows"]["codex_usage"].clone(), restored["verified_files"].clone()), (json!(2), json!(2)));
    assert_eq!(f.count("codex_usage"), 2);
}

#[test]
fn backups_expire_from_the_inventory_unless_held() {
    let f = collected();
    let first = f.tmp.path().join("b1");
    let second = f.tmp.path().join("b2");
    let one = json_of(&f, &["backup", "create", "--out", first.to_str().unwrap()]);
    let two = json_of(&f, &["backup", "create", "--out", second.to_str().unwrap()]);
    assert_eq!(class(&plan(&f), "artefact.backups")["eligible_count"], 0, "30 days by default");
    write_private(&config(&f).join("telemetry-retention.toml"), "schema = \"telemetry-retention.v1\"\n[days]\n\"artefact.backups\" = 0\n");
    json_of(&f, &["maintenance", "hold", "add", "--class", "artefact.backups", "--scope", two["backup_id"].as_str().unwrap(), "--reason", "restore exercise"]);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let due = plan(&f);
    let backups = class(&due, "artefact.backups");
    assert_eq!(backups["eligible"].as_array().unwrap().iter().map(|e| e["key"].clone()).collect::<Vec<_>>(), [json!(format!("backup:{}", one["backup_id"].as_str().unwrap()))]);
    assert_eq!(backups["held"], json!([{"key": format!("backup:{}", two["backup_id"].as_str().unwrap()), "hold_id": "hold-1"}]));
    let digest = due["plan_digest"].as_str().unwrap().to_owned();
    json_of(&f, &["maintenance", "apply", "--confirm", &digest, "--json"]);
    assert!(!first.exists() && second.join("telemetry.db").is_file());
    let list = json_of(&f, &["backup", "list"]);
    let deleted: Vec<bool> = list["backups"].as_array().unwrap().iter().map(|b| !b["deleted_unix_ms"].is_null()).collect();
    assert_eq!(deleted, [true, false]);
}

#[test]
fn quarantines_spools_and_replay_repositories_are_cleaned_only_after_their_verdict() {
    let f = Fixture::new();
    let now = unix_ms();
    let old = now - 8 * DAY;
    plant_attempt(&f, "a-done", "completed", true, Some(("completed", old)));
    plant_attempt(&f, "a-fresh", "completed", true, Some(("completed", now - DAY)));
    plant_attempt(&f, "a-live", "running", false, None);
    plant_attempt(&f, "a-noverdict", "failed", true, Some(("failed", old)));
    plant_attempt(&f, "a-pending", "completed", true, Some(("completed", old)));
    plant_attempt(&f, "a-unobserved", "completed", false, Some(("completed", old)));
    let q = f.project.join(".git-quarantine");
    // Sizes: 11 + 38 = 49 bytes in 2 files.
    file(&q.join("a-done/repo-00/upper/objects/ab/cdef"), b"object-data");
    file(&q.join("a-done/repo-00/import.json"), br#"{"state":"refused","reason":"fixture"}"#);
    for attempt in ["a-fresh", "a-noverdict", "a-live", "a-unobserved"] { file(&q.join(attempt).join("repo-00/upper/HEAD"), b"ref"); }
    for attempt in ["a-fresh", "a-live", "a-unobserved"] { file(&q.join(attempt).join("repo-00/import.json"), b"{}"); }
    let spool = f.project.join(".state/spool");
    let (d1, d2) = ("1".repeat(64), "2".repeat(64));
    // 5 + 6 = 11 bytes.
    file(&spool.join("a-done").join(format!("{d1}.request")), b"req-1");
    file(&spool.join("a-done").join(format!("{d1}.receipt")), b"rcpt-1");
    file(&spool.join("a-pending").join(format!("{d2}.request")), b"req-2");
    for n in 0..257 { file(&spool.join("a-live").join(format!("{n:064}.request")), b"x"); }
    // Replay candidates (fixture rows): one finished 31 days ago, one still queued, one outside the replay root.
    let repos = f.root.join(".replay/demo/repos/s1/7");
    file(&repos.join("case-1/objects/pack"), b"pack-bytes");
    file(&repos.join("case-2/HEAD"), b"ref");
    file(&f.tmp.path().join("elsewhere/HEAD"), b"ref");
    let db = state(&f);
    db.execute("INSERT INTO replay_log(seq,kind,principal,authority,recorded_unix_ms) VALUES(6,'suite','operator:cli','replay_owner.v1',?1)", [now - 40 * DAY]).unwrap();
    db.execute("INSERT INTO replay_suites(seq,suite_version,extractor,manifest_digest,exclusions) VALUES(6,'s1','fixture',?1,'{}')", [format!("sha256:{}", "6".repeat(64))]).unwrap();
    for (ordinal, case) in ["case-1", "case-2", "case-3"].iter().enumerate() {
        db.execute("INSERT INTO replay_cases(seq,suite_version,case_id,ordinal,source_task_id,source_submission_id,source_result_id,contract_revision,contract_digest,
            repository,object_format,base_oid,reference_oid,integrated_oid,hidden_check_ref,hidden_checks,solution_paths,classification,stratum,contamination,status)
            VALUES(6,'s1',?1,?2,'work','sub','res',1,?3,'/repo','sha1',?4,?4,?4,?5,'[\"t\"]','[\"p\"]','{}','code','{}','eligible')",
            rusqlite::params![case, ordinal as i64 + 1, "c".repeat(64), "a".repeat(40), format!("sha256:{}", "5".repeat(64))]).unwrap();
    }
    db.execute("INSERT INTO replay_log(seq,kind,principal,authority,recorded_unix_ms) VALUES(7,'run','operator:cli','replay_owner.v1',?1)", [now - 31 * DAY]).unwrap();
    db.execute("INSERT INTO replay_runs(seq,run_id,suite_version,configuration,subset,seed,cases) VALUES(7,?1,'s1','cfg','all','seed','[\"case-1\",\"case-2\",\"case-3\"]')",
        [format!("sha256:{}", "7".repeat(64))]).unwrap();
    for (task, task_state, case, repository) in [("replay-a", "succeeded", "case-1", repos.join("case-1")), ("replay-b", "queued", "case-2", repos.join("case-2")),
        ("replay-c", "failed", "case-3", f.tmp.path().join("elsewhere"))] {
        db.execute("INSERT INTO tasks(id,revision,state,title) VALUES(?1,1,?2,?1)", [task, task_state]).unwrap();
        db.execute("INSERT INTO replay_candidates(task_id,run_id,suite_version,case_id,repository,registered_unix_ms) VALUES(?1,?2,'s1',?3,?4,?5)",
            rusqlite::params![task, format!("sha256:{}", "7".repeat(64)), case, repository.display().to_string(), now - 31 * DAY]).unwrap();
    }
    drop(db);
    let due = plan(&f);
    assert_eq!(class(&due, "artefact.git_quarantine")["eligible"], json!([{"key": "attempt:a-done", "bytes": 49, "files": 2, "age_days": 8}]));
    assert_eq!(class(&due, "artefact.git_quarantine")["blocked"], json!([{"key": "attempt:a-live", "reason": "attempt_not_terminal"},
        {"key": "attempt:a-noverdict", "reason": "import_verdict_missing"}, {"key": "attempt:a-unobserved", "reason": "termination_not_observed"}]));
    assert_eq!(class(&due, "artefact.submission_spool")["eligible"], json!([{"key": "attempt:a-done", "bytes": 11, "files": 2, "age_days": 8}]));
    assert_eq!(class(&due, "artefact.submission_spool")["blocked"], json!([{"key": "attempt:a-live", "reason": "attempt_not_terminal"},
        {"key": "attempt:a-pending", "reason": "request_without_receipt", "requests": 1}]));
    assert_eq!(class(&due, "artefact.replay_repos")["eligible"], json!([{"key": "task:replay-a", "bytes": 10, "files": 1, "age_days": 31}]));
    assert_eq!(class(&due, "artefact.replay_repos")["blocked"], json!([{"key": "task:replay-b", "reason": "task_not_terminal"},
        {"key": "task:replay-c", "reason": "outside_replay_root"}]));
    // Quota: the live attempt's spool is over its 256-entry cap (the ticker refuses it).
    let spools = due["quotas"]["spool"].as_array().unwrap();
    assert_eq!(spools.iter().find(|s| s["attempt"] == "a-live").unwrap()["state"], "over_limit");
    assert_eq!(spools.iter().find(|s| s["attempt"] == "a-done").unwrap(), &json!({"attempt": "a-done", "entries": 2, "bytes": 11, "limit_entries": 256, "state": "ok"}));
    assert!(ok(&f, &["maintenance", "plan"]).contains("quota spool a-live entries=257 limit=256 over_limit\n"));
    let digest = due["plan_digest"].as_str().unwrap().to_owned();
    let applied = json_of(&f, &["maintenance", "apply", "--confirm", &digest, "--json"]);
    assert_eq!(applied["deleted"], json!({"artefact.git_quarantine": 1, "artefact.replay_repos": 1, "artefact.submission_spool": 1}));
    assert!(!q.join("a-done").exists() && !spool.join("a-done").exists() && !repos.join("case-1").exists());
    for kept in [q.join("a-noverdict"), q.join("a-fresh"), spool.join("a-pending"), repos.join("case-2"), f.tmp.path().join("elsewhere")] { assert!(kept.exists(), "{}", kept.display()); }
    assert!(spool.join("a-pending").join(format!("{d2}.request")).is_file(), "an undisposed request is never retired");
    assert_eq!(plan(&f)["tombstones"]["by_class"], json!({"artefact.git_quarantine": 1, "artefact.replay_repos": 1, "artefact.submission_spool": 1}));
}

#[test]
fn a_spool_retired_before_ingestion_is_reported_not_lost() {
    let f = Fixture::new();
    let spool = f.project.join(".state/spool").join(&f.attempt);
    fs::create_dir_all(&spool).unwrap();
    let document = f.tmp.path().join("result.json");
    fs::write(&document, format!("{{\"attempt_id\":\"{}\"}}", f.attempt)).unwrap();
    let worker = Command::new(BIN).env_clear().env("HOME", f.tmp.path().join("home")).env("PATH", "/usr/bin:/bin")
        .env("HERDR_PROJECTS_SUBMISSION_SPOOL", &spool).args(["--root", f.root.to_str().unwrap(), "result", "demo", "submit", "--input-file", document.to_str().unwrap()])
        .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !fs::read_dir(&spool).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().ends_with(".request")) {
        assert!(std::time::Instant::now() < deadline, "no request written");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // Maintenance never retires it: the attempt is live and the request has no receipt.
    let due = plan(&f);
    assert_eq!(class(&due, "artefact.submission_spool")["blocked"], json!([{"key": format!("attempt:{}", f.attempt), "reason": "attempt_not_terminal"}]));
    json_of(&f, &["maintenance", "apply", "--json"]);
    assert!(spool.is_dir());
    // Retired out of band before the ticker ingested it: the worker's command says so.
    let started = std::time::Instant::now();
    fs::remove_dir_all(&spool).unwrap();
    let out = worker.wait_with_output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("lost request") && stderr.contains("nothing was submitted"), "{stderr}");
    assert!(started.elapsed() < std::time::Duration::from_secs(10), "reported at once, not after the receipt wait");
    let submissions: i64 = state(&f).query_row("SELECT count(*) FROM result_submissions", [], |r| r.get(0)).unwrap();
    assert_eq!(submissions, 0);
}

#[test]
fn opt_in_export_evidence_expires() {
    let f = collected();
    let outbox = f.tmp.path().join("outbox");
    fs::create_dir(&outbox).unwrap();
    fs::set_permissions(&outbox, fs::Permissions::from_mode(0o700)).unwrap();
    let dir = config(&f);
    write_private(&dir.join("telemetry-export.toml"), &format!("schema = \"telemetry-export-config.v1\"\n[external]\nenabled = true\ndestination = \"directory\"\ndirectory = \"{}\"\n", outbox.display()));
    // One real export, and pages written earlier (fixture ages).
    let written = json_of(&f, &["export", "--metric", "M08", "--external"]);
    let fresh = written["written"][0]["file"].as_str().unwrap().to_owned();
    let now_s = unix_ms() / 1000;
    for (name, days) in [("demo-0123456789abcdef-0.json", 8), ("demo-fedcba9876543210-0.csv", 2), ("demo-fedcba9876543210-0.csv.manifest.json", 2),
        ("other-0123456789abcdef-0.json", 8), ("notes.txt", 30)] {
        write_private(&outbox.join(name), "{}");
        let touched = Command::new("touch").args(["-d", &format!("@{}", now_s - days * 86_400), outbox.join(name).to_str().unwrap()]).status().unwrap();
        assert!(touched.success());
    }
    let due = plan(&f);
    assert_eq!(class(&due, "optin.external_export_files")["eligible"], json!([{"key": "file:demo-0123456789abcdef-0.json", "files": 1, "age_days": 8}]));
    // A project override may only shorten it.
    write_private(&dir.join("telemetry-retention.toml"), "schema = \"telemetry-retention.v1\"\n[projects.demo.days]\n\"optin.external_export_files\" = 10\n");
    assert!(fail(&f, &["maintenance", "plan"]).contains("may only be shortened"));
    write_private(&dir.join("telemetry-retention.toml"), "schema = \"telemetry-retention.v1\"\n[projects.demo.days]\n\"optin.external_export_files\" = 1\n");
    let due = plan(&f);
    let exports = class(&due, "optin.external_export_files");
    assert_eq!((exports["retention_days"].clone(), exports["policy_source"].clone()), (json!(1), json!("project_override")));
    assert_eq!(exports["eligible"], json!([{"key": "file:demo-0123456789abcdef-0.json", "files": 1, "age_days": 8},
        {"key": "file:demo-fedcba9876543210-0.csv", "files": 2, "age_days": 2}]));
    let digest = due["plan_digest"].as_str().unwrap().to_owned();
    json_of(&f, &["maintenance", "apply", "--confirm", &digest, "--json"]);
    let mut left: Vec<String> = fs::read_dir(&outbox).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    left.sort();
    let mut expected = vec![fresh, "notes.txt".to_owned(), "other-0123456789abcdef-0.json".to_owned()];
    expected.sort();
    assert_eq!(left, expected);
    // Disabled export: its directory is no longer the deployment's, nothing is touched.
    write_private(&dir.join("telemetry-export.toml"), "schema = \"telemetry-export-config.v1\"\n");
    write_private(&dir.join("telemetry-retention.toml"), "schema = \"telemetry-retention.v1\"\n[days]\n\"optin.external_export_files\" = 0\n");
    assert_eq!(class(&plan(&f), "optin.external_export_files")["eligible_count"], 0);
}

#[test]
fn secret_canaries_never_reach_persisted_metadata() {
    let f = collected();
    // Canaries in the rollouts (fixture lines: instructions, messages,
    // reasoning, last agent message, credits), in operator input and in configs.
    let dir = config(&f);
    write_private(&dir.join("telemetry-retention.toml"), "# CANARY_CONFIG sk-canaryRetentionConfig0010\nschema = \"telemetry-retention.v1\"\n");
    f.cli_args(&["analytics", "refresh"]);
    f.cli_args(&["health", "evaluate", "--json"]);
    assert!(fail(&f, &["maintenance", "hold", "add", "--class", "all", "--scope", "sk-canaryHoldScope0011", "--reason", "x"]).contains("must not look like a credential"));
    json_of(&f, &["maintenance", "hold", "add", "--class", "all", "--reason", "incident Bearer sk-canaryBearer0012 token=sk-canaryToken0013"]);
    let backup = f.tmp.path().join("canary-backup");
    json_of(&f, &["backup", "create", "--out", backup.to_str().unwrap()]);
    json_of(&f, &["backup", "restore", "--from", backup.to_str().unwrap(), "--force"]);
    remove_sidecar(&f);
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    f.cli_args(&["analytics", "refresh"]);
    let backup2 = f.tmp.path().join("canary-backup-2");
    json_of(&f, &["backup", "create", "--out", backup2.to_str().unwrap()]);
    let mut scanned = 0;
    for file in files(&f.project).into_iter().chain(files(&backup)).chain(files(&backup2)) {
        let bytes = fs::read(&file).unwrap();
        for canary in [&b"CANARY"[..], b"sk-canary"] { assert!(!contains(&bytes, canary), "{} holds a canary", file.display()); }
        scanned += 1;
    }
    assert!(scanned >= 6, "sidecar, ops store and both backups were scanned");
    assert_eq!(f.count("codex_usage"), 2);
}

/// Replace every `sha256:` + 64 hex digits, and every run of exactly 13 digits (Unix ms).
fn normalize(text: &str, f: &Fixture) -> String {
    let base = fs::canonicalize(f.tmp.path()).unwrap();
    let mut text = text.replace(&base.display().to_string(), "<tmp>").replace(&f.tmp.path().display().to_string(), "<tmp>").replace(&f.attempt, "<attempt>");
    let mut out = String::new();
    while let Some(at) = text.find("sha256:") {
        let hex = &text[at + 7..];
        let n = hex.bytes().take_while(u8::is_ascii_hexdigit).count();
        out += &text[..at + 7];
        out += if n == 64 { "<digest>" } else { "" };
        text = text[at + 7 + if n == 64 { 64 } else { 0 }..].to_owned();
    }
    out += &text;
    let (mut result, mut digits) = (String::new(), String::new());
    for c in out.chars().chain(std::iter::once(' ')) {
        if c.is_ascii_digit() { digits.push(c); continue; }
        result += if digits.len() == 13 { "<unix_ms>" } else { &digits };
        digits.clear();
        result.push(c);
    }
    result.pop();
    // SQLite file sizes and stream versions (other lanes add migrations) are not part of the procedure.
    let mut streams = false;
    result.lines().map(|l| {
        let comma = if l.ends_with(',') { "," } else { "" };
        if l.trim_start().starts_with("\"streams\": {") { streams = true; return l.to_owned(); }
        if streams && l.trim_start().starts_with('}') { streams = false; }
        match (l.find("\"bytes\": "), streams) {
            (Some(at), _) => format!("{}\"bytes\": <bytes>{comma}", &l[..at]),
            (None, true) => format!("{}: <version>{comma}", l.split(':').next().unwrap()),
            (None, false) => l.to_owned(),
        }
    }).collect::<Vec<_>>().join("\n") + "\n"
}

const RUNBOOK: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/telemetry/operations-runbook.md");

/// The runbook's command transcripts are this fixture run's real output
/// (normalized: digests, Unix ms, SQLite byte sizes, the temporary root and
/// the attempt id). `HERDR_RUNBOOK_WRITE=1` rewrites them in place.
#[test]
fn runbook_transcripts_are_the_fixture_run() {
    let f = collected();
    let mut transcripts: Vec<(&str, String)> = Vec::new();
    let mut run = |name: &'static str, args: &[&str], expect_ok: bool| -> String {
        let out = command(&f, args, None, None);
        assert_eq!(out.status.success(), expect_ok, "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        let shown = args.iter().map(|a| if a.contains(' ') { format!("\"{a}\"") } else { (*a).to_owned() }).collect::<Vec<_>>().join(" ");
        let text = format!("$ herdr-projects telemetry demo {shown}\n{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        transcripts.push((name, normalize(&text, &f)));
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let backups = f.tmp.path().join("backup-2026-09-30");
    run("classes", &["maintenance", "classes"], true);
    run("backup-create", &["backup", "create", "--out", backups.to_str().unwrap()], true);
    // The session's acceptance is 91 days old and its attempt ended (fixture).
    f.sidecar().execute("UPDATE rollout_sources SET observed_unix_ms=observed_unix_ms-?1", [91 * DAY]).unwrap();
    f.cancel_reserved();
    let plan_text = run("plan", &["maintenance", "plan"], true);
    let digest = plan_text.split("digest=").nth(1).unwrap().split_whitespace().next().unwrap().to_owned();
    run("apply-unconfirmed", &["maintenance", "apply"], false);
    run("hold-add", &["maintenance", "hold", "add", "--class", "sidecar.normalized_sessions", "--scope", SID, "--reason", "litigation hold 42"], true);
    run("plan-held", &["maintenance", "plan"], true);
    run("hold-release", &["maintenance", "hold", "release", "hold-1", "--reason", "released by counsel"], true);
    run("apply", &["maintenance", "apply", "--confirm", &digest], true);
    run("backup-verify", &["backup", "verify", "--from", backups.to_str().unwrap()], true);
    run("restore", &["backup", "restore", "--from", backups.to_str().unwrap()], true);
    assert_eq!(f.count("codex_usage"), 0);
    let doc = fs::read_to_string(RUNBOOK).unwrap();
    let mut rewritten = String::new();
    let mut rest = doc.as_str();
    let mut seen = Vec::new();
    while let Some(at) = rest.find("<!-- transcript: ") {
        let name_end = rest[at..].find(" -->").unwrap() + at;
        let name = &rest[at + 17..name_end];
        let open = rest[name_end..].find("```text\n").unwrap() + name_end + 8;
        let close = rest[open..].find("```\n").unwrap() + open;
        let actual = &transcripts.iter().find(|(n, _)| *n == name).unwrap_or_else(|| panic!("unknown transcript {name}")).1;
        if std::env::var_os("HERDR_RUNBOOK_WRITE").is_none() {
            assert_eq!(&rest[open..close], actual.as_str(), "runbook transcript {name} differs from the fixture run");
        }
        rewritten += &rest[..open];
        rewritten += actual;
        rest = &rest[close..];
        seen.push(name.to_owned());
    }
    rewritten += rest;
    let mut expected: Vec<String> = transcripts.iter().map(|(n, _)| n.to_string()).collect();
    expected.sort();
    seen.sort();
    assert_eq!(seen, expected, "every transcript appears once");
    if std::env::var_os("HERDR_RUNBOOK_WRITE").is_some() { fs::write(RUNBOOK, rewritten).unwrap(); }
}

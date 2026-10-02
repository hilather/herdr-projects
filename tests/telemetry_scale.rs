//! TM5.1 scale, overhead and fault certification (docs/telemetry/certificate-scale.md,
//! plan doc 10 §6).
//!
//! One deterministic workload generator serves two uses:
//!
//! - `scale_gates_hold_under_load` (runs in normal CI time): a small
//!   project (200 bindings, 32 active attempts, about 5,000 rollout events)
//!   driven through burst ingestion, SQLite contention (racing collects,
//!   syncs, refreshes and export paging), a full sidecar (the collector's
//!   spool: a file-size limit), a stalled exporter and collectors killed
//!   mid-pass. The correctness gates must hold with zero violations: usage
//!   equals the generator's independent totals and is accepted once per
//!   record, as-of answers are reproducible, a sidecar rebuilt from scratch
//!   holds a byte-identical ledger, and telemetry never writes `state.db`.
//! - `scale_*` benches (`#[ignore]`): the doc 10 matrix (10,000 bindings,
//!   32/64 active attempts, 100,000 and 1,000,000 events) in phases that each
//!   stay under ten minutes and share one prepared dataset under
//!   `SCALE_DATA`. Each writes `results-<phase>.json` there. Run them with a
//!   release build, one test thread, `TMPDIR` on disk and `PATH` without a
//!   real `herdr` (docs/telemetry/certificate-scale.md §2 has the commands).
//!
//! The expected usage is the generator's own sum of the counters it wrote,
//! never read back from a production aggregate. Simulated attempts are
//! planted canonical rows, not launched agents: nothing here certifies
//! live-agent capacity.

#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.

mod support;

/// SQLite's memory statistics are off in this process before any SQLite use,
/// as in the `herdr-farm` binary (src/main.rs), whose ticker hosts the
/// telemetry pass on a thread beside the controller: with them on, every
/// SQLite allocation of every thread takes one process-wide mutex.
/// `SCALE_SQLITE_MEMSTATUS=1` keeps them on, to measure that contention.
#[used]
#[unsafe(link_section = ".init_array")]
static SQLITE_MEMSTATUS_HOOK: unsafe extern "C" fn() = sqlite_memstatus_from_env;

unsafe extern "C" fn sqlite_memstatus_from_env() {
    // SAFETY: runs from .init_array before main and before any SQLite use; getenv reads a NUL-terminated name.
    unsafe {
        let value = libc::getenv(c"SCALE_SQLITE_MEMSTATUS".as_ptr());
        if value.is_null() || *value != b'1' as libc::c_char { rusqlite::ffi::sqlite3_config(rusqlite::ffi::SQLITE_CONFIG_MEMSTATUS, 0); }
    }
}

use herdr_farm::telemetry::{self, codex};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, io::{Read, Write}, os::unix::process::CommandExt, path::{Path, PathBuf}, process::{Command, Stdio},
    sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}}, time::{Duration, Instant}};
use support::telemetry::*;

// ---------------------------------------------------------------------------
// Deterministic generator

/// splitmix64: a fixed, seedable sequence (no crate).
#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// Uniform in `lo..=hi`.
    fn range(&mut self, lo: i64, hi: i64) -> i64 { lo + (self.next() % (hi - lo + 1) as u64) as i64 }
}

/// Turns per rollout and lines per turn: a session is `1 + TURNS * TURN_LINES` events.
const TURNS: usize = 10;
const TURN_LINES: usize = 10;
const SESSION_EVENTS: usize = 1 + TURNS * TURN_LINES;
const CLASSES: [&str; 6] = ["code", "dependency_change", "docs", "read_only", "schema_change", "tests"];
const WORDS: [&str; 16] = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india", "juliet", "kilo", "lima", "mike", "november", "oscar", "papa"];

#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
struct Scale { attempts: usize, active: usize, events: usize, homes: usize, seed: u64 }

/// The generator's independent usage totals: the counters it wrote.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Totals { input: i64, cached: i64, output: i64, reasoning: i64, records: i64 }

/// Event mix and bytes by line kind.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
struct Mix { counts: std::collections::BTreeMap<String, u64>, bytes: std::collections::BTreeMap<String, u64> }

impl Mix {
    fn add(&mut self, kind: &str, bytes: usize) {
        *self.counts.entry(kind.into()).or_default() += 1;
        *self.bytes.entry(kind.into()).or_default() += bytes as u64;
    }
    fn events(&self) -> u64 { self.counts.values().sum() }
    fn total_bytes(&self) -> u64 { self.bytes.values().sum() }
}

fn iso(ms: i64) -> String { jiff::Timestamp::from_millisecond(ms).unwrap().to_string() }

fn text(rng: &mut Rng, lo: i64, hi: i64) -> String {
    let len = rng.range(lo, hi) as usize;
    let mut out = String::with_capacity(len + 12);
    while out.len() < len { out.push_str(WORDS[(rng.next() % 16) as usize]); out.push(' '); }
    out.truncate(len);
    out
}

/// One Codex 0.154.0 rollout being written, line by line, in the shapes of
/// contracts §5 (live-certified keys, plus the bounded free text real
/// rollouts carry and the collector must skip).
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Session { path: PathBuf, sid: String, cwd: String, model: String, turn: usize, step: usize, cum: [i64; 4], last: [i64; 4], rng: Rng }

fn counters([input, cached, output, reasoning]: [i64; 4]) -> Value {
    json!({"input_tokens": input, "cached_input_tokens": cached, "cache_write_input_tokens": 0, "output_tokens": output,
        "reasoning_output_tokens": reasoning, "total_tokens": input + output})
}

impl Session {
    fn meta(&self, ts: i64) -> String {
        json!({"timestamp": iso(ts), "type": "session_meta", "payload": {"id": self.sid, "session_id": self.sid, "timestamp": iso(ts), "cwd": self.cwd,
            "originator": "codex_exec", "cli_version": "0.154.0", "source": "exec", "thread_source": "user", "model_provider": "openai"}}).to_string()
    }

    /// The next line of the turn cycle at time `ts`, adding usage to `totals`.
    fn line(&mut self, ts: i64, totals: &mut Totals, mix: &mut Mix) -> String {
        if self.step == 0 { self.turn += 1; }
        let (turn, sid) = (format!("turn-{}", self.turn), self.sid.clone());
        let (kind, value) = match self.step {
            0 => ("turn_context", json!({"type": "turn_context", "payload": {"turn_id": turn, "cwd": self.cwd, "approval_policy": "never", "model": self.model, "effort": "medium"}})),
            1 => ("response_item.message", json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": text(&mut self.rng, 64, 1024)}]}})),
            2 => ("response_item.custom_tool_call", json!({"type": "response_item", "payload": {"type": "custom_tool_call", "call_id": format!("call-{}", self.turn), "name": "exec",
                "status": "completed", "input": text(&mut self.rng, 32, 512), "internal_chat_message_metadata_passthrough": {"turn_id": turn}}})),
            3 => ("event_msg.item_completed", json!({"type": "event_msg", "payload": {"type": "item_completed", "thread_id": sid, "turn_id": turn,
                "item": {"type": "CommandExecution", "id": format!("exec-{}", self.turn), "status": "completed", "source": "unified_exec_startup", "exit_code": 0, "duration": {"secs": 0, "nanos": 2000}}}})),
            4 => ("response_item.custom_tool_call_output", json!({"type": "response_item", "payload": {"type": "custom_tool_call_output", "call_id": format!("call-{}", self.turn),
                "output": text(&mut self.rng, 64, 2048)}})),
            5 | 7 => {
                let input = self.rng.range(500, 20_000);
                let cached = self.rng.range(0, input / 2);
                let output = self.rng.range(50, 2_000);
                let reasoning = self.rng.range(0, output / 2);
                self.last = [input, cached, output, reasoning];
                for (c, v) in self.cum.iter_mut().zip(self.last) { *c += v; }
                *totals = Totals { input: totals.input + input, cached: totals.cached + cached, output: totals.output + output, reasoning: totals.reasoning + reasoning, records: totals.records + 1 };
                let response = format!("resp-{}-{}", self.turn, if self.step == 5 { 1 } else { 2 });
                ("token_usage_record", json!({"type": "token_usage_record", "payload": {"thread_id": sid, "session_id": sid, "turn_id": turn, "root_turn_id": turn,
                    "response_id": response, "usage": counters(self.last), "thread_token_usage": counters(self.cum)}}))
            }
            6 => ("response_item.reasoning", json!({"type": "response_item", "payload": {"type": "reasoning", "summary": [{"type": "summary_text", "text": text(&mut self.rng, 32, 512)}]}})),
            8 => {
                // A 300-minute window on fixed boundaries: used % rises with time, so
                // every session of one execution home reports one consistent window.
                let window = 300 * 60_000;
                let start = ts - ts.rem_euclid(window);
                let used = ((ts - start) * 1000 / window) as f64 / 10.0;
                ("event_msg.token_count", json!({"type": "event_msg", "payload": {"type": "token_count",
                    "info": {"total_token_usage": counters(self.cum), "last_token_usage": counters(self.last), "model_context_window": 272_000},
                    "rate_limits": {"limit_id": "codex", "limit_name": null, "plan_type": "pro", "primary": {"used_percent": used, "window_minutes": 300, "resets_at": (start + window) / 1000},
                        "secondary": null, "rate_limit_reached_type": null}}}))
            }
            _ => ("event_msg.task_complete", json!({"type": "event_msg", "payload": {"type": "task_complete", "turn_id": turn, "duration_ms": 4200, "time_to_first_token_ms": 350}})),
        };
        self.step = (self.step + 1) % TURN_LINES;
        let mut value = value;
        value.as_object_mut().unwrap().insert("timestamp".into(), json!(iso(ts)));
        let line = value.to_string();
        mix.add(kind, line.len() + 1);
        line
    }

    /// Append `n` lines stamped now; returns each usage record's append time.
    fn append(&mut self, n: usize, totals: &mut Totals, mix: &mut Mix) -> Vec<i64> {
        let mut text = String::new();
        let mut usage = Vec::new();
        for _ in 0..n {
            let now = unix_ms();
            let is_usage = self.step == 5 || self.step == 7;
            text += &self.line(now, totals, mix);
            text.push('\n');
            if is_usage { usage.push(now); }
        }
        fs::OpenOptions::new().append(true).open(&self.path).unwrap().write_all(text.as_bytes()).unwrap();
        usage
    }
}

fn hex(seed: &str) -> String { format!("{:x}", Sha256::digest(seed.as_bytes())) }

/// An `agent_configuration.v1` row (as tests/telemetry_compare.rs), one per variant.
fn configuration(db: &rusqlite::Connection, variant: usize) -> String {
    let canonical = json!({"adapter": {"digest": "a".repeat(64), "id": "sim-adapter", "revision": 1}, "agent_digest": "e".repeat(64), "agent_version": "0.154.0",
        "arguments_digest": format!("{variant}").repeat(64), "definition_digest": "b".repeat(64), "environment_names": [], "kind": "codex",
        "permission_policy": {"digest": "d".repeat(64), "id": "sim-policy", "revision": 1}, "reasoning_effort": null, "reasoning_effort_reason": "mapping_unverified",
        "requested_model": null, "requested_model_reason": "mapping_unverified", "schema": "agent_configuration.v1"}).to_string();
    let id = format!("sha256:{}", hex(&canonical));
    db.execute("INSERT OR IGNORE INTO agent_configurations(configuration_id,canonical_json,first_decided_unix_ms) VALUES(?1,?2,1)", [&id, &canonical]).unwrap();
    id
}

/// The dataset: canonical rows, rollouts and the generator's state, kept in
/// `manifest.json` so later phases continue the same sessions.
#[derive(serde::Serialize, serde::Deserialize)]
struct Dataset { base: PathBuf, root: PathBuf, project: PathBuf, attempt: String, config_digest: Option<String>, binding: (String, u64, Option<u64>), scale: Scale, totals: Totals, mix: Mix, active: Vec<Session>, generated_ms: f64, #[serde(default)] producers: Producers }

impl Dataset {
    fn manifest(dir: &Path) -> PathBuf { dir.join("manifest.json") }
    fn load(dir: &Path) -> Self { serde_json::from_slice(&fs::read(Self::manifest(dir)).unwrap()).unwrap() }
    fn save(&self, dir: &Path) { fs::write(Self::manifest(dir), serde_json::to_vec_pretty(self).unwrap()).unwrap(); }
    fn home(&self) -> PathBuf { self.base.join("home") }
    fn state(&self) -> PathBuf { self.project.join(".state/state.db") }
    fn sidecar(&self) -> PathBuf { self.project.join(".state/telemetry.db") }
}

/// Plant `scale.attempts` canonical Codex attempts on the fixture project
/// (`scale.active` running, the rest terminal: 60 % accepted, 30 % failed,
/// 10 % cancelled), each with an active collector binding (every 20th also a
/// later revocation) and a dispatch decision, and write their rollouts:
/// every active attempt and enough terminal ones to reach `scale.events`.
/// Planted rows bypass the store (foreign keys off) as the other telemetry
/// suites plant fleets; no attempt is launched.
fn generate(f: Fixture, scale: Scale) -> Dataset {
    let started = Instant::now();
    let now = unix_ms();
    // The fixture's one runtime binding, read before the planted rows (the store's
    // snapshot decodes every operation; planted ones carry no launch payload).
    let snapshot = herdr_farm::store::SqliteStore::open(&f.project.join(".state/state.db")).unwrap().read_snapshot(None).unwrap();
    let binding = snapshot.runtime_bindings[0].clone();
    let observed = (binding.id.clone(), binding.revision, snapshot.tasks.iter().find(|t| Some(&t.id) == binding.task.as_ref()).map(|t| t.revision));
    let homes: Vec<PathBuf> = (0..scale.homes).map(|h| f.tmp.path().join(format!("codex-homes/h{h}"))).collect();
    let db = rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap();
    db.execute_batch("PRAGMA foreign_keys=OFF; BEGIN").unwrap();
    let configs: Vec<String> = (0..4).map(|v| configuration(&db, v)).collect();
    let terminal = scale.attempts - scale.active;
    let sessions = (scale.events / SESSION_EVENTS).max(scale.active);
    let history = sessions - scale.active;
    let mut rng = Rng(scale.seed);
    let mut plan = Vec::new();
    let mut producers = Producers::default();
    for i in 0..scale.attempts {
        let active = i >= terminal;
        let (task, attempt, op) = (format!("st{i:05}"), format!("s{i:05}"), format!("op-s{i:05}"));
        let home = homes[i % scale.homes].display().to_string();
        let decided = if active { now - 3_600_000 + (i - terminal) as i64 * 1_000 } else { now - 31 * 86_400_000 + i as i64 * (30 * 86_400_000 / scale.attempts as i64) };
        let outcome = if active { "running" } else { match rng.next() % 10 { 0..=5 => "accepted", 6..=8 => "failed", _ => "cancelled" } };
        let (task_state, attempt_state) = match outcome { "running" => ("running", "running"), "accepted" => ("succeeded", "completed"), "failed" => ("failed", "failed"), _ => ("cancelled", "cancelled") };
        let class = CLASSES[i % CLASSES.len()];
        db.execute("INSERT INTO tasks(id,revision,state,title) VALUES(?1,1,?2,?1)", [&task, task_state]).unwrap();
        db.execute("INSERT INTO task_contracts(task_id,contract_revision,project_store,expected_head,repository,base_oid,object_format,route,raw_bytes,raw_digest,installed_seq)
            VALUES(?1,1,'store',1,'/repo',?2,'sha1','verify_only',x'7b7d',?3,1)", rusqlite::params![task, "a".repeat(40), hex(&format!("contract-{task}"))]).unwrap();
        let classification = format!("sha256:{}", hex(&format!("class-{task}")));
        db.execute("INSERT INTO task_classifications(classification_id,task_id,contract_revision,taxonomy,class,band,features,classifier,revision,reason,created_unix_ms)
            VALUES(?1,?2,1,'taxonomy.v1',?3,'small','{}','rule:fixture',1,NULL,?4)", rusqlite::params![classification, task, class, decided - 1_000]).unwrap();
        db.execute("INSERT INTO operations(id,task_id,kind,target,payload_version,payload,payload_hash,expected_revision,due_unix_ms,idempotency_key) VALUES(?1,?2,'attempt.launch','sim',1,'{}',?3,1,0,?1)",
            rusqlite::params![op, task, hex("{}")]).unwrap();
        db.execute("INSERT INTO attempts(id,task_id,revision,state,reservation,termination_observed) VALUES(?1,?2,?3,?4,?1,?5)",
            rusqlite::params![attempt, task, if active { 1 } else { 2 }, attempt_state, !active]).unwrap();
        let inputs = json!({"inputs": {"version": 2, "effective_profile": {"kind": "codex", "execution_home": home}}}).to_string();
        db.execute("INSERT INTO attempt_inputs(attempt_id,operation_id,payload,payload_hash) VALUES(?1,?2,?3,?4)", rusqlite::params![attempt, op, inputs, hex(&inputs)]).unwrap();
        let config = &configs[i % configs.len()];
        let eligible = json!([{"configuration_id": config, "profile_digest": "0".repeat(64), "status": "chosen", "probability_ppm": 1_000_000}]).to_string();
        db.execute("INSERT INTO dispatch_decisions(attempt_id,task_id,task_revision,contract_revision,classification_id,chosen_configuration_id,eligible,chooser_kind,chooser_principal,reason_codes,note,policy,seed,decided_unix_ms)
            VALUES(?1,?2,1,1,?3,?4,?5,'automatic_admission','admission:sim','[\"unspecified\"]',NULL,NULL,NULL,?6)", rusqlite::params![attempt, task, classification, config, eligible, decided]).unwrap();
        db.execute("INSERT INTO collector_bindings(attempt_id,revision,state,collector,execution_home,unix_ms,source) VALUES(?1,1,'active','codex',?2,?3,'apply_launch_started')",
            rusqlite::params![attempt, home, decided + 500]).unwrap();
        if !active && i % 20 == 0 {
            db.execute("INSERT INTO collector_bindings(attempt_id,revision,state,collector,execution_home,unix_ms,source) VALUES(?1,2,'revoked','codex',?2,?3,'sim_revocation')",
                rusqlite::params![attempt, home, decided + 86_400_000]).unwrap();
        }
        db.execute("INSERT INTO attempt_lifecycle(attempt_id,state,attempt_revision,unix_ms,source) VALUES(?1,'reserved',1,?2,'sim')", rusqlite::params![attempt, decided]).unwrap();
        db.execute("INSERT INTO attempt_lifecycle(attempt_id,state,attempt_revision,unix_ms,source) VALUES(?1,'running',1,?2,'sim')", rusqlite::params![attempt, decided + 500]).unwrap();
        if active {
            // A launch receipt whose Herdr socket does not exist: the attention
            // sampler reads the binding and records a gap, never calling Herdr.
            let receipt = json!({"version": 2, "attempt": attempt, "operation": op, "route": {"machine": "", "socket": "/nonexistent/herdr-scale.sock",
                "workspace_id": "w1", "tab_id": "w1:t1", "pane_id": format!("w1:p{i}"), "cwd": "/"}, "terminal": "term", "session": {"device": 1, "inode": i, "born_secs": 3, "born_nanos": 4},
                "agent": {"kind": "codex", "name": "worker"}, "observed_unix_ms": decided + 500});
            db.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('runtime.launch_started',?1,1,1,?2)", rusqlite::params![op, receipt.to_string()]).unwrap();
        } else {
            let end = decided + 20 * 60_000;
            db.execute("INSERT INTO attempt_lifecycle(attempt_id,state,attempt_revision,unix_ms,source) VALUES(?1,?2,2,?3,'sim')", rusqlite::params![attempt, attempt_state, end]).unwrap();
            if outcome == "accepted" {
                let (submission, result) = (hex(&format!("submission-{task}")), hex(&format!("result-{task}")));
                db.execute("INSERT INTO result_submissions(submission_id,project_store,idempotency_key,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,artifact_manifest,claimed_checks,created_unix_ms)
                    VALUES(?1,'store',?1,?2,'{}',?3,1,?2,?4,'/repo',?5,?5,'sha1','[]','[]',?6)", rusqlite::params![submission, hex("d"), task, attempt, "a".repeat(40), end - 10_000]).unwrap();
                db.execute("INSERT INTO acceptance_policies(task_id,contract_revision,policy_id,body) VALUES(?1,1,'sim-policy','{}')", [&task]).unwrap();
                db.execute("INSERT INTO verification_runs(run_id,project_store,idempotency_key,payload_digest,submission_id,task_id,contract_revision,contract_digest,attempt_id,
                    policy_id,policy_digest,commit_oid,tree_oid,object_format,memory_fence,isolation,argv,library_manifest,state,reason,exit_status,receipt_digest,store_device,store_inode,created_unix_ms)
                    VALUES(?1,'store',?2,?3,?4,?5,1,?3,?6,'sim-policy',?7,?8,?8,'sha1',0,'linux-unshare-user-pid-mount-v1','[\"true\"]','[]','accepted',NULL,0,?7,1,1,?9)",
                    rusqlite::params![result, &result[..32], hex("d"), submission, task, attempt, hex("e"), "a".repeat(40), end - 1_000]).unwrap();
                db.execute("INSERT INTO verified_results(result_id,run_id,submission_id,commit_oid,tree_oid,object_format,policy_digest,receipt_digest,isolation,memory_fence,created_unix_ms)
                    VALUES(?1,?1,?2,?3,?3,'sha1',?4,?4,'linux-unshare-user-pid-mount-v1',0,?5)", rusqlite::params![result, submission, "a".repeat(40), hex("e"), end]).unwrap();
                let flagged = i.is_multiple_of(5);
                producers.proxies.push(Proxy { task: task.clone(), attempt: attempt.clone(), submission: submission.clone(), run: result.clone(), at: end - 1_000, flagged });
                if i.is_multiple_of(10) && let Some(outcome) = plant_integration(&db, &task, &op, &result, end, i, now) { producers.outcomes.push(outcome); }
            }
        }
        // History rollouts evenly spaced over the terminal attempts (every home gets some).
        let with_rollout = active || (history > 0 && (i * history) % terminal < history);
        if with_rollout { plan.push((i, attempt, homes[i % scale.homes].clone(), decided, active)); }
    }
    db.execute_batch("COMMIT").unwrap();
    let violations: i64 = db.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r.get(0)).unwrap();
    assert_eq!(violations, 0, "planted rows keep every foreign key");
    drop(db);
    let (mut totals, mut mix, mut active) = (Totals::default(), Mix::default(), Vec::new());
    for (i, attempt, home, decided, is_active) in plan {
        let dir = home.join(".codex/sessions/2026/09/28");
        fs::create_dir_all(&dir).unwrap();
        let sid = format!("00000000-0000-4000-8000-{i:012}");
        let mut s = Session { path: dir.join(format!("rollout-2026-09-28T00-00-00-{sid}.jsonl")), sid, cwd: format!("{}/.state/worktrees/{attempt}/repo-00", f.project.display()),
            model: if i % 3 == 0 { "gpt-5.5-mini".into() } else { "gpt-5.5".into() }, turn: 0, step: 0, cum: [0; 4], last: [0; 4], rng: Rng(scale.seed ^ (i as u64).wrapping_mul(0x9E37_79B9)) };
        let start = decided + 1_000;
        let mut out = s.meta(start);
        mix.add("session_meta", out.len() + 1);
        out.push('\n');
        // An active session stops before its last turn's `task_complete`: it is mid-turn.
        let lines = if is_active { SESSION_EVENTS - 2 } else { SESSION_EVENTS - 1 };
        for n in 0..lines {
            let ts = start + (n / TURN_LINES) as i64 * 60_000 + (n % TURN_LINES) as i64 * 500 + 500;
            out += &s.line(ts, &mut totals, &mut mix);
            out.push('\n');
        }
        fs::write(&s.path, out).unwrap();
        if is_active { active.push(s); }
    }
    for i in terminal..scale.attempts {
        producers.attention.push((format!("s{i:05}"), now - 3_600_000 + (i - terminal) as i64 * 1_000 + 1_000));
    }
    record_producer_mix(&mut mix, &producers);
    let Fixture { tmp, root, project, attempt, config, .. } = f;
    let base = tmp.keep();
    let d = Dataset { base, root, project, attempt, config_digest: config.digest, binding: observed, scale, totals, mix, active, generated_ms: started.elapsed().as_secs_f64() * 1e3, producers };
    plant_producers(&d);
    d
}

/// Declared simulated quality facts, copied by value into the real quality lane.
/// These are fixture observations, never claims of live verification or git evidence.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Producers { proxies: Vec<Proxy>, outcomes: Vec<Outcome>, attention: Vec<(String, i64)> }
#[derive(serde::Serialize, serde::Deserialize)]
struct Proxy { task: String, attempt: String, submission: String, run: String, at: i64, flagged: bool }
#[derive(serde::Serialize, serde::Deserialize)]
struct Outcome { id: String, at: i64, reverted: bool, added: i64 }

fn plant_integration(db: &rusqlite::Connection, task: &str, op: &str, result: &str, end: i64, i: usize, now: i64) -> Option<Outcome> {
    let id = hex(&format!("integration-{task}"));
    let oid = "a".repeat(40);
    db.execute("INSERT OR IGNORE INTO integration_targets VALUES('/repo','refs/heads/scale',1)", []).unwrap();
    db.execute("INSERT OR IGNORE INTO integration_target_leases VALUES('/repo','refs/heads/scale',NULL,1)", []).unwrap();
    db.execute("INSERT INTO integration_operations(operation_id,project_store,idempotency_key,payload_digest,repository,ref_name,expected_old_oid,verified_result_id,state,generation,object_format,checks_passed,created_unix_ms) VALUES(?1,'store',?1,?2,'/repo','refs/heads/scale',?3,?4,'integrated',1,'sha1',1,?5)", rusqlite::params![op, id, oid, result, end]).unwrap();
    db.execute("INSERT INTO integration_candidates VALUES(?1,?2,?3,?3,?3,?3,'ort','sha1','published',?4)", rusqlite::params![id, op, oid, end]).unwrap();
    db.execute("INSERT INTO integrated_commits VALUES(?1,?1,?2,'/repo','refs/heads/scale',?3,?3,?3,'sha1',?4)", rusqlite::params![id, op, oid, end]).unwrap();
    (end < now - 14 * 86_400_000).then_some(Outcome { id, at: end, reverted: i.is_multiple_of(30), added: 40 + (i % 160) as i64 })
}

fn record_producer_mix(mix: &mut Mix, producers: &Producers) {
    for row in &producers.proxies {
        let payload = json!({"kind": "first_candidate_ci", "task_id": row.task, "submission_id": row.submission, "attempt_id": row.attempt,
            "run_id": row.run, "policy_digest": hex("e"), "base_oid": "a".repeat(40), "candidate_oid": "a".repeat(40), "ci_state": "accepted",
            "verified_unix_ms": row.at, "tests_added_lines": if row.flagged { 0 } else { 12 }, "tests_deleted_lines": if row.flagged { 8 } else { 2 },
            "tests_binary_files": 0, "weakening": if row.flagged { "flagged" } else { "clear" }, "weakening_reason": null,
            "weakening_rule": "tests-net-removal.v1", "source_trust": "proxy_observed", "observed_unix_ms": row.at + 1_000});
        mix.add("quality.first_candidate_ci", serde_json::to_vec(&payload).unwrap().len());
    }
    for row in &producers.outcomes {
        let payload = json!({"integrated_id": row.id, "horizon_ms": 1_209_600_000, "commit_oid": "a".repeat(40), "integrated_unix_ms": row.at,
            "horizon_oid": "a".repeat(40), "reverted": if row.reverted { "trailer" } else { "none" }, "added_lines": row.added,
            "surviving_lines": if row.reverted { 0 } else { row.added * 3 / 4 }, "churn_added_lines": 24, "churn_deleted_lines": 8,
            "unavailable_reason": null, "rule": "outcomes.v1", "source_trust": "proxy_observed", "observed_unix_ms": row.at + 14 * 86_400_000});
        mix.add("quality.integration_outcome", serde_json::to_vec(&payload).unwrap().len());
    }
    for (attempt, at) in &producers.attention {
        for (n, state) in ["working", "blocked", "blocked", "working", "idle"].iter().enumerate() {
            mix.add("attention.sample", serde_json::to_vec(&json!({"attempt_id": attempt, "observed_unix_ms": at + n as i64 * 60_000, "state": state, "gap": null, "interval_ms": 60_000, "source": "herdr-agent-list-v1"})).unwrap().len());
        }
    }
}

fn plant_producers(d: &Dataset) {
    let mut db = telemetry::sidecar::open(&d.project, true).unwrap().unwrap();
    let tx = db.transaction().unwrap();
    for p in &d.producers.proxies {
        tx.execute("INSERT INTO proxy_signals VALUES('first_candidate_ci',?1,?2,?3,?4,?5,?6,?6,'accepted',?7,?8,?9,0,?10,NULL,'tests-net-removal.v1','proxy_observed',?11)
            ON CONFLICT(kind,task_id) DO UPDATE SET tests_added_lines=excluded.tests_added_lines,tests_deleted_lines=excluded.tests_deleted_lines,
            tests_binary_files=excluded.tests_binary_files,weakening=excluded.weakening,weakening_reason=excluded.weakening_reason,
            observed_unix_ms=excluded.observed_unix_ms WHERE proxy_signals.weakening='unavailable' ",
            rusqlite::params![p.task, p.submission, p.attempt, p.run, hex("e"), "a".repeat(40), p.at, if p.flagged { 0 } else { 12 }, if p.flagged { 8 } else { 2 }, if p.flagged { "flagged" } else { "clear" }, p.at + 1_000]).unwrap();
    }
    for o in &d.producers.outcomes {
        tx.execute("INSERT OR IGNORE INTO integration_outcomes VALUES(?1,1209600000,?2,?3,?2,?4,?5,?6,24,8,NULL,'outcomes.v1','proxy_observed',?7)",
            rusqlite::params![o.id, "a".repeat(40), o.at, if o.reverted { "trailer" } else { "none" }, o.added, if o.reverted { 0 } else { o.added * 3 / 4 }, o.at + 14 * 86_400_000]).unwrap();
    }
    for (attempt, at) in &d.producers.attention {
        for (n, state) in ["working", "blocked", "blocked", "working", "idle"].iter().enumerate() {
            let at = at + n as i64 * 60_000;
            tx.execute("INSERT INTO attention_samples SELECT ?1,?2,?3,NULL,60000,'herdr-agent-list-v1' WHERE NOT EXISTS(SELECT 1 FROM attention_samples WHERE attempt_id=?1 AND observed_unix_ms=?2 AND state=?3)", rusqlite::params![attempt, at, state]).unwrap();
        }
    }
    tx.commit().unwrap();
}

/// Check public projections against independently declared fixture facts, not
/// SQL aggregates of those facts. Includes exclusions and exact waiting duration.
fn producer_gates(d: &Dataset, violations: &mut Vec<String>) {
    if d.producers.proxies.is_empty() { return; }
    let quality = telemetry(d, &["quality", "report"]).ok().json();
    let metrics = &quality["metrics"];
    let flagged = d.producers.proxies.iter().filter(|p| p.flagged).count();
    let clear = d.producers.proxies.len() - flagged;
    let added: i64 = d.producers.outcomes.iter().map(|o| o.added).sum();
    let surviving: i64 = d.producers.outcomes.iter().map(|o| if o.reverted { 0 } else { o.added * 3 / 4 }).sum();
    let reverted = d.producers.outcomes.iter().filter(|o| o.reverted).count();
    for (metric, key, expected) in [("M45", "numerator", json!(clear)), ("M45", "denominator", json!(clear)),
        ("M47", "numerator", json!(surviving)), ("M47", "denominator", json!(added)),
        ("M48", "numerator", json!(reverted)), ("M48", "denominator", json!(d.producers.outcomes.len()))] {
        if metrics[metric][key] != expected { violations.push(format!("{metric}.{key}: {} != {expected}", metrics[metric][key])); }
    }
    if metrics["M45"]["excluded"]["test_weakening"] != json!(flagged) { violations.push("quality weakening exclusions differ".into()); }
    let attention = telemetry(d, &["accounting", "attention", "--json"]).ok().json();
    let n = d.producers.attention.len();
    if attention["fleet"]["waiting_sum_ms"] != json!(n as i64 * 120_000) || attention["fleet"]["interventions"] != json!(n) || attention["orphan_samples"] != json!(0) {
        violations.push(format!("attention durations/interventions differ: {}", attention["fleet"]));
    }
}

// ---------------------------------------------------------------------------
// Measurement

#[derive(Clone, Debug, Default, serde::Serialize)]
struct Run { wall_ms: f64, user_ms: f64, sys_ms: f64, maxrss_kib: i64, code: i32, #[serde(skip)] stdout: Vec<u8>, #[serde(skip)] stderr: String }

impl Run {
    fn json(&self) -> Value { serde_json::from_slice(&self.stdout).unwrap_or_else(|e| panic!("{e}: {}\n{}", String::from_utf8_lossy(&self.stdout), self.stderr)) }
    fn ok(self) -> Self { assert_eq!(self.code, 0, "{}", self.stderr); self }
}

fn tv_ms(tv: libc::timeval) -> f64 { tv.tv_sec as f64 * 1e3 + tv.tv_usec as f64 / 1e3 }

/// The CLI command for dataset `d`, clean environment, no real Herdr.
fn command(d: &Dataset, args: &[&str]) -> Command {
    let mut c = Command::new(BIN);
    c.env_clear().env("HOME", d.home()).env("PATH", "/usr/bin:/bin").env("HERDR_BIN_PATH", "/bin/false")
        .args(["--root", d.root.to_str().unwrap()]).args(args);
    c
}

/// Run to completion; wall time, the child's own CPU (wait4) and its peak RSS.
/// The peak is the child's `VmHWM`, polled every 10 ms: wait4's `ru_maxrss`
/// would include this harness's own RSS copied at fork.
#[allow(clippy::zombie_processes)] // Reaped by the wait4 below, which also returns its rusage.
fn measure(mut c: Command) -> Run {
    let started = Instant::now();
    let mut child = c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let mut err = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || { let mut s = String::new(); let _ = err.read_to_string(&mut s); s });
    let status_path = format!("/proc/{}/status", child.id());
    let hwm = std::thread::spawn(move || {
        let mut peak = 0i64;
        while let Ok(status) = fs::read_to_string(&status_path) {
            if status.lines().any(|l| l.starts_with("State:") && l.contains('Z')) { break; }
            let Some(kib) = status.lines().find(|l| l.starts_with("VmHWM:")).and_then(|l| l.split_whitespace().nth(1)?.parse::<i64>().ok()) else { break };
            peak = peak.max(kib);
            std::thread::sleep(Duration::from_millis(10));
        }
        peak
    });
    let mut stdout = Vec::new();
    child.stdout.take().unwrap().read_to_end(&mut stdout).unwrap();
    let (mut status, mut ru): (libc::c_int, libc::rusage) = (0, unsafe { std::mem::zeroed() });
    // SAFETY: waits for our own child; `status` and `ru` are valid out-pointers.
    let pid = unsafe { libc::wait4(child.id() as libc::pid_t, &mut status, 0, &mut ru) };
    assert_eq!(pid, child.id() as libc::pid_t);
    let wall_ms = started.elapsed().as_secs_f64() * 1e3;
    let code = if libc::WIFEXITED(status) { libc::WEXITSTATUS(status) } else { -libc::WTERMSIG(status) };
    Run { wall_ms, user_ms: tv_ms(ru.ru_utime), sys_ms: tv_ms(ru.ru_stime), maxrss_kib: hwm.join().unwrap(), code, stdout, stderr: reader.join().unwrap() }
}

fn cli(d: &Dataset, args: &[&str]) -> Run { measure(command(d, args)) }

fn telemetry(d: &Dataset, args: &[&str]) -> Run {
    let mut all = vec!["telemetry", "demo"];
    all.extend_from_slice(args);
    cli(d, &all)
}

/// This thread's CPU time (ms).
fn thread_cpu_ms() -> f64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: valid out-pointer.
    unsafe { libc::getrusage(libc::RUSAGE_THREAD, &mut ru) };
    tv_ms(ru.ru_utime) + tv_ms(ru.ru_stime)
}

fn self_rusage() -> Value {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: valid out-pointer.
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    json!({"user_ms": tv_ms(ru.ru_utime), "sys_ms": tv_ms(ru.ru_stime), "maxrss_kib": ru.ru_maxrss})
}

/// Nearest-rank summary of a sample.
fn dist(sample: &[f64]) -> Value {
    if sample.is_empty() { return json!({"n": 0}); }
    let mut s = sample.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    let rank = |p: f64| s[((p / 100.0 * s.len() as f64).ceil() as usize).clamp(1, s.len()) - 1];
    let mean = s.iter().sum::<f64>() / s.len() as f64;
    let sd = (s.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / s.len() as f64).sqrt();
    let r = |x: f64| (x * 100.0).round() / 100.0;
    json!({"n": s.len(), "min": r(s[0]), "p50": r(rank(50.0)), "p95": r(rank(95.0)), "p99": r(rank(99.0)), "max": r(s[s.len() - 1]), "mean": r(mean), "sd": r(sd)})
}

/// Spread of per-repeat values: range and coefficient of variation (%).
fn noise(values: &[f64]) -> Value {
    let mean = values.iter().sum::<f64>() / values.len().max(1) as f64;
    let sd = (values.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / values.len().max(1) as f64).sqrt();
    let r = |x: f64| (x * 100.0).round() / 100.0;
    json!({"repeats": values.len(), "values": values.iter().map(|v| r(*v)).collect::<Vec<_>>(), "cv_percent": r(if mean > 0.0 { sd / mean * 100.0 } else { 0.0 })})
}

fn load_average() -> String { fs::read_to_string("/proc/loadavg").unwrap_or_default().trim().to_owned() }

fn file_size(path: &Path) -> u64 { fs::metadata(path).map(|m| m.len()).unwrap_or(0) }

fn sidecar_bytes(d: &Dataset) -> Value {
    json!({"db": file_size(&d.sidecar()), "wal": file_size(&d.sidecar().with_extension("db-wal"))})
}

/// The ticker's telemetry pass (src/ticker.rs `telemetry_pass`), without its
/// once-per-interval throttle: the Codex collect within the tick byte budget,
/// then every lane's tick. Errors are logged and the pass continues, as there.
fn telemetry_pass(project: &Path) -> (f64, Option<codex::Collected>, Vec<String>) {
    let (ms, collected, errors, _, _) = telemetry_pass_steps(project);
    (ms, collected, errors)
}

/// As `telemetry_pass`, with each step's milliseconds (`collect`, then each lane's stream).
/// A pass: its milliseconds, what the collect read, errors, and per-step milliseconds.
type PassSteps = (f64, Option<codex::Collected>, Vec<String>, Vec<(&'static str, f64)>, Value);

/// The shared worker's ledger turn; slower lanes run on DeferredLanes.
fn telemetry_core_steps(project: &Path) -> PassSteps {
    let started = Instant::now();
    let mut errors = Vec::new();
    let collected = match codex::collect(project, codex::Budget::TICK, false) {
        Ok(c) => c, Err(e) => { errors.push(format!("collect: {e:#}")); None }
    };
    let mut steps = vec![("collect", started.elapsed().as_secs_f64() * 1e3)];
    let t = Instant::now();
    let accounting = match telemetry::accounting::tick_observed(project, codex::Budget::TICK) {
        Ok(v) => v, Err(e) => { errors.push(format!("accounting: {e:#}")); Value::Null }
    };
    steps.push(("accounting", t.elapsed().as_secs_f64() * 1e3));
    (started.elapsed().as_secs_f64() * 1e3, collected, errors, steps, accounting)
}

fn telemetry_pass_steps(project: &Path) -> PassSteps {
    let started = Instant::now();
    let mut errors = Vec::new();
    let mut steps = Vec::new();
    let collected = match codex::collect(project, codex::Budget::TICK, false) { Ok(c) => c, Err(e) => { errors.push(format!("collect: {e:#}")); None } };
    steps.push(("collect", started.elapsed().as_secs_f64() * 1e3));
    let mut accounting = Value::Null;
    for lane in &telemetry::LANES {
        let t = Instant::now();
        if lane.stream == "accounting" {
            match telemetry::accounting::tick_observed(project, codex::Budget::TICK) {
                Ok(v) => accounting = v, Err(e) => errors.push(format!("accounting tick: {e:#}")),
            }
        } else if let Err(e) = (lane.tick)(project, codex::Budget::TICK) { errors.push(format!("{} tick: {e:#}", lane.stream)); }
        steps.push((lane.stream, t.elapsed().as_secs_f64() * 1e3));
    }
    (started.elapsed().as_secs_f64() * 1e3, collected, errors, steps, accounting)
}

// ---------------------------------------------------------------------------
// Correctness gates

/// Every gate over the synced ledger: the delta records equal the generator's
/// totals, each is accepted exactly once, no quarantine or unresolved record,
/// and `report` M08/M09 agree. Returns the violations (empty = pass).
fn usage_gates(d: &Dataset) -> Vec<String> {
    let mut v = Vec::new();
    let db = rusqlite::Connection::open_with_flags(d.sidecar(), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let q = |sql: &str| db.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
    let twice = q("SELECT count(*) FROM (SELECT entry_id FROM usage_dispositions WHERE disposition='accepted' GROUP BY entry_id HAVING count(*)>1)");
    if twice != 0 { v.push(format!("{twice} entries accepted more than once")); }
    let (records, input, cached, output, reasoning): (i64, i64, i64, i64, i64) = db.query_row("SELECT count(*),coalesce(sum(input_tokens),0),coalesce(sum(cache_read_tokens),0),
        coalesce(sum(output_tokens),0),coalesce(sum(reasoning_tokens),0) FROM usage_entries e WHERE basis='delta'
        AND EXISTS(SELECT 1 FROM usage_dispositions p WHERE p.entry_id=e.entry_id AND p.disposition='accepted')", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).unwrap();
    let got = Totals { input, cached, output, reasoning, records };
    if got != d.totals { v.push(format!("ledger {got:?} != generator {:?}", d.totals)); }
    let other = q("SELECT count(*) FROM usage_dispositions WHERE disposition NOT IN ('accepted','duplicate')");
    if other != 0 { v.push(format!("{other} dispositions neither accepted nor duplicate")); }
    let unbound = q("SELECT count(*) FROM rollout_sources WHERE binding<>'bound'");
    if unbound != 0 { v.push(format!("{unbound} rollouts not bound")); }
    let report = telemetry(d, &["report", "--json"]).ok().json();
    if report["metrics"]["M08"]["value"] != json!(input) || report["metrics"]["M09"]["value"] != json!(output) {
        v.push(format!("report M08/M09 {} / {} != {input} / {output}", report["metrics"]["M08"]["value"], report["metrics"]["M09"]["value"]));
    }
    producer_gates(d, &mut v);
    v
}

/// Digest of every canonical store file: telemetry must never change it.
fn canonical_digest(d: &Dataset) -> String {
    let mut h = Sha256::new();
    for suffix in ["", "-wal"] {
        let path = PathBuf::from(format!("{}{suffix}", d.state().display()));
        if let Ok(bytes) = fs::read(&path) { h.update(suffix.as_bytes()); h.update(&bytes); }
    }
    format!("{:x}", h.finalize())
}

/// The ledger's rows in a stable order (a rebuilt sidecar must match them byte for byte).
fn ledger_rows(sidecar: &Path) -> Vec<String> {
    let db = rusqlite::Connection::open_with_flags(sidecar, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    db.prepare("SELECT e.entry_id||'|'||e.basis||'|'||coalesce(e.model,'')||'|'||e.native||'|'||coalesce(e.total_tokens,'')||'|'||
        (SELECT group_concat(p.disposition||':'||coalesce(p.reason,''),',') FROM usage_dispositions p WHERE p.entry_id=e.entry_id) FROM usage_entries e ORDER BY e.entry_id").unwrap()
        .query_map([], |r| r.get::<_, String>(0)).unwrap().map(Result::unwrap).collect()
}

// ---------------------------------------------------------------------------
// The CI gate test

/// Doc 10 §6 in miniature, with every correctness gate at zero violations:
/// cold ingestion of the generated history; a burst of appends at 10× the
/// normal rate collected by concurrent, racing `collect`, `accounting sync`,
/// `analytics refresh` and export paging processes (SQLite contention: none
/// may fail); a sidecar that cannot grow (the collector's spool is full:
/// the refused range is a recorded coverage gap, nothing is lost, the next
/// collect recovers it); an exporter whose reader never reads (it blocks
/// alone: collection and analytics continue); collectors and syncs killed
/// mid-pass (the resumed ledger equals one rebuilt from scratch). Throughout,
/// `state.db` is byte-identical, an as-of answer recorded before the load is
/// reproduced after it, and `analytics rebuild --verify` finds every tracked
/// cell identical.
#[test]
fn scale_gates_hold_under_load() {
    let f = Fixture::new();
    let mut d = generate(f, Scale { attempts: 200, active: 32, events: 5_000, homes: 4, seed: 51 });
    let _cleanup = Cleanup(d.base.clone());
    let canonical = canonical_digest(&d);
    telemetry(&d, &["collect"]).ok();
    telemetry(&d, &["accounting", "sync"]).ok();
    assert_eq!(usage_gates(&d), Vec::<String>::new());
    let refresh = telemetry(&d, &["analytics", "refresh"]).ok().json();
    // As-of: the M08 revision recorded now must answer identically after the load.
    let revisions = telemetry(&d, &["analytics", "revisions", "--metric", "M08"]).ok().json();
    let seq = revisions["revisions"].as_array().and_then(|r| r.last()).map(|r| r["revision"].as_i64().unwrap()).unwrap_or_else(|| panic!("{refresh}\n{revisions}"));
    // The pinned revision's content; its later supersession (`current_revision`,
    // `restated`, `superseded_by`) is lineage that legitimately grows.
    let as_of = |d: &Dataset| {
        let mut v = telemetry(d, &["query", "--metric", "M08", "--as-of-seq", &seq.to_string(), "--json"]).ok().json();
        v.as_object_mut().unwrap().remove("query_unix_ms");
        for key in ["current_revision", "restated", "superseded_by"] { v["results"][0]["projection"].as_object_mut().unwrap().remove(key); }
        v
    };
    let before = as_of(&d);
    assert_eq!(before["results"][0]["projection"]["revision"], json!(seq), "{before}");

    // Burst: 10 × 100 events for each second of load, appended while racing processes read and write.
    let stop = Arc::new(AtomicBool::new(false));
    let failures = Arc::new(Mutex::new(Vec::<String>::new()));
    let dir = d.base.clone();
    d.save(&dir);
    let racers: Vec<_> = [&["collect"][..], &["collect"], &["accounting", "sync"], &["analytics", "refresh"], &["export", "--metric", "M02", "--drill", "numerator", "--page-size", "50"]]
        .into_iter().map(|args| {
            let (stop, failures, dir) = (stop.clone(), failures.clone(), dir.clone());
            let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            std::thread::spawn(move || {
                let d = Dataset::load(&dir);
                let mut runs = 0;
                while !stop.load(Ordering::Relaxed) || runs == 0 {
                    let run = telemetry(&d, &args.iter().map(String::as_str).collect::<Vec<_>>());
                    if run.code != 0 { failures.lock().unwrap().push(format!("{args:?}: {}", run.stderr)); }
                    runs += 1;
                }
                runs
            })
        }).collect();
    let started = Instant::now();
    let mut appended = 0;
    while started.elapsed() < Duration::from_secs(4) {
        let second = Instant::now();
        for s in d.active.iter_mut() { s.append(1000 / 32, &mut d.totals, &mut d.mix); appended += 1000 / 32; }
        std::thread::sleep(Duration::from_millis(1000).saturating_sub(second.elapsed()));
    }
    stop.store(true, Ordering::Relaxed);
    let runs: Vec<i32> = racers.into_iter().map(|r| r.join().unwrap()).collect();
    assert_eq!(*failures.lock().unwrap(), Vec::<String>::new(), "racing processes failed (runs {runs:?})");
    assert!(appended >= 3_000, "{appended}");

    // Full spool: the sidecar may not grow; the collect records a gap and stores nothing of the range.
    for s in d.active.iter_mut() { s.append(100, &mut d.totals, &mut d.mix); }
    refused_collect(&d).ok();
    let gaps = |d: &Dataset, recovery: &str| rusqlite::Connection::open(d.sidecar()).unwrap()
        .query_row("SELECT count(*) FROM coverage_gaps WHERE reason='sidecar_write_failed' AND recovery=?1", [recovery], |r| r.get::<_, i64>(0)).unwrap();
    assert!(gaps(&d, "pending") >= 1, "the refused range is a recorded gap");

    // A stalled exporter (its reader never reads) blocks only itself.
    fs::create_dir_all(d.home().join(".config/herdr-farm")).unwrap();
    let config = d.home().join(".config/herdr-farm/telemetry-export.toml");
    fs::write(&config, "schema = \"telemetry-export-config.v1\"\n[external]\nenabled = true\ndestination = \"stdout\"\n").unwrap();
    fs::set_permissions(&config, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    let (mut stalled, _reader) = stalled_exporter(&d);
    let pass = Instant::now();
    telemetry(&d, &["collect"]).ok();
    telemetry(&d, &["accounting", "sync"]).ok();
    telemetry(&d, &["analytics", "refresh"]).ok();
    assert!(pass.elapsed() < Duration::from_secs(60), "collection waited on the stalled exporter");
    assert!(gaps(&d, "recovered") >= 1);
    assert_eq!(gaps(&d, "pending"), 0, "the next collect with room recovers the gap");
    assert!(stalled.try_wait().unwrap().is_none(), "the exporter was blocked on its reader throughout");
    let _ = stalled.kill();
    let _ = stalled.wait();

    // Restart recovery: collects and syncs killed at growing delays mid-pass, then one full pass.
    for s in d.active.iter_mut() { s.append(60, &mut d.totals, &mut d.mix); }
    for (n, args) in [["collect", ""], ["accounting", "sync"], ["collect", ""], ["analytics", "refresh"]].iter().enumerate() {
        let args: Vec<&str> = args.iter().copied().filter(|a| !a.is_empty()).collect();
        let mut all = vec!["telemetry", "demo"];
        all.extend(args);
        let mut child = command(&d, &all).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        std::thread::sleep(Duration::from_millis(5 + 15 * n as u64));
        let _ = child.kill();
        let _ = child.wait();
    }
    telemetry(&d, &["collect"]).ok();
    telemetry(&d, &["accounting", "sync"]).ok();
    assert_eq!(usage_gates(&d), Vec::<String>::new());
    telemetry(&d, &["analytics", "refresh"]).ok();
    let verify = telemetry(&d, &["analytics", "rebuild", "--verify"]).ok().json();
    let cells = verify["cells"].as_array().unwrap_or_else(|| panic!("{verify}"));
    assert!(!cells.is_empty() && cells.iter().all(|c| c["identical"] == true && c["stored_intact"] == true), "{verify}");
    assert_eq!(as_of(&d), before, "an as-of answer is reproduced after the load");

    // Byte-identical rebuild: the resumed ledger equals one collected from scratch.
    let resumed = ledger_rows(&d.sidecar());
    let copy = d.base.join("resumed.db");
    fs::copy(d.sidecar(), &copy).unwrap();
    for suffix in ["", "-wal", "-shm"] { let _ = fs::remove_file(format!("{}{suffix}", d.sidecar().display())); }
    telemetry(&d, &["collect"]).ok();
    telemetry(&d, &["accounting", "sync"]).ok();
    assert!(resumed == ledger_rows(&d.sidecar()), "rebuilt ledger differs from the resumed one");
    assert_eq!(canonical_digest(&d), canonical, "telemetry wrote the canonical store");
}

/// Real ticker CLI: recorded native sources opt an absent sidecar into collection.
#[test]
fn ticker_collects_recorded_sources_before_observing_a_new_sidecar() {
    struct Ticker(std::process::Child);
    impl Drop for Ticker { fn drop(&mut self) { let _=self.0.kill(); let _=self.0.wait(); } }
    let mut d=generate(Fixture::new(),Scale{attempts:200,active:4,events:4_000,homes:4,seed:55});
    // This workflow declares native usage only; quality/attention facts remain
    // covered by the unchanged load workflow with its preinstalled producers.
    d.producers=Producers::default();
    let _cleanup=Cleanup(d.base.clone());
    fs::write(d.project.join("PROJECT.md"),"initial ticker collection fixture\n").unwrap();
    fs::write(d.project.join(".state/format.json"),"{}").unwrap();
    rusqlite::Connection::open(d.state()).unwrap().execute("UPDATE project_control SET state='paused',factory_admission='off'",[]).unwrap();
    // Remove only the isolated generated sidecar. Native files and canonical
    // input receipts remain, so the ticker must discover those recorded sources.
    for suffix in ["","-wal","-shm"] {let _=fs::remove_file(format!("{}{suffix}",d.sidecar().display()));}
    let mut child=Ticker(command(&d,&["ticker","run"]).env("HERDR_PROJECTS_TELEMETRY_COLLECT_SECS","1")
        .stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
    let deadline=Instant::now()+Duration::from_secs(30);
    loop {
        let ready=rusqlite::Connection::open_with_flags(d.sidecar(),rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()
            .and_then(|db|db.query_row("SELECT count(*) FROM usage_entries WHERE basis='delta'",[],|r|r.get::<_,i64>(0)).ok())==Some(d.totals.records);
        if ready {break;}
        assert!(Instant::now()<deadline&&child.0.try_wait().unwrap().is_none(),"{}",fs::read_to_string(d.root.join(".ticker.log")).unwrap_or_default());
        std::thread::sleep(Duration::from_millis(50));
    }
    fs::write(d.root.join(".ticker.stop"),b"").unwrap();
    assert!(child.0.wait().unwrap().success());
    let db=rusqlite::Connection::open_with_flags(d.sidecar(),rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert!(!db.query_row("SELECT active FROM operating_clock WHERE singleton=1",[],|r|r.get::<_,bool>(0)).unwrap());
    assert_eq!(db.query_row("SELECT count(*) FROM operating_intervals",[],|r|r.get::<_,i64>(0)).unwrap(),0);
    drop(db);
    let violations=usage_gates(&d);
    assert!(violations.is_empty(),"{violations:?}");
}

/// Real ticker CLI: kill after a committed prefix of one long rollout, then
/// resume through the ticker and compare exact usage and ledger replay bytes.
#[test]
fn ticker_batched_telemetry_resumes_after_kill() {
    struct Ticker(std::process::Child);
    impl Drop for Ticker { fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); } }
    let mut d = generate(Fixture::new(), Scale { attempts: 200, active: 4, events: 4_000, homes: 4, seed: 54 });
    let _cleanup = Cleanup(d.base.clone());
    fs::write(d.project.join("PROJECT.md"), "ticker batch fixture\n").unwrap();
    fs::write(d.project.join(".state/format.json"), "{}").unwrap();
    // This fixture exercises telemetry, with controller admission disabled.
    rusqlite::Connection::open(d.state()).unwrap().execute("UPDATE project_control SET state='paused',factory_admission='off'", []).unwrap();
    telemetry(&d, &["collect"]).ok();
    telemetry(&d, &["accounting", "sync"]).ok();
    telemetry(&d, &["analytics", "refresh"]).ok();
    let revisions = telemetry(&d, &["analytics", "revisions", "--metric", "M08"]).ok().json();
    let seq = revisions["revisions"].as_array().unwrap().last().unwrap()["revision"].as_i64().unwrap();
    let pinned = |d: &Dataset| {
        let mut v = telemetry(d, &["query", "--metric", "M08", "--as-of-seq", &seq.to_string(), "--json"]).ok().json();
        v.as_object_mut().unwrap().remove("query_unix_ms");
        for key in ["current_revision", "restated", "superseded_by"] { v["results"][0]["projection"].as_object_mut().unwrap().remove(key); }
        v
    };
    let before = pinned(&d);
    let file = d.active[0].path.clone();
    let key = format!("sha256:{}", hex(file.to_str().unwrap()));
    let initial = fs::metadata(&file).unwrap().len();
    d.active[0].append(12_000, &mut d.totals, &mut d.mix);
    let offset = || rusqlite::Connection::open(d.sidecar()).unwrap().query_row("SELECT byte_offset FROM collect_offsets WHERE path_digest=?1", [&key], |r| r.get::<_, u64>(0)).unwrap();
    assert_eq!(offset(), initial);
    let end = fs::metadata(&file).unwrap().len();
    let start = || Ticker(command(&d, &["ticker", "run"]).env("HERDR_PROJECTS_TELEMETRY_COLLECT_SECS", "1")
        .stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
    let log = || fs::read_to_string(d.root.join(".ticker.log")).unwrap_or_default();
    let mut killed = start();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut observed_priority = false;
    loop {
        for entry in fs::read_dir(format!("/proc/{}/task", killed.0.id())).unwrap().flatten() {
            if fs::read_to_string(entry.path().join("comm")).unwrap_or_default().trim() != "telemetry-pass" { continue; }
            let tid: i32 = entry.file_name().to_str().unwrap().parse().unwrap();
            // SAFETY: these syscalls query a live fixture child thread only.
            let (policy, nice, io) = unsafe { (libc::sched_getscheduler(tid), libc::getpriority(libc::PRIO_PROCESS, tid as _),
                libc::syscall(libc::SYS_ioprio_get, 1, tid)) };
            let warnings = log();
            if !(policy == libc::SCHED_IDLE || warnings.contains("SCHED_IDLE:"))
                || !(nice == 19 || warnings.contains("nice 19:"))
                || !(io == 3 << 13 || warnings.contains("I/O idle:")) { continue; }
            // SAFETY: query the process leader, which must retain the test's policy.
            assert_eq!(unsafe { libc::sched_getscheduler(killed.0.id() as _) }, unsafe { libc::sched_getscheduler(0) });
            observed_priority = true;
        }
        if offset() > initial && offset() < end { break; }
        assert!(Instant::now() < deadline && killed.0.try_wait().unwrap().is_none(), "no committed partial batch: {}", log());
        std::thread::sleep(Duration::from_millis(5));
    }
    killed.0.kill().unwrap();
    killed.0.wait().unwrap();
    assert!(observed_priority);
    let prefix = offset();
    assert!(prefix > initial && prefix < end, "kill must leave a committed prefix: {prefix}/{end}");
    let persisted: (u64, u64) = rusqlite::Connection::open(d.sidecar()).unwrap().query_row("SELECT o.byte_offset,c.byte_offset FROM collect_offsets o JOIN source_cursors c ON c.source=o.path_digest WHERE o.path_digest=?1", [&key], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!(persisted, (prefix, prefix), "input and envelope cursors commit together");
    let mut resumed = start();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let records: i64 = rusqlite::Connection::open(d.sidecar()).unwrap().query_row("SELECT count(*) FROM usage_entries WHERE basis='delta'", [], |r| r.get(0)).unwrap();
        if offset() == end && records == d.totals.records { break; }
        assert!(Instant::now() < deadline && resumed.0.try_wait().unwrap().is_none(), "ticker did not resume: {}", log());
        std::thread::sleep(Duration::from_millis(20));
    }
    fs::write(d.root.join(".ticker.stop"), b"").unwrap();
    assert!(resumed.0.wait().unwrap().success());
    assert_eq!(usage_gates(&d), Vec::<String>::new());
    assert_eq!(pinned(&d), before);
    let ledger = ledger_rows(&d.sidecar());
    for suffix in ["", "-wal", "-shm"] { let _ = fs::remove_file(format!("{}{suffix}", d.sidecar().display())); }
    let canonical = canonical_digest(&d);
    plant_producers(&d);
    telemetry(&d, &["collect"]).ok();
    telemetry(&d, &["accounting", "sync"]).ok();
    assert_eq!(usage_gates(&d), Vec::<String>::new());
    assert!(ledger == ledger_rows(&d.sidecar()), "batch replay changed ledger bytes");
    assert_eq!(canonical_digest(&d), canonical);
}

/// A collect whose sidecar cannot grow: the write-ahead log is truncated and
/// the process may not write past 1 MiB of any file (a full disk for the
/// collector's spool), so the backlog's transaction fails part way.
fn refused_collect(d: &Dataset) -> Run {
    rusqlite::Connection::open(d.sidecar()).unwrap().query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(())).unwrap();
    let limit = 1 << 20;
    let mut full = command(d, &["telemetry", "demo", "collect"]);
    // SAFETY: only async-signal-safe libc calls between fork and exec.
    unsafe {
        full.pre_exec(move || {
            libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
            let rlimit = libc::rlimit { rlim_cur: limit, rlim_max: limit };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &rlimit) != 0 { return Err(std::io::Error::last_os_error()); }
            Ok(())
        });
    }
    measure(full)
}

/// `export --external` to stdout (enabled in `telemetry-export.toml`) whose
/// reader never reads: stdout is a one-page pipe, so the page blocks the
/// exporter in `write` once its query has finished. Returns the exporter and
/// the unread end.
fn stalled_exporter(d: &Dataset) -> (std::process::Child, std::os::fd::OwnedFd) {
    use std::os::fd::{FromRawFd, OwnedFd};
    let config = d.home().join(".config/herdr-farm");
    fs::create_dir_all(&config).unwrap();
    let path = config.join("telemetry-export.toml");
    fs::write(&path, "schema = \"telemetry-export-config.v1\"\n[external]\nenabled = true\ndestination = \"stdout\"\n").unwrap();
    fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    let mut fds = [0; 2];
    // SAFETY: `fds` is a valid two-element array; each descriptor is owned once below.
    let (reader, writer) = unsafe {
        assert_eq!(libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC), 0);
        libc::fcntl(fds[1], libc::F_SETPIPE_SZ, 4096);
        (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1]))
    };
    let child = command(d, &["telemetry", "demo", "export", "--metric", "M02", "--drill", "denominator", "--page-size", "500", "--external"])
        .stdout(Stdio::from(writer)).stderr(Stdio::null()).spawn().unwrap();
    // Until its query has run and the page is waiting on the pipe.
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        let stat = fs::read_to_string(format!("/proc/{}/wchan", child.id())).unwrap_or_default();
        if stat.contains("pipe") { break; }
        std::thread::sleep(Duration::from_millis(50));
    }
    (child, reader)
}

struct Cleanup(PathBuf);
impl Drop for Cleanup { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }

// ---------------------------------------------------------------------------
// Benches (ignored): the doc 10 matrix, one phase per test

fn data_dir() -> PathBuf {
    let dir = PathBuf::from(std::env::var("SCALE_DATA").expect("SCALE_DATA: the dataset directory (on disk, not tmpfs)"));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn env_usize(name: &str, default: usize) -> usize { std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default) }

fn write_results(dir: &Path, phase: &str, value: &Value) {
    let path = dir.join(format!("results-{phase}.json"));
    fs::write(&path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
    println!("{phase}: {}", serde_json::to_string(value).unwrap());
}

fn host() -> Value {
    let cpu = fs::read_to_string("/proc/cpuinfo").unwrap_or_default().lines().find(|l| l.starts_with("model name")).map(|l| l.split(':').nth(1).unwrap_or("").trim().to_owned());
    let mem = fs::read_to_string("/proc/meminfo").unwrap_or_default().lines().next().map(str::to_owned);
    json!({"cpu": cpu, "cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0), "mem": mem,
        "kernel": fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default().trim(), "loadavg_start": load_average()})
}

/// Phase 0: generate the dataset (`SCALE_EVENTS`, `SCALE_ACTIVE`,
/// `SCALE_ATTEMPTS`) and record the canonical store's digest.
#[test]
#[ignore]
fn scale_0_generate() {
    let dir = data_dir();
    if Dataset::manifest(&dir).exists() {
        let mut d = Dataset::load(&dir);
        if !d.producers.proxies.is_empty() {
            plant_producers(&d);
            println!("replayed the existing producer facts");
            return;
        }
        let db = rusqlite::Connection::open(d.state()).unwrap();
        db.execute_batch("PRAGMA foreign_keys=OFF; BEGIN").unwrap();
        let rows: Vec<(String, String, String, String, i64)> = db.prepare("SELECT s.task_id,s.attempt_id,s.submission_id,v.result_id,v.created_unix_ms FROM result_submissions s JOIN verified_results v ON v.submission_id=s.submission_id ORDER BY s.task_id").unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).unwrap().map(Result::unwrap).collect();
        for (task, attempt, submission, run, end) in rows {
            let i: usize = task.strip_prefix("st").unwrap().parse().unwrap();
            d.producers.proxies.push(Proxy { task: task.clone(), attempt, submission, run: run.clone(), at: end - 1_000, flagged: i.is_multiple_of(5) });
            if i.is_multiple_of(10) && let Some(o) = plant_integration(&db, &task, &format!("op-s{i:05}"), &run, end, i, unix_ms()) { d.producers.outcomes.push(o); }
        }
        for i in d.scale.attempts - d.scale.active..d.scale.attempts {
            let at: i64 = db.query_row("SELECT unix_ms+500 FROM attempt_lifecycle WHERE attempt_id=?1 AND state='running'", [format!("s{i:05}")], |r| r.get(0)).unwrap();
            d.producers.attention.push((format!("s{i:05}"), at));
        }
        db.execute_batch("COMMIT").unwrap();
        assert_eq!(db.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
        drop(db);
        record_producer_mix(&mut d.mix, &d.producers);
        plant_producers(&d);
        d.save(&dir);
        fs::write(dir.join("canonical.digest"), canonical_digest(&d)).unwrap();
        println!("extended the same dataset: {} events, {} logical bytes", d.mix.events(), d.mix.total_bytes());
        return;
    }
    let scale = Scale { attempts: env_usize("SCALE_ATTEMPTS", 10_000), active: env_usize("SCALE_ACTIVE", 64), events: env_usize("SCALE_EVENTS", 100_000), homes: 8, seed: 5_100 };
    let d = generate(Fixture::new(), scale);
    d.save(&dir);
    fs::write(dir.join("canonical.digest"), canonical_digest(&d)).unwrap();
    println!("generated {} events, {} bytes in {:.0} ms", d.mix.events(), d.mix.total_bytes(), d.generated_ms);
}

/// Phase 1: ingest the generated dataset cold through the CLI as an operator
/// would (collect until the byte budget is no longer exhausted, sync,
/// analytics refresh, health evaluate), and check every usage gate.
#[test]
#[ignore]
fn scale_1_ingest() {
    let dir = data_dir();
    let host = host();
    let d = Dataset::load(&dir);
    let canonical = fs::read_to_string(dir.join("canonical.digest")).unwrap();
    let mut collects = Vec::new();
    loop {
        let run = telemetry(&d, &["collect"]).ok();
        let done = run.json()["collected"]["budget_exhausted"] == false;
        collects.push(json!({"wall_ms": run.wall_ms, "user_ms": run.user_ms, "sys_ms": run.sys_ms, "maxrss_kib": run.maxrss_kib, "collected": run.json()["collected"]}));
        if done { break; }
    }
    let wall: f64 = collects.iter().map(|c| c["wall_ms"].as_f64().unwrap()).sum();
    let sync = telemetry(&d, &["accounting", "sync"]).ok();
    let refresh = telemetry(&d, &["analytics", "refresh"]).ok();
    let health = telemetry(&d, &["health", "evaluate", "--json"]).ok();
    let violations = usage_gates(&d);
    let plans = telemetry(&d, &["analytics", "plans"]).ok().json();
    // One ticker telemetry pass in its own process (this test binary, `scale_pass_child`):
    // its peak RSS, with the analytics refresh due.
    let pass = measure({
        let mut c = Command::new(std::env::current_exe().unwrap());
        c.args(["--exact", "scale_pass_child", "--ignored", "--test-threads=1", "--nocapture"]).env("SCALE_PASS_CHILD", "1").env("SCALE_DATA", &dir)
            .env("PATH", "/usr/bin:/bin").env("HERDR_BIN_PATH", "/bin/false");
        c
    }).ok();
    // libtest prints the test's name on the same line as its first output.
    let child = String::from_utf8_lossy(&pass.stdout).lines().find_map(|l| l.split_once("pass: ").map(|(_, v)| v.to_owned())).unwrap_or_default();
    let events = d.mix.events();
    let rollout_bytes: u64 = d.mix.bytes.iter().filter(|(k, _)| !k.starts_with("quality.") && !k.starts_with("attention.")).map(|(_, bytes)| bytes).sum();
    let out = json!({"host": host, "scale": d.scale, "generated_ms": d.generated_ms, "events": events, "rollout_bytes": rollout_bytes, "logical_event_bytes": d.mix.total_bytes(), "mix": d.mix,
        "totals": d.totals, "collect_runs": collects, "ingest": {"wall_ms": wall, "rollout_events_per_s": d.mix.counts.iter().filter(|(k, _)| !k.starts_with("quality.") && !k.starts_with("attention.")).map(|(_, n)| n).sum::<u64>() as f64 / wall * 1e3, "mib_per_s": rollout_bytes as f64 / 1048576.0 / wall * 1e3},
        "sync": sync, "analytics_refresh": refresh, "health_evaluate": health, "sidecar_bytes": sidecar_bytes(&d),
        "pass_process": {"wall_ms": pass.wall_ms, "user_ms": pass.user_ms, "sys_ms": pass.sys_ms, "maxrss_kib": pass.maxrss_kib, "steps": serde_json::from_str::<Value>(&child).unwrap_or(Value::Null)},
        "state_bytes": file_size(&d.state()), "canonical_unchanged": canonical_digest(&d) == canonical, "violations": violations, "plans": plans, "loadavg_end": load_average()});
    write_results(&dir, "prepare", &out);
    assert_eq!(out["violations"], json!([]));
}

/// Child of `scale_1_ingest` (`SCALE_PASS_CHILD=1`): one ticker telemetry pass
/// with the once-a-minute analytics refresh due (its last run moved back),
/// timed by step. The health evaluation (at most every five minutes) is
/// measured on its own by the ingest phase. Returns at once otherwise.
#[test]
#[ignore]
fn scale_pass_child() {
    if std::env::var("SCALE_PASS_CHILD").as_deref() != Ok("1") { return; }
    let d = Dataset::load(&data_dir());
    telemetry::background::idle_priority(|warning| eprintln!("{warning}"));
    rusqlite::Connection::open(d.sidecar()).unwrap().execute_batch("UPDATE analytics_cells SET checked_unix_ms=checked_unix_ms-120000").unwrap();
    let (ms, collected, errors, steps, accounting) = telemetry_pass_steps(&d.project);
    println!("pass: {}", json!({"ms": ms, "collected": collected, "errors": errors, "accounting": accounting, "steps": steps.into_iter().map(|(k, v)| (k.to_owned(), json!(v))).collect::<serde_json::Map<_, _>>()}));
}

/// The bounded read surfaces, each through the CLI (a fresh process per
/// answer, as dashboards, `watch` restarts and the coordinator call them),
/// plus the fleet pane refresh and digest section as the long-running
/// `watch` renders them (in process). `SCALE_REPEATS` rounds, the order
/// rotated each round.
/// A copy without the fields that are a function of the clock.
fn timeless(v: &Value) -> Value {
    const CLOCK: [&str; 10] = ["query_unix_ms", "observation_cutoff_unix_ms", "lag_ms", "evaluated_unix_ms", "age_ms", "observed_unix_ms", "from_unix_ms", "to_unix_ms", "text", "lag_text"];
    match v {
        Value::Object(o) => Value::Object(o.iter().filter(|(k, _)| !CLOCK.contains(&k.as_str())).map(|(k, v)| (k.clone(), timeless(v))).collect()),
        Value::Array(a) => Value::Array(a.iter().map(timeless).collect()),
        other => other.clone(),
    }
}

#[test]
#[ignore]
fn scale_2_queries() {
    let dir = data_dir();
    let load0 = load_average();
    let d = Dataset::load(&dir);
    let repeats = env_usize("SCALE_REPEATS", 5);
    let per = env_usize("SCALE_PER_ROUND", 4);
    let config = d.home().join(".config/herdr-farm");
    fs::create_dir_all(&config).unwrap();
    let queries: Vec<(&str, Vec<&str>)> = vec![
        ("query M08 (lane usage)", vec!["query", "--metric", "M08", "--json"]),
        ("query M13 (coverage)", vec!["query", "--metric", "M13", "--json"]),
        ("query M02 terminal_cohort", vec!["query", "--metric", "M02", "--json"]),
        ("query M02 by task_class", vec!["query", "--metric", "M02", "--by", "task_class", "--json"]),
        ("query M07 assignment_cohort", vec!["query", "--metric", "M07", "--cohort", "assignment_cohort", "--json"]),
        ("query M02 as-of seq 1", vec!["query", "--metric", "M02", "--as-of-seq", "1000000", "--json"]),
        ("report", vec!["report", "--json"]),
        ("view project", vec!["view", "project", "--json"]),
        ("view cost", vec!["view", "cost", "--json"]),
        ("view health", vec!["view", "health", "--json"]),
        ("compare M02", vec!["compare", "--metric", "M02", "--json"]),
        ("health (live states)", vec!["health", "--json"]),
        ("export M02 drill page 500", vec!["export", "--metric", "M02", "--drill", "numerator", "--page-size", "500"]),
        ("workspace show (pane, fresh process)", vec!["workspace", "show", "--json"]),
        ("workspace digest (fresh process)", vec!["workspace", "digest"]),
    ];
    // `SCALE_QUERY_SET=light|heavy|dg1|p8` splits the list so each run stays under ten
    // minutes at a million events: heavy = the fleet surfaces, health, report, compare.
    let heavy = |name: &str| ["workspace", "health", "report", "compare"].iter().any(|h| name.starts_with(h));
    let set = std::env::var("SCALE_QUERY_SET").unwrap_or_else(|_| "all".into());
    let queries: Vec<(&str, Vec<&str>)> = queries.into_iter().filter(|(name, _)| match set.as_str() { "light" => !heavy(name), "heavy" => heavy(name), "dg1" => *name == "report" || *name == "query M02 terminal_cohort", "p8" => *name == "compare M02" || *name == "health (live states)",
        "p9" => ["view project", "view health", "health (live states)", "query M02 terminal_cohort", "query M07 assignment_cohort"].contains(name), _ => true }).collect();
    let in_process = !matches!(set.as_str(), "light" | "dg1" | "p8" | "p9");
    let query_bin = std::env::var_os("SCALE_QUERY_BIN");
    let comparison_bin = std::env::var_os("SCALE_COMPARE_BIN");
    let mut byte_comparisons = 0;
    let mut samples: std::collections::BTreeMap<String, Vec<Run>> = Default::default();
    let mut rounds: std::collections::BTreeMap<String, Vec<f64>> = Default::default();
    let mut digest_lines = 0;
    let mut digest_bytes = 0;
    for round in 0..repeats {
        let mut order: Vec<usize> = (0..queries.len()).collect();
        order.rotate_left(round % queries.len());
        let mut round_samples: std::collections::BTreeMap<String, Vec<f64>> = Default::default();
        for &q in &order {
            let (name, args) = &queries[q];
            for _ in 0..per {
                let run = if let Some(binary) = &query_bin {
                    // Preserve the normal fixture environment when comparing
                    // a preserved release CLI on the same disk dataset.
                    let mut c = Command::new(binary);
                    c.env_clear().env("HOME", d.home()).env("PATH", "/usr/bin:/bin").env("HERDR_BIN_PATH", "/bin/false")
                        .args(["--root", d.root.to_str().unwrap(), "telemetry", "demo"]).args(args);
                    measure(c).ok()
                } else { telemetry(&d, args).ok() };
                if let Some(binary) = &comparison_bin && (*name == "compare M02" || set == "p9") {
                    let mut c = Command::new(binary);
                    c.env_clear().env("HOME", d.home()).env("PATH", "/usr/bin:/bin").env("HERDR_BIN_PATH", "/bin/false")
                        .args(["--root", d.root.to_str().unwrap(), "telemetry", "demo"]).args(args);
                    let baseline = measure(c).ok().stdout;
                    if *name == "compare M02" { assert_eq!(run.stdout, baseline, "compare bytes differ from baseline"); }
                    else {
                        // The dashboards print the clock (query time, lag, ages, rule windows): everything else is identical.
                        let parse = |bytes: &[u8]| timeless(&serde_json::from_slice::<Value>(bytes).unwrap());
                        assert_eq!(parse(&run.stdout), parse(&baseline), "{name} differs from baseline");
                    }
                    byte_comparisons += 1;
                }
                round_samples.entry(name.to_string()).or_default().push(run.wall_ms);
                samples.entry(name.to_string()).or_default().push(run);
            }
        }
        // In process: the pane's refresh and the digest section, as `watch` and `context` compute them.
        for _ in 0..if in_process { per } else { 0 } {
            let t = Instant::now();
            let snapshot = telemetry::workspace::snapshot(&d.project, "demo");
            let pane = telemetry::workspace::text(&snapshot);
            round_samples.entry("pane refresh (in process)".into()).or_default().push(t.elapsed().as_secs_f64() * 1e3);
            assert!(!pane.is_empty() && snapshot["status"] != "unavailable", "{snapshot}");
            let t = Instant::now();
            let section = telemetry::workspace::context_section(&d.project, "demo", &config).unwrap();
            round_samples.entry("digest section (in process)".into()).or_default().push(t.elapsed().as_secs_f64() * 1e3);
            digest_lines = section.lines().count();
            digest_bytes = section.len();
        }
        for (name, values) in round_samples {
            rounds.entry(name.clone()).or_default().push(dist(&values)["p50"].as_f64().unwrap());
            if name.contains("in process") { samples.entry(name).or_default().extend(values.iter().map(|&wall_ms| Run { wall_ms, ..Default::default() })); }
        }
    }
    // Coordinator digest with the telemetry section and with `[telemetry] views = false`
    // (`context` needs the project's PROJECT.md).
    if !d.project.join("PROJECT.md").exists() { fs::write(d.project.join("PROJECT.md"), "scale fixture\n").unwrap(); }
    let mut context = json!({});
    let context_works = in_process && cli(&d, &["context", "demo", "--peek"]).code == 0;
    if context_works {
        for (label, on) in [("views_on", true), ("views_off", false), ("views_on_again", true)] {
            fs::write(config.join("config.toml"), format!("[telemetry]\nviews = {on}\n")).unwrap();
            let walls: Vec<f64> = (0..repeats * per).map(|_| cli(&d, &["context", "demo", "--peek"]).ok().wall_ms).collect();
            context[label] = dist(&walls);
        }
        let _ = fs::remove_file(config.join("config.toml"));
    }
    let results: Value = samples.iter().map(|(name, runs)| (name.clone(), json!({
        "wall_ms": dist(&runs.iter().map(|r| r.wall_ms).collect::<Vec<_>>()),
        "cpu_ms": dist(&runs.iter().map(|r| r.user_ms + r.sys_ms).collect::<Vec<_>>()),
        "maxrss_kib": runs.iter().map(|r| r.maxrss_kib).max(),
        "round_p50_noise": noise(&rounds[name]),
    }))).collect::<serde_json::Map<_, _>>().into();
    let startup: Vec<f64> = (0..repeats * per).map(|_| cli(&d, &["--version"]).wall_ms).collect();
    let mut out = json!({"scale": d.scale, "query_set": set, "query_binary": query_bin.as_ref().map(|b| b.to_string_lossy()).unwrap_or_else(|| BIN.into()), "loadavg_start": load0, "results": results, "process_startup_ms": dist(&startup),
        "digest_section": {"lines": digest_lines, "bytes": digest_bytes}, "context_peek": context, "context_available": context_works, "loadavg_end": load_average()});
    out["compare_byte_checks"] = json!(byte_comparisons);
    write_results(&dir, &format!("queries-{}", std::env::var("SCALE_TAG").unwrap_or_default()), &out);
}

/// Live appends at `rate` events/second spread over the active sessions.
fn appender(rate: usize, stop: Arc<AtomicBool>, log: Arc<Mutex<Vec<(i64, String)>>>, state: Arc<Mutex<(Vec<Session>, Totals, Mix)>>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let tick = Duration::from_millis(100);
        let per_tick = (rate / 10).max(1);
        let mut next = 0usize;
        while !stop.load(Ordering::Relaxed) {
            let started = Instant::now();
            {
                let mut guard = state.lock().unwrap();
                let (sessions, totals, mix) = &mut *guard;
                let n = sessions.len();
                let mut remaining = per_tick;
                while remaining > 0 {
                    let k = remaining.min(4);
                    let s = &mut sessions[next % n];
                    let sid = s.sid.clone();
                    let usage = s.append(k, totals, mix);
                    // Per usage record: its append time and session.
                    let mut logged = log.lock().unwrap();
                    for at in usage { logged.push((at, sid.clone())); }
                    remaining -= k;
                    next += 1;
                }
            }
            std::thread::sleep(tick.saturating_sub(started.elapsed()));
        }
    })
}

/// The pass loop: the ticker's telemetry pass every `cadence`, recording its
/// duration and, after each pass, how many delta entries each active session
/// has in the ledger (the derived view).
/// Per pass its record; after each pass the time and each session's ledger delta count; the thread's CPU ms.
type PassLoop = (Vec<Value>, Vec<(i64, std::collections::BTreeMap<String, i64>)>, f64);

fn pass_loop(project: PathBuf, cadence: Duration, stop: Arc<AtomicBool>, sids: Vec<String>) -> std::thread::JoinHandle<PassLoop> {
    std::thread::spawn(move || {
        telemetry::background::idle_priority(|warning| eprintln!("{warning}"));
        let (mut passes, mut views) = (Vec::new(), Vec::new());
        let cpu0 = thread_cpu_ms();
        while !stop.load(Ordering::Relaxed) {
            let started = Instant::now();
            let (ms, collected, errors, steps, accounting) = telemetry_pass_steps(&project);
            let seen = delta_counts(&project, &sids);
            views.push((unix_ms(), seen));
            passes.push(json!({"ms": ms, "bytes": collected.as_ref().map(|c| c.bytes), "records": collected.as_ref().map(|c| c.records),
                "budget_exhausted": collected.as_ref().map(|c| c.budget_exhausted), "errors": errors, "accounting": accounting,
                "steps": steps.into_iter().map(|(k, v)| (k.to_owned(), json!(v))).collect::<serde_json::Map<_, _>>()}));
            std::thread::sleep(cadence.saturating_sub(started.elapsed()));
        }
        (passes, views, thread_cpu_ms() - cpu0)
    })
}

fn delta_counts(project: &Path, sids: &[String]) -> std::collections::BTreeMap<String, i64> {
    let db = rusqlite::Connection::open_with_flags(project.join(".state/telemetry.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut stmt = db.prepare("SELECT count(*) FROM usage_entries WHERE session_id=?1 AND basis='delta'").unwrap();
    sids.iter().map(|s| (s.clone(), stmt.query_row([s], |r| r.get::<_, i64>(0)).unwrap())).collect()
}

/// Freshness: for each appended usage record, the time from its append to the
/// end of the first pass after which the ledger holds it.
fn freshness(appends: &[(i64, String)], base: &std::collections::BTreeMap<String, i64>, views: &[(i64, std::collections::BTreeMap<String, i64>)]) -> (Vec<f64>, usize) {
    let mut per_session: std::collections::BTreeMap<&str, Vec<i64>> = Default::default();
    for (at, sid) in appends { per_session.entry(sid).or_default().push(*at); }
    let (mut out, mut unseen) = (Vec::new(), 0);
    for (sid, times) in per_session {
        let start = base.get(sid).copied().unwrap_or(0);
        for (k, at) in times.iter().enumerate() {
            let needed = start + k as i64 + 1;
            match views.iter().find(|(_, seen)| seen.get(sid).copied().unwrap_or(0) >= needed) {
                Some((when, _)) => out.push((*when - *at) as f64),
                None => unseen += 1,
            }
        }
    }
    (out, unseen)
}

/// Phase 4: derived-view freshness at 100 events/s, then the doc 10 fault
/// burst (10× for 60 s) and its drain, with the pass at `SCALE_CADENCE_MS`
/// (default 1000: the fastest cadence the harness drives; the ticker's own
/// default is one pass per 300 s). Afterwards the usage gates must hold.
#[test]
#[ignore]
fn scale_4_freshness_burst() {
    let dir = data_dir();
    let mut d = Dataset::load(&dir);
    let cadence = Duration::from_millis(env_usize("SCALE_CADENCE_MS", 1000) as u64);
    let steady = Duration::from_secs(env_usize("SCALE_STEADY_S", 120) as u64);
    let burst = Duration::from_secs(env_usize("SCALE_BURST_S", 60) as u64);
    let sids: Vec<String> = d.active.iter().map(|s| s.sid.clone()).collect();
    let load0 = load_average();
    // Warm: one pass drains anything pending.
    let (warm, _, _) = telemetry_pass(&d.project);
    let state = Arc::new(Mutex::new((std::mem::take(&mut d.active), d.totals, d.mix.clone())));
    let mut phases = serde_json::Map::new();
    for (label, rate, length) in [("steady_100_per_s", 100, steady), ("burst_1000_per_s", 1000, burst), ("drain_100_per_s", 100, Duration::from_secs(30))] {
        let base = delta_counts(&d.project, &sids);
        let stop = Arc::new(AtomicBool::new(false));
        let log = Arc::new(Mutex::new(Vec::new()));
        let passes = pass_loop(d.project.clone(), cadence, stop.clone(), sids.clone());
        let writer = appender(rate, stop.clone(), log.clone(), state.clone());
        std::thread::sleep(length);
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
        let (records, mut views, cpu) = passes.join().unwrap();
        // Settle: passes until the ledger holds every appended record (bounded).
        let settle = Instant::now();
        let mut settle_passes = 0;
        let expected: i64 = state.lock().unwrap().1.records;
        loop {
            let (_, _, _) = telemetry_pass(&d.project);
            settle_passes += 1;
            views.push((unix_ms(), delta_counts(&d.project, &sids)));
            let db = rusqlite::Connection::open_with_flags(d.sidecar(), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
            let have: i64 = db.query_row("SELECT count(*) FROM usage_entries WHERE basis='delta'", [], |r| r.get(0)).unwrap();
            if have >= expected || settle_passes >= 200 { break; }
        }
        let appended = log.lock().unwrap().clone();
        let (fresh, unseen) = freshness(&appended, &base, &views);
        let ms: Vec<f64> = records.iter().map(|p| p["ms"].as_f64().unwrap()).collect();
        let exhausted = records.iter().filter(|p| p["budget_exhausted"] == true).count();
        let errors: Vec<&Value> = records.iter().filter(|p| p["errors"].as_array().is_some_and(|e| !e.is_empty())).collect();
        let steps: serde_json::Map<String, Value> = records.first().and_then(|p| p["steps"].as_object()).map(|first| first.keys().map(|k| (k.clone(),
            dist(&records.iter().filter_map(|p| p["steps"][k].as_f64()).collect::<Vec<_>>()))).collect()).unwrap_or_default();
        phases.insert(label.into(), json!({"rate_per_s": rate, "pass_steps_ms": steps, "seconds": length.as_secs(), "usage_records_appended": appended.len(), "passes": records.len(),
            "pass_ms": dist(&ms), "accounting_passes": records, "pass_cpu_ms_total": cpu, "passes_budget_exhausted": exhausted, "pass_errors": errors.len(), "first_errors": errors.iter().take(3).collect::<Vec<_>>(),
            "freshness_ms": dist(&fresh), "unseen_after_settle": unseen, "settle": {"passes": settle_passes, "ms": settle.elapsed().as_secs_f64() * 1e3},
            "sidecar_bytes": sidecar_bytes(&d), "loadavg": load_average()}));
    }
    let (sessions, totals, mix) = Arc::try_unwrap(state).ok().unwrap().into_inner().unwrap();
    (d.active, d.totals, d.mix) = (sessions, totals, mix);
    d.save(&dir);
    let violations = usage_gates(&d);
    let out = json!({"scale": d.scale, "cadence_ms": cadence.as_millis() as u64, "warm_pass_ms": warm, "loadavg_start": load0, "phases": phases,
        "process": self_rusage(), "violations": violations, "loadavg_end": load_average()});
    write_results(&dir, &format!("freshness-{}", std::env::var("SCALE_TAG").unwrap_or_default()), &out);
    assert_eq!(out["violations"], json!([]));
}

/// Controller operations: an admission decision (read path) and a
/// reconciliation commit (the observation record the ticker writes each pass),
/// every 100 ms. Returns `(admission_ms, reconcile_ms)` samples.
fn controller(d: &Dataset, store: &mut herdr_farm::store::SqliteStore, stop: &AtomicBool, until: Instant) -> (Vec<f64>, Vec<f64>) {
    let (mut admission, mut reconcile) = (Vec::new(), Vec::new());
    let (binding, binding_revision, task_revision) = d.binding.clone();
    while Instant::now() < until && !stop.load(Ordering::Relaxed) {
        let started = Instant::now();
        let observed = herdr_farm::admission::admit_decision_observed(&d.project);
        admission.push(started.elapsed().as_secs_f64() * 1e3);
        observed.result.unwrap();
        let t = Instant::now();
        let head = store.current_head().unwrap();
        store.record_observations(head, &[herdr_farm::reconcile::RuntimeObservation { binding: binding.clone(), binding_revision,
            task_revision, observed_unix_ms: unix_ms(), collector: "herdr-git-v1".into(), config_digest: d.config_digest.clone(),
            ..herdr_farm::reconcile::RuntimeObservation::default() }]).unwrap();
        reconcile.push(t.elapsed().as_secs_f64() * 1e3);
        std::thread::sleep(Duration::from_millis(100).saturating_sub(started.elapsed()));
    }
    (admission, reconcile)
}

/// Phase 3: controller latency with telemetry disabled (the ticker's
/// telemetry pass off: `HERDR_PROJECTS_TELEMETRY_COLLECT_SECS=0`; no reader)
/// and enabled (the pass every `SCALE_CADENCE_MS`, a pane refresh, digest
/// and export page every `SCALE_READER_MS`), under identical live appends
/// (100 events/s). Blocks of `SCALE_BLOCK_S` alternate off/on
/// `SCALE_REPEATS` times; at most four threads run.
#[test]
#[ignore]
fn scale_3_controller() {
    let dir = data_dir();
    let mut d = Dataset::load(&dir);
    let repeats = env_usize("SCALE_REPEATS", 5);
    let block = Duration::from_secs(env_usize("SCALE_BLOCK_S", 30) as u64);
    let cadence = Duration::from_millis(env_usize("SCALE_CADENCE_MS", 1000) as u64);
    let reader_every = Duration::from_millis(env_usize("SCALE_READER_MS", 1000) as u64);
    let sids: Vec<String> = d.active.iter().map(|s| s.sid.clone()).collect();
    let config = d.home().join(".config/herdr-farm");
    fs::create_dir_all(&config).unwrap();
    let mut store = herdr_farm::store::SqliteStore::open(&d.state()).unwrap();
    let state = Arc::new(Mutex::new((std::mem::take(&mut d.active), d.totals, d.mix.clone())));
    let load0 = load_average();
    let mut blocks = Vec::new();
    for round in 0..repeats {
        let order = if round % 2 == 0 { [false, true] } else { [true, false] };
        for on in order {
            let stop = Arc::new(AtomicBool::new(false));
            let log = Arc::new(Mutex::new(Vec::new()));
            let writer = appender(100, stop.clone(), log, state.clone());
            let passes = on.then(|| pass_loop(d.project.clone(), cadence, stop.clone(), sids.clone()));
            // The operator's surfaces run in their own processes, as `watch`,
            // `context` and exports do: a pane refresh, the digest section and an
            // export page, every `SCALE_READER_MS`.
            let reader = on.then(|| {
                let (dir, stop) = (dir.clone(), stop.clone());
                std::thread::spawn(move || {
                    let d = Dataset::load(&dir);
                    let mut ms = Vec::new();
                    while !stop.load(Ordering::Relaxed) {
                        let started = Instant::now();
                        for args in [&["workspace", "show", "--json"][..], &["workspace", "digest"], &["export", "--metric", "M02", "--drill", "numerator", "--page-size", "100"]] {
                            telemetry(&d, args).ok();
                        }
                        ms.push(started.elapsed().as_secs_f64() * 1e3);
                        std::thread::sleep(reader_every.saturating_sub(started.elapsed()));
                    }
                    ms
                })
            });
            let cpu0 = thread_cpu_ms();
            let (admission, reconcile) = controller(&d, &mut store, &stop, Instant::now() + block);
            let controller_cpu = thread_cpu_ms() - cpu0;
            stop.store(true, Ordering::Relaxed);
            writer.join().unwrap();
            let pass = passes.map(|p| p.join().unwrap());
            let reads = reader.map(|r| r.join().unwrap());
            blocks.push(json!({"round": round, "telemetry": on, "admission_ms": dist(&admission), "reconcile_ms": dist(&reconcile), "controller_cpu_ms": controller_cpu,
                "pass_ms": pass.as_ref().map(|p| dist(&p.0.iter().map(|x| x["ms"].as_f64().unwrap()).collect::<Vec<_>>())),
                "pass_cpu_ms": pass.as_ref().map(|p| p.2), "reader_ms": reads.as_ref().map(|r| dist(r)), "loadavg": load_average(),
                "_admission": admission, "_reconcile": reconcile}));
        }
    }
    let pooled = |on: bool, key: &str| -> Vec<f64> { blocks.iter().filter(|b| b["telemetry"] == on).flat_map(|b| b[key].as_array().unwrap().iter().map(|x| x.as_f64().unwrap())).collect() };
    let per_block = |on: bool, key: &str, p: &str| -> Vec<f64> { blocks.iter().filter(|b| b["telemetry"] == on).map(|b| b[key][p].as_f64().unwrap()).collect() };
    let mut summary = serde_json::Map::new();
    for (key, raw) in [("admission_ms", "_admission"), ("reconcile_ms", "_reconcile")] {
        let (off, on) = (dist(&pooled(false, raw)), dist(&pooled(true, raw)));
        let change = |p: &str| (on[p].as_f64().unwrap() / off[p].as_f64().unwrap() - 1.0) * 100.0;
        summary.insert(key.into(), json!({"off": off, "on": on, "p50_increase_percent": change("p50"), "p95_increase_percent": change("p95"),
            "block_p50_off": noise(&per_block(false, key, "p50")), "block_p50_on": noise(&per_block(true, key, "p50")),
            "block_p95_off": noise(&per_block(false, key, "p95")), "block_p95_on": noise(&per_block(true, key, "p95"))}));
    }
    for b in &mut blocks { let o = b.as_object_mut().unwrap(); o.remove("_admission"); o.remove("_reconcile"); }
    let (sessions, totals, mix) = Arc::try_unwrap(state).ok().unwrap().into_inner().unwrap();
    (d.active, d.totals, d.mix) = (sessions, totals, mix);
    d.save(&dir);
    // Drain what the disabled blocks left, then the gates.
    for _ in 0..500 { let (_, c, _) = telemetry_pass(&d.project); if c.is_some_and(|c| !c.budget_exhausted && c.bytes == 0) { break; } }
    let violations = usage_gates(&d);
    let out = json!({"scale": d.scale, "cadence_ms": cadence.as_millis() as u64, "reader_ms": reader_every.as_millis() as u64,
        "sqlite_memstatus": if std::env::var("SCALE_SQLITE_MEMSTATUS").as_deref() == Ok("1") { "on (SQLite default)" } else { "off (as the binary)" }, "block_s": block.as_secs(), "loadavg_start": load0, "summary": summary, "blocks": blocks,
        "process": self_rusage(), "violations": violations, "loadavg_end": load_average()});
    write_results(&dir, &format!("controller-{}", std::env::var("SCALE_TAG").unwrap_or_default()), &out);
    assert_eq!(out["violations"], json!([]));
}

/// Phase 5: faults at scale. SQLite contention (racing collect/sync,
/// refresh/health and export-paging processes beside the controller), a full
/// sidecar during a backlog, a stalled exporter beside the pass and the
/// controller, and restart recovery (collect, sync and refresh killed
/// mid-pass, then one pass). Every gate afterwards.
#[test]
#[ignore]
fn scale_5_faults() {
    let dir = data_dir();
    let mut d = Dataset::load(&dir);
    let mut store = herdr_farm::store::SqliteStore::open(&d.state()).unwrap();
    let load0 = load_average();
    let mut out = serde_json::Map::new();
    let config = d.home().join(".config/herdr-farm");
    fs::create_dir_all(&config).unwrap();

    // Contention: three racing processes plus the controller (4 threads), 60 s, with a backlog to collect.
    for s in d.active.iter_mut() { s.append(200, &mut d.totals, &mut d.mix); }
    d.save(&dir);
    let stop = Arc::new(AtomicBool::new(false));
    let racers: Vec<_> = [vec!["collect", "|", "accounting", "sync"], vec!["analytics", "refresh", "|", "health", "evaluate"],
        vec!["export", "--metric", "M02", "--drill", "numerator", "--page-size", "500"]].into_iter().map(|script| {
        let (stop, dir) = (stop.clone(), dir.clone());
        let script: Vec<String> = script.iter().map(|s| s.to_string()).collect();
        std::thread::spawn(move || {
            let d = Dataset::load(&dir);
            let (mut walls, mut failures) = (Vec::new(), Vec::new());
            while !stop.load(Ordering::Relaxed) {
                for args in script.split(|a| a == "|") {
                    let run = telemetry(&d, &args.iter().map(String::as_str).collect::<Vec<_>>());
                    if run.code != 0 { failures.push(format!("{args:?}: {}", run.stderr.lines().last().unwrap_or(""))); } else { walls.push(run.wall_ms); }
                }
            }
            (script.join(" "), walls, failures)
        })
    }).collect();
    let (admission, reconcile) = controller(&d, &mut store, &stop, Instant::now() + Duration::from_secs(60));
    stop.store(true, Ordering::Relaxed);
    let raced: Vec<Value> = racers.into_iter().map(|r| r.join().unwrap()).map(|(name, walls, failures)| json!({"loop": name, "wall_ms": dist(&walls), "failures": failures})).collect();
    out.insert("contention".into(), json!({"loops": raced, "admission_ms": dist(&admission), "reconcile_ms": dist(&reconcile), "loadavg": load_average()}));

    // Full spool: a backlog, then a collect whose sidecar may not grow.
    for s in d.active.iter_mut() { s.append(400, &mut d.totals, &mut d.mix); }
    d.save(&dir);
    let refused = refused_collect(&d);
    let gaps = |d: &Dataset, recovery: &str| rusqlite::Connection::open(d.sidecar()).unwrap()
        .query_row("SELECT count(*) FROM coverage_gaps WHERE reason='sidecar_write_failed' AND recovery=?1", [recovery], |r| r.get::<_, i64>(0)).unwrap();
    let pending = gaps(&d, "pending");
    let recover = Instant::now();
    let mut recover_runs = 0;
    loop { recover_runs += 1; if telemetry(&d, &["collect"]).ok().json()["collected"]["budget_exhausted"] == false { break; } }
    out.insert("full_spool".into(), json!({"refused_collect": {"code": refused.code, "wall_ms": refused.wall_ms, "stderr": refused.stderr.lines().last()},
        "gaps_pending_after_refusal": pending, "recovery_ms": recover.elapsed().as_secs_f64() * 1e3, "recovery_collects": recover_runs,
        "gaps_pending_after_recovery": gaps(&d, "pending"), "gaps_recovered": gaps(&d, "recovered")}));

    // Stalled exporter: stdout destination whose reader never reads, beside passes and the controller.
    let (mut stalled, _reader) = stalled_exporter(&d);
    let stop = Arc::new(AtomicBool::new(false));
    let passes = pass_loop(d.project.clone(), Duration::from_secs(1), stop.clone(), Vec::new());
    for s in d.active.iter_mut() { s.append(100, &mut d.totals, &mut d.mix); }
    let (admission, reconcile) = controller(&d, &mut store, &stop, Instant::now() + Duration::from_secs(30));
    stop.store(true, Ordering::Relaxed);
    let (records, _, _) = passes.join().unwrap();
    let still_blocked = stalled.try_wait().unwrap().is_none();
    let _ = stalled.kill();
    let _ = stalled.wait();
    let _ = fs::remove_file(config.join("telemetry-export.toml"));
    out.insert("stalled_exporter".into(), json!({"exporter_still_blocked": still_blocked, "passes": records.len(),
        "pass_ms": dist(&records.iter().map(|p| p["ms"].as_f64().unwrap()).collect::<Vec<_>>()), "admission_ms": dist(&admission), "reconcile_ms": dist(&reconcile),
        "sidecar_bytes": sidecar_bytes(&d)}));
    d.save(&dir);

    // Restart recovery: kill each step mid-run at growing delays, then one pass.
    for s in d.active.iter_mut() { s.append(400, &mut d.totals, &mut d.mix); }
    d.save(&dir);
    let mut kills = Vec::new();
    for (n, args) in [vec!["collect"], vec!["accounting", "sync"], vec!["analytics", "refresh"], vec!["collect"], vec!["health", "evaluate"]].into_iter().enumerate() {
        let mut all = vec!["telemetry", "demo"];
        all.extend(&args);
        let mut child = command(&d, &all).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        let delay = 50 + 150 * n as u64;
        std::thread::sleep(Duration::from_millis(delay));
        let finished = child.try_wait().unwrap().is_some();
        let _ = child.kill();
        let _ = child.wait();
        kills.push(json!({"step": args.join(" "), "delay_ms": delay, "finished_before_kill": finished}));
    }
    let restart = Instant::now();
    let mut restart_passes = 0;
    loop { restart_passes += 1; let (_, c, _) = telemetry_pass(&d.project); if c.is_some_and(|c| !c.budget_exhausted) || restart_passes > 500 { break; } }
    // The pass syncs the ledger; the CLI then syncs once more with its own budget.
    telemetry(&d, &["accounting", "sync"]).ok();
    // A rebuild compares with the latest revision, so refresh first (the tick refreshes at
    // most once a minute). Cells that depend on the wall clock (M35's activity windows)
    // restate as time passes (contracts-analytics.md §4): one retry absorbs that.
    let mut drifted = Vec::new();
    let (mut identical, mut verify) = (false, Value::Null);
    for _ in 0..2 {
        telemetry(&d, &["analytics", "refresh"]).ok();
        verify = telemetry(&d, &["analytics", "rebuild", "--verify"]).ok().json();
        identical = verify["cells"].as_array().is_some_and(|c| c.iter().all(|c| c["identical"] == true && c["stored_intact"] == true));
        if identical { break; }
        drifted.extend(verify["cells"].as_array().into_iter().flatten().filter(|c| c["identical"] != true).map(|c| c["cell"]["metric"].clone()));
    }
    out.insert("restart".into(), json!({"kills": kills, "recovery_ms": restart.elapsed().as_secs_f64() * 1e3, "passes": restart_passes, "analytics_rebuild_identical": identical,
        "restated_on_first_verify": drifted}));

    let violations = usage_gates(&d);
    let out = json!({"scale": d.scale, "loadavg_start": load0, "faults": out, "process": self_rusage(), "violations": violations,
        "loadavg_end": load_average()});
    write_results(&dir, &format!("faults-{}", std::env::var("SCALE_TAG").unwrap_or_default()), &out);
    assert_eq!(out["violations"], json!([]));
    assert!(identical, "{verify}");
}

/// Old event-time records arrive in an already refreshed window, via a slow
/// appender (one bounded chunk per 100 ms). Wall-clock arrival is separate from
/// event time. Existing session/response identities and counter semantics hold.
fn late_slow(d: &mut Dataset, chunks: usize) -> Value {
    telemetry(d, &["analytics", "refresh"]).ok();
    let revisions = telemetry(d, &["analytics", "revisions", "--metric", "M08"]).ok().json();
    let seq = revisions["revisions"].as_array().unwrap().last().unwrap()["revision"].as_i64().unwrap();
    let pinned = |d: &Dataset| {
        let mut v = telemetry(d, &["query", "--metric", "M08", "--as-of-seq", &seq.to_string(), "--json"]).ok().json();
        v.as_object_mut().unwrap().remove("query_unix_ms");
        for k in ["current_revision", "restated", "superseded_by"] { v["results"][0]["projection"].as_object_mut().unwrap().remove(k); }
        v
    };
    let before = pinned(d);
    let input = d.totals.input;
    let started = Instant::now();
    let event_ms = d.producers.attention[0].1 + 300_000;
    let mut collected = Vec::new();
    for chunk in 0..chunks {
        let s = &mut d.active[chunk % d.scale.active];
        let mut lines = String::new();
        for n in 0..20 {
            lines += &s.line(event_ms + n * 500, &mut d.totals, &mut d.mix);
            lines.push('\n');
        }
        fs::OpenOptions::new().append(true).open(&s.path).unwrap().write_all(lines.as_bytes()).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let run = telemetry(d, &["collect"]).ok();
        collected.push(run.wall_ms);
        telemetry(d, &["accounting", "sync"]).ok();
    }
    let refresh = telemetry(d, &["analytics", "refresh"]).ok().json();
    assert!(refresh["appended"].as_array().unwrap().iter().any(|a| a["cell"]["metric"] == "M08" && a["kind"] == "restatement" && a["supersedes"] == seq), "{refresh}");
    assert_eq!(pinned(d), before, "late arrivals changed a pinned as-of revision");
    let current = telemetry(d, &["query", "--metric", "M08", "--json"]).ok().json();
    assert_eq!(current["results"][0]["value"], json!(d.totals.input), "{current}");
    assert!(d.totals.input > input);
    assert_eq!(usage_gates(d), Vec::<String>::new());
    let verify = telemetry(d, &["analytics", "rebuild", "--verify"]).ok().json();
    assert!(verify["cells"].as_array().unwrap().iter().all(|c| c["identical"] == true && c["stored_intact"] == true), "{verify}");
    json!({"chunks": chunks, "event_unix_ms": event_ms, "arrival_unix_ms": unix_ms(), "appender_sleep_ms": 100,
        "late_input_tokens": d.totals.input - input, "collect_ms": dist(&collected), "wall_ms": started.elapsed().as_secs_f64() * 1e3,
        "as_of_unchanged": true, "rebuild_identical": true})
}

#[test]
fn scale_producers_restate_and_replay() {
    let mut d = generate(Fixture::new(), Scale { attempts: 60, active: 4, events: 1_000, homes: 2, seed: 51 });
    let _cleanup = Cleanup(d.base.clone());
    let canonical = canonical_digest(&d);
    telemetry(&d, &["collect"]).ok();
    telemetry(&d, &["accounting", "sync"]).ok();
    // A real quality pass first records an unavailable repository observation.
    // The declared simulated observation must settle that prior row, just as
    // the quality producer settles unavailable observations after recovery.
    rusqlite::Connection::open(d.sidecar()).unwrap().execute("DELETE FROM proxy_signals WHERE task_id=?1", [&d.producers.proxies[0].task]).unwrap();
    telemetry(&d, &["quality", "collect"]).ok();
    assert_eq!(telemetry(&d, &["quality", "report"]).ok().json()["metrics"]["M45"]["excluded"]["weakening_unavailable"], json!(1));
    plant_producers(&d);
    assert_eq!(usage_gates(&d), Vec::<String>::new());
    late_slow(&mut d, 2);
    let rows = ledger_rows(&d.sidecar());
    // Compare the same pipeline on both sidecars, including quality collection.
    telemetry(&d, &["quality", "collect"]).ok();
    let quality = telemetry(&d, &["quality", "report"]).ok().json();
    for suffix in ["", "-wal", "-shm"] { fs::remove_file(format!("{}{suffix}", d.sidecar().display())).ok(); }
    plant_producers(&d);
    telemetry(&d, &["collect"]).ok();
    telemetry(&d, &["accounting", "sync"]).ok();
    assert_eq!(usage_gates(&d), Vec::<String>::new());
    telemetry(&d, &["quality", "collect"]).ok();
    assert_eq!(telemetry(&d, &["quality", "report"]).ok().json(), quality);
    assert_eq!(ledger_rows(&d.sidecar()), rows);
    assert_eq!(canonical_digest(&d), canonical);
}

#[test]
#[ignore]
fn scale_7_late_slow() {
    let dir = data_dir();
    let mut d = Dataset::load(&dir);
    let canonical = canonical_digest(&d);
    let load = load_average();
    let repeats: Vec<_> = (0..env_usize("SCALE_REPEATS", 3)).map(|_| late_slow(&mut d, 10)).collect();
    d.save(&dir);
    assert_eq!(canonical_digest(&d), canonical);
    write_results(&dir, "late-slow", &json!({"scale": d.scale, "runs": repeats, "loadavg_start": load, "loadavg_end": load_average()}));
}


/// Four projects share one non-blocking controller, one collection/accounting
/// worker, one bounded derived-lane worker and one operator reader, like the
/// ticker. No per-project threads. SCALE_FAIRNESS_DERIVED=0 measures the
/// original single-worker scheduling with the same accounting implementation.
#[test]
#[ignore]
fn scale_6_fairness() {
    let dir = data_dir();
    let load = load_average();
    let mut datasets = vec![Dataset::load(&dir)];
    for i in 1..4 {
        let sub = dir.join(format!("light-{i}"));
        fs::create_dir_all(&sub).unwrap();
        let d = if Dataset::manifest(&sub).exists() { Dataset::load(&sub) } else {
            let d = generate(Fixture::new(), Scale { attempts: 200, active: 4, events: 1_000, homes: 2, seed: 5_100 + i });
            d.save(&sub);
            d
        };
        datasets.push(d);
    }
    for d in &datasets { telemetry(d, &["collect"]).ok(); telemetry(d, &["accounting", "sync"]).ok(); }
    let sids: Vec<Vec<String>> = datasets.iter().map(|d| d.active.iter().map(|s| s.sid.clone()).collect()).collect();
    let projects: Vec<PathBuf> = datasets.iter().map(|d| d.project.clone()).collect();
    let state = Arc::new(Mutex::new(datasets.iter_mut().map(|d| (std::mem::take(&mut d.active), d.totals, d.mix.clone())).collect::<Vec<_>>()));
    let mut stores: Vec<_> = datasets.iter().map(|d| herdr_farm::store::SqliteStore::open(&d.state()).unwrap()).collect();
    let cadence = Duration::from_millis(env_usize("SCALE_CADENCE_MS", 1_000) as u64);
    let reader_every = Duration::from_millis(env_usize("SCALE_READER_MS", 5_000) as u64);
    let length = Duration::from_secs(env_usize("SCALE_FAIRNESS_S", 15) as u64);
    let mut rounds = Vec::new();
    for round in 0..env_usize("SCALE_REPEATS", 3) {
        let base: Vec<_> = projects.iter().zip(&sids).map(|(p, ids)| delta_counts(p, ids)).collect();
        let stop = Arc::new(AtomicBool::new(false));
        let log = Arc::new(Mutex::new(vec![Vec::<(i64, String)>::new(); 4]));
        let writer = {
            let (state, stop, log) = (state.clone(), stop.clone(), log.clone());
            std::thread::spawn(move || {
                let mut cursor = 0;
                while !stop.load(Ordering::Relaxed) {
                    let started = Instant::now();
                    let mut state = state.lock().unwrap();
                    let mut log = log.lock().unwrap();
                    for (i, (sessions, totals, mix)) in state.iter_mut().enumerate() {
                        let index = cursor % sessions.len();
                        let s = &mut sessions[index];
                        let sid = s.sid.clone();
                        for at in s.append(if i == 0 { 10 } else { 1 }, totals, mix) { log[i].push((at, sid.clone())); }
                    }
                    cursor += 1;
                    drop(log);
                    drop(state);
                    std::thread::sleep(Duration::from_millis(100).saturating_sub(started.elapsed()));
                }
            })
        };
        let worker = {
            let (projects, sids, stop) = (projects.clone(), sids.clone(), stop.clone());
            std::thread::spawn(move || {
                telemetry::background::idle_priority(|warning| eprintln!("{warning}"));
                let (derived_tx, derived_rx) = std::sync::mpsc::channel();
                let split = std::env::var("SCALE_FAIRNESS_DERIVED").as_deref() != Ok("0");
                let mut derived = split.then(|| telemetry::background::DeferredLanes::new(move |project| {
                    telemetry::background::idle_priority(|warning| eprintln!("{warning}"));
                    let started = Instant::now();
                    let mut errors = Vec::new();
                    for lane in telemetry::LANES.iter().filter(|lane| lane.stream != "accounting") {
                        if let Err(e) = (lane.tick)(project, codex::Budget::TICK) { errors.push(format!("{}: {e:#}", lane.stream)); }
                    }
                    derived_tx.send(json!({"project": project, "ms": started.elapsed().as_secs_f64() * 1e3, "errors": errors})).unwrap();
                }).unwrap());
                let mut views = vec![Vec::new(); 4];
                let mut passes = vec![Vec::new(); 4];
                let mut diagnostics = vec![Vec::new(); 4];
                while !stop.load(Ordering::Relaxed) {
                    let started = Instant::now();
                    for (i, project) in projects.iter().enumerate() {
                        let (ms, collected, errors, steps, accounting) = if split { telemetry_core_steps(project) } else { telemetry_pass_steps(project) };
                        if let Some(derived) = &derived { assert!(derived.submit(project)); }
                        diagnostics[i].push(json!({"ms": ms, "collected": collected, "accounting": accounting, "steps": steps.into_iter().map(|(k, v)| (k.to_owned(), json!(v))).collect::<serde_json::Map<_, _>>()}));
                        assert!(errors.is_empty(), "{errors:?}");
                        passes[i].push(ms);
                        views[i].push((unix_ms(), delta_counts(project, &sids[i])));
                    }
                    std::thread::sleep(cadence.saturating_sub(started.elapsed()));
                }
                if let Some(derived) = &mut derived { derived.stop(); }
                (views, passes, diagnostics, derived, derived_rx)
            })
        };
        let reader = {
            let (stop, dirs) = (stop.clone(), datasets.iter().map(|d| d.base.clone()).collect::<Vec<_>>());
            // The hot manifest is under SCALE_DATA, while light manifests are
            // saved under their fixture bases for isolated CLI environments.
            for d in &datasets { d.save(&d.base); }
            std::thread::spawn(move || {
                let datasets: Vec<_> = dirs.iter().map(|p| Dataset::load(p)).collect();
                let mut panel = vec![Vec::new(); 4];
                let mut digest = vec![Vec::new(); 4];
                while !stop.load(Ordering::Relaxed) {
                    let started = Instant::now();
                    for (i, d) in datasets.iter().enumerate() {
                        panel[i].push(telemetry(d, &["workspace", "show", "--json"]).ok().wall_ms);
                        digest[i].push(telemetry(d, &["workspace", "digest"]).ok().wall_ms);
                    }
                    std::thread::sleep(reader_every.saturating_sub(started.elapsed()));
                }
                (panel, digest)
            })
        };
        let until = Instant::now() + length;
        let mut admission = vec![Vec::new(); 4];
        let mut reconcile = vec![Vec::new(); 4];
        while Instant::now() < until {
            for (i, d) in datasets.iter().enumerate() {
                let (a, r) = controller(d, &mut stores[i], &stop, Instant::now() + Duration::from_millis(1));
                admission[i].extend(a);
                reconcile[i].extend(r);
            }
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
        let (mut views, passes, diagnostics, mut derived, derived_rx) = worker.join().unwrap();
        let mut results = Vec::new();
        for i in 0..4 {
            // Drain, but retain the true append-to-first-visible duration.
            for _ in 0..200 {
                let (_, c, errors, _, _) = if derived.is_some() { telemetry_core_steps(&projects[i]) } else { telemetry_pass_steps(&projects[i]) };
                assert!(errors.is_empty(), "{errors:?}");
                views[i].push((unix_ms(), delta_counts(&projects[i], &sids[i])));
                if c.is_some_and(|c| !c.budget_exhausted) { break; }
            }
            let (fresh, unseen) = freshness(&log.lock().unwrap()[i], &base[i], &views[i]);
            assert_eq!(unseen, 0);
            results.push(json!({"project": i, "hot": i == 0, "admission_ms": dist(&admission[i]), "reconcile_ms": dist(&reconcile[i]),
                "freshness_ms": dist(&fresh), "pass_ms": dist(&passes[i]), "accounting_passes": diagnostics[i],
                "appended_usage": fresh.len(), "unseen": unseen}));
        }
        // Drain before joining a slow surface reader: its last query must not
        // artificially delay our observation of already collectable records.
        let (panel, digest) = reader.join().unwrap();
        for i in 0..4 {
            results[i]["panel_ms"] = dist(&panel[i]);
            results[i]["digest_ms"] = dist(&digest[i]);
        }
        if let Some(derived) = &mut derived {
            while !derived.is_finished() { std::thread::sleep(Duration::from_millis(20)); }
            derived.reap();
        }
        let derived_passes: Vec<Value> = derived_rx.try_iter().collect();
        assert!(derived_passes.iter().all(|pass| pass["errors"].as_array().unwrap().is_empty()), "{derived_passes:?}");
        rounds.push(json!({"round": round, "projects": results, "derived_passes": derived_passes, "loadavg": load_average()}));
    }
    let state = Arc::try_unwrap(state).ok().unwrap().into_inner().unwrap();
    for (i, (d, (active, totals, mix))) in datasets.iter_mut().zip(state).enumerate() {
        (d.active, d.totals, d.mix) = (active, totals, mix);
        d.save(&if i == 0 { dir.clone() } else { dir.join(format!("light-{i}")) });
        assert_eq!(usage_gates(d), Vec::<String>::new());
    }
    // Fixed before measurement: light p95 <= doc 10's 5 s freshness target.
    // Report misses before asserting; a failed target is certification evidence.
    let fair = rounds.iter().all(|r| r["projects"].as_array().unwrap().iter().skip(1).all(|p|
        p["freshness_ms"]["n"].as_u64().unwrap() > 0 && p["freshness_ms"]["p95"].as_f64().unwrap() <= 5_000.0));
    write_results(&dir, "fairness", &json!({"hot_scale": datasets[0].scale, "light_events": 1_000, "projects": 4,
        "cadence_ms": cadence.as_millis(), "reader_ms": reader_every.as_millis(), "round_seconds": length.as_secs(), "derived_worker": std::env::var("SCALE_FAIRNESS_DERIVED").as_deref() != Ok("0"),
        "criterion": "each light project has observed usage and p95 append-to-ledger freshness <= 5000 ms in every round", "fair": fair,
        "rounds": rounds, "loadavg_start": load, "loadavg_end": load_average()}));
    assert!(fair, "light-project freshness exceeds the fixed 5 s fairness criterion; see results-fairness.json");
}

/// Isolate accounting RSS and latency after a bounded fleet append. The optional
/// executable override compares a preserved pre-change CLI on the same dataset;
/// the normal gate and every answer still use the public CLI surfaces.
#[test]
#[ignore = "100k accounting pass resource measurement; on-disk SCALE_DATA required"]
fn scale_8_accounting_pass() {
    let dir = data_dir();
    let mut d = Dataset::load(&dir);
    let canonical = canonical_digest(&d);
    let run_cli = |d: &Dataset, args: &[&str]| match std::env::var_os("SCALE_ACCOUNTING_BIN") {
        None => telemetry(d, args),
        Some(bin) => measure({
            let mut c = Command::new(bin);
            c.env_clear().env("HOME", d.home()).env("PATH", "/usr/bin:/bin").env("HERDR_BIN_PATH", "/bin/false")
                .args(["--root", d.root.to_str().unwrap(), "telemetry", "demo"]).args(args);
            c
        }),
    }.ok();
    run_cli(&d, &["accounting", "sync"]);
    let load0 = load_average();
    let mut runs = Vec::new();
    for _ in 0..env_usize("SCALE_REPEATS", 3) {
        for session in &mut d.active { session.append(16, &mut d.totals, &mut d.mix); }
        run_cli(&d, &["collect"]);
        runs.push(run_cli(&d, &["accounting", "sync"]));
    }
    d.save(&dir);
    let violations = usage_gates(&d);
    write_results(&dir, &format!("accounting-pass-{}", std::env::var("SCALE_TAG").unwrap_or_default()), &json!({
        "scale": d.scale, "loadavg_start": load0, "loadavg_end": load_average(),
        "sync_ms": dist(&runs.iter().map(|r| r.wall_ms).collect::<Vec<_>>()),
        "maxrss_kib": runs.iter().map(|r| r.maxrss_kib).max(), "runs": runs, "violations": violations,
        "canonical_unchanged": canonical_digest(&d) == canonical}));
    assert!(violations.is_empty(), "{violations:?}");
    assert_eq!(canonical_digest(&d), canonical);
}

/// Three foreground health evaluations over the same prepared dataset. An
/// optional preserved baseline CLI allows the identical persisted inputs to
/// exercise the old rule implementation; each evaluation is a public workflow.
#[test]
#[ignore = "100k health resource measurement; on-disk SCALE_DATA required"]
fn scale_9_health_evaluate() {
    let dir = data_dir();
    let d = Dataset::load(&dir);
    let load = load_average();
    let canonical = canonical_digest(&d);
    let mut runs = Vec::new();
    for _ in 0..env_usize("SCALE_REPEATS", 3) {
        let args = ["telemetry", "demo", "health", "evaluate", "--json"];
        let mut cmd = match std::env::var_os("SCALE_HEALTH_BIN") {
            Some(bin) => {
                let mut c = Command::new(bin);
                c.env_clear().env("HOME", d.home()).env("PATH", "/usr/bin:/bin").env("HERDR_BIN_PATH", "/bin/false")
                    .args(["--root", d.root.to_str().unwrap()]).args(args);
                c
            }
            None => command(&d, &args),
        };
        cmd.stdout(Stdio::piped());
        let run = measure(cmd).ok();
        assert_eq!(run.json()["recorded"], true);
        runs.push(json!({"wall_ms": run.wall_ms, "user_ms": run.user_ms, "sys_ms": run.sys_ms, "maxrss_kib": run.maxrss_kib}));
    }
    let violations = usage_gates(&d);
    write_results(&dir, &format!("health-evaluate-{}", std::env::var("SCALE_TAG").unwrap_or_default()), &json!({
        "scale": d.scale, "runs": runs, "wall_ms": dist(&runs.iter().map(|r| r["wall_ms"].as_f64().unwrap()).collect::<Vec<_>>()),
        "loadavg_start": load, "loadavg_end": load_average(), "canonical_unchanged": canonical == canonical_digest(&d), "violations": violations,
    }));
    assert_eq!(canonical, canonical_digest(&d));
    assert!(violations.is_empty(), "{violations:?}");
}

/// Repeat refresh on exactly the same collected inputs. An executable override
/// lets the before CLI and after CLI share the dataset without changing its
/// events. Each CLI's own peak RSS is sampled by the existing process harness.
#[test]
#[ignore = "100k analytics refresh resource measurement; on-disk SCALE_DATA required"]
fn scale_9_analytics_refresh() {
    let dir = data_dir();
    let d = Dataset::load(&dir);
    let canonical = canonical_digest(&d);
    let load0 = load_average();
    let mut runs = Vec::new();
    for _ in 0..env_usize("SCALE_REPEATS", 3) {
        let run = match std::env::var_os("SCALE_ANALYTICS_BIN") {
            None => telemetry(&d, &["analytics", "refresh"]),
            Some(bin) => measure({
                let mut c = Command::new(bin);
                c.env_clear().env("HOME", d.home()).env("PATH", "/usr/bin:/bin").env("HERDR_BIN_PATH", "/bin/false")
                    .args(["--root", d.root.to_str().unwrap(), "telemetry", "demo", "analytics", "refresh"]);
                c
            }),
        }.ok();
        runs.push(run);
    }
    let violations = usage_gates(&d);
    write_results(&dir, &format!("analytics-refresh-{}", std::env::var("SCALE_TAG").unwrap_or_default()), &json!({
        "scale": d.scale, "loadavg_start": load0, "loadavg_end": load_average(),
        "refresh_ms": dist(&runs.iter().map(|r| r.wall_ms).collect::<Vec<_>>()),
        "maxrss_kib": runs.iter().map(|r| r.maxrss_kib).max(), "runs": runs, "violations": violations,
        "canonical_unchanged": canonical_digest(&d) == canonical}));
    assert!(violations.is_empty(), "{violations:?}");
    assert_eq!(canonical_digest(&d), canonical);
}

/// DG3 produced-signal scale sample, separate from the unchanged load oracle.
/// Seed one observed interval spanning the generator's accepted transitions,
/// then measure real CLI query/report/refresh with a hand-computed tasks/hour.
#[test]
#[ignore = "100k M03 operating observations; on-disk SCALE_DATA required"]
fn scale_10_operating_throughput() {
    let dir=data_dir();
    let d=Dataset::load(&dir);
    let canonical=canonical_digest(&d);
    let load0=load_average();
    let from=d.producers.proxies.iter().map(|p|p.at).min().unwrap()-1000;
    // Generator receipts occur 1000 ms after each proxy's timestamp.
    let to=d.producers.proxies.iter().map(|p|p.at).max().unwrap()+2000;
    let operating_ms=to-from;
    telemetry::operating::observe(&d.project,"dg3-scale-fixture",true,1,from,operating_ms).unwrap();
    telemetry::operating::observe(&d.project,"dg3-scale-fixture",true,1,to,operating_ms).unwrap();
    let expected=format!("{}/{}",d.producers.proxies.len() as i128*3_600_000,i128::from(operating_ms));
    let (from,to)=(from.to_string(),to.to_string());
    let mut queries=Vec::new();
    let mut reports=Vec::new();
    let mut refreshes=Vec::new();
    for _ in 0..env_usize("SCALE_REPEATS",3) {
        let query=telemetry(&d,&["query","--metric","M03","--from",&from,"--to",&to,"--json"]).ok();
        let body=query.json();
        let m=&body["results"][0];
        assert_eq!(m["value"],expected);
        assert_eq!(m["numerator"],d.producers.proxies.len());
        assert_eq!(m["operating_ms"],operating_ms);
        assert_eq!(m["coverage"]["state"],"complete");
        queries.push(query.wall_ms);
        let report=telemetry(&d,&["report","--json"]).ok();
        assert_eq!(report.json()["metrics"]["M03"]["value"],expected);
        reports.push(report.wall_ms);
        refreshes.push(telemetry(&d,&["analytics","refresh","--metric","M03","--from",&from,"--to",&to]).ok().wall_ms);
    }
    assert_eq!(telemetry(&d,&["analytics","rebuild","--verify"]).ok().json()["identical"],true);
    let violations=usage_gates(&d);
    write_results(&dir,"operating-throughput",&json!({"scale":d.scale,"query_ms":dist(&queries),"report_ms":dist(&reports),"refresh_ms":dist(&refreshes),
        "accepted":d.producers.proxies.len(),"operating_ms":operating_ms,"expected":expected,"loadavg_start":load0,"loadavg_end":load_average(),
        "violations":violations,"canonical_unchanged":canonical==canonical_digest(&d)}));
    assert!(violations.is_empty(),"{violations:?}");
    assert_eq!(canonical,canonical_digest(&d));
}

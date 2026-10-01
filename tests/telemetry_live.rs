//! Opt-in paid certification workflows. Never selected by the ordinary suite.
#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)]
mod support;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use support::telemetry::*;
const MARKER: &str = "HERDR_LIVE_PRIVACY_LC0";
const COUNTERS: [&str; 6] = [
    "input_tokens",
    "cached_input_tokens",
    "cache_write_input_tokens",
    "output_tokens",
    "reasoning_output_tokens",
    "total_tokens",
];

// Shell words only: deliberately no expansion, substitution or shell execution.
fn words(text: &str) -> Vec<String> {
    let mut result = Vec::new();
    let (mut word, mut quote, mut escape, mut started) = (String::new(), None, false, false);
    for c in text.chars() {
        if escape {
            word.push(c);
            escape = false;
            continue;
        }
        if c == '\\' && quote != Some('\'') {
            escape = true;
            started = true;
            continue;
        }
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                word.push(c);
            }
        } else if c == '\'' || c == '"' {
            quote = Some(c);
            started = true;
        } else if c.is_whitespace() {
            if started {
                result.push(std::mem::take(&mut word));
                started = false;
            }
        } else {
            word.push(c);
            started = true;
        }
    }
    assert!(
        !escape && quote.is_none(),
        "invalid HERDR_LIVE_ARGS quoting"
    );
    if started {
        result.push(word);
    }
    result
}
struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        // Every child owns a process group, so timeout cleanup includes helpers.
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn command(bin: &Path, home: &Path, cwd: &Path) -> Command {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(bin);
    cmd.process_group(0);
    cmd.env_clear()
        .env(
            "PATH",
            std::env::var("HERDR_LIVE_PATH")
                .unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into()),
        )
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_RUNTIME_DIR", home.join("runtime"))
        .env("CODEX_HOME", home.join(".codex"))
        .env("GROK_HOME", home.join(".grok"))
        .current_dir(cwd)
        .stdin(Stdio::null());
    // Optional steward-supplied login token (e.g. Claude Code's
    // CLAUDE_CODE_OAUTH_TOKEN from `claude setup-token`): read from a private
    // file into this child's environment only, never logged or reported.
    if let (Ok(file), Ok(name)) = (std::env::var("HERDR_LIVE_TOKEN_FILE"), std::env::var("HERDR_LIVE_TOKEN_ENV")) {
        let token = std::fs::read_to_string(&file).expect("HERDR_LIVE_TOKEN_FILE readable");
        cmd.env(name, token.trim());
    }
    // Optional non-secret settings, one `NAME=value` per line, e.g. Muse's
    // `MUSE_AUTH_PATH` pointing at the owner's login file in place (owner
    // decision 2026-10-01: no copy, so token refreshes stay in one file).
    if let Ok(extra) = std::env::var("HERDR_LIVE_ENV") {
        for line in extra.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let (name, value) = line.split_once('=').expect("HERDR_LIVE_ENV lines are NAME=value");
            cmd.env(name, value);
        }
    }
    cmd
}
fn ready(process: &mut Process) -> Value {
    let stdout = process.0.stdout.take().unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let result = BufReader::new(stdout).read_line(&mut line);
        let _ = send.send((result, line));
    });
    let (result, line) = receive
        .recv_timeout(Duration::from_secs(15))
        .expect("receiver readiness timed out");
    result.unwrap();
    serde_json::from_str(&line).expect("receiver readiness JSON missing")
}
fn run(mut cmd: Command, scratch: &Path, name: &str) -> Vec<u8> {
    let out = scratch.join(format!("{name}.stdout"));
    let err = scratch.join(format!("{name}.stderr"));
    let mut child = Process(
        cmd.stdout(fs::File::create(&out).unwrap())
            .stderr(fs::File::create(err).unwrap())
            .spawn()
            .expect("harness spawn failed"),
    );
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(
                status.success(),
                "harness failed; inspect private scratch locally"
            );
            break;
        }
        assert!(Instant::now() < deadline, "harness timed out");
        std::thread::sleep(Duration::from_millis(50));
    }
    fs::read(out).unwrap()
}
fn counters(v: &Value) -> Value {
    let aliases = [
        ("input_tokens", "input_tokens"),
        ("cached_input_tokens", "cache_read_input_tokens"),
        ("cache_write_input_tokens", "cache_creation_input_tokens"),
        ("output_tokens", "output_tokens"),
        ("reasoning_output_tokens", "reasoning_tokens"),
        ("total_tokens", "total_tokens"),
    ];
    Value::Object(
        aliases
            .into_iter()
            .filter_map(|(key, alias)| {
                v.get(key)
                    .or_else(|| v.get(alias))
                    .and_then(Value::as_u64)
                    .map(|n| (key.into(), json!(n)))
            })
            .collect(),
    )
}
fn own_stdout(bytes: &[u8]) -> Vec<Value> {
    let values: Vec<Value> = serde_json::from_slice(bytes)
        .map(|v| vec![v])
        .unwrap_or_else(|_| {
            bytes
                .split(|b| *b == b'\n')
                .filter_map(|line| serde_json::from_slice(line).ok())
                .collect()
        });
    values
        .into_iter()
        .filter_map(|v| {
            let source = v.get("usage").or_else(|| v.pointer("/payload/thread_token_usage")).unwrap_or(&Value::Null);
            let usage = counters(source);
            let unknown: Vec<_> = source.as_object().into_iter().flat_map(|o| o.keys()).filter(|k| !COUNTERS.contains(&k.as_str()) && !["cache_read_input_tokens", "cache_creation_input_tokens", "reasoning_tokens"].contains(&k.as_str())).cloned().collect();
            let model = v.get("model").and_then(Value::as_str).or_else(|| {
                // A multi-model result has no single authoritative model.
                v.get("modelUsage").and_then(Value::as_object)
                    .filter(|models| models.len() == 1)
                    .and_then(|models| models.keys().next().map(String::as_str))
            }).filter(|s| s.len() <= 128 && s.chars().all(|c| c.is_ascii_alphanumeric() || "-._/:".contains(c)) && !s.contains(MARKER));
            (!usage.as_object().unwrap().is_empty())
                .then(|| json!({"counters":usage,"unmapped_keys":unknown,"model":model}))
        })
        .collect()
}
// Only adapter usage outputs are inspected, never configuration or credentials.
fn own_files(kind: &str, home: &Path) -> Vec<Value> {
    let mut paths = Vec::new();
    if kind == "codex" {
        fn sessions(dir: &Path, paths: &mut Vec<PathBuf>) {
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            for entry in entries {
                let entry = entry.unwrap();
                let ty = entry.file_type().unwrap();
                if ty.is_dir() {
                    sessions(&entry.path(), paths);
                } else if ty.is_file() && entry.path().extension().is_some_and(|e| e == "jsonl") {
                    paths.push(entry.path());
                }
            }
        }
        sessions(&home.join(".codex/sessions"), &mut paths);
    } else if kind == "grok" {
        // Exact fresh-session output paths supplied by steward; no home walk.
        if let Ok(list) = std::env::var("HERDR_LIVE_USAGE_FILES") {
            for name in list.lines() {
                let path = Path::new(name);
                assert!(
                    path.components()
                        .all(|c| matches!(c, std::path::Component::Normal(_)))
                );
                assert!(
                    name.starts_with(".grok/sessions/")
                        && matches!(
                            path.file_name().and_then(|s| s.to_str()),
                            Some("signals.json" | "summary.json")
                        )
                );
                assert!(
                    !fs::symlink_metadata(home.join(path))
                        .unwrap()
                        .file_type()
                        .is_symlink()
                );
                paths.push(home.join(path));
            }
        }
    }
    paths
        .into_iter()
        .filter_map(|path| {
            let bytes = fs::read(path).unwrap();
            if kind != "codex" {
                return own_stdout(&bytes).into_iter().last();
            }
            let mut last = None;
            let mut model = None;
            for line in bytes.split(|b| *b == b'\n') {
                let Ok(v) = serde_json::from_slice::<Value>(line) else {
                    continue;
                };
                if v["type"] == "turn_context" {
                    model = v
                        .pointer("/payload/model")
                        .and_then(Value::as_str)
                        .filter(|s| {
                            s.len() <= 128
                                && s.chars()
                                    .all(|c| c.is_ascii_alphanumeric() || "-._/:".contains(c))
                                && !s.contains(MARKER)
                        })
                        .map(str::to_owned);
                }
                if let Some(usage) = v.pointer("/payload/thread_token_usage") {
                    last = Some(counters(usage));
                }
            }
            last.map(|usage| json!({"counters":usage,"model":model}))
        })
        .collect()
}
fn live(kind: &str) {
    assert_eq!(
        std::env::var("HERDR_LIVE").as_deref(),
        Ok("1"),
        "live tests require HERDR_LIVE=1"
    );
    let home = PathBuf::from(
        std::env::var_os("HERDR_LIVE_HOME").expect("disposable HERDR_LIVE_HOME required"),
    );
    let bin = PathBuf::from(
        std::env::var_os("HERDR_LIVE_BIN").expect("absolute HERDR_LIVE_BIN required"),
    );
    let output =
        PathBuf::from(std::env::var_os("HERDR_LIVE_OUT").expect("HERDR_LIVE_OUT required"));
    assert!(home.is_absolute() && bin.is_absolute() && output.is_absolute());
    assert!(
        !output.starts_with(&home),
        "report must be outside the execution home"
    );
    assert!(
        !fs::symlink_metadata(&home)
            .unwrap()
            .file_type()
            .is_symlink(),
        "execution home must not be a symlink"
    );
    if let Some(owner_home) = std::env::var_os("HOME") {
        let owner_home = PathBuf::from(owner_home);
        assert_ne!(home, owner_home, "a disposable execution home is required");
        for dir in [
            ".claude",
            ".grok",
            ".config/muse",
            ".codex",
            ".gemini",
            ".local/share/opencode",
            ".cursor",
            ".copilot",
            ".ssh",
            ".herdr-projects",
        ] {
            assert!(
                !home.starts_with(owner_home.join(dir)),
                "owner data directories are forbidden"
            );
        }
    }

    // No traversal or inspection of the prepared home, including its login.
    let args = std::env::var("HERDR_LIVE_ARGS").expect("HERDR_LIVE_ARGS required");
    assert!(
        args.contains("{prompt}"),
        "args must contain the prompt placeholder"
    );
    let mut f = Fixture::reserved();
    let version_bytes = run(
        {
            let mut c = command(&bin, &home, f.tmp.path());
            c.arg("--version");
            c
        },
        f.tmp.path(),
        "version",
    );
    let version = String::from_utf8(version_bytes).unwrap();
    let version = version
        .split_whitespace()
        .map(|s| s.trim_start_matches('v'))
        .find(|s| {
            s.starts_with(|c: char| c.is_ascii_digit())
                && s.contains('.')
                && s.len() <= 64
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || ".-+".contains(c))
        })
        .expect("version not reported")
        .to_owned();
    let mut profile = codex_profile(&f.config, kind, "live", Some(&home));
    profile.agent.version = version.clone();
    plant_profile(&f.project.join(".state/state.db"), profile);
    f.readmit("live");
    let db = rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap();
    (f.attempt, f.decided) = db.query_row("SELECT a.id,d.decided_unix_ms FROM attempts a JOIN dispatch_decisions d ON d.attempt_id=a.id WHERE a.state='reserved'", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    db.execute("INSERT INTO collector_bindings(attempt_id,revision,state,collector,execution_home,unix_ms,source) VALUES(?1,1,'active',?2,?3,?4,'apply_launch_started')", rusqlite::params![f.attempt,kind,home.display().to_string(),unix_ms()]).unwrap();
    f.home = home;
    let cwd = PathBuf::from(f.worktree());
    fs::create_dir_all(&cwd).unwrap();
    // Real CLIs may require a Git workspace; never consult owner Git config.
    run(
        {
            let mut c = command(Path::new("/usr/bin/git"), &f.tmp.path().join("home"), &cwd);
            c.args(["init", "--quiet"]);
            c
        },
        f.tmp.path(),
        "git-init",
    );

    let mut receiver = None;
    let mut endpoint = None;
    let mut attempt_token = None;
    if matches!(kind, "grok" | "muse") {
        let minted = herdr_projects::telemetry::otlp::mint_attempt_token(&f.project, &f.attempt, 600).unwrap();
        attempt_token = Some(minted["token"].as_str().unwrap().to_owned());
        let mut c = command(Path::new(BIN), &f.tmp.path().join("home"), f.tmp.path());
        c.args(["--root", f.root.to_str().unwrap(), "telemetry", "demo", "otlp", "serve", "--port", "0", "--seconds", "600"]);
        let mut p = Process(c.stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap());
        let ready = ready(&mut p);
        endpoint = Some(format!("http://{}", ready["address"].as_str().unwrap()));
        receiver = Some(p);
    }
    let mut own = Vec::new();
    let resume = std::env::var("HERDR_LIVE_RESUME_ARGS").ok();
    if let Some(args) = &resume {
        assert!(
            args.contains("{prompt}"),
            "resume args must contain the prompt placeholder"
        );
    }
    for (n, template) in std::iter::once(args).chain(resume).enumerate() {
        let prompt = format!("Reply with the single word OK and do not use any tools. {MARKER}");
        let mut c = command(&bin, &f.home, &cwd);
        c.args(
            words(&template)
                .into_iter()
                .map(|s| s.replace("{prompt}", &prompt)),
        );
        if kind == "grok" && !template.contains("--output-format") {
            c.args(["--output-format", "json"]);
        }
        if let Some(endpoint) = &endpoint {
            c.env(
                "OTEL_EXPORTER_OTLP_HEADERS",
                format!("Authorization=Bearer {}", attempt_token.as_ref().unwrap()),
            )
            .env("GROK_EXTERNAL_OTEL", "1")
            .env("OTEL_METRICS_EXPORTER", "otlp")
            .env("OTEL_LOGS_EXPORTER", "otlp")
            .env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/protobuf")
            .env("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint)
            .env(
                "OTEL_RESOURCE_ATTRIBUTES",
                format!("herdr.attempt_id={}", f.attempt),
            )
            .env("OTEL_LOG_USER_PROMPTS", "false")
            .env("OTEL_LOG_ASSISTANT_RESPONSES", "false")
            .env("OTEL_LOG_TOOL_DETAILS", "false")
            .env("OTEL_LOG_TOOL_CONTENT", "false");
        }
        own.extend(own_stdout(&run(c, f.tmp.path(), &format!("turn-{n}"))));
    }
    drop(receiver);
    let file_usage = if kind == "grok" { Vec::new() } else { own_files(kind, &f.home) };
    if !file_usage.is_empty() {
        own = file_usage;
    }
    if matches!(kind, "claude" | "grok") {
        for record in &mut own {
            record["reported_counters"] = record["counters"].clone();
            let c = &mut record["counters"];
            if let Some(input) = c["input_tokens"].as_u64() {
                let input = input
                    + c["cached_input_tokens"].as_u64().unwrap_or(0)
                    + c["cache_write_input_tokens"].as_u64().unwrap_or(0);
                c["input_tokens"] = json!(input);
                if let Some(out) = c["output_tokens"].as_u64() {
                    c["total_tokens"] = json!(input + out);
                }
            }
        }
    }
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    let usage = f.cli_args(&["usage", "--json"]).0;
    let attempts = f.cli_args(&["attempts", "--json"]).0;
    let entries = f.cli_args(&["accounting", "entries"]).0;
    let counted = support::telemetry::accepted_delta_entries(&entries);
    let mut records: Vec<Value> = counted.iter().map(|e| counters(&e["native"])).collect();
    let mut totals = BTreeMap::<String, u64>::new();
    for entry in counted {
        let record = counters(&entry["native"]);
        for key in COUNTERS {
            if let Some(n) = record[key].as_u64() {
                *totals.entry(key.into()).or_default() += n;
            }
        }
    }
    let otlp = f.cli_args(&["otlp", "records"]).0;
    if kind == "grok" {
        totals.clear();
        records = otlp.as_array().into_iter().flatten()
            .filter(|r| r["kind"] == "usage" && r["usage_authority"] == "api_request"
                && r["attempt_id"] == f.attempt && r["binding"] == "exact")
            .map(|r| counters(&r["attributes"])).collect();
        for record in &records {
            for key in COUNTERS {
                if let Some(n) = record[key].as_u64() {
                    *totals.entry(key.into()).or_default() += n;
                }
            }
        }
    }
    let mut metric_totals = BTreeMap::<String, u64>::new();
    if kind == "grok" {
        for record in otlp.as_array().into_iter().flatten().filter(|r|
            r["reason"] == "usage_reconciliation_only" && r["attempt_id"] == f.attempt
                && r["binding"] == "exact" && r["native_name"] == "grok_code.token.usage") {
            // Only DELTA exports can be summed across the two turns.
            if record["aggregationTemporality"] != 1 { continue; }
            let key = match record["attributes"]["type"].as_str() {
                Some("input") => "input_tokens",
                Some("cache_read") => "cached_input_tokens",
                Some("cache_creation") => "cache_write_input_tokens",
                Some("output") => "output_tokens",
                Some("reasoning") => "reasoning_output_tokens",
                _ => continue,
            };
            if let Some(n) = record["value"].as_u64() {
                *metric_totals.entry(key.into()).or_default() += n;
            }
        }
    }
    let metric_differences: BTreeMap<_, _> = metric_totals.iter().map(|(key, n)| (
        key.clone(), i128::from(*n) - i128::from(*totals.get(key).unwrap_or(&0))
    )).collect();
    let mut reported = BTreeMap::<String, u64>::new();
    for record in &own {
        for key in COUNTERS {
            if let Some(n) = record["counters"][key].as_u64() {
                *reported.entry(key.into()).or_default() += n;
            }
        }
    }
    let differences: BTreeMap<_, _> = COUNTERS
        .into_iter()
        .map(|k| {
            let difference = reported
                .get(k)
                .map(|n| json!(i128::from(*totals.get(k).unwrap_or(&0)) - i128::from(*n)))
                .unwrap_or_else(|| json!("not_reported"));
            (k.to_owned(), difference)
        })
        .collect();
    let mut hits = 0;
    for name in ["telemetry.db", "telemetry.db-wal", "telemetry.db-shm"] {
        if let Ok(bytes) = fs::read(f.project.join(".state").join(name)) {
            hits += bytes
                .windows(MARKER.len())
                .filter(|w| *w == MARKER.as_bytes())
                .count();
        }
    }
    let bindings = f.cli_args(&["collectors", "bindings"]).0;
    // Only numeric counters and declared diagnostic keys leave scratch.
    let mut keys = Vec::new();
    fn diagnostic_keys(v: &Value, keys: &mut Vec<String>) {
        match v {
            Value::Object(o) => {
                for (k, v) in o {
                    if k == "unmapped_attribute_keys" || k == "unmapped_resource_keys" {
                        for key in v.as_array().into_iter().flatten().filter_map(Value::as_str) {
                            keys.push(key.to_owned());
                        }
                    } else {
                        diagnostic_keys(v, keys);
                    }
                }
            }
            Value::Array(a) => {
                for v in a {
                    diagnostic_keys(v, keys);
                }
            }
            _ => {}
        }
    }
    diagnostic_keys(&otlp, &mut keys);
    for record in &own {
        keys.extend(
            record["unmapped_keys"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned),
        );
    }
    keys.sort();
    keys.dedup();
    let quarantined = entries["entries"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|e| {
            e["provenance"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|p| p["disposition"] == "conflict")
        })
        || bindings["sources"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|s| s["binding"] == "quarantined");
    let otlp_bound = otlp
        .as_array()
        .into_iter()
        .flatten()
        .any(|r| r["attempt_id"] == f.attempt && r["binding"] == "exact");
    let otlp_counts: Vec<_> = otlp.as_array().into_iter().flatten().filter(|r| r["kind"] == "usage").map(|r| json!({"value":r["value"].as_f64(),"counter":r["attributes"]["type"].as_str(),"counters":counters(&r["attributes"]),"temporality":r["temporality"].as_str()})).collect();
    let source_bound = bindings["sources"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|s| s["attempt_id"] == f.attempt && s["binding"] == "bound");
    let bound = source_bound
        || otlp_bound
        || usage["attempts"].as_array().into_iter().flatten().any(|a| {
            a["attempt_id"] == f.attempt && a["usage"]["records"].as_u64().unwrap_or(0) > 0
        });
    let report = json!({"harness":kind,"version":version,"turns":if std::env::var_os("HERDR_LIVE_RESUME_ARGS").is_some(){2}else{1},
        "usage_records":records,"otlp_usage_records":otlp_counts,"ledger_totals":if kind == "grok" {json!({})} else {json!(totals)},"usage_totals":totals,"harness_usage":if own.is_empty(){json!("not_reported")}else{json!(own)},
        "differences":differences,"metric_reconciliation":{"totals":metric_totals,"differences":metric_differences},"unmapped_keys":keys,"binding_outcome":if quarantined{"quarantined"}else if bound{"bound"}else{"unbound"},
        "privacy":{"marker":MARKER,"sidecar_hits":hits},"attempt_query_observed":attempts["attempts"].as_array().is_some(),
        "binding_query_observed":!bindings.is_null(),"certification_changed":false,"evidence_state":if bound && !own.is_empty() && (kind != "grok" || !records.is_empty()) && metric_differences.values().all(|v| *v == 0) && differences.values().all(|v| v == &json!(0)) && hits == 0 {"review_required"} else {"incomplete"}});
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(output)
        .unwrap();
    file.write_all(serde_json::to_string_pretty(&report).unwrap().as_bytes())
        .unwrap();
    assert_eq!(hits, 0, "privacy marker reached the sidecar");
    for delta in differences.values().filter(|v| v.is_number()) {
        assert_eq!(
            delta,
            &json!(0),
            "harness and ledger differ; report is not a certificate"
        );
    }
}
#[test]
#[ignore = "steward-only paid live run"]
fn claude_live() {
    live("claude");
}
#[test]
#[ignore = "steward-only paid live run"]
fn grok_live() {
    live("grok");
}
#[test]
#[ignore = "steward-only paid live run"]
fn muse_live() {
    live("muse");
}
#[test]
#[ignore = "steward-only paid live run"]
fn codex_live() {
    live("codex");
}

#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)]
mod support;
use herdr_projects::telemetry::otlp;
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    os::unix::fs::PermissionsExt,
    process::{Child, Command, Stdio},
};
use support::telemetry::*;

fn payload(f: &Fixture, name: &str) -> Vec<u8> {
    fs::read_to_string(format!(
        "{}/tests/fixtures/telemetry/otlp/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
    .replace("@ATTEMPT@", &f.attempt)
    .into_bytes()
}
#[test]
fn documented_harness_fixtures_map_exact_rows_and_replay() {
    let f = Fixture::reserved();
    for (name, path, count) in [
        ("claude-logs", "/v1/logs", 2),
        ("gemini-logs", "/v1/logs", 2),
        ("claude-metrics", "/v1/metrics", 5),
        ("gemini-metrics", "/v1/metrics", 2),
    ] {
        let bytes = payload(&f, name);
        assert_eq!(otlp::ingest(&f.project, path, &bytes).unwrap(), count);
        assert_eq!(otlp::ingest(&f.project, path, &bytes).unwrap(), 0);
    }
    // Synthetic Cursor trace canary: traces are unsupported, including the
    // installed service/version. Rejection must leave other adapters intact.
    let traces = json!({"resourceSpans":[{"resource":{"attributes":[
        {"key":"service.name","value":{"stringValue":"cursor-agent-cli"}},
        {"key":"service.version","value":{"stringValue":"2026.09.28-64d2043"}}
    ]},"scopeSpans":[{"spans":[{"name":"McpSdkClient.callTool","attributes":[
        {"key":"tool.output","value":{"stringValue":"OTLP_SECRET_CONTENT"}}
    ]}]}]}]});
    assert!(otlp::ingest(&f.project, "/v1/traces", &serde_json::to_vec(&traces).unwrap()).is_err());
    let rows = otlp::records(&f.project).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 11);
    for r in rows.as_array().unwrap() {
        assert_eq!(r["attempt_id"], f.attempt);
        assert_eq!(r["binding"], "exact");
        assert_eq!(r["source_trust"], "collector_observed");
        assert_eq!(r["certified"], "fixture");
    }
    let find = |name: &str| {
        rows.as_array()
            .unwrap()
            .iter()
            .find(|r| r["native_name"] == name)
            .unwrap()
    };
    assert_eq!(
        find("claude_code.api_request")["attributes"],
        json!({"model":"claude-sonnet","input_tokens":11,"output_tokens":7,"cache_read_tokens":3,"cache_creation_tokens":2,"duration_ms":123,"cost_usd":0.04})
    );
    assert_eq!(
        find("claude_code.tool_result")["attributes"],
        json!({"tool_name":"Read","success":true,"duration_ms":8})
    );
    assert_eq!(
        find("gemini_cli.api_response")["attributes"],
        json!({"model":"gemini-pro","input_token_count":19,"output_token_count":5,"cached_content_token_count":4,"thoughts_token_count":2,"tool_token_count":1,"total_token_count":31,"duration_ms":90})
    );
    assert_eq!(
        find("gemini_cli.tool_call")["attributes"],
        json!({"function_name":"read_file","success":false,"duration_ms":12})
    );
    let mut cm: Vec<_> = rows
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["native_name"] == "claude_code.token.usage")
        .map(|r| {
            (
                r["attributes"]["type"].as_str().unwrap(),
                r["value"].as_i64().unwrap(),
            )
        })
        .collect();
    cm.sort();
    assert_eq!(
        cm,
        vec![
            ("cacheCreation", 2),
            ("cacheRead", 3),
            ("input", 11),
            ("output", 7)
        ]
    );
    assert_eq!(find("claude_code.cost.usage")["value"], 0.04);
    let mut gm: Vec<_> = rows
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["native_name"] == "gemini_cli.token.usage")
        .map(|r| {
            (
                r["attributes"]["type"].as_str().unwrap(),
                r["value"].as_i64().unwrap(),
            )
        })
        .collect();
    gm.sort();
    assert_eq!(gm, vec![("input", 19), ("output", 5)]);
    let (cap, _) = f.cli_args(&["collectors", "capabilities", "--json"]);
    for harness in ["claude-code", "gemini-cli"] {
        let adapter = cap["adapters"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["adapter"] == format!("otlp:{harness}"))
            .unwrap();
        assert!(
            adapter["fields"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|v| v["available"] == true)
                .all(|v| v["certified"] == "fixture")
        );
    }
    let codex = cap["adapters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["adapter"] == "otlp:codex")
        .unwrap();
    assert_eq!(codex["fields"][0]["certified"], "none");
    assert!(!rows.to_string().contains("OTLP_SECRET_CONTENT"));
    for suffix in ["", "-wal", "-shm"] {
        if let Ok(bytes) = fs::read(f.project.join(format!(".state/telemetry.db{suffix}"))) {
            assert!(!bytes.windows(19).any(|w| w == b"OTLP_SECRET_CONTENT"));
        }
    }
    let (cli, _) = f.cli_args(&["otlp", "records"]);
    assert_eq!(cli, rows);
}
#[test]
fn malformed_requests_are_atomic_and_create_no_sidecar() {
    let f = Fixture::reserved();
    for bytes in [b"{".as_slice(), b"{}", b"[]", b"{\"resourceLogs\":[{}]}"] {
        assert!(otlp::ingest(&f.project, "/v1/logs", bytes).is_err());
    }
    let mut v: Value = serde_json::from_slice(&payload(&f, "claude-logs")).unwrap();
    v["resourceLogs"][0]["scopeLogs"][0]["logRecords"][1]["attributes"][1]["value"] =
        json!({"stringValue":"invalid boolean"});
    assert!(otlp::ingest(&f.project, "/v1/logs", &serde_json::to_vec(&v).unwrap()).is_err());
    let mut timestamp: Value = serde_json::from_slice(&payload(&f, "claude-logs")).unwrap();
    timestamp["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0]["timeUnixNano"] =
        json!("0".repeat(21));
    assert!(
        otlp::ingest(
            &f.project,
            "/v1/logs",
            &serde_json::to_vec(&timestamp).unwrap()
        )
        .is_err()
    );
    let mut fractional: Value = serde_json::from_slice(&payload(&f, "claude-metrics")).unwrap();
    fractional["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["sum"]["dataPoints"][0]["asInt"] =
        json!("1.5");
    assert!(
        otlp::ingest(
            &f.project,
            "/v1/metrics",
            &serde_json::to_vec(&fractional).unwrap()
        )
        .is_err()
    );
    assert!(!f.project.join(".state/telemetry.db").exists());
    assert!(otlp::ingest(&f.project, "/v1/logs", &vec![b' '; otlp::MAX_BODY + 1]).is_err());
}
#[test]
fn unbound_unknown_and_uncertified_sources_never_guess() {
    let f = Fixture::reserved();
    let mut v: Value = serde_json::from_slice(&payload(&f, "gemini-logs")).unwrap();
    v["resourceLogs"][0]["resource"]["attributes"][1]["value"] =
        json!({"stringValue":"absent-attempt"});
    otlp::ingest(&f.project, "/v1/logs", &serde_json::to_vec(&v).unwrap()).unwrap();
    v["resourceLogs"][0]["resource"]["attributes"]
        .as_array_mut()
        .unwrap()
        .remove(1);
    otlp::ingest(&f.project, "/v1/logs", &serde_json::to_vec(&v).unwrap()).unwrap();
    for service in ["unknown-secret-harness", "codex"] {
        v["resourceLogs"][0]["resource"]["attributes"][0]["value"] = json!({"stringValue":service});
        otlp::ingest(&f.project, "/v1/logs", &serde_json::to_vec(&v).unwrap()).unwrap();
    }
    let rows = otlp::records(&f.project).unwrap();
    assert!(
        rows.as_array()
            .unwrap()
            .iter()
            .all(|r| r["attempt_id"].is_null())
    );
    assert_eq!(
        rows.as_array()
            .unwrap()
            .iter()
            .filter(|r| r["binding"] == "unknown_attempt")
            .count(),
        2
    );
    let unmapped: Vec<_> = rows
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["kind"] == "unmapped")
        .collect();
    assert_eq!(unmapped.len(), 4);
    assert!(
        unmapped
            .iter()
            .all(|r| r.get("native_name").is_none() && r.get("attributes").is_none())
    );
    assert!(!rows.to_string().contains("unknown-secret-harness"));
    assert!(unmapped.iter().all(|r| {
        r["unmapped_attribute_keys"]
            .as_array()
            .unwrap()
            .contains(&json!("function_args"))
    }));
    let mut changed = payload(&f, "claude-logs");
    otlp::ingest(&f.project, "/v1/logs", &changed).unwrap();
    changed = String::from_utf8(changed)
        .unwrap()
        .replace("OTLP_SECRET_CONTENT", "DIFFERENT_SECRET_CONTENT")
        .into_bytes();
    assert_eq!(otlp::ingest(&f.project, "/v1/logs", &changed).unwrap(), 0);
}

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn server(f: &Fixture) -> (Server, String, String) {
    let mut child = Command::new(BIN)
        .env_clear()
        .env("HOME", f.tmp.path().join("home"))
        .env("PATH", "/usr/bin:/bin")
        .args([
            "--root",
            f.root.to_str().unwrap(),
            "telemetry",
            "demo",
            "otlp",
            "serve",
            "--port",
            "0",
            "--seconds",
            "30",
            "--max-requests",
            "50",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    if line.is_empty() {
        let output = child.wait_with_output().unwrap();
        panic!(
            "OTLP TCP loopback listener unavailable: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let ready: Value = serde_json::from_str(&line).unwrap();
    let path = ready["token_file"].as_str().unwrap();
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    (
        Server(child),
        ready["address"].as_str().unwrap().to_owned(),
        fs::read_to_string(path).unwrap(),
    )
}
fn http(
    address: &str,
    token: Option<&str>,
    path: &str,
    ty: &str,
    body: &[u8],
    length: usize,
) -> u16 {
    let mut s = TcpStream::connect(address).unwrap();
    s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    write!(s,"POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: {ty}\r\nContent-Length: {length}\r\n").unwrap();
    if let Some(t) = token {
        write!(s, "Authorization: Bearer {t}\r\n").unwrap();
    }
    s.write_all(b"\r\n").unwrap();
    s.write_all(body).unwrap();
    let mut reply = String::new();
    let _ = s.read_to_string(&mut reply);
    reply.split_whitespace().nth(1).unwrap().parse().unwrap()
}
#[test]
fn http_auth_limits_malformed_and_replay() {
    let f = Fixture::reserved();
    let (_server, address, token) = server(&f);
    assert_eq!(
        http(&address, None, "/v1/logs", "application/json", b"", 0),
        401
    );
    assert_eq!(
        http(
            &address,
            Some("wrong"),
            "/v1/logs",
            "application/json",
            b"",
            0
        ),
        401
    );
    assert_eq!(
        http(
            &address,
            Some(&token),
            "/v1/logs",
            "application/json",
            b"",
            otlp::MAX_BODY + 1
        ),
        413
    );
    assert_eq!(
        http(
            &address,
            Some(&token),
            "/v1/logs",
            "application/x-protobuf",
            b"\x0b",
            1
        ),
        400
    );
    assert_eq!(
        http(
            &address,
            Some(&token),
            "/v1/logs",
            "application/json",
            b"{",
            1
        ),
        400
    );
    assert!(!f.project.join(".state/telemetry.db").exists());
    for (name, path) in [
        ("claude-logs", "/v1/logs"),
        ("gemini-metrics", "/v1/metrics"),
    ] {
        let bytes = payload(&f, name);
        for _ in 0..2 {
            assert_eq!(
                http(
                    &address,
                    Some(&token),
                    path,
                    "application/json",
                    &bytes,
                    bytes.len()
                ),
                200
            );
        }
    }
    assert_eq!(
        otlp::records(&f.project).unwrap().as_array().unwrap().len(),
        4
    );
}
#[test]
fn http_request_rate_is_bounded() {
    let f = Fixture::reserved();
    let (_server, address, token) = server(&f);
    let replies: Vec<_> = (0..40)
        .map(|_| {
            http(
                &address,
                Some(&token),
                "/v1/logs",
                "application/json",
                b"{}",
                2,
            )
        })
        .collect();
    assert!(replies.contains(&429));
    assert!(!f.project.join(".state/telemetry.db").exists());
}

#[test]
fn sidecar_migration_retention_and_backup_preserve_otlp_evidence() {
    let f = Fixture::reserved();
    let minted = otlp::mint_attempt_token(&f.project, &f.attempt, 3600).unwrap();
    otlp::revoke_attempt_token(&f.project, minted["token_hash"].as_str().unwrap()).unwrap();
    otlp::ingest(&f.project, "/v1/logs", &payload(&f, "claude-logs")).unwrap();
    otlp::ingest(&f.project, "/v1/metrics", &payload(&f, "grok-metrics")).unwrap();
    let before = otlp::records(&f.project).unwrap();
    assert_eq!(
        f.sidecar()
            .query_row(
                "SELECT version FROM telemetry_streams WHERE stream='otlp'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        3
    );
    let (classes, _) = f.cli_args(&["maintenance", "classes", "--json"]);
    let class = classes["classes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["class"] == "sidecar.otlp")
        .unwrap();
    assert_eq!(class["action"], "retain");
    assert_eq!(class["basis"], "source_of_truth");
    let token_class = classes["classes"].as_array().unwrap().iter()
        .find(|c| c["class"] == "sidecar.otlp_attempt_tokens").unwrap();
    assert_eq!(token_class["action"], "retain");
    assert_eq!(token_class["basis"], "source_of_truth");
    assert!(class["retention_days"].is_null());
    let backup = f.tmp.path().join("otlp-backup");
    f.cli_args(&["backup", "create", "--out", backup.to_str().unwrap()]);
    let copy = rusqlite::Connection::open_with_flags(
        backup.join("telemetry.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    assert_eq!(
        copy.query_row("SELECT count(*) FROM otlp_records", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        12
    );
    assert_eq!(copy.query_row("SELECT count(*) FROM otlp_attempt_tokens WHERE revoked_unix_ms IS NOT NULL", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
    let backup_bytes = fs::read(backup.join("telemetry.db")).unwrap();
    let secret = minted["token"].as_str().unwrap();
    assert!(!backup_bytes.windows(secret.len()).any(|w| w == secret.as_bytes()));
    drop(copy);
    f.cli_args(&["backup", "verify", "--from", backup.to_str().unwrap()]);
    f.cli_args(&["backup", "restore", "--from", backup.to_str().unwrap()]);
    assert_eq!(otlp::records(&f.project).unwrap(), before);
    assert!(otlp::ingest_attempt(&f.project, "/v1/logs", &payload(&f, "claude-logs"), "application/json", secret).is_err());
    assert!(fs::read_dir(&backup).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".token")
    }));
}

#[test]
fn unsupported_names_categories_and_missing_service_are_unmapped() {
    let f = Fixture::reserved();
    let mut logs: Value = serde_json::from_slice(&payload(&f, "gemini-logs")).unwrap();
    logs["resourceLogs"][0]["scopeLogs"][0]["logRecords"]
        .as_array_mut()
        .unwrap()
        .truncate(1);
    logs["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0]["eventName"] =
        json!("gemini_cli.api.response");
    assert_eq!(
        otlp::ingest(&f.project, "/v1/logs", &serde_json::to_vec(&logs).unwrap()).unwrap(),
        1
    );
    let mut metrics: Value = serde_json::from_slice(&payload(&f, "claude-metrics")).unwrap();
    metrics["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
        .as_array_mut()
        .unwrap()
        .truncate(1);
    metrics["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["sum"]["dataPoints"][0]["attributes"]
        [1]["value"] = json!({"stringValue":"unknown-token-category-secret"});
    assert_eq!(
        otlp::ingest(
            &f.project,
            "/v1/metrics",
            &serde_json::to_vec(&metrics).unwrap()
        )
        .unwrap(),
        1
    );
    logs["resourceLogs"][0]["resource"]["attributes"]
        .as_array_mut()
        .unwrap()
        .remove(0);
    assert_eq!(
        otlp::ingest(&f.project, "/v1/logs", &serde_json::to_vec(&logs).unwrap()).unwrap(),
        1
    );
    let rows = otlp::records(&f.project).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 3);
    assert!(
        rows.as_array()
            .unwrap()
            .iter()
            .all(|r| r["kind"] == "unmapped"
                && r.get("value").is_none()
                && r.get("attributes").is_none())
    );
    assert!(
        rows.as_array()
            .unwrap()
            .iter()
            .any(|r| r["binding"] == "unbound" && r["attempt_id"].is_null())
    );
    assert!(!rows.to_string().contains("unknown-token-category-secret"));
}

#[test]
fn grok_1046_metrics_are_versioned_bound_and_content_free() {
    let f = Fixture::reserved();
    let bytes = payload(&f, "grok-metrics");
    assert_eq!(otlp::ingest(&f.project, "/v1/metrics", &bytes).unwrap(), 10);
    assert_eq!(otlp::ingest(&f.project, "/v1/metrics", &bytes).unwrap(), 0);
    let (rows, _) = f.cli_args(&["otlp", "records"]);
    let rows = rows.as_array().unwrap();
    for row in rows {
        assert_eq!(row["adapter"], "otlp:grok");
        assert_eq!(row["attempt_id"], f.attempt);
        assert_eq!(row["binding"], "exact");
        assert_eq!(row["cli_version"], "1.0.46");
        assert_eq!(row["aggregationTemporality"], 1);
        assert_eq!(row["unmapped_attribute_keys"].as_array().unwrap().len(), 10);
    }
    let mut tokens: Vec<_> = rows.iter().filter(|r| r["native_name"] == "grok_code.token.usage")
        .map(|r| (r["attributes"]["type"].as_str().unwrap(), r["value"].as_i64().unwrap())).collect();
    tokens.sort();
    assert_eq!(tokens, vec![("cache_creation", 3), ("cache_read", 7), ("input", 31), ("output", 11), ("reasoning", 5)]);
    for (name, value, unit) in [("cost.usage", json!(0.0127), "USD"), ("session.count", json!(1), "{session}"),
        ("turn.count", json!(2), "{turn}"), ("tool.usage", json!(4), "{call}"), ("error.count", json!(2), "{error}")] {
        let r = rows.iter().find(|r| r["native_name"] == format!("grok_code.{name}")).unwrap();
        assert_eq!(r["value"], value);
        assert_eq!(r["unit"], unit);
    }
    let changed = String::from_utf8(bytes.clone()).unwrap().replace("GROK_SECRET_CONTENT", "GROK_OTHER_SECRET");
    assert_eq!(otlp::ingest(&f.project, "/v1/metrics", changed.as_bytes()).unwrap(), 0);
    let mut v: Value = serde_json::from_slice(&bytes).unwrap();
    v["resourceMetrics"][0]["resource"]["attributes"].as_array_mut().unwrap().retain(|a| a["key"] != "herdr.attempt_id");
    otlp::ingest(&f.project, "/v1/metrics", &serde_json::to_vec(&v).unwrap()).unwrap();
    v["resourceMetrics"][0]["resource"]["attributes"][1]["value"]["stringValue"] = json!("1.0.47");
    otlp::ingest(&f.project, "/v1/metrics", &serde_json::to_vec(&v).unwrap()).unwrap();
    let all = otlp::records(&f.project).unwrap();
    assert_eq!(all.as_array().unwrap().iter().filter(|r| r["binding"] == "unbound").count(), 20);
    assert_eq!(all.as_array().unwrap().iter().filter(|r| r["reason"] == "cli_version_uncertified" && r["kind"] == "unmapped").count(), 10);
    assert!(!all.to_string().contains("GROK_SECRET_CONTENT"));
    let mut unknown: Value = serde_json::from_slice(&bytes).unwrap();
    unknown["resourceMetrics"][0]["scopeMetrics"][0]["metrics"].as_array_mut().unwrap().truncate(1);
    unknown["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["sum"]["dataPoints"][0]["attributes"][0]["value"]["stringValue"] = json!("GROK_SECRET_UNKNOWN_TYPE");
    unknown["resourceMetrics"][0]["resource"]["attributes"][2]["value"]["stringValue"] = json!("GROK_SECRET_UNKNOWN_ATTEMPT");
    assert_eq!(otlp::ingest(&f.project, "/v1/metrics", &serde_json::to_vec(&unknown).unwrap()).unwrap(), 1);
    let diagnostics = otlp::records(&f.project).unwrap();
    let diagnostic = diagnostics.as_array().unwrap().iter().find(|r| r["binding"] == "unknown_attempt").unwrap();
    assert_eq!(diagnostic["kind"], "unmapped");
    assert!(diagnostic["attempt_id"].is_null());
    assert!(diagnostic["value"].is_null());
    assert!(!diagnostics.to_string().contains("GROK_SECRET_UNKNOWN"));
    for suffix in ["", "-wal", "-shm"] {
        if let Ok(bytes) = fs::read(f.project.join(format!(".state/telemetry.db{suffix}"))) {
            assert!(!bytes.windows(19).any(|w| w == b"GROK_SECRET_CONTENT"));
        }
    }
    let (cap, _) = f.cli_args(&["collectors", "capabilities", "--json"]);
    let grok = cap["adapters"].as_array().unwrap().iter().find(|a| a["adapter"] == "otlp:grok").unwrap();
    assert_eq!(grok["fixture_versions"], json!(["1.0.46"]));
    // Live only for the api_request usage counters and model the 1.0.46 live run observed.
    assert_eq!(grok["certified_versions"], json!(["1.0.46"]));
    assert_eq!(grok["native_source"]["certified"], "none");
    for f in grok["fields"].as_array().unwrap().iter().filter(|f| f["available"] == true) {
        let live = f["kind"] == "grok_code.api_request" && ["model", "input_tokens", "output_tokens", "reasoning_tokens",
            "cache_read_tokens", "cache_creation_tokens"].contains(&f["field"].as_str().unwrap());
        assert_eq!(f["certified"], if live { "live" } else { "fixture" }, "{f}");
    }
}

#[test]
fn muse_installed_contract_is_version_gated_and_content_free() {
    let f = Fixture::reserved();
    let logs = payload(&f, "muse-logs");
    let metrics = payload(&f, "muse-metrics");
    assert_eq!(otlp::ingest(&f.project, "/v1/logs", &logs).unwrap(), 1);
    assert_eq!(otlp::ingest(&f.project, "/v1/metrics", &metrics).unwrap(), 4);
    assert_eq!(otlp::ingest(&f.project, "/v1/logs", &logs).unwrap(), 0);
    let rows = otlp::records(&f.project).unwrap();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 5);
    assert!(rows.iter().all(|r| r["adapter"] == "otlp:muse" && r["attempt_id"] == f.attempt && r["binding"] == "exact" && r["cli_version"] == "1.4.0-R4161.1" && r["certified"] == "fixture"));
    let call = rows.iter().find(|r| r["native_name"] == "model_call").unwrap();
    assert_eq!(call["attributes"], json!({"gen_ai.request.model":"synthetic-model","gen_ai.provider.name":"meta","gen_ai.usage.input_tokens":17,"gen_ai.usage.output_tokens":5,"tokens.cached":3,"duration_ms":90}));
    let mut counts: Vec<_> = rows.iter().filter(|r| r["native_name"] == "tbh.approval_review.token_usage").map(|r| (r["attributes"]["token_type"].as_str().unwrap(), r["value"].as_i64().unwrap())).collect();
    counts.sort();
    assert_eq!(counts, vec![("cached_input",4),("input",11),("output",2),("total",13)]);
    let mut missing: Value = serde_json::from_slice(&logs).unwrap();
    missing["resourceLogs"][0]["resource"]["attributes"].as_array_mut().unwrap().retain(|a| a["key"] != "herdr.attempt_id");
    assert_eq!(otlp::ingest(&f.project, "/v1/logs", &serde_json::to_vec(&missing).unwrap()).unwrap(), 1);
    let (capabilities, _) = f.cli_args(&["collectors", "capabilities", "--json"]);
    let cap = capabilities["adapters"].as_array().unwrap().iter().find(|a| a["adapter"] == "otlp:muse").unwrap();
    assert_eq!(cap["accepted_versions"], json!(["1.4.0-R4161.1"]));
    assert_eq!(cap["certified_versions"], json!([]));
    assert!(cap["fields"].as_array().unwrap().iter().filter(|f| f["available"] == true).all(|f| f["certified"] == "fixture"));
    let unbound = String::from_utf8(logs.clone()).unwrap().replace(&f.attempt, "not-an-attempt");
    assert_eq!(otlp::ingest(&f.project, "/v1/logs", unbound.as_bytes()).unwrap(), 1);
    let unknown = String::from_utf8(logs).unwrap().replace("1.4.0-R4161.1", "future");
    assert_eq!(otlp::ingest(&f.project, "/v1/logs", unknown.as_bytes()).unwrap(), 1);
    let rows = otlp::records(&f.project).unwrap();
    assert!(rows.as_array().unwrap().iter().any(|r| r["binding"] == "unbound" && r["attempt_id"].is_null()));
    assert!(rows.as_array().unwrap().iter().any(|r| r["binding"] == "unknown_attempt" && r["attempt_id"].is_null()));
    assert!(rows.as_array().unwrap().iter().any(|r| r["kind"] == "unmapped" && r["reason"] == "cli_version_uncertified"));
    assert!(!rows.to_string().contains("MUSE_SECRET_CONTENT"));
    for entry in fs::read_dir(f.project.join(".state")).unwrap().flatten() {
        if entry.file_type().unwrap().is_file() {
            assert!(!fs::read(entry.path()).unwrap().windows(b"MUSE_SECRET_CONTENT".len()).any(|w| w == b"MUSE_SECRET_CONTENT"));
        }
    }
}

#[test]
fn devin_3000_11_3_contract_is_version_gated_bound_and_content_free() {
    let f = Fixture::reserved();
    let logs = payload(&f, "devin-logs");
    let metrics = payload(&f, "devin-metrics");
    assert_eq!(otlp::ingest(&f.project, "/v1/logs", &logs).unwrap(), 3);
    assert_eq!(otlp::ingest(&f.project, "/v1/metrics", &metrics).unwrap(), 6);
    assert_eq!(otlp::ingest(&f.project, "/v1/logs", &logs).unwrap(), 0);
    assert_eq!(otlp::ingest(&f.project, "/v1/metrics", &metrics).unwrap(), 0);
    let (rows, _) = f.cli_args(&["otlp", "records"]);
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 9);
    assert!(rows.iter().all(|r| r["adapter"] == "otlp:devin" && r["attempt_id"] == f.attempt && r["binding"] == "exact"
        && r["cli_version"] == "3000.11.3" && r["service_build"] == "3000.11.3 (9c803229faa4)" && r["certified"] == "fixture"));
    // Fixture evidence never enters accounting: no request authority is declared.
    assert!(rows.iter().all(|r| r.get("usage_authority").is_none()));
    let request = rows.iter().find(|r| r["native_name"] == "api_request").unwrap();
    assert_eq!(request["kind"], "usage");
    assert_eq!(request["attributes"], json!({"model":"synthetic-model","request_id":"req-synthetic-1","input_tokens":100,"output_tokens":20,
        "cache_read_tokens":30,"cache_creation_tokens":5,"duration_ms":250}));
    let tool = rows.iter().find(|r| r["native_name"] == "tool_result").unwrap();
    assert_eq!((tool["kind"].clone(), tool["attributes"].clone()), (json!("tool"), json!({"tool_name":"exec","success":true})));
    let mut tokens: Vec<_> = rows.iter().filter(|r| r["native_name"] == "devin.token.usage")
        .map(|r| (r["attributes"]["type"].as_str().unwrap(), r["value"].as_i64().unwrap(), r["unit"].as_str().unwrap(), r["aggregationTemporality"].as_i64().unwrap())).collect();
    tokens.sort();
    assert_eq!(tokens, vec![("cacheCreation", 5, "tokens", 1), ("cacheRead", 30, "tokens", 1), ("input", 100, "tokens", 1), ("output", 20, "tokens", 1)]);
    // Prompts, session/user identity and unreviewed instruments keep keys only.
    let prompt = rows.iter().find(|r| r["kind"] == "unmapped" && r["unmapped_attribute_keys"].to_string().contains("prompt_length")).unwrap();
    assert!(prompt.get("attributes").is_none() && prompt.get("native_name").is_none());
    // Identical key-less diagnostics (session count, active time) collapse by digest.
    assert_eq!(rows.iter().filter(|r| r["kind"] == "unmapped").count(), 3);
    // Prefixed event names (unproven exporter spelling) map identically; other versions are diagnostics.
    let prefixed = String::from_utf8(logs.clone()).unwrap().replace("\"api_request\"", "\"devin.api_request\"");
    assert_eq!(otlp::ingest(&f.project, "/v1/logs", prefixed.as_bytes()).unwrap(), 0);
    let future = String::from_utf8(logs.clone()).unwrap().replace("3000.11.3 (9c803229faa4)", "3000.12.0 (ffffffffffff)");
    assert_eq!(otlp::ingest(&f.project, "/v1/logs", future.as_bytes()).unwrap(), 3);
    let unbound = String::from_utf8(logs.clone()).unwrap().replace(&f.attempt, "not-an-attempt");
    assert_eq!(otlp::ingest(&f.project, "/v1/logs", unbound.as_bytes()).unwrap(), 3);
    let mut missing: Value = serde_json::from_slice(&metrics).unwrap();
    missing["resourceMetrics"][0]["resource"]["attributes"].as_array_mut().unwrap().retain(|a| a["key"] != "herdr.attempt_id");
    assert_eq!(otlp::ingest(&f.project, "/v1/metrics", &serde_json::to_vec(&missing).unwrap()).unwrap(), 6);
    let all = otlp::records(&f.project).unwrap();
    let all = all.as_array().unwrap();
    assert!(all.iter().filter(|r| r["reason"] == "cli_version_uncertified").all(|r| r["kind"] == "unmapped" && r["cli_version"].is_null() && r.get("service_build").is_none()));
    assert_eq!(all.iter().filter(|r| r["reason"] == "cli_version_uncertified").count(), 3);
    assert_eq!(all.iter().filter(|r| r["binding"] == "unknown_attempt" && r["attempt_id"].is_null()).count(), 3);
    assert_eq!(all.iter().filter(|r| r["binding"] == "unbound" && r["attempt_id"].is_null()).count(), 6);
    privacy_scan(&f, &["DEVIN_SECRET_CONTENT", "DEVIN_SECRET_USER", "DEVIN_SECRET_SESSION", "DEVIN_SECRET_PROMPT", "DEVIN_SECRET_TOOLUSE"]);
    let (capabilities, _) = f.cli_args(&["collectors", "capabilities", "--json"]);
    let adapters = capabilities["adapters"].as_array().unwrap();
    let cap = adapters.iter().find(|a| a["adapter"] == "otlp:devin").unwrap();
    assert_eq!((cap["fixture_versions"].clone(), cap["accepted_versions"].clone(), cap["certified_versions"].clone()), (json!(["3000.11.3"]), json!(["3000.11.3"]), json!([])));
    assert_eq!(cap["native_source"], json!({"certified":"none","reason":"local_usage_schema_not_established"}));
    assert!(cap["fields"].as_array().unwrap().iter().filter(|f| f["available"] == true).all(|f| f["certified"] == "fixture"));
    assert!(adapters.iter().all(|a| a["adapter"] == "devin" || a["adapter"].as_str().unwrap().starts_with("otlp:") || !a["adapter"].to_string().contains("devin")));
}

#[test]
fn devin_protobuf_export_matches_json_rows() {
    let f = Fixture::reserved();
    for (fixture, metrics, expected) in [("devin-logs", false, 3), ("devin-metrics", true, 6)] {
        let root: Value = serde_json::from_slice(&payload(&f, fixture)).unwrap();
        let endpoint = if metrics { "/v1/metrics" } else { "/v1/logs" };
        assert_eq!(otlp::ingest_protobuf(&f.project, endpoint, &pb_request(&root, metrics)).unwrap(), expected);
        let before = otlp::records(&f.project).unwrap();
        assert_eq!(otlp::ingest(&f.project, endpoint, &serde_json::to_vec(&root).unwrap()).unwrap(), 0);
        assert_eq!(otlp::records(&f.project).unwrap(), before);
    }
    privacy_scan(&f, &["DEVIN_SECRET_CONTENT", "DEVIN_SECRET_USER"]);
}

// Independent fixture encoder: OTLP field numbers from the public wire contract.
fn pb_varint(mut n: u64) -> Vec<u8> {
    let mut out = Vec::new();
    while n >= 128 {
        out.push((n as u8 & 127) | 128);
        n >>= 7;
    }
    out.push(n as u8);
    out
}
fn pb_field(n: u64, wire: u64, bytes: &[u8]) -> Vec<u8> {
    let mut out = pb_varint(n * 8 + wire);
    if wire == 2 {
        out.extend(pb_varint(bytes.len() as u64));
    }
    out.extend(bytes);
    out
}
fn pb_any(value: &Value) -> Vec<u8> {
    if let Some(v) = value.get("stringValue") {
        pb_field(1, 2, v.as_str().unwrap().as_bytes())
    } else if let Some(v) = value.get("boolValue") {
        pb_field(2, 0, &pb_varint(u64::from(v.as_bool().unwrap())))
    } else if let Some(v) = value.get("intValue") {
        pb_field(
            3,
            0,
            &pb_varint(
                v.as_i64()
                    .unwrap_or_else(|| v.as_str().unwrap().parse().unwrap()) as u64,
            ),
        )
    } else if let Some(v) = value.get("doubleValue") {
        pb_field(4, 1, &v.as_f64().unwrap().to_le_bytes())
    } else if let Some(v) = value.get("arrayValue") {
        pb_field(
            5,
            2,
            &v["values"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|a| pb_field(1, 2, &pb_any(a)))
                .collect::<Vec<_>>(),
        )
    } else if let Some(v) = value.get("kvlistValue") {
        pb_field(6, 2, &pb_attrs(1, &v["values"]))
    } else {
        pb_field(7, 2, b"unsupported secret bytes")
    }
}
fn pb_attrs(n: u64, attrs: &Value) -> Vec<u8> {
    attrs
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|a| {
            let mut kv = pb_field(1, 2, a["key"].as_str().unwrap().as_bytes());
            kv.extend(pb_field(2, 2, &pb_any(&a["value"])));
            pb_field(n, 2, &kv)
        })
        .collect()
}
fn pb_point(point: &Value, histogram: bool) -> Vec<u8> {
    let mut out = pb_attrs(if histogram { 9 } else { 7 }, &point["attributes"]);
    for (key, n) in [
        ("startTimeUnixNano", 2),
        ("timeUnixNano", 3),
        ("asInt", 6),
        ("count", 4),
    ] {
        if let Some(v) = point.get(key) {
            let nvalue = v
                .as_u64()
                .unwrap_or_else(|| v.as_str().unwrap().parse().unwrap());
            out.extend(pb_field(n, 1, &nvalue.to_le_bytes()));
        }
    }
    for (key, n) in [("asDouble", 4), ("sum", 5)] {
        if let Some(v) = point.get(key) {
            out.extend(pb_field(n, 1, &v.as_f64().unwrap().to_le_bytes()));
        }
    }
    out
}
fn pb_request(root: &Value, metrics: bool) -> Vec<u8> {
    root[if metrics {
        "resourceMetrics"
    } else {
        "resourceLogs"
    }]
    .as_array()
    .unwrap()
    .iter()
    .flat_map(|r| {
        let mut resource = pb_field(1, 2, &pb_attrs(1, &r["resource"]["attributes"]));
        for scope in r[if metrics { "scopeMetrics" } else { "scopeLogs" }]
            .as_array()
            .unwrap()
        {
            let mut s = Vec::new();
            for entry in scope[if metrics { "metrics" } else { "logRecords" }]
                .as_array()
                .unwrap()
            {
                let mut e = Vec::new();
                if metrics {
                    e.extend(pb_field(1, 2, entry["name"].as_str().unwrap().as_bytes()));
                    e.extend(pb_field(
                        3,
                        2,
                        entry["unit"].as_str().unwrap_or("").as_bytes(),
                    ));
                    for (key, n) in [("sum", 7), ("gauge", 5), ("histogram", 9)] {
                        if let Some(data) = entry.get(key) {
                            let mut d: Vec<_> = data["dataPoints"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .flat_map(|p| pb_field(1, 2, &pb_point(p, key == "histogram")))
                                .collect();
                            if let Some(t) = data.get("aggregationTemporality") {
                                d.extend(pb_field(2, 0, &pb_varint(t.as_u64().unwrap())));
                            }
                            e.extend(pb_field(n, 2, &d));
                        }
                    }
                    for (key, n) in [("exponentialHistogram", 10), ("summary", 11)] {
                        if entry.get(key).is_some() {
                            // A point containing a private attribute; decoder must leave it opaque.
                            let point = pb_attrs(7, &json!([{"key":"private_point_key","value":{"stringValue":"DG4H_UNSUPPORTED_SECRET"}}]));
                            e.extend(pb_field(n, 2, &pb_field(1, 2, &point)));
                        }
                    }
                } else {
                    if let Some(t) = entry.get("timeUnixNano") {
                        e.extend(pb_field(
                            1,
                            1,
                            &t.as_str().unwrap().parse::<u64>().unwrap().to_le_bytes(),
                        ));
                    }
                    if let Some(t) = entry.get("severityNumber") {
                        e.extend(pb_field(2, 0, &pb_varint(t.as_u64().unwrap())));
                    }
                    if let Some(name) = entry.get("eventName") {
                        e.extend(pb_field(12, 2, name.as_str().unwrap().as_bytes()));
                    }
                    if let Some(body) = entry.get("body") {
                        e.extend(pb_field(5, 2, &pb_any(body)));
                    }
                    e.extend(pb_attrs(6, &entry["attributes"]));
                }
                s.extend(pb_field(2, 2, &e));
            }
            resource.extend(pb_field(2, 2, &s));
        }
        pb_field(1, 2, &resource)
    })
    .collect()
}
fn privacy_scan(f: &Fixture, markers: &[&str]) {
    let rows = otlp::records(&f.project).unwrap().to_string();
    for marker in markers {
        assert!(!rows.contains(marker));
        for suffix in ["", "-wal", "-shm"] {
            if let Ok(bytes) = fs::read(f.project.join(format!(".state/telemetry.db{suffix}"))) {
                assert!(
                    !bytes.windows(marker.len()).any(|w| w == marker.as_bytes()),
                    "privacy leak {suffix}"
                );
            }
        }
    }
}
#[test]
fn mixed_unsupported_metric_types_preserve_usage_and_match_json() {
    let f = Fixture::reserved();
    let mut root: Value = serde_json::from_slice(&payload(&f, "grok-metrics")).unwrap();
    let entries = root["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
        .as_array_mut()
        .unwrap();
    entries.retain(|entry| entry["name"] == "grok_code.token.usage");
    let usage_count = entries.len();
    for (name, key) in [
        ("grok_code.turn.ttft", "exponentialHistogram"),
        ("startup.duration", "summary"),
    ] {
        entries.push(json!({"name":name, key:{"dataPoints":[{"attributes":[{"key":"private_point_key","value":{"stringValue":"DG4H_UNSUPPORTED_SECRET"}}]}]}}));
    }
    assert_eq!(
        otlp::ingest_protobuf(&f.project, "/v1/metrics", &pb_request(&root, true)).unwrap(),
        usage_count + 2
    );
    let rows = otlp::records(&f.project).unwrap();
    assert_eq!(
        rows.as_array()
            .unwrap()
            .iter()
            .filter(|r| r["native_name"] == "grok_code.token.usage" && r["reason"] == "usage_reconciliation_only")
            .count(),
        usage_count
    );
    for name in ["grok_code.turn.ttft", "startup.duration"] {
        let row = rows
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["native_name"] == name)
            .unwrap();
        assert_eq!(row["kind"], "unmapped");
        assert_eq!(row["reason"], "unsupported_metric_type");
        assert_eq!(row["unmapped_attribute_keys"], json!([]));
        assert!(row.get("value").is_none());
    }
    assert_eq!(
        otlp::ingest(
            &f.project,
            "/v1/metrics",
            &serde_json::to_vec(&root).unwrap()
        )
        .unwrap(),
        0
    );
    assert_eq!(otlp::records(&f.project).unwrap(), rows);
    privacy_scan(&f, &["DG4H_UNSUPPORTED_SECRET", "private_point_key"]);
    // Absent or conflicting data fields reject atomically in either transport.
    for invalid in [
        json!({"name":"missing"}),
        json!({"name":"conflict","summary":{},"exponentialHistogram":{}}),
    ] {
        root["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
            .as_array_mut()
            .unwrap()
            .push(invalid);
        assert!(
            otlp::ingest(
                &f.project,
                "/v1/metrics",
                &serde_json::to_vec(&root).unwrap()
            )
            .is_err()
        );
        assert!(
            otlp::ingest_protobuf(&f.project, "/v1/metrics", &pb_request(&root, true)).is_err()
        );
        assert_eq!(otlp::records(&f.project).unwrap(), rows);
        root["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
            .as_array_mut()
            .unwrap()
            .pop();
    }
}

#[test]
fn protobuf_grok_muse_match_json_including_gauge_histogram_and_privacy() {
    let f = Fixture::reserved();
    for (fixture, metrics, expected) in [
        ("grok-metrics", true, 10),
        ("muse-logs", false, 1),
        ("muse-metrics", true, 4),
    ] {
        let root: Value = serde_json::from_slice(&payload(&f, fixture)).unwrap();
        let endpoint = if metrics { "/v1/metrics" } else { "/v1/logs" };
        assert_eq!(
            otlp::ingest_protobuf(&f.project, endpoint, &pb_request(&root, metrics)).unwrap(),
            expected
        );
        let before = otlp::records(&f.project).unwrap();
        assert_eq!(
            otlp::ingest(&f.project, endpoint, &serde_json::to_vec(&root).unwrap()).unwrap(),
            0
        );
        assert_eq!(otlp::records(&f.project).unwrap(), before);
    }
    let mut root: Value = serde_json::from_slice(&payload(&f, "grok-metrics")).unwrap();
    let entries = root["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
        .as_array_mut()
        .unwrap();
    entries.truncate(1);
    let mut data = entries[0].as_object_mut().unwrap().remove("sum").unwrap();
    data.as_object_mut()
        .unwrap()
        .remove("aggregationTemporality");
    data["dataPoints"].as_array_mut().unwrap().truncate(1);
    data["dataPoints"][0]["asDouble"] = json!(31.5);
    data["dataPoints"][0]
        .as_object_mut()
        .unwrap()
        .remove("asInt");
    for any in [
        json!({"boolValue":true}),
        json!({"intValue":"-1"}),
        json!({"doubleValue":1.25}),
        json!({"arrayValue":{"values":[{"stringValue":"DG4H_PLANTED_SECRET"}]}}),
        json!({"kvlistValue":{"values":[{"key":"nested","value":{"stringValue":"DG4H_PLANTED_SECRET"}}]}}),
        json!({"bytesValue":"AAAA"}),
    ] {
        let attrs = data["dataPoints"][0]["attributes"].as_array_mut().unwrap();
        attrs.push(json!({"key":format!("unknown{}",attrs.len()),"value":any}));
    }
    entries[0]["gauge"] = data.clone();
    assert_eq!(
        otlp::ingest_protobuf(&f.project, "/v1/metrics", &pb_request(&root, true)).unwrap(),
        1
    );
    assert_eq!(
        otlp::ingest(
            &f.project,
            "/v1/metrics",
            &serde_json::to_vec(&root).unwrap()
        )
        .unwrap(),
        0
    );
    let entry = &mut root["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0];
    entry.as_object_mut().unwrap().remove("gauge");
    data["aggregationTemporality"] = json!(2);
    data["dataPoints"][0]
        .as_object_mut()
        .unwrap()
        .remove("asDouble");
    data["dataPoints"][0]["count"] = json!("3");
    data["dataPoints"][0]["sum"] = json!(42.5);
    entry["histogram"] = data;
    assert_eq!(
        otlp::ingest_protobuf(&f.project, "/v1/metrics", &pb_request(&root, true)).unwrap(),
        1
    );
    assert_eq!(
        otlp::ingest(
            &f.project,
            "/v1/metrics",
            &serde_json::to_vec(&root).unwrap()
        )
        .unwrap(),
        0
    );
    let rows = otlp::records(&f.project).unwrap();
    assert!(
        rows.as_array()
            .unwrap()
            .iter()
            .any(|r| r["count"] == 3 && r["sum"] == 42.5)
    );
    assert!(
        rows.as_array()
            .unwrap()
            .iter()
            .any(|r| r["value"] == 31.5 && r.get("aggregationTemporality").is_none())
    );
    privacy_scan(
        &f,
        &[
            "GROK_SECRET_CONTENT",
            "MUSE_SECRET_CONTENT",
            "DG4H_PLANTED_SECRET",
            "unsupported secret bytes",
        ],
    );
}
#[test]
fn attempt_tokens_cli_bind_quarantine_revoke_expire_and_never_store_secrets() {
    let f = Fixture::reserved();
    assert!(f.cli_fail(&["otlp", "mint-token", "--attempt", "absent"]).contains("unknown attempt"));
    assert!(otlp::mint_attempt_token(&f.project, &f.attempt, 0).is_err());
    assert!(otlp::mint_attempt_token(&f.project, &f.attempt, 86401).is_err());
    assert!(!f.project.join(".state/telemetry.db").exists());
    let (minted, _) = f.cli_args(&[
        "otlp",
        "mint-token",
        "--attempt",
        &f.attempt,
        "--seconds",
        "60",
    ]);
    let token = minted["token"].as_str().unwrap();
    let mut root: Value = serde_json::from_slice(&payload(&f, "muse-logs")).unwrap();
    root["resourceLogs"][0]["resource"]["attributes"]
        .as_array_mut()
        .unwrap()
        .retain(|a| a["key"] != "herdr.attempt_id");
    root["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0]["severityNumber"] = json!(9);
    let bytes = pb_request(&root, false);
    assert_eq!(
        otlp::ingest_attempt(
            &f.project,
            "/v1/logs",
            &bytes,
            "application/x-protobuf",
            token
        )
        .unwrap(),
        1
    );
    let rows = otlp::records(&f.project).unwrap();
    assert_eq!(rows[0]["attempt_id"], f.attempt);
    assert_eq!(rows[0]["binding"], "exact");
    assert_eq!(rows[0]["severityNumber"], 9);
    assert_eq!(
        otlp::ingest_attempt(
            &f.project,
            "/v1/logs",
            &serde_json::to_vec(&root).unwrap(),
            "application/json",
            token
        )
        .unwrap(),
        0
    );
    // A second real canonical attempt exists in this same project.
    f.readmit("other");
    let second_attempt = herdr_projects::store::SqliteStore::open(&f.project.join(".state/state.db"))
        .unwrap().read_snapshot(None).unwrap().attempts.into_iter()
        .find(|a| a.id.as_str() != f.attempt).unwrap().id.as_str().to_owned();
    root["resourceLogs"][0]["resource"]["attributes"]
        .as_array_mut()
        .unwrap()
        .push(json!({"key":"herdr.attempt_id","value":{"stringValue":second_attempt}}));
    assert_eq!(
        otlp::ingest_attempt(
            &f.project,
            "/v1/logs",
            &pb_request(&root, false),
            "application/x-protobuf",
            token
        )
        .unwrap(),
        1
    );
    let rows = otlp::records(&f.project).unwrap();
    let quarantined = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["reason"] == "cross_attempt_quarantined")
        .unwrap();
    assert!(quarantined["attempt_id"].is_null());
    assert_eq!(quarantined["binding"], "unknown_attempt");
    let other = Fixture::reserved();
    assert!(
        otlp::ingest_attempt(
            &other.project,
            "/v1/logs",
            &bytes,
            "application/x-protobuf",
            token
        )
        .is_err()
    );
    f.cli_args(&[
        "otlp",
        "revoke-token",
        "--token-hash",
        minted["token_hash"].as_str().unwrap(),
    ]);
    assert!(
        otlp::ingest_attempt(
            &f.project,
            "/v1/logs",
            &bytes,
            "application/x-protobuf",
            token
        )
        .is_err()
    );
    let expiring = otlp::mint_attempt_token(&f.project, &f.attempt, 1).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert!(
        otlp::ingest_attempt(
            &f.project,
            "/v1/logs",
            &bytes,
            "application/x-protobuf",
            expiring["token"].as_str().unwrap()
        )
        .is_err()
    );
    assert_eq!(otlp::records(&f.project).unwrap(), rows);
    privacy_scan(
        &f,
        &[
            token,
            expiring["token"].as_str().unwrap(),
            "MUSE_SECRET_CONTENT",
            &second_attempt,
        ],
    );
}
#[test]
fn protobuf_malformed_truncated_oversized_and_limits_are_atomic() {
    let f = Fixture::reserved();
    let root: Value = serde_json::from_slice(&payload(&f, "grok-metrics")).unwrap();
    let good = pb_request(&root, true);
    let mut extended = good.clone();
    extended.extend(pb_field(100, 0, &pb_varint(42)));
    extended.extend(pb_field(101, 1, &42u64.to_le_bytes()));
    extended.extend(pb_field(102, 2, b"DG4H_UNKNOWN_FIELD_SECRET"));
    extended.extend(pb_field(103, 5, &42u32.to_le_bytes()));
    for bad in [
        vec![0x0b],
        vec![0x0f],
        vec![0],
        vec![0x80; 11],
        vec![0x0a, 0xff, 0xff],
        good[..good.len() - 1].to_vec(),
        vec![0; otlp::MAX_BODY + 1],
    ] {
        assert!(otlp::ingest_protobuf(&f.project, "/v1/metrics", &bad).is_err());
    }
    let mut many = root.clone();
    let point =
        many["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["sum"]["dataPoints"][0].clone();
    many["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["sum"]["dataPoints"] =
        json!(vec![point.clone(); 4097]);
    assert!(otlp::ingest_protobuf(&f.project, "/v1/metrics", &pb_request(&many, true)).is_err());
    many = root.clone();
    let attr = json!({"key":"too_many","value":{"stringValue":"secret"}});
    many["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["sum"]["dataPoints"][0]["attributes"] =
        json!(vec![attr; 129]);
    assert!(otlp::ingest_protobuf(&f.project, "/v1/metrics", &pb_request(&many, true)).is_err());
    let mut nested = pb_field(1, 2, b"secret");
    for _ in 0..20 {
        nested = pb_field(5, 2, &pb_field(1, 2, &nested));
    }
    let mut kv = pb_field(1, 2, b"unknown");
    kv.extend(pb_field(2, 2, &nested));
    let nested_request = pb_field(1, 2, &pb_field(1, 2, &pb_field(1, 2, &kv)));
    assert!(otlp::ingest_protobuf(&f.project, "/v1/metrics", &nested_request).is_err());
    assert!(!f.project.join(".state/telemetry.db").exists());
    assert_eq!(
        otlp::ingest_protobuf(&f.project, "/v1/metrics", &good).unwrap(),
        10
    );
    let before = otlp::records(&f.project).unwrap();
    assert_eq!(
        otlp::ingest_protobuf(&f.project, "/v1/metrics", &extended).unwrap(),
        0
    );
    privacy_scan(&f, &["DG4H_UNKNOWN_FIELD_SECRET"]);
    let mut bad = good;
    bad.extend([0x0a, 0x03, 0x12, 0x05, 0x01]);
    assert!(otlp::ingest_protobuf(&f.project, "/v1/metrics", &bad).is_err());
    assert_eq!(otlp::records(&f.project).unwrap(), before);
}

#[test]
fn http_protobuf_attempt_token_binding_auth_and_project_token_unchanged() {
    let f = Fixture::reserved();
    let minted = otlp::mint_attempt_token(&f.project, &f.attempt, 60).unwrap();
    let attempt_token = minted["token"].as_str().unwrap();
    let (_server, address, project_token) = server(&f);
    let mut root: Value = serde_json::from_slice(&payload(&f, "grok-metrics")).unwrap();
    root["resourceMetrics"][0]["resource"]["attributes"]
        .as_array_mut()
        .unwrap()
        .retain(|a| a["key"] != "herdr.attempt_id");
    let bytes = pb_request(&root, true);
    assert_eq!(
        http(
            &address,
            Some(attempt_token),
            "/v1/metrics",
            "application/x-protobuf",
            &bytes,
            bytes.len()
        ),
        200
    );
    assert!(
        otlp::records(&f.project)
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["attempt_id"] == f.attempt)
    );
    root["resourceMetrics"][0]["resource"]["attributes"]
        .as_array_mut()
        .unwrap()
        .push(json!({"key":"herdr.attempt_id","value":{"stringValue":"other-attempt"}}));
    let bytes = pb_request(&root, true);
    assert_eq!(
        http(
            &address,
            Some(attempt_token),
            "/v1/metrics",
            "application/x-protobuf",
            &bytes,
            bytes.len()
        ),
        200
    );
    let rows = otlp::records(&f.project).unwrap();
    assert_eq!(
        rows.as_array()
            .unwrap()
            .iter()
            .filter(|r| r["reason"] == "cross_attempt_quarantined" && r["attempt_id"].is_null())
            .count(),
        10
    );
    for bad in [b"\x0b".as_slice(), &bytes[..bytes.len() - 1]] {
        assert_eq!(
            http(
                &address,
                Some(attempt_token),
                "/v1/metrics",
                "application/x-protobuf",
                bad,
                bad.len()
            ),
            400
        );
    }
    assert_eq!(
        http(
            &address,
            Some(attempt_token),
            "/v1/metrics",
            "application/x-protobuf",
            b"",
            otlp::MAX_BODY + 1
        ),
        413
    );
    assert_eq!(
        http(
            &address,
            Some(attempt_token),
            "/v1/metrics",
            "application/x-protobuf\r\nContent-Encoding: gzip",
            b"",
            0
        ),
        415
    );
    assert_eq!(
        http(
            &address,
            Some(attempt_token),
            "/v1/traces",
            "application/x-protobuf",
            b"",
            0
        ),
        404
    );
    otlp::revoke_attempt_token(&f.project, minted["token_hash"].as_str().unwrap()).unwrap();
    assert_eq!(
        http(
            &address,
            Some(attempt_token),
            "/v1/metrics",
            "application/x-protobuf",
            &bytes,
            bytes.len()
        ),
        401
    );
    let expired = otlp::mint_attempt_token(&f.project, &f.attempt, 1).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert_eq!(
        http(
            &address,
            Some(expired["token"].as_str().unwrap()),
            "/v1/metrics",
            "application/x-protobuf",
            &bytes,
            bytes.len()
        ),
        401
    );
    assert_eq!(otlp::records(&f.project).unwrap(), rows);
    // The original project credential still binds solely from the resource ID.
    let json = payload(&f, "grok-metrics");
    assert_eq!(
        http(
            &address,
            Some(&project_token),
            "/v1/metrics",
            "application/json",
            &json,
            json.len()
        ),
        200
    );
    assert_eq!(otlp::records(&f.project).unwrap(), rows);
    assert_eq!(
        http(
            &address,
            Some(&project_token),
            "/v1/metrics",
            "application/x-protobuf",
            &bytes,
            bytes.len()
        ),
        200
    );
    // The project credential binds solely from the resource attribute, which
    // names no known attempt: exactly this request's 10 rows are added, all
    // unbound. LC4's reconciliation-only reasons don't change binding.
    let after = otlp::records(&f.project).unwrap();
    let before = rows.as_array().unwrap();
    let added: Vec<&Value> = after.as_array().unwrap().iter().filter(|r| !before.contains(r)).collect();
    assert_eq!(added.len(), 10);
    assert!(added.iter().all(|r| r["binding"] == "unknown_attempt" && r["attempt_id"].is_null()));
    privacy_scan(&f, &[attempt_token, "GROK_SECRET_CONTENT"]);
}

// Real 1.0.46 capture structure, with synthetic identity values and timestamps.
// Histogram bucket contents were not captured and remain keys-only diagnostics.
#[test]
fn grok_live_shape_protobuf_two_turns_uses_api_calls_without_pii_or_double_count() {
    for metrics_first in [false, true] {
        let mut f = Fixture::reserved();
        let mut profile = codex_profile(&f.config, "grok", "grok", Some(&f.home));
        profile.agent.version = "1.0.46".into();
        plant_profile(&f.project.join(".state/state.db"), profile);
        f.readmit("grok");
        (f.attempt,f.decided) = rusqlite::Connection::open(f.project.join(".state/state.db")).unwrap()
            .query_row("SELECT a.id,d.decided_unix_ms FROM attempts a JOIN dispatch_decisions d ON d.attempt_id=a.id WHERE a.state='reserved'", [], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
        f.cli("collect");
        f.cli_args(&["accounting", "sync"]);
        let minted = otlp::mint_attempt_token(&f.project, &f.attempt, 600).unwrap();
        let token = minted["token"].as_str().unwrap();
        for turn in 1..=2 {
            for metrics in if metrics_first { [true, false] } else { [false, true] } {
                let signal = if metrics { "metrics" } else { "logs" };
                let root: Value = serde_json::from_slice(&fs::read(format!(
                    "{}/tests/fixtures/telemetry/grok-1.0.46/turn-{turn}-{signal}.json",
                    env!("CARGO_MANIFEST_DIR")
                )).unwrap()).unwrap();
                let endpoint = if metrics { "/v1/metrics" } else { "/v1/logs" };
                let bytes = pb_request(&root, metrics);
                assert!(otlp::ingest_attempt(&f.project, endpoint, &bytes, "application/x-protobuf", token).unwrap() > 0);
                assert_eq!(otlp::ingest_attempt(&f.project, endpoint, &bytes, "application/x-protobuf", token).unwrap(), 0);
                if !metrics {
                    // Replay the same source event with a changed exporter timestamp.
                    let mut replay = root.clone();
                    for log in replay["resourceLogs"][0]["scopeLogs"][0]["logRecords"].as_array_mut().unwrap() {
                        log["timeUnixNano"] = json!("9999999999");
                    }
                    otlp::ingest_attempt(&f.project, endpoint, &pb_request(&replay, false), "application/x-protobuf", token).unwrap();
                }
            }
            f.cli_args(&["accounting", "sync"]);
            assert_eq!(f.cli_args(&["accounting", "status"]).0["sync"]["mode"], "incremental");
        }
        let rows = f.cli_args(&["otlp", "records"]).0;
        let rows = rows.as_array().unwrap();
        assert!(rows.iter().all(|r| r["cli_version"] == "1.0.46" && r["binding"] == "exact" && r["attempt_id"] == f.attempt));
        assert!(rows.iter().all(|r| r["service_build"] == "1.0.46 (2765805b9442)"));
        let mut usage: Vec<_> = rows.iter().filter(|r| r["kind"] == "usage").collect();
        usage.sort_by_key(|r| r["attributes"]["turn_number"].as_u64().unwrap());
        assert_eq!(usage.len(), 2);
        for (row, input, cache, duration, cost) in [
            (usage[0], 15426, 1280, 6377, 9778),
            (usage[1], 15468, 5120, 1921, 7587),
        ] {
            let a = &row["attributes"];
            assert_eq!(row["usage_authority"], "api_request");
            assert_eq!(a["model"], "grok-4.5");
            assert_eq!(a["input_tokens"], input);
            assert_eq!(a["cached_input_tokens"], cache);
            assert_eq!(a["cache_write_input_tokens"], 0);
            assert_eq!(a["output_tokens"], 14);
            assert_eq!(a["reasoning_output_tokens"], 13);
            assert_eq!(a["total_tokens"], input + 14);
            assert_eq!(a["duration_ms"], duration);
            assert_eq!(a["cost_usd_micros"], cost);
            assert_eq!(a["stop_reason"], "stop");
        }
        for (field, total) in [("input_tokens", 30894), ("cached_input_tokens", 6400),
            ("output_tokens", 28), ("reasoning_output_tokens", 26), ("total_tokens", 30922), ("cost_usd_micros", 17365)] {
            assert_eq!(usage.iter().map(|r| r["attributes"][field].as_u64().unwrap()).sum::<u64>(), total);
        }
        let metrics: Vec<_> = rows.iter().filter(|r| r["reason"] == "usage_reconciliation_only").collect();
        assert_eq!(metrics.iter().filter(|r| r["native_name"] == "grok_code.token.usage").count(), 8);
        assert_eq!(metrics.iter().filter(|r| r["attributes"]["type"] == "input").map(|r| r["value"].as_u64().unwrap()).sum::<u64>(), 30894);
        assert!((metrics.iter().filter(|r| r["native_name"] == "grok_code.cost.usage").map(|r| r["value"].as_f64().unwrap()).sum::<f64>() - 0.01736584).abs() < 1e-10);
        for row in rows {
            for key in ["user.email", "user.id", "team.id", "client_identifier", "prompt_length", "response_length"] {
                assert!(row["attributes"].get(key).is_none());
            }
        }
        f.cli_args(&["accounting", "sync"]);
        let ledger = f.cli_args(&["accounting", "entries"]).0;
        let counted = accepted_delta_entries(&ledger);
        assert_eq!(counted.len(), 2);
        assert!(counted.iter().all(|e| e["source"] == "otlp:grok" && e["model"] == "grok-4.5"));
        assert_eq!(counted.iter().map(|e| e["normalized"]["input_tokens"].as_u64().unwrap()).sum::<u64>(), 30894);
        assert_eq!(counted.iter().map(|e| e["normalized"]["cache_read_tokens"].as_u64().unwrap()).sum::<u64>(), 6400);
        assert_eq!(counted.iter().map(|e| e["normalized"]["output_tokens"].as_u64().unwrap()).sum::<u64>(), 28);
        for command in ["usage", "attempts"] {
            let queried = f.cli_args(&[command, "--json"]).0;
            let attempt = queried["attempts"].as_array().unwrap().iter().find(|a| a["attempt_id"] == f.attempt).unwrap();
            assert_eq!(attempt["usage"]["total_tokens"], 30922, "{queried}");
            assert_eq!(attempt["usage"]["records"], 2);
        }
        assert_eq!(f.report()["metrics"]["M08"]["value"], 30894);
        assert_eq!(f.report()["metrics"]["M09"]["value"], 28);
        f.cli_args(&["accounting", "sync"]);
        assert_eq!(f.cli_args(&["accounting", "entries"]).0, ledger);
        f.sidecar().execute("DELETE FROM usage_ledger", []).unwrap();
        f.cli_args(&["accounting", "sync"]);
        assert_eq!(f.cli_args(&["accounting", "entries"]).0, ledger);
        f.cli_args(&["analytics", "refresh"]);
        let verify = f.cli_args(&["analytics", "rebuild", "--verify"]).0;
        assert_eq!(verify["identical"], true, "{verify}");
        let card = f.tmp.path().join("otlp-rates.json");
        fs::write(&card, serde_json::to_vec(&json!({"card_id":"synthetic-grok","version":1,"provider":"synthetic",
            "product":"otlp:grok","models":["grok-4.5"],"currency":"USD","rate_unit":1,
            "effective_from_unix_ms":0,"effective_to_unix_ms":null,
            "includes":{"discounts":false,"taxes":false,"fees":false},"source":"invented fixture rates",
            "rates":[{"category":"input","rate":"1"},{"category":"cache_read","rate":"1"},{"category":"output","rate":"1"}]})).unwrap()).unwrap();
        f.cli_args(&["accounting", "import-rate-card", card.to_str().unwrap()]);
        f.cli_args(&["accounting", "reprice"]);
        let cost = f.cli_args(&["accounting", "cost", "--json"]).0;
        assert_eq!(cost["attempts"][0]["estimate"]["amount"], "30922", "{cost}");
        let backup = f.tmp.path().join("grok-ledger-backup");
        f.cli_args(&["backup", "create", "--out", backup.to_str().unwrap()]);
        let copy = rusqlite::Connection::open_with_flags(backup.join("telemetry.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert_eq!(copy.query_row("SELECT sum(e.total_tokens) FROM usage_entries e WHERE EXISTS(SELECT 1 FROM usage_dispositions d WHERE d.entry_id=e.entry_id AND d.disposition='accepted')", [], |r| r.get::<_, i64>(0)).unwrap(), 30922);
        f.cancel_reserved();
        let future = (unix_ms() + 100 * 86400000).to_string();
        let plan = f.cli_args(&["maintenance", "plan", "--now", &future, "--json"]).0;
        let sessions = plan["classes"].as_array().unwrap().iter().find(|c| c["class"] == "sidecar.normalized_sessions").unwrap();
        assert_eq!(sessions["eligible_count"], 0, "{sessions}");
        privacy_scan(&f, &["lc4-planted-secret@example.invalid"]);
    }
}

#[test]
fn grok_build_version_fallback_and_request_identity_are_fail_closed() {
    let f = Fixture::reserved();
    let minted = otlp::mint_attempt_token(&f.project, &f.attempt, 600).unwrap();
    let token = minted["token"].as_str().unwrap();
    let original: Value = serde_json::from_slice(include_bytes!("fixtures/telemetry/grok-1.0.46/turn-1-logs.json")).unwrap();
    let mut body_only = original.clone();
    for log in body_only["resourceLogs"][0]["scopeLogs"][0]["logRecords"].as_array_mut().unwrap() {
        log["body"] = json!({"stringValue": log["eventName"].clone()});
        log.as_object_mut().unwrap().remove("eventName");
    }
    for (bytes, content_type) in [
        (serde_json::to_vec(&body_only).unwrap(), "application/json"),
        (pb_request(&body_only, false), "application/x-protobuf"),
    ] {
        // DG4a accepts JSON stringValue as a name only. DG4h discards
        // protobuf bodies, so only that transport must remain unmapped.
        let body_fixture = Fixture::reserved();
        let body_token = otlp::mint_attempt_token(&body_fixture.project, &body_fixture.attempt, 600).unwrap();
        otlp::ingest_attempt(&body_fixture.project, "/v1/logs", &bytes, content_type, body_token["token"].as_str().unwrap()).unwrap();
        let records = otlp::records(&body_fixture.project).unwrap();
        let rows = records.as_array().unwrap();
        if content_type == "application/json" {
            assert_eq!(rows.iter().filter(|r| r["kind"] == "usage").count(), 1);
            let usage = rows.iter().find(|r| r["kind"] == "usage").unwrap();
            assert_eq!(usage["attributes"]["input_tokens"], 15426);
            assert_eq!(usage["attributes"]["output_tokens"], 14);
        } else {
            assert!(rows.iter().all(|r| r["kind"] == "unmapped"));
        }
        assert!(rows.iter().all(|r| r.get("body").is_none()));
        privacy_scan(&body_fixture, &["lc4-planted-secret@example.invalid"]);
    }
    let mut root = original.clone();
    root["resourceLogs"][0]["resource"]["attributes"].as_array_mut().unwrap().retain(|a| a["key"] != "client.version");
    otlp::ingest_attempt(&f.project, "/v1/logs", &pb_request(&root, false), "application/x-protobuf", token).unwrap();
    assert_eq!(otlp::records(&f.project).unwrap().as_array().unwrap().iter().filter(|r| r["kind"] == "usage").count(), 1);
    for attr in root["resourceLogs"][0]["resource"]["attributes"].as_array_mut().unwrap() {
        if attr["key"] == "service.version" { attr["value"]["stringValue"] = json!("1.0.47 (2765805b9442)"); }
    }
    otlp::ingest_attempt(&f.project, "/v1/logs", &pb_request(&root, false), "application/x-protobuf", token).unwrap();
    root = original.clone();
    for attr in root["resourceLogs"][0]["resource"]["attributes"].as_array_mut().unwrap() {
        if attr["key"] == "client.version" { attr["value"]["stringValue"] = json!("1.0.47"); }
    }
    otlp::ingest_attempt(&f.project, "/v1/logs", &pb_request(&root, false), "application/x-protobuf", token).unwrap();
    root = original;
    for log in root["resourceLogs"][0]["scopeLogs"][0]["logRecords"].as_array_mut().unwrap() {
        log["attributes"].as_array_mut().unwrap().retain(|a| !["session.id", "event.sequence", "prompt.id"].contains(&a["key"].as_str().unwrap()));
    }
    otlp::ingest_attempt(&f.project, "/v1/logs", &pb_request(&root, false), "application/x-protobuf", token).unwrap();
    let records = otlp::records(&f.project).unwrap();
    let rows = records.as_array().unwrap();
    assert_eq!(rows.iter().filter(|r| r["kind"] == "usage").count(), 1);
    assert!(rows.iter().any(|r| r["reason"] == "cli_version_uncertified"));
    assert!(rows.iter().any(|r| r["reason"] == "missing_usage_identity" && r["kind"] == "unmapped"));
    privacy_scan(&f, &["lc4-planted-secret@example.invalid"]);
}

#[test]
fn grok_prompt_turn_fallback_retains_cache_creation_and_isolates_invalid_usage() {
    let f = Fixture::reserved();
    let minted = otlp::mint_attempt_token(&f.project, &f.attempt, 600).unwrap();
    let token = minted["token"].as_str().unwrap();
    let mut root: Value = serde_json::from_slice(include_bytes!("fixtures/telemetry/grok-1.0.46/turn-1-logs.json")).unwrap();
    let logs = root["resourceLogs"][0]["scopeLogs"][0]["logRecords"].as_array_mut().unwrap();
    logs.retain(|r| r["eventName"] == "grok_code.api_request");
    let attrs = logs[0]["attributes"].as_array_mut().unwrap();
    attrs.retain(|a| !["session.id", "event.sequence"].contains(&a["key"].as_str().unwrap()));
    for attr in attrs {
        if attr["key"] == "cache_creation_tokens" { attr["value"]["intValue"] = json!("5"); }
    }
    let bytes = pb_request(&root, false);
    assert_eq!(otlp::ingest_attempt(&f.project, "/v1/logs", &bytes, "application/x-protobuf", token).unwrap(), 1);
    assert_eq!(otlp::ingest_attempt(&f.project, "/v1/logs", &bytes, "application/x-protobuf", token).unwrap(), 0);
    let before = f.cli_args(&["otlp", "records"]).0;
    assert_eq!(before[0]["kind"], "usage");
    assert_eq!(before[0]["attributes"]["input_tokens"], 15426);
    assert_eq!(before[0]["attributes"]["cache_write_input_tokens"], 5);
    assert_eq!(before[0]["attributes"]["total_tokens"], 15440);
    for attr in root["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0]["attributes"].as_array_mut().unwrap() {
        if attr["key"] == "cache_read_tokens" { attr["value"]["intValue"] = json!("15426"); }
    }
    let good = serde_json::from_slice::<Value>(include_bytes!("fixtures/telemetry/grok-1.0.46/turn-2-logs.json")).unwrap();
    let good = good["resourceLogs"][0]["scopeLogs"][0]["logRecords"].as_array().unwrap().iter().find(|r| r["eventName"] == "grok_code.api_request").unwrap().clone();
    root["resourceLogs"][0]["scopeLogs"][0]["logRecords"].as_array_mut().unwrap().push(good);
    assert_eq!(otlp::ingest_attempt(&f.project, "/v1/logs", &pb_request(&root, false), "application/x-protobuf", token).unwrap(), 2);
    let records = f.cli_args(&["otlp", "records"]).0;
    let rows = records.as_array().unwrap();
    assert_eq!(rows.iter().filter(|r| r["kind"] == "usage").count(), 2);
    assert!(rows.iter().any(|r| r["kind"] == "usage" && r["attributes"]["input_tokens"] == 15468));
    let diagnostic = rows.iter().find(|r| r["reason"] == "invalid_usage_counters").unwrap();
    assert_eq!(diagnostic["kind"], "unmapped");
    assert!(diagnostic.get("attributes").is_none());
    assert!(diagnostic.get("usage_authority").is_none());
    assert!(diagnostic["unmapped_attribute_keys"].as_array().unwrap().contains(&json!("cache_read_tokens")));
    f.cli_args(&["accounting", "sync"]);
    let ledger = f.cli_args(&["accounting", "entries"]).0;
    assert_eq!(accepted_delta_entries(&ledger).len(), 2);
    assert!(ledger["entries"].as_array().unwrap().iter().any(|e| e["normalized"]["cache_write_tokens"] == 5 && e["normalized"]["new_input_tokens"] == 14141));
    privacy_scan(&f, &["lc4-planted-secret@example.invalid"]);
}

#[test]
fn grok_continuation_sequence_restart_does_not_merge_distinct_api_calls() {
    let f = Fixture::reserved();
    let minted = otlp::mint_attempt_token(&f.project, &f.attempt, 600).unwrap();
    let token = minted["token"].as_str().unwrap();
    let mut root: Value = serde_json::from_slice(include_bytes!("fixtures/telemetry/grok-1.0.46/turn-1-logs.json")).unwrap();
    let logs = root["resourceLogs"][0]["scopeLogs"][0]["logRecords"].as_array_mut().unwrap();
    logs.retain(|r| r["eventName"] == "grok_code.api_request");
    assert_eq!(otlp::ingest_attempt(&f.project, "/v1/logs", &pb_request(&root, false), "application/x-protobuf", token).unwrap(), 1);
    // A resumed headless process restarts its event sequence. Prompt/turn
    // context keeps an equally numbered call in the same session distinct.
    for attr in root["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0]["attributes"].as_array_mut().unwrap() {
        if attr["key"] == "turn_number" { attr["value"]["intValue"] = json!("1"); }
        if attr["key"] == "prompt.id" { attr["value"]["stringValue"] = json!("resumed-prompt-fixture"); }
    }
    let bytes = pb_request(&root, false);
    assert_eq!(otlp::ingest_attempt(&f.project, "/v1/logs", &bytes, "application/x-protobuf", token).unwrap(), 1);
    assert_eq!(otlp::ingest_attempt(&f.project, "/v1/logs", &bytes, "application/x-protobuf", token).unwrap(), 0);
    let records = f.cli_args(&["otlp", "records"]).0;
    assert_eq!(records.as_array().unwrap().len(), 2);
    assert!(records.as_array().unwrap().iter().all(|r| r["kind"] == "usage"));
    assert_eq!(records.as_array().unwrap().iter().map(|r| r["attributes"]["input_tokens"].as_u64().unwrap()).sum::<u64>(), 30852);
    privacy_scan(&f, &["lc4-planted-secret@example.invalid"]);
}

#[test]
fn uncertified_unbound_and_metric_only_grok_never_enter_accounting() {
    for excluded in ["uncertified", "unbound", "metric_only", "codex"] {
        let f = Fixture::reserved();
        let metrics = excluded == "metric_only";
        let signal = if metrics { "metrics" } else { "logs" };
        let mut root: Value = serde_json::from_slice(&fs::read(format!("{}/tests/fixtures/telemetry/grok-1.0.46/turn-1-{signal}.json", env!("CARGO_MANIFEST_DIR"))).unwrap()).unwrap();
        let group = if metrics { "resourceMetrics" } else { "resourceLogs" };
        let attrs = root[group][0]["resource"]["attributes"].as_array_mut().unwrap();
        attrs.push(json!({"key":"herdr.attempt_id","value":{"stringValue":f.attempt}}));
        if excluded == "unbound" { attrs.retain(|a| a["key"] != "herdr.attempt_id"); }
        if excluded == "uncertified" {
            attrs.retain(|a| a["key"] != "client.version");
            attrs.push(json!({"key":"client.version","value":{"stringValue":"1.0.99"}}));
        }
        if excluded == "codex" {
            attrs.retain(|a| a["key"] != "service.name");
            attrs.push(json!({"key":"service.name","value":{"stringValue":"codex"}}));
        }
        otlp::ingest(&f.project, if metrics { "/v1/metrics" } else { "/v1/logs" }, &serde_json::to_vec(&root).unwrap()).unwrap();
        f.cli_args(&["accounting", "sync"]);
        let ledger = f.cli_args(&["accounting", "entries"]).0;
        assert!(ledger["entries"].as_array().unwrap().is_empty(), "{excluded}: {ledger}");
        let usage = f.cli_args(&["usage", "--json"]).0;
        assert!(usage["attempts"].as_array().unwrap().iter().all(|a| a["usage"]["status"] == "unavailable"));
        privacy_scan(&f, &["lc4-planted-secret@example.invalid"]);
    }
}

#[test]
fn accounting_upgrade_backfills_existing_grok_records_and_preserves_native_ledger() {
    let f = Fixture::new();
    f.rollout(&f.home, SID, &["head.jsonl"], &f.worktree(), f.decided + 1000, "0.154.0");
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    let native = f.cli_args(&["accounting", "entries"]).0;
    let mut request: Value = serde_json::from_slice(&fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/telemetry/grok-1.0.46/turn-1-logs.json")).unwrap()).unwrap();
    request["resourceLogs"][0]["resource"]["attributes"].as_array_mut().unwrap()
        .push(json!({"key":"herdr.attempt_id","value":{"stringValue":f.attempt}}));
    otlp::ingest(&f.project, "/v1/logs", &serde_json::to_vec(&request).unwrap()).unwrap();
    let evidence = f.cli_args(&["otlp", "records"]).0;
    // Reconstruct the known-gap v18 state: retained OTLP evidence, with no
    // OTLP accounting bridge or projected source rows. Native ledger stays.
    let db = f.sidecar();
    db.execute_batch("DROP TRIGGER accounting_otlp_insert; DROP TRIGGER accounting_otlp_update; DROP TRIGGER accounting_otlp_delete;
        DROP TRIGGER accounting_native_otlp_insert; DROP TRIGGER accounting_native_otlp_update; DROP TRIGGER accounting_native_otlp_delete;
        DROP VIEW otlp_ledger_sources;
        DELETE FROM codex_usage_times WHERE session_id LIKE 'otlp:%';
        DELETE FROM codex_usage WHERE session_id LIKE 'otlp:%';
        DELETE FROM rollout_sources WHERE originator LIKE 'otlp:%';
        UPDATE telemetry_streams SET version=18 WHERE stream='accounting';").unwrap();
    drop(db);
    assert_eq!(f.cli_args(&["accounting", "entries"]).0, native);
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(f.cli_args(&["accounting", "status"]).0["version"], 19);
    let upgraded = f.cli_args(&["accounting", "entries"]).0;
    assert_eq!(accepted_delta_entries(&upgraded).len(), 2);
    assert!(upgraded["entries"].as_array().unwrap().contains(&native["entries"][0]));
    assert_eq!(f.report()["metrics"]["M08"]["value"], 16426);
    assert_eq!(f.cli_args(&["otlp", "records"]).0, evidence);
    f.cli("collect");
    f.cli_args(&["accounting", "sync"]);
    assert_eq!(f.cli_args(&["accounting", "entries"]).0, upgraded);
    privacy_scan(&f, &["lc4-planted-secret@example.invalid"]);
}

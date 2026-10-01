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
    logs["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0]["body"] =
        json!({"stringValue":"gemini_cli.api.response"});
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
    assert_eq!(grok["certified_versions"], json!([]));
    assert_eq!(grok["native_source"]["certified"], "none");
    assert!(grok["fields"].as_array().unwrap().iter().filter(|f| f["available"] == true).all(|f| f["certified"] == "fixture"));
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
            .filter(|r| r["native_name"] == "grok_code.token.usage" && r["kind"] != "unmapped")
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
    assert_eq!(
        otlp::records(&f.project)
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["binding"] == "unknown_attempt" && r.get("reason").is_none())
            .count(),
        10
    );
    privacy_scan(&f, &[attempt_token, "GROK_SECRET_CONTENT"]);
}

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
            b"",
            0
        ),
        415
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
    otlp::ingest(&f.project, "/v1/logs", &payload(&f, "claude-logs")).unwrap();
    let before = otlp::records(&f.project).unwrap();
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
    let (classes, _) = f.cli_args(&["maintenance", "classes", "--json"]);
    let class = classes["classes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["class"] == "sidecar.otlp")
        .unwrap();
    assert_eq!(class["action"], "retain");
    assert_eq!(class["basis"], "source_of_truth");
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
        2
    );
    drop(copy);
    f.cli_args(&["backup", "verify", "--from", backup.to_str().unwrap()]);
    f.cli_args(&["backup", "restore", "--from", backup.to_str().unwrap()]);
    assert_eq!(otlp::records(&f.project).unwrap(), before);
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

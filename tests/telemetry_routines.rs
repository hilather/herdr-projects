//! Scheduled telemetry workflows through owner-signed routine-store CLI.
#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)]
mod support;
use support::replay::*;
use herdr_farm::{authority, domain::*, migration, runtime};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf};

fn enable(lab: &Lab) {
    let config = lab.path(".config/herdr-farm/config.toml");
    let mut text = fs::read_to_string(&config).unwrap();
    text += &format!("\n[safety.{:?}]\nroutine_commands=true\n", lab.project.canonicalize().unwrap().display().to_string());
    fs::write(config, text).unwrap();
    lab.observe();
    let s = lab.state();
    runtime::set_state(&lab.project, s.head, s.control.unwrap().revision, ProjectState::Active, &lab.path(".config/herdr-farm/config.toml")).unwrap();
}
fn install(lab: &Lab, name: &str, template: Value) {
    let config = lab.path(".config/herdr-farm/config.toml");
    let script = format!("# herdr-telemetry-routine.v1\n{template}\n");
    let path = lab.project.join(format!("{name}.routine"));
    fs::write(&path, &script).unwrap();
    let definition = RoutineDefinition { version: 1, name: name.into(), revision: 1, project_store: lab.store(), authority: authority::policy_reference(&lab.project).unwrap(), config: migration::config_reference(&config).unwrap(), enabled: true, schedule: "every 168h".into(), timezone: "UTC".into(), start_unix_ms: jiff::Timestamp::now().as_millisecond() - 1000, missed: MissedRunPolicy::CoalesceLatest, overlap: OverlapPolicy::Skip, script: path.display().to_string(), script_sha256: format!("{:x}", Sha256::digest(script.as_bytes())), cwd: lab.project.canonicalize().unwrap().display().to_string(), deadline_ms: 60_000, output_cap_bytes: 16_384 };
    let document = lab.path(&format!("{name}.json"));
    fs::write(&document, serde_json::to_vec(&definition).unwrap()).unwrap();
    sign(&lab.key, authority::ROUTINE_SIGNATURE_NAMESPACE, &document);
    lab.ok(&["routine-store", "demo", "import", document.to_str().unwrap(), document.with_extension("json.sig").to_str().unwrap(), "--expected-head", &lab.head().to_string()]);
}
fn schedule(lab: &Lab, name: &str) -> String {
    let occurrence = lab.ok(&["routine-store", "demo", "schedule", name, "--expected-head", &lab.head().to_string()]);
    occurrence["operation"].as_str().unwrap().into()
}
fn execute(lab: &Lab, operation: &str) -> Value {
    lab.ok(&["routine-store", "demo", "execute", operation, "--expected-head", &lab.head().to_string()])
}

#[test]
fn weekly_report_versions_manifest_and_worker_brief_exclusion() {
    let mut lab = Lab::new("unknown_usage='allow_with_warning'");
    enable(&lab);
    install(&lab, "weekly", json!({"kind":"weekly_report"}));
    let operation = schedule(&lab, "weekly");
    let before = lab.state();
    let receipt = execute(&lab, &operation);
    assert_eq!(receipt["succeeded"], true, "{receipt}");
    let result: Value = serde_json::from_slice(&serde_json::from_value::<Vec<u8>>(receipt["stdout"].clone()).unwrap()).unwrap();
    let path = PathBuf::from(result["report"].as_str().unwrap());
    let report = fs::read_to_string(&path).unwrap();
    let week = jiff::Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC).strftime("%G-W%V").to_string();
    assert_eq!(path, lab.project.join(format!("library/fleet-{week}.md")));
    assert_eq!(report, r#"# Fleet report: demo

Advisory; grants no launch, budget or selection. All-time evidence at export query time.

- M01: 0; basis=terminal_cohort/task_terminal_time; coverage=complete; n=n/a (not_applicable:no_denominator)
- M02: n/a (empty_denominator); basis=terminal_cohort/task_terminal_time; coverage=complete; n=0
- M13: n/a (collection_not_run); basis=activity_window/attempt_decided; coverage=unavailable; n=n/a (unavailable:collection_not_run)
- M38: n/a (throttling_not_certified); basis=activity_window/availability_interval; coverage=unavailable; n=n/a (unavailable:throttling_not_certified)
- M39: n/a (provider_errors_not_certified); basis=activity_window/invocation; coverage=unavailable; n=n/a (unavailable:provider_errors_not_certified)
- M40: n/a (value_not_reported); basis=activity_window/attempt_decided; coverage=unknown; n=n/a (unavailable:value_not_reported)
- M49: n/a (no_replay_suite); basis=activity_window/replay_attempt_decided; coverage=unavailable; n=n/a (unavailable:no_replay_suite)
"#);
    let manifest: Value = serde_json::from_slice(&fs::read(result["manifest"].as_str().unwrap()).unwrap()).unwrap();
    assert_eq!(manifest["contract"], "export.v1");
    assert_eq!(manifest["report"]["digest"], "sha256:b88a59705009326f276a8425fffadf7aba6dca4efe17355b1f14a71dddbd9def");
    assert_eq!(manifest["report"]["bytes"], 1015);
    let after = lab.state();
    assert_eq!((&after.tasks, &after.attempts, &after.approvals), (&before.tasks, &before.attempts, &before.approvals));
    install(&lab, "weekly-again", json!({"kind":"weekly_report"}));
    let receipt = execute(&lab, &schedule(&lab, "weekly-again"));
    assert_eq!(receipt["succeeded"], true);
    assert_eq!(fs::read_to_string(&path).unwrap(), report);
    assert!(lab.project.join(format!("library/fleet-{week}-v2.md")).exists());
    // A freshly retained worker snapshot and approved attempt after publication
    // must still select only instructions and scoped memory, never library files.
    lab.prepare_profile();
    let binding = lab.state().runtime_bindings.iter().find(|b| b.task.as_ref().is_some_and(|t| t.as_str() == "work")).unwrap().id.as_str().to_owned();
    let selection = lab.selection("work", &binding, &lab.repo, None);
    let attempt = lab.reserve(&selection);
    let brief = herdr_farm::memory::render_attempt_brief(&lab.project, attempt.as_str()).unwrap();
    assert!(brief.text.contains("Retained instructions"));
    assert!(!brief.text.contains("# Fleet report:") && !brief.text.contains("no_replay_suite") && !brief.text.contains(&format!("fleet-{week}")));
}

#[test]
fn scheduled_replay_queues_only_drawn_cases_and_refuses_missing_inputs() {
    let mut lab = Lab::new("unknown_usage='allow_with_warning'");
    lab.build_history();
    enable(&lab);
    lab.replay(&["extract", "--suite", "v1"]);
    install(&lab, "replay", json!({"kind":"replay","suite":"v1","configuration":"worker","subset":"stratified:2","seed":"routine"}));
    let operation = schedule(&lab, "replay");
    let before = lab.state();
    let receipt = execute(&lab, &operation);
    assert_eq!(receipt["succeeded"], true, "{receipt}");
    let run: Value = serde_json::from_slice(&serde_json::from_value::<Vec<u8>>(receipt["stdout"].clone()).unwrap()).unwrap();
    assert_eq!(run["cases"], json!(["farewell.r1", "usage-docs.r1"]));
    assert_eq!(run["tasks"].as_array().unwrap().len(), 2);
    assert_eq!(run["tasks"][0]["task_id"], "replay-v1-2-1");
    assert_eq!(run["tasks"][1]["task_id"], "replay-v1-2-2");
    let after = lab.state();
    let added: Vec<_> = after.tasks.iter().filter(|t| !before.tasks.iter().any(|old| old.id == t.id)).collect();
    assert_eq!(added.len(), 2);
    assert!(added.iter().all(|t| t.state == TaskState::Queued && t.active_attempt.is_none()));
    assert_eq!((&after.attempts, &after.approvals, &after.attempt_inputs, &after.budget_policies), (&before.attempts, &before.approvals, &before.attempt_inputs, &before.budget_policies));
    assert_eq!(after.operations, before.operations);
    assert_eq!(after.deliveries.len(), before.deliveries.len());
    for task in added {
        assert_eq!(lab.db().query_row("SELECT count(*) FROM task_contracts WHERE task_id=?1", [task.id.as_str()], |r| r.get::<_, u64>(0)).unwrap(), 0);
    }
    for (name, suite, configuration, reason) in [("missing", "missing", "worker", "suite"), ("unknown", "v1", "unknown", "unknown replay configuration")] {
        install(&lab, name, json!({"kind":"replay","suite":suite,"configuration":configuration,"subset":"stratified:2","seed":"routine"}));
        let operation = schedule(&lab, name);
        let before = lab.state();
        let error = lab.fail(&["routine-store", "demo", "execute", &operation, "--expected-head", &lab.head().to_string()]);
        assert!(error.contains(reason), "{error}");
        assert_eq!(runtime::snapshot(&lab.project).unwrap(), before);
    }
}

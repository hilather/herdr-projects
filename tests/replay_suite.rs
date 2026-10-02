#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)]
mod support;
use support::replay::*;
use herdr_farm::authority;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, os::unix::fs::MetadataExt, path::{Path, PathBuf}, process::Command};

/// Suite v1 golden: the fixture's five accepted changes yield three eligible
/// cases (two `code`, one `docs`), one contaminated case (`secret-flag`: its
/// solution line is pasted in PROJECT.md, found by the metadata-only scan)
/// and one exclusion (`readme-typo` has no test file, so no hidden check).
/// Each case's base is the integration tip before the accepted change, its
/// reference the accepted candidate; the hidden checks' bytes live only in
/// the owner's check store outside the project, never in the store.
/// Extraction is deterministic (a second suite version records the same
/// cases), a suite version is immutable, the stratified subset is
/// reproducible and covers both strata, and a retired case leaves the draw.
/// A run needs no owner-wide `[worker_isolation] hide` of the source
/// repository: each candidate's sandbox hides it per launch.
#[test]
fn extraction_golden_contamination_counts_and_reproducible_subset() {
    let mut lab = Lab::new("unknown_usage='allow_with_warning'");
    lab.build_history();
    let extracted = lab.replay(&["extract", "--suite", "v1"]);
    let golden: Value = serde_json::from_str(&fs::read_to_string(GOLDEN).unwrap()).unwrap();
    let normalized = json!({"suite": {"suite_version": extracted["suite"]["suite_version"], "extractor": extracted["suite"]["extractor"], "exclusions": extracted["suite"]["exclusions"]},
        "cases": lab.normalize(&extracted["cases"])});
    assert_eq!(normalized, golden, "{}", serde_json::to_string_pretty(&normalized).unwrap());
    // Hidden checks: private, content-addressed, outside the project, with the test file's exact bytes.
    let checks = lab.root().join(".replay/demo/checks");
    assert_eq!(fs::metadata(&checks).unwrap().mode() & 0o777, 0o700);
    for case in extracted["cases"].as_array().unwrap() {
        let change = lab.change(case["case_id"].as_str().unwrap());
        for check in case["hidden_checks"].as_array().unwrap() {
            let path = checks.join(check["sha256"].as_str().unwrap());
            let expected = change["files"][format!("tests/expected/{}", check["target"].as_str().unwrap())].as_str().unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), expected);
            assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
            for file in ["state.db", "state.db-wal"] {
                assert!(!contains(&fs::read(lab.project.join(".state").join(file)).unwrap_or_default(), expected.trim()), "hidden check content in {file}");
            }
        }
    }
    // Deterministic: another version over the same history records the same cases.
    let again = lab.replay(&["extract", "--suite", "v1-again"]);
    assert_eq!(again["cases"], extracted["cases"]);
    assert_eq!(again["suite"]["exclusions"], extracted["suite"]["exclusions"]);
    assert!(lab.fail(&["replay", "demo", "extract", "--suite", "v1"]).contains("immutable"));
    // Reproducible stratified subset: same seed, same draw; both strata; never the contaminated case.
    let subset = |n: &str, seed: &str| lab.replay(&["subset", "--suite", "v1", "--subset", &format!("stratified:{n}"), "--seed", seed])["cases"].clone();
    let two = subset("2", "alpha");
    assert_eq!(two, subset("2", "alpha"));
    let strata: Vec<&str> = two.as_array().unwrap().iter().map(|c| if c == "usage-docs.r1" { "docs" } else { "code" }).collect();
    assert_eq!(strata.iter().filter(|s| **s == "docs").count(), 1, "{two}");
    assert_eq!(subset("9", "alpha").as_array().unwrap().len(), 3);
    assert!(!subset("9", "beta").as_array().unwrap().contains(&json!("secret-flag.r1")));
    // Retirement: the case stays readable, leaves every later draw, and is append-only.
    lab.replay(&["retire", "--suite", "v1", "--case", "farewell.r1", "--reason", "farewell test no longer applies"]);
    let shown = lab.replay(&["show", "--suite", "v1"]);
    assert_eq!((shown["cases"].as_array().unwrap().len(), &shown["retired"][0]["case_id"]), (4, &json!("farewell.r1")));
    let mut drawn: Vec<String> = subset("9", "alpha").as_array().unwrap().iter().map(|c| c.as_str().unwrap().to_owned()).collect();
    drawn.sort();
    assert_eq!(drawn, ["greet-name.r1", "usage-docs.r1"]);
    assert!(lab.fail(&["replay", "demo", "retire", "--suite", "v1", "--case", "farewell.r1", "--reason", "again"]).contains("already retired"));
    let db = lab.db();
    for sql in ["UPDATE replay_cases SET status='eligible'", "DELETE FROM replay_retirements", "UPDATE replay_suites SET exclusions='{}'"] {
        assert!(db.execute(sql, []).unwrap_err().to_string().contains("append-only"), "{sql}");
    }
    drop(db);
    // The owner configuration hides nothing extra (no `[worker_isolation]`), yet the run starts.
    assert!(!fs::read_to_string(lab.path(".config/herdr-farm/config.toml")).unwrap().contains("worker_isolation"));
    lab.replay(&["run", "--suite", "v1", "--configuration", "alpha", "--subset", "stratified:1", "--seed", "alpha", "--expected-head", &lab.head().to_string()]);
    assert_eq!(lab.db().query_row("SELECT count(*) FROM replay_runs", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
}

/// A stale CLI head is checked after staging and leaves neither repositories
/// nor a partially recorded run or queued candidate.
#[test]
fn stale_run_head_cleans_staged_repositories_without_recording() {
    let mut lab = Lab::new("unknown_usage='allow_with_warning'");
    lab.build_history();
    lab.replay(&["extract", "--suite", "v1"]);
    let old_head = lab.head();
    lab.ok(&["task", "demo", "add", "unrelated", "--title", "unrelated", "--expected-head", &old_head.to_string()]);
    let before = lab.state();
    let error = lab.fail(&["replay", "demo", "run", "--suite", "v1", "--configuration", "manual", "--subset", "stratified:3", "--seed", "alpha", "--expected-head", &old_head.to_string()]);
    assert!(error.contains(&format!("project head is {}, expected {old_head}", before.head)), "{error}");
    assert_eq!(lab.replay(&["show", "--suite", "v1"])["runs"], json!([]));
    let after = lab.state();
    assert_eq!(after.head, before.head);
    assert_eq!(after.tasks, before.tasks);
    assert_eq!(after.scheduler.unwrap().queue, before.scheduler.unwrap().queue);
    let repos = lab.root().join(".replay/demo/repos/v1");
    assert_eq!(fs::read_dir(repos).unwrap().count(), 0);
}

/// A late store error rolls back even the run and earlier candidates.
#[test]
fn failed_run_store_step_rolls_back_and_cleans_repositories() {
    let mut lab = Lab::new("unknown_usage='allow_with_warning'");
    lab.build_history();
    lab.replay(&["extract", "--suite", "v1"]);
    lab.db().execute_batch("CREATE TRIGGER refuse_replay_candidate BEFORE INSERT ON replay_candidates BEGIN SELECT RAISE(ABORT, 'fixture store failure'); END").unwrap();
    let before = lab.state();
    lab.fail(&["replay", "demo", "run", "--suite", "v1", "--configuration", "manual", "--subset", "stratified:3", "--seed", "alpha", "--expected-head", &before.head.to_string()]);
    assert_eq!(lab.replay(&["show", "--suite", "v1"])["runs"], json!([]));
    let after = lab.state();
    assert_eq!(after.head, before.head);
    assert_eq!(after.tasks, before.tasks);
    assert_eq!(after.scheduler.unwrap().queue, before.scheduler.unwrap().queue);
    assert_eq!(fs::read_dir(lab.root().join(".replay/demo/repos/v1")).unwrap().count(), 0);
}

/// Run one replay of v1 and install its drafted contract: the task is an
/// ordinary queued task whose contract is verify-only over a replay
/// repository at the case's base, with one hidden-check policy.
fn replay_one(lab: &mut Lab, configuration: &str, seed: &str, route: Option<&str>) -> (Value, String, String, Value) {
    lab.build_history();
    lab.replay(&["extract", "--suite", "v1"]);
    let run = lab.replay(&["run", "--suite", "v1", "--configuration", configuration, "--subset", "stratified:1", "--seed", seed, "--expected-head", &lab.head().to_string()]);
    let task = run["tasks"][0]["task_id"].as_str().unwrap().to_owned();
    let (digest, document) = lab.install_replay_contract_routed(&task, route);
    (run, task, digest, document)
}

/// A replay candidate is verified by its hidden check in the isolated
/// verifier (a wrong candidate fails, the reference solution passes, and a
/// hidden file changed on disk is refused as unavailable), yet it never
/// integrates: the operator's `result integrate` is refused before any write,
/// the automatic producer never enqueues it and raw SQL cannot create an
/// integration job, lease or operation for it. Nothing can depend on it:
/// `task queue` naming it as a predecessor and a raw satisfaction row are refused.
#[test]
fn replay_candidate_is_verified_by_hidden_checks_but_never_integrates_or_releases_dependents() {
    let mut lab = Lab::new("unknown_usage='allow_with_warning'");
    let (run, task, digest, document) = replay_one(&mut lab, "manual", "gamma", Some("verify_then_integrate"));
    let case = run["cases"][0].as_str().unwrap().to_owned();
    let repository = PathBuf::from(run["tasks"][0]["repository"].as_str().unwrap());
    let base = run["tasks"][0]["base_oid"].as_str().unwrap().to_owned();
    assert!(repository.starts_with(lab.root().canonicalize().unwrap().join(".replay/demo/repos/v1")));
    // The replay repository holds only the base history: neither the reference solution nor its tests.
    let shown = lab.replay(&["show", "--suite", "v1"]);
    let record = shown["cases"].as_array().unwrap().iter().find(|c| c["case_id"] == case.as_str()).unwrap().clone();
    assert_eq!(lab.git_in(&repository, &["rev-parse", "HEAD"]), base);
    let all = lab.git_in(&repository, &["rev-list", "--all"]);
    assert!(!all.contains(record["reference_oid"].as_str().unwrap()) && !all.contains(record["integrated_oid"].as_str().unwrap()));
    let tree = lab.git_in(&repository, &["ls-tree", "-r", "--name-only", "HEAD"]);
    for check in record["hidden_checks"].as_array().unwrap() {
        assert!(!tree.lines().any(|p| p == format!("tests/expected/{}", check["target"].as_str().unwrap())), "{tree}");
    }
    // The contract is verify-only with one policy per hidden check; it names the hidden file by path and digest only.
    // (The owner re-routed this one to integration before signing: the guard does not rest on the route.)
    assert!(document["non_goals"].as_str().unwrap().contains("never integrated"), "{document}");
    assert_eq!((&document["route"], &document["base_oid"], document["acceptance_policies"].as_array().unwrap().len()), (&json!("verify_then_integrate"), &json!(base), 1));
    let policy = document["acceptance_policies"][0].clone();
    let text = policy["text"].as_str().unwrap().to_owned();
    let expected = record["hidden_checks"][0]["sha256"].as_str().unwrap();
    let hidden = lab.root().canonicalize().unwrap().join(".replay/demo/checks").join(expected);
    let content = fs::read_to_string(&hidden).unwrap();
    assert!(text.contains(expected) && text.contains(hidden.to_str().unwrap()) && !text.contains(content.trim()), "{text}");
    // The task is an ordinary queued task; the run granted nothing.
    let state = lab.state();
    assert!(state.scheduler.as_ref().unwrap().queue.iter().any(|q| q.task.as_str() == task));
    assert!(state.approvals.is_empty() && state.attempts.iter().all(|a| a.task.as_str() != task));

    let attempt = format!("{task}-attempt");
    lab.plant_attempt(&task, &attempt);
    let outputs: Vec<String> = lab.solution(&case).keys().cloned().collect();
    let outputs: Vec<&str> = outputs.iter().map(String::as_str).collect();
    let wrong = lab.commit_files(&repository, "wrong", &base, &lab.solution(&case).keys().map(|k| (k.clone(), "not the expected content\n".to_owned())).collect());
    let wrong = lab.submit(&task, &attempt, &digest, &repository, &base, &wrong, &outputs, "replay-wrong");
    let rejected = lab.verify(&wrong, "hidden-1", &text, "verify-wrong");
    assert_eq!((&rejected["state"], &rejected["reason"]), (&json!("rejected"), &json!("checks_failed")), "{rejected}");
    let right = lab.commit_files(&repository, "right", &base, &lab.solution(&case));
    let right = lab.submit(&task, &attempt, &digest, &repository, &base, &right, &outputs, "replay-right");
    // A hidden file that no longer has its pinned digest is refused before any check runs.
    fs::write(&hidden, "tampered\n").unwrap();
    let unavailable = lab.verify(&right, "hidden-1", &text, "verify-tampered");
    assert_eq!((&unavailable["state"], &unavailable["reason"]), (&json!("rejected"), &json!("hidden_check_unavailable")), "{unavailable}");
    fs::write(&hidden, &content).unwrap();
    let accepted = lab.verify(&right, "hidden-1", &text, "verify-right");
    assert_eq!(accepted["state"], "accepted", "{accepted}");
    let result = accepted["receipt"]["result_id"].as_str().unwrap().to_owned();

    // Never integrates. The replay repository is the configured target's repository only for this refusal.
    lab.git_in(&repository, &["branch", "integration", &base]);
    lab.ok(&["result", "demo", "configure-integration", "--repository", repository.to_str().unwrap(), "--reference", "refs/heads/integration"]);
    let head = lab.head().to_string();
    lab.ok(&["result", "demo", "auto", "--integrate", "on", "--expected-head", &head]);
    let pending = || lab.db().query_row("SELECT count(*) FROM pending_integration_work WHERE submission_id=?1", [&right], |r| r.get::<_, i64>(0)).unwrap();
    assert_eq!(pending(), 1, "the verified replay submission enters the pending projection");
    let turn = herdr_farm::store::service_project_integration_jobs(&lab.project).unwrap();
    assert_eq!((turn.enqueued, pending()), (0, 0), "the producer drops it");
    let work = lab.path("integrate-replay");
    let refused = lab.fail(&["result", "demo", "integrate", &result, "--repository", repository.to_str().unwrap(), "--idempotency-key", "integrate-replay", "--work-dir", work.to_str().unwrap()]);
    assert!(refused.contains("a replay candidate never integrates"), "{refused}");
    assert!(!work.exists());
    assert_eq!(lab.git_in(&repository, &["rev-parse", "refs/heads/integration"]), base);
    let db = lab.db();
    let count = |sql: &str| db.query_row(sql, [&task], |r| r.get::<_, i64>(0)).unwrap();
    assert_eq!(count("SELECT count(*) FROM operations WHERE task_id=?1 AND kind IN ('integration.run','integration.lease')"), 0);
    let hash = "d".repeat(64);
    for (id, kind, payload) in [("raw-job", "integration.run", json!({"submission_id": right})), ("raw-lease", "integration.lease", json!({"result_id": result}))] {
        let error = db.execute("INSERT INTO operations(id,task_id,kind,target,payload_version,payload,payload_hash,expected_revision,due_unix_ms,idempotency_key)
            VALUES(?1,'work',?2,'refs/heads/integration',1,?3,?4,1,0,?1)", rusqlite::params![id, kind, payload.to_string(), hash]).unwrap_err();
        assert!(error.to_string().contains("a replay candidate never integrates"), "{kind}: {error}");
    }
    let error = db.execute("INSERT INTO integration_operations(operation_id,project_store,idempotency_key,payload_digest,repository,ref_name,expected_old_oid,verified_result_id,candidate_id,state,generation,object_format,checks_passed,reason,created_unix_ms)
        VALUES('raw-op',?1,'raw-op',?2,?3,'refs/heads/integration',?4,?5,NULL,'effect_pending',1,'sha256',0,NULL,1)",
        rusqlite::params![lab.store(), hash, repository.display().to_string(), base, result]).unwrap_err();
    assert!(error.to_string().contains("a replay candidate never integrates"), "{error}");
    // Never releases a dependent.
    let error = db.execute("INSERT INTO dependency_satisfactions(satisfaction_id,task_id,predecessor_task,requirement,state,evidence_kind,evidence_id,created_unix_ms)
        VALUES(?1,'work',?2,'verified_result','valid','verified_result',?3,1)", rusqlite::params![hash, task, result]).unwrap_err();
    assert!(error.to_string().contains("never releases dependents"), "{error}");
    for sql in ["UPDATE replay_candidates SET run_id=run_id", "DELETE FROM replay_candidates"] {
        assert!(db.execute(sql, []).unwrap_err().to_string().contains("append-only"), "{sql}");
    }
    drop(db);
    lab.ok(&["task", "demo", "add", "consumer", "--title", "consumer", "--expected-head", &lab.head().to_string()]);
    let request = lab.path("consumer-queue.json");
    fs::write(&request, json!({"priority": 0, "dependencies": [{"predecessor": task, "requirement": "verified_result"}]}).to_string()).unwrap();
    let refused = lab.fail(&["task", "demo", "queue", "consumer", "--input-file", request.to_str().unwrap(), "--expected-revision", "1", "--expected-head", &lab.head().to_string()]);
    assert!(refused.contains("never releases dependents"), "{refused}");
    // The source tasks integrated normally; only the replay candidate is held back.
    assert_eq!(lab.db().query_row("SELECT count(*) FROM integrated_commits", [], |r| r.get::<_, i64>(0)).unwrap(), 5);
}

/// A probe agent for a replay candidate: it tries to read each hidden check
/// file, list the owner's replay directories and the source repository, and
/// searches everything it can read (its project, its worktree and every
/// revision of its repository) for the hidden check's content. Then, as a
/// deterministic "pass" agent, it writes the solution it was built with,
/// commits, publishes `probe-1.txt` (report, `head`, `objects`), waits for
/// the owner's `submit.json` and submits through its spool (`probe-2.txt`).
const PROBE_AGENT: &str = r#"
use std::{fs, path::Path, process::Command, time::Duration};
fn publish(name: &str, text: &str) { fs::write(format!("{name}.tmp"), text).unwrap(); fs::rename(format!("{name}.tmp"), name).unwrap(); }
fn run(program: &str, args: &[&str]) -> (bool, String) {
    match Command::new(program).args(args).output() {
        Ok(out) => (out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)).replace('\n', "|")),
        Err(error) => (false, error.to_string()),
    }
}
fn walk(dir: &Path, needle: &[u8], hits: &mut Vec<String>, seen: &mut usize) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        *seen += 1;
        if *seen > 20000 { return }
        let path = entry.path();
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() { walk(&path, needle, hits, seen); }
        else if kind.is_file() {
            if let Ok(bytes) = fs::read(&path) { if bytes.windows(needle.len()).any(|w| w == needle) { hits.push(path.display().to_string()); } }
        }
    }
}
fn main() {
    if std::env::args().nth(1).as_deref() == Some("--version") { println!("2.1.0 (Claude Code)"); return }
    let mut report = String::new();
    for path in READS {
        report += &format!("read {path} {}\n", match fs::read(path) { Ok(_) => "OK".to_owned(), Err(error) => format!("ERR:{:?}", error.kind()) });
    }
    for dir in LISTS {
        let listed = fs::read_dir(dir).map(|d| { let mut n: Vec<String> = d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect(); n.sort(); n.join(",") });
        report += &format!("list {dir} {}\n", match listed { Ok(names) => format!("OK:{names}"), Err(error) => format!("ERR:{:?}", error.kind()) });
    }
    let mut hits = Vec::new();
    let mut seen = 0;
    for dir in SEARCH { walk(Path::new(dir), SENTINEL.as_bytes(), &mut hits, &mut seen); }
    walk(Path::new("."), SENTINEL.as_bytes(), &mut hits, &mut seen);
    report += &format!("hits {}\nsearched {}\n", hits.join(","), seen);
    let git = |args: &[&str]| run("/usr/bin/git", &[&["-c", "user.name=worker", "-c", "user.email=worker@example.invalid"], args].concat());
    let revisions = git(&["rev-list", "--all"]).1;
    report += &format!("reference {}\n", revisions.contains(REFERENCE));
    let grep = git(&["grep", "-F", "-l", SENTINEL.trim(), "--all-match", "HEAD"]);
    report += &format!("grep {}\n", grep.0);
    for (path, text) in SOLUTION {
        if let Some(parent) = Path::new(path).parent() { fs::create_dir_all(parent).unwrap(); }
        fs::write(path, text).unwrap();
    }
    // Control: the same search finds the content once the solution is written.
    let mut control = Vec::new();
    walk(Path::new("."), SENTINEL.as_bytes(), &mut control, &mut 0);
    report += &format!("control {}\n", control.len());
    let (added, _) = git(&["add", "-A"]);
    let (committed, output) = git(&["commit", "-qm", "replay candidate"]);
    let head = git(&["rev-parse", "HEAD"]).1.trim_end_matches('|').to_owned();
    report += &format!("commit {} {}\nhead {head}\n", added && committed, output);
    report += &format!("objects {}\n", git(&["rev-list", "--objects", &head]).1.trim_end_matches('|').split('|').map(|l| l.split(' ').next().unwrap()).collect::<Vec<_>>().join(","));
    publish("probe-1.txt", &report);
    while !Path::new("submit.json").exists() { std::thread::sleep(Duration::from_millis(50)); }
    let mut last = (false, String::new());
    for _ in 0..200 {
        last = run(BIN, &["--root", ROOT, "result", "demo", "submit", "--input-file", "submit.json"]);
        if last.0 { break }
        std::thread::sleep(Duration::from_millis(100));
    }
    publish("probe-2.txt", &format!("submitted {} {}\n", last.0, last.1));
    loop { std::thread::park() }
}
"#;

/// A replay candidate launched for real: `replay run`, the owner-signed
/// contract, a binding on the replay repository, `launch draft`, the owner's
/// signed approval and `launch reserve`, then the ticker creates the worker
/// on the Herdr stand-in. Inside its sandbox the candidate cannot read the
/// hidden check file, list the owner's replay directories or read the source
/// repository; the hidden check's content is nowhere it can read (its
/// project, worktree, retained knowledge or any revision of its repository),
/// and the reference solution is not in its repository. It still commits
/// and submits through its spool; the hidden check then passes in the
/// isolated verifier, and the report counts it for the profile's
/// configuration (dispatched by the operator with reason `replay`) under suite v1.
/// The owner configuration hides nothing extra: the candidate's own supervisor
/// argv hides the source repository and the check store.
#[test]
fn launched_replay_candidate_cannot_read_hidden_checks_and_is_verified_by_them() {
    let mut lab = Lab::new("unknown_usage='allow_with_warning'");
    let (run, task, digest, document) = replay_one(&mut lab, "probe", "delta", None);
    let case = run["cases"][0].as_str().unwrap().to_owned();
    let repository = PathBuf::from(run["tasks"][0]["repository"].as_str().unwrap());
    let base = run["tasks"][0]["base_oid"].as_str().unwrap().to_owned();
    let record = lab.replay(&["show", "--suite", "v1"])["cases"].as_array().unwrap().iter().find(|c| c["case_id"] == case.as_str()).unwrap().clone();
    let checks = lab.root().canonicalize().unwrap().join(".replay/demo/checks");
    let hidden = checks.join(record["hidden_checks"][0]["sha256"].as_str().unwrap());
    let sentinel = fs::read_to_string(&hidden).unwrap();
    let text = document["acceptance_policies"][0]["text"].as_str().unwrap().to_owned();
    let binding = lab.bind(&task, &repository);
    lab.observe();
    let home = lab.home.path().canonicalize().unwrap();
    let quoted = |paths: &[PathBuf]| paths.iter().map(|p| format!("{:?}", p.to_str().unwrap())).collect::<Vec<_>>().join(",");
    let reads = [hidden.clone(), home.join("repo/.git/HEAD"), home.join(format!("repo/{}", lab.change(&case)["files"].as_object().unwrap().keys().next().unwrap()))];
    let lists = [checks.clone(), lab.root().canonicalize().unwrap().join(".replay/demo"), home.join("repo")];
    let search = [lab.project.canonicalize().unwrap(), repository.clone()];
    let solution: Vec<String> = lab.solution(&case).iter().map(|(k, v)| format!("({k:?}, {v:?})")).collect();
    lab.build_agent(&format!("{PROBE_AGENT}\nconst READS: &[&str] = &[{}];\nconst LISTS: &[&str] = &[{}];\nconst SEARCH: &[&str] = &[{}];\nconst SENTINEL: &str = {sentinel:?};\n\
        const REFERENCE: &str = {:?};\nconst SOLUTION: &[(&str, &str)] = &[{}];\nconst ROOT: &str = {:?};\nconst BIN: &str = {BIN:?};\n",
        quoted(&reads), quoted(&lists), quoted(&search), record["reference_oid"].as_str().unwrap(), solution.join(","), lab.root().canonicalize().unwrap().to_str().unwrap()));
    lab.prepare_profile();
    let selection = lab.selection(&task, &binding, &repository, Some("replay"));
    let attempt = lab.reserve(&selection);
    let worktree = PathBuf::from(lab.ok(&["memory", "demo", "attempt-input", "--attempt", attempt.as_str()])["worktrees"][0]["path"].as_str().unwrap());
    lab.serve();
    let mut ticker = lab.spawn();
    lab.wait(&mut ticker, 180, &|| worktree.join("probe-1.txt").exists());
    let report = fs::read_to_string(worktree.join("probe-1.txt")).unwrap();
    for path in &reads { assert!(report.contains(&format!("read {} ERR:", path.display())), "{} was readable:\n{report}", path.display()); }
    // Only the replay repository's own mount point shows under the owner's replay directory.
    for dir in &lists {
        let line = report.lines().find(|l| l.starts_with(&format!("list {} ", dir.display()))).unwrap();
        assert!(line.contains(" ERR:") || line.ends_with(" OK:") || line.ends_with(" OK:repos"), "{line}");
    }
    assert!(report.contains("\nhits \n") && report.contains("\nreference false\n") && report.contains("\ngrep false\n"), "{report}");
    let searched: usize = report.lines().find_map(|l| l.strip_prefix("searched ")).unwrap().parse().unwrap();
    assert!(searched > 20 && report.contains("\ncontrol 1\n"), "{report}");
    assert!(report.contains("\ncommit true"), "{report}");
    let candidate = report.lines().find_map(|l| l.strip_prefix("head ")).unwrap().to_owned();
    let objects: Vec<Value> = report.lines().find_map(|l| l.strip_prefix("objects ")).unwrap().split(',')
        .map(|oid| json!({"oid": oid, "relative_path": format!("{}/{}", &oid[..2], &oid[2..])})).collect();
    let outputs: Vec<Value> = lab.solution(&case).keys().map(|p| json!({"path": p, "oid": candidate})).collect();
    fs::write(worktree.join("submit.json.tmp"), json!({"idempotency_key": "replay-probe", "task_id": task, "contract_revision": 1, "contract_digest": digest,
        "attempt_id": attempt.as_str(), "repository": repository.display().to_string(), "base_oid": base, "candidate_oid": candidate, "object_format": "sha256",
        "artifact_manifest": outputs, "claimed_checks": [], "objects": objects}).to_string()).unwrap();
    fs::rename(worktree.join("submit.json.tmp"), worktree.join("submit.json")).unwrap();
    lab.wait(&mut ticker, 60, &|| worktree.join("probe-2.txt").exists());
    let submitted = fs::read_to_string(worktree.join("probe-2.txt")).unwrap();
    assert!(submitted.starts_with("submitted true"), "{submitted}");
    let submission = lab.ok_live(&|| ["result", "demo", "show"].map(String::from).to_vec()).as_array().unwrap().iter()
        .find(|s| s["task_id"] == task.as_str()).unwrap()["submission_id"].as_str().unwrap().to_owned();
    let policy = lab.path("hidden-policy.json");
    fs::write(&policy, &text).unwrap();
    let tries = std::cell::Cell::new(0);
    let verified = lab.ok_live(&|| { tries.set(tries.get() + 1); ["result", "demo", "verify", &submission, "--policy-id", "hidden-1", "--policy-file", policy.to_str().unwrap(),
        "--idempotency-key", "replay-probe-verify", "--work-dir", &lab.path(&format!("probe-verify-{}", tries.get())).display().to_string()].map(String::from).to_vec() });
    assert_eq!(verified["state"], "accepted", "{verified}");
    // The launch was the operator's, with the ordinary budget reservation and reason `replay`.
    let (configuration, chooser, reasons): (String, String, String) = lab.db().query_row("SELECT chosen_configuration_id,chooser_kind,reason_codes FROM dispatch_decisions WHERE attempt_id=?1",
        [attempt.as_str()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
    assert_eq!((chooser.as_str(), reasons.as_str()), ("operator", r#"["replay"]"#));
    let report = lab.replay(&["report", "--suite", "v1"]);
    assert_eq!(report["configurations"], json!([{"configuration_id": configuration, "labels": ["probe"], "suite_version": "v1", "numerator": 1, "denominator": 1,
        "value": "1/1", "n": 1, "failed": 0, "pending": 0, "by_stratum": {record["stratum"].as_str().unwrap(): {"numerator": 1, "denominator": 1, "value": "1/1"}}}]));
    lab.ok_live(&|| ["task", "demo", "cancel-attempt", attempt.as_str(), "--expected-revision", &lab.state().attempts.iter().find(|a| a.id == attempt).unwrap().revision.to_string(),
        "--expected-head", &lab.head().to_string(), "--reason", "probe done"].map(String::from).to_vec());
    lab.wait(&mut ticker, 60, &|| lab.state().attempts.iter().find(|a| a.id == attempt).unwrap().termination_observed);
    lab.stop(ticker);
    // Never integrated: the source repository and the replay repository are unchanged.
    assert_eq!(lab.git_in(&repository, &["rev-parse", "refs/heads/main"]), base);
    assert_eq!(lab.db().query_row("SELECT count(*) FROM integrated_commits", [], |r| r.get::<_, i64>(0)).unwrap(), 5);
    // The hides were this launch's own: its supervisor argv names the source repository and the check store.
    let argv = launched_commands(&lab);
    assert_eq!(argv.len(), 1);
    for hide in [format!("hide:{}", home.join("repo").display()), format!("hide:{}", checks.display())] { assert!(argv[0].contains(&hide), "{hide}: {:?}", argv[0]); }
}

/// The supervisor argv of every worker the Herdr stand-in created, in order.
fn launched_commands(lab: &Lab) -> Vec<Vec<String>> {
    fs::read_to_string(lab.path("lab/requests")).unwrap().lines().map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|r| r["method"] == "workspace.create_command").map(|r| serde_json::from_value(r["params"]["command"].clone()).unwrap()).collect()
}

/// The replay hide is per launch. With a replay run registered on the source
/// repository `~/repo` and no owner-wide hide, the ordinary task `work` on
/// that repository launches through the ticker: its supervisor argv hides
/// neither the source repository nor the check store, and the probe inside
/// its sandbox reads the repository (an owner-wide hide of `~/repo` would
/// refuse this launch, since the repository it needs would be hidden) while
/// the hidden check, under the covered projects root, stays unreadable.
#[test]
fn ordinary_task_on_the_source_repository_still_launches_without_replay_hides() {
    let mut lab = Lab::new("unknown_usage='allow_with_warning'");
    let (run, _, _, _) = replay_one(&mut lab, "probe", "delta", None);
    let case = run["cases"][0].as_str().unwrap().to_owned();
    let record = lab.replay(&["show", "--suite", "v1"])["cases"].as_array().unwrap().iter().find(|c| c["case_id"] == case.as_str()).unwrap().clone();
    let checks = lab.root().canonicalize().unwrap().join(".replay/demo/checks");
    let hidden = checks.join(record["hidden_checks"][0]["sha256"].as_str().unwrap());
    let home = lab.home.path().canonicalize().unwrap();
    let source = home.join("repo");
    let reads = [hidden.clone(), source.join(".git/HEAD")];
    let quoted = |paths: &[PathBuf]| paths.iter().map(|p| format!("{:?}", p.to_str().unwrap())).collect::<Vec<_>>().join(",");
    lab.build_agent(&format!("{PROBE_AGENT}\nconst READS: &[&str] = &[{}];\nconst LISTS: &[&str] = &[{}];\nconst SEARCH: &[&str] = &[{:?}];\nconst SENTINEL: &str = {:?};\n\
        const REFERENCE: &str = {:?};\nconst SOLUTION: &[(&str, &str)] = &[];\nconst ROOT: &str = {:?};\nconst BIN: &str = {BIN:?};\n",
        quoted(&reads), quoted(std::slice::from_ref(&checks)), lab.project.canonicalize().unwrap().to_str().unwrap(), fs::read_to_string(&hidden).unwrap(),
        record["reference_oid"].as_str().unwrap(), lab.root().canonicalize().unwrap().to_str().unwrap()));
    lab.prepare_profile();
    let binding = lab.state().runtime_bindings.iter().find(|b| b.task.as_ref().is_some_and(|t| t.as_str() == "work")).unwrap().id.clone();
    let attempt = lab.reserve(&lab.selection("work", &binding, &source, None));
    let worktree = PathBuf::from(lab.ok(&["memory", "demo", "attempt-input", "--attempt", attempt.as_str()])["worktrees"][0]["path"].as_str().unwrap());
    lab.serve();
    let mut ticker = lab.spawn();
    lab.wait(&mut ticker, 180, &|| worktree.join("probe-1.txt").exists());
    let report = fs::read_to_string(worktree.join("probe-1.txt")).unwrap();
    assert!(report.contains(&format!("read {} OK", source.join(".git/HEAD").display())), "{report}");
    assert!(report.contains(&format!("read {} ERR:", hidden.display())), "{report}");
    let argv = launched_commands(&lab);
    assert_eq!(argv.len(), 1);
    for hide in [format!("hide:{}", source.display()), format!("hide:{}", checks.display())] { assert!(!argv[0].contains(&hide), "{hide}: {:?}", argv[0]); }
    lab.ok_live(&|| ["task", "demo", "cancel-attempt", attempt.as_str(), "--expected-revision", &lab.state().attempts.iter().find(|a| a.id == attempt).unwrap().revision.to_string(),
        "--expected-head", &lab.head().to_string(), "--reason", "probe done"].map(String::from).to_vec());
    lab.wait(&mut ticker, 60, &|| lab.state().attempts.iter().find(|a| a.id == attempt).unwrap().termination_observed);
    lab.stop(ticker);
}

/// Budget and authority are those of ordinary work. `replay run` writes
/// only ordinary tasks and queue entries (no approval, attempt, reservation or
/// operation). A replay contract installs only with the owner's signature.
/// Under a profile budget the brief exceeds, `launch draft` refuses the replay
/// task with exactly the ordinary task's refusal, before any approval.
#[test]
fn replay_work_has_the_budget_and_authority_of_ordinary_work() {
    let mut lab = Lab::new("soft_input_tokens=500\nunknown_usage='allow_with_warning'");
    lab.build_history();
    lab.replay(&["extract", "--suite", "v1"]);
    let before = lab.state();
    let run = lab.replay(&["run", "--suite", "v1", "--configuration", "budget", "--subset", "stratified:1", "--seed", "gamma", "--expected-head", &lab.head().to_string()]);
    let task = run["tasks"][0]["task_id"].as_str().unwrap().to_owned();
    let after = lab.state();
    assert_eq!((&after.approvals, &after.attempts, &after.operations, &after.deliveries, &after.ownership), (&before.approvals, &before.attempts, &before.operations, &before.deliveries, &before.ownership));
    let added: Vec<&str> = after.tasks.iter().filter(|t| !before.tasks.iter().any(|b| b.id == t.id)).map(|t| t.id.as_str()).collect();
    assert_eq!(added, [task.as_str()]);
    // Unsigned, or signed by another key: refused, nothing installed.
    let document = lab.path("replay-contract.json");
    lab.replay(&["contract", &task, "--output", document.to_str().unwrap()]);
    let stranger = lab.path("stranger");
    assert!(Command::new("/usr/bin/ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&stranger).output().unwrap().status.success());
    sign(&stranger, authority::CONTRACT_SIGNATURE_NAMESPACE, &document);
    lab.fail(&["task", "demo", "contract", "put", "--input-file", document.to_str().unwrap(), "--signature", document.with_extension("json.sig").to_str().unwrap()]);
    assert_eq!(lab.db().query_row("SELECT count(*) FROM task_contracts WHERE task_id=?1", [&task], |r| r.get::<_, i64>(0)).unwrap(), 0);
    lab.install_replay_contract(&task);
    // Same budget refusal as the ordinary task, before any approval.
    let repository = PathBuf::from(run["tasks"][0]["repository"].as_str().unwrap());
    let binding = lab.bind(&task, &repository);
    lab.observe();
    lab.prepare_profile();
    let draft = |selection: &Path| lab.fail(&["launch", "demo", "draft", "--selection", selection.to_str().unwrap(), "--expected-head", &lab.head().to_string()]);
    let replay_refusal = draft(&lab.selection(&task, &binding, &repository, Some("replay")));
    let work_binding = lab.state().runtime_bindings.iter().find(|b| b.task.as_ref().is_some_and(|t| t.as_str() == "work")).unwrap().id.clone();
    let work_refusal = draft(&lab.selection("work", &work_binding, &lab.repo.canonicalize().unwrap(), None));
    for refusal in [&replay_refusal, &work_refusal] { assert!(refusal.contains("exceeding the captured budget"), "{refusal}"); }
    assert!(lab.state().approvals.is_empty() && lab.state().attempts.iter().all(|a| a.task.as_str() != task));
}

/// Suite v1 report comparing two configurations on the same reproducible
/// subset (all three eligible cases): configuration `alpha`'s fake agent
/// writes each case's solution, `beta`'s writes a wrong file. Each replay
/// task's attempt is dispatched with its configuration (planted as the
/// reservation writes it), submits through `result submit` and is verified
/// by its hidden check. By hand: alpha 3/3 (code 2/2, docs 1/1), beta 0/3;
/// excluded: one contaminated case, one source without a hidden check.
/// `telemetry query M49` serves the latest suite's pooled 3/6 with the
/// per-configuration cells, and the registry reports M49 active.
#[test]
fn report_compares_two_configurations_with_suite_version() {
    let mut lab = Lab::new("unknown_usage='allow_with_warning'");
    lab.build_history();
    lab.replay(&["extract", "--suite", "v1"]);
    // Two content-addressed agent configurations, as the dispatch log records them.
    let mut configurations = BTreeMap::new();
    for name in ["alpha", "beta"] {
        let canonical = json!({"schema": "agent_configuration.v1", "agent_digest": format!("{:x}", Sha256::digest(name.as_bytes())), "kind": "claude"}).to_string();
        let id = format!("sha256:{:x}", Sha256::digest(canonical.as_bytes()));
        lab.db().execute("INSERT INTO agent_configurations(configuration_id,canonical_json,first_decided_unix_ms) VALUES(?1,?2,1)", rusqlite::params![id, canonical]).unwrap();
        configurations.insert(name, id);
    }
    for name in ["alpha", "beta"] {
        let run = lab.replay(&["run", "--suite", "v1", "--configuration", name, "--subset", "stratified:3", "--seed", "compare", "--expected-head", &lab.head().to_string()]);
        assert_eq!(run["cases"].as_array().unwrap().len(), 3);
        for (index, planned) in run["tasks"].as_array().unwrap().iter().enumerate() {
            let task = planned["task_id"].as_str().unwrap();
            let case = planned["case_id"].as_str().unwrap();
            let repository = PathBuf::from(planned["repository"].as_str().unwrap());
            let base = planned["base_oid"].as_str().unwrap();
            let (digest, document) = lab.install_replay_contract(task);
            let attempt = format!("{task}-attempt");
            lab.plant_attempt(task, &attempt);
            lab.db().execute("INSERT INTO dispatch_decisions(attempt_id,task_id,task_revision,contract_revision,chosen_configuration_id,eligible,chooser_kind,chooser_principal,reason_codes,decided_unix_ms)
                VALUES(?1,?2,1,1,?3,json_array(?3),'operator','operator:cli','[\"replay\"]',?4)", rusqlite::params![attempt, task, configurations[name], 1_000 + index as i64]).unwrap();
            let files = match name { "alpha" => lab.solution(case), _ => lab.solution(case).keys().map(|k| (k.clone(), "a different greeting entirely\n".to_owned())).collect() };
            let candidate = lab.commit_files(&repository, &format!("{name}-{index}"), base, &files);
            let outputs: Vec<String> = files.keys().cloned().collect();
            let submission = lab.submit(task, &attempt, &digest, &repository, base, &candidate, &outputs.iter().map(String::as_str).collect::<Vec<_>>(), &format!("{task}-submit"));
            for policy in document["acceptance_policies"].as_array().unwrap() {
                let verdict = lab.verify(&submission, policy["id"].as_str().unwrap(), policy["text"].as_str().unwrap(), &format!("{task}-{}", policy["id"].as_str().unwrap()));
                assert_eq!(verdict["state"], if name == "alpha" { "accepted" } else { "rejected" }, "{verdict}");
            }
            lab.end_attempt(&attempt);
        }
    }
    let report = lab.replay(&["report", "--suite", "v1"]);
    let cell = |id: &str, labels: &str, p: u64, n: u64, code: Value, docs: Value| json!({"configuration_id": id, "labels": [labels], "suite_version": "v1",
        "numerator": p, "denominator": n, "value": format!("{p}/{n}"), "n": n, "failed": n - p, "pending": 0, "by_stratum": {"code": code, "docs": docs}});
    let mut expected = vec![cell(&configurations["alpha"], "alpha", 3, 3, json!({"numerator": 2, "denominator": 2, "value": "2/2"}), json!({"numerator": 1, "denominator": 1, "value": "1/1"})),
        cell(&configurations["beta"], "beta", 0, 3, json!({"numerator": 0, "denominator": 2, "value": "0/2"}), json!({"numerator": 0, "denominator": 1, "value": "0/1"}))];
    expected.sort_by_key(|c| c["configuration_id"].as_str().unwrap().to_owned());
    assert_eq!(report["configurations"], json!(expected));
    assert_eq!((&report["suite_version"], &report["metric"], &report["definition"]), (&json!("v1"), &json!("M49"), &json!("M49.v1")));
    assert_eq!(report["exclusions"]["extraction"], json!({"sources": 5, "no_hidden_check": 1, "no_solution_paths": 0, "hidden_check_too_large": 0, "contaminated": 1}));
    assert_eq!((&report["exclusions"]["contaminated_cases"], &report["cases"]), (&json!(1), &json!({"recorded": 4, "eligible": 3, "retired": 0})));
    assert_eq!(report["uncertainty"]["method"], "raw_with_n");
    // M49 through the registry and the query service.
    let registry = lab.ok(&["telemetry", "demo", "metrics", "registry", "--json"]);
    let m49 = registry["metrics"].as_array().unwrap().iter().find(|m| m["id"] == "M49").unwrap().clone();
    assert_eq!((&m49["active"], &m49["definition"]), (&json!(true), &json!("M49.v1")));
    let query = lab.ok(&["telemetry", "demo", "query", "--metric", "M49", "--json"]);
    let text = query.to_string();
    assert!(text.contains(r#""value":"3/6""#) && text.contains(&configurations["alpha"]) && text.contains(r#""suite_version":"v1""#), "{query}");
    // Lifecycle metrics leave replay candidates to M49. By hand: the five source tasks are accepted (integrated),
    // each with one attempt; `work` is open; the six replay candidates (alpha's three verified, beta's three
    // rejected) are excluded. Terminal cohort: M01 = 5, M02 = 5/5, M07 = 5 attempts / 5 accepted.
    let lifecycle = |metric: &str| lab.ok(&["telemetry", "demo", "query", "--metric", metric, "--json"])["results"][0].clone();
    let exclusions = json!({"open": 1, "replay_candidate": 6});
    for (metric, numerator, denominator, value) in [("M01", json!(5), Value::Null, json!(5)), ("M02", json!(5), json!(5), json!("5/5")), ("M07", json!(5), json!(5), json!("5/5"))] {
        let m = lifecycle(metric);
        assert_eq!((&m["numerator"], &m["denominator"], &m["value"], &m["exclusions"]), (&numerator, &denominator, &value, &exclusions), "{metric}: {m}");
    }
    // M06 has no replay sample either (the source attempts carry no admission time).
    assert_eq!(lifecycle("M06")["exclusions"], exclusions);
    let drill = lab.ok(&["telemetry", "demo", "query", "--metric", "M02", "--json", "--drill", "excluded.replay_candidate"])["drill"]["rows"].clone();
    let mut ids: Vec<&str> = drill.as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect();
    ids.sort();
    assert_eq!(ids, ["replay-v1-2-1", "replay-v1-2-2", "replay-v1-2-3", "replay-v1-3-1", "replay-v1-3-2", "replay-v1-3-3"]);
    // The central report's T/A slice agrees.
    let report = lab.ok(&["telemetry", "demo", "report", "--json"]);
    assert_eq!(report["tasks"], json!({"accepted": 5, "open": 1, "succeeded_without_evidence": 0, "terminal": 5, "replay_candidates": 6}));
    let (m02, m07) = (&report["metrics"]["M02"], &report["metrics"]["M07"]);
    assert_eq!((&m02["value"], &m02["excluded"]), (&json!("5/5"), &json!({"open": 1, "outside_window": 0, "replay_candidate": 6})));
    assert_eq!((&m07["value"], &m07["excluded"]), (&json!("5/5"), &json!({"replay_candidate": 6})));
    assert_eq!(report["metrics"]["M49"]["value"], json!("3/6"));
}

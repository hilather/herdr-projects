//! TM3.5 quality and attribution certification (plan doc 10 §5, doc 06 §7,
//! docs/telemetry/certificate-quality.md). Independent of the lane tests:
//! every expected value below is computed by hand from the plan's metric
//! definitions (doc 07 M20–M29, M41–M48) and written into the test before
//! the product ran, never read back from a product aggregate.
//!
//! Everything runs end to end through the compiled CLI on a disposable
//! project with an owner signing key and a real SHA-256 Git repository:
//! signed task contracts, `result submit`, real isolated `result verify`
//! runs whose policies really pass or fail on the candidate's content, real
//! `result integrate` merges, real regression commits, the review, triage,
//! fix, seed and candidate-group commands. Only launch records that need a
//! live agent (reviewer and repair attempts, their dispatch decisions) are
//! planted, as the canonical launch would write them; the sandboxed worker
//! scenario uses the real isolated launch and spool path instead.
//!
//! These certify the quality views on fixtures (`certified-fixture`). They
//! say nothing about real producers (a live reviewer, live repairs, real CI):
//! that is the separate producer certificate (plan doc 10 §7, TM5.2).

#![cfg(all(feature = "state-store", target_os = "linux"))]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.

mod support;

use herdr_farm::{domain::agent_configuration, store::{FindingTarget, SqliteStore, TriageOutcome, TriageRequest}};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{fs, path::{Path, PathBuf}, process::{Command, Output}};
use support::telemetry::{codex_profile, plant_profile, unix_ms, BIN};

/// A weak acceptance policy: any committed candidate passes.
const WEAK: &str = r#"{"version":1,"checks":["/usr/bin/git","diff","--quiet"]}"#;
/// The regression check for the division defect: the candidate's
/// `src/lib.rs` must guard the zero divisor.
const GUARDED: &str = r#"{"version":1,"checks":["/usr/bin/git","grep","--quiet","-F","b == 0","--","src/lib.rs"]}"#;
/// The regression check for the overflow defect.
const CHECKED: &str = r#"{"version":1,"checks":["/usr/bin/git","grep","--quiet","-F","checked_mul","--","src/lib.rs"]}"#;

/// The clean base: both functions are safe.
const BASE_SRC: &str = "pub fn ratio(a: u32, b: u32) -> u32 {\n    if b == 0 { return 0; }\n    a / b\n}\n\npub fn percent(a: u32) -> Option<u32> {\n    a.checked_mul(100)\n}\n";
/// The author's refactor introduces both known defects: an unguarded
/// division and an overflowing multiplication.
const DEFECT_SRC: &str = "pub fn ratio(a: u32, b: u32) -> u32 {\n    a / b\n}\n\npub fn percent(a: u32) -> u32 {\n    a * 100\n}\n";

fn evidence(c: char) -> String { format!("sha256:{}", c.to_string().repeat(64)) }

/// A disposable real project: owner key, active project `demo`, a SHA-256
/// repository whose base holds `BASE_SRC`, retained native profiles `fast`
/// (A, Codex), `slow` (B, Codex) and `claude` (C, Claude), and task
/// `reviews` that owns the planted reviewer attempts.
struct Lab { home: tempfile::TempDir, root: PathBuf, project: PathBuf, key: PathBuf, repo: PathBuf, store: String, base: String, a: String, b: String, c: String }

impl Lab {
    fn new() -> Self {
        use herdr_farm::{domain::ProjectState, migration, runtime};
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        let run = |args: &[&str]| Command::new(BIN).env_clear().env("HOME", home.path()).args(args).output().unwrap();
        for action in ["new", "pause"] { assert!(run(&["--root", root.to_str().unwrap(), action, "demo"]).status.success()); }
        let key = home.path().join("owner");
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&key).output().unwrap().status.success());
        let public = fs::read_to_string(key.with_extension("pub")).unwrap().split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        let project = root.join("demo");
        let config = home.path().join(".config/herdr-farm/config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, format!("[authority]\nversion=1\nrevision=1\napproval_public_key={public:?}\n[profiles.worker]\nkind='claude'\npermission_policy='interactive'\n[profiles.worker.budget]\nmax_wall_seconds=60\nunknown_usage='allow_with_warning'\n")).unwrap();
        let plan = migration::inspect_with_config(&project, &config).unwrap();
        migration::apply(&project, &plan, true).unwrap();
        let s = runtime::snapshot(&project).unwrap();
        runtime::set_state(&project, s.head, s.control.unwrap().revision, ProjectState::Active, &config).unwrap();
        let store = project.join(".state/state.db").canonicalize().unwrap().display().to_string();
        let repo = home.path().join("repo");
        fs::create_dir_all(repo.join("src")).unwrap();
        let mut lab = Lab { home, root, project, key, repo, store, base: String::new(), a: String::new(), b: String::new(), c: String::new() };
        lab.git(&["init", "-q", "--object-format=sha256"]);
        fs::write(lab.repo.join("src/lib.rs"), BASE_SRC).unwrap();
        lab.git(&["add", "."]);
        lab.git(&["commit", "-qm", "base"]);
        lab.base = lab.git(&["rev-parse", "HEAD"]);
        lab.git(&["branch", "integration", &lab.base]);
        let reference = migration::config_reference(&config).unwrap();
        let db_path = lab.project.join(".state/state.db");
        let mut fast = codex_profile(&reference, "codex", "fast", Some(&lab.home.path().join("fast-home")));
        fast.arguments_digest = "1".repeat(64);
        let mut slow = codex_profile(&reference, "codex", "slow", Some(&lab.home.path().join("slow-home")));
        slow.arguments_digest = "2".repeat(64);
        let claude = codex_profile(&reference, "claude", "claude", Some(&lab.home.path().join("claude-home")));
        (lab.a, lab.b, lab.c) = (agent_configuration(&fast).id, agent_configuration(&slow).id, agent_configuration(&claude).id);
        for profile in [&fast, &slow, &claude] {
            let c = agent_configuration(profile);
            rusqlite::Connection::open(&db_path).unwrap().execute("INSERT OR IGNORE INTO agent_configurations VALUES(?1,?2,1)", rusqlite::params![c.id, c.canonical_json]).unwrap();
        }
        plant_profile(&db_path, fast);
        plant_profile(&db_path, slow);
        plant_profile(&db_path, claude);
        let head = runtime::snapshot(&lab.project).unwrap().head;
        runtime::add_task(&lab.project, herdr_farm::domain::TaskId::new("reviews").unwrap(), "reviews".into(), head).unwrap();
        lab
    }
    fn hp(&self, args: &[&str]) -> Output {
        Command::new(BIN).env_clear().env("HOME", self.home.path()).env("PATH", "/usr/bin:/bin").arg("--root").arg(&self.root).args(args).output().unwrap()
    }
    fn ok(&self, args: &[&str]) -> Value {
        let out = self.hp(args);
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
    }
    fn fail(&self, args: &[&str]) -> String {
        let out = self.hp(args);
        assert!(!out.status.success(), "{args:?} succeeded: {}", String::from_utf8_lossy(&out.stdout));
        String::from_utf8_lossy(&out.stderr).into_owned()
    }
    /// `telemetry demo ARGS`.
    fn t(&self, args: &[&str]) -> Value { self.ok(&[&["telemetry", "demo"][..], args].concat()) }
    /// `telemetry demo ARGS`, which must fail and must contain `text`.
    fn refused(&self, args: &[&str], text: &str) {
        let before = self.ledger_head();
        let err = self.fail(&[&["telemetry", "demo"][..], args].concat());
        assert!(err.contains(text), "{args:?}: {err}");
        assert_eq!(self.ledger_head(), before, "{args:?} was refused but moved the ledger");
    }
    fn db(&self) -> rusqlite::Connection { rusqlite::Connection::open(self.project.join(".state/state.db")).unwrap() }
    fn count(&self, sql: &str) -> i64 { self.db().query_row(sql, [], |r| r.get(0)).unwrap() }
    /// The head of the one review/triage/fix/seed ordering.
    fn ledger_head(&self) -> i64 { self.t(&["review", "findings", "show"])["findings"]["head_seq"].as_i64().unwrap() }
    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("/usr/bin/git").env_clear().env("PATH", "/usr/bin:/bin").env("HOME", self.home.path()).env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "fixture").env("GIT_AUTHOR_EMAIL", "fixture@example.com").env("GIT_COMMITTER_NAME", "fixture").env("GIT_COMMITTER_EMAIL", "fixture@example.com")
            .current_dir(&self.repo).args(args).output().unwrap();
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }
    /// Commit `source` as `src/lib.rs` on branch `branch` started at `from`; returns the commit.
    fn commit(&self, branch: &str, from: &str, source: &str) -> String {
        self.git(&["checkout", "-qB", branch, from]);
        fs::write(self.repo.join("src/lib.rs"), source).unwrap();
        self.git(&["add", "."]);
        self.git(&["commit", "-qm", branch]);
        let oid = self.git(&["rev-parse", "HEAD"]);
        self.git(&["checkout", "-q", "--detach"]);
        oid
    }
    /// A signed verify-then-integrate contract (revision 1) for new task
    /// `task` at `base` with acceptance policy `policy`; returns its digest.
    fn contract(&self, task: &str, base: &str, policy: &str) -> String {
        use herdr_farm::{authority::CONTRACT_SIGNATURE_NAMESPACE, domain::TaskId, runtime};
        let head = runtime::add_task(&self.project, TaskId::new(task).unwrap(), task.into(), runtime::snapshot(&self.project).unwrap().head).unwrap();
        let repository = self.repo.canonicalize().unwrap().display().to_string();
        let mut document = serde_json::to_vec_pretty(&json!({
            "version": 3, "outputs": [{"path": "src/lib.rs", "kind": "git_file"}], "scope": {"paths": [{"path": "src/", "access": "write"}]},
            "project_store": self.store, "expected_head": head, "task_id": task, "contract_revision": 1, "deliverable": "ship", "non_goals": "no launch",
            "acceptance_policies": [{"id": "clean", "text": policy}], "repository": repository, "base_oid": base, "object_format": "sha256",
            "dependencies": [], "capability_flags": [], "profile_kind": "codex", "retry_class": "none", "result_schema_id": "result-v1",
            "route": "verify_then_integrate", "authority": herdr_farm::authority::policy_reference(&self.project).unwrap()})).unwrap();
        document.push(b'\n');
        let doc = self.home.path().join(format!("{task}-contract.json"));
        fs::write(&doc, &document).unwrap();
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-Y", "sign", "-f"]).arg(&self.key).args(["-n", CONTRACT_SIGNATURE_NAMESPACE]).arg(&doc).status().unwrap().success());
        self.ok(&["task", "demo", "contract", "put", "--input-file", doc.to_str().unwrap(), "--signature", doc.with_extension("json.sig").to_str().unwrap()])["digest"].as_str().unwrap().to_owned()
    }
    /// A running attempt of `task` as a launch writes it, with its dispatch
    /// decision when `configuration` is known.
    fn attempt(&self, task: &str, attempt: &str, configuration: Option<&str>) {
        let db = self.db();
        db.execute("INSERT INTO attempts(id,task_id,revision,state,snapshot,reservation,termination_observed) VALUES(?1,?2,1,'running',NULL,?1,0)", [attempt, task]).unwrap();
        if let Some(configuration) = configuration {
            db.execute("INSERT INTO dispatch_decisions(attempt_id,task_id,task_revision,contract_revision,chosen_configuration_id,eligible,chooser_kind,chooser_principal,reason_codes,decided_unix_ms)
                VALUES(?1,?2,1,1,?3,'[\"x\"]','operator','operator:cli','[\"x\"]',1)", rusqlite::params![attempt, task, configuration]).unwrap();
        }
    }
    /// `result submit` of `attempt`'s candidate for `task`; returns the submission id.
    fn result(&self, task: &str, attempt: &str, digest: &str, base: &str, candidate: &str, claimed: &[&str]) -> String {
        let repository = self.repo.canonicalize().unwrap().display().to_string();
        let objects: Vec<Value> = self.git(&["rev-list", "--objects", "--all"]).lines()
            .map(|line| { let oid = line.split_whitespace().next().unwrap(); json!({"oid": oid, "relative_path": format!("{}/{}", &oid[..2], &oid[2..])}) }).collect();
        let key = format!("{attempt}-{}", &candidate[..12]);
        let path = self.home.path().join(format!("{key}-result.json"));
        fs::write(&path, json!({"idempotency_key": key, "task_id": task, "contract_revision": 1, "contract_digest": digest,
            "attempt_id": attempt, "repository": repository, "base_oid": base, "candidate_oid": candidate, "object_format": "sha256",
            "artifact_manifest": [{"path": "src/lib.rs", "oid": candidate}], "claimed_checks": claimed, "objects": objects}).to_string()).unwrap();
        self.ok(&["result", "demo", "submit", "--input-file", path.to_str().unwrap()])["submission_id"].as_str().unwrap().to_owned()
    }
    /// A real isolated verification of `submission` under `policy`: (state, run id, verified result id).
    fn verify(&self, submission: &str, key: &str, policy: &str) -> (String, String, Option<String>) {
        let file = self.home.path().join(format!("{key}-policy.json"));
        fs::write(&file, policy).unwrap();
        let work = self.home.path().join(format!("{key}-work"));
        let out = self.hp(&["result", "demo", "verify", submission, "--policy-id", "clean", "--policy-file", file.to_str().unwrap(), "--idempotency-key", key,
            "--work-dir", work.to_str().unwrap(), "--timeout-seconds", "60"]);
        // A rejected run is recorded and the command fails with its reason.
        assert!(out.status.success() || String::from_utf8_lossy(&out.stderr).contains("verification rejected: checks_failed"), "{}", String::from_utf8_lossy(&out.stderr));
        let (run, state): (String, String) = self.db().query_row("SELECT run_id,state FROM verification_runs WHERE idempotency_key=?1", [key], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        let result = self.db().query_row("SELECT result_id FROM verified_results WHERE run_id=?1", [&run], |r| r.get(0)).ok();
        (state, run, result)
    }
    /// A real `result integrate` of verified result `result` onto
    /// `refs/heads/integration`; returns the `integrated_commits` id and commit.
    fn integrate(&self, result: &str, key: &str) -> (String, String) {
        let work = self.home.path().join(format!("{key}-work"));
        let out = self.ok(&["result", "demo", "integrate", result, "--repository", self.repo.to_str().unwrap(), "--idempotency-key", key, "--work-dir", work.to_str().unwrap()]);
        assert_eq!(out["state"], json!("integrated"), "{out}");
        let operation = out["operation_id"].as_str().unwrap();
        self.db().query_row("SELECT integrated_id,commit_oid FROM integrated_commits WHERE operation_id=?1", [operation], |r| Ok((r.get(0)?, r.get(1)?))).unwrap()
    }
    fn configure_integration(&self) {
        self.ok(&["result", "demo", "configure-integration", "--repository", self.repo.to_str().unwrap(), "--reference", "refs/heads/integration"]);
    }
    /// Open, assign (`--reviewer P`) and start a review of `submission` by
    /// planted reviewer attempt `attempt` (configuration `configuration`).
    fn review(&self, submission: &str, kind: &str, reviewer: &str, attempt: &str, configuration: &str) -> (String, String) {
        let opportunity = self.t(&["review", "open", submission, "--kind", kind, "--protocol", "review-protocol.v1"])["opportunity"]["opportunity_id"].as_str().unwrap().to_owned();
        self.t(&["review", "assign", &opportunity, "--reviewer", reviewer]);
        self.attempt("reviews", attempt, Some(configuration));
        let session = self.t(&["review", "start", &opportunity, "--attempt", attempt])["session"]["session_id"].as_str().unwrap().to_owned();
        (opportunity, session)
    }
    /// Record `session`'s end through `review complete`; returns the completion.
    fn complete(&self, session: &str, submission: &str, candidate: &str, outcome: &str, findings: Value) -> Value {
        let mut receipt = json!({"schema": "review_receipt.v1", "session_id": session, "submission_id": submission, "candidate_oid": candidate,
            "outcome": outcome, "findings": findings, "evidence": [evidence('e')]});
        if outcome != "completed" { receipt["reason"] = json!("budget_exhausted"); }
        let path = self.home.path().join(format!("receipt-{}.json", &session[7..19]));
        fs::write(&path, receipt.to_string()).unwrap();
        self.t(&["review", "complete", "--input-file", path.to_str().unwrap()])["completion"].clone()
    }
    fn metrics(&self) -> Value { self.t(&["review", "report"])["metrics"].clone() }
    fn findings(&self, as_of: Option<i64>) -> Value { self.view(&["review", "findings", "show"], "findings", as_of) }
    fn fixes(&self, as_of: Option<i64>) -> Value { self.view(&["review", "fixes", "show"], "fixes", as_of) }
    fn view(&self, args: &[&str], key: &str, as_of: Option<i64>) -> Value {
        let seq = as_of.map(|s| s.to_string());
        let mut all = args.to_vec();
        if let Some(seq) = &seq { all.extend(["--as-of", seq.as_str()]); }
        self.t(&all)[key].clone()
    }
    /// `review fixes ARGS`; returns the event.
    fn fix(&self, args: &[&str]) -> Value { self.t(&[&["review", "fixes"][..], args].concat())["event"].clone() }
    /// `review findings ARGS`; returns the event.
    fn triage(&self, args: &[&str]) -> Value { self.t(&[&["review", "findings"][..], args].concat())["event"].clone() }
}

/// Exact fraction text `"n/d"` (or `"n"`) as a reduced (numerator, denominator).
fn frac(text: &str) -> (i64, i64) {
    let int = |t: &str| t.parse::<i64>().unwrap_or_else(|e| panic!("{text:?}: {e}"));
    let (n, d) = text.split_once('/').map_or_else(|| (int(text), 1), |(n, d)| (int(n), int(d)));
    fn gcd(a: i64, b: i64) -> i64 { if b == 0 { a.abs().max(1) } else { gcd(b, a % b) } }
    let g = gcd(n, d);
    (n / g, d / g)
}
fn add((a, b): (i64, i64), (c, d): (i64, i64)) -> (i64, i64) { frac(&format!("{}/{}", a * d + c * b, b * d)) }

/// Attribution reconciles (plan doc 06 §5): per finding and role the shares
/// sum to `allocated` and `allocated + unallocated = 1`; M21 = the sum of
/// the discovery credit of F, `value + unallocated = |F|`, and its
/// per-configuration cells sum to its value.
fn assert_attribution_reconciles(fixes: &Value, metrics: &Value) {
    let mut discovery = (0, 1);
    let mut f = 0;
    for finding in fixes["findings"].as_array().unwrap() {
        for role in ["discovery", "validation", "implementation", "verification", "integration"] {
            let credit = &finding[role];
            if credit.is_null() { continue; }
            let shares = credit["shares"].as_array().unwrap().iter().fold((0, 1), |acc, s| add(acc, frac(s["share"].as_str().unwrap())));
            assert_eq!(shares, frac(credit["allocated"].as_str().unwrap()), "{} {role}", finding["finding_id"]);
            assert_eq!(add(shares, frac(credit["unallocated"].as_str().unwrap())), (1, 1), "{} {role}", finding["finding_id"]);
        }
        if finding["status"] == "validated" && !finding["seeded_evaluation"].as_bool().unwrap() {
            f += 1;
            discovery = add(discovery, frac(finding["discovery"]["allocated"].as_str().unwrap()));
        }
    }
    let m21 = &metrics["M21"];
    assert_eq!(frac(m21["value"].as_str().unwrap()), discovery);
    assert_eq!(add(discovery, frac(m21["unallocated"].as_str().unwrap())), (f, 1));
    let cells = m21["by_configuration"].as_object().unwrap().values().fold((0, 1), |acc, v| add(acc, frac(v.as_str().unwrap())));
    assert_eq!(cells, discovery, "M21 cells sum to its value");
}

/// Rebuild check: every view captured at a watermark is reproduced exactly
/// by replaying the canonical ledger to that watermark, in this project and
/// in a fresh copy of its canonical store opened by a new process (a rebuild
/// from canonical rows only); and the quality metrics a report showed at that
/// watermark are recomputed from the replayed view by the plan's
/// definitions, so no report value comes from anything but canonical
/// decisions.
fn assert_rebuilds(lab: &Lab, captured: &[(i64, Value, Value, Value, Value)]) {
    let fresh = tempfile::tempdir().unwrap();
    let fresh_root = fresh.path().join("root");
    fs::create_dir_all(fresh_root.join("demo/.state")).unwrap();
    // A consistent copy of the canonical store (the WAL folded in by VACUUM INTO).
    lab.db().execute("VACUUM INTO ?1", [fresh_root.join("demo/.state/state.db").to_str().unwrap()]).unwrap();
    let fresh_view = |args: &[&str], key: &str, seq: i64| -> Value {
        let out = Command::new(BIN).env_clear().env("HOME", fresh.path()).env("PATH", "/usr/bin:/bin").arg("--root").arg(&fresh_root)
            .args(["telemetry", "demo"]).args(args).args(["--as-of", &seq.to_string()]).output().unwrap();
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice::<Value>(&out.stdout).unwrap()[key].clone()
    };
    let strip = |mut v: Value| { v.as_object_mut().unwrap().remove("head_seq"); v };
    for (seq, findings, fixes, metrics, shown) in captured {
        let replayed_findings = lab.findings(Some(*seq));
        let replayed_fixes = lab.fixes(Some(*seq));
        assert_eq!(strip(replayed_findings.clone()), strip(findings.clone()), "findings as of {seq}");
        assert_eq!(strip(replayed_fixes.clone()), strip(fixes.clone()), "fixes as of {seq}");
        assert_eq!(strip(fresh_view(&["review", "findings", "show"], "findings", *seq)), strip(findings.clone()), "fresh findings as of {seq}");
        assert_eq!(strip(fresh_view(&["review", "fixes", "show"], "fixes", *seq)), strip(fixes.clone()), "fresh fixes as of {seq}");
        // Opportunities, assignments, sessions and completions replay exactly too (0063).
        let replayed_show = lab.t(&["review", "show", "--as-of", &seq.to_string()]);
        let show_body = |mut v: Value| { let o = v.as_object_mut().unwrap(); o.remove("head_seq"); o.remove("as_of_seq"); v };
        assert_eq!(replayed_show["as_of_seq"], json!(seq), "review show as of {seq}");
        assert_eq!(show_body(replayed_show), show_body(shown.clone()), "review show as of {seq}");
        let fresh_show = Command::new(BIN).env_clear().env("HOME", fresh.path()).env("PATH", "/usr/bin:/bin").arg("--root").arg(&fresh_root)
            .args(["telemetry", "demo", "review", "show", "--as-of", &seq.to_string()]).output().unwrap();
        assert!(fresh_show.status.success(), "{}", String::from_utf8_lossy(&fresh_show.stderr));
        assert_eq!(show_body(serde_json::from_slice(&fresh_show.stdout).unwrap()), show_body(shown.clone()), "fresh review show as of {seq}");
        // M22/M23 by the plan: submissions with ≥1 validated claim (resp. duplicate-only) / adjudicated; pending outside.
        let subs: Vec<&Value> = replayed_findings["submissions"].as_array().unwrap().iter().collect();
        let adjudicated = subs.iter().filter(|s| s["outcome"] != "pending").count();
        let validated = subs.iter().filter(|s| s["outcome"] != "pending" && s["claims"].as_array().unwrap().iter().any(|c| c["outcome"] == "validated")).count();
        let duplicate_only = subs.iter().filter(|s| s["outcome"] != "pending" && s["claims"].as_array().unwrap().iter().all(|c| c["outcome"] == "duplicate")).count();
        let ratio = |n: usize, d: usize| if d == 0 { Value::Null } else { json!(format!("{n}/{d}")) };
        assert_eq!((&metrics["M22"]["value"], &metrics["M23"]["value"], &metrics["M22"]["pending"]),
            (&ratio(validated, adjudicated), &ratio(duplicate_only, adjudicated), &json!(subs.len() - adjudicated)), "M22/M23 as of {seq}");
        // M25/M26 finding outcomes: over validated unique findings, ever verified / currently resolved.
        let f: Vec<&Value> = replayed_fixes["findings"].as_array().unwrap().iter().filter(|x| x["status"] == "validated").collect();
        let verified = f.iter().filter(|x| x["verified"] == json!(true)).count();
        let resolved = f.iter().filter(|x| x["currently_resolved"] == json!(true)).count();
        assert_eq!((&metrics["M25"]["value"], &metrics["M26"]["value"]), (&ratio(verified, f.len()), &ratio(resolved, f.len())), "M25/M26 as of {seq}");
        assert_eq!(metrics["M22"]["as_of_seq"], json!(seq));
        assert_attribution_reconciles(&replayed_fixes, metrics);
    }
}

/// Capture the head views (`findings show`, `fixes show`, `review show`) and
/// the report at the current watermark; the report states that watermark.
fn capture(lab: &Lab) -> (i64, Value, Value, Value, Value) {
    let findings = lab.findings(None);
    let head = findings["head_seq"].as_i64().unwrap();
    let report = lab.t(&["review", "report"]);
    assert_eq!((&report["as_of_seq"], &report["metrics"]["M20"]["as_of_seq"], &report["metrics"]["M22"]["as_of_seq"]), (&json!(head), &json!(head), &json!(head)),
        "one report, one watermark");
    (head, findings, lab.fixes(None), report["metrics"].clone(), lab.t(&["review", "show"]))
}

const D0_SRC: &str = "pub fn ratio(a: u32, b: u32) -> u32 {\n    a.saturating_div(b)\n}\n\npub fn percent(a: u32) -> u32 {\n    a * 100\n}\n";
const D1_SRC: &str = "pub fn ratio(a: u32, b: u32) -> u32 {\n    if b == 0 { return 0; }\n    a / b\n}\n\npub fn percent(a: u32) -> u32 {\n    a * 100\n}\n";
const D2_SRC: &str = "pub fn ratio(a: u32, b: u32) -> u32 {\n    a / b.max(1)\n}\n\npub fn percent(a: u32) -> u32 {\n    a * 100\n}\n";
const V1_SRC: &str = "pub fn ratio(a: u32, b: u32) -> u32 {\n    if b == 0 { return 0; }\n    a / b\n}\n\npub fn percent(a: u32) -> Option<u32> {\n    a.checked_mul(100)\n}\n";

/// Doc 10 §5 on a real repository: known defects, reports, partial and
/// empty reviews, failed and verified repairs, a verified-but-unintegrated
/// fix, a real regression and a reopen cycle, attribution, and the rebuild
/// check. The one ordering, by hand (each review's opening, assignment,
/// session start and completion take a seq, then its submissions):
///
/// | seq | row                                                                  |
/// |-----|----------------------------------------------------------------------|
/// | 1–6 | O1 code by rev-1 (C): open, assign, start, complete, subs 1 `div`, 2 `overflow` |
/// | 7–11 | O2 security by rev-2 (B): open, assign, start, complete, sub 3 `zero` (same defect)|
/// | 12–15 | O3 security by rev-3 (A): open, assign, start, completed with zero findings |
/// | 16–20 | O4 test by rev-4 (A): open, assign, start, timed out, sub 4 `partial` |
/// | 21  | O5 opened (never assigned)                                           |
/// | 22–23 | O6 opened, assigned (no session)                                   |
/// | 24  | claim 1 validated: D = `finding:canonical-24` (high)                 |
/// | 25  | claim 2 validated: V = `finding:canonical-25` (medium)               |
/// | 26  | claim 3 validated as D: a derived duplicate                          |
/// | 27  | claim 4 rejected                                                     |
/// | 28–29 | repair 28 of D assigned A; d-a1 (A) bound                          |
/// | 30  | D2 proposed (branch changed after D1 passed; D2's run rejected)      |
/// | 31–34 | D1 proposed, verified by its own run, integrated (I2), closed fixed|
/// | 35–37 | repair 35 of V assigned A; v-a1 (A) bound, fails; v-b1 (B) bound   |
/// | 38–40 | V1 proposed, verified, closed fixed; never integrated              |
/// | 41  | D's introduction: author x-attempt at X1, reliable bisect            |
/// | 42  | D reopened: real regression commit R on the integration branch       |
/// | 43  | repair 43 of D, unassigned (a new cycle, censored)                   |
/// | 44  | V's mixed implementation credit split v-b1 2/3, v-a1 1/3             |
/// | 45  | D's discovery shared rev-1 1/2, rev-2 1/2                            |
/// | 46  | 45 retracted                                                         |
///
/// Hand-computed from doc 07: M20 = 3/5 (O1, O2, O3 completed of five
/// assigned; O4 timed out, O6 no session, O5 unassigned outside); M22 =
/// 2/4, M23 = 1/4 (subs 1, 2 validated-only, 3 duplicate-only, 4
/// rejected-only); unique findings 2, M21 = 2 (C 2) with participation 2.
/// At 34: M25 = M26 = 1/2, A's cohort 1/1. At 40: M25 = 2/2, M26 = 1/2, A's
/// cohort M25 2/2 and M26 1/2 (reassigned 1), B null (no assigned
/// opportunity); M29 = 8/11. At 42: M26 = 0/2 and A's 0/2, M25 unchanged,
/// M27 = 1/1, M29 = 9/11. At 44: M29 = 10/11. At 45: M21 = 2 with B 1/2, C
/// 3/2, participation 3; at 46 back to C 2. Every captured `review show`
/// replays exactly with `--as-of` (O5 and O6 appear only from 21 and 22).
#[test]
fn repair_lifecycle_on_a_real_repository_matches_hand_computed_views() {
    let lab = Lab::new();
    lab.configure_integration();
    let (a, b, c) = (lab.a.clone(), lab.b.clone(), lab.c.clone());
    let unavailable = |reason: &str| json!({"status": "unavailable", "reason": reason});

    // The author's candidate X1 introduces both defects; the weak policy passes it and it integrates.
    let dx = lab.contract("x", &lab.base, WEAK);
    lab.attempt("x", "x-attempt", Some(&a));
    let x1 = lab.commit("x1", &lab.base, DEFECT_SRC);
    let sx = lab.result("x", "x-attempt", &dx, &lab.base, &x1, &["all checks passed", "no division by zero possible"]);
    let (state, _, rx) = lab.verify(&sx, "verify-x", WEAK);
    assert_eq!(state, "accepted");
    let (ix, _) = lab.integrate(&rx.unwrap(), "integrate-x");
    let t1 = lab.git(&["rev-parse", "refs/heads/integration"]);

    // Reviews of the exact candidate X1.
    let (o1, s1) = lab.review(&sx, "code", "claude", "rev-1", &c);
    // A receipt naming another candidate is refused and writes nothing.
    let wrong = lab.home.path().join("wrong.json");
    fs::write(&wrong, json!({"schema": "review_receipt.v1", "session_id": s1, "submission_id": sx, "candidate_oid": t1, "outcome": "completed", "findings": [], "evidence": []}).to_string()).unwrap();
    lab.refused(&["review", "complete", "--input-file", wrong.to_str().unwrap()], "names another candidate");
    let done = lab.complete(&s1, &sx, &x1, "completed", json!([{"ref": "finding:div", "title": "Division by zero when b is 0"}, {"ref": "finding:overflow", "title": "percent overflows above u32::MAX/100"}]));
    assert_eq!((&done["finding_submissions"], &done["trust"], &done["coverage_basis"]), (&json!([1, 2]), &json!("proposal"), &json!("declared")));
    let (o2, s2) = lab.review(&sx, "security", "slow", "rev-2", &b);
    assert_eq!(lab.complete(&s2, &sx, &x1, "completed", json!([{"ref": "finding:zero", "title": "ratio panics on a zero divisor"}]))["finding_submissions"], json!([3]));
    let (o3, s3) = lab.review(&sx, "security", "fast", "rev-3", &a);
    assert_eq!(lab.complete(&s3, &sx, &x1, "completed", json!([]))["findings_submitted"], json!(0));
    let (o4, s4) = lab.review(&sx, "test", "fast", "rev-4", &a);
    assert_eq!(lab.complete(&s4, &sx, &x1, "timed_out", json!(["finding:partial"]))["finding_submissions"], json!([4]));
    let o5 = lab.t(&["review", "open", &sx, "--kind", "code", "--protocol", "review-protocol.v1"])["opportunity"]["opportunity_id"].as_str().unwrap().to_owned();
    let o6 = lab.t(&["review", "open", &sx, "--kind", "architecture", "--protocol", "review-protocol.v1"])["opportunity"]["opportunity_id"].as_str().unwrap().to_owned();
    lab.t(&["review", "assign", &o6, "--reviewer", "fast"]);
    assert_eq!(lab.ledger_head(), 23);

    // Zero is a real zero only for a completed review of a recorded opportunity; missing review data is unavailable.
    let shown = lab.t(&["review", "show"]);
    let by_id = |id: &str| shown["opportunities"].as_array().unwrap().iter().find(|o| o["opportunity_id"] == id).unwrap().clone();
    assert_eq!((&by_id(&o3)["status"], &by_id(&o3)["findings_submitted"], &by_id(&o3)["kind"], &by_id(&o3)["candidate_oid"]), (&json!("completed"), &json!(0), &json!("security"), &json!(x1)));
    assert_eq!((&by_id(&o4)["status"], &by_id(&o4)["findings_submitted"]), (&json!("ended_without_completion"), &unavailable("ended_without_completion")));
    assert_eq!(by_id(&o5)["findings_submitted"], unavailable("unassigned"));
    assert_eq!(by_id(&o6)["findings_submitted"], unavailable("no_session"));
    for o in [&o1, &o2] { assert_eq!(by_id(o)["status"], json!("completed")); }

    // Before triage every report is an assertion: pending, outside every denominator.
    let m = lab.metrics();
    assert_eq!((&m["M20"]["value"], &m["M20"]["unassigned"]), (&json!("3/5"), &json!(1)));
    assert_eq!(m["M20"]["status"], json!({"completed": 3, "completed_empty": 1, "ended_without_completion": 1, "in_progress": 0, "no_session": 1}));
    assert_eq!(m["M20"]["by_kind_protocol"], json!({"architecture/review-protocol.v1": "0/1", "code/review-protocol.v1": "1/1", "security/review-protocol.v1": "2/2", "test/review-protocol.v1": "0/1"}));
    assert_eq!((&m["M22"]["value"], &m["M22"]["pending"], &m["M21"]["value"], &m["M25"]["value"]), (&json!(null), &json!(4), &json!("0"), &json!(null)));
    let mut captured = vec![capture(&lab)];

    // Owner triage.
    assert_eq!(lab.triage(&["validate", "1", "--new", "--severity", "high", "--evidence", &evidence('a')])["subject"]["finding_id"], json!("finding:canonical-24"));
    lab.triage(&["validate", "2", "--new", "--severity", "medium", "--evidence", &evidence('b')]);
    lab.triage(&["validate", "3", "--finding", "finding:canonical-24", "--severity", "high", "--evidence", &evidence('c')]);
    lab.triage(&["reject", "4", "--reason", "insufficient_evidence"]);
    let (d, v) = ("finding:canonical-24", "finding:canonical-25");
    let m = lab.metrics();
    assert_eq!((&m["M22"]["value"], &m["M23"]["value"], &m["M22"]["pending"]), (&json!("2/4"), &json!("1/4"), &json!(0)));
    assert_eq!(m["M22"]["buckets"], json!({"validated_only": 2, "rejected_only": 1, "duplicate_only": 1, "mixed": 0}));
    assert_eq!((&m["M21"]["value"], &m["M21"]["by_configuration"], &m["M21"]["participation"]), (&json!("2"), &json!({c.as_str(): "2"}), &json!(2)));
    assert_eq!(lab.findings(None)["unique_findings"], json!(2));
    captured.push(capture(&lab));

    // Repair of D, assigned to A: a failed candidate, a passing one, then a changed branch.
    let t1_digest = lab.contract("fix-d", &t1, GUARDED);
    lab.attempt("fix-d", "d-a1", Some(&a));
    assert_eq!(lab.fix(&["open", d, "--assign", "fast"])["seq"], json!(28));
    lab.fix(&["bind", "28", "--attempt", "d-a1"]);
    let d0 = lab.commit("d0", &t1, D0_SRC);
    let sd0 = lab.result("fix-d", "d-a1", &t1_digest, &t1, &d0, &["fixed"]);
    assert_eq!(lab.verify(&sd0, "verify-d0", GUARDED).0, "rejected", "the regression check fails on the wrong fix");
    let d1 = lab.commit("d1", &t1, D1_SRC);
    let sd1 = lab.result("fix-d", "d-a1", &t1_digest, &t1, &d1, &["fixed"]);
    let (state, rd1, rd1_result) = lab.verify(&sd1, "verify-d1", GUARDED);
    assert_eq!(state, "accepted");
    let d2 = lab.commit("d2", &d1, D2_SRC);
    let sd2 = lab.result("fix-d", "d-a1", &t1_digest, &t1, &d2, &["fixed", "still passes"]);
    let (state, rd2, _) = lab.verify(&sd2, "verify-d2", GUARDED);
    assert_eq!(state, "rejected");
    assert_eq!(lab.fix(&["propose", "28", "--submission", &sd2])["seq"], json!(30));
    // D1's passing run never verifies the changed branch D2; D2's own run failed.
    lab.refused(&["review", "fixes", "verify", "30", "--run", &rd1, "--assurance", "regression_reproduced", "--evidence", &evidence('d')], "passing checks on one commit cannot verify another");
    lab.refused(&["review", "fixes", "verify", "30", "--run", &rd2, "--assurance", "regression_reproduced", "--evidence", &evidence('d')], "was rejected");
    assert_eq!(lab.fix(&["propose", "28", "--submission", &sd1])["seq"], json!(31));
    lab.fix(&["verify", "31", "--run", &rd1, "--assurance", "regression_reproduced", "--evidence", &evidence('d')]);
    lab.refused(&["review", "fixes", "integrate", "31", "--integrated", &ix], "did not integrate the fix's exact verified candidate");
    let (i2, _) = lab.integrate(&rd1_result.unwrap(), "integrate-d1");
    let t2 = lab.git(&["rev-parse", "refs/heads/integration"]);
    lab.fix(&["integrate", "31", "--integrated", &i2]);
    assert_eq!(lab.fix(&["close", "28", "--outcome", "fixed"])["seq"], json!(34));
    let m = lab.metrics();
    assert_eq!((&m["M25"]["value"], &m["M26"]["value"]), (&json!("1/2"), &json!("1/2")));
    assert_eq!((&m["M25"]["by_assignment"][a.as_str()]["value"], &m["M26"]["by_assignment"][a.as_str()]["value"]), (&json!("1/1"), &json!("1/1")));
    assert_eq!((&m["M27"]["value"], &m["M27"]["censored"]), (&json!(null), &json!(1)), "an integration younger than the horizon is censored");
    captured.push(capture(&lab));

    // Repair of V, assigned to A: A's attempt fails with no candidate, B's reassignment is verified but never integrated.
    let t2_digest = lab.contract("fix-v", &t2, CHECKED);
    lab.attempt("fix-v", "v-a1", Some(&a));
    lab.attempt("fix-v", "v-b1", Some(&b));
    assert_eq!(lab.fix(&["open", v, "--assign", "fast"])["seq"], json!(35));
    lab.fix(&["bind", "35", "--attempt", "v-a1"]);
    assert_eq!(lab.fix(&["bind", "35", "--attempt", "v-b1"])["subject"]["ordinal"], json!(2));
    let v1 = lab.commit("v1", &t2, V1_SRC);
    let sv1 = lab.result("fix-v", "v-b1", &t2_digest, &t2, &v1, &["fixed"]);
    let (state, rv1, _) = lab.verify(&sv1, "verify-v1", CHECKED);
    assert_eq!(state, "accepted");
    lab.fix(&["propose", "35", "--submission", &sv1]);
    lab.fix(&["verify", "38", "--run", &rv1, "--assurance", "approved_alternative", "--evidence", &evidence('e')]);
    assert_eq!(lab.fix(&["close", "35", "--outcome", "fixed"])["seq"], json!(40));
    let fixes = lab.fixes(None);
    let vf = fixes["findings"].as_array().unwrap().iter().find(|f| f["finding_id"] == v).unwrap().clone();
    assert_eq!((&vf["remediation"], &vf["verified"], &vf["integrated"], &vf["currently_resolved"]), (&json!("fix_verified"), &json!(true), &json!(false), &json!(false)),
        "verified on its branch is not integrated");
    let m = lab.metrics();
    assert_eq!((&m["M25"]["value"], &m["M26"]["value"], &m["M29"]["value"]), (&json!("2/2"), &json!("1/2"), &json!("8/11")));
    assert_eq!(m["M25"]["by_assignment"], json!({
        a.as_str(): {"numerator": 2, "denominator": 2, "value": "2/2", "not_achieved": 0, "reassigned": 1, "censored": 0},
        b.as_str(): {"numerator": 0, "denominator": 0, "value": null, "not_achieved": 0, "reassigned": 0, "censored": 0, "reason": "no_assigned_opportunities"}}));
    assert_eq!(m["M26"]["by_assignment"][a.as_str()], json!({"numerator": 1, "denominator": 2, "value": "1/2", "not_achieved": 1, "reassigned": 1, "censored": 0}));
    captured.push(capture(&lab));

    // Introduction needs causal evidence; the fixer is never charged through its fix.
    lab.refused(&["review", "fixes", "introduce", d, "--commit", &x1, "--method", "blame", "--contributor", "x-attempt=1", "--evidence", &evidence('f')], "is inference, not causal evidence");
    lab.refused(&["review", "fixes", "introduce", d, "--commit", &d1, "--method", "reliable_bisect", "--contributor", "d-a1=1", "--evidence", &evidence('f')], "the fixer is not charged");
    assert_eq!(lab.fix(&["introduce", d, "--commit", &x1, "--method", "reliable_bisect", "--contributor", "x-attempt=1", "--evidence", &evidence('f')])["seq"], json!(41));

    // A real regression on the integration branch reopens D: history stays, current credit goes.
    let regression = lab.commit("integration", &t2, DEFECT_SRC);
    assert_eq!(lab.fix(&["reopen", d, "--reason", "regression", "--observed", &regression, "--evidence", &evidence('0')])["seq"], json!(42));
    let fixes = lab.fixes(None);
    let df = fixes["findings"].as_array().unwrap().iter().find(|f| f["finding_id"] == d).unwrap().clone();
    assert_eq!((&df["remediation"], &df["verified"], &df["integrated"], &df["currently_resolved"]), (&json!("reopened"), &json!(true), &json!(true), &json!(false)));
    assert_eq!((&df["resolutions"][0]["integrated_id"], &df["resolutions"][0]["ended_seq"], &df["resolutions"][0]["ended_by"]), (&json!(i2), &json!(42), &json!("reopened")));
    assert_eq!((&df["introduction"]["introducing_oid"], &df["introduction"]["detection_oid"]), (&json!(x1), &json!(x1)));
    let m = lab.metrics();
    assert_eq!((&m["M25"]["value"], &m["M26"]["value"], &m["M27"]["value"], &m["M29"]["value"]), (&json!("2/2"), &json!("0/2"), &json!("1/1"), &json!("9/11")));
    assert_eq!((&m["M26"]["by_assignment"][a.as_str()]["value"], &m["M25"]["by_assignment"][a.as_str()]["value"]), (&json!("0/2"), &json!("2/2")),
        "the reopened repair stays in its cohort");
    captured.push(capture(&lab));

    // A new repair cycle is its own opportunity, censored within its horizon.
    lab.fix(&["open", d, "--unassigned"]);
    assert_eq!(lab.metrics()["M25"]["by_assignment"]["unassigned"], json!({"numerator": 0, "denominator": 0, "value": null, "not_achieved": 0, "reassigned": 0, "censored": 1, "reason": "empty_denominator"}));
    // Mixed contributions are split, never full credit each.
    lab.refused(&["review", "fixes", "credit", v, "--role", "implementation", "--proposal", "38", "--share", "v-b1=1", "--share", "v-a1=1/3", "--evidence", &evidence('1')], "sum to more than 1");
    lab.fix(&["credit", v, "--role", "implementation", "--proposal", "38", "--share", "v-b1=2/3", "--share", "v-a1=1/3", "--evidence", &evidence('1')]);
    assert_eq!(lab.metrics()["M29"]["value"], json!("10/11"));
    captured.push(capture(&lab));
    // Shared discovery keeps one finding's worth of credit.
    assert_eq!(lab.fix(&["credit", d, "--role", "discovery", "--share", "rev-1=1/2", "--share", "rev-2=1/2", "--evidence", &evidence('2')])["seq"], json!(45));
    let m21 = lab.metrics()["M21"].clone();
    assert_eq!((&m21["value"], &m21["by_configuration"], &m21["participation"], &m21["unallocated"]), (&json!("2"), &json!({b.as_str(): "1/2", c.as_str(): "3/2"}), &json!(3), &json!("0")));
    captured.push(capture(&lab));
    lab.fix(&["retract", "45"]);
    assert_eq!(lab.metrics()["M21"]["by_configuration"], json!({c.as_str(): "2"}));
    captured.push(capture(&lab));
    assert_eq!(captured.iter().map(|c| c.0).collect::<Vec<_>>(), [23, 27, 34, 40, 42, 44, 45, 46]);

    // The history keeps every correction; every earlier view rebuilds exactly.
    let kinds: Vec<String> = lab.fixes(None)["history"].as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap().to_owned()).collect();
    assert_eq!(kinds, ["repair_opened", "attempt_bound", "proposed", "proposed", "verified", "integrated", "repair_closed", "repair_opened", "attempt_bound", "attempt_bound",
        "proposed", "verified", "repair_closed", "introduced", "reopened", "repair_opened", "credited", "credited", "retracted"]);
    assert_rebuilds(&lab, &captured);

    // Proxies (doc 07 M45–M48) are analytics over canonical rows: collecting
    // them never changes the canonical store, and rebuilding the sidecar from
    // scratch gives the same values. First candidates: x passes, fix-d's D0
    // fails the regression check, fix-v's V1 passes: M45 = 2/3. Both
    // integrations are younger than the 14-day horizon: censored, no rate.
    let canonical = || ["state.db", "state.db-wal"].map(|n| fs::read(lab.project.join(".state").join(n)).unwrap_or_default());
    let before = canonical();
    lab.t(&["quality", "collect"]);
    assert!(before == canonical(), "collecting proxies wrote the canonical store");
    let proxies = lab.t(&["quality", "report"]);
    let m45 = proxies["metrics"]["M45"].clone();
    assert_eq!((&m45["value"], &m45["proxy"], &m45["source_trust"]), (&json!("2/3"), &json!(true), &json!("proxy_observed")), "{proxies}");
    for id in ["M47", "M48"] {
        assert_eq!((&proxies["metrics"][id]["censored"], &proxies["metrics"][id]["proxy"]), (&json!(2), &json!(true)), "{id}: {proxies}");
        assert_eq!(proxies["metrics"][id]["value"], json!(null), "{id}: a censored integration is never a rate: {proxies}");
    }
    // No producer: unavailable, never 0; every proxy is labelled as one.
    assert_eq!((&proxies["metrics"]["M46"]["value"], &proxies["metrics"]["flaky_tests"]["value"]),
        (&unavailable("no_main_check_producer"), &unavailable("no_repeat_runs")));
    for id in ["M45", "M46", "M47", "M48", "flaky_tests"] {
        assert_eq!((&proxies["metrics"][id]["proxy"], &proxies["metrics"][id]["source_trust"]), (&json!(true), &json!("proxy_observed")), "{id}");
    }
    fs::remove_file(lab.project.join(".state/telemetry.db")).unwrap();
    let _ = fs::remove_file(lab.project.join(".state/telemetry.db-wal"));
    let _ = fs::remove_file(lab.project.join(".state/telemetry.db-shm"));
    lab.t(&["quality", "collect"]);
    assert_eq!(lab.t(&["quality", "report"])["metrics"], proxies["metrics"], "a rebuilt sidecar gives the same proxies");
    assert!(before == canonical());
    // No proxy value moved a canonical decision: the fix views are unchanged.
    assert_eq!(lab.fixes(None), captured.last().unwrap().2);

    // Worker assertions stay assertions: the claimed checks are kept on the submission, never a verification.
    let claimed: String = lab.db().query_row("SELECT claimed_checks FROM result_submissions WHERE submission_id=?1", [&sd2], |r| r.get(0)).unwrap();
    assert_eq!(claimed, r#"["fixed","still passes"]"#);
    assert_eq!(lab.count(&format!("SELECT count(*) FROM verified_results WHERE submission_id='{sd2}'")), 0);
}

/// Doc 10 §5 denominator and correction fixture, on the real author
/// candidate X1. Each review is opened, assigned, started and completed (4
/// seqs) before its submissions. By hand:
///
/// | seq   | row                                                                 |
/// |-------|---------------------------------------------------------------------|
/// | 1–5   | O0 by rev-0 (B): open, assign, start, complete, sub 1               |
/// | 6     | claim 1 validated: prior finding P = `finding:canonical-6`          |
/// |       | window starts (`--since`)                                           |
/// | 7–11  | O1 by rev-1 (C): sub 2 = S1                                         |
/// | 12–16 | O2 by rev-2 (C): sub 3 = S2                                         |
/// | 17    | S1 split into claims 4, 5                                           |
/// | 18    | claim 4 validated new: N = `finding:canonical-18`                   |
/// | 19    | claim 5 duplicate of P                                              |
/// | 20    | claim 3 (S2) rejected                                               |
/// | 21    | N merged into P                                                     |
/// | 22    | 21 unmerged                                                         |
/// | 23–27 | O3 by rev-3 (A): sub 4, one broad report                            |
/// | 28    | claim 6 validated new: W = `finding:canonical-28`                   |
/// | 29–32 | repair 29 of W (unassigned): w-a1 bound, W1 proposed, verified      |
/// | 33    | sub 4 split into claims 7, 8, 9                                     |
/// | 34–36 | claim 7 validated as W; 8, 9 new: X = canonical-35, Y = canonical-36|
/// | 37    | sub 4's revision 1 restored; 38 revision 2 restored                 |
/// | 39    | W merged into N; 40 unmerged                                        |
///
/// In the window: at 20 S1 is mixed and S2 rejected-only: adjudicated 2,
/// M22 = 1/2 (50%), M23 = 0/2. At 21 S1 is duplicate-only: M22 = 0/2, M23 =
/// 1/2 (50%). At 22 the rates return (a new as-of revision; 21 still shows
/// the merge). At 36 the broad report is one validated submission with three
/// findings: M22 = 2/3; unique findings 5; W's fix stays on W alone: M25 =
/// 1/5 overall, X and Y unverified. At 37 X and Y are unvalidated again:
/// M25 = 1/3, M21 = 3. At 39 W is merged into N: the fix is not copied onto
/// N (M25 = 0/2); at 40 it is W's again (1/3).
#[test]
fn split_merge_and_unmerge_corrections_recompute_denominators_and_keep_history() {
    let lab = Lab::new();
    let (a, b, c) = (lab.a.clone(), lab.b.clone(), lab.c.clone());
    let dx = lab.contract("x", &lab.base, WEAK);
    lab.attempt("x", "x-attempt", Some(&a));
    let x1 = lab.commit("x1", &lab.base, DEFECT_SRC);
    let sx = lab.result("x", "x-attempt", &dx, &lab.base, &x1, &[]);
    let report = |since: Option<i64>| {
        let s = since.map(|s| s.to_string());
        let mut args = vec!["review", "report"];
        if let Some(s) = &s { args.extend(["--since", s.as_str()]); }
        let m = lab.t(&args)["metrics"].clone();
        (m["M22"]["value"].clone(), m["M23"]["value"].clone(), m["M22"]["denominator"].clone())
    };
    let mut captured = Vec::new();

    let (_, s0) = lab.review(&sx, "code", "slow", "rev-0", &b);
    lab.complete(&s0, &sx, &x1, "completed", json!(["finding:prior"]));
    lab.triage(&["validate", "1", "--new", "--severity", "high", "--evidence", &evidence('a')]);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let since = Some(unix_ms());
    std::thread::sleep(std::time::Duration::from_millis(5));
    let (_, s1) = lab.review(&sx, "code", "claude", "rev-1", &c);
    lab.complete(&s1, &sx, &x1, "completed", json!([{"ref": "finding:s1", "title": "Retry loop and division by zero"}]));
    let (_, s2) = lab.review(&sx, "security", "claude", "rev-2", &c);
    lab.complete(&s2, &sx, &x1, "completed", json!(["finding:s2"]));
    assert_eq!(lab.triage(&["split", "2", "--claim", "Retry loop never ends", "--claim", "Division by zero, as reported before"])["subject"], json!({"submission_id": 2, "revision": 2, "claims": [4, 5]}));
    lab.triage(&["validate", "4", "--new", "--severity", "medium", "--evidence", &evidence('b')]);
    lab.triage(&["duplicate", "5", "--of", "finding:canonical-6"]);
    lab.triage(&["reject", "3", "--reason", "intended_behavior"]);
    assert_eq!(report(since), (json!("1/2"), json!("0/2"), json!(2)), "S1 mixed, S2 rejected-only");
    assert_eq!(report(None), (json!("2/3"), json!("0/3"), json!(3)));
    captured.push(capture(&lab));

    assert_eq!(lab.triage(&["merge", "finding:canonical-18", "--into", "finding:canonical-6"])["seq"], json!(21));
    assert_eq!(report(since), (json!("0/2"), json!("1/2"), json!(2)), "S1 became duplicate-only");
    assert_eq!(lab.findings(None)["unique_findings"], json!(1));
    captured.push(capture(&lab));
    lab.triage(&["unmerge", "21"]);
    assert_eq!(report(since), (json!("1/2"), json!("0/2"), json!(2)));
    let at22 = capture(&lab);
    assert_eq!((&at22.3["M22"]["value"], &at22.3["M22"]["as_of_seq"]), (&captured[0].3["M22"]["value"], &json!(22)), "the earlier rates under a new as-of revision");
    captured.push(at22);

    // One broad report, a verified fix, then a split into three findings.
    let (_, s3) = lab.review(&sx, "test", "fast", "rev-3", &a);
    lab.complete(&s3, &sx, &x1, "completed", json!(["finding:broad"]));
    assert_eq!(lab.triage(&["validate", "6", "--new", "--severity", "high", "--evidence", &evidence('c')])["seq"], json!(28));
    let w = "finding:canonical-28";
    let dw = lab.contract("fix-w", &lab.base, GUARDED);
    lab.attempt("fix-w", "w-a1", Some(&a));
    lab.fix(&["open", w, "--unassigned"]);
    lab.fix(&["bind", "29", "--attempt", "w-a1"]);
    let w1 = lab.commit("w1", &lab.base, D1_SRC);
    let sw1 = lab.result("fix-w", "w-a1", &dw, &lab.base, &w1, &[]);
    let (state, rw1, _) = lab.verify(&sw1, "verify-w1", GUARDED);
    assert_eq!(state, "accepted");
    lab.fix(&["propose", "29", "--submission", &sw1]);
    assert_eq!(lab.fix(&["verify", "31", "--run", &rw1, "--assurance", "regression_reproduced", "--evidence", &evidence('d')])["seq"], json!(32));
    assert_eq!(lab.metrics()["M25"]["value"], json!("1/3"), "P, N, W; W verified");
    captured.push(capture(&lab));
    assert_eq!(lab.triage(&["split", "4", "--claim", "Loader crash", "--claim", "Leaked handle", "--claim", "Wrong exit code"])["subject"]["claims"], json!([7, 8, 9]));
    lab.triage(&["validate", "7", "--finding", w, "--severity", "high", "--evidence", &evidence('c')]);
    lab.triage(&["validate", "8", "--new", "--severity", "low", "--evidence", &evidence('e')]);
    lab.triage(&["validate", "9", "--new", "--severity", "low", "--evidence", &evidence('f')]);
    let state = lab.findings(None);
    assert_eq!((&state["unique_findings"], &state["summary"]["submissions"]), (&json!(5), &json!(4)), "a split never adds submissions");
    let sub4 = state["submissions"].as_array().unwrap().iter().find(|s| s["submission_id"] == 4).unwrap().clone();
    assert_eq!((&sub4["outcome"], &sub4["has_validated_claim"]), (&json!("validated_only"), &json!(true)));
    assert_eq!(report(since), (json!("2/3"), json!("0/3"), json!(3)), "three findings, one validated submission");
    let m = lab.metrics();
    assert_eq!((&m["M25"]["value"], &m["M21"]["value"], &m["M21"]["by_configuration"]), (&json!("1/5"), &json!("5"), &json!({a.as_str(): "3", b.as_str(): "1", c.as_str(): "1"})));
    let fixes = lab.fixes(None);
    let verified: Vec<&str> = fixes["findings"].as_array().unwrap().iter().filter(|f| f["verified"] == json!(true)).map(|f| f["finding_id"].as_str().unwrap()).collect();
    assert_eq!(verified, [w], "the fix is not copied to split findings");
    captured.push(capture(&lab));

    // Restore the unsplit report, then the split: decisions return with their claims.
    lab.triage(&["restore", "4", "--revision", "1"]);
    let m = lab.metrics();
    assert_eq!((&m["M25"]["value"], &m["M21"]["value"], &lab.findings(None)["unique_findings"]), (&json!("1/3"), &json!("3"), &json!(3)));
    captured.push(capture(&lab));
    lab.triage(&["restore", "4", "--revision", "2"]);
    assert_eq!(lab.metrics()["M25"]["value"], json!("1/5"));
    captured.push(capture(&lab));

    // A merge never copies a fix onto another finding; unmerge returns it to its own.
    assert_eq!(lab.triage(&["merge", w, "--into", "finding:canonical-18"])["seq"], json!(39));
    let m = lab.metrics();
    assert_eq!(m["M25"]["value"], json!("0/4"), "P, N, X, Y: W's fix stays on merged W");
    assert_eq!(m["M25"]["by_assignment"]["unassigned"]["censored"], json!(1), "the repair opportunity itself stays visible");
    captured.push(capture(&lab));
    lab.triage(&["unmerge", "39"]);
    assert_eq!(lab.metrics()["M25"]["value"], json!("1/5"));
    captured.push(capture(&lab));

    assert_eq!(captured.iter().map(|c| c.0).collect::<Vec<_>>(), [20, 21, 22, 32, 36, 37, 38, 39, 40]);
    let kinds: Vec<String> = lab.findings(None)["history"].as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap().to_owned()).collect();
    assert_eq!(kinds, ["submitted", "decided", "submitted", "submitted", "split", "decided", "decided", "decided", "merged", "unmerged", "submitted", "decided",
        "split", "decided", "decided", "decided", "restored", "restored", "merged", "unmerged"]);
    assert_rebuilds(&lab, &captured);
}

impl Lab {
    /// Bind `attempt` (configuration of arm `arm`) to candidate group `group`,
    /// as `admit_prepared` writes it in the reservation transaction.
    fn arm(&self, task: &str, attempt: &str, group: &str, arm: u32, configuration: &str) {
        self.attempt(task, attempt, Some(configuration));
        self.db().execute("INSERT INTO candidate_arm_attempts(group_id,arm,attempt_id,bound_unix_ms,source) VALUES(?1,?2,?3,1,'admit_prepared')", rusqlite::params![group, arm, attempt]).unwrap();
    }
    /// Queue `consumer` behind `predecessor`'s `verified_result` through `task queue`.
    fn queue_dependent(&self, consumer: &str, predecessor: &str) {
        use herdr_farm::{domain::TaskId, runtime};
        let head = runtime::add_task(&self.project, TaskId::new(consumer).unwrap(), format!("consumer {consumer}"), runtime::snapshot(&self.project).unwrap().head).unwrap();
        let request = self.home.path().join(format!("{consumer}-queue.json"));
        fs::write(&request, json!({"priority": 0, "dependencies": [{"predecessor": predecessor, "requirement": "verified_result"}]}).to_string()).unwrap();
        self.ok(&["task", "demo", "queue", consumer, "--input-file", request.to_str().unwrap(), "--expected-revision", "1", "--expected-head", &head.to_string()]);
    }
    /// `consumer`'s dependency blockers in `scheduler inspect` (factory
    /// admission is off, so a counting edge shows only `admission_disabled:`).
    fn blockers(&self, consumer: &str) -> Vec<String> {
        let report = self.ok(&["scheduler", "demo", "inspect"]);
        let entry = report["entries"].as_array().unwrap().iter().find(|e| e["task"] == consumer).unwrap().clone();
        entry["blockers"].as_array().unwrap().iter().map(|b| b.as_str().unwrap().to_owned())
            .filter(|b| ["verified_dependency_evidence_unavailable:", "predecessor_failed:", "admission_disabled:"].iter().any(|p| b.starts_with(p))).collect()
    }
    /// Make `attempt` its task's active, started attempt as a launch records it.
    fn started(&self, task: &str, attempt: &str) {
        let db = self.db();
        db.execute("UPDATE tasks SET active_attempt=?2 WHERE id=?1", [task, attempt]).unwrap();
        let operation = format!("op-{attempt}");
        db.execute("INSERT INTO operations(id,task_id,kind,target,payload_version,payload,payload_hash,expected_revision,due_unix_ms,idempotency_key) VALUES(?1,?2,'runtime.launch','binding',1,'{}',?3,1,0,?1)",
            rusqlite::params![operation, task, format!("{:x}", Sha256::digest(b"{}"))]).unwrap();
        let inputs = r#"{"inputs":{"version":2,"effective_profile":{}}}"#;
        db.execute("INSERT INTO attempt_inputs(attempt_id,operation_id,payload,payload_hash) VALUES(?1,?2,?3,?4)", rusqlite::params![attempt, operation, inputs, format!("{:x}", Sha256::digest(inputs.as_bytes()))]).unwrap();
        db.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('runtime.launch_started',?1,1,1,'{}')", [&operation]).unwrap();
    }
    /// `task demo complete TASK`; must be refused with `reason`, writing no completion request.
    fn completion_refused(&self, task: &str, reason: &str) {
        let revision: i64 = self.db().query_row("SELECT revision FROM tasks WHERE id=?1", [task], |r| r.get(0)).unwrap();
        let before = self.count("SELECT max(sequence) FROM events");
        let err = self.fail(&["task", "demo", "complete", task, "--expected-revision", &revision.to_string()]);
        assert!(err.contains(reason), "{task}: {err}");
        assert_eq!(self.count("SELECT max(sequence) FROM events"), before, "{task}: a refused completion wrote");
    }
    fn pending_integration(&self) -> Vec<String> {
        let mut v: Vec<String> = self.db().prepare("SELECT submission_id FROM pending_integration_work").unwrap().query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
        v.sort();
        v
    }
    fn auto_integrate(&self) {
        self.configure_integration();
        let head = herdr_farm::runtime::snapshot(&self.project).unwrap().head.to_string();
        assert_eq!(self.ok(&["result", "demo", "auto", "--integrate", "on", "--expected-head", &head])["integrate"], json!(true));
    }
    /// The CLI as a worker runs it: `HOME` is a retained profile's execution home.
    fn as_worker(&self, home: &str, args: &[&str]) -> Output {
        Command::new(BIN).env_clear().env("HOME", self.home.path().join(home)).env("PATH", "/usr/bin:/bin").arg("--root").arg(&self.root).args(args).output().unwrap()
    }
}

/// Doc 06 §6c/§7 and doc 10 §5a: a candidate group where selection and
/// verification disagree, over the real verification and integration path.
/// Task g (policy: the zero divisor is guarded) seals arms 1 `fast` (A) and
/// 2 `slow` (B). Arm 1's candidate has no guard: its real run is rejected.
/// Arm 2's candidate passes. The operator selects arm 1 anyway. Task h is
/// the same, closed by the rule. Worker attempts to elevate their own arm
/// (library principal `worker:*`, or the CLI from a worker's execution home)
/// are refused and write nothing.
///
/// By hand: g's selection verifies and integrates nothing: arm 1 has no
/// verified result, arm 2 is a held loser (pending projection empty, `result
/// integrate` refused), g's dependent stays blocked and g cannot complete.
/// h's rule picks arm 2 (the first accepted in launch order), which is queued,
/// integrates and releases h's dependent. M41 (min 1) over g and h: A 1/2, B
/// 1/2, A vs B 1 win 1 loss. M42 uses verified outcomes, not selections:
/// A−B = (0 − 2)/2 = −100 points, B−A = +100.
#[test]
fn selection_that_disagrees_with_verification_verifies_and_releases_nothing() {
    let lab = Lab::new();
    let (a, b) = (lab.a.clone(), lab.b.clone());
    let mut groups = std::collections::BTreeMap::new();
    let mut subs = std::collections::BTreeMap::new();
    for task in ["g", "h"] {
        let digest = lab.contract(task, &lab.base, GUARDED);
        let group = lab.t(&["quality", "groups", "create", task, "--arm", "fast", "--arm", "slow"])["group"]["group_id"].as_str().unwrap().to_owned();
        for (arm, configuration, source) in [(1, &a, D2_SRC), (2, &b, D1_SRC)] {
            let attempt = format!("{task}-a{arm}");
            lab.arm(task, &attempt, &group, arm, configuration);
            let candidate = lab.commit(&attempt, &lab.base, &format!("{source}// {attempt}\n"));
            let sub = lab.result(task, &attempt, &digest, &lab.base, &candidate, &["all checks passed"]);
            let (state, _, result) = lab.verify(&sub, &format!("verify-{attempt}"), GUARDED);
            assert_eq!(state, if arm == 1 { "rejected" } else { "accepted" }, "{attempt}");
            // Arm 1's worker then ends (as its termination would be recorded): its outcome settles as rejected.
            if arm == 1 { lab.db().execute("UPDATE attempts SET state='failed',termination_observed=1 WHERE id=?1", [&attempt]).unwrap(); }
            subs.insert((task, arm), (sub, result));
        }
        groups.insert(task, group);
        lab.queue_dependent(&format!("b{task}"), task);
    }
    lab.auto_integrate();
    let (g, h) = (groups["g"].clone(), groups["h"].clone());
    let missing = |p: &str| vec![format!("verified_dependency_evidence_unavailable:{p}:verified_result")];
    let counts = vec!["admission_disabled:verified_result".to_owned()];
    assert_eq!(lab.blockers("bg"), missing("g"), "no arm releases before its group's selection");
    // Both verified arms enter the pending projection; the producer's turn drops every held arm.
    assert_eq!(lab.pending_integration().len(), 2);
    assert_eq!(herdr_farm::store::service_project_integration_jobs(&lab.project).unwrap().enqueued, 0);
    assert_eq!(lab.pending_integration(), Vec::<String>::new());

    // A worker cannot elevate its own arm: not through the store as a worker principal, not through the owner's CLI from its execution home.
    let mut store = SqliteStore::open(&lab.project.join(".state/state.db")).unwrap();
    let choice = herdr_farm::store::SelectionChoice::Arm { arm: 1, submission: None, runner_up: vec![] };
    for principal in ["worker:g-a1", "g-a1", "g-a2", "import:bot"] {
        let err = store.select_candidate(&g, &choice, "operator_judgment", principal, unix_ms()).unwrap_err();
        assert!(format!("{err:?}").contains("cannot select"), "{principal}: {err:?}");
    }
    drop(store);
    for args in [vec!["telemetry", "demo", "quality", "groups", "select", &g, "--arm", "1"], vec!["telemetry", "demo", "quality", "groups", "select", &g, "--judge", "me", "--submission", &subs[&("g", 1)].0],
        vec!["telemetry", "demo", "quality", "groups", "create", "bg", "--arm", "fast", "--arm", "slow"]] {
        let out = lab.as_worker("fast-home", &args);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success() && err.contains("refuses to run inside a worker execution context"), "{args:?}: {err}");
    }
    assert_eq!(lab.count("SELECT count(*) FROM candidate_selections"), 0);

    // The operator selects g's rejected arm 1: selection is not verification.
    let selection = lab.t(&["quality", "groups", "select", &g, "--arm", "1", "--reason", "operator_judgment"])["selection"].clone();
    assert_eq!((&selection["outcome"], &selection["arm"], &selection["submission_id"]), (&json!("selected"), &json!(1), &json!(subs[&("g", 1)].0)));
    let outcomes: Vec<(Value, Value)> = selection["evidence"].as_array().unwrap().iter().map(|e| (e["verification"].clone(), e["arm_outcome"].clone())).collect();
    assert_eq!(outcomes, [(json!("rejected"), json!("rejected")), (json!("accepted"), json!("accepted"))]);
    assert_eq!(lab.pending_integration(), Vec::<String>::new(), "nothing verified to queue; the verified loser is held");
    assert_eq!(herdr_farm::store::service_project_integration_jobs(&lab.project).unwrap().enqueued, 0);
    let g2_result = subs[&("g", 2)].1.clone().unwrap();
    let work = lab.home.path().join("integrate-g2-work");
    let err = lab.fail(&["result", "demo", "integrate", &g2_result, "--repository", lab.repo.to_str().unwrap(), "--idempotency-key", "integrate-g2", "--work-dir", work.to_str().unwrap()]);
    assert!(err.contains("a candidate-group arm integrates only as its group's selection"), "{err}");
    assert_eq!(lab.count("SELECT count(*) FROM integration_operations"), 0);
    assert_eq!(lab.blockers("bg"), missing("g"), "the selected candidate is not verified; the verified loser never counts");
    lab.started("g", "g-a2");
    lab.completion_refused("g", "a candidate group's task completes only from its selected submission");

    // The rule closes h with its first verified arm, which is queued and integrates.
    let rule = lab.t(&["quality", "groups", "select", &h, "--rule"])["selection"].clone();
    assert_eq!((&rule["arm"], &rule["submission_id"], &rule["reason"]), (&json!(2), &json!(subs[&("h", 2)].0), &json!("first_passing_verification")));
    assert_eq!(lab.pending_integration(), vec![subs[&("h", 2)].0.clone()]);
    assert_eq!(lab.blockers("bh"), counts);
    lab.integrate(subs[&("h", 2)].1.as_ref().unwrap(), "integrate-h2");

    // M41 counts selections; M42 counts verified outcomes.
    let report = lab.t(&["quality", "groups", "report", "--min-groups", "1"]);
    let (m41, m42) = (&report["metrics"]["M41"], &report["metrics"]["M42"]);
    let cell = |c: &str| m41["by_selector"]["all"]["configurations"].as_array().unwrap().iter().find(|x| x["configuration_id"] == c).unwrap().clone();
    assert_eq!(cell(&a), json!({"configuration_id": a, "groups": 2, "selected": 1, "no_selection": 0, "other_selected": 1, "value": "1/2"}));
    assert_eq!(cell(&b), json!({"configuration_id": b, "groups": 2, "selected": 1, "no_selection": 0, "other_selected": 1, "value": "1/2"}));
    let pair = |list: &Value, x: &str, y: &str| list.as_array().unwrap().iter().find(|p| p["a"] == x && p["b"] == y).unwrap().clone();
    let h2h = pair(&m41["by_selector"]["all"]["head_to_head"], &a, &b);
    assert_eq!((&h2h["wins"], &h2h["losses"], &h2h["value"]), (&json!(1), &json!(1), &json!("1/2")));
    let ab = pair(&m42["pairs"], &a, &b);
    assert_eq!((&ab["n"], &ab["both_accepted"], &ab["a_only"], &ab["b_only"], &ab["neither"], &ab["value"]), (&json!(2), &json!(0), &json!(0), &json!(2), &json!(0), &json!(-100.0)));
    assert_eq!(pair(&m42["pairs"], &b, &a)["value"], json!(100.0));

    // Rebuild: a fresh copy of the canonical store gives the same paired metrics.
    let fresh = tempfile::tempdir().unwrap();
    fs::create_dir_all(fresh.path().join("root/demo/.state")).unwrap();
    lab.db().execute("VACUUM INTO ?1", [fresh.path().join("root/demo/.state/state.db").to_str().unwrap()]).unwrap();
    let out = Command::new(BIN).env_clear().env("HOME", fresh.path()).env("PATH", "/usr/bin:/bin").arg("--root").arg(fresh.path().join("root"))
        .args(["telemetry", "demo", "quality", "groups", "report", "--min-groups", "1"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap()["metrics"], report["metrics"]);
}

impl Lab {
    /// A new ed25519 key `name` in the lab home; returns (key path, public key).
    fn keypair(&self, name: &str) -> (PathBuf, String) {
        let key = self.home.path().join(name);
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&key).output().unwrap().status.success());
        let public = fs::read_to_string(key.with_extension("pub")).unwrap().split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        (key, public)
    }
    fn sign(&self, key: &Path, namespace: &str, file: &Path) {
        let _ = fs::remove_file(file.with_extension(format!("{}.sig", file.extension().unwrap().to_str().unwrap())));
        assert!(Command::new("/usr/bin/ssh-keygen").args(["-Y", "sign", "-f"]).arg(key).args(["-n", namespace]).arg(file).output().unwrap().status.success());
    }
    /// An owner-signed `code_review` grant to `subject` (public key `public`)
    /// over task x's kinds `kinds`, installed; returns its grant id.
    fn grant(&self, name: &str, subject: &str, public: &str, kinds: &[&str]) -> String {
        let now = unix_ms();
        let grant = json!({"schema": "code_review_authority.v1", "scope": "code_review", "issuer": "owner", "subject": subject,
            "subject_public_key": public, "subject_configurations": [], "project_store": self.store, "repositories": [self.repo.canonicalize().unwrap()],
            "tasks": [{"task_id": "x", "contract_revision": 1}], "kinds": kinds, "review_configurations": [], "actions": ["accept_review_completion"],
            "max_decisions": 4, "valid_from_unix_ms": now - 60_000, "expires_unix_ms": now + 3_600_000,
            "prohibited_effects": ["alter_requirements", "approve_author_attempt", "approve_own_work", "child_delegation", "increase_permissions"],
            "authority": herdr_farm::authority::policy_reference(&self.project).unwrap()});
        let file = self.home.path().join(format!("{name}.json"));
        fs::write(&file, serde_json::to_vec_pretty(&grant).unwrap()).unwrap();
        self.sign(&self.key, "code-review-authority@herdr-projects", &file);
        self.t(&["review", "authority", "import", file.to_str().unwrap(), file.with_extension("json.sig").to_str().unwrap()])["grant"]["grant_id"].as_str().unwrap().to_owned()
    }
    /// Draft the acceptance of `session` under `grant`, signed with `key`;
    /// returns (document, signature) paths.
    fn acceptance_request(&self, name: &str, session: &str, grant: &str, key: &Path) -> (String, String) {
        let file = self.home.path().join(format!("{name}.json"));
        self.t(&["review", "accept", "draft", session, "--grant", grant, "--output", file.to_str().unwrap()]);
        self.sign(key, "review-acceptance@herdr-projects", &file);
        (file.display().to_string(), file.with_extension("json.sig").display().to_string())
    }
}

/// Doc 10 §5 first fixture and doc 06 §2/§7 (forged receipts, reviewer
/// self-approval, scoped authority). The author's result carries claimed
/// checks, a worker's "verifier receipt": it stays an assertion (no run, no
/// verified result, the dependent stays blocked, M45 counts it pending) until
/// the native verifier runs. Review receipts cannot carry acceptance or a
/// decision; worker principals cannot triage, attribute, seed or reveal.
/// Delegated acceptance under an owner-signed `code_review` grant for kind
/// `code` accepts O1 only: O2 (`security`) is outside its scope, a grant
/// naming the reviewing attempt or the author is refused, and neither the
/// owner's key nor another key stands in for the subject's. By hand:
/// acceptance changes no triage: M22 stays null with 2 pending, then 2/2
/// after the owner validates both; M24's closed cohort holds O1 alone
/// (accepted), O2 and O3 await acceptance, its numerator is 1 (O2's finding
/// is from an undecided review).
#[test]
fn worker_assertions_and_self_approval_never_become_accepted_outcomes() {
    let lab = Lab::new();
    let (a, b, c) = (lab.a.clone(), lab.b.clone(), lab.c.clone());
    let dx = lab.contract("x", &lab.base, WEAK);
    lab.attempt("x", "x-attempt", Some(&a));
    let x1 = lab.commit("x1", &lab.base, DEFECT_SRC);
    let forged = format!("verification_run:{} accepted", "f".repeat(64));
    let sx = lab.result("x", "x-attempt", &dx, &lab.base, &x1, &["all checks passed", &forged]);
    lab.queue_dependent("bx", "x");
    lab.auto_integrate();
    assert_eq!((lab.count("SELECT count(*) FROM verification_runs"), lab.count("SELECT count(*) FROM verified_results")), (0, 0));
    assert_eq!(lab.pending_integration(), Vec::<String>::new());
    assert_eq!(lab.blockers("bx"), vec!["verified_dependency_evidence_unavailable:x:verified_result".to_owned()], "a claimed check releases nothing");
    lab.t(&["quality", "collect"]);
    let m45 = lab.t(&["quality", "report"])["metrics"]["M45"].clone();
    assert_eq!((&m45["value"], &m45["pending"]), (&json!(null), &json!(1)), "{m45}");
    let (state, _, _) = lab.verify(&sx, "verify-x", WEAK);
    assert_eq!(state, "accepted");
    assert_eq!(lab.blockers("bx"), vec!["admission_disabled:verified_result".to_owned()], "only the native verifier's receipt counts");
    lab.t(&["quality", "collect"]);
    assert_eq!(lab.t(&["quality", "report"])["metrics"]["M45"]["value"], json!("1/1"));

    // Receipts are proposals: acceptance or decisions inside them are refused.
    let (_, s1) = lab.review(&sx, "code", "claude", "rev-1", &c);
    let receipt = json!({"schema": "review_receipt.v1", "session_id": s1, "submission_id": sx, "candidate_oid": x1, "outcome": "completed", "findings": ["finding:div"], "evidence": []});
    for (field, value) in [("accepted", json!(true)), ("trust", json!("accepted")), ("verified", json!(true)), ("independent", json!(true))] {
        let mut forged = receipt.clone();
        forged[field] = value;
        let path = lab.home.path().join(format!("forged-{field}.json"));
        fs::write(&path, forged.to_string()).unwrap();
        lab.refused(&["review", "complete", "--input-file", path.to_str().unwrap()], &format!("unknown field `{field}`"));
    }
    let mut elevated = receipt.clone();
    elevated["findings"] = json!([{"ref": "finding:div", "title": "Division by zero", "validated": true}]);
    let path = lab.home.path().join("elevated.json");
    fs::write(&path, elevated.to_string()).unwrap();
    lab.refused(&["review", "complete", "--input-file", path.to_str().unwrap()], "unknown field `validated` in a finding");
    lab.complete(&s1, &sx, &x1, "completed", json!(["finding:div"]));
    let (_, s2) = lab.review(&sx, "security", "slow", "rev-2", &b);
    lab.complete(&s2, &sx, &x1, "completed", json!(["finding:zero"]));
    let (_, s3) = lab.review(&sx, "code", "fast", "rev-3", &a);
    lab.complete(&s3, &sx, &x1, "completed", json!([]));

    // Worker, reviewer and author identities hold no owner authority in the store.
    let head = lab.ledger_head();
    let mut store = SqliteStore::open(&lab.project.join(".state/state.db")).unwrap();
    let validate = TriageRequest { outcome: TriageOutcome::Validated { target: FindingTarget::New { title: None }, severity: "high".into() }, evidence: vec![evidence('a')], expected_seq: None };
    for principal in ["worker:rev-1", "rev-1", "x-attempt", "worker:x-attempt", "import:ci"] {
        assert!(store.triage_finding_claim(1, &validate, principal, unix_ms()).is_err(), "{principal} triaged");
        assert!(store.register_evaluation_candidate(&sx, &herdr_farm::store::EvaluationArm::CleanControl, None, principal, unix_ms()).is_err(), "{principal} registered a seed arm");
        assert!(store.reveal_evaluation_candidate(&sx, None, principal, unix_ms()).is_err(), "{principal} revealed");
        assert!(store.open_repair("finding:canonical-1", &herdr_farm::store::RepairAssignment::Unassigned, 86_400_000, None, principal, unix_ms()).is_err(), "{principal} opened a repair");
    }
    drop(store);
    assert_eq!(lab.ledger_head(), head, "refused principals wrote nothing");

    // Scoped delegated acceptance.
    let (carol_key, carol) = lab.keypair("carol");
    let g_carol = lab.grant("grant-carol", "reviewer:carol", &carol, &["code"]);
    let (doc, sig) = lab.acceptance_request("accept-s1", &s1, &g_carol, &carol_key);
    assert_eq!(lab.t(&["review", "accept", &s1, "--document", &doc, "--signature", &sig])["acceptance"]["decision"], json!("accepted"));
    let decisions = || lab.count("SELECT count(*) FROM review_acceptances");
    // O2 is a security review: outside the grant's kinds.
    let (doc, sig) = lab.acceptance_request("accept-s2", &s2, &g_carol, &carol_key);
    lab.refused(&["review", "accept", &s2, "--document", &doc, "--signature", &sig], "the session is outside the grant's scope: review kind");
    // The owner's key cannot stand in for the subject's; nor can a worker's own key.
    let (worker_key, _) = lab.keypair("worker-key");
    for (name, key) in [("owner-signed", lab.key.clone()), ("worker-signed", worker_key)] {
        let (doc, sig) = lab.acceptance_request(name, &s3, &g_carol, &key);
        lab.refused(&["review", "accept", &s3, "--document", &doc, "--signature", &sig], "it must be signed with the grant subject's key");
    }
    // Self-approval: a grant whose subject is the reviewing attempt, or the author, decides nothing.
    for (subject, name) in [("reviewer:rev-3", "self"), ("reviewer:x-attempt", "author")] {
        let (key, public) = lab.keypair(name);
        let grant = lab.grant(&format!("grant-{name}"), subject, &public, &["code"]);
        let (doc, sig) = lab.acceptance_request(&format!("accept-{name}"), &s3, &grant, &key);
        lab.refused(&["review", "accept", &s3, "--document", &doc, "--signature", &sig], "a reviewer cannot accept its own review or a review of work by the author attempt");
    }
    assert_eq!(decisions(), 1);
    let shown = lab.t(&["review", "show"]);
    let completion = |s: &str| shown["opportunities"].as_array().unwrap().iter().flat_map(|o| o["sessions"].as_array().unwrap().clone()).find(|x| x["session_id"] == s).unwrap()["completion"].clone();
    assert_eq!((&completion(&s1)["trust"], &completion(&s1)["acceptance"]["decision"], &completion(&s2)["acceptance"]), (&json!("proposal"), &json!("accepted"), &json!(null)));

    // Acceptance decides reviews, never findings.
    let m = lab.metrics();
    assert_eq!((&m["M22"]["value"], &m["M22"]["pending"]), (&json!(null), &json!(2)));
    assert_eq!(m["M24"]["opportunities"], json!({"closed": 0, "accepted": 0, "rejected": 0, "unsuccessful": 0, "awaiting_acceptance": 2, "awaiting_adjudication": 1, "open": 0}), "{}", m["M24"]);
    lab.triage(&["validate", "1", "--new", "--severity", "high", "--evidence", &evidence('a')]);
    lab.triage(&["validate", "2", "--new", "--severity", "high", "--evidence", &evidence('b')]);
    let m = lab.metrics();
    assert_eq!((&m["M22"]["value"], &m["M21"]["value"]), (&json!("2/2"), &json!("2")));
    assert_eq!((&m["M24"]["opportunities"]["closed"], &m["M24"]["opportunities"]["accepted"], &m["M24"]["numerator"]), (&json!(1), &json!(1), &json!(1)), "{}", m["M24"]);
    assert_eq!(m["M22"]["review_acceptance"]["accepted"], json!(1));
}

fn seed_set() -> Value {
    serde_json::from_str(&fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/telemetry/seeds/starter-seed-set.json")).unwrap()).unwrap()
}
/// (class, seeded source, reproducer reference) of starter seed `id`.
fn seed(id: &str) -> (String, String, String) {
    let set = seed_set();
    let s = set["seeds"].as_array().unwrap().iter().find(|s| s["id"] == id).unwrap().clone();
    (s["class"].as_str().unwrap().to_owned(), s["seeded"].as_str().unwrap().to_owned(), format!("sha256:{:x}", Sha256::digest(s["reproducer"].as_str().unwrap().as_bytes())))
}

/// Doc 10 §5a seeded review, doc 06 §6a, and the seeded-candidate adversary,
/// on the real repository with the starter seeds injected into disposable
/// candidates. Seeded S1–S4 (logic, boundary, security, test_weakening) and
/// clean controls C1, C2, each its own task, registered at seq 1–6 before any
/// review. Configuration R (`fast`, A) reviews each once, each review opened,
/// assigned, started and completed: S1 at 7–10 with two findings (claims 1,
/// 2 at 11, 12), then S2, S3, S4, C1, C2 five seqs each (claims 3–7), up to
/// 37. Triage 38–44: claims 1–5 and 7 validated, claim 6 (C1) rejected.
/// 45–47 link claims 1, 3, 4 to the seeds of S1, S2, S3.
///
/// Replay (exact, 0063): as of 6 no opportunity is listed (M43 and M44
/// `not_completed` 0); as of 9 only S1's, opened, assigned and started but
/// not completed (M43 `not_completed` 1, no trial); as of 12 S1's is
/// completed and its trial pending (pending 1); as of 37 all six are
/// completed and every trial and control pending (M43 pending 4, M44
/// pending 2); as of 46 two seeds are detected.
///
/// By hand: M43 = 3/4 = 75.00 (S4's seed missed; its validated claim 5 is an
/// incidental finding), M44 = 1/2 = 50.00 (C1's rejected-only report; C2's
/// validated finding is no false alarm), the same for R. The seed-linked
/// submissions are evaluation artefacts: M22 = 3/4 (claims 2, 5, 7
/// validated, 6 rejected), F = 3 findings (M21 = 3, 3 seeded_evaluation).
/// A seeded candidate that passes verification never integrates (the
/// producer enqueues only the control; `result integrate` refused), never
/// releases its dependent and never completes its task. In a candidate group
/// whose arm 1 is a seeded candidate that passes verification and arm 2 a
/// clean one that passes too, the operator's and a judge's selection of arm
/// 1 are refused (a seeded arm is never a winner), and the rule
/// (`first_accepted_in_launch_order.v2`) skips arm 1 (`rule_skip`
/// `seeded_candidate`, no rank) and selects arm 2 (rank 1), which releases
/// the group's dependent and integrates; arm 1 still never integrates.
#[test]
fn seeded_recall_and_the_seeded_candidate_guard_end_to_end() {
    let lab = Lab::new();
    let a = lab.a.clone();
    let clean = seed_set()["clean"].as_str().unwrap().to_owned();
    let mut candidates = Vec::new();
    for (task, source) in [("s1", seed("logic-inverted-guard")), ("s2", seed("boundary-off-by-one")), ("s3", seed("security-unchecked-index")), ("s4", seed("test-weakening-ignored-case"))]
        .into_iter().map(|(t, s)| (t, Some(s))).chain([("c1", None), ("c2", None)]) {
        let digest = lab.contract(task, &lab.base, WEAK);
        let attempt = format!("{task}-attempt");
        lab.attempt(task, &attempt, Some(&a));
        let oid = lab.commit(task, &lab.base, source.as_ref().map_or(clean.as_str(), |s| s.1.as_str()));
        let sub = lab.result(task, &attempt, &digest, &lab.base, &oid, &["all checks passed"]);
        candidates.push((task, sub, oid, source));
    }
    for (task, sub, _, source) in &candidates {
        let event = match source {
            Some((class, _, reproducer)) => lab.t(&["review", "seeds", "register", sub, "--seed", &format!("{class}={reproducer}")])["event"].clone(),
            None => lab.t(&["review", "seeds", "register", sub, "--control"])["event"].clone(),
        };
        assert_eq!((&event["authority"], &event["principal"]), (&json!("evaluation_owner.v1"), &json!("operator:cli")), "{task}");
    }
    assert_eq!(lab.ledger_head(), 6);
    let seeds_view = || { let mut v = lab.t(&["review", "seeds", "show"])["seeds"].clone(); v.as_object_mut().unwrap().remove("head_seq"); v };
    let seeds_report = |as_of: Option<i64>| {
        let seq = as_of.map(|s| s.to_string());
        let mut args = vec!["review", "seeds", "report", "--min-trials", "1"];
        if let Some(seq) = &seq { args.extend(["--as-of", seq.as_str()]); }
        lab.t(&args)["metrics"].clone()
    };
    // (seq, seeds show, seeds report, review show) at the head.
    let checkpoint = |seq: i64| {
        assert_eq!(lab.ledger_head(), seq);
        (seq, seeds_view(), seeds_report(None), lab.t(&["review", "show"]))
    };
    let mut captured = vec![checkpoint(6)];
    // Blind reviews by R. The presentation never shows seed state.
    let mut sessions = Vec::new();
    for (i, (task, sub, oid, _)) in candidates.iter().enumerate() {
        let (opportunity, session) = lab.review(sub, "code", "fast", &format!("r-{task}"), &a);
        if i == 0 { captured.push(checkpoint(9)); }
        let presented = lab.t(&["review", "present", &opportunity]).to_string();
        for hidden in ["seed", "control", "reproducer", "evaluation"] { assert!(!presented.contains(hidden), "{hidden} shown to the reviewer of {task}"); }
        let findings = if i == 0 { json!(["finding:a", "finding:b"]) } else { json!([format!("finding:{task}")]) };
        lab.complete(&session, sub, oid, "completed", findings);
        if i == 0 { captured.push(checkpoint(12)); }
        sessions.push(session);
    }
    captured.push(checkpoint(37));
    for claim in ["1", "2", "3", "4", "5"] { lab.triage(&["validate", claim, "--new", "--severity", "high", "--evidence", &evidence('a')]); }
    lab.triage(&["reject", "6", "--reason", "intended_behavior"]);
    lab.triage(&["validate", "7", "--new", "--severity", "low", "--evidence", &evidence('b')]);
    let seeds = lab.t(&["review", "seeds", "show"])["seeds"].clone();
    let seed_of = |sub: &str| seeds["candidates"].as_array().unwrap().iter().find(|c| c["submission_id"] == sub).unwrap()["seeds"][0]["seed_id"].as_i64().unwrap().to_string();
    captured.push(checkpoint(44));
    // A detection needs a triaged claim of a review of the seed's own candidate.
    lab.refused(&["review", "seeds", "detect", &seed_of(&candidates[0].1), "--claim", "3", "--evidence", &evidence('c')], "another candidate");
    for (candidate, claim) in [(0, "1"), (1, "3"), (2, "4")] {
        lab.t(&["review", "seeds", "detect", &seed_of(&candidates[candidate].1), "--claim", claim, "--evidence", &evidence('c')]);
    }
    captured.push(checkpoint(47));

    let report = seeds_report(None);
    let cell = |m: &Value, n: &str, d: &str| (m[n].clone(), m[d].clone(), m["pending"].clone(), m["value"].clone(), m["percent"].clone());
    assert_eq!(cell(&report["M43"], "detected", "trials"), (json!(3), json!(4), json!(0), json!("3/4"), json!("75.00")));
    assert_eq!(cell(&report["M44"], "false_alarms", "controls"), (json!(1), json!(2), json!(0), json!("1/2"), json!("50.00")));
    assert_eq!(cell(&report["M43"]["by_configuration"][a.as_str()], "detected", "trials"), (json!(3), json!(4), json!(0), json!("3/4"), json!("75.00")));
    assert_eq!(report["M43"]["by_seed_class"]["test_weakening"]["detected"], json!(0));
    // At the default minimum sample (20 trials) the same counts are shown, never a rate.
    assert_eq!(lab.metrics()["M43"]["value"], json!({"status": "unavailable", "reason": "insufficient_data"}));
    let m = lab.metrics();
    assert_eq!((&m["M22"]["value"], &m["M22"]["seeded_evaluation"]), (&json!("3/4"), &json!({"submissions": 3, "claims": 3})));
    assert_eq!((&m["M21"]["value"], &m["M21"]["seeded_evaluation"]), (&json!("3"), &json!(3)));
    // Rebuild: M43 at an earlier watermark, and the historical opportunity counts by hand.
    assert_eq!(seeds_report(Some(46))["M43"]["detected"], json!(2));
    let counts = |m: &Value| (m["M43"]["trials"].clone(), m["M43"]["pending"].clone(), m["M43"]["not_completed"].clone(),
        m["M44"]["controls"].clone(), m["M44"]["pending"].clone(), m["M44"]["not_completed"].clone());
    for (seq, expected) in [(6, (0, 0, 0, 0, 0, 0)), (9, (0, 0, 1, 0, 0, 0)), (12, (0, 1, 0, 0, 0, 0)), (37, (0, 4, 0, 0, 2, 0))] {
        let (t, p, n, c, cp, cn) = expected;
        assert_eq!(counts(&seeds_report(Some(seq))), (json!(t), json!(p), json!(n), json!(c), json!(cp), json!(cn)), "M43/M44 counts as of {seq}");
    }
    // Every view replays exactly at every checkpoint: opportunities and
    // assignments are ledger rows (0063), so no later opportunity is listed.
    for (seq, seeds, report, shown) in &captured {
        let at = seq.to_string();
        let mut replay = lab.t(&["review", "seeds", "show", "--as-of", &at])["seeds"].clone();
        replay.as_object_mut().unwrap().remove("head_seq");
        assert_eq!(&replay, seeds, "seeds as of {seq}");
        assert_eq!(&seeds_report(Some(*seq)), report, "seeds report as of {seq}");
        let mut replay = lab.t(&["review", "show", "--as-of", &at]);
        for key in ["head_seq", "as_of_seq"] { replay.as_object_mut().unwrap().remove(key); }
        assert_eq!(&replay, shown, "review show as of {seq}");
    }
    let listed = |seq: usize| captured[seq].1["opportunities"].as_array().unwrap().len();
    assert_eq!((listed(0), listed(1), listed(2), listed(3)), (0, 1, 1, 6), "opportunities listed as of 6, 9, 12, 37");

    // Reveal after every review ended; a revealed candidate is never reviewed again.
    lab.t(&["review", "seeds", "reveal", &candidates[0].1]);
    lab.refused(&["review", "open", &candidates[0].1, "--kind", "code", "--protocol", "review-protocol.v1"], "revealed");
    lab.t(&["review", "seeds", "dispose", &candidates[0].1, "--disposition", "discarded"]);

    // The guard: S1 and C1 pass verification; only C1 is ever integrable.
    for task in ["bs1", "bc1"] { lab.queue_dependent(task, &task[1..]); }
    let (_, _, rs1) = lab.verify(&candidates[0].1, "verify-s1", WEAK);
    assert_eq!(lab.verify(&candidates[4].1, "verify-c1", WEAK).0, "accepted");
    lab.auto_integrate();
    assert_eq!(herdr_farm::store::service_project_integration_jobs(&lab.project).unwrap().enqueued, 1);
    let jobs: Vec<String> = lab.db().prepare("SELECT json_extract(payload,'$.submission_id') FROM operations WHERE kind='integration.run'").unwrap()
        .query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(jobs, vec![candidates[4].1.clone()], "the producer enqueues the control, never the seeded candidate");
    let target = lab.git(&["rev-parse", "refs/heads/integration"]);
    let work = lab.home.path().join("integrate-s1-work");
    let err = lab.fail(&["result", "demo", "integrate", rs1.as_ref().unwrap(), "--repository", lab.repo.to_str().unwrap(), "--idempotency-key", "integrate-s1", "--work-dir", work.to_str().unwrap()]);
    assert!(err.contains("a seeded candidate never integrates"), "{err}");
    assert_eq!((lab.git(&["rev-parse", "refs/heads/integration"]), work.exists()), (target, false));
    assert_eq!(lab.blockers("bs1"), vec!["verified_dependency_evidence_unavailable:s1:verified_result".to_owned()]);
    assert_eq!(lab.blockers("bc1"), vec!["admission_disabled:verified_result".to_owned()]);

    // A seeded arm is never a group's winner (TM3.5 finding 3). Arm 1 is a
    // seeded candidate and arm 2 a clean one; both pass their real checks.
    let digest = lab.contract("sg", &lab.base, WEAK);
    let group = lab.t(&["quality", "groups", "create", "sg", "--arm", "fast", "--arm", "slow"])["group"]["group_id"].as_str().unwrap().to_owned();
    let (class, seeded, reproducer) = seed("logic-inverted-guard");
    let (mut arm_subs, mut arm_results) = (Vec::new(), Vec::new());
    for (arm, configuration, source) in [(1u32, lab.a.clone(), seeded), (2, lab.b.clone(), format!("{clean}// clean arm\n"))] {
        let attempt = format!("sg-a{arm}");
        lab.arm("sg", &attempt, &group, arm, &configuration);
        let oid = lab.commit(&attempt, &lab.base, &source);
        let sub = lab.result("sg", &attempt, &digest, &lab.base, &oid, &[]);
        if arm == 1 { lab.t(&["review", "seeds", "register", &sub, "--seed", &format!("{class}={reproducer}")]); }
        let (state, _, result) = lab.verify(&sub, &format!("verify-{attempt}"), WEAK);
        assert_eq!(state, "accepted", "{attempt}");
        arm_subs.push(sub);
        arm_results.push(result.unwrap());
    }
    lab.queue_dependent("bsg", "sg");
    // Neither the operator nor a judge may select the seeded arm; nothing is written.
    let refused = "a seeded arm is an evaluation artefact and never a group's winner";
    lab.refused(&["quality", "groups", "select", &group, "--arm", "1", "--reason", "operator_judgment"], refused);
    lab.refused(&["quality", "groups", "select", &group, "--judge", "panel", "--submission", &arm_subs[0]], refused);
    assert_eq!(lab.count("SELECT count(*) FROM candidate_selections"), 0);
    assert_eq!(lab.blockers("bsg"), vec!["verified_dependency_evidence_unavailable:sg:verified_result".to_owned()]);
    // The rule skips the seeded arm and selects the first clean accepted arm in launch order.
    let rule = lab.t(&["quality", "groups", "select", &group, "--rule"])["selection"].clone();
    assert_eq!((&rule["outcome"], &rule["arm"], &rule["submission_id"], &rule["selector_kind"], &rule["selector_principal"], &rule["reason"]),
        (&json!("selected"), &json!(2), &json!(arm_subs[1]), &json!("rule"), &json!("rule:first_accepted_in_launch_order.v2"), &json!("first_passing_verification")));
    assert_eq!(rule["evidence"], json!([
        {"arm": 1, "attempt_id": "sg-a1", "submission_id": arm_subs[0], "verification": "accepted", "arm_outcome": "accepted", "rank": null, "rule_skip": "seeded_candidate"},
        {"arm": 2, "attempt_id": "sg-a2", "submission_id": arm_subs[1], "verification": "accepted", "arm_outcome": "accepted", "rank": 1}]));
    // The clean winner releases the dependent and integrates; the seeded arm never does.
    assert_eq!(lab.blockers("bsg"), vec!["admission_disabled:verified_result".to_owned()]);
    assert!(lab.pending_integration().contains(&arm_subs[1]), "the selected clean arm is queued for integration");
    let work = lab.home.path().join("integrate-sg-1");
    let err = lab.fail(&["result", "demo", "integrate", &arm_results[0], "--repository", lab.repo.to_str().unwrap(), "--idempotency-key", "integrate-sg-1", "--work-dir", work.to_str().unwrap()]);
    assert!(err.contains("a seeded candidate never integrates"), "{err}");
    let before = lab.git(&["rev-parse", "refs/heads/integration"]);
    let (_, commit) = lab.integrate(&arm_results[1], "integrate-sg-2");
    assert_eq!(lab.git(&["rev-parse", "refs/heads/integration"]), commit);
    assert_ne!(commit, before);
    assert_eq!(lab.count("SELECT count(*) FROM integration_operations WHERE state='integrated'"), 1);
    assert_eq!(herdr_farm::store::service_project_integration_jobs(&lab.project).unwrap().enqueued, 0, "nothing else of the group is integrable");
    // Last: the fixture's minimal launch records are not a full snapshot.
    lab.started("s1", "s1-attempt");
    lab.completion_refused("s1", "a seeded candidate never completes its task");
}

/// Doc 06 §6 / doc 07 M28 on the real candidate X1, with a severity floor.
/// By hand: 1 registers `skeptical-floor.v1` (min severity medium); O1 (code,
/// rev-1) is opened at 2, assigned 3, runs at 4–5 with finding `k` (sub 1,
/// 6), validated as K at 7 (`finding:canonical-7`). Pass P1 is opened at 8
/// and bound at 9 after O1 (cutoff 8: K known), assigned 10, runs 11–12 and
/// reports `new`, `again`, `minor` (subs 2–4 at 13–15). While triage is
/// pending P1 is excluded (`pending_triage`): M28 has no denominator. 16
/// validates `new` as N, 17 validates `again` as K (a rediscovery), 18
/// validates `minor` as low (below the floor): M28 = 1/1, rediscovered 1.
/// P2 (opened 19, bound 20, assigned 21) times out (22–24, its finding
/// rejected at 25): `not_completed`, no yield. P3 on the later candidate X2,
/// opened 26 and bound at 27 after O1: `changed_artifact`. 28 retracts P1's
/// binding: M28 has no eligible pass (null, never 0); as of 18 it is still
/// 1/1, and P2 and P3 are not yet listed.
#[test]
fn skeptical_yield_counts_only_new_findings_on_the_same_artifact() {
    let lab = Lab::new();
    let (a, c) = (lab.a.clone(), lab.c.clone());
    let dx = lab.contract("x", &lab.base, WEAK);
    lab.attempt("x", "x-attempt", Some(&a));
    let x1 = lab.commit("x1", &lab.base, DEFECT_SRC);
    let sx = lab.result("x", "x-attempt", &dx, &lab.base, &x1, &[]);
    let protocol = json!({"schema": "review_protocol.v1", "protocol": "skeptical-floor.v1", "kind": "skeptical", "scope": "candidate_diff", "role": "evaluation",
        "challenges": ["unsupported_claims", "missed_edge_cases"], "failure_classes": ["logic", "boundary"], "permitted_tools": ["read"], "budget_ms": 600_000,
        "evidence_min": 1, "stopping_rule": "checklist_complete", "prior_disclosure": "withheld", "reviewer_profile": null,
        "outcome": {"primary": "new_validated_unique_findings.v1", "adjudication": "owner_triage.v1", "severity_policy": "finding_severity.v1", "min_severity": "medium"}});
    let file = lab.home.path().join("protocol.json");
    fs::write(&file, protocol.to_string()).unwrap();
    assert_eq!(lab.t(&["review", "protocols", "register", "--input-file", file.to_str().unwrap()])["event"]["seq"], json!(1));
    let (o1, s1) = lab.review(&sx, "code", "claude", "rev-1", &c);
    lab.complete(&s1, &sx, &x1, "completed", json!(["finding:k"]));
    lab.triage(&["validate", "1", "--new", "--severity", "high", "--evidence", &evidence('a')]);
    let pass = |submission: &str| {
        let o = lab.t(&["review", "open", submission, "--kind", "skeptical", "--protocol", "skeptical-floor.v1", "--budget-ms", "600000"])["opportunity"]["opportunity_id"].as_str().unwrap().to_owned();
        let bound = lab.t(&["review", "protocols", "bind", &o, "--prior", &o1])["event"].clone();
        (o, bound)
    };
    let (p1, bound) = pass(&sx);
    assert_eq!((&bound["seq"], &bound["subject"]["cutoff_seq"], &bound["subject"]["comparability"], &bound["subject"]["prior_coverage"]), (&json!(9), &json!(8), &json!("same_artifact"), &json!("complete")));
    lab.t(&["review", "assign", &p1, "--reviewer", "fast"]);
    lab.attempt("reviews", "rev-p1", Some(&a));
    let sp1 = lab.t(&["review", "start", &p1, "--attempt", "rev-p1"])["session"]["session_id"].as_str().unwrap().to_owned();
    lab.complete(&sp1, &sx, &x1, "completed", json!(["finding:new", "finding:again", "finding:minor"]));
    let m28 = lab.metrics()["M28"].clone();
    assert_eq!((&m28["value"], &m28["excluded"]), (&json!(null), &json!({"pending_triage": 1})), "{m28}");
    lab.triage(&["validate", "2", "--new", "--severity", "high", "--evidence", &evidence('b')]);
    lab.triage(&["validate", "3", "--finding", "finding:canonical-7", "--severity", "high", "--evidence", &evidence('c')]);
    lab.triage(&["validate", "4", "--new", "--severity", "low", "--evidence", &evidence('d')]);
    let shown = lab.t(&["review", "protocols", "show"])["protocols"].clone();
    let p = shown["passes"].as_array().unwrap().iter().find(|x| x["opportunity_id"] == p1).unwrap().clone();
    assert_eq!(p["claims"].as_array().unwrap().iter().map(|c| c["incremental"].as_str().unwrap()).collect::<Vec<_>>(), ["new", "rediscovered", "below_severity_floor"]);
    let m28 = lab.metrics()["M28"].clone();
    assert_eq!((&m28["value"], &m28["rediscovered"], &m28["estimate"], &m28["observational"]), (&json!("1/1"), &json!(1), &json!("descriptive"), &json!(true)));
    let at18 = lab.t(&["review", "protocols", "show"])["protocols"].clone();

    // A timed-out pass has no yield; a pass on a later candidate is not comparable.
    let (p2, _) = pass(&sx);
    lab.t(&["review", "assign", &p2, "--reviewer", "fast"]);
    lab.attempt("reviews", "rev-p2", Some(&a));
    let sp2 = lab.t(&["review", "start", &p2, "--attempt", "rev-p2"])["session"]["session_id"].as_str().unwrap().to_owned();
    lab.complete(&sp2, &sx, &x1, "timed_out", json!(["finding:late"]));
    lab.triage(&["reject", "5", "--reason", "insufficient_evidence"]);
    let x2 = lab.commit("x2", &x1, &format!("{DEFECT_SRC}// x2\n"));
    let sx2 = lab.result("x", "x-attempt", &dx, &lab.base, &x2, &[]);
    let (p3, bound) = pass(&sx2);
    assert_eq!((&bound["seq"], &bound["subject"]["comparability"]), (&json!(27), &json!("changed_artifact")));
    assert_ne!(p3, p1, "a changed artifact is another opportunity");
    let m28 = lab.metrics()["M28"].clone();
    assert_eq!((&m28["value"], &m28["excluded"]), (&json!("1/1"), &json!({"changed_artifact": 1, "not_completed": 1})));
    // The owner retracts P1's binding: no eligible pass remains; history keeps it.
    assert_eq!(lab.t(&["review", "protocols", "retract", "9"])["event"]["seq"], json!(28));
    let m28 = lab.metrics()["M28"].clone();
    assert_eq!((&m28["value"], &m28["reason"], &m28["excluded"]), (&json!(null), &json!("empty_denominator"), &json!({"changed_artifact": 1, "not_completed": 1, "retracted": 1})));
    let mut replay = lab.t(&["review", "protocols", "show", "--as-of", "18"])["protocols"].clone();
    let mut expected = at18.clone();
    for v in [&mut replay, &mut expected] { v.as_object_mut().unwrap().remove("head_seq"); }
    assert_eq!(replay, expected, "as of 18 the protocol view replays exactly");
}

/// Doc 10 §5 second denominator fixture, with real repair candidates. O1
/// (rev-1) is opened, assigned, started and completed with P, Q, R, S (seq
/// 1–8), validated at 9–12. By hand:
///
/// | repair | finding | initial group | history                                    | cohort cell        |
/// |--------|---------|---------------|--------------------------------------------|--------------------|
/// | 13     | P       | A (`fast`)    | p-a1's candidate fails its real check; 16 closes `no_fix` | A: not achieved |
/// | 17     | Q       | A             | q-a1 (A) fails; q-b1 (B) reassigned 19, verified, integrated, closed `fixed` | A: achieved |
/// | 24     | R       | B (`slow`)    | open, horizon 1 day: censored              | B: censored        |
/// | 25     | S       | C (`claude`)  | 26 closes `cancelled`                      | C: not achieved    |
///
/// A's M25 = M26 = 1/2 (the failed repair stays); B has only a censored
/// opportunity (null, `empty_denominator`, never 100%) although B implemented
/// Q's fix; C 0/1. Finding outcomes: M25 = M26 = 1/4. Q's implementation is
/// mixed and unallocated until 27 credits q-b1 fully; B's cohort is unchanged:
/// earned credit never defines eligibility.
#[test]
fn repair_cohorts_keep_failed_cancelled_and_reassigned_opportunities() {
    let lab = Lab::new();
    lab.configure_integration();
    let (a, b, c) = (lab.a.clone(), lab.b.clone(), lab.c.clone());
    let dx = lab.contract("x", &lab.base, WEAK);
    lab.attempt("x", "x-attempt", Some(&a));
    let x1 = lab.commit("x1", &lab.base, DEFECT_SRC);
    let sx = lab.result("x", "x-attempt", &dx, &lab.base, &x1, &[]);
    let (_, s1) = lab.review(&sx, "code", "claude", "rev-1", &c);
    lab.complete(&s1, &sx, &x1, "completed", json!(["finding:p", "finding:q", "finding:r", "finding:s"]));
    for claim in ["1", "2", "3", "4"] { lab.triage(&["validate", claim, "--new", "--severity", "medium", "--evidence", &evidence('a')]); }
    let (p, q, r, s) = ("finding:canonical-9", "finding:canonical-10", "finding:canonical-11", "finding:canonical-12");

    let dp = lab.contract("fix-p", &lab.base, GUARDED);
    lab.attempt("fix-p", "p-a1", Some(&a));
    assert_eq!(lab.fix(&["open", p, "--assign", "fast"])["seq"], json!(13));
    lab.fix(&["bind", "13", "--attempt", "p-a1"]);
    let wrong = lab.commit("p-wrong", &lab.base, D2_SRC);
    let sp = lab.result("fix-p", "p-a1", &dp, &lab.base, &wrong, &["fixed"]);
    let (state, rp, _) = lab.verify(&sp, "verify-p", GUARDED);
    assert_eq!(state, "rejected");
    lab.fix(&["propose", "13", "--submission", &sp]);
    lab.refused(&["review", "fixes", "verify", "15", "--run", &rp, "--assurance", "regression_reproduced", "--evidence", &evidence('b')], "was rejected");
    lab.refused(&["review", "fixes", "close", "13", "--outcome", "fixed"], "has no verified fix");
    lab.fix(&["close", "13", "--outcome", "no_fix"]);

    let dq = lab.contract("fix-q", &lab.base, GUARDED);
    lab.attempt("fix-q", "q-a1", Some(&a));
    lab.attempt("fix-q", "q-b1", Some(&b));
    assert_eq!(lab.fix(&["open", q, "--assign", "fast"])["seq"], json!(17));
    lab.fix(&["bind", "17", "--attempt", "q-a1"]);
    lab.fix(&["bind", "17", "--attempt", "q-b1"]);
    let fixed = lab.commit("q-fix", &lab.base, D1_SRC);
    let sq = lab.result("fix-q", "q-b1", &dq, &lab.base, &fixed, &["fixed"]);
    let (state, rq, rq_result) = lab.verify(&sq, "verify-q", GUARDED);
    assert_eq!(state, "accepted");
    lab.fix(&["propose", "17", "--submission", &sq]);
    lab.fix(&["verify", "20", "--run", &rq, "--assurance", "regression_reproduced", "--evidence", &evidence('c')]);
    let (iq, _) = lab.integrate(&rq_result.unwrap(), "integrate-q");
    lab.fix(&["integrate", "20", "--integrated", &iq]);
    assert_eq!(lab.fix(&["close", "17", "--outcome", "fixed"])["seq"], json!(23));
    assert_eq!(lab.fix(&["open", r, "--assign", "slow", "--horizon-days", "1"])["seq"], json!(24));
    lab.fix(&["open", s, "--assign", "claude"]);
    lab.refused(&["review", "fixes", "close", "25", "--outcome", "fixed"], "has no verified fix");
    assert_eq!(lab.fix(&["close", "25", "--outcome", "cancelled"])["seq"], json!(26));

    let m = lab.metrics();
    let expected = |achieved: usize| json!({
        a.as_str(): {"numerator": achieved, "denominator": 2, "value": format!("{achieved}/2"), "not_achieved": 2 - achieved, "reassigned": 1, "censored": 0},
        b.as_str(): {"numerator": 0, "denominator": 0, "value": null, "not_achieved": 0, "reassigned": 0, "censored": 1, "reason": "empty_denominator"},
        c.as_str(): {"numerator": 0, "denominator": 1, "value": "0/1", "not_achieved": 1, "reassigned": 0, "censored": 0}});
    assert_eq!((&m["M25"]["by_assignment"], &m["M26"]["by_assignment"]), (&expected(1), &expected(1)));
    assert_eq!((&m["M25"]["value"], &m["M26"]["value"], &m["M25"]["censored"]), (&json!("1/4"), &json!("1/4"), &json!(1)));
    let fixes = lab.fixes(None);
    let repair = |seq: i64| fixes["repairs"].as_array().unwrap().iter().find(|x| x["repair_seq"] == seq).unwrap().clone();
    let history: Vec<(Value, Value, Value)> = repair(17)["attempts"].as_array().unwrap().iter().map(|t| (t["attempt_id"].clone(), t["configuration_id"].clone(), t["reassignment"].clone())).collect();
    assert_eq!(history, [(json!("q-a1"), json!(a), json!(false)), (json!("q-b1"), json!(b), json!(true))], "effective history is kept, the cohort stays A");
    assert_eq!((&repair(17)["configuration_id"], &repair(17)["outcome"]), (&json!(a), &json!("currently_resolved")));
    assert_eq!((&repair(13)["closure"], &repair(13)["outcome"], &repair(25)["closure"], &repair(24)["closure"]), (&json!("no_fix"), &json!("proposed"), &json!("cancelled"), &json!(null)));
    let qf = fixes["findings"].as_array().unwrap().iter().find(|f| f["finding_id"] == q).unwrap().clone();
    assert_eq!((&qf["implementation"]["allocated"], &qf["implementation"]["unallocated_reason"]), (&json!("0"), &json!("mixed_contribution_unallocated")));

    lab.fix(&["credit", q, "--role", "implementation", "--proposal", "20", "--share", "q-b1=1", "--evidence", &evidence('d')]);
    let qf = lab.fixes(None)["findings"].as_array().unwrap().iter().find(|f| f["finding_id"] == q).unwrap().clone();
    assert_eq!((&qf["implementation"]["shares"][0]["attempt_id"], &qf["implementation"]["shares"][0]["configuration_id"], &qf["implementation"]["allocated"]), (&json!("q-b1"), &json!(b), &json!("1")));
    let m = lab.metrics();
    assert_eq!(m["M25"]["by_assignment"], expected(1), "earned credit never moves an opportunity into the fixer's cohort");
    assert_attribution_reconciles(&lab.fixes(None), &m);
}

#[path = "quality_certification/worker_lab.rs"]
mod worker_lab;

/// A malicious reviewing worker, launched by the ticker in its sandbox. It
/// waits for the owner's `plan.txt`, then tries to elevate its own report
/// and publishes each outcome as `<name>.txt` (`true|false`, then output).
const MALICIOUS_REVIEWER: &str = r##"
use std::{fs, path::Path, process::Command, time::Duration};
fn publish(name: &str, text: &str) { fs::write(format!("{name}.tmp"), text).unwrap(); fs::rename(format!("{name}.tmp"), format!("{name}.txt")).unwrap(); }
fn field(json: &str, key: &str) -> String {
    let marker = format!("\"{key}\": \"");
    json.find(&marker).map(|i| json[i + marker.len()..].split('"').next().unwrap().to_owned()).unwrap_or_default()
}
fn hp(args: &[&str]) -> String {
    let out = Command::new(BIN).args(["--root", ROOT, "telemetry", "demo"]).args(args).output().unwrap();
    format!("{}\n{}{}", out.status.success(), String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
}
/// The same command after shedding both worker markers (another directory, another HOME).
fn evading(args: &[&str]) -> String {
    let out = Command::new(BIN).current_dir("/").env("HOME", "/tmp").args(["--root", ROOT, "telemetry", "demo"]).args(args).output().unwrap();
    format!("{}\n{}{}", out.status.success(), String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
}
fn main() {
    if std::env::args().nth(1).as_deref() == Some("--version") { println!("2.1.0 (Claude Code)"); return }
    let spool = std::env::var("HERDR_PROJECTS_SUBMISSION_SPOOL").unwrap_or_default();
    let attempt = Path::new(&spool).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    publish("ready", "");
    while !Path::new("plan.txt").exists() { std::thread::sleep(Duration::from_millis(100)); }
    let plan = fs::read_to_string("plan.txt").unwrap();
    let get = |k: &str| plan.lines().find_map(|l| l.strip_prefix(&format!("{k}="))).unwrap().to_owned();
    let session = hp(&["review", "session", "--attempt", &attempt]);
    publish("session", &session);
    let (sid, sub, oid) = (field(&session, "session_id"), field(&session, "submission_id"), field(&session, "candidate_oid"));
    fs::write("receipt.json", format!("{{\"schema\":\"review_receipt.v1\",\"session_id\":\"{sid}\",\"submission_id\":\"{sub}\",\"candidate_oid\":\"{oid}\",\"outcome\":\"completed\",\"findings\":[{{\"ref\":\"finding:self\",\"title\":\"Critical defect, verified by me\"}}],\"evidence\":[]}}")).unwrap();
    publish("submit", &hp(&["review", "submit", "--input-file", "receipt.json"]));
    let evidence = format!("sha256:{}", "a".repeat(64));
    publish("validate", &hp(&["review", "findings", "validate", "1", "--new", "--severity", "critical", "--evidence", &evidence]));
    publish("split", &hp(&["review", "findings", "split", "1", "--claim", "a", "--claim", "b"]));
    publish("accept", &hp(&["review", "accept", &sid, "--document", "receipt.json", "--signature", "receipt.json"]));
    publish("complete", &hp(&["review", "complete", "--input-file", "receipt.json"]));
    publish("fixes", &hp(&["review", "fixes", "open", "finding:canonical-1", "--unassigned"]));
    let other = get("other_session");
    fs::write("other.json", format!("{{\"schema\":\"review_receipt.v1\",\"session_id\":\"{other}\",\"submission_id\":\"{sub}\",\"candidate_oid\":\"{oid}\",\"outcome\":\"completed\",\"findings\":[\"finding:forged\"],\"evidence\":[]}}")).unwrap();
    publish("foreign-submit", &hp(&["review", "submit", "--input-file", "other.json"]));
    publish("foreign-session", &hp(&["review", "session", "--attempt", &get("other_attempt")]));
    let name = get("planted_name");
    fs::copy("planted.request", format!("{spool}/.planted")).unwrap();
    fs::rename(format!("{spool}/.planted"), format!("{spool}/{name}.request")).unwrap();
    let receipt = format!("{spool}/{name}.receipt");
    while !Path::new(&receipt).exists() { std::thread::sleep(Duration::from_millis(100)); }
    publish("planted", &fs::read_to_string(&receipt).unwrap());
    publish("seed-register", &hp(&["review", "seeds", "register", &sub, "--control"]));
    publish("seed-reveal", &hp(&["review", "seeds", "reveal", &sub]));
    publish("groups", &hp(&["quality", "groups", "select", &get("group"), "--arm", "1"]));
    let store = format!("{ROOT}/demo/.state/state.db");
    publish("validate-evading", &evading(&["review", "findings", "validate", "1", "--new", "--severity", "critical", "--evidence", &evidence]));
    publish("store-write", &match fs::OpenOptions::new().write(true).open(&store) { Ok(_) => "OK".into(), Err(e) => format!("ERR:{:?}", e.kind()) });
    publish("done", "");
    loop { std::thread::park() }
}
"##;

/// The card's malicious worker, over the real isolated launch and spool
/// path. A review task is launched from its blind brief; inside its sandbox
/// the reviewing worker submits its own receipt (a proposal, recorded as
/// `worker:<attempt>`), then tries to (a) triage and split its own finding,
/// (b) accept or complete its own review, open a repair, (c) submit a
/// receipt for another attempt's session, read that attempt's session, and
/// plant a spool request naming another attempt, (d) register and reveal
/// seeds, select a candidate-group arm, and write the store directly. Every
/// elevation is refused (worker markers, the spool's own-work check, the
/// store's launch binding, a read-only store) and writes nothing: the only
/// ledger rows are the two opportunities' openings and assignments, the
/// launched session's start, the owner's other session, the worker's
/// completion and its one pending submission (head 8). By hand: M22 has
/// no adjudicated submission (null, 1 pending), M24 none closed, until the
/// owner triages.
#[test]
fn a_sandboxed_worker_cannot_elevate_its_own_report() {
    use worker_lab::*;
    let mut lab = WorkerLab::new();
    lab.write_agent(&format!("{MALICIOUS_REVIEWER}\nconst ROOT: &str = {:?};\nconst BIN: &str = {BIN:?};\n", lab.path("root").canonicalize().unwrap().display().to_string()));
    let world = lab.review_world();
    let attempt = lab.reserve_selection(&world.selection);
    // The owner's own review session of another opportunity, by another attempt (never launched).
    let o2 = lab.ok(&["telemetry", "demo", "review", "open", &world.submission, "--kind", "security", "--protocol", "review-protocol.v1"])["opportunity"]["opportunity_id"].as_str().unwrap().to_owned();
    lab.ok(&["telemetry", "demo", "review", "assign", &o2, "--reviewer", "worker"]);
    rusqlite::Connection::open(lab.project.join(".state/state.db")).unwrap()
        .execute("INSERT INTO attempts(id,task_id,revision,state,reservation,termination_observed) VALUES('other-attempt','authored',1,'completed','other-attempt',1)", []).unwrap();
    let other = lab.ok(&["telemetry", "demo", "review", "start", &o2, "--attempt", "other-attempt"])["session"]["session_id"].as_str().unwrap().to_owned();
    let (planted_name, planted) = spool_request("review_submit", "other-attempt", "{}");
    let worktree = lab.planned_worktree(&attempt);
    lab.serve();
    let mut ticker = lab.spawn();
    // Nothing is written into the worktree before the launched worker runs (the launch checks its checkout).
    lab.wait(&mut ticker, 180, &|| worktree.join("ready.txt").exists());
    fs::write(worktree.join("planted.request"), &planted).unwrap();
    fs::write(worktree.join("plan.tmp"), format!("other_session={other}\nother_attempt=other-attempt\nplanted_name={planted_name}\ngroup=sha256:{}\n", "0".repeat(64))).unwrap();
    fs::rename(worktree.join("plan.tmp"), worktree.join("plan.txt")).unwrap();
    let started = std::time::Instant::now();
    while !worktree.join("done.txt").exists() {
        if started.elapsed() > std::time::Duration::from_secs(300) {
            let listing: Vec<String> = fs::read_dir(&worktree).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
            panic!("the probe did not finish: {listing:?}\n{}", fs::read_to_string(lab.path("root/.ticker.log")).unwrap_or_default().lines().filter(|l| !l.contains("owns lock")).take(60).collect::<Vec<_>>().join("\n"));
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    lab.stop(ticker);
    let read = |name: &str| -> (bool, String) {
        let text = fs::read_to_string(worktree.join(format!("{name}.txt"))).unwrap();
        let (status, rest) = text.split_once('\n').unwrap_or((text.as_str(), ""));
        (status == "true", rest.to_owned())
    };
    let (ok, session) = read("session");
    assert!(ok, "{session}");
    let launched = serde_json::from_str::<Value>(&session).unwrap()["session"].clone();
    assert_eq!((&launched["opportunity_id"], &launched["submission_id"], &launched["candidate_oid"]), (&json!(world.opportunity), &json!(world.submission), &json!(world.candidate)));
    let (ok, submitted) = read("submit");
    assert!(ok, "{submitted}");
    let completion = serde_json::from_str::<Value>(&submitted).unwrap()["completion"].clone();
    assert_eq!((&completion["trust"], &completion["coverage_basis"], &completion["recorder_principal"]), (&json!("proposal"), &json!("declared"), &json!(format!("worker:{}", attempt.as_str()))));
    // The first marker that holds is named: the working directory is its task worktree (HOME is its execution home too).
    let context = "refuses to run inside a worker execution context: the working directory is a task worktree";
    for (name, reason) in [("validate", context), ("split", context), ("accept", context), ("complete", context), ("fixes", context), ("seed-register", context), ("seed-reveal", context),
        ("groups", "`quality groups` records the project owner (operator:cli) and refuses to run inside a worker execution context: the working directory is a task worktree"),
        ("foreign-submit", "the worker receipt channel takes receipts only for review sessions recorded at launch"),
        ("foreign-session", "submission spool refused the request: review session asks for another attempt's session")] {
        let (ok, out) = read(name);
        assert!(!ok && out.contains(reason), "{name}: {out}");
    }
    // Markers are not authority: with both shed, the read-only store still refuses every write.
    let (ok, out) = read("validate-evading");
    assert!(!ok && out.contains("attempt to write a readonly database"), "{out}");
    let raw = |name: &str| fs::read_to_string(worktree.join(format!("{name}.txt"))).unwrap();
    let planted: Value = serde_json::from_str(&raw("planted")).unwrap();
    assert_eq!(planted["error"], json!("submission spool refused the request: the request names another attempt than its spool"), "{planted}");
    assert_eq!(raw("store-write"), "ERR:ReadOnlyFilesystem", "the project store is read-only in the sandbox");
    let denials = lab.spool_denials(attempt.as_str());
    for reason in ["the request names another attempt than its spool", "review session asks for another attempt's session"] {
        assert!(denials.iter().any(|d| d == reason), "{reason}: {denials:?}");
    }

    // Nothing was elevated: the worker's report is one pending proposal.
    let db = rusqlite::Connection::open(lab.project.join(".state/state.db")).unwrap();
    let count = |sql: &str| db.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
    for table in ["finding_decisions", "review_acceptances", "seeded_candidates", "candidate_selections", "repair_opportunities", "finding_claim_sets WHERE revision>1"] {
        assert_eq!(count(&format!("SELECT count(*) FROM {table}")), 0, "{table}");
    }
    assert_eq!(count(&format!("SELECT count(*) FROM review_completions WHERE session_id='{other}'")), 0, "the other attempt's session has no completion");
    let findings = lab.ok(&["telemetry", "demo", "review", "findings", "show"])["findings"].clone();
    assert_eq!((&findings["summary"]["submissions"], &findings["summary"]["pending"], &findings["unique_findings"], &findings["head_seq"]), (&json!(1), &json!(1), &json!(0), &json!(8)));
    assert_eq!(findings["submissions"][0]["trust"], json!("proposal"));
    let m = lab.ok(&["telemetry", "demo", "review", "report"])["metrics"].clone();
    assert_eq!((&m["M22"]["value"], &m["M22"]["pending"], &m["M21"]["value"], &m["M24"]["opportunities"]["closed"]), (&json!(null), &json!(1), &json!("0"), &json!(0)));
    // Only the owner's triage makes it count.
    lab.ok(&["telemetry", "demo", "review", "findings", "reject", "1", "--reason", "insufficient_evidence"]);
    assert_eq!(lab.ok(&["telemetry", "demo", "review", "report"])["metrics"]["M22"]["value"], json!("0/1"));
}

//! `doctor`: what is installed, where things resolve, and whether it fits.

use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;

use crate::herdr::{self, Herdr};
use crate::paths::{self, Ctx, Env, SessionFlags};
use crate::project;
use crate::runner::{Cmd, Runner};

const TOOL_TIMEOUT: Duration = Duration::from_secs(10);

/// Prints the report and returns whether every required check passed.
pub fn run(ctx: &Ctx, session: &SessionFlags) -> Result<bool> {
    let (text, healthy) = report(ctx.env, &ctx.root, &ctx.config_dir, session, ctx.runner);
    print!("{text}");
    Ok(healthy)
}

fn report(
    env: &Env,
    root: &Path,
    config_dir: &Path,
    session: &SessionFlags,
    runner: &dyn Runner,
) -> (String, bool) {
    let mut out = String::new();
    let mut healthy = true;
    let mut check = |out: &mut String, ok: Option<bool>, label: &str, detail: String| {
        let mark = match ok {
            Some(true) => "ok  ",
            Some(false) => {
                healthy = false;
                "FAIL"
            }
            None => "warn",
        };
        let _ = writeln!(out, "[{mark}] {label}: {detail}");
    };

    let binary = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|e| format!("unknown ({e})"));
    let _ = writeln!(out, "binary:     {binary}");
    let _ = writeln!(out, "version:    {}", crate::VERSION);
    let _ = writeln!(out, "root:       {}", root.display());
    let _ = writeln!(out, "config dir: {}", config_dir.display());
    let _ = writeln!(out);

    let bin = env.herdr_bin();
    match herdr::version(&bin, runner) {
        Ok(version) if version >= herdr::MIN_VERSION => {
            check(&mut out, Some(true), "herdr", format!("{version} ({bin})"))
        }
        Ok(version) => check(
            &mut out,
            Some(false),
            "herdr",
            format!("{version} ({bin}); {} or later is required", herdr::MIN_VERSION),
        ),
        Err(error) => check(&mut out, Some(false), "herdr", format!("{error:#}")),
    }

    match paths::resolve_session(session, env, runner) {
        Ok(found) => {
            let reachable = Herdr::new(&bin, &found.socket, runner).reachable();
            let name = found.name.as_deref().unwrap_or("-");
            check(
                &mut out,
                if reachable { Some(true) } else { None },
                "session",
                format!(
                    "{} (name: {name}){}",
                    found.socket.display(),
                    if reachable { "" } else { "; not reachable" }
                ),
            );
        }
        Err(error) => check(&mut out, Some(false), "session", format!("{error:#}")),
    }

    for (tool, args, required) in [
        ("git", vec!["--version"], true),
        ("ssh", vec!["-V"], true),
        ("rsync", vec!["--version"], false),
        ("gh", vec!["--version"], false),
    ] {
        let result = runner.run(&Cmd::new(tool, TOOL_TIMEOUT).args(args));
        match result {
            Ok(o) if o.success() => {
                let text = if o.stdout.trim().is_empty() { &o.stderr } else { &o.stdout };
                let line = text.lines().next().unwrap_or("").trim().to_string();
                check(&mut out, Some(true), tool, line);
            }
            Ok(o) => check(&mut out, required.then_some(false), tool, o.error_text()),
            Err(error) => check(&mut out, required.then_some(false), tool, format!("{error:#}")),
        }
    }
    match runner.run(&Cmd::new("gh", TOOL_TIMEOUT).args(["auth", "status"])) {
        Ok(o) if o.success() => check(&mut out, Some(true), "gh auth", "logged in".into()),
        Ok(o) => check(
            &mut out,
            None,
            "gh auth",
            format!(
                "{}; pull request follow-up will not work",
                o.error_text().lines().next().unwrap_or("not logged in")
            ),
        ),
        Err(_) => check(&mut out, None, "gh auth", "gh is not installed".into()),
    }

    if root.is_dir() {
        let count = project::list_slugs(root).len();
        check(&mut out, Some(true), "root", format!("{count} project(s)"));
    } else {
        check(
            &mut out,
            None,
            "root",
            "does not exist yet; `new` creates it".into(),
        );
    }

    match crate::ticker::lock_state(root) {
        crate::ticker::LockState::Free => check(&mut out, None, "ticker", "not running".into()),
        crate::ticker::LockState::Held(info) => check(
            &mut out,
            Some(true),
            "ticker",
            format!(
                "running, version {} (this binary: {}), root {}",
                info.version,
                crate::VERSION,
                info.root
            ),
        ),
    }
    match crate::ticker::metrics_state(root) {
        crate::ticker::MetricsFile::Present(metrics) => {
            let running = matches!(crate::ticker::lock_state(root), crate::ticker::LockState::Held(_));
            check(
                &mut out,
                if running { Some(true) } else { None },
                "executor",
                format!(
                    "control q={} r={} hw={} done={}; transfer q={} r={} hw={} done={}; delay_ms={}; uncertain={}",
                    metrics.control.queued, metrics.control.running, metrics.control.high_water, metrics.control.completed,
                    metrics.transfer.queued, metrics.transfer.running, metrics.transfer.high_water, metrics.transfer.completed,
                    metrics.max_queue_delay_ms, metrics.uncertain
                ),
            );
        }
        crate::ticker::MetricsFile::Absent => {
            if matches!(crate::ticker::lock_state(root), crate::ticker::LockState::Held(_)) {
                check(&mut out, None, "executor", "metrics unavailable".into());
            }
        }
        crate::ticker::MetricsFile::Invalid => check(&mut out, None, "executor", "metrics unreadable".into()),
    }

    for slug in project::list_slugs(root) {
        let Ok(project) = project::Project::load(root, &slug) else {
            continue;
        };
        let label = format!("project {slug}");
        if let Err(error) = project.try_status() { check(&mut out, Some(false), &label, format!("lifecycle record: {error:#}")); }
        for diagnostic in crate::thread::list_with_diagnostics(&project).1 {
            check(&mut out, Some(false), &label, format!("thread record: {diagnostic}; preserve and repair the file"));
        }
        for diagnostic in crate::inbox::unhandled_with_diagnostics(&project).1 {
            check(&mut out, Some(false), &label, format!("inbox record: {diagnostic}; preserve and repair the file"));
        }
        #[cfg(feature="state-store")]
        if project::ensure_legacy(&project.dir()).is_err() {
            match herdr_projects::runtime::checkpoint_sizes(&project.dir()) {
                Ok(Some(sizes)) => check(&mut out, Some(true), &label, format!(
                    "coordinator checkpoint {} full_chars={} delta_chars={} created_unix_ms={}",
                    sizes.checkpoint_id, sizes.full_chars, sizes.delta_chars, sizes.created_unix_ms
                )),
                Ok(None) => {},
                Err(_) => {},
            }
        }
        let retry_path = project.state_dir().join("ticker.json");
        match std::fs::read(&retry_path) {
            Ok(bytes) => match serde_json::from_slice::<crate::steps::State>(&bytes) {
                Ok(state) => {
                    let count = state.finalizations.len() + state.pending_events.len();
                    if count > 0 || !state.notification_retry.hash.is_empty() {
                        check(&mut out, None, &label, format!(
                            "{} pending finalization(s), {} pending inbox event(s), notification retry {}; state: {}",
                            state.finalizations.len(), state.pending_events.len(),
                            if state.notification_retry.hash.is_empty() { "none" } else { "pending" }, retry_path.display()));
                    }
                    for (id, pending) in &state.finalizations {
                        check(&mut out, None, &format!("{label} thread {id}"), format!(
                            "final copy attempt {}; next {}; {}", pending.retry.attempts,
                            pending.retry.next_attempt, crate::pr::sanitize(&pending.retry.last_error)));
                    }
                    if !state.notification_retry.retry.last_error.is_empty() {
                        check(&mut out, None, &label, format!("notification: {}; next {}",
                            crate::pr::sanitize(&state.notification_retry.retry.last_error), state.notification_retry.retry.next_attempt));
                    }
                    if let Some(claim)=&state.notification_claim {
                        let count=claim.batch.as_ref().map_or_else(||"unknown legacy batch".to_string(),|b|format!("{} item(s)",b.ids.len()));
                        check(&mut out,None,&label,format!("notification sequence {}: {:?}, {}; inspect with `notification {} inspect`; reconcile with `notification {} acknowledge --sequence {}` or `notification {} retry --sequence {} --accept-possible-duplicate`",claim.sequence,claim.phase,count,project.slug,project.slug,claim.sequence,project.slug,claim.sequence));
                    }
                    if !state.gh_outages.is_empty() || !state.machine_outages.is_empty() {
                        check(&mut out, None, &label, format!("{} GitHub resource outage(s), {} machine outage(s)", state.gh_outages.len(), state.machine_outages.len()));
                    }
                }
                Err(error) => check(&mut out, Some(false), &label, format!("invalid retry state {}: {error}", retry_path.display())),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
            Err(error) => check(&mut out, Some(false), &label, format!("cannot read retry state {}: {error}", retry_path.display())),
        }
        let Some(record) = project.coordinator() else {
            check(&mut out, Some(true), &label, format!("{}; never opened", project.status()));
            continue;
        };
        if !Path::new(&record.socket).exists() {
            check(&mut out, None, &label, format!("recorded socket {} no longer exists; `open --rebind` moves it", record.socket));
            continue;
        }
        let herdr = Herdr::new(&bin, &record.socket, runner);
        match herdr.pane_list() {
            Err(error) => check(&mut out, None, &label, format!("session at {} unreachable: {error}", record.socket)),
            Ok(panes) => {
                let workspace = panes.iter().any(|p| p.workspace_id == record.workspace_id);
                let pane = panes.iter().any(|p| crate::coordinator::pane_matches(&record, p));
                check(
                    &mut out,
                    if pane { Some(true) } else { None },
                    &label,
                    format!(
                        "{}; socket {}; workspace {} {}; coordinator pane {} {}",
                        project.status(),
                        record.socket,
                        record.workspace_id,
                        if workspace { "exists" } else { "is gone" },
                        record.pane_id,
                        if pane { "exists" } else { "is gone (run `open`)" },
                    ),
                );
            }
        }
    }

    // Machines that projects use need an SSH target for report and library copies.
    let mut machines = std::collections::BTreeSet::new();
    for slug in project::list_slugs(root) {
        let Ok(project) = project::Project::load(root, &slug) else {
            continue;
        };
        if let Ok((settings, _)) = project.read_project_md() {
            machines.extend(settings.repos.into_iter().filter_map(|r| r.machine));
        }
        machines.extend(crate::thread::list(&project).into_iter().filter(|t| t.is_remote() && t.status != crate::thread::Status::Resolved).map(|t| t.machine));
    }
    for machine in machines {
        match crate::remote::ssh_target(runner, &bin, config_dir, &machine) {
            Ok(target) => check(&mut out, Some(true), &format!("machine {machine}"), format!("ssh target {target}")),
            Err(error) => check(&mut out, Some(false), &format!("machine {machine}"), format!("{error:#}")),
        }
    }

    (out, healthy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::fake::{FakeRunner, fail, ok};

    fn runner_with_herdr(version: &str) -> FakeRunner {
        let runner = FakeRunner::new();
        runner.on("herdr --version", ok(version));
        runner.on("session list --json", ok(r#"{"sessions":[]}"#));
        runner.on("git --version", ok("git version 2.50.0\n"));
        runner.on("ssh -V", ok(""));
        runner.on("rsync --version", ok("rsync 3\n"));
        runner.on("gh --version", ok("gh version 2\n"));
        runner.on("gh auth status", fail(1, "not logged in"));
        runner
    }

    #[test]
    fn retry_diagnostics_include_unopened_projects_and_corrupt_state() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let runner = runner_with_herdr("herdr 0.9.1\n");
        let root = home.path().join("root");
        let project = project::create(&root, "demo", "test", vec![]).unwrap();
        let mut state = crate::steps::State::default();
        state.notification_retry.hash = "undelivered".into();
        state.notification_retry.retry.last_error = "delivery failed".into();
        crate::steps::save_state(&project, &state).unwrap();
        let (text, _) = report(&env, &root, &home.path().join("cfg"), &SessionFlags::default(), &runner);
        assert!(text.contains("notification retry pending"), "{text}");
        assert!(text.contains("delivery failed"), "{text}");
        std::fs::write(project.state_dir().join("ticker.json"), "{broken").unwrap();
        let (text, healthy) = report(&env, &root, &home.path().join("cfg"), &SessionFlags::default(), &runner);
        assert!(!healthy);
        assert!(text.contains("invalid retry state"), "{text}");
    }

    #[test]
    fn old_herdr_fails_and_names_the_minimum() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let runner = runner_with_herdr("herdr 0.9.0\n");
        let (text, healthy) = report(
            &env,
            &home.path().join("root"),
            &home.path().join("cfg"),
            &SessionFlags::default(),
            &runner,
        );
        assert!(!healthy);
        assert!(text.contains("[FAIL] herdr: 0.9.0"), "{text}");
        assert!(text.contains("0.9.1 or later"));
    }

    #[test]
    fn new_herdr_passes_and_warnings_do_not_fail() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let runner = runner_with_herdr("herdr 0.9.1\n");
        let root = home.path().join("root");
        let (text, healthy) = report(
            &env,
            &root,
            &home.path().join("cfg"),
            &SessionFlags::default(),
            &runner,
        );
        assert!(healthy, "{text}");
        assert!(text.contains("[warn] gh auth"));
        assert!(text.contains("[warn] root"));
        assert!(text.contains(&format!("root:       {}", root.display())));
        assert!(!root.exists(), "doctor must not create the root");
        assert!(!text.contains("[FAIL] executor"), "{text}");
        assert!(!text.contains("[ok  ] executor"), "{text}");
    }

    #[test]
    fn executor_metrics_are_advisory_and_never_fail_doctor() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let runner = runner_with_herdr("herdr 0.9.1\n");
        let root = home.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let metrics = crate::ticker::ExecutorMetrics {
            control: crate::ticker::LaneMetrics { queued: 0, running: 1, high_water: 2, completed: 3 },
            transfer: crate::ticker::LaneMetrics { queued: 0, running: 0, high_water: 0, completed: 0 },
            max_queue_delay_ms: 40,
            uncertain: false,
        };
        std::fs::write(root.join(".ticker-metrics.json"), serde_json::to_vec(&metrics).unwrap()).unwrap();
        let (text, healthy) = report(&env, &root, &home.path().join("cfg"), &SessionFlags::default(), &runner);
        assert!(healthy, "{text}");
        assert!(text.contains("[warn] executor: control q=0 r=1 hw=2 done=3"), "{text}");
        std::fs::write(root.join(".ticker-metrics.json"), "{broken").unwrap();
        let (text, healthy) = report(&env, &root, &home.path().join("cfg"), &SessionFlags::default(), &runner);
        assert!(healthy, "{text}");
        assert!(text.contains("[warn] executor: metrics unreadable"), "{text}");
    }
}

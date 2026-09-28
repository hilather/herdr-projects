//! `doctor`: what is installed, where things resolve, and whether it fits.

use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;

use crate::herdr::{self, Agent, Herdr, Pane};
use crate::paths::{self, Ctx, Env, SessionFlags};
use crate::project;
use crate::runner::{Cmd, Runner};

const TOOL_TIMEOUT: Duration = Duration::from_secs(10);

/// Coordinator pane + identity diagnostics shared by the legacy and migrated
/// paths. Each entry is (mark, detail) for the report `check` closure.
/// Read-only: never renames panes, starts agents, or clears `prime_pending`.
/// Coordinator identity is the whole tuple (workspace/tab/pane/cwd/name),
/// not the pane alone; priming also needs the configured kind and readiness.
fn coordinator_lines(
    slug: &str,
    record: &project::Coordinator,
    configured_kind: &str,
    status: &str,
    panes: &[Pane],
    agents: &[Agent],
    ticker_running: bool,
) -> Vec<(Option<bool>, String)> {
    let mut lines = Vec::new();
    let workspace = panes.iter().any(|p| p.workspace_id == record.workspace_id);
    let pane = panes.iter().any(|p| crate::coordinator::pane_matches(record, p));
    let matched = agents.iter().find(|a| crate::coordinator::agent_matches(record, a));
    let in_pane: Vec<_> = agents.iter().filter(|a| a.pane_id == record.pane_id).collect();
    let kind_mismatch = matched.is_some_and(|a| !configured_kind.is_empty() && a.agent != configured_kind);
    // A pane that holds no agent, another name, or the wrong kind is not ok,
    // even though the pane itself exists; the identity line below explains.
    let identity_broken = pane && (matched.is_none() || kind_mismatch);
    lines.push((
        if pane && !identity_broken { Some(true) } else { None },
        format!(
            "{status}; socket {}; workspace {} {}; coordinator pane {} {}",
            record.socket,
            record.workspace_id,
            if workspace { "exists" } else { "is gone" },
            record.pane_id,
            if pane { "exists" } else { "is gone (run `open`)" },
        ),
    ));
    let age_secs = crate::thread::seconds_since(&record.updated, jiff::Timestamp::now());
    let uncertain_prime = record.prime_claim.as_ref().is_some_and(|c| {
        c.delivery.phase == herdr_projects::prompt_claim::Phase::Uncertain
            || c.delivery.phase == herdr_projects::prompt_claim::Phase::Pending
    });
    let uncertain_launch = record.launch_claim.as_ref().is_some_and(|c| {
        c.phase == herdr_projects::launch_claim::Phase::Uncertain || c.phase == herdr_projects::launch_claim::Phase::Pending
    });
    if let Some(agent) = matched {
        if kind_mismatch {
            lines.push((None, format!("coordinator kind mismatch: configured `{configured_kind}` but live agent kind is `{}`; update PROJECT.md coordinator_agent or inspect pane {} then `open {slug} --reprime`; do not rename unrelated panes", agent.agent, record.pane_id)));
        } else if !record.prime_pending && !uncertain_prime && !uncertain_launch {
            lines.push((Some(true), format!("coordinator identity: live agent `{}` matches {}:{}:{} (kind `{}`)", agent.name, record.workspace_id, record.tab_id, record.pane_id, agent.agent)));
        } else if uncertain_prime || uncertain_launch {
            lines.push((None, format!("coordinator priming/start needs reconciliation (uncertain claim); inspect pane {} before explicitly running `open {slug} --reprime`", record.pane_id)));
        } else if agent.ready() {
            // Ready but still pending: normal brief startup when recent and
            // the ticker runs; otherwise stuck (ticker down, attempts out).
            let recent = age_secs < 600;
            if recent && ticker_running && record.launch_attempts < crate::coordinator::MAX_LAUNCH_ATTEMPTS {
                lines.push((None, format!("priming pending (normal startup): matching agent `{}` is ready; the ticker sends the priming prompt when it polls", agent.name)));
            } else {
                lines.push((None, format!("priming undelivered (stuck): matching agent `{}` is ready but prime_pending is set (age {}s, ticker {}); ensure `ticker start`, inspect pane {}, then `open {slug} --reprime` only after inspection", agent.name, age_secs, if ticker_running { "running" } else { "not running" }, record.pane_id)));
            }
        } else {
            lines.push((None, format!("priming pending (normal startup): matching agent `{}` is {} (not ready); the ticker delivers the priming prompt when it is idle/done", agent.name, agent.agent_status)));
        }
    } else if !pane {
        // Pane gone is already reported above; no separate identity line.
    } else if in_pane.is_empty() {
        lines.push((None, format!("coordinator stuck: pane {} exists but holds no agent (expected `{}`); inspect the pane, ensure `ticker start`, then `open {slug}` or `open {slug} --reprime` after inspection; do not rename unrelated panes", record.pane_id, record.agent_name)));
    } else {
        let names: Vec<String> = in_pane.iter().map(|a| format!("`{}` kind `{}`", a.name, a.agent)).collect();
        lines.push((None, format!("coordinator stuck: pane {} holds {} (expected `{}` in {}:{}); inspect before `open {slug} --reprime`; do not rename unrelated panes", record.pane_id, names.join(", "), record.agent_name, record.workspace_id, record.tab_id)));
    }
    if record.prime_pending && record.launch_attempts >= crate::coordinator::MAX_LAUNCH_ATTEMPTS {
        lines.push((None, format!("coordinator launch attempts exhausted ({}); inspect pane {} then `open {slug} --reprime` to request new work", record.launch_attempts, record.pane_id)));
    }
    lines
}

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
    write_compiled_features(&mut out);
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
    #[cfg(not(feature = "state-store"))]
    check(&mut out, None, "telemetry", "fleet panel unavailable: this build lacks `state-store`; rebuild with `cargo build --release --locked --features state-store`".into());

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
        let label = format!("project {slug}");
        // Migrated projects fail `Project::load` via `ensure_legacy`. Report
        // their memory mode and capability mismatches read-only instead of
        // skipping them silently.
        let project = match project::Project::load(root, &slug) {
            Ok(project) => project,
            Err(_) => {
                let dir = root.join(&slug);
                let format_path = dir.join(".state/format.json");
                let journal = dir.join(".state/migration/journal.json");
                if !format_path.exists() && !journal.exists() {
                    check(&mut out, Some(false), &label, "project load failed without migration markers; preserve and repair the record".into());
                    continue;
                }
                if journal.exists() && !format_path.exists() {
                    check(&mut out, Some(false), &label, "migration is incomplete (journal without format); run `migration status` then `migration recover --writers-stopped`".into());
                    continue;
                }
                let (memory, runtime) = std::fs::read_to_string(&format_path)
                    .ok()
                    .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                    .map(|v| (
                        v.get("memory").and_then(|m| m.as_str()).unwrap_or("unknown").to_string(),
                        v.get("runtime").and_then(|m| m.as_str()).unwrap_or("unknown").to_string(),
                    ))
                    .unwrap_or(("unknown".into(), "unknown".into()));
                if memory != "legacy-markdown" && memory != "sqlite-v1" {
                    check(&mut out, Some(false), &label, format!("unknown format.memory `{memory}`; preserve and repair .state/format.json"));
                    continue;
                }
                check(&mut out, Some(true), &label, format!("migrated runtime={runtime} memory={memory}; legacy thread/inbox files are pre-cutover originals"));
                #[cfg(feature="state-store")]
                {
                    // Doctor always runs the whole-store check and shows the ticker's last one.
                    let last = herdr_projects::store::integrity::load(&dir.join(".state/state.db"))
                        .map_or("none recorded".to_string(), |r| format!("{} at {} (schema {})", r.result, r.checked_unix_ms, r.schema));
                    match herdr_projects::migration::open_active(&dir) {
                        Ok(_) => check(&mut out, Some(true), &label, format!("store integrity: ok; last periodic check {last}")),
                        Err(error) if matches!(error.downcast_ref(), Some(herdr_projects::store::StoreError::Corrupt(_))) => check(&mut out, Some(false), &label,
                            format!("store integrity: corrupt; last periodic check {last}; preserve the store and restore it, never auto-repair")),
                        Err(_) => {},
                    }
                    // Telemetry S7: sidecar presence and last collect age, read-only.
                    match herdr_projects::telemetry::panel::collection(&dir, jiff::Timestamp::now().as_millisecond()) {
                        Ok(line) => check(&mut out, Some(true), &label, format!("telemetry: {line}")),
                        Err(error) => check(&mut out, None, &label, format!("telemetry: sidecar unreadable: {error:#}")),
                    }
                }
                if memory == "sqlite-v1" {
                    // A prohibition ("do not edit MEMORY.md") is correct guidance,
                    // not a mismatch; only an instruction to edit as authority warns.
                    let body = std::fs::read_to_string(dir.join("PROJECT.md")).unwrap_or_default().to_lowercase();
                    let says_edit = body.contains("edit memory.md")
                        && !(body.contains("do not edit memory.md")
                            || body.contains("don't edit memory.md")
                            || body.contains("never edit memory.md")
                            || body.contains("do not edit them"));
                    let claims_authority = body.contains("memory/*.md") && body.contains("authoritative");
                    if says_edit || claims_authority {
                        check(&mut out, None, &label, "capability mismatch: instructions say to edit MEMORY.md as authority but memory owner is SQLite (projections); use `memory PROJECT import/preview` with signed review; see `skill`".into());
                    }
                    let mem = std::fs::read_to_string(dir.join("MEMORY.md")).unwrap_or_default();
                    if !mem.contains("herdr-projects memory projection") {
                        check(&mut out, None, &label, "MEMORY.md is not a SQLite projection; run `migration export` for the current view; do not edit as authority".into());
                    }
                }
                match crate::memory_review::load(&dir) {
                    Err(error) => check(&mut out, Some(false), &label, format!("memory-review state: {error:#}")),
                    Ok(obligations) => {
                        let pending = obligations.iter().filter(|o| matches!(o.status, crate::memory_review::Status::Pending | crate::memory_review::Status::Deferred)).count();
                        if pending > 0 {
                            check(&mut out, Some(true), &label, format!("memory-review: {pending} unresolved; see `memory-review {slug} list`"));
                        }
                    }
                }
                // Migrated coordinator identity: the legacy record still names
                // the expected pane/agent; check it against the live agent list
                // instead of skipping identity for migrated projects.
                let record: Option<project::Coordinator> = std::fs::read(dir.join(".state/coordinator.json"))
                    .ok()
                    .and_then(|b| serde_json::from_slice(&b).ok());
                match record {
                    None => check(&mut out, Some(true), &label, "no legacy coordinator record; runtime bindings own identity".into()),
                    Some(record) if record.socket.is_empty() && record.pane_id.is_empty() => {
                        check(&mut out, Some(true), &label, "coordinator never opened; runtime bindings own identity".into());
                    }
                    Some(record) => {
                        if !Path::new(&record.socket).exists() {
                            check(&mut out, None, &label, format!("recorded socket {} no longer exists; `open --rebind` moves it", record.socket));
                            continue;
                        }
                        let herdr = Herdr::new(&bin, &record.socket, runner);
                        let panes = match herdr.pane_list() {
                            Err(error) => {
                                check(&mut out, None, &label, format!("session at {} unreachable: {error}", record.socket));
                                continue;
                            }
                            Ok(panes) => panes,
                        };
                        let agents = match herdr.agent_list() {
                            Err(error) => {
                                check(&mut out, None, &label, format!("agent inventory unreachable: {error}; cannot verify coordinator identity"));
                                continue;
                            }
                            Ok(agents) => agents,
                        };
                        let configured_kind = std::fs::read_to_string(dir.join("PROJECT.md"))
                            .ok()
                            .and_then(|t| crate::project::parse_project_md(&t).ok())
                            .map(|(s, _)| s.coordinator_agent)
                            .unwrap_or_default();
                        let ticker_running = !matches!(crate::ticker::lock_state(root), crate::ticker::LockState::Free);
                        for (mark, detail) in coordinator_lines(&slug, &record, &configured_kind, "migrated", &panes, &agents, ticker_running) {
                            check(&mut out, mark, &label, detail);
                        }
                    }
                }
                continue;
            }
        };
        if let Err(error) = project.try_status() { check(&mut out, Some(false), &label, format!("lifecycle record: {error:#}")); }
        for diagnostic in crate::thread::list_with_diagnostics(&project).1 {
            check(&mut out, Some(false), &label, format!("thread record: {diagnostic}; preserve and repair the file"));
        }
        for diagnostic in crate::inbox::unhandled_with_diagnostics(&project).1 {
            check(&mut out, Some(false), &label, format!("inbox record: {diagnostic}; preserve and repair the file"));
        }
        // Legacy memory capability: `memory propose` is SQLite-only worker
        // intake; legacy Remember review uses `memory-review` file candidates.
        match crate::memory_review::memory_owner(&project.dir()) {
            Err(error) => check(&mut out, Some(false), &label, format!("memory mode: {error:#}")),
            Ok(_) => {
                let body = std::fs::read_to_string(project.dir().join("PROJECT.md")).unwrap_or_default();
                if body.contains("memory propose") || body.contains("Memory owner: SQLite") {
                    check(&mut out, None, &label, format!("capability mismatch: instructions mention SQLite memory commands but storage is legacy-markdown; use `memory-review {slug} list/show/ingest/propose/reject/defer`; see `skill`"));
                }
                let mem = std::fs::read_to_string(project.dir().join("MEMORY.md")).unwrap_or_default();
                if mem.contains("herdr-projects memory projection") {
                    check(&mut out, None, &label, "capability mismatch: MEMORY.md looks like a SQLite projection on a legacy-markdown project; preserve and repair it".into());
                }
            }
        }
        match crate::memory_review::load(&project.dir()) {
            Err(error) => check(&mut out, Some(false), &label, format!("memory-review state: {error:#}")),
            Ok(obligations) => {
                let pending = obligations.iter().filter(|o| matches!(o.status, crate::memory_review::Status::Pending | crate::memory_review::Status::Deferred)).count();
                if pending > 0 {
                    check(&mut out, Some(true), &label, format!("memory-review: {pending} unresolved; see `memory-review {slug} list`"));
                }
            }
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
        let panes = match herdr.pane_list() {
            Err(error) => {
                check(&mut out, None, &label, format!("session at {} unreachable: {error}", record.socket));
                continue;
            }
            Ok(panes) => panes,
        };
        let agents = match herdr.agent_list() {
            Err(error) => {
                let pane = panes.iter().any(|p| crate::coordinator::pane_matches(&record, p));
                check(&mut out, if pane { Some(true) } else { None }, &label, format!("{}; coordinator pane {} (agent inventory unreachable: {error}; cannot verify identity)", project.status(), record.pane_id));
                continue;
            }
            Ok(agents) => agents,
        };
        let configured_kind = project.read_project_md().map(|(s, _)| s.coordinator_agent).unwrap_or_default();
        let ticker_running = !matches!(crate::ticker::lock_state(root), crate::ticker::LockState::Free);
        for (mark, detail) in coordinator_lines(&slug, &record, &configured_kind, &project.status().to_string(), &panes, &agents, ticker_running) {
            check(&mut out, mark, &label, detail);
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

/// OS is an argument so a Linux test can assert the unsupported label.
/// There is no macOS-only factory path: any other OS, and any live SSH route, refuses launch.
fn factory_platform_label(target_os: &str, live_ssh: bool) -> &'static str {
    if target_os == "linux" && !live_ssh {
        "linux"
    } else {
        "unsupported"
    }
}

/// Identity of this binary only. Opening a project database would be a migration path.
/// Does not resolve config, read projects, run tools, or contact sessions.
pub fn build_info() -> serde_json::Value {
    #[cfg(feature="state-store")]
    let (schema,sqlite,compatible,dispatch)=(
        Some(herdr_projects::store::SCHEMA),
        serde_json::json!({"version":rusqlite::version(),"minimum":herdr_projects::store::MIN_SQLITE_VERSION,
            "compatible":rusqlite::version_number()>=herdr_projects::store::MIN_SQLITE}),
        rusqlite::version_number()>=herdr_projects::store::MIN_SQLITE,
        Some(crate::canonical_controller::launch_dispatch_enabled()),
    );
    #[cfg(not(feature="state-store"))]
    let (schema,sqlite,compatible,dispatch):(Option<u32>,serde_json::Value,bool,Option<bool>)=(None,serde_json::Value::Null,false,None);
    serde_json::json!({
        "version":crate::VERSION,"os":std::env::consts::OS,"arch":std::env::consts::ARCH,
        "state_store":cfg!(feature="state-store"),"schema":schema,"sqlite":sqlite,
        "prepared_dispatch":dispatch,
        "factory_runtime_compatible":cfg!(all(feature="state-store",target_os="linux"))&&compatible,
        "live_capacity_certified":false,
    })
}

fn write_compiled_features(out: &mut String) {
    #[cfg(feature = "state-store")]
    {
        let _ = writeln!(out, "state-store: compiled");
        let _ = writeln!(out, "schema: {}", herdr_projects::store::SCHEMA);
        let _ = writeln!(out, "sqlite: {}", rusqlite::version());
        let _ = writeln!(
            out,
            "prepared_dispatch: {}",
            crate::canonical_controller::launch_dispatch_enabled()
        );
        let _ = writeln!(
            out,
            "factory: explicit factory binary `cargo build --release --locked --features state-store --target-dir target/factory`"
        );
        let _ = writeln!(
            out,
            "admission: off until a signed policy; doctor does not enable it or launch"
        );
    }
    #[cfg(not(feature = "state-store"))]
    {
        let _ = writeln!(out, "state-store: not compiled");
        let _ = writeln!(out, "schema: absent");
        let _ = writeln!(out, "sqlite: absent");
        let _ = writeln!(out, "prepared_dispatch: absent");
        let _ = writeln!(
            out,
            "factory: canonical factory commands are absent; explicit factory binary `cargo build --release --locked --features state-store --target-dir target/factory`"
        );
        let _ = writeln!(out, "admission: absent");
    }
    // This process is not an SSH route. macOS and live SSH stay unsupported labels.
    let platform = factory_platform_label(std::env::consts::OS, false);
    let _ = writeln!(out, "platform: {platform}");
    let _ = writeln!(
        out,
        "macOS: {}; canonical launch does not run",
        factory_platform_label("macos", false)
    );
    let _ = writeln!(
        out,
        "live SSH: {}; canonical launch does not run",
        factory_platform_label(std::env::consts::OS, true)
    );
    let _ = writeln!(
        out,
        "factory-path: {}",
        if platform == "linux" {
            "linux local only"
        } else {
            "unsupported; canonical launch does not run"
        }
    );
    let _ = writeln!(
        out,
        "upgrade: existing projects upgrade only through `migration PROJECT upgrade-store`"
    );
}

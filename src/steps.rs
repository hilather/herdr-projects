//! The ticker's per-project steps beyond thread state: inbox items, the nudge,
//! pull requests, routines, auto-resolve. Each is "compare with last time,
//! write an inbox item when it changed".

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::herdr::Herdr;
use crate::paths::Ctx;
use crate::project::{self, Project, Settings};
use crate::thread::{self, CopyOutcome, Group, Status, Thread};
use crate::threads;
use crate::{inbox, pr, routine};

mod recovery;
pub use recovery::{PendingFinalization, PendingEvent, NotificationRetry};

pub const NUDGE_TEXT: &str = "[herdr-projects ticker: automated, not the user, approves nothing] New inbox items. Run context.";
pub const PR_INTERVAL_SECS: i64 = 120;
pub const DONE_RETENTION_DAYS: u64 = 30;
const DEFAULT_OUTAGE_SECS: i64 = 600;

/// `.state/ticker.json`: what the ticker compared against last time.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct State {
    pub last_pr_check: String,
    pub prs: BTreeMap<String, pr::Summary>,
    pub pr_urls: BTreeMap<String, String>,
    /// thread id -> the pull request URL an "ignored" item was written for.
    pub pr_ignored: BTreeMap<String, String>,
    /// thread id -> report hash a "bad PR: line" note was written for.
    pub pr_line_noted: BTreeMap<String, String>,
    pub routines: routine::States,
    /// Hashes of files a `config-error` item was already written for.
    pub config_errors: BTreeSet<String>,
    /// Hash of the set of unseen item ids that was last nudged.
    pub nudged: String,
    pub session_item_written: bool,
    pub finalizations: BTreeMap<String, PendingFinalization>,
    pub notification_retry: NotificationRetry,
    pub gh_outages: BTreeMap<String, Outage>,
    pub machine_outages: BTreeMap<String, Outage>,
    pub event_sequence: u64,
    pub pending_events: BTreeMap<String, PendingEvent>,
}

pub fn try_load_state(project: &Project) -> Result<State> {
    use anyhow::Context;
    let path = project.state_dir().join("ticker.json");
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("{} is invalid; preserve this file and repair or restore it before resuming the ticker", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}; refusing to replace retry state", path.display())),
    }
}

#[cfg(test)]
pub fn load_state(project: &Project) -> State {
    try_load_state(project).expect("valid fixture ticker state")
}

/// Only the ticker writes this file, so its own read-modify-write is safe; the
/// write still happens under the project lock, like every `.state/` write.
pub fn save_state(project: &Project, state: &State) -> Result<()> {
    let _lock = project.lock()?;
    project::write_json(&project.state_dir().join("ticker.json"), state)
}

/// Continuous-failure tracking for `gh` or a machine: one item when it has
/// failed for the threshold, one more when it recovers, nothing for blips.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Outage {
    failing_since: Option<i64>,
    reported: bool,
    pub last_error: String,
}

#[derive(Debug, PartialEq)]
pub enum OutageEvent {
    Down,
    Recovered,
}

impl Outage {
    pub fn record(&mut self, ok: bool, error: &str, now: jiff::Timestamp, threshold_secs: i64) -> Option<OutageEvent> {
        if ok {
            let was_reported = self.reported;
            *self = Outage::default();
            return was_reported.then_some(OutageEvent::Recovered);
        }
        self.last_error = error.to_string();
        let since = *self.failing_since.get_or_insert(now.as_second());
        if !self.reported && now.as_second().saturating_sub(since) >= threshold_secs {
            self.reported = true;
            return Some(OutageEvent::Down);
        }
        None
    }
}

pub const REMOTE_INTERVAL: Duration = Duration::from_secs(60);
pub const REMOTE_RETRY_DELAY: Duration = Duration::from_secs(120);

/// A saved machine label can denote different observations in different
/// project sessions. Eligibility and outage delivery must have the same scope.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct MachineKey {
    pub project: std::path::PathBuf,
    pub socket: String,
    pub machine: String,
}

#[derive(Debug, Clone, Default)]
pub struct MachineMemory {
    /// Monotonic elapsed deadline; controller tick duration does not affect cadence.
    pub next_poll: Option<Instant>,
}

/// What the ticker process remembers between ticks (not persisted).
pub struct Memory {
    pub started: jiff::Timestamp,
    pub outage_secs: i64,
    pub tick: u64,
    pub machines: BTreeMap<MachineKey, MachineMemory>,
    pub pr_reads: Option<crate::pr_polling::Reads>,
    pub remote_reads:Option<crate::remote_polling::Reads>,
    #[cfg(feature="state-store")]
    pub routine_jobs:Option<crate::routine_jobs::Queue>,
    #[cfg(test)]
    clock: Option<Instant>,
}

impl Memory {
    pub fn new(ctx: &Ctx) -> Memory {
        Memory {
            started: jiff::Timestamp::now(),
            // Overridable so an outage can be exercised without waiting ten minutes.
            outage_secs: ctx.env.var("HERDR_PROJECTS_OUTAGE_SECS").and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_OUTAGE_SECS),
            tick: 0,
            machines: BTreeMap::new(),
            pr_reads: None,
            remote_reads: None,
            #[cfg(feature="state-store")]
            routine_jobs: None,
            #[cfg(test)]
            clock: None,
        }
    }

    fn monotonic_now(&self) -> Instant {
        #[cfg(test)]
        if let Some(now)=self.clock {return now;}
        Instant::now()
    }

    #[cfg(test)]
    pub fn advance_clock(&mut self, elapsed:Duration) {
        self.clock=Some(self.monotonic_now()+elapsed);
    }

    /// Check a monotonic deadline, independent of successful or delayed ticks.
    pub fn machine_is_due(&mut self, machine: &MachineKey) -> bool {
        let now=self.monotonic_now();
        let entry=self.machines.entry(machine.clone()).or_default();
        if entry.next_poll.is_some_and(|deadline|now<deadline) {return false;}
        entry.next_poll=Some(now+REMOTE_INTERVAL);
        true
    }

    pub fn record_machine(&mut self, machine: &MachineKey, error: Option<&str>) {
        let now=self.monotonic_now();
        // Backoff begins when the failed command finishes, not when its tick began.
        if error.is_some() {
            self.machines.entry(machine.clone()).or_default().next_poll=Some(now+REMOTE_RETRY_DELAY);
        }
    }
}

/// One `outage` item when a machine has been unreachable for the threshold,
/// one more when it is back. Short outages write nothing.
pub fn write_machine_outage(project: &Project, state: &mut State, key: &MachineKey, error: Option<&str>, now: jiff::Timestamp, threshold: i64) -> Result<()> {
    let machine = &key.machine;
    let resource = serde_json::to_string(&(&key.socket, machine))?;
    recovery::record_outage(project, state, &resource, Some(machine), error, now, threshold)
}

/// A group change seen in the cheap pass.
#[derive(Debug, Clone, PartialEq)]
pub struct Transition {
    pub id: String,
    pub to: Group,
    pub note: String,
}

fn thread_label(t: &Thread) -> String {
    format!("{} \"{}\"", t.id, t.title)
}

/// Step 1's inbox items, written after the copies so a Ready for review item
/// always points at a home copy that exists.
pub fn write_thread_items(project: &Project, state: &mut State, transitions: &[Transition], session_lost: bool, copy_notes: &BTreeMap<String, Vec<String>>) -> Result<()> {
    if session_lost {
        if !state.session_item_written {
            let open = thread::list(project).iter().filter(|t| t.status == Status::Open && !t.is_remote()).count();
            inbox::write(project, "session", "session", &format!("herdr session restarted; {open} threads need `thread restart`, and the coordinator needs `open`"), "")?;
            state.session_item_written = true;
        }
        return Ok(());
    }
    state.session_item_written = false;

    for change in transitions {
        if !matches!(change.to, Group::WaitingOnYou | Group::Landing | Group::Idle) {
            continue;
        }
        let Ok(t) = thread::load(project, &change.id) else {
            continue;
        };
        let mut summary = format!("{} is now {} ({})", thread_label(&t), change.to.label(), change.note);
        if change.to == Group::WaitingOnYou && !t.pane_id.is_empty() {
            summary.push_str(&format!("; it needs the user in pane {}", t.pane_id));
            if t.is_remote() {
                summary.push_str(&format!(" on machine `{}` (reach it with `herdr --remote <ssh target>`, or select the machine in herdr's sidebar)", t.machine));
            }
        }
        inbox::write(project, "thread-state", &t.id, &summary, "")?;
    }

    // Ready for review: once per report hash, so an agent that goes back and
    // forth between working and idle on an unchanged report produces nothing.
    for t in thread::list(project) {
        if t.status != Status::Open || t.report_hash.is_empty() || t.report_hash == t.last_review_item_hash {
            continue;
        }
        if t.last_group != Group::ReadyForReview.token() && t.last_group != Group::Landing.token() {
            continue;
        }
        let mut summary = format!("{} has a new report: threads/{}.md", thread_label(&t), t.id);
        if let Some(notes) = copy_notes.get(&t.id) {
            summary.push_str(&format!("; not everything was copied: {}", notes.join("; ")));
        }
        inbox::write(project, "thread-state", &t.id, &summary, "")?;
        let hash = t.report_hash.clone();
        thread::update(project, &t.id, |t| t.last_review_item_hash = hash)?;
    }
    Ok(())
}

fn hash_ids(ids: &BTreeSet<String>) -> String {
    thread::sha256_hex(ids.iter().cloned().collect::<Vec<_>>().join("\n").as_bytes())
}

/// Step 6. A given set of unseen items is announced once; there is no timed
/// re-nudge. With `nudge = false` (the default) the user gets a herdr
/// notification instead of a prompt in the coordinator.
pub fn nudge(project: &Project, state: &mut State, settings: &Settings, herdr: &Herdr, coordinator_ready: Option<&str>) -> Result<()> {
    nudge_at(project, state, settings, herdr, coordinator_ready, jiff::Timestamp::now())
}

pub fn nudge_at(project: &Project, state: &mut State, settings: &Settings, herdr: &Herdr, coordinator_ready: Option<&str>, now: jiff::Timestamp) -> Result<()> {
    let seen = inbox::seen(project);
    let unseen: BTreeSet<String> = inbox::unhandled(project).into_iter().map(|i| i.id).filter(|id| !seen.contains(id)).collect();
    if unseen.is_empty() {
        state.notification_retry = NotificationRetry::default();
        return Ok(());
    }
    let hash = hash_ids(&unseen);
    if hash == state.nudged {
        state.notification_retry = NotificationRetry::default();
        return Ok(());
    }
    if settings.nudge && coordinator_ready.is_none() { return Ok(()); }
    state.notification_retry.hash = hash.clone();
    if !state.notification_retry.retry.due(now) { return Ok(()); }
    state.notification_retry.retry.reserve(now, &hash);
    state.notification_retry.retry.last_error = "notification delivery reserved; outcome not yet recorded".into();
    save_state(project, state)?;
    let delivered = if settings.nudge {
        herdr.agent_prompt(coordinator_ready.unwrap(), NUDGE_TEXT)
    } else {
        let body = format!("{} new inbox item(s). The coordinator reads them at its next turn.", unseen.len());
        herdr.notification_show(&format!("herdr-projects: {}", project.slug), &body)
    };
    if let Err(error) = delivered {
        let error: anyhow::Error = error.into();
        state.notification_retry.retry.failed(&error);
        save_state(project, state)?;
        return Err(error);
    }
    state.nudged = hash;
    state.notification_retry = NotificationRetry::default();
    save_state(project, state)
}

/// Step 2, every two minutes.
pub fn pull_requests(ctx: &Ctx, project: &Project, state: &mut State, memory: &mut Memory, now: jiff::Timestamp) -> Vec<anyhow::Error> {
    if project.status() != project::Status::Active { return Vec::new(); }
    let mut errors = recovery::retry_finalizations(ctx, project, state, now);
    errors.extend(recovery::flush_events(project, state, now));
    if thread::seconds_since(&state.last_pr_check, now) < PR_INTERVAL_SECS && !state.last_pr_check.is_empty() {
        return errors;
    }
    let previous_check=state.last_pr_check.clone();
    let mut pending_read=false;
    state.last_pr_check = now.to_string();

    for t in thread::list(project) {
        let mut t = t;
        if t.status != Status::Open {
            continue;
        }
        // The `PR:` line of the home copy of the report.
        let report = std::fs::read_to_string(thread::home_report_path(project, &t.id)).unwrap_or_default();
        let url = match pr::pr_line(&report) {
            Ok(url) => url.unwrap_or_default(),
            Err(note) => {
                if state.pr_line_noted.get(&t.id) != Some(&t.report_hash) {
                    match recovery::queue_event(state, "pr", &t.id, &format!("{}: {note}", thread_label(&t)), "", now) {
                        Ok(()) => { state.pr_line_noted.insert(t.id.clone(), t.report_hash.clone()); }
                        Err(e) => errors.push(e),
                    }
                }
                String::new()
            }
        };
        if url != t.pr {
            let new_url = url.clone();
            match thread::update(project, &t.id, |t| t.pr = new_url) {
                Ok(updated) => t = updated,
                Err(e) => { errors.push(e); continue; }
            }
        }
        if url.is_empty() {
            continue;
        }

        let read=if let Some(reads)=memory.pr_reads.as_mut() {
            match reads.poll(&project.canonical_dir(),&t.id,&thread::sha256_hex(serde_json::json!([recovery::fingerprint(&t),t.report_hash,thread::sha256_hex(report.as_bytes())]).to_string().as_bytes()),&url) {
                Ok(crate::pr_polling::Poll::Pending)=>{pending_read=true;continue;},
                Ok(crate::pr_polling::Poll::NotDue)=>continue,
                Ok(crate::pr_polling::Poll::Ready(result))=>result,
                Err(error)=>{pending_read=true;errors.push(error.context("PR executor admission"));continue;},
            }
        }else{pr::view(ctx.runner,&url)};
        let json = match read {
            Ok(json) => {
                errors.extend(recovery::record_outage(project, state, &url, None, None, now, memory.outage_secs).err());
                json
            }
            Err(error) => {
                let text = pr::sanitize(&format!("{error:#}"));
                errors.extend(recovery::record_outage(project, state, &url, None, Some(&text), now, memory.outage_secs).err());
                continue;
            }
        };
        match pr::reduce(&json, &t.branch, &t.origin) {
            Err(error) => errors.push(error.context(format!("{}: gh output", t.id))),
            Ok(pr::Checked::Ignored(reason)) => {
                if state.pr_ignored.get(&t.id) != Some(&url) {
                    match recovery::queue_event(state, "pr", &t.id, &format!("{}: the pull request in its report is ignored: {reason}", thread_label(&t)), "", now) {
                        Ok(()) => { state.pr_ignored.insert(t.id.clone(), url.clone()); }
                        Err(e) => errors.push(e),
                    }
                }
            }
            Ok(pr::Checked::Summary(summary)) => {
                let old = if state.pr_urls.get(&t.id).is_some_and(|old_url| old_url != &url) { None } else { state.prs.get(&t.id).cloned() };
                let (pr_state, pr_review) = (summary.state.clone(), summary.review_decision.clone());
                let updated = thread::update_checked(project, &t.id, |current| {
                    if recovery::fingerprint(current) != recovery::fingerprint(&t) || current.status != Status::Open {
                        anyhow::bail!("{}: thread changed while polling its PR", t.id);
                    }
                    current.pr_state = pr_state;
                    current.pr_review = pr_review;
                    Ok(())
                });
                let t = match updated { Ok(t) => t, Err(e) => { errors.push(e); continue; } };
                let change = pr::describe_change(old.as_ref(), &summary);
                let merged = summary.state == "MERGED";
                if old.as_ref() != Some(&summary) {
                    match recovery::queue_event(state, "pr", &t.id, &format!("{}: pull request {change}", thread_label(&t)), "", now) {
                        Ok(()) => { state.prs.insert(t.id.clone(), summary); state.pr_urls.insert(t.id.clone(), url.clone()); }
                        Err(e) => { errors.push(e); continue; }
                    }
                }
                if merged {
                    recovery::schedule_finalization(state, &t);
                }
            }
        }
    }
    if pending_read {state.last_pr_check=previous_check;}
    errors.extend(recovery::flush_events(project, state, now));
    errors.extend(recovery::retry_finalizations(ctx, project, state, now));
    errors.extend(recovery::flush_events(project, state, now));
    errors
}

/// Idle auto-resolve: the final copy first; if it fails the
/// thread is not resolved and the next tick tries again.
fn resolve_after_copy(ctx: &Ctx, project: &Project, t: &Thread, reason: &str) -> Result<bool> {
    let copied = threads::final_copy(ctx, project, t);
    if let CopyOutcome::Failed(error) = copied.outcome {
        anyhow::bail!("{}: not resolved ({reason}) because the final copy failed: {error}", t.id);
    }
    thread::update_checked(project, &t.id, |current| {
        if thread::execution_fingerprint(current) != thread::execution_fingerprint(t) || current.status != Status::Open {
            anyhow::bail!("thread changed during idle finalization");
        }
        current.status = Status::Resolved;
        current.resolved_reason = reason.to_string();
        current.prompt_pending = false;
        Ok(())
    })?;
    Ok(true)
}

/// Step 4. Measured from the later of the last state change, the last report
/// change and the time this ticker process started, so a ticker that was down
/// for a week does not resolve everything at once.
pub fn auto_resolve(ctx: &Ctx, project: &Project, settings: &Settings, memory: &Memory, state: &State, now: jiff::Timestamp) -> Vec<anyhow::Error> {
    let mut errors = Vec::new();
    let limit = i64::from(settings.auto_resolve_days) * 86_400;
    if limit == 0 {
        return errors;
    }
    for t in thread::list(project) {
        if t.removal.is_some() || t.status != Status::Open || t.last_group != Group::Idle.token() || state.finalizations.contains_key(&t.id) {
            continue;
        }
        // The later of the three reference times is the smallest elapsed time.
        // A thread with neither timestamp has no clock to measure from.
        let elapsed = |stamp: &str| stamp.parse::<jiff::Timestamp>().ok().map(|then| now.as_second() - then.as_second());
        let since_ticker_start = now.as_second() - memory.started.as_second();
        let Some(since_thread) = [elapsed(&t.last_state_change), elapsed(&t.last_report_change)].into_iter().flatten().min() else {
            continue;
        };
        if since_thread.min(since_ticker_start) < limit {
            continue;
        }
        match resolve_after_copy(ctx, project, &t, "auto") {
            Ok(_) => errors.extend(inbox::write(project, "thread-state", &t.id, &format!("{} was idle for {} days and was resolved automatically; `thread resolve --reopen` undoes it", thread_label(&t), settings.auto_resolve_days), "").err()),
            Err(error) => errors.push(error),
        }
    }
    errors
}

/// Step 3, plus `config-error` items for files that do not parse.
pub fn routines(ctx: &Ctx, project: &Project, state: &mut State, routine_commands: bool, project_md_error: Option<(String, String)>, now: &jiff::Zoned) -> Vec<anyhow::Error> {
    let mut errors = Vec::new();
    let (routines, broken) = routine::load_all(project);

    let mut problems: Vec<(String, String, String)> = broken.into_iter().map(|b| (b.file, b.hash, b.error)).collect();
    if let Some((hash, error)) = project_md_error {
        problems.push(("PROJECT.md".into(), hash, error));
    }
    for (file, hash, error) in problems {
        // One item per distinct file hash, so an unfixed file does not repeat.
        if state.config_errors.insert(hash) {
            let stem = file.trim_start_matches("routines/").trim_end_matches(".md");
            errors.extend(inbox::write(project, "config-error", stem, &format!("{file} is not usable: {}", pr::sanitize(&error)), "").err());
        }
    }

    let prefix = crate::coordinator::current_prefix(&ctx.root).unwrap_or_default();
    for r in routines.iter().filter(|r| r.enabled) {
        let entry = state.routines.entry(r.name.clone()).or_default();
        let Ok(last_run) = entry.last_run.parse::<jiff::Timestamp>() else {
            // First seen counts as the last run: nothing fires the moment a
            // routine file appears.
            entry.last_run = now.timestamp().to_string();
            continue;
        };
        if !routine::is_due(&r.schedule, last_run, now) {
            continue;
        }
        entry.last_run = now.timestamp().to_string();

        if r.command.is_empty() {
            errors.extend(inbox::write(project, "routine", &r.name, &format!("routine `{}` is due", r.name), &r.prompt).err());
            continue;
        }
        if !routine_commands || !routine::is_approved(&ctx.config_dir, project, r) {
            let hash = r.command_hash();
            if entry.approval_item_for != hash {
                entry.approval_item_for = hash;
                let why = if routine_commands { "its command is not approved (or was edited since approval)" } else { "routine commands are not enabled for this project" };
                let summary = format!(
                    "routine `{}` did not run: {why}. The user enables them with `routine_commands = true` (see `{prefix} safety show {}`) and approves with `{prefix} routine approve {} {}` in a terminal",
                    r.name, project.slug, project.slug, r.name
                );
                errors.extend(inbox::write(project, "routine-approval", &r.name, &summary, "").err());
            }
            continue;
        }
        match routine::run_command(ctx.runner, project, r) {
            Ok(ran) => {
                if ran.output_hash != entry.output_hash {
                    entry.output_hash = ran.output_hash;
                    let body = format!("{}\n\n{}", r.prompt, ran.block);
                    errors.extend(inbox::write(project, "routine", &r.name, &format!("routine `{}` ran ({}) and its output changed", r.name, ran.exit), body.trim()).err());
                }
            }
            Err(error) => errors.push(error.context(format!("routine {}", r.name))),
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> jiff::Timestamp {
        text.parse().unwrap()
    }

    #[test]
    fn machine_cadence_and_backoff_are_scoped_to_project_and_session() {
        let world = crate::scenarios::World::new();
        let mut memory = Memory::new(&world.ctx());
        let a = MachineKey { project: "/a".into(), socket: "one.sock".into(), machine: "box".into() };
        let b = MachineKey { project: "/b".into(), ..a.clone() };
        let rebound = MachineKey { socket: "two.sock".into(), ..a.clone() };
        memory.outage_secs = 0;
        // Freeze monotonic time to exercise exact deadline boundaries.
        memory.advance_clock(Duration::ZERO);
        assert!(memory.machine_is_due(&a));
        assert!(!memory.machine_is_due(&a));
        assert!(memory.machine_is_due(&b));
        memory.record_machine(&a, Some("offline"));
        memory.record_machine(&b, None);
        memory.advance_clock(REMOTE_INTERVAL);
        assert!(!memory.machine_is_due(&a));
        assert!(memory.machine_is_due(&b));
        assert!(memory.machine_is_due(&rebound));
        memory.advance_clock(REMOTE_RETRY_DELAY-REMOTE_INTERVAL);
        assert!(memory.machine_is_due(&a));
        memory.record_machine(&a, None);
        assert!(!memory.machine_is_due(&a));
    }

    #[test]
    fn remote_deadlines_ignore_tick_counts_and_backoff_starts_after_failure() {
        let world=crate::scenarios::World::new();let mut memory=Memory::new(&world.ctx());
        memory.advance_clock(Duration::ZERO);
        let key=MachineKey{project:"/project".into(),socket:"session".into(),machine:"slow".into()};
        assert!(memory.machine_is_due(&key));
        // Thousands of fast control passes cannot cause early retry.
        memory.tick=10_000;assert!(!memory.machine_is_due(&key));
        memory.advance_clock(REMOTE_INTERVAL-Duration::from_millis(1));
        assert!(!memory.machine_is_due(&key));
        memory.advance_clock(Duration::from_millis(1));assert!(memory.machine_is_due(&key));
        // A long operation consumes time without any additional ticker iteration.
        memory.advance_clock(Duration::from_secs(180));
        memory.record_machine(&key,Some("timed out"));
        memory.advance_clock(REMOTE_RETRY_DELAY-Duration::from_millis(1));
        assert!(!memory.machine_is_due(&key));
        memory.advance_clock(Duration::from_millis(1));assert!(memory.machine_is_due(&key));
        // Missed deadlines produce one poll, never a burst of catch-up commands.
        memory.advance_clock(Duration::from_secs(3600));assert!(memory.machine_is_due(&key));
        assert!(!memory.machine_is_due(&key));
    }

    #[test]
    fn short_outages_write_nothing_and_long_ones_write_one_item_each_way() {
        let mut outage = Outage::default();
        assert_eq!(outage.record(false, "e", at("2026-09-17T10:00:00Z"), 600), None);
        assert_eq!(outage.record(false, "e", at("2026-09-17T10:05:00Z"), 600), None);
        // A blip that ends before the threshold reports nothing at all.
        assert_eq!(outage.record(true, "", at("2026-09-17T10:06:00Z"), 600), None);

        assert_eq!(outage.record(false, "e", at("2026-09-17T11:00:00Z"), 600), None);
        assert_eq!(outage.record(false, "e", at("2026-09-17T11:10:00Z"), 600), Some(OutageEvent::Down));
        assert_eq!(outage.record(false, "e", at("2026-09-17T11:30:00Z"), 600), None);
        assert_eq!(outage.record(true, "", at("2026-09-17T11:31:00Z"), 600), Some(OutageEvent::Recovered));
        assert_eq!(outage.record(true, "", at("2026-09-17T11:32:00Z"), 600), None);
    }
}

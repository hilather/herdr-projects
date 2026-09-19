//! Compatible file-backed retry records; replaced by the W03 operation store.

use super::*;
use anyhow::{Context, bail};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Retry {
    pub attempts: u32,
    pub next_attempt: String,
    pub last_error: String,
    pub blocked: bool,
}

impl Retry {
    pub fn due(&self, now: jiff::Timestamp) -> bool {
        !self.blocked && self.next_attempt.parse::<jiff::Timestamp>().map_or(true, |next| now >= next)
    }

    pub fn reserve(&mut self, now: jiff::Timestamp, key: &str) {
        self.attempts = self.attempts.saturating_add(1);
        let exponential = 15_i64 * (1_i64 << self.attempts.saturating_sub(1).min(5));
        let jitter = thread::sha256_hex(format!("{key}:{}", self.attempts).as_bytes()).as_bytes()[0] as i64 % 5;
        self.next_attempt = (now + jiff::SignedDuration::from_secs((exponential + jitter).min(300))).to_string();
    }

    pub fn failed(&mut self, error: &anyhow::Error) {
        self.last_error = format!("{error:#}").chars().take(2048).collect();
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct NotificationRetry {
    pub hash: String,
    pub retry: Retry,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PendingEvent {
    pub id: String,
    pub kind: String,
    pub subject: String,
    pub summary: String,
    pub body: String,
    #[serde(default)]
    pub retry: Retry,
}

/// Queue and dedup-state changes are saved together before delivery.
pub(super) fn queue_event(state: &mut State, kind: &str, subject: &str, summary: &str, body: &str, now: jiff::Timestamp) -> Result<()> {
    state.event_sequence = state.event_sequence.checked_add(1).context("event sequence exhausted")?;
    let id = format!("{}-{kind}-{}-retry-{:020}", now.strftime("%Y%m%dT%H%M%SZ"), inbox::safe_subject(subject), state.event_sequence);
    state.pending_events.insert(id.clone(), PendingEvent {
        id, kind: kind.into(), subject: subject.into(), summary: summary.into(), body: body.into(), retry: Retry::default(),
    });
    Ok(())
}

pub(super) fn flush_events(project: &Project, state: &mut State, now: jiff::Timestamp) -> Vec<anyhow::Error> {
    if state.pending_events.is_empty() { return Vec::new(); }
    if let Err(e) = save_state(project, state) { return vec![e]; }
    let mut errors = Vec::new();
    let mut changed = false;
    for (id, event) in state.pending_events.clone() {
        if !event.retry.due(now) { continue; }
        changed = true;
        match inbox::write_once(project, &event.id, &event.kind, &event.subject, &event.summary, &event.body) {
            Ok(()) => { state.pending_events.remove(&id); }
            Err(e) => {
                let retry = &mut state.pending_events.get_mut(&id).unwrap().retry;
                retry.reserve(now, &id);
                retry.failed(&e);
                errors.push(e);
            }
        }
    }
    if changed { errors.extend(save_state(project, state).err()); }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_capability_stays_blocked_after_restart() {
        let retry = Retry { blocked: true, ..Retry::default() };
        let restored: Retry = serde_json::from_str(&serde_json::to_string(&retry).unwrap()).unwrap();
        assert!(!restored.due(now() + jiff::SignedDuration::from_hours(24)));
        assert!(serde_json::from_str::<Retry>("{}").unwrap().due(now()));
    }

    fn now() -> jiff::Timestamp { "2026-09-19T12:00:00Z".parse().unwrap() }

    #[test]
    fn outage_streaks_survive_restart_and_healthy_resources_do_not_reset_them() {
        let root = tempfile::tempdir().unwrap();
        let a = project::create(root.path(), "a", "test", vec![]).unwrap();
        let b = project::create(root.path(), "b", "test", vec![]).unwrap();
        for project in [&a, &b] {
            let mut state = State::default();
            record_outage(project, &mut state, "bad-pr", None, Some("offline"), now(), 60).unwrap();
            record_outage(project, &mut state, "healthy-pr", None, None, now(), 60).unwrap();
        }
        for project in [&a, &b] {
            let mut state = load_state(project);
            let later = now() + jiff::SignedDuration::from_secs(61);
            record_outage(project, &mut state, "bad-pr", None, Some("still offline"), later, 60).unwrap();
            record_outage(project, &mut state, "healthy-pr", None, None, later, 60).unwrap();
            assert_eq!(state.pending_events.len(), 1);
            assert!(flush_events(project, &mut state, later).is_empty());
            let mut restarted = load_state(project);
            record_outage(project, &mut restarted, "bad-pr", None, None, later, 60).unwrap();
            assert!(flush_events(project, &mut restarted, later).is_empty());
            assert_eq!(inbox::unhandled(project).len(), 2);
            assert!(load_state(project).gh_outages.is_empty());
        }
    }

    #[test]
    fn failed_inbox_delivery_retries_and_replay_recognizes_handled_events() {
        let root = tempfile::tempdir().unwrap();
        let project = project::create(root.path(), "demo", "test", vec![]).unwrap();
        let mut state = State::default();
        record_outage(&project, &mut state, "socket:machine", Some("worker"), Some("offline"), now(), 0).unwrap();
        let event = state.pending_events.values().next().unwrap().clone();
        let path = project.dir().join("inbox").join(format!("{}.md", event.id));
        std::fs::create_dir(&path).unwrap();
        assert_eq!(flush_events(&project, &mut state, now()).len(), 1);
        let mut restarted = load_state(&project);
        let retry = &restarted.pending_events[&event.id].retry;
        assert_eq!(retry.attempts, 1);
        let due = retry.next_attempt.parse().unwrap();
        std::fs::remove_dir(&path).unwrap();
        assert!(flush_events(&project, &mut restarted, now()).is_empty());
        assert!(!path.exists());
        // Simulate delivery and user handling before the ticker's receipt save.
        inbox::write_once(&project, &event.id, &event.kind, &event.subject, &event.summary, &event.body).unwrap();
        inbox::done(&project, &[event.id.clone()], false).unwrap();
        assert!(flush_events(&project, &mut restarted, due).is_empty());
        assert!(load_state(&project).pending_events.is_empty());
        assert!(!path.exists());
        assert!(project.dir().join("inbox/done").join(format!("{}.md", event.id)).exists());
    }

    #[test]
    fn retries_are_bounded_even_after_attempt_counter_saturates() {
        let mut retry = Retry::default();
        for _ in 0..30 {
            retry.reserve(now(), "operation");
            let next = retry.next_attempt.parse::<jiff::Timestamp>().unwrap();
            let delay = next.as_second() - now().as_second();
            assert!((15..=300).contains(&delay));
        }
        retry.attempts = u32::MAX;
        retry.reserve(now(), "operation");
        assert_eq!(retry.attempts, u32::MAX);
        assert_eq!(retry.next_attempt.parse::<jiff::Timestamp>().unwrap().as_second() - now().as_second(), 300);
    }
}

/// Health and its delivery obligation share one atomic ticker-state write.
/// A healthy PR/session never resets a different resource's failure streak.
pub(super) fn record_outage(project: &Project, state: &mut State, resource: &str, machine: Option<&str>, error: Option<&str>, now: jiff::Timestamp, threshold: i64) -> Result<()> {
    let outages = if machine.is_some() { &mut state.machine_outages } else { &mut state.gh_outages };
    let old = outages.get(resource).cloned().unwrap_or_default();
    let mut health = old.clone();
    let event = health.record(error.is_none(), error.unwrap_or(""), now, threshold);
    if let Some(event) = event {
        let summary = match (machine, event) {
            (Some(label), OutageEvent::Down) => format!("machine `{label}` has been unreachable for {} minutes; its threads keep their last known state. Last error: {}", threshold / 60, pr::sanitize(&health.last_error)),
            (Some(label), OutageEvent::Recovered) => format!("machine `{label}` is reachable again"),
            (None, OutageEvent::Down) => format!("`gh` has been failing for {} minutes for {resource}; that pull request is not being followed. Last error: {}", threshold / 60, pr::sanitize(&health.last_error)),
            (None, OutageEvent::Recovered) => format!("`gh` is working again for {resource}; pull request follow-up has resumed"),
        };
        let subject = machine.map(str::to_string).unwrap_or_else(|| format!("gh-{}", &thread::sha256_hex(resource.as_bytes())[..16]));
        queue_event(state, "outage", &subject, &summary, "", now)?;
    }
    if health != old {
        let outages = if machine.is_some() { &mut state.machine_outages } else { &mut state.gh_outages };
        if health == Outage::default() { outages.remove(resource); }
        else { outages.insert(resource.into(), health); }
        save_state(project, state)?;
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PendingFinalization {
    pub operation_id: String,
    pub fingerprint: String,
    pub pr: String,
    pub reason: String,
    #[serde(default)]
    pub retry: Retry,
    #[serde(default)]
    pub partial_noted: bool,
}

pub(super) fn fingerprint(t: &Thread) -> String {
    thread::execution_fingerprint(t)
}

pub(super) fn schedule_finalization(state: &mut State, t: &Thread) {
    if t.pr.is_empty() || t.suppressed_merged_pr == t.pr || state.finalizations.contains_key(&t.id) { return; }
    let fingerprint = fingerprint(t);
    state.finalizations.insert(t.id.clone(), PendingFinalization {
        operation_id: format!("merged-{fingerprint}"), fingerprint, pr: t.pr.clone(),
        reason: "merged".into(), retry: Retry::default(), partial_noted: false,
    });
}

fn current(t: &Thread, pending: &PendingFinalization) -> bool {
    t.status == Status::Open && t.suppressed_merged_pr != pending.pr && fingerprint(t) == pending.fingerprint
}

fn report_matches(project: &Project, id: &str, url: &str) -> Result<bool> {
    let report = std::fs::read_to_string(thread::home_report_path(project, id))?;
    let found = pr::pr_line(&report).map_err(anyhow::Error::msg)?;
    Ok(found.as_deref() == Some(url))
}

pub(super) fn retry_finalizations(ctx: &Ctx, project: &Project, state: &mut State, now: jiff::Timestamp) -> Vec<anyhow::Error> {
    if project.status() != project::Status::Active { return Vec::new(); }
    let mut errors = Vec::new();
    for (id, mut pending) in state.finalizations.clone() {
        let outcome = (|| -> Result<()> {
            let record = thread::load(project, &id)?;
            if !current(&record, &pending) || !report_matches(project, &id, &pending.pr)? {
                // Resolved (including a crash after commit), reopened, cancelled
                // or rebound: never run an old copy against a replacement.
                if record.status == Status::Open && record.pr == pending.pr {
                    thread::update_checked(project, &id, |t| {
                        if t.pr == pending.pr { t.suppressed_merged_pr = pending.pr.clone(); }
                        Ok(())
                    })?;
                }
                state.finalizations.remove(&id);
                save_state(project, state)?;
                return Ok(());
            }
            if !pending.retry.due(now) { return Ok(()); }
            pending.retry.reserve(now, &pending.operation_id);
            pending.retry.last_error = "copy attempt reserved; outcome not yet recorded".into();
            state.finalizations.insert(id.clone(), pending.clone());
            // Durable intent and retry reservation precede every external copy.
            save_state(project, state)?;
            let copied = threads::copy_for_finalization(ctx, project, &record);
            if let CopyOutcome::Failed(error) = &copied.outcome {
                if error.contains("[transport-unsupported]") {
                    if let Some(saved) = state.finalizations.get_mut(&id) { saved.retry.blocked = true; }
                }
                bail!("{id}: not resolved ({}) because the final copy failed: {error}", pending.reason);
            }
            if let CopyOutcome::Partial(notes) = &copied.outcome {
                if !pending.partial_noted {
                    queue_event(state, "copy", &id, &format!("{}: merged final copy was partial: {}", thread_label(&record), notes.join("; ")), "", now)?;
                    pending.partial_noted = true;
                    state.finalizations.insert(id.clone(), pending.clone());
                    save_state(project, state)?;
                }
            }
            thread::update_checked(project, &id, |t| {
                if project.status() != project::Status::Active { bail!("project is no longer active; finalization remains pending"); }
                if !current(t, &pending) { bail!("{id}: thread identity changed during final copy"); }
                if !report_matches(project, &id, &pending.pr)? { bail!("{id}: report PR changed during final copy"); }
                if let Some(hash) = copied.report_hash {
                    if hash != t.report_hash { t.last_report_change = now.to_string(); }
                    t.report_hash = hash;
                }
                t.status = Status::Resolved;
                t.resolved_reason = pending.reason.clone();
                t.prompt_pending = false;
                t.last_finalization = pending.operation_id.clone();
                if let Some(snapshot) = copied.artifact_snapshot { t.artifact_snapshot = snapshot; }
                Ok(())
            })?;
            state.finalizations.remove(&id);
            save_state(project, state)?;
            Ok(())
        })();
        if let Err(error) = outcome {
            if let Some(pending) = state.finalizations.get_mut(&id) { pending.retry.failed(&error); }
            errors.push(error);
            errors.extend(save_state(project, state).err());
        }
    }
    errors
}

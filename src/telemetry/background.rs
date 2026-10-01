//! Scheduling for the ticker's disposable telemetry worker, never the controller.

/// Lower only the calling thread's CPU and I/O priority. Children inherit it,
/// including any gated subprocess launched by a lane. Failure is advisory and
/// reported once per process; it never prevents collection or a lane tick.
pub fn idle_priority(log: impl FnOnce(&str)) {
    #[cfg(target_os = "linux")]
    {
        static WARNED: std::sync::Once = std::sync::Once::new();
        let mut errors = Vec::new();
        // SAFETY: pid 0 means the calling Linux thread; param is initialized.
        let param = libc::sched_param { sched_priority: 0 };
        if unsafe { libc::sched_setscheduler(0, libc::SCHED_IDLE, &param) } != 0 {
            errors.push(format!("SCHED_IDLE: {}", std::io::Error::last_os_error()));
        }
        // Linux nice values are per-thread. This also supplies a fallback when
        // SCHED_IDLE is refused; do not change the controller's process leader.
        // SAFETY: gettid has no pointer arguments; the returned tid is live here.
        let tid = unsafe { libc::syscall(libc::SYS_gettid) } as libc::id_t;
        // SAFETY: valid priority selector, current thread id and nice value.
        if unsafe { libc::setpriority(libc::PRIO_PROCESS, tid, 19) } != 0 {
            errors.push(format!("nice 19: {}", std::io::Error::last_os_error()));
        }
        // IOPRIO_WHO_PROCESS=1, who=0 selects this thread, IDLE=3 << 13.
        // SAFETY: ioprio_set takes three integer arguments, no pointers.
        if unsafe { libc::syscall(libc::SYS_ioprio_set, 1, 0, 3 << 13) } != 0 {
            errors.push(format!("I/O idle: {}", std::io::Error::last_os_error()));
        }
        if !errors.is_empty() {
            WARNED.call_once(|| log(&format!("telemetry background priority: {} (continuing)", errors.join("; "))));
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = log;
}

/// One bounded worker for the slower derived lanes. Collection/accounting keep
/// their own turn, so a long health or analytics evaluation cannot hold up the
/// next project's ledger. At most 64 projects wait, with one queued turn per
/// project; a full queue defers admission without blocking its caller.
pub struct DeferredLanes {
    sender: Option<std::sync::mpsc::SyncSender<std::path::PathBuf>>,
    pending: std::sync::Arc<std::sync::Mutex<std::collections::BTreeSet<std::path::PathBuf>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl DeferredLanes {
    pub fn new(run: impl Fn(&std::path::Path) + Send + 'static) -> std::io::Result<Self> {
        let (sender, receiver) = std::sync::mpsc::sync_channel::<std::path::PathBuf>(64);
        let pending = std::sync::Arc::new(std::sync::Mutex::new(std::collections::BTreeSet::new()));
        let queued = pending.clone();
        let thread = std::thread::Builder::new().name("telemetry-derived".into()).spawn(move || {
            while let Ok(project) = receiver.recv() {
                if let Ok(mut pending) = queued.lock() { pending.remove(&project); }
                run(&project);
            }
        })?;
        Ok(Self { sender: Some(sender), pending, thread: Some(thread) })
    }

    pub fn submit(&self, project: &std::path::Path) -> bool {
        let Some(sender) = &self.sender else { return false };
        let Ok(mut pending) = self.pending.lock() else { return false };
        if !pending.insert(project.to_owned()) { return true; }
        if sender.try_send(project.to_owned()).is_err() {
            pending.remove(project);
            return false;
        }
        true
    }

    /// Finish accepted turns, then exit. This never waits for a running lane.
    pub fn stop(&mut self) { self.sender.take(); }

    pub fn is_finished(&self) -> bool { self.thread.as_ref().is_none_or(|thread| thread.is_finished()) }

    /// Reap only a finished worker. The controller never joins unfinished work.
    pub fn reap(&mut self) {
        if self.is_finished() && let Some(thread) = self.thread.take() { let _ = thread.join(); }
    }
}

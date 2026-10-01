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

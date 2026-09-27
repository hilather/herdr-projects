//! Numeric-only observations of the controlled connection. No SQL or values
//! cross the callback. Counts include opening checks after hook installation.
use rusqlite::{ffi, Connection};
use std::sync::{Arc, atomic::{AtomicBool, AtomicU64, Ordering}};

#[derive(Default)]
struct Counts {
    attached: AtomicBool,
    rows: AtomicU64,
    steps: AtomicU64,
}

#[derive(Clone, Default)]
pub(crate) struct SqlWork(Arc<Counts>);

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct SqlWorkMetrics {
    pub connection_observed: bool,
    /// SQLite ROW notifications, including scalar queries and opening checks.
    /// This is not the number of Rust objects decoded or rows visited by scans.
    pub sqlite_rows_returned: u64,
    /// VM instructions in statements completed/reset while the hook is installed.
    pub sqlite_vm_steps: u64,
}

impl SqlWork {
    pub fn snapshot(&self) -> SqlWorkMetrics {
        SqlWorkMetrics {
            connection_observed: self.0.attached.load(Ordering::Relaxed),
            sqlite_rows_returned: self.0.rows.load(Ordering::Relaxed),
            sqlite_vm_steps: self.0.steps.load(Ordering::Relaxed),
        }
    }

    // SAFETY: The owning ControlledStore retains this Arc and removes the hook
    // before dropping it. Its private connection never replaces this hook.
    pub(super) unsafe fn attach(&self, connection: &Connection) {
        unsafe {
            ffi::sqlite3_trace_v2(connection.handle(),
                ffi::SQLITE_TRACE_ROW | ffi::SQLITE_TRACE_PROFILE,
                Some(observe), Arc::as_ptr(&self.0).cast_mut().cast());
        }
        self.0.attached.store(true, Ordering::Relaxed);
    }

    pub(super) fn detach(connection: &Connection) {
        // No callback can run concurrently: ControlledStore owns the connection
        // exclusively, and detach runs before its fields are destroyed.
        unsafe { ffi::sqlite3_trace_v2(connection.handle(), 0, None, std::ptr::null_mut()); }
    }
}

unsafe extern "C" fn observe(event: std::ffi::c_uint, context: *mut std::ffi::c_void,
    statement: *mut std::ffi::c_void, _: *mut std::ffi::c_void) -> std::ffi::c_int {
    // SQLite supplies the registered, live Counts pointer and a statement for
    // ROW/PROFILE events. Atomic arithmetic cannot panic across the C boundary.
    let counts = unsafe { &*context.cast::<Counts>() };
    if event == ffi::SQLITE_TRACE_ROW {
        counts.rows.fetch_add(1, Ordering::Relaxed);
    } else if event == ffi::SQLITE_TRACE_PROFILE {
        // Reset at every execution so cached/reused statements aren't counted
        // cumulatively. No other code consumes this statement status counter.
        let steps = unsafe { ffi::sqlite3_stmt_status(statement.cast(), ffi::SQLITE_STMTSTATUS_VM_STEP, 1) };
        counts.steps.fetch_add(steps.max(0) as u64, Ordering::Relaxed);
    }
    0
}

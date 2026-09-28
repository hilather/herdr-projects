/// Test harness only, as in the library: SQLite's memory statistics serialize
/// every allocation of every parallel test on one process-wide mutex.
#[cfg(all(test, feature = "state-store"))]
#[used]
#[unsafe(link_section = ".init_array")]
static SQLITE_TEST_MEMSTATUS_HOOK: unsafe extern "C" fn() = disable_sqlite_test_memstatus;

#[cfg(all(test, feature = "state-store"))]
unsafe extern "C" fn disable_sqlite_test_memstatus() {
    // SAFETY: runs from .init_array before main and before any SQLite use.
    unsafe { rusqlite::ffi::sqlite3_config(rusqlite::ffi::SQLITE_CONFIG_MEMSTATUS, 0) };
}

#[cfg(feature="state-store")]
mod canonical_controller;
#[cfg(feature="state-store")]
mod canonical_notification_jobs;
#[cfg(feature="state-store")]
mod canonical_finalization_jobs;
#[cfg(feature="state-store")]
mod canonical_brief_jobs;
#[cfg(feature="state-store")]
mod routine_jobs;
#[cfg(all(feature="state-store",target_os="linux"))]
mod canonical_verification_jobs;
#[cfg(all(feature="state-store",target_os="linux"))]
mod canonical_integration_jobs;
mod actions;
mod agents;
mod artifacts;
#[cfg(all(feature = "state-store", target_os = "linux"))]
use herdr_projects::source_tree;
#[cfg(not(all(feature = "state-store", target_os = "linux")))]
mod source_tree;
mod copy_jobs;
mod brief_jobs;
mod token_jobs;
mod fair_admission;
mod local_reports;
mod local_observations;
mod adopt;
mod cli;
mod cleanup;
mod coordinator;
mod coordinator_jobs;
mod notification_inventory;
mod doctor;
mod herdr;
mod inbox;
mod lifecycle;
mod overview;
mod paths;
mod pr;
mod project;
#[cfg(feature="state-store")]
mod migration_preflight;
mod remote;
mod remote_api;
mod repair;
mod routine;
mod legacy_routine_jobs;
#[path = "binary_runner.rs"]
mod runner;
#[allow(dead_code)] // Transfer callers and operational metrics follow PR read integration.
mod executor;
mod pr_polling;
mod remote_polling;
#[cfg(test)]
mod scenarios;
mod steps;
mod thread;
mod threads;
mod ticker;

/// Crate version plus a build identifier (short git hash and build time), so a
/// rebuilt binary always differs from the one a running ticker was started from.
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("HP_BUILD_ID"));

/// A herdr server that was not started from a login shell hands its plugins a
/// minimal `PATH`, so `gh`, `rsync` or the agent CLI may be missing for the
/// ticker although they work in the user's terminal. The usual install folders
/// are appended (never prepended: what the user's `PATH` resolves still wins).
fn extend_path() {
    let current = std::env::var_os("PATH").unwrap_or_default();
    let mut dirs: Vec<std::path::PathBuf> = std::env::split_paths(&current).collect();
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let mut extra: Vec<std::path::PathBuf> = ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"].iter().map(Into::into).collect();
    if let Some(home) = home {
        extra.push(home.join(".local/bin"));
        extra.push(home.join(".cargo/bin"));
    }
    for dir in extra {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    if let Ok(joined) = std::env::join_paths(dirs) {
        // SAFETY: first thing in `main`, before any thread exists.
        unsafe { std::env::set_var("PATH", joined) };
    }
}

fn main() {
    // The unshare child enters here. It must not extend PATH or open the store.
    if std::env::args().nth(1).as_deref() == Some("verification-setup") {
        #[cfg(all(feature = "state-store", target_os = "linux"))]
        {
            let code = herdr_projects::verification::setup_main();
            std::process::exit(code);
        }
        #[cfg(not(all(feature = "state-store", target_os = "linux")))]
        {
            eprintln!("herdr-projects: verification-setup requires Linux state-store");
            std::process::exit(72);
        }
    }
    extend_path();
    if let Err(error) = cli::run() {
        eprintln!("herdr-projects: {error:#}");
        std::process::exit(1);
    }
}
#[cfg(feature="state-store")]
mod reconcile_live;

#[cfg(feature="state-store")]
mod notification_delivery;

#[cfg(feature="state-store")]
mod finalization_delivery;

#[cfg(feature="state-store")]
mod runtime_ownership;

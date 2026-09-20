//! Opt-in Phase B storage. Legacy commands do not create or open this database.
#[cfg(feature = "state-store")]
pub mod domain;
#[cfg(feature = "state-store")]
pub mod store;

#[cfg(feature = "state-store")]
pub mod migration;
#[cfg(feature = "state-store")]
pub mod projections;

#[cfg(feature = "state-store")]
pub mod operations;
#[cfg(feature = "state-store")]
pub mod runtime;
#[cfg(feature="state-store")]
pub mod reconcile;

/// Bounded external command execution shared by the CLI and trusted library ingress.
pub mod runner;
pub mod execution_guard;
pub mod supervision;
pub mod status_notice;
pub mod copy_receipt;
#[cfg(feature = "state-store")]
pub mod authority;
#[cfg(feature = "state-store")]
pub mod routines;

/// Schedule semantics shared by legacy and durable routines.
pub mod schedule;

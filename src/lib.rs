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
pub mod review_notice;
pub mod live_copy_intent;
pub mod final_copy_intent;
#[cfg(feature = "state-store")]
pub mod authority;
#[cfg(feature = "state-store")]
pub mod routines;
#[cfg(feature = "state-store")]
pub mod memory;

/// Schedule semantics shared by legacy and durable routines.
pub mod schedule;

pub mod prompt_claim;
pub mod launch_claim;
pub mod coordinator_prime;
pub mod notification_claim;
pub mod worker_supervision;
#[cfg(all(feature="state-store",target_os="linux"))]
pub mod source_tree;
#[cfg(all(feature="state-store",target_os="linux"))]
pub mod worktree_preservation;
#[cfg(feature = "state-store")]
pub mod canonical_worker;

pub mod profile_config;

#[cfg(feature = "state-store")]
pub mod profile_preparation;

#[cfg(all(feature = "state-store", target_os = "linux"))]
pub mod launch_preparation;

#[cfg(all(feature = "state-store", target_os = "linux"))]
pub mod worktree_preparation;

//! Shared end-to-end test support, included by test crates with `mod support;`.
pub mod telemetry;
#[cfg(all(feature = "state-store", target_os = "linux"))]
pub mod replay;

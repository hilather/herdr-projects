//! Shared production command runner, with binary-only scripted test support.
pub use herdr_projects::runner::*;
#[cfg(test)]
#[path = "runner/fake.rs"]
pub mod fake;

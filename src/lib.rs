//! Opt-in Phase B storage. Legacy commands do not create or open this database.
#[cfg(feature = "state-store")]
pub mod domain;
#[cfg(feature = "state-store")]
pub mod store;

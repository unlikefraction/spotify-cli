//! spotify-cli backend.
//!
//! The backend exchanges Silicon Accounts sign-in tokens and persists encrypted replay results
//! so CLI and browser sessions survive refreshes safely. It verifies Carbon and Silicon
//! identities live, sends actor-scoped Ting user verification proofs, accepts bug reports,
//! and relays telemetry.

pub mod api;
mod auth_cache;
pub mod config;
pub mod error;
pub mod identity;
pub mod store;
pub mod telemetry;
pub mod ting;

pub use error::{AppError, AppResult};

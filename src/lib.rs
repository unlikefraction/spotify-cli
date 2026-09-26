//! spotify-cli backend.
//!
//! The backend exists to keep the IAM application secret off users' machines. It:
//! - exchanges IAM short-lived tokens (SLTs) for application sessions, refreshes and revokes them;
//! - registers each Silicon as a Ting recipient at login (`subscriptions.register`);
//! - sends trigger notifications to Ting on the calling Silicon's behalf (`tings.send`), minting a
//!   single-use IAM OBO proof bound to the exact request bytes for every send;
//! - accepts bug reports, relays CLI/daemon/web telemetry to Space Station, and receives IAM
//!   webhooks.
//!
//! It stores no IAM tokens: every authenticated call is introspected live, and refresh tokens stay
//! with the client that owns them.

pub mod api;
pub mod config;
#[cfg(feature = "dev")]
pub mod dev;
pub mod error;
pub mod identity;
pub mod store;
pub mod telemetry;
pub mod ting;

pub use error::{AppError, AppResult};

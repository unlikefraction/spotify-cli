//! spotify-cli backend.
//!
//! The backend exists to keep the IAM application secret off users' machines. It:
//! - exchanges IAM short-lived tokens (SLTs) for application sessions, refreshes and revokes them;
//! - requests separate Ting feature consent, then stores each reusable OBO root encrypted at rest;
//! - sends trigger notifications using the explicitly selected Ting account and organization;
//! - accepts bug reports, relays telemetry, and verifies IAM webhooks.
//!
//! Ordinary application sessions stay on the client. Durable feature consent survives logout;
//! every incoming application bearer is introspected live and Ting verifies each outgoing token.

pub mod api;
pub mod config;
#[cfg(feature = "dev")]
pub mod dev;
pub mod error;
pub mod identity;
mod obo;
pub mod store;
pub mod telemetry;
pub mod ting;

pub use error::{AppError, AppResult};

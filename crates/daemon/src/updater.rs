//! Silicon Apps owns installation and updates.
use crate::service::Daemon;
use serde_json::{Value, json};
use silicon_spotify_client::{Result, VERSION};
use std::path::PathBuf;
use std::sync::Arc;

pub fn install_method() -> (&'static str, Option<PathBuf>) {
    let exe = std::env::current_exe().ok();
    let source = exe
        .as_ref()
        .is_some_and(|p| p.to_string_lossy().contains("/target/"));
    (if source { "source" } else { "silicon-apps" }, exe)
}
pub async fn run(daemon: Arc<Daemon>) {
    let _ = check(&daemon).await;
}
pub async fn check(daemon: &Daemon) -> Result<Value> {
    let status = json!({"current": VERSION, "method": install_method().0, "manager": "silicon-apps", "note": "Silicon Apps manages updates. Run `silicon-apps update spotify` to update."});
    *daemon
        .update
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = status.clone();
    Ok(status)
}

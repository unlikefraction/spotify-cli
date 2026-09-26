//! Local backend with fake IAM and Ting, for end-to-end runs of the CLI and daemon.
//!
//! `cargo run --example dev_backend --features dev -- [bind] [ting-log.jsonl]`
//! Log in with any `oac_…` token (becomes si:dev) or a public id like `si:alice`.

use std::sync::Arc;

use silicon_spotify::dev::{FakeIam, FakeTing};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let bind = args
        .first()
        .cloned()
        .unwrap_or_else(|| "127.0.0.1:8787".into());
    let log = args.get(1).map(std::path::PathBuf::from);
    let ting = Arc::new(FakeTing::logging(log.clone()));
    let state = silicon_spotify::dev::state(ting, Arc::new(FakeIam::default()))?;
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    eprintln!(
        "dev backend (FAKE IAM + FAKE Ting) on http://{bind}; tings → {:?}",
        log
    );
    axum::serve(listener, silicon_spotify::api::router(state)).await?;
    Ok(())
}

//! `spotify-api`: the spotify-cli backend server.

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    let _ = rustls::crypto::ring::default_provider().install_default();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .json()
        .init();
    if std::env::args().any(|a| a == "--version") {
        println!(
            "{}",
            serde_json::json!({"version": env!("CARGO_PKG_VERSION")})
        );
        return Ok(());
    }
    let settings = silicon_spotify::config::Settings::from_env()?;
    let bind = settings.bind;
    let state = silicon_spotify::api::production(settings)?;
    let telemetry = state.telemetry.clone();
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(%bind, version = env!("CARGO_PKG_VERSION"), "spotify-cli backend listening");
    axum::serve(listener, silicon_spotify::api::router(state))
        .with_graceful_shutdown(shutdown())
        .await?;
    telemetry.flush();
    Ok(())
}

/// Resolves on Ctrl-C or, on Unix, SIGTERM (what systemd sends on stop/restart).
async fn shutdown() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

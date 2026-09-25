mod auth;
mod config;
mod crypto;
mod error;
mod html;
mod jobs;
mod mail;
mod model;
mod state;
mod trpc;

use crate::config::Config;
use crate::crypto::Crypto;
use crate::state::State;
use anyhow::Result;
use axum::Router;
use axum::routing::get;
use sqlx::postgres::PgPoolOptions;
use std::sync::Arc;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,sqlx=warn,tower_http=info".into()),
        )
        .init();
    let _ = rustls::crypto::ring::default_provider().install_default();

    let config = Config::from_env()?;
    let db = PgPoolOptions::new().max_connections(20).connect(&config.database_url).await?;
    sqlx::migrate!("./migrations").run(&db).await?;

    let state = Arc::new(State {
        db,
        crypto: Crypto::new(&config.encryption_key),
        http: reqwest::Client::builder()
            .user_agent("zero-server-rs")
            .timeout(std::time::Duration::from_secs(60))
            .build()?,
        sync_notify: Default::default(),
        mailbox_locks: Default::default(),
        config: config.clone(),
    });
    jobs::spawn_all(state.clone());

    // The frontend is a single-page app: unknown paths fall back to index.html.
    let index = format!("{}/index.html", config.static_dir);
    let spa = ServeDir::new(&config.static_dir).fallback(ServeFile::new(index));

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(auth::router())
        .merge(trpc::router())
        .fallback_service(spa)
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", config.port)).await?;
    tracing::info!(port = config.port, app_url = %config.app_url, "zero server listening");
    axum::serve(listener, app).await?;
    Ok(())
}

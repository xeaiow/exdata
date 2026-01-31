mod api;
mod cache;
mod config;
mod exchanges;
mod models;
mod validator;

use axum::{Router, routing::get};
use cache::{Cache, SharedCache};
use std::sync::Arc;
use tower_http::compression::CompressionLayer;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let cache: SharedCache = Arc::new(Cache::new());
    let client = reqwest::Client::new();

    // Spawn exchange tasks (5 futures only)
    tokio::spawn(exchanges::binance::run_future(cache.clone(), client.clone()));
    tokio::spawn(exchanges::bybit::run_future(cache.clone(), client.clone()));
    tokio::spawn(exchanges::okx::run_future(cache.clone(), client.clone()));
    tokio::spawn(exchanges::gate::run_future(cache.clone(), client.clone()));
    tokio::spawn(exchanges::bitget::run_future(cache.clone(), client.clone()));
    tokio::spawn(exchanges::zoomex::run_future(cache.clone(), client.clone()));

    // Spawn validator if configured
    if let Some(validator_config) = config::load_validator_config() {
        tokio::spawn(validator::run(cache.clone(), client.clone(), validator_config));
    }

    let app = Router::new()
        .route("/api/exdata", get(api::get_exdata))
        .layer(CompressionLayer::new())
        .with_state(cache);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    tracing::info!("listening on 0.0.0.0:3000");
    axum::serve(listener, app).await.unwrap();
}

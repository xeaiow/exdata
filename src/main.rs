mod api;
mod cache;
mod exchanges;
mod models;

use axum::{Router, routing::get};
use cache::{Cache, SharedCache};
use std::sync::Arc;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let cache: SharedCache = Arc::new(Cache::new());

    let app = Router::new()
        .route("/api/exdata", get(api::get_exdata))
        .with_state(cache.clone());

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    tracing::info!("listening on 0.0.0.0:3000");
    axum::serve(listener, app).await.unwrap();
}

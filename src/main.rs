mod api;
mod cache;
mod config;
mod exchanges;
mod models;
mod recorder;
mod validator;
mod spread;
mod ws;

use axum::{Router, routing::get};
use cache::{Cache, SharedCache};
use std::sync::Arc;
use tower_http::compression::CompressionLayer;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let (ticker_tx, _) = tokio::sync::broadcast::channel::<spread::TickerChanged>(4096);
    let cache: SharedCache = Arc::new(Cache::new(ticker_tx));
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

    // Spawn recorder if configured
    if let Some(recorder_config) = config::load_recorder_config() {
        tokio::spawn(recorder::run_recorder(
            cache.clone(),
            recorder_config.clickhouse_url,
            recorder_config.clickhouse_user,
            recorder_config.clickhouse_password,
        ));
    }

    // Background serializer: periodically flushes dirty exchange sections to cached JSON.
    // This decouples JSON serialization from the WS message hot path, reducing write lock
    // contention when multiple chunk workers update the same ExchangeSection concurrently.
    {
        let cache = cache.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(200));
            loop {
                interval.tick().await;
                let sections = [
                    &cache.binance_future,
                    &cache.bybit_future,
                    &cache.okx_future,
                    &cache.gate_future,
                    &cache.bitget_future,
                    &cache.zoomex_future,
                ];
                for section_lock in sections {
                    let mut section = section_lock.write().await;
                    if section.dirty {
                        section.serialize_cache();
                        section.dirty = false;
                    }
                }
            }
        });
    }

    // Spread calculator: receives TickerChanged events, computes spreads,
    // broadcasts SpreadOpportunity to WS clients.
    let (spread_tx, _) = tokio::sync::broadcast::channel::<spread::SpreadOpportunity>(4096);
    {
        let cache = cache.clone();
        let ticker_rx = cache.ticker_tx.subscribe();
        let spread_tx = spread_tx.clone();
        tokio::spawn(spread::run_spread_calculator(cache, ticker_rx, spread_tx));
    }

    // REST routes (state: SharedCache)
    let rest_app = Router::new()
        .route("/api/exdata", get(api::get_exdata))
        .layer(CompressionLayer::new())
        .with_state(cache.clone());

    // WS routes (state: SharedWsState)
    let ws_state: ws::SharedWsState = Arc::new(ws::WsState {
        cache: cache.clone(),
        spread_tx: spread_tx.clone(),
    });
    let ws_app = Router::new()
        .route("/ws/spreads", get(ws::ws_spreads))
        .route("/ws/tickers", get(ws::ws_tickers))
        .with_state(ws_state);

    let app = rest_app.merge(ws_app);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    tracing::info!("listening on 0.0.0.0:3000");
    axum::serve(listener, app).await.unwrap();
}

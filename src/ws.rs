use axum::body::Bytes;
use axum::extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use std::sync::Arc;
use tokio::sync::broadcast;

use crate::cache::SharedCache;
use crate::spread::{self, SpreadMessage, SpreadOpportunity};

/// Shared state for the WS handler.
pub struct WsState {
    pub cache: SharedCache,
    pub spread_tx: broadcast::Sender<SpreadOpportunity>,
}

pub type SharedWsState = Arc<WsState>;

/// Axum handler: upgrade HTTP -> WebSocket.
pub async fn ws_spreads(
    ws: WebSocketUpgrade,
    State(state): State<SharedWsState>,
) -> Response {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

async fn handle_ws(mut socket: WebSocket, state: SharedWsState) {
    tracing::info!("ws/spreads: client connected");

    // Subscribe before snapshot so updates during snapshot computation are buffered.
    let mut spread_rx = state.spread_tx.subscribe();

    // Compute and send initial snapshot
    let snapshot = build_snapshot(&state.cache).await;
    let msg = SpreadMessage::Snapshot { data: snapshot };
    let json = match serde_json::to_string(&msg) {
        Ok(j) => j,
        Err(e) => {
            tracing::error!("ws/spreads: snapshot serialize error: {}", e);
            return;
        }
    };
    if socket
        .send(Message::Text(Utf8Bytes::from(json)))
        .await
        .is_err()
    {
        return;
    }

    // Stream updates with ping/pong keepalive
    let mut ping_interval = tokio::time::interval(std::time::Duration::from_secs(30));
    ping_interval.tick().await; // consume immediate tick

    let pong_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    let sleep = tokio::time::sleep_until(pong_deadline);
    tokio::pin!(sleep);

    loop {
        tokio::select! {
            result = spread_rx.recv() => {
                match result {
                    Ok(opp) => {
                        let msg = SpreadMessage::Update { data: opp };
                        let json = match serde_json::to_string(&msg) {
                            Ok(j) => j,
                            Err(_) => continue,
                        };
                        if socket.send(Message::Text(Utf8Bytes::from(json))).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("ws/spreads: client lagged {} updates", n);
                        let snapshot = build_snapshot(&state.cache).await;
                        let msg = SpreadMessage::Snapshot { data: snapshot };
                        if let Ok(json) = serde_json::to_string(&msg) {
                            if socket.send(Message::Text(Utf8Bytes::from(json))).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Pong(_))) => {
                        sleep.as_mut().reset(tokio::time::Instant::now() + std::time::Duration::from_secs(60));
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
            _ = ping_interval.tick() => {
                if socket.send(Message::Ping(Bytes::new())).await.is_err() {
                    break;
                }
            }
            _ = &mut sleep => {
                tracing::warn!("ws/spreads: client pong timeout, disconnecting");
                break;
            }
        }
    }

    tracing::info!("ws/spreads: client disconnected");
}

/// Build a full snapshot of all current spread opportunities (spread > 0).
async fn build_snapshot(cache: &SharedCache) -> Vec<SpreadOpportunity> {
    let symbols = spread::collect_all_symbols(cache).await;
    let mut all_opps = Vec::new();

    for symbol in &symbols {
        let tickers = spread::read_symbol_tickers(cache, symbol).await;
        if tickers.len() < 2 {
            continue;
        }
        let opps = spread::compute_spreads(symbol, &tickers);
        for opp in opps {
            if opp.spread_percent > 0.0 {
                all_opps.push(opp);
            }
        }
    }

    all_opps
}

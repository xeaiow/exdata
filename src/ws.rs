use axum::body::Bytes;
use axum::extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use serde::Deserialize;
use std::collections::HashSet;
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

#[derive(Deserialize)]
pub(crate) struct WsQuery {
    #[serde(default)]
    min_spread: f64,
}

/// Axum handler: upgrade HTTP -> WebSocket.
pub async fn ws_spreads(
    ws: WebSocketUpgrade,
    State(state): State<SharedWsState>,
    Query(query): Query<WsQuery>,
) -> Response {
    let min_spread = query.min_spread;
    ws.on_upgrade(move |socket| handle_ws(socket, state, min_spread))
}

async fn handle_ws(mut socket: WebSocket, state: SharedWsState, min_spread: f64) {
    tracing::info!("ws/spreads: client connected (min_spread={})", min_spread);

    // Subscribe before snapshot so updates during snapshot computation are buffered.
    let mut spread_rx = state.spread_tx.subscribe();

    // Compute and send initial snapshot (filtered by min_spread)
    let snapshot = build_snapshot(&state.cache, min_spread).await;

    // Track which pairs have been sent to this client (for threshold-crossing detection)
    let mut sent_pairs: HashSet<(String, &'static str, &'static str)> = HashSet::new();
    for opp in &snapshot {
        sent_pairs.insert((opp.symbol.clone(), opp.long_exchange, opp.short_exchange));
    }

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
                        let key = (opp.symbol.clone(), opp.long_exchange, opp.short_exchange);

                        let should_send = if opp.spread_percent >= min_spread {
                            // Above threshold: send and track
                            sent_pairs.insert(key);
                            true
                        } else if sent_pairs.remove(&key) {
                            // Crossed below threshold: send final update
                            true
                        } else {
                            false
                        };

                        if should_send {
                            let msg = SpreadMessage::Update { data: opp };
                            let json = match serde_json::to_string(&msg) {
                                Ok(j) => j,
                                Err(_) => continue,
                            };
                            if socket.send(Message::Text(Utf8Bytes::from(json))).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("ws/spreads: client lagged {} updates", n);
                        let snapshot = build_snapshot(&state.cache, min_spread).await;
                        sent_pairs.clear();
                        for opp in &snapshot {
                            sent_pairs.insert((opp.symbol.clone(), opp.long_exchange, opp.short_exchange));
                        }
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

/// Build a full snapshot of all current spread opportunities filtered by min_spread.
async fn build_snapshot(cache: &SharedCache, min_spread: f64) -> Vec<SpreadOpportunity> {
    let symbols = spread::collect_all_symbols(cache).await;
    let mut all_opps = Vec::new();

    for symbol in &symbols {
        let tickers = spread::read_symbol_tickers(cache, symbol).await;
        if tickers.len() < 2 {
            continue;
        }
        let opps = spread::compute_spreads(symbol, &tickers);
        for opp in opps {
            if opp.spread_percent >= min_spread {
                all_opps.push(opp);
            }
        }
    }

    all_opps
}

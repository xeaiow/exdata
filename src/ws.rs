use axum::body::Bytes;
use axum::extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
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
                // Drain: collect the first message + all immediately available messages
                let mut pending: HashMap<(String, &'static str, &'static str), SpreadOpportunity> = HashMap::new();
                let mut need_snapshot = false;
                let mut closed = false;

                match result {
                    Ok(opp) => {
                        let key = (opp.symbol.clone(), opp.long_exchange, opp.short_exchange);
                        pending.insert(key, opp);
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("ws/spreads: client lagged {} updates", n);
                        need_snapshot = true;
                    }
                    Err(broadcast::error::RecvError::Closed) => { closed = true; }
                }

                if closed { break; }

                // Non-blocking drain: grab everything already buffered
                if !need_snapshot {
                    loop {
                        match spread_rx.try_recv() {
                            Ok(opp) => {
                                let key = (opp.symbol.clone(), opp.long_exchange, opp.short_exchange);
                                pending.insert(key, opp);
                            }
                            Err(broadcast::error::TryRecvError::Lagged(n)) => {
                                tracing::warn!("ws/spreads: client lagged {} updates (during drain)", n);
                                need_snapshot = true;
                                break;
                            }
                            Err(broadcast::error::TryRecvError::Empty) => break,
                            Err(broadcast::error::TryRecvError::Closed) => { closed = true; break; }
                        }
                    }
                    if closed { break; }
                }

                // If lagged, send full snapshot
                if need_snapshot {
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
                    continue;
                }

                // Filter by min_spread and track threshold crossings
                let mut to_send: Vec<SpreadOpportunity> = Vec::new();
                for (key, opp) in pending {
                    if opp.spread_percent >= min_spread {
                        sent_pairs.insert(key);
                        to_send.push(opp);
                    } else if sent_pairs.remove(&key) {
                        to_send.push(opp);
                    }
                }

                // Send batch
                if !to_send.is_empty() {
                    let msg = if to_send.len() == 1 {
                        SpreadMessage::Update { data: to_send.into_iter().next().unwrap() }
                    } else {
                        SpreadMessage::BatchUpdate { data: to_send }
                    };
                    let json = match serde_json::to_string(&msg) {
                        Ok(j) => j,
                        Err(_) => continue,
                    };
                    if socket.send(Message::Text(Utf8Bytes::from(json))).await.is_err() {
                        break;
                    }
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

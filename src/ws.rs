use axum::body::Bytes;
use axum::extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::broadcast;

use crate::cache::SharedCache;
use crate::exchanges::now_ms;
use crate::spread::{self, ExchangeName, SpreadMessage, SpreadOpportunity};

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

// ── /ws/tickers: per-subscription ticker push ──────────────────────────────

#[derive(Deserialize)]
struct TickerSubCommand {
    action: String,
    symbol: String,
    long_exchange: String,
    short_exchange: String,
}

#[derive(Clone, Serialize)]
struct TickerUpdate {
    symbol: String,
    long_exchange: &'static str,
    short_exchange: &'static str,
    long_ask: f64,
    long_bid: f64,
    short_ask: f64,
    short_bid: f64,
    long_ts: u64,
    short_ts: u64,
    server_ts: u64,
    spread_percent: f64,
}

/// Axum handler: upgrade HTTP -> WebSocket for per-position ticker subscriptions.
pub async fn ws_tickers(
    ws: WebSocketUpgrade,
    State(state): State<SharedWsState>,
) -> Response {
    ws.on_upgrade(move |socket| handle_ws_tickers(socket, state))
}

async fn handle_ws_tickers(mut socket: WebSocket, state: SharedWsState) {
    tracing::info!("ws/tickers: client connected");

    // Subscribe to raw ticker changes (same channel that drives spread calculator)
    let mut ticker_rx = state.cache.ticker_tx.subscribe();

    // Per-client subscriptions: (symbol, long_exchange_name, short_exchange_name)
    // Store resolved ExchangeName enums alongside the &'static str for cache lookup
    let mut subscriptions: HashMap<
        (String, &'static str, &'static str),
        (ExchangeName, ExchangeName),
    > = HashMap::new();

    let mut ping_interval = tokio::time::interval(std::time::Duration::from_secs(30));
    ping_interval.tick().await;

    let pong_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    let sleep = tokio::time::sleep_until(pong_deadline);
    tokio::pin!(sleep);

    loop {
        tokio::select! {
            result = ticker_rx.recv() => {
                // Drain all pending ticker change events, dedup by symbol
                let mut changed_symbols: HashSet<String> = HashSet::new();

                match result {
                    Ok(e) => { changed_symbols.insert(e.symbol); }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // On lag, collect all subscribed symbols
                        for (key, _) in &subscriptions {
                            changed_symbols.insert(key.0.clone());
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }

                // Non-blocking drain
                loop {
                    match ticker_rx.try_recv() {
                        Ok(e) => { changed_symbols.insert(e.symbol); }
                        Err(broadcast::error::TryRecvError::Lagged(_)) => {
                            for (key, _) in &subscriptions {
                                changed_symbols.insert(key.0.clone());
                            }
                        }
                        Err(broadcast::error::TryRecvError::Empty) => break,
                        Err(broadcast::error::TryRecvError::Closed) => break,
                    }
                }

                // For each changed symbol, check subscriptions and push updates
                for symbol in &changed_symbols {
                    for ((sub_sym, long_ex, short_ex), (long_name, short_name)) in &subscriptions {
                        if sub_sym != symbol {
                            continue;
                        }
                        if let Some(update) = build_ticker_update(
                            &state.cache, sub_sym, *long_ex, *short_ex, *long_name, *short_name,
                        ).await {
                            if let Ok(json) = serde_json::to_string(&update) {
                                if socket.send(Message::Text(Utf8Bytes::from(json))).await.is_err() {
                                    tracing::info!("ws/tickers: client disconnected (send error)");
                                    return;
                                }
                            }
                        }
                    }
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<TickerSubCommand>(&text) {
                            Ok(cmd) => {
                                let long_name = match ExchangeName::from_str(&cmd.long_exchange) {
                                    Some(n) => n,
                                    None => {
                                        tracing::warn!("ws/tickers: unknown long_exchange: {}", cmd.long_exchange);
                                        continue;
                                    }
                                };
                                let short_name = match ExchangeName::from_str(&cmd.short_exchange) {
                                    Some(n) => n,
                                    None => {
                                        tracing::warn!("ws/tickers: unknown short_exchange: {}", cmd.short_exchange);
                                        continue;
                                    }
                                };
                                let key = (cmd.symbol.clone(), long_name.as_str(), short_name.as_str());

                                match cmd.action.as_str() {
                                    "subscribe" => {
                                        tracing::info!("ws/tickers: subscribe {} {}/{}", cmd.symbol, cmd.long_exchange, cmd.short_exchange);
                                        subscriptions.insert(key.clone(), (long_name, short_name));
                                        // Push immediate snapshot
                                        if let Some(update) = build_ticker_update(
                                            &state.cache, &cmd.symbol,
                                            key.1, key.2, long_name, short_name,
                                        ).await {
                                            if let Ok(json) = serde_json::to_string(&update) {
                                                if socket.send(Message::Text(Utf8Bytes::from(json))).await.is_err() {
                                                    return;
                                                }
                                            }
                                        }
                                    }
                                    "unsubscribe" => {
                                        tracing::info!("ws/tickers: unsubscribe {} {}/{}", cmd.symbol, cmd.long_exchange, cmd.short_exchange);
                                        subscriptions.remove(&key);
                                    }
                                    _ => {
                                        tracing::warn!("ws/tickers: unknown action: {}", cmd.action);
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!("ws/tickers: parse error: {}", e);
                            }
                        }
                    }
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
                tracing::warn!("ws/tickers: client pong timeout, disconnecting");
                break;
            }
        }
    }

    tracing::info!("ws/tickers: client disconnected");
}

/// Build a TickerUpdate by reading both exchange tickers from cache (no freshness filter).
async fn build_ticker_update(
    cache: &SharedCache,
    symbol: &str,
    long_ex: &'static str,
    short_ex: &'static str,
    long_name: ExchangeName,
    short_name: ExchangeName,
) -> Option<TickerUpdate> {
    let long = spread::read_exchange_ticker(cache, long_name, symbol).await;
    let short = spread::read_exchange_ticker(cache, short_name, symbol).await;

    // Need at least one side to have data
    let (long_ask, long_bid, long_ts) = long.unwrap_or((0.0, 0.0, 0));
    let (short_ask, short_bid, short_ts) = short.unwrap_or((0.0, 0.0, 0));

    if long_ts == 0 && short_ts == 0 {
        return None;
    }

    let spread_percent = if long_ask > 0.0 && short_bid > 0.0 {
        let mid = (long_ask + short_bid) / 2.0;
        ((short_bid - long_ask) / mid * 100.0 * 100.0).round() / 100.0
    } else {
        0.0
    };

    Some(TickerUpdate {
        symbol: symbol.to_string(),
        long_exchange: long_ex,
        short_exchange: short_ex,
        long_ask,
        long_bid,
        short_ask,
        short_bid,
        long_ts,
        short_ts,
        server_ts: now_ms(),
        spread_percent,
    })
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

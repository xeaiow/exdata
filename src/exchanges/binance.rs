use crate::cache::SharedCache;
use crate::exchanges::{backoff_sleep, now_ms, parse_f64};
use crate::models::{ExchangeItem, PriceLevel};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use tokio_tungstenite::{connect_async, tungstenite::Message};

// ── Serde structs ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct CombinedMsg {
    stream: String,
    data: serde_json::Value,
}

#[derive(Deserialize)]
struct FuturesTicker {
    s: String,
    q: String,
}

#[derive(Deserialize)]
struct MarkPriceUpdate {
    s: String,
    p: String,
    i: String,
    r: String,
}

#[derive(Deserialize)]
struct FundingInfoItem {
    symbol: String,
    #[serde(rename = "adjustedFundingRateCap")]
    adjusted_funding_rate_cap: String,
    #[serde(rename = "fundingIntervalHours")]
    funding_interval_hours: u32,
}

#[derive(Deserialize)]
struct RestBookTicker {
    symbol: String,
    #[serde(rename = "bidPrice")]
    bid_price: String,
    #[serde(rename = "askPrice")]
    ask_price: String,
}

// ── Futures helpers ─────────────────────────────────────────────────────────

/// Fetch funding info from REST and return (interval_hours, max_rate_percent) by symbol.
async fn fetch_funding_info(
    client: &reqwest::Client,
) -> HashMap<String, (u32, String)> {
    let url = "https://fapi.binance.com/fapi/v1/fundingInfo";
    let resp = match client.get(url).timeout(std::time::Duration::from_secs(10)).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("binance futures: funding info request failed: {}", e);
            return HashMap::new();
        }
    };

    let items: Vec<FundingInfoItem> = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("binance futures: funding info parse failed: {}", e);
            return HashMap::new();
        }
    };

    let mut map = HashMap::with_capacity(items.len());
    for item in items {
        let cap_dec = parse_f64(&item.adjusted_funding_rate_cap);
        let max_pct = format!("{:.3}", cap_dec * 100.0);
        map.insert(item.symbol, (item.funding_interval_hours, max_pct));
    }
    map
}

/// Fetch initial book ticker snapshot so every symbol has bid/ask from the start.
/// The `!bookTicker` WebSocket stream is push-on-change only (no initial snapshot),
/// so without this, illiquid symbols would stay at a=0 / b=0.
async fn fetch_book_tickers(client: &reqwest::Client) -> Vec<RestBookTicker> {
    let url = "https://fapi.binance.com/fapi/v1/ticker/bookTicker";
    let resp = match client.get(url).timeout(std::time::Duration::from_secs(10)).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("binance futures: book ticker fetch failed: {}", e);
            return Vec::new();
        }
    };
    match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("binance futures: book ticker parse failed: {}", e);
            Vec::new()
        }
    }
}

// ── Funding info refresher ───────────────────────────────────────────────────

/// Periodically refreshes funding info (rate_interval, rate_max) without restarting WS.
async fn funding_refresher(cache: SharedCache, client: reqwest::Client) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
    interval.tick().await; // skip immediate tick (coordinator just seeded)

    loop {
        interval.tick().await;
        let funding_info = fetch_funding_info(&client).await;
        if funding_info.is_empty() {
            continue;
        }
        let mut section = cache.binance_future.write().await;
        for (sym, (interval_h, max_rate)) in &funding_info {
            if let Some(item) = section.items.get_mut(sym) {
                item.rate_interval = Some(*interval_h);
                item.rate_max = Some(max_rate.clone());
            }
        }
    }
}

// ── Futures (coordinator + chunk worker) ────────────────────────────────────

/// Coordinator: fetches funding info, seeds REST, spawns single chunk worker, restarts every 5 min.
pub async fn run_future(cache: SharedCache, client: reqwest::Client) {
    let mut attempt: u32 = 0;

    loop {
        tracing::info!("binance futures: fetching funding info...");
        let funding_info = fetch_funding_info(&client).await;
        tracing::info!(
            "binance futures: loaded funding info for {} symbols",
            funding_info.len()
        );

        if funding_info.is_empty() {
            tracing::warn!("binance futures: no funding info found, retrying...");
            backoff_sleep(attempt).await;
            attempt = attempt.saturating_add(1);
            continue;
        }
        attempt = 0;

        // Build valid symbols from funding info keys (USDT only)
        let valid_symbols: Vec<String> = funding_info
            .keys()
            .filter(|s| s.ends_with("USDT") && *s != "ALLUSDT")
            .cloned()
            .collect();

        // Seed bid/ask from REST
        let book_tickers = fetch_book_tickers(&client).await;
        if !book_tickers.is_empty() {
            let valid_set: HashSet<&String> = valid_symbols.iter().collect();
            let ts = now_ms();
            let mut section = cache.binance_future.write().await;
            for bt in &book_tickers {
                if !valid_set.contains(&bt.symbol) {
                    continue;
                }
                let item = section
                    .items
                    .entry(bt.symbol.clone())
                    .or_insert_with(|| ExchangeItem {
                        name: bt.symbol.clone(),
                        ..Default::default()
                    });
                item.a = parse_f64(&bt.ask_price);
                item.b = parse_f64(&bt.bid_price);
                item.ts = ts;
                // Always refresh funding info from REST (exchange may change intervals dynamically)
                if let Some((interval, max_rate)) = funding_info.get(&bt.symbol) {
                    item.rate_interval = Some(*interval);
                    item.rate_max = Some(max_rate.clone());
                }
            }
            tracing::info!(
                "binance futures: seeded {} book tickers from REST",
                section.items.len()
            );
            section.serialize_cache();
        }
        cache.binance_future.write().await.restarting = false;

        // Spawn single chunk worker (Binance uses array broadcast, no per-symbol sub)
        let funding_info = std::sync::Arc::new(funding_info);
        tracing::info!(
            "binance futures: spawning single chunk worker ({} symbols)",
            valid_symbols.len()
        );
        let handle = tokio::spawn(run_chunk(
            cache.clone(),
            valid_symbols,
            funding_info,
            0,
        ));

        // Spawn funding info refresher (updates rate_interval/rate_max every 60s)
        let refresher_handle = tokio::spawn(funding_refresher(cache.clone(), client.clone()));

        // Sleep 5 minutes, then abort worker + refresher and restart
        tokio::time::sleep(std::time::Duration::from_secs(300)).await;

        tracing::info!("binance futures: coordinator restarting, aborting worker");
        cache.binance_future.write().await.restarting = true;
        refresher_handle.abort();
        let _ = refresher_handle.await;
        handle.abort();
        let _ = handle.await;
    }
}

/// Single chunk worker: manages WS connection to Binance array broadcast streams.
/// Self-reconnects on disconnect; coordinator handles instrument refresh.
async fn run_chunk(
    cache: SharedCache,
    symbols: Vec<String>,
    funding_info: std::sync::Arc<HashMap<String, (u32, String)>>,
    chunk_id: usize,
) {
    let valid_set: HashSet<String> = symbols.into_iter().collect();
    let mut attempt: u32 = 0;

    loop {
        tracing::info!(
            "binance futures chunk-{}: connecting ({} symbols)...",
            chunk_id,
            valid_set.len()
        );
        let depth_streams: Vec<String> = valid_set
            .iter()
            .map(|s| format!("{}@depth20@500ms", s.to_lowercase()))
            .collect();
        let url = format!(
            "wss://fstream.binance.com/stream?streams=!ticker@arr/!markPrice@arr@1s/{}",
            depth_streams.join("/")
        );
        let ws_result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            connect_async(&url),
        )
        .await;
        match ws_result {
            Ok(Ok((ws, _))) => {
                attempt = 0;
                tracing::info!("binance futures chunk-{}: connected", chunk_id);
                let (mut write, mut read) = ws.split();

                let connect_time = tokio::time::Instant::now();

                let mut ping_interval =
                    tokio::time::interval(std::time::Duration::from_secs(30));
                ping_interval.tick().await;

                let mut read_deadline =
                    tokio::time::Instant::now() + std::time::Duration::from_secs(30);
                let mut depth_throttle: std::collections::HashMap<String, tokio::time::Instant> = std::collections::HashMap::new();

                loop {
                    tokio::select! {
                        msg = read.next() => {
                            read_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
                            let msg = match msg {
                                Some(Ok(m)) => m,
                                Some(Err(e)) => {
                                    tracing::warn!("binance futures chunk-{}: read error: {}", chunk_id, e);
                                    break;
                                }
                                None => {
                                    tracing::warn!("binance futures chunk-{}: stream ended", chunk_id);
                                    break;
                                }
                            };

                            let text = match msg {
                                Message::Text(t) => t,
                                Message::Ping(_) | Message::Pong(_) => continue,
                                Message::Close(_) => {
                                    tracing::warn!("binance futures chunk-{}: server closed connection", chunk_id);
                                    break;
                                }
                                _ => continue,
                            };

                            let combined: CombinedMsg = match serde_json::from_str(&text) {
                                Ok(v) => v,
                                Err(e) => {
                                    tracing::warn!("binance futures chunk-{}: parse combined msg error: {}", chunk_id, e);
                                    continue;
                                }
                            };

                            let stream = combined.stream.as_str();
                            let ts = now_ms();

                            if stream == "!ticker@arr" {
                                let tickers: Vec<FuturesTicker> =
                                    match serde_json::from_value(combined.data) {
                                        Ok(v) => v,
                                        Err(e) => {
                                            tracing::warn!(
                                                "binance futures chunk-{}: parse ticker array error: {}",
                                                chunk_id, e
                                            );
                                            continue;
                                        }
                                    };
                                let mut section = cache.binance_future.write().await;
                                section.ts = ts;
                                for t in &tickers {
                                    if !valid_set.contains(&t.s) {
                                        continue;
                                    }
                                    let item = section
                                        .items
                                        .entry(t.s.clone())
                                        .or_insert_with(|| ExchangeItem {
                                            name: t.s.clone(),
                                            ..Default::default()
                                        });
                                    item.trade24_count = parse_f64(&t.q);
                                    if let Some((interval, max_rate)) = funding_info.get(&t.s) {
                                        item.rate_interval = Some(*interval);
                                        item.rate_max = Some(max_rate.clone());
                                    }
                                }
                                section.dirty = true;
                            } else if stream.contains("markPrice") {
                                let updates: Vec<MarkPriceUpdate> =
                                    match serde_json::from_value(combined.data) {
                                        Ok(v) => v,
                                        Err(e) => {
                                            tracing::warn!(
                                                "binance futures chunk-{}: parse markPrice array error: {}",
                                                chunk_id, e
                                            );
                                            continue;
                                        }
                                    };
                                let mut section = cache.binance_future.write().await;
                                section.ts = ts;
                                for u in &updates {
                                    if !valid_set.contains(&u.s) {
                                        continue;
                                    }
                                    let rate_dec = parse_f64(&u.r);
                                    let rate_str = format!("{:.3}", rate_dec * 100.0);
                                    let item = section
                                        .items
                                        .entry(u.s.clone())
                                        .or_insert_with(|| ExchangeItem {
                                            name: u.s.clone(),
                                            ..Default::default()
                                        });
                                    item.rate = Some(rate_str);
                                    item.mark_price = Some(u.p.clone());
                                    item.index_price = Some(u.i.clone());
                                    if let Some((interval, max_rate)) = funding_info.get(&u.s) {
                                        item.rate_interval = Some(*interval);
                                        item.rate_max = Some(max_rate.clone());
                                    }
                                }
                                section.dirty = true;
                            } else if stream.contains("@depth20") {
                                // stream = "{symbol_lower}@depth20@500ms"
                                let sym_upper = stream
                                    .split('@')
                                    .next()
                                    .unwrap_or("")
                                    .to_uppercase();
                                if sym_upper.is_empty() || !valid_set.contains(&sym_upper) {
                                    continue;
                                }

                                let bids_raw = combined.data.get("b")
                                    .and_then(|v| v.as_array());
                                let asks_raw = combined.data.get("a")
                                    .and_then(|v| v.as_array());

                                let parse_levels = |arr: &[serde_json::Value]| -> Vec<PriceLevel> {
                                    arr.iter()
                                        .take(20)
                                        .filter_map(|entry| {
                                            let pair = entry.as_array()?;
                                            if pair.len() < 2 { return None; }
                                            Some(PriceLevel {
                                                price: parse_f64(pair[0].as_str().unwrap_or("0")),
                                                qty: parse_f64(pair[1].as_str().unwrap_or("0")),
                                            })
                                        })
                                        .collect()
                                };

                                let mut updated = false;
                                {
                                    let mut section = cache.binance_future.write().await;
                                    let item = section
                                        .items
                                        .entry(sym_upper.clone())
                                        .or_insert_with(|| {
                                            let mut ei = ExchangeItem {
                                                name: sym_upper.clone(),
                                                ..Default::default()
                                            };
                                            if let Some((interval, max_rate)) = funding_info.get(&sym_upper) {
                                                ei.rate_interval = Some(*interval);
                                                ei.rate_max = Some(max_rate.clone());
                                            }
                                            ei
                                        });
                                    if let Some(b) = bids_raw {
                                        item.bids = parse_levels(b);
                                    }
                                    if let Some(a) = asks_raw {
                                        item.asks = parse_levels(a);
                                    }
                                    item.depth_ts = ts;
                                    // Derive BBO from depth best levels
                                    if let Some(best) = item.asks.first() { item.a = best.price; }
                                    if let Some(best) = item.bids.first() { item.b = best.price; }
                                    item.ts = ts;
                                    section.dirty = true;
                                    updated = true;
                                }
                                if updated {
                                    let now_inst = tokio::time::Instant::now();
                                    let should_fire = depth_throttle.get(&sym_upper)
                                        .map_or(true, |&last| now_inst.duration_since(last) >= std::time::Duration::from_millis(500));
                                    if should_fire {
                                        depth_throttle.insert(sym_upper.clone(), now_inst);
                                        let _ = cache.ticker_tx.send(crate::spread::TickerChanged { symbol: sym_upper.clone() });
                                    }
                                }
                            } else {
                                tracing::warn!("binance futures chunk-{}: unknown stream: {}", chunk_id, stream);
                            }
                        }
                        _ = ping_interval.tick() => {
                            if let Err(e) = write.send(Message::Ping(vec![].into())).await {
                                tracing::error!("binance futures chunk-{}: ping send error: {}", chunk_id, e);
                                break;
                            }
                        }
                        _ = tokio::time::sleep_until(read_deadline) => {
                            tracing::warn!("binance futures chunk-{}: no message for 30s, reconnecting", chunk_id);
                            break;
                        }
                        _ = tokio::time::sleep_until(connect_time + std::time::Duration::from_secs(85500)) => {
                            tracing::info!("binance futures chunk-{}: approaching 24h limit, proactively reconnecting", chunk_id);
                            break;
                        }
                    }
                }
            }
            Ok(Err(e)) => {
                tracing::error!("binance futures chunk-{}: connection failed: {}", chunk_id, e);
            }
            Err(_) => {
                tracing::error!("binance futures chunk-{}: connection timed out", chunk_id);
            }
        }
        // Don't clear cache — coordinator handles that
        backoff_sleep(attempt).await;
        attempt = attempt.saturating_add(1);
    }
}

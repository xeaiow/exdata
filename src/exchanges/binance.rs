use crate::cache::SharedCache;
use crate::exchanges::{backoff_sleep, now_ms, parse_f64};
use crate::models::ExchangeItem;
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
struct FuturesBookTicker {
    s: String,
    a: String,
    b: String,
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
    let resp = match client.get(url).send().await {
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
    let resp = match client.get(url).send().await {
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

// ── Futures ─────────────────────────────────────────────────────────────────

pub async fn run_future(cache: SharedCache, client: reqwest::Client) {
    let mut attempt: u32 = 0;
    loop {
        tracing::info!("binance futures: fetching funding info...");
        let mut funding_info = fetch_funding_info(&client).await;
        tracing::info!(
            "binance futures: loaded funding info for {} symbols",
            funding_info.len()
        );

        tracing::info!("binance futures: connecting...");
        let url = "wss://fstream.binance.com/stream?streams=!ticker@arr/!markPrice@arr@1s/!bookTicker";
        match connect_async(url).await {
            Ok((ws, _)) => {
                attempt = 0;
                tracing::info!("binance futures: connected");

                // Seed bid/ask from REST before entering the WS loop
                let book_tickers = fetch_book_tickers(&client).await;
                if !book_tickers.is_empty() {
                    let mut section = cache.binance_future.write().await;
                    for bt in &book_tickers {
                        if !bt.symbol.ends_with("USDT") {
                            continue;
                        }
                        let item =
                            section.items.entry(bt.symbol.clone()).or_insert_with(|| {
                                ExchangeItem {
                                    name: bt.symbol.clone(),
                                    ..Default::default()
                                }
                            });
                        item.a = parse_f64(&bt.ask_price);
                        item.b = parse_f64(&bt.bid_price);
                        item.ts = now_ms();
                    }
                    tracing::info!(
                        "binance futures: seeded {} book tickers from REST",
                        section.items.len()
                    );
                    section.serialize_cache();
                }

                let (mut write, mut read) = ws.split();

                let mut refresh_interval =
                    tokio::time::interval(std::time::Duration::from_secs(300));
                refresh_interval.tick().await; // consume the immediate first tick

                let mut ping_interval =
                    tokio::time::interval(std::time::Duration::from_secs(30));
                ping_interval.tick().await; // consume the immediate first tick

                let mut read_deadline =
                    tokio::time::Instant::now() + std::time::Duration::from_secs(30);

                loop {
                    tokio::select! {
                        msg = read.next() => {
                            read_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
                            let msg = match msg {
                                Some(Ok(m)) => m,
                                Some(Err(e)) => {
                                    tracing::warn!("binance futures: read error: {}", e);
                                    break;
                                }
                                None => {
                                    tracing::warn!("binance futures: stream ended");
                                    break;
                                }
                            };

                            let text = match msg {
                                Message::Text(t) => t,
                                Message::Ping(_) | Message::Pong(_) => continue,
                                Message::Close(_) => {
                                    tracing::warn!("binance futures: server closed connection");
                                    break;
                                }
                                _ => continue,
                            };

                            let combined: CombinedMsg = match serde_json::from_str(&text) {
                                Ok(v) => v,
                                Err(e) => {
                                    tracing::warn!("binance futures: parse combined msg error: {}", e);
                                    continue;
                                }
                            };

                            let stream = combined.stream.as_str();
                            let ts = now_ms();

                            if stream == "!bookTicker" {
                                // Single object
                                let bt: FuturesBookTicker = match serde_json::from_value(combined.data) {
                                    Ok(v) => v,
                                    Err(e) => {
                                        tracing::warn!("binance futures: parse bookTicker error: {}", e);
                                        continue;
                                    }
                                };
                                if !bt.s.ends_with("USDT") {
                                    continue;
                                }
                                let mut section = cache.binance_future.write().await;
                                section.ts = ts;
                                let item = section
                                    .items
                                    .entry(bt.s.clone())
                                    .or_insert_with(|| ExchangeItem {
                                        name: bt.s.clone(),
                                        ..Default::default()
                                    });
                                item.a = parse_f64(&bt.a);
                                item.b = parse_f64(&bt.b);
                                item.ts = ts;
                                // Apply funding info
                                if let Some((interval, max_rate)) = funding_info.get(&bt.s) {
                                    item.rate_interval = Some(*interval);
                                    item.rate_max = Some(max_rate.clone());
                                }
                                section.serialize_cache();
                            } else if stream == "!ticker@arr" {
                                // Array of ticker objects
                                let tickers: Vec<FuturesTicker> =
                                    match serde_json::from_value(combined.data) {
                                        Ok(v) => v,
                                        Err(e) => {
                                            tracing::warn!(
                                                "binance futures: parse ticker array error: {}",
                                                e
                                            );
                                            continue;
                                        }
                                    };
                                let mut section = cache.binance_future.write().await;
                                section.ts = ts;
                                for t in &tickers {
                                    if !t.s.ends_with("USDT") {
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
                                section.serialize_cache();
                            } else if stream.contains("markPrice") {
                                // Array of markPriceUpdate objects
                                let updates: Vec<MarkPriceUpdate> =
                                    match serde_json::from_value(combined.data) {
                                        Ok(v) => v,
                                        Err(e) => {
                                            tracing::warn!(
                                                "binance futures: parse markPrice array error: {}",
                                                e
                                            );
                                            continue;
                                        }
                                    };
                                let mut section = cache.binance_future.write().await;
                                section.ts = ts;
                                for u in &updates {
                                    if !u.s.ends_with("USDT") {
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
                                section.serialize_cache();
                            } else {
                                tracing::warn!("binance futures: unknown stream: {}", stream);
                            }
                        }
                        _ = ping_interval.tick() => {
                            if let Err(e) = write.send(Message::Ping(vec![].into())).await {
                                tracing::error!("binance futures: ping send error: {}", e);
                                break;
                            }
                        }
                        _ = tokio::time::sleep_until(read_deadline) => {
                            tracing::warn!("binance futures: no message for 30s, reconnecting");
                            break;
                        }
                        _ = refresh_interval.tick() => {
                            tracing::info!("binance futures: refreshing instrument list...");
                            let new_funding = fetch_funding_info(&client).await;
                            if new_funding.is_empty() {
                                tracing::warn!("binance futures: refresh returned empty funding info, skipping");
                                continue;
                            }
                            let new_book = fetch_book_tickers(&client).await;

                            // Build valid symbol set from funding info keys
                            let valid_symbols: HashSet<&String> = new_funding.keys().collect();
                            tracing::info!(
                                "binance futures: refresh found {} symbols",
                                valid_symbols.len()
                            );

                            // Clean stale cache entries and seed new book tickers
                            {
                                let mut section = cache.binance_future.write().await;
                                let before = section.items.len();
                                section.items.retain(|k, _| valid_symbols.contains(k));
                                let removed = before - section.items.len();
                                if removed > 0 {
                                    tracing::info!(
                                        "binance futures: removed {} stale cache entries",
                                        removed
                                    );
                                }

                                // Seed bid/ask for new symbols and update existing items with a=0
                                let seed_ts = now_ms();
                                for bt in &new_book {
                                    if !bt.symbol.ends_with("USDT") {
                                        continue;
                                    }
                                    let item = section
                                        .items
                                        .entry(bt.symbol.clone())
                                        .or_insert_with(|| ExchangeItem {
                                            name: bt.symbol.clone(),
                                            ..Default::default()
                                        });
                                    // Only seed from REST if bid/ask is still zero
                                    // (avoid overwriting fresher WS data)
                                    if item.a == 0.0 && item.b == 0.0 {
                                        item.a = parse_f64(&bt.ask_price);
                                        item.b = parse_f64(&bt.bid_price);
                                        item.ts = seed_ts;
                                    }
                                }
                                section.serialize_cache();
                            }

                            funding_info = new_funding;
                        }
                    }
                }
            }
            Err(e) => {
                tracing::error!("binance futures: connection failed: {}", e);
            }
        }
        backoff_sleep(attempt).await;
        attempt = attempt.saturating_add(1);
    }
}

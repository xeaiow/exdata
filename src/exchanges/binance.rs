use crate::cache::SharedCache;
use crate::exchanges::{backoff_sleep, now_ms, parse_f64};
use crate::models::ExchangeItem;
use futures_util::StreamExt;
use serde::Deserialize;
use std::collections::HashMap;
use tokio_tungstenite::{connect_async, tungstenite::Message};

// ── Serde structs ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SpotTicker {
    s: String,
    a: String,
    b: String,
    q: String,
}

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

// ── Spot ────────────────────────────────────────────────────────────────────

pub async fn run_spot(cache: SharedCache) {
    let mut attempt: u32 = 0;
    loop {
        tracing::info!("binance spot: connecting...");
        let url = "wss://stream.binance.com:9443/ws/!ticker@arr";
        match connect_async(url).await {
            Ok((ws, _)) => {
                attempt = 0;
                tracing::info!("binance spot: connected");
                let (_, mut read) = ws.split();

                while let Some(msg) = read.next().await {
                    let text = match msg {
                        Ok(Message::Text(t)) => t,
                        Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => continue,
                        Ok(Message::Close(_)) => {
                            tracing::warn!("binance spot: server closed connection");
                            break;
                        }
                        Ok(_) => continue,
                        Err(e) => {
                            tracing::warn!("binance spot: read error: {}", e);
                            break;
                        }
                    };

                    let tickers: Vec<SpotTicker> = match serde_json::from_str(&text) {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!("binance spot: parse error: {}", e);
                            continue;
                        }
                    };

                    let ts = now_ms();
                    let mut section = cache.binance_spot.write().await;
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
                        item.a = parse_f64(&t.a);
                        item.b = parse_f64(&t.b);
                        item.trade24_count = parse_f64(&t.q);
                    }
                }
            }
            Err(e) => {
                tracing::error!("binance spot: connection failed: {}", e);
            }
        }
        backoff_sleep(attempt).await;
        attempt = attempt.saturating_add(1);
    }
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

// ── Futures ─────────────────────────────────────────────────────────────────

pub async fn run_future(cache: SharedCache, client: reqwest::Client) {
    let mut attempt: u32 = 0;
    loop {
        tracing::info!("binance futures: fetching funding info...");
        let funding_info = fetch_funding_info(&client).await;
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
                let (_, mut read) = ws.split();

                while let Some(msg) = read.next().await {
                    let text = match msg {
                        Ok(Message::Text(t)) => t,
                        Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => continue,
                        Ok(Message::Close(_)) => {
                            tracing::warn!("binance futures: server closed connection");
                            break;
                        }
                        Ok(_) => continue,
                        Err(e) => {
                            tracing::warn!("binance futures: read error: {}", e);
                            break;
                        }
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
                        // Apply funding info
                        if let Some((interval, max_rate)) = funding_info.get(&bt.s) {
                            item.rate_interval = Some(*interval);
                            item.rate_max = Some(max_rate.clone());
                        }
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
                    } else {
                        tracing::warn!("binance futures: unknown stream: {}", stream);
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

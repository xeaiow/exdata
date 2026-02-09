use crate::cache::SharedCache;
use crate::exchanges::{backoff_sleep, now_ms, parse_f64};
use crate::models::{ExchangeItem, PriceLevel};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use tokio_tungstenite::{connect_async, tungstenite::Message};

// ── REST serde structs ──────────────────────────────────────────────────────

#[derive(Deserialize)]
struct InstrumentsResponse {
    result: InstrumentsResult,
}

#[derive(Deserialize)]
struct InstrumentsResult {
    list: Vec<InstrumentInfo>,
    #[serde(rename = "nextPageCursor", default)]
    next_page_cursor: Option<String>,
}

#[derive(Deserialize)]
struct InstrumentInfo {
    symbol: String,
    status: String,
    #[serde(rename = "quoteCoin")]
    quote_coin: String,
    #[serde(rename = "contractType", default)]
    contract_type: Option<String>,
    #[serde(rename = "fundingInterval", default)]
    funding_interval: Option<u64>,
    #[serde(rename = "upperFundingRate", default)]
    upper_funding_rate: Option<String>,
}

// ── WS serde structs ────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct WsMsg {
    topic: Option<String>,
    #[serde(default)]
    data: Option<serde_json::Value>,
    #[serde(default)]
    ts: Option<u64>,
    #[serde(default)]
    op: Option<String>,
    #[serde(rename = "type", default)]
    msg_type: Option<String>,
}

#[derive(Deserialize)]
struct TickerData {
    symbol: String,
    #[serde(rename = "ask1Price", default)]
    ask1_price: Option<String>,
    #[serde(rename = "bid1Price", default)]
    bid1_price: Option<String>,
    #[serde(rename = "turnover24h", default)]
    turnover_24h: Option<String>,
    #[serde(rename = "fundingRate", default)]
    funding_rate: Option<String>,
    #[serde(rename = "markPrice", default)]
    mark_price: Option<String>,
    #[serde(rename = "indexPrice", default)]
    index_price: Option<String>,
}

// ── REST helpers ─────────────────────────────────────────────────────────────

/// Fetch ticker snapshot (bid1Price, ask1Price, turnover24h).
/// The WS `tickers` stream is push-on-change only, so illiquid symbols would stay at a=0/b=0.
#[derive(Deserialize)]
struct TickersResponse {
    result: TickersResult,
}

#[derive(Deserialize)]
struct TickersResult {
    list: Vec<RestTickerItem>,
}

#[derive(Deserialize)]
struct RestTickerItem {
    symbol: String,
    #[serde(rename = "bid1Price", default)]
    bid1_price: String,
    #[serde(rename = "ask1Price", default)]
    ask1_price: String,
    #[serde(rename = "turnover24h", default)]
    turnover_24h: String,
}

async fn fetch_futures_tickers(client: &reqwest::Client) -> Vec<RestTickerItem> {
    let url = "https://openapi.zoomex.com/cloud/trade/v3/market/tickers?category=linear";
    let resp = match client.get(url).timeout(std::time::Duration::from_secs(10)).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("zoomex futures: tickers fetch failed: {}", e);
            return Vec::new();
        }
    };
    let body: TickersResponse = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("zoomex futures: tickers parse failed: {}", e);
            return Vec::new();
        }
    };
    body.result.list
}

/// Funding info for futures: symbol -> (interval_hours, rate_max).
struct FuturesInstrumentData {
    symbols: Vec<String>,
    funding_info: HashMap<String, (u32, String)>,
}

/// Fetch funding rate caps from Bybit's v5 API (Zoomex's v3 API lacks upperFundingRate).
/// Returns symbol -> max rate percentage string (e.g. "2.000").
async fn fetch_bybit_funding_caps(client: &reqwest::Client) -> HashMap<String, String> {
    let mut caps = HashMap::new();
    let mut cursor: Option<String> = None;

    loop {
        let mut url =
            "https://api.bybit.com/v5/market/instruments-info?category=linear&limit=1000"
                .to_string();
        if let Some(ref c) = cursor {
            if !c.is_empty() {
                url.push_str(&format!("&cursor={}", c));
            }
        }

        let resp = match client.get(&url).timeout(std::time::Duration::from_secs(10)).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("zoomex futures: bybit funding caps fetch failed: {}", e);
                break;
            }
        };

        let body: InstrumentsResponse = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("zoomex futures: bybit funding caps parse failed: {}", e);
                break;
            }
        };

        for info in &body.result.list {
            if let Some(ref rate) = info.upper_funding_rate {
                let v = parse_f64(rate);
                caps.insert(info.symbol.clone(), format!("{:.3}", v * 100.0));
            }
        }

        match body.result.next_page_cursor {
            Some(ref c) if !c.is_empty() => cursor = Some(c.clone()),
            _ => break,
        }
    }

    caps
}

/// Fetch linear perpetual instrument info (USDT quote, LinearPerpetual, Trading).
async fn fetch_futures_instruments(client: &reqwest::Client) -> FuturesInstrumentData {
    let mut symbols = Vec::new();
    let mut funding_info: HashMap<String, (u32, String)> = HashMap::new();
    let mut cursor: Option<String> = None;

    // Fetch funding rate caps from Bybit (Zoomex v3 API lacks this field)
    let bybit_caps = fetch_bybit_funding_caps(client).await;
    if !bybit_caps.is_empty() {
        tracing::info!(
            "zoomex futures: loaded {} funding rate caps from Bybit",
            bybit_caps.len()
        );
    }

    loop {
        let mut url =
            "https://openapi.zoomex.com/cloud/trade/v3/market/instruments-info?category=linear&limit=1000"
                .to_string();
        if let Some(ref c) = cursor {
            if !c.is_empty() {
                url.push_str(&format!("&cursor={}", c));
            }
        }

        let resp = match client.get(&url).timeout(std::time::Duration::from_secs(10)).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("zoomex futures: instruments request failed: {}", e);
                break;
            }
        };

        let body: InstrumentsResponse = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("zoomex futures: instruments parse failed: {}", e);
                break;
            }
        };

        for info in &body.result.list {
            let is_perp = info
                .contract_type
                .as_deref() == Some("LinearPerpetual");
            if info.quote_coin == "USDT" && info.status == "Trading" && is_perp && info.symbol != "ALLUSDT" {
                symbols.push(info.symbol.clone());
                let interval_hours = info.funding_interval.unwrap_or(480) / 60;
                let rate_max = bybit_caps
                    .get(&info.symbol)
                    .cloned()
                    .unwrap_or_else(|| "--".to_string());
                funding_info.insert(
                    info.symbol.clone(),
                    (interval_hours as u32, rate_max),
                );
            }
        }

        match body.result.next_page_cursor {
            Some(ref c) if !c.is_empty() => cursor = Some(c.clone()),
            _ => break,
        }
    }

    FuturesInstrumentData {
        symbols,
        funding_info,
    }
}

// ── Futures (coordinator + chunk workers) ───────────────────────────────────

/// Coordinator: fetches instruments, seeds REST, spawns chunk workers, restarts every 5 min.
pub async fn run_future(cache: SharedCache, client: reqwest::Client) {
    let mut attempt: u32 = 0;

    loop {
        tracing::info!("zoomex futures: fetching instrument list...");
        let instr = fetch_futures_instruments(&client).await;
        tracing::info!(
            "zoomex futures: found {} USDT perpetual symbols",
            instr.symbols.len()
        );

        if instr.symbols.is_empty() {
            tracing::warn!("zoomex futures: no symbols found, retrying...");
            backoff_sleep(attempt).await;
            attempt = attempt.saturating_add(1);
            continue;
        }
        attempt = 0;

        let current_symbols: HashSet<String> = instr.symbols.iter().cloned().collect();

        // Seed bid/ask from REST
        let futures_tickers = fetch_futures_tickers(&client).await;
        if !futures_tickers.is_empty() {
            let ts = now_ms();
            let mut section = cache.zoomex_future.write().await;
            for t in &futures_tickers {
                if !current_symbols.contains(&t.symbol) {
                    continue;
                }
                let item = section
                    .items
                    .entry(t.symbol.clone())
                    .or_insert_with(|| {
                        let mut ei = ExchangeItem {
                            name: t.symbol.clone(),
                            ..Default::default()
                        };
                        if let Some((interval, max_rate)) = instr.funding_info.get(&t.symbol) {
                            ei.rate_interval = Some(*interval);
                            ei.rate_max = Some(max_rate.clone());
                        }
                        ei
                    });
                item.a = parse_f64(&t.ask1_price);
                item.b = parse_f64(&t.bid1_price);
                item.ts = ts;
                item.trade24_count = parse_f64(&t.turnover_24h);
            }
            tracing::info!(
                "zoomex futures: seeded {} tickers from REST",
                section.items.len()
            );
            section.serialize_cache();
        }

        // Split symbols into chunks of 50 and spawn a worker for each
        let funding_info = std::sync::Arc::new(instr.funding_info);
        let chunks: Vec<Vec<String>> = instr
            .symbols
            .chunks(50)
            .map(|c| c.to_vec())
            .collect();
        let num_chunks = chunks.len();
        tracing::info!(
            "zoomex futures: spawning {} chunk workers ({} symbols)",
            num_chunks,
            instr.symbols.len()
        );

        let mut handles = Vec::with_capacity(num_chunks);
        for (idx, chunk_symbols) in chunks.into_iter().enumerate() {
            let cache = cache.clone();
            let funding = funding_info.clone();
            let handle = tokio::spawn(run_chunk(cache, chunk_symbols, funding, idx));
            handles.push(handle);
        }

        // Sleep 5 minutes, then abort all chunks and restart
        tokio::time::sleep(std::time::Duration::from_secs(300)).await;

        tracing::info!("zoomex futures: coordinator restarting, aborting {} chunks", handles.len());
        for h in &handles {
            h.abort();
        }
        for h in handles {
            let _ = h.await;
        }

        // Clear cache before full restart
        cache.zoomex_future.write().await.clear();
    }
}

/// Chunk worker: manages a single WS connection subscribing to ≤50 symbols.
/// Self-reconnects on disconnect; no refresh logic (coordinator handles that).
async fn run_chunk(
    cache: SharedCache,
    symbols: Vec<String>,
    funding_info: std::sync::Arc<HashMap<String, (u32, String)>>,
    chunk_id: usize,
) {
    let mut attempt: u32 = 0;

    loop {
        tracing::info!("zoomex futures chunk-{}: connecting ({} symbols)...", chunk_id, symbols.len());
        let url = "wss://stream.zoomex.com/v5/public/linear";
        let ws_result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            connect_async(url),
        ).await;
        match ws_result {
            Ok(Ok((ws, _))) => {
                attempt = 0;
                tracing::info!("zoomex futures chunk-{}: connected", chunk_id);
                let (mut write, mut read) = ws.split();

                // Subscribe tickers in batches of 10
                let mut subscribe_failed = false;
                for chunk in symbols.chunks(10) {
                    let args: Vec<String> =
                        chunk.iter().map(|s| format!("tickers.{}", s)).collect();
                    let sub = serde_json::json!({
                        "op": "subscribe",
                        "args": args
                    });
                    if let Err(e) = write.send(Message::Text(sub.to_string().into())).await {
                        tracing::error!("zoomex futures chunk-{}: subscribe send error: {}", chunk_id, e);
                        subscribe_failed = true;
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                // Subscribe orderbook.50 in separate batches of 10
                if !subscribe_failed {
                    for chunk in symbols.chunks(10) {
                        let args: Vec<String> =
                            chunk.iter().map(|s| format!("orderbook.50.{}", s)).collect();
                        let sub = serde_json::json!({
                            "op": "subscribe",
                            "args": args
                        });
                        if let Err(e) = write.send(Message::Text(sub.to_string().into())).await {
                            tracing::error!("zoomex futures chunk-{}: orderbook subscribe send error: {}", chunk_id, e);
                            subscribe_failed = true;
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }

                if subscribe_failed {
                    tracing::warn!("zoomex futures chunk-{}: subscribe failed, reconnecting...", chunk_id);
                } else {

                let mut ping_interval = tokio::time::interval(std::time::Duration::from_secs(20));
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
                                    tracing::warn!("zoomex futures chunk-{}: read error: {}", chunk_id, e);
                                    break;
                                }
                                None => {
                                    tracing::warn!("zoomex futures chunk-{}: stream ended", chunk_id);
                                    break;
                                }
                            };

                            let text = match msg {
                                Message::Text(t) => t,
                                Message::Ping(_) | Message::Pong(_) => continue,
                                Message::Close(_) => {
                                    tracing::warn!("zoomex futures chunk-{}: server closed connection", chunk_id);
                                    break;
                                }
                                _ => continue,
                            };

                            let ws_msg: WsMsg = match serde_json::from_str(&text) {
                                Ok(v) => v,
                                Err(e) => {
                                    tracing::warn!("zoomex futures chunk-{}: parse error: {}", chunk_id, e);
                                    continue;
                                }
                            };

                            // Skip non-ticker messages (pong responses, subscribe acks)
                            if ws_msg.op.is_some() {
                                continue;
                            }

                            let topic = match ws_msg.topic {
                                Some(ref t) => t.clone(),
                                None => continue,
                            };

                            let data_val = match ws_msg.data {
                                Some(v) => v,
                                None => continue,
                            };

                            if topic.starts_with("orderbook.50.") {
                                // Only process snapshot (ignore delta to avoid partial overwrites)
                                if ws_msg.msg_type.as_deref() != Some("snapshot") {
                                    continue;
                                }
                                let symbol = topic.strip_prefix("orderbook.50.").unwrap_or("").to_string();
                                if symbol.is_empty() {
                                    continue;
                                }

                                let parse_levels = |key: &str| -> Vec<PriceLevel> {
                                    data_val.get(key)
                                        .and_then(|v| v.as_array())
                                        .map(|arr| {
                                            arr.iter()
                                                .take(5)
                                                .filter_map(|entry| {
                                                    let pair = entry.as_array()?;
                                                    if pair.len() < 2 { return None; }
                                                    Some(PriceLevel {
                                                        price: parse_f64(pair[0].as_str().unwrap_or("0")),
                                                        qty: parse_f64(pair[1].as_str().unwrap_or("0")),
                                                    })
                                                })
                                                .collect()
                                        })
                                        .unwrap_or_default()
                                };

                                let bids = parse_levels("b");
                                let asks = parse_levels("a");

                                if !bids.is_empty() || !asks.is_empty() {
                                    let mut section = cache.zoomex_future.write().await;
                                    if let Some(item) = section.items.get_mut(&symbol) {
                                        item.bids = bids;
                                        item.asks = asks;
                                    }
                                    section.dirty = true;
                                }
                                continue;
                            }

                            if !topic.starts_with("tickers.") {
                                continue;
                            }

                            let ticker: TickerData = match serde_json::from_value(data_val) {
                                Ok(v) => v,
                                Err(e) => {
                                    tracing::warn!("zoomex futures chunk-{}: parse ticker error: {}", chunk_id, e);
                                    continue;
                                }
                            };

                            let ts = ws_msg.ts.unwrap_or_else(now_ms);

                            let mut section = cache.zoomex_future.write().await;
                            section.ts = ts;

                            let item = section
                                .items
                                .entry(ticker.symbol.clone())
                                .or_insert_with(|| {
                                    let mut ei = ExchangeItem {
                                        name: ticker.symbol.clone(),
                                        ..Default::default()
                                    };
                                    if let Some((interval, max_rate)) = funding_info.get(&ticker.symbol) {
                                        ei.rate_interval = Some(*interval);
                                        ei.rate_max = Some(max_rate.clone());
                                    }
                                    ei
                                });

                            // Delta messages only have partial fields; only update what's present
                            if let Some(ref v) = ticker.ask1_price {
                                item.a = parse_f64(v);
                                item.ts = ts;
                            }
                            if let Some(ref v) = ticker.bid1_price {
                                item.b = parse_f64(v);
                                item.ts = ts;
                            }
                            if let Some(ref v) = ticker.turnover_24h {
                                item.trade24_count = parse_f64(v);
                            }
                            if let Some(ref v) = ticker.funding_rate {
                                let rate_dec = parse_f64(v);
                                item.rate = Some(format!("{:.3}", rate_dec * 100.0));
                            }
                            if let Some(ref v) = ticker.mark_price {
                                item.mark_price = Some(v.clone());
                            }
                            if let Some(ref v) = ticker.index_price {
                                item.index_price = Some(v.clone());
                            }

                            // Ensure funding info stays applied
                            if item.rate_interval.is_none() {
                                if let Some((interval, max_rate)) = funding_info.get(&ticker.symbol) {
                                    item.rate_interval = Some(*interval);
                                    item.rate_max = Some(max_rate.clone());
                                }
                            }
                            section.dirty = true;
                            drop(section);
                            let _ = cache.ticker_tx.send(crate::spread::TickerChanged {
                                symbol: ticker.symbol.clone(),
                            });
                        }
                        _ = tokio::time::sleep_until(read_deadline) => {
                            tracing::warn!("zoomex futures chunk-{}: no message for 30s, reconnecting", chunk_id);
                            break;
                        }
                        _ = ping_interval.tick() => {
                            let ping = serde_json::json!({"op": "ping"});
                            if let Err(e) = write.send(Message::Text(ping.to_string().into())).await {
                                tracing::error!("zoomex futures chunk-{}: ping send error: {}", chunk_id, e);
                                break;
                            }
                        }
                    }
                }
                } // else !subscribe_failed
            }
            Ok(Err(e)) => {
                tracing::error!("zoomex futures chunk-{}: connection failed: {}", chunk_id, e);
            }
            Err(_) => {
                tracing::error!("zoomex futures chunk-{}: connection timed out", chunk_id);
            }
        }
        // Don't clear cache here — other chunks are still running
        backoff_sleep(attempt).await;
        attempt = attempt.saturating_add(1);
    }
}

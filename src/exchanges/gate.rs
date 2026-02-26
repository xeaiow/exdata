use crate::cache::SharedCache;
use crate::exchanges::{backoff_sleep, json_f64, now_ms, parse_f64};
use crate::models::{ExchangeItem, PriceLevel};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use tokio_tungstenite::{connect_async, tungstenite::Message};

// ── REST serde structs (Futures) ───────────────────────────────────────────

#[derive(Deserialize)]
struct FuturesContract {
    name: String,
    in_delisting: bool,
    funding_interval: u64,
    #[serde(default)]
    funding_rate_limit: Option<String>,
}

/// Contract info from REST: (rate_interval_hours, rate_max_pct)
struct ContractInfo {
    symbols: Vec<String>,
    funding_info: HashMap<String, (u32, String)>,
}

// ── WS serde structs ───────────────────────────────────────────────────────

#[derive(Deserialize)]
struct GateWsMsg {
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    event: Option<String>,
    #[serde(default)]
    result: Option<serde_json::Value>,
}

// ── Symbol normalization ───────────────────────────────────────────────────

/// "BTC_USDT" -> "BTCUSDT"
fn normalize_symbol(s: &str) -> String {
    s.replace('_', "")
}

// ── REST helpers ───────────────────────────────────────────────────────────

/// Fetch futures ticker snapshot so every symbol has bid/ask from the start.
#[derive(Deserialize)]
struct GateFuturesTicker {
    contract: String,
    #[serde(default)]
    lowest_ask: String,
    #[serde(default)]
    highest_bid: String,
    #[serde(default)]
    volume_24h_quote: String,
}

async fn fetch_futures_tickers(client: &reqwest::Client) -> Vec<GateFuturesTicker> {
    let url = "https://api.gateio.ws/api/v4/futures/usdt/tickers";
    let resp = match client.get(url).timeout(std::time::Duration::from_secs(10)).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("gate futures: tickers fetch failed: {}", e);
            return Vec::new();
        }
    };
    match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("gate futures: tickers parse failed: {}", e);
            Vec::new()
        }
    }
}

/// Fetch futures contracts: NOT in_delisting.
async fn fetch_futures_contracts(client: &reqwest::Client) -> ContractInfo {
    let url = "https://api.gateio.ws/api/v4/futures/usdt/contracts";
    let resp = match client.get(url).timeout(std::time::Duration::from_secs(10)).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("gate futures: contracts request failed: {}", e);
            return ContractInfo {
                symbols: Vec::new(),
                funding_info: HashMap::new(),
            };
        }
    };

    let contracts: Vec<FuturesContract> = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("gate futures: contracts parse failed: {}", e);
            return ContractInfo {
                symbols: Vec::new(),
                funding_info: HashMap::new(),
            };
        }
    };

    let mut symbols = Vec::new();
    let mut funding_info = HashMap::new();

    for c in contracts {
        if c.in_delisting || normalize_symbol(&c.name) == "ALLUSDT" {
            continue;
        }

        let interval_hours = (c.funding_interval / 3600) as u32;
        let rate_max = match c.funding_rate_limit {
            Some(ref limit) => format!("{:.3}", parse_f64(limit) * 100.0),
            None => "--".to_string(),
        };

        funding_info.insert(c.name.clone(), (interval_hours, rate_max));
        symbols.push(c.name);
    }

    ContractInfo {
        symbols,
        funding_info,
    }
}

// ── Funding info refresher ───────────────────────────────────────────────────

/// Periodically refreshes funding info (rate_interval, rate_max) without restarting WS.
async fn funding_refresher(cache: SharedCache, client: reqwest::Client) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
    interval.tick().await; // skip immediate tick (coordinator just seeded)

    loop {
        interval.tick().await;
        let contract_info = fetch_futures_contracts(&client).await;
        if contract_info.funding_info.is_empty() {
            continue;
        }
        let mut section = cache.gate_future.write().await;
        for (raw_name, (interval_h, max_rate)) in &contract_info.funding_info {
            let normalized = normalize_symbol(raw_name);
            if let Some(item) = section.items.get_mut(&normalized) {
                item.rate_interval = Some(*interval_h);
                item.rate_max = Some(max_rate.clone());
            }
        }
    }
}

// ── Futures (coordinator + chunk workers) ─────────────────────────────────

/// Coordinator: fetches contracts, seeds REST, spawns chunk workers, restarts every 5 min.
pub async fn run_future(cache: SharedCache, client: reqwest::Client) {
    let mut attempt: u32 = 0;

    loop {
        tracing::info!("gate futures: fetching contracts...");
        let contract_info = fetch_futures_contracts(&client).await;
        tracing::info!(
            "gate futures: found {} active contracts",
            contract_info.symbols.len()
        );

        if contract_info.symbols.is_empty() {
            tracing::warn!("gate futures: no contracts found, retrying...");
            backoff_sleep(attempt).await;
            attempt = attempt.saturating_add(1);
            continue;
        }
        attempt = 0;

        let current_symbols: HashSet<String> = contract_info.symbols.iter().cloned().collect();

        // Seed bid/ask from REST
        let futures_tickers = fetch_futures_tickers(&client).await;
        if !futures_tickers.is_empty() {
            let ts = now_ms();
            let mut section = cache.gate_future.write().await;
            for t in &futures_tickers {
                if !current_symbols.contains(&t.contract) {
                    continue;
                }
                let normalized = normalize_symbol(&t.contract);
                let item = section
                    .items
                    .entry(normalized.clone())
                    .or_insert_with(|| ExchangeItem {
                        name: normalized,
                        ..Default::default()
                    });
                item.a = parse_f64(&t.lowest_ask);
                item.b = parse_f64(&t.highest_bid);
                item.ts = ts;
                item.trade24_count = parse_f64(&t.volume_24h_quote);
                // Always refresh funding info from REST (exchange may change intervals dynamically)
                if let Some((interval, max_rate)) =
                    contract_info.funding_info.get(&t.contract)
                {
                    item.rate_interval = Some(*interval);
                    item.rate_max = Some(max_rate.clone());
                }
            }
            tracing::info!(
                "gate futures: seeded {} tickers from REST",
                section.items.len()
            );
            section.serialize_cache();
        }

        // Split symbols into chunks of 50 and spawn a worker for each
        let funding_info = std::sync::Arc::new(contract_info.funding_info);
        let chunks: Vec<Vec<String>> = contract_info
            .symbols
            .chunks(50)
            .map(|c| c.to_vec())
            .collect();
        let num_chunks = chunks.len();
        tracing::info!(
            "gate futures: spawning {} chunk workers ({} symbols)",
            num_chunks,
            contract_info.symbols.len()
        );

        let mut handles = Vec::with_capacity(num_chunks);
        for (idx, chunk_symbols) in chunks.into_iter().enumerate() {
            let cache = cache.clone();
            let funding = funding_info.clone();
            let handle = tokio::spawn(run_chunk(cache, chunk_symbols, funding, idx));
            handles.push(handle);
        }

        // Spawn funding info refresher (updates rate_interval/rate_max every 60s)
        let refresher_handle = tokio::spawn(funding_refresher(cache.clone(), client.clone()));

        // Sleep 5 minutes, then abort all chunks + refresher and restart
        tokio::time::sleep(std::time::Duration::from_secs(300)).await;

        tracing::info!("gate futures: coordinator restarting, aborting {} chunks", handles.len());
        refresher_handle.abort();
        let _ = refresher_handle.await;
        for h in &handles {
            h.abort();
        }
        for h in handles {
            let _ = h.await;
        }
    }
}

/// Chunk worker: manages a single WS connection subscribing to ≤50 symbols.
/// Subscribes to both futures.tickers and futures.book_ticker channels.
/// Self-reconnects on disconnect; no refresh logic (coordinator handles that).
async fn run_chunk(
    cache: SharedCache,
    symbols: Vec<String>,
    funding_info: std::sync::Arc<HashMap<String, (u32, String)>>,
    chunk_id: usize,
) {
    let mut attempt: u32 = 0;

    loop {
        tracing::info!("gate futures chunk-{}: connecting ({} symbols)...", chunk_id, symbols.len());
        let url = "wss://fx-ws.gateio.ws/v4/ws/usdt";
        let ws_result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            connect_async(url),
        ).await;
        match ws_result {
            Ok(Ok((ws, _))) => {
                attempt = 0;
                tracing::info!("gate futures chunk-{}: connected", chunk_id);
                let (mut write, mut read) = ws.split();

                // Subscribe to futures.tickers
                let mut subscribe_failed = false;
                {
                    let payload: Vec<&str> = symbols.iter().map(|s| s.as_str()).collect();
                    let now_secs = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs();
                    let sub = serde_json::json!({
                        "time": now_secs,
                        "channel": "futures.tickers",
                        "event": "subscribe",
                        "payload": payload
                    });
                    if let Err(e) = write.send(Message::Text(sub.to_string().into())).await {
                        tracing::error!("gate futures chunk-{}: tickers subscribe send error: {}", chunk_id, e);
                        subscribe_failed = true;
                    }
                }

                // Subscribe to futures.book_ticker
                if !subscribe_failed {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    let payload: Vec<&str> = symbols.iter().map(|s| s.as_str()).collect();
                    let now_secs = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs();
                    let sub = serde_json::json!({
                        "time": now_secs,
                        "channel": "futures.book_ticker",
                        "event": "subscribe",
                        "payload": payload
                    });
                    if let Err(e) = write.send(Message::Text(sub.to_string().into())).await {
                        tracing::error!("gate futures chunk-{}: book_ticker subscribe send error: {}", chunk_id, e);
                        subscribe_failed = true;
                    }
                }

                // Subscribe to futures.order_book (snapshot, depth=20, one symbol per message)
                if !subscribe_failed {
                    for symbol in &symbols {
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        let now_secs = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_secs();
                        let sub = serde_json::json!({
                            "time": now_secs,
                            "channel": "futures.order_book",
                            "event": "subscribe",
                            "payload": [symbol, "20", "0"]
                        });
                        if let Err(e) = write.send(Message::Text(sub.to_string().into())).await {
                            tracing::error!("gate futures chunk-{}: order_book subscribe send error: {}", chunk_id, e);
                            subscribe_failed = true;
                            break;
                        }
                    }
                }

                if subscribe_failed {
                    tracing::warn!("gate futures chunk-{}: subscribe failed, reconnecting...", chunk_id);
                } else {

                let mut ping_interval =
                    tokio::time::interval(std::time::Duration::from_secs(15));
                ping_interval.tick().await; // consume the immediate first tick
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
                                    tracing::warn!("gate futures chunk-{}: read error: {}", chunk_id, e);
                                    break;
                                }
                                None => {
                                    tracing::warn!("gate futures chunk-{}: stream ended", chunk_id);
                                    break;
                                }
                            };

                            let text = match msg {
                                Message::Text(t) => t,
                                Message::Ping(_) | Message::Pong(_) => continue,
                                Message::Close(_) => {
                                    tracing::warn!("gate futures chunk-{}: server closed connection", chunk_id);
                                    break;
                                }
                                _ => continue,
                            };

                            let text_str: &str = text.as_ref();

                            let ws_msg: GateWsMsg = match serde_json::from_str(text_str) {
                                Ok(v) => v,
                                Err(e) => {
                                    tracing::warn!("gate futures chunk-{}: parse error: {}", chunk_id, e);
                                    continue;
                                }
                            };

                            // Skip non-data messages (subscribe confirmations, etc.)
                            match ws_msg.event.as_deref() {
                                Some("update") | Some("all") => {}
                                _ => continue,
                            }

                            let channel = match ws_msg.channel.as_deref() {
                                Some(c) => c,
                                None => continue,
                            };

                            let result = match ws_msg.result {
                                Some(v) => v,
                                None => continue,
                            };

                            let ts = now_ms();

                            match channel {
                                "futures.tickers" => {
                                    let items: Vec<serde_json::Value> = match serde_json::from_value(result)
                                    {
                                        Ok(v) => v,
                                        Err(e) => {
                                            tracing::warn!(
                                                "gate futures chunk-{}: parse tickers array error: {}",
                                                chunk_id, e
                                            );
                                            continue;
                                        }
                                    };

                                    let mut section = cache.gate_future.write().await;
                                    section.ts = ts;

                                    for ticker in &items {
                                        let contract = ticker
                                            .get("contract")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("");
                                        if contract.is_empty() {
                                            continue;
                                        }

                                        let normalized = normalize_symbol(contract);

                                        let volume_val = ticker.get("volume_24h_quote")
                                            .map(json_f64).unwrap_or(0.0);

                                        let rate_dec = ticker.get("funding_rate")
                                            .map(json_f64).unwrap_or(0.0);
                                        let rate_str = format!("{:.3}", rate_dec * 100.0);
                                        let mark_price_str = ticker.get("mark_price")
                                            .and_then(|v| v.as_str().map(|s| s.to_string())
                                                .or_else(|| v.as_f64().map(|n| format!("{}", n))))
                                            .unwrap_or_default();
                                        let index_price_str = ticker.get("index_price")
                                            .and_then(|v| v.as_str().map(|s| s.to_string())
                                                .or_else(|| v.as_f64().map(|n| format!("{}", n))))
                                            .unwrap_or_default();

                                        let item = section
                                            .items
                                            .entry(normalized.clone())
                                            .or_insert_with(|| {
                                                let mut ei = ExchangeItem {
                                                    name: normalized,
                                                    ..Default::default()
                                                };
                                                if let Some((interval, max_rate)) =
                                                    funding_info.get(contract)
                                                {
                                                    ei.rate_interval = Some(*interval);
                                                    ei.rate_max = Some(max_rate.clone());
                                                }
                                                ei
                                            });

                                        item.trade24_count = volume_val;
                                        item.rate = Some(rate_str);
                                        if !mark_price_str.is_empty() {
                                            item.mark_price = Some(mark_price_str);
                                        }
                                        if !index_price_str.is_empty() {
                                            item.index_price = Some(index_price_str);
                                        }

                                        if item.rate_interval.is_none() {
                                            if let Some((interval, max_rate)) =
                                                funding_info.get(contract)
                                            {
                                                item.rate_interval = Some(*interval);
                                                item.rate_max = Some(max_rate.clone());
                                            }
                                        }
                                    }
                                    section.dirty = true;
                                }
                                "futures.book_ticker" => {
                                    let contract = result
                                        .get("s")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");
                                    if contract.is_empty() {
                                        continue;
                                    }

                                    let normalized = normalize_symbol(contract);
                                    let symbol_for_event = normalized.clone();

                                    let ask = result
                                        .get("a")
                                        .map(json_f64)
                                        .unwrap_or(0.0);
                                    let bid = result
                                        .get("b")
                                        .map(json_f64)
                                        .unwrap_or(0.0);

                                    let mut section = cache.gate_future.write().await;
                                    section.ts = ts;

                                    let item = section
                                        .items
                                        .entry(normalized.clone())
                                        .or_insert_with(|| {
                                            let mut ei = ExchangeItem {
                                                name: normalized,
                                                ..Default::default()
                                            };
                                            if let Some((interval, max_rate)) =
                                                funding_info.get(contract)
                                            {
                                                ei.rate_interval = Some(*interval);
                                                ei.rate_max = Some(max_rate.clone());
                                            }
                                            ei
                                        });

                                    item.a = ask;
                                    item.b = bid;
                                    item.ts = ts;

                                    if item.rate_interval.is_none() {
                                        if let Some((interval, max_rate)) =
                                            funding_info.get(contract)
                                        {
                                            item.rate_interval = Some(*interval);
                                            item.rate_max = Some(max_rate.clone());
                                        }
                                    }
                                    section.dirty = true;
                                    drop(section);
                                    let _ = cache.ticker_tx.send(crate::spread::TickerChanged {
                                        symbol: symbol_for_event,
                                    });
                                }
                                "futures.order_book" => {
                                    let contract = result
                                        .get("contract")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");
                                    if contract.is_empty() {
                                        continue;
                                    }

                                    let normalized = normalize_symbol(contract);

                                    // Gate order_book format: {"asks": [{"p":"price","s":"size"},...], "bids": [...]}
                                    let parse_levels = |key: &str| -> Vec<PriceLevel> {
                                        result.get(key)
                                            .and_then(|v| v.as_array())
                                            .map(|arr| {
                                                arr.iter()
                                                    .take(20)
                                                    .filter_map(|entry| {
                                                        let o = entry.as_object()?;
                                                        let price = o.get("p")
                                                            .and_then(|v| v.as_str())
                                                            .map(parse_f64)
                                                            .unwrap_or(0.0);
                                                        let qty = o.get("s")
                                                            .and_then(|v| v.as_str())
                                                            .map(parse_f64)
                                                            .or_else(|| o.get("s").and_then(|v| v.as_f64()))
                                                            .unwrap_or(0.0);
                                                        Some(PriceLevel { price, qty })
                                                    })
                                                    .collect()
                                            })
                                            .unwrap_or_default()
                                    };

                                    let asks = parse_levels("asks");
                                    let bids = parse_levels("bids");

                                    if !asks.is_empty() || !bids.is_empty() {
                                        let mut updated = false;
                                        {
                                            let mut section = cache.gate_future.write().await;
                                            if let Some(item) = section.items.get_mut(&normalized) {
                                                item.asks = asks;
                                                item.bids = bids;
                                                item.depth_ts = now_ms();
                                                section.dirty = true;
                                                updated = true;
                                            }
                                        }
                                        if updated {
                                            let now_inst = tokio::time::Instant::now();
                                            let should_fire = depth_throttle.get(&normalized)
                                                .map_or(true, |&last| now_inst.duration_since(last) >= std::time::Duration::from_millis(500));
                                            if should_fire {
                                                depth_throttle.insert(normalized.clone(), now_inst);
                                                let _ = cache.ticker_tx.send(crate::spread::TickerChanged { symbol: normalized.clone() });
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                        _ = tokio::time::sleep_until(read_deadline) => {
                            tracing::warn!("gate futures chunk-{}: no message for 30s, reconnecting", chunk_id);
                            break;
                        }
                        _ = ping_interval.tick() => {
                            let now_secs = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap()
                                .as_secs();
                            let ping = serde_json::json!({
                                "time": now_secs,
                                "channel": "futures.ping"
                            });
                            if let Err(e) = write.send(Message::Text(ping.to_string().into())).await {
                                tracing::error!("gate futures chunk-{}: ping send error: {}", chunk_id, e);
                                break;
                            }
                        }
                    }
                }
                } // else !subscribe_failed
            }
            Ok(Err(e)) => {
                tracing::error!("gate futures chunk-{}: connection failed: {}", chunk_id, e);
            }
            Err(_) => {
                tracing::error!("gate futures chunk-{}: connection timed out", chunk_id);
            }
        }
        // Don't clear cache here — other chunks are still running
        backoff_sleep(attempt).await;
        attempt = attempt.saturating_add(1);
    }
}

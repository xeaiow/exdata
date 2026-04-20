use crate::cache::SharedCache;
use crate::exchanges::{backoff_sleep, json_f64, now_ms, parse_f64};
use crate::models::{ExchangeItem, PriceLevel};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use tokio_tungstenite::{connect_async, tungstenite::Message};

// ── REST serde structs ──────────────────────────────────────────────────────

#[derive(Deserialize)]
struct FuturesContractsResponse {
    data: Vec<FuturesContractInfo>,
}

#[derive(Deserialize)]
struct FuturesContractInfo {
    symbol: String,
    #[serde(rename = "symbolStatus")]
    status: String,
    #[serde(rename = "fundInterval", default)]
    fund_interval: Option<String>,
}

// ── WS serde structs ────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct WsMsg {
    arg: Option<WsArg>,
    data: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    event: Option<String>,
}

#[derive(Deserialize)]
struct WsArg {
    #[serde(rename = "instType")]
    _inst_type: String,
    channel: String,
    #[serde(rename = "instId")]
    inst_id: String,
}

// ── REST helpers ────────────────────────────────────────────────────────────

/// Fetch ticker snapshot so every symbol has bid/ask from the start.
/// The WS `ticker` channel is push-on-change only, so illiquid symbols would stay at a=0/b=0.
#[derive(Deserialize)]
struct SpotTickersResponse {
    data: Vec<SpotTickerItem>,
}

#[derive(Deserialize)]
struct SpotTickerItem {
    symbol: String,
    #[serde(rename = "askPr", default)]
    ask_pr: String,
    #[serde(rename = "bidPr", default)]
    bid_pr: String,
    #[serde(rename = "quoteVolume", default)]
    quote_volume: String,
}

async fn fetch_futures_tickers(client: &reqwest::Client) -> Vec<SpotTickerItem> {
    let url = "https://api.bitget.com/api/v2/mix/market/tickers?productType=USDT-FUTURES";
    let resp = match client.get(url).timeout(std::time::Duration::from_secs(10)).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("bitget futures: tickers fetch failed: {}", e);
            return Vec::new();
        }
    };
    let body: SpotTickersResponse = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("bitget futures: tickers parse failed: {}", e);
            return Vec::new();
        }
    };
    body.data
}

/// Fetch funding info from the current-fund-rate endpoint.
/// Returns symbol -> (interval_hours, rate_max_pct).
/// This endpoint provides both interval and max rate, superseding contract-level funding data.
async fn fetch_funding_rates_info(client: &reqwest::Client) -> HashMap<String, (u32, String)> {
    #[derive(Deserialize)]
    struct FundRateResponse {
        data: Vec<FundRateItem>,
    }
    #[derive(Deserialize)]
    struct FundRateItem {
        symbol: String,
        #[serde(rename = "fundingRateInterval", default)]
        funding_rate_interval: Option<String>,
        #[serde(rename = "maxFundingRate", default)]
        max_funding_rate: Option<String>,
    }

    let url = "https://api.bitget.com/api/v2/mix/market/current-fund-rate?productType=USDT-FUTURES";
    let resp = match client.get(url).timeout(std::time::Duration::from_secs(10)).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("bitget futures: funding rates fetch failed: {}", e);
            return HashMap::new();
        }
    };
    let body: FundRateResponse = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("bitget futures: funding rates parse failed: {}", e);
            return HashMap::new();
        }
    };
    let mut map = HashMap::with_capacity(body.data.len());
    for item in body.data {
        let interval: u32 = item
            .funding_rate_interval
            .as_deref()
            .unwrap_or("8")
            .parse()
            .unwrap_or(8);
        let max_rate = match item.max_funding_rate {
            Some(ref v) => format!("{:.3}", parse_f64(v) * 100.0),
            None => "--".to_string(),
        };
        map.insert(item.symbol, (interval, max_rate));
    }
    map
}

/// Funding info for futures: symbol -> (interval_hours, rate_max_str).
struct FuturesInstrumentData {
    symbols: Vec<String>,
    funding_info: HashMap<String, (u32, String)>,
}

/// Fetch futures contracts (USDT-FUTURES, normal status).
async fn fetch_futures_contracts(client: &reqwest::Client) -> FuturesInstrumentData {
    let url = "https://api.bitget.com/api/v2/mix/market/contracts?productType=USDT-FUTURES";
    let resp = match client.get(url).timeout(std::time::Duration::from_secs(10)).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("bitget futures: contracts request failed: {}", e);
            return FuturesInstrumentData {
                symbols: Vec::new(),
                funding_info: HashMap::new(),
            };
        }
    };

    let body: FuturesContractsResponse = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("bitget futures: contracts parse failed: {}", e);
            return FuturesInstrumentData {
                symbols: Vec::new(),
                funding_info: HashMap::new(),
            };
        }
    };

    let mut symbols = Vec::new();
    let mut funding_info: HashMap<String, (u32, String)> = HashMap::new();

    for contract in body.data {
        if contract.status != "normal" || contract.symbol == "ALLUSDT" {
            continue;
        }

        let interval_hours: u32 = contract
            .fund_interval
            .as_deref()
            .unwrap_or("8")
            .parse()
            .unwrap_or(8);

        let rate_max_str = "--".to_string();

        symbols.push(contract.symbol.clone());
        funding_info.insert(contract.symbol, (interval_hours, rate_max_str));
    }

    FuturesInstrumentData {
        symbols,
        funding_info,
    }
}

// ── Funding info refresher ───────────────────────────────────────────────────

/// Periodically refreshes funding info (rate_interval, rate_max) without restarting WS.
/// Bitget requires two API calls: contracts (for interval) + funding rates (for max rate).
async fn funding_refresher(cache: SharedCache, client: reqwest::Client) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
    interval.tick().await; // skip immediate tick (coordinator just seeded)

    loop {
        interval.tick().await;
        let mut instr = fetch_futures_contracts(&client).await;
        if instr.funding_info.is_empty() {
            continue;
        }
        let fund_info = fetch_funding_rates_info(&client).await;
        for (sym, (fi_interval, max_rate)) in &fund_info {
            if let Some(info) = instr.funding_info.get_mut(sym) {
                info.0 = *fi_interval;
                info.1 = max_rate.clone();
            }
        }
        let mut section = cache.bitget_future.write().await;
        for (sym, (interval_h, max_rate)) in &instr.funding_info {
            if let Some(item) = section.items.get_mut(sym) {
                item.rate_interval = Some(*interval_h);
                item.rate_max = Some(max_rate.clone());
            }
        }
    }
}

// ── Futures (coordinator + chunk workers) ───────────────────────────────────

/// Coordinator: fetches contracts, seeds REST, spawns chunk workers, restarts every 5 min.
pub async fn run_future(cache: SharedCache, client: reqwest::Client) {
    let mut attempt: u32 = 0;

    loop {
        tracing::info!("bitget futures: fetching contracts list...");
        let mut instr = fetch_futures_contracts(&client).await;
        tracing::info!(
            "bitget futures: found {} USDT-FUTURES contracts",
            instr.symbols.len()
        );

        if instr.symbols.is_empty() {
            tracing::warn!("bitget futures: no contracts found, retrying...");
            backoff_sleep(attempt).await;
            attempt = attempt.saturating_add(1);
            continue;
        }
        attempt = 0;

        // Fetch funding info (interval + max rate) and merge into funding_info
        let fund_info = fetch_funding_rates_info(&client).await;
        if !fund_info.is_empty() {
            tracing::info!("bitget futures: loaded funding info for {} symbols", fund_info.len());
            for (sym, (interval, max_rate)) in &fund_info {
                if let Some(info) = instr.funding_info.get_mut(sym) {
                    info.0 = *interval;
                    info.1 = max_rate.clone();
                }
            }
        }

        let current_symbols: HashSet<String> = instr.symbols.iter().cloned().collect();

        // Seed bid/ask from REST
        let futures_tickers = fetch_futures_tickers(&client).await;
        if !futures_tickers.is_empty() {
            let ts = now_ms();
            let mut section = cache.bitget_future.write().await;
            for t in &futures_tickers {
                if !current_symbols.contains(&t.symbol) {
                    continue;
                }
                let item = section
                    .items
                    .entry(t.symbol.clone())
                    .or_insert_with(|| ExchangeItem {
                        name: t.symbol.clone(),
                        ..Default::default()
                    });
                item.a = parse_f64(&t.ask_pr);
                item.b = parse_f64(&t.bid_pr);
                item.ts = ts;
                item.trade24_count = parse_f64(&t.quote_volume);
                // Always refresh funding info from REST (exchange may change intervals dynamically)
                if let Some((interval, max_rate)) = instr.funding_info.get(&t.symbol) {
                    item.rate_interval = Some(*interval);
                    item.rate_max = Some(max_rate.clone());
                }
            }
            tracing::info!(
                "bitget futures: seeded {} tickers from REST",
                section.items.len()
            );
            section.serialize_cache();
        }
        cache.bitget_future.write().await.restarting = false;

        // Split symbols into chunks of 50 and spawn a worker for each
        let funding_info = std::sync::Arc::new(instr.funding_info);
        let chunks: Vec<Vec<String>> = instr
            .symbols
            .chunks(50)
            .map(|c| c.to_vec())
            .collect();
        let num_chunks = chunks.len();
        tracing::info!(
            "bitget futures: spawning {} chunk workers ({} symbols)",
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

        // Spawn funding info refresher (updates rate_interval/rate_max every 60s)
        let refresher_handle = tokio::spawn(funding_refresher(cache.clone(), client.clone()));

        // Sleep 5 minutes, then abort all chunks + refresher and restart
        tokio::time::sleep(std::time::Duration::from_secs(300)).await;

        tracing::info!("bitget futures: coordinator restarting, aborting {} chunks", handles.len());
        cache.bitget_future.write().await.restarting = true;
        refresher_handle.abort();
        let _ = refresher_handle.await;
        for h in &handles {
            h.abort();
        }
        // Wait for all to finish (aborted or otherwise)
        for h in handles {
            let _ = h.await;
        }
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
        tracing::info!("bitget futures chunk-{}: connecting ({} symbols)...", chunk_id, symbols.len());
        let url = "wss://ws.bitget.com/v2/ws/public";
        let ws_result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            connect_async(url),
        ).await;
        match ws_result {
            Ok(Ok((ws, _))) => {
                attempt = 0;
                tracing::info!("bitget futures chunk-{}: connected", chunk_id);
                let (mut write, mut read) = ws.split();

                // Subscribe all symbols in one batch (≤50)
                let mut args: Vec<serde_json::Value> = symbols
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "instType": "USDT-FUTURES",
                            "channel": "ticker",
                            "instId": s
                        })
                    })
                    .collect();
                // Also subscribe to books15 for depth data
                args.extend(symbols.iter().map(|s| {
                    serde_json::json!({
                        "instType": "USDT-FUTURES",
                        "channel": "books15",
                        "instId": s
                    })
                }));
                let sub = serde_json::json!({
                    "op": "subscribe",
                    "args": args
                });
                if let Err(e) = write.send(Message::Text(sub.to_string().into())).await {
                    tracing::error!("bitget futures chunk-{}: subscribe send error: {}", chunk_id, e);
                    backoff_sleep(attempt).await;
                    attempt = attempt.saturating_add(1);
                    continue;
                }

                let mut ping_interval =
                    tokio::time::interval(std::time::Duration::from_secs(30));
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
                                    tracing::warn!("bitget futures chunk-{}: read error: {}", chunk_id, e);
                                    break;
                                }
                                None => {
                                    tracing::warn!("bitget futures chunk-{}: stream ended", chunk_id);
                                    break;
                                }
                            };

                            let text = match msg {
                                Message::Text(t) => t,
                                Message::Ping(_) | Message::Pong(_) => continue,
                                Message::Close(_) => {
                                    tracing::warn!("bitget futures chunk-{}: server closed connection", chunk_id);
                                    break;
                                }
                                _ => continue,
                            };

                            let text_str: &str = text.as_ref();

                            // Skip pong responses
                            if text_str == "pong" {
                                continue;
                            }

                            let ws_msg: WsMsg = match serde_json::from_str(text_str) {
                                Ok(v) => v,
                                Err(e) => {
                                    tracing::warn!("bitget futures chunk-{}: parse error: {}", chunk_id, e);
                                    continue;
                                }
                            };

                            // Skip subscribe confirmations (event field present)
                            if ws_msg.event.is_some() {
                                continue;
                            }

                            let arg = match ws_msg.arg {
                                Some(ref a) => a,
                                None => continue,
                            };

                            let data = match ws_msg.data {
                                Some(ref d) if !d.is_empty() => &d[0],
                                _ => continue,
                            };

                            if arg.channel == "books15" || arg.channel == "books5" {
                                let inst_id = data
                                    .get("instId")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or(&arg.inst_id);

                                // Bitget books format: {"asks": [[price, qty], ...], "bids": ...}
                                let parse_levels = |key: &str| -> Vec<PriceLevel> {
                                    data.get(key)
                                        .and_then(|v| v.as_array())
                                        .map(|arr| {
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
                                        })
                                        .unwrap_or_default()
                                };

                                let asks = parse_levels("asks");
                                let bids = parse_levels("bids");

                                if !asks.is_empty() || !bids.is_empty() {
                                    let inst_id_owned = inst_id.to_string();
                                    {
                                        let mut section = cache.bitget_future.write().await;
                                        let item = section
                                            .items
                                            .entry(inst_id_owned.clone())
                                            .or_insert_with(|| {
                                                let mut ei = ExchangeItem {
                                                    name: inst_id_owned.clone(),
                                                    ..Default::default()
                                                };
                                                if let Some((interval, max_rate)) = funding_info.get(inst_id) {
                                                    ei.rate_interval = Some(*interval);
                                                    ei.rate_max = Some(max_rate.clone());
                                                }
                                                ei
                                            });
                                        item.asks = asks;
                                        item.bids = bids;
                                        item.depth_ts = now_ms();
                                        // Derive BBO from depth best levels
                                        if let Some(best) = item.asks.first() { item.a = best.price; }
                                        if let Some(best) = item.bids.first() { item.b = best.price; }
                                        item.ts = item.depth_ts;
                                        section.dirty = true;
                                    }
                                    {
                                        let now_inst = tokio::time::Instant::now();
                                        let should_fire = depth_throttle.get(&inst_id_owned)
                                            .map_or(true, |&last| now_inst.duration_since(last) >= std::time::Duration::from_millis(500));
                                        if should_fire {
                                            depth_throttle.insert(inst_id_owned.clone(), now_inst);
                                            let _ = cache.ticker_tx.send(crate::spread::TickerChanged { symbol: inst_id_owned });
                                        }
                                    }
                                }
                                continue;
                            }

                            if arg.channel != "ticker" {
                                continue;
                            }

                            let inst_id = data
                                .get("instId")
                                .and_then(|v| v.as_str())
                                .unwrap_or(&arg.inst_id);

                            // bid/ask now driven by depth (books15) stream
                            let quote_volume = data.get("quoteVolume").map(json_f64).unwrap_or(0.0);
                            let rate_dec = data.get("fundingRate").map(json_f64).unwrap_or(0.0);
                            let mark_price = data.get("markPrice").map(json_f64).unwrap_or(0.0);
                            let index_price = data.get("indexPrice").map(json_f64).unwrap_or(0.0);

                            let rate_str = format!("{:.3}", rate_dec * 100.0);

                            let mut section = cache.bitget_future.write().await;

                            let item = section
                                .items
                                .entry(inst_id.to_string())
                                .or_insert_with(|| {
                                    let mut ei = ExchangeItem {
                                        name: inst_id.to_string(),
                                        ..Default::default()
                                    };
                                    if let Some((interval, max_rate)) = funding_info.get(inst_id) {
                                        ei.rate_interval = Some(*interval);
                                        ei.rate_max = Some(max_rate.clone());
                                    }
                                    ei
                                });

                            item.trade24_count = quote_volume;
                            item.rate = Some(rate_str);
                            item.mark_price = Some(format!("{}", mark_price));
                            item.index_price = Some(format!("{}", index_price));

                            // Ensure funding info stays applied
                            if item.rate_interval.is_none() {
                                if let Some((interval, max_rate)) = funding_info.get(inst_id) {
                                    item.rate_interval = Some(*interval);
                                    item.rate_max = Some(max_rate.clone());
                                }
                            }
                            section.dirty = true;
                            // No TickerChanged here -- depth updates drive spread recalc
                        }
                        _ = tokio::time::sleep_until(read_deadline) => {
                            tracing::warn!("bitget futures chunk-{}: no message for 30s, reconnecting", chunk_id);
                            break;
                        }
                        _ = ping_interval.tick() => {
                            // Bitget uses plain text "ping" for heartbeat
                            if let Err(e) = write.send(Message::Text("ping".into())).await {
                                tracing::error!("bitget futures chunk-{}: ping send error: {}", chunk_id, e);
                                break;
                            }
                        }
                    }
                }
            }
            Ok(Err(e)) => {
                tracing::error!("bitget futures chunk-{}: connection failed: {}", chunk_id, e);
            }
            Err(_) => {
                tracing::error!("bitget futures chunk-{}: connection timed out", chunk_id);
            }
        }
        // Don't clear cache here — other chunks are still running
        backoff_sleep(attempt).await;
        attempt = attempt.saturating_add(1);
    }
}

use crate::cache::SharedCache;
use crate::exchanges::{backoff_sleep, now_ms, parse_f64};
use crate::models::ExchangeItem;
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

/// Fetch max funding rates from the current-fund-rate endpoint.
/// Returns symbol -> maxFundingRate (as percentage string, e.g. "0.300").
async fn fetch_max_funding_rates(client: &reqwest::Client) -> HashMap<String, String> {
    #[derive(Deserialize)]
    struct FundRateResponse {
        data: Vec<FundRateItem>,
    }
    #[derive(Deserialize)]
    struct FundRateItem {
        symbol: String,
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
        if let Some(ref max_rate) = item.max_funding_rate {
            let v = parse_f64(max_rate);
            map.insert(item.symbol, format!("{:.3}", v * 100.0));
        }
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
        if contract.status != "normal" {
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

        // Fetch max funding rates and merge into funding_info
        let max_rates = fetch_max_funding_rates(&client).await;
        if !max_rates.is_empty() {
            tracing::info!("bitget futures: loaded max funding rates for {} symbols", max_rates.len());
            for (sym, max_rate) in &max_rates {
                if let Some(info) = instr.funding_info.get_mut(sym) {
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
                item.a = parse_f64(&t.ask_pr);
                item.b = parse_f64(&t.bid_pr);
                item.ts = ts;
                item.trade24_count = parse_f64(&t.quote_volume);
            }
            tracing::info!(
                "bitget futures: seeded {} tickers from REST",
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

        // Sleep 5 minutes, then abort all chunks and restart
        tokio::time::sleep(std::time::Duration::from_secs(300)).await;

        tracing::info!("bitget futures: coordinator restarting, aborting {} chunks", handles.len());
        for h in &handles {
            h.abort();
        }
        // Wait for all to finish (aborted or otherwise)
        for h in handles {
            let _ = h.await;
        }

        // Clear cache before full restart
        cache.bitget_future.write().await.clear();
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
                let args: Vec<serde_json::Value> = symbols
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "instType": "USDT-FUTURES",
                            "channel": "ticker",
                            "instId": s
                        })
                    })
                    .collect();
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

                            if arg.channel != "ticker" {
                                continue;
                            }

                            let data = match ws_msg.data {
                                Some(ref d) if !d.is_empty() => &d[0],
                                _ => continue,
                            };

                            let inst_id = data
                                .get("instId")
                                .and_then(|v| v.as_str())
                                .unwrap_or(&arg.inst_id);

                            let ask_pr = data
                                .get("askPr")
                                .and_then(|v| v.as_str())
                                .unwrap_or("0");
                            let bid_pr = data
                                .get("bidPr")
                                .and_then(|v| v.as_str())
                                .unwrap_or("0");
                            let quote_volume = data
                                .get("quoteVolume")
                                .and_then(|v| v.as_str())
                                .unwrap_or("0");
                            let funding_rate = data
                                .get("fundingRate")
                                .and_then(|v| v.as_str())
                                .unwrap_or("0");
                            let mark_price = data
                                .get("markPrice")
                                .and_then(|v| v.as_str())
                                .unwrap_or("0");
                            let index_price = data
                                .get("indexPrice")
                                .and_then(|v| v.as_str())
                                .unwrap_or("0");

                            let rate_dec = parse_f64(funding_rate);
                            let rate_str = format!("{:.3}", rate_dec * 100.0);

                            let ts = now_ms();
                            let mut section = cache.bitget_future.write().await;
                            section.ts = ts;

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

                            item.a = parse_f64(ask_pr);
                            item.b = parse_f64(bid_pr);
                            item.ts = ts;
                            item.trade24_count = parse_f64(quote_volume);
                            item.rate = Some(rate_str);
                            item.mark_price = Some(mark_price.to_string());
                            item.index_price = Some(index_price.to_string());

                            // Ensure funding info stays applied
                            if item.rate_interval.is_none() {
                                if let Some((interval, max_rate)) = funding_info.get(inst_id) {
                                    item.rate_interval = Some(*interval);
                                    item.rate_max = Some(max_rate.clone());
                                }
                            }
                            section.serialize_cache();
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

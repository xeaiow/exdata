use crate::cache::SharedCache;
use crate::exchanges::{backoff_sleep, json_f64, now_ms, parse_f64};
use crate::models::{ExchangeItem, PriceLevel};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::collections::HashSet;
use tokio_tungstenite::{connect_async, tungstenite::Message};

// ── REST serde structs ──────────────────────────────────────────────────────

#[derive(Deserialize)]
struct OkxResponse {
    data: Vec<OkxInstrument>,
}

#[derive(Deserialize)]
struct OkxInstrument {
    #[serde(rename = "instId")]
    inst_id: String,
    state: String,
    #[serde(rename = "settleCcy", default)]
    settle_ccy: Option<String>,
    #[serde(rename = "ctType", default)]
    ct_type: Option<String>,
}

// ── WS serde structs ────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct WsMsg {
    arg: Option<WsArg>,
    data: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    event: Option<String>,
    #[serde(default)]
    code: Option<String>,
}

#[derive(Deserialize)]
struct WsArg {
    channel: String,
    #[serde(rename = "instId")]
    inst_id: String,
}

// ── Symbol normalization helpers ────────────────────────────────────────────

/// Remove all hyphens: "BTC-USDT" → "BTCUSDT", "BTC-USDT-SWAP" → "BTCUSDTSWAP"
fn remove_hyphens(s: &str) -> String {
    s.replace('-', "")
}

/// Normalize swap instId: "BTC-USDT-SWAP" → "BTCUSDT"
fn normalize_swap(inst_id: &str) -> String {
    let base = inst_id.strip_suffix("-SWAP").unwrap_or(inst_id);
    remove_hyphens(base)
}

// ── REST helpers ────────────────────────────────────────────────────────────

/// Fetch ticker snapshot so every symbol has bid/ask from the start.
/// The WS `tickers` channel is push-on-change only, so illiquid symbols would stay at a=0/b=0.
#[derive(Deserialize)]
struct OkxTickersResponse {
    data: Vec<OkxTickerItem>,
}

#[derive(Deserialize)]
struct OkxTickerItem {
    #[serde(rename = "instId")]
    inst_id: String,
    #[serde(rename = "askPx", default)]
    ask_px: String,
    #[serde(rename = "bidPx", default)]
    bid_px: String,
    #[serde(rename = "volCcy24h", default)]
    vol_ccy_24h: String,
}

async fn fetch_swap_tickers(client: &reqwest::Client) -> Vec<OkxTickerItem> {
    let url = "https://www.okx.com/api/v5/market/tickers?instType=SWAP";
    let resp = match client.get(url).timeout(std::time::Duration::from_secs(10)).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("okx futures: tickers fetch failed: {}", e);
            return Vec::new();
        }
    };
    let body: OkxTickersResponse = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("okx futures: tickers parse failed: {}", e);
            return Vec::new();
        }
    };
    body.data
}

/// Fetch swap instrument instIds (USDT settle, linear, live state).
async fn fetch_swap_instruments(client: &reqwest::Client) -> Vec<String> {
    let url = "https://www.okx.com/api/v5/public/instruments?instType=SWAP";
    let resp = match client.get(url).timeout(std::time::Duration::from_secs(10)).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("okx futures: instruments request failed: {}", e);
            return Vec::new();
        }
    };

    let body: OkxResponse = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("okx futures: instruments parse failed: {}", e);
            return Vec::new();
        }
    };

    body.data
        .into_iter()
        .filter(|i| {
            i.state == "live"
                && i.settle_ccy.as_deref() == Some("USDT")
                && i.ct_type.as_deref() == Some("linear")
                && normalize_swap(&i.inst_id) != "ALLUSDT"
        })
        .map(|i| i.inst_id)
        .collect()
}

// ── Futures (coordinator + chunk workers) ────────────────────────────────────

/// Coordinator: fetches instruments, seeds REST, spawns chunk workers, restarts every 5 min.
pub async fn run_future(cache: SharedCache, client: reqwest::Client) {
    let mut attempt: u32 = 0;

    loop {
        tracing::info!("okx futures: fetching instrument list...");
        let instruments = fetch_swap_instruments(&client).await;
        tracing::info!(
            "okx futures: found {} USDT linear swap instruments",
            instruments.len()
        );

        if instruments.is_empty() {
            tracing::warn!("okx futures: no instruments found, retrying...");
            backoff_sleep(attempt).await;
            attempt = attempt.saturating_add(1);
            continue;
        }
        attempt = 0;

        // Seed bid/ask from REST
        let swap_tickers = fetch_swap_tickers(&client).await;
        if !swap_tickers.is_empty() {
            let instruments_set: HashSet<&String> = instruments.iter().collect();
            let ts = now_ms();
            let mut section = cache.okx_future.write().await;
            for t in &swap_tickers {
                if !instruments_set.contains(&t.inst_id) {
                    continue;
                }
                let normalized = normalize_swap(&t.inst_id);
                let item = section
                    .items
                    .entry(normalized.clone())
                    .or_insert_with(|| ExchangeItem {
                        name: normalized,
                        ..Default::default()
                    });
                item.a = parse_f64(&t.ask_px);
                item.b = parse_f64(&t.bid_px);
                item.ts = ts;
                item.trade24_count = parse_f64(&t.vol_ccy_24h);
            }
            tracing::info!(
                "okx futures: seeded {} tickers from REST",
                section.items.len()
            );
            section.serialize_cache();
        }
        cache.okx_future.write().await.restarting = false;

        // Split instruments into chunks of 50 and spawn a worker for each
        let chunks: Vec<Vec<String>> = instruments
            .chunks(50)
            .map(|c| c.to_vec())
            .collect();
        let num_chunks = chunks.len();
        tracing::info!(
            "okx futures: spawning {} chunk workers ({} symbols)",
            num_chunks,
            instruments.len()
        );

        let mut handles = Vec::with_capacity(num_chunks);
        for (idx, chunk_symbols) in chunks.into_iter().enumerate() {
            let cache = cache.clone();
            let handle = tokio::spawn(run_chunk(cache, chunk_symbols, idx));
            handles.push(handle);
        }

        // Sleep 5 minutes, then abort all chunks and restart
        tokio::time::sleep(std::time::Duration::from_secs(300)).await;

        tracing::info!(
            "okx futures: coordinator restarting, aborting {} chunks",
            handles.len()
        );
        cache.okx_future.write().await.restarting = true;
        for h in &handles {
            h.abort();
        }
        for h in handles {
            let _ = h.await;
        }
    }
}

/// Chunk worker: manages a single WS connection subscribing to ≤50 symbols.
/// Self-reconnects on disconnect; coordinator handles instrument refresh.
async fn run_chunk(
    cache: SharedCache,
    symbols: Vec<String>,
    chunk_id: usize,
) {
    let mut attempt: u32 = 0;

    loop {
        tracing::info!(
            "okx futures chunk-{}: connecting ({} symbols)...",
            chunk_id,
            symbols.len()
        );
        let url = "wss://ws.okx.com:8443/ws/v5/public";
        let ws_result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            connect_async(url),
        )
        .await;
        match ws_result {
            Ok(Ok((ws, _))) => {
                attempt = 0;
                tracing::info!("okx futures chunk-{}: connected", chunk_id);
                let (mut write, mut read) = ws.split();

                // Build subscription args for bbo-tbt + tickers + funding-rate + books5 channels
                let mut all_args: Vec<serde_json::Value> = Vec::new();
                for inst_id in &symbols {
                    all_args.push(serde_json::json!({
                        "channel": "bbo-tbt",
                        "instId": inst_id
                    }));
                    all_args.push(serde_json::json!({
                        "channel": "tickers",
                        "instId": inst_id
                    }));
                    all_args.push(serde_json::json!({
                        "channel": "funding-rate",
                        "instId": inst_id
                    }));
                    all_args.push(serde_json::json!({
                        "channel": "books5",
                        "instId": inst_id
                    }));
                }

                // Subscribe in batches of 50 args per message
                let mut subscribe_failed = false;
                for chunk in all_args.chunks(50) {
                    let sub = serde_json::json!({
                        "op": "subscribe",
                        "args": chunk
                    });
                    if let Err(e) = write.send(Message::Text(sub.to_string().into())).await {
                        tracing::error!(
                            "okx futures chunk-{}: subscribe send error: {}",
                            chunk_id,
                            e
                        );
                        subscribe_failed = true;
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }

                if subscribe_failed {
                    tracing::warn!(
                        "okx futures chunk-{}: subscribe failed, reconnecting...",
                        chunk_id
                    );
                    backoff_sleep(attempt).await;
                    attempt = attempt.saturating_add(1);
                    continue;
                }

                let mut ping_interval =
                    tokio::time::interval(std::time::Duration::from_secs(25));
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
                                    tracing::warn!("okx futures chunk-{}: read error: {}", chunk_id, e);
                                    break;
                                }
                                None => {
                                    tracing::warn!("okx futures chunk-{}: stream ended", chunk_id);
                                    break;
                                }
                            };

                            let text = match msg {
                                Message::Text(t) => t,
                                Message::Ping(_) | Message::Pong(_) => continue,
                                Message::Close(_) => {
                                    tracing::warn!("okx futures chunk-{}: server closed connection", chunk_id);
                                    break;
                                }
                                _ => continue,
                            };

                            // OKX sends "pong" as plain text in response to "ping"
                            let text_str: &str = text.as_ref();
                            if text_str == "pong" {
                                continue;
                            }

                            let ws_msg: WsMsg = match serde_json::from_str(text_str) {
                                Ok(v) => v,
                                Err(e) => {
                                    tracing::warn!("okx futures chunk-{}: parse error: {}", chunk_id, e);
                                    continue;
                                }
                            };

                            // Handle event messages
                            if ws_msg.event.is_some() {
                                if ws_msg.code.as_deref() == Some("64008") {
                                    tracing::warn!(
                                        "okx futures chunk-{}: maintenance warning (64008), reconnecting in 5s...",
                                        chunk_id
                                    );
                                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                                    break;
                                }
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

                            let channel = arg.channel.as_str();
                            let ts = now_ms();

                            match channel {
                                "bbo-tbt" => {
                                    // BBO stream: update bid/ask prices
                                    let inst_id = data
                                        .get("instId")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or(&arg.inst_id);
                                    let normalized = normalize_swap(inst_id);
                                    let symbol_for_event = normalized.clone();

                                    let ask_px = data.get("asks")
                                        .and_then(|v| v.as_array())
                                        .and_then(|arr| arr.first())
                                        .and_then(|entry| entry.as_array())
                                        .and_then(|pair| pair.first())
                                        .and_then(|v| v.as_str())
                                        .map(|s| parse_f64(s))
                                        .unwrap_or(0.0);
                                    let bid_px = data.get("bids")
                                        .and_then(|v| v.as_array())
                                        .and_then(|arr| arr.first())
                                        .and_then(|entry| entry.as_array())
                                        .and_then(|pair| pair.first())
                                        .and_then(|v| v.as_str())
                                        .map(|s| parse_f64(s))
                                        .unwrap_or(0.0);

                                    if bid_px > 0.0 || ask_px > 0.0 {
                                        let mut section = cache.okx_future.write().await;
                                        section.ts = ts;
                                        let item = section
                                            .items
                                            .entry(normalized.clone())
                                            .or_insert_with(|| ExchangeItem {
                                                name: normalized,
                                                ..Default::default()
                                            });
                                        if bid_px > 0.0 { item.b = bid_px; }
                                        if ask_px > 0.0 { item.a = ask_px; }
                                        item.ts = ts;
                                        section.dirty = true;
                                        drop(section);
                                        let _ = cache.ticker_tx.send(crate::spread::TickerChanged {
                                            symbol: symbol_for_event,
                                        });
                                    }
                                }
                                "tickers" => {
                                    // bid/ask now driven by bbo-tbt stream (not tickers)
                                    let inst_id = data
                                        .get("instId")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or(&arg.inst_id);
                                    let normalized = normalize_swap(inst_id);

                                    let vol_ccy = data.get("volCcy24h").map(json_f64).unwrap_or(0.0);

                                    let mut section = cache.okx_future.write().await;
                                    let item = section
                                        .items
                                        .entry(normalized.clone())
                                        .or_insert_with(|| ExchangeItem {
                                            name: normalized,
                                            ..Default::default()
                                        });
                                    item.trade24_count = vol_ccy;
                                    section.dirty = true;
                                    // No TickerChanged here — BBO updates (bbo-tbt) drive spread recalc
                                }
                                "funding-rate" => {
                                    let inst_id = data
                                        .get("instId")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or(&arg.inst_id);
                                    let normalized = normalize_swap(inst_id);

                                    let rate_dec = data.get("fundingRate").map(json_f64).unwrap_or(0.0);
                                    let rate_str = format!("{:.3}", rate_dec * 100.0);

                                    // Extract maxFundingRate
                                    let max_rate = data
                                        .get("maxFundingRate")
                                        .map(|v| format!("{:.3}", json_f64(v) * 100.0))
                                        .unwrap_or_else(|| "--".to_string());

                                    // Calculate interval from fundingTime and nextFundingTime
                                    let interval = {
                                        let ft = data.get("fundingTime").map(|v| json_f64(v) as u64).unwrap_or(0);
                                        let nft = data.get("nextFundingTime").map(|v| json_f64(v) as u64).unwrap_or(0);
                                        if nft > ft && ft > 0 {
                                            ((nft - ft) / 3_600_000) as u32
                                        } else {
                                            0
                                        }
                                    };

                                    let mut section = cache.okx_future.write().await;
                                    section.ts = ts;
                                    let item = section
                                        .items
                                        .entry(normalized.clone())
                                        .or_insert_with(|| ExchangeItem {
                                            name: normalized,
                                            ..Default::default()
                                        });
                                    item.rate = Some(rate_str);
                                    item.rate_max = Some(max_rate);
                                    if interval > 0 {
                                        item.rate_interval = Some(interval);
                                    }
                                    section.dirty = true;
                                }
                                "books5" => {
                                    let inst_id = data
                                        .get("instId")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or(&arg.inst_id);
                                    let normalized = normalize_swap(inst_id);

                                    // OKX books5 format: [[price, qty, _, num_orders], ...]
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
                                        let mut updated = false;
                                        {
                                            let mut section = cache.okx_future.write().await;
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
                            tracing::warn!("okx futures chunk-{}: no message for 30s, reconnecting", chunk_id);
                            break;
                        }
                        _ = ping_interval.tick() => {
                            // OKX uses plain text "ping" for heartbeat
                            if let Err(e) = write.send(Message::Text("ping".into())).await {
                                tracing::error!("okx futures chunk-{}: ping send error: {}", chunk_id, e);
                                break;
                            }
                        }
                    }
                }
            }
            Ok(Err(e)) => {
                tracing::error!("okx futures chunk-{}: connection failed: {}", chunk_id, e);
            }
            Err(_) => {
                tracing::error!("okx futures chunk-{}: connection timed out", chunk_id);
            }
        }
        // Don't clear cache — coordinator handles that
        backoff_sleep(attempt).await;
        attempt = attempt.saturating_add(1);
    }
}

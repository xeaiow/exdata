use crate::cache::SharedCache;
use crate::exchanges::{backoff_sleep, now_ms, parse_f64};
use crate::models::ExchangeItem;
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
    #[serde(rename = "quoteCcy", default)]
    quote_ccy: Option<String>,
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

/// Normalize spot instId: "BTC-USDT" → "BTCUSDT"
fn normalize_spot(inst_id: &str) -> String {
    remove_hyphens(inst_id)
}

/// Normalize swap instId: "BTC-USDT-SWAP" → "BTCUSDT"
fn normalize_swap(inst_id: &str) -> String {
    let base = inst_id.strip_suffix("-SWAP").unwrap_or(inst_id);
    remove_hyphens(base)
}

// ── REST helpers ────────────────────────────────────────────────────────────

/// Fetch spot instrument instIds (USDT quote, live state).
async fn fetch_spot_instruments(client: &reqwest::Client) -> Vec<String> {
    let url = "https://www.okx.com/api/v5/public/instruments?instType=SPOT";
    let resp = match client.get(url).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("okx spot: instruments request failed: {}", e);
            return Vec::new();
        }
    };

    let body: OkxResponse = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("okx spot: instruments parse failed: {}", e);
            return Vec::new();
        }
    };

    body.data
        .into_iter()
        .filter(|i| {
            i.state == "live" && i.quote_ccy.as_deref() == Some("USDT")
        })
        .map(|i| i.inst_id)
        .collect()
}

/// Fetch spot ticker snapshot so every symbol has bid/ask from the start.
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

async fn fetch_spot_tickers(client: &reqwest::Client) -> Vec<OkxTickerItem> {
    let url = "https://www.okx.com/api/v5/market/tickers?instType=SPOT";
    let resp = match client.get(url).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("okx spot: tickers fetch failed: {}", e);
            return Vec::new();
        }
    };
    let body: OkxTickersResponse = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("okx spot: tickers parse failed: {}", e);
            return Vec::new();
        }
    };
    body.data
}

/// Fetch swap instrument instIds (USDT settle, linear, live state).
async fn fetch_swap_instruments(client: &reqwest::Client) -> Vec<String> {
    let url = "https://www.okx.com/api/v5/public/instruments?instType=SWAP";
    let resp = match client.get(url).send().await {
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
        })
        .map(|i| i.inst_id)
        .collect()
}

// ── Spot ────────────────────────────────────────────────────────────────────

pub async fn run_spot(cache: SharedCache, client: reqwest::Client) {
    let mut attempt: u32 = 0;

    loop {
        tracing::info!("okx spot: fetching instrument list...");
        let instruments = fetch_spot_instruments(&client).await;
        tracing::info!("okx spot: found {} USDT instruments", instruments.len());

        if instruments.is_empty() {
            tracing::warn!("okx spot: no instruments found, retrying...");
            backoff_sleep(attempt).await;
            attempt = attempt.saturating_add(1);
            continue;
        }

        tracing::info!("okx spot: connecting...");
        let url = "wss://ws.okx.com:8443/ws/v5/public";
        match connect_async(url).await {
            Ok((ws, _)) => {
                attempt = 0;
                tracing::info!("okx spot: connected");
                // Seed bid/ask from REST before entering the WS loop
                let spot_tickers = fetch_spot_tickers(&client).await;
                if !spot_tickers.is_empty() {
                    let instruments_set: HashSet<&String> = instruments.iter().collect();
                    let mut section = cache.okx_spot.write().await;
                    for t in &spot_tickers {
                        if !instruments_set.contains(&t.inst_id) {
                            continue;
                        }
                        let normalized = normalize_spot(&t.inst_id);
                        let item = section
                            .items
                            .entry(normalized.clone())
                            .or_insert_with(|| ExchangeItem {
                                name: normalized,
                                rate_interval: Some(0),
                                rate: Some("--".to_string()),
                                rate_max: Some("--".to_string()),
                                ..Default::default()
                            });
                        item.a = parse_f64(&t.ask_px);
                        item.b = parse_f64(&t.bid_px);
                        item.trade24_count = parse_f64(&t.vol_ccy_24h);
                    }
                    tracing::info!(
                        "okx spot: seeded {} tickers from REST",
                        section.items.len()
                    );
                    section.serialize_cache();
                }

                let (mut write, mut read) = ws.split();

                // Subscribe in batches of 50 args per message
                for chunk in instruments.chunks(50) {
                    let args: Vec<serde_json::Value> = chunk
                        .iter()
                        .map(|id| {
                            serde_json::json!({
                                "channel": "tickers",
                                "instId": id
                            })
                        })
                        .collect();
                    let sub = serde_json::json!({
                        "op": "subscribe",
                        "args": args
                    });
                    if let Err(e) = write.send(Message::Text(sub.to_string().into())).await {
                        tracing::error!("okx spot: subscribe send error: {}", e);
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }

                let mut ping_interval =
                    tokio::time::interval(std::time::Duration::from_secs(25));
                ping_interval.tick().await; // consume the immediate first tick

                loop {
                    tokio::select! {
                        msg = read.next() => {
                            let msg = match msg {
                                Some(Ok(m)) => m,
                                Some(Err(e)) => {
                                    tracing::warn!("okx spot: read error: {}", e);
                                    break;
                                }
                                None => {
                                    tracing::warn!("okx spot: stream ended");
                                    break;
                                }
                            };

                            let text = match msg {
                                Message::Text(t) => t,
                                Message::Ping(_) | Message::Pong(_) => continue,
                                Message::Close(_) => {
                                    tracing::warn!("okx spot: server closed connection");
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
                                    tracing::warn!("okx spot: parse error: {}", e);
                                    continue;
                                }
                            };

                            // Skip subscribe confirmations
                            if ws_msg.event.is_some() {
                                continue;
                            }

                            let arg = match ws_msg.arg {
                                Some(ref a) => a,
                                None => continue,
                            };

                            if arg.channel != "tickers" {
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
                            let normalized = normalize_spot(inst_id);

                            let ask_px = data
                                .get("askPx")
                                .and_then(|v| v.as_str())
                                .unwrap_or("0");
                            let bid_px = data
                                .get("bidPx")
                                .and_then(|v| v.as_str())
                                .unwrap_or("0");
                            let vol_ccy = data
                                .get("volCcy24h")
                                .and_then(|v| v.as_str())
                                .unwrap_or("0");

                            let ts = now_ms();
                            let mut section = cache.okx_spot.write().await;
                            section.ts = ts;

                            let item = section
                                .items
                                .entry(normalized.clone())
                                .or_insert_with(|| ExchangeItem {
                                    name: normalized,
                                    rate_interval: Some(0),
                                    rate: Some("--".to_string()),
                                    rate_max: Some("--".to_string()),
                                    ..Default::default()
                                });
                            item.a = parse_f64(ask_px);
                            item.b = parse_f64(bid_px);
                            item.trade24_count = parse_f64(vol_ccy);
                            section.serialize_cache();
                        }
                        _ = ping_interval.tick() => {
                            // OKX uses plain text "ping" for heartbeat
                            if let Err(e) = write.send(Message::Text("ping".into())).await {
                                tracing::error!("okx spot: ping send error: {}", e);
                                break;
                            }
                        }
                    }
                }
            }
            Err(e) => {
                tracing::error!("okx spot: connection failed: {}", e);
            }
        }
        backoff_sleep(attempt).await;
        attempt = attempt.saturating_add(1);
    }
}

// ── Futures (Swap) ──────────────────────────────────────────────────────────

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

        let mut current_instruments: HashSet<String> =
            instruments.iter().cloned().collect();

        tracing::info!("okx futures: connecting...");
        let url = "wss://ws.okx.com:8443/ws/v5/public";
        match connect_async(url).await {
            Ok((ws, _)) => {
                attempt = 0;
                tracing::info!("okx futures: connected");
                let (mut write, mut read) = ws.split();

                // Build subscription args for tickers + funding-rate channels.
                let mut all_args: Vec<serde_json::Value> = Vec::new();
                for inst_id in &instruments {
                    all_args.push(serde_json::json!({
                        "channel": "tickers",
                        "instId": inst_id
                    }));
                    all_args.push(serde_json::json!({
                        "channel": "funding-rate",
                        "instId": inst_id
                    }));
                }

                // Subscribe in batches of 50 args per message
                for chunk in all_args.chunks(50) {
                    let sub = serde_json::json!({
                        "op": "subscribe",
                        "args": chunk
                    });
                    if let Err(e) = write.send(Message::Text(sub.to_string().into())).await {
                        tracing::error!("okx futures: subscribe send error: {}", e);
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }

                let mut ping_interval =
                    tokio::time::interval(std::time::Duration::from_secs(25));
                ping_interval.tick().await; // consume the immediate first tick

                let mut refresh_interval =
                    tokio::time::interval(std::time::Duration::from_secs(300));
                refresh_interval.tick().await; // consume the immediate first tick

                loop {
                    tokio::select! {
                        msg = read.next() => {
                            let msg = match msg {
                                Some(Ok(m)) => m,
                                Some(Err(e)) => {
                                    tracing::warn!("okx futures: read error: {}", e);
                                    break;
                                }
                                None => {
                                    tracing::warn!("okx futures: stream ended");
                                    break;
                                }
                            };

                            let text = match msg {
                                Message::Text(t) => t,
                                Message::Ping(_) | Message::Pong(_) => continue,
                                Message::Close(_) => {
                                    tracing::warn!("okx futures: server closed connection");
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
                                    tracing::warn!("okx futures: parse error: {}", e);
                                    continue;
                                }
                            };

                            // Skip subscribe confirmations
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

                            let channel = arg.channel.as_str();
                            let ts = now_ms();

                            match channel {
                                "tickers" => {
                                    let inst_id = data
                                        .get("instId")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or(&arg.inst_id);
                                    let normalized = normalize_swap(inst_id);

                                    let ask_px = data
                                        .get("askPx")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("0");
                                    let bid_px = data
                                        .get("bidPx")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("0");
                                    let vol_ccy = data
                                        .get("volCcy24h")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("0");

                                    let mut section = cache.okx_future.write().await;
                                    section.ts = ts;
                                    let item = section
                                        .items
                                        .entry(normalized.clone())
                                        .or_insert_with(|| ExchangeItem {
                                            name: normalized,
                                            rate_interval: Some(0),
                                            rate_max: Some("--".to_string()),
                                            ..Default::default()
                                        });
                                    item.a = parse_f64(ask_px);
                                    item.b = parse_f64(bid_px);
                                    item.trade24_count = parse_f64(vol_ccy);
                                    section.serialize_cache();
                                }
                                "funding-rate" => {
                                    let inst_id = data
                                        .get("instId")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or(&arg.inst_id);
                                    let normalized = normalize_swap(inst_id);

                                    let funding_rate = data
                                        .get("fundingRate")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("0");
                                    let rate_dec = parse_f64(funding_rate);
                                    let rate_str = format!("{:.3}", rate_dec * 100.0);

                                    // Extract maxFundingRate
                                    let max_rate = data
                                        .get("maxFundingRate")
                                        .and_then(|v| v.as_str())
                                        .map(|s| format!("{:.3}", parse_f64(s) * 100.0))
                                        .unwrap_or_else(|| "--".to_string());

                                    // Calculate interval from fundingTime and nextFundingTime
                                    let interval = {
                                        let ft = data.get("fundingTime").and_then(|v| v.as_str()).map(|s| parse_f64(s) as u64).unwrap_or(0);
                                        let nft = data.get("nextFundingTime").and_then(|v| v.as_str()).map(|s| parse_f64(s) as u64).unwrap_or(0);
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
                                    section.serialize_cache();
                                }
                                _ => {
                                    tracing::warn!(
                                        "okx futures: unknown channel: {}",
                                        channel
                                    );
                                }
                            }
                        }
                        _ = ping_interval.tick() => {
                            // OKX uses plain text "ping" for heartbeat
                            if let Err(e) = write.send(Message::Text("ping".into())).await {
                                tracing::error!("okx futures: ping send error: {}", e);
                                break;
                            }
                        }
                        _ = refresh_interval.tick() => {
                            tracing::info!("okx futures: refreshing instrument list...");
                            let new_instruments_vec = fetch_swap_instruments(&client).await;
                            if new_instruments_vec.is_empty() {
                                tracing::warn!("okx futures: refresh returned empty list, skipping");
                                continue;
                            }

                            let new_instruments: HashSet<String> =
                                new_instruments_vec.iter().cloned().collect();
                            tracing::info!(
                                "okx futures: refresh found {} instruments",
                                new_instruments.len()
                            );

                            // Unsubscribe removed instruments (both tickers + funding-rate)
                            let removed: Vec<String> = current_instruments
                                .difference(&new_instruments)
                                .cloned()
                                .collect();
                            if !removed.is_empty() {
                                tracing::info!(
                                    "okx futures: unsubscribing {} removed instruments",
                                    removed.len()
                                );
                                let mut unsub_args: Vec<serde_json::Value> = Vec::new();
                                for inst_id in &removed {
                                    unsub_args.push(serde_json::json!({
                                        "channel": "tickers",
                                        "instId": inst_id
                                    }));
                                    unsub_args.push(serde_json::json!({
                                        "channel": "funding-rate",
                                        "instId": inst_id
                                    }));
                                }
                                for chunk in unsub_args.chunks(50) {
                                    let unsub = serde_json::json!({
                                        "op": "unsubscribe",
                                        "args": chunk
                                    });
                                    if let Err(e) = write.send(Message::Text(unsub.to_string().into())).await {
                                        tracing::error!("okx futures: unsub send error: {}", e);
                                        break;
                                    }
                                }
                            }

                            // Subscribe new instruments (both tickers + funding-rate)
                            let added: Vec<String> = new_instruments
                                .difference(&current_instruments)
                                .cloned()
                                .collect();
                            if !added.is_empty() {
                                tracing::info!(
                                    "okx futures: subscribing {} new instruments",
                                    added.len()
                                );
                                let mut sub_args: Vec<serde_json::Value> = Vec::new();
                                for inst_id in &added {
                                    sub_args.push(serde_json::json!({
                                        "channel": "tickers",
                                        "instId": inst_id
                                    }));
                                    sub_args.push(serde_json::json!({
                                        "channel": "funding-rate",
                                        "instId": inst_id
                                    }));
                                }
                                for chunk in sub_args.chunks(50) {
                                    let sub = serde_json::json!({
                                        "op": "subscribe",
                                        "args": chunk
                                    });
                                    if let Err(e) = write.send(Message::Text(sub.to_string().into())).await {
                                        tracing::error!("okx futures: sub send error: {}", e);
                                        break;
                                    }
                                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                                }
                            }

                            // Clean stale cache entries
                            // Cache keys are normalized (e.g. "BTCUSDT") but instrument IDs
                            // are OKX format (e.g. "BTC-USDT-SWAP"), so build valid normalized set
                            {
                                let valid_normalized: HashSet<String> = new_instruments
                                    .iter()
                                    .map(|id| normalize_swap(id))
                                    .collect();
                                let mut section = cache.okx_future.write().await;
                                let before = section.items.len();
                                section.items.retain(|k, _| valid_normalized.contains(k));
                                let removed_count = before - section.items.len();
                                if removed_count > 0 {
                                    tracing::info!(
                                        "okx futures: removed {} stale cache entries",
                                        removed_count
                                    );
                                }
                                section.serialize_cache();
                            }

                            current_instruments = new_instruments;
                        }
                    }
                }
            }
            Err(e) => {
                tracing::error!("okx futures: connection failed: {}", e);
            }
        }
        backoff_sleep(attempt).await;
        attempt = attempt.saturating_add(1);
    }
}

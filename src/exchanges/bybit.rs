use crate::cache::SharedCache;
use crate::exchanges::{backoff_sleep, now_ms, parse_f64};
use crate::models::ExchangeItem;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::collections::HashMap;
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

/// Fetch spot instrument symbols (USDT quote, Trading status).
async fn fetch_spot_symbols(client: &reqwest::Client) -> Vec<String> {
    let mut symbols = Vec::new();
    let mut cursor: Option<String> = None;

    loop {
        let mut url =
            "https://api.bybit.com/v5/market/instruments-info?category=spot&limit=1000".to_string();
        if let Some(ref c) = cursor {
            if !c.is_empty() {
                url.push_str(&format!("&cursor={}", c));
            }
        }

        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("bybit spot: instruments request failed: {}", e);
                break;
            }
        };

        let body: InstrumentsResponse = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("bybit spot: instruments parse failed: {}", e);
                break;
            }
        };

        for info in &body.result.list {
            if info.quote_coin == "USDT" && info.status == "Trading" {
                symbols.push(info.symbol.clone());
            }
        }

        match body.result.next_page_cursor {
            Some(ref c) if !c.is_empty() => cursor = Some(c.clone()),
            _ => break,
        }
    }

    symbols
}

/// Funding info for futures: symbol -> (interval_hours, rate_max).
struct FuturesInstrumentData {
    symbols: Vec<String>,
    funding_info: HashMap<String, (u32, String)>,
}

/// Fetch linear perpetual instrument info (USDT quote, LinearPerpetual, Trading).
async fn fetch_futures_instruments(client: &reqwest::Client) -> FuturesInstrumentData {
    let mut symbols = Vec::new();
    let mut funding_info: HashMap<String, (u32, String)> = HashMap::new();
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

        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("bybit futures: instruments request failed: {}", e);
                break;
            }
        };

        let body: InstrumentsResponse = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("bybit futures: instruments parse failed: {}", e);
                break;
            }
        };

        for info in &body.result.list {
            let is_perp = info
                .contract_type
                .as_deref()
                .map_or(false, |ct| ct == "LinearPerpetual");
            if info.quote_coin == "USDT" && info.status == "Trading" && is_perp {
                symbols.push(info.symbol.clone());
                let interval_hours = info.funding_interval.unwrap_or(480) / 60;
                let rate_max = info
                    .upper_funding_rate
                    .as_deref()
                    .map(|s| {
                        let v = parse_f64(s);
                        format!("{:.3}", v * 100.0)
                    })
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

// ── Spot ─────────────────────────────────────────────────────────────────────

pub async fn run_spot(cache: SharedCache, client: reqwest::Client) {
    let mut attempt: u32 = 0;

    loop {
        tracing::info!("bybit spot: fetching instrument list...");
        let symbols = fetch_spot_symbols(&client).await;
        tracing::info!("bybit spot: found {} USDT symbols", symbols.len());

        if symbols.is_empty() {
            tracing::warn!("bybit spot: no symbols found, retrying...");
            backoff_sleep(attempt).await;
            attempt = attempt.saturating_add(1);
            continue;
        }

        tracing::info!("bybit spot: connecting...");
        let url = "wss://stream.bybit.com/v5/public/spot";
        match connect_async(url).await {
            Ok((ws, _)) => {
                attempt = 0;
                tracing::info!("bybit spot: connected");
                let (mut write, mut read) = ws.split();

                // Subscribe in batches of 10
                for chunk in symbols.chunks(10) {
                    let args: Vec<String> =
                        chunk.iter().map(|s| format!("tickers.{}", s)).collect();
                    let sub = serde_json::json!({
                        "op": "subscribe",
                        "args": args
                    });
                    if let Err(e) = write.send(Message::Text(sub.to_string().into())).await {
                        tracing::error!("bybit spot: subscribe send error: {}", e);
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }

                let mut ping_interval = tokio::time::interval(std::time::Duration::from_secs(20));
                ping_interval.tick().await; // consume the immediate first tick

                loop {
                    tokio::select! {
                        msg = read.next() => {
                            let msg = match msg {
                                Some(Ok(m)) => m,
                                Some(Err(e)) => {
                                    tracing::warn!("bybit spot: read error: {}", e);
                                    break;
                                }
                                None => {
                                    tracing::warn!("bybit spot: stream ended");
                                    break;
                                }
                            };

                            let text = match msg {
                                Message::Text(t) => t,
                                Message::Ping(_) | Message::Pong(_) => continue,
                                Message::Close(_) => {
                                    tracing::warn!("bybit spot: server closed connection");
                                    break;
                                }
                                _ => continue,
                            };

                            let ws_msg: WsMsg = match serde_json::from_str(&text) {
                                Ok(v) => v,
                                Err(e) => {
                                    tracing::warn!("bybit spot: parse error: {}", e);
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

                            if !topic.starts_with("tickers.") {
                                continue;
                            }

                            let data_val = match ws_msg.data {
                                Some(v) => v,
                                None => continue,
                            };

                            let ticker: TickerData = match serde_json::from_value(data_val) {
                                Ok(v) => v,
                                Err(e) => {
                                    tracing::warn!("bybit spot: parse ticker error: {}", e);
                                    continue;
                                }
                            };

                            let ts = ws_msg.ts.unwrap_or_else(now_ms);

                            let mut section = cache.bybit_spot.write().await;
                            section.ts = ts;

                            let item = section
                                .items
                                .entry(ticker.symbol.clone())
                                .or_insert_with(|| ExchangeItem {
                                    name: ticker.symbol.clone(),
                                    index_price: Some(String::new()),
                                    mark_price: Some(String::new()),
                                    ..Default::default()
                                });

                            // Delta messages only have partial fields; only update what's present
                            if let Some(ref v) = ticker.ask1_price {
                                item.a = parse_f64(v);
                            }
                            if let Some(ref v) = ticker.bid1_price {
                                item.b = parse_f64(v);
                            }
                            if let Some(ref v) = ticker.turnover_24h {
                                item.trade24_count = parse_f64(v);
                            }
                        }
                        _ = ping_interval.tick() => {
                            let ping = serde_json::json!({"op": "ping"});
                            if let Err(e) = write.send(Message::Text(ping.to_string().into())).await {
                                tracing::error!("bybit spot: ping send error: {}", e);
                                break;
                            }
                        }
                    }
                }
            }
            Err(e) => {
                tracing::error!("bybit spot: connection failed: {}", e);
            }
        }
        backoff_sleep(attempt).await;
        attempt = attempt.saturating_add(1);
    }
}

// ── Futures ──────────────────────────────────────────────────────────────────

pub async fn run_future(cache: SharedCache, client: reqwest::Client) {
    let mut attempt: u32 = 0;

    loop {
        tracing::info!("bybit futures: fetching instrument list...");
        let instr = fetch_futures_instruments(&client).await;
        tracing::info!(
            "bybit futures: found {} USDT perpetual symbols",
            instr.symbols.len()
        );

        if instr.symbols.is_empty() {
            tracing::warn!("bybit futures: no symbols found, retrying...");
            backoff_sleep(attempt).await;
            attempt = attempt.saturating_add(1);
            continue;
        }

        let funding_info = instr.funding_info;

        tracing::info!("bybit futures: connecting...");
        let url = "wss://stream.bybit.com/v5/public/linear";
        match connect_async(url).await {
            Ok((ws, _)) => {
                attempt = 0;
                tracing::info!("bybit futures: connected");
                let (mut write, mut read) = ws.split();

                // Subscribe in batches of 10
                for chunk in instr.symbols.chunks(10) {
                    let args: Vec<String> =
                        chunk.iter().map(|s| format!("tickers.{}", s)).collect();
                    let sub = serde_json::json!({
                        "op": "subscribe",
                        "args": args
                    });
                    if let Err(e) = write.send(Message::Text(sub.to_string().into())).await {
                        tracing::error!("bybit futures: subscribe send error: {}", e);
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }

                let mut ping_interval = tokio::time::interval(std::time::Duration::from_secs(20));
                ping_interval.tick().await; // consume the immediate first tick

                loop {
                    tokio::select! {
                        msg = read.next() => {
                            let msg = match msg {
                                Some(Ok(m)) => m,
                                Some(Err(e)) => {
                                    tracing::warn!("bybit futures: read error: {}", e);
                                    break;
                                }
                                None => {
                                    tracing::warn!("bybit futures: stream ended");
                                    break;
                                }
                            };

                            let text = match msg {
                                Message::Text(t) => t,
                                Message::Ping(_) | Message::Pong(_) => continue,
                                Message::Close(_) => {
                                    tracing::warn!("bybit futures: server closed connection");
                                    break;
                                }
                                _ => continue,
                            };

                            let ws_msg: WsMsg = match serde_json::from_str(&text) {
                                Ok(v) => v,
                                Err(e) => {
                                    tracing::warn!("bybit futures: parse error: {}", e);
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

                            if !topic.starts_with("tickers.") {
                                continue;
                            }

                            let data_val = match ws_msg.data {
                                Some(v) => v,
                                None => continue,
                            };

                            let ticker: TickerData = match serde_json::from_value(data_val) {
                                Ok(v) => v,
                                Err(e) => {
                                    tracing::warn!("bybit futures: parse ticker error: {}", e);
                                    continue;
                                }
                            };

                            let ts = ws_msg.ts.unwrap_or_else(now_ms);

                            let mut section = cache.bybit_future.write().await;
                            section.ts = ts;

                            let item = section
                                .items
                                .entry(ticker.symbol.clone())
                                .or_insert_with(|| {
                                    let mut ei = ExchangeItem {
                                        name: ticker.symbol.clone(),
                                        ..Default::default()
                                    };
                                    // Apply funding info from REST
                                    if let Some((interval, max_rate)) = funding_info.get(&ticker.symbol) {
                                        ei.rate_interval = Some(*interval);
                                        ei.rate_max = Some(max_rate.clone());
                                    }
                                    ei
                                });

                            // Delta messages only have partial fields; only update what's present
                            if let Some(ref v) = ticker.ask1_price {
                                item.a = parse_f64(v);
                            }
                            if let Some(ref v) = ticker.bid1_price {
                                item.b = parse_f64(v);
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
                        }
                        _ = ping_interval.tick() => {
                            let ping = serde_json::json!({"op": "ping"});
                            if let Err(e) = write.send(Message::Text(ping.to_string().into())).await {
                                tracing::error!("bybit futures: ping send error: {}", e);
                                break;
                            }
                        }
                    }
                }
            }
            Err(e) => {
                tracing::error!("bybit futures: connection failed: {}", e);
            }
        }
        backoff_sleep(attempt).await;
        attempt = attempt.saturating_add(1);
    }
}

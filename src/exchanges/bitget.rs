use crate::cache::SharedCache;
use crate::exchanges::{backoff_sleep, now_ms, parse_f64};
use crate::models::ExchangeItem;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::collections::HashMap;
use tokio_tungstenite::{connect_async, tungstenite::Message};

// ── REST serde structs ──────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SpotSymbolsResponse {
    data: Vec<SpotSymbolInfo>,
}

#[derive(Deserialize)]
struct SpotSymbolInfo {
    symbol: String,
    status: String,
    #[serde(rename = "quoteCoin")]
    quote_coin: String,
}

#[derive(Deserialize)]
struct FuturesContractsResponse {
    data: Vec<FuturesContractInfo>,
}

#[derive(Deserialize)]
struct FuturesContractInfo {
    symbol: String,
    status: String,
    #[serde(rename = "quoteCoin")]
    quote_coin: String,
    #[serde(rename = "fundingIntervalHours", default)]
    funding_interval_hours: Option<String>,
    #[serde(rename = "maxFundingRate", default)]
    max_funding_rate: Option<String>,
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
    inst_type: String,
    channel: String,
    #[serde(rename = "instId")]
    inst_id: String,
}

// ── REST helpers ────────────────────────────────────────────────────────────

/// Fetch spot symbols (USDT quote, online status).
async fn fetch_spot_symbols(client: &reqwest::Client) -> Vec<String> {
    let url = "https://api.bitget.com/api/v2/spot/public/symbols";
    let resp = match client.get(url).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("bitget spot: symbols request failed: {}", e);
            return Vec::new();
        }
    };

    let body: SpotSymbolsResponse = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("bitget spot: symbols parse failed: {}", e);
            return Vec::new();
        }
    };

    body.data
        .into_iter()
        .filter(|s| s.quote_coin == "USDT" && s.status == "online")
        .map(|s| s.symbol)
        .collect()
}

/// Funding info for futures: symbol -> (interval_hours, rate_max_str).
struct FuturesInstrumentData {
    symbols: Vec<String>,
    funding_info: HashMap<String, (u32, String)>,
}

/// Fetch futures contracts (USDT-FUTURES, normal status).
async fn fetch_futures_contracts(client: &reqwest::Client) -> FuturesInstrumentData {
    let url = "https://api.bitget.com/api/v2/mix/market/contracts?productType=USDT-FUTURES";
    let resp = match client.get(url).send().await {
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
            .funding_interval_hours
            .as_deref()
            .unwrap_or("8")
            .parse()
            .unwrap_or(8);

        let rate_max_str = match contract.max_funding_rate {
            Some(ref s) => {
                let val = parse_f64(s);
                format!("{:.3}", val * 100.0)
            }
            None => "--".to_string(),
        };

        symbols.push(contract.symbol.clone());
        funding_info.insert(contract.symbol, (interval_hours, rate_max_str));
    }

    FuturesInstrumentData {
        symbols,
        funding_info,
    }
}

// ── Spot ────────────────────────────────────────────────────────────────────

pub async fn run_spot(cache: SharedCache, client: reqwest::Client) {
    let mut attempt: u32 = 0;

    loop {
        tracing::info!("bitget spot: fetching symbol list...");
        let symbols = fetch_spot_symbols(&client).await;
        tracing::info!("bitget spot: found {} USDT symbols", symbols.len());

        if symbols.is_empty() {
            tracing::warn!("bitget spot: no symbols found, retrying...");
            backoff_sleep(attempt).await;
            attempt = attempt.saturating_add(1);
            continue;
        }

        tracing::info!("bitget spot: connecting...");
        let url = "wss://ws.bitget.com/v2/ws/public";
        match connect_async(url).await {
            Ok((ws, _)) => {
                attempt = 0;
                tracing::info!("bitget spot: connected");
                let (mut write, mut read) = ws.split();

                // Subscribe in batches of 50 args per message
                let args_all: Vec<serde_json::Value> = symbols
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "instType": "SPOT",
                            "channel": "ticker",
                            "instId": s
                        })
                    })
                    .collect();

                for chunk in args_all.chunks(50) {
                    let sub = serde_json::json!({
                        "op": "subscribe",
                        "args": chunk
                    });
                    if let Err(e) = write.send(Message::Text(sub.to_string().into())).await {
                        tracing::error!("bitget spot: subscribe send error: {}", e);
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }

                let mut ping_interval =
                    tokio::time::interval(std::time::Duration::from_secs(30));
                ping_interval.tick().await; // consume the immediate first tick

                let mut connected = true;
                while connected {
                    tokio::select! {
                        msg = read.next() => {
                            let msg = match msg {
                                Some(Ok(m)) => m,
                                Some(Err(e)) => {
                                    tracing::error!("bitget spot: read error: {}", e);
                                    connected = false;
                                    break;
                                }
                                None => {
                                    tracing::warn!("bitget spot: stream ended");
                                    connected = false;
                                    break;
                                }
                            };

                            let text = match msg {
                                Message::Text(t) => t,
                                Message::Ping(_) | Message::Pong(_) => continue,
                                Message::Close(_) => {
                                    tracing::warn!("bitget spot: server closed connection");
                                    connected = false;
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
                                    tracing::warn!("bitget spot: parse error: {}", e);
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

                            let ts = now_ms();
                            let mut section = cache.bitget_spot.write().await;
                            section.ts = ts;

                            let item = section
                                .items
                                .entry(inst_id.to_string())
                                .or_insert_with(|| ExchangeItem {
                                    name: inst_id.to_string(),
                                    ..Default::default()
                                });
                            item.a = parse_f64(ask_pr);
                            item.b = parse_f64(bid_pr);
                            item.trade24_count = parse_f64(quote_volume);
                        }
                        _ = ping_interval.tick() => {
                            // Bitget uses plain text "ping" for heartbeat
                            if let Err(e) = write.send(Message::Text("ping".into())).await {
                                tracing::error!("bitget spot: ping send error: {}", e);
                                connected = false;
                            }
                        }
                    }
                }
            }
            Err(e) => {
                tracing::error!("bitget spot: connection failed: {}", e);
            }
        }
        backoff_sleep(attempt).await;
        attempt = attempt.saturating_add(1);
    }
}

// ── Futures ─────────────────────────────────────────────────────────────────

pub async fn run_future(cache: SharedCache, client: reqwest::Client) {
    let mut attempt: u32 = 0;

    loop {
        tracing::info!("bitget futures: fetching contracts list...");
        let instr = fetch_futures_contracts(&client).await;
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

        let funding_info = instr.funding_info;

        tracing::info!("bitget futures: connecting...");
        let url = "wss://ws.bitget.com/v2/ws/public";
        match connect_async(url).await {
            Ok((ws, _)) => {
                attempt = 0;
                tracing::info!("bitget futures: connected");
                let (mut write, mut read) = ws.split();

                // Subscribe in batches of 50 args per message
                let args_all: Vec<serde_json::Value> = instr
                    .symbols
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "instType": "USDT-FUTURES",
                            "channel": "ticker",
                            "instId": s
                        })
                    })
                    .collect();

                for chunk in args_all.chunks(50) {
                    let sub = serde_json::json!({
                        "op": "subscribe",
                        "args": chunk
                    });
                    if let Err(e) = write.send(Message::Text(sub.to_string().into())).await {
                        tracing::error!("bitget futures: subscribe send error: {}", e);
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }

                let mut ping_interval =
                    tokio::time::interval(std::time::Duration::from_secs(30));
                ping_interval.tick().await; // consume the immediate first tick

                let mut connected = true;
                while connected {
                    tokio::select! {
                        msg = read.next() => {
                            let msg = match msg {
                                Some(Ok(m)) => m,
                                Some(Err(e)) => {
                                    tracing::error!("bitget futures: read error: {}", e);
                                    connected = false;
                                    break;
                                }
                                None => {
                                    tracing::warn!("bitget futures: stream ended");
                                    connected = false;
                                    break;
                                }
                            };

                            let text = match msg {
                                Message::Text(t) => t,
                                Message::Ping(_) | Message::Pong(_) => continue,
                                Message::Close(_) => {
                                    tracing::warn!("bitget futures: server closed connection");
                                    connected = false;
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
                                    tracing::warn!("bitget futures: parse error: {}", e);
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
                                    // Apply funding info from REST
                                    if let Some((interval, max_rate)) = funding_info.get(inst_id) {
                                        ei.rate_interval = Some(*interval);
                                        ei.rate_max = Some(max_rate.clone());
                                    }
                                    ei
                                });

                            item.a = parse_f64(ask_pr);
                            item.b = parse_f64(bid_pr);
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
                        }
                        _ = ping_interval.tick() => {
                            // Bitget uses plain text "ping" for heartbeat
                            if let Err(e) = write.send(Message::Text("ping".into())).await {
                                tracing::error!("bitget futures: ping send error: {}", e);
                                connected = false;
                            }
                        }
                    }
                }
            }
            Err(e) => {
                tracing::error!("bitget futures: connection failed: {}", e);
            }
        }
        backoff_sleep(attempt).await;
        attempt = attempt.saturating_add(1);
    }
}

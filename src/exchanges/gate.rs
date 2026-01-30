use crate::cache::SharedCache;
use crate::exchanges::{backoff_sleep, now_ms, parse_f64};
use crate::models::ExchangeItem;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use tokio_tungstenite::{connect_async, tungstenite::Message};

// ── REST serde structs (Spot) ──────────────────────────────────────────────

#[derive(Deserialize)]
struct SpotCurrencyPair {
    id: String,
    quote: String,
    trade_status: String,
}

// ── REST serde structs (Futures) ───────────────────────────────────────────

#[derive(Deserialize)]
struct FuturesContract {
    name: String,
    in_delisting: bool,
    funding_interval: u64,
    #[serde(default)]
    funding_cap_ratio: Option<String>,
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

/// Fetch spot symbols: USDT quote, tradable.
async fn fetch_spot_symbols(client: &reqwest::Client) -> Vec<String> {
    let url = "https://api.gateio.ws/api/v4/spot/currency_pairs";
    let resp = match client.get(url).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("gate spot: currency_pairs request failed: {}", e);
            return Vec::new();
        }
    };

    let pairs: Vec<SpotCurrencyPair> = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("gate spot: currency_pairs parse failed: {}", e);
            return Vec::new();
        }
    };

    pairs
        .into_iter()
        .filter(|p| p.quote == "USDT" && p.trade_status == "tradable")
        .map(|p| p.id)
        .collect()
}

/// Fetch spot ticker snapshot so every symbol has bid/ask from the start.
/// The WS `spot.tickers` channel is push-on-change only, so illiquid symbols would stay at a=0/b=0.
#[derive(Deserialize)]
struct GateSpotTicker {
    currency_pair: String,
    #[serde(default)]
    lowest_ask: String,
    #[serde(default)]
    highest_bid: String,
    #[serde(default)]
    quote_volume: String,
}

async fn fetch_spot_tickers(client: &reqwest::Client) -> Vec<GateSpotTicker> {
    let url = "https://api.gateio.ws/api/v4/spot/tickers";
    let resp = match client.get(url).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("gate spot: tickers fetch failed: {}", e);
            return Vec::new();
        }
    };
    match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("gate spot: tickers parse failed: {}", e);
            Vec::new()
        }
    }
}

/// Fetch futures contracts: NOT in_delisting.
async fn fetch_futures_contracts(client: &reqwest::Client) -> ContractInfo {
    let url = "https://api.gateio.ws/api/v4/futures/usdt/contracts";
    let resp = match client.get(url).send().await {
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
        if c.in_delisting {
            continue;
        }

        let interval_hours = (c.funding_interval / 3600) as u32;
        let rate_max = match c.funding_cap_ratio {
            Some(ref cap) => format!("{:.3}", parse_f64(cap) * 100.0),
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

// ── Spot ───────────────────────────────────────────────────────────────────

pub async fn run_spot(cache: SharedCache, client: reqwest::Client) {
    let mut attempt: u32 = 0;

    loop {
        tracing::info!("gate spot: fetching currency pairs...");
        let symbols = fetch_spot_symbols(&client).await;
        tracing::info!("gate spot: found {} USDT tradable pairs", symbols.len());

        if symbols.is_empty() {
            tracing::warn!("gate spot: no symbols found, retrying...");
            backoff_sleep(attempt).await;
            attempt = attempt.saturating_add(1);
            continue;
        }

        tracing::info!("gate spot: connecting...");
        let url = "wss://api.gateio.ws/ws/v4/";
        match connect_async(url).await {
            Ok((ws, _)) => {
                attempt = 0;
                tracing::info!("gate spot: connected");
                // Seed bid/ask from REST before entering the WS loop
                let spot_tickers = fetch_spot_tickers(&client).await;
                if !spot_tickers.is_empty() {
                    let symbols_set: HashSet<&String> = symbols.iter().collect();
                    let mut section = cache.gate_spot.write().await;
                    for t in &spot_tickers {
                        if !symbols_set.contains(&t.currency_pair) {
                            continue;
                        }
                        let normalized = normalize_symbol(&t.currency_pair);
                        let item = section
                            .items
                            .entry(normalized.clone())
                            .or_insert_with(|| ExchangeItem {
                                name: normalized,
                                ..Default::default()
                            });
                        item.a = parse_f64(&t.lowest_ask);
                        item.b = parse_f64(&t.highest_bid);
                        item.trade24_count = parse_f64(&t.quote_volume);
                    }
                    tracing::info!(
                        "gate spot: seeded {} tickers from REST",
                        section.items.len()
                    );
                }

                let (mut write, mut read) = ws.split();

                // Subscribe in batches of 50
                for chunk in symbols.chunks(50) {
                    let payload: Vec<&str> = chunk.iter().map(|s| s.as_str()).collect();
                    let now_secs = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs();
                    let sub = serde_json::json!({
                        "time": now_secs,
                        "channel": "spot.tickers",
                        "event": "subscribe",
                        "payload": payload
                    });
                    if let Err(e) = write.send(Message::Text(sub.to_string().into())).await {
                        tracing::error!("gate spot: subscribe send error: {}", e);
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }

                // Read loop - Gate.io handles heartbeat via server-side pings (tungstenite auto-replies)
                while let Some(msg) = read.next().await {
                    let text = match msg {
                        Ok(Message::Text(t)) => t,
                        Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => continue,
                        Ok(Message::Close(_)) => {
                            tracing::warn!("gate spot: server closed connection");
                            break;
                        }
                        Ok(_) => continue,
                        Err(e) => {
                            tracing::warn!("gate spot: read error: {}", e);
                            break;
                        }
                    };

                    let text_str: &str = text.as_ref();

                    let ws_msg: GateWsMsg = match serde_json::from_str(text_str) {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!("gate spot: parse error: {}", e);
                            continue;
                        }
                    };

                    // Skip non-update messages (subscribe confirmations, etc.)
                    match ws_msg.event.as_deref() {
                        Some("update") => {}
                        _ => continue,
                    }

                    match ws_msg.channel.as_deref() {
                        Some("spot.tickers") => {}
                        _ => continue,
                    }

                    let result = match ws_msg.result {
                        Some(v) => v,
                        None => continue,
                    };

                    // spot.tickers result is a single object, NOT an array
                    let currency_pair = result
                        .get("currency_pair")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if currency_pair.is_empty() {
                        continue;
                    }

                    let lowest_ask = result
                        .get("lowest_ask")
                        .and_then(|v| v.as_str())
                        .unwrap_or("0");
                    let highest_bid = result
                        .get("highest_bid")
                        .and_then(|v| v.as_str())
                        .unwrap_or("0");
                    let quote_volume = result
                        .get("quote_volume")
                        .and_then(|v| v.as_str())
                        .unwrap_or("0");

                    let normalized = normalize_symbol(currency_pair);
                    let ts = now_ms();

                    let mut section = cache.gate_spot.write().await;
                    section.ts = ts;

                    let item = section
                        .items
                        .entry(normalized.clone())
                        .or_insert_with(|| ExchangeItem {
                            name: normalized,
                            ..Default::default()
                        });
                    item.a = parse_f64(lowest_ask);
                    item.b = parse_f64(highest_bid);
                    item.trade24_count = parse_f64(quote_volume);
                }
            }
            Err(e) => {
                tracing::error!("gate spot: connection failed: {}", e);
            }
        }
        backoff_sleep(attempt).await;
        attempt = attempt.saturating_add(1);
    }
}

// ── Futures ────────────────────────────────────────────────────────────────

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

        let mut funding_info = contract_info.funding_info;
        let mut current_symbols: HashSet<String> =
            contract_info.symbols.iter().cloned().collect();

        tracing::info!("gate futures: connecting...");
        let url = "wss://fx-ws.gateio.ws/v4/ws/usdt";
        match connect_async(url).await {
            Ok((ws, _)) => {
                attempt = 0;
                tracing::info!("gate futures: connected");
                let (mut write, mut read) = ws.split();

                // Subscribe to futures.tickers in batches of 50
                for chunk in contract_info.symbols.chunks(50) {
                    let payload: Vec<&str> = chunk.iter().map(|s| s.as_str()).collect();
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
                        tracing::error!("gate futures: tickers subscribe send error: {}", e);
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }

                // Subscribe to futures.book_ticker in batches of 50
                for chunk in contract_info.symbols.chunks(50) {
                    let payload: Vec<&str> = chunk.iter().map(|s| s.as_str()).collect();
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
                        tracing::error!("gate futures: book_ticker subscribe send error: {}", e);
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }

                let mut refresh_interval =
                    tokio::time::interval(std::time::Duration::from_secs(300));
                refresh_interval.tick().await; // consume the immediate first tick

                // Read loop - Gate.io handles heartbeat via server-side pings (tungstenite auto-replies)
                loop {
                    tokio::select! {
                        msg = read.next() => {
                            let msg = match msg {
                                Some(Ok(m)) => m,
                                Some(Err(e)) => {
                                    tracing::warn!("gate futures: read error: {}", e);
                                    break;
                                }
                                None => {
                                    tracing::warn!("gate futures: stream ended");
                                    break;
                                }
                            };

                            let text = match msg {
                                Message::Text(t) => t,
                                Message::Ping(_) | Message::Pong(_) => continue,
                                Message::Close(_) => {
                                    tracing::warn!("gate futures: server closed connection");
                                    break;
                                }
                                _ => continue,
                            };

                            let text_str: &str = text.as_ref();

                            let ws_msg: GateWsMsg = match serde_json::from_str(text_str) {
                                Ok(v) => v,
                                Err(e) => {
                                    tracing::warn!("gate futures: parse error: {}", e);
                                    continue;
                                }
                            };

                            // Skip non-update messages (subscribe confirmations, etc.)
                            match ws_msg.event.as_deref() {
                                Some("update") => {}
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
                                    // futures.tickers result is an array
                                    let items: Vec<serde_json::Value> = match serde_json::from_value(result)
                                    {
                                        Ok(v) => v,
                                        Err(e) => {
                                            tracing::warn!(
                                                "gate futures: parse tickers array error: {}",
                                                e
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

                                        let volume_24h_quote = ticker
                                            .get("volume_24h_quote")
                                            .and_then(|v| v.as_str().or_else(|| v.as_f64().map(|_| "0")))
                                            .unwrap_or("0");
                                        // Handle both string and number for volume
                                        let volume_val = if let Some(s) = ticker.get("volume_24h_quote") {
                                            match s {
                                                serde_json::Value::String(sv) => parse_f64(sv),
                                                serde_json::Value::Number(n) => {
                                                    n.as_f64().unwrap_or(0.0)
                                                }
                                                _ => parse_f64(volume_24h_quote),
                                            }
                                        } else {
                                            0.0
                                        };

                                        let funding_rate = ticker
                                            .get("funding_rate")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("0");
                                        let mark_price = ticker
                                            .get("mark_price")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("0");
                                        let index_price = ticker
                                            .get("index_price")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("0");

                                        let rate_dec = parse_f64(funding_rate);
                                        let rate_str = format!("{:.3}", rate_dec * 100.0);

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
                                        item.mark_price = Some(mark_price.to_string());
                                        item.index_price = Some(index_price.to_string());

                                        // Ensure funding info stays applied
                                        if item.rate_interval.is_none() {
                                            if let Some((interval, max_rate)) =
                                                funding_info.get(contract)
                                            {
                                                item.rate_interval = Some(*interval);
                                                item.rate_max = Some(max_rate.clone());
                                            }
                                        }
                                    }
                                }
                                "futures.book_ticker" => {
                                    // futures.book_ticker result is a single object
                                    let contract = result
                                        .get("contract")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");
                                    if contract.is_empty() {
                                        continue;
                                    }

                                    let normalized = normalize_symbol(contract);

                                    let ask = result
                                        .get("a")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("0");
                                    let bid = result
                                        .get("b")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("0");

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

                                    item.a = parse_f64(ask);
                                    item.b = parse_f64(bid);

                                    // Ensure funding info stays applied
                                    if item.rate_interval.is_none() {
                                        if let Some((interval, max_rate)) =
                                            funding_info.get(contract)
                                        {
                                            item.rate_interval = Some(*interval);
                                            item.rate_max = Some(max_rate.clone());
                                        }
                                    }
                                }
                                _ => {
                                    tracing::warn!("gate futures: unknown channel: {}", channel);
                                }
                            }
                        }
                        _ = refresh_interval.tick() => {
                            tracing::info!("gate futures: refreshing contracts list...");
                            let new_contract_info = fetch_futures_contracts(&client).await;
                            if new_contract_info.symbols.is_empty() {
                                tracing::warn!("gate futures: refresh returned empty list, skipping");
                                continue;
                            }

                            let new_symbols: HashSet<String> =
                                new_contract_info.symbols.iter().cloned().collect();
                            tracing::info!(
                                "gate futures: refresh found {} contracts",
                                new_symbols.len()
                            );

                            // Unsubscribe removed symbols from both channels
                            let removed: Vec<String> = current_symbols
                                .difference(&new_symbols)
                                .cloned()
                                .collect();
                            if !removed.is_empty() {
                                tracing::info!(
                                    "gate futures: unsubscribing {} removed symbols",
                                    removed.len()
                                );
                                for chunk in removed.chunks(50) {
                                    let payload: Vec<&str> = chunk.iter().map(|s| s.as_str()).collect();
                                    let now_secs = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap()
                                        .as_secs();
                                    let unsub_tickers = serde_json::json!({
                                        "time": now_secs,
                                        "channel": "futures.tickers",
                                        "event": "unsubscribe",
                                        "payload": payload
                                    });
                                    if let Err(e) = write.send(Message::Text(unsub_tickers.to_string().into())).await {
                                        tracing::error!("gate futures: unsub tickers send error: {}", e);
                                        break;
                                    }
                                    let unsub_book = serde_json::json!({
                                        "time": now_secs,
                                        "channel": "futures.book_ticker",
                                        "event": "unsubscribe",
                                        "payload": payload
                                    });
                                    if let Err(e) = write.send(Message::Text(unsub_book.to_string().into())).await {
                                        tracing::error!("gate futures: unsub book_ticker send error: {}", e);
                                        break;
                                    }
                                }
                            }

                            // Subscribe new symbols to both channels
                            let added: Vec<String> = new_symbols
                                .difference(&current_symbols)
                                .cloned()
                                .collect();
                            if !added.is_empty() {
                                tracing::info!(
                                    "gate futures: subscribing {} new symbols",
                                    added.len()
                                );
                                for chunk in added.chunks(50) {
                                    let payload: Vec<&str> = chunk.iter().map(|s| s.as_str()).collect();
                                    let now_secs = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap()
                                        .as_secs();
                                    let sub_tickers = serde_json::json!({
                                        "time": now_secs,
                                        "channel": "futures.tickers",
                                        "event": "subscribe",
                                        "payload": payload
                                    });
                                    if let Err(e) = write.send(Message::Text(sub_tickers.to_string().into())).await {
                                        tracing::error!("gate futures: sub tickers send error: {}", e);
                                        break;
                                    }
                                    let sub_book = serde_json::json!({
                                        "time": now_secs,
                                        "channel": "futures.book_ticker",
                                        "event": "subscribe",
                                        "payload": payload
                                    });
                                    if let Err(e) = write.send(Message::Text(sub_book.to_string().into())).await {
                                        tracing::error!("gate futures: sub book_ticker send error: {}", e);
                                        break;
                                    }
                                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                                }
                            }

                            // Clean stale cache entries
                            // Cache keys are normalized (e.g. "BTCUSDT") but symbols are
                            // Gate format (e.g. "BTC_USDT"), so build valid normalized set
                            {
                                let valid_normalized: HashSet<String> = new_symbols
                                    .iter()
                                    .map(|s| normalize_symbol(s))
                                    .collect();
                                let mut section = cache.gate_future.write().await;
                                let before = section.items.len();
                                section.items.retain(|k, _| valid_normalized.contains(k));
                                let removed_count = before - section.items.len();
                                if removed_count > 0 {
                                    tracing::info!(
                                        "gate futures: removed {} stale cache entries",
                                        removed_count
                                    );
                                }
                            }

                            current_symbols = new_symbols;
                            funding_info = new_contract_info.funding_info;
                        }
                    }
                }
            }
            Err(e) => {
                tracing::error!("gate futures: connection failed: {}", e);
            }
        }
        backoff_sleep(attempt).await;
        attempt = attempt.saturating_add(1);
    }
}

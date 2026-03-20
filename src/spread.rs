use crate::cache::SharedCache;
use crate::exchanges::now_ms;
use crate::models::PriceLevel;
use serde::Serialize;
use std::collections::HashSet;

// ── Exchange name enum ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExchangeName {
    Binance,
    Bybit,
    Okx,
    Gate,
    Bitget,
    Zoomex,
}

impl ExchangeName {
    pub fn as_str(&self) -> &'static str {
        match self {
            ExchangeName::Binance => "binance",
            ExchangeName::Bybit => "bybit",
            ExchangeName::Okx => "okx",
            ExchangeName::Gate => "gate",
            ExchangeName::Bitget => "bitget",
            ExchangeName::Zoomex => "zoomex",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "binance" => Some(ExchangeName::Binance),
            "bybit" => Some(ExchangeName::Bybit),
            "okx" => Some(ExchangeName::Okx),
            "gate" => Some(ExchangeName::Gate),
            "bitget" => Some(ExchangeName::Bitget),
            "zoomex" => Some(ExchangeName::Zoomex),
            _ => None,
        }
    }
}

// ── Event / message types ───────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct TickerChanged {
    pub symbol: String,
}

#[derive(Clone, Serialize)]
pub struct Volume24h {
    pub long: f64,
    pub short: f64,
}

#[derive(Clone, Serialize)]
pub struct SpreadOpportunity {
    pub symbol: String,
    pub long_exchange: &'static str,
    pub short_exchange: &'static str,
    pub long_ask: f64,
    pub long_bid: f64,
    pub short_bid: f64,
    pub short_ask: f64,
    pub spread_percent: f64,
    pub volume_24h: Volume24h,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub long_asks: Vec<PriceLevel>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub short_bids: Vec<PriceLevel>,
    pub long_mark_price: f64,
    pub long_index_price: f64,
    pub short_mark_price: f64,
    pub short_index_price: f64,
    pub ts: u64,
    #[serde(rename = "depthTs", skip_serializing_if = "is_zero_u64")]
    pub depth_ts: u64,
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SpreadMessage {
    Snapshot { data: Vec<SpreadOpportunity> },
    Update { data: SpreadOpportunity },
    BatchUpdate { data: Vec<SpreadOpportunity> },
}

// ── Internal ticker struct ──────────────────────────────────────────────────

pub struct SymbolTicker {
    pub exchange: ExchangeName,
    pub ask: f64,
    pub bid: f64,
    pub ts: u64,
    pub depth_ts: u64,
    pub volume_24h: f64,
    pub asks: Vec<crate::models::PriceLevel>,
    pub bids: Vec<crate::models::PriceLevel>,
    pub mark_price: f64,
    pub index_price: f64,
}

// ── Read tickers from cache ─────────────────────────────────────────────────

pub async fn read_symbol_tickers(cache: &SharedCache, symbol: &str) -> Vec<SymbolTicker> {
    let now = now_ms();
    let mut tickers = Vec::new();

    // Read each section sequentially — acquire and release one lock at a time
    // to avoid potential deadlocks with the background serializer's write locks.
    let sections = [
        (ExchangeName::Binance, &cache.binance_future),
        (ExchangeName::Bybit, &cache.bybit_future),
        (ExchangeName::Okx, &cache.okx_future),
        (ExchangeName::Gate, &cache.gate_future),
        (ExchangeName::Bitget, &cache.bitget_future),
        (ExchangeName::Zoomex, &cache.zoomex_future),
    ];

    for (exchange, lock) in &sections {
        let section = lock.read().await;
        if let Some(item) = section.items.get(symbol) {
            if item.ts == 0 || now.saturating_sub(item.ts) > 10_000 {
                continue;
            }
            if item.a <= 0.0 || item.b <= 0.0 {
                continue;
            }
            tickers.push(SymbolTicker {
                exchange: *exchange,
                ask: item.a,
                bid: item.b,
                ts: item.ts,
                depth_ts: item.depth_ts,
                volume_24h: item.trade24_count,
                asks: item.asks.clone(),
                bids: item.bids.clone(),
                mark_price: item.mark_price.as_ref()
                    .and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0),
                index_price: item.index_price.as_ref()
                    .and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0),
            });
        }
    }

    tickers
}

/// Read a single exchange's ticker for a symbol without freshness filtering.
/// Returns (ask, bid, ts) or None if the symbol doesn't exist or prices are invalid.
pub async fn read_exchange_ticker(
    cache: &SharedCache,
    exchange: ExchangeName,
    symbol: &str,
) -> Option<(f64, f64, u64)> {
    let lock = match exchange {
        ExchangeName::Binance => &cache.binance_future,
        ExchangeName::Bybit => &cache.bybit_future,
        ExchangeName::Okx => &cache.okx_future,
        ExchangeName::Gate => &cache.gate_future,
        ExchangeName::Bitget => &cache.bitget_future,
        ExchangeName::Zoomex => &cache.zoomex_future,
    };
    let section = lock.read().await;
    if let Some(item) = section.items.get(symbol) {
        if item.ts > 0 && item.a > 0.0 && item.b > 0.0 {
            return Some((item.a, item.b, item.ts));
        }
    }
    None
}

// ── Compute spreads for a symbol ────────────────────────────────────────────

pub fn compute_spreads(symbol: &str, tickers: &[SymbolTicker]) -> Vec<SpreadOpportunity> {
    let mut results = Vec::new();

    for i in 0..tickers.len() {
        for j in (i + 1)..tickers.len() {
            let t1 = &tickers[i];
            let t2 = &tickers[j];

            // Skip if cross-exchange timestamps differ by more than 5 seconds
            if t1.ts.abs_diff(t2.ts) > 5_000 {
                continue;
            }

            // Skip if price ratio is too large (> 1.5x)
            let max_price = t1.ask.max(t1.bid).max(t2.ask).max(t2.bid);
            let min_price = t1.ask.min(t1.bid).min(t2.ask).min(t2.bid);
            if min_price > 0.0 && max_price / min_price > 1.5 {
                continue;
            }

            // Emit both directions so downstream clients always have fresh data
            // for every (symbol, exchangeA, exchangeB) key.
            let pairs: [(&SymbolTicker, &SymbolTicker); 2] = [(t1, t2), (t2, t1)];

            for &(long, short) in &pairs {
                let mid = (long.ask + short.bid) / 2.0;
                if mid <= 0.0 {
                    continue;
                }
                let spread_pct = (short.bid - long.ask) / mid * 100.0;

                // Skip if spread is unreasonably large
                if spread_pct.abs() > 20.0 {
                    continue;
                }

                let spread_rounded = (spread_pct * 100.0).round() / 100.0;
                let ts = long.ts.max(short.ts);

                // Use the older depth_ts (conservative: if either is 0, result is 0)
                let depth_ts = if long.depth_ts == 0 || short.depth_ts == 0 {
                    0
                } else {
                    long.depth_ts.min(short.depth_ts)
                };

                // Only include L2 depth if both sides are fresh (within 5s of ticker ts).
                let depth_fresh = depth_ts > 0 && ts.saturating_sub(depth_ts) <= 5_000;

                results.push(SpreadOpportunity {
                    symbol: symbol.to_string(),
                    long_exchange: long.exchange.as_str(),
                    short_exchange: short.exchange.as_str(),
                    long_ask: long.ask,
                    long_bid: long.bid,
                    short_bid: short.bid,
                    short_ask: short.ask,
                    spread_percent: spread_rounded,
                    volume_24h: Volume24h {
                        long: long.volume_24h,
                        short: short.volume_24h,
                    },
                    long_asks: if depth_fresh { long.asks.clone() } else { Vec::new() },
                    short_bids: if depth_fresh { short.bids.clone() } else { Vec::new() },
                    long_mark_price: long.mark_price,
                    long_index_price: long.index_price,
                    short_mark_price: short.mark_price,
                    short_index_price: short.index_price,
                    ts,
                    depth_ts,
                });
            }
        }
    }

    results
}

// ── Collect all symbols from cache ──────────────────────────────────────────

pub async fn collect_all_symbols(cache: &SharedCache) -> Vec<String> {
    let mut symbols = HashSet::new();

    let sections = [
        &cache.binance_future,
        &cache.bybit_future,
        &cache.okx_future,
        &cache.gate_future,
        &cache.bitget_future,
        &cache.zoomex_future,
    ];

    for lock in sections {
        let section = lock.read().await;
        for key in section.items.keys() {
            symbols.insert(key.clone());
        }
    }

    let mut result: Vec<String> = symbols.into_iter().collect();
    result.sort();
    result
}

// ── Spread calculator task ─────────────────────────────────────────────────

/// Spread calculator task: event-driven, wakes immediately on TickerChanged via
/// `recv().await`, then drains all pending events, deduplicates by symbol, and
/// computes spreads for the changed symbols. On lag, performs a full refresh
/// across all symbols so that downstream consumers never starve for data.
///
/// Throttle: max once per 500ms per pair.

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ExchangeItem;

    fn make_ticker(
        exchange: ExchangeName,
        ask: f64,
        bid: f64,
        mark_price: f64,
        index_price: f64,
    ) -> SymbolTicker {
        let now = crate::exchanges::now_ms();
        SymbolTicker {
            exchange,
            ask,
            bid,
            ts: now,
            depth_ts: 0,
            volume_24h: 1_000_000.0,
            asks: Vec::new(),
            bids: Vec::new(),
            mark_price,
            index_price,
        }
    }

    // ── compute_spreads: mark/index price passthrough ─────────────────────

    #[test]
    fn compute_spreads_passes_through_mark_index_prices() {
        let tickers = vec![
            make_ticker(ExchangeName::Binance, 100.5, 100.0, 100.3, 100.1),
            make_ticker(ExchangeName::Okx, 100.8, 100.2, 100.6, 100.2),
        ];

        let results = compute_spreads("BTCUSDT", &tickers);
        assert_eq!(results.len(), 2, "should produce 2 directions");

        // Find binance-long / okx-short direction
        let binance_long = results.iter().find(|o| o.long_exchange == "binance").unwrap();
        assert_eq!(binance_long.long_mark_price, 100.3);
        assert_eq!(binance_long.long_index_price, 100.1);
        assert_eq!(binance_long.short_mark_price, 100.6);
        assert_eq!(binance_long.short_index_price, 100.2);

        // Find okx-long / binance-short direction
        let okx_long = results.iter().find(|o| o.long_exchange == "okx").unwrap();
        assert_eq!(okx_long.long_mark_price, 100.6);
        assert_eq!(okx_long.long_index_price, 100.2);
        assert_eq!(okx_long.short_mark_price, 100.3);
        assert_eq!(okx_long.short_index_price, 100.1);
    }

    #[test]
    fn compute_spreads_zero_mark_index_passes_through() {
        // When mark/index prices are 0 (unavailable), they should still be passed through as 0
        let tickers = vec![
            make_ticker(ExchangeName::Binance, 50.0, 49.8, 0.0, 0.0),
            make_ticker(ExchangeName::Bybit, 50.1, 49.9, 50.05, 50.0),
        ];

        let results = compute_spreads("ETHUSDT", &tickers);
        let binance_long = results.iter().find(|o| o.long_exchange == "binance").unwrap();
        assert_eq!(binance_long.long_mark_price, 0.0);
        assert_eq!(binance_long.long_index_price, 0.0);
        assert_eq!(binance_long.short_mark_price, 50.05);
        assert_eq!(binance_long.short_index_price, 50.0);
    }

    #[test]
    fn compute_spreads_three_exchanges_produces_six_results() {
        let tickers = vec![
            make_ticker(ExchangeName::Binance, 100.0, 99.8, 99.9, 99.85),
            make_ticker(ExchangeName::Okx, 100.1, 99.9, 100.0, 99.95),
            make_ticker(ExchangeName::Bybit, 100.2, 100.0, 100.1, 100.05),
        ];

        let results = compute_spreads("SOLUSDT", &tickers);
        // 3 pairs × 2 directions = 6
        assert_eq!(results.len(), 6);

        // Every result should have non-zero mark/index prices
        for opp in &results {
            assert!(opp.long_mark_price > 0.0, "long_mark_price should be set for {}-{}", opp.long_exchange, opp.short_exchange);
            assert!(opp.long_index_price > 0.0, "long_index_price should be set");
            assert!(opp.short_mark_price > 0.0, "short_mark_price should be set");
            assert!(opp.short_index_price > 0.0, "short_index_price should be set");
        }
    }

    // ── read_symbol_tickers: ExchangeItem → SymbolTicker parsing ─────────

    #[tokio::test]
    async fn read_symbol_tickers_parses_mark_index_from_cache() {
        let (ticker_tx, _) = tokio::sync::broadcast::channel::<TickerChanged>(16);
        let cache: SharedCache = std::sync::Arc::new(crate::cache::Cache::new(ticker_tx));
        let now = crate::exchanges::now_ms();

        // Insert an item with mark/index price into binance cache
        {
            let mut section = cache.binance_future.write().await;
            section.items.insert("BTCUSDT".to_string(), ExchangeItem {
                name: "BTCUSDT".to_string(),
                ts: now,
                a: 84000.0,
                b: 83990.0,
                mark_price: Some("84005.50".to_string()),
                index_price: Some("84010.25".to_string()),
                ..Default::default()
            });
        }

        let tickers = read_symbol_tickers(&cache, "BTCUSDT").await;
        assert_eq!(tickers.len(), 1);
        assert_eq!(tickers[0].mark_price, 84005.50);
        assert_eq!(tickers[0].index_price, 84010.25);
    }

    #[tokio::test]
    async fn read_symbol_tickers_returns_zero_for_missing_mark_index() {
        let (ticker_tx, _) = tokio::sync::broadcast::channel::<TickerChanged>(16);
        let cache: SharedCache = std::sync::Arc::new(crate::cache::Cache::new(ticker_tx));
        let now = crate::exchanges::now_ms();

        // Insert item without mark/index price
        {
            let mut section = cache.okx_future.write().await;
            section.items.insert("ETHUSDT".to_string(), ExchangeItem {
                name: "ETHUSDT".to_string(),
                ts: now,
                a: 3000.0,
                b: 2999.0,
                mark_price: None,
                index_price: None,
                ..Default::default()
            });
        }

        let tickers = read_symbol_tickers(&cache, "ETHUSDT").await;
        assert_eq!(tickers.len(), 1);
        assert_eq!(tickers[0].mark_price, 0.0);
        assert_eq!(tickers[0].index_price, 0.0);
    }

    #[tokio::test]
    async fn read_symbol_tickers_handles_invalid_price_strings() {
        let (ticker_tx, _) = tokio::sync::broadcast::channel::<TickerChanged>(16);
        let cache: SharedCache = std::sync::Arc::new(crate::cache::Cache::new(ticker_tx));
        let now = crate::exchanges::now_ms();

        {
            let mut section = cache.bybit_future.write().await;
            section.items.insert("XRPUSDT".to_string(), ExchangeItem {
                name: "XRPUSDT".to_string(),
                ts: now,
                a: 0.5,
                b: 0.49,
                mark_price: Some("not_a_number".to_string()),
                index_price: Some("".to_string()),
                ..Default::default()
            });
        }

        let tickers = read_symbol_tickers(&cache, "XRPUSDT").await;
        assert_eq!(tickers.len(), 1);
        assert_eq!(tickers[0].mark_price, 0.0, "invalid string should parse as 0.0");
        assert_eq!(tickers[0].index_price, 0.0, "empty string should parse as 0.0");
    }

    // ── End-to-end: cache → tickers → compute_spreads ────────────────────

    #[tokio::test]
    async fn end_to_end_mark_index_through_spread_pipeline() {
        let (ticker_tx, _) = tokio::sync::broadcast::channel::<TickerChanged>(16);
        let cache: SharedCache = std::sync::Arc::new(crate::cache::Cache::new(ticker_tx));
        let now = crate::exchanges::now_ms();

        // Simulate two exchanges with different mark/index prices (PI divergence scenario)
        {
            let mut section = cache.binance_future.write().await;
            section.items.insert("SOLUSDT".to_string(), ExchangeItem {
                name: "SOLUSDT".to_string(),
                ts: now,
                a: 120.5,
                b: 120.3,
                mark_price: Some("120.45".to_string()),   // PI = (120.45 - 120.40) / 120.40 = +0.0415%
                index_price: Some("120.40".to_string()),
                ..Default::default()
            });
        }
        {
            let mut section = cache.okx_future.write().await;
            section.items.insert("SOLUSDT".to_string(), ExchangeItem {
                name: "SOLUSDT".to_string(),
                ts: now,
                a: 120.6,
                b: 120.4,
                mark_price: Some("120.30".to_string()),   // PI = (120.30 - 120.40) / 120.40 = -0.0831%
                index_price: Some("120.40".to_string()),
                ..Default::default()
            });
        }

        // Read tickers from cache
        let tickers = read_symbol_tickers(&cache, "SOLUSDT").await;
        assert_eq!(tickers.len(), 2);

        // Compute spreads
        let opps = compute_spreads("SOLUSDT", &tickers);
        assert!(opps.len() >= 2);

        // Verify PI divergence can be calculated from the output
        let opp = &opps[0];
        let long_pi = (opp.long_mark_price - opp.long_index_price) / opp.long_index_price;
        let short_pi = (opp.short_mark_price - opp.short_index_price) / opp.short_index_price;
        let divergence = (long_pi - short_pi).abs() * 100.0;

        assert!(opp.long_mark_price > 0.0, "long_mark_price should be populated");
        assert!(opp.long_index_price > 0.0, "long_index_price should be populated");
        assert!(opp.short_mark_price > 0.0, "short_mark_price should be populated");
        assert!(opp.short_index_price > 0.0, "short_index_price should be populated");
        assert!(divergence > 0.0, "divergence should be non-zero when mark prices differ");
    }
}

pub async fn run_spread_calculator(
    cache: SharedCache,
    mut ticker_rx: tokio::sync::broadcast::Receiver<TickerChanged>,
    spread_tx: tokio::sync::broadcast::Sender<SpreadOpportunity>,
) {
    // Throttle state: (symbol, long_exchange, short_exchange) → last_sent_ms
    let mut last_sent: std::collections::HashMap<(String, &'static str, &'static str), u64> =
        std::collections::HashMap::new();

    loop {
        // Wait for the first ticker change event (zero-latency wakeup)
        let mut changed = HashSet::new();
        let mut do_full_refresh = false;

        match ticker_rx.recv().await {
            Ok(e) => { changed.insert(e.symbol); }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::debug!("spread calculator: lagged {} events", n);
                do_full_refresh = true;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                tracing::error!("spread calculator: ticker channel closed");
                return;
            }
        }

        // Drain all remaining pending events (dedup, batch processing)
        loop {
            match ticker_rx.try_recv() {
                Ok(e) => { changed.insert(e.symbol); }
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(n)) => {
                    tracing::debug!("spread calculator: lagged {} events (drain)", n);
                    do_full_refresh = true;
                    // After Lagged, the receiver is reset — continue draining
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                    tracing::error!("spread calculator: ticker channel closed");
                    return;
                }
            }
        }

        let symbols: Vec<String> = if do_full_refresh {
            collect_all_symbols(&cache).await
        } else {
            changed.into_iter().collect()
        };

        let now = now_ms();

        for symbol in &symbols {
            let tickers = read_symbol_tickers(&cache, symbol).await;
            if tickers.len() < 2 {
                continue;
            }

            let opportunities = compute_spreads(symbol, &tickers);

            for opp in opportunities {
                let key = (
                    opp.symbol.clone(),
                    opp.long_exchange,
                    opp.short_exchange,
                );

                // Throttle: max once per 500ms per pair
                let should_send = match last_sent.get(&key) {
                    Some(&last_ts) => now.saturating_sub(last_ts) >= 500,
                    None => true,
                };

                if should_send {
                    last_sent.insert(key, now);
                    let _ = spread_tx.send(opp);
                }
            }
        }
    }
}

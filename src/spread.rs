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
            });
        }
    }

    tickers
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

            // Direction 1: long t1 (buy at t1.ask), short t2 (sell at t2.bid)
            let mid1 = (t1.ask + t2.bid) / 2.0;
            let spread1 = if mid1 > 0.0 {
                (t2.bid - t1.ask) / mid1 * 100.0
            } else {
                continue;
            };

            // Direction 2: long t2 (buy at t2.ask), short t1 (sell at t1.bid)
            let mid2 = (t2.ask + t1.bid) / 2.0;
            let spread2 = if mid2 > 0.0 {
                (t1.bid - t2.ask) / mid2 * 100.0
            } else {
                continue;
            };

            let (long, short, spread_pct) = if spread1 >= spread2 {
                (t1, t2, spread1)
            } else {
                (t2, t1, spread2)
            };

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
                long_asks: long.asks.clone(),
                short_bids: short.bids.clone(),
                ts,
                depth_ts,
            });
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

/// Spread calculator task: runs on a fixed 200ms interval, drains all pending
/// TickerChanged events, deduplicates by symbol, and computes spreads for the
/// changed symbols. On lag, performs a full refresh across all symbols so that
/// downstream consumers never starve for data.
///
/// Tiered throttle: spread > 3% → immediate, spread ≤ 3% → max once per 500ms per pair.
pub async fn run_spread_calculator(
    cache: SharedCache,
    mut ticker_rx: tokio::sync::broadcast::Receiver<TickerChanged>,
    spread_tx: tokio::sync::broadcast::Sender<SpreadOpportunity>,
) {
    // Throttle state: (symbol, long_exchange, short_exchange) → last_sent_ms
    let mut last_sent: std::collections::HashMap<(String, &'static str, &'static str), u64> =
        std::collections::HashMap::new();

    let mut interval = tokio::time::interval(std::time::Duration::from_millis(200));

    loop {
        interval.tick().await;

        // Drain all pending events from the broadcast channel
        let mut changed = HashSet::new();
        let mut do_full_refresh = false;

        loop {
            match ticker_rx.try_recv() {
                Ok(e) => { changed.insert(e.symbol); }
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(n)) => {
                    tracing::debug!("spread calculator: lagged {} events", n);
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
        } else if changed.is_empty() {
            continue; // Nothing to process this tick
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

                // Tiered throttle
                let should_send = if opp.spread_percent > 3.0 {
                    true // High-value: always send immediately
                } else {
                    match last_sent.get(&key) {
                        Some(&last_ts) => now.saturating_sub(last_ts) >= 500,
                        None => true,
                    }
                };

                if should_send {
                    last_sent.insert(key, now);
                    let _ = spread_tx.send(opp);
                }
            }
        }
    }
}

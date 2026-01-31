use crate::cache::SharedCache;
use crate::exchanges::now_ms;
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

pub const ALL_EXCHANGES: [ExchangeName; 6] = [
    ExchangeName::Binance,
    ExchangeName::Bybit,
    ExchangeName::Okx,
    ExchangeName::Gate,
    ExchangeName::Bitget,
    ExchangeName::Zoomex,
];

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
    pub exchange: ExchangeName,
    pub symbol: String,
}

#[derive(Serialize)]
pub struct Volume24h {
    pub long: f64,
    pub short: f64,
}

#[derive(Serialize)]
pub struct SpreadOpportunity {
    pub symbol: String,
    pub long_exchange: &'static str,
    pub short_exchange: &'static str,
    pub long_ask: f64,
    pub short_bid: f64,
    pub spread_percent: f64,
    pub volume_24h: Volume24h,
    pub ts: u64,
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
    pub volume_24h: f64,
}

// ── Read tickers from cache ─────────────────────────────────────────────────

pub async fn read_symbol_tickers(cache: &SharedCache, symbol: &str) -> Vec<SymbolTicker> {
    let now = now_ms();
    let mut tickers = Vec::new();

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
                volume_24h: item.trade24_count,
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

            results.push(SpreadOpportunity {
                symbol: symbol.to_string(),
                long_exchange: long.exchange.as_str(),
                short_exchange: short.exchange.as_str(),
                long_ask: long.ask,
                short_bid: short.bid,
                spread_percent: spread_rounded,
                volume_24h: Volume24h {
                    long: long.volume_24h,
                    short: short.volume_24h,
                },
                ts,
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

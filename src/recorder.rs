use crate::cache::SharedCache;
use crate::exchanges::now_ms;
use clickhouse::Client;
use serde::Serialize;
use std::time::Duration;
use time::OffsetDateTime;

#[derive(clickhouse::Row, Serialize)]
struct TickerSnapshot {
    #[serde(with = "clickhouse::serde::time::datetime64::millis")]
    ts: OffsetDateTime,
    exchange: String,
    symbol: String,
    bid: f64,
    ask: f64,
    volume_24h: f64,
    funding_rate: Option<f64>,
    funding_rate_max: Option<f64>,
    funding_interval_hours: Option<u32>,
    index_price: Option<f64>,
    mark_price: Option<f64>,
    asks: Vec<(f64, f64)>,
    bids: Vec<(f64, f64)>,
}

const CREATE_TABLE_SQL: &str = "
CREATE TABLE IF NOT EXISTS ticker_snapshots (
    ts            DateTime64(3),
    exchange      LowCardinality(String),
    symbol        LowCardinality(String),
    bid           Float64,
    ask           Float64,
    volume_24h    Float64,
    funding_rate            Nullable(Float64),
    funding_rate_max        Nullable(Float64),
    funding_interval_hours  Nullable(UInt32),
    index_price             Nullable(Float64),
    mark_price              Nullable(Float64),
    asks                    Array(Tuple(Float64, Float64)),
    bids                    Array(Tuple(Float64, Float64))
) ENGINE = MergeTree()
PARTITION BY toYYYYMMDD(ts)
ORDER BY (exchange, symbol, ts)
TTL toDateTime(ts) + INTERVAL 30 DAY DELETE
SETTINGS index_granularity = 8192
";

fn parse_opt_f64(s: &Option<String>) -> Option<f64> {
    s.as_ref().and_then(|v| v.parse::<f64>().ok())
}

pub async fn run_recorder(cache: SharedCache, clickhouse_url: String) {
    let ch = Client::default()
        .with_url(&clickhouse_url)
        .with_option("async_insert", "1")
        .with_option("wait_for_async_insert", "0");

    // Ensure table exists
    if let Err(e) = ch.query(CREATE_TABLE_SQL).execute().await {
        tracing::error!("recorder: failed to create table: {}", e);
        return;
    }
    tracing::info!("recorder: connected to ClickHouse at {}", clickhouse_url);

    let mut interval = tokio::time::interval(Duration::from_secs(1));
    loop {
        interval.tick().await;

        let now = now_ms();
        let ts = OffsetDateTime::from_unix_timestamp_nanos((now as i128) * 1_000_000)
            .unwrap_or(OffsetDateTime::now_utc());

        let sections = [
            ("binance", &cache.binance_future),
            ("bybit", &cache.bybit_future),
            ("okx", &cache.okx_future),
            ("gate", &cache.gate_future),
            ("bitget", &cache.bitget_future),
            ("zoomex", &cache.zoomex_future),
        ];

        let mut rows = Vec::with_capacity(2000);

        for (exchange, lock) in &sections {
            let section = lock.read().await;
            for item in section.items.values() {
                // Skip stale or invalid items
                if item.ts == 0 || now.saturating_sub(item.ts) > 60_000 {
                    continue;
                }
                if item.a <= 0.0 || item.b <= 0.0 {
                    continue;
                }

                rows.push(TickerSnapshot {
                    ts,
                    exchange: exchange.to_string(),
                    symbol: item.name.clone(),
                    bid: item.b,
                    ask: item.a,
                    volume_24h: item.trade24_count,
                    funding_rate: parse_opt_f64(&item.rate),
                    funding_rate_max: parse_opt_f64(&item.rate_max),
                    funding_interval_hours: item.rate_interval,
                    index_price: parse_opt_f64(&item.index_price),
                    mark_price: parse_opt_f64(&item.mark_price),
                    asks: item.asks.iter().map(|l| (l.price, l.qty)).collect(),
                    bids: item.bids.iter().map(|l| (l.price, l.qty)).collect(),
                });
            }
        }

        if rows.is_empty() {
            continue;
        }

        let row_count = rows.len();
        match ch.insert::<TickerSnapshot>("ticker_snapshots").await {
            Ok(mut insert) => {
                let mut ok = true;
                for row in rows {
                    if let Err(e) = insert.write(&row).await {
                        tracing::warn!("recorder: write error: {}", e);
                        ok = false;
                        break;
                    }
                }
                if ok {
                    if let Err(e) = insert.end().await {
                        tracing::warn!("recorder: insert end error: {}", e);
                    } else {
                        tracing::debug!("recorder: wrote {} rows", row_count);
                    }
                }
            }
            Err(e) => {
                tracing::warn!("recorder: insert init error: {}", e);
            }
        }
    }
}

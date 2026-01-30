pub mod binance;
pub mod bybit;
pub mod okx;
pub mod gate;
pub mod bitget;

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

pub async fn backoff_sleep(attempt: u32) {
    let secs = std::cmp::min(1u64 << attempt, 60);
    tracing::info!("reconnecting in {}s...", secs);
    tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
}

pub fn parse_f64(s: &str) -> f64 {
    s.parse::<f64>().unwrap_or(0.0)
}

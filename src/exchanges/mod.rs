pub mod binance;
pub mod bybit;
pub mod okx;
pub mod gate;
pub mod bitget;
pub mod zoomex;

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

pub async fn backoff_sleep(attempt: u32) {
    if attempt == 0 {
        return; // immediate reconnect on first attempt
    }
    let secs = std::cmp::min(1u64 << attempt, 5);
    tracing::info!("reconnecting in {}s...", secs);
    tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
}

pub fn parse_f64(s: &str) -> f64 {
    s.parse::<f64>().unwrap_or(0.0)
}

/// Parse a JSON value that may be either a string or a number into f64.
pub fn json_f64(v: &serde_json::Value) -> f64 {
    if let Some(s) = v.as_str() {
        parse_f64(s)
    } else if let Some(n) = v.as_f64() {
        n
    } else {
        0.0
    }
}

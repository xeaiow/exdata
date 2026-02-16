use crate::cache::SharedCache;
use crate::config::ValidatorConfig;
use crate::models::ExchangeItem;
use rand::seq::SliceRandom;
use serde::Deserialize;
use std::collections::HashMap;
use std::fmt::Write as FmtWrite;

// ── Baseline API response structs ───────────────────────────────────────────

#[derive(Deserialize)]
struct BaselineResponse {
    data: BaselineData,
}

#[derive(Deserialize)]
struct BaselineData {
    #[serde(rename = "binanceFuture", default)]
    binance_future: Option<BaselineSection>,
    #[serde(rename = "bybitFuture", default)]
    bybit_future: Option<BaselineSection>,
    #[serde(rename = "okxFuture", default)]
    okx_future: Option<BaselineSection>,
    #[serde(rename = "gateFuture", default)]
    gate_future: Option<BaselineSection>,
    #[serde(rename = "bitgetFuture", default)]
    bitget_future: Option<BaselineSection>,
}

#[derive(Deserialize)]
struct BaselineSection {
    #[serde(default)]
    list: Vec<BaselineItem>,
}

#[derive(Deserialize)]
struct BaselineItem {
    name: String,
    #[serde(default)]
    a: f64,
    #[serde(default)]
    b: f64,
    #[serde(rename = "rateInterval", default)]
    rate_interval: Option<u32>,
    #[serde(default)]
    rate: Option<String>,
    #[serde(rename = "rateMax", default)]
    rate_max: Option<String>,
    #[serde(rename = "indexPrice", default)]
    index_price: Option<String>,
    #[serde(rename = "markPrice", default)]
    mark_price: Option<String>,
}

// ── Exchange mapping ────────────────────────────────────────────────────────

struct ExchangeMapping {
    display_name: &'static str,
    baseline_key: &'static str,
}

const EXCHANGES: &[ExchangeMapping] = &[
    ExchangeMapping { display_name: "Binance", baseline_key: "binanceFuture" },
    ExchangeMapping { display_name: "OKX", baseline_key: "okxFuture" },
    ExchangeMapping { display_name: "Bitget", baseline_key: "bitgetFuture" },
    ExchangeMapping { display_name: "Gate", baseline_key: "gateFuture" },
    ExchangeMapping { display_name: "Zoomex", baseline_key: "bybitFuture" },
    ExchangeMapping { display_name: "Bybit", baseline_key: "bybitFuture" },
];

// ── Main loop ───────────────────────────────────────────────────────────────

pub async fn run(cache: SharedCache, client: reqwest::Client, config: ValidatorConfig) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(config.interval_secs));
    interval.tick().await; // consume the immediate first tick

    loop {
        interval.tick().await;
        if let Err(e) = run_cycle(&cache, &client, &config).await {
            tracing::error!("validator: cycle error: {}", e);
        }
    }
}

async fn run_cycle(
    cache: &SharedCache,
    client: &reqwest::Client,
    config: &ValidatorConfig,
) -> Result<(), String> {
    // 1. Fetch baseline
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let url = format!("{}?t={}", config.baseline_url, ts);

    let baseline = match client
        .get(&url)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
    {
        Ok(resp) => match resp.json::<BaselineResponse>().await {
            Ok(data) => data.data,
            Err(e) => {
                let msg = format!("基準 API 回應解析失敗: {}", e);
                tracing::error!("validator: {}", msg);
                send_alert(client, config, &format!("[ALERT] {}", msg)).await;
                return Err(msg);
            }
        },
        Err(e) => {
            let msg = if e.is_timeout() {
                "基準 API 請求超時 (10s)".to_string()
            } else {
                format!("基準 API 請求失敗: {}", e)
            };
            tracing::error!("validator: {}", msg);
            send_alert(client, config, &format!("[ALERT] {}", msg)).await;
            return Err(msg);
        }
    };

    // 2. Read own cache and build baseline lookup
    let own_sections = read_own_cache(cache).await;
    let baseline_sections = build_baseline_map(&baseline);

    // 3. Compare each exchange
    let mut alerts: Vec<String> = Vec::new();

    for (idx, mapping) in EXCHANGES.iter().enumerate() {
        let own = &own_sections[idx];
        let bl_items = baseline_sections.get(mapping.baseline_key);

        let bl_count = bl_items.map(|m| m.len()).unwrap_or(0);

        // Check disconnection
        if own.is_empty() {
            let msg = format!("{}: 斷線 (0 items，基準有 {} 個)", mapping.display_name, bl_count);
            tracing::warn!("validator: {}", msg);
            alerts.push(msg);
            continue;
        }

        let bl_map = match bl_items {
            Some(m) => m,
            None => continue,
        };

        // Find common symbols with valid prices
        let common: Vec<&String> = own
            .keys()
            .filter(|k| {
                if let (Some(e), Some(b)) = (own.get(*k), bl_map.get(k.as_str())) {
                    e.a > 0.0 && e.b > 0.0 && b.a > 0.0 && b.b > 0.0
                } else {
                    false
                }
            })
            .collect();

        if common.is_empty() {
            continue;
        }

        // Sample 10 (scope rng to avoid Send issue across await)
        let samples: Vec<&String> = {
            let mut rng = rand::thread_rng();
            let sample_size = common.len().min(10);
            common.choose_multiple(&mut rng, sample_size).cloned().collect()
        };

        for sym in &samples {
            let e = &own[*sym];
            let b = &bl_map[sym.as_str()];
            compare_item(mapping.display_name, sym, e, b, config, &mut alerts);
        }
    }

    // 4. Send alerts if any
    if alerts.is_empty() {
        tracing::info!("validator: all checks passed");
    } else {
        let mut msg = format!("[ALERT] 驗證異常 ({} 筆)\n", alerts.len());
        for a in &alerts {
            let _ = writeln!(msg, "• {}", a);
        }
        tracing::warn!("validator: {}", msg.trim());
        send_alert(client, config, msg.trim()).await;
    }

    Ok(())
}

// ── Compare a single item ───────────────────────────────────────────────────

fn compare_item(
    exchange: &str,
    symbol: &str,
    own: &ExchangeItem,
    bl: &BaselineItem,
    config: &ValidatorConfig,
    alerts: &mut Vec<String>,
) {
    // Price fields: a, b
    check_price_bps(exchange, symbol, "a", own.a, bl.a, config.price_threshold_bps, alerts);
    check_price_bps(exchange, symbol, "b", own.b, bl.b, config.price_threshold_bps, alerts);

    // markPrice
    if let (Some(ref ep), Some(ref bp)) = (&own.mark_price, &bl.mark_price) {
        if ep != "--" && bp != "--" {
            let ev = parse_f64(ep);
            let bv = parse_f64(bp);
            if ev > 0.0 && bv > 0.0 {
                check_price_bps(exchange, symbol, "markPrice", ev, bv, config.price_threshold_bps, alerts);
            }
        }
    }

    // indexPrice
    if let (Some(ref ep), Some(ref bp)) = (&own.index_price, &bl.index_price) {
        if ep != "--" && bp != "--" {
            let ev = parse_f64(ep);
            let bv = parse_f64(bp);
            if ev > 0.0 && bv > 0.0 {
                check_price_bps(exchange, symbol, "indexPrice", ev, bv, config.price_threshold_bps, alerts);
            }
        }
    }

    // rateInterval: exact match (skip if either side is None)
    if let (Some(ei), Some(bi)) = (own.rate_interval, bl.rate_interval) {
        if ei != bi {
            alerts.push(format!(
                "{} {}: rateInterval 不一致 exdata={} baseline={}",
                exchange, symbol, ei, bi
            ));
        }
    }

    // rate: absolute-difference tolerance
    if let (Some(ref ev), Some(ref bv)) = (&own.rate, &bl.rate) {
        let e_val = parse_f64(ev);
        let b_val = parse_f64(bv);
        if (e_val - b_val).abs() > config.rate_threshold {
            alerts.push(format!(
                "{} {}: rate 不一致 exdata={} baseline={} (差 {:.4}, 門檻 {})",
                exchange, symbol, ev, bv, (e_val - b_val).abs(), config.rate_threshold
            ));
        }
    }

    // rateMax: absolute-difference tolerance
    if let (Some(ref ev), Some(ref bv)) = (&own.rate_max, &bl.rate_max) {
        let e_val = parse_f64(ev);
        let b_val = parse_f64(bv);
        if (e_val - b_val).abs() > config.rate_threshold {
            alerts.push(format!(
                "{} {}: rateMax 不一致 exdata={} baseline={} (差 {:.4}, 門檻 {})",
                exchange, symbol, ev, bv, (e_val - b_val).abs(), config.rate_threshold
            ));
        }
    }
}

fn check_price_bps(
    exchange: &str,
    symbol: &str,
    field: &str,
    own_val: f64,
    bl_val: f64,
    threshold: f64,
    alerts: &mut Vec<String>,
) {
    if bl_val == 0.0 {
        return;
    }
    let bps = (own_val - bl_val) / bl_val * 10000.0;
    if bps.abs() > threshold {
        let diff = own_val - bl_val;
        alerts.push(format!(
            "{} {}: {} 價差 {:+.1} bps (門檻 {}) | exdata={} | baseline={} | diff={:+}",
            exchange, symbol, field, bps, threshold, own_val, bl_val, diff
        ));
    }
}

fn parse_f64(s: &str) -> f64 {
    s.parse::<f64>().unwrap_or(0.0)
}

// ── Read own cache ──────────────────────────────────────────────────────────

async fn read_own_cache(cache: &SharedCache) -> Vec<HashMap<String, ExchangeItem>> {
    let (binance, okx, bitget, gate, zoomex, bybit) = tokio::join!(
        cache.binance_future.read(),
        cache.okx_future.read(),
        cache.bitget_future.read(),
        cache.gate_future.read(),
        cache.zoomex_future.read(),
        cache.bybit_future.read(),
    );
    vec![
        binance.items.clone(),
        okx.items.clone(),
        bitget.items.clone(),
        gate.items.clone(),
        zoomex.items.clone(),
        bybit.items.clone(),
    ]
}

// ── Build baseline lookup ───────────────────────────────────────────────────

fn build_baseline_map(data: &BaselineData) -> HashMap<&str, HashMap<&str, &BaselineItem>> {
    let mut map = HashMap::new();

    if let Some(ref s) = data.binance_future {
        let m: HashMap<&str, &BaselineItem> = s.list.iter().map(|i| (i.name.as_str(), i)).collect();
        map.insert("binanceFuture", m);
    }
    if let Some(ref s) = data.okx_future {
        let m: HashMap<&str, &BaselineItem> = s.list.iter().map(|i| (i.name.as_str(), i)).collect();
        map.insert("okxFuture", m);
    }
    if let Some(ref s) = data.bitget_future {
        let m: HashMap<&str, &BaselineItem> = s.list.iter().map(|i| (i.name.as_str(), i)).collect();
        map.insert("bitgetFuture", m);
    }
    if let Some(ref s) = data.gate_future {
        let m: HashMap<&str, &BaselineItem> = s.list.iter().map(|i| (i.name.as_str(), i)).collect();
        map.insert("gateFuture", m);
    }
    if let Some(ref s) = data.bybit_future {
        let m: HashMap<&str, &BaselineItem> = s.list.iter().map(|i| (i.name.as_str(), i)).collect();
        map.insert("bybitFuture", m);
    }

    map
}

// ── Discord alert ───────────────────────────────────────────────────────────

async fn send_alert(client: &reqwest::Client, config: &ValidatorConfig, message: &str) {
    let webhook_url = match &config.discord_webhook_url {
        Some(url) if !url.is_empty() => url,
        _ => {
            tracing::info!("validator: no discord webhook configured, skipping alert");
            return;
        }
    };

    let payload = serde_json::json!({
        "content": message,
    });

    match client
        .post(webhook_url)
        .json(&payload)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            tracing::info!("validator: discord alert sent");
        }
        Ok(resp) => {
            tracing::warn!("validator: discord webhook returned {}", resp.status());
        }
        Err(e) => {
            tracing::error!("validator: discord webhook failed: {}", e);
        }
    }
}

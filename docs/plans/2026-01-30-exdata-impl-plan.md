# ExData Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build a Rust/Axum service that collects real-time Spot and Future data from 5 crypto exchanges (Binance, Bybit, OKX, Gate.io, Bitget) via WebSocket and exposes it via `GET /api/exdata`.

**Architecture:** 10 parallel tokio tasks (5 exchanges x 2 market types) maintain WebSocket connections. Data stored in-memory via `Arc<Cache>` with per-section `RwLock`. Axum HTTP server reads cache on each request. Exponential backoff reconnection on disconnect.

**Tech Stack:** Rust, Axum 0.8, Tokio, tokio-tungstenite, reqwest, serde, tracing

---

## Task 1: Project Scaffold

**Files:**
- Create: `Cargo.toml`
- Create: `src/main.rs`

**Step 1: Create Cargo.toml**

```toml
[package]
name = "exdata"
version = "0.1.0"
edition = "2021"

[dependencies]
axum = "0.8"
tokio = { version = "1", features = ["full"] }
tokio-tungstenite = { version = "0.26", features = ["native-tls"] }
reqwest = { version = "0.12", features = ["json"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tracing = "0.1"
tracing-subscriber = "0.3"
futures-util = "0.3"
```

**Step 2: Create minimal src/main.rs**

```rust
use axum::{Router, routing::get};

#[tokio::main]
async fn main() {
    tracing_subscriber::init();

    let app = Router::new().route("/api/exdata", get(|| async { "hello" }));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    tracing::info!("listening on 0.0.0.0:3000");
    axum::serve(listener, app).await.unwrap();
}
```

**Step 3: Verify**

Run: `cargo build`

**Step 4: Commit**

```bash
git add Cargo.toml src/main.rs
git commit -m "feat: project scaffold with axum"
```

---

## Task 2: Data Models

**Files:**
- Create: `src/models.rs`
- Modify: `src/main.rs` (add `mod models;`)

**Step 1: Create src/models.rs**

```rust
use serde::Serialize;
use std::collections::HashMap;

/// Unified item for both Spot and Future data.
/// Optional fields are skipped in JSON when None.
#[derive(Clone, Default, Serialize)]
pub struct ExchangeItem {
    pub name: String,
    pub a: f64,
    pub b: f64,
    #[serde(rename = "trade24Count")]
    pub trade24_count: f64,
    #[serde(rename = "rateInterval", skip_serializing_if = "Option::is_none")]
    pub rate_interval: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate: Option<String>,
    #[serde(rename = "rateMax", skip_serializing_if = "Option::is_none")]
    pub rate_max: Option<String>,
    #[serde(rename = "indexPrice", skip_serializing_if = "Option::is_none")]
    pub index_price: Option<String>,
    #[serde(rename = "markPrice", skip_serializing_if = "Option::is_none")]
    pub mark_price: Option<String>,
}

/// Internal storage for one exchange section.
pub struct ExchangeSection {
    pub ts: u64,
    pub items: HashMap<String, ExchangeItem>,
}

impl ExchangeSection {
    pub fn new() -> Self {
        Self {
            ts: 0,
            items: HashMap::new(),
        }
    }

    pub fn to_response(&self) -> ExchangeSectionResponse {
        ExchangeSectionResponse {
            ts: self.ts,
            list: self.items.values().cloned().collect(),
        }
    }
}

/// Serializable response for one exchange section.
#[derive(Serialize)]
pub struct ExchangeSectionResponse {
    pub ts: u64,
    pub list: Vec<ExchangeItem>,
}

/// Top-level API response.
#[derive(Serialize)]
pub struct ApiResponse {
    pub code: u32,
    pub data: ApiData,
}

#[derive(Serialize)]
pub struct ApiData {
    #[serde(rename = "binanceSpot")]
    pub binance_spot: ExchangeSectionResponse,
    #[serde(rename = "binanceFuture")]
    pub binance_future: ExchangeSectionResponse,
    #[serde(rename = "bybitSpot")]
    pub bybit_spot: ExchangeSectionResponse,
    #[serde(rename = "bybitFuture")]
    pub bybit_future: ExchangeSectionResponse,
    #[serde(rename = "okxSpot")]
    pub okx_spot: ExchangeSectionResponse,
    #[serde(rename = "okxFuture")]
    pub okx_future: ExchangeSectionResponse,
    #[serde(rename = "gateSpot")]
    pub gate_spot: ExchangeSectionResponse,
    #[serde(rename = "gateFuture")]
    pub gate_future: ExchangeSectionResponse,
    #[serde(rename = "bitgetSpot")]
    pub bitget_spot: ExchangeSectionResponse,
    #[serde(rename = "bitgetFuture")]
    pub bitget_future: ExchangeSectionResponse,
}
```

**Step 2: Add mod to main.rs**

Add `mod models;` at top of `src/main.rs`.

**Step 3: Verify**

Run: `cargo build`

**Step 4: Commit**

```bash
git add src/models.rs src/main.rs
git commit -m "feat: add data models"
```

---

## Task 3: Cache

**Files:**
- Create: `src/cache.rs`
- Modify: `src/main.rs` (add `mod cache;`)

**Step 1: Create src/cache.rs**

```rust
use crate::models::ExchangeSection;
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct Cache {
    pub binance_spot: RwLock<ExchangeSection>,
    pub binance_future: RwLock<ExchangeSection>,
    pub bybit_spot: RwLock<ExchangeSection>,
    pub bybit_future: RwLock<ExchangeSection>,
    pub okx_spot: RwLock<ExchangeSection>,
    pub okx_future: RwLock<ExchangeSection>,
    pub gate_spot: RwLock<ExchangeSection>,
    pub gate_future: RwLock<ExchangeSection>,
    pub bitget_spot: RwLock<ExchangeSection>,
    pub bitget_future: RwLock<ExchangeSection>,
}

impl Cache {
    pub fn new() -> Self {
        Self {
            binance_spot: RwLock::new(ExchangeSection::new()),
            binance_future: RwLock::new(ExchangeSection::new()),
            bybit_spot: RwLock::new(ExchangeSection::new()),
            bybit_future: RwLock::new(ExchangeSection::new()),
            okx_spot: RwLock::new(ExchangeSection::new()),
            okx_future: RwLock::new(ExchangeSection::new()),
            gate_spot: RwLock::new(ExchangeSection::new()),
            gate_future: RwLock::new(ExchangeSection::new()),
            bitget_spot: RwLock::new(ExchangeSection::new()),
            bitget_future: RwLock::new(ExchangeSection::new()),
        }
    }
}

pub type SharedCache = Arc<Cache>;
```

**Step 2: Commit**

```bash
git add src/cache.rs src/main.rs
git commit -m "feat: add shared cache"
```

---

## Task 4: API Handler

**Files:**
- Create: `src/api.rs`
- Modify: `src/main.rs` (add handler, wire cache into router)

**Step 1: Create src/api.rs**

```rust
use axum::{extract::State, Json};
use crate::cache::SharedCache;
use crate::models::{ApiResponse, ApiData};

pub async fn get_exdata(State(cache): State<SharedCache>) -> Json<ApiResponse> {
    let data = ApiData {
        binance_spot: cache.binance_spot.read().await.to_response(),
        binance_future: cache.binance_future.read().await.to_response(),
        bybit_spot: cache.bybit_spot.read().await.to_response(),
        bybit_future: cache.bybit_future.read().await.to_response(),
        okx_spot: cache.okx_spot.read().await.to_response(),
        okx_future: cache.okx_future.read().await.to_response(),
        gate_spot: cache.gate_spot.read().await.to_response(),
        gate_future: cache.gate_future.read().await.to_response(),
        bitget_spot: cache.bitget_spot.read().await.to_response(),
        bitget_future: cache.bitget_future.read().await.to_response(),
    };

    Json(ApiResponse { code: 0, data })
}
```

**Step 2: Update src/main.rs**

```rust
mod api;
mod cache;
mod models;

use axum::{Router, routing::get};
use cache::{Cache, SharedCache};
use std::sync::Arc;

#[tokio::main]
async fn main() {
    tracing_subscriber::init();

    let cache: SharedCache = Arc::new(Cache::new());

    let app = Router::new()
        .route("/api/exdata", get(api::get_exdata))
        .with_state(cache.clone());

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    tracing::info!("listening on 0.0.0.0:3000");
    axum::serve(listener, app).await.unwrap();
}
```

**Step 3: Verify**

Run: `cargo build`

**Step 4: Commit**

```bash
git add src/api.rs src/main.rs
git commit -m "feat: add API handler"
```

---

## Task 5: Exchange Common Infrastructure

**Files:**
- Create: `src/exchanges/mod.rs`

**Step 1: Create src/exchanges/mod.rs**

Provides:
- `now_ms()` — current timestamp in milliseconds
- `backoff_sleep()` — exponential backoff helper (1s, 2s, 4s, ... max 60s)
- `parse_f64()` — safe f64 parsing with 0.0 default
- Module declarations for each exchange

```rust
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
```

**Step 2: Add `mod exchanges;` to main.rs**

**Step 3: Commit**

```bash
git add src/exchanges/mod.rs src/main.rs
git commit -m "feat: add exchange common infrastructure"
```

---

## Task 6: Binance Exchange

**Files:**
- Create: `src/exchanges/binance.rs`
- Modify: `src/main.rs` (spawn Binance tasks)

### Binance Spot

**REST:** Not needed. Uses aggregate stream `!ticker@arr` which pushes all symbols.

**WebSocket:**
- URL: `wss://stream.binance.com:9443/ws/!ticker@arr`
- No subscribe message needed (stream name is in URL)
- Receives: Array of 24hrTicker objects every ~1 second

**Message format:**
```json
[
  {
    "e": "24hrTicker",
    "s": "BTCUSDT",
    "b": "50000.00",
    "a": "50001.00",
    "q": "1234567.89"
  }, ...
]
```

**Field mapping:**
- `s` → `name` (filter: must end with "USDT")
- `a` → `a` (best ask price, parse as f64)
- `b` → `b` (best bid price, parse as f64)
- `q` → `trade24Count` (24h quote volume, parse as f64)

**Timestamp:** Use `now_ms()` when processing the batch.

### Binance Futures

**REST (one-time on startup/reconnect):**
- `GET https://fapi.binance.com/fapi/v1/fundingInfo`
- Response: `[{ "symbol": "BTCUSDT", "adjustedFundingRateCap": "0.02", "adjustedFundingRateFloor": "-0.02", "fundingIntervalHours": 8 }]`
- Store as `HashMap<String, (u32, String)>` → (interval_hours, max_rate_percent)
- Max rate conversion: `adjustedFundingRateCap` is decimal (e.g., "0.02" = 2%). Multiply by 100 and format as "2.000".

**WebSocket:**
- URL: `wss://fstream.binance.com/stream?streams=!ticker@arr/!markPrice@arr@1s/!bookTicker`
- Combined stream format wraps each message:
  ```json
  { "stream": "!ticker@arr", "data": [...] }
  { "stream": "!bookTicker", "data": { ... } }
  { "stream": "!markPrice@arr@1s", "data": [...] }
  ```

**Three data sources, merged per symbol:**

1. `!bookTicker` → `a`, `b`
   ```json
   { "e": "bookTicker", "s": "BTCUSDT", "b": "50000.00", "a": "50001.00" }
   ```

2. `!ticker@arr` → `trade24Count`
   ```json
   [{ "e": "24hrTicker", "s": "BTCUSDT", "q": "1234567890.00" }]
   ```

3. `!markPrice@arr@1s` → `rate`, `markPrice`, `indexPrice`
   ```json
   [{ "e": "markPriceUpdate", "s": "BTCUSDT", "p": "50001.00", "i": "49999.50", "r": "0.00010000" }]
   ```
   - `r` is decimal funding rate. Convert: multiply by 100, format 3 decimals → "0.010"
   - `p` → `markPrice` (as string)
   - `i` → `indexPrice` (as string)

**Field mapping (merged from 3 sources):**
- `rateInterval` and `rateMax` from REST funding info HashMap
- `a`, `b` from bookTicker
- `trade24Count` from ticker (quote volume `q`)
- `rate`, `markPrice`, `indexPrice` from markPrice

**Cache update pattern:** Each WS message source updates only its fields on the existing item. Use `entry().or_insert_with()` to create items, then update specific fields.

### Implementation structure

```rust
// src/exchanges/binance.rs

use crate::cache::SharedCache;
use crate::exchanges::{now_ms, backoff_sleep, parse_f64};
use crate::models::ExchangeItem;
use futures_util::{StreamExt, SinkExt};
use serde::Deserialize;
use std::collections::HashMap;
use tokio_tungstenite::{connect_async, tungstenite::Message};

// --- Serde structs for Binance responses ---

#[derive(Deserialize)]
struct SpotTicker {
    s: String,
    a: String,
    b: String,
    q: String,
}

#[derive(Deserialize)]
struct CombinedMsg {
    stream: String,
    data: serde_json::Value,
}

#[derive(Deserialize)]
struct FuturesBookTicker {
    s: String,
    a: String,
    b: String,
}

#[derive(Deserialize)]
struct FuturesTicker {
    s: String,
    q: String,
}

#[derive(Deserialize)]
struct MarkPriceUpdate {
    s: String,
    p: String,  // mark price
    i: String,  // index price
    r: String,  // funding rate
}

#[derive(Deserialize)]
struct FundingInfoItem {
    symbol: String,
    #[serde(rename = "adjustedFundingRateCap")]
    adjusted_funding_rate_cap: String,
    #[serde(rename = "fundingIntervalHours")]
    funding_interval_hours: u32,
}

// --- Public entry points ---

pub async fn run_spot(cache: SharedCache) { /* outer reconnect loop */ }
pub async fn run_future(cache: SharedCache, client: reqwest::Client) { /* outer reconnect loop */ }
```

**Spot run loop:**
1. Connect WS to `wss://stream.binance.com:9443/ws/!ticker@arr`
2. Read messages in loop
3. Parse as `Vec<SpotTicker>`, filter `s.ends_with("USDT")`
4. Lock `cache.binance_spot`, update all items, set `ts = now_ms()`
5. On disconnect, exponential backoff, reconnect

**Future run loop:**
1. REST fetch funding info → `HashMap<String, (u32, String)>`
2. Connect WS to combined stream URL
3. Read messages, dispatch by `stream` field:
   - `"!bookTicker"` → parse as `FuturesBookTicker`, update `a`/`b`
   - `"!ticker@arr"` → parse as `Vec<FuturesTicker>`, update `trade24Count`
   - `"!markPrice@arr@1s"` → parse as `Vec<MarkPriceUpdate>`, update `rate`/`markPrice`/`indexPrice`
4. For each item, also set `rateInterval` and `rateMax` from funding info map
5. On disconnect, backoff, reconnect (re-fetch funding info too)

**Step: Commit**

```bash
git add src/exchanges/binance.rs
git commit -m "feat: add Binance spot and future exchange"
```

---

## Task 7: Bybit Exchange

**Files:**
- Create: `src/exchanges/bybit.rs`

### Bybit Spot

**REST:**
- `GET https://api.bybit.com/v5/market/instruments-info?category=spot&limit=1000`
- Response: `{ "result": { "list": [{ "symbol": "BTCUSDT", "status": "Trading", "quoteCoin": "USDT" }] } }`
- May need pagination if `nextPageCursor` is present. Repeat with `&cursor=...` until no more.
- Filter: `quoteCoin == "USDT"` and `status == "Trading"`

**WebSocket:**
- URL: `wss://stream.bybit.com/v5/public/spot`
- Subscribe: `{ "op": "subscribe", "args": ["tickers.BTCUSDT", "tickers.ETHUSDT", ...] }`
- Max ~10 args per subscribe message; send multiple messages.
- **Heartbeat:** Send `{ "op": "ping" }` every 20 seconds. Response: `{ "op": "pong", ... }`

**Message format:**
```json
{
  "topic": "tickers.BTCUSDT",
  "type": "snapshot",
  "data": {
    "symbol": "BTCUSDT",
    "bid1Price": "50000.00",
    "ask1Price": "50001.00",
    "turnover24h": "1234567890.00"
  },
  "ts": 1672515782136
}
```

**Field mapping:**
- `data.symbol` → `name`
- `data.ask1Price` → `a`
- `data.bid1Price` → `b`
- `data.turnover24h` → `trade24Count`
- `ts` → section `ts`

### Bybit Futures (Linear)

**REST:**
- `GET https://api.bybit.com/v5/market/instruments-info?category=linear&limit=1000`
- Filter: `quoteCoin == "USDT"` and `contractType == "LinearPerpetual"` and `status == "Trading"`
- Extract `fundingInterval` (in minutes, divide by 60 for hours)
- **Note:** Need pagination with `cursor` if more than 1000 symbols.

**WebSocket:**
- URL: `wss://stream.bybit.com/v5/public/linear`
- Subscribe: `{ "op": "subscribe", "args": ["tickers.BTCUSDT", ...] }`
- Heartbeat: Same as spot.

**Message format:**
```json
{
  "topic": "tickers.BTCUSDT",
  "type": "snapshot",
  "data": {
    "symbol": "BTCUSDT",
    "bid1Price": "50000.00",
    "ask1Price": "50001.00",
    "turnover24h": "1234567890.00",
    "fundingRate": "0.0001",
    "markPrice": "50000.50",
    "indexPrice": "49999.80"
  },
  "ts": 1672515782136
}
```

**Field mapping:**
- `data.ask1Price` → `a`
- `data.bid1Price` → `b`
- `data.turnover24h` → `trade24Count`
- `data.fundingRate` → `rate` (multiply by 100, format 3 decimals)
- `data.markPrice` → `markPrice`
- `data.indexPrice` → `indexPrice`
- `rateInterval` from REST instruments info
- `rateMax` — Bybit may not provide max funding rate. Use `"--"` if not available, or check if REST instruments endpoint includes it.

**Step: Commit**

```bash
git add src/exchanges/bybit.rs
git commit -m "feat: add Bybit spot and future exchange"
```

---

## Task 8: OKX Exchange

**Files:**
- Create: `src/exchanges/okx.rs`

### OKX Spot

**REST:**
- `GET https://www.okx.com/api/v5/public/instruments?instType=SPOT`
- Response: `{ "data": [{ "instId": "BTC-USDT", "state": "live", "quoteCcy": "USDT" }] }`
- Filter: `quoteCcy == "USDT"` and `state == "live"`
- Symbol normalization: `"BTC-USDT"` → `"BTCUSDT"` (remove hyphens)

**WebSocket:**
- URL: `wss://ws.okx.com:8443/ws/v5/public`
- Subscribe:
  ```json
  { "op": "subscribe", "args": [{ "channel": "tickers", "instId": "BTC-USDT" }, ...] }
  ```
- **Heartbeat:** Send `"ping"` text every 25 seconds. Response: `"pong"`

**Message format:**
```json
{
  "arg": { "channel": "tickers", "instId": "BTC-USDT" },
  "data": [{
    "instId": "BTC-USDT",
    "askPx": "50001.00",
    "bidPx": "50000.00",
    "volCcy24h": "1234567890.00"
  }]
}
```

**Field mapping:**
- `data[0].instId` → `name` (normalize: remove hyphens)
- `data[0].askPx` → `a`
- `data[0].bidPx` → `b`
- `data[0].volCcy24h` → `trade24Count` (this is quote volume)

### OKX Futures (Swap)

**REST:**
- `GET https://www.okx.com/api/v5/public/instruments?instType=SWAP`
- Filter: `settleCcy == "USDT"` and `ctType == "linear"` and `state == "live"`
- Symbol normalization: `"BTC-USDT-SWAP"` → `"BTCUSDT"` (remove hyphens and "-SWAP")

**WebSocket:** Same URL as spot. Subscribe to 4 channels per instrument:

1. **tickers** → `a`, `b`, `trade24Count`
   ```json
   { "op": "subscribe", "args": [{ "channel": "tickers", "instId": "BTC-USDT-SWAP" }] }
   ```
   Response `data[0]` fields: `askPx`, `bidPx`, `volCcy24h`

2. **funding-rate** → `rate`
   ```json
   { "op": "subscribe", "args": [{ "channel": "funding-rate", "instId": "BTC-USDT-SWAP" }] }
   ```
   Response `data[0]` fields: `fundingRate` (decimal, multiply by 100)

3. **mark-price** → `markPrice`
   ```json
   { "op": "subscribe", "args": [{ "channel": "mark-price", "instId": "BTC-USDT-SWAP" }] }
   ```
   Response `data[0]` fields: `markPx`

4. **index-tickers** → `indexPrice`
   ```json
   { "op": "subscribe", "args": [{ "channel": "index-tickers", "instId": "BTC-USDT" }] }
   ```
   Note: index-tickers uses `"BTC-USDT"` (NOT `"BTC-USDT-SWAP"`).
   Response `data[0]` fields: `idxPx`

**Funding interval and max rate:** Not available via WS or standard REST endpoint. Hardcode common values or use `"--"` as default. Most OKX swaps use 8h interval. Alternatively, check if `GET /api/v5/public/funding-rate?instId=XXX-USDT-SWAP` provides interval info.

**Step: Commit**

```bash
git add src/exchanges/okx.rs
git commit -m "feat: add OKX spot and future exchange"
```

---

## Task 9: Gate.io Exchange

**Files:**
- Create: `src/exchanges/gate.rs`

### Gate.io Spot

**REST:**
- `GET https://api.gateio.ws/api/v4/spot/currency_pairs`
- Response: `[{ "id": "BTC_USDT", "base": "BTC", "quote": "USDT", "trade_status": "tradable" }]`
- Filter: `quote == "USDT"` and `trade_status == "tradable"`
- Symbol normalization: `"BTC_USDT"` → `"BTCUSDT"` (remove underscore)

**WebSocket:**
- URL: `wss://api.gateio.ws/ws/v4/`
- Subscribe:
  ```json
  { "time": 1234567890, "channel": "spot.tickers", "event": "subscribe", "payload": ["BTC_USDT", "ETH_USDT", ...] }
  ```
- Heartbeat: Server-side pings, client responds with pong (handled by tungstenite).

**Message format:**
```json
{
  "time": 1234567890,
  "channel": "spot.tickers",
  "event": "update",
  "result": {
    "currency_pair": "BTC_USDT",
    "lowest_ask": "50001.00",
    "highest_bid": "50000.00",
    "quote_volume": "1234567890.00"
  }
}
```

**Field mapping:**
- `result.currency_pair` → `name` (normalize: remove underscore)
- `result.lowest_ask` → `a`
- `result.highest_bid` → `b`
- `result.quote_volume` → `trade24Count`

### Gate.io Futures

**REST:**
- `GET https://api.gateio.ws/api/v4/futures/usdt/contracts`
- Response: `[{ "name": "BTC_USDT", "in_delisting": false, "funding_rate_indicative": "0.0001", "mark_price_round": "0.01", "funding_interval": 28800, "funding_cap": "0.003", ... }]`
- Filter: NOT `in_delisting`
- `funding_interval` is in seconds. Divide by 3600 for hours.
- `funding_cap` is the max funding rate (decimal). Multiply by 100 for percent.
- Symbol normalization: `"BTC_USDT"` → `"BTCUSDT"`

**WebSocket:**
- URL: `wss://fx-ws.gateio.ws/v4/ws/usdt`
- Subscribe to 2 channels:

1. **futures.tickers** → `trade24Count`, `rate`, `markPrice`, `indexPrice`
   ```json
   { "time": 1234567890, "channel": "futures.tickers", "event": "subscribe", "payload": ["BTC_USDT", "ETH_USDT"] }
   ```
   Response:
   ```json
   {
     "channel": "futures.tickers",
     "event": "update",
     "result": [{
       "contract": "BTC_USDT",
       "funding_rate": "0.000100",
       "mark_price": "50000.50",
       "index_price": "49999.80",
       "quanto_base_rate": "",
       "volume_24h_quote": "1234567890"
     }]
   }
   ```

2. **futures.book_ticker** → `a`, `b`
   ```json
   { "time": 1234567890, "channel": "futures.book_ticker", "event": "subscribe", "payload": ["BTC_USDT", "ETH_USDT"] }
   ```
   Response:
   ```json
   {
     "channel": "futures.book_ticker",
     "event": "update",
     "result": {
       "t": 1672515782136,
       "contract": "BTC_USDT",
       "b": "50000.00",
       "B": 10,
       "a": "50001.00",
       "A": 5
     }
   }
   ```

**Field mapping (merged):**
- `result.contract` / `result[0].contract` → `name` (normalize)
- `a`, `b` from book_ticker
- `volume_24h_quote` → `trade24Count`
- `funding_rate` → `rate` (multiply by 100)
- `mark_price` → `markPrice`
- `index_price` → `indexPrice`
- `rateInterval` and `rateMax` from REST contracts data

**Step: Commit**

```bash
git add src/exchanges/gate.rs
git commit -m "feat: add Gate.io spot and future exchange"
```

---

## Task 10: Bitget Exchange

**Files:**
- Create: `src/exchanges/bitget.rs`

### Bitget Spot

**REST:**
- `GET https://api.bitget.com/api/v2/spot/public/symbols`
- Response: `{ "data": [{ "symbol": "BTCUSDT", "status": "online", "quoteCoin": "USDT" }] }`
- Filter: `quoteCoin == "USDT"` and `status == "online"`

**WebSocket:**
- URL: `wss://ws.bitget.com/v2/ws/public`
- Subscribe:
  ```json
  { "op": "subscribe", "args": [{ "instType": "SPOT", "channel": "ticker", "instId": "BTCUSDT" }, ...] }
  ```
- **Heartbeat:** Send `"ping"` text every 30 seconds. Response: `"pong"`

**Message format:**
```json
{
  "action": "snapshot",
  "arg": { "instType": "SPOT", "channel": "ticker", "instId": "BTCUSDT" },
  "data": [{
    "instId": "BTCUSDT",
    "askPr": "50001.00",
    "bidPr": "50000.00",
    "baseVolume": "1234.56",
    "quoteVolume": "61728000.00"
  }]
}
```

**Field mapping:**
- `data[0].instId` → `name`
- `data[0].askPr` → `a`
- `data[0].bidPr` → `b`
- `data[0].quoteVolume` → `trade24Count`

### Bitget Futures

**REST:**
- `GET https://api.bitget.com/api/v2/mix/market/contracts?productType=USDT-FUTURES`
- Response: `{ "data": [{ "symbol": "BTCUSDT", "status": "normal", "quoteCoin": "USDT", "fundingIntervalHours": "8", "maxFundingRate": "0.003" }] }`
- Filter: `status == "normal"`
- `fundingIntervalHours` → parse as u32 → `rateInterval`
- `maxFundingRate` → multiply by 100 → `rateMax`

**WebSocket:** Same URL as spot.
- Subscribe:
  ```json
  { "op": "subscribe", "args": [{ "instType": "USDT-FUTURES", "channel": "ticker", "instId": "BTCUSDT" }, ...] }
  ```

**Message format:**
```json
{
  "action": "snapshot",
  "arg": { "instType": "USDT-FUTURES", "channel": "ticker", "instId": "BTCUSDT" },
  "data": [{
    "instId": "BTCUSDT",
    "askPr": "50001.00",
    "bidPr": "50000.00",
    "baseVolume": "1234.56",
    "quoteVolume": "61728000.00",
    "fundingRate": "0.0001",
    "nextFundingTime": "1672560000000",
    "markPrice": "50000.50",
    "indexPrice": "49999.80"
  }]
}
```

**Field mapping:**
- `data[0].instId` → `name`
- `data[0].askPr` → `a`
- `data[0].bidPr` → `b`
- `data[0].quoteVolume` → `trade24Count`
- `data[0].fundingRate` → `rate` (multiply by 100, format 3 decimals)
- `data[0].markPrice` → `markPrice`
- `data[0].indexPrice` → `indexPrice`
- `rateInterval` and `rateMax` from REST contracts data

**Step: Commit**

```bash
git add src/exchanges/bitget.rs
git commit -m "feat: add Bitget spot and future exchange"
```

---

## Task 11: Main Integration

**Files:**
- Modify: `src/main.rs`

**Step 1: Wire all exchanges in main.rs**

```rust
mod api;
mod cache;
mod exchanges;
mod models;

use axum::{Router, routing::get};
use cache::{Cache, SharedCache};
use std::sync::Arc;

#[tokio::main]
async fn main() {
    tracing_subscriber::init();

    let cache: SharedCache = Arc::new(Cache::new());
    let client = reqwest::Client::new();

    // Spawn exchange tasks (10 total)
    tokio::spawn(exchanges::binance::run_spot(cache.clone()));
    tokio::spawn(exchanges::binance::run_future(cache.clone(), client.clone()));
    tokio::spawn(exchanges::bybit::run_spot(cache.clone(), client.clone()));
    tokio::spawn(exchanges::bybit::run_future(cache.clone(), client.clone()));
    tokio::spawn(exchanges::okx::run_spot(cache.clone(), client.clone()));
    tokio::spawn(exchanges::okx::run_future(cache.clone(), client.clone()));
    tokio::spawn(exchanges::gate::run_spot(cache.clone(), client.clone()));
    tokio::spawn(exchanges::gate::run_future(cache.clone(), client.clone()));
    tokio::spawn(exchanges::bitget::run_spot(cache.clone(), client.clone()));
    tokio::spawn(exchanges::bitget::run_future(cache.clone(), client.clone()));

    let app = Router::new()
        .route("/api/exdata", get(api::get_exdata))
        .with_state(cache);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    tracing::info!("listening on 0.0.0.0:3000");
    axum::serve(listener, app).await.unwrap();
}
```

**Step 2: Verify**

Run: `cargo build`

**Step 3: Commit**

```bash
git add src/main.rs
git commit -m "feat: wire all exchanges in main"
```

---

## Task 12: Build, Run, Smoke Test

**Step 1: Build release**

Run: `cargo build --release`

**Step 2: Run the service**

Run: `cargo run`

**Step 3: Test endpoint**

Run: `curl http://localhost:3000/api/exdata | jq .`

Expected: JSON response with `code: 0` and `data` containing all 10 exchange sections. Initially lists may be empty, but should populate within a few seconds as WebSocket connections establish.

**Step 4: Commit**

```bash
git add -A
git commit -m "feat: exdata v0.1.0 complete"
```

---

## Key Implementation Notes

### Funding Rate Format
All exchanges provide funding rate as a decimal (e.g., `0.0001`). Convert to percentage string: multiply by 100, format with 3 decimal places → `"0.010"`. Use format `"{:.3}"`.

### Symbol Normalization
Output format is always uppercase without separators (e.g., `"BTCUSDT"`):
- Binance/Bybit/Bitget: Already correct format
- OKX: Remove hyphens and `-SWAP` suffix → `"BTC-USDT-SWAP"` → `"BTCUSDT"`
- Gate: Remove underscores → `"BTC_USDT"` → `"BTCUSDT"`

### WebSocket Subscription Batching
For per-symbol subscription exchanges (Bybit, OKX, Gate, Bitget):
- Batch subscribe args into groups of 50-100 per message
- Send multiple subscribe messages on the same connection
- Small delay (100ms) between batch messages to avoid rate limits

### Heartbeat/Ping
- **Binance:** Protocol-level ping/pong (automatic by tungstenite)
- **Bybit:** Send `{"op":"ping"}` every 20s via a spawned interval task
- **OKX:** Send `"ping"` text every 25s
- **Gate:** Protocol-level ping/pong (automatic)
- **Bitget:** Send `"ping"` text every 30s

For exchanges needing application-level pings, spawn a separate tokio task that sends pings on the write half of the WS connection. Use `tokio::select!` in the main read loop to handle both incoming messages and the ping interval.

### Partial Cache Updates
Futures data comes from multiple WS channels. Each channel updates only its fields:
1. Get or insert the item: `items.entry(symbol).or_insert_with(|| ExchangeItem { name: symbol.clone(), ..Default::default() })`
2. Update only the fields this channel provides
3. Set `rateInterval` and `rateMax` from the REST-fetched funding info on every update

### Error Handling
- REST failures: log with `tracing::error!`, return empty data, retry on next reconnect cycle
- WS connection failures: log, exponential backoff
- WS message parse errors: log with `tracing::warn!`, skip message, continue
- Individual exchange failures: do NOT affect other exchanges

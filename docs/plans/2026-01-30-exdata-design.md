# ExData - Exchange Real-time Data API

## Overview

A Rust service built with `tokio-rs/axum` that collects real-time Spot and Future (perpetual swap) data from 5 cryptocurrency exchanges and exposes it via a single HTTP endpoint.

**Exchanges:** Binance, Bybit, OKX, Gate.io, Bitget

**Endpoint:** `GET /api/exdata`

---

## Architecture

### Data Flow

```
[Binance WS] ──┐
[Bybit WS]   ──┤
[OKX WS]     ──┼──► Arc<RwLock<Cache>> ◄── GET /api/exdata (read)
[Gate WS]    ──┤
[Bitget WS]  ──┘
```

- 10 parallel tokio tasks (5 exchanges x 2 types)
- Each task: REST API to get USDT pairs list → WebSocket subscribe → continuously update in-memory cache
- HTTP handler reads cache directly, no polling interval
- Exponential backoff reconnection on disconnect (1s, 2s, 4s, ... max 60s)

### Project Structure

```
src/
├── main.rs                 # Entry: start Axum + spawn exchange tasks
├── cache.rs                # Shared cache structure
├── models.rs               # SpotItem, FutureItem, ApiResponse
├── api.rs                  # HTTP handler
└── exchanges/
    ├── mod.rs              # Exchange trait + common logic
    ├── binance.rs
    ├── bybit.rs
    ├── okx.rs
    ├── gate.rs
    └── bitget.rs
```

---

## Data Models

### SpotItem

| Field | JSON Key | Type | Description |
|-------|----------|------|-------------|
| name | `name` | String | Symbol, e.g. "BTCUSDT" |
| a | `a` | f64 | Best ask price |
| b | `b` | f64 | Best bid price |
| trade24_count | `trade24Count` | f64 | 24h trading volume |

### FutureItem

| Field | JSON Key | Type | Description |
|-------|----------|------|-------------|
| name | `name` | String | Symbol |
| a | `a` | f64 | Best ask price |
| b | `b` | f64 | Best bid price |
| rate_interval | `rateInterval` | u32 | Funding rate interval (hours) |
| trade24_count | `trade24Count` | f64 | 24h trading volume |
| rate | `rate` | String | Current funding rate |
| rate_max | `rateMax` | String | Max funding rate |
| index_price | `indexPrice` | String | Index price |
| mark_price | `markPrice` | String | Mark price |

### Cache

Each exchange section contains `ts` (timestamp ms) and `list` (Vec of items). 10 sections total:

`binanceSpot`, `binanceFuture`, `bybitSpot`, `bybitFuture`, `okxSpot`, `okxFuture`, `gateSpot`, `gateFuture`, `bitgetSpot`, `bitgetFuture`

Stored as `Arc<RwLock<Cache>>` in heap memory. No persistence.

---

## WebSocket Subscriptions

### Binance

**Spot** (1 channel)
- URL: `wss://stream.binance.com:9443/ws/`
- Channel: `<symbol>@ticker` — provides ask, bid, 24h volume

**Futures** (3 channels)
- URL: `wss://fstream.binance.com/ws/`
- `<symbol>@bookTicker` — ask, bid
- `<symbol>@ticker` — 24h volume
- `<symbol>@markPrice@1s` — funding rate, mark price, index price
- REST: `GET /fapi/v1/fundingInfo` — funding interval, max funding rate

### Bybit

**Spot** (2 channels)
- URL: `wss://stream.bybit.com/v5/public/spot`
- `tickers.<symbol>` — volume
- `orderbook.1.<symbol>` — ask, bid

**Futures / Linear** (1 channel)
- URL: `wss://stream.bybit.com/v5/public/linear`
- `tickers.<symbol>` — ALL data (ask, bid, volume, funding rate, interval, cap, mark price, index price)

### OKX

**Spot** (1 channel)
- URL: `wss://ws.okx.com:8443/ws/v5/public`
- `tickers` (instId: `XXX-USDT`) — ask, bid, volume

**Futures / Swap** (4 channels)
- URL: `wss://ws.okx.com:8443/ws/v5/public`
- `tickers` (instId: `XXX-USDT-SWAP`) — ask, bid, volume
- `funding-rate` — funding rate
- `mark-price` — mark price
- `index-tickers` — index price
- REST: funding interval, max funding rate

### Gate.io

**Spot** (1 channel)
- URL: `wss://api.gateio.ws/ws/v4/`
- `spot.tickers` — ask, bid, volume

**Futures** (2 channels)
- URL: `wss://fx-ws.gateio.ws/v4/ws/usdt`
- `futures.tickers` — funding rate, mark price, index price, volume
- `futures.book_ticker` — ask, bid
- REST: funding interval, max funding rate

### Bitget

**Spot** (1 channel)
- URL: `wss://ws.bitget.com/v2/ws/public`
- `ticker` (instType: SPOT) — ask, bid, volume

**Futures** (1 channel)
- URL: `wss://ws.bitget.com/v2/ws/public`
- `ticker` (instType: USDT-FUTURES) — ask, bid, funding rate, mark price, index price, volume
- REST: funding interval, max funding rate

---

## Task Lifecycle

```
loop {
    1. REST: fetch USDT pairs list + funding info (futures only)
    2. Connect WebSocket
    3. Subscribe to all channels
    4. loop {
         receive message → parse → update Cache
         disconnect → break inner loop
       }
    5. Exponential backoff wait (1s, 2s, 4s, ... max 60s)
    6. Go to step 1 (re-fetch pairs list, may have changed)
}
```

### Error Handling

- REST failure → log error, exponential backoff retry
- WS connection failure → same
- WS parse error (single message) → log warning, skip, don't disconnect
- An exchange going down entirely → doesn't affect others, cache retains last valid data

---

## Dependencies

```toml
axum = "0.8"
tokio = { version = "1", features = ["full"] }
tokio-tungstenite = { version = "0.26", features = ["native-tls"] }
reqwest = { version = "0.12", features = ["json"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tracing = "0.1"
tracing-subscriber = "0.3"
```

---

## API Response Format

```json
{
    "code": 0,
    "data": {
        "binanceSpot": { "ts": 1769742482550, "list": [...] },
        "binanceFuture": { "ts": 1769742483102, "list": [...] },
        "bybitSpot": { "ts": 1769742481973, "list": [...] },
        "bybitFuture": { "ts": 1769742482165, "list": [...] },
        "okxSpot": { "ts": 1769742481516, "list": [...] },
        "okxFuture": { "ts": 1769742481502, "list": [...] },
        "gateSpot": { "ts": 1769742483186, "list": [...] },
        "gateFuture": { "ts": 1769742481144, "list": [...] },
        "bitgetSpot": { "ts": 1769742482196, "list": [...] },
        "bitgetFuture": { "ts": 1769742482205, "list": [...] }
    }
}
```

Spot items: `{ name, a, b, trade24Count }`
Future items: `{ name, a, b, rateInterval, trade24Count, rate, rateMax, indexPrice, markPrice }`

Some exchanges may include extra empty fields in spot items (e.g. bitgetSpot includes `rateMax: "--"`, `indexPrice: ""`, `markPrice: ""`).

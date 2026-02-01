# `/ws/spreads` WebSocket Endpoint Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add a `/ws/spreads` WebSocket endpoint to the Pulse API that pushes real-time spread opportunities to downstream clients.

**Architecture:** Exchange WS tasks fire `TickerChanged` events via `tokio::sync::broadcast`. A dedicated spread calculator task receives events, reads cache with read locks, computes cross-exchange spreads, and broadcasts results to all connected WS clients via a second broadcast channel. The WS handler sends snapshot on connect, then streams updates with tiered throttling.

**Tech Stack:** Rust, Axum 0.8 (built-in WebSocket support via `axum::extract::ws`), `tokio::sync::broadcast`

**Design doc:** `docs/plans/2026-01-31-pulse-websocket-realtime-spread-design.md`

---

## Codebase Context

```
src/
├── main.rs          — entry point, spawns exchange tasks + HTTP server
├── api.rs           — GET /api/exdata handler
├── cache.rs         — SharedCache = Arc<Cache>, 6 RwLock<ExchangeSection>
├── models.rs        — ExchangeItem, ExchangeSection (with dirty flag)
├── config.rs        — config loading
├── validator.rs     — baseline validator
└── exchanges/
    ├── mod.rs        — now_ms(), parse_f64(), backoff_sleep()
    ├── binance.rs    — sets section.dirty = true after item update
    ├── bybit.rs      — same pattern
    ├── okx.rs        — same pattern
    ├── gate.rs       — same pattern
    ├── bitget.rs     — same pattern
    └── zoomex.rs     — same pattern
```

Key types:
- `SharedCache = Arc<Cache>` — 6 `RwLock<ExchangeSection>` fields
- `ExchangeItem` — has `name: String`, `ts: u64`, `a: f64` (ask), `b: f64` (bid), `trade24_count: f64`
- Exchange names in cache fields: `binance_future`, `bybit_future`, `okx_future`, `gate_future`, `bitget_future`, `zoomex_future`

Current Cargo deps include `axum = "0.8"` (has built-in WS support) and `tokio` with full features.

---

## Task 1: Add `spread` module with data types and calculation logic

**Files:**
- Create: `src/spread.rs`
- Modify: `src/main.rs` (add `mod spread;`)

### Step 1: Create `src/spread.rs` with types and spread calculation

This module contains:
1. `ExchangeName` enum with 6 variants
2. `TickerChanged` event struct (for broadcast channel)
3. `SpreadOpportunity` struct (serializable to JSON)
4. `SpreadUpdate` enum (Snapshot / Update) for WS messages
5. `compute_spreads_for_symbol()` — core calculation logic
6. Exchange name constants and helpers

```rust
use serde::Serialize;
use crate::cache::SharedCache;
use crate::exchanges::now_ms;

// ── Exchange names ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
            Self::Binance => "binance",
            Self::Bybit => "bybit",
            Self::Okx => "okx",
            Self::Gate => "gate",
            Self::Bitget => "bitget",
            Self::Zoomex => "zoomex",
        }
    }
}

pub const ALL_EXCHANGES: [ExchangeName; 6] = [
    ExchangeName::Binance,
    ExchangeName::Bybit,
    ExchangeName::Okx,
    ExchangeName::Gate,
    ExchangeName::Bitget,
    ExchangeName::Zoomex,
];

// ── Event types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct TickerChanged {
    pub exchange: ExchangeName,
    pub symbol: String,
}

// ── Spread types ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
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

#[derive(Debug, Clone, Serialize)]
pub struct Volume24h {
    pub long: f64,
    pub short: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SpreadMessage {
    Snapshot { data: Vec<SpreadOpportunity> },
    Update { data: SpreadOpportunity },
}

// ── Spread calculation ──────────────────────────────────────────────────────

/// Per-exchange ticker data for one symbol, extracted from cache under read lock.
pub struct SymbolTicker {
    pub exchange: ExchangeName,
    pub ask: f64,
    pub bid: f64,
    pub ts: u64,
    pub volume_24h: f64,
}

/// Read all tickers for a given symbol across all exchanges from cache.
/// Filters out stale (> 10s) or zero-ts entries.
pub async fn read_symbol_tickers(
    cache: &SharedCache,
    symbol: &str,
) -> Vec<SymbolTicker> {
    let now = now_ms();
    let staleness_threshold = 10_000; // 10 seconds

    let sections = [
        (ExchangeName::Binance, &cache.binance_future),
        (ExchangeName::Bybit, &cache.bybit_future),
        (ExchangeName::Okx, &cache.okx_future),
        (ExchangeName::Gate, &cache.gate_future),
        (ExchangeName::Bitget, &cache.bitget_future),
        (ExchangeName::Zoomex, &cache.zoomex_future),
    ];

    let mut tickers = Vec::new();
    for (exchange, lock) in &sections {
        let section = lock.read().await;
        if let Some(item) = section.items.get(symbol) {
            if item.ts == 0 || now.saturating_sub(item.ts) > staleness_threshold {
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

/// Compute all spread opportunities for a symbol given its tickers across exchanges.
/// Returns Vec of opportunities (both directions for each pair, only the better one).
pub fn compute_spreads(
    symbol: &str,
    tickers: &[SymbolTicker],
) -> Vec<SpreadOpportunity> {
    let mut results = Vec::new();

    for i in 0..tickers.len() {
        for j in (i + 1)..tickers.len() {
            let t1 = &tickers[i];
            let t2 = &tickers[j];

            // Price ratio check: skip if > 1.5x (different contract specs)
            let ratio = if t1.ask > t2.ask { t1.ask / t2.ask } else { t2.ask / t1.ask };
            if ratio > 1.5 {
                continue;
            }

            // Direction 1: long t1 (buy at t1.ask), short t2 (sell at t2.bid)
            let mid1 = (t1.ask + t2.bid) / 2.0;
            let spread1 = if mid1 > 0.0 { (t2.bid - t1.ask) / mid1 * 100.0 } else { 0.0 };

            // Direction 2: long t2 (buy at t2.ask), short t1 (sell at t1.bid)
            let mid2 = (t2.ask + t1.bid) / 2.0;
            let spread2 = if mid2 > 0.0 { (t1.bid - t2.ask) / mid2 * 100.0 } else { 0.0 };

            // Pick the better direction
            let (long, short, spread) = if spread1 >= spread2 {
                (t1, t2, spread1)
            } else {
                (t2, t1, spread2)
            };

            // Filter anomalous spreads
            if spread > 20.0 || spread < -20.0 {
                continue;
            }

            results.push(SpreadOpportunity {
                symbol: symbol.to_string(),
                long_exchange: long.exchange.as_str(),
                short_exchange: short.exchange.as_str(),
                long_ask: long.ask,
                short_bid: short.bid,
                spread_percent: (spread * 100.0).round() / 100.0, // 2 decimal places
                volume_24h: Volume24h {
                    long: long.volume_24h,
                    short: short.volume_24h,
                },
                ts: std::cmp::max(long.ts, short.ts),
            });
        }
    }
    results
}

/// Collect all symbols that exist across any exchange. Returns a deduplicated Vec.
pub async fn collect_all_symbols(cache: &SharedCache) -> Vec<String> {
    let mut symbols = std::collections::HashSet::new();
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
    symbols.into_iter().collect()
}
```

### Step 2: Add `mod spread;` to `src/main.rs`

Add after the existing `mod validator;` line:

```rust
mod spread;
```

### Step 3: Build to verify

Run: `cargo build`
Expected: compiles with no errors (spread module is defined but not yet wired)

### Step 4: Commit

```bash
git add src/spread.rs src/main.rs
git commit -m "feat: add spread module with types and calculation logic"
```

---

## Task 2: Add TickerChanged broadcast to exchange tasks

**Files:**
- Modify: `src/cache.rs` — add broadcast sender to Cache
- Modify: `src/main.rs` — create broadcast channel, pass to Cache
- Modify: `src/exchanges/binance.rs` — send TickerChanged after item update
- Modify: `src/exchanges/bybit.rs` — same
- Modify: `src/exchanges/okx.rs` — same
- Modify: `src/exchanges/gate.rs` — same
- Modify: `src/exchanges/bitget.rs` — same
- Modify: `src/exchanges/zoomex.rs` — same

### Step 1: Add broadcast sender to Cache

In `src/cache.rs`, add:

```rust
use crate::models::ExchangeSection;
use crate::spread::TickerChanged;
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast};

pub struct Cache {
    pub binance_future: RwLock<ExchangeSection>,
    pub bybit_future: RwLock<ExchangeSection>,
    pub okx_future: RwLock<ExchangeSection>,
    pub gate_future: RwLock<ExchangeSection>,
    pub bitget_future: RwLock<ExchangeSection>,
    pub zoomex_future: RwLock<ExchangeSection>,
    pub ticker_tx: broadcast::Sender<TickerChanged>,
}

impl Cache {
    pub fn new(ticker_tx: broadcast::Sender<TickerChanged>) -> Self {
        Self {
            binance_future: RwLock::new(ExchangeSection::new()),
            bybit_future: RwLock::new(ExchangeSection::new()),
            okx_future: RwLock::new(ExchangeSection::new()),
            gate_future: RwLock::new(ExchangeSection::new()),
            bitget_future: RwLock::new(ExchangeSection::new()),
            zoomex_future: RwLock::new(ExchangeSection::new()),
            ticker_tx,
        }
    }
}

pub type SharedCache = Arc<Cache>;
```

### Step 2: Update `src/main.rs` to create broadcast channel

Replace:
```rust
let cache: SharedCache = Arc::new(Cache::new());
```

With:
```rust
let (ticker_tx, _) = tokio::sync::broadcast::channel::<spread::TickerChanged>(4096);
let cache: SharedCache = Arc::new(Cache::new(ticker_tx));
```

### Step 3: Add broadcast sends to each exchange

In each exchange's WS message handler, **after** `section.dirty = true;` and **after dropping the write lock** (by ending the block or letting `section` go out of scope), send a TickerChanged event. The send is fire-and-forget — if no receivers, the message is dropped.

The pattern for each exchange (example for binance bookTicker in `src/exchanges/binance.rs`):

For each place where `section.dirty = true;` is set in a block that updates bid/ask (the price-relevant updates), add after the write lock is released:

```rust
// After the write lock scope ends:
let _ = cache.ticker_tx.send(crate::spread::TickerChanged {
    exchange: crate::spread::ExchangeName::Binance,  // adjust per exchange
    symbol: bt.s.clone(),  // adjust per exchange — the symbol that changed
});
```

**Important:** Only send `TickerChanged` when **bid or ask** changes (the `!bookTicker` stream for Binance, and the ticker streams for other exchanges). Don't send for funding-rate-only or volume-only updates — those don't affect spread calculation.

**Per-exchange details:**

**binance.rs** — send after `!bookTicker` block only (line ~301 area, after `section.dirty = true`). The `!ticker@arr` and `markPrice` streams update volume/funding, not bid/ask.

Drop the section before sending:
```rust
section.dirty = true;
drop(section);
let _ = cache.ticker_tx.send(crate::spread::TickerChanged {
    exchange: crate::spread::ExchangeName::Binance,
    symbol: bt.s.clone(),
});
```

**bybit.rs** — send after the ticker update block (line ~440), but only if ask or bid was updated. The simplest approach: send unconditionally (the cost is cheap).
```rust
section.dirty = true;
drop(section);
let _ = cache.ticker_tx.send(crate::spread::TickerChanged {
    exchange: crate::spread::ExchangeName::Bybit,
    symbol: ticker.symbol.clone(),
});
```

**okx.rs** — send after the `"tickers"` channel block only (not `"funding-rate"`).
```rust
section.dirty = true;
drop(section);
let _ = cache.ticker_tx.send(crate::spread::TickerChanged {
    exchange: crate::spread::ExchangeName::Okx,
    symbol: normalized.clone(),
});
```

**gate.rs** — send after the `"futures.book_ticker"` block only (that's where bid/ask updates).
```rust
section.dirty = true;
drop(section);
let _ = cache.ticker_tx.send(crate::spread::TickerChanged {
    exchange: crate::spread::ExchangeName::Gate,
    symbol: normalized.clone(),
});
```

**bitget.rs** — send after the ticker update block.
```rust
section.dirty = true;
drop(section);
let _ = cache.ticker_tx.send(crate::spread::TickerChanged {
    exchange: crate::spread::ExchangeName::Bitget,
    symbol: inst_id.to_string(),
});
```

**zoomex.rs** — same pattern as bybit.
```rust
section.dirty = true;
drop(section);
let _ = cache.ticker_tx.send(crate::spread::TickerChanged {
    exchange: crate::spread::ExchangeName::Zoomex,
    symbol: ticker.symbol.clone(),
});
```

### Step 4: Build to verify

Run: `cargo build`
Expected: compiles with no errors

### Step 5: Commit

```bash
git add src/cache.rs src/main.rs src/exchanges/
git commit -m "feat: broadcast TickerChanged events from all exchange WS tasks"
```

---

## Task 3: Add spread calculator task

**Files:**
- Modify: `src/spread.rs` — add `run_spread_calculator()` function
- Modify: `src/main.rs` — spawn the calculator task

### Step 1: Add calculator to `src/spread.rs`

Append to end of `src/spread.rs`:

```rust
/// Spread calculator task: listens for TickerChanged events, computes spreads,
/// and broadcasts SpreadOpportunity updates to all WS clients.
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

    loop {
        let event = match ticker_rx.recv().await {
            Ok(e) => e,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!("spread calculator: lagged {} events, continuing", n);
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                tracing::error!("spread calculator: ticker channel closed");
                break;
            }
        };

        // Read tickers for the changed symbol across all exchanges
        let tickers = read_symbol_tickers(&cache, &event.symbol).await;
        if tickers.len() < 2 {
            continue; // Need at least 2 exchanges
        }

        // Compute all spread pairs for this symbol
        let opportunities = compute_spreads(&event.symbol, &tickers);
        let now = now_ms();

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
```

### Step 2: Wire up in `src/main.rs`

After the background serializer block and before the Router, add:

```rust
// Spread calculator: receives TickerChanged events, computes spreads,
// broadcasts SpreadOpportunity to WS clients.
let (spread_tx, _) = tokio::sync::broadcast::channel::<spread::SpreadOpportunity>(4096);
{
    let cache = cache.clone();
    let ticker_rx = cache.ticker_tx.subscribe();
    let spread_tx = spread_tx.clone();
    tokio::spawn(spread::run_spread_calculator(cache, ticker_rx, spread_tx));
}
```

### Step 3: Build to verify

Run: `cargo build`
Expected: compiles (spread_tx will have an "unused" warning since nothing subscribes yet — that's fine)

### Step 4: Commit

```bash
git add src/spread.rs src/main.rs
git commit -m "feat: add spread calculator task with tiered throttle"
```

---

## Task 4: Add WebSocket endpoint `/ws/spreads`

**Files:**
- Create: `src/ws.rs` — WebSocket handler
- Modify: `src/main.rs` — add route, pass spread_tx as state

### Step 1: Create `src/ws.rs`

```rust
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use std::sync::Arc;
use tokio::sync::broadcast;

use crate::cache::SharedCache;
use crate::spread::{self, SpreadMessage, SpreadOpportunity};

/// Shared state for the WS handler.
pub struct WsState {
    pub cache: SharedCache,
    pub spread_tx: broadcast::Sender<SpreadOpportunity>,
}

pub type SharedWsState = Arc<WsState>;

/// Axum handler: upgrade HTTP → WebSocket.
pub async fn ws_spreads(
    ws: WebSocketUpgrade,
    State(state): State<SharedWsState>,
) -> Response {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

async fn handle_ws(mut socket: WebSocket, state: SharedWsState) {
    tracing::info!("ws/spreads: client connected");

    // 1. Compute and send initial snapshot
    let snapshot = build_snapshot(&state.cache).await;
    let msg = SpreadMessage::Snapshot { data: snapshot };
    let json = match serde_json::to_string(&msg) {
        Ok(j) => j,
        Err(e) => {
            tracing::error!("ws/spreads: snapshot serialize error: {}", e);
            return;
        }
    };
    if socket.send(Message::Text(json.into())).await.is_err() {
        return; // Client disconnected
    }

    // 2. Subscribe to spread updates
    let mut spread_rx = state.spread_tx.subscribe();

    // 3. Stream updates with ping/pong keepalive
    let mut ping_interval = tokio::time::interval(std::time::Duration::from_secs(30));
    ping_interval.tick().await; // consume immediate tick

    let mut pong_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);

    loop {
        tokio::select! {
            // Receive spread updates and forward to client
            result = spread_rx.recv() => {
                match result {
                    Ok(opp) => {
                        let msg = SpreadMessage::Update { data: opp };
                        let json = match serde_json::to_string(&msg) {
                            Ok(j) => j,
                            Err(_) => continue,
                        };
                        if socket.send(Message::Text(json.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("ws/spreads: client lagged {} updates", n);
                        // Send fresh snapshot to re-sync
                        let snapshot = build_snapshot(&state.cache).await;
                        let msg = SpreadMessage::Snapshot { data: snapshot };
                        if let Ok(json) = serde_json::to_string(&msg) {
                            if socket.send(Message::Text(json.into())).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            // Read client messages (pong responses, close frames)
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Pong(_))) => {
                        pong_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {} // Ignore other messages
                }
            }
            // Send periodic pings
            _ = ping_interval.tick() => {
                if socket.send(Message::Ping(vec![].into())).await.is_err() {
                    break;
                }
            }
            // Disconnect if no pong received within 60s
            _ = tokio::time::sleep_until(pong_deadline) => {
                tracing::warn!("ws/spreads: client pong timeout, disconnecting");
                break;
            }
        }
    }

    tracing::info!("ws/spreads: client disconnected");
}

/// Build a full snapshot of all current spread opportunities (spread > 0).
async fn build_snapshot(cache: &SharedCache) -> Vec<SpreadOpportunity> {
    let symbols = spread::collect_all_symbols(cache).await;
    let mut all_opps = Vec::new();

    for symbol in &symbols {
        let tickers = spread::read_symbol_tickers(cache, symbol).await;
        if tickers.len() < 2 {
            continue;
        }
        let opps = spread::compute_spreads(symbol, &tickers);
        for opp in opps {
            if opp.spread_percent > 0.0 {
                all_opps.push(opp);
            }
        }
    }

    all_opps
}
```

### Step 2: Add `mod ws;` and update Router in `src/main.rs`

Add after `mod spread;`:
```rust
mod ws;
```

Replace the Router and state setup. Change from:
```rust
let app = Router::new()
    .route("/api/exdata", get(api::get_exdata))
    .layer(CompressionLayer::new())
    .with_state(cache);
```

To a dual-state approach using `axum::Router::nest` or method routing. Since Axum 0.8 requires a single state type per Router, use two routers merged:

```rust
// REST routes (state: SharedCache)
let rest_app = Router::new()
    .route("/api/exdata", get(api::get_exdata))
    .layer(CompressionLayer::new())
    .with_state(cache.clone());

// WS routes (state: SharedWsState)
let ws_state: ws::SharedWsState = Arc::new(ws::WsState {
    cache: cache.clone(),
    spread_tx: spread_tx.clone(),
});
let ws_app = Router::new()
    .route("/ws/spreads", get(ws::ws_spreads))
    .with_state(ws_state);

let app = rest_app.merge(ws_app);
```

### Step 3: Build to verify

Run: `cargo build`
Expected: compiles with no errors

### Step 4: Commit

```bash
git add src/ws.rs src/main.rs
git commit -m "feat: add /ws/spreads WebSocket endpoint with snapshot and streaming"
```

---

## Task 5: Build and smoke test

### Step 1: Build release

Run: `cargo build`
Expected: compiles with no errors or warnings (besides possibly unused spread_tx `_` receiver)

### Step 2: Manual smoke test

1. Start the server: `cargo run`
2. In another terminal, connect with websocat or similar:
   ```bash
   # Install if needed: cargo install websocat
   websocat ws://localhost:3000/ws/spreads
   ```
3. Verify:
   - Receive a `{"type":"snapshot","data":[...]}` message on connect
   - Receive `{"type":"update","data":{...}}` messages as tickers change
   - Spread values make sense (positive = real opportunity)

### Step 3: Commit all final adjustments

```bash
git add -A
git commit -m "feat: complete /ws/spreads real-time spread push implementation"
```

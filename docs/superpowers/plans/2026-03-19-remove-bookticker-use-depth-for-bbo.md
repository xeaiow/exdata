# Remove bookTicker, Use Depth for BBO — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate BBO/depth temporal misalignment by deriving `item.a`/`item.b` from depth best levels and removing all bookTicker WS subscriptions.

**Architecture:** Each exchange subscribes to 3 WS channels (tickers, bookTicker, depth). We modify the depth handler to also set `item.a`/`item.b`/`item.ts`, then remove the bookTicker channel subscription and handler entirely. The pattern is identical across all 6 exchanges.

**Tech Stack:** Rust, Tokio, tokio-tungstenite WebSocket

**Spec:** `docs/superpowers/specs/2026-03-19-remove-bookticker-use-depth-for-bbo-design.md`

---

### Task 1: Binance — depth handler sets BBO + remove bookTicker

**Files:**
- Modify: `src/exchanges/binance.rs:17-22` (remove `FuturesBookTicker` struct)
- Modify: `src/exchanges/binance.rs:245` (remove `!bookTicker` from stream URL)
- Modify: `src/exchanges/binance.rs:306-337` (remove bookTicker handler)
- Modify: `src/exchanges/binance.rs:436-449` (update depth handler)

- [ ] **Step 1: Update depth handler to set `a`/`b`/`ts` and use `entry().or_insert_with()`**

In `src/exchanges/binance.rs`, replace the depth handler block (lines 436-449):

```rust
// BEFORE:
let mut updated = false;
{
    let mut section = cache.binance_future.write().await;
    if let Some(item) = section.items.get_mut(&sym_upper) {
        if let Some(b) = bids_raw {
            item.bids = parse_levels(b);
        }
        if let Some(a) = asks_raw {
            item.asks = parse_levels(a);
        }
        item.depth_ts = ts;
        section.dirty = true;
        updated = true;
    }
}
```

```rust
// AFTER:
let mut updated = false;
{
    let mut section = cache.binance_future.write().await;
    let item = section
        .items
        .entry(sym_upper.clone())
        .or_insert_with(|| {
            let mut ei = ExchangeItem {
                name: sym_upper.clone(),
                ..Default::default()
            };
            if let Some((interval, max_rate)) = funding_info.get(&sym_upper) {
                ei.rate_interval = Some(*interval);
                ei.rate_max = Some(max_rate.clone());
            }
            ei
        });
    if let Some(b) = bids_raw {
        item.bids = parse_levels(b);
    }
    if let Some(a) = asks_raw {
        item.asks = parse_levels(a);
    }
    item.depth_ts = ts;
    // Derive BBO from depth best levels
    if let Some(best) = item.asks.first() { item.a = best.price; }
    if let Some(best) = item.bids.first() { item.b = best.price; }
    item.ts = ts;
    section.dirty = true;
    updated = true;
}
```

- [ ] **Step 2: Remove `FuturesBookTicker` struct**

Delete lines 17-22:
```rust
#[derive(Deserialize)]
struct FuturesBookTicker {
    s: String,
    a: String,
    b: String,
}
```

- [ ] **Step 3: Remove `!bookTicker` from stream URL**

Line 245, change:
```rust
// BEFORE:
"wss://fstream.binance.com/stream?streams=!ticker@arr/!markPrice@arr@1s/!bookTicker/{}",
```
```rust
// AFTER:
"wss://fstream.binance.com/stream?streams=!ticker@arr/!markPrice@arr@1s/{}",
```

- [ ] **Step 4: Remove bookTicker handler match arm**

Delete lines 306-337 (the entire `if stream == "!bookTicker" { ... }` block). Update the next arm from `} else if stream == "!ticker@arr" {` to `if stream == "!ticker@arr" {`.

- [ ] **Step 5: Build and verify**

```bash
cd /home/cloud-user/workspace/exdata && cargo build 2>&1 | tail -5
```
Expected: compiles without errors.

- [ ] **Step 6: Run tests**

```bash
cd /home/cloud-user/workspace/exdata && cargo test 2>&1 | tail -10
```
Expected: all tests pass.

- [ ] **Step 7: Commit**

```bash
cd /home/cloud-user/workspace/exdata && git add src/exchanges/binance.rs && git commit -m "refactor(binance): derive BBO from depth, remove bookTicker subscription"
```

---

### Task 2: Gate — depth handler sets BBO + remove bookTicker

**Files:**
- Modify: `src/exchanges/gate.rs:296-314` (remove book_ticker subscription)
- Modify: `src/exchanges/gate.rs:483-541` (remove book_ticker handler)
- Modify: `src/exchanges/gate.rs:581-591` (update depth handler)

- [ ] **Step 1: Update depth handler to set `a`/`b`/`ts` and use `entry().or_insert_with()`**

In `src/exchanges/gate.rs`, replace the depth handler block (lines 581-591):

```rust
// BEFORE:
if !asks.is_empty() || !bids.is_empty() {
    let mut updated = false;
    {
        let mut section = cache.gate_future.write().await;
        if let Some(item) = section.items.get_mut(&normalized) {
            item.asks = asks;
            item.bids = bids;
            item.depth_ts = now_ms();
            section.dirty = true;
            updated = true;
        }
    }
```

```rust
// AFTER:
if !asks.is_empty() || !bids.is_empty() {
    let mut updated = false;
    {
        let mut section = cache.gate_future.write().await;
        let item = section
            .items
            .entry(normalized.clone())
            .or_insert_with(|| {
                let mut ei = ExchangeItem {
                    name: normalized.clone(),
                    ..Default::default()
                };
                if let Some((interval, max_rate)) = funding_info.get(contract) {
                    ei.rate_interval = Some(*interval);
                    ei.rate_max = Some(max_rate.clone());
                }
                ei
            });
        item.asks = asks;
        item.bids = bids;
        item.depth_ts = now_ms();
        // Derive BBO from depth best levels
        if let Some(best) = item.asks.first() { item.a = best.price; }
        if let Some(best) = item.bids.first() { item.b = best.price; }
        item.ts = item.depth_ts;
        section.dirty = true;
        updated = true;
    }
```

- [ ] **Step 2: Remove bookTicker subscription block**

Delete lines 296-314 (the `// Subscribe to futures.book_ticker` block including the `if !subscribe_failed` check, the JSON construction, and the send).

- [ ] **Step 3: Remove bookTicker handler match arm**

Delete lines 483-541 (the entire `"futures.book_ticker" => { ... }` match arm).

- [ ] **Step 4: Update stale doc comment referencing book_ticker**

Line ~253: change `/// Subscribes to both futures.tickers and futures.book_ticker channels.` → `/// Subscribes to futures.tickers and futures.order_book channels.`

- [ ] **Step 5: Build and verify**

```bash
cd /home/cloud-user/workspace/exdata && cargo build 2>&1 | tail -5
```

- [ ] **Step 6: Commit**

```bash
cd /home/cloud-user/workspace/exdata && git add src/exchanges/gate.rs && git commit -m "refactor(gate): derive BBO from depth, remove book_ticker subscription"
```

---

### Task 3: OKX — depth handler sets BBO + remove bookTicker

**Files:**
- Modify: `src/exchanges/okx.rs:249-253` (remove bbo-tbt subscription arg)
- Modify: `src/exchanges/okx.rs:382-426` (remove bbo-tbt handler)
- Modify: `src/exchanges/okx.rs:566-576` (update depth handler)

- [ ] **Step 1: Update depth handler to set `a`/`b`/`ts` and use `entry().or_insert_with()`**

In `src/exchanges/okx.rs`, replace the depth handler block (lines 566-576):

```rust
// BEFORE:
if !asks.is_empty() || !bids.is_empty() {
    let mut updated = false;
    {
        let mut section = cache.okx_future.write().await;
        if let Some(item) = section.items.get_mut(&normalized) {
            item.asks = asks;
            item.bids = bids;
            item.depth_ts = now_ms();
            section.dirty = true;
            updated = true;
        }
    }
```

```rust
// AFTER:
if !asks.is_empty() || !bids.is_empty() {
    let mut updated = false;
    {
        let mut section = cache.okx_future.write().await;
        let item = section
            .items
            .entry(normalized.clone())
            .or_insert_with(|| ExchangeItem {
                name: normalized.clone(),
                ..Default::default()
            });
        item.asks = asks;
        item.bids = bids;
        item.depth_ts = now_ms();
        // Derive BBO from depth best levels
        if let Some(best) = item.asks.first() { item.a = best.price; }
        if let Some(best) = item.bids.first() { item.b = best.price; }
        item.ts = item.depth_ts;
        section.dirty = true;
        updated = true;
    }
```

- [ ] **Step 2: Remove bbo-tbt subscription arg**

In the subscription args loop (line 249-253), delete:
```rust
all_args.push(serde_json::json!({
    "channel": "bbo-tbt",
    "instId": inst_id
}));
```

- [ ] **Step 3: Remove bbo-tbt handler match arm**

Delete lines 382-426 (the entire `"bbo-tbt" => { ... }` match arm).

- [ ] **Step 4: Update stale comments referencing bbo-tbt**

Update comments that still reference the removed channel:
- Line ~429: change `// bid/ask now driven by bbo-tbt stream (not tickers)` → `// bid/ask now driven by depth (books5) stream`
- Line ~448: change `// No TickerChanged here -- BBO updates (bbo-tbt) drive spread recalc` → `// No TickerChanged here -- depth updates drive spread recalc`

- [ ] **Step 5: Build and verify**

```bash
cd /home/cloud-user/workspace/exdata && cargo build 2>&1 | tail -5
```

- [ ] **Step 6: Commit**

```bash
cd /home/cloud-user/workspace/exdata && git add src/exchanges/okx.rs && git commit -m "refactor(okx): derive BBO from depth, remove bbo-tbt subscription"
```

---

### Task 4: Bitget — depth handler sets BBO + remove bookTicker

**Files:**
- Modify: `src/exchanges/bitget.rs:367-374` (remove books1 subscription arg)
- Modify: `src/exchanges/bitget.rs:457-508` (remove books1 handler)
- Modify: `src/exchanges/bitget.rs:540-551` (update depth handler)

- [ ] **Step 1: Update depth handler to set `a`/`b`/`ts` and use `entry().or_insert_with()`**

In `src/exchanges/bitget.rs`, replace the depth handler block (lines 540-551):

```rust
// BEFORE:
if !asks.is_empty() || !bids.is_empty() {
    let mut updated = false;
    let inst_id_owned = inst_id.to_string();
    {
        let mut section = cache.bitget_future.write().await;
        if let Some(item) = section.items.get_mut(inst_id) {
            item.asks = asks;
            item.bids = bids;
            item.depth_ts = now_ms();
            section.dirty = true;
            updated = true;
        }
    }
```

```rust
// AFTER:
if !asks.is_empty() || !bids.is_empty() {
    let mut updated = false;
    let inst_id_owned = inst_id.to_string();
    {
        let mut section = cache.bitget_future.write().await;
        let item = section
            .items
            .entry(inst_id_owned.clone())
            .or_insert_with(|| {
                let mut ei = ExchangeItem {
                    name: inst_id_owned.clone(),
                    ..Default::default()
                };
                if let Some((interval, max_rate)) = funding_info.get(inst_id) {
                    ei.rate_interval = Some(*interval);
                    ei.rate_max = Some(max_rate.clone());
                }
                ei
            });
        item.asks = asks;
        item.bids = bids;
        item.depth_ts = now_ms();
        // Derive BBO from depth best levels
        if let Some(best) = item.asks.first() { item.a = best.price; }
        if let Some(best) = item.bids.first() { item.b = best.price; }
        item.ts = item.depth_ts;
        section.dirty = true;
        updated = true;
    }
```

- [ ] **Step 2: Remove books1 subscription args**

Delete lines 367-374:
```rust
// Subscribe books1 (BBO) for bid/ask updates
args.extend(symbols.iter().map(|s| {
    serde_json::json!({
        "instType": "USDT-FUTURES",
        "channel": "books1",
        "instId": s
    })
}));
```

- [ ] **Step 3: Remove books1 handler**

Delete lines 457-508 (the entire `if arg.channel == "books1" { ... }` block).

- [ ] **Step 4: Update stale comments referencing books1**

Update comments that still reference the removed channel:
- Line ~575: change `// bid/ask now driven by books1 BBO stream (not ticker)` → `// bid/ask now driven by depth (books15) stream`
- Line ~613: change `// No TickerChanged here -- BBO updates (books1) drive spread recalc` → `// No TickerChanged here -- depth updates drive spread recalc`

- [ ] **Step 5: Build and verify**

```bash
cd /home/cloud-user/workspace/exdata && cargo build 2>&1 | tail -5
```

- [ ] **Step 6: Commit**

```bash
cd /home/cloud-user/workspace/exdata && git add src/exchanges/bitget.rs && git commit -m "refactor(bitget): derive BBO from depth, remove books1 subscription"
```

---

### Task 5: Bybit — depth handler sets BBO + remove bookTicker

**Files:**
- Modify: `src/exchanges/bybit.rs:342-358` (remove orderbook.1 subscription loop)
- Modify: `src/exchanges/bybit.rs:436-486` (remove orderbook.1 handler)
- Modify: `src/exchanges/bybit.rs:522-532` (update depth handler)

- [ ] **Step 1: Update depth handler to set `a`/`b`/`ts` and use `entry().or_insert_with()`**

In `src/exchanges/bybit.rs`, replace the depth handler block (lines 522-532):

```rust
// BEFORE:
if !bids.is_empty() || !asks.is_empty() {
    let mut updated = false;
    {
        let mut section = cache.bybit_future.write().await;
        if let Some(item) = section.items.get_mut(&symbol) {
            item.bids = bids;
            item.asks = asks;
            item.depth_ts = now_ms();
            section.dirty = true;
            updated = true;
        }
    }
```

```rust
// AFTER:
if !bids.is_empty() || !asks.is_empty() {
    let mut updated = false;
    {
        let mut section = cache.bybit_future.write().await;
        let item = section
            .items
            .entry(symbol.clone())
            .or_insert_with(|| {
                let mut ei = ExchangeItem {
                    name: symbol.clone(),
                    ..Default::default()
                };
                if let Some((interval, max_rate)) = funding_info.get(&symbol) {
                    ei.rate_interval = Some(*interval);
                    ei.rate_max = Some(max_rate.clone());
                }
                ei
            });
        item.bids = bids;
        item.asks = asks;
        item.depth_ts = now_ms();
        // Derive BBO from depth best levels
        if let Some(best) = item.asks.first() { item.a = best.price; }
        if let Some(best) = item.bids.first() { item.b = best.price; }
        item.ts = item.depth_ts;
        section.dirty = true;
        updated = true;
    }
```

- [ ] **Step 2: Remove orderbook.1 subscription loop**

Delete lines 342-358 (the `// Subscribe orderbook.1 (BBO) in batches of 10` block).

- [ ] **Step 3: Remove orderbook.1 handler**

Delete lines 436-486 (the entire `if topic.starts_with("orderbook.1.") { ... }` block). Update the next `if topic.starts_with("orderbook.50.")` from `} else if` to `if` if needed.

- [ ] **Step 4: Update stale comments referencing orderbook.1**

Update comments that still reference the removed channel:
- Line ~574: change `// bid/ask now driven by orderbook.1 BBO stream (not tickers)` → `// bid/ask now driven by depth (orderbook.50) stream`
- Line ~597: change `// No TickerChanged here -- BBO updates (orderbook.1) drive spread recalc` → `// No TickerChanged here -- depth updates drive spread recalc`

- [ ] **Step 5: Build and verify**

```bash
cd /home/cloud-user/workspace/exdata && cargo build 2>&1 | tail -5
```

- [ ] **Step 6: Commit**

```bash
cd /home/cloud-user/workspace/exdata && git add src/exchanges/bybit.rs && git commit -m "refactor(bybit): derive BBO from depth, remove orderbook.1 subscription"
```

---

### Task 6: Zoomex — depth handler sets BBO + remove bookTicker

**Files:**
- Modify: `src/exchanges/zoomex.rs:344-360` (remove orderbook.1 subscription loop)
- Modify: `src/exchanges/zoomex.rs:438-488` (remove orderbook.1 handler)
- Modify: `src/exchanges/zoomex.rs:523-533` (update depth handler)

- [ ] **Step 1: Update depth handler to set `a`/`b`/`ts` and use `entry().or_insert_with()`**

In `src/exchanges/zoomex.rs`, replace the depth handler block (lines 523-533):

```rust
// BEFORE:
if !bids.is_empty() || !asks.is_empty() {
    let mut updated = false;
    {
        let mut section = cache.zoomex_future.write().await;
        if let Some(item) = section.items.get_mut(&symbol) {
            item.bids = bids;
            item.asks = asks;
            item.depth_ts = now_ms();
            section.dirty = true;
            updated = true;
        }
    }
```

```rust
// AFTER:
if !bids.is_empty() || !asks.is_empty() {
    let mut updated = false;
    {
        let mut section = cache.zoomex_future.write().await;
        let item = section
            .items
            .entry(symbol.clone())
            .or_insert_with(|| {
                let mut ei = ExchangeItem {
                    name: symbol.clone(),
                    ..Default::default()
                };
                if let Some((interval, max_rate)) = funding_info.get(&symbol) {
                    ei.rate_interval = Some(*interval);
                    ei.rate_max = Some(max_rate.clone());
                }
                ei
            });
        item.bids = bids;
        item.asks = asks;
        item.depth_ts = now_ms();
        // Derive BBO from depth best levels
        if let Some(best) = item.asks.first() { item.a = best.price; }
        if let Some(best) = item.bids.first() { item.b = best.price; }
        item.ts = item.depth_ts;
        section.dirty = true;
        updated = true;
    }
```

- [ ] **Step 2: Remove orderbook.1 subscription loop**

Delete lines 344-360 (the `// Subscribe orderbook.1 (BBO) in batches of 10` block).

- [ ] **Step 3: Remove orderbook.1 handler**

Delete lines 438-488 (the entire `if topic.starts_with("orderbook.1.") { ... }` block).

- [ ] **Step 4: Build and verify**

```bash
cd /home/cloud-user/workspace/exdata && cargo build 2>&1 | tail -5
```

- [ ] **Step 5: Commit**

```bash
cd /home/cloud-user/workspace/exdata && git add src/exchanges/zoomex.rs && git commit -m "refactor(zoomex): derive BBO from depth, remove orderbook.1 subscription"
```

---

### Task 7: Final verification

**Files:** None (verification only)

- [ ] **Step 1: Full build**

```bash
cd /home/cloud-user/workspace/exdata && cargo build 2>&1 | tail -10
```
Expected: compiles with no errors and no warnings related to bookTicker.

- [ ] **Step 2: Run all tests**

```bash
cd /home/cloud-user/workspace/exdata && cargo test 2>&1 | tail -15
```
Expected: all tests pass. The `compute_spreads` tests in `spread.rs` still pass since they operate on `SymbolTicker` structs directly and don't depend on bookTicker.

- [ ] **Step 3: Grep for leftover bookTicker references**

```bash
cd /home/cloud-user/workspace/exdata && grep -rn -i "book.ticker\|bbo-tbt\|books1\|orderbook\.1[^0-9]" src/exchanges/ --include="*.rs"
```
Expected: no matches (all bookTicker references removed). Note: `fetch_book_tickers()` in `binance.rs` is the REST seed function and must be kept.

```bash
cd /home/cloud-user/workspace/exdata && grep -rn "book_ticker\|bookTicker\|BookTicker" src/exchanges/ --include="*.rs"
```
Expected: only `fetch_book_tickers` REST function in `binance.rs` remains.

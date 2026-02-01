# WS min_spread Filter Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add a per-connection `min_spread` query parameter to `/ws/spreads` that filters which spread opportunities are pushed to each client.

**Architecture:** Filter is applied at the WebSocket connection layer in `src/ws.rs`. The spread calculator (`src/spread.rs`) is untouched — it continues broadcasting all results. Each connection maintains a `sent_pairs` HashSet for threshold-crossing detection. `build_snapshot()` gains a `min_spread` parameter.

**Tech Stack:** Rust, Axum 0.8 (Query extractor + WebSocket), Tokio broadcast, serde

---

### Task 1: Add query parameter parsing to `ws_spreads()`

**Files:**
- Modify: `src/ws.rs:1-25`

**Step 1: Add the query parameter struct and update the handler signature**

Add a `WsQuery` struct with `serde::Deserialize` and update `ws_spreads()` to extract it via `axum::extract::Query`. Pass `min_spread` into `handle_ws()`.

```rust
use axum::extract::Query;
use serde::Deserialize;

#[derive(Deserialize)]
struct WsQuery {
    #[serde(default)]
    min_spread: f64,
}

pub async fn ws_spreads(
    ws: WebSocketUpgrade,
    State(state): State<SharedWsState>,
    Query(query): Query<WsQuery>,
) -> Response {
    let min_spread = query.min_spread;
    ws.on_upgrade(move |socket| handle_ws(socket, state, min_spread))
}
```

**Step 2: Verify it compiles**

Run: `cargo check`
Expected: Warning about unused `min_spread` in `handle_ws` (since we haven't changed the signature yet), or error because `handle_ws` doesn't accept 3 args. Both are expected — we fix this in Task 2.

---

### Task 2: Update `handle_ws()` with filtering logic

**Files:**
- Modify: `src/ws.rs:27-108`

**Step 1: Update `handle_ws` signature and add `sent_pairs` tracking**

Change `handle_ws` to accept `min_spread: f64`. Add `use std::collections::HashSet;` at the top. After sending the snapshot, populate `sent_pairs` from the snapshot contents.

```rust
use std::collections::HashSet;

async fn handle_ws(mut socket: WebSocket, state: SharedWsState, min_spread: f64) {
    tracing::info!("ws/spreads: client connected (min_spread={})", min_spread);

    let mut spread_rx = state.spread_tx.subscribe();

    // Compute and send initial snapshot (filtered)
    let snapshot = build_snapshot(&state.cache, min_spread).await;

    // Track which pairs have been sent to this client
    let mut sent_pairs: HashSet<(String, &'static str, &'static str)> = HashSet::new();
    for opp in &snapshot {
        sent_pairs.insert((opp.symbol.clone(), opp.long_exchange, opp.short_exchange));
    }

    let msg = SpreadMessage::Snapshot { data: snapshot };
    // ... rest of snapshot send unchanged ...
```

**Step 2: Add filtering in the update branch**

Replace the `Ok(opp)` match arm with threshold-aware logic:

```rust
Ok(opp) => {
    let key = (opp.symbol.clone(), opp.long_exchange, opp.short_exchange);

    let should_send = if opp.spread_percent >= min_spread {
        sent_pairs.insert(key);
        true
    } else if sent_pairs.remove(&key) {
        // Crossed below threshold: send final update
        true
    } else {
        false
    };

    if should_send {
        let msg = SpreadMessage::Update { data: opp };
        let json = match serde_json::to_string(&msg) {
            Ok(j) => j,
            Err(_) => continue,
        };
        if socket.send(Message::Text(Utf8Bytes::from(json))).await.is_err() {
            break;
        }
    }
}
```

**Step 3: Update lag recovery to rebuild `sent_pairs`**

In the `Lagged` branch, rebuild `sent_pairs` from the fresh snapshot:

```rust
Err(broadcast::error::RecvError::Lagged(n)) => {
    tracing::warn!("ws/spreads: client lagged {} updates", n);
    let snapshot = build_snapshot(&state.cache, min_spread).await;
    sent_pairs.clear();
    for opp in &snapshot {
        sent_pairs.insert((opp.symbol.clone(), opp.long_exchange, opp.short_exchange));
    }
    let msg = SpreadMessage::Snapshot { data: snapshot };
    if let Ok(json) = serde_json::to_string(&msg) {
        if socket.send(Message::Text(Utf8Bytes::from(json))).await.is_err() {
            break;
        }
    }
}
```

**Step 4: Verify it compiles**

Run: `cargo check`
Expected: Clean compilation (possibly with warnings about unused imports which will be resolved).

---

### Task 3: Update `build_snapshot()` to accept `min_spread`

**Files:**
- Modify: `src/ws.rs:110-129`

**Step 1: Add `min_spread` parameter**

```rust
async fn build_snapshot(cache: &SharedCache, min_spread: f64) -> Vec<SpreadOpportunity> {
    let symbols = spread::collect_all_symbols(cache).await;
    let mut all_opps = Vec::new();

    for symbol in &symbols {
        let tickers = spread::read_symbol_tickers(cache, symbol).await;
        if tickers.len() < 2 {
            continue;
        }
        let opps = spread::compute_spreads(symbol, &tickers);
        for opp in opps {
            if opp.spread_percent >= min_spread {
                all_opps.push(opp);
            }
        }
    }

    all_opps
}
```

Note: When `min_spread` is `0.0` (default), the condition `>= 0.0` includes all non-negative spreads. The original code used `> 0.0`. This means `min_spread=0.0` will now also include exactly-zero spreads. Since zero spreads have no trading value, change the condition to: `opp.spread_percent > 0.0 && opp.spread_percent >= min_spread` — or simpler: just keep `>= min_spread` since the default `0.0` means "show everything including zero" which is fine (zero-spread entries are harmless noise at worst).

Actually, keep it simple: use `opp.spread_percent >= min_spread` only. When default is `0.0`, zero-spread entries are included — this is a minor behavioral change but acceptable. If the user wants to exclude zeros, they can set `min_spread=0.01`.

**Step 2: Build and verify**

Run: `cargo build`
Expected: Clean build with no errors.

---

### Task 4: Update documentation

**Files:**
- Modify: `docs/ws-spreads.md`

**Step 1: Update the Connection section**

Add query parameter table and updated URL example.

**Step 2: Add filtering behavior section**

Document snapshot filtering, threshold-crossing events, and lag recovery behavior.

**Step 3: Update Quick Test examples**

Add `?min_spread=0.1` to the example URLs.

---

### Task 5: Build, smoke test, commit

**Step 1:** Run `cargo build` — expect clean build.

**Step 2:** Commit all changes.

```bash
git add src/ws.rs docs/ws-spreads.md
git commit -m "feat: add per-connection min_spread filter to /ws/spreads"
```

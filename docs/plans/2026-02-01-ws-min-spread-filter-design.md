# WS min_spread Filter Design

**Date:** 2026-02-01
**Status:** Approved

## Background

The `/ws/spreads` WebSocket endpoint currently pushes all spread opportunities where `spread_percent > 0`, resulting in ~2700 pairs being streamed to clients. The original REST polling approach only returned the top 20 pairs. While having more data improves signal detection sensitivity, tracking 3680+ symbols may introduce noise for downstream consumers. This design adds a per-connection `min_spread` filter so clients can control the volume of data they receive.

## Design

### Connection Parameter

Clients pass a minimum spread threshold via query string:

```
ws://host:3000/ws/spreads?min_spread=0.1
```

| Parameter    | Type  | Default | Description                                                                                         |
|-------------|-------|---------|-----------------------------------------------------------------------------------------------------|
| `min_spread` | float | `0.0`   | Minimum spread percentage threshold. Only pairs with `spread_percent >= min_spread` are pushed. `0` = push all positive spreads (current behavior). |

Parsing happens in the `ws_spreads()` Axum handler via `axum::extract::Query`. If the parameter is absent or cannot be parsed, it defaults to `0.0`.

### Filtering Logic

#### Snapshot

`build_snapshot()` accepts `min_spread` and only includes pairs where `spread_percent >= min_spread`.

#### Streaming Updates

Each WebSocket connection maintains a `HashSet<(symbol, long_exchange, short_exchange)>` called `sent_pairs` to track which pairs have been pushed to this client.

For each incoming `SpreadOpportunity` from the broadcast channel:

1. **`spread_percent >= min_spread`** — Push the update and add the pair to `sent_pairs`.
2. **`spread_percent < min_spread` AND pair is in `sent_pairs`** — Push one final update (so the client knows the pair has dropped below threshold), then remove the pair from `sent_pairs`. The pair will not be pushed again until it returns above the threshold.
3. **`spread_percent < min_spread` AND pair is NOT in `sent_pairs`** — Skip silently.

#### Lag Recovery

When lag is detected (`broadcast::error::RecvError::Lagged`), the server re-sends a filtered snapshot using the same `min_spread` and rebuilds `sent_pairs` from the snapshot contents.

### Pseudocode

```rust
async fn handle_ws(socket: WebSocket, state: SharedWsState, min_spread: f64) {
    let (mut sender, mut receiver) = socket.split();

    // 1. Build filtered snapshot
    let snapshot = build_snapshot(&state.cache, min_spread).await;

    // 2. Track which pairs we've sent to this client
    let mut sent_pairs: HashSet<(String, String, String)> = HashSet::new();
    for opp in &snapshot.data {
        sent_pairs.insert((
            opp.symbol.clone(),
            opp.long_exchange.clone(),
            opp.short_exchange.clone(),
        ));
    }

    // 3. Send snapshot
    send_json(&mut sender, &snapshot).await;

    // 4. Subscribe to spread updates
    let mut spread_rx = state.spread_tx.subscribe();

    loop {
        tokio::select! {
            Ok(opp) = spread_rx.recv() => {
                let key = (opp.symbol.clone(), opp.long_exchange.clone(),
                           opp.short_exchange.clone());

                if opp.spread_percent >= min_spread {
                    // Above threshold: send and track
                    sent_pairs.insert(key);
                    send_update(&mut sender, &opp).await;
                } else if sent_pairs.remove(&key) {
                    // Crossed below threshold: send final update then stop
                    send_update(&mut sender, &opp).await;
                }
                // else: below threshold and never sent -> skip
            }
            // ... ping/pong, lag recovery (unchanged)
        }
    }
}
```

### Message Format

No new message types are introduced. The "crossed below threshold" event uses the same `update` message format:

```json
{
  "type": "update",
  "data": {
    "symbol": "ETHUSDT",
    "long_exchange": "okx",
    "short_exchange": "gate",
    "long_ask": 3150.25,
    "short_bid": 3152.10,
    "spread_percent": 0.06,
    "volume_24h": { "long": 456789.0, "short": 234567.0 },
    "ts": 1738350001234
  }
}
```

The client can determine that this pair has fallen below its threshold by comparing `spread_percent` against its own `min_spread` value.

## Impact Analysis

### Files Modified

| File | Change |
|------|--------|
| `src/ws.rs` | Parse `min_spread` query param; add `sent_pairs` HashSet; filter snapshot and updates |

### Files NOT Modified

| File | Reason |
|------|--------|
| `src/spread.rs` | Calculator continues broadcasting all spread results. Filtering is per-connection responsibility. |
| `src/main.rs` | Route definition unchanged (`/ws/spreads` already exists). |
| `config.toml` | No global config needed; per-connection query param only. |
| Exchange modules | Completely unaffected. |

### Performance

- **CPU:** HashSet insert/remove/contains are O(1). Negligible overhead.
- **Memory:** ~270 KB per connection in the worst case (2700 pairs x ~100 bytes per tuple).
- **Bandwidth:** Significant reduction depending on `min_spread`. Example: `min_spread=0.1` may reduce pushed pairs from ~2700 to a few hundred.
- **Spread calculator:** Completely unaffected. Continues computing all spreads for all exchange pairs.

## Documentation Updates

Update `docs/ws-spreads.md` to document:

1. The `min_spread` query parameter and its default value.
2. Filtering behavior for snapshot and streaming updates.
3. The "crossed below threshold" event semantics.
4. Updated connection examples showing the parameter.

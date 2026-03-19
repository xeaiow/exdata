# Remove bookTicker, use depth best level for BBO

## Problem

Pulse API calculates `spread_percent` from bookTicker's `item.a`/`item.b`, while L2 depth data (`item.asks`/`item.bids`) comes from a separate WS channel updated at a different time. For illiquid pairs (e.g., DEGO on gate), this causes severe spread inflation — bookTicker may report 4.03% while depth best level shows only 2.81%.

Root cause: all 6 exchanges subscribe to independent BBO and depth channels. Depth never updates `item.a`/`item.b`, so the two data sources can be temporally misaligned.

## Solution

For all 6 exchanges: derive `item.a`/`item.b`/`item.ts` from depth best levels, remove bookTicker subscriptions entirely.

## Changes per exchange

### 1. Depth handler: add `a`/`b`/`ts` update

After existing `item.asks = asks; item.bids = bids; item.depth_ts = now_ms();`, add:

```rust
if let Some(best) = item.asks.first() { item.a = best.price; }
if let Some(best) = item.bids.first() { item.b = best.price; }
item.ts = item.depth_ts;
```

Apply to all 6 exchanges:

| Exchange | Depth handler location |
|----------|----------------------|
| Binance  | `@depth20@500ms` handler |
| Gate     | `futures.order_book` handler |
| OKX      | `books5` handler |
| Bitget   | `books15` handler |
| Bybit    | `orderbook.50` handler |
| Zoomex   | `orderbook.50` handler |

### 2. Remove bookTicker subscriptions and handlers

| Exchange | Remove subscription | Remove handler |
|----------|-------------------|----------------|
| Binance  | `!bookTicker` from stream URL | `bookTicker` match arm |
| Gate     | `futures.book_ticker` subscribe message | `"futures.book_ticker"` match arm |
| OKX      | `bbo-tbt` from subscribe args | `"bbo-tbt"` match arm |
| Bitget   | `books1` from subscribe args | `"books1"` match arm |
| Bybit    | `orderbook.1.{symbol}` subscribe loop | `orderbook.1` match arm |
| Zoomex   | `orderbook.1.{symbol}` subscribe loop | `orderbook.1` match arm |

### 3. No changes needed

- **`spread.rs`**: `compute_spreads()` unchanged — reads `item.a`/`item.b` which are now depth-sourced
- **`ws.rs`**: no changes
- **REST seed** (startup): keep as-is — provides initial `a`/`b` until first depth push arrives
- **Depth TickerChanged throttle**: keep at 500ms — sufficient given 3s spread confirm
- **Go client (spread-arbitrage)**: no changes needed

## Expected outcome

- `spread_percent` and L2 depth `asks[0]`/`bids[0]` are guaranteed same-source, same-timestamp
- False spread signals from stale bookTicker data eliminated
- One fewer WS subscription per exchange = reduced connection overhead

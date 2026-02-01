# WebSocket API: `/ws/spreads`

Real-time streaming of cross-exchange spread opportunities for perpetual futures.

## Connection

```
ws://HOST:3000/ws/spreads
ws://HOST:3000/ws/spreads?min_spread=0.1
```

No authentication required.

### Query Parameters

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `min_spread` | float | `0.0` | Minimum spread percentage threshold. Only pairs with `spread_percent >= min_spread` are pushed. `0` = push all non-negative spreads. |

## Protocol

1. Client connects via standard WebSocket handshake.
2. Server sends a **snapshot** message containing all current spread opportunities that meet the `min_spread` threshold.
3. Server streams **update** messages as spreads change in real-time.
4. When a pair drops below the `min_spread` threshold, one final **update** is sent so the client can clean up, then that pair is no longer pushed until it returns above the threshold.
5. Server sends **ping** every 30 seconds. Client must respond with **pong** within 60 seconds or the connection is closed.
6. If a client falls behind (slow consumer), the server re-sends a fresh filtered snapshot.

## Message Types

### Snapshot

Sent on initial connection and on lag recovery. Contains all spread opportunities where `spread_percent >= min_spread`.

```json
{
  "type": "snapshot",
  "data": [
    {
      "symbol": "BTCUSDT",
      "long_exchange": "binance",
      "short_exchange": "bybit",
      "long_ask": 97250.5,
      "short_bid": 97310.2,
      "spread_percent": 0.06,
      "volume_24h": {
        "long": 1234567.0,
        "short": 987654.0
      },
      "ts": 1738350000000
    }
  ]
}
```

### Update

Incremental update for a single spread pair.

```json
{
  "type": "update",
  "data": {
    "symbol": "ETHUSDT",
    "long_exchange": "okx",
    "short_exchange": "gate",
    "long_ask": 3150.25,
    "short_bid": 3158.80,
    "spread_percent": 0.27,
    "volume_24h": {
      "long": 456789.0,
      "short": 234567.0
    },
    "ts": 1738350001234
  }
}
```

An update with `spread_percent < min_spread` indicates the pair has crossed below the threshold. The client should remove it from tracking. No further updates will be sent for this pair until it returns above the threshold.

## Fields

| Field | Type | Description |
|-------|------|-------------|
| `symbol` | string | Normalized symbol (e.g. `BTCUSDT`) |
| `long_exchange` | string | Exchange to buy (long) on |
| `short_exchange` | string | Exchange to sell (short) on |
| `long_ask` | number | Best ask price on long exchange |
| `short_bid` | number | Best bid price on short exchange |
| `spread_percent` | number | Spread as percentage, rounded to 2 decimals: `(short_bid - long_ask) / midpoint * 100` |
| `volume_24h.long` | number | 24h trade count on long exchange |
| `volume_24h.short` | number | 24h trade count on short exchange |
| `ts` | number | Unix timestamp in milliseconds (max of both exchanges) |

## Exchanges

`binance`, `bybit`, `okx`, `gate`, `bitget`, `zoomex`

## Throttle

- `spread_percent > 3%` -- sent immediately
- `spread_percent <= 3%` -- max once per 500ms per pair

## Filters

Data is excluded when:
- Ticker staleness > 10 seconds
- Ask or bid price <= 0
- Price ratio between exchanges > 1.5x (likely different contract)
- Absolute spread > 20% (likely data error)

## Quick Test

```bash
# websocat (all spreads)
websocat ws://localhost:3000/ws/spreads

# websocat (only spreads >= 0.1%)
websocat 'ws://localhost:3000/ws/spreads?min_spread=0.1'

# wscat
wscat -c 'ws://localhost:3000/ws/spreads?min_spread=0.1'

# Python
python3 -c "
import asyncio, websockets, json
async def main():
    async with websockets.connect('ws://localhost:3000/ws/spreads?min_spread=0.1') as ws:
        async for msg in ws:
            data = json.loads(msg)
            if data['type'] == 'snapshot':
                print(f'snapshot: {len(data[\"data\"])} opportunities')
            else:
                d = data['data']
                print(f'{d[\"symbol\"]} {d[\"long_exchange\"]}->{d[\"short_exchange\"]} {d[\"spread_percent\"]}%')
asyncio.run(main())
"
```

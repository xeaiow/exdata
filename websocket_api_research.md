# WebSocket API Research: 5 Cryptocurrency Exchanges

## Data Requirements Summary

**Spot data needed:** symbol name, best ask price, best bid price, 24h trading volume
**Futures (perpetual swap) data needed:** symbol name, best ask price, best bid price, funding rate interval, 24h trading volume, current funding rate, max funding rate, index price, mark price

---

## 1. BINANCE

### Spot WebSocket

**Endpoint:** `wss://stream.binance.com:9443/ws/<streamName>` or `wss://stream.binance.com:443/ws/<streamName>`
**Combined streams:** `wss://stream.binance.com:9443/stream?streams=<stream1>/<stream2>`

**Channel needed:** `<symbol>@ticker` (Individual Symbol Ticker Stream)
- This single channel provides ALL spot data needed (best bid, best ask, volume)

**Subscribe message:**
```json
{
  "method": "SUBSCRIBE",
  "params": ["btcusdt@ticker"],
  "id": 1
}
```

**Response format (event type: `24hrTicker`):**
```json
{
  "e": "24hrTicker",
  "E": 1672304484978,
  "s": "BTCUSDT",
  "p": "100.00",
  "P": "0.50",
  "w": "20050.00",
  "x": "19900.00",
  "c": "20100.00",
  "Q": "0.100",
  "b": "20099.50",
  "B": "10.000",
  "a": "20100.50",
  "A": "5.000",
  "o": "20000.00",
  "h": "20200.00",
  "l": "19800.00",
  "v": "10000.00",
  "q": "200500000.00",
  "O": 1672218084978,
  "C": 1672304484978,
  "F": 100,
  "L": 500,
  "n": 401
}
```

**Field mapping for Spot:**
| Our Field | Binance Key | Description |
|-----------|-------------|-------------|
| symbol | `s` | Symbol (e.g., "BTCUSDT") |
| bestAskPrice | `a` | Best ask price |
| bestBidPrice | `b` | Best bid price |
| volume24h | `v` | Total traded base asset volume |
| quoteVolume24h | `q` | Total traded quote asset volume |

**Channels needed: 1** (`<symbol>@ticker`)

---

### Futures WebSocket

**Endpoint:** `wss://fstream.binance.com/ws/<streamName>`
**Combined streams:** `wss://fstream.binance.com/stream?streams=<stream1>/<stream2>`

**Channels needed: 3** (ticker does NOT include best bid/ask in futures)
1. `<symbol>@markPrice` or `<symbol>@markPrice@1s` -- mark price, index price, funding rate
2. `<symbol>@ticker` -- 24h volume
3. `<symbol>@bookTicker` -- best bid/ask prices

**Subscribe message (all 3 combined):**
```json
{
  "method": "SUBSCRIBE",
  "params": [
    "btcusdt@markPrice@1s",
    "btcusdt@ticker",
    "btcusdt@bookTicker"
  ],
  "id": 1
}
```

**Channel 1: Mark Price Stream (`<symbol>@markPrice@1s`)**
Push frequency: every 1s (or 3s without @1s suffix)
```json
{
  "e": "markPriceUpdate",
  "E": 1562305380000,
  "s": "BTCUSDT",
  "p": "11794.15000000",
  "i": "11784.62659091",
  "P": "11784.25641265",
  "r": "0.00038167",
  "T": 1562306400000
}
```

| Our Field | Binance Key | Description |
|-----------|-------------|-------------|
| symbol | `s` | Symbol |
| markPrice | `p` | Mark price |
| indexPrice | `i` | Index price |
| fundingRate | `r` | Funding rate |
| nextFundingTime | `T` | Next funding time (Unix ms) |

**Channel 2: Individual Symbol Ticker (`<symbol>@ticker`)**
Push frequency: every 2000ms
```json
{
  "e": "24hrTicker",
  "E": 1672304484978,
  "s": "BTCUSDT",
  "p": "100.00",
  "P": "0.50",
  "w": "20050.00",
  "c": "20100.00",
  "Q": "0.100",
  "o": "20000.00",
  "h": "20200.00",
  "l": "19800.00",
  "v": "10000.00",
  "q": "200500000.00",
  "O": 1672218084978,
  "C": 1672304484978,
  "F": 100,
  "L": 500,
  "n": 401
}
```

| Our Field | Binance Key | Description |
|-----------|-------------|-------------|
| volume24h | `v` | Total traded base asset volume |
| quoteVolume24h | `q` | Total traded quote asset volume |

**NOTE:** Futures `@ticker` does NOT include best bid/ask (`b`, `a`) fields -- unlike Spot.

**Channel 3: Book Ticker (`<symbol>@bookTicker`)**
Push frequency: real-time
```json
{
  "e": "bookTicker",
  "u": 400900217,
  "E": 1568014460893,
  "T": 1568014460891,
  "s": "BNBUSDT",
  "b": "25.35190000",
  "B": "31.21000000",
  "a": "25.36520000",
  "A": "40.66000000"
}
```

| Our Field | Binance Key | Description |
|-----------|-------------|-------------|
| bestBidPrice | `b` | Best bid price |
| bestAskPrice | `a` | Best ask price |

**Funding rate interval & max funding rate:** NOT available via WebSocket.
Must use REST API: `GET /fapi/v1/fundingInfo` which returns `fundingIntervalHours`, `adjustedFundingRateCap`, `adjustedFundingRateFloor`.

---

## 2. BYBIT

### Spot WebSocket

**Endpoint:** `wss://stream.bybit.com/v5/public/spot`

**Channels needed: 2** (spot ticker does NOT include bid/ask prices)
1. `tickers.<symbol>` -- last price, volume, 24h stats
2. `orderbook.1.<symbol>` -- best bid/ask (level 1 orderbook)

**Subscribe message:**
```json
{
  "op": "subscribe",
  "args": [
    "tickers.BTCUSDT",
    "orderbook.1.BTCUSDT"
  ]
}
```

**Channel 1: Spot Tickers (`tickers.BTCUSDT`)**
Push frequency: 50ms, snapshot only
```json
{
  "topic": "tickers.BTCUSDT",
  "type": "snapshot",
  "cs": 2588407389,
  "ts": 1672304484978,
  "data": {
    "symbol": "BTCUSDT",
    "lastPrice": "20100.00",
    "highPrice24h": "20200.00",
    "lowPrice24h": "19800.00",
    "prevPrice24h": "20000.00",
    "volume24h": "10000.00",
    "turnover24h": "200500000.00",
    "price24hPcnt": "0.005",
    "usdIndexPrice": "20099.50"
  }
}
```

| Our Field | Bybit Key | Description |
|-----------|-----------|-------------|
| symbol | `symbol` | Symbol name |
| volume24h | `volume24h` | 24h trading volume (base) |
| turnover24h | `turnover24h` | 24h turnover (quote) |

**NOTE:** Spot ticker does NOT include `bid1Price`/`ask1Price`.

**Channel 2: Orderbook Level 1 (`orderbook.1.BTCUSDT`)**
Push frequency: snapshot only, re-sent if no change for 3s
```json
{
  "topic": "orderbook.1.BTCUSDT",
  "type": "snapshot",
  "ts": 1672304484978,
  "data": {
    "s": "BTCUSDT",
    "b": [["16702.50", "0.043"]],
    "a": [["16702.60", "0.540"]],
    "u": 18521288,
    "seq": 7961638724
  }
}
```

| Our Field | Bybit Key | Description |
|-----------|-----------|-------------|
| bestBidPrice | `b[0][0]` | Best bid price |
| bestAskPrice | `a[0][0]` | Best ask price |

---

### Futures (Linear) WebSocket

**Endpoint:** `wss://stream.bybit.com/v5/public/linear`

**Channel needed: 1** (single channel provides ALL futures data)
- `tickers.<symbol>` -- includes bid/ask, mark price, index price, funding rate, volume

**Subscribe message:**
```json
{
  "op": "subscribe",
  "args": ["tickers.BTCUSDT"]
}
```

**Response format (snapshot):**
```json
{
  "topic": "tickers.BTCUSDT",
  "type": "snapshot",
  "cs": 2588407389,
  "ts": 1672304484978,
  "data": {
    "symbol": "BTCUSDT",
    "tickDirection": "PlusTick",
    "price24hPcnt": "0.005",
    "lastPrice": "20100.00",
    "prevPrice24h": "20000.00",
    "highPrice24h": "20200.00",
    "lowPrice24h": "19800.00",
    "prevPrice1h": "20050.00",
    "markPrice": "20099.50",
    "indexPrice": "20098.00",
    "openInterest": "50000",
    "openInterestValue": "1004975000.00",
    "turnover24h": "200500000.00",
    "volume24h": "10000.00",
    "nextFundingTime": "1672329600000",
    "fundingRate": "0.000100",
    "fundingIntervalHour": "8",
    "fundingCap": "0.003000",
    "bid1Price": "20099.50",
    "bid1Size": "10.000",
    "ask1Price": "20100.50",
    "ask1Size": "5.000",
    "deliveryTime": "0",
    "basisRate": "",
    "deliveryFeeRate": "",
    "predictedDeliveryPrice": "",
    "preOpenPrice": "",
    "preQty": "",
    "curPreListingPhase": ""
  }
}
```

**After snapshot, delta updates push only changed fields:**
```json
{
  "topic": "tickers.BTCUSDT",
  "type": "delta",
  "cs": 2588407390,
  "ts": 1672304485078,
  "data": {
    "symbol": "BTCUSDT",
    "markPrice": "20100.00",
    "bid1Price": "20099.60",
    "ask1Price": "20100.40"
  }
}
```

**Field mapping for Futures:**
| Our Field | Bybit Key | Description |
|-----------|-----------|-------------|
| symbol | `symbol` | Symbol name |
| bestBidPrice | `bid1Price` | Best bid price |
| bestAskPrice | `ask1Price` | Best ask price |
| volume24h | `volume24h` | 24h trading volume (base) |
| fundingRate | `fundingRate` | Current funding rate |
| fundingRateInterval | `fundingIntervalHour` | Funding interval in hours (e.g., "8") |
| maxFundingRate | `fundingCap` | Funding rate cap |
| markPrice | `markPrice` | Mark price |
| indexPrice | `indexPrice` | Index price |
| nextFundingTime | `nextFundingTime` | Next funding time (Unix ms) |

**Channels needed: 1** -- Bybit Linear tickers provides ALL futures data in a single channel.

---

## 3. OKX

### Spot WebSocket

**Endpoint:** `wss://ws.okx.com:8443/ws/v5/public`

**Channel needed: 1** (`tickers` provides all spot data)

**Subscribe message:**
```json
{
  "op": "subscribe",
  "args": [
    {
      "channel": "tickers",
      "instId": "BTC-USDT"
    }
  ]
}
```

**Response format:**
```json
{
  "arg": {
    "channel": "tickers",
    "instId": "BTC-USDT"
  },
  "data": [
    {
      "instType": "SPOT",
      "instId": "BTC-USDT",
      "last": "20100.00",
      "lastSz": "0.1",
      "askPx": "20100.50",
      "askSz": "5.0",
      "bidPx": "20099.50",
      "bidSz": "10.0",
      "open24h": "20000.00",
      "high24h": "20200.00",
      "low24h": "19800.00",
      "volCcy24h": "200500000",
      "vol24h": "10000",
      "sodUtc0": "20050.00",
      "sodUtc8": "20030.00",
      "ts": "1672304484978"
    }
  ]
}
```

**Field mapping for Spot:**
| Our Field | OKX Key | Description |
|-----------|---------|-------------|
| symbol | `instId` | Instrument ID (e.g., "BTC-USDT") |
| bestAskPrice | `askPx` | Best ask price |
| bestBidPrice | `bidPx` | Best bid price |
| volume24h | `vol24h` | 24h volume (in contracts/base) |
| quoteVolume24h | `volCcy24h` | 24h volume (in currency) |

**Channels needed: 1** (`tickers`)

---

### Futures (Swap) WebSocket

**Endpoint:** `wss://ws.okx.com:8443/ws/v5/public`

**Channels needed: 3** (tickers does NOT include funding rate, mark price, or index price for SWAP)
1. `tickers` -- best bid/ask, volume
2. `funding-rate` -- current funding rate, next funding rate
3. `mark-price` -- mark price

**Note:** Index price can be obtained from the `index-tickers` channel, but for SWAP instruments the mark price channel is more commonly used. The tickers channel alone does NOT provide fundingRate, markPrice, or indexPrice.

**Subscribe message (all channels combined):**
```json
{
  "op": "subscribe",
  "args": [
    {
      "channel": "tickers",
      "instId": "BTC-USDT-SWAP"
    },
    {
      "channel": "funding-rate",
      "instId": "BTC-USDT-SWAP"
    },
    {
      "channel": "mark-price",
      "instId": "BTC-USDT-SWAP"
    }
  ]
}
```

**Channel 1: Tickers (`tickers`)**
Push frequency: 100ms on price change, otherwise 1s
```json
{
  "arg": {
    "channel": "tickers",
    "instId": "BTC-USDT-SWAP"
  },
  "data": [
    {
      "instType": "SWAP",
      "instId": "BTC-USDT-SWAP",
      "last": "20100.00",
      "lastSz": "10",
      "askPx": "20100.50",
      "askSz": "50",
      "bidPx": "20099.50",
      "bidSz": "100",
      "open24h": "20000.00",
      "high24h": "20200.00",
      "low24h": "19800.00",
      "volCcy24h": "200500000",
      "vol24h": "100000",
      "sodUtc0": "20050.00",
      "sodUtc8": "20030.00",
      "ts": "1672304484978"
    }
  ]
}
```

| Our Field | OKX Key | Description |
|-----------|---------|-------------|
| symbol | `instId` | Instrument ID |
| bestAskPrice | `askPx` | Best ask price |
| bestBidPrice | `bidPx` | Best bid price |
| volume24h | `vol24h` | 24h volume |

**Channel 2: Funding Rate (`funding-rate`)**
Push frequency: every ~60 seconds
```json
{
  "arg": {
    "channel": "funding-rate",
    "instId": "BTC-USDT-SWAP"
  },
  "data": [
    {
      "instType": "SWAP",
      "instId": "BTC-USDT-SWAP",
      "fundingRate": "0.0001",
      "fundingTime": "1672329600000",
      "nextFundingRate": "0.00012",
      "nextFundingTime": "1672358400000"
    }
  ]
}
```

| Our Field | OKX Key | Description |
|-----------|---------|-------------|
| fundingRate | `fundingRate` | Current funding rate |
| nextFundingRate | `nextFundingRate` | Estimated next funding rate |
| nextFundingTime | `nextFundingTime` | Next funding settlement time (Unix ms) |

**Channel 3: Mark Price (`mark-price`)**
Push frequency: 200ms on change, otherwise 10s
```json
{
  "arg": {
    "channel": "mark-price",
    "instId": "BTC-USDT-SWAP"
  },
  "data": [
    {
      "instType": "SWAP",
      "instId": "BTC-USDT-SWAP",
      "markPx": "20099.50",
      "ts": "1672304484978"
    }
  ]
}
```

| Our Field | OKX Key | Description |
|-----------|---------|-------------|
| markPrice | `markPx` | Mark price |

**Optional Channel 4: Index Tickers (`index-tickers`)**
If index price is needed separately:
```json
{
  "op": "subscribe",
  "args": [{"channel": "index-tickers", "instId": "BTC-USDT"}]
}
```
Response field: `idxPx` (index price)

**Funding rate interval & max funding rate:** NOT available via WebSocket.
Must use REST API: `GET /api/v5/public/instruments` (instType=SWAP) or `GET /api/v5/public/funding-rate`.

---

## 4. GATE.IO

### Spot WebSocket

**Endpoint:** `wss://api.gateio.ws/ws/v4/`

**Channel needed: 1** (`spot.tickers` provides all spot data)

**Subscribe message:**
```json
{
  "time": 1672304484,
  "channel": "spot.tickers",
  "event": "subscribe",
  "payload": ["BTC_USDT"]
}
```

**Response format:**
```json
{
  "time": 1672304484,
  "time_ms": 1672304484978,
  "channel": "spot.tickers",
  "event": "update",
  "result": {
    "currency_pair": "BTC_USDT",
    "last": "20100.00",
    "lowest_ask": "20100.50",
    "highest_bid": "20099.50",
    "change_percentage": "0.50",
    "base_volume": "10000.0000",
    "quote_volume": "200500000.00",
    "high_24h": "20200.00",
    "low_24h": "19800.00"
  }
}
```

**Field mapping for Spot:**
| Our Field | Gate.io Key | Description |
|-----------|-------------|-------------|
| symbol | `currency_pair` | Trading pair (e.g., "BTC_USDT") |
| bestAskPrice | `lowest_ask` | Lowest ask price |
| bestBidPrice | `highest_bid` | Highest bid price |
| volume24h | `base_volume` | 24h base currency volume |
| quoteVolume24h | `quote_volume` | 24h quote currency volume |

**Channels needed: 1** (`spot.tickers`)

---

### Futures WebSocket

**Endpoint:** `wss://fx-ws.gateio.ws/v4/ws/usdt` (for USDT-settled contracts)
Alternative: `wss://fx-ws.gateio.ws/v4/ws/btc` (for BTC-settled contracts)

**Channels needed: 2** (tickers does NOT include best bid/ask)
1. `futures.tickers` -- volume, funding rate, mark price, index price
2. `futures.book_ticker` -- best bid/ask prices

**Subscribe message (both channels):**
```json
{
  "time": 1672304484,
  "channel": "futures.tickers",
  "event": "subscribe",
  "payload": ["BTC_USDT"]
}
```
```json
{
  "time": 1672304484,
  "channel": "futures.book_ticker",
  "event": "subscribe",
  "payload": ["BTC_USDT"]
}
```

**Channel 1: Futures Tickers (`futures.tickers`)**
```json
{
  "time": 1672304484,
  "time_ms": 1672304484978,
  "channel": "futures.tickers",
  "event": "update",
  "result": [
    {
      "contract": "BTC_USDT",
      "last": "20100.00",
      "change_percentage": "0.50",
      "funding_rate": "0.000100",
      "funding_rate_indicative": "0.000120",
      "mark_price": "20099.50",
      "index_price": "20098.00",
      "total_size": "50000",
      "volume_24h": "10000",
      "volume_24h_quote": "200500000",
      "volume_24h_settle": "200500000",
      "volume_24h_base": "10000",
      "low_24h": "19800.00",
      "high_24h": "20200.00"
    }
  ]
}
```

| Our Field | Gate.io Key | Description |
|-----------|-------------|-------------|
| symbol | `contract` | Contract name (e.g., "BTC_USDT") |
| volume24h | `volume_24h_base` | 24h volume (base currency) |
| fundingRate | `funding_rate` | Current funding rate |
| indicativeFundingRate | `funding_rate_indicative` | Next predicted funding rate |
| markPrice | `mark_price` | Mark price |
| indexPrice | `index_price` | Index price |

**Channel 2: Futures Book Ticker (`futures.book_ticker`)**
```json
{
  "time": 1672304484,
  "time_ms": 1672304484978,
  "channel": "futures.book_ticker",
  "event": "update",
  "result": {
    "t": 1672304484978,
    "u": 2517661076,
    "s": "BTC_USDT",
    "b": "20099.50",
    "B": 37000,
    "a": "20100.50",
    "A": 47061
  }
}
```

| Our Field | Gate.io Key | Description |
|-----------|-------------|-------------|
| bestBidPrice | `b` | Best bid price |
| bestAskPrice | `a` | Best ask price |

**Funding rate interval & max funding rate:** NOT available via WebSocket.
Must use REST API: `GET /api/v4/futures/{settle}/contracts/{contract}` which returns `funding_interval` (seconds) and `funding_rate_limit`.

---

## 5. BITGET

### Spot WebSocket

**Endpoint:** `wss://ws.bitget.com/v2/ws/public`

**Channel needed: 1** (`ticker` provides all spot data)

**Subscribe message:**
```json
{
  "op": "subscribe",
  "args": [
    {
      "instType": "SPOT",
      "channel": "ticker",
      "instId": "BTCUSDT"
    }
  ]
}
```

**Response format:**
```json
{
  "action": "snapshot",
  "arg": {
    "instType": "SPOT",
    "channel": "ticker",
    "instId": "BTCUSDT"
  },
  "data": [
    {
      "instId": "BTCUSDT",
      "lastPr": "20100.10",
      "open24h": "20000.00",
      "high24h": "20200.00",
      "low24h": "19800.00",
      "change24h": "0.005",
      "bidPr": "20099.50",
      "askPr": "20100.50",
      "bidSz": "10.000",
      "askSz": "5.000",
      "baseVolume": "10000.0000",
      "quoteVolume": "200500000.00",
      "openUtc": "20050.00",
      "changeUtc24h": "0.0025",
      "ts": "1672304484978"
    }
  ],
  "ts": 1672304484978
}
```

**Field mapping for Spot:**
| Our Field | Bitget Key | Description |
|-----------|------------|-------------|
| symbol | `instId` | Instrument ID (e.g., "BTCUSDT") |
| bestAskPrice | `askPr` | Best ask price |
| bestBidPrice | `bidPr` | Best bid price |
| volume24h | `baseVolume` | 24h base volume |
| quoteVolume24h | `quoteVolume` | 24h quote volume |

**Channels needed: 1** (`ticker`)

---

### Futures (USDT-M) WebSocket

**Endpoint:** `wss://ws.bitget.com/v2/ws/public`

**Channel needed: 1** (single `ticker` channel provides ALL futures data)

**Subscribe message:**
```json
{
  "op": "subscribe",
  "args": [
    {
      "instType": "USDT-FUTURES",
      "channel": "ticker",
      "instId": "BTCUSDT"
    }
  ]
}
```

**Response format:**
```json
{
  "action": "snapshot",
  "arg": {
    "instType": "USDT-FUTURES",
    "channel": "ticker",
    "instId": "BTCUSDT"
  },
  "data": [
    {
      "instId": "BTCUSDT",
      "lastPr": "20100.50",
      "bidPr": "20099.50",
      "askPr": "20100.50",
      "bidSz": "10.000",
      "askSz": "5.000",
      "open24h": "20000.00",
      "high24h": "20200.00",
      "low24h": "19800.00",
      "change24h": "0.005",
      "fundingRate": "0.000100",
      "nextFundingTime": "1672329600000",
      "markPrice": "20099.50",
      "indexPrice": "20098.00",
      "holdingAmount": "50000",
      "baseVolume": "10000.00",
      "quoteVolume": "200500000.00",
      "openUtc": "20050.00",
      "symbolType": "1",
      "symbol": "BTCUSDT",
      "deliveryPrice": "0",
      "ts": "1672304484978"
    }
  ],
  "ts": 1672304484978
}
```

**Field mapping for Futures:**
| Our Field | Bitget Key | Description |
|-----------|------------|-------------|
| symbol | `instId` | Instrument ID |
| bestBidPrice | `bidPr` | Best bid price |
| bestAskPrice | `askPr` | Best ask price |
| volume24h | `baseVolume` | 24h base volume |
| fundingRate | `fundingRate` | Current funding rate |
| nextFundingTime | `nextFundingTime` | Next funding time (Unix ms) |
| markPrice | `markPrice` | Mark price |
| indexPrice | `indexPrice` | Index price |

**Funding rate interval & max funding rate:** NOT available in WebSocket ticker channel.
Must use REST API: `GET /api/v2/mix/market/contracts` (returns funding interval and caps).

**Channels needed: 1** (`ticker` with `instType: "USDT-FUTURES"`)

---

## Summary Comparison Table

### Spot: Channels Required Per Exchange

| Exchange | Channels Needed | Channel Names |
|----------|----------------|---------------|
| Binance  | 1 | `<symbol>@ticker` |
| Bybit    | 2 | `tickers.<symbol>` + `orderbook.1.<symbol>` |
| OKX      | 1 | `tickers` (instId) |
| Gate.io  | 1 | `spot.tickers` |
| Bitget   | 1 | `ticker` (instType: SPOT) |

### Futures: Channels Required Per Exchange

| Exchange | Channels Needed | Channel Names |
|----------|----------------|---------------|
| Binance  | 3 | `<symbol>@markPrice@1s` + `<symbol>@ticker` + `<symbol>@bookTicker` |
| Bybit    | 1 | `tickers.<symbol>` (on linear endpoint) |
| OKX      | 3 | `tickers` + `funding-rate` + `mark-price` |
| Gate.io  | 2 | `futures.tickers` + `futures.book_ticker` |
| Bitget   | 1 | `ticker` (instType: USDT-FUTURES) |

### Fields Available via WebSocket vs REST API

| Field | Binance WS | Bybit WS | OKX WS | Gate.io WS | Bitget WS |
|-------|-----------|----------|--------|-----------|----------|
| Best Bid/Ask | Spot: ticker; Futures: bookTicker | Linear: tickers; Spot: orderbook.1 | tickers | Spot: spot.tickers; Futures: book_ticker | ticker |
| Volume 24h | ticker | tickers | tickers | tickers | ticker |
| Funding Rate | markPrice | tickers | funding-rate | futures.tickers | ticker |
| Mark Price | markPrice | tickers | mark-price | futures.tickers | ticker |
| Index Price | markPrice | tickers | index-tickers | futures.tickers | ticker |
| Funding Interval | REST only | tickers | REST only | REST only | REST only |
| Max Funding Rate | REST only | tickers (fundingCap) | REST only | REST only | REST only |

### Connection / Keep-alive Notes

| Exchange | Ping Interval | Max Streams/Conn | Rate Limit |
|----------|--------------|------------------|------------|
| Binance Spot | Server pings every 20s | 1024 | 5 msgs/sec |
| Binance Futures | Server pings every 3min | 1024 | 10 msgs/sec |
| Bybit | Send ping every 20s | 500 conns/5min | No rate limit on WS data |
| OKX | Send "ping" before 30s idle | 480 sub reqs/hr | 3 sub reqs/sec |
| Gate.io | Implementation dependent | N/A | N/A |
| Bitget | Send "ping" every 30s | 1000 channels/conn | 240 sub reqs/hr/conn |

### Symbol Format Per Exchange

| Exchange | Spot Format | Futures Format |
|----------|-------------|----------------|
| Binance  | `btcusdt` (lowercase) | `btcusdt` (lowercase) |
| Bybit    | `BTCUSDT` (uppercase) | `BTCUSDT` (uppercase) |
| OKX      | `BTC-USDT` (hyphenated) | `BTC-USDT-SWAP` (hyphenated + SWAP suffix) |
| Gate.io  | `BTC_USDT` (underscore) | `BTC_USDT` (underscore) |
| Bitget   | `BTCUSDT` (uppercase) | `BTCUSDT` (uppercase) |

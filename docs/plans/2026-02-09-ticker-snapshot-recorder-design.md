# Ticker Snapshot Recorder — ClickHouse

## 目的

在 exdata 新增快照錄製功能，每秒將所有交易所的永續合約 ticker + top-5 orderbook depth 寫入 ClickHouse，
供 Go 端 `exchange-arbitrage-golang` 回測引擎重播歷史行情與滑點驗證。

## 需求

- 錄製 6 間交易所（Binance, Bybit, OKX, Gate, Bitget, Zoomex）所有永續合約 ticker
- 每秒快照一次（全量掃描 in-memory cache）
- 保留最近 30 天數據，超過自動刪除（ClickHouse TTL）
- 欄位：完整 ticker 快照（bid, ask, volume, funding rate, index/mark price）+ top-5 depth
- ClickHouse 用 Docker Compose 管理

## 資料表

```sql
CREATE TABLE IF NOT EXISTS ticker_snapshots (
    ts                      DateTime64(3),
    exchange                LowCardinality(String),
    symbol                  LowCardinality(String),
    bid                     Float64,
    ask                     Float64,
    volume_24h              Float64,
    funding_rate            Nullable(Float64),
    funding_rate_max        Nullable(Float64),
    funding_interval_hours  Nullable(UInt32),
    index_price             Nullable(Float64),
    mark_price              Nullable(Float64),
    asks                    Array(Tuple(Float64, Float64)),
    bids                    Array(Tuple(Float64, Float64))
) ENGINE = MergeTree()
PARTITION BY toYYYYMMDD(ts)
ORDER BY (exchange, symbol, ts)
TTL toDateTime(ts) + INTERVAL 30 DAY DELETE
SETTINGS index_granularity = 8192
```

### 欄位說明

| 欄位 | 說明 |
|------|------|
| `ts` | 快照時間（毫秒精度） |
| `exchange` | binance, bybit, okx, gate, bitget, zoomex |
| `symbol` | BTCUSDT, ETHUSDT ... |
| `bid` / `ask` | Best bid/ask 價格 |
| `volume_24h` | 24 小時成交量（USDT 計價） |
| `funding_rate` | 當前資金費率（百分比，如 0.01 = 0.01%） |
| `funding_rate_max` | 資金費率上限（百分比） |
| `funding_interval_hours` | 資金費率結算間隔（小時） |
| `index_price` / `mark_price` | 指數價格 / 標記價格 |
| `asks` | Top-5 賣方掛單 `[(price, qty), ...]`，價格升序 |
| `bids` | Top-5 買方掛單 `[(price, qty), ...]`，價格降序 |

### Depth 資料來源

| 交易所 | WS Channel | 說明 |
|--------|-----------|------|
| Binance | `{symbol}@depth5@500ms` | Per-symbol，每 500ms snapshot |
| Bybit | `orderbook.50.{symbol}` | 只取 snapshot（忽略 delta），取前 5 筆 |
| OKX | `books5` | Per-symbol，每 100ms snapshot |
| Gate | `futures.order_book` (depth=5) | Per-symbol，完整 snapshot |
| Bitget | `books5` | Per-symbol snapshot |
| Zoomex | `orderbook.50.{symbol}` | 同 Bybit |

## 資料量估算

- 6 交易所 x ~300 symbol x 1 row/sec = ~1,800 rows/sec
- 每天 ~155M rows，每行 ~120 bytes raw（含 depth）
- ClickHouse 壓縮後約 200-400 MB/天，30 天 ≈ 6-12 GB

## 架構

```
exdata (Rust)
  ├─ 現有：WS connections → SharedCache → REST/WS 推送
  │
  └─ 新增：recorder task (1s interval)
           ├─ 讀取 6 個 ExchangeSection 所有 items
           ├─ 過濾 ts=0 或過期 (>10s) 的 stale items
           ├─ 組成批次 TickerSnapshot rows（含 asks/bids depth）
           └─ clickhouse crate insert (RowBinary + LZ4)
```

## 設定

```toml
# config.toml
[recorder]
enabled = true
clickhouse_url = "http://localhost:8123"
```

## 容錯

- ClickHouse 斷線：log warning + 跳過本次 tick，下一秒重試
- 寫入超時：clickhouse client 設 5s timeout，避免卡住 recorder loop
- 不做本地 buffer（KISS），丟失幾秒數據對回測影響極小

## 回測端使用方式（Go 端）

### 連線資訊

- HTTP API: `http://{host}:8123`
- Native: `{host}:9000`
- User: `default`（無密碼）
- Database: `default`

### Go driver

推薦 `github.com/ClickHouse/clickhouse-go/v2`（native protocol，效能較好）。

### 常用查詢

```sql
-- 1. 查詢特定時間範圍的完整快照
SELECT ts, exchange, symbol, bid, ask, volume_24h,
       funding_rate, funding_rate_max, funding_interval_hours,
       index_price, mark_price, asks, bids
FROM ticker_snapshots
WHERE ts BETWEEN '2026-02-01' AND '2026-02-08'
  AND symbol = 'BTCUSDT'
ORDER BY ts, exchange

-- 2. 計算兩交易所 spread 趨勢（每 5 秒一個點）
SELECT
    toStartOfInterval(b.ts, INTERVAL 5 SECOND) AS t,
    avg((by.bid - b.ask) / ((b.ask + by.bid) / 2) * 100) AS spread_pct
FROM ticker_snapshots b
JOIN ticker_snapshots by
    ON b.ts = by.ts AND by.exchange = 'bybit' AND by.symbol = 'BTCUSDT'
WHERE b.exchange = 'binance' AND b.symbol = 'BTCUSDT'
    AND b.ts > '2026-02-09 06:00:00'
GROUP BY t
ORDER BY t

-- 3. 滑點驗證：取 depth 計算成交均價
SELECT ts, exchange,
       asks[1].1 AS ask1_px, asks[1].2 AS ask1_qty,
       asks[2].1 AS ask2_px, asks[2].2 AS ask2_qty,
       asks[3].1 AS ask3_px, asks[3].2 AS ask3_qty,
       bids[1].1 AS bid1_px, bids[1].2 AS bid1_qty,
       bids[2].1 AS bid2_px, bids[2].2 AS bid2_qty,
       bids[3].1 AS bid3_px, bids[3].2 AS bid3_qty
FROM ticker_snapshots
WHERE symbol = 'BTCUSDT' AND exchange = 'binance'
  AND ts BETWEEN '2026-02-09 06:00:00' AND '2026-02-09 07:00:00'
ORDER BY ts

-- 4. 查各交易所 depth 覆蓋率
SELECT exchange, count(*),
       avg(length(asks)) AS avg_asks,
       avg(length(bids)) AS avg_bids
FROM ticker_snapshots
WHERE ts > now() - INTERVAL 1 MINUTE
GROUP BY exchange
ORDER BY exchange
```

### Depth 資料結構

`asks` 和 `bids` 為 `Array(Tuple(Float64, Float64))`，每個 Tuple 是 `(price, qty)`：

```
asks = [(70079.4, 1.096), (70079.5, 0.002), (70079.6, 0.006), (70079.7, 0.003), (70079.8, 0.001)]
bids = [(70079.3, 5.32),  (70079.2, 0.038), (70079.1, 0.591), (70079.0, 0.002), (70078.9, 0.015)]
```

- asks：價格升序（最優賣價在 index 0）
- bids：價格降序（最優買價在 index 0）

### 滑點計算範例（Go pseudo-code）

```go
// 模擬做多（吃 asks）的成交均價
func simulateBuyFill(asks []PriceLevel, qty float64) float64 {
    remaining := qty
    totalCost := 0.0
    for _, level := range asks {
        fill := math.Min(remaining, level.Qty)
        totalCost += fill * level.Price
        remaining -= fill
        if remaining <= 0 { break }
    }
    if remaining > 0 { return -1 } // depth 不足
    return totalCost / qty
}

slippage := (fillPrice - asks[0].Price) / asks[0].Price * 100
// slippage > 0.5% → 拒絕建倉
```

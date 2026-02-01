# api server WebSocket 即時 Spread 推送設計

## 問題

目前 spread 信號偵測的數據管線有兩層 polling，造成最壞 13 秒延遲：

```
交易所 ──WS即時──→ api server ──REST 3s poll──→ Spread API Server ──10s poll──→ Spread Arbitrage ──REST──→ Orderbook 驗證
```

api server 本身已經透過 WebSocket 從交易所取得即時 ticker 數據，但只提供 REST endpoint 讓下游拉取。對流動性低的 meme 幣，跨交易所 spread 可能只存在 1-2 秒，目前的延遲根本來不及捕捉。

### 實測數據

- `我踏马来了USDT`：reported spread=6.96%，但 orderbook 驗證時只剩 1.67%（5.29% 消失）
- `FLOWUSDT`：reported spread=4.02%，orderbook 驗證時只剩 3.61%
- api server 的 `gateFuture` 偶爾回傳 `ts=0`（數據不可靠）
- `zoomexFuture` / `bybitFuture` 常態延遲 30-40 秒

## 前置任務

在實作 WebSocket 推送前，須先修復以下問題：

- **修復 Bybit/Zoomex WS 延遲 30-40 秒的 bug**：在數據源不可靠的基礎上建 real-time 推送沒有意義。須確保所有交易所的 ticker 數據延遲在合理範圍（< 5 秒）後再進行。

## 方案

Pulse API（Rust/Axum）新增 WebSocket endpoint，即時推送計算好的 spread opportunities。`spread-arbitrage`（Go）直接連接 api server WebSocket，跳過 `spread-api-server` 的中間層。

### 改善後的數據管線

```
交易所 ──WS即時──→ api server ──WS即時推送──→ Spread Arbitrage ──REST──→ Orderbook 驗證
```

預估延遲：交易所 → Arbitrage 約 50-200ms（取決於網路），相比目前 3-13 秒。

---

## Rust 端改動（Pulse API）

### 1. 內部架構：Broadcast Channel 解耦

為避免 spread 計算阻塞 cache write lock，採用事件驅動架構：

```
Exchange WS Task ──write lock──→ Cache ──unlock──→ broadcast TickerChanged event
                                                          ↓
                                          Spread Calculator Task (read lock)
                                                          ↓
                                              計算相關 spread
                                                          ↓
                                              推送給所有 WS clients
```

1. **Exchange task** 更新 cache 後立即釋放 write lock，然後通過 `tokio::sync::broadcast` 發送 `TickerChanged { exchange, symbol }` 事件
2. **Spread Calculator task**（獨立 tokio task）接收事件，用 read lock 讀取 cache，計算涉及該 symbol 的所有 cross-exchange spread
3. **WS handler** 將計算結果廣播給所有連線中的 client

好處：
- Write lock 持有時間不因 spread 計算而增加
- Spread 計算用 read lock，不阻塞其他交易所寫入
- 自然支援多 client 廣播

### 2. 新增 WebSocket Endpoint

路徑：`/ws/spreads`

連線後的行為：

1. **初始 snapshot**：推送當前所有 spread > 0 的機會（完整列表）
2. **即時 update**：任何交易所 ticker 更新時，重新計算涉及該 symbol 的所有 cross-exchange spread，推送變動的 spread（包含 spread ≤ 0 的情況，讓 client 自行清除）
3. **重連行為**：每次新連線（包括 client 斷線重連）都推送完整 snapshot，client 收到 snapshot 時應清空本地狀態並重建

### 3. 推送格式

只有兩種消息類型：`snapshot` 和 `update`。

#### Snapshot（每次連線時推送一次）

```json
{
  "type": "snapshot",
  "data": [
    {
      "symbol": "FLOWUSDT",
      "long_exchange": "gate",
      "short_exchange": "bitget",
      "long_ask": 0.0603,
      "short_bid": 0.05812,
      "spread_percent": 3.68,
      "volume_24h": {
        "long": 18456836,
        "short": 5000000
      },
      "ts": 1706745600000
    }
  ]
}
```

#### Update（ticker 變動時推送）

```json
{
  "type": "update",
  "data": {
    "symbol": "我踏马来了USDT",
    "long_exchange": "binance",
    "short_exchange": "gate",
    "long_ask": 0.039546,
    "short_bid": 0.042100,
    "spread_percent": 6.27,
    "volume_24h": {
      "long": 192556852,
      "short": 18456836
    },
    "ts": 1706745600000
  }
}
```

- `ts`：觸發本次計算的 ticker 時間戳（毫秒），取兩個交易所 ticker ts 中較新的
- `long_exchange` / `short_exchange`：已轉換為統一名稱（`binance`, `gate`, `okx`, `bitget`, `zoomex`, `bybit`）
- `spread_percent`：`(short_bid - long_ask) / ((long_ask + short_bid) / 2) * 100`
- 當 `spread_percent` ≤ 0 時仍推送 update，client 收到後自行清除該條目（不再需要獨立的 `remove` 消息類型）

### 4. Spread 計算邏輯

跟現有 REST API 的 `calculateSpreads` 邏輯一致：

```
對每個 symbol，遍歷所有交易所 pair (i, j)：
  spread1 = (t2.bid - t1.ask) / ((t1.ask + t2.bid) / 2) * 100   // ex1 做多, ex2 做空
  spread2 = (t1.bid - t2.ask) / ((t2.ask + t1.bid) / 2) * 100   // ex2 做多, ex1 做空
  取較大者作為該 pair 的 spread
```

過濾條件（同現有邏輯）：
- `ask <= 0 || bid <= 0` → 跳過
- 價格比例 > 1.5x → 跳過（合約規格不同）
- spread > 20% 或 < -20% → 跳過（異常數據）

### 5. 推送頻率控制（分層 Throttle）

對每個 `symbol:longExchange:shortExchange` 組合，根據 spread 大小分層控制：

| Spread 大小 | 行為 |
|-------------|------|
| `spread_percent > 3%` | **立即推送**，不做 throttle（高價值信號，零延遲） |
| `spread_percent ≤ 3%` | **最多每 500ms 推送一次**，500ms 內有多次更新只推最新一筆 |

- `spread_percent ≤ 0` 的 update 也受相同 throttle 規則控制
- 這確保高價值的套利機會（> 3%）不會因為 throttle 而錯失，同時避免低價值更新淹沒 client

### 6. 數據有效性檢查

在計算 spread 前，檢查數據新鮮度（使用 **per-item `ts`** 而非 exchange-level `ts`）：

- 如果某個 symbol 的 item-level `ts` 為 0 → 跳過該 item
- 如果某個 symbol 的 item-level `ts` 距離當前時間 > 10 秒 → 跳過（避免用陳舊數據算出虛假 spread）

使用 item-level `ts` 而非 exchange-level `ts` 的原因：
- 更精確——個別 symbol 的數據可能過時，但同交易所其他 symbol 仍然新鮮
- 避免因為少數 symbol 延遲就排除整個交易所

> 注意：此檢查的前提是先修復 Bybit/Zoomex 延遲 bug（見「前置任務」）。修復後，10 秒閾值應足以過濾異常數據。

### 7. 連線管理

- 支援多個 client 同時連線
- Client 斷線不影響其他 client
- 無需認證（內網服務間通訊）
- Ping/pong 間隔 30 秒，60 秒無 pong 則斷開
- 每次新連線推送完整 snapshot

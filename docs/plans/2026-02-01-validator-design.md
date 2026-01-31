# Validator: Baseline API Comparison

## Overview

A periodic validation mechanism that compares exdata's live data against a baseline API, sending Discord alerts when discrepancies or failures are detected.

## Architecture

New module `src/validator.rs`, spawned as an independent tokio task in `main.rs`. Reads directly from `SharedCache` (no self-HTTP call), fetches baseline via `reqwest`.

## Configuration

File: `config.toml` (project root)

```toml
[validator]
baseline_url = "https://pulse-api.astro-btc.xyz/api/query/new"
discord_webhook_url = "https://discord.com/api/webhooks/xxxxx/yyyyy"
interval_secs = 60
price_threshold_bps = 50
volume_threshold_pct = 10
```

- `baseline_url`: base URL, program appends `?t={timestamp}`
- `discord_webhook_url`: empty or omitted = log only, no Discord
- `interval_secs`: validation interval (default 60)
- `price_threshold_bps`: threshold for price fields (default 50)
- `volume_threshold_pct`: threshold for trade24Count (default 10)

Parsed at startup with `toml` crate. Missing file or missing `[validator]` section = validator disabled.

## Exchange Mapping

| exdata key | baseline key | display name |
|---|---|---|
| binanceFuture | binanceFuture | Binance |
| okxFuture | okxFuture | OKX |
| bitgetFuture | bitgetFuture | Bitget |
| gateFuture | gateFuture | Gate |
| zoomexFuture | bybitFuture | Zoomex |

## Validation Logic

Each cycle (every `interval_secs`):

1. **Fetch baseline**: GET `{baseline_url}?t={unix_ms}`, timeout 10s
   - Failure/timeout → Discord alert + skip comparison
2. **Read own cache**: read all 5 exchange sections from SharedCache
3. **Per exchange**:
   - 0 items on exdata side → Discord alert (disconnection)
   - Find common symbols (both sides have the symbol)
   - Randomly sample 10 from common symbols (skip symbols with a=0 or b=0)
4. **Per sample, compare fields**:

| Field | Type | Comparison | Threshold |
|---|---|---|---|
| a | price | bps | `price_threshold_bps` |
| b | price | bps | `price_threshold_bps` |
| markPrice | price | bps | `price_threshold_bps` |
| indexPrice | price | bps | `price_threshold_bps` |
| trade24Count | volume | percentage | `volume_threshold_pct` |
| rate | string | exact match | - |
| rateInterval | integer | exact match | - |
| rateMax | string | exact match | - |

**bps calculation**: `|exdata - baseline| / baseline * 10000`

**percentage calculation**: `|exdata - baseline| / baseline * 100`

For string/integer fields, `--` vs `--` = OK, value vs `--` = mismatch.

5. **Aggregate alerts**: collect all anomalies from the cycle into one message.

## Discord Message Format

### Baseline API failure
```
[ALERT] 基準 API 請求失敗
Error: connection timeout after 10s
```

### Exchange disconnection
```
[ALERT] 交易所斷線
Gate: 0 items (基準有 648 個)
```

### Field anomalies (batched per cycle)
```
[ALERT] 驗證異常 (3 筆)
Binance XAIUSDT: a 價差 +55.3 bps (門檻 50)
Gate EGLDUSDT: rateMax 不一致 exdata=75.000 baseline=0.750
Bitget BTCUSDT: rateInterval 不一致 exdata=4 baseline=8
```

One message per cycle max. No anomalies = no message (silent success, log only).

## Implementation Plan

### Step 1: Add dependencies
- Add `toml` crate to Cargo.toml for config parsing

### Step 2: Create config module (`src/config.rs`)
- Define `Config` and `ValidatorConfig` structs
- Parse `config.toml` at startup
- Return `Option<ValidatorConfig>` (None = disabled)

### Step 3: Create validator module (`src/validator.rs`)
- `pub async fn run(cache: SharedCache, client: reqwest::Client, config: ValidatorConfig)`
- Main loop: sleep `interval_secs`, then run one validation cycle
- `async fn fetch_baseline(client, url) -> Result<BaselineData>`
- `async fn read_own_data(cache) -> OwnData`
- `fn compare_exchange(own, baseline, config) -> Vec<Anomaly>`
- `async fn send_discord(client, webhook_url, message)`

### Step 4: Wire up in main.rs
- Read config at startup
- If validator config exists, spawn validator task

### Step 5: Create config.toml.example
- Example config file for reference

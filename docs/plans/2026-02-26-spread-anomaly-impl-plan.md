# Spread Anomaly Detection Script — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Create a Python CLI script that queries ClickHouse `ticker_snapshots` to detect symbols with abnormally high cross-exchange spreads using Modified Z-Score.

**Architecture:** Single-file Python script queries ClickHouse for raw bid/ask data per (symbol, exchange, minute), computes cross-exchange spreads in pandas via a self-merge, then applies Modified Z-Score per symbol to flag anomalies.

**Tech Stack:** Python 3, clickhouse-connect, pandas, scipy, tabulate, argparse

---

### Task 1: Create scripts directory and requirements file

**Files:**
- Create: `scripts/requirements.txt`

**Step 1: Create the requirements file**

```txt
clickhouse-connect>=0.7
pandas>=2.0
scipy>=1.11
tabulate>=0.9
```

**Step 2: Verify**

Run: `cat scripts/requirements.txt`
Expected: file contents shown above

**Step 3: Commit**

```bash
git add scripts/requirements.txt
git commit -m "chore: add scripts/requirements.txt for spread anomaly tool"
```

---

### Task 2: Build the ClickHouse query and data loading

**Files:**
- Create: `scripts/spread_anomaly.py`

**Step 1: Write the script skeleton with CLI args and ClickHouse query**

The script should:
1. Parse CLI args: `--hours` (default 24), `--threshold` (default 3.5), `--top` (default 20), `--clickhouse-url` (default from config.toml pattern)
2. Connect to ClickHouse using `clickhouse_connect`
3. Query `ticker_snapshots` aggregated to 1-minute resolution: for each (symbol, exchange, minute), get the avg bid and avg ask
4. Return as a pandas DataFrame

```python
#!/usr/bin/env python3
"""Detect symbols with abnormally high cross-exchange spreads using Modified Z-Score."""

import argparse
import sys

import clickhouse_connect
import pandas as pd
from scipy.stats import median_abs_deviation
from tabulate import tabulate


def parse_args():
    p = argparse.ArgumentParser(description="Detect abnormal cross-exchange spread symbols")
    p.add_argument("--hours", type=int, default=24, help="Lookback hours (default: 24)")
    p.add_argument("--threshold", type=float, default=3.5, help="Modified Z-Score threshold (default: 3.5)")
    p.add_argument("--top", type=int, default=20, help="Show top N results (default: 20)")
    p.add_argument("--clickhouse-url", default="localhost", help="ClickHouse host (default: localhost)")
    p.add_argument("--clickhouse-port", type=int, default=8123, help="ClickHouse HTTP port (default: 8123)")
    p.add_argument("--clickhouse-user", default="admin", help="ClickHouse user (default: admin)")
    p.add_argument("--clickhouse-password", default="oweklx513ake", help="ClickHouse password")
    return p.parse_args()


def load_data(client, hours: int) -> pd.DataFrame:
    """Query ClickHouse for per-minute aggregated bid/ask per (symbol, exchange)."""
    query = f"""
    SELECT
        toStartOfMinute(ts) AS minute,
        symbol,
        exchange,
        avg(bid) AS bid,
        avg(ask) AS ask
    FROM ticker_snapshots
    WHERE ts >= now() - INTERVAL {hours} HOUR
      AND bid > 0 AND ask > 0
    GROUP BY minute, symbol, exchange
    ORDER BY minute, symbol, exchange
    """
    result = client.query(query)
    df = pd.DataFrame(result.result_rows, columns=["minute", "symbol", "exchange", "bid", "ask"])
    return df
```

**Step 2: Run to verify it parses without error**

Run: `cd /Users/wuguanxing/Desktop/workspace/exdata && python3 -c "import ast; ast.parse(open('scripts/spread_anomaly.py').read()); print('OK')"`
Expected: `OK`

---

### Task 3: Implement cross-exchange spread computation

**Files:**
- Modify: `scripts/spread_anomaly.py`

**Step 1: Add spread computation function**

After `load_data`, add a function that self-merges the dataframe on (symbol, minute) across different exchanges and computes spread:

```python
def compute_spreads(df: pd.DataFrame) -> pd.DataFrame:
    """Compute cross-exchange spread for every (symbol, minute, exchange_pair).

    For each pair, we take the best direction:
      spread = max(
        (ex_A.bid - ex_B.ask) / midpoint,
        (ex_B.bid - ex_A.ask) / midpoint
      ) * 100
    """
    # Self-merge: every pair of exchanges for same (symbol, minute)
    merged = df.merge(df, on=["symbol", "minute"], suffixes=("_a", "_b"))
    # Keep only pairs where exchange_a < exchange_b (avoid duplicates and self-joins)
    merged = merged[merged["exchange_a"] < merged["exchange_b"]].copy()

    if merged.empty:
        return pd.DataFrame(columns=["symbol", "minute", "exchange_a", "exchange_b", "spread_pct"])

    # Direction 1: long A (buy at A.ask), short B (sell at B.bid)
    mid1 = (merged["ask_a"] + merged["bid_b"]) / 2
    spread1 = (merged["bid_b"] - merged["ask_a"]) / mid1 * 100

    # Direction 2: long B (buy at B.ask), short A (sell at A.bid)
    mid2 = (merged["ask_b"] + merged["bid_a"]) / 2
    spread2 = (merged["bid_a"] - merged["ask_b"]) / mid2 * 100

    merged["spread_pct"] = pd.concat([spread1, spread2], axis=1).max(axis=1)

    # Determine which direction won for labeling
    merged["long_ex"] = merged.apply(
        lambda r: r["exchange_a"] if spread2.loc[r.name] >= spread1.loc[r.name] else r["exchange_b"],
        axis=1,
    )
    merged["short_ex"] = merged.apply(
        lambda r: r["exchange_b"] if spread2.loc[r.name] >= spread1.loc[r.name] else r["exchange_a"],
        axis=1,
    )

    # Filter out data errors: price ratio > 1.5x or spread > 20%
    max_price = merged[["bid_a", "ask_a", "bid_b", "ask_b"]].max(axis=1)
    min_price = merged[["bid_a", "ask_a", "bid_b", "ask_b"]].min(axis=1)
    valid = (max_price / min_price <= 1.5) & (merged["spread_pct"].abs() <= 20.0)
    merged = merged[valid].copy()

    return merged[["symbol", "minute", "exchange_a", "exchange_b", "long_ex", "short_ex", "spread_pct"]]
```

**Step 2: Verify syntax**

Run: `cd /Users/wuguanxing/Desktop/workspace/exdata && python3 -c "import ast; ast.parse(open('scripts/spread_anomaly.py').read()); print('OK')"`
Expected: `OK`

---

### Task 4: Implement Modified Z-Score anomaly detection

**Files:**
- Modify: `scripts/spread_anomaly.py`

**Step 1: Add anomaly detection function**

```python
def detect_anomalies(spreads: pd.DataFrame, threshold: float) -> pd.DataFrame:
    """Apply Modified Z-Score per symbol to find anomalous spread behavior.

    Modified Z-Score = 0.6745 * (x - median) / MAD
    Threshold: |MZ| > 3.5 is anomaly (Iglewicz & Hoaglin, 1993)
    """
    # For each symbol, compute stats across all its exchange-pair spreads
    symbol_stats = []

    for symbol, group in spreads.groupby("symbol"):
        values = group["spread_pct"].values
        if len(values) < 30:
            continue  # Insufficient data for reliable statistics

        med = float(pd.Series(values).median())
        mad = float(median_abs_deviation(values, scale=1.0))

        # Max spread and the exchange pair that produced it
        max_idx = group["spread_pct"].idxmax()
        max_spread = group.loc[max_idx, "spread_pct"]
        long_ex = group.loc[max_idx, "long_ex"]
        short_ex = group.loc[max_idx, "short_ex"]

        if mad > 0:
            modified_z = 0.6745 * (max_spread - med) / mad
        else:
            # MAD=0 fallback: use standard Z-Score
            std = float(pd.Series(values).std())
            if std > 0:
                modified_z = (max_spread - med) / std
            else:
                modified_z = 0.0

        symbol_stats.append({
            "symbol": symbol,
            "exchange_pair": f"{long_ex}→{short_ex}",
            "max_spread": round(max_spread, 4),
            "median_spread": round(med, 4),
            "mad": round(mad, 6),
            "modified_z": round(modified_z, 2),
            "data_points": len(values),
            "status": "ANOMALY" if modified_z > threshold else "normal",
        })

    result = pd.DataFrame(symbol_stats)
    if result.empty:
        return result
    return result.sort_values("modified_z", ascending=False).reset_index(drop=True)
```

**Step 2: Verify syntax**

Run: `cd /Users/wuguanxing/Desktop/workspace/exdata && python3 -c "import ast; ast.parse(open('scripts/spread_anomaly.py').read()); print('OK')"`
Expected: `OK`

---

### Task 5: Add main function and output formatting

**Files:**
- Modify: `scripts/spread_anomaly.py`

**Step 1: Add main function that ties everything together**

```python
def main():
    args = parse_args()

    print(f"Connecting to ClickHouse at {args.clickhouse_url}:{args.clickhouse_port}...")
    client = clickhouse_connect.get_client(
        host=args.clickhouse_url,
        port=args.clickhouse_port,
        username=args.clickhouse_user,
        password=args.clickhouse_password,
    )

    print(f"Querying last {args.hours} hours of ticker data...")
    df = load_data(client, args.hours)
    if df.empty:
        print("No data found in the specified time range.")
        sys.exit(0)

    n_symbols = df["symbol"].nunique()
    n_exchanges = df["exchange"].nunique()
    print(f"Loaded {len(df)} rows — {n_symbols} symbols across {n_exchanges} exchanges")

    print("Computing cross-exchange spreads...")
    spreads = compute_spreads(df)
    if spreads.empty:
        print("No valid cross-exchange spread pairs found.")
        sys.exit(0)

    print(f"Computed {len(spreads)} spread data points")
    print(f"Detecting anomalies (Modified Z-Score threshold={args.threshold})...\n")

    results = detect_anomalies(spreads, args.threshold)
    if results.empty:
        print("No symbols with sufficient data (min 30 data points).")
        sys.exit(0)

    # Split anomalies and normal
    anomalies = results[results["status"] == "ANOMALY"]

    # Display
    display_cols = ["symbol", "exchange_pair", "max_spread", "median_spread", "modified_z", "data_points", "status"]
    top_results = results.head(args.top)

    print(f"=== Top {min(args.top, len(top_results))} symbols by Modified Z-Score ===\n")
    print(tabulate(
        top_results[display_cols].values.tolist(),
        headers=["Symbol", "Exchange Pair", "Max Spread%", "Median Spread%", "Modified Z", "Data Pts", "Status"],
        tablefmt="simple",
        floatfmt=(".4f", "", ".4f", ".4f", ".2f", "d", ""),
    ))

    print(f"\n--- Summary ---")
    print(f"Total symbols analyzed: {len(results)}")
    print(f"Anomalies detected: {len(anomalies)}")
    if not anomalies.empty:
        print(f"Top anomaly: {anomalies.iloc[0]['symbol']} (Modified Z={anomalies.iloc[0]['modified_z']:.2f})")


if __name__ == "__main__":
    main()
```

**Step 2: Verify full script syntax**

Run: `cd /Users/wuguanxing/Desktop/workspace/exdata && python3 -c "import ast; ast.parse(open('scripts/spread_anomaly.py').read()); print('OK')"`
Expected: `OK`

---

### Task 6: Install dependencies and run end-to-end test

**Step 1: Install Python dependencies**

Run: `pip install clickhouse-connect pandas scipy tabulate`

**Step 2: Run the script against live ClickHouse**

Run: `cd /Users/wuguanxing/Desktop/workspace/exdata && python3 scripts/spread_anomaly.py --hours 1 --top 10`

Expected: Table output with spread anomalies (or "No data found" if ClickHouse is not running / empty).

**Step 3: If successful, run with full 24h**

Run: `cd /Users/wuguanxing/Desktop/workspace/exdata && python3 scripts/spread_anomaly.py --hours 24 --top 20`

---

### Task 7: Final commit

**Step 1: Commit the complete script**

```bash
git add scripts/spread_anomaly.py scripts/requirements.txt
git commit -m "feat: add spread anomaly detection script using Modified Z-Score"
```

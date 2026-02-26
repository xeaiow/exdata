#!/usr/bin/env python3
"""Detect abnormal cross-exchange cryptocurrency spreads.

Queries ClickHouse for recent ticker snapshots, computes pairwise
exchange spreads at 1-minute resolution, and flags statistically
anomalous symbols using a Modified Z-Score method.
"""

from __future__ import annotations

import argparse
import os
import sys

import clickhouse_connect
import pandas as pd
from scipy.stats import median_abs_deviation
from tabulate import tabulate


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Detect abnormal cross-exchange cryptocurrency spreads",
    )
    parser.add_argument(
        "--hours",
        type=int,
        default=24,
        help="Lookback hours (default: 24)",
    )
    parser.add_argument(
        "--threshold",
        type=float,
        default=3.5,
        help="Modified Z-Score threshold (default: 3.5)",
    )
    parser.add_argument(
        "--top",
        type=int,
        default=20,
        help="Show top N results (default: 20)",
    )
    parser.add_argument(
        "--clickhouse-url",
        type=str,
        default="localhost",
        help="ClickHouse host (default: localhost)",
    )
    parser.add_argument(
        "--clickhouse-port",
        type=int,
        default=8123,
        help="ClickHouse HTTP port (default: 8123)",
    )
    parser.add_argument(
        "--clickhouse-user",
        type=str,
        default="admin",
        help="ClickHouse user (default: admin)",
    )
    parser.add_argument(
        "--clickhouse-password",
        type=str,
        default=os.environ.get("CLICKHOUSE_PASSWORD", ""),
        help="ClickHouse password (default: $CLICKHOUSE_PASSWORD)",
    )
    return parser.parse_args()


# ---------------------------------------------------------------------------
# Data loading
# ---------------------------------------------------------------------------

def load_data(client, hours: int) -> pd.DataFrame:
    """Query ClickHouse ``ticker_snapshots`` and aggregate to 1-min bars."""
    query = (
        "SELECT "
        "  toStartOfMinute(ts) AS minute, "
        "  symbol, "
        "  exchange, "
        "  avg(bid) AS bid, "
        "  avg(ask) AS ask "
        "FROM ticker_snapshots "
        f"WHERE ts >= now() - INTERVAL {hours} HOUR "
        "  AND bid > 0 AND ask > 0 "
        "GROUP BY minute, symbol, exchange "
        "ORDER BY minute, symbol, exchange"
    )
    result = client.query(query)
    df = pd.DataFrame(
        result.result_rows,
        columns=["minute", "symbol", "exchange", "bid", "ask"],
    )
    return df


# ---------------------------------------------------------------------------
# Spread computation
# ---------------------------------------------------------------------------

def compute_spreads(df: pd.DataFrame) -> pd.DataFrame:
    """Self-merge on (symbol, minute) to get all exchange pair spreads."""
    merged = df.merge(df, on=["symbol", "minute"], suffixes=("_a", "_b"))

    # Keep only ordered pairs (avoid duplicates and self-joins)
    merged = merged[merged["exchange_a"] < merged["exchange_b"]].copy()

    if merged.empty:
        return pd.DataFrame(
            columns=[
                "symbol", "minute", "exchange_a", "exchange_b",
                "long_ex", "short_ex", "spread_pct",
            ]
        )

    # Midpoint of all four price legs
    midpoint = (
        merged["bid_a"] + merged["ask_a"] + merged["bid_b"] + merged["ask_b"]
    ) / 4.0

    # Direction 1: long A (buy at A.ask), short B (sell at B.bid)
    spread1 = (merged["bid_b"] - merged["ask_a"]) / midpoint * 100.0
    # Direction 2: long B (buy at B.ask), short A (sell at A.bid)
    spread2 = (merged["bid_a"] - merged["ask_b"]) / midpoint * 100.0

    # Best direction
    merged["spread_pct"] = pd.concat([spread1, spread2], axis=1).max(axis=1)

    # Determine long/short exchange labels based on winning direction
    dir2_wins = spread2 > spread1
    merged["long_ex"] = merged["exchange_a"].where(~dir2_wins, merged["exchange_b"])
    merged["short_ex"] = merged["exchange_b"].where(~dir2_wins, merged["exchange_a"])

    # Sanity filters: price ratio <= 1.5x, |spread| <= 20%
    price_ratio = (
        merged[["ask_a", "ask_b"]].max(axis=1)
        / merged[["ask_a", "ask_b"]].min(axis=1)
    )
    merged = merged[
        (price_ratio <= 1.5) & (merged["spread_pct"].abs() <= 20.0)
    ].copy()

    return merged[
        ["symbol", "minute", "exchange_a", "exchange_b",
         "long_ex", "short_ex", "spread_pct"]
    ].reset_index(drop=True)


# ---------------------------------------------------------------------------
# Anomaly detection
# ---------------------------------------------------------------------------

def detect_anomalies(spreads: pd.DataFrame, threshold: float) -> pd.DataFrame:
    """Flag symbols whose max spread is anomalous via Modified Z-Score."""
    records: list[dict] = []

    for symbol, group in spreads.groupby("symbol"):
        if len(group) < 30:
            continue

        values = group["spread_pct"].values
        median = float(pd.Series(values).median())
        mad = float(median_abs_deviation(values, scale=1.0))

        max_idx = group["spread_pct"].idxmax()
        max_spread = float(group.loc[max_idx, "spread_pct"])
        long_ex = group.loc[max_idx, "long_ex"]
        short_ex = group.loc[max_idx, "short_ex"]

        if mad > 0:
            modified_z = 0.6745 * (max_spread - median) / mad
        else:
            std = float(pd.Series(values).std())
            if std > 0:
                modified_z = (max_spread - median) / std
            else:
                modified_z = 0.0

        status = "ANOMALY" if abs(modified_z) > threshold else "normal"

        records.append({
            "symbol": symbol,
            "median_spread": round(median, 4),
            "mad": round(mad, 4),
            "max_spread": round(max_spread, 4),
            "long_ex": long_ex,
            "short_ex": short_ex,
            "modified_z": round(modified_z, 4),
            "status": status,
        })

    result = pd.DataFrame(records)
    if not result.empty:
        result = result.sort_values("modified_z", ascending=False).reset_index(
            drop=True
        )
    return result


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main() -> None:
    args = parse_args()

    print(f"[1/4] Connecting to ClickHouse at {args.clickhouse_url}:{args.clickhouse_port} ...")
    try:
        client = clickhouse_connect.get_client(
            host=args.clickhouse_url,
            port=args.clickhouse_port,
            username=args.clickhouse_user,
            password=args.clickhouse_password,
        )
    except Exception as exc:
        print(f"Failed to connect to ClickHouse: {exc}", file=sys.stderr)
        sys.exit(1)

    print(f"[2/4] Loading last {args.hours}h of ticker snapshots ...")
    df = load_data(client, args.hours)
    if df.empty:
        print("No data found. Exiting.")
        sys.exit(0)
    print(f"       {len(df)} rows loaded.")

    print("[3/4] Computing pairwise exchange spreads ...")
    spreads = compute_spreads(df)
    if spreads.empty:
        print("No cross-exchange pairs found. Exiting.")
        sys.exit(0)
    print(f"       {len(spreads)} spread observations across "
          f"{spreads['symbol'].nunique()} symbols.")

    print(f"[4/4] Detecting anomalies (threshold={args.threshold}) ...")
    anomalies = detect_anomalies(spreads, args.threshold)
    if anomalies.empty:
        print("No symbols with sufficient data (>=30 points). Exiting.")
        sys.exit(0)

    top = anomalies.head(args.top)
    print()
    print(tabulate(top, headers="keys", tablefmt="simple", showindex=False))
    print()

    total_symbols = len(anomalies)
    anomaly_count = int((anomalies["status"] == "ANOMALY").sum())
    top_row = anomalies.iloc[0]
    print(f"Summary: {total_symbols} symbols analysed, "
          f"{anomaly_count} anomalies detected.")
    print(f"Top anomaly: {top_row['symbol']} "
          f"(z={top_row['modified_z']:.2f}, "
          f"spread={top_row['max_spread']:.4f}%, "
          f"long={top_row['long_ex']}, short={top_row['short_ex']})")


if __name__ == "__main__":
    main()

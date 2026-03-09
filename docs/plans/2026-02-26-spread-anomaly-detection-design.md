# Spread Anomaly Detection CLI Script

**Date**: 2026-02-26
**Status**: Approved

## Goal

Create a Python CLI script that queries ClickHouse `ticker_snapshots` data to detect symbols with abnormally high cross-exchange arbitrage spreads using the Modified Z-Score method.

## Algorithm: Modified Z-Score (Iglewicz & Hoaglin, 1993)

### Formula

```
Modified Z-Score = 0.6745 × (x - median) / MAD
```

Where:
- `x` = observed spread value
- `median` = median of all spread values for that symbol
- `MAD` = Median Absolute Deviation = median(|x_i - median|)
- `0.6745` = consistency constant (0.6745 ≈ Q(0.75) of standard normal distribution)

### Why Modified Z-Score

- **Robust to outliers**: Unlike standard Z-Score, the median and MAD are not affected by extreme values
- **Suitable for fat-tailed distributions**: Financial spread data typically has heavier tails than normal distribution
- **Well-established**: Widely cited in statistical literature for outlier detection
- **Simple threshold**: |Modified Z| > 3.5 is the standard anomaly threshold

### Edge Cases

- **MAD = 0**: When most spreads are identical, fallback to standard Z-Score
- **Insufficient data**: Require minimum 30 data points per symbol for reliable statistics

## Data Pipeline

### Step 1: Query ClickHouse

For each symbol, compute cross-exchange spread from `ticker_snapshots`:

```sql
-- For each (symbol, minute), find the max cross-exchange spread
-- spread = (exchange_A.bid - exchange_B.ask) / midpoint * 100
-- Filter: price ratio < 1.5x, spread < 20% (data error exclusion)
```

Time range: configurable, default 24 hours.

### Step 2: Compute Modified Z-Score per Symbol

Group by symbol, calculate median spread and MAD, then score each symbol's max/mean spread against its distribution.

### Step 3: Output

Table sorted by Modified Z-Score descending:

| Symbol | Exchange Pair | Max Spread | Median Spread | Modified Z | Status |
|--------|--------------|-----------|--------------|-----------|--------|
| XYZUSDT | binance→bybit | 5.23% | 0.12% | 18.7 | ANOMALY |

## CLI Interface

```bash
python scripts/spread_anomaly.py \
  --hours 24 \          # Time range (default: 24)
  --threshold 3.5 \     # Modified Z-Score threshold (default: 3.5)
  --top 20              # Show top N results (default: 20)
```

## Dependencies

- `clickhouse-connect`: ClickHouse client
- `pandas`: Data processing
- `scipy.stats`: `median_abs_deviation` function
- `tabulate`: Table formatting

## File Location

`scripts/spread_anomaly.py`

## References

- Iglewicz, B. & Hoaglin, D.C. (1993). "Volume 16: How to Detect and Handle Outliers"
- Leys, C. et al. (2013). "Detecting outliers: Do not use standard deviation around the mean, use absolute deviation around the median"

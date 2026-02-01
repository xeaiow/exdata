# Validator Tolerance Relaxation

## Problem

Baseline validation produces too many false-positive alerts:
- `rate` uses exact string match, but small rounding differences (e.g., -0.043 vs -0.044) are acceptable
- `trade24Count` compares exchange trading volumes, which are inherently different across exchanges

## Changes

### 1. Remove `trade24Count` comparison
Delete the `trade24Count` percentage-diff check in `compare_item()`.

### 2. Rate/rateMax: absolute-difference tolerance
- Parse both rate strings to `f64`
- Alert only when `|exdata - baseline| > rate_threshold`
- New config field: `rate_threshold` (default `0.01`)

### 3. No changes to
- `a`, `b` (bid/ask) bps comparison
- `markPrice`, `indexPrice` bps comparison
- `rateInterval` exact match

## Config

```toml
[validator]
rate_threshold = 0.01   # new, default 0.01
```

## Files to modify

| File | Change |
|------|--------|
| `src/config.rs` | Add `rate_threshold: f64` field with default |
| `src/validator.rs` | Remove trade24Count block; change rate/rateMax to absolute-diff |
| `config.toml.example` | Add `rate_threshold` |

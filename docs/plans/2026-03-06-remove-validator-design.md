# Remove Validator Module

## Goal

Remove the entire validator module (baseline URL comparison feature) from the codebase.

## Changes

| File | Action |
|------|--------|
| `src/validator.rs` | Delete |
| `src/main.rs` | Remove `mod validator;` and spawn block |
| `src/config.rs` | Remove `ValidatorConfig`, `load_validator_config()`, validator field in `ConfigFile` |
| `config.toml.example` | Remove `[validator]` section |
| `Cargo.toml` | Remove `rand` dependency (only used by validator) |

## Not Changed

- `docs/plans/` historical design docs — kept for reference
- `RecorderConfig` and `load_recorder_config()` — unaffected

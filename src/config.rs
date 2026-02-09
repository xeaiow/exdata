use serde::Deserialize;

#[derive(Deserialize)]
struct ConfigFile {
    validator: Option<ValidatorConfig>,
    recorder: Option<RecorderConfig>,
}

#[derive(Clone, Deserialize)]
pub struct ValidatorConfig {
    pub baseline_url: String,
    #[serde(default)]
    pub discord_webhook_url: Option<String>,
    #[serde(default = "default_interval")]
    pub interval_secs: u64,
    #[serde(default = "default_price_threshold")]
    pub price_threshold_bps: f64,
    #[serde(default = "default_rate_threshold")]
    pub rate_threshold: f64,
}

#[derive(Clone, Deserialize)]
pub struct RecorderConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_clickhouse_url")]
    pub clickhouse_url: String,
}

fn default_clickhouse_url() -> String {
    "http://localhost:8123".to_string()
}

fn default_interval() -> u64 {
    60
}
fn default_price_threshold() -> f64 {
    50.0
}
fn default_rate_threshold() -> f64 {
    0.01
}

fn load_config_file() -> Option<ConfigFile> {
    let content = match std::fs::read_to_string("config.toml") {
        Ok(c) => c,
        Err(_) => return None,
    };
    match toml::from_str(&content) {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::warn!("config.toml parse error: {}", e);
            None
        }
    }
}

/// Try to load config.toml. Returns None if file missing or [validator] section absent.
pub fn load_validator_config() -> Option<ValidatorConfig> {
    let config = load_config_file()?;
    match config.validator {
        Some(v) => {
            tracing::info!(
                "validator enabled: baseline={}, interval={}s, price_threshold={}bps",
                v.baseline_url,
                v.interval_secs,
                v.price_threshold_bps,
            );
            Some(v)
        }
        None => {
            tracing::info!("no [validator] section in config.toml, validator disabled");
            None
        }
    }
}

/// Load recorder config from config.toml. Returns None if disabled or missing.
pub fn load_recorder_config() -> Option<RecorderConfig> {
    let config = load_config_file()?;
    match config.recorder {
        Some(r) if r.enabled => {
            tracing::info!("recorder enabled: clickhouse_url={}", r.clickhouse_url);
            Some(r)
        }
        _ => {
            tracing::info!("recorder disabled");
            None
        }
    }
}

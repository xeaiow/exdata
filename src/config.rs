use serde::Deserialize;

#[derive(Deserialize)]
struct ConfigFile {
    validator: Option<ValidatorConfig>,
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
    #[serde(default = "default_volume_threshold")]
    pub volume_threshold_pct: f64,
}

fn default_interval() -> u64 {
    60
}
fn default_price_threshold() -> f64 {
    50.0
}
fn default_volume_threshold() -> f64 {
    10.0
}

/// Try to load config.toml. Returns None if file missing or [validator] section absent.
pub fn load_validator_config() -> Option<ValidatorConfig> {
    let content = match std::fs::read_to_string("config.toml") {
        Ok(c) => c,
        Err(_) => {
            tracing::info!("config.toml not found, validator disabled");
            return None;
        }
    };

    let config: ConfigFile = match toml::from_str(&content) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("config.toml parse error: {}, validator disabled", e);
            return None;
        }
    };

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

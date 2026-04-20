use serde::Deserialize;

#[derive(Deserialize)]
struct ConfigFile {
    recorder: Option<RecorderConfig>,
}

#[derive(Clone, Deserialize)]
pub struct RecorderConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_clickhouse_url")]
    pub clickhouse_url: String,
    #[serde(default = "default_clickhouse_user")]
    pub clickhouse_user: String,
    #[serde(default)]
    pub clickhouse_password: String,
}

fn default_clickhouse_url() -> String {
    "http://localhost:8123".to_string()
}

fn default_clickhouse_user() -> String {
    "default".to_string()
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

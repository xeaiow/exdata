use serde::Serialize;
use std::collections::HashMap;

fn is_zero(v: &u64) -> bool {
    *v == 0
}

#[derive(Clone, Default, Serialize)]
pub struct PriceLevel {
    pub price: f64,
    pub qty: f64,
}

/// Unified item for both Spot and Future data.
/// Optional fields are skipped in JSON when None.
#[derive(Clone, Default, Serialize)]
pub struct ExchangeItem {
    pub name: String,
    /// Timestamp (ms) when this item's bid/ask was last updated.
    pub ts: u64,
    /// Timestamp (ms) when this item's depth (asks/bids) was last updated.
    #[serde(rename = "depthTs", skip_serializing_if = "is_zero")]
    pub depth_ts: u64,
    pub a: f64,
    pub b: f64,
    #[serde(rename = "trade24Count")]
    pub trade24_count: f64,
    #[serde(rename = "rateInterval", skip_serializing_if = "Option::is_none")]
    pub rate_interval: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate: Option<String>,
    #[serde(rename = "rateMax", skip_serializing_if = "Option::is_none")]
    pub rate_max: Option<String>,
    #[serde(rename = "indexPrice", skip_serializing_if = "Option::is_none")]
    pub index_price: Option<String>,
    #[serde(rename = "markPrice", skip_serializing_if = "Option::is_none")]
    pub mark_price: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub asks: Vec<PriceLevel>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub bids: Vec<PriceLevel>,
}

/// Internal storage for one exchange section.
pub struct ExchangeSection {
    pub ts: u64,
    pub items: HashMap<String, ExchangeItem>,
    /// Pre-serialized JSON for this section, updated on every write.
    pub cached_json: String,
    /// Whether items have been modified since the last serialize_cache() call.
    pub dirty: bool,
    /// Set to true while the coordinator is restarting (aborting workers → re-seeding).
    /// Validator skips this section when true to avoid false disconnect alerts.
    pub restarting: bool,
}

impl ExchangeSection {
    pub fn new() -> Self {
        let mut s = Self {
            ts: 0,
            items: HashMap::new(),
            cached_json: String::new(),
            dirty: false,
            restarting: true,
        };
        s.serialize_cache();
        s
    }

    /// Clear all items and reset timestamp, then re-serialize.
    /// Call on WS disconnect to prevent stale data from being served.
    pub fn clear(&mut self) {
        self.ts = 0;
        self.items.clear();
        self.dirty = false;
        self.serialize_cache();
    }

    /// Re-serialize the section to JSON. Call after every batch of updates.
    pub fn serialize_cache(&mut self) {
        #[derive(Serialize)]
        struct Resp<'a> {
            ts: u64,
            list: Vec<&'a ExchangeItem>,
        }
        let resp = Resp {
            ts: self.ts,
            list: self.items.values().collect(),
        };
        self.cached_json = serde_json::to_string(&resp).unwrap_or_default();
    }
}

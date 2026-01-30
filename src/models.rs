use serde::Serialize;
use std::collections::HashMap;

/// Unified item for both Spot and Future data.
/// Optional fields are skipped in JSON when None.
#[derive(Clone, Default, Serialize)]
pub struct ExchangeItem {
    pub name: String,
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
}

/// Internal storage for one exchange section.
pub struct ExchangeSection {
    pub ts: u64,
    pub items: HashMap<String, ExchangeItem>,
}

impl ExchangeSection {
    pub fn new() -> Self {
        Self {
            ts: 0,
            items: HashMap::new(),
        }
    }

    pub fn to_response(&self) -> ExchangeSectionResponse {
        ExchangeSectionResponse {
            ts: self.ts,
            list: self.items.values().cloned().collect(),
        }
    }
}

/// Serializable response for one exchange section.
#[derive(Serialize)]
pub struct ExchangeSectionResponse {
    pub ts: u64,
    pub list: Vec<ExchangeItem>,
}

/// Top-level API response.
#[derive(Serialize)]
pub struct ApiResponse {
    pub code: u32,
    pub data: ApiData,
}

#[derive(Serialize)]
pub struct ApiData {
    #[serde(rename = "binanceSpot")]
    pub binance_spot: ExchangeSectionResponse,
    #[serde(rename = "binanceFuture")]
    pub binance_future: ExchangeSectionResponse,
    #[serde(rename = "bybitSpot")]
    pub bybit_spot: ExchangeSectionResponse,
    #[serde(rename = "bybitFuture")]
    pub bybit_future: ExchangeSectionResponse,
    #[serde(rename = "okxSpot")]
    pub okx_spot: ExchangeSectionResponse,
    #[serde(rename = "okxFuture")]
    pub okx_future: ExchangeSectionResponse,
    #[serde(rename = "gateSpot")]
    pub gate_spot: ExchangeSectionResponse,
    #[serde(rename = "gateFuture")]
    pub gate_future: ExchangeSectionResponse,
    #[serde(rename = "bitgetSpot")]
    pub bitget_spot: ExchangeSectionResponse,
    #[serde(rename = "bitgetFuture")]
    pub bitget_future: ExchangeSectionResponse,
}

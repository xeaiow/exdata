use crate::models::ExchangeSection;
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct Cache {
    pub binance_future: RwLock<ExchangeSection>,
    pub bybit_future: RwLock<ExchangeSection>,
    pub okx_future: RwLock<ExchangeSection>,
    pub gate_future: RwLock<ExchangeSection>,
    pub bitget_future: RwLock<ExchangeSection>,
}

impl Cache {
    pub fn new() -> Self {
        Self {
            binance_future: RwLock::new(ExchangeSection::new()),
            bybit_future: RwLock::new(ExchangeSection::new()),
            okx_future: RwLock::new(ExchangeSection::new()),
            gate_future: RwLock::new(ExchangeSection::new()),
            bitget_future: RwLock::new(ExchangeSection::new()),
        }
    }
}

pub type SharedCache = Arc<Cache>;

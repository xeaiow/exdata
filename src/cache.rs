use crate::models::ExchangeSection;
use crate::spread::TickerChanged;
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast};

pub struct Cache {
    pub binance_future: RwLock<ExchangeSection>,
    pub bybit_future: RwLock<ExchangeSection>,
    pub okx_future: RwLock<ExchangeSection>,
    pub gate_future: RwLock<ExchangeSection>,
    pub bitget_future: RwLock<ExchangeSection>,
    pub zoomex_future: RwLock<ExchangeSection>,
    pub ticker_tx: broadcast::Sender<TickerChanged>,
}

impl Cache {
    pub fn new(ticker_tx: broadcast::Sender<TickerChanged>) -> Self {
        Self {
            binance_future: RwLock::new(ExchangeSection::new()),
            bybit_future: RwLock::new(ExchangeSection::new()),
            okx_future: RwLock::new(ExchangeSection::new()),
            gate_future: RwLock::new(ExchangeSection::new()),
            bitget_future: RwLock::new(ExchangeSection::new()),
            zoomex_future: RwLock::new(ExchangeSection::new()),
            ticker_tx,
        }
    }
}

pub type SharedCache = Arc<Cache>;

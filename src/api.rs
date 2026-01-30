use axum::{extract::State, Json};
use crate::cache::SharedCache;
use crate::models::{ApiResponse, ApiData};

pub async fn get_exdata(State(cache): State<SharedCache>) -> Json<ApiResponse> {
    let data = ApiData {
        binance_spot: cache.binance_spot.read().await.to_response(),
        binance_future: cache.binance_future.read().await.to_response(),
        bybit_spot: cache.bybit_spot.read().await.to_response(),
        bybit_future: cache.bybit_future.read().await.to_response(),
        okx_spot: cache.okx_spot.read().await.to_response(),
        okx_future: cache.okx_future.read().await.to_response(),
        gate_spot: cache.gate_spot.read().await.to_response(),
        gate_future: cache.gate_future.read().await.to_response(),
        bitget_spot: cache.bitget_spot.read().await.to_response(),
        bitget_future: cache.bitget_future.read().await.to_response(),
    };

    Json(ApiResponse { code: 0, data })
}

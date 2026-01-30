use axum::{extract::State, Json};
use crate::cache::SharedCache;
use crate::models::{ApiResponse, ApiData};

pub async fn get_exdata(State(cache): State<SharedCache>) -> Json<ApiResponse> {
    let (
        binance_spot,
        binance_future,
        bybit_spot,
        bybit_future,
        okx_spot,
        okx_future,
        gate_spot,
        gate_future,
        bitget_spot,
        bitget_future,
    ) = tokio::join!(
        async { cache.binance_spot.read().await.to_response() },
        async { cache.binance_future.read().await.to_response() },
        async { cache.bybit_spot.read().await.to_response() },
        async { cache.bybit_future.read().await.to_response() },
        async { cache.okx_spot.read().await.to_response() },
        async { cache.okx_future.read().await.to_response() },
        async { cache.gate_spot.read().await.to_response() },
        async { cache.gate_future.read().await.to_response() },
        async { cache.bitget_spot.read().await.to_response() },
        async { cache.bitget_future.read().await.to_response() },
    );

    let data = ApiData {
        binance_spot,
        binance_future,
        bybit_spot,
        bybit_future,
        okx_spot,
        okx_future,
        gate_spot,
        gate_future,
        bitget_spot,
        bitget_future,
    };

    Json(ApiResponse { code: 0, data })
}

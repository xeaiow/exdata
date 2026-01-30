use axum::extract::State;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use crate::cache::SharedCache;

pub async fn get_exdata(State(cache): State<SharedCache>) -> Response {
    let (bs, bf, bys, byf, os, of, gs, gf, bgs, bgf) = tokio::join!(
        async { cache.binance_spot.read().await.cached_json.clone() },
        async { cache.binance_future.read().await.cached_json.clone() },
        async { cache.bybit_spot.read().await.cached_json.clone() },
        async { cache.bybit_future.read().await.cached_json.clone() },
        async { cache.okx_spot.read().await.cached_json.clone() },
        async { cache.okx_future.read().await.cached_json.clone() },
        async { cache.gate_spot.read().await.cached_json.clone() },
        async { cache.gate_future.read().await.cached_json.clone() },
        async { cache.bitget_spot.read().await.cached_json.clone() },
        async { cache.bitget_future.read().await.cached_json.clone() },
    );

    let cap = 150 + bs.len() + bf.len() + bys.len() + byf.len()
        + os.len() + of.len() + gs.len() + gf.len() + bgs.len() + bgf.len();
    let mut body = String::with_capacity(cap);
    body.push_str(r#"{"code":0,"data":{"binanceSpot":"#);
    body.push_str(&bs);
    body.push_str(r#","binanceFuture":"#);
    body.push_str(&bf);
    body.push_str(r#","bybitSpot":"#);
    body.push_str(&bys);
    body.push_str(r#","bybitFuture":"#);
    body.push_str(&byf);
    body.push_str(r#","okxSpot":"#);
    body.push_str(&os);
    body.push_str(r#","okxFuture":"#);
    body.push_str(&of);
    body.push_str(r#","gateSpot":"#);
    body.push_str(&gs);
    body.push_str(r#","gateFuture":"#);
    body.push_str(&gf);
    body.push_str(r#","bitgetSpot":"#);
    body.push_str(&bgs);
    body.push_str(r#","bitgetFuture":"#);
    body.push_str(&bgf);
    body.push_str("}}");

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, HeaderValue::from_static("application/json"))],
        body,
    ).into_response()
}

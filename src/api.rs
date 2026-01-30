use axum::extract::State;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use crate::cache::SharedCache;

pub async fn get_exdata(State(cache): State<SharedCache>) -> Response {
    let (bf, byf, of, gf, bgf, zf) = tokio::join!(
        async { cache.binance_future.read().await.cached_json.clone() },
        async { cache.bybit_future.read().await.cached_json.clone() },
        async { cache.okx_future.read().await.cached_json.clone() },
        async { cache.gate_future.read().await.cached_json.clone() },
        async { cache.bitget_future.read().await.cached_json.clone() },
        async { cache.zoomex_future.read().await.cached_json.clone() },
    );

    let cap = 120 + bf.len() + byf.len() + of.len() + gf.len() + bgf.len() + zf.len();
    let mut body = String::with_capacity(cap);
    body.push_str(r#"{"code":0,"data":{"binanceFuture":"#);
    body.push_str(&bf);
    body.push_str(r#","bybitFuture":"#);
    body.push_str(&byf);
    body.push_str(r#","okxFuture":"#);
    body.push_str(&of);
    body.push_str(r#","gateFuture":"#);
    body.push_str(&gf);
    body.push_str(r#","bitgetFuture":"#);
    body.push_str(&bgf);
    body.push_str(r#","zoomexFuture":"#);
    body.push_str(&zf);
    body.push_str("}}");

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("application/json")),
            (header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=1")),
        ],
        body,
    ).into_response()
}

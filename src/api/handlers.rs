use crate::api::state::AppState;
use crate::api::templates;
use crate::metrics::counters::get_snapshot_json;
use crate::trading::jito::{jito_prometheus_metrics, trigger_jito_emergency_stop};
use crate::trading::runtime_gate::{gate_snapshot, latency_snapshot, runtime_prometheus_metrics};
use axum::response::sse::{Event, KeepAlive};
use axum::{
    extract::State,
    http::{header, HeaderMap, Method, Request, StatusCode},
    middleware::Next,
    response::{Html, IntoResponse, Response, Sse},
    Json,
};
use base64::{engine::general_purpose, Engine as _};
use futures::stream::{self, Stream, StreamExt};
use std::convert::Infallible;
use tokio_stream::wrappers::BroadcastStream;

fn accepts_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("application/json"))
        .unwrap_or(false)
}

pub async fn auth_middleware(
    State(state): State<AppState>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    // Let CORS preflight pass through; protected routes still require auth.
    if req.method() == Method::OPTIONS {
        return next.run(req).await;
    }

    let auth_header = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());

    if let Some(auth_header) = auth_header {
        if let Some(credentials) = auth_header.strip_prefix("Basic ") {
            if let Ok(decoded) = general_purpose::STANDARD.decode(credentials) {
                if let Ok(cred_str) = String::from_utf8(decoded) {
                    if let Some((username, password)) = cred_str.split_once(':') {
                        if username == state.config.username && password == state.config.password {
                            return next.run(req).await;
                        }
                    }
                }
            }
        }
    }

    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"Aether Sniper\"")],
        "Unauthorized",
    )
        .into_response()
}

pub async fn get_dashboard(State(state): State<AppState>) -> Html<String> {
    let metrics = state.cached_metrics().await;
    let positions_owned = state.cached_positions().await;

    let content = templates::dashboard_view(&metrics, &positions_owned);
    Html(content.into_string())
}

pub async fn get_stats(headers: HeaderMap, State(state): State<AppState>) -> Response {
    let metrics = state.cached_metrics().await;
    if accepts_json(&headers) {
        Json(serde_json::to_value(&metrics).unwrap_or_default()).into_response()
    } else {
        let content = templates::stats_component(&metrics);
        Html(content.into_string()).into_response()
    }
}

pub async fn get_positions(headers: HeaderMap, State(state): State<AppState>) -> Response {
    let positions_owned = state.cached_positions().await;
    if accepts_json(&headers) {
        Json(serde_json::to_value(&positions_owned).unwrap_or_default()).into_response()
    } else {
        let content = templates::positions_component(&positions_owned);
        Html(content.into_string()).into_response()
    }
}

pub async fn get_metrics() -> impl IntoResponse {
    let mut body = jito_prometheus_metrics();
    body.push('\n');
    body.push_str(&runtime_prometheus_metrics());
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], body)
}

pub async fn emergency_stop() -> impl IntoResponse {
    trigger_jito_emergency_stop("manual API emergency_stop");
    (StatusCode::OK, "emergency_stop=true")
}

pub async fn sse_handler(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.event_tx.subscribe();
    let stream = BroadcastStream::new(rx);

    let stream = stream.flat_map(|result| {
        match result {
            Ok(event) => {
                match event {
                    crate::types::StreamEvent::NewPool(pool, latency) => {
                        let internal_lag = if latency.ws_receive_us > 0 && latency.pool_parsed_us > latency.ws_receive_us {
                             latency.pool_parsed_us.saturating_sub(latency.ws_receive_us)
                        } else {
                             latency.pool_parsed_us
                        };
                        let lat_class = if internal_lag < 200 { "latency-good" } else if internal_lag < 5000 { "latency-warn" } else { "latency-bad" };

                        // We need to update templates::pool_list_item sig or just format it here.
                        // For speed, let's just format it here to match PumpFun style or update template.
                        // Actually, updating template is cleaner but changing signature might break other calls.
                        // Let's modify templates.rs one more time to accept style class? No, templates.rs logic for lat string is there.
                        // Let's rewrite the logic in templates.rs to be smarter.

                        let html = templates::pool_list_item(&pool, Some(&latency), lat_class).into_string();
                        let log_event = Event::default().event("log").data(html);
                        let stats_event = Event::default().event("stats").data("");
                        stream::iter(vec![Ok(log_event), Ok(stats_event)])
                    }
                    crate::types::StreamEvent::NewPumpFunToken(token, latency) => {
                        let internal_lag = if latency.ws_receive_us > 0 && latency.pool_parsed_us > latency.ws_receive_us {
                             latency.pool_parsed_us.saturating_sub(latency.ws_receive_us)
                        } else {
                             latency.pool_parsed_us
                        };

                        let lat_str = format!("{}us", internal_lag);
                        let lat_class = if internal_lag < 200 { "latency-good" } else if internal_lag < 5000 { "latency-warn" } else { "latency-bad" };

                        let html = format!(
                            "<li><span class='timestamp'>{}</span> [PUMP.FUN] <span class='mint'>{}...</span> (Slot: {}) <span class='{}'>[{}]</span></li>",
                            token.detected_at.format("%H:%M:%S.%3f"),
                            token.mint.to_string().get(0..8).unwrap_or("????????"),
                            token.slot,
                            lat_class,
                            lat_str
                        );
                        let log_event = Event::default().event("log").data(html);
                        let stats_event = Event::default().event("stats").data("");
                        stream::iter(vec![Ok(log_event), Ok(stats_event)])
                    }
                    crate::types::StreamEvent::PriceUpdate { .. } => {
                        let stats_event = Event::default().event("stats").data("");
                        let positions_event = Event::default().event("positions").data("");
                        stream::iter(vec![Ok(stats_event), Ok(positions_event)])
                    }
                    crate::types::StreamEvent::Error(e) => {
                         let html = format!("<li><span class='val-down'>Error: {}</span></li>", e);
                         let log_event = Event::default().event("log").data(html);
                         stream::iter(vec![Ok(log_event)])
                    }
                    // FIXED: Handle other variants exhaustively
                    _ => {
                        // For other events (TradeExecuted etc), we can add logs later or ignore for now to fix compile error
                        stream::iter(vec![])
                    }
                }
            }
            Err(_) => {
                stream::iter(vec![])
            },
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}

pub async fn sse_json_handler(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.event_tx.subscribe();
    let stream = BroadcastStream::new(rx);

    let stream = stream.filter_map(|result| async move {
        match result {
            Ok(event) => {
                let json = match &event {
                    crate::types::StreamEvent::NewPool(pool, latency) => {
                        serde_json::json!({
                            "type": "new_pool",
                            "pool": pool,
                            "latency": latency,
                        })
                    }
                    crate::types::StreamEvent::NewPumpFunToken(token, latency) => {
                        serde_json::json!({
                            "type": "new_pumpfun_token",
                            "token": token,
                            "latency": latency,
                        })
                    }
                    crate::types::StreamEvent::PriceUpdate { pool_id, price_sol } => {
                        serde_json::json!({
                            "type": "price_update",
                            "pool_id": pool_id.to_string(),
                            "price_sol": price_sol,
                        })
                    }
                    crate::types::StreamEvent::TradeExecuted(result) => {
                        serde_json::json!({
                            "type": "trade_executed",
                            "trade": result,
                        })
                    }
                    crate::types::StreamEvent::TradeFailed { reason, pool_id } => {
                        serde_json::json!({
                            "type": "trade_failed",
                            "reason": reason,
                            "pool_id": pool_id.map(|p| p.to_string()),
                        })
                    }
                    crate::types::StreamEvent::Error(e) => {
                        serde_json::json!({
                            "type": "error",
                            "message": e,
                        })
                    }
                };
                let data = serde_json::to_string(&json).unwrap_or_default();
                Some(Ok(Event::default().event("message").data(data)))
            }
            Err(_) => None,
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}

pub async fn get_counters() -> impl IntoResponse {
    let json_str = get_snapshot_json();
    ([(header::CONTENT_TYPE, "application/json")], json_str)
}

pub async fn get_runtime() -> impl IntoResponse {
    let latency = latency_snapshot(None);
    let gate = gate_snapshot();
    Json(serde_json::json!({
        "latency": latency,
        "gate": gate,
    }))
}

pub async fn get_uptime(State(state): State<AppState>) -> impl IntoResponse {
    Json(serde_json::json!({
        "uptime_secs": state.uptime_secs(),
        "uptime_formatted": state.uptime_formatted(),
    }))
}

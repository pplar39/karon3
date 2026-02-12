use crate::api::handlers;
use crate::api::state::AppState;
use axum::http::{header, HeaderName, HeaderValue, Method};
use axum::{
    middleware,
    routing::{get, post},
    Router,
};
use tower_http::cors::{AllowHeaders, AllowMethods, AllowOrigin, CorsLayer};
use tower_http::services::ServeDir;

pub fn create_router(state: AppState) -> Router {
    let read_only_cors = CorsLayer::new()
        .allow_origin(AllowOrigin::list([
            HeaderValue::from_static("http://127.0.0.1:3030"),
            HeaderValue::from_static("http://localhost:3030"),
        ]))
        .allow_methods(AllowMethods::list([Method::GET, Method::OPTIONS]))
        .allow_headers(AllowHeaders::list([
            header::ACCEPT,
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            HeaderName::from_static("cache-control"),
        ]));

    // Read-only routes that may be fetched by dashboard SPA.
    let protected_read = Router::new()
        .route("/", get(handlers::get_dashboard))
        .route("/api/stats", get(handlers::get_stats))
        .route("/api/positions", get(handlers::get_positions))
        .route("/events", get(handlers::sse_handler))
        .route("/events/json", get(handlers::sse_json_handler))
        .route("/api/counters", get(handlers::get_counters))
        .route("/api/runtime", get(handlers::get_runtime))
        .route("/api/uptime", get(handlers::get_uptime))
        .layer(read_only_cors);

    let protected_control =
        Router::new().route("/api/emergency_stop", post(handlers::emergency_stop));

    let protected = protected_read
        .merge(protected_control)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            handlers::auth_middleware,
        ));

    let base = Router::new().route("/metrics", get(handlers::get_metrics));

    let router = if state.config.enabled {
        // Dashboard SPA only served when UI is enabled
        base.merge(protected)
            .nest_service("/dashboard", ServeDir::new("Luna_dashboard_v4/dist"))
    } else {
        // Keep emergency control and metrics exposed when UI is disabled.
        // No /dashboard, no other protected routes.
        base.route("/api/emergency_stop", post(handlers::emergency_stop))
    };

    router.with_state(state)
}

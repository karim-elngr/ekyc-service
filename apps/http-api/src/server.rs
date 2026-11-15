use crate::{auth::Clients, directory, interactive, product};
use axum::{
    extract::{DefaultBodyLimit, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use ekyc_postgres::Store;
use ekyc_restate_ingress::CaseCommands;
use serde_json::json;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

#[derive(Default)]
pub(crate) struct Metrics {
    pub(crate) accepted: AtomicU64,
    pub(crate) throttled: AtomicU64,
    pub(crate) dependency_failures: AtomicU64,
    pub(crate) reads: AtomicU64,
}
pub(crate) struct App {
    pub(crate) commands: CaseCommands,
    pub(crate) store: Store,
    pub(crate) clients: Clients,
    pub(crate) interactive: Option<interactive::MockProviders>,
    pub(crate) limit: u32,
    pub(crate) metrics: Metrics,
}
pub(crate) type Shared = Arc<App>;

pub(crate) fn error(status: StatusCode, code: &str) -> Response {
    (status, Json(json!({"error": code}))).into_response()
}
pub(crate) fn unavailable(app: &App) -> Response {
    app.metrics
        .dependency_failures
        .fetch_add(1, Ordering::Relaxed);
    error(StatusCode::SERVICE_UNAVAILABLE, "dependency_unavailable")
}
async fn reads_ready(State(app): State<Shared>) -> StatusCode {
    match app.store.ready().await {
        Ok(_) => StatusCode::OK,
        Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}
async fn ready(State(app): State<Shared>) -> StatusCode {
    let (database, commands) = tokio::join!(app.store.ready(), app.commands.ready());
    if database.is_ok() && commands {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}
async fn metrics(State(app): State<Shared>) -> impl IntoResponse {
    let m = &app.metrics;
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], format!(
        "ekyc_accepted_requests_total {}\nekyc_admission_throttled_total {}\nekyc_dependency_failures_total {}\nekyc_case_reads_total {}\n",
        m.accepted.load(Ordering::Relaxed), m.throttled.load(Ordering::Relaxed),
        m.dependency_failures.load(Ordering::Relaxed), m.reads.load(Ordering::Relaxed)))
}
pub(crate) fn router(state: Shared) -> Router {
    Router::new()
        .merge(product::routes())
        .merge(directory::routes())
        .merge(crate::staff::routes())
        .merge(interactive::routes())
        .route("/health/live", get(|| async { StatusCode::OK }))
        .route("/health/ready", get(ready))
        .route("/health/reads", get(reads_ready))
        .route("/metrics", get(metrics))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .with_state(state)
}

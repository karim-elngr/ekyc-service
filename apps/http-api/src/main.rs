mod auth;

use auth::{case_id, owned_by, Clients};
use axum::{
    extract::{rejection::JsonRejection, DefaultBodyLimit, Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use ekyc_domain::OpenCase;
use ekyc_postgres::{CaseView, Gate, Store};
use ekyc_restate_ingress::{CaseCommands, CommandError};
use serde_json::{json, Value};
use std::{
    env,
    error::Error,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

#[derive(Default)]
struct Metrics {
    accepted: AtomicU64,
    throttled: AtomicU64,
    dependency_failures: AtomicU64,
    reads: AtomicU64,
}
struct App {
    commands: CaseCommands,
    store: Store,
    clients: Clients,
    limit: u32,
    metrics: Metrics,
}
type Shared = Arc<App>;

fn error(status: StatusCode, code: &str) -> Response {
    (status, Json(json!({"error": code}))).into_response()
}
fn unavailable(app: &App) -> Response {
    app.metrics
        .dependency_failures
        .fetch_add(1, Ordering::Relaxed);
    error(StatusCode::SERVICE_UNAVAILABLE, "dependency_unavailable")
}
async fn open(
    State(app): State<Shared>,
    headers: HeaderMap,
    body: Result<Json<OpenCase>, JsonRejection>,
) -> Response {
    let Some(caller) = app.clients.authenticate(&headers) else {
        return error(StatusCode::UNAUTHORIZED, "unauthenticated");
    };
    let Some(key) = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .filter(|k| !k.is_empty() && k.len() <= 128 && k.bytes().all(|c| c.is_ascii_graphic()))
    else {
        return error(StatusCode::BAD_REQUEST, "invalid_idempotency_key");
    };
    match app.store.admit(&caller.id, app.limit).await {
        Ok(Gate::Granted) => (),
        Ok(Gate::Wait { retry_after_ms }) => {
            app.metrics.throttled.fetch_add(1, Ordering::Relaxed);
            let mut res = error(StatusCode::TOO_MANY_REQUESTS, "admission_rate_limited");
            let seconds = retry_after_ms.div_ceil(1000).max(1);
            if let Ok(value) = seconds.to_string().parse() {
                res.headers_mut().insert(header::RETRY_AFTER, value);
            }
            return res;
        }
        Ok(Gate::Closed) | Err(_) => return unavailable(&app),
    }
    let case = match body {
        Ok(Json(body)) => body,
        Err(err) => return error(err.status(), "invalid_request_body"),
    };
    if !caller.purposes.contains(&case.purpose) {
        return error(StatusCode::FORBIDDEN, "purpose_forbidden");
    }
    if case.validate().is_err() {
        return error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_case");
    }
    let id = case_id(&caller.id, key);
    match app.commands.open(&id, &case).await {
        Ok(()) => {
            app.metrics.accepted.fetch_add(1, Ordering::Relaxed);
            let location = format!("/v1/cases/{id}");
            (
                StatusCode::ACCEPTED,
                [(header::LOCATION, location.clone())],
                Json(json!({"case_id":id, "location":location})),
            )
                .into_response()
        }
        Err(CommandError::Conflict) => error(StatusCode::CONFLICT, "idempotency_key_reused"),
        Err(CommandError::Invalid) => error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_case"),
        Err(CommandError::Unavailable) => unavailable(&app),
    }
}

fn public_view(case: &CaseView) -> Value {
    let reasons: Vec<_> = case.decision.iter().flat_map(|decision| &decision.reasons)
        .map(|reason| json!({"check": reason.check, "code": reason.code, "message": reason.public_message()}))
        .collect();
    json!({"case_id":case.case_id, "purpose":case.purpose, "state":case.state,
        "scope":case.scope, "checks":case.checks, "version":case.version,
        "policy_version":case.policy_version, "decided_at":case.decided_at,
        "decision":case.decision.as_ref().map(|d| &d.verdict),
        "reasons":reasons})
}

async fn read(State(app): State<Shared>, Path(id): Path<String>, headers: HeaderMap) -> Response {
    let Some(caller) = app.clients.authenticate(&headers) else {
        return error(StatusCode::UNAUTHORIZED, "unauthenticated");
    };
    if !owned_by(&id, &caller.id) {
        return error(StatusCode::NOT_FOUND, "case_not_found");
    }
    match app.store.get(&id).await {
        Ok(Some(case)) => {
            app.metrics.reads.fetch_add(1, Ordering::Relaxed);
            Json(public_view(&case)).into_response()
        }
        Ok(None) => error(StatusCode::NOT_FOUND, "case_not_found"),
        Err(_) => unavailable(&app),
    }
}
async fn ready(State(app): State<Shared>) -> StatusCode {
    match app.store.ready().await {
        Ok(_) => StatusCode::OK,
        Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}
async fn metrics(State(app): State<Shared>) -> impl IntoResponse {
    let m = &app.metrics;
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], format!(
        "ekyc_accepted_requests_total {}\nekyc_admission_throttled_total {}\nekyc_dependency_failures_total {}\nekyc_case_reads_total {}\n",
        m.accepted.load(Ordering::Relaxed), m.throttled.load(Ordering::Relaxed),
        m.dependency_failures.load(Ordering::Relaxed), m.reads.load(Ordering::Relaxed)))
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt().with_target(false).init();
    let clients = Clients::parse(&env::var("EKYC_CLIENTS")?)?;
    let store = Store::connect(&env::var("DATABASE_URL")?).await?;
    let ingress = env::var("RESTATE_INGRESS_URL")?.parse::<reqwest::Url>()?;
    if !matches!(ingress.scheme(), "http" | "https") || ingress.host().is_none() {
        return Err("invalid Restate ingress URL".into());
    }
    let commands = CaseCommands::new(ingress, Duration::from_secs(8))?;
    let limit = env::var("ADMISSION_PER_MINUTE")
        .unwrap_or_else(|_| "60".into())
        .parse()?;
    if limit == 0 {
        return Err("admission quota must be positive".into());
    }
    let state = Arc::new(App {
        store,
        commands,
        clients,
        limit,
        metrics: Metrics::default(),
    });
    let app = Router::new()
        .route("/v1/cases", post(open))
        .route("/v1/cases/{id}", get(read))
        .route("/health/live", get(|| async { StatusCode::OK }))
        .route("/health/ready", get(ready))
        .route("/metrics", get(metrics))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .with_state(state);
    let addr: SocketAddr = env::var("API_BIND")
        .unwrap_or_else(|_| "0.0.0.0:8090".into())
        .parse()?;
    let socket = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "listening");
    axum::serve(socket, app).await?;
    Ok(())
}

#[cfg(test)]
mod view_tests {
    use super::*;

    #[test]
    fn query_exposes_stored_results_and_safe_messages() {
        let case: CaseView = serde_json::from_value(json!({
            "case_id":"case_42", "version":2, "state":"DECIDED","purpose":"address_refresh",
            "scope":["address"], "checks":[{"kind":"address","result":"REVIEW"}],
            "observations":{"address":{"kind":"address","passed":true,
                "confidence":75.34,"proof_date":"2026-11-01"}},
            "decision":{"verdict":"MANUAL_REVIEW","reasons":[
                {"check":"address","code":"ADDRESS_CONFIDENCE_LOW"}
            ]},
            "policy_version":"v1","evaluated_on":"2026-11-15","decided_at":"2026-11-15T13:00:00Z"
        }))
        .unwrap();
        let public = public_view(&case);
        assert_eq!(public["checks"][0]["result"], "REVIEW");
        assert_eq!(
            public["reasons"][0]["message"],
            "Address confidence did not exceed the required threshold."
        );
        assert!(!public.to_string().contains("75.34"));
        assert!(public.get("observations").is_none());
    }
}

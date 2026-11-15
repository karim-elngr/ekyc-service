use crate::{
    auth::{Access, Authenticated},
    error, unavailable, Shared,
};
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, put},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;

pub(super) fn routes() -> Router<Shared> {
    Router::new()
        .route("/v2/staff/session", get(session))
        .route(
            "/v2/staff/provider-routing",
            get(traffic_report).put(change_traffic),
        )
        .route("/v2/investigations", get(queue))
        .route("/v2/investigations/acknowledgement", put(acknowledge))
}

async fn session(Authenticated(caller): Authenticated) -> Response {
    let role = match caller.access {
        Access::Observer { .. } => "observer",
        Access::RoutingOperator { .. } => "routing_operator",
        Access::Reviewer { .. } => "reviewer",
        _ => return error(StatusCode::FORBIDDEN, "role_forbidden"),
    };
    let scope = caller.scope();
    Json(json!({"actor_id":caller.id,"role":role,"institution":scope.institution,"products":scope.products})).into_response()
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum QueueStatus {
    #[default]
    Open,
    Acknowledged,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Filter {
    #[serde(default)]
    status: QueueStatus,
    after: Option<String>,
    limit: Option<u32>,
}

async fn queue(
    State(app): State<Shared>,
    Authenticated(caller): Authenticated,
    query: Result<Query<Filter>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let Access::Reviewer {
        institution,
        product,
        ..
    } = &caller.access
    else {
        return error(StatusCode::FORBIDDEN, "role_forbidden");
    };
    let Ok(Query(query)) = query else {
        return error(StatusCode::BAD_REQUEST, "invalid_queue_filter");
    };
    let limit = query.limit.unwrap_or(25);
    if !(1..=100).contains(&limit) || query.after.as_ref().is_some_and(|v| !valid_id(v)) {
        return error(StatusCode::BAD_REQUEST, "invalid_queue_filter");
    }
    match app
        .store
        .investigations(
            institution,
            product,
            matches!(query.status, QueueStatus::Acknowledged),
            query.after.as_deref().unwrap_or(""),
            limit + 1,
        )
        .await
    {
        Ok(mut items) => {
            let more = items.len() > limit as usize;
            items.truncate(limit as usize);
            let after = items.last().filter(|_| more).map(|item| &item.work_id);
            Json(json!({"items":items,"next_after":after})).into_response()
        }
        Err(_) => unavailable(&app),
    }
}

fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 1024 && !value.chars().any(char::is_control)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ack {
    work_id: String,
}

async fn acknowledge(
    State(app): State<Shared>,
    Authenticated(caller): Authenticated,
    Json(body): Json<Ack>,
) -> Response {
    let Access::Reviewer {
        institution,
        product,
        ..
    } = &caller.access
    else {
        return error(StatusCode::FORBIDDEN, "role_forbidden");
    };
    if !valid_id(&body.work_id) {
        return error(StatusCode::BAD_REQUEST, "invalid_work_id");
    }
    match app
        .store
        .acknowledge_investigation(institution, product, &body.work_id, &caller.id)
        .await
    {
        Ok(Some(receipt)) => Json(receipt).into_response(),
        Ok(None) => error(StatusCode::NOT_FOUND, "work_not_found"),
        Err(_) => unavailable(&app),
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RoutingFilter {
    page: Option<u32>,
    case_id: Option<String>,
}

async fn traffic_report(
    State(app): State<Shared>,
    Authenticated(caller): Authenticated,
    Query(filter): Query<RoutingFilter>,
) -> Response {
    if matches!(caller.access, Access::Product { .. }) {
        return error(StatusCode::FORBIDDEN, "role_forbidden");
    }
    let page = filter.page.unwrap_or(1);
    if page == 0
        || filter
            .case_id
            .as_ref()
            .is_some_and(|id| id.len() > 128 || id.is_empty())
    {
        return error(StatusCode::BAD_REQUEST, "invalid_routing_filter");
    }
    let scope = caller.scope();
    match app
        .store
        .traffic_report(
            &scope.institution,
            &scope.products,
            page,
            filter.case_id.as_deref(),
        )
        .await
    {
        Ok(v) => Json(v).into_response(),
        Err(_) => unavailable(&app),
    }
}
async fn change_traffic(
    State(app): State<Shared>,
    Authenticated(caller): Authenticated,
    headers: axum::http::HeaderMap,
    Json(body): Json<ekyc_postgres::TrafficChange>,
) -> Response {
    let Access::RoutingOperator {
        institution,
        products,
    } = &caller.access
    else {
        return error(StatusCode::FORBIDDEN, "role_forbidden");
    };
    if !products.contains(&body.product) {
        return error(StatusCode::FORBIDDEN, "scope_forbidden");
    }
    let Some(key) = headers
        .get("Idempotency-Key")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty() && v.len() <= 128)
    else {
        return error(StatusCode::BAD_REQUEST, "invalid_command_key");
    };
    let capped = matches!(body.mode.as_str(), "capped" | "recovering");
    if !matches!(body.provider.as_str(), "address-a" | "address-b")
        || body.product != "wallet"
        || !matches!(
            body.mode.as_str(),
            "normal" | "paused" | "draining" | "capped" | "recovering"
        )
        || body.share_cap.is_some() != capped
        || body.share_cap.is_some_and(|x| !(0..=100).contains(&x))
        || !(1..=86400).contains(&body.duration_seconds)
        || !(0..i64::MAX).contains(&body.expected_version)
        || body.reason.trim().is_empty()
        || body.reason.len() > 512
    {
        return error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_routing_change");
    }
    match app
        .store
        .set_traffic(institution, &caller.id, key, &body)
        .await
    {
        Ok(Some(v)) => Json(v).into_response(),
        Ok(None) => error(StatusCode::CONFLICT, "routing_change_conflict"),
        Err(_) => unavailable(&app),
    }
}

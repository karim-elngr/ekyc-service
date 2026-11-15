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
        .route("/v2/investigations", get(queue))
        .route("/v2/investigations/acknowledgement", put(acknowledge))
}

async fn session(Authenticated(caller): Authenticated) -> Response {
    let role = match caller.access {
        Access::Observer { .. } => "observer",
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

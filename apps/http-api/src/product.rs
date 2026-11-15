use crate::{
    auth::{Access, Authenticated, Caller},
    error, unavailable, App, Shared,
};
use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use ekyc_domain::{
    product::wire::{valid_review_reason, Action, Command, Evidence, Open, Retry, Review},
    CheckKind,
};
use ekyc_postgres::Gate;
use ekyc_restate_ingress::CommandError;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

pub(super) fn routes() -> Router<Shared> {
    Router::new()
        .route("/v2/cases", post(open_product))
        .route("/v2/cases/{id}/evidence", post(submit))
        .route("/v2/cases/{id}/retry", post(retry))
        .route("/v2/cases/{id}/review", post(review).get(review_details))
}

fn valid(s: &str, limit: usize) -> bool {
    !s.trim().is_empty() && s.len() <= limit && !s.chars().any(char::is_control)
}

fn valid_evidence(refs: &BTreeMap<CheckKind, String>) -> bool {
    refs.iter()
        .all(|(kind, r)| *kind != CheckKind::Sanctions && valid(r, 256))
}

async fn review_details(
    State(app): State<Shared>,
    Path(id): Path<String>,
    Authenticated(caller): Authenticated,
) -> Response {
    let Access::Reviewer {
        institution,
        product,
        ..
    } = &caller.access
    else {
        return error(StatusCode::FORBIDDEN, "role_forbidden");
    };
    match app
        .store
        .product_review_details(&id, institution, product)
        .await
    {
        Ok(Some(handoff)) => {
            app.metrics.reads.fetch_add(1, Ordering::Relaxed);
            Json(handoff).into_response()
        }
        Ok(None) => error(StatusCode::NOT_FOUND, "case_not_found"),
        Err(_) => unavailable(&app),
    }
}

async fn open_product(
    State(app): State<Shared>,
    Authenticated(caller): Authenticated,
    headers: HeaderMap,
    Json(body): Json<Open>,
) -> Response {
    command(&app, &caller, &headers, None, Action::Open(body)).await
}
async fn submit(
    State(app): State<Shared>,
    Path(id): Path<String>,
    Authenticated(caller): Authenticated,
    headers: HeaderMap,
    Json(body): Json<Evidence>,
) -> Response {
    command(&app, &caller, &headers, Some(id), Action::Evidence(body)).await
}
async fn retry(
    State(app): State<Shared>,
    Path(id): Path<String>,
    Authenticated(caller): Authenticated,
    headers: HeaderMap,
    Json(body): Json<Retry>,
) -> Response {
    command(&app, &caller, &headers, Some(id), Action::Retry(body)).await
}
async fn review(
    State(app): State<Shared>,
    Path(id): Path<String>,
    Authenticated(caller): Authenticated,
    headers: HeaderMap,
    Json(mut body): Json<Review>,
) -> Response {
    body.reason = body.reason.replace("\r\n", "\n");
    command(&app, &caller, &headers, Some(id), Action::Review(body)).await
}

async fn command(
    app: &App,
    caller: &Caller,
    headers: &HeaderMap,
    id: Option<String>,
    action: Action,
) -> Response {
    let (institution, product) = match (&caller.access, &action) {
        (
            Access::Reviewer {
                institution,
                product,
                ..
            },
            Action::Review(_),
        ) => (institution, product),
        (Access::Product { .. }, Action::Review(_)) => {
            return error(StatusCode::FORBIDDEN, "role_forbidden")
        }
        (Access::Product { use_cases, .. }, Action::Open(open))
            if !use_cases.contains(&open.use_case) =>
        {
            return error(StatusCode::FORBIDDEN, "use_case_forbidden")
        }
        (
            Access::Product {
                institution,
                product,
                ..
            },
            _,
        ) => (institution, product),
        _ => return error(StatusCode::FORBIDDEN, "role_forbidden"),
    };
    let Some(key) = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .filter(|k| !k.is_empty() && k.len() <= 128 && k.bytes().all(|b| b.is_ascii_graphic()))
    else {
        return error(StatusCode::BAD_REQUEST, "invalid_idempotency_key");
    };
    match &action {
        Action::Open(body) if !valid(&body.subject_ref, 256) || !valid_evidence(&body.evidence) => {
            return error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_case")
        }
        Action::Evidence(body) if body.evidence.is_empty() || !valid_evidence(&body.evidence) => {
            return error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_evidence")
        }
        Action::Review(body) if !valid_review_reason(&body.reason) => {
            return error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_review_reason");
        }
        Action::Review(body)
            if !valid(&body.operation_id, 256) || !valid(&body.review_ref, 256) =>
        {
            return error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_review")
        }
        _ => (),
    }
    let id = match id {
        None => caller.case_id(key),
        Some(id) => {
            if matches!(caller.access, Access::Product { .. }) && !caller.owns(&id) {
                return error(StatusCode::NOT_FOUND, "case_not_found");
            }
            match app.store.scoped_case(&id, &caller.scope()).await {
                Ok(Some(_)) => id,
                Ok(None) => return error(StatusCode::NOT_FOUND, "case_not_found"),
                Err(_) => return unavailable(app),
            }
        }
    };
    match app.store.admit(&caller.id, app.limit).await {
        Ok(Gate::Granted) => (),
        Ok(Gate::Wait { retry_after_ms }) => {
            app.metrics.throttled.fetch_add(1, Ordering::Relaxed);
            let mut res = error(StatusCode::TOO_MANY_REQUESTS, "admission_rate_limited");
            res.headers_mut().insert(
                header::RETRY_AFTER,
                retry_after_ms.div_ceil(1000).max(1).into(),
            );
            return res;
        }
        _ => return unavailable(app),
    }
    let opening = matches!(action, Action::Open(_));
    let body = Command {
        caller: caller.id.clone(),
        institution: institution.clone(),
        product: product.clone(),
        key: key.to_owned(),
        action,
    };
    match app.commands.product(&id, &body).await {
        Ok(()) => {
            app.metrics.accepted.fetch_add(1, Ordering::Relaxed);
            let location = format!("/v2/cases/{id}");
            (
                StatusCode::ACCEPTED,
                [(header::LOCATION, location.clone())],
                Json(json!({"case_id":id,"location":location})),
            )
                .into_response()
        }
        Err(CommandError::NotFound) if opening => unavailable(app),
        Err(CommandError::NotFound) => error(StatusCode::NOT_FOUND, "case_not_found"),
        Err(CommandError::Forbidden) => error(StatusCode::FORBIDDEN, "access_denied"),
        Err(CommandError::Conflict(code)) => error(StatusCode::CONFLICT, code),
        Err(CommandError::Invalid) => error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_command"),
        Err(CommandError::Unavailable) => unavailable(app),
    }
}

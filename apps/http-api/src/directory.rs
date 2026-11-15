use crate::{
    auth::{valid_slug, Authenticated, Caller},
    error, unavailable, Shared,
};
use axum::{
    extract::{rejection::QueryRejection, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use chrono::{DateTime, Duration, Utc};
use ekyc_domain::product::CaseState;
use ekyc_postgres::{CaseFilter, CaseScope, PageOrder};
use serde::Deserialize;
use serde_json::json;
use std::sync::atomic::Ordering;

pub(super) fn routes() -> Router<Shared> {
    Router::new()
        .route("/v2/cases", get(list))
        .route("/v2/cases/overview", get(overview))
        .route("/v2/cases/{id}", get(read))
        .route("/v2/cases/{id}/history", get(history))
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Search {
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    product: Option<String>,
    state: Option<CaseState>,
    use_case: Option<String>,
    query: Option<String>,
    sort: Option<PageOrder>,
    page: Option<u32>,
    page_size: Option<u32>,
}

fn invalid(message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error":"invalid_query","message":message})),
    )
        .into_response()
}

fn parse<T>(query: Result<Query<T>, QueryRejection>) -> Result<T, Box<Response>> {
    query
        .map(|Query(v)| v)
        .map_err(|e| Box::new(invalid(&e.body_text())))
}

fn time_window(
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Result<(DateTime<Utc>, DateTime<Utc>), &'static str> {
    let to = to.unwrap_or(now);
    let from = match from {
        Some(f) => f,
        None => to
            .checked_sub_signed(Duration::days(7))
            .ok_or("invalid end of window")?,
    };
    if from >= to || to - from > Duration::days(366) {
        return Err("from must precede to by at most 366 days");
    }
    Ok((from, to))
}

impl Search {
    fn filter(&self, now: DateTime<Utc>) -> Result<CaseFilter, &'static str> {
        let (from, to) = time_window(self.from, self.to, now)?;
        if self.use_case.as_deref().is_some_and(|s| !valid_slug(s)) {
            return Err("invalid use_case");
        }
        if self.product.as_deref().is_some_and(|s| !valid_slug(s)) {
            return Err("invalid product");
        }
        if self.query.as_ref().is_some_and(|s| {
            s.trim().is_empty() || s.len() > 256 || s.chars().any(char::is_control)
        }) {
            return Err("query must contain 1 to 256 bytes of searchable text");
        }
        Ok(CaseFilter {
            from,
            to,
            state: self.state.map(|s| s.as_str().to_owned()),
            use_case: self.use_case.clone(),
            query: self.query.clone(),
        })
    }
    fn pagination(&self) -> Result<(u32, u32, u32), &'static str> {
        let page = self.page.unwrap_or(1);
        let size = self.page_size.unwrap_or(20);
        if page == 0 || !(1..=100).contains(&size) {
            return Err("page starts at 1 and page_size must be between 1 and 100");
        }
        let offset = (page - 1)
            .checked_mul(size)
            .filter(|n| *n <= 10_000)
            .ok_or("page offset exceeds 10000; narrow the time range or filters")?;
        Ok((page, size, offset))
    }
}

struct SearchRequest {
    scope: CaseScope,
    query: Search,
    filter: CaseFilter,
}

impl SearchRequest {
    fn parse(
        caller: &Caller,
        query: Result<Query<Search>, QueryRejection>,
    ) -> Result<Self, Box<Response>> {
        let query = parse(query)?;
        let filter = query.filter(Utc::now()).map_err(invalid)?;
        let mut scope = caller.scope();
        if let Some(product) = &query.product {
            if !scope.products.contains(product) {
                return Err(Box::new(error(StatusCode::FORBIDDEN, "product_forbidden")));
            }
            scope.products = vec![product.clone()];
        }
        Ok(Self {
            scope,
            query,
            filter,
        })
    }
}

async fn list(
    State(app): State<Shared>,
    Authenticated(caller): Authenticated,
    query: Result<Query<Search>, QueryRejection>,
) -> Response {
    let req = match SearchRequest::parse(&caller, query) {
        Ok(r) => r,
        Err(e) => return *e,
    };
    let (page, size, offset) = match req.query.pagination() {
        Ok(v) => v,
        Err(e) => return invalid(e),
    };
    match app
        .store
        .case_page(
            &req.scope,
            &req.filter,
            req.query.sort.unwrap_or_default(),
            size,
            offset,
        )
        .await
    {
        Ok(result) => {
            app.metrics.reads.fetch_add(1, Ordering::Relaxed);
            Json(json!({"cases":result.cases,"total":result.total,"page":page,"page_size":size,"from":req.filter.from,"to":req.filter.to})).into_response()
        }
        Err(_) => unavailable(&app),
    }
}

async fn overview(
    State(app): State<Shared>,
    Authenticated(caller): Authenticated,
    query: Result<Query<Search>, QueryRejection>,
) -> Response {
    let req = match SearchRequest::parse(&caller, query) {
        Ok(r) => r,
        Err(e) => return *e,
    };
    if req.query.page.is_some() || req.query.page_size.is_some() || req.query.sort.is_some() {
        return invalid("overview accepts filters only");
    }
    match app.store.case_overview(&req.scope, &req.filter).await {
        Ok(view) => {
            app.metrics.reads.fetch_add(1, Ordering::Relaxed);
            Json(json!({"total":view.total,"states":view.states,"checks":view.checks,"from":req.filter.from,"to":req.filter.to})).into_response()
        }
        Err(_) => unavailable(&app),
    }
}

async fn read(
    State(app): State<Shared>,
    Path(id): Path<String>,
    Authenticated(caller): Authenticated,
) -> Response {
    match app.store.scoped_case(&id, &caller.scope()).await {
        Ok(Some(view)) => {
            app.metrics.reads.fetch_add(1, Ordering::Relaxed);
            Json(view).into_response()
        }
        Ok(None) => error(StatusCode::NOT_FOUND, "case_not_found"),
        Err(_) => unavailable(&app),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct History {
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    before_version: Option<i64>,
    limit: Option<u32>,
}

async fn history(
    State(app): State<Shared>,
    Path(id): Path<String>,
    Authenticated(caller): Authenticated,
    query: Result<Query<History>, QueryRejection>,
) -> Response {
    let query = match parse(query) {
        Ok(q) => q,
        Err(e) => return *e,
    };
    let window = if query.from.is_some() || query.to.is_some() {
        match time_window(query.from, query.to, Utc::now()) {
            Ok(w) => Some(w),
            Err(e) => return invalid(e),
        }
    } else {
        None
    };
    let limit = query.limit.unwrap_or(20);
    if !(1..=100).contains(&limit) || query.before_version.is_some_and(|v| v <= 0) {
        return invalid("limit must be between 1 and 100 and before_version must be positive");
    }
    match app
        .store
        .case_history(&id, &caller.scope(), query.before_version, window, limit)
        .await
    {
        Ok(Some(page)) => {
            app.metrics.reads.fetch_add(1, Ordering::Relaxed);
            Json(page).into_response()
        }
        Ok(None) => error(StatusCode::NOT_FOUND, "case_not_found"),
        Err(_) => unavailable(&app),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn time_and_page_limits_are_explicit_and_overflow_safe() {
        let end: DateTime<Utc> = "2026-11-15T00:00:00Z".parse().unwrap();
        let mut q = Search::default();
        let f = q.filter(end).unwrap();
        assert_eq!((f.from, f.to), (end - Duration::days(7), end));
        q.from = Some(end - Duration::days(367));
        assert!(q.filter(end).is_err());
        q.from = Some(end);
        assert!(q.filter(end).is_err());
        q.page = Some(u32::MAX);
        assert!(q.pagination().is_err());
        q.page = Some(501);
        assert_eq!(q.pagination(), Ok((501, 20, 10000)));
    }
}

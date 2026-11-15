//! Real HTTP routing and projections, using an isolated PostgreSQL database per test.
use super::*;
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Router,
};
use chrono::{Duration as Days, Utc};
use ekyc_domain::{
    product::{BindingSnapshot, Policy, ProductCase},
    CheckKind,
};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::collections::BTreeMap;
use tower::ServiceExt;

const WALLET: &str = "local-demo-wallet-product";
const OBSERVER: &str = "local-demo-institution-observer";
const REVIEWER: &str = "local-demo-wallet-reviewer";

async fn call(
    app: &Router,
    method: &str,
    path: &str,
    token: &str,
    body: Value,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .header("idempotency-key", "test-key")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

#[sqlx::test(migrations = false)]
async fn protected_routes_share_scopes_and_expose_only_public_history(pool: PgPool) {
    let store = Store::from_pool(pool.clone()).await.unwrap();
    let at = Utc::now() - Days::hours(2);
    let wallet = format!("v2~wallet~{:064x}", 1);
    let other = format!("v2~second~{:064x}", 2);
    let foreign = format!("v2~other-lending~{:064x}", 3);
    let loan = format!("v2~lending~{:064x}", 4);
    for (tenant, prod, id, use_case) in [
        ("demo-bank", "wallet", &wallet, "onboarding"),
        ("demo-bank", "wallet", &other, "onboarding"),
        ("other-bank", "lending", &foreign, "loan_application"),
        ("demo-bank", "lending", &loan, "loan_application"),
    ] {
        let mut case = ProductCase::open(
            id.clone(),
            tenant.into(),
            "customer".into(),
            Policy::load(prod, use_case).unwrap(),
            BTreeMap::new(),
            &[],
            BindingSnapshot::default(),
            at,
        )
        .unwrap();
        store.publish_product(&case).await.unwrap();
        if id == &wallet {
            let evidence = ekyc_domain::product::BoundEvidence {
                tenant_id: tenant.into(),
                subject_ref: "customer".into(),
                kind: CheckKind::Document,
                evidence_ref: "internal-document".into(),
                revision: "v1".into(),
                provider: "document-vendor".into(),
                callback: true,
                provider_ref: "demo:interactive".into(),
                method: "document_verification".into(),
                document_revision: Some("id".into()),
                portrait_revision: None,
            };
            case.refresh_bindings(
                BindingSnapshot {
                    provider_candidates: Default::default(),
                    current_document_revision: Some("id".into()),
                    ..BindingSnapshot::default()
                },
                at,
            )
            .unwrap();
            case.provide(evidence, at).unwrap();
            store.publish_product(&case).await.unwrap();
        }
    }
    let app = server::router(Arc::new(App {
        store,
        clients: Clients::parse(include_str!("../../../config/demo-clients.json")).unwrap(),
        commands: CaseCommands::new(
            "http://127.0.0.1:1".parse().unwrap(),
            Duration::from_millis(100),
        )
        .unwrap(),
        interactive: None,
        limit: 2,
        metrics: Metrics::default(),
    }));
    let get = |path: &str, token: &str| {
        let app = app.clone();
        let path = path.to_owned();
        let token = token.to_owned();
        async move { call(&app, "GET", &path, &token, Value::Null).await }
    };
    let (status, page) = get("/v2/cases?page_size=1", OBSERVER).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        (
            page["total"].as_i64(),
            page["cases"].as_array().unwrap().len()
        ),
        (Some(3), 1)
    );
    assert_eq!(get("/v2/cases", WALLET).await.1["total"], 1);
    assert_eq!(
        get("/v2/cases/overview?product=wallet", OBSERVER).await.1["total"],
        2
    );
    assert_eq!(
        get("/v2/cases/overview?product=lending", WALLET).await.0,
        StatusCode::FORBIDDEN
    );
    for path in [
        "/v2/cases?state=UNKNOWN",
        "/v2/cases?page_size=101",
        "/v2/cases?from=2000-01-01T00:00:00Z",
        "/v2/cases/overview?page=1",
        "/v2/cases?unexpected=1",
        "/v2/cases?sort=x",
        "/v2/cases?page=0",
    ] {
        let (status, err) = get(path, OBSERVER).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
        assert_eq!(err["error"], "invalid_query");
    }
    assert_eq!(get("/v1/cases", OBSERVER).await.0, StatusCode::NOT_FOUND);
    assert_eq!(
        get("/v2/cases", "missing").await.0,
        StatusCode::UNAUTHORIZED
    );
    for (id, token) in [(&foreign, OBSERVER), (&other, WALLET), (&loan, REVIEWER)] {
        assert_eq!(
            get(&format!("/v2/cases/{id}/history"), token).await.0,
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        get(&format!("/v2/cases/{wallet}/review"), OBSERVER).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        get(&format!("/v2/cases/{other}/review"), REVIEWER).await.0,
        StatusCode::OK
    );
    let history_url = format!("/v2/cases/{wallet}/history");
    let (status, h) = get(&format!("{history_url}?limit=1"), OBSERVER).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!h.to_string().contains("internal-document"));
    let next = h["next_before_version"].as_i64().unwrap();
    let (_, older) = get(&format!("{history_url}?before_version={next}"), OBSERVER).await;
    assert_eq!(older["revisions"][0]["version"], 1);
    assert!(older["next_before_version"].is_null());
    assert!(get(&format!("{history_url}?before_version=1"), OBSERVER)
        .await
        .1["revisions"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(
        get(&format!("{history_url}?to=2000-01-01T00:00:00Z"), OBSERVER)
            .await
            .1["revisions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        get(&format!("/v2/cases/{wallet}/interactive"), OBSERVER)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &app,
            "POST",
            &format!("/v2/cases/{wallet}/interactive/document?operation_id=superseded"),
            WALLET,
            json!({"response":"pass"})
        )
        .await
        .1["error"],
        "stale_operation"
    );
    assert_eq!(
        call(
            &app,
            "POST",
            "/v2/cases",
            OBSERVER,
            json!({"subject_ref":"person","use_case":"onboarding"})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}

mod staff;

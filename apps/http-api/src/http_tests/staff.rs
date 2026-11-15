use super::*;

#[sqlx::test(migrations = false)]
async fn staff_queue_covers_old_reviews_and_late_alerts_with_scoped_immutable_acknowledgements(
    pool: PgPool,
) {
    let store = Store::from_pool(pool.clone()).await.unwrap();
    let old = Utc::now() - Days::days(11);
    for (id, bank, state) in [
        ("old-review", "demo-bank", "AWAITING_REVIEW"),
        ("closed", "demo-bank", "APPROVED"),
        ("foreign", "other-bank", "APPROVED"),
    ] {
        let case = ProductCase::open(
            id.into(),
            bank.into(),
            "customer".into(),
            Policy::load("lending", "loan_application").unwrap(),
            BTreeMap::new(),
            &[],
            BindingSnapshot::default(),
            old,
        )
        .unwrap();
        store.publish_product(&case).await.unwrap();
        let checks = if state == "AWAITING_REVIEW" {
            json!([{"kind":"sanctions","status":"REVIEW","operation_id":"screen-1","observed_at":old}])
        } else {
            json!([])
        };
        sqlx::query("UPDATE product_case SET state=$2,public_view=jsonb_set(public_view,'{checks}',$3) WHERE case_id=$1")
            .bind(id).bind(state).bind(checks).execute(&pool).await.unwrap();
        if state == "APPROVED" {
            store
                .late_product_screen(&case, "late-1", old, Some(old))
                .await
                .unwrap();
        }
    }
    let app = server::router(Arc::new(App {
        store,
        clients: Clients::parse(include_str!("../../../../config/demo-clients.json")).unwrap(),
        commands: CaseCommands::new(
            "http://127.0.0.1:1".parse().unwrap(),
            Duration::from_millis(100),
        )
        .unwrap(),
        interactive: None,
        limit: 10,
        metrics: Metrics::default(),
    }));
    let alice = "local-demo-reviewer-alice";
    let sam = "local-demo-reviewer-sam";
    for (token, role) in [
        (alice, "reviewer"),
        (sam, "reviewer"),
        (OBSERVER, "observer"),
    ] {
        let (status, session) = call(&app, "GET", "/v2/staff/session", token, Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(session["role"], role);
    }
    let (status, open) = call(&app, "GET", "/v2/investigations", alice, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let items = open["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert!(items
        .iter()
        .any(|i| i["case_id"] == "old-review" && i["operation_id"] == "screen-1"));
    let (_, first) = call(
        &app,
        "GET",
        "/v2/investigations?limit=1",
        alice,
        Value::Null,
    )
    .await;
    let cursor = first["next_after"].as_str().unwrap();
    let (_, next) = call(
        &app,
        "GET",
        &format!("/v2/investigations?limit=1&after={cursor}"),
        sam,
        Value::Null,
    )
    .await;
    assert_ne!(first["items"][0]["work_id"], next["items"][0]["work_id"]);
    assert!(next["next_after"].is_null());
    let late = items.iter().find(|i| i["case_id"] == "closed").unwrap();
    assert_eq!(late["state"], "APPROVED");
    let ack = json!({"work_id":late["work_id"]});
    assert_eq!(
        call(&app, "GET", "/v2/investigations", OBSERVER, Value::Null)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &app,
            "PUT",
            "/v2/investigations/acknowledgement",
            OBSERVER,
            ack.clone()
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &app,
            "PUT",
            "/v2/investigations/acknowledgement",
            REVIEWER,
            ack.clone()
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    for token in [alice, sam] {
        let (status, item) = call(
            &app,
            "PUT",
            "/v2/investigations/acknowledgement",
            token,
            ack.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(item["acknowledged_by"], "reviewer-alice");
        assert_eq!(item["operation_id"], "late-1");
    }
    let (_, history) = call(
        &app,
        "GET",
        "/v2/investigations?status=acknowledged",
        sam,
        Value::Null,
    )
    .await;
    assert_eq!(history["items"].as_array().unwrap().len(), 1);
    assert_eq!(history["items"][0]["state"], "APPROVED");
    let (_, rest) = call(&app, "GET", "/v2/investigations", sam, Value::Null).await;
    assert_eq!(rest["items"].as_array().unwrap().len(), 1);
    assert_eq!(rest["items"][0]["case_id"], "old-review");
    let claim = json!({"work_id":rest["items"][0]["work_id"]});
    let (one, two) = tokio::join!(
        call(
            &app,
            "PUT",
            "/v2/investigations/acknowledgement",
            alice,
            claim.clone()
        ),
        call(
            &app,
            "PUT",
            "/v2/investigations/acknowledgement",
            sam,
            claim
        ),
    );
    assert_eq!(one, two);
    assert_eq!(one.0, StatusCode::OK);
    let (_, still_open) = call(&app, "GET", "/v2/investigations", sam, Value::Null).await;
    assert_eq!(
        still_open["items"][0]["acknowledged_by"],
        one.1["acknowledged_by"]
    );
}

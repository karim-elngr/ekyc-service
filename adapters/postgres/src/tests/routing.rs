use crate::{ContractBook, Store, TrafficChange};
use chrono::Utc;
use ekyc_domain::{
    product::{BindingSnapshot, BoundEvidence, Policy, ProductCase},
    CheckKind,
};
use sqlx::{Error, PgPool};
use std::collections::BTreeMap;

fn pilot(id: &str, use_case: &str, vendors: &[&str]) -> ProductCase {
    let kind = CheckKind::Address;
    let assets: Vec<_> = vendors
        .iter()
        .map(|vendor| BoundEvidence {
            tenant_id: "bank".into(),
            subject_ref: "subject".into(),
            kind,
            evidence_ref: "v1".into(),
            revision: "v1".into(),
            provider: vendor.to_string(),
            provider_ref: format!("opaque-{vendor}"),
            callback: false,
            method: format!("address_verification_{}", vendor.chars().last().unwrap()),
            document_revision: None,
            portrait_revision: None,
        })
        .collect();
    ProductCase::open(
        id.into(),
        "bank".into(),
        "subject".into(),
        Policy::load("wallet", use_case).unwrap(),
        BTreeMap::from([(kind, assets[0].clone())]),
        &[],
        BindingSnapshot {
            provider_candidates: BTreeMap::from([(kind, assets)]),
            ..Default::default()
        },
        Utc::now(),
    )
    .unwrap()
}
async fn setup(pool: PgPool) -> Result<Store, Error> {
    let store = Store { pool };
    let book: ContractBook =
        serde_json::from_str(include_str!("../../../../config/provider-contracts.json")).unwrap();
    store.install_contracts(&book).await?;
    Ok(store)
}
async fn change(store: &Store, mode: &str, version: i64) -> Result<(), Error> {
    assert!(store
        .set_traffic(
            "bank",
            "operator",
            &format!("change-{version}"),
            &TrafficChange {
                product: "wallet".into(),
                provider: "address-a".into(),
                expected_version: version,
                mode: mode.into(),
                share_cap: None,
                duration_seconds: 300,
                reason: "test".into()
            }
        )
        .await?
        .is_some());
    Ok(())
}
async fn gate(store: &Store, op: &str, lane: &str, send: bool) -> Result<Option<String>, Error> {
    sqlx::query_scalar("SELECT ekyc_check_route($1,$2,$3)")
        .bind(op)
        .bind(lane)
        .bind(send)
        .fetch_one(&store.pool)
        .await
}

#[sqlx::test(migrations = "./migrations")]
async fn persisted_choice_separates_drain_pause_and_replacement(pool: PgPool) -> Result<(), Error> {
    let store = setup(pool).await?;
    let mut case = pilot("first", "address-priority", &["address-a", "address-b"]);
    let deadline = Utc::now().timestamp_millis() + 60_000;
    let (original, when) = store
        .assign_route(&case, CheckKind::Address, deadline, false)
        .await?
        .unwrap();
    assert_eq!(original.provider, "address-a");
    assert_eq!(when, deadline);
    store.publish_product(&case).await?;
    change(&store, "draining", 0).await?;
    assert_eq!(
        gate(&store, "first/address/1", "submit", false).await?,
        None
    );
    assert_eq!(
        store
            .assign_route(&case, CheckKind::Address, deadline + 500, false)
            .await?
            .unwrap()
            .0,
        original
    );
    let next = pilot("next", "address-priority", &["address-a", "address-b"]);
    assert_eq!(
        store
            .assign_route(&next, CheckKind::Address, deadline, false)
            .await?
            .unwrap()
            .0
            .provider,
        "address-b"
    );
    change(&store, "paused", 1).await?;
    assert_eq!(
        gate(&store, "first/address/1", "submit", true)
            .await?
            .as_deref(),
        Some("paused")
    );
    assert!(case.replace_address("first/address/1", Utc::now()));
    let (b, inherited) = store
        .assign_route(&case, CheckKind::Address, deadline + 3000, true)
        .await?
        .unwrap();
    assert_eq!(b.provider, "address-b");
    assert_eq!(inherited, deadline);
    assert_eq!(
        gate(&store, "first/address/1", "submit", true)
            .await?
            .as_deref(),
        Some("retired")
    );
    assert_eq!(
        gate(&store, "first/address/1", "lookup", true)
            .await?
            .as_deref(),
        Some("no_outstanding_operation")
    );
    let single = pilot("single", "address-priority", &["address-a"]);
    let (bound, _) = store
        .assign_route(&single, CheckKind::Address, deadline, false)
        .await?
        .unwrap();
    assert_eq!(bound.provider, "address-a");
    store.publish_product(&single).await?;
    assert_eq!(
        gate(&store, "single/address/1", "submit", true)
            .await?
            .as_deref(),
        Some("paused")
    );
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn ambiguous_replacement_preserves_retrieval_and_original_outcome(
    pool: PgPool,
) -> Result<(), Error> {
    let store = setup(pool).await?;
    let mut case = pilot(
        "ambiguous",
        "address-replacement",
        &["address-a", "address-b"],
    );
    let deadline = Utc::now().timestamp_millis() + 60_000;
    let (a, _) = store
        .assign_route(&case, CheckKind::Address, deadline, false)
        .await?
        .unwrap();
    assert_eq!(a.provider, "address-a", "fixture must hash to A");
    store.publish_product(&case).await?;
    let op = "ambiguous/address/1";
    assert_eq!(gate(&store, op, "submit", true).await?, None);
    assert!(case.replace_address(op, Utc::now()));
    let (b, d) = store
        .assign_route(&case, CheckKind::Address, deadline + 5000, true)
        .await?
        .unwrap();
    assert_eq!((b.provider.as_str(), d), ("address-b", deadline));
    change(&store, "paused", 0).await?;
    assert_eq!(gate(&store, op, "lookup", false).await?, None);
    assert_eq!(
        gate(&store, op, "submit", true).await?.as_deref(),
        Some("retired")
    );
    let answers = store.route_answers(&case.case_id, op).await?;
    assert_eq!(answers.len(), 2);
    assert_eq!(answers[0].status, "outstanding");
    assert_eq!(answers[1].binding.provider_ref, "opaque-address-b");
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn recovery_requires_a_full_new_sample_and_regression_steps_back(
    pool: PgPool,
) -> Result<(), Error> {
    use crate::{AttemptAdmission, AttemptIdentity};
    let store = setup(pool).await?;
    let deadline = Utc::now().timestamp_millis() + 60_000;
    change(&store, "recovering", 0).await?;
    let routing = Policy::load("wallet", "address-pilot")
        .unwrap()
        .checks
        .remove(0)
        .routing
        .unwrap();
    let mut cases = (0..)
        .map(|i| format!("ramp-{i}"))
        .filter(|id| routing.bucket(&format!("{id}/address/v1")) < 5);
    for (expected, latency) in [(5, 80), (20, 80), (50, 900), (20, 80), (50, 80), (80, 80)] {
        for j in 0..4 {
            let id = cases.next().unwrap();
            let mut case = pilot(&id, "address-pilot", &["address-a", "address-b"]);
            let (asset, _) = store
                .assign_route(&case, CheckKind::Address, deadline, false)
                .await?
                .unwrap();
            assert_eq!(asset.provider, "address-a");
            let stage: i32 = sqlx::query_scalar(
                "SELECT stage FROM provider_route_health WHERE provider='address-a'",
            )
            .fetch_one(&store.pool)
            .await?;
            assert_eq!(
                stage, expected,
                "window must stay at stage through four new calls"
            );
            case.checks.get_mut(&CheckKind::Address).unwrap().evidence = Some(asset);
            store.publish_product(&case).await?;
            let operation = format!("{id}/address/1");
            let token = format!("{operation}/submit/1");
            let decision = store
                .authorize_attempt(AttemptIdentity {
                    token: &token,
                    case_id: &id,
                    operation: &operation,
                    kind: CheckKind::Address,
                    provider: "address-a",
                    pool: "demo-address-a",
                    lane: "submit",
                    deadline_ms: deadline,
                    max_attempts: 3,
                })
                .await?;
            assert!(
                matches!(decision, AttemptAdmission::Send { .. }),
                "{decision:?} ({j})"
            );
            store
                .finish_attempt(&token, "ok", serde_json::json!({"ok":true}), latency, 0)
                .await?;
        }
        // Fast-forward the fixture's dwell via the test database, keeping only this stage's samples.
        sqlx::query("UPDATE provider_route_health SET since=since-interval '2 seconds' WHERE provider='address-a'").execute(&store.pool).await?;
        sqlx::query("UPDATE provider_attempt SET finished_at=clock_timestamp()-interval '1 minute' WHERE token NOT IN
                     (SELECT token FROM provider_attempt ORDER BY started_at DESC LIMIT 4)").execute(&store.pool).await?;
    }
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn shadow_observes_scoped_overrides_without_creating_route_or_accounting_rows(
    pool: PgPool,
) -> Result<(), Error> {
    let store = setup(pool).await?;
    let case = pilot("shadow", "address-priority", &["address-a", "address-b"]);
    change(&store, "paused", 0).await?;
    let decision = Store::shadow_address(&store.pool, &case).await?;
    assert_eq!(decision["prospective_provider"], "address-b");
    assert_eq!(decision["reason"], "outage_diversion");
    assert!(!decision.to_string().contains("opaque-"));
    let effects: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM product_case)+(SELECT count(*) FROM provider_route_health)
        +(SELECT count(*) FROM provider_route_selection)+(SELECT count(*) FROM provider_attempt)
        +(SELECT count(*) FROM provider_spend_reservation)",
    )
    .fetch_one(&store.pool)
    .await?;
    assert_eq!(effects, 0);
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn queued_retry_and_replacement_share_one_last_submission(pool: PgPool) -> Result<(), Error> {
    use crate::{AttemptAdmission, AttemptIdentity};
    let store = setup(pool).await?;
    let mut case = pilot(
        "ambiguous",
        "address-replacement",
        &["address-a", "address-b"],
    );
    let deadline = Utc::now().timestamp_millis() + 60_000;
    let (binding, _) = store
        .assign_route(&case, CheckKind::Address, deadline, false)
        .await?
        .unwrap();
    assert_eq!(binding.provider, "address-a");
    case.checks.get_mut(&CheckKind::Address).unwrap().evidence = Some(binding);
    store.publish_product(&case).await?;
    let original = "ambiguous/address/1";
    let admit = |token: &'static str,
                 provider: &'static str,
                 pool: &'static str,
                 op: &'static str,
                 lane: &'static str| {
        store.authorize_attempt(AttemptIdentity {
            token,
            case_id: "ambiguous",
            operation: op,
            kind: CheckKind::Address,
            provider,
            pool,
            lane,
            deadline_ms: deadline,
            max_attempts: 2,
        })
    };
    assert!(matches!(
        admit("first", "address-a", "demo-address-a", original, "submit").await?,
        AttemptAdmission::Send { .. }
    ));
    let stale_retry = admit("retry", "address-a", "demo-address-a", original, "submit");
    let replacement = async {
        assert!(case.replace_address(original, Utc::now()));
        let (binding, inherited) = store
            .assign_route(&case, CheckKind::Address, deadline + 5_000, true)
            .await?
            .unwrap();
        assert_eq!(
            (binding.provider.as_str(), inherited),
            ("address-b", deadline)
        );
        case.checks.get_mut(&CheckKind::Address).unwrap().evidence = Some(binding);
        store.publish_product(&case).await?;
        admit(
            "replacement",
            "address-b",
            "demo-address-b",
            "ambiguous/address/2",
            "submit",
        )
        .await
    };
    let (a, b) = tokio::try_join!(stale_retry, replacement)?;
    assert_eq!(
        [a, b]
            .into_iter()
            .filter(|r| matches!(r, AttemptAdmission::Send { .. }))
            .count(),
        1
    );
    assert_eq!(sqlx::query_scalar::<_,i32>("SELECT submissions FROM provider_attempt_budget WHERE requirement_id='ambiguous/address/v1'")
        .fetch_one(&store.pool).await?,2);
    assert!(matches!(
        admit(
            "lookup-original",
            "address-a",
            "demo-address-a",
            original,
            "lookup"
        )
        .await?,
        AttemptAdmission::Send { .. }
    ));
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn concurrent_zero_share_probes_cannot_become_ordinary_sends_after_close(
    pool: PgPool,
) -> Result<(), Error> {
    use crate::{AttemptAdmission, AttemptIdentity};
    let store = setup(pool).await?;
    store
        .set_traffic(
            "bank",
            "operator",
            "zero",
            &TrafficChange {
                product: "wallet".into(),
                provider: "address-a".into(),
                expected_version: 0,
                mode: "capped".into(),
                share_cap: Some(0),
                duration_seconds: 300,
                reason: "withdraw ordinary traffic".into(),
            },
        )
        .await?
        .unwrap();
    sqlx::query(
        "UPDATE provider_circuit SET generation=7,open_until=clock_timestamp()-interval '1 second'
        WHERE breaker_key='address-a-submit'",
    )
    .execute(&store.pool)
    .await?;
    let deadline = Utc::now().timestamp_millis() + 60_000;
    for id in ["probe-one", "probe-two"] {
        let mut case = pilot(id, "address-pilot", &["address-a", "address-b"]);
        let (asset, _) = store
            .assign_route(&case, CheckKind::Address, deadline, false)
            .await?
            .unwrap();
        assert_eq!(asset.provider, "address-a");
        case.checks.get_mut(&CheckKind::Address).unwrap().evidence = Some(asset);
        store.publish_product(&case).await?;
    }
    let authorize = |case_id: &'static str, op: &'static str, token: &'static str| {
        store.authorize_attempt(AttemptIdentity {
            token,
            case_id,
            operation: op,
            kind: CheckKind::Address,
            provider: "address-a",
            pool: "demo-address-a",
            lane: "submit",
            deadline_ms: deadline,
            max_attempts: 3,
        })
    };
    let (first, second) = tokio::join!(
        authorize("probe-one", "probe-one/address/1", "probe-token-1"),
        authorize("probe-two", "probe-two/address/1", "probe-token-2")
    );
    let (winner, other, op) = match (first?, second?) {
        (
            AttemptAdmission::Send { .. },
            AttemptAdmission::Wait { .. } | AttemptAdmission::Blocked { .. },
        ) => ("probe-token-1", "probe-two", "probe-two/address/1"),
        (
            AttemptAdmission::Wait { .. } | AttemptAdmission::Blocked { .. },
            AttemptAdmission::Send { .. },
        ) => ("probe-token-2", "probe-one", "probe-one/address/1"),
        r => panic!("unexpected probe race {r:?}"),
    };
    store
        .finish_attempt(winner, "ok", serde_json::json!({"ok":true}), 50, 0)
        .await?;
    assert!(
        matches!(authorize(other,op,"after-close").await?,AttemptAdmission::Blocked{reason} if reason=="share_cap")
    );
    let attempts: i64 = sqlx::query_scalar("SELECT count(*) FROM provider_attempt")
        .fetch_one(&store.pool)
        .await?;
    assert_eq!(attempts, 1);
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn expired_authorization_with_zero_physical_sends_leaves_an_actionable_block(
    pool: PgPool,
) -> Result<(), Error> {
    use crate::{AttemptAdmission, AttemptIdentity};
    let store = setup(pool).await?;
    let case = pilot("expired", "address-priority", &["address-a"]);
    let deadline = Utc::now().timestamp_millis() + 60_000;
    store
        .assign_route(&case, CheckKind::Address, deadline, false)
        .await?;
    store.publish_product(&case).await?;
    let op = "expired/address/1";
    let decision = store
        .authorize_attempt(AttemptIdentity {
            token: "last-call",
            case_id: "expired",
            operation: op,
            kind: CheckKind::Address,
            provider: "address-a",
            pool: "demo-address-a",
            lane: "submit",
            deadline_ms: deadline,
            max_attempts: 2,
        })
        .await?;
    assert!(matches!(decision, AttemptAdmission::Send { .. }));
    store.block_unsent_route("expired", op, "deadline").await?;
    assert_eq!(
        store.route_answers("expired", op).await?[0].status,
        "outstanding"
    );
    store
        .finish_attempt(
            "last-call",
            "expired",
            serde_json::json!({"Blocked":"deadline"}),
            0,
            0,
        )
        .await?;
    store.block_unsent_route("expired", op, "deadline").await?;
    let result:(String,bool,String)=sqlx::query_as("SELECT status,possible_send,block_reason FROM provider_route_selection WHERE operation_id=$1")
        .bind(op).fetch_one(&store.pool).await?;
    assert_eq!(result, ("blocked".into(), false, "deadline".into()));
    Ok(())
}

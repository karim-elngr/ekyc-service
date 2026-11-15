//! Run with a PostgreSQL DATABASE_URL and cargo test -p ekyc-postgres --features postgres-tests.
use super::*;
use ekyc_domain::{CheckResult, Verdict};

fn initial_view() -> CaseView {
    CaseView {
        case_id: "owned-case".into(),
        version: 1,
        state: "ACCEPTED".into(),
        purpose: Purpose::AddressRefresh,
        scope: vec![CheckKind::Address],
        checks: vec![CheckSummary {
            kind: CheckKind::Address,
            result: CheckResult::Pending,
        }],
        observations: BTreeMap::new(),
        decision: None,
        policy_version: "v1".into(),
        evaluated_on: None,
        decided_at: None,
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn replay_cannot_overwrite_a_decision_or_reemit_its_event(pool: PgPool) -> Result<(), Error> {
    let store = Store { pool };
    let original = initial_view();
    let mut terminal = original.clone();
    terminal.version = 2;
    terminal.state = "DECIDED".into();
    terminal.decision = Some(Decision {
        verdict: Verdict::Approved,
        reasons: vec![],
    });
    let now = Utc::now();
    terminal.checks[0].result = CheckResult::Pass;
    terminal.evaluated_on = Some(now.date_naive());
    terminal.decided_at = Some(now);

    store.publish(&original).await?;
    let (a, b) = tokio::join!(store.publish(&terminal), store.publish(&terminal));
    a?;
    b?;
    store.publish(&original).await?;
    let mut post_decision = original.clone();
    post_decision.version = 3;
    store.publish(&post_decision).await?;

    let view = store.get(&original.case_id).await?.unwrap();
    assert_eq!(view.version, 2);
    assert_eq!(view.checks, terminal.checks);
    assert_eq!(view.decision.unwrap().verdict, Verdict::Approved);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM case_revision")
            .fetch_one(&store.pool)
            .await?,
        2
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM decision_outbox")
            .fetch_one(&store.pool)
            .await?,
        1
    );
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn only_one_caller_request_wins_the_last_slot(pool: PgPool) -> Result<(), Error> {
    let store = Store { pool };
    let (a, b) = tokio::join!(store.admit("tenant-a", 1), store.admit("tenant-a", 1));
    let results = [a?, b?];
    assert_eq!(results.iter().filter(|r| **r == Gate::Granted).count(), 1);
    assert!(results.iter().any(|r| matches!(r, Gate::Wait { .. })));
    assert_eq!(store.admit("tenant-b", 1).await?, Gate::Granted);
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn lease_limits_apply_across_workers_and_replays(pool: PgPool) -> Result<(), Error> {
    let store = Store { pool };
    let limits = ProviderLimits {
        max_in_flight: 1,
        per_minute: 2,
        ..ProviderLimits::default()
    };
    let k = CheckKind::Face;
    let (a, b) = tokio::join!(
        store.acquire_provider(k, "token-a", limits),
        store.acquire_provider(k, "token-b", limits)
    );
    let (winner, other) = match (a?, b?) {
        (Gate::Granted, Gate::Wait { .. }) => ("token-a", "token-b"),
        (Gate::Wait { .. }, Gate::Granted) => ("token-b", "token-a"),
        result => panic!("unexpected concurrent grants: {result:?}"),
    };
    assert!(matches!(
        store.acquire_provider(k, winner, limits).await?,
        Gate::Wait { .. }
    ));
    store.finish_provider(k, winner, false, limits).await?;
    store.finish_provider(k, winner, false, limits).await?;
    assert_eq!(
        store.acquire_provider(k, winner, limits).await?,
        Gate::Closed
    );
    assert_eq!(
        store.acquire_provider(k, other, limits).await?,
        Gate::Granted
    );
    store.finish_provider(k, other, false, limits).await?;
    assert!(matches!(
        store.acquire_provider(k, "third", limits).await?,
        Gate::Wait { .. }
    ));
    assert_eq!(
        store
            .acquire_provider(CheckKind::Address, "third", limits)
            .await?,
        Gate::Granted
    );
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn half_open_probe_fences_old_results_and_honors_shared_retry_after(
    pool: PgPool,
) -> Result<(), Error> {
    let store = Store { pool };
    let limits = ProviderLimits {
        failure_threshold: 1,
        ..ProviderLimits::default()
    };
    let k = CheckKind::Sanctions;
    assert_eq!(
        store.acquire_provider(k, "old", limits).await?,
        Gate::Granted
    );
    assert_eq!(
        store.acquire_provider(k, "failed", limits).await?,
        Gate::Granted
    );
    store.finish_provider(k, "failed", true, limits).await?;
    store.finish_provider(k, "old", false, limits).await?;
    assert!(matches!(
        store.acquire_provider(k, "held", limits).await?,
        Gate::Wait { .. }
    ));

    sqlx::query("UPDATE provider_gate SET open_until = clock_timestamp() - interval '1 second'")
        .execute(&store.pool)
        .await?;
    assert_eq!(
        store.acquire_provider(k, "probe", limits).await?,
        Gate::Granted
    );
    assert!(matches!(
        store.acquire_provider(k, "other", limits).await?,
        Gate::Wait { .. }
    ));
    store
        .finish_provider_with_backoff(k, "probe", false, limits, Some(5_000))
        .await?;
    assert!(matches!(
        store.acquire_provider(k, "other", limits).await?,
        Gate::Wait { .. }
    ));
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn historical_v1_projection_backfills_without_reinterpreting_evidence(
    pool: PgPool,
) -> Result<(), Error> {
    let snapshot = serde_json::json!({
        "case_id":"legacy", "version":2, "state":"DECIDED", "purpose":"full_onboarding",
        "scope":["document", "face", "address", "sanctions"],
        "observations": {
            "document":{"kind":"document","passed":true,"confidence":95.0,"expires_on":"2026-11-15"},
            "face":{"kind":"missing"},
            "sanctions":{"kind":"unavailable"}
        },
        "decision":{"verdict":"REJECTED","reasons":[
            {"check":"face","code":"MISSING_EVIDENCE"},
            {"check":"sanctions","code":"SANCTIONS_UNAVAILABLE"}
        ]},
        "policy_version":"v1","evaluated_on":"2026-11-15","decided_at":"2026-11-15T12:00:00Z"
    });
    sqlx::query("INSERT INTO case_view (case_id, version, terminal, snapshot) VALUES ('legacy', 2, TRUE, $1)")
        .bind(&snapshot).execute(&pool).await?;
    sqlx::query(include_str!("../migrations/0004_project_check_results.sql"))
        .execute(&pool)
        .await?;
    let view = Store { pool }.get("legacy").await?.unwrap();
    let results: Vec<_> = view.checks.iter().map(|check| check.result).collect();
    assert_eq!(
        results,
        vec![
            CheckResult::Pass,
            CheckResult::Review,
            CheckResult::SkippedAfterDecision,
            CheckResult::Fail
        ]
    );
    Ok(())
}

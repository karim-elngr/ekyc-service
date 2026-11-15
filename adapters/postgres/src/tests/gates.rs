//! Run with a PostgreSQL DATABASE_URL and cargo test -p ekyc-postgres --features postgres-tests.
use crate::{Gate, ProviderLimits, Store};
use ekyc_domain::CheckKind;
use sqlx::{Error, PgPool};
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

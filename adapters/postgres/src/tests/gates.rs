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

#[sqlx::test(migrations = "./migrations")]
async fn concurrent_last_unit_and_replay_charge_once(pool: PgPool) -> Result<(), Error> {
    let present: bool = sqlx::query_scalar("SELECT to_regprocedure('ekyc_authorize_attempt(text,text,text,text,text,text,text,bigint,integer)') IS NOT NULL").fetch_one(&pool).await?;
    assert!(
        present,
        "the transactional one-use dispatch gate must be installed"
    );
    use super::fixtures::{now, open};
    let case = open("dispatch-a", "wallet", &[CheckKind::Face], &[], now());
    let op = case.checks[&CheckKind::Face].operation_id.as_ref().unwrap();
    Store { pool: pool.clone() }
        .restore_product_projection(&case)
        .await?;
    sqlx::raw_sql("INSERT INTO provider_rate_bucket VALUES ('account',2,1,NULL); INSERT INTO provider_circuit(breaker_key) VALUES ('face'); INSERT INTO provider_billing_account VALUES ('invoice','USD','UTC',1,0); INSERT INTO provider_route_contract VALUES ('face','face-vendor','pool','account','face','invoice','usd-v1',1,0,'operation',3,1000,2000)").execute(&pool).await?;
    let authorize = |token: &'static str| {
        sqlx::query_scalar::<_,serde_json::Value>("SELECT ekyc_authorize_attempt($1,'dispatch-a',$2,'face','face-vendor','pool','submit',4102444800000,3)").bind(token).bind(op).fetch_one(&pool)
    };
    let (a, b) = tokio::try_join!(authorize("t1"), authorize("t2"))?;
    assert_eq!([&a, &b].iter().filter(|v| v["state"] == "send").count(), 1);
    let winner = if a["state"] == "send" { "t1" } else { "t2" };
    assert_eq!(authorize(winner).await?["state"], "unknown");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM provider_spend_reservation")
            .fetch_one(&pool)
            .await?,
        1
    );
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn shared_vendor_throttle_and_probe_generation_survive_late_completion(
    pool: PgPool,
) -> Result<(), Error> {
    use super::fixtures::{now, open};
    let case = open(
        "fencing",
        "wallet",
        &[CheckKind::Face, CheckKind::Document],
        &[],
        now(),
    );
    let face = case.checks[&CheckKind::Face].operation_id.as_ref().unwrap();
    let doc = case.checks[&CheckKind::Document]
        .operation_id
        .as_ref()
        .unwrap();
    Store { pool: pool.clone() }
        .restore_product_projection(&case)
        .await?;
    sqlx::raw_sql("INSERT INTO provider_rate_bucket VALUES('shared',100,1,NULL);
        INSERT INTO provider_circuit(breaker_key) VALUES('a'),('b');
        INSERT INTO provider_billing_account VALUES ('invoice','USD','UTC',20,1);
        INSERT INTO provider_route_contract VALUES
        ('face','face-vendor','pool','shared','a','invoice','v1',1,1,'operation',1,5000,5000),
        ('document','document-vendor','pool','shared','b','invoice','v1',1,0,'request',1,5000,5000)")
        .execute(&pool).await?;
    let admit = |token: &'static str, kind: &'static str, lane: &'static str| {
        let (op, provider) = if kind == "face" {
            (face, "face-vendor")
        } else {
            (doc, "document-vendor")
        };
        sqlx::query_scalar::<_, serde_json::Value>(
            "SELECT ekyc_authorize_attempt($1,'fencing',$2,$3,$4,'pool',$5,4102444800000,20)",
        )
        .bind(token)
        .bind(op)
        .bind(kind)
        .bind(provider)
        .bind(lane)
        .fetch_one(&pool)
    };
    let finish = |token: &'static str, class: &'static str| {
        sqlx::query("SELECT ekyc_finish_attempt($1,$2,'{}',5,3000)")
            .bind(token)
            .bind(class)
            .execute(&pool)
    };
    for token in ["a1", "a2"] {
        assert_eq!(admit(token, "face", "submit").await?["state"], "send");
    }
    finish("a2", "transient").await?;
    finish("a1", "ok").await?;
    assert_eq!(
        admit("a3", "face", "submit").await?["reason"],
        "breaker_open"
    );
    assert_eq!(admit("b1", "document", "submit").await?["state"], "send");
    finish("b1", "throttled").await?;
    assert_eq!(
        admit("b2", "document", "submit").await?["reason"],
        "vendor_throttle"
    );
    assert_eq!(
        admit("a4", "face", "lookup").await?["reason"],
        "vendor_throttle"
    );
    sqlx::raw_sql("UPDATE provider_rate_bucket SET throttled_until=NULL; UPDATE provider_circuit SET open_until=now()-interval '1 second' WHERE breaker_key='a'").execute(&pool).await?;
    let (p1, p2) = tokio::try_join!(admit("p1", "face", "submit"), admit("p2", "face", "submit"))?;
    assert_eq!(
        [&p1, &p2].iter().filter(|x| x["state"] == "send").count(),
        1
    );
    let first = if p1["state"] == "send" { "p1" } else { "p2" };
    sqlx::query(
        "UPDATE provider_circuit SET probe_until=now()-interval '1 second' WHERE breaker_key='a'",
    )
    .execute(&pool)
    .await?;
    assert_eq!(admit("p3", "face", "submit").await?["state"], "send");
    finish(first, "ok").await?;
    let active: String =
        sqlx::query_scalar("SELECT probe_token FROM provider_circuit WHERE breaker_key='a'")
            .fetch_one(&pool)
            .await?;
    assert_eq!(active, "p3");
    finish("p3", "ok").await?;
    assert_eq!(admit("p4", "face", "submit").await?["state"], "send");
    assert_eq!(admit("p3", "face", "submit").await?["state"], "saved");
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn simultaneous_invoice_reservations_respect_shared_ceiling_and_lookup_reserve(
    pool: PgPool,
) -> Result<(), Error> {
    use super::fixtures::{now, open};
    for id in ["acct1", "acct2"] {
        Store { pool: pool.clone() }
            .restore_product_projection(&open(id, "wallet", &[CheckKind::Face], &[], now()))
            .await?;
    }
    sqlx::raw_sql(
        "INSERT INTO provider_rate_bucket VALUES('shared',10,1,NULL);
        INSERT INTO provider_circuit(breaker_key) VALUES('a');
        INSERT INTO provider_billing_account VALUES ('invoice','USD','Pacific/Auckland',3,1);
        INSERT INTO provider_route_contract VALUES
        ('face','face-vendor','pool','shared','a','invoice','march',2,1,'operation',3,5000,5000)",
    )
    .execute(&pool)
    .await?;
    let admit = |token: &'static str, id: &'static str, lane: &'static str| {
        sqlx::query_scalar::<_,serde_json::Value>("SELECT ekyc_authorize_attempt($1,$2,$2||'/face/1','face','face-vendor','pool',$3,4102444800000,8)").bind(token).bind(id).bind(lane).fetch_one(&pool)
    };
    let (a, b) = tokio::try_join!(admit("a", "acct1", "submit"), admit("b", "acct2", "submit"))?;
    assert_eq!([&a, &b].iter().filter(|x| x["state"] == "send").count(), 1);
    let (winner, loser) = if a["state"] == "send" {
        ("acct1", "acct2")
    } else {
        ("acct2", "acct1")
    };
    assert_eq!(admit("retry", winner, "submit").await?["state"], "send");
    assert_eq!(admit("lookup", winner, "lookup").await?["state"], "send");
    assert_eq!(
        admit("cap", loser, "submit").await?["reason"],
        "spend_ceiling"
    );
    sqlx::query("UPDATE provider_spend_reservation SET period_start=(period_start-interval '1 month')::date").execute(&pool).await?;
    assert_eq!(admit("next", loser, "submit").await?["state"], "send");
    for _ in 0..2 {
        sqlx::query(
            "SELECT ekyc_reconcile_charge('invoice','op/'||$1||'/face/1','line-0002',2,'USD')",
        )
        .bind(winner)
        .execute(&pool)
        .await?;
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM provider_billing_receipt")
        .fetch_one(&pool)
        .await?;
    assert_eq!(count, 1);
    let state:String=sqlx::query_scalar("SELECT status FROM provider_spend_reservation WHERE operation_id=$1||'/face/1' AND lane='submit'").bind(winner).fetch_one(&pool).await?;
    assert_eq!(state, "confirmed");
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn ordinary_submit_reads_the_committed_revision_after_waiting_for_the_case_lock(
    pool: PgPool,
) -> Result<(), Error> {
    use super::fixtures::{now, open};
    use crate::{AttemptAdmission, AttemptIdentity, ContractBook};
    let store = Store { pool: pool.clone() };
    let mut case = open(
        "lock-case",
        "wallet",
        &[CheckKind::Document, CheckKind::Face],
        &[],
        now(),
    );
    super::fixtures::settle(
        &mut case,
        CheckKind::Document,
        super::fixtures::passing(CheckKind::Document),
        now(),
    );
    case.checks
        .get_mut(&CheckKind::Sanctions)
        .unwrap()
        .evidence
        .as_mut()
        .unwrap()
        .provider = "sanctions".into();
    let op = case.checks[&CheckKind::Sanctions]
        .operation_id
        .clone()
        .unwrap();
    store.restore_product_projection(&case).await?;
    let book: ContractBook =
        serde_json::from_str(include_str!("../../../../config/provider-contracts.json")).unwrap();
    store.install_contracts(&book).await?;
    let mut publishing = pool.begin().await?;
    sqlx::query("INSERT INTO product_case_revision(case_id,version,assessment) SELECT case_id,version+1,assessment FROM product_case_revision WHERE case_id='lock-case'").execute(&mut *publishing).await?;
    sqlx::query("UPDATE product_case SET version=version+1 WHERE case_id='lock-case'")
        .execute(&mut *publishing)
        .await?;
    let pending = tokio::spawn(async move {
        store
            .authorize_attempt(AttemptIdentity {
                token: "submit-sanctions",
                case_id: "lock-case",
                operation: &op,
                kind: CheckKind::Sanctions,
                provider: "sanctions",
                pool: "demo-sanctions",
                lane: "submit",
                deadline_ms: 4102444800000,
                max_attempts: 3,
            })
            .await
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let blocked:bool=sqlx::query_scalar("SELECT EXISTS(SELECT FROM pg_stat_activity WHERE datname=current_database() AND query LIKE 'SELECT ekyc_authorize_attempt%' AND cardinality(pg_blocking_pids(pid))>0)").fetch_one(&pool).await?;
        if blocked {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "authorization did not wait for case publication"
        );
        tokio::task::yield_now().await;
    }
    publishing.commit().await?;
    let decision = pending.await.unwrap()?;
    assert!(
        matches!(decision, AttemptAdmission::Send { .. }),
        "{decision:?}"
    );
    Ok(())
}

use super::fixtures::*;
use crate::{CaseScope, Store};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{types::Json, Error, PgPool};
#[sqlx::test(migrations = false)]
async fn unified_schema_preserves_existing_cases_facts_and_pending_deliveries(
    pool: PgPool,
) -> Result<(), Error> {
    let migrations = sqlx::migrate!("./migrations");
    for migration in migrations.iter().filter(|m| m.version < 7) {
        sqlx::raw_sql(&migration.sql).execute(&pool).await?;
    }
    sqlx::raw_sql("INSERT INTO case_view(case_id,version,terminal,snapshot) VALUES ('retired',2,true,'{}');
        INSERT INTO case_revision(case_id,version,snapshot) VALUES ('retired',2,'{}');
        INSERT INTO decision_outbox(event_id,case_id,version,payload,claim_count,next_attempt_at)
           VALUES('retired:2','retired',2,'{\"event_id\":\"retired:2\",\"event_type\":\"ekyc.case.decided.v1\",\"verdict\":\"APPROVED\"}',3,'2026-11-17T00:00:00Z')")
        .execute(&pool).await?;
    let c = approve_wallet(&format!("v2~wallet~{:064x}", 1));
    let v = c.view();
    sqlx::query("INSERT INTO product_case(case_id,institution,product,subject_ref,version,state,public_view)
        VALUES($1,$2,$3,$4,$5,'APPROVED',$6)")
        .bind(&c.case_id).bind(&c.tenant_id).bind(&c.policy.product).bind(&c.subject_ref).bind(c.revision).bind(Json(&v)).execute(&pool).await?;
    sqlx::query("INSERT INTO product_case_revision(case_id,version,assessment) VALUES($1,$2,$3)")
        .bind(&c.case_id)
        .bind(c.revision)
        .bind(Json(&c))
        .execute(&pool)
        .await?;
    let fact = &c.fresh_passes()[0];
    sqlx::query("INSERT INTO product_fact(operation_id,origin_case_id,institution,subject_ref,fact) VALUES($1,$2,$3,$4,$5)")
        .bind(&fact.operation_id).bind(&c.case_id).bind(&c.tenant_id).bind(&c.subject_ref).bind(Json(fact)).execute(&pool).await?;
    sqlx::query("INSERT INTO decision_outbox(event_id,product_case_id,version,payload) VALUES('active',$1,$2,'{\"event_type\":\"ekyc.case.decided.v2\"}')")
        .bind(&c.case_id).bind(c.revision).execute(&pool).await?;
    let migration = migrations.iter().find(|m| m.version == 7).unwrap();
    sqlx::raw_sql(&migration.sql).execute(&pool).await?;
    let store = Store { pool };
    let scope = CaseScope {
        owner: Some("wallet".into()),
        ..product_scope("institution-a", "wallet")
    };
    assert_eq!(
        store.scoped_case(&c.case_id, &scope).await?,
        Some(v.clone())
    );
    assert_eq!(
        store.product_facts(&c.tenant_id, &c.subject_ref).await?,
        std::slice::from_ref(fact)
    );
    let history = store
        .case_history(&c.case_id, &scope, None, None, 1)
        .await?
        .unwrap();
    assert_eq!(history.revisions[0].view, v);
    let (created, archived_tables_gone, legacy, attempts, next_try, active): (DateTime<Utc>,bool,Json<Value>,i64,DateTime<Utc>,i64) =
        sqlx::query_as("SELECT c.created_at,to_regclass('case_revision') IS NULL AND to_regclass('case_view') IS NULL,e.payload,e.claim_count,e.next_attempt_at,
            (SELECT count(*) FROM decision_outbox WHERE product_case_id=c.case_id AND delivered_at IS NULL)
            FROM product_case c,decision_outbox e WHERE e.event_id='retired:2' AND e.delivered_at IS NULL")
            .fetch_one(&store.pool).await?;
    assert_eq!(created, c.created_at);
    assert!(archived_tables_gone);
    assert_eq!(legacy.0["verdict"], "APPROVED");
    assert_eq!(
        (attempts, next_try, active),
        (3, "2026-11-17T00:00:00Z".parse().unwrap(), 1)
    );
    // An in-flight webhook retains its original fencing token and can complete.
    let delivered=sqlx::query("UPDATE decision_outbox SET delivered_at=now() WHERE event_id='retired:2' AND claim_count=3")
        .execute(&store.pool).await?;
    assert_eq!(delivered.rows_affected(), 1);
    Ok(())
}

/// Exercise the same migrator used by worker and API startup, with v1 fixtures
/// at a genuinely applied migration 6 (including SQLx migration bookkeeping).
#[sqlx::test(migrations = false)]
async fn populated_startup_preserves_every_v1_revision(pool: PgPool) -> Result<(), Error> {
    let mut earlier = sqlx::migrate!("./migrations");
    earlier.migrations = earlier
        .iter()
        .filter(|m| m.version <= 6)
        .cloned()
        .collect::<Vec<_>>()
        .into();
    earlier.run(&pool).await?;
    sqlx::raw_sql("INSERT INTO case_view(case_id,version,terminal,snapshot) VALUES ('archive-case',2,true,'{\"approved\":true}');
                   INSERT INTO case_revision(case_id,version,recorded_at,snapshot) VALUES
                   ('archive-case',1,'2025-03-05T10:11:12Z','{\"requested\":true}'),
                   ('archive-case',2,'2025-03-05T11:12:13Z','{\"approved\":true}')").execute(&pool).await?;
    let expected: Vec<Json<Value>> =
        sqlx::query_scalar("SELECT to_jsonb(r) FROM case_revision r ORDER BY version")
            .fetch_all(&pool)
            .await?;
    let expected_case: Json<Value> = sqlx::query_scalar("SELECT to_jsonb(c) FROM case_view c")
        .fetch_one(&pool)
        .await?;
    let (api, worker) = tokio::join!(
        Store::from_pool(pool.clone()),
        Store::from_pool(pool.clone())
    );
    api?;
    worker?;
    let archived: Vec<Json<Value>> = sqlx::query_scalar(
        "SELECT to_jsonb(r) FROM ekyc_v1_archive.case_revision r ORDER BY version",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(archived, expected);
    let archived_case: Json<Value> =
        sqlx::query_scalar("SELECT to_jsonb(c) FROM ekyc_v1_archive.case_view c")
            .fetch_one(&pool)
            .await?;
    assert_eq!(archived_case, expected_case);
    Ok(())
}

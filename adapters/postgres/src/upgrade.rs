//! Hold the SQLx deployment lock and both source-table locks from archive
//! selection through migration 0007. A failed archive or migration rolls back both.
use sqlx::{migrate::Migrate, Acquire, Error, PgPool};

pub(super) async fn migrate(pool: &PgPool) -> Result<(), Error> {
    let mut conn = pool.acquire().await?;
    // SQLx's deployment lock is session-scoped. Disconnect even on cancellation.
    conn.close_on_drop();
    conn.lock().await?;
    let mut tx = conn.begin().await?;
    let legacy: bool = sqlx::query_scalar("SELECT to_regclass('case_view') IS NOT NULL")
        .fetch_one(&mut *tx)
        .await?;
    if legacy {
        sqlx::query("LOCK TABLE case_view, case_revision IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *tx)
            .await?;
        let populated: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT FROM case_view) OR EXISTS(SELECT FROM case_revision)",
        )
        .fetch_one(&mut *tx)
        .await?;
        if populated {
            sqlx::raw_sql(include_str!("archive_v1.sql"))
                .execute(&mut *tx)
                .await?;
        }
    }
    sqlx::migrate!("./migrations")
        .set_locking(false)
        .run_direct(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

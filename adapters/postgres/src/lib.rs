//! PostgreSQL read models, an idempotent decision-event outbox, and shared dispatch limits.
mod contracts;
pub use contracts::{AttemptAdmission, AttemptIdentity, ContractBook, PoolLimits};
mod directory;
mod routing;
mod traffic;
pub use traffic::TrafficChange;
mod investigations;
pub use investigations::Investigation;
mod reuse;
mod upgrade;
pub use reuse::ReuseQuery;
mod product;
mod strict_screen;
pub use directory::{CaseFilter, CaseOverview, CasePage, CaseScope, HistoryPage, PageOrder};
use ekyc_domain::CheckKind;
use serde::{Deserialize, Serialize};
use sqlx::{postgres::PgPoolOptions, types::Json, Error, PgPool};
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Gate {
    Granted,
    Wait {
        retry_after_ms: u64,
    },
    /// This particular lease token already finished or expired.
    Closed,
}

impl Gate {
    fn from_result(result: i64) -> Self {
        match result {
            0 => Self::Granted,
            n if n > 0 => Self::Wait {
                retry_after_ms: n as u64,
            },
            _ => Self::Closed,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ProviderLimits {
    pub per_minute: u32,
    pub max_in_flight: u32,
    pub lease_ms: u64,
    pub failure_threshold: u32,
    pub cooldown_ms: u64,
}

impl Default for ProviderLimits {
    fn default() -> Self {
        Self {
            per_minute: 120,
            max_in_flight: 8,
            lease_ms: 15_000,
            failure_threshold: 5,
            cooldown_ms: 10_000,
        }
    }
}

#[derive(Clone)]
pub struct Store {
    pool: PgPool,
}

impl Store {
    pub async fn connect(database_url: &str) -> Result<Self, Error> {
        let pool = PgPoolOptions::new()
            .max_connections(16)
            .acquire_timeout(Duration::from_secs(5))
            .connect(database_url)
            .await?;
        Self::from_pool(pool).await
    }

    pub async fn from_pool(pool: PgPool) -> Result<Self, Error> {
        upgrade::migrate(&pool).await?;
        Ok(Self { pool })
    }

    pub async fn ready(&self) -> Result<(), Error> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    pub async fn admit(&self, caller: &str, limit_per_minute: u32) -> Result<Gate, Error> {
        if limit_per_minute == 0 {
            return Ok(Gate::Wait {
                retry_after_ms: 60_000,
            });
        }
        let result: i64 = sqlx::query_scalar("SELECT ekyc_admit($1, $2)")
            .bind(caller)
            .bind(i64::from(limit_per_minute))
            .fetch_one(&self.pool)
            .await?;
        Ok(Gate::from_result(result))
    }

    pub async fn acquire_provider(
        &self,
        kind: CheckKind,
        token: &str,
        limits: ProviderLimits,
    ) -> Result<Gate, Error> {
        if limits.per_minute == 0 || limits.max_in_flight == 0 {
            return Ok(Gate::Wait {
                retry_after_ms: 60_000,
            });
        }
        if limits.lease_ms == 0 || limits.failure_threshold == 0 || limits.cooldown_ms == 0 {
            return Err(Error::Protocol(
                "provider lease, cooldown and failure threshold must be positive".into(),
            ));
        }
        let result: i64 = sqlx::query_scalar("SELECT ekyc_acquire_provider($1,$2,$3,$4,$5)")
            .bind(kind.as_str())
            .bind(token)
            .bind(i64::from(limits.per_minute))
            .bind(i64::from(limits.max_in_flight))
            .bind(duration_ms(limits.lease_ms)?)
            .fetch_one(&self.pool)
            .await?;
        Ok(Gate::from_result(result))
    }

    pub async fn finish_provider(
        &self,
        kind: CheckKind,
        token: &str,
        failed: bool,
        limits: ProviderLimits,
    ) -> Result<(), Error> {
        self.finish_provider_with_backoff(kind, token, failed, limits, None)
            .await
    }

    pub async fn finish_provider_with_backoff(
        &self,
        kind: CheckKind,
        token: &str,
        failed: bool,
        limits: ProviderLimits,
        retry_after_ms: Option<u64>,
    ) -> Result<(), Error> {
        sqlx::query("SELECT ekyc_finish_provider($1,$2,$3,$4,$5,$6)")
            .bind(kind.as_str())
            .bind(token)
            .bind(failed)
            .bind(i64::from(limits.failure_threshold))
            .bind(duration_ms(limits.cooldown_ms)?)
            .bind(retry_after_ms.map(duration_ms).transpose()?)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

fn duration_ms(value: u64) -> Result<i64, Error> {
    // Keep configured intervals inside PostgreSQL timestamp arithmetic bounds.
    i64::try_from(value.min(365 * 24 * 60 * 60 * 1000))
        .map_err(|_| Error::Protocol("invalid provider interval".into()))
}

#[cfg(all(test, feature = "postgres-tests"))]
mod tests;

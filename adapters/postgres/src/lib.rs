//! PostgreSQL read models, an idempotent decision-event outbox, and shared dispatch limits.
use chrono::{DateTime, NaiveDate, Utc};
use ekyc_domain::{CheckKind, CheckSummary, Decision, Observation, Purpose};
use serde::{Deserialize, Serialize};
use sqlx::{postgres::PgPoolOptions, types::Json, Error, PgPool};
use std::{collections::BTreeMap, time::Duration};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CaseView {
    pub case_id: String,
    pub version: i64,
    pub state: String,
    pub purpose: Purpose,
    pub scope: Vec<CheckKind>,
    pub checks: Vec<CheckSummary>,
    pub observations: BTreeMap<CheckKind, Observation>,
    pub decision: Option<Decision>,
    pub policy_version: String,
    pub evaluated_on: Option<NaiveDate>,
    pub decided_at: Option<DateTime<Utc>>,
}

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
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool })
    }

    pub async fn ready(&self) -> Result<(), Error> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    pub async fn get(&self, case_id: &str) -> Result<Option<CaseView>, Error> {
        let row: Option<Json<CaseView>> =
            sqlx::query_scalar("SELECT snapshot FROM case_view WHERE case_id = $1")
                .bind(case_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|value| value.0))
    }

    /// Apply one authoritative snapshot. The row lock implied by ON CONFLICT
    /// serializes writers; replayed or outdated versions do not produce events.
    pub async fn publish(&self, view: &CaseView) -> Result<(), Error> {
        let mut tx = self.pool.begin().await?;
        let changed = sqlx::query(
            "INSERT INTO case_view (case_id, version, terminal, snapshot) VALUES ($1, $2, $3, $4)
             ON CONFLICT (case_id) DO UPDATE SET
                version = excluded.version, terminal = excluded.terminal, snapshot = excluded.snapshot
             WHERE case_view.version < excluded.version AND NOT case_view.terminal",
        )
        .bind(&view.case_id)
        .bind(view.version)
        .bind(view.decision.is_some())
        .bind(Json(view))
        .execute(&mut *tx)
        .await?;

        if changed.rows_affected() == 1 {
            sqlx::query(
                "INSERT INTO case_revision (case_id, version, snapshot) VALUES ($1, $2, $3)",
            )
            .bind(&view.case_id)
            .bind(view.version)
            .bind(Json(view))
            .execute(&mut *tx)
            .await?;
            if let Some(decision) = &view.decision {
                let event_id = format!("{}:{}", view.case_id, view.version);
                let event = serde_json::json!({
                    "event_id": event_id,
                    "event_type": "ekyc.case.decided.v1",
                    "case_id": view.case_id,
                    "version": view.version,
                    "purpose": view.purpose,
                    "scope": view.scope,
                    "verdict": decision.verdict,
                    "policy_version": view.policy_version,
                    "decided_at": view.decided_at
                });
                sqlx::query(
                    "INSERT INTO decision_outbox (event_id, case_id, version, payload)
                     VALUES ($1, $2, $3, $4)",
                )
                .bind(event_id)
                .bind(&view.case_id)
                .bind(view.version)
                .bind(event)
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await
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

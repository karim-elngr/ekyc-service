use super::{Error, Store};
use ekyc_domain::CheckKind;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::types::Json;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolLimits {
    pub total: i32,
    pub submit: i32,
    pub lookup: i32,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateContract {
    pub per_minute: i32,
    pub lookup_reserve: i32,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BillingContract {
    pub currency: String,
    pub timezone: String,
    pub monthly_ceiling: i64,
    pub lookup_reserve: i64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteContract {
    pub kind: CheckKind,
    pub provider: String,
    pub pool: String,
    pub rate_bucket: String,
    pub breaker_key: String,
    pub billing_account: String,
    pub tariff_revision: String,
    pub submit_cost: i64,
    pub lookup_cost: i64,
    pub billing_unit: String,
    pub failure_threshold: i32,
    pub cooldown_ms: i32,
    pub probe_ms: i32,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContractBook {
    pub pools: BTreeMap<String, PoolLimits>,
    pub rates: BTreeMap<String, RateContract>,
    pub billing: BTreeMap<String, BillingContract>,
    pub routes: Vec<RouteContract>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AttemptAdmission {
    Send { deadline_ms: i64 },
    Wait { reason: String, retry_ms: u64 },
    Blocked { reason: String },
    Unknown,
    Saved { result: Value },
}

pub struct AttemptIdentity<'a> {
    pub token: &'a str,
    pub case_id: &'a str,
    pub operation: &'a str,
    pub kind: CheckKind,
    pub provider: &'a str,
    pub pool: &'a str,
    pub lane: &'a str,
    pub deadline_ms: i64,
    pub max_attempts: u32,
}

impl Store {
    /// Install missing synthetic contract domains without resetting live ceilings or breaker state.
    pub async fn install_contracts(&self, book: &ContractBook) -> Result<(), Error> {
        let mut tx = self.pool.begin().await?;
        for (id, rate) in &book.rates {
            sqlx::query("INSERT INTO provider_rate_bucket(bucket,per_minute,lookup_reserve) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
                .bind(id).bind(rate.per_minute).bind(rate.lookup_reserve).execute(&mut *tx).await?;
        }
        for (id, billing) in &book.billing {
            let valid: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT FROM pg_timezone_names WHERE name=$1)")
                    .bind(&billing.timezone)
                    .fetch_one(&mut *tx)
                    .await?;
            if !valid {
                return Err(Error::Protocol("unknown billing timezone".into()));
            }
            sqlx::query("INSERT INTO provider_billing_account VALUES($1,$2,$3,$4,$5) ON CONFLICT DO NOTHING")
                .bind(id).bind(&billing.currency).bind(&billing.timezone).bind(billing.monthly_ceiling)
                .bind(billing.lookup_reserve).execute(&mut *tx).await?;
        }
        for route in &book.routes {
            sqlx::query(
                "INSERT INTO provider_circuit(breaker_key) VALUES($1) ON CONFLICT DO NOTHING",
            )
            .bind(&route.breaker_key)
            .execute(&mut *tx)
            .await?;
            sqlx::query("INSERT INTO provider_route_contract VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) ON CONFLICT DO NOTHING")
                .bind(route.kind.as_str()).bind(&route.provider).bind(&route.pool).bind(&route.rate_bucket)
                .bind(&route.breaker_key).bind(&route.billing_account).bind(&route.tariff_revision)
                .bind(route.submit_cost).bind(route.lookup_cost).bind(&route.billing_unit)
                .bind(route.failure_threshold).bind(route.cooldown_ms).bind(route.probe_ms)
                .execute(&mut *tx).await?;
        }
        tx.commit().await
    }

    pub async fn authorize_attempt(
        &self,
        id: AttemptIdentity<'_>,
    ) -> Result<AttemptAdmission, Error> {
        let reply: Json<AttemptAdmission> =
            sqlx::query_scalar("SELECT ekyc_authorize_attempt($1,$2,$3,$4,$5,$6,$7,$8,$9)")
                .bind(id.token)
                .bind(id.case_id)
                .bind(id.operation)
                .bind(id.kind.as_str())
                .bind(id.provider)
                .bind(id.pool)
                .bind(id.lane)
                .bind(id.deadline_ms)
                .bind(id.max_attempts as i32)
                .fetch_one(&self.pool)
                .await?;
        Ok(reply.0)
    }

    pub async fn finish_attempt(
        &self,
        token: &str,
        class: &str,
        result: impl Serialize,
        elapsed_ms: i64,
        backoff_ms: u64,
    ) -> Result<(), Error> {
        sqlx::query("SELECT ekyc_finish_attempt($1,$2,$3,$4,$5)")
            .bind(token)
            .bind(class)
            .bind(Json(result))
            .bind(elapsed_ms)
            .bind(super::duration_ms(backoff_ms)?)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

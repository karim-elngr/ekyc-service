//! At-least-once delivery from the PostgreSQL decision outbox to one operator-configured sink.

use reqwest::{header::RETRY_AFTER, Client, Url};
use serde_json::Value;
use sqlx::{postgres::PgPoolOptions, types::Json, PgPool};
use std::{
    env,
    error::Error,
    io,
    time::{Duration, SystemTime},
};
use tokio::time::sleep;

type ClaimedEvent = (String, i64, Json<Value>);

const CLAIM: &str = "
    WITH next AS (
        SELECT event_id FROM decision_outbox
        WHERE delivered_at IS NULL AND next_attempt_at <= now()
          AND (claim_until IS NULL OR claim_until <= now())
        ORDER BY next_attempt_at, created_at, event_id LIMIT 1 FOR UPDATE SKIP LOCKED
    )
    UPDATE decision_outbox AS o
    SET claim_count = claim_count + 1, claim_until = now() + interval '30 seconds'
    FROM next WHERE o.event_id = next.event_id
    RETURNING o.event_id, o.claim_count, o.payload";

fn config(name: &str) -> io::Result<String> {
    env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("{name} is required")))
}

fn retry_seconds(attempt: i64, retry_after: Option<&str>) -> i64 {
    // Exponential application backoff stops growing at five minutes. A longer
    // explicit receiver cooldown takes precedence. Clamp only to a DB-safe horizon.
    let exponent = (attempt - 1).clamp(0, 9) as u32;
    let backoff = (1i64 << exponent).min(300);
    let hint = retry_after
        .and_then(|value| {
            let value = value.trim();
            if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
                return Some(value.parse::<u64>().unwrap_or(u64::MAX));
            }
            let deadline = httpdate::parse_http_date(value).ok()?;
            let delay = deadline
                .duration_since(SystemTime::now())
                .unwrap_or_default();
            Some(
                delay
                    .as_secs()
                    .saturating_add(u64::from(delay.subsec_nanos() != 0)),
            )
        })
        .unwrap_or(0);
    backoff.max(hint.min(365 * 24 * 60 * 60) as i64)
}

async fn deliver(
    pool: &PgPool,
    client: &Client,
    url: &Url,
    token: &str,
) -> Result<bool, sqlx::Error> {
    // This single autocommit statement ends before the external request starts.
    let Some((id, claim, Json(payload))) = sqlx::query_as::<_, ClaimedEvent>(CLAIM)
        .fetch_optional(pool)
        .await?
    else {
        return Ok(false);
    };
    let result = client
        .post(url.clone())
        .bearer_auth(token)
        .header("Idempotency-Key", id.as_str())
        .json(&payload)
        .send()
        .await;
    match result {
        Ok(response) if response.status().is_success() => {
            sqlx::query(
                "UPDATE decision_outbox SET delivered_at = now(), claim_until = NULL
                 WHERE event_id = $1 AND claim_count = $2 AND delivered_at IS NULL",
            )
            .bind(&id)
            .bind(claim)
            .execute(pool)
            .await?;
        }
        other => {
            let response = other.as_ref().ok();
            let hint = response
                .and_then(|r| r.headers().get(RETRY_AFTER))
                .and_then(|v| v.to_str().ok());
            let delay = retry_seconds(claim, hint);
            let status = response.map(|r| r.status().as_u16());
            eprintln!("webhook pending; attempt={claim} status={status:?} next_seconds={delay}");
            sqlx::query(
                "UPDATE decision_outbox SET claim_until = NULL,
                     next_attempt_at = now() + $3::bigint * interval '1 second'
                 WHERE event_id = $1 AND claim_count = $2 AND delivered_at IS NULL",
            )
            .bind(&id)
            .bind(claim)
            .bind(delay)
            .execute(pool)
            .await?;
        }
    }
    Ok(true)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let url = Url::parse(&config("WEBHOOK_URL")?)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid webhook endpoint").into());
    }
    let token = config("WEBHOOK_TOKEN")?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(5))
        .connect_lazy(&config("DATABASE_URL")?)?;
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .build()?;
    let mut idle_ms: u64 = 100;
    loop {
        match deliver(&pool, &client, &url, &token).await {
            Ok(true) => {
                idle_ms = 100;
                continue;
            }
            Err(_) => {
                eprintln!("webhook database unavailable; leaving outstanding deliveries for retry")
            }
            Ok(false) => {}
        }
        sleep(Duration::from_millis(idle_ms)).await;
        idle_ms = (idle_ms * 2).min(2000);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_delay_caps_growth_and_honors_receiver_cooldown() {
        assert_eq!(retry_seconds(1, None), 1);
        assert_eq!(retry_seconds(6, None), 32);
        assert_eq!(retry_seconds(1000, None), 300);
        assert_eq!(retry_seconds(1000, Some("600")), 600);
        assert_eq!(retry_seconds(1, Some("invalid")), 1);
    }
}

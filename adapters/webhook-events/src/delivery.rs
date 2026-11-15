use crate::retry::retry_seconds;
use reqwest::{header::RETRY_AFTER, Client, Url};
use serde_json::Value;
use sqlx::{types::Json, PgPool};

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

pub(crate) async fn deliver(
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

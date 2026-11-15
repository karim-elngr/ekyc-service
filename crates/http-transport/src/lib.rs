//! Shared HTTP mechanics. Vendor request/response schemas live in the adapters.

use ekyc_ports::{ProviderError, ProviderErrorKind};
use reqwest::{header::RETRY_AFTER, redirect::Policy, Client, StatusCode};
use serde::{de::DeserializeOwned, Serialize};
use std::time::{Duration, SystemTime};

const MAX_RESPONSE_BYTES: usize = 64 * 1024;

#[derive(Clone)]
pub struct Transport {
    client: Client,
}

impl Transport {
    pub fn new(timeout: Duration) -> Result<Self, String> {
        if timeout.is_zero() {
            return Err("provider timeout must be positive".into());
        }
        let client = Client::builder()
            .timeout(timeout)
            .connect_timeout(timeout.min(Duration::from_secs(2)))
            .pool_idle_timeout(Duration::from_secs(60))
            .pool_max_idle_per_host(16)
            .redirect(Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .map_err(|_| "could not configure provider HTTP client".to_string())?;
        Ok(Self { client })
    }

    /// Register an asynchronous check. Only an explicit acceptance opens a callback wait.
    pub async fn post_accepted<I: Serialize + Sync>(
        &self,
        endpoint: &str,
        operation_id: &str,
        input: &I,
    ) -> Result<(), ProviderError> {
        let response = self.post_response(endpoint, operation_id, input).await?;
        if response.status() == StatusCode::ACCEPTED {
            Ok(())
        } else {
            Err(permanent())
        }
    }

    async fn post_response<I: Serialize + Sync>(
        &self,
        endpoint: &str,
        operation_id: &str,
        input: &I,
    ) -> Result<reqwest::Response, ProviderError> {
        let response = self
            .client
            .post(endpoint)
            .header("Idempotency-Key", operation_id)
            .json(input)
            .send()
            .await
            .map_err(transport_error)?;
        if !response.status().is_success() {
            let retry_after = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| retry_after_ms(value, SystemTime::now()));
            return Err(status_error(response.status(), retry_after));
        }
        Ok(response)
    }

    pub async fn post_bytes<I: Serialize + Sync>(
        &self,
        endpoint: &str,
        operation_id: &str,
        input: &I,
    ) -> Result<Vec<u8>, ProviderError> {
        let mut response = self.post_response(endpoint, operation_id, input).await?;
        if response
            .content_length()
            .is_some_and(|size| size > MAX_RESPONSE_BYTES as u64)
        {
            return Err(permanent());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
            if chunk.len() > MAX_RESPONSE_BYTES - bytes.len() {
                return Err(permanent());
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

pub fn decode_json<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ProviderError> {
    serde_json::from_slice(bytes).map_err(|_| permanent())
}

fn permanent() -> ProviderError {
    ProviderError {
        kind: ProviderErrorKind::Permanent,
        retry_after_ms: None,
    }
}

fn transport_error(error: reqwest::Error) -> ProviderError {
    // Builder failures include malformed endpoints and invalid headers. A lost
    // connection or truncated body may have followed a successful submission.
    if error.is_builder() {
        return permanent();
    }
    ProviderError {
        kind: ProviderErrorKind::Transient,
        retry_after_ms: None,
    }
}

fn status_error(status: StatusCode, retry_after_ms: Option<u64>) -> ProviderError {
    let kind = if status == StatusCode::TOO_MANY_REQUESTS {
        ProviderErrorKind::Throttled
    } else if status.is_server_error() || status == StatusCode::REQUEST_TIMEOUT {
        ProviderErrorKind::Transient
    } else {
        return permanent();
    };
    ProviderError {
        kind,
        retry_after_ms,
    }
}

fn retry_after_ms(value: &str, now: SystemTime) -> Option<u64> {
    let value = value.trim();
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        // An extremely long valid delay means no retry within our case budget.
        let seconds = value.parse::<u64>().unwrap_or(u64::MAX);
        return Some(seconds.saturating_mul(1000));
    }
    let deadline = httpdate::parse_http_date(value).ok()?;
    Some(
        deadline
            .duration_since(now)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    #[test]
    fn retry_after_supports_http_dates_and_seconds() {
        let now = UNIX_EPOCH + Duration::from_secs(10);
        assert_eq!(retry_after_ms("2", now), Some(2000));
        assert_eq!(
            retry_after_ms("Thu, 01 Jan 1970 00:00:13 GMT", now),
            Some(3000)
        );
        assert_eq!(
            retry_after_ms("Thu, 01 Jan 1970 00:00:00 GMT", now),
            Some(0)
        );
        assert_eq!(retry_after_ms("invalid", now), None);
    }

    #[test]
    fn rate_limits_are_distinct_from_failures() {
        let throttled = status_error(StatusCode::TOO_MANY_REQUESTS, Some(1000));
        assert_eq!(throttled.kind, ProviderErrorKind::Throttled);
        assert_eq!(throttled.retry_after_ms, Some(1000));
        assert_eq!(
            status_error(StatusCode::SERVICE_UNAVAILABLE, None).kind,
            ProviderErrorKind::Transient
        );
        assert_eq!(status_error(StatusCode::BAD_REQUEST, None), permanent());
    }
}

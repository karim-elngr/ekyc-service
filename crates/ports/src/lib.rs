//! The outbound verification port. Each provider owns its wire contract.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ekyc_domain::{CheckKind, Observation};
use serde::{Deserialize, Serialize};
use std::fmt;

/// The vendor assigns this instant exactly once, including for a cached result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evaluated<T> {
    pub evaluated_at: DateTime<Utc>,
    #[serde(flatten)]
    pub result: T,
}

impl<T> Evaluated<T> {
    pub fn map<R>(self, f: impl FnOnce(T) -> R) -> Evaluated<R> {
        Evaluated {
            evaluated_at: self.evaluated_at,
            result: f(self.result),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ScreeningLookup {
    Pending,
    NotAccepted,
    Inconclusive,
    Complete(Evaluated<Observation>),
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn kind(&self) -> CheckKind;

    /// Decode a successful vendor response, whether delivered inline or by callback.
    fn decode(&self, body: &[u8]) -> Result<Evaluated<Observation>, ProviderError>;

    /// Read-only authoritative operation lookup for supported address and sanctions vendors.
    async fn screening_result(
        &self,
        _operation_id: &str,
    ) -> Result<ScreeningLookup, ProviderError> {
        Err(ProviderError {
            kind: ProviderErrorKind::Permanent,
            retry_after_ms: None,
        })
    }

    /// Operation IDs are stable across retries of one logical check.
    async fn check(
        &self,
        evidence_ref: &str,
        operation_id: &str,
    ) -> Result<Evaluated<Observation>, ProviderError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorKind {
    Transient,
    Throttled,
    Permanent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    pub retry_after_ms: Option<u64>,
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self.kind {
            ProviderErrorKind::Transient => "provider temporarily unavailable",
            ProviderErrorKind::Throttled => "provider rate limited the request",
            ProviderErrorKind::Permanent => "provider could not complete the check",
        };
        f.write_str(message)
    }
}

impl std::error::Error for ProviderError {}

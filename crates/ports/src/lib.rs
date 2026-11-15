//! The outbound verification port. Each provider owns its wire contract.

use async_trait::async_trait;
use ekyc_domain::{CheckKind, Observation};
use serde::{Deserialize, Serialize};
use std::fmt;

#[async_trait]
pub trait Provider: Send + Sync {
    fn kind(&self) -> CheckKind;

    /// Operation IDs are stable across retries of one logical check.
    async fn check(
        &self,
        evidence_ref: &str,
        operation_id: &str,
    ) -> Result<Observation, ProviderError>;
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

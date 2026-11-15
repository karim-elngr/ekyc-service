use ekyc_domain::CheckKind;
use ekyc_postgres::ProviderLimits;
use serde::{Deserialize, Serialize};
use std::env;
use std::io::{Error, ErrorKind};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct RetrySettings {
    pub case_deadline_ms: u64,
    pub max_attempts: u32,
    pub retry_base_ms: u64,
}

impl RetrySettings {
    pub fn from_env() -> Result<Self, Error> {
        Ok(Self {
            case_deadline_ms: number("CASE_DEADLINE_MS", 30_000, 86_400_000)?,
            max_attempts: number("MAX_ATTEMPTS", 3, 32)? as u32,
            retry_base_ms: number("RETRY_BASE_MS", 250, 300_000)?,
        })
    }
}

pub fn provider_limits(kind: CheckKind, timeout_ms: u64) -> Result<ProviderLimits, Error> {
    let prefix = kind.as_str().to_ascii_uppercase();
    let per_minute = number("PROVIDER_PER_MINUTE", 60, 1_000_000)?;
    let in_flight = number("PROVIDER_MAX_IN_FLIGHT", 4, 10_000)?;
    Ok(ProviderLimits {
        per_minute: number(&format!("{prefix}_PER_MINUTE"), per_minute, 1_000_000)? as u32,
        max_in_flight: number(&format!("{prefix}_MAX_IN_FLIGHT"), in_flight, 10_000)? as u32,
        lease_ms: timeout_ms + 2_000,
        failure_threshold: number("BREAKER_FAILURES", 3, 1000)? as u32,
        cooldown_ms: number("BREAKER_COOLDOWN_MS", 5000, 3_600_000)?,
    })
}

pub fn number(key: &str, default: u64, max: u64) -> Result<u64, Error> {
    match env::var(key) {
        Ok(text) => text
            .parse::<u64>()
            .ok()
            .filter(|v| (1..=max).contains(v))
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::InvalidInput,
                    format!("{key} must be in 1..={max}"),
                )
            }),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(e) => Err(Error::new(ErrorKind::InvalidInput, e)),
    }
}

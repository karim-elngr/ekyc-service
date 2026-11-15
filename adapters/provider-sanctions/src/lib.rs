use async_trait::async_trait;
use ekyc_domain::{CheckKind, Observation};
use ekyc_http_transport::Transport;
use ekyc_ports::{Provider, ProviderError, ProviderErrorKind};
use serde::{Deserialize, Serialize};

pub struct SanctionsProvider {
    transport: Transport,
    endpoint: String,
}

impl SanctionsProvider {
    pub fn new(transport: Transport, base_url: String) -> Self {
        let endpoint = format!("{}/screenings", base_url.trim_end_matches('/'));
        Self {
            transport,
            endpoint,
        }
    }
}

#[derive(Serialize)]
struct Request<'a> {
    identity_ref: &'a str,
}

#[derive(Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum ScreeningStatus {
    Clear,
    Hit,
}

#[derive(Deserialize)]
struct Response {
    screening_status: ScreeningStatus,
    match_count: u32,
}

#[async_trait]
impl Provider for SanctionsProvider {
    fn kind(&self) -> CheckKind {
        CheckKind::Sanctions
    }

    async fn check(
        &self,
        evidence_ref: &str,
        operation_id: &str,
    ) -> Result<Observation, ProviderError> {
        let input = Request {
            identity_ref: evidence_ref,
        };
        let response: Response = self
            .transport
            .post(&self.endpoint, operation_id, &input)
            .await?;
        match (response.screening_status, response.match_count) {
            (ScreeningStatus::Clear, 0) => Ok(Observation::Sanctions { hit: false }),
            (ScreeningStatus::Hit, 1..) => Ok(Observation::Sanctions { hit: true }),
            _ => Err(ProviderError {
                kind: ProviderErrorKind::Permanent,
                retry_after_ms: None,
            }),
        }
    }
}

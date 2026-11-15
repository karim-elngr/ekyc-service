use async_trait::async_trait;
use ekyc_domain::{CheckKind, Observation};
use ekyc_http_transport::Transport;
use ekyc_ports::{Provider, ProviderError};
use serde::{Deserialize, Serialize};

pub struct FaceProvider {
    transport: Transport,
    endpoint: String,
}

impl FaceProvider {
    pub fn new(transport: Transport, base_url: String) -> Self {
        let endpoint = format!("{}/faces/compare", base_url.trim_end_matches('/'));
        Self {
            transport,
            endpoint,
        }
    }
}

#[derive(Serialize)]
struct Request<'a> {
    capture_ref: &'a str,
}

#[derive(Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum Status {
    Pass,
    Fail,
    Review,
}

#[derive(Deserialize)]
struct Response {
    match_status: Status,
    confidence: f64,
    similarity_score: f64,
}

#[async_trait]
impl Provider for FaceProvider {
    fn kind(&self) -> CheckKind {
        CheckKind::Face
    }

    async fn check(
        &self,
        evidence_ref: &str,
        operation_id: &str,
    ) -> Result<Observation, ProviderError> {
        let input = Request {
            capture_ref: evidence_ref,
        };
        let response: Response = self
            .transport
            .post(&self.endpoint, operation_id, &input)
            .await?;
        Ok(Observation::Face {
            passed: matches!(response.match_status, Status::Pass),
            confidence: response.confidence,
            similarity: response.similarity_score,
        })
    }
}

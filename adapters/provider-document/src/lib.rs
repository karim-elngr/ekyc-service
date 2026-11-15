use async_trait::async_trait;
use chrono::NaiveDate;
use ekyc_domain::{CheckKind, Observation};
use ekyc_http_transport::Transport;
use ekyc_ports::{Provider, ProviderError};
use serde::{Deserialize, Serialize};

pub struct DocumentProvider {
    transport: Transport,
    endpoint: String,
}

impl DocumentProvider {
    pub fn new(transport: Transport, base_url: String) -> Self {
        let endpoint = format!("{}/documents/verify", base_url.trim_end_matches('/'));
        Self {
            transport,
            endpoint,
        }
    }
}

#[derive(Serialize)]
struct Request<'a> {
    document_ref: &'a str,
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
    status: Status,
    confidence: f64,
    expires_on: NaiveDate,
}

#[async_trait]
impl Provider for DocumentProvider {
    fn kind(&self) -> CheckKind {
        CheckKind::Document
    }

    async fn check(
        &self,
        evidence_ref: &str,
        operation_id: &str,
    ) -> Result<Observation, ProviderError> {
        let input = Request {
            document_ref: evidence_ref,
        };
        let response: Response = self
            .transport
            .post(&self.endpoint, operation_id, &input)
            .await?;
        Ok(Observation::Document {
            passed: matches!(response.status, Status::Pass),
            confidence: response.confidence,
            expires_on: response.expires_on,
        })
    }
}

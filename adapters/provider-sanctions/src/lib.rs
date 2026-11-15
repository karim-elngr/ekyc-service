use async_trait::async_trait;
use ekyc_domain::{CheckKind, Observation};
use ekyc_http_transport::{decode_json, Transport};
use ekyc_ports::{Evaluated, Provider, ProviderError, ProviderErrorKind, ScreeningLookup};
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
    ) -> Result<Evaluated<Observation>, ProviderError> {
        let input = Request {
            identity_ref: evidence_ref,
        };
        let body = self
            .transport
            .post_bytes(&self.endpoint, operation_id, &input)
            .await?;
        self.decode(&body)
    }

    fn decode(&self, body: &[u8]) -> Result<Evaluated<Observation>, ProviderError> {
        let reply: Evaluated<Response> = decode_json(body)?;
        decode(reply)
    }

    async fn screening_result(&self, operation_id: &str) -> Result<ScreeningLookup, ProviderError> {
        #[derive(Serialize)]
        struct LookupRequest<'a> {
            operation_id: &'a str,
        }
        #[derive(Deserialize)]
        #[serde(tag = "status", rename_all = "snake_case")]
        enum Lookup {
            Pending,
            NotAccepted,
            Inconclusive,
            Complete { result: Evaluated<Response> },
        }
        let body = self
            .transport
            .post_bytes(
                &format!("{}/result", self.endpoint),
                operation_id,
                &LookupRequest { operation_id },
            )
            .await?;
        Ok(match decode_json(&body)? {
            Lookup::Pending => ScreeningLookup::Pending,
            Lookup::NotAccepted => ScreeningLookup::NotAccepted,
            Lookup::Inconclusive => ScreeningLookup::Inconclusive,
            Lookup::Complete { result } => ScreeningLookup::Complete(decode(result)?),
        })
    }
}

fn decode(reply: Evaluated<Response>) -> Result<Evaluated<Observation>, ProviderError> {
    let response = reply.result;
    match (response.screening_status, response.match_count) {
        (ScreeningStatus::Clear, 0) => Ok(Evaluated {
            evaluated_at: reply.evaluated_at,
            result: Observation::Sanctions { hit: false },
        }),
        (ScreeningStatus::Hit, 1..) => Ok(Evaluated {
            evaluated_at: reply.evaluated_at,
            result: Observation::Sanctions { hit: true },
        }),
        _ => Err(ProviderError {
            kind: ProviderErrorKind::Permanent,
            retry_after_ms: None,
        }),
    }
}

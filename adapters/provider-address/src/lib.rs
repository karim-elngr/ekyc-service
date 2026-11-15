use async_trait::async_trait;
use chrono::NaiveDate;
use ekyc_domain::{CheckKind, Observation};
use ekyc_http_transport::{decode_json, Transport};
use ekyc_ports::{Evaluated, Provider, ProviderError, ScreeningLookup};
use serde::{Deserialize, Serialize};

pub struct AddressProvider {
    transport: Transport,
    endpoint: String,
    lookup_endpoint: String,
}

impl AddressProvider {
    pub fn new(transport: Transport, base_url: String) -> Self {
        let base = base_url.trim_end_matches('/');
        Self {
            transport,
            endpoint: format!("{base}/addresses/verify"),
            lookup_endpoint: format!("{base}/addresses/result"),
        }
    }
}

#[derive(Serialize)]
struct Request<'a> {
    proof_ref: &'a str,
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
    proof_date: NaiveDate,
}

#[async_trait]
impl Provider for AddressProvider {
    fn kind(&self) -> CheckKind {
        CheckKind::Address
    }

    async fn check(
        &self,
        evidence_ref: &str,
        operation_id: &str,
    ) -> Result<Evaluated<Observation>, ProviderError> {
        let input = Request {
            proof_ref: evidence_ref,
        };
        let body = self
            .transport
            .post_bytes(&self.endpoint, operation_id, &input)
            .await?;
        self.decode(&body)
    }

    fn decode(&self, body: &[u8]) -> Result<Evaluated<Observation>, ProviderError> {
        Ok(address_finding(decode_json(body)?))
    }

    async fn screening_result(&self, operation_id: &str) -> Result<ScreeningLookup, ProviderError> {
        #[derive(Serialize)]
        struct Request<'a> {
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
                &self.lookup_endpoint,
                operation_id,
                &Request { operation_id },
            )
            .await?;
        Ok(match decode_json(&body)? {
            Lookup::Pending => ScreeningLookup::Pending,
            Lookup::NotAccepted => ScreeningLookup::NotAccepted,
            Lookup::Inconclusive => ScreeningLookup::Inconclusive,
            Lookup::Complete { result } => ScreeningLookup::Complete(address_finding(result)),
        })
    }
}

fn address_finding(reply: Evaluated<Response>) -> Evaluated<Observation> {
    reply.map(|response| Observation::Address {
        passed: matches!(response.status, Status::Pass),
        confidence: response.confidence,
        proof_date: response.proof_date,
    })
}

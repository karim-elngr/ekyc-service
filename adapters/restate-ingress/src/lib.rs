use reqwest::{Client, StatusCode, Url};
use std::time::Duration;

#[derive(Clone)]
pub struct CaseCommands {
    client: Client,
    ingress: Url,
}

#[derive(Debug)]
pub enum CommandError {
    Conflict(&'static str),
    Invalid,
    Unavailable,
    NotFound,
    Forbidden,
}

impl CaseCommands {
    pub fn new(ingress: Url, timeout: Duration) -> Result<Self, reqwest::Error> {
        let client = Client::builder()
            .timeout(timeout)
            .connect_timeout(Duration::from_secs(2))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self { client, ingress })
    }

    /// A shared, read-only invocation exercises the same routing as a new command.
    pub async fn ready(&self) -> bool {
        let mut url = self.ingress.clone();
        let Ok(mut path) = url.path_segments_mut() else {
            return false;
        };
        path.pop_if_empty().extend([
            "restate",
            "call",
            "ProductCase",
            "system-readiness",
            "readiness_v2",
        ]);
        drop(path);
        let Ok(response) = self.client.post(url).send().await else {
            return false;
        };
        response.status().is_success()
            && matches!(
                response.json::<String>().await.as_deref(),
                Ok("ekyc.commands.v2")
            )
    }

    pub async fn product<T: serde::Serialize + Sync>(
        &self,
        key: &str,
        body: &T,
    ) -> Result<(), CommandError> {
        let mut url = self.ingress.clone();
        url.path_segments_mut()
            .map_err(|_| CommandError::Unavailable)?
            .pop_if_empty()
            .extend(["restate", "call", "ProductCase", key, "command"]);
        // The virtual object compares the actual request for every retry. An ingress
        // response cache keyed only by Idempotency-Key would hide conflicting reuse.
        let res = self
            .client
            .post(url)
            .json(body)
            .send()
            .await
            .map_err(|_| CommandError::Unavailable)?;
        match res.status() {
            status if status.is_success() => Ok(()),
            StatusCode::NOT_FOUND => Err(CommandError::NotFound),
            StatusCode::FORBIDDEN => Err(CommandError::Forbidden),
            StatusCode::CONFLICT => {
                // Only worker-defined public codes cross the API boundary.
                #[derive(serde::Deserialize)]
                struct Failure {
                    message: String,
                }
                let code = match res
                    .json::<Failure>()
                    .await
                    .ok()
                    .map(|f| f.message)
                    .as_deref()
                {
                    Some("idempotency_key_reused" | "case_already_open") => {
                        "idempotency_key_reused"
                    }
                    Some("stale_operation") => "stale_operation",
                    Some("case_already_decided") => "case_already_decided",
                    Some("invalid_case_state") => "invalid_case_state",
                    _ => "command_conflict",
                };
                Err(CommandError::Conflict(code))
            }
            StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => {
                Err(CommandError::Invalid)
            }
            _ => Err(CommandError::Unavailable),
        }
    }
}

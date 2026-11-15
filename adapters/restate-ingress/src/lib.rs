use ekyc_domain::OpenCase;
use reqwest::{Client, StatusCode, Url};
use std::time::Duration;

#[derive(Clone)]
pub struct CaseCommands {
    client: Client,
    ingress: Url,
}

#[derive(Debug)]
pub enum CommandError {
    Conflict,
    Invalid,
    Unavailable,
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

    pub async fn open(&self, key: &str, case: &OpenCase) -> Result<(), CommandError> {
        let mut url = self.ingress.clone();
        url.path_segments_mut()
            .map_err(|_| CommandError::Unavailable)?
            .pop_if_empty()
            .extend(["restate", "call", "VerificationCase", key, "open"]);
        // The virtual object compares the actual request for every retry. An ingress
        // response cache keyed only by Idempotency-Key would hide conflicting reuse.
        let res = self
            .client
            .post(url)
            .json(case)
            .send()
            .await
            .map_err(|_| CommandError::Unavailable)?;
        match res.status() {
            status if status.is_success() => Ok(()),
            StatusCode::CONFLICT => Err(CommandError::Conflict),
            StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => {
                Err(CommandError::Invalid)
            }
            _ => Err(CommandError::Unavailable),
        }
    }
}

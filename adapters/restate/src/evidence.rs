//! Resolve opaque evidence references against the institution's authoritative issuer.
use ekyc_domain::{product::BoundEvidence, CheckKind};
use reqwest::{Client, StatusCode, Url};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, env, time::Duration};

#[derive(Clone)]
pub(crate) struct EvidenceResolver {
    client: Client,
    endpoint: (Url, String),
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct EvidenceSnapshot {
    pub evidence: BTreeMap<CheckKind, BoundEvidence>,
    pub current_document_revision: Option<String>,
    pub current_portrait_revision: Option<String>,
    pub screening: Option<BoundEvidence>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub(crate) enum EvidenceError {
    Invalid,
    Unavailable,
}

impl EvidenceResolver {
    pub fn from_env() -> Result<Self, String> {
        let endpoint = match (
            env::var("EKYC_EVIDENCE_URL").ok(),
            env::var("EKYC_EVIDENCE_TOKEN").ok(),
        ) {
            (Some(base), Some(token)) if !token.is_empty() => {
                let url = Url::parse(&format!("{}/v2/resolve", base.trim_end_matches('/')))
                    .map_err(|_| "invalid evidence issuer URL")?;
                if !matches!(url.scheme(), "http" | "https")
                    || url.host().is_none()
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || url.fragment().is_some()
                    || url.query().is_some()
                {
                    return Err("invalid evidence issuer URL".into());
                }
                (url, token)
            }
            _ => return Err("configure both EKYC_EVIDENCE_URL and EKYC_EVIDENCE_TOKEN".into()),
        };
        let client = Client::builder()
            .timeout(Duration::from_secs(4))
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .map_err(|_| "unable to configure evidence client")?;
        Ok(Self { client, endpoint })
    }

    pub async fn ready(&self) -> bool {
        self.resolve("system-health", "readiness", &BTreeMap::new())
            .await
            .is_ok()
    }

    pub async fn resolve(
        &self,
        institution: &str,
        subject: &str,
        requested: &BTreeMap<CheckKind, String>,
    ) -> Result<EvidenceSnapshot, EvidenceError> {
        use EvidenceError::{Invalid, Unavailable};
        let (url, token) = &self.endpoint;
        let mut response = self.client.post(url.clone()).bearer_auth(token)
            .json(&serde_json::json!({"institution":institution,"subject_ref":subject,"evidence":requested}))
            .send().await.map_err(|_|Unavailable)?;
        match response.status() {
            StatusCode::OK => (),
            StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND | StatusCode::UNPROCESSABLE_ENTITY => {
                return Err(Invalid)
            }
            _ => return Err(Unavailable),
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Unavailable)? {
            if body.len() + chunk.len() > 65536 {
                return Err(Unavailable);
            }
            body.extend_from_slice(&chunk);
        }
        let snapshot: EvidenceSnapshot = serde_json::from_slice(&body).map_err(|_| Unavailable)?;
        let bound = |asset: &BoundEvidence, kind: CheckKind| {
            asset.tenant_id == institution
                && asset.subject_ref == subject
                && asset.kind == kind
                && [
                    &asset.evidence_ref,
                    &asset.provider_ref,
                    &asset.method,
                    &asset.provider,
                    &asset.revision,
                ]
                .into_iter()
                .all(|s| !s.trim().is_empty() && s.len() <= 512 && !s.chars().any(char::is_control))
        };
        if requested.len() != snapshot.evidence.len()
            || !snapshot.evidence.iter().all(|(kind, asset)| {
                bound(asset, *kind) && requested.get(kind) == Some(&asset.evidence_ref)
            })
        {
            return Err(Invalid);
        }
        if let Some(screen) = &snapshot.screening {
            if !bound(screen, CheckKind::Sanctions)
                || screen.document_revision.is_none()
                || screen.document_revision != snapshot.current_document_revision
            {
                return Err(Invalid);
            }
        }
        Ok(snapshot)
    }
}

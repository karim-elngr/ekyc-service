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
    #[serde(default)]
    pub version: Option<u8>,
    #[serde(default)]
    pub provider_candidates: BTreeMap<CheckKind, Vec<BoundEvidence>>,
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
            .header("Accept", "application/vnd.ekyc.issuer.v3+json")
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
        validate_issuer(snapshot, institution, subject, requested)
    }
}

fn validate_issuer(
    snapshot: EvidenceSnapshot,
    institution: &str,
    subject: &str,
    requested: &BTreeMap<CheckKind, String>,
) -> Result<EvidenceSnapshot, EvidenceError> {
    use EvidenceError::Invalid;
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
    let mut snapshot = snapshot;
    match snapshot.version {
        None | Some(2) if snapshot.provider_candidates.is_empty() => {
            for (kind, asset) in snapshot
                .evidence
                .iter()
                .chain(snapshot.screening.iter().map(|s| (&s.kind, s)))
            {
                snapshot
                    .provider_candidates
                    .insert(*kind, vec![asset.clone()]);
            }
        }
        Some(3) => {
            let anchors = snapshot
                .evidence
                .iter()
                .chain(snapshot.screening.iter().map(|s| (&s.kind, s)));
            if snapshot.provider_candidates.len() != anchors.clone().count() {
                return Err(Invalid);
            }
            for (kind, anchor) in anchors {
                let candidates = snapshot.provider_candidates.get(kind).ok_or(Invalid)?;
                let mut seen = std::collections::BTreeSet::new();
                if candidates.is_empty()
                    || candidates.len() > 16
                    || !candidates.iter().all(|b| {
                        bound(b, *kind)
                            && b.evidence_ref == anchor.evidence_ref
                            && b.revision == anchor.revision
                            && b.document_revision == anchor.document_revision
                            && b.portrait_revision == anchor.portrait_revision
                            && seen.insert(&b.provider)
                    })
                {
                    return Err(Invalid);
                }
            }
        }
        _ => return Err(Invalid),
    }
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn address(provider: &str) -> BoundEvidence {
        BoundEvidence {
            kind: CheckKind::Address,
            tenant_id: "institution".into(),
            subject_ref: "subject".into(),
            evidence_ref: "asset".into(),
            revision: "v1".into(),
            provider: provider.into(),
            provider_ref: format!("{provider}-locator"),
            method: "address_verification".into(),
            callback: false,
            document_revision: None,
            portrait_revision: None,
        }
    }
    #[test]
    fn candidates_cannot_cross_the_asset_revision_subject_or_tenant() {
        let (a, b) = (address("address-a"), address("address-b"));
        let request = BTreeMap::from([(CheckKind::Address, "asset".to_owned())]);
        let snapshot = EvidenceSnapshot {
            version: Some(3),
            evidence: BTreeMap::from([(CheckKind::Address, a.clone())]),
            provider_candidates: BTreeMap::from([(CheckKind::Address, vec![a.clone(), b])]),
            screening: None,
            current_document_revision: None,
            current_portrait_revision: None,
        };
        assert!(validate_issuer(snapshot.clone(), "institution", "subject", &request).is_ok());
        for field in [
            "tenant_id",
            "subject_ref",
            "evidence_ref",
            "revision",
            "provider",
        ] {
            let mut altered = serde_json::to_value(&snapshot).unwrap();
            altered["provider_candidates"]["address"][1][field] =
                serde_json::json!(if field == "provider" {
                    "address-a"
                } else {
                    "unrelated"
                });
            assert!(
                matches!(
                    validate_issuer(
                        serde_json::from_value(altered).unwrap(),
                        "institution",
                        "subject",
                        &request
                    ),
                    Err(EvidenceError::Invalid)
                ),
                "{field}"
            );
        }
        let old = EvidenceSnapshot {
            version: None,
            provider_candidates: Default::default(),
            ..snapshot
        };
        assert_eq!(
            validate_issuer(old, "institution", "subject", &request)
                .unwrap()
                .provider_candidates[&CheckKind::Address],
            vec![a]
        );
    }
}

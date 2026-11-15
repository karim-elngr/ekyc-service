//! Resolve issuer-provided vendor identities against operator-controlled endpoints.
use crate::evidence::EvidenceError;
use ekyc_domain::{product::BoundEvidence, valid_reference, CheckKind};
use reqwest::Url;
use std::{collections::BTreeMap, env};

#[derive(Clone)]
pub(crate) struct Catalog {
    pub endpoints: BTreeMap<CheckKind, BTreeMap<String, String>>,
    pub demo_callbacks: bool,
}

impl Catalog {
    pub fn from_env() -> Result<Self, String> {
        let endpoints = match env::var("EKYC_PROVIDER_CATALOG") {
            Ok(raw) => {
                serde_json::from_str(&raw).map_err(|_| "invalid EKYC_PROVIDER_CATALOG JSON")?
            }
            Err(env::VarError::NotPresent) => {
                let mut endpoints = BTreeMap::new();
                for kind in CheckKind::ALL {
                    let key = format!("{}_URL", kind.as_str().to_ascii_uppercase());
                    let base = env::var(&key).map_err(|_| format!("missing {key}"))?;
                    endpoints.insert(kind, BTreeMap::from([(kind.as_str().to_owned(), base)]));
                }
                endpoints
            }
            Err(_) => return Err("invalid EKYC_PROVIDER_CATALOG encoding".into()),
        };
        let demo_callbacks = match env::var("EKYC_DEMO_CALLBACKS").as_deref() {
            Ok("1") => true,
            Ok("0") | Err(env::VarError::NotPresent) => false,
            _ => return Err("EKYC_DEMO_CALLBACKS must be 0 or 1".into()),
        };
        let catalog = Self {
            endpoints,
            demo_callbacks,
        };
        for kind in CheckKind::ALL {
            let routes = catalog
                .endpoints
                .get(&kind)
                .filter(|r| !r.is_empty())
                .ok_or_else(|| format!("no providers configured for {}", kind.as_str()))?;
            for (id, base) in routes {
                let url = Url::parse(base).map_err(|_| format!("invalid provider URL for {id}"))?;
                if !valid_reference(id)
                    || !matches!(url.scheme(), "http" | "https")
                    || url.host().is_none()
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || url.query().is_some()
                    || url.fragment().is_some()
                {
                    return Err(format!(
                        "invalid provider identity or URL for {}",
                        kind.as_str()
                    ));
                }
            }
        }
        Ok(catalog)
    }

    pub fn bind(&self, asset: &mut BoundEvidence) -> Result<(), EvidenceError> {
        if !self
            .endpoints
            .get(&asset.kind)
            .is_some_and(|routes| routes.contains_key(&asset.provider))
        {
            return Err(EvidenceError::Invalid);
        }
        // Only configured demo adapters interpret this synthetic locator. Production locators are opaque.
        asset.callback = self.demo_callbacks
            && asset.provider == asset.kind.as_str()
            && asset.provider_ref == "demo:interactive";
        Ok(())
    }
}

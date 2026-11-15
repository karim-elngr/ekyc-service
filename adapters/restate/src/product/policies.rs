use ekyc_domain::product::Policy;
use std::{collections::BTreeMap, env, fs, path::Path};

// When supplied, the directory is the complete set of active product policies.
pub(crate) struct Policies(Option<BTreeMap<(String, String), Policy>>);

impl Policies {
    pub fn from_env() -> Result<Self, String> {
        let Some(path) = env::var_os("EKYC_POLICY_DIR") else {
            return Ok(Self(None));
        };
        Self::from_directory(Path::new(&path))
    }

    pub(super) fn from_directory(path: &Path) -> Result<Self, String> {
        let mut policies = BTreeMap::new();
        for entry in fs::read_dir(path).map_err(|e| e.to_string())? {
            let path = entry.map_err(|e| e.to_string())?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let policy = Policy::from_json(&fs::read_to_string(path).map_err(|e| e.to_string())?)?;
            if policies
                .insert((policy.product.clone(), policy.use_case.clone()), policy)
                .is_some()
            {
                return Err("multiple active policies for the same product and use case".into());
            }
        }
        if policies.is_empty() {
            return Err("policy directory contains no policies".into());
        }
        Ok(Self(Some(policies)))
    }

    pub(super) fn select(&self, product: &str, use_case: &str) -> Result<Policy, String> {
        match &self.0 {
            Some(policies) => policies
                .get(&(product.to_owned(), use_case.to_owned()))
                .cloned()
                .ok_or_else(|| "unknown product/use case".into()),
            None => Policy::load(product, use_case),
        }
    }
}

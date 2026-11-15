//! The Restate admin plane is read-only from the worker. Registration owns rule creation.
use ekyc_domain::CheckKind;
use ekyc_postgres::ContractBook;
use serde::Deserialize;
use std::{collections::BTreeMap, env, path::Path, sync::Arc, time::Duration};

#[derive(Clone)]
pub(crate) struct Rules {
    pub book: Arc<ContractBook>,
    contracts: BTreeMap<(CheckKind, String), String>,
    endpoint: String,
    client: reqwest::Client,
}

impl Rules {
    pub fn from_env() -> Result<Self, Box<dyn std::error::Error>> {
        let path = env::var_os("EKYC_PROVIDER_CONTRACTS_FILE").unwrap_or_else(|| {
            if Path::new("/config/provider-contracts.json").exists() {
                "/config/provider-contracts.json".into()
            } else {
                "config/provider-contracts.json".into()
            }
        });
        let book: ContractBook = serde_json::from_slice(&std::fs::read(path)?)?;
        let mut contracts = BTreeMap::new();
        for (id, limits) in &book.pools {
            if id.is_empty()
                || id.len() > 36
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
                || limits.lookup < 1
                || limits.lookup > limits.total
                || limits.submit < 1
                || limits.submit >= limits.total
            {
                return Err("invalid concurrency pool or lookup headroom".into());
            }
        }
        for route in &book.routes {
            if !book.pools.contains_key(&route.pool)
                || !book.rates.contains_key(&route.rate_bucket)
                || !book.billing.contains_key(&route.billing_account)
                || contracts
                    .insert((route.kind, route.provider.clone()), route.pool.clone())
                    .is_some()
            {
                return Err("invalid or duplicate provider contract".into());
            }
        }
        let admin =
            env::var("RESTATE_ADMIN_URL").unwrap_or_else(|_| "http://127.0.0.1:9070".into());
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()?;
        Ok(Self {
            book: Arc::new(book),
            contracts,
            endpoint: format!("{}/query", admin.trim_end_matches('/')),
            client,
        })
    }

    pub fn pool(&self, kind: CheckKind, provider: &str) -> Option<&str> {
        self.contracts
            .get(&(kind, provider.to_owned()))
            .map(String::as_str)
    }

    pub async fn effective(&self, pool: Option<&str>) -> bool {
        #[derive(Deserialize)]
        struct Row {
            pattern: String,
            concurrency: Option<i64>,
        }
        #[derive(Deserialize)]
        struct Query {
            rows: Vec<Row>,
        }
        let Ok(response)=self.client.post(&self.endpoint).header("Accept","application/json")
            .json(&serde_json::json!({"query":"SELECT pattern,concurrency FROM sys_rules WHERE NOT disabled"}))
            .send().await else { return false; };
        if !response.status().is_success() {
            return false;
        }
        let Ok(rows) = response.json::<Query>().await else {
            return false;
        };
        let active: BTreeMap<_, _> = rows
            .rows
            .into_iter()
            .filter_map(|r| r.concurrency.map(|v| (r.pattern, v)))
            .collect();
        self.book
            .pools
            .iter()
            .filter(|(id, _)| pool.is_none_or(|p| p == id.as_str()))
            .all(|(id, limits)| {
                let submit_rule = format!("{id}/submit");
                let headroom = active
                    .get(id)
                    .zip(active.get(&submit_rule))
                    .is_some_and(|(total, submit)| submit < total);
                headroom
                    && [
                        (id.clone(), limits.total),
                        (submit_rule, limits.submit),
                        (format!("{id}/lookup"), limits.lookup),
                    ]
                    .into_iter()
                    .all(|(pattern, cap)| {
                        active
                            .get(&pattern)
                            .is_some_and(|v| (1..=i64::from(cap)).contains(v))
                    })
            })
    }
}
